// Copyright 2022 The Jujutsu Authors
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

use indoc::indoc;
use test_case::test_case;
use testutils::TestResult;
use testutils::git;

use crate::common::CommandOutput;
use crate::common::TestEnvironment;
use crate::common::TestWorkDir;

#[cfg(unix)]
pub(super) fn install_fake_btrfs(test_env: &TestEnvironment, script: &str) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt as _;

    let bin_dir = test_env.env_root().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let btrfs_path = bin_dir.join("btrfs");
    std::fs::write(&btrfs_path, script).unwrap();
    let mut permissions = std::fs::metadata(&btrfs_path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&btrfs_path, permissions).unwrap();
    let mut paths = vec![bin_dir];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).unwrap()
}

/// Owns real-Btrfs fixture paths until a test either finishes normally or
/// unwinds through an assertion. Failed integration tests must not leave
/// read-only snapshots or converted repository subvolumes in the checkout.
#[cfg(all(target_os = "linux", feature = "awacs"))]
struct RealBtrfsFixtureCleanup {
    subvolume_roots: Vec<std::path::PathBuf>,
    awacs_state: std::path::PathBuf,
    runtime_dir: std::path::PathBuf,
    armed: bool,
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
impl RealBtrfsFixtureCleanup {
    fn new(
        repo_root: std::path::PathBuf,
        awacs_state: std::path::PathBuf,
        runtime_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            subvolume_roots: vec![repo_root],
            awacs_state,
            runtime_dir,
            armed: true,
        }
    }

    fn track_subvolume_root(&mut self, root: std::path::PathBuf) {
        self.subvolume_roots.push(root);
    }

    fn cleanup(&mut self) -> std::io::Result<()> {
        self.remove_managed_snapshots()?;
        self.remove_migration_siblings()?;
        for root in self.subvolume_roots.iter().rev() {
            remove_fixture_tree(root)?;
        }
        remove_plain_tree(&self.awacs_state)?;
        remove_plain_tree(&self.runtime_dir)?;
        self.armed = false;
        Ok(())
    }

