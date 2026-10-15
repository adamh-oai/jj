use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::IsTerminal;
use std::io::Write;
use std::io::{self};
use std::os::fd::AsFd;
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use btrfs_awacs::bootstrap::InitProgress;
use btrfs_awacs::bootstrap::initialize_descendant_root_with_watch_consumer;
use btrfs_awacs::bootstrap::initialize_root;
use btrfs_awacs::bootstrap::require_retained_descendant_baseline;
use btrfs_awacs::bootstrap::root_state_key;
use btrfs_awacs::bootstrap::state_root_for_root;
use btrfs_awacs::broker::ChangedObjectsExecution;
use btrfs_awacs::broker::ExpectedSubvolume;
use btrfs_awacs::broker::execute_changed_objects;
use btrfs_awacs::broker_protocol::BrokerDispatcher;
use btrfs_awacs::broker_transport::FileBrokerListener;
use btrfs_awacs::btrfs::OpenedSubvolume;
use btrfs_awacs::btrfs::remove_subvolume_tree;
use btrfs_awacs::btrfs::remove_tree;
use btrfs_awacs::btrfs::set_subvolume_readonly;
use btrfs_awacs::btrfs::supports_changed_objects_v2;
use btrfs_awacs::git_index_handoff::GitIndexHandoff;
use btrfs_awacs::git_index_handoff::seed_git_fsmonitor_index;
use btrfs_awacs::git_index_handoff::timed_git_phase;
use btrfs_awacs::manifest::CHANGE_CREATED;
use btrfs_awacs::manifest::CHANGE_DELETED;
use btrfs_awacs::manifest::CHANGE_DIR_ENTRIES;
use btrfs_awacs::manifest::CHANGE_FILE_DATA;
use btrfs_awacs::manifest::CHANGE_INODE;
use btrfs_awacs::manifest::CHANGE_REF;
use btrfs_awacs::manifest::CHANGE_VERITY;
use btrfs_awacs::manifest::CHANGE_XATTR;
use btrfs_awacs::manifest::ChangedObjectsManifest;
use btrfs_awacs::manifest::ChangedObjectsV2;
use btrfs_awacs::manifest::Reference;
use btrfs_awacs::manifest::SubvolumeBoundary;
use btrfs_awacs::manifest::parse_changed_objects_v2;
use btrfs_awacs::recursive_snapshot::recursive_snapshot;
use btrfs_awacs::scan::BeginScanRequest;
use btrfs_awacs::scan::Invalidation;
use btrfs_awacs::scan::ScanClient;
use btrfs_awacs::scan::ScanOutcome;
use btrfs_awacs::scan::SnapshotBaseline;
use btrfs_awacs::scan_facade::DirectScanClient;
use btrfs_awacs::status::RootStatus;
use btrfs_awacs::status::decode_git_fsmonitor_token;
use btrfs_awacs::status::encode_git_fsmonitor_token;
use btrfs_awacs::status::git_index_layout;
use btrfs_awacs::status::read_git_index_extension;
use btrfs_awacs::subvolume_migration::MigrationOptions;
use btrfs_awacs::subvolume_migration::convert_subvolume_root;
use clap::Parser;
use clap::Subcommand as ClapSubcommand;
use clap::ValueEnum;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

const SNAPSHOT_DIR: &str = ".btrfs-awacs";
const SNAPSHOT_PREFIX: &str = "snapshot-";
const MANAGED_SNAPSHOT_CHILD_NAME: &str = "snapshot";
const GIT_FSMONITOR_AWACS: &str = "git-fsmonitor-awacs";

#[derive(Debug, Parser)]
#[command(
    name = "awacs",
    version,
    about = "Btrfs snapshot change index, direct scan client, and benchmark tools"
)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, ClapSubcommand)]
enum CliCommand {
    /// Convert a directory to a Btrfs subvolume and seed its AWACS watch.
    Init {
        #[arg(value_name = "ROOT")]
        root: PathBuf,
        /// Set compression on new subvolumes and rewrite file extents instead
        /// of reflinking them.
        #[arg(long)]
        compress: Option<bool>,
        /// Keep a partial conversion after failure.
        #[arg(long)]
        keep: bool,
    },
    /// Git worktree lifecycle operations backed by Btrfs snapshots and AWACS.
    Git {
        #[command(subcommand)]
        command: GitCommand,
    },
    /// Snapshot a Btrfs subvolume and all of its nested subvolumes.
    Rsnapshot {
        #[arg(value_name = "SRC")]
        source: PathBuf,
        #[arg(value_name = "DST")]
        destination: PathBuf,
    },
    /// Create a timestamped read-only benchmark snapshot.
    Snap {
        #[arg(value_name = "BTRFS_SUBVOLUME")]
        source: PathBuf,
    },
    /// Compare the two newest benchmark snapshots.
    Compare {
        #[arg(value_name = "BTRFS_SUBVOLUME")]
        source: PathBuf,
    },
    /// Dump manager state and verify recorded snapshots against Btrfs.
    Dump {
        /// Initialized AWACS root whose manager state should be inspected.
        #[arg(value_name = "ROOT", default_value = ".")]
        root: PathBuf,
        /// Root-specific manager SQLite database.
        #[arg(long, value_name = "PATH")]
        manager_db: Option<PathBuf>,
        /// Also inspect this managed snapshot directory for unrecorded cuts.
        #[arg(long, value_name = "PATH")]
        managed_dir: Option<PathBuf>,
    },
    /// Inspect AWACS kernel data in a human-readable format.
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
    /// Run the privileged filesystem-operation broker.
    BrokerServe {
        /// Private manager-owned request directory.
        directory: PathBuf,
        manager_uid: u32,
        manager_gid: u32,
        #[arg(value_name = "BTRFS_PROBE_ROOT")]
        probe_root: PathBuf,
    },
}

#[derive(Debug, ClapSubcommand)]
enum DebugCommand {
    /// Compare two read-only snapshots and pretty-print their changed objects.
    ChangedObjects {
        #[arg(value_name = "SOURCE_SNAPSHOT")]
        source_snapshot: PathBuf,
        #[arg(value_name = "TARGET_SNAPSHOT")]
        target_snapshot: PathBuf,
        #[arg(long, hide = true)]
        summary: bool,
    },
}

#[derive(Debug, ClapSubcommand)]
enum GitCommand {
    /// Register, snapshot, initialize, seed, and check out a linked worktree.
    #[command(name = "worktree-add")]
    WorktreeAdd {
        #[arg(long, value_name = "PATH")]
        source: PathBuf,
        #[arg(long, value_name = "PATH")]
        destination: PathBuf,
        #[arg(long = "ref", value_name = "REF")]
        reference: String,
        #[arg(long, default_value = "git", value_name = "PATH")]
        git: PathBuf,
        /// Fail instead of falling back to ordinary Git when snapshotting is
        /// ineligible.
        #[arg(long)]
        required: bool,
        #[arg(long)]
        detach: bool,
        #[arg(long, action = clap::ArgAction::Count)]
        force: u8,
        #[arg(long)]
        relative_paths: bool,
        #[arg(long, value_name = "REASON")]
        lock_reason: Option<String>,
        #[arg(long)]
        quiet: bool,
        /// Validate snapshot eligibility and inherited state without creating
        /// anything.
        #[arg(long)]
        check_only: bool,
        /// Trust a successful preflight performed by the Git caller.
        ///
        /// This is intentionally hidden and only skips the expensive mutable
        /// source eligibility checks. The caller must not use it unless it
        /// already accepted the race between preflight and snapshot creation.
        #[arg(long, hide = true)]
        no_check: bool,
    },
    /// Remove AWACS state and the Btrfs subvolume for a linked worktree.
    #[command(name = "worktree-remove")]
    WorktreeRemove {
        #[arg(long, value_name = "PATH")]
        path: PathBuf,
    },
}

#[derive(Debug)]
struct GitWorktreeAdd<'a> {
    source: &'a Path,
    destination: &'a Path,
    reference: &'a str,
    git: &'a Path,
    required: bool,
    detach: bool,
    force: u8,
    relative_paths: bool,
    lock_reason: Option<&'a str>,
    quiet: bool,
    check_only: bool,
    no_check: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum SnapshotMode {
    Ro,
    Rw,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct ChangedObjectsSummary {
    objects: usize,
    created: usize,
    deleted: usize,
    raw_ref_adds: usize,
    raw_ref_deletes: usize,
    net_ref_adds: usize,
    net_ref_deletes: usize,
}

fn main() {
    if env::args_os()
        .next()
        .as_deref()
        .and_then(|argument| Path::new(argument).file_name())
        == Some(OsStr::new(GIT_FSMONITOR_AWACS))
    {
        let _tracing_guard = init_tracing("git-fsmonitor");
        finish(run_git_fsmonitor_hook());
        return;
    }
    let explicit_command = env::args_os().nth(1);
    let component = match explicit_command.as_deref() {
        Some(command) if command == OsStr::new("broker-serve") => "broker-serve",
        _ => "awacs",
    };
    let _tracing_guard = init_tracing(component);
    run_cli(Cli::parse());
}

fn init_tracing(component: &'static str) -> Option<WorkerGuard> {
    let path = match awacs_log_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("awacs logging disabled: {error}");
            return None;
        }
    };
    let parent = path
        .parent()
        .ok_or_else(|| "log path has no parent".to_owned())
        .ok()?;
    if let Err(error) = fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
    {
        eprintln!(
            "awacs logging disabled: create {}: {error}",
            parent.display()
        );
        return None;
    }
    let filename = path
        .file_name()
        .ok_or_else(|| "log path has no filename".to_owned())
        .ok()?;
    if let Err(error) = OpenOptions::new().create(true).append(true).open(&path) {
        eprintln!("awacs logging disabled: open {}: {error}", path.display());
        return None;
    }
    let appender = tracing_appender::rolling::never(parent, filename);
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("AWACS_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .with_current_span(true)
        .with_span_list(true)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .finish();
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        eprintln!("awacs logging disabled: subscriber already initialized");
        return None;
    }
    info!(
        component,
        pid = std::process::id(),
        ppid = unsafe { libc::getppid() },
        log_path = %path.display(),
        "process started"
    );
    Some(guard)
}

fn awacs_log_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("AWACS_LOG_FILE") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("AWACS_LOG_FILE must be an absolute path".to_owned());
        }
        return Ok(path);
    }
    let state_home =
        match env::var_os("XDG_STATE_HOME") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(env::var_os("HOME").ok_or_else(|| {
                "HOME or XDG_STATE_HOME is required for default log path".to_owned()
            })?)
            .join(".local/state"),
        };
    Ok(state_home.join("btrfs-awacs/awacs.log"))
}

fn run_cli(cli: Cli) {
    match cli.command {
        CliCommand::Init {
            root,
            compress,
            keep,
        } => finish(run_init(&root, compress, keep)),
        CliCommand::Git { command } => match command {
            GitCommand::WorktreeAdd {
                source,
                destination,
                reference,
                git,
                required,
                detach,
                force,
                relative_paths,
                lock_reason,
                quiet,
                check_only,
                no_check,
            } => finish(run_git_worktree_add(&GitWorktreeAdd {
                source: &source,
                destination: &destination,
                reference: &reference,
                git: &git,
                required,
                detach,
                force,
                relative_paths,
                lock_reason: lock_reason.as_deref(),
                quiet,
                check_only,
                no_check,
            })),
            GitCommand::WorktreeRemove { path } => finish(run_git_worktree_remove(&path)),
        },
        CliCommand::Rsnapshot {
            source,
            destination,
        } => finish(recursive_snapshot(&source, &destination).map_err(|error| {
            format!(
                "recursively snapshot {} to {}: {error}",
                source.display(),
                destination.display()
            )
        })),
        CliCommand::Snap { source } => finish_timed(|| run_benchmark(&source, true)),
        CliCommand::Compare { source } => finish_timed(|| run_benchmark(&source, false)),
        CliCommand::Dump {
            root,
            manager_db,
            managed_dir,
        } => finish(run_dump(
            &root,
            manager_db.as_deref(),
            managed_dir.as_deref(),
        )),
        CliCommand::Debug { command } => match command {
            DebugCommand::ChangedObjects {
                source_snapshot,
                target_snapshot,
                summary,
            } => finish(run_debug_changed_objects(
                &source_snapshot,
                &target_snapshot,
                summary,
            )),
        },
        CliCommand::BrokerServe {
            directory,
            manager_uid,
            manager_gid,
            probe_root,
        } => finish(run_broker_server(
            directory,
            manager_uid,
            manager_gid,
            probe_root,
        )),
    }
}

fn run_init(root: &Path, compress: Option<bool>, keep: bool) -> Result<(), String> {
    // Conversion replaces the root when it is an ordinary directory, or just
    // the ordinary metadata directories when the root is already a subvolume.
    // Replacing `.git` recursively snapshots its managed cuts with new UUIDs,
    // while linked worktrees still depend on the original recorded identities.
    let preserves_git_metadata =
        OpenedSubvolume::open(root).is_ok() && OpenedSubvolume::open(&root.join(".git")).is_ok();
    require_safe_init_git_metadata(root, preserves_git_metadata)?;
    let interactive = io::stderr().is_terminal();
    let committed = convert_subvolume_root(
        root,
        MigrationOptions {
            compression: compress,
            keep_temporary_on_drop: keep,
        },
        |phase| {
            if interactive {
                eprintln!("awacs: {phase}...");
            }
        },
    )
    .map_err(|error| format!("convert AWACS root: {error}"))?;
    let initialized = initialize_root(root, |progress| {
        if !interactive {
            return;
        }
        match progress {
            InitProgress::Phase(phase) => eprintln!("awacs: {phase}..."),
        }
    })
    .map_err(|error| format!("seed AWACS root: {error}"))?;
    if interactive {
        eprintln!();
    }
    committed
        .discard_displaced()
        .map_err(|error| format!("remove displaced AWACS root: {error}"))?;
    println!(
        "initialized watch={} snapshot={}",
        Uuid::from_bytes(initialized.watch_id),
        initialized.snapshot_id,
    );
    Ok(())
}

fn require_safe_init_git_metadata(root: &Path, preserves_git_metadata: bool) -> Result<(), String> {
    if preserves_git_metadata {
        return Ok(());
    }
    let git_directory = root.join(".git");
    match fs::symlink_metadata(&git_directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("inspect Git metadata before conversion: {error}")),
        // Linked-worktree gitfiles point outside the converted root. Metadata
        // symlinks are rejected by the conversion helper itself.
        Ok(metadata) if !metadata.is_dir() => return Ok(()),
        Ok(_) => {}
    }
    let worktrees = git_directory.join("worktrees");
    match fs::symlink_metadata(&worktrees) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "inspect linked Git worktrees before conversion: {error}"
            ));
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "cannot safely inspect linked Git worktree metadata at {} before conversion",
                worktrees.display()
            ));
        }
    }
    for entry in fs::read_dir(&worktrees)
        .map_err(|error| format!("inspect linked Git worktrees before conversion: {error}"))?
    {
        let entry = entry
            .map_err(|error| format!("inspect linked Git worktree before conversion: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspect linked Git worktree type: {error}"))?;
        if file_type.is_symlink() {
            return Err(format!(
                "cannot safely inspect symbolic-link Git worktree metadata at {} before conversion",
                entry.path().display()
            ));
        }
        if !file_type.is_dir() {
            continue;
        }
        let awacs = entry.path().join("awacs");
        match fs::symlink_metadata(&awacs) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("inspect linked worktree AWACS state: {error}"));
            }
            Ok(_) => {
                return Err(format!(
                    "cannot convert Git metadata while linked-worktree AWACS state remains at {}; \
                     recursive snapshots would change its recorded snapshot identities",
                    awacs.display()
                ));
            }
        }
    }
    Ok(())
}

const GIT_AWACS_BYPASS: &str = "GIT_AWACS_BYPASS";
const AWACS_WORKTREE_MARKER: &str = "awacs-worktree";
const AWACS_JJ_PENDING_CONSUMER_MARKER: &str = "awacs-jj-pending-consumer";
const AWACS_JJ_PENDING_WORKING_COPY_STATE: &str = "awacs-jj-pending-working-copy";
const JJ_SUBVOLUME_MODE_MARKER: &str = ".jj/working_copy/subvolume_mode";
const JJ_AWACS_ADOPTION_SEED_MARKER: &str = ".jj/working_copy/awacs-adoption-seed";
const JJ_WORKING_COPY_LOCK: &str = ".jj/working_copy/working_copy.lock";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingJjSourceSeed {
    baseline_filesystem_uuid: [u8; 16],
    baseline_snapshot_uuid: [u8; 16],
    baseline_owner_id: [u8; 16],
}

struct JjWorkingCopyLock {
    path: PathBuf,
    file: File,
}

impl Drop for JjWorkingCopyLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        // SAFETY: unlocking the same still-open descriptor is best-effort
        // cleanup and has no memory-safety precondition.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| format!("read current directory: {error}"))
}

/// Resolves as much of a destination path as already exists, then normalizes
/// the remaining components without requiring the destination to exist yet.
///
/// `git worktree add ../sibling` passes the literal `..` through to AWACS.
/// Comparing that spelling directly with a canonical source path makes the
/// sibling look like a descendant of the source.
fn canonical_path_for_creation(path: &Path) -> Result<PathBuf, String> {
    let absolute = absolute_path(path)?;
    let mut ancestor = absolute.as_path();
    let mut missing_components = Vec::new();

    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                while let Some(component) = missing_components.pop() {
                    canonical.push(component);
                }
                return normalize_absolute_path(&canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let component = ancestor.file_name().ok_or_else(|| {
                    format!(
                        "find existing ancestor for destination {}",
                        absolute.display()
                    )
                })?;
                missing_components.push(component.to_owned());
                ancestor = ancestor.parent().ok_or_else(|| {
                    format!(
                        "find existing ancestor for destination {}",
                        absolute.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "canonicalize destination ancestor {}: {error}",
                    ancestor.display()
                ));
            }
        }
    }
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf, String> {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !normalized.has_root() {
                    return Err(format!("normalize destination path {}", path.display()));
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }
    Ok(normalized)
}

fn git_capture(git: &Path, cwd: &Path, arguments: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new(git)
        .arg("-C")
        .arg(cwd)
        .args(arguments)
        .env(GIT_AWACS_BYPASS, "1")
        .output()
        .map_err(|error| format!("run git {}: {error}", arguments.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git {} failed{}{}",
            arguments.join(" "),
            if stderr.is_empty() { "" } else { ": " },
            stderr.trim_end(),
        ));
    }
    if !output.stderr.is_empty() {
        io::stderr()
            .lock()
            .write_all(&output.stderr)
            .map_err(|error| format!("forward Git diagnostics: {error}"))?;
    }
    Ok(output.stdout)
}

