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
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt as _;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "git")]
use std::process::Command;

use clap_complete::ArgValueCandidates;
use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigSource;
use jj_lib::file_util::FileIdentity;
use jj_lib::file_util::IoResultExt as _;
#[cfg(feature = "git")]
use jj_lib::lock::FileLock;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::workspace_store::SimpleWorkspaceStore;
use jj_lib::workspace_store::WorkspaceStore as _;
#[cfg(unix)]
use nix::dir::Dir;
#[cfg(unix)]
use nix::fcntl::AtFlags;
#[cfg(unix)]
use nix::fcntl::OFlag;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use nix::fcntl::RenameFlags;
#[cfg(all(unix, not(all(target_os = "linux", target_env = "gnu"))))]
use nix::fcntl::renameat;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use nix::fcntl::renameat2;
#[cfg(unix)]
use nix::sys::stat::Mode;
#[cfg(unix)]
use nix::sys::stat::SFlag;
#[cfg(unix)]
use nix::sys::stat::fstatat;
#[cfg(unix)]
use nix::unistd::UnlinkatFlags;
#[cfg(unix)]
use nix::unistd::unlinkat;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::WorkspaceCommandHelper;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::command_error::user_error_with_message;
use crate::commands::btrfs::btrfs_user_subvol_rm_allowed;
use crate::commands::btrfs::delete_btrfs_subvolume;
use crate::commands::btrfs::is_btrfs_subvolume;
use crate::complete;
use crate::ui::Ui;

/// Remove a workspace and its directory from disk
///
/// The current workspace cannot be removed. Use `jj workspace forget` if you
/// only want to stop tracking a workspace without deleting its files.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspaceRemoveArgs {
    /// Name of the workspace to remove
    #[arg(value_name = "WORKSPACE", add = ArgValueCandidates::new(complete::workspaces))]
    workspace: WorkspaceNameBuf,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_remove(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspaceRemoveArgs,
) -> Result<(), CommandError> {
    // Reject an unsafe target before snapshotting the invoking workspace. A
    // failed attempt to remove the primary workspace must not create an
    // operation while proving that its shared repo is still needed.
    let lifecycle_store = {
        let workspace_command = command.workspace_helper_no_snapshot(ui).await?;
        let workspace_store = SimpleWorkspaceStore::load(workspace_command.repo_path())?;
        preflight_workspace_removal(
            command,
            &workspace_command,
            &workspace_store,
            args.workspace.as_ref(),
        )?;
        workspace_store
    };

    // Preserve normal command behavior for a safe removal: snapshot the
    // invoking workspace while holding the repository-wide lifecycle lock,
    // then repeat the preflight, delete the target, forget it, and publish the
    // repo transaction before releasing that lock. This keeps a concurrent
    // add/rename/forget from changing the registered survivor topology between
    // the safety proof and the destructive effect.
    let lifecycle_lock = lifecycle_store.lock_lifecycle()?;
    let mut workspace_command = command.workspace_helper(ui).await?;
    let target = preflight_workspace_removal(
        command,
        &workspace_command,
        &lifecycle_store,
        args.workspace.as_ref(),
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
        &lifecycle_store,
        args.workspace.as_ref(),
        &target,
    )?;
    let verified_btrfs_subvolume = is_btrfs_subvolume(&target.path).unwrap_or(false);
    #[cfg(feature = "git")]
    let git_cleanup = linked_git_worktree_cleanup(&workspace_command, &target);

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
                "The verified workspace remains at {} because restoring its original name \
                 failed: {restore_err:?}",
                claimed.path.display()
            ));
        }
        return Err(err);
    }

    #[cfg(feature = "git")]
    let git_cleanup_error = git_cleanup
        .as_ref()
        .and_then(|cleanup| cleanup_linked_git_worktree(cleanup).err());
    lifecycle_store.forget(&[args.workspace.as_ref()])?;
    tx.finish(
        ui,
        format!("remove workspace {}", args.workspace.as_symbol()),
    )
    .await?;
    drop(lifecycle_lock);
    #[cfg(feature = "git")]
    if let Some(err) = git_cleanup_error {
        writeln!(
            ui.warning_default(),
            "Warning: Removed the workspace, but failed to prune its linked Git worktree +             metadata: {err:?}"
        )?;
    }
    writeln!(
        ui.status(),
        "Removed workspace at \"{}\"",
        target.path.display()
    )?;
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

/// Snapshots the target in memory with filesystem monitoring disabled, then
/// discards the lock instead of publishing a working-copy operation. Removal
/// must not silently destroy files that are absent from the recorded commit.
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

#[cfg(unix)]
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
            "The Btrfs filesystem is mounted with `user_subvol_rm_allowed`; check the \
             subvolume ownership and parent-directory permissions."
        }
        Ok(None) | Err(_) => {
            "Check whether the Btrfs filesystem is mounted with `user_subvol_rm_allowed`."
        }
    };
    let display_path = path.to_string_lossy();
    let quoted_path = shlex::try_quote(&display_path).unwrap_or_else(|_| display_path.clone());
    err.add_hint(format!(
        "{mount_hint} The workspace was not forgotten. Run \
         `sudo btrfs subvolume delete {quoted_path}` only after deciding whether to forget \
         its registration."
    ));
}

#[cfg(feature = "git")]
#[derive(Clone, Debug)]
struct GitWorktreeCleanup {
    git_repo_path: PathBuf,
    git_executable: PathBuf,
    lock_root: PathBuf,
}

#[cfg(feature = "git")]
fn linked_git_worktree_cleanup(
    workspace_command: &WorkspaceCommandHelper,
    target: &RemovalTarget,
) -> Option<GitWorktreeCleanup> {
    if !target.path.join(".git").is_file() {
        return None;
    }
    let git_backend = jj_lib::git::get_git_backend(workspace_command.repo().store()).ok()?;
    Some(GitWorktreeCleanup {
        git_repo_path: git_backend.git_repo_path().to_owned(),
        git_executable: git_backend.git_executable_path().to_owned(),
        lock_root: workspace_command.repo_path().to_owned(),
    })
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
    Ok(())
}

fn preflight_workspace_removal(
    command: &CommandHelper,
    workspace_command: &WorkspaceCommandHelper,
    workspace_store: &SimpleWorkspaceStore,
    workspace_name: &WorkspaceName,
) -> Result<RemovalTarget, CommandError> {
    if workspace_name.as_str() == workspace_command.workspace_name().as_str() {
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
            command,
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
    command: &CommandHelper,
    workspace_command: &WorkspaceCommandHelper,
    workspace_store: &SimpleWorkspaceStore,
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
    resolve_registered_workspace(command, workspace_command, workspace_store, workspace_name)
        .map(Some)
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
    workspace_store: &SimpleWorkspaceStore,
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
    workspace_store: &SimpleWorkspaceStore,
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
