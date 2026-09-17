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

use std::collections::BTreeMap;

use jj_lib::backend::FileId;
use jj_lib::conflicts;
use jj_lib::conflicts::ConflictMarkerStyle;
use jj_lib::conflicts::MaterializedTreeValue;
use test_case::test_case;

use super::*;

struct ExactSnapshot {
    test_repo: TestRepo,
    scan_root: PathBuf,
    state_path: PathBuf,
    settings: TreeStateSettings,
}

impl ExactSnapshot {
    fn new(backend: TestRepoBackend) -> TestResult<Self> {
        let test_repo = TestRepo::init_with_backend(backend);
        let scan_root = test_repo.env.root().join("scan-root");
        let state_path = test_repo.env.root().join("state");
        std::fs::create_dir(&scan_root)?;
        std::fs::create_dir(&state_path)?;
        enable_snapshot_mode(&state_path)?;
        let settings = TreeStateSettings::try_from_user_settings(test_repo.repo.settings())?;
        TreeState::init(
            test_repo.repo.store().clone(),
            scan_root.clone(),
            state_path.clone(),
            &settings,
        )?;
        Ok(Self {
            test_repo,
            scan_root,
            state_path,
            settings,
        })
    }

    fn snapshot(
        &self,
        baseline: &MergedTree,
        paths: &[&RepoPath],
        options: &SnapshotOptions<'_>,
    ) -> TestResult<MergedTree> {
        let mut state = TreeState::load(
            self.test_repo.repo.store().clone(),
            self.scan_root.clone(),
            self.state_path.clone(),
            &self.settings,
        )?;
        state.reset(baseline).block_on()?;
        state.save()?;
        seed_test_awacs_baseline(&self.state_path, b"baseline-a", [3; 32])?;
        let settings = TreeStateSettings {
            fsmonitor_settings: FsmonitorSettings::TestAwacs {
                scan_root: self.scan_root.clone(),
                changed_files: Some(
                    paths
                        .iter()
                        .map(|path| path.to_fs_path_unchecked(Path::new("")))
                        .collect(),
                ),
                cursor: b"baseline-b".to_vec(),
            },
            ..self.settings.clone()
        };
        let mut state = TreeState::load(
            self.test_repo.repo.store().clone(),
            self.scan_root.clone(),
            self.state_path.clone(),
            &settings,
        )?;
        state.snapshot(options).block_on()?;
        Ok(state.current_tree().clone())
    }
}

fn snapshot_options() -> SnapshotOptions<'static> {
    SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    }
}

#[test]
fn exact_directories_visit_each_file_once() -> TestResult {
    let fixture = ExactSnapshot::new(TestRepoBackend::Test)?;
    let repo = &fixture.test_repo.repo;
    let baseline = create_tree(
        repo,
        &[
            (repo_path("dir/a"), "old a\n"),
            (repo_path("dir/nested/b"), "old b\n"),
            (repo_path("dir/untouched"), "retained\n"),
            (repo_path("deleted/child"), "deleted\n"),
            (repo_path("to-file/child"), "old child\n"),
            (repo_path("to-dir"), "old file\n"),
        ],
    );
    let changed = [
        (repo_path("dir/a"), "new a\n"),
        (repo_path("dir/nested/b"), "new b\n"),
        (repo_path("to-file"), "new file\n"),
        (repo_path("to-dir/child"), "new child\n"),
    ];
    for (path, content) in changed {
        testutils::write_working_copy_file(&fixture.scan_root, path, content);
    }
    // Neither the missing tracked sibling nor this new sibling is in the
    // delta. Covering directory roots must not widen the exact matcher.
    testutils::write_working_copy_file(&fixture.scan_root, repo_path("dir/unreported"), "new\n");
    let paths = [
        "dir",
        "dir/a",
        "dir/nested",
        "dir/nested/b",
        "deleted",
        "deleted/child",
        "to-file",
        "to-file/child",
        "to-dir",
        "to-dir/child",
    ]
    .map(repo_path);
    let visits = std::sync::Mutex::new(BTreeMap::new());
    let progress = |path: &RepoPath| {
        *visits.lock().unwrap().entry(path.to_owned()).or_insert(0) += 1;
    };
    let options = SnapshotOptions {
        progress: Some(&progress),
        ..snapshot_options()
    };
    let actual = fixture.snapshot(&baseline, &paths, &options)?;
    let expected = create_tree(
        repo,
        &[
            changed[0],
            changed[1],
            changed[2],
            changed[3],
            (repo_path("dir/untouched"), "retained\n"),
        ],
    );
    assert_tree_eq!(actual, expected);
    assert_eq!(
        *visits.lock().unwrap(),
        changed
            .into_iter()
            .map(|(path, _)| (path.to_owned(), 1))
            .collect()
    );
    Ok(())
}