fn git_status(
    git: &Path,
    cwd: &Path,
    arguments: &[OsString],
    extra_env: &[(&str, &OsStr)],
) -> Result<(), String> {
    let mut command = Command::new(git);
    command
        .arg("-C")
        .arg(cwd)
        .args(arguments)
        .env(GIT_AWACS_BYPASS, "1");
    for (name, value) in extra_env {
        command.env(name, value);
    }
    let status = command
        .status()
        .map_err(|error| format!("run git: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("git exited with {status}"))
    }
}

fn git_path(git: &Path, cwd: &Path, name: &str) -> Result<PathBuf, String> {
    let output = git_capture(git, cwd, &["rev-parse", "--git-path", name])?;
    let value = String::from_utf8(output)
        .map_err(|error| format!("decode git path for {name}: {error}"))?;
    let path = PathBuf::from(value.trim_end());
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn git_object_hash_len(git: &Path, source: &Path) -> Result<usize, String> {
    let output = git_capture(git, source, &["rev-parse", "--show-object-format"])?;
    match output.strip_suffix(b"\n").unwrap_or(&output) {
        b"sha1" => Ok(20),
        b"sha256" => Ok(32),
        other => Err(format!(
            "unsupported Git object format {}",
            String::from_utf8_lossy(other)
        )),
    }
}

/// Takes JJ's normal exclusive working-copy lock when this is a JJ checkout.
///
/// Holding the same lock JJ uses for journal transitions keeps the handoff and
/// compact journal consistent with the recursive snapshot. Retain the lock
/// until the compact source journal has also been staged outside the child.
/// Later child initialization uses the recorded immutable baseline revision.
fn lock_jj_working_copy_for_snapshot(source: &Path) -> Result<Option<JjWorkingCopyLock>, String> {
    let state_path = source.join(".jj/working_copy");
    if !state_path.is_dir() {
        return Ok(None);
    }
    let path = source.join(JJ_WORKING_COPY_LOCK);
    loop {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| format!("open JJ working-copy lock {}: {error}", path.display()))?;
        // SAFETY: flock only inspects the valid open descriptor and blocks
        // until JJ or another AWACS worktree creator releases this path.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "lock JJ working-copy state {}: {}",
                path.display(),
                io::Error::last_os_error()
            ));
        }
        let metadata = file
            .metadata()
            .map_err(|error| format!("inspect JJ working-copy lock {}: {error}", path.display()))?;
        if metadata.nlink() != 0 {
            return Ok(Some(JjWorkingCopyLock { path, file }));
        }
        // JJ removes the old path immediately before unlocking. If we opened
        // that unlinked inode, retry so our lock remains visible to the next
        // JJ writer rather than protecting only the stale file descriptor.
        // SAFETY: see the matching unlock in Drop.
        let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Reads the JJ-owned handoff only for a committed snapshot-backed source.
///
/// Ordinary Git repositories and ordinary JJ repositories have no subvolume
/// marker and keep the stock Git worktree behavior. Once JJ has enabled strict
/// snapshot mode, however, a Git-created descendant must carry the exact
/// committed semantic-tree baseline that delayed workspace adoption will
/// consume. Refusing a missing or transitional handoff here is important:
/// this function runs before Git creates a branch or worktree registration.
fn read_pending_jj_source_seed(source: &Path) -> Result<Option<PendingJjSourceSeed>, String> {
    let mode_path = source.join(JJ_SUBVOLUME_MODE_MARKER);
    let mode = match fs::read(&mode_path) {
        Ok(mode) => mode,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "read JJ snapshot mode marker {}: {error}",
                mode_path.display()
            ));
        }
    };
    if mode != b"snapshot-backed\n" {
        return Err(
            "source JJ snapshot mode is not committed; finish or rebuild its AWACS baseline \
             before creating a Git worktree"
                .to_owned(),
        );
    }
    let path = source.join(JJ_AWACS_ADOPTION_SEED_MARKER);
    let contents = fs::read_to_string(&path).map_err(|error| {
        format!(
            "source JJ workspace has no transferable AWACS adoption seed at {}: {error}; run jj \
             status to publish a clean baseline",
            path.display()
        )
    })?;
    let mut fields = contents.trim_end().split(':');
    let format = fields.next();
    if !matches!(
        format,
        Some("jj-awacs-adoption-v1" | "jj-awacs-adoption-v2")
    ) {
        return Err("source JJ AWACS adoption seed has an unsupported format".to_owned());
    }
    let mut parse_uuid = |name: &str| -> Result<[u8; 16], String> {
        let value = fields
            .next()
            .ok_or_else(|| format!("source JJ AWACS adoption seed lacks {name}"))?;
        Uuid::parse_str(value)
            .map(|uuid| *uuid.as_bytes())
            .map_err(|error| format!("source JJ AWACS adoption seed has invalid {name}: {error}"))
    };
    let baseline_filesystem_uuid = parse_uuid("filesystem identity")?;
    let baseline_snapshot_uuid = parse_uuid("snapshot identity")?;
    let baseline_owner_id = if format == Some("jj-awacs-adoption-v2") {
        parse_uuid("baseline owner identity")?
    } else {
        read_v1_jj_baseline_owner(source)?
    };
    if fields.next().is_some() {
        return Err("source JJ AWACS adoption seed has trailing fields".to_owned());
    }
    Ok(Some(PendingJjSourceSeed {
        baseline_filesystem_uuid,
        baseline_snapshot_uuid,
        baseline_owner_id,
    }))
}

const JJ_WORKING_COPY_STATE_MAGIC: &[u8] = b"\0JJ-WORKING-COPY-STATE\0v1\n";

/// Reads the owner field from the compact JJ journal for v1 handoffs written
/// before the owner was added to the small adoption marker.
fn read_v1_jj_baseline_owner(source: &Path) -> Result<[u8; 16], String> {
    let path = source.join(".jj/working_copy/checkout");
    let bytes = fs::read(&path).map_err(|error| {
        format!(
            "read source JJ working-copy journal {}: {error}",
            path.display()
        )
    })?;
    let proto = bytes
        .strip_prefix(JJ_WORKING_COPY_STATE_MAGIC)
        .ok_or_else(|| {
            format!(
                "source JJ adoption seed predates owner IDs and {} is not a compact working-copy \
                 journal",
                path.display()
            )
        })?;
    let owner = protobuf_length_delimited_field(proto, 18)?.ok_or_else(|| {
        "source JJ adoption seed predates owner IDs and its working-copy journal has no baseline \
         owner"
            .to_owned()
    })?;
    owner.try_into().map_err(|_| {
        "source JJ working-copy journal has an invalid baseline owner identity".to_owned()
    })
}

fn protobuf_length_delimited_field<'a>(
    bytes: &'a [u8],
    wanted_field: u64,
) -> Result<Option<&'a [u8]>, String> {
    let mut offset = 0;
    while offset < bytes.len() {
        let key = read_protobuf_varint(bytes, &mut offset)?;
        let field = key >> 3;
        let wire_type = key & 7;
        match wire_type {
            0 => {
                read_protobuf_varint(bytes, &mut offset)?;
            }
            1 => advance_protobuf(bytes, &mut offset, 8)?,
            2 => {
                let length = usize::try_from(read_protobuf_varint(bytes, &mut offset)?)
                    .map_err(|_| "source JJ working-copy journal field is too large".to_owned())?;
                let start = offset;
                advance_protobuf(bytes, &mut offset, length)?;
                if field == wanted_field {
                    return Ok(Some(&bytes[start..offset]));
                }
            }
            5 => advance_protobuf(bytes, &mut offset, 4)?,
            _ => {
                return Err(format!(
                    "source JJ working-copy journal has unsupported protobuf wire type {wire_type}"
                ));
            }
        }
    }
    Ok(None)
}

fn read_protobuf_varint(bytes: &[u8], offset: &mut usize) -> Result<u64, String> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes
            .get(*offset)
            .ok_or_else(|| "source JJ working-copy journal has a truncated varint".to_owned())?;
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("source JJ working-copy journal has an oversized varint".to_owned())
}

fn advance_protobuf(bytes: &[u8], offset: &mut usize, length: usize) -> Result<(), String> {
    let end = offset
        .checked_add(length)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| "source JJ working-copy journal has a truncated field".to_owned())?;
    *offset = end;
    Ok(())
}

fn git_worktree_args(options: &GitWorktreeAdd<'_>, no_checkout: bool) -> Vec<OsString> {
    let mut arguments = vec![OsString::from("worktree"), OsString::from("add")];
    if no_checkout {
        arguments.push(OsString::from("--no-checkout"));
    }
    if options.detach {
        arguments.push(OsString::from("--detach"));
    }
    for _ in 0..options.force {
        arguments.push(OsString::from("--force"));
    }
    if options.relative_paths {
        arguments.push(OsString::from("--relative-paths"));
    }
    if let Some(reason) = options.lock_reason {
        arguments.push(OsString::from("--lock"));
        arguments.push(OsString::from("--reason"));
        arguments.push(OsString::from(reason));
    }
    if options.quiet {
        arguments.push(OsString::from("--quiet"));
    }
    arguments.push(options.destination.as_os_str().to_owned());
    arguments.push(OsString::from(options.reference));
    arguments
}

fn snapshot_ineligible(options: &GitWorktreeAdd<'_>, reason: &str) -> Result<bool, String> {
    if options.required {
        Err(reason.to_owned())
    } else {
        eprintln!("AWACS: {reason}; falling back to ordinary git worktree add");
        Ok(true)
    }
}

fn reject_jj_conflicted_git_target(
    git: &Path,
    source: &Path,
    reference: &str,
) -> Result<(), String> {
    let entries = git_capture(git, source, &["ls-tree", "--name-only", "-z", reference])?;
    if entries.split(|byte| *byte == 0).any(|entry| {
        entry == b"JJ-CONFLICT-README"
            || entry.starts_with(b".jjconflict-base-")
            || entry.starts_with(b".jjconflict-side-")
    }) {
        return Err(format!(
            "cannot check out conflicted JJ commit {reference}: its .jjconflict-* trees would \
             materialize complete repository copies; resolve the conflicts or choose an \
             unconflicted revision"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SourceGitChanges {
    tracked_paths: Vec<Vec<u8>>,
    untracked_paths: Vec<Vec<u8>>,
}

fn source_git_changes(options: &GitWorktreeAdd<'_>) -> Result<SourceGitChanges, String> {
    let status = git_capture(
        options.git,
        options.source,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
    )?;
    parse_source_git_changes(&status)
}

fn parse_source_git_changes(status: &[u8]) -> Result<SourceGitChanges, String> {
    let mut changes = SourceGitChanges::default();
    let mut records = status
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 || record[2] != b' ' {
            return Err("decode Git porcelain status record".to_owned());
        }
        let state = &record[..2];
        let path = record[3..].to_vec();
        if state == b"??" {
            changes.untracked_paths.push(path);
            continue;
        }
        if state == b"!!" {
            continue;
        }
        changes.tracked_paths.push(path);
        if state.iter().any(|state| matches!(state, b'R' | b'C')) {
            let source_path = records
                .next()
                .ok_or_else(|| "decode Git porcelain rename source path".to_owned())?;
            changes.tracked_paths.push(source_path.to_vec());
        }
    }
    Ok(changes)
}

fn validated_relative_git_path(path: &[u8]) -> Result<PathBuf, String> {
    use std::path::Component;

    let path = PathBuf::from(OsString::from_vec(path.to_vec()));
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(format!(
            "Git reported a path outside the worktree: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn remove_copied_untracked_paths(destination: &Path, paths: &[Vec<u8>]) -> Result<(), String> {
    for path in paths {
        let path = destination.join(validated_relative_git_path(path)?);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "inspect copied untracked path {}: {error}",
                    path.display()
                ));
            }
        };
        if metadata.is_dir() {
            remove_tree(&path).map_err(|error| {
                format!(
                    "remove copied untracked directory {}: {error}",
                    path.display()
                )
            })?;
        } else {
            fs::remove_file(&path).map_err(|error| {
                format!("remove copied untracked path {}: {error}", path.display())
            })?;
        }
    }
    Ok(())
}

fn reset_checkout_to_target(git: &Path, destination: &Path, target: &str) -> Result<(), String> {
    git_status(
        git,
        destination,
        &[
            OsString::from("reset"),
            OsString::from("--hard"),
            OsString::from("--quiet"),
            OsString::from(target.trim_end()),
        ],
        &[],
    )
}

/// Finishes a snapshot-backed checkout by using the copied source index as
/// Git's baseline for a hard reset to the requested target.
///
/// The Btrfs snapshot already copied the source worktree, and its stat-warm
/// index has been seeded with the destination-owned fsmonitor cursor while
/// retaining the source dirty bitmap. Remove the copied untracked paths and
/// directories first, then let Git reconcile tracked files without a large
/// literal-pathspec match or statting every unchanged entry.
fn materialize_snapshot_checkout(
    git: &Path,
    destination: &Path,
    target: &str,
    source_changes: &SourceGitChanges,
) -> Result<(), String> {
    // Normal-mode status can report a whole untracked directory. Removing it
    // also removes any ignored output it contains, while ignored-only paths
    // outside reported untracked directories remain available for reuse.
    timed_git_phase("remove_copied_untracked", || {
        remove_copied_untracked_paths(destination, &source_changes.untracked_paths)
    })?;
    timed_git_phase("reset_destination", || {
        reset_checkout_to_target(git, destination, target)
    })
}

/// Checks ordinary index entries without invoking an index-reading Git command.
/// `ls-files --stage` formats every path and can refresh fsmonitor just to ask
/// whether any mode is a gitlink. Required extensions (notably split indexes)
/// and sparse-directory entries need Git's interpretation, so retain the
/// authoritative command for those formats and for unreadable/malformed files.
fn source_index_has_submodules(git: &Path, source: &Path) -> Result<bool, String> {
    let direct = (|| {
        let index_path = git_path(git, source, "index")?;
        let hash_len = git_object_hash_len(git, source)?;
        let bytes =
            fs::read(&index_path).map_err(|error| format!("read source Git index: {error}"))?;
        let layout = git_index_layout(&bytes, hash_len)?;
        Ok::<_, String>(
            (!layout.has_required_extensions() && !layout.has_sparse_directories)
                .then_some(layout.has_gitlinks),
        )
    })();
    match direct {
        Ok(Some(has_submodules)) => return Ok(has_submodules),
        Ok(None) => {}
        Err(error) => tracing::debug!(
            %error,
            "delegating source index eligibility to Git"
        ),
    }
    let stage = git_capture(git, source, &["ls-files", "--stage", "-z"])?;
    Ok(stage
        .split(|byte| *byte == 0)
        .any(|entry| entry.starts_with(b"160000 ")))
}

/// The snapshot handoff uses one worktree's own index as the baseline for
/// another. Git's repository overrides are valid for ordinary Git commands,
/// but would make this cross-worktree copy ambiguous. Git itself exports
/// GIT_PREFIX for commands run inside a worktree; it describes the caller's
/// original directory and does not redirect the repository or its index.
fn snapshot_git_environment_is_safe() -> bool {
    [
        "GIT_INDEX_FILE",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_SHALLOW_FILE",
        "GIT_GRAFT_FILE",
        "GIT_REPLACE_REF_BASE",
        "GIT_QUARANTINE_PATH",
        "GIT_CONFIG",
    ]
    .iter()
    .all(|name| env::var_os(name).is_none())
}

/// Verify that the Git administrative directory belongs to this worktree.
/// Linked worktrees use a gitfile and an administrative `gitdir` backreference;
/// both paths may be relative. A gitfile pointing at a different repository
/// can otherwise make Git report the requested root while using a foreign index.
fn snapshot_git_metadata_matches(source: &Path, git_dir: &Path) -> bool {
    let marker = source.join(".git");
    let opened = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&marker)
    {
        Ok(file) => file,
        Err(_) => return false,
    };
    let Ok(metadata) = opened.metadata() else {
        return false;
    };
    let associated = if metadata.is_dir() {
        git_dir.to_owned()
    } else if metadata.is_file() {
        let Ok(backreference) = fs::read(git_dir.join("gitdir")) else {
            return false;
        };
        let backreference = PathBuf::from(OsString::from_vec(
            backreference
                .strip_suffix(b"\n")
                .unwrap_or(&backreference)
                .to_vec(),
        ));
        if backreference.is_absolute() {
            backreference
        } else {
            git_dir.join(backreference)
        }
    } else {
        return false;
    };
    let (Ok(marker_path), Ok(associated_path), Ok(associated_metadata)) = (
        fs::canonicalize(&marker),
        fs::canonicalize(&associated),
        fs::metadata(&associated),
    ) else {
        return false;
    };
    marker_path == associated_path
        && metadata.dev() == associated_metadata.dev()
        && metadata.ino() == associated_metadata.ino()
}

/// Git may use a different worktree through repository configuration even
/// when no environment override is present. Only snapshot the tree whose
/// index Git actually describes.
fn snapshot_git_worktree_matches(git: &Path, source: &Path) -> Result<bool, String> {
    let output = git_capture(git, source, &["rev-parse", "--show-toplevel"])?;
    let path = PathBuf::from(OsString::from_vec(
        output.strip_suffix(b"\n").unwrap_or(&output).to_vec(),
    ));
    let worktree =
        fs::canonicalize(path).map_err(|error| format!("canonicalize Git worktree: {error}"))?;
    if worktree != source {
        return Ok(false);
    }
    let output = git_capture(git, source, &["rev-parse", "--absolute-git-dir"])?;
    let git_dir = PathBuf::from(OsString::from_vec(
        output.strip_suffix(b"\n").unwrap_or(&output).to_vec(),
    ));
    Ok(snapshot_git_metadata_matches(source, &git_dir))
}

/// Open the source index without following a symlink. The returned descriptor
/// is used for the handoff so a later path replacement cannot redirect it.
fn open_snapshot_source_git_index(index: &Path) -> Result<Option<File>, String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(index)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(error) => return Err(format!("open source Git index: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect source Git index: {error}"))?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Ok(None);
    }
    Ok(Some(file))
}

fn fallback_git_worktree_add(options: &GitWorktreeAdd<'_>, reason: &str) -> Result<(), String> {
    snapshot_ineligible(options, reason)?;
    if options.check_only {
        return Ok(());
    }
    git_status(
        options.git,
        options.source,
        &git_worktree_args(options, false),
        &[],
    )
}

fn snapshot_eligibility(options: &GitWorktreeAdd<'_>) -> Result<bool, String> {
    if options.destination.exists() {
        return snapshot_ineligible(options, "snapshot destination already exists");
    }
    if options.destination.starts_with(options.source) {
        return snapshot_ineligible(
            options,
            "snapshot destination cannot be inside the source worktree",
        );
    }
    let source_metadata = fs::metadata(options.source)
        .map_err(|error| format!("inspect source worktree: {error}"))?;
    if source_metadata.ino() != 256 {
        return snapshot_ineligible(options, "source worktree is not a Btrfs subvolume");
    }
    if source_index_has_submodules(options.git, options.source)? {
        return snapshot_ineligible(options, "source contains submodules");
    }
    for name in ["index.sparse", "core.sparseCheckout"] {
        let output = Command::new(options.git)
            .arg("-C")
            .arg(options.source)
            .args(["config", "--bool", name])
            .env(GIT_AWACS_BYPASS, "1")
            .output()
            .map_err(|error| format!("read {name}: {error}"))?;
        if output.status.success() && output.stdout.starts_with(b"true") {
            return snapshot_ineligible(options, "sparse indexes are not supported");
        }
    }
    Ok(false)
}

fn remove_copied_dotgit(destination: &Path) -> Result<(), String> {
    let dotgit = destination.join(".git");
    remove_tree(&dotgit).map_err(|error| format!("remove copied .git: {error}"))
}

