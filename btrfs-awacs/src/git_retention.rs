//! Retain Git's published index cursor and responses it can still publish.
//!
//! A hook response precedes Git's index write. In particular, `git status`
//! refreshes fsmonitor before acquiring its index lock. Keep each response
//! until its Git process has exited, then reconcile the published index before
//! releasing that response's pin. These are ordinary consumer pins so older
//! managers also preserve their physical snapshots and replay intervals.

use crate::bootstrap::git_admin_directory;
use crate::compat::decode_direct_scan_cursor;
use crate::scan::SnapshotBaseline;
use crate::status::{
    GitFsmonitorStatus, decode_git_fsmonitor_token, inspect_fsmonitor_extension,
    read_git_index_extension_bounded_from_file,
};
use crate::store::Store;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const RESPONSE_REASON_PREFIX: &str = "git-response:";
const CURRENT_REASON: &str = "git-index";

/// The process which can publish one fsmonitor response into an index.
///
/// The boot, PID namespace, and start time distinguish reused process IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitPublisher {
    boot_id: [u8; 16],
    pid: u32,
    start_time: u64,
    pid_namespace: (u64, u64),
    mount_namespace: (u64, u64),
    index_path: Option<PathBuf>,
}

/// Capture the nearest Git ancestor before opening the manager or taking cuts.
///
/// Hooks may run through shell wrappers. If the publisher cannot be identified,
/// callers still retain the response, without assuming a deadline for it.
pub fn capture_git_publisher() -> Option<GitPublisher> {
    let index_path = std::env::var_os("GIT_INDEX_FILE").map(PathBuf::from);
    let boot_id = *Uuid::parse_str(
        fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()?
            .trim(),
    )
    .ok()?
    .as_bytes();
    let pid_namespace = current_pid_namespace()?;
    let hook_mount_namespace = current_mount_namespace();
    let mut pid = u32::try_from(unsafe { libc::getppid() }).ok()?;
    let mut visited = Vec::new();
    while pid > 1 && visited.len() < 64 && !visited.contains(&pid) {
        visited.push(pid);
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let identity = parse_process_stat(&stat)?;
        let executable = fs::read_link(format!("/proc/{pid}/exe")).ok();
        if executable
            .as_ref()
            .and_then(|path| path.file_name())
            .is_some_and(|name| name == "git")
            || identity.name == "git"
        {
            let mount_namespace = process_mount_namespace(pid)?;
            // Git runs the hook at the worktree root, but a relative index
            // override belongs to Git's own cwd. Read that process's view.
            let index_path = match index_path.as_ref() {
                Some(path) if !path.is_absolute() => {
                    Some(fs::read_link(format!("/proc/{pid}/cwd")).ok()?.join(path))
                }
                path => path.cloned(),
            }
            .map(|path| {
                if hook_mount_namespace == Some(mount_namespace) {
                    normalize_index_path(&path)
                } else {
                    path
                }
            });
            return Some(GitPublisher {
                boot_id,
                pid,
                start_time: identity.start_time,
                pid_namespace,
                mount_namespace,
                index_path,
            });
        }
        pid = identity.parent_pid;
    }
    None
}

/// Git's retention roots must survive older managers releasing the watch's
/// original consumer lane. Keep their owner distinct without changing cursor
/// tokens or the deployed snapshot-pins schema.
pub(crate) fn retention_owner(watch_id: [u8; 16]) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs:git-retention-owner:1\0");
    hash.update(watch_id);
    let mut owner: [u8; 16] = hash.finalize()[..16].try_into().unwrap();
    if owner == watch_id {
        owner[0] ^= 1;
    }
    owner
}

fn encode_path(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    path.as_os_str()
        .as_bytes()
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 15)]),
            ]
        })
        .collect()
}

fn decode_path(encoded: &str) -> Option<PathBuf> {
    if encoded.len() % 2 != 0 {
        return None;
    }
    let bytes = encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect::<Option<Vec<_>>>()?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
    (!path.as_os_str().is_empty()
        && !path
            .components()
            .any(|component| component == std::path::Component::ParentDir))
    .then_some(path)
}

fn index_reason(mount_namespace: (u64, u64), path: &Path) -> String {
    format!(
        "{CURRENT_REASON}:{}:{}:{}",
        mount_namespace.0,
        mount_namespace.1,
        encode_path(path)
    )
}

fn index_from_reason(reason: &str) -> Option<((u64, u64), PathBuf)> {
    let mut fields = reason.strip_prefix("git-index:")?.split(':');
    let mount_namespace = (fields.next()?.parse().ok()?, fields.next()?.parse().ok()?);
    let path = decode_path(fields.next()?)?;
    fields.next().is_none().then_some((mount_namespace, path))
}

fn normalize_index_path(path: &Path) -> PathBuf {
    // Git's index lock follows symbolic links too. Normalize aliases so the
    // same index does not acquire a new durable root for each path spelling.
    fs::canonicalize(path)
        .or_else(|_| {
            let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
            let name = path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
            fs::canonicalize(parent).map(|parent| parent.join(name))
        })
        .unwrap_or_else(|_| path.to_owned())
}

impl GitPublisher {
    fn reason(&self) -> String {
        format!(
            "{RESPONSE_REASON_PREFIX}{}:{}:{}:{}:{}:{}:{}:{}",
            Uuid::from_bytes(self.boot_id),
            self.pid,
            self.start_time,
            self.pid_namespace.0,
            self.pid_namespace.1,
            self.mount_namespace.0,
            self.mount_namespace.1,
            self.index_path
                .as_ref()
                .map(|path| encode_path(path))
                .unwrap_or_default(),
        )
    }

