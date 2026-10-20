// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Measures the actual compact TreeState snapshot with a deterministic exact
//! delta and a temporary Git store. See ../benches/awacs_snapshot_benchmark.md.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use futures::AsyncReadExt as _;
use futures::StreamExt as _;
use jj_lib::backend::CopyId;
use jj_lib::backend::TreeValue;
use jj_lib::fsmonitor::FsmonitorSettings;
use jj_lib::local_working_copy::TreeState;
use jj_lib::local_working_copy::TreeStateSettings;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::protos::local_working_copy::AwacsSnapshotBaseline;
use jj_lib::protos::local_working_copy::WorkingCopyState;
use jj_lib::protos::local_working_copy::WorkingCopyStatePhase;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::store::Store;
use jj_lib::working_copy::SnapshotOptions;
use pollster::FutureExt as _;
use prost::Message as _;
use sha2::Digest as _;
use sha2::Sha256;
use testutils::TestRepo;
use testutils::TestRepoBackend;
use testutils::empty_snapshot_options;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const JOURNAL_MAGIC: &[u8] = b"\0JJ-WORKING-COPY-STATE\0v1\n";
const INPUT_FINGERPRINT: [u8; 32] = [3; 32];
const IGNORE_CONTENT: &[u8] =
    b"__awacs_bench_ignored__/\n*.awacs-bench-ignored\n!keep.awacs-bench-ignored\n";

struct Options {
    inventory: Option<PathBuf>,
    ignore_paths: Option<PathBuf>,
    files: usize,
    file_bytes: usize,
    seed: u64,
    files_only: bool,
}

fn options() -> Result<Options> {
    let mut result = Options {
        inventory: None,
        ignore_paths: None,
        files: 10_000,
        file_bytes: 20_480,
        seed: 17,
        files_only: false,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" {
            println!(
                "awacs_snapshot_benchmark [--inventory TSV] [--ignore-paths JSON] [--files N] \
                 [--bytes N] [--seed N] [--mode all|files-only]\n\nTSV records are f, d, s, or x \
                 followed by a tab and a relative path.\nSynthetic fixture setup and content \
                 verification are outside snapshot timing."
            );
            std::process::exit(0);
        }
        let value = args.next().ok_or("option requires a value")?;
        match arg.as_str() {
            "--inventory" => result.inventory = Some(value.into()),
            "--ignore-paths" => result.ignore_paths = Some(value.into()),
            "--files" => result.files = value.parse()?,
            "--bytes" => result.file_bytes = value.parse()?,
            "--seed" => result.seed = value.parse()?,
            "--mode" => {
                result.files_only = match value.as_str() {
                    "all" => false,
                    "files-only" => true,
                    _ => return Err("mode must be all or files-only".into()),
                };
            }
            _ => return Err(format!("unknown option: {arg}").into()),
        }
    }
    if result.files == 0 || result.file_bytes < 128 {
        return Err("files must be positive and bytes must be at least 128".into());
    }
    Ok(result)
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\t', '\r', '\n', '\0'])
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".." | ".git" | ".jj"))
    {
        return Err("inventory contains an unsafe or reserved path".into());
    }
    RepoPathBuf::from_internal_string(path)?;
    Ok(())
}

fn parent_paths(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(index, _)| &path[..index])
}

