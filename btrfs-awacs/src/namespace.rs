//! Identity checks for the opened root of each direct snapshot scan.

use crate::btrfs::OpenedSubvolume;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewBinding {
    // Retained in existing stored records and cursor encodings. This identifies
    // the activation; it does not imply that a kernel monitor is running.
    pub monitor_session_id: [u8; 16],
    pub root_path: Vec<u8>,
    pub fs_uuid: [u8; 16],
    pub subvol_uuid: [u8; 16],
    pub mount_ns_dev: u64,
    pub mount_ns_ino: u64,
    pub process_root_dev: u64,
    pub process_root_ino: u64,
    pub process_root_mnt_id: u64,
    pub watched_root_dev: u64,
    pub watched_root_ino: u64,
    pub watched_root_mnt_id: u64,
}

impl ViewBinding {
    /// Compares the requested root with an existing activation. Each request
    /// opens its root again, but may reuse the activation for that same object.
    pub fn same_root(&self, other: &Self) -> bool {
        self.root_path == other.root_path
            && self.fs_uuid == other.fs_uuid
            && self.subvol_uuid == other.subvol_uuid
            && self.mount_ns_dev == other.mount_ns_dev
            && self.mount_ns_ino == other.mount_ns_ino
            && self.process_root_dev == other.process_root_dev
            && self.process_root_ino == other.process_root_ino
            && self.process_root_mnt_id == other.process_root_mnt_id
            && self.watched_root_dev == other.watched_root_dev
            && self.watched_root_ino == other.watched_root_ino
            && self.watched_root_mnt_id == other.watched_root_mnt_id
    }
}

/// One request's opened source. Snapshot creation must use this descriptor,
/// so a later rename or replacement of its pathname cannot redirect the cut.
#[derive(Debug)]
pub struct RootView {
    source: OpenedSubvolume,
    path: PathBuf,
    binding: ViewBinding,
}

impl RootView {
    pub fn open(root: &Path) -> Result<Self, NamespaceError> {
        let path = std::fs::canonicalize(root)
            .map_err(|error| NamespaceError::context("canonicalize scan root", error))?;
        reject_descendant_mounts(&path)?;
        let source = OpenedSubvolume::open(&path)
            .map_err(|error| NamespaceError::context("open scan subvolume", error))?;
        let mount_namespace = File::open("/proc/self/ns/mnt")
            .and_then(|file| file.metadata())
            .map_err(|error| NamespaceError::context("stat mount namespace", error))?;
        let process_root = File::open("/proc/self/root")
            .map_err(|error| NamespaceError::context("open process root", error))?;
        let process_root_identity = file_identity(process_root.as_fd())?;
        let root_identity = file_identity(source.as_fd())?;
        let binding = ViewBinding {
            monitor_session_id: *Uuid::new_v4().as_bytes(),
            root_path: path.as_os_str().as_bytes().to_vec(),
            fs_uuid: source.filesystem.fs_uuid,
            subvol_uuid: source.subvolume.uuid,
            mount_ns_dev: mount_namespace.dev(),
            mount_ns_ino: mount_namespace.ino(),
            process_root_dev: process_root_identity.device,
            process_root_ino: process_root_identity.inode,
            process_root_mnt_id: process_root_identity.mount,
            watched_root_dev: root_identity.device,
            watched_root_ino: root_identity.inode,
            watched_root_mnt_id: root_identity.mount,
        };
        Ok(Self {
            source,
            path,
            binding,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn source(&self) -> &OpenedSubvolume {
        &self.source
    }

    pub fn binding(&self) -> &ViewBinding {
        &self.binding
    }
}

struct FileIdentity {
    device: u64,
    inode: u64,
    mount: u64,
}

fn file_identity(fd: BorrowedFd<'_>) -> Result<FileIdentity, NamespaceError> {
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: stat is writable, fd is live, and AT_EMPTY_PATH selects it.
    let result = unsafe {
        libc::statx(
            fd.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_NO_AUTOMOUNT,
            libc::STATX_INO | libc::STATX_MNT_ID,
            &mut stat,
        )
    };
    if result != 0 {
        return Err(NamespaceError::io("stat opened root identity"));
    }
    Ok(FileIdentity {
        device: libc::makedev(stat.stx_dev_major, stat.stx_dev_minor),
        inode: stat.stx_ino,
        mount: stat.stx_mnt_id,
    })
}

fn reject_descendant_mounts(root: &Path) -> Result<(), NamespaceError> {
    let root = root.as_os_str().as_bytes();
    let bytes = std::fs::read("/proc/self/mountinfo")
        .map_err(|error| NamespaceError::context("read mount topology", error))?;
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
        let encoded = fields
            .get(4)
            .ok_or_else(|| NamespaceError::new("mountinfo line omits its mountpoint"))?;
        let mountpoint = unescape_mountinfo(encoded)?;
        if mountpoint.len() > root.len()
            && mountpoint.starts_with(root)
            && mountpoint.get(root.len()) == Some(&b'/')
        {
            return Err(NamespaceError::new(
                "watched root contains an existing descendant mountpoint",
            ));
        }
    }
    Ok(())
}

fn unescape_mountinfo(encoded: &[u8]) -> Result<Vec<u8>, NamespaceError> {
    let mut result = Vec::with_capacity(encoded.len());
    let mut cursor = 0;
    while cursor < encoded.len() {
        if encoded[cursor] != b'\\' {
            result.push(encoded[cursor]);
            cursor += 1;
            continue;
        }
        let octal = encoded
            .get(cursor + 1..cursor + 4)
            .ok_or_else(|| NamespaceError::new("truncated mountinfo escape"))?;
        if !octal.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
            return Err(NamespaceError::new("invalid mountinfo escape"));
        }
        let value = u16::from(octal[0] - b'0') * 64
            + u16::from(octal[1] - b'0') * 8
            + u16::from(octal[2] - b'0');
        result.push(
            u8::try_from(value)
                .map_err(|_| NamespaceError::new("mountinfo escape exceeds one byte"))?,
        );
        cursor += 4;
    }
    Ok(result)
}

#[derive(Debug)]
pub struct NamespaceError {
    message: String,
}

impl NamespaceError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(context: &str) -> Self {
        Self::context(context, io::Error::last_os_error())
    }

    fn context(context: impl fmt::Display, error: impl fmt::Display) -> Self {
        Self::new(format!("{context}: {error}"))
    }
}

impl fmt::Display for NamespaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for NamespaceError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_paths_are_unescaped_as_raw_bytes() {
        assert_eq!(
            unescape_mountinfo(br"/path\040with\011bytes\134x").unwrap(),
            b"/path with\tbytes\\x"
        );
        assert!(unescape_mountinfo(br"/bad\09x").is_err());
        assert!(unescape_mountinfo(br"/bad\777").is_err());
    }
}
