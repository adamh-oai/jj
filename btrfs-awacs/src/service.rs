//! Manager orchestration for the SQLite and privileged-broker contracts.
//!
//! Production uses the external authenticated seqpacket broker. UML and unit
//! tests may use the same dispatcher over an embedded socketpair.

use crate::broker::{
    ChangedObjectsExecution, ChangedObjectsResult, ExpectedSubvolume, MAX_CHANGED_OBJECT_OUTPUT,
    SeqPacket,
};
use crate::broker_protocol::{BrokerClient, BrokerDispatcher};
use crate::btrfs::{
    ChangedObjectsIoctlResult, OpenedSubvolume, create_snapshot as create_btrfs_snapshot,
    destroy_snapshot, filesystem_info, set_subvolume_readonly, subvolume_info,
};
use crate::index::{Index, Object};
use crate::manager::{
    CutAdmission, CutRequest, CutReservation, HistoricalChanges, HistoricalComparisonAdmission,
    HistoricalComparisonRequest, InitializeRequest, InitializeReservation, InitializedWatch,
    Permissions, Principal, PublishedCut, RecordedSnapshot, SnapshotDeleteReservation,
    SnapshotIdentity,
};
use crate::manifest::{
    CHANGE_CREATED, CHANGE_INODE, CHANGE_XATTR, CHANGED_OBJECTS_V2_MAGIC, ChangedObjectsManifest,
    ChangedObjectsV2Completion, ChangedObjectsV2Header, parse_changed_objects,
    parse_changed_objects_v2,
};
use crate::snapshot_walk::{
    SnapshotIndexError, SnapshotWalkProgress, read_snapshot_index_with_progress,
};
use crate::store::{Store, decode_u64};
use crate::tree_index::materialize_stream_object;
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
    /// V2 streams must retain their endpoint/header and footer proof until
    /// the broker-verified endpoints and ioctl completion can be compared.
    v2_proof: Option<ParsedV2Proof>,
}

#[derive(Clone, Copy, Debug)]
struct ParsedV2Proof {
    header: ChangedObjectsV2Header,
    completion: ChangedObjectsV2Completion,
}

#[derive(Debug)]
struct StagedKernelChangedObjects {
    parsed: ParsedKernelChangedObjects,
    broker_result: ChangedObjectsResult,
}

struct SnapshotIndexResult {
    index: Index,
}

enum InitialIndexSource<'a> {
    Walk(&'a mut dyn FnMut(SnapshotWalkProgress)),
    Clone(Index),
}

const DEFAULT_LEASE_NS: i64 = 300_000_000_000;
const DEFAULT_MAINTENANCE_WATCH_LIMIT: usize = 1;
const DEFAULT_MAINTENANCE_BOUNDARY_DELETE_LIMIT: usize = 16;
const DEFAULT_MAINTENANCE_SNAPSHOT_DELETE_LIMIT: usize = 2;
const EXTERNAL_BROKER_STARTUP_RETRY_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(10);
const EXTERNAL_BROKER_STARTUP_RETRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(100);
const MANIFEST_STAGE_TRAILER_MAGIC: &[u8; 16] = b"bsend-stage-v2\0\0";
const MANIFEST_STAGE_TRAILER_LEN: usize = 16 + 8 + 32 + 8 + 8 + 8;
const MANIFEST_STAGE_V2_IOCTL: u64 = 1;