/// Saves the locked source's compact JJ state outside the child subvolume, then
/// removes copied JJ metadata before AWACS records the child baseline.
///
/// Read the journal from the source while its working-copy lock still protects
/// the handoff and recursive snapshot. Remove the copied `.jj` tree, including
/// its nested subvolumes, before establishing the child baseline so delayed
/// adoption does not produce a large metadata deletion.
fn stage_pending_jj_working_copy_state(
    source: &Path,
    destination: &Path,
    pending_state_path: &Path,
    source_lock: &JjWorkingCopyLock,
) -> Result<(), String> {
    if source_lock.path != source.join(JJ_WORKING_COPY_LOCK) {
        return Err("JJ working-copy lock does not protect the journal source".to_owned());
    }
    timed_git_phase("stage_jj_journal", || {
        let source_state_path = source.join(".jj/working_copy");
        if !source_state_path.is_dir() {
            return Err(format!(
                "source JJ working-copy state is absent at {}",
                source_state_path.display()
            ));
        }
        if pending_state_path.exists() {
            remove_tree(pending_state_path).map_err(|error| {
                format!(
                    "remove stale pending JJ working-copy state {}: {error}",
                    pending_state_path.display()
                )
            })?;
        }
        fs::create_dir(pending_state_path).map_err(|error| {
            format!(
                "create pending JJ working-copy state {}: {error}",
                pending_state_path.display()
            )
        })?;
        let mut copied_journal = false;
        for name in ["checkout", "working_copy_state", "subvolume_mode", "type"] {
            let source_file = source_state_path.join(name);
            if !source_file.is_file() {
                continue;
            }
            fs::copy(&source_file, pending_state_path.join(name)).map_err(|error| {
                format!(
                    "copy pending JJ working-copy state {}: {error}",
                    source_file.display()
                )
            })?;
            copied_journal |= matches!(name, "checkout" | "working_copy_state");
        }
        if !copied_journal {
            return Err(format!(
                "source JJ working-copy state at {} has no committed journal",
                source_state_path.display()
            ));
        }
        Ok(())
    })?;
    timed_git_phase("remove_copied_jj", || {
        match remove_copied_jj_metadata(destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    })
    .map_err(|error| format!("remove copied JJ metadata: {error}"))
}

fn remove_copied_jj_metadata(destination: &Path) -> io::Result<()> {
    let started = Instant::now();
    let open_directory = |path: &Path| {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
    };
    let destination_directory = open_directory(destination)?;
    let destination_fd_path = PathBuf::from(format!(
        "/proc/self/fd/{}",
        destination_directory.as_raw_fd()
    ));
    if fs::metadata(&destination_fd_path).is_err() {
        // No identity below the destination has been captured yet. The shared
        // remover still verifies mounts and destroys any nested subvolumes.
        return remove_tree(&destination.join(".jj"));
    }
    let metadata_path = destination_fd_path.join(".jj");
    let Ok(metadata) = open_directory(&metadata_path) else {
        return remove_tree(&metadata_path);
    };
    // Never enumerate a metadata subvolume before its destroy ioctl. An
    // inode-256 false positive on another filesystem only skips parallelism.
    if metadata.metadata()?.ino() == btrfs_awacs::btrfs::ROOT_INODE {
        return finish_copied_jj_metadata_removal(&destination_directory, &metadata);
    }
    let metadata_mount = copied_jj_mount_id(&metadata)?;
    if copied_jj_mount_id(&destination_directory)? != metadata_mount {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    let metadata_fd_path = PathBuf::from(format!("/proc/self/fd/{}", metadata.as_raw_fd()));
    // Pointer files, symlinks, missing repositories, and nested repository
    // subvolumes use the shared serial remover without following links.
    let repository = open_directory(&metadata_fd_path.join("repo")).ok();
    // DEPRECATED: this traversal and parallel cleanup are only for backward
    // compatibility when both `.jj` and `.jj/repo` are ordinary directories.
    // TODO: Remove them once supported metadata layouts always use subvolumes.
    let directories = match repository.as_ref() {
        Some(repository) if repository.metadata()?.ino() != btrfs_awacs::btrfs::ROOT_INODE => {
            // Check the mount boundary before any worker can remove files.
            // Equal device IDs alone would miss a bind mount.
            if copied_jj_mount_id(repository)? != metadata_mount {
                return Err(io::Error::from_raw_os_error(libc::EXDEV));
            }
            copied_jj_repository_subtrees(repository).unwrap_or_default()
        }
        _ => Vec::new(),
    };
    if directories.len() > 1 {
        remove_copied_jj_subtrees(&directories, &remove_tree)?;
    }
    finish_copied_jj_metadata_removal(&destination_directory, &metadata)?;
    drop(repository);
    drop(metadata);
    drop(destination_directory);
    tracing::debug!(
        repository_subtrees = directories.len(),
        worker_count = if directories.len() > 1 {
            directories.len().min(3)
        } else {
            1
        },
        elapsed_ms = started.elapsed().as_millis(),
        "removed copied JJ metadata"
    );
    Ok(())
}

fn copied_jj_mount_id(directory: &File) -> io::Result<u64> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", directory.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
        .ok_or_else(|| io::Error::other("cannot verify copied JJ directory mount identity"))
}

fn finish_copied_jj_metadata_removal(destination: &File, metadata: &File) -> io::Result<()> {
    // Workers have joined. The shared remover verifies the held identity and
    // mount before traversal, and uses ioctls for every nested subvolume.
    btrfs_awacs::btrfs::remove_directory_at(
        destination.as_fd(),
        OsStr::new(".jj"),
        metadata.as_fd(),
    )
}

fn copied_jj_repository_subtrees(repository: &File) -> io::Result<Vec<PathBuf>> {
    // Keep the repository descriptor alive through enumeration and deletion.
    // A renamed/replaced .jj or repo cannot redirect workers through a symlink.
    // The shared remover protects each final component and destroys subvolumes.
    let anchored_path = PathBuf::from(format!("/proc/self/fd/{}", repository.as_raw_fd()));
    let mut directories = Vec::new();
    for entry in fs::read_dir(anchored_path)? {
        let entry = entry?;
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => directories.push(entry.path()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(directories)
}

/// DEPRECATED: bounded parallel deletion for legacy plain-directory metadata.
/// TODO: Remove with `copied_jj_repository_subtrees` and their tests once
/// supported copied metadata layouts always use subvolumes.
fn remove_copied_jj_subtrees(
    directories: &[PathBuf],
    remove: &(impl Fn(&Path) -> io::Result<()> + Sync),
) -> io::Result<()> {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    // Independent repository subtrees avoid the parent-directory lock shared
    // by individual files. Cap foreground work rather than spawning per entry.
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        let mut failure = None;
        for _ in 0..directories.len().min(3) {
            let worker = std::thread::Builder::new().spawn_scoped(scope, || {
                loop {
                    let Some(path) = directories.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        return Ok(());
                    };
                    match remove(path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
            });
            match worker {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        for worker in workers {
            let result = worker
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("copied JJ cleanup worker panicked")));
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    })
}

/// Deletes one verified Btrfs subvolume and its nested subvolumes through the
/// kernel API, without unlinking ordinary files inside the subvolume tree.
///
/// The btrfs CLI prints its own warnings before returning status, which makes
/// Git worktree removal noisy and leaves AWACS unable to explain which owned
/// path failed. The shared remover verifies identities, uses DESTROY_V2, and
/// never falls back to ordinary recursive deletion when subvolume removal
/// fails.
fn destroy_subvolume_path(path: &Path) -> Result<(), String> {
    remove_subvolume_tree(path)
        .map_err(|error| format!("delete Btrfs subvolume {}: {error}", path.display()))
}

fn awacs_state_dir(root: &Path) -> Result<PathBuf, String> {
    let key = root_state_key(root).map_err(|error| format!("read AWACS root identity: {error}"))?;
    let state_root =
        state_root_for_root(root).map_err(|error| format!("resolve AWACS state root: {error}"))?;
    Ok(state_root.join(key))
}

fn remove_awacs_state(root: &Path) -> Result<(), String> {
    let state_dir = awacs_state_dir(root)?;
    if !state_dir.exists() {
        return Ok(());
    }
    let managed = state_dir.join("managed");
    if managed.exists() {
        for entry in fs::read_dir(&managed)
            .map_err(|error| format!("read {}: {error}", managed.display()))?
        {
            let path = entry
                .map_err(|error| format!("read {} entry: {error}", managed.display()))?
                .path();
            if path
                .file_name()
                .is_some_and(|name| name.as_bytes().starts_with(b"cut-"))
            {
                // Older state stored each cut as a direct subvolume. Current
                // state keeps the read-only subvolume inside a private
                // cut-*/snapshot wrapper so metadata can survive asynchronous
                // deletion. Handle both layouts during rollback/removal.
                let wrapped_snapshot = path.join(MANAGED_SNAPSHOT_CHILD_NAME);
                let cut_path = if wrapped_snapshot.exists() {
                    &wrapped_snapshot
                } else {
                    &path
                };
                let cut = OpenedSubvolume::open(cut_path)
                    .map_err(|error| format!("open managed cut {}: {error}", cut_path.display()))?;
                if cut.subvolume.readonly() {
                    set_subvolume_readonly(cut.as_fd(), false).map_err(|error| {
                        format!("make managed cut {} writable: {error}", cut_path.display())
                    })?;
                }
                destroy_subvolume_path(cut_path)?;
                if cut_path != &path {
                    remove_tree(&path)
                        .map_err(|error| format!("remove managed cut wrapper: {error}"))?;
                }
            }
        }
    }
    remove_tree(&state_dir)
        .map_err(|error| format!("remove AWACS state {}: {error}", state_dir.display()))
}

#[derive(Clone, Copy)]
enum GitWorktreeDestination {
    Registered,
    Absent,
    Snapshot,
}

struct GitWorktreeRegistration {
    parent: File,
    name: OsString,
    directory: File,
}

impl GitWorktreeRegistration {
    fn open(path: &Path) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or_else(|| "Git registration has no parent".to_owned())?;
        let name = path
            .file_name()
            .ok_or_else(|| "Git registration has no basename".to_owned())?;
        let open_directory = |path: &Path| {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(path)
                .map_err(|error| format!("open registered Git administrative directory: {error}"))
        };
        Ok(Self {
            parent: open_directory(parent)?,
            name: name.to_owned(),
            directory: open_directory(path)?,
        })
    }

    fn remove(&self) -> Result<(), String> {
        btrfs_awacs::btrfs::remove_directory_at(
            self.parent.as_fd(),
            &self.name,
            self.directory.as_fd(),
        )
        .map_err(|error| format!("remove registered Git administrative directory: {error}"))
    }
}

fn rollback_git_worktree(
    options: &GitWorktreeAdd<'_>,
    destination_state: GitWorktreeDestination,
    registration: Option<&GitWorktreeRegistration>,
) -> Result<(), String> {
    if matches!(destination_state, GitWorktreeDestination::Absent) {
        // The recursive helper owns only its private staging directory until
        // publication succeeds. A path now occupying the destination belongs
        // to another creator. Even `git worktree remove` could delete it, so
        // remove only the exact registration captured before staging began.
        return registration
            .ok_or_else(|| "Git registration was not retained for rollback".to_owned())?
            .remove();
    }
    match fs::symlink_metadata(options.destination) {
        Ok(_) if matches!(destination_state, GitWorktreeDestination::Snapshot) => {
            remove_awacs_state(options.destination)?;
            destroy_subvolume_path(options.destination)?;
        }
        Ok(_) => remove_tree(options.destination)
            .map_err(|error| format!("remove registered worktree during rollback: {error}"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect worktree during rollback: {error}")),
    }
    // Only remove Git's registration after the filesystem tree is gone. If a
    // subvolume destroy failed, asking Git to remove it would reintroduce the
    // generic recursive-deletion fallback that this rollback must avoid.
    let mut arguments = vec![
        OsString::from("worktree"),
        OsString::from("remove"),
        OsString::from("--force"),
        OsString::from("--force"),
    ];
    arguments.push(options.destination.as_os_str().to_owned());
    git_status(options.git, options.source, &arguments, &[])
}

fn run_git_worktree_add(options: &GitWorktreeAdd<'_>) -> Result<(), String> {
    timed_git_phase("worktree_add", || run_git_worktree_add_inner(options))
}

fn run_git_worktree_add_inner(options: &GitWorktreeAdd<'_>) -> Result<(), String> {
    let source = fs::canonicalize(options.source)
        .map_err(|error| format!("canonicalize source worktree: {error}"))?;
    let destination = canonical_path_for_creation(options.destination)?;
    let options = GitWorktreeAdd {
        source: &source,
        destination: &destination,
        reference: options.reference,
        git: options.git,
        required: options.required,
        detach: options.detach,
        force: options.force,
        relative_paths: options.relative_paths,
        lock_reason: options.lock_reason,
        quiet: options.quiet,
        check_only: options.check_only,
        no_check: options.no_check,
    };
    if options.check_only && options.no_check {
        return Err("--check-only and --no-check cannot be used together".to_owned());
    }
    reject_jj_conflicted_git_target(options.git, options.source, options.reference)?;
    if !snapshot_git_environment_is_safe() {
        return fallback_git_worktree_add(
            &options,
            "Git repository or index is redirected by the environment",
        );
    }
    if !snapshot_git_worktree_matches(options.git, &source)? {
        return fallback_git_worktree_add(
            &options,
            "Git worktree or metadata does not match the snapshot source",
        );
    }
    let source_index = git_path(options.git, &source, "index")?;
    if open_snapshot_source_git_index(&source_index)?.is_none() {
        return fallback_git_worktree_add(
            &options,
            "source Git index is missing, linked, or not a regular file",
        );
    }
    let mut jj_working_copy_lock = timed_git_phase("lock_source_jj", || {
        lock_jj_working_copy_for_snapshot(&source)
    })?;
    if !options.no_check
        && timed_git_phase("check_snapshot_eligibility", || {
            snapshot_eligibility(&options)
        })?
    {
        drop(jj_working_copy_lock.take());
        if options.check_only {
            return Ok(());
        }
        return git_status(
            options.git,
            options.source,
            &git_worktree_args(&options, false),
            &[],
        );
    }
    let pending_jj_source = timed_git_phase("read_source_jj_seed", || {
        read_pending_jj_source_seed(&source)
    })?;
    if let Some(seed) = pending_jj_source {
        let source_subvolume = OpenedSubvolume::open(&source)
            .map_err(|error| format!("inspect source JJ subvolume: {error}"))?;
        if source_subvolume.filesystem.fs_uuid != seed.baseline_filesystem_uuid {
            return Err(
                "source JJ AWACS adoption seed belongs to a different filesystem".to_owned(),
            );
        }
        timed_git_phase("validate_source_jj_baseline", || {
            require_retained_descendant_baseline(
                &source,
                seed.baseline_snapshot_uuid,
                seed.baseline_owner_id,
            )
        })
        .map_err(|error| format!("validate retained JJ adoption baseline: {error}"))?;
    }
    // Git's new-branch path calls us once before creating the branch and once
    // afterwards with --no-check. Static Btrfs/JJ eligibility belongs in the
    // first call, but collecting mutable dirty paths there only duplicates an
    // expensive status and cannot be transferred safely across the race.
    if options.check_only {
        return Ok(());
    }

    let hash_len = git_object_hash_len(options.git, &source)?;
    // The source Git marker can change while eligibility checks are running.
    if !snapshot_git_worktree_matches(options.git, &source)? {
        drop(jj_working_copy_lock.take());
        return fallback_git_worktree_add(
            &options,
            "Git worktree or metadata does not match the snapshot source",
        );
    }
    // The worktree's Git directory can change during eligibility checks.
    // Resolve the index again and open it immediately before capture.
    let source_index = git_path(options.git, &source, "index")?;
    let Some(index) = open_snapshot_source_git_index(&source_index)? else {
        drop(jj_working_copy_lock.take());
        return fallback_git_worktree_add(
            &options,
            "source Git index is missing, linked, or not a regular file",
        );
    };
    let mut handoff = timed_git_phase("capture_source_index_and_cut", || {
        GitIndexHandoff::capture(&source, &source_index, index, hash_len)
    })?;
    let target = String::from_utf8(git_capture(
        options.git,
        &source,
        &["rev-parse", options.reference],
    )?)
    .map_err(|error| format!("decode requested worktree target: {error}"))?;
    timed_git_phase("register_worktree", || {
        git_status(
            options.git,
            options.source,
            &git_worktree_args(&options, true),
            &[],
        )
    })?;
    let mut destination_state = GitWorktreeDestination::Registered;
    let mut registration = None;
    let result = (|| {
        let gitfile = fs::read(destination.join(".git"))
            .map_err(|error| format!("read registered worktree .git: {error}"))?;
        let gitdir_pointer = git_path(options.git, &destination, "gitdir")?;
        let administrative_directory = gitdir_pointer
            .parent()
            .ok_or_else(|| "registered Git administrative directory has no path".to_owned())?;
        registration = Some(GitWorktreeRegistration::open(administrative_directory)?);
        remove_tree(&destination)
            .map_err(|error| format!("remove registered empty worktree: {error}"))?;
        destination_state = GitWorktreeDestination::Absent;
        timed_git_phase("snapshot_worktree", || {
            recursive_snapshot(&source, &destination)
                .map_err(|error| format!("recursively snapshot source worktree: {error}"))
        })?;
        destination_state = GitWorktreeDestination::Snapshot;
        timed_git_phase("remove_copied_git", || remove_copied_dotgit(&destination))?;
        fs::write(destination.join(".git"), gitfile)
            .map_err(|error| format!("restore registered worktree .git: {error}"))?;
        let destination_index = git_path(options.git, &destination, "index")?;
        timed_git_phase("copy_source_index", || {
            handoff.copy_index(&destination_index)
        })?;
        let pending_jj_state_path = if pending_jj_source.is_some() {
            let path = git_path(
                options.git,
                &destination,
                AWACS_JJ_PENDING_WORKING_COPY_STATE,
            )?;
            let source_lock = jj_working_copy_lock
                .as_ref()
                .ok_or_else(|| "source JJ journal has no working-copy lock".to_owned())?;
            stage_pending_jj_working_copy_state(&source, &destination, &path, source_lock)?;
            Some(path)
        } else {
            None
        };
        drop(jj_working_copy_lock.take());
        // A source JJ handoff means this descendant may be adopted as a
        // JJ workspace later. Give JJ its own durable consumer pin now; Git
        // receives a separate child-watch-owned pin below. The two lanes can
        // then advance independently across one shared replay timeline.
        let pending_jj_consumer = pending_jj_source.map(|_| *Uuid::new_v4().as_bytes());
        // Only JJ's semantic baseline requires a retained parent snapshot.
        // The Git index cursor is reconciled separately through its source
        // lease; it must never be interpreted as a physical inheritance pin.
        let inherited_snapshot_uuid = pending_jj_source.map(|seed| seed.baseline_snapshot_uuid);
        let initialized = timed_git_phase("initialize_destination_awacs", || {
            initialize_descendant_root_with_watch_consumer(
                &destination,
                &source,
                inherited_snapshot_uuid,
                pending_jj_consumer,
                |_| {},
            )
        })
        .map_err(|error| format!("initialize AWACS snapshot descendant: {error}"))?;
        let mut git_client = timed_git_phase("open_destination_client", || {
            DirectScanClient::for_root(&destination)
        })
        .map_err(|error| format!("open child AWACS Git baseline: {error}"))?;
        let initial_git_baseline = timed_git_phase("load_destination_baseline", || {
            git_client.initial_baseline(&destination)
        })
        .map_err(|error| format!("load child AWACS Git baseline: {error}"))?;
        let git_owner_id: [u8; 16] = initial_git_baseline
            .retention_token
            .as_slice()
            .try_into()
            .map_err(|_| "child AWACS Git baseline has an invalid lane owner".to_owned())?;
        let mut git_lease = timed_git_phase("begin_destination_scan", || {
            git_client.begin_scan(&BeginScanRequest {
                live_root: destination.clone(),
                baseline_owner_id: git_owner_id,
                previous_baseline: Some(initial_git_baseline),
                allow_full_invalidation: false,
            })
        })
        .map_err(|error| format!("establish verified child AWACS Git baseline: {error}"))?;
        let git_baseline = git_lease.next_baseline.clone();
        let seed_result = (|| {
            timed_git_phase("validate_destination_scan", || {
                git_client
                    .validate_scan_root(&git_lease)
                    .map_err(|error| format!("validate child AWACS Git baseline: {error}"))
            })?;
            let invalidation = timed_git_phase("reconcile_copied_index", || {
                handoff.reconcile(&mut git_client, &mut git_lease, &destination)
            })?;
            timed_git_phase("seed_destination_index", || {
                seed_git_fsmonitor_index(
                    &destination_index,
                    &destination,
                    hash_len,
                    &git_baseline,
                    &invalidation,
                )
            })?;
            timed_git_phase("observe_destination_index", || {
                git_client
                    .observe_persisted_git_index(&destination, &git_baseline)
                    .map_err(|error| format!("retain published child Git cursor: {error}"))
            })?;
            timed_git_phase("promote_destination_baseline", || {
                git_lease
                    .promote()
                    .map_err(|error| format!("promote child AWACS Git baseline: {error}"))
            })?;
            timed_git_phase("finish_destination_scan", || {
                git_lease
                    .finish(ScanOutcome::Committed)
                    .map_err(|error| format!("commit child AWACS Git baseline: {error}"))
            })
        })();
        if let Err(error) = seed_result {
            let _ = git_lease.finish(ScanOutcome::Aborted);
            return Err(error);
        }

        // The child cursor now accounts for every change since the captured
        // index. Classify the actual child, including paths created between
        // the source cut and clone, before materializing the requested tree.
        drop(handoff);
        let child_changes = timed_git_phase("destination_status", || {
            source_git_changes(&GitWorktreeAdd {
                source: &destination,
                ..options
            })
        })?;
        materialize_snapshot_checkout(options.git, &destination, &target, &child_changes)?;
        if timed_git_phase("inspect_materialized_fsmonitor", || {
            read_git_index_extension(&destination_index, hash_len, b"FSMN")
        })?
        .is_none()
        {
            // Git removes FSMN on read when the repository has not enabled
            // fsmonitor yet. Keep the child-owned cursor available for its
            // eventual first hook; absent bitmap state is conservatively dirty.
            timed_git_phase("reseed_destination_index", || {
                seed_git_fsmonitor_index(
                    &destination_index,
                    &destination,
                    hash_len,
                    &git_baseline,
                    &Invalidation::Full,
                )
            })?;
        }
        let zero = "0".repeat(target.trim_end().len());
        timed_git_phase("post_checkout_hook", || {
            git_status(
                options.git,
                &destination,
                &[
                    OsString::from("hook"),
                    OsString::from("run"),
                    OsString::from("--ignore-missing"),
                    OsString::from("post-checkout"),
                    OsString::from("--"),
                    OsString::from(zero),
                    OsString::from(target.trim_end()),
                    OsString::from("1"),
                ],
                &[],
            )
        })?;
        let marker = git_path(options.git, &destination, AWACS_WORKTREE_MARKER)?;
        fs::write(&marker, source.as_os_str().as_bytes()).map_err(|error| {
            format!("write AWACS worktree marker {}: {error}", marker.display())
        })?;
        if let Some(owner_id) = pending_jj_consumer {
            let marker = git_path(options.git, &destination, AWACS_JJ_PENDING_CONSUMER_MARKER)?;
            let seed = format!(
                "awacs-jj-pending-v2:{}:{}:{}\n",
                Uuid::from_bytes(owner_id),
                Uuid::from_bytes(initialized.snapshot_identity.fs_uuid),
                Uuid::from_bytes(initialized.snapshot_identity.subvol_uuid),
            );
            fs::write(&marker, seed).map_err(|error| {
                format!(
                    "write pending JJ consumer marker {}: {error}",
                    marker.display()
                )
            })?;
            debug_assert!(pending_jj_state_path.is_some());
        }
        timed_git_phase("close_destination_client", || {
            drop(git_lease);
            drop(git_client);
            Ok::<(), String>(())
        })?;
        Ok(())
    })();
    if let Err(error) = result {
        drop(jj_working_copy_lock.take());
        if let Err(rollback_error) =
            rollback_git_worktree(&options, destination_state, registration.as_ref())
        {
            return Err(format!(
                "{error}; worktree rollback failed: {rollback_error}"
            ));
        }
        return Err(error);
    }
    Ok(())
}

fn run_git_worktree_remove(path: &Path) -> Result<(), String> {
    let path = fs::canonicalize(path)
        .map_err(|error| format!("canonicalize worktree for removal: {error}"))?;
    remove_awacs_state(&path)?;
    destroy_subvolume_path(&path)
}

/// Implements Git's fsmonitor hook protocol v2 without a persistent user
/// daemon. The returned token carries enough immutable snapshot identity for
/// the next short-lived hook process to ask the embedded coordinator for a
/// conservative replay.
fn run_git_fsmonitor_hook() -> Result<(), String> {
    timed_git_phase("fsmonitor_hook", run_git_fsmonitor_hook_inner)
}

fn run_git_fsmonitor_hook_inner() -> Result<(), String> {
    let mut arguments = env::args_os().skip(1);
    let version = arguments
        .next()
        .ok_or_else(|| "git fsmonitor hook requires protocol version".to_owned())?;
    if version != OsStr::new("2") {
        return Err("git-fsmonitor-awacs supports only hook protocol version 2".to_owned());
    }
    let prior_token = arguments
        .next()
        .ok_or_else(|| "git fsmonitor hook requires a prior token".to_owned())?;
    if arguments.next().is_some() {
        return Err("git fsmonitor hook received unexpected arguments".to_owned());
    }
    let root = env::current_dir().map_err(|error| format!("read Git worktree root: {error}"))?;
    let mut previous_baseline = parse_git_fsmonitor_prior_token(prior_token.as_bytes())?;
    let is_bootstrap = previous_baseline.is_none();
    let mut client = match timed_git_phase("open_fsmonitor_client", || {
        DirectScanClient::for_root(&root)
    }) {
        Ok(client) => client,
        Err(error) if is_uninitialized_awacs_root(&error) => {
            // git worktree add can invoke the configured hook while it is
            // still populating the destination. Do not manufacture a
            // whole-tree invalidation or a cursor before that destination has
            // authoritative AWACS state; a hook failure makes Git use its own
            // correctness fallback without advancing AWACS.
            tracing::debug!(
                root = %root.display(),
                error = %error,
                "AWACS fsmonitor root is not initialized; declining hook query"
            );
            return Err(format!("open AWACS fsmonitor root: {error}"));
        }
        Err(error) => return Err(format!("open AWACS fsmonitor root: {error}")),
    };
    let baseline_owner_id = if let Some(previous_baseline) = previous_baseline.as_mut() {
        // v1 tokens predate durable Git-lane ownership. Recover this worktree's
        // stable owner from its initialized watch while retaining the exact
        // baseline named by the old token; the normal reconciliation below either
        // adopts that endpoint or fails closed if history is already gone.
        if previous_baseline.retention_token.is_empty() {
            previous_baseline.retention_token = client
                .initial_baseline(&root)
                .map_err(|error| format!("load Git AWACS lane owner: {error}"))?
                .retention_token;
        }
        previous_baseline
            .retention_token
            .as_slice()
            .try_into()
            .map_err(|_| "AWACS Git fsmonitor token has an invalid lane owner".to_owned())?
    } else {
        // Git initializes a new hook with a numeric timestamp. An index used
        // by Git's built-in fsmonitor can instead contain a `builtin:` token.
        // Neither proves anything about this index to AWACS, so establish a
        // new watch-owned lane and explicitly require Git to rescan everything.
        let status = RootStatus::open(&root)
            .map_err(|error| format!("identify initialized Git AWACS watch: {error}"))?;
        tracing::info!(
            previous_backend = if prior_token.as_bytes().starts_with(b"builtin:") {
                "git_builtin"
            } else {
                "git_timestamp"
            },
            "bootstrapping Git fsmonitor with an authoritative whole-tree refresh"
        );
        status.watch.id
    };
    let mut lease = timed_git_phase("begin_fsmonitor_scan", || {
        client.begin_scan(&BeginScanRequest {
            live_root: root,
            // Git cursors name positions on the shared logical replay
            // timeline. Concurrent Git commands may publish them out of
            // order, so no cursor exclusively owns a physical snapshot.
            baseline_owner_id,
            previous_baseline,
            // Git's protocol explicitly represents expired replay history
            // with `/`, causing an authoritative whole-index refresh.
            allow_full_invalidation: true,
        })
    })
    .map_err(|error| format!("begin AWACS fsmonitor scan: {error}"))?;
    timed_git_phase("validate_fsmonitor_scan", || {
        client
            .validate_scan_root(&lease)
            .map_err(|error| format!("validate AWACS fsmonitor snapshot: {error}"))
    })?;
    let token = encode_git_fsmonitor_token(&lease.next_baseline)?;
    let paths = match git_fsmonitor_invalidations(is_bootstrap, &lease.invalidation) {
        Ok(paths) => paths,
        Err(error) => {
            let _ = lease.finish(ScanOutcome::Aborted);
            return Err(error);
        }
    };
    timed_git_phase("promote_fsmonitor_cursor", || {
        lease
            .promote()
            .map_err(|error| format!("promote AWACS fsmonitor cursor: {error}"))
    })?;
    timed_git_phase("finish_fsmonitor_scan", || {
        lease
            .finish(ScanOutcome::Committed)
            .map_err(|error| format!("finish AWACS fsmonitor cursor: {error}"))
    })?;
    let response = timed_git_phase("write_fsmonitor_response", || {
        write_git_fsmonitor_response(&token, paths.iter().map(Vec::as_slice))
    });
    timed_git_phase("close_fsmonitor_client", || {
        drop(lease);
        drop(client);
        Ok::<(), String>(())
    })?;
    response
}

fn git_fsmonitor_invalidations(
    is_bootstrap: bool,
    invalidation: &Invalidation,
) -> Result<Vec<Vec<u8>>, String> {
    match invalidation {
        _ if is_bootstrap => Ok(vec![b"/".to_vec()]),
        Invalidation::ExactPaths(paths) => Ok(paths.clone()),
        Invalidation::Full => Ok(vec![b"/".to_vec()]),
        // Git's hook protocol has no first-class prefix result. Until the
        // Git adapter proves directory-prefix semantics, fail the hook rather
        // than widening an exact AWACS answer to a whole-tree invalidation.
        Invalidation::Prefixes(_) => {
            Err("AWACS cannot represent prefix invalidations in the Git fsmonitor hook".to_owned())
        }
    }
}

fn parse_git_fsmonitor_prior_token(token: &[u8]) -> Result<Option<SnapshotBaseline>, String> {
    if let Some(baseline) = decode_git_fsmonitor_token(token) {
        return Ok(Some(baseline));
    }
    // A Git built-in fsmonitor token is not an AWACS cursor, but it can be
    // migrated safely in exactly the same way as Git's numeric placeholder:
    // bootstrap a fresh cursor and return `/` to invalidate the whole index.
    // In particular, `builtin:fake` is persisted when the built-in daemon
    // could not provide a usable token. Reject other malformed opaque tokens
    // so corruption of an AWACS cursor is not silently treated as valid.
    if (!token.is_empty() && token.iter().all(u8::is_ascii_digit)) || token.starts_with(b"builtin:")
    {
        return Ok(None);
    }
    Err(
        "Git fsmonitor received a missing or malformed AWACS token; no AWACS baseline is proven \
         to match the Git index; an authoritative index refresh and explicit baseline \
         initialization are required"
            .to_owned(),
    )
}

fn is_uninitialized_awacs_root(error: &btrfs_awacs::scan::ScanError) -> bool {
    error.kind() == btrfs_awacs::scan::ScanErrorKind::Unavailable
        && error.message().contains("not initialized")
}

fn write_git_fsmonitor_response<'a>(
    token: &str,
    paths: impl IntoIterator<Item = &'a [u8]>,
) -> Result<(), String> {
    write_git_fsmonitor_response_to(io::stdout().lock(), token, paths)
}

fn write_git_fsmonitor_response_to<'a>(
    mut stdout: impl io::Write,
    token: &str,
    paths: impl IntoIterator<Item = &'a [u8]>,
) -> Result<(), String> {
    stdout
        .write_all(token.as_bytes())
        .and_then(|()| stdout.write_all(&[0]))
        .map_err(|error| format!("write Git fsmonitor token: {error}"))?;
    for path in paths {
        stdout
            .write_all(path)
            .and_then(|()| stdout.write_all(&[0]))
            .map_err(|error| format!("write Git fsmonitor path: {error}"))?;
    }
    Ok(())
}

fn finish(result: Result<(), String>) {
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn finish_timed(action: impl FnOnce() -> Result<(), String>) {
    let started = Instant::now();
    match action() {
        Ok(()) => println!("elapsed: {:.3}s", started.elapsed().as_secs_f64()),
        Err(error) => {
            eprintln!("error: {error}");
            eprintln!("elapsed: {:.3}s", started.elapsed().as_secs_f64());
            std::process::exit(1);
        }
    }
}

#[derive(Debug)]
struct DumpSnapshot {
    id: i64,
    filesystem_uuid: [u8; 16],
    subvolume_uuid: [u8; 16],
    parent_uuid: Option<[u8; 16]>,
    received_uuid: Option<[u8; 16]>,
    root_id: u64,
    ctransid: u64,
    otransid: u64,
    path: PathBuf,
    readonly: bool,
    physical_state: String,
    pin_count: i64,
}

fn manager_db_path(root: &Path, manager_db_override: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(manager_db) = manager_db_override {
        return Ok(manager_db.to_owned());
    }
    let root = fs::canonicalize(root)
        .map_err(|error| format!("canonicalize AWACS root {}: {error}", root.display()))?;
    Ok(awacs_state_dir(&root)?.join("manager.sqlite3"))
}

fn manager_table_counts(connection: &Connection) -> Result<Vec<(String, i64)>, String> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .map_err(|error| format!("prepare manager table discovery: {error}"))?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("query manager tables: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("decode manager tables: {error}"))?;
    tables
        .into_iter()
        .map(|table| {
            let escaped = table.replace('"', "\"\"");
            let count = connection
                .query_row(&format!("SELECT count(*) FROM \"{escaped}\""), [], |row| {
                    row.get(0)
                })
                .map_err(|error| format!("count {table}: {error}"))?;
            Ok((table, count))
        })
        .collect()
}

fn managed_snapshot_paths(snapshot: &Path) -> Option<(&Path, &Path)> {
    let entry = snapshot.parent()?;
    Some((entry, entry.parent()?))
}

fn dump_blob<const N: usize>(bytes: Vec<u8>, label: &str) -> Result<[u8; N], String> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| format!("{label} has length {}, expected {N}", bytes.len()))
}

fn dump_optional_blob<const N: usize>(
    bytes: Option<Vec<u8>>,
    label: &str,
) -> Result<Option<[u8; N]>, String> {
    bytes.map(|bytes| dump_blob(bytes, label)).transpose()
}

fn dump_u64(bytes: Vec<u8>, label: &str) -> Result<u64, String> {
    Ok(u64::from_be_bytes(dump_blob(bytes, label)?))
}

fn run_dump(
    root: &Path,
    manager_db_override: Option<&Path>,
    managed_dir: Option<&Path>,
) -> Result<(), String> {
    let manager_db = manager_db_path(root, manager_db_override)?;
    let connection = Connection::open_with_flags(
        &manager_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| format!("open manager database {}: {error}", manager_db.display()))?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .map_err(|error| format!("configure manager database read: {error}"))?;

    println!("AWACS manager state");
    println!("  database: {}", manager_db.display());
    let metadata = connection
        .query_row(
            "SELECT store_uuid, clock_format_version, last_boot_id, created_ns FROM \
             service_metadata WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .map_err(|error| format!("read service metadata: {error}"))?;
    println!(
        "  store: {} clock-format={} boot={} created-ns={}",
        hex_bytes(&metadata.0),
        metadata.1,
        hex_bytes(&metadata.2),
        metadata.3
    );

    println!("\nTable counts:");
    for (table, count) in manager_table_counts(&connection)? {
        println!("  {table}: {count}");
    }

    println!("\nWatches:");
    let mut watch_statement = connection
        .prepare(
            "SELECT id, live_path, state, fsmonitor_state, indexed_seq, last_cut_seq, \
             replay_floor_seq FROM watches ORDER BY live_path",
        )
        .map_err(|error| format!("prepare watch dump: {error}"))?;
    let watches = watch_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })
        .map_err(|error| format!("query watches: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("decode watches: {error}"))?;
    if watches.is_empty() {
        println!("  (none)");
    }
    for (id, path, state, monitor, indexed, last_cut, floor) in watches {
        let path = PathBuf::from(OsString::from_vec(path));
        println!(
            "  {} state={state} fsmonitor={monitor} indexed={indexed:?} last-cut={last_cut:?} \
             replay-floor={floor:?} path={}",
            hex_bytes(&id),
            path.display()
        );
    }

    let snapshots = load_dump_snapshots(&connection)?;
    let mut inconsistencies = Vec::new();
    let mut recorded_paths = HashSet::new();
    println!("\nSnapshots:");
    if snapshots.is_empty() {
        println!("  (none)");
    }
    for snapshot in &snapshots {
        if let Some((entry, _)) = managed_snapshot_paths(&snapshot.path) {
            recorded_paths.insert(entry.to_owned());
        }
        let filesystem = inspect_dump_snapshot(snapshot, &mut inconsistencies);
        println!(
            "  #{} state={} pins={} fs={} subvol={} ro={} path={} [{}]",
            snapshot.id,
            snapshot.physical_state,
            snapshot.pin_count,
            hex_bytes(&snapshot.filesystem_uuid),
            hex_bytes(&snapshot.subvolume_uuid),
            snapshot.readonly,
            snapshot.path.display(),
            filesystem,
        );
    }

    println!("\nPins:");
    let mut pin_statement = connection
        .prepare(
            "SELECT snapshot_id, owner_kind, owner_id, reason FROM snapshot_pins ORDER BY \
             snapshot_id, owner_kind, owner_id, reason",
        )
        .map_err(|error| format!("prepare pin dump: {error}"))?;
    let pins = pin_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| format!("query pins: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("decode pins: {error}"))?;
    if pins.is_empty() {
        println!("  (none)");
    }
    for (snapshot_id, owner_kind, owner_id, reason) in pins {
        println!(
            "  snapshot=#{snapshot_id} owner={owner_kind}:{} reason={reason}",
            hex_bytes(&owner_id)
        );
    }

    let mut check_dirs = snapshots
        .iter()
        .filter_map(|snapshot| {
            managed_snapshot_paths(&snapshot.path).map(|(_, directory)| directory.to_owned())
        })
        .collect::<HashSet<_>>();
    if let Some(managed_dir) = managed_dir {
        check_dirs.insert(managed_dir.to_owned());
    }
    for directory in check_dirs {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                inconsistencies.push(format!(
                    "cannot list managed directory {}: {error}",
                    directory.display()
                ));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    inconsistencies.push(format!(
                        "cannot read an entry in {}: {error}",
                        directory.display()
                    ));
                    continue;
                }
            };
            let path = entry.path();
            if entry.file_name().as_bytes().starts_with(b"cut-") && !recorded_paths.contains(&path)
            {
                inconsistencies.push(format!(
                    "managed cut is present on disk but absent from snapshots: {}",
                    path.display()
                ));
            }
        }
    }
    let mut foreign_keys = connection
        .prepare("PRAGMA foreign_key_check")
        .map_err(|error| format!("prepare foreign-key check: {error}"))?;
    let violations = foreign_keys
        .query_map([], |row| {
            Ok(format!(
                "foreign key violation: table={} rowid={:?} parent={} constraint={}",
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|error| format!("query foreign-key check: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("decode foreign-key check: {error}"))?;
    inconsistencies.extend(violations);

    println!("\nFilesystem consistency:");
    if inconsistencies.is_empty() {
        println!("  ok");
    } else {
        for inconsistency in &inconsistencies {
            println!("  INCONSISTENT: {inconsistency}");
        }
    }
    Ok(())
}

fn load_dump_snapshots(connection: &Connection) -> Result<Vec<DumpSnapshot>, String> {
    let mut statement = connection
        .prepare(
            r#"SELECT s.id, f.fs_uuid, s.subvol_uuid, s.parent_uuid, s.received_uuid,
                      s.root_id, s.ctransid, s.otransid, s.path, s.readonly,
                      s.physical_state,
                      (SELECT count(*) FROM snapshot_pins p WHERE p.snapshot_id = s.id)
                 FROM snapshots s JOIN filesystems f ON f.id = s.filesystem_id
                ORDER BY s.id"#,
        )
        .map_err(|error| format!("prepare snapshot dump: {error}"))?;
    let raw = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
                row.get::<_, Vec<u8>>(5)?,
                row.get::<_, Vec<u8>>(6)?,
                row.get::<_, Vec<u8>>(7)?,
                row.get::<_, Vec<u8>>(8)?,
                row.get::<_, bool>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, i64>(11)?,
            ))
        })
        .map_err(|error| format!("query snapshots: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("decode snapshots: {error}"))?;
    raw.into_iter()
        .map(
            |(
                id,
                filesystem_uuid,
                subvolume_uuid,
                parent_uuid,
                received_uuid,
                root_id,
                ctransid,
                otransid,
                path,
                readonly,
                physical_state,
                pin_count,
            )| {
                Ok(DumpSnapshot {
                    id,
                    filesystem_uuid: dump_blob(filesystem_uuid, "snapshot filesystem UUID")?,
                    subvolume_uuid: dump_blob(subvolume_uuid, "snapshot subvolume UUID")?,
                    parent_uuid: dump_optional_blob(parent_uuid, "snapshot parent UUID")?,
                    received_uuid: dump_optional_blob(received_uuid, "snapshot received UUID")?,
                    root_id: dump_u64(root_id, "snapshot root ID")?,
                    ctransid: dump_u64(ctransid, "snapshot ctransid")?,
                    otransid: dump_u64(otransid, "snapshot otransid")?,
                    path: PathBuf::from(OsString::from_vec(path)),
                    readonly,
                    physical_state,
                    pin_count,
                })
            },
        )
        .collect()
}

fn inspect_dump_snapshot(snapshot: &DumpSnapshot, inconsistencies: &mut Vec<String>) -> String {
    if matches!(snapshot.physical_state.as_str(), "deleted" | "lost") {
        if snapshot.path.exists() {
            inconsistencies.push(format!(
                "snapshot #{} is {} in SQLite but still exists at {}",
                snapshot.id,
                snapshot.physical_state,
                snapshot.path.display()
            ));
            return "unexpectedly present".to_owned();
        }
        return "absent as recorded".to_owned();
    }
    let opened = match OpenedSubvolume::open(&snapshot.path) {
        Ok(opened) => opened,
        Err(error) => {
            if snapshot.physical_state == "present" {
                inconsistencies.push(format!(
                    "snapshot #{} is present in SQLite but cannot be opened at {}: {error}",
                    snapshot.id,
                    snapshot.path.display()
                ));
            }
            return format!("unavailable: {error}");
        }
    };
    if let Err(error) = opened.revalidate() {
        inconsistencies.push(format!(
            "snapshot #{} failed Btrfs revalidation at {}: {error}",
            snapshot.id,
            snapshot.path.display()
        ));
        return format!("invalid: {error}");
    }
    let mut mismatches = Vec::new();
    if opened.filesystem.fs_uuid != snapshot.filesystem_uuid {
        mismatches.push("filesystem UUID");
    }
    if opened.subvolume.uuid != snapshot.subvolume_uuid {
        mismatches.push("subvolume UUID");
    }
    if opened.subvolume.parent_uuid != snapshot.parent_uuid {
        mismatches.push("parent UUID");
    }
    if opened.subvolume.received_uuid != snapshot.received_uuid {
        mismatches.push("received UUID");
    }
    if opened.subvolume.root_id != snapshot.root_id {
        mismatches.push("root ID");
    }
    if opened.subvolume.ctransid != snapshot.ctransid {
        mismatches.push("ctransid");
    }
    if opened.subvolume.otransid != snapshot.otransid {
        mismatches.push("otransid");
    }
    if opened.subvolume.readonly() != snapshot.readonly {
        mismatches.push("read-only flag");
    }
    if mismatches.is_empty() {
        "matches Btrfs".to_owned()
    } else {
        inconsistencies.push(format!(
            "snapshot #{} at {} mismatches SQLite fields: {}",
            snapshot.id,
            snapshot.path.display(),
            mismatches.join(", ")
        ));
        format!("mismatch: {}", mismatches.join(", "))
    }
}

struct TemporaryChangedObjectsStream {
    path: PathBuf,
    file: File,
}

impl TemporaryChangedObjectsStream {
    fn create() -> Result<Self, String> {
        let path = env::temp_dir().join(format!("awacs-changed-objects-{}", Uuid::new_v4()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("create changed-object stream {}: {error}", path.display()))?;
        Ok(Self { path, file })
    }
}

impl Drop for TemporaryChangedObjectsStream {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn run_debug_changed_objects(
    source_path: &Path,
    target_path: &Path,
    summary_only: bool,
) -> Result<(), String> {
    let source = OpenedSubvolume::open(source_path)
        .map_err(|error| format!("open source snapshot {}: {error}", source_path.display()))?;
    let target = OpenedSubvolume::open(target_path)
        .map_err(|error| format!("open target snapshot {}: {error}", target_path.display()))?;
    let output = TemporaryChangedObjectsStream::create()?;
    let request = ChangedObjectsExecution {
        parent: ExpectedSubvolume::from_observed(&source.filesystem, &source.subvolume),
        target: ExpectedSubvolume::from_observed(&target.filesystem, &target.subvolume),
        output_owner_uid: unsafe { libc::geteuid() },
        max_output_bytes: env::var("BTRFS_AWACS_TEST_MAX_OUTPUT_BYTES")
            .ok()
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|error| format!("invalid test output limit: {error}"))
            })
            .transpose()?
            .unwrap_or(64 * 1024 * 1024),
    };
    let result = execute_changed_objects(
        &request,
        source.as_fd(),
        target.as_fd(),
        output.file.as_fd(),
    )
    .map_err(|error| format!("compare changed objects: {error}"))?;
    let bytes = fs::read(&output.path).map_err(|error| {
        format!(
            "read changed-object stream {}: {error}",
            output.path.display()
        )
    })?;
    if bytes.len() as u64 != result.output_bytes {
        return Err(format!(
            "broker result reported {} bytes but output contains {}",
            result.output_bytes,
            bytes.len()
        ));
    }
    let parsed = parse_changed_objects_v2(&bytes)
        .map_err(|error| format!("parse changed-objects v2 stream: {error}"))?;
    if summary_only {
        let summary = summarize_changed_objects(&parsed.manifest);
        println!(
            "objects={} created={} deleted={} refs=+{}/-{} raw_refs=+{}/-{}",
            summary.objects,
            summary.created,
            summary.deleted,
            summary.net_ref_adds,
            summary.net_ref_deletes,
            summary.raw_ref_adds,
            summary.raw_ref_deletes,
        );
    } else {
        print!("{}", format_changed_objects_stream(&parsed));
    }
    Ok(())
}

fn summarize_changed_objects(manifest: &ChangedObjectsManifest) -> ChangedObjectsSummary {
    ChangedObjectsSummary {
        objects: manifest.objects.len(),
        created: manifest
            .objects
            .values()
            .filter(|change| change.is_created())
            .count(),
        deleted: manifest
            .objects
            .values()
            .filter(|change| change.is_deleted())
            .count(),
        raw_ref_adds: manifest.raw_ref_adds,
        raw_ref_deletes: manifest.raw_ref_deletes,
        net_ref_adds: manifest.ref_adds.len(),
        net_ref_deletes: manifest.ref_deletes.len(),
    }
}

fn format_changed_objects_stream(stream: &ChangedObjectsV2) -> String {
    let mut output = String::new();
    writeln!(output, "Changed-object stream (v2)").unwrap();
    writeln!(
        output,
        "  filesystem: {}",
        Uuid::from_bytes(stream.header.fs_uuid)
    )
    .unwrap();
    writeln!(
        output,
        "  source: {} root={} transaction={}",
        Uuid::from_bytes(stream.header.source_uuid),
        stream.header.source_root_id,
        stream.header.source_ctransid,
    )
    .unwrap();
    writeln!(
        output,
        "  target: {} root={} transaction={}",
        Uuid::from_bytes(stream.header.target_uuid),
        stream.header.target_root_id,
        stream.header.target_ctransid,
    )
    .unwrap();
    writeln!(
        output,
        "  guarantees: subvolume-boundaries={} dirty-witness={}",
        stream.header.boundary_records, stream.header.dirty_witness,
    )
    .unwrap();

    writeln!(output, "\nObjects ({}):", stream.manifest.objects.len()).unwrap();
    for change in stream.manifest.objects.values() {
        writeln!(
            output,
            "  inode={} generations={:?}->{:?} changes={}",
            change.ino,
            change.old_generation,
            change.new_generation,
            format_change_mask(change.change_mask),
        )
        .unwrap();
        if let Some(target) = stream.target_objects.get(&change.ino) {
            writeln!(
                output,
                "    target: generation={} sequence={} transaction={} mode={:#o} links={} uid={} \
                 gid={} device={}",
                target.generation,
                target.change_sequence,
                target.transid,
                target.mode,
                target.nlink,
                target.uid,
                target.gid,
                target.rdev,
            )
            .unwrap();
        }
        if let Some(attributes) = stream.target_security_xattrs.get(&change.ino) {
            if attributes.is_empty() {
                writeln!(output, "    security attributes: (none)").unwrap();
            }
            for (name, value) in attributes {
                writeln!(
                    output,
                    "    attribute {:?}=0x{}",
                    String::from_utf8_lossy(name),
                    hex_bytes(value),
                )
                .unwrap();
            }
        }
    }

    format_changed_references(&mut output, "References added", &stream.manifest.ref_adds);
    format_changed_references(
        &mut output,
        "References removed",
        &stream.manifest.ref_deletes,
    );
    format_changed_boundaries(&mut output, "Subvolumes added", &stream.boundary_adds);
    format_changed_boundaries(&mut output, "Subvolumes removed", &stream.boundary_deletes);
    writeln!(
        output,
        "\nCompletion: records={} bytes={} raw-references=+{}/-{}",
        stream.completion.record_count,
        stream.completion.output_bytes().unwrap_or(u64::MAX),
        stream.manifest.raw_ref_adds,
        stream.manifest.raw_ref_deletes,
    )
    .unwrap();
    output
}

fn format_change_mask(mask: u64) -> String {
    [
        (CHANGE_INODE, "inode"),
        (CHANGE_REF, "references"),
        (CHANGE_XATTR, "attributes"),
        (CHANGE_FILE_DATA, "file-data"),
        (CHANGE_VERITY, "verity"),
        (CHANGE_CREATED, "created"),
        (CHANGE_DELETED, "deleted"),
        (CHANGE_DIR_ENTRIES, "directory-entries"),
    ]
    .into_iter()
    .filter_map(|(flag, name)| (mask & flag != 0).then_some(name))
    .collect::<Vec<_>>()
    .join(",")
}

fn format_changed_references(
    output: &mut String,
    label: &str,
    references: &std::collections::BTreeSet<Reference>,
) {
    writeln!(output, "\n{label} ({}):", references.len()).unwrap();
    for reference in references {
        writeln!(
            output,
            "  inode={} parent={} name={:?}",
            reference.ino,
            reference.parent_ino,
            String::from_utf8_lossy(&reference.name),
        )
        .unwrap();
    }
}

fn format_changed_boundaries(
    output: &mut String,
    label: &str,
    boundaries: &std::collections::BTreeSet<SubvolumeBoundary>,
) {
    writeln!(output, "\n{label} ({}):", boundaries.len()).unwrap();
    for boundary in boundaries {
        writeln!(
            output,
            "  parent={} child-root={} name={:?}",
            boundary.parent_ino,
            boundary.child_root_id,
            String::from_utf8_lossy(&boundary.name),
        )
        .unwrap();
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn run_benchmark(source: &Path, create: bool) -> Result<(), String> {
    if !source.is_dir() {
        return Err(format!("{} is not a directory", source.display()));
    }
    let snapshot_dir = source.join(SNAPSHOT_DIR);
    if create {
        snap(source, &snapshot_dir)
    } else {
        compare(&snapshot_dir)
    }
}

fn run_broker_server(
    directory: PathBuf,
    manager_uid: u32,
    manager_gid: u32,
    probe_root: PathBuf,
) -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("broker-serve must run as root".to_owned());
    }
    verify_changed_objects_kernel_support(&probe_root)?;
    if !directory.is_absolute() {
        return Err("broker request directory must be absolute".to_owned());
    }
    match fs::symlink_metadata(&directory) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .map_err(|error| {
                    format!(
                        "create broker request directory {}: {error}",
                        directory.display()
                    )
                })?;
            let path = std::ffi::CString::new(directory.as_os_str().as_bytes())
                .map_err(|_| "broker request directory contains NUL".to_owned())?;
            if unsafe { libc::chown(path.as_ptr(), manager_uid, manager_gid) } != 0 {
                return Err(format!(
                    "set broker request directory ownership: {}",
                    io::Error::last_os_error()
                ));
            }
        }
        Err(error) => {
            return Err(format!(
                "stat broker request directory {}: {error}",
                directory.display()
            ));
        }
    }
    let mut listener = FileBrokerListener::new(&directory, manager_uid)
        .map_err(|error| format!("start broker request listener: {error}"))?;
    let dispatcher = std::sync::Arc::new(
        BrokerDispatcher::new(manager_uid)
            .map_err(|error| format!("start broker snapshot trash worker: {error}"))?,
    );
    loop {
        let request = listener
            .receive()
            .map_err(|error| format!("receive broker request: {error}"))?;
        let dispatcher = std::sync::Arc::clone(&dispatcher);
        std::thread::Builder::new()
            .name("btrfs-awacs-broker-client".to_owned())
            .spawn(move || {
                if let Err(error) = dispatcher.serve_file_request(request) {
                    log::warn!("AWACS broker request failed: {error}");
                }
            })
            .map_err(|error| format!("start broker request worker: {error}"))?;
    }
}

