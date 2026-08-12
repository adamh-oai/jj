//! Manager orchestration for the SQLite and privileged-broker contracts.
//!
//! Production uses the external authenticated seqpacket broker. UML and unit
//! tests may use the same dispatcher over an embedded socketpair.

use crate::broker::{
    snapshot_create_effect_hash, snapshot_delete_effect_hash, snapshot_target_locator_hash,
    worktree_rename_effect_hash, ChangedObjectsExecution, EffectKind, ExpectedManagedDirectory,
    ExpectedReservation, ExpectedSubvolume, ReceiptRequest, SeqPacket, SnapshotCreateExecution,
    SnapshotDeleteExecution, WorktreeRenameExecution, MAX_CHANGED_OBJECT_OUTPUT,
};
use crate::broker_protocol::{decode_index, decode_objects, BrokerClient, BrokerDispatcher};
use crate::btrfs::{inode_generation, OpenedSubvolume};
use crate::index::{Index, Object};
use crate::manager::{
    worktree_policy_hash, CutAdmission, CutRequest, CutReservation, FacadeActivation,
    HistoricalChanges, HistoricalComparisonAdmission, HistoricalComparisonRequest,
    InitializeRequest, InitializeReservation, InitializedWatch, Permissions, Principal,
    PublishedCut, RecordedSnapshot, SnapshotDeleteReservation, SnapshotIdentity, WorktreePolicy,
    WorktreeRequest, WorktreeReservation,
};
use crate::manifest::{
    parse_changed_objects, parse_changed_objects_v2, ChangedObjectsManifest,
    CHANGED_OBJECTS_V2_MAGIC, CHANGE_CREATED, CHANGE_INODE, CHANGE_XATTR,
};
use crate::namespace::{NamespaceMonitor, PendingNamespaceMonitor};
use crate::store::{decode_u64, BrokerJournal, Store};
use crate::tree_index::{materialize_stream_object, PRIVILEGE_FSCRYPT};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug)]
struct ParsedKernelChangedObjects {
    manifest: ChangedObjectsManifest,
    /// Exact canonical target rows supplied by v2. Legacy streams leave this
    /// absent and use the broker's bounded target-object tree search.
    target_objects: Option<BTreeMap<u64, Object>>,
    dirty_witness_contract: bool,
}

struct FullIndexResult {
    index: Index,
    dirty_witness_contract: bool,
}

const DEFAULT_LEASE_NS: i64 = 300_000_000_000;
const MANIFEST_STAGE_TRAILER_MAGIC: &[u8; 16] = b"bsend-stage-v1\0\0";
const MANIFEST_STAGE_TRAILER_LEN: usize = 16 + 8 + 32;

#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub managed_snapshot_directory: PathBuf,
    pub spool_directory: PathBuf,
    pub boot_id: [u8; 16],
    pub lease_ns: i64,
    pub max_manifest_bytes: u64,
    pub experimental_dirty_witness_verified: bool,
    /// When set, connect to the separately privileged broker instead of
    /// starting the embedded UML/test dispatcher.
    pub broker_socket: Option<PathBuf>,
    pub fault_after_initialize_snapshot: bool,
    pub fault_after_cut_snapshot: bool,
    pub fault_next_incremental_comparison: bool,
    pub fault_after_manifest_stage: bool,
    pub fault_after_snapshot_delete: bool,
    pub fault_after_worktree_create: bool,
    pub fault_after_worktree_publish: bool,
    pub replay_window_cuts: i64,
    pub replay_window_ns: i64,
}

impl ServiceConfig {
    pub fn new(
        managed_snapshot_directory: PathBuf,
        spool_directory: PathBuf,
        boot_id: [u8; 16],
    ) -> Self {
        Self {
            managed_snapshot_directory,
            spool_directory,
            boot_id,
            lease_ns: DEFAULT_LEASE_NS,
            max_manifest_bytes: MAX_CHANGED_OBJECT_OUTPUT,
            experimental_dirty_witness_verified: false,
            broker_socket: None,
            fault_after_initialize_snapshot: false,
            fault_after_cut_snapshot: false,
            fault_next_incremental_comparison: false,
            fault_after_manifest_stage: false,
            fault_after_snapshot_delete: false,
            fault_after_worktree_create: false,
            fault_after_worktree_publish: false,
            replay_window_cuts: 128,
            replay_window_ns: 86_400_000_000_000,
        }
    }

    pub fn allow_experimental_dirty_witness(mut self) -> Self {
        self.experimental_dirty_witness_verified = true;
        self
    }

    pub fn with_broker_socket(mut self, path: PathBuf) -> Self {
        self.broker_socket = Some(path);
        self
    }

    pub fn with_initialize_snapshot_failpoint(mut self) -> Self {
        self.fault_after_initialize_snapshot = true;
        self
    }

    pub fn with_cut_snapshot_failpoint(mut self) -> Self {
        self.fault_after_cut_snapshot = true;
        self
    }

    pub fn with_incremental_comparison_failpoint(mut self) -> Self {
        self.fault_next_incremental_comparison = true;
        self
    }

    pub fn with_manifest_stage_failpoint(mut self) -> Self {
        self.fault_after_manifest_stage = true;
        self
    }

    pub fn with_snapshot_delete_failpoint(mut self) -> Self {
        self.fault_after_snapshot_delete = true;
        self
    }

    pub fn with_worktree_create_failpoint(mut self) -> Self {
        self.fault_after_worktree_create = true;
        self
    }

    pub fn with_worktree_publish_failpoint(mut self) -> Self {
        self.fault_after_worktree_publish = true;
        self
    }

    pub fn with_replay_retention(mut self, cuts: i64, duration_ns: i64) -> Self {
        self.replay_window_cuts = cuts;
        self.replay_window_ns = duration_ns;
        self
    }
}

#[derive(Clone, Debug)]
pub struct InitializeOptions {
    pub principal: Principal,
    pub permissions: Permissions,
    pub requester_uid: u32,
    pub requester_gid: u32,
    pub now_ns: i64,
}

#[derive(Clone, Debug)]
pub struct ChangesOptions {
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub requester_uid: u32,
    pub requester_gid: u32,
    pub now_ns: i64,
}

#[derive(Clone, Debug)]
pub struct WorktreeOptions {
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub destination_root: PathBuf,
    pub destination_parent: PathBuf,
    pub destination_name: Vec<u8>,
    pub reservation_name: Vec<u8>,
    pub reservation_nonce: [u8; 32],
    pub requester_uid: u32,
    pub requester_gid: u32,
    pub now_ns: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedWorktree {
    pub worktree_id: [u8; 16],
    pub watch_id: [u8; 16],
    pub grant_id: [u8; 16],
    pub subvol_uuid: [u8; 16],
    pub path: PathBuf,
    pub seed_revision_id: i64,
    pub seed_snapshot_id: i64,
}

#[derive(Debug)]
pub(crate) struct WorktreeViewHandoff {
    pub authorization_id: [u8; 16],
    pub activation: FacadeActivation,
    pub monitor: NamespaceMonitor,
    pub snapshot_uuid: [u8; 16],
}

struct RecoveringInitialize {
    reservation: InitializeReservation,
    lease_owner: [u8; 16],
    reserved_path: PathBuf,
    live_path: PathBuf,
    recorded: Option<RecordedSnapshot>,
}

struct RecoveringInitializeRow {
    operation_id: Vec<u8>,
    filesystem_id: i64,
    watch_id: Vec<u8>,
    authorization_id: Vec<u8>,
    clock_epoch: Vec<u8>,
    lease_owner: Vec<u8>,
    operation_fence: i64,
    reserved_path: Vec<u8>,
    live_path: Vec<u8>,
    state: String,
    snapshot_id: Option<i64>,
    fs_uuid: Option<Vec<u8>>,
    subvol_uuid: Option<Vec<u8>>,
    parent_uuid: Option<Vec<u8>>,
    received_uuid: Option<Vec<u8>>,
    root_id: Option<Vec<u8>>,
    ctransid: Option<Vec<u8>>,
    otransid: Option<Vec<u8>>,
    snapshot_path: Option<Vec<u8>>,
    readonly: Option<i64>,
    created_ns: Option<i64>,
}

struct CutCompletion {
    reservation: CutReservation,
    lease_owner: [u8; 16],
    target: Option<ExpectedSubvolume>,
    destination_path: PathBuf,
    recorded: Option<RecordedSnapshot>,
    physical_published: bool,
    reuse_staged_spool: bool,
    now_ns: i64,
}

struct RecoveringCut {
    completion: CutCompletion,
    live_path: PathBuf,
}

struct RecoveringCutRow {
    operation_id: Vec<u8>,
    filesystem_id: i64,
    watch_id: Vec<u8>,
    authorization_id: Vec<u8>,
    sequence: i64,
    base_snapshot_id: i64,
    source_subvol_uuid: Vec<u8>,
    lease_owner: Vec<u8>,
    operation_fence: i64,
    cut_fence: i64,
    reserved_path: Vec<u8>,
    live_path: Vec<u8>,
    state: String,
    snapshot_id: Option<i64>,
    fs_uuid: Option<Vec<u8>>,
    subvol_uuid: Option<Vec<u8>>,
    parent_uuid: Option<Vec<u8>>,
    received_uuid: Option<Vec<u8>>,
    root_id: Option<Vec<u8>>,
    ctransid: Option<Vec<u8>>,
    otransid: Option<Vec<u8>>,
    snapshot_path: Option<Vec<u8>>,
    readonly: Option<i64>,
    created_ns: Option<i64>,
}

struct RecoveringSnapshotDelete {
    reservation: SnapshotDeleteReservation,
    lease_owner: [u8; 16],
    state: String,
}

struct RecoveringSnapshotDeleteRow {
    operation_id: Vec<u8>,
    snapshot_id: i64,
    filesystem_id: i64,
    lease_owner: Vec<u8>,
    operation_fence: i64,
    state: String,
    fs_uuid: Vec<u8>,
    subvol_uuid: Vec<u8>,
    parent_uuid: Option<Vec<u8>>,
    received_uuid: Option<Vec<u8>>,
    root_id: Vec<u8>,
    ctransid: Vec<u8>,
    otransid: Vec<u8>,
    snapshot_path: Vec<u8>,
    readonly: i64,
    created_ns: i64,
}

struct RecoveringWorktree {
    reservation: WorktreeReservation,
    lease_owner: [u8; 16],
    state: String,
    staged_path: PathBuf,
    destination_root_path: PathBuf,
    destination_root_uuid: [u8; 16],
    destination_root_generation: u64,
    destination_parent_ino: u64,
    destination_parent_generation: u64,
    destination_name: Vec<u8>,
    reservation_name: Vec<u8>,
    reservation_ino: u64,
    reservation_generation: u64,
    reservation_nonce: [u8; 32],
    requester_uid: u32,
    discovered_uuid: Option<[u8; 16]>,
}

struct RecoveringWorktreeRow {
    operation_id: Vec<u8>,
    worktree_id: Vec<u8>,
    filesystem_id: i64,
    seed_snapshot_id: i64,
    seed_revision_id: i64,
    seed_subvol_uuid: Vec<u8>,
    lease_owner: Vec<u8>,
    operation_fence: i64,
    state: String,
    staged_path: Vec<u8>,
    destination_root_path: Vec<u8>,
    destination_root_uuid: Vec<u8>,
    destination_root_generation: Vec<u8>,
    destination_parent_ino: Vec<u8>,
    destination_parent_generation: Vec<u8>,
    destination_name: Vec<u8>,
    reservation_name: Vec<u8>,
    reservation_ino: Vec<u8>,
    reservation_generation: Vec<u8>,
    reservation_nonce: Vec<u8>,
    requester_uid: u32,
    discovered_uuid: Option<Vec<u8>>,
    policy_hash: Vec<u8>,
}

#[derive(Debug)]
pub struct Service {
    store: Store,
    broker: BrokerClient,
    manager_store_uuid: [u8; 16],
    manager_session_id: [u8; 16],
    lease_owner: [u8; 16],
    config: ServiceConfig,
    worktree_view_handoffs: BTreeMap<[u8; 16], WorktreeViewHandoff>,
    dirty_witness_contract_seen: bool,
}

impl Service {
    pub fn new(
        mut store: Store,
        journal: BrokerJournal,
        config: ServiceConfig,
    ) -> Result<Self, ServiceError> {
        if config.broker_socket.is_some() {
            return Err(ServiceError::new(
                "Service::new is for the embedded broker; use Service::new_external",
            ));
        }
        validate_private_directory(&config.managed_snapshot_directory)?;
        validate_private_directory(&config.spool_directory)?;
        if config.lease_ns <= 0 {
            return Err(ServiceError::new("service lease duration must be positive"));
        }
        if config.max_manifest_bytes == 0 || config.max_manifest_bytes > MAX_CHANGED_OBJECT_OUTPUT {
            return Err(ServiceError::new("invalid changed-object manifest limit"));
        }
        if config.replay_window_cuts <= 0 || config.replay_window_ns <= 0 {
            return Err(ServiceError::new("invalid replay retention window"));
        }
        let manager_store_uuid = store
            .metadata()
            .map_err(|error| ServiceError::context("read manager metadata", error))?
            .store_uuid;
        store
            .recover_process_state(config.boot_id)
            .map_err(|error| ServiceError::context("recover manager process state", error))?;
        let (client_socket, server_socket) = SeqPacket::pair()
            .map_err(|error| ServiceError::context("create broker channel", error))?;
        let manager_uid = unsafe { libc::geteuid() };
        std::thread::Builder::new()
            .name("btrfs-awacs-broker".to_owned())
            .spawn(move || {
                let dispatcher = BrokerDispatcher::with_journal(manager_uid, journal);
                while dispatcher.serve_one(&server_socket).is_ok() {}
            })
            .map_err(|error| ServiceError::context("start broker dispatcher", error))?;
        let broker = BrokerClient::connect(client_socket, manager_store_uuid)
            .map_err(|error| ServiceError::context("handshake with broker", error))?;
        let manager_session_id = broker.session_id();
        let lease_owner = random_id();
        let mut service = Self {
            store,
            broker,
            manager_store_uuid,
            manager_session_id,
            lease_owner,
            config,
            worktree_view_handoffs: BTreeMap::new(),
            dirty_witness_contract_seen: false,
        };
        let recovery_now = current_unix_time_ns()?;
        service
            .store
            .abort_planned_operations(recovery_now)
            .map_err(|error| ServiceError::context("abort pre-effect operations", error))?;
        service
            .store
            .takeover_recovery_leases(
                lease_owner,
                lease_expiry(recovery_now, service.config.lease_ns)?,
            )
            .map_err(|error| ServiceError::context("take over recovery leases", error))?;
        service.recover_initialize_operations()?;
        service.recover_cut_operations()?;
        service.recover_worktree_operations()?;
        service.recover_snapshot_delete_operations()?;
        cleanup_stale_spool_files(&service.config.spool_directory)?;
        quarantine_unexpected_managed_entries(
            &service.store,
            &service.config.managed_snapshot_directory,
        )?;
        reject_unresolved_receipts(&service.broker)?;
        Ok(service)
    }

