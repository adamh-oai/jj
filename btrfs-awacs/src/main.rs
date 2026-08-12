use btrfs_awacs::broker::{
    execute_changed_objects, execute_full_index, execute_snapshot_create, execute_snapshot_delete,
    execute_target_object_lookup, execute_worktree_rename, snapshot_create_effect_hash,
    snapshot_delete_effect_hash, snapshot_target_locator_hash, worktree_rename_effect_hash,
    ChangedObjectsExecution, EffectKind, ExpectedManagedDirectory, ExpectedReservation,
    ExpectedSubvolume, ReceiptRequest, SeqPacketListener, SessionGate, SnapshotCreateExecution,
    SnapshotDeleteExecution, WorktreeRenameExecution,
};
use btrfs_awacs::broker_protocol::BrokerDispatcher;
use btrfs_awacs::bser::{decode_frame, encode_frame, Limits as BserLimits, Value as BserValue};
use btrfs_awacs::btrfs::{send_changed_objects, OpenedSubvolume};
use btrfs_awacs::compat::ClientFlavor;
use btrfs_awacs::facade::FacadeService;
use btrfs_awacs::git_fsmonitor::{run_hook_over_socket, run_hook_v2};
use btrfs_awacs::manager::{
    Permissions, Principal, PERMISSION_CUT, PERMISSION_READ, PERMISSION_RETAIN, PERMISSION_TRIGGER,
};
use btrfs_awacs::manifest::{
    parse_changed_objects, parse_changed_objects_v2, CHANGED_OBJECTS_V2_MAGIC,
};
use btrfs_awacs::service::{
    ChangesOptions, InitializeOptions, Service, ServiceConfig, WorktreeOptions,
};
use btrfs_awacs::store::{BrokerJournal, ServiceMetadata, Store};
use btrfs_awacs::trigger::{
    claim_one_pending, execute_claimed, finish_claimed, TriggerCommandConfig,
};
use btrfs_awacs::watchman::WatchmanEndpoint;
use btrfs_awacs::watchman_transport::CredentialedStream;
use clap::{Parser, Subcommand as ClapSubcommand, ValueEnum};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SNAPSHOT_DIR: &str = ".btrfs-awacs";
const SNAPSHOT_PREFIX: &str = "snapshot-";
const CHANGED_OBJECTS_HELPER: &str = "__changed-objects-send";
const WATCHMAN_SERVER: &str = "watchman-serve";
const WATCHMAN_SERVER_PROGRAM: &str = "btrfs-awacs-watchman";
const GIT_FSMONITOR_PROGRAM: &str = "git-fsmonitor-hook";
const SEND_HELPER_UNSUPPORTED_EXIT_CODE: i32 = 2;
const EOPNOTSUPP: i32 = 95;

