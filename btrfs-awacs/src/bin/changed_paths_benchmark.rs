use btrfs_awacs::broker::{
    ChangedObjectsExecution, ExpectedSubvolume, INODE_PATH_OUTPUT_HEADER_SIZE,
    INODE_PATH_OUTPUT_MAGIC, INODE_PATH_OUTPUT_VERSION, INODE_REF_IOCTL_BATCH_SIZE,
    InodePathsExecution, SeqPacket,
};
use btrfs_awacs::broker_protocol::{BrokerClient, BrokerDispatcher};
use btrfs_awacs::btrfs::{OpenedSubvolume, ROOT_INODE};
use btrfs_awacs::manager::{PERMISSION_CUT, PERMISSION_READ, Permissions, Principal};
use btrfs_awacs::manifest::{
    CHANGED_OBJECTS_V2_MAGIC, ChangedObjectsManifest, parse_changed_objects,
    parse_changed_objects_v2,
};
use btrfs_awacs::service::{ChangesOptions, InitializeOptions, Service, ServiceConfig};
use btrfs_awacs::store::{ServiceMetadata, Store};
use clap::Parser;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const DEFAULT_COUNTS: [usize; 4] = [10, 100, 1_000, 100_000];
const DEFAULT_HISTORY_SEED: u64 = 0x5eed_5eed_5eed_5eed;
const HISTORY_COMMIT_BATCH_SIZE: usize = 512;
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;

/// Measures kernel changed-object enumeration and two changed-inode path
/// projection lanes on an owned Btrfs fixture.
///
/// The fixture root must be an existing writable directory on Btrfs. The
/// benchmark derives four disjoint path cohorts from randomly ordered commits
/// in a separate history checkout, keeps only paths that still exist in that
/// checkout's @ working tree, then snapshots that checkout into one uniquely
/// named child subvolume and creates a private AWACS store beneath it. The
/// timed mutations touch only the owned snapshot, so the benchmark preserves
/// the history checkout's full directory/inode workload without modifying it.
#[derive(Debug, Parser)]
#[command(name = "changed-paths-benchmark")]
struct Args {
    /// Existing writable Btrfs directory under which an owned fixture is made.
    #[arg(long, value_name = "PATH")]
    fixture_root: PathBuf,
    /// Checkout whose Git history supplies changed paths and whose @ tree filters them.
    #[arg(long, value_name = "PATH")]
    history_repo: PathBuf,
    /// Seed used to deterministically randomize the historical commit order.
    #[arg(long, default_value_t = DEFAULT_HISTORY_SEED)]
    history_seed: u64,
    /// Number of timed repeats for each changed-file count.
    #[arg(long, default_value_t = 3)]
    repetitions: usize,
    /// Keep the owned fixture after the run for inspection.
    #[arg(long)]
    keep_fixture: bool,
}

#[derive(Debug)]
struct TimingRow {
    changed_files: usize,
    repeat: usize,
    changed_objects_us: u128,
    reverse_lookup_us: u128,
    manifest_objects: usize,
    resolved_paths: usize,
    manifest_bytes: u64,
    reverse_output_bytes: u64,
}

fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("changed-paths-benchmark: {error}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), String> {
    if args.repetitions == 0 {
        return Err("--repetitions must be positive".to_owned());
    }
    let cohorts = HistoryPathCohorts::from_repo(&args.history_repo, args.history_seed)?;
    eprintln!(
        "history_repo={} history_seed={} commits_scanned={} paths={}",
        args.history_repo.display(),
        args.history_seed,
        cohorts.commits_scanned,
        cohorts.path_count(),
    );
    let fixture = Fixture::create(&args.fixture_root, &args.history_repo)?;
    eprintln!("fixture={}", fixture.root.display());
    let result = run_fixture(&fixture, args.repetitions, &cohorts);
    if args.keep_fixture {
        eprintln!("kept_fixture={}", fixture.root.display());
    } else if let Err(error) = fixture.cleanup() {
        eprintln!(
            "warning: fixture cleanup failed ({error}); kept_fixture={}",
            fixture.root.display()
        );
    }
    result
}

