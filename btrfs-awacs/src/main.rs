use btrfs_awacs::broker::{
    execute_changed_objects, execute_full_index, execute_snapshot_create, execute_snapshot_delete,
    execute_target_object_lookup, snapshot_create_effect_hash, snapshot_delete_effect_hash,
    snapshot_target_locator_hash, ChangedObjectsExecution, EffectKind, ExpectedManagedDirectory,
    ExpectedSubvolume, ReceiptRequest, SeqPacketListener, SessionGate, SnapshotCreateExecution,
    SnapshotDeleteExecution,
};
use btrfs_awacs::broker_protocol::BrokerDispatcher;
use btrfs_awacs::btrfs::{send_changed_objects, OpenedSubvolume};
use btrfs_awacs::facade::FacadeService;
use btrfs_awacs::manager::{Permissions, Principal, PERMISSION_CUT, PERMISSION_READ};
use btrfs_awacs::manifest::{
    parse_changed_objects, parse_changed_objects_v2, CHANGED_OBJECTS_V2_MAGIC,
};
use btrfs_awacs::namespace::NamespaceMonitor;
use btrfs_awacs::scan::{ScanSocket, ScanSocketListener, SocketScanClient, SocketScanDispatcher};
use btrfs_awacs::scan_facade::FacadeScanHandler;
use btrfs_awacs::service::{ChangesOptions, InitializeOptions, Service, ServiceConfig};
use btrfs_awacs::store::{BrokerJournal, ServiceMetadata, Store};
use clap::{Parser, Subcommand as ClapSubcommand, ValueEnum};
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

const SNAPSHOT_DIR: &str = ".btrfs-awacs";
const SNAPSHOT_PREFIX: &str = "snapshot-";
const CHANGED_OBJECTS_HELPER: &str = "__changed-objects-send";
const SCAN_SERVER: &str = "scan-serve";
const SEND_HELPER_UNSUPPORTED_EXIT_CODE: i32 = 2;
const EOPNOTSUPP: i32 = 95;

