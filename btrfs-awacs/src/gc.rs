//! Repository-independent collection of UUID-keyed AWACS state.
//!
//! The state directory, rather than a checkout path, is the discovery root.
//! A missing pathname never proves deletion: only the broker's filesystem
//! identity lookup can authorize retiring a store.

use crate::bootstrap::{RootInitPaths, external_service_config, read_boot_id};
use crate::broker::{ExpectedSubvolume, SnapshotTrashExecution, SubvolumeExistsExecution};
use crate::broker_protocol::BrokerClient;
use crate::btrfs::OpenedSubvolume;
use crate::git_retention::GitPublisher;
use crate::service::Service;
use crate::store::Store;
use rusqlite::Connection;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const RETIRED_LOCK: &[u8] = b"awacs-retired-v1\n";
const SNAPSHOT_LIMIT: usize = 256;

#[derive(Debug)]
pub struct GcError(String);

impl fmt::Display for GcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GcError {}

impl From<io::Error> for GcError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<rusqlite::Error> for GcError {
    fn from(error: rusqlite::Error) -> Self {
        Self(error.to_string())
    }
}

fn error(error: impl fmt::Display) -> GcError {
    GcError(error.to_string())
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub dry_run: bool,
    pub entries: Vec<GcEntry>,
}

#[derive(Debug)]
pub struct GcEntry {
    pub path: PathBuf,
    pub action: &'static str,
    pub detail: String,
    pub snapshots: usize,
}

impl GcReport {
    fn record(
        &mut self,
        path: &Path,
        action: &'static str,
        detail: impl Into<String>,
        snapshots: usize,
    ) {
        self.entries.push(GcEntry {
            path: path.to_owned(),
            action,
            detail: detail.into(),
            snapshots,
        });
    }
}

/// Fences holders of this exact lock inode, even after its directory is removed.
/// The caller must hold its exclusive flock. Never clear a retired lock inode;
/// an explicit rebuild creates a new lock after retiring the previous store.
pub(crate) fn mark_retired_lock(lock: &File) -> Result<(), GcError> {
    lock.write_all_at(RETIRED_LOCK, 0)?;
    lock.set_len(RETIRED_LOCK.len() as u64)?;
    lock.sync_all()?;
    Ok(())
}

/// Must run after acquiring flock, including on already-open direct clients.
pub(crate) fn ensure_current_lock(lock: &File, lock_path: &Path) -> Result<(), GcError> {
    if lock.metadata()?.len() != 0 {
        return Err(error(
            "AWACS state was retired; reopen the initialized root",
        ));
    }
    let current = fs::symlink_metadata(lock_path)?;
    let held = lock.metadata()?;
    if !current.is_file() || current.dev() != held.dev() || current.ino() != held.ino() {
        return Err(error(
            "AWACS state was replaced; reopen the initialized root",
        ));
    }
    Ok(())
}

struct StateLock(File);

impl StateLock {
    fn acquire(path: &Path, dry_run: bool) -> Result<Self, GcError> {
        let file = OpenOptions::new()
            .read(true)
            .write(!dry_run)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(error("root.lock must be a private owned regular file"));
        }
        // Nonblocking: an active checkout must not stall a global sweep.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(error(format!(
                "root is busy: {}",
                io::Error::last_os_error()
            )));
        }
        let lock = Self(file);
        let named = fs::symlink_metadata(path)?;
        if !named.is_file() || named.dev() != metadata.dev() || named.ino() != metadata.ino() {
            return Err(error("root.lock changed while acquiring it"));
        }
        Ok(lock)
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn private_directory(path: &Path) -> Result<File, GcError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(error(format!(
            "{} must be a private directory owned by this user",
            path.display()
        )));
    }
    Ok(file)
}

/// A real SQLite read transaction supplies one coherent view across WAL
/// checkpoints, including checkpoints from another client's destructor outside
/// root.lock. SQLite may maintain its -shm coordination file even in dry-run;
/// no manager records, snapshots, or baseline state are modified here.
struct Inspection {
    connection: Connection,
}

impl Inspection {
    fn open(database: &Path) -> Result<Self, GcError> {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = database.as_os_str().to_owned();
            path.push(suffix);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => return Err(error("manager database sidecars must be regular files")),
                Err(e) if !suffix.is_empty() && e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(error(e)),
            }
        }
        let connection =
            Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        connection
            .execute_batch("PRAGMA query_only = ON; PRAGMA trusted_schema = OFF; BEGIN DEFERRED")?;
        let application: i64 =
            connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if application != 0x4241_5731 || version != 14 {
            return Err(error(
                "unsupported manager database; rebuild its baseline before collection",
            ));
        }
        Ok(Self { connection })
    }
}