    fn from_reason(reason: &str) -> Option<Self> {
        let mut fields = reason.strip_prefix(RESPONSE_REASON_PREFIX)?.split(':');
        let boot_id = *Uuid::parse_str(fields.next()?).ok()?.as_bytes();
        let pid = fields.next()?.parse().ok()?;
        let start_time = fields.next()?.parse().ok()?;
        let pid_namespace = (fields.next()?.parse().ok()?, fields.next()?.parse().ok()?);
        let mount_namespace = (fields.next()?.parse().ok()?, fields.next()?.parse().ok()?);
        let index_path = match fields.next()? {
            "" => None,
            encoded => Some(decode_path(encoded)?),
        };
        if fields.next().is_some() || pid == 0 {
            return None;
        }
        Some(Self {
            boot_id,
            pid,
            start_time,
            pid_namespace,
            mount_namespace,
            index_path,
        })
    }

    fn has_exited(&self, boot_id: [u8; 16]) -> bool {
        if self.boot_id != boot_id {
            return true;
        }
        // Numeric PIDs and start times belong to their PID namespace. A host
        // collector must not interpret an inner namespace's PID as its own.
        if current_pid_namespace() != Some(self.pid_namespace) {
            return false;
        }
        match fs::read_to_string(format!("/proc/{}/stat", self.pid)) {
            Ok(stat) => parse_process_stat(&stat)
                .is_some_and(|identity| identity.start_time != self.start_time),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
    }
}

fn current_pid_namespace() -> Option<(u64, u64)> {
    namespace_identity("pid")
}

fn current_mount_namespace() -> Option<(u64, u64)> {
    namespace_identity("mnt")
}

fn process_mount_namespace(pid: u32) -> Option<(u64, u64)> {
    let metadata = fs::metadata(format!("/proc/{pid}/ns/mnt")).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

fn namespace_identity(kind: &str) -> Option<(u64, u64)> {
    let metadata = fs::metadata(format!("/proc/self/ns/{kind}")).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

struct ProcessStat<'a> {
    name: &'a str,
    parent_pid: u32,
    start_time: u64,
}

fn parse_process_stat(stat: &str) -> Option<ProcessStat<'_>> {
    let (_, after_open) = stat.split_once('(')?;
    // comm may contain spaces and closing parentheses.
    let (name, after_comm) = after_open.rsplit_once(')')?;
    let fields: Vec<_> = after_comm.split_whitespace().collect();
    Some(ProcessStat {
        name,
        parent_pid: fields.get(1)?.parse().ok()?,
        // The suffix starts at field 3 (state); starttime is field 22.
        start_time: fields.get(19)?.parse().ok()?,
    })
}

impl Store {
    /// Pin a response before dropping its scan lease or returning it to Git.
    pub(crate) fn retain_git_response(
        &mut self,
        baseline: &SnapshotBaseline,
        publisher: Option<&GitPublisher>,
    ) -> Result<(), String> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let (watch_id, snapshot_id) = validate_local_baseline(&transaction, baseline)?;
        let publisher = publisher
            .map(|publisher| {
                let mut publisher = publisher.clone();
                if let Some(path) = publisher
                    .index_path
                    .as_ref()
                    .filter(|_| current_mount_namespace() == Some(publisher.mount_namespace))
                {
                    let root: Vec<u8> = transaction
                        .query_row(
                            "SELECT live_path FROM watches WHERE id = ?1",
                            [watch_id.as_slice()],
                            |row| row.get(0),
                        )
                        .map_err(|error| error.to_string())?;
                    let root = PathBuf::from(std::ffi::OsString::from_vec(root));
                    let root = fs::canonicalize(&root).unwrap_or(root);
                    if let Ok(relative) = path.strip_prefix(root) {
                        if !relative.as_os_str().is_empty() {
                            publisher.index_path = Some(relative.to_owned());
                        }
                    }
                }
                Ok::<_, String>(publisher)
            })
            .transpose()?;
        let reason = publisher
            .as_ref()
            .map(GitPublisher::reason)
            .unwrap_or_else(|| {
                // Unknown publishers cannot be proven finished by a timeout.
                format!("{RESPONSE_REASON_PREFIX}unknown:{}", Uuid::new_v4())
            });
        insert_pin(
            &transaction,
            snapshot_id,
            retention_owner(watch_id),
            &reason,
        )?;
        transaction.commit().map_err(|error| error.to_string())
    }

    /// Protect all published Git cursors before either replay or physical GC.
    ///
    /// A read or validation failure rolls back the entire pass. The caller
    /// must skip collection rather than guessing which roots are safe to drop.
    pub(crate) fn refresh_git_index_roots(&mut self, boot_id: [u8; 16]) -> Result<(), String> {
        self.refresh_git_index_roots_with(
            |publisher| publisher.has_exited(boot_id),
            read_index_baseline,
        )
    }

