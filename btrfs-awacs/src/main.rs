use btrfs_awacs::bootstrap::{
    InitProgress, initialize_descendant_root_with_watch_consumer, initialize_root,
    initialize_root_with_watch_consumer, require_retained_descendant_baseline, root_state_key,
    state_root_for_root,
};
use btrfs_awacs::broker::{
    ChangedObjectsExecution, ExpectedSubvolume, SeqPacketListener, execute_changed_objects,
};
use btrfs_awacs::broker_protocol::BrokerDispatcher;
use btrfs_awacs::btrfs::{
    OpenedSubvolume, destroy_snapshot, send_changed_objects, set_subvolume_readonly,
    supports_changed_objects_v2,
};
use btrfs_awacs::manager::{PERMISSION_CUT, PERMISSION_READ, Permissions, Principal};
use btrfs_awacs::manifest::{
    CHANGED_OBJECTS_V2_MAGIC, parse_changed_objects, parse_changed_objects_v2,
};
use btrfs_awacs::namespace::NamespaceMonitor;
use btrfs_awacs::scan::{
    BeginScanRequest, Invalidation, ScanClient, ScanOutcome, SnapshotBaseline, SnapshotIdentity,
};
use btrfs_awacs::scan_facade::DirectScanClient;
use btrfs_awacs::service::{ChangesOptions, InitializeOptions, Service, ServiceConfig};
use btrfs_awacs::store::{ServiceMetadata, Store};
use btrfs_awacs::subvolume_migration::{MigrationOptions, convert_subvolume_root};
use clap::{Parser, Subcommand as ClapSubcommand, ValueEnum};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::ops::Range;
use std::os::fd::{AsFd, AsRawFd as _};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

const SNAPSHOT_DIR: &str = ".btrfs-awacs";
const SNAPSHOT_PREFIX: &str = "snapshot-";
const MANAGED_SNAPSHOT_CHILD_NAME: &str = "snapshot";
const CHANGED_OBJECTS_HELPER: &str = "__changed-objects-send";
const GIT_FSMONITOR_AWACS: &str = "git-fsmonitor-awacs";
const SEND_HELPER_UNSUPPORTED_EXIT_CODE: i32 = 2;
const EOPNOTSUPP: i32 = 95;

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
        /// Root-specific manager SQLite database.
        #[arg(long, value_name = "PATH")]
        manager_db: Option<PathBuf>,
        /// Also inspect this managed snapshot directory for unrecorded cuts.
        #[arg(long, value_name = "PATH")]
        managed_dir: Option<PathBuf>,
    },
    /// Run the privileged filesystem-operation broker.
    BrokerServe {
        socket: PathBuf,
        manager_uid: u32,
        manager_gid: u32,
        #[arg(value_name = "BTRFS_PROBE_ROOT")]
        probe_root: PathBuf,
    },
    /// Diagnostic: emit the legacy changed-object stream.
    #[command(name = "__changed-objects-send")]
    ChangedObjectsSend {
        snapshot: PathBuf,
        parent_root_id: u64,
    },
    /// Diagnostic: print Btrfs filesystem and subvolume identity.
    #[command(name = "__btrfs-inspect")]
    BtrfsInspect { subvolume: PathBuf },
    /// Diagnostic: request a changed-object stream through the broker.
    #[command(name = "__broker-changed-objects")]
    BrokerChangedObjects {
        parent_snapshot: PathBuf,
        target_snapshot: PathBuf,
        output: PathBuf,
    },
    /// Acceptance helper: prove nested-subvolume rejection.
    #[command(name = "__nested-boundary-smoke")]
    NestedBoundarySmoke {
        source: PathBuf,
        managed_dir: PathBuf,
        spool_dir: PathBuf,
        manager_db: PathBuf,
    },
    /// Acceptance helper: prove root-path and mount-topology ABA detection.
    #[command(name = "__namespace-view-smoke")]
    NamespaceViewSmoke { source: PathBuf },
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
        /// Fail instead of falling back to ordinary Git when snapshotting is ineligible.
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
        /// Validate snapshot eligibility and inherited state without creating anything.
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

#[derive(Debug)]
enum SendHelperError {
    Unsupported(String),
    Other(String),
}

impl SendHelperError {
    fn message(&self) -> &str {
        match self {
            Self::Unsupported(message) | Self::Other(message) => message,
        }
    }
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
        CliCommand::Snap { source } => finish_timed(|| run_benchmark(&source, true)),
        CliCommand::Compare { source } => finish_timed(|| run_benchmark(&source, false)),
        CliCommand::Dump {
            manager_db,
            managed_dir,
        } => finish(run_dump(manager_db.as_deref(), managed_dir.as_deref())),
        CliCommand::BrokerServe {
            socket,
            manager_uid,
            manager_gid,
            probe_root,
        } => finish(run_broker_server(
            socket,
            manager_uid,
            manager_gid,
            probe_root,
        )),
        CliCommand::ChangedObjectsSend {
            snapshot,
            parent_root_id,
        } => finish_send_helper(run_changed_objects_send_helper(&snapshot, parent_root_id)),
        CliCommand::BtrfsInspect { subvolume } => finish(run_btrfs_inspect_helper(&subvolume)),
        CliCommand::BrokerChangedObjects {
            parent_snapshot,
            target_snapshot,
            output,
        } => finish(run_broker_changed_objects_helper(
            &parent_snapshot,
            &target_snapshot,
            &output,
        )),
        CliCommand::NestedBoundarySmoke {
            source,
            managed_dir,
            spool_dir,
            manager_db,
        } => finish(run_nested_boundary_smoke_helper(
            source,
            managed_dir,
            spool_dir,
            manager_db,
        )),
        CliCommand::NamespaceViewSmoke { source } => {
            finish(run_namespace_view_smoke_helper(&source))
        }
    }
}

