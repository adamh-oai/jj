// Copyright 2020 The Jujutsu Authors
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

use std::convert::Infallible;
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(all(feature = "awacs", unix))]
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;

use assert_matches::assert_matches;
use bstr::BString;
use futures::AsyncReadExt as _;
use gix::odb::pack::FindExt as _;
use indoc::indoc;
use itertools::Itertools as _;
use jj_lib::backend::CopyId;
use jj_lib::backend::TreeId;
use jj_lib::backend::TreeValue;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::conflicts::ConflictMaterializeOptions;
use jj_lib::default_backend_factories::default_working_copy_factories;
use jj_lib::file_util;
use jj_lib::file_util::check_symlink_support;
use jj_lib::file_util::symlink_dir;
use jj_lib::file_util::symlink_file;
use jj_lib::files::FileMergeHunkLevel;
#[cfg(all(feature = "awacs", unix))]
use jj_lib::fsmonitor::AwacsConfig;
use jj_lib::fsmonitor::FsmonitorSettings;
use jj_lib::fsmonitor::WatchmanConfig;
use jj_lib::git::get_git_backend;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::local_working_copy::LocalWorkingCopy;
use jj_lib::local_working_copy::TreeState;
use jj_lib::local_working_copy::TreeStateSettings;
use jj_lib::local_working_copy::snapshot_mode_has_committed_baseline;
use jj_lib::matchers::FilesMatcher;
use jj_lib::merge::Merge;
use jj_lib::merge::SameChange;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::secret_backend::SecretBackend;
use jj_lib::tree_builder::TreeBuilder;
use jj_lib::tree_merge::MergeOptions;
use jj_lib::working_copy::CheckoutError;
use jj_lib::working_copy::CheckoutStats;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::working_copy::UntrackedReason;
use jj_lib::working_copy::WorkingCopy as _;
use jj_lib::workspace::Workspace;
use pollster::FutureExt as _;
use prost::Message as _;
use test_case::test_case;
use testutils::CommitBuilderExt as _;
use testutils::TestRepo;
use testutils::TestRepoBackend;
use testutils::TestResult;
use testutils::TestWorkspace;
use testutils::assert_tree_eq;
use testutils::commit_with_tree;
use testutils::create_tree;
use testutils::create_tree_with;
use testutils::empty_snapshot_options;
use testutils::repo_path;
use testutils::repo_path_buf;
use testutils::repo_path_component;
use testutils::write_random_commit;

fn check_icase_fs(dir: &Path) -> bool {
    let test_file = tempfile::Builder::new()
        .prefix("icase-")
        .tempfile_in(dir)
        .unwrap();
    let orig_name = test_file.path().file_name().unwrap().to_str().unwrap();
    let upper_name = orig_name.to_ascii_uppercase();
    assert_ne!(orig_name, upper_name);
    dir.join(upper_name).try_exists().unwrap()
}

/// Returns true if the directory appears to ignore some unicode zero-width
/// characters, as in HFS+.
fn check_hfs_plus(dir: &Path) -> bool {
    let test_file = tempfile::Builder::new()
        .prefix("hfs-plus-\u{200c}-")
        .tempfile_in(dir)
        .unwrap();
    let orig_name = test_file.path().file_name().unwrap().to_str().unwrap();
    let stripped_name = orig_name.replace('\u{200c}', "");
    assert_ne!(orig_name, stripped_name);
    dir.join(stripped_name).try_exists().unwrap()
}

#[cfg(all(feature = "awacs", unix))]
struct FakeAwacsSession {
    outcomes: Arc<Mutex<Vec<btrfs_awacs::scan::ScanOutcome>>>,
}

#[cfg(all(feature = "awacs", unix))]
impl btrfs_awacs::scan::ScanSession for FakeAwacsSession {
    fn renew(&mut self) -> Result<(), btrfs_awacs::scan::ScanError> {
        Ok(())
    }

    fn promote(&mut self) -> Result<(), btrfs_awacs::scan::ScanError> {
        Ok(())
    }

    fn finish(
        &mut self,
        outcome: btrfs_awacs::scan::ScanOutcome,
    ) -> Result<(), btrfs_awacs::scan::ScanError> {
        self.outcomes.lock().unwrap().push(outcome);
        Ok(())
    }
}

#[cfg(all(feature = "awacs", unix))]
struct FakeAwacsClient {
    scan_root: PathBuf,
    outcomes: Arc<Mutex<Vec<btrfs_awacs::scan::ScanOutcome>>>,
    requests: Arc<Mutex<Vec<Option<btrfs_awacs::scan::SnapshotBaseline>>>>,
    valid_scan_root: bool,
}

#[cfg(all(feature = "awacs", unix))]
impl btrfs_awacs::scan::ScanClient for FakeAwacsClient {
    fn begin_scan(
        &mut self,
        request: &btrfs_awacs::scan::BeginScanRequest,
    ) -> Result<btrfs_awacs::scan::SnapshotLease, btrfs_awacs::scan::ScanError> {
        self.requests
            .lock()
            .unwrap()
            .push(request.previous_baseline.clone());
        Ok(btrfs_awacs::scan::SnapshotLease::new(
            btrfs_awacs::scan::SnapshotBaseline {
                identity: btrfs_awacs::scan::SnapshotIdentity {
                    filesystem_uuid: [1; 16],
                    subvolume_uuid: [2; 16],
                    read_only: true,
                },
                continuity_token: b"baseline".to_vec(),
                retention_token: request.baseline_owner_id.to_vec(),
            },
            btrfs_awacs::scan::Invalidation::Prefixes(vec![b"dir".to_vec()]),
            u64::MAX,
            File::open(&self.scan_root).unwrap(),
            Box::new(FakeAwacsSession {
                outcomes: self.outcomes.clone(),
            }),
        ))
    }

    fn release_baseline(
        &mut self,
        _baseline_owner_id: [u8; 16],
    ) -> Result<(), btrfs_awacs::scan::ScanError> {
        Ok(())
    }

    fn validate_scan_root(
        &self,
        _lease: &btrfs_awacs::scan::SnapshotLease,
    ) -> Result<(), btrfs_awacs::scan::ScanError> {
        if self.valid_scan_root {
            Ok(())
        } else {
            Err(btrfs_awacs::scan::ScanError::new(
                btrfs_awacs::scan::ScanErrorKind::MalformedResponse,
                "rejected synthetic scan root",
            ))
        }
    }
}

/// Returns true if the directory appears to support Windows short file names.
fn check_vfat(dir: &Path) -> bool {
    let _test_file = tempfile::Builder::new()
        .prefix("vfattest-")
        .tempfile_in(dir)
        .unwrap();
    let short_name = "VFATTE~1";
    dir.join(short_name).try_exists().unwrap()
}

fn to_owned_path_vec(paths: &[&RepoPath]) -> Vec<RepoPathBuf> {
    paths.iter().map(|&path| path.to_owned()).collect()
}

fn write_legacy_tree_state(
    state_path: &Path,
    tree: &MergedTree,
    update: impl FnOnce(&mut jj_lib::protos::local_working_copy::TreeState),
) -> io::Result<()> {
    let mut proto = jj_lib::protos::local_working_copy::TreeState {
        tree_ids: tree.tree_ids().iter().map(|id| id.to_bytes()).collect(),
        conflict_labels: tree.labels().as_slice().to_owned(),
        sparse_patterns: Some(jj_lib::protos::local_working_copy::SparsePatterns {
            prefixes: vec![String::new()],
        }),
        ..Default::default()
    };
    update(&mut proto);
    std::fs::write(state_path.join("tree_state"), proto.encode_to_vec())
}

fn read_compact_working_copy_state(
    state_path: &Path,
) -> io::Result<jj_lib::protos::local_working_copy::WorkingCopyState> {
    const STATE_MAGIC: &[u8] = b"\0JJ-WORKING-COPY-STATE\0v1\n";
    let path = ["checkout", "working_copy_state"]
        .into_iter()
        .map(|name| state_path.join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "compact working-copy state is missing",
            )
        })?;
    let bytes = std::fs::read(path)?;
    let payload = bytes.strip_prefix(STATE_MAGIC).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "working-copy state has no compact-journal marker",
        )
    })?;
    jj_lib::protos::local_working_copy::WorkingCopyState::decode(payload)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn write_compact_working_copy_state(
    state_path: &Path,
    state: &jj_lib::protos::local_working_copy::WorkingCopyState,
) -> io::Result<()> {
    const STATE_MAGIC: &[u8] = b"\0JJ-WORKING-COPY-STATE\0v1\n";
    let mut bytes = STATE_MAGIC.to_vec();
    bytes.extend_from_slice(&state.encode_to_vec());
    std::fs::write(state_path.join("working_copy_state"), bytes)
}

fn enable_snapshot_mode(state_path: &Path) -> io::Result<()> {
    std::fs::write(state_path.join("subvolume_mode"), b"enabling\n")
}

fn seed_test_awacs_baseline(
    state_path: &Path,
    cursor: &[u8],
    input_fingerprint: [u8; 32],
) -> io::Result<()> {
    let mut journal = read_compact_working_copy_state(state_path)?;
    journal.phase = jj_lib::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline as i32;
    journal.baseline = Some(jj_lib::protos::local_working_copy::AwacsSnapshotBaseline {
        filesystem_uuid: vec![1; 16],
        subvolume_uuid: vec![2; 16],
        continuity_token: cursor.to_vec(),
        retention_token: cursor.to_vec(),
        interpretation_input_fingerprint: input_fingerprint.to_vec(),
    });
    write_compact_working_copy_state(state_path, &journal)
}

#[test]
fn test_root() -> TestResult {
    // Test that the working copy is clean and empty after init.
    let mut test_workspace = TestWorkspace::init();

    let wc = test_workspace.workspace.working_copy();
    assert_eq!(wc.sparse_patterns()?, vec![RepoPathBuf::root()]);
    let new_tree = test_workspace.snapshot()?;
    let repo = &test_workspace.repo;
    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(WorkspaceName::DEFAULT)
        .unwrap();
    let wc_commit = repo.store().get_commit(wc_commit_id)?;
    assert_tree_eq!(new_tree, wc_commit.tree());
    assert_tree_eq!(new_tree, repo.store().empty_merged_tree());
    Ok(())
}

#[test_case(TestRepoBackend::Simple ; "simple backend")]
#[test_case(TestRepoBackend::Git ; "git backend")]
fn test_checkout_file_transitions(backend: TestRepoBackend) -> TestResult {
    // Tests switching between commits where a certain path is of one type in one
    // commit and another type in the other. Includes a "missing" type, so we cover
    // additions and removals as well.

    let mut test_workspace = TestWorkspace::init_with_backend(backend);
    let repo = &test_workspace.repo;
    let store = repo.store().clone();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Kind {
        Missing,
        Normal,
        Executable,
        // Executable, but same content as Normal, to test transition where only the bit changed
        ExecutableNormalContent,
        Conflict,
        // Same content as Executable, to test that transition preserves the executable bit
        ConflictedExecutableContent,
        Symlink,
        Tree,
        GitSubmodule,
    }

    fn write_path(
        repo: &Arc<ReadonlyRepo>,
        tree_builder: &mut MergedTreeBuilder,
        kind: Kind,
        path: &RepoPath,
    ) {
        let store = repo.store();
        let copy_id = CopyId::placeholder();
        let value = match kind {
            Kind::Missing => Merge::absent(),
            Kind::Normal => {
                let id = testutils::write_file(store, path, "normal file contents");
                Merge::normal(TreeValue::File {
                    id,
                    executable: false,
                    copy_id,
                })
            }
            Kind::Executable => {
                let id: jj_lib::backend::FileId =
                    testutils::write_file(store, path, "executable file contents");
                Merge::normal(TreeValue::File {
                    id,
                    executable: true,
                    copy_id,
                })
            }
            Kind::ExecutableNormalContent => {
                let id = testutils::write_file(store, path, "normal file contents");
                Merge::normal(TreeValue::File {
                    id,
                    executable: true,
                    copy_id,
                })
            }
            Kind::Conflict => {
                let base_file_id = testutils::write_file(store, path, "base file contents");
                let left_file_id = testutils::write_file(store, path, "left file contents");
                let right_file_id = testutils::write_file(store, path, "right file contents");
                Merge::from_removes_adds(
                    vec![Some(TreeValue::File {
                        id: base_file_id,
                        executable: false,
                        copy_id: copy_id.clone(),
                    })],
                    vec![
                        Some(TreeValue::File {
                            id: left_file_id,
                            executable: false,
                            copy_id: copy_id.clone(),
                        }),
                        Some(TreeValue::File {
                            id: right_file_id,
                            executable: false,
                            copy_id: copy_id.clone(),
                        }),
                    ],
                )
            }
            Kind::ConflictedExecutableContent => {
                let base_file_id = testutils::write_file(store, path, "executable file contents");
                let left_file_id =
                    testutils::write_file(store, path, "left executable file contents");
                let right_file_id =
                    testutils::write_file(store, path, "right executable file contents");
                Merge::from_removes_adds(
                    vec![Some(TreeValue::File {
                        id: base_file_id,
                        executable: true,
                        copy_id: copy_id.clone(),
                    })],
                    vec![
                        Some(TreeValue::File {
                            id: left_file_id,
                            executable: true,
                            copy_id: copy_id.clone(),
                        }),
                        Some(TreeValue::File {
                            id: right_file_id,
                            executable: true,
                            copy_id: copy_id.clone(),
                        }),
                    ],
                )
            }
            Kind::Symlink => {
                let id = store.write_symlink(path, "target").block_on().unwrap();
                Merge::normal(TreeValue::Symlink(id))
            }
            Kind::Tree => {
                let file_path = path.join(repo_path_component("file"));
                let id = testutils::write_file(store, &file_path, "normal file contents");
                let value = TreeValue::File {
                    id,
                    executable: false,
                    copy_id: copy_id.clone(),
                };
                tree_builder.set_or_remove(file_path, Merge::normal(value));
                return;
            }
            Kind::GitSubmodule => {
                let mut tx = repo.start_transaction();
                let id = write_random_commit(tx.repo_mut()).id().clone();
                tx.commit("test").block_on().unwrap();
                Merge::normal(TreeValue::GitSubmodule(id))
            }
        };
        tree_builder.set_or_remove(path.to_owned(), value);
    }

    let mut kinds = vec![
        Kind::Missing,
        Kind::Normal,
        Kind::Executable,
        Kind::ExecutableNormalContent,
        Kind::Conflict,
        Kind::ConflictedExecutableContent,
        Kind::Tree,
    ];
    kinds.push(Kind::Symlink);
    if backend == TestRepoBackend::Git {
        kinds.push(Kind::GitSubmodule);
    }
    let mut left_tree_builder = MergedTreeBuilder::new(store.empty_merged_tree());
    let mut right_tree_builder = MergedTreeBuilder::new(store.empty_merged_tree());
    let mut files = vec![];
    for left_kind in &kinds {
        for right_kind in &kinds {
            let path = repo_path_buf(format!("{left_kind:?}_{right_kind:?}"));
            write_path(repo, &mut left_tree_builder, *left_kind, &path);
            write_path(repo, &mut right_tree_builder, *right_kind, &path);
            files.push((*left_kind, *right_kind, path.clone()));
        }
    }
    let left_tree = left_tree_builder.write_tree().block_on()?;
    let right_tree = right_tree_builder.write_tree().block_on()?;
    let left_commit = commit_with_tree(&store, left_tree);
    let right_commit = commit_with_tree(&store, right_tree.clone());

    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &left_commit)
        .block_on()?;
    ws.check_out(repo.op_id().clone(), None, &right_commit)
        .block_on()?;

    // Check that the working copy is clean.
    let new_tree = test_workspace.snapshot()?;
    assert_tree_eq!(new_tree, right_tree);

    for (_left_kind, right_kind, path) in &files {
        let wc_path = workspace_root.join(path.as_internal_file_string());
        let maybe_metadata = wc_path.symlink_metadata();
        match right_kind {
            Kind::Missing => {
                assert!(maybe_metadata.is_err(), "{path:?} should not exist");
            }
            Kind::Normal => {
                assert!(maybe_metadata.is_ok(), "{path:?} should exist");
                let metadata = maybe_metadata?;
                assert!(metadata.is_file(), "{path:?} should be a file");
                #[cfg(unix)]
                assert_eq!(
                    metadata.permissions().mode() & 0o111,
                    0,
                    "{path:?} should not be executable"
                );
            }
            Kind::Executable | Kind::ExecutableNormalContent => {
                assert!(maybe_metadata.is_ok(), "{path:?} should exist");
                let metadata = maybe_metadata?;
                assert!(metadata.is_file(), "{path:?} should be a file");
                #[cfg(unix)]
                assert_ne!(
                    metadata.permissions().mode() & 0o111,
                    0,
                    "{path:?} should be executable"
                );
            }
            Kind::Conflict => {
                assert!(maybe_metadata.is_ok(), "{path:?} should exist");
                let metadata = maybe_metadata?;
                assert!(metadata.is_file(), "{path:?} should be a file");
                #[cfg(unix)]
                assert_eq!(
                    metadata.permissions().mode() & 0o111,
                    0,
                    "{path:?} should not be executable"
                );
            }
            Kind::ConflictedExecutableContent => {
                assert!(maybe_metadata.is_ok(), "{path:?} should exist");
                let metadata = maybe_metadata?;
                assert!(metadata.is_file(), "{path:?} should be a file");
                #[cfg(unix)]
                assert_ne!(
                    metadata.permissions().mode() & 0o111,
                    0,
                    "{path:?} should be executable"
                );
            }
            Kind::Symlink => {
                assert!(maybe_metadata.is_ok(), "{path:?} should exist");
                let metadata = maybe_metadata?;
                if check_symlink_support().unwrap_or(false) {
                    assert!(
                        metadata.file_type().is_symlink(),
                        "{path:?} should be a symlink"
                    );
                }
            }
            Kind::Tree => {
                assert!(maybe_metadata.is_ok(), "{path:?} should exist");
                let metadata = maybe_metadata?;
                assert!(metadata.is_dir(), "{path:?} should be a directory");
            }
            Kind::GitSubmodule => {
                assert!(maybe_metadata.is_ok(), "{path:?} should exist");
                let metadata = maybe_metadata?;
                assert!(metadata.is_dir(), "{path:?} should be a directory");
            }
        }
    }
    Ok(())
}

