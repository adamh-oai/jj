// Copyright 2021 The Jujutsu Authors
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

use std::assert_matches;
use std::thread;

use jj_lib::default_backend_factories::default_working_copy_factories;
use jj_lib::default_backend_factories::default_working_copy_factory;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;
use jj_lib::workspace::Workspace;
use jj_lib::workspace::WorkspaceInitError;
use jj_lib::workspace::WorkspaceLoadError;
use pollster::FutureExt as _;
use testutils::TestEnvironment;
use testutils::TestResult;
use testutils::TestWorkspace;

#[test]
fn test_load_bad_path() {
    let settings = testutils::user_settings();
    let test_env = TestEnvironment::init();
    let workspace_root = test_env.root().to_owned();
    // We haven't created a repo in the workspace_root, so it should fail to load.
    let result = Workspace::load(
        &settings,
        &workspace_root,
        &test_env.default_backend_factories(),
        &default_working_copy_factories(),
    );
    assert_matches!(
        result.err(),
        Some(WorkspaceLoadError::NoWorkspaceHere(root)) if root == workspace_root
    );
}

#[test]
fn test_init_additional_workspace() -> TestResult {
    let settings = testutils::user_settings();
    let test_workspace = TestWorkspace::init_with_settings(&settings);
    let workspace = &test_workspace.workspace;

    let ws2_name = WorkspaceNameBuf::from("ws2");
    let ws2_root = test_workspace.root_dir().join("ws2_root");
    std::fs::create_dir(&ws2_root)?;
    let (ws2, repo) = Workspace::init_workspace_with_existing_repo(
        &ws2_root,
        test_workspace.repo_path(),
        &test_workspace.repo,
        &*default_working_copy_factory(),
        ws2_name.clone(),
    )
    .block_on()?;
    let wc_commit_id = repo.view().get_wc_commit_id(&ws2_name);
    assert_ne!(wc_commit_id, None);
    let wc_commit_id = wc_commit_id.unwrap();
    let wc_commit = repo.store().get_commit(wc_commit_id)?;
    assert_eq!(
        wc_commit.parent_ids(),
        vec![repo.store().root_commit_id().clone()]
    );
    assert_eq!(ws2.workspace_name(), &ws2_name);
    assert_eq!(
        *ws2.repo_path(),
        dunce::canonicalize(workspace.repo_path())?
    );
    assert_eq!(*ws2.workspace_root(), dunce::canonicalize(&ws2_root)?);

    let repo_file_path = ws2_root.join(".jj").join("repo");
    let repo_file_contents = std::fs::read(&repo_file_path)?;
    let stored_path = String::from_utf8(repo_file_contents)?;
    assert_eq!(stored_path, "../../repo/.jj/repo");

    let same_workspace = Workspace::load(
        &settings,
        &ws2_root,
        &test_workspace.env.default_backend_factories(),
        &default_working_copy_factories(),
    );
    assert!(same_workspace.is_ok());
    let same_workspace = same_workspace?;
    assert_eq!(same_workspace.workspace_name(), &ws2_name);
    assert_eq!(
        *same_workspace.repo_path(),
        dunce::canonicalize(workspace.repo_path())?
    );
    assert_eq!(same_workspace.workspace_root(), ws2.workspace_root());
    Ok(())
}

