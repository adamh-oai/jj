use crate::btrfs::SubvolumeInfo;
use crate::compat::{project_events, ClientFlavor};
use crate::index::{
    apply_manifest, object_security_digest, object_state_digest, reference_state_digest,
    xor_digest, Event, EventKind, Index, Object, ROOT_INO,
};
use crate::manifest::{
    ChangedObjectsManifest, Reference, CHANGE_CREATED, CHANGE_INODE, CHANGE_REF,
};
use crate::namespace::ViewBinding;
use crate::store::{decode_u64, encode_u64, Store, StoreError};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use uuid::Uuid;

pub const PERMISSION_READ: u8 = 0x01;
pub const PERMISSION_CUT: u8 = 0x02;
pub const PERMISSION_WORKTREE: u8 = 0x04;
pub const PERMISSION_TRIGGER: u8 = 0x08;
pub const PERMISSION_RETAIN: u8 = 0x10;
pub const PERMISSION_ADMIN: u8 = 0x20;
pub const PERMISSION_MASK: u8 = 0x3f;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Permissions(u8);

impl Permissions {
    pub fn new(bits: u8) -> Result<Self, ManagerError> {
        if bits == 0 || bits & !PERMISSION_MASK != 0 {
            return Err(ManagerError::new(format!(
                "invalid grant permission mask {bits:#x}"
            )));
        }
        Ok(Self(bits))
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn contains(self, required: u8) -> bool {
        self.0 & required == required
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Principal {
    Uid(u64),
    Service(Vec<u8>),
}

impl Principal {
    fn kind_and_id(&self) -> (&'static str, Vec<u8>) {
        match self {
            Self::Uid(uid) => ("uid", encode_u64(*uid).to_vec()),
            Self::Service(id) => ("service", id.clone()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializeRequest {
    pub fs_uuid: [u8; 16],
    pub source_subvol_uuid: [u8; 16],
    pub source_path: Vec<u8>,
    pub reserved_snapshot_path: Vec<u8>,
    pub principal: Principal,
    pub permissions: Permissions,
    pub requester_uid: u32,
    pub requester_gid: u32,
    pub lease_owner: [u8; 16],
    pub now_ns: i64,
    pub lease_expires_ns: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializeReservation {
    pub filesystem_id: i64,
    pub watch_id: [u8; 16],
    pub grant_id: [u8; 16],
    pub operation_id: [u8; 16],
    pub clock_epoch: [u8; 16],
    pub operation_fence: i64,
    pub topology_fence: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotIdentity {
    pub fs_uuid: [u8; 16],
    pub subvol_uuid: [u8; 16],
    pub parent_uuid: Option<[u8; 16]>,
    pub received_uuid: Option<[u8; 16]>,
    pub root_id: u64,
    pub ctransid: u64,
    pub otransid: u64,
    pub path: Vec<u8>,
    pub readonly: bool,
    pub created_ns: i64,
}

impl SnapshotIdentity {
    pub fn from_subvolume(
        fs_uuid: [u8; 16],
        subvolume: &SubvolumeInfo,
        path: Vec<u8>,
        created_ns: i64,
    ) -> Self {
        Self {
            fs_uuid,
            subvol_uuid: subvolume.uuid,
            parent_uuid: subvolume.parent_uuid,
            received_uuid: subvolume.received_uuid,
            root_id: subvolume.root_id,
            ctransid: subvolume.ctransid,
            otransid: subvolume.otransid,
            path,
            readonly: subvolume.readonly(),
            created_ns,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedSnapshot {
    pub snapshot_id: i64,
    pub identity: SnapshotIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializedWatch {
    pub watch_id: [u8; 16],
    pub grant_id: [u8; 16],
    pub revision_id: i64,
    pub snapshot_id: i64,
    pub sequence: i64,
    pub fresh_instance: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionLease {
    pub id: [u8; 16],
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub snapshot_id: i64,
    pub lease_fence: i64,
    pub expires_ns: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CutRequest {
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub reserved_snapshot_path: Vec<u8>,
    pub requester_uid: u32,
    pub requester_gid: u32,
    pub lease_owner: [u8; 16],
    pub now_ns: i64,
    pub lease_expires_ns: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CutReservation {
    pub filesystem_id: i64,
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub operation_id: [u8; 16],
    pub sequence: i64,
    pub base_snapshot_id: i64,
    pub source_subvol_uuid: [u8; 16],
    pub operation_fence: i64,
    pub cut_fence: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CutAdmission {
    pub id: [u8; 16],
    pub reservation: CutReservation,
    pub requester_session_id: [u8; 16],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedCut {
    pub watch_id: [u8; 16],
    pub sequence: i64,
    pub snapshot_id: i64,
    pub revision_id: i64,
    pub comparison_id: i64,
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalChanges {
    pub watch_id: [u8; 16],
    pub from_snapshot_uuid: [u8; 16],
    pub to_snapshot_uuid: [u8; 16],
    pub from_sequence: i64,
    pub to_sequence: i64,
    pub fresh_instance: bool,
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalComparisonClaim {
    pub comparison_id: i64,
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub from_snapshot_uuid: [u8; 16],
    pub to_snapshot_uuid: [u8; 16],
    pub from_snapshot_id: i64,
    pub to_snapshot_id: i64,
    pub from_revision_id: i64,
    pub to_revision_id: i64,
    pub from_sequence: i64,
    pub to_sequence: i64,
    pub lease_owner: [u8; 16],
    pub lease_fence: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalComparisonRequest {
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub requester_uid: u32,
    pub from_snapshot_uuid: [u8; 16],
    pub to_snapshot_uuid: [u8; 16],
    pub lease_owner: [u8; 16],
    pub now_ns: i64,
    pub lease_expires_ns: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoricalComparisonAdmission {
    Ready(HistoricalChanges),
    Claimed(HistoricalComparisonClaim),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotDeleteReservation {
    pub operation_id: [u8; 16],
    pub snapshot_id: i64,
    pub filesystem_id: i64,
    pub operation_fence: i64,
    pub identity: SnapshotIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreePolicy {
    pub policy_id: [u8; 16],
    pub destination_fs_uuid: [u8; 16],
    pub destination_root_subvol_uuid: [u8; 16],
    pub destination_root_path: Vec<u8>,
    pub destination_root_generation: u64,
    pub metadata_policy: String,
    pub allow_idmapped: bool,
    pub policy_hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeRequest {
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub policy: WorktreePolicy,
    pub staged_path: Vec<u8>,
    pub final_path: Vec<u8>,
    pub destination_parent_subvol_uuid: [u8; 16],
    pub destination_parent_ino: u64,
    pub destination_parent_generation: u64,
    pub destination_name: Vec<u8>,
    pub reservation_name: Vec<u8>,
    pub reservation_ino: u64,
    pub reservation_generation: u64,
    pub reservation_nonce: [u8; 32],
    pub requester_uid: u32,
    pub requester_gid: u32,
    pub lease_owner: [u8; 16],
    pub now_ns: i64,
    pub lease_expires_ns: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeReservation {
    pub operation_id: [u8; 16],
    pub worktree_id: [u8; 16],
    pub filesystem_id: i64,
    pub seed_snapshot_id: i64,
    pub seed_revision_id: i64,
    pub seed_subvol_uuid: [u8; 16],
    pub operation_fence: i64,
    pub policy_hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackedWorktree {
    pub worktree_id: [u8; 16],
    pub watch_id: [u8; 16],
    pub grant_id: [u8; 16],
    pub seed_revision_id: i64,
    pub seed_snapshot_id: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacadeActivation {
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub clock_epoch: [u8; 16],
    pub monitor_session_id: [u8; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardCursor {
    pub epoch: [u8; 16],
    pub sequence: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationHint {
    Path(Vec<u8>),
    DirectoryPrefix(Vec<u8>),
    Object {
        path: Vec<u8>,
        ino: u64,
        generation: u64,
    },
    FullInvalidation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvedWorktreeSeed {
    pub worktree_id: [u8; 16],
    pub snapshot_uuid: [u8; 16],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryLeaseReservation {
    pub id: [u8; 16],
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub clock_epoch: [u8; 16],
    pub from_sequence: Option<i64>,
    pub to_sequence: i64,
    pub guard: Option<([u8; 16], i64, i64)>,
    pub lease_owner: [u8; 16],
    pub lease_fence: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerRun {
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub through_sequence: i64,
    pub run_owner: [u8; 16],
    pub run_fence: i64,
}

type WatchGrantIds = ([u8; 16], [u8; 16]);
type EncodedMutationHint<'a> = (&'a str, Option<&'a [u8]>, Option<[u8; 8]>, Option<[u8; 8]>);
type GuardBoundaryRow = (i64, Vec<u8>, String, Option<Vec<u8>>, Option<i64>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryReport {
    pub invalidated_facades: usize,
    pub released_queries: usize,
    pub reclaimed_trigger_runs: usize,
    pub abandoned_historical_comparisons: usize,
    pub boot_changed: bool,
}

struct WorktreeHead {
    filesystem_id: i64,
    revision_id: i64,
    snapshot_id: i64,
    indexed_seq: i64,
    last_cut_seq: i64,
    single_owner_uid: Option<Vec<u8>>,
    privileged_metadata_count: i64,
    subvol_uuid: Vec<u8>,
}

struct RevisionMetadata {
    summary_version: i64,
    owner_cardinality: i64,
    owner_uid_xor: u64,
    object_count: i64,
    ref_count: i64,
    state_hash: [u8; 32],
    single_owner_uid: Option<u64>,
    privileged_metadata_count: i64,
    security_state_hash: [u8; 32],
}

struct PlannedCutAdmissionRow {
    filesystem_id: i64,
    authorization_id: Vec<u8>,
    sequence: i64,
    base_snapshot_id: i64,
    source_subvol_uuid: Vec<u8>,
    operation_fence: i64,
    operation_id: Vec<u8>,
    cut_fence: i64,
}

pub fn worktree_policy_hash(policy: &WorktreePolicy) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-worktree-policy-v1\0");
    hash.update(policy.policy_id);
    hash.update(policy.destination_fs_uuid);
    hash.update(policy.destination_root_subvol_uuid);
    hash.update((policy.destination_root_path.len() as u64).to_be_bytes());
    hash.update(&policy.destination_root_path);
    hash.update(policy.destination_root_generation.to_be_bytes());
    hash.update((policy.metadata_policy.len() as u64).to_be_bytes());
    hash.update(policy.metadata_policy.as_bytes());
    hash.update([u8::from(policy.allow_idmapped)]);
    hash.finalize().into()
}

#[derive(Debug)]
pub struct ManagerError {
    message: String,
}

impl ManagerError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ManagerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ManagerError {}

impl From<rusqlite::Error> for ManagerError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<StoreError> for ManagerError {
    fn from(error: StoreError) -> Self {
        Self::new(error.to_string())
    }
}

impl Store {
    pub fn active_uid_watch_at_path(
        &self,
        live_path: &[u8],
        requester_uid: u32,
        required_permissions: u8,
    ) -> Result<Option<WatchGrantIds>, ManagerError> {
        if live_path.is_empty()
            || required_permissions == 0
            || required_permissions & !PERMISSION_MASK != 0
        {
            return Err(ManagerError::new("invalid active-watch lookup"));
        }
        let principal = encode_u64(u64::from(requester_uid));
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .connection()
            .query_row(
                r#"SELECT w.id, g.id
                     FROM watches w JOIN watch_grants g ON g.watch_id = w.id
                    WHERE w.live_path = ?1 AND w.state = 'active'
                      AND g.principal_kind = 'uid' AND g.principal_id = ?2
                      AND g.state = 'active' AND (g.permissions & ?3) = ?3
                    ORDER BY g.created_ns DESC LIMIT 1"#,
                params![
                    live_path,
                    principal.as_slice(),
                    i64::from(required_permissions),
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(watch, grant)| {
            Ok((
                fixed_manager_blob(&watch, "watch ID")?,
                fixed_manager_blob(&grant, "grant ID")?,
            ))
        })
        .transpose()
    }

    /// Abandons intents which never crossed the durable pre-effect boundary.
    /// The caller must first obtain the broker session fence/drain barrier so
    /// absence of a receipt remains proof that no old request can start.
    pub fn abort_planned_operations(&mut self, now_ns: i64) -> Result<usize, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE cut_admissions SET state = 'abandoned' \
             WHERE state = 'waiting' AND expires_ns <= ?1",
            [now_ns],
        )?;
        let operation_count: i64 = transaction.query_row(
            "SELECT count(*) FROM operations WHERE state = 'planned'",
            [],
            |row| row.get(0),
        )?;
        let delete_count: i64 = transaction.query_row(
            "SELECT count(*) FROM snapshot_delete_operations WHERE state = 'planned'",
            [],
            |row| row.get(0),
        )?;

        transaction.execute(
            r#"UPDATE watches
                  SET cut_owner = NULL, cut_expires_ns = NULL
                WHERE EXISTS (
                    SELECT 1 FROM operations o
                     WHERE o.kind = 'cut' AND o.state = 'planned'
                       AND o.watch_id = watches.id
                       AND o.lease_owner = watches.cut_owner
                )"#,
            [],
        )?;
        transaction.execute(
            "DELETE FROM snapshot_pins WHERE owner_kind = 'operation' \
             AND owner_id IN (SELECT id FROM operations WHERE state = 'planned')",
            [],
        )?;
        transaction.execute(
            "DELETE FROM cut_admissions WHERE operation_id IN (\
                 SELECT id FROM operations WHERE state = 'planned'\
             )",
            [],
        )?;
        transaction.execute(
            "DELETE FROM worktrees WHERE operation_id IN (\
                 SELECT id FROM operations WHERE kind = 'worktree' AND state = 'planned'\
             ) AND state = 'creating'",
            [],
        )?;
        transaction.execute(
            "DELETE FROM operations WHERE kind IN ('cut', 'worktree') AND state = 'planned'",
            [],
        )?;
        transaction.execute(
            "DELETE FROM operations WHERE kind = 'initialize' AND state = 'planned'",
            [],
        )?;
        transaction.execute(
            "DELETE FROM watch_grants WHERE watch_id IN (\
                 SELECT id FROM watches w WHERE state = 'initializing' \
                   AND NOT EXISTS (SELECT 1 FROM operations o WHERE o.watch_id = w.id)\
             )",
            [],
        )?;
        transaction.execute(
            "DELETE FROM watches WHERE state = 'initializing' \
             AND NOT EXISTS (SELECT 1 FROM operations o WHERE o.watch_id = watches.id)",
            [],
        )?;

        transaction.execute(
            r#"UPDATE snapshots
                  SET physical_state = 'present'
                WHERE id IN (
                    SELECT snapshot_id FROM snapshot_delete_operations
                     WHERE state = 'planned'
                ) AND physical_state = 'deleting'"#,
            [],
        )?;
        transaction.execute(
            "DELETE FROM snapshot_delete_operations WHERE state = 'planned'",
            [],
        )?;
        transaction.execute(
            "UPDATE topology_leases SET lease_owner = NULL, lease_expires_ns = NULL \
             WHERE lease_owner IS NOT NULL AND lease_expires_ns <= ?1",
            [now_ns],
        )?;
        transaction.commit()?;
        usize::try_from(operation_count + delete_count)
            .map_err(|_| ManagerError::new("abandoned operation count overflow"))
    }

    /// Rotates all post-effect recovery leases to a new manager process after
    /// the broker's old-session drain barrier. The effect/receipt fence stays
    /// immutable so an already-started broker mutation can be reconciled, but
    /// every manager publication CAS now requires the new owner.
    pub fn takeover_recovery_leases(
        &mut self,
        new_owner: [u8; 16],
        lease_expires_ns: i64,
    ) -> Result<usize, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            r#"UPDATE watches
                  SET cut_owner = ?1, cut_expires_ns = ?2
                WHERE EXISTS (
                    SELECT 1 FROM operations o
                     WHERE o.watch_id = watches.id AND o.kind = 'cut'
                       AND o.state NOT IN ('planned', 'done', 'failed')
                       AND o.lease_owner = watches.cut_owner
                )"#,
            params![new_owner.as_slice(), lease_expires_ns],
        )?;
        transaction.execute(
            r#"UPDATE topology_leases
                  SET lease_owner = ?1, lease_expires_ns = ?2
                WHERE EXISTS (
                    SELECT 1 FROM operations o
                     WHERE o.filesystem_id = topology_leases.filesystem_id
                       AND o.kind IN ('initialize', 'worktree')
                       AND o.state NOT IN ('planned', 'done', 'failed')
                       AND o.lease_owner = topology_leases.lease_owner
                )"#,
            params![new_owner.as_slice(), lease_expires_ns],
        )?;
        let operations = transaction.execute(
            r#"UPDATE operations
                  SET lease_owner = ?1, lease_expires_ns = ?2
                WHERE state NOT IN ('planned', 'done', 'failed')"#,
            params![new_owner.as_slice(), lease_expires_ns],
        )?;
        let deletes = transaction.execute(
            r#"UPDATE snapshot_delete_operations
                  SET lease_owner = ?1, lease_expires_ns = ?2
                WHERE state NOT IN ('planned', 'done', 'failed')"#,
            params![new_owner.as_slice(), lease_expires_ns],
        )?;
        transaction.commit()?;
        operations
            .checked_add(deletes)
            .ok_or_else(|| ManagerError::new("recovery lease count overflow"))
    }

    pub fn recover_process_state(
        &mut self,
        current_boot_id: [u8; 16],
    ) -> Result<RecoveryReport, ManagerError> {
        let previous_boot_id = self.metadata()?.last_boot_id;
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active_queries: i64 = transaction.query_row(
            "SELECT count(*) FROM query_leases WHERE state = 'active'",
            [],
            |row| row.get(0),
        )?;
        transaction.execute(
            "DELETE FROM query_revision_pins WHERE query_id IN (\
                 SELECT id FROM query_leases WHERE state = 'active'\
             )",
            [],
        )?;
        transaction.execute(
            "DELETE FROM query_comparison_pins WHERE query_id IN (\
                 SELECT id FROM query_leases WHERE state = 'active'\
             )",
            [],
        )?;
        transaction.execute(
            "UPDATE query_leases SET state = 'released', lease_fence = lease_fence + 1 \
             WHERE state = 'active'",
            [],
        )?;
        let reclaimed_trigger_runs = transaction.execute(
            "UPDATE watchman_triggers \
                SET run_owner = NULL, run_expires_ns = NULL, run_fence = run_fence + 1 \
              WHERE run_owner IS NOT NULL",
            [],
        )?;
        // These read-only jobs are owned by one manager process.  A restart
        // invalidates their publication fence, but their endpoint pins must
        // also be released so that they cannot leak retained snapshots.
        let historical_comparison_ids = {
            let mut statement = transaction.prepare(
                "SELECT id FROM comparisons \
                  WHERE algorithm_version = 2 AND state = 'claimed'",
            )?;
            let ids = statement
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids
        };
        for comparison_id in &historical_comparison_ids {
            let owner_id = encode_u64(
                u64::try_from(*comparison_id)
                    .map_err(|_| ManagerError::new("comparison ID is negative"))?,
            );
            transaction.execute(
                "DELETE FROM snapshot_pins \
                  WHERE owner_kind = 'comparison' AND owner_id = ?1",
                [owner_id.as_slice()],
            )?;
        }
        let abandoned_historical_comparisons = transaction.execute(
            r#"UPDATE comparisons
                  SET state = 'failed', lease_owner = NULL,
                      lease_expires_ns = NULL, lease_fence = lease_fence + 1
                WHERE algorithm_version = 2 AND state = 'claimed'"#,
            [],
        )?;
        let mut statement = transaction
            .prepare("SELECT id FROM watches WHERE fsmonitor_state != 'disabled' ORDER BY id")?;
        let watch_ids = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for watch_id in &watch_ids {
            let replacement_epoch = random_id();
            transaction.execute(
                r#"UPDATE watches
                      SET clock_epoch = ?2,
                          fsmonitor_owner_grant_id = NULL,
                          fsmonitor_root = NULL,
                          mount_ns_dev = NULL, mount_ns_ino = NULL,
                          view_root_dev = NULL, view_root_ino = NULL,
                          view_root_mnt_id = NULL,
                          view_monitor_session_id = NULL,
                          guard_epoch = NULL, guard_head_seq = NULL,
                          guard_replay_floor_seq = NULL,
                          fsmonitor_state = 'disabled'
                    WHERE id = ?1 AND fsmonitor_state != 'disabled'"#,
                params![watch_id, replacement_epoch.as_slice()],
            )?;
        }
        transaction.execute(
            "UPDATE service_metadata SET last_boot_id = ?1 WHERE singleton = 1",
            [current_boot_id.as_slice()],
        )?;
        transaction.commit()?;
        Ok(RecoveryReport {
            invalidated_facades: watch_ids.len(),
            released_queries: usize::try_from(active_queries)
                .map_err(|_| ManagerError::new("active query count overflow"))?,
            reclaimed_trigger_runs,
            abandoned_historical_comparisons,
            boot_changed: previous_boot_id != current_boot_id,
        })
    }

    pub fn register_fixed_jj_trigger(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
    ) -> Result<bool, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let indexed_sequence: Option<i64> = transaction
            .query_row(
                r#"SELECT w.indexed_seq
                     FROM watches w JOIN watch_grants g ON g.watch_id = w.id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND g.id = ?2 AND g.state = 'active'
                      AND (g.permissions & ?3) = ?3"#,
                params![
                    watch_id.as_slice(),
                    authorization_id.as_slice(),
                    i64::from(PERMISSION_READ | PERMISSION_CUT | PERMISSION_TRIGGER),
                ],
                |row| row.get(0),
            )
            .optional()?;
        let indexed_sequence = indexed_sequence
            .ok_or_else(|| ManagerError::new("trigger authorization is not active"))?;
        let existed: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM watchman_triggers \
                 WHERE watch_id = ?1 AND owner_grant_id = ?2 \
                   AND name = 'jj-background-monitor'",
                params![watch_id.as_slice(), authorization_id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        transaction.execute(
            r#"INSERT INTO watchman_triggers(
                   watch_id, name, owner_grant_id, command_kind,
                   expression_kind, state, last_evaluated_seq,
                   pending_through_seq, run_owner, run_fence, run_expires_ns
               ) VALUES (?1, 'jj-background-monitor', ?2, 'jj-snapshot-v1',
                         'exclude-git-jj-v1', 'active', NULL, ?3, NULL, 0, NULL)
               ON CONFLICT(watch_id, owner_grant_id, name) DO UPDATE SET
                   command_kind = excluded.command_kind,
                   expression_kind = excluded.expression_kind,
                   state = 'active',
                   pending_through_seq = MAX(
                       COALESCE(watchman_triggers.pending_through_seq, ?3), ?3
                   )"#,
            params![
                watch_id.as_slice(),
                authorization_id.as_slice(),
                indexed_sequence,
            ],
        )?;
        transaction.commit()?;
        Ok(existed == Some(1))
    }

    pub fn has_fixed_jj_trigger(
        &self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
    ) -> Result<bool, ManagerError> {
        let present: Option<i64> = self
            .connection()
            .query_row(
                r#"SELECT 1 FROM watchman_triggers t
                    JOIN watch_grants g
                      ON g.id = t.owner_grant_id AND g.watch_id = t.watch_id
                   WHERE t.watch_id = ?1 AND t.owner_grant_id = ?2
                     AND t.name = 'jj-background-monitor'
                     AND t.state = 'active' AND g.state = 'active'"#,
                params![watch_id.as_slice(), authorization_id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(present == Some(1))
    }

    pub fn active_fixed_jj_trigger_watches(
        &self,
        requester_uid: u32,
    ) -> Result<Vec<[u8; 16]>, ManagerError> {
        let mut statement = self.connection().prepare(
            r#"SELECT DISTINCT t.watch_id
                 FROM watchman_triggers t
                 JOIN watches w ON w.id = t.watch_id
                 JOIN watch_grants g
                   ON g.id = t.owner_grant_id AND g.watch_id = t.watch_id
                WHERE t.name = 'jj-background-monitor' AND t.state = 'active'
                  AND w.state = 'active'
                  AND w.fsmonitor_owner_grant_id = t.owner_grant_id
                  AND w.fsmonitor_state IN ('snapshot_only', 'guard_arming',
                                            'guard_active', 'guard_gapped')
                  AND g.state = 'active' AND g.principal_kind = 'uid'
                  AND g.principal_id = ?1
                ORDER BY t.watch_id"#,
        )?;
        let principal = encode_u64(u64::from(requester_uid));
        let rows = statement
            .query_map([principal.as_slice()], |row| row.get::<_, Vec<u8>>(0))?
            .map(|row| {
                row?.try_into()
                    .map_err(|_| ManagerError::new("trigger watch ID has invalid length"))
            })
            .collect();
        rows
    }

    pub fn delete_fixed_jj_trigger(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
    ) -> Result<bool, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorized: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM watch_grants WHERE id = ?1 AND watch_id = ?2 AND state = 'active'",
                params![authorization_id.as_slice(), watch_id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        if authorized != Some(1) {
            return Err(ManagerError::new("trigger authorization is not active"));
        }
        let deleted = transaction.execute(
            "DELETE FROM watchman_triggers \
             WHERE watch_id = ?1 AND owner_grant_id = ?2 \
               AND name = 'jj-background-monitor'",
            params![watch_id.as_slice(), authorization_id.as_slice()],
        )?;
        transaction.commit()?;
        Ok(deleted == 1)
    }

    pub fn claim_fixed_jj_trigger(
        &mut self,
        run_owner: [u8; 16],
        now_ns: i64,
        run_expires_ns: i64,
    ) -> Result<Option<TriggerRun>, ManagerError> {
        if run_expires_ns <= now_ns {
            return Err(ManagerError::new(
                "trigger run expiry must follow its claim time",
            ));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate: Option<(Vec<u8>, Vec<u8>, i64, i64)> = transaction
            .query_row(
                r#"SELECT t.watch_id, t.owner_grant_id, t.pending_through_seq,
                          t.run_fence
                     FROM watchman_triggers t
                     JOIN watches w ON w.id = t.watch_id
                     JOIN watch_grants g
                       ON g.id = t.owner_grant_id AND g.watch_id = t.watch_id
                    WHERE t.state = 'active' AND w.state = 'active'
                      AND g.state = 'active'
                      AND t.pending_through_seq IS NOT NULL
                      AND (t.last_evaluated_seq IS NULL
                           OR t.pending_through_seq > t.last_evaluated_seq)
                      AND (t.run_owner IS NULL OR t.run_expires_ns <= ?1)
                    ORDER BY t.run_fence, COALESCE(t.run_expires_ns, 0), t.watch_id
                    LIMIT 1"#,
                [now_ns],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((watch_id, authorization_id, through_sequence, old_fence)) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };
        let watch_id: [u8; 16] = watch_id
            .try_into()
            .map_err(|_| ManagerError::new("trigger watch ID has invalid length"))?;
        let authorization_id: [u8; 16] = authorization_id
            .try_into()
            .map_err(|_| ManagerError::new("trigger grant ID has invalid length"))?;
        let run_fence = old_fence
            .checked_add(1)
            .ok_or_else(|| ManagerError::new("trigger run fence overflow"))?;
        require_one(
            transaction.execute(
                r#"UPDATE watchman_triggers
                      SET run_owner = ?3, run_fence = ?4, run_expires_ns = ?5
                    WHERE watch_id = ?1 AND owner_grant_id = ?2
                      AND name = 'jj-background-monitor' AND state = 'active'
                      AND run_fence = ?6
                      AND (run_owner IS NULL OR run_expires_ns <= ?7)"#,
                params![
                    watch_id.as_slice(),
                    authorization_id.as_slice(),
                    run_owner.as_slice(),
                    run_fence,
                    run_expires_ns,
                    old_fence,
                    now_ns,
                ],
            )?,
            "claim jj trigger run",
        )?;
        transaction.commit()?;
        Ok(Some(TriggerRun {
            watch_id,
            authorization_id,
            through_sequence,
            run_owner,
            run_fence,
        }))
    }

    pub fn finish_fixed_jj_trigger(
        &mut self,
        run: &TriggerRun,
        succeeded: bool,
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = if succeeded {
            transaction.execute(
                r#"UPDATE watchman_triggers
                      SET last_evaluated_seq = MAX(
                              COALESCE(last_evaluated_seq, ?6), ?6
                          ),
                          run_owner = NULL, run_expires_ns = NULL
                    WHERE watch_id = ?1 AND owner_grant_id = ?2
                      AND name = 'jj-background-monitor' AND state = 'active'
                      AND run_owner = ?3 AND run_fence = ?4
                      AND pending_through_seq >= ?5"#,
                params![
                    run.watch_id.as_slice(),
                    run.authorization_id.as_slice(),
                    run.run_owner.as_slice(),
                    run.run_fence,
                    run.through_sequence,
                    run.through_sequence,
                ],
            )?
        } else {
            transaction.execute(
                r#"UPDATE watchman_triggers
                      SET run_owner = NULL, run_expires_ns = NULL
                    WHERE watch_id = ?1 AND owner_grant_id = ?2
                      AND name = 'jj-background-monitor' AND state = 'active'
                      AND run_owner = ?3 AND run_fence = ?4"#,
                params![
                    run.watch_id.as_slice(),
                    run.authorization_id.as_slice(),
                    run.run_owner.as_slice(),
                    run.run_fence,
                ],
            )?
        };
        require_one(updated, "finish jj trigger run")?;
        transaction.commit()?;
        Ok(())
    }

    pub fn fixed_jj_trigger_root(
        &self,
        run: &TriggerRun,
        requester_uid: u32,
    ) -> Result<Vec<u8>, ManagerError> {
        let root: Option<Vec<u8>> = self
            .connection()
            .query_row(
                r#"SELECT w.fsmonitor_root
                     FROM watchman_triggers t
                     JOIN watches w ON w.id = t.watch_id
                     JOIN watch_grants g
                       ON g.id = t.owner_grant_id AND g.watch_id = t.watch_id
                    WHERE t.watch_id = ?1 AND t.owner_grant_id = ?2
                      AND t.name = 'jj-background-monitor' AND t.state = 'active'
                      AND t.run_owner = ?3 AND t.run_fence = ?4
                      AND w.state = 'active'
                      AND w.fsmonitor_owner_grant_id = t.owner_grant_id
                      AND w.fsmonitor_state IN ('snapshot_only', 'guard_arming',
                                                'guard_active', 'guard_gapped')
                      AND g.state = 'active' AND g.principal_kind = 'uid'
                      AND g.principal_id = ?5"#,
                params![
                    run.watch_id.as_slice(),
                    run.authorization_id.as_slice(),
                    run.run_owner.as_slice(),
                    run.run_fence,
                    encode_u64(u64::from(requester_uid)).as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        root.ok_or_else(|| ManagerError::new("trigger run view or authorization is stale"))
    }

    pub fn invalidate_snapshot_facade(
        &mut self,
        activation: &FacadeActivation,
    ) -> Result<(), ManagerError> {
        let replacement_epoch = random_id();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active_responses: i64 = transaction.query_row(
            "SELECT count(*) FROM query_leases \
             WHERE watch_id = ?1 AND authorization_id = ?2 AND clock_epoch = ?3 \
               AND state = 'active'",
            params![
                activation.watch_id.as_slice(),
                activation.authorization_id.as_slice(),
                activation.clock_epoch.as_slice(),
            ],
            |row| row.get(0),
        )?;
        if active_responses != 0 {
            return Err(ManagerError::new(
                "snapshot facade has a response lease which must drain before invalidation",
            ));
        }
        transaction.execute(
            r#"DELETE FROM query_revision_pins
                WHERE query_id IN (
                    SELECT id FROM query_leases
                     WHERE watch_id = ?1 AND clock_epoch = ?2 AND state = 'active'
                )"#,
            params![
                activation.watch_id.as_slice(),
                activation.clock_epoch.as_slice(),
            ],
        )?;
        transaction.execute(
            r#"DELETE FROM query_comparison_pins
                WHERE query_id IN (
                    SELECT id FROM query_leases
                     WHERE watch_id = ?1 AND clock_epoch = ?2 AND state = 'active'
                )"#,
            params![
                activation.watch_id.as_slice(),
                activation.clock_epoch.as_slice(),
            ],
        )?;
        transaction.execute(
            "UPDATE query_leases SET state = 'released', lease_fence = lease_fence + 1 \
             WHERE watch_id = ?1 AND clock_epoch = ?2 AND state = 'active'",
            params![
                activation.watch_id.as_slice(),
                activation.clock_epoch.as_slice(),
            ],
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE watches
                      SET clock_epoch = ?4,
                          fsmonitor_owner_grant_id = NULL,
                          fsmonitor_root = NULL,
                          mount_ns_dev = NULL, mount_ns_ino = NULL,
                          view_root_dev = NULL, view_root_ino = NULL,
                          view_root_mnt_id = NULL,
                          view_monitor_session_id = NULL,
                          guard_epoch = NULL, guard_head_seq = NULL,
                          guard_replay_floor_seq = NULL,
                          fsmonitor_state = 'disabled'
                    WHERE id = ?1 AND state = 'active'
                      AND fsmonitor_owner_grant_id = ?2
                      AND clock_epoch = ?3
                      AND view_monitor_session_id = ?5"#,
                params![
                    activation.watch_id.as_slice(),
                    activation.authorization_id.as_slice(),
                    activation.clock_epoch.as_slice(),
                    replacement_epoch.as_slice(),
                    activation.monitor_session_id.as_slice(),
                ],
            )?,
            "invalidate snapshot facade",
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn revoke_grant(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        let replacement_epoch = random_id();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE cut_admissions SET state = 'abandoned' \
             WHERE authorization_id = ?1 AND watch_id = ?2 AND state = 'waiting'",
            params![authorization_id.as_slice(), watch_id.as_slice()],
        )?;
        // Operations which have not crossed the durable pre-effect boundary
        // are cancelled under the same writer lock. A racing worker either
        // commits planned->fs_started first (and revocation must wait for
        // reconciliation) or its conditional transition loses to these
        // deletes and it can never issue the ioctl.
        transaction.execute(
            r#"UPDATE watches SET cut_owner = NULL, cut_expires_ns = NULL
                WHERE id = ?1 AND EXISTS (
                    SELECT 1 FROM operations o
                     WHERE o.watch_id = watches.id AND o.authorization_id = ?2
                       AND o.kind = 'cut' AND o.state = 'planned'
                       AND o.lease_owner = watches.cut_owner)"#,
            params![watch_id.as_slice(), authorization_id.as_slice()],
        )?;
        transaction.execute(
            r#"DELETE FROM snapshot_pins
                WHERE owner_kind = 'operation' AND owner_id IN (
                    SELECT id FROM operations
                     WHERE watch_id = ?1 AND authorization_id = ?2
                       AND state = 'planned')"#,
            params![watch_id.as_slice(), authorization_id.as_slice()],
        )?;
        transaction.execute(
            r#"DELETE FROM cut_admissions WHERE operation_id IN (
                    SELECT id FROM operations
                     WHERE watch_id = ?1 AND authorization_id = ?2
                       AND state = 'planned')"#,
            params![watch_id.as_slice(), authorization_id.as_slice()],
        )?;
        transaction.execute(
            r#"DELETE FROM worktrees WHERE operation_id IN (
                    SELECT id FROM operations
                     WHERE watch_id = ?1 AND authorization_id = ?2
                       AND kind = 'worktree' AND state = 'planned')
                  AND state = 'creating'"#,
            params![watch_id.as_slice(), authorization_id.as_slice()],
        )?;
        transaction.execute(
            "DELETE FROM operations WHERE watch_id = ?1 AND authorization_id = ?2 \
             AND kind IN ('cut', 'worktree') AND state = 'planned'",
            params![watch_id.as_slice(), authorization_id.as_slice()],
        )?;
        let active_effects: i64 = transaction.query_row(
            "SELECT count(*) FROM operations WHERE watch_id = ?1 AND authorization_id = ?2 \
             AND state NOT IN ('done', 'failed')",
            params![watch_id.as_slice(), authorization_id.as_slice()],
            |row| row.get(0),
        )?;
        if active_effects != 0 {
            return Err(ManagerError::new(
                "grant has an operation which must be reconciled before revocation",
            ));
        }
        let active_queries: i64 = transaction.query_row(
            "SELECT count(*) FROM query_leases \
             WHERE watch_id = ?1 AND authorization_id = ?2 AND state = 'active'",
            params![watch_id.as_slice(), authorization_id.as_slice()],
            |row| row.get(0),
        )?;
        if active_queries != 0 {
            return Err(ManagerError::new(
                "grant has a response lease which must drain before revocation",
            ));
        }
        transaction.execute(
            r#"DELETE FROM snapshot_pins WHERE owner_kind = 'retention-lease' AND EXISTS (
                   SELECT 1 FROM retention_leases r
                    WHERE r.id = snapshot_pins.owner_id
                      AND r.snapshot_id = snapshot_pins.snapshot_id
                      AND r.watch_id = ?1 AND r.authorization_id = ?2
                      AND r.state = 'active')"#,
            params![watch_id.as_slice(), authorization_id.as_slice()],
        )?;
        transaction.execute(
            "UPDATE retention_leases SET state = 'revoked', lease_fence = lease_fence + 1 \
             WHERE watch_id = ?1 AND authorization_id = ?2 AND state = 'active'",
            params![watch_id.as_slice(), authorization_id.as_slice()],
        )?;
        transaction.execute(
            "DELETE FROM watchman_triggers WHERE watch_id = ?1 AND owner_grant_id = ?2",
            params![watch_id.as_slice(), authorization_id.as_slice()],
        )?;
        require_one(
            transaction.execute(
                "UPDATE watch_grants SET state = 'revoked', revoked_ns = ?3 \
                 WHERE id = ?1 AND watch_id = ?2 AND state = 'active'",
                params![authorization_id.as_slice(), watch_id.as_slice(), now_ns],
            )?,
            "revoke watch grant",
        )?;
        transaction.execute(
            r#"UPDATE watches
                  SET clock_epoch = ?3,
                      fsmonitor_owner_grant_id = NULL, fsmonitor_root = NULL,
                      mount_ns_dev = NULL, mount_ns_ino = NULL,
                      view_root_dev = NULL, view_root_ino = NULL,
                      view_root_mnt_id = NULL, view_monitor_session_id = NULL,
                      guard_epoch = NULL, guard_head_seq = NULL,
                      guard_replay_floor_seq = NULL, fsmonitor_state = 'disabled'
                WHERE id = ?1 AND fsmonitor_owner_grant_id = ?2"#,
            params![
                watch_id.as_slice(),
                authorization_id.as_slice(),
                replacement_epoch.as_slice(),
            ],
        )?;
        transaction.execute(
            r#"UPDATE watches SET state = 'blocked'
                WHERE id = ?1 AND state = 'active'
                  AND NOT EXISTS (
                      SELECT 1 FROM watch_grants g
                       WHERE g.watch_id = watches.id AND g.state = 'active')"#,
            [watch_id.as_slice()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn activate_snapshot_facade(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        binding: &ViewBinding,
    ) -> Result<FacadeActivation, ManagerError> {
        let clock_epoch = random_id();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: Option<i64> = transaction
            .query_row(
                r#"SELECT 1
                     FROM watches w
                     JOIN watch_grants g ON g.watch_id = w.id
                     JOIN filesystems f ON f.id = w.filesystem_id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND g.id = ?2 AND g.state = 'active'
                      AND (g.permissions & ?3) = ?3
                      AND w.live_path = ?4 AND w.live_subvol_uuid = ?5
                      AND f.fs_uuid = ?6"#,
                params![
                    watch_id.as_slice(),
                    authorization_id.as_slice(),
                    i64::from(PERMISSION_READ | PERMISSION_CUT),
                    binding.root_path,
                    binding.subvol_uuid.as_slice(),
                    binding.fs_uuid.as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if valid != Some(1) {
            return Err(ManagerError::new(
                "facade binding does not match an authorized active watch",
            ));
        }
        require_one(
            transaction.execute(
                r#"UPDATE watches
                      SET clock_epoch = ?3,
                          fsmonitor_owner_grant_id = ?2,
                          fsmonitor_root = ?4,
                          mount_ns_dev = ?5, mount_ns_ino = ?6,
                          view_root_dev = ?7, view_root_ino = ?8,
                          view_root_mnt_id = ?9,
                          view_monitor_session_id = ?10,
                          guard_epoch = NULL, guard_head_seq = NULL,
                          guard_replay_floor_seq = NULL,
                          fsmonitor_state = 'snapshot_only'
                    WHERE id = ?1 AND state = 'active'
                      AND (fsmonitor_state = 'disabled'
                           OR fsmonitor_owner_grant_id = ?2)"#,
                params![
                    watch_id.as_slice(),
                    authorization_id.as_slice(),
                    clock_epoch.as_slice(),
                    binding.root_path,
                    encode_u64(binding.mount_ns_dev).as_slice(),
                    encode_u64(binding.mount_ns_ino).as_slice(),
                    encode_u64(binding.process_root_dev).as_slice(),
                    encode_u64(binding.process_root_ino).as_slice(),
                    encode_u64(binding.process_root_mnt_id).as_slice(),
                    binding.monitor_session_id.as_slice(),
                ],
            )?,
            "activate snapshot facade",
        )?;
        transaction.commit()?;
        Ok(FacadeActivation {
            watch_id,
            authorization_id,
            clock_epoch,
            monitor_session_id: binding.monitor_session_id,
        })
    }

    /// Starts a fresh optional precision epoch. The producer must not publish
    /// a complete cursor until its recursive watches and a same-fd marker have
    /// both been drained.
    pub fn begin_precision_guard(
        &mut self,
        activation: &FacadeActivation,
    ) -> Result<GuardCursor, ManagerError> {
        let epoch = random_id();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_one(
            transaction.execute(
                r#"UPDATE watches
                      SET guard_epoch = ?3, guard_head_seq = 0,
                          guard_replay_floor_seq = 0,
                          fsmonitor_state = 'guard_arming'
                    WHERE id = ?1 AND state = 'active'
                      AND fsmonitor_owner_grant_id = ?2
                      AND clock_epoch = ?4 AND view_monitor_session_id = ?5
                      AND fsmonitor_state IN ('snapshot_only', 'guard_gapped')"#,
                params![
                    activation.watch_id.as_slice(),
                    activation.authorization_id.as_slice(),
                    epoch.as_slice(),
                    activation.clock_epoch.as_slice(),
                    activation.monitor_session_id.as_slice(),
                ],
            )?,
            "begin precision guard",
        )?;
        transaction.commit()?;
        Ok(GuardCursor { epoch, sequence: 0 })
    }

    /// Appends a durable prefix of conservative mutation hints. Event rows and
    /// the visible head advance in one writer transaction.
    pub fn append_precision_events(
        &mut self,
        activation: &FacadeActivation,
        epoch: [u8; 16],
        events: &[MutationHint],
        observed_ns: i64,
    ) -> Result<GuardCursor, ManagerError> {
        if observed_ns < 0 {
            return Err(ManagerError::new("invalid mutation observation time"));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head: Option<i64> = transaction
            .query_row(
                r#"SELECT guard_head_seq FROM watches
                    WHERE id = ?1 AND state = 'active'
                      AND fsmonitor_owner_grant_id = ?2
                      AND clock_epoch = ?3 AND view_monitor_session_id = ?4
                      AND guard_epoch = ?5
                      AND fsmonitor_state IN ('guard_arming', 'guard_active')"#,
                params![
                    activation.watch_id.as_slice(),
                    activation.authorization_id.as_slice(),
                    activation.clock_epoch.as_slice(),
                    activation.monitor_session_id.as_slice(),
                    epoch.as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        let mut sequence =
            head.ok_or_else(|| ManagerError::new("precision guard is stale or gapped"))?;
        for event in events {
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| ManagerError::new("precision sequence overflow"))?;
            let (kind, path, ino, generation): EncodedMutationHint<'_> = match event {
                MutationHint::Path(path) => ("path", Some(path), None, None),
                MutationHint::DirectoryPrefix(path) => ("directory-prefix", Some(path), None, None),
                MutationHint::Object {
                    path,
                    ino,
                    generation,
                } => (
                    "object",
                    Some(path),
                    Some(encode_u64(*ino)),
                    Some(encode_u64(*generation)),
                ),
                MutationHint::FullInvalidation => ("full-invalidation", None, None, None),
            };
            transaction.execute(
                r#"INSERT INTO mutation_events(
                       watch_id, guard_epoch, sequence, event_kind, path,
                       ino, generation, observed_ns
                   ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
                params![
                    activation.watch_id.as_slice(),
                    epoch.as_slice(),
                    sequence,
                    kind,
                    path,
                    ino.as_ref().map(|bytes| bytes.as_slice()),
                    generation.as_ref().map(|bytes| bytes.as_slice()),
                    observed_ns,
                ],
            )?;
        }
        require_one(
            transaction.execute(
                r#"UPDATE watches SET guard_head_seq = ?3
                    WHERE id = ?1 AND guard_epoch = ?2
                      AND guard_head_seq = ?4
                      AND fsmonitor_state IN ('guard_arming', 'guard_active')"#,
                params![
                    activation.watch_id.as_slice(),
                    epoch.as_slice(),
                    sequence,
                    head
                ],
            )?,
            "advance precision head",
        )?;
        transaction.commit()?;
        Ok(GuardCursor { epoch, sequence })
    }

    pub fn complete_precision_guard(
        &mut self,
        activation: &FacadeActivation,
        cursor: GuardCursor,
    ) -> Result<(), ManagerError> {
        require_one(
            self.connection_mut().execute(
                r#"UPDATE watches SET fsmonitor_state = 'guard_active'
                    WHERE id = ?1 AND state = 'active'
                      AND fsmonitor_owner_grant_id = ?2
                      AND clock_epoch = ?3 AND view_monitor_session_id = ?4
                      AND fsmonitor_state = 'guard_arming'
                      AND guard_epoch = ?5 AND guard_head_seq = ?6"#,
                params![
                    activation.watch_id.as_slice(),
                    activation.authorization_id.as_slice(),
                    activation.clock_epoch.as_slice(),
                    activation.monitor_session_id.as_slice(),
                    cursor.epoch.as_slice(),
                    cursor.sequence,
                ],
            )?,
            "complete precision guard",
        )
    }

    /// A producer-side ambiguity is terminal for this epoch but leaves the
    /// mandatory snapshot facade active and correct.
    pub fn gap_precision_guard(
        &mut self,
        activation: &FacadeActivation,
        epoch: [u8; 16],
    ) -> Result<(), ManagerError> {
        require_one(
            self.connection_mut().execute(
                r#"UPDATE watches SET fsmonitor_state = 'guard_gapped'
                    WHERE id = ?1 AND state = 'active'
                      AND fsmonitor_owner_grant_id = ?2
                      AND clock_epoch = ?3 AND view_monitor_session_id = ?4
                      AND guard_epoch = ?5
                      AND fsmonitor_state IN ('guard_arming', 'guard_active')"#,
                params![
                    activation.watch_id.as_slice(),
                    activation.authorization_id.as_slice(),
                    activation.clock_epoch.as_slice(),
                    activation.monitor_session_id.as_slice(),
                    epoch.as_slice(),
                ],
            )?,
            "gap precision guard",
        )
    }

    pub fn finalize_cut_boundary(
        &mut self,
        activation: &FacadeActivation,
        binding: &ViewBinding,
        sequence: i64,
        guard: Option<GuardCursor>,
    ) -> Result<(), ManagerError> {
        if sequence <= 0
            || activation.monitor_session_id != binding.monitor_session_id
            || activation.watch_id == [0; 16]
        {
            return Err(ManagerError::new("invalid facade cut boundary"));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<GuardBoundaryRow> = transaction
            .query_row(
                r#"SELECT c.target_snapshot_id, c.operation_id,
                          w.fsmonitor_state, w.guard_epoch, w.guard_head_seq
                     FROM watches w
                     JOIN watch_grants g ON g.watch_id = w.id
                     JOIN watch_cuts c ON c.watch_id = w.id AND c.sequence = ?3
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND w.fsmonitor_state IN ('snapshot_only', 'guard_arming',
                                                'guard_active', 'guard_gapped')
                      AND w.fsmonitor_owner_grant_id = ?2
                      AND w.clock_epoch = ?4
                      AND w.view_monitor_session_id = ?5
                      AND w.fsmonitor_root = ?6
                      AND w.mount_ns_dev = ?7 AND w.mount_ns_ino = ?8
                      AND w.view_root_dev = ?9 AND w.view_root_ino = ?10
                      AND w.view_root_mnt_id = ?11
                      AND g.id = ?2 AND g.state = 'active'
                      AND c.state = 'ready'"#,
                params![
                    activation.watch_id.as_slice(),
                    activation.authorization_id.as_slice(),
                    sequence,
                    activation.clock_epoch.as_slice(),
                    activation.monitor_session_id.as_slice(),
                    binding.root_path,
                    encode_u64(binding.mount_ns_dev).as_slice(),
                    encode_u64(binding.mount_ns_ino).as_slice(),
                    encode_u64(binding.process_root_dev).as_slice(),
                    encode_u64(binding.process_root_ino).as_slice(),
                    encode_u64(binding.process_root_mnt_id).as_slice(),
                ],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let (snapshot_id, operation_id, guard_state, current_epoch, current_head) =
            row.ok_or_else(|| ManagerError::new("cut boundary binding or authorization is stale"))?;
        let certified = match guard {
            Some(cursor)
                if guard_state == "guard_active"
                    && current_epoch.as_deref() == Some(cursor.epoch.as_slice())
                    && current_head == Some(cursor.sequence) =>
            {
                Some(cursor)
            }
            _ => None,
        };
        let guard_epoch = certified.as_ref().map(|cursor| cursor.epoch.as_slice());
        let guard_sequence = certified.as_ref().map(|cursor| cursor.sequence);
        let guard_complete = i64::from(certified.is_some());
        let existing: Option<(i64, Vec<u8>, Vec<u8>)> = transaction
            .query_row(
                r#"SELECT target_snapshot_id, cut_operation_id, clock_epoch
                     FROM fsmonitor_boundaries
                    WHERE watch_id = ?1 AND cut_sequence = ?2
                      AND boundary_kind = 'cut'"#,
                params![activation.watch_id.as_slice(), sequence],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((existing_snapshot, existing_operation, existing_epoch)) = existing {
            if existing_snapshot != snapshot_id
                || existing_operation != operation_id
                || existing_epoch != activation.clock_epoch
            {
                return Err(ManagerError::new(
                    "existing cut boundary conflicts with the current publication",
                ));
            }
            transaction.commit()?;
            return Ok(());
        }
        require_one(
            transaction.execute(
                "UPDATE operations SET guard_epoch = ?2, guard_sequence = ?3 \
                 WHERE id = ?1 AND kind = 'cut' AND state = 'done'",
                params![operation_id, guard_epoch, guard_sequence],
            )?,
            "record cut precision cursor",
        )?;
        transaction.execute(
            r#"INSERT INTO fsmonitor_boundaries(
                   watch_id, cut_sequence, target_snapshot_id, boundary_kind,
                   cut_operation_id, seed_worktree_id, clock_epoch,
                   guard_epoch, guard_sequence, guard_complete
               ) VALUES (?1, ?2, ?3, 'cut', ?4, NULL, ?5, ?6, ?7, ?8)"#,
            params![
                activation.watch_id.as_slice(),
                sequence,
                snapshot_id,
                operation_id,
                activation.clock_epoch.as_slice(),
                guard_epoch,
                guard_sequence,
                guard_complete,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn finalize_proved_worktree_seed(
        &mut self,
        activation: &FacadeActivation,
        binding: &ViewBinding,
    ) -> Result<ProvedWorktreeSeed, ManagerError> {
        if activation.monitor_session_id != binding.monitor_session_id {
            return Err(ManagerError::new(
                "proved Worktree monitor session is stale",
            ));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(Vec<u8>, i64, Vec<u8>)> = transaction
            .query_row(
                r#"SELECT wt.id, r.snapshot_id, s.subvol_uuid
                     FROM watches w
                     JOIN watch_grants g ON g.watch_id = w.id
                     JOIN worktrees wt ON wt.watch_id = w.id
                     JOIN revisions r ON r.id = w.indexed_revision_id
                     JOIN snapshots s ON s.id = r.snapshot_id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND w.indexed_seq = 0 AND w.last_cut_seq = 0
                      AND w.last_cut_snapshot_id = r.snapshot_id
                      AND w.fsmonitor_owner_grant_id = ?2
                      AND w.clock_epoch = ?3
                      AND w.view_monitor_session_id = ?4
                      AND w.fsmonitor_root = ?5
                      AND w.mount_ns_dev = ?6 AND w.mount_ns_ino = ?7
                      AND w.view_root_dev = ?8 AND w.view_root_ino = ?9
                      AND w.view_root_mnt_id = ?10
                      AND g.id = ?2 AND g.state = 'active'
                      AND wt.state = 'present' AND wt.seed_revision_id = r.id"#,
                params![
                    activation.watch_id.as_slice(),
                    activation.authorization_id.as_slice(),
                    activation.clock_epoch.as_slice(),
                    activation.monitor_session_id.as_slice(),
                    binding.root_path,
                    encode_u64(binding.mount_ns_dev).as_slice(),
                    encode_u64(binding.mount_ns_ino).as_slice(),
                    encode_u64(binding.process_root_dev).as_slice(),
                    encode_u64(binding.process_root_ino).as_slice(),
                    encode_u64(binding.process_root_mnt_id).as_slice(),
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (worktree_id, snapshot_id, snapshot_uuid) = row.ok_or_else(|| {
            ManagerError::new("proved Worktree seed binding or authorization is stale")
        })?;
        transaction.execute(
            r#"INSERT INTO fsmonitor_boundaries(
                   watch_id, cut_sequence, target_snapshot_id, boundary_kind,
                   cut_operation_id, seed_worktree_id, clock_epoch,
                   guard_epoch, guard_sequence, guard_complete
               ) VALUES (?1, 0, ?2, 'proved_worktree_seed', NULL, ?3, ?4,
                         NULL, NULL, 0)"#,
            params![
                activation.watch_id.as_slice(),
                snapshot_id,
                worktree_id,
                activation.clock_epoch.as_slice(),
            ],
        )?;
        transaction.commit()?;
        Ok(ProvedWorktreeSeed {
            worktree_id: fixed_manager_blob(&worktree_id, "Worktree ID")?,
            snapshot_uuid: fixed_manager_blob(&snapshot_uuid, "seed snapshot UUID")?,
        })
    }

    pub fn begin_query_lease(
        &mut self,
        activation: &FacadeActivation,
        from_sequence: Option<i64>,
        to_sequence: i64,
        lease_owner: [u8; 16],
        lease_expires_ns: i64,
    ) -> Result<QueryLeaseReservation, ManagerError> {
        if to_sequence <= 0
            || from_sequence.is_some_and(|from| from < 0 || from > to_sequence)
            || lease_expires_ns <= 0
        {
            return Err(ManagerError::new("invalid query lease range"));
        }
        let id = random_id();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: Option<i64> = transaction
            .query_row(
                r#"SELECT 1
                     FROM watches w JOIN watch_grants g ON g.watch_id = w.id
                     JOIN fsmonitor_boundaries b
                       ON b.watch_id = w.id AND b.cut_sequence = ?3
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND g.id = ?2 AND g.state = 'active'
                      AND (g.permissions & ?6) = ?6
                      AND w.fsmonitor_owner_grant_id = ?2
                      AND w.clock_epoch = ?4
                      AND w.view_monitor_session_id = ?5
                      AND b.clock_epoch = ?4"#,
                params![
                    activation.watch_id.as_slice(),
                    activation.authorization_id.as_slice(),
                    to_sequence,
                    activation.clock_epoch.as_slice(),
                    activation.monitor_session_id.as_slice(),
                    i64::from(PERMISSION_READ | PERMISSION_CUT),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if valid != Some(1) {
            return Err(ManagerError::new(
                "query lease authorization or target boundary is stale",
            ));
        }
        if let Some(from) = from_sequence {
            let valid_from: Option<i64> = transaction
                .query_row(
                    "SELECT 1 FROM fsmonitor_boundaries \
                     WHERE watch_id = ?1 AND cut_sequence = ?2 AND clock_epoch = ?3",
                    params![
                        activation.watch_id.as_slice(),
                        from,
                        activation.clock_epoch.as_slice(),
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if valid_from != Some(1) {
                return Err(ManagerError::new("query source boundary is stale"));
            }
        }
        let guard: Option<(Vec<u8>, i64, i64)> = match from_sequence {
            Some(from) => transaction
                .query_row(
                    r#"SELECT a.guard_epoch, a.guard_sequence, b.guard_sequence
                         FROM fsmonitor_boundaries a
                         JOIN fsmonitor_boundaries b ON b.watch_id = a.watch_id
                         JOIN watches w ON w.id = a.watch_id
                        WHERE a.watch_id = ?1 AND a.cut_sequence = ?2
                          AND b.cut_sequence = ?3
                          AND a.guard_complete = 1 AND b.guard_complete = 1
                          AND a.guard_epoch = b.guard_epoch
                          AND w.guard_epoch = a.guard_epoch
                          AND w.guard_replay_floor_seq <= a.guard_sequence
                          AND a.guard_sequence <= b.guard_sequence"#,
                    params![activation.watch_id.as_slice(), from, to_sequence],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?,
            None => None,
        };
        let guard_epoch = guard.as_ref().map(|(epoch, _, _)| epoch.as_slice());
        let from_guard = guard.as_ref().map(|(_, from, _)| *from);
        let to_guard = guard.as_ref().map(|(_, _, to)| *to);
        transaction.execute(
            r#"INSERT INTO query_leases(
                   id, watch_id, authorization_id, clock_epoch,
                   from_cut_sequence, to_cut_sequence,
                   guard_epoch, from_guard_sequence, to_guard_sequence,
                   lease_owner, lease_fence, lease_expires_ns, state
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                         ?10, 1, ?11, 'active')"#,
            params![
                id.as_slice(),
                activation.watch_id.as_slice(),
                activation.authorization_id.as_slice(),
                activation.clock_epoch.as_slice(),
                from_sequence,
                to_sequence,
                guard_epoch,
                from_guard,
                to_guard,
                lease_owner.as_slice(),
                lease_expires_ns,
            ],
        )?;
        let lower = from_sequence.unwrap_or(to_sequence);
        transaction.execute(
            r#"INSERT INTO query_revision_pins(query_id, revision_id)
               SELECT DISTINCT ?1, r.id
                 FROM fsmonitor_boundaries b
                 JOIN revisions r ON r.snapshot_id = b.target_snapshot_id
                WHERE b.watch_id = ?2 AND b.cut_sequence >= ?3
                  AND b.cut_sequence <= ?4 AND b.clock_epoch = ?5"#,
            params![
                id.as_slice(),
                activation.watch_id.as_slice(),
                lower,
                to_sequence,
                activation.clock_epoch.as_slice(),
            ],
        )?;
        transaction.execute(
            r#"INSERT INTO query_comparison_pins(query_id, comparison_id)
               SELECT ?1, c.comparison_id
                 FROM watch_cuts c
                WHERE c.watch_id = ?2 AND c.sequence > ?3 AND c.sequence <= ?4
                  AND c.state = 'ready' AND c.comparison_id IS NOT NULL"#,
            params![
                id.as_slice(),
                activation.watch_id.as_slice(),
                from_sequence.unwrap_or(to_sequence),
                to_sequence,
            ],
        )?;
        transaction.commit()?;
        let guard = guard
            .map(|(epoch, from, to)| {
                fixed_manager_blob(&epoch, "guard epoch").map(|epoch| (epoch, from, to))
            })
            .transpose()?;
        Ok(QueryLeaseReservation {
            id,
            watch_id: activation.watch_id,
            authorization_id: activation.authorization_id,
            clock_epoch: activation.clock_epoch,
            from_sequence,
            to_sequence,
            guard,
            lease_owner,
            lease_fence: 1,
        })
    }

    pub fn release_query_lease(
        &mut self,
        lease: &QueryLeaseReservation,
        activation: &FacadeActivation,
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: Option<i64> = transaction
            .query_row(
                r#"SELECT 1 FROM query_leases q
                    JOIN watches w ON w.id = q.watch_id
                    JOIN watch_grants g ON g.id = q.authorization_id
                   WHERE q.id = ?1 AND q.watch_id = ?2
                     AND q.authorization_id = ?3 AND q.clock_epoch = ?4
                     AND q.lease_owner = ?5 AND q.lease_fence = ?6
                     AND q.state = 'active' AND w.state = 'active'
                     AND w.clock_epoch = ?4 AND w.view_monitor_session_id = ?7
                     AND g.state = 'active'"#,
                params![
                    lease.id.as_slice(),
                    lease.watch_id.as_slice(),
                    lease.authorization_id.as_slice(),
                    lease.clock_epoch.as_slice(),
                    lease.lease_owner.as_slice(),
                    lease.lease_fence,
                    activation.monitor_session_id.as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if valid != Some(1) {
            return Err(ManagerError::new("query lease response fence is stale"));
        }
        transaction.execute(
            "DELETE FROM query_revision_pins WHERE query_id = ?1",
            [lease.id.as_slice()],
        )?;
        transaction.execute(
            "DELETE FROM query_comparison_pins WHERE query_id = ?1",
            [lease.id.as_slice()],
        )?;
        require_one(
            transaction.execute(
                "UPDATE query_leases SET state = 'released' \
                 WHERE id = ?1 AND state = 'active' AND lease_fence = ?2",
                params![lease.id.as_slice(), lease.lease_fence],
            )?,
            "release query lease",
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn reserve_initialize(
        &mut self,
        request: &InitializeRequest,
    ) -> Result<InitializeReservation, ManagerError> {
        if !path_is_absolute(&request.source_path)
            || !path_is_absolute(&request.reserved_snapshot_path)
        {
            return Err(ManagerError::new("initialize paths must be absolute"));
        }
        if request.lease_expires_ns <= request.now_ns {
            return Err(ManagerError::new(
                "initialize lease must expire after admission",
            ));
        }
        if request.permissions.contains(PERMISSION_WORKTREE) {
            return Err(ManagerError::new(
                "a WORKTREE grant requires an immutable destination policy",
            ));
        }

        let watch_id = random_id();
        let grant_id = random_id();
        let operation_id = random_id();
        let clock_epoch = random_id();
        let (principal_kind, principal_id) = request.principal.kind_and_id();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        transaction.execute(
            "INSERT INTO filesystems(fs_uuid) VALUES (?1) \
             ON CONFLICT(fs_uuid) DO NOTHING",
            [request.fs_uuid.as_slice()],
        )?;
        let filesystem_id: i64 = transaction.query_row(
            "SELECT id FROM filesystems WHERE fs_uuid = ?1",
            [request.fs_uuid.as_slice()],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO topology_leases( \
                 filesystem_id, lease_owner, lease_fence, lease_expires_ns \
             ) VALUES (?1, NULL, 0, NULL) \
             ON CONFLICT(filesystem_id) DO NOTHING",
            [filesystem_id],
        )?;
        let claimed = transaction.execute(
            "UPDATE topology_leases \
                SET lease_owner = ?2, \
                    lease_fence = lease_fence + 1, \
                    lease_expires_ns = ?3 \
              WHERE filesystem_id = ?1 \
                AND (lease_owner IS NULL \
                     OR lease_expires_ns <= ?4 \
                     OR lease_owner = ?2)",
            params![
                filesystem_id,
                request.lease_owner.as_slice(),
                request.lease_expires_ns,
                request.now_ns,
            ],
        )?;
        if claimed != 1 {
            return Err(ManagerError::new("filesystem topology lease is busy"));
        }
        let topology_fence: i64 = transaction.query_row(
            "SELECT lease_fence FROM topology_leases WHERE filesystem_id = ?1",
            [filesystem_id],
            |row| row.get(0),
        )?;
        reject_source_containing_worktree(&transaction, filesystem_id, &request.source_path)?;

        transaction.execute(
            "INSERT INTO watches( \
                 id, filesystem_id, live_subvol_uuid, live_path, \
                 indexed_revision_id, indexed_seq, last_cut_snapshot_id, \
                 last_cut_seq, cut_owner, cut_fence, cut_expires_ns, \
                 clock_epoch, replay_floor_seq, fsmonitor_state, state \
             ) VALUES ( \
                 ?1, ?2, ?3, ?4, NULL, NULL, NULL, NULL, NULL, 0, NULL, \
                 ?5, NULL, 'disabled', 'initializing' \
             )",
            params![
                watch_id.as_slice(),
                filesystem_id,
                request.source_subvol_uuid.as_slice(),
                request.source_path,
                clock_epoch.as_slice(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO watch_grants( \
                 id, watch_id, principal_kind, principal_id, permissions, \
                 state, created_ns, revoked_ns \
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, NULL)",
            params![
                grant_id.as_slice(),
                watch_id.as_slice(),
                principal_kind,
                principal_id,
                request.permissions.bits(),
                request.now_ns,
            ],
        )?;
        transaction.execute(
            "INSERT INTO operations( \
                 id, kind, state, filesystem_id, watch_id, sequence, \
                 source_subvol_uuid, base_snapshot_id, expected_parent_uuid, \
                 requested_readonly, requester_uid, requester_gid, \
                 authorization_id, reserved_path, lease_owner, lease_fence, \
                 lease_expires_ns, updated_ns \
             ) VALUES ( \
                 ?1, 'initialize', 'planned', ?2, ?3, 0, ?4, NULL, ?4, 1, \
                 ?5, ?6, ?7, ?8, ?9, 1, ?10, ?11 \
             )",
            params![
                operation_id.as_slice(),
                filesystem_id,
                watch_id.as_slice(),
                request.source_subvol_uuid.as_slice(),
                request.requester_uid,
                request.requester_gid,
                grant_id.as_slice(),
                request.reserved_snapshot_path,
                request.lease_owner.as_slice(),
                request.lease_expires_ns,
                request.now_ns,
            ],
        )?;
        let released = transaction.execute(
            "UPDATE topology_leases \
                SET lease_owner = NULL, lease_expires_ns = NULL \
              WHERE filesystem_id = ?1 AND lease_owner = ?2 \
                AND lease_fence = ?3",
            params![
                filesystem_id,
                request.lease_owner.as_slice(),
                topology_fence,
            ],
        )?;
        if released != 1 {
            return Err(ManagerError::new(
                "lost filesystem topology lease while reserving initialize",
            ));
        }
        transaction.commit()?;
        Ok(InitializeReservation {
            filesystem_id,
            watch_id,
            grant_id,
            operation_id,
            clock_epoch,
            operation_fence: 1,
            topology_fence,
        })
    }

    pub fn start_initialize_filesystem_effect(
        &mut self,
        reservation: &InitializeReservation,
        lease_owner: [u8; 16],
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        let changed = self.connection_mut().execute(
            "UPDATE operations \
                SET state = 'fs_started', updated_ns = ?4 \
              WHERE id = ?1 AND watch_id = ?2 AND state = 'planned' \
                AND lease_owner = ?3 AND lease_fence = ?5",
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice(),
                lease_owner.as_slice(),
                now_ns,
                reservation.operation_fence,
            ],
        )?;
        require_one(changed, "start initialize filesystem effect")
    }

    pub fn record_initialize_snapshot(
        &mut self,
        reservation: &InitializeReservation,
        lease_owner: [u8; 16],
        snapshot: &SnapshotIdentity,
        now_ns: i64,
    ) -> Result<RecordedSnapshot, ManagerError> {
        if !snapshot.readonly {
            return Err(ManagerError::new("initialize snapshot is not read-only"));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (filesystem_id, source_uuid, expected_path): (i64, Vec<u8>, Vec<u8>) = transaction
            .query_row(
                "SELECT filesystem_id, source_subvol_uuid, reserved_path \
                   FROM operations \
                  WHERE id = ?1 AND watch_id = ?2 AND state = 'fs_started' \
                    AND lease_owner = ?3 AND lease_fence = ?4",
                params![
                    reservation.operation_id.as_slice(),
                    reservation.watch_id.as_slice(),
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| ManagerError::new("initialize operation fence is stale"))?;
        let source_uuid: [u8; 16] = source_uuid
            .try_into()
            .map_err(|_| ManagerError::new("stored source subvolume UUID has invalid length"))?;
        if filesystem_id != reservation.filesystem_id
            || snapshot.fs_uuid != filesystem_uuid(&transaction, filesystem_id)?
            || snapshot.parent_uuid != Some(source_uuid)
            || snapshot.path != expected_path
        {
            return Err(ManagerError::new(
                "created snapshot identity does not match initialize intent",
            ));
        }
        transaction.execute(
            "INSERT INTO snapshots( \
                 filesystem_id, subvol_uuid, parent_uuid, received_uuid, \
                 root_id, ctransid, otransid, path, readonly, physical_state, \
                 created_ns, deleted_ns \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, 'present', ?9, NULL)",
            params![
                filesystem_id,
                snapshot.subvol_uuid.as_slice(),
                snapshot.parent_uuid.as_ref().map(<[u8; 16]>::as_slice),
                snapshot.received_uuid.as_ref().map(<[u8; 16]>::as_slice),
                encode_u64(snapshot.root_id).as_slice(),
                encode_u64(snapshot.ctransid).as_slice(),
                encode_u64(snapshot.otransid).as_slice(),
                snapshot.path,
                snapshot.created_ns,
            ],
        )?;
        let snapshot_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) \
             VALUES (?1, 'operation', ?2, 'initialize-build')",
            params![snapshot_id, reservation.operation_id.as_slice()],
        )?;
        let changed = transaction.execute(
            "UPDATE operations \
                SET state = 'uuid_recorded', discovered_uuid = ?4, updated_ns = ?5 \
              WHERE id = ?1 AND watch_id = ?2 AND state = 'fs_started' \
                AND lease_owner = ?3 AND lease_fence = ?6",
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice(),
                lease_owner.as_slice(),
                snapshot.subvol_uuid.as_slice(),
                now_ns,
                reservation.operation_fence,
            ],
        )?;
        require_one(changed, "record initialize snapshot")?;
        transaction.commit()?;
        Ok(RecordedSnapshot {
            snapshot_id,
            identity: snapshot.clone(),
        })
    }

    pub fn publish_initial_checkpoint(
        &mut self,
        reservation: &InitializeReservation,
        lease_owner: [u8; 16],
        snapshot: &RecordedSnapshot,
        index: &Index,
        now_ns: i64,
    ) -> Result<InitializedWatch, ManagerError> {
        index.validate().map_err(|error| {
            ManagerError::new(format!("refuse invalid initial checkpoint: {error}"))
        })?;
        let state_hash = index.state_hash();
        let safety = index.safety_summary();
        let owner_counts = index_owner_counts(index)?;
        let owner_cardinality = i64::try_from(owner_counts.len())
            .map_err(|_| ManagerError::new("owner cardinality overflow"))?;
        let owner_uid_xor = owner_uid_xor(&owner_counts);
        let object_count = i64::try_from(index.objects.len())
            .map_err(|_| ManagerError::new("object count exceeds SQLite INTEGER"))?;
        let ref_count = i64::try_from(index.references.len())
            .map_err(|_| ManagerError::new("reference count exceeds SQLite INTEGER"))?;
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_initialize_publication(
            &transaction,
            reservation,
            lease_owner,
            snapshot.snapshot_id,
            snapshot.identity.subvol_uuid,
        )?;
        transaction.execute(
            "INSERT INTO revisions( \
                 snapshot_id, storage_base_revision_id, provenance_comparison_id, \
                 delta_depth, state, builder_owner, builder_fence, \
                 builder_expires_ns, object_count, ref_count, state_hash, \
                 single_owner_uid, privileged_metadata_count, security_state_hash, \
                 owner_cardinality, owner_uid_xor, \
                 summary_version, \
                 created_ns \
             ) VALUES ( \
                 ?1, NULL, NULL, 0, 'building', ?2, ?3, NULL, ?4, ?5, ?6, \
                 ?7, ?8, ?9, ?10, ?11, 2, ?12 \
             )",
            params![
                snapshot.snapshot_id,
                lease_owner.as_slice(),
                reservation.operation_fence,
                object_count,
                ref_count,
                state_hash.as_slice(),
                safety
                    .single_owner_uid
                    .map(encode_u64)
                    .as_ref()
                    .map(<[u8; 8]>::as_slice),
                i64::try_from(safety.privileged_metadata_count)
                    .map_err(|_| ManagerError::new("privileged metadata count overflow"))?,
                safety.security_state_hash.as_slice(),
                owner_cardinality,
                encode_u64(owner_uid_xor).as_slice(),
                now_ns,
            ],
        )?;
        let revision_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO revision_checkpoints( \
                 revision_id, state, builder_owner, builder_fence, \
                 builder_expires_ns, object_count, ref_count, state_hash \
             ) VALUES (?1, 'building', ?2, ?3, NULL, ?4, ?5, ?6)",
            params![
                revision_id,
                lease_owner.as_slice(),
                reservation.operation_fence,
                object_count,
                ref_count,
                state_hash.as_slice(),
            ],
        )?;
        insert_checkpoint(&transaction, revision_id, index)?;
        require_one(
            transaction.execute(
                "UPDATE revision_checkpoints SET state = 'ready' \
                  WHERE revision_id = ?1 AND state = 'building' \
                    AND builder_owner = ?2 AND builder_fence = ?3",
                params![
                    revision_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
            )?,
            "publish initial checkpoint",
        )?;
        require_one(
            transaction.execute(
                "UPDATE revisions SET state = 'ready' \
                  WHERE id = ?1 AND state = 'building' \
                    AND builder_owner = ?2 AND builder_fence = ?3",
                params![
                    revision_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
            )?,
            "publish initial revision",
        )?;
        require_one(
            transaction.execute(
                "UPDATE watches \
                    SET indexed_revision_id = ?2, indexed_seq = 0, \
                        last_cut_snapshot_id = ?3, last_cut_seq = 0, \
                        replay_floor_seq = 0, state = 'active' \
                  WHERE id = ?1 AND state = 'initializing' \
                    AND indexed_revision_id IS NULL AND last_cut_snapshot_id IS NULL",
                params![
                    reservation.watch_id.as_slice(),
                    revision_id,
                    snapshot.snapshot_id,
                ],
            )?,
            "activate initialized watch",
        )?;
        for (kind, reason) in [
            ("watch-indexed-head", "initialized-index-head"),
            ("watch-last-cut", "initialized-physical-head"),
        ] {
            transaction.execute(
                "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    snapshot.snapshot_id,
                    kind,
                    reservation.watch_id.as_slice(),
                    reason,
                ],
            )?;
        }
        transaction.execute(
            "DELETE FROM snapshot_pins \
              WHERE snapshot_id = ?1 AND owner_kind = 'operation' \
                AND owner_id = ?2 AND reason = 'initialize-build'",
            params![snapshot.snapshot_id, reservation.operation_id.as_slice()],
        )?;
        require_one(
            transaction.execute(
                "UPDATE operations \
                    SET state = 'done', lease_owner = NULL, lease_expires_ns = NULL, \
                        updated_ns = ?4 \
                  WHERE id = ?1 AND watch_id = ?2 AND state = 'uuid_recorded' \
                    AND lease_owner = ?3 AND lease_fence = ?5",
                params![
                    reservation.operation_id.as_slice(),
                    reservation.watch_id.as_slice(),
                    lease_owner.as_slice(),
                    now_ns,
                    reservation.operation_fence,
                ],
            )?,
            "complete initialize operation",
        )?;
        transaction.commit()?;
        Ok(InitializedWatch {
            watch_id: reservation.watch_id,
            grant_id: reservation.grant_id,
            revision_id,
            snapshot_id: snapshot.snapshot_id,
            sequence: 0,
            fresh_instance: true,
        })
    }

    pub fn admit_planned_cut(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        requester_session_id: [u8; 16],
        request_kind: &str,
        now_ns: i64,
        expires_ns: i64,
    ) -> Result<Option<CutAdmission>, ManagerError> {
        if !matches!(request_kind, "clock" | "query" | "trigger") || expires_ns <= now_ns {
            return Err(ManagerError::new("invalid cut admission"));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorized: Option<i64> = transaction
            .query_row(
                r#"SELECT 1 FROM watch_grants
                    WHERE id = ?1 AND watch_id = ?2 AND state = 'active'
                      AND (permissions & ?3) = ?3"#,
                params![
                    authorization_id.as_slice(),
                    watch_id.as_slice(),
                    i64::from(PERMISSION_READ | PERMISSION_CUT),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if authorized != Some(1) {
            return Err(ManagerError::new("cut admission authorization is stale"));
        }
        let row: Option<PlannedCutAdmissionRow> = transaction
            .query_row(
                r#"SELECT o.filesystem_id, o.authorization_id, o.sequence,
                          o.base_snapshot_id, o.source_subvol_uuid,
                          o.lease_fence, o.id, w.cut_fence
                     FROM operations o JOIN watches w ON w.id = o.watch_id
                    WHERE o.watch_id = ?1 AND o.kind = 'cut'
                      AND o.state = 'planned'
                    ORDER BY o.sequence LIMIT 1"#,
                [watch_id.as_slice()],
                |row| {
                    Ok(PlannedCutAdmissionRow {
                        filesystem_id: row.get(0)?,
                        authorization_id: row.get(1)?,
                        sequence: row.get(2)?,
                        base_snapshot_id: row.get(3)?,
                        source_subvol_uuid: row.get(4)?,
                        operation_fence: row.get(5)?,
                        operation_id: row.get(6)?,
                        cut_fence: row.get(7)?,
                    })
                },
            )
            .optional()?;
        let Some(row) = row else {
            transaction.commit()?;
            return Ok(None);
        };
        let admission_id = random_id();
        transaction.execute(
            r#"INSERT INTO cut_admissions(
                   id, operation_id, watch_id, authorization_id,
                   requester_session_id, request_kind, state,
                   admitted_ns, expires_ns
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'waiting', ?7, ?8)"#,
            params![
                admission_id.as_slice(),
                row.operation_id,
                watch_id.as_slice(),
                authorization_id.as_slice(),
                requester_session_id.as_slice(),
                request_kind,
                now_ns,
                expires_ns,
            ],
        )?;
        transaction.commit()?;
        Ok(Some(CutAdmission {
            id: admission_id,
            reservation: CutReservation {
                filesystem_id: row.filesystem_id,
                watch_id,
                authorization_id: fixed_manager_blob(
                    &row.authorization_id,
                    "cut operation authorization ID",
                )?,
                operation_id: fixed_manager_blob(&row.operation_id, "cut operation ID")?,
                sequence: row.sequence,
                base_snapshot_id: row.base_snapshot_id,
                source_subvol_uuid: fixed_manager_blob(
                    &row.source_subvol_uuid,
                    "cut source subvolume UUID",
                )?,
                operation_fence: row.operation_fence,
                cut_fence: row.cut_fence,
            },
            requester_session_id,
        }))
    }

    pub fn poll_cut_admission(
        &self,
        admission: &CutAdmission,
        now_ns: i64,
    ) -> Result<Option<PublishedCut>, ManagerError> {
        let state: Option<(String, i64, Vec<u8>)> = self
            .connection()
            .query_row(
                r#"SELECT state, expires_ns, requester_session_id
                     FROM cut_admissions
                    WHERE id = ?1 AND operation_id = ?2 AND watch_id = ?3"#,
                params![
                    admission.id.as_slice(),
                    admission.reservation.operation_id.as_slice(),
                    admission.reservation.watch_id.as_slice(),
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (state, expires_ns, session_id) =
            state.ok_or_else(|| ManagerError::new("cut admission disappeared"))?;
        if session_id != admission.requester_session_id {
            return Err(ManagerError::new("cut admission session is stale"));
        }
        if state == "abandoned" || expires_ns <= now_ns {
            return Err(ManagerError::new("cut admission expired or was abandoned"));
        }
        if state == "waiting" {
            return Ok(None);
        }
        if state != "fulfilled" {
            return Err(ManagerError::new("cut admission has an invalid state"));
        }
        self.load_published_cut(admission.reservation.operation_id)
            .map(Some)
    }

    pub fn abandon_cut_admission(&mut self, admission: &CutAdmission) -> Result<(), ManagerError> {
        require_one(
            self.connection_mut().execute(
                r#"UPDATE cut_admissions SET state = 'abandoned'
                    WHERE id = ?1 AND operation_id = ?2 AND watch_id = ?3
                      AND requester_session_id = ?4 AND state = 'waiting'"#,
                params![
                    admission.id.as_slice(),
                    admission.reservation.operation_id.as_slice(),
                    admission.reservation.watch_id.as_slice(),
                    admission.requester_session_id.as_slice(),
                ],
            )?,
            "abandon cut admission",
        )
    }

    pub fn reserve_cut(&mut self, request: &CutRequest) -> Result<CutReservation, ManagerError> {
        if request.reserved_snapshot_path.is_empty() {
            return Err(ManagerError::new("cut snapshot path must not be empty"));
        }
        if request.lease_expires_ns <= request.now_ns {
            return Err(ManagerError::new("cut lease must expire after admission"));
        }
        let operation_id = random_id();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorization: Option<i64> = transaction
            .query_row(
                r#"SELECT permissions
                     FROM watch_grants
                    WHERE id = ?1 AND watch_id = ?2 AND state = 'active'"#,
                params![
                    request.authorization_id.as_slice(),
                    request.watch_id.as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        let authorization = authorization
            .ok_or_else(|| ManagerError::new("cut authorization is absent or revoked"))?;
        let required = i64::from(PERMISSION_READ | PERMISSION_CUT);
        if authorization & required != required {
            return Err(ManagerError::new("grant lacks READ|CUT"));
        }

        let watch: Option<(i64, Vec<u8>, i64, i64)> = transaction
            .query_row(
                r#"SELECT filesystem_id, live_subvol_uuid,
                          last_cut_snapshot_id, last_cut_seq
                     FROM watches
                    WHERE id = ?1 AND state = 'active'"#,
                [request.watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let (filesystem_id, source_uuid, base_snapshot_id, last_cut_seq) =
            watch.ok_or_else(|| ManagerError::new("watch is not active"))?;
        let source_subvol_uuid: [u8; 16] = source_uuid
            .try_into()
            .map_err(|_| ManagerError::new("watch source UUID has invalid length"))?;
        let claimed = transaction.execute(
            r#"UPDATE watches
                  SET cut_owner = ?2,
                      cut_fence = cut_fence + 1,
                      cut_expires_ns = ?3
                WHERE id = ?1 AND state = 'active'
                  AND (cut_owner IS NULL OR cut_expires_ns <= ?4 OR cut_owner = ?2)"#,
            params![
                request.watch_id.as_slice(),
                request.lease_owner.as_slice(),
                request.lease_expires_ns,
                request.now_ns,
            ],
        )?;
        require_one(claimed, "claim watch cut lease")?;
        let cut_fence: i64 = transaction.query_row(
            "SELECT cut_fence FROM watches WHERE id = ?1",
            [request.watch_id.as_slice()],
            |row| row.get(0),
        )?;
        let max_operation_sequence: Option<i64> = transaction.query_row(
            "SELECT max(sequence) FROM operations WHERE watch_id = ?1",
            [request.watch_id.as_slice()],
            |row| row.get(0),
        )?;
        let sequence = max_operation_sequence.unwrap_or(last_cut_seq) + 1;
        if sequence <= last_cut_seq {
            return Err(ManagerError::new("cut sequence did not advance"));
        }
        transaction.execute(
            r#"INSERT INTO operations(
                   id, kind, state, filesystem_id, watch_id, sequence,
                   source_subvol_uuid, base_snapshot_id, expected_parent_uuid,
                   requested_readonly, requester_uid, requester_gid,
                   authorization_id, reserved_path, lease_owner, lease_fence,
                   lease_expires_ns, updated_ns
               ) VALUES (
                   ?1, 'cut', 'planned', ?2, ?3, ?4, ?5, ?6, ?5, 1,
                   ?7, ?8, ?9, ?10, ?11, 1, ?12, ?13
               )"#,
            params![
                operation_id.as_slice(),
                filesystem_id,
                request.watch_id.as_slice(),
                sequence,
                source_subvol_uuid.as_slice(),
                base_snapshot_id,
                request.requester_uid,
                request.requester_gid,
                request.authorization_id.as_slice(),
                request.reserved_snapshot_path,
                request.lease_owner.as_slice(),
                request.lease_expires_ns,
                request.now_ns,
            ],
        )?;
        transaction.execute(
            r#"INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason)
               VALUES (?1, 'operation', ?2, 'cut-base')"#,
            params![base_snapshot_id, operation_id.as_slice()],
        )?;
        transaction.commit()?;
        Ok(CutReservation {
            filesystem_id,
            watch_id: request.watch_id,
            authorization_id: request.authorization_id,
            operation_id,
            sequence,
            base_snapshot_id,
            source_subvol_uuid,
            operation_fence: 1,
            cut_fence,
        })
    }

    pub fn start_cut_filesystem_effect(
        &mut self,
        reservation: &CutReservation,
        lease_owner: [u8; 16],
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        let changed = self.connection_mut().execute(
            r#"UPDATE operations
                  SET state = 'fs_started', updated_ns = ?4
                WHERE id = ?1 AND watch_id = ?2 AND state = 'planned'
                  AND lease_owner = ?3 AND lease_fence = ?5
                  AND sequence = ?6"#,
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice(),
                lease_owner.as_slice(),
                now_ns,
                reservation.operation_fence,
                reservation.sequence,
            ],
        )?;
        require_one(changed, "start cut filesystem effect")
    }

    pub fn record_cut_snapshot(
        &mut self,
        reservation: &CutReservation,
        lease_owner: [u8; 16],
        snapshot: &SnapshotIdentity,
        now_ns: i64,
    ) -> Result<RecordedSnapshot, ManagerError> {
        if !snapshot.readonly
            || snapshot.fs_uuid
                != filesystem_uuid_from_connection(self.connection(), reservation.filesystem_id)?
            || snapshot.parent_uuid != Some(reservation.source_subvol_uuid)
        {
            return Err(ManagerError::new(
                "cut snapshot identity does not match the source intent",
            ));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expected_path: Option<Vec<u8>> = transaction
            .query_row(
                r#"SELECT reserved_path FROM operations
                    WHERE id = ?1 AND watch_id = ?2 AND sequence = ?3
                      AND state = 'fs_started' AND lease_owner = ?4
                      AND lease_fence = ?5"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.watch_id.as_slice(),
                    reservation.sequence,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
                |row| row.get(0),
            )
            .optional()?;
        if expected_path.as_deref() != Some(snapshot.path.as_slice()) {
            return Err(ManagerError::new(
                "cut snapshot path or operation fence does not match",
            ));
        }
        let snapshot_id = insert_snapshot(&transaction, reservation.filesystem_id, snapshot)?;
        transaction.execute(
            r#"INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason)
               VALUES (?1, 'operation', ?2, 'cut-target')"#,
            params![snapshot_id, reservation.operation_id.as_slice()],
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE operations
                      SET state = 'uuid_recorded', discovered_uuid = ?4, updated_ns = ?5
                    WHERE id = ?1 AND watch_id = ?2 AND sequence = ?3
                      AND state = 'fs_started' AND lease_owner = ?6
                      AND lease_fence = ?7"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.watch_id.as_slice(),
                    reservation.sequence,
                    snapshot.subvol_uuid.as_slice(),
                    now_ns,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
            )?,
            "record cut snapshot",
        )?;
        transaction.commit()?;
        Ok(RecordedSnapshot {
            snapshot_id,
            identity: snapshot.clone(),
        })
    }

    /// Publishes the physical cut only after the caller has independently
    /// validated the immutable snapshot's nested-subvolume and fscrypt policy.
    pub fn publish_validated_physical_cut(
        &mut self,
        reservation: &CutReservation,
        lease_owner: [u8; 16],
        snapshot: &RecordedSnapshot,
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_cut_snapshot(
            &transaction,
            reservation,
            lease_owner,
            snapshot.snapshot_id,
            snapshot.identity.subvol_uuid,
        )?;
        transaction.execute(
            r#"INSERT INTO watch_cuts(
                   watch_id, sequence, operation_id, base_snapshot_id,
                   target_snapshot_id, comparison_id,
                   comparison_from_snapshot_id, state, fresh_instance
               ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, 'created', 0)"#,
            params![
                reservation.watch_id.as_slice(),
                reservation.sequence,
                reservation.operation_id.as_slice(),
                reservation.base_snapshot_id,
                snapshot.snapshot_id,
            ],
        )?;
        transaction.execute(
            r#"INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason)
               VALUES (?1, 'watch-last-cut', ?2, 'physical-head')"#,
            params![snapshot.snapshot_id, reservation.watch_id.as_slice()],
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE watches
                      SET last_cut_snapshot_id = ?2, last_cut_seq = ?3,
                          cut_owner = NULL, cut_expires_ns = NULL
                    WHERE id = ?1 AND state = 'active'
                      AND last_cut_snapshot_id = ?4 AND last_cut_seq = ?5
                      AND cut_owner = ?6 AND cut_fence = ?7"#,
                params![
                    reservation.watch_id.as_slice(),
                    snapshot.snapshot_id,
                    reservation.sequence,
                    reservation.base_snapshot_id,
                    reservation.sequence - 1,
                    lease_owner.as_slice(),
                    reservation.cut_fence,
                ],
            )?,
            "advance physical cut head",
        )?;
        transaction.execute(
            r#"DELETE FROM snapshot_pins
                WHERE snapshot_id = ?1 AND owner_kind = 'watch-last-cut'
                  AND owner_id = ?2"#,
            params![
                reservation.base_snapshot_id,
                reservation.watch_id.as_slice(),
            ],
        )?;
        transaction.execute(
            "UPDATE operations SET state = 'manifest_ready', updated_ns = ?2 WHERE id = ?1",
            params![reservation.operation_id.as_slice(), now_ns],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn publish_adjacent_delta(
        &mut self,
        reservation: &CutReservation,
        lease_owner: [u8; 16],
        snapshot: &RecordedSnapshot,
        manifest: &ChangedObjectsManifest,
        target_objects: &BTreeMap<u64, Object>,
        now_ns: i64,
    ) -> Result<PublishedCut, ManagerError> {
        let (base_revision_id, base_snapshot_id, mut base_depth): (i64, i64, i64) = self
            .connection()
            .query_row(
                r#"SELECT w.indexed_revision_id, r.snapshot_id, r.delta_depth
                     FROM watches w JOIN revisions r ON r.id = w.indexed_revision_id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND w.indexed_seq = ?2 AND r.state = 'ready'"#,
                params![reservation.watch_id.as_slice(), reservation.sequence - 1,],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| ManagerError::new("prior cut is not the ready indexed head"))?;
        if base_snapshot_id != reservation.base_snapshot_id {
            return Err(ManagerError::new(
                "cut base snapshot differs from indexed predecessor",
            ));
        }
        if base_depth >= 31 {
            self.compact_revision(base_revision_id, lease_owner)?;
            base_depth = 0;
        }
        let mut base_metadata = self.load_revision_metadata(base_revision_id)?;
        if base_metadata.summary_version != 2 {
            self.compact_revision(base_revision_id, lease_owner)?;
            base_depth = 0;
            base_metadata = self.load_revision_metadata(base_revision_id)?;
        }
        let base_subset = self.load_revision_delta_subset(base_revision_id, manifest)?;
        let applied = apply_manifest(&base_subset, manifest, target_objects).map_err(|error| {
            ManagerError::new(format!("apply changed-object manifest: {error}"))
        })?;
        let chain = self.revision_storage_chain(base_revision_id)?;
        let mut touched_uids = BTreeSet::new();
        for &ino in manifest.objects.keys() {
            touched_uids.extend(base_subset.objects.get(&ino).map(|object| object.uid));
            touched_uids.extend(applied.index.objects.get(&ino).map(|object| object.uid));
        }
        let mut base_owner_counts = BTreeMap::new();
        for uid in touched_uids {
            base_owner_counts.insert(uid, self.load_revision_owner_count_from_chain(&chain, uid)?);
        }
        let (revision_metadata, owner_count_overrides) = apply_revision_metadata(
            &base_metadata,
            &base_subset,
            &applied.index,
            manifest,
            &base_owner_counts,
        )?;
        let manifest_hash = manifest.canonical_hash();
        stage_delta_rows(
            self.connection_mut(),
            manifest,
            &applied.events,
            &applied.index,
            &owner_count_overrides,
        )?;

        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_delta_publication(
            &transaction,
            reservation,
            lease_owner,
            snapshot.snapshot_id,
            base_revision_id,
        )?;
        transaction.execute(
            r#"INSERT INTO comparisons(
                   from_snapshot_id, to_snapshot_id, comparison_kind,
                   algorithm_version, state, lease_owner, lease_fence,
                   lease_expires_ns, manifest_hash, raw_ref_adds, raw_ref_deletes
               ) VALUES (?1, ?2, 'incremental', 1, 'claimed', ?3, ?4, NULL,
                         ?5, ?6, ?7)"#,
            params![
                base_snapshot_id,
                snapshot.snapshot_id,
                lease_owner.as_slice(),
                reservation.operation_fence,
                manifest_hash.as_slice(),
                i64::try_from(manifest.raw_ref_adds)
                    .map_err(|_| ManagerError::new("raw ref add count overflow"))?,
                i64::try_from(manifest.raw_ref_deletes)
                    .map_err(|_| ManagerError::new("raw ref delete count overflow"))?,
            ],
        )?;
        let comparison_id = transaction.last_insert_rowid();
        import_staged_comparison_rows(&transaction, comparison_id)?;
        transaction.execute(
            r#"INSERT INTO revisions(
                   snapshot_id, storage_base_revision_id, provenance_comparison_id,
                   delta_depth, state, builder_owner, builder_fence,
                   builder_expires_ns, object_count, ref_count, state_hash,
                   single_owner_uid, privileged_metadata_count, security_state_hash,
                   owner_cardinality, owner_uid_xor,
                   summary_version,
                   created_ns
               ) VALUES (?1, ?2, ?3, ?4, 'building', ?5, ?6, NULL, ?7, ?8,
                         ?9, ?10, ?11, ?12, ?13, ?14, 2, ?15)"#,
            params![
                snapshot.snapshot_id,
                base_revision_id,
                comparison_id,
                base_depth + 1,
                lease_owner.as_slice(),
                reservation.operation_fence,
                revision_metadata.object_count,
                revision_metadata.ref_count,
                revision_metadata.state_hash.as_slice(),
                revision_metadata
                    .single_owner_uid
                    .map(encode_u64)
                    .as_ref()
                    .map(<[u8; 8]>::as_slice),
                revision_metadata.privileged_metadata_count,
                revision_metadata.security_state_hash.as_slice(),
                revision_metadata.owner_cardinality,
                encode_u64(revision_metadata.owner_uid_xor).as_slice(),
                now_ns,
            ],
        )?;
        let revision_id = transaction.last_insert_rowid();
        import_staged_revision_rows(&transaction, revision_id)?;
        require_one(
            transaction.execute(
                r#"UPDATE revisions SET state = 'ready'
                    WHERE id = ?1 AND state = 'building'
                      AND builder_owner = ?2 AND builder_fence = ?3"#,
                params![
                    revision_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
            )?,
            "publish delta revision",
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE comparisons SET state = 'index_ready'
                    WHERE id = ?1 AND state = 'claimed'
                      AND lease_owner = ?2 AND lease_fence = ?3"#,
                params![
                    comparison_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
            )?,
            "publish comparison",
        )?;
        transaction.execute(
            r#"INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason)
               VALUES (?1, 'watch-indexed-head', ?2, 'indexed-head')"#,
            params![snapshot.snapshot_id, reservation.watch_id.as_slice()],
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE watches
                      SET indexed_revision_id = ?2, indexed_seq = ?3
                    WHERE id = ?1 AND state = 'active'
                      AND indexed_revision_id = ?4 AND indexed_seq = ?5"#,
                params![
                    reservation.watch_id.as_slice(),
                    revision_id,
                    reservation.sequence,
                    base_revision_id,
                    reservation.sequence - 1,
                ],
            )?,
            "advance indexed watch head",
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE watch_cuts
                      SET comparison_id = ?3, comparison_from_snapshot_id = ?4,
                          state = 'ready'
                    WHERE watch_id = ?1 AND sequence = ?2 AND state = 'created'
                      AND target_snapshot_id = ?5"#,
                params![
                    reservation.watch_id.as_slice(),
                    reservation.sequence,
                    comparison_id,
                    base_snapshot_id,
                    snapshot.snapshot_id,
                ],
            )?,
            "mark watch cut ready",
        )?;
        transaction.execute(
            r#"DELETE FROM snapshot_pins
                WHERE snapshot_id = ?1 AND owner_kind = 'watch-indexed-head'
                  AND owner_id = ?2"#,
            params![base_snapshot_id, reservation.watch_id.as_slice()],
        )?;
        transaction.execute(
            r#"DELETE FROM snapshot_pins
                WHERE owner_kind = 'operation' AND owner_id = ?1
                  AND snapshot_id IN (?2, ?3)"#,
            params![
                reservation.operation_id.as_slice(),
                base_snapshot_id,
                snapshot.snapshot_id,
            ],
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE operations
                      SET state = 'done', lease_owner = NULL, lease_expires_ns = NULL,
                          updated_ns = ?4
                    WHERE id = ?1 AND watch_id = ?2 AND state = 'manifest_ready'
                      AND lease_owner = ?3 AND lease_fence = ?5"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.watch_id.as_slice(),
                    lease_owner.as_slice(),
                    now_ns,
                    reservation.operation_fence,
                ],
            )?,
            "complete cut operation",
        )?;
        transaction.execute(
            "UPDATE cut_admissions SET state = 'fulfilled' \
             WHERE operation_id = ?1 AND watch_id = ?2 AND state = 'waiting'",
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice(),
            ],
        )?;
        let trigger_projection = project_events(&applied.events, ClientFlavor::Jj);
        if trigger_projection.fresh_instance || !trigger_projection.paths.is_empty() {
            transaction.execute(
                r#"UPDATE watchman_triggers
                      SET pending_through_seq = MAX(
                              COALESCE(pending_through_seq, ?2), ?2
                          )
                    WHERE watch_id = ?1 AND state = 'active'"#,
                params![reservation.watch_id.as_slice(), reservation.sequence],
            )?;
        }
        transaction.commit()?;
        // Publication is already durable; failure to reclaim connection-local
        // staging must not turn a committed cut into an apparent failure. The
        // next stage build clears these tables before use, and connection exit
        // removes the TEMP database.
        let _ = clear_staged_delta(self.connection_mut());
        Ok(PublishedCut {
            watch_id: reservation.watch_id,
            sequence: reservation.sequence,
            snapshot_id: snapshot.snapshot_id,
            revision_id,
            comparison_id,
            events: applied.events,
        })
    }

    pub fn fail_cut_comparison(
        &mut self,
        reservation: &CutReservation,
        lease_owner: [u8; 16],
        error: &str,
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        if error.is_empty() {
            return Err(ManagerError::new("terminal cut failure needs an error"));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_one(
            transaction.execute(
                r#"UPDATE watch_cuts SET state = 'failed'
                    WHERE watch_id = ?1 AND sequence = ?2
                      AND operation_id = ?3 AND state = 'created'
                      AND comparison_id IS NULL"#,
                params![
                    reservation.watch_id.as_slice(),
                    reservation.sequence,
                    reservation.operation_id.as_slice(),
                ],
            )?,
            "fail cut comparison",
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE operations
                      SET state = 'failed', error = ?4, lease_owner = NULL,
                          lease_expires_ns = NULL, updated_ns = ?5
                    WHERE id = ?1 AND watch_id = ?2 AND sequence = ?3
                      AND state = 'manifest_ready' AND lease_owner = ?6
                      AND lease_fence = ?7"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.watch_id.as_slice(),
                    reservation.sequence,
                    error,
                    now_ns,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
            )?,
            "fail cut operation",
        )?;
        transaction.execute(
            "DELETE FROM snapshot_pins WHERE owner_kind = 'operation' AND owner_id = ?1",
            [reservation.operation_id.as_slice()],
        )?;
        transaction.execute(
            r#"UPDATE cut_admissions SET state = 'abandoned'
                WHERE operation_id = ?1 AND watch_id = ?2 AND state = 'waiting'"#,
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice()
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn publish_full_fresh_checkpoint(
        &mut self,
        reservation: &CutReservation,
        lease_owner: [u8; 16],
        snapshot: &RecordedSnapshot,
        index: &Index,
        now_ns: i64,
    ) -> Result<PublishedCut, ManagerError> {
        index.validate().map_err(|error| {
            ManagerError::new(format!("refuse invalid gap-recovery checkpoint: {error}"))
        })?;
        let events = full_fresh_events(index)?;
        let state_hash = index.state_hash();
        let safety = index.safety_summary();
        let owner_counts = index_owner_counts(index)?;
        let owner_cardinality = i64::try_from(owner_counts.len())
            .map_err(|_| ManagerError::new("owner cardinality overflow"))?;
        let object_count = i64::try_from(index.objects.len())
            .map_err(|_| ManagerError::new("object count exceeds SQLite INTEGER"))?;
        let ref_count = i64::try_from(index.references.len())
            .map_err(|_| ManagerError::new("reference count exceeds SQLite INTEGER"))?;
        let raw_ref_count = i64::try_from(index.references.len())
            .map_err(|_| ManagerError::new("reference count exceeds SQLite INTEGER"))?;

        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head: Option<(i64, i64)> = transaction
            .query_row(
                r#"SELECT w.indexed_revision_id, w.indexed_seq
                     FROM watches w JOIN revisions r ON r.id = w.indexed_revision_id
                    WHERE w.id = ?1 AND w.state = 'active' AND r.state = 'ready'
                      AND w.indexed_seq < ?2"#,
                params![reservation.watch_id.as_slice(), reservation.sequence],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (base_revision_id, base_sequence) =
            head.ok_or_else(|| ManagerError::new("gap target is not newer than the indexed head"))?;
        let base_snapshot_id: i64 = transaction.query_row(
            "SELECT snapshot_id FROM revisions WHERE id = ?1 AND state = 'ready'",
            [base_revision_id],
            |row| row.get(0),
        )?;
        let current_valid: Option<i64> = transaction
            .query_row(
                r#"SELECT 1 FROM operations o
                     JOIN watch_cuts c ON c.operation_id = o.id
                                      AND c.watch_id = o.watch_id
                                      AND c.sequence = o.sequence
                     JOIN snapshots s ON s.id = c.target_snapshot_id
                    WHERE o.id = ?1 AND o.watch_id = ?2 AND o.sequence = ?3
                      AND o.state = 'manifest_ready' AND o.lease_owner = ?4
                      AND o.lease_fence = ?5 AND c.state = 'created'
                      AND c.target_snapshot_id = ?6 AND s.subvol_uuid = ?7
                      AND s.physical_state = 'present' AND s.readonly = 1"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.watch_id.as_slice(),
                    reservation.sequence,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                    snapshot.snapshot_id,
                    snapshot.identity.subvol_uuid.as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if current_valid != Some(1) {
            return Err(ManagerError::new(
                "full-fresh publication fence or target identity is stale",
            ));
        }
        let failed_count: i64 = transaction.query_row(
            r#"SELECT count(*) FROM watch_cuts c
                 JOIN operations o ON o.id = c.operation_id
                                  AND o.watch_id = c.watch_id
                                  AND o.sequence = c.sequence
                WHERE c.watch_id = ?1 AND c.sequence > ?2 AND c.sequence < ?3
                  AND c.state = 'failed' AND o.state = 'failed'"#,
            params![
                reservation.watch_id.as_slice(),
                base_sequence,
                reservation.sequence
            ],
            |row| row.get(0),
        )?;
        let expected_failed = reservation.sequence - base_sequence - 1;
        if failed_count != expected_failed {
            return Err(ManagerError::new(
                "gap contains a cut which is not terminally failed",
            ));
        }

        transaction.execute(
            r#"INSERT INTO comparisons(
                   from_snapshot_id, to_snapshot_id, comparison_kind,
                   algorithm_version, state, lease_owner, lease_fence,
                   lease_expires_ns, manifest_hash, raw_ref_adds, raw_ref_deletes
               ) VALUES (?1, ?2, 'full_fresh', 1, 'claimed', ?3, ?4, NULL,
                         ?5, ?6, 0)"#,
            params![
                base_snapshot_id,
                snapshot.snapshot_id,
                lease_owner.as_slice(),
                reservation.operation_fence,
                state_hash.as_slice(),
                raw_ref_count,
            ],
        )?;
        let comparison_id = transaction.last_insert_rowid();
        {
            let mut objects = transaction.prepare_cached(
                r#"INSERT INTO comparison_objects(
                       comparison_id, ino, old_generation, new_generation, change_mask)
                   VALUES (?1, ?2, NULL, ?3, ?4)"#,
            )?;
            for object in index.objects.values() {
                objects.execute(params![
                    comparison_id,
                    encode_u64(object.ino).as_slice(),
                    encode_u64(object.generation).as_slice(),
                    i64::try_from(CHANGE_CREATED | CHANGE_INODE)
                        .expect("change mask fits SQLite INTEGER"),
                ])?;
            }
        }
        {
            let mut references = transaction.prepare_cached(
                r#"INSERT INTO comparison_refs(
                       comparison_id, operation, ino, parent_ino, name)
                   VALUES (?1, 1, ?2, ?3, ?4)"#,
            )?;
            for reference in &index.references {
                references.execute(params![
                    comparison_id,
                    encode_u64(reference.ino).as_slice(),
                    encode_u64(reference.parent_ino).as_slice(),
                    reference.name,
                ])?;
            }
        }
        {
            let mut stored_events = transaction.prepare_cached(
                r#"INSERT INTO change_events(
                       comparison_id, ordinal, event_kind, ino, old_generation,
                       new_generation, change_mask, old_path, new_path)
                   VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL, ?7)"#,
            )?;
            for (ordinal, event) in events.iter().enumerate() {
                stored_events.execute(params![
                    comparison_id,
                    i64::try_from(ordinal)
                        .map_err(|_| ManagerError::new("event ordinal overflow"))?,
                    event_kind_name(&event.kind),
                    encode_u64(event.ino).as_slice(),
                    event
                        .new_generation
                        .map(encode_u64)
                        .as_ref()
                        .map(<[u8; 8]>::as_slice),
                    i64::try_from(event.change_mask)
                        .map_err(|_| ManagerError::new("event mask overflow"))?,
                    event.new_path,
                ])?;
            }
        }
        transaction.execute(
            r#"INSERT INTO revisions(
                   snapshot_id, storage_base_revision_id, provenance_comparison_id,
                   delta_depth, state, builder_owner, builder_fence,
                   builder_expires_ns, object_count, ref_count, state_hash,
                   single_owner_uid, privileged_metadata_count, security_state_hash,
                   owner_cardinality, owner_uid_xor, summary_version, created_ns)
               VALUES (?1, NULL, ?2, 0, 'building', ?3, ?4, NULL, ?5, ?6,
                       ?7, ?8, ?9, ?10, ?11, ?12, 2, ?13)"#,
            params![
                snapshot.snapshot_id,
                comparison_id,
                lease_owner.as_slice(),
                reservation.operation_fence,
                object_count,
                ref_count,
                state_hash.as_slice(),
                safety
                    .single_owner_uid
                    .map(encode_u64)
                    .as_ref()
                    .map(<[u8; 8]>::as_slice),
                i64::try_from(safety.privileged_metadata_count)
                    .map_err(|_| ManagerError::new("privileged metadata count overflow"))?,
                safety.security_state_hash.as_slice(),
                owner_cardinality,
                encode_u64(owner_uid_xor(&owner_counts)).as_slice(),
                now_ns,
            ],
        )?;
        let revision_id = transaction.last_insert_rowid();
        transaction.execute(
            r#"INSERT INTO revision_checkpoints(
                   revision_id, state, builder_owner, builder_fence,
                   builder_expires_ns, object_count, ref_count, state_hash)
               VALUES (?1, 'building', ?2, ?3, NULL, ?4, ?5, ?6)"#,
            params![
                revision_id,
                lease_owner.as_slice(),
                reservation.operation_fence,
                object_count,
                ref_count,
                state_hash.as_slice(),
            ],
        )?;
        insert_checkpoint(&transaction, revision_id, index)?;
        require_one(
            transaction.execute(
                r#"UPDATE revision_checkpoints SET state = 'ready'
                    WHERE revision_id = ?1 AND state = 'building'
                      AND builder_owner = ?2 AND builder_fence = ?3"#,
                params![
                    revision_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence
                ],
            )?,
            "publish gap-recovery checkpoint",
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE revisions SET state = 'ready'
                    WHERE id = ?1 AND state = 'building'
                      AND builder_owner = ?2 AND builder_fence = ?3"#,
                params![
                    revision_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence
                ],
            )?,
            "publish gap-recovery revision",
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE comparisons SET state = 'index_ready'
                    WHERE id = ?1 AND state = 'claimed'
                      AND lease_owner = ?2 AND lease_fence = ?3"#,
                params![
                    comparison_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence
                ],
            )?,
            "publish full-fresh comparison",
        )?;
        transaction.execute(
            r#"INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason)
               VALUES (?1, 'watch-indexed-head', ?2, 'gap-recovery-head')"#,
            params![snapshot.snapshot_id, reservation.watch_id.as_slice()],
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE watches SET indexed_revision_id = ?2, indexed_seq = ?3
                    WHERE id = ?1 AND state = 'active'
                      AND indexed_revision_id = ?4 AND indexed_seq = ?5"#,
                params![
                    reservation.watch_id.as_slice(),
                    revision_id,
                    reservation.sequence,
                    base_revision_id,
                    base_sequence,
                ],
            )?,
            "advance indexed head across terminal gap",
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE watch_cuts
                      SET comparison_id = ?3, comparison_from_snapshot_id = ?4,
                          state = 'ready', fresh_instance = 1
                    WHERE watch_id = ?1 AND sequence = ?2 AND state = 'created'
                      AND target_snapshot_id = ?5"#,
                params![
                    reservation.watch_id.as_slice(),
                    reservation.sequence,
                    comparison_id,
                    base_snapshot_id,
                    snapshot.snapshot_id,
                ],
            )?,
            "mark gap-recovery cut ready",
        )?;
        transaction.execute(
            r#"DELETE FROM snapshot_pins
                WHERE snapshot_id = ?1 AND owner_kind = 'watch-indexed-head'
                  AND owner_id = ?2"#,
            params![base_snapshot_id, reservation.watch_id.as_slice()],
        )?;
        transaction.execute(
            "DELETE FROM snapshot_pins WHERE owner_kind = 'operation' AND owner_id = ?1",
            [reservation.operation_id.as_slice()],
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE operations
                      SET state = 'done', lease_owner = NULL, lease_expires_ns = NULL,
                          updated_ns = ?4
                    WHERE id = ?1 AND watch_id = ?2 AND state = 'manifest_ready'
                      AND lease_owner = ?3 AND lease_fence = ?5"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.watch_id.as_slice(),
                    lease_owner.as_slice(),
                    now_ns,
                    reservation.operation_fence,
                ],
            )?,
            "complete gap-recovery operation",
        )?;
        transaction.execute(
            r#"UPDATE cut_admissions SET state = 'fulfilled'
                WHERE operation_id = ?1 AND watch_id = ?2 AND state = 'waiting'"#,
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice()
            ],
        )?;
        transaction.execute(
            r#"UPDATE watchman_triggers
                  SET pending_through_seq = MAX(COALESCE(pending_through_seq, ?2), ?2)
                WHERE watch_id = ?1 AND state = 'active'"#,
            params![reservation.watch_id.as_slice(), reservation.sequence],
        )?;
        transaction.commit()?;
        Ok(PublishedCut {
            watch_id: reservation.watch_id,
            sequence: reservation.sequence,
            snapshot_id: snapshot.snapshot_id,
            revision_id,
            comparison_id,
            events,
        })
    }

    pub fn compact_revision(
        &mut self,
        revision_id: i64,
        builder_owner: [u8; 16],
    ) -> Result<bool, ManagerError> {
        let index = self.load_revision(revision_id)?;
        let object_count = i64::try_from(index.objects.len())
            .map_err(|_| ManagerError::new("checkpoint object count overflow"))?;
        let ref_count = i64::try_from(index.references.len())
            .map_err(|_| ManagerError::new("checkpoint reference count overflow"))?;
        let state_hash = index.state_hash();
        let safety = index.safety_summary();
        let owner_counts = index_owner_counts(&index)?;
        let owner_cardinality = i64::try_from(owner_counts.len())
            .map_err(|_| ManagerError::new("owner cardinality overflow"))?;
        let owner_uid_xor = owner_uid_xor(&owner_counts);
        let privileged_metadata_count = i64::try_from(safety.privileged_metadata_count)
            .map_err(|_| ManagerError::new("privileged metadata count overflow"))?;
        let summary = RevisionMetadata {
            summary_version: 2,
            owner_cardinality,
            owner_uid_xor,
            object_count,
            ref_count,
            state_hash,
            single_owner_uid: safety.single_owner_uid,
            privileged_metadata_count,
            security_state_hash: safety.security_state_hash,
        };
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision: Option<(Option<i64>, i64, String, i64)> = transaction
            .query_row(
                "SELECT storage_base_revision_id, delta_depth, state, summary_version \
                 FROM revisions WHERE id = ?1",
                [revision_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let (storage_base, delta_depth, state, summary_version) =
            revision.ok_or_else(|| ManagerError::new("revision disappeared during compaction"))?;
        if state != "ready" {
            return Err(ManagerError::new("only ready revisions can be compacted"));
        }
        let checkpoint_state: Option<String> = transaction
            .query_row(
                "SELECT state FROM revision_checkpoints WHERE revision_id = ?1",
                [revision_id],
                |row| row.get(0),
            )
            .optional()?;
        if checkpoint_state.as_deref() == Some("ready") {
            if storage_base.is_none() && delta_depth == 0 {
                if summary_version != 2 {
                    replace_checkpoint_owner_counts(&transaction, revision_id, &owner_counts)?;
                    update_revision_summary(&transaction, revision_id, &summary)?;
                }
                transaction.commit()?;
                return Ok(summary_version != 2);
            }
            require_one(
                transaction.execute(
                    "UPDATE revisions SET storage_base_revision_id = NULL, delta_depth = 0 \
                     WHERE id = ?1 AND state = 'ready'",
                    [revision_id],
                )?,
                "activate existing revision checkpoint",
            )?;
        } else {
            if checkpoint_state.is_some() {
                return Err(ManagerError::new(
                    "revision has an incomplete checkpoint owned by another job",
                ));
            }
            transaction.execute(
                r#"INSERT INTO revision_checkpoints(
                       revision_id, state, builder_owner, builder_fence,
                       builder_expires_ns, object_count, ref_count, state_hash
                   ) VALUES (?1, 'building', ?2, 1, NULL, ?3, ?4, ?5)"#,
                params![
                    revision_id,
                    builder_owner.as_slice(),
                    object_count,
                    ref_count,
                    state_hash.as_slice(),
                ],
            )?;
            insert_checkpoint(&transaction, revision_id, &index)?;
            require_one(
                transaction.execute(
                    r#"UPDATE revision_checkpoints SET state = 'ready'
                        WHERE revision_id = ?1 AND state = 'building'
                          AND builder_owner = ?2 AND builder_fence = 1"#,
                    params![revision_id, builder_owner.as_slice()],
                )?,
                "publish compacted checkpoint",
            )?;
            require_one(
                transaction.execute(
                    r#"UPDATE revisions
                          SET storage_base_revision_id = NULL, delta_depth = 0,
                              state_hash = ?2
                        WHERE id = ?1 AND state = 'ready'"#,
                    params![revision_id, state_hash.as_slice()],
                )?,
                "activate compacted checkpoint",
            )?;
        }
        update_revision_summary(&transaction, revision_id, &summary)?;
        transaction.execute(
            "DELETE FROM object_overrides WHERE revision_id = ?1",
            [revision_id],
        )?;
        transaction.execute(
            "DELETE FROM ref_overrides WHERE revision_id = ?1",
            [revision_id],
        )?;
        transaction.execute(
            "DELETE FROM owner_count_overrides WHERE revision_id = ?1",
            [revision_id],
        )?;
        transaction.commit()?;
        let reloaded = self.load_checkpoint(revision_id)?;
        if reloaded != index {
            return Err(ManagerError::new(
                "published checkpoint differs from its source revision",
            ));
        }
        Ok(true)
    }

    pub fn advance_replay_floor(
        &mut self,
        watch_id: [u8; 16],
        new_floor: i64,
        now_ns: i64,
        compactor_owner: [u8; 16],
    ) -> Result<usize, ManagerError> {
        let (current_floor, indexed_sequence, indexed_revision): (i64, i64, i64) = self
            .connection()
            .query_row(
                "SELECT replay_floor_seq, indexed_seq, indexed_revision_id \
                 FROM watches WHERE id = ?1 AND state IN ('active', 'blocked')",
                [watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| ManagerError::new("replay-floor watch is absent"))?;
        if new_floor < current_floor || new_floor > indexed_sequence {
            return Err(ManagerError::new("invalid replay-floor advance"));
        }
        if new_floor == current_floor {
            return Ok(0);
        }
        self.compact_revision(indexed_revision, compactor_owner)?;

        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM query_revision_pins WHERE query_id IN (\
                 SELECT id FROM query_leases \
                  WHERE state = 'active' AND lease_expires_ns <= ?1\
             )",
            [now_ns],
        )?;
        transaction.execute(
            "DELETE FROM query_comparison_pins WHERE query_id IN (\
                 SELECT id FROM query_leases \
                  WHERE state = 'active' AND lease_expires_ns <= ?1\
             )",
            [now_ns],
        )?;
        transaction.execute(
            "UPDATE query_leases SET state = 'released', lease_fence = lease_fence + 1 \
             WHERE state = 'active' AND lease_expires_ns <= ?1",
            [now_ns],
        )?;
        let conflicting_queries: i64 = transaction.query_row(
            r#"SELECT count(*) FROM query_leases
                WHERE watch_id = ?1 AND state = 'active'
                  AND from_cut_sequence IS NOT NULL
                  AND from_cut_sequence < ?2"#,
            params![watch_id.as_slice(), new_floor],
            |row| row.get(0),
        )?;
        if conflicting_queries != 0 {
            return Err(ManagerError::new(
                "active query lease still protects history below the requested floor",
            ));
        }
        let guard_floor: Option<(Vec<u8>, i64)> = transaction
            .query_row(
                r#"SELECT b.guard_epoch, b.guard_sequence
                     FROM fsmonitor_boundaries b
                     JOIN watches w ON w.id = b.watch_id
                    WHERE b.watch_id = ?1 AND b.cut_sequence = ?2
                      AND b.guard_complete = 1
                      AND b.guard_epoch = w.guard_epoch
                      AND b.guard_sequence >= w.guard_replay_floor_seq"#,
                params![watch_id.as_slice(), new_floor],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((guard_epoch, guard_sequence)) = guard_floor {
            let guarded_queries: i64 = transaction.query_row(
                r#"SELECT count(*) FROM query_leases
                    WHERE watch_id = ?1 AND state = 'active'
                      AND guard_epoch = ?2 AND from_guard_sequence < ?3"#,
                params![watch_id.as_slice(), guard_epoch, guard_sequence],
                |row| row.get(0),
            )?;
            if guarded_queries == 0 {
                transaction.execute(
                    "DELETE FROM mutation_events WHERE watch_id = ?1 \
                     AND guard_epoch = ?2 AND sequence <= ?3",
                    params![watch_id.as_slice(), guard_epoch, guard_sequence],
                )?;
                transaction.execute(
                    "UPDATE watches SET guard_replay_floor_seq = ?2 \
                     WHERE id = ?1 AND guard_epoch = ?3 \
                       AND guard_replay_floor_seq <= ?2",
                    params![watch_id.as_slice(), guard_sequence, guard_epoch],
                )?;
            }
        }
        let comparison_selector = "SELECT comparison_id FROM watch_cuts c WHERE c.watch_id = ?1 \
             AND c.sequence <= ?2 AND c.comparison_id IS NOT NULL \
             AND NOT EXISTS (SELECT 1 FROM query_comparison_pins p \
                              WHERE p.comparison_id = c.comparison_id)";
        for table in ["change_events", "comparison_refs", "comparison_objects"] {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE comparison_id IN ({comparison_selector})"),
                params![watch_id.as_slice(), new_floor],
            )?;
        }
        transaction.execute(
            "DELETE FROM fsmonitor_boundaries \
             WHERE watch_id = ?1 AND cut_sequence < ?2",
            params![watch_id.as_slice(), new_floor],
        )?;
        transaction.execute(
            r#"DELETE FROM mutation_events
                WHERE watch_id = ?1
                  AND guard_epoch NOT IN (
                      SELECT guard_epoch FROM fsmonitor_boundaries
                       WHERE watch_id = ?1 AND guard_complete = 1
                      UNION
                      SELECT guard_epoch FROM query_leases
                       WHERE watch_id = ?1 AND state = 'active'
                         AND guard_epoch IS NOT NULL
                  )"#,
            [watch_id.as_slice()],
        )?;
        transaction.execute(
            "DELETE FROM cut_admissions WHERE operation_id IN (\
                 SELECT operation_id FROM watch_cuts \
                  WHERE watch_id = ?1 AND sequence < ?2\
             )",
            params![watch_id.as_slice(), new_floor],
        )?;
        transaction.execute(
            "DELETE FROM watch_cuts WHERE watch_id = ?1 AND sequence < ?2",
            params![watch_id.as_slice(), new_floor],
        )?;
        transaction.execute(
            "DELETE FROM operations WHERE watch_id = ?1 AND kind = 'cut' \
             AND state = 'done' AND sequence < ?2",
            params![watch_id.as_slice(), new_floor],
        )?;
        require_one(
            transaction.execute(
                "UPDATE watches SET replay_floor_seq = ?2 \
                 WHERE id = ?1 AND replay_floor_seq = ?3 AND indexed_seq >= ?2",
                params![watch_id.as_slice(), new_floor, current_floor],
            )?,
            "advance watch replay floor",
        )?;

        let mut reclaimed = 0_usize;
        loop {
            let candidate: Option<i64> = transaction
                .query_row(
                    r#"SELECT r.id FROM revisions r
                        WHERE r.state = 'ready'
                          AND NOT EXISTS (
                              SELECT 1 FROM watches w
                               WHERE w.indexed_revision_id = r.id)
                          AND NOT EXISTS (
                              SELECT 1 FROM worktrees wt
                               WHERE wt.seed_revision_id = r.id)
                          AND NOT EXISTS (
                              SELECT 1 FROM revisions child
                               WHERE child.storage_base_revision_id = r.id)
                          AND NOT EXISTS (
                              SELECT 1 FROM query_revision_pins p
                               WHERE p.revision_id = r.id)
                          AND NOT EXISTS (
                              SELECT 1 FROM snapshot_pins p
                               WHERE p.snapshot_id = r.snapshot_id)
                          AND NOT EXISTS (
                              SELECT 1 FROM fsmonitor_boundaries b
                               WHERE b.target_snapshot_id = r.snapshot_id)
                        ORDER BY r.id LIMIT 1"#,
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(revision_id) = candidate else {
                break;
            };
            transaction.execute(
                "DELETE FROM checkpoint_objects WHERE revision_id = ?1",
                [revision_id],
            )?;
            transaction.execute(
                "DELETE FROM checkpoint_refs WHERE revision_id = ?1",
                [revision_id],
            )?;
            transaction.execute(
                "DELETE FROM checkpoint_owner_counts WHERE revision_id = ?1",
                [revision_id],
            )?;
            transaction.execute(
                "DELETE FROM revision_checkpoints WHERE revision_id = ?1",
                [revision_id],
            )?;
            transaction.execute(
                "DELETE FROM object_overrides WHERE revision_id = ?1",
                [revision_id],
            )?;
            transaction.execute(
                "DELETE FROM ref_overrides WHERE revision_id = ?1",
                [revision_id],
            )?;
            transaction.execute(
                "DELETE FROM owner_count_overrides WHERE revision_id = ?1",
                [revision_id],
            )?;
            require_one(
                transaction.execute("DELETE FROM revisions WHERE id = ?1", [revision_id])?,
                "reclaim logical revision",
            )?;
            reclaimed += 1;
        }
        for table in ["change_events", "comparison_refs", "comparison_objects"] {
            transaction.execute(
                &format!(
                    "DELETE FROM {table} WHERE comparison_id IN (\
                         SELECT id FROM comparisons c \
                          WHERE NOT EXISTS (SELECT 1 FROM revisions r \
                                             WHERE r.provenance_comparison_id = c.id) \
                            AND NOT EXISTS (SELECT 1 FROM watch_cuts wc \
                                             WHERE wc.comparison_id = c.id) \
                            AND NOT EXISTS (SELECT 1 FROM query_comparison_pins p \
                                             WHERE p.comparison_id = c.id) \
                            AND NOT (c.algorithm_version = 2 \
                                     AND c.state = 'index_ready' \
                                     AND EXISTS (SELECT 1 FROM revisions a \
                                                  WHERE a.snapshot_id = c.from_snapshot_id \
                                                    AND a.state = 'ready') \
                                     AND EXISTS (SELECT 1 FROM revisions b \
                                                  WHERE b.snapshot_id = c.to_snapshot_id \
                                                    AND b.state = 'ready'))\
                     )"
                ),
                [],
            )?;
        }
        transaction.execute(
            r#"DELETE FROM comparisons
                WHERE NOT EXISTS (
                    SELECT 1 FROM revisions r
                     WHERE r.provenance_comparison_id = comparisons.id)
                  AND NOT EXISTS (
                    SELECT 1 FROM watch_cuts wc
                     WHERE wc.comparison_id = comparisons.id)
                  AND NOT EXISTS (
                    SELECT 1 FROM query_comparison_pins p
                     WHERE p.comparison_id = comparisons.id)
                  AND NOT (comparisons.algorithm_version = 2
                           AND comparisons.state = 'index_ready'
                           AND EXISTS (SELECT 1 FROM revisions a
                                        WHERE a.snapshot_id = comparisons.from_snapshot_id
                                          AND a.state = 'ready')
                           AND EXISTS (SELECT 1 FROM revisions b
                                        WHERE b.snapshot_id = comparisons.to_snapshot_id
                                          AND b.state = 'ready'))"#,
            [],
        )?;
        transaction.commit()?;
        Ok(reclaimed)
    }

    /// Trusted provisioning step which anchors a WORKTREE grant to one
    /// immutable destination-root policy generation.
    pub fn provision_worktree_policy(
        &mut self,
        watch_id: [u8; 16],
        grant_id: [u8; 16],
        policy: &WorktreePolicy,
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        if !matches!(
            policy.metadata_policy.as_str(),
            "sanitized-private-user-tree" | "admin-trusted-preserve"
        ) || policy.allow_idmapped
            || worktree_policy_hash(policy) != policy.policy_hash
        {
            return Err(ManagerError::new("invalid Worktree policy"));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let filesystem_id: i64 = transaction
            .query_row(
                r#"SELECT w.filesystem_id
                     FROM watches w JOIN watch_grants g ON g.watch_id = w.id
                    WHERE w.id = ?1 AND w.state = 'active' AND g.id = ?2
                      AND g.state = 'active'"#,
                params![watch_id.as_slice(), grant_id.as_slice()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| ManagerError::new("Worktree grant is absent or revoked"))?;
        if filesystem_uuid(&transaction, filesystem_id)? != policy.destination_fs_uuid {
            return Err(ManagerError::new(
                "Worktree destination must be on the watched Btrfs filesystem",
            ));
        }
        require_one(
            transaction.execute(
                "UPDATE watch_grants SET permissions = permissions | ?3 \
                 WHERE id = ?1 AND watch_id = ?2 AND state = 'active'",
                params![
                    grant_id.as_slice(),
                    watch_id.as_slice(),
                    PERMISSION_WORKTREE
                ],
            )?,
            "add WORKTREE permission",
        )?;
        transaction.execute(
            r#"INSERT INTO worktree_grant_policies(
                   id, grant_id, destination_filesystem_id,
                   destination_root_subvol_uuid, destination_root_path, destination_root_ino,
                   destination_root_generation, metadata_policy, allow_idmapped,
                   policy_hash, created_ns
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"#,
            params![
                policy.policy_id.as_slice(),
                grant_id.as_slice(),
                filesystem_id,
                policy.destination_root_subvol_uuid.as_slice(),
                policy.destination_root_path,
                encode_u64(crate::index::ROOT_INO).as_slice(),
                encode_u64(policy.destination_root_generation).as_slice(),
                policy.metadata_policy,
                i64::from(policy.allow_idmapped),
                policy.policy_hash.as_slice(),
                now_ns,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn reserve_worktree(
        &mut self,
        request: &WorktreeRequest,
    ) -> Result<WorktreeReservation, ManagerError> {
        if request.lease_expires_ns <= request.now_ns
            || !path_is_absolute(&request.staged_path)
            || !path_is_absolute(&request.final_path)
            || request.destination_name.is_empty()
            || request.reservation_name.is_empty()
            || worktree_policy_hash(&request.policy) != request.policy.policy_hash
        {
            return Err(ManagerError::new("invalid Worktree reservation request"));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let policy_valid: Option<i64> = transaction
            .query_row(
                r#"SELECT 1
                     FROM worktree_grant_policies p
                     JOIN watch_grants g ON g.id = p.grant_id
                     JOIN filesystems f ON f.id = p.destination_filesystem_id
                    WHERE p.id = ?1 AND p.grant_id = ?2 AND g.watch_id = ?3
                      AND g.state = 'active'
                      AND (g.permissions & ?4) = ?4
                      AND f.fs_uuid = ?5
                      AND p.destination_root_subvol_uuid = ?6
                      AND p.destination_root_path = ?7
                      AND p.destination_root_generation = ?8
                      AND p.metadata_policy = ?9 AND p.allow_idmapped = ?10
                      AND p.policy_hash = ?11"#,
                params![
                    request.policy.policy_id.as_slice(),
                    request.authorization_id.as_slice(),
                    request.watch_id.as_slice(),
                    i64::from(PERMISSION_READ | PERMISSION_CUT | PERMISSION_WORKTREE),
                    request.policy.destination_fs_uuid.as_slice(),
                    request.policy.destination_root_subvol_uuid.as_slice(),
                    request.policy.destination_root_path,
                    encode_u64(request.policy.destination_root_generation).as_slice(),
                    request.policy.metadata_policy,
                    i64::from(request.policy.allow_idmapped),
                    request.policy.policy_hash.as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if policy_valid != Some(1) {
            return Err(ManagerError::new(
                "Worktree policy or authorization is stale",
            ));
        }
        let head: WorktreeHead = transaction.query_row(
            r#"SELECT w.filesystem_id, w.indexed_revision_id,
                          w.last_cut_snapshot_id, w.indexed_seq, w.last_cut_seq,
                          r.single_owner_uid, r.privileged_metadata_count,
                          s.subvol_uuid
                     FROM watches w
                     JOIN revisions r ON r.id = w.indexed_revision_id
                     JOIN snapshots s ON s.id = w.last_cut_snapshot_id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND r.state = 'ready' AND s.physical_state = 'present'"#,
            [request.watch_id.as_slice()],
            |row| {
                Ok(WorktreeHead {
                    filesystem_id: row.get(0)?,
                    revision_id: row.get(1)?,
                    snapshot_id: row.get(2)?,
                    indexed_seq: row.get(3)?,
                    last_cut_seq: row.get(4)?,
                    single_owner_uid: row.get(5)?,
                    privileged_metadata_count: row.get(6)?,
                    subvol_uuid: row.get(7)?,
                })
            },
        )?;
        if head.indexed_seq != head.last_cut_seq {
            return Err(ManagerError::new(
                "Worktree requires the indexed and physical heads to match",
            ));
        }
        if request.policy.metadata_policy == "sanitized-private-user-tree" {
            let owner = head
                .single_owner_uid
                .as_deref()
                .map(decode_u64)
                .transpose()?
                .ok_or_else(|| ManagerError::new("Worktree seed has mixed ownership"))?;
            if owner != u64::from(request.requester_uid) || head.privileged_metadata_count != 0 {
                return Err(ManagerError::new(
                    "Worktree seed is not a sanitized caller-owned tree",
                ));
            }
        }
        transaction.execute(
            "INSERT INTO topology_leases( \
                 filesystem_id, lease_owner, lease_fence, lease_expires_ns \
             ) VALUES (?1, NULL, 0, NULL) \
             ON CONFLICT(filesystem_id) DO NOTHING",
            [head.filesystem_id],
        )?;
        let topology_fence = claim_topology_lease(
            &transaction,
            head.filesystem_id,
            request.lease_owner,
            request.now_ns,
            request.lease_expires_ns,
        )?;
        reject_destination_below_watch(&transaction, head.filesystem_id, &request.final_path)?;
        let seed_subvol_uuid = fixed_manager_blob::<16>(&head.subvol_uuid, "seed subvolume UUID")?;
        let operation_id = random_id();
        let worktree_id = random_id();
        transaction.execute(
            r#"INSERT INTO operations(
                   id, kind, state, filesystem_id, watch_id, sequence,
                   source_subvol_uuid, base_snapshot_id, expected_parent_uuid,
                   requested_readonly, requester_uid, requester_gid,
                   authorization_id, worktree_policy_id, reserved_path, final_path,
                   destination_parent_subvol_uuid, destination_parent_ino,
                   destination_parent_generation, destination_name,
                   destination_reservation_name, destination_reservation_ino,
                   destination_reservation_generation, destination_reservation_nonce,
                   lease_owner, lease_fence, lease_expires_ns, updated_ns
               ) VALUES (
                   ?1, 'worktree', 'planned', ?2, ?3, NULL, ?4, ?5, ?4, 0,
                   ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20, 1, ?21, ?22
               )"#,
            params![
                operation_id.as_slice(),
                head.filesystem_id,
                request.watch_id.as_slice(),
                seed_subvol_uuid.as_slice(),
                head.snapshot_id,
                request.requester_uid,
                request.requester_gid,
                request.authorization_id.as_slice(),
                request.policy.policy_id.as_slice(),
                request.staged_path,
                request.final_path,
                request.destination_parent_subvol_uuid.as_slice(),
                encode_u64(request.destination_parent_ino).as_slice(),
                encode_u64(request.destination_parent_generation).as_slice(),
                request.destination_name,
                request.reservation_name,
                encode_u64(request.reservation_ino).as_slice(),
                encode_u64(request.reservation_generation).as_slice(),
                request.reservation_nonce.as_slice(),
                request.lease_owner.as_slice(),
                request.lease_expires_ns,
                request.now_ns,
            ],
        )?;
        transaction.execute(
            "INSERT INTO worktrees(id, filesystem_id, subvol_uuid, path, \
             seed_revision_id, watch_id, operation_id, state) \
             VALUES (?1, ?2, NULL, ?3, ?4, NULL, ?5, 'creating')",
            params![
                worktree_id.as_slice(),
                head.filesystem_id,
                request.final_path,
                head.revision_id,
                operation_id.as_slice(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) \
             VALUES (?1, 'operation', ?2, 'worktree-seed')",
            params![head.snapshot_id, operation_id.as_slice()],
        )?;
        release_topology_lease(
            &transaction,
            head.filesystem_id,
            request.lease_owner,
            topology_fence,
        )?;
        transaction.commit()?;
        Ok(WorktreeReservation {
            operation_id,
            worktree_id,
            filesystem_id: head.filesystem_id,
            seed_snapshot_id: head.snapshot_id,
            seed_revision_id: head.revision_id,
            seed_subvol_uuid,
            operation_fence: 1,
            policy_hash: request.policy.policy_hash,
        })
    }

    pub fn start_worktree_effect(
        &mut self,
        reservation: &WorktreeReservation,
        lease_owner: [u8; 16],
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        require_one(
            self.connection_mut().execute(
                "UPDATE operations SET state = 'fs_started', updated_ns = ?4 \
                 WHERE id = ?1 AND state = 'planned' AND lease_owner = ?2 \
                   AND lease_fence = ?3",
                params![
                    reservation.operation_id.as_slice(),
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                    now_ns
                ],
            )?,
            "start Worktree effect",
        )
    }

    pub fn record_created_worktree(
        &mut self,
        reservation: &WorktreeReservation,
        lease_owner: [u8; 16],
        subvol_uuid: [u8; 16],
        parent_uuid: Option<[u8; 16]>,
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        if parent_uuid != Some(reservation.seed_subvol_uuid) {
            return Err(ManagerError::new(
                "writable clone has the wrong parent UUID",
            ));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_one(transaction.execute(
            "UPDATE worktrees SET subvol_uuid = ?2 WHERE id = ?1 AND state = 'creating' AND subvol_uuid IS NULL",
            params![reservation.worktree_id.as_slice(), subvol_uuid.as_slice()],
        )?, "record Worktree UUID")?;
        require_one(transaction.execute(
            "UPDATE operations SET state = 'awaiting_destination', discovered_uuid = ?4, updated_ns = ?5 \
             WHERE id = ?1 AND state = 'fs_started' AND lease_owner = ?2 AND lease_fence = ?3",
            params![reservation.operation_id.as_slice(), lease_owner.as_slice(), reservation.operation_fence, subvol_uuid.as_slice(), now_ns],
        )?, "record created Worktree")?;
        transaction.commit()?;
        Ok(())
    }

    /// Reacquires the filesystem topology exclusion and rechecks the final
    /// locator immediately before the broker is allowed to publish it. The
    /// caller keeps this lease until `publish_worktree` commits.
    pub fn prepare_worktree_publication(
        &mut self,
        reservation: &WorktreeReservation,
        lease_owner: [u8; 16],
        resolved_final_path: &[u8],
        now_ns: i64,
        lease_expires_ns: i64,
    ) -> Result<i64, ManagerError> {
        if lease_expires_ns <= now_ns || !path_is_absolute(resolved_final_path) {
            return Err(ManagerError::new(
                "invalid Worktree topology publication request",
            ));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorized: Option<i64> = transaction
            .query_row(
                r#"SELECT 1
                     FROM operations o
                     JOIN watch_grants g
                       ON g.id = o.authorization_id AND g.watch_id = o.watch_id
                     JOIN worktree_grant_policies p ON p.id = o.worktree_policy_id
                    WHERE o.id = ?1 AND o.kind = 'worktree'
                      AND o.state = 'awaiting_destination'
                      AND o.filesystem_id = ?2 AND o.lease_owner = ?3
                      AND o.lease_fence = ?4 AND g.state = 'active'
                      AND (g.permissions & ?5) = ?5 AND p.policy_hash = ?6"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.filesystem_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                    i64::from(PERMISSION_READ | PERMISSION_CUT | PERMISSION_WORKTREE),
                    reservation.policy_hash.as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if authorized != Some(1) {
            return Err(ManagerError::new("Worktree publication authority is stale"));
        }
        let topology_fence = claim_topology_lease(
            &transaction,
            reservation.filesystem_id,
            lease_owner,
            now_ns,
            lease_expires_ns,
        )?;
        reject_destination_below_watch(
            &transaction,
            reservation.filesystem_id,
            resolved_final_path,
        )?;
        transaction.commit()?;
        Ok(topology_fence)
    }

    pub fn publish_worktree(
        &mut self,
        reservation: &WorktreeReservation,
        lease_owner: [u8; 16],
        topology_fence: i64,
        subvol_uuid: [u8; 16],
        now_ns: i64,
    ) -> Result<TrackedWorktree, ManagerError> {
        let child_watch_id = random_id();
        let child_grant_id = random_id();
        let child_clock_epoch = random_id();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let topology_valid: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM topology_leases \
                 WHERE filesystem_id = ?1 AND lease_owner = ?2 AND lease_fence = ?3",
                params![
                    reservation.filesystem_id,
                    lease_owner.as_slice(),
                    topology_fence,
                ],
                |row| row.get(0),
            )
            .optional()?;
        if topology_valid != Some(1) {
            return Err(ManagerError::new(
                "lost filesystem topology lease before Worktree publication",
            ));
        }
        let (final_path, principal_kind, principal_id, permissions): (
            Vec<u8>,
            String,
            Vec<u8>,
            i64,
        ) = transaction.query_row(
            r#"SELECT o.final_path, g.principal_kind, g.principal_id, g.permissions
                 FROM operations o JOIN watch_grants g
                   ON g.id = o.authorization_id AND g.watch_id = o.watch_id
                WHERE o.id = ?1 AND o.state = 'awaiting_destination'
                  AND o.lease_owner = ?2 AND o.lease_fence = ?3
                  AND o.discovered_uuid = ?4 AND g.state = 'active'"#,
            params![
                reservation.operation_id.as_slice(),
                lease_owner.as_slice(),
                reservation.operation_fence,
                subvol_uuid.as_slice(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        transaction.execute(
            r#"INSERT INTO watches(
                   id, filesystem_id, live_subvol_uuid, live_path,
                   indexed_revision_id, indexed_seq, last_cut_snapshot_id,
                   last_cut_seq, cut_owner, cut_fence, cut_expires_ns,
                   clock_epoch, replay_floor_seq, fsmonitor_state, state
               ) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, 0, NULL, 0, NULL,
                         ?7, 0, 'disabled', 'active')"#,
            params![
                child_watch_id.as_slice(),
                reservation.filesystem_id,
                subvol_uuid.as_slice(),
                final_path,
                reservation.seed_revision_id,
                reservation.seed_snapshot_id,
                child_clock_epoch.as_slice(),
            ],
        )?;
        transaction.execute(
            r#"INSERT INTO watch_grants(
                   id, watch_id, principal_kind, principal_id, permissions,
                   state, created_ns, revoked_ns
               ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, NULL)"#,
            params![
                child_grant_id.as_slice(),
                child_watch_id.as_slice(),
                principal_kind,
                principal_id,
                permissions,
                now_ns,
            ],
        )?;
        require_one(
            transaction.execute(
                "UPDATE worktrees SET state = 'present', watch_id = ?3 \
             WHERE id = ?1 AND state = 'creating' AND subvol_uuid = ?2",
                params![
                    reservation.worktree_id.as_slice(),
                    subvol_uuid.as_slice(),
                    child_watch_id.as_slice()
                ],
            )?,
            "publish Worktree row",
        )?;
        require_one(transaction.execute(
            "UPDATE operations SET state = 'done', lease_owner = NULL, lease_expires_ns = NULL, updated_ns = ?5 \
             WHERE id = ?1 AND state = 'awaiting_destination' AND lease_owner = ?2 \
               AND lease_fence = ?3 AND discovered_uuid = ?4",
            params![reservation.operation_id.as_slice(), lease_owner.as_slice(), reservation.operation_fence, subvol_uuid.as_slice(), now_ns],
        )?, "finish Worktree operation")?;
        transaction.execute(
            "DELETE FROM snapshot_pins WHERE snapshot_id = ?1 AND owner_kind = 'operation' AND owner_id = ?2 AND reason = 'worktree-seed'",
            params![reservation.seed_snapshot_id, reservation.operation_id.as_slice()],
        )?;
        for (kind, reason) in [
            ("watch-indexed-head", "worktree-seed-index"),
            ("watch-last-cut", "worktree-seed-cut"),
        ] {
            transaction.execute(
                "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    reservation.seed_snapshot_id,
                    kind,
                    child_watch_id.as_slice(),
                    reason,
                ],
            )?;
        }
        release_topology_lease(
            &transaction,
            reservation.filesystem_id,
            lease_owner,
            topology_fence,
        )?;
        transaction.commit()?;
        Ok(TrackedWorktree {
            worktree_id: reservation.worktree_id,
            watch_id: child_watch_id,
            grant_id: child_grant_id,
            seed_revision_id: reservation.seed_revision_id,
            seed_snapshot_id: reservation.seed_snapshot_id,
        })
    }

    pub fn create_retention_lease(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        snapshot_id: i64,
        now_ns: i64,
        expires_ns: i64,
    ) -> Result<RetentionLease, ManagerError> {
        if expires_ns <= now_ns {
            return Err(ManagerError::new(
                "retention lease must expire after creation",
            ));
        }
        let id = random_id();
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorized: Option<i64> = transaction
            .query_row(
                r#"SELECT 1
                     FROM watches w
                     JOIN watch_grants g ON g.watch_id = w.id
                     JOIN snapshots s ON s.id = ?3 AND s.filesystem_id = w.filesystem_id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND g.id = ?2 AND g.state = 'active'
                      AND (g.permissions & ?4) = ?4
                      AND s.physical_state = 'present'
                      AND (w.last_cut_snapshot_id = s.id OR EXISTS (
                           SELECT 1 FROM watch_cuts c
                            WHERE c.watch_id = w.id
                              AND (c.base_snapshot_id = s.id OR c.target_snapshot_id = s.id)))"#,
                params![
                    watch_id.as_slice(),
                    authorization_id.as_slice(),
                    snapshot_id,
                    i64::from(PERMISSION_RETAIN),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if authorized != Some(1) {
            return Err(ManagerError::new(
                "retention target is not an authorized snapshot of this watch",
            ));
        }
        transaction.execute(
            r#"INSERT INTO retention_leases(
                   id, watch_id, authorization_id, snapshot_id, state,
                   lease_fence, expires_ns, created_ns)
               VALUES (?1, ?2, ?3, ?4, 'active', 0, ?5, ?6)"#,
            params![
                id.as_slice(),
                watch_id.as_slice(),
                authorization_id.as_slice(),
                snapshot_id,
                expires_ns,
                now_ns,
            ],
        )?;
        transaction.execute(
            "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) \
             VALUES (?1, 'retention-lease', ?2, 'caller-retention')",
            params![snapshot_id, id.as_slice()],
        )?;
        transaction.commit()?;
        Ok(RetentionLease {
            id,
            watch_id,
            authorization_id,
            snapshot_id,
            lease_fence: 0,
            expires_ns,
        })
    }

    pub fn release_retention_lease(&mut self, lease: &RetentionLease) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_one(
            transaction.execute(
                r#"UPDATE retention_leases
                      SET state = 'released', lease_fence = lease_fence + 1
                    WHERE id = ?1 AND watch_id = ?2 AND authorization_id = ?3
                      AND snapshot_id = ?4 AND state = 'active'
                      AND lease_fence = ?5"#,
                params![
                    lease.id.as_slice(),
                    lease.watch_id.as_slice(),
                    lease.authorization_id.as_slice(),
                    lease.snapshot_id,
                    lease.lease_fence,
                ],
            )?,
            "release retention lease",
        )?;
        require_one(
            transaction.execute(
                "DELETE FROM snapshot_pins WHERE snapshot_id = ?1 \
                 AND owner_kind = 'retention-lease' AND owner_id = ?2 \
                 AND reason = 'caller-retention'",
                params![lease.snapshot_id, lease.id.as_slice()],
            )?,
            "release retention pin",
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn expire_retention_leases(&mut self, now_ns: i64) -> Result<usize, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expired: i64 = transaction.query_row(
            "SELECT count(*) FROM retention_leases \
             WHERE state = 'active' AND expires_ns <= ?1",
            [now_ns],
            |row| row.get(0),
        )?;
        transaction.execute(
            r#"DELETE FROM snapshot_pins
                WHERE owner_kind = 'retention-lease'
                  AND EXISTS (
                      SELECT 1 FROM retention_leases r
                       WHERE r.id = snapshot_pins.owner_id
                         AND r.snapshot_id = snapshot_pins.snapshot_id
                         AND r.state = 'active' AND r.expires_ns <= ?1)"#,
            [now_ns],
        )?;
        transaction.execute(
            "UPDATE retention_leases SET state = 'expired', lease_fence = lease_fence + 1 \
             WHERE state = 'active' AND expires_ns <= ?1",
            [now_ns],
        )?;
        transaction.commit()?;
        usize::try_from(expired).map_err(|_| ManagerError::new("expired lease count overflow"))
    }

    pub fn reserve_unpinned_snapshot_deletes(
        &mut self,
        lease_owner: [u8; 16],
        now_ns: i64,
        lease_expires_ns: i64,
        limit: usize,
    ) -> Result<Vec<SnapshotDeleteReservation>, ManagerError> {
        if lease_expires_ns <= now_ns || limit == 0 {
            return Err(ManagerError::new(
                "snapshot-delete lease and positive batch limit are required",
            ));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates = {
            let mut statement = transaction.prepare(
                r#"SELECT s.id, s.filesystem_id, f.fs_uuid, s.subvol_uuid,
                          s.parent_uuid, s.received_uuid, s.root_id, s.ctransid,
                          s.otransid, s.path, s.readonly, s.created_ns
                     FROM snapshots s
                     JOIN filesystems f ON f.id = s.filesystem_id
                    WHERE s.physical_state = 'present'
                      AND NOT EXISTS (
                          SELECT 1 FROM snapshot_pins p WHERE p.snapshot_id = s.id
                      )
                      AND NOT EXISTS (
                          SELECT 1 FROM snapshot_delete_operations d
                           WHERE d.snapshot_id = s.id AND d.state != 'done'
                      )
                    ORDER BY s.id
                    LIMIT ?1"#,
            )?;
            let rows = statement.query_map(
                [i64::try_from(limit)
                    .map_err(|_| ManagerError::new("snapshot-delete batch limit overflow"))?],
                decode_snapshot_delete_candidate,
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut reservations = Vec::new();
        for (snapshot_id, filesystem_id, identity) in candidates {
            let changed = transaction.execute(
                r#"UPDATE snapshots
                      SET physical_state = 'deleting'
                    WHERE id = ?1 AND physical_state = 'present'
                      AND NOT EXISTS (
                          SELECT 1 FROM snapshot_pins p WHERE p.snapshot_id = snapshots.id
                      )"#,
                [snapshot_id],
            )?;
            if changed == 0 {
                continue;
            }
            let operation_id = random_id();
            transaction.execute(
                r#"INSERT INTO snapshot_delete_operations(
                       id, snapshot_id, filesystem_id, state, lease_owner,
                       lease_fence, lease_expires_ns, error, updated_ns
                   ) VALUES (?1, ?2, ?3, 'planned', ?4, 1, ?5, NULL, ?6)"#,
                params![
                    operation_id.as_slice(),
                    snapshot_id,
                    filesystem_id,
                    lease_owner.as_slice(),
                    lease_expires_ns,
                    now_ns,
                ],
            )?;
            reservations.push(SnapshotDeleteReservation {
                operation_id,
                snapshot_id,
                filesystem_id,
                operation_fence: 1,
                identity,
            });
        }
        transaction.commit()?;
        Ok(reservations)
    }

    pub fn start_snapshot_delete(
        &mut self,
        reservation: &SnapshotDeleteReservation,
        lease_owner: [u8; 16],
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        require_one(
            self.connection_mut().execute(
                r#"UPDATE snapshot_delete_operations
                      SET state = 'fs_started', updated_ns = ?5
                    WHERE id = ?1 AND snapshot_id = ?2 AND state = 'planned'
                      AND lease_owner = ?3 AND lease_fence = ?4"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.snapshot_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                    now_ns,
                ],
            )?,
            "start snapshot delete",
        )
    }

    pub fn record_snapshot_delete_durable(
        &mut self,
        reservation: &SnapshotDeleteReservation,
        lease_owner: [u8; 16],
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_one(
            transaction.execute(
                r#"UPDATE snapshot_delete_operations
                      SET state = 'fs_deleted', updated_ns = ?5
                    WHERE id = ?1 AND snapshot_id = ?2 AND state = 'fs_started'
                      AND lease_owner = ?3 AND lease_fence = ?4"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.snapshot_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                    now_ns,
                ],
            )?,
            "record snapshot namespace deletion",
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE snapshot_delete_operations
                      SET state = 'delete_durable', updated_ns = ?5
                    WHERE id = ?1 AND snapshot_id = ?2 AND state = 'fs_deleted'
                      AND lease_owner = ?3 AND lease_fence = ?4"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.snapshot_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                    now_ns,
                ],
            )?,
            "record durable snapshot deletion",
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn finish_snapshot_delete(
        &mut self,
        reservation: &SnapshotDeleteReservation,
        lease_owner: [u8; 16],
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_one(
            transaction.execute(
                r#"UPDATE snapshots
                      SET physical_state = 'deleted', deleted_ns = ?2
                    WHERE id = ?1 AND physical_state = 'deleting'
                      AND NOT EXISTS (
                          SELECT 1 FROM snapshot_pins p WHERE p.snapshot_id = snapshots.id
                      )"#,
                params![reservation.snapshot_id, now_ns],
            )?,
            "tombstone deleted snapshot",
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE snapshot_delete_operations
                      SET state = 'done', lease_owner = NULL,
                          lease_expires_ns = NULL, updated_ns = ?5
                    WHERE id = ?1 AND snapshot_id = ?2 AND state = 'delete_durable'
                      AND lease_owner = ?3 AND lease_fence = ?4"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.snapshot_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                    now_ns,
                ],
            )?,
            "finish snapshot delete",
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn load_published_cut(&self, operation_id: [u8; 16]) -> Result<PublishedCut, ManagerError> {
        let row: Option<(Vec<u8>, i64, i64, i64, i64)> = self
            .connection()
            .query_row(
                r#"SELECT c.watch_id, c.sequence, c.target_snapshot_id,
                          r.id, c.comparison_id
                     FROM watch_cuts c
                     JOIN revisions r
                       ON r.snapshot_id = c.target_snapshot_id
                      AND r.provenance_comparison_id = c.comparison_id
                    WHERE c.operation_id = ?1 AND c.state = 'ready'
                      AND r.state = 'ready'"#,
                [operation_id.as_slice()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let (watch_id, sequence, snapshot_id, revision_id, comparison_id) =
            row.ok_or_else(|| ManagerError::new("fulfilled cut is not a ready indexed result"))?;
        let mut statement = self.connection().prepare(
            r#"SELECT event_kind, ino, old_generation, new_generation,
                      change_mask, old_path, new_path
                 FROM change_events
                WHERE comparison_id = ?1 ORDER BY ordinal"#,
        )?;
        let events = statement
            .query_map([comparison_id], |row| {
                let kind: String = row.get(0)?;
                let ino: Vec<u8> = row.get(1)?;
                let old_generation: Option<Vec<u8>> = row.get(2)?;
                let new_generation: Option<Vec<u8>> = row.get(3)?;
                let change_mask: i64 = row.get(4)?;
                Ok((
                    kind,
                    ino,
                    old_generation,
                    new_generation,
                    change_mask,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(
                |(kind, ino, old_generation, new_generation, change_mask, old_path, new_path)| {
                    Ok(Event {
                        kind: parse_event_kind(&kind)?,
                        ino: decode_u64(&ino)?,
                        old_generation: old_generation.as_deref().map(decode_u64).transpose()?,
                        new_generation: new_generation.as_deref().map(decode_u64).transpose()?,
                        change_mask: u64::try_from(change_mask)
                            .map_err(|_| ManagerError::new("stored event mask is negative"))?,
                        old_path,
                        new_path,
                    })
                },
            )
            .collect::<Result<Vec<_>, ManagerError>>()?;
        Ok(PublishedCut {
            watch_id: fixed_manager_blob(&watch_id, "published cut watch ID")?,
            sequence,
            snapshot_id,
            revision_id,
            comparison_id,
            events,
        })
    }

    pub fn claim_historical_comparison(
        &mut self,
        request: &HistoricalComparisonRequest,
    ) -> Result<HistoricalComparisonAdmission, ManagerError> {
        let watch_id = request.watch_id;
        let authorization_id = request.authorization_id;
        let requester_uid = request.requester_uid;
        let from_snapshot_uuid = request.from_snapshot_uuid;
        let to_snapshot_uuid = request.to_snapshot_uuid;
        let lease_owner = request.lease_owner;
        let now_ns = request.now_ns;
        let lease_expires_ns = request.lease_expires_ns;
        if lease_expires_ns <= now_ns {
            return Err(ManagerError::new(
                "historical comparison lease must expire after admission",
            ));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorized: Option<i64> = transaction
            .query_row(
                r#"SELECT 1 FROM watches w
                     JOIN watch_grants g ON g.watch_id = w.id
                    WHERE w.id = ?1 AND w.state IN ('active', 'blocked')
                      AND g.id = ?2 AND g.state = 'active'
                      AND (g.permissions & ?3) = ?3
                      AND g.principal_kind = 'uid' AND g.principal_id = ?4"#,
                params![
                    watch_id.as_slice(),
                    authorization_id.as_slice(),
                    i64::from(PERMISSION_READ),
                    encode_u64(u64::from(requester_uid)).as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if authorized != Some(1) {
            return Err(ManagerError::new(
                "historical comparison authorization is absent or stale",
            ));
        }
        let from_sequence =
            historical_snapshot_sequence(&transaction, watch_id, from_snapshot_uuid)?.ok_or_else(
                || ManagerError::new("source snapshot is not retained on this watch"),
            )?;
        let to_sequence = historical_snapshot_sequence(&transaction, watch_id, to_snapshot_uuid)?
            .ok_or_else(|| {
            ManagerError::new("target snapshot is not retained on this watch")
        })?;
        if from_sequence > to_sequence {
            return Err(ManagerError::new(
                "historical comparison source is newer than its target",
            ));
        }
        if from_sequence == to_sequence {
            transaction.commit()?;
            return Ok(HistoricalComparisonAdmission::Ready(HistoricalChanges {
                watch_id,
                from_snapshot_uuid,
                to_snapshot_uuid,
                from_sequence,
                to_sequence,
                fresh_instance: false,
                events: Vec::new(),
            }));
        }
        let (from_snapshot_id, from_revision_id): (i64, i64) = transaction
            .query_row(
                r#"SELECT s.id, r.id FROM snapshots s
                     JOIN revisions r ON r.snapshot_id = s.id AND r.state = 'ready'
                    WHERE s.subvol_uuid = ?1 AND s.physical_state = 'present'
                      AND s.filesystem_id = (
                          SELECT filesystem_id FROM watches WHERE id = ?2
                      )"#,
                params![from_snapshot_uuid.as_slice(), watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| ManagerError::new("source snapshot revision is not retained"))?;
        let (to_snapshot_id, to_revision_id): (i64, i64) = transaction
            .query_row(
                r#"SELECT s.id, r.id FROM snapshots s
                     JOIN revisions r ON r.snapshot_id = s.id AND r.state = 'ready'
                    WHERE s.subvol_uuid = ?1 AND s.physical_state = 'present'
                      AND s.filesystem_id = (
                          SELECT filesystem_id FROM watches WHERE id = ?2
                      )"#,
                params![to_snapshot_uuid.as_slice(), watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| ManagerError::new("target snapshot revision is not retained"))?;
        transaction.execute(
            r#"INSERT INTO comparisons(
                   from_snapshot_id, to_snapshot_id, comparison_kind,
                   algorithm_version, state, lease_owner, lease_fence,
                   lease_expires_ns, manifest_hash, raw_ref_adds, raw_ref_deletes)
               VALUES (?1, ?2, 'incremental', 2, 'claimed', ?3, 1, ?4,
                       NULL, NULL, NULL)
               ON CONFLICT(from_snapshot_id, to_snapshot_id,
                           comparison_kind, algorithm_version) DO NOTHING"#,
            params![
                from_snapshot_id,
                to_snapshot_id,
                lease_owner.as_slice(),
                lease_expires_ns,
            ],
        )?;
        let row: (i64, String, Option<Vec<u8>>, i64, Option<i64>) = transaction.query_row(
            r#"SELECT id, state, lease_owner, lease_fence, lease_expires_ns
                 FROM comparisons
                WHERE from_snapshot_id = ?1 AND to_snapshot_id = ?2
                  AND comparison_kind = 'incremental' AND algorithm_version = 2"#,
            params![from_snapshot_id, to_snapshot_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let (comparison_id, state, current_owner, old_fence, old_expiry) = row;
        if state == "index_ready" {
            let events = load_comparison_events(&transaction, comparison_id)?;
            transaction.commit()?;
            return Ok(HistoricalComparisonAdmission::Ready(HistoricalChanges {
                watch_id,
                from_snapshot_uuid,
                to_snapshot_uuid,
                from_sequence,
                to_sequence,
                fresh_instance: false,
                events,
            }));
        }
        let already_owned = current_owner.as_deref() == Some(lease_owner.as_slice());
        if !already_owned && old_expiry.is_some_and(|expiry| expiry > now_ns) {
            return Err(ManagerError::new("historical comparison job is busy"));
        }
        let lease_fence = if already_owned && state == "claimed" {
            old_fence
        } else {
            old_fence
                .checked_add(1)
                .ok_or_else(|| ManagerError::new("historical comparison fence overflow"))?
        };
        if !already_owned || state != "claimed" {
            require_one(
                transaction.execute(
                    r#"UPDATE comparisons
                          SET state = 'claimed', lease_owner = ?2,
                              lease_fence = ?3, lease_expires_ns = ?4,
                              manifest_hash = NULL, raw_ref_adds = NULL,
                              raw_ref_deletes = NULL
                        WHERE id = ?1 AND state != 'index_ready'
                          AND (lease_owner IS NULL OR lease_expires_ns <= ?5
                               OR lease_owner = ?2)"#,
                    params![
                        comparison_id,
                        lease_owner.as_slice(),
                        lease_fence,
                        lease_expires_ns,
                        now_ns,
                    ],
                )?,
                "claim historical comparison",
            )?;
        }
        let comparison_owner = encode_u64(
            u64::try_from(comparison_id)
                .map_err(|_| ManagerError::new("comparison ID is negative"))?,
        );
        for (snapshot_id, reason) in [
            (from_snapshot_id, "historical-comparison-source"),
            (to_snapshot_id, "historical-comparison-target"),
        ] {
            transaction.execute(
                r#"INSERT OR IGNORE INTO snapshot_pins(
                       snapshot_id, owner_kind, owner_id, reason)
                   VALUES (?1, 'comparison', ?2, ?3)"#,
                params![snapshot_id, comparison_owner.as_slice(), reason],
            )?;
        }
        transaction.commit()?;
        Ok(HistoricalComparisonAdmission::Claimed(
            HistoricalComparisonClaim {
                comparison_id,
                watch_id,
                authorization_id,
                from_snapshot_uuid,
                to_snapshot_uuid,
                from_snapshot_id,
                to_snapshot_id,
                from_revision_id,
                to_revision_id,
                from_sequence,
                to_sequence,
                lease_owner,
                lease_fence,
            },
        ))
    }

    pub fn publish_historical_comparison(
        &mut self,
        claim: &HistoricalComparisonClaim,
        manifest: &ChangedObjectsManifest,
        target_objects: &BTreeMap<u64, Object>,
        now_ns: i64,
    ) -> Result<HistoricalChanges, ManagerError> {
        let base = self.load_revision(claim.from_revision_id)?;
        let expected_target = self.load_revision(claim.to_revision_id)?;
        let applied = apply_manifest(&base, manifest, target_objects)
            .map_err(|error| ManagerError::new(format!("apply historical manifest: {error}")))?;
        if applied.index != expected_target {
            return Err(ManagerError::new(
                "historical kernel delta does not reproduce the indexed target",
            ));
        }
        stage_delta_rows(
            self.connection_mut(),
            manifest,
            &applied.events,
            &applied.index,
            &BTreeMap::new(),
        )?;
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: Option<i64> = transaction
            .query_row(
                r#"SELECT 1 FROM comparisons c
                     JOIN snapshots a ON a.id = c.from_snapshot_id
                     JOIN snapshots b ON b.id = c.to_snapshot_id
                     JOIN revisions ra ON ra.id = ?7 AND ra.snapshot_id = a.id
                     JOIN revisions rb ON rb.id = ?8 AND rb.snapshot_id = b.id
                     JOIN watch_grants g ON g.id = ?9 AND g.watch_id = ?10
                    WHERE c.id = ?1 AND c.state = 'claimed'
                      AND c.lease_owner = ?2 AND c.lease_fence = ?3
                      AND c.from_snapshot_id = ?4 AND c.to_snapshot_id = ?5
                      AND c.algorithm_version = 2 AND c.comparison_kind = 'incremental'
                      AND c.lease_expires_ns > ?6
                      AND a.physical_state = 'present' AND b.physical_state = 'present'
                      AND ra.state = 'ready' AND rb.state = 'ready'
                      AND g.state = 'active' AND (g.permissions & ?11) = ?11"#,
                params![
                    claim.comparison_id,
                    claim.lease_owner.as_slice(),
                    claim.lease_fence,
                    claim.from_snapshot_id,
                    claim.to_snapshot_id,
                    now_ns,
                    claim.from_revision_id,
                    claim.to_revision_id,
                    claim.authorization_id.as_slice(),
                    claim.watch_id.as_slice(),
                    i64::from(PERMISSION_READ),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if valid != Some(1) {
            return Err(ManagerError::new(
                "historical comparison publication fence is stale",
            ));
        }
        import_staged_comparison_rows(&transaction, claim.comparison_id)?;
        require_one(
            transaction.execute(
                r#"UPDATE comparisons
                      SET state = 'index_ready', lease_owner = NULL,
                          lease_expires_ns = NULL, manifest_hash = ?4,
                          raw_ref_adds = ?5, raw_ref_deletes = ?6
                    WHERE id = ?1 AND state = 'claimed'
                      AND lease_owner = ?2 AND lease_fence = ?3"#,
                params![
                    claim.comparison_id,
                    claim.lease_owner.as_slice(),
                    claim.lease_fence,
                    manifest.canonical_hash().as_slice(),
                    i64::try_from(manifest.raw_ref_adds)
                        .map_err(|_| ManagerError::new("raw ref add count overflow"))?,
                    i64::try_from(manifest.raw_ref_deletes)
                        .map_err(|_| ManagerError::new("raw ref delete count overflow"))?,
                ],
            )?,
            "publish historical comparison",
        )?;
        let comparison_owner = encode_u64(
            u64::try_from(claim.comparison_id)
                .map_err(|_| ManagerError::new("comparison ID is negative"))?,
        );
        transaction.execute(
            "DELETE FROM snapshot_pins WHERE owner_kind = 'comparison' AND owner_id = ?1",
            [comparison_owner.as_slice()],
        )?;
        transaction.commit()?;
        let _ = clear_staged_delta(self.connection_mut());
        Ok(HistoricalChanges {
            watch_id: claim.watch_id,
            from_snapshot_uuid: claim.from_snapshot_uuid,
            to_snapshot_uuid: claim.to_snapshot_uuid,
            from_sequence: claim.from_sequence,
            to_sequence: claim.to_sequence,
            fresh_instance: false,
            events: applied.events,
        })
    }

    pub fn replay_historical_changes(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        requester_uid: u32,
        from_snapshot_uuid: [u8; 16],
        to_snapshot_uuid: [u8; 16],
    ) -> Result<HistoricalChanges, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let authorized: Option<i64> = transaction
            .query_row(
                r#"SELECT 1 FROM watches w
                     JOIN watch_grants g ON g.watch_id = w.id
                    WHERE w.id = ?1 AND w.state IN ('active', 'blocked')
                      AND g.id = ?2 AND g.state = 'active'
                      AND (g.permissions & ?3) = ?3
                      AND g.principal_kind = 'uid' AND g.principal_id = ?4"#,
                params![
                    watch_id.as_slice(),
                    authorization_id.as_slice(),
                    i64::from(PERMISSION_READ),
                    encode_u64(u64::from(requester_uid)).as_slice(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if authorized != Some(1) {
            return Err(ManagerError::new(
                "historical replay authorization is absent or stale",
            ));
        }
        let from_sequence =
            historical_snapshot_sequence(&transaction, watch_id, from_snapshot_uuid)?.ok_or_else(
                || ManagerError::new("source snapshot is not a cut on this watch branch"),
            )?;
        let to_sequence = historical_snapshot_sequence(&transaction, watch_id, to_snapshot_uuid)?
            .ok_or_else(|| {
            ManagerError::new("target snapshot is not a cut on this watch branch")
        })?;
        if from_sequence > to_sequence {
            return Err(ManagerError::new(
                "historical replay source is newer than its target",
            ));
        }
        let replay_floor: i64 = transaction.query_row(
            "SELECT replay_floor_seq FROM watches WHERE id = ?1",
            [watch_id.as_slice()],
            |row| row.get(0),
        )?;
        let expected = to_sequence - from_sequence;
        let ready_count: i64 = transaction.query_row(
            r#"SELECT count(*) FROM watch_cuts
                WHERE watch_id = ?1 AND sequence > ?2 AND sequence <= ?3
                  AND state = 'ready' AND fresh_instance = 0
                  AND comparison_id IS NOT NULL"#,
            params![watch_id.as_slice(), from_sequence, to_sequence],
            |row| row.get(0),
        )?;
        let fresh_instance = from_sequence < replay_floor || ready_count != expected;
        let events = if fresh_instance {
            Vec::new()
        } else {
            load_historical_events(&transaction, watch_id, from_sequence, to_sequence)?
        };
        transaction.commit()?;
        Ok(HistoricalChanges {
            watch_id,
            from_snapshot_uuid,
            to_snapshot_uuid,
            from_sequence,
            to_sequence,
            fresh_instance,
            events,
        })
    }

    pub fn load_revision(&self, revision_id: i64) -> Result<Index, ManagerError> {
        let chain = self.revision_storage_chain(revision_id)?;
        let checkpoint_id = *chain.last().expect("chain always contains target");
        let mut index = self.load_checkpoint(checkpoint_id)?;
        for &overlay_revision in chain.iter().rev().skip(1) {
            apply_stored_overrides(self.connection(), overlay_revision, &mut index)?;
            index.validate().map_err(|error| {
                ManagerError::new(format!(
                    "stored revision {overlay_revision} is invalid: {error}"
                ))
            })?;
        }
        Ok(index)
    }

    fn revision_storage_chain(&self, revision_id: i64) -> Result<Vec<i64>, ManagerError> {
        let mut chain = Vec::new();
        let mut current = revision_id;
        loop {
            if chain.len() > 32 {
                return Err(ManagerError::new(
                    "revision overlay chain exceeds the maximum depth of 32",
                ));
            }
            let row: Option<(Option<i64>, String)> = self
                .connection()
                .query_row(
                    "SELECT storage_base_revision_id, state FROM revisions WHERE id = ?1",
                    [current],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (base, state) =
                row.ok_or_else(|| ManagerError::new(format!("revision {current} does not exist")))?;
            if state != "ready" {
                return Err(ManagerError::new(format!(
                    "revision {current} is not ready"
                )));
            }
            chain.push(current);
            match base {
                Some(base) => current = base,
                None => return Ok(chain),
            }
        }
    }

    fn load_revision_object_from_chain(
        &self,
        chain: &[i64],
        ino: u64,
    ) -> Result<Option<Object>, ManagerError> {
        let encoded = encode_u64(ino);
        for &revision_id in &chain[..chain.len().saturating_sub(1)] {
            let row = self
                .connection()
                .query_row(
                    r#"SELECT present, generation, mode, nlink, uid, gid, rdev,
                              privilege_flags, security_xattr_hash
                         FROM object_overrides
                        WHERE revision_id = ?1 AND ino = ?2"#,
                    params![revision_id, encoded.as_slice()],
                    decode_optional_object_row,
                )
                .optional()?;
            if let Some(object) = row {
                return Ok(object.map(|mut object| {
                    object.ino = ino;
                    object
                }));
            }
        }
        let checkpoint_id = *chain
            .last()
            .ok_or_else(|| ManagerError::new("empty revision storage chain"))?;
        self.connection()
            .query_row(
                r#"SELECT generation, mode, nlink, uid, gid, rdev,
                          privilege_flags, security_xattr_hash
                     FROM checkpoint_objects
                    WHERE revision_id = ?1 AND ino = ?2"#,
                params![checkpoint_id, encoded.as_slice()],
                |row| decode_checkpoint_object_row(row, ino),
            )
            .optional()
            .map_err(ManagerError::from)
    }

    fn load_revision_refs_for_inode_from_chain(
        &self,
        chain: &[i64],
        ino: u64,
    ) -> Result<BTreeSet<Reference>, ManagerError> {
        let checkpoint_id = *chain
            .last()
            .ok_or_else(|| ManagerError::new("empty revision storage chain"))?;
        let encoded = encode_u64(ino);
        let mut references = BTreeSet::new();
        let mut statement = self.connection().prepare(
            "SELECT parent_ino, name FROM checkpoint_refs \
             WHERE revision_id = ?1 AND ino = ?2 ORDER BY parent_ino, name",
        )?;
        let rows = statement.query_map(params![checkpoint_id, encoded.as_slice()], |row| {
            Ok(Reference {
                ino,
                parent_ino: decode_sql_u64(row.get_ref(0)?.as_blob()?)?,
                name: row.get(1)?,
            })
        })?;
        for row in rows {
            references.insert(row?);
        }
        drop(statement);
        for &revision_id in chain[..chain.len().saturating_sub(1)].iter().rev() {
            let mut statement = self.connection().prepare(
                "SELECT parent_ino, name, present FROM ref_overrides \
                 WHERE revision_id = ?1 AND ino = ?2 ORDER BY parent_ino, name",
            )?;
            let rows = statement.query_map(params![revision_id, encoded.as_slice()], |row| {
                Ok((
                    Reference {
                        ino,
                        parent_ino: decode_sql_u64(row.get_ref(0)?.as_blob()?)?,
                        name: row.get(1)?,
                    },
                    row.get::<_, bool>(2)?,
                ))
            })?;
            for row in rows {
                let (reference, present) = row?;
                if present {
                    references.insert(reference);
                } else {
                    references.remove(&reference);
                }
            }
        }
        Ok(references)
    }

    fn load_revision_refs_for_name_from_chain(
        &self,
        chain: &[i64],
        parent_ino: u64,
        name: &[u8],
    ) -> Result<BTreeSet<Reference>, ManagerError> {
        let checkpoint_id = *chain
            .last()
            .ok_or_else(|| ManagerError::new("empty revision storage chain"))?;
        let encoded_parent = encode_u64(parent_ino);
        let mut candidates = BTreeSet::new();
        let mut statement = self.connection().prepare(
            "SELECT ino FROM checkpoint_refs \
             WHERE revision_id = ?1 AND parent_ino = ?2 AND name = ?3",
        )?;
        let rows = statement.query_map(
            params![checkpoint_id, encoded_parent.as_slice(), name],
            |row| {
                Ok(Reference {
                    ino: decode_sql_u64(row.get_ref(0)?.as_blob()?)?,
                    parent_ino,
                    name: name.to_vec(),
                })
            },
        )?;
        for row in rows {
            candidates.insert(row?);
        }
        drop(statement);
        for &revision_id in chain[..chain.len().saturating_sub(1)].iter().rev() {
            let mut statement = self.connection().prepare(
                "SELECT ino, present FROM ref_overrides \
                 WHERE revision_id = ?1 AND parent_ino = ?2 AND name = ?3",
            )?;
            let rows = statement.query_map(
                params![revision_id, encoded_parent.as_slice(), name],
                |row| {
                    Ok((
                        Reference {
                            ino: decode_sql_u64(row.get_ref(0)?.as_blob()?)?,
                            parent_ino,
                            name: name.to_vec(),
                        },
                        row.get::<_, bool>(1)?,
                    ))
                },
            )?;
            for row in rows {
                let (reference, present) = row?;
                if present {
                    candidates.insert(reference);
                } else {
                    candidates.remove(&reference);
                }
            }
        }
        Ok(candidates)
    }

    fn load_revision_owner_count_from_chain(
        &self,
        chain: &[i64],
        uid: u64,
    ) -> Result<i64, ManagerError> {
        let encoded = encode_u64(uid);
        for &revision_id in &chain[..chain.len().saturating_sub(1)] {
            let count = self
                .connection()
                .query_row(
                    "SELECT object_count FROM owner_count_overrides \
                     WHERE revision_id = ?1 AND uid = ?2",
                    params![revision_id, encoded.as_slice()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(count) = count {
                return Ok(count);
            }
        }
        let checkpoint_id = *chain
            .last()
            .ok_or_else(|| ManagerError::new("empty revision storage chain"))?;
        Ok(self
            .connection()
            .query_row(
                "SELECT object_count FROM checkpoint_owner_counts \
                 WHERE revision_id = ?1 AND uid = ?2",
                params![checkpoint_id, encoded.as_slice()],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    fn load_revision_delta_subset(
        &self,
        revision_id: i64,
        manifest: &ChangedObjectsManifest,
    ) -> Result<Index, ManagerError> {
        let chain = self.revision_storage_chain(revision_id)?;
        let mut pending = VecDeque::new();
        let mut queued = BTreeSet::new();
        let queue = |ino: u64, pending: &mut VecDeque<u64>, queued: &mut BTreeSet<u64>| {
            if queued.insert(ino) {
                pending.push_back(ino);
            }
        };
        queue(ROOT_INO, &mut pending, &mut queued);
        for &ino in manifest.objects.keys() {
            queue(ino, &mut pending, &mut queued);
        }
        for reference in manifest.ref_adds.iter().chain(&manifest.ref_deletes) {
            queue(reference.ino, &mut pending, &mut queued);
            queue(reference.parent_ino, &mut pending, &mut queued);
        }
        for reference in &manifest.ref_adds {
            for existing in self.load_revision_refs_for_name_from_chain(
                &chain,
                reference.parent_ino,
                &reference.name,
            )? {
                queue(existing.ino, &mut pending, &mut queued);
            }
        }

        let mut subset = Index::default();
        while let Some(ino) = pending.pop_front() {
            if let Some(object) = self.load_revision_object_from_chain(&chain, ino)? {
                subset.objects.insert(ino, object);
            }
            for reference in self.load_revision_refs_for_inode_from_chain(&chain, ino)? {
                queue(reference.parent_ino, &mut pending, &mut queued);
                subset.references.insert(reference);
            }
        }
        subset.validate().map_err(|error| {
            ManagerError::new(format!(
                "changed-object revision subset is invalid: {error}"
            ))
        })?;
        Ok(subset)
    }

    fn load_revision_metadata(&self, revision_id: i64) -> Result<RevisionMetadata, ManagerError> {
        self.connection()
            .query_row(
                r#"SELECT object_count, ref_count, state_hash, single_owner_uid,
                          privileged_metadata_count, security_state_hash,
                          summary_version, owner_cardinality, owner_uid_xor
                     FROM revisions WHERE id = ?1 AND state = 'ready'"#,
                [revision_id],
                |row| {
                    let state_hash: Option<Vec<u8>> = row.get(2)?;
                    let owner: Option<Vec<u8>> = row.get(3)?;
                    let security_state_hash: Option<Vec<u8>> = row.get(5)?;
                    let summary_version: i64 = row.get(6)?;
                    let owner_uid_xor: Option<Vec<u8>> = row.get(8)?;
                    Ok(RevisionMetadata {
                        summary_version,
                        owner_cardinality: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                        owner_uid_xor: owner_uid_xor
                            .as_deref()
                            .map(decode_sql_u64)
                            .transpose()?
                            .unwrap_or(0),
                        object_count: row.get(0)?,
                        ref_count: row.get(1)?,
                        state_hash: state_hash
                            .as_deref()
                            .map(fixed_sql_blob)
                            .transpose()?
                            .unwrap_or([0; 32]),
                        single_owner_uid: owner.as_deref().map(decode_sql_u64).transpose()?,
                        privileged_metadata_count: row.get(4)?,
                        security_state_hash: security_state_hash
                            .as_deref()
                            .map(fixed_sql_blob)
                            .transpose()?
                            .unwrap_or([0; 32]),
                    })
                },
            )
            .map_err(ManagerError::from)
    }

    pub fn load_checkpoint(&self, revision_id: i64) -> Result<Index, ManagerError> {
        let ready: Option<i64> = self
            .connection()
            .query_row(
                "SELECT r.id \
                   FROM revisions r JOIN revision_checkpoints c \
                     ON c.revision_id = r.id \
                  WHERE r.id = ?1 AND r.state = 'ready' AND c.state = 'ready'",
                [revision_id],
                |row| row.get(0),
            )
            .optional()?;
        if ready.is_none() {
            return Err(ManagerError::new(format!(
                "revision {revision_id} has no ready checkpoint"
            )));
        }
        let mut index = Index::default();
        let mut objects = self.connection().prepare(
            "SELECT ino, generation, mode, nlink, uid, gid, rdev, \
                    privilege_flags, security_xattr_hash \
               FROM checkpoint_objects WHERE revision_id = ?1 ORDER BY ino",
        )?;
        let rows = objects.query_map([revision_id], |row| {
            Ok(Object {
                ino: decode_sql_u64(row.get_ref(0)?.as_blob()?)?,
                generation: decode_sql_u64(row.get_ref(1)?.as_blob()?)?,
                mode: row.get(2)?,
                nlink: row.get(3)?,
                uid: decode_sql_u64(row.get_ref(4)?.as_blob()?)?,
                gid: decode_sql_u64(row.get_ref(5)?.as_blob()?)?,
                rdev: decode_sql_u64(row.get_ref(6)?.as_blob()?)?,
                privilege_flags: row.get(7)?,
                security_xattr_hash: fixed_sql_blob(row.get_ref(8)?.as_blob()?)?,
            })
        })?;
        for object in rows {
            let object = object?;
            if index.objects.insert(object.ino, object).is_some() {
                return Err(ManagerError::new("checkpoint contains duplicate object"));
            }
        }
        let mut references = self.connection().prepare(
            "SELECT ino, parent_ino, name FROM checkpoint_refs \
              WHERE revision_id = ?1 ORDER BY ino, parent_ino, name",
        )?;
        let rows = references.query_map([revision_id], |row| {
            Ok(Reference {
                ino: decode_sql_u64(row.get_ref(0)?.as_blob()?)?,
                parent_ino: decode_sql_u64(row.get_ref(1)?.as_blob()?)?,
                name: row.get(2)?,
            })
        })?;
        for reference in rows {
            if !index.references.insert(reference?) {
                return Err(ManagerError::new("checkpoint contains duplicate reference"));
            }
        }
        index
            .validate()
            .map_err(|error| ManagerError::new(format!("stored checkpoint is invalid: {error}")))?;
        Ok(index)
    }
}

fn full_fresh_events(index: &Index) -> Result<Vec<Event>, ManagerError> {
    let mut events = Vec::new();
    for object in index.objects.values() {
        let paths = index.paths(object.ino).map_err(|error| {
            ManagerError::new(format!("resolve full-fresh inode paths: {error}"))
        })?;
        for path in paths {
            events.push(Event {
                kind: EventKind::PathAdded,
                ino: object.ino,
                old_generation: None,
                new_generation: Some(object.generation),
                change_mask: CHANGE_CREATED | CHANGE_INODE | CHANGE_REF,
                old_path: None,
                new_path: Some(path),
            });
        }
    }
    Ok(events)
}

fn historical_snapshot_sequence(
    transaction: &Transaction<'_>,
    watch_id: [u8; 16],
    snapshot_uuid: [u8; 16],
) -> Result<Option<i64>, ManagerError> {
    let (minimum, maximum): (Option<i64>, Option<i64>) = transaction.query_row(
        r#"SELECT min(candidate.sequence), max(candidate.sequence)
                 FROM (
                     SELECT c.sequence AS sequence
                       FROM watch_cuts c
                       JOIN snapshots s ON s.id = c.target_snapshot_id
                      WHERE c.watch_id = ?1 AND s.subvol_uuid = ?2
                     UNION ALL
                     SELECT c.sequence - 1 AS sequence
                       FROM watch_cuts c
                       JOIN snapshots s ON s.id = c.base_snapshot_id
                      WHERE c.watch_id = ?1 AND s.subvol_uuid = ?2
                     UNION ALL
                     SELECT w.indexed_seq AS sequence
                       FROM watches w
                       JOIN snapshots s ON s.id = w.last_cut_snapshot_id
                      WHERE w.id = ?1 AND s.subvol_uuid = ?2
                        AND NOT EXISTS (
                            SELECT 1 FROM watch_cuts c WHERE c.watch_id = w.id
                        )
                 ) AS candidate"#,
        params![watch_id.as_slice(), snapshot_uuid.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (Some(minimum), Some(maximum)) = (minimum, maximum) else {
        return Ok(None);
    };
    if minimum != maximum {
        return Err(ManagerError::new(
            "snapshot UUID maps to inconsistent sequences on one watch branch",
        ));
    }
    Ok(Some(minimum))
}

fn load_historical_events(
    transaction: &Transaction<'_>,
    watch_id: [u8; 16],
    from_sequence: i64,
    to_sequence: i64,
) -> Result<Vec<Event>, ManagerError> {
    let mut statement = transaction.prepare(
        r#"SELECT e.event_kind, e.ino, e.old_generation, e.new_generation,
                  e.change_mask, e.old_path, e.new_path
             FROM watch_cuts c
             JOIN change_events e ON e.comparison_id = c.comparison_id
            WHERE c.watch_id = ?1 AND c.sequence > ?2 AND c.sequence <= ?3
              AND c.state = 'ready' AND c.fresh_instance = 0
            ORDER BY c.sequence, e.ordinal"#,
    )?;
    let stored = statement
        .query_map(
            params![watch_id.as_slice(), from_sequence, to_sequence],
            |row| {
                let kind: String = row.get(0)?;
                let ino: Vec<u8> = row.get(1)?;
                let old_generation: Option<Vec<u8>> = row.get(2)?;
                let new_generation: Option<Vec<u8>> = row.get(3)?;
                let change_mask: i64 = row.get(4)?;
                Ok((
                    kind,
                    ino,
                    old_generation,
                    new_generation,
                    change_mask,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    stored
        .into_iter()
        .map(
            |(kind, ino, old_generation, new_generation, change_mask, old_path, new_path)| {
                Ok(Event {
                    kind: parse_event_kind(&kind)?,
                    ino: decode_u64(&ino)?,
                    old_generation: old_generation.as_deref().map(decode_u64).transpose()?,
                    new_generation: new_generation.as_deref().map(decode_u64).transpose()?,
                    change_mask: u64::try_from(change_mask)
                        .map_err(|_| ManagerError::new("stored event mask is negative"))?,
                    old_path,
                    new_path,
                })
            },
        )
        .collect()
}

fn load_comparison_events(
    transaction: &Transaction<'_>,
    comparison_id: i64,
) -> Result<Vec<Event>, ManagerError> {
    let mut statement = transaction.prepare(
        r#"SELECT event_kind, ino, old_generation, new_generation,
                  change_mask, old_path, new_path
             FROM change_events
            WHERE comparison_id = ?1 ORDER BY ordinal"#,
    )?;
    let stored = statement
        .query_map([comparison_id], |row| {
            let kind: String = row.get(0)?;
            let ino: Vec<u8> = row.get(1)?;
            let old_generation: Option<Vec<u8>> = row.get(2)?;
            let new_generation: Option<Vec<u8>> = row.get(3)?;
            let change_mask: i64 = row.get(4)?;
            Ok((
                kind,
                ino,
                old_generation,
                new_generation,
                change_mask,
                row.get(5)?,
                row.get(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    stored
        .into_iter()
        .map(
            |(kind, ino, old_generation, new_generation, change_mask, old_path, new_path)| {
                Ok(Event {
                    kind: parse_event_kind(&kind)?,
                    ino: decode_u64(&ino)?,
                    old_generation: old_generation.as_deref().map(decode_u64).transpose()?,
                    new_generation: new_generation.as_deref().map(decode_u64).transpose()?,
                    change_mask: u64::try_from(change_mask)
                        .map_err(|_| ManagerError::new("stored event mask is negative"))?,
                    old_path,
                    new_path,
                })
            },
        )
        .collect()
}

fn insert_snapshot(
    transaction: &Transaction<'_>,
    filesystem_id: i64,
    snapshot: &SnapshotIdentity,
) -> Result<i64, ManagerError> {
    transaction.execute(
        r#"INSERT INTO snapshots(
               filesystem_id, subvol_uuid, parent_uuid, received_uuid,
               root_id, ctransid, otransid, path, readonly, physical_state,
               created_ns, deleted_ns
           ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, 'present', ?9, NULL)"#,
        params![
            filesystem_id,
            snapshot.subvol_uuid.as_slice(),
            snapshot.parent_uuid.as_ref().map(<[u8; 16]>::as_slice),
            snapshot.received_uuid.as_ref().map(<[u8; 16]>::as_slice),
            encode_u64(snapshot.root_id).as_slice(),
            encode_u64(snapshot.ctransid).as_slice(),
            encode_u64(snapshot.otransid).as_slice(),
            snapshot.path,
            snapshot.created_ns,
        ],
    )?;
    Ok(transaction.last_insert_rowid())
}

fn verify_cut_snapshot(
    transaction: &Transaction<'_>,
    reservation: &CutReservation,
    lease_owner: [u8; 16],
    snapshot_id: i64,
    snapshot_uuid: [u8; 16],
) -> Result<(), ManagerError> {
    let valid: Option<i64> = transaction
        .query_row(
            r#"SELECT 1
                 FROM operations o
                 JOIN snapshots s ON s.id = ?7 AND s.filesystem_id = o.filesystem_id
                 JOIN watches w ON w.id = o.watch_id
                WHERE o.id = ?1 AND o.watch_id = ?2 AND o.sequence = ?3
                  AND o.state = 'uuid_recorded' AND o.lease_owner = ?4
                  AND o.lease_fence = ?5 AND o.discovered_uuid = ?6
                  AND s.subvol_uuid = ?6 AND s.physical_state = 'present'
                  AND s.readonly = 1 AND w.cut_owner = ?4 AND w.cut_fence = ?8"#,
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice(),
                reservation.sequence,
                lease_owner.as_slice(),
                reservation.operation_fence,
                snapshot_uuid.as_slice(),
                snapshot_id,
                reservation.cut_fence,
            ],
            |row| row.get(0),
        )
        .optional()?;
    if valid != Some(1) {
        return Err(ManagerError::new(
            "physical cut publication fence or identity is stale",
        ));
    }
    Ok(())
}

fn verify_delta_publication(
    transaction: &Transaction<'_>,
    reservation: &CutReservation,
    lease_owner: [u8; 16],
    snapshot_id: i64,
    base_revision_id: i64,
) -> Result<(), ManagerError> {
    let valid: Option<i64> = transaction
        .query_row(
            r#"SELECT 1
                 FROM operations o
                 JOIN watches w ON w.id = o.watch_id
                 JOIN watch_cuts c ON c.watch_id = o.watch_id
                                  AND c.sequence = o.sequence
                 JOIN snapshots s ON s.id = c.target_snapshot_id
                WHERE o.id = ?1 AND o.watch_id = ?2 AND o.sequence = ?3
                  AND o.state = 'manifest_ready' AND o.lease_owner = ?4
                  AND o.lease_fence = ?5 AND c.state = 'created'
                  AND c.target_snapshot_id = ?6 AND s.physical_state = 'present'
                  AND w.indexed_revision_id = ?7 AND w.indexed_seq = ?8"#,
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice(),
                reservation.sequence,
                lease_owner.as_slice(),
                reservation.operation_fence,
                snapshot_id,
                base_revision_id,
                reservation.sequence - 1,
            ],
            |row| row.get(0),
        )
        .optional()?;
    if valid != Some(1) {
        return Err(ManagerError::new(
            "delta publication fence, predecessor, or snapshot is stale",
        ));
    }
    Ok(())
}

fn stage_delta_rows(
    connection: &mut rusqlite::Connection,
    manifest: &ChangedObjectsManifest,
    events: &[Event],
    target: &Index,
    owner_counts: &BTreeMap<u64, i64>,
) -> Result<(), ManagerError> {
    connection.execute_batch(
        r#"CREATE TEMP TABLE IF NOT EXISTS delta_stage_objects (
               ino BLOB PRIMARY KEY, old_generation BLOB,
               new_generation BLOB, change_mask INTEGER NOT NULL
           ) WITHOUT ROWID;
           CREATE TEMP TABLE IF NOT EXISTS delta_stage_refs (
               operation INTEGER NOT NULL, ino BLOB NOT NULL,
               parent_ino BLOB NOT NULL, name BLOB NOT NULL,
               PRIMARY KEY (operation, ino, parent_ino, name)
           ) WITHOUT ROWID;
           CREATE TEMP TABLE IF NOT EXISTS delta_stage_events (
               ordinal INTEGER PRIMARY KEY, event_kind TEXT NOT NULL,
               ino BLOB NOT NULL, old_generation BLOB, new_generation BLOB,
               change_mask INTEGER NOT NULL, old_path BLOB, new_path BLOB
           ) WITHOUT ROWID;
           CREATE TEMP TABLE IF NOT EXISTS delta_stage_object_overrides (
               ino BLOB PRIMARY KEY, present INTEGER NOT NULL,
               generation BLOB, mode INTEGER, nlink INTEGER, uid BLOB,
               gid BLOB, rdev BLOB, privilege_flags INTEGER,
               security_xattr_hash BLOB
           ) WITHOUT ROWID;
           CREATE TEMP TABLE IF NOT EXISTS delta_stage_ref_overrides (
               ino BLOB NOT NULL, parent_ino BLOB NOT NULL, name BLOB NOT NULL,
               present INTEGER NOT NULL,
               PRIMARY KEY (ino, parent_ino, name)
           ) WITHOUT ROWID;
           CREATE TEMP TABLE IF NOT EXISTS delta_stage_owner_counts (
               uid BLOB PRIMARY KEY, object_count INTEGER NOT NULL
           ) WITHOUT ROWID;
           DELETE FROM delta_stage_objects;
           DELETE FROM delta_stage_refs;
           DELETE FROM delta_stage_events;
           DELETE FROM delta_stage_object_overrides;
           DELETE FROM delta_stage_ref_overrides;
           DELETE FROM delta_stage_owner_counts;"#,
    )?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    {
        let mut statement = transaction.prepare_cached(
            r#"INSERT INTO delta_stage_objects(
                   ino, old_generation, new_generation, change_mask
               ) VALUES (?1, ?2, ?3, ?4)"#,
        )?;
        for change in manifest.objects.values() {
            statement.execute(params![
                encode_u64(change.ino).as_slice(),
                change
                    .old_generation
                    .map(encode_u64)
                    .as_ref()
                    .map(<[u8; 8]>::as_slice),
                change
                    .new_generation
                    .map(encode_u64)
                    .as_ref()
                    .map(<[u8; 8]>::as_slice),
                i64::try_from(change.change_mask)
                    .map_err(|_| ManagerError::new("change mask exceeds SQLite INTEGER"))?,
            ])?;
        }
    }
    {
        let mut statement = transaction.prepare_cached(
            r#"INSERT INTO delta_stage_refs(operation, ino, parent_ino, name)
               VALUES (?1, ?2, ?3, ?4)"#,
        )?;
        for (operation, references) in [(-1_i64, &manifest.ref_deletes), (1, &manifest.ref_adds)] {
            for reference in references {
                statement.execute(params![
                    operation,
                    encode_u64(reference.ino).as_slice(),
                    encode_u64(reference.parent_ino).as_slice(),
                    reference.name,
                ])?;
            }
        }
    }
    {
        let mut statement = transaction.prepare_cached(
            r#"INSERT INTO delta_stage_events(
                   ordinal, event_kind, ino, old_generation,
                   new_generation, change_mask, old_path, new_path
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
        )?;
        for (ordinal, event) in events.iter().enumerate() {
            statement.execute(params![
                i64::try_from(ordinal)
                    .map_err(|_| ManagerError::new("event ordinal exceeds SQLite INTEGER"))?,
                event_kind_name(&event.kind),
                encode_u64(event.ino).as_slice(),
                event
                    .old_generation
                    .map(encode_u64)
                    .as_ref()
                    .map(<[u8; 8]>::as_slice),
                event
                    .new_generation
                    .map(encode_u64)
                    .as_ref()
                    .map(<[u8; 8]>::as_slice),
                i64::try_from(event.change_mask)
                    .map_err(|_| ManagerError::new("event mask exceeds SQLite INTEGER"))?,
                event.old_path,
                event.new_path,
            ])?;
        }
    }
    {
        let mut statement = transaction.prepare_cached(
            r#"INSERT INTO delta_stage_object_overrides(
                   ino, present, generation, mode, nlink, uid, gid, rdev,
                   privilege_flags, security_xattr_hash
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
        )?;
        for change in manifest.objects.values() {
            if let Some(object) = target.objects.get(&change.ino) {
                statement.execute(params![
                    encode_u64(change.ino).as_slice(),
                    1,
                    encode_u64(object.generation).as_slice(),
                    object.mode,
                    object.nlink,
                    encode_u64(object.uid).as_slice(),
                    encode_u64(object.gid).as_slice(),
                    encode_u64(object.rdev).as_slice(),
                    i64::try_from(object.privilege_flags)
                        .map_err(|_| ManagerError::new("privilege flags overflow"))?,
                    object.security_xattr_hash.as_slice(),
                ])?;
            } else {
                statement.execute(params![
                    encode_u64(change.ino).as_slice(),
                    0,
                    rusqlite::types::Null,
                    rusqlite::types::Null,
                    rusqlite::types::Null,
                    rusqlite::types::Null,
                    rusqlite::types::Null,
                    rusqlite::types::Null,
                    rusqlite::types::Null,
                    rusqlite::types::Null,
                ])?;
            }
        }
    }
    {
        let mut statement = transaction.prepare_cached(
            r#"INSERT INTO delta_stage_ref_overrides(ino, parent_ino, name, present)
               VALUES (?1, ?2, ?3, ?4)"#,
        )?;
        for (present, references) in [(0_i64, &manifest.ref_deletes), (1, &manifest.ref_adds)] {
            for reference in references {
                statement.execute(params![
                    encode_u64(reference.ino).as_slice(),
                    encode_u64(reference.parent_ino).as_slice(),
                    reference.name,
                    present,
                ])?;
            }
        }
    }
    {
        let mut statement = transaction.prepare_cached(
            "INSERT INTO delta_stage_owner_counts(uid, object_count) VALUES (?1, ?2)",
        )?;
        for (&uid, &count) in owner_counts {
            if count < 0 {
                return Err(ManagerError::new("refuse negative owner-count override"));
            }
            statement.execute(params![encode_u64(uid).as_slice(), count])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn import_staged_comparison_rows(
    transaction: &Transaction<'_>,
    comparison_id: i64,
) -> Result<(), ManagerError> {
    transaction.execute(
        r#"INSERT INTO comparison_objects(
               comparison_id, ino, old_generation, new_generation, change_mask)
           SELECT ?1, ino, old_generation, new_generation, change_mask
             FROM delta_stage_objects ORDER BY ino"#,
        [comparison_id],
    )?;
    transaction.execute(
        r#"INSERT INTO comparison_refs(comparison_id, operation, ino, parent_ino, name)
           SELECT ?1, operation, ino, parent_ino, name
             FROM delta_stage_refs ORDER BY operation, ino, parent_ino, name"#,
        [comparison_id],
    )?;
    transaction.execute(
        r#"INSERT INTO change_events(
               comparison_id, ordinal, event_kind, ino, old_generation,
               new_generation, change_mask, old_path, new_path)
           SELECT ?1, ordinal, event_kind, ino, old_generation,
                  new_generation, change_mask, old_path, new_path
             FROM delta_stage_events ORDER BY ordinal"#,
        [comparison_id],
    )?;
    Ok(())
}

fn import_staged_revision_rows(
    transaction: &Transaction<'_>,
    revision_id: i64,
) -> Result<(), ManagerError> {
    transaction.execute(
        r#"INSERT INTO object_overrides(
               revision_id, ino, present, generation, mode, nlink, uid, gid,
               rdev, privilege_flags, security_xattr_hash)
           SELECT ?1, ino, present, generation, mode, nlink, uid, gid,
                  rdev, privilege_flags, security_xattr_hash
             FROM delta_stage_object_overrides ORDER BY ino"#,
        [revision_id],
    )?;
    transaction.execute(
        r#"INSERT INTO ref_overrides(revision_id, ino, parent_ino, name, present)
           SELECT ?1, ino, parent_ino, name, present
             FROM delta_stage_ref_overrides ORDER BY ino, parent_ino, name"#,
        [revision_id],
    )?;
    transaction.execute(
        r#"INSERT INTO owner_count_overrides(revision_id, uid, object_count)
           SELECT ?1, uid, object_count
             FROM delta_stage_owner_counts ORDER BY uid"#,
        [revision_id],
    )?;
    Ok(())
}

fn clear_staged_delta(connection: &mut rusqlite::Connection) -> Result<(), ManagerError> {
    connection.execute_batch(
        "DELETE FROM delta_stage_objects;
         DELETE FROM delta_stage_refs;
         DELETE FROM delta_stage_events;
         DELETE FROM delta_stage_object_overrides;
         DELETE FROM delta_stage_ref_overrides;
         DELETE FROM delta_stage_owner_counts;",
    )?;
    Ok(())
}

fn index_owner_counts(index: &Index) -> Result<BTreeMap<u64, i64>, ManagerError> {
    let mut counts = BTreeMap::new();
    for object in index.objects.values() {
        let count = counts.entry(object.uid).or_insert(0_i64);
        *count = count
            .checked_add(1)
            .ok_or_else(|| ManagerError::new("owner object count overflow"))?;
    }
    if counts.is_empty() {
        return Err(ManagerError::new("index has no object owners"));
    }
    Ok(counts)
}

fn owner_uid_xor(counts: &BTreeMap<u64, i64>) -> u64 {
    counts
        .iter()
        .filter_map(|(&uid, &count)| (count > 0).then_some(uid))
        .fold(0, |accumulator, uid| accumulator ^ uid)
}

fn event_kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::PathAdded => "path-added",
        EventKind::PathRemoved => "path-removed",
        EventKind::PathChanged => "path-changed",
        EventKind::SubtreeMoved => "subtree-moved",
        EventKind::DirectoryDirtyWitness => "directory-dirty-witness",
    }
}

fn parse_event_kind(kind: &str) -> Result<EventKind, ManagerError> {
    match kind {
        "path-added" => Ok(EventKind::PathAdded),
        "path-removed" => Ok(EventKind::PathRemoved),
        "path-changed" => Ok(EventKind::PathChanged),
        "subtree-moved" => Ok(EventKind::SubtreeMoved),
        "directory-dirty-witness" => Ok(EventKind::DirectoryDirtyWitness),
        _ => Err(ManagerError::new(format!(
            "unknown stored event kind {kind:?}"
        ))),
    }
}

fn apply_revision_metadata(
    base: &RevisionMetadata,
    base_subset: &Index,
    target_subset: &Index,
    manifest: &ChangedObjectsManifest,
    base_owner_counts: &BTreeMap<u64, i64>,
) -> Result<(RevisionMetadata, BTreeMap<u64, i64>), ManagerError> {
    let mut metadata = RevisionMetadata {
        summary_version: 2,
        owner_cardinality: base.owner_cardinality,
        owner_uid_xor: base.owner_uid_xor,
        object_count: base.object_count,
        ref_count: base.ref_count,
        state_hash: base.state_hash,
        single_owner_uid: base.single_owner_uid,
        privileged_metadata_count: base.privileged_metadata_count,
        security_state_hash: base.security_state_hash,
    };
    let mut owner_count_overrides = base_owner_counts.clone();
    for &ino in manifest.objects.keys() {
        let old = base_subset.objects.get(&ino);
        let new = target_subset.objects.get(&ino);
        match (old, new) {
            (None, Some(_)) => {
                metadata.object_count = metadata
                    .object_count
                    .checked_add(1)
                    .ok_or_else(|| ManagerError::new("revision object count overflow"))?;
            }
            (Some(_), None) => {
                metadata.object_count = metadata
                    .object_count
                    .checked_sub(1)
                    .ok_or_else(|| ManagerError::new("revision object count underflow"))?;
            }
            _ => {}
        }
        if old != new {
            if let Some(old) = old {
                xor_digest(&mut metadata.state_hash, &object_state_digest(old));
                xor_digest(
                    &mut metadata.security_state_hash,
                    &object_security_digest(old),
                );
                if old.privilege_flags != 0 {
                    metadata.privileged_metadata_count -= 1;
                }
            }
            if let Some(new) = new {
                xor_digest(&mut metadata.state_hash, &object_state_digest(new));
                xor_digest(
                    &mut metadata.security_state_hash,
                    &object_security_digest(new),
                );
                if new.privilege_flags != 0 {
                    metadata.privileged_metadata_count += 1;
                }
            }
        }
        if old.map(|object| object.uid) != new.map(|object| object.uid) {
            if let Some(old_uid) = old.map(|object| object.uid) {
                *owner_count_overrides.get_mut(&old_uid).ok_or_else(|| {
                    ManagerError::new("old owner count was not loaded for changed object")
                })? -= 1;
            }
            if let Some(new_uid) = new.map(|object| object.uid) {
                *owner_count_overrides.get_mut(&new_uid).ok_or_else(|| {
                    ManagerError::new("new owner count was not loaded for changed object")
                })? += 1;
            }
        }
    }
    for reference in &manifest.ref_deletes {
        metadata.ref_count = metadata
            .ref_count
            .checked_sub(1)
            .ok_or_else(|| ManagerError::new("revision reference count underflow"))?;
        xor_digest(&mut metadata.state_hash, &reference_state_digest(reference));
    }
    for reference in &manifest.ref_adds {
        metadata.ref_count = metadata
            .ref_count
            .checked_add(1)
            .ok_or_else(|| ManagerError::new("revision reference count overflow"))?;
        xor_digest(&mut metadata.state_hash, &reference_state_digest(reference));
    }
    if metadata.object_count <= 0 || metadata.ref_count < 0 {
        return Err(ManagerError::new("incremental revision counts are invalid"));
    }
    if metadata.privileged_metadata_count < 0 {
        return Err(ManagerError::new(
            "incremental privileged-metadata count underflow",
        ));
    }
    for (&uid, &new_count) in &owner_count_overrides {
        let old_count = base_owner_counts[&uid];
        if new_count < 0 {
            return Err(ManagerError::new("incremental owner count underflow"));
        }
        match (old_count == 0, new_count == 0) {
            (true, false) => {
                metadata.owner_cardinality = metadata
                    .owner_cardinality
                    .checked_add(1)
                    .ok_or_else(|| ManagerError::new("owner cardinality overflow"))?;
                metadata.owner_uid_xor ^= uid;
            }
            (false, true) => {
                metadata.owner_cardinality = metadata
                    .owner_cardinality
                    .checked_sub(1)
                    .ok_or_else(|| ManagerError::new("owner cardinality underflow"))?;
                metadata.owner_uid_xor ^= uid;
            }
            _ => {}
        }
    }
    if metadata.owner_cardinality <= 0 {
        return Err(ManagerError::new("incremental revision has no owners"));
    }
    metadata.single_owner_uid = (metadata.owner_cardinality == 1).then_some(metadata.owner_uid_xor);
    Ok((metadata, owner_count_overrides))
}

fn apply_stored_overrides(
    connection: &rusqlite::Connection,
    revision_id: i64,
    index: &mut Index,
) -> Result<(), ManagerError> {
    let mut objects = connection.prepare(
        r#"SELECT ino, present, generation, mode, nlink, uid, gid, rdev,
                  privilege_flags, security_xattr_hash
             FROM object_overrides WHERE revision_id = ?1 ORDER BY ino"#,
    )?;
    let rows = objects.query_map([revision_id], |row| {
        let ino = decode_sql_u64(row.get_ref(0)?.as_blob()?)?;
        let present: bool = row.get(1)?;
        if present {
            Ok((
                ino,
                Some(Object {
                    ino,
                    generation: decode_sql_u64(row.get_ref(2)?.as_blob()?)?,
                    mode: row.get(3)?,
                    nlink: row.get(4)?,
                    uid: decode_sql_u64(row.get_ref(5)?.as_blob()?)?,
                    gid: decode_sql_u64(row.get_ref(6)?.as_blob()?)?,
                    rdev: decode_sql_u64(row.get_ref(7)?.as_blob()?)?,
                    privilege_flags: row.get(8)?,
                    security_xattr_hash: fixed_sql_blob(row.get_ref(9)?.as_blob()?)?,
                }),
            ))
        } else {
            Ok((ino, None))
        }
    })?;
    for row in rows {
        let (ino, object) = row?;
        match object {
            Some(object) => {
                index.objects.insert(ino, object);
            }
            None => {
                index.objects.remove(&ino);
            }
        }
    }
    let mut references = connection.prepare(
        r#"SELECT ino, parent_ino, name, present
             FROM ref_overrides
            WHERE revision_id = ?1 ORDER BY ino, parent_ino, name"#,
    )?;
    let rows = references.query_map([revision_id], |row| {
        Ok((
            Reference {
                ino: decode_sql_u64(row.get_ref(0)?.as_blob()?)?,
                parent_ino: decode_sql_u64(row.get_ref(1)?.as_blob()?)?,
                name: row.get(2)?,
            },
            row.get::<_, bool>(3)?,
        ))
    })?;
    for row in rows {
        let (reference, present) = row?;
        if present {
            if !index.references.insert(reference) {
                return Err(ManagerError::new(
                    "stored overlay adds an inherited reference",
                ));
            }
        } else if !index.references.remove(&reference) {
            return Err(ManagerError::new(
                "stored overlay deletes an absent reference",
            ));
        }
    }
    Ok(())
}

fn decode_optional_object_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<Object>> {
    if !row.get::<_, bool>(0)? {
        return Ok(None);
    }
    Ok(Some(Object {
        ino: 0,
        generation: decode_sql_u64(row.get_ref(1)?.as_blob()?)?,
        mode: row.get(2)?,
        nlink: row.get(3)?,
        uid: decode_sql_u64(row.get_ref(4)?.as_blob()?)?,
        gid: decode_sql_u64(row.get_ref(5)?.as_blob()?)?,
        rdev: decode_sql_u64(row.get_ref(6)?.as_blob()?)?,
        privilege_flags: row.get(7)?,
        security_xattr_hash: fixed_sql_blob(row.get_ref(8)?.as_blob()?)?,
    }))
}

fn decode_checkpoint_object_row(row: &rusqlite::Row<'_>, ino: u64) -> rusqlite::Result<Object> {
    Ok(Object {
        ino,
        generation: decode_sql_u64(row.get_ref(0)?.as_blob()?)?,
        mode: row.get(1)?,
        nlink: row.get(2)?,
        uid: decode_sql_u64(row.get_ref(3)?.as_blob()?)?,
        gid: decode_sql_u64(row.get_ref(4)?.as_blob()?)?,
        rdev: decode_sql_u64(row.get_ref(5)?.as_blob()?)?,
        privilege_flags: row.get(6)?,
        security_xattr_hash: fixed_sql_blob(row.get_ref(7)?.as_blob()?)?,
    })
}

fn verify_initialize_publication(
    transaction: &Transaction<'_>,
    reservation: &InitializeReservation,
    lease_owner: [u8; 16],
    snapshot_id: i64,
    snapshot_uuid: [u8; 16],
) -> Result<(), ManagerError> {
    let valid: Option<i64> = transaction
        .query_row(
            "SELECT 1 \
               FROM operations o \
               JOIN watches w ON w.id = o.watch_id \
               JOIN snapshots s ON s.id = ?6 AND s.filesystem_id = o.filesystem_id \
              WHERE o.id = ?1 AND o.watch_id = ?2 AND o.state = 'uuid_recorded' \
                AND o.lease_owner = ?3 AND o.lease_fence = ?4 \
                AND o.discovered_uuid = ?5 AND s.subvol_uuid = ?5 \
                AND s.physical_state = 'present' AND s.readonly = 1 \
                AND w.state = 'initializing'",
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice(),
                lease_owner.as_slice(),
                reservation.operation_fence,
                snapshot_uuid.as_slice(),
                snapshot_id,
            ],
            |row| row.get(0),
        )
        .optional()?;
    if valid != Some(1) {
        return Err(ManagerError::new(
            "initialize publication fence or snapshot identity is stale",
        ));
    }
    Ok(())
}

fn update_revision_summary(
    transaction: &Transaction<'_>,
    revision_id: i64,
    summary: &RevisionMetadata,
) -> Result<(), ManagerError> {
    require_one(
        transaction.execute(
            r#"UPDATE revisions
                  SET object_count = ?2, ref_count = ?3, state_hash = ?4,
                      single_owner_uid = ?5, privileged_metadata_count = ?6,
                      security_state_hash = ?7, owner_cardinality = ?8,
                      owner_uid_xor = ?9, summary_version = 2
                WHERE id = ?1 AND state = 'ready'"#,
            params![
                revision_id,
                summary.object_count,
                summary.ref_count,
                summary.state_hash.as_slice(),
                summary
                    .single_owner_uid
                    .map(encode_u64)
                    .as_ref()
                    .map(<[u8; 8]>::as_slice),
                summary.privileged_metadata_count,
                summary.security_state_hash.as_slice(),
                summary.owner_cardinality,
                encode_u64(summary.owner_uid_xor).as_slice(),
            ],
        )?,
        "update revision summary",
    )?;
    Ok(())
}

fn insert_checkpoint(
    transaction: &Transaction<'_>,
    revision_id: i64,
    index: &Index,
) -> Result<(), ManagerError> {
    {
        let mut statement = transaction.prepare_cached(
            "INSERT INTO checkpoint_objects( \
                 revision_id, ino, generation, mode, nlink, uid, gid, rdev, \
                 privilege_flags, security_xattr_hash \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        for object in index.objects.values() {
            statement.execute(params![
                revision_id,
                encode_u64(object.ino).as_slice(),
                encode_u64(object.generation).as_slice(),
                object.mode,
                object.nlink,
                encode_u64(object.uid).as_slice(),
                encode_u64(object.gid).as_slice(),
                encode_u64(object.rdev).as_slice(),
                i64::try_from(object.privilege_flags)
                    .map_err(|_| ManagerError::new("privilege flags exceed SQLite INTEGER"))?,
                object.security_xattr_hash.as_slice(),
            ])?;
        }
    }
    {
        let mut statement = transaction.prepare_cached(
            "INSERT INTO checkpoint_refs(revision_id, ino, parent_ino, name) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for reference in &index.references {
            statement.execute(params![
                revision_id,
                encode_u64(reference.ino).as_slice(),
                encode_u64(reference.parent_ino).as_slice(),
                reference.name,
            ])?;
        }
    }
    replace_checkpoint_owner_counts(transaction, revision_id, &index_owner_counts(index)?)?;
    Ok(())
}

fn replace_checkpoint_owner_counts(
    transaction: &Transaction<'_>,
    revision_id: i64,
    counts: &BTreeMap<u64, i64>,
) -> Result<(), ManagerError> {
    transaction.execute(
        "DELETE FROM checkpoint_owner_counts WHERE revision_id = ?1",
        [revision_id],
    )?;
    let mut statement = transaction.prepare_cached(
        "INSERT INTO checkpoint_owner_counts(revision_id, uid, object_count) \
         VALUES (?1, ?2, ?3)",
    )?;
    for (&uid, &count) in counts {
        statement.execute(params![revision_id, encode_u64(uid).as_slice(), count])?;
    }
    Ok(())
}

fn filesystem_uuid(
    transaction: &Transaction<'_>,
    filesystem_id: i64,
) -> Result<[u8; 16], ManagerError> {
    let bytes: Vec<u8> = transaction.query_row(
        "SELECT fs_uuid FROM filesystems WHERE id = ?1",
        [filesystem_id],
        |row| row.get(0),
    )?;
    bytes
        .try_into()
        .map_err(|_| ManagerError::new("stored filesystem UUID has invalid length"))
}

fn filesystem_uuid_from_connection(
    connection: &rusqlite::Connection,
    filesystem_id: i64,
) -> Result<[u8; 16], ManagerError> {
    let bytes: Vec<u8> = connection.query_row(
        "SELECT fs_uuid FROM filesystems WHERE id = ?1",
        [filesystem_id],
        |row| row.get(0),
    )?;
    bytes
        .try_into()
        .map_err(|_| ManagerError::new("stored filesystem UUID has invalid length"))
}

fn claim_topology_lease(
    transaction: &Transaction<'_>,
    filesystem_id: i64,
    lease_owner: [u8; 16],
    now_ns: i64,
    lease_expires_ns: i64,
) -> Result<i64, ManagerError> {
    let claimed = transaction.execute(
        "UPDATE topology_leases \
            SET lease_owner = ?2, lease_fence = lease_fence + 1, lease_expires_ns = ?3 \
          WHERE filesystem_id = ?1 \
            AND (lease_owner IS NULL OR lease_expires_ns <= ?4 OR lease_owner = ?2)",
        params![
            filesystem_id,
            lease_owner.as_slice(),
            lease_expires_ns,
            now_ns,
        ],
    )?;
    if claimed != 1 {
        return Err(ManagerError::new("filesystem topology lease is busy"));
    }
    transaction
        .query_row(
            "SELECT lease_fence FROM topology_leases WHERE filesystem_id = ?1",
            [filesystem_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn release_topology_lease(
    transaction: &Transaction<'_>,
    filesystem_id: i64,
    lease_owner: [u8; 16],
    topology_fence: i64,
) -> Result<(), ManagerError> {
    require_one(
        transaction.execute(
            "UPDATE topology_leases SET lease_owner = NULL, lease_expires_ns = NULL \
             WHERE filesystem_id = ?1 AND lease_owner = ?2 AND lease_fence = ?3",
            params![filesystem_id, lease_owner.as_slice(), topology_fence,],
        )?,
        "release filesystem topology lease",
    )
}

fn reject_source_containing_worktree(
    transaction: &Transaction<'_>,
    filesystem_id: i64,
    source_path: &[u8],
) -> Result<(), ManagerError> {
    let mut statement = transaction.prepare(
        "SELECT path FROM worktrees \
         WHERE filesystem_id = ?1 AND state IN ('creating', 'present', 'deleting')",
    )?;
    let paths = statement
        .query_map([filesystem_id], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if paths
        .iter()
        .any(|path| path_is_same_or_descendant(path, source_path))
    {
        return Err(ManagerError::new(
            "initialize source contains a non-deleted Worktree reservation",
        ));
    }
    Ok(())
}

fn reject_destination_below_watch(
    transaction: &Transaction<'_>,
    filesystem_id: i64,
    destination_path: &[u8],
) -> Result<(), ManagerError> {
    let mut statement = transaction.prepare(
        "SELECT live_path FROM watches \
         WHERE filesystem_id = ?1 AND state IN ('initializing', 'active', 'blocked')",
    )?;
    let paths = statement
        .query_map([filesystem_id], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if paths
        .iter()
        .any(|path| path_is_same_or_descendant(destination_path, path))
    {
        return Err(ManagerError::new(
            "Worktree destination is below a non-deleted watched root",
        ));
    }
    Ok(())
}

fn path_is_same_or_descendant(candidate: &[u8], ancestor: &[u8]) -> bool {
    let candidate = Path::new(std::ffi::OsStr::from_bytes(candidate));
    let ancestor = Path::new(std::ffi::OsStr::from_bytes(ancestor));
    candidate.is_absolute() && ancestor.is_absolute() && candidate.starts_with(ancestor)
}

fn path_is_absolute(path: &[u8]) -> bool {
    Path::new(std::ffi::OsStr::from_bytes(path)).is_absolute()
}

fn require_one(changed: usize, action: &str) -> Result<(), ManagerError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(ManagerError::new(format!(
            "{action} affected {changed} rows, expected exactly one"
        )))
    }
}

fn random_id() -> [u8; 16] {
    *Uuid::new_v4().as_bytes()
}

fn decode_sql_u64(value: &[u8]) -> rusqlite::Result<u64> {
    decode_u64(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            value.len(),
            rusqlite::types::Type::Blob,
            Box::new(error),
        )
    })
}

fn fixed_sql_blob<const N: usize>(value: &[u8]) -> rusqlite::Result<[u8; N]> {
    value.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            value.len(),
            rusqlite::types::Type::Blob,
            Box::new(ManagerError::new(format!(
                "BLOB has length {}, expected {N}",
                value.len()
            ))),
        )
    })
}

fn fixed_manager_blob<const N: usize>(value: &[u8], field: &str) -> Result<[u8; N], ManagerError> {
    value.try_into().map_err(|_| {
        ManagerError::new(format!(
            "stored {field} has length {}, expected {N}",
            value.len()
        ))
    })
}

fn decode_snapshot_delete_candidate(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(i64, i64, SnapshotIdentity)> {
    let readonly: i64 = row.get(10)?;
    if readonly != 1 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            10,
            rusqlite::types::Type::Integer,
            Box::new(ManagerError::new(
                "managed snapshot delete candidate is not read-only",
            )),
        ));
    }
    let optional_uuid = |column| -> rusqlite::Result<Option<[u8; 16]>> {
        let value: Option<Vec<u8>> = row.get(column)?;
        value.as_deref().map(fixed_sql_blob::<16>).transpose()
    };
    Ok((
        row.get(0)?,
        row.get(1)?,
        SnapshotIdentity {
            fs_uuid: fixed_sql_blob(row.get_ref(2)?.as_blob()?)?,
            subvol_uuid: fixed_sql_blob(row.get_ref(3)?.as_blob()?)?,
            parent_uuid: optional_uuid(4)?,
            received_uuid: optional_uuid(5)?,
            root_id: decode_sql_u64(row.get_ref(6)?.as_blob()?)?,
            ctransid: decode_sql_u64(row.get_ref(7)?.as_blob()?)?,
            otransid: decode_sql_u64(row.get_ref(8)?.as_blob()?)?,
            path: row.get(9)?,
            readonly: true,
            created_ns: row.get(11)?,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{MODE_DIRECTORY, ROOT_INO};
    use crate::manifest::{
        ObjectChange, CHANGE_CREATED, CHANGE_DELETED, CHANGE_FILE_DATA, CHANGE_INODE, CHANGE_REF,
    };
    use crate::namespace::ViewBinding;
    use crate::store::ServiceMetadata;
    use tempfile::tempdir;

    fn object(ino: u64, mode: u32, nlink: u32) -> Object {
        Object {
            ino,
            generation: ino + 1,
            mode,
            nlink,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            privilege_flags: 0,
            security_xattr_hash: [0; 32],
        }
    }

    fn index() -> Index {
        let mut index = Index::default();
        index
            .objects
            .insert(ROOT_INO, object(ROOT_INO, MODE_DIRECTORY | 0o755, 1));
        index.objects.insert(300, object(300, 0o100644, 2));
        for name in [b"a".as_slice(), &[0xff, b'b']] {
            index.references.insert(Reference {
                ino: 300,
                parent_ino: ROOT_INO,
                name: name.to_vec(),
            });
        }
        index
    }

    fn setup() -> (tempfile::TempDir, Store, InitializeRequest) {
        let temp = tempdir().unwrap();
        let metadata = ServiceMetadata {
            store_uuid: [10; 16],
            clock_hmac_key: [11; 32],
            clock_format_version: 1,
            last_boot_id: [12; 16],
            created_ns: 1,
        };
        let store = Store::create(&temp.path().join("state.sqlite3"), &metadata).unwrap();
        let request = InitializeRequest {
            fs_uuid: [1; 16],
            source_subvol_uuid: [2; 16],
            source_path: b"/source".to_vec(),
            reserved_snapshot_path: b"/store/snapshots/w/s-0-op".to_vec(),
            principal: Principal::Uid(1000),
            permissions: Permissions::new(PERMISSION_READ | PERMISSION_CUT).unwrap(),
            requester_uid: 1000,
            requester_gid: 1000,
            lease_owner: [3; 16],
            now_ns: 100,
            lease_expires_ns: 1000,
        };
        (temp, store, request)
    }

    fn snapshot(request: &InitializeRequest) -> SnapshotIdentity {
        SnapshotIdentity {
            fs_uuid: request.fs_uuid,
            subvol_uuid: [4; 16],
            parent_uuid: Some(request.source_subvol_uuid),
            received_uuid: None,
            root_id: 900,
            ctransid: 10,
            otransid: 9,
            path: request.reserved_snapshot_path.clone(),
            readonly: true,
            created_ns: 200,
        }
    }

    fn initialize_watch(
        store: &mut Store,
        request: &InitializeRequest,
    ) -> (InitializeReservation, InitializedWatch) {
        initialize_watch_with_index(store, request, &index())
    }

    fn initialize_watch_with_index(
        store: &mut Store,
        request: &InitializeRequest,
        initial_index: &Index,
    ) -> (InitializeReservation, InitializedWatch) {
        let reservation = store.reserve_initialize(request).unwrap();
        store
            .start_initialize_filesystem_effect(&reservation, request.lease_owner, 150)
            .unwrap();
        let recorded = store
            .record_initialize_snapshot(&reservation, request.lease_owner, &snapshot(request), 200)
            .unwrap();
        let initialized = store
            .publish_initial_checkpoint(
                &reservation,
                request.lease_owner,
                &recorded,
                initial_index,
                300,
            )
            .unwrap();
        (reservation, initialized)
    }

    #[test]
    fn delta_subset_loads_changed_aliases_and_ancestors_without_unrelated_objects() {
        let (_temp, mut store, request) = setup();
        let mut initial = index();
        initial
            .objects
            .insert(400, object(400, MODE_DIRECTORY | 0o755, 1));
        initial.objects.insert(401, object(401, 0o100644, 1));
        initial.references.insert(Reference {
            ino: 400,
            parent_ino: ROOT_INO,
            name: b"unrelated-directory".to_vec(),
        });
        initial.references.insert(Reference {
            ino: 401,
            parent_ino: 400,
            name: b"unrelated-file".to_vec(),
        });
        let (_reservation, initialized) =
            initialize_watch_with_index(&mut store, &request, &initial);
        let manifest = ChangedObjectsManifest {
            objects: [(
                300,
                ObjectChange {
                    ino: 300,
                    old_generation: Some(301),
                    new_generation: Some(301),
                    change_mask: CHANGE_FILE_DATA,
                },
            )]
            .into(),
            ref_adds: BTreeSet::new(),
            ref_deletes: BTreeSet::new(),
            raw_ref_adds: 0,
            raw_ref_deletes: 0,
        };
        let subset = store
            .load_revision_delta_subset(initialized.revision_id, &manifest)
            .unwrap();
        assert_eq!(
            subset.objects.keys().copied().collect::<Vec<_>>(),
            vec![256, 300]
        );
        assert_eq!(
            subset.paths(300).unwrap(),
            vec![b"a".to_vec(), vec![0xff, b'b']]
        );
        assert!(!subset.objects.contains_key(&400));
        assert!(!subset.objects.contains_key(&401));
    }

    #[test]
    fn owner_cardinality_delta_recovers_single_owner_without_a_namespace_scan() {
        let mut base = Index::default();
        base.objects
            .insert(ROOT_INO, object(ROOT_INO, MODE_DIRECTORY | 0o755, 1));
        let mut other_owner = object(300, 0o100644, 1);
        other_owner.uid = 2000;
        base.objects.insert(300, other_owner.clone());
        let reference = Reference {
            ino: 300,
            parent_ino: ROOT_INO,
            name: b"other-owner".to_vec(),
        };
        base.references.insert(reference.clone());
        base.validate().unwrap();
        let mut target = base.clone();
        target.objects.remove(&300);
        target.references.remove(&reference);
        target.validate().unwrap();
        let safety = base.safety_summary();
        let metadata = RevisionMetadata {
            summary_version: 2,
            owner_cardinality: 2,
            owner_uid_xor: 1000 ^ 2000,
            object_count: 2,
            ref_count: 1,
            state_hash: base.state_hash(),
            single_owner_uid: safety.single_owner_uid,
            privileged_metadata_count: 0,
            security_state_hash: safety.security_state_hash,
        };
        let manifest = ChangedObjectsManifest {
            objects: [(
                300,
                ObjectChange {
                    ino: 300,
                    old_generation: Some(other_owner.generation),
                    new_generation: None,
                    change_mask: CHANGE_INODE | CHANGE_REF | CHANGE_DELETED,
                },
            )]
            .into(),
            ref_adds: BTreeSet::new(),
            ref_deletes: [reference].into(),
            raw_ref_adds: 0,
            raw_ref_deletes: 1,
        };
        let (updated, counts) = apply_revision_metadata(
            &metadata,
            &base,
            &target,
            &manifest,
            &BTreeMap::from([(2000, 1)]),
        )
        .unwrap();
        assert_eq!(counts, BTreeMap::from([(2000, 0)]));
        assert_eq!(updated.owner_cardinality, 1);
        assert_eq!(updated.owner_uid_xor, 1000);
        assert_eq!(updated.single_owner_uid, Some(1000));
        assert_eq!(updated.state_hash, target.state_hash());
        assert_eq!(
            updated.security_state_hash,
            target.safety_summary().security_state_hash
        );
    }

    #[test]
    fn compaction_upgrades_a_legacy_summary_and_owner_checkpoint() {
        let (_temp, mut store, request) = setup();
        let (_reservation, initialized) = initialize_watch(&mut store, &request);
        store
            .connection_mut()
            .execute(
                "DELETE FROM checkpoint_owner_counts WHERE revision_id = ?1",
                [initialized.revision_id],
            )
            .unwrap();
        store
            .connection_mut()
            .execute(
                r#"UPDATE revisions
                      SET summary_version = 1, state_hash = NULL,
                          security_state_hash = NULL,
                          owner_cardinality = NULL, owner_uid_xor = NULL
                    WHERE id = ?1"#,
                [initialized.revision_id],
            )
            .unwrap();
        assert!(store
            .compact_revision(initialized.revision_id, [88; 16])
            .unwrap());
        let metadata = store
            .load_revision_metadata(initialized.revision_id)
            .unwrap();
        assert_eq!(metadata.summary_version, 2);
        assert_eq!(metadata.owner_cardinality, 1);
        assert_eq!(metadata.owner_uid_xor, 1000);
        let owner_count: i64 = store
            .connection()
            .query_row(
                "SELECT object_count FROM checkpoint_owner_counts \
                 WHERE revision_id = ?1 AND uid = ?2",
                params![initialized.revision_id, encode_u64(1000).as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner_count, 2);
    }

    fn cut_snapshot(request: &CutRequest, source_uuid: [u8; 16]) -> SnapshotIdentity {
        SnapshotIdentity {
            fs_uuid: [1; 16],
            subvol_uuid: [5; 16],
            parent_uuid: Some(source_uuid),
            received_uuid: None,
            root_id: 901,
            ctransid: 11,
            otransid: 10,
            path: request.reserved_snapshot_path.clone(),
            readonly: true,
            created_ns: 500,
        }
    }

    #[test]
    fn publishes_initial_checkpoint_atomically_and_round_trips_bytes() {
        let (_temp, mut store, request) = setup();
        let reservation = store.reserve_initialize(&request).unwrap();
        store
            .start_initialize_filesystem_effect(&reservation, request.lease_owner, 150)
            .unwrap();
        let recorded = store
            .record_initialize_snapshot(&reservation, request.lease_owner, &snapshot(&request), 200)
            .unwrap();
        let expected = index();
        let initialized = store
            .publish_initial_checkpoint(
                &reservation,
                request.lease_owner,
                &recorded,
                &expected,
                300,
            )
            .unwrap();
        assert_eq!(initialized.sequence, 0);
        assert!(initialized.fresh_instance);
        assert_eq!(
            store.load_checkpoint(initialized.revision_id).unwrap(),
            expected
        );
        assert!(store.foreign_key_violations().unwrap().is_empty());

        let (state, indexed_seq, last_cut_seq): (String, i64, i64) = store
            .connection()
            .query_row(
                "SELECT state, indexed_seq, last_cut_seq FROM watches WHERE id = ?1",
                [reservation.watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (state.as_str(), indexed_seq, last_cut_seq),
            ("active", 0, 0)
        );
        let pin_count: i64 = store
            .connection()
            .query_row(
                "SELECT count(*) FROM snapshot_pins WHERE snapshot_id = ?1",
                [recorded.snapshot_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pin_count, 2);
    }

    #[test]
    fn startup_aborts_only_pre_effect_initialize_intents() {
        let (_temp, mut store, request) = setup();
        store.reserve_initialize(&request).unwrap();
        assert_eq!(store.abort_planned_operations(200).unwrap(), 1);
        let state: (i64, i64, i64) = store
            .connection()
            .query_row(
                "SELECT \
                     (SELECT count(*) FROM operations), \
                     (SELECT count(*) FROM watches), \
                     (SELECT count(*) FROM watch_grants)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, (0, 0, 0));

        let reservation = store.reserve_initialize(&request).unwrap();
        store
            .start_initialize_filesystem_effect(&reservation, request.lease_owner, 250)
            .unwrap();
        assert_eq!(store.abort_planned_operations(300).unwrap(), 0);
        let state: String = store
            .connection()
            .query_row(
                "SELECT state FROM operations WHERE id = ?1",
                [reservation.operation_id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "fs_started");
    }

    #[test]
    fn startup_takeover_fences_the_old_post_effect_worker_owner() {
        let (_temp, mut store, request) = setup();
        let reservation = store.reserve_initialize(&request).unwrap();
        store
            .start_initialize_filesystem_effect(&reservation, request.lease_owner, 150)
            .unwrap();
        let new_owner = [99; 16];
        assert_eq!(store.takeover_recovery_leases(new_owner, 2_000).unwrap(), 1);
        assert!(
            store
                .record_initialize_snapshot(
                    &reservation,
                    request.lease_owner,
                    &snapshot(&request),
                    200,
                )
                .is_err()
        );
        store
            .record_initialize_snapshot(&reservation, new_owner, &snapshot(&request), 200)
            .unwrap();
    }

    #[test]
    fn retention_leases_pin_transactionally_and_expire() {
        let (_temp, mut store, mut request) = setup();
        request.permissions =
            Permissions::new(PERMISSION_READ | PERMISSION_CUT | PERMISSION_RETAIN).unwrap();
        let (_reservation, initialized) = initialize_watch(&mut store, &request);
        let lease = store
            .create_retention_lease(
                initialized.watch_id,
                initialized.grant_id,
                initialized.snapshot_id,
                400,
                500,
            )
            .unwrap();
        let pins: i64 = store
            .connection()
            .query_row(
                "SELECT count(*) FROM snapshot_pins WHERE owner_kind = 'retention-lease' AND owner_id = ?1",
                [lease.id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pins, 1);
        assert_eq!(store.expire_retention_leases(499).unwrap(), 0);
        assert_eq!(store.expire_retention_leases(500).unwrap(), 1);
        let state: String = store
            .connection()
            .query_row(
                "SELECT state FROM retention_leases WHERE id = ?1",
                [lease.id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "expired");
        let pins: i64 = store
            .connection()
            .query_row(
                "SELECT count(*) FROM snapshot_pins WHERE owner_kind = 'retention-lease' AND owner_id = ?1",
                [lease.id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pins, 0);
    }

    #[test]
    fn revocation_releases_retention_and_blocks_last_grant() {
        let (_temp, mut store, mut request) = setup();
        request.permissions =
            Permissions::new(PERMISSION_READ | PERMISSION_CUT | PERMISSION_RETAIN).unwrap();
        let (_reservation, initialized) = initialize_watch(&mut store, &request);
        let lease = store
            .create_retention_lease(
                initialized.watch_id,
                initialized.grant_id,
                initialized.snapshot_id,
                400,
                800,
            )
            .unwrap();
        store
            .revoke_grant(initialized.watch_id, initialized.grant_id, 450)
            .unwrap();
        let (grant_state, watch_state, lease_state, pins): (String, String, String, i64) = store
            .connection()
            .query_row(
                r#"SELECT g.state, w.state, r.state,
                          (SELECT count(*) FROM snapshot_pins p
                            WHERE p.owner_kind = 'retention-lease' AND p.owner_id = r.id)
                     FROM watch_grants g
                     JOIN watches w ON w.id = g.watch_id
                     JOIN retention_leases r ON r.authorization_id = g.id
                    WHERE g.id = ?1 AND r.id = ?2"#,
                params![initialized.grant_id.as_slice(), lease.id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(grant_state, "revoked");
        assert_eq!(watch_state, "blocked");
        assert_eq!(lease_state, "revoked");
        assert_eq!(pins, 0);
    }

    #[test]
    fn revocation_waits_for_the_active_response_fence() {
        let (_temp, mut store, request) = setup();
        let (_reservation, initialized) = initialize_watch(&mut store, &request);
        let query_id = [70; 16];
        let query_owner = [71_u8; 16];
        let clock_epoch: Vec<u8> = store
            .connection()
            .query_row(
                "SELECT clock_epoch FROM watches WHERE id = ?1",
                [initialized.watch_id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        store
            .connection_mut()
            .execute(
                r#"INSERT INTO query_leases(
                       id, watch_id, authorization_id, clock_epoch,
                       from_cut_sequence, to_cut_sequence, guard_epoch,
                       from_guard_sequence, to_guard_sequence, lease_owner,
                       lease_fence, lease_expires_ns, state
                   ) VALUES (?1, ?2, ?3, ?4, NULL, 0, NULL, NULL, NULL,
                             ?5, 1, 1000, 'active')"#,
                params![
                    query_id.as_slice(),
                    initialized.watch_id.as_slice(),
                    initialized.grant_id.as_slice(),
                    clock_epoch,
                    query_owner.as_slice(),
                ],
            )
            .unwrap();
        store
            .connection_mut()
            .execute(
                "INSERT INTO query_revision_pins(query_id, revision_id) VALUES (?1, ?2)",
                params![query_id.as_slice(), initialized.revision_id],
            )
            .unwrap();
        assert!(store
            .revoke_grant(initialized.watch_id, initialized.grant_id, 450)
            .unwrap_err()
            .to_string()
            .contains("response lease"));
        let states: (String, String) = store
            .connection()
            .query_row(
                "SELECT g.state, q.state FROM watch_grants g JOIN query_leases q \
                 ON q.authorization_id = g.id WHERE g.id = ?1 AND q.id = ?2",
                params![initialized.grant_id.as_slice(), query_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(states, ("active".to_owned(), "active".to_owned()));

        store
            .connection_mut()
            .execute(
                "DELETE FROM query_revision_pins WHERE query_id = ?1",
                [query_id.as_slice()],
            )
            .unwrap();
        store
            .connection_mut()
            .execute(
                "UPDATE query_leases SET state = 'released' WHERE id = ?1",
                [query_id.as_slice()],
            )
            .unwrap();
        store
            .revoke_grant(initialized.watch_id, initialized.grant_id, 451)
            .unwrap();
    }

    #[test]
    fn facade_invalidation_waits_for_the_active_response_fence() {
        let (_temp, mut store, request) = setup();
        let (_reservation, initialized) = initialize_watch(&mut store, &request);
        let binding = ViewBinding {
            monitor_session_id: [72; 16],
            root_path: request.source_path.clone(),
            fs_uuid: request.fs_uuid,
            subvol_uuid: request.source_subvol_uuid,
            mount_ns_dev: 1,
            mount_ns_ino: 2,
            process_root_dev: 3,
            process_root_ino: 4,
            process_root_mnt_id: 5,
            watched_root_dev: 6,
            watched_root_ino: 7,
            watched_root_mnt_id: 8,
        };
        let activation = store
            .activate_snapshot_facade(initialized.watch_id, initialized.grant_id, &binding)
            .unwrap();
        let query_id = [73; 16];
        store
            .connection_mut()
            .execute(
                r#"INSERT INTO query_leases(
                       id, watch_id, authorization_id, clock_epoch,
                       from_cut_sequence, to_cut_sequence, guard_epoch,
                       from_guard_sequence, to_guard_sequence, lease_owner,
                       lease_fence, lease_expires_ns, state
                   ) VALUES (?1, ?2, ?3, ?4, NULL, 0, NULL, NULL, NULL,
                             ?5, 1, 1000, 'active')"#,
                params![
                    query_id.as_slice(),
                    initialized.watch_id.as_slice(),
                    initialized.grant_id.as_slice(),
                    activation.clock_epoch.as_slice(),
                    [74_u8; 16].as_slice(),
                ],
            )
            .unwrap();

        assert!(store
            .invalidate_snapshot_facade(&activation)
            .unwrap_err()
            .to_string()
            .contains("response lease"));
        let (state, epoch): (String, Vec<u8>) = store
            .connection()
            .query_row(
                "SELECT fsmonitor_state, clock_epoch FROM watches WHERE id = ?1",
                [initialized.watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "snapshot_only");
        assert_eq!(epoch, activation.clock_epoch);

        store
            .connection_mut()
            .execute(
                "UPDATE query_leases SET state = 'released' WHERE id = ?1",
                [query_id.as_slice()],
            )
            .unwrap();
        store.invalidate_snapshot_facade(&activation).unwrap();
    }

    #[test]
    fn revocation_cancels_a_cut_before_the_effect_boundary() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let cut = store
            .reserve_cut(&CutRequest {
                watch_id: initialized.watch_id,
                authorization_id: initialized.grant_id,
                reserved_snapshot_path: b"/store/snapshots/revoked-cut".to_vec(),
                requester_uid: 1000,
                requester_gid: 1000,
                lease_owner: [61; 16],
                now_ns: 400,
                lease_expires_ns: 2_000,
            })
            .unwrap();
        store
            .admit_planned_cut(
                initialized.watch_id,
                initialized.grant_id,
                [62; 16],
                "query",
                410,
                1_500,
            )
            .unwrap()
            .unwrap();
        store
            .revoke_grant(initialized.watch_id, initialized.grant_id, 450)
            .unwrap();
        let state: (i64, i64, i64, String) = store
            .connection()
            .query_row(
                r#"SELECT
                    (SELECT count(*) FROM operations WHERE id = ?1),
                    (SELECT count(*) FROM snapshot_pins
                      WHERE owner_kind = 'operation' AND owner_id = ?1),
                    (SELECT count(*) FROM cut_admissions WHERE operation_id = ?1),
                    (SELECT state FROM watch_grants WHERE id = ?2)"#,
                params![cut.operation_id.as_slice(), initialized.grant_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(state, (0, 0, 0, "revoked".to_owned()));
        assert!(store
            .start_cut_filesystem_effect(&cut, [61; 16], 500)
            .is_err());
    }

    #[test]
    fn stale_fence_cannot_start_or_publish() {
        let (_temp, mut store, request) = setup();
        let reservation = store.reserve_initialize(&request).unwrap();
        assert!(store
            .start_initialize_filesystem_effect(&reservation, [99; 16], 150)
            .is_err());
        store
            .start_initialize_filesystem_effect(&reservation, request.lease_owner, 150)
            .unwrap();
        let recorded = store
            .record_initialize_snapshot(&reservation, request.lease_owner, &snapshot(&request), 200)
            .unwrap();
        assert!(store
            .publish_initial_checkpoint(&reservation, [99; 16], &recorded, &index(), 300)
            .is_err());
        let revision_count: i64 = store
            .connection()
            .query_row("SELECT count(*) FROM revisions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(revision_count, 0);
    }

    #[test]
    fn snapshot_gc_reserves_only_unpinned_rows_and_tombstones_under_one_fence() {
        let (_temp, mut store, request) = setup();
        let (_reservation, initialized) = initialize_watch(&mut store, &request);
        assert!(store
            .reserve_unpinned_snapshot_deletes([20; 16], 400, 1000, 4)
            .unwrap()
            .is_empty());

        store
            .connection_mut()
            .execute(
                "DELETE FROM snapshot_pins WHERE snapshot_id = ?1",
                [initialized.snapshot_id],
            )
            .unwrap();
        let reservations = store
            .reserve_unpinned_snapshot_deletes([20; 16], 400, 1000, 4)
            .unwrap();
        assert_eq!(reservations.len(), 1);
        let reservation = &reservations[0];
        assert_eq!(reservation.snapshot_id, initialized.snapshot_id);
        assert!(store
            .start_snapshot_delete(reservation, [21; 16], 450)
            .is_err());
        store
            .start_snapshot_delete(reservation, [20; 16], 450)
            .unwrap();
        store
            .record_snapshot_delete_durable(reservation, [20; 16], 500)
            .unwrap();
        store
            .finish_snapshot_delete(reservation, [20; 16], 550)
            .unwrap();
        let (snapshot_state, operation_state): (String, String) = store
            .connection()
            .query_row(
                "SELECT s.physical_state, d.state FROM snapshots s \
                 JOIN snapshot_delete_operations d ON d.snapshot_id = s.id \
                 WHERE s.id = ?1",
                [initialized.snapshot_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (snapshot_state.as_str(), operation_state.as_str()),
            ("deleted", "done")
        );
    }

    #[test]
    fn refuses_worktree_permission_without_policy() {
        let (_temp, mut store, mut request) = setup();
        request.permissions =
            Permissions::new(PERMISSION_READ | PERMISSION_CUT | PERMISSION_WORKTREE).unwrap();
        assert!(store.reserve_initialize(&request).is_err());
    }

    #[test]
    fn topology_path_exclusion_is_component_aware_and_symmetric() {
        assert!(path_is_same_or_descendant(b"/watch/child", b"/watch"));
        assert!(path_is_same_or_descendant(b"/watch", b"/watch"));
        assert!(!path_is_same_or_descendant(b"/watch-two", b"/watch"));
        assert!(!path_is_same_or_descendant(b"relative/child", b"relative"));

        let (_temp, mut store, request) = setup();
        let (initialize, initialized) = initialize_watch(&mut store, &request);
        let transaction = store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert!(reject_destination_below_watch(
            &transaction,
            initialize.filesystem_id,
            b"/source/worktree"
        )
        .is_err());
        reject_destination_below_watch(
            &transaction,
            initialize.filesystem_id,
            b"/source-two/worktree",
        )
        .unwrap();

        let operation_id = [91_u8; 16];
        transaction
            .execute(
                r#"INSERT INTO operations(
                       id, kind, state, filesystem_id, watch_id, sequence,
                       source_subvol_uuid, base_snapshot_id, expected_parent_uuid,
                       requested_readonly, requester_uid, requester_gid,
                       authorization_id, reserved_path, lease_owner, lease_fence,
                       lease_expires_ns, updated_ns
                   ) VALUES (?1, 'cut', 'planned', ?2, ?3, 100, ?4, ?5,
                             ?4, 1, 1000, 1000, ?6, ?7, ?8, 1, 1000, 400)"#,
                params![
                    operation_id.as_slice(),
                    initialize.filesystem_id,
                    initialized.watch_id.as_slice(),
                    request.source_subvol_uuid.as_slice(),
                    initialized.snapshot_id,
                    initialized.grant_id.as_slice(),
                    b"/staging/worktree".as_slice(),
                    request.lease_owner.as_slice(),
                ],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO worktrees(id, filesystem_id, subvol_uuid, path, \
                 seed_revision_id, watch_id, operation_id, state) \
                 VALUES (?1, ?2, NULL, ?3, ?4, NULL, ?5, 'creating')",
                params![
                    [92_u8; 16].as_slice(),
                    initialize.filesystem_id,
                    b"/future/worktree".as_slice(),
                    initialized.revision_id,
                    operation_id.as_slice(),
                ],
            )
            .unwrap();
        assert!(reject_source_containing_worktree(
            &transaction,
            initialize.filesystem_id,
            b"/future"
        )
        .is_err());
        reject_source_containing_worktree(&transaction, initialize.filesystem_id, b"/future-two")
            .unwrap();
        transaction.rollback().unwrap();
    }

    #[test]
    fn continuity_loss_rotates_and_disables_the_facade() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let binding = ViewBinding {
            monitor_session_id: [31; 16],
            root_path: request.source_path.clone(),
            fs_uuid: request.fs_uuid,
            subvol_uuid: request.source_subvol_uuid,
            mount_ns_dev: 1,
            mount_ns_ino: 2,
            process_root_dev: 3,
            process_root_ino: 4,
            process_root_mnt_id: 5,
            watched_root_dev: 6,
            watched_root_ino: 7,
            watched_root_mnt_id: 8,
        };
        let activation = store
            .activate_snapshot_facade(initialized.watch_id, initialized.grant_id, &binding)
            .unwrap();
        store.invalidate_snapshot_facade(&activation).unwrap();
        let (state, epoch, owner): (String, Vec<u8>, Option<Vec<u8>>) = store
            .connection()
            .query_row(
                "SELECT fsmonitor_state, clock_epoch, fsmonitor_owner_grant_id \
                   FROM watches WHERE id = ?1",
                [initialized.watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "disabled");
        assert_ne!(epoch, activation.clock_epoch);
        assert_eq!(owner, None);
        assert!(store.invalidate_snapshot_facade(&activation).is_err());
        store
            .activate_snapshot_facade(initialized.watch_id, initialized.grant_id, &binding)
            .unwrap();
        let report = store.recover_process_state([99; 16]).unwrap();
        assert_eq!(report.invalidated_facades, 1);
        assert!(report.boot_changed);
        let state: String = store
            .connection()
            .query_row(
                "SELECT fsmonitor_state FROM watches WHERE id = ?1",
                [initialized.watch_id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "disabled");
    }

    #[test]
    fn process_recovery_fails_direct_comparisons_and_releases_endpoint_pins() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        store
            .connection_mut()
            .execute(
                r#"INSERT INTO comparisons(
                       from_snapshot_id, to_snapshot_id, comparison_kind,
                       algorithm_version, state, lease_owner, lease_fence,
                       lease_expires_ns)
                   VALUES (?1, ?1, 'incremental', 2, 'claimed', ?2, 4, 9999)"#,
                params![initialized.snapshot_id, [88_u8; 16].as_slice()],
            )
            .unwrap();
        let comparison_id = store.connection().last_insert_rowid();
        let owner_id = encode_u64(u64::try_from(comparison_id).unwrap());
        store
            .connection_mut()
            .execute(
                "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) \
                 VALUES (?1, 'comparison', ?2, 'test')",
                params![initialized.snapshot_id, owner_id.as_slice()],
            )
            .unwrap();

        let report = store.recover_process_state([99; 16]).unwrap();
        assert_eq!(report.abandoned_historical_comparisons, 1);
        let recovered: (String, Option<Vec<u8>>, i64, i64) = store
            .connection()
            .query_row(
                "SELECT state, lease_owner, lease_fence, \
                        (SELECT count(*) FROM snapshot_pins \
                          WHERE owner_kind = 'comparison' AND owner_id = ?2) \
                   FROM comparisons WHERE id = ?1",
                params![comparison_id, owner_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(recovered, ("failed".to_owned(), None, 5, 0));
    }

    #[test]
    fn precision_events_and_guard_state_advance_atomically() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let binding = ViewBinding {
            monitor_session_id: [51; 16],
            root_path: request.source_path.clone(),
            fs_uuid: request.fs_uuid,
            subvol_uuid: request.source_subvol_uuid,
            mount_ns_dev: 1,
            mount_ns_ino: 2,
            process_root_dev: 3,
            process_root_ino: 4,
            process_root_mnt_id: 5,
            watched_root_dev: 6,
            watched_root_ino: 7,
            watched_root_mnt_id: 8,
        };
        let activation = store
            .activate_snapshot_facade(initialized.watch_id, initialized.grant_id, &binding)
            .unwrap();
        let initial = store.begin_precision_guard(&activation).unwrap();
        assert_eq!(initial.sequence, 0);
        let cursor = store
            .append_precision_events(
                &activation,
                initial.epoch,
                &[
                    MutationHint::Path(b"raw\xffname".to_vec()),
                    MutationHint::DirectoryPrefix(b"moved".to_vec()),
                ],
                500,
            )
            .unwrap();
        assert_eq!(cursor.sequence, 2);
        store.complete_precision_guard(&activation, cursor).unwrap();
        let state: (String, i64, i64) = store
            .connection()
            .query_row(
                "SELECT fsmonitor_state, guard_head_seq, \
                        (SELECT count(*) FROM mutation_events WHERE watch_id = ?1) \
                   FROM watches WHERE id = ?1",
                [initialized.watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, ("guard_active".to_owned(), 2, 2));
        store
            .gap_precision_guard(&activation, initial.epoch)
            .unwrap();
        assert!(store
            .append_precision_events(
                &activation,
                initial.epoch,
                &[MutationHint::Path(b"too-late".to_vec())],
                600,
            )
            .is_err());
        let replacement = ViewBinding {
            monitor_session_id: [52; 16],
            ..binding
        };
        store
            .activate_snapshot_facade(initialized.watch_id, initialized.grant_id, &replacement)
            .unwrap();
        let reset: (String, Option<Vec<u8>>) = store
            .connection()
            .query_row(
                "SELECT fsmonitor_state, guard_epoch FROM watches WHERE id = ?1",
                [initialized.watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(reset, ("snapshot_only".to_owned(), None));
    }

    #[test]
    fn sqlite_full_cannot_publish_a_partial_precision_head() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let binding = ViewBinding {
            monitor_session_id: [61; 16],
            root_path: request.source_path.clone(),
            fs_uuid: request.fs_uuid,
            subvol_uuid: request.source_subvol_uuid,
            mount_ns_dev: 1,
            mount_ns_ino: 2,
            process_root_dev: 3,
            process_root_ino: 4,
            process_root_mnt_id: 5,
            watched_root_dev: 6,
            watched_root_ino: 7,
            watched_root_mnt_id: 8,
        };
        let activation = store
            .activate_snapshot_facade(initialized.watch_id, initialized.grant_id, &binding)
            .unwrap();
        let cursor = store.begin_precision_guard(&activation).unwrap();
        let page_count: i64 = store
            .connection()
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        store
            .connection_mut()
            .pragma_update(None, "max_page_count", page_count)
            .unwrap();
        let oversized = MutationHint::Path(vec![b'x'; 2 * 1024 * 1024]);
        assert!(store
            .append_precision_events(&activation, cursor.epoch, &[oversized], 500)
            .is_err());
        let (head, events): (i64, i64) = store
            .connection()
            .query_row(
                "SELECT w.guard_head_seq, \
                        (SELECT count(*) FROM mutation_events e WHERE e.watch_id = w.id) \
                   FROM watches w WHERE w.id = ?1",
                [initialized.watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((head, events), (0, 0));
    }

    #[test]
    fn fixed_jj_trigger_is_grant_scoped_and_fenced() {
        let (_temp, mut store, mut request) = setup();
        request.permissions =
            Permissions::new(PERMISSION_READ | PERMISSION_CUT | PERMISSION_TRIGGER).unwrap();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        assert!(!store
            .has_fixed_jj_trigger(initialized.watch_id, initialized.grant_id)
            .unwrap());
        assert!(!store
            .register_fixed_jj_trigger(initialized.watch_id, initialized.grant_id)
            .unwrap());
        assert!(store
            .register_fixed_jj_trigger(initialized.watch_id, initialized.grant_id)
            .unwrap());
        let binding = ViewBinding {
            monitor_session_id: [40; 16],
            root_path: request.source_path.clone(),
            fs_uuid: request.fs_uuid,
            subvol_uuid: request.source_subvol_uuid,
            mount_ns_dev: 1,
            mount_ns_ino: 2,
            process_root_dev: 3,
            process_root_ino: 4,
            process_root_mnt_id: 5,
            watched_root_dev: 6,
            watched_root_ino: 7,
            watched_root_mnt_id: 8,
        };
        store
            .activate_snapshot_facade(initialized.watch_id, initialized.grant_id, &binding)
            .unwrap();
        assert_eq!(
            store
                .active_fixed_jj_trigger_watches(request.requester_uid)
                .unwrap(),
            vec![initialized.watch_id]
        );
        let run = store
            .claim_fixed_jj_trigger([41; 16], 400, 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(run.through_sequence, 0);
        assert!(store
            .claim_fixed_jj_trigger([42; 16], 500, 1_100)
            .unwrap()
            .is_none());
        store.finish_fixed_jj_trigger(&run, true).unwrap();
        assert!(store
            .claim_fixed_jj_trigger([42; 16], 600, 1_200)
            .unwrap()
            .is_none());
        assert!(store
            .delete_fixed_jj_trigger(initialized.watch_id, initialized.grant_id)
            .unwrap());
        assert!(!store
            .delete_fixed_jj_trigger(initialized.watch_id, initialized.grant_id)
            .unwrap());
        assert!(store
            .active_fixed_jj_trigger_watches(request.requester_uid)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn trigger_claim_prefers_the_least_run_watch_for_failure_fairness() {
        let (_temp, mut store, mut first_request) = setup();
        first_request.permissions =
            Permissions::new(PERMISSION_READ | PERMISSION_CUT | PERMISSION_TRIGGER).unwrap();
        let (_first_reservation, first) = initialize_watch(&mut store, &first_request);

        let mut second_request = first_request.clone();
        second_request.source_subvol_uuid = [22; 16];
        second_request.source_path = b"/source-two".to_vec();
        second_request.reserved_snapshot_path = b"/store/snapshots/w2/s-0-op".to_vec();
        second_request.lease_owner = [23; 16];
        let second_reservation = store.reserve_initialize(&second_request).unwrap();
        store
            .start_initialize_filesystem_effect(
                &second_reservation,
                second_request.lease_owner,
                150,
            )
            .unwrap();
        let mut second_snapshot = snapshot(&second_request);
        second_snapshot.subvol_uuid = [24; 16];
        second_snapshot.root_id = 902;
        let second_recorded = store
            .record_initialize_snapshot(
                &second_reservation,
                second_request.lease_owner,
                &second_snapshot,
                200,
            )
            .unwrap();
        let second = store
            .publish_initial_checkpoint(
                &second_reservation,
                second_request.lease_owner,
                &second_recorded,
                &index(),
                300,
            )
            .unwrap();

        for (initialized, request, session) in [
            (&first, &first_request, [50; 16]),
            (&second, &second_request, [51; 16]),
        ] {
            let binding = ViewBinding {
                monitor_session_id: session,
                root_path: request.source_path.clone(),
                fs_uuid: request.fs_uuid,
                subvol_uuid: request.source_subvol_uuid,
                mount_ns_dev: 1,
                mount_ns_ino: 2,
                process_root_dev: 3,
                process_root_ino: 4,
                process_root_mnt_id: 5,
                watched_root_dev: 6,
                watched_root_ino: 7,
                watched_root_mnt_id: 8,
            };
            store
                .activate_snapshot_facade(initialized.watch_id, initialized.grant_id, &binding)
                .unwrap();
            store
                .register_fixed_jj_trigger(initialized.watch_id, initialized.grant_id)
                .unwrap();
        }
        store
            .connection_mut()
            .execute(
                "UPDATE watchman_triggers SET run_fence = 10 WHERE watch_id = ?1",
                [first.watch_id.as_slice()],
            )
            .unwrap();
        let claimed = store
            .claim_fixed_jj_trigger([52; 16], 400, 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.watch_id, second.watch_id);
    }

    #[test]
    fn publishes_adjacent_delta_as_overlay_and_preserves_directory_witness() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let cut_request = CutRequest {
            watch_id: initialized.watch_id,
            authorization_id: initialized.grant_id,
            reserved_snapshot_path: b"/store/snapshots/w/s-1-op".to_vec(),
            requester_uid: 1000,
            requester_gid: 1000,
            lease_owner: [6; 16],
            now_ns: 400,
            lease_expires_ns: 2000,
        };
        let cut = store.reserve_cut(&cut_request).unwrap();
        assert_eq!(cut.sequence, 1);
        let first_admission = store
            .admit_planned_cut(
                initialized.watch_id,
                initialized.grant_id,
                [70; 16],
                "query",
                410,
                2_000,
            )
            .unwrap()
            .unwrap();
        let second_admission = store
            .admit_planned_cut(
                initialized.watch_id,
                initialized.grant_id,
                [71; 16],
                "clock",
                420,
                2_000,
            )
            .unwrap()
            .unwrap();
        assert_eq!(first_admission.reservation.operation_id, cut.operation_id);
        assert_eq!(second_admission.reservation.operation_id, cut.operation_id);
        store
            .start_cut_filesystem_effect(&cut, cut_request.lease_owner, 450)
            .unwrap();
        assert!(store
            .admit_planned_cut(
                initialized.watch_id,
                initialized.grant_id,
                [72; 16],
                "query",
                460,
                2_000,
            )
            .unwrap()
            .is_none());
        let recorded = store
            .record_cut_snapshot(
                &cut,
                cut_request.lease_owner,
                &cut_snapshot(&cut_request, cut.source_subvol_uuid),
                500,
            )
            .unwrap();
        store
            .publish_validated_physical_cut(&cut, cut_request.lease_owner, &recorded, 550)
            .unwrap();

        let root = object(ROOT_INO, MODE_DIRECTORY | 0o755, 1);
        let new_file = object(301, 0o100644, 1);
        let manifest = ChangedObjectsManifest {
            objects: [
                (
                    ROOT_INO,
                    ObjectChange {
                        ino: ROOT_INO,
                        old_generation: Some(root.generation),
                        new_generation: Some(root.generation),
                        change_mask: CHANGE_INODE,
                    },
                ),
                (
                    300,
                    ObjectChange {
                        ino: 300,
                        old_generation: Some(301),
                        new_generation: Some(301),
                        change_mask: CHANGE_FILE_DATA,
                    },
                ),
                (
                    301,
                    ObjectChange {
                        ino: 301,
                        old_generation: None,
                        new_generation: Some(new_file.generation),
                        change_mask: CHANGE_INODE | CHANGE_REF | CHANGE_CREATED,
                    },
                ),
            ]
            .into(),
            ref_adds: [Reference {
                ino: 301,
                parent_ino: ROOT_INO,
                name: b"new".to_vec(),
            }]
            .into(),
            ref_deletes: Default::default(),
            raw_ref_adds: 1,
            raw_ref_deletes: 0,
        };
        let target_objects = [(ROOT_INO, root), (301, new_file)].into();
        let published = store
            .publish_adjacent_delta(
                &cut,
                cut_request.lease_owner,
                &recorded,
                &manifest,
                &target_objects,
                600,
            )
            .unwrap();
        assert_eq!(published.sequence, 1);
        assert_eq!(
            store
                .poll_cut_admission(&first_admission, 610)
                .unwrap()
                .unwrap(),
            published
        );
        assert_eq!(
            store
                .poll_cut_admission(&second_admission, 610)
                .unwrap()
                .unwrap(),
            published
        );
        assert!(published
            .events
            .iter()
            .any(|event| event.kind == EventKind::DirectoryDirtyWitness));
        let full_revision = store.load_revision(published.revision_id).unwrap();
        assert_eq!(full_revision.paths(301).unwrap(), vec![b"new".to_vec()]);
        let stored_metadata = store.load_revision_metadata(published.revision_id).unwrap();
        let full_safety = full_revision.safety_summary();
        assert_eq!(
            stored_metadata.object_count,
            i64::try_from(full_revision.objects.len()).unwrap()
        );
        assert_eq!(
            stored_metadata.ref_count,
            i64::try_from(full_revision.references.len()).unwrap()
        );
        assert_eq!(stored_metadata.state_hash, full_revision.state_hash());
        assert_eq!(
            stored_metadata.single_owner_uid,
            full_safety.single_owner_uid
        );
        assert_eq!(
            stored_metadata.privileged_metadata_count,
            i64::try_from(full_safety.privileged_metadata_count).unwrap()
        );
        assert_eq!(
            stored_metadata.security_state_hash,
            full_safety.security_state_hash
        );
        let full_owner_counts = index_owner_counts(&full_revision).unwrap();
        assert_eq!(
            stored_metadata.owner_cardinality,
            i64::try_from(full_owner_counts.len()).unwrap()
        );
        assert_eq!(
            stored_metadata.owner_uid_xor,
            owner_uid_xor(&full_owner_counts)
        );
        let preserved_witness: i64 = store
            .connection()
            .query_row(
                r#"SELECT count(*) FROM comparison_objects
                    WHERE comparison_id = ?1 AND ino = ?2"#,
                params![published.comparison_id, encode_u64(ROOT_INO).as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved_witness, 1);
        let staged_rows: i64 = store
            .connection()
            .query_row(
                "SELECT (SELECT count(*) FROM delta_stage_objects) + \
                        (SELECT count(*) FROM delta_stage_refs) + \
                        (SELECT count(*) FROM delta_stage_events) + \
                        (SELECT count(*) FROM delta_stage_object_overrides) + \
                        (SELECT count(*) FROM delta_stage_ref_overrides) + \
                        (SELECT count(*) FROM delta_stage_owner_counts)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(staged_rows, 0);
        store
            .connection_mut()
            .execute(
                "UPDATE watch_grants SET permissions = ?2 WHERE id = ?1",
                params![initialized.grant_id.as_slice(), i64::from(PERMISSION_READ)],
            )
            .unwrap();
        let historical = store
            .replay_historical_changes(
                initialized.watch_id,
                initialized.grant_id,
                1000,
                [4; 16],
                [5; 16],
            )
            .unwrap();
        assert_eq!((historical.from_sequence, historical.to_sequence), (0, 1));
        assert!(!historical.fresh_instance);
        assert_eq!(historical.events, published.events);
        let empty = store
            .replay_historical_changes(
                initialized.watch_id,
                initialized.grant_id,
                1000,
                [5; 16],
                [5; 16],
            )
            .unwrap();
        assert!(!empty.fresh_instance);
        assert!(empty.events.is_empty());
        assert!(store
            .replay_historical_changes(
                initialized.watch_id,
                initialized.grant_id,
                1000,
                [99; 16],
                [5; 16],
            )
            .is_err());
        let expected_after_compaction = store.load_revision(published.revision_id).unwrap();
        store
            .connection_mut()
            .execute(
                "UPDATE watch_grants SET permissions = ?2 WHERE id = ?1",
                params![
                    initialized.grant_id.as_slice(),
                    i64::from(PERMISSION_READ | PERMISSION_RETAIN)
                ],
            )
            .unwrap();
        let _source_retention = store
            .create_retention_lease(
                initialized.watch_id,
                initialized.grant_id,
                initialized.snapshot_id,
                650,
                2_000,
            )
            .unwrap();
        store
            .connection_mut()
            .execute(
                r#"INSERT INTO query_leases(
                       id, watch_id, authorization_id, clock_epoch,
                       from_cut_sequence, to_cut_sequence,
                       guard_epoch, from_guard_sequence, to_guard_sequence,
                       lease_owner, lease_fence, lease_expires_ns, state
                   )
                   SELECT ?1, w.id, ?2, w.clock_epoch, 0, 1,
                          NULL, NULL, NULL, ?3, 1, 750, 'active'
                     FROM watches w WHERE w.id = ?4"#,
                params![
                    [74_u8; 16].as_slice(),
                    initialized.grant_id.as_slice(),
                    [75_u8; 16].as_slice(),
                    initialized.watch_id.as_slice(),
                ],
            )
            .unwrap();
        assert!(store
            .advance_replay_floor(initialized.watch_id, 1, 700, [73; 16])
            .is_err());
        let reclaimed = store
            .advance_replay_floor(initialized.watch_id, 1, 800, [73; 16])
            .unwrap();
        assert_eq!(reclaimed, 0);
        assert_eq!(
            store.load_revision(published.revision_id).unwrap(),
            expected_after_compaction
        );
        let (depth, event_count, replay_floor): (i64, i64, i64) = store
            .connection()
            .query_row(
                "SELECT r.delta_depth, \
                        (SELECT count(*) FROM change_events WHERE comparison_id = ?2), \
                        w.replay_floor_seq \
                   FROM revisions r JOIN watches w ON w.indexed_revision_id = r.id \
                  WHERE r.id = ?1",
                params![published.revision_id, published.comparison_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((depth, event_count, replay_floor), (0, 0, 1));
        let reclaimed_history = store
            .replay_historical_changes(
                initialized.watch_id,
                initialized.grant_id,
                1000,
                [4; 16],
                [5; 16],
            )
            .unwrap();
        assert!(reclaimed_history.fresh_instance);
        assert!(reclaimed_history.events.is_empty());
        let claim = match store
            .claim_historical_comparison(&HistoricalComparisonRequest {
                watch_id: initialized.watch_id,
                authorization_id: initialized.grant_id,
                requester_uid: 1000,
                from_snapshot_uuid: [4; 16],
                to_snapshot_uuid: [5; 16],
                lease_owner: [76; 16],
                now_ns: 900,
                lease_expires_ns: 2_000,
            })
            .unwrap()
        {
            HistoricalComparisonAdmission::Claimed(claim) => claim,
            HistoricalComparisonAdmission::Ready(_) => panic!("direct comparison was not built"),
        };
        let direct = store
            .publish_historical_comparison(&claim, &manifest, &target_objects, 950)
            .unwrap();
        assert!(!direct.fresh_instance);
        assert_eq!(direct.events, published.events);
        let cached = store
            .claim_historical_comparison(&HistoricalComparisonRequest {
                watch_id: initialized.watch_id,
                authorization_id: initialized.grant_id,
                requester_uid: 1000,
                from_snapshot_uuid: [4; 16],
                to_snapshot_uuid: [5; 16],
                lease_owner: [77; 16],
                now_ns: 1_000,
                lease_expires_ns: 2_000,
            })
            .unwrap();
        assert_eq!(cached, HistoricalComparisonAdmission::Ready(direct));
        assert!(store.foreign_key_violations().unwrap().is_empty());
    }

    #[test]
    fn admission_and_batch_close_are_writer_serialized_across_connections() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let cut_request = CutRequest {
            watch_id: initialized.watch_id,
            authorization_id: initialized.grant_id,
            reserved_snapshot_path: b"/store/snapshots/w/race".to_vec(),
            requester_uid: 1000,
            requester_gid: 1000,
            lease_owner: [80; 16],
            now_ns: 400,
            lease_expires_ns: 2_000,
        };
        let cut = store.reserve_cut(&cut_request).unwrap();
        let mut second = Store::open(store.path()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let admission_barrier = barrier.clone();
        let watch_id = initialized.watch_id;
        let grant_id = initialized.grant_id;
        let admission_thread = std::thread::spawn(move || {
            admission_barrier.wait();
            store
                .admit_planned_cut(watch_id, grant_id, [81; 16], "query", 410, 2_000)
                .unwrap()
        });
        let close_barrier = barrier;
        let close_cut = cut.clone();
        let close_thread = std::thread::spawn(move || {
            close_barrier.wait();
            second
                .start_cut_filesystem_effect(&close_cut, [80; 16], 420)
                .unwrap();
            second
        });
        let admission = admission_thread.join().unwrap();
        let second = close_thread.join().unwrap();
        let operation_state: String = second
            .connection()
            .query_row(
                "SELECT state FROM operations WHERE id = ?1",
                [cut.operation_id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(operation_state, "fs_started");
        if let Some(admission) = admission {
            assert_eq!(admission.reservation.operation_id, cut.operation_id);
        }
    }

    #[test]
    fn terminal_gap_is_rebuilt_as_a_fenced_full_fresh_checkpoint() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let first_request = CutRequest {
            watch_id: initialized.watch_id,
            authorization_id: initialized.grant_id,
            reserved_snapshot_path: b"/store/snapshots/w/s-1-op".to_vec(),
            requester_uid: 1000,
            requester_gid: 1000,
            lease_owner: [6; 16],
            now_ns: 400,
            lease_expires_ns: 2000,
        };
        let first = store.reserve_cut(&first_request).unwrap();
        store
            .start_cut_filesystem_effect(&first, first_request.lease_owner, 450)
            .unwrap();
        let first_snapshot = store
            .record_cut_snapshot(
                &first,
                first_request.lease_owner,
                &cut_snapshot(&first_request, first.source_subvol_uuid),
                500,
            )
            .unwrap();
        store
            .publish_validated_physical_cut(&first, first_request.lease_owner, &first_snapshot, 550)
            .unwrap();
        store
            .fail_cut_comparison(&first, first_request.lease_owner, "invalid delta", 575)
            .unwrap();
        let second_request = CutRequest {
            reserved_snapshot_path: b"/store/snapshots/w/s-2-op".to_vec(),
            lease_owner: [7; 16],
            now_ns: 600,
            lease_expires_ns: 2200,
            ..first_request
        };
        let second = store.reserve_cut(&second_request).unwrap();
        assert_eq!(second.sequence, 2);
        assert_eq!(second.base_snapshot_id, first_snapshot.snapshot_id);
        let indexed_seq: i64 = store
            .connection()
            .query_row(
                "SELECT indexed_seq FROM watches WHERE id = ?1",
                [initialized.watch_id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed_seq, 0);

        store
            .start_cut_filesystem_effect(&second, second_request.lease_owner, 650)
            .unwrap();
        let mut second_identity = cut_snapshot(&second_request, second.source_subvol_uuid);
        second_identity.subvol_uuid = [6; 16];
        second_identity.root_id = 902;
        second_identity.ctransid = 12;
        second_identity.otransid = 11;
        second_identity.created_ns = 700;
        let second_snapshot = store
            .record_cut_snapshot(&second, second_request.lease_owner, &second_identity, 700)
            .unwrap();
        store
            .publish_validated_physical_cut(
                &second,
                second_request.lease_owner,
                &second_snapshot,
                750,
            )
            .unwrap();
        let published = store
            .publish_full_fresh_checkpoint(
                &second,
                second_request.lease_owner,
                &second_snapshot,
                &index(),
                800,
            )
            .unwrap();
        assert_eq!(published.sequence, 2);
        assert_eq!(store.load_revision(published.revision_id).unwrap(), index());
        let states: (String, String, i64, i64, i64) = store
            .connection()
            .query_row(
                r#"SELECT first.state, second.state, second.fresh_instance,
                          w.indexed_seq, r.delta_depth
                     FROM watch_cuts first
                     JOIN watch_cuts second ON second.watch_id = first.watch_id
                     JOIN watches w ON w.id = first.watch_id
                     JOIN revisions r ON r.id = w.indexed_revision_id
                    WHERE first.watch_id = ?1
                      AND first.sequence = 1 AND second.sequence = 2"#,
                [initialized.watch_id.as_slice()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(states, ("failed".into(), "ready".into(), 1, 2, 0));
        let history = store
            .replay_historical_changes(
                initialized.watch_id,
                initialized.grant_id,
                1000,
                [4; 16],
                [6; 16],
            )
            .unwrap();
        assert!(history.fresh_instance);
        assert!(history.events.is_empty());
        assert!(store.foreign_key_violations().unwrap().is_empty());
    }
}
