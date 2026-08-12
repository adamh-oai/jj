//! Explicit one-time AWACS root initialization.
//!
//! Daemon startup deliberately does not create watches or indexes. Callers
//! must run this bootstrap first, either through awacs init or JJ's explicit
//! subvolume enable workflow.

use crate::btrfs::{filesystem_info, subvolume_info};
use crate::manager::{InitializedWatch, PERMISSION_CUT, PERMISSION_READ, Permissions, Principal};
use crate::service::{InitializeOptions, Service, ServiceConfig};
use crate::store::{ServiceMetadata, Store};
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootInitPaths {
    pub managed_dir: PathBuf,
    /// Stable broker-owned trash lives beside the per-root state directory so
    /// a rebuild can discard that directory while old snapshot deletion keeps
    /// draining in the broker.
    pub snapshot_trash_dir: PathBuf,
    pub spool_dir: PathBuf,
    pub manager_db: PathBuf,
}

impl RootInitPaths {
    pub fn from_environment(root: &Path) -> Result<Self, BootstrapError> {
        let subvolume_uuid = live_subvolume_uuid(root)?;
        Self::for_subvolume_uuid(root, subvolume_uuid)
    }

    fn for_subvolume_uuid(root: &Path, subvolume_uuid: [u8; 16]) -> Result<Self, BootstrapError> {
        let state_root = state_root_for_root(root)?;
        let state_dir_name = Uuid::from_bytes(subvolume_uuid).to_string();
        let state_dir = state_root.join(&state_dir_name);
        create_private_directory(&state_dir)?;
        let (managed_dir, snapshot_trash_dir) = match env::var_os("BTRFS_AWACS_MANAGED_DIR") {
            Some(path) => {
                let root = PathBuf::from(path);
                (
                    root.join(&state_dir_name),
                    root.join(format!(".broker-trash-{state_dir_name}")),
                )
            }
            None => (
                state_dir.join("managed"),
                state_root.join(format!(".broker-trash-{state_dir_name}")),
            ),
        };
        create_private_directory(&managed_dir)?;
        let spool_dir = state_dir.join("spool");
        create_private_directory(&spool_dir)?;
        let manager_db = state_dir.join("manager.sqlite3");
        if let Some(parent) = manager_db.parent() {
            create_private_directory(parent)?;
        }
        Ok(Self {
            managed_dir,
            snapshot_trash_dir,
            spool_dir,
            manager_db,
        })
    }
}

/// Resolves the shared parent for one root's UUID-keyed AWACS state.
///
/// Git-colocated roots keep state in their Git administrative directory so a
/// staged main-worktree activation moves `.git/awacs` together with `.git`,
/// while linked worktrees use their own `$GIT_COMMON_DIR/worktrees/<id>/awacs`
/// directory rather than sharing operational state. Non-Git callers retain
/// the historical hidden-sibling default.
pub fn state_root_for_root(root: &Path) -> Result<PathBuf, BootstrapError> {
    match env::var_os("BTRFS_AWACS_STATE_DIR") {
        Some(path) => Ok(PathBuf::from(path)),
        None => default_state_root(root),
    }
}

fn default_state_root(root: &Path) -> Result<PathBuf, BootstrapError> {
    if let Some(git_dir) = git_admin_directory(root)? {
        return Ok(git_dir.join("awacs"));
    }
    hidden_sibling_state_root(root)
}

fn git_admin_directory(root: &Path) -> Result<Option<PathBuf>, BootstrapError> {
    let dot_git = root.join(".git");
    let metadata = match fs::metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BootstrapError::new(format!(
                "inspect Git administrative path {}: {error}",
                dot_git.display()
            )));
        }
    };
    if metadata.is_dir() {
        return Ok(Some(dot_git));
    }
    if !metadata.is_file() {
        return Err(BootstrapError::new(format!(
            "Git administrative path {} is neither a directory nor a gitfile",
            dot_git.display()
        )));
    }

    let contents = fs::read(&dot_git).map_err(|error| {
        BootstrapError::new(format!("read gitfile {}: {error}", dot_git.display()))
    })?;
    let git_dir = parse_gitfile_directory(&dot_git, &contents)?;
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        root.join(git_dir)
    };
    let git_dir = fs::canonicalize(&git_dir).map_err(|error| {
        BootstrapError::new(format!(
            "resolve gitfile {} administrative directory {}: {error}",
            dot_git.display(),
            git_dir.display()
        ))
    })?;
    if !git_dir.is_dir() {
        return Err(BootstrapError::new(format!(
            "gitfile {} administrative path {} is not a directory",
            dot_git.display(),
            git_dir.display()
        )));
    }
    Ok(Some(git_dir))
}