/// Durable baseline pins alone do not keep a deleted checkout alive. In-flight
/// scans, explicit retention, and publishers do: a retired store cannot cancel
/// their access merely because its original working directory disappeared.
pub(crate) fn retirement_blocker(
    connection: &Connection,
    boot_id: [u8; 16],
    now_ns: i64,
) -> Result<Option<String>, GcError> {
    let previous_boot: Vec<u8> = connection.query_row(
        "SELECT last_boot_id FROM service_metadata WHERE singleton = 1",
        [],
        |r| r.get(0),
    )?;
    if previous_boot.len() != 16 {
        return Err(error("invalid manager boot identity"));
    }
    let same_boot = previous_boot == boot_id;
    let checks = [
        (
            "active query lease",
            "SELECT EXISTS(SELECT 1 FROM query_leases WHERE state = 'active' AND lease_expires_ns > ?1)",
            same_boot,
        ),
        (
            "active retention lease",
            "SELECT EXISTS(SELECT 1 FROM retention_leases WHERE state = 'active' AND expires_ns > ?1)",
            true,
        ),
        (
            "active snapshot operation",
            "SELECT EXISTS(SELECT 1 FROM operations WHERE state NOT IN ('done','failed') AND (lease_expires_ns IS NULL OR lease_expires_ns > ?1))",
            same_boot,
        ),
        (
            "active comparison",
            "SELECT EXISTS(SELECT 1 FROM comparisons WHERE state = 'claimed' AND (lease_expires_ns IS NULL OR lease_expires_ns > ?1))",
            same_boot,
        ),
        (
            "active deletion",
            "SELECT EXISTS(SELECT 1 FROM snapshot_delete_operations WHERE state NOT IN ('done','failed') AND lease_expires_ns > ?1)",
            same_boot,
        ),
    ];
    for (reason, sql, relevant) in checks {
        if relevant && connection.query_row(sql, [now_ns], |r| r.get::<_, bool>(0))? {
            return Ok(Some(reason.to_owned()));
        }
    }
    let mut statement = connection
        .prepare("SELECT reason FROM snapshot_pins WHERE owner_kind = 'consumer-baseline'")?;
    for reason in statement.query_map([], |r| r.get::<_, String>(0))? {
        let reason = reason?;
        if let Some(publisher) = GitPublisher::from_reason(&reason) {
            if !publisher.has_exited(boot_id) {
                return Ok(Some(
                    "a Git publisher is still alive or its process namespace is unavailable"
                        .to_owned(),
                ));
            }
        } else if reason.starts_with("git-response") {
            return Ok(Some(
                "unrecognized Git publisher retention record".to_owned(),
            ));
        }
    }
    Ok(None)
}

struct RootIdentity {
    filesystem: [u8; 16],
    subvolume: [u8; 16],
    path: PathBuf,
}

fn identity(connection: &Connection, expected: Uuid) -> Result<RootIdentity, GcError> {
    let mut statement = connection.prepare("SELECT f.fs_uuid, w.live_subvol_uuid, w.live_path FROM watches w JOIN filesystems f ON f.id = w.filesystem_id")?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() != 1 {
        return Err(error("expected exactly one watch in a root store"));
    }
    let (filesystem, subvolume, path) = rows.into_iter().next().unwrap();
    let filesystem = filesystem
        .try_into()
        .map_err(|_| error("invalid filesystem UUID"))?;
    let subvolume = subvolume
        .try_into()
        .map_err(|_| error("invalid subvolume UUID"))?;
    if subvolume != *expected.as_bytes() {
        return Err(error("store directory and watch UUID disagree"));
    }
    Ok(RootIdentity {
        filesystem,
        subvolume,
        path: PathBuf::from(OsString::from_vec(path)),
    })
}

fn broker_client() -> Result<BrokerClient, GcError> {
    let directory = std::env::var_os("BTRFS_AWACS_BROKER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(crate::broker_transport::DEFAULT_BROKER_DIRECTORY));
    BrokerClient::connect_endpoint(&directory, *Uuid::new_v4().as_bytes()).map_err(error)
}

/// Collect all recognized stores and broker trash directly below this root.
/// Errors concerning an individual store are reported as deferred entries;
/// missing filesystems, moved roots and malformed state are never deleted.
pub fn collect(state_root: &Path, dry_run: bool) -> Result<GcReport, GcError> {
    if !state_root.is_absolute() {
        return Err(error("AWACS state root must be absolute"));
    }
    private_directory(state_root)?;
    let boot_id = read_boot_id().map_err(error)?;
    let now_ns = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(error)?
            .as_nanos(),
    )
    .map_err(error)?;
    let mut report = GcReport {
        dry_run,
        entries: Vec::new(),
    };
    scan_parent(state_root, true, boot_id, now_ns, &mut report)?;
    Ok(report)
}