fn verify_changed_objects_kernel_support(probe_root: &Path) -> Result<(), String> {
    let probe = OpenedSubvolume::open(probe_root).map_err(|error| {
        format!(
            "open Btrfs ioctl probe root {}: {error}",
            probe_root.display()
        )
    })?;
    if !supports_changed_objects_v2(probe.as_fd())
        .map_err(|error| format!("probe BTRFS_IOC_CHANGED_OBJECTS: {error}"))?
    {
        return Err(format!(
            "running kernel does not support BTRFS_IOC_CHANGED_OBJECTS for {}; boot the patched \
             Btrfs kernel before starting AWACS",
            probe_root.display()
        ));
    }
    Ok(())
}

fn snap(source: &Path, snapshot_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(snapshot_dir)
        .map_err(|error| format!("create {}: {error}", snapshot_dir.display()))?;

    let snapshot = create_snapshot(source, snapshot_dir)?;
    println!("snapshot: {}", snapshot.display());
    Ok(())
}

fn compare(snapshot_dir: &Path) -> Result<(), String> {
    let snapshots = if snapshot_dir.exists() {
        snapshots_in(snapshot_dir)?
    } else {
        Vec::new()
    };
    let (parent, current) = require_last_two_snapshots(snapshot_dir, snapshots)?;

    let changed = count_changed_files(&parent, &current, true)?;
    println!("changed: {changed}");
    Ok(())
}

