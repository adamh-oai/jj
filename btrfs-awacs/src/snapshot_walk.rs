//! Complete immutable snapshot indexing through ordinary userspace VFS calls.
//!
//! The walker intentionally consumes an already-open read-only snapshot root.
//! It never asks the privileged broker to reveal names or inode references.

use crate::btrfs::{inode_generation, subvolume_info};
use crate::index::{Index, MODE_TYPE_MASK, Object, ROOT_INO};
use crate::manifest::Reference;
use crate::tree_index::{
    PRIVILEGE_CAPABILITY, PRIVILEGE_DEVICE, PRIVILEGE_FSCRYPT, PRIVILEGE_SECURITY_XATTR,
    PRIVILEGE_SETGID, PRIVILEGE_SETUID, PRIVILEGE_TRUSTED_XATTR,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fmt;
use std::mem::zeroed;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

const FILEID_BTRFS_WITHOUT_PARENT: i32 = 0x4d;
const FILEID_BTRFS_WITH_PARENT: i32 = 0x4e;
const FILEID_BTRFS_WITH_PARENT_ROOT: i32 = 0x4f;
const BTRFS_HANDLE_BYTES: usize = 128;

#[repr(C)]
struct HandleBuffer {
    handle_bytes: u32,
    handle_type: i32,
    handle: [u8; BTRFS_HANDLE_BYTES],
}

#[derive(Debug)]
pub enum SnapshotIndexError {
    FscryptDirectory(Vec<u8>),
    NestedSubvolume(Vec<u8>),
    Other(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnapshotWalkProgress {
    pub directories: usize,
    pub objects: usize,
    pub references: usize,
}

/// One active depth-first traversal frame.
///
/// Holding frames only for the current ancestor chain keeps descriptor usage
/// proportional to tree depth. A breadth-first queue of open directory fds can
/// exhaust RLIMIT_NOFILE on a shallow repository with many sibling
/// directories.
struct DirectoryFrame {
    directory: OwnedFd,
    parent_ino: u64,
    parent_path: Vec<u8>,
    names: Vec<Vec<u8>>,
    next_name: usize,
}

impl SnapshotIndexError {
    fn new(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    fn io(context: &str) -> Self {
        Self::new(format!("{context}: {}", std::io::Error::last_os_error()))
    }
}

impl fmt::Display for SnapshotIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FscryptDirectory(path) => {
                write!(
                    formatter,
                    "immutable snapshot contains fscrypt directory {:?}",
                    String::from_utf8_lossy(path)
                )
            }
            Self::NestedSubvolume(path) => {
                write!(
                    formatter,
                    "immutable snapshot contains nested subvolume {:?}",
                    String::from_utf8_lossy(path)
                )
            }
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SnapshotIndexError {}

/// Builds the complete inode/reference graph for one immutable snapshot.
///
/// Names remain repository-relative raw bytes. The caller is responsible for
/// verifying the root fd's expected snapshot identity before and after this
/// walk; this function verifies that the traversal never escapes that root.
pub fn read_snapshot_index(root: BorrowedFd<'_>) -> Result<Index, SnapshotIndexError> {
    read_snapshot_index_with_progress(root, |_| {})
}

/// Builds the complete inode/reference graph and reports monotonic traversal
/// counts after every 100 visited entries and at completion. Callers can use
/// this for interactive progress without coupling the library walker to a
/// terminal.
pub fn read_snapshot_index_with_progress(
    root: BorrowedFd<'_>,
    mut progress: impl FnMut(SnapshotWalkProgress),
) -> Result<Index, SnapshotIndexError> {
    let root_stat = fd_stat(root)?;
    if root_stat.st_ino != ROOT_INO || root_stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(SnapshotIndexError::new(
            "snapshot index root is not Btrfs directory inode 256",
        ));
    }
    let root_info = subvolume_info(root)
        .map_err(|error| SnapshotIndexError::new(format!("inspect snapshot subvolume: {error}")))?;
    if !root_info.readonly() {
        return Err(SnapshotIndexError::new(
            "snapshot index root is not read-only",
        ));
    }

    let root_xattrs = read_xattrs(proc_fd_path(root.as_raw_fd(), None), true)?;
    let root_object = object_from_stat(
        ROOT_INO,
        inode_generation(root).map_err(|error| {
            SnapshotIndexError::new(format!("read root inode generation: {error}"))
        })?,
        &root_stat,
        &root_xattrs,
    )?;
    if root_object.privilege_flags & PRIVILEGE_FSCRYPT != 0 {
        return Err(SnapshotIndexError::FscryptDirectory(Vec::new()));
    }

    let mut index = Index {
        objects: BTreeMap::from([(ROOT_INO, root_object)]),
        references: BTreeSet::new(),
    };
    let mut directories_seen = 0;
    let mut entries_since_progress = 0;
    progress(SnapshotWalkProgress {
        directories: directories_seen,
        objects: index.objects.len(),
        references: index.references.len(),
    });
    let root_directory = dup_fd(root)?;
    let root_names = directory_names(root_directory.as_fd())?;
    let mut directories = vec![DirectoryFrame {
        directory: root_directory,
        parent_ino: ROOT_INO,
        parent_path: Vec::new(),
        names: root_names,
        next_name: 0,
    }];
    directories_seen += 1;
    while !directories.is_empty() {
        let frame = directories.last_mut().unwrap();
        if frame.next_name == frame.names.len() {
            directories.pop();
            continue;
        }
        let name = frame.names[frame.next_name].clone();
        frame.next_name += 1;
        // A colocated JJ checkout may keep `.git` as its own Btrfs
        // subvolume. It is repository control state, not working-copy
        // namespace: neither JJ nor Git fsmonitor consumes paths below
        // it, and parent-subvolume changed-object comparisons cannot
        // describe its descendants anyway. Skip only the root control
        // directory; other nested subvolumes remain a hard error.
        if frame.parent_path.is_empty() && name == b".git" {
            entries_since_progress += 1;
            if entries_since_progress == 100 {
                progress(SnapshotWalkProgress {
                    directories: directories_seen,
                    objects: index.objects.len(),
                    references: index.references.len(),
                });
                entries_since_progress = 0;
            }
            continue;
        }
        let relative_path = join_path(&frame.parent_path, &name);
        let child = open_path(frame.directory.as_fd(), &name)?;
        let stat = fd_stat(child.as_fd())?;
        if stat.st_dev != root_stat.st_dev {
            return Err(SnapshotIndexError::new(format!(
                "snapshot index crosses a mount at {:?}",
                String::from_utf8_lossy(&relative_path)
            )));
        }
        let ino = stat.st_ino;
        let is_directory = stat.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if is_directory && ino == ROOT_INO {
            return Err(SnapshotIndexError::NestedSubvolume(relative_path));
        }
        let generation =
            generation_from_handle(frame.directory.as_fd(), &name, ino, root_info.root_id)?;
        let xattrs = read_xattrs(
            proc_fd_path(frame.directory.as_raw_fd(), Some(&name)),
            false,
        )?;
        let object = object_from_stat(ino, generation, &stat, &xattrs)?;
        if object.privilege_flags & PRIVILEGE_FSCRYPT != 0 {
            return Err(SnapshotIndexError::FscryptDirectory(relative_path));
        }
        if let Some(existing) = index.objects.get(&ino) {
            if existing != &object {
                return Err(SnapshotIndexError::new(format!(
                    "snapshot index found inconsistent aliases for inode {ino}"
                )));
            }
        } else {
            index.objects.insert(ino, object);
        }
        if !index.references.insert(Reference {
            ino,
            parent_ino: frame.parent_ino,
            name: name.clone(),
        }) {
            return Err(SnapshotIndexError::new(
                "snapshot index contains duplicate reference",
            ));
        }
        if is_directory {
            let directory = open_directory(frame.directory.as_fd(), &name)?;
            let names = directory_names(directory.as_fd())?;
            directories_seen += 1;
            directories.push(DirectoryFrame {
                directory,
                parent_ino: ino,
                parent_path: relative_path,
                names,
                next_name: 0,
            });
        }
        entries_since_progress += 1;
        if entries_since_progress == 100 {
            progress(SnapshotWalkProgress {
                directories: directories_seen,
                objects: index.objects.len(),
                references: index.references.len(),
            });
            entries_since_progress = 0;
        }
    }
    progress(SnapshotWalkProgress {
        directories: directories_seen,
        objects: index.objects.len(),
        references: index.references.len(),
    });
    index
        .validate()
        .map_err(|error| SnapshotIndexError::new(format!("snapshot index is invalid: {error}")))?;
    Ok(index)
}

fn object_from_stat(
    ino: u64,
    generation: u64,
    stat: &libc::stat,
    xattrs: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<Object, SnapshotIndexError> {
    let mode = stat.st_mode;
    let nlink = u32::try_from(stat.st_nlink)
        .map_err(|_| SnapshotIndexError::new(format!("inode {ino} link count exceeds u32")))?;
    let (xattr_flags, security_xattr_hash) = classify_xattrs(xattrs);
    let mut privilege_flags = xattr_flags;
    if mode & 0o4000 != 0 {
        privilege_flags |= PRIVILEGE_SETUID;
    }
    if mode & 0o2000 != 0 {
        privilege_flags |= PRIVILEGE_SETGID;
    }
    if matches!(mode & MODE_TYPE_MASK, 0o020000 | 0o060000) {
        privilege_flags |= PRIVILEGE_DEVICE;
    }
    Ok(Object {
        ino,
        generation,
        mode,
        nlink,
        uid: stat.st_uid.into(),
        gid: stat.st_gid.into(),
        rdev: stat.st_rdev,
        privilege_flags,
        security_xattr_hash,
    })
}

fn classify_xattrs(xattrs: &BTreeMap<Vec<u8>, Vec<u8>>) -> (u64, [u8; 32]) {
    let mut flags = 0_u64;
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-security-xattrs-v1\0");
    for (name, value) in xattrs {
        let relevant =
            if name.as_slice() == b"fscrypt.context" || name.as_slice() == b"security.fscrypt" {
                flags |= PRIVILEGE_FSCRYPT;
                true
            } else if name.as_slice() == b"security.capability" {
                flags |= PRIVILEGE_CAPABILITY | PRIVILEGE_SECURITY_XATTR;
                true
            } else if name.starts_with(b"security.") {
                flags |= PRIVILEGE_SECURITY_XATTR;
                true
            } else if name.starts_with(b"trusted.") {
                flags |= PRIVILEGE_TRUSTED_XATTR;
                true
            } else {
                false
            };
        if relevant {
            hash.update((name.len() as u64).to_be_bytes());
            hash.update(name);
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value);
        }
    }
    (flags, hash.finalize().into())
}

fn directory_names(directory: BorrowedFd<'_>) -> Result<Vec<Vec<u8>>, SnapshotIndexError> {
    let duplicate = dup_fd(directory)?;
    // SAFETY: fdopendir takes ownership of the duplicated descriptor on success.
    let stream = unsafe { libc::fdopendir(duplicate.as_raw_fd()) };
    if stream.is_null() {
        return Err(SnapshotIndexError::io("open snapshot directory stream"));
    }
    std::mem::forget(duplicate);
    let mut names = Vec::new();
    loop {
        // SAFETY: readdir reports end-of-directory and errors with the same
        // null return, so clear thread-local errno before each call.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: stream remains valid until closed below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            if std::io::Error::last_os_error().raw_os_error() != Some(0) {
                // SAFETY: stream was returned by fdopendir and is closed
                // exactly once on this error path.
                let _ = unsafe { libc::closedir(stream) };
                return Err(SnapshotIndexError::io("read snapshot directory stream"));
            }
            break;
        }
        // SAFETY: d_name is NUL-terminated by readdir for a live entry.
        let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_bytes()
            .to_vec();
        if bytes != b"." && bytes != b".." {
            names.push(bytes);
        }
    }
    // SAFETY: stream was returned by fdopendir and is closed exactly once.
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(SnapshotIndexError::io("close snapshot directory stream"));
    }
    names.sort();
    Ok(names)
}

