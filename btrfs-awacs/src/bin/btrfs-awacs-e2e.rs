use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use btrfs_awacs::bser::decode_frame;
use btrfs_awacs::bser::encode_frame;
use btrfs_awacs::bser::Limits as BserLimits;
use btrfs_awacs::bser::Value as BserValue;
use btrfs_awacs::compat::ClientFlavor;
use btrfs_awacs::facade::FacadeService;
use btrfs_awacs::manager::Permissions;
use btrfs_awacs::manager::Principal;
use btrfs_awacs::manager::PERMISSION_CUT;
use btrfs_awacs::manager::PERMISSION_READ;
use btrfs_awacs::service::InitializeOptions;
use btrfs_awacs::service::Service;
use btrfs_awacs::service::ServiceConfig;
use btrfs_awacs::service::WorktreeOptions;
use btrfs_awacs::store::BrokerJournal;
use btrfs_awacs::store::ServiceMetadata;
use btrfs_awacs::store::Store;
use btrfs_awacs::watchman::WatchmanEndpoint;
use btrfs_awacs::watchman_transport::CredentialedStream;
use clap::Parser;

type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Parser)]
#[command(about = "Run the privileged Btrfs AWACS Worktree matrix")]
struct Args {
    /// Existing directory on the Btrfs filesystem used for test subvolumes.
    #[arg(long, value_name = "PATH")]
    root: PathBuf,

    /// Preserve failed matrix cells for inspection.
    #[arg(long)]
    keep_failures: bool,
}

#[derive(Clone, Copy)]
struct SnapshotVariation {
    name: &'static str,
    populate: fn(&Path) -> io::Result<()>,
}

#[derive(Clone, Copy)]
struct ModificationVariation {
    name: &'static str,
    apply: fn(&Path) -> io::Result<()>,
    expected_paths: &'static [&'static [u8]],
}

const SNAPSHOT_VARIATIONS: &[SnapshotVariation] = &[
    SnapshotVariation {
        name: "minimal",
        populate: snapshot_minimal,
    },
    SnapshotVariation {
        name: "nested",
        populate: snapshot_nested,
    },
    SnapshotVariation {
        name: "hardlinks",
        populate: snapshot_hardlinks,
    },
];

const MODIFICATION_VARIATIONS: &[ModificationVariation] = &[
    ModificationVariation {
        name: "modify-file",
        apply: modify_file,
        expected_paths: &[b"common.txt"],
    },
    ModificationVariation {
        name: "create-file",
        apply: create_file,
        expected_paths: &[b"created.txt"],
    },
    ModificationVariation {
        name: "rename-file",
        apply: rename_file,
        expected_paths: &[b"rename-source.txt", b"renamed.txt"],
    },
    ModificationVariation {
        name: "modify-hardlink",
        apply: modify_hardlink,
        expected_paths: &[b"linked-alias.txt", b"linked-source.txt"],
    },
];

