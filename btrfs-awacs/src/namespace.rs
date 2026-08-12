//! Mandatory client-view continuity monitor used before clocks can be minted.

use crate::btrfs::OpenedSubvolume;
use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

const IN_BINDING_MASK: u32 = libc::IN_ATTRIB
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_UNMOUNT
    | libc::IN_IGNORED
    | libc::IN_Q_OVERFLOW;
const IN_SELF_MASK: u32 = libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_UNMOUNT
    | libc::IN_IGNORED
    | libc::IN_Q_OVERFLOW;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewBinding {
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

#[derive(Debug)]
pub struct NamespaceMonitor {
    inotify: OwnedFd,
    mountinfo: File,
    watched_names: BTreeMap<i32, Option<Vec<u8>>>,
    canonical_root: PathBuf,
    binding: ViewBinding,
}

#[derive(Debug)]
pub struct PendingNamespaceMonitor {
    inotify: OwnedFd,
    mountinfo: File,
    watched_names: BTreeMap<i32, Option<Vec<u8>>>,
    destination_parent: PathBuf,
    destination_name: Vec<u8>,
    destination_watch: i32,
    mount_ns_dev: u64,
    mount_ns_ino: u64,
    process_root_dev: u64,
    process_root_ino: u64,
    process_root_mnt_id: u64,
}

impl PendingNamespaceMonitor {
    pub fn arm(destination_parent: &Path, destination_name: &[u8]) -> Result<Self, NamespaceError> {
        if destination_name.is_empty()
            || destination_name.contains(&b'/')
            || destination_name.contains(&b'\0')
        {
            return Err(NamespaceError::new("invalid pending destination basename"));
        }
        let destination_parent = std::fs::canonicalize(destination_parent)
            .map_err(|error| NamespaceError::context("canonicalize destination parent", error))?;
        if !destination_parent.is_absolute() {
            return Err(NamespaceError::new("destination parent must be absolute"));
        }
        let final_path = destination_parent.join(OsString::from_vec(destination_name.to_vec()));
        match std::fs::symlink_metadata(&final_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(NamespaceError::context(
                    "inspect pending destination",
                    error,
                ));
            }
            Ok(_) => return Err(NamespaceError::new("pending destination already exists")),
        }
        reject_descendant_mounts(&destination_parent)?;
        // SAFETY: successful inotify_init1 returns one new descriptor.
        let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify_fd < 0 {
            return Err(NamespaceError::io("inotify_init1"));
        }
        // SAFETY: ownership of the just-created descriptor is transferred.
        let inotify = unsafe { OwnedFd::from_raw_fd(inotify_fd) };
        let mut watched_names = arm_component_chain(inotify.as_fd(), &destination_parent)?;
        let destination_watch = add_watch(inotify.as_fd(), &destination_parent, IN_BINDING_MASK)?;
        watched_names.insert(destination_watch, Some(destination_name.to_vec()));
        let mountinfo = File::open("/proc/self/mountinfo")
            .map_err(|error| NamespaceError::context("open mountinfo monitor", error))?;
        let mount_namespace = std::fs::metadata("/proc/self/ns/mnt")
            .map_err(|error| NamespaceError::context("stat mount namespace", error))?;
        let process_root = std::fs::metadata("/proc/self/root")
            .map_err(|error| NamespaceError::context("stat process root", error))?;
        let process_root_fd = File::open("/proc/self/root")
            .map_err(|error| NamespaceError::context("open process root", error))?;
        let pending = Self {
            inotify,
            mountinfo,
            watched_names,
            destination_parent,
            destination_name: destination_name.to_vec(),
            destination_watch,
            mount_ns_dev: mount_namespace.dev(),
            mount_ns_ino: mount_namespace.ino(),
            process_root_dev: process_root.dev(),
            process_root_ino: process_root.ino(),
            process_root_mnt_id: mount_id(process_root_fd.as_fd())?,
        };
        pending.reject_mount_events()?;
        if pending.drain_expected_move(false)? {
            return Err(NamespaceError::new(
                "destination appeared while its monitor was arming",
            ));
        }
        Ok(pending)
    }

    pub fn complete(
        mut self,
        expected_fs_uuid: [u8; 16],
        expected_subvol_uuid: [u8; 16],
    ) -> Result<NamespaceMonitor, NamespaceError> {
        self.reject_mount_events()?;
        if !self.drain_expected_move(true)? {
            return Err(NamespaceError::new(
                "pending destination monitor did not observe the publication move",
            ));
        }
        self.reject_mount_events()?;
        let current_namespace = std::fs::metadata("/proc/self/ns/mnt")
            .map_err(|error| NamespaceError::context("recheck mount namespace", error))?;
        let current_process_root = std::fs::metadata("/proc/self/root")
            .map_err(|error| NamespaceError::context("recheck process root", error))?;
        let current_process_root_fd = File::open("/proc/self/root")
            .map_err(|error| NamespaceError::context("reopen process root", error))?;
        if current_namespace.dev() != self.mount_ns_dev
            || current_namespace.ino() != self.mount_ns_ino
            || current_process_root.dev() != self.process_root_dev
            || current_process_root.ino() != self.process_root_ino
            || mount_id(current_process_root_fd.as_fd())? != self.process_root_mnt_id
        {
            return Err(NamespaceError::new(
                "namespace changed during Worktree publication",
            ));
        }
        let canonical_root = self
            .destination_parent
            .join(OsString::from_vec(self.destination_name.clone()));
        reject_descendant_mounts(&canonical_root)?;
        let opened = OpenedSubvolume::open(&canonical_root)
            .map_err(|error| NamespaceError::context("open published Worktree", error))?;
        if opened.filesystem.fs_uuid != expected_fs_uuid
            || opened.subvolume.uuid != expected_subvol_uuid
        {
            return Err(NamespaceError::new(
                "published Worktree identity differs from the monitored intent",
            ));
        }
        let root_metadata = std::fs::metadata(&canonical_root)
            .map_err(|error| NamespaceError::context("stat published Worktree", error))?;
        let root_watch = add_watch(self.inotify.as_fd(), &canonical_root, IN_SELF_MASK)?;
        self.watched_names.insert(root_watch, None);
        let binding = ViewBinding {
            monitor_session_id: *Uuid::new_v4().as_bytes(),
            root_path: canonical_root.as_os_str().as_bytes().to_vec(),
            fs_uuid: opened.filesystem.fs_uuid,
            subvol_uuid: opened.subvolume.uuid,
            mount_ns_dev: self.mount_ns_dev,
            mount_ns_ino: self.mount_ns_ino,
            process_root_dev: self.process_root_dev,
            process_root_ino: self.process_root_ino,
            process_root_mnt_id: self.process_root_mnt_id,
            watched_root_dev: root_metadata.dev(),
            watched_root_ino: root_metadata.ino(),
            watched_root_mnt_id: mount_id(opened.as_fd())?,
        };
        let monitor = NamespaceMonitor {
            inotify: self.inotify,
            mountinfo: self.mountinfo,
            watched_names: self.watched_names,
            canonical_root,
            binding,
        };
        monitor.check_continuity()?;
        Ok(monitor)
    }

    fn reject_mount_events(&self) -> Result<(), NamespaceError> {
        poll_mountinfo(&self.mountinfo)
    }

    fn drain_expected_move(&self, accept_move: bool) -> Result<bool, NamespaceError> {
        let mut observed = false;
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            // SAFETY: buffer is writable and the descriptor is nonblocking.
            let read = unsafe {
                libc::read(
                    self.inotify.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(observed);
                }
                return Err(NamespaceError::context(
                    "read pending binding monitor",
                    error,
                ));
            }
            if read == 0 {
                return Err(NamespaceError::new("pending binding monitor reached EOF"));
            }
            let read = read as usize;
            let mut offset = 0;
            while offset < read {
                if read - offset < 16 {
                    return Err(NamespaceError::new("truncated pending inotify event"));
                }
                let wd = i32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap());
                let mask = u32::from_ne_bytes(buffer[offset + 4..offset + 8].try_into().unwrap());
                let length =
                    u32::from_ne_bytes(buffer[offset + 12..offset + 16].try_into().unwrap())
                        as usize;
                let end = offset
                    .checked_add(16 + length)
                    .ok_or_else(|| NamespaceError::new("pending inotify event overflow"))?;
                if end > read {
                    return Err(NamespaceError::new("truncated pending inotify name"));
                }
                let name = buffer[offset + 16..end]
                    .split(|byte| *byte == 0)
                    .next()
                    .unwrap_or_default();
                let expected_move = wd == self.destination_watch
                    && name == self.destination_name
                    && mask & libc::IN_MOVED_TO != 0;
                if expected_move {
                    if !accept_move || observed {
                        return Err(NamespaceError::new(
                            "pending destination had an unexpected publication event",
                        ));
                    }
                    observed = true;
                } else {
                    let expected = self.watched_names.get(&wd);
                    let self_event = mask
                        & (libc::IN_DELETE_SELF
                            | libc::IN_MOVE_SELF
                            | libc::IN_UNMOUNT
                            | libc::IN_IGNORED
                            | libc::IN_Q_OVERFLOW)
                        != 0;
                    let watched_object_attributes = name.is_empty() && mask & libc::IN_ATTRIB != 0;
                    if self_event
                        || watched_object_attributes
                        || expected.is_some_and(|expected| {
                            expected
                                .as_deref()
                                .is_none_or(|expected_name| expected_name == name)
                        })
                    {
                        return Err(NamespaceError::new(
                            "pending root-path monitor observed an unexpected event",
                        ));
                    }
                }
                offset = end;
            }
        }
    }
}

