use crate::btrfs::{
    ChangedObjectsIoctlResult, FilesystemInfo, ROOT_INODE, SubvolumeInfo, changed_objects_v2,
    destroy_snapshot_by_id, filesystem_info, has_nested_subvolumes, inode_paths, inode_refs_batch,
    send_changed_objects, subvolume_info,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt;
use std::fs;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const BROKER_PROTOCOL_VERSION: u16 = 9;
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;
pub const MAX_FRAME_FDS: usize = 4;
const FRAME_MAGIC: &[u8; 4] = b"BAWB";
const FRAME_HEADER_SIZE: usize = 16;
const SNAPSHOT_TRASH_ENTRY_PREFIX: &[u8] = b"snapshot-";
const MANAGED_SNAPSHOT_CHILD_NAME: &[u8] = b"snapshot";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Opcode {
    Handshake = 1,
    ChangedObjects = 4,
    HasNestedSubvolumes = 5,
    TrashSnapshot = 6,
    DrainSnapshotTrash = 7,
    InodePaths = 8,
}

impl Opcode {
    pub(crate) fn decode(value: u16) -> Result<Self, BrokerError> {
        match value {
            1 => Ok(Self::Handshake),
            4 => Ok(Self::ChangedObjects),
            5 => Ok(Self::HasNestedSubvolumes),
            6 => Ok(Self::TrashSnapshot),
            7 => Ok(Self::DrainSnapshotTrash),
            8 => Ok(Self::InodePaths),
            _ => Err(BrokerError::new(format!("unknown broker opcode {value}"))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub opcode: Opcode,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(opcode: Opcode, payload: Vec<u8>) -> Result<Self, BrokerError> {
        if payload.len() > MAX_FRAME_PAYLOAD {
            return Err(BrokerError::new(format!(
                "broker payload is {} bytes, limit is {MAX_FRAME_PAYLOAD}",
                payload.len()
            )));
        }
        Ok(Self { opcode, payload })
    }
}

#[derive(Debug)]
pub struct ReceivedFrame {
    pub frame: Frame,
    pub fds: Vec<OwnedFd>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedSubvolume {
    pub filesystem_uuid: [u8; 16],
    pub subvolume_uuid: [u8; 16],
    pub root_id: u64,
    pub generation: u64,
    pub ctransid: u64,
    pub otransid: u64,
    pub parent_uuid: Option<[u8; 16]>,
    pub received_uuid: Option<[u8; 16]>,
    pub readonly: bool,
}

impl ExpectedSubvolume {
    pub fn from_observed(filesystem: &FilesystemInfo, subvolume: &SubvolumeInfo) -> Self {
        Self {
            filesystem_uuid: filesystem.fs_uuid,
            subvolume_uuid: subvolume.uuid,
            root_id: subvolume.root_id,
            generation: subvolume.generation,
            ctransid: subvolume.ctransid,
            otransid: subvolume.otransid,
            parent_uuid: subvolume.parent_uuid,
            received_uuid: subvolume.received_uuid,
            readonly: subvolume.readonly(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedObjectsExecution {
    pub parent: ExpectedSubvolume,
    pub target: ExpectedSubvolume,
    pub output_owner_uid: u32,
    pub max_output_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedObjectsResult {
    pub output_bytes: u64,
    pub manifest_hash: [u8; 32],
    /// Present only when the dedicated v2 ioctl succeeded. Legacy send-flag
    /// fallback has no kernel completion counters to prove.
    pub v2_ioctl: Option<ChangedObjectsIoctlResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NestedSubvolumesExecution {
    pub target: ExpectedSubvolume,
}

/// Bounded reverse path lookup for a fixed-width file of changed inode IDs.
///
/// The input file contains exactly inode_count big-endian u64 values. The
/// output is a private binary stream with one record per input inode:
///   magic[4] = BAWP, version:u16, header_len:u16,
///   inode_count:u64, path_count:u64,
///   repeated { ino:u64, path_count:u32, repeated { len:u32, bytes[len] } }.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InodePathsExecution {
    pub target: ExpectedSubvolume,
    pub owner_uid: u32,
    pub inode_count: u64,
    pub max_output_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodePathsResult {
    pub inode_count: u64,
    pub path_count: u64,
    pub output_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotTrashExecution {
    /// Exact immutable root identity authorized by the manager's durable
    /// unpinned-snapshot state transition.
    pub target: ExpectedSubvolume,
    /// Basename of the ordinary manager-owned wrapper directory. The broker
    /// reopens its fixed `snapshot` child to prove scope, then atomically moves
    /// the wrapper into broker-owned trash before returning to the manager.
    pub entry_name: Vec<u8>,
    /// Stable basename for the broker-owned trash directory under the supplied
    /// private trash parent. It is keyed by the live root identity so a fresh
    /// manager store after rebuild can drain older trash.
    pub trash_name: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotTrashResult {
    pub verify_elapsed_ns: u64,
    pub rename_elapsed_ns: u64,
}

#[derive(Debug)]
pub struct SnapshotTrash {
    sender: mpsc::Sender<OwnedFd>,
}

impl SnapshotTrash {
    pub fn new() -> Result<Self, BrokerError> {
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("btrfs-awacs-snapshot-trash".to_owned())
            .spawn(move || drain_snapshot_trash_worker(receiver))
            .map_err(|error| BrokerError::new(format!("start snapshot trash worker: {error}")))?;
        Ok(Self { sender })
    }

    fn queue_drain(&self, trash_dir: OwnedFd) {
        if self.sender.send(trash_dir).is_err() {
            log::error!(
                "AWACS broker snapshot trash worker stopped; retained trash will be retried after broker restart"
            );
        }
    }
}

pub const MAX_CHANGED_OBJECT_OUTPUT: u64 = 1024 * 1024 * 1024;
pub const MAX_INODE_PATH_INPUTS: u64 = 1_000_000;
pub const MAX_INODE_PATH_OUTPUT: u64 = 1024 * 1024 * 1024;
pub const INODE_REF_IOCTL_BATCH_SIZE: usize = 64;
pub const INODE_PATH_OUTPUT_MAGIC: &[u8; 4] = b"BAWP";
pub const INODE_PATH_OUTPUT_VERSION: u16 = 1;
pub const INODE_PATH_OUTPUT_HEADER_SIZE: u16 = 24;

pub fn execute_changed_objects(
    request: &ChangedObjectsExecution,
    parent_fd: BorrowedFd<'_>,
    target_fd: BorrowedFd<'_>,
    output_fd: BorrowedFd<'_>,
) -> Result<ChangedObjectsResult, BrokerError> {
    if request.max_output_bytes == 0 || request.max_output_bytes > MAX_CHANGED_OBJECT_OUTPUT {
        return Err(BrokerError::new(format!(
            "changed-object output limit must be between 1 and {MAX_CHANGED_OBJECT_OUTPUT}"
        )));
    }
    if request.parent.filesystem_uuid != request.target.filesystem_uuid
        || request.parent.root_id == request.target.root_id
        || !request.parent.readonly
        || !request.target.readonly
    {
        return Err(BrokerError::new(
            "changed-object endpoints must be distinct read-only roots on one filesystem",
        ));
    }
    let parent_before = verify_subvolume(parent_fd, &request.parent)?;
    let target_before = verify_subvolume(target_fd, &request.target)?;
    verify_output_file(output_fd, request.output_owner_uid, true)?;

    let v2_ioctl = match changed_objects_v2(
        target_fd,
        parent_fd,
        output_fd,
        request.max_output_bytes,
        100_000_000,
    ) {
        Ok(result) => {
            if result.output_bytes > request.max_output_bytes {
                return Err(BrokerError::new(
                    "v2 changed-object ioctl exceeded its output limit",
                ));
            }
            Some(result)
        }
        Err(error) if error.raw_os_error() == Some(libc::ENOTTY) => {
            // Transitional compatibility for kernels carrying only the
            // original private send flag. V2-capable kernels never fall back
            // after a validation, limit, or execution error.
            send_changed_objects(target_fd, parent_before.root_id, output_fd).map_err(
                |fallback| {
                    BrokerError::new(format!(
                        "v2 changed-object ioctl is unavailable ({error}); legacy ioctl failed: {fallback}"
                    ))
                },
            )?;
            None
        }
        Err(error) => {
            return Err(BrokerError::new(format!(
                "run fd-anchored changed-object ioctl: {error}"
            )));
        }
    };
    let parent_after = verify_subvolume(parent_fd, &request.parent)?;
    let target_after = verify_subvolume(target_fd, &request.target)?;
    if parent_after != parent_before || target_after != target_before {
        return Err(BrokerError::new(
            "changed-object endpoint metadata changed during comparison",
        ));
    }

    // Recheck the attributes which the manager could have changed while the
    // ioctl was running. The file is no longer empty, but it must still be the
    // same private, single-link, read-write regular file.
    verify_output_file(output_fd, request.output_owner_uid, false)?;
    let metadata = fd_metadata(output_fd)?;
    let output_bytes = u64::try_from(metadata.st_size)
        .map_err(|_| BrokerError::new("changed-object output has negative size"))?;
    if output_bytes > request.max_output_bytes {
        return Err(BrokerError::new(format!(
            "changed-object output is {output_bytes} bytes, limit is {}",
            request.max_output_bytes
        )));
    }
    if v2_ioctl.is_some_and(|result| result.output_bytes != output_bytes) {
        return Err(BrokerError::new(
            "v2 changed-object ioctl byte count differs from output length",
        ));
    }
    let manifest_hash = hash_fd(output_fd, output_bytes)?;
    Ok(ChangedObjectsResult {
        output_bytes,
        manifest_hash,
        v2_ioctl,
    })
}

/// Resolves paths for changed inode IDs relative to one immutable target
/// snapshot while keeping every privileged Btrfs ioctl inside the broker.
///
/// Deleted inodes are represented by a record with zero paths. This lets a
/// caller feed the complete changed-object inode set without prefiltering it.
pub fn execute_inode_paths(
    request: &InodePathsExecution,
    target_fd: BorrowedFd<'_>,
    input_fd: BorrowedFd<'_>,
    output_fd: BorrowedFd<'_>,
) -> Result<InodePathsResult, BrokerError> {
    if !request.target.readonly {
        return Err(BrokerError::new("inode-path target must be read-only"));
    }
    if request.inode_count == 0 || request.inode_count > MAX_INODE_PATH_INPUTS {
        return Err(BrokerError::new(format!(
            "inode-path input count must be between 1 and {MAX_INODE_PATH_INPUTS}"
        )));
    }
    if request.max_output_bytes < u64::from(INODE_PATH_OUTPUT_HEADER_SIZE)
        || request.max_output_bytes > MAX_INODE_PATH_OUTPUT
    {
        return Err(BrokerError::new(format!(
            "inode-path output limit must be between {} and {MAX_INODE_PATH_OUTPUT}",
            INODE_PATH_OUTPUT_HEADER_SIZE
        )));
    }
    let input_bytes = request
        .inode_count
        .checked_mul(size_of::<u64>() as u64)
        .ok_or_else(|| BrokerError::new("inode-path input size overflow"))?;
    verify_private_input_file(input_fd, request.owner_uid, input_bytes)?;
    verify_output_file(output_fd, request.owner_uid, true)?;
    let passed_before = verify_subvolume(target_fd, &request.target)?;
    // Reopen through the broker so the ioctl uses broker credentials while
    // remaining anchored to the manager-passed immutable directory fd.
    let broker_target = reopen_directory_fd(target_fd)?;
    let target_before = verify_subvolume(broker_target.as_fd(), &request.target)?;
    if target_before != passed_before {
        return Err(BrokerError::new(
            "broker-reopened inode-path target changed identity",
        ));
    }

    let input = read_exact_fd(input_fd, input_bytes, "read inode-path input")?;
    let mut seen = std::collections::BTreeSet::new();
    let mut ordered_inodes = Vec::with_capacity(
        usize::try_from(request.inode_count)
            .map_err(|_| BrokerError::new("inode-path input count exceeds usize"))?,
    );
    for encoded in input.chunks_exact(size_of::<u64>()) {
        let ino = u64::from_be_bytes(encoded.try_into().expect("fixed inode slice"));
        if ino == 0 || !seen.insert(ino) {
            return Err(BrokerError::new(
                "inode-path input must contain unique nonzero inode IDs",
            ));
        }
        ordered_inodes.push(ino);
    }
    let paths_by_inode = resolve_paths_from_batched_refs(broker_target.as_fd(), &ordered_inodes)?;
    let mut output_offset = 0_u64;
    let mut header = Vec::with_capacity(usize::from(INODE_PATH_OUTPUT_HEADER_SIZE));
    header.extend_from_slice(INODE_PATH_OUTPUT_MAGIC);
    header.extend_from_slice(&INODE_PATH_OUTPUT_VERSION.to_be_bytes());
    header.extend_from_slice(&INODE_PATH_OUTPUT_HEADER_SIZE.to_be_bytes());
    header.extend_from_slice(&request.inode_count.to_be_bytes());
    header.extend_from_slice(&0_u64.to_be_bytes());
    append_fd(
        output_fd,
        &mut output_offset,
        &header,
        request.max_output_bytes,
        "write inode-path output header",
    )?;

    let mut path_count = 0_u64;
    for ino in ordered_inodes {
        let paths = paths_by_inode.get(&ino).cloned().unwrap_or_default();
        let record_path_count = u32::try_from(paths.len())
            .map_err(|_| BrokerError::new("inode has too many reverse paths"))?;
        let mut record = Vec::new();
        record.extend_from_slice(&ino.to_be_bytes());
        record.extend_from_slice(&record_path_count.to_be_bytes());
        for path in paths {
            let length = u32::try_from(path.len())
                .map_err(|_| BrokerError::new("inode reverse path exceeds u32 length"))?;
            record.extend_from_slice(&length.to_be_bytes());
            record.extend_from_slice(&path);
            path_count = path_count
                .checked_add(1)
                .ok_or_else(|| BrokerError::new("inode-path output count overflow"))?;
        }
        append_fd(
            output_fd,
            &mut output_offset,
            &record,
            request.max_output_bytes,
            "write inode-path output record",
        )?;
    }
    write_all_at(
        output_fd,
        16,
        &path_count.to_be_bytes(),
        "write inode-path output path count",
    )?;

    let target_after = verify_subvolume(broker_target.as_fd(), &request.target)?;
    let passed_after = verify_subvolume(target_fd, &request.target)?;
    if target_after != target_before || passed_after != passed_before {
        return Err(BrokerError::new(
            "inode-path target metadata changed during lookup",
        ));
    }
    verify_private_input_file(input_fd, request.owner_uid, input_bytes)?;
    verify_output_file(output_fd, request.owner_uid, false)?;
    let metadata = fd_metadata(output_fd)?;
    let output_bytes = u64::try_from(metadata.st_size)
        .map_err(|_| BrokerError::new("inode-path output has negative size"))?;
    if output_bytes != output_offset {
        return Err(BrokerError::new(
            "inode-path output size differs from bytes written",
        ));
    }
    Ok(InodePathsResult {
        inode_count: request.inode_count,
        path_count,
        output_bytes,
    })
}

/// Resolves changed inode paths with exact-ID immediate-ref ioctl batches,
/// then resolves each unique parent directory once.
///
/// Each batch stays bounded at the kernel UAPI's 64-inode limit. Sparse IDs
/// do not scan intervening Btrfs keys.
fn resolve_paths_from_batched_refs(
    target_fd: BorrowedFd<'_>,
    inodes: &[u64],
) -> Result<BTreeMap<u64, Vec<Vec<u8>>>, BrokerError> {
    let mut sorted: Vec<_> = inodes
        .iter()
        .copied()
        .filter(|ino| *ino != ROOT_INODE)
        .collect();
    sorted.sort_unstable();
    let mut refs_by_inode = BTreeMap::new();
    for batch in sorted.chunks(INODE_REF_IOCTL_BATCH_SIZE) {
        let refs = inode_refs_batch(target_fd, batch)
            .map_err(|error| BrokerError::new(format!("lookup batched inode refs: {error}")))?;
        for (ino, refs) in refs {
            if refs_by_inode.insert(ino, refs).is_some() {
                return Err(BrokerError::new(
                    "batched inode-ref scans returned duplicate inode ranges",
                ));
            }
        }
    }

    let mut parent_paths: BTreeMap<u64, Vec<Vec<u8>>> = BTreeMap::new();
    let mut paths_by_inode = BTreeMap::new();
    for &ino in inodes {
        if ino == ROOT_INODE {
            paths_by_inode.insert(ino, vec![Vec::new()]);
            continue;
        }
        let mut paths = Vec::new();
        for reference in refs_by_inode.get(&ino).into_iter().flatten() {
            let parents = if let Some(paths) = parent_paths.get(&reference.parent_ino) {
                paths.clone()
            } else {
                let paths = match inode_paths(target_fd, reference.parent_ino) {
                    Ok(paths) => paths,
                    Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Vec::new(),
                    Err(error) => {
                        return Err(BrokerError::new(format!(
                            "resolve parent inode {} relative to target root: {error}",
                            reference.parent_ino
                        )));
                    }
                };
                parent_paths.insert(reference.parent_ino, paths.clone());
                paths
            };
            for parent in parents {
                let mut path = parent;
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(&reference.name);
                paths.push(path);
            }
        }
        paths.sort();
        paths.dedup();
        paths_by_inode.insert(ino, paths);
    }
    Ok(paths_by_inode)
}

/// Answers the nested-subvolume question from Btrfs root refs while keeping
/// the target fd and immutable identity validation inside the broker.
pub fn execute_has_nested_subvolumes(
    request: &NestedSubvolumesExecution,
    target_fd: BorrowedFd<'_>,
) -> Result<bool, BrokerError> {
    if !request.target.readonly {
        return Err(BrokerError::new(
            "nested-subvolume target must be read-only",
        ));
    }
    let received_before = verify_subvolume(target_fd, &request.target)?;
    // Btrfs tree search checks the credentials attached to the open file
    // description, not merely the broker's current credentials. Reopen "."
    // relative to the manager-passed directory fd so the operation remains
    // fd-anchored while the new description carries broker credentials.
    let broker_target = reopen_directory_fd(target_fd)?;
    let target_before = verify_subvolume(broker_target.as_fd(), &request.target)?;
    if target_before != received_before {
        return Err(BrokerError::new(
            "broker-reopened nested-subvolume target changed identity",
        ));
    }
    let has_nested = has_nested_subvolumes(broker_target.as_fd(), target_before.root_id)
        .map_err(|error| BrokerError::new(format!("query nested subvolumes: {error}")))?;
    let target_after = verify_subvolume(broker_target.as_fd(), &request.target)?;
    let received_after = verify_subvolume(target_fd, &request.target)?;
    if target_after != target_before || received_after != received_before {
        return Err(BrokerError::new(
            "nested-subvolume target metadata changed during query",
        ));
    }
    Ok(has_nested)
}

/// Moves one ordinary wrapper containing an exact read-only snapshot into
/// broker-owned trash after proving that the manager-passed target fd still
/// names the expected root and that the same root is currently reachable by
/// the requested wrapper/child names.
///
/// The durable wrapper rename is the foreground completion boundary. A broker
/// worker later deletes the verified child root by ID, so slow Btrfs metadata
/// cleanup never blocks the manager's snapshot transaction.
pub fn execute_snapshot_trash(
    trash: &SnapshotTrash,
    request: &SnapshotTrashExecution,
    manager_uid: u32,
    source_parent_fd: BorrowedFd<'_>,
    trash_parent_fd: BorrowedFd<'_>,
    target_fd: BorrowedFd<'_>,
) -> Result<SnapshotTrashResult, BrokerError> {
    if !request.target.readonly {
        return Err(BrokerError::new("snapshot trash target must be read-only"));
    }
    validate_delete_basename(&request.entry_name)?;
    validate_trash_basename(&request.trash_name)?;
    let verify_started = Instant::now();
    verify_private_manager_directory(source_parent_fd, manager_uid)?;
    verify_private_manager_directory(trash_parent_fd, manager_uid)?;
    let source_filesystem = filesystem_info(source_parent_fd).map_err(|error| {
        BrokerError::new(format!("inspect snapshot source filesystem: {error}"))
    })?;
    let trash_filesystem = filesystem_info(trash_parent_fd)
        .map_err(|error| BrokerError::new(format!("inspect trash filesystem: {error}")))?;
    if source_filesystem.fs_uuid != request.target.filesystem_uuid
        || trash_filesystem.fs_uuid != request.target.filesystem_uuid
    {
        return Err(BrokerError::new(
            "snapshot source and trash parents must be on the target filesystem",
        ));
    }
    verify_manager_owned_subvolume_root(target_fd, manager_uid)?;
    let passed_target = verify_subvolume(target_fd, &request.target)?;
    let entry = open_named_directory_fd(source_parent_fd, &request.entry_name)?;
    verify_private_manager_directory(entry.as_fd(), manager_uid)?;
    let named_target = open_named_directory_fd(entry.as_fd(), MANAGED_SNAPSHOT_CHILD_NAME)?;
    verify_manager_owned_subvolume_root(named_target.as_fd(), manager_uid)?;
    let named_subvolume = verify_subvolume(named_target.as_fd(), &request.target)?;
    if named_subvolume != passed_target {
        return Err(BrokerError::new(
            "snapshot trash basename no longer names the passed target",
        ));
    }
    let verify_elapsed_ns = elapsed_ns(verify_started);
    log::debug!(
        "AWACS broker snapshot trash phase completed: phase=open and verify elapsed={:?}",
        Duration::from_nanos(verify_elapsed_ns)
    );
    let rename_started = Instant::now();
    let trash_dir =
        open_or_create_snapshot_trash(trash_parent_fd, manager_uid, &request.trash_name)?;
    let trash_entry_name = snapshot_trash_entry_name(&request.target);
    rename_noreplace_at(
        source_parent_fd,
        &request.entry_name,
        trash_dir.as_fd(),
        &trash_entry_name,
    )?;
    fsync_directory(source_parent_fd)?;
    fsync_directory(trash_dir.as_fd())?;
    let rename_elapsed_ns = elapsed_ns(rename_started);
    log::debug!(
        "AWACS broker snapshot trash phase completed: phase=rename and fsync elapsed={:?}",
        Duration::from_nanos(rename_elapsed_ns)
    );
    trash.queue_drain(trash_dir);
    Ok(SnapshotTrashResult {
        verify_elapsed_ns,
        rename_elapsed_ns,
    })
}

/// Queues any retained broker-owned trash for asynchronous deletion. This is
/// safe to call on every manager open and is how broker restart recovery gets
/// rearmed without a persistent broker-side database.
pub fn execute_snapshot_trash_drain(
    trash: &SnapshotTrash,
    manager_uid: u32,
    trash_parent_fd: BorrowedFd<'_>,
    trash_name: &[u8],
) -> Result<(), BrokerError> {
    validate_trash_basename(trash_name)?;
    verify_private_manager_directory(trash_parent_fd, manager_uid)?;
    let trash_dir = open_or_create_snapshot_trash(trash_parent_fd, manager_uid, trash_name)?;
    trash.queue_drain(trash_dir);
    Ok(())
}

fn validate_delete_basename(name: &[u8]) -> Result<(), BrokerError> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return Err(BrokerError::new("invalid snapshot delete basename"));
    }
    Ok(())
}

fn validate_trash_basename(name: &[u8]) -> Result<(), BrokerError> {
    if name.is_empty()
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
        || name.contains(&0)
        || !name.starts_with(b".broker-trash-")
    {
        return Err(BrokerError::new("invalid snapshot trash basename"));
    }
    Ok(())
}

fn open_or_create_snapshot_trash(
    parent_fd: BorrowedFd<'_>,
    manager_uid: u32,
    name: &[u8],
) -> Result<OwnedFd, BrokerError> {
    verify_private_manager_directory(parent_fd, manager_uid)?;
    let name =
        CString::new(name).map_err(|_| BrokerError::new("snapshot trash basename contains NUL"))?;
    // SAFETY: parent fd remains valid, name is NUL terminated, and mkdirat
    // only creates this broker-private directory beneath the validated parent.
    let result = unsafe { libc::mkdirat(parent_fd.as_raw_fd(), name.as_ptr(), 0o700) };
    let created = if result == 0 {
        true
    } else if io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
        false
    } else {
        return Err(BrokerError::io("create snapshot trash directory"));
    };
    let trash_dir = open_named_directory_fd(parent_fd, name.as_bytes())?;
    let metadata = fd_metadata(trash_dir.as_fd())?;
    let broker_uid = unsafe { libc::geteuid() };
    if metadata.st_uid != broker_uid || metadata.st_mode & 0o077 != 0 {
        return Err(BrokerError::new(
            "snapshot trash directory must be private and owned by the broker",
        ));
    }
    if created {
        // Persist the broker-owned trash directory itself before any later
        // snapshot rename makes it the only remaining name for that root.
        fsync_directory(parent_fd)?;
    }
    Ok(trash_dir)
}

fn snapshot_trash_entry_name(target: &ExpectedSubvolume) -> Vec<u8> {
    format!(
        "{}{}-{:016x}",
        String::from_utf8_lossy(SNAPSHOT_TRASH_ENTRY_PREFIX),
        Uuid::from_bytes(target.subvolume_uuid),
        target.root_id,
    )
    .into_bytes()
}

fn rename_noreplace_at(
    source_parent_fd: BorrowedFd<'_>,
    source_name: &[u8],
    destination_parent_fd: BorrowedFd<'_>,
    destination_name: &[u8],
) -> Result<(), BrokerError> {
    let source_name = CString::new(source_name)
        .map_err(|_| BrokerError::new("snapshot trash source basename contains NUL"))?;
    let destination_name = CString::new(destination_name)
        .map_err(|_| BrokerError::new("snapshot trash destination basename contains NUL"))?;
    // SAFETY: both directory fds remain live, both names are NUL terminated,
    // and RENAME_NOREPLACE keeps a retry from overwriting retained evidence.
    let result = unsafe {
        libc::renameat2(
            source_parent_fd.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent_fd.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(BrokerError::io("move snapshot into broker trash"));
    }
    Ok(())
}

fn fsync_directory(fd: BorrowedFd<'_>) -> Result<(), BrokerError> {
    // SAFETY: fd is live for the duration of fsync.
    if unsafe { libc::fsync(fd.as_raw_fd()) } != 0 {
        return Err(BrokerError::io("fsync snapshot trash directory"));
    }
    Ok(())
}

fn drain_snapshot_trash_worker(receiver: mpsc::Receiver<OwnedFd>) {
    while let Ok(trash_dir) = receiver.recv() {
        if let Err(error) = drain_snapshot_trash_dir(trash_dir) {
            log::error!("AWACS broker failed to drain snapshot trash: {error}");
        }
    }
}

fn drain_snapshot_trash_dir(trash_dir: OwnedFd) -> Result<(), BrokerError> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", trash_dir.as_raw_fd()));
    let entries = fs::read_dir(&path).map_err(|_| BrokerError::io("read snapshot trash"))?;
    for entry in entries {
        let entry = entry.map_err(|_| BrokerError::io("read snapshot trash entry"))?;
        let name = entry.file_name();
        if !name.as_bytes().starts_with(SNAPSHOT_TRASH_ENTRY_PREFIX) {
            log::warn!(
                "AWACS broker ignored unexpected snapshot trash entry {:?}",
                name
            );
            continue;
        }
        let wrapper = match open_named_directory_fd(trash_dir.as_fd(), name.as_bytes()) {
            Ok(wrapper) => wrapper,
            Err(error) => {
                log::warn!(
                    "AWACS broker could not open snapshot trash entry {:?}: {error}",
                    name
                );
                continue;
            }
        };
        let target = match open_named_directory_fd(wrapper.as_fd(), MANAGED_SNAPSHOT_CHILD_NAME) {
            Ok(target) => target,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                if let Err(error) = unlink_directory_at(trash_dir.as_fd(), name.as_bytes()) {
                    log::warn!(
                        "AWACS broker could not remove empty snapshot trash wrapper {:?}: {error}",
                        name
                    );
                }
                continue;
            }
            Err(error) => {
                log::warn!(
                    "AWACS broker could not open snapshot trash child {:?}: {error}",
                    name
                );
                continue;
            }
        };
        let metadata = fd_metadata(target.as_fd())?;
        if metadata.st_ino != ROOT_INODE || metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
            log::warn!(
                "AWACS broker ignored non-subvolume snapshot trash entry {:?}",
                name
            );
            continue;
        }
        let subvolume = subvolume_info(target.as_fd())
            .map_err(|error| BrokerError::new(format!("inspect snapshot trash entry: {error}")))?;
        if !subvolume.readonly() {
            log::warn!(
                "AWACS broker ignored writable snapshot trash entry {:?}",
                name
            );
            continue;
        }
        let destroy_started = Instant::now();
        match destroy_snapshot_by_id(wrapper.as_fd(), subvolume.root_id) {
            Ok(()) => {
                if let Err(error) = unlink_directory_at(trash_dir.as_fd(), name.as_bytes()) {
                    log::warn!(
                        "AWACS broker snapshot trash delete left wrapper {:?}: {error}",
                        name
                    );
                }
                log::debug!(
                    "AWACS broker snapshot trash delete completed: entry={:?} elapsed={:?}",
                    name,
                    destroy_started.elapsed(),
                );
            }
            Err(error) => log::warn!(
                "AWACS broker snapshot trash delete failed: entry={:?} elapsed={:?} error={error}",
                name,
                destroy_started.elapsed(),
            ),
        }
    }
    Ok(())
}

fn unlink_directory_at(parent_fd: BorrowedFd<'_>, name: &[u8]) -> Result<(), BrokerError> {
    let name = CString::new(name)
        .map_err(|_| BrokerError::new("snapshot trash wrapper basename contains NUL"))?;
    // SAFETY: parent_fd remains live and name is NUL terminated. AT_REMOVEDIR
    // refuses to remove anything except the now-empty wrapper directory.
    if unsafe { libc::unlinkat(parent_fd.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(BrokerError::io("remove snapshot trash wrapper"));
    }
    fsync_directory(parent_fd)
}

fn verify_private_manager_directory(
    fd: BorrowedFd<'_>,
    manager_uid: u32,
) -> Result<(), BrokerError> {
    let metadata = fd_metadata(fd)?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(BrokerError::new(
            "snapshot delete parent fd is not a directory",
        ));
    }
    if metadata.st_uid != manager_uid || metadata.st_mode & 0o077 != 0 {
        return Err(BrokerError::new(
            "snapshot delete parent must be a private directory owned by the manager",
        ));
    }
    Ok(())
}

fn verify_manager_owned_subvolume_root(
    fd: BorrowedFd<'_>,
    manager_uid: u32,
) -> Result<(), BrokerError> {
    let metadata = fd_metadata(fd)?;
    if metadata.st_uid != manager_uid {
        return Err(BrokerError::new(
            "snapshot delete target must be owned by the manager",
        ));
    }
    Ok(())
}

fn open_named_directory_fd(parent_fd: BorrowedFd<'_>, name: &[u8]) -> Result<OwnedFd, BrokerError> {
    let name = CString::new(name)
        .map_err(|_| BrokerError::new("snapshot delete basename contains NUL"))?;
    // SAFETY: parent fd remains valid, name is NUL terminated, and a
    // successful returned descriptor is uniquely adopted into OwnedFd.
    let opened = unsafe {
        libc::openat(
            parent_fd.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if opened < 0 {
        return Err(BrokerError::io("open named snapshot for delete"));
    }
    // SAFETY: openat returned one new owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(opened) })
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn reopen_directory_fd(fd: BorrowedFd<'_>) -> Result<OwnedFd, BrokerError> {
    let dot = CString::new(".").expect("static directory name has no NUL");
    // SAFETY: fd remains valid for the syscall, dot is NUL terminated, and a
    // successful returned descriptor is uniquely adopted into OwnedFd.
    let reopened = unsafe {
        libc::openat(
            fd.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if reopened < 0 {
        return Err(BrokerError::io("reopen broker subvolume fd"));
    }
    // SAFETY: openat returned a new owned descriptor on success.
    Ok(unsafe { OwnedFd::from_raw_fd(reopened) })
}

fn verify_subvolume(
    fd: BorrowedFd<'_>,
    expected: &ExpectedSubvolume,
) -> Result<SubvolumeInfo, BrokerError> {
    let metadata = fd_metadata(fd)?;
    if metadata.st_ino != ROOT_INODE || metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(BrokerError::new(
            "broker subvolume fd is not directory inode 256",
        ));
    }
    let filesystem = filesystem_info(fd)
        .map_err(|error| BrokerError::new(format!("inspect Btrfs filesystem: {error}")))?;
    let subvolume = subvolume_info(fd)
        .map_err(|error| BrokerError::new(format!("inspect Btrfs subvolume: {error}")))?;
    if ExpectedSubvolume::from_observed(&filesystem, &subvolume) != *expected {
        return Err(BrokerError::new(
            "broker subvolume fd does not match the expected immutable identity",
        ));
    }
    Ok(subvolume)
}

fn verify_output_file(
    fd: BorrowedFd<'_>,
    expected_uid: u32,
    require_empty: bool,
) -> Result<(), BrokerError> {
    let metadata = fd_metadata(fd)?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_nlink != 1
        || metadata.st_uid != expected_uid
        || (require_empty && metadata.st_size != 0)
        || metadata.st_mode & 0o077 != 0
    {
        return Err(BrokerError::new(
            "broker output must be a new, private, single-link regular file owned by the manager",
        ));
    }
    // SAFETY: F_GETFL does not modify memory and fd remains borrowed.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(BrokerError::io("read changed-object output flags"));
    }
    if flags & libc::O_ACCMODE != libc::O_RDWR {
        return Err(BrokerError::new(
            "changed-object output fd must be open read-write",
        ));
    }
    Ok(())
}

fn verify_private_input_file(
    fd: BorrowedFd<'_>,
    expected_uid: u32,
    expected_size: u64,
) -> Result<(), BrokerError> {
    let metadata = fd_metadata(fd)?;
    let size = u64::try_from(metadata.st_size)
        .map_err(|_| BrokerError::new("inode-path input has negative size"))?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_nlink != 1
        || metadata.st_uid != expected_uid
        || metadata.st_mode & 0o077 != 0
        || size != expected_size
    {
        return Err(BrokerError::new(
            "broker input must be a private, single-link regular file owned by the manager with the expected size",
        ));
    }
    // SAFETY: F_GETFL does not modify memory and fd remains borrowed.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(BrokerError::io("read inode-path input flags"));
    }
    if flags & libc::O_ACCMODE == libc::O_WRONLY {
        return Err(BrokerError::new(
            "inode-path input fd must be open read-only or read-write",
        ));
    }
    Ok(())
}

fn fd_metadata(fd: BorrowedFd<'_>) -> Result<libc::stat, BrokerError> {
    // SAFETY: fstat initializes the provided stat buffer for a valid borrowed
    // descriptor and does not retain its address.
    unsafe {
        let mut metadata: libc::stat = zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut metadata) != 0 {
            return Err(BrokerError::io("fstat broker fd"));
        }
        Ok(metadata)
    }
}

fn hash_fd(fd: BorrowedFd<'_>, length: u64) -> Result<[u8; 32], BrokerError> {
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut offset = 0_u64;
    while offset < length {
        let wanted = usize::try_from((length - offset).min(buffer.len() as u64))
            .expect("bounded by local buffer");
        // SAFETY: pread writes at most wanted bytes into the live buffer and
        // does not change the shared file offset.
        let read = unsafe {
            libc::pread(
                fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                wanted,
                offset
                    .try_into()
                    .map_err(|_| BrokerError::new("manifest offset exceeds off_t"))?,
            )
        };
        if read < 0 {
            return Err(BrokerError::io("read changed-object manifest for hashing"));
        }
        if read == 0 {
            return Err(BrokerError::new(
                "changed-object manifest ended while hashing",
            ));
        }
        hash.update(&buffer[..read as usize]);
        offset += read as u64;
    }
    Ok(hash.finalize().into())
}

fn read_exact_fd(fd: BorrowedFd<'_>, length: u64, context: &str) -> Result<Vec<u8>, BrokerError> {
    let length = usize::try_from(length)
        .map_err(|_| BrokerError::new("broker input length exceeds usize"))?;
    let mut bytes = vec![0_u8; length];
    let mut offset = 0_usize;
    while offset < bytes.len() {
        // SAFETY: pread writes into the remaining live byte slice and does
        // not change the shared file offset.
        let read = unsafe {
            libc::pread(
                fd.as_raw_fd(),
                bytes[offset..].as_mut_ptr().cast(),
                bytes.len() - offset,
                offset
                    .try_into()
                    .map_err(|_| BrokerError::new("broker input offset exceeds off_t"))?,
            )
        };
        if read < 0 {
            return Err(BrokerError::io(context));
        }
        if read == 0 {
            return Err(BrokerError::new(format!("{context}: unexpected EOF")));
        }
        offset += read as usize;
    }
    Ok(bytes)
}

fn append_fd(
    fd: BorrowedFd<'_>,
    offset: &mut u64,
    bytes: &[u8],
    limit: u64,
    context: &str,
) -> Result<(), BrokerError> {
    let length = u64::try_from(bytes.len())
        .map_err(|_| BrokerError::new("broker output chunk length exceeds u64"))?;
    let next = offset
        .checked_add(length)
        .ok_or_else(|| BrokerError::new("broker output length overflow"))?;
    if next > limit {
        return Err(BrokerError::new(format!(
            "inode-path output exceeds its {limit}-byte limit"
        )));
    }
    write_all_at(fd, *offset, bytes, context)?;
    *offset = next;
    Ok(())
}

fn write_all_at(
    fd: BorrowedFd<'_>,
    mut offset: u64,
    mut bytes: &[u8],
    context: &str,
) -> Result<(), BrokerError> {
    while !bytes.is_empty() {
        // SAFETY: pwrite reads from the live byte slice and does not change
        // the shared file offset.
        let written = unsafe {
            libc::pwrite(
                fd.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                offset
                    .try_into()
                    .map_err(|_| BrokerError::new("broker output offset exceeds off_t"))?,
            )
        };
        if written < 0 {
            return Err(BrokerError::io(context));
        }
        if written == 0 {
            return Err(BrokerError::new(format!(
                "{context}: short zero-byte write"
            )));
        }
        let written = written as usize;
        bytes = &bytes[written..];
        offset = offset
            .checked_add(written as u64)
            .ok_or_else(|| BrokerError::new("broker output offset overflow"))?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct SeqPacket {
    fd: OwnedFd,
}

#[derive(Debug)]
pub struct SeqPacketListener {
    fd: OwnedFd,
}

impl SeqPacket {
    pub fn pair() -> Result<(Self, Self), BrokerError> {
        let mut fds = [-1; 2];
        // SAFETY: fds points to two writable integers. On success ownership of
        // both returned descriptors is transferred into OwnedFd below.
        let result = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if result != 0 {
            return Err(BrokerError::io("create broker socketpair"));
        }
        // SAFETY: socketpair succeeded and returned two unique owned fds.
        Ok(unsafe {
            (
                Self {
                    fd: OwnedFd::from_raw_fd(fds[0]),
                },
                Self {
                    fd: OwnedFd::from_raw_fd(fds[1]),
                },
            )
        })
    }

    pub fn connect(path: &Path) -> Result<Self, BrokerError> {
        let (address, length) = unix_socket_address(path)?;
        // SAFETY: socket returns one owned descriptor on success.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(BrokerError::io("create broker client socket"));
        }
        // SAFETY: ownership of the just-created descriptor is transferred.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: address and its computed initialized length remain live.
        if unsafe { libc::connect(fd.as_raw_fd(), (&raw const address).cast(), length) } != 0 {
            return Err(BrokerError::io("connect broker socket"));
        }
        Ok(Self { fd })
    }

    pub fn send(&self, frame: &Frame, fds: &[BorrowedFd<'_>]) -> Result<(), BrokerError> {
        if frame.payload.len() > MAX_FRAME_PAYLOAD {
            return Err(BrokerError::new("broker payload exceeds frame limit"));
        }
        if fds.len() > MAX_FRAME_FDS {
            return Err(BrokerError::new(
                "too many file descriptors in broker frame",
            ));
        }
        let header = encode_header(frame, fds.len())?;
        let iovecs = [
            libc::iovec {
                iov_base: header.as_ptr().cast_mut().cast(),
                iov_len: header.len(),
            },
            libc::iovec {
                iov_base: frame.payload.as_ptr().cast_mut().cast(),
                iov_len: frame.payload.len(),
            },
        ];
        // This buffer is aligned for cmsghdr by using usize elements.
        let mut control = [0_usize; 16];
        // SAFETY: every pointer and length in msg refers to a live buffer for
        // the duration of sendmsg. SCM_RIGHTS contains valid borrowed fds.
        let sent = unsafe {
            let mut message: libc::msghdr = zeroed();
            message.msg_iov = iovecs.as_ptr().cast_mut();
            message.msg_iovlen = iovecs.len();
            if !fds.is_empty() {
                let control_len = libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) as usize;
                if control_len > std::mem::size_of_val(&control) {
                    return Err(BrokerError::new("SCM_RIGHTS control buffer overflow"));
                }
                message.msg_control = control.as_mut_ptr().cast();
                message.msg_controllen = control_len;
                let cmsg = libc::CMSG_FIRSTHDR(&message);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as usize;
                let destination = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for (index, fd) in fds.iter().enumerate() {
                    destination.add(index).write(fd.as_raw_fd());
                }
            }
            libc::sendmsg(self.fd.as_raw_fd(), &message, libc::MSG_NOSIGNAL)
        };
        if sent < 0 {
            return Err(BrokerError::io("send broker frame"));
        }
        let expected = header.len() + frame.payload.len();
        if sent as usize != expected {
            return Err(BrokerError::new(format!(
                "short broker packet write: {sent} of {expected} bytes"
            )));
        }
        Ok(())
    }

    pub fn receive(&self) -> Result<ReceivedFrame, BrokerError> {
        let mut bytes = vec![0_u8; FRAME_HEADER_SIZE + MAX_FRAME_PAYLOAD];
        let mut control = [0_usize; 16];
        // SAFETY: msg points at writable live byte/control buffers. Any fds
        // returned by the kernel are immediately wrapped or closed below.
        let (received, flags, raw_fds) = unsafe {
            let mut iovec = libc::iovec {
                iov_base: bytes.as_mut_ptr().cast(),
                iov_len: bytes.len(),
            };
            let mut message: libc::msghdr = zeroed();
            message.msg_iov = &mut iovec;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = std::mem::size_of_val(&control);
            let received = libc::recvmsg(self.fd.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC);
            if received < 0 {
                return Err(BrokerError::io("receive broker frame"));
            }
            let mut raw_fds = Vec::new();
            let mut cmsg = libc::CMSG_FIRSTHDR(&message);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
                    for fd in raw_fds {
                        libc::close(fd);
                    }
                    return Err(BrokerError::new(
                        "broker frame contains unsupported ancillary data",
                    ));
                }
                let header_len = libc::CMSG_LEN(0) as usize;
                let data_len = (*cmsg).cmsg_len.checked_sub(header_len).ok_or_else(|| {
                    BrokerError::new("broker frame has malformed ancillary length")
                })?;
                if data_len % size_of::<RawFd>() != 0 {
                    for fd in raw_fds {
                        libc::close(fd);
                    }
                    return Err(BrokerError::new(
                        "broker SCM_RIGHTS payload has partial descriptor",
                    ));
                }
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for index in 0..data_len / size_of::<RawFd>() {
                    raw_fds.push(data.add(index).read());
                }
                cmsg = libc::CMSG_NXTHDR(&message, cmsg);
            }
            (received as usize, message.msg_flags, raw_fds)
        };

        let fds: Vec<OwnedFd> = raw_fds
            .into_iter()
            .map(|fd| {
                // SAFETY: each descriptor was transferred exactly once by
                // SCM_RIGHTS and is now uniquely owned by this vector.
                unsafe { OwnedFd::from_raw_fd(fd) }
            })
            .collect();
        if flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(BrokerError::new(
                "broker frame or ancillary data was truncated",
            ));
        }
        if received < FRAME_HEADER_SIZE {
            return Err(BrokerError::new("truncated broker frame header"));
        }
        bytes.truncate(received);
        let (opcode, payload_len, fd_count) = decode_header(&bytes[..FRAME_HEADER_SIZE])?;
        if received != FRAME_HEADER_SIZE + payload_len {
            return Err(BrokerError::new(format!(
                "broker frame length mismatch: packet={received}, payload={payload_len}"
            )));
        }
        if fds.len() != fd_count {
            let actual = fds.len();
            return Err(BrokerError::new(format!(
                "broker frame declared {fd_count} descriptors but carried {}",
                actual
            )));
        }
        Ok(ReceivedFrame {
            frame: Frame {
                opcode,
                payload: bytes[FRAME_HEADER_SIZE..].to_vec(),
            },
            fds,
        })
    }

    pub fn peer_credentials(&self) -> Result<PeerCredentials, BrokerError> {
        // SAFETY: getsockopt writes one ucred to the initialized buffer and
        // updates its length. self owns a connected AF_UNIX socket.
        unsafe {
            let mut credentials: libc::ucred = zeroed();
            let mut length = size_of::<libc::ucred>() as libc::socklen_t;
            let result = libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            );
            if result != 0 {
                return Err(BrokerError::io("read broker peer credentials"));
            }
            if length as usize != size_of::<libc::ucred>() {
                return Err(BrokerError::new("unexpected SO_PEERCRED length"));
            }
            Ok(PeerCredentials {
                pid: credentials
                    .pid
                    .try_into()
                    .map_err(|_| BrokerError::new("SO_PEERCRED returned a negative process ID"))?,
                uid: credentials.uid,
                gid: credentials.gid,
            })
        }
    }
}

impl SeqPacketListener {
    /// Binds a new socket path. Existing entries are never unlinked.
    pub fn bind(path: &Path, mode: u32) -> Result<Self, BrokerError> {
        if mode & !0o777 != 0 {
            return Err(BrokerError::new(
                "broker socket mode contains unsupported bits",
            ));
        }
        let (address, length) = unix_socket_address(path)?;
        // SAFETY: socket returns one owned descriptor on success.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(BrokerError::io("create broker listener socket"));
        }
        // SAFETY: ownership of the just-created descriptor is transferred.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: address and its computed initialized length remain live.
        if unsafe { libc::bind(fd.as_raw_fd(), (&raw const address).cast(), length) } != 0 {
            return Err(BrokerError::io("bind broker socket"));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| BrokerError::new(format!("set broker socket mode: {error}")))?;
        // SAFETY: fd is a bound SOCK_SEQPACKET socket.
        if unsafe { libc::listen(fd.as_raw_fd(), 128) } != 0 {
            return Err(BrokerError::io("listen on broker socket"));
        }
        Ok(Self { fd })
    }

    pub fn accept(&self) -> Result<SeqPacket, BrokerError> {
        // SAFETY: accept4 returns one new owned descriptor and no peer address
        // is requested.
        let raw = unsafe {
            libc::accept4(
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(BrokerError::io("accept broker connection"));
        }
        // SAFETY: accept4 returned a fresh descriptor.
        Ok(SeqPacket {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        })
    }
}

fn unix_socket_address(path: &Path) -> Result<(libc::sockaddr_un, libc::socklen_t), BrokerError> {
    let bytes = path.as_os_str().as_bytes();
    if !path.is_absolute() || bytes.contains(&0) {
        return Err(BrokerError::new(
            "broker socket path must be absolute and contain no NUL",
        ));
    }
    // SAFETY: zero is a valid initialization for sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if bytes.len() >= address.sun_path.len() {
        return Err(BrokerError::new("broker socket path exceeds sockaddr_un"));
    }
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        .checked_add(bytes.len() + 1)
        .and_then(|value| libc::socklen_t::try_from(value).ok())
        .ok_or_else(|| BrokerError::new("broker socket address length overflow"))?;
    Ok((address, length))
}

fn encode_header(frame: &Frame, fd_count: usize) -> Result<[u8; FRAME_HEADER_SIZE], BrokerError> {
    let payload_len = u32::try_from(frame.payload.len())
        .map_err(|_| BrokerError::new("broker payload length exceeds u32"))?;
    let fd_count = u16::try_from(fd_count)
        .map_err(|_| BrokerError::new("broker descriptor count exceeds u16"))?;
    let mut header = [0; FRAME_HEADER_SIZE];
    header[..4].copy_from_slice(FRAME_MAGIC);
    header[4..6].copy_from_slice(&BROKER_PROTOCOL_VERSION.to_be_bytes());
    header[6..8].copy_from_slice(&(frame.opcode as u16).to_be_bytes());
    header[8..10].copy_from_slice(&0_u16.to_be_bytes());
    header[10..12].copy_from_slice(&fd_count.to_be_bytes());
    header[12..16].copy_from_slice(&payload_len.to_be_bytes());
    Ok(header)
}

fn decode_header(header: &[u8]) -> Result<(Opcode, usize, usize), BrokerError> {
    if header.len() != FRAME_HEADER_SIZE || &header[..4] != FRAME_MAGIC {
        return Err(BrokerError::new(
            "invalid broker frame magic or header length",
        ));
    }
    let version = u16::from_be_bytes(header[4..6].try_into().expect("fixed header"));
    if version != BROKER_PROTOCOL_VERSION {
        return Err(BrokerError::new(format!(
            "unsupported broker protocol version {version}"
        )));
    }
    let opcode = Opcode::decode(u16::from_be_bytes(
        header[6..8].try_into().expect("fixed header"),
    ))?;
    let flags = u16::from_be_bytes(header[8..10].try_into().expect("fixed header"));
    if flags != 0 {
        return Err(BrokerError::new(format!(
            "unsupported broker frame flags {flags:#x}"
        )));
    }
    let fd_count = usize::from(u16::from_be_bytes(
        header[10..12].try_into().expect("fixed header"),
    ));
    if fd_count > MAX_FRAME_FDS {
        return Err(BrokerError::new(
            "broker frame descriptor count exceeds limit",
        ));
    }
    let payload_len = u32::from_be_bytes(header[12..16].try_into().expect("fixed header")) as usize;
    if payload_len > MAX_FRAME_PAYLOAD {
        return Err(BrokerError::new("broker frame payload exceeds limit"));
    }
    Ok((opcode, payload_len, fd_count))
}

#[derive(Debug, Default)]
pub struct SessionGate {
    sessions: Mutex<BTreeMap<[u8; 16], SessionState>>,
    drained: Condvar,
    handshakes: Mutex<()>,
}

#[derive(Debug)]
struct SessionState {
    session_id: [u8; 16],
    in_flight: usize,
}

#[derive(Debug)]
pub struct SessionPermit<'a> {
    gate: &'a SessionGate,
    manager_store_uuid: [u8; 16],
}

impl SessionGate {
    pub fn handshake(&self, manager_store_uuid: [u8; 16]) -> [u8; 16] {
        // Only one handshake may install/wait a session at a time. Otherwise
        // two simultaneous recovery clients could each return an ID already
        // fenced by the other before either receives its response.
        let _handshake = self
            .handshakes
            .lock()
            .expect("broker handshake mutex poisoned");
        let session = *Uuid::new_v4().as_bytes();
        let mut sessions = self.sessions.lock().expect("broker session mutex poisoned");
        let in_flight = sessions
            .get(&manager_store_uuid)
            .map_or(0, |state| state.in_flight);
        sessions.insert(
            manager_store_uuid,
            SessionState {
                session_id: session,
                in_flight,
            },
        );
        while sessions
            .get(&manager_store_uuid)
            .is_some_and(|state| state.in_flight != 0)
        {
            sessions = self
                .drained
                .wait(sessions)
                .expect("broker session mutex poisoned while draining");
        }
        session
    }

    pub fn authorize(
        &self,
        manager_store_uuid: [u8; 16],
        manager_session_id: [u8; 16],
    ) -> Result<SessionPermit<'_>, BrokerError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| BrokerError::new("broker session mutex poisoned"))?;
        let Some(state) = sessions.get_mut(&manager_store_uuid) else {
            return Err(BrokerError::new("manager session has been fenced"));
        };
        if state.session_id != manager_session_id {
            return Err(BrokerError::new("manager session has been fenced"));
        }
        state.in_flight = state
            .in_flight
            .checked_add(1)
            .ok_or_else(|| BrokerError::new("broker in-flight request count overflow"))?;
        Ok(SessionPermit {
            gate: self,
            manager_store_uuid,
        })
    }

    pub fn join(
        &self,
        manager_store_uuid: [u8; 16],
        manager_session_id: [u8; 16],
    ) -> Result<(), BrokerError> {
        let permit = self.authorize(manager_store_uuid, manager_session_id)?;
        drop(permit);
        Ok(())
    }
}

impl Drop for SessionPermit<'_> {
    fn drop(&mut self) {
        let Ok(mut sessions) = self.gate.sessions.lock() else {
            return;
        };
        let Some(state) = sessions.get_mut(&self.manager_store_uuid) else {
            return;
        };
        debug_assert!(state.in_flight != 0);
        state.in_flight = state.in_flight.saturating_sub(1);
        if state.in_flight == 0 {
            self.gate.drained.notify_all();
        }
    }
}

#[derive(Debug)]
pub struct BrokerError {
    message: String,
    raw_os_error: Option<i32>,
}

impl BrokerError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            raw_os_error: None,
        }
    }

    fn io(context: &str) -> Self {
        let error = io::Error::last_os_error();
        Self {
            message: format!("{context}: {error}"),
            raw_os_error: error.raw_os_error(),
        }
    }

    pub fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }
}

