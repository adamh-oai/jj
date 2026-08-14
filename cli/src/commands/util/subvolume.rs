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

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use btrfs_awacs::bootstrap::{InitProgress, RootInitPaths, initialize_root};
use btrfs_awacs::btrfs::{destroy_snapshot, set_subvolume_readonly, subvolume_info};
use btrfs_awacs::subvolume_migration::{
    MigrationOptions, convert_subvolume_root, copy_children, copy_children_except,
};
use futures::future::{Either, select};
use jj_lib::file_util::IoResultExt as _;
use jj_lib::local_working_copy::{
    LocalWorkingCopy, reset_local_working_copy_fsmonitor, seed_local_working_copy_tree,
    snapshot_mode_has_committed_baseline,
};

use crate::cleanup_guard::CleanupGuard;
use crate::cli_util::{CommandHelper, shell_quote};
use crate::command_error::{CommandError, internal_error, user_error};
use crate::commands::btrfs::{
    begin_subvolume_mode, btrfs_command, btrfs_subvolume_id, delete_btrfs_subvolume, is_btrfs_path,
    is_btrfs_subvolume, is_subvolume_mode_committed, is_subvolume_mode_enabled, set_subvolume_mode,
};
use crate::progress::ProgressWriter;
use crate::ui::Ui;

/// Manage the Btrfs subvolume layout of the current repository.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum UtilSubvolumeCommand {
    /// Initialize a replacement checkout, then atomically activate it.
    Enable {
        /// Set compression on new subvolumes and rewrite file extents instead
        /// of reflinking them.
        #[arg(long)]
        compress: Option<bool>,
        /// Keep a partial migration checkout after failure.
        #[arg(long)]
        keep: bool,
        /// Discard and rebuild the committed AWACS cursor for an already
        /// enabled checkout.
        #[arg(long)]
        rebuild_baseline: bool,
    },
    /// Convert the repository root and its .git directory back to plain directories.
    Disable,
    /// Show the repository subvolume layout and working-copy snapshot state.
    Status,
}

