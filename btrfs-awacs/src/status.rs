//! Read-only inspection of AWACS roots and their Git/JJ baseline consumers.

use crate::bootstrap::{root_state_key, state_root_for_root};
use crate::btrfs::OpenedSubvolume;
use crate::compat::decode_direct_scan_cursor;
use crate::scan::{SnapshotBaseline, SnapshotIdentity};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::fmt;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug)]
pub struct StatusError {
    message: String,
}

impl StatusError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for StatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StatusError {}

impl From<rusqlite::Error> for StatusError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexChecksum {
    Valid,
    Omitted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitFsmonitorStatus {
    Missing,
    TimestampPlaceholder,
    Invalid(String),
    Baseline(SnapshotBaseline),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitIndexStatus {
    pub version: u32,
    pub entries: u32,
    pub checksum: IndexChecksum,
    pub fsmonitor: GitFsmonitorStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchStatus {
    pub id: [u8; 16],
    pub filesystem_uuid: [u8; 16],
    pub live_subvolume_uuid: [u8; 16],
    pub live_path: PathBuf,
    pub state: String,
    pub fsmonitor_state: String,
    pub indexed_sequence: Option<u64>,
    pub last_cut_sequence: Option<u64>,
    pub replay_floor_sequence: Option<u64>,
    clock_epoch: [u8; 16],
    owner_grant_id: Option<[u8; 16]>,
    monitor_session_id: Option<[u8; 16]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOperation {
    pub kind: String,
    pub state: String,
    pub sequence: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BaselineStatus {
    pub owner_id: Option<[u8; 16]>,
    pub snapshot_uuid: [u8; 16],
    pub sequence: Option<u64>,
    pub committed_pin: bool,
    pub pending_pin: bool,
    pub issues: Vec<String>,
}

pub struct RootStatus {
    pub database: PathBuf,
    pub watch: WatchStatus,
    pub pending_operations: Vec<PendingOperation>,
    pub pending_baselines: u64,
    pub issues: Vec<String>,
    connection: Connection,
    store_uuid: [u8; 16],
    clock_format_version: u32,
}

impl RootStatus {
    /// Opens the existing manager without creating paths, locks, or SQLite state.
    pub fn open(root: &Path) -> Result<Self, StatusError> {
        let root = fs::canonicalize(root)
            .map_err(|error| StatusError::new(format!("resolve AWACS root: {error}")))?;
        let live = OpenedSubvolume::open(&root)
            .map_err(|error| StatusError::new(format!("inspect AWACS root: {error}")))?;
        let database =
            state_root_for_root(&root)
                .map_err(|error| StatusError::new(format!("resolve AWACS state root: {error}")))?
                .join(root_state_key(&root).map_err(|error| {
                    StatusError::new(format!("inspect AWACS root key: {error}"))
                })?)
                .join("manager.sqlite3");
        let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| {
            StatusError::new(format!(
                "open manager database {}: {error}",
                database.display()
            ))
        })?;
        connection.busy_timeout(Duration::from_millis(250))?;

        let metadata: (Vec<u8>, i64) = connection.query_row(
            "SELECT store_uuid, clock_format_version \
             FROM service_metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let store_uuid = fixed_blob(metadata.0, "store UUID")?;
        let clock_format_version = u32::try_from(metadata.1)
            .map_err(|_| StatusError::new("manager clock format version is invalid"))?;

        let row = connection
            .query_row(
                "SELECT w.id, f.fs_uuid, w.live_subvol_uuid, w.live_path, w.state, \
                        w.fsmonitor_state, w.indexed_seq, w.last_cut_seq, \
                        w.replay_floor_seq, w.clock_epoch, w.fsmonitor_owner_grant_id, \
                        w.view_monitor_session_id \
                   FROM watches w JOIN filesystems f ON f.id = w.filesystem_id \
                  WHERE w.live_subvol_uuid = ?1",
                [live.subvolume.uuid.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Vec<u8>>(9)?,
                        row.get::<_, Option<Vec<u8>>>(10)?,
                        row.get::<_, Option<Vec<u8>>>(11)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| StatusError::new("no AWACS watch belongs to this live subvolume"))?;
        let watch = WatchStatus {
            id: fixed_blob(row.0, "watch ID")?,
            filesystem_uuid: fixed_blob(row.1, "watch filesystem UUID")?,
            live_subvolume_uuid: fixed_blob(row.2, "watch live subvolume UUID")?,
            live_path: PathBuf::from(std::ffi::OsString::from_vec(row.3)),
            state: row.4,
            fsmonitor_state: row.5,
            indexed_sequence: optional_sequence(row.6, "indexed sequence")?,
            last_cut_sequence: optional_sequence(row.7, "last cut sequence")?,
            replay_floor_sequence: optional_sequence(row.8, "replay floor sequence")?,
            clock_epoch: fixed_blob(row.9, "watch clock epoch")?,
            owner_grant_id: row
                .10
                .map(|value| fixed_blob(value, "watch owner grant ID"))
                .transpose()?,
            monitor_session_id: row
                .11
                .map(|value| fixed_blob(value, "watch monitor session ID"))
                .transpose()?,
        };

        let mut issues = Vec::new();
        if watch.filesystem_uuid != live.filesystem.fs_uuid {
            issues.push("watch filesystem UUID does not match the live root".to_owned());
        }
        if watch.live_path.as_os_str().as_bytes() != root.as_os_str().as_bytes() {
            issues.push(format!(
                "watch live path {} does not match {}",
                watch.live_path.display(),
                root.display()
            ));
        }
        if watch.state != "active" {
            issues.push(format!("watch is {} instead of active", watch.state));
        }
        if matches!(watch.fsmonitor_state.as_str(), "disabled" | "guard_gapped") {
            issues.push(format!("filesystem monitor is {}", watch.fsmonitor_state));
        }
        if watch.indexed_sequence != watch.last_cut_sequence {
            issues.push("indexed sequence does not match the latest cut".to_owned());
        }

        let pending_operations = {
            let mut statement = connection.prepare(
                "SELECT kind, state, sequence FROM operations \
                  WHERE watch_id = ?1 AND state NOT IN ('done', 'failed') \
                  ORDER BY updated_ns",
            )?;
            let rows = statement.query_map([watch.id.as_slice()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })?;
            rows.map(|row| {
                let (kind, state, sequence) = row?;
                Ok(PendingOperation {
                    kind,
                    state,
                    sequence: optional_sequence(sequence, "pending operation sequence")?,
                })
            })
            .collect::<Result<Vec<_>, StatusError>>()?
        };
        let pending_baselines: u64 = connection.query_row(
            "SELECT count(*) FROM snapshot_pins p \
                  JOIN snapshots s ON s.id = p.snapshot_id \
                  JOIN watches w ON w.filesystem_id = s.filesystem_id \
             WHERE w.id = ?1 AND p.owner_kind = 'consumer-baseline' AND p.reason = 'pending'",
            [watch.id.as_slice()],
            |row| row.get(0),
        )?;

        Ok(Self {
            database,
            watch,
            pending_operations,
            pending_baselines,
            issues,
            connection,
            store_uuid,
            clock_format_version,
        })
    }

    /// Applies identical cursor-integrity and retention checks to any consumer.
    pub fn inspect_baseline(&self, baseline: &SnapshotBaseline) -> BaselineStatus {
        let owner_id = baseline.retention_token.as_slice().try_into().ok();
        let mut status = BaselineStatus {
            owner_id,
            snapshot_uuid: baseline.identity.subvolume_uuid,
            sequence: None,
            committed_pin: false,
            pending_pin: false,
            issues: Vec::new(),
        };
        if baseline.identity.filesystem_uuid != self.watch.filesystem_uuid {
            status
                .issues
                .push("baseline filesystem UUID does not match this watch".to_owned());
        }
        if !baseline.identity.read_only {
            status
                .issues
                .push("baseline does not identify a read-only snapshot".to_owned());
        }
        let Some(owner_id) = owner_id else {
            status
                .issues
                .push("baseline owner is missing or is not a UUID".to_owned());
            return status;
        };

        match decode_direct_scan_cursor(&baseline.continuity_token) {
            Ok(claims) => {
                status.sequence = Some(claims.cut_sequence);
                if claims.format_version != self.clock_format_version {
                    status
                        .issues
                        .push("cursor format version is stale".to_owned());
                }
                if claims.store_uuid != self.store_uuid {
                    status
                        .issues
                        .push("cursor belongs to another AWACS manager".to_owned());
                }
                if claims.watch_id != self.watch.id {
                    status
                        .issues
                        .push("cursor belongs to another worktree watch".to_owned());
                }
                if let Err(error) = self.inspect_cursor_boundary(
                    claims.cut_sequence,
                    claims.cursor_epoch,
                    claims.target_snapshot_uuid,
                    &mut status.issues,
                ) {
                    status
                        .issues
                        .push(format!("inspect cursor boundary: {error}"));
                }
                if Some(claims.owner_grant_id) != self.watch.owner_grant_id {
                    status
                        .issues
                        .push("cursor authorization grant is stale".to_owned());
                }
                if claims.cursor_epoch == self.watch.clock_epoch
                    && Some(claims.monitor_session_id) != self.watch.monitor_session_id
                {
                    status
                        .issues
                        .push("cursor monitor session is stale".to_owned());
                }
                if claims.target_snapshot_uuid != baseline.identity.subvolume_uuid {
                    status
                        .issues
                        .push("cursor names a different snapshot".to_owned());
                }
                if self
                    .watch
                    .replay_floor_sequence
                    .is_some_and(|floor| claims.cut_sequence < floor)
                {
                    status
                        .issues
                        .push("cursor is older than the replay floor".to_owned());
                }
                if self
                    .watch
                    .indexed_sequence
                    .is_some_and(|indexed| claims.cut_sequence > indexed)
                {
                    status
                        .issues
                        .push("cursor is newer than the indexed head".to_owned());
                }
                if let Err(error) = self.inspect_replay(claims.cut_sequence, &mut status.issues) {
                    status
                        .issues
                        .push(format!("inspect replay history: {error}"));
                }
            }
            Err(error) => status
                .issues
                .push(format!("cursor integrity check failed: {error}")),
        }

        if let Err(error) = self.inspect_snapshot(baseline, owner_id, &mut status) {
            status.issues.push(error.to_string());
        }
        status
    }

    /// Opens the exact immutable snapshot retained by a committed non-Git
    /// consumer. The caller must hold that consumer's journal lock until it
    /// finishes using the returned path, so the committed pin cannot advance.
    pub fn open_retained_baseline_snapshot(
        &self,
        baseline: &SnapshotBaseline,
    ) -> Result<(PathBuf, OpenedSubvolume), StatusError> {
        let status = self.inspect_baseline(baseline);
        if !self.issues.is_empty()
            || !status.issues.is_empty()
            || !status.committed_pin
            || status.owner_id == Some(self.watch.id)
        {
            return Err(StatusError::new(
                "source baseline must have a valid committed immutable-snapshot pin",
            ));
        }
        let path: Vec<u8> = self.connection.query_row(
            "SELECT s.path FROM snapshots s \
             JOIN filesystems f ON f.id = s.filesystem_id \
             WHERE f.fs_uuid = ?1 AND s.subvol_uuid = ?2",
            params![
                baseline.identity.filesystem_uuid.as_slice(),
                baseline.identity.subvolume_uuid.as_slice(),
            ],
            |row| row.get(0),
        )?;
        let path = PathBuf::from(std::ffi::OsString::from_vec(path));
        let snapshot = OpenedSubvolume::open(&path)
            .map_err(|error| StatusError::new(format!("open retained source snapshot: {error}")))?;
        if snapshot.filesystem.fs_uuid != baseline.identity.filesystem_uuid
            || snapshot.subvolume.uuid != baseline.identity.subvolume_uuid
            || !snapshot.subvolume.readonly()
        {
            return Err(StatusError::new(
                "retained source snapshot identity or read-only flag changed",
            ));
        }
        Ok((path, snapshot))
    }

    fn inspect_cursor_boundary(
        &self,
        sequence: u64,
        cursor_epoch: [u8; 16],
        snapshot_uuid: [u8; 16],
        issues: &mut Vec<String>,
    ) -> Result<(), StatusError> {
        let sequence = i64::try_from(sequence)
            .map_err(|_| StatusError::new("cursor sequence exceeds SQLite range"))?;
        let boundary = self
            .connection
            .query_row(
                "SELECT b.clock_epoch, s.subvol_uuid \
                   FROM fsmonitor_boundaries b \
                   JOIN snapshots s ON s.id = b.target_snapshot_id \
                  WHERE b.watch_id = ?1 AND b.cut_sequence = ?2",
                params![self.watch.id.as_slice(), sequence],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let Some((boundary_epoch, boundary_snapshot_uuid)) = boundary else {
            issues.push("cursor has no recorded historical boundary".to_owned());
            return Ok(());
        };
        if fixed_blob(boundary_epoch, "cursor boundary epoch")? != cursor_epoch {
            issues.push("cursor epoch does not match its recorded boundary".to_owned());
        }
        if fixed_blob(boundary_snapshot_uuid, "cursor boundary snapshot UUID")? != snapshot_uuid {
            issues.push("cursor snapshot does not match its recorded boundary".to_owned());
        }
        Ok(())
    }

    fn inspect_replay(&self, sequence: u64, issues: &mut Vec<String>) -> Result<(), StatusError> {
        let Some(indexed) = self.watch.indexed_sequence else {
            return Ok(());
        };
        if sequence >= indexed {
            return Ok(());
        }
        let sequence = i64::try_from(sequence)
            .map_err(|_| StatusError::new("cursor sequence exceeds SQLite range"))?;
        let indexed = i64::try_from(indexed)
            .map_err(|_| StatusError::new("indexed sequence exceeds SQLite range"))?;
        if !crate::manager::has_complete_retained_replay(
            &self.connection,
            self.watch.id,
            sequence,
            indexed,
        )
        .map_err(|error| StatusError::new(error.to_string()))?
        {
            issues.push(format!(
                "replay history is incomplete between cuts {sequence} and {indexed}"
            ));
        }
        Ok(())
    }

    fn inspect_snapshot(
        &self,
        baseline: &SnapshotBaseline,
        owner_id: [u8; 16],
        status: &mut BaselineStatus,
    ) -> Result<(), StatusError> {
        let snapshot = self
            .connection
            .query_row(
                "SELECT s.id, s.path, s.readonly, s.physical_state, r.state \
                   FROM snapshots s \
                   JOIN filesystems f ON f.id = s.filesystem_id \
                   LEFT JOIN revisions r ON r.snapshot_id = s.id \
                  WHERE f.fs_uuid = ?1 AND s.subvol_uuid = ?2",
                params![
                    baseline.identity.filesystem_uuid.as_slice(),
                    baseline.identity.subvolume_uuid.as_slice()
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| StatusError::new("baseline snapshot is absent from the manager"))?;
        let (snapshot_id, path, readonly, physical_state, revision_state) = snapshot;
        let path = PathBuf::from(std::ffi::OsString::from_vec(path));
        let logical_git_cursor = owner_id == self.watch.id;
        if !readonly {
            status
                .issues
                .push("snapshot is not recorded as read-only".to_owned());
        }
        if !logical_git_cursor && physical_state != "present" {
            status
                .issues
                .push(format!("snapshot is {physical_state} instead of present"));
        }
        if !logical_git_cursor && revision_state.as_deref() != Some("ready") {
            status
                .issues
                .push("snapshot revision is not ready".to_owned());
        }

        if !logical_git_cursor {
            match OpenedSubvolume::open(&path) {
                Ok(snapshot) => {
                    if snapshot.filesystem.fs_uuid != baseline.identity.filesystem_uuid {
                        status
                            .issues
                            .push("physical snapshot filesystem UUID differs".to_owned());
                    }
                    if snapshot.subvolume.uuid != baseline.identity.subvolume_uuid {
                        status
                            .issues
                            .push("physical snapshot UUID differs".to_owned());
                    }
                    if !snapshot.subvolume.readonly() {
                        status
                            .issues
                            .push("physical snapshot is not read-only".to_owned());
                    }
                }
                Err(error) => status
                    .issues
                    .push(format!("physical snapshot is unavailable: {error}")),
            }
        }

        let mut statement = self.connection.prepare(
            "SELECT reason FROM snapshot_pins \
              WHERE snapshot_id = ?1 AND owner_kind = 'consumer-baseline' AND owner_id = ?2",
        )?;
        let reasons = statement.query_map(params![snapshot_id, owner_id.as_slice()], |row| {
            row.get::<_, String>(0)
        })?;
        for reason in reasons {
            match reason?.as_str() {
                "committed" | "git-index" => status.committed_pin = true,
                "pending" => status.pending_pin = true,
                _ => {}
            }
        }
        if !logical_git_cursor && !status.committed_pin {
            status
                .issues
                .push("snapshot has no committed pin for this consumer owner".to_owned());
        }
        if status.pending_pin {
            status
                .issues
                .push("consumer has an unfinished pending baseline pin".to_owned());
        }

        if let Some(sequence) = status.sequence {
            let sequence = i64::try_from(sequence)
                .map_err(|_| StatusError::new("cursor sequence exceeds SQLite range"))?;
            let exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM watch_cuts \
                  WHERE watch_id = ?1 AND sequence = ?2 \
                    AND target_snapshot_id = ?3 AND state = 'ready')",
                params![self.watch.id.as_slice(), sequence, snapshot_id],
                |row| row.get(0),
            )?;
            if !exists {
                status
                    .issues
                    .push("snapshot is not the cursor's ready watch cut".to_owned());
            }
        }
        Ok(())
    }
}

pub fn inspect_git_index(index_path: &Path) -> Result<GitIndexStatus, StatusError> {
    let bytes = fs::read(index_path).map_err(|error| {
        StatusError::new(format!("read Git index {}: {error}", index_path.display()))
    })?;
    if bytes.len() < 12 || &bytes[..4] != b"DIRC" {
        return Err(StatusError::new("Git index header is missing or malformed"));
    }
    let version = read_be_u32(&bytes[4..8]).map_err(StatusError::new)?;
    let entries = read_be_u32(&bytes[8..12]).map_err(StatusError::new)?;
    let mut selected = None;
    for hash_len in [20, 32] {
        if bytes.len() < 12 + hash_len {
            continue;
        }
        let checksum_start = bytes.len() - hash_len;
        let trailer = &bytes[checksum_start..];
        let checksum = if trailer.iter().all(|byte| *byte == 0) {
            IndexChecksum::Omitted
        } else if git_index_checksum(&bytes[..checksum_start], hash_len)
            .map_err(StatusError::new)?
            == trailer
        {
            IndexChecksum::Valid
        } else {
            continue;
        };
        match git_index_extension_data_range(&bytes, hash_len, b"FSMN") {
            Ok(extension) => {
                selected = Some((checksum, extension));
                break;
            }
            Err(_) if hash_len == 20 => continue,
            Err(error) => return Err(StatusError::new(error)),
        }
    }
    let (checksum, extension) = selected.ok_or_else(|| {
        StatusError::new("Git index checksum is invalid or index entries are malformed")
    })?;
    let fsmonitor = match extension {
        None => GitFsmonitorStatus::Missing,
        Some(range) => inspect_fsmonitor_extension(&bytes[range]),
    };
    Ok(GitIndexStatus {
        version,
        entries,
        checksum,
        fsmonitor,
    })
}

fn inspect_fsmonitor_extension(extension: &[u8]) -> GitFsmonitorStatus {
    if extension.len() < 5 {
        return GitFsmonitorStatus::Invalid("fsmonitor extension is truncated".to_owned());
    }
    let Ok(version) = read_be_u32(extension) else {
        return GitFsmonitorStatus::Invalid("fsmonitor extension version is malformed".to_owned());
    };
    if version != 2 {
        return GitFsmonitorStatus::Invalid(format!(
            "fsmonitor extension uses unsupported version {version}"
        ));
    }
    let Some(end) = extension[4..].iter().position(|byte| *byte == 0) else {
        return GitFsmonitorStatus::Invalid("fsmonitor token is not NUL terminated".to_owned());
    };
    let token = &extension[4..4 + end];
    if !token.is_empty() && token.iter().all(u8::is_ascii_digit) {
        return GitFsmonitorStatus::TimestampPlaceholder;
    }
    if !token.starts_with(b"awacs-git-v2:") {
        return GitFsmonitorStatus::Invalid("fsmonitor token is not an AWACS v2 token".to_owned());
    }
    match decode_git_fsmonitor_token(token) {
        Some(baseline) => GitFsmonitorStatus::Baseline(baseline),
        None => GitFsmonitorStatus::Invalid("AWACS fsmonitor token is malformed".to_owned()),
    }
}

/// Encodes a verified baseline for Git's independent, watch-owned index lane.
pub fn encode_git_fsmonitor_token(baseline: &SnapshotBaseline) -> Result<String, String> {
    if baseline.continuity_token.is_empty() {
        return Err("AWACS Git fsmonitor token requires established continuity".to_owned());
    }
    let continuity = std::str::from_utf8(&baseline.continuity_token)
        .map_err(|_| "AWACS continuity token is not UTF-8".to_owned())?;
    let owner_id: [u8; 16] = baseline
        .retention_token
        .as_slice()
        .try_into()
        .map_err(|_| "AWACS Git lane owner is not a UUID".to_owned())?;
    Ok(format!(
        "awacs-git-v2:{}:{}:{}:{continuity}",
        Uuid::from_bytes(owner_id),
        Uuid::from_bytes(baseline.identity.filesystem_uuid),
        Uuid::from_bytes(baseline.identity.subvolume_uuid),
    ))
}

pub fn decode_git_fsmonitor_token(token: &[u8]) -> Option<SnapshotBaseline> {
    let token = std::str::from_utf8(token).ok()?;
    let (retention_token, filesystem_uuid, subvolume_uuid, continuity_token) =
        if token.starts_with("awacs-git-v2:") {
            let mut fields = token.splitn(5, ':');
            if fields.next()? != "awacs-git-v2" {
                return None;
            }
            (
                Uuid::parse_str(fields.next()?).ok()?.as_bytes().to_vec(),
                *Uuid::parse_str(fields.next()?).ok()?.as_bytes(),
                *Uuid::parse_str(fields.next()?).ok()?.as_bytes(),
                fields.next()?.as_bytes().to_vec(),
            )
        } else if token.starts_with("awacs-git-v1:") {
            let mut fields = token.splitn(4, ':');
            if fields.next()? != "awacs-git-v1" {
                return None;
            }
            (
                Vec::new(),
                *Uuid::parse_str(fields.next()?).ok()?.as_bytes(),
                *Uuid::parse_str(fields.next()?).ok()?.as_bytes(),
                fields.next()?.as_bytes().to_vec(),
            )
        } else {
            return None;
        };
    if continuity_token.is_empty() {
        return None;
    }
    Some(SnapshotBaseline {
        identity: SnapshotIdentity {
            filesystem_uuid,
            subvolume_uuid,
            read_only: true,
        },
        continuity_token,
        retention_token,
    })
}

pub fn git_index_checksum(bytes: &[u8], hash_len: usize) -> Result<Vec<u8>, String> {
    git_index_checksum_slices([bytes], hash_len)
}

pub fn git_index_checksum_slices<'a>(
    slices: impl IntoIterator<Item = &'a [u8]>,
    hash_len: usize,
) -> Result<Vec<u8>, String> {
    match hash_len {
        20 => {
            let mut hash = Sha1::new();
            for bytes in slices {
                hash.update(bytes);
            }
            Ok(hash.finalize().to_vec())
        }
        32 => {
            let mut hash = Sha256::new();
            for bytes in slices {
                hash.update(bytes);
            }
            Ok(hash.finalize().to_vec())
        }
        _ => Err(format!("unsupported Git object hash length {hash_len}")),
    }
}

/// Reads one extension without loading index entries when a valid EOIE is present.
///
/// Like `git_index_extension_data_range`, this is an extension reader, not a
/// validator of the index's content checksum. `inspect_git_index` retains that
/// separate, full-file check. EOIE checks extension-header integrity, not the
/// entry bytes or extension contents.
pub fn read_git_index_extension(
    index_path: &Path,
    hash_len: usize,
    wanted: &[u8; 4],
) -> Result<Option<Vec<u8>>, String> {
    let mut file = fs::File::open(index_path)
        .map_err(|error| format!("open Git index for extension read: {error}"))?;
    read_git_index_extension_from_file(&mut file, hash_len, wanted)
}

/// Reads an extension from the caller's held index version, even if its
/// pathname is atomically replaced. The same descriptor can then be used to
/// copy it.
pub fn read_git_index_extension_from_file(
    file: &mut fs::File,
    hash_len: usize,
    wanted: &[u8; 4],
) -> Result<Option<Vec<u8>>, String> {
    let started = Instant::now();
    let mut stats = GitIndexExtensionReadStats::default();
    let result = (|| {
        // Keep this descriptor for every seek and for the fallback. A concurrent
        // atomic replacement of the path must not mix two versions of the index.
        let before = file
            .metadata()
            .map_err(|error| format!("inspect Git index before extension read: {error}"))?;
        let result =
            read_git_index_extension_from_reader(file, before.len(), hash_len, wanted, &mut stats);
        let after = file
            .metadata()
            .map_err(|error| format!("inspect Git index after extension read: {error}"))?;
        if git_index_read_metadata_changed(&before, &after) {
            return Err("Git index changed in place during extension read".to_owned());
        }
        result
    })();
    tracing::info!(
        pid = std::process::id(),
        mode = stats.mode,
        fallback_reason = stats.fallback_reason,
        bytes_read = stats.bytes_read,
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        success = result.is_ok(),
        "Git index extension read"
    );
    result
}

struct GitIndexExtensionReadStats {
    bytes_read: u64,
    mode: &'static str,
    fallback_reason: &'static str,
}

impl Default for GitIndexExtensionReadStats {
    fn default() -> Self {
        Self {
            bytes_read: 0,
            mode: "header",
            fallback_reason: "none",
        }
    }
}

fn git_index_read_metadata_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        // Replacing the pathname unlinks our old inode, changing ctime but not
        // its contents. Permit that case; with an unchanged link count, a ctime
        // change also catches an in-place writer that restored the old mtime.
        || (before.nlink() <= after.nlink()
            && (before.ctime() != after.ctime() || before.ctime_nsec() != after.ctime_nsec()))
}

fn read_git_index_at<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    mut buffer: &mut [u8],
    stats: &mut GitIndexExtensionReadStats,
) -> Result<(), String> {
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek Git index: {error}"))?;
    while !buffer.is_empty() {
        match reader.read(buffer) {
            Ok(0) => return Err("truncated Git index while reading extension".to_owned()),
            Ok(read) => {
                stats.bytes_read += read as u64;
                buffer = &mut buffer[read..];
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("read Git index extension: {error}")),
        }
    }
    Ok(())
}

fn git_index_read_buffer(len: u64) -> Result<Vec<u8>, String> {
    let len = usize::try_from(len).map_err(|_| "Git index read size exceeds address space")?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|error| format!("allocate Git index read buffer: {error}"))?;
    bytes.resize(len, 0);
    Ok(bytes)
}

fn read_git_index_extension_from_reader<R: Read + Seek>(
    reader: &mut R,
    index_len: u64,
    hash_len: usize,
    wanted: &[u8; 4],
    stats: &mut GitIndexExtensionReadStats,
) -> Result<Option<Vec<u8>>, String> {
    if !matches!(hash_len, 20 | 32) {
        return Err(format!("unsupported Git object hash length {hash_len}"));
    }
    if index_len < 12 + hash_len as u64 {
        return Err("Git index header is missing or malformed".to_owned());
    }
    let mut header = [0; 12];
    read_git_index_at(reader, 0, &mut header, stats)?;
    if &header[..4] != b"DIRC" {
        return Err("Git index header is missing or malformed".to_owned());
    }
    let version = read_be_u32(&header[4..8])?;
    if !(2..=4).contains(&version) {
        return Err(format!("unsupported Git index version {version}"));
    }
    let entries = read_be_u32(&header[8..12])?;
    match git_index_eoie_extension_range(
        reader, index_len, hash_len, version, entries, wanted, stats,
    ) {
        Ok(range) => {
            stats.mode = "eoie";
            range
                .map(|range| {
                    let mut bytes = git_index_read_buffer(range.end - range.start)?;
                    read_git_index_at(reader, range.start, &mut bytes, stats)?;
                    Ok(bytes)
                })
                .transpose()
        }
        Err(reason) => {
            stats.mode = "full";
            stats.fallback_reason = reason;
            let mut bytes = git_index_read_buffer(index_len)?;
            read_git_index_at(reader, 0, &mut bytes, stats)?;
            let range = git_index_extension_data_range(&bytes, hash_len, wanted)?;
            if let Some(range) = &range {
                // The existing parser returns at the first matching extension.
                // Also validate the remaining chain before accepting its data.
                validate_git_index_extension_suffix(&bytes, range.end, hash_len)?;
            }
            Ok(range.map(|range| bytes[range].to_vec()))
        }
    }
}

fn git_index_eoie_extension_range<R: Read + Seek>(
    reader: &mut R,
    index_len: u64,
    hash_len: usize,
    version: u32,
    entries: u32,
    wanted: &[u8; 4],
    stats: &mut GitIndexExtensionReadStats,
) -> Result<Option<Range<u64>>, &'static str> {
    // EOIE is last, before the index checksum: signature + size + entry-end
    // offset + repository-format hash. Its own header is not in that hash.
    let eoie_len = 8 + 4 + hash_len;
    let eoie_start = index_len
        .checked_sub(hash_len as u64 + eoie_len as u64)
        .filter(|offset| *offset >= 12)
        .ok_or("missing_eoie")?;
    let mut eoie = [0; 8 + 4 + 32];
    let eoie = &mut eoie[..eoie_len];
    read_git_index_at(reader, eoie_start, eoie, stats).map_err(|_| "eoie_read")?;
    if &eoie[..4] != b"EOIE" {
        return Err("missing_eoie");
    }
    if read_be_u32(&eoie[4..8]).map_err(|_| "eoie_size")? as usize != 4 + hash_len {
        return Err("eoie_size");
    }
    let extension_start = u64::from(read_be_u32(&eoie[8..12]).map_err(|_| "eoie_offset")?);
    // Cheap necessary bounds on the entry region without reading its payload.
    // Empty paths are allowed (for example, in a split index).
    let min_entry_len = if version == 4 {
        40 + hash_len + 2 + 1 + 1
    } else {
        (40 + hash_len + 2 + 1 + 7) & !7
    } as u64;
    let min_extension_start = 12 + u64::from(entries) * min_entry_len;
    if extension_start < min_extension_start
        || extension_start > eoie_start
        || (entries == 0 && extension_start != 12)
    {
        return Err("eoie_offset");
    }

    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    let mut selected = None;
    let mut offset = extension_start;
    while offset < eoie_start {
        if eoie_start - offset < 8 {
            return Err("extension_header_bounds");
        }
        let mut header = [0; 8];
        read_git_index_at(reader, offset, &mut header, stats)
            .map_err(|_| "extension_header_read")?;
        let size = u64::from(read_be_u32(&header[4..8]).map_err(|_| "extension_size")?);
        let payload_start = offset + 8;
        let end = payload_start
            .checked_add(size)
            .filter(|end| *end <= eoie_start)
            .ok_or("extension_bounds")?;
        if &header[..4] == b"EOIE" {
            return Err("nonfinal_eoie");
        }
        if &header[..4] == wanted && selected.is_none() {
            selected = Some(payload_start..end);
        }
        if hash_len == 20 {
            sha1.update(header);
        } else {
            sha256.update(header);
        }
        offset = end;
    }
    let digest = if hash_len == 20 {
        sha1.finalize().to_vec()
    } else {
        sha256.finalize().to_vec()
    };
    if digest != eoie[12..] {
        return Err("eoie_digest");
    }
    if wanted == b"EOIE" {
        selected = Some(eoie_start + 8..eoie_start + eoie_len as u64);
    }
    Ok(selected)
}

fn validate_git_index_extension_suffix(
    bytes: &[u8],
    mut offset: usize,
    hash_len: usize,
) -> Result<(), String> {
    let extension_end = bytes.len() - hash_len;
    while offset < extension_end {
        if extension_end - offset < 8 {
            return Err("truncated Git index extension header".to_owned());
        }
        let size = read_be_u32(&bytes[offset + 4..offset + 8])? as usize;
        offset = offset
            .checked_add(8)
            .and_then(|offset| offset.checked_add(size))
            .filter(|offset| *offset <= extension_end)
            .ok_or_else(|| "truncated Git index extension".to_owned())?;
    }
    Ok(())
}

/// The entry summary and extension directory from one Git index traversal.
///
/// This describes the file as stored. Required extensions such as split-index
/// `link` can change the meaning of its entries, so callers must delegate to
/// Git before interpreting those entries as a complete index.
#[derive(Debug)]
pub struct GitIndexLayout {
    pub entries: u32,
    pub has_gitlinks: bool,
    pub has_sparse_directories: bool,
    extensions: Vec<([u8; 4], Range<usize>)>,
}

impl GitIndexLayout {
    pub fn extension_data_range(&self, wanted: &[u8; 4]) -> Option<Range<usize>> {
        self.extensions
            .iter()
            .find(|(signature, _)| signature == wanted)
            .map(|(_, range)| range.clone())
    }

