//! Recursive Btrfs snapshots, including subvolumes beneath ordinary
//! directories.

use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use crate::btrfs;

const ROOTREF_COUNT: usize = 255;
const LOOKUP_PATH_SIZE: usize = 3824;
// Linux UAPI: _IOWR(BTRFS_IOCTL_MAGIC, nr, 4096).
const GET_SUBVOL_ROOTREF: libc::c_ulong = 0xc000_0000 | (4096 << 16) | (0x94 << 8) | 61;
const INO_LOOKUP_USER: libc::c_ulong = 0xc000_0000 | (4096 << 16) | (0x94 << 8) | 62;

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct RootRef {
    treeid: u64,
    dirid: u64,
}

#[repr(C)]
struct RootRefs {
    min_treeid: u64,
    roots: [RootRef; ROOTREF_COUNT],
    num_items: u8,
    align: [u8; 7],
}

#[repr(C)]
struct Lookup {
    dirid: u64,
    treeid: u64,
    name: [u8; 256],
    path: [u8; LOOKUP_PATH_SIZE],
}

const _: [(); 4096] = [(); size_of::<RootRefs>()];
const _: [(); 4096] = [(); size_of::<Lookup>()];

#[derive(Debug, Eq, PartialEq)]
struct Child {
    root_id: u64,
    path: PathBuf,
}

struct Created {
    path: PathBuf,
    uuid: [u8; 16],
    readonly: bool,
}

/// Snapshots a Btrfs subvolume and every descendant subvolume at `destination`.
///
/// The destination must not exist, must be outside the source, and must be on
/// the same filesystem. Its parent must already exist. The new root is
/// writable; descendant subvolumes retain their source read-only properties.
/// Every new snapshot has its own UUID, as with an ordinary Btrfs snapshot.
///
/// Child discovery uses Btrfs root references rather than walking ordinary
/// files. Snapshots are taken individually, so callers that require a
/// consistent view across subvolumes must quiesce writers for the duration of
/// this operation. The complete tree is published with an atomic, non-replacing
/// rename. On failure its private staging directory is removed; a cleanup
/// failure is reported with the original error and the retained staging path.
pub fn recursive_snapshot(source: &Path, destination: &Path) -> io::Result<()> {
    let (source, destination) = normalized_paths(source, destination)?;
    let source = btrfs::OpenedSubvolume::open(&source).map_err(btrfs_error)?;
    let parent_path = destination
        .parent()
        .expect("normalized absolute destination");
    let name = destination
        .file_name()
        .expect("validated destination basename");
    let parent = File::open(parent_path)?;
    let filesystem = btrfs::filesystem_info(parent.as_fd()).map_err(btrfs_error)?;
    if filesystem.fs_uuid != source.filesystem.fs_uuid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recursive snapshot destination is on a different Btrfs filesystem",
        ));
    }
    let parent_root = btrfs::subvolume_info(parent.as_fd())
        .map_err(btrfs_error)?
        .root_id;
    if parent_root == source.subvolume.root_id {
        return Err(inside_source());
    }

    let (staging_name, staging) = create_staging(parent.as_fd())?;
    let staging_path = parent_path.join(&staging_name);
    let result = (|| {
        let children = child_subvolumes(source.as_fd())?;
        btrfs::create_snapshot(source.as_fd(), staging.as_fd(), b"snapshot", false)
            .map_err(btrfs_error)?;
        let output = open_at(staging.as_fd(), OsStr::new("snapshot"))?;
        let mut created = Vec::new();
        populate(
            source.as_fd(),
            output.as_fd(),
            children,
            Path::new(""),
            source.filesystem.fs_uuid,
            parent_root,
            &mut created,
        )?;
        // Keep parents writable until their children have been populated and
        // configured, including read-only subvolumes with their own children.
        for entry in created.iter().rev().filter(|entry| entry.readonly) {
            let child = open_relative(output.as_fd(), &entry.path)?;
            verify_uuid(child.as_fd(), entry.uuid)?;
            btrfs::set_subvolume_readonly(child.as_fd(), true).map_err(btrfs_error)?;
        }
        publish_snapshot(staging.as_fd(), parent.as_fd(), name)
    })();
    let cleanup = btrfs::remove_directory_at(parent.as_fd(), &staging_name, staging.as_fd());
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => Err(io::Error::other(format!(
            "{error}; recursive snapshot cleanup failed: {cleanup}; staging directory retained at \
             {}",
            staging_path.display()
        ))),
        (Ok(()), Err(error)) => {
            // Publication already succeeded. An empty staging directory does
            // not make the snapshot fail, and callers may rely on success to
            // track ownership of the newly published destination.
            tracing::warn!(
                path = %staging_path.display(),
                %error,
                "snapshot published but empty staging directory could not be removed"
            );
            Ok(())
        }
    }
}