fn parse_gitfile_directory(dot_git: &Path, contents: &[u8]) -> Result<PathBuf, BootstrapError> {
    let line = contents
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or(contents);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let path = line.strip_prefix(b"gitdir: ").ok_or_else(|| {
        BootstrapError::new(format!(
            "parse gitfile {}: expected `gitdir: <path>`",
            dot_git.display()
        ))
    })?;
    if path.is_empty() {
        return Err(BootstrapError::new(format!(
            "parse gitfile {}: Git administrative path is empty",
            dot_git.display()
        )));
    }
    Ok(PathBuf::from(OsString::from_vec(path.to_vec())))
}

fn hidden_sibling_state_root(root: &Path) -> Result<PathBuf, BootstrapError> {
    let root_parent = root.parent().ok_or_else(|| {
        BootstrapError::new(format!("AWACS root {} has no parent", root.display()))
    })?;
    let root_name = root.file_name().ok_or_else(|| {
        BootstrapError::new(format!("AWACS root {} has no file name", root.display()))
    })?;
    let mut state_name = OsString::from(".");
    state_name.push(root_name);
    state_name.push("-awacs-state");
    Ok(root_parent.join(state_name))
}

/// Stable per-live-subvolume key used for state directories and scan sockets.
pub fn root_state_key(root: &Path) -> Result<String, BootstrapError> {
    Ok(Uuid::from_bytes(live_subvolume_uuid(root)?).to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitProgress {
    Phase(&'static str),
}

#[derive(Debug)]
pub struct BootstrapError {
    message: String,
}

impl BootstrapError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BootstrapError {}

/// Creates the first immutable snapshot and durable watch state for an
/// already-converted Btrfs subvolume root.
pub fn initialize_root(
    root: &Path,
    mut progress: impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    initialize_root_with_consumer_mode(root, false, &mut progress)
}

/// Creates the first immutable root snapshot and pins it as the watch-local
/// consumer's initial endpoint.
///
/// Git worktree creation uses this after a dirty-source checkout has already
/// materialized its target tree. The watch id is a stable, root-local Git
/// lane owner, distinct from any JJ workspace owner.
pub fn initialize_root_with_watch_consumer(
    root: &Path,
    mut progress: impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    initialize_root_with_consumer_mode(root, true, &mut progress)
}

fn initialize_root_with_consumer_mode(
    root: &Path,
    retain_watch_consumer: bool,
    progress: &mut impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    let _root_lock = lock_root_state(&canonical_root)?;
    let now_ns = current_time_ns()?;
    progress(InitProgress::Phase("opening state"));
    let mut service = open_bootstrap_service(&canonical_root, now_ns)?;
    if service.require_initialized_root(&canonical_root).is_ok() {
        return Err(BootstrapError::new(format!(
            "{} is already initialized",
            canonical_root.display()
        )));
    }
    progress(InitProgress::Phase("creating initial immutable snapshot"));
    let initialized = service
        .initialize(&canonical_root, &initialize_options(now_ns)?)
        .map_err(|error| BootstrapError::new(format!("initialize AWACS root: {error}")))?;
    if retain_watch_consumer {
        retain_initial_consumer_baseline(&mut service, &initialized, initialized.watch_id)?;
    }
    progress(InitProgress::Phase("initial watch is ready"));
    Ok(initialized)
}

/// Initializes a Btrfs snapshot worktree from its parent's retained baseline.
///
/// Unlike the historical lineage adoption path, this creates a new immutable
/// baseline snapshot and a new sequence-zero revision for the child. The
/// copied map is then independent of the parent watch and its snapshots.
pub fn initialize_descendant_root(
    root: &Path,
    parent_root: &Path,
    inherited_baseline_snapshot_uuid: Option<[u8; 16]>,
    mut progress: impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    initialize_descendant_root_with_owner(
        root,
        parent_root,
        inherited_baseline_snapshot_uuid,
        None,
        false,
        &mut progress,
    )
}

/// Initializes a descendant and commits its first immutable snapshot as a
/// durable consumer baseline owned by `owner_id`.
///
/// This is used when another tool prepares a worktree for a delayed consumer:
/// the physical snapshot and shared replay history must remain available even
/// before that consumer writes its own local journal. Generic descendants use
/// [`initialize_descendant_root()`] and do not acquire a durable owner.
pub fn initialize_descendant_root_with_consumer(
    root: &Path,
    parent_root: &Path,
    inherited_baseline_snapshot_uuid: Option<[u8; 16]>,
    owner_id: [u8; 16],
    mut progress: impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    initialize_descendant_root_with_owner(
        root,
        parent_root,
        inherited_baseline_snapshot_uuid,
        Some(owner_id),
        false,
        &mut progress,
    )
}

/// Initializes a descendant with a watch-local consumer endpoint plus an
/// optional independent consumer endpoint.
///
/// Git worktree creation uses the watch-local owner for its index token lane
/// and passes the copied JJ workspace owner, when present, as the additional
/// endpoint. Both pin sequence zero before Git can materialize a different
/// target tree.
pub fn initialize_descendant_root_with_watch_consumer(
    root: &Path,
    parent_root: &Path,
    inherited_baseline_snapshot_uuid: Option<[u8; 16]>,
    additional_owner_id: Option<[u8; 16]>,
    mut progress: impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    initialize_descendant_root_with_owner(
        root,
        parent_root,
        inherited_baseline_snapshot_uuid,
        additional_owner_id,
        true,
        &mut progress,
    )
}

/// Verifies that delayed JJ adoption can inherit one exact retained parent
/// baseline before Git creates any linked-worktree state.
///
/// A copied JJ journal names an immutable semantic-tree baseline. The baseline
/// must still have both a ready revision and a durable consumer pin; otherwise
/// a concurrent direct-scan GC is free to remove its physical snapshot after a
/// check-only preflight succeeds.
pub fn require_retained_descendant_baseline(
    parent_root: &Path,
    baseline_snapshot_uuid: [u8; 16],
    baseline_owner_id: [u8; 16],
) -> Result<(), BootstrapError> {
    let canonical_parent_root = fs::canonicalize(parent_root)
        .map_err(|error| BootstrapError::new(format!("canonicalize parent AWACS root: {error}")))?;
    let parent_uuid = live_subvolume_uuid(&canonical_parent_root)?;
    let parent_file = File::open(&canonical_parent_root)
        .map_err(|error| BootstrapError::new(format!("open parent AWACS root: {error}")))?;
    let filesystem = filesystem_info(parent_file.as_fd()).map_err(|error| {
        BootstrapError::new(format!("inspect parent AWACS filesystem: {error}"))
    })?;
    let parent_paths = RootInitPaths::for_subvolume_uuid(&canonical_parent_root, parent_uuid)?;
    let _parent_lock = lock_root_state_paths(&parent_paths)?;
    let mut parent_store = Store::open(&parent_paths.manager_db).map_err(|error| {
        BootstrapError::new(format!(
            "open parent manager store {}: {error}",
            parent_paths.manager_db.display()
        ))
    })?;
    let retained = parent_store
        .retain_existing_consumer_seed_revision_for_baseline(
            filesystem.fs_uuid,
            baseline_snapshot_uuid,
            baseline_owner_id,
        )
        .map_err(|error| BootstrapError::new(format!("find retained parent baseline: {error}")))?;
    if retained.is_none() {
        if parent_store
            .baseline_physical_state(filesystem.fs_uuid, baseline_snapshot_uuid)
            .map_err(|error| BootstrapError::new(format!("inspect parent baseline: {error}")))?
            .as_deref()
            == Some("deleted")
        {
            return Err(BootstrapError::new(format!(
                "{} JJ adoption baseline {} was already collected; run jj status once with the upgraded AWACS consumer to publish a new retained baseline",
                canonical_parent_root.display(),
                Uuid::from_bytes(baseline_snapshot_uuid)
            )));
        }
        return Err(BootstrapError::new(format!(
            "{} has no retained AWACS parent baseline to seed",
            canonical_parent_root.display()
        )));
    }
    Ok(())
}

fn initialize_descendant_root_with_owner(
    root: &Path,
    parent_root: &Path,
    inherited_baseline_snapshot_uuid: Option<[u8; 16]>,
    owner_id: Option<[u8; 16]>,
    retain_watch_consumer: bool,
    progress: &mut impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    let started = Instant::now();
    log::debug!(
        "AWACS descendant initialization started: root={} parent_root={}",
        root.display(),
        parent_root.display()
    );
    let result = initialize_descendant_root_inner(
        root,
        parent_root,
        inherited_baseline_snapshot_uuid,
        owner_id,
        retain_watch_consumer,
        progress,
    );
    match &result {
        Ok(_) => log::debug!(
            "AWACS descendant initialization completed: root={} elapsed={:?}",
            root.display(),
            started.elapsed()
        ),
        Err(error) => log::debug!(
            "AWACS descendant initialization failed: root={} elapsed={:?} error={error}",
            root.display(),
            started.elapsed()
        ),
    }
    result
}

fn initialize_descendant_root_inner(
    root: &Path,
    parent_root: &Path,
    inherited_baseline_snapshot_uuid: Option<[u8; 16]>,
    owner_id: Option<[u8; 16]>,
    retain_watch_consumer: bool,
    progress: &mut impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    let canonical_parent_root = fs::canonicalize(parent_root)
        .map_err(|error| BootstrapError::new(format!("canonicalize parent AWACS root: {error}")))?;
    let child_subvolume_uuid = live_subvolume_uuid(&canonical_root)?;
    let child_file = File::open(&canonical_root)
        .map_err(|error| BootstrapError::new(format!("open AWACS root: {error}")))?;
    let child_subvolume = subvolume_info(child_file.as_fd())
        .map_err(|error| BootstrapError::new(format!("inspect AWACS root: {error}")))?;
    let child_filesystem = filesystem_info(child_file.as_fd())
        .map_err(|error| BootstrapError::new(format!("inspect AWACS filesystem: {error}")))?;
    let parent_uuid = child_subvolume.parent_uuid.ok_or_else(|| {
        BootstrapError::new(format!(
            "{} is not a Btrfs snapshot with a parent",
            canonical_root.display()
        ))
    })?;
    let observed_parent_uuid = live_subvolume_uuid(&canonical_parent_root)?;
    if observed_parent_uuid != parent_uuid {
        return Err(BootstrapError::new(format!(
            "{} is not the Btrfs snapshot parent of {}",
            canonical_parent_root.display(),
            canonical_root.display()
        )));
    }
    let now_ns = current_time_ns()?;
    progress(InitProgress::Phase("opening state"));
    let parent_paths = RootInitPaths::for_subvolume_uuid(&canonical_parent_root, parent_uuid)?;
    let child_paths = RootInitPaths::for_subvolume_uuid(&canonical_root, child_subvolume_uuid)?;
    // Parent first, child second: every descendant seed follows this order,
    // while the fresh child has no independent callers yet.
    let _parent_lock = lock_root_state_paths(&parent_paths)?;
    let _child_lock = lock_root_state_paths(&child_paths)?;
    let mut parent_store =
        timed_descendant_phase(&canonical_root, "open parent manager store", || {
            Store::open(&parent_paths.manager_db).map_err(|error| {
                BootstrapError::new(format!(
                    "open parent manager store {}: {error}",
                    parent_paths.manager_db.display()
                ))
            })
        })?;
    let seed_revision_id = timed_descendant_phase(&canonical_root, "find parent baseline", || {
        let revision = if let Some(baseline_snapshot_uuid) = inherited_baseline_snapshot_uuid {
            parent_store.descendant_seed_revision_for_baseline(
                child_filesystem.fs_uuid,
                baseline_snapshot_uuid,
            )
        } else {
            parent_store.descendant_seed_revision(child_filesystem.fs_uuid, parent_uuid)
        }
        .map_err(|error| BootstrapError::new(format!("find parent AWACS baseline: {error}")))?;
        revision.ok_or_else(|| {
            BootstrapError::new(format!(
                "{} has no retained AWACS parent baseline to seed",
                canonical_root.display()
            ))
        })
    })?;
    if child_paths.manager_db.exists() {
        return Err(BootstrapError::new(format!(
            "{} is already initialized",
            canonical_root.display()
        )));
    }
    let boot_id = read_boot_id()?;
    let metadata = ServiceMetadata::generate(boot_id, now_ns).map_err(|error| {
        BootstrapError::new(format!("generate child manager metadata: {error}"))
    })?;
    progress(InitProgress::Phase("creating child state"));
    let child_store =
        timed_descendant_phase(&canonical_root, "create child operational store", || {
            Store::create_descendant_seed(
                &mut parent_store,
                &child_paths.manager_db,
                &metadata,
                seed_revision_id,
                now_ns,
                child_paths
                    .managed_dir
                    // Imported parent snapshots are tombstones in the child
                    // store, but startup still validates every recorded path
                    // as one private managed wrapper entry. Use a normal
                    // `cut-<32-hex-id>/snapshot` shape without creating any
                    // physical entry for these inert rows.
                    .join("cut-")
                    .as_os_str()
                    .as_bytes(),
            )
            .map_err(|error| {
                BootstrapError::new(format!("create child operational store: {error}"))
            })
        })?;
    drop(parent_store);
    let config = external_service_config(
        child_paths.managed_dir,
        child_paths.snapshot_trash_dir,
        child_paths.spool_dir,
        boot_id,
    );
    let mut service = timed_descendant_phase(&canonical_root, "open child service", || {
        Service::new_external(child_store, config)
            .map_err(|error| BootstrapError::new(format!("open child AWACS service: {error}")))
    })?;
    progress(InitProgress::Phase("publishing inherited baseline"));
    debug_assert_eq!(child_subvolume_uuid, child_subvolume.uuid);
    let options = initialize_options(now_ns)?;
    let initialized =
        timed_descendant_phase(&canonical_root, "publish inherited child baseline", || {
            service
                .initialize_with_inherited_revision(
                    &canonical_root,
                    &options,
                    seed_revision_id,
                    inherited_baseline_snapshot_uuid,
                )
                .map_err(|error| {
                    BootstrapError::new(format!("initialize AWACS snapshot worktree: {error}"))
                })
        })?;
    let mut owner_ids = Vec::new();
    if retain_watch_consumer {
        owner_ids.push(initialized.watch_id);
    }
    if let Some(owner_id) = owner_id
        && !owner_ids.contains(&owner_id)
    {
        owner_ids.push(owner_id);
    }
    for owner_id in owner_ids {
        timed_descendant_phase(&canonical_root, "retain initial consumer baseline", || {
            retain_initial_consumer_baseline(&mut service, &initialized, owner_id)
        })?;
    }
    progress(InitProgress::Phase("initial watch is ready"));
    Ok(initialized)
}

fn retain_initial_consumer_baseline(
    service: &mut Service,
    initialized: &InitializedWatch,
    owner_id: [u8; 16],
) -> Result<(), BootstrapError> {
    service
        .store_mut()
        .stage_consumer_baseline(
            initialized.watch_id,
            initialized.grant_id,
            owner_id,
            initialized.snapshot_id,
        )
        .and_then(|()| service.store_mut().finish_consumer_baseline(owner_id, true))
        .map_err(|error| BootstrapError::new(format!("retain initial baseline: {error}")))
}

fn timed_descendant_phase<T>(
    root: &Path,
    phase: &'static str,
    action: impl FnOnce() -> Result<T, BootstrapError>,
) -> Result<T, BootstrapError> {
    let started = Instant::now();
    log::debug!(
        "AWACS descendant initialization phase started: root={} phase={phase}",
        root.display()
    );
    let result = action();
    match &result {
        Ok(_) => log::debug!(
            "AWACS descendant initialization phase completed: root={} phase={phase} elapsed={:?}",
            root.display(),
            started.elapsed()
        ),
        Err(error) => log::debug!(
            "AWACS descendant initialization phase failed: root={} phase={phase} elapsed={:?} error={error}",
            root.display(),
            started.elapsed()
        ),
    }
    result
}

/// Opens the already-initialized state for one root through the stateless
/// privileged broker.
///
/// Direct scan callers use this instead of discovering or starting a
/// persistent user daemon. Initialization remains an explicit separate
/// operation; opening an absent database is an actionable error.
pub fn open_initialized_root_service(root: &Path) -> Result<Service, BootstrapError> {
    let (canonical_root, service) = open_existing_root_service(root)?;
    service
        .require_initialized_root(&canonical_root)
        .map_err(|error| BootstrapError::new(format!("verify AWACS root state: {error}")))?;
    Ok(service)
}

/// Opens an existing operational store without requiring bootstrap to have
/// published an active watch yet.
fn open_existing_root_service(root: &Path) -> Result<(PathBuf, Service), BootstrapError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    let paths = RootInitPaths::from_environment(&canonical_root)?;
    if !paths.manager_db.exists() {
        return Err(BootstrapError::new(format!(
            "{} is not initialized; run awacs init {} first",
            canonical_root.display(),
            canonical_root.display(),
        )));
    }
    let store = Store::open(&paths.manager_db)
        .map_err(|error| BootstrapError::new(format!("open manager store: {error}")))?;
    let config = external_service_config(
        paths.managed_dir,
        paths.snapshot_trash_dir,
        paths.spool_dir,
        read_boot_id()?,
    );
    let service = Service::new_external(store, config)
        .map_err(|error| BootstrapError::new(format!("open AWACS root service: {error}")))?;
    Ok((canonical_root, service))
}

/// Moves every old or partially-created managed snapshot into broker-owned
/// trash before a caller discards this root's operational database during an
/// explicit rebuild.
///
/// Rebuild deliberately does not open or interpret the old manager database:
/// it creates one throwaway store only to authenticate a cleanup-only broker
/// session, scans physical wrapper directories, then deletes the entire old
/// state tree. Physical Btrfs deletion continues asynchronously after this
/// returns.
pub fn trash_existing_root_snapshots_for_rebuild(root: &Path) -> Result<usize, BootstrapError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    let paths = RootInitPaths::from_environment(&canonical_root)?;
    let state_dir = paths
        .manager_db
        .parent()
        .ok_or_else(|| BootstrapError::new("AWACS manager database has no state directory"))?;
    let cleanup_dir = state_dir.join(format!(".rebuild-cleanup-{}", Uuid::new_v4()));
    create_private_directory(&cleanup_dir)?;
    let cleanup_db = cleanup_dir.join("manager.sqlite3");
    let boot_id = read_boot_id()?;
    let metadata = ServiceMetadata::generate(boot_id, current_time_ns()?)
        .map_err(|error| BootstrapError::new(format!("generate rebuild metadata: {error}")))?;
    let store = Store::create(&cleanup_db, &metadata)
        .map_err(|error| BootstrapError::new(format!("create rebuild cleanup store: {error}")))?;
    let config = external_service_config(
        paths.managed_dir,
        paths.snapshot_trash_dir,
        paths.spool_dir,
        boot_id,
    );
    let service = Service::new_external_rebuild_cleanup(store, config)
        .map_err(|error| BootstrapError::new(format!("open rebuild cleanup service: {error}")))?;
    let result = service
        .trash_managed_snapshots_for_rebuild()
        .map_err(|error| BootstrapError::new(format!("trash old AWACS snapshots: {error}")));
    drop(service);
    fs::remove_dir_all(&cleanup_dir).map_err(|error| {
        BootstrapError::new(format!(
            "remove rebuild cleanup store {}: {error}",
            cleanup_dir.display()
        ))
    })?;
    result
}

fn live_subvolume_uuid(root: &Path) -> Result<[u8; 16], BootstrapError> {
    let file = File::open(root)
        .map_err(|error| BootstrapError::new(format!("open AWACS root: {error}")))?;
    subvolume_info(file.as_fd())
        .map(|subvolume| subvolume.uuid)
        .map_err(|error| BootstrapError::new(format!("inspect AWACS root: {error}")))
}

fn open_bootstrap_service(root: &Path, now_ns: i64) -> Result<Service, BootstrapError> {
    let paths = RootInitPaths::from_environment(root)?;
    let boot_id = read_boot_id()?;
    let store = if paths.manager_db.exists() {
        Store::open(&paths.manager_db)
            .map_err(|error| BootstrapError::new(format!("open manager store: {error}")))?
    } else {
        let metadata = ServiceMetadata::generate(boot_id, now_ns)
            .map_err(|error| BootstrapError::new(format!("generate manager metadata: {error}")))?;
        Store::create(&paths.manager_db, &metadata)
            .map_err(|error| BootstrapError::new(format!("create manager store: {error}")))?
    };
    let config = external_service_config(
        paths.managed_dir,
        paths.snapshot_trash_dir,
        paths.spool_dir,
        boot_id,
    );
    Service::new_external(store, config)
        .map_err(|error| BootstrapError::new(format!("open AWACS bootstrap service: {error}")))
}

fn external_service_config(
    managed_dir: PathBuf,
    snapshot_trash_dir: PathBuf,
    spool_dir: PathBuf,
    boot_id: [u8; 16],
) -> ServiceConfig {
    let broker_socket = env::var_os("BTRFS_AWACS_BROKER_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/btrfs-awacs/broker.sock"));
    ServiceConfig::new(managed_dir, spool_dir, boot_id)
        .with_snapshot_trash_directory(snapshot_trash_dir)
        .with_broker_socket(broker_socket)
}