fn open_path(directory: BorrowedFd<'_>, name: &[u8]) -> Result<OwnedFd, SnapshotIndexError> {
    openat(
        directory,
        name,
        libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
}

fn open_directory(directory: BorrowedFd<'_>, name: &[u8]) -> Result<OwnedFd, SnapshotIndexError> {
    openat(
        directory,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
}

fn openat(
    directory: BorrowedFd<'_>,
    name: &[u8],
    flags: i32,
) -> Result<OwnedFd, SnapshotIndexError> {
    let name = CString::new(name)
        .map_err(|_| SnapshotIndexError::new("snapshot entry name contains NUL"))?;
    // SAFETY: directory stays live, name is NUL-terminated, and ownership of
    // a successful fd is transferred immediately to OwnedFd.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(SnapshotIndexError::io("open snapshot entry"));
    }
    // SAFETY: fd is newly returned and owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn dup_fd(fd: BorrowedFd<'_>) -> Result<OwnedFd, SnapshotIndexError> {
    // SAFETY: F_DUPFD_CLOEXEC duplicates a live descriptor.
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(SnapshotIndexError::io("duplicate snapshot directory fd"));
    }
    // SAFETY: duplicate is newly returned and owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

fn fd_stat(fd: BorrowedFd<'_>) -> Result<libc::stat, SnapshotIndexError> {
    // SAFETY: fstat initializes stat for a live descriptor.
    unsafe {
        let mut stat: libc::stat = zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut stat) != 0 {
            return Err(SnapshotIndexError::io("stat snapshot entry"));
        }
        Ok(stat)
    }
}