    pub fn new_external(mut store: Store, config: ServiceConfig) -> Result<Self, ServiceError> {
        validate_private_directory(&config.managed_snapshot_directory)?;
        validate_private_directory(&config.spool_directory)?;
        if config.lease_ns <= 0 {
            return Err(ServiceError::new("service lease duration must be positive"));
        }
        if config.max_manifest_bytes == 0 || config.max_manifest_bytes > MAX_CHANGED_OBJECT_OUTPUT {
            return Err(ServiceError::new("invalid changed-object manifest limit"));
        }
        if config.replay_window_cuts <= 0 || config.replay_window_ns <= 0 {
            return Err(ServiceError::new("invalid replay retention window"));
        }
        let socket_path = config
            .broker_socket
            .as_ref()
            .ok_or_else(|| ServiceError::new("external broker socket is not configured"))?;
        let manager_store_uuid = store
            .metadata()
            .map_err(|error| ServiceError::context("read manager metadata", error))?
            .store_uuid;
        store
            .recover_process_state(config.boot_id)
            .map_err(|error| ServiceError::context("recover manager process state", error))?;
        let socket = SeqPacket::connect(socket_path)
            .map_err(|error| ServiceError::context("connect external broker", error))?;
        let broker = BrokerClient::connect(socket, manager_store_uuid)
            .map_err(|error| ServiceError::context("handshake with broker", error))?;
        let manager_session_id = broker.session_id();
        let lease_owner = random_id();
        let mut service = Self {
            store,
            broker,
            manager_store_uuid,
            manager_session_id,
            lease_owner,
            config,
            worktree_view_handoffs: BTreeMap::new(),
            dirty_witness_contract_seen: false,
        };
        let recovery_now = current_unix_time_ns()?;
        service
            .store
            .abort_planned_operations(recovery_now)
            .map_err(|error| ServiceError::context("abort pre-effect operations", error))?;
        service
            .store
            .takeover_recovery_leases(
                lease_owner,
                lease_expiry(recovery_now, service.config.lease_ns)?,
            )
            .map_err(|error| ServiceError::context("take over recovery leases", error))?;
        service.recover_initialize_operations()?;
        service.recover_cut_operations()?;
        service.recover_worktree_operations()?;
        service.recover_snapshot_delete_operations()?;
        cleanup_stale_spool_files(&service.config.spool_directory)?;
        quarantine_unexpected_managed_entries(
            &service.store,
            &service.config.managed_snapshot_directory,
        )?;
        reject_unresolved_receipts(&service.broker)?;
        Ok(service)
    }

    fn recover_initialize_operations(&mut self) -> Result<(), ServiceError> {
        let recovering = self.load_recovering_initializes()?;
        for operation in recovering {
            let recorded = if let Some(recorded) = operation.recorded {
                recorded
            } else {
                let source = OpenedSubvolume::open(&operation.live_path)
                    .map_err(|error| ServiceError::context("open recovery source", error))?;
                if source.subvolume.uuid
                    != self.initialize_source_uuid(operation.reservation.operation_id)?
                {
                    return Err(ServiceError::new(
                        "initialize recovery source subvolume changed",
                    ));
                }
                let destination_name = operation
                    .reserved_path
                    .file_name()
                    .ok_or_else(|| ServiceError::new("recovery snapshot path has no basename"))?
                    .as_bytes()
                    .to_vec();
                let destination =
                    File::open(&self.config.managed_snapshot_directory).map_err(|error| {
                        ServiceError::context("open recovery snapshot directory", error)
                    })?;
                let target = if self
                    .broker
                    .has_stored_effect(
                        crate::broker::Opcode::CreateSnapshot,
                        operation.reservation.operation_id,
                        operation.reservation.operation_fence,
                    )
                    .map_err(|error| {
                        ServiceError::context("inspect stored initialize effect", error)
                    })? {
                    self.broker
                        .reconcile_snapshot_create(
                            operation.reservation.operation_id,
                            operation.reservation.operation_fence,
                            source.as_fd(),
                            destination.as_fd(),
                        )
                        .map_err(|error| {
                            ServiceError::context("reconcile initialize snapshot", error)
                        })?
                        .snapshot
                } else {
                    self.create_snapshot(
                        &source,
                        &destination_name,
                        operation.reservation.operation_id,
                        operation.reservation.operation_fence,
                        true,
                        current_unix_time_ns()?,
                    )?
                };
                let now_ns = current_unix_time_ns()?;
                let identity = snapshot_identity(
                    &target,
                    operation.reserved_path.as_os_str().as_bytes().to_vec(),
                    now_ns,
                );
                self.store
                    .record_initialize_snapshot(
                        &operation.reservation,
                        operation.lease_owner,
                        &identity,
                        now_ns,
                    )
                    .map_err(|error| {
                        ServiceError::context("record recovered initialize snapshot", error)
                    })?
            };
            reject_nested_subvolumes(&operation.reserved_path)?;
            let snapshot_fd = OpenedSubvolume::open(&operation.reserved_path)
                .map_err(|error| ServiceError::context("open recovered snapshot", error))?;
            let expected =
                ExpectedSubvolume::from_observed(&snapshot_fd.filesystem, &snapshot_fd.subvolume);
            verify_recorded_snapshot(&recorded.identity, &expected)?;
            let full_index = self.broker_full_index(&expected, snapshot_fd.as_fd())?;
            self.dirty_witness_contract_seen |= full_index.dirty_witness_contract;
            self.store
                .publish_initial_checkpoint(
                    &operation.reservation,
                    operation.lease_owner,
                    &recorded,
                    &full_index.index,
                    current_unix_time_ns()?,
                )
                .map_err(|error| {
                    ServiceError::context("publish recovered initial checkpoint", error)
                })?;
        }
        Ok(())
    }