    pub fn has_required_extensions(&self) -> bool {
        self.extensions
            .iter()
            .any(|(signature, _)| !signature[0].is_ascii_uppercase())
    }
}

pub fn git_index_layout(bytes: &[u8], hash_len: usize) -> Result<GitIndexLayout, String> {
    git_index_layout_impl(bytes, hash_len, None)
}

/// Visits the stored entries in bitmap order, reconstructing v4 paths without
/// requiring a separate allocation per entry. Required extensions and sparse
/// directories must still be checked in the returned layout before trusting
/// this as a complete index.
pub fn visit_git_index_paths(
    bytes: &[u8],
    hash_len: usize,
    mut visitor: impl FnMut(u32, &[u8]) -> Result<(), String>,
) -> Result<GitIndexLayout, String> {
    git_index_layout_impl(bytes, hash_len, Some(&mut visitor))
}

type GitIndexPathVisitor<'a> = &'a mut dyn FnMut(u32, &[u8]) -> Result<(), String>;

fn git_index_layout_impl(
    bytes: &[u8],
    hash_len: usize,
    mut visitor: Option<GitIndexPathVisitor<'_>>,
) -> Result<GitIndexLayout, String> {
    if !matches!(hash_len, 20 | 32) {
        return Err(format!("unsupported Git object hash length {hash_len}"));
    }
    if bytes.len() < 12 + hash_len || &bytes[..4] != b"DIRC" {
        return Err("Git index header is missing or malformed".to_owned());
    }
    // Never let an entry's variable-length path consume the checksum trailer.
    let bytes = &bytes[..bytes.len() - hash_len];
    let version = read_be_u32(&bytes[4..8])?;
    if !(2..=4).contains(&version) {
        return Err(format!("unsupported Git index version {version}"));
    }
    let entries = read_be_u32(&bytes[8..12])?;
    let mut layout = GitIndexLayout {
        entries,
        has_gitlinks: false,
        has_sparse_directories: false,
        extensions: Vec::new(),
    };
    let mut offset = 12usize;
    let mut previous_path = Vec::new();
    for entry in 0..entries {
        let entry_start = offset;
        offset = offset
            .checked_add(40 + hash_len + 2)
            .filter(|offset| *offset <= bytes.len())
            .ok_or_else(|| "truncated Git index entry".to_owned())?;
        let mode = read_be_u32(&bytes[entry_start + 24..entry_start + 28])?;
        layout.has_gitlinks |= mode & 0o170000 == 0o160000;
        layout.has_sparse_directories |= mode & 0o170000 == 0o040000;
        let flags = u16::from_be_bytes(bytes[offset - 2..offset].try_into().unwrap());
        if flags & 0x4000 != 0 {
            offset = offset
                .checked_add(2)
                .filter(|offset| *offset <= bytes.len())
                .ok_or_else(|| "truncated extended Git index flags".to_owned())?;
            if visitor.is_some() {
                let extended = u16::from_be_bytes(bytes[offset - 2..offset].try_into().unwrap());
                if version == 2 || extended & !0x6000 != 0 {
                    return Err("unsupported extended Git index flags".to_owned());
                }
            }
        }
        if version == 4 {
            if let Some(visitor) = visitor.as_mut() {
                let (strip, suffix_start) = read_git_index_v4_prefix(bytes, offset)?;
                let retained = previous_path
                    .len()
                    .checked_sub(strip)
                    .ok_or_else(|| "Git index v4 prefix exceeds previous path".to_owned())?;
                offset = skip_git_index_nul(bytes, suffix_start)?;
                previous_path.truncate(retained);
                previous_path.extend_from_slice(&bytes[suffix_start..offset - 1]);
                let name_len = usize::from(flags & 0x0fff);
                if name_len != 0x0fff && previous_path.len() != name_len {
                    return Err("Git index v4 path length does not match its flags".to_owned());
                }
                visitor(entry, &previous_path)?;
            } else {
                offset = skip_git_index_leb128(bytes, offset)?;
                offset = skip_git_index_nul(bytes, offset)?;
            }
        } else {
            let path_start = offset;
            let path_len = (flags & 0x0fff) as usize;
            offset = if path_len == 0x0fff {
                skip_git_index_nul(bytes, offset)?
            } else {
                let path_end = offset
                    .checked_add(path_len)
                    .filter(|offset| *offset < bytes.len())
                    .ok_or_else(|| "truncated Git index path".to_owned())?;
                if bytes[path_end] != 0 {
                    return Err("unterminated Git index path".to_owned());
                }
                path_end + 1
            };
            if let Some(visitor) = visitor.as_mut() {
                let path = &bytes[path_start..offset - 1];
                if path.contains(&0) {
                    return Err("Git index path contains an embedded NUL".to_owned());
                }
                visitor(entry, path)?;
            }
            let padded_len = (offset - entry_start + 7) & !7;
            offset = entry_start
                .checked_add(padded_len)
                .filter(|offset| *offset <= bytes.len())
                .ok_or_else(|| "truncated Git index padding".to_owned())?;
        }
    }
    while offset < bytes.len() {
        if bytes.len() - offset < 8 {
            return Err("truncated Git index extension header".to_owned());
        }
        let signature: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
        let size = read_be_u32(&bytes[offset + 4..offset + 8])? as usize;
        offset += 8;
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| "truncated Git index extension".to_owned())?;
        layout.extensions.push((signature, offset..end));
        offset = end;
    }
    Ok(layout)
}

