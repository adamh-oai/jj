use crate::btrfs::SubvolumeInfo;
use crate::index::{Event, EventKind};
use crate::manifest::ChangedObjectsManifest;
use crate::namespace::ViewBinding;
use crate::store::{Store, StoreError, decode_u64, encode_u64};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const PERMISSION_READ: u8 = 0x01;
pub const PERMISSION_CUT: u8 = 0x02;
pub const PERMISSION_RETAIN: u8 = 0x10;
pub const PERMISSION_ADMIN: u8 = 0x20;
pub const PERMISSION_MASK: u8 = 0x33;

const REPLAY_SPOOL_TRAILER_MAGIC: &[u8; 16] = b"awacs-replay-v1\0";
const REPLAY_SPOOL_TRAILER_LEN: usize = 16 + 8 + 32;

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
    pub snapshot_identity: SnapshotIdentity,
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

type WatchGrantIds = ([u8; 16], [u8; 16]);
type EncodedMutationHint<'a> = (&'a str, Option<&'a [u8]>, Option<[u8; 8]>, Option<[u8; 8]>);
type GuardBoundaryRow = (i64, Vec<u8>, String, Option<Vec<u8>>, Option<i64>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryReport {
    pub invalidated_facades: usize,
    pub released_queries: usize,
    pub abandoned_historical_comparisons: usize,
    pub boot_changed: bool,
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

    /// Rebinds an authorized active watch after its live subvolume was renamed.
    ///
    /// A successful subvolume migration swaps a staging path into the final
    /// repository path without changing the Btrfs subvolume UUID. The durable
    /// watch follows that identity; its path is only the current namespace
    /// binding used to arm the facade monitor.
    pub fn rebind_active_uid_watch_path_by_subvolume(
        &mut self,
        fs_uuid: [u8; 16],
        live_subvol_uuid: [u8; 16],
        live_path: &[u8],
        requester_uid: u32,
        required_permissions: u8,
    ) -> Result<Option<WatchGrantIds>, ManagerError> {
        if !path_is_absolute(live_path)
            || required_permissions == 0
            || required_permissions & !PERMISSION_MASK != 0
        {
            return Err(ManagerError::new("invalid active-watch rebind"));
        }
        let principal = encode_u64(u64::from(requester_uid));
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(Vec<u8>, Vec<u8>)> = transaction
            .query_row(
                r#"SELECT w.id, g.id
                     FROM watches w
                     JOIN watch_grants g ON g.watch_id = w.id
                     JOIN filesystems f ON f.id = w.filesystem_id
                    WHERE f.fs_uuid = ?1 AND w.live_subvol_uuid = ?2
                      AND w.state = 'active'
                      AND g.principal_kind = 'uid' AND g.principal_id = ?3
                      AND g.state = 'active' AND (g.permissions & ?4) = ?4
                    ORDER BY g.created_ns DESC LIMIT 1"#,
                params![
                    fs_uuid.as_slice(),
                    live_subvol_uuid.as_slice(),
                    principal.as_slice(),
                    i64::from(required_permissions),
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((watch, grant)) = row else {
            transaction.commit()?;
            return Ok(None);
        };
        require_one(
            transaction.execute(
                "UPDATE watches SET live_path = ?2 WHERE id = ?1 AND state = 'active'",
                params![watch.as_slice(), live_path],
            )?,
            "rebind active watch path",
        )?;
        transaction.commit()?;
        Ok(Some((
            fixed_manager_blob(&watch, "watch ID")?,
            fixed_manager_blob(&grant, "grant ID")?,
        )))
    }

    /// Returns the newest ready revision whose physical tree is the parent of
    /// a freshly-created Btrfs snapshot, or whose active watch is the live
    /// parent subvolume. Callers copy this immutable index into a new
    /// child-owned checkpoint; they must not point the child watch at the
    /// parent's revision or snapshot.
    pub fn descendant_seed_revision(
        &self,
        fs_uuid: [u8; 16],
        parent_uuid: [u8; 16],
    ) -> Result<Option<i64>, ManagerError> {
        self.connection()
            .query_row(
                r#"SELECT revision_id
                     FROM (
                           SELECT r.id AS revision_id
                             FROM snapshots s
                             JOIN revisions r ON r.snapshot_id = s.id
                             JOIN filesystems f ON f.id = s.filesystem_id
                            WHERE f.fs_uuid = ?1 AND s.subvol_uuid = ?2
                              AND s.physical_state = 'present' AND r.state = 'ready'
                           UNION ALL
                           SELECT r.id AS revision_id
                             FROM watches w
                             JOIN revisions r ON r.id = w.indexed_revision_id
                             JOIN snapshots s ON s.id = r.snapshot_id
                             JOIN filesystems f ON f.id = w.filesystem_id
                            WHERE f.fs_uuid = ?1 AND w.live_subvol_uuid = ?2
                              AND w.state = 'active' AND r.state = 'ready'
                              AND s.physical_state = 'present'
                          )
                 ORDER BY revision_id DESC LIMIT 1"#,
                params![fs_uuid.as_slice(), parent_uuid.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(ManagerError::from)
    }

    /// Returns the ready revision for one exact inherited immutable baseline.
    ///
    /// A delayed consumer copies its semantic tree while holding the source
    /// working-copy lock, then names that exact snapshot in its handoff. The
    /// source watch may advance independently after the filesystem clone, so
    /// descendant bootstrap must not silently substitute a newer path map.
    pub fn descendant_seed_revision_for_baseline(
        &self,
        fs_uuid: [u8; 16],
        baseline_snapshot_uuid: [u8; 16],
    ) -> Result<Option<i64>, ManagerError> {
        self.connection()
            .query_row(
                r#"SELECT r.id
                     FROM revisions r
                     JOIN snapshots s ON s.id = r.snapshot_id
                     JOIN filesystems f ON f.id = s.filesystem_id
                    WHERE f.fs_uuid = ?1 AND s.subvol_uuid = ?2
                      AND s.physical_state = 'present' AND r.state = 'ready'
                    ORDER BY r.id DESC LIMIT 1"#,
                params![fs_uuid.as_slice(), baseline_snapshot_uuid.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(ManagerError::from)
    }

    /// Returns an exact inherited baseline only when a durable consumer still
    /// pins its physical snapshot.
    ///
    /// Git-mediated delayed adoption validates this before Git creates a
    /// branch or linked-worktree registration. A ready metadata row alone is
    /// insufficient: direct snapshot GC may legally delete an unpinned
    /// physical baseline between preflight and descendant initialization.
    pub fn retained_consumer_seed_revision_for_baseline(
        &self,
        fs_uuid: [u8; 16],
        baseline_snapshot_uuid: [u8; 16],
    ) -> Result<Option<i64>, ManagerError> {
        self.connection()
            .query_row(
                r#"SELECT r.id
                     FROM revisions r
                     JOIN snapshots s ON s.id = r.snapshot_id
                     JOIN filesystems f ON f.id = s.filesystem_id
                    WHERE f.fs_uuid = ?1 AND s.subvol_uuid = ?2
                      AND s.physical_state = 'present' AND r.state = 'ready'
                      AND EXISTS (
                          SELECT 1 FROM snapshot_pins p
                           WHERE p.snapshot_id = s.id
                             AND p.owner_kind = 'consumer-baseline')
                    ORDER BY r.id DESC LIMIT 1"#,
                params![fs_uuid.as_slice(), baseline_snapshot_uuid.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(ManagerError::from)
    }

    pub fn baseline_physical_state(
        &self,
        fs_uuid: [u8; 16],
        baseline_snapshot_uuid: [u8; 16],
    ) -> Result<Option<String>, ManagerError> {
        self.connection()
            .query_row(
                r#"SELECT s.physical_state
                     FROM snapshots s
                     JOIN filesystems f ON f.id = s.filesystem_id
                    WHERE f.fs_uuid = ?1 AND s.subvol_uuid = ?2
                    ORDER BY s.id DESC LIMIT 1"#,
                params![fs_uuid.as_slice(), baseline_snapshot_uuid.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(ManagerError::from)
    }

    /// Returns one exact ready inherited baseline, atomically adding the
    /// source JJ consumer's committed pin when upgrading pre-pin state.
    ///
    /// Older clean JJ journals can name a still-present immutable baseline
    /// without a consumer-baseline row. Git-mediated adoption runs before the
    /// next JJ scan, so it must perform the same one-time migration while
    /// holding the manager writer transaction; a check followed by a later
    /// pin would race snapshot GC.
    pub fn retain_existing_consumer_seed_revision_for_baseline(
        &mut self,
        fs_uuid: [u8; 16],
        baseline_snapshot_uuid: [u8; 16],
        owner_id: [u8; 16],
    ) -> Result<Option<i64>, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate: Option<(i64, i64)> = transaction
            .query_row(
                r#"SELECT r.id, s.id
                     FROM revisions r
                     JOIN snapshots s ON s.id = r.snapshot_id
                     JOIN filesystems f ON f.id = s.filesystem_id
                    WHERE f.fs_uuid = ?1 AND s.subvol_uuid = ?2
                      AND s.physical_state = 'present' AND r.state = 'ready'
                    ORDER BY r.id DESC LIMIT 1"#,
                params![fs_uuid.as_slice(), baseline_snapshot_uuid.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((revision_id, snapshot_id)) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };
        let retained: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM snapshot_pins WHERE snapshot_id = ?1 AND owner_kind = 'consumer-baseline' LIMIT 1",
                [snapshot_id],
                |row| row.get(0),
            )
            .optional()?;
        if retained.is_none() {
            transaction.execute(
                "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) VALUES (?1, 'consumer-baseline', ?2, 'committed')",
                params![snapshot_id, owner_id.as_slice()],
            )?;
        }
        transaction.commit()?;
        Ok(Some(revision_id))
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
            "DELETE FROM operations WHERE kind = 'cut' AND state = 'planned'",
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
                       AND o.kind = 'initialize'
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
        // These ephemeral historical-read jobs are owned by one manager
        // process. A restart invalidates their publication fence, but their
        // endpoint pins must also be released so they cannot leak retained
        // snapshots. Canonical algorithm-v2 cut comparisons are recovered by
        // their cut operation instead of being abandoned here.
        let historical_comparison_ids = {
            let mut statement = transaction.prepare(
                "SELECT id FROM comparisons \
                  WHERE algorithm_version = 3 AND state = 'claimed'",
            )?;
            statement
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
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
                WHERE algorithm_version = 3 AND state = 'claimed'"#,
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
            abandoned_historical_comparisons,
            boot_changed: previous_boot_id != current_boot_id,
        })
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
            "DELETE FROM operations WHERE watch_id = ?1 AND authorization_id = ?2 \
             AND kind = 'cut' AND state = 'planned'",
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
                   cut_operation_id, clock_epoch,
                   guard_epoch, guard_sequence, guard_complete
               ) VALUES (?1, ?2, ?3, 'cut', ?4, ?5, ?6, ?7, ?8)"#,
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

    /// Releases one daemon-free scan fence after another short-lived caller
    /// may already have activated a newer facade epoch.
    ///
    /// The lease row itself is the authority here: its random ID, owner, and
    /// fence are sufficient to identify the in-flight scan. Requiring the
    /// watch's current monitor session would make two overlapping direct
    /// callers invalidate each other's otherwise usable open snapshot fds.
    pub fn release_query_lease_direct(
        &mut self,
        lease: &QueryLeaseReservation,
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: Option<i64> = transaction
            .query_row(
                r#"SELECT 1 FROM query_leases
                    WHERE id = ?1 AND watch_id = ?2
                      AND authorization_id = ?3 AND clock_epoch = ?4
                      AND lease_owner = ?5 AND lease_fence = ?6
                      AND state = 'active'"#,
                params![
                    lease.id.as_slice(),
                    lease.watch_id.as_slice(),
                    lease.authorization_id.as_slice(),
                    lease.clock_epoch.as_slice(),
                    lease.lease_owner.as_slice(),
                    lease.lease_fence,
                ],
                |row| row.get(0),
            )
            .optional()?;
        if valid != Some(1) {
            return Err(ManagerError::new(
                "direct query lease response fence is stale",
            ));
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
            "release direct query lease",
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn renew_query_lease(
        &mut self,
        lease: &QueryLeaseReservation,
        activation: &FacadeActivation,
        now_ns: i64,
        lease_expires_ns: i64,
    ) -> Result<(), ManagerError> {
        if lease_expires_ns <= now_ns {
            return Err(ManagerError::new(
                "renewed query lease must expire after renewal",
            ));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_one(
            transaction.execute(
                r#"UPDATE query_leases
                      SET lease_expires_ns = ?8
                    WHERE id = ?1 AND watch_id = ?2
                      AND authorization_id = ?3 AND clock_epoch = ?4
                      AND lease_owner = ?5 AND lease_fence = ?6
                      AND state = 'active' AND lease_expires_ns > ?9
                      AND EXISTS (
                          SELECT 1 FROM watches w JOIN watch_grants g
                            ON g.id = query_leases.authorization_id
                           AND g.watch_id = query_leases.watch_id
                           AND g.state = 'active'
                           WHERE w.id = query_leases.watch_id
                             AND w.state = 'active'
                             AND w.clock_epoch = query_leases.clock_epoch
                             AND w.view_monitor_session_id = ?7
                      )"#,
                params![
                    lease.id.as_slice(),
                    lease.watch_id.as_slice(),
                    lease.authorization_id.as_slice(),
                    lease.clock_epoch.as_slice(),
                    lease.lease_owner.as_slice(),
                    lease.lease_fence,
                    activation.monitor_session_id.as_slice(),
                    lease_expires_ns,
                    now_ns,
                ],
            )?,
            "renew query lease",
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Renews one daemon-free scan fence without requiring its old facade
    /// epoch to remain the watch's latest activation.
    pub fn renew_query_lease_direct(
        &mut self,
        lease: &QueryLeaseReservation,
        now_ns: i64,
        lease_expires_ns: i64,
    ) -> Result<(), ManagerError> {
        if lease_expires_ns <= now_ns {
            return Err(ManagerError::new(
                "renewed query lease must expire after renewal",
            ));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_one(
            transaction.execute(
                r#"UPDATE query_leases
                      SET lease_expires_ns = ?7
                    WHERE id = ?1 AND watch_id = ?2
                      AND authorization_id = ?3 AND clock_epoch = ?4
                      AND lease_owner = ?5 AND lease_fence = ?6
                      AND state = 'active' AND lease_expires_ns > ?8
                      AND EXISTS (
                          SELECT 1 FROM watches w JOIN watch_grants g
                            ON g.id = query_leases.authorization_id
                           AND g.watch_id = query_leases.watch_id
                           AND g.state = 'active'
                           WHERE w.id = query_leases.watch_id
                             AND w.state = 'active'
                      )"#,
                params![
                    lease.id.as_slice(),
                    lease.watch_id.as_slice(),
                    lease.authorization_id.as_slice(),
                    lease.clock_epoch.as_slice(),
                    lease.lease_owner.as_slice(),
                    lease.lease_fence,
                    lease_expires_ns,
                    now_ns,
                ],
            )?,
            "renew direct query lease",
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Releases expired direct-query leases and their revision/comparison
    /// pins in one writer transaction.
    pub fn expire_query_leases(&mut self, now_ns: i64) -> Result<usize, ManagerError> {
        self.expire_query_leases_inner(now_ns, None)
    }

    /// Releases at most `limit` expired direct-query leases and their pins in
    /// one writer transaction. Production maintenance uses this bounded form
    /// so an abandoned-client backlog cannot monopolize one tick.
    pub fn expire_query_leases_bounded(
        &mut self,
        now_ns: i64,
        limit: usize,
    ) -> Result<usize, ManagerError> {
        if limit == 0 {
            return Err(ManagerError::new(
                "query lease expiry limit must be positive",
            ));
        }
        self.expire_query_leases_inner(now_ns, Some(limit))
    }

    fn expire_query_leases_inner(
        &mut self,
        now_ns: i64,
        limit: Option<usize>,
    ) -> Result<usize, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expired: i64;
        if let Some(limit) = limit {
            let limit = i64::try_from(limit)
                .map_err(|_| ManagerError::new("query lease expiry limit overflow"))?;
            expired = transaction.query_row(
                r#"SELECT count(*) FROM (
                       SELECT id FROM query_leases
                        WHERE state = 'active' AND lease_expires_ns <= ?1
                        ORDER BY id LIMIT ?2
                   )"#,
                params![now_ns, limit],
                |row| row.get(0),
            )?;
            transaction.execute(
                r#"DELETE FROM query_revision_pins WHERE query_id IN (
                       SELECT id FROM query_leases
                        WHERE state = 'active' AND lease_expires_ns <= ?1
                        ORDER BY id LIMIT ?2
                   )"#,
                params![now_ns, limit],
            )?;
            transaction.execute(
                r#"DELETE FROM query_comparison_pins WHERE query_id IN (
                       SELECT id FROM query_leases
                        WHERE state = 'active' AND lease_expires_ns <= ?1
                        ORDER BY id LIMIT ?2
                   )"#,
                params![now_ns, limit],
            )?;
            transaction.execute(
                r#"UPDATE query_leases
                      SET state = 'released', lease_fence = lease_fence + 1
                    WHERE id IN (
                       SELECT id FROM query_leases
                        WHERE state = 'active' AND lease_expires_ns <= ?1
                        ORDER BY id LIMIT ?2
                    )"#,
                params![now_ns, limit],
            )?;
        } else {
            expired = transaction.query_row(
                "SELECT count(*) FROM query_leases WHERE state = 'active' AND lease_expires_ns <= ?1",
                [now_ns],
                |row| row.get(0),
            )?;
            transaction.execute(
                r#"DELETE FROM query_revision_pins WHERE query_id IN (
                       SELECT id FROM query_leases
                        WHERE state = 'active' AND lease_expires_ns <= ?1
                   )"#,
                [now_ns],
            )?;
            transaction.execute(
                r#"DELETE FROM query_comparison_pins WHERE query_id IN (
                       SELECT id FROM query_leases
                        WHERE state = 'active' AND lease_expires_ns <= ?1
                   )"#,
                [now_ns],
            )?;
            transaction.execute(
                r#"UPDATE query_leases
                      SET state = 'released', lease_fence = lease_fence + 1
                    WHERE state = 'active' AND lease_expires_ns <= ?1"#,
                [now_ns],
            )?;
        }
        transaction.commit()?;
        usize::try_from(expired).map_err(|_| ManagerError::new("expired query count overflow"))
    }

    /// Fails at most `limit` expired ephemeral historical comparisons and
    /// releases their endpoint pins. A worker that outlives its lease will
    /// subsequently fail its publication fence instead of keeping snapshots
    /// pinned until restart.
    pub fn expire_historical_comparisons_bounded(
        &mut self,
        now_ns: i64,
        limit: usize,
    ) -> Result<usize, ManagerError> {
        if limit == 0 {
            return Err(ManagerError::new(
                "historical comparison expiry limit must be positive",
            ));
        }
        let limit = i64::try_from(limit)
            .map_err(|_| ManagerError::new("historical comparison expiry limit overflow"))?;
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let comparison_ids = {
            let mut statement = transaction.prepare(
                r#"SELECT id FROM comparisons
                    WHERE comparison_kind = 'incremental'
                      AND algorithm_version = 3 AND state = 'claimed'
                      AND lease_expires_ns <= ?1
                    ORDER BY id LIMIT ?2"#,
            )?;
            statement
                .query_map(params![now_ns, limit], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for comparison_id in &comparison_ids {
            let owner_id = encode_u64(
                u64::try_from(*comparison_id)
                    .map_err(|_| ManagerError::new("comparison ID is negative"))?,
            );
            transaction.execute(
                "DELETE FROM snapshot_pins WHERE owner_kind = 'comparison' AND owner_id = ?1",
                [owner_id.as_slice()],
            )?;
            require_one(
                transaction.execute(
                    r#"UPDATE comparisons
                          SET state = 'failed', lease_owner = NULL,
                              lease_expires_ns = NULL, lease_fence = lease_fence + 1
                        WHERE id = ?1 AND comparison_kind = 'incremental'
                          AND algorithm_version = 3 AND state = 'claimed'
                          AND lease_expires_ns <= ?2"#,
                    params![comparison_id, now_ns],
                )?,
                "expire historical comparison",
            )?;
        }
        transaction.commit()?;
        Ok(comparison_ids.len())
    }

    pub fn reserve_initialize(
        &mut self,
        request: &InitializeRequest,
    ) -> Result<InitializeReservation, ManagerError> {
        self.reserve_initialize_with_state(request, "planned")
    }

    /// Reserves initialization and durably crosses the filesystem-effect
    /// boundary in the same transaction. Callers which create the snapshot
    /// immediately after reservation do not need a second SQLite commit just
    /// to change `planned` into `fs_started`.
    pub fn reserve_initialize_started(
        &mut self,
        request: &InitializeRequest,
    ) -> Result<InitializeReservation, ManagerError> {
        self.reserve_initialize_with_state(request, "fs_started")
    }

    fn reserve_initialize_with_state(
        &mut self,
        request: &InitializeRequest,
        operation_state: &'static str,
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
                 ?1, 'initialize', ?2, ?3, ?4, 0, ?5, NULL, ?5, 1, \
                 ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12 \
             )",
            params![
                operation_id.as_slice(),
                operation_state,
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

    /// Removes an initialize operation which crossed the manager's durable
    /// pre-effect fence but has neither a live source nor a created snapshot.
    ///
    /// The caller must inspect the reserved snapshot path first. With direct
    /// unprivileged Btrfs create, a surviving snapshot is the durable proof
    /// that the filesystem effect happened; when both paths are absent there
    /// is no effect left to reconcile.
    pub fn cancel_unstarted_initialize(
        &mut self,
        reservation: &InitializeReservation,
        lease_owner: [u8; 16],
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_one(
            transaction.execute(
                r#"DELETE FROM operations
                     WHERE id = ?1 AND watch_id = ?2 AND kind = 'initialize'
                       AND state = 'fs_started' AND lease_owner = ?3
                       AND lease_fence = ?4"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.watch_id.as_slice(),
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
            )?,
            "cancel unstarted initialize",
        )?;
        transaction.execute(
            r#"DELETE FROM watch_grants WHERE watch_id = ?1
                 AND NOT EXISTS (SELECT 1 FROM operations o WHERE o.watch_id = ?1)"#,
            [reservation.watch_id.as_slice()],
        )?;
        transaction.execute(
            r#"DELETE FROM watches WHERE id = ?1 AND state = 'initializing'
                 AND NOT EXISTS (SELECT 1 FROM operations o WHERE o.watch_id = ?1)"#,
            [reservation.watch_id.as_slice()],
        )?;
        transaction.execute(
            r#"UPDATE topology_leases
                   SET lease_owner = NULL, lease_expires_ns = NULL
                 WHERE filesystem_id = ?1 AND lease_owner = ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM operations o
                        WHERE o.filesystem_id = ?1 AND o.kind = 'initialize'
                          AND o.state NOT IN ('planned', 'done', 'failed')
                   )"#,
            params![reservation.filesystem_id, lease_owner.as_slice()],
        )?;
        transaction.commit()?;
        Ok(())
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

    pub fn publish_initial_snapshot(
        &mut self,
        reservation: &InitializeReservation,
        lease_owner: [u8; 16],
        snapshot: &RecordedSnapshot,
        inherited_baseline_snapshot_uuid: Option<[u8; 16]>,
        now_ns: i64,
    ) -> Result<InitializedWatch, ManagerError> {
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
        let revision_id = allocate_revision_id(&transaction)?;
        insert_pathless_revision(
            &transaction,
            revision_id,
            snapshot.snapshot_id,
            None,
            reservation.operation_fence,
            now_ns,
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE watches
                      SET indexed_revision_id = ?2, indexed_seq = 0,
                          last_cut_snapshot_id = ?3, last_cut_seq = 0,
                          replay_floor_seq = 0,
                          inherited_baseline_snapshot_uuid = ?4,
                          state = 'active'
                    WHERE id = ?1 AND state = 'initializing'
                      AND indexed_revision_id IS NULL AND last_cut_snapshot_id IS NULL"#,
                params![
                    reservation.watch_id.as_slice(),
                    revision_id,
                    snapshot.snapshot_id,
                    inherited_baseline_snapshot_uuid
                        .as_ref()
                        .map(<[u8; 16]>::as_slice),
                ],
            )?,
            "activate initialized watch",
        )?;
        for (kind, reason) in [
            ("watch-indexed-head", "initialized-index-head"),
            ("watch-last-cut", "initialized-physical-head"),
        ] {
            transaction.execute(
                "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) VALUES (?1, ?2, ?3, ?4)",
                params![
                    snapshot.snapshot_id,
                    kind,
                    reservation.watch_id.as_slice(),
                    reason,
                ],
            )?;
        }
        transaction.execute(
            "DELETE FROM snapshot_pins WHERE snapshot_id = ?1 AND owner_kind = 'operation' AND owner_id = ?2 AND reason = 'initialize-build'",
            params![snapshot.snapshot_id, reservation.operation_id.as_slice()],
        )?;
        require_one(
            transaction.execute(
                "UPDATE operations SET state = 'done', lease_owner = NULL, lease_expires_ns = NULL, updated_ns = ?4 WHERE id = ?1 AND watch_id = ?2 AND state = 'uuid_recorded' AND lease_owner = ?3 AND lease_fence = ?5",
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
            snapshot_identity: snapshot.identity.clone(),
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
        if !matches!(request_kind, "clock" | "query") || expires_ns <= now_ns {
            return Err(ManagerError::new("invalid cut admission"));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        reap_expired_planned_cuts(&transaction, now_ns)?;
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
        reap_expired_planned_cuts(&transaction, request.now_ns)?;
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
                  AND lease_expires_ns > ?4
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

    /// Publishes an adjacent immutable comparison whose exact path events were
    /// derived from broker-resolved endpoint paths.
    pub fn publish_adjacent_broker_delta(
        &mut self,
        reservation: &CutReservation,
        lease_owner: [u8; 16],
        snapshot: &RecordedSnapshot,
        manifest: &ChangedObjectsManifest,
        events: &[Event],
        replay_spool: Option<(&[u8], [u8; 32])>,
        now_ns: i64,
    ) -> Result<PublishedCut, ManagerError> {
        let (base_revision_id, base_snapshot_id): (i64, i64) = self
            .connection()
            .query_row(
                r#"SELECT w.indexed_revision_id, r.snapshot_id
                     FROM watches w JOIN revisions r ON r.id = w.indexed_revision_id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND w.indexed_seq = ?2 AND r.state = 'ready'"#,
                params![reservation.watch_id.as_slice(), reservation.sequence - 1],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| ManagerError::new("prior cut is not the ready indexed head"))?;
        if base_snapshot_id != reservation.base_snapshot_id {
            return Err(ManagerError::new(
                "cut base snapshot differs from indexed predecessor",
            ));
        }
        stage_broker_delta_rows(self.connection_mut(), manifest, events)?;
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
               ) VALUES (?1, ?2, 'incremental', 4, 'claimed', ?3, ?4, NULL,
                         ?5, ?6, ?7)"#,
            params![
                base_snapshot_id,
                snapshot.snapshot_id,
                lease_owner.as_slice(),
                reservation.operation_fence,
                manifest.canonical_hash().as_slice(),
                i64::try_from(manifest.raw_ref_adds)
                    .map_err(|_| ManagerError::new("raw ref add count overflow"))?,
                i64::try_from(manifest.raw_ref_deletes)
                    .map_err(|_| ManagerError::new("raw ref delete count overflow"))?,
            ],
        )?;
        let comparison_id = transaction.last_insert_rowid();
        import_staged_comparison_rows(&transaction, comparison_id)?;
        if replay_spool.is_some() {
            for table in ["change_events", "comparison_refs", "comparison_objects"] {
                transaction.execute(
                    &format!("DELETE FROM {table} WHERE comparison_id = ?1"),
                    [comparison_id],
                )?;
            }
        }
        let revision_id = allocate_revision_id(&transaction)?;
        insert_pathless_revision(
            &transaction,
            revision_id,
            snapshot.snapshot_id,
            Some(comparison_id),
            reservation.operation_fence,
            now_ns,
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
        if let Some((replay_spool_path, replay_payload_hash)) = replay_spool {
            transaction.execute(
                r#"INSERT INTO replay_spools(
                       watch_id, sequence, from_snapshot_id, to_snapshot_id,
                       path, payload_hash, event_count
                   ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
                params![
                    reservation.watch_id.as_slice(),
                    reservation.sequence,
                    base_snapshot_id,
                    snapshot.snapshot_id,
                    replay_spool_path,
                    replay_payload_hash.as_slice(),
                    i64::try_from(events.len())
                        .map_err(|_| ManagerError::new("replay event count overflow"))?,
                ],
            )?;
        }
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
            "UPDATE cut_admissions SET state = 'fulfilled' WHERE operation_id = ?1 AND watch_id = ?2 AND state = 'waiting'",
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice(),
            ],
        )?;
        transaction.commit()?;
        let _ = clear_staged_delta(self.connection_mut());
        Ok(PublishedCut {
            watch_id: reservation.watch_id,
            sequence: reservation.sequence,
            snapshot_id: snapshot.snapshot_id,
            revision_id,
            comparison_id,
            events: events.to_vec(),
        })
    }

    /// Checks that an adjacent manifest can be applied to the indexed
    /// predecessor before the caller advances the physical cut head.
    ///
    /// Publication repeats the same derivation under its final fence. This
    /// preflight is intentionally read-only: a malformed manifest or target
    /// object set must not turn an otherwise healthy watch into a physical
    /// head which can never be indexed.
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

    /// Terminally records a cut whose immutable target failed validation
    /// before it was allowed to become the physical head.
    ///
    /// The failed cut row preserves the monotonic sequence gap needed by a
    /// later full-fresh recovery cut, while the watch keeps its prior physical
    /// and indexed heads. Releasing operation pins leaves the rejected target
    /// eligible for the ordinary fenced snapshot-GC path.
    pub fn fail_unpublished_cut(
        &mut self,
        reservation: &CutReservation,
        lease_owner: [u8; 16],
        snapshot: &RecordedSnapshot,
        error: &str,
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        if error.is_empty() {
            return Err(ManagerError::new("terminal cut failure needs an error"));
        }
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
               ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, 'failed', 0)"#,
            params![
                reservation.watch_id.as_slice(),
                reservation.sequence,
                reservation.operation_id.as_slice(),
                reservation.base_snapshot_id,
                snapshot.snapshot_id,
            ],
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE operations
                      SET state = 'failed', error = ?4, lease_owner = NULL,
                          lease_expires_ns = NULL, updated_ns = ?5
                    WHERE id = ?1 AND watch_id = ?2 AND sequence = ?3
                      AND state = 'uuid_recorded' AND lease_owner = ?6
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
            "fail unpublished cut operation",
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE watches
                      SET cut_owner = NULL, cut_expires_ns = NULL
                    WHERE id = ?1 AND state = 'active'
                      AND last_cut_snapshot_id = ?2
                      AND cut_owner = ?3 AND cut_fence = ?4"#,
                params![
                    reservation.watch_id.as_slice(),
                    reservation.base_snapshot_id,
                    lease_owner.as_slice(),
                    reservation.cut_fence,
                ],
            )?,
            "release unpublished cut lease",
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
                reservation.watch_id.as_slice(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn advance_replay_floor(
        &mut self,
        watch_id: [u8; 16],
        new_floor: i64,
        now_ns: i64,
        _compactor_owner: [u8; 16],
    ) -> Result<usize, ManagerError> {
        let (current_floor, indexed_sequence): (i64, i64) = self
            .connection()
            .query_row(
                "SELECT replay_floor_seq, indexed_seq \
                 FROM watches WHERE id = ?1 AND state IN ('active', 'blocked')",
                [watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| ManagerError::new("replay-floor watch is absent"))?;
        if new_floor < current_floor || new_floor > indexed_sequence {
            return Err(ManagerError::new("invalid replay-floor advance"));
        }
        if new_floor == current_floor {
            return Ok(0);
        }
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
        let replay_spool_paths = {
            let mut statement = transaction
                .prepare("SELECT path FROM replay_spools WHERE watch_id = ?1 AND sequence < ?2")?;
            statement
                .query_map(params![watch_id.as_slice(), new_floor], |row| {
                    row.get::<_, Vec<u8>>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        transaction.execute(
            "DELETE FROM replay_spools WHERE watch_id = ?1 AND sequence < ?2",
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
                            AND c.state != 'claimed' \
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
                  AND comparisons.state != 'claimed'
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
        remove_replay_spool_files(&replay_spool_paths);
        Ok(reclaimed)
    }

    /// Retains the shared replay interval needed by durable consumers.
    ///
    /// Physical snapshot pins remain per consumer, while adjacent-event
    /// spools stay on one shared timeline. Boundary removal and old-cut
    /// reclamation are one immediate transaction so a retained boundary never
    /// loses the watch_cuts row named by its composite foreign key.
    pub fn retain_shared_replay_history(
        &mut self,
        watch_id: [u8; 16],
        _compactor_owner: [u8; 16],
        now_ns: i64,
        max_cuts: i64,
        max_age_ns: i64,
        delete_limit: usize,
    ) -> Result<usize, ManagerError> {
        if max_cuts <= 0 || max_age_ns <= 0 || delete_limit == 0 {
            return Err(ManagerError::new("invalid replay retention limits"));
        }
        // Keep this public helper correct when called directly outside the
        // production scheduler. The bounded global sweep normally does this
        // first; this per-watch slice prevents expired endpoints from
        // protecting history forever without making one tick unbounded.
        self.expire_query_leases_bounded(now_ns, delete_limit)?;
        let (boundary_count, newest): (i64, Option<i64>) = self.connection().query_row(
            r#"SELECT count(*), max(cut_sequence)
                     FROM fsmonitor_boundaries WHERE watch_id = ?1"#,
            [watch_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if boundary_count <= 1 {
            return Ok(0);
        }
        let newest = newest.ok_or_else(|| ManagerError::new("replay checkpoint set is empty"))?;
        // Durable consumers own independent endpoints on one shared revision
        // timeline. Keep the entire adjacent-event interval from the oldest
        // committed or pending consumer through the newest cut. When there
        // are no durable consumers, only the newest opportunistic endpoint is
        // retained; stale consumers must fail rather than receive an
        // unproven whole-tree invalidation.
        let oldest_consumer_sequence: Option<i64> = self.connection().query_row(
            r#"SELECT min(sequence)
                 FROM (
                     SELECT b.cut_sequence AS sequence
                       FROM fsmonitor_boundaries b
                       JOIN snapshot_pins p
                         ON p.snapshot_id = b.target_snapshot_id
                      WHERE b.watch_id = ?1
                        AND p.owner_kind = 'consumer-baseline'
                     UNION ALL
                     SELECT c.sequence - 1 AS sequence
                       FROM watch_cuts c
                       JOIN snapshot_pins p
                         ON p.snapshot_id = c.base_snapshot_id
                      WHERE c.watch_id = ?1
                        AND p.owner_kind = 'consumer-baseline'
                     UNION ALL
                     SELECT 0 AS sequence
                       FROM watches w
                       JOIN snapshots s
                         ON s.filesystem_id = w.filesystem_id
                        AND s.subvol_uuid = w.inherited_baseline_snapshot_uuid
                       JOIN snapshot_pins p
                         ON p.snapshot_id = s.id
                      WHERE w.id = ?1
                        AND p.owner_kind = 'consumer-baseline'
                 ) consumer_endpoints"#,
            [watch_id.as_slice()],
            |row| row.get(0),
        )?;
        let mut retained = BTreeSet::from([newest]);
        if let Some(oldest_consumer_sequence) = oldest_consumer_sequence {
            let mut statement = self.connection().prepare(
                r#"SELECT cut_sequence FROM fsmonitor_boundaries
                    WHERE watch_id = ?1 AND cut_sequence >= ?2
                    ORDER BY cut_sequence"#,
            )?;
            let rows = statement
                .query_map(
                    params![watch_id.as_slice(), oldest_consumer_sequence],
                    |row| row.get::<_, i64>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            retained.extend(rows);
        }

        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Fetch only a bounded prefix. Active endpoints are excluded in SQL
        // under this writer transaction: a Begin either committed before this
        // query and is protected here, or waits and sees the post-GC set.
        let candidate_limit = delete_limit
            .checked_add(retained.len())
            .ok_or_else(|| ManagerError::new("boundary candidate limit overflow"))?;
        let candidates = {
            let mut statement = transaction.prepare(
                r#"SELECT b.cut_sequence
                     FROM fsmonitor_boundaries b
                    WHERE b.watch_id = ?1 AND b.cut_sequence <= ?2
                      AND NOT EXISTS (
                          SELECT 1 FROM query_leases q
                           WHERE q.watch_id = b.watch_id AND q.state = 'active'
                             AND q.lease_expires_ns > ?3
                             AND (q.from_cut_sequence = b.cut_sequence
                                  OR q.to_cut_sequence = b.cut_sequence)
                      )
                    ORDER BY b.cut_sequence LIMIT ?4"#,
            )?;
            statement
                .query_map(
                    params![
                        watch_id.as_slice(),
                        newest,
                        now_ns,
                        i64::try_from(candidate_limit).map_err(|_| {
                            ManagerError::new("boundary candidate limit exceeds SQLite integer")
                        })?,
                    ],
                    |row| row.get::<_, i64>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut removed = 0_usize;
        let mut removed_sequences = Vec::new();
        for sequence in candidates
            .into_iter()
            .filter(|sequence| !retained.contains(sequence))
            .take(delete_limit)
        {
            let changed = transaction.execute(
                r#"DELETE FROM fsmonitor_boundaries
                    WHERE watch_id = ?1 AND cut_sequence = ?2
                      AND NOT EXISTS (
                          SELECT 1 FROM query_leases q
                           WHERE q.watch_id = fsmonitor_boundaries.watch_id
                             AND q.state = 'active' AND q.lease_expires_ns > ?3
                             AND (q.from_cut_sequence = fsmonitor_boundaries.cut_sequence
                                  OR q.to_cut_sequence = fsmonitor_boundaries.cut_sequence)
                      )"#,
                params![watch_id.as_slice(), sequence, now_ns],
            )?;
            if changed != 0 {
                removed += changed;
                removed_sequences.push(sequence);
            }
        }
        let replay_floor: i64 = transaction
            .query_row(
                "SELECT min(cut_sequence) FROM fsmonitor_boundaries WHERE watch_id = ?1",
                [watch_id.as_slice()],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .ok_or_else(|| ManagerError::new("retained replay set is empty"))?;
        transaction.execute(
            r#"UPDATE watches SET replay_floor_seq = ?2
                WHERE id = ?1 AND replay_floor_seq < ?2"#,
            params![watch_id.as_slice(), replay_floor],
        )?;
        // Drop only cuts whose boundary was removed above. Surviving
        // boundaries continue to own their exact composite parent row.
        let mut replay_spool_paths = Vec::new();
        for sequence in removed_sequences {
            transaction.execute(
                r#"DELETE FROM cut_admissions WHERE operation_id IN (
                       SELECT operation_id FROM watch_cuts
                        WHERE watch_id = ?1 AND sequence = ?2
                   )"#,
                params![watch_id.as_slice(), sequence],
            )?;
            let path: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT path FROM replay_spools WHERE watch_id = ?1 AND sequence = ?2",
                    params![watch_id.as_slice(), sequence],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(path) = path {
                replay_spool_paths.push(path);
            }
            transaction.execute(
                "DELETE FROM replay_spools WHERE watch_id = ?1 AND sequence = ?2",
                params![watch_id.as_slice(), sequence],
            )?;
            transaction.execute(
                "DELETE FROM watch_cuts WHERE watch_id = ?1 AND sequence = ?2",
                params![watch_id.as_slice(), sequence],
            )?;
            transaction.execute(
                r#"DELETE FROM operations WHERE watch_id = ?1 AND kind = 'cut'
                    AND state = 'done' AND sequence = ?2"#,
                params![watch_id.as_slice(), sequence],
            )?;
        }
        transaction.commit()?;
        remove_replay_spool_files(&replay_spool_paths);
        Ok(removed)
    }

    /// Reclaims at most `limit` orphan comparison/revision units independent
    /// of any watch slice. Keeping this separate from boundary retention means
    /// idle or short-history watches cannot starve global cleanup.
    pub fn reclaim_orphan_history_bounded(&mut self, limit: usize) -> Result<usize, ManagerError> {
        if limit == 0 {
            return Err(ManagerError::new("history reclaim limit must be positive"));
        }
        let reclaimed_comparisons = self.reclaim_unreferenced_comparisons(limit)?;
        let remaining = limit.saturating_sub(reclaimed_comparisons);
        let reclaimed_revisions = if remaining == 0 {
            0
        } else {
            self.reclaim_unreferenced_revisions(remaining)?
        };
        reclaimed_comparisons
            .checked_add(reclaimed_revisions)
            .ok_or_else(|| ManagerError::new("history reclaim count overflow"))
    }

    fn reclaim_unreferenced_comparisons(&mut self, limit: usize) -> Result<usize, ManagerError> {
        if limit == 0 {
            return Err(ManagerError::new(
                "comparison reclaim limit must be positive",
            ));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let orphan_comparisons = {
            let mut statement = transaction.prepare(
                r#"SELECT id FROM comparisons
                    WHERE NOT EXISTS (SELECT 1 FROM revisions r
                                       WHERE r.provenance_comparison_id = comparisons.id)
                      AND NOT EXISTS (SELECT 1 FROM watch_cuts wc
                                       WHERE wc.comparison_id = comparisons.id)
                      AND NOT EXISTS (SELECT 1 FROM query_comparison_pins p
                                       WHERE p.comparison_id = comparisons.id)
                      AND comparisons.state != 'claimed'
                    ORDER BY id LIMIT ?1"#,
            )?;
            statement
                .query_map(
                    [i64::try_from(limit)
                        .map_err(|_| ManagerError::new("comparison reclaim limit overflow"))?],
                    |row| row.get::<_, i64>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        for comparison_id in &orphan_comparisons {
            for table in ["change_events", "comparison_refs", "comparison_objects"] {
                transaction.execute(
                    &format!("DELETE FROM {table} WHERE comparison_id = ?1"),
                    [comparison_id],
                )?;
            }
            transaction.execute("DELETE FROM comparisons WHERE id = ?1", [comparison_id])?;
        }
        transaction.commit()?;
        Ok(orphan_comparisons.len())
    }

    fn reclaim_unreferenced_revisions(&mut self, limit: usize) -> Result<usize, ManagerError> {
        if limit == 0 {
            return Err(ManagerError::new("revision reclaim limit must be positive"));
        }
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut reclaimed = 0_usize;
        while reclaimed < limit {
            let candidate: Option<i64> = transaction
                .query_row(
                    r#"SELECT r.id FROM revisions r
                        WHERE r.state = 'ready'
                          AND NOT EXISTS (
                              SELECT 1 FROM watches w
                               WHERE w.indexed_revision_id = r.id)
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
            require_one(
                transaction.execute("DELETE FROM revisions WHERE id = ?1", [revision_id])?,
                "reclaim logical revision",
            )?;
            reclaimed += 1;
        }
        transaction.commit()?;
        Ok(reclaimed)
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

    /// Reconciles the durable consumer baseline pins with the baseline named
    /// by the caller's journal after a possible crash. A pending candidate is
    /// adopted when the journal names it, discarded when the journal still
    /// names the committed baseline, and otherwise both stale pins are
    /// released so the caller must establish a fresh full-scan baseline.
    pub fn reconcile_consumer_baseline(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        owner_id: [u8; 16],
        previous_snapshot_id: Option<i64>,
    ) -> Result<bool, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorized: Option<i64> = transaction
            .query_row(
                r#"SELECT 1 FROM watches w JOIN watch_grants g ON g.watch_id = w.id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND g.id = ?2 AND g.state = 'active'
                      AND (g.permissions & ?3) = ?3"#,
                params![
                    watch_id.as_slice(),
                    authorization_id.as_slice(),
                    i64::from(PERMISSION_READ | PERMISSION_CUT),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if authorized != Some(1) {
            return Err(ManagerError::new(
                "consumer baseline owner is not authorized",
            ));
        }
        let committed: Option<i64> = transaction
            .query_row(
                "SELECT snapshot_id FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'committed'",
                [owner_id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        let pending: Option<i64> = transaction
            .query_row(
                "SELECT snapshot_id FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'pending'",
                [owner_id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        // Older consumer journals and Git tokens can already name a valid
        // immutable baseline while predating durable consumer pins. Adopt
        // that exact baseline in place during the first upgraded scan so each
        // lane can migrate without forcing a forbidden strict-mode full
        // traversal.
        let adoptable_unpinned: Option<i64> = if committed.is_none() && pending.is_none() {
            previous_snapshot_id
                .map(|previous| {
                    transaction
                        .query_row(
                            r#"SELECT 1
                                 FROM (
                                     SELECT b.target_snapshot_id AS snapshot_id
                                       FROM fsmonitor_boundaries b
                                      WHERE b.watch_id = ?1
                                     UNION ALL
                                     SELECT w.last_cut_snapshot_id AS snapshot_id
                                       FROM watches w
                                      WHERE w.id = ?1 AND w.indexed_seq = 0
                                 ) candidate
                                 JOIN snapshots s ON s.id = candidate.snapshot_id
                                 JOIN revisions r ON r.snapshot_id = s.id
                                WHERE candidate.snapshot_id = ?2
                                  AND s.physical_state = 'present' AND r.state = 'ready'
                                LIMIT 1"#,
                            params![watch_id.as_slice(), previous],
                            |row| row.get(0),
                        )
                        .optional()
                })
                .transpose()?
                .flatten()
        } else {
            None
        };
        let retained = match previous_snapshot_id {
            Some(previous) if pending == Some(previous) => {
                transaction.execute(
                    "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'committed'",
                    [owner_id.as_slice()],
                )?;
                transaction.execute(
                    "UPDATE snapshot_pins SET reason = 'committed' WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'pending'",
                    [owner_id.as_slice()],
                )?;
                true
            }
            Some(previous) if committed == Some(previous) => {
                transaction.execute(
                    "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'pending'",
                    [owner_id.as_slice()],
                )?;
                true
            }
            Some(previous) if adoptable_unpinned == Some(1) => {
                transaction.execute(
                    "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) VALUES (?1, 'consumer-baseline', ?2, 'committed')",
                    params![previous, owner_id.as_slice()],
                )?;
                true
            }
            _ => {
                transaction.execute(
                    "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1",
                    [owner_id.as_slice()],
                )?;
                false
            }
        };
        transaction.commit()?;
        Ok(retained)
    }

    /// Pins the scan candidate without releasing the committed baseline.
    pub fn stage_consumer_baseline(
        &mut self,
        watch_id: [u8; 16],
        authorization_id: [u8; 16],
        owner_id: [u8; 16],
        snapshot_id: i64,
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: Option<i64> = transaction
            .query_row(
                r#"SELECT 1 FROM watches w JOIN watch_grants g ON g.watch_id = w.id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND g.id = ?2 AND g.state = 'active'
                      AND (g.permissions & ?4) = ?4
                      AND (
                          EXISTS (
                              SELECT 1 FROM fsmonitor_boundaries b
                               WHERE b.watch_id = w.id AND b.target_snapshot_id = ?3
                          )
                          OR EXISTS (
                              SELECT 1 FROM revisions r
                               WHERE r.id = w.indexed_revision_id
                                 AND r.snapshot_id = ?3 AND r.state = 'ready'
                          )
                      )"#,
                params![
                    watch_id.as_slice(),
                    authorization_id.as_slice(),
                    snapshot_id,
                    i64::from(PERMISSION_READ | PERMISSION_CUT),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if valid != Some(1) {
            return Err(ManagerError::new(
                "consumer baseline candidate is not authorized",
            ));
        }
        transaction.execute(
            "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'pending'",
            [owner_id.as_slice()],
        )?;
        transaction.execute(
            "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) VALUES (?1, 'consumer-baseline', ?2, 'pending')",
            params![snapshot_id, owner_id.as_slice()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Completes the two-phase consumer baseline handoff.
    pub fn finish_consumer_baseline(
        &mut self,
        owner_id: [u8; 16],
        committed: bool,
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if committed {
            transaction.execute(
                "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'committed'",
                [owner_id.as_slice()],
            )?;
            require_one(
                transaction.execute(
                    "UPDATE snapshot_pins SET reason = 'committed' WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'pending'",
                    [owner_id.as_slice()],
                )?,
                "commit consumer baseline",
            )?;
        } else {
            transaction.execute(
                "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'pending'",
                [owner_id.as_slice()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Releases every durable baseline pin owned by one consumer.
    ///
    /// This is intentionally idempotent: a retry after a lost response, or a
    /// consumer whose journal was already cleared, must not leave stale pins
    /// behind or fail the caller's mode transition.
    pub fn release_consumer_baseline(&mut self, owner_id: [u8; 16]) -> Result<(), ManagerError> {
        self.connection_mut().execute(
            "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1",
            [owner_id.as_slice()],
        )?;
        Ok(())
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
        self.expire_retention_leases_inner(now_ns, None)
    }

    /// Releases at most `limit` expired caller-retention leases and their
    /// pins in one writer transaction for bounded production maintenance.
    pub fn expire_retention_leases_bounded(
        &mut self,
        now_ns: i64,
        limit: usize,
    ) -> Result<usize, ManagerError> {
        if limit == 0 {
            return Err(ManagerError::new(
                "retention lease expiry limit must be positive",
            ));
        }
        self.expire_retention_leases_inner(now_ns, Some(limit))
    }

    fn expire_retention_leases_inner(
        &mut self,
        now_ns: i64,
        limit: Option<usize>,
    ) -> Result<usize, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expired: i64;
        if let Some(limit) = limit {
            let limit = i64::try_from(limit)
                .map_err(|_| ManagerError::new("retention lease expiry limit overflow"))?;
            expired = transaction.query_row(
                r#"SELECT count(*) FROM (
                       SELECT id FROM retention_leases
                        WHERE state = 'active' AND expires_ns <= ?1
                        ORDER BY id LIMIT ?2
                   )"#,
                params![now_ns, limit],
                |row| row.get(0),
            )?;
            transaction.execute(
                r#"DELETE FROM snapshot_pins
                    WHERE owner_kind = 'retention-lease'
                      AND EXISTS (
                          SELECT 1 FROM retention_leases r
                           WHERE r.id = snapshot_pins.owner_id
                             AND r.snapshot_id = snapshot_pins.snapshot_id
                             AND r.state = 'active' AND r.expires_ns <= ?1
                             AND r.id IN (
                                 SELECT id FROM retention_leases
                                  WHERE state = 'active' AND expires_ns <= ?1
                                  ORDER BY id LIMIT ?2
                             ))"#,
                params![now_ns, limit],
            )?;
            transaction.execute(
                r#"UPDATE retention_leases
                      SET state = 'expired', lease_fence = lease_fence + 1
                    WHERE id IN (
                       SELECT id FROM retention_leases
                        WHERE state = 'active' AND expires_ns <= ?1
                        ORDER BY id LIMIT ?2
                    )"#,
                params![now_ns, limit],
            )?;
        } else {
            expired = transaction.query_row(
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
        }
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
                          SELECT 1 FROM query_revision_pins p
                          JOIN revisions r ON r.id = p.revision_id
                           WHERE r.snapshot_id = s.id
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
                      )
                      AND NOT EXISTS (
                          SELECT 1 FROM query_revision_pins p
                          JOIN revisions r ON r.id = p.revision_id
                           WHERE r.snapshot_id = snapshots.id
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

    /// Rolls back an intent that never crossed the fs_started effect boundary.
    /// This is safe without broker reconciliation because a delete effect is
    /// forbidden until start_snapshot_delete has committed.
    pub fn cancel_planned_snapshot_delete(
        &mut self,
        reservation: &SnapshotDeleteReservation,
        lease_owner: [u8; 16],
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_one(
            transaction.execute(
                r#"UPDATE snapshots
                      SET physical_state = 'present'
                    WHERE id = ?1 AND physical_state = 'deleting'
                      AND EXISTS (
                          SELECT 1 FROM snapshot_delete_operations d
                           WHERE d.id = ?2 AND d.snapshot_id = snapshots.id
                             AND d.state = 'planned' AND d.lease_owner = ?3
                             AND d.lease_fence = ?4
                      )"#,
                params![
                    reservation.snapshot_id,
                    reservation.operation_id.as_slice(),
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
            )?,
            "restore planned snapshot delete",
        )?;
        require_one(
            transaction.execute(
                r#"DELETE FROM snapshot_delete_operations
                    WHERE id = ?1 AND snapshot_id = ?2 AND state = 'planned'
                      AND lease_owner = ?3 AND lease_fence = ?4"#,
                params![
                    reservation.operation_id.as_slice(),
                    reservation.snapshot_id,
                    lease_owner.as_slice(),
                    reservation.operation_fence,
                ],
            )?,
            "cancel planned snapshot delete",
        )?;
        transaction.commit()?;
        Ok(())
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
        let replay_floor: i64 = transaction.query_row(
            "SELECT replay_floor_seq FROM watches WHERE id = ?1",
            [watch_id.as_slice()],
            |row| row.get(0),
        )?;
        let expected = to_sequence - from_sequence;
        let spool_count: i64 = transaction.query_row(
            r#"SELECT count(*) FROM replay_spools
                WHERE watch_id = ?1 AND sequence > ?2 AND sequence <= ?3"#,
            params![watch_id.as_slice(), from_sequence, to_sequence],
            |row| row.get(0),
        )?;
        if from_sequence >= replay_floor && spool_count == expected {
            let events =
                load_historical_events(&transaction, watch_id, from_sequence, to_sequence)?;
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
               VALUES (?1, ?2, 'incremental', 3, 'claimed', ?3, 1, ?4,
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
                  AND comparison_kind = 'incremental' AND algorithm_version = 3"#,
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
            for table in ["change_events", "comparison_refs", "comparison_objects"] {
                transaction.execute(
                    &format!("DELETE FROM {table} WHERE comparison_id = ?1"),
                    [comparison_id],
                )?;
            }
            transaction.execute(
                "DELETE FROM comparisons WHERE id = ?1 AND comparison_kind = 'incremental' AND algorithm_version = 3",
                [comparison_id],
            )?;
            transaction.commit()?;
            return self.claim_historical_comparison(request);
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

    pub fn publish_historical_broker_comparison(
        &mut self,
        claim: &HistoricalComparisonClaim,
        manifest: &ChangedObjectsManifest,
        events: &[Event],
        now_ns: i64,
    ) -> Result<HistoricalChanges, ManagerError> {
        stage_broker_delta_rows(self.connection_mut(), manifest, events)?;
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
                      AND c.algorithm_version = 3 AND c.comparison_kind = 'incremental'
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
        for table in ["change_events", "comparison_refs", "comparison_objects"] {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE comparison_id = ?1"),
                [claim.comparison_id],
            )?;
        }
        transaction.execute(
            "DELETE FROM comparisons WHERE id = ?1 AND comparison_kind = 'incremental' AND algorithm_version = 3",
            [claim.comparison_id],
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
            events: events.to_vec(),
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
        let spool_count: i64 = transaction.query_row(
            r#"SELECT count(*) FROM replay_spools
                WHERE watch_id = ?1 AND sequence > ?2 AND sequence <= ?3"#,
            params![watch_id.as_slice(), from_sequence, to_sequence],
            |row| row.get(0),
        )?;
        // A v7 store has no canonical spool rows for already-published cuts.
        // Keep those old cursors readable during the one-way migration, but
        // never mix old SQLite event rows with a partially retained spool
        // range: a mixed range cannot prove an exact replay.
        let ready_count = if spool_count == 0 {
            transaction.query_row(
                r#"SELECT count(*) FROM watch_cuts
                    WHERE watch_id = ?1 AND sequence > ?2 AND sequence <= ?3
                      AND state = 'ready' AND fresh_instance = 0
                      AND comparison_id IS NOT NULL"#,
                params![watch_id.as_slice(), from_sequence, to_sequence],
                |row| row.get(0),
            )?
        } else {
            spool_count
        };
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
                     UNION ALL
                     SELECT 0 AS sequence
                       FROM watches w
                      WHERE w.id = ?1
                        AND w.inherited_baseline_snapshot_uuid = ?2
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
        r#"SELECT path, payload_hash, event_count
             FROM replay_spools
            WHERE watch_id = ?1 AND sequence > ?2 AND sequence <= ?3
            ORDER BY sequence"#,
    )?;
    let spools = statement
        .query_map(
            params![watch_id.as_slice(), from_sequence, to_sequence],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    if spools.is_empty() {
        let mut statement = transaction.prepare(
            r#"SELECT e.event_kind, e.ino, e.old_generation, e.new_generation,
                      e.change_mask, e.old_path, e.new_path
                 FROM watch_cuts c
                 JOIN change_events e ON e.comparison_id = c.comparison_id
                WHERE c.watch_id = ?1 AND c.sequence > ?2 AND c.sequence <= ?3
                  AND c.state = 'ready' AND c.fresh_instance = 0
                ORDER BY c.sequence, e.ordinal"#,
        )?;
        let rows = statement
            .query_map(
                params![watch_id.as_slice(), from_sequence, to_sequence],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        return rows
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
            .collect();
    }
    let mut events = Vec::new();
    for (path, hash, count) in spools {
        let hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| ManagerError::new("replay spool hash has invalid length"))?;
        events.extend(load_replay_spool_payload(&path, hash, count)?);
    }
    Ok(events)
}

#[cfg(any())]
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

fn stage_broker_delta_rows(
    connection: &mut rusqlite::Connection,
    manifest: &ChangedObjectsManifest,
    events: &[Event],
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
           DELETE FROM delta_stage_objects;
           DELETE FROM delta_stage_refs;
           DELETE FROM delta_stage_events;"#,
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
    transaction.commit()?;
    Ok(())
}

fn insert_pathless_revision(
    transaction: &Transaction<'_>,
    revision_id: i64,
    snapshot_id: i64,
    provenance_comparison_id: Option<i64>,
    builder_fence: i64,
    now_ns: i64,
) -> Result<(), ManagerError> {
    transaction.execute(
        r#"INSERT INTO revisions(
               id, snapshot_id, provenance_comparison_id, state,
               builder_fence, created_ns
           ) VALUES (?1, ?2, ?3, 'ready', ?4, ?5)"#,
        params![
            revision_id,
            snapshot_id,
            provenance_comparison_id,
            builder_fence,
            now_ns,
        ],
    )?;
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

fn clear_staged_delta(connection: &mut rusqlite::Connection) -> Result<(), ManagerError> {
    connection.execute_batch(
        "DELETE FROM delta_stage_objects;
         DELETE FROM delta_stage_refs;
         DELETE FROM delta_stage_events;",
    )?;
    Ok(())
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

pub(crate) fn append_replay_spool_payload(
    path: &Path,
    events: &[Event],
) -> Result<[u8; 32], ManagerError> {
    let mut payload = Vec::new();
    push_u64(&mut payload, events.len() as u64);
    for event in events {
        payload.push(match event.kind {
            EventKind::PathAdded => 1,
            EventKind::PathRemoved => 2,
            EventKind::PathChanged => 3,
            EventKind::SubtreeMoved => 4,
            EventKind::DirectoryDirtyWitness => 5,
        });
        push_u64(&mut payload, event.ino);
        push_optional_u64(&mut payload, event.old_generation);
        push_optional_u64(&mut payload, event.new_generation);
        push_u64(&mut payload, event.change_mask);
        push_optional_bytes(&mut payload, event.old_path.as_deref());
        push_optional_bytes(&mut payload, event.new_path.as_deref());
    }
    let hash: [u8; 32] = Sha256::digest(&payload).into();
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| ManagerError::new(format!("open replay spool: {error}")))?;
    file.write_all(&payload)
        .and_then(|()| file.write_all(REPLAY_SPOOL_TRAILER_MAGIC))
        .and_then(|()| file.write_all(&(payload.len() as u64).to_le_bytes()))
        .and_then(|()| file.write_all(&hash))
        .and_then(|()| file.sync_all())
        .map_err(|error| ManagerError::new(format!("append replay spool payload: {error}")))?;
    Ok(hash)
}

fn load_replay_spool_payload(
    path: &[u8],
    expected_hash: [u8; 32],
    expected_count: i64,
) -> Result<Vec<Event>, ManagerError> {
    let path = PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()));
    let mut file = fs::File::open(&path)
        .map_err(|error| ManagerError::new(format!("open retained replay spool: {error}")))?;
    let length = file
        .metadata()
        .map_err(|error| ManagerError::new(format!("stat retained replay spool: {error}")))?
        .len();
    if length < REPLAY_SPOOL_TRAILER_LEN as u64 {
        return Err(ManagerError::new("retained replay spool has no trailer"));
    }
    file.seek(SeekFrom::End(-(REPLAY_SPOOL_TRAILER_LEN as i64)))
        .map_err(|error| ManagerError::new(format!("seek retained replay spool: {error}")))?;
    let mut trailer = [0_u8; REPLAY_SPOOL_TRAILER_LEN];
    file.read_exact(&mut trailer)
        .map_err(|error| ManagerError::new(format!("read retained replay trailer: {error}")))?;
    if &trailer[..16] != REPLAY_SPOOL_TRAILER_MAGIC {
        return Err(ManagerError::new(
            "retained replay spool trailer magic is invalid",
        ));
    }
    let payload_len = u64::from_le_bytes(trailer[16..24].try_into().unwrap());
    let trailer_hash: [u8; 32] = trailer[24..56].try_into().unwrap();
    if trailer_hash != expected_hash
        || payload_len > length.saturating_sub(REPLAY_SPOOL_TRAILER_LEN as u64)
    {
        return Err(ManagerError::new(
            "retained replay spool metadata is invalid",
        ));
    }
    file.seek(SeekFrom::End(
        -((REPLAY_SPOOL_TRAILER_LEN as u64 + payload_len) as i64),
    ))
    .map_err(|error| ManagerError::new(format!("seek replay payload: {error}")))?;
    let mut payload = vec![0; payload_len as usize];
    file.read_exact(&mut payload)
        .map_err(|error| ManagerError::new(format!("read replay payload: {error}")))?;
    let actual_hash: [u8; 32] = Sha256::digest(&payload).into();
    if actual_hash != expected_hash {
        return Err(ManagerError::new(
            "retained replay spool payload hash mismatches",
        ));
    }
    decode_replay_events(&payload, expected_count)
}

fn remove_replay_spool_files(paths: &[Vec<u8>]) {
    for path in paths {
        let path = PathBuf::from(std::ffi::OsString::from_vec(path.clone()));
        let _ = fs::remove_file(path);
    }
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_optional_u64(bytes: &mut Vec<u8>, value: Option<u64>) {
    bytes.push(u8::from(value.is_some()));
    if let Some(value) = value {
        push_u64(bytes, value);
    }
}

fn push_optional_bytes(bytes: &mut Vec<u8>, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            push_u64(bytes, value.len() as u64);
            bytes.extend_from_slice(value);
        }
        None => push_u64(bytes, u64::MAX),
    }
}

fn take_u64(bytes: &mut &[u8]) -> Result<u64, ManagerError> {
    let value = bytes
        .get(..8)
        .ok_or_else(|| ManagerError::new("truncated replay spool payload"))?;
    *bytes = &bytes[8..];
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

fn take_optional_u64(bytes: &mut &[u8]) -> Result<Option<u64>, ManagerError> {
    let present = *bytes
        .first()
        .ok_or_else(|| ManagerError::new("truncated replay spool payload"))?;
    *bytes = &bytes[1..];
    match present {
        0 => Ok(None),
        1 => Ok(Some(take_u64(bytes)?)),
        _ => Err(ManagerError::new("invalid replay optional integer")),
    }
}

fn take_optional_bytes(bytes: &mut &[u8]) -> Result<Option<Vec<u8>>, ManagerError> {
    let length = take_u64(bytes)?;
    if length == u64::MAX {
        return Ok(None);
    }
    let length = usize::try_from(length)
        .map_err(|_| ManagerError::new("replay path length exceeds memory limits"))?;
    let value = bytes
        .get(..length)
        .ok_or_else(|| ManagerError::new("truncated replay path"))?
        .to_vec();
    *bytes = &bytes[length..];
    Ok(Some(value))
}

fn decode_replay_events(bytes: &[u8], expected_count: i64) -> Result<Vec<Event>, ManagerError> {
    let mut bytes = bytes;
    let count = take_u64(&mut bytes)?;
    if count != u64::try_from(expected_count).unwrap_or(u64::MAX) {
        return Err(ManagerError::new(
            "replay spool event count mismatches catalog",
        ));
    }
    let mut events = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let kind = match bytes.first().copied() {
            Some(1) => EventKind::PathAdded,
            Some(2) => EventKind::PathRemoved,
            Some(3) => EventKind::PathChanged,
            Some(4) => EventKind::SubtreeMoved,
            Some(5) => EventKind::DirectoryDirtyWitness,
            _ => return Err(ManagerError::new("invalid replay event kind")),
        };
        bytes = &bytes[1..];
        events.push(Event {
            kind,
            ino: take_u64(&mut bytes)?,
            old_generation: take_optional_u64(&mut bytes)?,
            new_generation: take_optional_u64(&mut bytes)?,
            change_mask: take_u64(&mut bytes)?,
            old_path: take_optional_bytes(&mut bytes)?,
            new_path: take_optional_bytes(&mut bytes)?,
        });
    }
    if !bytes.is_empty() {
        return Err(ManagerError::new("replay spool payload has trailing bytes"));
    }
    Ok(events)
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

fn allocate_revision_id(transaction: &Transaction<'_>) -> Result<i64, ManagerError> {
    transaction
        .query_row(
            "SELECT COALESCE(MAX(id) + 1, 1) FROM revisions",
            [],
            |row| row.get(0),
        )
        .map_err(ManagerError::from)
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

fn path_is_absolute(path: &[u8]) -> bool {
    Path::new(std::ffi::OsStr::from_bytes(path)).is_absolute()
}

/// Removes cuts which never crossed the filesystem-effect boundary before
/// their lease expired. The conditional effect transition also checks the
/// lease, so once this transaction wins, no stale worker can issue an ioctl.
fn reap_expired_planned_cuts(
    transaction: &Transaction<'_>,
    now_ns: i64,
) -> Result<(), ManagerError> {
    transaction.execute(
        r#"UPDATE watches
              SET cut_owner = NULL, cut_expires_ns = NULL
            WHERE EXISTS (
                SELECT 1 FROM operations o
                 WHERE o.kind = 'cut' AND o.state = 'planned'
                   AND o.watch_id = watches.id
                   AND o.lease_owner = watches.cut_owner
                   AND o.lease_expires_ns <= ?1
            )"#,
        [now_ns],
    )?;
    transaction.execute(
        r#"DELETE FROM snapshot_pins
            WHERE owner_kind = 'operation' AND owner_id IN (
                SELECT id FROM operations
                 WHERE kind = 'cut' AND state = 'planned'
                   AND lease_expires_ns <= ?1
            )"#,
        [now_ns],
    )?;
    transaction.execute(
        r#"DELETE FROM cut_admissions WHERE operation_id IN (
                SELECT id FROM operations
                 WHERE kind = 'cut' AND state = 'planned'
                   AND lease_expires_ns <= ?1
            )"#,
        [now_ns],
    )?;
    transaction.execute(
        r#"DELETE FROM operations
            WHERE kind = 'cut' AND state = 'planned'
              AND lease_expires_ns <= ?1"#,
        [now_ns],
    )?;
    Ok(())
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
    use crate::manifest::CHANGE_REF;
    use crate::namespace::ViewBinding;
    use crate::store::ServiceMetadata;
    use tempfile::tempdir;

    #[test]
    fn retained_replay_spool_round_trips_event_payload() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("cut.spool");
        fs::write(&path, b"kernel-manifest-and-stage-trailer").unwrap();
        let events = vec![Event {
            kind: EventKind::SubtreeMoved,
            ino: 300,
            old_generation: Some(10),
            new_generation: Some(11),
            change_mask: CHANGE_REF,
            old_path: Some(b"old".to_vec()),
            new_path: Some(b"new".to_vec()),
        }];
        let hash = append_replay_spool_payload(&path, &events).unwrap();
        assert_eq!(
            load_replay_spool_payload(path.as_os_str().as_bytes(), hash, 1).unwrap(),
            events
        );
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
        let reservation = store.reserve_initialize(request).unwrap();
        store
            .start_initialize_filesystem_effect(&reservation, request.lease_owner, 150)
            .unwrap();
        let recorded = store
            .record_initialize_snapshot(&reservation, request.lease_owner, &snapshot(request), 200)
            .unwrap();
        let initialized = store
            .publish_initial_snapshot(&reservation, request.lease_owner, &recorded, None, 300)
            .unwrap();
        (reservation, initialized)
    }

    #[test]
    fn permissions_reject_retired_permission_bits() {
        assert!(Permissions::new(0x04).is_err());
        assert!(Permissions::new(0x08).is_err());
        assert!(Permissions::new(PERMISSION_READ | PERMISSION_CUT | 0x04).is_err());
        assert!(Permissions::new(PERMISSION_READ | PERMISSION_CUT | 0x08).is_err());
        assert!(Permissions::new(PERMISSION_READ | PERMISSION_CUT).is_ok());
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
    fn invalid_unpublished_cut_preserves_heads_and_records_a_gap() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let cut_request = CutRequest {
            watch_id: initialized.watch_id,
            authorization_id: initialized.grant_id,
            reserved_snapshot_path: b"/store/snapshots/w/invalid-cut".to_vec(),
            requester_uid: 1000,
            requester_gid: 1000,
            lease_owner: [6; 16],
            now_ns: 400,
            lease_expires_ns: 2_000,
        };
        let cut = store.reserve_cut(&cut_request).unwrap();
        store
            .start_cut_filesystem_effect(&cut, cut_request.lease_owner, 450)
            .unwrap();
        let recorded = store
            .record_cut_snapshot(
                &cut,
                cut_request.lease_owner,
                &cut_snapshot(&cut_request, request.source_subvol_uuid),
                500,
            )
            .unwrap();

        store
            .fail_unpublished_cut(
                &cut,
                cut_request.lease_owner,
                &recorded,
                "invalid immutable target",
                550,
            )
            .unwrap();

        let (last_cut_seq, indexed_seq, cut_owner): (i64, i64, Option<Vec<u8>>) = store
            .connection()
            .query_row(
                "SELECT last_cut_seq, indexed_seq, cut_owner FROM watches WHERE id = ?1",
                [initialized.watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((last_cut_seq, indexed_seq), (0, 0));
        assert!(cut_owner.is_none());
        let (operation_state, cut_state): (String, String) = store
            .connection()
            .query_row(
                r#"SELECT o.state, c.state
                     FROM operations o JOIN watch_cuts c ON c.operation_id = o.id
                    WHERE o.id = ?1"#,
                [cut.operation_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (operation_state.as_str(), cut_state.as_str()),
            ("failed", "failed")
        );
        let operation_pins: i64 = store
            .connection()
            .query_row(
                "SELECT count(*) FROM snapshot_pins WHERE owner_kind = 'operation' AND owner_id = ?1",
                [cut.operation_id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(operation_pins, 0);

        let next_request = CutRequest {
            reserved_snapshot_path: b"/store/snapshots/w/recovery-cut".to_vec(),
            lease_owner: [7; 16],
            now_ns: 600,
            ..cut_request
        };
        let next = store.reserve_cut(&next_request).unwrap();
        assert_eq!(next.sequence, 2);
        assert_eq!(next.base_snapshot_id, initialized.snapshot_id);
        assert!(store.foreign_key_violations().unwrap().is_empty());
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

        let reservation = store.reserve_initialize_started(&request).unwrap();
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
    fn startup_can_cancel_pre_broker_initialize_after_source_disappears() {
        let (_temp, mut store, request) = setup();
        let reservation = store.reserve_initialize(&request).unwrap();
        store
            .start_initialize_filesystem_effect(&reservation, request.lease_owner, 250)
            .unwrap();
        store
            .cancel_unstarted_initialize(&reservation, request.lease_owner)
            .unwrap();
        let state: (i64, i64, i64) = store
            .connection()
            .query_row(
                r#"SELECT
                       (SELECT count(*) FROM operations),
                       (SELECT count(*) FROM watches),
                       (SELECT count(*) FROM watch_grants)"#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, (0, 0, 0));
        assert!(store.foreign_key_violations().unwrap().is_empty());
    }

    #[test]
    fn active_watch_rebinds_after_live_subvolume_path_moves() {
        let (_temp, mut store, request) = setup();
        let (_reservation, initialized) = initialize_watch(&mut store, &request);
        let moved_path = b"/repo/moved";
        let rebound = store
            .rebind_active_uid_watch_path_by_subvolume(
                request.fs_uuid,
                request.source_subvol_uuid,
                moved_path,
                request.requester_uid,
                PERMISSION_READ | PERMISSION_CUT,
            )
            .unwrap();
        assert_eq!(rebound, Some((initialized.watch_id, initialized.grant_id)));
        assert_eq!(
            store
                .active_uid_watch_at_path(
                    moved_path,
                    request.requester_uid,
                    PERMISSION_READ | PERMISSION_CUT,
                )
                .unwrap(),
            Some((initialized.watch_id, initialized.grant_id))
        );
        assert_eq!(
            store
                .active_uid_watch_at_path(
                    &request.source_path,
                    request.requester_uid,
                    PERMISSION_READ | PERMISSION_CUT,
                )
                .unwrap(),
            None
        );
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
        assert!(
            store
                .revoke_grant(initialized.watch_id, initialized.grant_id, 450)
                .unwrap_err()
                .to_string()
                .contains("response lease")
        );
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

        assert!(
            store
                .invalidate_snapshot_facade(&activation)
                .unwrap_err()
                .to_string()
                .contains("response lease")
        );
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
        assert!(
            store
                .start_cut_filesystem_effect(&cut, [61; 16], 500)
                .is_err()
        );
    }

    #[test]
    fn expired_planned_cut_is_reaped_before_new_admission() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let expired_request = CutRequest {
            watch_id: initialized.watch_id,
            authorization_id: initialized.grant_id,
            reserved_snapshot_path: b"/store/snapshots/expired-cut".to_vec(),
            requester_uid: 1000,
            requester_gid: 1000,
            lease_owner: [63; 16],
            now_ns: 400,
            lease_expires_ns: 500,
        };
        let expired = store.reserve_cut(&expired_request).unwrap();
        store
            .admit_planned_cut(
                initialized.watch_id,
                initialized.grant_id,
                [64; 16],
                "query",
                450,
                550,
            )
            .unwrap()
            .unwrap();

        assert!(
            store
                .admit_planned_cut(
                    initialized.watch_id,
                    initialized.grant_id,
                    [65; 16],
                    "query",
                    600,
                    700,
                )
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .start_cut_filesystem_effect(&expired, expired_request.lease_owner, 600)
                .is_err()
        );
        let stale_rows: (i64, i64, i64) = store
            .connection()
            .query_row(
                r#"SELECT
                    (SELECT count(*) FROM operations WHERE id = ?1),
                    (SELECT count(*) FROM snapshot_pins
                      WHERE owner_kind = 'operation' AND owner_id = ?1),
                    (SELECT count(*) FROM cut_admissions WHERE operation_id = ?1)"#,
                [expired.operation_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stale_rows, (0, 0, 0));

        let fresh = store
            .reserve_cut(&CutRequest {
                watch_id: initialized.watch_id,
                authorization_id: initialized.grant_id,
                reserved_snapshot_path: b"/store/snapshots/fresh-cut".to_vec(),
                requester_uid: 1000,
                requester_gid: 1000,
                lease_owner: [66; 16],
                now_ns: 600,
                lease_expires_ns: 700,
            })
            .unwrap();
        assert_eq!(fresh.sequence, 1);
    }

    #[test]
    fn stale_fence_cannot_start_or_publish() {
        let (_temp, mut store, request) = setup();
        let reservation = store.reserve_initialize(&request).unwrap();
        assert!(
            store
                .start_initialize_filesystem_effect(&reservation, [99; 16], 150)
                .is_err()
        );
        store
            .start_initialize_filesystem_effect(&reservation, request.lease_owner, 150)
            .unwrap();
        let recorded = store
            .record_initialize_snapshot(&reservation, request.lease_owner, &snapshot(&request), 200)
            .unwrap();
        assert!(
            store
                .publish_initial_snapshot(&reservation, [99; 16], &recorded, None, 300)
                .is_err()
        );
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
        assert!(
            store
                .reserve_unpinned_snapshot_deletes([20; 16], 400, 1000, 4)
                .unwrap()
                .is_empty()
        );

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
        assert!(
            store
                .start_snapshot_delete(reservation, [21; 16], 450)
                .is_err()
        );
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
                   VALUES (?1, ?1, 'incremental', 3, 'claimed', ?2, 4, 9999)"#,
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
        assert!(
            store
                .append_precision_events(
                    &activation,
                    initial.epoch,
                    &[MutationHint::Path(b"too-late".to_vec())],
                    600,
                )
                .is_err()
        );
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
        assert!(
            store
                .append_precision_events(&activation, cursor.epoch, &[oversized], 500)
                .is_err()
        );
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
}