    fn load_recovering_initializes(&self) -> Result<Vec<RecoveringInitialize>, ServiceError> {
        let mut statement = self
            .store
            .connection()
            .prepare(
                r#"SELECT o.id, o.filesystem_id, o.watch_id, o.authorization_id,
                          w.clock_epoch, o.lease_owner, o.lease_fence,
                          o.reserved_path, w.live_path, o.state,
                          s.id, f.fs_uuid, s.subvol_uuid, s.parent_uuid,
                          s.received_uuid, s.root_id, s.ctransid, s.otransid,
                          s.path, s.readonly, s.created_ns
                     FROM operations o
                     JOIN watches w ON w.id = o.watch_id
                     JOIN filesystems f ON f.id = o.filesystem_id
                     LEFT JOIN snapshots s
                       ON s.filesystem_id = o.filesystem_id
                      AND s.subvol_uuid = o.discovered_uuid
                    WHERE o.kind = 'initialize'
                      AND o.state IN ('fs_started', 'uuid_recorded')
                    ORDER BY o.updated_ns, o.id"#,
            )
            .map_err(|error| ServiceError::context("prepare initialize recovery", error))?;
        let rows = statement
            .query_map([], |row| {
                Ok(RecoveringInitializeRow {
                    operation_id: row.get(0)?,
                    filesystem_id: row.get(1)?,
                    watch_id: row.get(2)?,
                    authorization_id: row.get(3)?,
                    clock_epoch: row.get(4)?,
                    lease_owner: row.get(5)?,
                    operation_fence: row.get(6)?,
                    reserved_path: row.get(7)?,
                    live_path: row.get(8)?,
                    state: row.get(9)?,
                    snapshot_id: row.get(10)?,
                    fs_uuid: row.get(11)?,
                    subvol_uuid: row.get(12)?,
                    parent_uuid: row.get(13)?,
                    received_uuid: row.get(14)?,
                    root_id: row.get(15)?,
                    ctransid: row.get(16)?,
                    otransid: row.get(17)?,
                    snapshot_path: row.get(18)?,
                    readonly: row.get(19)?,
                    created_ns: row.get(20)?,
                })
            })
            .map_err(|error| ServiceError::context("query initialize recovery", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ServiceError::context("decode initialize recovery", error))?;
        rows.into_iter().map(decode_recovering_initialize).collect()
    }

    fn initialize_source_uuid(&self, operation_id: [u8; 16]) -> Result<[u8; 16], ServiceError> {
        let bytes: Vec<u8> = self
            .store
            .connection()
            .query_row(
                "SELECT source_subvol_uuid FROM operations WHERE id = ?1",
                [operation_id.as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| ServiceError::context("load initialize source UUID", error))?;
        fixed_service_blob(&bytes, "initialize source UUID")
    }

    fn recover_cut_operations(&mut self) -> Result<(), ServiceError> {
        for mut operation in self.load_recovering_cuts()? {
            if let Some(recorded) = operation.completion.recorded.as_ref() {
                let target = OpenedSubvolume::open(&operation.completion.destination_path)
                    .map_err(|error| ServiceError::context("open recorded cut snapshot", error))?;
                operation.completion.target = Some(ExpectedSubvolume::from_observed(
                    &target.filesystem,
                    &target.subvolume,
                ));
                verify_recorded_snapshot(
                    &recorded.identity,
                    operation
                        .completion
                        .target
                        .as_ref()
                        .expect("recovery target populated above"),
                )?;
            } else {
                let source = OpenedSubvolume::open(&operation.live_path)
                    .map_err(|error| ServiceError::context("open cut recovery source", error))?;
                if source.subvolume.uuid != operation.completion.reservation.source_subvol_uuid {
                    return Err(ServiceError::new("cut recovery source subvolume changed"));
                }
                let destination_name = operation
                    .completion
                    .destination_path
                    .file_name()
                    .ok_or_else(|| ServiceError::new("recovery cut path has no basename"))?
                    .as_bytes()
                    .to_vec();
                let destination = File::open(&self.config.managed_snapshot_directory)
                    .map_err(|error| ServiceError::context("open cut recovery directory", error))?;
                operation.completion.target = Some(
                    if self
                        .broker
                        .has_stored_effect(
                            crate::broker::Opcode::CreateSnapshot,
                            operation.completion.reservation.operation_id,
                            operation.completion.reservation.operation_fence,
                        )
                        .map_err(|error| {
                            ServiceError::context("inspect stored cut effect", error)
                        })?
                    {
                        self.broker
                            .reconcile_snapshot_create(
                                operation.completion.reservation.operation_id,
                                operation.completion.reservation.operation_fence,
                                source.as_fd(),
                                destination.as_fd(),
                            )
                            .map_err(|error| {
                                ServiceError::context("reconcile cut snapshot", error)
                            })?
                            .snapshot
                    } else {
                        self.create_snapshot(
                            &source,
                            &destination_name,
                            operation.completion.reservation.operation_id,
                            operation.completion.reservation.operation_fence,
                            true,
                            operation.completion.now_ns,
                        )?
                    },
                );
            }
            self.finish_cut(operation.completion)?;
        }
        Ok(())
    }

    fn load_recovering_cuts(&self) -> Result<Vec<RecoveringCut>, ServiceError> {
        let mut statement = self
            .store
            .connection()
            .prepare(
                r#"SELECT o.id, o.filesystem_id, o.watch_id, o.authorization_id,
                          o.sequence, o.base_snapshot_id, o.source_subvol_uuid,
                          o.lease_owner, o.lease_fence, w.cut_fence,
                          o.reserved_path, w.live_path, o.state,
                          s.id, f.fs_uuid, s.subvol_uuid, s.parent_uuid,
                          s.received_uuid, s.root_id, s.ctransid, s.otransid,
                          s.path, s.readonly, s.created_ns
                     FROM operations o
                     JOIN watches w ON w.id = o.watch_id
                     JOIN filesystems f ON f.id = o.filesystem_id
                     LEFT JOIN snapshots s
                       ON s.filesystem_id = o.filesystem_id
                      AND s.subvol_uuid = o.discovered_uuid
                    WHERE o.kind = 'cut'
                      AND o.state IN ('fs_started', 'uuid_recorded', 'manifest_ready')
                    ORDER BY o.sequence, o.id"#,
            )
            .map_err(|error| ServiceError::context("prepare cut recovery", error))?;
        let rows = statement
            .query_map([], |row| {
                Ok(RecoveringCutRow {
                    operation_id: row.get(0)?,
                    filesystem_id: row.get(1)?,
                    watch_id: row.get(2)?,
                    authorization_id: row.get(3)?,
                    sequence: row.get(4)?,
                    base_snapshot_id: row.get(5)?,
                    source_subvol_uuid: row.get(6)?,
                    lease_owner: row.get(7)?,
                    operation_fence: row.get(8)?,
                    cut_fence: row.get(9)?,
                    reserved_path: row.get(10)?,
                    live_path: row.get(11)?,
                    state: row.get(12)?,
                    snapshot_id: row.get(13)?,
                    fs_uuid: row.get(14)?,
                    subvol_uuid: row.get(15)?,
                    parent_uuid: row.get(16)?,
                    received_uuid: row.get(17)?,
                    root_id: row.get(18)?,
                    ctransid: row.get(19)?,
                    otransid: row.get(20)?,
                    snapshot_path: row.get(21)?,
                    readonly: row.get(22)?,
                    created_ns: row.get(23)?,
                })
            })
            .map_err(|error| ServiceError::context("query cut recovery", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ServiceError::context("decode cut recovery", error))?;
        rows.into_iter().map(decode_recovering_cut).collect()
    }

    fn recover_worktree_operations(&mut self) -> Result<(), ServiceError> {
        for mut operation in self.load_recovering_worktrees()? {
            let mut staged = None;
            if operation.state == "fs_started" {
                let seed_path = self.snapshot_path(operation.reservation.seed_snapshot_id)?;
                let seed = OpenedSubvolume::open(&seed_path).map_err(|error| {
                    ServiceError::context("open recovered Worktree seed", error)
                })?;
                let staging_name = operation
                    .staged_path
                    .file_name()
                    .ok_or_else(|| {
                        ServiceError::new("recovery Worktree staging path has no basename")
                    })?
                    .as_bytes()
                    .to_vec();
                let staging_parent =
                    File::open(&self.config.managed_snapshot_directory).map_err(|error| {
                        ServiceError::context("open recovered Worktree staging directory", error)
                    })?;
                let create_operation_id =
                    derived_effect_id(operation.reservation.operation_id, b"create");
                let created = if self
                    .broker
                    .has_stored_effect(
                        crate::broker::Opcode::CreateSnapshot,
                        create_operation_id,
                        operation.reservation.operation_fence,
                    )
                    .map_err(|error| {
                        ServiceError::context("inspect stored Worktree-create effect", error)
                    })? {
                    self.broker
                        .reconcile_snapshot_create(
                            create_operation_id,
                            operation.reservation.operation_fence,
                            seed.as_fd(),
                            staging_parent.as_fd(),
                        )
                        .map_err(|error| ServiceError::context("reconcile Worktree clone", error))?
                        .snapshot
                } else {
                    self.create_snapshot(
                        &seed,
                        &staging_name,
                        create_operation_id,
                        operation.reservation.operation_fence,
                        false,
                        current_unix_time_ns()?,
                    )?
                };
                self.store
                    .record_created_worktree(
                        &operation.reservation,
                        operation.lease_owner,
                        created.subvolume_uuid,
                        created.parent_uuid,
                        current_unix_time_ns()?,
                    )
                    .map_err(|error| {
                        ServiceError::context("record recovered Worktree clone", error)
                    })?;
                operation.discovered_uuid = Some(created.subvolume_uuid);
                operation.state = "awaiting_destination".to_owned();
                staged = Some(created);
            }
            if operation.state != "awaiting_destination" {
                return Err(ServiceError::new("invalid Worktree recovery state"));
            }
            self.finish_recovered_worktree(&operation, staged)?;
        }
        Ok(())
    }

    fn load_recovering_worktrees(&self) -> Result<Vec<RecoveringWorktree>, ServiceError> {
        let mut statement = self
            .store
            .connection()
            .prepare(
                r#"SELECT o.id, wt.id, o.filesystem_id, o.base_snapshot_id,
                          wt.seed_revision_id, o.source_subvol_uuid,
                          o.lease_owner, o.lease_fence, o.state, o.reserved_path,
                          p.destination_root_path, p.destination_root_subvol_uuid,
                          p.destination_root_generation, o.destination_parent_ino,
                          o.destination_parent_generation, o.destination_name,
                          o.destination_reservation_name,
                          o.destination_reservation_ino,
                          o.destination_reservation_generation,
                          o.destination_reservation_nonce, o.requester_uid,
                          o.discovered_uuid, p.policy_hash
                     FROM operations o
                     JOIN worktrees wt ON wt.operation_id = o.id
                     JOIN worktree_grant_policies p ON p.id = o.worktree_policy_id
                    WHERE o.kind = 'worktree'
                      AND o.state IN ('fs_started', 'awaiting_destination')
                    ORDER BY o.updated_ns, o.id"#,
            )
            .map_err(|error| ServiceError::context("prepare Worktree recovery", error))?;
        let rows = statement
            .query_map([], |row| {
                Ok(RecoveringWorktreeRow {
                    operation_id: row.get(0)?,
                    worktree_id: row.get(1)?,
                    filesystem_id: row.get(2)?,
                    seed_snapshot_id: row.get(3)?,
                    seed_revision_id: row.get(4)?,
                    seed_subvol_uuid: row.get(5)?,
                    lease_owner: row.get(6)?,
                    operation_fence: row.get(7)?,
                    state: row.get(8)?,
                    staged_path: row.get(9)?,
                    destination_root_path: row.get(10)?,
                    destination_root_uuid: row.get(11)?,
                    destination_root_generation: row.get(12)?,
                    destination_parent_ino: row.get(13)?,
                    destination_parent_generation: row.get(14)?,
                    destination_name: row.get(15)?,
                    reservation_name: row.get(16)?,
                    reservation_ino: row.get(17)?,
                    reservation_generation: row.get(18)?,
                    reservation_nonce: row.get(19)?,
                    requester_uid: row.get(20)?,
                    discovered_uuid: row.get(21)?,
                    policy_hash: row.get(22)?,
                })
            })
            .map_err(|error| ServiceError::context("query Worktree recovery", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ServiceError::context("decode Worktree recovery", error))?;
        rows.into_iter().map(decode_recovering_worktree).collect()
    }

    fn finish_recovered_worktree(
        &mut self,
        operation: &RecoveringWorktree,
        staged: Option<ExpectedSubvolume>,
    ) -> Result<(), ServiceError> {
        let root = OpenedSubvolume::open(&operation.destination_root_path)
            .map_err(|error| ServiceError::context("open Worktree recovery policy root", error))?;
        if root.subvolume.uuid != operation.destination_root_uuid
            || inode_generation(root.as_fd()).map_err(|error| {
                ServiceError::context("verify Worktree recovery root generation", error)
            })? != operation.destination_root_generation
        {
            return Err(ServiceError::new("Worktree recovery policy root changed"));
        }
        let parent_path = find_directory_by_identity(
            &operation.destination_root_path,
            operation.destination_parent_ino,
            operation.destination_parent_generation,
        )?;
        let destination_relative_parent =
            relative_directory_bytes(&operation.destination_root_path, &parent_path)?;
        let destination = File::open(&parent_path)
            .map_err(|error| ServiceError::context("open Worktree recovery destination", error))?;
        let destination_identity = ExpectedManagedDirectory::from_observed(destination.as_fd())
            .map_err(|error| {
                ServiceError::context("inspect Worktree recovery destination", error)
            })?;
        if destination_identity.inode != operation.destination_parent_ino {
            return Err(ServiceError::new(
                "Worktree recovery destination identity changed",
            ));
        }
        let staging_parent =
            File::open(&self.config.managed_snapshot_directory).map_err(|error| {
                ServiceError::context("open Worktree recovery staging parent", error)
            })?;
        let publication_now = current_unix_time_ns()?;
        let resolved_final_path = parent_path.join(std::ffi::OsString::from_vec(
            operation.destination_name.clone(),
        ));
        let topology_fence = self
            .store
            .prepare_worktree_publication(
                &operation.reservation,
                operation.lease_owner,
                resolved_final_path.as_os_str().as_bytes(),
                publication_now,
                lease_expiry(publication_now, self.config.lease_ns)?,
            )
            .map_err(|error| {
                ServiceError::context("prepare recovered Worktree publication", error)
            })?;
        let stored = self
            .broker
            .has_stored_effect(
                crate::broker::Opcode::PublishWorktree,
                operation.reservation.operation_id,
                operation.reservation.operation_fence,
            )
            .map_err(|error| {
                ServiceError::context("inspect stored Worktree-publish effect", error)
            })?;
        let published_uuid = if stored {
            self.broker
                .reconcile_worktree_publish(
                    operation.reservation.operation_id,
                    operation.reservation.operation_fence,
                    staging_parent.as_fd(),
                    root.as_fd(),
                )
                .map_err(|error| ServiceError::context("reconcile Worktree publication", error))?
                .worktree_subvolume_uuid
        } else {
            let staged = match staged {
                Some(staged) => staged,
                None => {
                    let opened = OpenedSubvolume::open(&operation.staged_path)
                        .map_err(|error| ServiceError::context("open staged Worktree", error))?;
                    ExpectedSubvolume::from_observed(&opened.filesystem, &opened.subvolume)
                }
            };
            let observed_reservation = ExpectedReservation::from_observed(
                destination.as_fd(),
                &operation.reservation_name,
                operation.requester_uid,
                operation.reservation_nonce,
            )
            .map_err(|error| {
                ServiceError::context("inspect Worktree recovery reservation", error)
            })?;
            let reservation_path = parent_path.join(std::ffi::OsString::from_vec(
                operation.reservation_name.clone(),
            ));
            let reservation_file = File::open(&reservation_path).map_err(|error| {
                ServiceError::context("open Worktree recovery reservation", error)
            })?;
            if observed_reservation.inode != operation.reservation_ino
                || inode_generation(reservation_file.as_fd()).map_err(|error| {
                    ServiceError::context("verify Worktree reservation generation", error)
                })? != operation.reservation_generation
            {
                return Err(ServiceError::new(
                    "Worktree recovery reservation identity changed",
                ));
            }
            let staging_name = operation
                .staged_path
                .file_name()
                .ok_or_else(|| ServiceError::new("staged Worktree path has no basename"))?
                .as_bytes()
                .to_vec();
            let receipt = ReceiptRequest {
                id: random_id(),
                manager_store_uuid: self.manager_store_uuid,
                manager_session_id: self.manager_session_id,
                operation_id: operation.reservation.operation_id,
                operation_fence: operation.reservation.operation_fence,
                effect_kind: EffectKind::WorktreeRename,
                filesystem_uuid: destination_identity.filesystem_uuid,
                target_locator_hash: [0; 32],
                effect_arguments_hash: [0; 32],
                boot_id: self.config.boot_id,
                started_ns: current_unix_time_ns()?,
            };
            let mut execution = WorktreeRenameExecution {
                receipt,
                worktree: staged,
                staging_parent: ExpectedManagedDirectory::from_observed(staging_parent.as_fd())
                    .map_err(|error| {
                        ServiceError::context("inspect Worktree recovery staging", error)
                    })?,
                staging_name,
                destination_parent: destination_identity,
                destination_root: ExpectedSubvolume::from_observed(
                    &root.filesystem,
                    &root.subvolume,
                ),
                destination_root_directory: ExpectedManagedDirectory::from_observed(root.as_fd())
                    .map_err(|error| {
                    ServiceError::context("inspect Worktree recovery policy root", error)
                })?,
                destination_relative_parent,
                destination_name: operation.destination_name.clone(),
                reservation: observed_reservation,
                authorization_hash: operation.reservation.policy_hash,
            };
            execution.receipt.target_locator_hash = snapshot_target_locator_hash(
                &execution.destination_parent,
                &execution.destination_name,
            );
            execution.receipt.effect_arguments_hash = worktree_rename_effect_hash(&execution);
            self.broker
                .publish_worktree(&execution, staging_parent.as_fd(), root.as_fd())
                .map_err(|error| ServiceError::context("publish recovered Worktree", error))?
                .worktree_subvolume_uuid
        };
        if Some(published_uuid) != operation.discovered_uuid {
            return Err(ServiceError::new(
                "recovered Worktree UUID differs from manager intent",
            ));
        }
        if self.config.fault_after_worktree_publish {
            return Err(ServiceError::new(
                "injected failure after Worktree publication effect",
            ));
        }
        self.store
            .publish_worktree(
                &operation.reservation,
                operation.lease_owner,
                topology_fence,
                published_uuid,
                current_unix_time_ns()?,
            )
            .map_err(|error| ServiceError::context("publish recovered Worktree metadata", error))?;
        Ok(())
    }

    fn recover_snapshot_delete_operations(&mut self) -> Result<(), ServiceError> {
        for operation in self.load_recovering_snapshot_deletes()? {
            if operation.state == "fs_started" {
                self.execute_or_reconcile_snapshot_delete(
                    &operation.reservation,
                    operation.lease_owner,
                    current_unix_time_ns()?,
                    true,
                )?;
            } else if operation.state != "delete_durable" {
                return Err(ServiceError::new("invalid snapshot-delete recovery state"));
            }
            self.store
                .finish_snapshot_delete(
                    &operation.reservation,
                    operation.lease_owner,
                    current_unix_time_ns()?,
                )
                .map_err(|error| {
                    ServiceError::context("finish recovered snapshot deletion", error)
                })?;
        }
        Ok(())
    }

    fn load_recovering_snapshot_deletes(
        &self,
    ) -> Result<Vec<RecoveringSnapshotDelete>, ServiceError> {
        let mut statement = self
            .store
            .connection()
            .prepare(
                r#"SELECT d.id, d.snapshot_id, d.filesystem_id, d.lease_owner,
                          d.lease_fence, d.state, f.fs_uuid, s.subvol_uuid,
                          s.parent_uuid, s.received_uuid, s.root_id, s.ctransid,
                          s.otransid, s.path, s.readonly, s.created_ns
                     FROM snapshot_delete_operations d
                     JOIN snapshots s ON s.id = d.snapshot_id
                     JOIN filesystems f ON f.id = d.filesystem_id
                    WHERE d.state IN ('fs_started', 'delete_durable')
                    ORDER BY d.updated_ns, d.id"#,
            )
            .map_err(|error| ServiceError::context("prepare snapshot-delete recovery", error))?;
        let rows = statement
            .query_map([], |row| {
                Ok(RecoveringSnapshotDeleteRow {
                    operation_id: row.get(0)?,
                    snapshot_id: row.get(1)?,
                    filesystem_id: row.get(2)?,
                    lease_owner: row.get(3)?,
                    operation_fence: row.get(4)?,
                    state: row.get(5)?,
                    fs_uuid: row.get(6)?,
                    subvol_uuid: row.get(7)?,
                    parent_uuid: row.get(8)?,
                    received_uuid: row.get(9)?,
                    root_id: row.get(10)?,
                    ctransid: row.get(11)?,
                    otransid: row.get(12)?,
                    snapshot_path: row.get(13)?,
                    readonly: row.get(14)?,
                    created_ns: row.get(15)?,
                })
            })
            .map_err(|error| ServiceError::context("query snapshot-delete recovery", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ServiceError::context("decode snapshot-delete recovery", error))?;
        rows.into_iter()
            .map(decode_recovering_snapshot_delete)
            .collect()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    pub(crate) fn take_worktree_view_handoff(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        root: &Path,
    ) -> Option<WorktreeViewHandoff> {
        let handoff = self.worktree_view_handoffs.remove(&watch_id)?;
        let matches = handoff.authorization_id == authorization_id
            && std::fs::canonicalize(root).ok().is_some_and(|canonical| {
                canonical.as_os_str().as_bytes() == handoff.monitor.binding().root_path
            });
        if matches {
            Some(handoff)
        } else {
            self.worktree_view_handoffs.insert(watch_id, handoff);
            None
        }
    }

    pub fn snapshot_facade_is_enabled(&self) -> bool {
        self.config.experimental_dirty_witness_verified && self.dirty_witness_contract_seen
    }

    /// Fail-closed facade gate. A process restart or kernel downgrade clears
    /// the in-memory capability observation, so revalidate the immutable head
    /// through the same v2 full-index ABI before minting another clock.
    pub fn ensure_snapshot_facade_is_enabled(
        &mut self,
        watch_id: [u8; 16],
    ) -> Result<bool, ServiceError> {
        if !self.config.experimental_dirty_witness_verified {
            return Ok(false);
        }
        if self.dirty_witness_contract_seen {
            return Ok(true);
        }
        let (revision_id, path): (i64, Vec<u8>) = self
            .store
            .connection()
            .query_row(
                "SELECT w.indexed_revision_id, s.path \
                   FROM watches w \
                   JOIN revisions r ON r.id = w.indexed_revision_id \
                   JOIN snapshots s ON s.id = r.snapshot_id \
                  WHERE w.id = ?1 AND w.state = 'active'",
                [watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| ServiceError::context("load facade ABI probe head", error))?;
        let path = PathBuf::from(OsString::from_vec(path));
        let snapshot = OpenedSubvolume::open(&path)
            .map_err(|error| ServiceError::context("open facade ABI probe head", error))?;
        let expected = ExpectedSubvolume::from_observed(&snapshot.filesystem, &snapshot.subvolume);
        let full_index = self.broker_full_index(&expected, snapshot.as_fd())?;
        if !full_index.dirty_witness_contract {
            return Ok(false);
        }
        let indexed = self
            .store
            .load_revision(revision_id)
            .map_err(|error| ServiceError::context("load facade ABI probe revision", error))?;
        if indexed != full_index.index {
            return Err(ServiceError::new(
                "facade ABI probe full index differs from the committed revision",
            ));
        }
        self.dirty_witness_contract_seen = true;
        Ok(true)
    }

    pub fn query_worker(&self) -> Result<Self, ServiceError> {
        let socket_path =
            self.config.broker_socket.as_ref().ok_or_else(|| {
                ServiceError::new("concurrent query workers require external broker")
            })?;
        let store = Store::open(self.store.path())
            .map_err(|error| ServiceError::context("open query-worker store", error))?;
        let socket = SeqPacket::connect(socket_path)
            .map_err(|error| ServiceError::context("connect query-worker broker", error))?;
        let broker = BrokerClient::connect_existing(
            socket,
            self.manager_store_uuid,
            self.manager_session_id,
        )
        .map_err(|error| ServiceError::context("join query-worker broker session", error))?;
        Ok(Self {
            store,
            broker,
            manager_store_uuid: self.manager_store_uuid,
            manager_session_id: self.manager_session_id,
            lease_owner: random_id(),
            config: self.config.clone(),
            dirty_witness_contract_seen: self.dirty_witness_contract_seen,
            worktree_view_handoffs: BTreeMap::new(),
        })
    }

    pub fn initialize(
        &mut self,
        source_path: &Path,
        options: &InitializeOptions,
    ) -> Result<InitializedWatch, ServiceError> {
        let canonical_source = fs::canonicalize(source_path)
            .map_err(|error| ServiceError::context("canonicalize initialize source", error))?;
        reject_managed_descendant(&canonical_source, &self.config.managed_snapshot_directory)?;
        let source = OpenedSubvolume::open(&canonical_source)
            .map_err(|error| ServiceError::context("open initialize source", error))?;
        if source.subvolume.is_top_level() {
            return Err(ServiceError::new(
                "cannot initialize the Btrfs top-level root",
            ));
        }
        let destination_name = operation_name(b"cut-initialize-");
        let destination_path = self
            .config
            .managed_snapshot_directory
            .join(std::ffi::OsString::from_vec(destination_name.clone()));
        let request = InitializeRequest {
            fs_uuid: source.filesystem.fs_uuid,
            source_subvol_uuid: source.subvolume.uuid,
            source_path: canonical_source.as_os_str().as_bytes().to_vec(),
            reserved_snapshot_path: destination_path.as_os_str().as_bytes().to_vec(),
            principal: options.principal.clone(),
            permissions: options.permissions,
            requester_uid: options.requester_uid,
            requester_gid: options.requester_gid,
            lease_owner: self.lease_owner,
            now_ns: options.now_ns,
            lease_expires_ns: lease_expiry(options.now_ns, self.config.lease_ns)?,
        };
        let reservation = self
            .store
            .reserve_initialize(&request)
            .map_err(|error| ServiceError::context("reserve initialize", error))?;
        self.store
            .start_initialize_filesystem_effect(&reservation, self.lease_owner, options.now_ns)
            .map_err(|error| ServiceError::context("start initialize effect", error))?;

        let snapshot = self.create_snapshot(
            &source,
            &destination_name,
            reservation.operation_id,
            reservation.operation_fence,
            true,
            options.now_ns,
        )?;
        if self.config.fault_after_initialize_snapshot {
            return Err(ServiceError::new(
                "injected failure after initialize snapshot effect",
            ));
        }
        let identity = snapshot_identity(
            &snapshot,
            destination_path.as_os_str().as_bytes().to_vec(),
            options.now_ns,
        );
        let recorded = self
            .store
            .record_initialize_snapshot(&reservation, self.lease_owner, &identity, options.now_ns)
            .map_err(|error| ServiceError::context("record initialize snapshot", error))?;
        reject_nested_subvolumes(&destination_path)?;
        let snapshot_fd = OpenedSubvolume::open(&destination_path)
            .map_err(|error| ServiceError::context("reopen initialize snapshot", error))?;
        let expected =
            ExpectedSubvolume::from_observed(&snapshot_fd.filesystem, &snapshot_fd.subvolume);
        if expected != snapshot {
            return Err(ServiceError::new(
                "initialize snapshot identity changed before indexing",
            ));
        }
        let full_index = self.broker_full_index(&expected, snapshot_fd.as_fd())?;
        self.dirty_witness_contract_seen |= full_index.dirty_witness_contract;
        self.store
            .publish_initial_checkpoint(
                &reservation,
                self.lease_owner,
                &recorded,
                &full_index.index,
                options.now_ns,
            )
            .map_err(|error| ServiceError::context("publish initial checkpoint", error))
    }

    pub fn changes(&mut self, options: &ChangesOptions) -> Result<PublishedCut, ServiceError> {
        let admission_expiry = lease_expiry(options.now_ns, self.config.lease_ns)?;
        if let Some(admission) = self
            .store
            .admit_planned_cut(
                options.watch_id,
                options.authorization_id,
                self.manager_session_id,
                "query",
                options.now_ns,
                admission_expiry,
            )
            .map_err(|error| ServiceError::context("join planned cut", error))?
        {
            return self.wait_for_cut_admission(&admission, options.now_ns, admission_expiry);
        }
        let (live_path, _base_path) = self.watch_paths(options.watch_id)?;
        let live = OpenedSubvolume::open(&live_path)
            .map_err(|error| ServiceError::context("open watched subvolume", error))?;
        let destination_name = operation_name(b"cut-");
        let destination_path = self
            .config
            .managed_snapshot_directory
            .join(std::ffi::OsString::from_vec(destination_name.clone()));
        let request = CutRequest {
            watch_id: options.watch_id,
            authorization_id: options.authorization_id,
            reserved_snapshot_path: destination_path.as_os_str().as_bytes().to_vec(),
            requester_uid: options.requester_uid,
            requester_gid: options.requester_gid,
            lease_owner: self.lease_owner,
            now_ns: options.now_ns,
            lease_expires_ns: lease_expiry(options.now_ns, self.config.lease_ns)?,
        };
        let reservation = match self.store.reserve_cut(&request) {
            Ok(reservation) => reservation,
            Err(reserve_error) => {
                if let Some(admission) = self
                    .store
                    .admit_planned_cut(
                        options.watch_id,
                        options.authorization_id,
                        self.manager_session_id,
                        "query",
                        options.now_ns,
                        admission_expiry,
                    )
                    .map_err(|error| ServiceError::context("retry cut admission", error))?
                {
                    return self.wait_for_cut_admission(
                        &admission,
                        options.now_ns,
                        admission_expiry,
                    );
                }
                return Err(ServiceError::context("reserve cut", reserve_error));
            }
        };
        let admission = self
            .store
            .admit_planned_cut(
                options.watch_id,
                options.authorization_id,
                self.manager_session_id,
                "query",
                options.now_ns,
                admission_expiry,
            )
            .map_err(|error| ServiceError::context("admit cut leader", error))?
            .ok_or_else(|| ServiceError::new("reserved cut was closed before leader admission"))?;
        if admission.reservation.operation_id != reservation.operation_id {
            return Err(ServiceError::new(
                "leader admission attached to a different cut operation",
            ));
        }
        if reservation.source_subvol_uuid != live.subvolume.uuid {
            return Err(ServiceError::new(
                "watched live subvolume no longer matches the reserved UUID",
            ));
        }
        self.store
            .start_cut_filesystem_effect(&reservation, self.lease_owner, options.now_ns)
            .map_err(|error| ServiceError::context("start cut effect", error))?;
        let target = self.create_snapshot(
            &live,
            &destination_name,
            reservation.operation_id,
            reservation.operation_fence,
            true,
            options.now_ns,
        )?;
        if self.config.fault_after_cut_snapshot {
            return Err(ServiceError::new(
                "injected failure after cut snapshot effect",
            ));
        }
        self.finish_cut(CutCompletion {
            reservation,
            lease_owner: self.lease_owner,
            target: Some(target),
            destination_path,
            recorded: None,
            physical_published: false,
            reuse_staged_spool: false,
            now_ns: options.now_ns,
        })?;
        self.store
            .poll_cut_admission(&admission, options.now_ns)
            .map_err(|error| ServiceError::context("validate fulfilled cut admission", error))?
            .ok_or_else(|| ServiceError::new("published cut admission is still waiting"))
    }

    pub fn historical_changes(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        requester_uid: u32,
        from_snapshot_uuid: [u8; 16],
        to_snapshot_uuid: [u8; 16],
        now_ns: i64,
    ) -> Result<HistoricalChanges, ServiceError> {
        let replayed = self
            .store
            .replay_historical_changes(
                watch_id,
                authorization_id,
                requester_uid,
                from_snapshot_uuid,
                to_snapshot_uuid,
            )
            .map_err(|error| ServiceError::context("replay retained history", error))?;
        if !replayed.fresh_instance {
            return Ok(replayed);
        }
        let admission = match self
            .store
            .claim_historical_comparison(&HistoricalComparisonRequest {
                watch_id,
                authorization_id,
                requester_uid,
                from_snapshot_uuid,
                to_snapshot_uuid,
                lease_owner: self.lease_owner,
                now_ns,
                lease_expires_ns: lease_expiry(now_ns, self.config.lease_ns)?,
            }) {
            Ok(admission) => admission,
            // A fresh result is already a complete, safe answer. A direct job
            // is an optimization available only while both endpoint revisions
            // remain retained and no other worker owns the comparison fence.
            Err(_) => return Ok(replayed),
        };
        let claim = match admission {
            HistoricalComparisonAdmission::Ready(changes) => return Ok(changes),
            HistoricalComparisonAdmission::Claimed(claim) => claim,
        };
        let source_path = self.snapshot_path(claim.from_snapshot_id)?;
        let target_path = self.snapshot_path(claim.to_snapshot_id)?;
        let source = OpenedSubvolume::open(&source_path)
            .map_err(|error| ServiceError::context("open historical source", error))?;
        let target = OpenedSubvolume::open(&target_path)
            .map_err(|error| ServiceError::context("open historical target", error))?;
        let source_expected =
            ExpectedSubvolume::from_observed(&source.filesystem, &source.subvolume);
        let target_expected =
            ExpectedSubvolume::from_observed(&target.filesystem, &target.subvolume);
        if source_expected.subvolume_uuid != claim.from_snapshot_uuid
            || target_expected.subvolume_uuid != claim.to_snapshot_uuid
            || source_expected.filesystem_uuid != target_expected.filesystem_uuid
        {
            return Err(ServiceError::new(
                "historical comparison endpoints changed after admission",
            ));
        }
        let spool_path = self.config.spool_directory.join(format!(
            "historical-{}-{}.part",
            claim.comparison_id, claim.lease_fence
        ));
        let mut spool = create_private_spool(&spool_path)?;
        let comparison = self
            .broker
            .changed_objects(
                &ChangedObjectsExecution {
                    parent: source_expected,
                    target: target_expected.clone(),
                    output_owner_uid: unsafe { libc::geteuid() },
                    max_output_bytes: self.config.max_manifest_bytes,
                },
                source.as_fd(),
                target.as_fd(),
                spool.as_fd(),
            )
            .map_err(|error| ServiceError::context("compare historical snapshots", error))?;
        spool
            .seek(SeekFrom::Start(0))
            .map_err(|error| ServiceError::context("rewind historical manifest", error))?;
        let mut bytes = Vec::with_capacity(comparison.output_bytes as usize);
        (&mut spool)
            .take(comparison.output_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| ServiceError::context("read historical manifest", error))?;
        if bytes.len() as u64 != comparison.output_bytes
            || hash_bytes(&bytes) != comparison.manifest_hash
        {
            return Err(ServiceError::new(
                "historical manifest failed its broker length/hash",
            ));
        }
        let parsed = parse_kernel_changed_objects(&bytes)
            .map_err(|error| ServiceError::context("parse historical manifest", error))?;
        self.dirty_witness_contract_seen |= parsed.dirty_witness_contract;
        let required: BTreeSet<_> = parsed
            .manifest
            .objects
            .values()
            .filter(|change| {
                !change.is_deleted()
                    && (change.change_mask & (CHANGE_CREATED | CHANGE_INODE | CHANGE_XATTR) != 0)
            })
            .map(|change| change.ino)
            .collect();
        let target_objects =
            self.resolve_target_objects(&parsed, &target_expected, target.as_fd(), &required)?;
        let changes = self
            .store
            .publish_historical_comparison(&claim, &parsed.manifest, &target_objects, now_ns)
            .map_err(|error| ServiceError::context("publish historical comparison", error))?;
        drop(spool);
        let _ = fs::remove_file(spool_path);
        Ok(changes)
    }

    fn wait_for_cut_admission(
        &self,
        admission: &CutAdmission,
        admitted_ns: i64,
        expires_ns: i64,
    ) -> Result<PublishedCut, ServiceError> {
        let started = std::time::Instant::now();
        loop {
            let elapsed_ns = i64::try_from(started.elapsed().as_nanos()).unwrap_or(i64::MAX);
            let now_ns = admitted_ns.saturating_add(elapsed_ns);
            if let Some(published) = self
                .store
                .poll_cut_admission(admission, now_ns)
                .map_err(|error| ServiceError::context("wait for shared cut", error))?
            {
                return Ok(published);
            }
            if now_ns >= expires_ns {
                return Err(ServiceError::new("shared cut admission timed out"));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn finish_cut(&mut self, completion: CutCompletion) -> Result<PublishedCut, ServiceError> {
        let reservation = &completion.reservation;
        let target = completion
            .target
            .as_ref()
            .ok_or_else(|| ServiceError::new("cut completion omits target identity"))?;
        let recorded = if let Some(recorded) = completion.recorded {
            recorded
        } else {
            let identity = snapshot_identity(
                target,
                completion.destination_path.as_os_str().as_bytes().to_vec(),
                completion.now_ns,
            );
            self.store
                .record_cut_snapshot(
                    reservation,
                    completion.lease_owner,
                    &identity,
                    completion.now_ns,
                )
                .map_err(|error| ServiceError::context("record cut snapshot", error))?
        };
        if !completion.physical_published {
            self.store
                .publish_validated_physical_cut(
                    reservation,
                    completion.lease_owner,
                    &recorded,
                    completion.now_ns,
                )
                .map_err(|error| ServiceError::context("publish physical cut", error))?;
        }
        let base_path = self.snapshot_path(reservation.base_snapshot_id)?;
        let base = OpenedSubvolume::open(&base_path)
            .map_err(|error| ServiceError::context("open base snapshot", error))?;
        let target_fd = OpenedSubvolume::open(&completion.destination_path)
            .map_err(|error| ServiceError::context("reopen target snapshot", error))?;
        let parent_expected = ExpectedSubvolume::from_observed(&base.filesystem, &base.subvolume);
        let target_expected =
            ExpectedSubvolume::from_observed(&target_fd.filesystem, &target_fd.subvolume);
        if parent_expected.subvolume_uuid != self.snapshot_uuid(reservation.base_snapshot_id)?
            || target_expected != *target
        {
            return Err(ServiceError::new(
                "comparison endpoints do not match reserved immutable snapshots",
            ));
        }
        let spool_name = format!(
            "manifest-{}-{}.part",
            hex_id(&reservation.operation_id),
            reservation.operation_fence
        );
        let spool_path = self.config.spool_directory.join(spool_name);
        let staged_manifest = if completion.reuse_staged_spool {
            load_staged_manifest(&spool_path, self.config.max_manifest_bytes)?
        } else {
            None
        };
        let inject_incremental_failure =
            std::mem::take(&mut self.config.fault_next_incremental_comparison);
        let inject_manifest_stage_failure =
            std::mem::take(&mut self.config.fault_after_manifest_stage);
        let incremental = (|| -> Result<PublishedCut, ServiceError> {
            if inject_incremental_failure {
                return Err(ServiceError::new(
                    "injected terminal incremental comparison failure",
                ));
            }
            let parsed = if let Some(parsed) = staged_manifest {
                parsed
            } else {
                let mut spool = create_private_spool(&spool_path)?;
                let comparison = self
                    .broker
                    .changed_objects(
                        &ChangedObjectsExecution {
                            parent: parent_expected,
                            target: target_expected.clone(),
                            output_owner_uid: unsafe { libc::geteuid() },
                            max_output_bytes: self.config.max_manifest_bytes,
                        },
                        base.as_fd(),
                        target_fd.as_fd(),
                        spool.as_fd(),
                    )
                    .map_err(|error| ServiceError::context("compare immutable snapshots", error))?;
                spool
                    .seek(SeekFrom::Start(0))
                    .map_err(|error| ServiceError::context("rewind manifest", error))?;
                let mut bytes = Vec::with_capacity(comparison.output_bytes as usize);
                (&mut spool)
                    .take(comparison.output_bytes + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| {
                        ServiceError::context("read changed-object manifest", error)
                    })?;
                if bytes.len() as u64 != comparison.output_bytes
                    || hash_bytes(&bytes) != comparison.manifest_hash
                {
                    return Err(ServiceError::new(
                        "spooled changed-object manifest failed its broker hash",
                    ));
                }
                let parsed = parse_kernel_changed_objects(&bytes).map_err(|error| {
                    ServiceError::context("parse changed-object manifest", error)
                })?;
                write_manifest_stage_trailer(
                    &mut spool,
                    comparison.output_bytes,
                    comparison.manifest_hash,
                )?;
                parsed
            };
            // V2 proves the target remains boundary-free from the accepted
            // boundary-free base using mandatory DIR_INDEX transition
            // records. Legacy kernels have no such contract, so retain the
            // fail-closed namespace scan for them.
            if parsed.target_objects.is_none() {
                reject_nested_subvolumes(&completion.destination_path)?;
            }
            self.dirty_witness_contract_seen |= parsed.dirty_witness_contract;
            let required: BTreeSet<_> = parsed
                .manifest
                .objects
                .values()
                .filter(|change| {
                    !change.is_deleted()
                        && (change.change_mask & (CHANGE_CREATED | CHANGE_INODE | CHANGE_XATTR)
                            != 0)
                })
                .map(|change| change.ino)
                .collect();
            if inject_manifest_stage_failure {
                return Err(ServiceError::new(
                    "injected failure after durable manifest staging",
                ));
            }
            let target_objects = self.resolve_target_objects(
                &parsed,
                &target_expected,
                target_fd.as_fd(),
                &required,
            )?;
            let published = self
                .store
                .publish_adjacent_delta(
                    reservation,
                    completion.lease_owner,
                    &recorded,
                    &parsed.manifest,
                    &target_objects,
                    completion.now_ns,
                )
                .map_err(|error| ServiceError::context("publish indexed cut", error))?;
            // The cut is committed. Startup spool reconciliation can remove a
            // leftover file, so cleanup failure must not manufacture a false
            // publication failure and trigger a second publication attempt.
            let _ = fs::remove_file(&spool_path);
            Ok(published)
        })();
        match incremental {
            Ok(published) => Ok(published),
            Err(error) if inject_manifest_stage_failure => Err(error),
            Err(incremental_error) => {
                discard_private_spool(&spool_path)?;
                // The legacy full-index encoding does not certify nested
                // subvolume boundaries. This scan is redundant for v2, but
                // is required before accepting a possible legacy fallback.
                reject_nested_subvolumes(&completion.destination_path)?;
                let full_index = self
                    .broker_full_index(&target_expected, target_fd.as_fd())
                    .map_err(|fallback| {
                        ServiceError::new(format!(
                            "incremental cut failed: {incremental_error}; full-index fallback failed: {fallback}"
                        ))
                    })?;
                self.dirty_witness_contract_seen |= full_index.dirty_witness_contract;
                self.store
                    .publish_full_fresh_checkpoint(
                        reservation,
                        completion.lease_owner,
                        &recorded,
                    &full_index.index,
                        completion.now_ns,
                    )
                    .map_err(|fallback| {
                        ServiceError::new(format!(
                            "incremental cut failed: {incremental_error}; full-fresh publication failed: {fallback}"
                        ))
                    })
            }
        }
    }

    pub fn garbage_collect(&mut self, now_ns: i64, limit: usize) -> Result<usize, ServiceError> {
        self.store
            .expire_retention_leases(now_ns)
            .map_err(|error| ServiceError::context("expire retention leases", error))?;
        self.maintain_history(now_ns)?;
        let reservations = self
            .store
            .reserve_unpinned_snapshot_deletes(
                self.lease_owner,
                now_ns,
                lease_expiry(now_ns, self.config.lease_ns)?,
                limit,
            )
            .map_err(|error| ServiceError::context("reserve snapshot GC", error))?;
        let mut completed = 0;
        for reservation in reservations {
            self.store
                .start_snapshot_delete(&reservation, self.lease_owner, now_ns)
                .map_err(|error| ServiceError::context("start snapshot GC effect", error))?;
            self.execute_or_reconcile_snapshot_delete(
                &reservation,
                self.lease_owner,
                now_ns,
                false,
            )?;
            if self.config.fault_after_snapshot_delete {
                return Err(ServiceError::new(
                    "injected failure after snapshot deletion effect",
                ));
            }
            self.store
                .finish_snapshot_delete(&reservation, self.lease_owner, now_ns)
                .map_err(|error| ServiceError::context("finish snapshot GC", error))?;
            completed += 1;
        }
        Ok(completed)
    }

    pub fn maintain_history(&mut self, now_ns: i64) -> Result<usize, ServiceError> {
        let cutoff_ns = now_ns.saturating_sub(self.config.replay_window_ns);
        let watches = {
            let mut statement = self
                .store
                .connection()
                .prepare(
                    "SELECT id, replay_floor_seq, indexed_seq FROM watches \
                     WHERE state IN ('active', 'blocked') ORDER BY id",
                )
                .map_err(|error| ServiceError::context("prepare history maintenance", error))?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|error| ServiceError::context("query history maintenance", error))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| ServiceError::context("decode history maintenance", error))?;
            rows
        };
        let mut reclaimed = 0_usize;
        for (watch_bytes, current_floor, indexed_sequence) in watches {
            let watch_id = fixed_service_blob(&watch_bytes, "history-maintenance watch ID")?;
            let count_floor = indexed_sequence
                .saturating_sub(self.config.replay_window_cuts)
                .max(current_floor);
            let time_floor: Option<i64> = self
                .store
                .connection()
                .query_row(
                    r#"SELECT max(sequence) FROM operations
                        WHERE watch_id = ?1 AND kind = 'cut' AND state = 'done'
                          AND sequence <= ?2 AND updated_ns <= ?3"#,
                    rusqlite::params![watch_id.as_slice(), count_floor, cutoff_ns],
                    |row| row.get(0),
                )
                .map_err(|error| ServiceError::context("select replay retention floor", error))?;
            let target_floor = time_floor.unwrap_or(current_floor).max(current_floor);
            if target_floor > current_floor {
                reclaimed += self
                    .store
                    .advance_replay_floor(watch_id, target_floor, now_ns, self.lease_owner)
                    .map_err(|error| {
                        ServiceError::context("advance replay retention floor", error)
                    })?;
            }
        }
        Ok(reclaimed)
    }

    fn execute_or_reconcile_snapshot_delete(
        &mut self,
        reservation: &SnapshotDeleteReservation,
        lease_owner: [u8; 16],
        now_ns: i64,
        recovery: bool,
    ) -> Result<(), ServiceError> {
        let path = PathBuf::from(std::ffi::OsString::from_vec(
            reservation.identity.path.clone(),
        ));
        if path.parent() != Some(self.config.managed_snapshot_directory.as_path()) {
            return Err(ServiceError::new(
                "snapshot GC candidate is outside the managed directory",
            ));
        }
        let destination = File::open(&self.config.managed_snapshot_directory).map_err(|error| {
            ServiceError::context("open managed directory for snapshot GC", error)
        })?;
        let stored = recovery
            && self
                .broker
                .has_stored_effect(
                    crate::broker::Opcode::DeleteSnapshot,
                    reservation.operation_id,
                    reservation.operation_fence,
                )
                .map_err(|error| {
                    ServiceError::context("inspect stored snapshot-delete effect", error)
                })?;
        let deleted_uuid = if stored {
            self.broker
                .reconcile_snapshot_delete(
                    reservation.operation_id,
                    reservation.operation_fence,
                    destination.as_fd(),
                )
                .map_err(|error| {
                    ServiceError::context("reconcile managed snapshot deletion", error)
                })?
                .deleted_subvolume_uuid
        } else {
            let name = path
                .file_name()
                .ok_or_else(|| ServiceError::new("snapshot GC path has no basename"))?
                .as_bytes()
                .to_vec();
            let target = OpenedSubvolume::open(&path)
                .map_err(|error| ServiceError::context("open snapshot GC target", error))?;
            let expected = ExpectedSubvolume::from_observed(&target.filesystem, &target.subvolume);
            verify_recorded_snapshot(&reservation.identity, &expected)?;
            let destination_parent = ExpectedManagedDirectory::from_observed(destination.as_fd())
                .map_err(|error| {
                ServiceError::context("inspect snapshot GC directory", error)
            })?;
            let receipt = ReceiptRequest {
                id: random_id(),
                manager_store_uuid: self.manager_store_uuid,
                manager_session_id: self.manager_session_id,
                operation_id: reservation.operation_id,
                operation_fence: reservation.operation_fence,
                effect_kind: EffectKind::SnapshotDelete,
                filesystem_uuid: reservation.identity.fs_uuid,
                target_locator_hash: [0; 32],
                effect_arguments_hash: [0; 32],
                boot_id: self.config.boot_id,
                started_ns: now_ns,
            };
            let mut execution = SnapshotDeleteExecution {
                receipt,
                target: expected,
                destination_parent,
                destination_name: name,
            };
            execution.receipt.target_locator_hash = snapshot_target_locator_hash(
                &execution.destination_parent,
                &execution.destination_name,
            );
            execution.receipt.effect_arguments_hash = snapshot_delete_effect_hash(&execution);
            self.broker
                .delete_snapshot(&execution, destination.as_fd())
                .map_err(|error| ServiceError::context("delete managed snapshot", error))?
                .deleted_subvolume_uuid
        };
        if deleted_uuid != reservation.identity.subvol_uuid {
            return Err(ServiceError::new(
                "snapshot-delete receipt returned the wrong subvolume UUID",
            ));
        }
        self.store
            .record_snapshot_delete_durable(reservation, lease_owner, now_ns)
            .map_err(|error| ServiceError::context("record durable snapshot GC", error))
    }

    pub fn provision_sanitized_worktree_policy(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        destination_root: &Path,
        now_ns: i64,
    ) -> Result<WorktreePolicy, ServiceError> {
        let root = OpenedSubvolume::open(destination_root)
            .map_err(|error| ServiceError::context("open Worktree destination root", error))?;
        let canonical_root = fs::canonicalize(destination_root)
            .map_err(|error| ServiceError::context("canonicalize Worktree policy root", error))?;
        let mut policy = WorktreePolicy {
            policy_id: random_id(),
            destination_fs_uuid: root.filesystem.fs_uuid,
            destination_root_subvol_uuid: root.subvolume.uuid,
            destination_root_path: canonical_root.as_os_str().as_bytes().to_vec(),
            destination_root_generation: inode_generation(root.as_fd()).map_err(|error| {
                ServiceError::context("read Worktree policy-root inode generation", error)
            })?,
            metadata_policy: "sanitized-private-user-tree".to_owned(),
            allow_idmapped: false,
            policy_hash: [0; 32],
        };
        policy.policy_hash = worktree_policy_hash(&policy);
        self.store
            .provision_worktree_policy(watch_id, authorization_id, &policy, now_ns)
            .map_err(|error| ServiceError::context("provision Worktree policy", error))?;
        Ok(policy)
    }

    pub fn worktree(
        &mut self,
        policy: &WorktreePolicy,
        options: &WorktreeOptions,
    ) -> Result<PublishedWorktree, ServiceError> {
        let anchor = self.changes(&ChangesOptions {
            watch_id: options.watch_id,
            authorization_id: options.authorization_id,
            requester_uid: options.requester_uid,
            requester_gid: options.requester_gid,
            now_ns: options.now_ns,
        })?;
        let root = OpenedSubvolume::open(&options.destination_root)
            .map_err(|error| ServiceError::context("reopen Worktree policy root", error))?;
        if root.filesystem.fs_uuid != policy.destination_fs_uuid
            || root.subvolume.uuid != policy.destination_root_subvol_uuid
            || inode_generation(root.as_fd()).map_err(|error| {
                ServiceError::context("recheck Worktree policy-root inode generation", error)
            })? != policy.destination_root_generation
        {
            return Err(ServiceError::new(
                "Worktree destination policy root changed",
            ));
        }
        let canonical_root = fs::canonicalize(&options.destination_root)
            .map_err(|error| ServiceError::context("canonicalize Worktree root", error))?;
        let canonical_parent = fs::canonicalize(&options.destination_parent)
            .map_err(|error| ServiceError::context("canonicalize Worktree parent", error))?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(ServiceError::new(
                "Worktree destination parent is outside its policy root",
            ));
        }
        let destination_relative_parent =
            relative_directory_bytes(&canonical_root, &canonical_parent)?;
        let destination = File::open(&options.destination_parent)
            .map_err(|error| ServiceError::context("open Worktree destination parent", error))?;
        let destination_identity = ExpectedManagedDirectory::from_observed(destination.as_fd())
            .map_err(|error| ServiceError::context("inspect Worktree destination", error))?;
        let reservation = ExpectedReservation::from_observed(
            destination.as_fd(),
            &options.reservation_name,
            options.requester_uid,
            options.reservation_nonce,
        )
        .map_err(|error| ServiceError::context("inspect Worktree reservation", error))?;
        let reservation_file = File::open(options.destination_parent.join(
            std::ffi::OsString::from_vec(options.reservation_name.clone()),
        ))
        .map_err(|error| ServiceError::context("open Worktree reservation generation", error))?;
        let destination_parent_generation = inode_generation(destination.as_fd())
            .map_err(|error| ServiceError::context("read destination inode generation", error))?;
        let reservation_generation = inode_generation(reservation_file.as_fd())
            .map_err(|error| ServiceError::context("read reservation inode generation", error))?;
        let staging_name = operation_name(b"worktree-");
        let staging_path = self
            .config
            .managed_snapshot_directory
            .join(std::ffi::OsString::from_vec(staging_name.clone()));
        let final_path = canonical_parent.join(std::ffi::OsString::from_vec(
            options.destination_name.clone(),
        ));
        let request = WorktreeRequest {
            watch_id: options.watch_id,
            authorization_id: options.authorization_id,
            policy: policy.clone(),
            staged_path: staging_path.as_os_str().as_bytes().to_vec(),
            final_path: final_path.as_os_str().as_bytes().to_vec(),
            destination_parent_subvol_uuid: root.subvolume.uuid,
            destination_parent_ino: destination_identity.inode,
            destination_parent_generation,
            destination_name: options.destination_name.clone(),
            reservation_name: options.reservation_name.clone(),
            reservation_ino: reservation.inode,
            reservation_generation,
            reservation_nonce: options.reservation_nonce,
            requester_uid: options.requester_uid,
            requester_gid: options.requester_gid,
            lease_owner: self.lease_owner,
            now_ns: options.now_ns,
            lease_expires_ns: lease_expiry(options.now_ns, self.config.lease_ns)?,
        };
        let reserved = self
            .store
            .reserve_worktree(&request)
            .map_err(|error| ServiceError::context("reserve Worktree", error))?;
        if reserved.seed_snapshot_id != anchor.snapshot_id
            || reserved.seed_revision_id != anchor.revision_id
        {
            return Err(ServiceError::new(
                "Worktree seed differs from its anchor cut",
            ));
        }
        self.store
            .start_worktree_effect(&reserved, self.lease_owner, options.now_ns)
            .map_err(|error| ServiceError::context("start Worktree effect", error))?;
        let seed_path = self.snapshot_path(reserved.seed_snapshot_id)?;
        let seed = OpenedSubvolume::open(&seed_path)
            .map_err(|error| ServiceError::context("open Worktree seed snapshot", error))?;
        let staged = self.create_snapshot(
            &seed,
            &staging_name,
            derived_effect_id(reserved.operation_id, b"create"),
            reserved.operation_fence,
            false,
            options.now_ns,
        )?;
        if self.config.fault_after_worktree_create {
            return Err(ServiceError::new(
                "injected failure after Worktree clone effect",
            ));
        }
        self.store
            .record_created_worktree(
                &reserved,
                self.lease_owner,
                staged.subvolume_uuid,
                staged.parent_uuid,
                options.now_ns,
            )
            .map_err(|error| ServiceError::context("record writable clone", error))?;
        let topology_fence = self
            .store
            .prepare_worktree_publication(
                &reserved,
                self.lease_owner,
                final_path.as_os_str().as_bytes(),
                options.now_ns,
                lease_expiry(options.now_ns, self.config.lease_ns)?,
            )
            .map_err(|error| ServiceError::context("prepare Worktree publication", error))?;
        let staging_parent = File::open(&self.config.managed_snapshot_directory)
            .map_err(|error| ServiceError::context("open Worktree staging parent", error))?;
        let receipt = ReceiptRequest {
            id: random_id(),
            manager_store_uuid: self.manager_store_uuid,
            manager_session_id: self.manager_session_id,
            operation_id: reserved.operation_id,
            operation_fence: reserved.operation_fence,
            effect_kind: EffectKind::WorktreeRename,
            filesystem_uuid: policy.destination_fs_uuid,
            target_locator_hash: [0; 32],
            effect_arguments_hash: [0; 32],
            boot_id: self.config.boot_id,
            started_ns: options.now_ns,
        };
        let mut execution = WorktreeRenameExecution {
            receipt,
            worktree: staged,
            staging_parent: ExpectedManagedDirectory::from_observed(staging_parent.as_fd())
                .map_err(|error| ServiceError::context("inspect Worktree staging", error))?,
            staging_name,
            destination_parent: destination_identity,
            destination_root: ExpectedSubvolume::from_observed(&root.filesystem, &root.subvolume),
            destination_root_directory: ExpectedManagedDirectory::from_observed(root.as_fd())
                .map_err(|error| ServiceError::context("inspect Worktree policy root", error))?,
            destination_relative_parent,
            destination_name: options.destination_name.clone(),
            reservation,
            authorization_hash: policy.policy_hash,
        };
        execution.receipt.target_locator_hash = snapshot_target_locator_hash(
            &execution.destination_parent,
            &execution.destination_name,
        );
        execution.receipt.effect_arguments_hash = worktree_rename_effect_hash(&execution);
        let pending_view = self
            .config
            .experimental_dirty_witness_verified
            .then(|| {
                PendingNamespaceMonitor::arm(&options.destination_parent, &options.destination_name)
            })
            .transpose()
            .ok()
            .flatten();
        self.broker
            .publish_worktree(&execution, staging_parent.as_fd(), root.as_fd())
            .map_err(|error| ServiceError::context("publish Worktree", error))?;
        let completed_view = pending_view.and_then(|pending| {
            pending
                .complete(
                    policy.destination_fs_uuid,
                    execution.worktree.subvolume_uuid,
                )
                .ok()
        });
        if self.config.fault_after_worktree_publish {
            return Err(ServiceError::new(
                "injected failure after Worktree publication effect",
            ));
        }
        let tracked = self
            .store
            .publish_worktree(
                &reserved,
                self.lease_owner,
                topology_fence,
                execution.worktree.subvolume_uuid,
                options.now_ns,
            )
            .map_err(|error| ServiceError::context("publish Worktree metadata", error))?;
        if let Some(monitor) = completed_view {
            if let Ok(activation) = self.store.activate_snapshot_facade(
                tracked.watch_id,
                tracked.grant_id,
                monitor.binding(),
            ) {
                match self
                    .store
                    .finalize_proved_worktree_seed(&activation, monitor.binding())
                {
                    Ok(seed) => {
                        self.worktree_view_handoffs.insert(
                            tracked.watch_id,
                            WorktreeViewHandoff {
                                authorization_id: tracked.grant_id,
                                activation,
                                monitor,
                                snapshot_uuid: seed.snapshot_uuid,
                            },
                        );
                    }
                    Err(_) => {
                        let _ = self.store.invalidate_snapshot_facade(&activation);
                    }
                }
            }
        }
        Ok(PublishedWorktree {
            worktree_id: reserved.worktree_id,
            watch_id: tracked.watch_id,
            grant_id: tracked.grant_id,
            subvol_uuid: execution.worktree.subvolume_uuid,
            path: final_path,
            seed_revision_id: reserved.seed_revision_id,
            seed_snapshot_id: reserved.seed_snapshot_id,
        })
    }

    fn create_snapshot(
        &mut self,
        source: &OpenedSubvolume,
        destination_name: &[u8],
        operation_id: [u8; 16],
        operation_fence: i64,
        readonly: bool,
        now_ns: i64,
    ) -> Result<ExpectedSubvolume, ServiceError> {
        let destination = File::open(&self.config.managed_snapshot_directory)
            .map_err(|error| ServiceError::context("open managed snapshot directory", error))?;
        let destination_parent = ExpectedManagedDirectory::from_observed(destination.as_fd())
            .map_err(|error| ServiceError::context("inspect managed snapshot directory", error))?;
        let receipt = ReceiptRequest {
            id: random_id(),
            manager_store_uuid: self.manager_store_uuid,
            manager_session_id: self.manager_session_id,
            operation_id,
            operation_fence,
            effect_kind: EffectKind::SnapshotCreate,
            filesystem_uuid: source.filesystem.fs_uuid,
            target_locator_hash: [0; 32],
            effect_arguments_hash: [0; 32],
            boot_id: self.config.boot_id,
            started_ns: now_ns,
        };
        let mut execution = SnapshotCreateExecution {
            receipt,
            source: ExpectedSubvolume::from_observed(&source.filesystem, &source.subvolume),
            destination_parent,
            destination_name: destination_name.to_vec(),
            readonly,
        };
        execution.receipt.target_locator_hash = snapshot_target_locator_hash(
            &execution.destination_parent,
            &execution.destination_name,
        );
        execution.receipt.effect_arguments_hash = snapshot_create_effect_hash(&execution);
        self.broker
            .create_snapshot(&execution, source.as_fd(), destination.as_fd())
            .map(|result| result.snapshot)
            .map_err(|error| ServiceError::context("create snapshot", error))
    }

    fn broker_full_index(
        &self,
        expected: &ExpectedSubvolume,
        snapshot: BorrowedFd<'_>,
    ) -> Result<FullIndexResult, ServiceError> {
        let path = self
            .config
            .spool_directory
            .join(std::ffi::OsString::from_vec(operation_name(b"full-index-")));
        let mut output = create_private_spool(&path)?;
        let result = self
            .broker
            .full_index(
                expected,
                snapshot,
                output.as_fd(),
                unsafe { libc::geteuid() },
                self.config.max_manifest_bytes,
            )
            .map_err(|error| ServiceError::context("request full index", error))?;
        let bytes = read_broker_output(&mut output, &result)?;
        drop(output);
        fs::remove_file(&path)
            .map_err(|error| ServiceError::context("remove full-index spool", error))?;
        let index = decode_index(&bytes)
            .map_err(|error| ServiceError::context("decode full index", error))?;
        reject_fscrypt_index(&index)?;
        Ok(FullIndexResult {
            index,
            dirty_witness_contract: false,
        })
    }

    fn broker_target_objects(
        &self,
        expected: &ExpectedSubvolume,
        snapshot: BorrowedFd<'_>,
        inodes: &BTreeSet<u64>,
    ) -> Result<BTreeMap<u64, Object>, ServiceError> {
        let path = self
            .config
            .spool_directory
            .join(std::ffi::OsString::from_vec(operation_name(
                b"target-index-",
            )));
        let mut output = create_private_spool(&path)?;
        let result = self
            .broker
            .target_objects(
                expected,
                snapshot,
                output.as_fd(),
                unsafe { libc::geteuid() },
                self.config.max_manifest_bytes,
                inodes,
            )
            .map_err(|error| ServiceError::context("request target objects", error))?;
        let bytes = read_broker_output(&mut output, &result)?;
        drop(output);
        fs::remove_file(&path)
            .map_err(|error| ServiceError::context("remove target-index spool", error))?;
        decode_objects(&bytes)
            .map_err(|error| ServiceError::context("decode target objects", error))
    }

    fn resolve_target_objects(
        &self,
        parsed: &ParsedKernelChangedObjects,
        expected: &ExpectedSubvolume,
        snapshot: BorrowedFd<'_>,
        required: &BTreeSet<u64>,
    ) -> Result<BTreeMap<u64, Object>, ServiceError> {
        let objects: BTreeMap<_, _> = if let Some(stream_objects) = &parsed.target_objects {
            required
                .iter()
                .map(|ino| {
                    stream_objects
                        .get(ino)
                        .cloned()
                        .map(|object| (*ino, object))
                        .ok_or_else(|| {
                            ServiceError::new(format!(
                                "changed-objects v2 omitted required target inode {ino}"
                            ))
                        })
                })
                .collect::<Result<_, _>>()?
        } else {
            self.broker_target_objects(expected, snapshot, required)?
        };
        if let Some(object) = objects
            .values()
            .find(|object| object.privilege_flags & PRIVILEGE_FSCRYPT != 0)
        {
            return Err(ServiceError::new(format!(
                "immutable snapshot contains fscrypt inode {}",
                object.ino
            )));
        }
        Ok(objects)
    }

    fn watch_paths(&self, watch_id: [u8; 16]) -> Result<(PathBuf, PathBuf), ServiceError> {
        self.store
            .connection()
            .query_row(
                "SELECT w.live_path, s.path FROM watches w \
                 JOIN snapshots s ON s.id = w.last_cut_snapshot_id \
                 WHERE w.id = ?1 AND w.state = 'active' AND s.physical_state = 'present'",
                [watch_id.as_slice()],
                |row| {
                    let live: Vec<u8> = row.get(0)?;
                    let base: Vec<u8> = row.get(1)?;
                    Ok((
                        PathBuf::from(std::ffi::OsString::from_vec(live)),
                        PathBuf::from(std::ffi::OsString::from_vec(base)),
                    ))
                },
            )
            .map_err(|error| ServiceError::context("load watch paths", error))
    }

    fn snapshot_uuid(&self, snapshot_id: i64) -> Result<[u8; 16], ServiceError> {
        let bytes: Vec<u8> = self
            .store
            .connection()
            .query_row(
                "SELECT subvol_uuid FROM snapshots WHERE id = ?1 AND physical_state = 'present'",
                [snapshot_id],
                |row| row.get(0),
            )
            .map_err(|error| ServiceError::context("load snapshot UUID", error))?;
        bytes
            .try_into()
            .map_err(|_| ServiceError::new("stored snapshot UUID has invalid length"))
    }

    fn snapshot_path(&self, snapshot_id: i64) -> Result<PathBuf, ServiceError> {
        let bytes: Vec<u8> = self
            .store
            .connection()
            .query_row(
                "SELECT path FROM snapshots WHERE id = ?1 AND physical_state = 'present'",
                [snapshot_id],
                |row| row.get(0),
            )
            .map_err(|error| ServiceError::context("load snapshot path", error))?;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
}

fn snapshot_identity(
    snapshot: &ExpectedSubvolume,
    path: Vec<u8>,
    created_ns: i64,
) -> SnapshotIdentity {
    SnapshotIdentity {
        fs_uuid: snapshot.filesystem_uuid,
        subvol_uuid: snapshot.subvolume_uuid,
        parent_uuid: snapshot.parent_uuid,
        received_uuid: snapshot.received_uuid,
        root_id: snapshot.root_id,
        ctransid: snapshot.ctransid,
        otransid: snapshot.otransid,
        path,
        readonly: snapshot.readonly,
        created_ns,
    }
}

fn verify_recorded_snapshot(
    recorded: &SnapshotIdentity,
    observed: &ExpectedSubvolume,
) -> Result<(), ServiceError> {
    if recorded.fs_uuid != observed.filesystem_uuid
        || recorded.subvol_uuid != observed.subvolume_uuid
        || recorded.parent_uuid != observed.parent_uuid
        || recorded.received_uuid != observed.received_uuid
        || recorded.root_id != observed.root_id
        || recorded.ctransid != observed.ctransid
        || recorded.otransid != observed.otransid
        || recorded.readonly != observed.readonly
    {
        return Err(ServiceError::new(
            "managed snapshot no longer matches its recorded identity",
        ));
    }
    Ok(())
}

fn validate_private_directory(path: &Path) -> Result<(), ServiceError> {
    let metadata = fs::metadata(path)
        .map_err(|error| ServiceError::context(format!("stat {}", path.display()), error))?;
    if !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
        return Err(ServiceError::new(format!(
            "{} must be an existing private directory",
            path.display()
        )));
    }
    Ok(())
}

fn reject_managed_descendant(source: &Path, managed: &Path) -> Result<(), ServiceError> {
    let source = fs::canonicalize(source)
        .map_err(|error| ServiceError::context("canonicalize source", error))?;
    let managed = fs::canonicalize(managed)
        .map_err(|error| ServiceError::context("canonicalize managed directory", error))?;
    if managed.starts_with(&source) {
        return Err(ServiceError::new(
            "managed snapshot directory must not be inside the watched source",
        ));
    }
    Ok(())
}

fn reject_nested_subvolumes(root: &Path) -> Result<(), ServiceError> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| ServiceError::context("scan immutable snapshot", error))?
        {
            let entry =
                entry.map_err(|error| ServiceError::context("read snapshot entry", error))?;
            let file_type = entry
                .file_type()
                .map_err(|error| ServiceError::context("read snapshot entry type", error))?;
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|error| ServiceError::context("stat snapshot directory", error))?;
            if metadata.ino() == crate::btrfs::ROOT_INODE {
                return Err(ServiceError::new(format!(
                    "immutable snapshot contains nested subvolume {}",
                    entry.path().display()
                )));
            }
            pending.push(entry.path());
        }
    }
    Ok(())
}

fn reject_fscrypt_index(index: &Index) -> Result<(), ServiceError> {
    if let Some(object) = index
        .objects
        .values()
        .find(|object| object.privilege_flags & PRIVILEGE_FSCRYPT != 0)
    {
        return Err(ServiceError::new(format!(
            "immutable snapshot contains fscrypt inode {}",
            object.ino
        )));
    }
    Ok(())
}

fn find_directory_by_identity(
    root: &Path,
    wanted_ino: u64,
    wanted_generation: u64,
) -> Result<PathBuf, ServiceError> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let directory = File::open(&path)
            .map_err(|error| ServiceError::context("open destination-policy directory", error))?;
        let metadata = directory
            .metadata()
            .map_err(|error| ServiceError::context("stat destination-policy directory", error))?;
        if metadata.ino() == wanted_ino
            && inode_generation(directory.as_fd()).map_err(|error| {
                ServiceError::context("read destination-policy directory generation", error)
            })? == wanted_generation
        {
            return Ok(path);
        }
        for entry in fs::read_dir(&path)
            .map_err(|error| ServiceError::context("scan destination policy root", error))?
        {
            let entry = entry
                .map_err(|error| ServiceError::context("read destination policy entry", error))?;
            let file_type = entry.file_type().map_err(|error| {
                ServiceError::context("read destination policy entry type", error)
            })?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let child_metadata = entry
                .metadata()
                .map_err(|error| ServiceError::context("stat destination policy child", error))?;
            if child_metadata.ino() == crate::btrfs::ROOT_INODE {
                continue;
            }
            pending.push(entry.path());
        }
    }
    Err(ServiceError::new(
        "Worktree destination parent is no longer beneath its policy root",
    ))
}

fn relative_directory_bytes(root: &Path, child: &Path) -> Result<Vec<u8>, ServiceError> {
    let relative = child
        .strip_prefix(root)
        .map_err(|_| ServiceError::new("Worktree destination parent is outside its policy root"))?;
    let bytes = relative.as_os_str().as_bytes();
    if relative.is_absolute()
        || bytes.split(|byte| *byte == b'/').any(|component| {
            !bytes.is_empty() && (component.is_empty() || component == b"." || component == b"..")
        })
    {
        return Err(ServiceError::new(
            "Worktree destination relative path is not normalized",
        ));
    }
    Ok(bytes.to_vec())
}

fn reject_unresolved_receipts(broker: &BrokerClient) -> Result<(), ServiceError> {
    let count = broker
        .unresolved_receipt_count()
        .map_err(|error| ServiceError::context("inspect unresolved broker receipts", error))?;
    if count != 0 {
        return Err(ServiceError::new(format!(
            "broker has {count} unresolved receipt(s); exact recovery is required before service",
        )));
    }
    Ok(())
}

fn decode_recovering_initialize(
    row: RecoveringInitializeRow,
) -> Result<RecoveringInitialize, ServiceError> {
    let reservation = InitializeReservation {
        filesystem_id: row.filesystem_id,
        watch_id: fixed_service_blob(&row.watch_id, "recovery watch ID")?,
        grant_id: fixed_service_blob(&row.authorization_id, "recovery grant ID")?,
        operation_id: fixed_service_blob(&row.operation_id, "recovery operation ID")?,
        clock_epoch: fixed_service_blob(&row.clock_epoch, "recovery clock epoch")?,
        operation_fence: row.operation_fence,
        topology_fence: 0,
    };
    let recorded = match row.state.as_str() {
        "fs_started" => {
            if row.snapshot_id.is_some() {
                return Err(ServiceError::new(
                    "fs_started initialize unexpectedly has a recorded snapshot",
                ));
            }
            None
        }
        "uuid_recorded" => {
            let required = |value: Option<Vec<u8>>, field: &str| {
                value.ok_or_else(|| ServiceError::new(format!("recorded initialize omits {field}")))
            };
            let identity = SnapshotIdentity {
                fs_uuid: fixed_service_blob(
                    &required(row.fs_uuid, "filesystem UUID")?,
                    "filesystem UUID",
                )?,
                subvol_uuid: fixed_service_blob(
                    &required(row.subvol_uuid, "subvolume UUID")?,
                    "subvolume UUID",
                )?,
                parent_uuid: row
                    .parent_uuid
                    .as_deref()
                    .map(|value| fixed_service_blob(value, "parent UUID"))
                    .transpose()?,
                received_uuid: row
                    .received_uuid
                    .as_deref()
                    .map(|value| fixed_service_blob(value, "received UUID"))
                    .transpose()?,
                root_id: decode_recovery_u64(required(row.root_id, "root ID")?, "root ID")?,
                ctransid: decode_recovery_u64(required(row.ctransid, "ctransid")?, "ctransid")?,
                otransid: decode_recovery_u64(required(row.otransid, "otransid")?, "otransid")?,
                path: required(row.snapshot_path, "snapshot path")?,
                readonly: row.readonly == Some(1),
                created_ns: row
                    .created_ns
                    .ok_or_else(|| ServiceError::new("recorded initialize omits created_ns"))?,
            };
            Some(RecordedSnapshot {
                snapshot_id: row
                    .snapshot_id
                    .ok_or_else(|| ServiceError::new("recorded initialize omits snapshot ID"))?,
                identity,
            })
        }
        _ => return Err(ServiceError::new("invalid initialize recovery state")),
    };
    Ok(RecoveringInitialize {
        reservation,
        lease_owner: fixed_service_blob(&row.lease_owner, "recovery lease owner")?,
        reserved_path: PathBuf::from(std::ffi::OsString::from_vec(row.reserved_path)),
        live_path: PathBuf::from(std::ffi::OsString::from_vec(row.live_path)),
        recorded,
    })
}

fn decode_recovering_cut(row: RecoveringCutRow) -> Result<RecoveringCut, ServiceError> {
    let reservation = CutReservation {
        filesystem_id: row.filesystem_id,
        watch_id: fixed_service_blob(&row.watch_id, "cut recovery watch ID")?,
        authorization_id: fixed_service_blob(
            &row.authorization_id,
            "cut recovery authorization ID",
        )?,
        operation_id: fixed_service_blob(&row.operation_id, "cut recovery operation ID")?,
        sequence: row.sequence,
        base_snapshot_id: row.base_snapshot_id,
        source_subvol_uuid: fixed_service_blob(
            &row.source_subvol_uuid,
            "cut recovery source UUID",
        )?,
        operation_fence: row.operation_fence,
        cut_fence: row.cut_fence,
    };
    let physical_published = row.state == "manifest_ready";
    let recorded = match row.state.as_str() {
        "fs_started" => {
            if row.snapshot_id.is_some() {
                return Err(ServiceError::new(
                    "fs_started cut unexpectedly has a recorded snapshot",
                ));
            }
            None
        }
        "uuid_recorded" | "manifest_ready" => Some(RecordedSnapshot {
            snapshot_id: row
                .snapshot_id
                .ok_or_else(|| ServiceError::new("recorded cut omits snapshot ID"))?,
            identity: decode_recovery_snapshot_identity(
                row.fs_uuid,
                row.subvol_uuid,
                row.parent_uuid,
                row.received_uuid,
                row.root_id,
                row.ctransid,
                row.otransid,
                row.snapshot_path,
                row.readonly,
                row.created_ns,
                "cut",
            )?,
        }),
        _ => return Err(ServiceError::new("invalid cut recovery state")),
    };
    Ok(RecoveringCut {
        completion: CutCompletion {
            reservation,
            lease_owner: fixed_service_blob(&row.lease_owner, "cut recovery lease owner")?,
            target: None,
            destination_path: PathBuf::from(std::ffi::OsString::from_vec(row.reserved_path)),
            recorded,
            physical_published,
            reuse_staged_spool: true,
            now_ns: current_unix_time_ns()?,
        },
        live_path: PathBuf::from(std::ffi::OsString::from_vec(row.live_path)),
    })
}

fn decode_recovering_snapshot_delete(
    row: RecoveringSnapshotDeleteRow,
) -> Result<RecoveringSnapshotDelete, ServiceError> {
    let identity = decode_recovery_snapshot_identity(
        Some(row.fs_uuid),
        Some(row.subvol_uuid),
        row.parent_uuid,
        row.received_uuid,
        Some(row.root_id),
        Some(row.ctransid),
        Some(row.otransid),
        Some(row.snapshot_path),
        Some(row.readonly),
        Some(row.created_ns),
        "snapshot deletion",
    )?;
    Ok(RecoveringSnapshotDelete {
        reservation: SnapshotDeleteReservation {
            operation_id: fixed_service_blob(
                &row.operation_id,
                "snapshot-delete recovery operation ID",
            )?,
            snapshot_id: row.snapshot_id,
            filesystem_id: row.filesystem_id,
            operation_fence: row.operation_fence,
            identity,
        },
        lease_owner: fixed_service_blob(&row.lease_owner, "snapshot-delete recovery lease owner")?,
        state: row.state,
    })
}

fn decode_recovering_worktree(
    row: RecoveringWorktreeRow,
) -> Result<RecoveringWorktree, ServiceError> {
    let discovered_uuid = row
        .discovered_uuid
        .as_deref()
        .map(|value| fixed_service_blob(value, "Worktree recovery discovered UUID"))
        .transpose()?;
    match row.state.as_str() {
        "fs_started" if discovered_uuid.is_some() => {
            return Err(ServiceError::new(
                "fs_started Worktree unexpectedly has a discovered UUID",
            ));
        }
        "awaiting_destination" if discovered_uuid.is_none() => {
            return Err(ServiceError::new(
                "awaiting-destination Worktree omits its discovered UUID",
            ));
        }
        "fs_started" | "awaiting_destination" => {}
        _ => return Err(ServiceError::new("invalid Worktree recovery state")),
    }
    Ok(RecoveringWorktree {
        reservation: WorktreeReservation {
            operation_id: fixed_service_blob(&row.operation_id, "Worktree recovery operation ID")?,
            worktree_id: fixed_service_blob(&row.worktree_id, "Worktree recovery ID")?,
            filesystem_id: row.filesystem_id,
            seed_snapshot_id: row.seed_snapshot_id,
            seed_revision_id: row.seed_revision_id,
            seed_subvol_uuid: fixed_service_blob(
                &row.seed_subvol_uuid,
                "Worktree recovery seed UUID",
            )?,
            operation_fence: row.operation_fence,
            policy_hash: fixed_service_blob(&row.policy_hash, "Worktree recovery policy hash")?,
        },
        lease_owner: fixed_service_blob(&row.lease_owner, "Worktree recovery lease owner")?,
        state: row.state,
        staged_path: PathBuf::from(std::ffi::OsString::from_vec(row.staged_path)),
        destination_root_path: PathBuf::from(std::ffi::OsString::from_vec(
            row.destination_root_path,
        )),
        destination_root_uuid: fixed_service_blob(
            &row.destination_root_uuid,
            "Worktree recovery root UUID",
        )?,
        destination_root_generation: decode_recovery_u64(
            row.destination_root_generation,
            "Worktree recovery root generation",
        )?,
        destination_parent_ino: decode_recovery_u64(
            row.destination_parent_ino,
            "Worktree recovery parent inode",
        )?,
        destination_parent_generation: decode_recovery_u64(
            row.destination_parent_generation,
            "Worktree recovery parent generation",
        )?,
        destination_name: row.destination_name,
        reservation_name: row.reservation_name,
        reservation_ino: decode_recovery_u64(
            row.reservation_ino,
            "Worktree recovery reservation inode",
        )?,
        reservation_generation: decode_recovery_u64(
            row.reservation_generation,
            "Worktree recovery reservation generation",
        )?,
        reservation_nonce: fixed_service_blob(
            &row.reservation_nonce,
            "Worktree recovery reservation nonce",
        )?,
        requester_uid: row.requester_uid,
        discovered_uuid,
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_recovery_snapshot_identity(
    fs_uuid: Option<Vec<u8>>,
    subvol_uuid: Option<Vec<u8>>,
    parent_uuid: Option<Vec<u8>>,
    received_uuid: Option<Vec<u8>>,
    root_id: Option<Vec<u8>>,
    ctransid: Option<Vec<u8>>,
    otransid: Option<Vec<u8>>,
    snapshot_path: Option<Vec<u8>>,
    readonly: Option<i64>,
    created_ns: Option<i64>,
    operation: &str,
) -> Result<SnapshotIdentity, ServiceError> {
    let required = |value: Option<Vec<u8>>, field: &str| {
        value.ok_or_else(|| ServiceError::new(format!("recorded {operation} omits {field}")))
    };
    Ok(SnapshotIdentity {
        fs_uuid: fixed_service_blob(&required(fs_uuid, "filesystem UUID")?, "filesystem UUID")?,
        subvol_uuid: fixed_service_blob(
            &required(subvol_uuid, "subvolume UUID")?,
            "subvolume UUID",
        )?,
        parent_uuid: parent_uuid
            .as_deref()
            .map(|value| fixed_service_blob(value, "parent UUID"))
            .transpose()?,
        received_uuid: received_uuid
            .as_deref()
            .map(|value| fixed_service_blob(value, "received UUID"))
            .transpose()?,
        root_id: decode_recovery_u64(required(root_id, "root ID")?, "root ID")?,
        ctransid: decode_recovery_u64(required(ctransid, "ctransid")?, "ctransid")?,
        otransid: decode_recovery_u64(required(otransid, "otransid")?, "otransid")?,
        path: required(snapshot_path, "snapshot path")?,
        readonly: readonly == Some(1),
        created_ns: created_ns
            .ok_or_else(|| ServiceError::new(format!("recorded {operation} omits created_ns")))?,
    })
}

fn decode_recovery_u64(value: Vec<u8>, field: &str) -> Result<u64, ServiceError> {
    decode_u64(&value).map_err(|error| ServiceError::context(format!("decode {field}"), error))
}

fn fixed_service_blob<const N: usize>(value: &[u8], field: &str) -> Result<[u8; N], ServiceError> {
    value.try_into().map_err(|_| {
        ServiceError::new(format!(
            "stored {field} has length {}, expected {N}",
            value.len()
        ))
    })
}

fn current_unix_time_ns() -> Result<i64, ServiceError> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| ServiceError::context("read recovery clock", error))?;
    i64::try_from(duration.as_nanos())
        .map_err(|_| ServiceError::new("recovery clock exceeds signed nanoseconds"))
}

fn write_manifest_stage_trailer(
    spool: &mut File,
    manifest_len: u64,
    manifest_hash: [u8; 32],
) -> Result<(), ServiceError> {
    let end = spool
        .seek(SeekFrom::End(0))
        .map_err(|error| ServiceError::context("seek manifest stage end", error))?;
    if end != manifest_len {
        return Err(ServiceError::new(
            "manifest stage length changed before completion",
        ));
    }
    spool
        .write_all(MANIFEST_STAGE_TRAILER_MAGIC)
        .and_then(|()| spool.write_all(&manifest_len.to_le_bytes()))
        .and_then(|()| spool.write_all(&manifest_hash))
        .and_then(|()| spool.sync_all())
        .map_err(|error| ServiceError::context("durably complete manifest stage", error))
}

fn load_staged_manifest(
    path: &Path,
    max_manifest_bytes: u64,
) -> Result<Option<ParsedKernelChangedObjects>, ServiceError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ServiceError::context("inspect manifest stage", error)),
    };
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(ServiceError::new(
            "manifest stage is not a private manager-owned regular file",
        ));
    }
    let maximum = max_manifest_bytes
        .checked_add(MANIFEST_STAGE_TRAILER_LEN as u64)
        .ok_or_else(|| ServiceError::new("manifest stage limit overflow"))?;
    if metadata.len() > maximum {
        return Err(ServiceError::new(
            "manifest stage exceeds its configured limit",
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .map_err(|_| ServiceError::new("manifest stage length exceeds memory limits"))?,
    );
    File::open(path)
        .map_err(|error| ServiceError::context("open manifest stage", error))?
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ServiceError::context("read manifest stage", error))?;
    let valid = (|| -> Option<&[u8]> {
        let split = bytes.len().checked_sub(MANIFEST_STAGE_TRAILER_LEN)?;
        let (manifest, trailer) = bytes.split_at(split);
        if trailer.get(..16)? != MANIFEST_STAGE_TRAILER_MAGIC {
            return None;
        }
        let declared_len = u64::from_le_bytes(trailer.get(16..24)?.try_into().ok()?);
        let expected_hash: [u8; 32] = trailer.get(24..56)?.try_into().ok()?;
        if declared_len != manifest.len() as u64 || hash_bytes(manifest) != expected_hash {
            return None;
        }
        Some(manifest)
    })();
    let Some(manifest_bytes) = valid else {
        discard_private_spool(path)?;
        return Ok(None);
    };
    match parse_kernel_changed_objects(manifest_bytes) {
        Ok(manifest) => Ok(Some(manifest)),
        Err(_) => {
            discard_private_spool(path)?;
            Ok(None)
        }
    }
}