fn create_staging(parent: BorrowedFd<'_>) -> io::Result<(OsString, File)> {
    let name = OsString::from(format!(".awacs-rsnapshot-{}", uuid::Uuid::new_v4()));
    let c_name = CString::new(name.as_bytes())?;
    // SAFETY: parent is a live directory and the name is one NUL-terminated
    // component. An existing name is never reused.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let directory = open_at(parent, &name).map_err(|error| {
        io::Error::other(format!(
            "cannot open snapshot staging directory {name:?}: {error}; directory retained"
        ))
    })?;
    let metadata = directory.metadata()?;
    // The private parent protects construction and rollback against other
    // users even when source permissions make the snapshot root writable.
    // Processes sharing our uid remain in the same trust boundary.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "snapshot staging directory {name:?} is not private to the current user"
        )));
    }
    Ok((name, directory))
}

fn publish_snapshot(
    staging: BorrowedFd<'_>,
    parent: BorrowedFd<'_>,
    name: &OsStr,
) -> io::Result<()> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: both parents are live directories and names are valid C strings.
    // Never replace a destination created while snapshots were being prepared.
    if unsafe {
        libc::renameat2(
            staging.as_raw_fd(),
            c"snapshot".as_ptr(),
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn normalized_paths(source: &Path, destination: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let source = fs::canonicalize(source)?;
    let name = destination.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "snapshot destination has no basename",
        )
    })?;
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty());
    let destination = fs::canonicalize(parent.unwrap_or_else(|| Path::new(".")))?.join(name);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "snapshot destination exists",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if destination.starts_with(&source) {
        return Err(inside_source());
    }
    Ok((source, destination))
}

fn inside_source() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "recursive snapshot destination is inside the source",
    )
}

fn populate(
    source: BorrowedFd<'_>,
    destination: BorrowedFd<'_>,
    children: Vec<Child>,
    relative: &Path,
    filesystem: [u8; 16],
    destination_parent_root: u64,
    created: &mut Vec<Created>,
) -> io::Result<()> {
    for child in &children {
        if child.root_id == destination_parent_root {
            // Also detects a destination reached through a bind-mount alias.
            return Err(inside_source());
        }
        let child_source = open_relative(source, &child.path)?;
        let source_info = btrfs::subvolume_info(child_source.as_fd()).map_err(btrfs_error)?;
        if source_info.root_id != child.root_id
            || btrfs::filesystem_info(child_source.as_fd())
                .map_err(btrfs_error)?
                .fs_uuid
                != filesystem
        {
            return Err(io::Error::other(
                "source child subvolume identity changed or is hidden by a mount",
            ));
        }
        // Enumerate before snapshotting, so the final comparison also catches
        // children removed or renamed between taking this snapshot and
        // starting to populate its descendants.
        let grandchildren = child_subvolumes(child_source.as_fd())?;
        let parent = open_relative(
            destination,
            child.path.parent().expect("child relative path"),
        )?;
        let name = child.path.file_name().expect("validated child basename");
        let placeholder = open_at(parent.as_fd(), name)?;
        if placeholder.metadata()?.ino() != 2 {
            return Err(io::Error::other(
                "nested snapshot destination is not a Btrfs placeholder",
            ));
        }
        let c_name = CString::new(name.as_bytes())?;
        // SAFETY: parent is an open directory and c_name is one component.
        // AT_REMOVEDIR also refuses to remove a nonempty or replaced file.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            return Err(io::Error::last_os_error());
        }
        btrfs::create_snapshot(child_source.as_fd(), parent.as_fd(), name.as_bytes(), false)
            .map_err(btrfs_error)?;
        let child_destination = open_at(parent.as_fd(), name)?;
        let path = relative.join(&child.path);
        created.push(Created {
            path: path.clone(),
            uuid: btrfs::subvolume_info(child_destination.as_fd())
                .map_err(btrfs_error)?
                .uuid,
            readonly: source_info.readonly(),
        });
        populate(
            child_source.as_fd(),
            child_destination.as_fd(),
            grandchildren,
            &path,
            filesystem,
            destination_parent_root,
            created,
        )?;
    }
    if child_subvolumes(source)? != children {
        return Err(io::Error::other(
            "source subvolume topology changed during recursive snapshot",
        ));
    }
    Ok(())
}