#[derive(Debug, Parser)]
#[command(
    name = "btrfs-awacs",
    version,
    about = "Btrfs snapshot change index, direct scan service, and benchmark tools"
)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, ClapSubcommand)]
enum CliCommand {
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
    /// Run the privileged filesystem-operation broker.
    BrokerServe {
        socket: PathBuf,
        receipt_db: PathBuf,
        manager_uid: u32,
        manager_gid: u32,
    },
    /// Run the direct immutable-snapshot scan service.
    ScanServe(ScanServeArgs),
    /// Discover or activate the namespace daemon and print its scan socket.
    ScanSockname {
        #[arg(value_name = "LIVE_ROOT")]
        root: PathBuf,
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
    /// Diagnostic: create a snapshot through the receipt-backed broker.
    #[command(name = "__broker-create-snapshot")]
    BrokerCreateSnapshot {
        source: PathBuf,
        destination_dir: PathBuf,
        destination_name: OsString,
        #[arg(value_enum)]
        mode: SnapshotMode,
        journal: PathBuf,
    },
    /// Diagnostic: delete a managed snapshot through the broker.
    #[command(name = "__broker-delete-snapshot")]
    BrokerDeleteSnapshot {
        target: PathBuf,
        containing_dir: PathBuf,
        target_name: OsString,
        journal: PathBuf,
    },
    /// Diagnostic: request a complete inode/reference index through the broker.
    #[command(name = "__broker-full-index")]
    BrokerFullIndex { snapshot: PathBuf },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum SnapshotMode {
    Ro,
    Rw,
}

#[derive(Debug, clap::Args)]
struct ScanServeArgs {
    socket: PathBuf,
    managed_dir: PathBuf,
    spool_dir: PathBuf,
    manager_db: PathBuf,
    broker_socket: PathBuf,
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
    let explicit_command = env::args_os().nth(1);
    let component = match explicit_command.as_deref() {
        Some(command) if command == OsStr::new(SCAN_SERVER) => "scan-serve",
        Some(command) if command == OsStr::new("broker-serve") => "broker-serve",
        _ => "btrfs-awacs",
    };
    let _tracing_guard = init_tracing(component);
    run_cli(Cli::parse());
}

fn init_tracing(component: &'static str) -> Option<WorkerGuard> {
    let path = match awacs_log_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("btrfs-awacs logging disabled: {error}");
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
            "btrfs-awacs logging disabled: create {}: {error}",
            parent.display()
        );
        return None;
    }
    let filename = path
        .file_name()
        .ok_or_else(|| "log path has no filename".to_owned())
        .ok()?;
    let appender = tracing_appender::rolling::never(parent, filename);
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("BTRFS_AWACS_LOG_FILTER")
        .unwrap_or_else(|_| EnvFilter::new("info"));
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
        eprintln!("btrfs-awacs logging disabled: subscriber already initialized");
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
    if let Some(path) = env::var_os("BTRFS_AWACS_LOG") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("BTRFS_AWACS_LOG must be an absolute path".to_owned());
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
        CliCommand::Snap { source } => finish_timed(|| run_benchmark(&source, true)),
        CliCommand::Compare { source } => finish_timed(|| run_benchmark(&source, false)),
        CliCommand::BrokerServe {
            socket,
            receipt_db,
            manager_uid,
            manager_gid,
        } => finish(run_broker_server(
            socket,
            receipt_db,
            manager_uid,
            manager_gid,
        )),
        CliCommand::ScanServe(arguments) => finish(run_scan_server(arguments)),
        CliCommand::ScanSockname { root } => finish(run_scan_sockname(&root)),
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
        CliCommand::BrokerCreateSnapshot {
            source,
            destination_dir,
            destination_name,
            mode,
            journal,
        } => finish(run_broker_create_snapshot_helper(
            &source,
            &destination_dir,
            &destination_name,
            mode == SnapshotMode::Ro,
            &journal,
        )),
        CliCommand::BrokerDeleteSnapshot {
            target,
            containing_dir,
            target_name,
            journal,
        } => finish(run_broker_delete_snapshot_helper(
            &target,
            &containing_dir,
            &target_name,
            &journal,
        )),
        CliCommand::BrokerFullIndex { snapshot } => finish(run_broker_full_index_helper(&snapshot)),
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

fn run_scan_sockname(root: &Path) -> Result<(), String> {
    let scan_socket = ensure_scan_daemon(root)?;
    validate_user_socket(&scan_socket)?;
    io::stdout()
        .write_all(scan_socket.as_os_str().as_bytes())
        .and_then(|()| io::stdout().write_all(&[0]))
        .map_err(|error| format!("write AWACS scan socket discovery response: {error}"))
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

fn finish_send_helper(result: Result<(), SendHelperError>) {
    if let Err(error) = result {
        eprintln!("error: {}", error.message());
        std::process::exit(match error {
            SendHelperError::Unsupported(_) => SEND_HELPER_UNSUPPORTED_EXIT_CODE,
            SendHelperError::Other(_) => 1,
        });
    }
}

fn ensure_scan_daemon(caller_directory: &Path) -> Result<PathBuf, String> {
    let socket = namespace_scan_socket()?;
    if socket.exists() && existing_scan_socket_is_live(&socket)? {
        return Ok(socket);
    }
    let namespace_directory = socket
        .parent()
        .ok_or_else(|| "namespace socket has no parent".to_owned())?;
    let lock_path = namespace_directory.join("daemon.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|error| format!("open daemon lock {}: {error}", lock_path.display()))?;
    // SAFETY: lock is live and LOCK_EX serializes namespace daemon activation.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!(
            "lock daemon activation: {}",
            io::Error::last_os_error()
        ));
    }
    if socket.exists() && existing_scan_socket_is_live(&socket)? {
        return Ok(socket);
    }
    let root = automatic_scan_root(caller_directory)?;
    let paths = automatic_scan_paths(&root)?;
    let executable = env::current_exe().map_err(|error| format!("locate btrfs-awacs: {error}"))?;
    let daemon_stderr = automatic_daemon_stderr()?;
    let mut child = Command::new(executable)
        .arg(SCAN_SERVER)
        .arg(&socket)
        .arg(&paths.managed_dir)
        .arg(&paths.spool_dir)
        .arg(&paths.manager_db)
        .arg(&paths.broker_socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Discovery is invoked with stderr captured by callers such as jj. A
        // long-lived daemon must not inherit that pipe, or the caller waits
        // forever for EOF after discovery itself exits.
        .stderr(daemon_stderr)
        .spawn()
        .map_err(|error| format!("start AWACS scan daemon: {error}"))?;
    // Keep the activation lock until the daemon publishes its socket so
    // concurrent discovery calls cannot spawn competing daemons.
    for _ in 0..12_000 {
        if socket.exists() {
            validate_user_socket(&socket)?;
            return Ok(socket);
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect AWACS scan daemon: {error}"))?
        {
            return Err(format!("AWACS scan daemon exited with {status}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("timed out waiting for AWACS scan daemon socket".to_owned())
}

#[derive(Debug)]
struct AutomaticScanPaths {
    managed_dir: PathBuf,
    spool_dir: PathBuf,
    manager_db: PathBuf,
    broker_socket: PathBuf,
}

fn automatic_scan_root(caller_directory: &Path) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(caller_directory)
        .map_err(|error| format!("canonicalize AWACS caller directory: {error}"))?;
    // Discovery may be invoked from somewhere inside a working copy. Prefer
    // the nearest repository root so snapshots cover the whole tree rather
    // than only the caller's current subdirectory.
    for candidate in canonical.ancestors() {
        if candidate.join(".jj").exists() || candidate.join(".git").exists() {
            return Ok(candidate.to_path_buf());
        }
    }
    Ok(canonical)
}

fn automatic_scan_paths(root: &Path) -> Result<AutomaticScanPaths, String> {
    let state_home = match env::var_os("XDG_STATE_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(
            env::var_os("HOME")
                .ok_or_else(|| "HOME is required when XDG_STATE_HOME is unset".to_owned())?,
        )
        .join(".local/state"),
    };
    let state_dir = state_home.join("btrfs-awacs");
    fs::create_dir_all(&state_dir).map_err(|error| {
        format!(
            "create AWACS state directory {}: {error}",
            state_dir.display()
        )
    })?;
    let root_parent = root
        .parent()
        .ok_or_else(|| format!("AWACS root {} has no parent", root.display()))?;
    let managed_dir = env::var_os("BTRFS_AWACS_MANAGED_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root_parent.join(".btrfs-awacs-managed"));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&managed_dir)
        .map_err(|error| {
            format!(
                "create AWACS managed directory {}: {error}",
                managed_dir.display()
            )
        })?;
    let spool_dir = env::var_os("BTRFS_AWACS_SPOOL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("spool"));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&spool_dir)
        .map_err(|error| {
            format!(
                "create AWACS spool directory {}: {error}",
                spool_dir.display()
            )
        })?;
    let manager_db = env::var_os("BTRFS_AWACS_MANAGER_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("manager.sqlite3"));
    if let Some(parent) = manager_db.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| {
                format!(
                    "create AWACS manager directory {}: {error}",
                    parent.display()
                )
            })?;
    }
    Ok(AutomaticScanPaths {
        // Snapshot clones must stay on the live root's Btrfs filesystem.
        managed_dir,
        spool_dir,
        manager_db,
        broker_socket: env::var_os("BTRFS_AWACS_BROKER_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/run/btrfs-awacs/broker.sock")),
    })
}

fn automatic_daemon_stderr() -> Result<Stdio, String> {
    let path = awacs_log_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "AWACS log path has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create AWACS log directory {}: {error}", parent.display()))?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("open AWACS daemon error log {}: {error}", path.display()))?;
    Ok(Stdio::from(file))
}

fn namespace_scan_socket() -> Result<PathBuf, String> {
    let runtime = PathBuf::from(
        env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| "XDG_RUNTIME_DIR is required".to_owned())?,
    );
    validate_private_runtime(&runtime)?;
    let namespace = fs::metadata("/proc/self/ns/mnt")
        .map_err(|error| format!("stat mount namespace: {error}"))?;
    let base = runtime.join("btrfs-awacs");
    create_private_directory(&base)?;
    let directory = base.join(format!("mnt-{}-{}", namespace.dev(), namespace.ino()));
    create_private_directory(&directory)?;
    Ok(directory.join("scan.sock"))
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!(
                "create runtime directory {}: {error}",
                path.display()
            ))
        }
    }
    validate_private_runtime(path)
}