    fn remove_managed_snapshots(&self) -> std::io::Result<()> {
        let managed = self.awacs_state.join("managed");
        if !managed.exists() {
            return Ok(());
        }
        let mut snapshots = Vec::new();
        for directory in std::fs::read_dir(&managed)? {
            let directory = directory?.path();
            if !directory.is_dir() {
                continue;
            }
            if directory
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".broker-trash-"))
            {
                // The broker owns these wrappers (and therefore they may not
                // be traversable by the test user), but once its asynchronous
                // drain has removed the nested snapshot they are empty and
                // can be unlinked through the test-owned parent directory.
                std::fs::remove_dir(&directory)?;
                continue;
            }
            for snapshot in std::fs::read_dir(directory)? {
                let snapshot = snapshot?.path();
                if snapshot
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("cut-"))
                {
                    let wrapped_snapshot = snapshot.join("snapshot");
                    snapshots.push(if wrapped_snapshot.exists() {
                        wrapped_snapshot
                    } else {
                        snapshot
                    });
                }
            }
        }
        let mut first_error = None;
        for snapshot in snapshots {
            if let Err(error) = remove_fixture_tree(&snapshot) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn remove_migration_siblings(&self) -> std::io::Result<()> {
        let mut first_error = None;
        for root in &self.subvolume_roots {
            let Some(parent) = root.parent() else {
                continue;
            };
            let Some(name) = root.file_name().map(|name| name.to_string_lossy()) else {
                continue;
            };
            let prefixes = [
                format!("{name}.jj-subvolume-source-"),
                format!(".{name}.jj-subvolume-init-"),
                format!(".{name}.jj-subvolume-disable-"),
                format!(".{name}.awacs-subvolume-source-"),
                format!(".{name}.awacs-subvolume-tmp-"),
            ];
            for entry in std::fs::read_dir(parent)? {
                let path = entry?.path();
                if !path.file_name().is_some_and(|entry_name| {
                    prefixes
                        .iter()
                        .any(|prefix| entry_name.to_string_lossy().starts_with(prefix))
                }) {
                    continue;
                }
                if let Err(error) = remove_fixture_tree(&path) {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
impl Drop for RealBtrfsFixtureCleanup {
    fn drop(&mut self) {
        if self.armed {
            // Cleanup during unwinding cannot report a second failure. Retry
            // every path best-effort so one stubborn subvolume does not keep
            // the remaining fixture tree alive.
            drop(self.remove_managed_snapshots());
            drop(self.remove_migration_siblings());
            for root in self.subvolume_roots.iter().rev() {
                drop(remove_fixture_tree(root));
            }
            drop(remove_plain_tree(&self.awacs_state));
            drop(remove_plain_tree(&self.runtime_dir));
        }
    }
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn default_awacs_state_root(root: &std::path::Path) -> std::path::PathBuf {
    gix::open(root)
        .expect("real Btrfs fixture must be a Git repository")
        .git_dir()
        .join("awacs")
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn assert_default_awacs_layout(root: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::MetadataExt as _;

    let state_root = default_awacs_state_root(root);
    let root_key = btrfs_awacs::bootstrap::root_state_key(root)
        .expect("real Btrfs fixture must have a stable subvolume identity");
    let state_dir = state_root.join(root_key);
    let manager = state_dir.join("manager.sqlite3");
    assert!(
        manager.is_file(),
        "manager must use the production Git-admin path: {}",
        manager.display()
    );
    let managed = state_dir.join("managed");
    assert!(
        managed.is_dir(),
        "managed snapshots must be colocated with the manager: {}",
        managed.display()
    );
    let physical_snapshots = std::fs::read_dir(&managed)
        .expect("default managed snapshot directory must be readable")
        .filter_map(Result::ok)
        .filter_map(|cut| std::fs::metadata(cut.path().join("snapshot")).ok())
        .filter(|metadata| metadata.ino() == 256)
        .count();
    assert!(
        physical_snapshots > 0,
        "the default managed directory must contain physical Btrfs snapshots, not copied \
         placeholders: {}",
        managed.display()
    );
    manager
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn assert_awacs_state_retired(
    root: &std::path::Path,
    removed_state_root: &std::path::Path,
    expected_identity: &jj_lib::file_util::FileIdentity,
) -> std::path::PathBuf {
    use std::os::unix::fs::MetadataExt as _;

    let admin_dir = removed_state_root.parent().unwrap();
    assert!(
        !admin_dir.exists(),
        "removed workspace's Git administrative directory must actually be pruned: {}",
        admin_dir.display()
    );
    let retired = std::fs::read_dir(default_awacs_state_root(root))
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".retired-worktree-")
                && jj_lib::file_util::FileIdentity::from_symlink_path(entry.path()).unwrap()
                    == *expected_identity
        })
        .expect("removed workspace's AWACS directory must be retired by identity")
        .path();
    let mut physical_snapshots = 0;
    for state in std::fs::read_dir(&retired).unwrap() {
        let managed = state.unwrap().path().join("managed");
        if managed.is_dir() {
            physical_snapshots += std::fs::read_dir(managed)
                .unwrap()
                .map(Result::unwrap)
                .filter_map(|cut| std::fs::metadata(cut.path().join("snapshot")).ok())
                .filter(|metadata| metadata.ino() == 256)
                .count();
        }
    }
    assert!(
        physical_snapshots > 0,
        "retirement must preserve physical snapshots until subvolume disable"
    );
    retired
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn remove_fixture_git_snapshots(git_dir: &std::path::Path) -> std::io::Result<()> {
    let mut state_roots = vec![git_dir.join("awacs")];
    let worktrees = git_dir.join("worktrees");
    if std::fs::symlink_metadata(&worktrees).is_ok_and(|metadata| metadata.is_dir()) {
        for worktree in std::fs::read_dir(worktrees)? {
            let worktree = worktree?;
            if worktree.file_type()?.is_dir() {
                state_roots.push(worktree.path().join("awacs"));
            }
        }
    }
    while let Some(state_root) = state_roots.pop() {
        if !std::fs::symlink_metadata(&state_root).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        for state in std::fs::read_dir(state_root)? {
            let state = state?;
            if !state.file_type()?.is_dir() {
                continue;
            }
            if state
                .file_name()
                .to_string_lossy()
                .starts_with(".retired-worktree-")
            {
                state_roots.push(state.path());
                continue;
            }
            let managed = state.path().join("managed");
            if !std::fs::symlink_metadata(&managed).is_ok_and(|metadata| metadata.is_dir()) {
                continue;
            }
            for cut in std::fs::read_dir(&managed)? {
                let cut = cut?;
                if !cut.file_type()?.is_dir() {
                    continue;
                }
                let cut = cut.path();
                if cut
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".broker-trash-"))
                {
                    std::fs::remove_dir(&cut)?;
                } else {
                    remove_fixture_tree(&cut.join("snapshot"))?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn remove_fixture_tree(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return remove_plain_tree(path);
    }
    let is_subvolume = metadata.ino() == 256;
    if is_subvolume {
        drop(
            std::process::Command::new("btrfs")
                .args(["property", "set", "-ts"])
                .arg(path)
                .args(["ro", "false"])
                .status(),
        );
    }
    // Production puts managed snapshots below .git. Delete those nested
    // subvolumes before their containing metadata, including retained sources.
    // Only scan Git directories physically inside fixture-owned roots. The
    // linked-worktree state also lives here, under .git/worktrees/<id>/awacs;
    // never follow a gitfile to an unrelated repository during cleanup.
    let git_dir = path.join(".git");
    if std::fs::symlink_metadata(&git_dir).is_ok_and(|metadata| metadata.is_dir()) {
        remove_fixture_git_snapshots(&git_dir)?;
    }
    // Failed metadata conversion can leave the old nested subvolume under
    // its exact temporary name. These are children of this owned fixture,
    // never paths discovered by following a linked worktree's gitfile.
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("..git.jj-subvolume-disable-") {
            remove_fixture_git_snapshots(&entry.path())?;
            remove_fixture_tree(&entry.path())?;
        } else if name.starts_with("..jj.jj-subvolume-disable-") {
            remove_fixture_tree(&entry.path())?;
        }
    }
    if !is_subvolume {
        return remove_plain_tree(path);
    }
    // Conversion makes repository metadata separate nested subvolumes. The
    // containing snapshot must be writable before either an omitted-subvolume
    // placeholder or an ordinary copied metadata directory can be removed.
    remove_fixture_tree(&path.join(".git"))?;
    remove_fixture_tree(&path.join(".jj"))?;
    let status = std::process::Command::new("btrfs")
        .args(["subvolume", "delete"])
        .arg(path)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "delete Btrfs test subvolume {} failed with {status}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[test]
#[ignore = "requires patched Git, JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_git_snapshot_worktree_from_collected_fsmonitor_baseline() -> TestResult {
    git_snapshot_worktree_handoff(GitSnapshotHandoffScenario::CollectedCursor)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[test]
#[ignore = "requires patched Git, JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_workspace_add_reuses_git_index_from_collected_fsmonitor_baseline() -> TestResult {
    git_snapshot_worktree_handoff(GitSnapshotHandoffScenario::ColocatedJjReuse)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[test]
#[ignore = "requires patched Git, JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_workspace_add_falls_back_for_staged_git_index() -> TestResult {
    git_snapshot_worktree_handoff(GitSnapshotHandoffScenario::ColocatedJjStagedIndex)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[test]
#[ignore = "requires patched Git, JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_workspace_add_falls_back_for_empty_sparse_patterns() -> TestResult {
    git_snapshot_worktree_handoff(GitSnapshotHandoffScenario::ColocatedJjEmptySparse)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[test]
#[ignore = "requires patched Git, JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_workspace_add_falls_back_for_worktree_dependent_filter() -> TestResult {
    git_snapshot_worktree_handoff(GitSnapshotHandoffScenario::ColocatedJjFilter)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[test]
#[ignore = "requires patched Git, JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_git_snapshot_worktree_reconciles_changes_during_handoff() -> TestResult {
    git_snapshot_worktree_handoff(GitSnapshotHandoffScenario::ConcurrentChanges)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[test]
#[ignore = "requires patched Git, JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_git_snapshot_worktree_without_source_fsmonitor_cursor() -> TestResult {
    git_snapshot_worktree_handoff(GitSnapshotHandoffScenario::MissingCursor)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum GitSnapshotHandoffScenario {
    CollectedCursor,
    ColocatedJjReuse,
    ColocatedJjStagedIndex,
    ColocatedJjEmptySparse,
    ColocatedJjFilter,
    ConcurrentChanges,
    MissingCursor,
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn git_snapshot_worktree_handoff(scenario: GitSnapshotHandoffScenario) -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use btrfs_awacs::btrfs::OpenedSubvolume;
    use btrfs_awacs::status::GitFsmonitorStatus;
    use btrfs_awacs::status::RootStatus;
    use btrfs_awacs::status::git_index_checksum;
    use btrfs_awacs::status::git_index_layout;
    use btrfs_awacs::status::inspect_git_index;

    let base = std::path::PathBuf::from(
        std::env::var_os("JJ_TEST_BTRFS_ROOT").expect("JJ_TEST_BTRFS_ROOT is required"),
    );
    let awacs = std::fs::canonicalize(
        std::env::var_os("BTRFS_AWACS_COMMAND").expect("BTRFS_AWACS_COMMAND is required"),
    )?;
    let broker = std::env::var_os("BTRFS_AWACS_BROKER_SOCKET")
        .expect("BTRFS_AWACS_BROKER_SOCKET must select a matching broker");
    assert!(base.is_absolute());
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let root = base.join(format!("git-cursor-source-{suffix}"));
    let child = base.join(format!("git-cursor-child-{suffix}"));
    let state = base.join(format!(".git-cursor-state-{suffix}"));
    let runtime = std::env::temp_dir().join(format!("git-cursor-{suffix}"));
    let mut cleanup = RealBtrfsFixtureCleanup::new(root.clone(), state.clone(), runtime.clone());
    cleanup.track_subvolume_root(child.clone());
    for directory in [&root, &state, &runtime] {
        std::fs::create_dir(directory)?;
    }
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    let hook = runtime.join("git-fsmonitor-awacs");
    std::os::unix::fs::symlink(&awacs, &hook)?;
    // Patched Git finds the worktree engine through PATH, independently of
    // core.fsmonitor. Both entry points must use the selected real binary.
    std::os::unix::fs::symlink(&awacs, runtime.join("awacs"))?;
    let path = std::env::join_paths(std::iter::once(runtime.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))?;
    let command = |program: &std::ffi::OsStr, directory: &std::path::Path| {
        let mut cmd = Command::new(program);
        cmd.current_dir(directory)
            .env("PATH", &path)
            .env("BTRFS_AWACS_BROKER_DIR", &broker)
            .env("BTRFS_AWACS_BROKER_SOCKET", &broker)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("AWACS_LOG_FILE", state.join("awacs.log"))
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OPTIONAL_LOCKS")
            .env_remove("GIT_AWACS_BYPASS")
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR");
        cmd
    };
    let git_command = |directory: &std::path::Path| command(std::ffi::OsStr::new("git"), directory);
    let test_env = TestEnvironment::default();
    let jj_command = |directory: &std::path::Path| {
        let mut cmd = test_env.new_jj_cmd();
        cmd.current_dir(directory)
            .env("PATH", &path)
            .env("BTRFS_AWACS_COMMAND", &awacs)
            .env("BTRFS_AWACS_BROKER_DIR", &broker)
            .env("BTRFS_AWACS_BROKER_SOCKET", &broker)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_OPTIONAL_LOCKS", "0");
        cmd
    };
    let run = |cmd: &mut Command| -> std::io::Result<std::process::Output> {
        let output = cmd.output()?;
        assert!(
            output.status.success(),
            "{cmd:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output)
    };

    let colocated_jj = matches!(
        scenario,
        GitSnapshotHandoffScenario::ColocatedJjReuse
            | GitSnapshotHandoffScenario::ColocatedJjStagedIndex
            | GitSnapshotHandoffScenario::ColocatedJjEmptySparse
            | GitSnapshotHandoffScenario::ColocatedJjFilter
    );
    let empty_sparse = scenario == GitSnapshotHandoffScenario::ColocatedJjEmptySparse;
    let custom_filter = scenario == GitSnapshotHandoffScenario::ColocatedJjFilter;
    // Git-only scenarios intentionally have no JJ consumer, which would pin
    // a physical seed and select a different inheritance path.
    run(git_command(&root).args(["init", "--quiet", "--object-format=sha1"]))?;
    let filter_attributes = "one filter=cwd\n";
    if custom_filter {
        std::fs::write(root.join(".gitattributes"), filter_attributes)?;
        // The same configuration cleans identical bytes differently in the
        // source and child. A matching TREE alone cannot transfer clean bits.
        run(git_command(&root).args([
            "config",
            "filter.cwd.clean",
            "case \"$PWD\" in *git-cursor-source-*) cat;; *) sed s/first/filtered/;; esac",
        ]))?;
        run(git_command(&root).args(["add", ".gitattributes"]))?;
    }
    for (name, contents) in [("one", "first\n"), ("two", "second\n")] {
        std::fs::write(root.join(name), contents)?;
    }
    run(git_command(&root).args(["add", "one", "two"]))?;
    run(git_command(&root).args([
        "-c",
        "user.name=Test User",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--quiet",
        "-m",
        "initial",
    ]))?;
    let source_head = run(git_command(&root).args(["rev-parse", "HEAD"]))?.stdout;
    let source_tree = run(git_command(&root).args(["rev-parse", "HEAD^{tree}"]))?.stdout;
    if colocated_jj {
        jj_command(&root)
            .args(["git", "init", "--colocate", "."])
            .assert()
            .success();
        // Source init writes .jj/.gitignore, but workspace add installs fresh
        // metadata. Ignore that administration in every linked worktree so
        // this fixture measures cache inheritance rather than metadata noise.
        std::fs::write(root.join(".git/info/exclude"), ".jj/\n")?;
    }
    run(git_command(&root)
        .args(["config", "core.fsmonitor"])
        .arg(&hook))?;
    run(git_command(&root).args(["config", "core.fsmonitorHookVersion", "2"]))?;
    run(git_command(&root).args(["config", "core.untrackedCache", "true"]))?;
    if colocated_jj {
        jj_command(&root)
            .args(["util", "subvolume", "enable"])
            .assert()
            .success();
    } else {
        run(command(awacs.as_os_str(), &base).arg("init").arg(&root))?;
    }
    // Subvolume enable can select core.fsmonitor=true. Keep this fixture on
    // the requested AWACS hook even when that Git build interprets true as
    // its native daemon rather than selecting AWACS automatically.
    run(git_command(&root)
        .args(["config", "core.fsmonitor"])
        .arg(&hook))?;
    run(git_command(&root).args(["config", "core.fsmonitorHookVersion", "2"]))?;
    let status = run(git_command(&root).args(["status", "--porcelain=v1"]))?;
    assert!(status.stdout.is_empty());
    assert_eq!(root.join(".jj").exists(), colocated_jj);
    if colocated_jj {
        assert_eq!(
            run(git_command(&root).arg("write-tree"))?.stdout,
            source_tree,
            "the warmed index must retain a valid cache tree for its semantic source"
        );
    }
    // Establish a durable starting cursor explicitly. Only subsequent
    // commands suppress optional index writes to exercise stale-cursor replay.
    run(git_command(&root).args(["update-index", "--refresh", "--force-write-index"]))?;
    let index_path = root.join(".git/index");
    let mut original_index = std::fs::read(&index_path)?;
    let source_stat_cache = if scenario == GitSnapshotHandoffScenario::ColocatedJjReuse {
        assert!(
            git_index_layout(&original_index, 20)
                .map_err(std::io::Error::other)?
                .extension_data_range(b"UNTR")
                .is_some(),
            "the source must have a warm untracked cache before testing its inheritance"
        );
        let index = gix::open(&root)?.open_index()?;
        let entries = index
            .entries()
            .iter()
            .map(|entry| (entry.path(&index).to_vec(), entry.stat))
            .collect::<Vec<_>>();
        assert!(
            entries.iter().all(|(_, stat)| stat.size > 0),
            "the source index must contain real stat data, not a freshly reconstructed tree"
        );
        Some(entries)
    } else {
        None
    };
    let GitFsmonitorStatus::Baseline(baseline) = inspect_git_index(&index_path)?.fsmonitor else {
        panic!("bootstrap status must persist an AWACS cursor");
    };
    let manager = assert_default_awacs_layout(&root);
    let mut baseline_snapshot = None;
    for entry in std::fs::read_dir(manager.parent().unwrap().join("managed"))? {
        let snapshot = entry?.path().join("snapshot");
        if let Ok(opened) = OpenedSubvolume::open(&snapshot)
            && opened.subvolume.uuid == baseline.identity.subvolume_uuid
        {
            baseline_snapshot = Some(snapshot);
        }
    }
    let baseline_snapshot = baseline_snapshot.expect("the initial cursor's snapshot must exist");

    // Git is allowed to leave an unchanged index unwritten. Make that
    // deterministic even if racy stat data would otherwise cause a refresh;
    // fsmonitor stays enabled and performs real cuts, replay, and GC.
    for attempt in 0..2 {
        let trace_path = runtime.join(format!("status-{attempt}.jsonl"));
        let status = run(git_command(&root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TRACE2_EVENT", &trace_path)
            .args(["status", "--porcelain=v1"]))?;
        assert!(status.stdout.is_empty());
        assert!(
            std::fs::read(&index_path)? == original_index,
            "status must leave the persisted index and cursor byte-identical"
        );
        let trace = std::fs::read_to_string(trace_path)?;
        let events = trace
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert!(
            events.iter().any(|event| {
                event["category"] == "fsmonitor"
                    && event["key"] == "apply_count"
                    && event["value"] == "0"
            }),
            "status must use exact empty replay, not a failed hook or full invalidation"
        );
        assert!(!trace.contains("query/trivial-response"));
        assert!(
            !baseline_snapshot.exists(),
            "GC must collect the physical snapshot named by the unchanged cursor"
        );
        let retained = RootStatus::open(&root)?.inspect_baseline(&baseline);
        assert!(
            retained.issues.is_empty(),
            "the logical cursor must remain valid after physical collection: {:?}",
            retained.issues
        );
    }
    if colocated_jj {
        // The JJ journal must retain a different physical cut. A Git cursor
        // whose snapshot was just collected cannot double as JJ's seed.
        jj_command(&root).arg("status").assert().success();
        assert!(
            std::fs::read(&index_path)? == original_index,
            "advancing JJ must leave the stale, logically valid Git cursor unchanged"
        );
    }
    if scenario == GitSnapshotHandoffScenario::ColocatedJjStagedIndex {
        // The live files and JJ's semantic tree stay unchanged, while Git's
        // cached root TREE now describes a different staged blob. Matching
        // physical snapshots alone cannot authorize inheritance of this tree.
        let staged_file = runtime.join("staged-contents");
        std::fs::write(&staged_file, "staged contents absent from JJ\n")?;
        let staged_blob = run(git_command(&root)
            .args(["hash-object", "-w", "--no-filters"])
            .arg(&staged_file))?;
        let staged_blob = String::from_utf8(staged_blob.stdout)?;
        run(git_command(&root)
            .args(["update-index", "--cacheinfo"])
            .arg(format!("100644,{},one", staged_blob.trim())))?;
        assert_ne!(
            run(git_command(&root).arg("write-tree"))?.stdout,
            source_tree,
            "the staged source index must differ from the requested JJ tree"
        );
        original_index = std::fs::read(&index_path)?;
        assert_eq!(std::fs::read_to_string(root.join("one"))?, "first\n");
    }

    if scenario == GitSnapshotHandoffScenario::MissingCursor {
        // Simulate an index published before fsmonitor was enabled, without
        // changing the monitor configuration or issuing an unmonitored Git
        // command. Drop EOIE as well because it hashes extension headers.
        let layout = git_index_layout(&original_index, 20).map_err(std::io::Error::other)?;
        let mut ranges = [b"FSMN", b"EOIE"]
            .into_iter()
            .filter_map(|signature| layout.extension_data_range(signature))
            .collect::<Vec<_>>();
        ranges.sort_unstable_by_key(|range| std::cmp::Reverse(range.start));
        original_index.truncate(original_index.len() - 20);
        for range in ranges {
            original_index.drain(range.start - 8..range.end);
        }
        let checksum = git_index_checksum(&original_index, 20).map_err(std::io::Error::other)?;
        original_index.extend_from_slice(&checksum);
        std::fs::write(&index_path, &original_index)?;
        assert_eq!(
            inspect_git_index(&index_path)?.fsmonitor,
            GitFsmonitorStatus::Missing
        );
        std::fs::write(root.join("one"), "unobserved edit\n")?;
        std::fs::write(root.join("without-cursor"), "unobserved new file\n")?;
    }

    let race = scenario == GitSnapshotHandoffScenario::ConcurrentChanges;
    let creation_trace = runtime.join("creation.jsonl");
    let replacement_index = b"source index atomically replaced during creation\n";
    let race_marker = runtime.join("race-ran");
    let creation = if colocated_jj {
        let mut cmd = jj_command(&root);
        cmd.env("GIT_TRACE2_EVENT", &creation_trace).args([
            "workspace",
            "add",
            "--name",
            "reused-index",
        ]);
        if empty_sparse {
            cmd.arg("--sparse-patterns=empty");
        }
        cmd.arg(&child).output()?
    } else if race {
        // This edit belongs to replay from the copied index cursor to the
        // leased source cut. Optional index writes remain disabled so the
        // copied cursor and clean bitmap still predate it.
        std::fs::write(root.join("two"), "before\n")?;
        let real_git =
            run(command(std::ffi::OsStr::new("sh"), &base).args(["-c", "command -v git"]))?;
        let real_git = std::path::PathBuf::from(String::from_utf8(real_git.stdout)?.trim());
        assert!(real_git.is_absolute());
        let wrapper = runtime.join("git-during-registration");
        std::fs::write(
            &wrapper,
            r#"#!/bin/sh
set -eu
if [ "$1" = -C ] && [ "$2" = "$JJ_TEST_RACE_ROOT" ] &&
   [ "$3" = worktree ] && [ "$4" = add ] && [ "$5" = --no-checkout ]; then
    "$JJ_TEST_REAL_GIT" "$@"
    # Registration follows capture of the source index and its leased cut.
    # These changes therefore require the source-cut to child-cut bridge.
    printf 'after\n' > "$JJ_TEST_RACE_ROOT/one"
    printf 'new after source cut\n' > "$JJ_TEST_RACE_ROOT/after-cut"
    # Replacement, rather than an in-place write, models Git's publication
    # protocol. Poison bytes make any later pathname reopen observable.
    printf 'source index atomically replaced during creation\n' > "$JJ_TEST_RACE_ROOT/.git/index.next"
    mv "$JJ_TEST_RACE_ROOT/.git/index.next" "$JJ_TEST_RACE_ROOT/.git/index"
    printf ran > "$JJ_TEST_RACE_MARKER"
else
    exec "$JJ_TEST_REAL_GIT" "$@"
fi
"#,
        )?;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
        command(awacs.as_os_str(), &base)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TRACE2_EVENT", &creation_trace)
            .env("JJ_TEST_REAL_GIT", real_git)
            .env("JJ_TEST_RACE_ROOT", &root)
            .env("JJ_TEST_RACE_MARKER", &race_marker)
            .args(["git", "worktree-add", "--required", "--detach", "--git"])
            .arg(&wrapper)
            .arg("--source")
            .arg(&root)
            .arg("--destination")
            .arg(&child)
            .args(["--ref", "HEAD"])
            .output()?
    } else {
        git_command(&root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TRACE2_EVENT", &creation_trace)
            .args(["worktree", "add", "--btrfs-snapshot", "--detach"])
            .arg(&child)
            .arg("HEAD")
            .output()?
    };
    assert!(
        std::fs::read(&index_path)?
            == if race {
                replacement_index.as_slice()
            } else {
                original_index.as_slice()
            },
        "creation must neither publish the source cursor nor overwrite an intervening index writer"
    );
    assert!(
        creation.status.success(),
        "snapshot creation must reconcile the captured index with the child cut ({}):\n{}",
        creation.status,
        String::from_utf8_lossy(&creation.stderr)
    );
    // Read the child index before either frontend can repair or warm it. A
    // correct cold reconstruction can pass the GC/status checks below, but
    // it must not pass the separate warm-index inheritance requirement.
    let inherited_cache = if let Some(source_stats) = source_stat_cache {
        let child_repo = gix::open(&child)?;
        let child_index = child_repo.open_index()?;
        let child_stats = child_index
            .entries()
            .iter()
            .map(|entry| (entry.path(&child_index).to_vec(), entry.stat))
            .collect::<Vec<_>>();
        let bytes = std::fs::read(child_repo.git_dir().join("index"))?;
        let has_untracked_cache = git_index_layout(&bytes, 20)
            .map_err(std::io::Error::other)?
            .extension_data_range(b"UNTR")
            .is_some();
        Some((source_stats == child_stats, has_untracked_cache))
    } else {
        None
    };
    if race {
        assert_eq!(std::fs::read_to_string(&race_marker)?, "ran");
        assert_eq!(std::fs::read_to_string(root.join("one"))?, "after\n");
        assert_eq!(std::fs::read_to_string(root.join("two"))?, "before\n");
        assert!(root.join("after-cut").is_file());
        assert!(
            !child.join("after-cut").exists(),
            "materialization must classify and remove untracked files from the actual child cut"
        );
    }
    if scenario == GitSnapshotHandoffScenario::MissingCursor {
        assert_eq!(
            std::fs::read_to_string(root.join("one"))?,
            "unobserved edit\n"
        );
        assert!(root.join("without-cursor").is_file());
        assert!(!child.join("without-cursor").exists());
    }
    if scenario == GitSnapshotHandoffScenario::CollectedCursor {
        let events = std::fs::read_to_string(&creation_trace)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        let lstats = events
            .iter()
            .filter(|event| {
                event["event"] == "data"
                    && event["category"] == "index"
                    && matches!(
                        event["key"].as_str(),
                        Some("refresh/sum_lstat" | "preload/sum_lstat")
                    )
            })
            .map(|event| event["value"].as_str().unwrap().parse::<u64>())
            .collect::<Result<Vec<_>, _>>()?;
        assert!(
            !lstats.is_empty() && lstats.iter().sum::<u64>() == 0,
            "a clean logical replay must preserve the copied index's clean bits, not move a full \
             tracked refresh into creation: {lstats:?}"
        );
        assert!(!events.iter().any(|event| matches!(
            event["key"].as_str(),
            Some("query/failed" | "query/trivial-response")
        )));
    }
    OpenedSubvolume::open(&child)?;
    if colocated_jj {
        assert_eq!(
            run(git_command(&child).args(["rev-parse", "HEAD^{tree}"]))?.stdout,
            source_tree
        );
        // Compare Git's actual index objects independently of its clean bits
        // and HEAD. Blindly inheriting a staged source index must fail here.
        let child_repo = gix::open(&child)?;
        let index = child_repo.open_index()?;
        assert_eq!(index.entries().len(), if custom_filter { 3 } else { 2 });
        for entry in index.entries() {
            let name: &[u8] = entry.path(&index).as_ref();
            let expected = match name {
                b"one" => b"first\n".as_slice(),
                b"two" => b"second\n".as_slice(),
                b".gitattributes" if custom_filter => filter_attributes.as_bytes(),
                other => panic!("unexpected child index entry {other:?}"),
            };
            assert_eq!(child_repo.find_object(entry.id)?.data, expected);
        }
    } else {
        assert_eq!(
            run(git_command(&child).args(["rev-parse", "HEAD"]))?.stdout,
            source_head
        );
    }
    if empty_sparse {
        let child_repo = gix::open(&child)?;
        let index = child_repo.open_index()?;
        let mut expected_git_status = String::new();
        for entry in index.entries() {
            let name = std::str::from_utf8(entry.path(&index).as_ref())?;
            assert!(
                !child.join(name).exists(),
                "sparse checkout materialized {name}"
            );
            if !entry
                .flags
                .contains(gix::index::entry::Flags::SKIP_WORKTREE)
            {
                expected_git_status.push_str(&format!(" D {name}\n"));
            }
        }
        let status = run(git_command(&child).args(["status", "--porcelain=v1"]))?;
        assert_eq!(
            status.stdout,
            expected_git_status.as_bytes(),
            "missing sparse paths must not be hidden by copied clean bits"
        );
        // Check the registered semantic tree separately from taking another
        // sparse working-copy snapshot: this test targets index inheritance.
        let diff = jj_command(&child)
            .args(["--ignore-working-copy", "diff", "--summary"])
            .output()?;
        assert!(
            diff.status.success(),
            "{}",
            String::from_utf8_lossy(&diff.stderr)
        );
        assert!(
            diff.stdout.is_empty(),
            "sparse omissions must not change JJ's tree"
        );
        assert!(
            std::fs::read(&index_path)? == original_index,
            "sparse child initialization must preserve the source index"
        );
        cleanup.cleanup()?;
        return Ok(());
    }
    for (name, contents) in [("one", "first\n"), ("two", "second\n")] {
        assert_eq!(std::fs::read_to_string(child.join(name))?, contents);
    }
    if let Some((preserved_stats, preserved_untracked_cache)) = inherited_cache {
        assert!(
            preserved_stats && preserved_untracked_cache,
            "clean equal-tree JJ creation must inherit the warm Git index, not merely reconstruct \
             equivalent entries after GC: preserved_stats={preserved_stats}, \
             preserved_untracked_cache={preserved_untracked_cache}"
        );
    }
    if custom_filter {
        assert_ne!(
            run(git_command(&root).args(["hash-object", "--path=one", "one"]))?.stdout,
            run(git_command(&child).args(["hash-object", "--path=one", "one"]))?.stdout,
            "the fixture must actually have worktree-dependent clean semantics"
        );
        let child_status = run(git_command(&child).args(["status", "--porcelain=v1"]))?;
        assert_eq!(
            child_status.stdout, b" M one\n",
            "inherited clean bits must not hide a child-specific conversion"
        );
        assert!(
            std::fs::read(&index_path)? == original_index,
            "filter fallback must preserve the source index"
        );
        cleanup.cleanup()?;
        return Ok(());
    }
    let child_status = run(git_command(&child).args(["status", "--porcelain=v1"]))?;
    assert!(
        child_status.stdout.is_empty(),
        "snapshot child must be clean before further Git/JJ activity:\n{}",
        String::from_utf8_lossy(&child_status.stdout)
    );
    let GitFsmonitorStatus::Baseline(child_baseline) =
        inspect_git_index(&gix::open(&child)?.git_dir().join("index"))?.fsmonitor
    else {
        panic!("the child must have its own AWACS cursor");
    };
    assert_ne!(child_baseline.retention_token, baseline.retention_token);
    assert!(
        RootStatus::open(&child)?
            .inspect_baseline(&child_baseline)
            .issues
            .is_empty()
    );
    if colocated_jj {
        let child_index_path = gix::open(&child)?.git_dir().join("index");
        let retained_index = std::fs::read(&child_index_path)?;
        // Each frontend advances and collects history while the other keeps
        // its own baseline. Neither may consume the other's retention owner.
        for _ in 0..2 {
            jj_command(&child).arg("status").assert().success();
            assert!(
                std::fs::read(&child_index_path)? == retained_index,
                "JJ must not rewrite Git's persisted index while advancing its own baseline"
            );
            assert!(
                RootStatus::open(&child)?
                    .inspect_baseline(&child_baseline)
                    .issues
                    .is_empty(),
                "JJ retention must preserve Git's logically valid cursor"
            );
            assert!(
                run(git_command(&child)
                    .env("GIT_OPTIONAL_LOCKS", "0")
                    .args(["status", "--porcelain=v1"]))?
                .stdout
                .is_empty()
            );
        }
        jj_command(&child).arg("status").assert().success();
        assert!(
            std::fs::read(&index_path)? == original_index,
            "child activity must not publish or replace the source index"
        );
    }
    cleanup.cleanup()?;
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_workspace_add_seeds_first_git_status_without_full_refresh() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use btrfs_awacs::status::GitFsmonitorStatus;
    use btrfs_awacs::status::RootStatus;
    use btrfs_awacs::status::inspect_git_index;

    let base = std::path::PathBuf::from(std::env::var_os("JJ_TEST_BTRFS_ROOT").unwrap());
    let awacs = std::env::var_os("BTRFS_AWACS_COMMAND").unwrap();
    let broker = std::env::var_os("BTRFS_AWACS_BROKER_SOCKET").unwrap();
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let root = base.join(format!("jj-git-seed-{suffix}"));
    let unused_state = base.join(format!(".jj-git-seed-state-{suffix}"));
    let runtime = std::env::temp_dir().join(format!("jj-git-seed-{suffix}"));
    for directory in [&root, &unused_state, &runtime] {
        std::fs::create_dir(directory)?;
    }
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    let mut cleanup = RealBtrfsFixtureCleanup::new(root.clone(), unused_state, runtime.clone());
    std::os::unix::fs::symlink(&awacs, runtime.join("git-fsmonitor-awacs"))?;
    let mut paths = vec![runtime.clone()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths)?;
    let configure = |cmd: &mut assert_cmd::Command| {
        cmd.env("PATH", &path)
            .env("BTRFS_AWACS_COMMAND", &awacs)
            .env("BTRFS_AWACS_BROKER_DIR", &broker)
            .env("BTRFS_AWACS_BROKER_SOCKET", &broker)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR");
    };
    let git_command = |directory: &std::path::Path| {
        let mut cmd = Command::new("git");
        cmd.current_dir(directory)
            .env("PATH", &path)
            .env("BTRFS_AWACS_BROKER_DIR", &broker)
            .env("BTRFS_AWACS_BROKER_SOCKET", &broker)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR");
        cmd
    };
    let test_env = TestEnvironment::default();
    let main = test_env.work_dir(&root);
    main.run_jj_with(|cmd| {
        configure(cmd);
        cmd.args(["git", "init", "--colocate", "."])
    })
    .success();
    for number in 0..128 {
        main.write_file(format!("tracked/{number:03}"), "committed contents\n");
    }
    main.run_jj_with(|cmd| {
        configure(cmd);
        cmd.args(["commit", "-m", "populated parent"])
    })
    .success();
    assert!(
        git_command(&root)
            .args(["config", "core.fsmonitor", "true"])
            .status()?
            .success()
    );
    main.run_jj_with(|cmd| {
        configure(cmd);
        cmd.args(["util", "subvolume", "enable"])
    })
    .success();
    assert!(matches!(
        inspect_git_index(&root.join(".git/index"))?.fsmonitor,
        GitFsmonitorStatus::Baseline(_)
    ));
    let real_btrfs = Command::new("sh")
        .args(["-c", "command -v btrfs"])
        .output()?;
    assert!(real_btrfs.status.success());
    let real_btrfs = std::path::PathBuf::from(String::from_utf8(real_btrfs.stdout)?.trim());
    assert!(real_btrfs.is_absolute());
    let race_bin = runtime.join("race-bin");
    std::fs::create_dir(&race_bin)?;
    let race_wrapper = race_bin.join("btrfs");
    std::fs::write(
        &race_wrapper,
        r#"#!/bin/sh
last=
for argument do last=$argument; done
if [ "$1" = subvolume ] && [ "$2" = snapshot ] && [ "$last" = "$JJ_TEST_RACE_DEST" ]; then
    printf '%s\n' 'edit between the JJ scan and physical clone' > "$JJ_TEST_RACE_ROOT/tracked/002"
    printf ran > "$JJ_TEST_RACE_MARKER"
fi
exec "$JJ_TEST_REAL_BTRFS" "$@"
"#,
    )?;
    std::fs::set_permissions(&race_wrapper, std::fs::Permissions::from_mode(0o755))?;
    let race_path =
        std::env::join_paths(std::iter::once(race_bin).chain(std::env::split_paths(&path)))?;

    // The source Git cursor intentionally predates this edit. Copying its
    // clean bitmap and replacing only the cursor would not be a valid seed.
    for (name, revision, source_contents) in [
        ("parent", None, "source edit not yet seen by Git\n"),
        ("clean", None, "committed contents\n"),
        (
            "source",
            Some("@"),
            "another source edit, still unseen by Git\n",
        ),
        (
            "after-jj",
            Some("@"),
            "source edit before an intervening JJ command\n",
        ),
        (
            "conversion",
            Some("@"),
            "source edit with a Git clean filter\n",
        ),
        ("race", None, "source edit before the clone race\n"),
    ] {
        let attributes = "tracked/003 filter=jj-seed-test\n";
        if name == "conversion" {
            main.write_file(".gitattributes", attributes);
            assert!(
                git_command(&root)
                    .args([
                        "config",
                        "filter.jj-seed-test.clean",
                        "sed s/committed/converted/"
                    ])
                    .status()?
                    .success()
            );
        } else if root.join(".gitattributes").exists() {
            std::fs::remove_file(root.join(".gitattributes"))?;
        }
        main.write_file("tracked/001", source_contents);
        let child = base.join(format!("jj-git-seed-{name}-{suffix}"));
        cleanup.track_subvolume_root(child.clone());
        let creation_trace = runtime.join(format!("{name}-creation.jsonl"));
        let race_marker = runtime.join("source-race-ran");
        main.run_jj_with(|cmd| {
            configure(cmd);
            cmd.env("GIT_TRACE2_EVENT", &creation_trace);
            if name == "race" {
                cmd.env("PATH", &race_path)
                    .env("JJ_TEST_REAL_BTRFS", &real_btrfs)
                    .env("JJ_TEST_RACE_ROOT", &root)
                    .env("JJ_TEST_RACE_DEST", &child)
                    .env("JJ_TEST_RACE_MARKER", &race_marker);
            }
            cmd.args(["workspace", "add", "--name", name]);
            if let Some(revision) = revision {
                cmd.args(["-r", revision]);
            }
            cmd.arg(&child)
        })
        .success();
        if name == "race" {
            assert!(
                race_marker.is_file(),
                "the source edit must happen before the physical clone"
            );
        }
        // Moving the full refresh into workspace creation is not a fix.
        let mut creation_lstats = 0;
        for line in std::fs::read_to_string(&creation_trace)?.lines() {
            let event: serde_json::Value = serde_json::from_str(line)?;
            if event["event"] == "data"
                && event["category"] == "index"
                && matches!(
                    event["key"].as_str(),
                    Some("refresh/sum_lstat" | "preload/sum_lstat")
                )
            {
                creation_lstats += event["value"].as_str().unwrap().parse::<u64>()?;
            }
        }
        assert!(
            creation_lstats <= 8,
            "workspace creation refreshed {creation_lstats} tracked entries"
        );
        assert_default_awacs_layout(&child);
        let child_repo = gix::open(&child)?;
        let initial_index = child_repo.open_index()?;
        assert_eq!(
            initial_index.entries().len(),
            if name == "conversion" { 129 } else { 128 }
        );
        // Check index semantics independently of fsmonitor: an incorrectly
        // trusted old blob could otherwise be hidden by its clean bit.
        for entry in initial_index.entries() {
            let index_path: &[u8] = entry.path(&initial_index).as_ref();
            let expected = if index_path == b".gitattributes" {
                attributes
            } else if index_path == b"tracked/001" && revision.is_some() {
                source_contents
            } else {
                "committed contents\n"
            };
            let object = child_repo.find_object(entry.id)?;
            assert_eq!(object.kind, gix::objs::Kind::Blob);
            assert_eq!(
                object.data,
                expected.as_bytes(),
                "incorrect index entry {index_path:?}"
            );
        }
        let initial_git = inspect_git_index(&child_repo.git_dir().join("index"))?;
        let child_state = RootStatus::open(&child)?;
        let baseline_issues = match &initial_git.fsmonitor {
            GitFsmonitorStatus::Baseline(baseline) => child_state.inspect_baseline(baseline).issues,
            _ => Vec::new(),
        };
        let observed_index = if let GitFsmonitorStatus::Baseline(baseline) = &initial_git.fsmonitor
        {
            let sequence = child_state.inspect_baseline(baseline).sequence.unwrap();
            let store = btrfs_awacs::store::Store::open(&child_state.database)?;
            store.connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM git_cursor_observations WHERE watch_id = ?1 AND \
                 cut_sequence = ?2 AND retain_until_ns IS NULL)",
                (child_state.watch.id.as_slice(), sequence),
                |row| row.get::<_, bool>(0),
            )?
        } else {
            false
        };
        // The pinned codec exposes the FSMN bitmap on this upstream gix
        // version, which can otherwise read but not inspect individual bits.
        let codec_index = gix_index_cache::File::at(
            child_repo.git_dir().join("index"),
            gix_index_cache::hash::Kind::Sha1,
            false,
            gix_index_cache::decode::Options::default(),
        )?;
        let mut dirty_entries = 0;
        if let Some(monitor) = codec_index.fs_monitor() {
            monitor
                .for_each_dirty_entry(|_| {
                    dirty_entries += 1;
                    Some(())
                })
                .expect("new workspace must have a valid FSMN bitmap");
        } else {
            dirty_entries = initial_index.entries().len();
        }
        assert_eq!(
            std::fs::read_to_string(child.join("tracked/001"))?,
            if revision.is_some() {
                source_contents
            } else {
                "committed contents\n"
            }
        );

        if name == "after-jj" {
            // A persisted Git cursor must survive an independent JJ scan and
            // its retention pass before Git observes that cursor for the first time.
            test_env
                .work_dir(&child)
                .run_jj_with(|cmd| {
                    configure(cmd);
                    cmd.arg("status")
                })
                .success();
        }
        // Otherwise no normal Git/JJ reader may warm the child before this
        // mutation and first status. A fresh cursor must not hide later writes.
        std::fs::write(
            child.join("tracked/000"),
            "new child edit with a different length\n",
        )?;
        let trace = runtime.join(format!("{name}-status.jsonl"));
        let status = git_command(&child)
            .env("GIT_TRACE2_EVENT", &trace)
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=no"])
            .output()?;
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        // A clone of the exact immutable JJ baseline can exclude the racing
        // source edit. If it copied the live edit, Git must report it instead
        // of trusting a clean bit derived from the older semantic tree.
        let raced_contents = std::fs::read(child.join("tracked/002"))?;
        let mut expected_status = b" M tracked/000\0".to_vec();
        if raced_contents != b"committed contents\n" {
            assert_eq!(
                raced_contents,
                b"edit between the JJ scan and physical clone\n"
            );
            expected_status.extend_from_slice(b" M tracked/002\0");
        }
        if name == "conversion" {
            // Git compares cleaned contents, which differ from the raw JJ
            // blob here. Its fsmonitor bit must not conceal that difference.
            expected_status.extend_from_slice(b" M tracked/003\0");
        }
        assert_eq!(status.stdout, expected_status);
        let events = std::fs::read_to_string(&trace)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        let sid = events
            .iter()
            .find(|event| event["event"] == "cmd_name" && event["name"] == "status")
            .expect("Git status must identify itself in Trace2")["sid"]
            .clone();
        let data = events
            .iter()
            .filter(|event| event["event"] == "data" && event["sid"] == sid)
            .collect::<Vec<_>>();
        let counter = |category: &str, key: &str| {
            data.iter()
                .filter(|event| event["category"] == category && event["key"] == key)
                .map(|event| event["value"].as_str().unwrap().parse::<u64>().unwrap())
                .collect::<Vec<_>>()
        };
        let refresh = counter("index", "refresh/sum_lstat");
        assert!(
            !refresh.is_empty(),
            "Git must report its tracked refresh work: {events:?}"
        );
        let lstats =
            refresh.iter().sum::<u64>() + counter("index", "preload/sum_lstat").iter().sum::<u64>();
        let fsmonitor = match initial_git.fsmonitor {
            GitFsmonitorStatus::Baseline(_) => "Baseline",
            GitFsmonitorStatus::Missing => "Missing",
            _ => "Other",
        };
        assert!(
            matches!(initial_git.fsmonitor, GitFsmonitorStatus::Baseline(_))
                && baseline_issues.is_empty()
                && observed_index
                && dirty_entries <= 2
                && (1..=8).contains(&lstats),
            "first Git status must use a valid child baseline and refresh only changed entries: \
             fsmonitor: {fsmonitor}, baseline_issues={baseline_issues:?}, \
             observed_index={observed_index}, dirty_entries={dirty_entries}, \
             tracked_lstats={lstats}"
        );
        assert!(
            counter("index", "read/cache_nr")
                .iter()
                .any(|count| *count >= 128)
        );
        assert!(
            counter("fsm_hook", "query/response-length")
                .iter()
                .any(|count| *count > 0)
        );
        assert!(
            counter("fsmonitor", "apply_count")
                .iter()
                .any(|count| *count > 0)
        );
        assert!(!data.iter().any(|event| matches!(
            event["key"].as_str(),
            Some("query/failed" | "query/trivial-response")
        )));
        eprintln!(
            "{name}: first Git status tracked_lstats={lstats}, \
             initial_dirty_entries={dirty_entries}, creation_lstats={creation_lstats}"
        );

        if name == "conversion" {
            // A revision error may leave a child behind. It must not report a
            // filtered path clean, even when workspace creation did not finish.
            let failed_child = base.join(format!("jj-git-seed-failed-{suffix}"));
            cleanup.track_subvolume_root(failed_child.clone());
            let failed = main.run_jj_with(|cmd| {
                configure(cmd);
                cmd.args([
                    "workspace",
                    "add",
                    "--name",
                    "failed",
                    "-r",
                    "missing-git-seed-revision",
                ])
                .arg(&failed_child)
            });
            assert!(!failed.status.success(), "{failed}");
            assert!(failed.stderr.raw().contains("missing-git-seed-revision"));
            assert!(failed_child.join(".git").is_file());
            let diff = git_command(&failed_child)
                .args(["status", "--porcelain=v1", "-z", "--untracked-files=no"])
                .output()?;
            assert!(diff.status.success());
            assert!(
                diff.stdout
                    .split(|byte| *byte == 0)
                    .any(|entry| entry.ends_with(b" tracked/003"))
            );
        }
    }
    cleanup.cleanup()?;
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_util_subvolume_migration_finalizes_default_awacs_paths_and_git_cursor() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use btrfs_awacs::status::GitFsmonitorStatus;
    use btrfs_awacs::status::inspect_git_index;

    let btrfs_root = std::path::PathBuf::from(
        std::env::var_os("JJ_TEST_BTRFS_ROOT").expect("JJ_TEST_BTRFS_ROOT is required"),
    );
    let awacs_command =
        std::env::var_os("BTRFS_AWACS_COMMAND").expect("BTRFS_AWACS_COMMAND is required");
    let broker_socket = std::env::var_os("BTRFS_AWACS_BROKER_SOCKET")
        .expect("BTRFS_AWACS_BROKER_SOCKET is required");
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let repo_root = btrfs_root.join(format!("jj-finalize-{suffix}"));
    let awacs_state = btrfs_root.join(format!(".jj-finalize-state-{suffix}"));
    let runtime_dir = std::env::temp_dir().join(format!("jj-finalize-{suffix}"));
    for directory in [&repo_root, &awacs_state, &runtime_dir] {
        std::fs::create_dir(directory)?;
    }
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))?;
    let mut cleanup =
        RealBtrfsFixtureCleanup::new(repo_root.clone(), awacs_state.clone(), runtime_dir.clone());
    let hook = runtime_dir.join("git-fsmonitor-awacs");
    std::os::unix::fs::symlink(&awacs_command, &hook)?;
    let mut paths = vec![runtime_dir.clone()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths)?;
    let configure = |cmd: &mut assert_cmd::Command| {
        cmd.env("PATH", &path)
            .env("BTRFS_AWACS_COMMAND", &awacs_command)
            .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR");
    };
    let run_git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&repo_root)
            .env("PATH", &path)
            .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR")
            .output()
    };
    let test_env = TestEnvironment::default();
    let main_dir = test_env.work_dir(&repo_root);
    main_dir
        .run_jj_with(|cmd| {
            configure(cmd);
            cmd.args(["git", "init", "--colocate", "."])
        })
        .success();
    main_dir.write_file("file", "before migration\n");
    main_dir.write_file("deleted", "delete me\n");
    main_dir.write_file(".gitignore", "ignored\n");
    main_dir
        .run_jj_with(|cmd| {
            configure(cmd);
            cmd.args(["commit", "-m", "initial"])
        })
        .success();
    assert!(
        run_git(&["config", "core.fsmonitor", hook.to_str().unwrap()])?
            .status
            .success()
    );
    assert!(
        run_git(&["config", "core.fsmonitorHookVersion", "2"])?
            .status
            .success()
    );
    let initialized = Command::new(&awacs_command)
        .arg("init")
        .arg(&repo_root)
        .env("PATH", &path)
        .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .output()?;
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    assert!(
        run_git(&["status", "--porcelain=v1", "-z"])?
            .status
            .success()
    );
    // Exercise an established Git history, not only its first bootstrap cut:
    // a copied cursor should also be newer than the replacement manager head.
    for _ in 0..2 {
        assert!(
            run_git(&["update-index", "--refresh", "--force-write-index"])?
                .status
                .success()
        );
    }
    let original_cursor = inspect_git_index(&repo_root.join(".git/index"))?.fsmonitor;
    let GitFsmonitorStatus::Baseline(original_baseline) = &original_cursor else {
        panic!(
            "the migration source must already have a real Git AWACS cursor: {original_cursor:?}"
        );
    };
    let original_status = btrfs_awacs::status::RootStatus::open(&repo_root)?;
    assert!(
        original_status.issues.is_empty(),
        "source AWACS state must be healthy before migration: {:?}",
        original_status.issues
    );
    let original_git_status = original_status.inspect_baseline(original_baseline);
    assert!(
        original_git_status.issues.is_empty(),
        "source Git AWACS cursor must belong to its current manager before migration: {:?}",
        original_git_status.issues
    );
    assert_default_awacs_layout(&repo_root);
    main_dir.write_file("file", "modified before migration\n");
    main_dir.write_file("untracked", "preserve new file\n");
    main_dir.write_file("ignored", "preserve ignored file\n");
    std::fs::remove_file(repo_root.join("deleted"))?;

    for arguments in [
        vec!["util", "subvolume", "enable"],
        vec!["util", "subvolume", "enable", "--rebuild-baseline"],
    ] {
        let before = inspect_git_index(&repo_root.join(".git/index"))?.fsmonitor;
        let operation = arguments.join(" ");
        main_dir
            .run_jj_with(|cmd| {
                configure(cmd);
                cmd.args(&arguments)
            })
            .success();
        // Status is read-only: verify immediately, before any normal JJ scan
        // can lazily register the activated path and hide a migration bug.
        let status = main_dir
            .run_jj_with(|cmd| {
                configure(cmd);
                cmd.args(["util", "subvolume", "status"])
            })
            .success();
        assert!(
            status.stdout.raw().contains("Consistency: OK"),
            "immediate `{operation}` consistency failed before any normal JJ/Git scan:\n{status}"
        );
        assert_default_awacs_layout(&repo_root);
        let after = inspect_git_index(&repo_root.join(".git/index"))?.fsmonitor;
        assert!(matches!(after, GitFsmonitorStatus::Baseline(_)));
        assert_ne!(
            after, before,
            "migration/rebuild must replace the old manager cursor"
        );
        assert_eq!(
            std::fs::read_to_string(repo_root.join("file"))?,
            "modified before migration\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo_root.join("untracked"))?,
            "preserve new file\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo_root.join("ignored"))?,
            "preserve ignored file\n"
        );
        assert!(!repo_root.join("deleted").exists());
        assert_eq!(
            run_git(&["config", "core.fsmonitor"])?.stdout,
            format!("{}\n", hook.display()).as_bytes()
        );
        assert_eq!(
            run_git(&["config", "core.fsmonitorHookVersion"])?.stdout,
            b"2\n"
        );
    }

    // Git's index refresh can succeed when a hook fails. An incompatible
    // protocol must fail finalization instead of leaving a timestamp token
    // behind while reporting a successful rebuild.
    assert!(
        run_git(&["config", "core.fsmonitorHookVersion", "1"])?
            .status
            .success()
    );
    let failed = main_dir.run_jj_with(|cmd| {
        configure(cmd);
        cmd.args(["util", "subvolume", "enable", "--rebuild-baseline"])
    });
    assert!(!failed.status.success(), "{failed}");
    assert!(
        failed
            .stderr
            .raw()
            .contains("Git did not publish a valid AWACS fsmonitor baseline"),
        "{failed}"
    );
    assert_eq!(
        run_git(&["config", "core.fsmonitorHookVersion"])?.stdout,
        b"1\n"
    );
    assert!(
        run_git(&["config", "core.fsmonitorHookVersion", "2"])?
            .status
            .success()
    );

    // Retrying enable repairs only the unfinished finalization. Preserve the
    // new manager and committed JJ journal instead of rebuilding either one.
    let before_retry = btrfs_awacs::status::RootStatus::open(&repo_root)?;
    let before_journal = std::fs::read(repo_root.join(".jj/working_copy/checkout"))?;
    let git_repo = gix::open(&repo_root)?;
    let semantic_entries = |index: &gix::index::File| {
        index
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.path(index).to_vec(),
                    entry.mode,
                    entry.id,
                    entry.stage(),
                )
            })
            .collect::<Vec<_>>()
    };
    let before_index = semantic_entries(&git_repo.open_index()?);
    main_dir
        .run_jj_with(|cmd| {
            configure(cmd);
            cmd.args(["util", "subvolume", "enable"])
        })
        .success();
    let retried_status = main_dir
        .run_jj_with(|cmd| {
            configure(cmd);
            cmd.args(["util", "subvolume", "status"])
        })
        .success();
    assert!(
        retried_status.stdout.raw().contains("Consistency: OK"),
        "{retried_status}"
    );
    let after_retry = btrfs_awacs::status::RootStatus::open(&repo_root)?;
    assert_eq!(after_retry.database, before_retry.database);
    assert_eq!(after_retry.watch.id, before_retry.watch.id);
    assert_eq!(
        std::fs::read(repo_root.join(".jj/working_copy/checkout"))?,
        before_journal
    );
    assert_eq!(semantic_entries(&git_repo.open_index()?), before_index);

    // Rebuilding AWACS must not take over explicitly disabled or custom Git
    // fsmonitor configurations, even if an old AWACS index extension exists.
    for value in ["false", "/nonexistent/custom-fsmonitor"] {
        assert!(
            run_git(&["config", "core.fsmonitor", value])?
                .status
                .success()
        );
        main_dir
            .run_jj_with(|cmd| {
                configure(cmd);
                cmd.args(["util", "subvolume", "enable", "--rebuild-baseline"])
            })
            .success();
        assert_eq!(
            run_git(&["config", "core.fsmonitor"])?.stdout,
            format!("{value}\n").as_bytes()
        );
        assert_eq!(
            run_git(&["config", "core.fsmonitorHookVersion"])?.stdout,
            b"2\n"
        );
    }

    // An empty Git index still needs both explicit FSMN writes, even with
    // GIT_OPTIONAL_LOCKS=0 and no tracked stat data that otherwise changed.
    assert!(
        run_git(&["config", "core.fsmonitor", hook.to_str().unwrap()])?
            .status
            .success()
    );
    // The root checkout removes .gitignore too; do not let the deliberately
    // preserved ignored file become a newly tracked entry in this empty case.
    std::fs::remove_file(repo_root.join("ignored"))?;
    main_dir
        .run_jj_with(|cmd| {
            configure(cmd);
            cmd.args(["new", "root()"])
        })
        .success();
    assert_eq!(inspect_git_index(&repo_root.join(".git/index"))?.entries, 0);
    main_dir
        .run_jj_with(|cmd| {
            configure(cmd);
            cmd.args(["util", "subvolume", "enable", "--rebuild-baseline"])
        })
        .success();
    let empty_status = main_dir
        .run_jj_with(|cmd| {
            configure(cmd);
            cmd.args(["util", "subvolume", "status"])
        })
        .success();
    assert!(
        empty_status.stdout.raw().contains("Consistency: OK"),
        "{empty_status}"
    );
    assert_eq!(inspect_git_index(&repo_root.join(".git/index"))?.entries, 0);
    cleanup.cleanup()?;
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn remove_plain_tree(path: &std::path::Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[test]
fn test_workspaces_invalid_name() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let main_dir = test_env.work_dir("repo");

    // refuse to create, directory not created
    let output = main_dir.run_jj(["workspace", "add", "--name", "", "../secondary"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: New workspace name cannot be empty
    [EOF]
    [exit status: 1]
    ");
    assert!(!test_env.env_root().join("secondary").exists());

    // refuse to rename
    let output = main_dir.run_jj(["workspace", "rename", ""]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: New workspace name cannot be empty
    [EOF]
    [exit status: 1]
    ");
}

/// Test adding a second and a third workspace
#[test]
fn test_workspaces_add_second_and_third_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . rlvkpnrz 504e3d8c (empty) (no description set)
    [EOF]
    ");

    let output = main_dir.run_jj(["workspace", "add", "--name", "second", "../secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../secondary"
    Working copy  (@) now at: rzvqmyuk bcc858e1 (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    // Can see the working-copy commit in each workspace in the log output. The "@"
    // node in the graph indicates the current workspace's working-copy commit.
    insta::assert_snapshot!(get_log_output(&main_dir), @r#"
    @  504e3d8c1bcd default@
    │ ○  bcc858e1d93f second@
    ├─╯
    ○  7b22a8cbe888 "initial"
    ◆  000000000000
    [EOF]
    "#);
    insta::assert_snapshot!(get_log_output(&secondary_dir), @r#"
    @  bcc858e1d93f second@
    │ ○  504e3d8c1bcd default@
    ├─╯
    ○  7b22a8cbe888 "initial"
    ◆  000000000000
    [EOF]
    "#);

    // Both workspaces show up when we list them
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . rlvkpnrz 504e3d8c (empty) (no description set)
    second: ../secondary rzvqmyuk bcc858e1 (empty) (no description set)
    [EOF]
    ");

    // Check that a workspace can be created in an existing empty directory
    main_dir.create_dir("../third");
    let output = main_dir.run_jj(["workspace", "add", "--name", "third", "../third"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../third"
    Working copy  (@) now at: nuwvvtmy d55e769c (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    // Existing non-empty directories are not allowed
    main_dir.create_dir("../occupied");
    main_dir.write_file("../occupied/file", "contents");
    let output = main_dir.run_jj(["workspace", "add", "--name", "occupied", "../occupied"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    ------- stderr -------
    Error: Destination path exists and is not an empty directory
    [EOF]
    [exit status: 1]
    ");

    // Duplicate names are not allowed, directory not created
    let output = main_dir.run_jj(["workspace", "add", "--name", "third", "../tertiary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    ------- stderr -------
    Error: Workspace named 'third' already exists
    [EOF]
    [exit status: 1]
    ");
    assert!(!test_env.env_root().join("tertiary").exists());
}

#[test]
fn test_workspaces_add_with_message() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    let output = main_dir.run_jj([
        "workspace",
        "add",
        "--name",
        "second",
        "-m",
        "add second workspace",
        "../secondary",
    ]);

    // Check that the newly created workspace has a description for the commit
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../secondary"
    Working copy  (@) now at: pmmvwywv 47a2d9a1 (empty) add second workspace
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    main_dir
        .run_jj([
            "workspace",
            "add",
            "--name",
            "third",
            "-m",
            "first message",
            "-m",
            "second message",
            "../tertiary",
        ])
        .success();

    // Check that multiple messages work as expected like with the "new" command
    let output = main_dir.run_jj(["log", "-r", "third@", "-Tdescription", "--no-graph"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    first message

    second message
    [EOF]
    ");

    // Test that adding workspace with no message has no trailers
    test_env.add_config(
        r#"[templates]
        commit_trailers = '"Signed-off-by: " ++ committer.email()'
        "#,
    );

    let output = main_dir.run_jj(["workspace", "add", "--name", "fourth", "../quaternary"]);

    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../quaternary"
    Working copy  (@) now at: nppvrztz 0bfa7004 (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);
}

#[test]
fn test_workspaces_add_colocated_git_worktree() {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    main_dir.create_dir("../secondary");
    let output = main_dir.run_jj(["workspace", "add", "--colocate", "../secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created Git worktree for the new workspace.
    Created workspace in "../secondary"
    Working copy  (@) now at: pmmvwywv 058f604d (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    assert!(secondary_dir.root().join(".git").exists());

    let worktree_output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(main_dir.root())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&worktree_output.stdout);
    assert!(
        stdout.contains(secondary_dir.root().to_str().unwrap()),
        "worktree list should mention secondary workspace: {stdout}"
    );

    let status_output = std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(secondary_dir.root())
        .output()
        .unwrap();
    let git_status = String::from_utf8_lossy(&status_output.stdout);
    assert!(
        git_status.is_empty() || git_status.trim() == "?? .jj/",
        "git status should be clean in the new worktree; got {git_status:?}"
    );
}

#[test]
fn test_workspaces_adopt_existing_colocated_git_worktree() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let worktree_dir = test_env.work_dir("worktree");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    let add_output = std::process::Command::new("git")
        .args(["worktree", "add", "--detach", "../worktree", "HEAD"])
        .current_dir(main_dir.root())
        .output()?;
    assert!(
        add_output.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    worktree_dir.write_file("file", "changed in worktree\n");
    worktree_dir.write_file("untracked", "new\n");

    let output = worktree_dir
        .run_jj(["workspace", "adopt", "--name", "adopted"])
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("Adopted Git worktree as workspace 'adopted'")
    );

    let output = worktree_dir.run_jj(["status"]).success();
    assert!(output.stdout.raw().contains("M file"));
    assert!(output.stdout.raw().contains("A untracked"));

    let worktree_output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(main_dir.root())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&worktree_output.stdout);
    assert!(
        stdout.contains(worktree_dir.root().to_str().unwrap()),
        "worktree list should mention adopted workspace: {stdout}"
    );

    let status_output = std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(worktree_dir.root())
        .output()
        .unwrap();
    assert!(
        status_output.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&status_output.stderr)
    );

    let output = main_dir.run_jj(["workspace", "list"]).success();
    assert!(output.stdout.raw().contains("adopted:"));

    Ok(())
}

#[test]
fn test_workspaces_adopt_rejects_main_git_worktree() {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");

    let output = main_dir.run_jj(["workspace", "adopt"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: Cannot adopt the main Git worktree; run this from a linked Git worktree
    [EOF]
    [exit status: 1]
    ");
}

#[cfg(unix)]
#[test]
fn test_workspaces_adopt_existing_snapshot_git_worktree() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let worktree_dir = test_env.work_dir("worktree");
    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir.write_file(".jj/working_copy/subvolume_mode", "snapshot-backed\n");
    let add_output = std::process::Command::new("git")
        .args(["worktree", "add", "--detach", "../worktree", "HEAD"])
        .current_dir(main_dir.root())
        .output()?;
    assert!(add_output.status.success());
    let marker = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "awacs-jj-pending-consumer"])
        .current_dir(worktree_dir.root())
        .output()?;
    assert!(
        marker.status.success(),
        "failed to find pending JJ marker path: {}",
        String::from_utf8_lossy(&marker.stderr)
    );
    let marker_path = std::path::PathBuf::from(String::from_utf8(marker.stdout)?.trim_end());
    let pending_state_path = marker_path
        .parent()
        .unwrap()
        .join("awacs-jj-pending-working-copy");
    std::fs::create_dir(&pending_state_path)?;
    std::fs::write(pending_state_path.join("checkout"), b"pending")?;
    std::fs::write(
        pending_state_path.join("subvolume_mode"),
        b"snapshot-backed\n",
    )?;
    std::fs::write(
        &marker_path,
        "awacs-jj-pending-v2:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:\
         11111111-1111-1111-1111-111111111111:22222222-2222-2222-2222-222222222222\n",
    )?;
    assert!(!worktree_dir.root().join(".jj").exists());

    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    exit 0
fi
exit 1
"#,
    );
    worktree_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", worktree_dir.root())
                .args(["workspace", "adopt", "--name", "adopted"])
        })
        .success();
    assert!(
        worktree_dir
            .root()
            .join(".jj/working_copy/subvolume_mode")
            .is_file()
    );
    assert!(worktree_dir.root().join(".jj/repo").is_file());
    assert!(!marker_path.exists());
    assert!(!pending_state_path.exists());

    // Successful adoption consumes the pending seed. Re-adoption must diagnose
    // the existing workspace before looking for that now-absent seed, even
    // when the name differs or the command runs below the worktree root.
    worktree_dir.create_dir("nested");
    let checkout_path = worktree_dir.root().join(".jj/working_copy/checkout");
    let checkout_before = std::fs::read(&checkout_path)?;
    for (directory, args) in [
        ("worktree", vec!["workspace", "adopt"]),
        (
            "worktree/nested",
            vec!["workspace", "adopt", "--name", "another-name"],
        ),
    ] {
        let output = test_env
            .work_dir(directory)
            .run_jj_with(|cmd| cmd.env("PATH", &path).args(args));
        insta::allow_duplicates! {
            insta::assert_snapshot!(output, @"
            ------- stderr -------
            Error: Cannot adopt; already an jj workspace.
            [EOF]
            [exit status: 1]
            ");
        }
        assert_eq!(std::fs::read(&checkout_path)?, checkout_before);
        assert!(!marker_path.exists());
        assert!(!pending_state_path.exists());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_workspaces_adopt_snapshot_rolls_back_after_initialization_failure() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let worktree_dir = test_env.work_dir("worktree");
    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir.write_file(".jj/working_copy/subvolume_mode", "snapshot-backed\n");
    let add_output = std::process::Command::new("git")
        .args(["worktree", "add", "--detach", "../worktree", "HEAD"])
        .current_dir(main_dir.root())
        .output()?;
    assert!(add_output.status.success());
    let marker = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "awacs-jj-pending-consumer"])
        .current_dir(worktree_dir.root())
        .output()?;
    assert!(
        marker.status.success(),
        "failed to find pending JJ marker path: {}",
        String::from_utf8_lossy(&marker.stderr)
    );
    let marker_path = std::path::PathBuf::from(String::from_utf8(marker.stdout)?.trim_end());
    let pending_state_path = marker_path
        .parent()
        .unwrap()
        .join("awacs-jj-pending-working-copy");
    std::fs::create_dir(&pending_state_path)?;
    std::fs::write(pending_state_path.join("checkout"), b"pending")?;
    std::fs::write(
        pending_state_path.join("subvolume_mode"),
        b"snapshot-backed\n",
    )?;
    std::fs::write(
        &marker_path,
        "awacs-jj-pending-v2:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:\
         11111111-1111-1111-1111-111111111111:22222222-2222-2222-2222-222222222222\n",
    )?;

    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    exit 0
fi
exit 1
"#,
    );
    // Fail after workspace initialization but before the first snapshot can
    // complete. The pending AWACS seed must remain available for a real retry,
    // while both the JJ registration and partial .jj directory are removed.
    let output = worktree_dir.run_jj_with(|cmd| {
        cmd.env("PATH", &path)
            .env("JJ_TEST_AWACS_SCAN_ROOT", worktree_dir.root())
            .args([
                "workspace",
                "adopt",
                "--name",
                "adopted",
                "--config=snapshot.auto-track='glob:['",
            ])
    });
    assert!(!output.status.success(), "{output}");
    assert!(
        output.stderr.raw().contains("Failed to parse fileset"),
        "{output}"
    );
    assert!(
        !worktree_dir.root().join(".jj").exists(),
        "failed adoption should remove partial .jj metadata"
    );
    assert!(marker_path.exists());
    assert!(pending_state_path.exists());
    let output = main_dir
        .run_jj(["--ignore-working-copy", "workspace", "list"])
        .success();
    assert!(
        !output.stdout.raw().contains("adopted:"),
        "failed adoption should remove the workspace registration: {output}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_workspaces_adopt_snapshot_requires_pending_awacs_seed() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let worktree_dir = test_env.work_dir("worktree");
    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir.write_file(".jj/working_copy/subvolume_mode", "snapshot-backed\n");
    let add_output = std::process::Command::new("git")
        .args(["worktree", "add", "--detach", "../worktree", "HEAD"])
        .current_dir(main_dir.root())
        .output()?;
    assert!(add_output.status.success());

    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    exit 0
fi
exit 1
"#,
    );
    let output = worktree_dir.run_jj_with(|cmd| {
        cmd.env("PATH", &path)
            .args(["workspace", "adopt", "--name", "adopted"])
    });
    assert!(
        output
            .stderr
            .raw()
            .contains("without a pending JJ AWACS seed"),
        "{}",
        output.stderr.raw()
    );
    Ok(())
}

#[test_case(false, false; "main worktree with full git index")]
#[test_case(false, true; "main worktree with sparse git index")]
#[test_case(true, false; "linked worktree with full git index")]
#[test_case(true, true; "linked worktree with sparse git index")]
fn test_workspaces_colocated_git_worktree_preserves_git_sparse_checkout(
    linked_worktree: bool,
    sparse_index: bool,
) {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file(".gitignore", ".jj/\n");
    main_dir.create_dir("included");
    main_dir.write_file("included/visible", "original\n");
    main_dir.create_dir("excluded");
    main_dir.write_file("excluded/hidden", "hidden\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    let sparse_dir = if linked_worktree {
        main_dir
            .run_jj(["workspace", "add", "--colocate", "../secondary"])
            .success();
        &secondary_dir
    } else {
        &main_dir
    };
    let initial_git_status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(sparse_dir.root())
        .output()
        .unwrap();
    assert!(
        initial_git_status.status.success(),
        "initial git status failed: {}",
        String::from_utf8_lossy(&initial_git_status.stderr)
    );
    assert!(
        initial_git_status.stdout.is_empty(),
        "the linked Git worktree must start clean: {}",
        String::from_utf8_lossy(&initial_git_status.stdout)
    );

    // With optional locks disabled, `git status` cannot refresh the index.
    // Refresh it explicitly before Git removes sparse-excluded files.
    let refresh = std::process::Command::new("git")
        .args(["update-index", "--refresh"])
        .current_dir(sparse_dir.root())
        .output()
        .unwrap();
    assert!(
        refresh.status.success(),
        "git update-index --refresh failed: {}",
        String::from_utf8_lossy(&refresh.stderr)
    );

    let index_mode = if sparse_index {
        "--sparse-index"
    } else {
        "--no-sparse-index"
    };
    let sparse_checkout = std::process::Command::new("git")
        .args(["sparse-checkout", "set", "--cone", index_mode, "included"])
        .current_dir(sparse_dir.root())
        .output()
        .unwrap();
    assert!(
        sparse_checkout.status.success(),
        "git sparse-checkout failed: {}",
        String::from_utf8_lossy(&sparse_checkout.stderr)
    );
    let sparse_git_status = std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(sparse_dir.root())
        .output()
        .unwrap();
    assert!(
        !sparse_dir.root().join("excluded/hidden").exists(),
        "Git must omit the excluded path when setting the sparse profile; sparse-checkout stderr: \
         {}; git status: {}; git status stderr: {}",
        String::from_utf8_lossy(&sparse_checkout.stderr),
        String::from_utf8_lossy(&sparse_git_status.stdout),
        String::from_utf8_lossy(&sparse_git_status.stderr)
    );

    let status = sparse_dir.run_jj(["status"]).success();
    assert!(
        !status.stdout.raw().contains("excluded/hidden"),
        "jj must not snapshot a sparse-excluded path as deleted: {}",
        status.stdout.raw()
    );
    assert!(!sparse_dir.root().join("excluded/hidden").exists());

    sparse_dir.write_file("included/visible", "modified\n");
    let status = sparse_dir.run_jj(["status"]).success();
    assert!(
        status.stdout.raw().contains("included/visible"),
        "jj must continue tracking changes inside the sparse profile: {}",
        status.stdout.raw()
    );
    assert!(
        !status.stdout.raw().contains("excluded/hidden"),
        "jj must preserve the sparse-excluded tree: {}",
        status.stdout.raw()
    );

    sparse_dir
        .run_jj(["describe", "-m", "preserve sparse Git worktree"])
        .success();

    assert_eq!(
        std::fs::read_to_string(sparse_dir.root().join("included/visible")).unwrap(),
        "modified\n",
        "jj must not discard changes inside the sparse profile"
    );
    let jj_diff = sparse_dir.run_jj(["diff", "--name-only"]).success();
    assert!(
        jj_diff.stdout.raw().contains("included/visible"),
        "jj must retain the real included-path change: {}",
        jj_diff.stdout.raw()
    );
    assert!(
        !jj_diff.stdout.raw().contains("excluded/hidden"),
        "jj must not record a sparse-excluded deletion: {}",
        jj_diff.stdout.raw()
    );

    let git_files = std::process::Command::new("git")
        .args([
            "ls-files",
            "-t",
            "--",
            "included/visible",
            "excluded/hidden",
        ])
        .current_dir(sparse_dir.root())
        .output()
        .unwrap();
    assert!(
        git_files.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&git_files.stderr)
    );
    let git_files = String::from_utf8_lossy(&git_files.stdout);
    assert!(
        git_files.contains("H included/visible\n"),
        "jj must not mark an included path skip-worktree: {git_files}"
    );
    assert!(
        git_files.contains("S excluded/hidden\n"),
        "jj must preserve the Git skip-worktree flag: {git_files}"
    );

    let git_status = std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(sparse_dir.root())
        .output()
        .unwrap();
    assert!(
        git_status.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&git_status.stderr)
    );
    let git_status = String::from_utf8_lossy(&git_status.stdout);
    assert!(
        !git_status.contains("excluded/hidden"),
        "Git must not report sparse-excluded paths as deleted: {git_status}"
    );

    assert!(!sparse_dir.root().join("excluded/hidden").exists());
    if linked_worktree {
        assert!(
            main_dir.root().join("excluded/hidden").exists(),
            "a linked-worktree sparse profile must not change the main worktree"
        );
    }
}

#[cfg(unix)]
#[test]
fn test_workspaces_add_snapshot() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file(".gitignore", "ignored\n");
    main_dir.write_file("file", "first\n");
    main_dir.run_jj(["commit", "-m", "first"]).success();
    main_dir.write_file("file", "second\n");
    main_dir.run_jj(["commit", "-m", "second"]).success();
    main_dir.write_file("ignored", "cached build output\n");
    main_dir.write_file(".jj/working_copy/subvolume_mode", "snapshot-backed\n");

    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    exit 0
fi
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "snapshot" ]; then
    /bin/cp -a "$3" "$4"
    exit $?
fi
exit 1
"#,
    );
    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "enable", "--rebuild-baseline"])
        })
        .success();
    let output = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args([
                    "--debug",
                    "workspace",
                    "add",
                    "--colocate",
                    "-r",
                    "@--",
                    "../secondary",
                ])
        })
        .success();
    for message in [
        "created Btrfs snapshot",
        "removed copied .jj metadata",
        "removed copied .git metadata",
        "recorded snapshot tree as workspace baseline",
        "seeded snapshot workspace AWACS baseline without traversal",
        "initialized snapshot workspace metadata",
        "created temporary Git worktree for snapshot",
        "repaired snapshot Git worktree metadata",
        "checked out snapshot workspace",
    ] {
        assert!(
            output.stderr.raw().contains(message),
            "debug output should contain {message:?}: {}",
            output.stderr.raw()
        );
    }

    // The snapshot keeps ignored materialized files, while normal add
    // checkout semantics still update tracked files to the requested revision.
    assert_eq!(
        std::fs::read_to_string(secondary_dir.root().join("file"))?,
        "first\n"
    );
    assert_eq!(
        std::fs::read_to_string(secondary_dir.root().join("ignored"))?,
        "cached build output\n"
    );
    assert!(secondary_dir.root().join(".jj").is_dir());
    assert!(secondary_dir.root().join(".git").is_file());
    assert!(
        secondary_dir
            .root()
            .join(".jj/working_copy/subvolume_mode")
            .is_file()
    );

    let status_output = std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(secondary_dir.root())
        .output()
        .unwrap();
    assert!(
        status_output.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&status_output.stderr)
    );

    let worktree_output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(main_dir.root())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&worktree_output.stdout);
    assert!(
        stdout.contains(secondary_dir.root().to_str().unwrap()),
        "worktree list should mention snapshot workspace: {stdout}"
    );

    let tertiary_dir = test_env.work_dir("tertiary");
    let output = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .env("JJ_LOG", "info")
                .args([
                    "--ignore-working-copy",
                    "workspace",
                    "add",
                    "--colocate",
                    "../tertiary",
                ])
        })
        .success();
    let log = output.stderr.raw();
    for event in [
        "enabling",
        "seeding working-copy tree",
        "committing subvolume mode marker",
        "snapshot-backed",
        "initialized snapshot workspace baseline",
    ] {
        assert!(log.contains(event), "missing {event} in log: {log}");
    }
    assert!(
        log.lines().any(|line| line.contains("ignore_working_copy")
            && line.contains("true")
            && line.contains("initializing snapshot workspace baseline")),
        "{log}"
    );
    assert!(tertiary_dir.root().join(".jj").is_dir());
    assert_eq!(
        std::fs::read(tertiary_dir.root().join(".jj/working_copy/subvolume_mode"))?,
        b"snapshot-backed\n"
    );
    let status = tertiary_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", tertiary_dir.root())
                .args(["util", "subvolume", "status"])
        })
        .success();
    assert!(status.stdout.raw().contains("(clean-baseline,"), "{status}");
    tertiary_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", tertiary_dir.root())
                .arg("status")
        })
        .success();

    // The trace identifies a command error and its phase after the enabling
    // marker is written.
    let output = main_dir.run_jj_with(|cmd| {
        cmd.env("PATH", &path)
            .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
            .env("JJ_LOG", "info")
            .args([
                "--ignore-working-copy",
                "--config=snapshot.auto-track='glob:['",
                "workspace",
                "add",
                "--no-colocate",
                "../failed-baseline",
            ])
    });
    assert!(!output.status.success(), "{output}");
    let log = output.stderr.raw();
    assert!(
        log.lines().any(|line| line.contains("updated subvolume mode marker")
            && line.contains("enabling")),
        "{log}"
    );
    assert!(
        log.lines().any(
            |line| line.contains("snapshot workspace baseline initialization failed")
                && line.contains("seeding AWACS baseline")
                && line.contains("error_kind")
        ),
        "{log}"
    );
    assert!(log.contains("workspace add failed"), "{log}");
    assert!(
        !log.lines()
            .any(|line| line.contains("updated subvolume mode marker")
                && line.contains("snapshot-backed")),
        "{log}"
    );
    assert_eq!(
        std::fs::read(
            test_env
                .env_root()
                .join("failed-baseline/.jj/working_copy/subvolume_mode")
        )?,
        b"enabling\n"
    );

    for (option, destination, error) in [
        (
            "--no-integrate-operation",
            "../not-integrated",
            "Cannot create a snapshot-backed workspace with --no-integrate-operation",
        ),
        (
            "--at-op=@-",
            "../historical",
            "Cannot create a snapshot-backed workspace at a historical operation",
        ),
    ] {
        let output = main_dir.run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args([option, "workspace", "add", "--colocate", destination])
        });
        assert!(!output.status.success(), "{output}");
        assert!(output.stderr.raw().contains(error), "{output}");
        assert!(!main_dir.root().join(destination).exists(), "{output}");
    }

    Ok(())
}