#[test]
fn test_checkout_no_op() -> TestResult {
    // Check out another commit with the same tree that's already checked out. The
    // recorded operation should be updated even though the tree is unchanged.
    let mut test_workspace = TestWorkspace::init();
    let repo = test_workspace.repo.clone();

    let file_path = repo_path("file");

    let tree = create_tree(&repo, &[(file_path, "contents")]);
    let commit1 = commit_with_tree(repo.store(), tree.clone());
    let commit2 = commit_with_tree(repo.store(), tree);

    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;

    // Test the setup: the file should exist on disk and in the semantic tree.
    assert!(
        file_path
            .to_fs_path_unchecked(ws.workspace_root())
            .is_file()
    );
    let wc: &LocalWorkingCopy = ws.working_copy().downcast_ref().unwrap();
    assert_tree_eq!(*wc.tree()?, commit1.tree());

    // Update to commit2 (same tree as commit1)
    let new_op_id = OperationId::from_bytes(b"whatever");
    let stats = ws.check_out(new_op_id.clone(), None, &commit2).block_on()?;
    assert_eq!(stats, CheckoutStats::default());

    // The semantic tree is unchanged but the recorded operation id is updated.
    let wc: &LocalWorkingCopy = ws.working_copy().downcast_ref().unwrap();
    assert_tree_eq!(*wc.tree()?, commit2.tree());
    assert_eq!(*wc.operation_id(), new_op_id);
    Ok(())
}

// Test case for issue #2165
#[test]
fn test_conflict_subdirectory() -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;

    let path = repo_path("sub/file");
    let empty_tree = create_tree(repo, &[]);
    let tree1 = create_tree(repo, &[(path, "0")]);
    let commit1 = commit_with_tree(repo.store(), tree1.clone());
    let tree2 = create_tree(repo, &[(path, "1")]);
    let merged_tree = MergedTree::merge(Merge::from_vec(vec![
        (tree1, "tree 1".into()),
        (empty_tree, "empty".into()),
        (tree2, "tree 2".into()),
    ]))
    .block_on()?;
    let merged_commit = commit_with_tree(repo.store(), merged_tree);
    let repo = &test_workspace.repo;
    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;
    ws.check_out(repo.op_id().clone(), None, &merged_commit)
        .block_on()?;
    Ok(())
}

#[test]
fn test_acl() -> TestResult {
    let settings = testutils::user_settings();
    let test_workspace =
        TestWorkspace::init_with_backend_and_settings(TestRepoBackend::Git, &settings);
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let secret_modified_path = repo_path("secret/modified");
    let secret_added_path = repo_path("secret/added");
    let secret_deleted_path = repo_path("secret/deleted");
    let became_secret_path = repo_path("file1");
    let became_public_path = repo_path("file2");
    let tree1 = create_tree(
        repo,
        &[
            (secret_modified_path, "0"),
            (secret_deleted_path, "0"),
            (became_secret_path, "public"),
            (became_public_path, "secret"),
        ],
    );
    let tree2 = create_tree(
        repo,
        &[
            (secret_modified_path, "1"),
            (secret_added_path, "1"),
            (became_secret_path, "secret"),
            (became_public_path, "public"),
        ],
    );
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);
    SecretBackend::adopt_git_repo(&workspace_root);

    let mut ws = Workspace::load(
        &settings,
        &workspace_root,
        &test_workspace.env.default_backend_factories(),
        &default_working_copy_factories(),
    )?;
    // Reload commits from the store associated with the workspace
    let repo = ws.repo_loader().load_at(repo.operation()).block_on()?;
    let commit1 = repo.store().get_commit(commit1.id())?;
    let commit2 = repo.store().get_commit(commit2.id())?;

    ws.check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;
    assert!(
        !secret_modified_path
            .to_fs_path_unchecked(&workspace_root)
            .is_file()
    );
    assert!(
        !secret_added_path
            .to_fs_path_unchecked(&workspace_root)
            .is_file()
    );
    assert!(
        !secret_deleted_path
            .to_fs_path_unchecked(&workspace_root)
            .is_file()
    );
    assert!(
        became_secret_path
            .to_fs_path_unchecked(&workspace_root)
            .is_file()
    );
    assert!(
        !became_public_path
            .to_fs_path_unchecked(&workspace_root)
            .is_file()
    );
    ws.check_out(repo.op_id().clone(), None, &commit2)
        .block_on()?;
    assert!(
        !secret_modified_path
            .to_fs_path_unchecked(&workspace_root)
            .is_file()
    );
    assert!(
        !secret_added_path
            .to_fs_path_unchecked(&workspace_root)
            .is_file()
    );
    assert!(
        !secret_deleted_path
            .to_fs_path_unchecked(&workspace_root)
            .is_file()
    );
    assert!(
        !became_secret_path
            .to_fs_path_unchecked(&workspace_root)
            .is_file()
    );
    assert!(
        became_public_path
            .to_fs_path_unchecked(&workspace_root)
            .is_file()
    );
    Ok(())
}

#[test]
fn test_tree_builder_file_directory_transition() -> TestResult {
    let test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let store = repo.store();
    let mut ws = test_workspace.workspace;
    let workspace_root = ws.workspace_root().to_owned();
    let mut check_out_tree = |tree_id: &TreeId| {
        let tree = repo
            .store()
            .get_tree(RepoPathBuf::root(), tree_id)
            .block_on()
            .unwrap();
        let commit = commit_with_tree(
            repo.store(),
            MergedTree::resolved(repo.store().clone(), tree.id().clone()),
        );
        ws.check_out(repo.op_id().clone(), None, &commit)
            .block_on()
            .unwrap();
    };

    let parent_path = repo_path("foo/bar");
    let child_path = repo_path("foo/bar/baz");

    // Add file at parent_path
    let mut tree_builder = TreeBuilder::new(store.clone(), store.empty_tree_id().clone());
    tree_builder.set(
        parent_path.to_owned(),
        TreeValue::File {
            id: testutils::write_file(store, parent_path, ""),
            executable: false,
            copy_id: CopyId::placeholder(),
        },
    );
    let tree_id = tree_builder.write_tree().block_on()?;
    check_out_tree(&tree_id);
    assert!(parent_path.to_fs_path_unchecked(&workspace_root).is_file());
    assert!(!child_path.to_fs_path_unchecked(&workspace_root).exists());

    // Turn parent_path into directory, add file at child_path
    let mut tree_builder = TreeBuilder::new(store.clone(), tree_id);
    tree_builder.remove(parent_path.to_owned());
    tree_builder.set(
        child_path.to_owned(),
        TreeValue::File {
            id: testutils::write_file(store, child_path, ""),
            executable: false,
            copy_id: CopyId::placeholder(),
        },
    );
    let tree_id = tree_builder.write_tree().block_on()?;
    check_out_tree(&tree_id);
    assert!(parent_path.to_fs_path_unchecked(&workspace_root).is_dir());
    assert!(child_path.to_fs_path_unchecked(&workspace_root).is_file());

    // Turn parent_path back to file
    let mut tree_builder = TreeBuilder::new(store.clone(), tree_id);
    tree_builder.remove(child_path.to_owned());
    tree_builder.set(
        parent_path.to_owned(),
        TreeValue::File {
            id: testutils::write_file(store, parent_path, ""),
            executable: false,
            copy_id: CopyId::placeholder(),
        },
    );
    let tree_id = tree_builder.write_tree().block_on()?;
    check_out_tree(&tree_id);
    assert!(parent_path.to_fs_path_unchecked(&workspace_root).is_file());
    assert!(!child_path.to_fs_path_unchecked(&workspace_root).exists());
    Ok(())
}

#[test]
fn test_conflicting_changes_on_disk() -> TestResult {
    let test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let mut ws = test_workspace.workspace;
    let workspace_root = ws.workspace_root().to_owned();

    // file on disk conflicts with file in target commit
    let file_file_path = repo_path("file-file");
    // file on disk conflicts with directory in target commit
    let file_dir_path = repo_path("file-dir");
    // directory on disk conflicts with file in target commit
    let dir_file_path = repo_path("dir-file");
    let tree = create_tree(
        repo,
        &[
            (file_file_path, "committed contents"),
            (
                &file_dir_path.join(repo_path_component("file")),
                "committed contents",
            ),
            (dir_file_path, "committed contents"),
        ],
    );
    let commit = commit_with_tree(repo.store(), tree);

    std::fs::write(
        file_file_path.to_fs_path_unchecked(&workspace_root),
        "contents on disk",
    )?;
    std::fs::write(
        file_dir_path.to_fs_path_unchecked(&workspace_root),
        "contents on disk",
    )?;
    std::fs::create_dir(dir_file_path.to_fs_path_unchecked(&workspace_root))?;
    std::fs::write(
        dir_file_path
            .to_fs_path_unchecked(&workspace_root)
            .join("file"),
        "contents on disk",
    )?;

    let stats = ws
        .check_out(repo.op_id().clone(), None, &commit)
        .block_on()?;
    assert_eq!(
        stats,
        CheckoutStats {
            updated_files: 0,
            added_files: 3,
            removed_files: 0,
            skipped_files: 3
        }
    );

    assert_eq!(
        std::fs::read_to_string(file_file_path.to_fs_path_unchecked(&workspace_root)).ok(),
        Some("contents on disk".to_string())
    );
    assert_eq!(
        std::fs::read_to_string(file_dir_path.to_fs_path_unchecked(&workspace_root)).ok(),
        Some("contents on disk".to_string())
    );
    assert_eq!(
        std::fs::read_to_string(
            dir_file_path
                .to_fs_path_unchecked(&workspace_root)
                .join("file")
        )
        .ok(),
        Some("contents on disk".to_string())
    );
    Ok(())
}

#[test]
fn test_reset() -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let op_id = repo.op_id().clone();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let ignored_path = repo_path("ignored");
    let gitignore_path = repo_path(".gitignore");

    let tree_without_file = create_tree(repo, &[(gitignore_path, "ignored\n")]);
    let commit_without_file = commit_with_tree(repo.store(), tree_without_file.clone());
    let tree_with_file = create_tree(
        repo,
        &[(gitignore_path, "ignored\n"), (ignored_path, "code")],
    );
    let commit_with_file = commit_with_tree(repo.store(), tree_with_file.clone());

    let ws = &mut test_workspace.workspace;
    let commit = commit_with_tree(repo.store(), tree_with_file.clone());
    ws.check_out(repo.op_id().clone(), None, &commit)
        .block_on()?;

    // Test the setup: the file should exist on disk and in the semantic tree.
    assert!(ignored_path.to_fs_path_unchecked(&workspace_root).is_file());
    let wc: &LocalWorkingCopy = ws.working_copy().downcast_ref().unwrap();
    assert_tree_eq!(*wc.tree()?, tree_with_file);

    // After we reset to the commit without the file, it should still exist on disk,
    // but it should not be in the semantic tree, and it should not get added
    // when we commit the working copy (because it's ignored).
    let mut locked_ws = ws.start_working_copy_mutation().block_on()?;
    locked_ws
        .locked_wc()
        .reset(&commit_without_file)
        .block_on()?;
    locked_ws.finish(op_id.clone()).block_on()?;
    assert!(ignored_path.to_fs_path_unchecked(&workspace_root).is_file());
    let wc: &LocalWorkingCopy = ws.working_copy().downcast_ref().unwrap();
    assert_tree_eq!(*wc.tree()?, tree_without_file);
    let new_tree = test_workspace.snapshot()?;
    assert_tree_eq!(new_tree, tree_without_file);

    // Now test the opposite direction: resetting to a commit where the file is
    // tracked. The file should become tracked (even though it's ignored).
    let ws = &mut test_workspace.workspace;
    let mut locked_ws = ws.start_working_copy_mutation().block_on()?;
    locked_ws.locked_wc().reset(&commit_with_file).block_on()?;
    locked_ws.finish(op_id.clone()).block_on()?;
    assert!(ignored_path.to_fs_path_unchecked(&workspace_root).is_file());
    let wc: &LocalWorkingCopy = ws.working_copy().downcast_ref().unwrap();
    assert_tree_eq!(*wc.tree()?, tree_with_file);
    let new_tree = test_workspace.snapshot()?;
    assert_tree_eq!(new_tree, tree_with_file);
    Ok(())
}

#[test]
fn test_checkout_discard() -> TestResult {
    // Start a mutation, do a checkout, and then discard the mutation. The working
    // copy files should remain changed, but the state files should not be
    // written.
    let mut test_workspace = TestWorkspace::init();
    let repo = test_workspace.repo.clone();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let file1_path = repo_path("file1");
    let file2_path = repo_path("file2");

    let store = repo.store();
    let tree1 = create_tree(&repo, &[(file1_path, "contents")]);
    let tree2 = create_tree(&repo, &[(file2_path, "contents")]);
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);

    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;
    let wc: &LocalWorkingCopy = ws.working_copy().downcast_ref().unwrap();
    let state_path = wc.state_path().to_path_buf();

    // Test the setup: the file should exist on disk and in the semantic tree.
    assert!(file1_path.to_fs_path_unchecked(&workspace_root).is_file());
    let wc: &LocalWorkingCopy = ws.working_copy().downcast_ref().unwrap();
    assert_tree_eq!(*wc.tree()?, commit1.tree());

    // Start a checkout
    let mut locked_ws = ws.start_working_copy_mutation().block_on()?;
    locked_ws.locked_wc().check_out(&commit2).block_on()?;
    // The live files change immediately, but the legacy tree-state path does
    // not publish a new semantic tree until the mutation is finished.
    assert!(!file1_path.to_fs_path_unchecked(&workspace_root).is_file());
    assert!(file2_path.to_fs_path_unchecked(&workspace_root).is_file());
    let reloaded_wc = LocalWorkingCopy::load(
        store.clone(),
        workspace_root.clone(),
        state_path.clone(),
        repo.settings(),
    )?;
    assert_tree_eq!(*reloaded_wc.tree()?, commit1.tree());
    drop(locked_ws);

    // Discarding the mutation leaves the legacy on-disk state unchanged.
    let wc: &LocalWorkingCopy = ws.working_copy().downcast_ref().unwrap();
    assert_tree_eq!(*wc.tree()?, commit1.tree());
    assert!(!file1_path.to_fs_path_unchecked(&workspace_root).is_file());
    assert!(file2_path.to_fs_path_unchecked(&workspace_root).is_file());
    let reloaded_wc =
        LocalWorkingCopy::load(store.clone(), workspace_root, state_path, repo.settings())?;
    assert_tree_eq!(*reloaded_wc.tree()?, commit1.tree());
    Ok(())
}

#[test]
fn test_snapshot_file_directory_transition() -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = test_workspace.repo.clone();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    let to_ws_path = |path: &RepoPath| path.to_fs_path(&workspace_root).unwrap();

    // file <-> directory transition at root and sub directories
    let file1_path = repo_path("foo/bar");
    let file2_path = repo_path("sub/bar/baz");
    let file1p_path = file1_path.parent().unwrap();
    let file2p_path = file2_path.parent().unwrap();

    let tree1 = create_tree(&repo, &[(file1p_path, "1p"), (file2p_path, "2p")]);
    let tree2 = create_tree(&repo, &[(file1_path, "1"), (file2_path, "2")]);
    let commit1 = commit_with_tree(repo.store(), tree1.clone());
    let commit2 = commit_with_tree(repo.store(), tree2.clone());

    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;

    // file -> directory
    std::fs::remove_file(to_ws_path(file1p_path))?;
    std::fs::remove_file(to_ws_path(file2p_path))?;
    std::fs::create_dir(to_ws_path(file1p_path))?;
    std::fs::create_dir(to_ws_path(file2p_path))?;
    std::fs::write(to_ws_path(file1_path), "1")?;
    std::fs::write(to_ws_path(file2_path), "2")?;
    let new_tree = test_workspace.snapshot()?;
    assert_tree_eq!(new_tree, tree2);

    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit2)
        .block_on()?;

    // directory -> file
    std::fs::remove_file(to_ws_path(file1_path))?;
    std::fs::remove_file(to_ws_path(file2_path))?;
    std::fs::remove_dir(to_ws_path(file1p_path))?;
    std::fs::remove_dir(to_ws_path(file2p_path))?;
    std::fs::write(to_ws_path(file1p_path), "1p")?;
    std::fs::write(to_ws_path(file2p_path), "2p")?;
    let new_tree = test_workspace.snapshot()?;
    assert_tree_eq!(new_tree, tree1);
    Ok(())
}

