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

use std::ffi::OsStr;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use std::os::fd::AsFd as _;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use std::os::fd::OwnedFd;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use std::os::unix::ffi::OsStringExt as _;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "git")]
use std::process::Command;

use clap_complete::ArgValueCandidates;
use itertools::Itertools as _;
use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigSource;
use jj_lib::file_util::FileIdentity;
use jj_lib::file_util::IoResultExt as _;
#[cfg(feature = "git")]
use jj_lib::git::GitSubprocessOptions;
use jj_lib::lock::FileLock;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::workspace_store::WorkspaceStore;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use nix::dir::Dir;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use nix::fcntl::AtFlags;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use nix::fcntl::OFlag;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use nix::fcntl::RenameFlags;
#[cfg(all(unix, not(all(target_os = "linux", target_env = "gnu"))))]
use nix::fcntl::renameat;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use nix::fcntl::renameat2;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use nix::sys::stat::Mode;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use nix::sys::stat::SFlag;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use nix::sys::stat::fstatat;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use nix::unistd::UnlinkatFlags;
#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
use nix::unistd::unlinkat;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::WorkspaceCommandHelper;
use crate::command_error::CommandError;
use crate::command_error::print_error_sources;
use crate::command_error::user_error;
use crate::command_error::user_error_with_message;
use crate::commands::btrfs::btrfs_user_subvol_rm_allowed;
use crate::commands::btrfs::delete_btrfs_subvolume;
use crate::commands::btrfs::is_btrfs_subvolume_for_removal;
use crate::commands::btrfs::is_subvolume_mode_enabled;
use crate::commands::btrfs::subvolume_operation_lock_path;
use crate::complete;
#[cfg(feature = "git")]
use crate::git_util::unlink_git_worktree;
use crate::ui::Ui;

const USE_WORKSPACE_FORGET_HINT: &str =
    "Use `jj workspace forget` to stop tracking a workspace without deleting files.";

/// Remove workspaces and their working-copy files from disk
///
/// Ordinary working copies are snapshotted before removal. Snapshot-backed
/// and Btrfs-subvolume workspaces must have no unsnapshotted changes, and may
/// not be the current workspace. The main workspace cannot be removed.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspaceRemoveArgs {
    /// Skip validity and dirty checks; delete the recorded path if it exists
    #[arg(long)]
    force: bool,

    /// Names of the workspaces to remove
    #[arg(required = true, add = ArgValueCandidates::new(complete::workspaces))]
    workspaces: Vec<WorkspaceNameBuf>,
}

struct SingleWorkspaceRemoveArgs {
    workspace: WorkspaceNameBuf,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_remove(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspaceRemoveArgs,
) -> Result<(), CommandError> {
    let initial = command.workspace_helper_no_snapshot(ui).await?;
    if !args.force {
        initial
            .check_working_copy_writable()
            .map_err(|err| err.hinted(USE_WORKSPACE_FORGET_HINT))?;
    }
    let _lifecycle_lock = super::lock_workspace_lifecycle(initial.repo_path())?;
    let workspace_command = command.workspace_helper_no_snapshot(ui).await?;
    let workspace_store = workspace_command.repo().loader().workspace_store().clone();

    let mut ordinary = Vec::new();
    let mut guarded = Vec::new();
    let mut seen: Vec<WorkspaceNameBuf> = Vec::new();
    let mut _subvolume_locks = Vec::new();
    let mut subvolume_lock_paths = Vec::new();
    for name in &args.workspaces {
        if seen.contains(name) {
            continue;
        }
        seen.push(name.clone());
        if args.force {
            if name.as_str() == workspace_command.workspace_name().as_str() {
                return Err(user_error(
                    "Cannot remove the current workspace; run this command from another workspace",
                ));
            }
            let is_tracked = workspace_command
                .repo()
                .view()
                .get_wc_commit_id(name)
                .is_some();
            let stored_path = workspace_store.get_workspace_path(name)?;
            if !is_tracked && stored_path.is_none() {
                return Err(user_error(format!(
                    "No such workspace: {}",
                    name.as_symbol()
                )));
            }
            if let Some(stored_path) = stored_path {
                let path =
                    normalize_absolute_path(&workspace_command.repo_path().join(stored_path));
                let parent = path
                    .parent()
                    .ok_or_else(|| user_error("Workspace path has no parent"))?;
                match dunce::canonicalize(parent) {
                    Ok(parent) => {
                        let name_part = path
                            .file_name()
                            .ok_or_else(|| user_error("Workspace path has no final component"))?;
                        lock_subvolume_operation(
                            name,
                            &parent.join(name_part),
                            &mut subvolume_lock_paths,
                            &mut _subvolume_locks,
                        )?;
                    }
                    // No process can hold the sibling lock when the parent is
                    // absent, and force must still clear stale registrations.
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err).context(parent).map_err(Into::into),
                }
            }
            guarded.push(name.clone());
            continue;
        }
        if workspace_command
            .repo()
            .view()
            .get_wc_commit_id(name)
            .is_none()
        {
            writeln!(
                ui.warning_default(),
                "No such workspace: {}",
                name.as_symbol()
            )?;
            continue;
        }
        let target_before_lock = preflight_workspace_removal(
            command,
            &workspace_command,
            workspace_store.as_ref(),
            name.as_ref(),
            true,
        )?;
        // Mode transitions replace the checkout directory under a separate
        // lock. Hold that lock for every target, including ordinary ones, so
        // one cannot turn into a snapshot workspace after classification.
        // `try_lock` avoids a lock-order cycle with an in-progress transition.
        lock_subvolume_operation(
            name,
            &target_before_lock.path,
            &mut subvolume_lock_paths,
            &mut _subvolume_locks,
        )?;
        let target = preflight_workspace_removal(
            command,
            &workspace_command,
            workspace_store.as_ref(),
            name.as_ref(),
            true,
        )?;
        if target.path != target_before_lock.path || target.identity != target_before_lock.identity
        {
            return Err(unsafe_removal_error(
                name,
                "its filesystem identity changed during validation",
            ));
        }
        let requires_guard = is_subvolume_mode_enabled(&target.path)
            || is_btrfs_subvolume_for_removal(&target.path)?;
        // An ordinary checkout can still have AWACS data in its linked Git
        // administrative directory. Route it through the guarded cleanup too.
        #[cfg(feature = "git")]
        let requires_guard = {
            let has_awacs_git_state = linked_git_worktree_cleanup(&workspace_command, &target)?
                .is_some_and(|cleanup| cleanup.awacs_identity.is_some());
            requires_guard || has_awacs_git_state
        };
        if requires_guard {
            if name.as_str() == workspace_command.workspace_name().as_str() {
                return Err(user_error(
                    "Cannot remove the current workspace; run this command from another workspace",
                ));
            }
            // Test all guarded workspaces before deleting any target. Each is
            // checked again just before deletion to catch later changes.
            reject_dirty_removal_target(ui, command, &workspace_command, name.as_ref(), &target)
                .await?;
            guarded.push(name.clone());
        } else {
            ordinary.push(name.clone());
        }
    }
    drop(workspace_command);
    drop(initial);

