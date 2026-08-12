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

use std::fs;
use std::path::Path;
use std::process::Command;

use jj_lib::file_util::IoResultExt as _;
use jj_lib::git;
use jj_lib::local_working_copy::LockedLocalWorkingCopy;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::workspace_store::SimpleWorkspaceStore;
use jj_lib::workspace_store::WorkspaceStore as _;
use tracing::instrument;

use super::add::GitWorktreePlan;
use super::add::create_git_worktree;
use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
use crate::ui::Ui;

/// Adopt a filesystem copy of the current workspace as a new workspace.
///
/// This is useful after copying a workspace with a filesystem snapshot. The
/// copied workspace initially has the same identity as the source workspace.
/// This command assigns it a new identity without rewriting its files.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspaceAdoptArgs {
    /// A name for the adopted workspace
    ///
    /// To override the default, which is the basename of the workspace root.
    #[arg(long)]
    name: Option<WorkspaceNameBuf>,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_adopt(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspaceAdoptArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper_no_snapshot(ui).await?;
    let workspace_root = workspace_command.workspace().workspace_root().to_owned();
    let repo_path = workspace_command.repo_path().to_owned();
    let source_workspace_name = workspace_command.working_copy().workspace_name().to_owned();
    let source_operation_id = workspace_command.working_copy().operation_id().clone();
    let workspace_name = if let Some(name) = &args.name {
        name.to_owned()
    } else {
        workspace_root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| user_error("Workspace root name is not valid UTF-8"))?
            .into()
    };
    if workspace_name.as_str().is_empty() {
        return Err(user_error("New workspace name cannot be empty"));
    }
    if workspace_name == source_workspace_name {
        return Err(user_error(
            "New workspace name must differ from the copied workspace name",
        ));
    }

    // A directory here means that the whole repository was copied, not just a
    // workspace attached to a shared repository. In that case there is no
    // shared repository in which to register the adopted workspace.
    if workspace_root.join(".jj").join("repo").is_dir() {
        return Err(user_error(
            "Cannot adopt a copy containing its own .jj/repo directory",
        )
        .hinted(
            "Copy a linked workspace whose .jj/repo is a file pointing to the shared repository.",
        ));
    }

    let workspace_store = SimpleWorkspaceStore::load(&repo_path)?;
    let recorded_path = workspace_store
        .get_workspace_path(&source_workspace_name)?
        .ok_or_else(|| {
            user_error("Cannot determine the original workspace path")
                .hinted("The source workspace has no path recorded in the workspace store.")
        })?;
    let recorded_root = repo_path.join(recorded_path);
    if dunce::canonicalize(&recorded_root).ok() == Some(workspace_root.clone()) {
        return Err(user_error(
            "Cannot adopt the original workspace; this command expects a filesystem copy",
        ));
    }
    if workspace_command
        .repo()
        .view()
        .get_wc_commit_id(&workspace_name)
        .is_some()
    {
        return Err(user_error(format!(
            "Workspace named '{name}' already exists",
            name = workspace_name.as_symbol()
        )));
    }

    // Resolve the source working-copy commit from the operation captured in
    // the copied checkout state, not from the source workspace's current
    // position. The source may have advanced since the filesystem copy.
    let source_operation = workspace_command
        .workspace()
        .repo_loader()
        .load_operation(&source_operation_id)
        .await?;
    let source_repo = workspace_command
        .workspace()
        .repo_loader()
        .load_at(&source_operation)
        .await?;
    let source_commit_id = source_repo
        .view()
        .get_wc_commit_id(&source_workspace_name)
        .ok_or_else(|| {
            user_error(format!(
                "Copied workspace '{}' was not tracked at its recorded operation",
                source_workspace_name.as_symbol()
            ))
        })?;
    let source_commit = workspace_command
        .repo()
        .store()
        .get_commit_async(source_commit_id)
        .await?;
    if workspace_command
        .working_copy()
        .tree()?
        .tree_ids_and_labels()
        != source_commit.tree().tree_ids_and_labels()
    {
        return Err(user_error(
            "Copied working-copy metadata does not match its recorded commit",
        ));
    }

    let git_worktree_plan =
        if crate::git_util::is_colocated_git_workspace(workspace_command.workspace()) {
            let git_backend =
                git::get_git_backend(workspace_command.repo().store()).map_err(|_| {
                    internal_error_with_message(
                        "Colocated workspace does not use a Git-backed repository",
                        "missing Git backend",
                    )
                })?;
            let git_repo_path = dunce::canonicalize(git_backend.git_repo_path())
                .unwrap_or_else(|_| git_backend.git_repo_path().to_owned());
            Some(GitWorktreePlan {
                git_repo_path,
                git_executable: git_backend.git_executable_path().to_owned(),
                checkout_commit_id: source_commit.id().clone(),
            })
        } else {
            None
        };
    let repo = workspace_command.repo().clone();

    // Lock the copied checkout before modifying either its identity or the
    // shared repo. We intentionally don't snapshot it: dirty files in the
    // filesystem copy should become changes in the new workspace later.
    let (mut locked_ws, _current_source_commit) = workspace_command
        .unchecked_start_working_copy_mutation()
        .await?;
    if locked_ws.locked_wc().old_operation_id() != &source_operation_id
        || locked_ws.locked_wc().old_tree().tree_ids_and_labels()
            != source_commit.tree().tree_ids_and_labels()
    {
        return Err(user_error(
            "Copied working-copy metadata changed during adoption",
        ));
    }
    let local_working_copy = locked_ws
        .locked_wc()
        .downcast_mut::<LockedLocalWorkingCopy>()
        .ok_or_else(|| user_error("Only local-disk working copies can be adopted currently"))?;
    local_working_copy
        .reset_watchman()
        .map_err(|err| internal_error_with_message("Failed to reset fsmonitor state", err))?;

    if let Some(plan) = &git_worktree_plan {
        adopt_copied_git_worktree(plan, &workspace_root, &repo_path)?;
    }

    // Give the copy its own empty working-copy commit based on the copied
    // source @. Sharing the source's working-copy commit would let the two
    // workspaces rewrite each other's mutable state.
    let mut tx = repo.start_transaction();
    tx.set_workspace_name(&workspace_name);
    if tx.repo().view().get_wc_commit_id(&workspace_name).is_some() {
        return Err(user_error(format!(
            "Workspace named '{name}' already exists",
            name = workspace_name.as_symbol()
        )));
    }
    let new_commit = tx
        .repo_mut()
        .check_out(workspace_name.clone(), &source_commit)
        .await?;
    let unpublished = tx
        .write(format!(
            "adopt copied workspace '{}'",
            workspace_name.as_symbol()
        ))
        .await?;
    let new_repo = if command.should_commit_transaction() {
        unpublished.publish().await?
    } else {
        unpublished.leave_unpublished()
    };

    workspace_store.add(&workspace_name, &workspace_root)?;
    locked_ws
        .locked_wc()
        .rename_workspace(workspace_name.clone());
    locked_ws.finish(new_repo.op_id().clone()).await?;

    writeln!(
        ui.status(),
        "Adopted copied workspace as '{}'",
        workspace_name.as_symbol()
    )?;
    writeln!(
        ui.status(),
        "Working copy (@) now at: {}",
        new_commit.id().hex()
    )?;
    Ok(())
}

