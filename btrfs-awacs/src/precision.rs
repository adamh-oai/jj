//! Optional recursive namespace journal. Its output may improve precision but
//! is never required for snapshot correctness.

use crate::manager::{FacadeActivation, GuardCursor, MutationHint};
use crate::store::Store;
use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const WATCH_MASK: u32 = libc::IN_ATTRIB
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MODIFY
    | libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_UNMOUNT
    | libc::IN_IGNORED
    | libc::IN_Q_OVERFLOW;
const FATAL_MASK: u32 = libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_UNMOUNT
    | libc::IN_IGNORED
    | libc::IN_Q_OVERFLOW;

#[derive(Clone, Debug)]
enum WatchedDirectory {
    Tree(Vec<u8>),
    Marker,
}

#[derive(Debug)]
pub struct PrecisionGuard {
    inotify: OwnedFd,
    root: PathBuf,
    marker_directory: PathBuf,
    directories: BTreeMap<i32, WatchedDirectory>,
    epoch: [u8; 16],
    gapped: bool,
}

impl PrecisionGuard {
    pub fn arm(
        root: &Path,
        marker_directory: &Path,
        epoch: [u8; 16],
    ) -> Result<Self, PrecisionError> {
        let root = fs::canonicalize(root)
            .map_err(|error| PrecisionError::context("canonicalize precision root", error))?;
        let marker_directory = fs::canonicalize(marker_directory)
            .map_err(|error| PrecisionError::context("canonicalize marker directory", error))?;
        if marker_directory.starts_with(&root) || root.starts_with(&marker_directory) {
            return Err(PrecisionError::new(
                "precision marker directory and watched root must be disjoint",
            ));
        }
        // SAFETY: a successful call returns a new owned descriptor.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(PrecisionError::io("inotify_init1"));
        }
        // SAFETY: ownership of the new descriptor is transferred exactly once.
        let inotify = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut guard = Self {
            inotify,
            root,
            marker_directory,
            directories: BTreeMap::new(),
            epoch,
            gapped: false,
        };
        let marker = guard.marker_directory.clone();
        let marker_wd = guard.add_watch(&marker)?;
        guard
            .directories
            .insert(marker_wd, WatchedDirectory::Marker);
        let root = guard.root.clone();
        guard.add_tree(&root, Vec::new())?;
        Ok(guard)
    }

    pub fn epoch(&self) -> [u8; 16] {
        self.epoch
    }

    /// Duplicates the inotify description for a scheduler poll.  The
    /// duplicate keeps the open file description alive if the owning facade
    /// is concurrently invalidated after its lock is released.
    pub fn duplicate_readiness_fd(&self) -> Result<OwnedFd, PrecisionError> {
        // SAFETY: `self.inotify` is live and F_DUPFD_CLOEXEC returns a new fd.
        let fd = unsafe { libc::fcntl(self.inotify.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(PrecisionError::io("duplicate precision readiness fd"));
        }
        // SAFETY: ownership of the newly duplicated descriptor transfers once.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn certify(
        &mut self,
        store: &mut Store,
        activation: &FacadeActivation,
        observed_ns: i64,
    ) -> Result<GuardCursor, PrecisionError> {
        if self.gapped {
            return Err(PrecisionError::new("precision guard is gapped"));
        }
        let marker_name = format!(".btrfs-awacs-marker-{}", Uuid::new_v4());
        let marker_path = self.marker_directory.join(&marker_name);
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker_path)
            .map_err(|error| PrecisionError::context("create precision marker", error))?;
        marker
            .write_all(b"1")
            .and_then(|()| marker.sync_all())
            .map_err(|error| PrecisionError::context("commit precision marker", error))?;
        drop(marker);

        // Deletion is the terminal event. Waiting for CREATE or CLOSE_WRITE
        // would leave a later marker event readable and make an early-wakeup
        // poll spin forever. Inotify preserves event order on this instance,
        // so observing the delete drains every earlier tree event as well.
        if let Err(error) = fs::remove_file(&marker_path) {
            return self.fail(
                store,
                activation,
                PrecisionError::context("remove committed precision marker", error),
            );
        }
        self.drain_until_marker(store, activation, marker_name.as_bytes(), observed_ns)
    }

    fn drain_until_marker(
        &mut self,
        store: &mut Store,
        activation: &FacadeActivation,
        marker_name: &[u8],
        observed_ns: i64,
    ) -> Result<GuardCursor, PrecisionError> {
        for _ in 0..100 {
            let (events, marker_seen) = match self.read_events(marker_name) {
                Ok(value) => value,
                Err(error) => return self.fail(store, activation, error),
            };
            let cursor = store
                .append_precision_events(activation, self.epoch, &events, observed_ns)
                .map_err(|error| PrecisionError::context("persist precision events", error))?;
            if marker_seen {
                return Ok(cursor);
            }
            let mut descriptor = libc::pollfd {
                fd: self.inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: descriptor is one initialized pollfd and the timeout is bounded.
            let polled = unsafe { libc::poll(&mut descriptor, 1, 50) };
            if polled < 0 {
                return self.fail(
                    store,
                    activation,
                    PrecisionError::io("poll precision guard"),
                );
            }
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return self.fail(
                    store,
                    activation,
                    PrecisionError::new("precision guard descriptor failed"),
                );
            }
        }
        self.fail(
            store,
            activation,
            PrecisionError::new("timed out draining precision marker"),
        )
    }

    fn read_events(
        &mut self,
        marker_name: &[u8],
    ) -> Result<(Vec<MutationHint>, bool), PrecisionError> {
        let mut output = Vec::new();
        let mut marker_seen = false;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            // SAFETY: buffer is writable and the descriptor is nonblocking.
            let count = unsafe {
                libc::read(
                    self.inotify.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok((output, marker_seen));
                }
                return Err(PrecisionError::context("read precision events", error));
            }
            if count == 0 {
                return Err(PrecisionError::new("precision guard reached EOF"));
            }
            let mut offset = 0_usize;
            let count = count as usize;
            while offset < count {
                if count - offset < 16 {
                    return Err(PrecisionError::new("truncated precision event"));
                }
                let wd = i32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap());
                let mask = u32::from_ne_bytes(buffer[offset + 4..offset + 8].try_into().unwrap());
                let length =
                    u32::from_ne_bytes(buffer[offset + 12..offset + 16].try_into().unwrap())
                        as usize;
                let end = offset
                    .checked_add(16 + length)
                    .ok_or_else(|| PrecisionError::new("precision event length overflow"))?;
                if end > count {
                    return Err(PrecisionError::new("truncated precision event name"));
                }
                let name = buffer[offset + 16..end]
                    .split(|byte| *byte == 0)
                    .next()
                    .unwrap_or_default();
                if mask & libc::IN_Q_OVERFLOW != 0 {
                    return Err(PrecisionError::new("precision event queue overflowed"));
                }
                match self.directories.get(&wd).cloned() {
                    Some(WatchedDirectory::Marker) => {
                        if name == marker_name && mask & libc::IN_DELETE != 0 {
                            marker_seen = true;
                        }
                    }
                    Some(WatchedDirectory::Tree(parent)) => {
                        if mask & FATAL_MASK != 0 {
                            return Err(PrecisionError::new("precision subtree watch was lost"));
                        }
                        if name.is_empty() {
                            return Err(PrecisionError::new("unscoped precision event"));
                        }
                        let path = join_relative(&parent, name);
                        let directory = mask & libc::IN_ISDIR != 0;
                        if directory {
                            output.push(MutationHint::DirectoryPrefix(path.clone()));
                            if mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                                let absolute = self.root.join(OsString::from_vec(path.clone()));
                                self.add_tree(&absolute, path)?;
                            }
                        } else {
                            output.push(MutationHint::Path(path));
                        }
                    }
                    None => {
                        return Err(PrecisionError::new("event from an unknown precision watch"))
                    }
                }
                offset = end;
            }
        }
    }

    fn add_tree(&mut self, directory: &Path, relative: Vec<u8>) -> Result<(), PrecisionError> {
        let wd = self.add_watch(directory)?;
        self.directories
            .insert(wd, WatchedDirectory::Tree(relative.clone()));
        let entries = fs::read_dir(directory)
            .map_err(|error| PrecisionError::context("enumerate precision directory", error))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                PrecisionError::context("read precision directory entry", error)
            })?;
            let metadata = entry
                .file_type()
                .map_err(|error| PrecisionError::context("classify precision entry", error))?;
            if metadata.is_symlink() || !metadata.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let child_relative = join_relative(&relative, name.as_bytes());
            self.add_tree(&entry.path(), child_relative)?;
        }
        Ok(())
    }

    fn add_watch(&self, directory: &Path) -> Result<i32, PrecisionError> {
        let path = CString::new(directory.as_os_str().as_bytes())
            .map_err(|_| PrecisionError::new("precision path contains NUL"))?;
        // SAFETY: fd and NUL-terminated path are valid for the duration of the call.
        let wd =
            unsafe { libc::inotify_add_watch(self.inotify.as_raw_fd(), path.as_ptr(), WATCH_MASK) };
        if wd < 0 {
            Err(PrecisionError::io("inotify_add_watch"))
        } else {
            Ok(wd)
        }
    }

    fn fail<T>(
        &mut self,
        store: &mut Store,
        activation: &FacadeActivation,
        error: PrecisionError,
    ) -> Result<T, PrecisionError> {
        self.gapped = true;
        match store.gap_precision_guard(activation, self.epoch) {
            Ok(()) => Err(error),
            Err(gap) => Err(PrecisionError::new(format!(
                "{error}; persist precision gap: {gap}"
            ))),
        }
    }
}