fn last_two_snapshots(mut snapshots: Vec<PathBuf>) -> Option<(PathBuf, PathBuf)> {
    snapshots.sort();
    let current = snapshots.pop()?;
    let parent = snapshots.pop()?;
    Some((parent, current))
}

fn require_last_two_snapshots(
    snapshot_dir: &Path,
    snapshots: Vec<PathBuf>,
) -> Result<(PathBuf, PathBuf), String> {
    let snapshot_count = snapshots.len();
    last_two_snapshots(snapshots).ok_or_else(|| {
        format!(
            "compare requires at least two snapshots in {} (found {snapshot_count})",
            snapshot_dir.display()
        )
    })
}

fn snapshots_in(snapshot_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(snapshot_dir)
        .map_err(|error| format!("read {}: {error}", snapshot_dir.display()))?;
    let mut snapshots = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", snapshot_dir.display()))?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(SNAPSHOT_PREFIX) {
            let file_type = entry
                .file_type()
                .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?;
            if file_type.is_dir() {
                snapshots.push(entry.path());
            }
        }
    }

    Ok(snapshots)
}

fn create_snapshot(source: &Path, snapshot_dir: &Path) -> Result<PathBuf, String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read system clock: {error}"))?
        .as_nanos();

    for suffix in 0..1000 {
        let name = if suffix == 0 {
            format!("{SNAPSHOT_PREFIX}{timestamp:039}")
        } else {
            format!("{SNAPSHOT_PREFIX}{timestamp:039}-{suffix}")
        };
        let destination = snapshot_dir.join(name);
        if destination.exists() {
            continue;
        }

        run_btrfs(["subvolume", "snapshot", "-r"], [source, &destination])?;
        return Ok(destination);
    }

    Err("could not choose a unique snapshot name".to_owned())
}

fn run_btrfs<const N: usize, const M: usize>(
    args: [&str; N],
    paths: [&Path; M],
) -> Result<(), String> {
    let status = Command::new("sudo")
        .args(["--non-interactive", "btrfs"])
        .args(args)
        .args(paths)
        .status()
        .map_err(|error| format!("run sudo btrfs: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("btrfs exited with {status}"))
    }
}

fn count_changed_files(parent: &Path, current: &Path, timing: bool) -> Result<usize, String> {
    if !timing {
        return count_changed_files_from_send(parent, current);
    }

    let normal_started = Instant::now();
    let normal = count_changed_files_from_send(parent, current)?;
    let normal_elapsed = normal_started.elapsed();
    println!(
        "normal --no-data: changed={normal} elapsed={:.3}s",
        normal_elapsed.as_secs_f64()
    );
    io::stdout()
        .flush()
        .map_err(|error| format!("flush normal comparison result: {error}"))?;

    let objects_started = Instant::now();
    let objects = changed_objects_summary(parent, current)?;
    let objects_elapsed = objects_started.elapsed();
    println!(
        "changed-objects: {objects} elapsed={:.3}s",
        objects_elapsed.as_secs_f64()
    );
    print_timing_comparison("changed-objects", normal_elapsed, objects_elapsed);

    Ok(normal)
}

fn print_timing_comparison(label: &str, normal: Duration, specialized: Duration) {
    let normal = normal.as_secs_f64();
    let specialized = specialized.as_secs_f64();

    if normal == 0.0 || specialized == 0.0 {
        println!("comparison: {label} duration too short to calculate");
    } else if specialized <= normal {
        println!(
            "comparison: {label} is {:.1}% faster ({:.2}x speedup)",
            (1.0 - specialized / normal) * 100.0,
            normal / specialized
        );
    } else {
        println!(
            "comparison: {label} is {:.1}% slower ({:.2}x as long)",
            (specialized / normal - 1.0) * 100.0,
            specialized / normal
        );
    }
}

fn count_changed_files_from_send(parent: &Path, current: &Path) -> Result<usize, String> {
    let mut send = Command::new("sudo")
        .args(["--non-interactive", "btrfs", "send", "--no-data", "-p"])
        .arg(parent)
        .arg(current)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("run btrfs send: {error}"))?;

    let send_stdout = send
        .stdout
        .take()
        .ok_or_else(|| "capture btrfs send output".to_owned())?;
    let dump = Command::new("sudo")
        .args(["--non-interactive", "btrfs", "receive", "--dump"])
        .stdin(Stdio::from(send_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            terminate(&mut send);
            format!("run sudo btrfs receive --dump: {error}")
        })?;

    let dump_output = dump
        .wait_with_output()
        .map_err(|error| format!("wait for sudo btrfs receive --dump: {error}"))?;
    let send_output = send
        .wait_with_output()
        .map_err(|error| format!("wait for sudo btrfs send: {error}"))?;

    if !send_output.status.success() {
        return Err(command_failure(
            "sudo --non-interactive btrfs send",
            send_output.status,
            &send_output.stderr,
        ));
    }
    if !dump_output.status.success() {
        return Err(command_failure(
            "sudo --non-interactive btrfs receive --dump",
            dump_output.status,
            &dump_output.stderr,
        ));
    }

    let dump = String::from_utf8_lossy(&dump_output.stdout);
    Ok(changed_paths(&dump).len())
}