fn parse_kernel_changed_objects(bytes: &[u8]) -> Result<ParsedKernelChangedObjects, ServiceError> {
    if bytes.get(..CHANGED_OBJECTS_V2_MAGIC.len()) == Some(CHANGED_OBJECTS_V2_MAGIC) {
        let parsed = parse_changed_objects_v2(bytes)
            .map_err(|error| ServiceError::context("parse changed-objects v2 stream", error))?;
        if !parsed.header.boundary_records {
            return Err(ServiceError::new(
                "changed-objects v2 stream does not certify subvolume boundaries",
            ));
        }
        if let Some(boundary) = parsed.boundary_adds.iter().next() {
            return Err(ServiceError::new(format!(
                "immutable snapshot contains nested subvolume boundary parent={} child_root={} name={:?}",
                boundary.parent_ino, boundary.child_root_id, boundary.name
            )));
        }
        let mut target_objects = BTreeMap::new();
        for (&ino, metadata) in &parsed.target_objects {
            let xattrs = parsed.target_security_xattrs.get(&ino).ok_or_else(|| {
                ServiceError::new(format!(
                    "changed-objects v2 target inode {ino} lacks an exact xattr reset"
                ))
            })?;
            let object = materialize_stream_object(ino, metadata, xattrs)
                .map_err(|error| ServiceError::context("materialize v2 target object", error))?;
            target_objects.insert(ino, object);
        }
        Ok(ParsedKernelChangedObjects {
            manifest: parsed.manifest,
            target_objects: Some(target_objects),
            dirty_witness_contract: parsed.header.dirty_witness,
        })
    } else {
        parse_changed_objects(bytes)
            .map(|manifest| ParsedKernelChangedObjects {
                manifest,
                target_objects: None,
                dirty_witness_contract: false,
            })
            .map_err(|error| ServiceError::context("parse legacy changed-object stream", error))
    }
}