/// Immediate nested roots, relative to this root. Their contents are omitted
/// from a non-recursive AWACS cut, so consumers reusing filesystem caches must
/// invalidate these prefixes when handing a recursive snapshot to a new root.
pub fn child_subvolume_paths(source: BorrowedFd<'_>) -> io::Result<Vec<PathBuf>> {
    Ok(child_subvolumes(source)?
        .into_iter()
        .map(|child| child.path)
        .collect())
}

fn child_subvolumes(source: BorrowedFd<'_>) -> io::Result<Vec<Child>> {
    let mut children = Vec::new();
    let mut cursor = 0;
    loop {
        let mut refs = RootRefs {
            min_treeid: cursor,
            roots: [RootRef::default(); ROOTREF_COUNT],
            num_items: 0,
            align: [0; 7],
        };
        // SAFETY: source is live and refs matches the 4096-byte UAPI structure.
        let result = unsafe { libc::ioctl(source.as_raw_fd(), GET_SUBVOL_ROOTREF, &mut refs) };
        let more = if result < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EOVERFLOW) {
                return Err(error);
            }
            true
        } else {
            false
        };
        for reference in &refs.roots[..usize::from(refs.num_items)] {
            if reference.treeid < cursor {
                return Err(io::Error::other("Btrfs root references are not ordered"));
            }
            let mut lookup = Lookup {
                dirid: reference.dirid,
                treeid: reference.treeid,
                name: [0; 256],
                path: [0; LOOKUP_PATH_SIZE],
            };
            // SAFETY: lookup matches the 4096-byte BTRFS_IOC_INO_LOOKUP_USER UAPI.
            if unsafe { libc::ioctl(source.as_raw_fd(), INO_LOOKUP_USER, &mut lookup) } != 0 {
                return Err(io::Error::last_os_error());
            }
            children.push(Child {
                root_id: reference.treeid,
                path: lookup_path(&lookup.path, &lookup.name)?,
            });
            cursor = reference
                .treeid
                .checked_add(1)
                .ok_or_else(|| io::Error::other("Btrfs root reference cursor overflow"))?;
        }
        if !more {
            return Ok(children);
        }
        if refs.num_items == 0 || refs.min_treeid != cursor {
            return Err(io::Error::other(
                "Btrfs root reference cursor did not advance",
            ));
        }
    }
}

fn lookup_path(parent: &[u8], name: &[u8]) -> io::Result<PathBuf> {
    let parent = nul_terminated(parent)?;
    let name = nul_terminated(name)?;
    if name.is_empty() || name.contains(&b'/') || matches!(name, b"." | b"..") {
        return Err(io::Error::other(
            "Btrfs returned an invalid child subvolume name",
        ));
    }
    let path = Path::new(OsStr::from_bytes(parent));
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(io::Error::other(
            "Btrfs returned an invalid child subvolume path",
        ));
    }
    Ok(path.join(OsString::from_vec(name.to_vec())))
}

fn nul_terminated(bytes: &[u8]) -> io::Result<&[u8]> {
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| io::Error::other("Btrfs returned an unterminated subvolume path"))?;
    Ok(&bytes[..length])
}

fn open_relative(parent: BorrowedFd<'_>, relative: &Path) -> io::Result<File> {
    let mut directory = open_at(parent, OsStr::new("."))?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid relative snapshot path",
            ));
        };
        directory = open_at(directory.as_fd(), name)?;
    }
    Ok(directory)
}

fn open_at(parent: BorrowedFd<'_>, name: &OsStr) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: parent is live and name is NUL-terminated. O_NOFOLLOW rejects
    // symlinks in every component traversed by open_relative.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: openat transferred ownership of this new descriptor to us.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn verify_uuid(file: BorrowedFd<'_>, expected: [u8; 16]) -> io::Result<()> {
    if btrfs::subvolume_info(file).map_err(btrfs_error)?.uuid != expected {
        return Err(io::Error::other("new snapshot identity changed"));
    }
    Ok(())
}