#[cfg(unix)]
#[test]
fn test_workspaces_add_plain_workspace_when_subvolume_mode_is_disabled() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "tracked\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
touch "$0.called"
echo unexpected-btrfs-probe >&2
exit 1
"#,
    );
    let output = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .args(["--debug", "workspace", "add", "../secondary"])
        })
        .success();

    assert!(
        !output
            .stderr
            .raw()
            .contains("recorded snapshot tree as workspace baseline"),
        "plain workspace must not retain snapshot-only baseline state: {}",
        output.stderr.raw()
    );
    assert_eq!(
        std::fs::read_to_string(secondary_dir.root().join("file"))?,
        "tracked\n"
    );
    assert!(
        !test_env.env_root().join("bin/btrfs.called").exists(),
        "disabled mode must not infer snapshot semantics from filesystem topology"
    );

    // Stock workspace creation accepts an existing empty destination. Disabled
    // mode must take that same ordinary-checkout path.
    let tertiary_dir = test_env.work_dir("tertiary");
    std::fs::create_dir(tertiary_dir.root())?;
    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .args(["workspace", "add", "../tertiary"])
        })
        .success();
    assert_eq!(
        std::fs::read_to_string(tertiary_dir.root().join("file"))?,
        "tracked\n"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_workspaces_add_plain_workspace_without_btrfs_tool() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");
    let empty_path = test_env.env_root().join("empty-bin");
    std::fs::create_dir(&empty_path)?;

    main_dir.write_file("file", "tracked\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &empty_path)
                .args(["workspace", "add", "../secondary"])
        })
        .success();
    assert_eq!(
        std::fs::read_to_string(secondary_dir.root().join("file"))?,
        "tracked\n"
    );

    let tertiary_dir = test_env.work_dir("tertiary");
    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &empty_path)
                .args(["workspace", "add", "../tertiary"])
        })
        .success();
    assert_eq!(
        std::fs::read_to_string(tertiary_dir.root().join("file"))?,
        "tracked\n"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_workspaces_add_subvolume_cross_filesystem_fails() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "tracked\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir.write_file(".jj/working_copy/subvolume_mode", "snapshot-backed\n");

    // Model an enabled source whose destination is on a different filesystem.
    // Both Btrfs and the source baseline are synthetic in this fixture.
    // Persistent subvolume mode must preserve the Btrfs error.
    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "snapshot" ]; then
    echo 'Invalid cross-device link' >&2
    exit 1
