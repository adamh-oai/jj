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
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use btrfs_awacs::bootstrap::InitProgress;
use btrfs_awacs::bootstrap::finalize_root_path;
use btrfs_awacs::bootstrap::initialize_root;
use btrfs_awacs::bootstrap::trash_discarded_git_awacs_state;
use btrfs_awacs::scan::SnapshotBaseline;
use btrfs_awacs::scan::SnapshotIdentity;
use btrfs_awacs::scan_facade::DirectScanClient;
use btrfs_awacs::status::BaselineStatus;
use btrfs_awacs::status::GitFsmonitorStatus;
use btrfs_awacs::status::IndexChecksum;
use btrfs_awacs::status::RootStatus;
use btrfs_awacs::status::inspect_git_index;
use btrfs_awacs::subvolume_migration::MigrationOptions;
use btrfs_awacs::subvolume_migration::convert_subvolume_root;
use btrfs_awacs::subvolume_migration::copy_children;
use btrfs_awacs::subvolume_migration::copy_children_except;
use futures::TryStreamExt as _;
use futures::future::Either;
use futures::future::select;
use jj_lib::file_util::IoResultExt as _;
use jj_lib::local_working_copy::LocalWorkingCopy;
use jj_lib::local_working_copy::WorkingCopyBaselineStatus;
use jj_lib::local_working_copy::reset_local_working_copy_fsmonitor;
use jj_lib::local_working_copy::seed_local_working_copy_tree;
use jj_lib::local_working_copy::snapshot_mode_has_committed_baseline;
use jj_lib::lock::FileLock;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::repo::Repo as _;
use jj_lib::working_copy::LockedWorkingCopy;
use jj_lib::working_copy::WorkingCopyFreshness;

use crate::cleanup_guard::CleanupGuard;
use crate::cli_util::CommandHelper;
use crate::cli_util::shell_quote;
use crate::command_error::CommandError;
use crate::command_error::internal_error;
use crate::command_error::user_error;
use crate::commands::btrfs::begin_subvolume_mode;
use crate::commands::btrfs::btrfs_subvolume_id;
use crate::commands::btrfs::delete_btrfs_subvolume;
use crate::commands::btrfs::is_btrfs_path;
use crate::commands::btrfs::is_btrfs_subvolume;
use crate::commands::btrfs::is_subvolume_mode_committed;
use crate::commands::btrfs::is_subvolume_mode_enabled;
use crate::commands::btrfs::recursive_btrfs_snapshot;
use crate::commands::btrfs::set_subvolume_mode;
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
    /// Convert the repository root and its .git directory back to plain
    /// directories.
    Disable,
    /// Show the repository subvolume layout and working-copy snapshot state.
    Status,
}

pub async fn cmd_util_subvolume(
    ui: &mut Ui,
    command: &CommandHelper,
    subcommand: &UtilSubvolumeCommand,
) -> Result<(), CommandError> {
    let result = cmd_util_subvolume_impl(ui, command, subcommand).await;
    if let Err(err) = &result {
        tracing::warn!(error_kind = ?err.kind, "subvolume operation failed");
    }
    result
}