fn run_init(root: &Path, compress: Option<bool>, keep: bool) -> Result<(), String> {
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
/// The handoff record and compact journal are copied by one Btrfs snapshot.
/// Holding the same lock JJ uses for journal transitions prevents a concurrent
/// JJ command from removing or replacing the record between validation and
/// that snapshot. The lock is released immediately after the filesystem clone;
/// later child initialization uses the recorded immutable baseline revision.
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
            "source JJ snapshot mode is not committed; finish or rebuild its AWACS baseline before creating a Git worktree"
                .to_owned(),
        );
    }
    let path = source.join(JJ_AWACS_ADOPTION_SEED_MARKER);
    let contents = fs::read_to_string(&path).map_err(|error| {
        format!(
            "source JJ workspace has no transferable AWACS adoption seed at {}: {error}; run jj status to publish a clean baseline",
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
                "source JJ adoption seed predates owner IDs and {} is not a compact working-copy journal",
                path.display()
            )
        })?;
    let owner = protobuf_length_delimited_field(proto, 18)?.ok_or_else(|| {
        "source JJ adoption seed predates owner IDs and its working-copy journal has no baseline owner"
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

fn inherited_git_fsmonitor_baseline(
    source_index: &Path,
    hash_len: usize,
) -> Result<SnapshotBaseline, String> {
    let bytes = fs::read(source_index)
        .map_err(|error| format!("read source Git index {}: {error}", source_index.display()))?;
    let fsmonitor = git_index_extension_data_range(&bytes, hash_len, b"FSMN")?
        .ok_or_else(|| "source Git index has no fsmonitor cache extension".to_owned())?;
    if fsmonitor.len() < 5 || read_be_u32(&bytes[fsmonitor.start..fsmonitor.start + 4])? != 2 {
        return Err("source Git fsmonitor cache is not protocol v2".to_owned());
    }
    let token_start = fsmonitor.start + 4;
    let token_end = bytes[token_start..fsmonitor.end]
        .iter()
        .position(|byte| *byte == 0)
        .map(|offset| token_start + offset)
        .ok_or_else(|| "source Git fsmonitor token is not NUL terminated".to_owned())?;
    decode_git_fsmonitor_token(&bytes[token_start..token_end])
        .ok_or_else(|| "source Git fsmonitor token is not an AWACS token".to_owned())
}

fn copy_inherited_git_index(source_index: &Path, destination_index: &Path) -> Result<(), String> {
    fs::copy(source_index, destination_index).map_err(|error| {
        format!(
            "write inherited Git index {}: {error}",
            destination_index.display()
        )
    })?;
    Ok(())
}

fn git_index_extension_data_range(
    bytes: &[u8],
    hash_len: usize,
    wanted: &[u8; 4],
) -> Result<Option<Range<usize>>, String> {
    if bytes.len() < 12 + hash_len || &bytes[..4] != b"DIRC" {
        return Err("Git index header is missing or malformed".to_owned());
    }
    let version = read_be_u32(&bytes[4..8])?;
    if !(2..=4).contains(&version) {
        return Err(format!("unsupported Git index version {version}"));
    }
    let entries = read_be_u32(&bytes[8..12])? as usize;
    let mut offset = 12usize;
    for _ in 0..entries {
        let entry_start = offset;
        offset = offset
            .checked_add(40 + hash_len)
            .filter(|offset| *offset + 2 <= bytes.len())
            .ok_or_else(|| "truncated Git index entry".to_owned())?;
        let flags = u16::from_be_bytes(bytes[offset..offset + 2].try_into().unwrap());
        offset += 2;
        if flags & 0x4000 != 0 {
            offset = offset
                .checked_add(2)
                .filter(|offset| *offset <= bytes.len())
                .ok_or_else(|| "truncated extended Git index flags".to_owned())?;
        }
        if version == 4 {
            offset = skip_git_index_leb128(bytes, offset)?;
            offset = skip_git_index_nul(bytes, offset)?;
        } else {
            let path_len = (flags & 0x0fff) as usize;
            offset = if path_len == 0x0fff {
                skip_git_index_nul(bytes, offset)?
            } else {
                offset
                    .checked_add(path_len)
                    .filter(|offset| *offset <= bytes.len())
                    .ok_or_else(|| "truncated Git index path".to_owned())?
            };
            let padded_len = (offset - entry_start + 8) & !7;
            offset = entry_start
                .checked_add(padded_len)
                .filter(|offset| *offset <= bytes.len())
                .ok_or_else(|| "truncated Git index padding".to_owned())?;
        }
    }
    let extension_end = bytes
        .len()
        .checked_sub(hash_len)
        .ok_or_else(|| "Git index has no checksum trailer".to_owned())?;
    if offset > extension_end {
        return Err("Git index entries overlap checksum trailer".to_owned());
    }
    while offset < extension_end {
        if extension_end - offset < 8 {
            return Err("truncated Git index extension header".to_owned());
        }
        let signature: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
        let size = read_be_u32(&bytes[offset + 4..offset + 8])? as usize;
        offset += 8;
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= extension_end)
            .ok_or_else(|| "truncated Git index extension".to_owned())?;
        if &signature == wanted {
            return Ok(Some(offset..end));
        }
        offset = end;
    }
    Ok(None)
}

fn read_be_u32(bytes: &[u8]) -> Result<u32, String> {
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

fn skip_git_index_nul(bytes: &[u8], offset: usize) -> Result<usize, String> {
    bytes[offset..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|index| offset + index + 1)
        .ok_or_else(|| "unterminated Git index path".to_owned())
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SourceGitChanges {
    tracked_paths: Vec<Vec<u8>>,
    untracked_paths: Vec<Vec<u8>>,
}

impl SourceGitChanges {
    fn is_empty(&self) -> bool {
        self.tracked_paths.is_empty() && self.untracked_paths.is_empty()
    }

    fn checkout_paths(&self, target_paths: impl IntoIterator<Item = Vec<u8>>) -> Vec<Vec<u8>> {
        let mut seen = HashSet::new();
        self.tracked_paths
            .iter()
            .cloned()
            .chain(target_paths)
            .filter(|path| seen.insert(path.clone()))
            .collect()
    }
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

fn target_changed_paths(
    git: &Path,
    source: &Path,
    source_target: &str,
    target: &str,
) -> Result<Vec<Vec<u8>>, String> {
    let source_target = source_target.trim_end();
    let target = target.trim_end();
    if source_target == target {
        return Ok(Vec::new());
    }
    let output = git_capture(
        git,
        source,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-z",
            source_target,
            target,
        ],
    )?;
    Ok(output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
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
            fs::remove_dir_all(&path).map_err(|error| {
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

fn restore_checkout_paths(
    git: &Path,
    destination: &Path,
    target: &str,
    paths: &[Vec<u8>],
) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let pathspec_path = git_path(git, destination, "awacs-worktree-restore-paths")?;
    let result = (|| {
        let mut pathspec = File::create(&pathspec_path).map_err(|error| {
            format!(
                "create snapshot worktree restore pathspec {}: {error}",
                pathspec_path.display()
            )
        })?;
        for path in paths {
            validated_relative_git_path(path)?;
            pathspec
                .write_all(path)
                .and_then(|()| pathspec.write_all(&[0]))
                .map_err(|error| {
                    format!(
                        "write snapshot worktree restore pathspec {}: {error}",
                        pathspec_path.display()
                    )
                })?;
        }
        let mut source_argument = OsString::from("--source=");
        source_argument.push(target.trim_end());
        let mut pathspec_argument = OsString::from("--pathspec-from-file=");
        pathspec_argument.push(pathspec_path.as_os_str());
        let arguments = vec![
            OsString::from("-c"),
            OsString::from("core.fsmonitor=false"),
            OsString::from("restore"),
            source_argument,
            OsString::from("--staged"),
            OsString::from("--worktree"),
            OsString::from("--no-overlay"),
            pathspec_argument,
            OsString::from("--pathspec-file-nul"),
        ];
        git_status(git, destination, &arguments, &[])
    })();
    match result {
        Err(error) => {
            let _ = fs::remove_file(&pathspec_path);
            Err(error)
        }
        Ok(()) => match fs::remove_file(&pathspec_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "remove snapshot worktree restore pathspec {}: {error}",
                pathspec_path.display()
            )),
        },
    }
}

/// Finishes a snapshot-backed checkout without ever traversing or rewriting
/// the whole tree.
///
/// The Btrfs snapshot already copied the source worktree and its stat-warm
/// index. A dirty source is still the normal case: remove only the exact
/// copied untracked paths, then restore only tracked source changes plus the
/// source-HEAD -> requested-target tree delta.
fn materialize_snapshot_checkout(
    git: &Path,
    source: &Path,
    destination: &Path,
    source_index: &Path,
    destination_index: &Path,
    source_target: &str,
    target: &str,
    source_changes: &SourceGitChanges,
) -> Result<(), String> {
    copy_inherited_git_index(source_index, destination_index)?;
    // The copied source FSMN token names the source watch's Git lane.
    // Preserve copied stat and untracked caches, but force the first child
    // status to bootstrap from its sequence-zero baseline instead of reusing
    // another worktree's cursor.
    git_status(
        git,
        destination,
        &[
            OsString::from("-c"),
            OsString::from("core.fsmonitor=false"),
            OsString::from("update-index"),
            OsString::from("--no-fsmonitor"),
        ],
        &[("GIT_INDEX_FILE", destination_index.as_os_str())],
    )?;
    let checkout_paths =
        source_changes.checkout_paths(target_changed_paths(git, source, source_target, target)?);
    // Remove only exact copied untracked paths before restoring the requested
    // tree. Ignored build output remains available for reuse.
    remove_copied_untracked_paths(destination, &source_changes.untracked_paths)?;
    restore_checkout_paths(git, destination, target, &checkout_paths)
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
    let stage = git_capture(options.git, options.source, &["ls-files", "--stage", "-z"])?;
    if stage
        .split(|byte| *byte == 0)
        .any(|entry| entry.starts_with(b"160000 "))
    {
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
    let metadata =
        fs::symlink_metadata(&dotgit).map_err(|error| format!("inspect copied .git: {error}"))?;
    if metadata.is_dir() {
        fs::remove_dir_all(&dotgit).map_err(|error| format!("remove copied .git: {error}"))
    } else {
        fs::remove_file(&dotgit).map_err(|error| format!("remove copied .git: {error}"))
    }
}

/// Saves the compact JJ state needed by delayed adoption outside the child
/// subvolume, then removes copied JJ metadata before AWACS records the child
/// baseline.
///
/// A Btrfs snapshot copies the source's ignored .jj directory, including the
/// very large shared repo store. Leaving that directory in the child baseline
/// and deleting it during jj workspace adopt turns adoption into a large
/// subtree-deletion delta. jj workspace add removes copied metadata before its
/// new working copy is initialized; Git-mediated creation must keep the same
/// ordering while retaining the compact committed journal that delayed
/// adoption needs to reconstruct the semantic tree.
fn stage_pending_jj_working_copy_state(
    destination: &Path,
    pending_state_path: &Path,
) -> Result<(), String> {
    let copied_state_path = destination.join(".jj/working_copy");
    if !copied_state_path.is_dir() {
        return Err(format!(
            "copied JJ working-copy state is absent at {}",
            copied_state_path.display()
        ));
    }
    if pending_state_path.exists() {
        fs::remove_dir_all(pending_state_path).map_err(|error| {
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
        let source = copied_state_path.join(name);
        if !source.is_file() {
            continue;
        }
        fs::copy(&source, pending_state_path.join(name)).map_err(|error| {
            format!(
                "copy pending JJ working-copy state {}: {error}",
                source.display()
            )
        })?;
        copied_journal |= matches!(name, "checkout" | "working_copy_state");
    }
    if !copied_journal {
        return Err(format!(
            "copied JJ working-copy state at {} has no committed journal",
            copied_state_path.display()
        ));
    }
    fs::remove_dir_all(destination.join(".jj"))
        .map_err(|error| format!("remove copied JJ metadata: {error}"))
}

fn create_btrfs_snapshot_with_cli(source: &Path, destination: &Path) -> Result<(), String> {
    let status = Command::new("btrfs")
        .arg("subvolume")
        .arg("snapshot")
        .arg(source)
        .arg(destination)
        .status()
        .map_err(|error| format!("run btrfs subvolume snapshot: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("btrfs subvolume snapshot exited with {status}"))
    }
}

/// Deletes one verified Btrfs subvolume through the kernel API.
///
/// The btrfs CLI prints its own warnings before returning status, which makes
/// Git worktree removal noisy and leaves AWACS unable to explain which owned
/// path failed. Open the target first so a plain directory or pathname
/// replacement is rejected, then issue DESTROY_V2 relative to the already-open
/// parent directory.
fn destroy_subvolume_path(path: &Path) -> Result<(), String> {
    let _target = OpenedSubvolume::open(path).map_err(|error| {
        format!(
            "verify Btrfs subvolume {} for deletion: {error}",
            path.display()
        )
    })?;
    let parent_path = path
        .parent()
        .ok_or_else(|| format!("Btrfs subvolume {} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("Btrfs subvolume {} has no basename", path.display()))?;
    let parent = File::open(parent_path).map_err(|error| {
        format!(
            "open Btrfs subvolume parent {} for deletion: {error}",
            parent_path.display()
        )
    })?;
    destroy_snapshot(parent.as_fd(), name.as_bytes())
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
                    fs::remove_dir_all(&path)
                        .map_err(|error| format!("remove managed cut wrapper: {error}"))?;
                }
            }
        }
    }
    fs::remove_dir_all(&state_dir)
        .map_err(|error| format!("remove AWACS state {}: {error}", state_dir.display()))
}

fn rollback_git_worktree(options: &GitWorktreeAdd<'_>) {
    let _ = remove_awacs_state(options.destination);
    let _ = destroy_subvolume_path(options.destination);
    if options.destination.exists() {
        let _ = fs::remove_dir_all(options.destination);
    }
    let mut arguments = vec![
        OsString::from("worktree"),
        OsString::from("remove"),
        OsString::from("--force"),
        OsString::from("--force"),
    ];
    arguments.push(options.destination.as_os_str().to_owned());
    let _ = git_status(options.git, options.source, &arguments, &[]);
}

fn run_git_worktree_add(options: &GitWorktreeAdd<'_>) -> Result<(), String> {
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
    let mut jj_working_copy_lock = lock_jj_working_copy_for_snapshot(&source)?;
    if !options.no_check && snapshot_eligibility(&options)? {
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
    let pending_jj_source = read_pending_jj_source_seed(&source)?;
    if let Some(seed) = pending_jj_source {
        let source_subvolume = OpenedSubvolume::open(&source)
            .map_err(|error| format!("inspect source JJ subvolume: {error}"))?;
        if source_subvolume.filesystem.fs_uuid != seed.baseline_filesystem_uuid {
            return Err(
                "source JJ AWACS adoption seed belongs to a different filesystem".to_owned(),
            );
        }
        require_retained_descendant_baseline(
            &source,
            seed.baseline_snapshot_uuid,
            seed.baseline_owner_id,
        )
        .map_err(|error| format!("validate retained JJ adoption baseline: {error}"))?;
    }
    // Git's new-branch path calls us once before creating the branch and once
    // afterwards with --no-check. Static Btrfs/JJ eligibility belongs in the
    // first call, but collecting mutable dirty paths there only duplicates an
    // expensive status and cannot be transferred safely across the race.
    if options.check_only {
        return Ok(());
    }

    // Capture the exact copied paths immediately before the Btrfs snapshot.
    // The later checkout restores only this set plus the source-HEAD -> target
    // tree delta; it must not turn a dirty source into a full-tree reset.
    let source_changes = source_git_changes(&options)?;
    let source_is_clean = source_changes.is_empty();
    let source_index = git_path(options.git, &source, "index")?;
    let target = String::from_utf8(git_capture(
        options.git,
        &source,
        &["rev-parse", options.reference],
    )?)
    .map_err(|error| format!("decode requested worktree target: {error}"))?;
    let source_target =
        String::from_utf8(git_capture(options.git, &source, &["rev-parse", "HEAD"])?)
            .map_err(|error| format!("decode source HEAD: {error}"))?;
    let inherited_baseline = if pending_jj_source.is_none()
        && source_is_clean
        && target.trim_end() == source_target.trim_end()
    {
        let hash_len = git_object_hash_len(options.git, &source)?;
        Some(inherited_git_fsmonitor_baseline(&source_index, hash_len)?)
    } else {
        None
    };
    git_status(
        options.git,
        options.source,
        &git_worktree_args(&options, true),
        &[],
    )?;
    let result = (|| {
        let gitfile = fs::read(destination.join(".git"))
            .map_err(|error| format!("read registered worktree .git: {error}"))?;
        fs::remove_dir_all(&destination)
            .map_err(|error| format!("remove registered empty worktree: {error}"))?;
        create_btrfs_snapshot_with_cli(&source, &destination)?;
        drop(jj_working_copy_lock.take());
        remove_copied_dotgit(&destination)?;
        fs::write(destination.join(".git"), gitfile)
            .map_err(|error| format!("restore registered worktree .git: {error}"))?;
        let destination_index = git_path(options.git, &destination, "index")?;
        let pending_jj_state_path = if pending_jj_source.is_some() {
            let path = git_path(
                options.git,
                &destination,
                AWACS_JJ_PENDING_WORKING_COPY_STATE,
            )?;
            stage_pending_jj_working_copy_state(&destination, &path)?;
            Some(path)
        } else {
            None
        };
        let source_target =
            String::from_utf8(git_capture(options.git, &source, &["rev-parse", "HEAD"])?)
                .map_err(|error| format!("decode source HEAD: {error}"))?;
        // A copied `.jj` directory means this descendant may be adopted as a
        // JJ workspace later. Give JJ its own durable consumer pin now; Git
        // receives a separate child-watch-owned pin below. The two lanes can
        // then advance independently across one shared replay timeline.
        let pending_jj_consumer = pending_jj_source.map(|_| *Uuid::new_v4().as_bytes());
        let inherited_snapshot_uuid = pending_jj_source
            .map(|seed| seed.baseline_snapshot_uuid)
            .or_else(|| {
                inherited_baseline
                    .as_ref()
                    .map(|baseline| baseline.identity.subvolume_uuid)
            });
        // A pending JJ consumer needs immutable snapshot A before Git writes
        // target B, so delayed adoption can reconcile the authenticated A -> B
        // transition. A dirty plain-Git source has no equivalent semantic
        // baseline, so publish its child baseline after exact-path
        // materialization rather than inheriting a stale parent path map.
        let initialized = if pending_jj_consumer.is_some() || source_is_clean {
            Some(
                initialize_descendant_root_with_watch_consumer(
                    &destination,
                    &source,
                    inherited_snapshot_uuid,
                    pending_jj_consumer,
                    |_| {},
                )
                .map_err(|error| format!("initialize AWACS snapshot descendant: {error}"))?,
            )
        } else {
            None
        };

        // One exact-path materializer handles clean and dirty sources alike.
        // There is deliberately no whole-tree checkout or clean fallback.
        materialize_snapshot_checkout(
            options.git,
            &source,
            &destination,
            &source_index,
            &destination_index,
            &source_target,
            &target,
            &source_changes,
        )?;
        let initialized = match initialized {
            Some(initialized) => initialized,
            None => initialize_root_with_watch_consumer(&destination, |_| {})
                .map_err(|error| format!("initialize materialized AWACS worktree: {error}"))?,
        };
        let zero = "0".repeat(target.trim_end().len());
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
        )?;
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
        Ok(())
    })();
    if let Err(error) = result {
        rollback_git_worktree(&options);
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
    let mut client = match DirectScanClient::for_root(&root) {
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
    let mut previous_baseline = match decode_git_fsmonitor_token(prior_token.as_bytes()) {
        Some(baseline) => baseline,
        None => {
            tracing::debug!(
                root = %root.display(),
                "AWACS Git fsmonitor token is absent; bootstrapping from initial snapshot"
            );
            client
                .initial_baseline(&root)
                .map_err(|error| format!("load initial AWACS fsmonitor baseline: {error}"))?
        }
    };
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
    let baseline_owner_id: [u8; 16] = previous_baseline
        .retention_token
        .as_slice()
        .try_into()
        .map_err(|_| "AWACS Git fsmonitor token has an invalid lane owner".to_owned())?;
    let mut lease = client
        .begin_scan(&BeginScanRequest {
            live_root: root,
            // The token carries this worktree's stable Git lane owner. Git
            // and JJ therefore retain separate endpoints while sharing only
            // the underlying exact replay timeline.
            baseline_owner_id,
            previous_baseline: Some(previous_baseline),
            allow_full_invalidation: false,
        })
        .map_err(|error| format!("begin AWACS fsmonitor scan: {error}"))?;
    client
        .validate_scan_root(&lease)
        .map_err(|error| format!("validate AWACS fsmonitor snapshot: {error}"))?;
    let token = encode_git_fsmonitor_token(&lease.next_baseline)?;
    let paths = match &lease.invalidation {
        Invalidation::ExactPaths(paths) => paths.clone(),
        Invalidation::Full => {
            let _ = lease.finish(ScanOutcome::Aborted);
            return Err(
                "AWACS refused to return a full invalidation to the Git fsmonitor hook".to_owned(),
            );
        }
        // Git's hook protocol has no first-class prefix result. Until the
        // Git adapter proves directory-prefix semantics, fail the hook rather
        // than widening an exact AWACS answer to a whole-tree invalidation.
        Invalidation::Prefixes(_) => {
            let _ = lease.finish(ScanOutcome::Aborted);
            return Err(
                "AWACS cannot represent prefix invalidations in the Git fsmonitor hook".to_owned(),
            );
        }
    };
    lease
        .promote()
        .map_err(|error| format!("promote AWACS fsmonitor cursor: {error}"))?;
    lease
        .finish(ScanOutcome::Committed)
        .map_err(|error| format!("finish AWACS fsmonitor cursor: {error}"))?;
    write_git_fsmonitor_response(&token, paths.iter().map(Vec::as_slice))
}

fn is_uninitialized_awacs_root(error: &btrfs_awacs::scan::ScanError) -> bool {
    error.kind() == btrfs_awacs::scan::ScanErrorKind::Unavailable
        && error.message().contains("not initialized")
}

fn write_git_fsmonitor_response<'a>(
    token: &str,
    paths: impl IntoIterator<Item = &'a [u8]>,
) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(token.as_bytes())
        .and_then(|()| stdout.write_all(&[0]))
        .map_err(|error| format!("write Git fsmonitor token: {error}"))?;
    for path in paths {
        stdout
            .write_all(&path)
            .and_then(|()| stdout.write_all(&[0]))
            .map_err(|error| format!("write Git fsmonitor path: {error}"))?;
    }
    Ok(())
}

fn encode_git_fsmonitor_token(baseline: &SnapshotBaseline) -> Result<String, String> {
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

fn decode_git_fsmonitor_token(token: &[u8]) -> Option<SnapshotBaseline> {
    let token = std::str::from_utf8(token).ok()?;
    let (retention_token, filesystem_uuid, subvolume_uuid, continuity_token) =
        if token.starts_with("awacs-git-v2:") {
            let mut fields = token.splitn(5, ':');
            if fields.next()? != "awacs-git-v2" {
                return None;
            }
            let owner_id = *Uuid::parse_str(fields.next()?).ok()?.as_bytes();
            let filesystem_uuid = *Uuid::parse_str(fields.next()?).ok()?.as_bytes();
            let subvolume_uuid = *Uuid::parse_str(fields.next()?).ok()?.as_bytes();
            let continuity_token = fields.next()?.as_bytes().to_vec();
            (
                owner_id.to_vec(),
                filesystem_uuid,
                subvolume_uuid,
                continuity_token,
            )
        } else if token.starts_with("awacs-git-v1:") {
            let mut fields = token.splitn(4, ':');
            if fields.next()? != "awacs-git-v1" {
                return None;
            }
            let filesystem_uuid = *Uuid::parse_str(fields.next()?).ok()?.as_bytes();
            let subvolume_uuid = *Uuid::parse_str(fields.next()?).ok()?.as_bytes();
            let continuity_token = fields.next()?.as_bytes().to_vec();
            (
                Vec::new(),
                filesystem_uuid,
                subvolume_uuid,
                continuity_token,
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

fn default_manager_db_path() -> Result<PathBuf, String> {
    Err("awacs dump requires --manager-db because manager state is per root".to_owned())
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

fn run_dump(manager_db_override: Option<&Path>, managed_dir: Option<&Path>) -> Result<(), String> {
    let manager_db = manager_db_override
        .map(Path::to_owned)
        .map_or_else(default_manager_db_path, Ok)?;
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
            "SELECT store_uuid, clock_format_version, last_boot_id, created_ns FROM service_metadata WHERE singleton = 1",
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
    for table in [
        "filesystems",
        "snapshots",
        "revisions",
        "revision_checkpoints",
        "comparisons",
        "change_events",
        "watches",
        "watch_grants",
        "operations",
        "watch_cuts",
        "fsmonitor_boundaries",
        "query_leases",
        "retention_leases",
        "snapshot_pins",
        "snapshot_delete_operations",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(|error| format!("count {table}: {error}"))?;
        println!("  {table}: {count}");
    }

    println!("\nWatches:");
    let mut watch_statement = connection
        .prepare(
            "SELECT id, live_path, state, fsmonitor_state, indexed_seq, last_cut_seq, replay_floor_seq FROM watches ORDER BY live_path",
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
            "  {} state={state} fsmonitor={monitor} indexed={indexed:?} last-cut={last_cut:?} replay-floor={floor:?} path={}",
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
        recorded_paths.insert(snapshot.path.clone());
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
            "SELECT snapshot_id, owner_kind, owner_id, reason FROM snapshot_pins ORDER BY snapshot_id, owner_kind, owner_id, reason",
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
        .filter_map(|snapshot| snapshot.path.parent().map(Path::to_owned))
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

fn finish_send_helper(result: Result<(), SendHelperError>) {
    if let Err(error) = result {
        eprintln!("error: {}", error.message());
        std::process::exit(match error {
            SendHelperError::Unsupported(_) => SEND_HELPER_UNSUPPORTED_EXIT_CODE,
            SendHelperError::Other(_) => 1,
        });
    }
}

fn run_btrfs_inspect_helper(path: &Path) -> Result<(), String> {
    let opened = OpenedSubvolume::open(path).map_err(|error| error.to_string())?;
    opened.revalidate().map_err(|error| error.to_string())?;
    println!(
        "fs_uuid={} subvol_uuid={} root_id={} readonly={} ctransid={} otransid={}",
        hex_bytes(&opened.filesystem.fs_uuid),
        hex_bytes(&opened.subvolume.uuid),
        opened.subvolume.root_id,
        opened.subvolume.readonly(),
        opened.subvolume.ctransid,
        opened.subvolume.otransid,
    );
    Ok(())
}

fn run_broker_changed_objects_helper(
    parent_path: &Path,
    target_path: &Path,
    output_path: &Path,
) -> Result<(), String> {
    let parent = OpenedSubvolume::open(parent_path).map_err(|error| error.to_string())?;
    let target = OpenedSubvolume::open(target_path).map_err(|error| error.to_string())?;
    let output = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output_path)
        .map_err(|error| format!("create {}: {error}", output_path.display()))?;
    let request = ChangedObjectsExecution {
        parent: ExpectedSubvolume::from_observed(&parent.filesystem, &parent.subvolume),
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
    let result = execute_changed_objects(&request, parent.as_fd(), target.as_fd(), output.as_fd())
        .map_err(|error| error.to_string())?;
    drop(output);

    let bytes = fs::read(output_path)
        .map_err(|error| format!("read {}: {error}", output_path.display()))?;
    if bytes.len() as u64 != result.output_bytes {
        return Err(format!(
            "broker result reported {} bytes but output contains {}",
            result.output_bytes,
            bytes.len()
        ));
    }
    let (manifest, boundary_adds, boundary_deletes) =
        if bytes.get(..CHANGED_OBJECTS_V2_MAGIC.len()) == Some(CHANGED_OBJECTS_V2_MAGIC) {
            let parsed = parse_changed_objects_v2(&bytes).map_err(|error| error.to_string())?;
            (
                parsed.manifest,
                parsed.boundary_adds.len(),
                parsed.boundary_deletes.len(),
            )
        } else {
            (
                parse_changed_objects(&bytes).map_err(|error| error.to_string())?,
                0,
                0,
            )
        };
    println!(
        "bytes={} sha256={} objects={} refs=+{}/-{} raw_refs=+{}/-{} boundaries=+{}/-{}",
        result.output_bytes,
        hex_bytes(&result.manifest_hash),
        manifest.objects.len(),
        manifest.ref_adds.len(),
        manifest.ref_deletes.len(),
        manifest.raw_ref_adds,
        manifest.raw_ref_deletes,
        boundary_adds,
        boundary_deletes,
    );
    Ok(())
}

fn run_namespace_view_smoke_helper(source_path: &Path) -> Result<(), String> {
    let source_path =
        fs::canonicalize(source_path).map_err(|error| format!("canonicalize source: {error}"))?;
    let parent = source_path
        .parent()
        .ok_or_else(|| "source has no parent directory".to_owned())?;
    let moved = parent.join(format!(".btrfs-awacs-namespace-aba-{}", std::process::id()));
    if moved.exists() {
        return Err(format!(
            "temporary ABA path already exists: {}",
            moved.display()
        ));
    }

    let root_monitor = NamespaceMonitor::arm(&source_path)
        .map_err(|error| format!("arm root monitor: {error}"))?;
    fs::rename(&source_path, &moved)
        .map_err(|error| format!("rename watched root away: {error}"))?;
    if let Err(error) = fs::rename(&moved, &source_path) {
        let _ = fs::rename(&moved, &source_path);
        return Err(format!("restore watched root: {error}"));
    }
    if root_monitor.check_continuity().is_ok() {
        return Err("root rename/restore ABA was not detected".to_owned());
    }

    let ancestor = source_path
        .parent()
        .ok_or_else(|| "source has no ancestor to exercise".to_owned())?;
    let ancestor_parent = ancestor
        .parent()
        .ok_or_else(|| "source ancestor has no parent to exercise".to_owned())?;
    let moved_ancestor =
        ancestor_parent.join(format!(".btrfs-awacs-ancestor-aba-{}", std::process::id()));
    if moved_ancestor.exists() {
        return Err(format!(
            "temporary ancestor ABA path already exists: {}",
            moved_ancestor.display()
        ));
    }
    let ancestor_monitor = NamespaceMonitor::arm(&source_path)
        .map_err(|error| format!("re-arm ancestor monitor: {error}"))?;
    fs::rename(ancestor, &moved_ancestor)
        .map_err(|error| format!("rename watched ancestor away: {error}"))?;
    if let Err(error) = fs::rename(&moved_ancestor, ancestor) {
        let _ = fs::rename(&moved_ancestor, ancestor);
        return Err(format!("restore watched ancestor: {error}"));
    }
    if ancestor_monitor.check_continuity().is_ok() {
        return Err("ancestor rename/restore ABA was not detected".to_owned());
    }

    let mount_monitor = NamespaceMonitor::arm(&source_path)
        .map_err(|error| format!("re-arm mount monitor: {error}"))?;
    let mount_target = source_path.join(format!(".btrfs-awacs-mount-aba-{}", std::process::id()));
    fs::create_dir(&mount_target)
        .map_err(|error| format!("create temporary mount target: {error}"))?;
    let source_c = std::ffi::CString::new(source_path.as_os_str().as_bytes())
        .map_err(|_| "source path contains NUL".to_owned())?;
    let target_c = std::ffi::CString::new(mount_target.as_os_str().as_bytes())
        .map_err(|_| "mount target contains NUL".to_owned())?;
    // SAFETY: both C strings remain live for the syscall and MS_BIND ignores
    // filesystem type and data pointers.
    let mounted = unsafe {
        libc::mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if mounted != 0 {
        let error = io::Error::last_os_error();
        let _ = fs::remove_dir(&mount_target);
        return Err(format!("attach temporary bind mount: {error}"));
    }
    // SAFETY: target_c remains live and names the mount just created.
    let unmounted = unsafe { libc::umount2(target_c.as_ptr(), 0) };
    if unmounted != 0 {
        let error = io::Error::last_os_error();
        // SAFETY: best-effort cleanup of the mount created above.
        unsafe {
            libc::umount2(target_c.as_ptr(), libc::MNT_DETACH);
        }
        let _ = fs::remove_dir(&mount_target);
        return Err(format!("detach temporary bind mount: {error}"));
    }
    fs::remove_dir(&mount_target)
        .map_err(|error| format!("remove temporary mount target: {error}"))?;
    if mount_monitor.check_continuity().is_ok() {
        return Err("mount attach/detach ABA was not detected".to_owned());
    }
    println!("root_aba=detected ancestor_aba=detected mount_aba=detected");
    Ok(())
}

fn run_nested_boundary_smoke_helper(
    source_path: PathBuf,
    managed_path: PathBuf,
    spool_path: PathBuf,
    store_path: PathBuf,
) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&spool_path)
        .map_err(|error| format!("create {}: {error}", spool_path.display()))?;
    let now_ns = current_time_ns()?;
    let metadata = ServiceMetadata::generate([0x81; 16], now_ns)
        .map_err(|error| format!("generate nested-boundary metadata: {error}"))?;
    let store = Store::create(&store_path, &metadata)
        .map_err(|error| format!("create nested-boundary store: {error}"))?;
    let socket = env::var_os("BTRFS_AWACS_BROKER_SOCKET")
        .ok_or_else(|| "nested-boundary smoke requires the external broker".to_owned())?;
    let config =
        ServiceConfig::new(managed_path, spool_path, [0x81; 16]).with_broker_socket(socket.into());
    let mut service = Service::new_external(store, config).map_err(|error| error.to_string())?;
    let permissions =
        Permissions::new(PERMISSION_READ | PERMISSION_CUT).map_err(|error| error.to_string())?;
    let initialized = service
        .initialize(
            &source_path,
            &InitializeOptions {
                principal: Principal::Uid(0),
                permissions,
                requester_uid: 0,
                requester_gid: 0,
                now_ns,
            },
        )
        .map_err(|error| format!("initialize boundary-free source: {error}"))?;

    let child = source_path.join("nested-child");
    let output = Command::new("/usr/bin/btrfs")
        .arg("subvolume")
        .arg("create")
        .arg(&child)
        .output()
        .map_err(|error| format!("execute nested subvolume creation: {error}"))?;
    if !output.status.success() {
        return Err(command_failure(
            "btrfs subvolume create",
            output.status,
            &output.stderr,
        ));
    }
    let error = service
        .changes(&ChangesOptions {
            watch_id: initialized.watch_id,
            authorization_id: initialized.grant_id,
            requester_uid: 0,
            requester_gid: 0,
            now_ns: current_time_ns()?,
        })
        .expect_err("nested-subvolume cut unexpectedly published");
    if !error.to_string().contains("nested subvolume") {
        return Err(format!(
            "nested-subvolume cut failed for the wrong reason: {error}"
        ));
    }
    let indexed_sequence: i64 = service
        .store()
        .connection()
        .query_row(
            "SELECT indexed_seq FROM watches WHERE id = ?1",
            [initialized.watch_id.as_slice()],
            |row| row.get(0),
        )
        .map_err(|error| format!("load nested-boundary watch sequence: {error}"))?;
    if indexed_sequence != 0 {
        return Err(format!(
            "nested-subvolume failure advanced indexed sequence to {indexed_sequence}"
        ));
    }
    println!("nested_boundary_delta_rejected=true indexed_sequence=0");
    Ok(())
}

fn current_time_ns() -> Result<i64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read system clock: {error}"))?;
    i64::try_from(elapsed.as_nanos())
        .map_err(|_| "system time exceeds signed nanoseconds".to_owned())
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
    socket_path: PathBuf,
    manager_uid: u32,
    manager_gid: u32,
    probe_root: PathBuf,
) -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("broker-serve must run as root".to_owned());
    }
    verify_changed_objects_kernel_support(&probe_root)?;
    validate_root_broker_directory(
        socket_path
            .parent()
            .ok_or_else(|| "broker socket has no parent directory".to_owned())?,
        false,
    )?;
    let listener = SeqPacketListener::bind(&socket_path, 0o660)
        .map_err(|error| format!("bind broker listener: {error}"))?;
    let socket = std::ffi::CString::new(socket_path.as_os_str().as_bytes())
        .map_err(|_| "broker socket path contains NUL".to_owned())?;
    // SAFETY: socket is a live NUL-terminated path. uid -1 preserves root
    // ownership while granting only the configured manager group access.
    if unsafe { libc::chown(socket.as_ptr(), u32::MAX, manager_gid) } != 0 {
        return Err(format!(
            "set broker socket group: {}",
            io::Error::last_os_error()
        ));
    }
    let dispatcher = std::sync::Arc::new(
        BrokerDispatcher::new(manager_uid)
            .map_err(|error| format!("start broker snapshot trash worker: {error}"))?,
    );
    loop {
        let connection = listener
            .accept()
            .map_err(|error| format!("accept broker connection: {error}"))?;
        let dispatcher = std::sync::Arc::clone(&dispatcher);
        std::thread::Builder::new()
            .name("btrfs-awacs-broker-client".to_owned())
            .spawn(move || while dispatcher.serve_one(&connection).is_ok() {})
            .map_err(|error| format!("start broker client worker: {error}"))?;
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
            "running kernel does not support BTRFS_IOC_CHANGED_OBJECTS for {}; boot the patched Btrfs kernel before starting AWACS",
            probe_root.display()
        ));
    }
    Ok(())
}

fn validate_root_broker_directory(path: &Path, private: bool) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat broker directory {}: {error}", path.display()))?;
    let forbidden = if private { 0o077 } else { 0o002 };
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.permissions().mode() & forbidden != 0 {
        return Err(format!(
            "broker directory {} must be root-owned and {}",
            path.display(),
            if private {
                "mode 0700 or stricter"
            } else {
                "not world-writable"
            }
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
    let objects = changed_objects_from_manifest(parent, current)?;
    let objects_elapsed = objects_started.elapsed();
    if let Some(objects) = objects {
        println!(
            "changed-objects: objects={} created={} deleted={} refs=+{}/-{} \
             raw_refs=+{}/-{} elapsed={:.3}s",
            objects.objects,
            objects.created,
            objects.deleted,
            objects.net_ref_adds,
            objects.net_ref_deletes,
            objects.raw_ref_adds,
            objects.raw_ref_deletes,
            objects_elapsed.as_secs_f64()
        );
        print_timing_comparison("changed-objects", normal_elapsed, objects_elapsed);
    }

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

fn changed_objects_from_manifest(
    parent: &Path,
    current: &Path,
) -> Result<Option<ChangedObjectsSummary>, String> {
    let parent_root = root_id(parent)?;
    let executable =
        env::current_exe().map_err(|error| format!("locate current executable: {error}"))?;
    let output = Command::new("sudo")
        .arg("--non-interactive")
        .arg(executable)
        .arg(CHANGED_OBJECTS_HELPER)
        .arg(current)
        .arg(parent_root.to_string())
        .output()
        .map_err(|error| format!("run changed-objects send helper: {error}"))?;

    if !output.status.success() {
        if output.status.code() == Some(SEND_HELPER_UNSUPPORTED_EXIT_CODE) {
            print_helper_warning(&output.stderr);
            return Ok(None);
        }
        return Err(command_failure(
            "sudo --non-interactive btrfs-awacs __changed-objects-send",
            output.status,
            &output.stderr,
        ));
    }

    parse_changed_objects_manifest(&output.stdout).map(Some)
}

fn root_id(path: &Path) -> Result<u64, String> {
    let output = Command::new("sudo")
        .args(["--non-interactive", "btrfs", "inspect-internal", "rootid"])
        .arg(path)
        .output()
        .map_err(|error| format!("read Btrfs root ID for {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(command_failure(
            "sudo --non-interactive btrfs inspect-internal rootid",
            output.status,
            &output.stderr,
        ));
    }

    let root_id = std::str::from_utf8(&output.stdout)
        .map_err(|error| format!("Btrfs root ID is not UTF-8: {error}"))?
        .trim();
    root_id
        .parse()
        .map_err(|error| format!("invalid Btrfs root ID {root_id:?}: {error}"))
}

fn run_changed_objects_send_helper(
    current: &Path,
    parent_root: u64,
) -> Result<(), SendHelperError> {
    let snapshot = File::open(current)
        .map_err(|error| SendHelperError::Other(format!("open {}: {error}", current.display())))?;
    let stdout = io::stdout();
    let result = send_changed_objects(snapshot.as_fd(), parent_root, stdout.as_fd());
    if let Err(error) = result {
        let unsupported = error.raw_os_error() == Some(EOPNOTSUPP);
        let message = error.to_string();
        return Err(if unsupported {
            SendHelperError::Unsupported(message)
        } else {
            SendHelperError::Other(message)
        });
    }

    Ok(())
}

fn print_helper_warning(stderr: &[u8]) {
    let message = String::from_utf8_lossy(stderr);
    let message = message
        .trim()
        .strip_prefix("error: ")
        .unwrap_or(message.trim());
    eprintln!("warning: {message}");
}

fn parse_changed_objects_manifest(manifest: &[u8]) -> Result<ChangedObjectsSummary, String> {
    let manifest = parse_changed_objects(manifest).map_err(|error| error.to_string())?;
    Ok(ChangedObjectsSummary {
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
    })
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
    use super::{
        ChangedObjectsSummary, Cli, CliCommand, GitCommand, JJ_WORKING_COPY_STATE_MAGIC,
        canonical_path_for_creation, changed_paths, copy_inherited_git_index,
        decode_git_fsmonitor_token, encode_git_fsmonitor_token, inherited_git_fsmonitor_baseline,
        is_uninitialized_awacs_root, last_two_snapshots, materialize_snapshot_checkout,
        parse_changed_objects_manifest, parse_source_git_changes, read_pending_jj_source_seed,
        require_last_two_snapshots, stage_pending_jj_working_copy_state,
    };
    use btrfs_awacs::manifest::{
        CHANGE_DELETED as CHANGED_OBJECT_DELETED, CHANGE_INODE as CHANGED_OBJECT_INODE,
        CHANGE_REF as CHANGED_OBJECT_REF,
    };
    use clap::{CommandFactory, Parser};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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

        let legacy = "awacs-git-v1:01010101-0101-0101-0101-010101010101:02020202-0202-0202-0202-020202020202:c:btrfs-awacs:scan:1:legacy";
        let decoded_legacy = decode_git_fsmonitor_token(legacy.as_bytes()).unwrap();
        assert_eq!(decoded_legacy.identity, baseline.identity);
        assert_eq!(
            decoded_legacy.continuity_token,
            b"c:btrfs-awacs:scan:1:legacy"
        );
        assert!(decoded_legacy.retention_token.is_empty());
    }

    #[test]
    fn copies_inherited_git_index_without_rewriting_fsmonitor() {
        let source_baseline = btrfs_awacs::scan::SnapshotBaseline {
            identity: btrfs_awacs::scan::SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
            continuity_token: b"c:btrfs-awacs:scan:1:inherited".to_vec(),
            retention_token: vec![3; 16],
        };
        let source_token = encode_git_fsmonitor_token(&source_baseline).unwrap();
        let mut fsmonitor = 2u32.to_be_bytes().to_vec();
        fsmonitor.extend_from_slice(source_token.as_bytes());
        fsmonitor.push(0);
        fsmonitor.extend_from_slice(&0u32.to_be_bytes());

        let mut index = b"DIRC".to_vec();
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&0u32.to_be_bytes());
        index.extend_from_slice(b"FSMN");
        index.extend_from_slice(&(fsmonitor.len() as u32).to_be_bytes());
        index.extend_from_slice(&fsmonitor);
        index.extend_from_slice(b"UNTR");
        index.extend_from_slice(&3u32.to_be_bytes());
        index.extend_from_slice(b"old");
        index.extend_from_slice(&[0; 20]);

        let root = TestDir::new("inherited-git-index");
        let source_index = root.path.join("source-index");
        let destination_index = root.path.join("destination-index");
        fs::write(&source_index, &index).unwrap();
        let inherited = inherited_git_fsmonitor_baseline(&source_index, 20).unwrap();
        assert_eq!(inherited, source_baseline);
        copy_inherited_git_index(&source_index, &destination_index).unwrap();
        assert_eq!(
            fs::read(destination_index).unwrap(),
            index,
            "the destination index retains the source FSMN and UNTR bytes"
        );
    }

    #[test]
    fn parses_dirty_git_paths_for_exact_snapshot_checkout() {
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
        assert_eq!(
            changes.checkout_paths([b"modified".to_vec(), b"target-only".to_vec(),]),
            vec![
                b"modified".to_vec(),
                b"added".to_vec(),
                b"renamed".to_vec(),
                b"old-name".to_vec(),
                b"target-only".to_vec(),
            ]
        );
    }

    #[test]
    fn dirty_source_uses_exact_snapshot_fast_path() {
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
        fs::write(source.join("file"), b"base\n").unwrap();
        fs::write(source.join("old-only"), b"old\n").unwrap();
        run_git(&source, &["add", "file", "old-only"]);
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
        materialize_snapshot_checkout(
            Path::new("git"),
            &source,
            &destination,
            &source_index,
            &destination_index,
            &source_target,
            &target,
            &changes,
        )
        .unwrap();

        assert_eq!(fs::read(destination.join("file")).unwrap(), b"target\n");
        assert_eq!(
            fs::read(destination.join("target-only")).unwrap(),
            b"target only\n"
        );
        assert!(!destination.join("source-untracked").exists());
        assert!(
            run_git(&destination, &["status", "--porcelain"]).is_empty(),
            "destination must be clean after exact-path restore"
        );
        assert_eq!(fs::read(source.join("file")).unwrap(), b"unstaged\n");
        assert!(source.join("source-untracked").exists());
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
            "jj-awacs-adoption-v2:11111111-1111-1111-1111-111111111111:22222222-2222-2222-2222-222222222222:33333333-3333-3333-3333-333333333333\n",
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
            "jj-awacs-adoption-v1:11111111-1111-1111-1111-111111111111:22222222-2222-2222-2222-222222222222\n",
        )
        .unwrap();
        let seed = read_pending_jj_source_seed(&root.path)
            .unwrap()
            .expect("v1 marker recovers owner from compact journal");
        assert_eq!(seed.baseline_owner_id, [0x44; 16]);
    }

    #[test]
    fn stages_pending_jj_state_before_child_baseline() {
        let root = TestDir::new("pending-jj-state");
        let destination = root.path.join("destination");
        let copied_state = destination.join(".jj/working_copy");
        fs::create_dir_all(destination.join(".jj/repo")).unwrap();
        fs::create_dir_all(&copied_state).unwrap();
        fs::write(copied_state.join("checkout"), b"journal").unwrap();
        fs::write(copied_state.join("subvolume_mode"), b"snapshot-backed\n").unwrap();
        fs::write(copied_state.join("type"), b"local").unwrap();
        fs::write(copied_state.join("working_copy.lock"), b"ignored").unwrap();
        let pending_state = root.path.join("git-admin/pending");
        fs::create_dir_all(pending_state.parent().unwrap()).unwrap();

        stage_pending_jj_working_copy_state(&destination, &pending_state).unwrap();

        assert_eq!(
            fs::read(pending_state.join("checkout")).unwrap(),
            b"journal"
        );
        assert_eq!(
            fs::read(pending_state.join("subvolume_mode")).unwrap(),
            b"snapshot-backed\n"
        );
        assert_eq!(fs::read(pending_state.join("type")).unwrap(), b"local");
        assert!(!pending_state.join("working_copy.lock").exists());
        assert!(!destination.join(".jj").exists());
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

    use std::path::{Path, PathBuf};

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
                manager_db: None,
                managed_dir: None,
            }
        ));
    }

    #[test]
    fn rejects_old_interface_and_extra_arguments() {
        assert!(Cli::try_parse_from(["awacs", "changes", "."]).is_err());
        assert!(Cli::try_parse_from(["awacs", "compare", "--timing", "."]).is_err());
        assert!(Cli::try_parse_from(["awacs", "snap", ".", "extra"]).is_err());
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
            "snap",
            "compare",
            "dump",
            "broker-serve",
            "__changed-objects-send",
            "__btrfs-inspect",
            "__broker-changed-objects",
            "__nested-boundary-smoke",
            "__namespace-view-smoke",
        ] {
            assert!(help.contains(command), "help omitted {command:?}\n{help}");
        }
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
                    "compare requires at least two snapshots in volume/.btrfs-awacs (found {count})"
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

    #[test]
    fn rejects_changed_objects_with_inconsistent_ref_data() {
        let mut no_ref_mask = changed_objects_header();
        push_ref(&mut no_ref_mask, 2, 300, 256, b"name");
        push_object(&mut no_ref_mask, 300, 7, 7, CHANGED_OBJECT_INODE);
        assert!(
            parse_changed_objects_manifest(&no_ref_mask)
                .unwrap_err()
                .contains("no ref change")
        );

        let mut no_ref_records = changed_objects_header();
        push_object(
            &mut no_ref_records,
            300,
            7,
            7,
            CHANGED_OBJECT_INODE | CHANGED_OBJECT_REF,
        );
        assert!(
            parse_changed_objects_manifest(&no_ref_records)
                .unwrap_err()
                .contains("no ref records")
        );
    }

    fn changed_objects_header() -> Vec<u8> {
        let mut manifest = b"btrfs-changes\0\0\0".to_vec();
        manifest.extend_from_slice(&1_u32.to_le_bytes());
        manifest.extend_from_slice(&24_u32.to_le_bytes());
        manifest
    }

    fn push_object(manifest: &mut Vec<u8>, ino: u64, old: u64, new: u64, changes: u64) {
        manifest.extend_from_slice(&1_u16.to_le_bytes());
        manifest.extend_from_slice(&0_u16.to_le_bytes());
        manifest.extend_from_slice(&40_u32.to_le_bytes());
        manifest.extend_from_slice(&ino.to_le_bytes());
        manifest.extend_from_slice(&old.to_le_bytes());
        manifest.extend_from_slice(&new.to_le_bytes());
        manifest.extend_from_slice(&changes.to_le_bytes());
    }

    fn push_ref(manifest: &mut Vec<u8>, record_type: u16, ino: u64, parent: u64, name: &[u8]) {
        manifest.extend_from_slice(&record_type.to_le_bytes());
        manifest.extend_from_slice(&0_u16.to_le_bytes());
        manifest.extend_from_slice(&(24_u32 + name.len() as u32).to_le_bytes());
        manifest.extend_from_slice(&ino.to_le_bytes());
        manifest.extend_from_slice(&parent.to_le_bytes());
        manifest.extend_from_slice(name);
    }

    #[test]
    fn parses_changed_objects_and_normalizes_reference_deltas() {
        let mut manifest = changed_objects_header();
        push_ref(&mut manifest, 2, 300, 256, b"same");
        push_ref(&mut manifest, 3, 300, 256, b"same");
        push_ref(&mut manifest, 2, 300, 256, b"new");
        push_object(
            &mut manifest,
            300,
            7,
            7,
            CHANGED_OBJECT_INODE | CHANGED_OBJECT_REF,
        );
        push_object(
            &mut manifest,
            301,
            8,
            0,
            CHANGED_OBJECT_INODE | CHANGED_OBJECT_DELETED,
        );

        assert_eq!(
            parse_changed_objects_manifest(&manifest).unwrap(),
            ChangedObjectsSummary {
                objects: 2,
                created: 0,
                deleted: 1,
                raw_ref_adds: 2,
                raw_ref_deletes: 1,
                net_ref_adds: 1,
                net_ref_deletes: 0,
            }
        );
    }

    #[test]
    fn rejects_malformed_changed_object_records() {
        let mut duplicate = changed_objects_header();
        push_object(&mut duplicate, 300, 7, 7, CHANGED_OBJECT_INODE);
        push_object(&mut duplicate, 300, 7, 7, CHANGED_OBJECT_INODE);
        assert!(
            parse_changed_objects_manifest(&duplicate)
                .unwrap_err()
                .contains("duplicate inode")
        );

        let mut missing_object = changed_objects_header();
        push_ref(&mut missing_object, 2, 300, 256, b"name");
        assert!(
            parse_changed_objects_manifest(&missing_object)
                .unwrap_err()
                .contains("no corresponding object")
        );

        duplicate.pop();
        assert!(parse_changed_objects_manifest(&duplicate).is_err());
    }
}