    if ordinary.is_empty() && guarded.is_empty() {
        writeln!(ui.status(), "Nothing changed.")?;
        return Ok(());
    }

    // Remove guarded targets first, as an ordinary target may be the current
    // workspace and would then leave no workspace from which to run the rest.
    for name in guarded {
        let single = SingleWorkspaceRemoveArgs { workspace: name };
        if args.force {
            cmd_workspace_remove_force(ui, command, &single, workspace_store.as_ref()).await?;
        } else {
            cmd_workspace_remove_guarded(ui, command, &single, workspace_store.as_ref()).await?;
        }
    }
    if !ordinary.is_empty() {
        // Preflight again under the same lifecycle lock after the guarded
        // removals, including a fresh check that each target still uses the
        // ordinary path. Never route a changed target into upstream deletion.
        let check = command.workspace_helper_no_snapshot(ui).await?;
        for name in &ordinary {
            let target = preflight_workspace_removal(
                command,
                &check,
                workspace_store.as_ref(),
                name.as_ref(),
                true,
            )?;
            let requires_guard = is_subvolume_mode_enabled(&target.path)
                || is_btrfs_subvolume_for_removal(&target.path)?;
            #[cfg(feature = "git")]
            let requires_guard = {
                let has_awacs_git_state = linked_git_worktree_cleanup(&check, &target)?
                    .is_some_and(|cleanup| cleanup.awacs_identity.is_some());
                requires_guard || has_awacs_git_state
            };
            if requires_guard {
                return Err(unsafe_removal_error(
                    name,
                    "its deletion mode changed during removal",
                ));
            }
        }
        drop(check);
        cmd_workspace_remove_ordinary(
            ui,
            command,
            &WorkspaceRemoveArgs {
                force: false,
                workspaces: ordinary,
            },
            workspace_store.as_ref(),
        )
        .await?;
    }
    Ok(())
}

fn lock_subvolume_operation(
    name: &WorkspaceName,
    workspace_path: &Path,
    locked_paths: &mut Vec<PathBuf>,
    locks: &mut Vec<FileLock>,
) -> Result<(), CommandError> {
    let lock_path = subvolume_operation_lock_path(workspace_path)?;
    if !locked_paths.contains(&lock_path) {
        let lock = FileLock::try_lock(lock_path.clone())
            .map_err(|err| {
                user_error(format!(
                    "Failed to lock subvolume operation at {}: {err}",
                    lock_path.display()
                ))
            })?
            .ok_or_else(|| unsafe_removal_error(name, "a subvolume operation is in progress"))?;
        locked_paths.push(lock_path);
        locks.push(lock);
    }
    Ok(())
}

async fn cmd_workspace_remove_guarded(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SingleWorkspaceRemoveArgs,
    workspace_store: &dyn WorkspaceStore,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    let target = preflight_workspace_removal(
        command,
        &workspace_command,
        workspace_store,
        args.workspace.as_ref(),
        false,
    )?;
    reject_dirty_removal_target(
        ui,
        command,
        &workspace_command,
        args.workspace.as_ref(),
        &target,
    )
    .await?;

    revalidate_removal_target(
        &workspace_command,
        workspace_store,
        args.workspace.as_ref(),
        &target,
    )?;
    let verified_btrfs_subvolume = is_btrfs_subvolume_for_removal(&target.path)?;
    #[cfg(feature = "git")]
    let git_cleanup = linked_git_worktree_cleanup(&workspace_command, &target)?;

    // Prepare the repo edit in memory before claiming or deleting the target
    // so a transaction-preparation failure leaves both the pathname and
    // workspace registration untouched.
    let mut tx = workspace_command.start_transaction();
    tx.repo_mut().remove_workspace(&args.workspace).await?;

    // Rename the already-verified directory to an unpredictable sibling
    // before deleting anything. Subsequent traversal is rooted in that claim
    // instead of the mutable registered pathname.
    let claimed = claim_removal_target(args.workspace.as_ref(), &target)?;

    let deletion_result = if verified_btrfs_subvolume {
        // The target was renamed to an unpredictable sibling after its
        // identity was verified, so deleting that claimed pathname is
        // race-safe. Avoid `--subvolid`: btrfs-progs searches the B-tree
        // to resolve it, which can require privileges even when
        // `user_subvol_rm_allowed` permits pathname deletion.
        delete_btrfs_subvolume(&claimed.path)
            .and_then(|deleted| {
                if deleted {
                    Ok(())
                } else {
                    Err(user_error(
                        "Failed to delete Btrfs subvolume: claimed path is not on Btrfs",
                    ))
                }
            })
            .map_err(|mut err| {
                // Failure restores the claimed directory to this
                // original target path before the hint is shown.
                add_btrfs_delete_hint(&mut err, &target.path);
                err
            })
    } else {
        delete_claimed_directory(&claimed.path, &target.identity)
    };
    if let Err(mut err) = deletion_result {
        if let Err(restore_err) = restore_claimed_target(&target, &claimed) {
            err.add_hint(format!(
                "The verified workspace remains at {} because restoring its original name failed: \
                 {restore_err:?}",
                claimed.path.display()
            ));
        }
        return Err(err);
    }

    #[cfg(feature = "git")]
    let git_cleanup_error = git_cleanup
        .as_ref()
        .and_then(|cleanup| cleanup_linked_git_worktree(cleanup).err());
    workspace_store.forget(&[args.workspace.as_ref()])?;
    tx.finish(
        ui,
        format!("remove workspace {}", args.workspace.as_symbol()),
    )
    .await?;
    #[cfg(feature = "git")]
    if let Some(err) = git_cleanup_error {
        writeln!(
            ui.warning_default(),
            "Warning: Removed the workspace, but failed to prune its linked Git worktree \
             metadata: {err:?}"
        )?;
    }
    writeln!(
        ui.status(),
        "Removed workspace at \"{}\"",
        target.path.display()
    )?;
    Ok(())
}