fn generation_from_handle(
    directory: BorrowedFd<'_>,
    name: &[u8],
    expected_ino: u64,
    expected_root_id: u64,
) -> Result<u64, SnapshotIndexError> {
    let name = CString::new(name)
        .map_err(|_| SnapshotIndexError::new("snapshot entry name contains NUL"))?;
    let mut handle = HandleBuffer {
        handle_bytes: BTRFS_HANDLE_BYTES as u32,
        handle_type: 0,
        handle: [0; BTRFS_HANDLE_BYTES],
    };
    let mut mount_id = 0_i32;
    // SAFETY: handle begins with the libc file_handle header followed by the
    // declared writable byte buffer; directory and name stay live.
    let result = unsafe {
        libc::name_to_handle_at(
            directory.as_raw_fd(),
            name.as_ptr(),
            (&mut handle as *mut HandleBuffer).cast::<libc::file_handle>(),
            &mut mount_id,
            0,
        )
    };
    if result != 0 {
        return Err(SnapshotIndexError::io("read snapshot inode file handle"));
    }
    if !matches!(
        handle.handle_type,
        FILEID_BTRFS_WITHOUT_PARENT | FILEID_BTRFS_WITH_PARENT | FILEID_BTRFS_WITH_PARENT_ROOT
    ) || handle.handle_bytes < 20
    {
        return Err(SnapshotIndexError::new(
            "snapshot entry has unsupported Btrfs file handle",
        ));
    }
    let ino = u64::from_le_bytes(handle.handle[0..8].try_into().unwrap());
    let root_id = u64::from_le_bytes(handle.handle[8..16].try_into().unwrap());
    let generation = u32::from_le_bytes(handle.handle[16..20].try_into().unwrap()) as u64;
    if ino != expected_ino || root_id != expected_root_id || generation == 0 {
        return Err(SnapshotIndexError::new(
            "snapshot entry file handle identity mismatch",
        ));
    }
    Ok(generation)
}