    fn refresh_git_index_roots_with(
        &mut self,
        mut has_exited: impl FnMut(&GitPublisher) -> bool,
        mut read_index: impl FnMut(&Path) -> Result<Option<SnapshotBaseline>, String>,
    ) -> Result<(), String> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mount_namespace = current_mount_namespace();
        let watches = {
            let mut statement = transaction
                .prepare("SELECT id, live_path FROM watches WHERE state IN ('active', 'blocked')")
                .map_err(|error| error.to_string())?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        for (watch_id, root) in watches {
            let watch_id: [u8; 16] = watch_id
                .try_into()
                .map_err(|_| "Git retention watch ID is not a UUID".to_owned())?;
            let owner = retention_owner(watch_id);
            let root = PathBuf::from(std::ffi::OsString::from_vec(root));
            let candidates = {
                let mut statement = transaction
                    .prepare(
                        "SELECT snapshot_id, reason FROM snapshot_pins \
                         WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1",
                    )
                    .map_err(|error| error.to_string())?;
                statement
                    .query_map([owner.as_slice()], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|error| error.to_string())?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.to_string())?
            };
            // Check process completion first. If a publisher exits after this
            // check, its pin survives until the next pass. If it exited before,
            // the following index read observes any write it could finish.
            let mut completed = Vec::new();
            let mut alternate_indexes = BTreeSet::new();
            for (snapshot_id, reason) in candidates {
                if let Some(publisher) = GitPublisher::from_reason(&reason) {
                    if let Some(path) = publisher.index_path.as_ref() {
                        alternate_indexes.insert((publisher.mount_namespace, path.clone()));
                    }
                    if has_exited(&publisher) {
                        let index = publisher
                            .index_path
                            .map(|path| (publisher.mount_namespace, path));
                        completed.push((snapshot_id, reason, index));
                    }
                } else if reason.starts_with("git-index:") {
                    let index = index_from_reason(&reason).ok_or_else(|| {
                        "tracked Git index retention path is malformed; deferring collection"
                            .to_owned()
                    })?;
                    alternate_indexes.insert(index);
                }
            }
            // No publisher liveness checks occur after these index reads.
            let canonical = read_index(&root)?;
            if let Some(baseline) = canonical.as_ref() {
                replace_index_root(
                    &transaction,
                    watch_id,
                    owner,
                    CURRENT_REASON,
                    Some(baseline),
                )?;
            }
            let mut uncertain_external = BTreeSet::new();
            for (index_namespace, path) in &alternate_indexes {
                if mount_namespace != Some(*index_namespace) {
                    // ENOENT in another filesystem view is not evidence that
                    // the publishing view has removed its index.
                    continue;
                }
                let resolved = if path.is_absolute() {
                    path.clone()
                } else {
                    root.join(path)
                };
                let baseline = match open_index_file(&resolved)? {
                    Some(mut index) => read_open_index(&mut index)?,
                    None if path.is_absolute() => {
                        // An external file can have moved independently. Keep
                        // this path's roots without blocking unrelated cleanup.
                        uncertain_external.insert((*index_namespace, path.clone()));
                        continue;
                    }
                    None => None,
                };
                replace_index_root(
                    &transaction,
                    watch_id,
                    owner,
                    &index_reason(*index_namespace, path),
                    baseline.as_ref(),
                )?;
            }
            for (candidate_snapshot, reason, index) in completed {
                if index.as_ref().is_some_and(|(namespace, path)| {
                    mount_namespace != Some(*namespace)
                        || uncertain_external.contains(&(*namespace, path.clone()))
                }) {
                    continue;
                }
                if index.is_none() && canonical.is_none() {
                    // Missing canonical Git metadata or a bootstrap token
                    // proves no replacement for its existing roots.
                    continue;
                }
                // The current pin was installed first, including when it is
                // the same snapshot as this now-completed response.
                transaction
                    .execute(
                        "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' \
                         AND owner_id = ?1 AND snapshot_id = ?2 AND reason = ?3",
                        params![owner.as_slice(), candidate_snapshot, reason],
                    )
                    .map_err(|error| error.to_string())?;
            }
        }
        transaction.commit().map_err(|error| error.to_string())
    }
}