fn main() {
    if let Err(error) = run() {
        eprintln!("btrfs-awacs-e2e: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    if unsafe { libc::geteuid() } != 0 {
        return Err("the e2e matrix must run as root".to_owned());
    }
    fs::create_dir_all(&args.root)
        .map_err(|error| format!("create test root {}: {error}", args.root.display()))?;
    let root = fs::canonicalize(&args.root)
        .map_err(|error| format!("canonicalize test root {}: {error}", args.root.display()))?;
    let run_root = root.join(format!("run-{}", std::process::id()));
    fs::create_dir(&run_root)
        .map_err(|error| format!("create run directory {}: {error}", run_root.display()))?;
    if let Err(error) = preflight_kernel(&run_root) {
        let _ = fs::remove_dir(&run_root);
        return Err(error);
    }

    let mut passed = 0_usize;
    let mut failures = Vec::new();
    for snapshot in SNAPSHOT_VARIATIONS {
        for modification in MODIFICATION_VARIATIONS {
            let name = format!("{}--{}", snapshot.name, modification.name);
            println!(
                "==> snapshot={} modification={}",
                snapshot.name, modification.name
            );
            let case_root = run_root.join(&name);
            let result = run_case(&case_root, *snapshot, *modification);
            let cleanup = if result.is_err() && args.keep_failures {
                eprintln!("preserving failed case at {}", case_root.display());
                Ok(())
            } else {
                cleanup_case(&run_root, &case_root)
            };
            match combine_case_results(result, cleanup) {
                Ok(()) => {
                    println!("PASS {name}");
                    passed += 1;
                }
                Err(error) => {
                    eprintln!("FAIL {name}: {error}");
                    failures.push(format!("{name}: {error}"));
                }
            }
        }
    }

    if !args.keep_failures || failures.is_empty() {
        if let Err(error) = fs::remove_dir(&run_root) {
            failures.push(format!(
                "remove empty run directory {}: {error}",
                run_root.display()
            ));
        }
    }
    println!("\n{passed} passed; {} failed", failures.len());
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn run_case(
    case_root: &Path,
    snapshot: SnapshotVariation,
    modification: ModificationVariation,
) -> Result<()> {
    fs::create_dir(case_root)
        .map_err(|error| format!("create case directory {}: {error}", case_root.display()))?;
    let source = case_root.join("source");
    let destination_root = case_root.join("destination-root");
    create_subvolume(&source)?;
    create_subvolume(&destination_root)?;

    populate_common_snapshot(&source).map_err(|error| {
        format!(
            "populate common snapshot tree {}: {error}",
            source.display()
        )
    })?;
    (snapshot.populate)(&source).map_err(|error| {
        format!(
            "populate {} snapshot tree {}: {error}",
            snapshot.name,
            source.display()
        )
    })?;

    let managed = destination_root.join("managed");
    let worktrees = destination_root.join("worktrees");
    let spool = case_root.join("spool");
    for directory in [&managed, &worktrees, &spool] {
        fs::create_dir(directory)
            .map_err(|error| format!("create {}: {error}", directory.display()))?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("make {} private: {error}", directory.display()))?;
    }

    let now = current_time_ns()?;
    let metadata = ServiceMetadata::generate([0x41; 16], now)
        .map_err(|error| format!("generate service metadata: {error}"))?;
    let store_path = case_root.join("manager.sqlite3");
    let journal_path = case_root.join("broker.sqlite3");
    let store = Store::create(&store_path, &metadata)
        .map_err(|error| format!("create manager store: {error}"))?;
    let journal = BrokerJournal::create(&journal_path)
        .map_err(|error| format!("create broker journal: {error}"))?;
    let config = ServiceConfig::new(managed, spool, [0x41; 16]).allow_experimental_dirty_witness();
    let mut service =
        Service::new(store, journal, config).map_err(|error| format!("create service: {error}"))?;
    let permissions = Permissions::new(PERMISSION_READ | PERMISSION_CUT)
        .map_err(|error| format!("create test permissions: {error}"))?;
    let initialized = service
        .initialize(
            &source,
            &InitializeOptions {
                principal: Principal::Uid(0),
                permissions,
                requester_uid: 0,
                requester_gid: 0,
                now_ns: current_time_ns()?,
            },
        )
        .map_err(|error| format!("initialize {} snapshot: {error}", snapshot.name))?;

    let policy = service
        .provision_sanitized_worktree_policy(
            initialized.watch_id,
            initialized.grant_id,
            &destination_root,
            current_time_ns()?,
        )
        .map_err(|error| format!("provision Worktree policy: {error}"))?;
    let reservation_name = b"worktree.reservation".to_vec();
    let reservation_nonce = [0x52; 32];
    let reservation_path = worktrees.join(std::ffi::OsStr::from_bytes(&reservation_name));
    let mut reservation = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&reservation_path)
        .map_err(|error| format!("create Worktree reservation: {error}"))?;
    reservation
        .write_all(&reservation_nonce)
        .and_then(|()| reservation.sync_all())
        .map_err(|error| format!("write Worktree reservation: {error}"))?;
    drop(reservation);

    let worktree = service
        .worktree(
            &policy,
            &WorktreeOptions {
                watch_id: initialized.watch_id,
                authorization_id: initialized.grant_id,
                destination_root,
                destination_parent: worktrees,
                destination_name: b"worktree".to_vec(),
                reservation_name,
                reservation_nonce,
                requester_uid: 0,
                requester_gid: 0,
                now_ns: current_time_ns()?,
            },
        )
        .map_err(|error| format!("publish Worktree: {error}"))?;

    let mut facade = FacadeService::new(service);
    let watchman = WatchmanEndpoint::default();
    watchman
        .register(
            &mut facade,
            &source,
            initialized.watch_id,
            initialized.grant_id,
            0,
            0,
        )
        .map_err(|error| format!("register source Watchman root: {error}"))?;
    let (watchman_client, watchman_server) =
        UnixStream::pair().map_err(|error| format!("create Watchman socket pair: {error}"))?;
    let mut client_transport = CredentialedStream::new(watchman_client)
        .map_err(|error| format!("arm Watchman client transport: {error}"))?;
    let mut server_transport = CredentialedStream::new(watchman_server)
        .map_err(|error| format!("arm Watchman server transport: {error}"))?;
    let worktree_root = fs::canonicalize(&worktree.path)
        .map_err(|error| format!("canonicalize Worktree: {error}"))?
        .as_os_str()
        .as_bytes()
        .to_vec();
    let watch_project = BserValue::Array(vec![
        BserValue::Bytes(b"watch-project".to_vec()),
        BserValue::Bytes(worktree_root.clone()),
    ]);
    let watch_response = watchman_round_trip(
        &watchman,
        &mut facade,
        &mut client_transport,
        &mut server_transport,
        &watch_project,
    )?;
    let watch_object = response_object(&watch_response, "watch-project")?;
    if watch_object.get(b"watch".as_slice()) != Some(&BserValue::Bytes(worktree_root.clone())) {
        return Err(format!(
            "Watchman watch-project did not preserve the Worktree root: {watch_response:?}"
        ));
    }

    let baseline_response = watchman_round_trip(
        &watchman,
        &mut facade,
        &mut client_transport,
        &mut server_transport,
        &watchman_query(&worktree_root, None),
    )?;
    let (baseline_clock, baseline_fresh, baseline_paths) = watchman_projection(&baseline_response)?;
    if !baseline_fresh || baseline_paths != [b"/".to_vec()] {
        return Err(format!(
            "initial Watchman query was not a fresh baseline: {baseline_response:?}"
        ));
    }

    (modification.apply)(&worktree.path).map_err(|error| {
        format!(
            "apply {} to {}: {error}",
            modification.name,
            worktree.path.display()
        )
    })?;
    let watchman_delta = watchman_round_trip(
        &watchman,
        &mut facade,
        &mut client_transport,
        &mut server_transport,
        &watchman_query(&worktree_root, Some(&baseline_clock)),
    )?;
    let (_, watchman_fresh, mut watchman_paths) = watchman_projection(&watchman_delta)?;
    watchman_paths.sort();

    let direct_delta = facade
        .query(
            worktree.watch_id,
            Some(&baseline_clock),
            ClientFlavor::Jj,
            0,
            0,
            current_time_ns()?,
        )
        .map_err(|error| format!("directly query modified Worktree: {error}"))?;
    let mut expected = modification
        .expected_paths
        .iter()
        .map(|path| path.to_vec())
        .collect::<Vec<_>>();
    expected.sort();
    let mut direct_paths = direct_delta.projection.paths;
    direct_paths.sort();
    if watchman_fresh || watchman_paths != expected {
        return Err(format!(
            "unexpected Watchman {} delta: fresh={watchman_fresh} expected={expected:?} actual={watchman_paths:?}",
            modification.name
        ));
    }
    if direct_delta.projection.fresh_instance || direct_paths != expected {
        return Err(format!(
            "unexpected direct {} delta: fresh={} expected={expected:?} actual={direct_paths:?}",
            modification.name, direct_delta.projection.fresh_instance
        ));
    }
    Ok(())
}