fn read_xattrs(
    path: Vec<u8>,
    follow_final: bool,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, SnapshotIndexError> {
    let path = CString::new(path)
        .map_err(|_| SnapshotIndexError::new("snapshot xattr path contains NUL"))?;
    let list = list_xattr_names(&path, follow_final)?;
    let mut xattrs = BTreeMap::new();
    for name in list {
        if is_relevant_xattr(&name) {
            let value = read_xattr_value(&path, &name, follow_final)?;
            xattrs.insert(name, value);
        }
    }
    Ok(xattrs)
}

fn list_xattr_names(
    path: &CString,
    follow_final: bool,
) -> Result<Vec<Vec<u8>>, SnapshotIndexError> {
    let size = unsafe {
        if follow_final {
            libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0)
        } else {
            libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0)
        }
    };
    if size < 0 {
        return Err(SnapshotIndexError::io("list snapshot xattrs"));
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = vec![0_u8; size as usize];
    let read = unsafe {
        if follow_final {
            libc::listxattr(path.as_ptr(), bytes.as_mut_ptr().cast(), bytes.len())
        } else {
            libc::llistxattr(path.as_ptr(), bytes.as_mut_ptr().cast(), bytes.len())
        }
    };
    if read < 0 {
        return Err(SnapshotIndexError::io("read snapshot xattr names"));
    }
    bytes.truncate(read as usize);
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn read_xattr_value(
    path: &CString,
    name: &[u8],
    follow_final: bool,
) -> Result<Vec<u8>, SnapshotIndexError> {
    let name = CString::new(name)
        .map_err(|_| SnapshotIndexError::new("snapshot xattr name contains NUL"))?;
    let size = unsafe {
        if follow_final {
            libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0)
        } else {
            libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0)
        }
    };
    if size < 0 {
        return Err(SnapshotIndexError::io("read snapshot xattr length"));
    }
    let mut value = vec![0_u8; size as usize];
    let read = unsafe {
        if follow_final {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        } else {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        }
    };
    if read < 0 {
        return Err(SnapshotIndexError::io("read snapshot xattr value"));
    }
    value.truncate(read as usize);
    Ok(value)
}