#[test]
fn cached_ignore_context_preserves_ancestry_and_expires_after_scan() -> TestResult {
    let fixture = ExactSnapshot::new(TestRepoBackend::Test)?;
    let repo = &fixture.test_repo.repo;
    let root_ignore = repo_path(".gitignore");
    let shared_ignore = repo_path("shared/.gitignore");
    let deep_ignore = repo_path("shared/deep/.gitignore");
    let baseline = create_tree(
        repo,
        &[
            (root_ignore, "ignored/\n"),
            (shared_ignore, "*.skip\nbranch/\n"),
            (deep_ignore, "!keep.skip\n"),
            (repo_path("ignored/tracked"), "old\n"),
            (repo_path("shared/branch/tracked"), "old\n"),
        ],
    );
    let files = [
        (root_ignore, "ignored/\n"),
        (shared_ignore, "*.skip\nbranch/\n"),
        (deep_ignore, "!keep.skip\n"),
        (repo_path("ignored/tracked"), "tracked\n"),
        (repo_path("ignored/forced"), "forced\n"),
        (repo_path("ignored/other"), "ignored\n"),
        (repo_path("shared/deep/a"), "a\n"),
        (repo_path("shared/deep/b"), "b\n"),
        (repo_path("shared/deep/keep.skip"), "kept\n"),
        (repo_path("shared/deep/other.skip"), "ignored\n"),
        (repo_path("shared/branch/.gitignore"), "!new\n"),
        (repo_path("shared/branch/new"), "ignored\n"),
        (repo_path("shared/branch/tracked"), "tracked\n"),
    ];
    for (path, content) in files {
        testutils::write_working_copy_file(&fixture.scan_root, path, content);
    }
    // The force-tracked file is not itself in the exact delta. Scheduling a
    // covering directory must retain the union with force tracking.
    let mut paths = files
        .iter()
        .map(|(path, _)| *path)
        .filter(|path| *path != repo_path("ignored/forced"))
        .collect_vec();
    paths.push(repo_path("ignored"));
    let force = FilesMatcher::new([repo_path("ignored/forced")]);
    let options = SnapshotOptions {
        force_tracking_matcher: &force,
        ..snapshot_options()
    };
    let actual = fixture.snapshot(&baseline, &paths, &options)?;
    let expected_files = [
        files[0], files[1], files[2], files[3], files[4], files[6], files[7], files[8], files[12],
    ];
    let expected = create_tree(repo, &expected_files);
    assert_tree_eq!(actual, expected);

    // The next immutable cut has different nested ignore rules. A cache must
    // belong to one scan, including contexts whose parent was unchanged.
    testutils::write_working_copy_file(&fixture.scan_root, deep_ignore, "!other.skip\n");
    let next = fixture.snapshot(
        &actual,
        &[deep_ignore, repo_path("shared/deep/other.skip")],
        &snapshot_options(),
    )?;
    let mut next_files = expected_files.to_vec();
    next_files[2] = (deep_ignore, "!other.skip\n");
    next_files.push(files[9]);
    assert_tree_eq!(next, create_tree(repo, &next_files));
    Ok(())
}