fn scan_parent(
    parent: &Path,
    allow_retired: bool,
    boot_id: [u8; 16],
    now_ns: i64,
    report: &mut GcReport,
) -> Result<(), GcError> {
    let parent_fd = private_directory(parent)?;
    let mut entries = fs::read_dir(parent)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let result = (|| {
            if let Ok(uuid) = Uuid::parse_str(name) {
                collect_store(
                    &path,
                    uuid,
                    boot_id,
                    now_ns,
                    report,
                    &mut |root, directory| {
                        broker_client()?
                            .subvolume_exists(
                                &SubvolumeExistsExecution {
                                    filesystem_uuid: root.filesystem,
                                    subvolume_uuid: root.subvolume,
                                },
                                directory.as_fd(),
                            )
                            .map_err(error)
                    },
                )
            } else if valid_trash_name(name) {
                if !entry.file_type()?.is_dir() {
                    Err(error("broker trash entry is not a directory"))
                } else if report.dry_run {
                    report.record(
                        &path,
                        "would-drain-trash",
                        "retry broker-owned pending deletion",
                        0,
                    );
                    Ok(())
                } else {
                    broker_client()?
                        .drain_snapshot_trash(name.as_bytes(), parent_fd.as_fd())
                        .map_err(error)?;
                    report.record(
                        &path,
                        "drain-trash",
                        "queued asynchronous broker deletion",
                        0,
                    );
                    Ok(())
                }
            } else if allow_retired
                && name
                    .strip_prefix(".retired-worktree-")
                    .is_some_and(|suffix| suffix.len() == 32 && Uuid::parse_str(suffix).is_ok())
            {
                scan_parent(&path, false, boot_id, now_ns, report)
            } else {
                Ok(())
            }
        })();
        if let Err(e) = result {
            report.record(&path, "deferred", e.to_string(), 0);
        }
    }
    Ok(())
}

fn valid_trash_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix(".broker-trash-") else {
        return false;
    };
    if !suffix.is_ascii() {
        return false;
    }
    Uuid::parse_str(suffix).is_ok()
        || (suffix.len() == 73
            && Uuid::parse_str(&suffix[..36]).is_ok()
            && suffix.as_bytes()[36] == b'-'
            && Uuid::parse_str(&suffix[37..]).is_ok())
}