fn inventory(options: &Options) -> Result<BTreeMap<String, char>> {
    if let Some(path) = &options.inventory {
        let mut result = BTreeMap::new();
        for line in fs::read_to_string(path)?.lines() {
            let (kind, path) = line.split_once('\t').ok_or("invalid inventory record")?;
            validate_path(path)?;
            let kind = match kind {
                "f" => 'f',
                "d" => 'd',
                "s" => 's',
                "x" => 'x',
                _ => return Err("inventory type must be f, d, s, or x".into()),
            };
            if result.insert(path.to_owned(), kind).is_some() {
                return Err("inventory contains duplicate paths".into());
            }
        }
        return Ok(result);
    }
    let mut result = BTreeMap::new();
    for index in 0..options.files {
        let mut group = index / 32;
        let mut path = String::new();
        for level in 0..5 {
            path.push_str(&format!("d{level}_{:02}/", group % 4));
            group /= 4;
        }
        path.push_str(&format!("file_{index:08}.txt"));
        for parent in parent_paths(&path) {
            result.insert(parent.to_owned(), 'd');
        }
        result.insert(path, 'f');
        if index % 32 == 0 {
            result.insert(format!("deleted/group_{index:08}/file.txt"), 'x');
            result.insert(format!("deleted/group_{index:08}"), 'x');
            result.insert("deleted".to_owned(), 'x');
        }
    }
    Ok(result)
}