fn changed_objects_summary(parent: &Path, current: &Path) -> Result<String, String> {
    let executable =
        env::current_exe().map_err(|error| format!("locate current executable: {error}"))?;
    let output = Command::new("sudo")
        .arg("--non-interactive")
        .arg(executable)
        .arg("debug")
        .arg("changed-objects")
        .arg(parent)
        .arg(current)
        .arg("--summary")
        .output()
        .map_err(|error| format!("run changed-objects debug helper: {error}"))?;

    if !output.status.success() {
        return Err(command_failure(
            "sudo --non-interactive awacs debug changed-objects",
            output.status,
            &output.stderr,
        ));
    }
    std::str::from_utf8(&output.stdout)
        .map(|summary| summary.trim().to_owned())
        .map_err(|error| format!("changed-object summary is not UTF-8: {error}"))
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn command_failure(command: &str, status: std::process::ExitStatus, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        format!("{command} exited with {status}")
    } else {
        format!("{command} exited with {status}: {detail}")
    }
}

// Returns one path per changed file represented by btrfs receive --dump.
// --no-data replaces writes with update_extent, so that command is included.
// Directory-only commands are excluded. A rename counts once at its target.
fn changed_paths(dump: &str) -> HashSet<String> {
    let mut paths = HashSet::new();

    for line in dump.lines() {
        let mut fields = line.trim().splitn(2, char::is_whitespace);
        let command = fields.next().unwrap_or("");
        let arguments = fields.next().unwrap_or("").trim_start();

        let path = match command {
            "mkfile" | "mknod" | "symlink" | "unlink" | "truncate" | "write" | "update_extent"
            | "chmod" | "chown" | "utimes" | "set_xattr" | "remove_xattr" | "clone" => {
                first_shell_word(arguments)
            }
            "link" => first_shell_word(arguments),
            "rename" => rename_destination(arguments),
            _ => None,
        };

        if let Some(path) = path
            && path != "."
            && path != "./"
        {
            paths.insert(path);
        }
    }

    paths
}

fn after_arrow(input: &str) -> Option<&str> {
    input.split_once(" -> ").map(|(_, right)| right)
}

fn rename_destination(input: &str) -> Option<String> {
    if let Some(destination) = after_arrow(input) {
        return first_shell_word(destination);
    }

    let attributes = after_first_shell_word(input)?;
    first_shell_word(attributes.strip_prefix("dest=")?)
}

fn after_first_shell_word(input: &str) -> Option<&str> {
    let mut quoted = false;
    let mut escaped = false;

    for (offset, character) in input.trim_start().char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character.is_whitespace() && !quoted {
            return Some(input.trim_start()[offset..].trim_start());
        }
    }

    None
}