fn join_relative(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = parent.to_vec();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

#[derive(Debug)]
pub struct PrecisionError {
    message: String,
}

impl PrecisionError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn context(context: &str, error: impl fmt::Display) -> Self {
        Self::new(format!("{context}: {error}"))
    }

    fn io(context: &str) -> Self {
        Self::context(context, io::Error::last_os_error())
    }
}

impl fmt::Display for PrecisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PrecisionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    #[test]
    fn recursive_guard_records_raw_paths_and_scopes_new_directories() {
        let root = TempDir::new().unwrap();
        let runtime = TempDir::new().unwrap();
        fs::create_dir(root.path().join("existing")).unwrap();
        let mut guard = PrecisionGuard::arm(root.path(), runtime.path(), [7; 16]).unwrap();

        File::create(root.path().join("existing").join("file")).unwrap();
        fs::create_dir(root.path().join("new-dir")).unwrap();
        File::create(root.path().join("new-dir").join("raced-file")).unwrap();
        let (events, marker) = guard.read_events(b"not-a-marker").unwrap();
        assert!(!marker);
        assert!(events.contains(&MutationHint::Path(b"existing/file".to_vec())));
        assert!(events.contains(&MutationHint::DirectoryPrefix(b"new-dir".to_vec())));
    }

    #[test]
    fn marker_directory_must_not_overlap_the_watched_tree() {
        let root = TempDir::new().unwrap();
        let marker = root.path().join("runtime");
        fs::create_dir(&marker).unwrap();
        assert!(PrecisionGuard::arm(root.path(), &marker, [8; 16]).is_err());
    }

    #[test]
    fn terminal_marker_delete_drains_readiness_without_a_self_wakeup() {
        let root = TempDir::new().unwrap();
        let runtime = TempDir::new().unwrap();
        let mut guard = PrecisionGuard::arm(root.path(), runtime.path(), [9; 16]).unwrap();
        File::create(root.path().join("changed")).unwrap();
        let readiness = guard.duplicate_readiness_fd().unwrap();
        let mut descriptor = libc::pollfd {
            fd: readiness.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 0) }, 1);

        let marker_name = b"barrier";
        let marker = runtime.path().join("barrier");
        File::create(&marker).unwrap();
        fs::remove_file(marker).unwrap();
        let (_, marker_seen) = guard.read_events(marker_name).unwrap();
        assert!(marker_seen);
        descriptor.revents = 0;
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 0) }, 0);
    }
}