#[test]
fn test_init_registered_working_copy() -> TestResult {
    let settings = testutils::user_settings();
    let test_workspace = TestWorkspace::init_with_settings(&settings);
    let ws2_name = WorkspaceNameBuf::from("ws2");
    let tree = testutils::create_tree(
        &test_workspace.repo,
        &[(RepoPath::from_internal_string("file")?, "target contents\n")],
    );
    let mut tx = test_workspace.repo.start_transaction();
    let target = tx
        .repo_mut()
        .new_commit(
            vec![test_workspace.repo.store().root_commit_id().clone()],
            tree,
        )
        .write()
        .block_on()?;
    let wc_commit = tx
        .repo_mut()
        .check_out(ws2_name.clone(), &target)
        .block_on()?;
    let repo = tx.commit("register workspace at target").block_on()?;
    let heads = repo.view().heads().clone();

    let ws2_root = test_workspace.root_dir().join("ws2_root");
    std::fs::create_dir(&ws2_root)?;
    std::fs::write(ws2_root.join("file"), "local changes\n")?;
    std::fs::create_dir(ws2_root.join(".git"))?;
    std::fs::write(ws2_root.join(".git/index"), "existing Git index\n")?;
    let ws2 = Workspace::init_workspace_with_registered_working_copy(
        &ws2_root,
        test_workspace.repo_path(),
        &repo,
        &*default_working_copy_factory(),
        ws2_name.clone(),
    )?;

    assert_eq!(
        repo.view().get_wc_commit_id(&ws2_name),
        Some(wc_commit.id())
    );
    assert_eq!(wc_commit.parent_ids(), vec![target.id().clone()]);
    assert_eq!(repo.view().heads(), &heads);
    assert_eq!(
        repo.loader().load_at_head().block_on()?.op_id(),
        repo.op_id()
    );
    assert_eq!(ws2.working_copy().operation_id(), repo.op_id());
    assert_eq!(ws2.workspace_name(), &ws2_name);
    assert_eq!(
        repo.loader()
            .workspace_store()
            .get_workspace_path(&ws2_name)?,
        Some(dunce::canonicalize(&ws2_root)?)
    );
    let reloaded = Workspace::load(
        &settings,
        &ws2_root,
        &test_workspace.env.default_backend_factories(),
        &default_working_copy_factories(),
    )?;
    assert_eq!(reloaded.working_copy().operation_id(), repo.op_id());
    assert_eq!(
        std::fs::read_to_string(ws2_root.join("file"))?,
        "local changes\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws2_root.join(".git/index"))?,
        "existing Git index\n"
    );
    Ok(())
}

#[test]
fn test_init_registered_working_copy_rejects_invalid_destination() -> TestResult {
    let test_workspace = TestWorkspace::init();
    let ws2_root = test_workspace.root_dir().join("ws2_root");
    std::fs::create_dir(&ws2_root)?;
    std::fs::write(ws2_root.join("file"), "existing file\n")?;
    let missing_name = WorkspaceNameBuf::from("missing");
    let result = Workspace::init_workspace_with_registered_working_copy(
        &ws2_root,
        test_workspace.repo_path(),
        &test_workspace.repo,
        &*default_working_copy_factory(),
        missing_name.clone(),
    );
    assert_matches!(
        result.err(),
        Some(WorkspaceInitError::MissingWorkingCopyCommit(name)) if name == missing_name
    );
    assert!(!ws2_root.join(".jj").exists());

    std::fs::create_dir(ws2_root.join(".jj"))?;
    std::fs::write(ws2_root.join(".jj/sentinel"), "existing metadata\n")?;
    let result = Workspace::init_workspace_with_registered_working_copy(
        &ws2_root,
        test_workspace.repo_path(),
        &test_workspace.repo,
        &*default_working_copy_factory(),
        test_workspace.workspace.workspace_name().to_owned(),
    );
    assert_matches!(
        result.err(),
        Some(WorkspaceInitError::DestinationExists(path)) if path == ws2_root.join(".jj")
    );
    assert_eq!(
        std::fs::read_to_string(ws2_root.join(".jj/sentinel"))?,
        "existing metadata\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws2_root.join("file"))?,
        "existing file\n"
    );
    Ok(())
}

#[test]
fn test_init_registered_working_copy_removes_partial_metadata() -> TestResult {
    let test_workspace = TestWorkspace::init();
    let ws2_root = test_workspace.root_dir().join("ws2_root");
    std::fs::create_dir(&ws2_root)?;
    std::fs::write(ws2_root.join("file"), "existing file\n")?;
    std::fs::create_dir(ws2_root.join(".git"))?;
    std::fs::write(ws2_root.join(".git/index"), "existing Git index\n")?;

    // Fail the workspace-store update after the factory has initialized its
    // state. Only metadata created by this call may be removed.
    let store_path = test_workspace.repo_path().join("workspace_store/index");
    let original_store = std::fs::read(&store_path)?;
    std::fs::write(&store_path, b"\xff")?;
    let result = Workspace::init_workspace_with_registered_working_copy(
        &ws2_root,
        test_workspace.repo_path(),
        &test_workspace.repo,
        &*default_working_copy_factory(),
        test_workspace.workspace.workspace_name().to_owned(),
    );
    std::fs::write(&store_path, original_store)?;
    assert_matches!(result.err(), Some(WorkspaceInitError::WorkspaceStore(_)));
    assert!(!ws2_root.join(".jj").exists());
    assert_eq!(
        std::fs::read_to_string(ws2_root.join("file"))?,
        "existing file\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws2_root.join(".git/index"))?,
        "existing Git index\n"
    );
    Ok(())
}