fi
exit 1
"#,
    );

    let output = main_dir.run_jj_with(|cmd| {
        cmd.env("PATH", &path)
            .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
            .args(["--ignore-working-copy", "workspace", "add", "../secondary"])
    });
    assert!(!output.status.success(), "{output}");
    assert!(
        output.stderr.raw().contains("Invalid cross-device link"),
        "{output}"
    );
    assert!(!test_env.env_root().join("secondary").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_workspaces_add_plain_workspace_when_btrfs_path_is_not_subvolume() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
echo not-a-subvolume >&2
exit 1
"#,
    );

    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .args(["workspace", "add", "../secondary"])
        })
        .success();
    assert!(test_env.env_root().join("secondary").join(".jj").is_dir());
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_util_subvolume_enable_disable_real_btrfs_awacs() -> TestResult {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use btrfs_awacs::store::Store;

    let btrfs_root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
        .map(std::path::PathBuf::from)
        .expect("JJ_TEST_BTRFS_ROOT must name a writable directory on real Btrfs");
    let awacs_command = std::env::var_os("BTRFS_AWACS_COMMAND")
        .expect("BTRFS_AWACS_COMMAND must name the real awacs binary");
    let broker_socket = std::env::var_os("BTRFS_AWACS_BROKER_SOCKET")
        .expect("BTRFS_AWACS_BROKER_SOCKET must name a matching live AWACS broker");
    assert!(
        btrfs_root.is_absolute(),
        "JJ_TEST_BTRFS_ROOT must be absolute"
    );
    assert!(
        Command::new("btrfs")
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR")
            .args(["inspect-internal", "rootid"])
            .arg(&btrfs_root)
            .status()?
            .success(),
        "JJ_TEST_BTRFS_ROOT is not on Btrfs"
    );

    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let repo_root = btrfs_root.join(format!("jj-subvolume-e2e-{suffix}"));
    let secondary_root = btrfs_root.join(format!("jj-subvolume-secondary-e2e-{suffix}"));
    let failed_root = btrfs_root.join(format!("jj-subvolume-failed-e2e-{suffix}"));
    let adopted_root = btrfs_root.join(format!("jj-subvolume-adopted-e2e-{suffix}"));
    let awacs_state = btrfs_root.join(format!(".jj-subvolume-e2e-awacs-{suffix}"));
    std::fs::create_dir(&repo_root)?;
    std::fs::create_dir(&awacs_state)?;
    // Namespace scan sockets live below XDG_RUNTIME_DIR and Unix socket paths
    // are short; keep this outside the descriptive Btrfs fixture pathname.
    let runtime_dir = std::env::temp_dir().join(format!("jj-awacs-{suffix}"));
    std::fs::create_dir(&runtime_dir)?;
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))?;
    let mut fixture_cleanup =
        RealBtrfsFixtureCleanup::new(repo_root.clone(), awacs_state.clone(), runtime_dir.clone());
    fixture_cleanup.track_subvolume_root(secondary_root.clone());
    fixture_cleanup.track_subvolume_root(failed_root.clone());
    fixture_cleanup.track_subvolume_root(adopted_root.clone());

    let git_fsmonitor_hook = runtime_dir.join("git-fsmonitor-awacs");
    std::os::unix::fs::symlink(&awacs_command, &git_fsmonitor_hook)?;
    let mut paths = vec![runtime_dir.clone()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths)?;

    let configure_awacs = |cmd: &mut assert_cmd::Command| {
        cmd.env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR")
            .env("PATH", &path)
            .env("BTRFS_AWACS_COMMAND", &awacs_command)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("AWACS_LOG_FILE", awacs_state.join("daemon.log"))
            .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket);
    };

    let test_env = TestEnvironment::default();
    let main_dir = test_env.work_dir(&repo_root);
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["git", "init", "--colocate", "."])
        })
        .success();
    main_dir.write_file("file", "tracked\n");
    main_dir.write_file(".gitignore", "tracked-ignored\nignored-untracked\n");
    main_dir.write_file("tracked-large", &"x".repeat(1024 * 1024 + 1));
    main_dir.write_file("tracked-ignored", "tracked despite ignore\n");
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args([
                "file",
                "track",
                "--include-ignored",
                "tracked-large",
                "tracked-ignored",
            ])
        })
        .success();
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["commit", "-m", "initial"])
        })
        .success();
    main_dir.write_file("ignored-untracked", "leave me untracked\n");

    let output = main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["util", "subvolume", "enable"])
        })
        .success();
    let activated_status = main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["util", "subvolume", "status"])
        })
        .success();
    assert!(
        activated_status.stdout.raw().contains("Consistency: OK"),
        "activation must be consistent before any normal JJ/Git scan:\n{activated_status}"
    );
    assert_default_awacs_layout(&repo_root);
    assert!(
        output
            .stderr
            .raw()
            .contains("awacs: creating initial snapshot baseline...")
    );
    assert!(
        !output.stderr.raw().contains("tracked-large"),
        "tracked large files must not be treated as new: {output}"
    );
    // A Btrfs subvolume root always has inode 256. `btrfs subvolume show`
    // also searches the B-tree for child snapshots and can therefore exit
    // unsuccessfully for an otherwise fully capable unprivileged user.
    assert!(repo_root.join(".jj/working_copy/subvolume_mode").exists());
    assert_eq!(std::fs::metadata(&repo_root)?.ino(), 256);
    assert_eq!(std::fs::metadata(repo_root.join(".git"))?.ino(), 256);
    assert_eq!(std::fs::metadata(repo_root.join(".jj"))?.ino(), 256);
    assert!(repo_root.join("ignored-untracked").is_file());

    // Enable must already publish both consumers. Switching from the default
    // boolean setting to the same explicit hook must preserve the valid Git
    // lane, so even the first ordinary Git status stays incremental.
    for (key, value) in [
        ("core.fsmonitor", git_fsmonitor_hook.as_os_str()),
        ("core.fsmonitorHookVersion", std::ffi::OsStr::new("2")),
    ] {
        let output = Command::new("git")
            .env("PATH", &path)
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR")
            .arg("config")
            .arg(key)
            .arg(value)
            .current_dir(&repo_root)
            .output()?;
        assert!(
            output.status.success(),
            "configure main Git fsmonitor {key}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let bootstrap_trace = runtime_dir.join("git-bootstrap-trace.jsonl");
    let bootstrap_status = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("AWACS_LOG_FILE", awacs_state.join("daemon.log"))
        .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
        .env("GIT_TRACE2_EVENT", &bootstrap_trace)
        .args(["status", "--porcelain"])
        .current_dir(&repo_root)
        .output()?;
    assert!(
        bootstrap_status.status.success(),
        "bootstrap main Git fsmonitor: {}",
        String::from_utf8_lossy(&bootstrap_status.stderr)
    );
    assert!(
        !std::fs::read_to_string(&bootstrap_trace)?.contains("query/trivial-response"),
        "enable must finish the authoritative bootstrap before the first Git status"
    );
    let bootstrap_index = btrfs_awacs::status::inspect_git_index(&repo_root.join(".git/index"))?;
    assert!(
        matches!(
            bootstrap_index.fsmonitor,
            btrfs_awacs::status::GitFsmonitorStatus::Baseline(_)
        ),
        "the first ordinary Git status must retain the already-established worktree-owned cursor"
    );
    let incremental_trace = runtime_dir.join("git-incremental-trace.jsonl");
    let incremental_status = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("AWACS_LOG_FILE", awacs_state.join("daemon.log"))
        .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
        .env("GIT_TRACE2_EVENT", &incremental_trace)
        .args(["status", "--porcelain"])
        .current_dir(&repo_root)
        .output()?;
    assert!(
        incremental_status.status.success(),
        "reuse bootstrapped Git fsmonitor cursor: {}",
        String::from_utf8_lossy(&incremental_status.stderr)
    );
    assert!(
        !std::fs::read_to_string(&incremental_trace)?.contains("query/trivial-response"),
        "the second Git status must remain incremental after bootstrap"
    );
    for attempt in 0..3 {
        let output = Command::new("git")
            .env("PATH", &path)
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("AWACS_LOG_FILE", awacs_state.join("daemon.log"))
            .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
            .args(["status", "--porcelain"])
            .current_dir(&repo_root)
            .output()?;
        assert!(
            output.status.success(),
            "repeat Git fsmonitor status {attempt}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("AWACS"),
            "Git may keep its old on-disk cursor when an unchanged index needs no rewrite: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for key in ["core.fsmonitor", "core.fsmonitorHookVersion"] {
        let output = Command::new("git")
            .env("PATH", &path)
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR")
            .args(["config", "--unset-all", key])
            .current_dir(&repo_root)
            .output()?;
        assert!(
            output.status.success(),
            "remove temporary main Git fsmonitor {key}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let tracked = main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["file", "list", "tracked-large", "tracked-ignored"])
        })
        .success();
    assert!(tracked.stdout.raw().contains("tracked-large"));
    assert!(tracked.stdout.raw().contains("tracked-ignored"));
    let ignored_untracked = main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["file", "list", "ignored-untracked"])
        })
        .success();
    assert!(!ignored_untracked.stdout.raw().contains("ignored-untracked"));

    let status = main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["util", "subvolume", "status"])
        })
        .success();
    assert!(status.stdout.raw().contains("Subvolume mode: enabled"));
    assert!(status.stdout.raw().contains("Working copy snapshot: awacs"));
    assert!(status.stdout.raw().contains("(clean-baseline,"));
    assert!(status.stdout.raw().contains("JJ journal: clean-baseline"));
    assert!(status.stdout.raw().contains("AWACS watch: active"));
    assert!(status.stdout.raw().contains("AWACS replay: floor="));
    assert!(status.stdout.raw().contains("JJ baseline: snapshot="));
    assert!(status.stdout.raw().contains("committed-pin=present"));

    // This is the behavior the synthetic lease cannot prove: a second command
    // must consume the real retained baseline through AWACS and remain fast.
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["status"])
        })
        .success();

    // Exercise delayed adoption after Git checks out a tree different from
    // the source snapshot. The child starts with source tree A and its JJ
    // journal, then Git materializes target tree B before `workspace adopt`
    // gets a chance to bind and reconcile the inherited baseline.
    main_dir.write_file("file", "target\n");
    main_dir.write_file("target-only", "created by target\n");
    main_dir.write_file("target-only-large", &"x".repeat(1024 * 1024 + 1));
    let stage_target = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("AWACS_LOG_FILE", awacs_state.join("daemon.log"))
        .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
        .args(["add", "file", "target-only", "target-only-large"])
        .current_dir(&repo_root)
        .output()?;
    assert!(
        stage_target.status.success(),
        "stage target Git tree: {}",
        String::from_utf8_lossy(&stage_target.stderr)
    );
    let commit_target = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("AWACS_LOG_FILE", awacs_state.join("daemon.log"))
        .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
        .args([
            "-c",
            "user.name=Test User",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "target",
        ])
        .current_dir(&repo_root)
        .output()?;
    assert!(
        commit_target.status.success(),
        "commit target Git tree: {}",
        String::from_utf8_lossy(&commit_target.stderr)
    );
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["bookmark", "create", "target", "-r", "@-"])
        })
        .success();
    let target_head = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["rev-parse", "refs/heads/target"])
        .current_dir(&repo_root)
        .output()?;
    assert!(
        target_head.status.success(),
        "resolve target Git ref: {}",
        String::from_utf8_lossy(&target_head.stderr)
    );
    let target_head = String::from_utf8(target_head.stdout)?.trim().to_owned();
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["new", "@--"])
        })
        .success();
    let source_head = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo_root)
        .output()?;
    assert!(
        source_head.status.success(),
        "resolve source Git HEAD: {}",
        String::from_utf8_lossy(&source_head.stderr)
    );
    assert_ne!(
        String::from_utf8(source_head.stdout)?.trim(),
        target_head,
        "the regression requires source HEAD A to differ from target B"
    );
    assert_eq!(
        std::fs::read_to_string(repo_root.join("file"))?,
        "tracked\n"
    );
    assert!(!repo_root.join("target-only").exists());
    assert!(!repo_root.join("target-only-large").exists());
    let source_index = std::fs::read(repo_root.join(".git/index"))?;
    assert!(
        btrfs_awacs::status::git_index_extension_data_range(&source_index, 20, b"TREE")
            .expect("the source Git index must remain parseable")
            .is_some(),
        "resetting the source JJ workspace must preserve its Git cache-tree extension"
    );

    // The Btrfs clone is only a transfer mechanism. Git must discard every
    // non-ignored source-side state before exposing the requested target.
    main_dir.write_file("file", "staged source change\n");
    let stage_source = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["add", "file"])
        .current_dir(&repo_root)
        .output()?;
    assert!(
        stage_source.status.success(),
        "stage source change: {}",
        String::from_utf8_lossy(&stage_source.stderr)
    );
    main_dir.write_file("file", "unstaged source change\n");
    main_dir.write_file("source-untracked", "discard me\n");
    let source_journal = std::fs::read(repo_root.join(".jj/working_copy/checkout"))?;
    assert!(source_journal.starts_with(b"\0JJ-WORKING-COPY-STATE\0v1\n"));

    let git_trace = awacs_state.join("dirty-worktree-git-trace.json");
    let worktree_add = Command::new(&awacs_command)
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("AWACS_LOG_FILE", awacs_state.join("daemon.log"))
        .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
        .env("GIT_TRACE2_EVENT", &git_trace)
        .args(["git", "worktree-add", "--source"])
        .arg(&repo_root)
        .arg("--destination")
        .arg(&adopted_root)
        .args([
            "--ref",
            "refs/heads/target",
            "--git",
            "git",
            "--required",
            "--detach",
        ])
        .output()?;
    assert!(
        worktree_add.status.success(),
        "create snapshot-backed Git worktree at target B failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&worktree_add.stdout),
        String::from_utf8_lossy(&worktree_add.stderr)
    );
    assert_default_awacs_layout(&adopted_root);
    assert!(
        !adopted_root.join(".jj").exists(),
        "a Git snapshot worktree must not inherit the source JJ repository"
    );
    assert_eq!(
        std::fs::read(repo_root.join(".jj/working_copy/checkout"))?,
        source_journal,
        "staging a deferred adoption journal must not modify the source"
    );
    let git_trace = std::fs::read_to_string(&git_trace)?;
    assert!(
        git_trace.contains(r#""reset","--hard""#),
        "dirty snapshot checkout must hard reset to the target: {git_trace}"
    );
    assert!(
        !git_trace.contains(r#""restore""#),
        "dirty snapshot checkout must not build a literal-pathspec restore: {git_trace}"
    );
    assert!(
        !git_trace.contains(r#""clean","-f""#),
        "dirty snapshot checkout must not scan the full tree: {git_trace}"
    );
    assert_eq!(
        std::fs::read_to_string(adopted_root.join("file"))?,
        "target\n"
    );
    assert!(adopted_root.join("target-only").is_file());
    assert_eq!(
        std::fs::metadata(adopted_root.join("target-only-large"))?.len(),
        1024 * 1024 + 1,
        "Git must materialize target-only tracked files above JJ's new-file size limit"
    );
    assert!(
        !adopted_root.join("source-untracked").exists(),
        "source untracked files must not leak into the target worktree"
    );
    assert_eq!(
        std::fs::read_to_string(repo_root.join("file"))?,
        "unstaged source change\n",
        "snapshot checkout must not modify the dirty source worktree"
    );
    assert!(repo_root.join("source-untracked").is_file());
    assert_eq!(
        std::fs::read_to_string(adopted_root.join("ignored-untracked"))?,
        "leave me untracked\n",
        "ignored source files should remain available as reusable build output"
    );
    // The snapshot-backed checkout must translate A's token into a child-
    // owned sequence-zero token before any Git reader can race on the first
    // hook query. The source token itself cannot cross worktrees because it
    // owns A's watch lane.
    let seeded_index = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["rev-parse", "--path-format=absolute", "--git-path", "index"])
        .current_dir(&adopted_root)
        .output()?;
    assert!(
        seeded_index.status.success(),
        "resolve seeded target Git index: {}",
        String::from_utf8_lossy(&seeded_index.stderr)
    );
    let seeded_index =
        std::path::PathBuf::from(String::from_utf8(seeded_index.stdout)?.trim().to_owned());
    let pending_journal = seeded_index
        .parent()
        .unwrap()
        .join("awacs-jj-pending-working-copy");
    assert_eq!(
        std::fs::read(pending_journal.join("checkout"))?,
        source_journal,
        "deferred adoption must retain the compact journal omitted by the root snapshot"
    );
    let seeded_index = std::fs::read(&seeded_index)?;
    assert!(
        btrfs_awacs::status::git_index_extension_data_range(&seeded_index, 20, b"TREE")
            .expect("the snapshot worktree's Git index must remain parseable")
            .is_some(),
        "snapshot worktree materialization must preserve its Git cache-tree extension"
    );
    let seeded_token_start = seeded_index
        .windows(b"awacs-git-v2:".len())
        .position(|window| window == b"awacs-git-v2:")
        .expect(
            "snapshot checkout must persist a destination-owned AWACS token before first status",
        );
    let seeded_token_end = seeded_index[seeded_token_start..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|offset| seeded_token_start + offset)
        .expect("the destination-owned AWACS token must be NUL terminated");
    let seeded_token = std::str::from_utf8(&seeded_index[seeded_token_start..seeded_token_end])?;
    let seeded_continuity = seeded_token
        .splitn(5, ':')
        .nth(4)
        .expect("the destination-owned AWACS token must contain a continuity field");
    assert!(
        !seeded_continuity.is_empty(),
        "snapshot checkout must persist established continuity, not an initial-baseline \
         placeholder: {seeded_token}"
    );

    // Exercise the first real hook query and require it to advance from the
    // already-seeded child token instead of widening to "/".
    let configure_hook = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["config", "core.fsmonitor"])
        .arg(&git_fsmonitor_hook)
        .current_dir(&adopted_root)
        .output()?;
    assert!(
        configure_hook.status.success(),
        "configure target Git fsmonitor hook: {}",
        String::from_utf8_lossy(&configure_hook.stderr)
    );
    let configure_hook_version = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["config", "core.fsmonitorHookVersion", "2"])
        .current_dir(&adopted_root)
        .output()?;
    assert!(
        configure_hook_version.status.success(),
        "configure target Git fsmonitor hook protocol: {}",
        String::from_utf8_lossy(&configure_hook_version.stderr)
    );
    let malformed_hook = Command::new(&git_fsmonitor_hook)
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("AWACS_LOG_FILE", awacs_state.join("daemon.log"))
        .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
        .args(["2", "not-an-awacs-cursor"])
        .current_dir(&adopted_root)
        .output()?;
    assert!(
        !malformed_hook.status.success(),
        "a malformed opaque cursor must not be treated as a verified AWACS baseline"
    );
    assert!(
        String::from_utf8_lossy(&malformed_hook.stderr)
            .contains("no AWACS baseline is proven to match the Git index"),
        "malformed cursor recovery must explain why no snapshot can safely be adopted: {}",
        String::from_utf8_lossy(&malformed_hook.stderr)
    );
    let adopted_git_status = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("AWACS_LOG_FILE", awacs_state.join("daemon.log"))
        .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket)
        .args(["status", "--porcelain"])
        .current_dir(&adopted_root)
        .output()?;
    assert!(
        adopted_git_status.status.success(),
        "inspect target Git status: {}",
        String::from_utf8_lossy(&adopted_git_status.stderr)
    );
    assert!(
        adopted_git_status.stdout.is_empty(),
        "the target Git worktree must be clean: {}",
        String::from_utf8_lossy(&adopted_git_status.stdout)
    );
    let adopted_index = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["rev-parse", "--path-format=absolute", "--git-path", "index"])
        .current_dir(&adopted_root)
        .output()?;
    assert!(
        adopted_index.status.success(),
        "resolve target Git index: {}",
        String::from_utf8_lossy(&adopted_index.stderr)
    );
    let adopted_index =
        std::path::PathBuf::from(String::from_utf8(adopted_index.stdout)?.trim().to_owned());
    let adopted_index = std::fs::read(&adopted_index)?;
    assert!(
        adopted_index
            .windows(b"awacs-git-v2:".len())
            .any(|window| window == b"awacs-git-v2:"),
        "target Git status must retain its destination-owned AWACS token"
    );
    let disable_hook = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["config", "--unset-all", "core.fsmonitor"])
        .current_dir(&adopted_root)
        .output()?;
    assert!(
        disable_hook.status.success(),
        "remove target Git fsmonitor hook: {}",
        String::from_utf8_lossy(&disable_hook.stderr)
    );
    let disable_hook_version = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["config", "--unset-all", "core.fsmonitorHookVersion"])
        .current_dir(&adopted_root)
        .output()?;
    assert!(
        disable_hook_version.status.success(),
        "remove target Git fsmonitor hook protocol: {}",
        String::from_utf8_lossy(&disable_hook_version.stderr)
    );

    let adopted_state_root = default_awacs_state_root(&adopted_root);
    let mut checked_shared_replay = false;
    for directory in std::fs::read_dir(&adopted_state_root)? {
        let manager_db = directory?.path().join("manager.sqlite3");
        if !manager_db.is_file() {
            continue;
        }
        let store = Store::open(&manager_db)?;
        let connection = store.connection();
        let (replay_floor, first_cut_sequence, retained_first_base): (i64, i64, i64) = connection
            .query_row(
            r#"SELECT w.replay_floor_seq, c.sequence,
                          EXISTS (
                              SELECT 1 FROM snapshot_pins p
                               WHERE p.snapshot_id = c.base_snapshot_id
                                 AND p.owner_kind = 'consumer-baseline'
                          )
                     FROM watches w
                     JOIN watch_cuts c ON c.watch_id = w.id
                    ORDER BY c.sequence
                    LIMIT 1"#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(
            retained_first_base, 1,
            "delayed JJ adoption must retain its original snapshot while Git advances"
        );
        assert!(
            replay_floor < first_cut_sequence,
            "shared replay must retain the first transition from the pinned JJ baseline: \
             floor={replay_floor}, first_cut={first_cut_sequence}"
        );
        checked_shared_replay = true;
    }
    assert!(
        checked_shared_replay,
        "the adopted worktree has no AWACS manager database"
    );

    let adopted_dir = test_env.work_dir(&adopted_root);
    adopted_dir.write_file("file", "edited after Git checkout\n");
    adopted_dir.write_file("adopted-untracked", "created before adoption\n");
    adopted_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["workspace", "adopt", "--name", "adopted"])
        })
        .success();
    assert!(adopted_root.join(".jj/repo").is_file());
    assert!(!adopted_root.join(".jj/working_copy/tree_state").exists());
    assert!(!pending_journal.exists());
    let adopted_status = adopted_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["status"])
        })
        .success();
    assert!(
        adopted_status.stdout.raw().contains("M file"),
        "adoption should preserve destination tracked edits: {adopted_status}"
    );
    assert!(
        adopted_status.stdout.raw().contains("A adopted-untracked"),
        "adoption should preserve destination untracked files: {adopted_status}"
    );
    assert!(
        !adopted_status.stdout.raw().contains("D target-only-large"),
        "adoption should not treat destination-tracked large files as deleted: {adopted_status}"
    );
    let adopted_parent = adopted_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["log", "-r", "@-", "--no-graph", "-T", "commit_id"])
        })
        .success();
    assert_eq!(
        adopted_parent.stdout.raw().trim(),
        target_head,
        "the adopted working-copy commit should remain based on Git target B"
    );
    // Explicit enable upgrades the adopted workspace's ordinary metadata to
    // a nested subvolume. Removing it must then handle both subvolume levels.
    adopted_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["util", "subvolume", "enable"])
        })
        .success();
    let adopted_enabled_status = adopted_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["util", "subvolume", "status"])
        })
        .success();
    assert!(
        adopted_enabled_status
            .stdout
            .raw()
            .contains("Consistency: OK"),
        "adopted activation must be consistent before another scan:\n{adopted_enabled_status}"
    );
    assert_default_awacs_layout(&adopted_root);
    assert_eq!(std::fs::metadata(adopted_root.join(".jj"))?.ino(), 256);
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["workspace", "remove", "adopted"])
        })
        .success();
    assert!(!adopted_root.exists());

    // A failure after the Btrfs snapshot and Git worktree pointer exist must
    // not strand either resource. Make the managed-snapshot parent a file so
    // descendant-root initialization fails before JJ workspace registration.
    let invalid_managed_parent = awacs_state.join("invalid-managed-parent");
    std::fs::write(&invalid_managed_parent, "not a directory")?;
    let failed_add = main_dir.run_jj_with(|cmd| {
        configure_awacs(cmd);
        cmd.env("BTRFS_AWACS_MANAGED_DIR", &invalid_managed_parent);
        cmd.args([
            "--ignore-working-copy",
            "workspace",
            "add",
            "--name",
            "failed",
            failed_root.to_str().unwrap(),
        ])
    });
    assert!(!failed_add.status.success(), "{failed_add}");
    assert!(
        failed_add
            .stderr
            .raw()
            .contains("Failed to initialize AWACS worktree"),
        "{failed_add}"
    );
    assert!(
        !failed_root.exists(),
        "failed workspace add must remove its destination subvolume"
    );
    let worktree_list = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&repo_root)
        .output()?;
    assert!(
        worktree_list.status.success(),
        "list Git worktrees after failed add: {}",
        String::from_utf8_lossy(&worktree_list.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&worktree_list.stdout)
            .contains(failed_root.to_string_lossy().as_ref()),
        "failed workspace add must prune its linked Git worktree registration"
    );

    // The OpenAI checkout enables Git's own AWACS snapshot worktree mode.
    // JJ has already made the physical snapshot here, so its temporary
    // no-checkout Git worktree must explicitly bypass that outer mechanism.
    let enable_git_snapshot_mode = Command::new("git")
        .env("PATH", &path)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_STATE_ROOT")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .args(["config", "worktree.btrfsSnapshot", "true"])
        .current_dir(&repo_root)
        .output()?;
    assert!(
        enable_git_snapshot_mode.status.success(),
        "enable Git snapshot worktree mode: {}",
        String::from_utf8_lossy(&enable_git_snapshot_mode.stderr)
    );

    let workspace_add = main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            // Exercise the real regression: the filesystem snapshot starts at
            // the source workspace's current tree, but the new workspace is
            // requested at a different tree. The child must commit its cloned
            // AWACS baseline before Git import or checkout materializes that
            // requested revision.
            cmd.args([
                "workspace",
                "add",
                "-r",
                "root()",
                "--name",
                "secondary",
                secondary_root.to_str().unwrap(),
            ])
        })
        .success();
    assert!(
        workspace_add
            .stderr
            .raw()
            .contains("awacs: publishing inherited baseline..."),
        "workspace add must share the parent map instead of walking the child: {workspace_add}"
    );
    assert!(
        !workspace_add.stderr.raw().contains("awacs: indexed "),
        "workspace add must not rebuild the child map by walking it: {workspace_add}"
    );
    let secondary_dir = test_env.work_dir(&secondary_root);
    assert_default_awacs_layout(&secondary_root);
    secondary_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["status"])
        })
        .success();

    let mut parent_store = None;
    let mut child_store = None;
    let parent_state_root = default_awacs_state_root(&repo_root);
    let child_state_root = default_awacs_state_root(&secondary_root);
    assert_ne!(
        parent_state_root, child_state_root,
        "linked worktrees must keep independent AWACS state roots"
    );
    for state_root in [&parent_state_root, &child_state_root] {
        for entry in std::fs::read_dir(state_root)? {
            let manager_db = entry?.path().join("manager.sqlite3");
            if !manager_db.is_file() {
                continue;
            }
            let store = Store::open(&manager_db)?;
            let parent_count: i64 = store.connection().query_row(
                "SELECT count(*) FROM watches WHERE live_path = ?1",
                [repo_root.as_os_str().as_bytes()],
                |row| row.get(0),
            )?;
            let child_count: i64 = store.connection().query_row(
                "SELECT count(*) FROM watches WHERE live_path = ?1",
                [secondary_root.as_os_str().as_bytes()],
                |row| row.get(0),
            )?;
            if parent_count == 1 {
                parent_store = Some((manager_db.clone(), store));
            } else if child_count == 1 {
                child_store = Some((manager_db.clone(), store));
            }
        }
    }
    let (parent_db, _parent_store) = parent_store.expect("parent AWACS store should exist");
    let (child_db, child_store) = child_store.expect("child AWACS store should exist");
    assert_ne!(
        parent_db, child_db,
        "worktree initialization should create a fresh SQLite store"
    );
    let child_watch_count: i64 =
        child_store
            .connection()
            .query_row("SELECT count(*) FROM watches", [], |row| row.get(0))?;
    assert_eq!(
        child_watch_count, 1,
        "child SQLite must contain only itself"
    );
    let (child_revision_id, current_child_snapshot_path): (i64, Vec<u8>) =
        child_store.connection().query_row(
            r#"SELECT w.indexed_revision_id,
                  w.last_cut_snapshot_id, s.path
             FROM watches w
             JOIN snapshots s ON s.id = w.last_cut_snapshot_id
            WHERE w.live_path = ?1 AND w.state = 'active'"#,
            [secondary_root.as_os_str().as_bytes()],
            |row| Ok((row.get(0)?, row.get(2)?)),
        )?;
    assert!(
        std::path::Path::new(std::ffi::OsStr::from_bytes(&current_child_snapshot_path)).exists(),
        "the current child-owned snapshot should exist physically"
    );
    // The first child snapshot pathname and its copied parent revision may
    // already be reclaimed after the seeded JJ cursor advances to a newer
    // child cut. The no-walk bootstrap progress above proves that the child
    // map came from the parent; the current child revision proves the
    // resulting independent store remains usable after checkout.
    let child_revision_state: String = child_store.connection().query_row(
        "SELECT state FROM revisions WHERE id = ?1",
        [child_revision_id],
        |row| row.get(0),
    )?;
    assert_eq!(
        child_revision_state, "ready",
        "the child must retain a usable indexed revision"
    );

    // Main metadata conversion cannot preserve a live linked workspace's
    // physical AWACS snapshots. Reject it before changing either workspace's
    // marker, committed cursor, Git index, or manager identity.
    let mut before_disable = Vec::new();
    for root in [&repo_root, &secondary_root] {
        for name in ["subvolume_mode", "checkout"] {
            let path = root.join(".jj/working_copy").join(name);
            before_disable.push((path.clone(), std::fs::read(path)?));
        }
        let index = gix::open(root)?.git_dir().join("index");
        before_disable.push((index.clone(), std::fs::read(index)?));
    }
    let parent_manager_identity = jj_lib::file_util::FileIdentity::from_symlink_path(&parent_db)?;
    let child_manager_identity = jj_lib::file_util::FileIdentity::from_symlink_path(&child_db)?;
    let rejected_disable = main_dir.run_jj_with(|cmd| {
        configure_awacs(cmd);
        cmd.args(["util", "subvolume", "disable"])
    });
    assert!(!rejected_disable.status.success(), "{rejected_disable}");
    assert!(
        rejected_disable
            .stderr
            .raw()
            .contains("Cannot disable the main workspace while linked Git worktree AWACS state"),
        "{rejected_disable}"
    );
    for (path, before) in before_disable {
        assert_eq!(
            std::fs::read(&path)?,
            before,
            "rejected disable must preserve {}",
            path.display()
        );
    }
    assert_eq!(
        jj_lib::file_util::FileIdentity::from_symlink_path(&parent_db)?,
        parent_manager_identity
    );
    assert_eq!(
        jj_lib::file_util::FileIdentity::from_symlink_path(&child_db)?,
        child_manager_identity
    );

    // Removing the worktree retires its snapshots and private broker trash
    // without copying them, then removes its Git administrative directory.
    let child_state_identity =
        jj_lib::file_util::FileIdentity::from_symlink_path(&child_state_root)?;
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["workspace", "remove", "secondary"])
        })
        .success();
    assert!(!secondary_root.exists());
    let retired_state =
        assert_awacs_state_retired(&repo_root, &child_state_root, &child_state_identity);
    let retired_manager = retired_state.join(child_db.strip_prefix(&child_state_root)?);
    assert_eq!(
        jj_lib::file_util::FileIdentity::from_symlink_path(retired_manager)?,
        child_manager_identity,
        "retirement must preserve the removed workspace's manager file"
    );

    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["util", "subvolume", "disable"])
        })
        .success();
    assert!(repo_root.join("file").is_file());
    assert!(!repo_root.join(".jj/working_copy/subvolume_mode").exists());
    assert_ne!(std::fs::metadata(repo_root.join(".jj"))?.ino(), 256);
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["status"])
        })
        .success();
    fixture_cleanup.cleanup()?;
    Ok(())
}

