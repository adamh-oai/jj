//! Explicit one-time AWACS root initialization.
//!
//! Daemon startup deliberately does not create watches or indexes. Callers
//! must run this bootstrap first, either through awacs init or JJ's explicit
//! subvolume enable workflow.

use crate::btrfs::{filesystem_info, subvolume_info};
use crate::manager::{InitializedWatch, PERMISSION_CUT, PERMISSION_READ, Permissions, Principal};
use crate::service::{InitializeOptions, Service, ServiceConfig};
use crate::snapshot_walk::SnapshotWalkProgress;
use crate::store::{ServiceMetadata, Store};
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootInitPaths {
    pub managed_dir: PathBuf,
    pub spool_dir: PathBuf,
    pub manager_db: PathBuf,
}

impl RootInitPaths {
    pub fn from_environment(root: &Path) -> Result<Self, BootstrapError> {
        let subvolume_uuid = live_subvolume_uuid(root)?;
        Self::for_subvolume_uuid(root, subvolume_uuid)
    }

    fn for_subvolume_uuid(root: &Path, subvolume_uuid: [u8; 16]) -> Result<Self, BootstrapError> {
        let state_home = match env::var_os("XDG_STATE_HOME") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(env::var_os("HOME").ok_or_else(|| {
                BootstrapError::new("HOME is required when XDG_STATE_HOME is unset")
            })?)
            .join(".local/state"),
        };
        let state_root = env::var_os("BTRFS_AWACS_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| state_home.join("btrfs-awacs/roots"));
        let state_dir = state_root.join(Uuid::from_bytes(subvolume_uuid).to_string());
        create_private_directory(&state_dir)?;
        let root_parent = root.parent().ok_or_else(|| {
            BootstrapError::new(format!("AWACS root {} has no parent", root.display()))
        })?;
        let managed_root = env::var_os("BTRFS_AWACS_MANAGED_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root_parent.join(".btrfs-awacs-managed"));
        let managed_dir = managed_root.join(Uuid::from_bytes(subvolume_uuid).to_string());
        create_private_directory(&managed_dir)?;
        let spool_dir = state_dir.join("spool");
        create_private_directory(&spool_dir)?;
        let manager_db = state_dir.join("manager.sqlite3");
        if let Some(parent) = manager_db.parent() {
            create_private_directory(parent)?;
        }
        Ok(Self {
            managed_dir,
            spool_dir,
            manager_db,
        })
    }
}

/// Stable per-live-subvolume key used for state directories and scan sockets.
pub fn root_state_key(root: &Path) -> Result<String, BootstrapError> {
    Ok(Uuid::from_bytes(live_subvolume_uuid(root)?).to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitProgress {
    Phase(&'static str),
    Index(SnapshotWalkProgress),
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

/// Creates the first immutable snapshot, complete path/inode index, and
/// durable watch state for an already-converted Btrfs subvolume root.
pub fn initialize_root(
    root: &Path,
    mut progress: impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    let _root_lock = lock_root_state(&canonical_root)?;
    let now_ns = current_time_ns()?;
    progress(InitProgress::Phase("opening AWACS state"));
    let mut service = open_bootstrap_service(&canonical_root, now_ns)?;
    if service.require_initialized_root(&canonical_root).is_ok() {
        return Err(BootstrapError::new(format!(
            "{} is already initialized",
            canonical_root.display()
        )));
    }
    progress(InitProgress::Phase("creating initial immutable snapshot"));
    let initialized = service
        .initialize_with_index_progress(&canonical_root, &initialize_options(now_ns)?, |counts| {
            progress(InitProgress::Index(counts))
        })
        .map_err(|error| BootstrapError::new(format!("initialize AWACS root: {error}")))?;
    progress(InitProgress::Phase("initial AWACS watch is ready"));
    Ok(initialized)
}

/// Initializes a Btrfs snapshot worktree from its parent's ready path map.
///
/// Unlike the historical lineage adoption path, this creates a new immutable
/// baseline snapshot and a new sequence-zero revision for the child. The
/// copied map is then independent of the parent watch and its snapshots.
pub fn initialize_descendant_root(
    root: &Path,
    mut progress: impl FnMut(InitProgress),
) -> Result<InitializedWatch, BootstrapError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| BootstrapError::new(format!("canonicalize AWACS root: {error}")))?;
    let _root_lock = lock_root_state(&canonical_root)?;
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
    let now_ns = current_time_ns()?;
    progress(InitProgress::Phase("opening AWACS state"));
    let parent_paths = RootInitPaths::for_subvolume_uuid(&canonical_root, parent_uuid)?;
    let parent_store = Store::open(&parent_paths.manager_db).map_err(|error| {
        BootstrapError::new(format!(
            "open parent manager store {}: {error}",
            parent_paths.manager_db.display()
        ))
    })?;
    let seed_revision_id = parent_store
        .descendant_seed_revision(child_filesystem.fs_uuid, parent_uuid)
        .map_err(|error| BootstrapError::new(format!("find parent AWACS path map: {error}")))?
        .ok_or_else(|| {
            BootstrapError::new(format!(
                "{} has no initialized AWACS parent to clone",
                canonical_root.display()
            ))
        })?;
    let seed_index = parent_store
        .load_revision(seed_revision_id)
        .map_err(|error| BootstrapError::new(format!("load parent AWACS path map: {error}")))?;
    let mut service = open_bootstrap_service(&canonical_root, now_ns)?;
    if service.require_initialized_root(&canonical_root).is_ok() {
        return Err(BootstrapError::new(format!(
            "{} is already initialized",
            canonical_root.display()
        )));
    }
    progress(InitProgress::Phase("cloning parent AWACS path map"));
    debug_assert_eq!(child_subvolume_uuid, child_subvolume.uuid);
    let initialized = service
        .initialize_with_index(&canonical_root, &initialize_options(now_ns)?, seed_index)
        .map_err(|error| {
            BootstrapError::new(format!("initialize AWACS snapshot worktree: {error}"))
        })?;
    progress(InitProgress::Phase("initial AWACS watch is ready"));
    Ok(initialized)
}

/// Opens the already-initialized state for one root through the stateless
/// privileged broker.
///
/// Direct scan callers use this instead of discovering or starting a
/// persistent user daemon. Initialization remains an explicit separate
/// operation; opening an absent database is an actionable error.
pub fn open_initialized_root_service(root: &Path) -> Result<Service, BootstrapError> {
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
    let broker_socket = env::var_os("BTRFS_AWACS_BROKER_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/btrfs-awacs/broker.sock"));
    let config = ServiceConfig::new(paths.managed_dir, paths.spool_dir, read_boot_id()?)
        .with_broker_socket(broker_socket);
    let service = Service::new_external(store, config)
        .map_err(|error| BootstrapError::new(format!("open AWACS root service: {error}")))?;
    service
        .require_initialized_root(&canonical_root)
        .map_err(|error| BootstrapError::new(format!("verify AWACS root state: {error}")))?;
    Ok(service)
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
    let config = ServiceConfig::new(paths.managed_dir, paths.spool_dir, boot_id);
    Service::new(store, config)
        .map_err(|error| BootstrapError::new(format!("open AWACS bootstrap service: {error}")))
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