async fn cmd_util_subvolume_impl(
    ui: &mut Ui,
    command: &CommandHelper,
    subcommand: &UtilSubvolumeCommand,
) -> Result<(), CommandError> {
    let recovery_root = command.workspace_loader()?.workspace_root().to_owned();
    // Serialize the entire operation before inspecting or relaxing the mode
    // marker. A rebuild temporarily has no committed baseline, which another
    // enable must not mistake for an interrupted migration. Keep the lock
    // outside the checkout: enable and disable can replace its directory.
    let operation_lock_path = subvolume_operation_lock_path(&recovery_root)?;
    let _operation_lock = if matches!(subcommand, UtilSubvolumeCommand::Status) {
        if let Some(owner) = active_kernel_lock_owner(&operation_lock_path) {
            writeln!(
                ui.stdout(),
                "Subvolume operation: in progress (pid={owner})"
            )?;
            writeln!(
                ui.stdout(),
                "Consistency: unavailable (subvolume operation in progress)"
            )?;
            return Ok(());
        }
        None
    } else {
        Some(
            FileLock::try_lock(operation_lock_path)
                .map_err(|err| user_error(format!("Failed to lock subvolume operation: {err}")))?
                .ok_or_else(|| {
                    user_error(
                        "Another subvolume operation is already running for this checkout. Wait \
                         for it to finish before retrying.",
                    )
                })?,
        )
    };
    // A previous enable may have committed the strict marker before its first
    // AWACS baseline was durably paired with the tree. Ordinary working-copy
    // loading rejects that state by design. Status inspects the broken state
    // directly, while enable temporarily reopens it for the generic loader.
    // The guard restores the strict marker if recovery or loading fails.
    let marker_was_enabling = fs::read(recovery_root.join(".jj/working_copy/subvolume_mode"))
        .is_ok_and(|marker| marker == b"enabling\n");
    let missing_committed_baseline = is_subvolume_mode_enabled(&recovery_root)
        && !snapshot_mode_has_committed_baseline(&recovery_root.join(".jj/working_copy"))
            .map_err(internal_error)?;
    if missing_committed_baseline && matches!(subcommand, UtilSubvolumeCommand::Status) {
        // Loading a strict working copy without its baseline fails deliberately.
        // Inspect its persisted paths directly instead of temporarily rewriting
        // the mode marker merely to report that broken state.
        write_subvolume_status(ui, None, &recovery_root, true)?;
        return Ok(());
    }
    let relax_load_invariant = missing_committed_baseline
        && is_subvolume_mode_committed(&recovery_root)
        && matches!(subcommand, UtilSubvolumeCommand::Enable { .. });
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
            // A failed initialization may leave the transition marker and no
            // baseline. Rebuild in place for both marker states so the
            // suggested repair works without replacing the user's checkout.
            if *rebuild_baseline && is_subvolume_mode_enabled(&workspace_root) {
                let colocated =
                    crate::git_util::is_colocated_git_workspace(workspace_command.workspace())?;
                if colocated && !require_colocated_workspace(&workspace_command)? {
                    require_main_subvolume_mode_for_linked_workspace(&workspace_command)?;
                }
                rebuild_snapshot_baseline(
                    ui,
                    command,
                    &mut workspace_command,
                    colocated,
                    marker_was_enabling,
                )
                .await?;
                recovery_armed.store(false, Ordering::Relaxed);
                drop(recovery_guard);
                return Ok(());
            }
            let main_workspace = require_colocated_workspace(&workspace_command)?;
            if !main_workspace {
                require_main_subvolume_mode_for_linked_workspace(&workspace_command)?;
            }
            if is_subvolume_mode_committed(&workspace_root)
                && !recovery_needed
                && fs::symlink_metadata(workspace_root.join(".jj"))
                    .context(workspace_root.join(".jj"))?
                    .is_dir()
                && is_btrfs_subvolume(&workspace_root.join(".jj"))?
            {
                // Activation may have succeeded before finalization was
                // interrupted. Repair paths and the Git cursor without
                // replacing the committed JJ baseline or manager history.
                finalize_subvolume_enable(ui, &workspace_root)?;
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
            if require_colocated_workspace(&workspace_command)? {
                require_no_linked_awacs_state(&workspace_root, "disable")?;
            }
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
            write_subvolume_status(
                ui,
                Some(&workspace_command),
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

fn subvolume_operation_lock_path(workspace_root: &Path) -> Result<PathBuf, CommandError> {
    let parent = workspace_root
        .parent()
        .ok_or_else(|| user_error("Repository root has no parent directory"))?;
    let name = workspace_root
        .file_name()
        .ok_or_else(|| user_error("Repository root has no final component"))?;
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(name);
    lock_name.push(".jj-subvolume.lock");
    Ok(parent.join(lock_name))
}

async fn rebuild_snapshot_baseline(
    ui: &Ui,
    command: &CommandHelper,
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
    colocated: bool,
    marker_was_enabling: bool,
) -> Result<(), CommandError> {
    workspace_command.check_working_copy_writable()?;
    writeln!(
        ui.status(),
        "awacs: rebuilding committed snapshot baseline..."
    )?;
    let workspace_root = workspace_command.workspace_root().to_owned();
    let repo = workspace_command.repo().clone();
    let workspace_name = workspace_command.workspace_name().to_owned();
    let operation_id = repo.op_id().clone();
    // Match ordinary snapshot lock ordering and keep the journal unchanged
    // while validating its operation and semantic tree.
    let git_import_export_lock = workspace_command.lock_git_import_export()?;
    let (mut locked_workspace, commit) = workspace_command
        .unchecked_start_working_copy_mutation()
        .await?;
    // The helper was loaded before waiting for these locks. Refuse to reset a
    // journal against an operation that another workspace has already changed.
    let published_operation = command.resolve_operation(ui, repo.loader(), &workspace_name)?;
    if published_operation.id() != repo.op_id() {
        return Err(user_error(
            "The repository changed while preparing the AWACS baseline rebuild. Retry `jj util \
             subvolume enable --rebuild-baseline`.",
        ));
    }
    let locked_wc = locked_workspace.locked_wc();
    let freshness = WorkingCopyFreshness::check_stale(locked_wc, &commit, &repo).await?;
    let same_tree =
        locked_wc.old_tree().tree_ids_and_labels() == commit.tree().tree_ids_and_labels();
    let old_operation = repo
        .loader()
        .load_operation(locked_wc.old_operation_id())
        .await?;
    // An interrupted workspace-add may have published only the empty
    // placeholder, before recording the source snapshot's tree. In that case
    // the journal and repo agree, but neither describes the inherited files.
    let is_initial_placeholder = marker_was_enabling
        && locked_wc.old_tree().tree_ids_and_labels()
            == repo.store().empty_merged_tree().tree_ids_and_labels()
        && old_operation.metadata().description
            == format!("add workspace '{}'", workspace_name.as_symbol());
    let seed_initial_tree = if is_initial_placeholder {
        is_interrupted_snapshot_workspace_initialization(
            &repo,
            &workspace_name,
            locked_wc,
            &commit,
            &old_operation,
        )
        .await?
    } else {
        false
    };
    if is_initial_placeholder && !seed_initial_tree {
        return Err(user_error(
            "Cannot rebuild AWACS baseline: interrupted workspace initialization did not record \
             the inherited source tree. The files remain on disk, but tracked files and conflicts \
             cannot be reconstructed safely. Recover or recreate this workspace from its source \
             before retrying.",
        ));
    }
    let recover_stale = !seed_initial_tree && freshness == WorkingCopyFreshness::WorkingCopyStale;
    if recover_stale {
        // Scan against the old semantic tree, which still identifies tracked
        // and conflicted paths, before reconciling the stale workspace.
        let old_view = old_operation.view().await?;
        let old_commit_id = old_view.get_wc_commit_id(&workspace_name).ok_or_else(|| {
            user_error(
                "Cannot rebuild AWACS baseline: the recorded operation has no workspace commit.",
            )
        })?;
        let old_commit = repo.store().get_commit_async(old_commit_id).await?;
        if old_commit.tree().tree_ids_and_labels() != locked_wc.old_tree().tree_ids_and_labels() {
            return Err(user_error(
                "Cannot rebuild AWACS baseline: the journal tree does not match the recorded \
                 operation. Preserve the working-copy files and resolve the journal state before \
                 retrying.",
            ));
        }
    } else if !seed_initial_tree && (!same_tree || freshness != WorkingCopyFreshness::Fresh) {
        return Err(user_error(
            "Cannot rebuild AWACS baseline: working copy operation or tree is ambiguous. Preserve \
             the working-copy files and resolve the journal state before retrying.",
        ));
    }

    check_awacs_root_state_for_rebuild(&workspace_root)?;
    if !reset_local_working_copy_fsmonitor(locked_workspace.locked_wc())? {
        return Err(user_error(
            "This command requires a snapshot-backed local-disk working copy",
        ));
    }
    if seed_initial_tree
        && !seed_local_working_copy_tree(locked_workspace.locked_wc(), &commit.tree()).await?
    {
        return Err(user_error(
            "This command requires a snapshot-backed local-disk working copy",
        ));
    }
    // Stale recovery needs the original journal operation; recording the
    // latest repo operation here would erase that provenance.
    let journal_operation_id = if recover_stale {
        old_operation.id().clone()
    } else {
        operation_id.clone()
    };
    locked_workspace.finish(journal_operation_id).await?;
    // The compact journal is intentionally baseline-less between the reset
    // above and the replacement immutable scan below. Temporarily use the
    // transition marker so helper reloads can enter that state, then restore
    // strict mode whether reseeding succeeds or fails.
    begin_subvolume_mode(&workspace_root)?;
    let baseline_result = if recover_stale {
        match command
            .snapshot_stale_working_copy_for_subvolume_rebuild(
                ui,
                &git_import_export_lock,
                &operation_id,
                old_operation.id(),
            )
            .await
        {
            Ok((recovered, stats)) => {
                *workspace_command = recovered;
                crate::cli_util::print_snapshot_stats(
                    ui,
                    &stats,
                    workspace_command.env().path_converter(),
                )
                .map_err(CommandError::from)
                .and_then(|()| verify_committed_snapshot_baseline(workspace_command))
            }
            Err(err) => Err(err),
        }
    } else {
        drop(git_import_export_lock);
        establish_initial_snapshot_baseline(ui, workspace_command, Some(&operation_id)).await
    };
    let marker_result = set_subvolume_mode(&workspace_root, true);
    baseline_result.map_err(|err| {
        if recover_stale {
            err.hinted(
                "The working copy was not reconciled. Keep its files and inspect any saved \
                 snapshot above; a rebuild retry may require manual operation reconciliation.",
            )
        } else {
            err
        }
    })?;
    marker_result?;
    // Rebind the manager path for both cases, but do not rewrite the Git index
    // in an unreconciled stale checkout.
    finalize_subvolume_root(ui, &workspace_root, colocated && !recover_stale)?;
    writeln!(ui.status(), "awacs: rebuilt committed snapshot baseline.")?;
    Ok(())
}

/// Recognize the narrow historical window after workspace-add published the
/// source commit but before it seeded the new working-copy journal. The
/// operation chain and exact commit must agree before we can safely use that
/// commit as the semantic tree for an initial scan.
async fn is_interrupted_snapshot_workspace_initialization(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    locked_wc: &dyn LockedWorkingCopy,
    commit: &jj_lib::commit::Commit,
    old_operation: &jj_lib::operation::Operation,
) -> Result<bool, CommandError> {
    if locked_wc.old_tree().tree_ids_and_labels()
        != repo.store().empty_merged_tree().tree_ids_and_labels()
    {
        return Ok(false);
    }
    let old_view = old_operation.view().await?;
    let Some(old_commit_id) = old_view.get_wc_commit_id(workspace_name) else {
        return Ok(false);
    };
    let old_commit = repo.store().get_commit_async(old_commit_id).await?;
    if old_commit.tree().tree_ids_and_labels() != locked_wc.old_tree().tree_ids_and_labels() {
        return Ok(false);
    }
    let expected_description = format!(
        "record snapshot baseline in workspace {}",
        workspace_name.as_symbol()
    );
    let mut ancestors = Box::pin(jj_lib::op_walk::walk_ancestors(std::slice::from_ref(
        repo.operation(),
    )));
    while let Some(operation) = ancestors.try_next().await? {
        if operation.id() == old_operation.id() {
            break;
        }
        if operation.parent_ids() == [old_operation.id().clone()]
            && operation.metadata().description == expected_description
        {
            let view = operation.view().await?;
            return Ok(view.get_wc_commit_id(workspace_name) == Some(commit.id()));
        }
    }
    Ok(false)
}

fn check_awacs_root_state_for_rebuild(root: &Path) -> Result<(), CommandError> {
    // Synthetic CLI tests provide their own baseline without a real Btrfs
    // root or external manager database.
    if std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_some() {
        return Ok(());
    }
    // Git's fsmonitor hook uses the same manager independently of the JJ
    // working-copy lock. Keep its state and outstanding leases intact; only
    // the JJ consumer's cursor is replaced by the explicit full scan.
    DirectScanClient::for_root(root).map_err(|err| {
        user_error(format!(
            "Cannot rebuild AWACS baseline: failed to open existing AWACS root state: {err}"
        ))
    })?;
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
    let cleanup_armed = Arc::new(AtomicBool::new(false));
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
    let result = init_subvolume_at_inner(
        ui,
        command,
        source_command,
        &destination,
        compress,
        &cleanup_armed,
    )
    .await;
    if let Err(err) = result {
        let destination_owned = cleanup_armed.swap(false, Ordering::Relaxed);
        drop(cleanup_guard);
        if destination_owned && destination.exists() {
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
                        "{err:?}; failed to remove partial migration checkout at {}: \
                         {cleanup_err:?}",
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
    cleanup_armed: &AtomicBool,
) -> Result<(), CommandError> {
    let source_root = source_command.workspace_root();
    let main_colocated_workspace = require_colocated_workspace(source_command)?;
    if main_colocated_workspace {
        // Recursive snapshots preserve contents and boundaries, but receive
        // new UUIDs. Copied linked-worktree cursors still name the originals.
        require_no_linked_awacs_state(source_root, "migrate")?;
    } else {
        require_main_subvolume_mode_for_linked_workspace(source_command)?;
    }
    for name in [".git", ".jj"] {
        let metadata_path = source_root.join(name);
        if fs::symlink_metadata(&metadata_path)
            .context(&metadata_path)?
            .file_type()
            .is_symlink()
        {
            return Err(user_error(format!(
                "Cannot migrate symbolic-link metadata at {}",
                metadata_path.display()
            )));
        }
    }
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

    // The root and its descendants are separate snapshots. Keep JJ's
    // normal working-copy lock across all of them so a concurrent checkout
    // cannot pair old files with a newer journal. start_mutation reloads that
    // journal after acquiring the lock, without snapshotting the source.
    let source_working_copy = source_command.working_copy();
    let source_lock = source_working_copy.start_mutation().await?;
    if source_lock.old_operation_id() != source_working_copy.operation_id()
        || source_lock.old_tree().tree_ids_and_labels()
            != source_working_copy.tree()?.tree_ids_and_labels()
    {
        return Err(user_error(
            "Source working copy changed while preparing subvolume migration. Try again.",
        ));
    }
    let allow_reflinks = compress.is_none();
    let source_is_subvolume = is_btrfs_subvolume(source_root)?;
    if allow_reflinks && source_is_subvolume {
        writeln!(
            ui.status(),
            "Recursively snapshotting repository subvolumes to {}...",
            destination.display()
        )?;
        recursive_btrfs_snapshot(source_root, &destination)
            .map_err(|err| user_error(format!("Failed to snapshot repository: {err}")))?;
        // A failed non-replacing snapshot may mean another creator now owns
        // this path. Only successful publication gives us cleanup ownership.
        cleanup_armed.store(true, Ordering::Relaxed);
    } else {
        writeln!(
            ui.status(),
            "Copying repository contents to {} before subvolume migration...",
            destination.display()
        )?;
        fs::create_dir(&destination).context(&destination)?;
        cleanup_armed.store(true, Ordering::Relaxed);
        copy_children(source_root, &destination, allow_reflinks)
            .map_err(|err| user_error(format!("Failed to copy repository: {err}")))?;
    }
    drop(source_lock);

    let committed = convert_subvolume_root(
        &destination,
        MigrationOptions {
            compression: compress,
            keep_temporary_on_drop: false,
        },
        |phase| {
            let _status_result = writeln!(ui.status(), "awacs: {phase}...");
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
    tracing::info!("initializing AWACS root");
    seed_initial_awacs_root(ui, &destination)?;
    begin_subvolume_mode(&destination)?;
    tracing::info!("resetting local working-copy state");
    reset_local_working_copy_state(&mut destination_command).await?;
    // Finishing the reset replaces the working-copy state directory.
    begin_subvolume_mode(&destination)?;
    tracing::info!("removing legacy working-copy tree state");
    remove_legacy_tree_state(&destination)?;
    // The helper above was loaded before the marker existed, so it still
    // owns the ordinary local-disk working-copy implementation. Reload after
    // the marker is durable so the initial transaction uses snapshot-backed
    // state and can publish the AWACS baseline.
    drop(destination_command);
    tracing::info!("reloading snapshot-backed workspace");
    let mut destination_command = command
        .workspace_helper_no_snapshot_at(ui, &destination)
        .await?;
    writeln!(ui.status(), "Seeding snapshot-backed working-copy tree...")?;
    tracing::info!("seeding snapshot-backed working-copy tree");
    seed_snapshot_working_copy_tree(&mut destination_command).await?;
    writeln!(ui.status(), "awacs: creating initial snapshot baseline...")?;
    establish_initial_snapshot_baseline(ui, &mut destination_command, None).await?;
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
        writeln!(ui.status(), "awacs: test root initialized...")?;
        return Ok(());
    }
    initialize_root(root, |progress| match progress {
        InitProgress::Phase(phase) => {
            let _status_result = writeln!(ui.status(), "awacs: {phase}...");
        }
    })
    .map_err(|err| user_error(format!("Failed to initialize AWACS root: {err}")))?;
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
                "Failed to activate initialized checkout: {err}; also failed to restore the \
                 original checkout: {restore_err}"
            )));
        }
        return Err(err.into());
    }
    reanchor_current_directory(source_root, &original_cwd)?;
    finalize_subvolume_enable(ui, source_root).map_err(|err| {
        err.hinted(format!(
            "The initialized checkout is active, but finalizing its AWACS state failed. The \
             original checkout is retained at {}",
            retained_source.display()
        ))
    })?;
    Ok(retained_source)
}

fn finalize_subvolume_enable(ui: &Ui, root: &Path) -> Result<(), CommandError> {
    finalize_subvolume_root(ui, root, true)
}

fn finalize_subvolume_root(ui: &Ui, root: &Path, colocated: bool) -> Result<(), CommandError> {
    // Synthetic CLI tests have neither real manager paths nor a Git AWACS hook.
    if env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_some() {
        return Ok(());
    }
    finalize_root_path(root)
        .map_err(|err| user_error(format!("Failed to finalize AWACS root paths: {err}")))?;
    if colocated {
        refresh_git_awacs_baseline(ui, root)?;
    }
    Ok(())
}

fn refresh_git_awacs_baseline(ui: &Ui, root: &Path) -> Result<(), CommandError> {
    use std::process::Command;

    if !crate::git_util::git_uses_awacs_fsmonitor(Path::new("git"), root)? {
        return Ok(());
    }
    let git_dir = subvolume_status_git_directory(None, root)
        .ok_or_else(|| user_error("Failed to locate Git index for AWACS baseline refresh"))?;
    let baseline_is_valid = || -> Result<bool, CommandError> {
        let index_path = git_dir.join("index");
        if !index_path.try_exists().context(&index_path)? {
            return Ok(false);
        }
        let index = inspect_git_index(&index_path)
            .map_err(|err| user_error(format!("Failed to inspect Git index: {err}")))?;
        let GitFsmonitorStatus::Baseline(baseline) = index.fsmonitor else {
            return Ok(false);
        };
        let status = RootStatus::open(root)
            .map_err(|err| user_error(format!("Failed to inspect AWACS root: {err}")))?;
        Ok(status.issues.is_empty() && status.inspect_baseline(&baseline).issues.is_empty())
    };
    if baseline_is_valid()? {
        return Ok(());
    }
    writeln!(
        ui.status(),
        "awacs: refreshing Git index snapshot baseline..."
    )?;
    for attempt in 0..2 {
        // A copied or rebuilt manager cannot authorize the index's old cursor.
        // Force Git's protocol-v1 correctness fallback for one full refresh:
        // AWACS only accepts v2, and Git persists a timestamp after v1 failure
        // (explicit v2 failure instead persists an unusable empty token). The
        // second refresh uses the unchanged configured protocol to bootstrap
        // the new AWACS lane. Never fabricate cursor claims or disable the
        // monitor, and do not accept an incompatible configured protocol.
        let mut refresh = Command::new("git");
        refresh.arg("-C").arg(root);
        if attempt == 0 {
            refresh.args(["-c", "core.fsmonitorHookVersion=1"]);
        }
        let refreshed = refresh
            .args(["update-index", "--refresh", "--force-write-index"])
            .output()
            .map_err(|err| user_error(format!("Failed to refresh Git AWACS baseline: {err}")))?;
        // Dirty or unmerged entries make --refresh exit 1 without changing
        // their staged contents. Explicit index writes still persist FSMN,
        // including for empty indexes and GIT_OPTIONAL_LOCKS=0 callers.
        if !matches!(refreshed.status.code(), Some(0 | 1)) {
            return Err(user_error(format!(
                "Git index refresh failed with {}",
                refreshed.status
            )));
        }
        if attempt == 0 {
            let index = inspect_git_index(&git_dir.join("index")).map_err(|err| {
                user_error(format!("Failed to inspect refreshed Git index: {err}"))
            })?;
            if !matches!(index.fsmonitor, GitFsmonitorStatus::TimestampPlaceholder) {
                return Err(user_error(
                    "Git did not persist the fsmonitor timestamp required to bootstrap its new \
                     AWACS baseline",
                ));
            }
            continue;
        }
        // A successful index write does not prove that its hook succeeded.
        // Require an authorized cursor instead of trusting the exit code.
        if baseline_is_valid()? {
            return Ok(());
        }
    }
    Err(user_error(
        "Git did not publish a valid AWACS fsmonitor baseline after two index refreshes; check \
         the configured Git fsmonitor hook and its protocol version",
    ))
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
        "If the snapshot-backed checkout looks good, delete the original checkout at \
         {retained_source}."
    )?;
    writeln!(
        ui.status(),
        "Enter the snapshot-backed checkout with: cd {}",
        shell_quote(&activated_checkout)
    )?;
    Ok(())
}