impl NamespaceMonitor {
    pub fn arm(root: &Path) -> Result<Self, NamespaceError> {
        let canonical_root = std::fs::canonicalize(root)
            .map_err(|error| NamespaceError::context("canonicalize watched root", error))?;
        if !canonical_root.is_absolute() {
            return Err(NamespaceError::new("watched root must be absolute"));
        }
        reject_descendant_mounts(&canonical_root)?;
        let opened = OpenedSubvolume::open(&canonical_root)
            .map_err(|error| NamespaceError::context("open watched subvolume", error))?;
        // SAFETY: successful inotify_init1 returns one new descriptor.
        let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify_fd < 0 {
            return Err(NamespaceError::io("inotify_init1"));
        }
        // SAFETY: ownership of the just-created descriptor is transferred.
        let inotify = unsafe { OwnedFd::from_raw_fd(inotify_fd) };
        let watched_names = arm_component_chain(inotify.as_fd(), &canonical_root)?;
        let mountinfo = File::open("/proc/self/mountinfo")
            .map_err(|error| NamespaceError::context("open mountinfo monitor", error))?;
        let mount_namespace = std::fs::metadata("/proc/self/ns/mnt")
            .map_err(|error| NamespaceError::context("stat mount namespace", error))?;
        let process_root = std::fs::metadata("/proc/self/root")
            .map_err(|error| NamespaceError::context("stat process root", error))?;
        let process_root_fd = File::open("/proc/self/root")
            .map_err(|error| NamespaceError::context("open process root", error))?;
        let root_metadata = std::fs::metadata(&canonical_root)
            .map_err(|error| NamespaceError::context("stat watched root", error))?;
        let binding = ViewBinding {
            monitor_session_id: *Uuid::new_v4().as_bytes(),
            root_path: canonical_root.as_os_str().as_bytes().to_vec(),
            fs_uuid: opened.filesystem.fs_uuid,
            subvol_uuid: opened.subvolume.uuid,
            mount_ns_dev: mount_namespace.dev(),
            mount_ns_ino: mount_namespace.ino(),
            process_root_dev: process_root.dev(),
            process_root_ino: process_root.ino(),
            process_root_mnt_id: mount_id(process_root_fd.as_fd())?,
            watched_root_dev: root_metadata.dev(),
            watched_root_ino: root_metadata.ino(),
            watched_root_mnt_id: mount_id(opened.as_fd())?,
        };
        let monitor = Self {
            inotify,
            mountinfo,
            watched_names,
            canonical_root,
            binding,
        };
        monitor.check_continuity()?;
        Ok(monitor)
    }