#[test]
fn test_materialize_snapshot_conflicted_files() -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo.clone();
    let ws = &mut test_workspace.workspace;
    let workspace_root = ws.workspace_root().to_owned();

    // Create tree with 3-sided conflict, with file1 and file2 having different
    // conflicts:
    // file1: A - A + A - B + C
    // file2: A - B + C - D + D
    let file1_path = repo_path("file1");
    let file2_path = repo_path("file2");
    let side1_tree = create_tree(repo, &[(file1_path, "a\n"), (file2_path, "1\n")]);
    let base1_tree = create_tree(repo, &[(file1_path, "a\n"), (file2_path, "2\n")]);
    let side2_tree = create_tree(repo, &[(file1_path, "a\n"), (file2_path, "4\n")]);
    let base2_tree = create_tree(repo, &[(file1_path, "b\n"), (file2_path, "3\n")]);
    let side3_tree = create_tree(repo, &[(file1_path, "c\n"), (file2_path, "3\n")]);
    let merged_tree = MergedTree::merge(Merge::from_vec(vec![
        (side1_tree, "side 1".into()),
        (base1_tree, "base 1".into()),
        (side2_tree, "side 2".into()),
        (base2_tree, "base 2".into()),
        (side3_tree, "side 3".into()),
    ]))
    .block_on()?;
    let commit = commit_with_tree(repo.store(), merged_tree.clone());

    let stats = ws
        .check_out(repo.op_id().clone(), None, &commit)
        .block_on()?;
    assert_eq!(
        stats,
        CheckoutStats {
            updated_files: 0,
            added_files: 2,
            removed_files: 0,
            skipped_files: 0
        }
    );

    // Even though the tree-level conflict is a 3-sided conflict, each file is
    // materialized as a 2-sided conflict.
    let file1_value = merged_tree.path_value(file1_path).block_on()?;
    let file2_value = merged_tree.path_value(file2_path).block_on()?;
    assert_eq!(file1_value.num_sides(), 3);
    assert_eq!(file2_value.num_sides(), 3);
    insta::assert_snapshot!(
        std::fs::read_to_string(file1_path.to_fs_path_unchecked(&workspace_root)).ok().unwrap(),
        @r"
    <<<<<<< conflict 1 of 1
    %%%%%%% diff from: base 2
    \\\\\\\        to: side 2
    -b
    +a
    +++++++ side 3
    c
    >>>>>>> conflict 1 of 1 ends
    ");
    insta::assert_snapshot!(
        std::fs::read_to_string(file2_path.to_fs_path_unchecked(&workspace_root)).ok().unwrap(),
        @r"
    <<<<<<< conflict 1 of 1
    %%%%%%% diff from: base 1
    \\\\\\\        to: side 1
    -2
    +1
    +++++++ side 2
    4
    >>>>>>> conflict 1 of 1 ends
    ");

    // Editing a conflicted file should correctly propagate updates to each of
    // the conflicting trees.
    testutils::write_working_copy_file(
        &workspace_root,
        file1_path,
        indoc! {"
            <<<<<<< conflict 1 of 1
            %%%%%%% diff from base to side #1
            -b_edited
            +a_edited
            +++++++ side #2
            c_edited
            >>>>>>> conflict 1 of 1 ends
        "},
    );

    let edited_tree = test_workspace.snapshot()?;
    let edited_file_value = edited_tree.path_value(file1_path).block_on()?;
    let edited_file_values = edited_file_value.iter().flatten().collect_vec();
    assert_eq!(edited_file_values.len(), 5);

    let get_file_id = |value: &TreeValue| match value {
        TreeValue::File { id, .. } => id.clone(),
        _ => panic!("unexpected value: {value:#?}"),
    };
    // The file IDs with indices 0 and 1 are the original unedited file values
    // which were simplified.
    let edited_file_file_id_0 = get_file_id(edited_file_values[0]);
    assert_eq!(
        testutils::read_file(repo.store(), file1_path, &edited_file_file_id_0),
        b"a\n"
    );
    assert_eq!(edited_file_values[0], edited_file_values[1]);
    let edited_file_file_id_2 = get_file_id(edited_file_values[2]);
    assert_eq!(
        testutils::read_file(repo.store(), file1_path, &edited_file_file_id_2),
        b"a_edited\n"
    );
    let edited_file_file_id_3 = get_file_id(edited_file_values[3]);
    assert_eq!(
        testutils::read_file(repo.store(), file1_path, &edited_file_file_id_3),
        b"b_edited\n"
    );
    let edited_file_file_id_4 = get_file_id(edited_file_values[4]);
    assert_eq!(
        testutils::read_file(repo.store(), file1_path, &edited_file_file_id_4),
        b"c_edited\n"
    );
    Ok(())
}

#[test]
fn test_materialize_snapshot_unchanged_conflicts() -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    // Both sides change "line 3" differently, right side deletes "line 5".
    let base_content = indoc! {"
        line 1
        line 2
        line 3
        line 4
        line 5
    "};
    let left_content = indoc! {"
        line 1
        line 2
        left 3.1
        left 3.2
        left 3.3
        line 4
        line 5
    "};
    let right_content = indoc! {"
        line 1
        line 2
        right 3.1
        line 4
    "};
    let file_path = repo_path("file");
    let base_tree = create_tree(repo, &[(file_path, base_content)]);
    let left_tree = create_tree(repo, &[(file_path, left_content)]);
    let right_tree = create_tree(repo, &[(file_path, right_content)]);
    let merged_tree = MergedTree::merge(Merge::from_vec(vec![
        (left_tree, "left".into()),
        (base_tree, "base".into()),
        (right_tree, "right".into()),
    ]))
    .block_on()?;
    let commit = commit_with_tree(repo.store(), merged_tree.clone());

    test_workspace
        .workspace
        .check_out(repo.op_id().clone(), None, &commit)
        .block_on()?;

    // "line 5" should be deleted from the checked-out content.
    let disk_path = file_path.to_fs_path_unchecked(&workspace_root);
    let materialized_content = std::fs::read_to_string(&disk_path)?;
    insta::assert_snapshot!(materialized_content, @r"
    line 1
    line 2
    <<<<<<< conflict 1 of 1
    +++++++ left
    left 3.1
    left 3.2
    left 3.3
    %%%%%%% diff from: base
    \\\\\\\        to: right
    -line 3
    +right 3.1
    >>>>>>> conflict 1 of 1 ends
    line 4
    ");

    let merged_tree_with_labels = MergedTree::new(
        merged_tree.store().clone(),
        merged_tree.tree_ids().clone(),
        ConflictLabels::from_vec(vec![
            "left label".into(),
            "base label".into(),
            "right label".into(),
        ]),
    );
    let commit_with_labels = commit_with_tree(repo.store(), merged_tree_with_labels.clone());

    // When checking out a commit with the same conflicts but different labels, the
    // file should still be updated.
    let stats = test_workspace
        .workspace
        .check_out(repo.op_id().clone(), None, &commit_with_labels)
        .block_on()?;
    assert_eq!(
        stats,
        CheckoutStats {
            updated_files: 1,
            ..CheckoutStats::default()
        }
    );
    let materialized_content = std::fs::read_to_string(&disk_path)?;
    insta::assert_snapshot!(materialized_content, @r"
    line 1
    line 2
    <<<<<<< conflict 1 of 1
    +++++++ left label
    left 3.1
    left 3.2
    left 3.3
    %%%%%%% diff from: base label
    \\\\\\\        to: right label
    -line 3
    +right 3.1
    >>>>>>> conflict 1 of 1 ends
    line 4
    ");

    // Update mtime to bypass file state comparison.
    let file = File::options().write(true).open(&disk_path)?;
    file.set_modified(SystemTime::now() + Duration::from_secs(1))?;
    drop(file);

    // Unchanged snapshot should be identical to the original even if "line 5"
    // could be deleted from all sides.
    let snapshotted_tree = test_workspace.snapshot()?;
    assert_tree_eq!(snapshotted_tree, merged_tree_with_labels);
    Ok(())
}

struct SnapshotModifiedMaterializedConflictTestConfig {
    base_contents: Option<&'static str>,
    parent1_contents: Option<&'static str>,
    parent2_contents: Option<&'static str>,
    // Edit the conflict contents of the conflict file. The input is the parsed hunks of the
    // conflict file. The contents of the conflict file will be replaced by the hunks returned.
    get_new_merge_hunks: fn(Vec<Merge<BString>>) -> Vec<Merge<BString>>,

    expected_file_contents: Merge<Option<&'static str>>,
}

#[test_case(SnapshotModifiedMaterializedConflictTestConfig {
    base_contents: None,
    parent1_contents: Some("parent1\n"),
    parent2_contents: Some("parent2\n"),
    get_new_merge_hunks: |mut hunks: Vec<Merge<BString>>| {
        for (i, side) in hunks[0].iter_mut().enumerate() {
            if i == 0 {
                side.extend(b"appended\n");
            }
        }
        hunks
    },
    expected_file_contents: Merge::from_vec(vec![
        Some("parent1\nappended\n"),
        None,
        Some("parent2\n"),
    ]),
}; "no base contents parent 1 appended")]
#[test_case(SnapshotModifiedMaterializedConflictTestConfig {
    base_contents: None,
    parent1_contents: Some("parent1\n"),
    parent2_contents: Some("parent2\n"),
    get_new_merge_hunks: |mut hunks: Vec<Merge<BString>>| {
        for (i, side) in hunks[0].iter_mut().enumerate() {
            if i == 2 {
                side.extend(b"appended\n");
            }
        }
        hunks
    },
    expected_file_contents: Merge::from_vec(vec![
        Some("parent1\n"),
        None,
        Some("parent2\nappended\n"),
    ]),
}; "no base contents parent 2 appended")]
#[test_case(SnapshotModifiedMaterializedConflictTestConfig {
    base_contents: None,
    parent1_contents: Some("parent1\n"),
    parent2_contents: Some("parent2\n"),
    get_new_merge_hunks: |mut hunks: Vec<Merge<BString>>| {
        for (i, side) in hunks[0].iter_mut().enumerate() {
            if i == 0 || i == 2 {
                side.extend(b"appended\n");
            }
        }
        hunks
    },
    expected_file_contents: Merge::from_vec(vec![
        Some("parent1\nappended\n"),
        None,
        Some("parent2\nappended\n"),
    ]),
}; "no base contents both parents appended")]
#[test_case(SnapshotModifiedMaterializedConflictTestConfig {
    base_contents: None,
    parent1_contents: Some("parent1\n"),
    parent2_contents: Some("parent2\n"),
    get_new_merge_hunks: |mut hunks: Vec<Merge<BString>>| {
        hunks.push(Merge::resolved(BString::from("appended\n")));
        hunks
    },
    expected_file_contents: Merge::from_vec(vec![
        Some("parent1\nappended\n"),
        // The file in the base change is also modified to preserve the materialized conflict.
        Some("appended\n"),
        Some("parent2\nappended\n"),
    ]),
}; "no base contents a new resolved hunk appended")]
#[test_case(SnapshotModifiedMaterializedConflictTestConfig {
    base_contents: None,
    parent1_contents: Some("parent1\n"),
    parent2_contents: Some("parent2\n"),
    get_new_merge_hunks: |mut hunks: Vec<Merge<BString>>| {
        for side in &mut hunks[0] {
            side.extend(b"appended\n");
        }
        hunks
    },
    expected_file_contents: Merge::from_vec(vec![
        Some("parent1\nappended\n"),
        // The file in the base change is also modified to preserve the materialized conflict.
        Some("appended\n"),
        Some("parent2\nappended\n"),
    ]),
}; "no base contents all sides of the existing hunk appended")]
#[test_case(SnapshotModifiedMaterializedConflictTestConfig {
    base_contents: None,
    parent1_contents: Some("parent1\n"),
    parent2_contents: Some("parent2\n"),
    get_new_merge_hunks: |mut hunks: Vec<Merge<BString>>| {
        hunks[0].iter_mut().nth(1).unwrap().extend(b"new base\n");
        hunks
    },
    // If the user adds contents to the absent side of a conflict hunk, we consider the conflict resolved.
    expected_file_contents: Merge::from_vec(vec![
        Some("parent1\n"),
        // The file in the base change is modified to preserve the materialized conflict.
        Some("new base\n"),
        Some("parent2\n"),
    ]),
}; "no base contents base side appended only")]
#[test_case(SnapshotModifiedMaterializedConflictTestConfig {
    base_contents: None,
    parent1_contents: Some("parent1\n"),
    parent2_contents: Some("parent2\n"),
    get_new_merge_hunks: |mut hunks: Vec<Merge<BString>>| {
        hunks.insert(0, Merge::resolved(BString::from("prepended\n")));
        hunks
    },
    expected_file_contents: Merge::from_vec(vec![
        Some("prepended\nparent1\n"),
        // The file in the base change is also modified to preserve the materialized conflict.
        Some("prepended\n"),
        Some("prepended\nparent2\n"),
    ]),
}; "no base contents a new resolved hunk prepended")]
#[test_case(SnapshotModifiedMaterializedConflictTestConfig {
    base_contents: Some("base\n"),
    parent1_contents: None,
    parent2_contents: Some("parent2\n"),
    get_new_merge_hunks: |mut hunks: Vec<Merge<BString>>| {
        hunks.push(Merge::resolved(BString::from("appended\n")));
        hunks
    },
    expected_file_contents: Merge::from_vec(vec![
        // The file in the parent1 change is also modified to preserve the materialized conflict.
        Some("appended\n"),
        Some("base\nappended\n"),
        Some("parent2\nappended\n"),
    ]),
}; "file removed in parent1 a resolved hunk appended in merge")]
#[test_case(SnapshotModifiedMaterializedConflictTestConfig {
    base_contents: Some("base\n"),
    parent1_contents: Some("parent1\n"),
    parent2_contents: None,
    get_new_merge_hunks: |mut hunks: Vec<Merge<BString>>| {
        hunks.push(Merge::resolved(BString::from("appended\n")));
        hunks
    },
    expected_file_contents: Merge::from_vec(vec![
        Some("parent1\nappended\n"),
        Some("base\nappended\n"),
        // The file in the parent2 change is also modified to preserve the materialized conflict.
        Some("appended\n"),
    ]),
}; "file removed in parent2 a resolved hunk appended in merge")]
fn test_snapshot_modified_materialized_conflict(
    SnapshotModifiedMaterializedConflictTestConfig {
        base_contents,
        parent1_contents,
        parent2_contents,
        get_new_merge_hunks,
        expected_file_contents,
    }: SnapshotModifiedMaterializedConflictTestConfig,
) -> TestResult {
    // In this test, we create the following commits, checkout the merge commit,
    // modify the merge contents, snapshot, and verify if the new merged tree is
    // correct.

    // D
    // |\
    // B C
    // |/
    // A

    // We can't use the tokio runtime here because the test backend will create
    // the tokio runtime in TestWorkspace::init, and tokio will panic if the
    // tokio runtime is dropped in an async context. See
    // https://docs.rs/tokio/1.47.1/tokio/runtime/struct.Handle.html#panics-2
    // for details.
    let mut test_workspace = TestWorkspace::init();
    let file_repo_path = repo_path("test-file");
    let file_disk_path = file_repo_path.to_fs_path(test_workspace.workspace.workspace_root())?;

    // Create the commits with given contents.
    let mut tx = test_workspace.repo.start_transaction();
    let tree = create_tree(
        &test_workspace.repo,
        base_contents
            .map(|contents| (file_repo_path, contents))
            .as_slice(),
    );
    let base_commit = tx
        .repo_mut()
        .new_commit(
            vec![test_workspace.repo.store().root_commit_id().clone()],
            tree,
        )
        .write_unwrap();
    let tree = create_tree(
        &test_workspace.repo,
        &parent1_contents
            .map(|contents| (file_repo_path, contents))
            .into_iter()
            .collect::<Vec<_>>(),
    );
    let parent1_commit = tx
        .repo_mut()
        .new_commit(vec![base_commit.id().clone()], tree)
        .write_unwrap();
    let tree = create_tree(
        &test_workspace.repo,
        &parent2_contents
            .map(|contents| (file_repo_path, contents))
            .into_iter()
            .collect::<Vec<_>>(),
    );
    let parent2_commit = tx
        .repo_mut()
        .new_commit(vec![base_commit.id().clone()], tree)
        .write_unwrap();
    // Update the repo to pick up the new commits.
    test_workspace.repo = tx.commit("create parent commits").block_on()?;

    // Create the merge commit.
    let tree =
        merge_commit_trees(&*test_workspace.repo, &[parent1_commit, parent2_commit]).block_on()?;
    let merge_commit = commit_with_tree(test_workspace.repo.store(), tree);

    // Checkout the merge commit.
    test_workspace
        .workspace
        .check_out(test_workspace.repo.op_id().clone(), None, &merge_commit)
        .block_on()?;
    let contents = std::fs::read(&file_disk_path)?;
    let hunks =
        jj_lib::conflicts::parse_conflict(&contents, 2, jj_lib::conflicts::MIN_CONFLICT_MARKER_LEN)
            .unwrap();
    let hunks = get_new_merge_hunks(hunks);
    let mut new_contents = vec![];
    for hunk in hunks {
        jj_lib::conflicts::materialize_merge_result(
            &hunk,
            &ConflictLabels::unlabeled(),
            &mut new_contents,
            &ConflictMaterializeOptions {
                marker_style: jj_lib::conflicts::ConflictMarkerStyle::Diff,
                marker_len: None,
                merge: MergeOptions {
                    hunk_level: FileMergeHunkLevel::Line,
                    same_change: SameChange::Accept,
                },
            },
        )?;
    }
    std::fs::write(&file_disk_path, new_contents)?;

    // Snapshot.
    let tree = test_workspace.snapshot()?;
    let actual_file_contents = tree
        .path_value(file_repo_path)
        .block_on()?
        .try_map_async(async |tree_value| {
            let Some(tree_value) = tree_value else {
                return Ok::<_, Infallible>(None);
            };
            let TreeValue::File { id, .. } = tree_value else {
                panic!("All sides of the conflict should be either a file or absent.");
            };
            let mut contents = vec![];
            test_workspace
                .repo
                .store()
                .read_file(file_repo_path, id)
                .await
                .unwrap()
                .read_to_end(&mut contents)
                .await
                .unwrap();
            Ok::<_, Infallible>(Some(String::from_utf8(contents).unwrap()))
        })
        .block_on()?;
    let expected_file_contents =
        expected_file_contents.map(|contents| contents.as_deref().map(str::to_string));
    assert_eq!(actual_file_contents, expected_file_contents);
    Ok(())
}