impl fmt::Display for BrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BrokerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::os::fd::AsFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use tempfile::tempdir;

    #[test]
    fn seqpacket_round_trips_one_bounded_frame_and_rights() {
        let (left, right) = SeqPacket::pair().unwrap();
        let file = File::open("/dev/null").unwrap();
        let frame = Frame::new(Opcode::ChangedObjects, vec![0, 1, 2, 0xff]).unwrap();
        left.send(&frame, &[file.as_fd()]).unwrap();
        let received = right.receive().unwrap();
        assert_eq!(received.frame, frame);
        assert_eq!(received.fds.len(), 1);
        let flags = unsafe { libc::fcntl(received.fds[0].as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        match left.peer_credentials() {
            Ok(credentials) => assert_eq!(credentials.uid, unsafe { libc::geteuid() }),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                // Some sandboxes block SO_PEERCRED; production startup
                // treats this as fatal rather than bypassing authentication.
            }
            Err(error) => panic!("read peer credentials: {error}"),
        }
    }

    #[test]
    fn removed_broker_operations_are_not_protocol_opcodes() {
        assert!(Opcode::decode(2).is_err());
        assert!(Opcode::decode(3).is_err());
        assert_eq!(Opcode::decode(5).unwrap(), Opcode::HasNestedSubvolumes);
        assert_eq!(Opcode::decode(6).unwrap(), Opcode::TrashSnapshot);
        assert_eq!(Opcode::decode(7).unwrap(), Opcode::DrainSnapshotTrash);
        assert!(Opcode::decode(9).is_err());
    }

    #[test]
    fn snapshot_trash_rejects_non_readonly_target_before_fd_use() {
        let file = File::open("/dev/null").unwrap();
        let trash = SnapshotTrash::new().unwrap();
        let request = SnapshotTrashExecution {
            target: ExpectedSubvolume {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                root_id: ROOT_INODE,
                generation: 1,
                ctransid: 1,
                otransid: 1,
                parent_uuid: None,
                received_uuid: None,
                readonly: false,
            },
            entry_name: b"entry".to_vec(),
            trash_name: b".broker-trash-test".to_vec(),
        };
        let error = execute_snapshot_trash(
            &trash,
            &request,
            unsafe { libc::geteuid() },
            file.as_fd(),
            file.as_fd(),
            file.as_fd(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("snapshot trash target must be read-only")
        );
    }

    #[test]
    fn seqpacket_listener_connects_without_replacing_existing_paths() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("broker.sock");
        let listener = match SeqPacketListener::bind(&path, 0o600) {
            Ok(listener) => listener,
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => return,
            Err(error) => panic!("bind test broker socket: {error}"),
        };
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(SeqPacketListener::bind(&path, 0o600).is_err());
        let client = SeqPacket::connect(&path).unwrap();
        let server = listener.accept().unwrap();
        client
            .send(&Frame::new(Opcode::Handshake, vec![1, 2]).unwrap(), &[])
            .unwrap();
        assert_eq!(server.receive().unwrap().frame.payload, vec![1, 2]);
    }

    #[test]
    fn changed_object_output_requires_a_private_new_read_write_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manifest.part");
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let uid = unsafe { libc::geteuid() };
        verify_output_file(output.as_fd(), uid, true).unwrap();

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            verify_output_file(output.as_fd(), uid, true)
                .unwrap_err()
                .to_string()
                .contains("private")
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let alias = temp.path().join("manifest.alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(
            verify_output_file(output.as_fd(), uid, true)
                .unwrap_err()
                .to_string()
                .contains("single-link")
        );
    }

    #[test]
    fn changed_object_output_rejects_read_only_and_nonempty_files() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manifest.part");
        fs::write(&path, b"stale").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let uid = unsafe { libc::geteuid() };
        let read_only = File::open(&path).unwrap();
        assert!(verify_output_file(read_only.as_fd(), uid, true).is_err());

        let read_write = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(
            verify_output_file(read_write.as_fd(), uid, true)
                .unwrap_err()
                .to_string()
                .contains("new")
        );
        verify_output_file(read_write.as_fd(), uid, false).unwrap();
    }

    #[test]
    fn changed_object_execution_rejects_non_subvolume_fds_before_ioctl() {
        let null = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let endpoint = |root_id| ExpectedSubvolume {
            filesystem_uuid: [1; 16],
            subvolume_uuid: [root_id as u8; 16],
            root_id,
            generation: 1,
            ctransid: 1,
            otransid: 1,
            parent_uuid: None,
            received_uuid: None,
            readonly: true,
        };
        let request = ChangedObjectsExecution {
            parent: endpoint(10),
            target: endpoint(11),
            output_owner_uid: unsafe { libc::geteuid() },
            max_output_bytes: 1024,
        };
        assert!(
            execute_changed_objects(&request, null.as_fd(), null.as_fd(), null.as_fd())
                .unwrap_err()
                .to_string()
                .contains("inode 256")
        );
    }

    #[test]
    fn new_handshake_fences_prior_session() {
        let gate = SessionGate::default();
        let store = [1; 16];
        let first = gate.handshake(store);
        gate.join(store, first).unwrap();
        gate.authorize(store, first).unwrap();
        let second = gate.handshake(store);
        assert!(gate.authorize(store, first).is_err());
        gate.authorize(store, second).unwrap();
    }

    #[test]
    fn recovery_handshake_waits_for_an_authorized_request_to_drain() {
        let gate = std::sync::Arc::new(SessionGate::default());
        let store = [2; 16];
        let first = gate.handshake(store);
        let permit = gate.authorize(store, first).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let worker_gate = std::sync::Arc::clone(&gate);
        let worker = std::thread::spawn(move || {
            sent.send(worker_gate.handshake(store)).unwrap();
        });
        assert!(
            received
                .recv_timeout(std::time::Duration::from_millis(25))
                .is_err()
        );
        drop(permit);
        let second = received
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        worker.join().unwrap();
        assert!(gate.authorize(store, first).is_err());
        gate.authorize(store, second).unwrap();
    }
}