    pub fn binding(&self) -> &ViewBinding {
        &self.binding
    }

    pub fn check_continuity(&self) -> Result<(), NamespaceError> {
        self.reject_binding_events()?;
        self.reject_mount_events()?;
        let current_namespace = std::fs::metadata("/proc/self/ns/mnt")
            .map_err(|error| NamespaceError::context("recheck mount namespace", error))?;
        let current_process_root = std::fs::metadata("/proc/self/root")
            .map_err(|error| NamespaceError::context("recheck process root", error))?;
        let current_process_root_fd = File::open("/proc/self/root")
            .map_err(|error| NamespaceError::context("reopen process root", error))?;
        let current_root = std::fs::metadata(&self.canonical_root)
            .map_err(|error| NamespaceError::context("recheck watched root", error))?;
        let opened = OpenedSubvolume::open(&self.canonical_root)
            .map_err(|error| NamespaceError::context("reopen watched subvolume", error))?;
        let binding = &self.binding;
        if current_namespace.dev() != binding.mount_ns_dev
            || current_namespace.ino() != binding.mount_ns_ino
            || current_process_root.dev() != binding.process_root_dev
            || current_process_root.ino() != binding.process_root_ino
            || mount_id(current_process_root_fd.as_fd())? != binding.process_root_mnt_id
            || current_root.dev() != binding.watched_root_dev
            || current_root.ino() != binding.watched_root_ino
            || mount_id(opened.as_fd())? != binding.watched_root_mnt_id
            || opened.filesystem.fs_uuid != binding.fs_uuid
            || opened.subvolume.uuid != binding.subvol_uuid
        {
            return Err(NamespaceError::new("client namespace binding changed"));
        }
        self.reject_binding_events()?;
        self.reject_mount_events()?;
        Ok(())
    }