#[derive(Debug, Parser)]
#[command(
    name = "btrfs-awacs",
    version,
    about = "Btrfs snapshot change index, focused Watchman service, and benchmark tools",
    after_help = "Installed multicall entry points:\n  watchman              Watchman discovery shim\n  btrfs-awacs-watchman  Focused Watchman server\n  git-fsmonitor-hook    Git fsmonitor hook protocol v2"
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
    /// Run the focused Watchman-compatible per-user service.
    WatchmanServe(WatchmanServeArgs),
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
    /// Diagnostic: publish a staged writable Worktree through the broker.
    #[command(name = "__broker-publish-worktree")]
    BrokerPublishWorktree {
        staged_worktree: PathBuf,
        staging_dir: PathBuf,
        staging_name: OsString,
        destination_dir: PathBuf,
        destination_name: OsString,
        reservation_name: OsString,
        journal: PathBuf,
    },
    /// Diagnostic: request a complete inode/reference index through the broker.
    #[command(name = "__broker-full-index")]
    BrokerFullIndex { snapshot: PathBuf },
    /// Acceptance helper: exercise the complete service workflow.
    #[command(name = "__service-smoke")]
    ServiceSmoke {
        source: PathBuf,
        managed_dir: PathBuf,
        spool_dir: PathBuf,
        manager_db: PathBuf,
        broker_journal: PathBuf,
    },
    /// Acceptance helper: exercise crash recovery boundaries.
    #[command(name = "__service-recovery-smoke")]
    ServiceRecoverySmoke {
        source: PathBuf,
        managed_dir: PathBuf,
        spool_dir: PathBuf,
        manager_db: PathBuf,
    },
    /// Acceptance helper: prove nested-subvolume rejection.
    #[command(name = "__nested-boundary-smoke")]
    NestedBoundarySmoke {
        source: PathBuf,
        managed_dir: PathBuf,
        spool_dir: PathBuf,
        manager_db: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum SnapshotMode {
    Ro,
    Rw,
}

#[derive(Debug, clap::Args)]
struct WatchmanServeArgs {
    socket: PathBuf,
    root: PathBuf,
    managed_dir: PathBuf,
    spool_dir: PathBuf,
    manager_db: PathBuf,
    broker_socket: PathBuf,
    watch_id: OsString,
    grant_id: OsString,
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
    let invoked_name = env::args_os()
        .next()
        .as_deref()
        .and_then(|name| Path::new(name).file_name())
        .map(OsStr::to_owned);
    let invoked_as_git_hook = invoked_name.as_deref() == Some(OsStr::new(GIT_FSMONITOR_PROGRAM));
    if invoked_as_git_hook {
        if let Err(error) = run_git_fsmonitor_program() {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
        return;
    }
    if invoked_name.as_deref() == Some(OsStr::new("watchman")) {
        if let Err(error) = run_watchman_discovery_shim() {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
        return;
    }
    if invoked_name.as_deref() == Some(OsStr::new(WATCHMAN_SERVER_PROGRAM)) {
        let mut arguments = env::args_os().collect::<Vec<_>>();
        arguments.insert(1, OsString::from(WATCHMAN_SERVER));
        run_cli(Cli::parse_from(arguments));
        return;
    }
    run_cli(Cli::parse());
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
        CliCommand::WatchmanServe(arguments) => finish(run_watchman_server(arguments)),
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
        CliCommand::BrokerPublishWorktree {
            staged_worktree,
            staging_dir,
            staging_name,
            destination_dir,
            destination_name,
            reservation_name,
            journal,
        } => finish(run_broker_publish_worktree_helper(
            &staged_worktree,
            &staging_dir,
            &staging_name,
            &destination_dir,
            &destination_name,
            &reservation_name,
            &journal,
        )),
        CliCommand::BrokerFullIndex { snapshot } => finish(run_broker_full_index_helper(&snapshot)),
        CliCommand::ServiceSmoke {
            source,
            managed_dir,
            spool_dir,
            manager_db,
            broker_journal,
        } => finish(run_service_smoke_helper(
            source,
            managed_dir,
            spool_dir,
            manager_db,
            broker_journal,
        )),
        CliCommand::ServiceRecoverySmoke {
            source,
            managed_dir,
            spool_dir,
            manager_db,
        } => finish(run_service_recovery_smoke_helper(
            source,
            managed_dir,
            spool_dir,
            manager_db,
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
    }
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

fn run_git_fsmonitor_program() -> Result<(), String> {
    let socket = match env::var_os("BTRFS_AWACS_SOCKET") {
        Some(socket) => PathBuf::from(socket),
        None => ensure_watchman_daemon()?,
    };
    let root =
        env::current_dir().map_err(|error| format!("read Git worktree directory: {error}"))?;
    let argv = env::args_os()
        .skip(1)
        .map(|value| value.as_bytes().to_vec())
        .collect::<Vec<_>>();
    let response =
        run_hook_over_socket(&socket, &root, &argv).map_err(|error| error.to_string())?;
    io::stdout()
        .write_all(&response)
        .and_then(|()| io::stdout().flush())
        .map_err(|error| format!("write Git fsmonitor response: {error}"))
}

fn run_watchman_discovery_shim() -> Result<(), String> {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.as_slice()
        != [
            OsString::from("--output-encoding"),
            OsString::from("bser-v2"),
            OsString::from("get-sockname"),
        ]
        && arguments.as_slice()
            != [
                OsString::from("--output-encoding=bser-v2"),
                OsString::from("get-sockname"),
            ]
    {
        return Err(
            "focused watchman shim supports only --output-encoding bser-v2 get-sockname".to_owned(),
        );
    }
    let socket = ensure_watchman_daemon()?;
    let response = BserValue::Object(BTreeMap::from([
        (
            b"version".to_vec(),
            BserValue::Bytes(b"btrfs-awacs-0.1".to_vec()),
        ),
        (
            b"sockname".to_vec(),
            BserValue::Bytes(socket.as_os_str().as_bytes().to_vec()),
        ),
    ]));
    let frame = encode_frame(&response, BserLimits::default())
        .map_err(|error| format!("encode Watchman discovery response: {error}"))?;
    io::stdout()
        .write_all(&frame)
        .and_then(|()| io::stdout().flush())
        .map_err(|error| format!("write Watchman discovery response: {error}"))
}

fn ensure_watchman_daemon() -> Result<PathBuf, String> {
    let socket = namespace_watchman_socket()?;
    if socket.exists() {
        validate_user_socket(&socket)?;
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
    if socket.exists() {
        validate_user_socket(&socket)?;
        return Ok(socket);
    }
    let required = |name: &str| {
        env::var_os(name)
            .ok_or_else(|| format!("{name} is required to activate the focused Watchman daemon"))
    };
    let executable = env::current_exe().map_err(|error| format!("locate btrfs-awacs: {error}"))?;
    let mut child = Command::new(executable)
        .arg(WATCHMAN_SERVER)
        .arg(&socket)
        .arg(required("BTRFS_AWACS_ROOT")?)
        .arg(required("BTRFS_AWACS_MANAGED_DIR")?)
        .arg(required("BTRFS_AWACS_SPOOL_DIR")?)
        .arg(required("BTRFS_AWACS_MANAGER_DB")?)
        .arg(required("BTRFS_AWACS_BROKER_SOCKET")?)
        .arg(env::var_os("BTRFS_AWACS_WATCH_ID").unwrap_or_else(|| OsString::from("auto")))
        .arg(env::var_os("BTRFS_AWACS_GRANT_ID").unwrap_or_else(|| OsString::from("auto")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("start focused Watchman daemon: {error}"))?;
    for _ in 0..100 {
        if socket.exists() {
            validate_user_socket(&socket)?;
            return Ok(socket);
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect focused Watchman daemon: {error}"))?
        {
            return Err(format!("focused Watchman daemon exited with {status}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("timed out waiting for focused Watchman daemon socket".to_owned())
}

fn namespace_watchman_socket() -> Result<PathBuf, String> {
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
    Ok(directory.join("watchman.sock"))
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
        .map_err(|error| format!("stat Watchman socket {}: {error}", path.display()))?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(format!(
            "Watchman socket {} has unsafe type, owner, or mode",
            path.display()
        ));
    }
    Ok(())
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

fn run_broker_publish_worktree_helper(
    staged_path: &Path,
    staging_parent_path: &Path,
    staging_name: &OsStr,
    destination_parent_path: &Path,
    destination_name: &OsStr,
    reservation_name: &OsStr,
    journal_path: &Path,
) -> Result<(), String> {
    let staged = OpenedSubvolume::open(staged_path).map_err(|error| error.to_string())?;
    let staging_parent = File::open(staging_parent_path).map_err(|error| {
        format!(
            "open staging parent {}: {error}",
            staging_parent_path.display()
        )
    })?;
    let destination_parent = File::open(destination_parent_path).map_err(|error| {
        format!(
            "open destination parent {}: {error}",
            destination_parent_path.display()
        )
    })?;
    let canonical_destination = fs::canonicalize(destination_parent_path)
        .map_err(|error| format!("canonicalize destination parent: {error}"))?;
    let mut destination_root_path = canonical_destination.clone();
    let destination_root = loop {
        if let Ok(opened) = OpenedSubvolume::open(&destination_root_path) {
            break opened;
        }
        if !destination_root_path.pop() {
            return Err("destination parent has no enclosing Btrfs subvolume root".to_owned());
        }
    };
    let destination_relative_parent = canonical_destination
        .strip_prefix(&destination_root_path)
        .map_err(|_| "destination parent escaped its discovered policy root".to_owned())?
        .as_os_str()
        .as_bytes()
        .to_vec();
    let reservation_path = destination_parent_path.join(reservation_name);
    let reservation_bytes = fs::read(&reservation_path)
        .map_err(|error| format!("read {}: {error}", reservation_path.display()))?;
    let nonce: [u8; 32] = reservation_bytes.try_into().map_err(|bytes: Vec<u8>| {
        format!(
            "reservation {} has {} bytes, expected 32",
            reservation_path.display(),
            bytes.len()
        )
    })?;
    let destination_identity = ExpectedManagedDirectory::from_observed(destination_parent.as_fd())
        .map_err(|error| error.to_string())?;
    let reservation = ExpectedReservation::from_observed(
        destination_parent.as_fd(),
        reservation_name.as_bytes(),
        unsafe { libc::geteuid() },
        nonce,
    )
    .map_err(|error| error.to_string())?;
    let gate = SessionGate::default();
    let manager_store_uuid = [0x51; 16];
    let manager_session_id = gate.handshake(manager_store_uuid);
    let receipt = ReceiptRequest {
        id: [0x52; 16],
        manager_store_uuid,
        manager_session_id,
        operation_id: [0x53; 16],
        operation_fence: 1,
        effect_kind: EffectKind::WorktreeRename,
        filesystem_uuid: staged.filesystem.fs_uuid,
        target_locator_hash: [0; 32],
        effect_arguments_hash: [0; 32],
        boot_id: [0x54; 16],
        started_ns: 1,
    };
    let mut execution = WorktreeRenameExecution {
        receipt,
        worktree: ExpectedSubvolume::from_observed(&staged.filesystem, &staged.subvolume),
        staging_parent: ExpectedManagedDirectory::from_observed(staging_parent.as_fd())
            .map_err(|error| error.to_string())?,
        staging_name: staging_name.as_bytes().to_vec(),
        destination_parent: destination_identity,
        destination_root: ExpectedSubvolume::from_observed(
            &destination_root.filesystem,
            &destination_root.subvolume,
        ),
        destination_root_directory: ExpectedManagedDirectory::from_observed(
            destination_root.as_fd(),
        )
        .map_err(|error| error.to_string())?,
        destination_relative_parent,
        destination_name: destination_name.as_bytes().to_vec(),
        reservation,
        authorization_hash: [0x55; 32],
    };
    execution.receipt.target_locator_hash =
        snapshot_target_locator_hash(&execution.destination_parent, &execution.destination_name);
    execution.receipt.effect_arguments_hash = worktree_rename_effect_hash(&execution);
    let mut journal = BrokerJournal::create(journal_path)
        .map_err(|error| format!("create broker journal: {error}"))?;

    let first = execute_worktree_rename(
        &gate,
        &mut journal,
        &execution,
        staging_parent.as_fd(),
        destination_root.as_fd(),
    )
    .map_err(|error| error.to_string())?;
    let repeated = execute_worktree_rename(
        &gate,
        &mut journal,
        &execution,
        staging_parent.as_fd(),
        destination_root.as_fd(),
    )
    .map_err(|error| format!("idempotent replay failed: {error}"))?;
    if repeated != first {
        return Err("idempotent worktree replay returned a different result".to_owned());
    }
    println!(
        "worktree_subvol_uuid={} result_sha256={} idempotent=true",
        hex_bytes(&first.worktree_subvolume_uuid),
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

fn run_service_smoke_helper(
    source_path: PathBuf,
    managed_path: PathBuf,
    spool_path: PathBuf,
    store_path: PathBuf,
    journal_path: PathBuf,
) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&spool_path)
        .map_err(|error| format!("create {}: {error}", spool_path.display()))?;
    let now_ns = current_time_ns()?;
    let metadata = ServiceMetadata::generate([0x71; 16], now_ns)
        .map_err(|error| format!("generate service metadata: {error}"))?;
    let store = Store::create(&store_path, &metadata)
        .map_err(|error| format!("create manager store: {error}"))?;
    let permissions =
        Permissions::new(PERMISSION_READ | PERMISSION_CUT | PERMISSION_TRIGGER | PERMISSION_RETAIN)
            .map_err(|error| error.to_string())?;
    let base_config = ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x71; 16])
        .allow_experimental_dirty_witness()
        .with_incremental_comparison_failpoint();
    let mut service = if let Some(socket) = env::var_os("BTRFS_AWACS_BROKER_SOCKET") {
        Service::new_external(store, base_config.with_broker_socket(socket.into()))
    } else {
        let journal = BrokerJournal::create(&journal_path)
            .map_err(|error| format!("create broker journal: {error}"))?;
        Service::new(store, journal, base_config)
    }
    .map_err(|error| error.to_string())?;
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
        .map_err(|error| error.to_string())?;
    let initial_snapshot_uuid: Vec<u8> = service
        .store()
        .connection()
        .query_row(
            "SELECT subvol_uuid FROM snapshots WHERE id = ?1",
            [initialized.snapshot_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("load initial snapshot UUID: {error}"))?;
    let initial_snapshot_uuid: [u8; 16] = initial_snapshot_uuid
        .try_into()
        .map_err(|_| "initial snapshot UUID has invalid length".to_owned())?;
    let initial_retention = service
        .store_mut()
        .create_retention_lease(
            initialized.watch_id,
            initialized.grant_id,
            initialized.snapshot_id,
            now_ns,
            now_ns + 300_000_000_000,
        )
        .map_err(|error| format!("retain direct-comparison source: {error}"))?;

    let changed_dir = source_path.join("service-dir");
    fs::create_dir(&changed_dir)
        .map_err(|error| format!("create {}: {error}", changed_dir.display()))?;
    let changed_file = changed_dir.join("changed");
    fs::write(&changed_file, b"changed after initial cut\n")
        .map_err(|error| format!("write {}: {error}", changed_file.display()))?;
    let xattr_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&changed_file)
        .map_err(|error| format!("open {} for xattr: {error}", changed_file.display()))?;
    let xattr_name = b"trusted.btrfs-awacs\0";
    let xattr_value = b"v2-security-state";
    // SAFETY: all pointers reference live byte arrays, the name is NUL
    // terminated, and fsetxattr does not retain them.
    if unsafe {
        libc::fsetxattr(
            xattr_file.as_raw_fd(),
            xattr_name.as_ptr().cast(),
            xattr_value.as_ptr().cast(),
            xattr_value.len(),
            0,
        )
    } != 0
    {
        return Err(format!(
            "set trusted xattr on {}: {}",
            changed_file.display(),
            io::Error::last_os_error()
        ));
    }
    fs::hard_link(&changed_file, source_path.join("service-hardlink"))
        .map_err(|error| format!("create service hardlink: {error}"))?;
    let published = service
        .changes(&ChangesOptions {
            watch_id: initialized.watch_id,
            authorization_id: initialized.grant_id,
            requester_uid: 0,
            requester_gid: 0,
            now_ns: current_time_ns()?,
        })
        .map_err(|error| error.to_string())?;
    let stored = service
        .store()
        .load_revision(published.revision_id)
        .map_err(|error| format!("load published revision: {error}"))?;
    let target_path: Vec<u8> = service
        .store()
        .connection()
        .query_row(
            "SELECT path FROM snapshots WHERE id = ?1",
            [published.snapshot_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("load published snapshot path: {error}"))?;
    let target_path = PathBuf::from(OsString::from_vec(target_path));
    let target = OpenedSubvolume::open(&target_path).map_err(|error| error.to_string())?;
    let expected = ExpectedSubvolume::from_observed(&target.filesystem, &target.subvolume);
    let independently_read =
        execute_full_index(&expected, target.as_fd()).map_err(|error| error.to_string())?;
    if stored != independently_read {
        return Err(format!(
            "incremental revision differs from full target index: stored={stored:?} independent={independently_read:?}"
        ));
    }
    let comparison_kind: String = service
        .store()
        .connection()
        .query_row(
            "SELECT comparison_kind FROM comparisons WHERE id = ?1",
            [published.comparison_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("load recovery comparison kind: {error}"))?;
    if comparison_kind != "full_fresh" {
        return Err(format!(
            "injected incremental failure published {comparison_kind:?} instead of full_fresh"
        ));
    }
    println!("full_fresh_recovery=ok");
    let target_snapshot_uuid: Vec<u8> = service
        .store()
        .connection()
        .query_row(
            "SELECT subvol_uuid FROM snapshots WHERE id = ?1",
            [published.snapshot_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("load direct-comparison target UUID: {error}"))?;
    let target_snapshot_uuid: [u8; 16] = target_snapshot_uuid
        .try_into()
        .map_err(|_| "target snapshot UUID has invalid length".to_owned())?;
    service
        .store_mut()
        .advance_replay_floor(initialized.watch_id, 1, current_time_ns()?, [0x79; 16])
        .map_err(|error| format!("reclaim adjacent history for direct comparison: {error}"))?;
    let historical = service
        .historical_changes(
            initialized.watch_id,
            initialized.grant_id,
            0,
            initial_snapshot_uuid,
            target_snapshot_uuid,
            current_time_ns()?,
        )
        .map_err(|error| format!("run direct historical comparison: {error}"))?;
    if historical.fresh_instance || historical.events.is_empty() {
        return Err("direct historical comparison returned no incremental witness".to_owned());
    }
    service
        .store_mut()
        .release_retention_lease(&initial_retention)
        .map_err(|error| format!("release direct-comparison source: {error}"))?;
    println!("direct_historical_comparison=ok");
    // SAFETY: xattr_file and the NUL-terminated name remain live for the call.
    if unsafe { libc::fremovexattr(xattr_file.as_raw_fd(), xattr_name.as_ptr().cast()) } != 0 {
        return Err(format!(
            "remove trusted xattr on {}: {}",
            changed_file.display(),
            io::Error::last_os_error()
        ));
    }
    drop(xattr_file);
    if let Some(socket) = env::var_os("BTRFS_AWACS_BROKER_SOCKET") {
        drop(service);
        let store = Store::open(&store_path)
            .map_err(|error| format!("reopen manager store for facade ABI probe: {error}"))?;
        let config = ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x71; 16])
            .allow_experimental_dirty_witness()
            .with_broker_socket(socket.into());
        service = Service::new_external(store, config)
            .map_err(|error| format!("restart service for facade ABI probe: {error}"))?;
    }
    let policy = service
        .provision_sanitized_worktree_policy(
            initialized.watch_id,
            initialized.grant_id,
            Path::new("/source"),
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    let reservation_nonce = [0x7a; 32];
    let reservation_path = Path::new("/source/worktrees/service-reservation");
    let mut reservation = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(reservation_path)
        .map_err(|error| format!("create {}: {error}", reservation_path.display()))?;
    reservation
        .write_all(&reservation_nonce)
        .map_err(|error| format!("write Worktree reservation: {error}"))?;
    reservation
        .sync_all()
        .map_err(|error| format!("sync Worktree reservation: {error}"))?;
    drop(reservation);
    let worktree = service
        .worktree(
            &policy,
            &WorktreeOptions {
                watch_id: initialized.watch_id,
                authorization_id: initialized.grant_id,
                destination_root: PathBuf::from("/source"),
                destination_parent: PathBuf::from("/source/worktrees"),
                destination_name: b"service-worktree".to_vec(),
                reservation_name: b"service-reservation".to_vec(),
                reservation_nonce,
                requester_uid: 0,
                requester_gid: 0,
                now_ns: current_time_ns()?,
            },
        )
        .map_err(|error| error.to_string())?;
    fs::write(worktree.path.join("writable-after-publish"), b"yes\n")
        .map_err(|error| format!("write published Worktree: {error}"))?;
    let gc_deleted = service
        .garbage_collect(current_time_ns()?, 8)
        .map_err(|error| error.to_string())?;
    if gc_deleted != 2
        || service
            .store()
            .load_revision(published.revision_id)
            .map_err(|error| format!("load revision after physical GC: {error}"))?
            != independently_read
    {
        return Err("physical GC did not delete exactly the unpinned old cuts".to_owned());
    }
    let mut facade = FacadeService::new(service);
    let worktree_seed_clock = facade
        .activate_proved_worktree(worktree.watch_id, worktree.grant_id, &worktree.path)
        .map_err(|error| error.to_string())?;
    let worktree_baseline = facade
        .query(
            worktree.watch_id,
            Some(&worktree_seed_clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    let worktree_changed_file = worktree.path.join("service-dir/changed");
    fs::write(&worktree_changed_file, b"worktree-only change\n")
        .map_err(|error| format!("write tracked Worktree change: {error}"))?;
    let worktree_incremental = facade
        .query(
            worktree.watch_id,
            Some(&worktree_baseline.clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    if worktree_incremental.projection.fresh_instance
        || worktree_incremental.projection.paths
            != [
                b"service-dir/changed".to_vec(),
                b"service-hardlink".to_vec(),
            ]
    {
        return Err(format!(
            "proved Worktree seed did not produce an exact first delta: {:?}",
            worktree_incremental.projection
        ));
    }
    facade
        .activate(initialized.watch_id, initialized.grant_id, &source_path)
        .map_err(|error| error.to_string())?;
    let fresh = facade
        .query(
            initialized.watch_id,
            None,
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    if !fresh.projection.fresh_instance || fresh.projection.paths != [b"/".to_vec()] {
        return Err("first facade query was not a fresh baseline".to_owned());
    }

    // The required correctness backstop is the snapshot delta, not inotify.
    // Exercise namespace mutations before the optional precision guard exists;
    // each endpoint-equal case must retain a directory dirty witness and force
    // jj to crawl rather than return an empty incremental result.
    let transient = source_path.join("snapshot-only-transient");
    fs::write(&transient, b"observably transient\n")
        .map_err(|error| format!("create snapshot-only transient: {error}"))?;
    fs::remove_file(&transient)
        .map_err(|error| format!("delete snapshot-only transient: {error}"))?;
    let snapshot_file_witness = facade
        .query(
            initialized.watch_id,
            Some(&fresh.clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    if !snapshot_file_witness.projection.fresh_instance
        || snapshot_file_witness.projection.paths != [b"/".to_vec()]
    {
        return Err(format!(
            "snapshot-only transient file lost its directory witness: {:?}",
            snapshot_file_witness.projection
        ));
    }

    let transient_tree = source_path.join("snapshot-only-tree");
    fs::create_dir(&transient_tree)
        .map_err(|error| format!("create snapshot-only transient tree: {error}"))?;
    fs::write(transient_tree.join("child"), b"transient subtree\n")
        .map_err(|error| format!("populate snapshot-only transient tree: {error}"))?;
    fs::remove_dir_all(&transient_tree)
        .map_err(|error| format!("delete snapshot-only transient tree: {error}"))?;
    let snapshot_tree_witness = facade
        .query(
            initialized.watch_id,
            Some(&snapshot_file_witness.clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    if !snapshot_tree_witness.projection.fresh_instance
        || snapshot_tree_witness.projection.paths != [b"/".to_vec()]
    {
        return Err(format!(
            "snapshot-only transient subtree lost its ancestor witness: {:?}",
            snapshot_tree_witness.projection
        ));
    }

    let nested_transient = changed_dir.join("mixed-transient");
    fs::write(&nested_transient, b"transient beside a retained create\n")
        .map_err(|error| format!("create mixed transient entry: {error}"))?;
    fs::remove_file(&nested_transient)
        .map_err(|error| format!("delete mixed transient entry: {error}"))?;
    fs::write(changed_dir.join("mixed-persistent"), b"persistent\n")
        .map_err(|error| format!("create mixed persistent entry: {error}"))?;
    let snapshot_mixed_witness = facade
        .query(
            initialized.watch_id,
            Some(&snapshot_tree_witness.clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    if !snapshot_mixed_witness.projection.fresh_instance
        || snapshot_mixed_witness.projection.paths != [b"/".to_vec()]
    {
        return Err(format!(
            "known nested create erased its directory dirty witness: {:?}",
            snapshot_mixed_witness.projection
        ));
    }

    let original = fs::read(&changed_file)
        .map_err(|error| format!("read hardlink witness fixture: {error}"))?;
    fs::write(&changed_file, b"temporary data state\n")
        .map_err(|error| format!("modify hardlink witness fixture: {error}"))?;
    fs::write(&changed_file, &original)
        .map_err(|error| format!("restore hardlink witness fixture: {error}"))?;
    let snapshot_data_witness = facade
        .query(
            initialized.watch_id,
            Some(&snapshot_mixed_witness.clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    require_hardlink_projection(&snapshot_data_witness, "modify/restore")?;

    let original_mode = fs::metadata(&changed_file)
        .map_err(|error| format!("stat metadata witness fixture: {error}"))?
        .permissions()
        .mode();
    fs::set_permissions(&changed_file, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("modify metadata witness fixture: {error}"))?;
    fs::set_permissions(
        &changed_file,
        fs::Permissions::from_mode(original_mode & 0o7777),
    )
    .map_err(|error| format!("restore metadata witness fixture: {error}"))?;
    let snapshot_metadata_witness = facade
        .query(
            initialized.watch_id,
            Some(&snapshot_data_witness.clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    require_hardlink_projection(&snapshot_metadata_witness, "metadata modify/restore")?;

    let mapped = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&changed_file)
        .map_err(|error| format!("open mmap witness fixture: {error}"))?;
    let length = original.len();
    if length == 0 {
        return Err("mmap witness fixture is empty".to_owned());
    }
    // SAFETY: `mapped` remains open, the requested range is within the file,
    // and the mapping is unmapped exactly once after both synchronous writes.
    let address = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            mapped.as_raw_fd(),
            0,
        )
    };
    if address == libc::MAP_FAILED {
        return Err(format!(
            "map mmap witness fixture: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: the successful mapping covers at least one writable byte.
    unsafe {
        let byte = address.cast::<u8>();
        *byte ^= 0x01;
        if libc::msync(address, length, libc::MS_SYNC) != 0 {
            let error = io::Error::last_os_error();
            libc::munmap(address, length);
            return Err(format!("sync modified mmap witness fixture: {error}"));
        }
        *byte ^= 0x01;
        if libc::msync(address, length, libc::MS_SYNC) != 0 {
            let error = io::Error::last_os_error();
            libc::munmap(address, length);
            return Err(format!("sync restored mmap witness fixture: {error}"));
        }
        if libc::munmap(address, length) != 0 {
            return Err(format!(
                "unmap mmap witness fixture: {}",
                io::Error::last_os_error()
            ));
        }
    }
    drop(mapped);
    let snapshot_mmap_witness = facade
        .query(
            initialized.watch_id,
            Some(&snapshot_metadata_witness.clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    require_hardlink_projection(&snapshot_mmap_witness, "writable mmap modify/restore")?;

    facade
        .activate_precision_guard(initialized.watch_id, &spool_path, current_time_ns()?)
        .map_err(|error| format!("activate precision guard: {error}"))?;
    let precision_fresh = facade
        .query(
            initialized.watch_id,
            None,
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    if !precision_fresh.projection.fresh_instance
        || precision_fresh.projection.paths != [b"/".to_vec()]
    {
        return Err("precision guard did not establish a fresh baseline".to_owned());
    }
    fs::write(&changed_file, b"data-only facade change\n")
        .map_err(|error| format!("write facade change: {error}"))?;
    let incremental = facade
        .query(
            initialized.watch_id,
            Some(&precision_fresh.clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    if incremental.projection.fresh_instance
        || incremental.projection.paths
            != [
                b"service-dir/changed".to_vec(),
                b"service-hardlink".to_vec(),
            ]
    {
        return Err(format!(
            "hardlink facade projection is incomplete: {:?}",
            incremental.projection
        ));
    }
    let transient = source_path.join("transient-after-clock");
    fs::write(&transient, b"observably transient\n")
        .map_err(|error| format!("create transient dirty-witness file: {error}"))?;
    fs::remove_file(&transient)
        .map_err(|error| format!("delete transient dirty-witness file: {error}"))?;
    let transient_result = facade
        .query(
            initialized.watch_id,
            Some(&incremental.clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| error.to_string())?;
    if transient_result.projection.fresh_instance
        || transient_result.projection.paths != [b"transient-after-clock".to_vec()]
    {
        return Err(format!(
            "precision journal did not preserve the transient path: {:?}",
            transient_result.projection
        ));
    }

    // Exercise the actual client encodings against the same durable facade,
    // not only the semantic projection API. Re-registration deliberately
    // rotates the clock epoch, just as a namespace-daemon handoff does.
    let mut watchman = WatchmanEndpoint::default();
    watchman.enable_fixed_jj_trigger();
    watchman
        .register(
            &mut facade,
            &source_path,
            initialized.watch_id,
            initialized.grant_id,
            0,
            0,
        )
        .map_err(|error| error.to_string())?;
    let watch_root = fs::canonicalize(&source_path)
        .map_err(|error| format!("canonicalize Watchman test root: {error}"))?
        .as_os_str()
        .as_bytes()
        .to_vec();
    let watch_project = BserValue::Array(vec![
        BserValue::Bytes(b"watch-project".to_vec()),
        BserValue::Bytes(watch_root.clone()),
    ]);
    let request =
        encode_frame(&watch_project, BserLimits::default()).map_err(|error| error.to_string())?;
    let (watchman_client, watchman_server) =
        UnixStream::pair().map_err(|error| format!("create Watchman socket pair: {error}"))?;
    let mut client_transport = CredentialedStream::new(watchman_client)
        .map_err(|error| format!("arm Watchman client transport: {error}"))?;
    let mut transport = CredentialedStream::new(watchman_server)
        .map_err(|error| format!("arm Watchman credential transport: {error}"))?;
    client_transport
        .send_frame(&request, BserLimits::default())
        .map_err(|error| format!("send Watchman request: {error}"))?;
    transport
        .serve_one_frame(
            &watchman,
            &mut facade,
            current_time_ns()?,
            BserLimits::default(),
        )
        .map_err(|error| format!("serve authenticated Watchman frame: {error}"))?;
    let wire_response = client_transport
        .receive_frame(BserLimits::default())
        .map_err(|error| format!("receive Watchman response: {error}"))?
        .bytes;
    let BserValue::Object(watch_response) =
        decode_frame(&wire_response, BserLimits::default()).map_err(|error| error.to_string())?
    else {
        return Err("Watchman watch-project response is not an object".to_owned());
    };
    if watch_response.get(b"watch".as_slice()) != Some(&BserValue::Bytes(watch_root.clone())) {
        return Err("Watchman watch-project response did not preserve the root".to_owned());
    }
    let worktree_watch_root = fs::canonicalize(&worktree.path)
        .map_err(|error| format!("canonicalize Worktree Watchman root: {error}"))?
        .as_os_str()
        .as_bytes()
        .to_vec();
    let worktree_watch_project = BserValue::Array(vec![
        BserValue::Bytes(b"watch-project".to_vec()),
        BserValue::Bytes(worktree_watch_root.clone()),
    ]);
    let request = encode_frame(&worktree_watch_project, BserLimits::default())
        .map_err(|error| error.to_string())?;
    client_transport
        .send_frame(&request, BserLimits::default())
        .map_err(|error| format!("send dynamic Worktree watch request: {error}"))?;
    transport
        .serve_one_frame(
            &watchman,
            &mut facade,
            current_time_ns()?,
            BserLimits::default(),
        )
        .map_err(|error| format!("serve dynamic Worktree watch frame: {error}"))?;
    let response = client_transport
        .receive_frame(BserLimits::default())
        .map_err(|error| format!("receive dynamic Worktree watch response: {error}"))?;
    let BserValue::Object(response) =
        decode_frame(&response.bytes, BserLimits::default()).map_err(|error| error.to_string())?
    else {
        return Err("dynamic Worktree watch response is not an object".to_owned());
    };
    if response.get(b"watch".as_slice()) != Some(&BserValue::Bytes(worktree_watch_root.clone())) {
        return Err("dynamic Worktree watch-project did not preserve its root".to_owned());
    }

    let jj_expression = BserValue::Array(vec![
        BserValue::Bytes(b"not".to_vec()),
        BserValue::Array(vec![
            BserValue::Bytes(b"anyof".to_vec()),
            BserValue::Array(vec![
                BserValue::Bytes(b"name".to_vec()),
                BserValue::Array(vec![
                    BserValue::Bytes(b".git".to_vec()),
                    BserValue::Bytes(b".jj".to_vec()),
                ]),
                BserValue::Bytes(b"wholename".to_vec()),
            ]),
            BserValue::Array(vec![
                BserValue::Bytes(b"dirname".to_vec()),
                BserValue::Bytes(b".git".to_vec()),
            ]),
            BserValue::Array(vec![
                BserValue::Bytes(b"dirname".to_vec()),
                BserValue::Bytes(b".jj".to_vec()),
            ]),
        ]),
    ]);
    let trigger_definition = BserValue::Object(BTreeMap::from([
        (
            b"command".to_vec(),
            BserValue::Array(
                [b"jj".as_slice(), b"--quiet", b"util", b"snapshot"]
                    .into_iter()
                    .map(|part| BserValue::Bytes(part.to_vec()))
                    .collect(),
            ),
        ),
        (b"expression".to_vec(), jj_expression.clone()),
        (
            b"name".to_vec(),
            BserValue::Bytes(b"jj-background-monitor".to_vec()),
        ),
        (b"stderr".to_vec(), BserValue::Bytes(b">/dev/null".to_vec())),
        (b"stdout".to_vec(), BserValue::Bytes(b">/dev/null".to_vec())),
    ]));
    let trigger_register = BserValue::Array(vec![
        BserValue::Bytes(b"trigger".to_vec()),
        BserValue::Bytes(watch_root.clone()),
        trigger_definition,
    ]);
    let trigger_response = watchman
        .handle(&mut facade, &trigger_register, 0, 0, current_time_ns()?)
        .map_err(|error| error.to_string())?;
    let BserValue::Object(trigger_response) = trigger_response else {
        return Err("Watchman trigger response is not an object".to_owned());
    };
    if trigger_response.get(b"disposition".as_slice())
        != Some(&BserValue::Bytes(b"created".to_vec()))
    {
        return Err(format!(
            "Watchman trigger was not created: {trigger_response:?}"
        ));
    }
    let trigger_list = BserValue::Array(vec![
        BserValue::Bytes(b"trigger-list".to_vec()),
        BserValue::Bytes(watch_root.clone()),
    ]);
    let BserValue::Object(trigger_list_response) = watchman
        .handle(&mut facade, &trigger_list, 0, 0, current_time_ns()?)
        .map_err(|error| error.to_string())?
    else {
        return Err("Watchman trigger-list response is not an object".to_owned());
    };
    if !matches!(
        trigger_list_response.get(b"triggers".as_slice()),
        Some(BserValue::Array(triggers)) if triggers.len() == 1
    ) {
        return Err(format!(
            "Watchman trigger-list omitted the trigger: {trigger_list_response:?}"
        ));
    }
    let trigger_delete = BserValue::Array(vec![
        BserValue::Bytes(b"trigger-del".to_vec()),
        BserValue::Bytes(watch_root.clone()),
        BserValue::Bytes(b"jj-background-monitor".to_vec()),
    ]);
    watchman
        .handle(&mut facade, &trigger_delete, 0, 0, current_time_ns()?)
        .map_err(|error| error.to_string())?;
    let query = BserValue::Array(vec![
        BserValue::Bytes(b"query".to_vec()),
        BserValue::Bytes(watch_root),
        BserValue::Object(BTreeMap::from([
            (b"expression".to_vec(), jj_expression),
            (
                b"fields".to_vec(),
                BserValue::Array(vec![BserValue::Bytes(b"name".to_vec())]),
            ),
        ])),
    ]);
    let request = encode_frame(&query, BserLimits::default()).map_err(|error| error.to_string())?;
    let response = watchman
        .handle_frame(
            &mut facade,
            &request,
            0,
            0,
            current_time_ns()?,
            BserLimits::default(),
        )
        .map_err(|error| error.to_string())?;
    let BserValue::Object(query_response) =
        decode_frame(&response, BserLimits::default()).map_err(|error| error.to_string())?
    else {
        return Err("Watchman query response is not an object".to_owned());
    };
    if query_response.get(b"is_fresh_instance".as_slice()) != Some(&BserValue::Bool(true))
        || query_response.get(b"files".as_slice())
            != Some(&BserValue::Array(vec![BserValue::Bytes(b"/".to_vec())]))
    {
        return Err(format!(
            "Watchman initial query was not a fresh sentinel: {query_response:?}"
        ));
    }
    let BserValue::Bytes(git_old_clock) = query_response
        .get(b"clock".as_slice())
        .ok_or_else(|| "Watchman query omitted its clock".to_owned())?
    else {
        return Err("Watchman query clock is not a byte string".to_owned());
    };
    fs::write(&changed_file, b"data-only Git hook change\n")
        .map_err(|error| format!("write Git hook change: {error}"))?;
    let git_socket_path = spool_path.join("git-hook-watchman.sock");
    let listener = UnixListener::bind(&git_socket_path)
        .map_err(|error| format!("bind Git hook test socket: {error}"))?;
    let socket_git_response = std::thread::scope(|scope| -> Result<Vec<u8>, String> {
        let server = scope.spawn(|| -> Result<(), String> {
            let (stream, _) = listener
                .accept()
                .map_err(|error| format!("accept Git hook test client: {error}"))?;
            let mut stream = CredentialedStream::new(stream)
                .map_err(|error| format!("arm Git hook server stream: {error}"))?;
            stream
                .serve_one_frame(
                    &watchman,
                    &mut facade,
                    current_time_ns()?,
                    BserLimits::default(),
                )
                .map_err(|error| format!("serve Git hook query: {error}"))
        });
        let response = run_hook_over_socket(
            &git_socket_path,
            &source_path,
            &[b"2".to_vec(), git_old_clock.clone()],
        )
        .map_err(|error| error.to_string())?;
        server
            .join()
            .map_err(|_| "Git hook test server panicked".to_owned())??;
        Ok(response)
    })?;
    fs::remove_file(&git_socket_path)
        .map_err(|error| format!("remove Git hook test socket: {error}"))?;
    let socket_git_paths: Vec<&[u8]> = socket_git_response
        .split(|byte| *byte == 0)
        .skip(1)
        .filter(|field| !field.is_empty())
        .collect();
    if socket_git_paths
        != [
            b"service-dir/changed".as_slice(),
            b"service-hardlink".as_slice(),
        ]
    {
        return Err(format!(
            "socket Git hook-v2 response is incomplete: {socket_git_paths:?}"
        ));
    }
    let git_response = run_hook_v2(
        &mut facade,
        initialized.watch_id,
        &[b"2".to_vec(), git_old_clock.clone()],
        0,
        0,
        current_time_ns()?,
    )
    .map_err(|error| error.to_string())?;
    let mut fields = git_response.split(|byte| *byte == 0);
    let git_clock = fields.next().unwrap_or_default();
    let git_paths: Vec<&[u8]> = fields.filter(|field| !field.is_empty()).collect();
    if !git_clock.starts_with(b"c:btrfs-awacs:1:")
        || git_paths
            != [
                b"service-dir/changed".as_slice(),
                b"service-hardlink".as_slice(),
            ]
    {
        return Err(format!(
            "Git hook-v2 response is incomplete: clock={:?} paths={git_paths:?}",
            String::from_utf8_lossy(git_clock)
        ));
    }
    let active_query_leases: i64 = facade
        .service()
        .store()
        .connection()
        .query_row(
            "SELECT count(*) FROM query_leases WHERE state = 'active'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("inspect response-fence leases: {error}"))?;
    if active_query_leases != 0 {
        return Err(format!(
            "Watchman response path leaked {active_query_leases} active query leases"
        ));
    }
    println!(
        "watch={} sequence={} events={} objects={} refs={} worktree={} gc_deleted={} worktree_seed_fresh={} worktree_incremental={} facade_sequence={} facade_aliases={} facade_restart_probe=true snapshot_only_dirty_witness=true precision_transient=true watchman_fresh=true dynamic_worktree_watch=true git_aliases={} response_fence_released=true incremental_equals_full=true",
        hex_bytes(&initialized.watch_id),
        published.sequence,
        published.events.len(),
        stored.objects.len(),
        stored.references.len(),
        hex_bytes(&worktree.subvol_uuid),
        gc_deleted,
        worktree_baseline.projection.fresh_instance,
        worktree_incremental.sequence,
        incremental.sequence,
        incremental.projection.paths.len(),
        git_paths.len(),
    );
    Ok(())
}

fn require_hardlink_projection(
    result: &btrfs_awacs::facade::QueryResult,
    operation: &str,
) -> Result<(), String> {
    let expected = [
        b"service-dir/changed".to_vec(),
        b"service-hardlink".to_vec(),
    ];
    if result.projection.fresh_instance || result.projection.paths != expected {
        return Err(format!(
            "snapshot-only {operation} did not report every hardlink alias: {:?}",
            result.projection
        ));
    }
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

fn run_service_recovery_smoke_helper(
    source_path: PathBuf,
    managed_path: PathBuf,
    spool_path: PathBuf,
    store_path: PathBuf,
) -> Result<(), String> {
    let broker_socket = PathBuf::from(
        env::var_os("BTRFS_AWACS_BROKER_SOCKET")
            .ok_or_else(|| "recovery smoke requires BTRFS_AWACS_BROKER_SOCKET".to_owned())?,
    );
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&spool_path)
        .map_err(|error| format!("create recovery spool: {error}"))?;
    let now_ns = current_time_ns()?;
    let metadata = ServiceMetadata::generate([0x72; 16], now_ns)
        .map_err(|error| format!("generate recovery metadata: {error}"))?;
    let store = Store::create(&store_path, &metadata)
        .map_err(|error| format!("create recovery store: {error}"))?;
    let failing_config = ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x72; 16])
        .with_broker_socket(broker_socket.clone())
        .with_initialize_snapshot_failpoint();
    let mut service =
        Service::new_external(store, failing_config).map_err(|error| error.to_string())?;
    let error = service
        .initialize(
            &source_path,
            &InitializeOptions {
                principal: Principal::Uid(0),
                permissions: Permissions::new(PERMISSION_READ | PERMISSION_CUT)
                    .map_err(|error| error.to_string())?,
                requester_uid: 0,
                requester_gid: 0,
                now_ns,
            },
        )
        .expect_err("recovery failpoint must interrupt initialization");
    if !error.to_string().contains("injected failure") {
        return Err(format!("unexpected recovery failpoint error: {error}"));
    }
    drop(service);

    let store =
        Store::open(&store_path).map_err(|error| format!("reopen recovery store: {error}"))?;
    let recovered = Service::new_external(
        store,
        ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x72; 16])
            .with_broker_socket(broker_socket.clone()),
    )
    .map_err(|error| format!("recover interrupted initialize: {error}"))?;
    let state: (i64, i64, i64) = recovered
        .store()
        .connection()
        .query_row(
            "SELECT \
                 (SELECT count(*) FROM watches WHERE state = 'active'), \
                 (SELECT count(*) FROM operations WHERE kind = 'initialize' AND state = 'done'), \
                 (SELECT count(*) FROM revisions WHERE state = 'ready')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| format!("inspect recovered initialize: {error}"))?;
    if state != (1, 1, 1) {
        return Err(format!("initialize recovery did not converge: {state:?}"));
    }
    println!("initialize_recovery=ok");

    let (watch_id, grant_id): (Vec<u8>, Vec<u8>) = recovered
        .store()
        .connection()
        .query_row(
            "SELECT w.id, g.id FROM watches w JOIN watch_grants g ON g.watch_id = w.id \
             WHERE w.state = 'active' AND g.state = 'active'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| format!("load recovered watch identity: {error}"))?;
    let watch_id: [u8; 16] = watch_id
        .try_into()
        .map_err(|_| "recovered watch ID has invalid length".to_owned())?;
    let grant_id: [u8; 16] = grant_id
        .try_into()
        .map_err(|_| "recovered grant ID has invalid length".to_owned())?;
    drop(recovered);

    fs::write(source_path.join("cut-recovery-change"), b"recover me\n")
        .map_err(|error| format!("write cut recovery change: {error}"))?;
    let store =
        Store::open(&store_path).map_err(|error| format!("reopen store before cut: {error}"))?;
    let mut failing_cut = Service::new_external(
        store,
        ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x72; 16])
            .with_broker_socket(broker_socket.clone())
            .with_cut_snapshot_failpoint(),
    )
    .map_err(|error| format!("open service before interrupted cut: {error}"))?;
    let error = failing_cut
        .changes(&ChangesOptions {
            watch_id,
            authorization_id: grant_id,
            requester_uid: 0,
            requester_gid: 0,
            now_ns: current_time_ns()?,
        })
        .expect_err("cut recovery failpoint must interrupt the cut");
    if !error.to_string().contains("injected failure") {
        return Err(format!("unexpected cut recovery failpoint error: {error}"));
    }
    drop(failing_cut);

    let store = Store::open(&store_path)
        .map_err(|error| format!("reopen store for cut recovery: {error}"))?;
    let recovered = Service::new_external(
        store,
        ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x72; 16])
            .with_broker_socket(broker_socket.clone()),
    )
    .map_err(|error| format!("recover interrupted cut: {error}"))?;
    let (last_cut_seq, indexed_seq, operation_count, revision_id): (i64, i64, i64, i64) = recovered
        .store()
        .connection()
        .query_row(
            "SELECT w.last_cut_seq, w.indexed_seq, \
                        (SELECT count(*) FROM operations \
                          WHERE kind = 'cut' AND state = 'done'), \
                        w.indexed_revision_id \
                   FROM watches w WHERE w.id = ?1",
            [watch_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|error| format!("inspect recovered cut: {error}"))?;
    if (last_cut_seq, indexed_seq, operation_count) != (1, 1, 1) {
        return Err(format!(
            "cut recovery did not converge: last={last_cut_seq} indexed={indexed_seq} operations={operation_count}"
        ));
    }
    let index = recovered
        .store()
        .load_revision(revision_id)
        .map_err(|error| format!("load recovered cut revision: {error}"))?;
    if !index
        .references
        .iter()
        .any(|reference| reference.name == b"cut-recovery-change")
    {
        return Err("recovered cut index omits the changed path".to_owned());
    }
    println!("cut_recovery=ok");

    drop(recovered);
    fs::write(
        source_path.join("staged-recovery-change"),
        b"reuse staged delta\n",
    )
    .map_err(|error| format!("write staged recovery change: {error}"))?;
    let store = Store::open(&store_path)
        .map_err(|error| format!("reopen store before staged cut: {error}"))?;
    let mut staged_cut = Service::new_external(
        store,
        ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x72; 16])
            .with_broker_socket(broker_socket.clone())
            .with_manifest_stage_failpoint(),
    )
    .map_err(|error| format!("open service before staged cut: {error}"))?;
    let error = staged_cut
        .changes(&ChangesOptions {
            watch_id,
            authorization_id: grant_id,
            requester_uid: 0,
            requester_gid: 0,
            now_ns: current_time_ns()?,
        })
        .expect_err("manifest-stage failpoint must interrupt the cut");
    if !error.to_string().contains("durable manifest staging") {
        return Err(format!(
            "unexpected manifest-stage failpoint error: {error}"
        ));
    }
    drop(staged_cut);
    let staged_count = fs::read_dir(&spool_path)
        .map_err(|error| format!("enumerate durable stages: {error}"))?
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            name.as_bytes().starts_with(b"manifest-") && name.as_bytes().ends_with(b".part")
        })
        .count();
    if staged_count != 1 {
        return Err(format!(
            "manifest-stage failure retained {staged_count} stages instead of one"
        ));
    }
    let store = Store::open(&store_path)
        .map_err(|error| format!("reopen store for staged recovery: {error}"))?;
    let mut recovered = Service::new_external(
        store,
        ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x72; 16])
            .with_broker_socket(broker_socket.clone()),
    )
    .map_err(|error| format!("recover durable manifest stage: {error}"))?;
    let (staged_sequence, staged_revision): (i64, i64) = recovered
        .store()
        .connection()
        .query_row(
            "SELECT indexed_seq, indexed_revision_id FROM watches WHERE id = ?1",
            [watch_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| format!("inspect staged cut recovery: {error}"))?;
    let staged_index = recovered
        .store()
        .load_revision(staged_revision)
        .map_err(|error| format!("load staged cut revision: {error}"))?;
    if staged_sequence != 2
        || !staged_index
            .references
            .iter()
            .any(|reference| reference.name == b"staged-recovery-change")
        || fs::read_dir(&spool_path)
            .map_err(|error| format!("inspect consumed manifest stage: {error}"))?
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().as_bytes().starts_with(b"manifest-"))
    {
        return Err("durable manifest stage recovery did not converge".to_owned());
    }
    println!("manifest_stage_recovery=ok");

    let destination_root = PathBuf::from("/source");
    let destination_parent = PathBuf::from("/source/worktrees");
    let destination_name = b"recovered-worktree".to_vec();
    let reservation_name = b"recovered-worktree.reservation".to_vec();
    let reservation_nonce = [0x51; 32];
    let policy = recovered
        .provision_sanitized_worktree_policy(
            watch_id,
            grant_id,
            &destination_root,
            current_time_ns()?,
        )
        .map_err(|error| format!("provision recovery Worktree policy: {error}"))?;
    let reservation_path =
        destination_parent.join(std::ffi::OsString::from_vec(reservation_name.clone()));
    let mut reservation_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&reservation_path)
        .map_err(|error| format!("create recovery Worktree reservation: {error}"))?;
    reservation_file
        .write_all(&reservation_nonce)
        .map_err(|error| format!("write recovery Worktree reservation: {error}"))?;
    reservation_file
        .sync_all()
        .map_err(|error| format!("sync recovery Worktree reservation: {error}"))?;
    drop(reservation_file);

    drop(recovered);
    let store = Store::open(&store_path)
        .map_err(|error| format!("reopen store before interrupted Worktree create: {error}"))?;
    let mut failing_worktree = Service::new_external(
        store,
        ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x72; 16])
            .with_broker_socket(broker_socket.clone())
            .with_worktree_create_failpoint(),
    )
    .map_err(|error| format!("open service before interrupted Worktree create: {error}"))?;
    let worktree_options = WorktreeOptions {
        watch_id,
        authorization_id: grant_id,
        destination_root: destination_root.clone(),
        destination_parent: destination_parent.clone(),
        destination_name: destination_name.clone(),
        reservation_name: reservation_name.clone(),
        reservation_nonce,
        requester_uid: 0,
        requester_gid: 0,
        now_ns: current_time_ns()?,
    };
    let error = failing_worktree
        .worktree(&policy, &worktree_options)
        .expect_err("Worktree-create failpoint must interrupt publication");
    if !error.to_string().contains("injected failure") {
        return Err(format!(
            "unexpected Worktree-create failpoint error: {error}"
        ));
    }
    drop(failing_worktree);

    let store = Store::open(&store_path)
        .map_err(|error| format!("reopen store for Worktree-create recovery: {error}"))?;
    let error = Service::new_external(
        store,
        ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x72; 16])
            .with_broker_socket(broker_socket.clone())
            .with_worktree_publish_failpoint(),
    )
    .expect_err("Worktree-publish failpoint must interrupt recovery");
    if !error.to_string().contains("injected failure") {
        return Err(format!(
            "unexpected Worktree-publish failpoint error: {error}"
        ));
    }

    let store = Store::open(&store_path)
        .map_err(|error| format!("reopen store for Worktree-publish recovery: {error}"))?;
    let recovered = Service::new_external(
        store,
        ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x72; 16])
            .with_broker_socket(broker_socket.clone()),
    )
    .map_err(|error| format!("recover interrupted Worktree publication: {error}"))?;
    let worktree_path = destination_parent.join(std::ffi::OsString::from_vec(destination_name));
    if !worktree_path.is_dir() || reservation_path.exists() {
        return Err("recovered Worktree namespace did not converge".to_owned());
    }
    fs::write(worktree_path.join("writable-after-recovery"), b"ok\n")
        .map_err(|error| format!("write recovered Worktree: {error}"))?;
    let worktree_count: i64 = recovered
        .store()
        .connection()
        .query_row(
            "SELECT count(*) FROM operations WHERE kind = 'worktree' AND state = 'done'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("inspect recovered Worktree: {error}"))?;
    if worktree_count != 1 {
        return Err(format!(
            "Worktree recovery did not converge: {worktree_count}"
        ));
    }
    println!("worktree_recovery=ok");

    drop(recovered);
    let store = Store::open(&store_path)
        .map_err(|error| format!("reopen store before interrupted GC: {error}"))?;
    let mut failing_gc = Service::new_external(
        store,
        ServiceConfig::new(managed_path.clone(), spool_path.clone(), [0x72; 16])
            .with_broker_socket(broker_socket.clone())
            .with_snapshot_delete_failpoint(),
    )
    .map_err(|error| format!("open service before interrupted GC: {error}"))?;
    let error = failing_gc
        .garbage_collect(current_time_ns()?, 1)
        .expect_err("snapshot-delete failpoint must interrupt GC");
    if !error.to_string().contains("injected failure") {
        return Err(format!(
            "unexpected snapshot-delete failpoint error: {error}"
        ));
    }
    drop(failing_gc);

    let store = Store::open(&store_path)
        .map_err(|error| format!("reopen store for GC recovery: {error}"))?;
    let recovered = Service::new_external(
        store,
        ServiceConfig::new(managed_path, spool_path, [0x72; 16]).with_broker_socket(broker_socket),
    )
    .map_err(|error| format!("recover interrupted snapshot deletion: {error}"))?;
    let state: (i64, i64) = recovered
        .store()
        .connection()
        .query_row(
            "SELECT \
                 (SELECT count(*) FROM snapshot_delete_operations WHERE state = 'done'), \
                 (SELECT count(*) FROM snapshots WHERE physical_state = 'deleted')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| format!("inspect recovered snapshot deletion: {error}"))?;
    if state != (1, 1) {
        return Err(format!(
            "snapshot-delete recovery did not converge: {state:?}"
        ));
    }
    println!("snapshot_delete_recovery=ok");
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

#[derive(Clone)]
struct DaemonTriggerConfig {
    command: TriggerCommandConfig,
    interval: Duration,
}

fn configured_trigger_runner(
    socket_path: &Path,
    uid: u32,
    gid: u32,
) -> Result<Option<DaemonTriggerConfig>, String> {
    let Some(jj) = env::var_os("BTRFS_AWACS_JJ") else {
        return Ok(None);
    };
    if uid == 0 {
        return Err("BTRFS_AWACS_JJ cannot enable a trigger runner in a root daemon".to_owned());
    }
    let jj = fs::canonicalize(PathBuf::from(jj))
        .map_err(|error| format!("canonicalize BTRFS_AWACS_JJ: {error}"))?;
    let jj_metadata = fs::metadata(&jj)
        .map_err(|error| format!("stat configured jj executable {}: {error}", jj.display()))?;
    if !jj.is_absolute() || !jj_metadata.is_file() || jj_metadata.permissions().mode() & 0o111 == 0
    {
        return Err("BTRFS_AWACS_JJ must resolve to an executable regular file".to_owned());
    }
    let home = PathBuf::from(
        env::var_os("HOME")
            .ok_or_else(|| "HOME is required when BTRFS_AWACS_JJ is configured".to_owned())?,
    );
    if !home.is_absolute() || !socket_path.is_absolute() {
        return Err("trigger HOME and daemon socket paths must be absolute".to_owned());
    }
    let interval_ms = match env::var("BTRFS_AWACS_TRIGGER_INTERVAL_MS") {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| "BTRFS_AWACS_TRIGGER_INTERVAL_MS must be an integer".to_owned())?,
        Err(env::VarError::NotPresent) => 1_000,
        Err(error) => {
            return Err(format!("read BTRFS_AWACS_TRIGGER_INTERVAL_MS: {error}"));
        }
    };
    if !(10..=3_600_000).contains(&interval_ms) {
        return Err("BTRFS_AWACS_TRIGGER_INTERVAL_MS must be between 10 and 3600000".to_owned());
    }
    Ok(Some(DaemonTriggerConfig {
        command: TriggerCommandConfig {
            jj_executable: jj,
            daemon_socket: socket_path.to_path_buf(),
            home,
            requester_uid: uid,
            requester_gid: gid,
            run_owner: *Uuid::new_v4().as_bytes(),
            lease_ns: 300_000_000_000,
        },
        interval: Duration::from_millis(interval_ms),
    }))
}

fn spawn_trigger_scheduler(
    facade: std::sync::Arc<std::sync::Mutex<FacadeService>>,
    config: DaemonTriggerConfig,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("btrfs-awacs-jj-trigger".to_owned())
        .spawn(move || loop {
            let (watch_ids, readiness_fds) = match facade.lock() {
                Ok(facade) => match facade
                    .service()
                    .store()
                    .active_fixed_jj_trigger_watches(config.command.requester_uid)
                {
                    Ok(watches) => match facade.precision_readiness_fds(&watches) {
                        Ok(fds) => (watches, fds),
                        Err(error) => {
                            eprintln!("btrfs-awacs trigger scheduler: precision poll: {error}");
                            (watches, Vec::new())
                        }
                    },
                    Err(error) => {
                        eprintln!("btrfs-awacs trigger scheduler: list watches: {error}");
                        std::thread::sleep(config.interval);
                        continue;
                    }
                },
                Err(_) => return,
            };
            if let Err(error) = wait_for_precision_or_interval(&readiness_fds, config.interval) {
                eprintln!("btrfs-awacs trigger scheduler: wait: {error}");
                std::thread::sleep(config.interval);
            }
            for watch_id in watch_ids {
                let now_ns = match current_time_ns() {
                    Ok(now_ns) => now_ns,
                    Err(error) => {
                        eprintln!("btrfs-awacs trigger scheduler: {error}");
                        continue;
                    }
                };
                let pending = match facade.lock() {
                    Ok(mut facade) => facade.begin_concurrent_query(
                        watch_id,
                        None,
                        ClientFlavor::Jj,
                        config.command.requester_uid,
                        config.command.requester_gid,
                        now_ns,
                    ),
                    Err(_) => return,
                };
                let pending = match pending {
                    Ok(pending) => pending,
                    Err(error) => {
                        eprintln!("btrfs-awacs trigger scheduler: admit periodic cut: {error}");
                        continue;
                    }
                };
                let completed = match pending.execute() {
                    Ok(completed) => completed,
                    Err(error) => {
                        eprintln!("btrfs-awacs trigger scheduler: run periodic cut: {error}");
                        continue;
                    }
                };
                let result = match facade.lock() {
                    Ok(mut facade) => facade
                        .finish_concurrent_query(completed)
                        .and_then(|prepared| facade.finish_query_response(prepared)),
                    Err(_) => return,
                };
                if let Err(error) = result {
                    eprintln!("btrfs-awacs trigger scheduler: finalize periodic cut: {error}");
                }
            }

            // Claim under the facade lock, execute without it, then finish
            // under the original durable run fence. This avoids deadlocking
            // when `jj util snapshot` connects back to this daemon.
            let now_ns = match current_time_ns() {
                Ok(now_ns) => now_ns,
                Err(error) => {
                    eprintln!("btrfs-awacs trigger scheduler: {error}");
                    continue;
                }
            };
            let claimed = match facade.lock() {
                Ok(mut facade) => claim_one_pending(&mut facade, &config.command, now_ns),
                Err(_) => return,
            };
            let claimed = match claimed {
                Ok(Some(claimed)) => claimed,
                Ok(None) => continue,
                Err(error) => {
                    eprintln!("btrfs-awacs trigger scheduler: claim: {error}");
                    continue;
                }
            };
            let execution = execute_claimed(&config.command, &claimed);
            let outcome = match facade.lock() {
                Ok(mut facade) => finish_claimed(&mut facade, claimed, execution),
                Err(_) => return,
            };
            match outcome {
                Ok(outcome) if !outcome.succeeded => eprintln!(
                    "btrfs-awacs trigger scheduler: jj exited {:?}",
                    outcome.exit_code
                ),
                Ok(_) => {}
                Err(error) => eprintln!("btrfs-awacs trigger scheduler: finish: {error}"),
            }
        })
        .map(|_| ())
        .map_err(|error| format!("start jj trigger scheduler: {error}"))
}

fn wait_for_precision_or_interval(fds: &[OwnedFd], interval: Duration) -> Result<(), String> {
    if fds.is_empty() {
        std::thread::sleep(interval);
        return Ok(());
    }
    let timeout = i32::try_from(interval.as_millis())
        .map_err(|_| "trigger polling interval exceeds poll(2) range".to_owned())?;
    let mut descriptors: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // SAFETY: the vector is initialized for its full length and remains alive
    // for the bounded poll call.
    let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, timeout) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(());
        }
        return Err(format!("poll precision descriptors: {error}"));
    }
    if descriptors.iter().any(|descriptor| {
        descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
    }) {
        return Err("precision readiness descriptor failed".to_owned());
    }
    Ok(())
}

fn run_watchman_server(arguments: WatchmanServeArgs) -> Result<(), String> {
    let WatchmanServeArgs {
        socket: socket_path,
        root,
        managed_dir: managed,
        spool_dir: spool,
        manager_db: store_path,
        broker_socket,
        watch_id: watch_argument,
        grant_id: grant_argument,
    } = arguments;
    let runtime = socket_path
        .parent()
        .ok_or_else(|| "Watchman socket has no parent directory".to_owned())?;
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
            "refusing to replace existing Watchman socket {}",
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
    let mut config = ServiceConfig::new(managed, spool, boot_id).with_broker_socket(broker_socket);
    if env::var_os("BTRFS_AWACS_EXPERIMENTAL_DIRTY_WITNESS").as_deref() == Some(OsStr::new("1")) {
        config = config.allow_experimental_dirty_witness();
    }
    let mut service = Service::new_external(store, config).map_err(|error| error.to_string())?;
    let automatic = watch_argument == OsStr::new("auto") && grant_argument == OsStr::new("auto");
    if (watch_argument == OsStr::new("auto")) != (grant_argument == OsStr::new("auto")) {
        return Err(
            "watch-id and grant-id must either both be auto or both be explicit".to_owned(),
        );
    }
    let (watch_id, grant_id) = if automatic {
        let canonical_root = fs::canonicalize(&root)
            .map_err(|error| format!("canonicalize automatic watch root: {error}"))?;
        let existing = service
            .store()
            .active_uid_watch_at_path(
                canonical_root.as_os_str().as_bytes(),
                uid,
                PERMISSION_READ | PERMISSION_CUT,
            )
            .map_err(|error| format!("resolve automatic watch registration: {error}"))?;
        match existing {
            Some(existing) => existing,
            None => {
                let initialized = service
                    .initialize(
                        &canonical_root,
                        &InitializeOptions {
                            principal: Principal::Uid(u64::from(uid)),
                            permissions: Permissions::new(
                                PERMISSION_READ | PERMISSION_CUT | PERMISSION_TRIGGER,
                            )
                            .map_err(|error| error.to_string())?,
                            requester_uid: uid,
                            requester_gid: gid,
                            now_ns: current_time_ns()?,
                        },
                    )
                    .map_err(|error| format!("initialize automatic watch: {error}"))?;
                (initialized.watch_id, initialized.grant_id)
            }
        }
    } else {
        (
            parse_hex_id(&watch_argument)?,
            parse_hex_id(&grant_argument)?,
        )
    };
    let mut facade = FacadeService::new(service);
    let mut endpoint = WatchmanEndpoint::default();
    let trigger_config = configured_trigger_runner(&socket_path, uid, gid)?;
    if trigger_config.is_some() {
        endpoint.enable_fixed_jj_trigger();
    }
    if env::var_os("BTRFS_AWACS_PRECISION_GUARD").as_deref() == Some(OsStr::new("1")) {
        endpoint.enable_precision_guard(runtime.to_path_buf());
    }
    endpoint
        .register(&mut facade, &root, watch_id, grant_id, uid, gid)
        .map_err(|error| error.to_string())?;
    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| format!("bind Watchman socket {}: {error}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("set Watchman socket mode: {error}"))?;
    let endpoint = std::sync::Arc::new(endpoint);
    let facade = std::sync::Arc::new(std::sync::Mutex::new(facade));
    if let Some(trigger_config) = trigger_config {
        spawn_trigger_scheduler(std::sync::Arc::clone(&facade), trigger_config)?;
    }
    loop {
        let (stream, _) = listener
            .accept()
            .map_err(|error| format!("accept Watchman connection: {error}"))?;
        let endpoint = std::sync::Arc::clone(&endpoint);
        let facade = std::sync::Arc::clone(&facade);
        std::thread::Builder::new()
            .name("btrfs-awacs-watchman-client".to_owned())
            .spawn(move || {
                let Ok(mut transport) = CredentialedStream::new(stream) else {
                    return;
                };
                loop {
                    let Ok(frame) = transport.receive_frame(BserLimits::default()) else {
                        return;
                    };
                    let Ok(now_ns) = current_time_ns() else {
                        return;
                    };
                    let request = match facade.lock() {
                        Ok(facade) => match transport.decode_and_authorize(
                            &endpoint,
                            &facade,
                            &frame,
                            BserLimits::default(),
                        ) {
                            Ok(request) => request,
                            Err(error) => {
                                eprintln!("btrfs-awacs Watchman transport failed: {error}");
                                return;
                            }
                        },
                        Err(_) => return,
                    };
                    let concurrent = match facade.lock() {
                        Ok(mut facade) => endpoint.begin_concurrent_frame(
                            &mut facade,
                            &request,
                            frame.identity.uid,
                            frame.identity.gid,
                            now_ns,
                            BserLimits::default(),
                        ),
                        Err(_) => return,
                    };
                    let prepared = match concurrent {
                        Ok(Some(pending)) => {
                            let completed = match pending.execute() {
                                Ok(completed) => completed,
                                Err(error) => {
                                    eprintln!(
                                        "btrfs-awacs Watchman concurrent cut failed: {error}"
                                    );
                                    return;
                                }
                            };
                            match facade.lock() {
                                Ok(mut facade) => endpoint
                                    .finish_concurrent_frame(&mut facade, completed)
                                    .map_err(|error| error.to_string()),
                                Err(_) => return,
                            }
                        }
                        Ok(None) => match facade.lock() {
                            Ok(mut facade) => transport
                                .prepare_authenticated_frame(
                                    &endpoint,
                                    &mut facade,
                                    frame,
                                    now_ns,
                                    BserLimits::default(),
                                )
                                .map_err(|error| error.to_string()),
                            Err(_) => return,
                        },
                        Err(error) => endpoint
                            .prepare_error_frame(error, BserLimits::default())
                            .map_err(|error| error.to_string()),
                    };
                    let prepared = match prepared {
                        Ok(prepared) => prepared,
                        Err(error) => {
                            eprintln!("btrfs-awacs Watchman client frame failed: {error}");
                            return;
                        }
                    };
                    let write = transport.send_prepared_frame(&prepared, BserLimits::default());
                    let release = match facade.lock() {
                        Ok(mut facade) => {
                            transport.finish_prepared_frame(&endpoint, &mut facade, prepared)
                        }
                        Err(_) => return,
                    };
                    if let Err(error) = combine_daemon_response(write, release) {
                        eprintln!("btrfs-awacs Watchman client frame failed: {error}");
                        return;
                    }
                }
            })
            .map_err(|error| format!("start Watchman client worker: {error}"))?;
    }
}

fn combine_daemon_response(
    write: Result<(), btrfs_awacs::watchman_transport::TransportError>,
    release: Result<(), btrfs_awacs::watchman_transport::TransportError>,
) -> Result<(), String> {
    match (write, release) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(write), Ok(())) => Err(write.to_string()),
        (Ok(()), Err(release)) => Err(release.to_string()),
        (Err(write), Err(release)) => Err(format!("{write}; {release}")),
    }
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
    fn discovery_accepts_only_an_owned_unix_socket() {
        let temp = tempdir().unwrap();
        let socket = temp.path().join("watchman.sock");
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
    fn default_help_lists_every_subcommand_and_multicall_entrypoint() {
        let help = Cli::command().render_long_help().to_string();
        for command in [
            "snap",
            "compare",
            "broker-serve",
            "watchman-serve",
            "__changed-objects-send",
            "__btrfs-inspect",
            "__broker-changed-objects",
            "__broker-create-snapshot",
            "__broker-delete-snapshot",
            "__broker-publish-worktree",
            "__broker-full-index",
            "__service-smoke",
            "__service-recovery-smoke",
            "__nested-boundary-smoke",
            "git-fsmonitor-hook",
            "btrfs-awacs-watchman",
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