/// Replays one deterministic working-copy workload through several degrees of
/// real process concurrency. The width-1 run is the sequential oracle, while
/// the explicit expected map prevents all variants from agreeing on the same
/// wrong result.
#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_awacs_real_btrfs_deterministic_concurrent_mutations() -> TestResult {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Command;
    use std::process::Output;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    #[derive(Clone)]
    struct StressCommand {
        jj: PathBuf,
        awacs: OsString,
        path: OsString,
        home: PathBuf,
        config: PathBuf,
        tmp: PathBuf,
        runtime: PathBuf,
        log: PathBuf,
        broker_socket: OsString,
    }

    impl StressCommand {
        fn run(&self, root: &Path, seed: usize, args: &[&str]) -> Output {
            let timestamp = format!("2001-02-03T04:05:{:02}+07:00", seed % 60);
            Command::new(&self.jj)
                .env_remove("BTRFS_AWACS_STATE_DIR")
                .env_remove("BTRFS_AWACS_STATE_ROOT")
                .env_remove("BTRFS_AWACS_MANAGED_DIR")
                .current_dir(root)
                .env_clear()
                .env("PATH", &self.path)
                .env("COLUMNS", "100")
                .env("HOME", &self.home)
                .env("TMPDIR", &self.tmp)
                .env("JJ_CONFIG", &self.config)
                .env("JJ_USER", "Test User")
                .env("JJ_EMAIL", "test.user@example.com")
                .env("JJ_OP_HOSTNAME", "host.example.com")
                .env("JJ_OP_USERNAME", "test-username")
                .env("JJ_TZ_OFFSET_MINS", "660")
                .env("JJ_RANDOMNESS_SEED", seed.to_string())
                .env("JJ_TIMESTAMP", &timestamp)
                .env("JJ_OP_TIMESTAMP", &timestamp)
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("BTRFS_AWACS_COMMAND", &self.awacs)
                .env("XDG_RUNTIME_DIR", &self.runtime)
                .env("AWACS_LOG_FILE", &self.log)
                .env("BTRFS_AWACS_BROKER_SOCKET", &self.broker_socket)
                .args(args)
                .output()
                .unwrap()
        }
    }

    #[derive(Clone, Debug)]
    enum Mutation {
        Write(String, String),
        Remove(String),
        Rename(String, String),
        Transient(String),
    }

    impl Mutation {
        fn apply(&self, root: &Path) -> std::io::Result<()> {
            let ensure_parent = |path: &Path| -> std::io::Result<()> {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                Ok(())
            };
            match self {
                Self::Write(path, contents) => {
                    let path = root.join(path);
                    ensure_parent(&path)?;
                    std::fs::write(path, contents)
                }
                Self::Remove(path) => std::fs::remove_file(root.join(path)),
                Self::Rename(from, to) => {
                    let to = root.join(to);
                    ensure_parent(&to)?;
                    std::fs::rename(root.join(from), to)
                }
                Self::Transient(path) => {
                    let path = root.join(path);
                    ensure_parent(&path)?;
                    std::fs::write(&path, "must net out\n")?;
                    // Give sibling status processes a deterministic chance to
                    // observe the intermediate file. The final tree must not
                    // depend on whether any one of them did.
                    thread::sleep(Duration::from_millis(20));
                    std::fs::remove_file(path)
                }
            }
        }
    }

    fn phases(round: usize) -> Vec<Vec<Mutation>> {
        let prefix = format!("round-{round}");
        vec![
            vec![
                Mutation::Write(
                    format!("{prefix}/modify.txt"),
                    format!("modified-{round}\n"),
                ),
                Mutation::Write(format!("{prefix}/added.txt"), format!("added-{round}\n")),
                Mutation::Remove(format!("{prefix}/remove.txt")),
                Mutation::Rename(
                    format!("{prefix}/rename.txt"),
                    format!("{prefix}/renamed.txt"),
                ),
            ],
            vec![
                Mutation::Write(format!("{prefix}/nested/a.txt"), format!("a-{round}\n")),
                Mutation::Write(format!("{prefix}/nested/b.txt"), format!("b-{round}\n")),
                Mutation::Write(
                    format!("{prefix}/dir/base.txt"),
                    format!("base-modified-{round}\n"),
                ),
                Mutation::Transient(format!("{prefix}/transient.txt")),
            ],
            vec![Mutation::Rename(
                format!("{prefix}/dir"),
                format!("{prefix}/moved"),
            )],
            vec![Mutation::Write(
                format!("{prefix}/moved/after.txt"),
                format!("after-{round}\n"),
            )],
        ]
    }

    fn expected_tree(rounds: usize) -> BTreeMap<String, Vec<u8>> {
        let mut expected =
            BTreeMap::from([("seed/unchanged.txt".to_owned(), b"unchanged\n".to_vec())]);
        for round in 0..rounds {
            let prefix = format!("round-{round}");
            for (suffix, contents) in [
                ("modify.txt", format!("modified-{round}\n")),
                ("added.txt", format!("added-{round}\n")),
                ("renamed.txt", format!("rename-{round}\n")),
                ("nested/a.txt", format!("a-{round}\n")),
                ("nested/b.txt", format!("b-{round}\n")),
                ("moved/base.txt", format!("base-modified-{round}\n")),
                ("moved/after.txt", format!("after-{round}\n")),
            ] {
                expected.insert(format!("{prefix}/{suffix}"), contents.into_bytes());
            }
        }
        expected
    }

    fn assert_success(output: &Output, context: &str) {
        assert!(
            output.status.success(),
            "{context} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn run_workload(command: &StressCommand, root: &Path, width: usize, rounds: usize) {
        let mut seed = width * 10_000;
        for round in 0..rounds {
            for phase in phases(round) {
                for chunk in phase.chunks(width) {
                    let barrier = Arc::new(Barrier::new(chunk.len()));
                    let mut workers = Vec::new();
                    for mutation in chunk {
                        let command = command.clone();
                        let root = root.to_owned();
                        let mutation = mutation.clone();
                        let barrier = barrier.clone();
                        seed += 1;
                        let worker_seed = seed;
                        workers.push(thread::spawn(move || {
                            barrier.wait();
                            mutation.apply(&root).unwrap();
                            // A stable stagger broadens the set of scan vs.
                            // mutation interleavings without introducing
                            // randomness into the expected endpoint.
                            thread::sleep(Duration::from_millis((worker_seed % 4) as u64 * 5));
                            let output = command.run(&root, worker_seed, &["status"]);
                            (mutation, output)
                        }));
                    }
                    for worker in workers {
                        let (mutation, output) = worker.join().unwrap();
                        assert_success(&output, &format!("status after {mutation:?}"));
                    }
                }
            }
        }
    }

    fn observed_tree(
        command: &StressCommand,
        root: &Path,
        seed: usize,
    ) -> BTreeMap<String, Vec<u8>> {
        let status = command.run(root, seed, &["status"]);
        assert_success(&status, "final settling status");
        let listed = command.run(root, seed + 1, &["file", "list"]);
        assert_success(&listed, "final file list");
        String::from_utf8(listed.stdout.clone())
            .unwrap()
            .lines()
            .filter(|path| !path.is_empty())
            .map(|path| {
                let shown = command.run(root, seed + 2, &["file", "show", path]);
                assert_success(&shown, &format!("show final path {path}"));
                (path.to_owned(), shown.stdout)
            })
            .collect()
    }

    let rounds = std::env::var("JJ_TEST_AWACS_STRESS_ROUNDS")
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .expect("stress rounds must be an integer")
        })
        .unwrap_or(3);
    assert!(rounds > 0, "stress rounds must be positive");
    let btrfs_root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
        .map(PathBuf::from)
        .expect("JJ_TEST_BTRFS_ROOT must name a writable directory on real Btrfs");
    let awacs_command = std::env::var_os("BTRFS_AWACS_COMMAND")
        .expect("BTRFS_AWACS_COMMAND must name the real awacs binary");
    let broker_socket = std::env::var_os("BTRFS_AWACS_BROKER_SOCKET")
        .expect("BTRFS_AWACS_BROKER_SOCKET must name a matching live AWACS broker");
    assert!(
        Command::new("btrfs")
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR")
            .args(["inspect-internal", "rootid"])
            .arg(&btrfs_root)
            .status()?
            .success(),
        "JJ_TEST_BTRFS_ROOT is not on Btrfs"
    );

    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let repo_root = btrfs_root.join(format!("jj-awacs-stress-{suffix}"));
    let awacs_state = btrfs_root.join(format!(".jj-awacs-stress-state-{suffix}"));
    let runtime_dir = std::env::temp_dir().join(format!("jj-awacs-stress-{suffix}"));
    std::fs::create_dir(&repo_root)?;
    std::fs::create_dir(&awacs_state)?;
    std::fs::create_dir(&runtime_dir)?;
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))?;
    let mut fixture_cleanup =
        RealBtrfsFixtureCleanup::new(repo_root.clone(), awacs_state.clone(), runtime_dir.clone());

    let git_fsmonitor_hook = runtime_dir.join("git-fsmonitor-awacs");
    std::os::unix::fs::symlink(&awacs_command, &git_fsmonitor_hook)?;
    let mut paths = vec![runtime_dir.clone()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths)?;

    let configure_awacs = |cmd: &mut assert_cmd::Command| {
        cmd.env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR")
            .env("PATH", &path)
            .env("BTRFS_AWACS_COMMAND", &awacs_command)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("AWACS_LOG_FILE", awacs_state.join("daemon.log"))
            .env("BTRFS_AWACS_BROKER_SOCKET", &broker_socket);
    };

    let test_env = TestEnvironment::default();
    let main_dir = test_env.work_dir(&repo_root);
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["git", "init", "--colocate", "."])
        })
        .success();
    main_dir.write_file("seed/unchanged.txt", "unchanged\n");
    for round in 0..rounds {
        main_dir.write_file(
            format!("round-{round}/modify.txt"),
            format!("original-{round}\n"),
        );
        main_dir.write_file(
            format!("round-{round}/remove.txt"),
            format!("remove-{round}\n"),
        );
        main_dir.write_file(
            format!("round-{round}/rename.txt"),
            format!("rename-{round}\n"),
        );
        main_dir.write_file(
            format!("round-{round}/dir/base.txt"),
            format!("base-{round}\n"),
        );
    }
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["commit", "-m", "stress base"])
        })
        .success();
    let base_commit = main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["log", "-r", "@-", "--no-graph", "-T", "commit_id"])
        })
        .success()
        .stdout
        .raw()
        .trim()
        .to_owned();

    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["util", "subvolume", "enable"])
        })
        .success();
    let activated_status = main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["util", "subvolume", "status"])
        })
        .success();
    assert!(
        activated_status.stdout.raw().contains("Consistency: OK"),
        "stress activation must be consistent before any ordinary scan:\n{activated_status}"
    );
    assert_default_awacs_layout(&repo_root);

    let stress_command = StressCommand {
        jj: TestEnvironment::jj_executable(),
        awacs: awacs_command.clone(),
        path: path.clone(),
        home: test_env.home_dir().to_owned(),
        config: test_env.config_path().to_owned(),
        tmp: test_env.env_root().join("tmp"),
        runtime: runtime_dir.clone(),
        log: awacs_state.join("daemon.log"),
        broker_socket: broker_socket.clone(),
    };
    let expected = expected_tree(rounds);
    let mut sequential = None;
    let mut workspace_names = Vec::new();
    for width in [1, 2, 4] {
        let workspace_name = format!("stress-w{width}");
        let workspace_root = btrfs_root.join(format!("jj-awacs-stress-{suffix}-w{width}"));
        fixture_cleanup.track_subvolume_root(workspace_root.clone());
        main_dir
            .run_jj_with(|cmd| {
                configure_awacs(cmd);
                cmd.args([
                    "workspace",
                    "add",
                    "-r",
                    &base_commit,
                    "--name",
                    &workspace_name,
                    workspace_root.to_str().unwrap(),
                ])
            })
            .success();
        assert_default_awacs_layout(&workspace_root);
        run_workload(&stress_command, &workspace_root, width, rounds);
        let observed = observed_tree(&stress_command, &workspace_root, width * 100_000);
        assert_eq!(
            observed, expected,
            "width {width} disagrees with expected tree"
        );
        if let Some(sequential) = &sequential {
            assert_eq!(
                &observed, sequential,
                "width {width} disagrees with the sequential result"
            );
        } else {
            sequential = Some(observed);
        }
        workspace_names.push((workspace_name, workspace_root));
    }

    for (workspace_name, workspace_root) in workspace_names {
        let state_root = default_awacs_state_root(&workspace_root);
        let state_identity = jj_lib::file_util::FileIdentity::from_symlink_path(&state_root)?;
        main_dir
            .run_jj_with(|cmd| {
                configure_awacs(cmd);
                cmd.args(["workspace", "remove", &workspace_name])
            })
            .success();
        assert_awacs_state_retired(&repo_root, &state_root, &state_identity);
    }
    main_dir
        .run_jj_with(|cmd| {
            configure_awacs(cmd);
            cmd.args(["util", "subvolume", "disable"])
        })
        .success();
    fixture_cleanup.cleanup()?;
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_enable_disable() -> TestResult {
    check_util_subvolume_enable_disable(false, ".")
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_enable_disable_with_nested_jj() -> TestResult {
    check_util_subvolume_enable_disable(true, ".")
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_enable_disable_from_outside_workspace() -> TestResult {
    check_util_subvolume_enable_disable(false, "../outside")
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_enable_disable_from_subdirectory() -> TestResult {
    check_util_subvolume_enable_disable(false, "nested")
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_missing_baseline_does_not_import_external_git_head() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    main_dir.write_file("tracked", "contents\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    let output = std::process::Command::new("git")
        .current_dir(main_dir.root())
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "Test Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .args(["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "external"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let external_head = String::from_utf8(output.stdout)?.trim().to_owned();
    let output = std::process::Command::new("git")
        .current_dir(main_dir.root())
        .args(["update-ref", "HEAD", &external_head])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(
        main_dir.root().join(".jj/working_copy/subvolume_mode"),
        b"enabling\n",
    )?;

    let read_working_copy = || {
        main_dir
            .run_jj([
                "--ignore-working-copy",
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                "commit_id",
            ])
            .success()
            .stdout
            .raw()
            .to_owned()
    };
    let read_operation = || {
        main_dir
            .run_jj([
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "--limit=1",
                "-T",
                "id",
            ])
            .success()
            .stdout
            .raw()
            .to_owned()
    };
    let working_copy_before = read_working_copy();
    let operation_before = read_operation();
    let output = main_dir.run_jj_with(|cmd| {
        cmd.env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
            .arg("status")
    });
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("jj util subvolume enable --rebuild-baseline"),
        "{output}"
    );
    assert_eq!(read_working_copy(), working_copy_before);
    assert_eq!(read_operation(), operation_before);
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test_case("enabling", true; "interrupted_enable_colocated")]
#[test_case("snapshot-backed", true; "enabled_colocated")]
#[test_case("enabling", false; "interrupted_enable_non_colocated")]
#[test_case("snapshot-backed", false; "enabled_non_colocated")]
fn test_util_subvolume_rebuild_missing_baseline(marker: &str, colocated: bool) -> TestResult {
    let test_env = TestEnvironment::default();
    let init_args = if colocated {
        vec!["git", "init", "--colocate", "main"]
    } else {
        vec!["git", "init", "main"]
    };
    test_env.run_jj_in(".", init_args).success();
    let main_dir = test_env.work_dir("main");
    main_dir.write_file("ignored-tracked", "tracked before ignore\n");
    main_dir.write_file("oversized-tracked", "tracked before size limit\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir.write_file(".gitignore", "ignored-tracked\n");
    main_dir
        .run_jj(["commit", "-m", "ignore tracked file"])
        .success();
    main_dir.write_file("ignored-tracked", "unsnapshotted ignored edit\n");
    main_dir.write_file(
        "oversized-tracked",
        "unsnapshotted oversized edit that exceeds the size limit\n",
    );
    main_dir.write_file("created-before-rebuild", "preserved\n");
    let marker_path = main_dir.root().join(".jj/working_copy/subvolume_mode");
    std::fs::write(&marker_path, format!("{marker}\n"))?;
    let checkout_path = main_dir.root().join(".jj/working_copy/checkout");
    let checkout_before = std::fs::read(&checkout_path)?;

    // An ordinary command must stop before traversing the whole working copy.
    let output = main_dir.run_jj_with(|cmd| {
        cmd.env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
            .args(["--config=snapshot.max-new-file-size=32", "status"])
    });
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("jj util subvolume enable --rebuild-baseline"),
        "{output}"
    );
    assert_eq!(std::fs::read(&checkout_path)?, checkout_before);
    assert_eq!(
        std::fs::read(&marker_path)?,
        format!("{marker}\n").as_bytes()
    );

    let output = main_dir
        .run_jj_with(|cmd| {
            cmd.env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root()).args([
                "--config=snapshot.max-new-file-size=32",
                "util",
                "subvolume",
                "enable",
                "--rebuild-baseline",
            ])
        })
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("awacs: rebuilt committed snapshot baseline."),
        "{output}"
    );
    assert_eq!(std::fs::read(&marker_path)?, b"snapshot-backed\n");
    let restored = main_dir
        .run_jj_with(|cmd| {
            cmd.env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root()).args([
                "--ignore-working-copy",
                "file",
                "show",
                "created-before-rebuild",
            ])
        })
        .success();
    assert_eq!(restored.stdout.raw(), "preserved\n");
    for (path, expected) in [
        ("ignored-tracked", "unsnapshotted ignored edit\n"),
        (
            "oversized-tracked",
            "unsnapshotted oversized edit that exceeds the size limit\n",
        ),
    ] {
        let restored = main_dir
            .run_jj_with(|cmd| {
                cmd.env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root()).args([
                    "--ignore-working-copy",
                    "file",
                    "show",
                    path,
                ])
            })
            .success();
        assert_eq!(restored.stdout.raw(), expected, "{path}");
    }
    main_dir
        .run_jj_with(|cmd| {
            cmd.env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["--config=snapshot.max-new-file-size=32", "status"])
        })
        .success();
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_rebuild_interrupted_workspace_add_preserves_conflict() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let child_dir = test_env.work_dir("child");
    let empty_tree_state = std::fs::read(main_dir.root().join(".jj/working_copy/tree_state"))?;

    main_dir.write_file("conflicted", "base\n");
    main_dir.write_file("ignored-tracked", "tracked before ignore\n");
    main_dir.write_file("oversized-tracked", "tracked before size limit\n");
    main_dir.run_jj(["commit", "-m", "base"]).success();
    main_dir.write_file(".gitignore", "ignored-tracked\n");
    main_dir.write_file("conflicted", "left\n");
    main_dir.run_jj(["describe", "-m", "left"]).success();
    main_dir.run_jj(["bookmark", "create", "left"]).success();
    main_dir.run_jj(["new", "@-"]).success();
    main_dir.write_file(".gitignore", "ignored-tracked\n");
    main_dir.write_file("conflicted", "right\n");
    main_dir.run_jj(["describe", "-m", "right"]).success();
    main_dir.run_jj(["bookmark", "create", "right"]).success();
    main_dir.run_jj(["new", "left", "right"]).success();
    let conflict_count = main_dir
        .run_jj([
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "self.conflicted_files().len()",
        ])
        .success();
    assert_eq!(conflict_count.stdout.raw(), "1");

    // Create an actual snapshot workspace so the repository contains both
    // the add-workspace operation and its baseline-recording child.
    main_dir.write_file(".jj/working_copy/subvolume_mode", "snapshot-backed\n");
    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    exit 0
fi
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "snapshot" ]; then
    /bin/cp -a "$3" "$4"
    exit $?
fi
exit 1
"#,
    );
    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "enable", "--rebuild-baseline"])
        })
        .success();
    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["workspace", "add", "--no-colocate", "-r", "@", "../child"])
        })
        .success();
    let operations = main_dir
        .run_jj([
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "-T",
            "id ++ \" \" ++ description ++ \"\\n\"",
        ])
        .success();
    let operation_id = |description: &str| {
        operations
            .stdout
            .raw()
            .lines()
            .find_map(|line| {
                let (id, actual_description) = line.split_once(' ')?;
                (actual_description == description).then_some(id.to_owned())
            })
            .unwrap_or_else(|| panic!("missing {description}: {operations}"))
    };
    let add_operation = operation_id("add workspace 'child'");
    let baseline_operation = operation_id("record snapshot baseline in workspace child");
    main_dir
        .run_jj(["--ignore-working-copy", "op", "restore", &add_operation])
        .success();

    // Restore the initial, empty legacy journal that workspace initialization
    // would have written before seeding. The checkout file is a protobuf:
    // field 2 is the operation ID and field 3 is the workspace name.
    let operation_bytes: Vec<_> = add_operation
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    let mut checkout = vec![0x12, u8::try_from(operation_bytes.len())?];
    checkout.extend(operation_bytes);
    checkout.extend([0x1a, 5]);
    checkout.extend(b"child");
    std::fs::write(child_dir.root().join(".jj/working_copy/checkout"), checkout)?;
    std::fs::write(
        child_dir.root().join(".jj/working_copy/tree_state"),
        empty_tree_state,
    )?;
    std::fs::write(
        child_dir.root().join(".jj/working_copy/subvolume_mode"),
        b"enabling\n",
    )?;
    let marker_text = std::fs::read_to_string(child_dir.root().join("conflicted"))?;
    assert!(marker_text.contains("left\n"), "{marker_text}");
    child_dir.write_file(
        "conflicted",
        &marker_text.replace("left\n", "edited-left\n"),
    );
    child_dir.write_file("ignored-tracked", "unsnapshotted ignored edit\n");
    child_dir.write_file(
        "oversized-tracked",
        "unsnapshotted oversized edit exceeding the limit\n",
    );

    // The add operation's empty tree matches the empty journal, but the
    // physical snapshot contains inherited tracked files and conflict data.
    // Until the source tree is recorded, a rebuild must not assume those
    // files are untracked and silently reclassify them.
    let checkout_before = std::fs::read(child_dir.root().join(".jj/working_copy/checkout"))?;
    let tree_state_before = std::fs::read(child_dir.root().join(".jj/working_copy/tree_state"))?;
    let operation_before = main_dir.current_operation_id();
    let conflicted_before = child_dir.read_file("conflicted");
    let ignored_before = child_dir.read_file("ignored-tracked");
    let oversized_before = child_dir.read_file("oversized-tracked");
    let output = child_dir.run_jj_with(|cmd| {
        cmd.env("JJ_TEST_AWACS_SCAN_ROOT", child_dir.root()).args([
            "--config=snapshot.max-new-file-size=32",
            "util",
            "subvolume",
            "enable",
            "--rebuild-baseline",
        ])
    });
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("did not record the inherited source tree"),
        "{output}"
    );
    assert_eq!(main_dir.current_operation_id(), operation_before);
    assert_eq!(
        std::fs::read(child_dir.root().join(".jj/working_copy/checkout"))?,
        checkout_before
    );
    assert_eq!(
        std::fs::read(child_dir.root().join(".jj/working_copy/tree_state"))?,
        tree_state_before
    );
    assert_eq!(
        std::fs::read(child_dir.root().join(".jj/working_copy/subvolume_mode"))?,
        b"enabling\n"
    );
    assert_eq!(child_dir.read_file("conflicted"), conflicted_before);
    assert_eq!(child_dir.read_file("ignored-tracked"), ignored_before);
    assert_eq!(child_dir.read_file("oversized-tracked"), oversized_before);

    // Once the source commit was recorded, rebuilding can safely recover it.
    main_dir
        .run_jj([
            "--ignore-working-copy",
            "op",
            "restore",
            &baseline_operation,
        ])
        .success();
    child_dir
        .run_jj_with(|cmd| {
            cmd.env("JJ_TEST_AWACS_SCAN_ROOT", child_dir.root()).args([
                "--config=snapshot.max-new-file-size=32",
                "util",
                "subvolume",
                "enable",
                "--rebuild-baseline",
            ])
        })
        .success();
    let output = child_dir
        .run_jj([
            "--ignore-working-copy",
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "self.conflicted_files().len()",
        ])
        .success();
    assert_eq!(output.stdout.raw(), "1", "rebuild must retain the conflict");
    let file = child_dir
        .run_jj(["--ignore-working-copy", "file", "show", "conflicted"])
        .success();
    assert!(file.stdout.raw().contains("edited-left\n"), "{file}");
    for (path, expected) in [
        ("ignored-tracked", "unsnapshotted ignored edit\n"),
        (
            "oversized-tracked",
            "unsnapshotted oversized edit exceeding the limit\n",
        ),
    ] {
        let file = child_dir
            .run_jj(["--ignore-working-copy", "file", "show", path])
            .success();
        assert_eq!(file.stdout.raw(), expected, "{path}");
    }
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_rebuild_refuses_stale_working_copy() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");
    main_dir.write_file("file", "original\n");
    main_dir.run_jj(["new"]).success();
    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    // Squashing from the other workspace rewrites the main workspace's
    // commit, leaving its files and journal at the previous operation.
    secondary_dir.write_file("file", "changed in secondary\n");
    secondary_dir.run_jj(["squash"]).success();
    main_dir.write_file("file", "unsnapshotted local edit\n");
    let marker_path = main_dir.root().join(".jj/working_copy/subvolume_mode");
    let checkout_path = main_dir.root().join(".jj/working_copy/checkout");
    let tree_state_path = main_dir.root().join(".jj/working_copy/tree_state");
    std::fs::write(&marker_path, b"enabling\n")?;
    let checkout_before = std::fs::read(&checkout_path)?;
    let tree_state_before = std::fs::read(&tree_state_path)?;
    let read_working_copy = || {
        secondary_dir
            .run_jj([
                "--ignore-working-copy",
                "log",
                "-r",
                "default@",
                "--no-graph",
                "-T",
                "commit_id",
            ])
            .success()
            .stdout
            .raw()
            .to_owned()
    };
    let working_copy_before = read_working_copy();
    let operation_before = main_dir.current_operation_id();
    let output = main_dir.run_jj_with(|cmd| {
        cmd.env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root()).args([
            "util",
            "subvolume",
            "enable",
            "--rebuild-baseline",
        ])
    });
    assert!(!output.status.success(), "{output}");
    assert!(output.stderr.raw().contains("stale"), "{output}");
    assert_eq!(read_working_copy(), working_copy_before);
    assert_eq!(main_dir.current_operation_id(), operation_before);
    assert_eq!(std::fs::read(checkout_path)?, checkout_before);
    assert_eq!(std::fs::read(tree_state_path)?, tree_state_before);
    assert_eq!(std::fs::read(marker_path)?, b"enabling\n");
    assert_eq!(main_dir.read_file("file"), "unsnapshotted local edit\n");
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_rebuild_preserves_journal_when_manager_is_missing() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    main_dir.write_file("tracked", "contents\n");
    main_dir.run_jj(["status"]).success();
    main_dir.write_file("tracked", "unsnapshotted edit\n");
    let marker_path = main_dir.root().join(".jj/working_copy/subvolume_mode");
    let checkout_path = main_dir.root().join(".jj/working_copy/checkout");
    let tree_state_path = main_dir.root().join(".jj/working_copy/tree_state");
    std::fs::write(&marker_path, b"enabling\n")?;
    let checkout_before = std::fs::read(&checkout_path)?;
    let tree_state_before = std::fs::read(&tree_state_path)?;
    let operation_before = main_dir.current_operation_id();

    // The temp workspace has no AWACS manager. Do not use the synthetic scan
    // environment: the real manager-open check must reject it before reset.
    let output = main_dir.run_jj(["util", "subvolume", "enable", "--rebuild-baseline"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("failed to open existing AWACS root state"),
        "{output}"
    );
    assert_eq!(std::fs::read(checkout_path)?, checkout_before);
    assert_eq!(std::fs::read(tree_state_path)?, tree_state_before);
    assert_eq!(std::fs::read(marker_path)?, b"enabling\n");
    assert_eq!(main_dir.current_operation_id(), operation_before);
    assert_eq!(main_dir.read_file("tracked"), "unsnapshotted edit\n");
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
fn check_util_subvolume_enable_disable(source_has_nested_jj: bool, enable_cwd: &str) -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    main_dir.write_file(".gitignore", "/.fake-subvolume\n");
    main_dir.write_file("file", "tracked\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir
        .run_jj(["config", "set", "--repo", "fsmonitor.backend", "watchman"])
        .success();
    main_dir
        .run_jj([
            "config",
            "set",
            "--repo",
            "test-subvolume.metadata",
            "retained repository configuration",
        ])
        .success();
    // Exercise the fast idempotent path: an earlier attempt may already have
    // converted both boundaries before failing to publish the baseline.
    main_dir.write_file(".fake-subvolume", "");
    main_dir.write_file(".git/.fake-subvolume", "");
    if source_has_nested_jj {
        main_dir.write_file(".jj/.fake-subvolume", "");
    }
    main_dir.write_file(".jj/metadata-sentinel", "retained metadata\n");
    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    test -f "$3/.fake-subvolume"
    exit $?
fi
if [ "$1" = "subvolume" ] && [ "$2" = "create" ]; then
    mkdir "$3"
    touch "$3/.fake-subvolume"
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "snapshot" ]; then
    if [ "$3" = "-r" ]; then
        source="$4"
        destination="$5"
    else
        source="$3"
        destination="$4"
    fi
    mkdir "$destination"
    cp -a "$source/." "$destination/"
    # The synthetic adapter models the recursive snapshot as one operation.
    # Real Btrfs tests cover omitted descendants and read-only boundaries.
    touch "$destination/.fake-subvolume"
    if [ -n "$JJ_TEST_SUBVOLUME_READY" ]; then
        touch "$JJ_TEST_SUBVOLUME_READY"
        while [ ! -f "$JJ_TEST_SUBVOLUME_RELEASE" ]; do
            sleep 0.01
        done
    fi
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    case "$3" in
        *jj-subvolume-rollback-*)
            test -f "$3/.fake-writable" || {
                echo read-only snapshot >&2
                exit 1
            }
            ;;
    esac
    rm -rf "$3"
    exit 0
fi
if [ "$1" = "property" ] && [ "$2" = "set" ] && [ "$3" = "-ts" ]; then
    touch "$4/.fake-writable"
    exit 0
fi
exit 1
"#,
    );

    // Simulate an interrupted older enable which committed the strict marker
    // before publishing its first AWACS baseline. The ordinary command (with
    // working-copy snapshotting enabled) must enter recovery before generic
    // workspace loading enforces the baseline invariant.
    std::fs::write(
        main_dir.root().join(".jj/working_copy/subvolume_mode"),
        b"snapshot-backed\n",
    )?;
    let marker_path = main_dir.root().join(".jj/working_copy/subvolume_mode");
    let original_marker_permissions = std::fs::metadata(&marker_path)?.permissions();
    let mut readonly_marker_permissions = original_marker_permissions.clone();
    readonly_marker_permissions.set_readonly(true);
    std::fs::set_permissions(&marker_path, readonly_marker_permissions)?;
    let status = main_dir
        .run_jj_with(|cmd| cmd.env("PATH", &path).args(["util", "subvolume", "status"]))
        .success();
    std::fs::set_permissions(&marker_path, original_marker_permissions)?;
    assert!(status.stdout.raw().contains("Subvolume mode: enabled"));
    assert!(
        status
            .stdout
            .raw()
            .contains("Working copy snapshot: none (required committed AWACS baseline is missing)")
    );
    assert!(status.stdout.raw().contains(
        "Recovery: run `jj util subvolume enable` to build and activate a replacement checkout."
    ));
    assert!(
        status
            .stdout
            .raw()
            .contains("JJ journal: missing committed baseline")
    );
    assert!(status.stdout.raw().contains("Git index: version="));
    assert!(
        status
            .stdout
            .raw()
            .contains("Git fsmonitor: missing (no FSMN extension)")
    );
    assert!(status.stdout.raw().contains("Consistency: ERROR"));

    // Pause a real enable after it relaxes the source marker but before it
    // finishes its replacement checkout. A concurrent enable used to treat
    // this in-progress state as an interrupted migration and start another.
    let ready_path = test_env.env_root().join("subvolume-ready");
    let release_path = test_env.env_root().join("subvolume-release");
    struct ReleaseOperation<'a>(&'a std::path::Path);
    impl Drop for ReleaseOperation<'_> {
        fn drop(&mut self) {
            // Also unblock the child if an assertion below fails.
            let _result = std::fs::write(self.0, b"release\n");
        }
    }
    // --repository selects the checkout independently of the caller's cwd.
    // A cwd inside the replaced checkout must still be reanchored afterward.
    let enable_cwd = main_dir.root().join(enable_cwd);
    std::fs::create_dir_all(&enable_cwd)?;
    let mut first_command = test_env.new_jj_cmd();
    first_command
        .current_dir(&enable_cwd)
        .env("PATH", &path)
        .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
        .env("JJ_TEST_SUBVOLUME_READY", &ready_path)
        .env("JJ_TEST_SUBVOLUME_RELEASE", &release_path)
        .timeout(std::time::Duration::from_secs(30))
        .arg("--repository")
        .arg(main_dir.root())
        .args(["util", "subvolume", "enable"]);
    let output = std::thread::scope(|scope| {
        let first_enable = scope.spawn(move || first_command.output().unwrap());
        let release = ReleaseOperation(&release_path);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready_path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "enable did not reach barrier"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(std::fs::read(&marker_path).unwrap(), b"enabling\n");
        let checkout_path = main_dir.root().join(".jj/working_copy/checkout");
        let checkout_before = std::fs::read(&checkout_path).unwrap();
        for args in [
            vec!["util", "subvolume", "enable"],
            vec!["util", "subvolume", "enable", "--rebuild-baseline"],
            vec!["util", "subvolume", "disable"],
        ] {
            let concurrent = main_dir.run_jj_with(|cmd| {
                cmd.env("PATH", &path)
                    .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                    .timeout(std::time::Duration::from_secs(5))
                    .args(args)
            });
            assert!(!concurrent.status.success(), "{concurrent}");
            assert!(
                concurrent
                    .stderr
                    .raw()
                    .contains("Another subvolume operation is already running"),
                "{concurrent}"
            );
        }
        let status = main_dir
            .run_jj_with(|cmd| {
                cmd.timeout(std::time::Duration::from_secs(5))
                    .args(["util", "subvolume", "status"])
            })
            .success();
        assert!(
            status
                .stdout
                .raw()
                .contains("Subvolume operation: in progress (pid=")
        );
        assert!(
            status
                .stdout
                .raw()
                .contains("Consistency: unavailable (subvolume operation in progress)")
        );
        assert!(!status.stdout.raw().contains("Consistency: ERROR"));
        assert_eq!(std::fs::read(&marker_path).unwrap(), b"enabling\n");
        assert_eq!(std::fs::read(&checkout_path).unwrap(), checkout_before);
        drop(release);
        first_enable.join().unwrap()
    });
    let stderr = String::from_utf8(output.stderr)?;
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("Building snapshot-backed checkout at"));
    assert!(stderr.contains("Recursively snapshotting repository subvolumes to"));
    assert!(stderr.contains("awacs: creating initial snapshot baseline..."));
    assert!(
        stderr
            .find("awacs: test root initialized...")
            .is_some_and(|initialized| {
                stderr
                    .find("Seeding snapshot-backed working-copy tree...")
                    .is_some_and(|seed_tree| initialized < seed_tree)
            }),
        "AWACS root initialization must precede snapshot-backed tree seeding: {stderr}"
    );
    assert!(
        stderr
            .find("snapshotting the working copy and committing the AWACS baseline")
            .is_some_and(|baseline| {
                stderr
                    .find("importing the colocated Git HEAD")
                    .is_some_and(|import_head| baseline < import_head)
            }),
        "the first AWACS baseline must precede any Git HEAD materialization: {stderr}"
    );
    assert!(!stderr.contains("Failed to query filesystem monitor"));
    assert!(stderr.contains("Original checkout retained at"));
    assert!(
        stderr.contains(
            "If the snapshot-backed checkout looks good, delete the original checkout at"
        )
    );
    assert!(stderr.contains("Enter the snapshot-backed checkout with: cd"));
    assert!(main_dir.root().join(".fake-subvolume").is_file());
    assert!(main_dir.root().join(".git/.fake-subvolume").is_file());
    assert!(main_dir.root().join(".jj/.fake-subvolume").is_file());
    assert_eq!(
        std::fs::read_to_string(main_dir.root().join(".jj/metadata-sentinel"))?,
        "retained metadata\n"
    );
    let retained_sources = std::fs::read_dir(main_dir.root().parent().unwrap())?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with("main.jj-subvolume-source-")
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(retained_sources.len(), 1);
    assert!(retained_sources[0].join("file").is_file());
    assert!(
        main_dir
            .root()
            .join(".jj/working_copy/subvolume_mode")
            .is_file()
    );
    assert!(!main_dir.root().join(".jj/working_copy/tree_state").exists());
    let obsolete_fsmonitor_lock = main_dir
        .root()
        .join(".git/index.fsmonitor-transaction.lock");
    std::fs::write(&obsolete_fsmonitor_lock, b"")?;
    let status = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "status"])
        })
        .success();
    std::fs::remove_file(&obsolete_fsmonitor_lock)?;
    assert!(status.stdout.raw().contains("Subvolume mode: enabled"));
    assert!(
        status
            .stdout
            .raw()
            .contains("Repository root: Btrfs subvolume (ID 5)")
    );
    assert!(status.stdout.raw().contains(".git: Btrfs subvolume (ID 5)"));
    assert!(status.stdout.raw().contains(".jj: Btrfs subvolume (ID 5)"));
    assert!(status.stdout.raw().contains("Working copy snapshot: awacs"));
    assert!(status.stdout.raw().contains("(clean-baseline,"));
    assert!(status.stdout.raw().contains("JJ journal: clean-baseline"));
    assert!(
        status
            .stdout
            .raw()
            .contains("Git fsmonitor: missing (no FSMN extension)")
    );
    assert!(status.stdout.raw().contains("AWACS manager: unavailable"));
    assert!(status.stdout.raw().contains("Locks: none"));
    assert!(!status.stdout.raw().contains("Git fsmonitor transaction"));

    let git_index_lock = main_dir.root().join(".git/index.lock");
    std::fs::write(&git_index_lock, b"")?;
    let locked_status = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "status"])
        })
        .success();
    std::fs::remove_file(&git_index_lock)?;
    assert!(
        locked_status
            .stdout
            .raw()
            .contains("Locks: Git index (owner unknown)")
    );

    let index_path = main_dir.root().join(".git/index");
    let original_index = std::fs::read(&index_path)?;
    let mut placeholder_index = original_index[..original_index.len() - 20].to_vec();
    let mut placeholder = 2_u32.to_be_bytes().to_vec();
    placeholder.extend_from_slice(b"123456789\0");
    placeholder_index.extend_from_slice(b"FSMN");
    placeholder_index.extend_from_slice(&(placeholder.len() as u32).to_be_bytes());
    placeholder_index.extend_from_slice(&placeholder);
    let checksum = btrfs_awacs::status::git_index_checksum(&placeholder_index, 20)
        .map_err(std::io::Error::other)?;
    placeholder_index.extend_from_slice(&checksum);
    std::fs::write(&index_path, placeholder_index)?;
    let placeholder_status = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "status"])
        })
        .success();
    std::fs::write(&index_path, original_index)?;
    assert!(
        placeholder_status
            .stdout
            .raw()
            .contains("Git fsmonitor: invalid (timestamp placeholder)")
    );

    // Re-running enable against a healthy strict marker is a true no-op.
    let output = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "enable"])
        })
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("Btrfs subvolume mode is already enabled.")
    );

    // A committed baseline from the previous layout is not enough: enable
    // must still migrate ordinary .jj metadata while preserving repository
    // history, configuration, and the compact working-copy state.
    let committed_parent = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args([
                    "--ignore-working-copy",
                    "log",
                    "-r",
                    "@-",
                    "--no-graph",
                    "-T",
                    "commit_id",
                ])
        })
        .success()
        .stdout
        .raw()
        .to_owned();
    std::fs::remove_file(main_dir.root().join(".jj/.fake-subvolume"))?;
    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "enable"])
        })
        .success();
    assert!(main_dir.root().join(".jj/.fake-subvolume").is_file());
    assert_eq!(
        std::fs::read_to_string(main_dir.root().join(".jj/metadata-sentinel"))?,
        "retained metadata\n"
    );
    let migrated_parent = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args([
                    "--ignore-working-copy",
                    "log",
                    "-r",
                    "@-",
                    "--no-graph",
                    "-T",
                    "commit_id",
                ])
        })
        .success();
    assert_eq!(migrated_parent.stdout.raw(), committed_parent);
    let migrated_config = main_dir
        .run_jj(["config", "get", "test-subvolume.metadata"])
        .success();
    assert_eq!(
        migrated_config.stdout.raw().trim(),
        "retained repository configuration"
    );
    assert!(
        std::fs::read(main_dir.root().join(".jj/working_copy/checkout"))?
            .starts_with(b"\0JJ-WORKING-COPY-STATE\0v1\n")
    );

    // An explicit rebuild clears an unusable committed cursor and permits
    // the next immutable AWACS scan to establish a replacement baseline.
    let output = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "enable", "--rebuild-baseline"])
        })
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("awacs: rebuilding committed snapshot baseline...")
    );
    assert!(
        output
            .stderr
            .raw()
            .contains("awacs: rebuilt committed snapshot baseline.")
    );
    let status = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "status"])
        })
        .success();
    assert!(status.stdout.raw().contains("Working copy snapshot: awacs"));
    assert!(status.stdout.raw().contains("(clean-baseline,"));

    // A controlled checkout should immediately advance the retained baseline
    // instead of leaving final subvolume mode in a no-baseline state.
    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["new", "root()"])
        })
        .success();
    assert!(
        main_dir
            .root()
            .join(".jj/working_copy/subvolume_mode")
            .is_file()
    );
    assert!(
        std::fs::read(main_dir.root().join(".jj/working_copy/checkout"))
            .unwrap()
            .starts_with(b"\0JJ-WORKING-COPY-STATE\0v1\n")
    );
    let status = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "status"])
        })
        .success();
    assert!(status.stdout.raw().contains("Working copy snapshot: awacs"));
    assert!(status.stdout.raw().contains("(clean-baseline,"));
    main_dir.write_file("file", "tracked\n");

    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "disable"])
        })
        .success();
    assert!(main_dir.root().join("file").is_file());
    assert!(main_dir.root().join(".git").is_dir());
    assert!(main_dir.root().join(".jj/repo").is_dir());
    assert_eq!(
        std::fs::read_to_string(main_dir.root().join(".jj/metadata-sentinel"))?,
        "retained metadata\n"
    );
    let restored_config = main_dir
        .run_jj(["config", "get", "test-subvolume.metadata"])
        .success();
    assert_eq!(
        restored_config.stdout.raw().trim(),
        "retained repository configuration"
    );
    assert!(
        !main_dir
            .root()
            .join(".jj/working_copy/subvolume_mode")
            .exists()
    );
    let status = main_dir
        .run_jj_with(|cmd| cmd.env("PATH", &path).args(["util", "subvolume", "status"]))
        .success();
    assert!(status.stdout.raw().contains("Subvolume mode: disabled"));
    assert!(
        status
            .stdout
            .raw()
            .contains("Working copy snapshot: none (subvolume mode disabled)")
    );
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_enable_linked_requires_enabled_main() {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");
    main_dir
        .run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();

    let output = secondary_dir.run_jj(["util", "subvolume", "enable"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output.stderr.raw().contains(
            "Cannot enable subvolume mode in a linked workspace until the main workspace is +             snapshot-backed"
        ),
        "{output}"
    );
    assert!(
        output
            .stderr
            .raw()
            .contains("Run `jj util subvolume enable` from the main workspace first"),
        "{output}"
    );
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_enable_is_workspace_local() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");
    main_dir.write_file(".gitignore", "/.fake-subvolume\n");
    main_dir.write_file("file", "tracked\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir
        .run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();

    // Pretend that only the main root and its .git directory are already
    // Btrfs subvolumes. The linked workspace starts as an ordinary directory
    // with a Git .git file.
    main_dir.write_file(".fake-subvolume", "");
    main_dir.write_file(".git/.fake-subvolume", "");
    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    test -f "$3/.fake-subvolume"
    exit $?
fi
if [ "$1" = "subvolume" ] && [ "$2" = "create" ]; then
    mkdir "$3"
    touch "$3/.fake-subvolume"
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "snapshot" ]; then
    if [ "$3" = "-r" ]; then
        source="$4"
        destination="$5"
    else
        source="$3"
        destination="$4"
    fi
    mkdir "$destination"
    cp -a "$source/." "$destination/"
    touch "$destination/.fake-subvolume"
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    rm -rf "$3"
    exit 0
fi
exit 1
"#,
    );

    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "enable"])
        })
        .success();
    assert!(
        main_dir
            .root()
            .join(".jj/working_copy/subvolume_mode")
            .is_file()
    );
    assert!(main_dir.root().join(".jj/.fake-subvolume").is_file());
    assert!(!secondary_dir.root().join(".jj/.fake-subvolume").exists());
    assert!(
        !secondary_dir
            .root()
            .join(".jj/working_copy/subvolume_mode")
            .exists()
    );
    secondary_dir.write_file("file", "legacy workspace change\n");
    let legacy_status = secondary_dir
        .run_jj_with(|cmd| cmd.env("PATH", &path).args(["status"]))
        .success();
    assert!(legacy_status.stdout.raw().contains("M file"));

    secondary_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", secondary_dir.root())
                .args(["util", "subvolume", "enable"])
        })
        .success();
    assert!(
        secondary_dir
            .root()
            .join(".jj/working_copy/subvolume_mode")
            .is_file()
    );
    assert!(secondary_dir.root().join(".git").is_file());
    assert!(secondary_dir.root().join(".jj/.fake-subvolume").is_file());
    let migrated_status = secondary_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", secondary_dir.root())
                .args(["util", "subvolume", "status"])
        })
        .success();
    assert!(
        migrated_status
            .stdout
            .raw()
            .contains("Working copy snapshot: awacs")
    );
    assert!(
        migrated_status
            .stdout
            .raw()
            .contains(".git: linked worktree (administrative directory")
    );
    assert!(migrated_status.stdout.raw().contains("Git index: version="));
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_enable_compression_rewrites_extents() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    main_dir.write_file(".gitignore", "/.fake-subvolume\n/.compression-*\n");
    main_dir.write_file("file", "tracked\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    test -f "$3/.fake-subvolume"
    exit $?
fi
if [ "$1" = "subvolume" ] && [ "$2" = "create" ]; then
    mkdir "$3"
    touch "$3/.fake-subvolume"
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "snapshot" ]; then
    echo snapshot must not be used with --compress >&2
    exit 1