#[test]
fn test_snapshot_racy_timestamps() -> TestResult {
    // Tests that file modifications are detected even if they happen the same
    // millisecond as the updated working copy state.
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let file_path = workspace_root.join("file");
    let mut previous_tree = repo.store().empty_merged_tree();
    for i in 0..100 {
        std::fs::write(&file_path, format!("contents {i}").as_bytes())?;
        let mut locked_ws = test_workspace
            .workspace
            .start_working_copy_mutation()
            .block_on()?;
        let (new_tree, _stats) = locked_ws
            .locked_wc()
            .snapshot(&empty_snapshot_options())
            .block_on()?;
        assert_ne!(new_tree.tree_ids(), previous_tree.tree_ids());
        previous_tree = new_tree;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_snapshot_special_file() -> TestResult {
    // Tests that we ignore when special files (such as sockets and pipes) exist on
    // disk.
    let mut test_workspace = TestWorkspace::init();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    let ws = &mut test_workspace.workspace;

    let file1_path = repo_path("file1");
    let file1_disk_path = file1_path.to_fs_path_unchecked(&workspace_root);
    std::fs::write(&file1_disk_path, "contents".as_bytes())?;
    let file2_path = repo_path("file2");
    let file2_disk_path = file2_path.to_fs_path_unchecked(&workspace_root);
    std::fs::write(file2_disk_path, "contents".as_bytes())?;

    let fifo_disk_path = workspace_root.join("fifo");
    nix::unistd::mkfifo(&fifo_disk_path, nix::sys::stat::Mode::S_IRWXU)?;
    assert!(fifo_disk_path.exists());
    assert!(!fifo_disk_path.is_file());

    // Snapshot the working copy with the socket file
    let mut locked_ws = ws.start_working_copy_mutation().block_on()?;
    let (tree, _stats) = locked_ws
        .locked_wc()
        .snapshot(&empty_snapshot_options())
        .block_on()?;
    locked_ws
        .finish(OperationId::from_hex("abc123"))
        .block_on()?;
    // Only the regular files should be in the tree
    assert_eq!(
        tree.entries().map(|(path, _value)| path).collect_vec(),
        to_owned_path_vec(&[file1_path, file2_path])
    );
    // Replace a regular file by a socket and snapshot the working copy again
    std::fs::remove_file(&file1_disk_path)?;
    nix::unistd::mkfifo(&file1_disk_path, nix::sys::stat::Mode::S_IRWXU)?;
    let tree = test_workspace.snapshot()?;
    // Only the regular file should be in the tree
    assert_eq!(
        tree.entries().map(|(path, _value)| path).collect_vec(),
        to_owned_path_vec(&[file2_path])
    );
    Ok(())
}

#[test]
fn test_gitignores() -> TestResult {
    // Tests that .gitignore files are respected.

    let mut test_workspace = TestWorkspace::init();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let gitignore_path = repo_path(".gitignore");
    let added_path = repo_path("added");
    let modified_path = repo_path("modified");
    let removed_path = repo_path("removed");
    let ignored_path = repo_path("ignored");
    let subdir_modified_path = repo_path("dir/modified");
    let subdir_ignored_path = repo_path("dir/ignored");

    testutils::write_working_copy_file(&workspace_root, gitignore_path, "ignored\n");
    testutils::write_working_copy_file(&workspace_root, modified_path, "1");
    testutils::write_working_copy_file(&workspace_root, removed_path, "1");
    std::fs::create_dir(workspace_root.join("dir"))?;
    testutils::write_working_copy_file(&workspace_root, subdir_modified_path, "1");

    let tree1 = test_workspace.snapshot()?;
    let files1 = tree1.entries().map(|(name, _value)| name).collect_vec();
    assert_eq!(
        files1,
        to_owned_path_vec(&[
            gitignore_path,
            subdir_modified_path,
            modified_path,
            removed_path,
        ])
    );

    testutils::write_working_copy_file(
        &workspace_root,
        gitignore_path,
        "ignored\nmodified\nremoved\n",
    );
    testutils::write_working_copy_file(&workspace_root, added_path, "2");
    testutils::write_working_copy_file(&workspace_root, modified_path, "2");
    std::fs::remove_file(removed_path.to_fs_path_unchecked(&workspace_root))?;
    testutils::write_working_copy_file(&workspace_root, ignored_path, "2");
    testutils::write_working_copy_file(&workspace_root, subdir_modified_path, "2");
    testutils::write_working_copy_file(&workspace_root, subdir_ignored_path, "2");

    let tree2 = test_workspace.snapshot()?;
    let files2 = tree2.entries().map(|(name, _value)| name).collect_vec();
    assert_eq!(
        files2,
        to_owned_path_vec(&[
            gitignore_path,
            added_path,
            subdir_modified_path,
            modified_path,
        ])
    );
    Ok(())
}

#[test]
fn test_gitignores_walk() -> TestResult {
    // Tests that .gitignore files are respected.

    let mut test_workspace = TestWorkspace::init();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let gitignore_path = repo_path(".gitignore");
    let ignored_path = repo_path("ignore_dir/file");
    let subdir_not_ignored_path = repo_path("subdir/ignore_dir/file");

    let nested_gitignore_path = repo_path("nested/dir/.gitignore");
    let nested_ignored_path = repo_path("nested/dir/ignored");
    let also_ignored_path = repo_path("nested/dir/also_ignored");
    let nested_path = repo_path("nested/dir/ignore_dir");

    testutils::write_working_copy_file(
        &workspace_root,
        gitignore_path,
        "/ignore_dir\n/**/also_ignored",
    );
    testutils::write_working_copy_file(&workspace_root, ignored_path, "1");
    testutils::write_working_copy_file(&workspace_root, subdir_not_ignored_path, "1");

    testutils::write_working_copy_file(&workspace_root, nested_gitignore_path, "/ignored\n");
    testutils::write_working_copy_file(&workspace_root, nested_ignored_path, "2");
    testutils::write_working_copy_file(&workspace_root, also_ignored_path, "2");
    testutils::write_working_copy_file(&workspace_root, nested_path, "2");

    let tree1 = test_workspace.snapshot()?;
    let files1 = tree1.entries().map(|(name, _value)| name).collect_vec();
    assert_eq!(
        files1,
        to_owned_path_vec(&[
            gitignore_path,
            nested_gitignore_path,
            nested_path,
            subdir_not_ignored_path,
        ])
    );
    Ok(())
}

#[test]
fn test_gitignores_in_ignored_dir() -> TestResult {
    // Tests that .gitignore files in an ignored directory are ignored, i.e. that
    // they cannot override the ignores from the parent

    let mut test_workspace = TestWorkspace::init();
    let op_id = test_workspace.repo.op_id().clone();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let gitignore_path = repo_path(".gitignore");
    let nested_gitignore_path = repo_path("ignored/.gitignore");
    let ignored_path = repo_path("ignored/file");

    let tree1 = create_tree(&test_workspace.repo, &[(gitignore_path, "ignored\n")]);
    let commit1 = commit_with_tree(test_workspace.repo.store(), tree1.clone());
    let ws = &mut test_workspace.workspace;
    ws.check_out(op_id.clone(), None, &commit1).block_on()?;

    testutils::write_working_copy_file(&workspace_root, nested_gitignore_path, "!file\n");
    testutils::write_working_copy_file(&workspace_root, ignored_path, "contents");

    let new_tree = test_workspace.snapshot()?;
    assert_tree_eq!(new_tree, tree1);

    // The nested .gitignore is ignored even if it's tracked
    let tree2 = create_tree(
        &test_workspace.repo,
        &[
            (gitignore_path, "ignored\n"),
            (nested_gitignore_path, "!file\n"),
        ],
    );
    let commit2 = commit_with_tree(test_workspace.repo.store(), tree2.clone());
    let mut locked_ws = test_workspace
        .workspace
        .start_working_copy_mutation()
        .block_on()?;
    locked_ws.locked_wc().reset(&commit2).block_on()?;
    locked_ws
        .finish(OperationId::from_hex("abc123"))
        .block_on()?;

    let new_tree = test_workspace.snapshot()?;
    assert_tree_eq!(new_tree, tree2);
    Ok(())
}

#[test]
fn test_gitignores_checkout_never_overwrites_ignored() -> TestResult {
    // Tests that a .gitignore'd file doesn't get overwritten if check out a commit
    // where the file is tracked.

    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    // Write an ignored file called "modified" to disk
    let gitignore_path = repo_path(".gitignore");
    testutils::write_working_copy_file(&workspace_root, gitignore_path, "modified\n");
    let modified_path = repo_path("modified");
    testutils::write_working_copy_file(&workspace_root, modified_path, "garbage");

    // Create a tree that adds the same file but with different contents
    let tree = create_tree(repo, &[(modified_path, "contents")]);
    let commit = commit_with_tree(repo.store(), tree);

    // Now check out the tree that adds the file "modified" with contents
    // "contents". The exiting contents ("garbage") shouldn't be replaced in the
    // working copy.
    let ws = &mut test_workspace.workspace;
    assert!(
        ws.check_out(repo.op_id().clone(), None, &commit)
            .block_on()
            .is_ok()
    );

    // Check that the old contents are in the working copy
    let path = workspace_root.join("modified");
    assert!(path.is_file());
    assert_eq!(std::fs::read(&path)?, b"garbage");
    Ok(())
}

#[test]
fn test_gitignores_ignored_directory_already_tracked() -> TestResult {
    // Tests that a .gitignore'd directory that already has a tracked file in it
    // does not get removed when snapshotting the working directory.

    let mut test_workspace = TestWorkspace::init();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    let repo = test_workspace.repo.clone();

    let gitignore_path = repo_path(".gitignore");
    let unchanged_normal_path = repo_path("ignored/unchanged_normal");
    let modified_normal_path = repo_path("ignored/modified_normal");
    let deleted_normal_path = repo_path("ignored/deleted_normal");
    let unchanged_executable_path = repo_path("ignored/unchanged_executable");
    let modified_executable_path = repo_path("ignored/modified_executable");
    let deleted_executable_path = repo_path("ignored/deleted_executable");
    let unchanged_symlink_path = repo_path("ignored/unchanged_symlink");
    let modified_symlink_path = repo_path("ignored/modified_symlink");
    let deleted_symlink_path = repo_path("ignored/deleted_symlink");
    let tree = create_tree_with(&repo, |builder| {
        builder.file(gitignore_path, "/ignored/\n");
        builder.file(unchanged_normal_path, "contents");
        builder.file(modified_normal_path, "contents");
        builder.file(deleted_normal_path, "contents");
        builder
            .file(unchanged_executable_path, "contents")
            .executable(true);
        builder
            .file(modified_executable_path, "contents")
            .executable(true);
        builder
            .file(deleted_executable_path, "contents")
            .executable(true);
        builder.symlink(unchanged_symlink_path, "contents");
        builder.symlink(modified_symlink_path, "contents");
        builder.symlink(deleted_symlink_path, "contents");
    });
    let commit = commit_with_tree(repo.store(), tree);

    // Check out the tree with the files in `ignored/`
    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit)
        .block_on()?;

    // Make some changes inside the ignored directory and check that they are
    // detected when we snapshot. The files that are still there should not be
    // deleted from the resulting tree.
    std::fs::write(
        modified_normal_path.to_fs_path_unchecked(&workspace_root),
        "modified",
    )?;
    std::fs::remove_file(deleted_normal_path.to_fs_path_unchecked(&workspace_root))?;
    std::fs::write(
        modified_executable_path.to_fs_path_unchecked(&workspace_root),
        "modified",
    )?;
    std::fs::remove_file(deleted_executable_path.to_fs_path_unchecked(&workspace_root))?;
    let fs_path = modified_symlink_path.to_fs_path_unchecked(&workspace_root);
    std::fs::remove_file(&fs_path)?;
    if check_symlink_support().unwrap_or(false) {
        symlink_file("modified", &fs_path)?;
    } else {
        std::fs::write(fs_path, "modified")?;
    }
    std::fs::remove_file(deleted_symlink_path.to_fs_path_unchecked(&workspace_root))?;
    let new_tree = test_workspace.snapshot()?;
    let expected_tree = create_tree_with(&repo, |builder| {
        builder.file(gitignore_path, "/ignored/\n");
        builder.file(unchanged_normal_path, "contents");
        builder.file(modified_normal_path, "modified");
        builder
            .file(unchanged_executable_path, "contents")
            .executable(true);
        builder
            .file(modified_executable_path, "modified")
            .executable(true);
        builder.symlink(unchanged_symlink_path, "contents");
        builder.symlink(modified_symlink_path, "modified");
    });
    assert_tree_eq!(new_tree, expected_tree);
    Ok(())
}

#[test]
fn test_dotgit_ignored() -> TestResult {
    // Tests that .git directories and files are always ignored (we could accept
    // them if the backend is not git).

    let mut test_workspace = TestWorkspace::init();
    let store = test_workspace.repo.store().clone();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    // Test with a .git/ directory (with a file in, since we don't write empty
    // trees)
    let dotgit_path = workspace_root.join(".git");
    std::fs::create_dir(&dotgit_path)?;
    testutils::write_working_copy_file(&workspace_root, repo_path(".git/file"), "contents");
    let new_tree = test_workspace.snapshot()?;
    let empty_tree = store.empty_merged_tree();
    assert_tree_eq!(new_tree, empty_tree);
    std::fs::remove_dir_all(&dotgit_path)?;

    // Test with a .git file
    testutils::write_working_copy_file(&workspace_root, repo_path(".git"), "contents");
    let new_tree = test_workspace.snapshot()?;
    assert_tree_eq!(new_tree, empty_tree);
    std::fs::remove_file(workspace_root.join(".git"))?;

    // Test a nested repository foo/ containing .git and f.
    let foo_path = workspace_root.join("foo");
    std::fs::create_dir(&foo_path)?;
    testutils::write_working_copy_file(&workspace_root, repo_path("foo/.git"), "");
    testutils::write_working_copy_file(&workspace_root, repo_path("foo/f"), "contents");
    let new_tree = test_workspace.snapshot()?;
    assert_tree_eq!(new_tree, empty_tree);
    std::fs::remove_dir_all(&foo_path)?;
    Ok(())
}

#[test_case(""; "ignore nothing")]
#[test_case("/*\n"; "ignore all")]
fn test_git_submodule(gitignore_content: &str) -> TestResult {
    // Tests that git submodules are ignored.

    let mut test_workspace = TestWorkspace::init_with_backend(TestRepoBackend::Git);
    let repo = test_workspace.repo.clone();
    let store = repo.store().clone();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    let base_ignores = GitIgnoreFile::empty().chain(
        RepoPath::root(),
        Path::new(""),
        gitignore_content.as_bytes(),
    )?;
    let snapshot_options = SnapshotOptions {
        base_ignores,
        ..empty_snapshot_options()
    };
    let mut tx = repo.start_transaction();

    // Add files in sub directory. Sub directories are traversed differently
    // depending on .gitignore. #5246
    let added_path = repo_path("sub/added");
    let submodule_path = repo_path("sub/module");
    let added_submodule_path = repo_path("sub/module/added");

    let mut tree_builder = MergedTreeBuilder::new(store.empty_merged_tree());

    tree_builder.set_or_remove(
        added_path.to_owned(),
        Merge::normal(TreeValue::File {
            id: testutils::write_file(repo.store(), added_path, "added\n"),
            executable: false,
            copy_id: CopyId::new(vec![]),
        }),
    );

    let submodule_id1 = write_random_commit(tx.repo_mut()).id().clone();

    tree_builder.set_or_remove(
        submodule_path.to_owned(),
        Merge::normal(TreeValue::GitSubmodule(submodule_id1)),
    );

    let tree_id1 = tree_builder.write_tree().block_on()?;
    let commit1 = commit_with_tree(repo.store(), tree_id1.clone());

    let mut tree_builder = MergedTreeBuilder::new(tree_id1.clone());
    let submodule_id2 = write_random_commit(tx.repo_mut()).id().clone();
    tree_builder.set_or_remove(
        submodule_path.to_owned(),
        Merge::normal(TreeValue::GitSubmodule(submodule_id2)),
    );
    let tree_id2 = tree_builder.write_tree().block_on()?;
    let commit2 = commit_with_tree(repo.store(), tree_id2.clone());

    // A commit with a file instead of the submodule at the same path
    let mut tree_builder = MergedTreeBuilder::new(store.empty_merged_tree());
    tree_builder.set_or_remove(
        submodule_path.to_owned(),
        Merge::normal(TreeValue::File {
            id: testutils::write_file(
                repo.store(),
                submodule_path,
                "file with same path as submodule\n",
            ),
            executable: false,
            copy_id: CopyId::new(vec![]),
        }),
    );
    let tree_id3 = tree_builder.write_tree().block_on()?;
    let commit3_file_clash = commit_with_tree(repo.store(), tree_id3.clone());

    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;

    testutils::write_working_copy_file(
        &workspace_root,
        added_submodule_path,
        "i am a file in a submodule\n",
    );

    // Check that the files present in the submodule are not tracked
    // when we snapshot
    let (new_tree, _stats) = test_workspace.snapshot_with_options(&snapshot_options)?;
    assert_tree_eq!(new_tree, tree_id1);

    // Check that the files in the submodule are not deleted
    let file_in_submodule_path = added_submodule_path.to_fs_path_unchecked(&workspace_root);
    assert!(
        file_in_submodule_path.metadata().is_ok(),
        "{file_in_submodule_path:?} should exist"
    );

    // Check out new commit updating the submodule, which shouldn't fail because
    // of existing submodule files
    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit2)
        .block_on()?;

    // Check that the files in the submodule are not deleted
    let file_in_submodule_path = added_submodule_path.to_fs_path_unchecked(&workspace_root);
    assert!(
        file_in_submodule_path.metadata().is_ok(),
        "{file_in_submodule_path:?} should exist"
    );

    // Check that the files present in the submodule are not tracked
    // when we snapshot
    let (new_tree, _stats) = test_workspace.snapshot_with_options(&snapshot_options)?;
    assert_tree_eq!(new_tree, tree_id2);

    // Check out the empty tree, which shouldn't fail
    let ws = &mut test_workspace.workspace;
    let stats = ws
        .check_out(repo.op_id().clone(), None, &store.root_commit())
        .block_on()?;
    assert_eq!(stats.skipped_files, 0, "Empty tree should checkout cleanly");

    // Start with an empty submodule directory and check out a commit without
    // the submodule
    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;
    std::fs::remove_file(added_submodule_path.to_fs_path_unchecked(&workspace_root))?;
    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &store.root_commit())
        .block_on()?;

    // Check that the empty submodule directory was removed
    let submodule_dir = submodule_path.to_fs_path_unchecked(&workspace_root);
    assert!(
        submodule_dir.metadata().is_err(),
        "{submodule_dir:?} should not exist"
    );

    // Go back to a commit with the submodule
    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit2)
        .block_on()?;

    // Check that the empty submodule directory was created
    let submodule_dir = submodule_path.to_fs_path_unchecked(&workspace_root);
    assert!(
        submodule_dir.metadata().is_ok(),
        "{submodule_dir:?} should exist"
    );
    assert_eq!(stats.skipped_files, 0);

    // Restore submodule contents (pretend that the user did `git submodule update`)
    testutils::write_working_copy_file(
        &workspace_root,
        added_submodule_path,
        "i am a file in a submodule\n",
    );

    // Check that the files in the submodule are not deleted after checking out
    // a commit without the submodule
    let ws = &mut test_workspace.workspace;
    let stats = ws
        .check_out(repo.op_id().clone(), None, &store.root_commit())
        .block_on()?;
    let file_in_submodule_path = added_submodule_path.to_fs_path_unchecked(&workspace_root);
    assert!(
        file_in_submodule_path.metadata().is_ok(),
        "{file_in_submodule_path:?} should exist"
    );

    // Check that checking out a submodule over an existing directory with the
    // same path does not result in a conflict and that the submodule is still
    // recorded as a submodule in the commit
    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit2)
        .block_on()?;
    assert_eq!(stats.skipped_files, 0);
    let (new_tree, _stats) = test_workspace.snapshot_with_options(&snapshot_options)?;
    assert_tree_eq!(new_tree, tree_id2);

    // Restore submodule contents (pretend that the user did `git submodule update`)
    testutils::write_working_copy_file(
        &workspace_root,
        added_submodule_path,
        "i am a file in a submodule\n",
    );

    // Check out a commit which tries to place a file at the same path
    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit3_file_clash)
        .block_on()?;

    // Check that the submodule is not replaced by the file, preserving the
    // user's existing submodule files
    let file_in_submodule_path = added_submodule_path.to_fs_path_unchecked(&workspace_root);
    assert!(
        file_in_submodule_path.metadata().is_ok(),
        "{file_in_submodule_path:?} should exist"
    );
    Ok(())
}

#[test]
fn test_check_out_existing_file_cannot_be_removed() -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let file_path = repo_path("file");
    let tree1 = create_tree(repo, &[(file_path, "0")]);
    let tree2 = create_tree(repo, &[(file_path, "1")]);
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);

    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;

    // Make the parent directory readonly.
    let writable_dir_perm = workspace_root.symlink_metadata()?.permissions();
    let mut readonly_dir_perm = writable_dir_perm.clone();
    readonly_dir_perm.set_readonly(true);

    std::fs::set_permissions(&workspace_root, readonly_dir_perm)?;
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit2)
        .block_on();
    std::fs::set_permissions(&workspace_root, writable_dir_perm)?;

    // TODO: find a way to trigger the error on Windows
    if !cfg!(windows) {
        assert_matches!(
            result,
            Err(CheckoutError::Other { message, .. }) if message.contains("Failed to remove")
        );
    }
    Ok(())
}

#[test]
fn test_check_out_existing_file_replaced_with_directory() -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let file_path = repo_path("file");
    let tree1 = create_tree(repo, &[(file_path, "0")]);
    let tree2 = create_tree(repo, &[(file_path, "1")]);
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);

    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;

    std::fs::remove_file(file_path.to_fs_path_unchecked(&workspace_root))?;
    std::fs::create_dir(file_path.to_fs_path_unchecked(&workspace_root))?;

    // Checkout doesn't fail, but the file should be skipped.
    let stats = ws
        .check_out(repo.op_id().clone(), None, &commit2)
        .block_on()?;
    assert_eq!(stats.skipped_files, 1);
    assert!(file_path.to_fs_path_unchecked(&workspace_root).is_dir());
    Ok(())
}