fn replace_index_root(
    connection: &Connection,
    watch_id: [u8; 16],
    owner: [u8; 16],
    reason: &str,
    baseline: Option<&SnapshotBaseline>,
) -> Result<(), String> {
    let snapshot_id = baseline
        .map(|baseline| {
            let (baseline_watch, snapshot_id) = validate_local_baseline(connection, baseline)?;
            if baseline_watch != watch_id {
                return Err("Git index cursor belongs to a different worktree watch".to_owned());
            }
            insert_pin(connection, snapshot_id, owner, reason)?;
            Ok(snapshot_id)
        })
        .transpose()?;
    connection
        .execute(
            "DELETE FROM snapshot_pins WHERE owner_kind = 'consumer-baseline' \
             AND owner_id = ?1 AND reason = ?2 AND (?3 IS NULL OR snapshot_id != ?3)",
            params![owner.as_slice(), reason, snapshot_id],
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn insert_pin(
    connection: &Connection,
    snapshot_id: i64,
    owner: [u8; 16],
    reason: &str,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT OR IGNORE INTO snapshot_pins(snapshot_id, owner_kind, owner_id, reason) \
             VALUES (?1, 'consumer-baseline', ?2, ?3)",
            params![snapshot_id, owner.as_slice(), reason],
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Retention is conservative: stale grants or epochs do not invalidate an
/// existing index's claim on a locally known snapshot.
fn validate_local_baseline(
    connection: &Connection,
    baseline: &SnapshotBaseline,
) -> Result<([u8; 16], i64), String> {
    let claims = decode_direct_scan_cursor(&baseline.continuity_token)
        .map_err(|error| format!("decode Git retention cursor: {error}"))?;
    if !baseline.identity.read_only
        || (!baseline.retention_token.is_empty() && baseline.retention_token != claims.watch_id)
        || baseline.identity.subvolume_uuid != claims.target_snapshot_uuid
    {
        return Err("Git retention cursor has inconsistent snapshot or watch ownership".to_owned());
    }
    let metadata: (Vec<u8>, u32) = connection
        .query_row(
            "SELECT store_uuid, clock_format_version FROM service_metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| error.to_string())?;
    if metadata.0 != claims.store_uuid || metadata.1 != claims.format_version {
        return Err(
            "Git index cursor belongs to a different AWACS manager or cursor format".to_owned(),
        );
    }
    let snapshot: Option<(i64, String)> = connection
        .query_row(
            "SELECT s.id, s.physical_state FROM watches w \
             JOIN filesystems f ON f.id = w.filesystem_id \
             JOIN snapshots s ON s.filesystem_id = f.id \
             WHERE w.id = ?1 AND w.state IN ('active', 'blocked') \
               AND f.fs_uuid = ?2 AND s.subvol_uuid = ?3 AND s.readonly = 1",
            params![
                claims.watch_id.as_slice(),
                baseline.identity.filesystem_uuid.as_slice(),
                claims.target_snapshot_uuid.as_slice(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    match snapshot {
        Some((id, state)) if state == "present" => Ok((claims.watch_id, id)),
        Some((_, state)) => Err(format!(
            "Git index still references AWACS snapshot {}, which is {state}; \
             restore that snapshot or explicitly rebuild its baseline before collecting history",
            Uuid::from_bytes(claims.target_snapshot_uuid),
        )),
        None => Err(format!(
            "Git index references snapshot {} that is absent from its local AWACS watch; \
             collection is paused until that reference is repaired",
            Uuid::from_bytes(claims.target_snapshot_uuid),
        )),
    }
}

fn read_index_baseline(root: &Path) -> Result<Option<SnapshotBaseline>, String> {
    let metadata = fs::metadata(root)
        .map_err(|error| format!("inspect AWACS watch root before Git retention: {error}"))?;
    if !metadata.is_dir() {
        return Err(
            "AWACS watch root is not a directory; snapshot collection is paused".to_owned(),
        );
    }
    let Some(git_dir) = git_admin_directory(root).map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    read_index_file(&git_dir.join("index"))
}

fn read_index_file(path: &Path) -> Result<Option<SnapshotBaseline>, String> {
    match open_index_file(path)? {
        Some(mut index) => read_open_index(&mut index),
        None => Err(
            "Git index is missing; snapshot collection is paused until its references can be read"
                .to_owned(),
        ),
    }
}

fn open_index_file(path: &Path) -> Result<Option<File>, String> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(index) => Ok(Some(index)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("open Git index for snapshot retention: {error}")),
    }
}

fn read_open_index(index: &mut File) -> Result<Option<SnapshotBaseline>, String> {
    if !index
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Git index retention metadata must be a regular file".to_owned());
    }
    let mut failures = Vec::new();
    // Discover the object-hash width from the framed index itself. This does
    // not run Git or depend on process-local Git configuration overrides.
    for hash_len in [20, 32] {
        match read_git_index_extension_bounded_from_file(index, hash_len, b"FSMN") {
            Ok(Some(extension)) => {
                return match inspect_fsmonitor_extension(&extension) {
                    GitFsmonitorStatus::Baseline(baseline) => Ok(Some(baseline)),
                    GitFsmonitorStatus::Missing | GitFsmonitorStatus::TimestampPlaceholder => {
                        Ok(None)
                    }
                    GitFsmonitorStatus::Invalid(error) => {
                        // Existing v1 outer tokens omit the lane owner. Their
                        // checked inner cursor still identifies the local watch.
                        let legacy = extension
                            .strip_prefix(&2_u32.to_be_bytes())
                            .and_then(|payload| {
                                payload
                                    .iter()
                                    .position(|byte| *byte == 0)
                                    .map(|end| &payload[..end])
                            })
                            .filter(|token| token.starts_with(b"awacs-git-v1:"))
                            .and_then(decode_git_fsmonitor_token);
                        legacy.map(Some).ok_or_else(|| {
                            format!("Git index has an invalid snapshot retention cursor: {error}")
                        })
                    }
                };
            }
            Ok(None) => return Ok(None),
            Err(error) => failures.push(error),
        }
    }
    Err(format!(
        "cannot safely read Git index snapshot retention metadata: {}",
        failures.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::{BoundaryKind, CursorClaims, encode_direct_scan_cursor};
    use crate::scan::SnapshotIdentity;
    use crate::status::{encode_git_fsmonitor_token, git_index_checksum};
    use crate::store::ServiceMetadata;
    use std::cell::Cell;
    use std::os::unix::ffi::OsStrExt;

    const WATCH: [u8; 16] = [2; 16];
    const BOOT: [u8; 16] = [12; 16];

    struct Fixture {
        directory: tempfile::TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let mut store = Store::create(
                &directory.path().join("manager.sqlite3"),
                &ServiceMetadata {
                    store_uuid: [10; 16],
                    clock_format_version: 1,
                    last_boot_id: BOOT,
                    created_ns: 1,
                },
            )
            .unwrap();
            store
                .connection_mut()
                .execute(
                    "INSERT INTO filesystems(id, fs_uuid) VALUES (1, ?1)",
                    [[1_u8; 16].as_slice()],
                )
                .unwrap();
            for sequence in 1_u8..=6 {
                store
                    .connection_mut()
                    .execute(
                        "INSERT INTO snapshots(id, filesystem_id, subvol_uuid, root_id, \
                         ctransid, otransid, path, readonly, physical_state, created_ns) \
                         VALUES (?1, 1, ?2, zeroblob(8), zeroblob(8), zeroblob(8), \
                         ?3, 1, 'present', 1)",
                        params![
                            i64::from(sequence),
                            [sequence + 20; 16].as_slice(),
                            directory
                                .path()
                                .join(format!("snapshot-{sequence}"))
                                .as_os_str()
                                .as_bytes(),
                        ],
                    )
                    .unwrap();
                store
                    .connection_mut()
                    .execute(
                        "INSERT INTO revisions(id, snapshot_id, state, builder_fence, created_ns) \
                         VALUES (?1, ?1, 'ready', 1, 1)",
                        [i64::from(sequence)],
                    )
                    .unwrap();
            }
            store
                .connection_mut()
                .execute(
                    "INSERT INTO watches(id, filesystem_id, live_subvol_uuid, live_path, \
                     indexed_revision_id, indexed_seq, last_cut_snapshot_id, last_cut_seq, \
                     clock_epoch, replay_floor_seq, fsmonitor_state, state) \
                     VALUES (?1, 1, ?2, ?3, 6, 6, 6, 6, ?4, 0, 'disabled', 'active')",
                    params![
                        WATCH.as_slice(),
                        [3_u8; 16].as_slice(),
                        directory.path().as_os_str().as_bytes(),
                        [4_u8; 16].as_slice()
                    ],
                )
                .unwrap();
            fs::create_dir(directory.path().join(".git")).unwrap();
            Self { directory, store }
        }

        fn baseline(&self, sequence: u8) -> SnapshotBaseline {
            baseline(sequence)
        }

        fn write_index(&self, baseline: Option<&SnapshotBaseline>, hash_len: usize) {
            fs::write(
                self.directory.path().join(".git/index"),
                index_bytes(baseline, hash_len),
            )
            .unwrap();
        }

        fn roots(&self) -> Vec<(i64, String)> {
            self.store
                .connection()
                .prepare(
                    "SELECT snapshot_id, reason FROM snapshot_pins \
                     WHERE owner_kind = 'consumer-baseline' AND owner_id = ?1 \
                     ORDER BY snapshot_id, reason",
                )
                .unwrap()
                .query_map([retention_owner(WATCH).as_slice()], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        }
    }

    fn baseline(sequence: u8) -> SnapshotBaseline {
        let snapshot_uuid = [sequence + 20; 16];
        SnapshotBaseline {
            identity: SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: snapshot_uuid,
                read_only: true,
            },
            continuity_token: encode_direct_scan_cursor(&CursorClaims {
                format_version: 1,
                store_uuid: [10; 16],
                watch_id: WATCH,
                cursor_epoch: [42; 16],
                cut_sequence: u64::from(sequence),
                owner_grant_id: [43; 16],
                monitor_session_id: [44; 16],
                boundary_kind: BoundaryKind::Cut,
                algorithm_version: 1,
                target_snapshot_uuid: snapshot_uuid,
            }),
            retention_token: WATCH.to_vec(),
        }
    }

    fn index_bytes(baseline: Option<&SnapshotBaseline>, hash_len: usize) -> Vec<u8> {
        let token = baseline.map(|baseline| encode_git_fsmonitor_token(baseline).unwrap());
        index_bytes_with_token(token.as_deref(), hash_len)
    }

    fn index_bytes_with_token(token: Option<&str>, hash_len: usize) -> Vec<u8> {
        let mut bytes = b"DIRC".to_vec();
        bytes.extend_from_slice(&4_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        let mut headers = Vec::new();
        if let Some(token) = token {
            let mut extension = 2_u32.to_be_bytes().to_vec();
            extension.extend_from_slice(token.as_bytes());
            extension.push(0);
            headers.extend_from_slice(b"FSMN");
            headers.extend_from_slice(&(extension.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&headers);
            bytes.extend_from_slice(&extension);
        }
        bytes.extend_from_slice(b"EOIE");
        bytes.extend_from_slice(&((4 + hash_len) as u32).to_be_bytes());
        bytes.extend_from_slice(&12_u32.to_be_bytes());
        bytes.extend_from_slice(&git_index_checksum(&headers, hash_len).unwrap());
        // index.skipHash is deployed and must not disable retention.
        bytes.resize(bytes.len() + hash_len, 0);
        bytes
    }

    fn publisher() -> GitPublisher {
        GitPublisher {
            boot_id: BOOT,
            pid: std::process::id(),
            start_time: parse_process_stat(&fs::read_to_string("/proc/self/stat").unwrap())
                .unwrap()
                .start_time,
            pid_namespace: current_pid_namespace().unwrap(),
            mount_namespace: current_mount_namespace().unwrap(),
            index_path: None,
        }
    }

    #[test]
    fn idle_published_index_is_pinned_without_observations_or_recent_queries() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&fixture.baseline(2)), 20);
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        assert_eq!(fixture.roots(), [(2, CURRENT_REASON.to_owned())]);
        let now_ns = 48 * 60 * 60 * 1_000_000_000_i64;
        let deletions = fixture
            .store
            .reserve_unpinned_snapshot_deletes([9; 16], now_ns, now_ns + 100, 16)
            .unwrap();
        assert!(deletions.iter().all(|deletion| deletion.snapshot_id != 2));
    }

    #[test]
    fn live_responses_survive_out_of_order_index_publication() {
        let mut fixture = Fixture::new();
        let publisher = publisher();
        fixture.write_index(Some(&fixture.baseline(2)), 20);
        for sequence in [3, 5, 4] {
            fixture
                .store
                .retain_git_response(&baseline(sequence), Some(&publisher))
                .unwrap();
        }
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        fixture.write_index(Some(&fixture.baseline(5)), 20);
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        fixture.write_index(Some(&fixture.baseline(3)), 20);
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        assert_eq!(
            fixture
                .roots()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            [3, 3, 4, 5],
        );
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        assert_eq!(fixture.roots(), [(3, CURRENT_REASON.to_owned())]);
    }

    #[test]
    fn completed_publisher_is_checked_before_its_published_index_is_read() {
        let mut fixture = Fixture::new();
        fixture
            .store
            .retain_git_response(&baseline(4), Some(&publisher()))
            .unwrap();
        let completed = Cell::new(false);
        fixture
            .store
            .refresh_git_index_roots_with(
                |_| {
                    completed.set(true);
                    true
                },
                |_| {
                    assert!(completed.get());
                    Ok(Some(baseline(4)))
                },
            )
            .unwrap();
        assert_eq!(fixture.roots(), [(4, CURRENT_REASON.to_owned())]);
    }

    #[test]
    fn publisher_exiting_during_reconciliation_keeps_its_response_until_next_pass() {
        use std::sync::{Arc, Barrier};

        let mut fixture = Fixture::new();
        fixture.write_index(Some(&baseline(2)), 20);
        fixture
            .store
            .retain_git_response(&baseline(4), Some(&publisher()))
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let publishing_barrier = Arc::clone(&barrier);
        let git_dir = fixture.directory.path().join(".git");
        let writing = std::thread::spawn(move || {
            publishing_barrier.wait();
            let replacement = git_dir.join("replacement");
            fs::write(&replacement, index_bytes(Some(&baseline(4)), 20)).unwrap();
            fs::rename(replacement, git_dir.join("index")).unwrap();
            publishing_barrier.wait();
        });
        fixture
            .store
            .refresh_git_index_roots_with(
                |_| {
                    // It was alive when checked, then published and exited
                    // while collection was preparing to read the index.
                    barrier.wait();
                    barrier.wait();
                    false
                },
                read_index_baseline,
            )
            .unwrap();
        writing.join().unwrap();
        assert_eq!(fixture.roots().len(), 2);
        assert!(fixture.roots().iter().all(|(id, _)| *id == 4));
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        assert_eq!(fixture.roots(), [(4, CURRENT_REASON.to_owned())]);
    }

    #[test]
    fn unknown_publishers_and_bootstrap_indexes_preserve_existing_pins() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&fixture.baseline(2)), 20);
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        fixture
            .store
            .retain_git_response(&baseline(3), None)
            .unwrap();
        fixture
            .store
            .retain_git_response(&baseline(4), Some(&publisher()))
            .unwrap();
        let before = fixture.roots();
        fixture.write_index(None, 20);
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        assert_eq!(fixture.roots(), before);
        fixture.write_index(Some(&fixture.baseline(2)), 20);
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        let roots = fixture.roots();
        assert_eq!(roots.len(), 2);
        assert!(roots[1].1.starts_with("git-response:unknown:"));
    }

    #[test]
    fn explicit_alternate_index_retains_its_current_snapshot_and_releases_removed_files() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&baseline(2)), 20);
        let index_path = fixture.directory.path().join("alternate-index");
        let mut alternate_publisher = publisher();
        alternate_publisher.index_path = Some(index_path.clone());
        fixture
            .store
            .retain_git_response(&baseline(4), Some(&alternate_publisher))
            .unwrap();
        fs::write(&index_path, index_bytes(Some(&baseline(4)), 20)).unwrap();
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        assert_eq!(
            fixture.roots(),
            [
                (2, CURRENT_REASON.to_owned()),
                (
                    4,
                    index_reason(
                        alternate_publisher.mount_namespace,
                        Path::new("alternate-index")
                    )
                )
            ]
        );
        fs::write(&index_path, index_bytes(Some(&baseline(5)), 20)).unwrap();
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        assert_eq!(
            fixture.roots(),
            [
                (2, CURRENT_REASON.to_owned()),
                (
                    5,
                    index_reason(
                        alternate_publisher.mount_namespace,
                        Path::new("alternate-index")
                    )
                )
            ]
        );
        fs::remove_file(index_path).unwrap();
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        assert_eq!(fixture.roots(), [(2, CURRENT_REASON.to_owned())]);
    }

    #[test]
    fn repeated_temporary_indexes_do_not_accumulate_permanent_response_pins() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&baseline(2)), 20);
        for index in 0..12 {
            let path = fixture
                .directory
                .path()
                .join(format!("temporary-index-{index}"));
            let mut alternate_publisher = publisher();
            alternate_publisher.index_path = Some(path.clone());
            fixture
                .store
                .retain_git_response(&baseline(4), Some(&alternate_publisher))
                .unwrap();
            fs::write(&path, index_bytes(Some(&baseline(4)), 20)).unwrap();
            fixture
                .store
                .refresh_git_index_roots_with(|_| true, read_index_baseline)
                .unwrap();
            assert_eq!(fixture.roots().len(), 2);
            fs::remove_file(path).unwrap();
            fixture
                .store
                .refresh_git_index_roots_with(|_| true, read_index_baseline)
                .unwrap();
            assert_eq!(fixture.roots(), [(2, CURRENT_REASON.to_owned())]);
        }
    }

    #[test]
    fn alternate_index_errors_keep_roots_and_live_publishers_keep_missing_index_candidates() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&baseline(2)), 20);
        let path = fixture.directory.path().join("alternate-index");
        let mut alternate_publisher = publisher();
        alternate_publisher.index_path = Some(path.clone());
        fixture
            .store
            .retain_git_response(&baseline(4), Some(&alternate_publisher))
            .unwrap();
        fs::write(&path, b"corrupt").unwrap();
        let before = fixture.roots();
        assert!(fixture.store.refresh_git_index_roots(BOOT).is_err());
        assert_eq!(fixture.roots(), before);
        fs::remove_file(path).unwrap();
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        assert!(
            fixture
                .roots()
                .iter()
                .any(|(id, reason)| *id == 4 && reason.starts_with(RESPONSE_REASON_PREFIX))
        );
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        assert_eq!(fixture.roots(), [(2, CURRENT_REASON.to_owned())]);
    }

    #[test]
    fn another_mount_view_keeps_alternate_roots_even_after_the_publisher_exits() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&baseline(2)), 20);
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        let mut other_view = publisher();
        other_view.mount_namespace.1 = other_view.mount_namespace.1.wrapping_add(1);
        other_view.index_path = Some(fixture.directory.path().join("alternate-index"));
        fixture
            .store
            .retain_git_response(&baseline(4), Some(&other_view))
            .unwrap();
        insert_pin(
            fixture.store.connection(),
            4,
            retention_owner(WATCH),
            &index_reason(other_view.mount_namespace, Path::new("alternate-index")),
        )
        .unwrap();
        let before = fixture.roots();
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        assert_eq!(fixture.roots(), before);
    }

    #[test]
    fn moving_the_checkout_preserves_relative_alternate_index_roots() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&baseline(2)), 20);
        let mut alternate_publisher = publisher();
        alternate_publisher.index_path = Some(fixture.directory.path().join("alternate-index"));
        fixture
            .store
            .retain_git_response(&baseline(4), Some(&alternate_publisher))
            .unwrap();
        fs::write(
            alternate_publisher.index_path.as_ref().unwrap(),
            index_bytes(Some(&baseline(4)), 20),
        )
        .unwrap();
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        let before = fixture.roots();
        let destination_parent = tempfile::tempdir().unwrap();
        let destination = destination_parent.path().join("relocated-checkout");
        fs::rename(fixture.directory.path(), &destination).unwrap();
        fixture
            .store
            .connection_mut()
            .execute(
                "UPDATE watches SET live_path = ?1 WHERE id = ?2",
                params![destination.as_os_str().as_bytes(), WATCH.as_slice()],
            )
            .unwrap();
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        assert_eq!(fixture.roots(), before);
        assert!(fixture.roots().iter().any(|(id, _)| *id == 4));
    }

    #[test]
    fn a_missing_external_index_keeps_only_its_roots_without_blocking_other_collection() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&baseline(2)), 20);
        let external = tempfile::tempdir().unwrap();
        let path = external.path().join("index");
        let mut external_publisher = publisher();
        external_publisher.index_path = Some(path.clone());
        fixture
            .store
            .retain_git_response(&baseline(4), Some(&external_publisher))
            .unwrap();
        fs::write(&path, index_bytes(Some(&baseline(4)), 20)).unwrap();
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        fixture
            .store
            .retain_git_response(&baseline(5), Some(&external_publisher))
            .unwrap();
        fs::remove_file(path).unwrap();
        let before = fixture.roots();
        fixture
            .store
            .refresh_git_index_roots_with(|_| true, read_index_baseline)
            .unwrap();
        assert_eq!(fixture.roots(), before);
        let deletions = fixture
            .store
            .reserve_unpinned_snapshot_deletes([9; 16], 100, 200, 16)
            .unwrap();
        assert!(deletions.iter().any(|deletion| deletion.snapshot_id == 1));
        assert!(
            deletions
                .iter()
                .all(|deletion| ![2, 4, 5].contains(&deletion.snapshot_id))
        );
    }

    #[test]
    fn explicit_index_environment_keeps_a_reclaimable_publisher_identity() {
        const CHILD: &str = "AWACS_GIT_PUBLISHER_TEST_CHILD";
        const TEST: &str = "git_retention::tests::explicit_index_environment_keeps_a_reclaimable_publisher_identity";
        let mode = std::env::var(CHILD).ok();
        if mode.as_deref() == Some("hook") {
            let publisher =
                capture_git_publisher().expect("capture the hook caller's process identity");
            assert_eq!(
                Some(publisher.mount_namespace),
                process_mount_namespace(u32::try_from(unsafe { libc::getppid() }).unwrap()),
            );
            assert_eq!(
                publisher.index_path,
                Some(PathBuf::from(std::env::var_os("GIT_INDEX_FILE").unwrap()))
            );
            return;
        }
        if mode.as_deref() == Some("git") {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "hook")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let git = directory.path().join("git");
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &git).unwrap();
        let output = std::process::Command::new(git)
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "git")
            .env("GIT_INDEX_FILE", directory.path().join("alternate-index"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn alternate_index_aliases_share_one_durable_root_identity() {
        let directory = tempfile::tempdir().unwrap();
        let index = directory.path().join("index");
        fs::write(&index, b"index").unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(directory.path(), &alias).unwrap();
        assert_eq!(
            normalize_index_path(&alias.join("index")),
            normalize_index_path(&index)
        );
        assert_eq!(
            normalize_index_path(&alias.join("not-created-yet")),
            directory.path().join("not-created-yet")
        );
    }

    #[test]
    fn a_fifo_index_is_rejected_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("index-fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let error = read_index_file(&fifo).unwrap_err();
        assert!(error.contains("regular file"), "{error}");
    }

    #[test]
    fn invalid_or_deleted_current_snapshot_does_not_remove_any_roots() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&fixture.baseline(2)), 20);
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        fixture
            .store
            .retain_git_response(&baseline(3), Some(&publisher()))
            .unwrap();
        let before = fixture.roots();
        fs::write(fixture.directory.path().join(".git/index"), b"corrupt").unwrap();
        assert!(fixture.store.refresh_git_index_roots(BOOT).is_err());
        assert_eq!(fixture.roots(), before);
        fixture.write_index(Some(&fixture.baseline(4)), 20);
        fixture
            .store
            .connection_mut()
            .execute(
                "UPDATE snapshots SET physical_state = 'deleted' WHERE id = 4",
                [],
            )
            .unwrap();
        let error = fixture.store.refresh_git_index_roots(BOOT).unwrap_err();
        assert!(error.contains("which is deleted"), "{error}");
        assert_eq!(fixture.roots(), before);
    }

    #[test]
    fn missing_index_and_malformed_gitfile_preserve_current_and_response_roots() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&baseline(2)), 20);
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        fixture
            .store
            .retain_git_response(&baseline(3), Some(&publisher()))
            .unwrap();
        let before = fixture.roots();
        fs::remove_file(fixture.directory.path().join(".git/index")).unwrap();
        assert!(fixture.store.refresh_git_index_roots(BOOT).is_err());
        assert_eq!(fixture.roots(), before);
        fs::remove_dir(fixture.directory.path().join(".git")).unwrap();
        fs::write(fixture.directory.path().join(".git"), b"not a gitfile").unwrap();
        assert!(fixture.store.refresh_git_index_roots(BOOT).is_err());
        assert_eq!(fixture.roots(), before);
    }

    #[test]
    fn unavailable_watch_root_defers_cleanup_instead_of_looking_like_a_non_git_root() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&baseline(2)), 20);
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        fixture
            .store
            .retain_git_response(&baseline(3), Some(&publisher()))
            .unwrap();
        let before = fixture.roots();
        let root = fixture.directory.path().join("unavailable-root");
        fixture
            .store
            .connection_mut()
            .execute(
                "UPDATE watches SET live_path = ?1 WHERE id = ?2",
                params![root.as_os_str().as_bytes(), WATCH.as_slice()],
            )
            .unwrap();
        assert!(fixture.store.refresh_git_index_roots(BOOT).is_err());
        assert_eq!(fixture.roots(), before);
        fs::write(root, b"not a directory").unwrap();
        assert!(fixture.store.refresh_git_index_roots(BOOT).is_err());
        assert_eq!(fixture.roots(), before);
    }

    #[test]
    fn linked_worktree_sha256_index_and_stale_authorization_are_retained() {
        let mut fixture = Fixture::new();
        let administrative = fixture.directory.path().join("administrative");
        fs::rename(fixture.directory.path().join(".git"), &administrative).unwrap();
        fs::write(
            fixture.directory.path().join(".git"),
            b"gitdir: administrative\n",
        )
        .unwrap();
        fs::write(
            administrative.join("index"),
            index_bytes(Some(&baseline(2)), 32),
        )
        .unwrap();
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        assert_eq!(fixture.roots(), [(2, CURRENT_REASON.to_owned())]);
    }

    #[test]
    fn legacy_index_without_outer_lane_owner_recovers_watch_from_its_cursor() {
        let mut fixture = Fixture::new();
        let baseline = baseline(2);
        let legacy_token = format!(
            "awacs-git-v1:{}:{}:{}",
            Uuid::from_bytes(baseline.identity.filesystem_uuid),
            Uuid::from_bytes(baseline.identity.subvolume_uuid),
            std::str::from_utf8(&baseline.continuity_token).unwrap(),
        );
        fs::write(
            fixture.directory.path().join(".git/index"),
            index_bytes_with_token(Some(&legacy_token), 20),
        )
        .unwrap();
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        assert_eq!(fixture.roots(), [(2, CURRENT_REASON.to_owned())]);
    }

    #[test]
    fn reserved_owner_survives_legacy_git_consumer_release() {
        let mut fixture = Fixture::new();
        fixture.write_index(Some(&fixture.baseline(2)), 20);
        fixture.store.refresh_git_index_roots(BOOT).unwrap();
        fixture.store.release_consumer_baseline(WATCH).unwrap();
        assert_eq!(fixture.roots(), [(2, CURRENT_REASON.to_owned())]);
        assert_ne!(retention_owner(WATCH), WATCH);
    }

    #[test]
    fn process_identity_handles_comm_delimiters_pid_reuse_and_previous_boot() {
        let mut fields = vec!["S".to_owned(), "42".to_owned()];
        fields.resize(19, "0".to_owned());
        fields.push("12345".to_owned());
        let stat = format!("7 (name ) with spaces) {}", fields.join(" "));
        let identity = parse_process_stat(&stat).unwrap();
        assert_eq!(identity.name, "name ) with spaces");
        assert_eq!(identity.parent_pid, 42);
        assert_eq!(identity.start_time, 12345);
        assert!(parse_process_stat("7 (git) broken").is_none());
        let current = publisher();
        assert!(!current.has_exited(BOOT));
        let mut reused = current.clone();
        reused.start_time += 1;
        assert!(reused.has_exited(BOOT));
        assert!(current.has_exited([99; 16]));
        let mut other_namespace = current.clone();
        other_namespace.pid_namespace.1 = other_namespace.pid_namespace.1.wrapping_add(1);
        assert!(!other_namespace.has_exited(BOOT));
        assert!(other_namespace.has_exited([99; 16]));
        assert_eq!(GitPublisher::from_reason(&current.reason()), Some(current));
        assert!(GitPublisher::from_reason("git-response:unknown:x").is_none());
    }
}
