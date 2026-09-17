use crate::btrfs::{OpenedSubvolume, SubvolumeInfo};
use crate::index::{Event, EventKind};
use crate::manifest::ChangedObjectsManifest;
use crate::namespace::ViewBinding;
use crate::store::{Store, StoreError, decode_u64, encode_u64};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
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

/// A validated watch-owned Git cursor received as a query input. Creating or
/// returning a new cursor is not evidence that it was persisted or queried.
#[derive(Clone, Debug)]
pub struct GitCursorObservation {
    pub watch_id: [u8; 16],
    pub authorization_id: [u8; 16],
    pub filesystem_uuid: [u8; 16],
    pub snapshot_uuid: [u8; 16],
    pub clock_epoch: [u8; 16],
    pub sequence: i64,
    pub now_ns: i64,
    pub grace_ns: i64,
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
    /// watch follows that identity. Its stored namespace paths, including
    /// managed snapshots and replay spools that moved with the root, must
    /// change together before service recovery can open them. The caller must
    /// hold the root-state lock and supply identities observed from the live
    /// root, not identities inferred from its pathname.
    pub fn rebind_active_uid_watch_path_by_subvolume(
        &mut self,
        fs_uuid: [u8; 16],
        live_subvol_uuid: [u8; 16],
        live_path: &[u8],
        requester_uid: u32,
        required_permissions: u8,
    ) -> Result<Option<WatchGrantIds>, ManagerError> {
        self.rebind_active_uid_watch_path_with_snapshot_validator(
            fs_uuid,
            live_subvol_uuid,
            live_path,
            requester_uid,
            required_permissions,
            verify_relocated_snapshot,
        )
    }