#[test]
fn test_init_additional_workspace_absolute_path_compat() -> TestResult {
    let settings = testutils::user_settings();
    let test_workspace = TestWorkspace::init_with_settings(&settings);
    let workspace = &test_workspace.workspace;

    let ws2_name = WorkspaceNameBuf::from("ws2");
    let ws2_root = test_workspace.root_dir().join("ws2_root");
    std::fs::create_dir(&ws2_root)?;
    let (ws2, _repo) = Workspace::init_workspace_with_existing_repo(
        &ws2_root,
        test_workspace.repo_path(),
        &test_workspace.repo,
        &*default_working_copy_factory(),
        ws2_name.clone(),
    )
    .block_on()?;

    let repo_file_path = ws2_root.join(".jj").join("repo");
    let abs_repo_path = dunce::canonicalize(workspace.repo_path())?;
    std::fs::write(
        &repo_file_path,
        abs_repo_path.as_os_str().as_encoded_bytes(),
    )?;

    let same_workspace = Workspace::load(
        &settings,
        &ws2_root,
        &test_workspace.env.default_backend_factories(),
        &default_working_copy_factories(),
    );
    assert!(same_workspace.is_ok());
    let same_workspace = same_workspace?;
    assert_eq!(same_workspace.workspace_name(), &ws2_name);
    assert_eq!(*same_workspace.repo_path(), abs_repo_path);
    assert_eq!(same_workspace.workspace_root(), ws2.workspace_root());
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_init_additional_workspace_non_utf8_path() -> TestResult {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let settings = testutils::user_settings();
    let test_env = TestEnvironment::init();

    if testutils::check_strict_utf8_fs(test_env.root()) {
        eprintln!(
            "Skipping test \"test_init_additional_workspace_non_utf8_path\" due to strict UTF-8 \
             filesystem for path {:?}",
            test_env.root()
        );
        return Ok(());
    }

    let ws1_root = test_env.root().join(OsStr::from_bytes(b"ws1_root\xe0"));
    std::fs::create_dir(&ws1_root)?;
    let (ws1, repo) = Workspace::init_simple(&settings, &ws1_root).block_on()?;

    let ws2_name = WorkspaceNameBuf::from("ws2");
    let ws2_root = test_env.root().join(OsStr::from_bytes(b"ws2_root\xe0"));
    std::fs::create_dir(&ws2_root)?;
    let (ws2, _repo) = Workspace::init_workspace_with_existing_repo(
        &ws2_root,
        ws1.repo_path(),
        &repo,
        &*default_working_copy_factory(),
        ws2_name.clone(),
    )
    .block_on()?;
    assert_eq!(ws2.workspace_name(), &ws2_name);
    assert_eq!(*ws2.repo_path(), dunce::canonicalize(ws1.repo_path())?);
    assert_eq!(*ws2.workspace_root(), dunce::canonicalize(&ws2_root)?);
    let same_workspace = Workspace::load(
        &settings,
        &ws2_root,
        &test_env.default_backend_factories(),
        &default_working_copy_factories(),
    );
    let same_workspace = same_workspace?;
    assert_eq!(same_workspace.workspace_name(), &ws2_name);
    assert_eq!(
        *same_workspace.repo_path(),
        dunce::canonicalize(ws1.repo_path())?
    );
    assert_eq!(same_workspace.workspace_root(), ws2.workspace_root());
    Ok(())
}

/// Test cross-thread access to a workspace, which requires it to be Send
#[test]
fn test_sendable() {
    let test_workspace = TestWorkspace::init();
    let root = test_workspace.workspace.workspace_root().to_owned();

    thread::spawn(move || {
        let shared_workspace = test_workspace.workspace;
        assert_eq!(shared_workspace.workspace_root(), &root);
    })
    .join()
    .unwrap();
}