fn watchman_query(root: &[u8], since: Option<&str>) -> BserValue {
    let expression = BserValue::Array(vec![
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
    let mut options = BTreeMap::from([
        (b"expression".to_vec(), expression),
        (
            b"fields".to_vec(),
            BserValue::Array(vec![BserValue::Bytes(b"name".to_vec())]),
        ),
    ]);
    if let Some(since) = since {
        options.insert(
            b"since".to_vec(),
            BserValue::Bytes(since.as_bytes().to_vec()),
        );
    }
    BserValue::Array(vec![
        BserValue::Bytes(b"query".to_vec()),
        BserValue::Bytes(root.to_vec()),
        BserValue::Object(options),
    ])
}

fn watchman_round_trip(
    endpoint: &WatchmanEndpoint,
    facade: &mut FacadeService,
    client: &mut CredentialedStream,
    server: &mut CredentialedStream,
    request: &BserValue,
) -> Result<BserValue> {
    let limits = BserLimits::default();
    let encoded = encode_frame(request, limits)
        .map_err(|error| format!("encode Watchman request: {error}"))?;
    client
        .send_frame(&encoded, limits)
        .map_err(|error| format!("send Watchman request: {error}"))?;
    let frame = server
        .receive_frame(limits)
        .map_err(|error| format!("receive authenticated Watchman request: {error}"))?;
    let decoded = server
        .decode_and_authorize(endpoint, facade, &frame, limits)
        .map_err(|error| format!("authorize Watchman request: {error}"))?;
    let uid = frame.identity.uid;
    let gid = frame.identity.gid;
    let concurrent = endpoint
        .begin_concurrent_frame(facade, &decoded, uid, gid, current_time_ns()?, limits)
        .map_err(|error| format!("begin Watchman request: {error}"))?;
    let prepared = match concurrent {
        Some(pending) => {
            let completed = pending
                .execute()
                .map_err(|error| format!("execute concurrent Watchman query: {error}"))?;
            endpoint
                .finish_concurrent_frame(facade, completed)
                .map_err(|error| format!("finish concurrent Watchman query: {error}"))?
        }
        None => server
            .prepare_authenticated_frame(endpoint, facade, frame, current_time_ns()?, limits)
            .map_err(|error| format!("prepare Watchman response: {error}"))?,
    };
    let write = server
        .send_prepared_frame(&prepared, limits)
        .map_err(|error| format!("write Watchman response: {error}"));
    let release = server
        .finish_prepared_frame(endpoint, facade, prepared)
        .map_err(|error| format!("release Watchman response: {error}"));
    match (write, release) {
        (Ok(()), Ok(())) => {}
        (Err(write), Ok(())) => return Err(write),
        (Ok(()), Err(release)) => return Err(release),
        (Err(write), Err(release)) => return Err(format!("{write}; {release}")),
    }
    let response = client
        .receive_frame(limits)
        .map_err(|error| format!("receive Watchman response: {error}"))?;
    let response = decode_frame(&response.bytes, limits)
        .map_err(|error| format!("decode Watchman response: {error}"))?;
    if let BserValue::Object(object) = &response {
        if let Some(BserValue::Bytes(error)) = object.get(b"error".as_slice()) {
            return Err(format!(
                "Watchman returned an error: {}",
                String::from_utf8_lossy(error)
            ));
        }
    }
    Ok(response)
}

fn response_object<'a>(
    response: &'a BserValue,
    command: &str,
) -> Result<&'a BTreeMap<Vec<u8>, BserValue>> {
    match response {
        BserValue::Object(object) => Ok(object),
        _ => Err(format!("Watchman {command} response is not an object")),
    }
}

