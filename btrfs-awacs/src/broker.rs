use crate::btrfs::{
    changed_objects_v2, create_snapshot, destroy_snapshot, filesystem_info, send_changed_objects,
    subvolume_info, FilesystemInfo, SubvolumeInfo, ROOT_INODE, SUBVOL_NAME_MAX,
};
use crate::index::Index;
use crate::store::BrokerJournal;
use crate::tree_index::{read_full_index, read_target_objects};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const BROKER_PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;
pub const MAX_FRAME_FDS: usize = 4;
const FRAME_MAGIC: &[u8; 4] = b"BAWB";
const FRAME_HEADER_SIZE: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Opcode {
    Handshake = 1,
    InspectSubvolume = 2,
    CreateSnapshot = 3,
    ChangedObjects = 4,
    DeleteSnapshot = 5,
    PublishWorktree = 6,
    ReconcileReceipt = 7,
    FullIndex = 8,
    TargetObjectLookup = 9,
}

impl Opcode {
    pub(crate) fn decode(value: u16) -> Result<Self, BrokerError> {
        match value {
            1 => Ok(Self::Handshake),
            2 => Ok(Self::InspectSubvolume),
            3 => Ok(Self::CreateSnapshot),
            4 => Ok(Self::ChangedObjects),
            5 => Ok(Self::DeleteSnapshot),
            6 => Ok(Self::PublishWorktree),
            7 => Ok(Self::ReconcileReceipt),
            8 => Ok(Self::FullIndex),
            9 => Ok(Self::TargetObjectLookup),
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
pub struct ExpectedManagedDirectory {
    pub filesystem_uuid: [u8; 16],
    pub device: u64,
    pub inode: u64,
    pub owner_uid: u32,
    pub mode: u32,
    pub security_context_hash: [u8; 32],
}

impl ExpectedManagedDirectory {
    pub fn from_observed(fd: BorrowedFd<'_>) -> Result<Self, BrokerError> {
        let metadata = fd_metadata(fd)?;
        if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(BrokerError::new(
                "managed snapshot destination fd is not a directory",
            ));
        }
        let filesystem = filesystem_info(fd).map_err(|error| {
            BrokerError::new(format!("inspect destination filesystem: {error}"))
        })?;
        Ok(Self {
            filesystem_uuid: filesystem.fs_uuid,
            device: metadata.st_dev,
            inode: metadata.st_ino,
            owner_uid: metadata.st_uid,
            mode: metadata.st_mode & 0o7777,
            security_context_hash: security_context_hash(fd)?,
        })
    }
}

fn security_context_hash(fd: BorrowedFd<'_>) -> Result<[u8; 32], BrokerError> {
    let mut names = Vec::new();
    let mut complete = false;
    for _ in 0..3 {
        // SAFETY: a NULL buffer with size zero asks only for the required size.
        let required = unsafe { libc::flistxattr(fd.as_raw_fd(), std::ptr::null_mut(), 0) };
        if required < 0 {
            return Err(BrokerError::io("list directory security contexts"));
        }
        names.resize(required as usize, 0);
        // SAFETY: names has exactly the writable capacity supplied to the syscall.
        let read =
            unsafe { libc::flistxattr(fd.as_raw_fd(), names.as_mut_ptr().cast(), names.len()) };
        if read >= 0 {
            names.truncate(read as usize);
            complete = true;
            break;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::ERANGE) {
            return Err(BrokerError::io("read directory security-context names"));
        }
    }
    if !complete {
        return Err(BrokerError::new(
            "directory security contexts changed too frequently to bind",
        ));
    }
    let mut security_names = names
        .split(|byte| *byte == 0)
        .filter(|name| name.starts_with(b"security."))
        .map(Vec::from)
        .collect::<Vec<_>>();
    security_names.sort();
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-directory-security-v1\0");
    for name in security_names {
        let c_name = CString::new(name.clone())
            .map_err(|_| BrokerError::new("security xattr name contains NUL"))?;
        // SAFETY: NULL/zero requests the current value size.
        let required =
            unsafe { libc::fgetxattr(fd.as_raw_fd(), c_name.as_ptr(), std::ptr::null_mut(), 0) };
        if required < 0 {
            return Err(BrokerError::io("size directory security context"));
        }
        let mut value = vec![0_u8; required as usize];
        // SAFETY: value has the writable length passed to fgetxattr.
        let read = unsafe {
            libc::fgetxattr(
                fd.as_raw_fd(),
                c_name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        if read < 0 {
            return Err(BrokerError::io("read directory security context"));
        }
        value.truncate(read as usize);
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name);
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    Ok(hash.finalize().into())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotCreateExecution {
    pub receipt: ReceiptRequest,
    pub source: ExpectedSubvolume,
    pub destination_parent: ExpectedManagedDirectory,
    pub destination_name: Vec<u8>,
    pub readonly: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotCreateResult {
    pub snapshot: ExpectedSubvolume,
    pub result_hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotDeleteExecution {
    pub receipt: ReceiptRequest,
    pub target: ExpectedSubvolume,
    pub destination_parent: ExpectedManagedDirectory,
    pub destination_name: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotDeleteResult {
    pub deleted_subvolume_uuid: [u8; 16],
    pub result_hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedReservation {
    pub name: Vec<u8>,
    pub device: u64,
    pub inode: u64,
    pub owner_uid: u32,
    pub nonce: [u8; 32],
}

impl ExpectedReservation {
    pub fn from_observed(
        destination_parent: BorrowedFd<'_>,
        name: &[u8],
        owner_uid: u32,
        nonce: [u8; 32],
    ) -> Result<Self, BrokerError> {
        let fd = open_file_at(destination_parent, name)?
            .ok_or_else(|| BrokerError::new("worktree reservation is missing"))?;
        let metadata = verify_reservation_file(fd.as_fd(), owner_uid, nonce)?;
        Ok(Self {
            name: name.to_vec(),
            device: metadata.st_dev,
            inode: metadata.st_ino,
            owner_uid,
            nonce,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeRenameExecution {
    pub receipt: ReceiptRequest,
    pub worktree: ExpectedSubvolume,
    pub staging_parent: ExpectedManagedDirectory,
    pub staging_name: Vec<u8>,
    pub destination_parent: ExpectedManagedDirectory,
    /// Immutable policy anchor supplied as a separate fd. The broker resolves
    /// this relative parent itself with openat2(RESOLVE_BENEATH), so a
    /// compromised manager cannot escape the authorized destination root.
    pub destination_root: ExpectedSubvolume,
    pub destination_root_directory: ExpectedManagedDirectory,
    pub destination_relative_parent: Vec<u8>,
    pub destination_name: Vec<u8>,
    pub reservation: ExpectedReservation,
    pub authorization_hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeRenameResult {
    pub worktree_subvolume_uuid: [u8; 16],
    pub result_hash: [u8; 32],
}

pub fn snapshot_target_locator_hash(directory: &ExpectedManagedDirectory, name: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-snapshot-locator-v1\0");
    hash.update(directory.filesystem_uuid);
    hash.update(directory.device.to_be_bytes());
    hash.update(directory.inode.to_be_bytes());
    hash.update((name.len() as u64).to_be_bytes());
    hash.update(name);
    hash.finalize().into()
}

pub fn snapshot_create_effect_hash(execution: &SnapshotCreateExecution) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-snapshot-create-v1\0");
    hash_expected_subvolume(&mut hash, &execution.source);
    hash.update(execution.destination_parent.filesystem_uuid);
    hash.update(execution.destination_parent.device.to_be_bytes());
    hash.update(execution.destination_parent.inode.to_be_bytes());
    hash.update(execution.destination_parent.owner_uid.to_be_bytes());
    hash.update(execution.destination_parent.mode.to_be_bytes());
    hash.update((execution.destination_name.len() as u64).to_be_bytes());
    hash.update(&execution.destination_name);
    hash.update([u8::from(execution.readonly)]);
    hash.finalize().into()
}

pub fn snapshot_delete_effect_hash(execution: &SnapshotDeleteExecution) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-snapshot-delete-v1\0");
    hash_expected_subvolume(&mut hash, &execution.target);
    hash.update(execution.destination_parent.filesystem_uuid);
    hash.update(execution.destination_parent.device.to_be_bytes());
    hash.update(execution.destination_parent.inode.to_be_bytes());
    hash.update(execution.destination_parent.owner_uid.to_be_bytes());
    hash.update(execution.destination_parent.mode.to_be_bytes());
    hash.update((execution.destination_name.len() as u64).to_be_bytes());
    hash.update(&execution.destination_name);
    hash.finalize().into()
}

pub fn worktree_rename_effect_hash(execution: &WorktreeRenameExecution) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-worktree-rename-v1\0");
    hash_expected_subvolume(&mut hash, &execution.worktree);
    hash_directory(&mut hash, &execution.staging_parent);
    hash.update((execution.staging_name.len() as u64).to_be_bytes());
    hash.update(&execution.staging_name);
    hash_directory(&mut hash, &execution.destination_parent);
    hash_expected_subvolume(&mut hash, &execution.destination_root);
    hash_directory(&mut hash, &execution.destination_root_directory);
    hash.update((execution.destination_relative_parent.len() as u64).to_be_bytes());
    hash.update(&execution.destination_relative_parent);
    hash.update((execution.destination_name.len() as u64).to_be_bytes());
    hash.update(&execution.destination_name);
    hash.update((execution.reservation.name.len() as u64).to_be_bytes());
    hash.update(&execution.reservation.name);
    hash.update(execution.reservation.device.to_be_bytes());
    hash.update(execution.reservation.inode.to_be_bytes());
    hash.update(execution.reservation.owner_uid.to_be_bytes());
    hash.update(execution.reservation.nonce);
    hash.update(execution.authorization_hash);
    hash.finalize().into()
}

fn hash_directory(hash: &mut Sha256, directory: &ExpectedManagedDirectory) {
    hash.update(directory.filesystem_uuid);
    hash.update(directory.device.to_be_bytes());
    hash.update(directory.inode.to_be_bytes());
    hash.update(directory.owner_uid.to_be_bytes());
    hash.update(directory.mode.to_be_bytes());
    hash.update(directory.security_context_hash);
}

pub fn execute_snapshot_create(
    gate: &SessionGate,
    journal: &mut BrokerJournal,
    execution: &SnapshotCreateExecution,
    source_fd: BorrowedFd<'_>,
    destination_parent_fd: BorrowedFd<'_>,
) -> Result<SnapshotCreateResult, BrokerError> {
    gate.authorize(
        execution.receipt.manager_store_uuid,
        execution.receipt.manager_session_id,
    )?;
    validate_snapshot_create_request(execution)?;
    // Writable live sources are allowed to advance transaction metadata while
    // a cut is admitted. Authority is the stable root identity, not a promise
    // that ctransid remains frozen before the snapshot transaction.
    verify_subvolume_stable_identity(source_fd, &execution.source)?;
    verify_managed_directory(destination_parent_fd, &execution.destination_parent)?;

    match journal.begin_receipt(&execution.receipt)? {
        BeginReceipt::Existing(receipt) => match receipt.state {
            ReceiptState::Completed => {
                let result =
                    inspect_and_validate_created_snapshot(execution, destination_parent_fd)?
                        .ok_or_else(|| {
                            BrokerError::new("completed snapshot receipt target is missing")
                        })?;
                verify_completed_snapshot_receipt(&receipt, &result)?;
                gate.authorize(
                    execution.receipt.manager_store_uuid,
                    execution.receipt.manager_session_id,
                )?;
                return Ok(result);
            }
            ReceiptState::Running => {
                return Err(BrokerError::new(
                    "snapshot effect for this operation fence is already running",
                ));
            }
            ReceiptState::NeedsReconcile => {
                return reconcile_snapshot_create(
                    gate,
                    journal,
                    execution,
                    receipt.id,
                    destination_parent_fd,
                );
            }
            ReceiptState::FailedBeforeEffect => {
                return Err(BrokerError::new(
                    "snapshot effect previously failed before taking effect",
                ));
            }
        },
        BeginReceipt::Started(_) => {}
    }

    match inspect_snapshot_at(destination_parent_fd, &execution.destination_name) {
        Ok(None) => {}
        Ok(Some(_)) => {
            journal.fail_before_effect(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
                current_unix_time_ns()?,
            )?;
            return Err(BrokerError::new(
                "snapshot destination already existed when its receipt started",
            ));
        }
        Err(error) => {
            journal.fail_before_effect(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
                current_unix_time_ns()?,
            )?;
            return Err(BrokerError::new(format!(
                "snapshot destination could not be inspected before effect: {error}"
            )));
        }
    }

    let create_error = create_snapshot(
        source_fd,
        destination_parent_fd,
        &execution.destination_name,
        execution.readonly,
    )
    .err();
    let observed = inspect_and_validate_created_snapshot(execution, destination_parent_fd);
    let result = match observed {
        Ok(Some(result)) => result,
        Ok(None) if create_error.is_some() => {
            journal.fail_before_effect(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
                current_unix_time_ns()?,
            )?;
            return Err(BrokerError::new(format!(
                "snapshot ioctl failed before creating a target: {}",
                create_error.expect("matched error")
            )));
        }
        Ok(None) => {
            journal.mark_needs_reconcile(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
            )?;
            return Err(BrokerError::new(
                "snapshot ioctl succeeded but its target cannot be found",
            ));
        }
        Err(error) => {
            journal.mark_needs_reconcile(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
            )?;
            return Err(BrokerError::new(format!(
                "snapshot target requires reconciliation: {error}"
            )));
        }
    };
    // A matching object at the protected intent path is adoptable even if the
    // ioctl reported an error (for example, recovery raced a prior successful
    // invocation). Exact UUID/parent/flags validation above makes adoption
    // safe; the error is relevant only when no target exists.
    sync_filesystem(destination_parent_fd)?;
    let completed = journal.complete_receipt(
        execution.receipt.id,
        execution.receipt.manager_session_id,
        execution.receipt.request_hash(),
        result.snapshot.subvolume_uuid,
        result.result_hash,
        current_unix_time_ns()?,
    )?;
    verify_completed_snapshot_receipt(&completed, &result)?;
    gate.authorize(
        execution.receipt.manager_store_uuid,
        execution.receipt.manager_session_id,
    )?;
    Ok(result)
}

fn reconcile_snapshot_create(
    gate: &SessionGate,
    journal: &mut BrokerJournal,
    execution: &SnapshotCreateExecution,
    receipt_id: [u8; 16],
    destination_parent_fd: BorrowedFd<'_>,
) -> Result<SnapshotCreateResult, BrokerError> {
    let Some(result) = inspect_and_validate_created_snapshot(execution, destination_parent_fd)?
    else {
        journal.reconcile_failed_before_effect(
            receipt_id,
            execution.receipt.request_hash(),
            current_unix_time_ns()?,
        )?;
        return Err(BrokerError::new(
            "reconciled snapshot effect did not create a target",
        ));
    };
    sync_filesystem(destination_parent_fd)?;
    let receipt = journal.reconcile_completed(
        receipt_id,
        execution.receipt.request_hash(),
        result.snapshot.subvolume_uuid,
        result.result_hash,
        current_unix_time_ns()?,
    )?;
    verify_completed_snapshot_receipt(&receipt, &result)?;
    gate.authorize(
        execution.receipt.manager_store_uuid,
        execution.receipt.manager_session_id,
    )?;
    Ok(result)
}

fn validate_snapshot_create_request(
    execution: &SnapshotCreateExecution,
) -> Result<(), BrokerError> {
    validate_child_name(&execution.destination_name)?;
    if execution.receipt.effect_kind != EffectKind::SnapshotCreate {
        return Err(BrokerError::new(
            "snapshot executor received a non-snapshot receipt",
        ));
    }
    if execution.source.filesystem_uuid != execution.destination_parent.filesystem_uuid
        || execution.receipt.filesystem_uuid != execution.source.filesystem_uuid
    {
        return Err(BrokerError::new(
            "snapshot source, destination, and receipt must name one filesystem",
        ));
    }
    let locator =
        snapshot_target_locator_hash(&execution.destination_parent, &execution.destination_name);
    if execution.receipt.target_locator_hash != locator
        || execution.receipt.effect_arguments_hash != snapshot_create_effect_hash(execution)
    {
        return Err(BrokerError::new(
            "snapshot receipt hashes do not bind the supplied effect arguments",
        ));
    }
    Ok(())
}

fn verify_managed_directory(
    fd: BorrowedFd<'_>,
    expected: &ExpectedManagedDirectory,
) -> Result<(), BrokerError> {
    let observed = ExpectedManagedDirectory::from_observed(fd)?;
    if observed != *expected
        || observed.filesystem_uuid != expected.filesystem_uuid
        || observed.mode & 0o077 != 0
    {
        return Err(BrokerError::new(
            "snapshot destination is not the expected private manager directory",
        ));
    }
    Ok(())
}

fn inspect_and_validate_created_snapshot(
    execution: &SnapshotCreateExecution,
    destination_parent_fd: BorrowedFd<'_>,
) -> Result<Option<SnapshotCreateResult>, BrokerError> {
    let Some(snapshot) = observe_snapshot_at(destination_parent_fd, &execution.destination_name)?
    else {
        return Ok(None);
    };
    if snapshot.filesystem_uuid != execution.source.filesystem_uuid
        || snapshot.subvolume_uuid == execution.source.subvolume_uuid
        || snapshot.parent_uuid != Some(execution.source.subvolume_uuid)
        || snapshot.received_uuid.is_some()
        || snapshot.readonly != execution.readonly
    {
        return Err(BrokerError::new(
            "created snapshot does not match source UUID, filesystem, or requested flags",
        ));
    }
    let result_hash = snapshot_create_result_hash(execution, &snapshot);
    Ok(Some(SnapshotCreateResult {
        snapshot,
        result_hash,
    }))
}

fn observe_snapshot_at(
    destination_parent_fd: BorrowedFd<'_>,
    destination_name: &[u8],
) -> Result<Option<ExpectedSubvolume>, BrokerError> {
    let Some(fd) = inspect_snapshot_at(destination_parent_fd, destination_name)? else {
        return Ok(None);
    };
    let metadata = fd_metadata(fd.as_fd())?;
    if metadata.st_ino != ROOT_INODE || metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(BrokerError::new(
            "snapshot target is not a subvolume root directory",
        ));
    }
    let filesystem = filesystem_info(fd.as_fd())
        .map_err(|error| BrokerError::new(format!("inspect target filesystem: {error}")))?;
    let subvolume = subvolume_info(fd.as_fd())
        .map_err(|error| BrokerError::new(format!("inspect target snapshot: {error}")))?;
    Ok(Some(ExpectedSubvolume::from_observed(
        &filesystem,
        &subvolume,
    )))
}

fn inspect_snapshot_at(
    parent: BorrowedFd<'_>,
    name: &[u8],
) -> Result<Option<OwnedFd>, BrokerError> {
    validate_child_name(name)?;
    let name = CString::new(name).map_err(|_| BrokerError::new("snapshot name contains NUL"))?;
    // SAFETY: parent remains open, name is NUL terminated, and a successful
    // descriptor is transferred immediately into OwnedFd.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        // SAFETY: openat returned one newly owned descriptor.
        return Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(None)
    } else {
        Err(BrokerError::new(format!(
            "open snapshot target beneath manager directory: {error}"
        )))
    }
}

fn open_file_at(parent: BorrowedFd<'_>, name: &[u8]) -> Result<Option<OwnedFd>, BrokerError> {
    validate_child_name(name)?;
    let name = CString::new(name).map_err(|_| BrokerError::new("child name contains NUL"))?;
    // SAFETY: parent and the NUL-terminated name stay live for openat; a
    // successful descriptor is transferred exactly once into OwnedFd.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        // SAFETY: openat returned a new owned descriptor.
        return Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(None)
    } else {
        Err(BrokerError::new(format!(
            "open child beneath destination directory: {error}"
        )))
    }
}

fn verify_reservation_file(
    fd: BorrowedFd<'_>,
    owner_uid: u32,
    nonce: [u8; 32],
) -> Result<libc::stat, BrokerError> {
    let metadata = fd_metadata(fd)?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_mode & 0o7777 != 0o600
        || metadata.st_nlink != 1
        || metadata.st_uid != owner_uid
        || metadata.st_size != nonce.len() as i64
    {
        return Err(BrokerError::new(
            "worktree reservation must be one owner-only regular file containing its nonce",
        ));
    }
    let mut observed = [0_u8; 32];
    // SAFETY: pread writes exactly at most the live output array length.
    let read = unsafe {
        libc::pread(
            fd.as_raw_fd(),
            observed.as_mut_ptr().cast(),
            observed.len(),
            0,
        )
    };
    if read != observed.len() as isize || observed != nonce {
        return Err(BrokerError::new(
            "worktree reservation nonce does not match its operation",
        ));
    }
    Ok(metadata)
}

fn verify_reservation(
    parent: BorrowedFd<'_>,
    expected: &ExpectedReservation,
) -> Result<bool, BrokerError> {
    let Some(fd) = open_file_at(parent, &expected.name)? else {
        return Ok(false);
    };
    let metadata = verify_reservation_file(fd.as_fd(), expected.owner_uid, expected.nonce)?;
    if metadata.st_dev != expected.device || metadata.st_ino != expected.inode {
        return Err(BrokerError::new(
            "worktree reservation inode no longer matches the admitted capability",
        ));
    }
    Ok(true)
}

fn remove_reservation(
    parent: BorrowedFd<'_>,
    expected: &ExpectedReservation,
) -> Result<(), BrokerError> {
    if !verify_reservation(parent, expected)? {
        return Ok(());
    }
    let name = CString::new(expected.name.as_slice())
        .map_err(|_| BrokerError::new("reservation name contains NUL"))?;
    // SAFETY: parent and name remain live; flags zero removes a non-directory.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(BrokerError::io("remove worktree reservation"));
    }
    Ok(())
}

fn validate_child_name(name: &[u8]) -> Result<(), BrokerError> {
    if name.is_empty()
        || name.len() > SUBVOL_NAME_MAX
        || name.contains(&b'/')
        || name.contains(&b'\0')
        || name == b"."
        || name == b".."
    {
        return Err(BrokerError::new(
            "snapshot destination must be one safe nonempty basename",
        ));
    }
    Ok(())
}

fn snapshot_create_result_hash(
    execution: &SnapshotCreateExecution,
    snapshot: &ExpectedSubvolume,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-snapshot-result-v1\0");
    hash.update(execution.receipt.target_locator_hash);
    hash_expected_subvolume(&mut hash, snapshot);
    hash.finalize().into()
}

fn hash_expected_subvolume(hash: &mut Sha256, value: &ExpectedSubvolume) {
    hash.update(value.filesystem_uuid);
    hash.update(value.subvolume_uuid);
    hash.update(value.root_id.to_be_bytes());
    hash.update(value.generation.to_be_bytes());
    hash.update(value.ctransid.to_be_bytes());
    hash.update(value.otransid.to_be_bytes());
    hash_optional_uuid(hash, value.parent_uuid);
    hash_optional_uuid(hash, value.received_uuid);
    hash.update([u8::from(value.readonly)]);
}

fn hash_optional_uuid(hash: &mut Sha256, value: Option<[u8; 16]>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash.update(value);
        }
        None => hash.update([0]),
    }
}

fn verify_completed_snapshot_receipt(
    receipt: &Receipt,
    result: &SnapshotCreateResult,
) -> Result<(), BrokerError> {
    if receipt.state != ReceiptState::Completed
        || receipt.target_subvol_uuid != Some(result.snapshot.subvolume_uuid)
        || receipt.result_hash != Some(result.result_hash)
    {
        return Err(BrokerError::new(
            "completed snapshot receipt does not match the observed result",
        ));
    }
    Ok(())
}

fn sync_filesystem(fd: BorrowedFd<'_>) -> Result<(), BrokerError> {
    // SAFETY: syncfs operates on the filesystem containing this live fd.
    if unsafe { libc::syncfs(fd.as_raw_fd()) } != 0 {
        return Err(BrokerError::io("commit snapshot filesystem"));
    }
    Ok(())
}

fn current_unix_time_ns() -> Result<i64, BrokerError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| BrokerError::new(format!("read broker clock: {error}")))?;
    i64::try_from(elapsed.as_nanos())
        .map_err(|_| BrokerError::new("broker timestamp exceeds signed nanoseconds"))
}

pub fn execute_snapshot_delete(
    gate: &SessionGate,
    journal: &mut BrokerJournal,
    execution: &SnapshotDeleteExecution,
    destination_parent_fd: BorrowedFd<'_>,
) -> Result<SnapshotDeleteResult, BrokerError> {
    gate.authorize(
        execution.receipt.manager_store_uuid,
        execution.receipt.manager_session_id,
    )?;
    validate_snapshot_delete_request(execution)?;
    verify_managed_directory(destination_parent_fd, &execution.destination_parent)?;

    match journal.begin_receipt(&execution.receipt)? {
        BeginReceipt::Existing(receipt) => match receipt.state {
            ReceiptState::Completed => {
                if observe_snapshot_at(destination_parent_fd, &execution.destination_name)?
                    .is_some()
                {
                    return Err(BrokerError::new(
                        "completed deletion receipt target is present",
                    ));
                }
                let result = snapshot_delete_result(execution);
                verify_completed_delete_receipt(&receipt, &result)?;
                gate.authorize(
                    execution.receipt.manager_store_uuid,
                    execution.receipt.manager_session_id,
                )?;
                return Ok(result);
            }
            ReceiptState::Running => {
                return Err(BrokerError::new(
                    "snapshot deletion for this operation fence is already running",
                ));
            }
            ReceiptState::NeedsReconcile => {
                return reconcile_snapshot_delete(
                    gate,
                    journal,
                    execution,
                    receipt.id,
                    destination_parent_fd,
                );
            }
            ReceiptState::FailedBeforeEffect => {
                return Err(BrokerError::new(
                    "snapshot deletion previously failed before taking effect",
                ));
            }
        },
        BeginReceipt::Started(_) => {}
    }

    match observe_snapshot_at(destination_parent_fd, &execution.destination_name) {
        Ok(Some(observed)) if observed == execution.target => {}
        Ok(Some(_)) => {
            journal.fail_before_effect(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
                current_unix_time_ns()?,
            )?;
            return Err(BrokerError::new(
                "snapshot deletion target does not match the receipt identity",
            ));
        }
        Ok(None) => {
            journal.fail_before_effect(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
                current_unix_time_ns()?,
            )?;
            return Err(BrokerError::new(
                "snapshot deletion target was absent when its receipt started",
            ));
        }
        Err(error) => {
            journal.fail_before_effect(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
                current_unix_time_ns()?,
            )?;
            return Err(BrokerError::new(format!(
                "snapshot deletion target could not be inspected before effect: {error}"
            )));
        }
    }

    let delete_error = destroy_snapshot(destination_parent_fd, &execution.destination_name).err();
    match observe_snapshot_at(destination_parent_fd, &execution.destination_name) {
        Ok(None) => {
            sync_filesystem(destination_parent_fd)?;
            let result = snapshot_delete_result(execution);
            let receipt = journal.complete_receipt(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
                execution.target.subvolume_uuid,
                result.result_hash,
                current_unix_time_ns()?,
            )?;
            verify_completed_delete_receipt(&receipt, &result)?;
            gate.authorize(
                execution.receipt.manager_store_uuid,
                execution.receipt.manager_session_id,
            )?;
            Ok(result)
        }
        Ok(Some(observed)) if observed == execution.target && delete_error.is_some() => {
            journal.fail_before_effect(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
                current_unix_time_ns()?,
            )?;
            Err(BrokerError::new(format!(
                "snapshot deletion failed before taking effect: {}",
                delete_error.expect("matched error")
            )))
        }
        Ok(Some(_)) => {
            journal.mark_needs_reconcile(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
            )?;
            Err(BrokerError::new(
                "snapshot deletion returned without proving the exact target absent",
            ))
        }
        Err(error) => {
            journal.mark_needs_reconcile(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
            )?;
            Err(BrokerError::new(format!(
                "snapshot deletion requires reconciliation: {error}"
            )))
        }
    }
}

fn reconcile_snapshot_delete(
    gate: &SessionGate,
    journal: &mut BrokerJournal,
    execution: &SnapshotDeleteExecution,
    receipt_id: [u8; 16],
    destination_parent_fd: BorrowedFd<'_>,
) -> Result<SnapshotDeleteResult, BrokerError> {
    match observe_snapshot_at(destination_parent_fd, &execution.destination_name)? {
        None => {
            sync_filesystem(destination_parent_fd)?;
            let result = snapshot_delete_result(execution);
            let receipt = journal.reconcile_completed(
                receipt_id,
                execution.receipt.request_hash(),
                execution.target.subvolume_uuid,
                result.result_hash,
                current_unix_time_ns()?,
            )?;
            verify_completed_delete_receipt(&receipt, &result)?;
            gate.authorize(
                execution.receipt.manager_store_uuid,
                execution.receipt.manager_session_id,
            )?;
            Ok(result)
        }
        Some(observed) if observed == execution.target => {
            journal.reconcile_failed_before_effect(
                receipt_id,
                execution.receipt.request_hash(),
                current_unix_time_ns()?,
            )?;
            Err(BrokerError::new(
                "reconciled snapshot deletion did not take effect",
            ))
        }
        Some(_) => Err(BrokerError::new(
            "snapshot deletion path was replaced and cannot be reconciled",
        )),
    }
}

fn validate_snapshot_delete_request(
    execution: &SnapshotDeleteExecution,
) -> Result<(), BrokerError> {
    validate_child_name(&execution.destination_name)?;
    if execution.receipt.effect_kind != EffectKind::SnapshotDelete {
        return Err(BrokerError::new(
            "snapshot deletion executor received the wrong receipt kind",
        ));
    }
    if !execution.target.readonly {
        return Err(BrokerError::new(
            "managed snapshot deletion requires a read-only target",
        ));
    }
    if execution.target.filesystem_uuid != execution.destination_parent.filesystem_uuid
        || execution.receipt.filesystem_uuid != execution.target.filesystem_uuid
    {
        return Err(BrokerError::new(
            "snapshot deletion target, parent, and receipt must name one filesystem",
        ));
    }
    let locator =
        snapshot_target_locator_hash(&execution.destination_parent, &execution.destination_name);
    if execution.receipt.target_locator_hash != locator
        || execution.receipt.effect_arguments_hash != snapshot_delete_effect_hash(execution)
    {
        return Err(BrokerError::new(
            "snapshot deletion receipt hashes do not bind the supplied effect arguments",
        ));
    }
    Ok(())
}

fn snapshot_delete_result(execution: &SnapshotDeleteExecution) -> SnapshotDeleteResult {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-snapshot-deleted-v1\0");
    hash.update(execution.receipt.target_locator_hash);
    hash.update(execution.target.subvolume_uuid);
    SnapshotDeleteResult {
        deleted_subvolume_uuid: execution.target.subvolume_uuid,
        result_hash: hash.finalize().into(),
    }
}

fn verify_completed_delete_receipt(
    receipt: &Receipt,
    result: &SnapshotDeleteResult,
) -> Result<(), BrokerError> {
    if receipt.state != ReceiptState::Completed
        || receipt.target_subvol_uuid != Some(result.deleted_subvolume_uuid)
        || receipt.result_hash != Some(result.result_hash)
    {
        return Err(BrokerError::new(
            "completed deletion receipt does not match the expected result",
        ));
    }
    Ok(())
}

pub fn execute_worktree_rename(
    gate: &SessionGate,
    journal: &mut BrokerJournal,
    execution: &WorktreeRenameExecution,
    staging_parent_fd: BorrowedFd<'_>,
    destination_root_fd: BorrowedFd<'_>,
) -> Result<WorktreeRenameResult, BrokerError> {
    gate.authorize(
        execution.receipt.manager_store_uuid,
        execution.receipt.manager_session_id,
    )?;
    validate_worktree_rename_request(execution)?;
    verify_managed_directory(staging_parent_fd, &execution.staging_parent)?;
    verify_subvolume_stable_identity(destination_root_fd, &execution.destination_root)?;
    if ExpectedManagedDirectory::from_observed(destination_root_fd)?
        != execution.destination_root_directory
    {
        return Err(BrokerError::new(
            "Worktree destination policy-root security context changed",
        ));
    }
    reject_idmapped_mount(destination_root_fd)?;
    let destination_parent =
        open_directory_beneath(destination_root_fd, &execution.destination_relative_parent)?;
    let destination_parent_fd = destination_parent.as_fd();
    verify_managed_directory(destination_parent_fd, &execution.destination_parent)?;

    match journal.begin_receipt(&execution.receipt)? {
        BeginReceipt::Existing(receipt) => match receipt.state {
            ReceiptState::Completed => {
                verify_published_worktree(execution, staging_parent_fd, destination_parent_fd)?;
                if verify_reservation(destination_parent_fd, &execution.reservation)? {
                    return Err(BrokerError::new(
                        "completed worktree receipt still has its reservation",
                    ));
                }
                let result = worktree_rename_result(execution);
                verify_completed_worktree_receipt(&receipt, &result)?;
                gate.authorize(
                    execution.receipt.manager_store_uuid,
                    execution.receipt.manager_session_id,
                )?;
                return Ok(result);
            }
            ReceiptState::Running => {
                return Err(BrokerError::new(
                    "worktree publication for this operation fence is already running",
                ));
            }
            ReceiptState::NeedsReconcile => {
                return reconcile_worktree_rename(
                    gate,
                    journal,
                    execution,
                    receipt.id,
                    staging_parent_fd,
                    destination_parent_fd,
                );
            }
            ReceiptState::FailedBeforeEffect => {
                return Err(BrokerError::new(
                    "worktree publication previously failed before taking effect",
                ));
            }
        },
        BeginReceipt::Started(_) => {}
    }

    let preflight = (|| -> Result<(), BrokerError> {
        match observe_snapshot_at(staging_parent_fd, &execution.staging_name)? {
            Some(observed) if observed == execution.worktree => {}
            Some(_) => {
                return Err(BrokerError::new(
                    "staged worktree does not match the receipt identity",
                ));
            }
            None => return Err(BrokerError::new("staged worktree is missing")),
        }
        if observe_snapshot_at(destination_parent_fd, &execution.destination_name)?.is_some() {
            return Err(BrokerError::new("worktree destination already exists"));
        }
        if !verify_reservation(destination_parent_fd, &execution.reservation)? {
            return Err(BrokerError::new("worktree reservation is missing"));
        }
        Ok(())
    })();
    if let Err(error) = preflight {
        journal.fail_before_effect(
            execution.receipt.id,
            execution.receipt.manager_session_id,
            execution.receipt.request_hash(),
            current_unix_time_ns()?,
        )?;
        return Err(error);
    }

    let rename_error = rename_noreplace(
        staging_parent_fd,
        &execution.staging_name,
        destination_parent_fd,
        &execution.destination_name,
    )
    .err();
    let staging = observe_snapshot_at(staging_parent_fd, &execution.staging_name);
    let destination = observe_snapshot_at(destination_parent_fd, &execution.destination_name);
    match (staging, destination) {
        (Ok(None), Ok(Some(observed))) if observed == execution.worktree => {
            if let Err(error) = remove_reservation(destination_parent_fd, &execution.reservation) {
                journal.mark_needs_reconcile(
                    execution.receipt.id,
                    execution.receipt.manager_session_id,
                    execution.receipt.request_hash(),
                )?;
                return Err(BrokerError::new(format!(
                    "published worktree reservation requires reconciliation: {error}"
                )));
            }
            sync_filesystem(destination_parent_fd)?;
            let result = worktree_rename_result(execution);
            let receipt = journal.complete_receipt(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
                execution.worktree.subvolume_uuid,
                result.result_hash,
                current_unix_time_ns()?,
            )?;
            verify_completed_worktree_receipt(&receipt, &result)?;
            gate.authorize(
                execution.receipt.manager_store_uuid,
                execution.receipt.manager_session_id,
            )?;
            Ok(result)
        }
        (Ok(Some(staged)), Ok(None)) if staged == execution.worktree && rename_error.is_some() => {
            journal.fail_before_effect(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
                current_unix_time_ns()?,
            )?;
            Err(BrokerError::new(format!(
                "worktree rename failed before taking effect: {}",
                rename_error.expect("matched error")
            )))
        }
        (staging, destination) => {
            journal.mark_needs_reconcile(
                execution.receipt.id,
                execution.receipt.manager_session_id,
                execution.receipt.request_hash(),
            )?;
            Err(BrokerError::new(format!(
                "worktree rename requires reconciliation: staging={staging:?}, destination={destination:?}"
            )))
        }
    }
}

fn reconcile_worktree_rename(
    gate: &SessionGate,
    journal: &mut BrokerJournal,
    execution: &WorktreeRenameExecution,
    receipt_id: [u8; 16],
    staging_parent_fd: BorrowedFd<'_>,
    destination_parent_fd: BorrowedFd<'_>,
) -> Result<WorktreeRenameResult, BrokerError> {
    let staging = observe_snapshot_at(staging_parent_fd, &execution.staging_name)?;
    let destination = observe_snapshot_at(destination_parent_fd, &execution.destination_name)?;
    match (staging, destination) {
        (None, Some(observed)) if observed == execution.worktree => {
            remove_reservation(destination_parent_fd, &execution.reservation)?;
            sync_filesystem(destination_parent_fd)?;
            let result = worktree_rename_result(execution);
            let receipt = journal.reconcile_completed(
                receipt_id,
                execution.receipt.request_hash(),
                execution.worktree.subvolume_uuid,
                result.result_hash,
                current_unix_time_ns()?,
            )?;
            verify_completed_worktree_receipt(&receipt, &result)?;
            gate.authorize(
                execution.receipt.manager_store_uuid,
                execution.receipt.manager_session_id,
            )?;
            Ok(result)
        }
        (Some(staged), None) if staged == execution.worktree => {
            journal.reconcile_failed_before_effect(
                receipt_id,
                execution.receipt.request_hash(),
                current_unix_time_ns()?,
            )?;
            Err(BrokerError::new(
                "reconciled worktree publication did not take effect",
            ))
        }
        _ => Err(BrokerError::new(
            "worktree publication state is ambiguous and cannot be reconciled",
        )),
    }
}

fn validate_worktree_rename_request(
    execution: &WorktreeRenameExecution,
) -> Result<(), BrokerError> {
    validate_child_name(&execution.staging_name)?;
    validate_child_name(&execution.destination_name)?;
    validate_child_name(&execution.reservation.name)?;
    if execution.destination_name == execution.reservation.name {
        return Err(BrokerError::new(
            "worktree destination and reservation names must differ",
        ));
    }
    if execution.receipt.effect_kind != EffectKind::WorktreeRename {
        return Err(BrokerError::new(
            "worktree executor received the wrong receipt kind",
        ));
    }
    if execution.worktree.readonly {
        return Err(BrokerError::new(
            "published worktree snapshot must be writable",
        ));
    }
    if execution.worktree.filesystem_uuid != execution.staging_parent.filesystem_uuid
        || execution.worktree.filesystem_uuid != execution.destination_parent.filesystem_uuid
        || execution.worktree.filesystem_uuid != execution.destination_root.filesystem_uuid
        || execution.receipt.filesystem_uuid != execution.worktree.filesystem_uuid
    {
        return Err(BrokerError::new(
            "worktree, staging, destination, and receipt must name one filesystem",
        ));
    }
    if execution.destination_relative_parent.len() > libc::PATH_MAX as usize {
        return Err(BrokerError::new(
            "Worktree relative destination path exceeds PATH_MAX",
        ));
    }
    let locator =
        snapshot_target_locator_hash(&execution.destination_parent, &execution.destination_name);
    if execution.receipt.target_locator_hash != locator
        || execution.receipt.effect_arguments_hash != worktree_rename_effect_hash(execution)
    {
        return Err(BrokerError::new(
            "worktree receipt hashes do not bind the supplied effect arguments",
        ));
    }
    Ok(())
}

fn verify_published_worktree(
    execution: &WorktreeRenameExecution,
    staging_parent_fd: BorrowedFd<'_>,
    destination_parent_fd: BorrowedFd<'_>,
) -> Result<(), BrokerError> {
    if observe_snapshot_at(staging_parent_fd, &execution.staging_name)?.is_some() {
        return Err(BrokerError::new(
            "completed worktree remains at its staging path",
        ));
    }
    if observe_snapshot_at(destination_parent_fd, &execution.destination_name)?
        != Some(execution.worktree.clone())
    {
        return Err(BrokerError::new(
            "published worktree does not match its destination identity",
        ));
    }
    Ok(())
}

fn rename_noreplace(
    source_parent: BorrowedFd<'_>,
    source_name: &[u8],
    destination_parent: BorrowedFd<'_>,
    destination_name: &[u8],
) -> Result<(), BrokerError> {
    let source = CString::new(source_name)
        .map_err(|_| BrokerError::new("worktree staging name contains NUL"))?;
    let destination = CString::new(destination_name)
        .map_err(|_| BrokerError::new("worktree destination name contains NUL"))?;
    // SAFETY: both directory fds and NUL-terminated names remain live for the
    // syscall; RENAME_NOREPLACE makes destination publication atomic.
    if unsafe {
        libc::renameat2(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(BrokerError::io("publish worktree with renameat2"));
    }
    Ok(())
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[repr(C)]
struct MountIdRequest {
    size: u32,
    spare: u32,
    mount_id: u64,
    parameters: u64,
}

#[repr(C)]
struct StatMountBasic {
    size: u32,
    spare: u32,
    mask: u64,
    sb_dev_major: u32,
    sb_dev_minor: u32,
    sb_magic: u64,
    sb_flags: u32,
    fs_type: u32,
    mount_id: u64,
    parent_mount_id: u64,
    old_mount_id: u32,
    old_parent_mount_id: u32,
    mount_attributes: u64,
    mount_propagation: u64,
    mount_peer_group: u64,
    mount_master: u64,
    propagate_from: u64,
    mount_root: u32,
    mount_point: u32,
}

fn reject_idmapped_mount(fd: BorrowedFd<'_>) -> Result<(), BrokerError> {
    // Request the non-recycled mount ID needed by statmount(2).
    // SAFETY: statx is zeroed and all pointer/size arguments are valid.
    let mut statx: libc::statx = unsafe { zeroed() };
    let empty = c"";
    let status = unsafe {
        libc::statx(
            fd.as_raw_fd(),
            empty.as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_STATX_SYNC_AS_STAT,
            0x4000, // STATX_MNT_ID_UNIQUE
            &mut statx,
        )
    };
    if status != 0 || statx.stx_mask & 0x4000 == 0 {
        return Err(BrokerError::new(
            "kernel cannot prove the Worktree policy root mount identity",
        ));
    }
    let request = MountIdRequest {
        size: size_of::<MountIdRequest>() as u32,
        spare: 0,
        mount_id: statx.stx_mnt_id,
        parameters: 0x0000_0002, // STATMOUNT_MNT_BASIC
    };
    // The fixed prefix through mount_attributes is sufficient for this check.
    // SAFETY: output is writable and both sizes match the C ABI definitions.
    let mut output: StatMountBasic = unsafe { zeroed() };
    let result = unsafe {
        libc::syscall(
            i64::from(linux_raw_sys::general::__NR_statmount),
            &request,
            &mut output,
            size_of::<StatMountBasic>(),
            0,
        )
    };
    if result != 0 || output.mask & 0x0000_0002 == 0 {
        return Err(BrokerError::io("inspect Worktree mount attributes"));
    }
    if output.mount_attributes & 0x0010_0000 != 0 {
        return Err(BrokerError::new(
            "idmapped Worktree destinations are not supported by v1 policy",
        ));
    }
    Ok(())
}

fn open_directory_beneath(root: BorrowedFd<'_>, relative: &[u8]) -> Result<OwnedFd, BrokerError> {
    if relative.is_empty() {
        // SAFETY: fcntl duplicates the live descriptor and returns new ownership.
        let fd = unsafe { libc::fcntl(root.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(BrokerError::io("duplicate Worktree policy root"));
        }
        // SAFETY: fd is the new descriptor returned above.
        return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    if relative.starts_with(b"/")
        || relative
            .split(|byte| *byte == b'/')
            .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err(BrokerError::new(
            "Worktree destination relative path is not normalized",
        ));
    }
    let path = CString::new(relative)
        .map_err(|_| BrokerError::new("Worktree relative path contains NUL"))?;
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        // RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS |
        // RESOLVE_BENEATH. Destination policies do not cross mounts or links.
        resolve: 0x01 | 0x02 | 0x04 | 0x08,
    };
    // SAFETY: root, path, and open_how remain valid for this syscall.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            path.as_ptr(),
            &how,
            size_of::<OpenHow>(),
        ) as i32
    };
    if fd < 0 {
        return Err(BrokerError::io(
            "resolve Worktree destination beneath policy root",
        ));
    }
    // SAFETY: fd is the new descriptor returned by openat2.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn worktree_rename_result(execution: &WorktreeRenameExecution) -> WorktreeRenameResult {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-worktree-published-v1\0");
    hash.update(execution.receipt.target_locator_hash);
    hash.update(execution.worktree.subvolume_uuid);
    WorktreeRenameResult {
        worktree_subvolume_uuid: execution.worktree.subvolume_uuid,
        result_hash: hash.finalize().into(),
    }
}

fn verify_completed_worktree_receipt(
    receipt: &Receipt,
    result: &WorktreeRenameResult,
) -> Result<(), BrokerError> {
    if receipt.state != ReceiptState::Completed
        || receipt.target_subvol_uuid != Some(result.worktree_subvolume_uuid)
        || receipt.result_hash != Some(result.result_hash)
    {
        return Err(BrokerError::new(
            "completed worktree receipt does not match the published result",
        ));
    }
    Ok(())
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
}

pub const MAX_CHANGED_OBJECT_OUTPUT: u64 = 1024 * 1024 * 1024;

pub fn execute_full_index(
    expected: &ExpectedSubvolume,
    snapshot_fd: BorrowedFd<'_>,
) -> Result<Index, BrokerError> {
    if !expected.readonly {
        return Err(BrokerError::new(
            "full-index endpoint must be a read-only snapshot",
        ));
    }
    let before = verify_subvolume(snapshot_fd, expected)?;
    let index = read_full_index(snapshot_fd)
        .map_err(|error| BrokerError::new(format!("read full Btrfs index: {error}")))?;
    let after = verify_subvolume(snapshot_fd, expected)?;
    if after != before {
        return Err(BrokerError::new(
            "full-index snapshot metadata changed during tree search",
        ));
    }
    Ok(index)
}

pub fn execute_target_object_lookup(
    expected: &ExpectedSubvolume,
    snapshot_fd: BorrowedFd<'_>,
    inodes: &std::collections::BTreeSet<u64>,
) -> Result<BTreeMap<u64, crate::index::Object>, BrokerError> {
    if !expected.readonly {
        return Err(BrokerError::new(
            "target-object endpoint must be a read-only snapshot",
        ));
    }
    let before = verify_subvolume(snapshot_fd, expected)?;
    let objects = read_target_objects(snapshot_fd, inodes)
        .map_err(|error| BrokerError::new(format!("read target Btrfs objects: {error}")))?;
    let after = verify_subvolume(snapshot_fd, expected)?;
    if after != before {
        return Err(BrokerError::new(
            "target-object snapshot metadata changed during tree search",
        ));
    }
    Ok(objects)
}

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

    match changed_objects_v2(
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
        }
        Err(error) => {
            return Err(BrokerError::new(format!(
                "run fd-anchored changed-object ioctl: {error}"
            )))
        }
    }
    // A successful return must be durable before the manager can promote the
    // manifest from its fence-specific .part name.
    if unsafe { libc::fsync(output_fd.as_raw_fd()) } != 0 {
        return Err(BrokerError::io("fsync changed-object manifest"));
    }
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
    let manifest_hash = hash_fd(output_fd, output_bytes)?;
    Ok(ChangedObjectsResult {
        output_bytes,
        manifest_hash,
    })
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

fn verify_subvolume_stable_identity(
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
    if filesystem.fs_uuid != expected.filesystem_uuid
        || subvolume.uuid != expected.subvolume_uuid
        || subvolume.root_id != expected.root_id
        || subvolume.parent_uuid != expected.parent_uuid
        || subvolume.received_uuid != expected.received_uuid
        || subvolume.readonly() != expected.readonly
    {
        return Err(BrokerError::new(
            "broker subvolume fd does not match the expected stable identity",
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectKind {
    SnapshotCreate,
    WorktreeRename,
    SnapshotDelete,
}

impl EffectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::SnapshotCreate => "snapshot-create",
            Self::WorktreeRename => "worktree-rename",
            Self::SnapshotDelete => "snapshot-delete",
        }
    }

    fn parse(value: &str) -> Result<Self, BrokerError> {
        match value {
            "snapshot-create" => Ok(Self::SnapshotCreate),
            "worktree-rename" => Ok(Self::WorktreeRename),
            "snapshot-delete" => Ok(Self::SnapshotDelete),
            _ => Err(BrokerError::new(format!(
                "unknown broker effect kind {value:?}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptState {
    Running,
    Completed,
    FailedBeforeEffect,
    NeedsReconcile,
}

impl ReceiptState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::FailedBeforeEffect => "failed-before-effect",
            Self::NeedsReconcile => "needs-reconcile",
        }
    }

    fn parse(value: &str) -> Result<Self, BrokerError> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed-before-effect" => Ok(Self::FailedBeforeEffect),
            "needs-reconcile" => Ok(Self::NeedsReconcile),
            _ => Err(BrokerError::new(format!(
                "unknown broker receipt state {value:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptRequest {
    pub id: [u8; 16],
    pub manager_store_uuid: [u8; 16],
    pub manager_session_id: [u8; 16],
    pub operation_id: [u8; 16],
    pub operation_fence: i64,
    pub effect_kind: EffectKind,
    pub filesystem_uuid: [u8; 16],
    pub target_locator_hash: [u8; 32],
    /// Digest of all effect-specific fixed arguments, authorization
    /// generation/policy, expected identities, mount/idmap and LSM facts.
    pub effect_arguments_hash: [u8; 32],
    pub boot_id: [u8; 16],
    pub started_ns: i64,
}

impl ReceiptRequest {
    pub fn request_hash(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"btrfs-awacs-broker-request-v1\0");
        hash.update(self.manager_store_uuid);
        hash.update(self.operation_id);
        hash.update(self.operation_fence.to_be_bytes());
        hash.update(self.effect_kind.as_str().as_bytes());
        hash.update(self.filesystem_uuid);
        hash.update(self.target_locator_hash);
        hash.update(self.effect_arguments_hash);
        hash.finalize().into()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Receipt {
    pub id: [u8; 16],
    pub manager_store_uuid: [u8; 16],
    pub manager_session_id: [u8; 16],
    pub operation_id: [u8; 16],
    pub operation_fence: i64,
    pub effect_kind: EffectKind,
    pub request_hash: [u8; 32],
    pub filesystem_uuid: [u8; 16],
    pub target_subvol_uuid: Option<[u8; 16]>,
    pub target_locator_hash: [u8; 32],
    pub state: ReceiptState,
    pub result_hash: Option<[u8; 32]>,
    pub boot_id: [u8; 16],
    pub started_ns: i64,
    pub completed_ns: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredBrokerRequest {
    pub receipt: Receipt,
    pub opcode: Opcode,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BeginReceipt {
    Started(Receipt),
    Existing(Receipt),
}

struct ReceiptFinish {
    state: ReceiptState,
    target_subvol_uuid: Option<[u8; 16]>,
    result_hash: Option<[u8; 32]>,
    completed_ns: Option<i64>,
}

impl BrokerJournal {
    pub fn record_request_payload(
        &mut self,
        request: &ReceiptRequest,
        opcode: Opcode,
        payload: &[u8],
    ) -> Result<(), BrokerError> {
        if !matches!(
            opcode,
            Opcode::CreateSnapshot | Opcode::DeleteSnapshot | Opcode::PublishWorktree
        ) || payload.len() > MAX_FRAME_PAYLOAD
        {
            return Err(BrokerError::new("invalid effect request payload"));
        }
        let payload_hash: [u8; 32] = Sha256::digest(payload).into();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, Vec<u8>, Vec<u8>)> = transaction
            .query_row(
                r#"SELECT opcode, payload, payload_hash
                     FROM broker_request_payloads
                    WHERE manager_store_uuid = ?1 AND operation_id = ?2
                      AND operation_fence = ?3"#,
                params![
                    request.manager_store_uuid.as_slice(),
                    request.operation_id.as_slice(),
                    request.operation_fence,
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((existing_opcode, existing_payload, existing_hash)) = existing {
            if existing_opcode != opcode as i64
                || existing_payload != payload
                || existing_hash != payload_hash
            {
                return Err(BrokerError::new(
                    "operation/fence already has a different stored broker payload",
                ));
            }
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute(
            r#"INSERT INTO broker_request_payloads(
                   manager_store_uuid, operation_id, operation_fence,
                   opcode, payload, payload_hash)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
            params![
                request.manager_store_uuid.as_slice(),
                request.operation_id.as_slice(),
                request.operation_fence,
                opcode as i64,
                payload,
                payload_hash.as_slice(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn begin_receipt(&mut self, request: &ReceiptRequest) -> Result<BeginReceipt, BrokerError> {
        if request.operation_fence < 0 {
            return Err(BrokerError::new("operation fence must not be negative"));
        }
        let request_hash = request.request_hash();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = load_receipt_by_operation(
            &transaction,
            request.manager_store_uuid,
            request.operation_id,
            request.operation_fence,
        )?;
        if let Some(existing) = existing {
            if existing.request_hash != request_hash
                || existing.effect_kind != request.effect_kind
                || existing.filesystem_uuid != request.filesystem_uuid
                || existing.target_locator_hash != request.target_locator_hash
            {
                return Err(BrokerError::new(
                    "operation/fence already has a different broker request",
                ));
            }
            transaction.commit()?;
            return Ok(BeginReceipt::Existing(existing));
        }
        transaction.execute(
            r#"INSERT INTO broker_receipts(
                   id, manager_store_uuid, manager_session_id, operation_id,
                   operation_fence, effect_kind, request_hash, filesystem_uuid,
                   target_subvol_uuid, target_locator_hash, state, result_hash,
                   boot_id, started_ns, completed_ns
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9,
                         'running', NULL, ?10, ?11, NULL)"#,
            params![
                request.id.as_slice(),
                request.manager_store_uuid.as_slice(),
                request.manager_session_id.as_slice(),
                request.operation_id.as_slice(),
                request.operation_fence,
                request.effect_kind.as_str(),
                request_hash.as_slice(),
                request.filesystem_uuid.as_slice(),
                request.target_locator_hash.as_slice(),
                request.boot_id.as_slice(),
                request.started_ns,
            ],
        )?;
        let receipt = load_receipt_by_id(&transaction, request.id)?
            .ok_or_else(|| BrokerError::new("new broker receipt disappeared"))?;
        transaction.commit()?;
        Ok(BeginReceipt::Started(receipt))
    }

    pub fn complete_receipt(
        &mut self,
        id: [u8; 16],
        session_id: [u8; 16],
        request_hash: [u8; 32],
        target_subvol_uuid: [u8; 16],
        result_hash: [u8; 32],
        completed_ns: i64,
    ) -> Result<Receipt, BrokerError> {
        self.finish_receipt(
            id,
            session_id,
            request_hash,
            ReceiptFinish {
                state: ReceiptState::Completed,
                target_subvol_uuid: Some(target_subvol_uuid),
                result_hash: Some(result_hash),
                completed_ns: Some(completed_ns),
            },
        )
    }

    pub fn fail_before_effect(
        &mut self,
        id: [u8; 16],
        session_id: [u8; 16],
        request_hash: [u8; 32],
        completed_ns: i64,
    ) -> Result<Receipt, BrokerError> {
        self.finish_receipt(
            id,
            session_id,
            request_hash,
            ReceiptFinish {
                state: ReceiptState::FailedBeforeEffect,
                target_subvol_uuid: None,
                result_hash: None,
                completed_ns: Some(completed_ns),
            },
        )
    }

    pub fn mark_needs_reconcile(
        &mut self,
        id: [u8; 16],
        session_id: [u8; 16],
        request_hash: [u8; 32],
    ) -> Result<Receipt, BrokerError> {
        self.finish_receipt(
            id,
            session_id,
            request_hash,
            ReceiptFinish {
                state: ReceiptState::NeedsReconcile,
                target_subvol_uuid: None,
                result_hash: None,
                completed_ns: None,
            },
        )
    }

    pub fn reconcile_completed(
        &mut self,
        id: [u8; 16],
        request_hash: [u8; 32],
        target_subvol_uuid: [u8; 16],
        result_hash: [u8; 32],
        completed_ns: i64,
    ) -> Result<Receipt, BrokerError> {
        self.reconcile_receipt(
            id,
            request_hash,
            ReceiptState::Completed,
            Some(target_subvol_uuid),
            Some(result_hash),
            completed_ns,
        )
    }

    pub fn reconcile_failed_before_effect(
        &mut self,
        id: [u8; 16],
        request_hash: [u8; 32],
        completed_ns: i64,
    ) -> Result<Receipt, BrokerError> {
        self.reconcile_receipt(
            id,
            request_hash,
            ReceiptState::FailedBeforeEffect,
            None,
            None,
            completed_ns,
        )
    }

    pub fn recover_interrupted_receipts(&mut self) -> Result<usize, BrokerError> {
        Ok(self.connection_mut().execute(
            "UPDATE broker_receipts SET state = 'needs-reconcile' WHERE state = 'running'",
            [],
        )?)
    }

    pub fn unresolved_receipts(
        &self,
        manager_store_uuid: [u8; 16],
    ) -> Result<Vec<Receipt>, BrokerError> {
        let mut statement = self.connection().prepare(
            r#"SELECT id, manager_store_uuid, manager_session_id, operation_id,
                      operation_fence, effect_kind, request_hash, filesystem_uuid,
                      target_subvol_uuid, target_locator_hash, state, result_hash,
                      boot_id, started_ns, completed_ns
                 FROM broker_receipts
                WHERE manager_store_uuid = ?1
                  AND state IN ('running', 'needs-reconcile')
                ORDER BY started_ns, id"#,
        )?;
        let rows = statement.query_map([manager_store_uuid.as_slice()], decode_receipt_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(BrokerError::from)
    }

    pub fn unresolved_requests(
        &self,
        manager_store_uuid: [u8; 16],
    ) -> Result<Vec<StoredBrokerRequest>, BrokerError> {
        let mut statement = self.connection().prepare(
            r#"SELECT r.id, r.manager_store_uuid, r.manager_session_id,
                      r.operation_id, r.operation_fence, r.effect_kind,
                      r.request_hash, r.filesystem_uuid, r.target_subvol_uuid,
                      r.target_locator_hash, r.state, r.result_hash, r.boot_id,
                      r.started_ns, r.completed_ns, p.opcode, p.payload,
                      p.payload_hash
                 FROM broker_receipts r
                 JOIN broker_request_payloads p
                   ON p.manager_store_uuid = r.manager_store_uuid
                  AND p.operation_id = r.operation_id
                  AND p.operation_fence = r.operation_fence
                WHERE r.manager_store_uuid = ?1
                  AND r.state IN ('running', 'needs-reconcile')
                ORDER BY r.started_ns, r.id"#,
        )?;
        let rows =
            statement.query_map([manager_store_uuid.as_slice()], decode_stored_request_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(BrokerError::from)
    }

    pub fn request_for_operation(
        &self,
        manager_store_uuid: [u8; 16],
        operation_id: [u8; 16],
        operation_fence: i64,
        opcode: Opcode,
    ) -> Result<Option<StoredBrokerRequest>, BrokerError> {
        let mut statement = self.connection().prepare(
            r#"SELECT r.id, r.manager_store_uuid, r.manager_session_id,
                      r.operation_id, r.operation_fence, r.effect_kind,
                      r.request_hash, r.filesystem_uuid, r.target_subvol_uuid,
                      r.target_locator_hash, r.state, r.result_hash, r.boot_id,
                      r.started_ns, r.completed_ns, p.opcode, p.payload,
                      p.payload_hash
                 FROM broker_receipts r
                 JOIN broker_request_payloads p
                   ON p.manager_store_uuid = r.manager_store_uuid
                  AND p.operation_id = r.operation_id
                  AND p.operation_fence = r.operation_fence
                WHERE r.manager_store_uuid = ?1 AND r.operation_id = ?2
                  AND r.operation_fence = ?3 AND p.opcode = ?4"#,
        )?;
        statement
            .query_row(
                params![
                    manager_store_uuid.as_slice(),
                    operation_id.as_slice(),
                    operation_fence,
                    opcode as i64,
                ],
                decode_stored_request_row,
            )
            .optional()
            .map_err(BrokerError::from)
    }

    fn finish_receipt(
        &mut self,
        id: [u8; 16],
        session_id: [u8; 16],
        request_hash: [u8; 32],
        finish: ReceiptFinish,
    ) -> Result<Receipt, BrokerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            r#"UPDATE broker_receipts
                  SET state = ?5, target_subvol_uuid = ?6,
                      result_hash = ?7, completed_ns = ?8
                WHERE id = ?1 AND manager_session_id = ?2
                  AND request_hash = ?3 AND state = ?4"#,
            params![
                id.as_slice(),
                session_id.as_slice(),
                request_hash.as_slice(),
                ReceiptState::Running.as_str(),
                finish.state.as_str(),
                finish.target_subvol_uuid.as_ref().map(<[u8; 16]>::as_slice),
                finish.result_hash.as_ref().map(<[u8; 32]>::as_slice),
                finish.completed_ns,
            ],
        )?;
        if changed != 1 {
            return Err(BrokerError::new("broker receipt completion fence is stale"));
        }
        let receipt = load_receipt_by_id(&transaction, id)?
            .ok_or_else(|| BrokerError::new("completed broker receipt disappeared"))?;
        transaction.commit()?;
        Ok(receipt)
    }

    fn reconcile_receipt(
        &mut self,
        id: [u8; 16],
        request_hash: [u8; 32],
        to: ReceiptState,
        target_subvol_uuid: Option<[u8; 16]>,
        result_hash: Option<[u8; 32]>,
        completed_ns: i64,
    ) -> Result<Receipt, BrokerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            r#"UPDATE broker_receipts
                  SET state = ?3, target_subvol_uuid = ?4,
                      result_hash = ?5, completed_ns = ?6
                WHERE id = ?1 AND request_hash = ?2
                  AND state = 'needs-reconcile'"#,
            params![
                id.as_slice(),
                request_hash.as_slice(),
                to.as_str(),
                target_subvol_uuid.as_ref().map(<[u8; 16]>::as_slice),
                result_hash.as_ref().map(<[u8; 32]>::as_slice),
                completed_ns,
            ],
        )?;
        if changed != 1 {
            return Err(BrokerError::new(
                "broker receipt reconciliation fence is stale",
            ));
        }
        let receipt = load_receipt_by_id(&transaction, id)?
            .ok_or_else(|| BrokerError::new("reconciled broker receipt disappeared"))?;
        transaction.commit()?;
        Ok(receipt)
    }
}

fn load_receipt_by_operation(
    connection: &rusqlite::Connection,
    manager_store_uuid: [u8; 16],
    operation_id: [u8; 16],
    operation_fence: i64,
) -> Result<Option<Receipt>, BrokerError> {
    connection
        .query_row(
            r#"SELECT id, manager_store_uuid, manager_session_id, operation_id,
                      operation_fence, effect_kind, request_hash, filesystem_uuid,
                      target_subvol_uuid, target_locator_hash, state, result_hash,
                      boot_id, started_ns, completed_ns
                 FROM broker_receipts
                WHERE manager_store_uuid = ?1 AND operation_id = ?2
                  AND operation_fence = ?3"#,
            params![
                manager_store_uuid.as_slice(),
                operation_id.as_slice(),
                operation_fence,
            ],
            decode_receipt_row,
        )
        .optional()
        .map_err(BrokerError::from)
}

fn load_receipt_by_id(
    connection: &rusqlite::Connection,
    id: [u8; 16],
) -> Result<Option<Receipt>, BrokerError> {
    connection
        .query_row(
            r#"SELECT id, manager_store_uuid, manager_session_id, operation_id,
                      operation_fence, effect_kind, request_hash, filesystem_uuid,
                      target_subvol_uuid, target_locator_hash, state, result_hash,
                      boot_id, started_ns, completed_ns
                 FROM broker_receipts WHERE id = ?1"#,
            [id.as_slice()],
            decode_receipt_row,
        )
        .optional()
        .map_err(BrokerError::from)
}

fn decode_receipt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Receipt> {
    let effect_kind: String = row.get(5)?;
    let state: String = row.get(10)?;
    Ok(Receipt {
        id: sql_blob(row, 0, "receipt id")?,
        manager_store_uuid: sql_blob(row, 1, "manager store UUID")?,
        manager_session_id: sql_blob(row, 2, "manager session ID")?,
        operation_id: sql_blob(row, 3, "operation ID")?,
        operation_fence: row.get(4)?,
        effect_kind: EffectKind::parse(&effect_kind).map_err(sql_decode_error)?,
        request_hash: sql_blob(row, 6, "request hash")?,
        filesystem_uuid: sql_blob(row, 7, "filesystem UUID")?,
        target_subvol_uuid: sql_optional_blob(row, 8, "target subvolume UUID")?,
        target_locator_hash: sql_blob(row, 9, "target locator hash")?,
        state: ReceiptState::parse(&state).map_err(sql_decode_error)?,
        result_hash: sql_optional_blob(row, 11, "result hash")?,
        boot_id: sql_blob(row, 12, "boot ID")?,
        started_ns: row.get(13)?,
        completed_ns: row.get(14)?,
    })
}

fn decode_stored_request_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredBrokerRequest> {
    let receipt = decode_receipt_row(row)?;
    let opcode_value: i64 = row.get(15)?;
    let opcode_u16 = u16::try_from(opcode_value)
        .map_err(|_| sql_decode_error(BrokerError::new("stored broker opcode exceeds u16")))?;
    let opcode = Opcode::decode(opcode_u16).map_err(sql_decode_error)?;
    let payload: Vec<u8> = row.get(16)?;
    let stored_hash: Vec<u8> = row.get(17)?;
    let computed_hash: [u8; 32] = Sha256::digest(&payload).into();
    if stored_hash.as_slice() != computed_hash {
        return Err(sql_decode_error(BrokerError::new(
            "stored broker request payload hash mismatch",
        )));
    }
    Ok(StoredBrokerRequest {
        receipt,
        opcode,
        payload,
    })
}

fn sql_blob<const N: usize>(
    row: &rusqlite::Row<'_>,
    index: usize,
    field: &'static str,
) -> rusqlite::Result<[u8; N]> {
    let value: Vec<u8> = row.get(index)?;
    value.try_into().map_err(|value: Vec<u8>| {
        sql_decode_error(BrokerError::new(format!(
            "{field} has length {}, expected {N}",
            value.len()
        )))
    })
}

fn sql_optional_blob<const N: usize>(
    row: &rusqlite::Row<'_>,
    index: usize,
    field: &'static str,
) -> rusqlite::Result<Option<[u8; N]>> {
    let value: Option<Vec<u8>> = row.get(index)?;
    value
        .map(|value| {
            value.try_into().map_err(|value: Vec<u8>| {
                sql_decode_error(BrokerError::new(format!(
                    "{field} has length {}, expected {N}",
                    value.len()
                )))
            })
        })
        .transpose()
}

fn sql_decode_error(error: BrokerError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(error))
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

impl From<rusqlite::Error> for BrokerError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::os::fd::AsFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use tempfile::tempdir;

    fn request() -> ReceiptRequest {
        ReceiptRequest {
            id: [1; 16],
            manager_store_uuid: [2; 16],
            manager_session_id: [3; 16],
            operation_id: [4; 16],
            operation_fence: 9,
            effect_kind: EffectKind::SnapshotCreate,
            filesystem_uuid: [5; 16],
            target_locator_hash: [6; 32],
            effect_arguments_hash: [7; 32],
            boot_id: [8; 16],
            started_ns: 100,
        }
    }

    fn journal() -> (tempfile::TempDir, BrokerJournal) {
        let temp = tempdir().unwrap();
        let journal = BrokerJournal::create(&temp.path().join("receipts.sqlite3")).unwrap();
        (temp, journal)
    }

    fn snapshot_execution(session_id: [u8; 16]) -> SnapshotCreateExecution {
        let receipt = ReceiptRequest {
            id: [11; 16],
            manager_store_uuid: [12; 16],
            manager_session_id: session_id,
            operation_id: [13; 16],
            operation_fence: 4,
            effect_kind: EffectKind::SnapshotCreate,
            filesystem_uuid: [14; 16],
            target_locator_hash: [0; 32],
            effect_arguments_hash: [0; 32],
            boot_id: [15; 16],
            started_ns: 500,
        };
        let mut execution = SnapshotCreateExecution {
            receipt,
            source: ExpectedSubvolume {
                filesystem_uuid: [14; 16],
                subvolume_uuid: [16; 16],
                root_id: 257,
                generation: 8,
                ctransid: 9,
                otransid: 7,
                parent_uuid: Some([17; 16]),
                received_uuid: None,
                readonly: false,
            },
            destination_parent: ExpectedManagedDirectory {
                filesystem_uuid: [14; 16],
                device: 20,
                inode: 21,
                owner_uid: unsafe { libc::geteuid() },
                mode: 0o700,
                security_context_hash: [0; 32],
            },
            destination_name: b"snapshot-intent".to_vec(),
            readonly: true,
        };
        execution.receipt.target_locator_hash = snapshot_target_locator_hash(
            &execution.destination_parent,
            &execution.destination_name,
        );
        execution.receipt.effect_arguments_hash = snapshot_create_effect_hash(&execution);
        execution
    }

    fn snapshot_delete_execution(session_id: [u8; 16]) -> SnapshotDeleteExecution {
        let created = snapshot_execution(session_id);
        let receipt = ReceiptRequest {
            id: [21; 16],
            manager_store_uuid: created.receipt.manager_store_uuid,
            manager_session_id: session_id,
            operation_id: [22; 16],
            operation_fence: 5,
            effect_kind: EffectKind::SnapshotDelete,
            filesystem_uuid: created.receipt.filesystem_uuid,
            target_locator_hash: [0; 32],
            effect_arguments_hash: [0; 32],
            boot_id: [23; 16],
            started_ns: 600,
        };
        let mut execution = SnapshotDeleteExecution {
            receipt,
            target: ExpectedSubvolume {
                readonly: true,
                ..created.source
            },
            destination_parent: created.destination_parent,
            destination_name: created.destination_name,
        };
        execution.receipt.target_locator_hash = snapshot_target_locator_hash(
            &execution.destination_parent,
            &execution.destination_name,
        );
        execution.receipt.effect_arguments_hash = snapshot_delete_effect_hash(&execution);
        execution
    }

    fn worktree_execution(session_id: [u8; 16]) -> WorktreeRenameExecution {
        let created = snapshot_execution(session_id);
        let receipt = ReceiptRequest {
            id: [31; 16],
            manager_store_uuid: created.receipt.manager_store_uuid,
            manager_session_id: session_id,
            operation_id: [32; 16],
            operation_fence: 6,
            effect_kind: EffectKind::WorktreeRename,
            filesystem_uuid: created.receipt.filesystem_uuid,
            target_locator_hash: [0; 32],
            effect_arguments_hash: [0; 32],
            boot_id: [33; 16],
            started_ns: 700,
        };
        let mut execution = WorktreeRenameExecution {
            receipt,
            worktree: created.source.clone(),
            staging_parent: created.destination_parent.clone(),
            staging_name: b"staged-worktree".to_vec(),
            destination_parent: ExpectedManagedDirectory {
                inode: 30,
                ..created.destination_parent.clone()
            },
            destination_root: created.source,
            destination_root_directory: created.destination_parent.clone(),
            destination_relative_parent: Vec::new(),
            destination_name: b"published-worktree".to_vec(),
            reservation: ExpectedReservation {
                name: b"reservation".to_vec(),
                device: 20,
                inode: 31,
                owner_uid: unsafe { libc::geteuid() },
                nonce: [34; 32],
            },
            authorization_hash: [35; 32],
        };
        execution.receipt.target_locator_hash = snapshot_target_locator_hash(
            &execution.destination_parent,
            &execution.destination_name,
        );
        execution.receipt.effect_arguments_hash = worktree_rename_effect_hash(&execution);
        execution
    }

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
                // The Codex sandbox blocks SO_PEERCRED; production startup
                // treats this as fatal rather than bypassing authentication.
            }
            Err(error) => panic!("read peer credentials: {error}"),
        }
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
        assert!(verify_output_file(output.as_fd(), uid, true)
            .unwrap_err()
            .to_string()
            .contains("private"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let alias = temp.path().join("manifest.alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(verify_output_file(output.as_fd(), uid, true)
            .unwrap_err()
            .to_string()
            .contains("single-link"));
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
        assert!(verify_output_file(read_write.as_fd(), uid, true)
            .unwrap_err()
            .to_string()
            .contains("new"));
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
    fn snapshot_receipt_hashes_bind_every_effect_argument() {
        let execution = snapshot_execution([1; 16]);
        let original_locator = execution.receipt.target_locator_hash;
        let original_effect = execution.receipt.effect_arguments_hash;

        let mut renamed = execution.clone();
        renamed.destination_name = b"another-name".to_vec();
        assert_ne!(
            snapshot_target_locator_hash(&renamed.destination_parent, &renamed.destination_name),
            original_locator
        );
        assert_ne!(snapshot_create_effect_hash(&renamed), original_effect);

        let mut writable = execution.clone();
        writable.readonly = false;
        assert_ne!(snapshot_create_effect_hash(&writable), original_effect);

        let mut another_source = execution;
        another_source.source.subvolume_uuid = [99; 16];
        assert_ne!(
            snapshot_create_effect_hash(&another_source),
            original_effect
        );
    }

    #[test]
    fn snapshot_executor_rejects_unbound_arguments_before_receipt_or_ioctl() {
        let gate = SessionGate::default();
        let store = [12; 16];
        let session = gate.handshake(store);
        let mut execution = snapshot_execution(session);
        execution.destination_name = b"tampered".to_vec();
        let (_temp, mut journal) = journal();
        let null = File::open("/dev/null").unwrap();

        let error =
            execute_snapshot_create(&gate, &mut journal, &execution, null.as_fd(), null.as_fd())
                .unwrap_err();
        assert!(error.to_string().contains("do not bind"));
        assert!(journal.unresolved_receipts(store).unwrap().is_empty());
    }

    #[test]
    fn snapshot_delete_receipt_binds_target_and_locator() {
        let execution = snapshot_delete_execution([1; 16]);
        let original = execution.receipt.effect_arguments_hash;
        let mut another_target = execution.clone();
        another_target.target.subvolume_uuid = [55; 16];
        assert_ne!(snapshot_delete_effect_hash(&another_target), original);

        let mut another_parent = execution;
        another_parent.destination_parent.inode += 1;
        assert_ne!(snapshot_delete_effect_hash(&another_parent), original);
    }

    #[test]
    fn snapshot_delete_rejects_unbound_arguments_before_receipt() {
        let gate = SessionGate::default();
        let store = [12; 16];
        let session = gate.handshake(store);
        let mut execution = snapshot_delete_execution(session);
        execution.destination_name = b"tampered".to_vec();
        let (_temp, mut journal) = journal();
        let null = File::open("/dev/null").unwrap();

        let error =
            execute_snapshot_delete(&gate, &mut journal, &execution, null.as_fd()).unwrap_err();
        assert!(error.to_string().contains("do not bind"));
        assert!(journal.unresolved_receipts(store).unwrap().is_empty());
    }

    #[test]
    fn reservation_observation_requires_exact_private_inode_and_nonce() {
        let temp = tempdir().unwrap();
        let directory = File::open(temp.path()).unwrap();
        let path = temp.path().join("reservation");
        let nonce = [7_u8; 32];
        fs::write(&path, nonce).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let expected = ExpectedReservation::from_observed(
            directory.as_fd(),
            b"reservation",
            unsafe { libc::geteuid() },
            nonce,
        )
        .unwrap();
        assert!(verify_reservation(directory.as_fd(), &expected).unwrap());

        fs::write(&path, [8_u8; 32]).unwrap();
        assert!(verify_reservation(directory.as_fd(), &expected).is_err());
    }

    #[test]
    fn worktree_receipt_binds_both_directories_names_and_reservation() {
        let execution = worktree_execution([1; 16]);
        let original = execution.receipt.effect_arguments_hash;
        let mut renamed = execution.clone();
        renamed.destination_name = b"other".to_vec();
        assert_ne!(worktree_rename_effect_hash(&renamed), original);

        let mut another_nonce = execution;
        another_nonce.reservation.nonce = [99; 32];
        assert_ne!(worktree_rename_effect_hash(&another_nonce), original);
    }

    #[test]
    fn worktree_executor_rejects_unbound_arguments_before_receipt() {
        let gate = SessionGate::default();
        let store = [12; 16];
        let session = gate.handshake(store);
        let mut execution = worktree_execution(session);
        execution.destination_name = b"tampered".to_vec();
        let (_temp, mut journal) = journal();
        let null = File::open("/dev/null").unwrap();

        let error =
            execute_worktree_rename(&gate, &mut journal, &execution, null.as_fd(), null.as_fd())
                .unwrap_err();
        assert!(error.to_string().contains("do not bind"));
        assert!(journal.unresolved_receipts(store).unwrap().is_empty());
    }

    #[test]
    fn receipt_is_durable_before_effect_and_idempotent() {
        let (_temp, mut journal) = journal();
        let request = request();
        let started = journal.begin_receipt(&request).unwrap();
        let BeginReceipt::Started(started) = started else {
            panic!("first receipt was not started")
        };
        assert_eq!(started.state, ReceiptState::Running);
        assert_eq!(
            journal.begin_receipt(&request).unwrap(),
            BeginReceipt::Existing(started.clone())
        );
        let completed = journal
            .complete_receipt(
                request.id,
                request.manager_session_id,
                request.request_hash(),
                [9; 16],
                [10; 32],
                200,
            )
            .unwrap();
        assert_eq!(completed.state, ReceiptState::Completed);
        assert_eq!(completed.target_subvol_uuid, Some([9; 16]));
        assert_eq!(
            journal.begin_receipt(&request).unwrap(),
            BeginReceipt::Existing(completed)
        );
    }

    #[test]
    fn same_operation_fence_cannot_change_arguments() {
        let (_temp, mut journal) = journal();
        let request = request();
        journal.begin_receipt(&request).unwrap();
        let mut conflicting = request.clone();
        conflicting.id = [11; 16];
        conflicting.effect_arguments_hash = [12; 32];
        assert!(journal
            .begin_receipt(&conflicting)
            .unwrap_err()
            .to_string()
            .contains("different broker request"));
    }

    #[test]
    fn interrupted_receipt_requires_exact_reconciliation() {
        let (_temp, mut journal) = journal();
        let request = request();
        let payload = b"exact fixed request";
        journal
            .record_request_payload(&request, Opcode::CreateSnapshot, payload)
            .unwrap();
        journal.begin_receipt(&request).unwrap();
        assert_eq!(journal.recover_interrupted_receipts().unwrap(), 1);
        let unresolved = journal
            .unresolved_receipts(request.manager_store_uuid)
            .unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].state, ReceiptState::NeedsReconcile);
        let stored = journal
            .unresolved_requests(request.manager_store_uuid)
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].opcode, Opcode::CreateSnapshot);
        assert_eq!(stored[0].payload, payload);
        assert!(journal
            .record_request_payload(&request, Opcode::CreateSnapshot, b"different")
            .is_err());
        assert!(journal
            .reconcile_completed(request.id, [0; 32], [9; 16], [10; 32], 300)
            .is_err());
        let completed = journal
            .reconcile_completed(request.id, request.request_hash(), [9; 16], [10; 32], 300)
            .unwrap();
        assert_eq!(completed.state, ReceiptState::Completed);
        assert!(journal
            .unresolved_receipts(request.manager_store_uuid)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn stale_session_cannot_complete_running_effect() {
        let (_temp, mut journal) = journal();
        let request = request();
        journal.begin_receipt(&request).unwrap();
        assert!(journal
            .complete_receipt(
                request.id,
                [99; 16],
                request.request_hash(),
                [9; 16],
                [10; 32],
                200,
            )
            .is_err());
        assert_eq!(
            journal
                .unresolved_receipts(request.manager_store_uuid)
                .unwrap()[0]
                .state,
            ReceiptState::Running
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
        assert!(received
            .recv_timeout(std::time::Duration::from_millis(25))
            .is_err());
        drop(permit);
        let second = received
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        worker.join().unwrap();
        assert!(gate.authorize(store, first).is_err());
        gate.authorize(store, second).unwrap();
    }

    #[test]
    fn worktree_parent_resolution_is_beneath_and_symlink_free() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("allowed")).unwrap();
        std::os::unix::fs::symlink("/", temp.path().join("escape")).unwrap();
        let root = File::open(temp.path()).unwrap();
        let allowed = open_directory_beneath(root.as_fd(), b"allowed").unwrap();
        assert_eq!(
            fd_metadata(allowed.as_fd()).unwrap().st_ino,
            fs::metadata(temp.path().join("allowed")).unwrap().ino()
        );
        assert!(open_directory_beneath(root.as_fd(), b"../").is_err());
        assert!(open_directory_beneath(root.as_fd(), b"escape/tmp").is_err());
    }
}