fn create_private_spool(path: &Path) -> Result<File, ServiceError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| ServiceError::context(format!("create spool {}", path.display()), error))
}

fn discard_private_spool(path: &Path) -> Result<(), ServiceError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ServiceError::context("inspect stale manifest spool", error)),
    };
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(ServiceError::new(
            "stale manifest spool is not a private manager-owned regular file",
        ));
    }
    fs::remove_file(path)
        .map_err(|error| ServiceError::context("remove stale manifest spool", error))
}

fn cleanup_stale_spool_files(directory: &Path) -> Result<usize, ServiceError> {
    let mut removed = 0_usize;
    for entry in fs::read_dir(directory)
        .map_err(|error| ServiceError::context("enumerate recovery spool", error))?
    {
        let entry = entry.map_err(|error| ServiceError::context("read recovery spool", error))?;
        let name = entry.file_name();
        if !is_stale_spool_name(name.as_bytes()) {
            continue;
        }
        discard_private_spool(&entry.path())?;
        removed = removed
            .checked_add(1)
            .ok_or_else(|| ServiceError::new("stale spool count overflow"))?;
    }
    Ok(removed)
}

fn is_stale_spool_name(name: &[u8]) -> bool {
    fn hex32(value: &[u8]) -> bool {
        value.len() == 32 && value.iter().all(u8::is_ascii_hexdigit)
    }
    if let Some(id) = name.strip_prefix(b"full-index-") {
        return hex32(id);
    }
    if let Some(id) = name.strip_prefix(b"target-index-") {
        return hex32(id);
    }
    if let Some(rest) = name.strip_prefix(b"historical-") {
        let Some(rest) = rest.strip_suffix(b".part") else {
            return false;
        };
        let Some(separator) = rest.iter().rposition(|byte| *byte == b'-') else {
            return false;
        };
        return !rest[..separator].is_empty()
            && rest[..separator].iter().all(u8::is_ascii_digit)
            && !rest[separator + 1..].is_empty()
            && rest[separator + 1..].iter().all(u8::is_ascii_digit);
    }
    let Some(rest) = name.strip_prefix(b"manifest-") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(b".part") else {
        return false;
    };
    let Some(separator) = rest.iter().rposition(|byte| *byte == b'-') else {
        return false;
    };
    hex32(&rest[..separator])
        && !rest[separator + 1..].is_empty()
        && rest[separator + 1..].iter().all(u8::is_ascii_digit)
}