fi
if [ "$1" = "property" ] && [ "$2" = "set" ]; then
    test "$4" = "compression" || exit 1
    case "$5" in
        zstd|none) touch "$3/.compression-$5" ;;
        *) exit 1 ;;
    esac
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    rm -rf "$3"
    exit 0
fi
exit 1
"#,
    );

    let output = main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path)
                .env("JJ_TEST_AWACS_SCAN_ROOT", main_dir.root())
                .args(["util", "subvolume", "enable", "--compress=true"])
        })
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("awacs: preparing Btrfs subvolume conversion")
    );
    assert!(
        !output
            .stderr
            .raw()
            .contains("Recursively snapshotting repository")
    );
    assert!(main_dir.root().join(".compression-zstd").is_file());
    assert!(main_dir.root().join(".git/.compression-zstd").is_file());
    assert!(main_dir.root().join(".jj/.compression-zstd").is_file());
    assert!(main_dir.root().join(".jj/.fake-subvolume").is_file());
    assert_eq!(
        std::fs::read_to_string(main_dir.root().join("file"))?,
        "tracked\n"
    );
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_failed_snapshot_preserves_concurrent_destination() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    main_dir.write_file(".fake-subvolume", "");
    main_dir.write_file("file", "source contents\n");
    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    test -f "$3/.fake-subvolume"
    exit $?
fi
if [ "$1" = "subvolume" ] && [ "$2" = "snapshot" ]; then
    # Model a creator winning the destination race before atomic publication.
    mkdir "$4"
    printf '%s\n' 'another creator' > "$4/foreign"
    echo 'destination already exists' >&2
    exit 1
fi
exit 1
"#,
    );
    let output = main_dir.run_jj_with(|cmd| {
        cmd.env("PATH", &path)
            .env("JJ_TEST_BTRFS_CLI", "1")
            .args(["util", "subvolume", "enable"])
    });
    assert!(!output.status.success(), "{output}");
    assert!(output.stderr.raw().contains("destination already exists"));
    let occupied = std::fs::read_dir(main_dir.root().parent().unwrap())?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("jj-subvolume-init-")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        occupied.len(),
        1,
        "failed snapshot must preserve its competitor"
    );
    assert_eq!(
        std::fs::read(occupied[0].path().join("foreign"))?,
        b"another creator\n"
    );
    assert_eq!(
        std::fs::read(main_dir.root().join("file"))?,
        b"source contents\n"
    );
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_enable_rejects_linked_awacs_before_migration() -> TestResult {
    use std::os::unix::fs::MetadataExt as _;

    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    main_dir.write_file("file", "tracked\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir.write_file(
        ".git/worktrees/linked/awacs/manager.sqlite3",
        "linked-worktree state must remain untouched\n",
    );
    let root_inode = std::fs::metadata(main_dir.root())?.ino();
    let paths = [
        ".jj/working_copy/checkout",
        ".git/index",
        ".git/worktrees/linked/awacs/manager.sqlite3",
        "file",
    ];
    let before = paths
        .iter()
        .map(|path| std::fs::read(main_dir.root().join(path)))
        .collect::<Result<Vec<_>, _>>()?;
    let output = main_dir.run_jj(["util", "subvolume", "enable"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("Cannot migrate the main workspace while linked Git worktree AWACS state"),
        "{output}"
    );
    assert_eq!(std::fs::metadata(main_dir.root())?.ino(), root_inode);
    for (path, bytes) in paths.iter().zip(before) {
        assert_eq!(std::fs::read(main_dir.root().join(path))?, bytes);
    }
    assert!(
        std::fs::read_dir(main_dir.root().parent().unwrap())?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .contains("jj-subvolume-init-")),
        "rejected migration must not allocate a staging checkout"
    );
    Ok(())
}

#[cfg(unix)]
#[cfg(feature = "awacs")]
#[test]
fn test_util_subvolume_enable_keeps_source_and_staging_on_failure() {
    use std::os::unix::fs::MetadataExt as _;

    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    main_dir.write_file("file", "tracked\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    test -f "$3/.fake-subvolume"
    exit $?
fi
if [ "$1" = "subvolume" ] && [ "$2" = "create" ]; then
    mkdir "$3"
    touch "$3/.fake-subvolume"
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    rm -rf "$3"
    exit 0
fi
exit 1
"#,
    );

    let root_inode = std::fs::metadata(main_dir.root()).unwrap().ino();
    let output =
        main_dir.run_jj_with(|cmd| cmd.env("PATH", &path).args(["util", "subvolume", "enable"]));
    assert!(!output.status.success(), "{output}");
    assert_eq!(
        std::fs::metadata(main_dir.root()).unwrap().ino(),
        root_inode
    );
    assert!(main_dir.root().join("file").is_file());
    assert!(main_dir.root().join(".git/HEAD").is_file());
    assert!(main_dir.root().join(".git/objects").is_dir());
    let staged = std::fs::read_dir(main_dir.root().parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with(".main.jj-subvolume-init-")
            })
        })
        .collect::<Vec<_>>();
    assert!(staged.is_empty());

    let output = main_dir.run_jj_with(|cmd| {
        cmd.env("PATH", &path)
            .args(["util", "subvolume", "enable", "--keep"])
    });
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("Partial migration checkout retained at")
    );
    let staged = std::fs::read_dir(main_dir.root().parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with(".main.jj-subvolume-init-")
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(staged.len(), 1);
    assert!(staged[0].join("file").is_file());
    assert!(staged[0].join(".git/HEAD").is_file());
    assert!(
        !main_dir
            .root()
            .join(".jj/working_copy/subvolume_mode")
            .exists()
    );
}

#[cfg(all(unix, not(feature = "awacs")))]
#[test]
fn test_util_subvolume_is_unavailable_without_awacs() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let output = main_dir.run_jj(["util", "subvolume", "enable"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("unrecognized subcommand 'subvolume'")
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_workspace_remove_with_unrecorded_current_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    // Simulate a repository created before workspace paths were recorded.
    // Adding a later workspace creates a store row only for that workspace.
    main_dir.remove_dir_all(".jj/repo/workspace_store");
    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    main_dir
        .run_jj(["workspace", "remove", "secondary"])
        .success();
    assert!(!test_env.env_root().join("secondary").exists());
}

#[test]
fn test_workspace_remove_ignores_missing_unrelated_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let stale_dir = test_env.work_dir("stale");
    let target_dir = test_env.work_dir("target");

    main_dir.run_jj(["workspace", "add", "../stale"]).success();
    main_dir.run_jj(["workspace", "add", "../target"]).success();
    std::fs::remove_dir_all(stale_dir.root()).unwrap();

    main_dir.run_jj(["workspace", "remove", "target"]).success();
    assert!(!target_dir.root().exists());
}

#[test]
fn test_workspace_remove_ignores_unloadable_unrelated_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let stale_dir = test_env.work_dir("stale");
    let target_dir = test_env.work_dir("target");

    main_dir.run_jj(["workspace", "add", "../stale"]).success();
    main_dir.run_jj(["workspace", "add", "../target"]).success();
    std::fs::remove_dir_all(stale_dir.root().join(".jj")).unwrap();

    main_dir.run_jj(["workspace", "remove", "target"]).success();
    assert!(!target_dir.root().exists());
}

#[test]
fn test_workspace_remove_force_skips_snapshot_and_target_checks() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let stale_dir = test_env.work_dir("stale");
    let target_dir = test_env.work_dir("target");

    main_dir.run_jj(["workspace", "add", "../stale"]).success();
    main_dir.run_jj(["workspace", "add", "../target"]).success();
    std::fs::remove_dir_all(stale_dir.root()).unwrap();
    target_dir.write_file("dirty", "uncommitted target contents");
    main_dir.write_file("untracked", "must not be snapshotted");

    main_dir
        .run_jj(["workspace", "remove", "--force", "target"])
        .success();
    assert!(!target_dir.root().exists());
    let output = main_dir
        .run_jj(["--ignore-working-copy", "file", "list", "untracked"])
        .success();
    assert!(!output.stdout.raw().contains("untracked"));
}

#[test]
fn test_workspace_remove_force_forgets_missing_target() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let target_dir = test_env.work_dir("target");

    main_dir.run_jj(["workspace", "add", "../target"]).success();
    std::fs::remove_dir_all(target_dir.root()).unwrap();

    main_dir
        .run_jj(["workspace", "remove", "--force", "target"])
        .success();
    let output = main_dir.run_jj(["workspace", "list"]).success();
    assert!(!output.stdout.raw().contains("target:"));
}

#[cfg(unix)]
#[test]
fn test_workspaces_remove_btrfs_subvolume() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");
    let tertiary_dir = test_env.work_dir("tertiary");

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    main_dir
        .run_jj(["workspace", "add", "../tertiary"])
        .success();
    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    case "$3" in
        */secondary)
            echo 5
            exit 0
            ;;
        *) exit 1 ;;
    esac
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    case "$3" in
        */secondary) exit 0 ;;
        *) exit 1 ;;
    esac
fi
if [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    case "$3" in
        */.jj-removing-*)
            touch "$(dirname "$3")/btrfs-deleted"
            rm -rf "$3"
            exit 0
            ;;
    esac
    exit 1
fi
exit 1
"#,
    );

    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path).env("JJ_TEST_BTRFS_CLI", "1").args([
                "workspace",
                "remove",
                "secondary",
            ])
        })
        .success();
    assert!(!secondary_dir.root().exists());
    assert!(test_env.env_root().join("btrfs-deleted").exists());

    main_dir
        .run_jj_with(|cmd| {
            cmd.env("PATH", &path).env("JJ_TEST_BTRFS_CLI", "1").args([
                "workspace",
                "remove",
                "tertiary",
            ])
        })
        .success();
    assert!(!tertiary_dir.root().exists());
    assert!(!tertiary_dir.root().with_extension("btrfs-deleted").exists());

    let output = main_dir.run_jj(["workspace", "remove", "default"]);
    assert!(
        output
            .stderr
            .raw()
            .contains("Cannot remove the current workspace")
    );
}

#[cfg(unix)]
#[test]
fn test_workspaces_remove_btrfs_subvolume_permission_failure_preserves_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    let operation_id = main_dir.current_operation_id();
    let path = install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    echo 'Operation not permitted' >&2
    exit 1
fi
exit 1
"#,
    );

    let output = main_dir.run_jj_with(|cmd| {
        cmd.env("PATH", &path).env("JJ_TEST_BTRFS_CLI", "1").args([
            "workspace",
            "remove",
            "secondary",
        ])
    });
    assert!(!output.status.success(), "{output}");
    assert!(
        output.stderr.raw().contains("Operation not permitted"),
        "{output}"
    );
    assert!(
        output.stderr.raw().contains("user_subvol_rm_allowed"),
        "{output}"
    );
    assert!(
        output.stderr.raw().contains("sudo btrfs subvolume delete"),
        "{output}"
    );
    assert!(output.stderr.raw().contains("workspace was not forgotten"));
    assert!(secondary_dir.root().exists());
    assert_eq!(main_dir.current_operation_id(), operation_id);
    assert!(
        std::fs::read_dir(test_env.env_root())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".jj-removing-")),
        "failed deletion must restore the registered target name"
    );

    let output = main_dir.run_jj(["workspace", "list"]).success();
    assert!(output.stdout.raw().contains("secondary"), "{output}");
}

#[test]
fn test_workspace_remove_rejects_dirty_target() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "clean\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    secondary_dir.write_file("file", "dirty\n");
    let operation_id = main_dir.current_operation_id();

    let output = main_dir.run_jj(["workspace", "remove", "secondary"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output.stderr.raw().contains("uncommitted changes"),
        "{output}"
    );
    assert_eq!(main_dir.current_operation_id(), operation_id);
    assert_eq!(
        std::fs::read_to_string(secondary_dir.root().join("file")).unwrap(),
        "dirty\n"
    );
    let output = main_dir.run_jj(["workspace", "list"]).success();
    assert!(output.stdout.raw().contains("secondary"), "{output}");
}

#[test]
fn test_workspace_remove_cleans_colocated_git_worktree_admin() {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "contents\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir
        .run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let admin_dir = gix::open(secondary_dir.root())
        .unwrap()
        .git_dir()
        .to_owned();

    main_dir
        .run_jj(["workspace", "remove", "secondary"])
        .success();
    assert!(!secondary_dir.root().exists());
    assert!(
        !admin_dir.exists(),
        "Git worktree admin must actually be pruned"
    );
    let worktree_output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(main_dir.root())
        .output()
        .unwrap();
    assert!(
        worktree_output.status.success(),
        "git worktree list failed: {}",
        String::from_utf8_lossy(&worktree_output.stderr)
    );
    let stdout = String::from_utf8_lossy(&worktree_output.stdout);
    assert!(
        !stdout.contains(secondary_dir.root().to_str().unwrap()),
        "worktree list should not mention removed workspace: {stdout}"
    );
}

#[test]
fn test_workspace_remove_retires_only_its_awacs_state() {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    main_dir.write_file("file", "contents\n");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    for name in ["secondary", "survivor"] {
        main_dir
            .run_jj(["workspace", "add", "--colocate", &format!("../{name}")])
            .success();
    }
    let secondary_dir = test_env.work_dir("secondary");
    let survivor_dir = test_env.work_dir("survivor");
    let secondary_admin = gix::open(secondary_dir.root())
        .unwrap()
        .git_dir()
        .to_owned();
    let survivor_admin = gix::open(survivor_dir.root()).unwrap().git_dir().to_owned();
    let secondary_state = secondary_admin.join("awacs");
    let survivor_state = survivor_admin.join("awacs");
    for state in [&secondary_state, &survivor_state] {
        std::fs::create_dir(state).unwrap();
        std::fs::write(state.join("manager-marker"), b"retained state").unwrap();
    }
    let secondary_identity =
        jj_lib::file_util::FileIdentity::from_symlink_path(&secondary_state).unwrap();
    let survivor_identity =
        jj_lib::file_util::FileIdentity::from_symlink_path(&survivor_state).unwrap();
    let output = main_dir
        .run_jj(["workspace", "remove", "secondary"])
        .success();
    assert!(!output.stderr.raw().contains("failed to prune"), "{output}");
    assert!(!secondary_admin.exists());
    let retired = std::fs::read_dir(main_dir.root().join(".git/awacs"))
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".retired-worktree-")
        })
        .collect::<Vec<_>>();
    assert_eq!(retired.len(), 1, "only the removed workspace is retired");
    assert_eq!(
        jj_lib::file_util::FileIdentity::from_symlink_path(retired[0].path()).unwrap(),
        secondary_identity,
        "AWACS state must be renamed without copying"
    );
    assert_eq!(
        std::fs::read(retired[0].path().join("manager-marker")).unwrap(),
        b"retained state"
    );
    assert_eq!(
        jj_lib::file_util::FileIdentity::from_symlink_path(&survivor_state).unwrap(),
        survivor_identity,
        "a surviving workspace's AWACS state must not move"
    );
    assert_eq!(
        std::fs::read(survivor_state.join("manager-marker")).unwrap(),
        b"retained state"
    );
    survivor_dir.run_jj(["status"]).success();
}