fn remove_migration_checkout(path: &Path) -> Result<(), CommandError> {
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
    let Ok(relative_cwd) = original_cwd.strip_prefix(workspace_root) else {
        // --repository can select a checkout outside the current directory.
        // Replacing that checkout does not invalidate the caller's cwd.
        return Ok(());
    };
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
    workspace_command: Option<&crate::cli_util::WorkspaceCommandHelper>,
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
    let git_dir = subvolume_status_git_directory(workspace_command, workspace_root);
    let dot_git = workspace_root.join(".git");
    if dot_git.is_file() {
        match git_dir.as_ref() {
            Some(git_dir) => writeln!(
                ui.stdout(),
                ".git: linked worktree (administrative directory {})",
                git_dir.display()
            )?,
            None => writeln!(ui.stdout(), ".git: invalid linked-worktree gitfile")?,
        }
    } else {
        write_path_status(ui, ".git", &dot_git)?;
    }
    write_path_status(ui, ".jj", &workspace_root.join(".jj"))?;

    if !mode_enabled {
        writeln!(
            ui.stdout(),
            "Working copy snapshot: none (subvolume mode disabled)"
        )?;
        return Ok(());
    }

    let mut issues = Vec::new();
    let mut jj_baseline = None;
    if missing_committed_baseline {
        writeln!(
            ui.stdout(),
            "Working copy snapshot: none (required committed AWACS baseline is missing)"
        )?;
        writeln!(ui.stdout(), "JJ journal: missing committed baseline")?;
        issues.push("JJ working copy has no committed baseline".to_owned());
        writeln!(
            ui.stdout(),
            "Recovery: run `jj util subvolume enable` to build and activate a replacement \
             checkout."
        )?;
    } else {
        let workspace_command = workspace_command.ok_or_else(|| {
            internal_error("subvolume status has no loaded working copy for a committed baseline")
        })?;
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
        } else if let Some(reason) = journal.fallback_reason.as_deref() {
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
        let pending = journal.pending_mutation.as_deref().unwrap_or("none");
        writeln!(
            ui.stdout(),
            "JJ journal: {}, generation={}, pending={pending}",
            journal.phase,
            journal.generation
        )?;
        if journal.phase != "clean-baseline" {
            issues.push(format!("JJ journal is in {} phase", journal.phase));
        }
        if let Some(baseline) = journal.baseline.as_ref() {
            if baseline.owner_id != baseline.retention_token {
                issues.push("JJ baseline owner does not match its retention owner".to_owned());
            }
            match working_copy_snapshot_baseline(baseline) {
                Ok(baseline) => jj_baseline = Some(baseline),
                Err(error) => issues.push(format!("JJ baseline is invalid: {error}")),
            }
        } else {
            issues.push("JJ working copy has no snapshot baseline".to_owned());
        }
    }

    let mut git_baseline = None;
    let index_path = git_dir.as_ref().map(|git_dir| git_dir.join("index"));
    if let Some(index_path) = index_path.as_ref() {
        match inspect_git_index(index_path) {
            Ok(index) => {
                let checksum = match index.checksum {
                    IndexChecksum::Valid => "valid",
                    IndexChecksum::Omitted => "omitted",
                };
                writeln!(
                    ui.stdout(),
                    "Git index: version={}, entries={}, checksum={checksum}",
                    index.version,
                    index.entries
                )?;
                match index.fsmonitor {
                    GitFsmonitorStatus::Baseline(baseline) => {
                        writeln!(ui.stdout(), "Git fsmonitor: awacs-git-v2")?;
                        git_baseline = Some(baseline);
                    }
                    GitFsmonitorStatus::Missing => {
                        writeln!(ui.stdout(), "Git fsmonitor: missing (no FSMN extension)")?;
                        issues.push("Git index has no AWACS fsmonitor baseline".to_owned());
                    }
                    GitFsmonitorStatus::TimestampPlaceholder => {
                        writeln!(
                            ui.stdout(),
                            "Git fsmonitor: invalid (timestamp placeholder)"
                        )?;
                        issues.push("Git fsmonitor token is a timestamp placeholder".to_owned());
                    }
                    GitFsmonitorStatus::Invalid(error) => {
                        writeln!(ui.stdout(), "Git fsmonitor: invalid ({error})")?;
                        issues.push(format!("Git fsmonitor baseline is invalid: {error}"));
                    }
                }
            }
            Err(error) => {
                writeln!(ui.stdout(), "Git index: invalid ({error})")?;
                issues.push(format!("Git index is invalid: {error}"));
            }
        }
    } else {
        writeln!(
            ui.stdout(),
            "Git index: unavailable (Git administrative directory missing)"
        )?;
        issues.push("Git administrative directory is unavailable".to_owned());
    }

    let root_status = match RootStatus::open(workspace_root) {
        Ok(status) => {
            writeln!(ui.stdout(), "AWACS manager: {}", status.database.display())?;
            writeln!(
                ui.stdout(),
                "AWACS watch: {}, fsmonitor={}, id={}",
                status.watch.state,
                status.watch.fsmonitor_state,
                format_bytes(&status.watch.id)
            )?;
            writeln!(
                ui.stdout(),
                "AWACS replay: floor={}, indexed={}, last-cut={}",
                format_sequence(status.watch.replay_floor_sequence),
                format_sequence(status.watch.indexed_sequence),
                format_sequence(status.watch.last_cut_sequence)
            )?;
            issues.extend(status.issues.iter().cloned());
            if let Some(baseline) = jj_baseline.as_ref() {
                write_consumer_baseline_status(
                    ui,
                    "JJ",
                    &status.inspect_baseline(baseline),
                    &mut issues,
                )?;
            }
            if let Some(baseline) = git_baseline.as_ref() {
                write_consumer_baseline_status(
                    ui,
                    "Git",
                    &status.inspect_baseline(baseline),
                    &mut issues,
                )?;
            }
            if status.pending_operations.is_empty() && status.pending_baselines == 0 {
                writeln!(ui.stdout(), "Pending AWACS operations: none")?;
            } else {
                let operations = status
                    .pending_operations
                    .iter()
                    .map(|operation| {
                        format!(
                            "{}:{}:{}",
                            operation.kind,
                            operation.state,
                            format_sequence(operation.sequence)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    ui.stdout(),
                    "Pending AWACS operations: {}, pending-baselines={}",
                    if operations.is_empty() {
                        "none"
                    } else {
                        &operations
                    },
                    status.pending_baselines
                )?;
                issues.push("AWACS has unfinished operations or baseline promotions".to_owned());
            }
            Some(status)
        }
        Err(error) => {
            writeln!(ui.stdout(), "AWACS manager: unavailable ({error})")?;
            None
        }
    };

    let locks =
        inspect_subvolume_status_locks(workspace_root, index_path.as_deref(), root_status.as_ref());
    if locks.is_empty() {
        writeln!(ui.stdout(), "Locks: none")?;
    } else {
        writeln!(ui.stdout(), "Locks: {}", locks.join(", "))?;
        issues.push("working-copy or index locks are present".to_owned());
    }
    if issues.is_empty() && root_status.is_some() {
        writeln!(ui.stdout(), "Consistency: OK")?;
    } else if issues.is_empty() {
        writeln!(
            ui.stdout(),
            "Consistency: unavailable (AWACS manager unavailable)"
        )?;
    } else {
        writeln!(
            ui.stdout(),
            "Consistency: ERROR ({} issue(s))",
            issues.len()
        )?;
        for issue in issues {
            writeln!(ui.stdout(), "  - {issue}")?;
        }
    }
    Ok(())
}

fn subvolume_status_git_directory(
    workspace_command: Option<&crate::cli_util::WorkspaceCommandHelper>,
    workspace_root: &Path,
) -> Option<PathBuf> {
    if let Some(workspace_command) = workspace_command {
        return crate::git_util::workspace_git_dir(workspace_command.workspace());
    }
    let dot_git = workspace_root.join(".git");
    if dot_git.is_dir() {
        return fs::canonicalize(dot_git).ok();
    }
    let contents = fs::read_to_string(dot_git).ok()?;
    let git_dir = Path::new(contents.strip_prefix("gitdir:")?.trim());
    let git_dir = if git_dir.is_absolute() {
        git_dir.to_owned()
    } else {
        workspace_root.join(git_dir)
    };
    fs::canonicalize(git_dir).ok()
}

fn working_copy_snapshot_baseline(
    baseline: &WorkingCopyBaselineStatus,
) -> Result<SnapshotBaseline, String> {
    let filesystem_uuid = baseline
        .filesystem_uuid
        .as_slice()
        .try_into()
        .map_err(|_| "filesystem UUID has invalid length".to_owned())?;
    let subvolume_uuid = baseline
        .subvolume_uuid
        .as_slice()
        .try_into()
        .map_err(|_| "snapshot UUID has invalid length".to_owned())?;
    Ok(SnapshotBaseline {
        identity: SnapshotIdentity {
            filesystem_uuid,
            subvolume_uuid,
            read_only: true,
        },
        continuity_token: baseline.continuity_token.clone(),
        retention_token: baseline.retention_token.clone(),
    })
}

fn write_consumer_baseline_status(
    ui: &mut Ui,
    label: &str,
    baseline: &BaselineStatus,
    issues: &mut Vec<String>,
) -> Result<(), CommandError> {
    let owner = baseline
        .owner_id
        .as_ref()
        .map(|owner| format_bytes(owner))
        .unwrap_or_else(|| "invalid".to_owned());
    writeln!(
        ui.stdout(),
        "{label} baseline: snapshot={}, owner={owner}, sequence={}, committed-pin={}, \
         pending-pin={}",
        format_bytes(&baseline.snapshot_uuid),
        format_sequence(baseline.sequence),
        if baseline.committed_pin {
            "present"
        } else {
            "missing"
        },
        if baseline.pending_pin {
            "present"
        } else {
            "none"
        }
    )?;
    issues.extend(
        baseline
            .issues
            .iter()
            .map(|issue| format!("{label} baseline: {issue}")),
    );
    Ok(())
}

fn format_sequence(sequence: Option<u64>) -> String {
    sequence
        .map(|sequence| sequence.to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn inspect_subvolume_status_locks(
    workspace_root: &Path,
    index_path: Option<&Path>,
    root_status: Option<&RootStatus>,
) -> Vec<String> {
    let mut locks = Vec::new();
    let working_copy_lock = workspace_root.join(".jj/working_copy/working_copy.lock");
    if working_copy_lock.exists() {
        let owner = fs::read_to_string(&working_copy_lock)
            .ok()
            .map(|owner| owner.trim().to_owned())
            .filter(|owner| !owner.is_empty())
            .unwrap_or_else(|| "owner unknown".to_owned());
        locks.push(format!("JJ working copy ({owner})"));
    }
    if let Some(index_path) = index_path {
        let mut path = index_path.as_os_str().to_owned();
        path.push(".lock");
        if Path::new(&path).exists() {
            locks.push("Git index (owner unknown)".to_owned());
        }
    }
    if let Some(root_status) = root_status
        && let Some(parent) = root_status.database.parent()
        && let Some(owner) = active_kernel_lock_owner(&parent.join("root.lock"))
    {
        locks.push(format!("AWACS root (pid={owner})"));
    }
    locks
}

fn active_kernel_lock_owner(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let device = metadata.dev();
    let major = libc::major(device);
    let minor = libc::minor(device);
    let target = format!("{major:x}:{minor:x}:{}", metadata.ino());
    let locks = fs::read_to_string("/proc/locks").ok()?;
    locks.lines().find_map(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let identifier = fields.get(5)?;
        let mut parts = identifier.split(':');
        let major = u32::from_str_radix(parts.next()?, 16).ok()?;
        let minor = u32::from_str_radix(parts.next()?, 16).ok()?;
        let inode = parts.next()?;
        (format!("{major:x}:{minor:x}:{inode}") == target).then_some(fields.get(4)?.to_string())
    })
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
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut formatted = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        formatted.push(char::from(HEX[usize::from(byte >> 4)]));
        formatted.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    formatted
}

fn require_colocated_workspace(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
) -> Result<bool, CommandError> {
    let workspace_root = workspace_command.workspace_root();
    if !crate::git_util::is_colocated_git_workspace(workspace_command.workspace())? {
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

fn require_no_linked_awacs_state(workspace_root: &Path, action: &str) -> Result<(), CommandError> {
    let git_dir = workspace_root.join(".git");
    if !fs::symlink_metadata(&git_dir).context(&git_dir)?.is_dir() {
        return Err(user_error(
            "Cannot safely inspect linked worktrees through a symbolic-link .git directory",
        ));
    }
    let worktrees = git_dir.join("worktrees");
    match fs::symlink_metadata(&worktrees) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context(&worktrees).map_err(Into::into),
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(user_error(
                "Cannot safely inspect linked Git worktree metadata before changing subvolumes",
            )
            .hinted("Inspect the non-directory .git/worktrees entry before retrying."));
        }
    }
    for entry in fs::read_dir(&worktrees).context(&worktrees)? {
        let entry = entry.context(&worktrees)?;
        let file_type = entry.file_type().context(entry.path())?;
        if file_type.is_symlink() {
            return Err(user_error(format!(
                "Cannot safely inspect linked Git worktree metadata at {}",
                entry.path().display()
            ))
            .hinted("Inspect the symbolic-link worktree entry before retrying."));
        }
        if !file_type.is_dir() {
            continue;
        }
        let awacs = entry.path().join("awacs");
        match fs::symlink_metadata(&awacs) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).context(&awacs).map_err(Into::into),
            Ok(_) => {
                return Err(user_error(format!(
                    "Cannot {action} the main workspace while linked Git worktree AWACS state \
                     remains at {}",
                    awacs.display()
                ))
                .hinted(
                    "Remove the linked workspaces with `jj workspace remove <name>` before \
                     changing the main workspace's subvolume layout. If they were already \
                     removed, inspect their stale Git administrative state first; this command \
                     will not discard it.",
                ));
            }
        }
    }
    Ok(())
}

fn disable_subvolumes(workspace_root: &Path) -> Result<(), CommandError> {
    let dot_git = workspace_root.join(".git");
    if is_btrfs_subvolume(&dot_git)? {
        // AWACS state is keyed to the subvolume identity being discarded. It
        // can also contain broker-private trash directories that the caller
        // cannot read, so do not copy stale operational state into the plain
        // Git directory.
        convert_subvolume_to_directory(&dot_git, Some(OsStr::new("awacs")))?;
    }
    let dot_jj = workspace_root.join(".jj");
    if is_btrfs_subvolume(&dot_jj)? {
        convert_subvolume_to_directory(&dot_jj, None)?;
    }
    if is_btrfs_subvolume(workspace_root)? {
        convert_subvolume_to_directory(workspace_root, None)?;
    }
    Ok(())
}

fn convert_subvolume_to_directory(
    path: &Path,
    excluded_name: Option<&OsStr>,
) -> Result<(), CommandError> {
    let temporary = unique_sibling(path, "disable")?;
    fs::rename(path, &temporary).context(path)?;
    fs::create_dir(path).context(path)?;
    let copy_result = match excluded_name {
        Some(excluded_name) => copy_children_except(&temporary, path, excluded_name, true),
        None => copy_children(&temporary, path, true),
    };
    if let Err(err) =
        copy_result.map_err(|err| user_error(format!("Failed to copy subvolume contents: {err}")))
    {
        rollback_created_directory(path, &temporary)?;
        return Err(err);
    }
    let discarded_awacs = temporary.join("awacs");
    if excluded_name == Some(OsStr::new("awacs")) && discarded_awacs.is_dir() {
        // The replacement Git directory is complete and no longer refers to
        // these snapshots. Retire them through the broker before removing the
        // old metadata subvolume; unprivileged deletion cannot destroy a
        // read-only snapshot, and must not relax its protection instead.
        trash_discarded_git_awacs_state(&discarded_awacs).map_err(|err| {
            user_error(format!(
                "Failed to retire discarded AWACS state at {}: {err}; old metadata retained at {}",
                discarded_awacs.display(),
                temporary.display()
            ))
        })?;
        remove_discarded_git_subvolume(&temporary)?;
    } else if !delete_btrfs_subvolume(&temporary)? {
        return Err(user_error(format!(
            "Failed to remove Btrfs subvolume at {}",
            temporary.display()
        )));
    }
    Ok(())
}

fn remove_discarded_git_subvolume(path: &Path) -> Result<(), CommandError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match btrfs_awacs::btrfs::remove_subvolume_tree(path) {
            Ok(()) => return Ok(()),
            Err(err)
                if err.raw_os_error() == Some(libc::ENOTEMPTY) && Instant::now() < deadline =>
            {
                // Snapshot trash is drained asynchronously. The kernel is
                // authoritative: never report success while a hidden nested
                // subvolume still prevents deleting its containing root.
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                return Err(user_error(format!(
                    "Failed to remove discarded Git metadata at {}: {err}; old metadata retained",
                    path.display()
                )));
            }
        }
    }
}