fn quarantine_unexpected_managed_entries(
    store: &Store,
    managed_directory: &Path,
) -> Result<usize, ServiceError> {
    let mut known = BTreeSet::<Vec<u8>>::new();
    {
        let mut statement = store
            .connection()
            .prepare("SELECT path, physical_state FROM snapshots ORDER BY id")
            .map_err(|error| ServiceError::context("prepare managed snapshot scan", error))?;
        let snapshots = statement
            .query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| ServiceError::context("query managed snapshot scan", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ServiceError::context("decode managed snapshot scan", error))?;
        for (path, state) in snapshots {
            let path_buf = PathBuf::from(OsString::from_vec(path.clone()));
            if path_buf.parent() != Some(managed_directory) {
                return Err(ServiceError::new(
                    "stored managed snapshot path escapes its configured directory",
                ));
            }
            let reappeared = match fs::symlink_metadata(&path_buf) {
                Ok(_) => true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(ServiceError::context(
                        "inspect tombstoned managed snapshot path",
                        error,
                    ));
                }
            };
            if matches!(state.as_str(), "deleted" | "lost") && reappeared {
                return Err(ServiceError::new(format!(
                    "durability fault: {state} managed snapshot reappeared at {}",
                    path_buf.display()
                )));
            }
            if !matches!(state.as_str(), "deleted" | "lost") {
                known.insert(path);
            }
        }
    }
    {
        let mut statement = store
            .connection()
            .prepare(
                "SELECT reserved_path FROM operations \
                 WHERE state NOT IN ('planned', 'done', 'failed') ORDER BY id",
            )
            .map_err(|error| ServiceError::context("prepare managed operation scan", error))?;
        let paths = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|error| ServiceError::context("query managed operation scan", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ServiceError::context("decode managed operation scan", error))?;
        known.extend(paths);
    }

    let quarantine = managed_directory.join("quarantine");
    match fs::create_dir(&quarantine) {
        Ok(()) => fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o700))
            .map_err(|error| ServiceError::context("secure managed quarantine", error))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_private_directory(&quarantine)?;
        }
        Err(error) => return Err(ServiceError::context("create managed quarantine", error)),
    }

    let mut quarantined = 0_usize;
    for entry in fs::read_dir(managed_directory)
        .map_err(|error| ServiceError::context("enumerate managed directory", error))?
    {
        let entry = entry.map_err(|error| ServiceError::context("read managed entry", error))?;
        let name = entry.file_name();
        if name.as_bytes() == b"quarantine" || !is_managed_object_name(name.as_bytes()) {
            continue;
        }
        let path_bytes = entry.path().as_os_str().as_bytes().to_vec();
        if known.contains(&path_bytes) {
            continue;
        }
        let destination = quarantine.join(OsString::from_vec({
            let mut value = name.as_bytes().to_vec();
            value.extend_from_slice(b"-quarantine-");
            value.extend_from_slice(hex_id(&random_id()).as_bytes());
            value
        }));
        rename_noreplace(&entry.path(), &destination)?;
        quarantined = quarantined
            .checked_add(1)
            .ok_or_else(|| ServiceError::new("managed quarantine count overflow"))?;
    }
    Ok(quarantined)
}