async fn cmd_workspace_remove_force(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SingleWorkspaceRemoveArgs,
    workspace_store: &dyn WorkspaceStore,
) -> Result<(), CommandError> {
    // Force removal is also the recovery path for stale registrations, so do
    // not snapshot the invoking working copy or try to load the target.
    let mut workspace_command = command.workspace_helper_no_snapshot(ui).await?;

    if args.workspace.as_str() == workspace_command.workspace_name().as_str() {
        return Err(user_error(
            "Cannot remove the current workspace; run this command from another workspace",
        ));
    }

    let stored_path = workspace_store.get_workspace_path(args.workspace.as_ref())?;
    let is_tracked = workspace_command
        .repo()
        .view()
        .get_wc_commit_id(args.workspace.as_ref())
        .is_some();
    if !is_tracked && stored_path.is_none() {
        return Err(user_error(format!(
            "No such workspace: {}",
            args.workspace.as_symbol()
        )));
    }

    let registered_path = stored_path.as_ref().map(|stored_path| {
        normalize_absolute_path(&workspace_command.repo_path().join(stored_path))
    });
    let mut tx = if is_tracked {
        let mut tx = workspace_command.start_transaction();
        tx.repo_mut().remove_workspace(&args.workspace).await?;
        Some(tx)
    } else {
        None
    };

    let removed_path = if let Some(path) = registered_path.as_deref() {
        force_delete_registered_path(args.workspace.as_ref(), path)?
    } else {
        false
    };
    workspace_store.forget(&[args.workspace.as_ref()])?;
    if let Some(tx) = tx.take() {
        tx.finish(
            ui,
            format!("force remove workspace {}", args.workspace.as_symbol()),
        )
        .await?;
    }

    match (registered_path, removed_path) {
        (Some(path), true) => writeln!(ui.status(), "Removed workspace at \"{}\"", path.display())?,
        (Some(path), false) => writeln!(
            ui.status(),
            "Removed workspace registration; no path existed at \"{}\"",
            path.display()
        )?,
        (None, _) => writeln!(
            ui.status(),
            "Removed workspace registration for {}",
            args.workspace.as_symbol()
        )?,
    }
    Ok(())
}

struct RemovalTarget {
    stored_path: PathBuf,
    registered_path: PathBuf,
    path: PathBuf,
    identity: FileIdentity,
}

struct ClaimedRemovalTarget {
    path: PathBuf,
}

/// Deletes the recorded force-removal path without following symlinks.
///
/// The force path intentionally skips workspace identity, overlap, and dirty
/// checks. It still claims a directory before recursive deletion so a
/// pathname replacement cannot redirect deletion after the command starts.
fn force_delete_registered_path(
    workspace_name: &WorkspaceName,
    path: &Path,
) -> Result<bool, CommandError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).context(path).map_err(Into::into),
    };
    if !metadata.is_dir() {
        fs::remove_file(path).context(path)?;
        return Ok(true);
    }

    let identity = path_identity(path)?;
    let target = RemovalTarget {
        stored_path: PathBuf::new(),
        registered_path: path.to_owned(),
        path: path.to_owned(),
        identity,
    };
    let verified_btrfs_subvolume = is_btrfs_subvolume_for_removal(path)?;
    let claimed = claim_removal_target(workspace_name, &target)?;
    let deletion_result = if verified_btrfs_subvolume {
        delete_btrfs_subvolume(&claimed.path)
            .and_then(|deleted| {
                if deleted {
                    Ok(())
                } else {
                    Err(user_error(
                        "Failed to delete Btrfs subvolume: claimed path is not on Btrfs",
                    ))
                }
            })
            .map_err(|mut err| {
                add_btrfs_delete_hint(&mut err, path);
                err
            })
    } else {
        delete_claimed_directory(&claimed.path, &target.identity)
    };
    if let Err(mut err) = deletion_result {
        if let Err(restore_err) = restore_claimed_target(&target, &claimed) {
            err.add_hint(format!(
                "The force-removal target remains at {} because restoring its original name failed: {restore_err:?}",
                claimed.path.display()
            ));
        }
        return Err(err);
    }
    Ok(true)
}

/// Snapshots the target in memory, then discards the lock instead of
/// publishing a working-copy operation. Ordinary filesystem monitoring is
/// disabled, while snapshot-backed workspaces still use their AWACS baseline.
/// Removal must not silently destroy files absent from the recorded commit.
async fn reject_dirty_removal_target(
    ui: &Ui,
    command: &CommandHelper,
    workspace_command: &WorkspaceCommandHelper,
    workspace_name: &WorkspaceName,
    target: &RemovalTarget,
) -> Result<(), CommandError> {
    let mut config = workspace_command.settings().config().clone();
    let direct_scan_layer =
        ConfigLayer::parse(ConfigSource::CommandArg, "fsmonitor.backend = \"none\"")
            .expect("the built-in direct-scan config must parse");
    config.add_layer(direct_scan_layer);
    let settings = workspace_command.settings().with_new_config(config)?;
    let workspace = command.load_workspace_at(&target.path, &settings)?;
    if workspace.workspace_name().as_str() != workspace_name.as_str() {
        return Err(unsafe_removal_error(
            workspace_name,
            "its registered path identifies a different workspace before the dirty check",
        ));
    }

    let mut target_command =
        command.for_workable_repo(ui, workspace, workspace_command.repo().clone())?;
    let auto_tracking_matcher = target_command.auto_tracking_matcher(ui)?;
    let options = target_command
        .snapshot_options_with_start_tracking_matcher(auto_tracking_matcher.as_ref())?;
    let wc_commit_id = target_command
        .get_wc_commit_id()
        .cloned()
        .ok_or_else(|| unsafe_removal_error(workspace_name, "it has no recorded working copy"))?;
    let wc_commit = target_command
        .repo()
        .store()
        .get_commit_async(&wc_commit_id)
        .await?;
    let (mut locked_ws, _) = target_command.start_working_copy_mutation().await?;
    if wc_commit.tree().tree_ids_and_labels()
        != locked_ws.locked_wc().old_tree().tree_ids_and_labels()
    {
        return Err(user_error(
            "Concurrent working copy operation in target workspace. Try again.",
        ));
    }
    let (live_tree, _) = locked_ws.locked_wc().snapshot(&options).await?;
    if live_tree.tree_ids_and_labels() != wc_commit.tree().tree_ids_and_labels() {
        return Err(unsafe_removal_error(
            workspace_name,
            "its working copy has uncommitted changes",
        ));
    }
    Ok(())
}