fn validate_private_runtime(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat runtime directory {}: {error}", path.display()))?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "runtime directory {} must be owned by uid {uid} and mode 0700 or stricter",
            path.display()
        ));
    }
    Ok(())
}

fn validate_user_socket(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat AWACS socket {}: {error}", path.display()))?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(format!(
            "AWACS socket {} has unsafe type, owner, or mode",
            path.display()
        ));
    }
    Ok(())
}

fn existing_scan_socket_is_live(path: &Path) -> Result<bool, String> {
    validate_user_socket(path)?;
    match SocketScanClient::connect(path) {
        Ok(_) => Ok(true),
        Err(error)
            if error.message().contains("Connection refused")
                || error.message().contains("No such file or directory") =>
        {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "remove stale AWACS scan socket {}: {error}",
                        path.display()
                    ));
                }
            }
            Ok(false)
        }
        Err(error) => Err(format!(
            "connect to existing AWACS scan socket {}: {error}",
            path.display()
        )),
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

fn run_broker_create_snapshot_helper(
    source_path: &Path,
    destination_path: &Path,
    destination_name: &OsStr,
    readonly: bool,
    journal_path: &Path,
) -> Result<(), String> {
    let source = OpenedSubvolume::open(source_path).map_err(|error| error.to_string())?;
    let destination = File::open(destination_path)
        .map_err(|error| format!("open destination {}: {error}", destination_path.display()))?;
    let destination_parent = ExpectedManagedDirectory::from_observed(destination.as_fd())
        .map_err(|error| error.to_string())?;
    let gate = SessionGate::default();
    let manager_store_uuid = [0x31; 16];
    let manager_session_id = gate.handshake(manager_store_uuid);
    let receipt = ReceiptRequest {
        id: [0x32; 16],
        manager_store_uuid,
        manager_session_id,
        operation_id: [0x33; 16],
        operation_fence: 1,
        effect_kind: EffectKind::SnapshotCreate,
        filesystem_uuid: source.filesystem.fs_uuid,
        target_locator_hash: [0; 32],
        effect_arguments_hash: [0; 32],
        boot_id: [0x34; 16],
        started_ns: 1,
    };
    let mut execution = SnapshotCreateExecution {
        receipt,
        source: ExpectedSubvolume::from_observed(&source.filesystem, &source.subvolume),
        destination_parent,
        destination_name: destination_name.as_bytes().to_vec(),
        readonly,
    };
    execution.receipt.target_locator_hash =
        snapshot_target_locator_hash(&execution.destination_parent, &execution.destination_name);
    execution.receipt.effect_arguments_hash = snapshot_create_effect_hash(&execution);
    let mut journal = BrokerJournal::create(journal_path)
        .map_err(|error| format!("create broker journal: {error}"))?;

    let first = execute_snapshot_create(
        &gate,
        &mut journal,
        &execution,
        source.as_fd(),
        destination.as_fd(),
    )
    .map_err(|error| error.to_string())?;
    let repeated = execute_snapshot_create(
        &gate,
        &mut journal,
        &execution,
        source.as_fd(),
        destination.as_fd(),
    )
    .map_err(|error| format!("idempotent replay failed: {error}"))?;
    if repeated != first {
        return Err("idempotent snapshot replay returned a different result".to_owned());
    }
    println!(
        "subvol_uuid={} root_id={} readonly={} result_sha256={} idempotent=true",
        hex_bytes(&first.snapshot.subvolume_uuid),
        first.snapshot.root_id,
        first.snapshot.readonly,
        hex_bytes(&first.result_hash),
    );
    Ok(())
}