#[test]
fn test_workspaces_status_in_linked_git_worktree_does_not_auto_import_git_refs() {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    let git_repo = git::open(main_dir.root());
    let commit_id = main_dir
        .run_jj([
            "--ignore-working-copy",
            "log",
            "-Tcommit_id",
            "--no-graph",
            "-r@",
        ])
        .success()
        .stdout
        .into_raw();
    let commit_id = gix::ObjectId::from_hex(commit_id.as_bytes()).unwrap();
    git_repo
        .reference(
            "refs/heads/external",
            commit_id,
            gix::refs::transaction::PreviousValue::Any,
            "",
        )
        .unwrap();

    secondary_dir.run_jj(["status"]).success();
    let output = secondary_dir.run_jj(["--ignore-working-copy", "log", "-rexternal@git"]);
    insta::assert_snapshot!(output, @r"
    ------- stderr -------
    Error: Revision `external@git` doesn't exist
    [EOF]
    [exit status: 1]
    ");
}

/// Test how sparse patterns are inherited
#[test]
fn test_workspaces_sparse_patterns() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "ws1"]).success();
    let ws1_dir = test_env.work_dir("ws1");
    let ws2_dir = test_env.work_dir("ws2");
    let ws3_dir = test_env.work_dir("ws3");
    let ws4_dir = test_env.work_dir("ws4");
    let ws5_dir = test_env.work_dir("ws5");
    let ws6_dir = test_env.work_dir("ws6");

    ws1_dir
        .run_jj(["sparse", "set", "--clear", "--add=foo"])
        .success();
    ws1_dir.run_jj(["workspace", "add", "../ws2"]).success();
    let output = ws2_dir.run_jj(["sparse", "list"]);
    insta::assert_snapshot!(output, @"
    foo
    [EOF]
    ");
    ws2_dir.run_jj(["sparse", "set", "--add=bar"]).success();
    ws2_dir.run_jj(["workspace", "add", "../ws3"]).success();
    let output = ws3_dir.run_jj(["sparse", "list"]);
    insta::assert_snapshot!(output, @"
    bar
    foo
    [EOF]
    ");
    // --sparse-patterns behavior
    ws3_dir
        .run_jj(["workspace", "add", "--sparse-patterns=copy", "../ws4"])
        .success();
    let output = ws4_dir.run_jj(["sparse", "list"]);
    insta::assert_snapshot!(output, @"
    bar
    foo
    [EOF]
    ");
    ws3_dir
        .run_jj(["workspace", "add", "--sparse-patterns=full", "../ws5"])
        .success();
    let output = ws5_dir.run_jj(["sparse", "list"]);
    insta::assert_snapshot!(output, @"
    .
    [EOF]
    ");
    ws3_dir
        .run_jj(["workspace", "add", "--sparse-patterns=empty", "../ws6"])
        .success();
    let output = ws6_dir.run_jj(["sparse", "list"]);
    insta::assert_snapshot!(output, @"");
}

/// Test adding a second workspace while the current workspace is editing a
/// merge
#[test]
fn test_workspaces_add_second_workspace_on_merge() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.run_jj(["describe", "-m=left"]).success();
    main_dir.run_jj(["new", "@-", "-m=right"]).success();
    main_dir.run_jj(["new", "@-+", "-m=merge"]).success();

    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output, @"
    default: . zsuskuln 46ed31b6 (empty) merge
    [EOF]
    ");

    main_dir
        .run_jj(["workspace", "add", "--name", "second", "../secondary"])
        .success();

    // The new workspace's working-copy commit shares all parents with the old one.
    insta::assert_snapshot!(get_log_output(&main_dir), @r#"
    @    46ed31b61ce9 default@ "merge"
    ├─╮
    │ │ ○  d23b2d4ff55c second@
    ╭─┬─╯
    │ ○  3c52528f5893 "left"
    ○ │  a3155ab1bf5a "right"
    ├─╯
    ◆  000000000000
    [EOF]
    "#);
}

/// Test that --ignore-working-copy is respected
#[test]
fn test_workspaces_add_ignore_working_copy() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    // TODO: maybe better to error out early?
    let output = main_dir.run_jj(["workspace", "add", "--ignore-working-copy", "../secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../secondary"
    [EOF]
    "#);
}

/// Test that --no-integrate-operation is respected
#[test]
fn test_workspaces_add_no_integrate_operation() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    // TODO: maybe better to error out early?
    let output = main_dir.run_jj([
        "workspace",
        "add",
        "--no-integrate-operation",
        "../secondary",
    ]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../secondary"
    Operation left uncommitted because --no-integrate-operation was requested: 3fbea411d2b4
    [EOF]
    "#);
}

/// Test that --at-op is respected
#[test]
fn test_workspaces_add_at_operation() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file1", "");
    let output = main_dir.run_jj(["commit", "-m1"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: rlvkpnrz 59e07459 (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 9e4b0b91 1
    [EOF]
    ");

    main_dir.write_file("file2", "");
    let output = main_dir.run_jj(["commit", "-m2"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: kkmpptxz 6e9610ac (empty) (no description set)
    Parent commit (@-)      : rlvkpnrz 8b7259b9 2
    [EOF]
    ");

    // --at-op should disable snapshot in the main workspace, but the newly
    // created workspace should still be writable.
    main_dir.write_file("file3", "");
    let output = main_dir.run_jj(["workspace", "add", "--at-op=@-", "../secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../secondary"
    Working copy  (@) now at: rzvqmyuk b8772476 (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 9e4b0b91 1
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    let secondary_dir = test_env.work_dir("secondary");

    // New snapshot can be taken in the secondary workspace.
    secondary_dir.write_file("file4", "");
    let output = secondary_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    Working copy changes:
    A file4
    Working copy  (@) : rzvqmyuk f2ff8257 (no description set)
    Parent commit (@-): qpvuntsm 9e4b0b91 1
    [EOF]
    ------- stderr -------
    Concurrent modification detected, resolving automatically.
    [EOF]
    ");

    let output = secondary_dir.run_jj(["op", "log", "-Tdescription"]);
    insta::assert_snapshot!(output, @"
    @  snapshot working copy
    ○    reconcile divergent operations
    ├─╮
    ○ │  commit 9152e822279787a168ddf4cede6440a21faa00d7
    │ ○  create initial working-copy commit in workspace secondary
    │ ○  add workspace 'secondary'
    ├─╯
    ○  snapshot working copy
    ○  commit 093c3c9624b6cfe22b310586f5638792aa80e6d7
    ○  snapshot working copy
    ○  add workspace 'default'
    ○
    [EOF]
    ");
}

/// Test adding a workspace, but at a specific revision using '-r'
#[test]
fn test_workspaces_add_workspace_at_revision() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file-1", "contents");
    main_dir.run_jj(["commit", "-m", "first"]).success();

    main_dir.write_file("file-2", "contents");
    main_dir.run_jj(["commit", "-m", "second"]).success();

    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output, @"
    default: . kkmpptxz 5ac9178d (empty) (no description set)
    [EOF]
    ");

    let output = main_dir.run_jj([
        "workspace",
        "add",
        "--name",
        "second",
        "../secondary",
        "-r",
        "@--",
    ]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../secondary"
    Working copy  (@) now at: zxsnswpr ea5860fb (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 27473635 first
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    // Can see the working-copy commit in each workspace in the log output. The "@"
    // node in the graph indicates the current workspace's working-copy commit.
    insta::assert_snapshot!(get_log_output(&main_dir), @r#"
    @  5ac9178da8b2 default@
    ○  a47d8a593529 "second"
    │ ○  ea5860fbd622 second@
    ├─╯
    ○  27473635a942 "first"
    ◆  000000000000
    [EOF]
    "#);
    insta::assert_snapshot!(get_log_output(&secondary_dir), @r#"
    @  ea5860fbd622 second@
    │ ○  5ac9178da8b2 default@
    │ ○  a47d8a593529 "second"
    ├─╯
    ○  27473635a942 "first"
    ◆  000000000000
    [EOF]
    "#);
}

/// Test multiple `-r` flags to `workspace add` to create a workspace
/// working-copy commit with multiple parents.
#[test]
fn test_workspaces_add_workspace_multiple_revisions() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file-1", "contents");
    main_dir.run_jj(["commit", "-m", "first"]).success();
    main_dir.run_jj(["new", "-r", "root()"]).success();

    main_dir.write_file("file-2", "contents");
    main_dir.run_jj(["commit", "-m", "second"]).success();
    main_dir.run_jj(["new", "-r", "root()"]).success();

    main_dir.write_file("file-3", "contents");
    main_dir.run_jj(["commit", "-m", "third"]).success();
    main_dir.run_jj(["new", "-r", "root()"]).success();

    insta::assert_snapshot!(get_log_output(&main_dir), @r#"
    @  8d23abddc924
    │ ○  eba7f49e2358 "third"
    ├─╯
    │ ○  62444a45efcf "second"
    ├─╯
    │ ○  27473635a942 "first"
    ├─╯
    ◆  000000000000
    [EOF]
    "#);

    let output = main_dir.run_jj([
        "workspace",
        "add",
        "--name=merge",
        "../merged",
        "-r=subject(third)",
        "-r=subject(second)",
        "-r=subject(first)",
    ]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../merged"
    Working copy  (@) now at: wmwvqwsz 2d7c9a2d (empty) (no description set)
    Parent commit (@-)      : mzvwutvl eba7f49e third
    Parent commit (@-)      : kkmpptxz 62444a45 second
    Parent commit (@-)      : qpvuntsm 27473635 first
    Added 3 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    insta::assert_snapshot!(get_log_output(&main_dir), @r#"
    @  8d23abddc924 default@
    │ ○      2d7c9a2d41dc merge@
    │ ├─┬─╮
    │ │ │ ○  27473635a942 "first"
    ├─────╯
    │ │ ○  62444a45efcf "second"
    ├───╯
    │ ○  eba7f49e2358 "third"
    ├─╯
    ◆  000000000000
    [EOF]
    "#);
}

#[test]
fn test_workspaces_add_workspace_from_subdir() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    let subdir_dir = main_dir.create_dir("subdir");
    subdir_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . rlvkpnrz 0ba0ff35 (empty) (no description set)
    [EOF]
    ");

    // Create workspace while in sub-directory of current workspace
    let output = subdir_dir.run_jj(["workspace", "add", "../../secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../../secondary"
    Working copy  (@) now at: rzvqmyuk dea1be10 (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 80b67806 initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    // Both workspaces show up when we list them
    let output = secondary_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: ../main rlvkpnrz 0ba0ff35 (empty) (no description set)
    secondary: . rzvqmyuk dea1be10 (empty) (no description set)
    [EOF]
    ");
}

#[test]
fn test_workspaces_add_workspace_in_current_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    // Try to create workspace using name instead of path
    let output = main_dir.run_jj(["workspace", "add", "secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "secondary"
    Warning: Workspace created inside current directory. If this was unintentional, delete the "secondary" directory and run `jj workspace forget secondary` to remove it.
    Working copy  (@) now at: pmmvwywv 058f604d (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    // Workspace created despite warning
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output, @"
    default: . rlvkpnrz 504e3d8c (empty) (no description set)
    secondary: secondary pmmvwywv 058f604d (empty) (no description set)
    [EOF]
    ");

    // Use explicit path instead (no warning)
    let output = main_dir.run_jj(["workspace", "add", "./third"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "third"
    Working copy  (@) now at: zxsnswpr 1c1effec (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    // Both workspaces created
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output, @"
    default: . rlvkpnrz 504e3d8c (empty) (no description set)
    secondary: secondary pmmvwywv 058f604d (empty) (no description set)
    third: third zxsnswpr 1c1effec (empty) (no description set)
    [EOF]
    ");

    let output = main_dir.run_jj(["file", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    file
    [EOF]
    ");
}

#[test]
fn test_workspace_remove_rejects_shared_primary_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir
        .run_jj(["workspace", "add", "--name", "second", "../secondary"])
        .success();
    let operation_id = main_dir.current_operation_id();

    let output = secondary_dir.run_jj(["workspace", "remove", "default"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .to_string()
            .contains("contains the shared Jujutsu repository"),
        "{output}"
    );
    assert_eq!(main_dir.current_operation_id(), operation_id);
    assert!(main_dir.root().join(".jj/repo").is_dir());

    let output = secondary_dir.run_jj(["workspace", "list"]).success();
    let listing = output.stdout.to_string();
    assert!(listing.contains("default:"), "{output}");
    assert!(listing.contains("second:"), "{output}");
}

#[test]
fn test_workspace_remove_rejects_nested_surviving_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let nested_dir = test_env.work_dir("main/nested");

    main_dir
        .run_jj(["workspace", "add", "--name", "nested", "nested"])
        .success();
    let operation_id = main_dir.current_operation_id();

    let output = main_dir.run_jj(["workspace", "remove", "nested"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .to_string()
            .contains("overlaps surviving workspace default"),
        "{output}"
    );
    assert_eq!(main_dir.current_operation_id(), operation_id);
    assert!(nested_dir.root().is_dir());

    let output = main_dir.run_jj(["workspace", "list"]).success();
    let listing = output.stdout.to_string();
    assert!(listing.contains("default:"), "{output}");
    assert!(listing.contains("nested:"), "{output}");
}

#[cfg(unix)]
#[test]
fn test_workspace_remove_rejects_replaced_symlink() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let second_dir = test_env.work_dir("second");

    main_dir
        .run_jj(["workspace", "add", "--name", "second", "../second"])
        .success();
    let moved_path = test_env.env_root().join("second-real");
    std::fs::rename(second_dir.root(), &moved_path).unwrap();
    let unrelated_path = test_env.env_root().join("unrelated");
    std::fs::create_dir(&unrelated_path).unwrap();
    std::os::unix::fs::symlink(&unrelated_path, second_dir.root()).unwrap();
    let operation_id = main_dir.current_operation_id();

    let output = main_dir.run_jj(["workspace", "remove", "second"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .to_string()
            .contains("registered path contains a symlink"),
        "{output}"
    );
    assert_eq!(main_dir.current_operation_id(), operation_id);
    assert!(moved_path.is_dir());
    assert!(unrelated_path.is_dir());

    let output = main_dir.run_jj(["workspace", "list"]).success();
    let listing = output.stdout.to_string();
    assert!(listing.contains("default:"), "{output}");
    assert!(listing.contains("second:"), "{output}");
}

#[test]
fn test_workspace_add_override_path_in_store() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    let output = main_dir.run_jj(["workspace", "add", "--name", "second", "../secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../secondary"
    Working copy  (@) now at: pmmvwywv 058f604d (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    // Both workspaces show up when we list them
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . rlvkpnrz 504e3d8c (empty) (no description set)
    second: ../secondary pmmvwywv 058f604d (empty) (no description set)
    [EOF]
    ");

    // Undoing workspace addition
    let output = main_dir.run_jj(["operation", "restore", "@--"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Restored to operation: 7d5981d12f2a (2001-02-03 08:05:08) commit 006bd1130b84e90ab082adeabd7409270d5a86da
    [EOF]
    ");

    // Only default workspace show up when we list them
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . rlvkpnrz 504e3d8c (empty) (no description set)
    [EOF]
    ");

    // Re-creating the same workspace with different path
    let output = main_dir.run_jj(["workspace", "add", "--name", "second", "../tertiary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../tertiary"
    Working copy  (@) now at: spxsnpux 96ef6c50 (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    // Both workspaces show up when we list them
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . rlvkpnrz 504e3d8c (empty) (no description set)
    second: ../tertiary spxsnpux 96ef6c50 (empty) (no description set)
    [EOF]
    ");

    // The 'second' workspace points to the new path
    let output = main_dir.run_jj(["workspace", "root", "--name", "second"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/tertiary
    [EOF]
    ");
}

/// Test making changes to the working copy in a workspace as it gets rewritten
/// from another workspace
#[test]
fn test_workspaces_conflicting_edits() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "contents\n");
    main_dir.run_jj(["new"]).success();

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    insta::assert_snapshot!(get_log_output(&main_dir), @"
    @  393250c59e39 default@
    │ ○  547036666102 secondary@
    ├─╯
    ○  9a462e35578a
    ◆  000000000000
    [EOF]
    ");

    // Make changes in both working copies
    main_dir.write_file("file", "changed in main\n");
    secondary_dir.write_file("file", "changed in second\n");
    // Squash the changes from the main workspace into the initial commit (before
    // running any command in the secondary workspace
    let output = main_dir.run_jj(["squash"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Rebased 1 descendant commits.
    Working copy  (@) now at: mzvwutvl 3a9b690d (empty) (no description set)
    Parent commit (@-)      : qpvuntsm b853f7c8 (no description set)
    [EOF]
    ");

    // The secondary workspace's working-copy commit was updated
    insta::assert_snapshot!(get_log_output(&main_dir), @"
    @  3a9b690d6e67 default@
    │ ○  90f3d42e0bff secondary@
    ├─╯
    ○  b853f7c8b006
    ◆  000000000000
    [EOF]
    ");
    let output = secondary_dir.run_jj(["st"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: The working copy is stale (not updated since operation 149761aea7d1).
    Hint: Run `jj workspace update-stale` to update it.
    See https://docs.jj-vcs.dev/latest/working-copy/#stale-working-copy for more information.
    [EOF]
    [exit status: 1]
    ");
    // Same error on second run, and from another command
    let output = secondary_dir.run_jj(["log"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: The working copy is stale (not updated since operation 149761aea7d1).
    Hint: Run `jj workspace update-stale` to update it.
    See https://docs.jj-vcs.dev/latest/working-copy/#stale-working-copy for more information.
    [EOF]
    [exit status: 1]
    ");
    // It was detected that the working copy is now stale.
    // Since there was an uncommitted change in the working copy, it should
    // have been committed first (causing divergence)
    let output = secondary_dir.run_jj(["workspace", "update-stale"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Concurrent modification detected, resolving automatically.
    Rebased 1 descendant commits onto commits rewritten by other operation.
    Working copy  (@) now at: pmmvwywv/2 90f3d42e (divergent) (empty) (no description set)
    Parent commit (@-)      : qpvuntsm b853f7c8 (no description set)
    Added 0 files, modified 1 files, removed 0 files
    Updated working copy to fresh commit 90f3d42e0bff
    [EOF]
    ");
    insta::assert_snapshot!(get_log_output(&secondary_dir),
    @"
    @  90f3d42e0bff secondary@ (divergent)
    │ ×  8823f4273170 (divergent)
    ├─╯
    │ ○  3a9b690d6e67 default@
    ├─╯
    ○  b853f7c8b006
    ◆  000000000000
    [EOF]
    ");
    // The stale working copy should have been resolved by the previous command
    insta::assert_snapshot!(get_log_output(&secondary_dir), @"
    @  90f3d42e0bff secondary@ (divergent)
    │ ×  8823f4273170 (divergent)
    ├─╯
    │ ○  3a9b690d6e67 default@
    ├─╯
    ○  b853f7c8b006
    ◆  000000000000
    [EOF]
    ");
}

/// Test a clean working copy that gets rewritten from another workspace
#[test]
fn test_workspaces_updated_by_other() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "contents\n");
    main_dir.run_jj(["new"]).success();

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    insta::assert_snapshot!(get_log_output(&main_dir), @"
    @  393250c59e39 default@
    │ ○  547036666102 secondary@
    ├─╯
    ○  9a462e35578a
    ◆  000000000000
    [EOF]
    ");

    // Rewrite the check-out commit in one workspace.
    main_dir.write_file("file", "changed in main\n");
    let output = main_dir.run_jj(["squash"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Rebased 1 descendant commits.
    Working copy  (@) now at: mzvwutvl 3a9b690d (empty) (no description set)
    Parent commit (@-)      : qpvuntsm b853f7c8 (no description set)
    [EOF]
    ");

    // The secondary workspace's working-copy commit was updated.
    insta::assert_snapshot!(get_log_output(&main_dir), @"
    @  3a9b690d6e67 default@
    │ ○  90f3d42e0bff secondary@
    ├─╯
    ○  b853f7c8b006
    ◆  000000000000
    [EOF]
    ");
    let output = secondary_dir.run_jj(["st"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: The working copy is stale (not updated since operation 149761aea7d1).
    Hint: Run `jj workspace update-stale` to update it.
    See https://docs.jj-vcs.dev/latest/working-copy/#stale-working-copy for more information.
    [EOF]
    [exit status: 1]
    ");
    // It was detected that the working copy is now stale, but clean. So no
    // divergent commit should be created.
    let output = secondary_dir.run_jj(["workspace", "update-stale"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: pmmvwywv 90f3d42e (empty) (no description set)
    Parent commit (@-)      : qpvuntsm b853f7c8 (no description set)
    Added 0 files, modified 1 files, removed 0 files
    Updated working copy to fresh commit 90f3d42e0bff
    [EOF]
    ");
    insta::assert_snapshot!(get_log_output(&secondary_dir),
    @"
    @  90f3d42e0bff secondary@
    │ ○  3a9b690d6e67 default@
    ├─╯
    ○  b853f7c8b006
    ◆  000000000000
    [EOF]
    ");
}

/// Test a clean working copy that gets rewritten from another workspace
#[test]
fn test_workspaces_updated_by_other_automatic() {
    let test_env = TestEnvironment::default();
    test_env.add_config("snapshot.auto-update-stale = true\n");

    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "contents\n");
    main_dir.run_jj(["new"]).success();

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    insta::assert_snapshot!(get_log_output(&main_dir), @"
    @  393250c59e39 default@
    │ ○  547036666102 secondary@
    ├─╯
    ○  9a462e35578a
    ◆  000000000000
    [EOF]
    ");

    // Rewrite the check-out commit in one workspace.
    main_dir.write_file("file", "changed in main\n");
    let output = main_dir.run_jj(["squash"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Rebased 1 descendant commits.
    Working copy  (@) now at: mzvwutvl 3a9b690d (empty) (no description set)
    Parent commit (@-)      : qpvuntsm b853f7c8 (no description set)
    [EOF]
    ");

    // The secondary workspace's working-copy commit was updated.
    insta::assert_snapshot!(get_log_output(&main_dir), @"
    @  3a9b690d6e67 default@
    │ ○  90f3d42e0bff secondary@
    ├─╯
    ○  b853f7c8b006
    ◆  000000000000
    [EOF]
    ");

    // The first working copy gets automatically updated.
    let output = secondary_dir.run_jj(["st"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : pmmvwywv 90f3d42e (empty) (no description set)
    Parent commit (@-): qpvuntsm b853f7c8 (no description set)
    [EOF]
    ------- stderr -------
    Working copy  (@) now at: pmmvwywv 90f3d42e (empty) (no description set)
    Parent commit (@-)      : qpvuntsm b853f7c8 (no description set)
    Added 0 files, modified 1 files, removed 0 files
    Updated working copy to fresh commit 90f3d42e0bff
    [EOF]
    ");

    insta::assert_snapshot!(get_log_output(&secondary_dir),
    @"
    @  90f3d42e0bff secondary@
    │ ○  3a9b690d6e67 default@
    ├─╯
    ○  b853f7c8b006
    ◆  000000000000
    [EOF]
    ");
}

/// Test a dirty working copy that gets rewritten from another workspace
#[test]
fn test_workspaces_updated_by_other_with_changes_in_working_copy_automatic() {
    let test_env = TestEnvironment::default();
    test_env.add_config("snapshot.auto-update-stale = true\n");

    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "contents\n");
    main_dir.run_jj(["new"]).success();

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    insta::assert_snapshot!(get_log_output(&main_dir), @"
    @  393250c59e39 default@
    │ ○  547036666102 secondary@
    ├─╯
    ○  9a462e35578a
    ◆  000000000000
    [EOF]
    ");

    // Rewrite all commits from one workspace.
    main_dir.write_file("file", "changed in main\n");
    let output = main_dir.run_jj(["squash"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Rebased 1 descendant commits.
    Working copy  (@) now at: mzvwutvl 3a9b690d (empty) (no description set)
    Parent commit (@-)      : qpvuntsm b853f7c8 (no description set)
    [EOF]
    ");

    // The secondary workspace's working-copy commit was updated.
    insta::assert_snapshot!(get_log_output(&main_dir), @"
    @  3a9b690d6e67 default@
    │ ○  90f3d42e0bff secondary@
    ├─╯
    ○  b853f7c8b006
    ◆  000000000000
    [EOF]
    ");

    // The first working copy gets automatically updated.
    secondary_dir.write_file("file", "modified contents\n");
    let output = secondary_dir.run_jj(["describe", "-m", "modified"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Concurrent modification detected, resolving automatically.
    Rebased 1 descendant commits onto commits rewritten by other operation.
    Working copy  (@) now at: pmmvwywv/2 90f3d42e (divergent) (empty) (no description set)
    Parent commit (@-)      : qpvuntsm b853f7c8 (no description set)
    Added 0 files, modified 1 files, removed 0 files
    Updated working copy to fresh commit 90f3d42e0bff
    Working copy  (@) now at: pmmvwywv/0 c38323e3 (divergent) (empty) modified
    Parent commit (@-)      : qpvuntsm b853f7c8 (no description set)
    [EOF]
    ");

    // The snapshotting of the modified contents happens on top of the old
    // operation. The `describe` operation itself happens on top the reconciled
    // operation.
    let output = main_dir.run_jj(["op", "log", "-Tdescription"]);
    insta::assert_snapshot!(output, @"
    @  describe commit 90f3d42e0bff073721e2640e32c18fb1c386d7ce
    ○    reconcile divergent operations
    ├─╮
    ○ │  squash commits into 9a462e35578a347e6a3951bf7a58ad7146959a8b
    ○ │  snapshot working copy
    │ ○  snapshot working copy
    ├─╯
    ○  create initial working-copy commit in workspace secondary
    ○  add workspace 'secondary'
    ○  new empty commit
    ○  snapshot working copy
    ○  add workspace 'default'
    ○
    [EOF]
    ");

    // We get divergence between the newly described commit and the commit created
    // by snapshotting (the reconciliation happened to point secondary@ to the child
    // of the squashed commit rather than the snapshot commit).
    insta::assert_snapshot!(get_log_output(&secondary_dir),
    @r#"
    @  c38323e3e6f3 secondary@ (divergent) "modified"
    │ ×  48a90f069c8c (divergent)
    ├─╯
    │ ○  3a9b690d6e67 default@
    ├─╯
    ○  b853f7c8b006
    ◆  000000000000
    [EOF]
    "#);
}

#[test_case(false; "manual")]
#[test_case(true; "automatic")]
fn test_workspaces_current_op_discarded_by_other(automatic: bool) {
    let test_env = TestEnvironment::default();
    if automatic {
        test_env.add_config("snapshot.auto-update-stale = true\n");
    }

    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("modified", "base\n");
    main_dir.write_file("deleted", "base\n");
    main_dir.write_file("sparse", "base\n");
    main_dir.run_jj(["new"]).success();
    main_dir.write_file("modified", "main\n");
    main_dir.run_jj(["new"]).success();

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    // Make unsnapshotted writes in the secondary working copy
    secondary_dir
        .run_jj([
            "sparse",
            "set",
            "--clear",
            "--add=modified",
            "--add=deleted",
            "--add=added",
        ])
        .success();
    secondary_dir.write_file("modified", "secondary\n");
    secondary_dir.remove_file("deleted");
    secondary_dir.write_file("added", "secondary\n");

    // Create an op by abandoning the parent commit. Importantly, that commit also
    // changes the target tree in the secondary workspace.
    main_dir.run_jj(["abandon", "@-"]).success();

    let output = main_dir.run_jj([
        "operation",
        "log",
        "--template",
        r#"id.short(10) ++ " " ++ description"#,
    ]);
    insta::allow_duplicates! {
        insta::assert_snapshot!(output, @"
        @  a9b524b948 abandon commit de90575a14d8b9198dc0930f9de4a69f846ded36
        ○  0392b7d733 create initial working-copy commit in workspace secondary
        ○  766373d1f4 add workspace 'secondary'
        ○  eb6701963b new empty commit
        ○  1d937e1f1e snapshot working copy
        ○  e22ce69861 new empty commit
        ○  48aa617132 snapshot working copy
        ○  f63ee16f95 add workspace 'default'
        ○  0000000000
        [EOF]
        ");
    }

    // Abandon ops, including the one the secondary workspace is currently on.
    main_dir.run_jj(["operation", "abandon", "..@-"]).success();
    main_dir.run_jj(["util", "gc", "--expire=now"]).success();

    insta::allow_duplicates! {
        insta::assert_snapshot!(get_log_output(&main_dir), @"
        @  320bc89effc9 default@
        │ ○  891f00062e10 secondary@
        ├─╯
        ○  367415be5b44
        ◆  000000000000
        [EOF]
        ");
    }

    if automatic {
        // Run a no-op command to set the randomness seed for commit hashes.
        secondary_dir.run_jj(["help"]).success();

        let output = secondary_dir.run_jj(["st"]);
        insta::assert_snapshot!(output, @"
        Working copy changes:
        C {modified => added}
        D deleted
        M modified
        Working copy  (@) : kmkuslsw 18851b39 RECOVERY COMMIT FROM `jj workspace update-stale`
        Parent commit (@-): rzvqmyuk 891f0006 (empty) (no description set)
        [EOF]
        ------- stderr -------
        Failed to read working copy's current operation; attempting recovery. Error message from read attempt: Object 0392b7d73383f694906abd6ffd55416c30d1775a9790553062784f5c4553c09746b388aa86fb7d27b113d1096f50845e28dc24b3de8b1a9d515cbde3b44eb346 of type operation not found
        Created and checked out recovery commit 866928d1e0fd
        [EOF]
        ");
    } else {
        let output = secondary_dir.run_jj(["st"]);
        insta::assert_snapshot!(output, @"
        ------- stderr -------
        Error: Could not read working copy's operation.
        Hint: Run `jj workspace update-stale` to recover.
        See https://docs.jj-vcs.dev/latest/working-copy/#stale-working-copy for more information.
        [EOF]
        [exit status: 1]
        ");

        let output = secondary_dir.run_jj(["workspace", "update-stale"]);
        insta::assert_snapshot!(output, @"
        ------- stderr -------
        Failed to read working copy's current operation; attempting recovery. Error message from read attempt: Object 0392b7d73383f694906abd6ffd55416c30d1775a9790553062784f5c4553c09746b388aa86fb7d27b113d1096f50845e28dc24b3de8b1a9d515cbde3b44eb346 of type operation not found
        Created and checked out recovery commit 866928d1e0fd
        [EOF]
        ");
    }

    insta::allow_duplicates! {
        insta::assert_snapshot!(get_log_output(&main_dir), @r#"
        @  320bc89effc9 default@
        │ ○  18851b397d09 secondary@ "RECOVERY COMMIT FROM `jj workspace update-stale`"
        │ ○  891f00062e10
        ├─╯
        ○  367415be5b44
        ◆  000000000000
        [EOF]
        "#);
    }

    // The sparse patterns should remain
    let output = secondary_dir.run_jj(["sparse", "list"]);
    insta::allow_duplicates! {
        insta::assert_snapshot!(output, @"
        added
        deleted
        modified
        [EOF]
        ");
    }
    let output = secondary_dir.run_jj(["st"]);
    insta::allow_duplicates! {
        insta::assert_snapshot!(output, @"
        Working copy changes:
        C {modified => added}
        D deleted
        M modified
        Working copy  (@) : kmkuslsw 18851b39 RECOVERY COMMIT FROM `jj workspace update-stale`
        Parent commit (@-): rzvqmyuk 891f0006 (empty) (no description set)
        [EOF]
        ");
    }
    insta::allow_duplicates! {
        // The modified file should have the same contents it had before (not reset to
        // the base contents)
        insta::assert_snapshot!(secondary_dir.read_file("modified"), @"secondary");
    }

    let output = secondary_dir.run_jj(["evolog"]);
    if automatic {
        insta::assert_snapshot!(output, @"
        @  kmkuslsw test.user@example.com 2001-02-03 08:05:18 secondary@ 18851b39
        │  RECOVERY COMMIT FROM `jj workspace update-stale`
        │  -- operation 91f539374e6a snapshot working copy
        ○  kmkuslsw/1 test.user@example.com 2001-02-03 08:05:18 866928d1 (hidden)
           (empty) RECOVERY COMMIT FROM `jj workspace update-stale`
           -- operation 2a845e0b4514 recovery commit
        [EOF]
        ");
    } else {
        insta::assert_snapshot!(output, @"
        @  kmkuslsw test.user@example.com 2001-02-03 08:05:18 secondary@ 18851b39
        │  RECOVERY COMMIT FROM `jj workspace update-stale`
        │  -- operation 2d387a4a6355 snapshot working copy
        ○  kmkuslsw/1 test.user@example.com 2001-02-03 08:05:18 866928d1 (hidden)
           (empty) RECOVERY COMMIT FROM `jj workspace update-stale`
           -- operation 2a845e0b4514 recovery commit
        [EOF]
        ");
    }
}

#[test]
fn test_workspaces_update_stale_noop() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    let output = main_dir.run_jj(["workspace", "update-stale"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Attempted recovery, but the working copy is not stale.
    [EOF]
    ");

    let output = main_dir.run_jj(["workspace", "update-stale", "--ignore-working-copy"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: This command must be able to update the working copy.
    Hint: Don't use --ignore-working-copy.
    [EOF]
    [exit status: 1]
    ");

    let output = main_dir.run_jj_with(|cmd| {
        cmd.env("JJ_LOG", "info")
            .args(["workspace", "update-stale", "--ignore-working-copy"])
    });
    let log = output.stderr.raw();
    assert!(
        log.lines()
            .any(|line| line.contains("working_copy_not_writable")
                && line.contains("ignore_working_copy")
                && line.contains("true")),
        "{log}"
    );

    let output = main_dir.run_jj(["op", "log", "-Tdescription"]);
    insta::assert_snapshot!(output, @"
    @  add workspace 'default'
    ○
    [EOF]
    ");
}

/// If the working copy was last updated to an unpublished operation, it should
/// be reported, even if the latest published operation has the same tree.
#[test]
fn test_workspaces_unpublished_operation_same_tree() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.run_jj(["desc", "-m=A"]).success();
    let a_op_id = main_dir.current_operation_id();
    main_dir.run_jj(["new", "-m=B"]).success();
    let b_op_id = main_dir.current_operation_id();
    // Make the repo forget about the B operation
    main_dir.remove_file(format!(".jj/repo/op_heads/heads/{b_op_id}"));
    main_dir.write_file(format!(".jj/repo/op_heads/heads/{a_op_id}"), "");
    main_dir
        .run_jj(["new", "-m=C", "--ignore-working-copy"])
        .success();
    // The working copy should be stale
    let output = main_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Internal error: The repo was loaded at operation eb46e46959f1, which seems to be a sibling of the working copy's operation 2a68fa5a5771
    Hint: Run `jj op integrate 2a68fa5a5771` to add the working copy's operation to the operation log.
    [EOF]
    [exit status: 255]
    ");
    // Test recovery by running `jj workspace update-stale` even though `jj op
    // integrate` is a better solution
    let output = main_dir.run_jj(["workspace", "update-stale"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: zsuskuln 36a15ac4 (empty) C
    Parent commit (@-)      : qpvuntsm 8777db25 (empty) A
    Updated working copy to fresh commit 36a15ac414e8
    [EOF]
    ");
}

/// Test "update-stale" in a dirty, but not stale working copy.
#[test]
fn test_workspaces_update_stale_snapshot() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "changed in main\n");
    main_dir.run_jj(["new"]).success();
    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    // Record new operation in one workspace.
    main_dir.run_jj(["new"]).success();

    // Snapshot the other working copy, which unfortunately results in concurrent
    // operations, but should be resolved cleanly.
    secondary_dir.write_file("file", "changed in second\n");
    let output = secondary_dir.run_jj(["workspace", "update-stale"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Concurrent modification detected, resolving automatically.
    Attempted recovery, but the working copy is not stale.
    [EOF]
    ");

    insta::assert_snapshot!(get_log_output(&secondary_dir), @"
    @  35d779b3baea secondary@
    │ ○  c9516583d53b default@
    │ ○  f6ae7810ef56
    ├─╯
    ○  7d5738ba9943
    ◆  000000000000
    [EOF]
    ");
}

/// Test that "workspace update-stale" works in colocated repos.
///
/// This is a regression test for a bug introduced in commit 7a296ca1 where
/// the reload-to-HEAD logic (added to fix a race condition) would break
/// "workspace update-stale" by reloading the repo to HEAD before snapshotting,
/// even though recovery intentionally loads at an old operation.
#[test]
fn test_colocated_workspace_update_stale() {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");
    let git_repo = git::open(main_dir.root());

    main_dir.write_file("file", "contents\n");
    main_dir.run_jj(["new"]).success();

    // Create new bookmarked revision from the main workspace.
    main_dir
        .run_jj(["new", "--no-edit", "root()", "-mold book1"])
        .success();
    main_dir
        .run_jj(["bookmark", "set", "-rsubject('old book1')", "book1"])
        .success();

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    // Rewrite the check-out commit from the secondary workspace.
    // This makes the main (colocated) workspace's working copy stale.
    secondary_dir.write_file("file", "changed in secondary\n");
    secondary_dir.run_jj(["squash"]).success();

    // Update and export the bookmark from the secondary workspace.
    secondary_dir
        .run_jj(["new", "--no-edit", "root()", "-mnew book1"])
        .success();
    secondary_dir
        .run_jj([
            "bookmark",
            "set",
            "-rsubject('new book1')",
            "--allow-backwards",
            "book1",
        ])
        .success();
    secondary_dir.run_jj(["git", "export"]).success();

    // Create new Git ref and commit which will be imported later by "jj
    // workspace update-stale".
    git::add_commit(&git_repo, "refs/heads/book2", "file", b"", "book2", &[]);

    insta::assert_snapshot!(get_log_output(&secondary_dir), @r#"
    @  9cb8253861b5 secondary@
    │ ○  f562bf82f2da default@
    ├─╯
    ○  30ed2f28b710
    │ ○  e97ad7861f78 book1 "new book1"
    ├─╯
    │ ○  f656b467890b "old book1"
    ├─╯
    ◆  000000000000
    [EOF]
    "#);

    // The main workspace's working copy is now stale.
    let output = main_dir.run_jj(["st"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: The working copy is stale (not updated since operation 572e45b3fba3).
    Hint: Run `jj workspace update-stale` to update it.
    See https://docs.jj-vcs.dev/latest/working-copy/#stale-working-copy for more information.
    [EOF]
    [exit status: 1]
    ");

    // Before the fix, this would fail with the same "working copy is stale" error
    // because the colocated repo reload logic would reload to HEAD before
    // snapshotting, breaking the recovery.
    let output = main_dir.run_jj(["workspace", "update-stale"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: rlvkpnrz f562bf82 (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 30ed2f28 (no description set)
    Added 0 files, modified 1 files, removed 0 files
    Updated working copy to fresh commit f562bf82f2da
    Done importing changes from the underlying Git repo.
    [EOF]
    ");

    // Verify the workspace is now up-to-date. New bookmark "book2" should have
    // been imported by the previous command.
    let output = main_dir.run_jj(["st"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : rlvkpnrz f562bf82 (empty) (no description set)
    Parent commit (@-): qpvuntsm 30ed2f28 (no description set)
    [EOF]
    ");

    // The updated bookmark "book1" shouldn't be re-imported as an external
    // change. If it were, the "old book1" revision would be abandoned.
    insta::assert_snapshot!(get_log_output(&main_dir), @r#"
    @  f562bf82f2da default@
    │ ○  9cb8253861b5 secondary@
    ├─╯
    ○  30ed2f28b710
    │ ○  7fe3ff3b9a60 book2 "book2"
    ├─╯
    │ ○  e97ad7861f78 book1 "new book1"
    ├─╯
    │ ○  f656b467890b "old book1"
    ├─╯
    ◆  000000000000
    [EOF]
    "#);
}

/// Test forgetting workspaces
#[test]
fn test_workspaces_forget() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["new"]).success();

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    let output = main_dir.run_jj(["workspace", "forget"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Warning: The current workspace 'default' no longer exists after this operation. The working copy was left untouched.
    Hint: Restore to an operation that contains the workspace (e.g. `jj undo` or `jj redo`).
    [EOF]
    ");

    // When listing workspaces, only the secondary workspace shows up
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    secondary: ../secondary pmmvwywv 31da1455 (empty) (no description set)
    [EOF]
    ");

    // After forgetting the default, secondary root is still recorded, default no
    // longer exists
    let output = main_dir.run_jj(["workspace", "root", "--name", "secondary"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/secondary
    [EOF]
    ");
    let output = main_dir.run_jj(["workspace", "root", "--name", "default"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: No such workspace: default
    [EOF]
    [exit status: 1]
    ");

    // The old working copy doesn't get an "@" in the log output
    // TODO: It seems useful to still have the "secondary@" marker here even though
    // there's only one workspace. We should show it when the command is not run
    // from that workspace.
    insta::assert_snapshot!(get_log_output(&main_dir), @"
    ○  31da14559558
    ○  006bd1130b84
    ◆  000000000000
    [EOF]
    ");

    // Revision "@" cannot be used
    let output = main_dir.run_jj(["log", "-r", "@"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: Workspace `default` doesn't have a working-copy commit
    [EOF]
    [exit status: 1]
    ");

    // Try to add back the workspace
    // TODO: We should make this just add it back instead of failing
    let output = main_dir.run_jj(["workspace", "add", "."]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: Destination path exists and is not an empty directory
    [EOF]
    [exit status: 1]
    ");

    // Add a third workspace...
    main_dir.run_jj(["workspace", "add", "../third"]).success();
    // ... and then forget it, a non-existent one, and the secondary workspace too
    let output = main_dir.run_jj(["workspace", "forget", "secondary", "nonexistent", "third"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Warning: No such workspace: nonexistent
    [EOF]
    ");
    assert!(test_env.env_root().join("secondary").is_dir());
    assert!(test_env.env_root().join("third").is_dir());
    // No workspaces left
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output, @"");
}

/// Test forgetting workspace created before workspace store
#[test]
fn test_workspaces_forget_from_before_workspace_store() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.remove_dir_all(".jj/repo/workspace_store");

    let output = main_dir.run_jj(["workspace", "forget"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Warning: The current workspace 'default' no longer exists after this operation. The working copy was left untouched.
    Hint: Restore to an operation that contains the workspace (e.g. `jj undo` or `jj redo`).
    [EOF]
    ");

    // No workspaces left
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output, @"");
}

#[test]
fn test_workspaces_forget_nothing_changed() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let output = main_dir.run_jj(["workspace", "forget", "second", "third"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Warning: No such workspace: second
    Warning: No such workspace: third
    Nothing changed.
    [EOF]
    ");
}

#[test]
fn test_workspaces_forget_multi_transaction() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["new"]).success();

    main_dir.run_jj(["workspace", "add", "../second"]).success();
    main_dir.run_jj(["workspace", "add", "../third"]).success();

    // there should be three workspaces
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . rlvkpnrz f6bf8819 (empty) (no description set)
    second: ../second pmmvwywv 31da1455 (empty) (no description set)
    third: ../third rzvqmyuk bf5b5b4d (empty) (no description set)
    [EOF]
    ");

    // delete two at once, in a single tx
    main_dir
        .run_jj(["workspace", "forget", "second", "third", "fourth"])
        .success();
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . rlvkpnrz f6bf8819 (empty) (no description set)
    [EOF]
    ");

    // the op log should have the multiple valid workspaces forgotten in a single tx
    let output = main_dir.run_jj(["op", "log", "--limit", "1"]);
    insta::assert_snapshot!(output, @"
    @  da3075edb24f test-username@host.example.com default@ 2001-02-03 04:05:12.000 +07:00 - 2001-02-03 04:05:12.000 +07:00
    │  forget workspaces second, third
    │  args: jj workspace forget second third fourth
    [EOF]
    ");

    // now, undo, and that should restore both workspaces
    main_dir.run_jj(["undo"]).success();

    // finally, there should be three workspaces at the end, with their
    // recorded paths intact
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . rlvkpnrz f6bf8819 (empty) (no description set)
    second: ../second pmmvwywv 31da1455 (empty) (no description set)
    third: ../third rzvqmyuk bf5b5b4d (empty) (no description set)
    [EOF]
    ");
}

#[test]
fn test_workspaces_forget_abandon_commits() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");

    main_dir.run_jj(["workspace", "add", "../second"]).success();
    main_dir.run_jj(["workspace", "add", "../third"]).success();
    main_dir.run_jj(["workspace", "add", "../fourth"]).success();
    let third_dir = test_env.work_dir("third");
    third_dir.run_jj(["edit", "second@"]).success();
    let fourth_dir = test_env.work_dir("fourth");
    fourth_dir.run_jj(["edit", "second@"]).success();

    // there should be four workspaces, three of which are at the same empty commit
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . qpvuntsm 006bd113 (no description set)
    fourth: ../fourth uuqppmxq 94f41578 (empty) (no description set)
    second: ../second uuqppmxq 94f41578 (empty) (no description set)
    third: ../third uuqppmxq 94f41578 (empty) (no description set)
    [EOF]
    ");
    insta::assert_snapshot!(get_log_output(&main_dir), @"
    @  006bd1130b84 default@
    │ ○  94f41578a9e1 fourth@ second@ third@
    ├─╯
    ◆  000000000000
    [EOF]
    ");

    // delete the default workspace (should not abandon commit since not empty)
    main_dir
        .run_jj(["workspace", "forget", "default"])
        .success();
    insta::assert_snapshot!(get_log_output(&main_dir), @"
    ○  94f41578a9e1 fourth@ second@ third@
    │ ○  006bd1130b84
    ├─╯
    ◆  000000000000
    [EOF]
    ");

    // delete the second workspace (should not abandon commit since other workspaces
    // still have commit checked out)
    main_dir.run_jj(["workspace", "forget", "second"]).success();
    insta::assert_snapshot!(get_log_output(&main_dir), @"
    ○  94f41578a9e1 fourth@ third@
    │ ○  006bd1130b84
    ├─╯
    ◆  000000000000
    [EOF]
    ");

    // delete the last 2 workspaces (commit should be abandoned now even though
    // forgotten in same tx)
    main_dir
        .run_jj(["workspace", "forget", "third", "fourth"])
        .success();
    insta::assert_snapshot!(get_log_output(&main_dir), @"
    ○  006bd1130b84
    ◆  000000000000
    [EOF]
    ");
}

#[test]
fn test_workspaces_remove_removes_directory() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    let secondary_dir = test_env.work_dir("secondary");
    secondary_dir.write_file("secondary_file", "secondary contents");
    secondary_dir.run_jj(["status"]).success();

    assert!(test_env.env_root().join("secondary").is_dir());
    assert!(test_env.env_root().join("secondary/file").is_file());
    assert!(
        test_env
            .env_root()
            .join("secondary/secondary_file")
            .is_file()
    );

    let output = main_dir.run_jj(["workspace", "remove", "secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Removed workspace directory "$TEST_ENV/secondary".
    [EOF]
    "#);

    assert!(!test_env.env_root().join("secondary").exists());

    let output = main_dir.run_jj(["log", "--limit", "3"]);
    insta::assert_snapshot!(output, @r#"
    @  rlvkpnrz test.user@example.com 2001-02-03 08:05:08 504e3d8c
    │  (empty) (no description set)
    │ ○  pmmvwywv test.user@example.com 2001-02-03 08:05:10 0464fb03
    ├─╯  (no description set)
    ○  qpvuntsm test.user@example.com 2001-02-03 08:05:08 7b22a8cb
    │  initial
    [EOF]
    "#);

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    assert!(test_env.env_root().join("secondary").is_dir());
}

#[test]
fn test_workspaces_remove_snapshots_before_removal() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    let secondary_dir = test_env.work_dir("secondary");
    secondary_dir.write_file("unsnapshotted.txt", "important data");

    let output = main_dir.run_jj(["workspace", "remove", "secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Removed workspace directory "$TEST_ENV/secondary".
    [EOF]
    "#);

    assert!(!test_env.env_root().join("secondary").exists());

    let output = main_dir.run_jj(["log", "-r", r#"files("unsnapshotted.txt")"#, "--summary"]);
    insta::assert_snapshot!(output, @"
    ○  pmmvwywv test.user@example.com 2001-02-03 08:05:10 e7db06a0
    │  (no description set)
    ~  A unsnapshotted.txt
    [EOF]
    ");
}

#[test]
fn test_workspaces_remove_colocated() {
    let test_env = TestEnvironment::default();
    test_env.add_config("git.colocate = true");
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    assert!(test_env.env_root().join("secondary/.git").is_file());

    let output = main_dir.run_jj(["workspace", "remove", "secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Removed Git worktree for "$TEST_ENV/secondary".
    Removed workspace directory "$TEST_ENV/secondary".
    [EOF]
    "#);
    assert!(!test_env.env_root().join("secondary").exists());

    let main_repo = git::open(test_env.env_root().join("main"));
    assert_eq!(git_worktree_ids(&main_repo), [] as [String; 0]);
}

#[test]
fn test_workspaces_remove_requires_writable_working_copy() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    let secondary_dir = test_env.work_dir("secondary");
    secondary_dir.write_file("unsnapshotted.txt", "important data");

    let output = main_dir.run_jj(["--ignore-working-copy", "workspace", "remove", "secondary"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: This command must be able to update the working copy.
    Hint: Don't use --ignore-working-copy.
    Hint: Use `jj workspace forget` to stop tracking a workspace without deleting files.
    [EOF]
    [exit status: 1]
    "#);
    assert!(
        test_env
            .env_root()
            .join("secondary/unsnapshotted.txt")
            .is_file()
    );

    let output = main_dir.run_jj(["--at-op=@-", "workspace", "remove", "secondary"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: This command must be able to update the working copy.
    Hint: Don't use --at-op.
    Hint: Use `jj workspace forget` to stop tracking a workspace without deleting files.
    [EOF]
    [exit status: 1]
    "#);
    assert!(
        test_env
            .env_root()
            .join("secondary/unsnapshotted.txt")
            .is_file()
    );

    let output = main_dir.run_jj([
        "--no-integrate-operation",
        "workspace",
        "remove",
        "secondary",
    ]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: This command must be able to update the working copy.
    Hint: Don't use --no-integrate-operation.
    Hint: Use `jj workspace forget` to stop tracking a workspace without deleting files.
    [EOF]
    [exit status: 1]
    "#);
    assert!(
        test_env
            .env_root()
            .join("secondary/unsnapshotted.txt")
            .is_file()
    );
}

#[test]
fn test_workspaces_remove_refuses_main_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    let output = main_dir.run_jj(["workspace", "remove", "default"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: Cannot remove workspace 'default' because it contains the repository
    [EOF]
    [exit status: 1]
    "#);
    assert!(test_env.env_root().join("main/.jj/repo").is_dir());
}

#[test]
fn test_workspaces_remove_refuses_foreign_repository() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    // Replace the recorded workspace directory with an unrelated repository.
    std::fs::remove_dir_all(test_env.env_root().join("secondary")).unwrap();
    test_env
        .run_jj_in(".", ["git", "init", "secondary"])
        .success();

    let output = main_dir.run_jj(["workspace", "remove", "secondary"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: Cannot remove workspace 'secondary' because it belongs to another repository
    Hint: Use `jj workspace forget` to stop tracking a workspace without deleting files.
    [EOF]
    [exit status: 1]
    "#);
    assert!(test_env.env_root().join("secondary/.jj").is_dir());
}

#[test]
fn test_workspaces_remove_current_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    let output = main_dir.run_jj(["-R", "../secondary", "workspace", "remove", "secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Warning: The current workspace 'secondary' no longer exists after this operation. The working copy was left untouched.
    Hint: Restore to an operation that contains the workspace (e.g. `jj undo` or `jj redo`).
    Removed workspace directory "$TEST_ENV/secondary".
    [EOF]
    "#);
    assert!(!test_env.env_root().join("secondary").exists());
}

#[test]
fn test_list_workspaces_template() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();
    main_dir
        .run_jj(["workspace", "add", "--name", "second", "../secondary"])
        .success();

    // Default template
    let output = main_dir.run_jj(["workspace", "list", "--color=always"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    [38;5;2mdefault[39m: [39m.[39m [1m[38;5;13mr[38;5;8mlvkpnrz[39m [38;5;12m5[38;5;8m04e3d8c[39m [38;5;10m(empty)[39m [38;5;10m(no description set)[0m
    [38;5;2msecond[39m: [39m../secondary[39m [1m[38;5;5mp[0m[38;5;8mmmvwywv[39m [1m[38;5;4m05[0m[38;5;8m8f604d[39m [38;5;2m(empty)[39m [38;5;2m(no description set)[39m
    [EOF]
    ");

    let template = indoc! {r#"
        name ++ ": " ++ target.commit_id().short() ++ " " ++
        target.description().first_line() ++
        if(target.current_working_copy(), " (current)") ++ "\n"
    "#};

    // "current_working_copy" should point to the workspace we operate on
    let output = main_dir.run_jj(["workspace", "list", "-T", template]);
    insta::assert_snapshot!(output, @"
    default: 504e3d8c1bcd  (current)
    second: 058f604dffcd 
    [EOF]
    ");

    let output = secondary_dir.run_jj(["workspace", "list", "-T", template]);
    insta::assert_snapshot!(output, @"
    default: 504e3d8c1bcd 
    second: 058f604dffcd  (current)
    [EOF]
    ");
}

#[test]
fn test_list_workspaces_template_root() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir
        .run_jj(["workspace", "add", "--name", "second", "../secondary"])
        .success();

    let template = r#"name ++ ": " ++ root ++ "\n""#;
    let output = main_dir.run_jj(["workspace", "list", "-T", template]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: $TEST_ENV/main
    second: $TEST_ENV/secondary
    [EOF]
    ");

    let template = r#"name ++ ": " ++ if(root, root ++ " " ++ root.relative()) ++ "\n""#;
    let output = main_dir.run_jj(["workspace", "list", "-T", template]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: $TEST_ENV/main .
    second: $TEST_ENV/secondary ../secondary
    [EOF]
    ");

    let template = r#"name ++ ": " ++ if(root, root.relative()) ++ "\n""#;
    let output = main_dir.run_jj(["workspace", "list", "-T", template]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: .
    second: ../secondary
    [EOF]
    ");
}

#[test]
fn test_list_workspaces_template_root_unavailable() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir
        .run_jj(["workspace", "add", "--name", "second", "../secondary"])
        .success();
    std::fs::remove_dir_all(test_env.env_root().join("secondary")).unwrap();

    let template = r#"name ++ ": " ++ root ++ "\n""#;
    let output = main_dir.run_jj(["workspace", "list", "-T", template]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: $TEST_ENV/main
    second: $TEST_ENV/secondary
    [EOF]
    ");

    let template = r#"name ++ ": " ++ if(root, root.relative()) ++ "\n""#;
    let output = main_dir.run_jj(["workspace", "list", "-T", template]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: .
    second: ../secondary
    [EOF]
    ");

    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . qpvuntsm e8849ae1 (empty) (no description set)
    second: ../secondary uuqppmxq 94f41578 (empty) (no description set)
    [EOF]
    ");
}

/// Test getting the workspace root from primary and secondary workspaces
#[test]
fn test_workspaces_root() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let secondary_dir = test_env.work_dir("secondary");

    let output = main_dir.run_jj(["workspace", "root"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/main
    [EOF]
    ");
    let main_subdir_dir = main_dir.create_dir("subdir");
    let output = main_subdir_dir.run_jj(["workspace", "root"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/main
    [EOF]
    ");

    main_dir
        .run_jj(["workspace", "add", "--name", "secondary", "../secondary"])
        .success();
    // Explicitly request root of 'secondary' workspace from main workspace
    let output = main_dir.run_jj(["workspace", "root", "--name", "secondary"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/secondary
    [EOF]
    ");
    let output = secondary_dir.run_jj(["workspace", "root"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/secondary
    [EOF]
    ");
    let secondary_subdir_dir = secondary_dir.create_dir("subdir");
    let output = secondary_subdir_dir.run_jj(["workspace", "root"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/secondary
    [EOF]
    ");
}

#[test]
fn test_workspaces_relative_path() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    let repo_file = test_env.env_root().join("secondary/.jj/repo");
    let repo_path_bytes = std::fs::read(&repo_file)?;
    let stored_path = String::from_utf8(repo_path_bytes)?;
    assert_eq!(stored_path, "../../main/.jj/repo");

    let secondary_dir = test_env.work_dir("secondary");
    let output = secondary_dir.run_jj(["status"]);
    insta::assert_snapshot!(output, @"
    The working copy has no changes.
    Working copy  (@) : uuqppmxq 94f41578 (empty) (no description set)
    Parent commit (@-): zzzzzzzz 00000000 (empty) (no description set)
    [EOF]
    ");

    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . qpvuntsm e8849ae1 (empty) (no description set)
    secondary: ../secondary uuqppmxq 94f41578 (empty) (no description set)
    [EOF]
    ");

    let output = secondary_dir.run_jj(["workspace", "root"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/secondary
    [EOF]
    ");

    let secondary_subdir = secondary_dir.create_dir("subdir");
    let output = secondary_subdir.run_jj(["workspace", "root"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/secondary
    [EOF]
    ");

    let output = main_dir.run_jj(["workspace", "root", "--name", "secondary"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/secondary
    [EOF]
    ");
    Ok(())
}

#[test]
fn test_workspaces_root_unavailable() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();

    std::fs::remove_dir_all(test_env.env_root().join("secondary"))?;

    let output = main_dir.run_jj(["workspace", "root", "--name", "secondary"]);
    insta::assert_snapshot!(output.normalize_backslash().strip_stderr_last_line(), @"
    $TEST_ENV/secondary
    [EOF]
    ------- stderr -------
    Warning: Cannot access workspace path: $TEST_ENV/secondary
    [EOF]
    ");
    Ok(())
}

#[test]
fn test_debug_snapshot() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    work_dir.write_file("file", "contents");
    work_dir.run_jj(["debug", "snapshot"]).success();
    let output = work_dir.run_jj(["op", "log"]);
    insta::assert_snapshot!(output, @"
    @  d870c9898b59 test-username@host.example.com default@ 2001-02-03 04:05:08.000 +07:00 - 2001-02-03 04:05:08.000 +07:00
    │  snapshot working copy
    │  args: jj debug snapshot
    ○  f63ee16f9553 test-username@host.example.com 2001-02-03 04:05:07.000 +07:00 - 2001-02-03 04:05:07.000 +07:00
    │  add workspace 'default'
    ○  000000000000 root()
    [EOF]
    ");
    work_dir.run_jj(["describe", "-m", "initial"]).success();
    let output = work_dir.run_jj(["op", "log"]);
    insta::assert_snapshot!(output, @"
    @  fd7a8fca455a test-username@host.example.com default@ 2001-02-03 04:05:10.000 +07:00 - 2001-02-03 04:05:10.000 +07:00
    │  describe commit 006bd1130b84e90ab082adeabd7409270d5a86da
    │  args: jj describe -m initial
    ○  d870c9898b59 test-username@host.example.com default@ 2001-02-03 04:05:08.000 +07:00 - 2001-02-03 04:05:08.000 +07:00
    │  snapshot working copy
    │  args: jj debug snapshot
    ○  f63ee16f9553 test-username@host.example.com 2001-02-03 04:05:07.000 +07:00 - 2001-02-03 04:05:07.000 +07:00
    │  add workspace 'default'
    ○  000000000000 root()
    [EOF]
    ");
}

#[test]
fn test_workspaces_rename_nothing_changed() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    let output = main_dir.run_jj(["workspace", "rename", "default"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Nothing changed.
    [EOF]
    ");
}

#[test]
fn test_workspaces_rename_new_workspace_name_already_used() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    main_dir
        .run_jj(["workspace", "add", "--name", "second", "../secondary"])
        .success();
    let output = main_dir.run_jj(["workspace", "rename", "second"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: Failed to rename a workspace
    Caused by: Workspace second already exists
    [EOF]
    [exit status: 1]
    ");
}

#[test]
fn test_workspaces_remove_undo_readd() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    assert!(test_env.env_root().join("secondary").is_dir());

    main_dir
        .run_jj(["workspace", "remove", "secondary"])
        .success();
    assert!(!test_env.env_root().join("secondary").exists());

    let output = main_dir.run_jj(["undo"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Warning: Undoing a workspace removal restores the workspace, but does not restore the workspace directory.
    Hint: Run `jj workspace forget` then `jj workspace add` to recreate the directory.
    Undid operation: 3c30feac724f (2001-02-03 08:05:10) remove workspace secondary
    Restored to operation: 9d798e222e29 (2001-02-03 08:05:09) create initial working-copy commit in workspace secondary
    [EOF]
    "#);

    // Removing a workspace deletes its store path, so undo cannot restore it.
    let output = main_dir.run_jj(["workspace", "root", "--name", "secondary"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: Workspace has no recorded path: secondary
    [EOF]
    [exit status: 1]
    "#);

    main_dir
        .run_jj(["workspace", "forget", "secondary"])
        .success();
    main_dir
        .run_jj(["workspace", "add", "../secondary"])
        .success();
    assert!(test_env.env_root().join("secondary").is_dir());

    let output = main_dir.run_jj(["workspace", "root", "--name", "secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    $TEST_ENV/secondary
    [EOF]
    "#);
}

#[test]
fn test_workspaces_rename_forgotten_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    main_dir
        .run_jj(["workspace", "add", "--name", "second", "../secondary"])
        .success();
    main_dir.run_jj(["workspace", "forget", "second"]).success();
    let secondary_dir = test_env.work_dir("secondary");
    let output = secondary_dir.run_jj(["workspace", "rename", "third"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: The current workspace 'second' is not tracked in the repo.
    [EOF]
    [exit status: 1]
    ");
}

#[test]
fn test_workspaces_rename_workspace() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");
    main_dir
        .run_jj(["workspace", "add", "--name", "second", "../secondary"])
        .success();
    let secondary_dir = test_env.work_dir("secondary");

    // Both workspaces show up when we list them
    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . qpvuntsm e8849ae1 (empty) (no description set)
    second: ../secondary uuqppmxq 94f41578 (empty) (no description set)
    [EOF]
    ");

    let output = secondary_dir.run_jj(["workspace", "rename", "third"]);
    insta::assert_snapshot!(output, @"");

    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output.normalize_backslash(), @"
    default: . qpvuntsm e8849ae1 (empty) (no description set)
    third: ../secondary uuqppmxq 94f41578 (empty) (no description set)
    [EOF]
    ");

    // Can see the working-copy commit in each workspace in the log output.
    insta::assert_snapshot!(get_log_output(&main_dir), @"
    @  e8849ae12c70 default@
    │ ○  94f41578a9e1 third@
    ├─╯
    ◆  000000000000
    [EOF]
    ");
    insta::assert_snapshot!(get_log_output(&secondary_dir), @"
    @  94f41578a9e1 third@
    │ ○  e8849ae12c70 default@
    ├─╯
    ◆  000000000000
    [EOF]
    ");

    // The new workspace root is recorded and accessible
    let output = main_dir.run_jj(["workspace", "root", "--name", "secondary"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: No such workspace: secondary
    [EOF]
    [exit status: 1]
    ");
    let output = main_dir.run_jj(["workspace", "root", "--name", "third"]);
    insta::assert_snapshot!(output, @"
    $TEST_ENV/secondary
    [EOF]
    ");
}

#[test]
fn test_workspaces_rename_workspace_from_before_workspace_store() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let main_dir = test_env.work_dir("main");

    main_dir.remove_dir_all(".jj/repo/workspace_store");

    let output = main_dir.run_jj(["workspace", "rename", "third"]);
    insta::assert_snapshot!(output, @"");

    let output = main_dir.run_jj(["workspace", "list"]);
    insta::assert_snapshot!(output, @"
    third: qpvuntsm e8849ae1 (empty) (no description set)
    [EOF]
    ");

    // The workspace root is not in the store
    let output = main_dir.run_jj(["workspace", "root", "--name", "third"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Error: Workspace has no recorded path: third
    [EOF]
    [exit status: 1]
    ");

    let output = main_dir.run_jj(["workspace", "list", "-T", r#"name ++ ": " ++ root ++ "\n""#]);
    insta::assert_snapshot!(output, @"
    third: 
    [EOF]
    ");
}

#[test]
fn test_workspaces_add_forget_colocated() {
    let test_env = TestEnvironment::default();
    test_env.add_config("git.colocate = true");
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    // `--no-colocate` overrides the colocated default.
    let output = main_dir.run_jj(["workspace", "add", "--no-colocate", "../flag-off"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../flag-off"
    Working copy  (@) now at: pmmvwywv 058f604d (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    assert!(!test_env.env_root().join("flag-off/.git").exists());

    // So does `git.colocate = false`, even though the main workspace is
    // colocated.
    let output = main_dir.run_jj([
        "workspace",
        "add",
        "--config=git.colocate=false",
        "../config-off",
    ]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created workspace in "../config-off"
    Working copy  (@) now at: rzvqmyuk bcc858e1 (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    assert!(!test_env.env_root().join("config-off/.git").exists());

    // ...but `--colocate` overrides that in turn.
    let output = main_dir.run_jj([
        "workspace",
        "add",
        "--config=git.colocate=false",
        "--colocate",
        "../flag-on",
    ]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created Git worktree for the new workspace.
    Created workspace in "../flag-on"
    Working copy  (@) now at: zxsnswpr 1c1effec (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    assert!(test_env.env_root().join("flag-on/.git").is_file());

    // With neither flag, a colocated main workspace and `git.colocate = true`
    // create a worktree.
    let output = main_dir.run_jj(["workspace", "add", "../secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created Git worktree for the new workspace.
    Created workspace in "../secondary"
    Working copy  (@) now at: nppvrztz 0bfa7004 (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 7b22a8cb initial
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);
    assert!(test_env.env_root().join("secondary/.git").is_file());

    // The new workspace's .jj directory must be excluded, or Git would report
    // it as untracked in the worktree.
    assert!(
        test_env
            .env_root()
            .join("secondary/.jj/.gitignore")
            .is_file()
    );
    let secondary_repo = git::open(test_env.env_root().join("secondary"));
    insta::assert_debug_snapshot!(git::status(&secondary_repo), @r#"
    [
        GitStatus {
            path: ".jj/.gitignore",
            status: Worktree(
                Ignored,
            ),
        },
        GitStatus {
            path: ".jj/repo",
            status: Worktree(
                Ignored,
            ),
        },
        GitStatus {
            path: ".jj/working_copy",
            status: Worktree(
                Ignored,
            ),
        },
    ]
    "#);

    // The child workspace reports itself as colocated.
    let secondary_dir = test_env.work_dir("secondary");
    let output = secondary_dir.run_jj(["git", "colocation", "status", "--quiet"]);
    insta::assert_snapshot!(output, @"
    Workspace 'secondary' is currently colocated with Git.
    Last imported/exported Git HEAD: 7b22a8cbe888adcb4d5ff6dd46a38049e870c6ab
    [EOF]
    ");

    // Forgetting the workspace disconnects the worktree, but leaves the
    // directory contents in place.
    let output = main_dir.run_jj(["workspace", "forget", "secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Removed Git worktree for "$TEST_ENV/secondary".
    [EOF]
    "#);
    assert!(!test_env.env_root().join("secondary/.git").exists());
    assert!(test_env.env_root().join("secondary/file").is_file());

    // Git no longer knows about the worktree, but the others are untouched,
    // and no worktree left behind the branch `git worktree add --orphan`
    // insisted on naming.
    let main_repo = git::open(test_env.env_root().join("main"));
    assert_eq!(git_worktree_ids(&main_repo), ["flag-on"]);
    assert_eq!(local_branches(&main_repo), [] as [String; 0]);
}

#[test]
fn test_workspaces_add_colocated_at_revision() {
    let test_env = TestEnvironment::default();
    test_env.add_config("git.colocate = true");
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");

    // Build a chain of commits so the target is neither the tip nor the root.
    main_dir.write_file("file1", "one\n");
    main_dir.run_jj(["commit", "-m", "one"]).success();
    main_dir.write_file("file2", "two\n");
    main_dir.run_jj(["commit", "-m", "two"]).success();
    main_dir.write_file("file3", "three\n");
    main_dir.run_jj(["commit", "-m", "three"]).success();

    // `@---` is the commit that added file1.
    let target = main_dir
        .run_jj(["log", "--no-graph", "-r", "@---", "-T", "commit_id"])
        .success()
        .stdout
        .raw()
        .trim()
        .to_owned();

    let output = main_dir.run_jj(["workspace", "add", "-r", "@---", "../secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created Git worktree for the new workspace.
    Created workspace in "../secondary"
    Working copy  (@) now at: nppvrztz 514d6d42 (empty) (no description set)
    Parent commit (@-)      : qpvuntsm 44945a23 one
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    "#);

    // Git HEAD must be the revision that was asked for, not the main
    // workspace's HEAD.
    let secondary_repo = git::open(test_env.env_root().join("secondary"));
    assert_eq!(git_head(&secondary_repo), target);

    // jj, not git, is what populates the working copy, so only the files that
    // exist at the target revision should be present.
    assert!(test_env.env_root().join("secondary/file1").is_file());
    assert!(!test_env.env_root().join("secondary/file2").exists());
    assert!(!test_env.env_root().join("secondary/file3").exists());

    // Git and jj must agree about the state of that working copy: Git has to
    // read the worktree's own HEAD and index, not the main workspace's.
    let secondary_dir = test_env.work_dir("secondary");
    secondary_dir.write_file("file1", "modified in secondary\n");
    insta::assert_snapshot!(secondary_dir.run_jj(["diff"]), @"
    Modified regular file file1:
       1     : one
            1: modified in secondary
    [EOF]
    ");
    insta::assert_debug_snapshot!(git::status(&secondary_repo), @r#"
    [
        GitStatus {
            path: ".jj/.gitignore",
            status: Worktree(
                Ignored,
            ),
        },
        GitStatus {
            path: ".jj/repo",
            status: Worktree(
                Ignored,
            ),
        },
        GitStatus {
            path: ".jj/working_copy",
            status: Worktree(
                Ignored,
            ),
        },
        GitStatus {
            path: "file1",
            status: Worktree(
                Modified,
            ),
        },
    ]
    "#);
}

#[test]
fn test_workspaces_add_colocated_unborn_head() {
    let test_env = TestEnvironment::default();
    test_env.add_config("git.colocate = true");
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main_dir = test_env.work_dir("main");

    // A repo with no commits is fine: the worktree is created with an unborn
    // HEAD rather than being refused.
    let output = main_dir.run_jj(["workspace", "add", "../secondary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created Git worktree for the new workspace.
    Created workspace in "../secondary"
    Working copy  (@) now at: uuqppmxq 94f41578 (empty) (no description set)
    Parent commit (@-)      : zzzzzzzz 00000000 (empty) (no description set)
    [EOF]
    "#);
    let secondary_repo = git::open(test_env.env_root().join("secondary"));
    assert_eq!(git_head(&secondary_repo), "refs/jj/root");

    main_dir.write_file("file", "contents");
    main_dir.run_jj(["commit", "-m", "initial"]).success();

    // Same for a workspace whose working-copy commit has the root commit as
    // its parent: there is no Git commit to point HEAD at, so it must land on
    // jj's unborn placeholder.
    let output = main_dir.run_jj(["workspace", "add", "-r", "root()", "../tertiary"]);
    insta::assert_snapshot!(output.normalize_backslash(), @r#"
    ------- stderr -------
    Created Git worktree for the new workspace.
    Created workspace in "../tertiary"
    Working copy  (@) now at: rzvqmyuk 42960ba4 (empty) (no description set)
    Parent commit (@-)      : zzzzzzzz 00000000 (empty) (no description set)
    [EOF]
    "#);
    let tertiary_repo = git::open(test_env.env_root().join("tertiary"));
    assert_eq!(git_head(&tertiary_repo), "refs/jj/root");
    assert!(!test_env.env_root().join("tertiary/file").exists());
}

/// Returns the repo's Git HEAD: the ref name if HEAD points at a ref, or the
/// commit id if HEAD is detached.
fn git_head(repo: &gix::Repository) -> String {
    match repo.head().unwrap().kind {
        gix::head::Kind::Unborn(name) => name.as_bstr().to_string(),
        gix::head::Kind::Symbolic(reference) => reference.name.as_bstr().to_string(),
        gix::head::Kind::Detached { target, .. } => target.to_string(),
    }
}

fn local_branches(repo: &gix::Repository) -> Vec<String> {
    let mut names = repo
        .references()
        .unwrap()
        .local_branches()
        .unwrap()
        .map(|reference| reference.unwrap().name().as_bstr().to_string())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Returns the ids of the linked Git worktrees registered in the repo.
fn git_worktree_ids(repo: &gix::Repository) -> Vec<String> {
    let mut ids = repo
        .worktrees()
        .unwrap()
        .iter()
        .map(|proxy| proxy.id().to_string())
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

#[must_use]
fn get_log_output(work_dir: &TestWorkDir) -> CommandOutput {
    let template = r#"
    separate(" ",
      commit_id.short(),
      bookmarks,
      working_copies,
      if(divergent, "(divergent)"),
      surround('"', '"', description.first_line()),
    )
    "#;
    work_dir.run_jj(["log", "-T", template, "-r", "all()"])
}