/// Replace the copied linked-worktree pointer with a fresh Git worktree
/// identity, while leaving the already-materialized files untouched.
fn adopt_copied_git_worktree(
    plan: &GitWorktreePlan,
    workspace_root: &Path,
    lock_root: &Path,
) -> Result<(), CommandError> {
    let dot_git_path = workspace_root.join(".git");
    if dot_git_path.is_dir() {
        return Err(
            user_error("Cannot adopt a copy containing a .git directory")
                .hinted("Only copies of linked Git worktrees are supported."),
        );
    }
    if !dot_git_path.is_file() {
        return Err(user_error(
            "Cannot adopt a colocated workspace without a copied .git file",
        ));
    }

    let parent = workspace_root
        .parent()
        .ok_or_else(|| user_error("Workspace root has no parent directory"))?;
    let temporary_worktree = tempfile::Builder::new()
        .prefix(".jj-adopt-git-")
        .tempdir_in(parent)
        .context(parent)?;
    create_git_worktree(plan, temporary_worktree.path(), lock_root)?;

    let temporary_dot_git = temporary_worktree.path().join(".git");
    fs::remove_file(&dot_git_path).context(&dot_git_path)?;
    fs::rename(&temporary_dot_git, &dot_git_path).context(&dot_git_path)?;

    // Git's worktree admin directory still points to the temporary path. Let
    // Git repair that supported metadata now that the .git file is in place.
    let output = Command::new(&plan.git_executable)
        .arg("-C")
        .arg(workspace_root)
        .args(["worktree", "repair"])
        .output()
        .map_err(|err| user_error(format!("Failed to repair Git worktree: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(user_error(format!(
            "Failed to repair Git worktree: {stderr}"
        )));
    }

    Ok(())
}