fn run_broker_delete_snapshot_helper(
    target_path: &Path,
    destination_path: &Path,
    destination_name: &OsStr,
    journal_path: &Path,
) -> Result<(), String> {
    let target = OpenedSubvolume::open(target_path).map_err(|error| error.to_string())?;
    let destination = File::open(destination_path)
        .map_err(|error| format!("open destination {}: {error}", destination_path.display()))?;
    let destination_parent = ExpectedManagedDirectory::from_observed(destination.as_fd())
        .map_err(|error| error.to_string())?;
    let gate = SessionGate::default();
    let manager_store_uuid = [0x41; 16];
    let manager_session_id = gate.handshake(manager_store_uuid);
    let receipt = ReceiptRequest {
        id: [0x42; 16],
        manager_store_uuid,
        manager_session_id,
        operation_id: [0x43; 16],
        operation_fence: 1,
        effect_kind: EffectKind::SnapshotDelete,
        filesystem_uuid: target.filesystem.fs_uuid,
        target_locator_hash: [0; 32],
        effect_arguments_hash: [0; 32],
        boot_id: [0x44; 16],
        started_ns: 1,
    };
    let mut execution = SnapshotDeleteExecution {
        receipt,
        target: ExpectedSubvolume::from_observed(&target.filesystem, &target.subvolume),
        destination_parent,
        destination_name: destination_name.as_bytes().to_vec(),
    };
    execution.receipt.target_locator_hash =
        snapshot_target_locator_hash(&execution.destination_parent, &execution.destination_name);
    execution.receipt.effect_arguments_hash = snapshot_delete_effect_hash(&execution);
    let mut journal = BrokerJournal::create(journal_path)
        .map_err(|error| format!("create broker journal: {error}"))?;

    let first = execute_snapshot_delete(&gate, &mut journal, &execution, destination.as_fd())
        .map_err(|error| error.to_string())?;
    let repeated = execute_snapshot_delete(&gate, &mut journal, &execution, destination.as_fd())
        .map_err(|error| format!("idempotent replay failed: {error}"))?;
    if repeated != first {
        return Err("idempotent deletion replay returned a different result".to_owned());
    }
    println!(
        "deleted_subvol_uuid={} result_sha256={} idempotent=true",
        hex_bytes(&first.deleted_subvolume_uuid),
        hex_bytes(&first.result_hash),
    );
    Ok(())
}