pub fn git_index_extension_data_range(
    bytes: &[u8],
    hash_len: usize,
    wanted: &[u8; 4],
) -> Result<Option<Range<usize>>, String> {
    Ok(git_index_layout(bytes, hash_len)?.extension_data_range(wanted))
}

pub fn read_be_u32(bytes: &[u8]) -> Result<u32, String> {
    let bytes: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| "truncated u32".to_owned())?
        .try_into()
        .unwrap();
    Ok(u32::from_be_bytes(bytes))
}

fn skip_git_index_leb128(bytes: &[u8], mut offset: usize) -> Result<usize, String> {
    loop {
        let byte = *bytes
            .get(offset)
            .ok_or_else(|| "truncated Git index v4 path prefix".to_owned())?;
        offset += 1;
        if byte & 0x80 == 0 {
            return Ok(offset);
        }
    }
}

fn read_git_index_v4_prefix(bytes: &[u8], mut offset: usize) -> Result<(usize, usize), String> {
    // Git's offset varint uses an extra one before every continuation shift;
    // it is not unsigned LEB128.
    let mut value = 0usize;
    loop {
        let byte = *bytes
            .get(offset)
            .ok_or_else(|| "truncated Git index v4 path prefix".to_owned())?;
        offset += 1;
        value = value
            .checked_mul(128)
            .and_then(|value| value.checked_add(usize::from(byte & 0x7f)))
            .ok_or_else(|| "Git index v4 path prefix overflows".to_owned())?;
        if byte & 0x80 == 0 {
            return Ok((value, offset));
        }
        value = value
            .checked_add(1)
            .ok_or_else(|| "Git index v4 path prefix overflows".to_owned())?;
    }
}