pub async fn cmd_util_subvolume(
    ui: &mut Ui,
    command: &CommandHelper,
    subcommand: &UtilSubvolumeCommand,
) -> Result<(), CommandError> {
    // A previous enable may have committed the strict marker before its first
    // AWACS baseline was durably paired with the tree. Ordinary working-copy
    // loading rejects that state by design, so temporarily reopen only the
    // enable recovery and status-reporting paths before asking the generic
    // loader to read it. The guard restores the strict marker if loading,
    // reporting, or repair fails.
    let recovery_root = command.workspace_loader()?.workspace_root().to_owned();
    let missing_committed_baseline = is_subvolume_mode_committed(&recovery_root)
        && !snapshot_mode_has_committed_baseline(&recovery_root.join(".jj/working_copy"))
            .map_err(internal_error)?;
    let relax_load_invariant = missing_committed_baseline
        && matches!(
            subcommand,
            UtilSubvolumeCommand::Enable { .. } | UtilSubvolumeCommand::Status
        );
    let recovery_needed =
        missing_committed_baseline && matches!(subcommand, UtilSubvolumeCommand::Enable { .. });
    let recovery_armed = Arc::new(AtomicBool::new(false));
    let recovery_guard = {
        let recovery_root = recovery_root.clone();
        let recovery_armed = recovery_armed.clone();
        CleanupGuard::new(move || {
            if recovery_armed.load(Ordering::Relaxed)
                && let Err(err) = set_subvolume_mode(&recovery_root, true)
            {
                eprintln!("Failed to restore strict subvolume mode after recovery: {err:?}");
            }
        })
    };
    if relax_load_invariant {
        begin_subvolume_mode(&recovery_root)?;
        recovery_armed.store(true, Ordering::Relaxed);
    }
    // Topology conversion must not snapshot first: a repository can have
    // intentionally-untracked large files, and the conversion itself does not
    // need to change the semantic working-copy tree.
    let mut workspace_command = command.workspace_helper_no_snapshot(ui).await?;
    let workspace_root = workspace_command.workspace_root().to_owned();

    match subcommand {
        UtilSubvolumeCommand::Enable {
            compress,
            keep,
            rebuild_baseline,
        } => {
            let main_workspace = require_colocated_workspace(&workspace_command)?;
            if !main_workspace {
                require_main_subvolume_mode_for_linked_workspace(&workspace_command)?;
            }
            if is_subvolume_mode_committed(&workspace_root) && !recovery_needed {
                if *rebuild_baseline {
                    rebuild_snapshot_baseline(ui, &mut workspace_command).await?;
                    drop(recovery_guard);
                    return Ok(());
                }
                writeln!(ui.status(), "Btrfs subvolume mode is already enabled.")?;
                drop(recovery_guard);
                return Ok(());
            }
            let original_checkout =
                enable_subvolume_via_init(ui, command, &workspace_command, *compress, *keep)
                    .await?;
            if recovery_needed {
                // Loading a missing-baseline source temporarily relaxes its
                // strict marker. Preserve the retained source exactly as a
                // broken-but-diagnosable checkout rather than leaving it in
                // the transient transition mode.
                set_subvolume_mode(&original_checkout, true)?;
            }
            finish_retained_checkout(ui, &original_checkout, &workspace_root)?;
            recovery_armed.store(false, Ordering::Relaxed);
            drop(recovery_guard);
        }
        UtilSubvolumeCommand::Disable => {
            require_colocated_workspace(&workspace_command)?;
            workspace_command
                .working_copy()
                .tree()
                .map_err(internal_error)?;
            // Switch the on-disk working-copy encoding while the current
            // absolute state path is still in place. Converting the root
            // subvolume renames that directory temporarily; mutating a loaded
            // working copy after that rename can leave its checkout state
            // paired with the old pathname.
            let operation_id = workspace_command.repo().op_id().clone();
            let (mut locked_workspace, _commit) = workspace_command
                .unchecked_start_working_copy_mutation()
                .await?;
            if !reset_local_working_copy_fsmonitor(locked_workspace.locked_wc())? {
                return Err(user_error(
                    "This command requires a standard local-disk working copy",
                ));
            }
            set_subvolume_mode(&workspace_root, false)?;
            locked_workspace.finish(operation_id).await?;
            disable_subvolumes(&workspace_root)?;
        }
        UtilSubvolumeCommand::Status => {
            if relax_load_invariant {
                set_subvolume_mode(&workspace_root, true)?;
                recovery_armed.store(false, Ordering::Relaxed);
            }
            write_subvolume_status(
                ui,
                &workspace_command,
                &workspace_root,
                missing_committed_baseline,
            )?;
            return Ok(());
        }
    }

    let mode = match subcommand {
        UtilSubvolumeCommand::Enable { .. } => "enabled",
        UtilSubvolumeCommand::Disable => "disabled",
        UtilSubvolumeCommand::Status => unreachable!(),
    };
    writeln!(ui.status(), "Btrfs subvolume mode {mode}.")?;
    Ok(())
}

async fn rebuild_snapshot_baseline(
    ui: &Ui,
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
) -> Result<(), CommandError> {
    writeln!(
        ui.status(),
        "Rebuilding committed AWACS snapshot baseline..."
    )?;
    let workspace_root = workspace_command.workspace_root().to_owned();
    rebuild_awacs_root_state(ui, &workspace_root)?;
    let operation_id = workspace_command.repo().op_id().clone();
    let (mut locked_workspace, _commit) = workspace_command
        .unchecked_start_working_copy_mutation()
        .await?;
    if !reset_local_working_copy_fsmonitor(locked_workspace.locked_wc())? {
        return Err(user_error(
            "This command requires a snapshot-backed local-disk working copy",
        ));
    }
    locked_workspace.finish(operation_id).await?;
    // The compact journal is intentionally baseline-less between the reset
    // above and the replacement immutable scan below. Temporarily use the
    // transition marker so helper reloads can enter that state, then restore
    // strict mode whether reseeding succeeds or fails.
    begin_subvolume_mode(&workspace_root)?;
    let baseline_result = establish_initial_snapshot_baseline(ui, workspace_command).await;
    let marker_result = set_subvolume_mode(&workspace_root, true);
    baseline_result?;
    marker_result?;
    writeln!(ui.status(), "Rebuilt committed AWACS snapshot baseline.")?;
    Ok(())
}