#[test]
fn test_check_out_existing_directory_symlink() -> TestResult {
    if !check_symlink_support()? {
        eprintln!("Skipping test because symlink isn't supported");
        return Ok(());
    }

    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    // Creates a symlink in working directory, and a tree that will add a file
    // under the symlinked directory.
    symlink_dir("..", workspace_root.join("parent"))?;

    // Test two file paths writing to the same directory to ensure that
    // any directory creation optimizations which depend on how
    // `parent/escaped1` behaved don't allow `parent/escaped2` to be
    // created
    let file_path1 = repo_path("parent/escaped1");
    let file_path2 = repo_path("parent/escaped2");
    let tree = create_tree(repo, &[(file_path1, "contents"), (file_path2, "contents")]);
    let commit = commit_with_tree(repo.store(), tree);

    // Checkout doesn't fail, but the file should be skipped.
    let ws = &mut test_workspace.workspace;
    let stats = ws
        .check_out(repo.op_id().clone(), None, &commit)
        .block_on()?;
    assert_eq!(stats.skipped_files, 2);

    // Therefore, "../escaped*" paths shouldn't be created.
    assert!(!workspace_root.parent().unwrap().join("escaped1").exists());
    assert!(!workspace_root.parent().unwrap().join("escaped2").exists());
    Ok(())
}

#[test]
fn test_check_out_existing_directory_symlink_icase_fs() -> TestResult {
    if !check_symlink_support()? {
        eprintln!("Skipping test because symlink isn't supported");
        return Ok(());
    }

    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    let is_icase_fs = check_icase_fs(&workspace_root);

    // Creates a symlink in working directory, and a tree that will add a file
    // under the symlinked directory.
    symlink_dir("..", workspace_root.join("parent"))?;

    let file_path1 = repo_path("PARENT/escaped1");
    let file_path2 = repo_path("PARENT/escaped2");
    let tree = create_tree(repo, &[(file_path1, "contents"), (file_path2, "contents")]);
    let commit = commit_with_tree(repo.store(), tree);

    // Checkout doesn't fail, but the file should be skipped on icase fs.
    let ws = &mut test_workspace.workspace;
    let stats = ws
        .check_out(repo.op_id().clone(), None, &commit)
        .block_on()?;
    if is_icase_fs {
        assert_eq!(stats.skipped_files, 2);
    } else {
        assert_eq!(stats.skipped_files, 0);
    }

    // Therefore, "../escaped*" paths shouldn't be created.
    assert!(!workspace_root.parent().unwrap().join("escaped1").exists());
    assert!(!workspace_root.parent().unwrap().join("escaped2").exists());
    Ok(())
}

#[test_case(false; "symlink target does not exist")]
#[test_case(true; "symlink target exists")]
fn test_check_out_existing_file_symlink_icase_fs(victim_exists: bool) -> TestResult {
    if !check_symlink_support()? {
        eprintln!("Skipping test because symlink isn't supported");
        return Ok(());
    }

    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    let is_icase_fs = check_icase_fs(&workspace_root);

    // Creates a symlink in working directory, and a tree that will overwrite
    // the symlink content.
    symlink_file(
        PathBuf::from_iter(["..", "pwned"]),
        workspace_root.join("parent"),
    )?;
    let victim_file_path = workspace_root.parent().unwrap().join("pwned");
    if victim_exists {
        std::fs::write(&victim_file_path, "old")?;
    }
    assert_eq!(workspace_root.join("parent").exists(), victim_exists);

    let file_path = repo_path("PARENT");
    let tree = create_tree(repo, &[(file_path, "bad")]);
    let commit = commit_with_tree(repo.store(), tree);

    // Checkout doesn't fail, but the file should be skipped on icase fs.
    let ws = &mut test_workspace.workspace;
    let stats = ws
        .check_out(repo.op_id().clone(), None, &commit)
        .block_on()?;
    if is_icase_fs {
        assert_eq!(stats.skipped_files, 1);
    } else {
        assert_eq!(stats.skipped_files, 0);
    }

    // Therefore, "../pwned" shouldn't be updated.
    if victim_exists {
        assert_eq!(std::fs::read(&victim_file_path)?, b"old");
    } else {
        assert!(!victim_file_path.exists());
    }
    Ok(())
}

#[test]
fn test_check_out_file_removal_over_existing_directory_symlink() -> TestResult {
    if !check_symlink_support()? {
        eprintln!("Skipping test because symlink isn't supported");
        return Ok(());
    }

    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let file_path = repo_path("parent/escaped");
    let tree1 = create_tree(repo, &[(file_path, "contents")]);
    let tree2 = create_tree(repo, &[]);
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);

    // Check out "parent/escaped".
    let ws = &mut test_workspace.workspace;
    ws.check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;

    // Pretend that "parent" was a symlink, which might be created by
    // e.g. checking out "PARENT" on case-insensitive fs. The file
    // "parent/escaped" would be skipped in that case.
    std::fs::remove_file(file_path.to_fs_path_unchecked(&workspace_root))?;
    std::fs::remove_dir(workspace_root.join("parent"))?;
    symlink_dir("..", workspace_root.join("parent"))?;
    let victim_file_path = workspace_root.parent().unwrap().join("escaped");
    std::fs::write(&victim_file_path, "")?;
    assert!(file_path.to_fs_path_unchecked(&workspace_root).exists());

    // Check out empty tree, which tries to remove "parent/escaped".
    let stats = ws
        .check_out(repo.op_id().clone(), None, &commit2)
        .block_on()?;
    assert_eq!(stats.skipped_files, 1);

    // "../escaped" shouldn't be removed.
    assert!(victim_file_path.exists());
    Ok(())
}

#[test_case(".git"; "reserved .git dir")]
#[test_case(".jj"; "reserved .jj dir")]
#[test_case("symlink"; "looped")]
#[test_case("unknown"; "dead")]
#[cfg_attr(windows, ignore = "Windows impl follows symlink")] // FIXME
fn test_check_out_symlink_unusual_target(link_target: &str) -> TestResult {
    if !check_symlink_support()? {
        eprintln!("Skipping test because symlink isn't supported");
        return Ok(());
    }

    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    std::fs::create_dir(workspace_root.join(".git"))?;

    let symlink_path = repo_path("symlink");
    let symlink_disk_path = symlink_path.to_fs_path_unchecked(&workspace_root);
    let tree1 = create_tree_with(repo, |builder| {
        builder.symlink(symlink_path, link_target);
    });
    let tree2 = create_tree(repo, &[]);
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);

    // Check out tree containing symlink
    let ws = &mut test_workspace.workspace;
    let stats = ws
        .check_out(repo.op_id().clone(), None, &commit1)
        .block_on()?;
    assert_eq!(stats.added_files, 1);

    // Symlink should be created
    assert_eq!(symlink_disk_path.read_link()?.as_os_str(), link_target);

    // Check out empty tree
    let stats = ws
        .check_out(repo.op_id().clone(), None, &commit2)
        .block_on()?;
    assert_eq!(stats.removed_files, 1);

    // Symlink should be deleted
    assert_matches!(
        symlink_disk_path.symlink_metadata().map_err(|e| e.kind()),
        Err(io::ErrorKind::NotFound)
    );
    Ok(())
}

#[test_case("../pwned"; "escape from root")]
#[test_case("sub/../../pwned"; "escape from sub dir")]
fn test_check_out_malformed_file_path(file_path_str: &str) {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let file_path = repo_path(file_path_str);
    let tree = create_tree(repo, &[(file_path, "contents")]);
    let commit = commit_with_tree(repo.store(), tree);

    // Checkout should fail
    let ws = &mut test_workspace.workspace;
    let result = ws.check_out(repo.op_id().clone(), None, &commit).block_on();
    assert_matches!(result, Err(CheckoutError::InvalidRepoPath(_)));

    // Therefore, "pwned" file shouldn't be created.
    assert!(!workspace_root.join(file_path_str).exists());
    assert!(!workspace_root.parent().unwrap().join("pwned").exists());
}

#[test_case(r"sub\..\../pwned"; "path separator")]
#[test_case("d:/pwned"; "drive letter")]
fn test_check_out_malformed_file_path_windows(file_path_str: &str) {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    let file_path = repo_path(file_path_str);
    let tree = create_tree(repo, &[(file_path, "contents")]);
    let commit = commit_with_tree(repo.store(), tree);

    // Checkout should fail on Windows
    let ws = &mut test_workspace.workspace;
    let result = ws.check_out(repo.op_id().clone(), None, &commit).block_on();
    if cfg!(windows) {
        assert_matches!(result, Err(CheckoutError::InvalidRepoPath(_)));
    } else {
        assert_matches!(result, Ok(_));
    }

    // Therefore, "pwned" file shouldn't be created.
    if cfg!(windows) {
        assert!(!workspace_root.join(file_path_str).exists());
    }
    assert!(!workspace_root.parent().unwrap().join("pwned").exists());
}

#[test_case(".git"; "root .git file")]
#[test_case(".jj"; "root .jj file")]
#[test_case(".git/pwned"; "root .git dir")]
#[test_case(".jj/pwned"; "root .jj dir")]
#[test_case("sub/.git"; "sub .git file")]
#[test_case("sub/.jj"; "sub .jj file")]
#[test_case("sub/.git/pwned"; "sub .git dir")]
#[test_case("sub/.jj/pwned"; "sub .jj dir")]
fn test_check_out_reserved_file_path(file_path_str: &str) -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    std::fs::create_dir(workspace_root.join(".git"))?;

    let file_path = repo_path(file_path_str);
    let disk_path = file_path.to_fs_path_unchecked(&workspace_root);
    let tree1 = create_tree(repo, &[(file_path, "contents")]);
    let tree2 = create_tree(repo, &[]);
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);

    // Checkout should fail.
    let ws = &mut test_workspace.workspace;
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit1)
        .block_on();
    assert_matches!(result, Err(CheckoutError::ReservedPathComponent { .. }));

    // Therefore, "pwned" file shouldn't be created.
    if ![".git", ".jj"].contains(&file_path_str) {
        assert!(!disk_path.exists());
    }
    assert!(!workspace_root.join(".git").join("pwned").exists());
    assert!(!workspace_root.join(".jj").join("pwned").exists());
    assert!(!workspace_root.join("sub").join(".git").exists());
    assert!(!workspace_root.join("sub").join(".jj").exists());

    // Pretend that the checkout somehow succeeded.
    let mut locked_ws = ws.start_working_copy_mutation().block_on()?;
    locked_ws.locked_wc().reset(&commit1).block_on()?;
    locked_ws.finish(repo.op_id().clone()).block_on()?;
    if ![".git", ".jj"].contains(&file_path_str) {
        std::fs::create_dir_all(disk_path.parent().unwrap())?;
        std::fs::write(&disk_path, "")?;
    }

    // Check out empty tree, which tries to remove the file.
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit2)
        .block_on();
    assert_matches!(result, Err(CheckoutError::ReservedPathComponent { .. }));

    // The existing file shouldn't be removed.
    assert!(disk_path.exists());
    Ok(())
}

#[test_case(".Git/pwned"; "root .git dir")]
#[test_case(".jJ/pwned"; "root .jj dir")]
#[test_case("sub/.GIt"; "sub .git file")]
#[test_case("sub/.JJ"; "sub .jj file")]
#[test_case("sub/.gIT/pwned"; "sub .git dir")]
#[test_case("sub/.Jj/pwned"; "sub .jj dir")]
fn test_check_out_reserved_file_path_icase_fs(file_path_str: &str) -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    std::fs::create_dir(workspace_root.join(".git"))?;
    let is_icase_fs = check_icase_fs(&workspace_root);

    let file_path = repo_path(file_path_str);
    let disk_path = file_path.to_fs_path_unchecked(&workspace_root);
    let tree1 = create_tree(repo, &[(file_path, "contents")]);
    let tree2 = create_tree(repo, &[]);
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);

    // Checkout should fail on icase fs.
    let ws = &mut test_workspace.workspace;
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit1)
        .block_on();
    if is_icase_fs {
        assert_matches!(result, Err(CheckoutError::ReservedPathComponent { .. }));
    } else {
        assert_matches!(result, Ok(_));
    }

    // Therefore, "pwned" file shouldn't be created.
    if is_icase_fs {
        assert!(!disk_path.exists());
    }
    assert!(!workspace_root.join(".git").join("pwned").exists());
    assert!(!workspace_root.join(".jj").join("pwned").exists());
    assert!(!workspace_root.join("sub").join(".git").exists());
    assert!(!workspace_root.join("sub").join(".jj").exists());

    // Pretend that the checkout somehow succeeded.
    let mut locked_ws = ws.start_working_copy_mutation().block_on()?;
    locked_ws.locked_wc().reset(&commit1).block_on()?;
    locked_ws.finish(repo.op_id().clone()).block_on()?;
    std::fs::create_dir_all(disk_path.parent().unwrap())?;
    std::fs::write(&disk_path, "")?;

    // Check out empty tree, which tries to remove the file.
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit2)
        .block_on();
    if is_icase_fs {
        assert_matches!(result, Err(CheckoutError::ReservedPathComponent { .. }));
    } else {
        assert_matches!(result, Ok(_));
    }

    // The existing file shouldn't be removed on icase fs.
    if is_icase_fs {
        assert!(disk_path.exists());
    }
    Ok(())
}

// Here we don't test ignored characters exhaustively because our implementation
// isn't using deny list.
#[test_case("\u{200c}.git/pwned"; "root .git dir")]
#[test_case(".\u{200d}jj/pwned"; "root .jj dir")]
#[test_case("sub/.g\u{200c}it"; "sub .git file")]
#[test_case("sub/.jj\u{200d}"; "sub .jj file")]
#[test_case("sub/.gi\u{200e}t/pwned"; "sub .git dir")]
#[test_case("sub/.jj\u{200f}/pwned"; "sub .jj dir")]
fn test_check_out_reserved_file_path_hfs_plus(file_path_str: &str) -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    std::fs::create_dir(workspace_root.join(".git"))?;
    let is_hfs_plus = check_hfs_plus(&workspace_root);

    let file_path = repo_path(file_path_str);
    let disk_path = file_path.to_fs_path_unchecked(&workspace_root);
    let tree1 = create_tree(repo, &[(file_path, "contents")]);
    let tree2 = create_tree(repo, &[]);
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);

    // Checkout should fail on HFS+-like fs.
    let ws = &mut test_workspace.workspace;
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit1)
        .block_on();
    if is_hfs_plus {
        assert_matches!(result, Err(CheckoutError::ReservedPathComponent { .. }));
    } else {
        assert_matches!(result, Ok(_));
    }

    // Therefore, "pwned" file shouldn't be created.
    if is_hfs_plus {
        assert!(!disk_path.exists());
    }
    assert!(!workspace_root.join(".git").join("pwned").exists());
    assert!(!workspace_root.join(".jj").join("pwned").exists());
    assert!(!workspace_root.join("sub").join(".git").exists());
    assert!(!workspace_root.join("sub").join(".jj").exists());

    // Pretend that the checkout somehow succeeded.
    let mut locked_ws = ws.start_working_copy_mutation().block_on()?;
    locked_ws.locked_wc().reset(&commit1).block_on()?;
    locked_ws.finish(repo.op_id().clone()).block_on()?;
    std::fs::create_dir_all(disk_path.parent().unwrap())?;
    std::fs::write(&disk_path, "")?;

    // Check out empty tree, which tries to remove the file.
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit2)
        .block_on();
    if is_hfs_plus {
        assert_matches!(result, Err(CheckoutError::ReservedPathComponent { .. }));
    } else {
        assert_matches!(result, Ok(_));
    }

    // The existing file shouldn't be removed on HFS+-like fs.
    if is_hfs_plus {
        assert!(disk_path.exists());
    }
    Ok(())
}

#[test_case(".git/pwned", &["GIT~1/pwned", "GI2837~1/pwned"]; "root .git dir short name")]
#[test_case(".jj/pwned", &["JJ~1/pwned", "JJ2E09~1/pwned"]; "root .jj dir short name")]
#[test_case(".git/pwned", &[".GIT./pwned"]; "root .git dir trailing dots")]
#[test_case(".jj/pwned", &[".JJ../pwned"]; "root .jj dir trailing dots")]
#[test_case("sub/.git", &["sub/.GIT.."]; "sub .git file trailing dots")]
#[test_case("sub/.jj", &["sub/.JJ."]; "sub .jj file trailing dots")]
// TODO: Add more weird patterns?
// - https://en.wikipedia.org/wiki/8.3_filename
// - See is_ntfs_dotgit() of Git and pathauditor of Mercurial
fn test_check_out_reserved_file_path_vfat(
    vfat_path_str: &str,
    file_path_strs: &[&str],
) -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    std::fs::create_dir(workspace_root.join(".git"))?;
    let is_vfat = check_vfat(&workspace_root);

    let vfat_disk_path = workspace_root.join(vfat_path_str);
    let file_paths = file_path_strs.iter().map(|&s| repo_path(s)).collect_vec();
    let tree1 = create_tree_with(repo, |builder| {
        for path in file_paths {
            builder.file(path, "contents");
        }
    });
    let tree2 = create_tree(repo, &[]);
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);

    // Checkout should fail on VFAT-like fs.
    let ws = &mut test_workspace.workspace;
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit1)
        .block_on();
    if is_vfat {
        assert_matches!(result, Err(CheckoutError::ReservedPathComponent { .. }));
    } else {
        assert_matches!(result, Ok(_));
    }

    // Therefore, "pwned" file shouldn't be created.
    if is_vfat {
        assert!(!vfat_disk_path.exists());
    }
    assert!(!workspace_root.join(".git").join("pwned").exists());
    assert!(!workspace_root.join(".jj").join("pwned").exists());
    assert!(!workspace_root.join("sub").join(".git").exists());
    assert!(!workspace_root.join("sub").join(".jj").exists());

    // Pretend that the checkout somehow succeeded.
    let mut locked_ws = ws.start_working_copy_mutation().block_on()?;
    locked_ws.locked_wc().reset(&commit1).block_on()?;
    locked_ws.finish(repo.op_id().clone()).block_on()?;
    if is_vfat {
        std::fs::create_dir_all(vfat_disk_path.parent().unwrap())?;
        std::fs::write(&vfat_disk_path, "")?;
    }

    // Check out empty tree, which tries to remove the file.
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit2)
        .block_on();
    if is_vfat {
        assert_matches!(result, Err(CheckoutError::ReservedPathComponent { .. }));
    } else {
        assert_matches!(result, Ok(_));
    }

    // The existing file shouldn't be removed on VFAT-like fs.
    if is_vfat {
        assert!(vfat_disk_path.exists());
    }
    Ok(())
}