fn rename_noreplace(source: &Path, destination: &Path) -> Result<(), ServiceError> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| ServiceError::new("quarantine source contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| ServiceError::new("quarantine destination contains NUL"))?;
    // SAFETY: both paths are NUL-terminated and valid for the duration of the
    // call. RENAME_NOREPLACE prevents quarantine from overwriting evidence.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(ServiceError::context(
            "quarantine unexpected managed object",
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn is_managed_object_name(name: &[u8]) -> bool {
    [b"cut-initialize-".as_slice(), b"cut-", b"worktree-"]
        .into_iter()
        .any(|prefix| {
            name.strip_prefix(prefix)
                .is_some_and(|id| id.len() == 32 && id.iter().all(u8::is_ascii_hexdigit))
        })
}

fn read_broker_output(
    output: &mut File,
    result: &crate::broker::ChangedObjectsResult,
) -> Result<Vec<u8>, ServiceError> {
    output
        .seek(SeekFrom::Start(0))
        .map_err(|error| ServiceError::context("rewind broker output", error))?;
    let capacity = usize::try_from(result.output_bytes)
        .map_err(|_| ServiceError::new("broker output length exceeds address space"))?;
    let mut bytes = Vec::with_capacity(capacity);
    output
        .take(result.output_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ServiceError::context("read broker output", error))?;
    if bytes.len() as u64 != result.output_bytes || hash_bytes(&bytes) != result.manifest_hash {
        return Err(ServiceError::new(
            "broker output failed its length/hash check",
        ));
    }
    Ok(bytes)
}

fn operation_name(prefix: &[u8]) -> Vec<u8> {
    let mut name = prefix.to_vec();
    name.extend_from_slice(hex_id(&random_id()).as_bytes());
    name
}

fn random_id() -> [u8; 16] {
    *Uuid::new_v4().as_bytes()
}

fn hex_id(id: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(32);
    for byte in id {
        result.push(char::from(HEX[usize::from(byte >> 4)]));
        result.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    result
}

fn lease_expiry(now_ns: i64, lease_ns: i64) -> Result<i64, ServiceError> {
    now_ns
        .checked_add(lease_ns)
        .ok_or_else(|| ServiceError::new("lease expiration overflows signed nanoseconds"))
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn derived_effect_id(operation_id: [u8; 16], label: &[u8]) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-effect-id-v1\0");
    hash.update(operation_id);
    hash.update(label);
    hash.finalize()[..16]
        .try_into()
        .expect("fixed digest prefix")
}

#[derive(Debug)]
pub struct ServiceError {
    message: String,
}

impl ServiceError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn context(context: impl fmt::Display, error: impl fmt::Display) -> Self {
        Self::new(format!("{context}: {error}"))
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ServiceError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{CHANGED_OBJECTS_MAGIC, CHANGED_OBJECTS_VERSION};
    use crate::store::ServiceMetadata;
    use tempfile::tempdir;

    #[test]
    fn immutable_index_rejects_fscrypt_before_publication() {
        let index = Index {
            objects: BTreeMap::from([(
                crate::index::ROOT_INO,
                Object {
                    ino: crate::index::ROOT_INO,
                    generation: 1,
                    mode: crate::index::MODE_DIRECTORY | 0o700,
                    nlink: 1,
                    uid: 1000,
                    gid: 1000,
                    rdev: 0,
                    privilege_flags: PRIVILEGE_FSCRYPT,
                    security_xattr_hash: [0; 32],
                },
            )]),
            references: BTreeSet::new(),
        };
        assert!(reject_fscrypt_index(&index).is_err());
    }

    #[test]
    fn startup_spool_sweep_removes_only_exact_private_formats() {
        let spool = tempdir().unwrap();
        for name in [
            "full-index-0123456789abcdef0123456789abcdef",
            "target-index-fedcba9876543210fedcba9876543210",
            "manifest-0123456789abcdef0123456789abcdef-7.part",
            "historical-42-7.part",
        ] {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(spool.path().join(name))
                .unwrap();
        }
        fs::write(spool.path().join("operator-note"), b"keep").unwrap();
        fs::write(spool.path().join("manifest-not-an-id-7.part"), b"keep").unwrap();

        assert_eq!(cleanup_stale_spool_files(spool.path()).unwrap(), 4);
        assert!(spool.path().join("operator-note").exists());
        assert!(spool.path().join("manifest-not-an-id-7.part").exists());
    }

    #[test]
    fn completed_manifest_stage_is_reused_but_partial_output_is_discarded() {
        let spool = tempdir().unwrap();
        let complete_path = spool
            .path()
            .join("manifest-0123456789abcdef0123456789abcdef-7.part");
        let mut manifest = Vec::from(CHANGED_OBJECTS_MAGIC.as_slice());
        manifest.extend_from_slice(&CHANGED_OBJECTS_VERSION.to_le_bytes());
        manifest.extend_from_slice(&24_u32.to_le_bytes());
        let mut complete = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&complete_path)
            .unwrap();
        complete.write_all(&manifest).unwrap();
        write_manifest_stage_trailer(&mut complete, manifest.len() as u64, hash_bytes(&manifest))
            .unwrap();
        drop(complete);
        let reused = load_staged_manifest(&complete_path, 1024).unwrap().unwrap();
        assert!(reused.manifest.objects.is_empty());
        assert!(complete_path.exists());

        let partial_path = spool
            .path()
            .join("manifest-fedcba9876543210fedcba9876543210-8.part");
        let mut partial = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial_path)
            .unwrap();
        partial.write_all(&manifest).unwrap();
        drop(partial);
        assert!(load_staged_manifest(&partial_path, 1024).unwrap().is_none());
        assert!(!partial_path.exists());
    }

    #[test]
    fn startup_quarantines_an_unclaimed_managed_name_without_deleting_it() {
        let temp = tempdir().unwrap();
        let managed = temp.path().join("managed");
        fs::create_dir(&managed).unwrap();
        fs::set_permissions(&managed, fs::Permissions::from_mode(0o700)).unwrap();
        let unexpected = managed.join("cut-0123456789abcdef0123456789abcdef");
        fs::create_dir(&unexpected).unwrap();
        fs::write(unexpected.join("evidence"), b"preserve").unwrap();
        let metadata = ServiceMetadata {
            store_uuid: [1; 16],
            clock_hmac_key: [2; 32],
            clock_format_version: 1,
            last_boot_id: [3; 16],
            created_ns: 1,
        };
        let store = Store::create(&temp.path().join("state.sqlite3"), &metadata).unwrap();

        assert_eq!(
            quarantine_unexpected_managed_entries(&store, &managed).unwrap(),
            1
        );
        assert!(!unexpected.exists());
        let quarantined = fs::read_dir(managed.join("quarantine"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read(quarantined.join("evidence")).unwrap(), b"preserve");
    }
}