fn collect_store(
    directory: &Path,
    uuid: Uuid,
    boot_id: [u8; 16],
    now_ns: i64,
    report: &mut GcReport,
    probe: &mut dyn FnMut(&RootIdentity, &File) -> Result<bool, GcError>,
) -> Result<(), GcError> {
    private_directory(directory)?;
    // The last two cleanup steps unlink root.lock and remove this directory.
    // A crash between them leaves no lock to reopen. rmdir is atomic and
    // refuses a bundle that an initializer has made nonempty in the meantime.
    if fs::read_dir(directory)?.next().transpose()?.is_none() {
        if !report.dry_run {
            fs::remove_dir(directory)?;
            private_directory(
                directory
                    .parent()
                    .ok_or_else(|| error("store has no parent"))?,
            )?
            .sync_all()?;
        }
        report.record(
            directory,
            if report.dry_run {
                "would-retire"
            } else {
                "retired"
            },
            "removed empty state left by interrupted cleanup",
            0,
        );
        return Ok(());
    }
    let lock = StateLock::acquire(&directory.join("root.lock"), report.dry_run)?;
    let paths = RootInitPaths::for_state_directory(directory).map_err(error)?;
    // A crash can interrupt metadata removal after all snapshots were queued.
    // The retired inode is durable authorization to finish only that cleanup;
    // without the DB we never retire any remaining physical snapshots.
    if !paths.manager_db.try_exists()? && lock_is_retired(&lock.0)? {
        let plan = cleanup_plan(directory, &paths)?;
        if !plan.wrappers.is_empty() {
            return Err(error(
                "retired state lost its database while snapshots remain",
            ));
        }
        if !report.dry_run {
            remove_metadata(directory, plan)?;
        }
        report.record(
            directory,
            if report.dry_run {
                "would-retire"
            } else {
                "retired"
            },
            "finished interrupted state cleanup",
            0,
        );
        return Ok(());
    }
    let inspection = Inspection::open(&paths.manager_db)?;
    let root = identity(&inspection.connection, uuid)?;
    let state_parent = private_directory(
        directory
            .parent()
            .ok_or_else(|| error("store has no parent"))?,
    )?;
    let exists = probe(&root, &state_parent)?;
    if exists {
        ensure_current_lock(&lock.0, &directory.join("root.lock"))?;
        let source = OpenedSubvolume::open(&root.path).map_err(|e| {
            error(format!(
                "source still exists but its recorded path is unavailable: {e}"
            ))
        })?;
        if source.filesystem.fs_uuid != root.filesystem || source.subvolume.uuid != root.subvolume {
            return Err(error(
                "source still exists at another path; preserve its state until it is reopened",
            ));
        }
        if report.dry_run {
            report.record(
                directory,
                "live",
                "source exists; ordinary GC preserves its durable pins and leases",
                0,
            );
        } else {
            validate_storage_paths(&inspection.connection, &paths)?;
            // Validate directory ownership and the complete managed wrapper
            // layout before normal recovery may access stored paths.
            cleanup_plan(directory, &paths)?;
            let store = Store::open(&paths.manager_db).map_err(error)?;
            let config = external_service_config(
                paths.managed_dir,
                paths.snapshot_trash_dir,
                paths.spool_dir,
                boot_id,
            );
            let mut service = Service::new_external(store, config).map_err(error)?;
            let count = service
                .garbage_collect(now_ns, SNAPSHOT_LIMIT)
                .map_err(error)?;
            report.record(
                directory,
                "live",
                "ran bounded ordinary GC; retained live roots",
                count,
            );
        }
        return Ok(());
    }
    // The database may be the only remaining record of a split managed store
    // or of artifacts whose bundle moved without rewriting absolute paths.
    // Preserve that record until every active artifact is accounted for here.
    validate_storage_paths(&inspection.connection, &paths)?;
    if let Some(reason) = retirement_blocker(&inspection.connection, boot_id, now_ns)? {
        return Err(error(format!("source was deleted, but {reason}")));
    }
    let plan = cleanup_plan(directory, &paths)?;
    if report.dry_run {
        report.record(
            directory,
            "would-retire",
            "source identity is absent; snapshots and state can be collected",
            plan.wrappers.len(),
        );
        return Ok(());
    }
    // The durable flag is written before any physical effect. Cached clients
    // must observe it after flock, even if this bundle is later unlinked.
    mark_retired_lock(&lock.0)?;
    let broker = broker_client()?;
    for wrapper in &plan.wrappers {
        let parent = private_directory(wrapper.parent().unwrap())?;
        let snapshot = OpenedSubvolume::open(&wrapper.join("snapshot")).map_err(error)?;
        broker
            .trash_snapshot(
                &SnapshotTrashExecution {
                    target: ExpectedSubvolume::from_observed(
                        &snapshot.filesystem,
                        &snapshot.subvolume,
                    ),
                    entry_name: wrapper.file_name().unwrap().as_bytes().to_vec(),
                    trash_name: paths
                        .snapshot_trash_dir
                        .file_name()
                        .unwrap()
                        .as_bytes()
                        .to_vec(),
                },
                parent.as_fd(),
                state_parent.as_fd(),
                snapshot.as_fd(),
            )
            .map_err(error)?;
    }
    let count = plan.wrappers.len();
    drop(inspection);
    remove_metadata(directory, plan)?;
    state_parent.sync_all()?;
    report.record(
        directory,
        "retired",
        "queued snapshots for broker deletion and removed retired state",
        count,
    );
    Ok(())
}

fn lock_is_retired(lock: &File) -> Result<bool, GcError> {
    if lock.metadata()?.len() != RETIRED_LOCK.len() as u64 {
        return Ok(false);
    }
    let mut bytes = [0; 17];
    let len = lock.read_at(&mut bytes, 0)?;
    Ok(&bytes[..len] == RETIRED_LOCK)
}

