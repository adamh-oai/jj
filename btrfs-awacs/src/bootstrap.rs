//! Explicit one-time AWACS root initialization.
//!
//! Daemon startup deliberately does not create watches or indexes. Callers
//! must run this bootstrap first, either through awacs init or JJ's explicit
//! subvolume enable workflow.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use uuid::Uuid;

use crate::btrfs::OpenedSubvolume;
use crate::btrfs::filesystem_info;
use crate::btrfs::subvolume_info;
use crate::manager::InitializedWatch;
use crate::manager::PERMISSION_CUT;
use crate::manager::PERMISSION_READ;
use crate::manager::Permissions;
use crate::manager::Principal;
use crate::service::InitializeOptions;
use crate::service::Service;
use crate::service::ServiceConfig;
use crate::store::ServiceMetadata;
use crate::store::Store;

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
    /// Resolves an existing UUID bundle without creating it or opening its live
    /// worktree. Maintenance must be able to find state after that root is gone.
    pub(crate) fn for_state_directory(state_dir: &Path) -> Result<Self, BootstrapError> {
        let name = state_dir
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| Uuid::parse_str(name).is_ok_and(|uuid| uuid.to_string() == *name))
            .ok_or_else(|| {
                BootstrapError::new("AWACS state directory must have a UUID basename")
            })?;
        let parent = state_dir
            .parent()
            .ok_or_else(|| BootstrapError::new("AWACS state directory has no parent"))?;
        Ok(Self {
            managed_dir: state_dir.join("managed"),
            snapshot_trash_dir: parent.join(format!(".broker-trash-{name}")),
            spool_dir: state_dir.join("spool"),
            manager_db: state_dir.join("manager.sqlite3"),
        })
    }

    pub fn from_environment(root: &Path) -> Result<Self, BootstrapError> {
        let subvolume_uuid = live_subvolume_uuid(root)?;
        Self::for_subvolume_uuid(root, subvolume_uuid)
    }

    fn for_subvolume_uuid(root: &Path, subvolume_uuid: [u8; 16]) -> Result<Self, BootstrapError> {
        let paths = Self::resolve_for_subvolume_uuid(root, subvolume_uuid)?;
        create_private_directory(paths.manager_db.parent().expect("state directory"))?;
        for directory in [&paths.managed_dir, &paths.spool_dir] {
            create_private_directory(directory)?;
        }
        Ok(paths)
    }

    /// Check for existing state before creating supporting directories that
    /// would hide a missing or incorrectly configured state root.
    pub(crate) fn for_existing_root(root: &Path) -> Result<Self, BootstrapError> {
        let paths = Self::resolve_for_subvolume_uuid(root, live_subvolume_uuid(root)?)?;
        let configured_root = env::var_os("BTRFS_AWACS_STATE_DIR").map(PathBuf::from);
        paths.require_existing_manager(root, configured_root.as_deref())?;
        for directory in [&paths.managed_dir, &paths.spool_dir] {
            create_private_directory(directory)?;
        }
        Ok(paths)
    }

    fn resolve_for_subvolume_uuid(
        root: &Path,
        subvolume_uuid: [u8; 16],
    ) -> Result<Self, BootstrapError> {
        let state_root = state_root_for_root(root)?;
        let state_dir_name = Uuid::from_bytes(subvolume_uuid).to_string();
        let state_dir = state_root.join(&state_dir_name);
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
        let spool_dir = state_dir.join("spool");
        let manager_db = state_dir.join("manager.sqlite3");
        Ok(Self {
            managed_dir,
            snapshot_trash_dir,
            spool_dir,
            manager_db,
        })
    }

    fn require_existing_manager(
        &self,
        root: &Path,
        configured_root: Option<&Path>,
    ) -> Result<(), BootstrapError> {
        if let Some(path) = configured_root {
            match fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(BootstrapError::new(format!(
                        "BTRFS_AWACS_STATE_DIR points to {}, which is not a directory",
                        path.display(),
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(BootstrapError::new(format!(
                        "{} is not initialized: BTRFS_AWACS_STATE_DIR points to {}, which does \
                         not exist; set it to the existing AWACS state directory used when \
                         initializing or rebuilding this checkout",
                        root.display(),
                        path.display(),
                    )));
                }
                Err(error) => {
                    return Err(BootstrapError::new(format!(
                        "inspect BTRFS_AWACS_STATE_DIR directory {}: {error}",
                        path.display(),
                    )));
                }
            }
        }
        if self.manager_db.try_exists().map_err(|error| {
            BootstrapError::new(format!(
                "inspect AWACS manager database {}: {error}",
                self.manager_db.display(),
            ))
        })? {
            return Ok(());
        }
        let configuration = match configured_root {
            Some(path) => format!("BTRFS_AWACS_STATE_DIR is set to {}", path.display()),
            None => "BTRFS_AWACS_STATE_DIR is unset".to_owned(),
        };
        Err(BootstrapError::new(format!(
            "{} is not initialized: {configuration}; no AWACS manager database at {}. If this \
             checkout was already initialized or rebuilt, set BTRFS_AWACS_STATE_DIR to the state \
             directory used then; otherwise run awacs init {} first",
            root.display(),
            self.manager_db.display(),
            root.display(),
        )))
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

pub(crate) fn git_admin_directory(root: &Path) -> Result<Option<PathBuf>, BootstrapError> {
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
    initialize_root_locked(&canonical_root, retain_watch_consumer, progress)
}

fn initialize_root_locked(
    canonical_root: &Path,
    retain_watch_consumer: bool,
    progress: &mut impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
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
                "{} JJ adoption baseline {} was already collected; run jj status once with the \
                 upgraded AWACS consumer to publish a new retained baseline",
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
    timed_descendant_phase("initialize descendant", || {
        initialize_descendant_root_inner(
            root,
            parent_root,
            inherited_baseline_snapshot_uuid,
            owner_id,
            retain_watch_consumer,
            progress,
        )
    })
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
    let child_paths = timed_descendant_phase("prepare child state paths", || {
        RootInitPaths::for_subvolume_uuid(&canonical_root, child_subvolume_uuid)
    })?;
    // An exact inherited baseline must remain stable while the child is
    // initialized. Ordinary snapshot workspaces publish an independent child
    // baseline instead, so they never need to inspect or lock parent state.
    let parent_paths = inherited_baseline_snapshot_uuid
        .map(|_| {
            timed_descendant_phase("prepare parent state paths", || {
                RootInitPaths::for_subvolume_uuid(&canonical_parent_root, parent_uuid)
            })
        })
        .transpose()?;
    // When an exact baseline is inherited, preserve the parent-before-child
    // lock order used by all descendant seed operations.
    let _parent_lock = parent_paths
        .as_ref()
        .map(|paths| timed_descendant_phase("lock parent state", || lock_root_state_paths(paths)))
        .transpose()?;
    let _child_lock =
        timed_descendant_phase("lock child state", || lock_root_state_paths(&child_paths))?;
    let mut parent_store = parent_paths
        .as_ref()
        .map(|paths| {
            timed_descendant_phase("open parent manager store", || {
                Store::open(&paths.manager_db).map_err(|error| {
                    BootstrapError::new(format!(
                        "open parent manager store {}: {error}",
                        paths.manager_db.display()
                    ))
                })
            })
        })
        .transpose()?;
    let seed_revision_id = inherited_baseline_snapshot_uuid
        .map(|baseline_snapshot_uuid| {
            timed_descendant_phase("find parent baseline", || {
                let revision = parent_store
                    .as_ref()
                    .expect("an inherited baseline has an open parent store")
                    .descendant_seed_revision_for_baseline(
                        child_filesystem.fs_uuid,
                        baseline_snapshot_uuid,
                    )
                    .map_err(|error| {
                        BootstrapError::new(format!("find parent AWACS baseline: {error}"))
                    })?;
                revision.ok_or_else(|| {
                    BootstrapError::new(format!(
                        "{} has no retained AWACS parent baseline to seed",
                        canonical_root.display()
                    ))
                })
            })
        })
        .transpose()?;
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
    let child_store = timed_descendant_phase("create child operational store", || {
        let child_store = if let Some(seed_revision_id) = seed_revision_id {
            Store::create_descendant_seed(
                parent_store
                    .as_mut()
                    .expect("an inherited baseline has an open parent store"),
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
        } else {
            Store::create(&child_paths.manager_db, &metadata)
        };
        child_store.map_err(|error| {
            BootstrapError::new(format!("create child operational store: {error}"))
        })
    })?;
    timed_descendant_phase("close parent manager store", || {
        drop(parent_store);
        Ok(())
    })?;
    let config = external_service_config(
        child_paths.managed_dir,
        child_paths.snapshot_trash_dir,
        child_paths.spool_dir,
        boot_id,
    );
    let mut service = timed_descendant_phase("open child service", || {
        Service::new_external(child_store, config)
            .map_err(|error| BootstrapError::new(format!("open child AWACS service: {error}")))
    })?;
    progress(InitProgress::Phase("publishing inherited baseline"));
    debug_assert_eq!(child_subvolume_uuid, child_subvolume.uuid);
    let options = initialize_options(now_ns)?;
    let initialized = timed_descendant_phase("publish inherited child baseline", || {
        let initialized = if let Some(seed_revision_id) = seed_revision_id {
            service.initialize_with_inherited_revision(
                &canonical_root,
                &options,
                seed_revision_id,
                inherited_baseline_snapshot_uuid,
            )
        } else {
            service.initialize(&canonical_root, &options)
        };
        initialized.map_err(|error| {
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
        timed_descendant_phase("retain initial consumer baseline", || {
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
    phase: &'static str,
    action: impl FnOnce() -> Result<T, BootstrapError>,
) -> Result<T, BootstrapError> {
    let _span = tracing::info_span!("descendant_phase", phase, pid = std::process::id()).entered();
    let started = Instant::now();
    tracing::debug!(phase, "AWACS descendant initialization phase started");
    let result = action();
    let elapsed = started.elapsed();
    tracing::info!(
        phase,
        pid = std::process::id(),
        elapsed_ms = elapsed.as_millis() as u64,
        elapsed_us = elapsed.as_micros() as u64,
        success = result.is_ok(),
        "AWACS descendant initialization phase completed"
    );
    result
}

/// Opens the already-initialized state for one root through the stateless
/// privileged broker.
///
/// Direct scan callers use this instead of discovering or starting a
/// persistent user daemon. Initialization remains an explicit separate
/// operation; opening an absent database is an actionable error.
/// The caller must hold this root's `root.lock` through service startup.
pub fn open_initialized_root_service(root: &Path) -> Result<Service, BootstrapError> {
    let (canonical_root, service) = open_existing_root_service(root)?;
    crate::diagnostics::timed("verify initialized root", || {
        service.require_initialized_root(&canonical_root)
    })
    .map_err(|error| BootstrapError::new(format!("verify AWACS root state: {error}")))?;
    Ok(service)
}

/// Finalizes the namespace paths after an initialized live subvolume moves.
///
/// Subvolume activation renames the checkout and its nested Git metadata
/// together. Rebind their absolute storage paths without replacing any
/// snapshots, baselines, or replay history. This also repairs an interrupted
/// activation before a caller tries to recover the relocated service.
pub fn finalize_root_path(root: &Path) -> Result<(), BootstrapError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    let paths = RootInitPaths::for_existing_root(&canonical_root)?;
    let _root_lock = lock_root_state_paths(&paths)?;
    let mut store = Store::open(&paths.manager_db)
        .map_err(|error| BootstrapError::new(format!("open manager store: {error}")))?;
    if !rebind_root_path(&canonical_root, &mut store)? {
        return Err(BootstrapError::new(
            "AWACS root has no authorized active watch for its filesystem and subvolume identity",
        ));
    }
    Ok(())
}

fn rebind_root_path(root: &Path, store: &mut Store) -> Result<bool, BootstrapError> {
    // Read both identities from the same live descriptor. A copied manager
    // database or another checkout at the old name is not proof of a move.
    let live =
        crate::diagnostics::timed("rebind open live subvolume", || OpenedSubvolume::open(root))
            .map_err(|error| BootstrapError::new(format!("open AWACS root: {error}")))?;
    let rebound = crate::diagnostics::timed("rebind watch in store", || {
        store.rebind_active_uid_watch_path_by_subvolume(
            live.filesystem.fs_uuid,
            live.subvolume.uuid,
            root.as_os_str().as_bytes(),
            unsafe { libc::geteuid() },
            PERMISSION_READ | PERMISSION_CUT,
        )
    })
    .map_err(|error| BootstrapError::new(format!("finalize AWACS root paths: {error}")))?;
    Ok(rebound.is_some())
}

/// Opens an existing operational store without requiring bootstrap to have
/// published an active watch yet.
fn open_existing_root_service(root: &Path) -> Result<(PathBuf, Service), BootstrapError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    let paths = RootInitPaths::for_existing_root(&canonical_root)?;
    let mut store =
        crate::diagnostics::timed("open manager store", || Store::open(&paths.manager_db))
            .map_err(|error| BootstrapError::new(format!("open manager store: {error}")))?;
    // Recovery reads snapshot and operation paths immediately. Lazy facade
    // registration is too late when .git/awacs moved with the live root.
    // A partially initialized store may not have an active watch until
    // recovery finishes. No match means no namespace edits; the existing
    // initialized-root and scan authorization checks still apply afterward.
    crate::diagnostics::timed("rebind root path", || {
        rebind_root_path(&canonical_root, &mut store)
    })?;
    let config = external_service_config(
        paths.managed_dir,
        paths.snapshot_trash_dir,
        paths.spool_dir,
        read_boot_id()?,
    );
    let service = crate::diagnostics::timed("open external root service", || {
        Service::new_external(store, config)
    })
    .map_err(|error| BootstrapError::new(format!("open AWACS root service: {error}")))?;
    Ok((canonical_root, service))
}

/// Discards this live root's operational state and seeds an independent watch.
/// Both the configured store and the root's former colocated store are retired;
/// another worktree's UUID bundle is never included.
pub fn rebuild_root(
    root: &Path,
    mut progress: impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    let root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    discard_root_state(&root)?;
    // Old clients retain the retired lock inode. They cannot use the new
    // database even after this path acquires a fresh lock file.
    let _root_lock = lock_root_state(&root)?;
    initialize_root_locked(&root, false, &mut progress)
}

/// Retires exactly this root's current and legacy state bundles. The live root
/// is required only to prove its identity; cleanup does not depend on a usable
/// old database. Callers must serialize their own working-copy/index changes.
pub fn discard_root_state(root: &Path) -> Result<usize, BootstrapError> {
    let root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    let live = OpenedSubvolume::open(&root)
        .map_err(|error| BootstrapError::new(format!("open AWACS root: {error}")))?;
    let current = RootInitPaths::from_environment(&root)?;
    let candidates = rebuild_state_candidates(&root, live.subvolume.uuid, &current)?;
    let old_trash = candidates
        .iter()
        .map(|paths| paths.snapshot_trash_dir.clone())
        .collect::<Vec<_>>();
    let mut locked = Vec::new();
    let mut identities = std::collections::HashSet::new();
    for paths in candidates {
        let state_dir = paths.manager_db.parent().expect("state bundle parent");
        let Some(metadata) = existing_state_directory(state_dir)? else {
            continue;
        };
        // Different spellings and bind mounts can refer to the same bundle.
        if !identities.insert((metadata.dev(), metadata.ino())) {
            continue;
        }
        for directory in [&paths.managed_dir, &paths.spool_dir] {
            existing_state_directory(directory)?;
        }
        let lock = lock_root_state_paths_for_cleanup(&paths)?;
        preflight_rebuild_state(&paths)?;
        locked.push((paths, lock));
    }
    // Validate every bundle before retiring any of them. Active consumers or
    // artifacts outside the scheduled cleanup must leave discovery state intact.
    // Trash can outlive its bundle after an earlier interrupted cleanup.
    for trash in old_trash {
        relocate_snapshot_trash(&trash, &current.snapshot_trash_dir)?;
    }
    let mut trashed = 0;
    for (mut paths, lock) in locked {
        crate::gc::mark_retired_lock(&lock.file)
            .map_err(|error| BootstrapError::new(format!("retire AWACS root lock: {error}")))?;
        paths.snapshot_trash_dir = current.snapshot_trash_dir.clone();
        for directory in [&paths.managed_dir, &paths.spool_dir] {
            create_private_directory(directory)?;
        }
        trashed += trash_snapshot_paths_for_rebuild(&paths)?;
        let state_dir = paths.manager_db.parent().expect("state bundle parent");
        // A separately configured managed directory is not below state_dir.
        if !paths.managed_dir.starts_with(state_dir) {
            fs::remove_dir_all(&paths.managed_dir).map_err(|error| {
                BootstrapError::new(format!("remove retired managed snapshots: {error}"))
            })?;
        }
        fs::remove_dir_all(state_dir)
            .map_err(|error| BootstrapError::new(format!("remove retired AWACS state: {error}")))?;
        // The configured parent remains the durable home of broker trash.
        if state_dir.parent() != current.manager_db.parent().and_then(Path::parent) {
            let _ = fs::remove_dir(state_dir.parent().expect("state root parent"));
        }
    }
    Ok(trashed)
}

fn rebuild_state_candidates(
    root: &Path,
    subvolume_uuid: [u8; 16],
    current: &RootInitPaths,
) -> Result<Vec<RootInitPaths>, BootstrapError> {
    let key = Uuid::from_bytes(subvolume_uuid).to_string();
    let mut candidates = vec![current.clone()];
    for state_root in [default_state_root(root)?, hidden_sibling_state_root(root)?] {
        candidates.push(RootInitPaths::for_state_directory(&state_root.join(&key))?);
    }
    candidates.sort_by(|left, right| left.manager_db.cmp(&right.manager_db));
    candidates.dedup_by(|left, right| left.manager_db == right.manager_db);
    Ok(candidates)
}

fn existing_state_directory(path: &Path) -> Result<Option<fs::Metadata>, BootstrapError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(Some(metadata)),
        Ok(_) => Err(BootstrapError::new(format!(
            "AWACS state path {} must be a directory, not a symlink",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(BootstrapError::new(format!(
            "inspect AWACS state {}: {error}",
            path.display()
        ))),
    }
}

fn preflight_rebuild_state(paths: &RootInitPaths) -> Result<(), BootstrapError> {
    match fs::symlink_metadata(&paths.manager_db) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(BootstrapError::new(
                "AWACS manager database must be a regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(BootstrapError::new(format!(
                "inspect AWACS manager: {error}"
            )));
        }
    }
    let connection = rusqlite::Connection::open_with_flags(
        &paths.manager_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|error| BootstrapError::new(format!("open AWACS retention state: {error}")))?;
    // Explicit rebuild is also the repair path for a malformed database. Old
    // clients are fenced through their lock descriptor before it is removed.
    match connection.query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0)) {
        Ok(result) if result == "ok" => {}
        Ok(_) => return Ok(()),
        Err(rusqlite::Error::SqliteFailure(error, _))
            if matches!(
                error.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            ) =>
        {
            return Ok(());
        }
        Err(error) => {
            return Err(BootstrapError::new(format!(
                "inspect AWACS retention state: {error}"
            )));
        }
    }
    if let Some(reason) =
        crate::gc::retirement_blocker(&connection, read_boot_id()?, current_time_ns()?).map_err(
            |error| BootstrapError::new(format!("inspect AWACS active operations: {error}")),
        )?
    {
        return Err(BootstrapError::new(format!(
            "AWACS state is still in use ({reason}); retry the rebuild after the operation finishes"
        )));
    }
    crate::gc::validate_storage_paths(&connection, paths).map_err(|error| {
        BootstrapError::new(format!(
            "cannot rebuild AWACS state {}: {error}; preserve this database and restore its \
             original BTRFS_AWACS_STATE_DIR/BTRFS_AWACS_MANAGED_DIR configuration before \
             retrying, or explicitly retire the recorded external artifacts first; rebuild \
             does not discover or migrate former split storage directories",
            paths.manager_db.display()
        ))
    })?;
    Ok(())
}

/// Preserves already-retired snapshots when their old state parent is about
/// to disappear. Renaming the broker-owned directory needs no access to its
/// contents, and an in-flight broker drain continues through its open fd.
fn relocate_snapshot_trash(old: &Path, current: &Path) -> Result<(), BootstrapError> {
    if old == current || existing_state_directory(old)?.is_none() {
        return Ok(());
    }
    let parent = current.parent().expect("snapshot trash parent");
    create_private_directory(parent)?;
    let old_parent = old.parent().expect("old snapshot trash parent");
    let current_parent = fs::canonicalize(parent)
        .map_err(|error| BootstrapError::new(format!("resolve snapshot trash parent: {error}")))?;
    if fs::canonicalize(old_parent).ok().as_ref() == Some(&current_parent) {
        return Ok(());
    }
    let name = format!(
        "{}-{}",
        current
            .file_name()
            .expect("snapshot trash basename")
            .to_string_lossy(),
        Uuid::new_v4()
    );
    match fs::rename(old, parent.join(&name)) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EXDEV | libc::EACCES | libc::EPERM)
            ) =>
        {
            // Root-owned private trash can reject a cross-parent rename on
            // permissions before the kernel reports the subvolume boundary.
            // The broker validates both parents and handles its protected
            // contents without relaxing snapshot or trash permissions.
            let broker_directory = env::var_os("BTRFS_AWACS_BROKER_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(crate::broker_transport::DEFAULT_BROKER_DIRECTORY)
                });
            let broker = crate::broker_protocol::BrokerClient::connect_endpoint(
                &broker_directory,
                *Uuid::new_v4().as_bytes(),
            )
            .map_err(|error| {
                BootstrapError::new(format!("connect snapshot cleanup broker: {error}"))
            })?;
            let source_parent = File::open(old_parent).map_err(|error| {
                BootstrapError::new(format!("open old snapshot trash parent: {error}"))
            })?;
            let target_parent = File::open(parent).map_err(|error| {
                BootstrapError::new(format!("open new snapshot trash parent: {error}"))
            })?;
            broker
                .rehome_snapshot_trash(
                    source_parent.as_fd(),
                    old.file_name().expect("trash basename").as_bytes(),
                    target_parent.as_fd(),
                    name.as_bytes(),
                )
                .map_err(|error| {
                    BootstrapError::new(format!("relocate retired AWACS snapshots: {error}"))
                })?;
        }
        Err(error) => {
            return Err(BootstrapError::new(format!(
                "relocate retired AWACS snapshots: {error}"
            )));
        }
    }
    for directory in [old_parent, parent] {
        File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                BootstrapError::new(format!("persist retired snapshot relocation: {error}"))
            })?;
    }
    Ok(())
}