fn initialize_options(now_ns: i64) -> Result<InitializeOptions, BootstrapError> {
    let permissions = Permissions::new(PERMISSION_READ | PERMISSION_CUT)
        .map_err(|error| BootstrapError::new(format!("build AWACS permissions: {error}")))?;
    Ok(InitializeOptions {
        principal: Principal::Uid(u64::from(unsafe { libc::geteuid() })),
        permissions,
        requester_uid: unsafe { libc::geteuid() },
        requester_gid: unsafe { libc::getegid() },
        now_ns,
    })
}

fn create_private_directory(path: &Path) -> Result<(), BootstrapError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| BootstrapError::new(format!("create {}: {error}", path.display())))
}

struct RootStateLock {
    file: File,
}

fn lock_root_state(root: &Path) -> Result<RootStateLock, BootstrapError> {
    let paths = RootInitPaths::from_environment(root)?;
    lock_root_state_paths(&paths)
}

fn lock_root_state_paths(paths: &RootInitPaths) -> Result<RootStateLock, BootstrapError> {
    let state_dir = paths
        .manager_db
        .parent()
        .ok_or_else(|| BootstrapError::new("AWACS manager database has no state directory"))?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(state_dir.join("root.lock"))
        .map_err(|error| BootstrapError::new(format!("open AWACS root lock: {error}")))?;
    // SAFETY: flock only inspects the valid open descriptor and blocks until
    // another cooperating AWACS caller releases this root's state lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(BootstrapError::new(format!(
            "lock AWACS root state: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(RootStateLock { file })
}