// Extracts the first dump argument while preserving spaces inside quotes.
fn first_shell_word(input: &str) -> Option<String> {
    let mut result = String::new();
    let mut quoted = false;
    let mut escaped = false;

    for character in input.trim_start().chars() {
        if escaped {
            result.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character.is_whitespace() && !quoted {
            break;
        } else {
            result.push(character);
        }
    }

    if escaped {
        result.push('\\');
    }
    (!result.is_empty()).then_some(result)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::unix::ffi::OsStrExt as _;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use btrfs_awacs::manifest::CHANGE_CREATED as CHANGED_OBJECT_CREATED;
    use btrfs_awacs::manifest::CHANGE_DELETED as CHANGED_OBJECT_DELETED;
    use btrfs_awacs::manifest::CHANGE_INODE as CHANGED_OBJECT_INODE;
    use btrfs_awacs::manifest::CHANGE_REF as CHANGED_OBJECT_REF;
    use btrfs_awacs::manifest::ChangedObjectsManifest;
    use btrfs_awacs::manifest::ChangedObjectsV2;
    use btrfs_awacs::manifest::ChangedObjectsV2Completion;
    use btrfs_awacs::manifest::ChangedObjectsV2Header;
    use btrfs_awacs::manifest::ObjectChange;
    use btrfs_awacs::manifest::Reference;
    use btrfs_awacs::manifest::TargetObjectMetadata;
    use clap::CommandFactory;
    use clap::Parser;
    use rusqlite::Connection;

    use super::ChangedObjectsSummary;
    use super::Cli;
    use super::CliCommand;
    use super::DebugCommand;
    use super::GitCommand;
    use super::GitWorktreeAdd;
    use super::JJ_WORKING_COPY_STATE_MAGIC;
    use super::canonical_path_for_creation;
    use super::changed_paths;
    use super::decode_git_fsmonitor_token;
    use super::encode_git_fsmonitor_token;
    use super::format_changed_objects_stream;
    use super::git_fsmonitor_invalidations;
    use super::is_uninitialized_awacs_root;
    use super::last_two_snapshots;
    use super::lock_jj_working_copy_for_snapshot;
    use super::managed_snapshot_paths;
    use super::manager_db_path;
    use super::manager_table_counts;
    use super::materialize_snapshot_checkout;
    use super::parse_git_fsmonitor_prior_token;
    use super::parse_source_git_changes;
    use super::read_pending_jj_source_seed;
    use super::reject_jj_conflicted_git_target;
    use super::require_last_two_snapshots;
    use super::rollback_git_worktree;
    use super::run_git_worktree_add;
    use super::stage_pending_jj_working_copy_state;
    use super::summarize_changed_objects;
    use super::timed_git_phase;
    use super::write_git_fsmonitor_response_to;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "btrfs-awacs-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn init_preserves_linked_awacs_state_when_git_metadata_would_be_replaced() {
        let root = TestDir::new("init-linked-awacs-metadata");
        let worktree = root.path.join(".git/worktrees/linked");
        let state = worktree.join("awacs/watch");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("manager.sqlite3"), b"recorded snapshot identity").unwrap();
        fs::write(worktree.join("gitdir"), b"/linked/.git\n").unwrap();

        let error = super::require_safe_init_git_metadata(&root.path, false).unwrap_err();
        assert!(error.contains("linked-worktree AWACS state remains"));
        assert!(error.contains("change its recorded snapshot identities"));
        assert_eq!(
            fs::read(state.join("manager.sqlite3")).unwrap(),
            b"recorded snapshot identity"
        );
        assert_eq!(
            fs::read(worktree.join("gitdir")).unwrap(),
            b"/linked/.git\n"
        );

        // An existing root and `.git` subvolume are left in place, including
        // when conversion only needs to turn an ordinary `.jj` into a subvolume.
        super::require_safe_init_git_metadata(&root.path, true).unwrap();
        fs::remove_dir_all(worktree.join("awacs")).unwrap();
        super::require_safe_init_git_metadata(&root.path, false).unwrap();
    }

    #[test]
    fn init_rejects_uninspectable_linked_metadata_before_conversion() {
        let root = TestDir::new("init-linked-awacs-symlink");
        let worktrees = root.path.join(".git/worktrees");
        fs::create_dir_all(&worktrees).unwrap();
        std::os::unix::fs::symlink("/missing/worktree", worktrees.join("linked")).unwrap();
        let error = super::require_safe_init_git_metadata(&root.path, false).unwrap_err();
        assert!(error.contains("symbolic-link Git worktree metadata"));
        assert!(
            fs::symlink_metadata(worktrees.join("linked"))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn git_phase_timing_is_visible_at_info_without_error_contents() {
        use std::io::Write;
        use std::io::{self};
        use std::sync::Arc;
        use std::sync::Mutex;

        #[derive(Clone)]
        struct LogWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for LogWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(timed_git_phase("test_success", || Ok::<_, &str>(7)), Ok(7));
            assert_eq!(
                timed_git_phase("test_failure", || Err::<(), _>("private-error-detail")),
                Err("private-error-detail")
            );
        });
        let logged = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("\"phase\":\"test_success\""));
        assert!(logged.contains("\"phase\":\"test_failure\""));
        assert!(logged.contains("\"success\":true"));
        assert!(logged.contains("\"success\":false"));
        assert!(logged.contains("\"elapsed_ms\":"));
        assert!(!logged.contains("private-error-detail"));
    }

    #[test]
    fn git_fsmonitor_token_round_trips_snapshot_identity() {
        let baseline = btrfs_awacs::scan::SnapshotBaseline {
            identity: btrfs_awacs::scan::SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
            continuity_token: b"c:btrfs-awacs:scan:1:opaque".to_vec(),
            retention_token: vec![3; 16],
        };
        let token = encode_git_fsmonitor_token(&baseline).unwrap();
        assert!(token.starts_with("awacs-git-v2:"));
        let decoded = decode_git_fsmonitor_token(token.as_bytes()).unwrap();
        assert_eq!(decoded.identity, baseline.identity);
        assert_eq!(decoded.continuity_token, baseline.continuity_token);
        assert_eq!(decoded.retention_token, baseline.retention_token);

        let legacy = "awacs-git-v1:01010101-0101-0101-0101-010101010101:\
                      02020202-0202-0202-0202-020202020202:c:btrfs-awacs:scan:1:legacy";
        let decoded_legacy = decode_git_fsmonitor_token(legacy.as_bytes()).unwrap();
        assert_eq!(decoded_legacy.identity, baseline.identity);
        assert_eq!(
            decoded_legacy.continuity_token,
            b"c:btrfs-awacs:scan:1:legacy"
        );
        assert!(decoded_legacy.retention_token.is_empty());
    }

    #[test]
    fn git_fsmonitor_tokens_require_established_continuity() {
        let initial_baseline = btrfs_awacs::scan::SnapshotBaseline {
            identity: btrfs_awacs::scan::SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
            continuity_token: Vec::new(),
            retention_token: vec![3; 16],
        };

        assert!(
            encode_git_fsmonitor_token(&initial_baseline)
                .unwrap_err()
                .contains("established continuity"),
            "an initial baseline is not an established Git cursor"
        );
        assert!(decode_git_fsmonitor_token(b"1787706655995521277").is_none());
        assert!(
            decode_git_fsmonitor_token(
                b"awacs-git-v2:03030303-0303-0303-0303-030303030303:01010101-0101-0101-0101-010101010101:02020202-0202-0202-0202-020202020202:"
            )
            .is_none(),
            "an empty continuity suffix cannot become a valid Git cursor"
        );
    }

    #[test]
    fn git_fsmonitor_foreign_cursors_force_an_authoritative_refresh() {
        for token in [
            b"1787706655995521277".as_slice(),
            b"builtin:fake",
            b"builtin:some-previous-daemon-token",
        ] {
            let previous = parse_git_fsmonitor_prior_token(token)
                .expect("Git's timestamp and built-in cursor can bootstrap AWACS");
            assert!(previous.is_none());
            let paths = git_fsmonitor_invalidations(
                previous.is_none(),
                &btrfs_awacs::scan::Invalidation::ExactPaths(Vec::new()),
            )
            .unwrap();
            assert_eq!(
                paths,
                [b"/".to_vec()],
                "bootstrap must invalidate every entry"
            );
        }
        assert!(
            parse_git_fsmonitor_prior_token(b"not-an-awacs-cursor").is_err(),
            "malformed opaque cursors must not silently become bootstrap requests"
        );
        assert!(
            parse_git_fsmonitor_prior_token(b"awacs-git-v2:malformed").is_err(),
            "a malformed AWACS cursor must not be mistaken for a foreign cursor"
        );
        assert!(
            parse_git_fsmonitor_prior_token(b"").is_err(),
            "an empty opaque cursor is not Git's numeric bootstrap placeholder"
        );
    }

    #[test]
    fn git_fsmonitor_bootstrap_response_forces_authoritative_refresh() {
        let mut response = Vec::new();
        write_git_fsmonitor_response_to(
            &mut response,
            "awacs-git-v2:verified-cursor",
            [b"/".as_slice()],
        )
        .expect("the bootstrap response is writable");
        assert_eq!(
            response, b"awacs-git-v2:verified-cursor\0/\0",
            "Git must receive both an established cursor and a whole-tree invalidation"
        );
    }

    #[test]
    fn direct_submodule_check_preserves_index_and_does_not_query_fsmonitor() {
        use std::os::unix::fs::PermissionsExt as _;

        fn run_git(cwd: &Path, arguments: &[&str]) -> Vec<u8> {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(arguments)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        for format in ["sha1", "sha256"] {
            let root = TestDir::new("direct-index-submodules");
            run_git(&root.path, &["init", &format!("--object-format={format}")]);
            fs::write(root.path.join("tracked"), b"tracked\n").unwrap();
            run_git(&root.path, &["add", "tracked"]);
            let hook = root.path.join("fsmonitor-hook");
            fs::write(
                &hook,
                b"#!/bin/sh\nprintf called >> hook.called\nprintf 'test-token\\0/\\0'\n",
            )
            .unwrap();
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
            run_git(
                &root.path,
                &["config", "core.fsmonitor", hook.to_str().unwrap()],
            );
            let index = root.path.join(".git/index");
            let marker = root.path.join("hook.called");
            let before = fs::read(&index).unwrap();
            assert!(!super::source_index_has_submodules(Path::new("git"), &root.path).unwrap());
            assert!(
                !marker.exists(),
                "the direct check must not query fsmonitor"
            );
            assert_eq!(fs::read(&index).unwrap(), before);

            let oid = "1".repeat(if format == "sha1" { 40 } else { 64 });
            run_git(
                &root.path,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{oid},submodule"),
                ],
            );
            for version in ["2", "3", "4"] {
                run_git(
                    &root.path,
                    &["update-index", &format!("--index-version={version}")],
                );
                let _ = fs::remove_file(&marker);
                let before = fs::read(&index).unwrap();
                assert!(super::source_index_has_submodules(Path::new("git"), &root.path).unwrap());
                assert!(
                    !marker.exists(),
                    "checking gitlinks must not query fsmonitor"
                );
                assert_eq!(fs::read(&index).unwrap(), before);
            }

            run_git(&root.path, &["update-index", "--split-index"]);
            let _ = fs::remove_file(&marker);
            assert!(super::source_index_has_submodules(Path::new("git"), &root.path).unwrap());
            assert!(
                marker.exists(),
                "split indexes must use Git's authoritative reader"
            );
        }
    }

    #[test]
    fn snapshot_git_index_open_rejects_links_and_pins_the_open_file() {
        use std::io::Read as _;
        use std::os::unix::fs::symlink;

        let root = TestDir::new("snapshot-index-links");
        let index = root.path.join("index");
        let foreign = root.path.join("foreign");
        fs::write(&index, b"source index").unwrap();
        fs::write(&foreign, b"foreign index").unwrap();
        let mut opened = super::open_snapshot_source_git_index(&index)
            .unwrap()
            .unwrap();
        fs::rename(&index, root.path.join("old-index")).unwrap();
        symlink(&foreign, &index).unwrap();
        assert!(
            super::open_snapshot_source_git_index(&index)
                .unwrap()
                .is_none()
        );
        let mut bytes = Vec::new();
        opened.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"source index");
        fs::remove_file(&index).unwrap();
        fs::hard_link(&foreign, &index).unwrap();
        assert!(
            super::open_snapshot_source_git_index(&index)
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read(&foreign).unwrap(), b"foreign index");
    }

    #[test]
    fn direct_worktree_add_rejects_foreign_index_handoff() {
        use std::os::unix::fs::symlink;

        fn git(repo: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let root = TestDir::new("foreign-index-handoff");
        let source = root.path.join("source");
        let foreign = root.path.join("foreign");
        let destination = root.path.join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&foreign).unwrap();
        for repo in [&source, &foreign] {
            git(repo, &["init", "-q"]);
            git(repo, &["config", "user.name", "Fixture"]);
            git(repo, &["config", "user.email", "fixture@example.invalid"]);
            fs::write(repo.join("tracked"), b"same object\n").unwrap();
            git(repo, &["add", "tracked"]);
            git(repo, &["commit", "-q", "-m", "fixture"]);
        }
        let foreign_index = foreign.join(".git/index");
        let before = fs::read(&foreign_index).unwrap();
        let source_index = source.join(".git/index");
        fs::remove_file(&source_index).unwrap();
        symlink(&foreign_index, &source_index).unwrap();
        let options = GitWorktreeAdd {
            source: &source,
            destination: &destination,
            reference: "HEAD",
            git: Path::new("git"),
            required: true,
            detach: true,
            force: 0,
            relative_paths: false,
            lock_reason: None,
            quiet: true,
            check_only: false,
            no_check: true,
        };
        let error = run_git_worktree_add(&options).unwrap_err();
        assert!(
            error.contains("source Git index is missing, linked, or not a regular file"),
            "{error}"
        );
        assert!(!destination.exists());
        assert_eq!(fs::read(&foreign_index).unwrap(), before);
        // Optional preflight allows the caller's normal Git fallback.
        run_git_worktree_add(&GitWorktreeAdd {
            required: false,
            check_only: true,
            no_check: false,
            ..options
        })
        .unwrap();
        assert!(!destination.exists());
        assert_eq!(fs::read(&foreign_index).unwrap(), before);
    }

    #[test]
    fn snapshot_git_metadata_accepts_relative_linked_worktree_backreference() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new("git-worktree-backreference");
        let source = root.path.join("source");
        let git_dir = root.path.join("admin/worktrees/source");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        let marker = source.join(".git");
        fs::write(&marker, "gitdir: ../admin/worktrees/source\n").unwrap();
        fs::write(git_dir.join("gitdir"), "../../../source/.git\n").unwrap();
        assert!(super::snapshot_git_metadata_matches(&source, &git_dir));
        fs::write(git_dir.join("gitdir"), marker.to_str().unwrap()).unwrap();
        assert!(super::snapshot_git_metadata_matches(&source, &git_dir));
        let other = root.path.join("other-gitfile");
        fs::write(&other, "different worktree\n").unwrap();
        fs::write(git_dir.join("gitdir"), other.to_str().unwrap()).unwrap();
        assert!(!super::snapshot_git_metadata_matches(&source, &git_dir));
        fs::remove_file(&marker).unwrap();
        symlink(&other, &marker).unwrap();
        assert!(!super::snapshot_git_metadata_matches(&source, &git_dir));
    }

    #[test]
    fn direct_worktree_add_rejects_foreign_git_metadata() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new("foreign-git-metadata");
        let source = root.path.join("source");
        let foreign = root.path.join("foreign");
        let destination = root.path.join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&foreign).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&foreign)
                .args(args)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Fixture"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        fs::write(foreign.join("private"), b"fixture\n").unwrap();
        git(&["add", "private"]);
        git(&["commit", "-q", "-m", "fixture"]);
        let before = fs::read(foreign.join(".git/index")).unwrap();
        let marker = source.join(".git");
        for gitfile in [false, true] {
            if gitfile {
                fs::write(
                    &marker,
                    format!("gitdir: {}\n", foreign.join(".git").display()),
                )
                .unwrap();
            } else {
                symlink(foreign.join(".git"), &marker).unwrap();
            }
            let error = run_git_worktree_add(&GitWorktreeAdd {
                source: &source,
                destination: &destination,
                reference: "HEAD",
                git: Path::new("git"),
                required: true,
                detach: true,
                force: 0,
                relative_paths: false,
                lock_reason: None,
                quiet: true,
                check_only: false,
                no_check: true,
            })
            .unwrap_err();
            assert!(
                error.contains("Git worktree or metadata does not match the snapshot source"),
                "{error}"
            );
            assert!(!destination.exists());
            assert_eq!(fs::read(foreign.join(".git/index")).unwrap(), before);
            fs::remove_file(&marker).unwrap();
        }
    }

    #[test]
    fn direct_worktree_add_rejects_configured_worktree_redirect() {
        let root = TestDir::new("configured-worktree-handoff");
        let source = root.path.join("source");
        let foreign = root.path.join("foreign");
        let destination = root.path.join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&foreign).unwrap();
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(args)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Fixture"]);
        run(&["config", "user.email", "fixture@example.invalid"]);
        fs::write(source.join("tracked"), b"fixture\n").unwrap();
        run(&["add", "tracked"]);
        run(&["commit", "-q", "-m", "fixture"]);
        run(&["config", "core.worktree", foreign.to_str().unwrap()]);
        let error = run_git_worktree_add(&GitWorktreeAdd {
            source: &source,
            destination: &destination,
            reference: "HEAD",
            git: Path::new("git"),
            required: true,
            detach: true,
            force: 0,
            relative_paths: false,
            lock_reason: None,
            quiet: true,
            check_only: false,
            no_check: true,
        })
        .unwrap_err();
        assert!(
            error.contains("Git worktree or metadata does not match the snapshot source"),
            "{error}"
        );
        assert!(!destination.exists());
    }

    #[test]
    fn direct_worktree_add_rejects_environment_redirected_index() {
        const CHILD: &str = "AWACS_TEST_DIRECT_INDEX_OVERRIDE_CHILD";
        const SOURCE: &str = "AWACS_TEST_DIRECT_INDEX_SOURCE";
        const DESTINATION: &str = "AWACS_TEST_DIRECT_INDEX_DESTINATION";
        if std::env::var_os(CHILD).is_some() {
            let source = PathBuf::from(std::env::var_os(SOURCE).unwrap());
            let destination = PathBuf::from(std::env::var_os(DESTINATION).unwrap());
            let index = PathBuf::from(std::env::var_os("GIT_INDEX_FILE").unwrap());
            let before = fs::read(&index).unwrap();
            let options = GitWorktreeAdd {
                source: &source,
                destination: &destination,
                reference: "HEAD",
                git: Path::new("git"),
                required: true,
                detach: true,
                force: 0,
                relative_paths: false,
                lock_reason: None,
                quiet: true,
                check_only: false,
                no_check: true,
            };
            let error = run_git_worktree_add(&options).unwrap_err();
            assert!(
                error.contains("Git repository or index is redirected by the environment"),
                "{error}"
            );
            assert!(!destination.exists());
            assert_eq!(fs::read(&index).unwrap(), before);
            return;
        }
        let root = TestDir::new("environment-index-handoff");
        let source = root.path.join("source");
        let destination = root.path.join("destination");
        fs::create_dir(&source).unwrap();
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(args)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Fixture"]);
        run(&["config", "user.email", "fixture@example.invalid"]);
        fs::write(source.join("tracked"), b"fixture\n").unwrap();
        run(&["add", "tracked"]);
        run(&["commit", "-q", "-m", "fixture"]);
        let foreign = root.path.join("foreign-index");
        fs::copy(source.join(".git/index"), &foreign).unwrap();
        let test_name = std::thread::current().name().unwrap().to_owned();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env(CHILD, "1")
            .env(SOURCE, &source)
            .env(DESTINATION, &destination)
            .env("GIT_INDEX_FILE", &foreign)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[test]
    fn rejects_conflicted_git_target_before_worktree_creation() {
        fn run_git(cwd: &Path, arguments: &[&str]) -> Vec<u8> {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {} failed: {}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        let root = TestDir::new("conflicted-git-target");
        let source = root.path.join("source");
        let destination = root.path.join("destination");
        fs::create_dir(&source).unwrap();
        run_git(&source, &["init"]);
        run_git(&source, &["config", "user.name", "Test User"]);
        run_git(&source, &["config", "user.email", "test@example.com"]);
        fs::write(source.join("tracked"), b"tracked\n").unwrap();
        run_git(&source, &["add", "tracked"]);
        run_git(&source, &["commit", "-m", "ordinary commit"]);
        let ordinary_revision = String::from_utf8(run_git(&source, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();
        reject_jj_conflicted_git_target(Path::new("git"), &source, "HEAD").unwrap();

        for name in [
            ".jjconflict-base-0",
            ".jjconflict-side-0",
            ".jjconflict-side-1",
        ] {
            fs::create_dir(source.join(name)).unwrap();
            fs::write(source.join(name).join("tracked"), b"conflict input\n").unwrap();
        }
        fs::write(source.join("JJ-CONFLICT-README"), b"conflicted commit\n").unwrap();
        run_git(&source, &["add", "."]);
        run_git(&source, &["commit", "-m", "conflicted commit"]);

        let error = run_git_worktree_add(&GitWorktreeAdd {
            source: &source,
            destination: &destination,
            reference: "HEAD",
            git: Path::new("git"),
            required: false,
            detach: false,
            force: 0,
            relative_paths: false,
            lock_reason: None,
            quiet: false,
            check_only: true,
            no_check: false,
        })
        .unwrap_err();
        assert!(
            error.contains("cannot check out conflicted JJ commit HEAD"),
            "a conflicted target must be rejected even when snapshot mode is optional: {error}"
        );
        assert!(
            error.contains("resolve the conflicts or choose an unconflicted revision"),
            "the error must explain how to proceed safely: {error}"
        );
        assert!(
            !destination.exists(),
            "the conflict must be detected before creating a worktree or snapshot"
        );
        reject_jj_conflicted_git_target(Path::new("git"), &source, &ordinary_revision).unwrap();
    }

    #[test]
    fn parses_dirty_git_paths_for_snapshot_reset() {
        let changes = parse_source_git_changes(
            b" M modified\0A  added\0R  renamed\0old-name\0?? untracked-dir/\0",
        )
        .unwrap();
        assert_eq!(
            changes.tracked_paths,
            vec![
                b"modified".to_vec(),
                b"added".to_vec(),
                b"renamed".to_vec(),
                b"old-name".to_vec(),
            ]
        );
        assert_eq!(changes.untracked_paths, vec![b"untracked-dir/".to_vec()]);
    }

    #[test]
    fn dirty_source_removes_untracked_directories_before_snapshot_reset() {
        fn run_git(cwd: &Path, arguments: &[&str]) -> Vec<u8> {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {} failed: {}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        let root = TestDir::new("dirty-snapshot-checkout");
        let source = root.path.join("source");
        let destination = root.path.join("destination");
        fs::create_dir(&source).unwrap();
        run_git(&source, &["init"]);
        run_git(&source, &["config", "user.name", "Test User"]);
        run_git(&source, &["config", "user.email", "test@example.com"]);
        fs::write(
            source.join(".gitignore"),
            b"ignored-untracked\nmixed/ignored-output\n",
        )
        .unwrap();
        fs::write(source.join("file"), b"base\n").unwrap();
        fs::write(source.join("old-only"), b"old\n").unwrap();
        run_git(&source, &["add", ".gitignore", "file", "old-only"]);
        run_git(&source, &["commit", "-m", "source"]);
        let source_target = String::from_utf8(run_git(&source, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();

        fs::write(source.join("file"), b"target\n").unwrap();
        fs::write(source.join("target-only"), b"target only\n").unwrap();
        run_git(&source, &["add", "file", "target-only"]);
        run_git(&source, &["commit", "-m", "target"]);
        let target = String::from_utf8(run_git(&source, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();
        run_git(&source, &["checkout", "--detach", &source_target]);
        fs::write(source.join("file"), b"staged\n").unwrap();
        run_git(&source, &["add", "file"]);
        fs::write(source.join("file"), b"unstaged\n").unwrap();
        fs::write(source.join("source-untracked"), b"remove me\n").unwrap();
        fs::write(source.join("ignored-untracked"), b"keep me\n").unwrap();
        fs::create_dir(source.join("mixed")).unwrap();
        fs::write(source.join("mixed/source-untracked"), b"remove me\n").unwrap();
        fs::write(source.join("mixed/ignored-output"), b"keep me\n").unwrap();

        run_git(
            &source,
            &[
                "worktree",
                "add",
                "--no-checkout",
                destination.to_str().unwrap(),
                &target,
            ],
        );
        fs::write(destination.join("file"), b"unstaged\n").unwrap();
        fs::write(destination.join("old-only"), b"old\n").unwrap();
        fs::write(destination.join("source-untracked"), b"remove me\n").unwrap();
        fs::write(destination.join("ignored-untracked"), b"keep me\n").unwrap();
        fs::create_dir(destination.join("mixed")).unwrap();
        fs::write(destination.join("mixed/source-untracked"), b"remove me\n").unwrap();
        fs::write(destination.join("mixed/ignored-output"), b"keep me\n").unwrap();
        let source_index = super::git_path(Path::new("git"), &source, "index").unwrap();
        let destination_index = super::git_path(Path::new("git"), &destination, "index").unwrap();

        let changes = parse_source_git_changes(&run_git(
            &source,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=normal",
                "--ignore-submodules=none",
            ],
        ))
        .unwrap();
        assert!(
            changes.untracked_paths.contains(&b"mixed/".to_vec()),
            "normal-mode status should collapse the untracked mixed directory"
        );
        fs::copy(&source_index, &destination_index).unwrap();
        materialize_snapshot_checkout(Path::new("git"), &destination, &target, &changes).unwrap();

        assert_eq!(fs::read(destination.join("file")).unwrap(), b"target\n");
        assert_eq!(
            fs::read(destination.join("target-only")).unwrap(),
            b"target only\n"
        );
        assert!(!destination.join("source-untracked").exists());
        assert!(!destination.join("mixed/source-untracked").exists());
        assert_eq!(
            fs::read(destination.join("ignored-untracked")).unwrap(),
            b"keep me\n"
        );
        assert!(
            !destination.join("mixed/ignored-output").exists(),
            "ignored files inside a reported untracked directory are removed with it"
        );
        assert!(
            run_git(&destination, &["status", "--porcelain"]).is_empty(),
            "destination must be clean after hard reset"
        );
        assert_eq!(fs::read(source.join("file")).unwrap(), b"unstaged\n");
        assert!(source.join("source-untracked").exists());
        assert!(source.join("mixed/source-untracked").exists());
        assert!(source.join("ignored-untracked").exists());
        assert!(source.join("mixed/ignored-output").exists());
    }

    #[test]
    fn reads_only_committed_jj_adoption_seeds() {
        let root = TestDir::new("jj-adoption-seed");
        assert_eq!(read_pending_jj_source_seed(&root.path).unwrap(), None);

        let state = root.path.join(".jj/working_copy");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("subvolume_mode"), b"enabling\n").unwrap();
        assert!(
            read_pending_jj_source_seed(&root.path)
                .unwrap_err()
                .contains("not committed")
        );

        fs::write(state.join("subvolume_mode"), b"snapshot-backed\n").unwrap();
        fs::write(
            state.join("awacs-adoption-seed"),
            "jj-awacs-adoption-v2:11111111-1111-1111-1111-111111111111:\
             22222222-2222-2222-2222-222222222222:33333333-3333-3333-3333-333333333333\n",
        )
        .unwrap();
        let seed = read_pending_jj_source_seed(&root.path)
            .unwrap()
            .expect("committed marker has a seed");
        assert_eq!(seed.baseline_filesystem_uuid, [0x11; 16]);
        assert_eq!(seed.baseline_snapshot_uuid, [0x22; 16]);
        assert_eq!(seed.baseline_owner_id, [0x33; 16]);

        let mut checkout = JJ_WORKING_COPY_STATE_MAGIC.to_vec();
        checkout.extend_from_slice(&[0x92, 0x01, 0x10]);
        checkout.extend_from_slice(&[0x44; 16]);
        fs::write(state.join("checkout"), checkout).unwrap();
        fs::write(
            state.join("awacs-adoption-seed"),
            "jj-awacs-adoption-v1:11111111-1111-1111-1111-111111111111:\
             22222222-2222-2222-2222-222222222222\n",
        )
        .unwrap();
        let seed = read_pending_jj_source_seed(&root.path)
            .unwrap()
            .expect("v1 marker recovers owner from compact journal");
        assert_eq!(seed.baseline_owner_id, [0x44; 16]);
    }

    #[test]
    fn stages_locked_source_jj_state_without_requiring_copied_metadata() {
        for layout in ["absent", "empty-placeholder", "copied-directory"] {
            let root = TestDir::new("pending-jj-state");
            let source = root.path.join("source");
            let source_state = source.join(".jj/working_copy");
            let destination = root.path.join("destination");
            fs::create_dir_all(&source_state).unwrap();
            fs::create_dir_all(source.join(".jj/repo")).unwrap();
            fs::write(source.join(".jj/repo/keep"), b"source repository").unwrap();
            fs::create_dir(&destination).unwrap();
            match layout {
                "empty-placeholder" => fs::create_dir(destination.join(".jj")).unwrap(),
                "copied-directory" => {
                    let copied_state = destination.join(".jj/working_copy");
                    fs::create_dir_all(&copied_state).unwrap();
                    fs::write(copied_state.join("checkout"), b"stale copied journal").unwrap();
                }
                _ => {}
            }
            let files: [(&str, &[u8]); 4] = [
                ("checkout", b"compact source journal"),
                ("working_copy_state", b"legacy source state"),
                ("subvolume_mode", b"snapshot-backed\n"),
                ("type", b"local"),
            ];
            for (name, contents) in files {
                fs::write(source_state.join(name), contents).unwrap();
            }
            let lock = lock_jj_working_copy_for_snapshot(&source).unwrap().unwrap();
            let pending_state = root.path.join("git-admin/pending");
            fs::create_dir_all(&pending_state).unwrap();
            fs::write(pending_state.join("obsolete"), b"old staging").unwrap();

            stage_pending_jj_working_copy_state(&source, &destination, &pending_state, &lock)
                .unwrap();

            for (name, contents) in files {
                assert_eq!(
                    fs::read(pending_state.join(name)).unwrap(),
                    contents,
                    "{layout}: {name}"
                );
                assert_eq!(fs::read(source_state.join(name)).unwrap(), contents);
            }
            assert!(
                lock.path.exists(),
                "source lock must remain held through staging"
            );
            assert_eq!(
                fs::read(source.join(".jj/repo/keep")).unwrap(),
                b"source repository"
            );
            assert!(!pending_state.join("working_copy.lock").exists());
            assert!(!pending_state.join("obsolete").exists());
            assert!(!destination.join(".jj").exists());
        }
    }

    #[test]
    fn pending_jj_state_rejects_missing_source_journal_without_removing_destination_metadata() {
        let root = TestDir::new("pending-jj-state-missing-journal");
        let source = root.path.join("source");
        let source_state = source.join(".jj/working_copy");
        let destination = root.path.join("destination");
        let copied_state = destination.join(".jj/working_copy");
        fs::create_dir_all(&source_state).unwrap();
        fs::write(source_state.join("subvolume_mode"), b"snapshot-backed\n").unwrap();
        fs::create_dir_all(&copied_state).unwrap();
        fs::write(
            copied_state.join("checkout"),
            b"do not use this copied journal",
        )
        .unwrap();
        let lock = lock_jj_working_copy_for_snapshot(&source).unwrap().unwrap();
        let pending_state = root.path.join("git-admin/pending");
        fs::create_dir_all(pending_state.parent().unwrap()).unwrap();

        let error =
            stage_pending_jj_working_copy_state(&source, &destination, &pending_state, &lock)
                .unwrap_err();

        assert!(error.contains("source JJ working-copy state"));
        assert!(error.contains("has no committed journal"));
        assert_eq!(
            fs::read(copied_state.join("checkout")).unwrap(),
            b"do not use this copied journal"
        );
        assert!(lock.path.exists());
    }

    #[test]
    fn pending_jj_state_requires_lock_for_the_same_source() {
        let root = TestDir::new("pending-jj-state-lock-owner");
        let locked_source = root.path.join("locked-source");
        fs::create_dir_all(locked_source.join(".jj/working_copy")).unwrap();
        let lock = lock_jj_working_copy_for_snapshot(&locked_source)
            .unwrap()
            .unwrap();
        let pending_state = root.path.join("pending");
        let error = stage_pending_jj_working_copy_state(
            &root.path.join("other-source"),
            &root.path.join("destination"),
            &pending_state,
            &lock,
        )
        .unwrap_err();
        assert!(error.contains("does not protect the journal source"));
        assert!(!pending_state.exists());
    }

    #[test]
    fn failed_subvolume_rollback_does_not_fall_back_to_recursive_deletion() {
        let root = TestDir::new("worktree-rollback-no-fallback");
        let destination = root.path.join("destination");
        fs::create_dir(&destination).unwrap();
        fs::write(
            destination.join("keep"),
            b"preserve on failed subvolume verification",
        )
        .unwrap();
        let options = GitWorktreeAdd {
            source: &root.path,
            destination: &destination,
            reference: "HEAD",
            git: Path::new("/nonexistent-git-must-not-run"),
            required: true,
            detach: false,
            force: 0,
            relative_paths: false,
            lock_reason: None,
            quiet: true,
            check_only: false,
            no_check: false,
        };

        assert!(
            rollback_git_worktree(&options, super::GitWorktreeDestination::Snapshot, None).is_err()
        );
        assert_eq!(
            fs::read(destination.join("keep")).unwrap(),
            b"preserve on failed subvolume verification"
        );
    }

    #[test]
    fn failed_snapshot_publication_preserves_concurrently_created_destination() {
        let root = TestDir::new("worktree-rollback-unowned-destination");
        let administrative_directory = root.path.join("worktrees/registered");
        fs::create_dir_all(&administrative_directory).unwrap();
        fs::write(
            administrative_directory.join("gitdir"),
            b"destination/.git\n",
        )
        .unwrap();
        let registration = super::GitWorktreeRegistration::open(&administrative_directory).unwrap();
        let other_registration = root.path.join("worktrees/other");
        fs::create_dir(&other_registration).unwrap();
        fs::write(other_registration.join("keep"), b"other registration").unwrap();
        // The registered empty worktree was removed before rsnapshot began.
        // Another creator occupies the path before its non-replacing publish.
        let destination = root.path.join("destination");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), b"concurrent creator's files").unwrap();
        let options = GitWorktreeAdd {
            source: &root.path,
            destination: &destination,
            reference: "HEAD",
            git: Path::new("/nonexistent-git-must-not-run"),
            required: true,
            detach: false,
            force: 0,
            relative_paths: false,
            lock_reason: None,
            quiet: true,
            check_only: false,
            no_check: false,
        };

        rollback_git_worktree(
            &options,
            super::GitWorktreeDestination::Absent,
            Some(&registration),
        )
        .unwrap();

        assert_eq!(
            fs::read(destination.join("keep")).unwrap(),
            b"concurrent creator's files"
        );
        assert_eq!(
            fs::read(other_registration.join("keep")).unwrap(),
            b"other registration"
        );
        assert!(!administrative_directory.exists());
    }

    #[test]
    fn copied_jj_cleanup_handles_small_and_linked_repositories_without_following_links() {
        for shape in [
            "pointer",
            "symlink",
            "missing",
            "one-subtree",
            "multiple-subtrees",
        ] {
            let root = TestDir::new(&format!("jj-cleanup-{shape}"));
            let destination = root.path.join("destination");
            let metadata = destination.join(".jj");
            fs::create_dir_all(&metadata).unwrap();
            let outside = root.path.join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("keep"), b"shared repository").unwrap();
            match shape {
                "pointer" => {
                    fs::write(metadata.join("repo"), outside.as_os_str().as_bytes()).unwrap()
                }
                "symlink" => std::os::unix::fs::symlink(&outside, metadata.join("repo")).unwrap(),
                "one-subtree" => {
                    fs::create_dir_all(metadata.join("repo/store")).unwrap();
                    fs::write(metadata.join("repo/store/object"), b"copied").unwrap();
                }
                "multiple-subtrees" => {
                    for name in ["op_store/operations", "index/op_links", "store/extra"] {
                        let subtree = metadata.join("repo").join(name);
                        fs::create_dir_all(&subtree).unwrap();
                        fs::write(subtree.join("copied-object"), b"discarded").unwrap();
                    }
                    std::os::unix::fs::symlink(&outside, metadata.join("repo/index/outside"))
                        .unwrap();
                }
                _ => {}
            }
            super::remove_copied_jj_metadata(&destination).unwrap();
            assert!(!metadata.exists());
            assert_eq!(
                fs::read(outside.join("keep")).unwrap(),
                b"shared repository"
            );
        }
    }

    #[test]
    fn copied_jj_cleanup_anchors_repository_and_subtree_replacements() {
        let root = TestDir::new("jj-cleanup-repository-exchange");
        let repository_path = root.path.join("repo");
        for name in ["index", "store"] {
            fs::create_dir_all(repository_path.join(name)).unwrap();
            fs::write(repository_path.join(name).join("copied-object"), b"copied").unwrap();
        }
        let outside = root.path.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"must survive").unwrap();
        let repository = fs::File::open(&repository_path).unwrap();
        let moved_repository = root.path.join("moved-repository");
        fs::rename(&repository_path, &moved_repository).unwrap();
        std::os::unix::fs::symlink(&outside, &repository_path).unwrap();
        let directories = super::copied_jj_repository_subtrees(&repository).unwrap();
        assert_eq!(directories.len(), 2);
        // Exchange a final component after enumeration as well as its ancestor.
        fs::remove_dir_all(moved_repository.join("index")).unwrap();
        std::os::unix::fs::symlink(&outside, moved_repository.join("index")).unwrap();
        super::remove_copied_jj_subtrees(&directories, &super::remove_tree).unwrap();
        assert_eq!(fs::read_dir(&moved_repository).unwrap().count(), 0);
        assert!(fs::symlink_metadata(&repository_path).unwrap().is_symlink());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"must survive");
    }

    #[test]
    fn copied_jj_cleanup_never_recurses_into_replaced_metadata_directory() {
        for symlink in [false, true] {
            let root = TestDir::new("jj-cleanup-metadata-exchange");
            let metadata_path = root.path.join(".jj");
            fs::create_dir_all(metadata_path.join("repo/index")).unwrap();
            fs::write(metadata_path.join("repo/index/object"), b"copied").unwrap();
            let metadata = fs::File::open(&metadata_path).unwrap();
            let moved_metadata = root.path.join("moved-metadata");
            fs::rename(&metadata_path, &moved_metadata).unwrap();
            let replacement = root.path.join("replacement");
            fs::create_dir(&replacement).unwrap();
            fs::write(replacement.join("keep"), b"replacement contents").unwrap();
            if symlink {
                std::os::unix::fs::symlink(&replacement, &metadata_path).unwrap();
            } else {
                fs::rename(&replacement, &metadata_path).unwrap();
            }
            let destination = fs::File::open(&root.path).unwrap();
            let error =
                super::finish_copied_jj_metadata_removal(&destination, &metadata).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("identity changed during removal")
            );
            assert_eq!(
                fs::read(moved_metadata.join("repo/index/object")).unwrap(),
                b"copied"
            );
            assert_eq!(
                fs::read(metadata_path.join("keep")).unwrap(),
                b"replacement contents"
            );
        }
    }

    #[test]
    #[ignore = "requires a writable Btrfs directory in JJ_TEST_BTRFS_ROOT"]
    fn copied_jj_cleanup_destroys_subvolumes_without_unlinking_metadata_files() {
        use std::io::Read as _;
        use std::os::unix::fs::MetadataExt as _;

        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Err(error) = super::remove_tree(&self.0) {
                    eprintln!("failed to clean Btrfs test fixture: {error}");
                }
            }
        }

        let base = std::env::var_os("JJ_TEST_BTRFS_ROOT")
            .expect("set JJ_TEST_BTRFS_ROOT to a writable Btrfs directory");
        for shape in ["metadata", "repository", "subtree"] {
            let root = tempfile::tempdir_in(&base).unwrap();
            let destination = root.path().join("destination");
            fs::create_dir(&destination).unwrap();
            let _cleanup = Cleanup(destination.clone());
            let metadata = destination.join(".jj");
            let repository = metadata.join("repo");
            let subvolume = match shape {
                "metadata" => metadata.clone(),
                "repository" => {
                    fs::create_dir(&metadata).unwrap();
                    repository.clone()
                }
                "subtree" => {
                    fs::create_dir_all(&repository).unwrap();
                    repository.join("index")
                }
                _ => unreachable!(),
            };
            let created = std::process::Command::new("btrfs")
                .args(["subvolume", "create"])
                .arg(&subvolume)
                .output()
                .unwrap();
            assert!(created.status.success(), "{created:?}");
            let mut retained = Vec::new();
            for name in ["index", "store", "op_store"] {
                let subtree = repository.join(name);
                fs::create_dir_all(&subtree).unwrap();
                let path = subtree.join("keep");
                fs::write(&path, b"retained subvolume contents").unwrap();
                if shape != "subtree" || name == "index" {
                    retained.push(fs::File::open(&path).unwrap());
                }
            }

            super::remove_copied_jj_metadata(&destination).unwrap();

            assert!(!metadata.exists(), "{shape}");
            for mut file in retained {
                assert_eq!(file.metadata().unwrap().nlink(), 1, "{shape}");
                let mut contents = Vec::new();
                file.read_to_end(&mut contents).unwrap();
                assert_eq!(contents, b"retained subvolume contents", "{shape}");
            }
        }
    }

    #[test]
    fn copied_jj_cleanup_bounds_workers_and_joins_them_on_error() {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        let directories: Vec<std::path::PathBuf> = (0..6).map(|i| i.to_string().into()).collect();
        let started = AtomicUsize::new(0);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let completed = AtomicUsize::new(0);
        let first_workers = Barrier::new(3);
        let error = super::remove_copied_jj_subtrees(&directories, &|path| {
            let concurrent = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(concurrent, Ordering::SeqCst);
            if started.fetch_add(1, Ordering::SeqCst) < 3 {
                first_workers.wait();
            }
            let result = if path == std::path::Path::new("0") {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else if path == std::path::Path::new("1") {
                // A concurrently removed child is ignored like remove_dir_all.
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            };
            active.fetch_sub(1, Ordering::SeqCst);
            result
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(completed.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn git_fsmonitor_identifies_uninitialized_roots() {
        let uninitialized = btrfs_awacs::scan::ScanError::new(
            btrfs_awacs::scan::ScanErrorKind::Unavailable,
            "open initialized AWACS root: /tmp/new-worktree is not initialized",
        );
        assert!(is_uninitialized_awacs_root(&uninitialized));

        let unavailable = btrfs_awacs::scan::ScanError::new(
            btrfs_awacs::scan::ScanErrorKind::Unavailable,
            "permission denied",
        );
        assert!(!is_uninitialized_awacs_root(&unavailable));
    }

    #[test]
    fn normalizes_worktree_destination_before_descendant_check() {
        let root = TestDir::new("worktree-destination");
        let source = root.path.join("source");
        fs::create_dir(&source).unwrap();

        let sibling = canonical_path_for_creation(&source.join("../sibling")).unwrap();
        assert_eq!(sibling, root.path.join("sibling"));
        assert!(!sibling.starts_with(&source));

        let missing_parent =
            canonical_path_for_creation(&source.join("../new-parent/sibling")).unwrap();
        assert_eq!(missing_parent, root.path.join("new-parent/sibling"));
        assert!(!missing_parent.starts_with(&source));

        let nested = canonical_path_for_creation(&source.join("nested")).unwrap();
        assert!(nested.starts_with(&source));
    }

    use std::path::Path;
    use std::path::PathBuf;

    #[test]
    fn parses_recursive_snapshot_source_and_destination() {
        let snapshot =
            Cli::try_parse_from(["awacs", "rsnapshot", "source tree", "destination tree"]).unwrap();
        assert!(matches!(
            snapshot.command,
            CliCommand::Rsnapshot { source, destination }
                if source == Path::new("source tree")
                    && destination == Path::new("destination tree")
        ));
        assert!(Cli::try_parse_from(["awacs", "rsnapshot"]).is_err());
        assert!(Cli::try_parse_from(["awacs", "rsnapshot", "source"]).is_err());
        assert!(
            Cli::try_parse_from(["awacs", "rsnapshot", "source", "destination", "extra"]).is_err()
        );
    }

    #[test]
    fn parses_snap_and_compare_subcommands() {
        let init = Cli::try_parse_from(["awacs", "init", ".", "--compress=true"]).unwrap();
        assert!(matches!(
            init.command,
            CliCommand::Init {
                root,
                compress: Some(true),
                keep: false,
            } if root == Path::new(".")
        ));
        let snap = Cli::try_parse_from(["awacs", "snap", "."]).unwrap();
        assert!(matches!(
            snap.command,
            CliCommand::Snap { source } if source == Path::new(".")
        ));
        let compare = Cli::try_parse_from(["awacs", "compare", "."]).unwrap();
        assert!(matches!(
            compare.command,
            CliCommand::Compare { source } if source == Path::new(".")
        ));
        let dump = Cli::try_parse_from(["awacs", "dump"]).unwrap();
        assert!(matches!(
            dump.command,
            CliCommand::Dump {
                root,
                manager_db: None,
                managed_dir: None,
            } if root == Path::new(".")
        ));
        let dump = Cli::try_parse_from(["awacs", "dump", "/workspace"]).unwrap();
        assert!(matches!(
            dump.command,
            CliCommand::Dump { root, .. } if root == Path::new("/workspace")
        ));
        let debug =
            Cli::try_parse_from(["awacs", "debug", "changed-objects", "/source", "/target"])
                .unwrap();
        assert!(matches!(
            debug.command,
            CliCommand::Debug {
                command: DebugCommand::ChangedObjects {
                    source_snapshot,
                    target_snapshot,
                    summary: false,
                },
            } if source_snapshot == Path::new("/source")
                && target_snapshot == Path::new("/target")
        ));
    }

    #[test]
    fn rejects_old_interface_and_extra_arguments() {
        assert!(Cli::try_parse_from(["awacs", "changes", "."]).is_err());
        assert!(Cli::try_parse_from(["awacs", "compare", "--timing", "."]).is_err());
        assert!(Cli::try_parse_from(["awacs", "snap", ".", "extra"]).is_err());
        for command in [
            "__changed-objects-send",
            "__btrfs-inspect",
            "__broker-changed-objects",
            "__nested-boundary-smoke",
            "__namespace-view-smoke",
        ] {
            assert!(Cli::try_parse_from(["awacs", command]).is_err());
        }
    }

    #[test]
    fn parses_git_worktree_lifecycle_subcommands() {
        let add = Cli::try_parse_from([
            "awacs",
            "git",
            "worktree-add",
            "--source",
            "/source",
            "--destination",
            "/destination",
            "--ref",
            "HEAD",
            "--required",
            "--detach",
            "--force",
            "--relative-paths",
            "--lock-reason",
            "held",
            "--quiet",
            "--check-only",
            "--no-check",
        ])
        .unwrap();
        assert!(matches!(
            add.command,
            CliCommand::Git {
                command: GitCommand::WorktreeAdd {
                    source,
                    destination,
                    reference,
                    required: true,
                    detach: true,
                    force: 1,
                    relative_paths: true,
                    lock_reason: Some(reason),
                    quiet: true,
                    check_only: true,
                    no_check: true,
                    ..
                },
            } if source == Path::new("/source")
                && destination == Path::new("/destination")
                && reference == "HEAD"
                && reason == "held"
        ));
        let remove =
            Cli::try_parse_from(["awacs", "git", "worktree-remove", "--path", "/target"]).unwrap();
        assert!(matches!(
            remove.command,
            CliCommand::Git {
                command: GitCommand::WorktreeRemove { path },
            } if path == Path::new("/target")
        ));
    }

    #[test]
    fn default_help_lists_every_subcommand() {
        let help = Cli::command().render_long_help().to_string();
        for command in [
            "init",
            "git",
            "rsnapshot",
            "snap",
            "compare",
            "dump",
            "debug",
            "broker-serve",
        ] {
            assert!(help.contains(command), "help omitted {command:?}\n{help}");
        }
        assert!(
            !help.contains("__"),
            "help lists an internal command\n{help}"
        );
    }

    #[test]
    fn manager_database_override_does_not_require_a_live_root() {
        assert_eq!(
            manager_db_path(
                Path::new("/missing-root"),
                Some(Path::new("/state/manager.sqlite3")),
            )
            .unwrap(),
            Path::new("/state/manager.sqlite3")
        );
    }

    #[test]
    fn manager_dump_discovers_current_schema_tables() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(include_str!("store_schema.sql"))
            .unwrap();
        let counts = manager_table_counts(&connection).unwrap();
        assert!(
            counts
                .iter()
                .any(|(table, count)| table == "comparison_objects" && *count == 0)
        );
        assert!(
            counts
                .iter()
                .any(|(table, count)| table == "replay_spools" && *count == 0)
        );
        assert!(
            !counts
                .iter()
                .any(|(table, _)| table == "revision_checkpoints")
        );
    }

    #[test]
    fn manager_dump_checks_managed_cut_wrappers() {
        let (entry, directory) =
            managed_snapshot_paths(Path::new("/state/managed/cut-example/snapshot")).unwrap();
        assert_eq!(entry, Path::new("/state/managed/cut-example"));
        assert_eq!(directory, Path::new("/state/managed"));
    }

    #[test]
    fn selects_the_last_two_existing_snapshots() {
        let snapshots = vec![
            PathBuf::from("snapshot-3"),
            PathBuf::from("snapshot-1"),
            PathBuf::from("snapshot-2"),
        ];

        assert_eq!(
            last_two_snapshots(snapshots),
            Some((PathBuf::from("snapshot-2"), PathBuf::from("snapshot-3")))
        );
    }

    #[test]
    fn compare_requires_two_snapshots() {
        let snapshot_dir = PathBuf::from("volume/.btrfs-awacs");
        for snapshots in [vec![], vec![PathBuf::from("snapshot-1")]] {
            let count = snapshots.len();
            assert_eq!(
                require_last_two_snapshots(&snapshot_dir, snapshots).unwrap_err(),
                format!(
                    "compare requires at least two snapshots in volume/.btrfs-awacs (found \
                     {count})"
                )
            );
        }
    }

    #[test]
    fn counts_unique_file_paths_and_ignores_directories() {
        let dump = r#"
            snapshot ./snapshot-2 uuid=abc transid=2 parent_uuid=def parent_transid=1
            mkdir ./snapshot-2/new-dir
            mkfile ./snapshot-2/new.txt
            update_extent ./snapshot-2/new.txt offset=0 len=12
            chmod ./snapshot-2/new.txt mode=0644
            unlink ./snapshot-2/old.txt
            rmdir ./snapshot-2/old-dir
            end
        "#;

        let changed = changed_paths(dump);
        assert_eq!(changed.len(), 2);
        assert!(changed.contains("./snapshot-2/new.txt"));
        assert!(changed.contains("./snapshot-2/old.txt"));
    }

    #[test]
    fn handles_quoted_paths_and_counts_rename_once() {
        let dump = r#"
            mkfile "./snapshot-2/a file"
            update_extent "./snapshot-2/a file" offset=0 len=1
            rename "./snapshot-2/old name" -> "./snapshot-2/new name"
            link "./snapshot-2/a link" -> "./snapshot-2/a file"
        "#;

        let changed = changed_paths(dump);
        assert_eq!(changed.len(), 3);
        assert!(changed.contains("./snapshot-2/a file"));
        assert!(changed.contains("./snapshot-2/new name"));
        assert!(changed.contains("./snapshot-2/a link"));
    }

    #[test]
    fn handles_receive_dump_dest_rename_syntax() {
        let dump = r#"
            rename ./snapshot-2/old\ name dest=./snapshot-2/new\ name
            rename "./snapshot-2/another old name" dest="./snapshot-2/another new name"
        "#;

        let changed = changed_paths(dump);
        assert_eq!(changed.len(), 2);
        assert!(changed.contains("./snapshot-2/new name"));
        assert!(changed.contains("./snapshot-2/another new name"));
    }

    fn changed_objects_fixture() -> ChangedObjectsV2 {
        let reference = Reference {
            ino: 300,
            parent_ino: 256,
            name: b"new file".to_vec(),
        };
        let manifest = ChangedObjectsManifest {
            objects: BTreeMap::from([
                (
                    300,
                    ObjectChange {
                        ino: 300,
                        old_generation: None,
                        new_generation: Some(7),
                        change_mask: CHANGED_OBJECT_INODE
                            | CHANGED_OBJECT_REF
                            | CHANGED_OBJECT_CREATED,
                    },
                ),
                (
                    301,
                    ObjectChange {
                        ino: 301,
                        old_generation: Some(8),
                        new_generation: None,
                        change_mask: CHANGED_OBJECT_INODE | CHANGED_OBJECT_DELETED,
                    },
                ),
            ]),
            ref_adds: BTreeSet::from([reference]),
            ref_deletes: BTreeSet::new(),
            raw_ref_adds: 2,
            raw_ref_deletes: 1,
        };
        ChangedObjectsV2 {
            header: ChangedObjectsV2Header {
                boundary_records: true,
                dirty_witness: true,
                fs_uuid: [1; 16],
                source_uuid: [2; 16],
                target_uuid: [3; 16],
                source_ctransid: 10,
                target_ctransid: 11,
                source_root_id: 256,
                target_root_id: 257,
            },
            completion: ChangedObjectsV2Completion {
                record_count: 4,
                stream_bytes: 200,
            },
            manifest,
            target_objects: BTreeMap::from([(
                300,
                TargetObjectMetadata {
                    generation: 7,
                    change_sequence: 12,
                    transid: 11,
                    mode: 0o100644,
                    nlink: 1,
                    uid: 1000,
                    gid: 1000,
                    rdev: 0,
                },
            )]),
            target_security_xattrs: BTreeMap::from([(
                300,
                BTreeMap::from([(b"security.test".to_vec(), b"value".to_vec())]),
            )]),
            boundary_adds: BTreeSet::new(),
            boundary_deletes: BTreeSet::new(),
        }
    }

    #[test]
    fn summarizes_current_changed_object_stream() {
        assert_eq!(
            summarize_changed_objects(&changed_objects_fixture().manifest),
            ChangedObjectsSummary {
                objects: 2,
                created: 1,
                deleted: 1,
                raw_ref_adds: 2,
                raw_ref_deletes: 1,
                net_ref_adds: 1,
                net_ref_deletes: 0,
            }
        );
    }

    #[test]
    fn pretty_prints_changed_object_stream_records() {
        let formatted = format_changed_objects_stream(&changed_objects_fixture());
        for expected in [
            "Changed-object stream (v2)",
            "source: 02020202-0202-0202-0202-020202020202 root=256 transaction=10",
            "target: 03030303-0303-0303-0303-030303030303 root=257 transaction=11",
            "subvolume-boundaries=true dirty-witness=true",
            "inode=300 generations=None->Some(7) changes=inode,references,created",
            "mode=0o100644 links=1 uid=1000 gid=1000",
            "attribute \"security.test\"=0x76616c7565",
            "inode=301 generations=Some(8)->None changes=inode,deleted",
            "inode=300 parent=256 name=\"new file\"",
            "Completion: records=4 bytes=232 raw-references=+2/-1",
        ] {
            assert!(
                formatted.contains(expected),
                "missing {expected:?} in:\n{formatted}"
            );
        }
    }
}