fn run_fixture(
    fixture: &Fixture,
    repetitions: usize,
    cohorts: &HistoryPathCohorts,
) -> Result<(), String> {
    let initial_now_ns = now_ns()?;
    let metadata = ServiceMetadata::generate([0x42; 16], initial_now_ns)
        .map_err(|error| format!("generate store metadata: {error}"))?;
    let store = Store::create(&fixture.manager_db, &metadata)
        .map_err(|error| format!("create benchmark store: {error}"))?;
    let config = ServiceConfig::new(fixture.managed.clone(), fixture.spool.clone(), [0x42; 16]);
    let mut service =
        Service::new(store, config).map_err(|error| format!("open benchmark service: {error}"))?;
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let initialized = service
        .initialize(
            &fixture.source,
            &InitializeOptions {
                principal: Principal::Uid(u64::from(uid)),
                permissions: Permissions::new(PERMISSION_READ | PERMISSION_CUT)
                    .map_err(|error| error.to_string())?,
                requester_uid: uid,
                requester_gid: gid,
                now_ns: initial_now_ns,
            },
        )
        .map_err(|error| format!("initialize benchmark watch: {error}"))?;
    let benchmark_store = Store::open(&fixture.manager_db)
        .map_err(|error| format!("open benchmark reader: {error}"))?;
    let broker = embedded_broker(metadata.store_uuid, uid)?;
    let mut rows = Vec::new();
    print_header();

    for (&count, paths) in DEFAULT_COUNTS.iter().zip(&cohorts.paths) {
        mutate_files(&fixture.source, count, paths)?;
        let published = service
            .changes(&ChangesOptions {
                watch_id: initialized.watch_id,
                authorization_id: initialized.grant_id,
                requester_uid: uid,
                requester_gid: gid,
                now_ns: now_ns()?,
            })
            .map_err(|error| format!("publish {count}-file benchmark cut: {error}"))?;
        let (parent_path, target_path) =
            comparison_paths(&benchmark_store, published.comparison_id)?;
        let parent = OpenedSubvolume::open(&parent_path)
            .map_err(|error| format!("open parent snapshot: {error}"))?;
        let target = OpenedSubvolume::open(&target_path)
            .map_err(|error| format!("open target snapshot: {error}"))?;
        let parent_expected =
            ExpectedSubvolume::from_observed(&parent.filesystem, &parent.subvolume);
        let target_expected =
            ExpectedSubvolume::from_observed(&target.filesystem, &target.subvolume);

        for repeat in 1..=repetitions {
            let manifest_file = fixture.private_file("manifest")?;
            let changed_request = ChangedObjectsExecution {
                parent: parent_expected.clone(),
                target: target_expected.clone(),
                output_owner_uid: uid,
                max_output_bytes: MAX_OUTPUT_BYTES,
            };
            let changed_started = Instant::now();
            let changed = broker
                .changed_objects(
                    &changed_request,
                    parent.as_fd(),
                    target.as_fd(),
                    manifest_file.as_fd(),
                )
                .map_err(|error| format!("changed objects for {count} files: {error}"))?;
            let changed_objects_us = changed_started.elapsed().as_micros();
            let manifest_bytes = read_file(&manifest_file)?;
            let manifest = parse_manifest(&manifest_bytes)
                .map_err(|error| format!("parse changed objects for {count} files: {error}"))?;
            let inodes: Vec<u64> = manifest
                .objects
                .keys()
                .copied()
                .filter(|ino| *ino != ROOT_INODE)
                .collect();
            if inodes.len() != count {
                return Err(format!(
                    "{count}-file cut yielded {} non-root changed inodes",
                    inodes.len()
                ));
            }

            let input_file = fixture.private_file("inodes")?;
            write_inodes(&input_file, &inodes)?;
            let reverse_file = fixture.private_file("reverse")?;
            let reverse_request = InodePathsExecution {
                target: target_expected.clone(),
                owner_uid: uid,
                inode_count: u64::try_from(inodes.len())
                    .map_err(|_| "inode count exceeds u64".to_owned())?,
                max_output_bytes: MAX_OUTPUT_BYTES,
            };
            let reverse_started = Instant::now();
            let reverse = broker
                .inode_paths(
                    &reverse_request,
                    target.as_fd(),
                    input_file.as_fd(),
                    reverse_file.as_fd(),
                )
                .map_err(|error| format!("reverse lookup for {count} files: {error}"))?;
            let reverse_lookup_us = reverse_started.elapsed().as_micros();
            let reverse_paths = parse_reverse_output(&read_file(&reverse_file)?)?;
            if reverse_paths.len() != inodes.len()
                || inodes.iter().any(|ino| !reverse_paths.contains_key(ino))
            {
                return Err(format!(
                    "reverse lookup omitted one or more of the {count} changed inodes"
                ));
            }
            let row = TimingRow {
                changed_files: count,
                repeat,
                changed_objects_us,
                reverse_lookup_us,
                manifest_objects: manifest.objects.len(),
                resolved_paths: usize::try_from(reverse.path_count)
                    .map_err(|_| "path count exceeds usize".to_owned())?,
                manifest_bytes: changed.output_bytes,
                reverse_output_bytes: reverse.output_bytes,
            };
            print_row(&row);
            rows.push(row);
        }
    }
    eprintln!("rows={}", rows.len());
    Ok(())
}

