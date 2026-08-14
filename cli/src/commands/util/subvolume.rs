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
use std::io::Write as _;
use std::path::{Path, PathBuf};

use btrfs_awacs::subvolume_migration::{MigrationOptions, SubvolumeMigration, copy_children};
use jj_lib::file_util::IoResultExt as _;

use crate::cli_util::CommandHelper;
use crate::command_error::{CommandError, internal_error, user_error};
use crate::ui::Ui;

/// Migrate a Git-colocated checkout onto Btrfs subvolume boundaries.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum UtilSubvolumeCommand {
    /// Build a Btrfs-subvolume checkout at a new path.
    Init {
        /// New repository path to initialize.
        destination: PathBuf,
        /// Set compression on new subvolumes and rewrite file extents instead
        /// of reflinking them.
        #[arg(long)]
        compress: Option<bool>,
        /// Keep a partial migration checkout after failure.
        #[arg(long)]
        keep: bool,
    },
    /// Migrate the current checkout onto Btrfs subvolume boundaries.
    Enable {
        /// Set compression on new subvolumes and rewrite file extents instead
        /// of reflinking them.
        #[arg(long)]
        compress: Option<bool>,
        /// Keep a partial migration checkout after failure.
        #[arg(long)]
        keep: bool,
    },
}

pub async fn cmd_util_subvolume(
    ui: &mut Ui,
    command: &CommandHelper,
    subcommand: &UtilSubvolumeCommand,
) -> Result<(), CommandError> {
    // Topology migration must not snapshot first: intentionally-untracked
    // files are physical checkout contents and must remain untouched.
    let workspace_command = command.workspace_helper_no_snapshot(ui).await?;
    let source_root = workspace_command.workspace_root();
    require_main_colocated_workspace(source_root)?;
    workspace_command
        .working_copy()
        .tree()
        .map_err(internal_error)?;

    match subcommand {
        UtilSubvolumeCommand::Init {
            destination,
            compress,
            keep,
        } => init_subvolume_at(command, source_root, destination, *compress, *keep)?,
        UtilSubvolumeCommand::Enable { compress, keep } => {
            migrate_checkout(source_root, *compress, *keep)?
        }
    }
    writeln!(ui.status(), "Btrfs subvolume migration complete.")?;
    Ok(())
}

fn init_subvolume_at(
    command: &CommandHelper,
    source_root: &Path,
    destination_arg: &Path,
    compression: Option<bool>,
    keep_on_failure: bool,
) -> Result<(), CommandError> {
    let destination = absolute_new_path(command.cwd(), destination_arg)?;
    if destination.starts_with(source_root) {
        return Err(user_error(
            "New subvolume checkout path must not be inside the source repository",
        ));
    }
    if destination.exists() {
        return Err(user_error(format!(
            "New subvolume checkout path already exists: {}",
            destination.display()
        )));
    }
    fs::create_dir(&destination).context(&destination)?;
    if let Err(error) = copy_children(source_root, &destination, compression.is_none())
        .map_err(|error| user_error(format!("Failed to copy repository: {error}")))
        .and_then(|()| migrate_checkout(&destination, compression, keep_on_failure))
    {
        if !keep_on_failure {
            drop(remove_checkout(&destination));
        }
        return Err(error);
    }
    Ok(())
}

fn migrate_checkout(
    destination: &Path,
    compression: Option<bool>,
    keep_on_failure: bool,
) -> Result<(), CommandError> {
    let migration = SubvolumeMigration::prepare(
        destination,
        MigrationOptions {
            compression,
            keep_temporary_on_drop: keep_on_failure,
        },
    )
    .map_err(|error| user_error(format!("Failed to prepare subvolume migration: {error}")))?;
    if migration.pending_snapshot().is_some() {
        return Err(user_error(
            "Cannot migrate a checkout with a nested subvolume without snapshot handling",
        ));
    }
    let committed = migration
        .commit()
        .map_err(|error| user_error(format!("Failed to publish subvolume migration: {error}")))?;
    committed
        .discard_displaced()
        .map_err(|error| user_error(format!("Failed to remove migration source: {error}")))?;
    Ok(())
}

fn remove_checkout(path: &Path) -> Result<(), CommandError> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_dir_all(path).context(path)?;
    Ok(())
}

fn absolute_new_path(cwd: &Path, path: &Path) -> Result<PathBuf, CommandError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let parent = path
        .parent()
        .ok_or_else(|| user_error("New subvolume checkout path has no parent directory"))?;
    let parent = fs::canonicalize(parent)
        .map_err(|error| user_error(format!("Failed to resolve destination parent: {error}")))?;
    let name = path
        .file_name()
        .ok_or_else(|| user_error("New subvolume checkout path has no final component"))?;
    Ok(parent.join(name))
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