    fn reject_mount_events(&self) -> Result<(), NamespaceError> {
        poll_mountinfo(&self.mountinfo)
    }

    fn reject_binding_events(&self) -> Result<(), NamespaceError> {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            // SAFETY: buffer is writable and the descriptor is nonblocking.
            let read = unsafe {
                libc::read(
                    self.inotify.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                return Err(NamespaceError::context("read binding monitor", error));
            }
            if read == 0 {
                return Err(NamespaceError::new("binding monitor reached EOF"));
            }
            let mut offset = 0;
            let read = read as usize;
            while offset < read {
                if read - offset < 16 {
                    return Err(NamespaceError::new("truncated inotify event"));
                }
                let wd = i32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap());
                let mask = u32::from_ne_bytes(buffer[offset + 4..offset + 8].try_into().unwrap());
                let length =
                    u32::from_ne_bytes(buffer[offset + 12..offset + 16].try_into().unwrap())
                        as usize;
                let end = offset
                    .checked_add(16 + length)
                    .ok_or_else(|| NamespaceError::new("inotify event overflow"))?;
                if end > read {
                    return Err(NamespaceError::new("truncated inotify name"));
                }
                let name = buffer[offset + 16..end]
                    .split(|byte| *byte == 0)
                    .next()
                    .unwrap_or_default();
                let expected = self.watched_names.get(&wd);
                let self_event = mask
                    & (libc::IN_DELETE_SELF
                        | libc::IN_MOVE_SELF
                        | libc::IN_UNMOUNT
                        | libc::IN_IGNORED
                        | libc::IN_Q_OVERFLOW)
                    != 0;
                let watched_object_attributes = name.is_empty() && mask & libc::IN_ATTRIB != 0;
                if self_event
                    || watched_object_attributes
                    || expected.is_some_and(|expected| {
                        expected.as_deref().is_none_or(|expected| expected == name)
                    })
                {
                    return Err(NamespaceError::new(
                        "root-path binding monitor observed a relevant event",
                    ));
                }
                offset = end;
            }
        }
    }
}

fn poll_mountinfo(mountinfo: &File) -> Result<(), NamespaceError> {
    let mut pollfd = libc::pollfd {
        fd: mountinfo.as_raw_fd(),
        events: libc::POLLPRI | libc::POLLERR,
        revents: 0,
    };
    // SAFETY: pollfd is one initialized entry and timeout zero is nonblocking.
    let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
    if result < 0 {
        return Err(NamespaceError::io("poll mountinfo"));
    }
    if pollfd.revents & (libc::POLLPRI | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(NamespaceError::new(
            "mount topology monitor observed a change",
        ));
    }
    Ok(())
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

fn arm_component_chain(
    inotify: BorrowedFd<'_>,
    root: &Path,
) -> Result<BTreeMap<i32, Option<Vec<u8>>>, NamespaceError> {
    let components: Vec<OsString> = root
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect();
    let mut directory = PathBuf::from("/");
    let mut watches = BTreeMap::new();
    for component in &components {
        let wd = add_watch(inotify, &directory, IN_BINDING_MASK)?;
        watches.insert(wd, Some(component.as_bytes().to_vec()));
        directory.push(component);
    }
    let wd = add_watch(inotify, &directory, IN_SELF_MASK)?;
    watches.insert(wd, None);
    Ok(watches)
}

fn add_watch(fd: BorrowedFd<'_>, path: &Path, mask: u32) -> Result<i32, NamespaceError> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| NamespaceError::new("watch path contains NUL"))?;
    // SAFETY: fd and NUL-terminated path are live for the syscall.
    let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) };
    if wd < 0 {
        Err(NamespaceError::io("inotify_add_watch"))
    } else {
        Ok(wd)
    }
}

fn mount_id(fd: BorrowedFd<'_>) -> Result<u64, NamespaceError> {
    let mut statx: libc::statx = unsafe { std::mem::zeroed() };
    let empty = c"";
    // SAFETY: statx is writable, fd is live, and AT_EMPTY_PATH selects it.
    let result = unsafe {
        libc::statx(
            fd.as_raw_fd(),
            empty.as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_NO_AUTOMOUNT,
            libc::STATX_MNT_ID,
            &mut statx,
        )
    };
    if result != 0 {
        return Err(NamespaceError::io("statx mount ID"));
    }
    Ok(statx.stx_mnt_id)
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