#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub managed_snapshot_directory: PathBuf,
    pub spool_directory: PathBuf,
    pub boot_id: [u8; 16],
    pub lease_ns: i64,
    pub max_manifest_bytes: u64,
    /// When set, connect to the separately privileged broker instead of
    /// starting the embedded UML/test dispatcher.
    pub broker_socket: Option<PathBuf>,
    pub fault_after_initialize_snapshot: bool,
    pub fault_after_cut_snapshot: bool,
    pub fault_next_incremental_comparison: bool,
    pub fault_after_manifest_stage: bool,
    pub fault_after_snapshot_delete: bool,
    pub replay_window_cuts: i64,
    pub replay_window_ns: i64,
    pub maintenance_watch_limit: usize,
    pub maintenance_boundary_delete_limit: usize,
    pub maintenance_snapshot_delete_limit: usize,
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
            broker_socket: None,
            fault_after_initialize_snapshot: false,
            fault_after_cut_snapshot: false,
            fault_next_incremental_comparison: false,
            fault_after_manifest_stage: false,
            fault_after_snapshot_delete: false,
            replay_window_cuts: 128,
            replay_window_ns: 86_400_000_000_000,
            maintenance_watch_limit: DEFAULT_MAINTENANCE_WATCH_LIMIT,
            maintenance_boundary_delete_limit: DEFAULT_MAINTENANCE_BOUNDARY_DELETE_LIMIT,
            maintenance_snapshot_delete_limit: DEFAULT_MAINTENANCE_SNAPSHOT_DELETE_LIMIT,
        }
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

    pub fn with_replay_retention(mut self, cuts: i64, duration_ns: i64) -> Self {
        self.replay_window_cuts = cuts;
        self.replay_window_ns = duration_ns;
        self
    }

    pub fn with_maintenance_limits(
        mut self,
        watches: usize,
        boundary_deletes: usize,
        snapshot_deletes: usize,
    ) -> Self {
        self.maintenance_watch_limit = watches;
        self.maintenance_boundary_delete_limit = boundary_deletes;
        self.maintenance_snapshot_delete_limit = snapshot_deletes;
        self
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MaintenanceReport {
    pub expired_query_leases: usize,
    pub expired_retention_leases: usize,
    pub expired_historical_comparisons: usize,
    pub watches_processed: usize,
    pub history_rows_reclaimed: usize,
    pub snapshots_deleted: usize,
    pub more_work: bool,
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

#[derive(Debug)]
pub struct Service {
    store: Store,
    broker: BrokerClient,
    manager_store_uuid: [u8; 16],
    manager_session_id: [u8; 16],
    lease_owner: [u8; 16],
    config: ServiceConfig,
    maintenance_after_watch: Option<[u8; 16]>,
    last_maintenance_watches_processed: usize,
    last_maintenance_more_watches: bool,
}

impl Service {
    fn open_subvolume(&self, path: &Path) -> Result<OpenedSubvolume, ServiceError> {
        OpenedSubvolume::open(path).map_err(|error| ServiceError::context("open subvolume", error))
    }

    pub fn new(mut store: Store, config: ServiceConfig) -> Result<Self, ServiceError> {
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
        if config.maintenance_watch_limit == 0
            || config.maintenance_boundary_delete_limit == 0
            || config.maintenance_snapshot_delete_limit == 0
        {
            return Err(ServiceError::new("invalid maintenance limits"));
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
                let dispatcher = BrokerDispatcher::new(manager_uid);
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
            maintenance_after_watch: None,
            last_maintenance_watches_processed: 0,
            last_maintenance_more_watches: false,
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
        service.recover_snapshot_delete_operations()?;
        cleanup_stale_spool_files(&service.config.spool_directory)?;
        quarantine_unexpected_managed_entries(
            &service.store,
            &service.config.managed_snapshot_directory,
        )?;
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
        if config.maintenance_watch_limit == 0
            || config.maintenance_boundary_delete_limit == 0
            || config.maintenance_snapshot_delete_limit == 0
        {
            return Err(ServiceError::new("invalid maintenance limits"));
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
        let broker = connect_external_broker_with_retry(
            socket_path,
            manager_store_uuid,
            EXTERNAL_BROKER_STARTUP_RETRY_WINDOW,
            EXTERNAL_BROKER_STARTUP_RETRY_INTERVAL,
        )?;
        let manager_session_id = broker.session_id();
        let lease_owner = random_id();
        let mut service = Self {
            store,
            broker,
            manager_store_uuid,
            manager_session_id,
            lease_owner,
            config,
            maintenance_after_watch: None,
            last_maintenance_watches_processed: 0,
            last_maintenance_more_watches: false,
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
        service.recover_snapshot_delete_operations()?;
        cleanup_stale_spool_files(&service.config.spool_directory)?;
        quarantine_unexpected_managed_entries(
            &service.store,
            &service.config.managed_snapshot_directory,
        )?;
        Ok(service)
    }

    fn recover_initialize_operations(&mut self) -> Result<(), ServiceError> {
        let recovering = self.load_recovering_initializes()?;
        for operation in recovering {
            let recorded = if let Some(recorded) = operation.recorded {
                recorded
            } else {
                let source_uuid =
                    self.initialize_source_uuid(operation.reservation.operation_id)?;
                let target = if let Some(target) =
                    self.recover_existing_snapshot(&operation.reserved_path, source_uuid)?
                {
                    target
                } else {
                    if !operation
                        .live_path
                        .try_exists()
                        .map_err(|error| ServiceError::context("inspect recovery source", error))?
                    {
                        self.store
                            .cancel_unstarted_initialize(
                                &operation.reservation,
                                operation.lease_owner,
                            )
                            .map_err(|error| {
                                ServiceError::context("cancel missing-source initialize", error)
                            })?;
                        tracing::warn!(
                            source = %operation.live_path.display(),
                            operation_id = %hex_id(&operation.reservation.operation_id),
                            "cancelled initialize whose source and snapshot both disappeared"
                        );
                        continue;
                    }
                    let source = self
                        .open_subvolume(&operation.live_path)
                        .map_err(|error| ServiceError::context("open recovery source", error))?;
                    if source.subvolume.uuid != source_uuid {
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
                    self.create_snapshot(&source, &destination_name, true)?
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
            let snapshot_fd = self
                .open_subvolume(&operation.reserved_path)
                .map_err(|error| ServiceError::context("open recovered snapshot", error))?;
            let expected =
                ExpectedSubvolume::from_observed(&snapshot_fd.filesystem, &snapshot_fd.subvolume);
            verify_recorded_snapshot(&recorded.identity, &expected)?;
            let full_index =
                self.snapshot_full_index(&expected, snapshot_fd.as_fd(), &operation.live_path)?;
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
                let target = self
                    .open_subvolume(&operation.completion.destination_path)
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
                operation.completion.target = Some(
                    if let Some(target) = self.recover_existing_snapshot(
                        &operation.completion.destination_path,
                        operation.completion.reservation.source_subvol_uuid,
                    )? {
                        target
                    } else {
                        let source =
                            self.open_subvolume(&operation.live_path).map_err(|error| {
                                ServiceError::context("open cut recovery source", error)
                            })?;
                        if source.subvolume.uuid
                            != operation.completion.reservation.source_subvol_uuid
                        {
                            return Err(ServiceError::new("cut recovery source subvolume changed"));
                        }
                        let destination_name = operation
                            .completion
                            .destination_path
                            .file_name()
                            .ok_or_else(|| ServiceError::new("recovery cut path has no basename"))?
                            .as_bytes()
                            .to_vec();
                        self.create_snapshot(&source, &destination_name, true)?
                    },
                );
            }
            let operation_id = operation.completion.reservation.operation_id;
            if let Err(error) = self.finish_cut(operation.completion) {
                // A deterministic invalid target is already fenced into a
                // failed gap by `finish_cut`. Recovery must not make that
                // durable terminal state into a startup loop; only continue
                // once the failed operation row proves the transition landed.
                if is_terminal_unpublished_cut_rejection(&error)
                    && self.cut_operation_is_failed(operation_id)?
                {
                    tracing::warn!(
                        operation_id = %hex_id(&operation_id),
                        error = %error,
                        "recovery terminally rejected unpublished cut"
                    );
                    continue;
                }
                return Err(error);
            }
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

    fn cut_operation_is_failed(&self, operation_id: [u8; 16]) -> Result<bool, ServiceError> {
        let state: String = self
            .store
            .connection()
            .query_row(
                "SELECT state FROM operations WHERE id = ?1",
                [operation_id.as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| ServiceError::context("load recovered cut state", error))?;
        Ok(state == "failed")
    }

    fn recover_snapshot_delete_operations(&mut self) -> Result<(), ServiceError> {
        self.recover_snapshot_delete_operations_with_limit(i64::MAX)
            .map(|_| ())
    }

    fn recover_snapshot_delete_operations_bounded(
        &mut self,
        limit: usize,
    ) -> Result<usize, ServiceError> {
        if limit == 0 {
            return Err(ServiceError::new(
                "snapshot-delete recovery limit must be positive",
            ));
        }
        let limit = i64::try_from(limit)
            .map_err(|_| ServiceError::new("snapshot-delete recovery limit overflow"))?;
        self.recover_snapshot_delete_operations_with_limit(limit)
    }

    fn recover_snapshot_delete_operations_with_limit(
        &mut self,
        limit: i64,
    ) -> Result<usize, ServiceError> {
        let operations = self.load_recovering_snapshot_deletes(limit)?;
        let recovered = operations.len();
        for operation in operations {
            if operation.state == "fs_started" {
                self.execute_snapshot_delete(
                    &operation.reservation,
                    operation.lease_owner,
                    current_unix_time_ns()?,
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
        Ok(recovered)
    }

    fn load_recovering_snapshot_deletes(
        &self,
        limit: i64,
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
                    ORDER BY d.updated_ns, d.id
                    LIMIT ?1"#,
            )
            .map_err(|error| ServiceError::context("prepare snapshot-delete recovery", error))?;
        let rows = statement
            .query_map([limit], |row| {
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

    /// Verifies that explicit bootstrap already recorded an active watch for
    /// this live subvolume. Daemon startup uses this instead of creating
    /// state as a side effect of the first scan request.
    pub fn require_initialized_root(&self, root: &Path) -> Result<(), ServiceError> {
        let canonical_root = fs::canonicalize(root)
            .map_err(|error| ServiceError::context("canonicalize initialized root", error))?;
        let opened = self
            .open_subvolume(&canonical_root)
            .map_err(|error| ServiceError::context("open initialized root", error))?;
        let count: i64 = self
            .store
            .connection()
            .query_row(
                r#"SELECT count(*)
                     FROM watches w JOIN filesystems f ON f.id = w.filesystem_id
                    WHERE w.state = 'active'
                      AND f.fs_uuid = ?1 AND w.live_subvol_uuid = ?2"#,
                rusqlite::params![
                    opened.filesystem.fs_uuid.as_slice(),
                    opened.subvolume.uuid.as_slice(),
                ],
                |row| row.get(0),
            )
            .map_err(|error| ServiceError::context("load initialized AWACS root", error))?;
        if count == 0 {
            return Err(ServiceError::new(format!(
                "{} has not been initialized; run awacs init {} first",
                canonical_root.display(),
                canonical_root.display(),
            )));
        }
        Ok(())
    }

    pub fn snapshot_facade_is_enabled(&self) -> bool {
        true
    }

    /// Bootstrap has no changed-objects stream. Fresh-baseline clocks are
    /// therefore available immediately, while every later changed-objects
    /// comparison must independently advertise the dirty-witness capability
    /// before it can be published.
    pub fn ensure_snapshot_facade_is_enabled(
        &self,
        _watch_id: [u8; 16],
    ) -> Result<bool, ServiceError> {
        Ok(self.snapshot_facade_is_enabled())
    }

    fn worker_handle(&self, role: &str) -> Result<Self, ServiceError> {
        let socket_path = self.config.broker_socket.as_ref().ok_or_else(|| {
            ServiceError::new(format!("concurrent {role} workers require external broker"))
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
            maintenance_after_watch: None,
            last_maintenance_watches_processed: 0,
            last_maintenance_more_watches: false,
        })
    }

    pub fn query_worker(&self) -> Result<Self, ServiceError> {
        self.worker_handle("query")
    }

    /// Opens a second store/broker handle in the current manager session
    /// without running startup recovery or rotating any live leases.
    pub fn maintenance_worker(&self) -> Result<Self, ServiceError> {
        self.worker_handle("maintenance")
    }

    pub fn initialize(
        &mut self,
        source_path: &Path,
        options: &InitializeOptions,
    ) -> Result<InitializedWatch, ServiceError> {
        self.initialize_with_index_progress(source_path, options, |_| {})
    }

    pub fn initialize_with_index_progress(
        &mut self,
        source_path: &Path,
        options: &InitializeOptions,
        mut progress: impl FnMut(SnapshotWalkProgress),
    ) -> Result<InitializedWatch, ServiceError> {
        self.initialize_with_index_source(
            source_path,
            options,
            InitialIndexSource::Walk(&mut progress),
        )
    }

    /// Creates an initial immutable snapshot and publishes a caller-supplied
    /// complete index into this service's otherwise-fresh store.
    pub fn initialize_with_index(
        &mut self,
        source_path: &Path,
        options: &InitializeOptions,
        index: Index,
    ) -> Result<InitializedWatch, ServiceError> {
        self.initialize_with_index_source(source_path, options, InitialIndexSource::Clone(index))
    }

    fn initialize_with_index_source(
        &mut self,
        source_path: &Path,
        options: &InitializeOptions,
        index_source: InitialIndexSource<'_>,
    ) -> Result<InitializedWatch, ServiceError> {
        let canonical_source = fs::canonicalize(source_path)
            .map_err(|error| ServiceError::context("canonicalize initialize source", error))?;
        reject_managed_descendant(&canonical_source, &self.config.managed_snapshot_directory)?;
        let source = self
            .open_subvolume(&canonical_source)
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

        let snapshot = self.create_snapshot(&source, &destination_name, true)?;
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
        let snapshot_fd = self
            .open_subvolume(&destination_path)
            .map_err(|error| ServiceError::context("reopen initialize snapshot", error))?;
        let expected =
            ExpectedSubvolume::from_observed(&snapshot_fd.filesystem, &snapshot_fd.subvolume);
        if expected != snapshot {
            return Err(ServiceError::new(
                "initialize snapshot identity changed before indexing",
            ));
        }
        let full_index = match index_source {
            InitialIndexSource::Walk(progress) => self.snapshot_full_index_with_progress(
                &expected,
                snapshot_fd.as_fd(),
                &canonical_source,
                progress,
            )?,
            InitialIndexSource::Clone(index) => SnapshotIndexResult { index },
        };
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
        let live = self
            .open_subvolume(&live_path)
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
        let snapshot_started = std::time::Instant::now();
        let target = self.create_snapshot(&live, &destination_name, true)?;
        tracing::info!(
            elapsed_ms = snapshot_started.elapsed().as_millis() as u64,
            "query cut snapshot created"
        );
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
            Err(error) => {
                return Err(ServiceError::context(
                    "claim direct retained-snapshot comparison",
                    error,
                ));
            }
        };
        let claim = match admission {
            HistoricalComparisonAdmission::Ready(changes) => return Ok(changes),
            HistoricalComparisonAdmission::Claimed(claim) => claim,
        };
        let source_path = self.snapshot_path(claim.from_snapshot_id)?;
        let target_path = self.snapshot_path(claim.to_snapshot_id)?;
        let source = self
            .open_subvolume(&source_path)
            .map_err(|error| ServiceError::context("open historical source", error))?;
        let target = self
            .open_subvolume(&target_path)
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
                    parent: source_expected.clone(),
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
        validate_changed_objects_proof(&parsed, &source_expected, &target_expected, &comparison)?;
        require_dirty_witness_contract(parsed.dirty_witness_contract)?;
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
        let target_objects = self.resolve_target_objects(&parsed, &required)?;
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
        let finish_cut_started = std::time::Instant::now();
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
        let mut physical_published = completion.physical_published;
        let (live_path, _indexed_base_path) = self.watch_paths(reservation.watch_id)?;
        let base_path = self.snapshot_path(reservation.base_snapshot_id)?;
        let base = self
            .open_subvolume(&base_path)
            .map_err(|error| ServiceError::context("open base snapshot", error))?;
        let target_fd = self
            .open_subvolume(&completion.destination_path)
            .map_err(|error| ServiceError::context("reopen target snapshot", error))?;
        let parent_expected = ExpectedSubvolume::from_observed(&base.filesystem, &base.subvolume);
        let target_expected =
            ExpectedSubvolume::from_observed(&target_fd.filesystem, &target_fd.subvolume);
        if parent_expected.subvolume_uuid != self.snapshot_uuid(reservation.base_snapshot_id)?
            || target_expected != *target
        {
            let error = "comparison endpoints do not match reserved immutable snapshots";
            if !physical_published {
                self.store
                    .fail_unpublished_cut(
                        reservation,
                        completion.lease_owner,
                        &recorded,
                        error,
                        completion.now_ns,
                    )
                    .map_err(|failure| {
                        ServiceError::new(format!("{error}; fail unpublished cut: {failure}"))
                    })?;
            }
            return Err(ServiceError::new(error));
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
            let (parsed, comparison_result) = if let Some(staged) = staged_manifest {
                (staged.parsed, staged.broker_result)
            } else {
                let mut spool = create_private_spool(&spool_path)?;
                let changed_objects_started = std::time::Instant::now();
                let comparison = self
                    .broker
                    .changed_objects(
                        &ChangedObjectsExecution {
                            parent: parent_expected.clone(),
                            target: target_expected.clone(),
                            output_owner_uid: unsafe { libc::geteuid() },
                            max_output_bytes: self.config.max_manifest_bytes,
                        },
                        base.as_fd(),
                        target_fd.as_fd(),
                        spool.as_fd(),
                    )
                    .map_err(|error| ServiceError::context("compare immutable snapshots", error))?;
                tracing::info!(
                    elapsed_ms = changed_objects_started.elapsed().as_millis() as u64,
                    output_bytes = comparison.output_bytes,
                    "query cut changed-object comparison completed"
                );
                let manifest_read_started = std::time::Instant::now();
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
                tracing::info!(
                    elapsed_ms = manifest_read_started.elapsed().as_millis() as u64,
                    output_bytes = comparison.output_bytes,
                    "query cut changed-object manifest read"
                );
                let manifest_parse_started = std::time::Instant::now();
                let parsed = parse_kernel_changed_objects(&bytes).map_err(|error| {
                    ServiceError::context("parse changed-object manifest", error)
                })?;
                tracing::info!(
                    elapsed_ms = manifest_parse_started.elapsed().as_millis() as u64,
                    changed_objects = parsed.manifest.objects.len(),
                    target_objects = parsed.target_objects.as_ref().map_or(0, BTreeMap::len),
                    "query cut changed-object manifest parsed"
                );
                write_manifest_stage_trailer(&mut spool, &comparison)?;
                (parsed, comparison)
            };
            validate_changed_objects_proof(
                &parsed,
                &parent_expected,
                &target_expected,
                &comparison_result,
            )?;
            // V2 proves the target remains boundary-free from the accepted
            // boundary-free base using mandatory DIR_INDEX transition
            // records. Legacy kernels have no such contract, so retain the
            // fail-closed namespace scan for them.
            if parsed.target_objects.is_none() {
                let nested_scan_started = std::time::Instant::now();
                reject_nested_subvolumes(&completion.destination_path)?;
                tracing::info!(
                    elapsed_ms = nested_scan_started.elapsed().as_millis() as u64,
                    "query cut legacy nested-subvolume scan completed"
                );
            }
            require_dirty_witness_contract(parsed.dirty_witness_contract)?;
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
            let resolve_target_objects_started = std::time::Instant::now();
            let target_objects = self.resolve_target_objects(&parsed, &required)?;
            tracing::info!(
                elapsed_ms = resolve_target_objects_started.elapsed().as_millis() as u64,
                required_objects = required.len(),
                resolved_objects = target_objects.len(),
                "query cut target objects resolved"
            );
            if let Some(relative_path) = self
                .store
                .validate_adjacent_delta_target(reservation, &parsed.manifest, &target_objects)
                .map_err(|error| ServiceError::context("validate indexed cut", error))?
            {
                return Err(fscrypt_path_error(&live_path, &relative_path));
            }
            if !physical_published {
                self.store
                    .publish_validated_physical_cut(
                        reservation,
                        completion.lease_owner,
                        &recorded,
                        completion.now_ns,
                    )
                    .map_err(|error| ServiceError::context("publish physical cut", error))?;
                physical_published = true;
            }
            let publish_started = std::time::Instant::now();
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
            tracing::info!(
                elapsed_ms = publish_started.elapsed().as_millis() as u64,
                changed_objects = parsed.manifest.objects.len(),
                target_objects = target_objects.len(),
                "query cut adjacent delta published"
            );
            // The cut is committed. Startup spool reconciliation can remove a
            // leftover file, so cleanup failure must not manufacture a false
            // publication failure and trigger a second publication attempt.
            let _ = fs::remove_file(&spool_path);
            tracing::info!(
                elapsed_ms = finish_cut_started.elapsed().as_millis() as u64,
                "query cut finished"
            );
            Ok(published)
        })();
        match incremental {
            Ok(published) => Ok(published),
            Err(error) if inject_manifest_stage_failure => Err(error),
            Err(error) if is_terminal_unpublished_cut_rejection(&error) => {
                if !physical_published {
                    self.store
                        .fail_unpublished_cut(
                            reservation,
                            completion.lease_owner,
                            &recorded,
                            &error.to_string(),
                            completion.now_ns,
                        )
                        .map_err(|failure| {
                            ServiceError::new(format!("{error}; fail unpublished cut: {failure}"))
                        })?;
                }
                Err(error)
            }
            Err(incremental_error) => {
                let full_index = (|| -> Result<SnapshotIndexResult, ServiceError> {
                    discard_private_spool(&spool_path)?;
                    // The userspace snapshot walk also certifies nested
                    // subvolume boundaries. This scan is redundant for v2,
                    // but is required before accepting a possible legacy fallback.
                    if let Err(error) = reject_nested_subvolumes(&completion.destination_path) {
                        if !physical_published && is_nested_subvolume_rejection(&error) {
                            let message = format!(
                                "incremental cut failed: {incremental_error}; full snapshot index rejected target: {error}"
                            );
                            self.store
                                .fail_unpublished_cut(
                                    reservation,
                                    completion.lease_owner,
                                    &recorded,
                                    &message,
                                    completion.now_ns,
                                )
                                .map_err(|failure| {
                                    ServiceError::new(format!(
                                        "{message}; fail unpublished cut: {failure}"
                                    ))
                                })?;
                        }
                        return Err(error);
                    }
                    self.snapshot_full_index(&target_expected, target_fd.as_fd(), &live_path)
                })();
                let full_index = match full_index {
                    Ok(full_index) => full_index,
                    Err(fallback) => {
                        let message = format!(
                            "incremental cut failed: {incremental_error}; full snapshot index failed: {fallback}"
                        );
                        // Broker I/O, spool, and decoding failures remain
                        // retryable. No physical head has been published, so
                        // recovery can resume the same deterministic intent.
                        return Err(ServiceError::new(message));
                    }
                };
                if !physical_published {
                    self.store
                        .publish_validated_physical_cut(
                            reservation,
                            completion.lease_owner,
                            &recorded,
                            completion.now_ns,
                        )
                        .map_err(|error| ServiceError::context("publish physical cut", error))?;
                }
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

    /// Runs one bounded production-maintenance slice.
    ///
    /// Database work is bounded, and broker deletion I/O stays outside a
    /// SQLite transaction behind the existing reserve/start/finish fences.
    pub fn maintenance_tick(&mut self, now_ns: i64) -> Result<MaintenanceReport, ServiceError> {
        let expired_query_leases = self
            .store
            .expire_query_leases_bounded(now_ns, self.config.maintenance_boundary_delete_limit)
            .map_err(|error| ServiceError::context("expire query leases", error))?;
        let expired_retention_leases = self
            .store
            .expire_retention_leases_bounded(now_ns, self.config.maintenance_boundary_delete_limit)
            .map_err(|error| ServiceError::context("expire retention leases", error))?;
        let expired_historical_comparisons = self
            .store
            .expire_historical_comparisons_bounded(
                now_ns,
                self.config.maintenance_boundary_delete_limit,
            )
            .map_err(|error| ServiceError::context("expire historical comparisons", error))?;
        let retained_boundaries = self.maintain_history(now_ns)?;
        let orphan_history_rows = self
            .store
            .reclaim_orphan_history_bounded(self.config.maintenance_boundary_delete_limit)
            .map_err(|error| ServiceError::context("reclaim orphan history", error))?;
        let history_rows_reclaimed = retained_boundaries
            .checked_add(orphan_history_rows)
            .ok_or_else(|| ServiceError::new("maintenance history count overflow"))?;
        let snapshots_deleted =
            self.collect_snapshots(now_ns, self.config.maintenance_snapshot_delete_limit)?;
        Ok(MaintenanceReport {
            expired_query_leases,
            expired_retention_leases,
            expired_historical_comparisons,
            watches_processed: self.last_maintenance_watches_processed,
            history_rows_reclaimed,
            snapshots_deleted,
            more_work: self.last_maintenance_more_watches
                || expired_query_leases == self.config.maintenance_boundary_delete_limit
                || expired_retention_leases == self.config.maintenance_boundary_delete_limit
                || expired_historical_comparisons == self.config.maintenance_boundary_delete_limit
                || history_rows_reclaimed >= self.config.maintenance_boundary_delete_limit
                || snapshots_deleted == self.config.maintenance_snapshot_delete_limit,
        })
    }

    pub fn garbage_collect(&mut self, now_ns: i64, limit: usize) -> Result<usize, ServiceError> {
        self.store
            .expire_query_leases_bounded(now_ns, limit)
            .map_err(|error| ServiceError::context("expire query leases", error))?;
        self.store
            .expire_retention_leases_bounded(now_ns, limit)
            .map_err(|error| ServiceError::context("expire retention leases", error))?;
        self.store
            .expire_historical_comparisons_bounded(now_ns, limit)
            .map_err(|error| ServiceError::context("expire historical comparisons", error))?;
        self.maintain_history(now_ns)?;
        self.store
            .reclaim_orphan_history_bounded(limit)
            .map_err(|error| ServiceError::context("reclaim orphan history", error))?;
        self.collect_snapshots(now_ns, limit)
    }

    /// Collects physical snapshots for daemon-free callers without applying
    /// the legacy boundary-based replay policy.
    ///
    /// Direct callers release old `fsmonitor_boundaries` as soon as their
    /// scan fd is open. Their cursors replay from `watch_cuts` and
    /// `change_events`, so running `maintain_history()` here would mistake
    /// the absence of old physical-boundary rows for an expired logical
    /// replay window and advance `replay_floor_seq` to the newest cut.
    pub fn garbage_collect_direct(
        &mut self,
        now_ns: i64,
        limit: usize,
    ) -> Result<usize, ServiceError> {
        self.store
            .expire_query_leases_bounded(now_ns, limit)
            .map_err(|error| ServiceError::context("expire query leases", error))?;
        self.store
            .expire_retention_leases_bounded(now_ns, limit)
            .map_err(|error| ServiceError::context("expire retention leases", error))?;
        self.store
            .expire_historical_comparisons_bounded(now_ns, limit)
            .map_err(|error| ServiceError::context("expire historical comparisons", error))?;
        self.store
            .reclaim_orphan_history_bounded(limit)
            .map_err(|error| ServiceError::context("reclaim orphan history", error))?;
        self.collect_snapshots(now_ns, limit)
    }

    fn collect_snapshots(&mut self, now_ns: i64, limit: usize) -> Result<usize, ServiceError> {
        if limit == 0 {
            return Err(ServiceError::new("snapshot GC limit must be positive"));
        }
        // Finish live post-effect rows before reserving more work. This keeps
        // an effect failure from wedging a snapshot in deleting until process
        // restart, while still bounding filesystem I/O per tick.
        let mut completed = self.recover_snapshot_delete_operations_bounded(limit)?;
        while completed < limit {
            // Reserve one intent at a time. If this iteration fails after its
            // effect boundary, the next tick reconciles exactly that row; no
            // later batch members are left in planned/deleting limbo.
            let mut reservations = self
                .store
                .reserve_unpinned_snapshot_deletes(
                    self.lease_owner,
                    now_ns,
                    lease_expiry(now_ns, self.config.lease_ns)?,
                    1,
                )
                .map_err(|error| ServiceError::context("reserve snapshot GC", error))?;
            let Some(reservation) = reservations.pop() else {
                break;
            };
            if let Err(error) =
                self.store
                    .start_snapshot_delete(&reservation, self.lease_owner, now_ns)
            {
                // No broker effect may start before this transition commits.
                // Best-effort rollback avoids leaving a purely planned row
                // unavailable to later GC after a local start failure.
                let _ = self
                    .store
                    .cancel_planned_snapshot_delete(&reservation, self.lease_owner);
                return Err(ServiceError::context("start snapshot GC effect", error));
            }
            self.execute_snapshot_delete(&reservation, self.lease_owner, now_ns)?;
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
        let watches = self.next_maintenance_watches()?;
        self.last_maintenance_watches_processed = watches.len();
        let mut reclaimed = 0_usize;
        for watch_id in watches {
            reclaimed += self
                .store
                .retain_exponential_replay_checkpoints(
                    watch_id,
                    self.lease_owner,
                    now_ns,
                    self.config.replay_window_cuts,
                    self.config.replay_window_ns,
                    self.config.maintenance_boundary_delete_limit,
                )
                .map_err(|error| {
                    ServiceError::context("retain exponential replay checkpoints", error)
                })?;
        }
        Ok(reclaimed)
    }

    fn next_maintenance_watches(&mut self) -> Result<Vec<[u8; 16]>, ServiceError> {
        let limit = i64::try_from(self.config.maintenance_watch_limit)
            .map_err(|_| ServiceError::new("maintenance watch limit overflow"))?;
        let total: i64 = self
            .store
            .connection()
            .query_row(
                "SELECT count(*) FROM watches WHERE state IN ('active', 'blocked')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| ServiceError::context("count maintenance watches", error))?;
        let mut rows = if let Some(after) = self.maintenance_after_watch {
            let mut statement = self
                .store
                .connection()
                .prepare(
                    "SELECT id FROM watches WHERE state IN ('active', 'blocked') AND id > ?1 ORDER BY id LIMIT ?2",
                )
                .map_err(|error| ServiceError::context("prepare maintenance watches", error))?;
            statement
                .query_map(rusqlite::params![after.as_slice(), limit], |row| {
                    row.get::<_, Vec<u8>>(0)
                })
                .map_err(|error| ServiceError::context("query maintenance watches", error))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| ServiceError::context("decode maintenance watches", error))?
        } else {
            let mut statement = self
                .store
                .connection()
                .prepare(
                    "SELECT id FROM watches WHERE state IN ('active', 'blocked') ORDER BY id LIMIT ?1",
                )
                .map_err(|error| ServiceError::context("prepare maintenance watches", error))?;
            statement
                .query_map([limit], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|error| ServiceError::context("query maintenance watches", error))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| ServiceError::context("decode maintenance watches", error))?
        };
        if let Some(after) = self.maintenance_after_watch
            && rows.len() < self.config.maintenance_watch_limit
        {
            let remaining = i64::try_from(self.config.maintenance_watch_limit - rows.len())
                .map_err(|_| ServiceError::new("maintenance wrap limit overflow"))?;
            let mut statement = self
                    .store
                    .connection()
                    .prepare(
                        "SELECT id FROM watches WHERE state IN ('active', 'blocked') AND id <= ?1 ORDER BY id LIMIT ?2",
                    )
                    .map_err(|error| {
                        ServiceError::context("prepare wrapped maintenance watches", error)
                    })?;
            let wrapped = statement
                .query_map(rusqlite::params![after.as_slice(), remaining], |row| {
                    row.get::<_, Vec<u8>>(0)
                })
                .map_err(|error| ServiceError::context("query wrapped maintenance watches", error))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    ServiceError::context("decode wrapped maintenance watches", error)
                })?;
            rows.extend(wrapped);
        }
        let watches = rows
            .iter()
            .map(|bytes| fixed_service_blob(bytes, "history-maintenance watch ID"))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(last) = watches.last() {
            self.maintenance_after_watch = Some(*last);
        }
        self.last_maintenance_more_watches = usize::try_from(total)
            .map(|count| count > watches.len())
            .unwrap_or(true);
        Ok(watches)
    }

    fn execute_snapshot_delete(
        &mut self,
        reservation: &SnapshotDeleteReservation,
        lease_owner: [u8; 16],
        now_ns: i64,
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
        if path
            .try_exists()
            .map_err(|error| ServiceError::context("inspect snapshot GC target", error))?
        {
            let name = path
                .file_name()
                .ok_or_else(|| ServiceError::new("snapshot GC path has no basename"))?
                .as_bytes()
                .to_vec();
            let target = self
                .open_subvolume(&path)
                .map_err(|error| ServiceError::context("open snapshot GC target", error))?;
            let expected = ExpectedSubvolume::from_observed(&target.filesystem, &target.subvolume);
            verify_recorded_snapshot_for_delete(&reservation.identity, &expected)?;
            if expected.readonly {
                set_subvolume_readonly(target.as_fd(), false).map_err(|error| {
                    ServiceError::context("make managed snapshot writable for deletion", error)
                })?;
            }
            if let Err(error) = destroy_snapshot(destination.as_fd(), &name) {
                if expected.readonly {
                    let restore_error = set_subvolume_readonly(target.as_fd(), true).err();
                    return Err(ServiceError::new(match restore_error {
                        Some(restore_error) => format!(
                            "delete managed snapshot: {error}; also failed to restore read-only flag: {restore_error}"
                        ),
                        None => format!("delete managed snapshot: {error}"),
                    }));
                }
                return Err(ServiceError::context("delete managed snapshot", error));
            }
        }
        if path
            .try_exists()
            .map_err(|error| ServiceError::context("verify snapshot GC deletion", error))?
        {
            return Err(ServiceError::new(
                "snapshot still exists after direct deletion",
            ));
        }
        self.store
            .record_snapshot_delete_durable(reservation, lease_owner, now_ns)
            .map_err(|error| ServiceError::context("record durable snapshot GC", error))
    }

    fn create_snapshot(
        &mut self,
        source: &OpenedSubvolume,
        destination_name: &[u8],
        readonly: bool,
    ) -> Result<ExpectedSubvolume, ServiceError> {
        let destination = File::open(&self.config.managed_snapshot_directory)
            .map_err(|error| ServiceError::context("open managed snapshot directory", error))?;
        create_btrfs_snapshot(
            source.as_fd(),
            destination.as_fd(),
            destination_name,
            readonly,
        )
        .map_err(|error| ServiceError::context("create snapshot", error))?;
        let path = self
            .config
            .managed_snapshot_directory
            .join(std::ffi::OsString::from_vec(destination_name.to_vec()));
        let snapshot = self
            .open_subvolume(&path)
            .map_err(|error| ServiceError::context("open created snapshot", error))?;
        let expected = ExpectedSubvolume::from_observed(&snapshot.filesystem, &snapshot.subvolume);
        if expected.parent_uuid != Some(source.subvolume.uuid) || expected.readonly != readonly {
            return Err(ServiceError::new(
                "directly created snapshot identity does not match its request",
            ));
        }
        Ok(expected)
    }

    /// Returns a directly-created snapshot which survived a manager crash
    /// after the Btrfs ioctl but before its SQLite identity was recorded.
    fn recover_existing_snapshot(
        &self,
        path: &Path,
        source_subvolume_uuid: [u8; 16],
    ) -> Result<Option<ExpectedSubvolume>, ServiceError> {
        if !path
            .try_exists()
            .map_err(|error| ServiceError::context("inspect recovery snapshot", error))?
        {
            return Ok(None);
        }
        let snapshot = self
            .open_subvolume(path)
            .map_err(|error| ServiceError::context("open recovery snapshot", error))?;
        let expected = ExpectedSubvolume::from_observed(&snapshot.filesystem, &snapshot.subvolume);
        if expected.parent_uuid != Some(source_subvolume_uuid) || !expected.readonly {
            return Err(ServiceError::new(
                "recovery snapshot does not match its direct-create intent",
            ));
        }
        Ok(Some(expected))
    }

    fn snapshot_full_index(
        &self,
        expected: &ExpectedSubvolume,
        snapshot: BorrowedFd<'_>,
        display_root: &Path,
    ) -> Result<SnapshotIndexResult, ServiceError> {
        self.snapshot_full_index_with_progress(expected, snapshot, display_root, &mut |_| {})
    }

    fn snapshot_full_index_with_progress(
        &self,
        expected: &ExpectedSubvolume,
        snapshot: BorrowedFd<'_>,
        display_root: &Path,
        progress: &mut dyn FnMut(SnapshotWalkProgress),
    ) -> Result<SnapshotIndexResult, ServiceError> {
        verify_snapshot_endpoint(snapshot, expected)?;
        let index = match read_snapshot_index_with_progress(snapshot, progress) {
            Ok(index) => index,
            Err(SnapshotIndexError::FscryptDirectory(relative_path)) => {
                return Err(fscrypt_path_error(display_root, &relative_path));
            }
            Err(error) => {
                return Err(ServiceError::context(
                    "walk immutable snapshot index",
                    error,
                ));
            }
        };
        verify_snapshot_endpoint(snapshot, expected)?;
        Ok(SnapshotIndexResult { index })
    }

    fn resolve_target_objects(
        &self,
        parsed: &ParsedKernelChangedObjects,
        required: &BTreeSet<u64>,
    ) -> Result<BTreeMap<u64, Object>, ServiceError> {
        let stream_objects = parsed.target_objects.as_ref().ok_or_else(|| {
            ServiceError::new(
                "legacy changed-object streams are unsupported without target objects",
            )
        })?;
        let objects: BTreeMap<_, _> = required
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
            .collect::<Result<_, _>>()?;
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

    pub(crate) fn snapshot_path(&self, snapshot_id: i64) -> Result<PathBuf, ServiceError> {
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

fn connect_external_broker_with_retry(
    socket_path: &Path,
    manager_store_uuid: [u8; 16],
    retry_window: std::time::Duration,
    retry_interval: std::time::Duration,
) -> Result<BrokerClient, ServiceError> {
    retry_external_broker_startup(retry_window, retry_interval, || {
        SeqPacket::connect(socket_path)
            .map_err(|error| ServiceError::context("connect external broker", error))
            .and_then(|socket| {
                BrokerClient::connect(socket, manager_store_uuid)
                    .map_err(|error| ServiceError::context("handshake with broker", error))
            })
    })
}

fn retry_external_broker_startup<T>(
    retry_window: std::time::Duration,
    retry_interval: std::time::Duration,
    mut attempt: impl FnMut() -> Result<T, ServiceError>,
) -> Result<T, ServiceError> {
    let started = std::time::Instant::now();
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(_error) if started.elapsed() < retry_window => {
                std::thread::sleep(
                    retry_interval.min(retry_window.saturating_sub(started.elapsed())),
                );
            }
            Err(error) => {
                return Err(ServiceError::new(format!(
                    "external broker did not become ready within {} ms: {error}",
                    retry_window.as_millis()
                )));
            }
        }
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

/// A crash can land after GC made a read-only baseline writable but before it
/// removed the subvolume. Every immutable identity field must still match;
/// only the read-only bit may differ while the durable row is deleting.
fn verify_recorded_snapshot_for_delete(
    recorded: &SnapshotIdentity,
    observed: &ExpectedSubvolume,
) -> Result<(), ServiceError> {
    let mut observed = observed.clone();
    observed.readonly = recorded.readonly;
    verify_recorded_snapshot(recorded, &observed)
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

fn is_nested_subvolume_rejection(error: &ServiceError) -> bool {
    error
        .message
        .starts_with("immutable snapshot contains nested subvolume")
}

fn is_terminal_unpublished_cut_rejection(error: &ServiceError) -> bool {
    let message = error
        .message
        .strip_prefix("parse changed-object manifest: ")
        .unwrap_or(&error.message);
    [
        "parse changed-objects v2 stream:",
        "changed-objects v2 ",
        "legacy changed-object stream has a v2 ioctl completion",
        "immutable snapshot contains nested subvolume",
        "immutable snapshot contains fscrypt directory ",
        "materialize v2 target object:",
        "changed-objects stream does not advertise the dirty-witness capability",
    ]
    .iter()
    .any(|prefix| message.starts_with(prefix))
}

fn fscrypt_path_error(display_root: &Path, relative_path: &[u8]) -> ServiceError {
    let mut path = display_root.to_path_buf();
    if !relative_path.is_empty() {
        path.push(std::ffi::OsString::from_vec(relative_path.to_vec()));
    }
    ServiceError::new(format!(
        "immutable snapshot contains fscrypt directory {}; remove or move that directory before retrying AWACS indexing",
        path.display()
    ))
}

fn verify_snapshot_endpoint(
    snapshot: BorrowedFd<'_>,
    expected: &ExpectedSubvolume,
) -> Result<(), ServiceError> {
    let filesystem = filesystem_info(snapshot)
        .map_err(|error| ServiceError::context("inspect snapshot index filesystem", error))?;
    let subvolume = subvolume_info(snapshot)
        .map_err(|error| ServiceError::context("inspect snapshot index subvolume", error))?;
    if ExpectedSubvolume::from_observed(&filesystem, &subvolume) != *expected {
        return Err(ServiceError::new(
            "snapshot index endpoint changed during userspace walk",
        ));
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
    result: &ChangedObjectsResult,
) -> Result<(), ServiceError> {
    let end = spool
        .seek(SeekFrom::End(0))
        .map_err(|error| ServiceError::context("seek manifest stage end", error))?;
    if end != result.output_bytes {
        return Err(ServiceError::new(
            "manifest stage length changed before completion",
        ));
    }
    let (flags, ioctl_bytes, ioctl_records) = match result.v2_ioctl {
        Some(ioctl) => (
            MANIFEST_STAGE_V2_IOCTL,
            ioctl.output_bytes,
            ioctl.output_records,
        ),
        None => (0, 0, 0),
    };
    spool
        .write_all(MANIFEST_STAGE_TRAILER_MAGIC)
        .and_then(|()| spool.write_all(&result.output_bytes.to_le_bytes()))
        .and_then(|()| spool.write_all(&result.manifest_hash))
        .and_then(|()| spool.write_all(&flags.to_le_bytes()))
        .and_then(|()| spool.write_all(&ioctl_bytes.to_le_bytes()))
        .and_then(|()| spool.write_all(&ioctl_records.to_le_bytes()))
        .and_then(|()| spool.sync_all())
        .map_err(|error| ServiceError::context("durably complete manifest stage", error))
}

fn load_staged_manifest(
    path: &Path,
    max_manifest_bytes: u64,
) -> Result<Option<StagedKernelChangedObjects>, ServiceError> {
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
    let valid = (|| -> Option<(&[u8], ChangedObjectsResult)> {
        let split = bytes.len().checked_sub(MANIFEST_STAGE_TRAILER_LEN)?;
        let (manifest, trailer) = bytes.split_at(split);
        if trailer.get(..16)? != MANIFEST_STAGE_TRAILER_MAGIC {
            return None;
        }
        let declared_len = u64::from_le_bytes(trailer.get(16..24)?.try_into().ok()?);
        let expected_hash: [u8; 32] = trailer.get(24..56)?.try_into().ok()?;
        let flags = u64::from_le_bytes(trailer.get(56..64)?.try_into().ok()?);
        let ioctl_bytes = u64::from_le_bytes(trailer.get(64..72)?.try_into().ok()?);
        let ioctl_records = u64::from_le_bytes(trailer.get(72..80)?.try_into().ok()?);
        if declared_len != manifest.len() as u64 || hash_bytes(manifest) != expected_hash {
            return None;
        }
        let v2_ioctl = match flags {
            0 if ioctl_bytes == 0 && ioctl_records == 0 => None,
            MANIFEST_STAGE_V2_IOCTL => Some(ChangedObjectsIoctlResult {
                output_bytes: ioctl_bytes,
                output_records: ioctl_records,
            }),
            _ => return None,
        };
        Some((
            manifest,
            ChangedObjectsResult {
                output_bytes: declared_len,
                manifest_hash: expected_hash,
                v2_ioctl,
            },
        ))
    })();
    let Some((manifest_bytes, broker_result)) = valid else {
        discard_private_spool(path)?;
        return Ok(None);
    };
    match parse_kernel_changed_objects(manifest_bytes) {
        Ok(parsed) => Ok(Some(StagedKernelChangedObjects {
            parsed,
            broker_result,
        })),
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
            v2_proof: Some(ParsedV2Proof {
                header: parsed.header,
                completion: parsed.completion,
            }),
        })
    } else {
        parse_changed_objects(bytes)
            .map(|manifest| ParsedKernelChangedObjects {
                manifest,
                target_objects: None,
                dirty_witness_contract: false,
                v2_proof: None,
            })
            .map_err(|error| ServiceError::context("parse legacy changed-object stream", error))
    }
}

fn validate_changed_objects_proof(
    parsed: &ParsedKernelChangedObjects,
    parent: &ExpectedSubvolume,
    target: &ExpectedSubvolume,
    broker_result: &ChangedObjectsResult,
) -> Result<(), ServiceError> {
    let Some(proof) = parsed.v2_proof else {
        if broker_result.v2_ioctl.is_some() {
            return Err(ServiceError::new(
                "legacy changed-object stream has a v2 ioctl completion",
            ));
        }
        return Ok(());
    };
    let Some(ioctl) = broker_result.v2_ioctl else {
        return Err(ServiceError::new(
            "changed-objects v2 stream lacks a broker ioctl completion",
        ));
    };
    if proof.header.fs_uuid != parent.filesystem_uuid
        || proof.header.fs_uuid != target.filesystem_uuid
        || proof.header.source_uuid != parent.subvolume_uuid
        || proof.header.target_uuid != target.subvolume_uuid
        || proof.header.source_ctransid != parent.ctransid
        || proof.header.target_ctransid != target.ctransid
        || proof.header.source_root_id != parent.root_id
        || proof.header.target_root_id != target.root_id
    {
        return Err(ServiceError::new(
            "changed-objects v2 endpoint header does not match broker-verified endpoints",
        ));
    }
    if proof.completion.output_bytes() != Some(broker_result.output_bytes)
        || ioctl.output_bytes != broker_result.output_bytes
        || proof.completion.record_count != ioctl.output_records
    {
        return Err(ServiceError::new(
            "changed-objects v2 completion counters do not match broker ioctl result",
        ));
    }
    Ok(())
}

fn require_dirty_witness_contract(observed: bool) -> Result<(), ServiceError> {
    if !observed {
        return Err(ServiceError::new(
            "changed-objects stream does not advertise the dirty-witness capability",
        ));
    }
    Ok(())
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
    [b"cut-initialize-".as_slice(), b"cut-"]
        .into_iter()
        .any(|prefix| {
            name.strip_prefix(prefix)
                .is_some_and(|id| id.len() == 32 && id.iter().all(u8::is_ascii_hexdigit))
        })
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
    use crate::manifest::{
        CHANGED_OBJECTS_MAGIC, CHANGED_OBJECTS_V2_MAGIC, CHANGED_OBJECTS_V2_VERSION,
        CHANGED_OBJECTS_VERSION,
    };
    use crate::store::ServiceMetadata;
    use tempfile::tempdir;

    #[test]
    fn external_broker_startup_retries_until_ready() {
        let mut attempts = 0;
        let started = std::time::Instant::now();
        retry_external_broker_startup(
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(5),
            || {
                attempts += 1;
                if attempts < 4 {
                    Err(ServiceError::new("broker socket is not ready"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert_eq!(attempts, 4);
        assert!(started.elapsed() >= std::time::Duration::from_millis(15));
    }

    #[test]
    fn dirty_witness_capability_is_required_for_incremental_streams() {
        assert!(require_dirty_witness_contract(true).is_ok());
        let error = require_dirty_witness_contract(false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "changed-objects stream does not advertise the dirty-witness capability"
        );
    }

    #[test]
    fn fscrypt_error_includes_live_path() {
        let error = fscrypt_path_error(Path::new("/repo"), b"encrypted");
        assert!(error.to_string().contains("/repo/encrypted"));
    }

    #[test]
    fn nested_subvolume_rejections_are_terminal_for_legacy_and_v2_streams() {
        assert!(is_nested_subvolume_rejection(&ServiceError::new(
            "immutable snapshot contains nested subvolume /tmp/child",
        )));
        assert!(is_nested_subvolume_rejection(&ServiceError::new(
            "immutable snapshot contains nested subvolume boundary parent=1 child_root=2 name=[]",
        )));
    }

    #[test]
    fn maintenance_tick_reports_a_bounded_idle_slice() {
        let temp = tempdir().unwrap();
        let managed = temp.path().join("managed");
        let spool = temp.path().join("spool");
        fs::create_dir(&managed).unwrap();
        fs::create_dir(&spool).unwrap();
        fs::set_permissions(&managed, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&spool, fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = ServiceMetadata {
            store_uuid: [21; 16],
            clock_hmac_key: [22; 32],
            clock_format_version: 1,
            last_boot_id: [23; 16],
            created_ns: 1,
        };
        let store = Store::create(&temp.path().join("state.sqlite3"), &metadata).unwrap();
        let config = ServiceConfig::new(managed, spool, [23; 16]).with_maintenance_limits(1, 1, 1);
        let mut service = Service::new(store, config).unwrap();
        assert_eq!(
            service.maintenance_tick(1_000).unwrap(),
            MaintenanceReport::default()
        );
    }

    #[test]
    fn startup_spool_sweep_removes_only_exact_private_formats() {
        let spool = tempdir().unwrap();
        for name in [
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

        assert_eq!(cleanup_stale_spool_files(spool.path()).unwrap(), 2);
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
        let result = ChangedObjectsResult {
            output_bytes: manifest.len() as u64,
            manifest_hash: hash_bytes(&manifest),
            v2_ioctl: None,
        };
        write_manifest_stage_trailer(&mut complete, &result).unwrap();
        drop(complete);
        let reused = load_staged_manifest(&complete_path, 1024).unwrap().unwrap();
        assert!(reused.parsed.manifest.objects.is_empty());
        assert!(reused.broker_result.v2_ioctl.is_none());
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
    fn v2_endpoint_and_completion_proof_rejects_normal_mismatches() {
        let bytes = empty_v2_stream();
        let parsed = parse_kernel_changed_objects(&bytes).unwrap();
        let parent = expected_subvolume([1; 16], [2; 16], 256, 10);
        let target = expected_subvolume([1; 16], [3; 16], 257, 11);
        let result = v2_result(&bytes, 0);
        validate_changed_objects_proof(&parsed, &parent, &target, &result).unwrap();

        let wrong_parent = expected_subvolume([1; 16], [9; 16], 256, 10);
        let endpoint_error =
            validate_changed_objects_proof(&parsed, &wrong_parent, &target, &result).unwrap_err();
        assert!(endpoint_error.to_string().contains("endpoint header"));

        let mut wrong_bytes = result.clone();
        wrong_bytes.v2_ioctl.as_mut().unwrap().output_bytes += 1;
        let bytes_error =
            validate_changed_objects_proof(&parsed, &parent, &target, &wrong_bytes).unwrap_err();
        assert!(bytes_error.to_string().contains("completion counters"));

        let mut wrong_records = result;
        wrong_records.v2_ioctl.as_mut().unwrap().output_records += 1;
        let records_error =
            validate_changed_objects_proof(&parsed, &parent, &target, &wrong_records).unwrap_err();
        assert!(records_error.to_string().contains("completion counters"));
    }

    #[test]
    fn recovered_v2_stage_keeps_ioctl_proof_and_rejects_counter_mismatch() {
        let spool = tempdir().unwrap();
        let path = spool
            .path()
            .join("manifest-0123456789abcdef0123456789abcdef-7.part");
        let bytes = empty_v2_stream();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(&bytes).unwrap();
        let mut result = v2_result(&bytes, 0);
        result.v2_ioctl.as_mut().unwrap().output_records = 1;
        write_manifest_stage_trailer(&mut file, &result).unwrap();
        drop(file);

        let staged = load_staged_manifest(&path, 1024).unwrap().unwrap();
        assert_eq!(staged.broker_result.v2_ioctl.unwrap().output_records, 1);
        let parent = expected_subvolume([1; 16], [2; 16], 256, 10);
        let target = expected_subvolume([1; 16], [3; 16], 257, 11);
        let error =
            validate_changed_objects_proof(&staged.parsed, &parent, &target, &staged.broker_result)
                .unwrap_err();
        assert!(error.to_string().contains("completion counters"));
    }

    #[test]
    fn v2_proof_and_invalid_target_failures_are_terminal_before_publication() {
        for message in [
            "changed-objects v2 endpoint header does not match broker-verified endpoints",
            "changed-objects v2 completion counters do not match broker ioctl result",
            "parse changed-objects v2 stream: changed-objects v2 completion mismatch",
            "parse changed-object manifest: changed-objects v2 endpoint header does not match broker-verified endpoints",
            "immutable snapshot contains nested subvolume boundary parent=1 child_root=2 name=[]",
            "immutable snapshot contains fscrypt directory /repo/encrypted",
        ] {
            assert!(
                is_terminal_unpublished_cut_rejection(&ServiceError::new(message)),
                "{message}"
            );
        }
    }

    fn expected_subvolume(
        filesystem_uuid: [u8; 16],
        subvolume_uuid: [u8; 16],
        root_id: u64,
        ctransid: u64,
    ) -> ExpectedSubvolume {
        ExpectedSubvolume {
            filesystem_uuid,
            subvolume_uuid,
            root_id,
            generation: 1,
            ctransid,
            otransid: 1,
            parent_uuid: None,
            received_uuid: None,
            readonly: true,
        }
    }

    fn v2_result(bytes: &[u8], output_records: u64) -> ChangedObjectsResult {
        ChangedObjectsResult {
            output_bytes: bytes.len() as u64,
            manifest_hash: hash_bytes(bytes),
            v2_ioctl: Some(ChangedObjectsIoctlResult {
                output_bytes: bytes.len() as u64,
                output_records,
            }),
        }
    }

    fn empty_v2_stream() -> Vec<u8> {
        let mut bytes = CHANGED_OBJECTS_V2_MAGIC.to_vec();
        push_u32(&mut bytes, CHANGED_OBJECTS_V2_VERSION);
        push_u32(&mut bytes, 112);
        push_u64(&mut bytes, (1 << 1) | (1 << 2));
        bytes.extend_from_slice(&[1; 16]);
        bytes.extend_from_slice(&[2; 16]);
        bytes.extend_from_slice(&[3; 16]);
        push_u64(&mut bytes, 10);
        push_u64(&mut bytes, 11);
        push_u64(&mut bytes, 256);
        push_u64(&mut bytes, 257);
        let stream_bytes = bytes.len() as u64;
        let checksum = test_crc32c(&bytes);
        bytes.extend_from_slice(&0xffff_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        push_u32(&mut bytes, 32);
        push_u64(&mut bytes, 0);
        push_u64(&mut bytes, stream_bytes);
        push_u32(&mut bytes, checksum);
        push_u32(&mut bytes, 0);
        bytes
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u64(bytes: &mut Vec<u8>, value: u64) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn test_crc32c(bytes: &[u8]) -> u32 {
        let mut table = [0_u32; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = (value >> 1) ^ (0x82f6_3b78 & (0_u32.wrapping_sub(value & 1)));
            }
            *entry = value;
        }
        let mut crc = !0_u32;
        for byte in bytes {
            crc = table[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8);
        }
        !crc
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