fn run_broker_full_index_helper(snapshot_path: &Path) -> Result<(), String> {
    let snapshot = OpenedSubvolume::open(snapshot_path).map_err(|error| error.to_string())?;
    let expected = ExpectedSubvolume::from_observed(&snapshot.filesystem, &snapshot.subvolume);
    let index =
        execute_full_index(&expected, snapshot.as_fd()).map_err(|error| error.to_string())?;
    let inodes = index.objects.keys().copied().collect();
    let targeted = execute_target_object_lookup(&expected, snapshot.as_fd(), &inodes)
        .map_err(|error| error.to_string())?;
    if targeted != index.objects {
        return Err("targeted object lookup differs from full index".to_owned());
    }
    let safety = index.safety_summary();
    println!(
        "objects={} refs={} state_sha256={} security_sha256={} owner={} privileged={}",
        index.objects.len(),
        index.references.len(),
        hex_bytes(&index.state_hash()),
        hex_bytes(&safety.security_state_hash),
        safety
            .single_owner_uid
            .map_or_else(|| "mixed".to_owned(), |uid| uid.to_string()),
        safety.privileged_metadata_count,
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
    journal_path: PathBuf,
    manager_uid: u32,
    manager_gid: u32,
) -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("broker-serve must run as root".to_owned());
    }
    validate_root_broker_directory(
        socket_path
            .parent()
            .ok_or_else(|| "broker socket has no parent directory".to_owned())?,
        false,
    )?;
    validate_root_broker_directory(
        journal_path
            .parent()
            .ok_or_else(|| "broker journal has no parent directory".to_owned())?,
        true,
    )?;
    let mut journal = if journal_path.exists() {
        BrokerJournal::open(&journal_path)
    } else {
        BrokerJournal::create(&journal_path)
    }
    .map_err(|error| format!("open broker receipt journal: {error}"))?;
    journal
        .recover_interrupted_receipts()
        .map_err(|error| format!("recover interrupted broker receipts: {error}"))?;
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
    let dispatcher = std::sync::Arc::new(BrokerDispatcher::with_journal(manager_uid, journal));
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