fn hash(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn put_bytes(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn payload(path: &str, phase: u8, options: &Options) -> Vec<u8> {
    if path == ".gitignore" || path.ends_with("/.gitignore") {
        return IGNORE_CONTENT.to_vec();
    }
    let path_hash = hash(path.as_bytes());
    let mut state = u64::from_le_bytes(path_hash[..8].try_into().unwrap()) ^ options.seed;
    let length = options.file_bytes / 2 + state as usize % (options.file_bytes + 1);
    let mut result = Vec::with_capacity(length + 128);
    let mut line = 0;
    while result.len() < length {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        result.extend_from_slice(
            format!(
                "const item_{line:06} = fixture({phase}, 0x{state:016x}, \"deterministic varied \
                 source contents\");\n"
            )
            .as_bytes(),
        );
        line += 1;
    }
    result.truncate(length);
    result
}

#[derive(Clone)]
enum Expected {
    File { hash: [u8; 32], bytes: u64 },
    Symlink(String),
}

impl Expected {
    fn bytes(&self) -> u64 {
        match self {
            Self::File { bytes, .. } => *bytes,
            Self::Symlink(target) => target.len() as u64,
        }
    }
}

fn add_file(
    builder: &mut MergedTreeBuilder,
    store: &Arc<Store>,
    path: &str,
    contents: &[u8],
) -> Result<()> {
    let path = RepoPathBuf::from_internal_string(path)?;
    let mut reader = contents;
    let id = store.write_file(&path, &mut reader).block_on()?;
    builder.set_or_remove(
        path,
        Some(TreeValue::File {
            id,
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    Ok(())
}

struct Fixture {
    base_tree: MergedTree,
    expected_tree: MergedTree,
    expected: BTreeMap<String, Expected>,
    exact: Vec<PathBuf>,
    fingerprint: String,
    exact_fingerprint: String,
    ignore_layout: &'static str,
    ignore_layout_fingerprint: String,
    regular_files: usize,
    deleted_files: usize,
    ignore_files: usize,
}

fn setup_fixture(
    options: &Options,
    inventory: &BTreeMap<String, char>,
    store: &Arc<Store>,
    scan_root: &Path,
) -> Result<Fixture> {
    let ignore_paths = options
        .ignore_paths
        .as_ref()
        .map(|path| -> Result<BTreeSet<String>> {
            let paths: Vec<String> = serde_json::from_slice(&fs::read(path)?)?;
            let mut result = BTreeSet::new();
            for path in paths {
                validate_path(&path)?;
                if path != ".gitignore" && !path.ends_with("/.gitignore") {
                    return Err("ignore placement paths must name .gitignore files".into());
                }
                if !result.insert(path) {
                    return Err("ignore placement list contains duplicate paths".into());
                }
            }
            Ok(result)
        })
        .transpose()?;
    let ignore_layout = if ignore_paths.is_some() {
        "provided-paths"
    } else {
        "all-directories"
    };
    let mut ignore_layout_hash = Sha256::new();
    put_bytes(&mut ignore_layout_hash, ignore_layout.as_bytes());
    if let Some(paths) = &ignore_paths {
        for path in paths {
            put_bytes(&mut ignore_layout_hash, path.as_bytes());
        }
    }
    let ignore_layout_fingerprint = format!("{:x}", ignore_layout_hash.finalize());
    let mut directories = BTreeSet::from([String::new()]);
    let mut old_directories = BTreeSet::new();
    for (path, kind) in inventory {
        if *kind != 'x' {
            directories.extend(parent_paths(path).map(str::to_owned));
            if *kind == 'd' {
                directories.insert(path.clone());
            }
        } else {
            old_directories.extend(parent_paths(path).map(str::to_owned));
        }
    }
    // A reported absent path with a present descendant is a present directory
    // omitted from the optional topology inventory, not a removed file.
    let kind = |path: &str, original: char| {
        if original == 'x' && directories.contains(path) {
            'd'
        } else {
            original
        }
    };
    let mut base = MergedTreeBuilder::new(store.empty_merged_tree());
    let mut target = MergedTreeBuilder::new(store.empty_merged_tree());
    let mut expected = BTreeMap::new();
    let mut fixture_hash = Sha256::new();
    let mut exact_hash = Sha256::new();
    put_bytes(&mut fixture_hash, b"awacs-snapshot-fixture-v1");
    put_bytes(&mut fixture_hash, ignore_layout_fingerprint.as_bytes());
    fixture_hash.update(options.seed.to_le_bytes());
    fixture_hash.update((options.file_bytes as u64).to_le_bytes());
    let mut exact = Vec::new();
    let mut regular_files = 0;
    let mut deleted_files = 0;
    for (index, (path, original)) in inventory.iter().enumerate() {
        let kind = kind(path, *original);
        put_bytes(&mut fixture_hash, path.as_bytes());
        fixture_hash.update([kind as u8]);
        // Missing ancestor boundaries remain authoritative even in files-only
        // mode; only present directories can be omitted from the exact delta.
        if !options.files_only || kind != 'd' {
            exact.push(PathBuf::from(path));
            put_bytes(&mut exact_hash, path.as_bytes());
        }
        let repo_path = RepoPathBuf::from_internal_string(path)?;
        let disk_path = scan_root.join(path);
        match kind {
            'd' => fs::create_dir_all(&disk_path)?,
            'f' => {
                let new_contents = payload(path, u8::from(index % 8 != 0), options);
                if !old_directories.contains(path) && index % 16 != 0 {
                    add_file(&mut base, store, path, &payload(path, 0, options))?;
                }
                add_file(&mut target, store, path, &new_contents)?;
                fs::create_dir_all(disk_path.parent().unwrap())?;
                fs::write(&disk_path, &new_contents)?;
                expected.insert(
                    path.clone(),
                    Expected::File {
                        hash: hash(&new_contents),
                        bytes: new_contents.len() as u64,
                    },
                );
                regular_files += 1;
            }
            's' => {
                let target_name = "awacs-benchmark-missing-symlink-target";
                let id = store.write_symlink(&repo_path, target_name).block_on()?;
                if !old_directories.contains(path) {
                    base.set_or_remove(repo_path.clone(), Some(TreeValue::Symlink(id.clone())));
                }
                target.set_or_remove(repo_path, Some(TreeValue::Symlink(id)));
                fs::create_dir_all(disk_path.parent().unwrap())?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(target_name, &disk_path)?;
                #[cfg(not(unix))]
                return Err("symlink fixture records require Unix".into());
                expected.insert(path.clone(), Expected::Symlink(target_name.to_owned()));
            }
            'x' if !old_directories.contains(path) => {
                add_file(&mut base, store, path, &payload(path, 0, options))?;
                deleted_files += 1;
            }
            'x' => {}
            _ => unreachable!(),
        }
    }
    for directory in directories {
        fs::create_dir_all(scan_root.join(&directory))?;
        let path = if directory.is_empty() {
            ".gitignore".to_owned()
        } else {
            format!("{directory}/.gitignore")
        };
        // A provided placement list controls only supplemental ignore files.
        // Exact inventory .gitignore files above remain normal fixture files.
        if ignore_paths
            .as_ref()
            .is_some_and(|paths| !paths.contains(&path))
        {
            continue;
        }
        // Do not create a control file where the replay records a directory,
        // symlink, or deletion named .gitignore.
        if inventory.get(&path).is_some_and(|kind| *kind != 'f') {
            continue;
        }
        add_file(&mut base, store, &path, IGNORE_CONTENT)?;
        add_file(&mut target, store, &path, IGNORE_CONTENT)?;
        fs::write(scan_root.join(&path), IGNORE_CONTENT)?;
        expected.insert(
            path,
            Expected::File {
                hash: hash(IGNORE_CONTENT),
                bytes: IGNORE_CONTENT.len() as u64,
            },
        );
    }
    let ignore_files = expected
        .iter()
        .filter(|(path, value)| {
            (path.as_str() == ".gitignore" || path.ends_with("/.gitignore"))
                && matches!(value, Expected::File { .. })
        })
        .count();
    Ok(Fixture {
        base_tree: base.write_tree().block_on()?,
        expected_tree: target.write_tree().block_on()?,
        expected,
        exact,
        fingerprint: format!("{:x}", fixture_hash.finalize()),
        exact_fingerprint: format!("{:x}", exact_hash.finalize()),
        ignore_layout,
        ignore_layout_fingerprint,
        regular_files,
        deleted_files,
        ignore_files,
    })
}

fn seed_baseline(state_path: &Path) -> Result<()> {
    let bytes = fs::read(state_path.join("working_copy_state"))?;
    let mut journal = WorkingCopyState::decode(
        bytes
            .strip_prefix(JOURNAL_MAGIC)
            .ok_or("missing compact journal header")?,
    )?;
    journal.phase = WorkingCopyStatePhase::CleanBaseline as i32;
    journal.baseline = Some(AwacsSnapshotBaseline {
        filesystem_uuid: vec![1; 16],
        subvolume_uuid: vec![2; 16],
        continuity_token: b"benchmark-a".to_vec(),
        retention_token: b"benchmark-a".to_vec(),
        interpretation_input_fingerprint: INPUT_FINGERPRINT.to_vec(),
    });
    let mut bytes = JOURNAL_MAGIC.to_vec();
    journal.encode(&mut bytes)?;
    fs::write(state_path.join("working_copy_state"), bytes)?;
    fs::write(state_path.join("subvolume_mode"), b"snapshot-backed\n")?;
    Ok(())
}

#[derive(Default)]
struct Usage {
    user_seconds: f64,
    system_seconds: f64,
    minor_faults: i64,
    major_faults: i64,
    voluntary_switches: i64,
    involuntary_switches: i64,
    rchar: u64,
    read_bytes: u64,
}

fn usage() -> Result<Usage> {
    let mut result = Usage::default();
    #[cfg(unix)]
    {
        let mut value = std::mem::MaybeUninit::<nix::libc::rusage>::uninit();
        // SAFETY: getrusage initializes the correctly sized output on success.
        let status = unsafe { nix::libc::getrusage(nix::libc::RUSAGE_SELF, value.as_mut_ptr()) };
        if status != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: the successful getrusage call initialized value.
        let value = unsafe { value.assume_init() };
        result.user_seconds = value.ru_utime.tv_sec as f64 + value.ru_utime.tv_usec as f64 / 1e6;
        result.system_seconds = value.ru_stime.tv_sec as f64 + value.ru_stime.tv_usec as f64 / 1e6;
        result.minor_faults = value.ru_minflt as i64;
        result.major_faults = value.ru_majflt as i64;
        result.voluntary_switches = value.ru_nvcsw as i64;
        result.involuntary_switches = value.ru_nivcsw as i64;
    }
    if let Ok(contents) = fs::read_to_string("/proc/self/io") {
        for line in contents.lines() {
            if let Some((key, value)) = line.split_once(':') {
                match key {
                    "rchar" => result.rchar = value.trim().parse()?,
                    "read_bytes" => result.read_bytes = value.trim().parse()?,
                    _ => {}
                }
            }
        }
    }
    Ok(result)
}

fn validate_contents(tree: &MergedTree, expected: &BTreeMap<String, Expected>) -> Result<String> {
    let mut actual_files = 0;
    let mut digest = Sha256::new();
    let mut entries = tree.entries_stream();
    while let Some((path, value)) = entries.next().block_on() {
        let value = value?;
        let name = path.as_internal_file_string();
        let expected = expected.get(name).ok_or("unexpected result-tree path")?;
        put_bytes(&mut digest, name.as_bytes());
        match (value.as_normal(), expected) {
            (
                Some(TreeValue::File {
                    id,
                    executable: false,
                    ..
                }),
                Expected::File {
                    hash: expected_hash,
                    bytes,
                },
            ) => {
                let mut reader = tree.store().read_file(&path, id).block_on()?;
                let mut content = Vec::new();
                reader.read_to_end(&mut content).block_on()?;
                if content.len() as u64 != *bytes || hash(&content) != *expected_hash {
                    return Err("result blob does not match deterministic target bytes".into());
                }
                digest.update(expected_hash);
            }
            (Some(TreeValue::Symlink(id)), Expected::Symlink(expected_target)) => {
                let target = tree.store().read_symlink(&path, id).block_on()?;
                if &target != expected_target {
                    return Err("result symlink does not match target".into());
                }
                put_bytes(&mut digest, target.as_bytes());
            }
            _ => return Err("result tree has an unexpected type, mode, or conflict".into()),
        }
        actual_files += 1;
    }
    if actual_files != expected.len() {
        return Err("result tree is missing expected paths".into());
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn main() -> Result<()> {
    let options = options()?;
    let setup_started = Instant::now();
    let inventory = inventory(&options)?;
    if inventory.is_empty() {
        return Err("inventory is empty".into());
    }
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("immutable-scan-fixture");
    let state_path = test_repo.env.root().join("state");
    for path in [&workspace_root, &scan_root, &state_path] {
        fs::create_dir(path)?;
    }
    let fixture = setup_fixture(&options, &inventory, repo.store(), &scan_root)?;
    fs::write(state_path.join("subvolume_mode"), b"enabling\n")?;
    let base_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    let mut tree_state = TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &base_settings,
    )?;
    tree_state.reset(&fixture.base_tree).block_on()?;
    tree_state.save()?;
    seed_baseline(&state_path)?;
    let exact_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root,
            changed_files: Some(fixture.exact.clone()),
            cursor: b"benchmark-b".to_vec(),
        },
        ..base_settings
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root,
        state_path,
        &exact_settings,
    )?;
    let indexes: HashMap<_, _> = fixture
        .expected
        .keys()
        .enumerate()
        .map(|(i, path)| (path.as_str(), i))
        .collect();
    let visits: Vec<_> = (0..indexes.len()).map(|_| AtomicU64::new(0)).collect();
    let unexpected_visits = AtomicU64::new(0);
    let progress = |path: &RepoPath| {
        if let Some(index) = indexes.get(path.as_internal_file_string()) {
            visits[*index].fetch_add(1, Ordering::Relaxed);
        } else {
            unexpected_visits.fetch_add(1, Ordering::Relaxed);
        }
    };
    let snapshot_options = SnapshotOptions {
        progress: Some(&progress),
        awacs_input_fingerprint: Some(INPUT_FINGERPRINT),
        ..empty_snapshot_options()
    };
    let setup_seconds = setup_started.elapsed().as_secs_f64();
    let before = usage()?;
    let snapshot_started = Instant::now();
    let (_, stats) = tree_state.snapshot(&snapshot_options).block_on()?;
    let snapshot_seconds = snapshot_started.elapsed().as_secs_f64();
    let after = usage()?;

    let validation_started = Instant::now();
    if tree_state.current_tree().tree_ids_and_labels()
        != fixture.expected_tree.tree_ids_and_labels()
    {
        return Err(
            "snapshot result differs from the independently constructed target tree".into(),
        );
    }
    if !stats.untracked_paths.is_empty() || unexpected_visits.load(Ordering::Relaxed) != 0 {
        return Err("snapshot unexpectedly skipped or processed fixture paths".into());
    }
    let content_hash = validate_contents(tree_state.current_tree(), &fixture.expected)?;
    let mut total_visits = 0;
    let mut unique_visits = 0;
    let mut max_visits = 0;
    let mut processed_bytes = 0;
    let mut covered_hash = Sha256::new();
    let mut visit_hash = Sha256::new();
    for ((path, expected), count) in fixture.expected.iter().zip(&visits) {
        let count = count.load(Ordering::Relaxed);
        total_visits += count;
        unique_visits += u64::from(count != 0);
        max_visits = max_visits.max(count);
        processed_bytes += count * expected.bytes();
        if count != 0 {
            put_bytes(&mut covered_hash, path.as_bytes());
        }
        put_bytes(&mut visit_hash, path.as_bytes());
        visit_hash.update(count.to_le_bytes());
    }
    let validation_seconds = validation_started.elapsed().as_secs_f64();
    let tree_id = |tree: &MergedTree| tree.tree_ids().as_resolved().unwrap().hex();
    println!(
        concat!(
            "{{\"schema\":\"awacs-snapshot-benchmark-v1\",\"mode\":\"{}\",",
            "\"fixture_sha256\":\"{}\",\"exact_paths_sha256\":\"{}\",",
            "\"ignore_layout\":\"{}\",\"ignore_layout_sha256\":\"{}\",",
            "\"base_tree\":\"{}\",\"expected_tree\":\"{}\",\"result_tree\":\"{}\",",
            "\"validated_content_sha256\":\"{}\",\"covered_paths_sha256\":\"{:x}\",",
            "\"visit_counts_sha256\":\"{:x}\",\"regular_files\":{},\"deleted_files\":{},",
            "\"ignore_files\":{},\"target_files\":{},\"exact_paths\":{},",
            "\"setup_seconds\":{:.9},\"snapshot_seconds\":{:.9},\"validation_seconds\":{:.9},",
            "\"snapshot_user_seconds\":{:.9},\"snapshot_system_seconds\":{:.9},",
            "\"snapshot_minor_faults\":{},\"snapshot_major_faults\":{},",
            "\"snapshot_voluntary_switches\":{},\"snapshot_involuntary_switches\":{},",
            "\"snapshot_rchar\":{},\"snapshot_read_bytes\":{},",
            "\"file_visits\":{},\"unique_file_visits\":{},\"max_file_visits\":{},",
            "\"processed_file_bytes\":{},\"seed\":{},\"mean_file_bytes_parameter\":{}}}"
        ),
        if options.files_only {
            "files-only"
        } else {
            "all"
        },
        fixture.fingerprint,
        fixture.exact_fingerprint,
        fixture.ignore_layout,
        fixture.ignore_layout_fingerprint,
        tree_id(&fixture.base_tree),
        tree_id(&fixture.expected_tree),
        tree_id(tree_state.current_tree()),
        content_hash,
        covered_hash.finalize(),
        visit_hash.finalize(),
        fixture.regular_files,
        fixture.deleted_files,
        fixture.ignore_files,
        fixture.expected.len(),
        fixture.exact.len(),
        setup_seconds,
        snapshot_seconds,
        validation_seconds,
        after.user_seconds - before.user_seconds,
        after.system_seconds - before.system_seconds,
        after.minor_faults - before.minor_faults,
        after.major_faults - before.major_faults,
        after.voluntary_switches - before.voluntary_switches,
        after.involuntary_switches - before.involuntary_switches,
        after.rchar.saturating_sub(before.rchar),
        after.read_bytes.saturating_sub(before.read_bytes),
        total_visits,
        unique_visits,
        max_visits,
        processed_bytes,
        options.seed,
        options.file_bytes,
    );
    Ok(())
}