    fn rebind_active_uid_watch_path_with_snapshot_validator<F>(
        &mut self,
        fs_uuid: [u8; 16],
        live_subvol_uuid: [u8; 16],
        live_path: &[u8],
        requester_uid: u32,
        required_permissions: u8,
        mut verify_snapshot: F,
    ) -> Result<Option<WatchGrantIds>, ManagerError>
    where
        F: FnMut(&Path, [u8; 16], [u8; 16], &str) -> Result<(), ManagerError>,
    {
        if !path_is_absolute(live_path)
            || !namespace_path_is_normal(live_path)
            || required_permissions == 0
            || required_permissions & !PERMISSION_MASK != 0
        {
            return Err(ManagerError::new("invalid active-watch rebind"));
        }
        let principal = encode_u64(u64::from(requester_uid));
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>)> = transaction
            .query_row(
                r#"SELECT w.id, g.id, w.live_path, w.fsmonitor_root
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
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((watch, grant, old_path, fsmonitor_root)) = row else {
            transaction.commit()?;
            return Ok(None);
        };
        let ids = (
            fixed_manager_blob(&watch, "watch ID")?,
            fixed_manager_blob(&grant, "grant ID")?,
        );
        if old_path == live_path {
            transaction.commit()?;
            return Ok(Some(ids));
        }
        // This is a per-root manager operation, not authorization to move
        // other live watches or their storage. A shared manager needs a
        // separate relocation protocol that validates every affected watch.
        let other_watches: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM watches WHERE id != ?1 AND state != 'deleted')",
            [watch.as_slice()],
            |row| row.get(0),
        )?;
        if other_watches {
            return Err(ManagerError::new(
                "cannot relocate a root in a manager with multiple live watches",
            ));
        }
        if !namespace_path_is_normal(&old_path) {
            return Err(ManagerError::new(
                "stored watch root is not a normal absolute path",
            ));
        }
        let old_root = Path::new(std::ffi::OsStr::from_bytes(&old_path));
        let new_root = Path::new(std::ffi::OsStr::from_bytes(live_path));
        let snapshots = {
            let mut statement = transaction.prepare(
                "SELECT s.id, f.fs_uuid, s.subvol_uuid, s.path, s.physical_state FROM snapshots s \
                 JOIN filesystems f ON f.id = s.filesystem_id",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (id, filesystem, uuid, path, state) in snapshots {
            let Some(relocated) = relocated_namespace_path(&path, old_root, new_root)? else {
                continue;
            };
            verify_snapshot(
                &relocated,
                fixed_manager_blob(&filesystem, "snapshot filesystem UUID")?,
                fixed_manager_blob(&uuid, "snapshot UUID")?,
                &state,
            )?;
            transaction.execute(
                "UPDATE snapshots SET path = ?2 WHERE id = ?1",
                params![id, relocated.as_os_str().as_bytes()],
            )?;
        }
        // These are absolute storage paths. Event paths in change_events and
        // mutation_events describe names inside immutable trees and must not
        // be rewritten when their container moves.
        for (table, column) in [("operations", "reserved_path"), ("replay_spools", "path")] {
            let paths = {
                let mut statement =
                    transaction.prepare(&format!("SELECT DISTINCT {column} FROM {table}"))?;
                statement
                    .query_map([], |row| row.get::<_, Vec<u8>>(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for path in paths {
                if let Some(relocated) = relocated_namespace_path(&path, old_root, new_root)? {
                    transaction.execute(
                        &format!("UPDATE {table} SET {column} = ?2 WHERE {column} = ?1"),
                        params![path, relocated.as_os_str().as_bytes()],
                    )?;
                }
            }
        }
        let fsmonitor_root = fsmonitor_root
            .map(|path| {
                Ok::<_, ManagerError>(
                    relocated_namespace_path(&path, old_root, new_root)?
                        .map_or(path, |path| path.into_os_string().into_vec()),
                )
            })
            .transpose()?;
        require_one(
            transaction.execute(
                "UPDATE watches SET live_path = ?2, fsmonitor_root = ?3 WHERE id = ?1 AND state = \
                 'active'",
                params![watch.as_slice(), live_path, fsmonitor_root],
            )?,
            "rebind active watch path",
        )?;
        transaction.commit()?;
        Ok(Some(ids))
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

    /// Stages a validated immutable cut without advancing either published
    /// watch head. The target remains operation-pinned and recoverable until
    /// publish_adjacent_broker_delta() commits the physical and indexed heads
    /// together.
    pub fn stage_validated_physical_cut(
        &mut self,
        reservation: &CutReservation,
        lease_owner: [u8; 16],
        snapshot: &RecordedSnapshot,
        now_ns: i64,
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        stage_validated_physical_cut_in_transaction(
            &transaction,
            reservation,
            lease_owner,
            snapshot,
            now_ns,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Publishes an adjacent immutable comparison whose exact path events were
    /// derived from broker-resolved endpoint paths. A newly recorded cut is
    /// staged in this transaction; already-staged cuts use the same recovery
    /// path. Failure leaves the original operation state and pins intact.
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
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unstaged: bool = transaction.query_row(
            "SELECT state = 'uuid_recorded' FROM operations WHERE id = ?1",
            [reservation.operation_id.as_slice()],
            |row| row.get(0),
        )?;
        if unstaged {
            // The durable replay spool already exists. Stage its cut and
            // publish both heads under the same writer fence and commit.
            stage_validated_physical_cut_in_transaction(
                &transaction,
                reservation,
                lease_owner,
                snapshot,
                now_ns,
            )?;
        }
        let (base_revision_id, base_snapshot_id, base_sequence): (i64, i64, i64) = transaction
            .query_row(
                r#"SELECT w.indexed_revision_id, r.snapshot_id, w.indexed_seq
                     FROM watches w JOIN revisions r ON r.id = w.indexed_revision_id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND w.indexed_seq < ?2 AND r.state = 'ready'"#,
                params![reservation.watch_id.as_slice(), reservation.sequence],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| ManagerError::new("prior cut is not the ready indexed head"))?;
        if base_snapshot_id != reservation.base_snapshot_id {
            return Err(ManagerError::new(
                "cut base snapshot differs from indexed predecessor",
            ));
        }
        if replay_spool.is_some() {
            // The durable spool owns the replay payload. Keep the validation
            // previously provided by staging, without inserting every row
            // into SQLite only to delete it again in this transaction.
            validate_spooled_delta_rows(manifest, events)?;
        } else {
            stage_broker_delta_rows_in_transaction(&transaction, manifest, events)?;
        }
        verify_delta_publication(
            &transaction,
            reservation,
            lease_owner,
            snapshot.snapshot_id,
            base_revision_id,
            base_sequence,
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
        if replay_spool.is_none() {
            import_staged_comparison_rows(&transaction, comparison_id)?;
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
        transaction.execute(
            r#"INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason)
               VALUES (?1, 'watch-last-cut', ?2, 'physical-head')"#,
            params![snapshot.snapshot_id, reservation.watch_id.as_slice()],
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE watches
                      SET indexed_revision_id = ?2, indexed_seq = ?3,
                          last_cut_snapshot_id = ?4, last_cut_seq = ?3,
                          cut_owner = NULL, cut_expires_ns = NULL
                    WHERE id = ?1 AND state = 'active'
                      AND indexed_revision_id = ?5 AND indexed_seq = ?6
                      AND last_cut_snapshot_id = ?7 AND last_cut_seq = ?6
                      AND cut_owner = ?8 AND cut_fence = ?9"#,
                params![
                    reservation.watch_id.as_slice(),
                    revision_id,
                    reservation.sequence,
                    snapshot.snapshot_id,
                    base_revision_id,
                    base_sequence,
                    reservation.base_snapshot_id,
                    lease_owner.as_slice(),
                    reservation.cut_fence,
                ],
            )?,
            "advance published watch heads",
        )?;
        require_one(
            transaction.execute(
                r#"UPDATE watch_cuts
                      SET comparison_id = ?3, comparison_from_snapshot_id = ?4,
                          base_sequence = ?6, state = 'ready'
                    WHERE watch_id = ?1 AND sequence = ?2 AND state = 'created'
                      AND target_snapshot_id = ?5"#,
                params![
                    reservation.watch_id.as_slice(),
                    reservation.sequence,
                    comparison_id,
                    base_snapshot_id,
                    snapshot.snapshot_id,
                    base_sequence,
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
                WHERE snapshot_id = ?1 AND owner_kind = 'watch-last-cut'
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
        if replay_spool.is_none() {
            let _ = clear_staged_delta(self.connection_mut());
        }
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
    /// The failed cut row preserves the attempt's sequence while the watch
    /// keeps its prior physical and indexed heads. The next successful cut
    /// records that actual predecessor, bypassing the failed attempt without
    /// losing replay continuity. Releasing operation pins leaves the rejected
    /// target eligible for the ordinary fenced snapshot-GC path.
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

    /// Protects a Git cursor after a valid query input or verified published
    /// index readback. Validation and observation share the GC writer
    /// transaction, so
    /// a concurrent cleanup cannot prune the cursor between those steps.
    ///
    /// Supersession starts the old head's grace once. Later valid queries of
    /// that older cursor extend its own deadline; issuing new cursors or
    /// observing still-newer heads does not extend already-superseded ones.
    pub fn record_git_cursor_observation(
        &mut self,
        observation: &GitCursorObservation,
    ) -> Result<bool, ManagerError> {
        if observation.sequence <= 0 || observation.grace_ns <= 0 {
            return Err(ManagerError::new("invalid Git cursor observation"));
        }
        let deadline = observation
            .now_ns
            .checked_add(observation.grace_ns)
            .ok_or_else(|| ManagerError::new("Git cursor grace deadline overflow"))?;
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let watch: Option<(i64, i64, Vec<u8>)> = transaction
            .query_row(
                r#"SELECT w.indexed_seq, w.replay_floor_seq, f.fs_uuid
                     FROM watches w
                     JOIN watch_grants g ON g.watch_id = w.id
                     JOIN filesystems f ON f.id = w.filesystem_id
                    WHERE w.id = ?1 AND w.state = 'active'
                      AND w.fsmonitor_owner_grant_id = ?2
                      AND g.id = ?2 AND g.state = 'active'
                      AND (g.permissions & ?3) = ?3"#,
                params![
                    observation.watch_id.as_slice(),
                    observation.authorization_id.as_slice(),
                    i64::from(PERMISSION_READ | PERMISSION_CUT),
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (indexed_sequence, replay_floor, filesystem_uuid) = watch
            .ok_or_else(|| ManagerError::new("Git cursor observation authorization is stale"))?;
        if filesystem_uuid != observation.filesystem_uuid {
            return Err(ManagerError::new(
                "Git cursor observation filesystem differs",
            ));
        }
        if observation.sequence > indexed_sequence {
            return Err(ManagerError::new(
                "Git cursor observation is newer than the indexed head",
            ));
        }
        if observation.sequence < replay_floor {
            return Ok(false);
        }
        let boundary: Option<(Vec<u8>, Vec<u8>)> = transaction
            .query_row(
                r#"SELECT b.clock_epoch, s.subvol_uuid
                     FROM fsmonitor_boundaries b
                     JOIN watch_cuts c ON c.watch_id = b.watch_id AND c.sequence = b.cut_sequence
                     JOIN snapshots s ON s.id = b.target_snapshot_id
                    WHERE b.watch_id = ?1 AND b.cut_sequence = ?2 AND c.state = 'ready'"#,
                params![observation.watch_id.as_slice(), observation.sequence],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((epoch, snapshot_uuid)) = boundary else {
            return Ok(false);
        };
        if epoch != observation.clock_epoch || snapshot_uuid != observation.snapshot_uuid {
            return Err(ManagerError::new(
                "Git cursor observation boundary identity differs",
            ));
        }
        if !has_complete_retained_replay(
            &transaction,
            observation.watch_id,
            observation.sequence,
            indexed_sequence,
        )? {
            return Ok(false);
        }
        let head: Option<(i64, i64)> = transaction
            .query_row(
                "SELECT cut_sequence, last_queried_ns FROM git_cursor_observations \
                  WHERE watch_id = ?1 AND retain_until_ns IS NULL",
                [observation.watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let is_head = head.is_none_or(|(sequence, _)| observation.sequence >= sequence);
        if let Some((sequence, last_queried_ns)) = head {
            if observation.sequence > sequence {
                let superseded_until = last_queried_ns
                    .max(observation.now_ns)
                    .checked_add(observation.grace_ns)
                    .ok_or_else(|| {
                        ManagerError::new("Git cursor supersession deadline overflow")
                    })?;
                transaction.execute(
                    "UPDATE git_cursor_observations SET retain_until_ns = ?3 \
                      WHERE watch_id = ?1 AND cut_sequence = ?2 AND retain_until_ns IS NULL",
                    params![observation.watch_id.as_slice(), sequence, superseded_until],
                )?;
            }
        }
        let retain_until = (!is_head).then_some(deadline);
        transaction.execute(
            r#"INSERT INTO git_cursor_observations(
                   watch_id, cut_sequence, last_queried_ns, retain_until_ns
               ) VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT(watch_id, cut_sequence) DO UPDATE SET
                   last_queried_ns = max(last_queried_ns, excluded.last_queried_ns),
                   retain_until_ns = CASE
                       WHEN excluded.retain_until_ns IS NULL THEN NULL
                       ELSE max(retain_until_ns, excluded.retain_until_ns)
                   END"#,
            params![
                observation.watch_id.as_slice(),
                observation.sequence,
                observation.now_ns,
                retain_until,
            ],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn advance_replay_floor(
        &mut self,
        watch_id: [u8; 16],
        new_floor: i64,
        now_ns: i64,
        _compactor_owner: [u8; 16],
    ) -> Result<usize, ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current_floor, indexed_sequence): (i64, i64) = transaction
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
                  AND COALESCE(from_cut_sequence, to_cut_sequence) < ?2"#,
            params![watch_id.as_slice(), new_floor],
            |row| row.get(0),
        )?;
        if conflicting_queries != 0 {
            return Err(ManagerError::new(
                "active query lease still protects history below the requested floor",
            ));
        }
        transaction.execute(
            "DELETE FROM git_cursor_observations WHERE watch_id = ?1 AND retain_until_ns <= ?2",
            params![watch_id.as_slice(), now_ns],
        )?;
        if oldest_protected_replay_sequence(&transaction, watch_id, now_ns)?
            .is_some_and(|sequence| sequence < new_floor)
        {
            return Err(ManagerError::new(
                "a durable consumer or observed Git cursor still protects history below the requested floor",
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
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Expiration makes an older observation eligible for cleanup, not an
        // invalid cursor. Until its boundary is pruned, a later valid query
        // may observe it again. Remaining expired rows conservatively protect
        // history when the bounded deletion budget is exhausted.
        transaction.execute(
            r#"DELETE FROM git_cursor_observations
                WHERE watch_id = ?1 AND cut_sequence IN (
                    SELECT cut_sequence FROM git_cursor_observations
                     WHERE watch_id = ?1 AND retain_until_ns <= ?2
                     ORDER BY retain_until_ns, cut_sequence LIMIT ?3
                )"#,
            params![
                watch_id.as_slice(),
                now_ns,
                i64::try_from(delete_limit)
                    .map_err(|_| ManagerError::new("observation cleanup limit overflow"))?,
            ],
        )?;
        let newest: Option<i64> = transaction.query_row(
            "SELECT max(cut_sequence) FROM fsmonitor_boundaries WHERE watch_id = ?1",
            [watch_id.as_slice()],
            |row| row.get(0),
        )?;
        let Some(newest) = newest else {
            transaction.commit()?;
            return Ok(0);
        };
        let protected_sequence = oldest_protected_replay_sequence(&transaction, watch_id, now_ns)?;
        let oldest_window_sequence = newest.saturating_sub(max_cuts);
        let oldest_window_created_ns = now_ns.saturating_sub(max_age_ns);
        // Preserve the whole replay interval from the oldest protected input,
        // not just its endpoint. The fixed cache window is supplemental. All
        // reads and removals share the writer transaction with observations
        // and query admission, so newly protected history cannot be pruned.
        let removed_sequences = {
            let mut statement = transaction.prepare(
                r#"SELECT b.cut_sequence
                     FROM fsmonitor_boundaries b
                     JOIN snapshots s ON s.id = b.target_snapshot_id
                    WHERE b.watch_id = ?1 AND b.cut_sequence < ?2
                      AND (?3 IS NULL OR b.cut_sequence < ?3)
                      AND NOT (b.cut_sequence > ?4 AND s.created_ns >= ?5)
                    ORDER BY b.cut_sequence LIMIT ?6"#,
            )?;
            statement
                .query_map(
                    params![
                        watch_id.as_slice(),
                        newest,
                        protected_sequence,
                        oldest_window_sequence,
                        oldest_window_created_ns,
                        i64::try_from(delete_limit)
                            .map_err(|_| ManagerError::new("boundary cleanup limit overflow"))?,
                    ],
                    |row| row.get::<_, i64>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        for sequence in &removed_sequences {
            transaction.execute(
                "DELETE FROM fsmonitor_boundaries WHERE watch_id = ?1 AND cut_sequence = ?2",
                params![watch_id.as_slice(), sequence],
            )?;
        }
        let removed = removed_sequences.len();
        let replay_floor: i64 = transaction.query_row(
            r#"SELECT COALESCE(c.base_sequence, c.sequence - 1)
                     FROM fsmonitor_boundaries b
                     JOIN watch_cuts c ON c.watch_id = b.watch_id
                                      AND c.sequence = b.cut_sequence
                    WHERE b.watch_id = ?1 AND c.state = 'ready'
                    ORDER BY b.cut_sequence LIMIT 1"#,
            [watch_id.as_slice()],
            |row| row.get(0),
        )?;
        // The first retained transition also covers its actual base, which
        // may be several sequence numbers earlier after failed attempts.
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
    /// adopted when the journal names it and discarded when the journal still
    /// names the committed baseline. Git also retains the preceding on-disk
    /// index token because its hook cannot know whether the newer cursor was
    /// actually written to the index.
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
        let git_index: Option<i64> = transaction
            .query_row(
                "SELECT snapshot_id FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'git-index'",
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
                    "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason IN ('committed', 'git-index')",
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
                    "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason IN ('pending', 'git-index')",
                    [owner_id.as_slice()],
                )?;
                true
            }
            Some(previous) if git_index == Some(previous) => {
                transaction.execute(
                    "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason IN ('committed', 'pending')",
                    [owner_id.as_slice()],
                )?;
                require_one(
                    transaction.execute(
                        "UPDATE snapshot_pins SET reason = 'committed' WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'git-index'",
                        [owner_id.as_slice()],
                    )?,
                    "restore Git index baseline",
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
    ///
    /// Git's watch-owned lane retains its prior index baseline until the next
    /// query proves whether the replacement cursor was actually written.
    pub fn finish_consumer_baseline(
        &mut self,
        owner_id: [u8; 16],
        committed: bool,
    ) -> Result<(), ManagerError> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if committed {
            let git_lane: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM watches WHERE id = ?1 AND state = 'active')",
                [owner_id.as_slice()],
                |row| row.get(0),
            )?;
            transaction.execute(
                "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'git-index'",
                [owner_id.as_slice()],
            )?;
            if git_lane {
                transaction.execute(
                    "UPDATE snapshot_pins SET reason = 'git-index' WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'committed'",
                    [owner_id.as_slice()],
                )?;
            } else {
                transaction.execute(
                    "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 AND reason = 'committed'",
                    [owner_id.as_slice()],
                )?;
            }
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
        if from_sequence >= replay_floor
            && retained_replay_storage(&transaction, watch_id, from_sequence, to_sequence)?
                == Some(RetainedReplayStorage::Spools)
        {
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
        let fresh_instance = from_sequence < replay_floor
            || !has_complete_retained_replay(&transaction, watch_id, from_sequence, to_sequence)?;
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
                        AND c.state = 'ready'
                     UNION ALL
                     SELECT COALESCE(c.base_sequence, c.sequence - 1) AS sequence
                       FROM watch_cuts c
                       JOIN snapshots s ON s.id = c.base_snapshot_id
                      WHERE c.watch_id = ?1 AND s.subvol_uuid = ?2
                        AND c.state = 'ready'
                     UNION ALL
                     SELECT w.indexed_seq AS sequence
                       FROM watches w
                       JOIN snapshots s ON s.id = w.last_cut_snapshot_id
                      WHERE w.id = ?1 AND s.subvol_uuid = ?2
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

/// The complete suffix from this position is needed by a durable consumer,
/// a validated Git input, or a still-active query. Call under the GC writer
/// transaction so observing or admitting a query cannot race the decision.
fn oldest_protected_replay_sequence(
    transaction: &Transaction<'_>,
    watch_id: [u8; 16],
    now_ns: i64,
) -> Result<Option<i64>, ManagerError> {
    transaction
        .query_row(
            r#"SELECT min(sequence)
                 FROM (
                     SELECT cut_sequence AS sequence
                       FROM git_cursor_observations WHERE watch_id = ?1
                     UNION ALL
                     SELECT COALESCE(from_cut_sequence, to_cut_sequence)
                       FROM query_leases
                      WHERE watch_id = ?1 AND state = 'active' AND lease_expires_ns > ?2
                     UNION ALL
                     SELECT b.cut_sequence
                       FROM fsmonitor_boundaries b
                       JOIN snapshot_pins p ON p.snapshot_id = b.target_snapshot_id
                      WHERE b.watch_id = ?1 AND p.owner_kind = 'consumer-baseline'
                     UNION ALL
                     SELECT COALESCE(c.base_sequence, c.sequence - 1)
                       FROM watch_cuts c
                       JOIN snapshot_pins p ON p.snapshot_id = c.base_snapshot_id
                      WHERE c.watch_id = ?1 AND c.state = 'ready'
                        AND p.owner_kind = 'consumer-baseline'
                     UNION ALL
                     SELECT w.indexed_seq
                       FROM watches w
                       JOIN snapshot_pins p ON p.snapshot_id = w.last_cut_snapshot_id
                      WHERE w.id = ?1 AND p.owner_kind = 'consumer-baseline'
                     UNION ALL
                     SELECT 0
                       FROM watches w
                       JOIN snapshots s ON s.filesystem_id = w.filesystem_id
                                       AND s.subvol_uuid = w.inherited_baseline_snapshot_uuid
                       JOIN snapshot_pins p ON p.snapshot_id = s.id
                      WHERE w.id = ?1 AND p.owner_kind = 'consumer-baseline'
                 ) protected_endpoints"#,
            params![watch_id.as_slice(), now_ns],
            |row| row.get(0),
        )
        .map_err(ManagerError::from)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetainedReplayStorage {
    Legacy,
    Spools,
}

/// Checks the retained metadata chain without opening snapshots or payloads.
/// Callers must separately enforce cursor authorization and the replay floor.
/// This also supports read-only status inspection of an unmigrated v12 store.
pub fn has_complete_retained_replay(
    connection: &rusqlite::Connection,
    watch_id: [u8; 16],
    from_sequence: i64,
    to_sequence: i64,
) -> Result<bool, ManagerError> {
    Ok(retained_replay_storage(connection, watch_id, from_sequence, to_sequence)?.is_some())
}

fn retained_replay_storage(
    connection: &rusqlite::Connection,
    watch_id: [u8; 16],
    from_sequence: i64,
    to_sequence: i64,
) -> Result<Option<RetainedReplayStorage>, ManagerError> {
    if from_sequence < 0 || to_sequence < from_sequence {
        return Ok(None);
    }
    if from_sequence == to_sequence {
        return Ok(Some(RetainedReplayStorage::Legacy));
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    // A pre-v13 publisher already holding an open connection can still
    // publish an adjacent cut with NULL base_sequence after migration. Only
    // ready rows get this fallback: that publisher required sequence - 1.
    let predecessor = if version >= 13 {
        "COALESCE(c.base_sequence, c.sequence - 1)"
    } else {
        "c.sequence - 1"
    };
    let mut statement = connection.prepare(&format!(
        r#"SELECT c.sequence, {predecessor}, c.base_snapshot_id,
                  c.target_snapshot_id, c.fresh_instance, c.comparison_id,
                  p.from_snapshot_id, p.to_snapshot_id
             FROM watch_cuts c
             LEFT JOIN replay_spools p ON p.watch_id = c.watch_id AND p.sequence = c.sequence
            WHERE c.watch_id = ?1 AND c.state = 'ready'
              AND c.sequence >= ?2 AND c.sequence <= ?3
            ORDER BY c.sequence"#
    ))?;
    let mut rows = statement.query(params![watch_id.as_slice(), from_sequence, to_sequence])?;
    let mut sequence = from_sequence;
    let mut snapshot_id = None;
    let mut storage = None;
    while let Some(row) = rows.next()? {
        let target_sequence: i64 = row.get(0)?;
        let target_snapshot_id: i64 = row.get(3)?;
        if target_sequence == from_sequence {
            // If its cut is retained, it independently anchors the source.
            // Otherwise the first retained edge owns its exact base identity,
            // including the child's sequence-zero inherited-baseline alias.
            snapshot_id = Some(target_snapshot_id);
            continue;
        }
        let base_sequence: i64 = row.get(1)?;
        let base_snapshot_id: i64 = row.get(2)?;
        let fresh_instance: bool = row.get(4)?;
        let comparison_id: Option<i64> = row.get(5)?;
        if base_sequence != sequence
            || snapshot_id.is_some_and(|snapshot| snapshot != base_snapshot_id)
            || fresh_instance
            || comparison_id.is_none()
        {
            return Ok(None);
        }
        let spool_from: Option<i64> = row.get(6)?;
        let spool_to: Option<i64> = row.get(7)?;
        let edge_storage = match (spool_from, spool_to) {
            (Some(from), Some(to)) if from == base_snapshot_id && to == target_snapshot_id => {
                RetainedReplayStorage::Spools
            }
            (None, None) => RetainedReplayStorage::Legacy,
            _ => return Ok(None),
        };
        // Keep old SQLite-only history readable, but never mistake a mixed
        // range with a missing spool for a complete replay.
        if storage.is_some_and(|previous| previous != edge_storage) {
            return Ok(None);
        }
        storage = Some(edge_storage);
        sequence = target_sequence;
        snapshot_id = Some(target_snapshot_id);
    }
    Ok(if sequence == to_sequence {
        storage
    } else {
        None
    })
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

fn stage_validated_physical_cut_in_transaction(
    transaction: &Transaction<'_>,
    reservation: &CutReservation,
    lease_owner: [u8; 16],
    snapshot: &RecordedSnapshot,
    now_ns: i64,
) -> Result<(), ManagerError> {
    verify_cut_snapshot(
        transaction,
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
        "UPDATE operations SET state = 'manifest_ready', updated_ns = ?2 WHERE id = ?1",
        params![reservation.operation_id.as_slice(), now_ns],
    )?;
    Ok(())
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
    base_sequence: i64,
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
                  AND w.indexed_revision_id = ?7 AND w.indexed_seq = ?8
                  AND o.base_snapshot_id = ?9 AND c.base_snapshot_id = ?9"#,
            params![
                reservation.operation_id.as_slice(),
                reservation.watch_id.as_slice(),
                reservation.sequence,
                lease_owner.as_slice(),
                reservation.operation_fence,
                snapshot_id,
                base_revision_id,
                base_sequence,
                reservation.base_snapshot_id,
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

fn validate_spooled_delta_rows(
    manifest: &ChangedObjectsManifest,
    events: &[Event],
) -> Result<(), ManagerError> {
    for change in manifest.objects.values() {
        i64::try_from(change.change_mask)
            .map_err(|_| ManagerError::new("change mask exceeds SQLite INTEGER"))?;
    }
    // Parsed manifests are keyed by inode, so the map already proves
    // uniqueness. Retain the staging table's primary-key check for callers
    // that construct a manifest with different map keys.
    if manifest
        .objects
        .iter()
        .any(|(ino, change)| *ino != change.ino)
    {
        let mut inodes = std::collections::HashSet::with_capacity(manifest.objects.len());
        for change in manifest.objects.values() {
            if !inodes.insert(change.ino) {
                return Err(ManagerError::new("duplicate changed-object inode"));
            }
        }
    }
    for event in events {
        i64::try_from(event.change_mask)
            .map_err(|_| ManagerError::new("event mask exceeds SQLite INTEGER"))?;
    }
    Ok(())
}

fn stage_broker_delta_rows(
    connection: &mut rusqlite::Connection,
    manifest: &ChangedObjectsManifest,
    events: &[Event],
) -> Result<(), ManagerError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    stage_broker_delta_rows_in_transaction(&transaction, manifest, events)?;
    transaction.commit()?;
    Ok(())
}

fn stage_broker_delta_rows_in_transaction(
    transaction: &Transaction<'_>,
    manifest: &ChangedObjectsManifest,
    events: &[Event],
) -> Result<(), ManagerError> {
    transaction.execute_batch(
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

fn namespace_path_is_normal(path: &[u8]) -> bool {
    if !path_is_absolute(path) {
        return false;
    }
    let path = Path::new(std::ffi::OsStr::from_bytes(path));
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
}

fn relocated_namespace_path(
    path: &[u8],
    old_root: &Path,
    new_root: &Path,
) -> Result<Option<PathBuf>, ManagerError> {
    let stored = Path::new(std::ffi::OsStr::from_bytes(path));
    let Ok(relative) = stored.strip_prefix(old_root) else {
        // Explicitly configured managed directories can live outside the
        // checkout. They did not move with the root.
        return Ok(None);
    };
    if !path_is_absolute(path) || !namespace_path_is_normal(path) {
        return Err(ManagerError::new(
            "stored namespace path is not a normal absolute path",
        ));
    }
    Ok(Some(if relative.as_os_str().is_empty() {
        new_root.to_owned()
    } else {
        new_root.join(relative)
    }))
}

fn verify_relocated_snapshot(
    path: &Path,
    filesystem_uuid: [u8; 16],
    snapshot_uuid: [u8; 16],
    state: &str,
) -> Result<(), ManagerError> {
    match fs::symlink_metadata(path) {
        Ok(_) if matches!(state, "deleted" | "lost") => {
            return Err(ManagerError::new(
                "tombstoned snapshot reappeared at relocated path",
            ));
        }
        Ok(_) => {}
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && matches!(state, "creating" | "deleting" | "deleted" | "lost") =>
        {
            // Recovery may still need to finish an interrupted physical
            // create/delete, and tombstones have no physical tree to verify.
            return Ok(());
        }
        Err(error) => {
            return Err(ManagerError::new(format!(
                "inspect relocated snapshot: {error}"
            )));
        }
    }
    let snapshot = OpenedSubvolume::open(path)
        .map_err(|error| ManagerError::new(format!("open relocated snapshot: {error}")))?;
    if snapshot.filesystem.fs_uuid != filesystem_uuid
        || snapshot.subvolume.uuid != snapshot_uuid
        || !snapshot.subvolume.readonly()
    {
        return Err(ManagerError::new(
            "relocated snapshot identity or read-only state differs",
        ));
    }
    Ok(())
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
    use std::collections::BTreeSet;
    use tempfile::tempdir;

    include!("manager_publication_benchmark.rs");

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

    #[test]
    fn git_consumer_retains_the_cursor_still_persisted_in_its_index() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let first_snapshot = initialized.snapshot_id;
        store
            .connection_mut()
            .execute(
                r#"INSERT INTO snapshots(
                       filesystem_id, subvol_uuid, parent_uuid, received_uuid,
                       root_id, ctransid, otransid, path, readonly,
                       physical_state, created_ns, deleted_ns)
                   SELECT filesystem_id, ?1, parent_uuid, received_uuid,
                          root_id, ctransid, otransid, ?2, readonly,
                          physical_state, created_ns, deleted_ns
                     FROM snapshots WHERE id = ?3"#,
                params![
                    [91_u8; 16].as_slice(),
                    b"/store/snapshots/w/git-next".as_slice(),
                    first_snapshot
                ],
            )
            .unwrap();
        let second_snapshot = store.connection().last_insert_rowid();
        let owner = initialized.watch_id;
        for (snapshot, reason) in [(first_snapshot, "committed"), (second_snapshot, "pending")] {
            store
                .connection_mut()
                .execute(
                    "INSERT INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) \
                     VALUES (?1, 'consumer-baseline', ?2, ?3)",
                    params![snapshot, owner.as_slice(), reason],
                )
                .unwrap();
        }

        store.finish_consumer_baseline(owner, true).unwrap();
        let pins = |store: &Store| {
            let mut statement = store
                .connection()
                .prepare(
                    "SELECT snapshot_id, reason FROM snapshot_pins \
                     WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 \
                     ORDER BY snapshot_id",
                )
                .unwrap();
            statement
                .query_map([owner.as_slice()], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<Vec<(i64, String)>, _>>()
                .unwrap()
        };
        assert_eq!(
            pins(&store),
            vec![
                (first_snapshot, "git-index".to_owned()),
                (second_snapshot, "committed".to_owned())
            ],
            "Git must retain both the still-persisted cursor and its proposed replacement"
        );

        assert!(
            store
                .reconcile_consumer_baseline(
                    initialized.watch_id,
                    initialized.grant_id,
                    owner,
                    Some(first_snapshot),
                )
                .unwrap(),
            "a later Git process may still present the previous on-disk cursor"
        );
        assert_eq!(
            pins(&store),
            vec![(first_snapshot, "committed".to_owned())],
            "the unpublished replacement must be discarded while restoring the index cursor"
        );
    }

    #[test]
    fn bounded_logical_replay_survives_physical_snapshot_collection() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let manifest = ChangedObjectsManifest {
            objects: std::collections::BTreeMap::new(),
            ref_adds: BTreeSet::new(),
            ref_deletes: BTreeSet::new(),
            raw_ref_adds: 0,
            raw_ref_deletes: 0,
        };
        let mut snapshots = Vec::new();
        for sequence in 1_u8..=4 {
            let now_ns = i64::from(sequence) * 1_000;
            let cut_request = CutRequest {
                watch_id: initialized.watch_id,
                authorization_id: initialized.grant_id,
                reserved_snapshot_path: format!("/store/snapshots/w/logical-{sequence}")
                    .into_bytes(),
                requester_uid: 1000,
                requester_gid: 1000,
                lease_owner: [sequence + 10; 16],
                now_ns,
                lease_expires_ns: now_ns + 900,
            };
            let cut = store.reserve_cut(&cut_request).unwrap();
            store
                .start_cut_filesystem_effect(&cut, cut_request.lease_owner, now_ns + 1)
                .unwrap();
            let identity = SnapshotIdentity {
                subvol_uuid: [sequence + 20; 16],
                root_id: 900 + u64::from(sequence),
                created_ns: i64::from(sequence) * 100,
                ..cut_snapshot(&cut_request, request.source_subvol_uuid)
            };
            let recorded = store
                .record_cut_snapshot(&cut, cut_request.lease_owner, &identity, now_ns + 2)
                .unwrap();
            store
                .stage_validated_physical_cut(&cut, cut_request.lease_owner, &recorded, now_ns + 3)
                .unwrap();
            store
                .publish_adjacent_broker_delta(
                    &cut,
                    cut_request.lease_owner,
                    &recorded,
                    &manifest,
                    &[],
                    None,
                    now_ns + 4,
                )
                .unwrap();
            store
                .connection_mut()
                .execute(
                    r#"INSERT INTO fsmonitor_boundaries(
                           watch_id, cut_sequence, target_snapshot_id,
                           boundary_kind, cut_operation_id, clock_epoch,
                           guard_epoch, guard_sequence, guard_complete
                       ) VALUES (?1, ?2, ?3, 'cut', ?4,
                                 (SELECT clock_epoch FROM watches WHERE id = ?1),
                                 NULL, NULL, 0)"#,
                    params![
                        initialized.watch_id.as_slice(),
                        i64::from(sequence),
                        recorded.snapshot_id,
                        cut.operation_id.as_slice(),
                    ],
                )
                .unwrap();
            snapshots.push((recorded.snapshot_id, identity.subvol_uuid));
        }

        assert_eq!(
            store
                .retain_shared_replay_history(initialized.watch_id, [42; 16], 450, 2, 1_000, 16)
                .unwrap(),
            2,
            "the configured cut window bounds unpinned logical Git history"
        );
        let boundaries = {
            let mut statement = store
                .connection()
                .prepare(
                    "SELECT cut_sequence FROM fsmonitor_boundaries \
                     WHERE watch_id = ?1 ORDER BY cut_sequence",
                )
                .unwrap();
            statement
                .query_map([initialized.watch_id.as_slice()], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(boundaries, vec![3, 4]);

        store
            .connection_mut()
            .execute(
                "UPDATE snapshots SET physical_state = 'deleted', deleted_ns = 450 WHERE id = ?1",
                [snapshots[1].0],
            )
            .unwrap();
        let replay = store
            .replay_historical_changes(
                initialized.watch_id,
                initialized.grant_id,
                1000,
                snapshots[1].1,
                snapshots[3].1,
            )
            .unwrap();
        assert_eq!((replay.from_sequence, replay.to_sequence), (2, 4));
        assert!(!replay.fresh_instance);
        assert!(replay.events.is_empty());
        assert!(
            store
                .replay_historical_changes(
                    initialized.watch_id,
                    initialized.grant_id,
                    1000,
                    snapshots[0].1,
                    snapshots[3].1,
                )
                .is_err(),
            "cursors outside the bounded journal require Git's full-invalidation fallback"
        );
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

    fn record_test_cut(
        store: &mut Store,
        watch: &InitializedWatch,
        source_uuid: [u8; 16],
        serial: u8,
    ) -> (CutRequest, CutReservation, RecordedSnapshot) {
        let now_ns = i64::from(serial) * 1_000;
        let request = CutRequest {
            watch_id: watch.watch_id,
            authorization_id: watch.grant_id,
            reserved_snapshot_path: format!("/store/snapshots/gap-{serial}").into_bytes(),
            requester_uid: 1000,
            requester_gid: 1000,
            lease_owner: [serial + 10; 16],
            now_ns,
            lease_expires_ns: now_ns + 900,
        };
        let cut = store.reserve_cut(&request).unwrap();
        store
            .start_cut_filesystem_effect(&cut, request.lease_owner, now_ns + 1)
            .unwrap();
        let identity = SnapshotIdentity {
            subvol_uuid: [serial + 20; 16],
            root_id: 900 + u64::from(serial),
            created_ns: now_ns + 2,
            ..cut_snapshot(&request, source_uuid)
        };
        let recorded = store
            .record_cut_snapshot(&cut, request.lease_owner, &identity, now_ns + 2)
            .unwrap();
        (request, cut, recorded)
    }

    fn test_cut_manifest(events: &[Event]) -> ChangedObjectsManifest {
        ChangedObjectsManifest {
            objects: events
                .iter()
                .map(|event| {
                    (
                        event.ino,
                        crate::manifest::ObjectChange {
                            ino: event.ino,
                            old_generation: event.old_generation,
                            new_generation: event.new_generation,
                            change_mask: event.change_mask,
                        },
                    )
                })
                .collect(),
            ref_adds: BTreeSet::new(),
            ref_deletes: BTreeSet::new(),
            raw_ref_adds: 0,
            raw_ref_deletes: 0,
        }
    }

    fn test_cut_event(serial: u8) -> Event {
        let path = format!("changed-{serial}").into_bytes();
        Event {
            kind: EventKind::PathChanged,
            ino: 300 + u64::from(serial),
            old_generation: Some(10),
            new_generation: Some(10),
            change_mask: crate::manifest::CHANGE_INODE,
            old_path: Some(path.clone()),
            new_path: Some(path),
        }
    }

    fn publish_test_cut(
        store: &mut Store,
        request: &CutRequest,
        cut: &CutReservation,
        recorded: &RecordedSnapshot,
        events: &[Event],
        spool_directory: Option<&Path>,
    ) -> PublishedCut {
        store
            .stage_validated_physical_cut(cut, request.lease_owner, recorded, request.now_ns + 3)
            .unwrap();
        let spool = spool_directory.map(|directory| {
            let path = directory.join(format!("replay-{}.spool", cut.sequence));
            fs::write(&path, b"manifest").unwrap();
            let hash = append_replay_spool_payload(&path, events).unwrap();
            (path, hash)
        });
        let published = store
            .publish_adjacent_broker_delta(
                cut,
                request.lease_owner,
                recorded,
                &test_cut_manifest(events),
                events,
                spool
                    .as_ref()
                    .map(|(path, hash)| (path.as_os_str().as_bytes(), *hash)),
                request.now_ns + 4,
            )
            .unwrap();
        store
            .connection_mut()
            .execute(
                r#"INSERT INTO fsmonitor_boundaries(
                       watch_id, cut_sequence, target_snapshot_id,
                       boundary_kind, cut_operation_id, clock_epoch,
                       guard_epoch, guard_sequence, guard_complete
                   ) VALUES (?1, ?2, ?3, 'cut', ?4,
                             (SELECT clock_epoch FROM watches WHERE id = ?1),
                             NULL, NULL, 0)"#,
                params![
                    cut.watch_id.as_slice(),
                    cut.sequence,
                    recorded.snapshot_id,
                    cut.operation_id.as_slice(),
                ],
            )
            .unwrap();
        published
    }

    #[test]
    fn spooled_publication_replays_after_reopen_without_bulk_sqlite_writes() {
        let mut spooled_changes = Vec::new();
        for count in [1_usize, 257] {
            for spooled in [false, true] {
                let (temp, mut store, request) = setup();
                let (_, watch) = initialize_watch(&mut store, &request);
                let (cut_request, cut, recorded) =
                    record_test_cut(&mut store, &watch, request.source_subvol_uuid, 1);
                let events: Vec<_> = (0..count)
                    .map(|index| Event {
                        kind: EventKind::PathAdded,
                        ino: 300 + index as u64,
                        old_generation: None,
                        new_generation: Some(10),
                        change_mask: crate::manifest::CHANGE_INODE
                            | CHANGE_REF
                            | crate::manifest::CHANGE_CREATED,
                        old_path: None,
                        new_path: Some(format!("added-{index}").into_bytes()),
                    })
                    .collect();
                let mut manifest = test_cut_manifest(&events);
                manifest.ref_adds = events
                    .iter()
                    .map(|event| crate::manifest::Reference {
                        ino: event.ino,
                        parent_ino: 256,
                        name: event.new_path.clone().unwrap(),
                    })
                    .collect();
                manifest.raw_ref_adds = manifest.ref_adds.len();
                let spool_path = temp.path().join("replay.spool");
                fs::write(&spool_path, b"manifest").unwrap();
                let hash = append_replay_spool_payload(&spool_path, &events).unwrap();
                let changes_before = store.connection().total_changes();
                let published = store
                    .publish_adjacent_broker_delta(
                        &cut,
                        cut_request.lease_owner,
                        &recorded,
                        &manifest,
                        &events,
                        spooled.then_some((spool_path.as_os_str().as_bytes(), hash)),
                        cut_request.now_ns + 4,
                    )
                    .unwrap();
                if spooled {
                    spooled_changes.push(store.connection().total_changes() - changes_before);
                    let staging_tables: i64 = store
                        .connection()
                        .query_row(
                            "SELECT count(*) FROM sqlite_temp_master WHERE name LIKE 'delta_stage_%'",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap();
                    assert_eq!(staging_tables, 0);
                }
                for table in ["comparison_objects", "comparison_refs", "change_events"] {
                    let rows: i64 = store
                        .connection()
                        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })
                        .unwrap();
                    assert_eq!(rows, if spooled { 0 } else { count as i64 });
                }
                let metadata: (Vec<u8>, i64, i64, String) = store
                    .connection()
                    .query_row(
                        "SELECT manifest_hash, raw_ref_adds, raw_ref_deletes, state FROM comparisons WHERE id = ?1",
                        [published.comparison_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .unwrap();
                assert_eq!(
                    metadata,
                    (
                        manifest.canonical_hash().to_vec(),
                        count as i64,
                        0,
                        "index_ready".to_owned()
                    )
                );
                assert_eq!(published.events, events);
                // Retained logical replay must survive both process restart
                // and collection of its physical predecessor.
                store
                    .connection()
                    .execute(
                        "UPDATE snapshots SET physical_state = 'deleted', deleted_ns = 1500 WHERE id = ?1",
                        [watch.snapshot_id],
                    )
                    .unwrap();
                let path = store.path().to_owned();
                drop(store);
                let mut reopened = Store::open(&path).unwrap();
                let replay = reopened
                    .replay_historical_changes(
                        watch.watch_id,
                        watch.grant_id,
                        1000,
                        watch.snapshot_identity.subvol_uuid,
                        recorded.identity.subvol_uuid,
                    )
                    .unwrap();
                assert!(!replay.fresh_instance);
                assert_eq!(replay.events, events);
                assert!(reopened.foreign_key_violations().unwrap().is_empty());
            }
        }
        assert_eq!(spooled_changes[0], spooled_changes[1]);
    }

    #[test]
    fn spooled_publication_preserves_row_validation() {
        for spooled in [false, true] {
            for invalid in 0..3 {
                let (temp, mut store, request) = setup();
                let (_, watch) = initialize_watch(&mut store, &request);
                let (cut_request, cut, recorded) =
                    record_test_cut(&mut store, &watch, request.source_subvol_uuid, 1);
                let mut events = vec![test_cut_event(1)];
                let mut manifest = test_cut_manifest(&events);
                match invalid {
                    0 => manifest.objects.values_mut().next().unwrap().change_mask = u64::MAX,
                    1 => events[0].change_mask = u64::MAX,
                    2 => {
                        let change = *manifest.objects.values().next().unwrap();
                        manifest.objects.insert(change.ino + 1, change);
                    }
                    _ => unreachable!(),
                }
                let spool_path = temp.path().join("replay.spool");
                fs::write(&spool_path, b"manifest").unwrap();
                let hash = append_replay_spool_payload(&spool_path, &events).unwrap();
                assert!(
                    store
                        .publish_adjacent_broker_delta(
                            &cut,
                            cut_request.lease_owner,
                            &recorded,
                            &manifest,
                            &events,
                            spooled.then_some((spool_path.as_os_str().as_bytes(), hash)),
                            cut_request.now_ns + 4,
                        )
                        .is_err()
                );
                let state: (i64, i64, String) = store
                    .connection()
                    .query_row(
                        "SELECT w.indexed_seq, (SELECT count(*) FROM comparisons), o.state
                           FROM watches w JOIN operations o ON o.watch_id = w.id AND o.id = ?1",
                        [cut.operation_id.as_slice()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .unwrap();
                assert_eq!(state, (0, 0, "uuid_recorded".to_owned()));
            }
        }
    }

    fn git_observation_fixture(
        cuts: u8,
    ) -> (
        tempfile::TempDir,
        Store,
        InitializedWatch,
        FacadeActivation,
        Vec<RecordedSnapshot>,
    ) {
        let (temp, mut store, request) = setup();
        let (_, watch) = initialize_watch(&mut store, &request);
        let activation = store
            .activate_snapshot_facade(
                watch.watch_id,
                watch.grant_id,
                &ViewBinding {
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
                },
            )
            .unwrap();
        let snapshots = (1..=cuts)
            .map(|serial| {
                let (request, cut, recorded) =
                    record_test_cut(&mut store, &watch, request.source_subvol_uuid, serial);
                publish_test_cut(
                    &mut store,
                    &request,
                    &cut,
                    &recorded,
                    &[test_cut_event(serial)],
                    Some(temp.path()),
                );
                recorded
            })
            .collect();
        (temp, store, watch, activation, snapshots)
    }

    fn test_git_observation(
        watch: &InitializedWatch,
        activation: &FacadeActivation,
        snapshots: &[RecordedSnapshot],
        sequence: i64,
        now_ns: i64,
    ) -> GitCursorObservation {
        GitCursorObservation {
            watch_id: watch.watch_id,
            authorization_id: watch.grant_id,
            filesystem_uuid: watch.snapshot_identity.fs_uuid,
            snapshot_uuid: snapshots[usize::try_from(sequence - 1).unwrap()]
                .identity
                .subvol_uuid,
            clock_epoch: activation.clock_epoch,
            sequence,
            now_ns,
            grace_ns: 10,
        }
    }

    fn git_observation_rows(store: &Store, watch_id: [u8; 16]) -> Vec<(i64, i64, Option<i64>)> {
        store
            .connection()
            .prepare(
                "SELECT cut_sequence, last_queried_ns, retain_until_ns FROM git_cursor_observations \
                  WHERE watch_id = ?1 ORDER BY cut_sequence",
            )
            .unwrap()
            .query_map([watch_id.as_slice()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn retained_test_boundaries(store: &Store, watch_id: [u8; 16]) -> Vec<i64> {
        store
            .connection()
            .prepare("SELECT cut_sequence FROM fsmonitor_boundaries WHERE watch_id = ?1 ORDER BY cut_sequence")
            .unwrap()
            .query_map([watch_id.as_slice()], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn observed_git_input_survives_more_than_128_later_issued_cuts() {
        let (temp, mut store, watch, activation, snapshots) = git_observation_fixture(1);
        let observation = test_git_observation(&watch, &activation, &snapshots, 1, 2_000);
        assert!(store.record_git_cursor_observation(&observation).unwrap());
        let mut target_uuid = snapshots[0].identity.subvol_uuid;
        for serial in 2..=130 {
            let (request, cut, recorded) = record_test_cut(
                &mut store,
                &watch,
                watch.snapshot_identity.parent_uuid.unwrap(),
                serial,
            );
            publish_test_cut(
                &mut store,
                &request,
                &cut,
                &recorded,
                &[test_cut_event(serial)],
                Some(temp.path()),
            );
            target_uuid = recorded.identity.subvol_uuid;
        }
        assert_eq!(
            git_observation_rows(&store, watch.watch_id),
            [(1, 2_000, None)]
        );
        assert_eq!(
            store
                .retain_shared_replay_history(
                    watch.watch_id,
                    [42; 16],
                    100_000_000_000_000,
                    128,
                    86_400_000_000_000,
                    256,
                )
                .unwrap(),
            0
        );
        assert_eq!(retained_test_boundaries(&store, watch.watch_id).len(), 130);
        let replay = store
            .replay_historical_changes(
                watch.watch_id,
                watch.grant_id,
                1000,
                snapshots[0].identity.subvol_uuid,
                target_uuid,
            )
            .unwrap();
        assert!(!replay.fresh_instance);
        assert_eq!(replay.events.len(), 129);
        assert_eq!(replay.events.first(), Some(&test_cut_event(2)));
        assert_eq!(replay.events.last(), Some(&test_cut_event(130)));
        assert!(store.foreign_key_violations().unwrap().is_empty());
    }

    #[test]
    fn bounded_observation_expiry_keeps_remaining_old_inputs_protected() {
        let (_temp, mut store, watch, activation, snapshots) = git_observation_fixture(6);
        for sequence in 1..=6 {
            assert!(
                store
                    .record_git_cursor_observation(&test_git_observation(
                        &watch,
                        &activation,
                        &snapshots,
                        sequence,
                        10_000 + sequence
                    ),)
                    .unwrap()
            );
        }
        // Renew the oldest input so deadline order differs from sequence order.
        assert!(
            store
                .record_git_cursor_observation(&test_git_observation(
                    &watch,
                    &activation,
                    &snapshots,
                    1,
                    10_009
                ),)
                .unwrap()
        );
        for tick in 0..9 {
            let removed = store
                .retain_shared_replay_history(watch.watch_id, [42; 16], 20_000 + tick, 1, 1, 1)
                .unwrap();
            assert_eq!(removed, usize::from(tick >= 4));
            if tick < 4 {
                assert_eq!(
                    retained_test_boundaries(&store, watch.watch_id),
                    [1, 2, 3, 4, 5, 6]
                );
            }
            assert!(store.foreign_key_violations().unwrap().is_empty());
        }
        assert_eq!(
            git_observation_rows(&store, watch.watch_id),
            [(6, 10_006, None)]
        );
        assert_eq!(retained_test_boundaries(&store, watch.watch_id), [6]);
    }

    #[test]
    fn queried_git_cursor_survives_issued_cuts_and_older_inputs_get_last_use_grace() {
        let (temp, mut store, watch, activation, snapshots) = git_observation_fixture(6);
        assert!(git_observation_rows(&store, watch.watch_id).is_empty());
        // Logical replay remains valid after its physical source is collected.
        store
            .connection_mut()
            .execute(
                "UPDATE snapshots SET physical_state = 'deleted', deleted_ns = 10000 WHERE id = ?1",
                [snapshots[2].snapshot_id],
            )
            .unwrap();
        let mut old = test_git_observation(&watch, &activation, &snapshots, 3, 10_000);
        assert!(store.record_git_cursor_observation(&old).unwrap());
        drop(store);
        let mut store = Store::open(&temp.path().join("state.sqlite3")).unwrap();
        store
            .retain_shared_replay_history(watch.watch_id, [42; 16], 100_000, 1, 1, 16)
            .unwrap();
        assert_eq!(
            retained_test_boundaries(&store, watch.watch_id),
            [3, 4, 5, 6]
        );
        assert_eq!(
            git_observation_rows(&store, watch.watch_id),
            [(3, 10_000, None)]
        );
        let replay = store
            .replay_historical_changes(
                watch.watch_id,
                watch.grant_id,
                1000,
                snapshots[2].identity.subvol_uuid,
                snapshots[5].identity.subvol_uuid,
            )
            .unwrap();
        assert!(!replay.fresh_instance);
        assert_eq!(
            replay.events,
            [test_cut_event(4), test_cut_event(5), test_cut_event(6)]
        );

        let newer = test_git_observation(&watch, &activation, &snapshots, 5, 100_001);
        assert!(store.record_git_cursor_observation(&newer).unwrap());
        let newest = test_git_observation(&watch, &activation, &snapshots, 6, 100_005);
        assert!(store.record_git_cursor_observation(&newest).unwrap());
        assert_eq!(
            git_observation_rows(&store, watch.watch_id),
            [
                (3, 10_000, Some(100_011)),
                (5, 100_001, Some(100_015)),
                (6, 100_005, None),
            ]
        );
        let older_unseen = test_git_observation(&watch, &activation, &snapshots, 4, 100_006);
        assert!(store.record_git_cursor_observation(&older_unseen).unwrap());
        old.now_ns = 100_008;
        assert!(store.record_git_cursor_observation(&old).unwrap());
        assert_eq!(
            git_observation_rows(&store, watch.watch_id)[0],
            (3, 100_008, Some(100_018))
        );
        store
            .retain_shared_replay_history(watch.watch_id, [42; 16], 100_016, 1, 1, 16)
            .unwrap();
        assert_eq!(
            retained_test_boundaries(&store, watch.watch_id),
            [3, 4, 5, 6]
        );
        assert_eq!(git_observation_rows(&store, watch.watch_id).len(), 2);

        // Grace expiration is GC eligibility. A late query can still refresh
        // an exact cursor whose boundary and complete replay remain retained.
        store
            .retain_shared_replay_history(watch.watch_id, [42; 16], 100_019, 10, 200_000, 16)
            .unwrap();
        assert_eq!(
            git_observation_rows(&store, watch.watch_id),
            [(6, 100_005, None)]
        );
        old.now_ns = 100_020;
        assert!(store.record_git_cursor_observation(&old).unwrap());
        assert_eq!(
            git_observation_rows(&store, watch.watch_id)[0],
            (3, 100_020, Some(100_030))
        );
        store
            .retain_shared_replay_history(watch.watch_id, [42; 16], 100_031, 1, 1, 16)
            .unwrap();
        assert_eq!(retained_test_boundaries(&store, watch.watch_id), [6]);
        old.now_ns = 100_032;
        assert!(!store.record_git_cursor_observation(&old).unwrap());
        assert_eq!(
            git_observation_rows(&store, watch.watch_id),
            [(6, 100_005, None)]
        );
        assert!(!temp.path().join("replay-3.spool").exists());
        assert!(temp.path().join("replay-6.spool").exists());
        assert!(store.foreign_key_violations().unwrap().is_empty());
    }

    #[test]
    fn git_cursor_observation_rejects_foreign_identity_expired_floor_and_missing_replay() {
        let (_temp, mut store, watch, activation, snapshots) = git_observation_fixture(6);
        let head = test_git_observation(&watch, &activation, &snapshots, 4, 10_000);
        assert!(store.record_git_cursor_observation(&head).unwrap());
        let old = test_git_observation(&watch, &activation, &snapshots, 2, 10_001);
        for mutation in 0..6 {
            let mut invalid = old.clone();
            match mutation {
                0 => invalid.watch_id = [90; 16],
                1 => invalid.authorization_id = [91; 16],
                2 => invalid.filesystem_uuid = [92; 16],
                3 => invalid.snapshot_uuid = [93; 16],
                4 => invalid.clock_epoch = [94; 16],
                5 => invalid.sequence = 7,
                _ => unreachable!(),
            }
            assert!(store.record_git_cursor_observation(&invalid).is_err());
        }
        store
            .connection_mut()
            .execute(
                "DELETE FROM replay_spools WHERE watch_id = ?1 AND sequence = 5",
                [watch.watch_id.as_slice()],
            )
            .unwrap();
        assert!(!store.record_git_cursor_observation(&old).unwrap());
        store
            .connection_mut()
            .execute(
                "DELETE FROM fsmonitor_boundaries WHERE watch_id = ?1 AND cut_sequence = 2",
                [watch.watch_id.as_slice()],
            )
            .unwrap();
        assert!(!store.record_git_cursor_observation(&old).unwrap());
        store
            .connection_mut()
            .execute(
                "UPDATE watches SET replay_floor_seq = 3 WHERE id = ?1",
                [watch.watch_id.as_slice()],
            )
            .unwrap();
        let below_floor = test_git_observation(&watch, &activation, &snapshots, 1, 10_002);
        assert!(!store.record_git_cursor_observation(&below_floor).unwrap());
        assert_eq!(
            git_observation_rows(&store, watch.watch_id),
            [(4, 10_000, None)]
        );
    }

    #[test]
    fn replay_cleanup_and_manual_floor_preserve_active_intervals_and_durable_inputs() {
        let (_temp, mut store, watch, activation, snapshots) = git_observation_fixture(6);
        let lease = store
            .begin_query_lease(&activation, Some(2), 5, [80; 16], 40_000)
            .unwrap();
        store
            .retain_shared_replay_history(watch.watch_id, [42; 16], 20_000, 1, 1, 16)
            .unwrap();
        assert_eq!(
            retained_test_boundaries(&store, watch.watch_id),
            [2, 3, 4, 5, 6]
        );
        assert!(
            store
                .advance_replay_floor(watch.watch_id, 3, 20_001, [42; 16])
                .is_err()
        );
        store.release_query_lease(&lease, &activation).unwrap();

        let owner = [81; 16];
        store
            .stage_consumer_baseline(
                watch.watch_id,
                watch.grant_id,
                owner,
                snapshots[1].snapshot_id,
            )
            .unwrap();
        store.finish_consumer_baseline(owner, true).unwrap();
        store
            .retain_shared_replay_history(watch.watch_id, [42; 16], 20_002, 1, 1, 16)
            .unwrap();
        assert_eq!(
            retained_test_boundaries(&store, watch.watch_id),
            [2, 3, 4, 5, 6]
        );
        assert!(
            store
                .advance_replay_floor(watch.watch_id, 3, 20_003, [42; 16])
                .is_err()
        );
        store.release_consumer_baseline(owner).unwrap();

        let observed = test_git_observation(&watch, &activation, &snapshots, 4, 20_004);
        assert!(store.record_git_cursor_observation(&observed).unwrap());
        store
            .retain_shared_replay_history(watch.watch_id, [42; 16], 30_000, 1, 1, 16)
            .unwrap();
        assert_eq!(retained_test_boundaries(&store, watch.watch_id), [4, 5, 6]);
        assert!(
            store
                .advance_replay_floor(watch.watch_id, 5, 30_001, [42; 16])
                .is_err()
        );
        store
            .advance_replay_floor(watch.watch_id, 4, 30_001, [42; 16])
            .unwrap();
        assert!(store.foreign_key_violations().unwrap().is_empty());
    }

    #[test]
    fn cursor_observation_and_gc_serialize_without_pruning_an_accepted_input() {
        for _ in 0..4 {
            let (_temp, mut store, watch, activation, snapshots) = git_observation_fixture(4);
            let mut gc = Store::open(store.path()).unwrap();
            let observation = test_git_observation(&watch, &activation, &snapshots, 2, 10_000);
            let watch_id = watch.watch_id;
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let observe_barrier = barrier.clone();
            let observe = std::thread::spawn(move || {
                observe_barrier.wait();
                store.record_git_cursor_observation(&observation).unwrap()
            });
            let cleanup = std::thread::spawn(move || {
                barrier.wait();
                gc.retain_shared_replay_history(watch_id, [42; 16], 10_000, 1, 1, 16)
                    .unwrap();
                gc
            });
            let accepted = observe.join().unwrap();
            let gc = cleanup.join().unwrap();
            if accepted {
                assert_eq!(retained_test_boundaries(&gc, watch_id), [2, 3, 4]);
                assert_eq!(git_observation_rows(&gc, watch_id), [(2, 10_000, None)]);
                assert!(has_complete_retained_replay(gc.connection(), watch_id, 2, 4).unwrap());
            } else {
                assert_eq!(retained_test_boundaries(&gc, watch_id), [4]);
                assert!(git_observation_rows(&gc, watch_id).is_empty());
            }
            assert!(gc.foreign_key_violations().unwrap().is_empty());
        }
    }

    #[test]
    fn v13_migration_keeps_existing_floor_without_inventing_observed_inputs() {
        let (temp, mut store, watch, activation, snapshots) = git_observation_fixture(4);
        store
            .retain_shared_replay_history(watch.watch_id, [42; 16], 10_000, 1, 1, 16)
            .unwrap();
        store
            .connection_mut()
            .execute_batch(
                "DROP TABLE git_cursor_observations;
             DELETE FROM schema_migrations WHERE version = 14;
             INSERT INTO schema_migrations(version, name, applied_ns)
                 VALUES (13, 'published-cut-predecessors-v13', 1);
             PRAGMA user_version = 13;",
            )
            .unwrap();
        drop(store);
        let mut store = Store::open(&temp.path().join("state.sqlite3")).unwrap();
        assert!(git_observation_rows(&store, watch.watch_id).is_empty());
        let floor: i64 = store
            .connection()
            .query_row(
                "SELECT replay_floor_seq FROM watches WHERE id = ?1",
                [watch.watch_id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(floor, 3);
        assert!(
            !store
                .record_git_cursor_observation(&test_git_observation(
                    &watch,
                    &activation,
                    &snapshots,
                    2,
                    10_001
                ),)
                .unwrap()
        );
        assert!(
            store
                .record_git_cursor_observation(&test_git_observation(
                    &watch,
                    &activation,
                    &snapshots,
                    4,
                    10_001
                ),)
                .unwrap()
        );
        assert_eq!(
            git_observation_rows(&store, watch.watch_id),
            [(4, 10_001, None)]
        );
    }

    #[test]
    fn failed_cut_gaps_preserve_exact_replay_and_retention() {
        for use_spools in [false, true] {
            let (temp, mut store, request) = setup();
            let (_, watch) = initialize_watch(&mut store, &request);
            let owner = [88; 16];
            store
                .stage_consumer_baseline(watch.watch_id, watch.grant_id, owner, watch.snapshot_id)
                .unwrap();
            store.finish_consumer_baseline(owner, true).unwrap();
            let mut successful = Vec::new();
            let mut events = Vec::new();
            for serial in 1..=7 {
                let (cut_request, cut, recorded) =
                    record_test_cut(&mut store, &watch, request.source_subvol_uuid, serial);
                assert_eq!(cut.sequence, i64::from(serial));
                if matches!(serial, 1 | 2 | 4 | 5) {
                    store
                        .fail_unpublished_cut(
                            &cut,
                            cut_request.lease_owner,
                            &recorded,
                            "invalid immutable target",
                            cut_request.now_ns + 3,
                        )
                        .unwrap();
                    assert!(
                        store
                            .replay_historical_changes(
                                watch.watch_id,
                                watch.grant_id,
                                1000,
                                recorded.identity.subvol_uuid,
                                recorded.identity.subvol_uuid,
                            )
                            .is_err(),
                        "a failed snapshot must never become a published cursor"
                    );
                    let replay = store
                        .replay_historical_changes(
                            watch.watch_id,
                            watch.grant_id,
                            1000,
                            watch.snapshot_identity.subvol_uuid,
                            watch.snapshot_identity.subvol_uuid,
                        )
                        .unwrap();
                    assert_eq!((replay.from_sequence, replay.to_sequence), (0, 0));
                    assert!(!replay.fresh_instance);
                    continue;
                }
                let event = test_cut_event(serial);
                publish_test_cut(
                    &mut store,
                    &cut_request,
                    &cut,
                    &recorded,
                    std::slice::from_ref(&event),
                    use_spools.then_some(temp.path()),
                );
                events.push(event);
                successful.push(recorded);
            }
            let target = successful.last().unwrap().identity.subvol_uuid;
            let replay = store
                .replay_historical_changes(
                    watch.watch_id,
                    watch.grant_id,
                    1000,
                    watch.snapshot_identity.subvol_uuid,
                    target,
                )
                .unwrap();
            assert_eq!((replay.from_sequence, replay.to_sequence), (0, 7));
            assert!(!replay.fresh_instance);
            assert_eq!(replay.events, events);

            // A descendant's inherited cursor aliases its own sequence zero.
            let inherited = [99_u8; 16];
            store
                .connection_mut()
                .execute(
                    "UPDATE watches SET inherited_baseline_snapshot_uuid = ?2 WHERE id = ?1",
                    params![watch.watch_id.as_slice(), inherited.as_slice()],
                )
                .unwrap();
            let replay = store
                .replay_historical_changes(watch.watch_id, watch.grant_id, 1000, inherited, target)
                .unwrap();
            assert!(!replay.fresh_instance);
            assert_eq!(replay.events, events);

            assert_eq!(
                store
                    .retain_shared_replay_history(watch.watch_id, [42; 16], 8_000, 2, 10_000, 16)
                    .unwrap(),
                0,
                "the initial durable consumer still needs every successful transition"
            );
            store.release_consumer_baseline(owner).unwrap();
            assert_eq!(
                store
                    .retain_shared_replay_history(watch.watch_id, [42; 16], 8_000, 2, 10_000, 16)
                    .unwrap(),
                1
            );
            let floor: i64 = store
                .connection()
                .query_row(
                    "SELECT replay_floor_seq FROM watches WHERE id = ?1",
                    [watch.watch_id.as_slice()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(floor, 3, "the first retained edge is 3 -> 6, not 5 -> 6");
            store
                .connection_mut()
                .execute(
                    "UPDATE snapshots SET physical_state = 'deleted', deleted_ns = 8000 WHERE id = ?1",
                    [successful[0].snapshot_id],
                )
                .unwrap();
            let replay = store
                .replay_historical_changes(
                    watch.watch_id,
                    watch.grant_id,
                    1000,
                    successful[0].identity.subvol_uuid,
                    target,
                )
                .unwrap();
            assert_eq!((replay.from_sequence, replay.to_sequence), (3, 7));
            assert!(!replay.fresh_instance);
            assert_eq!(replay.events, events[1..]);
            if use_spools {
                let comparison = store
                    .claim_historical_comparison(&HistoricalComparisonRequest {
                        watch_id: watch.watch_id,
                        authorization_id: watch.grant_id,
                        requester_uid: 1000,
                        from_snapshot_uuid: successful[0].identity.subvol_uuid,
                        to_snapshot_uuid: target,
                        lease_owner: [43; 16],
                        now_ns: 8_001,
                        lease_expires_ns: 9_000,
                    })
                    .unwrap();
                let HistoricalComparisonAdmission::Ready(comparison) = comparison else {
                    panic!("retained spools must not require a deleted physical source");
                };
                assert_eq!(comparison.events, events[1..]);
                assert!(!temp.path().join("replay-3.spool").exists());
                assert!(temp.path().join("replay-6.spool").exists());
            }
            assert!(
                store
                    .replay_historical_changes(
                        watch.watch_id,
                        watch.grant_id,
                        1000,
                        inherited,
                        target
                    )
                    .unwrap()
                    .fresh_instance,
                "a cursor below the retained floor still requires invalidation"
            );
            assert!(store.foreign_key_violations().unwrap().is_empty());
        }
    }

    #[test]
    fn migrates_and_recovers_a_staged_successor_after_a_failed_cut() {
        let (temp, mut store, request) = setup();
        let (_, watch) = initialize_watch(&mut store, &request);
        let (first_request, first, first_snapshot) =
            record_test_cut(&mut store, &watch, request.source_subvol_uuid, 1);
        let first_event = test_cut_event(1);
        publish_test_cut(
            &mut store,
            &first_request,
            &first,
            &first_snapshot,
            std::slice::from_ref(&first_event),
            None,
        );
        let (failed_request, failed, rejected) =
            record_test_cut(&mut store, &watch, request.source_subvol_uuid, 2);
        store
            .fail_unpublished_cut(
                &failed,
                failed_request.lease_owner,
                &rejected,
                "invalid immutable target",
                2_003,
            )
            .unwrap();
        let (pending_request, pending, successor) =
            record_test_cut(&mut store, &watch, request.source_subvol_uuid, 3);
        store
            .stage_validated_physical_cut(&pending, pending_request.lease_owner, &successor, 3_003)
            .unwrap();
        // Recreate the exact pre-fix schema with ready, failed and staged rows.
        store
            .connection_mut()
            .execute_batch(
                "DROP TABLE git_cursor_observations;
                 ALTER TABLE watch_cuts DROP COLUMN base_sequence;
                 DELETE FROM schema_migrations WHERE version >= 13;
                 INSERT INTO schema_migrations(version, name, applied_ns)
                     VALUES (12, 'unkeyed-cursor-integrity-v12', 1);
                 PRAGMA user_version = 12;",
            )
            .unwrap();
        let metadata = store.metadata().unwrap();
        drop(store);
        let mut store = Store::open(&temp.path().join("state.sqlite3")).unwrap();
        assert_eq!(store.metadata().unwrap(), metadata);
        for (sequence, expected) in [(1, Some(0_i64)), (2, None), (3, None)] {
            let predecessor: Option<i64> = store
                .connection()
                .query_row(
                    "SELECT base_sequence FROM watch_cuts WHERE watch_id = ?1 AND sequence = ?2",
                    params![watch.watch_id.as_slice(), sequence],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(predecessor, expected);
        }
        let new_owner = [44; 16];
        assert_eq!(store.takeover_recovery_leases(new_owner, 5_000).unwrap(), 1);
        let successor_event = test_cut_event(3);
        let manifest = test_cut_manifest(std::slice::from_ref(&successor_event));
        assert!(
            store
                .publish_adjacent_broker_delta(
                    &pending,
                    pending_request.lease_owner,
                    &successor,
                    &manifest,
                    std::slice::from_ref(&successor_event),
                    None,
                    4_000,
                )
                .is_err(),
            "the old owner must remain fenced out after recovery takeover"
        );
        let published = store
            .publish_adjacent_broker_delta(
                &pending,
                new_owner,
                &successor,
                &manifest,
                std::slice::from_ref(&successor_event),
                None,
                4_000,
            )
            .unwrap();
        assert_eq!(published.sequence, 3);
        let predecessor: i64 = store
            .connection()
            .query_row(
                "SELECT base_sequence FROM watch_cuts WHERE watch_id = ?1 AND sequence = 3",
                [watch.watch_id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(predecessor, 1);
        // An already-open old publisher may still leave an adjacent ready row
        // without the new column populated after the schema upgrade.
        store
            .connection_mut()
            .execute(
                "UPDATE watch_cuts SET base_sequence = NULL WHERE watch_id = ?1 AND sequence = 1",
                [watch.watch_id.as_slice()],
            )
            .unwrap();
        let replay = store
            .replay_historical_changes(
                watch.watch_id,
                watch.grant_id,
                1000,
                watch.snapshot_identity.subvol_uuid,
                successor.identity.subvol_uuid,
            )
            .unwrap();
        assert_eq!((replay.from_sequence, replay.to_sequence), (0, 3));
        assert!(!replay.fresh_instance);
        assert_eq!(replay.events, [first_event, successor_event]);
        assert!(store.foreign_key_violations().unwrap().is_empty());
    }

    #[test]
    fn gap_publication_keeps_predecessor_owner_and_watch_fences() {
        let (_temp, mut store, request) = setup();
        let (_, watch) = initialize_watch(&mut store, &request);
        let (failed_request, failed, rejected) =
            record_test_cut(&mut store, &watch, request.source_subvol_uuid, 1);
        store
            .fail_unpublished_cut(
                &failed,
                failed_request.lease_owner,
                &rejected,
                "invalid immutable target",
                1_003,
            )
            .unwrap();
        let (pending_request, pending, successor) =
            record_test_cut(&mut store, &watch, request.source_subvol_uuid, 2);
        store
            .stage_validated_physical_cut(&pending, pending_request.lease_owner, &successor, 2_003)
            .unwrap();
        for mutation in 0..5 {
            let mut stale = pending.clone();
            let mut owner = pending_request.lease_owner;
            match mutation {
                0 => owner = [90; 16],
                1 => stale.cut_fence += 1,
                2 => stale.operation_fence += 1,
                3 => stale.base_snapshot_id = rejected.snapshot_id,
                4 => stale.watch_id = [91; 16],
                _ => unreachable!(),
            }
            assert!(
                store
                    .publish_adjacent_broker_delta(
                        &stale,
                        owner,
                        &successor,
                        &test_cut_manifest(&[]),
                        &[],
                        None,
                        2_004,
                    )
                    .is_err()
            );
            let state: (i64, i64, i64, Option<i64>) = store
                .connection()
                .query_row(
                    "SELECT w.indexed_seq, w.last_cut_snapshot_id, \
                            (SELECT count(*) FROM comparisons), c.base_sequence \
                       FROM watches w JOIN watch_cuts c ON c.watch_id = w.id AND c.sequence = 2 \
                      WHERE w.id = ?1",
                    [watch.watch_id.as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(state, (0, watch.snapshot_id, 0, None));
        }
        store
            .publish_adjacent_broker_delta(
                &pending,
                pending_request.lease_owner,
                &successor,
                &test_cut_manifest(&[]),
                &[],
                None,
                2_004,
            )
            .unwrap();
        let (next_request, next, next_snapshot) =
            record_test_cut(&mut store, &watch, request.source_subvol_uuid, 3);
        store
            .stage_validated_physical_cut(&next, next_request.lease_owner, &next_snapshot, 3_003)
            .unwrap();
        let mut stale = next.clone();
        stale.base_snapshot_id = watch.snapshot_id;
        assert!(
            store
                .publish_adjacent_broker_delta(
                    &stale,
                    next_request.lease_owner,
                    &next_snapshot,
                    &test_cut_manifest(&[]),
                    &[],
                    None,
                    3_004,
                )
                .unwrap_err()
                .to_string()
                .contains("cut base snapshot differs from indexed predecessor")
        );
        assert!(
            store
                .replay_historical_changes(
                    watch.watch_id,
                    watch.grant_id,
                    9999,
                    watch.snapshot_identity.subvol_uuid,
                    successor.identity.subvol_uuid,
                )
                .unwrap_err()
                .to_string()
                .contains("authorization")
        );
        let sibling_request = InitializeRequest {
            fs_uuid: [31; 16],
            source_subvol_uuid: [32; 16],
            source_path: b"/sibling-source".to_vec(),
            reserved_snapshot_path: b"/store/snapshots/sibling-initial".to_vec(),
            ..request
        };
        let (_, sibling) = initialize_watch(&mut store, &sibling_request);
        for (watch_id, grant_id) in [
            (watch.watch_id, sibling.grant_id),
            (sibling.watch_id, sibling.grant_id),
        ] {
            assert!(
                store
                    .replay_historical_changes(
                        watch_id,
                        grant_id,
                        1000,
                        watch.snapshot_identity.subvol_uuid,
                        successor.identity.subvol_uuid,
                    )
                    .is_err(),
                "another watch's grant must not authorize these events"
            );
        }
        assert!(store.foreign_key_violations().unwrap().is_empty());
    }

    #[test]
    fn new_cut_staging_and_publication_roll_back_together_and_can_recover() {
        let (temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let cut_request = CutRequest {
            watch_id: initialized.watch_id,
            authorization_id: initialized.grant_id,
            reserved_snapshot_path: b"/store/snapshots/w/combined-cut".to_vec(),
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
        let events = [test_cut_event(1)];
        let manifest = test_cut_manifest(&events);
        let spool_path = temp.path().join("replay.spool");
        fs::write(&spool_path, b"manifest").unwrap();
        let hash = append_replay_spool_payload(&spool_path, &events).unwrap();
        let spool = Some((spool_path.as_os_str().as_bytes(), hash));
        assert!(
            store
                .publish_adjacent_broker_delta(
                    &cut, [7; 16], &recorded, &manifest, &events, spool, 525,
                )
                .is_err(),
            "the compound path must preserve the operation owner fence"
        );
        store
            .connection()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_combined_head_update
             BEFORE UPDATE OF indexed_seq ON watches
             BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END;",
            )
            .unwrap();
        assert!(
            store
                .publish_adjacent_broker_delta(
                    &cut,
                    cut_request.lease_owner,
                    &recorded,
                    &manifest,
                    &events,
                    spool,
                    550,
                )
                .is_err()
        );
        // A separate connection must see neither staging nor partial heads,
        // and the original operation must still pin both immutable endpoints.
        let observer = Store::open(store.path()).unwrap();
        let failed: (i64, i64, i64, String, i64, i64) = observer.connection().query_row(
            "SELECT w.last_cut_snapshot_id, w.last_cut_seq, w.indexed_seq, o.state,
                    (SELECT count(*) FROM watch_cuts c WHERE c.operation_id = o.id),
                    (SELECT count(*) FROM snapshot_pins p WHERE p.owner_kind = 'operation' AND p.owner_id = o.id)
               FROM watches w JOIN operations o ON o.id = ?2 WHERE w.id = ?1",
            params![initialized.watch_id.as_slice(), cut.operation_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).unwrap();
        assert_eq!(
            failed,
            (
                initialized.snapshot_id,
                0,
                0,
                "uuid_recorded".to_owned(),
                0,
                2
            )
        );
        drop(observer);
        let path = store.path().to_owned();
        drop(store);
        let mut reopened = Store::open(&path).unwrap();
        let published = reopened
            .publish_adjacent_broker_delta(
                &cut,
                cut_request.lease_owner,
                &recorded,
                &manifest,
                &events,
                spool,
                600,
            )
            .unwrap();
        assert_eq!(published.events, events);
        let replay = reopened
            .replay_historical_changes(
                initialized.watch_id,
                initialized.grant_id,
                1000,
                initialized.snapshot_identity.subvol_uuid,
                recorded.identity.subvol_uuid,
            )
            .unwrap();
        assert!(!replay.fresh_instance);
        assert_eq!(replay.events, events);
        assert_eq!(published.sequence, 1);
        assert_eq!(published.snapshot_id, recorded.snapshot_id);
        let committed: (i64, i64, i64, String) = reopened
            .connection()
            .query_row(
                "SELECT w.last_cut_snapshot_id, w.last_cut_seq, w.indexed_seq, o.state
               FROM watches w JOIN operations o ON o.id = ?2 WHERE w.id = ?1",
                params![initialized.watch_id.as_slice(), cut.operation_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(committed, (recorded.snapshot_id, 1, 1, "done".to_owned()));
        assert!(reopened.foreign_key_violations().unwrap().is_empty());
    }

    #[test]
    fn staged_cut_keeps_published_heads_unchanged_until_indexed_commit() {
        let (_temp, mut store, request) = setup();
        let (_initialize, initialized) = initialize_watch(&mut store, &request);
        let cut_request = CutRequest {
            watch_id: initialized.watch_id,
            authorization_id: initialized.grant_id,
            reserved_snapshot_path: b"/store/snapshots/w/atomic-cut".to_vec(),
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
            .stage_validated_physical_cut(&cut, cut_request.lease_owner, &recorded, 550)
            .unwrap();

        let staged: (i64, i64, i64, String) = store
            .connection()
            .query_row(
                r#"SELECT w.last_cut_snapshot_id, w.last_cut_seq, w.indexed_seq, o.state
                     FROM watches w JOIN operations o ON o.watch_id = w.id
                    WHERE w.id = ?1 AND o.id = ?2"#,
                params![initialized.watch_id.as_slice(), cut.operation_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            staged,
            (initialized.snapshot_id, 0, 0, "manifest_ready".to_owned())
        );
        let second_request = CutRequest {
            reserved_snapshot_path: b"/store/snapshots/w/leapfrog-cut".to_vec(),
            lease_owner: [7; 16],
            now_ns: 600,
            ..cut_request.clone()
        };
        assert!(store.reserve_cut(&second_request).is_err());

        let manifest = ChangedObjectsManifest {
            objects: std::collections::BTreeMap::new(),
            ref_adds: BTreeSet::new(),
            ref_deletes: BTreeSet::new(),
            raw_ref_adds: 0,
            raw_ref_deletes: 0,
        };
        store
            .connection()
            .execute_batch(
                r#"CREATE TEMP TRIGGER fail_atomic_head_update
                   BEFORE UPDATE OF indexed_seq ON watches
                   BEGIN
                       SELECT RAISE(ABORT, 'database or disk is full');
                   END;"#,
            )
            .unwrap();
        assert!(
            store
                .publish_adjacent_broker_delta(
                    &cut,
                    cut_request.lease_owner,
                    &recorded,
                    &manifest,
                    &[],
                    None,
                    625,
                )
                .is_err()
        );
        let failed_commit: (i64, i64, i64, String, String) = store
            .connection()
            .query_row(
                r#"SELECT w.last_cut_snapshot_id, w.last_cut_seq, w.indexed_seq,
                           o.state, c.state
                     FROM watches w
                     JOIN operations o ON o.id = ?2
                     JOIN watch_cuts c ON c.operation_id = o.id
                    WHERE w.id = ?1"#,
                params![initialized.watch_id.as_slice(), cut.operation_id.as_slice()],
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
        assert_eq!(
            failed_commit,
            (
                initialized.snapshot_id,
                0,
                0,
                "manifest_ready".to_owned(),
                "created".to_owned()
            )
        );
        store
            .connection()
            .execute_batch("DROP TRIGGER fail_atomic_head_update;")
            .unwrap();
        let published = store
            .publish_adjacent_broker_delta(
                &cut,
                cut_request.lease_owner,
                &recorded,
                &manifest,
                &[],
                None,
                650,
            )
            .unwrap();
        assert_eq!(published.sequence, 1);
        assert_eq!(published.snapshot_id, recorded.snapshot_id);

        let committed: (i64, i64, i64, i64, String, String) = store
            .connection()
            .query_row(
                r#"SELECT w.last_cut_snapshot_id, w.last_cut_seq, w.indexed_seq,
                           r.snapshot_id, o.state, c.state
                     FROM watches w
                     JOIN revisions r ON r.id = w.indexed_revision_id
                     JOIN operations o ON o.id = ?2
                     JOIN watch_cuts c ON c.operation_id = o.id
                    WHERE w.id = ?1"#,
                params![initialized.watch_id.as_slice(), cut.operation_id.as_slice()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            committed,
            (
                recorded.snapshot_id,
                1,
                1,
                recorded.snapshot_id,
                "done".to_owned(),
                "ready".to_owned()
            )
        );
        assert!(
            store
                .connection()
                .execute(
                    "UPDATE watches SET last_cut_snapshot_id = ?2 WHERE id = ?1",
                    params![initialized.watch_id.as_slice(), initialized.snapshot_id],
                )
                .is_err()
        );
        assert!(store.foreign_key_violations().unwrap().is_empty());
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

    fn table_rows(store: &Store, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let mut statement = store
            .connection()
            .prepare(&format!("SELECT * FROM {table}"))
            .unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| {
                (0..columns).map(|column| row.get(column)).collect()
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn active_watch_relocation_preserves_history_and_rebases_every_namespace_column() {
        let (_temp, mut store, watch, _activation, snapshots) = git_observation_fixture(2);
        let moved_snapshot = b"/source/.git/awacs/managed/cut/snapshot";
        let lookalike_prefix = b"/source-other/snapshot";
        store
            .connection()
            .execute(
                "UPDATE snapshots SET path = ?2 WHERE id = ?1",
                params![snapshots[0].snapshot_id, moved_snapshot.as_slice()],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "UPDATE snapshots SET path = ?2 WHERE id = ?1",
                params![watch.snapshot_id, lookalike_prefix.as_slice()],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "UPDATE operations SET reserved_path = ?1 WHERE sequence = 1",
                [moved_snapshot.as_slice()],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "UPDATE replay_spools SET path = ?1 WHERE sequence = 1",
                [b"/source/.git/awacs/spool/cut.spool".as_slice()],
            )
            .unwrap();
        let history_tables = [
            "service_metadata",
            "revisions",
            "watch_cuts",
            "snapshot_pins",
            "git_cursor_observations",
            "fsmonitor_boundaries",
            "change_events",
        ];
        let history_before = history_tables.map(|table| table_rows(&store, table));
        let watch_before: (Vec<u8>, i64, i64) = store
            .connection()
            .query_row(
                "SELECT clock_epoch, indexed_seq, last_cut_seq FROM watches",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let mut verified = Vec::new();
        let rebound = store
            .rebind_active_uid_watch_path_with_snapshot_validator(
                [1; 16],
                [2; 16],
                b"/destination",
                1000,
                PERMISSION_READ | PERMISSION_CUT,
                |path, fs_uuid, uuid, state| {
                    verified.push((path.to_owned(), fs_uuid, uuid, state.to_owned()));
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(rebound, Some((watch.watch_id, watch.grant_id)));
        assert_eq!(
            verified,
            vec![(
                PathBuf::from("/destination/.git/awacs/managed/cut/snapshot"),
                [1; 16],
                snapshots[0].identity.subvol_uuid,
                "present".to_owned()
            )]
        );
        let binding: (Vec<u8>, Vec<u8>, Vec<u8>, i64, i64) = store
            .connection()
            .query_row(
                "SELECT live_path, fsmonitor_root, clock_epoch, indexed_seq, last_cut_seq FROM \
                 watches",
                [],
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
        assert_eq!(binding.0, b"/destination");
        assert_eq!(binding.1, b"/destination");
        assert_eq!((binding.2, binding.3, binding.4), watch_before);
        for (sql, expected) in [
            (
                "SELECT path FROM snapshots WHERE id = (SELECT target_snapshot_id FROM watch_cuts \
                 WHERE sequence = 1)",
                b"/destination/.git/awacs/managed/cut/snapshot".as_slice(),
            ),
            (
                "SELECT reserved_path FROM operations WHERE sequence = 1",
                b"/destination/.git/awacs/managed/cut/snapshot".as_slice(),
            ),
            (
                "SELECT path FROM replay_spools WHERE sequence = 1",
                b"/destination/.git/awacs/spool/cut.spool".as_slice(),
            ),
        ] {
            assert_eq!(
                store
                    .connection()
                    .query_row(sql, [], |row| row.get::<_, Vec<u8>>(0))
                    .unwrap(),
                expected
            );
        }
        let initial_path: Vec<u8> = store
            .connection()
            .query_row(
                "SELECT path FROM snapshots WHERE id = ?1",
                [watch.snapshot_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(initial_path, lookalike_prefix);
        let external_path: Vec<u8> = store
            .connection()
            .query_row(
                "SELECT path FROM snapshots WHERE id = ?1",
                [snapshots[1].snapshot_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(external_path, snapshots[1].identity.path);
        assert_eq!(
            history_tables.map(|table| table_rows(&store, table)),
            history_before
        );
        let relocated = table_rows(&store, "snapshots");
        assert_eq!(
            store
                .rebind_active_uid_watch_path_with_snapshot_validator(
                    [1; 16],
                    [2; 16],
                    b"/destination",
                    1000,
                    PERMISSION_READ | PERMISSION_CUT,
                    |_, _, _, _| panic!("an idempotent rebind must not revalidate unchanged paths"),
                )
                .unwrap(),
            rebound
        );
        assert_eq!(table_rows(&store, "snapshots"), relocated);
        assert!(store.foreign_key_violations().unwrap().is_empty());
    }

    #[test]
    fn active_watch_relocation_requires_matching_identity_and_authorization() {
        let (_temp, mut store, request) = setup();
        initialize_watch(&mut store, &request);
        let before = table_rows(&store, "watches");
        for (filesystem, subvolume, uid, permissions) in [
            ([99; 16], request.source_subvol_uuid, 1000, PERMISSION_READ),
            (request.fs_uuid, [99; 16], 1000, PERMISSION_READ),
            (
                request.fs_uuid,
                request.source_subvol_uuid,
                1001,
                PERMISSION_READ,
            ),
            (
                request.fs_uuid,
                request.source_subvol_uuid,
                1000,
                PERMISSION_ADMIN,
            ),
        ] {
            assert_eq!(
                store
                    .rebind_active_uid_watch_path_with_snapshot_validator(
                        filesystem,
                        subvolume,
                        b"/moved",
                        uid,
                        permissions,
                        |_, _, _, _| panic!("unauthorized relocation must not inspect snapshots"),
                    )
                    .unwrap(),
                None
            );
            assert_eq!(table_rows(&store, "watches"), before);
        }
    }

    #[test]
    fn active_watch_relocation_rolls_back_all_paths_on_snapshot_identity_failure() {
        let (_temp, mut store, _watch, _activation, snapshots) = git_observation_fixture(2);
        for snapshot in &snapshots {
            store
                .connection()
                .execute(
                    "UPDATE snapshots SET path = ?2 WHERE id = ?1",
                    params![
                        snapshot.snapshot_id,
                        format!("/source/snapshot-{}", snapshot.snapshot_id).as_bytes()
                    ],
                )
                .unwrap();
        }
        let tables = ["watches", "snapshots", "operations", "replay_spools"];
        let before = tables.map(|table| table_rows(&store, table));
        let mut calls = 0;
        let error = store
            .rebind_active_uid_watch_path_with_snapshot_validator(
                [1; 16],
                [2; 16],
                b"/moved",
                1000,
                PERMISSION_READ | PERMISSION_CUT,
                |_, _, _, _| {
                    calls += 1;
                    if calls == 2 {
                        Err(ManagerError::new("snapshot UUID differs"))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
        assert_eq!(calls, 2);
        assert!(error.to_string().contains("snapshot UUID differs"));
        assert_eq!(tables.map(|table| table_rows(&store, table)), before);
    }

    #[test]
    fn active_watch_relocation_rejects_shared_manager_without_changing_either_watch() {
        let (_temp, mut store, request) = setup();
        let (_, first) = initialize_watch(&mut store, &request);
        let second_request = InitializeRequest {
            source_subvol_uuid: [44; 16],
            source_path: b"/source/nested".to_vec(),
            reserved_snapshot_path: b"/source/.git/awacs/second/snapshot".to_vec(),
            principal: Principal::Uid(1001),
            requester_uid: 1001,
            ..request.clone()
        };
        let second = store.reserve_initialize(&second_request).unwrap();
        store
            .start_initialize_filesystem_effect(&second, second_request.lease_owner, 150)
            .unwrap();
        let second_snapshot = SnapshotIdentity {
            subvol_uuid: [45; 16],
            ..snapshot(&second_request)
        };
        let recorded = store
            .record_initialize_snapshot(&second, second_request.lease_owner, &second_snapshot, 200)
            .unwrap();
        store
            .publish_initial_snapshot(&second, second_request.lease_owner, &recorded, None, 300)
            .unwrap();
        let tables = [
            "watches",
            "watch_grants",
            "snapshots",
            "operations",
            "replay_spools",
        ];
        let before = tables.map(|table| table_rows(&store, table));
        let error = store
            .rebind_active_uid_watch_path_with_snapshot_validator(
                request.fs_uuid,
                request.source_subvol_uuid,
                b"/moved",
                request.requester_uid,
                PERMISSION_READ | PERMISSION_CUT,
                |_, _, _, _| {
                    panic!("shared-manager relocation must fail before inspecting snapshots")
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("multiple live watches"));
        assert_eq!(tables.map(|table| table_rows(&store, table)), before);
        assert_eq!(
            store
                .rebind_active_uid_watch_path_by_subvolume(
                    request.fs_uuid,
                    request.source_subvol_uuid,
                    &request.source_path,
                    request.requester_uid,
                    PERMISSION_READ | PERMISSION_CUT,
                )
                .unwrap(),
            Some((first.watch_id, first.grant_id))
        );
        assert_eq!(tables.map(|table| table_rows(&store, table)), before);
    }

    #[test]
    fn relocated_snapshot_validation_does_not_accept_missing_or_ordinary_present_trees() {
        let temporary = tempdir().unwrap();
        let missing = temporary.path().join("missing");
        assert!(verify_relocated_snapshot(&missing, [1; 16], [2; 16], "present").is_err());
        assert!(verify_relocated_snapshot(&missing, [1; 16], [2; 16], "deleted").is_ok());
        let ordinary = temporary.path().join("ordinary");
        fs::create_dir(&ordinary).unwrap();
        assert!(verify_relocated_snapshot(&ordinary, [1; 16], [2; 16], "present").is_err());
        assert!(verify_relocated_snapshot(&ordinary, [1; 16], [2; 16], "deleted").is_err());
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