#[cfg(unix)]
#[test_case("missing", false, false ; "unreported missing ancestor")]
#[test_case("file", false, false ; "unreported file ancestor")]
#[test_case("symlink", false, false ; "unreported symlink ancestor")]
#[test_case("nested", false, false ; "unreported nested repository")]
#[test_case("missing", false, true ; "covered unreported missing ancestor")]
#[test_case("file", false, true ; "covered unreported file ancestor")]
#[test_case("symlink", false, true ; "covered unreported symlink ancestor")]
#[test_case("nested", false, true ; "covered unreported nested repository")]
#[test_case("missing", true, false ; "reported missing ancestor")]
#[test_case("file", true, false ; "reported file replacement")]
#[test_case("symlink", true, false ; "reported symlink replacement")]
#[test_case("nested", true, false ; "reported nested repository")]
fn cached_parent_does_not_hide_directory_boundaries(
    kind: &str,
    reported: bool,
    covering_parent: bool,
) -> TestResult {
    let fixture = ExactSnapshot::new(TestRepoBackend::Test)?;
    let repo = &fixture.test_repo.repo;
    let ordinary = repo_path("parent/a");
    let boundary = repo_path("parent/trap");
    let child = repo_path("parent/trap/child");
    let baseline = create_tree(repo, &[(ordinary, "old\n"), (child, "old child\n")]);
    testutils::write_working_copy_file(&fixture.scan_root, ordinary, "new\n");
    let outside = fixture.test_repo.env.root().join("outside");
    std::fs::create_dir(&outside)?;
    testutils::write_working_copy_file(&outside, repo_path("child"), "outside\n");
    match kind {
        "missing" => {}
        "file" => testutils::write_working_copy_file(&fixture.scan_root, boundary, "replacement\n"),
        "symlink" => symlink_dir(&outside, boundary.to_fs_path(&fixture.scan_root)?)?,
        "nested" => {
            testutils::write_working_copy_file(&fixture.scan_root, child, "nested\n");
            testutils::write_working_copy_file(
                &fixture.scan_root,
                repo_path("parent/trap/.git"),
                "",
            );
        }
        _ => unreachable!(),
    }
    let mut paths = if reported {
        vec![ordinary, boundary, child]
    } else {
        vec![ordinary, child]
    };
    if covering_parent {
        paths.push(repo_path("parent"));
    }
    let result = fixture.snapshot(&baseline, &paths, &snapshot_options());
    if reported {
        let expected = create_tree_with(repo, |builder| {
            builder.file(ordinary, "new\n");
            match kind {
                "file" => {
                    builder.file(boundary, "replacement\n");
                }
                "symlink" => builder.symlink(boundary, outside.to_str().unwrap()),
                "missing" | "nested" => {}
                _ => unreachable!(),
            }
        });
        assert_tree_eq!(result?, expected);
    } else {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Refusing directed scan through")
        );
    }
    assert_eq!(std::fs::read(outside.join("child"))?, b"outside\n");
    Ok(())
}

#[test]
fn resolved_file_snapshot_does_not_read_old_git_blob() -> TestResult {
    let fixture = ExactSnapshot::new(TestRepoBackend::Git)?;
    let repo = &fixture.test_repo.repo;
    let path = repo_path("file");
    let missing_id = FileId::from_bytes(&[0xa5; 20]);
    assert!(
        repo.store()
            .read_file(path, &missing_id)
            .block_on()
            .is_err()
    );
    let mut builder = TreeBuilder::new(repo.store().clone(), repo.store().empty_tree_id().clone());
    builder.set(
        path.to_owned(),
        TreeValue::File {
            id: missing_id,
            executable: false,
            copy_id: CopyId::placeholder(),
        },
    );
    let baseline = MergedTree::resolved(repo.store().clone(), builder.write_tree().block_on()?);
    testutils::write_working_copy_file(&fixture.scan_root, path, "replacement\n");
    let actual = fixture.snapshot(&baseline, &[path], &snapshot_options())?;
    assert_tree_eq!(actual, create_tree(repo, &[(path, "replacement\n")]));
    Ok(())
}