fn watchman_projection(response: &BserValue) -> Result<(String, bool, Vec<Vec<u8>>)> {
    let object = response_object(response, "query")?;
    let clock = match object.get(b"clock".as_slice()) {
        Some(BserValue::Bytes(clock)) => std::str::from_utf8(clock)
            .map(str::to_owned)
            .map_err(|_| "Watchman query clock is not UTF-8".to_owned())?,
        _ => return Err("Watchman query omitted its byte-string clock".to_owned()),
    };
    let fresh = match object.get(b"is_fresh_instance".as_slice()) {
        Some(BserValue::Bool(fresh)) => *fresh,
        _ => return Err("Watchman query omitted is_fresh_instance".to_owned()),
    };
    let paths = match object.get(b"files".as_slice()) {
        Some(BserValue::Array(paths)) => paths
            .iter()
            .map(|path| match path {
                BserValue::Bytes(path) => Ok(path.clone()),
                _ => Err("Watchman query returned a non-byte path".to_owned()),
            })
            .collect::<Result<Vec<_>>>()?,
        _ => return Err("Watchman query omitted its files array".to_owned()),
    };
    Ok((clock, fresh, paths))
}

fn populate_common_snapshot(root: &Path) -> io::Result<()> {
    fs::write(root.join("common.txt"), b"common baseline\n")?;
    fs::write(root.join("rename-source.txt"), b"rename baseline\n")?;
    fs::write(root.join("linked-source.txt"), b"hardlink baseline\n")?;
    fs::hard_link(
        root.join("linked-source.txt"),
        root.join("linked-alias.txt"),
    )
}

fn snapshot_minimal(_root: &Path) -> io::Result<()> {
    Ok(())
}

fn snapshot_nested(root: &Path) -> io::Result<()> {
    let nested = root.join("snapshot-fixture/nested/deep");
    fs::create_dir_all(&nested)?;
    fs::write(nested.join("leaf.txt"), b"nested snapshot fixture\n")?;
    fs::write(
        root.join("snapshot-fixture/top.txt"),
        b"top snapshot fixture\n",
    )
}

fn snapshot_hardlinks(root: &Path) -> io::Result<()> {
    let fixture = root.join("snapshot-fixture");
    fs::create_dir(&fixture)?;
    fs::write(fixture.join("first.txt"), b"shared snapshot fixture\n")?;
    fs::hard_link(fixture.join("first.txt"), fixture.join("second.txt"))
}