fn rebuild_awacs_root_state(ui: &Ui, root: &Path) -> Result<(), CommandError> {
    // Synthetic CLI tests provide their own baseline without a real Btrfs
    // root or external manager database.
    if std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_some() {
        return Ok(());
    }
    let paths = RootInitPaths::from_environment(root)
        .map_err(|err| user_error(format!("Failed to resolve AWACS root state: {err}")))?;
    let state_dir = paths
        .manager_db
        .parent()
        .ok_or_else(|| user_error("AWACS manager database has no state directory"))?;
    writeln!(
        ui.status(),
        "Removing existing AWACS root state at {}...",
        state_dir.display()
    )?;
    if !paths.managed_dir.starts_with(state_dir) {
        remove_btrfs_aware(&paths.managed_dir)?;
    }
    remove_btrfs_aware(state_dir)?;
    writeln!(ui.status(), "Initializing fresh AWACS root state...")?;
    seed_initial_awacs_root(ui, root)
}

fn remove_btrfs_aware(path: &Path) -> Result<(), CommandError> {
    if !path.exists() {
        return Ok(());
    }
    let file_type = fs::symlink_metadata(path).context(path)?.file_type();
    if !file_type.is_dir() {
        fs::remove_file(path).context(path)?;
        return Ok(());
    }
    if is_btrfs_subvolume(path)? {
        delete_rebuild_snapshot(path)?;
        return Ok(());
    }
    for entry in fs::read_dir(path).context(path)? {
        let entry = entry.context(path)?;
        remove_btrfs_aware(&entry.path())?;
    }
    fs::remove_dir(path).context(path)?;
    Ok(())
}