fn embedded_broker(store_uuid: [u8; 16], uid: u32) -> Result<BrokerClient, String> {
    let (client_socket, server_socket) =
        SeqPacket::pair().map_err(|error| format!("create benchmark broker channel: {error}"))?;
    let dispatcher = BrokerDispatcher::new(uid)
        .map_err(|error| format!("create benchmark broker dispatcher: {error}"))?;
    thread::Builder::new()
        .name("changed-paths-benchmark-broker".to_owned())
        .spawn(move || while dispatcher.serve_one(&server_socket).is_ok() {})
        .map_err(|error| format!("start benchmark broker dispatcher: {error}"))?;
    BrokerClient::connect(client_socket, store_uuid)
        .map_err(|error| format!("connect benchmark broker: {error}"))
}

fn comparison_paths(store: &Store, comparison_id: i64) -> Result<(PathBuf, PathBuf), String> {
    let (parent, target): (Vec<u8>, Vec<u8>) = store
        .connection()
        .query_row(
            r#"SELECT a.path, b.path
                 FROM comparisons c
                 JOIN snapshots a ON a.id = c.from_snapshot_id
                 JOIN snapshots b ON b.id = c.to_snapshot_id
                WHERE c.id = ?1"#,
            [comparison_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| format!("load benchmark snapshot paths: {error}"))?;
    Ok((
        PathBuf::from(OsString::from_vec(parent)),
        PathBuf::from(OsString::from_vec(target)),
    ))
}

fn mutate_files(source: &Path, count: usize, paths: &[PathBuf]) -> Result<(), String> {
    if paths.len() != count {
        return Err(format!(
            "{count}-file cohort contains {} paths",
            paths.len()
        ));
    }
    for (index, relative_path) in paths.iter().enumerate() {
        let path = source.join(relative_path);
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|error| format!("open fixture file {}: {error}", path.display()))?;
        writeln!(file, "changed-{count}-{index}")
            .map_err(|error| format!("mutate fixture file {}: {error}", path.display()))?;
    }
    Ok(())
}

struct HistoryPathCohorts {
    paths: Vec<Vec<PathBuf>>,
    commits_scanned: usize,
}