#[test_case(".git"; "root .git file")]
#[test_case(".git/pwned"; "root .git dir")]
fn test_check_out_reserved_file_path_dot_git_symlink(file_path_str: &str) -> TestResult {
    if !check_symlink_support()? {
        eprintln!("Skipping test because symlink isn't supported");
        return Ok(());
    }

    let mut test_workspace = TestWorkspace::init();
    let repo = &test_workspace.repo;
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();

    // Create symlink .git -> ../git-repo
    let git_repo_dir = test_workspace.env.root().join("git-repo");
    let dot_git_path = workspace_root.join(".git");
    std::fs::create_dir(&git_repo_dir)?;
    symlink_dir(&git_repo_dir, &dot_git_path)?;
    assert!(dot_git_path.exists());

    let file_path = repo_path(file_path_str);
    let disk_path = file_path.to_fs_path_unchecked(&workspace_root);
    let tree1 = create_tree(repo, &[(file_path, "contents")]);
    let tree2 = create_tree(repo, &[]);
    let commit1 = commit_with_tree(repo.store(), tree1);
    let commit2 = commit_with_tree(repo.store(), tree2);

    // Checkout should fail.
    let ws = &mut test_workspace.workspace;
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit1)
        .block_on();
    assert_matches!(result, Err(CheckoutError::ReservedPathComponent { .. }));

    // Therefore, "pwned" file shouldn't be created.
    assert!(!git_repo_dir.join("pwned").exists());
    assert!(!dot_git_path.join("pwned").exists());

    // Pretend that the checkout somehow succeeded.
    let mut locked_ws = ws.start_working_copy_mutation().block_on()?;
    locked_ws.locked_wc().reset(&commit1).block_on()?;
    locked_ws.finish(repo.op_id().clone()).block_on()?;
    if file_path_str != ".git" {
        std::fs::write(&disk_path, "")?;
    }

    // Check out empty tree, which tries to remove the file.
    let result = ws
        .check_out(repo.op_id().clone(), None, &commit2)
        .block_on();
    assert_matches!(result, Err(CheckoutError::ReservedPathComponent { .. }));

    // The existing file shouldn't be removed.
    assert!(disk_path.exists());
    Ok(())
}

#[test]
fn test_fsmonitor() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;

    let foo_path = repo_path("foo");
    let bar_path = repo_path("bar");
    let nested_path = repo_path("path/to/nested");
    testutils::write_working_copy_file(&workspace_root, foo_path, "foo\n");
    testutils::write_working_copy_file(&workspace_root, bar_path, "bar\n");
    testutils::write_working_copy_file(&workspace_root, nested_path, "nested\n");

    let ignored_path = repo_path("path/to/ignored");
    let gitignore_path = repo_path("path/.gitignore");
    testutils::write_working_copy_file(&workspace_root, ignored_path, "ignored\n");
    testutils::write_working_copy_file(&workspace_root, gitignore_path, "to/ignored\n");

    let snapshot = |paths: &[&RepoPath]| {
        let changed_files = paths
            .iter()
            .map(|p| p.to_fs_path_unchecked(Path::new("")))
            .collect();
        let settings = TreeStateSettings {
            fsmonitor_settings: FsmonitorSettings::Test {
                changed_files,
                scan_root: None,
            },
            ..tree_state_settings.clone()
        };
        let mut tree_state = TreeState::load(
            repo.store().clone(),
            workspace_root.clone(),
            state_path.clone(),
            &settings,
        )
        .unwrap();
        tree_state
            .snapshot(&empty_snapshot_options())
            .block_on()
            .unwrap();
        tree_state
    };

    // Test is an advisory mutable-root monitor. Without per-path rows its
    // changed names cannot prove unchanged paths, so it conservatively scans
    // the whole working copy.
    let tree_state = snapshot(&[]);
    let expected_tree = create_tree(
        repo,
        &[
            (foo_path, "foo\n"),
            (bar_path, "bar\n"),
            (nested_path, "nested\n"),
            (gitignore_path, "to/ignored\n"),
        ],
    );
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);

    let tree_state = snapshot(&[foo_path]);
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);

    let mut tree_state = snapshot(&[foo_path, bar_path, nested_path, ignored_path]);
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    tree_state.save()?;

    testutils::write_working_copy_file(&workspace_root, foo_path, "updated foo\n");
    testutils::write_working_copy_file(&workspace_root, bar_path, "updated bar\n");
    let tree_state = snapshot(&[foo_path]);
    let expected_tree = create_tree(
        repo,
        &[
            (foo_path, "updated foo\n"),
            (bar_path, "updated bar\n"),
            (nested_path, "nested\n"),
            (gitignore_path, "to/ignored\n"),
        ],
    );
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);

    std::fs::remove_file(foo_path.to_fs_path_unchecked(&workspace_root))?;
    let mut tree_state = snapshot(&[foo_path]);
    let expected_tree = create_tree(
        repo,
        &[
            (bar_path, "updated bar\n"),
            (nested_path, "nested\n"),
            (gitignore_path, "to/ignored\n"),
        ],
    );
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    tree_state.save()?;
    Ok(())
}

#[test]
fn test_fsmonitor_scan_root_is_used_for_snapshot_reads() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;

    let gitignore_path = repo_path(".gitignore");
    let plain_path = repo_path("plain");
    let tracked_ignored_path = repo_path("ignored/tracked");
    let globally_ignored_path = repo_path("candidate.ignored");
    let changed_paths = [
        gitignore_path,
        plain_path,
        tracked_ignored_path,
        globally_ignored_path,
    ];
    let snapshot = |options: &SnapshotOptions<'_>| {
        let changed_files = changed_paths
            .iter()
            .map(|path| path.to_fs_path_unchecked(Path::new("")))
            .collect();
        let settings = TreeStateSettings {
            fsmonitor_settings: FsmonitorSettings::Test {
                changed_files,
                scan_root: Some(scan_root.clone()),
            },
            ..tree_state_settings.clone()
        };
        let mut tree_state = TreeState::load(
            repo.store().clone(),
            workspace_root.clone(),
            state_path.clone(),
            &settings,
        )
        .unwrap();
        tree_state.snapshot(options).block_on().unwrap();
        tree_state.save().unwrap();
        tree_state
    };

    // Establish tracked file state from the synthetic scan root. The live root
    // deliberately contains different data so reading from it would fail the
    // assertions below.
    testutils::write_working_copy_file(&scan_root, gitignore_path, "");
    testutils::write_working_copy_file(&scan_root, plain_path, "scan plain one\n");
    testutils::write_working_copy_file(&scan_root, tracked_ignored_path, "scan tracked one\n");
    testutils::write_working_copy_file(&workspace_root, gitignore_path, "");
    testutils::write_working_copy_file(&workspace_root, plain_path, "live plain one\n");
    testutils::write_working_copy_file(&workspace_root, tracked_ignored_path, "live tracked one\n");
    let tree_state = snapshot(&empty_snapshot_options());
    let expected_tree = create_tree(
        repo,
        &[
            (gitignore_path, ""),
            (plain_path, "scan plain one\n"),
            (tracked_ignored_path, "scan tracked one\n"),
        ],
    );
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);

    // Ignoring the directory exercises the tracked-file fast path, which must
    // resolve tracked paths relative to scan_root as well.
    testutils::write_working_copy_file(&scan_root, gitignore_path, "ignored/\n");
    testutils::write_working_copy_file(&scan_root, plain_path, "scan plain two longer\n");
    testutils::write_working_copy_file(
        &scan_root,
        tracked_ignored_path,
        "scan tracked two longer\n",
    );
    testutils::write_working_copy_file(&workspace_root, gitignore_path, "");
    testutils::write_working_copy_file(&workspace_root, plain_path, "live plain two\n");
    testutils::write_working_copy_file(&workspace_root, tracked_ignored_path, "live tracked two\n");
    let tree_state = snapshot(&empty_snapshot_options());
    let expected_tree = create_tree(
        repo,
        &[
            (gitignore_path, "ignored/\n"),
            (plain_path, "scan plain two longer\n"),
            (tracked_ignored_path, "scan tracked two longer\n"),
        ],
    );
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);

    // Worktree-relative global excludes are part of the snapshot contents,
    // rather than external mutable configuration read from the live root.
    std::fs::write(scan_root.join("global-ignore"), "*.ignored\n")?;
    std::fs::write(workspace_root.join("global-ignore"), "")?;
    testutils::write_working_copy_file(&scan_root, globally_ignored_path, "scan ignored\n");
    testutils::write_working_copy_file(&workspace_root, globally_ignored_path, "live visible\n");
    let options = SnapshotOptions {
        scan_root_ignores: vec![PathBuf::from("global-ignore")],
        ..empty_snapshot_options()
    };
    let tree_state = snapshot(&options);
    let expected_tree = create_tree(
        repo,
        &[
            (gitignore_path, "ignored/\n"),
            (plain_path, "scan plain two longer\n"),
            (tracked_ignored_path, "scan tracked two longer\n"),
            (repo_path("global-ignore"), "*.ignored\n"),
        ],
    );
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);

    assert!(state_path.join("working_copy_state").is_file());
    assert!(!scan_root.join("working_copy_state").exists());
    Ok(())
}

#[test]
#[cfg(unix)]
fn test_fsmonitor_scan_root_is_used_for_metadata_sensitive_reads() -> TestResult {
    if !file_util::check_symlink_support()? {
        eprintln!("Symlink not supported. Skip the test.");
        return Ok(());
    }
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;

    let executable_path = repo_path("executable");
    let symlink_path = repo_path("link");
    let deleted_path = repo_path("deleted");
    let large_path = repo_path("large");
    let changed_paths = [executable_path, symlink_path, deleted_path, large_path];
    let snapshot = |options: &SnapshotOptions<'_>| {
        let settings = TreeStateSettings {
            fsmonitor_settings: FsmonitorSettings::Test {
                changed_files: changed_paths
                    .iter()
                    .map(|path| path.to_fs_path_unchecked(Path::new("")))
                    .collect(),
                scan_root: Some(scan_root.clone()),
            },
            ..tree_state_settings.clone()
        };
        let mut tree_state = TreeState::load(
            repo.store().clone(),
            workspace_root.clone(),
            state_path.clone(),
            &settings,
        )
        .unwrap();
        let (_dirty, stats) = tree_state.snapshot(options).block_on().unwrap();
        tree_state.save().unwrap();
        (tree_state, stats)
    };

    testutils::write_working_copy_file(&scan_root, executable_path, "scan executable\n");
    testutils::write_working_copy_file(&workspace_root, executable_path, "live executable\n");
    std::fs::set_permissions(
        executable_path.to_fs_path_unchecked(&scan_root),
        std::fs::Permissions::from_mode(0o755),
    )?;
    std::fs::set_permissions(
        executable_path.to_fs_path_unchecked(&workspace_root),
        std::fs::Permissions::from_mode(0o644),
    )?;
    symlink_file("scan-target", symlink_path.to_fs_path_unchecked(&scan_root))?;
    symlink_file(
        "live-target",
        symlink_path.to_fs_path_unchecked(&workspace_root),
    )?;
    testutils::write_working_copy_file(&scan_root, deleted_path, "scan deleted\n");
    testutils::write_working_copy_file(&workspace_root, deleted_path, "live deleted\n");

    let (tree_state, _stats) = snapshot(&empty_snapshot_options());
    let expected_tree = create_tree_with(repo, |builder| {
        builder
            .file(executable_path, "scan executable\n")
            .executable(true);
        builder.symlink(symlink_path, "scan-target");
        builder.file(deleted_path, "scan deleted\n");
    });
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);

    // Leave conflicting live-root state behind while changing only the scan
    // root. The second snapshot must observe scan-root metadata and deletion,
    // and classify the scan-root large file as untracked even though the live
    // counterpart is small enough to track.
    std::fs::set_permissions(
        executable_path.to_fs_path_unchecked(&scan_root),
        std::fs::Permissions::from_mode(0o644),
    )?;
    std::fs::set_permissions(
        executable_path.to_fs_path_unchecked(&workspace_root),
        std::fs::Permissions::from_mode(0o755),
    )?;
    std::fs::remove_file(symlink_path.to_fs_path_unchecked(&scan_root))?;
    symlink_file(
        "scan-target-two",
        symlink_path.to_fs_path_unchecked(&scan_root),
    )?;
    std::fs::remove_file(deleted_path.to_fs_path_unchecked(&scan_root))?;
    std::fs::write(large_path.to_fs_path_unchecked(&scan_root), vec![0; 17])?;
    std::fs::write(large_path.to_fs_path_unchecked(&workspace_root), vec![0; 1])?;
    let options = SnapshotOptions {
        max_new_file_size: 16,
        ..empty_snapshot_options()
    };
    let (tree_state, stats) = snapshot(&options);
    let expected_tree = create_tree_with(repo, |builder| {
        builder.file(executable_path, "scan executable\n");
        builder.symlink(symlink_path, "scan-target-two");
    });
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    assert_eq!(
        stats
            .untracked_paths
            .keys()
            .map(AsRef::as_ref)
            .collect_vec(),
        [large_path]
    );
    Ok(())
}

#[test]
fn test_fsmonitor_cursor_migrates_legacy_watchman_clock() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    let empty_tree = repo.store().empty_merged_tree();
    write_legacy_tree_state(&state_path, &empty_tree, |proto| {
        #[expect(deprecated)]
        {
            proto.watchman_clock = Some(jj_lib::protos::local_working_copy::WatchmanClock {
                watchman_clock: Some(
                    jj_lib::protos::local_working_copy::watchman_clock::WatchmanClock::StringClock(
                        "legacy-clock".to_owned(),
                    ),
                ),
            });
        }
    })?;

    let watchman_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::Watchman(WatchmanConfig {
            register_trigger: false,
        }),
        ..tree_state_settings.clone()
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &watchman_settings,
    )?;
    tree_state.save()?;

    // Legacy monitor cursors migrate as NoBaseline because they are not a
    // retained immutable snapshot identity.
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;
    let (is_dirty, _stats) = tree_state.snapshot(&empty_snapshot_options()).block_on()?;
    assert!(!is_dirty);
    tree_state.save()?;

    // The clearing above is durable: a second load under the same backend has
    // no cursor left to invalidate.
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root,
        state_path,
        &tree_state_settings,
    )?;
    let (is_dirty, _stats) = tree_state.snapshot(&empty_snapshot_options()).block_on()?;
    assert!(!is_dirty);
    Ok(())
}

#[test]
fn test_compact_working_copy_state_migrates_legacy_tree_state() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    let semantic_tree = create_tree(repo, &[(repo_path("dir/tracked"), "tracked\n")]);
    write_legacy_tree_state(&state_path, &semantic_tree, |proto| {
        proto.sparse_patterns = Some(jj_lib::protos::local_working_copy::SparsePatterns {
            prefixes: vec!["dir".to_owned()],
        });
    })?;

    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    )?;
    assert_tree_eq!(*tree_state.current_tree(), semantic_tree);
    tree_state.save()?;

    let journal = read_compact_working_copy_state(&state_path)?;
    assert_eq!(journal.format_version, 2);
    assert_eq!(
        journal.tree_ids,
        semantic_tree
            .tree_ids()
            .iter()
            .map(|id| id.to_bytes())
            .collect_vec()
    );
    assert_eq!(
        journal.sparse_patterns.as_ref().unwrap().prefixes,
        vec!["dir".to_owned()]
    );
    assert_eq!(
        journal.phase(),
        jj_lib::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline
    );
    assert!(journal.fsmonitor_cursor.is_none());

    let reloaded = TreeState::load(repo.store().clone(), workspace_root, state_path, &settings)?;
    assert_tree_eq!(*reloaded.current_tree(), semantic_tree);
    Ok(())
}

#[test]
fn test_snapshot_mode_compact_state_tracks_semantic_tree_without_legacy_rows() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    let mut tree_state = TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    )?;
    testutils::write_working_copy_file(&workspace_root, repo_path("drop"), "drop\n");
    testutils::write_working_copy_file(&workspace_root, repo_path("keep"), "keep\n");
    testutils::write_working_copy_file(&workspace_root, repo_path("update"), "old\n");
    tree_state.snapshot(&empty_snapshot_options()).block_on()?;
    tree_state.save()?;
    let first_journal = read_compact_working_copy_state(&state_path)?;
    assert!(!state_path.join("tree_state").exists());

    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    )?;
    std::fs::remove_file(repo_path("drop").to_fs_path_unchecked(&workspace_root))?;
    testutils::write_working_copy_file(&workspace_root, repo_path("insert"), "inserted\n");
    testutils::write_working_copy_file(&workspace_root, repo_path("update"), "new contents\n");
    tree_state.snapshot(&empty_snapshot_options()).block_on()?;
    tree_state.save()?;

    let expected_tree = create_tree(
        repo,
        &[
            (repo_path("insert"), "inserted\n"),
            (repo_path("keep"), "keep\n"),
            (repo_path("update"), "new contents\n"),
        ],
    );
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    let second_journal = read_compact_working_copy_state(&state_path)?;
    assert!(second_journal.generation > first_journal.generation);
    assert_eq!(
        second_journal.tree_ids,
        expected_tree
            .tree_ids()
            .iter()
            .map(|id| id.to_bytes())
            .collect_vec()
    );
    assert!(!state_path.join("tree_state").exists());

    let reloaded = TreeState::load(repo.store().clone(), workspace_root, state_path, &settings)?;
    assert_tree_eq!(*reloaded.current_tree(), expected_tree);
    Ok(())
}

