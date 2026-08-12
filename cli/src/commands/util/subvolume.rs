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
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use jj_lib::file_util::IoResultExt as _;
use jj_lib::local_working_copy::LockedLocalWorkingCopy;

use crate::cli_util::CommandHelper;
use crate::command_error::{CommandError, user_error};
use crate::commands::btrfs::{
    create_btrfs_subvolume, delete_btrfs_subvolume, is_btrfs_path, is_btrfs_subvolume,
    set_subvolume_mode,
};
use crate::ui::Ui;

/// Manage the Btrfs subvolume layout of the current repository.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum UtilSubvolumeCommand {
    /// Put the repository root and its .git directory in Btrfs subvolumes.
    Enable,
    /// Convert the repository root and its .git directory back to plain directories.
    Disable,
}

pub async fn cmd_util_subvolume(
    ui: &mut Ui,
    command: &CommandHelper,
    subcommand: &UtilSubvolumeCommand,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    let workspace_root = workspace_command.workspace_root().to_owned();
    require_main_colocated_workspace(&workspace_root)?;

    match subcommand {
        UtilSubvolumeCommand::Enable => {
            enable_subvolumes(&workspace_root)?;
            reset_local_working_copy_state(&mut workspace_command).await?;
            set_subvolume_mode(&workspace_root, true)?;
        }
        UtilSubvolumeCommand::Disable => {
            disable_subvolumes(&workspace_root)?;
            set_subvolume_mode(&workspace_root, false)?;
            reset_local_working_copy_state(&mut workspace_command).await?;
        }
    }

    let mode = match subcommand {
        UtilSubvolumeCommand::Enable => "enabled",
        UtilSubvolumeCommand::Disable => "disabled",
    };
    writeln!(ui.status(), "Btrfs subvolume mode {mode}.")?;
    Ok(())
}

fn require_main_colocated_workspace(workspace_root: &Path) -> Result<(), CommandError> {
    if workspace_root.join(".jj/repo").is_file() {
        return Err(user_error(
            "This command cannot be used in a non-main Jujutsu workspace",
        ));
    }
    if !workspace_root.join(".git").is_dir() {
        return Err(user_error(
            "This command requires a Git-colocated repository with a .git directory",
        ));
    }
    Ok(())
}

fn enable_subvolumes(workspace_root: &Path) -> Result<(), CommandError> {
    if !is_btrfs_path(workspace_root)? {
        return Err(user_error(
            "Cannot enable subvolume mode outside a Btrfs filesystem",
        ));
    }
    if !is_btrfs_subvolume(workspace_root)? {
        convert_directory_to_subvolume(workspace_root)?;
    }
    let dot_git = workspace_root.join(".git");
    if !is_btrfs_subvolume(&dot_git)? {
        convert_directory_to_subvolume(&dot_git)?;
    }
    Ok(())
}

fn disable_subvolumes(workspace_root: &Path) -> Result<(), CommandError> {
    let dot_git = workspace_root.join(".git");
    if is_btrfs_subvolume(&dot_git)? {
        convert_subvolume_to_directory(&dot_git)?;
    }
    if is_btrfs_subvolume(workspace_root)? {
        convert_subvolume_to_directory(workspace_root)?;
    }
    Ok(())
}

fn convert_directory_to_subvolume(path: &Path) -> Result<(), CommandError> {
    let temporary = unique_sibling(path, "enable")?;
    fs::rename(path, &temporary).context(path)?;
    if !create_btrfs_subvolume(path)? {
        drop(fs::rename(&temporary, path));
        return Err(user_error(
            "Cannot enable subvolume mode outside a Btrfs filesystem",
        ));
    }
    move_children(&temporary, path)?;
    fs::remove_dir(&temporary).context(&temporary)?;
    Ok(())
}

fn convert_subvolume_to_directory(path: &Path) -> Result<(), CommandError> {
    let temporary = unique_sibling(path, "disable")?;
    fs::rename(path, &temporary).context(path)?;
    fs::create_dir(path).context(path)?;
    move_children(&temporary, path)?;
    if !delete_btrfs_subvolume(&temporary)? {
        return Err(user_error(format!(
            "Failed to remove Btrfs subvolume at {}",
            temporary.display()
        )));
    }
    Ok(())
}

fn move_children(source: &Path, destination: &Path) -> Result<(), CommandError> {
    for entry in fs::read_dir(source).context(source)? {
        let entry = entry.context(source)?;
        let target = destination.join(entry.file_name());
        fs::rename(entry.path(), &target).context(&target)?;
    }
    Ok(())
}

fn unique_sibling(path: &Path, action: &str) -> Result<PathBuf, CommandError> {
    let parent = path
        .parent()
        .ok_or_else(|| user_error("Repository root has no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| user_error("Repository root has no final path component"))?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..16 {
        let candidate = parent.join(format!(
            ".{name}.jj-subvolume-{action}-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(user_error("Failed to choose a temporary subvolume path"))
}

async fn reset_local_working_copy_state(
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
) -> Result<(), CommandError> {
    let operation_id = workspace_command.repo().op_id().clone();
    let (mut locked_workspace, _commit) = workspace_command.start_working_copy_mutation().await?;
    let Some(local_working_copy): Option<&mut LockedLocalWorkingCopy> =
        locked_workspace.locked_wc().downcast_mut()
    else {
        return Err(user_error(
            "This command requires a standard local-disk working copy",
        ));
    };
    // A topology transition invalidates any retained physical baseline. The
    // next snapshot must establish a new one for the new subvolume layout.
    local_working_copy.reset_watchman()?;
    locked_workspace.finish(operation_id).await?;
    Ok(())
}