impl HistoryPathCohorts {
    fn from_repo(repo: &Path, seed: u64) -> Result<Self, String> {
        if !repo.is_dir() {
            return Err(format!(
                "history repo {} is not a directory",
                repo.display()
            ));
        }
        let commits = randomized_history_commits(repo, seed)?;
        let required_paths: usize = DEFAULT_COUNTS.iter().sum();
        let mut selected_raw_paths = HashSet::with_capacity(required_paths);
        let mut selected_inodes = HashSet::with_capacity(required_paths);
        let mut selected_paths = Vec::with_capacity(required_paths);
        let mut commits_scanned = 0;

        for commit_batch in commits.chunks(HISTORY_COMMIT_BATCH_SIZE) {
            let changed_paths = changed_paths_for_commits(repo, commit_batch)?;
            commits_scanned += commit_batch.len();
            for raw_path in changed_paths.split(|byte| *byte == b'\0') {
                if raw_path.is_empty() || selected_raw_paths.contains(raw_path) {
                    continue;
                }
                let Some(path) = safe_relative_path(raw_path) else {
                    continue;
                };
                let Some(inode) = regular_file_inode_in_working_tree(repo, &path) else {
                    continue;
                };
                if !selected_inodes.insert(inode) {
                    continue;
                }
                selected_raw_paths.insert(raw_path.to_vec());
                selected_paths.push(path);
                if selected_paths.len() == required_paths {
                    return Ok(Self::from_selected_paths(selected_paths, commits_scanned));
                }
            }
            if commits_scanned % (HISTORY_COMMIT_BATCH_SIZE * 16) == 0 {
                eprintln!(
                    "history_commits_scanned={commits_scanned} eligible_paths={}",
                    selected_paths.len()
                );
            }
        }

        Err(format!(
            "history yielded {} distinct paths present in the @ working tree; need {required_paths}",
            selected_paths.len()
        ))
    }

    fn from_selected_paths(selected_paths: Vec<PathBuf>, commits_scanned: usize) -> Self {
        let mut selected_paths = selected_paths.into_iter();
        let paths = DEFAULT_COUNTS
            .iter()
            .map(|count| selected_paths.by_ref().take(*count).collect())
            .collect();
        Self {
            paths,
            commits_scanned,
        }
    }

    fn path_count(&self) -> usize {
        self.paths.iter().map(Vec::len).sum()
    }
}

fn regular_file_inode_in_working_tree(repo: &Path, path: &Path) -> Option<u64> {
    fs::symlink_metadata(repo.join(path))
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.ino())
}

fn randomized_history_commits(repo: &Path, seed: u64) -> Result<Vec<Vec<u8>>, String> {
    let output = command_output(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("rev-list")
            .arg("HEAD"),
        "list historical commits",
    )?;
    let mut commits: Vec<_> = output
        .split(|byte| *byte == b'\n')
        .filter(|commit| !commit.is_empty())
        .map(|commit| (history_order_key(seed, commit), commit.to_vec()))
        .collect();
    commits.sort_unstable_by(|left, right| left.cmp(right));
    Ok(commits.into_iter().map(|(_, commit)| commit).collect())
}

fn history_order_key(seed: u64, commit: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"changed-paths-benchmark-history\0");
    digest.update(seed.to_be_bytes());
    digest.update(commit);
    digest.finalize().into()
}

fn changed_paths_for_commits(repo: &Path, commits: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("diff-tree")
        .arg("--stdin")
        .arg("--root")
        .arg("-m")
        .arg("-r")
        .arg("--no-commit-id")
        .arg("--name-only")
        .arg("-z")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start historical changed-path extraction: {error}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "historical changed-path extraction has no stdin".to_owned())?;
        for commit in commits {
            stdin
                .write_all(commit)
                .and_then(|()| stdin.write_all(b"\n"))
                .map_err(|error| format!("write historical commit input: {error}"))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for historical changed-path extraction: {error}"))?;
    if !output.status.success() {
        return Err(command_failure(
            "extract historical changed paths",
            &output.stdout,
            &output.stderr,
        ));
    }
    Ok(output.stdout)
}

fn safe_relative_path(raw_path: &[u8]) -> Option<PathBuf> {
    let path = PathBuf::from(OsString::from_vec(raw_path.to_vec()));
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return None;
    }
    Some(path)
}

fn command_output(command: &mut Command, context: &str) -> Result<Vec<u8>, String> {
    let output = command
        .output()
        .map_err(|error| format!("{context}: {error}"))?;
    if !output.status.success() {
        return Err(command_failure(context, &output.stdout, &output.stderr));
    }
    Ok(output.stdout)
}