fn is_relevant_xattr(name: &[u8]) -> bool {
    name == b"fscrypt.context"
        || name == b"security.fscrypt"
        || name.starts_with(b"security.")
        || name.starts_with(b"trusted.")
}

fn proc_fd_path(fd: i32, name: Option<&[u8]>) -> Vec<u8> {
    let mut path = format!("/proc/self/fd/{fd}").into_bytes();
    if let Some(name) = name {
        path.push(b'/');
        path.extend_from_slice(name);
    }
    path
}

fn join_path(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(parent.len() + usize::from(!parent.is_empty()) + name.len());
    path.extend_from_slice(parent);
    if !parent.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_raw_relative_paths() {
        assert_eq!(join_path(b"", b"a\xff"), b"a\xff");
        assert_eq!(join_path(b"parent", b"child"), b"parent/child");
    }

    #[test]
    fn classifies_fscrypt_context() {
        let xattrs = BTreeMap::from([(b"fscrypt.context".to_vec(), vec![1, 2, 3])]);
        let (flags, hash) = classify_xattrs(&xattrs);
        assert_ne!(flags & PRIVILEGE_FSCRYPT, 0);
        assert_ne!(hash, [0; 32]);
    }

    #[test]
    #[ignore = "requires JJ_TEST_BTRFS_ROOT, Btrfs subvolume permissions, and --test-threads=1"]
    fn wide_snapshot_walk_stays_below_low_fd_limit() {
        use std::fs::{self, File};
        use std::path::PathBuf;
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        struct RlimitGuard(libc::rlimit);

        impl Drop for RlimitGuard {
            fn drop(&mut self) {
                // SAFETY: restoring the process-wide limit captured before
                // this serial ignored test is the inverse of the successful
                // setrlimit call below.
                assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) }, 0);
            }
        }

        let btrfs_root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
            .map(PathBuf::from)
            .expect("JJ_TEST_BTRFS_ROOT must name a writable Btrfs directory");
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let source = btrfs_root.join(format!("awacs-wide-source-{suffix}"));
        let snapshot = btrfs_root.join(format!("awacs-wide-snapshot-{suffix}"));
        assert!(
            Command::new("btrfs")
                .args(["subvolume", "create"])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        for index in 0..256 {
            let directory = source.join(format!("wide-{index:03}"));
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("file"), format!("{index}\n")).unwrap();
        }
        assert!(
            Command::new("btrfs")
                .args(["subvolume", "snapshot", "-r"])
                .arg(&source)
                .arg(&snapshot)
                .status()
                .unwrap()
                .success()
        );

        // SAFETY: getrlimit writes one initialized value to this local.
        let mut original = unsafe { zeroed::<libc::rlimit>() };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) },
            0
        );
        let open_fds = fs::read_dir("/proc/self/fd").unwrap().count() as libc::rlim_t;
        let limited_soft = open_fds + 16;
        assert!(
            limited_soft < original.rlim_cur,
            "test needs room to lower RLIMIT_NOFILE below the 256-wide tree"
        );
        let limited = libc::rlimit {
            rlim_cur: limited_soft,
            rlim_max: original.rlim_max,
        };
        // SAFETY: limited keeps the original hard limit and raises no limit.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limited) }, 0);
        let guard = RlimitGuard(original);
        let snapshot_file = File::open(&snapshot).unwrap();
        let index = read_snapshot_index(snapshot_file.as_fd()).unwrap();
        drop(snapshot_file);
        drop(guard);
        assert_eq!(index.references.len(), 512);

        assert!(
            Command::new("btrfs")
                .args(["property", "set", "-ts"])
                .arg(&snapshot)
                .args(["ro", "false"])
                .status()
                .unwrap()
                .success()
        );
        for path in [&snapshot, &source] {
            assert!(
                Command::new("btrfs")
                    .args(["subvolume", "delete"])
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
}