/// Deletes one managed snapshot during a from-scratch rebuild.
///
/// The normal AWACS GC path makes a retained read-only snapshot writable
/// immediately before destroying it. Rebuild has no live store left to drive
/// GC, so it must perform the same transition directly instead of asking the
/// btrfs CLI to remove a read-only root.
fn delete_rebuild_snapshot(path: &Path) -> Result<(), CommandError> {
    let parent = path
        .parent()
        .ok_or_else(|| user_error(format!("Btrfs subvolume has no parent: {}", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| user_error(format!("Btrfs subvolume has no name: {}", path.display())))?;
    let target = fs::File::open(path).context(path)?;
    let readonly = subvolume_info(target.as_fd())
        .map_err(|err| user_error(format!("Failed to inspect Btrfs subvolume: {err}")))?
        .readonly();
    if readonly {
        set_subvolume_readonly(target.as_fd(), false).map_err(|err| {
            user_error(format!(
                "Failed to make Btrfs subvolume writable for deletion: {err}"
            ))
        })?;
    }
    let parent = fs::File::open(parent).context(parent)?;
    if let Err(err) = destroy_snapshot(parent.as_fd(), name.as_bytes()) {
        if readonly && let Err(restore_err) = set_subvolume_readonly(target.as_fd(), true) {
            return Err(user_error(format!(
                "Failed to delete Btrfs subvolume: {err}; also failed to restore read-only flag: {restore_err}"
            )));
        }
        return Err(user_error(format!(
            "Failed to delete Btrfs subvolume: {err}"
        )));
    }
    Ok(())
}

async fn init_subvolume_at(
    ui: &Ui,
    command: &CommandHelper,
    source_command: &crate::cli_util::WorkspaceCommandHelper,
    destination_arg: &Path,
    compress: Option<bool>,
    keep_on_failure: bool,
) -> Result<(), CommandError> {
    let destination = absolute_new_path(command.cwd(), destination_arg)?;
    let destination_preexisted = destination.exists();
    let cleanup_armed = Arc::new(AtomicBool::new(!destination_preexisted));
    let cleanup_guard = {
        let destination = destination.clone();
        let cleanup_armed = cleanup_armed.clone();
        CleanupGuard::new(move || {
            if !cleanup_armed.load(Ordering::Relaxed) || !destination.exists() {
                return;
            }
            if keep_on_failure {
                eprintln!(
                    "Partial migration checkout retained at {}.",
                    destination.display()
                );
            } else if let Err(err) = remove_migration_checkout(&destination) {
                eprintln!(
                    "Failed to remove partial migration checkout at {}: {err:?}",
                    destination.display()
                );
            }
        })
    };
    let result = init_subvolume_at_inner(ui, command, source_command, &destination, compress).await;
    if let Err(err) = result {
        cleanup_armed.store(false, Ordering::Relaxed);
        drop(cleanup_guard);
        if !destination_preexisted && destination.exists() {
            if keep_on_failure {
                writeln!(
                    ui.status(),
                    "Partial migration checkout retained at {}.",
                    destination.display()
                )?;
            } else {
                writeln!(
                    ui.status(),
                    "Removing partial migration checkout at {}...",
                    destination.display()
                )?;
                if let Err(cleanup_err) = remove_migration_checkout(&destination) {
                    return Err(user_error(format!(
                        "{err:?}; failed to remove partial migration checkout at {}: {cleanup_err:?}",
                        destination.display()
                    )));
                }
            }
        }
        return Err(err);
    }
    cleanup_armed.store(false, Ordering::Relaxed);
    drop(cleanup_guard);
    Ok(())
}

async fn init_subvolume_at_inner(
    ui: &Ui,
    command: &CommandHelper,
    source_command: &crate::cli_util::WorkspaceCommandHelper,
    destination_arg: &Path,
    compress: Option<bool>,
) -> Result<(), CommandError> {
    let source_root = source_command.workspace_root();
    let main_colocated_workspace = require_colocated_workspace(source_command)?;
    if !main_colocated_workspace {
        require_main_subvolume_mode_for_linked_workspace(source_command)?;
    }
    source_command
        .working_copy()
        .tree()
        .map_err(internal_error)?;

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
    let parent = destination
        .parent()
        .ok_or_else(|| user_error("New subvolume checkout path has no parent directory"))?;
    if !is_btrfs_path(parent)? {
        return Err(user_error(format!(
            "Cannot initialize subvolume checkout outside a Btrfs filesystem: {}",
            parent.display()
        )));
    }

    let source_git = source_root.join(".git");
    let destination_git = destination.join(".git");
    let allow_reflinks = compress.is_none();
    if allow_reflinks && is_btrfs_subvolume(source_root)? {
        writeln!(
            ui.status(),
            "Snapshotting repository root subvolume to {}...",
            destination.display()
        )?;
        create_btrfs_snapshot(source_root, &destination, false)?;
    } else {
        writeln!(
            ui.status(),
            "Copying repository contents to {} before subvolume migration...",
            destination.display()
        )?;
        fs::create_dir(&destination).context(&destination)?;
        if allow_reflinks && is_btrfs_subvolume(&source_git)? {
            copy_children_except(
                source_root,
                &destination,
                OsStr::new(".git"),
                allow_reflinks,
            )
            .map_err(|err| user_error(format!("Failed to copy repository: {err}")))?;
            create_btrfs_snapshot(&source_git, &destination_git, false)?;
        } else {
            copy_children(source_root, &destination, allow_reflinks)
                .map_err(|err| user_error(format!("Failed to copy repository: {err}")))?;
        }
    }

    if allow_reflinks && is_btrfs_subvolume(source_root)? && is_btrfs_subvolume(&source_git)? {
        // Root snapshots omit nested subvolumes. Snapshotting is deliberately
        // JJ-owned; the reusable migration primitive only reports boundaries
        // that need this treatment.
        remove_existing_destination(&destination_git)?;
        writeln!(ui.status(), "Snapshotting nested .git subvolume...")?;
        create_btrfs_snapshot(&source_git, &destination_git, false)?;
    }

    let committed = convert_subvolume_root(
        &destination,
        MigrationOptions {
            compression: compress,
            keep_temporary_on_drop: false,
        },
        |phase| {
            let _status_result = writeln!(ui.status(), "AWACS init: {phase}...");
        },
    )
    .map_err(|err| user_error(format!("Failed to publish subvolume migration: {err}")))?;
    committed
        .discard_displaced()
        .map_err(|err| user_error(format!("Failed to remove migration source: {err}")))?;

    writeln!(
        ui.status(),
        "Enabling snapshot-backed working-copy state in {}...",
        destination.display()
    )?;
    let mut destination_command = command
        .workspace_helper_no_snapshot_at(ui, &destination)
        .await?;
    // Initialize the new Btrfs UUID before writing the mode marker. Even
    // though destination_command was loaded as an ordinary local working
    // copy, finishing its reset can reload mode from disk and begin a
    // snapshot-backed materialization. That path needs the external AWACS
    // baseline to exist already.
    seed_initial_awacs_root(ui, &destination)?;
    begin_subvolume_mode(&destination)?;
    reset_local_working_copy_state(&mut destination_command).await?;
    // Finishing the reset replaces the working-copy state directory.
    begin_subvolume_mode(&destination)?;
    remove_legacy_tree_state(&destination)?;
    // The helper above was loaded before the marker existed, so it still
    // owns the ordinary local-disk working-copy implementation. Reload after
    // the marker is durable so the initial transaction uses snapshot-backed
    // state and can publish the AWACS baseline.
    drop(destination_command);
    let mut destination_command = command
        .workspace_helper_no_snapshot_at(ui, &destination)
        .await?;
    writeln!(ui.status(), "Seeding snapshot-backed working-copy tree...")?;
    seed_snapshot_working_copy_tree(&mut destination_command).await?;
    writeln!(ui.status(), "Creating initial AWACS snapshot baseline...")?;
    establish_initial_snapshot_baseline(ui, &mut destination_command).await?;
    set_subvolume_mode(&destination, true)?;
    writeln!(
        ui.status(),
        "Initialized snapshot-backed Btrfs checkout at {}.",
        destination.display()
    )?;
    Ok(())
}

fn seed_initial_awacs_root(ui: &Ui, root: &Path) -> Result<(), CommandError> {
    // The existing CLI test backend supplies a synthetic AWACS baseline and
    // intentionally does not provide a real Btrfs filesystem to index.
    if std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_some() {
        writeln!(ui.status(), "AWACS init: test root initialized...")?;
        return Ok(());
    }
    let mut progress_writer = ProgressWriter::new(ui, "AWACS init");
    let mut final_counts = None;
    initialize_root(root, |progress| match progress {
        InitProgress::Phase(phase) => {
            let _status_result = writeln!(ui.status(), "AWACS init: {phase}...");
        }
        InitProgress::Index(counts) => {
            final_counts = Some(counts);
            if let Some(writer) = &mut progress_writer {
                writer
                    .display(&format!(
                        "{} directories, {} objects, {} paths",
                        counts.directories, counts.objects, counts.references,
                    ))
                    .ok();
            }
        }
    })
    .map_err(|err| user_error(format!("Failed to initialize AWACS root: {err}")))?;
    drop(progress_writer);
    if let Some(counts) = final_counts {
        writeln!(
            ui.status(),
            "AWACS init: indexed {} directories, {} objects, {} paths.",
            counts.directories,
            counts.objects,
            counts.references,
        )?;
    }
    Ok(())
}

/// Builds a complete replacement checkout beside the source, then performs
/// only the final pair of directory renames. Until initialization succeeds,
/// the source path is never renamed or modified by topology conversion. On
/// success, retain the source checkout beside the newly activated checkout so
/// both trees remain available for inspection or manual cleanup.
async fn enable_subvolume_via_init(
    ui: &Ui,
    command: &CommandHelper,
    source_command: &crate::cli_util::WorkspaceCommandHelper,
    compress: Option<bool>,
    keep_on_failure: bool,
) -> Result<PathBuf, CommandError> {
    let source_root = source_command.workspace_root();
    let staged = unique_sibling(source_root, "init")?;
    writeln!(
        ui.status(),
        "Building snapshot-backed checkout at {}...",
        staged.display()
    )?;
    init_subvolume_at(
        ui,
        command,
        source_command,
        &staged,
        compress,
        keep_on_failure,
    )
    .await?;

    let original_cwd = env::current_dir()
        .map_err(|err| user_error(format!("Failed to read current directory: {err}")))?;
    let retained_source = unique_visible_sibling(source_root, "source")?;
    writeln!(
        ui.status(),
        "Activating initialized checkout; retaining original at {}...",
        retained_source.display()
    )?;
    fs::rename(source_root, &retained_source).context(source_root)?;
    if let Err(err) = fs::rename(&staged, source_root).context(source_root) {
        // The initialization already succeeded, but activation is not a
        // reason to strand the original under a hidden name. Restore its
        // pathname and leave the initialized sibling in place for retry.
        if let Err(restore_err) = fs::rename(&retained_source, source_root).context(source_root) {
            return Err(user_error(format!(
                "Failed to activate initialized checkout: {err}; also failed to restore the original checkout: {restore_err}"
            )));
        }
        return Err(err.into());
    }
    reanchor_current_directory(source_root, &original_cwd)?;
    Ok(retained_source)
}

fn finish_retained_checkout(
    ui: &Ui,
    retained_source: &Path,
    activated_checkout: &Path,
) -> Result<(), CommandError> {
    let retained_source = retained_source.to_string_lossy();
    let activated_checkout = activated_checkout.to_string_lossy();
    writeln!(
        ui.status(),
        "Original checkout retained at {retained_source}."
    )?;
    writeln!(
        ui.status(),
        "If the snapshot-backed checkout looks good, delete the original checkout at {retained_source}."
    )?;
    writeln!(
        ui.status(),
        "Enter the snapshot-backed checkout with: cd {}",
        shell_quote(&activated_checkout)
    )?;
    Ok(())
}

fn remove_migration_checkout(path: &Path) -> Result<(), CommandError> {
    let dot_git = path.join(".git");
    if dot_git.exists() {
        remove_existing_destination(&dot_git)?;
    }
    remove_existing_destination(path)
}

async fn seed_snapshot_working_copy_tree(
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
) -> Result<(), CommandError> {
    let operation_id = workspace_command.repo().op_id().clone();
    let (mut locked_workspace, commit) = workspace_command
        .unchecked_start_working_copy_mutation()
        .await?;
    if !seed_local_working_copy_tree(locked_workspace.locked_wc(), &commit.tree())
        .await
        .map_err(internal_error)?
    {
        return Err(user_error(
            "This command requires a snapshot-backed local-disk working copy",
        ));
    }
    locked_workspace.finish(operation_id).await?;
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
        .map_err(|err| user_error(format!("Failed to resolve destination parent: {err}")))?;
    let name = path
        .file_name()
        .ok_or_else(|| user_error("New subvolume checkout path has no final component"))?;
    Ok(parent.join(name))
}

fn reanchor_current_directory(
    workspace_root: &Path,
    original_cwd: &Path,
) -> Result<(), CommandError> {
    let relative_cwd = original_cwd
        .strip_prefix(workspace_root)
        .map_err(|_| user_error("Cannot enable subvolume mode from outside the workspace root"))?;
    env::set_current_dir(workspace_root.join(relative_cwd)).map_err(|err| {
        user_error(format!(
            "Failed to restore current directory after root subvolume conversion: {err}"
        ))
    })
}

fn remove_legacy_tree_state(workspace_root: &Path) -> Result<(), CommandError> {
    let tree_state = workspace_root.join(".jj/working_copy/tree_state");
    if let Err(err) = fs::remove_file(&tree_state)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(user_error(format!(
            "Failed to remove legacy tree state at {}: {err}",
            tree_state.display()
        )));
    }
    Ok(())
}

fn write_subvolume_status(
    ui: &mut Ui,
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    workspace_root: &Path,
    missing_committed_baseline: bool,
) -> Result<(), CommandError> {
    let mode_enabled = is_subvolume_mode_enabled(workspace_root);
    writeln!(
        ui.stdout(),
        "Subvolume mode: {}",
        if mode_enabled { "enabled" } else { "disabled" }
    )?;
    write_path_status(ui, "Repository root", workspace_root)?;
    write_path_status(ui, ".git", &workspace_root.join(".git"))?;

    if !mode_enabled {
        writeln!(
            ui.stdout(),
            "Working copy snapshot: none (subvolume mode disabled)"
        )?;
        return Ok(());
    }

    if missing_committed_baseline {
        writeln!(
            ui.stdout(),
            "Working copy snapshot: none (required committed AWACS baseline is missing)"
        )?;
        writeln!(
            ui.stdout(),
            "Recovery: run `jj util subvolume enable` to build and activate a replacement checkout."
        )?;
        return Ok(());
    }

    let Some(local_working_copy) = workspace_command
        .working_copy()
        .downcast_ref::<LocalWorkingCopy>()
    else {
        return Err(user_error(
            "This command requires a standard local-disk working copy",
        ));
    };
    let journal = local_working_copy.journal_status()?;
    if let (Some(backend), Some(identity)) = (
        journal.baseline_backend.as_deref(),
        journal.baseline_snapshot_identity.as_deref(),
    ) {
        let retention = journal.baseline_retention.unwrap_or("unknown retention");
        writeln!(
            ui.stdout(),
            "Working copy snapshot: {backend} {} ({}, {retention})",
            format_bytes(identity),
            journal.phase,
        )?;
    } else if let Some(reason) = journal.fallback_reason {
        writeln!(
            ui.stdout(),
            "Working copy snapshot: none ({}: {reason})",
            journal.phase
        )?;
    } else {
        writeln!(
            ui.stdout(),
            "Working copy snapshot: none ({})",
            journal.phase
        )?;
    }
    Ok(())
}

fn write_path_status(ui: &mut Ui, label: &str, path: &Path) -> Result<(), CommandError> {
    if !path.exists() {
        writeln!(ui.stdout(), "{label}: missing")?;
    } else if !path.is_dir() {
        writeln!(ui.stdout(), "{label}: not a directory")?;
    } else {
        match is_btrfs_path(path) {
            Ok(false) => writeln!(ui.stdout(), "{label}: not on Btrfs")?,
            Err(_) => writeln!(ui.stdout(), "{label}: Btrfs status unavailable")?,
            Ok(true) if is_btrfs_subvolume(path)? => match btrfs_subvolume_id(path)? {
                Some(id) => writeln!(ui.stdout(), "{label}: Btrfs subvolume (ID {id})")?,
                None => writeln!(ui.stdout(), "{label}: Btrfs subvolume")?,
            },
            Ok(true) => writeln!(ui.stdout(), "{label}: Btrfs directory (not a subvolume)")?,
        }
    }
    Ok(())
}

fn format_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn require_colocated_workspace(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
) -> Result<bool, CommandError> {
    let workspace_root = workspace_command.workspace_root();
    if !crate::git_util::is_colocated_git_workspace(workspace_command.workspace()) {
        return Err(user_error(
            "This command requires a Git-colocated repository",
        ));
    }
    let main_workspace = !workspace_root.join(".jj/repo").is_file();
    if main_workspace && !workspace_root.join(".git").is_dir() {
        return Err(user_error(
            "The main Git-colocated workspace must have a .git directory",
        ));
    }
    if !main_workspace && !workspace_root.join(".git").is_file() {
        return Err(user_error(
            "A linked Git-colocated workspace must have a .git file",
        ));
    }
    Ok(main_workspace)
}

fn require_main_subvolume_mode_for_linked_workspace(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
) -> Result<(), CommandError> {
    let main_root = workspace_command
        .repo_path()
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| user_error("Cannot locate the main workspace root"))?;
    if !is_subvolume_mode_committed(main_root) {
        return Err(user_error(
            "Cannot enable subvolume mode in a linked workspace until the main workspace is +             snapshot-backed",
        )
        .hinted(
            "Run `jj util subvolume enable` from the main workspace first, then retry here.",
        ));
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

fn convert_subvolume_to_directory(path: &Path) -> Result<(), CommandError> {
    let temporary = unique_sibling(path, "disable")?;
    fs::rename(path, &temporary).context(path)?;
    fs::create_dir(path).context(path)?;
    if let Err(err) = copy_children(&temporary, path, true)
        .map_err(|err| user_error(format!("Failed to copy subvolume contents: {err}")))
    {
        rollback_created_directory(path, &temporary)?;
        return Err(err);
    }
    if !delete_btrfs_subvolume(&temporary)? {
        return Err(user_error(format!(
            "Failed to remove Btrfs subvolume at {}",
            temporary.display()
        )));
    }
    Ok(())
}

fn remove_existing_destination(path: &Path) -> Result<(), CommandError> {
    if !path.exists() {
        return Ok(());
    }
    let file_type = fs::symlink_metadata(path).context(path)?.file_type();
    if file_type.is_dir() {
        if is_btrfs_subvolume(path)? {
            if !delete_btrfs_subvolume(path)? {
                return Err(user_error(format!(
                    "Failed to remove Btrfs subvolume at {}",
                    path.display()
                )));
            }
        } else {
            fs::remove_dir_all(path).context(path)?;
        }
    } else {
        fs::remove_file(path).context(path)?;
    }
    Ok(())
}

fn rollback_created_directory(path: &Path, temporary: &Path) -> Result<(), CommandError> {
    fs::remove_dir_all(path).context(path)?;
    fs::rename(temporary, path).context(path)?;
    Ok(())
}

fn create_btrfs_snapshot(
    source: &Path,
    destination: &Path,
    read_only: bool,
) -> Result<(), CommandError> {
    let mut command = btrfs_command();
    command.args(["subvolume", "snapshot"]);
    if read_only {
        command.arg("-r");
    }
    let output = command
        .arg(source)
        .arg(destination)
        .output()
        .map_err(|err| user_error(format!("Failed to create Btrfs snapshot: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(user_error(format!(
            "Failed to create Btrfs snapshot: {stderr}"
        )));
    }
    Ok(())
}

fn unique_sibling(path: &Path, action: &str) -> Result<PathBuf, CommandError> {
    unique_sibling_with_prefix(path, action, ".")
}

fn unique_visible_sibling(path: &Path, action: &str) -> Result<PathBuf, CommandError> {
    unique_sibling_with_prefix(path, action, "")
}

fn unique_sibling_with_prefix(
    path: &Path,
    action: &str,
    prefix: &str,
) -> Result<PathBuf, CommandError> {
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
            "{prefix}{name}.jj-subvolume-{action}-{}-{nonce}-{attempt}",
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
    // The topology/mode switch deliberately changes how the same semantic
    // tree is encoded on disk, so the ordinary old-tree consistency check
    // would compare different state formats and reject the transition.
    let (mut locked_workspace, _commit) = workspace_command
        .unchecked_start_working_copy_mutation()
        .await?;
    if !reset_local_working_copy_fsmonitor(locked_workspace.locked_wc())? {
        return Err(user_error(
            "This command requires a standard local-disk working copy",
        ));
    }
    // A topology transition invalidates any retained physical baseline. The
    // next snapshot must establish a new one for the new subvolume layout.
    locked_workspace.finish(operation_id).await?;
    Ok(())
}

async fn establish_initial_snapshot_baseline(
    ui: &Ui,
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
) -> Result<(), CommandError> {
    let phase = Arc::new(Mutex::new("starting the JJ working-copy transaction"));
    let report_phase = {
        let phase = phase.clone();
        move |new_phase| -> Result<(), CommandError> {
            if let Ok(mut current_phase) = phase.lock() {
                *current_phase = new_phase;
            }
            writeln!(ui.status(), "JJ subvolume enable: {new_phase}...")?;
            Ok(())
        }
    };
    let mut snapshot = Box::pin(workspace_command.snapshot_for_subvolume_enable(ui, &report_phase));
    loop {
        let heartbeat = Box::pin(heartbeat_delay(Duration::from_secs(10)));
        match select(snapshot, heartbeat).await {
            Either::Left((result, _heartbeat)) => {
                result?;
                break;
            }
            Either::Right(((), pending_snapshot)) => {
                snapshot = pending_snapshot;
                let current_phase = phase
                    .lock()
                    .map(|current_phase| *current_phase)
                    .unwrap_or("running the JJ working-copy transaction");
                writeln!(
                    ui.status(),
                    "Still enabling subvolume mode: {current_phase}..."
                )?;
            }
        }
    }
    let local_working_copy = workspace_command
        .working_copy()
        .downcast_ref::<LocalWorkingCopy>()
        .ok_or_else(|| user_error("This command requires a standard local-disk working copy"))?;
    if local_working_copy
        .journal_status()?
        .baseline_backend
        .is_none()
    {
        return Err(user_error(
            "AWACS did not publish a committed snapshot baseline",
        ));
    }
    Ok(())
}

/// Sleeps without assuming the CLI is running inside a Tokio runtime.
async fn heartbeat_delay(duration: Duration) {
    let (sender, receiver) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        let _ = sender.send(());
    });
    let _ = receiver.await;
}