/// Retires every old or partially-created managed snapshot through the broker
/// before a caller discards this root's operational database during an
/// explicit rebuild.
///
/// Rebuild deliberately does not open or interpret the old manager database:
/// it creates one throwaway store only to authenticate a cleanup-only broker
/// session, scans physical wrapper directories, then deletes the entire old
/// state tree. Physical Btrfs deletion may continue asynchronously after this
/// returns.
pub fn trash_existing_root_snapshots_for_rebuild(root: &Path) -> Result<usize, BootstrapError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    let paths = RootInitPaths::from_environment(&canonical_root)?;
    trash_snapshot_paths_for_rebuild(&paths)
}

/// Retires snapshots from a Git AWACS state tree that its caller has already
/// detached and decided to discard after copying the replacement metadata.
///
/// Only direct UUID state directories and one layer of explicitly retired
/// worktree state are included. Paths come from this discarded tree, never
/// environment overrides, live worktree directories, or the old database.
/// Physical deletion may remain asynchronous; the caller must confirm that the
/// containing subvolume can be destroyed before reporting completion.
pub fn trash_discarded_git_awacs_state(state_root: &Path) -> Result<usize, BootstrapError> {
    let metadata = fs::symlink_metadata(state_root)
        .map_err(|error| BootstrapError::new(format!("inspect discarded AWACS state: {error}")))?;
    if !metadata.is_dir() {
        return Err(BootstrapError::new(
            "discarded AWACS state must be a directory, not a symlink",
        ));
    }
    let external_root = env::var_os("BTRFS_AWACS_MANAGED_DIR")
        .or_else(|| env::var_os("BTRFS_AWACS_STATE_DIR"))
        .map(PathBuf::from);
    if let Some(external_root) = &external_root {
        create_private_directory(external_root)?;
        relocate_discarded_snapshot_trash(state_root, external_root, true)?;
    }
    let mut state_dirs = Vec::new();
    collect_discarded_state_directories(state_root, true, &mut state_dirs)?;
    let mut trashed = 0usize;
    for state_dir in state_dirs {
        let state_name = state_dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("collected state directory has a UUID basename");
        let mut paths = RootInitPaths {
            managed_dir: state_dir.join("managed"),
            snapshot_trash_dir: state_dir
                .parent()
                .expect("collected state directory has a parent")
                .join(format!(".broker-trash-{state_name}")),
            spool_dir: state_dir.join("spool"),
            manager_db: state_dir.join("manager.sqlite3"),
        };
        for directory in [&paths.managed_dir, &paths.spool_dir] {
            match fs::symlink_metadata(directory) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(BootstrapError::new(format!(
                        "discarded AWACS path {} must be a directory, not a symlink",
                        directory.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    create_private_directory(directory)?;
                }
                Err(error) => {
                    return Err(BootstrapError::new(format!(
                        "inspect discarded AWACS path {}: {error}",
                        directory.display()
                    )));
                }
            }
        }
        if let Some(external_root) = &external_root {
            let external_trash = external_root.join(format!(".broker-trash-{state_name}"));
            relocate_snapshot_trash(&paths.snapshot_trash_dir, &external_trash)?;
            paths.snapshot_trash_dir = external_trash;
        }
        trashed = trashed
            .checked_add(trash_snapshot_paths_for_rebuild(&paths)?)
            .ok_or_else(|| BootstrapError::new("discarded AWACS snapshot count overflow"))?;
    }
    Ok(trashed)
}

fn relocate_discarded_snapshot_trash(
    state_root: &Path,
    external_root: &Path,
    include_retired: bool,
) -> Result<(), BootstrapError> {
    let entries = fs::read_dir(state_root)
        .and_then(|entries| entries.collect::<Result<Vec<_>, _>>())
        .map_err(|error| BootstrapError::new(format!("list discarded snapshot trash: {error}")))?;
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let is_directory = entry
            .file_type()
            .map_err(|error| {
                BootstrapError::new(format!("inspect discarded snapshot trash: {error}"))
            })?
            .is_dir();
        if !is_directory {
            continue;
        }
        if let Some(suffix) = name.strip_prefix(".broker-trash-") {
            let Some(uuid) = suffix
                .get(..36)
                .filter(|uuid| Uuid::parse_str(uuid).is_ok())
            else {
                continue;
            };
            let remainder = &suffix[36..];
            if !remainder.is_empty()
                && !remainder
                    .strip_prefix('-')
                    .is_some_and(|uuid| Uuid::parse_str(uuid).is_ok())
            {
                continue;
            }
            relocate_snapshot_trash(
                &entry.path(),
                &external_root.join(format!(".broker-trash-{uuid}")),
            )?;
        } else if include_retired
            && name
                .strip_prefix(".retired-worktree-")
                .is_some_and(|suffix| {
                    suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        {
            relocate_discarded_snapshot_trash(&entry.path(), external_root, false)?;
        }
    }
    Ok(())
}

fn collect_discarded_state_directories(
    state_root: &Path,
    include_retired: bool,
    state_dirs: &mut Vec<PathBuf>,
) -> Result<(), BootstrapError> {
    let entries = fs::read_dir(state_root)
        .map_err(|error| BootstrapError::new(format!("read discarded AWACS state: {error}")))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            BootstrapError::new(format!("read discarded AWACS state entry: {error}"))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let is_state =
            Uuid::parse_str(name).is_ok_and(|uuid| uuid.hyphenated().to_string() == name);
        let is_retired = include_retired
            && name
                .strip_prefix(".retired-worktree-")
                .is_some_and(|suffix| {
                    suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
                });
        if !is_state && !is_retired {
            continue;
        }
        // DirEntry::file_type does not follow symlinks. In particular, never
        // enter a UUID or retired-state symlink pointing at a live store.
        if !entry
            .file_type()
            .map_err(|error| {
                BootstrapError::new(format!("inspect discarded AWACS state entry: {error}"))
            })?
            .is_dir()
        {
            continue;
        }
        if is_state {
            state_dirs.push(entry.path());
        } else {
            collect_discarded_state_directories(&entry.path(), false, state_dirs)?;
        }
    }
    Ok(())
}

fn trash_snapshot_paths_for_rebuild(paths: &RootInitPaths) -> Result<usize, BootstrapError> {
    let state_dir = paths
        .manager_db
        .parent()
        .ok_or_else(|| BootstrapError::new("AWACS manager database has no state directory"))?;
    let live_subvolume_uuid = state_dir
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| Uuid::parse_str(name).ok())
        .ok_or_else(|| BootstrapError::new("AWACS state directory has no worktree UUID"))?;
    let cleanup_dir = state_dir.join(format!(".rebuild-cleanup-{}", Uuid::new_v4()));
    create_private_directory(&cleanup_dir)?;
    let cleanup_db = cleanup_dir.join("manager.sqlite3");
    let boot_id = read_boot_id()?;
    let metadata = ServiceMetadata::generate(boot_id, current_time_ns()?)
        .map_err(|error| BootstrapError::new(format!("generate rebuild metadata: {error}")))?;
    let store = Store::create(&cleanup_db, &metadata)
        .map_err(|error| BootstrapError::new(format!("create rebuild cleanup store: {error}")))?;
    let config = external_service_config(
        paths.managed_dir.clone(),
        paths.snapshot_trash_dir.clone(),
        paths.spool_dir.clone(),
        boot_id,
    );
    let service = Service::new_external_rebuild_cleanup(store, config)
        .map_err(|error| BootstrapError::new(format!("open rebuild cleanup service: {error}")))?;
    let result = service
        .trash_managed_snapshots_for_rebuild(*live_subvolume_uuid.as_bytes())
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

pub(crate) fn external_service_config(
    managed_dir: PathBuf,
    snapshot_trash_dir: PathBuf,
    spool_dir: PathBuf,
    boot_id: [u8; 16],
) -> ServiceConfig {
    let broker_directory = env::var_os("BTRFS_AWACS_BROKER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(crate::broker_transport::DEFAULT_BROKER_DIRECTORY));
    ServiceConfig::new(managed_dir, spool_dir, boot_id)
        .with_snapshot_trash_directory(snapshot_trash_dir)
        .with_broker_directory(broker_directory)
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
    let lock = lock_root_state_paths_for_cleanup(paths)?;
    let lock_path = paths
        .manager_db
        .parent()
        .expect("state directory")
        .join("root.lock");
    crate::gc::ensure_current_lock(&lock.file, &lock_path)
        .map_err(|error| BootstrapError::new(format!("validate AWACS root lock: {error}")))?;
    Ok(lock)
}

fn lock_root_state_paths_for_cleanup(
    paths: &RootInitPaths,
) -> Result<RootStateLock, BootstrapError> {
    let state_dir = paths
        .manager_db
        .parent()
        .ok_or_else(|| BootstrapError::new("AWACS manager database has no state directory"))?;
    let lock_path = state_dir.join("root.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|error| BootstrapError::new(format!("open AWACS root lock: {error}")))?;
    let opened = file
        .metadata()
        .map_err(|error| BootstrapError::new(format!("inspect AWACS root lock: {error}")))?;
    if !opened.is_file() || opened.uid() != unsafe { libc::geteuid() } || opened.mode() & 0o077 != 0
    {
        return Err(BootstrapError::new(
            "AWACS root lock must be a private owned regular file",
        ));
    }
    // SAFETY: flock only inspects the valid open descriptor and blocks until
    // another cooperating AWACS caller releases this root's state lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(BootstrapError::new(format!(
            "lock AWACS root state: {}",
            std::io::Error::last_os_error()
        )));
    }
    let named = fs::symlink_metadata(&lock_path)
        .map_err(|error| BootstrapError::new(format!("recheck AWACS root lock: {error}")))?;
    if !named.is_file() || named.dev() != opened.dev() || named.ino() != opened.ino() {
        return Err(BootstrapError::new(
            "AWACS root lock changed while acquiring it; retry the operation",
        ));
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

pub(crate) fn read_boot_id() -> Result<[u8; 16], BootstrapError> {
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
    use std::io::Read;
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn missing_default_manager_reports_unset_state_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("checkout");
        let state_root = root.join(".git/awacs");
        let paths =
            RootInitPaths::for_state_directory(&state_root.join(Uuid::from_u128(1).to_string()))
                .unwrap();
        let error = paths
            .require_existing_manager(&root, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("BTRFS_AWACS_STATE_DIR is unset"), "{error}");
        assert!(
            error.contains(paths.manager_db.to_str().unwrap()),
            "{error}"
        );
        assert!(error.contains("state directory used then"), "{error}");
        assert!(!state_root.exists());

        // The override remains optional for an initialized default store.
        fs::create_dir_all(paths.manager_db.parent().unwrap()).unwrap();
        let _store = Store::create(
            &paths.manager_db,
            &ServiceMetadata::generate([1; 16], 1).unwrap(),
        )
        .unwrap();
        paths.require_existing_manager(&root, None).unwrap();
    }

    #[test]
    fn missing_configured_state_directory_is_not_created() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("checkout");
        let state_root = fixture.path().join("external");
        let paths =
            RootInitPaths::for_state_directory(&state_root.join(Uuid::from_u128(1).to_string()))
                .unwrap();
        let error = paths
            .require_existing_manager(&root, Some(&state_root))
            .unwrap_err()
            .to_string();
        assert!(error.contains("BTRFS_AWACS_STATE_DIR points to"), "{error}");
        assert!(error.contains(state_root.to_str().unwrap()), "{error}");
        assert!(error.contains("does not exist"), "{error}");
        assert!(!state_root.exists());

        // An existing directory without this checkout's database gets a
        // different diagnosis, including the exact expected database path.
        fs::create_dir(&state_root).unwrap();
        let error = paths
            .require_existing_manager(&root, Some(&state_root))
            .unwrap_err()
            .to_string();
        assert!(error.contains("BTRFS_AWACS_STATE_DIR is set to"), "{error}");
        assert!(error.contains(state_root.to_str().unwrap()), "{error}");
        assert!(
            error.contains(paths.manager_db.to_str().unwrap()),
            "{error}"
        );
        assert!(!error.contains("does not exist"), "{error}");
        assert_eq!(fs::read_dir(&state_root).unwrap().count(), 0);

        fs::create_dir_all(paths.manager_db.parent().unwrap()).unwrap();
        let _store = Store::create(
            &paths.manager_db,
            &ServiceMetadata::generate([1; 16], 1).unwrap(),
        )
        .unwrap();
        paths
            .require_existing_manager(&root, Some(&state_root))
            .unwrap();
    }

    #[test]
    fn configured_state_path_must_be_a_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let state_root = fixture.path().join("external");
        fs::write(&state_root, b"preserve this file").unwrap();
        let paths =
            RootInitPaths::for_state_directory(&state_root.join(Uuid::from_u128(1).to_string()))
                .unwrap();
        let error = paths
            .require_existing_manager(fixture.path(), Some(&state_root))
            .unwrap_err()
            .to_string();
        assert!(error.contains("BTRFS_AWACS_STATE_DIR points to"), "{error}");
        assert!(error.contains(state_root.to_str().unwrap()), "{error}");
        assert!(error.contains("not a directory"), "{error}");
        assert_eq!(fs::read(state_root).unwrap(), b"preserve this file");
    }

    #[test]
    fn rebuild_candidates_only_include_the_live_uuid_and_own_git_admin_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("linked");
        let git_dir = fixture.path().join("main/.git/worktrees/linked");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            root.join(".git"),
            b"gitdir: ../main/.git/worktrees/linked\n",
        )
        .unwrap();
        let id = Uuid::from_u128(1);
        let current = RootInitPaths::for_state_directory(
            &fixture.path().join("external").join(id.to_string()),
        )
        .unwrap();
        let candidates = rebuild_state_candidates(&root, *id.as_bytes(), &current).unwrap();
        let mut actual = candidates
            .iter()
            .map(|paths| paths.manager_db.clone())
            .collect::<Vec<_>>();
        actual.sort();
        let mut expected = vec![
            current.manager_db,
            git_dir
                .join("awacs")
                .join(id.to_string())
                .join("manager.sqlite3"),
            fixture
                .path()
                .join(".linked-awacs-state")
                .join(id.to_string())
                .join("manager.sqlite3"),
        ];
        expected.sort();
        assert_eq!(actual, expected);
        assert!(!fixture.path().join("external").exists());
    }

    #[test]
    fn rebuild_relocates_pending_trash_to_external_store() {
        let fixture = tempfile::tempdir().unwrap();
        let id = Uuid::from_u128(1);
        let old = fixture
            .path()
            .join("legacy")
            .join(format!(".broker-trash-{id}"));
        let current = fixture
            .path()
            .join("external")
            .join(format!(".broker-trash-{id}"));
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("pending"), b"retained snapshot").unwrap();
        let retained_fd = File::open(old.join("pending")).unwrap();
        relocate_snapshot_trash(&old, &current).unwrap();
        assert!(!old.exists());
        let relocated = fs::read_dir(current.parent().unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            relocated
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!(".broker-trash-{id}-"))
        );
        assert_eq!(
            fs::read(relocated.join("pending")).unwrap(),
            b"retained snapshot"
        );
        assert_eq!(retained_fd.metadata().unwrap().len(), 17);
    }

    #[test]
    fn discarded_trash_without_a_state_bundle_is_relocated_with_bounded_scope() {
        let fixture = tempfile::tempdir().unwrap();
        let state_root = fixture.path().join("discarded");
        let external = fixture.path().join("external");
        let first = state_root.join(format!(".broker-trash-{}", Uuid::from_u128(1)));
        let retired = state_root.join(format!(".retired-worktree-{:032x}", 2));
        let second = retired.join(format!(".broker-trash-{}", Uuid::from_u128(3)));
        let unrelated = state_root
            .join("worktrees/live/awacs")
            .join(format!(".broker-trash-{}", Uuid::from_u128(4)));
        let nested = retired
            .join(format!(".retired-worktree-{:032x}", 5))
            .join(format!(".broker-trash-{}", Uuid::from_u128(6)));
        for path in [&first, &second, &unrelated, &nested] {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("pending"), b"retained").unwrap();
        }
        relocate_discarded_snapshot_trash(&state_root, &external, true).unwrap();
        assert!(!first.exists());
        assert!(!second.exists());
        assert_eq!(fs::read_dir(external).unwrap().count(), 2);
        assert_eq!(fs::read(unrelated.join("pending")).unwrap(), b"retained");
        assert_eq!(fs::read(nested.join("pending")).unwrap(), b"retained");
    }

    #[test]
    fn rebuild_preserves_readable_split_store_until_its_artifacts_are_configured() {
        let fixture = tempfile::tempdir().unwrap();
        let paths = RootInitPaths::for_state_directory(
            &fixture.path().join(Uuid::from_u128(1).to_string()),
        )
        .unwrap();
        for directory in [&paths.managed_dir, &paths.spool_dir] {
            create_private_directory(directory).unwrap();
        }
        let _lock = lock_root_state_paths_for_cleanup(&paths).unwrap();
        let store = Store::create(
            &paths.manager_db,
            &ServiceMetadata::generate(read_boot_id().unwrap(), 1).unwrap(),
        )
        .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO filesystems(id,fs_uuid) VALUES(1,?1)",
                [[2u8; 16].as_slice()],
            )
            .unwrap();
        let internal_snapshot = paths.managed_dir.join("cut-owned/snapshot");
        store.connection().execute(
            "INSERT INTO snapshots(id,filesystem_id,subvol_uuid,root_id,ctransid,otransid,path,readonly,physical_state,created_ns) VALUES(1,1,?1,zeroblob(8),zeroblob(8),zeroblob(8),?2,1,'present',1)",
            rusqlite::params![[3u8; 16].as_slice(), internal_snapshot.as_os_str().as_bytes()],
        ).unwrap();
        // The ordinary colocated layout remains eligible before switching
        // to external state; no live-root lookup or migration is needed.
        preflight_rebuild_state(&paths).unwrap();

        let former_managed = fixture.path().join("former-managed");
        let external_snapshot = former_managed.join("cut-owned/snapshot");
        fs::create_dir_all(&external_snapshot).unwrap();
        fs::write(external_snapshot.join("keep"), b"retained snapshot").unwrap();
        store
            .connection()
            .execute(
                "UPDATE snapshots SET path=?1 WHERE id=1",
                [external_snapshot.as_os_str().as_bytes()],
            )
            .unwrap();
        let wal = paths.manager_db.with_file_name("manager.sqlite3-wal");
        let database_before = fs::read(&paths.manager_db).unwrap();
        let wal_before = fs::read(&wal).unwrap();
        let error = preflight_rebuild_state(&paths).unwrap_err().to_string();
        assert!(error.contains("stored artifact path is outside this state bundle"));
        assert!(error.contains("BTRFS_AWACS_MANAGED_DIR"));
        assert_eq!(fs::read(&paths.manager_db).unwrap(), database_before);
        assert_eq!(fs::read(&wal).unwrap(), wal_before);
        assert!(
            fs::read(paths.manager_db.parent().unwrap().join("root.lock"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fs::read(external_snapshot.join("keep")).unwrap(),
            b"retained snapshot"
        );

        let configured = RootInitPaths {
            managed_dir: former_managed,
            ..paths
        };
        preflight_rebuild_state(&configured).unwrap();
    }

    #[test]
    fn explicit_rebuild_accepts_corrupt_database_but_rejects_symlinks() {
        let fixture = tempfile::tempdir().unwrap();
        let paths = RootInitPaths::for_state_directory(
            &fixture.path().join(Uuid::from_u128(1).to_string()),
        )
        .unwrap();
        fs::create_dir_all(paths.manager_db.parent().unwrap()).unwrap();
        fs::write(&paths.manager_db, b"corrupt manager state").unwrap();
        preflight_rebuild_state(&paths).unwrap();
        assert_eq!(
            fs::read(&paths.manager_db).unwrap(),
            b"corrupt manager state"
        );
        fs::remove_file(&paths.manager_db).unwrap();
        let external = fixture.path().join("external");
        fs::write(&external, b"do not touch").unwrap();
        std::os::unix::fs::symlink(&external, &paths.manager_db).unwrap();
        assert!(
            preflight_rebuild_state(&paths)
                .unwrap_err()
                .to_string()
                .contains("regular file")
        );
        assert_eq!(fs::read(external).unwrap(), b"do not touch");
    }

    #[test]
    fn discarded_state_scan_is_bounded_and_does_not_follow_symlinks() {
        let fixture = tempfile::tempdir().unwrap();
        let state_root = fixture.path().join("awacs");
        let direct = state_root.join(Uuid::from_u128(1).to_string());
        let retired = state_root.join(format!(".retired-worktree-{:032x}", 2));
        let retired_state = retired.join(Uuid::from_u128(3).to_string());
        let nested_retired = retired
            .join(format!(".retired-worktree-{:032x}", 4))
            .join(Uuid::from_u128(5).to_string());
        let live_worktree = state_root
            .join("worktrees/live/awacs")
            .join(Uuid::from_u128(6).to_string());
        let invalid_retirement = state_root
            .join(".retired-worktree-unrecognized")
            .join(Uuid::from_u128(7).to_string());
        let external_state = fixture.path().join("external");
        for path in [
            &direct,
            &retired_state,
            &nested_retired,
            &live_worktree,
            &invalid_retirement,
            &external_state,
        ] {
            fs::create_dir_all(path).unwrap();
        }
        std::os::unix::fs::symlink(
            &external_state,
            state_root.join(Uuid::from_u128(8).to_string()),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            &external_state,
            state_root.join(format!(".retired-worktree-{:032x}", 9)),
        )
        .unwrap();

        let mut state_dirs = Vec::new();
        collect_discarded_state_directories(&state_root, true, &mut state_dirs).unwrap();
        state_dirs.sort();
        let mut expected = vec![direct, retired_state];
        expected.sort();
        assert_eq!(state_dirs, expected);
    }

    #[test]
    fn discarded_state_cleanup_rejects_symbolic_roots_and_managed_directories() {
        let fixture = tempfile::tempdir().unwrap();
        let state_root = fixture.path().join("awacs");
        let state_dir = state_root.join(Uuid::from_u128(1).to_string());
        fs::create_dir_all(&state_dir).unwrap();
        let root_link = fixture.path().join("awacs-link");
        std::os::unix::fs::symlink(&state_root, &root_link).unwrap();
        assert!(
            trash_discarded_git_awacs_state(&root_link)
                .unwrap_err()
                .to_string()
                .contains("not a symlink")
        );

        let live_managed = fixture.path().join("live-managed");
        fs::create_dir(&live_managed).unwrap();
        fs::write(live_managed.join("keep"), b"live state").unwrap();
        std::os::unix::fs::symlink(&live_managed, state_dir.join("managed")).unwrap();
        assert!(
            trash_discarded_git_awacs_state(&state_root)
                .unwrap_err()
                .to_string()
                .contains("not a symlink")
        );
        assert_eq!(fs::read(live_managed.join("keep")).unwrap(), b"live state");
        assert!(!state_dir.join("spool").exists());
    }

    #[test]
    fn descendant_phase_timings_reach_info_subscriber_without_error_details() {
        let mut output = tempfile::tempfile().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .without_time()
            .json()
            .with_writer(Arc::new(output.try_clone().unwrap()))
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(
                timed_descendant_phase("successful phase", || Ok(37)).unwrap(),
                37
            );
            let result: Result<(), BootstrapError> = timed_descendant_phase("failed phase", || {
                Err(BootstrapError::new("private diagnostic must not appear"))
            });
            assert_eq!(
                result.unwrap_err().to_string(),
                "private diagnostic must not appear"
            );
        });

        output.seek(SeekFrom::Start(0)).unwrap();
        let mut text = String::new();
        output.read_to_string(&mut text).unwrap();
        let events = text.lines().collect::<Vec<_>>();
        assert_eq!(
            events.len(),
            2,
            "only INFO completions should be emitted: {text}"
        );
        for event in &events {
            assert!(event.contains("\"level\":\"INFO\""), "{event}");
            assert!(
                event.contains("AWACS descendant initialization phase completed"),
                "{event}"
            );
            assert!(event.contains("\"elapsed_ms\":"), "{event}");
            assert!(event.contains("\"elapsed_us\":"), "{event}");
            assert!(
                event.contains(&format!("\"pid\":{}", std::process::id())),
                "{event}"
            );
        }
        assert!(events[0].contains("\"phase\":\"successful phase\""));
        assert!(events[0].contains("\"success\":true"));
        assert!(events[1].contains("\"phase\":\"failed phase\""));
        assert!(events[1].contains("\"success\":false"));
        assert!(!text.contains("private diagnostic"));
        assert!(!text.contains("\"root\":"));
        assert!(!text.contains("\"error\":"));
    }

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