fn modify_file(root: &Path) -> io::Result<()> {
    fs::write(root.join("common.txt"), b"modified in Worktree\n")
}

fn create_file(root: &Path) -> io::Result<()> {
    fs::write(root.join("created.txt"), b"created in Worktree\n")
}

fn rename_file(root: &Path) -> io::Result<()> {
    fs::rename(root.join("rename-source.txt"), root.join("renamed.txt"))
}

fn modify_hardlink(root: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .append(true)
        .open(root.join("linked-source.txt"))?;
    file.write_all(b"modified through one alias\n")
}

fn create_subvolume(path: &Path) -> Result<()> {
    let output = Command::new("btrfs")
        .args(["subvolume", "create"])
        .arg(path)
        .output()
        .map_err(|error| format!("run btrfs subvolume create: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "create subvolume {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn preflight_kernel(run_root: &Path) -> Result<()> {
    let preflight_root = run_root.join("preflight");
    fs::create_dir(&preflight_root)
        .map_err(|error| format!("create kernel preflight directory: {error}"))?;
    let subvolume = preflight_root.join("source");
    if let Err(error) = create_subvolume(&subvolume) {
        let _ = fs::remove_dir(&preflight_root);
        return Err(error);
    }
    let inspection = btrfs_awacs::btrfs::OpenedSubvolume::open(&subvolume)
        .map(|_| ())
        .map_err(|error| {
            format!(
                "kernel preflight could not inspect a new Btrfs subvolume: {error}; \
                 this project requires the documented GET_SUBVOL_INFO fix and changed-object ABI"
            )
        });
    let cleanup = delete_subvolume(&subvolume).and_then(|()| {
        fs::remove_dir(&preflight_root)
            .map_err(|error| format!("remove kernel preflight directory: {error}"))
    });
    combine_case_results(inspection, cleanup)
}

fn cleanup_case(run_root: &Path, case_root: &Path) -> Result<()> {
    if !case_root.starts_with(run_root) || case_root == run_root {
        return Err(format!(
            "refusing to clean unsafe case path {}",
            case_root.display()
        ));
    }
    if !case_root.exists() {
        return Ok(());
    }
    let mut subvolumes = Vec::new();
    collect_subvolumes(case_root, &mut subvolumes)
        .map_err(|error| format!("find test subvolumes: {error}"))?;
    for subvolume in subvolumes {
        delete_subvolume(&subvolume)?;
    }
    fs::remove_dir_all(case_root)
        .map_err(|error| format!("remove case directory {}: {error}", case_root.display()))
}

fn delete_subvolume(path: &Path) -> Result<()> {
    let output = Command::new("btrfs")
        .args(["subvolume", "delete"])
        .arg(path)
        .output()
        .map_err(|error| format!("run btrfs subvolume delete: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "delete test subvolume {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn collect_subvolumes(directory: &Path, output: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_subvolumes(&entry.path(), output)?;
            if metadata.ino() == btrfs_awacs::btrfs::ROOT_INODE {
                output.push(entry.path());
            }
        }
    }
    Ok(())
}

fn combine_case_results(case: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (case, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(case), Ok(())) => Err(case),
        (Ok(()), Err(cleanup)) => Err(format!("cleanup failed: {cleanup}")),
        (Err(case), Err(cleanup)) => Err(format!("{case}; cleanup failed: {cleanup}")),
    }
}

fn current_time_ns() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read current time: {error}"))?;
    i64::try_from(elapsed.as_nanos()).map_err(|_| "current time exceeds i64".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_is_the_cartesian_product() {
        let names = SNAPSHOT_VARIATIONS
            .iter()
            .flat_map(|snapshot| {
                MODIFICATION_VARIATIONS
                    .iter()
                    .map(move |modification| format!("{}--{}", snapshot.name, modification.name))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names.len(),
            SNAPSHOT_VARIATIONS.len() * MODIFICATION_VARIATIONS.len()
        );
        let unique = names.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn every_snapshot_and_modification_fixture_composes() {
        for snapshot in SNAPSHOT_VARIATIONS {
            for modification in MODIFICATION_VARIATIONS {
                let temp = tempfile::tempdir().unwrap();
                populate_common_snapshot(temp.path()).unwrap();
                (snapshot.populate)(temp.path()).unwrap();
                (modification.apply)(temp.path()).unwrap();
            }
        }
    }
}