fn remove_metadata(directory: &Path, mut plan: CleanupPlan) -> Result<(), GcError> {
    // Keep the complete database until snapshots and payloads are gone. Then
    // remove its main file before the WAL: deleting the WAL first can leave a
    // database without its schema after a crash, which cannot be inspected or
    // recognized as the missing-database recovery state.
    plan.files
        .sort_by_key(|path| path.file_name() != Some(OsStr::new("manager.sqlite3")));
    let (metadata, payloads): (Vec<_>, Vec<_>) = plan
        .files
        .into_iter()
        .partition(|path| path.parent() == Some(directory));
    for path in payloads {
        fs::remove_file(path)?;
    }
    for path in plan.directories {
        fs::remove_dir(path)?;
    }
    for path in metadata {
        fs::remove_file(&path)?;
        if path.file_name() == Some(OsStr::new("manager.sqlite3")) {
            // Persist this recovery boundary before removing either sidecar.
            private_directory(directory)?.sync_all()?;
        }
    }
    fs::remove_file(directory.join("root.lock"))?;
    fs::remove_dir(directory)?;
    Ok(())
}

pub(crate) fn validate_storage_paths(
    connection: &Connection,
    paths: &RootInitPaths,
) -> Result<(), GcError> {
    for (sql, parent) in [
        (
            "SELECT path FROM snapshots WHERE physical_state IN ('creating','present','deleting') UNION SELECT reserved_path FROM operations WHERE state NOT IN ('done','failed')",
            &paths.managed_dir,
        ),
        ("SELECT path FROM replay_spools", &paths.spool_dir),
    ] {
        let mut statement = connection.prepare(sql)?;
        for value in statement.query_map([], |r| r.get::<_, Vec<u8>>(0))? {
            let path = PathBuf::from(OsString::from_vec(value?));
            let relative = path.strip_prefix(parent).map_err(|_| {
                error(format!(
                    "stored artifact path is outside this state bundle: {} (expected under {})",
                    path.display(),
                    parent.display()
                ))
            })?;
            if relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(error("stored artifact path is not normalized"));
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct CleanupPlan {
    wrappers: Vec<PathBuf>,
    files: Vec<PathBuf>,
    directories: Vec<PathBuf>,
}

fn cleanup_plan(directory: &Path, paths: &RootInitPaths) -> Result<CleanupPlan, GcError> {
    let mut plan = CleanupPlan::default();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = entry.file_type()?;
        match name.as_bytes() {
            b"root.lock" if kind.is_file() => {}
            b"manager.sqlite3" | b"manager.sqlite3-wal" | b"manager.sqlite3-shm"
                if kind.is_file() =>
            {
                plan.files.push(entry.path())
            }
            b"managed" if kind.is_dir() => collect_managed(&entry.path(), false, &mut plan)?,
            b"spool" if kind.is_dir() => {
                private_directory(&paths.spool_dir)?;
                for spool in fs::read_dir(&paths.spool_dir)? {
                    let spool = spool?;
                    if !spool.file_type()?.is_file() {
                        return Err(error("spool contains a non-regular entry"));
                    }
                    plan.files.push(spool.path());
                }
                plan.directories.push(paths.spool_dir.clone());
            }
            _ => {
                return Err(error(format!(
                    "unexpected state entry {}; refusing recursive deletion",
                    entry.path().display()
                )));
            }
        }
    }
    Ok(plan)
}

fn collect_managed(
    directory: &Path,
    in_quarantine: bool,
    plan: &mut CleanupPlan,
) -> Result<(), GcError> {
    private_directory(directory)?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if !entry.file_type()?.is_dir() {
            return Err(error("managed state contains a non-directory entry"));
        }
        if name == OsStr::new("quarantine") && !in_quarantine {
            collect_managed(&entry.path(), true, plan)?;
            continue;
        }
        if !name.as_bytes().starts_with(b"cut-") {
            return Err(error("unrecognized managed snapshot wrapper"));
        }
        private_directory(&entry.path())?;
        let children = fs::read_dir(entry.path())?.collect::<Result<Vec<_>, _>>()?;
        if children.is_empty() {
            plan.directories.push(entry.path());
            continue;
        }
        if children.len() != 1
            || children[0].file_name() != OsStr::new("snapshot")
            || !children[0].file_type()?.is_dir()
        {
            return Err(error("snapshot wrapper contains unexpected entries"));
        }
        let snapshot = OpenedSubvolume::open(&children[0].path()).map_err(error)?;
        if !snapshot.subvolume.readonly() {
            return Err(error("managed snapshot is writable"));
        }
        plan.wrappers.push(entry.path());
    }
    plan.directories.push(directory.to_owned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ServiceMetadata;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};

    fn lock_file(path: &Path) -> File {
        OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap()
    }

    #[test]
    fn retired_inode_fences_a_client_after_rebuild_replaces_its_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("root.lock");
        let old = lock_file(&path);
        ensure_current_lock(&old, &path).unwrap();
        mark_retired_lock(&old).unwrap();
        assert!(ensure_current_lock(&old, &path).is_err());
        fs::remove_file(&path).unwrap();
        let new = lock_file(&path);
        ensure_current_lock(&new, &path).unwrap();
        assert!(lock_is_retired(&old).unwrap());
        assert!(ensure_current_lock(&old, &path).is_err());
    }

    #[test]
    fn inode_replacement_and_symlinks_never_reactivate_a_cached_client() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("root.lock");
        let old = lock_file(&path);
        let original = temp.path().join("original.lock");
        fs::rename(&path, &original).unwrap();
        let _new = lock_file(&path);
        assert!(ensure_current_lock(&old, &path).is_err());
        fs::remove_file(&path).unwrap();
        symlink(&original, &path).unwrap();
        assert!(ensure_current_lock(&old, &path).is_err());
        assert!(StateLock::acquire(&path, true).is_err());
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        directory: PathBuf,
        paths: RootInitPaths,
        store: Store,
        uuid: Uuid,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let uuid = Uuid::new_v4();
        let directory = temp.path().join(uuid.to_string());
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let paths = RootInitPaths::for_state_directory(&directory).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&paths.managed_dir)
            .unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&paths.spool_dir)
            .unwrap();
        lock_file(&directory.join("root.lock"));
        let store = Store::create(
            &paths.manager_db,
            &ServiceMetadata::generate([1; 16], 1).unwrap(),
        )
        .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO filesystems(id,fs_uuid) VALUES(1,?1)",
                [[2u8; 16].as_slice()],
            )
            .unwrap();
        store.connection().execute("INSERT INTO watches(id,filesystem_id,live_subvol_uuid,live_path,clock_epoch,fsmonitor_state,state) VALUES(?1,1,?2,?3,?4,'disabled','initializing')", rusqlite::params![[3u8;16].as_slice(),uuid.as_bytes().as_slice(),b"/missing-root".as_slice(),[4u8;16].as_slice()]).unwrap();
        Fixture {
            _temp: temp,
            directory,
            paths,
            store,
            uuid,
        }
    }

    fn source_files(directory: &Path) -> Vec<(PathBuf, u64, i64, i64)> {
        let mut files = fs::read_dir(directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let metadata = entry.metadata().unwrap();
                (
                    entry.path(),
                    metadata.len(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                )
            })
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    #[test]
    fn deleted_source_dry_run_reads_wal_without_mutating_manager_records() {
        let fixture = fixture();
        let before = source_files(&fixture.directory);
        let mut report = GcReport {
            dry_run: true,
            entries: vec![],
        };
        collect_store(
            &fixture.directory,
            fixture.uuid,
            [1; 16],
            100,
            &mut report,
            &mut |_, _| Ok(false),
        )
        .unwrap();
        assert_eq!(report.entries[0].action, "would-retire");
        assert_eq!(source_files(&fixture.directory), before);
        assert_eq!(
            fs::metadata(fixture.directory.join("root.lock"))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            fixture
                .store
                .connection()
                .query_row("SELECT count(*) FROM watches", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn inspection_keeps_one_sqlite_view_while_another_connection_checkpoints() {
        let fixture = fixture();
        fixture
            .store
            .connection()
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        fixture
            .store
            .connection()
            .execute("UPDATE service_metadata SET created_ns=77", [])
            .unwrap();
        let inspection = Inspection::open(&fixture.paths.manager_db).unwrap();
        fixture
            .store
            .connection()
            .execute("UPDATE service_metadata SET created_ns=88", [])
            .unwrap();
        let busy: i64 = fixture
            .store
            .connection()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy, 1);
        let observed: i64 = inspection
            .connection
            .query_row("SELECT created_ns FROM service_metadata", [], |r| r.get(0))
            .unwrap();
        assert_eq!(observed, 77);
        drop(inspection);
        let observed: i64 = fixture
            .store
            .connection()
            .query_row("SELECT created_ns FROM service_metadata", [], |r| r.get(0))
            .unwrap();
        assert_eq!(observed, 88);
    }

    #[test]
    fn existing_but_moved_source_and_unavailable_filesystem_are_preserved() {
        let fixture = fixture();
        let before = source_files(&fixture.directory);
        let mut report = GcReport {
            dry_run: false,
            entries: vec![],
        };
        let result = collect_store(
            &fixture.directory,
            fixture.uuid,
            [1; 16],
            100,
            &mut report,
            &mut |_, _| Ok(true),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("recorded path is unavailable")
        );
        let result = collect_store(
            &fixture.directory,
            fixture.uuid,
            [1; 16],
            100,
            &mut report,
            &mut |_, _| Err(error("filesystem unavailable")),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("filesystem unavailable")
        );
        assert_eq!(source_files(&fixture.directory), before);
    }

    #[test]
    fn unknown_or_symbolic_state_is_not_retired() {
        let fixture = fixture();
        let keep = fixture.directory.join("unrecognized");
        fs::write(&keep, b"keep").unwrap();
        let mut report = GcReport {
            dry_run: false,
            entries: vec![],
        };
        assert!(
            collect_store(
                &fixture.directory,
                fixture.uuid,
                [1; 16],
                100,
                &mut report,
                &mut |_, _| Ok(false)
            )
            .is_err()
        );
        assert_eq!(fs::read(&keep).unwrap(), b"keep");
        assert_eq!(
            fs::metadata(fixture.directory.join("root.lock"))
                .unwrap()
                .len(),
            0
        );
        fs::remove_file(keep).unwrap();
        let outside = fixture._temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, fixture.paths.managed_dir.join("cut-foreign")).unwrap();
        assert!(cleanup_plan(&fixture.directory, &fixture.paths).is_err());
        assert!(outside.is_dir());
    }

    #[test]
    fn deleted_source_with_snapshots_outside_its_bundle_keeps_discovery_state() {
        for dry_run in [true, false] {
            let fixture = fixture();
            let outside = fixture._temp.path().join("separate-managed/snapshot");
            fs::create_dir_all(&outside).unwrap();
            fs::write(outside.join("keep"), b"snapshot contents").unwrap();
            fixture.store.connection().execute(
                "INSERT INTO snapshots(id,filesystem_id,subvol_uuid,root_id,ctransid,otransid,path,readonly,physical_state,created_ns) VALUES(1,1,?1,zeroblob(8),zeroblob(8),zeroblob(8),?2,1,'present',1)",
                rusqlite::params![[8u8;16].as_slice(), outside.as_os_str().as_bytes()],
            ).unwrap();
            let before = source_files(&fixture.directory);
            let mut report = GcReport {
                dry_run,
                entries: vec![],
            };
            let result = collect_store(
                &fixture.directory,
                fixture.uuid,
                [1; 16],
                100,
                &mut report,
                &mut |_, _| Ok(false),
            );
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("stored artifact path is outside this state bundle")
            );
            assert_eq!(source_files(&fixture.directory), before);
            assert_eq!(
                fs::read(outside.join("keep")).unwrap(),
                b"snapshot contents"
            );
            assert_eq!(
                fixture
                    .store
                    .connection()
                    .query_row("SELECT count(*) FROM snapshots", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                fs::metadata(fixture.directory.join("root.lock"))
                    .unwrap()
                    .len(),
                0
            );
        }
    }

    fn leases() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE service_metadata(singleton INTEGER,last_boot_id BLOB);
            CREATE TABLE query_leases(state TEXT,lease_expires_ns INTEGER);
            CREATE TABLE retention_leases(state TEXT,expires_ns INTEGER);
            CREATE TABLE operations(state TEXT,lease_expires_ns INTEGER);
            CREATE TABLE comparisons(state TEXT,lease_expires_ns INTEGER);
            CREATE TABLE snapshot_delete_operations(state TEXT,lease_expires_ns INTEGER);
            CREATE TABLE snapshot_pins(owner_kind TEXT,reason TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO service_metadata VALUES(1,?1)",
                [[1u8; 16].as_slice()],
            )
            .unwrap();
        connection
    }

    #[test]
    fn retirement_preserves_active_queries_and_explicit_retention() {
        let connection = leases();
        connection
            .execute("INSERT INTO query_leases VALUES('active',200)", [])
            .unwrap();
        assert_eq!(
            retirement_blocker(&connection, [1; 16], 100)
                .unwrap()
                .as_deref(),
            Some("active query lease")
        );
        assert!(
            retirement_blocker(&connection, [1; 16], 200)
                .unwrap()
                .is_none()
        );
        assert!(
            retirement_blocker(&connection, [2; 16], 100)
                .unwrap()
                .is_none()
        );
        connection
            .execute("INSERT INTO retention_leases VALUES('active',200)", [])
            .unwrap();
        assert_eq!(
            retirement_blocker(&connection, [2; 16], 100)
                .unwrap()
                .as_deref(),
            Some("active retention lease")
        );
    }

    #[test]
    fn retirement_waits_for_operations_and_unknown_publisher_liveness() {
        let connection = leases();
        connection
            .execute("INSERT INTO operations VALUES('fs_started',200)", [])
            .unwrap();
        assert_eq!(
            retirement_blocker(&connection, [1; 16], 100)
                .unwrap()
                .as_deref(),
            Some("active snapshot operation")
        );
        assert!(
            retirement_blocker(&connection, [1; 16], 200)
                .unwrap()
                .is_none()
        );
        connection
            .execute(
                "INSERT INTO snapshot_pins VALUES('consumer-baseline','git-response:malformed')",
                [],
            )
            .unwrap();
        assert!(
            retirement_blocker(&connection, [1; 16], 200)
                .unwrap()
                .unwrap()
                .contains("unrecognized Git publisher")
        );
    }

    #[test]
    fn retirement_preserves_publishers_in_other_process_namespaces() {
        let connection = leases();
        let reason = format!("git-response:{}:1:1:0:0:0:0:", Uuid::from_bytes([1; 16]));
        connection
            .execute(
                "INSERT INTO snapshot_pins VALUES('consumer-baseline',?1)",
                [&reason],
            )
            .unwrap();
        assert!(
            retirement_blocker(&connection, [1; 16], 100)
                .unwrap()
                .unwrap()
                .contains("publisher")
        );
        assert!(
            retirement_blocker(&connection, [2; 16], 100)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn state_cleanup_handles_spool_payloads_and_empty_wrappers_before_database() {
        let fixture = fixture();
        fs::write(fixture.paths.spool_dir.join("replay.spool"), b"payload").unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(fixture.paths.managed_dir.join("cut-empty"))
            .unwrap();
        let plan = cleanup_plan(&fixture.directory, &fixture.paths).unwrap();
        drop(fixture.store);
        // Closing the last database connection may remove WAL/SHM itself.
        let plan = CleanupPlan {
            files: plan.files.into_iter().filter(|p| p.exists()).collect(),
            ..plan
        };
        remove_metadata(&fixture.directory, plan).unwrap();
        assert!(!fixture.directory.exists());
    }

    #[test]
    fn cleanup_resumes_with_a_retired_lock_and_database_sidecars_only() {
        let fixture = fixture();
        let lock = StateLock::acquire(&fixture.directory.join("root.lock"), false).unwrap();
        mark_retired_lock(&lock.0).unwrap();
        drop(fixture.store);
        // Simulate a crash after the main database unlink was persisted, but
        // before either sidecar or the retired lock was removed.
        fs::remove_file(&fixture.paths.manager_db).unwrap();
        private_directory(&fixture.directory)
            .unwrap()
            .sync_all()
            .unwrap();
        for name in ["manager.sqlite3-wal", "manager.sqlite3-shm"] {
            assert!(fixture.directory.join(name).is_file());
        }
        drop(lock);
        let before = source_files(&fixture.directory);
        let mut report = GcReport {
            dry_run: true,
            entries: vec![],
        };
        let mut unexpected_probe = |_: &RootIdentity, _: &File| {
            panic!("retired metadata cleanup must not require its deleted source")
        };
        collect_store(
            &fixture.directory,
            fixture.uuid,
            [1; 16],
            100,
            &mut report,
            &mut unexpected_probe,
        )
        .unwrap();
        assert_eq!(report.entries[0].action, "would-retire");
        assert_eq!(source_files(&fixture.directory), before);
        report.dry_run = false;
        collect_store(
            &fixture.directory,
            fixture.uuid,
            [1; 16],
            100,
            &mut report,
            &mut unexpected_probe,
        )
        .unwrap();
        assert_eq!(report.entries[1].action, "retired");
        assert!(!fixture.directory.exists());
    }

    #[test]
    fn trash_discovery_accepts_relocated_queues_only_with_uuid_names() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        assert!(valid_trash_name(&format!(".broker-trash-{first}")));
        assert!(valid_trash_name(&format!(".broker-trash-{first}-{second}")));
        assert!(!valid_trash_name(".broker-trash-anything"));
    }

    #[test]
    fn cleanup_finishes_after_a_crash_between_lock_unlink_and_directory_removal() {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let uuid = Uuid::new_v4();
        let directory = temp.path().join(uuid.to_string());
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let mut report = GcReport::default();
        collect_store(&directory, uuid, [1; 16], 100, &mut report, &mut |_, _| {
            panic!("empty cleanup needs no filesystem probe")
        })
        .unwrap();
        assert!(!directory.exists());
        assert_eq!(report.entries[0].action, "retired");
    }
}