#[cfg(unix)]
#[test_case(false ; "file replacement")]
#[test_case(true ; "symlink replacement")]
fn exact_directory_replacement_deletes_only_sparse_children(symlink: bool) -> TestResult {
    let fixture = ExactSnapshot::new(TestRepoBackend::Test)?;
    let repo = &fixture.test_repo.repo;
    let boundary = repo_path("dir");
    let selected = repo_path("dir/selected");
    let omitted = repo_path("dir/omitted");
    let baseline = create_tree(
        repo,
        &[(selected, "old selected\n"), (omitted, "retained\n")],
    );
    let outside = fixture.test_repo.env.root().join("outside");
    std::fs::create_dir(&outside)?;
    testutils::write_working_copy_file(&outside, repo_path("selected"), "outside\n");
    if symlink {
        symlink_dir(&outside, boundary.to_fs_path(&fixture.scan_root)?)?;
    } else {
        testutils::write_working_copy_file(&fixture.scan_root, boundary, "replacement\n");
    }
    let options = SnapshotOptions {
        external_sparse_patterns: Some(vec![selected.to_owned()]),
        ..snapshot_options()
    };
    let actual = fixture.snapshot(&baseline, &[boundary, selected, omitted], &options)?;
    assert_tree_eq!(actual, create_tree(repo, &[(omitted, "retained\n")]));
    assert_eq!(std::fs::read(outside.join("selected"))?, b"outside\n");
    Ok(())
}

#[test]
fn conflicted_file_snapshot_preserves_long_marker_collision_handling() -> TestResult {
    let fixture = ExactSnapshot::new(TestRepoBackend::Test)?;
    let repo = &fixture.test_repo.repo;
    let path = repo_path("file");
    let base = create_tree(repo, &[(path, "base\n")]);
    let left = create_tree(
        repo,
        &[(path, "left contents\n<<<<<<<<<<<< literal left\n")],
    );
    let right = create_tree(repo, &[(path, "right\n>>>>>>>>>>>> literal right\n")]);
    let baseline = MergedTree::merge(Merge::from_vec(vec![
        (left, "left".into()),
        (base.clone(), "base".into()),
        (right.clone(), "right".into()),
    ]))
    .block_on()?;
    let MaterializedTreeValue::FileConflict(conflict) = conflicts::materialize_tree_value(
        repo.store(),
        path,
        baseline.path_value(path).block_on()?,
        baseline.labels(),
    )
    .block_on()?
    else {
        panic!("expected a file conflict");
    };
    assert!(
        conflicts::choose_materialized_conflict_marker_len(&conflict.contents)
            > conflicts::MIN_CONFLICT_MARKER_LEN
    );
    let contents = conflicts::materialize_merge_result_to_bytes(
        &conflict.contents,
        &conflict.labels,
        &ConflictMaterializeOptions {
            marker_style: ConflictMarkerStyle::Snapshot,
            marker_len: None,
            merge: repo.store().merge_options().clone(),
        },
    );
    let edited =
        String::from_utf8(contents.to_vec())?.replace("left contents\n", "edited left contents\n");
    testutils::write_working_copy_file(&fixture.scan_root, path, &edited);
    let actual = fixture.snapshot(&baseline, &[path], &snapshot_options())?;
    let left = create_tree(
        repo,
        &[(path, "edited left contents\n<<<<<<<<<<<< literal left\n")],
    );
    let expected = MergedTree::merge(Merge::from_vec(vec![
        (left, "left".into()),
        (base, "base".into()),
        (right, "right".into()),
    ]))
    .block_on()?;
    assert_tree_eq!(actual, expected);
    Ok(())
}