fn run_scan_server(arguments: ScanServeArgs) -> Result<(), String> {
    let ScanServeArgs {
        socket: socket_path,
        managed_dir: managed,
        spool_dir: spool,
        manager_db: store_path,
        broker_socket,
    } = arguments;
    let runtime = socket_path
        .parent()
        .ok_or_else(|| "AWACS scan socket has no parent directory".to_owned())?;
    let runtime_metadata = fs::symlink_metadata(runtime)
        .map_err(|error| format!("stat runtime directory {}: {error}", runtime.display()))?;
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    if !runtime_metadata.is_dir()
        || runtime_metadata.uid() != uid
        || runtime_metadata.permissions().mode() & 0o077 != 0
    {
        return Err(format!(
            "runtime directory {} must be owned by uid {uid} and mode 0700 or stricter",
            runtime.display()
        ));
    }
    if socket_path.exists() {
        return Err(format!(
            "refusing to replace existing AWACS scan socket {}",
            socket_path.display()
        ));
    }
    let boot_id = read_boot_id()?;
    let store = if store_path.exists() {
        Store::open(&store_path).map_err(|error| format!("open manager store: {error}"))?
    } else {
        let metadata = ServiceMetadata::generate(boot_id, current_time_ns()?)
            .map_err(|error| format!("create manager metadata: {error}"))?;
        Store::create(&store_path, &metadata)
            .map_err(|error| format!("create manager store: {error}"))?
    };
    let config = ServiceConfig::new(managed, spool, boot_id).with_broker_socket(broker_socket);
    let service = Service::new_external(store, config).map_err(|error| error.to_string())?;
    // Keep maintenance on its own store/broker connection. Snapshot deletion
    // can include filesystem durability work, so it must not hold the
    // request facade mutex while a client is waiting for Begin/Renew/Finish.
    let mut maintenance_service = service
        .maintenance_worker()
        .map_err(|error| format!("create AWACS maintenance worker: {error}"))?;
    let facade = std::sync::Arc::new(std::sync::Mutex::new(FacadeService::new(service)));
    let precision_marker_directory = (env::var_os("BTRFS_AWACS_PRECISION_GUARD").as_deref()
        == Some(OsStr::new("1")))
    .then(|| runtime.to_path_buf());
    let listener = ScanSocketListener::bind(&socket_path, 0o600)
        .map_err(|error| format!("bind AWACS scan listener: {error}"))?;
    let dispatcher = std::sync::Arc::new(SocketScanDispatcher::new(FacadeScanHandler::new(
        std::sync::Arc::clone(&facade),
        precision_marker_directory,
        uid,
        gid,
    )));
    std::thread::Builder::new()
        .name("btrfs-awacs-maintenance".to_owned())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(30));
            let now_ns = match current_time_ns() {
                Ok(now_ns) => now_ns,
                Err(error) => {
                    warn!(error = %error, "skip AWACS maintenance tick");
                    continue;
                }
            };
            let started = Instant::now();
            let result = maintenance_service.maintenance_tick(now_ns);
            match result {
                Ok(report) => info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    expired_query_leases = report.expired_query_leases,
                    expired_retention_leases = report.expired_retention_leases,
                    expired_historical_comparisons = report.expired_historical_comparisons,
                    watches_processed = report.watches_processed,
                    history_rows_reclaimed = report.history_rows_reclaimed,
                    snapshots_deleted = report.snapshots_deleted,
                    more_work = report.more_work,
                    "AWACS maintenance tick completed"
                ),
                Err(error) => warn!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %error,
                    "AWACS maintenance tick failed"
                ),
            }
        })
        .map_err(|error| format!("start AWACS maintenance worker: {error}"))?;
    loop {
        let connection = listener
            .accept()
            .map_err(|error| format!("accept AWACS scan connection: {error}"))?;
        if let Err(error) = validate_scan_peer_view(&connection, uid) {
            warn!(error = %error, "reject AWACS scan peer");
            continue;
        }
        let dispatcher = std::sync::Arc::clone(&dispatcher);
        std::thread::Builder::new()
            .name("btrfs-awacs-scan-client".to_owned())
            .spawn(move || while dispatcher.serve_one(&connection).is_ok() {})
            .map_err(|error| format!("start AWACS scan client worker: {error}"))?;
    }
}
fn validate_scan_peer_view(socket: &ScanSocket, expected_uid: u32) -> Result<(), String> {
    let peer = socket
        .peer_credentials()
        .map_err(|error| format!("read AWACS scan peer credentials: {error}"))?;
    if peer.uid != expected_uid {
        return Err(format!(
            "AWACS scan peer uid {} does not match daemon uid {expected_uid}",
            peer.uid
        ));
    }
    let peer_mount_namespace = fs::metadata(format!("/proc/{}/ns/mnt", peer.pid))
        .map_err(|error| format!("stat AWACS peer mount namespace: {error}"))?;
    let own_mount_namespace = fs::metadata("/proc/self/ns/mnt")
        .map_err(|error| format!("stat AWACS daemon mount namespace: {error}"))?;
    if peer_mount_namespace.dev() != own_mount_namespace.dev()
        || peer_mount_namespace.ino() != own_mount_namespace.ino()
    {
        return Err("AWACS scan peer is in a different mount namespace".to_owned());
    }
    let peer_root = fs::metadata(format!("/proc/{}/root", peer.pid))
        .map_err(|error| format!("stat AWACS peer process root: {error}"))?;
    let own_root = fs::metadata("/proc/self/root")
        .map_err(|error| format!("stat AWACS daemon process root: {error}"))?;
    if peer_root.dev() != own_root.dev() || peer_root.ino() != own_root.ino() {
        return Err("AWACS scan peer has a different process root".to_owned());
    }
    Ok(())
}