impl Drop for RootStateLock {
    fn drop(&mut self) {
        // SAFETY: unlocking the same still-open descriptor is best-effort
        // cleanup and has no memory-safety precondition.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn read_boot_id() -> Result<[u8; 16], BootstrapError> {
    let text = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| BootstrapError::new(format!("read kernel boot ID: {error}")))?;
    let uuid = Uuid::parse_str(text.trim())
        .map_err(|error| BootstrapError::new(format!("parse kernel boot ID: {error}")))?;
    Ok(*uuid.as_bytes())
}

fn current_time_ns() -> Result<i64, BootstrapError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| BootstrapError::new(format!("read system clock: {error}")))?;
    i64::try_from(elapsed.as_nanos())
        .map_err(|_| BootstrapError::new("system time exceeds signed nanoseconds"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_root_uses_main_worktree_git_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();

        assert_eq!(default_state_root(&root).unwrap(), root.join(".git/awacs"));
    }

    #[test]
    fn default_state_root_uses_linked_worktree_git_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worktree");
        let git_dir = directory.path().join("repo/.git/worktrees/worktree");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            root.join(".git"),
            b"gitdir: ../repo/.git/worktrees/worktree\n",
        )
        .unwrap();

        assert_eq!(default_state_root(&root).unwrap(), git_dir.join("awacs"));
    }

    #[test]
    fn default_state_root_without_git_is_hidden_sibling_of_tracked_root() {
        assert_eq!(
            default_state_root(Path::new("/foo/bar")).unwrap(),
            PathBuf::from("/foo/.bar-awacs-state")
        );
    }
}