fn btrfs_error(error: btrfs::BtrfsError) -> io::Error {
    io::Error::new(
        error
            .raw_os_error()
            .map(io::Error::from_raw_os_error)
            .map_or(io::ErrorKind::Other, |error| error.kind()),
        error,
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::*;

    #[test]
    fn lookup_paths_preserve_names_and_reject_escapes() {
        assert_eq!(
            lookup_path(b"ordinary/deep/\0", b"snapshot-\xff\0").unwrap(),
            PathBuf::from(OsString::from_vec(b"ordinary/deep/snapshot-\xff".to_vec()))
        );
        assert_eq!(lookup_path(b"\0", b"child\0").unwrap(), Path::new("child"));
        for parent in [
            b"../\0".as_slice(),
            b"/absolute/\0",
            b"ordinary/../\0",
            b"./\0",
        ] {
            assert!(lookup_path(parent, b"child\0").is_err());
        }
        for name in [b"\0".as_slice(), b"..\0", b".\0", b"a/b\0", b"unterminated"] {
            assert!(lookup_path(b"\0", name).is_err());
        }
    }

    #[test]
    fn rejects_existing_and_inside_source_destinations_without_changes() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("keep"), b"source contents").unwrap();
        let existing = fixture.path().join("existing");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("keep"), b"existing contents").unwrap();
        assert_eq!(
            recursive_snapshot(&source, &existing).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            recursive_snapshot(&source, &source.join("snapshot"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let alias = fixture.path().join("alias");
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        assert_eq!(
            recursive_snapshot(&source, &alias.join("snapshot"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let dangling = fixture.path().join("dangling");
        std::os::unix::fs::symlink("absent", &dangling).unwrap();
        assert_eq!(
            recursive_snapshot(&source, &dangling).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(source.join("keep")).unwrap(), b"source contents");
        assert_eq!(
            fs::read(existing.join("keep")).unwrap(),
            b"existing contents"
        );
        assert!(fs::symlink_metadata(&dangling).unwrap().is_symlink());
        assert!(!source.join("snapshot").exists());
    }

    #[test]
    fn descriptor_traversal_does_not_follow_symlinks() {
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir(fixture.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", fixture.path().join("alias")).unwrap();
        let root = File::open(fixture.path()).unwrap();
        assert!(open_relative(root.as_fd(), Path::new("alias")).is_err());
        assert!(open_relative(root.as_fd(), Path::new("../")).is_err());
        assert!(open_relative(root.as_fd(), Path::new("real")).is_ok());
    }

    #[test]
    fn publication_preserves_a_destination_created_during_preparation() {
        let fixture = tempfile::tempdir().unwrap();
        let parent = File::open(fixture.path()).unwrap();
        let (staging_name, staging) = create_staging(parent.as_fd()).unwrap();
        let staged_snapshot = fixture.path().join(&staging_name).join("snapshot");
        fs::create_dir(&staged_snapshot).unwrap();
        fs::write(staged_snapshot.join("contents"), b"new snapshot").unwrap();
        assert_eq!(staging.metadata().unwrap().mode() & 0o777, 0o700);
        let destination = fixture.path().join("destination");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("contents"), b"concurrent destination").unwrap();

        assert_eq!(
            publish_snapshot(staging.as_fd(), parent.as_fd(), OsStr::new("destination"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            fs::read(destination.join("contents")).unwrap(),
            b"concurrent destination"
        );
        assert_eq!(
            fs::read(staged_snapshot.join("contents")).unwrap(),
            b"new snapshot"
        );
        // Failure cleanup operates exclusively on the private staging tree.
        btrfs::remove_directory_at(parent.as_fd(), &staging_name, staging.as_fd()).unwrap();
        assert_eq!(
            fs::read(destination.join("contents")).unwrap(),
            b"concurrent destination"
        );
        assert!(!staged_snapshot.exists());
    }

    struct Fixture(tempfile::TempDir);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
                .expect("JJ_TEST_BTRFS_ROOT must name a writable directory on real Btrfs");
            Self(tempfile::tempdir_in(root).unwrap())
        }

        fn path(&self) -> &Path {
            self.0.path()
        }

        fn create(&self, path: &Path) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let output = Command::new("btrfs")
                .args(["subvolume", "create"])
                .arg(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            btrfs::remove_tree(self.0.path()).unwrap();
        }
    }

    fn readonly(path: &Path, value: bool) {
        btrfs::set_subvolume_readonly(File::open(path).unwrap().as_fd(), value).unwrap();
    }

    #[test]
    #[ignore = "requires JJ_TEST_BTRFS_ROOT and Btrfs snapshot permissions"]
    fn snapshots_arbitrary_descendants_and_readonly_parents() {
        let fixture = Fixture::new();
        let source = fixture.path().join("source");
        let destination = fixture.path().join("destination");
        let child = Path::new("ordinary/deep/arbitrary child");
        let grandchild = child.join("another directory/grandchild");
        let unusual = PathBuf::from(OsString::from_vec(b"other/child-\xff\n".to_vec()));
        for relative in [
            Path::new(""),
            child,
            grandchild.as_path(),
            unusual.as_path(),
        ] {
            fixture.create(&source.join(relative));
            fs::write(
                source.join(relative).join("contents"),
                relative.as_os_str().as_bytes(),
            )
            .unwrap();
        }
        std::os::unix::fs::symlink("/nonexistent/external", source.join("external")).unwrap();
        readonly(&source.join(&grandchild), true);
        readonly(&source.join(child), true);
        readonly(&source, true);

        // Deleting this child cannot create its private removal directory in
        // the read-only containing root. Its temporary writable transition
        // must be undone even though deletion fails before the rename.
        let child_before = btrfs::OpenedSubvolume::open(&source.join(child)).unwrap();
        assert!(btrfs::remove_subvolume_tree(&source.join(child)).is_err());
        let child_after = btrfs::OpenedSubvolume::open(&source.join(child)).unwrap();
        assert_eq!(child_after.subvolume.uuid, child_before.subvolume.uuid);
        assert!(child_after.subvolume.readonly());
        assert!(
            btrfs::OpenedSubvolume::open(&source.join(&grandchild))
                .unwrap()
                .subvolume
                .readonly()
        );

        recursive_snapshot(&source, &destination).unwrap();

        for relative in [
            Path::new(""),
            child,
            grandchild.as_path(),
            unusual.as_path(),
        ] {
            let before = btrfs::OpenedSubvolume::open(&source.join(relative)).unwrap();
            let after = btrfs::OpenedSubvolume::open(&destination.join(relative)).unwrap();
            assert_ne!(before.subvolume.uuid, after.subvolume.uuid);
            assert_eq!(after.subvolume.parent_uuid, Some(before.subvolume.uuid));
            assert_eq!(
                after.subvolume.readonly(),
                !relative.as_os_str().is_empty() && before.subvolume.readonly()
            );
            assert_eq!(
                fs::read(destination.join(relative).join("contents")).unwrap(),
                relative.as_os_str().as_bytes()
            );
        }
        assert!(
            btrfs::OpenedSubvolume::open(&source)
                .unwrap()
                .subvolume
                .readonly()
        );
        assert_eq!(
            fs::read_link(destination.join("external")).unwrap(),
            Path::new("/nonexistent/external")
        );
        fs::write(destination.join("contents"), b"new root contents").unwrap();
        fs::write(
            destination.join(&unusual).join("contents"),
            b"new child contents",
        )
        .unwrap();
        assert_eq!(fs::read(source.join("contents")).unwrap(), b"");
        assert_eq!(
            fs::read(source.join(&unusual).join("contents")).unwrap(),
            unusual.as_os_str().as_bytes()
        );
        // This exercises deleting read-only nested parents and grandchildren.
        btrfs::remove_subvolume_tree(&destination).unwrap();
        assert!(!destination.exists());
    }

    #[test]
    #[ignore = "requires JJ_TEST_BTRFS_ROOT and Btrfs snapshot permissions"]
    fn snapshots_more_children_than_one_rootref_page() {
        let fixture = Fixture::new();
        let source = fixture.path().join("source");
        fixture.create(&source);
        for index in 0..ROOTREF_COUNT + 2 {
            let child = source.join(format!("child-{index}"));
            fixture.create(&child);
            fs::write(child.join("contents"), index.to_string()).unwrap();
        }
        let destination = fixture.path().join("destination");
        recursive_snapshot(&source, &destination).unwrap();
        for index in 0..ROOTREF_COUNT + 2 {
            assert_eq!(
                fs::read_to_string(destination.join(format!("child-{index}/contents"))).unwrap(),
                index.to_string()
            );
        }
    }

    #[test]
    #[ignore = "requires JJ_TEST_BTRFS_ROOT and unprivileged Btrfs snapshot permissions"]
    fn inaccessible_source_child_removes_partial_snapshot() {
        // Root bypasses permission checks used to induce this real failure.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let fixture = Fixture::new();
        let source = fixture.path().join("source");
        fixture.create(&source);
        fixture.create(&source.join("first"));
        fs::write(source.join("first/keep"), b"preserve").unwrap();
        fixture.create(&source.join("first/private/child"));
        fs::set_permissions(source.join("first/private"), fs::Permissions::from_mode(0)).unwrap();
        let destination = fixture.path().join("destination");
        let result = recursive_snapshot(&source, &destination);
        fs::set_permissions(
            source.join("first/private"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(result.is_err());
        assert!(!destination.exists(), "partial output remains: {result:?}");
        assert_eq!(
            fs::read_dir(fixture.path()).unwrap().count(),
            1,
            "staging directory remains"
        );
        assert_eq!(fs::read(source.join("first/keep")).unwrap(), b"preserve");
        assert!(btrfs::OpenedSubvolume::open(&source.join("first/private/child")).is_ok());
    }
}