fn parse_hex_id(value: &OsStr) -> Result<[u8; 16], String> {
    let value = value.as_bytes();
    if value.len() != 32 {
        return Err("identifier must contain exactly 32 hexadecimal bytes".to_owned());
    }
    let mut output = [0_u8; 16];
    for (index, pair) in value.chunks_exact(2).enumerate() {
        let digit = |byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        output[index] = digit(pair[0])
            .zip(digit(pair[1]))
            .map(|(high, low)| high * 16 + low)
            .ok_or_else(|| "identifier contains a non-hexadecimal byte".to_owned())?;
    }
    Ok(output)
}

fn read_boot_id() -> Result<[u8; 16], String> {
    let text = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("read kernel boot ID: {error}"))?;
    let compact: String = text
        .trim()
        .chars()
        .filter(|character| *character != '-')
        .collect();
    parse_hex_id(OsStr::new(&compact))
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

        if let Some(path) = path {
            if path != "." && path != "./" {
                paths.insert(path);
            }
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
        changed_paths, last_two_snapshots, parse_changed_objects_manifest, parse_hex_id,
        require_last_two_snapshots, validate_user_socket, ChangedObjectsSummary, Cli, CliCommand,
    };
    use btrfs_awacs::manifest::{
        CHANGE_DELETED as CHANGED_OBJECT_DELETED, CHANGE_INODE as CHANGED_OBJECT_INODE,
        CHANGE_REF as CHANGED_OBJECT_REF,
    };
    use clap::{CommandFactory, Parser};
    use std::ffi::OsStr;
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    #[test]
    fn parses_fixed_width_runtime_identifiers() {
        assert_eq!(
            parse_hex_id(OsStr::new("000102030405060708090a0b0c0d0e0f")).unwrap(),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert!(parse_hex_id(OsStr::new("00")).is_err());
        assert!(parse_hex_id(OsStr::new("gg0102030405060708090a0b0c0d0e0f")).is_err());
    }

    #[test]
    fn discovery_accepts_only_an_owned_scan_socket() {
        let temp = tempdir().unwrap();
        let socket = temp.path().join("scan.sock");
        let _listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => return,
            Err(error) => panic!("bind discovery test socket: {error}"),
        };
        if let Err(error) =
            std::fs::set_permissions(&socket, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        {
            if error.raw_os_error() == Some(libc::EPERM) {
                return;
            }
            panic!("set socket permissions: {error}");
        }
        validate_user_socket(&socket).unwrap();
        let regular = temp.path().join("not-a-socket");
        std::fs::write(&regular, b"").unwrap();
        assert!(validate_user_socket(&regular).is_err());
    }

    #[test]
    fn parses_snap_and_compare_subcommands() {
        let snap = Cli::try_parse_from(["btrfs-awacs", "snap", "."]).unwrap();
        assert!(matches!(
            snap.command,
            CliCommand::Snap { source } if source == Path::new(".")
        ));
        let compare = Cli::try_parse_from(["btrfs-awacs", "compare", "."]).unwrap();
        assert!(matches!(
            compare.command,
            CliCommand::Compare { source } if source == Path::new(".")
        ));
    }

    #[test]
    fn rejects_old_interface_and_extra_arguments() {
        assert!(Cli::try_parse_from(["btrfs-awacs", "changes", "."]).is_err());
        assert!(Cli::try_parse_from(["btrfs-awacs", "compare", "--timing", "."]).is_err());
        assert!(Cli::try_parse_from(["btrfs-awacs", "snap", ".", "extra"]).is_err());
    }

    #[test]
    fn default_help_lists_every_subcommand() {
        let help = Cli::command().render_long_help().to_string();
        for command in [
            "snap",
            "compare",
            "broker-serve",
            "scan-serve",
            "scan-sockname",
            "__changed-objects-send",
            "__btrfs-inspect",
            "__broker-changed-objects",
            "__broker-create-snapshot",
            "__broker-delete-snapshot",
            "__broker-full-index",
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
        assert!(parse_changed_objects_manifest(&no_ref_mask)
            .unwrap_err()
            .contains("no ref change"));

        let mut no_ref_records = changed_objects_header();
        push_object(
            &mut no_ref_records,
            300,
            7,
            7,
            CHANGED_OBJECT_INODE | CHANGED_OBJECT_REF,
        );
        assert!(parse_changed_objects_manifest(&no_ref_records)
            .unwrap_err()
            .contains("no ref records"));
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
        assert!(parse_changed_objects_manifest(&duplicate)
            .unwrap_err()
            .contains("duplicate inode"));

        let mut missing_object = changed_objects_header();
        push_ref(&mut missing_object, 2, 300, 256, b"name");
        assert!(parse_changed_objects_manifest(&missing_object)
            .unwrap_err()
            .contains("no corresponding object"));

        duplicate.pop();
        assert!(parse_changed_objects_manifest(&duplicate).is_err());
    }
}