fn command_failure(context: &str, stdout: &[u8], stderr: &[u8]) -> String {
    format!(
        "{context} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_selected_paths_into_exact_disjoint_cohorts() {
        let selected_paths = (0..DEFAULT_COUNTS.iter().sum())
            .map(|index| PathBuf::from(format!("path-{index}")))
            .collect();
        let cohorts = HistoryPathCohorts::from_selected_paths(selected_paths, 42);

        assert_eq!(cohorts.commits_scanned, 42);
        assert_eq!(cohorts.path_count(), DEFAULT_COUNTS.iter().sum());
        assert_eq!(
            cohorts.paths.iter().map(Vec::len).collect::<Vec<_>>(),
            DEFAULT_COUNTS
        );
        let unique_paths: HashSet<_> = cohorts.paths.iter().flatten().collect();
        assert_eq!(unique_paths.len(), cohorts.path_count());
    }

    #[test]
    fn accepts_only_safe_relative_history_paths() {
        assert_eq!(
            safe_relative_path(b"project/example/file.rs"),
            Some(PathBuf::from("project/example/file.rs"))
        );
        assert!(safe_relative_path(b"").is_none());
        assert!(safe_relative_path(b"/absolute").is_none());
        assert!(safe_relative_path(b"../outside").is_none());
    }
}

fn write_inodes(file: &File, inodes: &[u64]) -> Result<(), String> {
    let mut file = file;
    for ino in inodes {
        file.write_all(&ino.to_be_bytes())
            .map_err(|error| format!("write inode input: {error}"))?;
    }
    Ok(())
}

fn read_file(file: &File) -> Result<Vec<u8>, String> {
    let mut file = file;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read benchmark output: {error}"))?;
    Ok(bytes)
}

fn parse_reverse_output(bytes: &[u8]) -> Result<BTreeMap<u64, Vec<Vec<u8>>>, String> {
    let header_len = usize::from(INODE_PATH_OUTPUT_HEADER_SIZE);
    if bytes.len() < header_len || bytes.get(..4) != Some(INODE_PATH_OUTPUT_MAGIC.as_slice()) {
        return Err("reverse output has invalid header".to_owned());
    }
    if take_u16(bytes, 4)? != INODE_PATH_OUTPUT_VERSION
        || usize::from(take_u16(bytes, 6)?) != header_len
    {
        return Err("reverse output has unsupported version or header length".to_owned());
    }
    let inode_count = usize::try_from(take_u64(bytes, 8)?)
        .map_err(|_| "reverse inode count exceeds usize".to_owned())?;
    let expected_paths = take_u64(bytes, 16)?;
    let mut cursor = header_len;
    let mut paths = BTreeMap::new();
    let mut actual_paths = 0_u64;
    for _ in 0..inode_count {
        let ino = take_u64(bytes, cursor)?;
        cursor += 8;
        let count = usize::try_from(take_u32(bytes, cursor)?)
            .map_err(|_| "reverse path count exceeds usize".to_owned())?;
        cursor += 4;
        let mut inode_paths = Vec::with_capacity(count);
        for _ in 0..count {
            let length = usize::try_from(take_u32(bytes, cursor)?)
                .map_err(|_| "reverse path length exceeds usize".to_owned())?;
            cursor += 4;
            let end = cursor
                .checked_add(length)
                .ok_or_else(|| "reverse path range overflow".to_owned())?;
            let path = bytes
                .get(cursor..end)
                .ok_or_else(|| "reverse output has truncated path".to_owned())?
                .to_vec();
            cursor = end;
            inode_paths.push(path);
            actual_paths += 1;
        }
        if paths.insert(ino, inode_paths).is_some() {
            return Err("reverse output contains duplicate inode".to_owned());
        }
    }
    if cursor != bytes.len() || actual_paths != expected_paths {
        return Err("reverse output has trailing bytes or mismatched path count".to_owned());
    }
    Ok(paths)
}

fn parse_manifest(bytes: &[u8]) -> Result<ChangedObjectsManifest, String> {
    if bytes.starts_with(CHANGED_OBJECTS_V2_MAGIC) {
        parse_changed_objects_v2(bytes)
            .map(|parsed| parsed.manifest)
            .map_err(|error| error.to_string())
    } else {
        parse_changed_objects(bytes).map_err(|error| error.to_string())
    }
}