fn remove_existing_destination(path: &Path) -> Result<(), CommandError> {
    match btrfs_awacs::btrfs::remove_tree(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context(path).map_err(Into::into),
    }
}

fn rollback_created_directory(path: &Path, temporary: &Path) -> Result<(), CommandError> {
    btrfs_awacs::btrfs::remove_tree(path).context(path)?;
    fs::rename(temporary, path).context(path)?;
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
    expected_published_operation: Option<&OperationId>,
) -> Result<(), CommandError> {
    let phase = Arc::new(Mutex::new("starting the JJ working-copy transaction"));
    let report_phase = {
        let phase = phase.clone();
        move |new_phase| -> Result<(), CommandError> {
            if let Ok(mut current_phase) = phase.lock() {
                *current_phase = new_phase;
            }
            tracing::info!(phase = new_phase, "establishing subvolume baseline");
            writeln!(ui.status(), "JJ subvolume enable: {new_phase}...")?;
            Ok(())
        }
    };
    let mut snapshot = Box::pin(workspace_command.snapshot_for_subvolume_enable(
        ui,
        &report_phase,
        expected_published_operation,
    ));
    loop {
        let heartbeat = Box::pin(heartbeat_delay(Duration::from_secs(10)));
        match select(snapshot, heartbeat).await {
            Either::Left((result, _heartbeat)) => {
                result.inspect_err(|err| {
                    let current_phase = phase
                        .lock()
                        .map(|current_phase| *current_phase)
                        .unwrap_or("running the JJ working-copy transaction");
                    tracing::warn!(phase = current_phase, error_kind = ?err.kind, "subvolume baseline initialization failed");
                })?;
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
    tracing::info!(
        phase = "verifying committed baseline",
        "establishing subvolume baseline"
    );
    let verification = verify_committed_snapshot_baseline(workspace_command);
    verification.inspect_err(|err| {
        tracing::warn!(phase = "verifying committed baseline", error_kind = ?err.kind, "subvolume baseline initialization failed");
    })?;
    tracing::info!("verified committed subvolume baseline");
    Ok(())
}

fn verify_committed_snapshot_baseline(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
) -> Result<(), CommandError> {
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