fn skip_git_index_nul(bytes: &[u8], offset: usize) -> Result<usize, String> {
    bytes
        .get(offset..)
        .ok_or_else(|| "unterminated Git index path".to_owned())?
        .iter()
        .position(|byte| *byte == 0)
        .map(|index| offset + index + 1)
        .ok_or_else(|| "unterminated Git index path".to_owned())
}

fn fixed_blob<const N: usize>(bytes: Vec<u8>, label: &str) -> Result<[u8; N], StatusError> {
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        StatusError::new(format!("{label} has length {}, expected {N}", bytes.len()))
    })
}

fn optional_sequence(sequence: Option<i64>, label: &str) -> Result<Option<u64>, StatusError> {
    sequence
        .map(|sequence| {
            u64::try_from(sequence)
                .map_err(|_| StatusError::new(format!("{label} must not be negative")))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::{BoundaryKind, CursorClaims, encode_direct_scan_cursor};
    use std::io::{Cursor, Write};

    struct ExtensionIndexFixture {
        bytes: Vec<u8>,
        entries: Range<usize>,
        other_payloads: Vec<Range<usize>>,
        wanted: Option<Vec<u8>>,
        eoie_start: Option<usize>,
    }

    fn extension_index_fixture(
        version: u32,
        hash_len: usize,
        with_wanted: bool,
        with_eoie: bool,
    ) -> ExtensionIndexFixture {
        let mut bytes = b"DIRC".to_vec();
        bytes.extend_from_slice(&version.to_be_bytes());
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        for entry in 0..2 {
            let start = bytes.len();
            bytes.extend_from_slice(&[0; 40]);
            bytes.extend_from_slice(&vec![entry + 1; hash_len]);
            let name = format!("file-{entry}");
            let flags = name.len() as u16 | if version == 3 { 0x4000 } else { 0 };
            bytes.extend_from_slice(&flags.to_be_bytes());
            if version == 3 {
                bytes.extend_from_slice(&0x4000_u16.to_be_bytes());
            }
            if version == 4 {
                // Replace the entire previous path, avoiding compression in
                // this fixture while still exercising v4 prefix encoding.
                bytes.push(if entry == 0 { 0 } else { name.len() as u8 });
            }
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
            if version != 4 {
                bytes.resize(start + ((bytes.len() - start + 7) & !7), 0);
            }
        }
        let entries = 12..bytes.len();
        let wanted = with_wanted.then(|| b"requested extension data".to_vec());
        let mut other_payloads = Vec::new();
        let mut headers = Vec::new();
        for (signature, payload) in [
            (*b"TREE", Some(vec![0x31; 32 * 1024])),
            (*b"FSMN", wanted.clone()),
            (*b"UNTR", Some(vec![0x32; 16 * 1024])),
        ] {
            let Some(payload) = payload else { continue };
            let header_start = bytes.len();
            bytes.extend_from_slice(&signature);
            bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            headers.extend_from_slice(&bytes[header_start..]);
            let payload_start = bytes.len();
            bytes.extend_from_slice(&payload);
            if signature != *b"FSMN" {
                other_payloads.push(payload_start..bytes.len());
            }
        }
        let eoie_start = with_eoie.then(|| {
            let start = bytes.len();
            bytes.extend_from_slice(b"EOIE");
            bytes.extend_from_slice(&((4 + hash_len) as u32).to_be_bytes());
            bytes.extend_from_slice(&(entries.end as u32).to_be_bytes());
            bytes.extend_from_slice(&git_index_checksum(&headers, hash_len).unwrap());
            start
        });
        let checksum = git_index_checksum(&bytes, hash_len).unwrap();
        bytes.extend_from_slice(&checksum);
        ExtensionIndexFixture {
            bytes,
            entries,
            other_payloads,
            wanted,
            eoie_start,
        }
    }

    fn extension_from_bytes(
        bytes: &[u8],
        hash_len: usize,
        wanted: &[u8; 4],
    ) -> (Result<Option<Vec<u8>>, String>, GitIndexExtensionReadStats) {
        let mut stats = GitIndexExtensionReadStats::default();
        let result = read_git_index_extension_from_reader(
            &mut Cursor::new(bytes),
            bytes.len() as u64,
            hash_len,
            wanted,
            &mut stats,
        );
        (result, stats)
    }

    struct GuardedIndexReader<'a> {
        cursor: Cursor<&'a [u8]>,
        forbidden: Vec<Range<usize>>,
        reads: Vec<Range<usize>>,
    }

    impl Read for GuardedIndexReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let start = self.cursor.position() as usize;
            let end = start
                .saturating_add(buffer.len())
                .min(self.cursor.get_ref().len());
            if self
                .forbidden
                .iter()
                .any(|range| start < range.end && end > range.start)
            {
                return Err(std::io::Error::other("entry or unrelated payload read"));
            }
            let read = self.cursor.read(buffer)?;
            self.reads.push(start..start + read);
            Ok(read)
        }
    }

    impl Seek for GuardedIndexReader<'_> {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.cursor.seek(position)
        }
    }

    #[test]
    fn git_index_eoie_reads_only_headers_and_requested_payload() {
        for hash_len in [20, 32] {
            for version in [2, 3, 4] {
                for with_wanted in [false, true] {
                    let fixture = extension_index_fixture(version, hash_len, with_wanted, true);
                    let mut forbidden = fixture.other_payloads.clone();
                    forbidden.push(fixture.entries.clone());
                    let mut reader = GuardedIndexReader {
                        cursor: Cursor::new(fixture.bytes.as_slice()),
                        forbidden,
                        reads: Vec::new(),
                    };
                    let mut stats = GitIndexExtensionReadStats::default();
                    let result = read_git_index_extension_from_reader(
                        &mut reader,
                        fixture.bytes.len() as u64,
                        hash_len,
                        b"FSMN",
                        &mut stats,
                    )
                    .unwrap();
                    assert_eq!(result, fixture.wanted);
                    assert_eq!(stats.mode, "eoie");
                    let expected_bytes = 12
                        + (8 + 4 + hash_len)
                        + 8 * if with_wanted { 3 } else { 2 }
                        + result.as_ref().map_or(0, Vec::len);
                    assert_eq!(stats.bytes_read, expected_bytes as u64);
                    assert_eq!(
                        stats.bytes_read,
                        reader
                            .reads
                            .iter()
                            .map(|range| range.len() as u64)
                            .sum::<u64>()
                    );
                    assert!(stats.bytes_read < 128);
                }
            }
        }
    }

    #[test]
    fn git_index_extension_falls_back_without_eoie() {
        for hash_len in [20, 32] {
            for version in [2, 3, 4] {
                for with_wanted in [false, true] {
                    let fixture = extension_index_fixture(version, hash_len, with_wanted, false);
                    let (result, stats) = extension_from_bytes(&fixture.bytes, hash_len, b"FSMN");
                    assert_eq!(result.unwrap(), fixture.wanted);
                    assert_eq!(stats.mode, "full");
                    assert_eq!(stats.fallback_reason, "missing_eoie");
                    assert!(stats.bytes_read >= fixture.bytes.len() as u64);
                }
            }
        }
    }

    #[test]
    fn git_index_extension_falls_back_for_stale_eoie() {
        for hash_len in [20, 32] {
            for with_wanted in [false, true] {
                for corruption in ["digest", "offset", "header"] {
                    let mut fixture = extension_index_fixture(3, hash_len, with_wanted, true);
                    let eoie_start = fixture.eoie_start.unwrap();
                    match corruption {
                        "digest" => fixture.bytes[eoie_start + 12] ^= 1,
                        "offset" => fixture.bytes[eoie_start + 8..eoie_start + 12]
                            .copy_from_slice(&12_u32.to_be_bytes()),
                        // The extension still has valid framing; only the EOIE
                        // header digest is now stale, as after an index rewrite.
                        "header" => fixture.bytes[fixture.entries.end] = b'Z',
                        _ => unreachable!(),
                    }
                    let (result, stats) = extension_from_bytes(&fixture.bytes, hash_len, b"FSMN");
                    assert_eq!(result.unwrap(), fixture.wanted);
                    assert_eq!(stats.mode, "full");
                    assert_eq!(
                        stats.fallback_reason,
                        if corruption == "offset" {
                            "eoie_offset"
                        } else {
                            "eoie_digest"
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn git_index_extension_finds_payload_appended_after_eoie() {
        for hash_len in [20, 32] {
            let mut fixture = extension_index_fixture(2, hash_len, false, true);
            fixture.bytes.truncate(fixture.bytes.len() - hash_len);
            fixture.bytes.extend_from_slice(b"FSMN");
            fixture.bytes.extend_from_slice(&3_u32.to_be_bytes());
            fixture.bytes.extend_from_slice(b"new");
            fixture.bytes.extend_from_slice(&vec![0; hash_len]);
            let (result, stats) = extension_from_bytes(&fixture.bytes, hash_len, b"FSMN");
            assert_eq!(result.unwrap(), Some(b"new".to_vec()));
            assert_eq!(stats.mode, "full");
        }
    }

    #[test]
    fn git_index_extension_rejects_malformed_suffix_and_eoie_bounds() {
        for hash_len in [20, 32] {
            for corruption in ["suffix", "eoie_size", "entry_count", "trailer", "header"] {
                let mut fixture = extension_index_fixture(2, hash_len, true, true);
                match corruption {
                    // A requested extension before a malformed suffix must not
                    // hide that malformed chain in either reader path.
                    "suffix" => {
                        let payload_start = fixture.other_payloads[1].start;
                        fixture.bytes[payload_start - 4..payload_start]
                            .copy_from_slice(&u32::MAX.to_be_bytes());
                    }
                    "eoie_size" => {
                        let start = fixture.eoie_start.unwrap();
                        fixture.bytes[start + 4..start + 8]
                            .copy_from_slice(&u32::MAX.to_be_bytes());
                    }
                    "entry_count" => fixture.bytes[8..12].copy_from_slice(&u32::MAX.to_be_bytes()),
                    "trailer" => {
                        fixture.bytes.pop();
                    }
                    "header" => fixture.bytes.truncate(11),
                    _ => unreachable!(),
                }
                let (result, _) = extension_from_bytes(&fixture.bytes, hash_len, b"FSMN");
                assert!(result.is_err(), "{corruption}, hash length {hash_len}");
            }
        }
    }

    #[test]
    fn git_index_eoie_requires_complete_header_chain() {
        for hash_len in [20, 32] {
            let mut fixture = extension_index_fixture(4, hash_len, true, true);
            let start = fixture.eoie_start.unwrap();
            // The final extension leaves a partial header before EOIE. The
            // wanted payload appeared earlier, but it is not safe to return.
            let last_size = fixture.other_payloads[1].len() as u32;
            let last_size_offset = fixture.other_payloads[1].start - 4;
            fixture.bytes[last_size_offset..last_size_offset + 4]
                .copy_from_slice(&(last_size - 1).to_be_bytes());
            let mut stats = GitIndexExtensionReadStats::default();
            let result = git_index_eoie_extension_range(
                &mut Cursor::new(&fixture.bytes),
                fixture.bytes.len() as u64,
                hash_len,
                4,
                2,
                b"FSMN",
                &mut stats,
            );
            assert_eq!(result.unwrap_err(), "extension_header_bounds");
            assert!(start > fixture.entries.end);
            assert!(
                extension_from_bytes(&fixture.bytes, hash_len, b"FSMN")
                    .0
                    .is_err()
            );
        }
    }

    #[test]
    fn git_index_extension_rejects_unsupported_formats() {
        let fixture = extension_index_fixture(2, 20, true, true);
        let mut malformed_header = fixture.bytes.clone();
        malformed_header[0] = b'X';
        assert!(
            extension_from_bytes(&malformed_header, 20, b"FSMN")
                .0
                .unwrap_err()
                .contains("header is missing or malformed")
        );
        for hash_len in [0, 16, 21, usize::MAX] {
            assert!(
                extension_from_bytes(&fixture.bytes, hash_len, b"FSMN")
                    .0
                    .unwrap_err()
                    .contains("unsupported Git object hash length")
            );
        }
        for version in [0, 1, 5, u32::MAX] {
            let mut bytes = fixture.bytes.clone();
            bytes[4..8].copy_from_slice(&version.to_be_bytes());
            assert!(
                extension_from_bytes(&bytes, 20, b"FSMN")
                    .0
                    .unwrap_err()
                    .contains("unsupported Git index version")
            );
        }
    }

    #[test]
    fn git_index_extension_keeps_original_descriptor_across_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let index = directory.path().join("index");
        let replacement = directory.path().join("index.lock");
        for with_eoie in [false, true] {
            let old = extension_index_fixture(2, 20, true, with_eoie);
            let new = extension_index_fixture(2, 20, false, with_eoie);
            fs::write(&index, &old.bytes).unwrap();
            fs::write(&replacement, &new.bytes).unwrap();
            let mut file = fs::File::open(&index).unwrap();
            let before = file.metadata().unwrap();
            fs::rename(&replacement, &index).unwrap();
            let mut stats = GitIndexExtensionReadStats::default();
            assert_eq!(
                read_git_index_extension_from_reader(
                    &mut file,
                    before.len(),
                    20,
                    b"FSMN",
                    &mut stats
                )
                .unwrap(),
                old.wanted
            );
            assert!(!git_index_read_metadata_changed(
                &before,
                &file.metadata().unwrap()
            ));
            assert_eq!(
                read_git_index_extension_from_file(&mut file, 20, b"FSMN").unwrap(),
                old.wanted
            );
            assert_eq!(read_git_index_extension(&index, 20, b"FSMN").unwrap(), None);
        }
    }

    #[test]
    fn git_index_extension_detects_in_place_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let index = directory.path().join("index");
        let fixture = extension_index_fixture(2, 20, true, true);
        fs::write(&index, &fixture.bytes).unwrap();
        let file = fs::File::open(&index).unwrap();
        let before = file.metadata().unwrap();
        let mut writer = fs::OpenOptions::new().write(true).open(&index).unwrap();
        writer.write_all(b"DIRC").unwrap();
        writer
            .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        assert!(git_index_read_metadata_changed(
            &before,
            &file.metadata().unwrap()
        ));
        let before_growth = file.metadata().unwrap();
        writer.set_len(before_growth.len() + 1).unwrap();
        assert!(git_index_read_metadata_changed(
            &before_growth,
            &file.metadata().unwrap()
        ));
    }

    fn write_index(path: &Path, token: Option<&[u8]>, hash_len: usize, omit_checksum: bool) {
        let mut bytes = b"DIRC".to_vec();
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        if let Some(token) = token {
            let mut payload = 2_u32.to_be_bytes().to_vec();
            payload.extend_from_slice(token);
            payload.push(0);
            bytes.extend_from_slice(b"FSMN");
            bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&payload);
        }
        let checksum = if omit_checksum {
            vec![0; hash_len]
        } else {
            git_index_checksum(&bytes, hash_len).unwrap()
        };
        bytes.extend_from_slice(&checksum);
        fs::write(path, bytes).unwrap();
    }

    fn index_with_modes(version: u32, hash_len: usize, modes: &[(u32, u16)]) -> Vec<u8> {
        let mut bytes = b"DIRC".to_vec();
        bytes.extend_from_slice(&version.to_be_bytes());
        bytes.extend_from_slice(&(modes.len() as u32).to_be_bytes());
        for (mode, stage) in modes {
            let start = bytes.len();
            bytes.resize(start + 40, 0);
            bytes[start + 24..start + 28].copy_from_slice(&mode.to_be_bytes());
            bytes.resize(bytes.len() + hash_len, 0);
            let flags = 1 | (stage << 12) | if version == 3 { 0x4000 } else { 0 };
            bytes.extend_from_slice(&flags.to_be_bytes());
            if version == 3 {
                bytes.extend_from_slice(&0x4000_u16.to_be_bytes());
            }
            if version == 4 {
                bytes.push(0); // Keep the previous path and append one component.
                bytes.extend_from_slice(b"a\0");
            } else {
                bytes.extend_from_slice(b"a\0");
                let padded_length = (bytes.len() - start + 7) & !7;
                bytes.resize(start + padded_length, 0);
            }
        }
        bytes.resize(bytes.len() + hash_len, 0);
        bytes
    }

    #[test]
    fn index_layout_detects_gitlinks_at_every_stage_and_supported_encoding() {
        for hash_len in [20, 32] {
            for version in [2, 3, 4] {
                for stage in 0..=3 {
                    let bytes = index_with_modes(
                        version,
                        hash_len,
                        &[(0o100644, 0), (0o120000, 0), (0o160000, stage)],
                    );
                    let layout = git_index_layout(&bytes, hash_len).unwrap();
                    assert_eq!(layout.entries, 3);
                    assert!(layout.has_gitlinks);
                    assert!(!layout.has_sparse_directories);
                    assert!(!layout.has_required_extensions());
                }
                let plain = index_with_modes(version, hash_len, &[(0o100755, 0)]);
                assert!(!git_index_layout(&plain, hash_len).unwrap().has_gitlinks);
                let sparse = index_with_modes(version, hash_len, &[(0o040000, 0)]);
                assert!(
                    git_index_layout(&sparse, hash_len)
                        .unwrap()
                        .has_sparse_directories
                );
            }
        }
    }

    #[test]
    fn index_layout_finds_required_extensions_after_optional_extensions() {
        for required in [b"link", b"sdir", b"next"] {
            let mut bytes = index_with_modes(2, 20, &[(0o100644, 0)]);
            bytes.truncate(bytes.len() - 20);
            for signature in [b"FSMN", b"UNTR", required] {
                bytes.extend_from_slice(signature);
                bytes.extend_from_slice(&3_u32.to_be_bytes());
                bytes.extend_from_slice(b"abc");
            }
            bytes.extend_from_slice(&[0; 20]);
            let layout = git_index_layout(&bytes, 20).unwrap();
            assert!(layout.has_required_extensions());
            for signature in [b"FSMN", b"UNTR", required] {
                let range = layout.extension_data_range(signature).unwrap();
                assert_eq!(&bytes[range], b"abc");
            }
        }
    }

    #[test]
    fn index_layout_handles_long_paths_ending_at_a_padding_boundary() {
        for hash_len in [20, 32] {
            let header_len = 40 + hash_len + 2;
            let name_len = 4095 + (8 - (header_len + 4095 + 1) % 8) % 8;
            let mut bytes = b"DIRC".to_vec();
            bytes.extend_from_slice(&2_u32.to_be_bytes());
            bytes.extend_from_slice(&1_u32.to_be_bytes());
            bytes.resize(12 + 40 + hash_len, 0);
            bytes[12 + 24..12 + 28].copy_from_slice(&0o100644_u32.to_be_bytes());
            bytes.extend_from_slice(&0x0fff_u16.to_be_bytes());
            bytes.resize(bytes.len() + name_len, b'a');
            bytes.push(0);
            assert_eq!((bytes.len() - 12) % 8, 0);
            bytes.extend_from_slice(b"FSMN");
            bytes.extend_from_slice(&3_u32.to_be_bytes());
            bytes.extend_from_slice(b"abc");
            bytes.resize(bytes.len() + hash_len, 0);
            let layout = git_index_layout(&bytes, hash_len).unwrap();
            assert_eq!(
                &bytes[layout.extension_data_range(b"FSMN").unwrap()],
                b"abc"
            );
        }
    }

    #[test]
    fn index_layout_rejects_truncated_entries_paths_and_extension_tails() {
        let original = index_with_modes(4, 20, &[(0o100644, 0)]);
        for removed in [1, 2, 10] {
            let mut bytes = original.clone();
            bytes.drain(bytes.len() - 20 - removed..bytes.len() - 20);
            assert!(git_index_layout(&bytes, 20).is_err());
        }
        let mut bytes = index_with_modes(2, 20, &[]);
        bytes.truncate(12);
        bytes.extend_from_slice(b"FSMN\0\0\0\0tail");
        bytes.extend_from_slice(&[0; 20]);
        assert!(git_index_layout(&bytes, 20).is_err());
        assert!(git_index_layout(&original, usize::MAX).is_err());
    }

    #[test]
    fn index_checksum_slices_match_contiguous_input_across_hash_blocks() {
        let bytes = (0..513).map(|n| (n % 251) as u8).collect::<Vec<_>>();
        for hash_len in [20, 32] {
            for split in [0, 1, 63, 64, 65, 255, 512, 513] {
                let slices = [&bytes[..split], &[][..], &bytes[split..]];
                assert_eq!(
                    git_index_checksum_slices(slices, hash_len).unwrap(),
                    git_index_checksum(&bytes, hash_len).unwrap()
                );
            }
        }
    }

    #[test]
    fn git_index_reports_missing_fsmonitor_and_omitted_checksum() {
        let directory = tempfile::tempdir().unwrap();
        let index = directory.path().join("index");
        write_index(&index, None, 20, true);

        let status = inspect_git_index(&index).unwrap();
        assert_eq!(status.version, 2);
        assert_eq!(status.entries, 0);
        assert_eq!(status.checksum, IndexChecksum::Omitted);
        assert_eq!(status.fsmonitor, GitFsmonitorStatus::Missing);
    }

    #[test]
    fn git_index_distinguishes_placeholder_and_established_token_shape() {
        let directory = tempfile::tempdir().unwrap();
        let index = directory.path().join("index");
        write_index(&index, Some(b"123456789"), 20, false);
        assert_eq!(
            inspect_git_index(&index).unwrap().fsmonitor,
            GitFsmonitorStatus::TimestampPlaceholder
        );

        let token = format!(
            "awacs-git-v2:{}:{}:{}:c:btrfs-awacs:scan:2:opaque",
            Uuid::from_bytes([1; 16]),
            Uuid::from_bytes([2; 16]),
            Uuid::from_bytes([3; 16])
        );
        write_index(&index, Some(token.as_bytes()), 32, false);
        let status = inspect_git_index(&index).unwrap();
        assert_eq!(status.checksum, IndexChecksum::Valid);
        let GitFsmonitorStatus::Baseline(baseline) = status.fsmonitor else {
            panic!("expected an AWACS Git baseline");
        };
        assert_eq!(baseline.retention_token, [1; 16]);
        assert_eq!(baseline.identity.filesystem_uuid, [2; 16]);
        assert_eq!(baseline.identity.subvolume_uuid, [3; 16]);
    }

    #[test]
    fn git_index_rejects_corrupt_checksum_and_legacy_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let index = directory.path().join("index");
        write_index(&index, Some(b"awacs-git-v1:invalid"), 20, false);
        assert!(matches!(
            inspect_git_index(&index).unwrap().fsmonitor,
            GitFsmonitorStatus::Invalid(_)
        ));

        let mut bytes = fs::read(&index).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&index, bytes).unwrap();
        assert!(
            inspect_git_index(&index)
                .unwrap_err()
                .to_string()
                .contains("checksum is invalid")
        );
    }

    fn baseline_fixture() -> (tempfile::TempDir, RootStatus, SnapshotBaseline) {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = directory.path().join("snapshot");
        fs::create_dir(&snapshot).unwrap();
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE filesystems (id INTEGER PRIMARY KEY, fs_uuid BLOB NOT NULL);
                 CREATE TABLE snapshots (
                     id INTEGER PRIMARY KEY, filesystem_id INTEGER NOT NULL,
                     subvol_uuid BLOB NOT NULL, path BLOB NOT NULL,
                     readonly INTEGER NOT NULL, physical_state TEXT NOT NULL
                 );
                 CREATE TABLE revisions (snapshot_id INTEGER, state TEXT);
                 CREATE TABLE snapshot_pins (
                     snapshot_id INTEGER, owner_kind TEXT, owner_id BLOB, reason TEXT
                 );
                 CREATE TABLE watch_cuts (
                     watch_id BLOB, sequence INTEGER, target_snapshot_id INTEGER,
                     state TEXT, fresh_instance INTEGER, comparison_id INTEGER
                 );
                 CREATE TABLE fsmonitor_boundaries (
                     watch_id BLOB, cut_sequence INTEGER, target_snapshot_id INTEGER,
                     clock_epoch BLOB
                 );
                 CREATE TABLE replay_spools (watch_id BLOB, sequence INTEGER);",
            )
            .unwrap();
        let filesystem_uuid = [1; 16];
        let watch_id = [2; 16];
        let snapshot_uuid = [3; 16];
        let owner_id = [4; 16];
        let grant_id = [5; 16];
        let session_id = [6; 16];
        let epoch = [7; 16];
        let store_uuid = [8; 16];
        connection
            .execute(
                "INSERT INTO filesystems(id, fs_uuid) VALUES (1, ?1)",
                [filesystem_uuid.as_slice()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO snapshots(id, filesystem_id, subvol_uuid, path, readonly, physical_state)
                 VALUES (1, 1, ?1, ?2, 1, 'present')",
                params![snapshot_uuid.as_slice(), snapshot.as_os_str().as_bytes()],
            )
            .unwrap();
        connection
            .execute("INSERT INTO revisions VALUES (1, 'ready')", [])
            .unwrap();
        connection
            .execute(
                "INSERT INTO snapshot_pins VALUES (1, 'consumer-baseline', ?1, 'committed')",
                [owner_id.as_slice()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO watch_cuts VALUES (?1, 4, 1, 'ready', 0, 1)",
                [watch_id.as_slice()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO fsmonitor_boundaries VALUES (?1, 4, 1, ?2)",
                params![watch_id.as_slice(), epoch.as_slice()],
            )
            .unwrap();
        let claims = CursorClaims {
            format_version: 1,
            store_uuid,
            watch_id,
            cursor_epoch: epoch,
            cut_sequence: 4,
            owner_grant_id: grant_id,
            monitor_session_id: session_id,
            boundary_kind: BoundaryKind::Cut,
            algorithm_version: 1,
            target_snapshot_uuid: snapshot_uuid,
        };
        let baseline = SnapshotBaseline {
            identity: SnapshotIdentity {
                filesystem_uuid,
                subvolume_uuid: snapshot_uuid,
                read_only: true,
            },
            continuity_token: encode_direct_scan_cursor(&claims),
            retention_token: owner_id.to_vec(),
        };
        let root = RootStatus {
            database: directory.path().join("manager.sqlite3"),
            watch: WatchStatus {
                id: watch_id,
                filesystem_uuid,
                live_subvolume_uuid: [10; 16],
                live_path: directory.path().to_owned(),
                state: "active".to_owned(),
                fsmonitor_state: "snapshot_only".to_owned(),
                indexed_sequence: Some(4),
                last_cut_sequence: Some(4),
                replay_floor_sequence: Some(4),
                clock_epoch: epoch,
                owner_grant_id: Some(grant_id),
                monitor_session_id: Some(session_id),
            },
            pending_operations: Vec::new(),
            pending_baselines: 0,
            issues: Vec::new(),
            connection,
            store_uuid,
            clock_format_version: 1,
        };
        (directory, root, baseline)
    }

    #[test]
    fn baseline_inspection_verifies_cursor_and_checks_owner_pin() {
        let (_directory, root, baseline) = baseline_fixture();
        let status = root.inspect_baseline(&baseline);
        assert_eq!(status.sequence, Some(4));
        assert!(status.committed_pin);
        assert!(!status.pending_pin);
        // The unit fixture deliberately uses an ordinary directory instead of
        // requiring a privileged Btrfs subvolume.
        assert_eq!(status.issues.len(), 1);
        assert!(status.issues[0].starts_with("physical snapshot is unavailable"));

        let mut wrong_owner = baseline.clone();
        wrong_owner.retention_token = [11; 16].to_vec();
        let wrong_owner = root.inspect_baseline(&wrong_owner);
        assert!(!wrong_owner.committed_pin);
        assert!(
            wrong_owner
                .issues
                .iter()
                .any(|issue| issue.contains("no committed pin"))
        );
    }

    #[test]
    fn baseline_inspection_accepts_retained_git_index_cursor_after_reactivation() {
        let (_directory, mut root, baseline) = baseline_fixture();
        root.connection
            .execute(
                "UPDATE snapshot_pins SET reason = 'git-index' WHERE reason = 'committed'",
                [],
            )
            .unwrap();
        root.watch.clock_epoch = [12; 16];
        root.watch.monitor_session_id = Some([13; 16]);

        let status = root.inspect_baseline(&baseline);
        assert!(
            status.committed_pin,
            "an on-disk Git cursor remains durably committed while its replacement is proposed"
        );
        assert!(
            !status
                .issues
                .iter()
                .any(|issue| issue.contains("no committed pin"))
        );
        assert_eq!(status.issues.len(), 1);
        assert!(status.issues[0].starts_with("physical snapshot is unavailable"));
    }

    #[test]
    fn git_logical_cursor_remains_valid_after_physical_snapshot_collection() {
        let (_directory, root, mut baseline) = baseline_fixture();
        baseline.retention_token = root.watch.id.to_vec();
        root.connection
            .execute("DELETE FROM snapshot_pins", [])
            .unwrap();
        root.connection
            .execute("DELETE FROM revisions", [])
            .unwrap();
        root.connection
            .execute("UPDATE snapshots SET physical_state = 'deleted'", [])
            .unwrap();

        let status = root.inspect_baseline(&baseline);
        assert_eq!(status.sequence, Some(4));
        assert!(!status.committed_pin);
        assert!(!status.pending_pin);
        assert!(
            status.issues.is_empty(),
            "a retained journal boundary, not a physical snapshot pin, owns a Git cursor: {:?}",
            status.issues
        );
    }

    #[test]
    fn baseline_inspection_rejects_missing_or_mismatched_historical_boundary() {
        let (_directory, root, baseline) = baseline_fixture();
        root.connection
            .execute(
                "UPDATE fsmonitor_boundaries SET clock_epoch = ?1",
                [[12; 16].as_slice()],
            )
            .unwrap();
        let mismatched = root.inspect_baseline(&baseline);
        assert!(
            mismatched
                .issues
                .iter()
                .any(|issue| issue.contains("epoch does not match its recorded boundary"))
        );

        root.connection
            .execute("DELETE FROM fsmonitor_boundaries", [])
            .unwrap();
        let missing = root.inspect_baseline(&baseline);
        assert!(
            missing
                .issues
                .iter()
                .any(|issue| issue.contains("no recorded historical boundary"))
        );
    }

    #[test]
    fn baseline_inspection_rejects_tampered_cursor_and_expired_replay() {
        let (_directory, mut root, mut baseline) = baseline_fixture();
        let changed = baseline.continuity_token.len() - 10;
        baseline.continuity_token[changed] = if baseline.continuity_token[changed] == b'A' {
            b'B'
        } else {
            b'A'
        };
        let tampered = root.inspect_baseline(&baseline);
        assert!(
            tampered
                .issues
                .iter()
                .any(|issue| issue.contains("cursor integrity check failed"))
        );

        let (_other_directory, _, baseline) = baseline_fixture();
        root.watch.replay_floor_sequence = Some(5);
        let expired = root.inspect_baseline(&baseline);
        assert!(
            expired
                .issues
                .iter()
                .any(|issue| issue.contains("older than the replay floor"))
        );
    }

    #[test]
    fn replay_inspection_accepts_failed_attempt_gaps_but_rejects_missing_history() {
        let (_directory, mut root, _baseline) = baseline_fixture();
        root.connection
            .execute_batch(
                "ALTER TABLE watch_cuts ADD COLUMN base_snapshot_id INTEGER;
                 ALTER TABLE watch_cuts ADD COLUMN base_sequence INTEGER;
                 ALTER TABLE replay_spools ADD COLUMN from_snapshot_id INTEGER;
                 ALTER TABLE replay_spools ADD COLUMN to_snapshot_id INTEGER;
                 PRAGMA user_version = 13;",
            )
            .unwrap();
        for (sequence, base_sequence, base_snapshot, target_snapshot) in
            [(6, 4, 1, 2), (9, 6, 2, 3)]
        {
            root.connection
                .execute(
                    "INSERT INTO watch_cuts(
                         watch_id, sequence, base_sequence, base_snapshot_id,
                         target_snapshot_id, state, fresh_instance, comparison_id
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'ready', 0, ?2)",
                    params![
                        root.watch.id.as_slice(),
                        sequence,
                        base_sequence,
                        base_snapshot,
                        target_snapshot,
                    ],
                )
                .unwrap();
            root.connection
                .execute(
                    "INSERT INTO replay_spools VALUES (?1, ?2, ?3, ?4)",
                    params![
                        root.watch.id.as_slice(),
                        sequence,
                        base_snapshot,
                        target_snapshot,
                    ],
                )
                .unwrap();
        }
        root.watch.indexed_sequence = Some(9);
        let mut issues = Vec::new();
        root.inspect_replay(4, &mut issues).unwrap();
        assert!(issues.is_empty(), "failed attempts need no replay edges");

        root.connection
            .execute("DELETE FROM replay_spools WHERE sequence = 6", [])
            .unwrap();
        root.inspect_replay(4, &mut issues).unwrap();
        assert_eq!(
            issues,
            ["replay history is incomplete between cuts 4 and 9"],
            "missing retained events must not be mistaken for a failed attempt"
        );

        root.connection
            .execute("DELETE FROM watch_cuts WHERE sequence = 6", [])
            .unwrap();
        issues.clear();
        root.inspect_replay(4, &mut issues).unwrap();
        assert_eq!(issues.len(), 1, "a missing predecessor must fail closed");
    }

    #[test]
    fn replay_inspection_reads_legacy_schema_without_migrating_it() {
        let (_directory, mut root, _baseline) = baseline_fixture();
        root.connection
            .execute_batch(
                "ALTER TABLE watch_cuts ADD COLUMN base_snapshot_id INTEGER;
                 ALTER TABLE replay_spools ADD COLUMN from_snapshot_id INTEGER;
                 ALTER TABLE replay_spools ADD COLUMN to_snapshot_id INTEGER;
                 PRAGMA user_version = 12;",
            )
            .unwrap();
        for (sequence, base_snapshot, target_snapshot) in [(5, 1, 2), (6, 2, 3)] {
            root.connection
                .execute(
                    "INSERT INTO watch_cuts(
                         watch_id, sequence, base_snapshot_id, target_snapshot_id,
                         state, fresh_instance, comparison_id
                     ) VALUES (?1, ?2, ?3, ?4, 'ready', 0, ?2)",
                    params![
                        root.watch.id.as_slice(),
                        sequence,
                        base_snapshot,
                        target_snapshot,
                    ],
                )
                .unwrap();
            root.connection
                .execute(
                    "INSERT INTO replay_spools VALUES (?1, ?2, ?3, ?4)",
                    params![
                        root.watch.id.as_slice(),
                        sequence,
                        base_snapshot,
                        target_snapshot,
                    ],
                )
                .unwrap();
        }
        root.connection
            .execute_batch("PRAGMA query_only = ON")
            .unwrap();
        root.watch.indexed_sequence = Some(6);
        let mut issues = Vec::new();
        root.inspect_replay(4, &mut issues).unwrap();
        assert!(
            issues.is_empty(),
            "read-only status supports the v12 schema"
        );
    }
}