#[test]
fn test_compact_working_copy_state_rejects_invalid_phase_and_sparse_prefix() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    )?;

    let mut journal = read_compact_working_copy_state(&state_path)?;
    journal.phase = 99;
    write_compact_working_copy_state(&state_path, &journal)?;
    let err = match TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    ) {
        Ok(_) => panic!("unknown compact-journal phase must fail closed"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("unsupported journal phase 99"),
        "unexpected error: {err}"
    );

    journal.phase = jj_lib::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline as i32;
    journal.sparse_patterns = Some(jj_lib::protos::local_working_copy::SparsePatterns {
        prefixes: vec!["bad//prefix".to_owned()],
    });
    write_compact_working_copy_state(&state_path, &journal)?;
    let err = match TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    ) {
        Ok(_) => panic!("invalid compact-journal sparse prefix must fail closed"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("invalid sparse prefix"),
        "unexpected error: {err}"
    );

    journal.sparse_patterns = Some(jj_lib::protos::local_working_copy::SparsePatterns {
        prefixes: vec!["".to_owned()],
    });
    journal.tree_ids = vec![vec![1], vec![2]];
    write_compact_working_copy_state(&state_path, &journal)?;
    let err = match TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    ) {
        Ok(_) => panic!("even compact-journal tree-ID merge shape must fail closed"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("odd, non-empty merge shape"),
        "unexpected error: {err}"
    );

    journal.tree_ids = repo
        .store()
        .empty_merged_tree()
        .tree_ids()
        .iter()
        .map(|id| id.to_bytes())
        .collect();
    journal.conflict_labels = vec!["invalid label".to_owned()];
    write_compact_working_copy_state(&state_path, &journal)?;
    let err = match TreeState::load(repo.store().clone(), workspace_root, state_path, &settings) {
        Ok(_) => panic!("resolved compact-journal tree labels must fail closed"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("conflict labels do not match"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[test]
fn test_compact_working_copy_state_recovers_pending_materialization() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    )?;

    let old_tree = create_tree(repo, &[(repo_path("old/file"), "old\n")]);
    let intended_tree = create_tree(repo, &[(repo_path("new/file"), "new\n")]);
    let mut journal = read_compact_working_copy_state(&state_path)?;
    journal.tree_ids = old_tree.tree_ids().iter().map(|id| id.to_bytes()).collect();
    journal.conflict_labels = old_tree.labels().as_slice().to_owned();
    journal.sparse_patterns = Some(jj_lib::protos::local_working_copy::SparsePatterns {
        prefixes: vec!["old".to_owned()],
    });
    journal.phase =
        jj_lib::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization as i32;
    journal.pending_tree_ids = intended_tree
        .tree_ids()
        .iter()
        .map(|id| id.to_bytes())
        .collect();
    journal.pending_conflict_labels = intended_tree.labels().as_slice().to_owned();
    journal.pending_sparse_patterns = vec!["new".to_owned()];
    journal.baseline = Some(jj_lib::protos::local_working_copy::AwacsSnapshotBaseline {
        filesystem_uuid: vec![1; 16],
        subvolume_uuid: b"old-baseline".to_vec(),
        continuity_token: b"old-baseline".to_vec(),
        interpretation_input_fingerprint: vec![3; 32],
        ..Default::default()
    });
    journal.pending_baseline = Some(jj_lib::protos::local_working_copy::AwacsSnapshotBaseline {
        filesystem_uuid: vec![1; 16],
        subvolume_uuid: b"candidate".to_vec(),
        continuity_token: b"candidate".to_vec(),
        interpretation_input_fingerprint: vec![3; 32],
        ..Default::default()
    });
    journal.transition_id = b"interrupted-transition".to_vec();
    journal.mutation_kind = "checkout".to_owned();
    write_compact_working_copy_state(&state_path, &journal)?;

    let mut recovered = TreeState::load(
        repo.store().clone(),
        workspace_root,
        state_path.clone(),
        &settings,
    )?;
    assert_tree_eq!(*recovered.current_tree(), intended_tree);
    assert_eq!(recovered.sparse_patterns(), &vec![repo_path_buf("new")]);
    recovered.save()?;

    let recovered_journal = read_compact_working_copy_state(&state_path)?;
    assert_eq!(
        recovered_journal.phase(),
        jj_lib::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline
    );
    assert_eq!(
        recovered_journal.tree_ids,
        intended_tree
            .tree_ids()
            .iter()
            .map(|id| id.to_bytes())
            .collect_vec()
    );
    assert_eq!(
        recovered_journal.sparse_patterns.as_ref().unwrap().prefixes,
        vec!["new".to_owned()]
    );
    assert!(recovered_journal.fsmonitor_cursor.is_none());
    assert!(recovered_journal.baseline.is_none());
    assert!(recovered_journal.pending_baseline.is_none());
    assert!(recovered_journal.pending_tree_ids.is_empty());
    assert!(recovered_journal.pending_sparse_patterns.is_empty());
    Ok(())
}

#[test]
fn test_strict_subvolume_mode_recovers_pending_materialization_incrementally() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let base_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &base_settings,
    )?;

    let old_tree = create_tree(repo, &[(repo_path("old/file"), "old\n")]);
    let intended_tree = create_tree(repo, &[(repo_path("new/file"), "new\n")]);
    let mut journal = read_compact_working_copy_state(&state_path)?;
    journal.tree_ids = old_tree.tree_ids().iter().map(|id| id.to_bytes()).collect();
    journal.conflict_labels = old_tree.labels().as_slice().to_owned();
    journal.phase =
        jj_lib::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization as i32;
    journal.pending_tree_ids = intended_tree
        .tree_ids()
        .iter()
        .map(|id| id.to_bytes())
        .collect();
    journal.pending_conflict_labels = intended_tree.labels().as_slice().to_owned();
    journal.pending_baseline = Some(jj_lib::protos::local_working_copy::AwacsSnapshotBaseline {
        filesystem_uuid: vec![1; 16],
        subvolume_uuid: vec![2; 16],
        continuity_token: b"baseline-a".to_vec(),
        interpretation_input_fingerprint: vec![3; 32],
        ..Default::default()
    });
    journal.mutation_kind = "checkout".to_owned();
    write_compact_working_copy_state(&state_path, &journal)?;
    std::fs::write(state_path.join("subvolume_mode"), b"snapshot-backed\n")?;

    assert!(snapshot_mode_has_committed_baseline(&state_path)?);
    let exact_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root: workspace_root.clone(),
            // An empty exact delta proves that recovery did not fall back to
            // a full traversal of the empty live root, which would remove
            // old/file from the semantic tree.
            changed_files: Some(vec![]),
            cursor: b"baseline-b".to_vec(),
        },
        ..base_settings
    };
    let mut recovered = TreeState::load(
        repo.store().clone(),
        workspace_root,
        state_path.clone(),
        &exact_settings,
    )?;
    assert_tree_eq!(*recovered.current_tree(), old_tree);

    let durable_journal = read_compact_working_copy_state(&state_path)?;
    assert_eq!(
        durable_journal.phase(),
        jj_lib::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization
    );
    assert!(durable_journal.pending_baseline.is_some());

    let options = SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    };
    recovered.snapshot(&options).block_on()?;
    assert_tree_eq!(*recovered.current_tree(), old_tree);
    Ok(())
}

#[test]
fn test_legacy_checkout_missing_tree_state_reinitializes_legacy_state() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    let checkout = jj_lib::protos::local_working_copy::Checkout {
        operation_id: repo.op_id().to_bytes(),
        workspace_name: WorkspaceName::DEFAULT.as_str().to_owned(),
    };
    std::fs::write(state_path.join("checkout"), checkout.encode_to_vec())?;

    let wc = LocalWorkingCopy::load(
        repo.store().clone(),
        workspace_root,
        state_path.clone(),
        repo.settings(),
    )?;
    wc.tree()?;
    assert!(state_path.join("tree_state").is_file());
    let checkout = std::fs::read(state_path.join("checkout"))?;
    assert!(!checkout.starts_with(b"\0JJ-WORKING-COPY-STATE\0v1\n"));
    Ok(())
}

#[test]
fn test_enabled_subvolume_mode_requires_committed_baseline() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    std::fs::write(state_path.join("subvolume_mode"), b"enabling\n")?;
    let settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    let mut tree_state = TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    )?;
    tree_state.save()?;
    std::fs::write(state_path.join("subvolume_mode"), b"snapshot-backed\n")?;

    let err = match TreeState::load(repo.store().clone(), workspace_root, state_path, &settings) {
        Ok(_) => panic!("enabled subvolume mode without a baseline must fail"),
        Err(err) => err,
    };
    assert!(
        err.to_string()
            .contains("subvolume mode requires a committed AWACS snapshot baseline"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[test]
fn test_legacy_tree_state_retains_file_state_cache() -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    testutils::write_working_copy_file(&workspace_root, repo_path("tracked"), "contents\n");
    test_workspace.snapshot()?;

    let wc: &LocalWorkingCopy = test_workspace
        .workspace
        .working_copy()
        .downcast_ref()
        .unwrap();
    let bytes = std::fs::read(wc.state_path().join("tree_state"))?;
    let proto = jj_lib::protos::local_working_copy::TreeState::decode(bytes.as_slice())?;
    assert_eq!(proto.file_states.len(), 1);
    assert_eq!(proto.file_states[0].path, "tracked");
    Ok(())
}

#[test]
fn test_direct_tree_state_does_not_persist_awacs_cursor() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;

    let settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root,
            changed_files: None,
            cursor: b"cursor".to_vec(),
        },
        ..tree_state_settings.clone()
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    )?;
    let options = SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    };
    tree_state.snapshot(&options).block_on()?;
    tree_state.save()?;

    // Direct TreeState callers have no commit boundary for the accepted scan,
    // so the cursor must not survive a reload. Switching to no fsmonitor
    // would make the snapshot dirty if a cursor had been persisted.
    let mut reloaded_tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root,
        state_path,
        &tree_state_settings,
    )?;
    let (is_dirty, _stats) = reloaded_tree_state
        .snapshot(&empty_snapshot_options())
        .block_on()?;
    assert!(!is_dirty);
    Ok(())
}

#[test]
fn test_test_awacs_exact_delta_uses_retained_semantic_baseline() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let base_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root,
        state_path.clone(),
        &base_settings,
    )?;

    let changed_path = repo_path("dir/changed");
    let untouched_path = repo_path("dir/untouched");
    let dir_to_file_path = repo_path("transition");
    let old_child_path = repo_path("transition/child");
    testutils::write_working_copy_file(&scan_root, changed_path, "before\n");
    testutils::write_working_copy_file(&scan_root, untouched_path, "untouched\n");
    testutils::write_working_copy_file(&scan_root, old_child_path, "child\n");
    let full_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root: scan_root.clone(),
            changed_files: None,
            cursor: b"baseline-a".to_vec(),
        },
        ..base_settings.clone()
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        test_repo.env.root().join("workspace"),
        state_path.clone(),
        &full_settings,
    )?;
    let options = SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    };
    tree_state.snapshot(&options).block_on()?;
    tree_state.save()?;

    // Direct TreeState snapshots intentionally abort their lease. Seed the
    // compact test journal with the retained A binding that a locked working
    // copy would publish after a successful synthetic promotion.
    seed_test_awacs_baseline(&state_path, b"baseline-a", [3; 32])?;

    testutils::write_working_copy_file(&scan_root, changed_path, "after\n");
    std::fs::remove_file(old_child_path.to_fs_path_unchecked(&scan_root))?;
    std::fs::remove_dir(dir_to_file_path.to_fs_path_unchecked(&scan_root))?;
    testutils::write_working_copy_file(&scan_root, dir_to_file_path, "replacement\n");
    // If the exact delta accidentally scans siblings, this special path would
    // either be observed or fail the scan. The unchanged semantic value from A
    // must instead survive untouched.
    std::fs::remove_file(untouched_path.to_fs_path_unchecked(&scan_root))?;
    let exact_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root: scan_root.clone(),
            changed_files: Some(vec![
                changed_path.to_fs_path_unchecked(Path::new("")),
                dir_to_file_path.to_fs_path_unchecked(Path::new("")),
            ]),
            cursor: b"baseline-b".to_vec(),
        },
        ..base_settings
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        test_repo.env.root().join("workspace"),
        state_path,
        &exact_settings,
    )?;
    tree_state.snapshot(&options).block_on()?;
    let expected_tree = create_tree(
        repo,
        &[
            (changed_path, "after\n"),
            (untouched_path, "untouched\n"),
            (dir_to_file_path, "replacement\n"),
        ],
    );
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    Ok(())
}

#[test]
fn test_test_awacs_ignore_change_keeps_unchanged_untracked_paths_untracked() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let base_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &base_settings,
    )?;

    let gitignore_path = repo_path(".gitignore");
    let ignored_path = repo_path("old.ignored");
    testutils::write_working_copy_file(&scan_root, gitignore_path, "*.ignored\n");
    testutils::write_working_copy_file(&scan_root, ignored_path, "old untracked\n");
    // Seed A with the old path absent, which is the durable semantic fact we
    // need to preserve when only the ignore rule changes.
    let baseline_tree = create_tree(repo, &[(gitignore_path, "*.ignored\n")]);
    let options = SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &base_settings,
    )?;
    tree_state.reset(&baseline_tree).block_on()?;
    tree_state.save()?;
    seed_test_awacs_baseline(&state_path, b"baseline-a", [3; 32])?;

    // Removing the ignore rule must not discover and retroactively track the
    // old file. Only the changed .gitignore path is in the AWACS delta.
    testutils::write_working_copy_file(&scan_root, gitignore_path, "");
    let exact_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root,
            changed_files: Some(vec![gitignore_path.to_fs_path_unchecked(Path::new(""))]),
            cursor: b"baseline-b".to_vec(),
        },
        ..base_settings
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root,
        state_path,
        &exact_settings,
    )?;
    assert_tree_eq!(*tree_state.current_tree(), baseline_tree);
    tree_state.snapshot(&options).block_on()?;
    let expected_tree = create_tree(repo, &[(gitignore_path, "")]);
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    Ok(())
}

#[test]
fn test_test_awacs_committed_baseline_refuses_full_fallback() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let base_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &base_settings,
    )?;
    let options = SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    };
    seed_test_awacs_baseline(&state_path, b"baseline-a", [3; 32])?;
    let settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root,
            changed_files: None,
            cursor: b"baseline-b".to_vec(),
        },
        ..base_settings
    };
    let mut tree_state =
        TreeState::load(repo.store().clone(), workspace_root, state_path, &settings)?;
    let error = tree_state.snapshot(&options).block_on().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Snapshot-backed working copy refused a full scan")
    );
    Ok(())
}

#[test]
fn test_test_awacs_exact_delta_respects_sparse_patterns() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let base_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root,
        state_path.clone(),
        &base_settings,
    )?;

    let included_path = repo_path("included/keep");
    let outside_path = repo_path("outside/file");
    testutils::write_working_copy_file(&scan_root, included_path, "included\n");
    testutils::write_working_copy_file(&scan_root, outside_path, "before\n");
    let full_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root: scan_root.clone(),
            changed_files: None,
            cursor: b"baseline-a".to_vec(),
        },
        ..base_settings.clone()
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        test_repo.env.root().join("workspace"),
        state_path.clone(),
        &full_settings,
    )?;
    let options = SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    };
    tree_state.snapshot(&options).block_on()?;
    tree_state.save()?;

    let mut journal = read_compact_working_copy_state(&state_path)?;
    journal.sparse_patterns = Some(jj_lib::protos::local_working_copy::SparsePatterns {
        prefixes: vec!["included".to_owned()],
    });
    write_compact_working_copy_state(&state_path, &journal)?;
    seed_test_awacs_baseline(&state_path, b"baseline-a", [3; 32])?;

    testutils::write_working_copy_file(&scan_root, outside_path, "after\n");
    let exact_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root,
            changed_files: Some(vec![outside_path.to_fs_path_unchecked(Path::new(""))]),
            cursor: b"baseline-b".to_vec(),
        },
        ..base_settings
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        test_repo.env.root().join("workspace"),
        state_path,
        &exact_settings,
    )?;
    tree_state.snapshot(&options).block_on()?;
    let expected_tree = create_tree(
        repo,
        &[(included_path, "included\n"), (outside_path, "before\n")],
    );
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    Ok(())
}

#[test]
#[cfg(unix)]
fn test_test_awacs_exact_delta_rejects_symlink_ancestor() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let escaped_root = test_repo.env.root().join("escaped-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&escaped_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let base_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root,
        state_path.clone(),
        &base_settings,
    )?;

    let path = repo_path("dir/file");
    testutils::write_working_copy_file(&scan_root, path, "before\n");
    let full_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root: scan_root.clone(),
            changed_files: None,
            cursor: b"baseline-a".to_vec(),
        },
        ..base_settings.clone()
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        test_repo.env.root().join("workspace"),
        state_path.clone(),
        &full_settings,
    )?;
    let options = SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    };
    tree_state.snapshot(&options).block_on()?;
    tree_state.save()?;
    seed_test_awacs_baseline(&state_path, b"baseline-a", [3; 32])?;

    std::fs::remove_file(path.to_fs_path_unchecked(&scan_root))?;
    std::fs::remove_dir(scan_root.join("dir"))?;
    testutils::write_working_copy_file(&escaped_root, repo_path("file"), "escaped\n");
    std::os::unix::fs::symlink(&escaped_root, scan_root.join("dir"))?;
    let exact_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::TestAwacs {
            scan_root,
            changed_files: Some(vec![path.to_fs_path_unchecked(Path::new(""))]),
            cursor: b"baseline-b".to_vec(),
        },
        ..base_settings
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        test_repo.env.root().join("workspace"),
        state_path,
        &exact_settings,
    )?;
    let err = tree_state.snapshot(&options).block_on().unwrap_err();
    assert!(
        err.to_string().contains("non-directory ancestor"),
        "unexpected error: {err}"
    );
    let expected_tree = create_tree(repo, &[(path, "before\n")]);
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    Ok(())
}

#[test]
#[cfg(all(feature = "awacs", unix))]
fn test_awacs_library_client_uses_full_then_retained_prefix_and_aborts_direct_snapshot()
-> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;

    let path = repo_path("dir/file");
    let untouched_path = repo_path("untouched");
    testutils::write_working_copy_file(&workspace_root, path, "live\n");
    testutils::write_working_copy_file(&scan_root, path, "leased\n");
    testutils::write_working_copy_file(&scan_root, untouched_path, "untouched\n");
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::Awacs(AwacsConfig {
            client: Some(Arc::new(Mutex::new(Box::new(FakeAwacsClient {
                scan_root: scan_root.clone(),
                outcomes: outcomes.clone(),
                requests: requests.clone(),
                valid_scan_root: true,
            })))),
        }),
        ..tree_state_settings
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &settings,
    )?;
    let options = SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    };
    tree_state.snapshot(&options).block_on()?;
    tree_state.save()?;

    // No durable A is paired with X yet, so the first Prefixes response must
    // be widened to a full scan despite the synthetic backend's hint.
    let expected_tree = create_tree(repo, &[(path, "leased\n"), (untouched_path, "untouched\n")]);
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    assert_eq!(
        outcomes.lock().unwrap().as_slice(),
        &[btrfs_awacs::scan::ScanOutcome::Aborted]
    );
    assert_eq!(requests.lock().unwrap().as_slice(), &[None]);

    // Seed the clean A binding a locked working-copy finish would publish,
    // then prove the next production adapter scan accepts only the returned
    // prefix. Removing an unchanged sibling from B must not delete its X
    // value.
    let mut journal = read_compact_working_copy_state(&state_path)?;
    journal.phase = jj_lib::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline as i32;
    journal.baseline = Some(jj_lib::protos::local_working_copy::AwacsSnapshotBaseline {
        filesystem_uuid: vec![1; 16],
        subvolume_uuid: vec![2; 16],
        continuity_token: b"baseline".to_vec(),
        // Production AWACS v1 is prove-or-Full, not hard-pinned.
        retention_token: Vec::new(),
        interpretation_input_fingerprint: vec![3; 32],
    });
    write_compact_working_copy_state(&state_path, &journal)?;
    testutils::write_working_copy_file(&scan_root, path, "leased again\n");
    std::fs::remove_file(untouched_path.to_fs_path_unchecked(&scan_root))?;

    // A direct TreeState caller still aborts B after scanning, but it may use
    // an already-published A binding for this one directed scan.
    let mut reloaded_tree_state =
        TreeState::load(repo.store().clone(), workspace_root, state_path, &settings)?;
    reloaded_tree_state.snapshot(&options).block_on()?;
    let expected_tree = create_tree(
        repo,
        &[(path, "leased again\n"), (untouched_path, "untouched\n")],
    );
    assert_tree_eq!(*reloaded_tree_state.current_tree(), expected_tree);
    assert_eq!(
        requests.lock().unwrap().as_slice(),
        &[
            None,
            Some(btrfs_awacs::scan::SnapshotBaseline {
                identity: btrfs_awacs::scan::SnapshotIdentity {
                    filesystem_uuid: [1; 16],
                    subvolume_uuid: [2; 16],
                    read_only: true,
                },
                continuity_token: b"baseline".to_vec(),
                retention_token: Vec::new(),
            }),
        ]
    );
    assert_eq!(
        outcomes.lock().unwrap().as_slice(),
        &[
            btrfs_awacs::scan::ScanOutcome::Aborted,
            btrfs_awacs::scan::ScanOutcome::Aborted,
        ]
    );
    Ok(())
}