/// Atomically moves the verified target to an unpredictable sibling name.
///
/// The registered pathname is never used for deletion after this point. On
/// Linux, renameat2(RENAME_NOREPLACE) also prevents replacing an attacker
/// supplied destination during the claim.
fn claim_removal_target(
    workspace_name: &WorkspaceName,
    target: &RemovalTarget,
) -> Result<ClaimedRemovalTarget, CommandError> {
    let parent = target
        .path
        .parent()
        .ok_or_else(|| unsafe_removal_error(workspace_name, "its path has no parent directory"))?;
    let source_name = target
        .path
        .file_name()
        .ok_or_else(|| unsafe_removal_error(workspace_name, "its path has no final component"))?;
    let parent_file = fs::File::open(parent).context(parent)?;

    for _ in 0..32 {
        let claimed_name =
            std::ffi::OsString::from(format!(".jj-removing-{:032x}", rand::random::<u128>()));
        let claimed_path = parent.join(&claimed_name);
        if !atomic_claim_rename(
            &parent_file,
            source_name,
            &claimed_name,
            &target.path,
            &claimed_path,
        )? {
            continue;
        }
        let claimed = ClaimedRemovalTarget { path: claimed_path };
        if &path_identity(&claimed.path)? != &target.identity {
            drop(restore_claimed_target(target, &claimed));
            return Err(unsafe_removal_error(
                workspace_name,
                "its filesystem identity changed while claiming it for deletion",
            ));
        }
        return Ok(claimed);
    }
    Err(unsafe_removal_error(
        workspace_name,
        "could not reserve a private deletion name",
    ))
}

fn atomic_claim_rename(
    parent: &fs::File,
    source_name: &OsStr,
    claimed_name: &OsStr,
    source_path: &Path,
    claimed_path: &Path,
) -> Result<bool, CommandError> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let _ = (source_path, claimed_path);
        return match renameat2(
            parent,
            source_name,
            parent,
            claimed_name,
            RenameFlags::RENAME_NOREPLACE,
        ) {
            Ok(()) => Ok(true),
            Err(nix::errno::Errno::EEXIST) => Ok(false),
            Err(err) => Err(user_error(format!(
                "Failed to atomically claim workspace directory: {err}"
            ))),
        };
    }

    #[cfg(all(unix, not(all(target_os = "linux", target_env = "gnu"))))]
    {
        let _ = (source_path, claimed_path);
        return renameat(parent, source_name, parent, claimed_name)
            .map(|()| true)
            .map_err(|err| {
                user_error(format!(
                    "Failed to atomically claim workspace directory: {err}"
                ))
            });
    }

    #[cfg(not(unix))]
    {
        let _ = (parent, source_name, claimed_name);
        // Windows does not replace an existing directory during rename. The
        // random name plus the post-rename identity check keeps this
        // best-effort fallback from deleting a different object.
        match fs::rename(source_path, claimed_path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(err) => Err(err).context(source_path).map_err(Into::into),
        }
    }
}

fn restore_claimed_target(
    target: &RemovalTarget,
    claimed: &ClaimedRemovalTarget,
) -> Result<(), CommandError> {
    if target.path.exists() {
        return Err(user_error(format!(
            "Cannot restore workspace path because {} now exists",
            target.path.display()
        )));
    }
    fs::rename(&claimed.path, &target.path).context(&claimed.path)?;
    Ok(())
}

fn delete_claimed_directory(
    path: &Path,
    expected_identity: &FileIdentity,
) -> Result<(), CommandError> {
    #[cfg(unix)]
    {
        delete_claimed_directory_unix(path, expected_identity)
    }

    #[cfg(not(unix))]
    {
        if &path_identity(path)? != expected_identity {
            return Err(user_error(
                "Claimed workspace identity changed before deletion",
            ));
        }
        fs::remove_dir_all(path).context(path)?;
        Ok(())
    }
}

