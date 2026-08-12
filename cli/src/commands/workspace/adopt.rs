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

use std::path::{Path, PathBuf};

use jj_lib::backend::CommitId;
use jj_lib::git;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::workspace::Workspace;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
use crate::commands::btrfs::is_btrfs_subvolume;
use crate::commands::btrfs::is_subvolume_mode_enabled;
use crate::commands::btrfs::set_subvolume_mode;
use crate::ui::Ui;

/// Adopt the current linked Git worktree as a jj workspace.
///
/// The worktree must belong to the Git repository backing an existing
/// Git-colocated jj workspace. Adoption creates `.jj` metadata in place and
/// preserves the existing Git worktree, files, HEAD, and index.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspaceAdoptArgs {
    /// A name for the adopted workspace
    ///
    /// To override the default, which is the basename of the worktree root.
    #[arg(long)]
    name: Option<WorkspaceNameBuf>,
}

struct ExistingGitWorktree {
    root: PathBuf,
    common_dir: PathBuf,
    head_id: CommitId,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_adopt(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspaceAdoptArgs,
) -> Result<(), CommandError> {
    let worktree = discover_linked_git_worktree(command.cwd())?;
    let main_workspace_root = worktree
        .common_dir
        .parent()
        .ok_or_else(|| user_error("Cannot locate the main Git worktree"))?;
    let (main_settings, _config_env) =
        command.settings_for_new_workspace(ui, main_workspace_root)?;
    let main_workspace = command.load_workspace_at(main_workspace_root, &main_settings)?;
    if !crate::git_util::is_colocated_git_workspace(&main_workspace) {
        return Err(user_error(
            "The main Git worktree is not an existing Git-colocated jj workspace",
        ));
    }

    let git_backend = git::get_git_backend(main_workspace.repo_loader().store()).map_err(|_| {
        internal_error_with_message(
            "Git-colocated workspace does not use a Git-backed repository",
            "missing Git backend",
        )
    })?;
    let backend_git_dir = dunce::canonicalize(git_backend.git_repo_path())
        .unwrap_or_else(|_| git_backend.git_repo_path().to_owned());
    if backend_git_dir != worktree.common_dir {
        return Err(user_error(
            "The linked Git worktree does not belong to the jj repository's Git backend",
        ));
    }
    let snapshot = is_subvolume_mode_enabled(main_workspace_root);
    if snapshot && !is_btrfs_subvolume(&worktree.root)? {
        return Err(user_error(
            "Cannot adopt a plain Git worktree while Btrfs subvolume mode is enabled",
        )
        .hinted("Adopt a linked Git worktree whose root is already a Btrfs subvolume."));
    }

    let workspace_name = workspace_name(&worktree.root, args)?;
    let op = command.resolve_operation(
        ui,
        main_workspace.repo_loader(),
        main_workspace.workspace_name(),
    )?;
    let repo = main_workspace.repo_loader().load_at(&op).await?;
    if repo.view().get_wc_commit_id(&workspace_name).is_some() {
        return Err(user_error(format!(
            "Workspace named '{name}' already exists",
            name = workspace_name.as_symbol()
        )));
    }

    if !repo.index().has_id(&worktree.head_id).await? {
        git_backend
            .import_head_commits([&worktree.head_id])
            .map_err(|err| user_error(format!("Failed to import Git HEAD: {err}")))?;
    }
    let head_commit = repo.store().get_commit_async(&worktree.head_id).await?;
    let working_copy_factory = command.get_working_copy_factory_at(main_workspace_root)?;
    let (mut workspace, repo) = Workspace::init_workspace_with_existing_repo(
        &worktree.root,
        main_workspace.repo_path(),
        &repo,
        working_copy_factory,
        workspace_name.clone(),
    )
    .await?;
    if snapshot {
        set_subvolume_mode(&worktree.root, true)?;
    }

    // `init_workspace_with_existing_repo()` starts at the root commit. Replace
    // that placeholder with an empty working-copy commit on the existing Git
    // HEAD, then reset only jj's working-copy metadata. The files and Git index
    // are already materialized by Git and must not be checked out again.
    let mut tx = repo.start_transaction();
    let wc_commit = tx
        .repo_mut()
        .check_out(workspace_name.clone(), &head_commit)
        .await?;
    tx.repo_mut()
        .set_git_head_target(&workspace_name, RefTarget::normal(worktree.head_id.clone()));
    tx.repo_mut().rebase_descendants().await?;
    let unpublished = tx
        .write(format!(
            "adopt existing Git worktree as workspace '{}'",
            workspace_name.as_symbol()
        ))
        .await?;
    let repo = if command.should_commit_transaction() {
        unpublished.publish().await?
    } else {
        unpublished.leave_unpublished()
    };

    let mut locked_workspace = workspace.start_working_copy_mutation().await?;
    locked_workspace.locked_wc().reset(&wc_commit).await?;
    locked_workspace.finish(repo.op_id().clone()).await?;

    let mut workspace_command = command.for_workable_repo(ui, workspace, repo)?;
    workspace_command.maybe_snapshot(ui).await?;
    writeln!(
        ui.status(),
        "Adopted Git worktree as workspace '{}'",
        workspace_name.as_symbol()
    )?;
    Ok(())
}

fn discover_linked_git_worktree(cwd: &Path) -> Result<ExistingGitWorktree, CommandError> {
    let repo = gix::discover(cwd)
        .map_err(|err| user_error(format!("Failed to discover Git worktree: {err}")))?;
    let root = repo
        .workdir()
        .ok_or_else(|| user_error("Cannot adopt a bare Git repository"))?;
    let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    let git_dir = dunce::canonicalize(repo.git_dir()).unwrap_or_else(|_| repo.git_dir().into());
    let common_dir =
        dunce::canonicalize(repo.common_dir()).unwrap_or_else(|_| repo.common_dir().into());
    if git_dir == common_dir {
        return Err(user_error(
            "Cannot adopt the main Git worktree; run this from a linked Git worktree",
        ));
    }
    let head_id = repo
        .head_id()
        .map(|id| CommitId::from_bytes(id.as_bytes()))
        .map_err(|err| user_error(format!("Cannot adopt a Git worktree without HEAD: {err}")))?;
    Ok(ExistingGitWorktree {
        root,
        common_dir,
        head_id,
    })
}

fn workspace_name(
    workspace_root: &Path,
    args: &WorkspaceAdoptArgs,
) -> Result<WorkspaceNameBuf, CommandError> {
    let name = if let Some(name) = &args.name {
        name.to_owned()
    } else {
        workspace_root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| user_error("Workspace root name is not valid UTF-8"))?
            .into()
    };
    if name.as_str().is_empty() {
        return Err(user_error("New workspace name cannot be empty"));
    }
    Ok(name)
}