#[test]
#[cfg(all(feature = "awacs", unix))]
fn test_awacs_rejected_scan_root_aborts_accepted_lease() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::Awacs(AwacsConfig {
            client: Some(Arc::new(Mutex::new(Box::new(FakeAwacsClient {
                scan_root,
                outcomes: outcomes.clone(),
                requests: Arc::new(Mutex::new(Vec::new())),
                valid_scan_root: false,
            })))),
        }),
        ..tree_state_settings
    };
    let mut tree_state =
        TreeState::load(repo.store().clone(), workspace_root, state_path, &settings)?;
    let options = SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    };
    let error = tree_state.snapshot(&options).block_on().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Failed to validate AWACS scan root")
    );
    assert_eq!(
        outcomes.lock().unwrap().as_slice(),
        &[btrfs_awacs::scan::ScanOutcome::Aborted]
    );
    Ok(())
}

#[test]
#[cfg(all(feature = "awacs", unix))]
fn test_awacs_baseline_input_mismatch_forces_full_begin() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let scan_root = test_repo.env.root().join("scan-root");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&scan_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    let mut tree_state = TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;
    tree_state.save()?;
    let mut journal = read_compact_working_copy_state(&state_path)?;
    journal.phase = jj_lib::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline as i32;
    journal.baseline = Some(jj_lib::protos::local_working_copy::AwacsSnapshotBaseline {
        filesystem_uuid: vec![1; 16],
        subvolume_uuid: vec![2; 16],
        continuity_token: b"stale-baseline".to_vec(),
        retention_token: Vec::new(),
        interpretation_input_fingerprint: vec![9; 32],
    });
    write_compact_working_copy_state(&state_path, &journal)?;

    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::Awacs(AwacsConfig {
            client: Some(Arc::new(Mutex::new(Box::new(FakeAwacsClient {
                scan_root,
                outcomes,
                requests: requests.clone(),
                valid_scan_root: true,
            })))),
        }),
        ..tree_state_settings
    };
    let mut tree_state =
        TreeState::load(repo.store().clone(), workspace_root, state_path, &settings)?;
    let options = SnapshotOptions {
        awacs_input_fingerprint: Some([3; 32]),
        ..empty_snapshot_options()
    };
    let (is_dirty, _stats) = tree_state.snapshot(&options).block_on()?;

    assert!(is_dirty);
    assert_eq!(requests.lock().unwrap().as_slice(), &[None]);
    Ok(())
}

#[test]
fn test_fsmonitor_cursor_cleared_by_sparse_change() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    enable_snapshot_mode(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    let empty_tree = repo.store().empty_merged_tree();
    write_legacy_tree_state(&state_path, &empty_tree, |proto| {
        proto.fsmonitor_cursor = Some(jj_lib::protos::local_working_copy::FsmonitorCursor {
            cursor: Some(
                jj_lib::protos::local_working_copy::fsmonitor_cursor::Cursor::Watchman(
                    jj_lib::protos::local_working_copy::WatchmanClock {
                        watchman_clock: Some(
                            jj_lib::protos::local_working_copy::watchman_clock::WatchmanClock::StringClock(
                                "cursor".to_owned(),
                            ),
                        ),
                    },
                ),
            ),
        });
    })?;

    let watchman_settings = TreeStateSettings {
        fsmonitor_settings: FsmonitorSettings::Watchman(WatchmanConfig {
            register_trigger: false,
        }),
        ..tree_state_settings.clone()
    };
    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &watchman_settings,
    )?;

    // A no-op sparse update keeps the cursor paired with the same tree state.
    tree_state.set_sparse_patterns(vec![RepoPathBuf::root()])?;
    tree_state.save()?;

    // Watchman clocks are not retained immutable baselines, so migration
    // already writes NoBaseline and a backend switch has nothing to clear.
    let mut probe = TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;
    let (is_dirty, _stats) = probe.snapshot(&empty_snapshot_options()).block_on()?;
    assert!(!is_dirty);

    let mut tree_state = TreeState::load(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &watchman_settings,
    )?;
    tree_state.set_sparse_patterns(vec![repo_path_buf("dir")])?;
    tree_state.save()?;

    // A changed sparse vector clears the cursor durably, so the same public
    // backend-mismatch probe is now clean.
    let mut probe = TreeState::load(
        repo.store().clone(),
        workspace_root,
        state_path,
        &tree_state_settings,
    )?;
    let (is_dirty, _stats) = probe.snapshot(&empty_snapshot_options()).block_on()?;
    assert!(!is_dirty);
    Ok(())
}

#[test]
fn track_ignored_with_flag_and_fsmonitor() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;

    let ignored_path = repo_path("file.ignored");
    let gitignore_path = repo_path(".gitignore");
    testutils::write_working_copy_file(&workspace_root, ignored_path, "contents\n");
    testutils::write_working_copy_file(&workspace_root, gitignore_path, "*.ignored\n");

    let snapshot = |paths: &[&RepoPath], matcher: Option<&FilesMatcher>| {
        let changed_files = paths
            .iter()
            .map(|p| p.to_fs_path_unchecked(Path::new("")))
            .collect();
        let settings = TreeStateSettings {
            fsmonitor_settings: FsmonitorSettings::Test {
                changed_files,
                scan_root: None,
            },
            ..tree_state_settings.clone()
        };
        let mut tree_state = TreeState::load(
            repo.store().clone(),
            workspace_root.clone(),
            state_path.clone(),
            &settings,
        )
        .unwrap();
        let mut options = empty_snapshot_options();
        if let Some(matcher) = matcher {
            options.force_tracking_matcher = matcher;
        }
        tree_state.snapshot(&options).block_on().unwrap();
        tree_state.save().unwrap();
        tree_state
    };

    let tree_state = snapshot(&[], None);
    let expected_tree = create_tree(repo, &[(gitignore_path, "*.ignored\n")]);
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);

    let tree_state = snapshot(&[ignored_path], None);
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);

    // Simulate `jj file track --include-ignored`.
    let force_tracking_matcher = FilesMatcher::new([ignored_path]);
    let tree_state = snapshot(&[], Some(&force_tracking_matcher));

    let expected_tree = create_tree(
        repo,
        &[
            (gitignore_path, "*.ignored\n"),
            (ignored_path, "contents\n"),
        ],
    );
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    Ok(())
}

#[test]
fn fsmonitor_gitignore_rescan_subtree() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let workspace_root = test_repo.env.root().join("workspace");
    let state_path = test_repo.env.root().join("state");
    std::fs::create_dir(&workspace_root)?;
    std::fs::create_dir(&state_path)?;
    let tree_state_settings = TreeStateSettings::try_from_user_settings(repo.settings())?;
    TreeState::init(
        repo.store().clone(),
        workspace_root.clone(),
        state_path.clone(),
        &tree_state_settings,
    )?;

    let ignored_path = repo_path("file.ignored");
    let gitignore_path = repo_path(".gitignore");
    testutils::write_working_copy_file(&workspace_root, ignored_path, "contents\n");
    testutils::write_working_copy_file(&workspace_root, gitignore_path, "*.ignored\n");

    let snapshot = |paths: &[&RepoPath]| {
        let changed_files = paths
            .iter()
            .map(|p| p.to_fs_path_unchecked(Path::new("")))
            .collect();
        let settings = TreeStateSettings {
            fsmonitor_settings: FsmonitorSettings::Test {
                changed_files,
                scan_root: None,
            },
            ..tree_state_settings.clone()
        };
        let mut tree_state = TreeState::load(
            repo.store().clone(),
            workspace_root.clone(),
            state_path.clone(),
            &settings,
        )
        .unwrap();
        tree_state
            .snapshot(&empty_snapshot_options())
            .block_on()
            .unwrap();
        tree_state.save().unwrap();
        tree_state
    };

    let tree_state = snapshot(&[gitignore_path, ignored_path]);
    let expected_tree = create_tree(repo, &[(gitignore_path, "*.ignored\n")]);
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);

    testutils::write_working_copy_file(&workspace_root, gitignore_path, "");
    let tree_state = snapshot(&[gitignore_path]);
    let expected_tree = create_tree(repo, &[(gitignore_path, ""), (ignored_path, "contents\n")]);
    assert_tree_eq!(*tree_state.current_tree(), expected_tree);
    Ok(())
}

#[test]
fn test_snapshot_max_new_file_size() -> TestResult {
    let mut test_workspace = TestWorkspace::init();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    let small_path = repo_path("small");
    let large_path = repo_path("large");
    let limit: usize = 1024;
    std::fs::write(
        small_path.to_fs_path_unchecked(&workspace_root),
        vec![0; limit],
    )?;
    let options = SnapshotOptions {
        max_new_file_size: limit as u64,
        ..empty_snapshot_options()
    };
    test_workspace
        .snapshot_with_options(&options)
        .expect("files exactly matching the size limit should succeed");
    std::fs::write(
        small_path.to_fs_path_unchecked(&workspace_root),
        vec![0; limit + 1],
    )?;
    let (old_tree, _stats) = test_workspace
        .snapshot_with_options(&options)
        .expect("existing files may grow beyond the size limit");

    // A new file of 1KiB + 1 bytes should be left untracked
    std::fs::write(
        large_path.to_fs_path_unchecked(&workspace_root),
        vec![0; limit + 1],
    )?;
    let (new_tree, stats) = test_workspace
        .snapshot_with_options(&options)
        .expect("snapshot should not fail because of new files beyond the size limit");
    assert_tree_eq!(new_tree, old_tree);
    assert_eq!(
        stats
            .untracked_paths
            .keys()
            .map(AsRef::as_ref)
            .collect_vec(),
        [large_path]
    );
    assert_matches!(
        stats.untracked_paths.values().next().unwrap(),
        UntrackedReason::FileTooLarge { size, .. } if *size == (limit as u64) + 1
    );

    // A file in sub directory should also be caught
    let sub_large_path = repo_path("sub/large");
    std::fs::create_dir(
        sub_large_path
            .parent()
            .unwrap()
            .to_fs_path_unchecked(&workspace_root),
    )?;
    std::fs::rename(
        large_path.to_fs_path_unchecked(&workspace_root),
        sub_large_path.to_fs_path_unchecked(&workspace_root),
    )?;
    let (new_tree, stats) = test_workspace
        .snapshot_with_options(&options)
        .expect("snapshot should not fail because of new files beyond the size limit");
    assert_tree_eq!(new_tree, old_tree);
    assert_eq!(
        stats
            .untracked_paths
            .keys()
            .map(AsRef::as_ref)
            .collect_vec(),
        [sub_large_path]
    );
    assert_matches!(
        stats.untracked_paths.values().next().unwrap(),
        UntrackedReason::FileTooLarge { .. }
    );
    Ok(())
}

#[test]
fn test_snapshot_symlink_use_forward_slash() -> TestResult {
    if !file_util::check_symlink_support()? {
        eprintln!("Symlink not supported. Skip the test.");
    }
    let mut test_workspace = TestWorkspace::init();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    let target = repo_path("target/link/target.txt");
    let target_path = target.to_fs_path(&workspace_root)?;
    std::fs::create_dir_all(target_path.parent().unwrap())?;
    std::fs::write(&target_path, "a\n")?;
    let link = repo_path("link/link.txt");
    let link_path = link.to_fs_path(&workspace_root)?;
    let link_contents = "../target/link/target.txt";
    std::fs::create_dir_all(link_path.parent().unwrap())?;
    symlink_file(link_contents, link_path)?;

    let tree = test_workspace
        .snapshot()
        .expect("Snapshot with symlink should succeed.");
    let tree_value = tree
        .path_value(link)
        .block_on()
        .expect("Failed to retrieve the MergedTreeValue from the path.")
        .into_resolved()
        .expect("Shouldn't have conflicts.")
        .expect("The link path should exist.");
    let TreeValue::Symlink(symlink_id) = tree_value.clone() else {
        panic!(
            "Expect {} to be a symlink, but got {:?}",
            link.as_internal_file_string(),
            tree_value
        );
    };
    let actual_link_contents = test_workspace
        .repo
        .store()
        .read_symlink(link, &symlink_id)
        .block_on()?;

    assert!(
        !actual_link_contents.contains('\\'),
        r#"Expect the symlink in the Store to use "/" as the separator, but got {actual_link_contents}."#
    );
    Ok(())
}

fn is_verbatim_path(path: &Path) -> bool {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    prefix.kind().is_verbatim()
}

#[cfg(windows)]
fn absolute_path_to_verbatim_path(input: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::path::Prefix;

    use bstr::ByteSlice as _;

    assert!(input.is_absolute());
    let input = input.canonicalize().unwrap();

    let mut components = input.components();
    let Component::Prefix(prefix_component) = components.next().unwrap() else {
        panic!("target should be an absolute path after being canonicalized");
    };
    let mut verbatim_path = match prefix_component.kind() {
        // C: -> \\?\Global\C:
        // \\?\C: -> \\?\Global\C:
        //
        // Prefix the path with `Global`, so that when we read back the symlink, it's still a
        // verbatim path. The symlink to a `\\?\C:` prefixed path (e.g. `\\?\C:\file.txt`)
        // will be converted to a non-verbatim path (e.g. `C:\file.txt`) when calling
        // `read_link()`.
        Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => {
            let mut verbatim_prefix = OsString::from(r"\\?\Global\");
            verbatim_prefix.push([disk].to_os_str().unwrap());
            verbatim_prefix.push(":");
            verbatim_prefix
        }
        _ => panic!("Unsupported path: {}", input.display()),
    };
    verbatim_path.push(components.as_path().as_os_str());
    let verbatim_path = PathBuf::from(verbatim_path);
    assert!(is_verbatim_path(&verbatim_path));
    verbatim_path
}

#[test_case(|link, target| file_util::relative_path(link.parent().unwrap(), target); "relative")]
#[test_case(|_, target| {
    assert!(target.is_absolute());
    target.to_owned()
}; "absolute")]
#[cfg_attr(
    windows,
    test_case(|_, target: &Path| absolute_path_to_verbatim_path(target); "verbatim absolute")
)]
fn test_snapshot_and_update_valid_symlink(
    get_link_target: impl FnOnce(&Path, &Path) -> PathBuf,
) -> TestResult {
    if !file_util::check_symlink_support()? {
        eprintln!("Symlink not supported. Skip the test.");
    }
    let mut test_workspace = TestWorkspace::init();
    let workspace_root = test_workspace.workspace.workspace_root().to_owned();
    let target = repo_path("target/link/target.txt");
    let target_path = target.to_fs_path(&workspace_root)?;
    std::fs::create_dir_all(target_path.parent().unwrap())?;
    // Unique contents that it's unlikely that we match accidentally.
    let file_contents = b"18bHZD165T@C\n";
    std::fs::write(&target_path, file_contents)?;
    let link = repo_path("link/link.txt");
    let link_path = link.to_fs_path(&workspace_root)?;
    let link_contents = get_link_target(&link_path, &target_path);
    std::fs::create_dir_all(link_path.parent().unwrap())?;
    symlink_file(&link_contents, &link_path)?;
    std::fs::read_link(&link_path).expect("The symlink itself should exist.");
    assert_eq!(std::fs::read(&link_path)?, file_contents);
    assert_eq!(
        is_verbatim_path(&std::fs::read_link(&link_path)?),
        is_verbatim_path(&link_contents),
        "Make sure that when we test with a verbatim path, it's still a verbatim path in the \
         Store when snapshotting."
    );

    let tree = test_workspace
        .snapshot()
        .expect("Snapshot with symlink should succeed.");
    let commit = commit_with_tree(test_workspace.repo.store(), tree);

    // Checkout the root commit to clear the workspace.
    let mut locked_ws = test_workspace
        .workspace
        .start_working_copy_mutation()
        .block_on()?;
    let root_commit = test_workspace.repo.store().root_commit();
    locked_ws.locked_wc().check_out(&root_commit).block_on()?;
    locked_ws
        .finish(test_workspace.repo.op_id().clone())
        .block_on()?;

    assert!(!std::fs::exists(&link_path)?);
    assert!(std::fs::read_link(&link_path).is_err());

    // Checkout the original commit back.
    let mut locked_ws = test_workspace
        .workspace
        .start_working_copy_mutation()
        .block_on()?;
    locked_ws.locked_wc().check_out(&commit).block_on()?;
    locked_ws
        .finish(test_workspace.repo.op_id().clone())
        .block_on()?;

    let actual_target = std::fs::read_link(&link_path).expect("The symlink itself should exist.");
    let actual_contents = std::fs::read(&link_path).unwrap_or_else(|e| {
        panic!(
            "Failed to read from the symlink at {}, which points to {}: {e:?}",
            link_path.display(),
            actual_target.display()
        )
    });
    assert_eq!(actual_contents, file_contents);
    assert_eq!(
        is_verbatim_path(&std::fs::read_link(&link_path)?),
        is_verbatim_path(&link_contents),
        "When we checkout a symlink to a verbatim path, it should still point to a verbatim path."
    );
    Ok(())
}

#[test]
fn test_always_store_empty_tree() -> TestResult {
    let mut test_workspace = TestWorkspace::init_with_backend(TestRepoBackend::Git);
    let git_backend = get_git_backend(test_workspace.repo.store())?;
    let git_repo = git_backend.git_repo();
    let empty_tree_id = gix::ObjectId::empty_tree(gix::hash::Kind::Sha1);

    test_workspace.snapshot()?;

    let mut buf = Vec::new();
    // Use objects.find as it doesn't short-circuit when asked for the empty tree
    let (empty_tree, _) = git_repo
        .objects
        .find(&empty_tree_id, &mut buf)
        .expect("empty tree should be stored in the git repo");
    assert_eq!(empty_tree.kind, gix::objs::Kind::Tree);
    assert!(empty_tree.data.is_empty());
    Ok(())
}