#[cfg(unix)]
fn delete_claimed_directory_unix(
    path: &Path,
    expected_identity: &FileIdentity,
) -> Result<(), CommandError> {
    let parent = path
        .parent()
        .ok_or_else(|| user_error("Claimed workspace path has no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| user_error("Claimed workspace path has no final component"))?;
    let parent_file = fs::File::open(parent).context(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context(path)?;
    let opened_identity = FileIdentity::from_file(file.try_clone().context(path)?).context(path)?;
    if &opened_identity != expected_identity {
        return Err(user_error(
            "Claimed workspace identity changed before deletion",
        ));
    }
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    {
        btrfs_awacs::btrfs::remove_directory_at(parent_file.as_fd(), name, file.as_fd())
            .context(path)?;
        Ok(())
    }
    #[cfg(not(all(target_os = "linux", feature = "awacs")))]
    {
        let owned_fd: OwnedFd = file.into();
        let mut dir = Dir::from_fd(owned_fd)
            .map_err(|err| user_error(format!("Failed to open claimed workspace: {err}")))?;
        delete_directory_contents(&mut dir, path)?;
        if &path_identity(path)? != expected_identity {
            return Err(user_error(
                "Claimed workspace identity changed during deletion",
            ));
        }
        unlinkat(&parent_file, name, UnlinkatFlags::RemoveDir)
            .map_err(|err| user_error(format!("Failed to remove {}: {err}", path.display())))?;
        Ok(())
    }
}

#[cfg(all(unix, not(all(target_os = "linux", feature = "awacs"))))]
fn delete_directory_contents(dir: &mut Dir, display_path: &Path) -> Result<(), CommandError> {
    let entry_names = dir
        .iter()
        .map(|entry| {
            let entry = entry.map_err(|err| {
                user_error(format!(
                    "Failed to read claimed workspace {}: {err}",
                    display_path.display()
                ))
            })?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                Ok(None)
            } else {
                Ok(Some(OsString::from_vec(bytes.to_vec())))
            }
        })
        .collect::<Result<Vec<_>, CommandError>>()?;

    for entry_name in entry_names.into_iter().flatten() {
        let child_path = display_path.join(&entry_name);
        let stat = match fstatat(&*dir, entry_name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(nix::errno::Errno::ENOENT) => continue,
            Err(err) => {
                return Err(user_error(format!(
                    "Failed to inspect {}: {err}",
                    child_path.display()
                )));
            }
        };
        let file_type = SFlag::from_bits_truncate(stat.st_mode);
        if file_type.contains(SFlag::S_IFDIR) {
            let mut child_dir = Dir::openat(
                &*dir,
                entry_name.as_os_str(),
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(|err| {
                user_error(format!(
                    "Failed to open {} for deletion: {err}",
                    child_path.display()
                ))
            })?;
            delete_directory_contents(&mut child_dir, &child_path)?;
            unlinkat(&*dir, entry_name.as_os_str(), UnlinkatFlags::RemoveDir).map_err(|err| {
                user_error(format!("Failed to remove {}: {err}", child_path.display()))
            })?;
        } else {
            unlinkat(&*dir, entry_name.as_os_str(), UnlinkatFlags::NoRemoveDir).map_err(|err| {
                user_error(format!("Failed to remove {}: {err}", child_path.display()))
            })?;
        }
    }
    Ok(())
}

fn add_btrfs_delete_hint(err: &mut CommandError, path: &Path) {
    let mount_hint = match btrfs_user_subvol_rm_allowed(path) {
        Ok(Some(false)) => "The Btrfs filesystem is not mounted with `user_subvol_rm_allowed`.",
        Ok(Some(true)) => {
            "The Btrfs filesystem is mounted with `user_subvol_rm_allowed`; check the subvolume \
             ownership and parent-directory permissions."
        }
        Ok(None) | Err(_) => {
            "Check whether the Btrfs filesystem is mounted with `user_subvol_rm_allowed`."
        }
    };
    let display_path = path.to_string_lossy();
    let quoted_path = shlex::try_quote(&display_path).unwrap_or_else(|_| display_path.clone());
    err.add_hint(format!(
        "{mount_hint} The workspace was not forgotten. Run `sudo btrfs subvolume delete \
         {quoted_path}` only after deciding whether to forget its registration."
    ));
}

#[cfg(feature = "git")]
#[derive(Debug)]
struct GitWorktreeCleanup {
    git_repo_path: PathBuf,
    git_executable: PathBuf,
    lock_root: PathBuf,
    worktree_path: PathBuf,
    common_dir: PathBuf,
    admin_dir: PathBuf,
    admin_identity: FileIdentity,
    gitdir_contents: Vec<u8>,
    awacs_identity: Option<FileIdentity>,
}

#[cfg(feature = "git")]
fn linked_git_worktree_cleanup(
    workspace_command: &WorkspaceCommandHelper,
    target: &RemovalTarget,
) -> Result<Option<GitWorktreeCleanup>, CommandError> {
    if !target.path.join(".git").is_file() {
        return Ok(None);
    }
    let Ok(git_backend) = jj_lib::git::get_git_backend(workspace_command.repo().store()) else {
        return Ok(None);
    };
    let git_repo = gix::open(&target.path)
        .map_err(|err| user_error(format!("Failed to inspect linked Git worktree: {err}")))?;
    let common_dir = canonicalize_path(
        git_backend.git_repo().common_dir(),
        "Cannot resolve the shared Git administrative directory",
    )?;
    let target_common_dir = canonicalize_path(
        git_repo.common_dir(),
        "Cannot resolve the linked worktree's shared Git directory",
    )?;
    let admin_dir = canonicalize_path(
        git_repo.git_dir(),
        "Cannot resolve the linked Git worktree administrative directory",
    )?;
    if target_common_dir != common_dir
        || admin_dir.parent() != Some(common_dir.join("worktrees").as_path())
    {
        return Err(user_error(
            "The linked Git worktree administrative directory is not in this repository's \
             worktrees directory",
        ));
    }
    let admin_identity = FileIdentity::from_symlink_path(&admin_dir).context(&admin_dir)?;
    let gitdir_path = admin_dir.join("gitdir");
    let gitdir_contents = fs::read(&gitdir_path).context(&gitdir_path)?;
    let backlink = jj_lib::file_util::path_from_bytes(
        gitdir_contents
            .strip_suffix(b"\n")
            .unwrap_or(&gitdir_contents),
    )
    .map_err(|err| user_error(format!("Invalid linked Git worktree backlink: {err}")))?;
    if normalize_absolute_path(&admin_dir.join(backlink)) != target.path.join(".git") {
        return Err(user_error(
            "The linked Git worktree administrative directory refers to a different worktree",
        ));
    }
    let awacs_dir = admin_dir.join("awacs");
    let awacs_identity = match fs::symlink_metadata(&awacs_dir) {
        Ok(metadata) if metadata.is_dir() => {
            Some(FileIdentity::from_symlink_path(&awacs_dir).context(&awacs_dir)?)
        }
        Ok(_) => {
            return Err(user_error(format!(
                "Cannot retire non-directory AWACS state at {}",
                awacs_dir.display()
            )));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).context(&awacs_dir).map_err(Into::into),
    };
    Ok(Some(GitWorktreeCleanup {
        git_repo_path: git_backend.git_repo_path().to_owned(),
        git_executable: jj_lib::git::GitSettings::from_settings(workspace_command.settings())?
            .executable_path,
        lock_root: workspace_command.repo_path().to_owned(),
        worktree_path: target.path.clone(),
        common_dir,
        admin_dir,
        admin_identity,
        gitdir_contents,
        awacs_identity,
    }))
}

#[cfg(feature = "git")]
fn cleanup_linked_git_worktree(cleanup: &GitWorktreeCleanup) -> Result<(), CommandError> {
    let lock_path = cleanup.lock_root.join("git_import_export.lock");
    let _lock = FileLock::lock(lock_path.clone()).map_err(|err| {
        user_error(format!(
            "Failed to take lock for Git import/export at {}: {err}",
            lock_path.display()
        ))
    })?;
    verify_removed_git_worktree(cleanup)?;
    retire_git_worktree_awacs(cleanup)?;
    verify_removed_git_worktree(cleanup)?;
    let output = Command::new(&cleanup.git_executable)
        .arg("--git-dir")
        .arg(&cleanup.git_repo_path)
        .args(["worktree", "prune", "--expire", "now"])
        .output()
        .map_err(|err| user_error(format!("Failed to prune Git worktree metadata: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(user_error(format!(
            "Failed to prune Git worktree metadata: {stderr}"
        )));
    }
    // Git's prune command can return success even when deleting an admin
    // directory failed, so its exit status alone does not prove cleanup.
    match fs::symlink_metadata(&cleanup.admin_dir) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context(&cleanup.admin_dir).map_err(Into::into),
        Ok(_) => {
            return Err(user_error(format!(
                "Git worktree prune left administrative directory {} in place: {}",
                cleanup.admin_dir.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(())
}

#[cfg(feature = "git")]
fn verify_removed_git_worktree(cleanup: &GitWorktreeCleanup) -> Result<(), CommandError> {
    match fs::symlink_metadata(&cleanup.worktree_path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context(&cleanup.worktree_path).map_err(Into::into),
        Ok(_) => {
            return Err(user_error(format!(
                "Cannot clean up linked Git worktree metadata because {} reappeared",
                cleanup.worktree_path.display()
            )));
        }
    }
    let identity =
        FileIdentity::from_symlink_path(&cleanup.admin_dir).context(&cleanup.admin_dir)?;
    if identity != cleanup.admin_identity {
        return Err(user_error(
            "The linked Git worktree administrative directory changed before cleanup",
        ));
    }
    let gitdir_path = cleanup.admin_dir.join("gitdir");
    if fs::read(&gitdir_path).context(&gitdir_path)? != cleanup.gitdir_contents {
        return Err(user_error(
            "The linked Git worktree backlink changed before cleanup",
        ));
    }
    Ok(())
}

#[cfg(feature = "git")]
fn retire_git_worktree_awacs(cleanup: &GitWorktreeCleanup) -> Result<(), CommandError> {
    let source = cleanup.admin_dir.join("awacs");
    let observed_identity = match FileIdentity::from_symlink_path(&source) {
        Ok(identity) => Some(identity),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).context(&source).map_err(Into::into),
    };
    if observed_identity != cleanup.awacs_identity {
        return Err(user_error(
            "The linked Git worktree's AWACS state changed before cleanup",
        ));
    }
    if observed_identity.is_none() {
        return Ok(());
    }

    // The worktree is already gone. Keep its private broker trash and nested
    // snapshots together without traversing them, under the main Git AWACS
    // directory that subvolume disable already discards. Both paths are in
    // the same Git metadata subvolume, so this is a rename, never a copy.
    let retirement_parent = cleanup.common_dir.join("awacs");
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    match builder.create(&retirement_parent) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err).context(&retirement_parent).map_err(Into::into),
    }
    if !fs::symlink_metadata(&retirement_parent)
        .context(&retirement_parent)?
        .is_dir()
    {
        return Err(user_error(format!(
            "Cannot retire AWACS state into non-directory {}",
            retirement_parent.display()
        )));
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let (source_parent, destination_parent) = {
        let open_directory = |path: &Path| {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .context(path)
        };
        let source_parent = open_directory(&cleanup.admin_dir)?;
        if FileIdentity::from_file(source_parent.try_clone().context(&cleanup.admin_dir)?)
            .context(&cleanup.admin_dir)?
            != cleanup.admin_identity
        {
            return Err(user_error(
                "The linked Git worktree administrative directory changed before retirement",
            ));
        }
        (source_parent, open_directory(&retirement_parent)?)
    };
    for _ in 0..32 {
        let name = format!(".retired-worktree-{:032x}", rand::random::<u128>());
        let destination = retirement_parent.join(&name);
        verify_removed_git_worktree(cleanup)?;
        if Some(FileIdentity::from_symlink_path(&source).context(&source)?)
            != cleanup.awacs_identity
        {
            return Err(user_error(
                "The linked Git worktree's AWACS state changed before retirement",
            ));
        }
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        let renamed = match renameat2(
            &source_parent,
            OsStr::new("awacs"),
            &destination_parent,
            OsStr::new(&name),
            RenameFlags::RENAME_NOREPLACE,
        ) {
            Ok(()) => true,
            Err(nix::errno::Errno::EEXIST) => false,
            Err(err) => {
                return Err(user_error(format!("Failed to retire AWACS state: {err}")));
            }
        };
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        let renamed = if destination.try_exists().context(&destination)? {
            false
        } else {
            fs::rename(&source, &destination).context(&source)?;
            true
        };
        if renamed {
            let retired_identity =
                FileIdentity::from_symlink_path(&destination).context(&destination)?;
            if Some(retired_identity) != cleanup.awacs_identity {
                return Err(user_error(format!(
                    "AWACS state changed during retirement; preserved it at {}",
                    destination.display()
                )));
            }
            return Ok(());
        }
    }
    Err(user_error(
        "Could not reserve an AWACS retirement directory",
    ))
}

fn preflight_workspace_removal(
    command: &CommandHelper,
    workspace_command: &WorkspaceCommandHelper,
    workspace_store: &dyn WorkspaceStore,
    workspace_name: &WorkspaceName,
    allow_current: bool,
) -> Result<RemovalTarget, CommandError> {
    if !allow_current && workspace_name.as_str() == workspace_command.workspace_name().as_str() {
        return Err(user_error(
            "Cannot remove the current workspace; run this command from another workspace",
        ));
    }
    if workspace_command
        .repo()
        .view()
        .get_wc_commit_id(workspace_name)
        .is_none()
    {
        return Err(user_error(format!(
            "No such workspace: {}",
            workspace_name.as_symbol()
        )));
    }

    let target =
        resolve_registered_workspace(command, workspace_command, workspace_store, workspace_name)?;
    let shared_repo_path = canonicalize_path(
        workspace_command.repo_path(),
        "Cannot resolve the shared Jujutsu repository",
    )?;
    if path_contains(&target.path, &shared_repo_path) {
        return Err(unsafe_removal_error(
            workspace_name,
            "its directory contains the shared Jujutsu repository",
        ));
    }

    #[cfg(feature = "git")]
    if let Ok(git_backend) = jj_lib::git::get_git_backend(workspace_command.repo().store()) {
        let git_repo_path = canonicalize_path(
            git_backend.git_repo_path(),
            "Cannot resolve the shared Git repository",
        )?;
        if path_contains(&target.path, &git_repo_path) {
            return Err(unsafe_removal_error(
                workspace_name,
                "its directory contains the shared Git repository",
            ));
        }
    }

    for (survivor_name, _) in workspace_command.repo().view().wc_commit_ids() {
        if survivor_name.as_str() == workspace_name.as_str() {
            continue;
        }
        let Some(survivor) = resolve_surviving_workspace(
            workspace_command,
            workspace_store,
            survivor_name.as_ref(),
        )?
        else {
            continue;
        };
        if target.identity == survivor.identity
            || path_contains(&target.path, &survivor.path)
            || path_contains(&survivor.path, &target.path)
        {
            return Err(unsafe_removal_error(
                workspace_name,
                format!(
                    "its directory overlaps surviving workspace {}",
                    survivor_name.as_symbol()
                ),
            ));
        }
    }

    Ok(target)
}

fn resolve_surviving_workspace(
    workspace_command: &WorkspaceCommandHelper,
    workspace_store: &dyn WorkspaceStore,
    workspace_name: &WorkspaceName,
) -> Result<Option<RemovalTarget>, CommandError> {
    if workspace_name.as_str() == workspace_command.workspace_name().as_str() {
        // Repositories created before workspace paths were recorded may not
        // have a store entry for the invoking workspace. Its loaded root is
        // still authoritative enough for the overlap check.
        return resolve_loaded_workspace(workspace_command).map(Some);
    }

    let Some(stored_path) = workspace_store.get_workspace_path(workspace_name)? else {
        // An unrecorded non-current workspace has no path that could overlap
        // the live target. Keep its stale working-copy registration for an
        // explicit `workspace forget`, but do not block unrelated removal.
        return Ok(None);
    };
    let registered_path =
        normalize_absolute_path(&workspace_command.repo_path().join(&stored_path));
    match fs::symlink_metadata(&registered_path) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // A missing registered path likewise cannot overlap the target.
            // The stale registration remains visible for explicit cleanup.
            return Ok(None);
        }
        Err(err) => {
            return Err(err).context(&registered_path).map_err(Into::into);
        }
    }
    // Survivor validation only needs a stable path and filesystem identity for
    // overlap checks. An unrelated stale registration must not block removal
    // merely because its path is no longer a loadable JJ workspace.
    resolve_registered_survivor_path(workspace_command, workspace_store, workspace_name).map(Some)
}

fn resolve_registered_survivor_path(
    workspace_command: &WorkspaceCommandHelper,
    workspace_store: &dyn WorkspaceStore,
    workspace_name: &WorkspaceName,
) -> Result<RemovalTarget, CommandError> {
    let stored_path = workspace_store
        .get_workspace_path(workspace_name)?
        .ok_or_else(|| {
            user_error(format!(
                "Workspace has no recorded path: {}",
                workspace_name.as_symbol()
            ))
        })?;
    let registered_path =
        normalize_absolute_path(&workspace_command.repo_path().join(&stored_path));
    reject_symlink_components(workspace_name, &registered_path)?;
    let path = canonicalize_path(
        &registered_path,
        format!(
            "Cannot resolve absolute workspace path: {}",
            registered_path.display()
        ),
    )?;
    let identity = path_identity(&path)?;
    Ok(RemovalTarget {
        stored_path,
        registered_path,
        path,
        identity,
    })
}

fn resolve_loaded_workspace(
    workspace_command: &WorkspaceCommandHelper,
) -> Result<RemovalTarget, CommandError> {
    let path = canonicalize_path(
        workspace_command.workspace().workspace_root(),
        "Cannot resolve current workspace path",
    )?;
    let identity = path_identity(&path)?;
    Ok(RemovalTarget {
        // These fields are only used when revalidating the removal target.
        // A loaded surviving workspace is never the target.
        stored_path: path.clone(),
        registered_path: path.clone(),
        path,
        identity,
    })
}

fn resolve_registered_workspace(
    command: &CommandHelper,
    workspace_command: &WorkspaceCommandHelper,
    workspace_store: &dyn WorkspaceStore,
    workspace_name: &WorkspaceName,
) -> Result<RemovalTarget, CommandError> {
    let stored_path = workspace_store
        .get_workspace_path(workspace_name)?
        .ok_or_else(|| {
            user_error(format!(
                "Workspace has no recorded path: {}",
                workspace_name.as_symbol()
            ))
        })?;
    let registered_path =
        normalize_absolute_path(&workspace_command.repo_path().join(&stored_path));
    reject_symlink_components(workspace_name, &registered_path)?;
    let metadata = fs::symlink_metadata(&registered_path).context(&registered_path)?;
    if !metadata.is_dir() {
        return Err(unsafe_removal_error(
            workspace_name,
            "its registered path is not a directory",
        ));
    }
    let path = canonicalize_path(
        &registered_path,
        format!(
            "Cannot resolve absolute workspace path: {}",
            registered_path.display()
        ),
    )?;
    let target_workspace = command
        .load_workspace_at(&path, workspace_command.settings())
        .map_err(|_| {
            unsafe_removal_error(
                workspace_name,
                "its registered path no longer identifies a loadable workspace",
            )
        })?;
    if target_workspace.workspace_name().as_str() != workspace_name.as_str() {
        return Err(unsafe_removal_error(
            workspace_name,
            "its registered path identifies a different workspace",
        ));
    }
    let target_repo_path = canonicalize_path(
        target_workspace.repo_path(),
        "Cannot resolve target workspace repository",
    )?;
    let shared_repo_path = canonicalize_path(
        workspace_command.repo_path(),
        "Cannot resolve the shared Jujutsu repository",
    )?;
    if target_repo_path != shared_repo_path {
        return Err(unsafe_removal_error(
            workspace_name,
            "its registered path points at a different Jujutsu repository",
        ));
    }
    let identity = path_identity(&path)?;
    Ok(RemovalTarget {
        stored_path,
        registered_path,
        path,
        identity,
    })
}

fn revalidate_removal_target(
    workspace_command: &WorkspaceCommandHelper,
    workspace_store: &dyn WorkspaceStore,
    workspace_name: &WorkspaceName,
    target: &RemovalTarget,
) -> Result<(), CommandError> {
    let stored_path = workspace_store
        .get_workspace_path(workspace_name)?
        .ok_or_else(|| {
            unsafe_removal_error(
                workspace_name,
                "its registration disappeared before deletion",
            )
        })?;
    if stored_path != target.stored_path {
        return Err(unsafe_removal_error(
            workspace_name,
            "its registered path changed before deletion",
        ));
    }
    let registered_path =
        normalize_absolute_path(&workspace_command.repo_path().join(&stored_path));
    if registered_path != target.registered_path {
        return Err(unsafe_removal_error(
            workspace_name,
            "its registered path changed before deletion",
        ));
    }
    reject_symlink_components(workspace_name, &registered_path)?;
    let path = canonicalize_path(
        &registered_path,
        format!(
            "Cannot revalidate absolute workspace path: {}",
            registered_path.display()
        ),
    )?;
    if path != target.path || path_identity(&path)? != target.identity {
        return Err(unsafe_removal_error(
            workspace_name,
            "its filesystem identity changed before deletion",
        ));
    }
    Ok(())
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn reject_symlink_components(
    workspace_name: &WorkspaceName,
    path: &Path,
) -> Result<(), CommandError> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        let metadata = fs::symlink_metadata(&prefix).context(&prefix)?;
        if metadata.file_type().is_symlink() {
            return Err(unsafe_removal_error(
                workspace_name,
                "its registered path contains a symlink",
            ));
        }
    }
    Ok(())
}

fn canonicalize_path(path: &Path, message: impl Into<String>) -> Result<PathBuf, CommandError> {
    dunce::canonicalize(path).map_err(|err| user_error_with_message(message, err))
}

fn path_identity(path: &Path) -> Result<FileIdentity, CommandError> {
    let file = fs::File::open(path).context(path)?;
    Ok(FileIdentity::from_file(file).context(path)?)
}

fn path_contains(parent: &Path, child: &Path) -> bool {
    child == parent || child.starts_with(parent)
}

fn unsafe_removal_error(
    workspace_name: &WorkspaceName,
    reason: impl std::fmt::Display,
) -> CommandError {
    user_error(format!(
        "Cannot remove workspace {}: {reason}",
        workspace_name.as_symbol()
    ))
}

#[instrument(skip_all)]
async fn cmd_workspace_remove_ordinary(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspaceRemoveArgs,
    workspace_store: &dyn WorkspaceStore,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    workspace_command
        .check_working_copy_writable()
        .map_err(|err| err.hinted(USE_WORKSPACE_FORGET_HINT))?;

    let wss = args.workspaces.clone();

    let mut remove_ws = Vec::new();
    for ws in &wss {
        if workspace_command
            .repo()
            .view()
            .get_wc_commit_id(ws)
            .is_none()
        {
            writeln!(
                ui.warning_default(),
                "No such workspace: {}",
                ws.as_symbol()
            )?;
        } else {
            remove_ws.push(ws);
        }
    }
    if remove_ws.is_empty() {
        writeln!(ui.status(), "Nothing changed.")?;
        return Ok(());
    }

    let repo_path = workspace_command.repo_path();

    let mut workspaces_to_remove = Vec::new();
    for ws in &remove_ws {
        let ws_path = workspace_store.get_workspace_path(ws)?.ok_or_else(|| {
            user_error(format!(
                "Cannot remove unreachable workspace '{}'",
                ws.as_symbol()
            ))
            .hinted(USE_WORKSPACE_FORGET_HINT)
        })?;
        let ws_path = dunce::canonicalize(repo_path.join(ws_path)).map_err(|err| {
            user_error_with_message(
                format!("Cannot access workspace '{}' directory", ws.as_symbol()),
                err,
            )
            .hinted(USE_WORKSPACE_FORGET_HINT)
        })?;
        // The repository itself lives under the main workspace (in `.jj/repo`),
        // so removing that directory would destroy the repository. Don't
        // suggest `jj workspace forget` here: forgetting the main workspace
        // isn't a useful thing to do either.
        if repo_path.starts_with(&ws_path) {
            return Err(user_error(format!(
                "Cannot remove workspace '{}' because it contains the repository",
                ws.as_symbol()
            )));
        }
        // The recorded path may since have been replaced by a workspace of an
        // unrelated repository, which we must not remove.
        let ws_workspace = command.load_workspace_at(&ws_path, workspace_command.settings())?;
        if ws_workspace.repo_path() != repo_path {
            return Err(user_error(format!(
                "Cannot remove workspace '{}' because it belongs to another repository",
                ws.as_symbol()
            ))
            .hinted(USE_WORKSPACE_FORGET_HINT));
        }
        workspaces_to_remove.push((ws_path, ws_workspace));
    }

    // Snapshot each workspace before its directory goes away, so that tracked
    // working-copy changes that were never committed survive as a commit.
    // Untracked and ignored files are not part of the snapshot and are lost
    // along with the directory.
    let mut op_id = workspace_command.repo().op_id().clone();
    let mut paths_to_remove = Vec::new();
    for (abs_path, ws_workspace) in workspaces_to_remove {
        let op = ws_workspace.repo_loader().load_operation(&op_id).await?;
        let ws_repo = ws_workspace.repo_loader().load_at(&op).await?;
        let mut ws_helper = command.for_workable_repo(ui, ws_workspace, ws_repo)?;
        ws_helper.maybe_snapshot(ui).await?;
        // Continue from the operation the snapshot created, so that the next
        // snapshot and the removal itself build on it.
        op_id = ws_helper.repo().op_id().clone();
        paths_to_remove.push(abs_path);
    }

    let workspace = command.load_workspace()?;
    let op = workspace.repo_loader().load_operation(&op_id).await?;
    let repo = workspace.repo_loader().load_at(&op).await?;
    workspace_command = command.for_workable_repo(ui, workspace, repo)?;

    let mut tx = workspace_command.start_transaction();
    for ws in &remove_ws {
        tx.repo_mut().remove_workspace(ws).await?;
    }
    let names = remove_ws.iter().map(|ws| ws.as_symbol()).join(", ");
    let description = if remove_ws.len() == 1 {
        format!("remove workspace {names}")
    } else {
        format!("remove workspaces {names}")
    };
    tx.finish(ui, description).await?;

    workspace_store.forget(&remove_ws.iter().map(|ws| ws.as_ref()).collect_vec())?;

    #[cfg(feature = "git")]
    {
        let subprocess_options = GitSubprocessOptions::from_settings(workspace_command.settings())?;
        let store = workspace_command.repo().store();
        for path in &paths_to_remove {
            unlink_git_worktree(ui, store, subprocess_options.clone(), path)?;
        }
    }

    for path in &paths_to_remove {
        if let Err(err) = std::fs::remove_dir_all(path) {
            writeln!(
                ui.warning_default(),
                r#"Failed to remove workspace directory "{}"."#,
                path.display()
            )?;
            print_error_sources(ui, Some(&err))?;
        } else {
            writeln!(
                ui.status(),
                r#"Removed workspace directory "{}"."#,
                path.display()
            )?;
        }
    }

    Ok(())
}