fn take_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "reverse output is truncated".to_owned())?;
    Ok(u16::from_be_bytes(value.try_into().expect("fixed slice")))
}

fn take_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "reverse output is truncated".to_owned())?;
    Ok(u32::from_be_bytes(value.try_into().expect("fixed slice")))
}

fn take_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| "reverse output is truncated".to_owned())?;
    Ok(u64::from_be_bytes(value.try_into().expect("fixed slice")))
}

fn print_header() {
    eprintln!("inode_ref_ioctl_batch_size={INODE_REF_IOCTL_BATCH_SIZE}");
    eprintln!(
        "changed_files,repeat,changed_objects_us,reverse_lookup_us,manifest_objects,resolved_paths,manifest_bytes,reverse_output_bytes"
    );
}

fn print_row(row: &TimingRow) {
    eprintln!(
        "{},{},{},{},{},{},{},{}",
        row.changed_files,
        row.repeat,
        row.changed_objects_us,
        row.reverse_lookup_us,
        row.manifest_objects,
        row.resolved_paths,
        row.manifest_bytes,
        row.reverse_output_bytes,
    );
}

fn now_ns() -> Result<i64, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read current time: {error}"))?;
    i64::try_from(duration.as_nanos()).map_err(|_| "current time exceeds i64 ns".to_owned())
}

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    managed: PathBuf,
    spool: PathBuf,
    manager_db: PathBuf,
    scratch: PathBuf,
}

impl Fixture {
    fn create(parent: &Path, history_repo: &Path) -> Result<Self, String> {
        if !parent.is_dir() {
            return Err(format!(
                "fixture root {} is not a directory",
                parent.display()
            ));
        }
        let root = parent.join(format!("changed-paths-benchmark-{}", Uuid::new_v4()));
        create_private_dir(&root)?;
        let source = root.join("source");
        snapshot_subvolume(history_repo, &source)?;
        let managed = root.join("managed");
        let spool = root.join("spool");
        let scratch = root.join("scratch");
        create_private_dir(&managed)?;
        create_private_dir(&spool)?;
        create_private_dir(&scratch)?;
        Ok(Self {
            manager_db: root.join("manager.sqlite3"),
            root,
            source,
            managed,
            spool,
            scratch,
        })
    }

    fn private_file(&self, kind: &str) -> Result<File, String> {
        let path = self.scratch.join(format!("{kind}-{}", Uuid::new_v4()));
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("create private benchmark file: {error}"))
    }

    fn cleanup(&self) -> Result<(), String> {
        let mut subvolumes = Vec::new();
        collect_subvolumes(&self.root, &mut subvolumes)?;
        subvolumes.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in subvolumes {
            run_btrfs(&["subvolume", "delete"], &path)?;
        }
        fs::remove_dir_all(&self.root)
            .map_err(|error| format!("remove fixture {}: {error}", self.root.display()))
    }
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("create private directory {}: {error}", path.display()))
}

fn collect_subvolumes(path: &Path, subvolumes: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(path).map_err(|error| format!("read {}: {error}", path.display()))? {
        let entry = entry.map_err(|error| format!("read {} entry: {error}", path.display()))?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)
            .map_err(|error| format!("stat {}: {error}", child.display()))?;
        if !metadata.is_dir() {
            continue;
        }
        if metadata.ino() == ROOT_INODE {
            subvolumes.push(child);
        } else {
            collect_subvolumes(&child, subvolumes)?;
        }
    }
    Ok(())
}

fn run_btrfs(arguments: &[&str], path: &Path) -> Result<(), String> {
    let output = Command::new("btrfs")
        .args(arguments)
        .arg(path)
        .output()
        .map_err(|error| format!("run btrfs for {}: {error}", path.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "btrfs {} {} failed: {}",
            arguments.join(" "),
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn snapshot_subvolume(source: &Path, destination: &Path) -> Result<(), String> {
    let output = Command::new("btrfs")
        .args(["subvolume", "snapshot"])
        .arg(source)
        .arg(destination)
        .output()
        .map_err(|error| format!("snapshot {}: {error}", source.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "btrfs subvolume snapshot {} {} failed: {}",
            source.display(),
            destination.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}
