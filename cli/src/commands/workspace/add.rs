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

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::bootstrap::{InitProgress, initialize_descendant_root};
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::manager::SnapshotIdentity;
use futures::future::try_join_all;
use itertools::Itertools as _;
use jj_lib::backend::CommitId;
use jj_lib::commit::CommitIteratorExt as _;
use jj_lib::file_util;
use jj_lib::file_util::IoResultExt as _;
use jj_lib::git;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use jj_lib::local_working_copy::seed_local_working_copy_tree;
use jj_lib::lock::FileLock;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::workspace::Workspace;
use jj_lib::workspace_store::SimpleWorkspaceStore;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use crate::commands::btrfs::begin_subvolume_mode;
use crate::commands::btrfs::btrfs_command;
use crate::commands::btrfs::is_btrfs_subvolume;
use crate::commands::btrfs::is_subvolume_mode_enabled;
use crate::commands::btrfs::set_subvolume_mode;
use crate::description_util::add_trailers;
use crate::description_util::join_message_paragraphs;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use crate::progress::ProgressWriter;
use crate::ui::Ui;

struct SnapshotPreparation {
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    initialized_awacs_snapshot: Option<SnapshotIdentity>,
}

/// How to handle sparse patterns when creating a new workspace.
#[derive(clap::ValueEnum, Clone, Debug, Eq, PartialEq)]
enum SparseInheritance {
    /// Copy all sparse patterns from the current workspace.
    Copy,
    /// Include all files in the new workspace.
    Full,
    /// Clear all files from the workspace (it will be empty).
    Empty,
}

/// Add a workspace
///
/// By default, the new workspace inherits the sparse patterns of the current
/// workspace. You can override this with the `--sparse-patterns` option.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspaceAddArgs {
    /// Where to create the new workspace
    #[arg(value_hint = clap::ValueHint::DirPath)]
    destination: String,
    /// A name for the workspace
    ///
    /// To override the default, which is the basename of the destination
    /// directory.
    #[arg(long)]
    name: Option<WorkspaceNameBuf>,

    /// A list of parent revisions for the working-copy commit of the newly
    /// created workspace. You may specify nothing, or any number of parents.
    ///
    /// If no revisions are specified, the new workspace will be created, and
    /// its working-copy commit will exist on top of the parent(s) of the
    /// working-copy commit in the current workspace, i.e. they will share the
    /// same parent(s).
    ///
    /// If any revisions are specified, the new workspace will be created, and
    /// the new working-copy commit will be created with all these revisions as
    /// parents, i.e. the working-copy commit will exist as if you had run `jj
    /// new r1 r2 r3 ...`.
    #[arg(long = "revision", short, value_name = "REVSETS", alias = "revisions")]
    revisions: Vec<RevisionArg>,

    /// The change description to use
    #[arg(long = "message", short, value_name = "MESSAGE")]
    message_paragraphs: Vec<String>,

    /// How to handle sparse patterns when creating a new workspace.
    #[arg(long, value_enum, default_value_t = SparseInheritance::Copy)]
    sparse_patterns: SparseInheritance,
}

fn is_empty_dir(path: &Path) -> bool {
    if let Ok(mut entries) = path.read_dir() {
        entries.next().is_none()
    } else {
        false
    }
}

#[instrument(skip_all)]
pub async fn cmd_workspace_add(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspaceAddArgs,
) -> Result<(), CommandError> {
    // Serialize the complete lifecycle transition, including destination
    // creation/snapshotting, workspace-store registration performed by
    // Workspace::init_workspace_with_existing_repo(), and repo publication.
    let lifecycle_store = {
        let workspace_command = command.workspace_helper_no_snapshot(ui).await?;
        SimpleWorkspaceStore::load(workspace_command.repo_path())?
    };
    let lifecycle_lock = lifecycle_store.lock_lifecycle()?;
    let old_workspace_command = command.workspace_helper(ui).await?;
    let mut snapshot = is_subvolume_mode_enabled(old_workspace_command.workspace_root());
    let snapshot_required = snapshot;
    let destination_path = command.cwd().join(&args.destination);
    let workspace_name = if let Some(name) = &args.name {
        name.to_owned()
    } else {
        let file_name = destination_path.file_name().unwrap();
        file_name
            .to_str()
            .ok_or_else(|| user_error("Destination path is not valid UTF-8"))?
            .into()
    };
    if workspace_name.as_str().is_empty() {
        return Err(user_error("New workspace name cannot be empty"));
    }

    let repo = old_workspace_command.repo();
    if repo.view().get_wc_commit_id(&workspace_name).is_some() {
        return Err(user_error(format!(
            "Workspace named '{name}' already exists",
            name = workspace_name.as_symbol()
        )));
    }
    let mut snapshot_source_commit = None;
    #[cfg_attr(
        not(all(target_os = "linux", feature = "awacs")),
        allow(unused_variables)
    )]
    let mut snapshot_preparation = None;
    if snapshot {
        if destination_path.exists() {
            if snapshot_required {
                return Err(user_error(
                    "Destination path exists; Btrfs snapshot destination must not exist",
                ));
            }
            snapshot = false;
        }
        if snapshot {
            let source_commit_id = old_workspace_command
                .get_wc_commit_id()
                .cloned()
                .unwrap_or_else(|| repo.store().root_commit_id().clone());
            let source_commit = repo.store().get_commit_async(&source_commit_id).await?;
            match create_btrfs_snapshot(
                ui,
                old_workspace_command.workspace().workspace_root(),
                &destination_path,
            ) {
                Ok(Some(preparation)) => {
                    // The source tree describes files inherited by a physical
                    // snapshot. A plain workspace starts empty and must
                    // materialize its checkout instead.
                    snapshot_source_commit = Some(source_commit);
                    snapshot_preparation = Some(preparation);
                }
                Ok(None) if snapshot_required => {
                    return Err(user_error("Current checkout is not on a Btrfs filesystem"));
                }
                Ok(None) => snapshot = false,
                Err(error) if snapshot_required => return Err(error),
                Err(_) => snapshot = false,
            }
        }
    }
    if !snapshot {
        if destination_path.exists() {
            if !is_empty_dir(&destination_path) {
                return Err(user_error(
                    "Destination path exists and is not an empty directory",
                ));
            }
        } else {
            fs::create_dir(&destination_path).context(&destination_path)?;
        }
    }
    let git_worktree_plan =
        if crate::git_util::is_colocated_git_workspace(old_workspace_command.workspace()) {
            let git_backend = git::get_git_backend(repo.store()).map_err(|_| {
                internal_error_with_message(
                    "Colocated workspace does not use a Git-backed repository",
                    "missing Git backend",
                )
            })?;
            let git_repo_path = dunce::canonicalize(git_backend.git_repo_path())
                .unwrap_or_else(|_| git_backend.git_repo_path().to_owned());
            let checkout_commit_id = old_workspace_command
                .get_wc_commit_id()
                .cloned()
                .unwrap_or_else(|| repo.store().root_commit_id().clone());
            Some(GitWorktreePlan {
                git_repo_path,
                git_executable: git_backend.git_executable_path().to_owned(),
                checkout_commit_id,
            })
        } else {
            None
        };
    if let Some(plan) = &git_worktree_plan {
        if snapshot {
            create_git_worktree_with_existing_files(
                plan,
                &destination_path,
                old_workspace_command.repo_path(),
            )?;
        } else {
            create_git_worktree(plan, &destination_path, old_workspace_command.repo_path())?;
        }
    }

    let working_copy_factory = command.get_working_copy_factory()?;
    let repo_path = old_workspace_command.repo_path();
    let initialization_started = Instant::now();
    // If we add per-workspace configuration, we'll need to reload settings for
    // the new workspace.
    let (new_workspace, repo) = Workspace::init_workspace_with_existing_repo(
        &destination_path,
        repo_path,
        repo,
        working_copy_factory,
        workspace_name.clone(),
    )
    .await?;
    if snapshot {
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        begin_subvolume_mode(&destination_path)?;
        #[cfg(not(all(target_os = "linux", feature = "awacs")))]
        set_subvolume_mode(&destination_path, true)?;
        tracing::debug!(
            elapsed = ?initialization_started.elapsed(),
            "initialized snapshot workspace metadata"
        );
    }
    writeln!(
        ui.status(),
        "Created workspace in \"{}\"",
        file_util::relative_path(command.cwd(), &destination_path).display()
    )?;
    // Show a warning if the user passed a path without a separator, since they
    // may have intended the argument to only be the name for the workspace.
    if !args.destination.contains(std::path::is_separator) {
        writeln!(
            ui.warning_default(),
            r#"Workspace created inside current directory. If this was unintentional, delete the "{}" directory and run `jj workspace forget {name}` to remove it."#,
            args.destination,
            name = workspace_name.as_symbol()
        )?;
    }

    let repo = if let Some(source_commit) = &snapshot_source_commit {
        let baseline_started = Instant::now();
        let mut tx = repo.start_transaction();
        tx.repo_mut()
            .edit(workspace_name.clone(), source_commit)
            .await?;
        tx.repo_mut().rebase_descendants().await?;
        let repo = tx
            .commit(format!(
                "record snapshot baseline in workspace {name}",
                name = workspace_name.as_symbol()
            ))
            .await?;
        tracing::debug!(elapsed = ?baseline_started.elapsed(), "recorded snapshot tree in repository");
        repo
    } else {
        repo
    };

    let mut new_workspace_command = if snapshot {
        // Workspace::init_workspace_with_existing_repo() selected the ordinary
        // working-copy implementation before the marker above existed. Reload
        // after the marker is durable so the child gets snapshot-backed
        // journal state, then pair that journal with the independently cloned
        // AWACS baseline before any checkout mutation asks to advance it.
        drop(new_workspace);
        let mut workspace_command = command
            .workspace_helper_no_snapshot_at(ui, &destination_path)
            .await?;
        let source_commit = snapshot_source_commit
            .as_ref()
            .expect("snapshot workspaces always retain the source commit");
        seed_snapshot_workspace_tree(&mut workspace_command, source_commit).await?;
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        if let Some(snapshot_identity) = snapshot_preparation
            .as_ref()
            .and_then(|preparation| preparation.initialized_awacs_snapshot.as_ref())
        {
            workspace_command
                .seed_initialized_snapshot_workspace_awacs_baseline(ui, snapshot_identity, None)
                .await?;
        } else {
            workspace_command
                .seed_snapshot_workspace_awacs_baseline(ui)
                .await?;
            establish_snapshot_workspace_baseline(ui, &mut workspace_command).await?;
        }
        #[cfg(not(all(target_os = "linux", feature = "awacs")))]
        establish_snapshot_workspace_baseline(ui, &mut workspace_command).await?;
        set_subvolume_mode(&destination_path, true)?;
        tracing::debug!("recorded snapshot tree as workspace baseline");
        workspace_command
    } else {
        command.for_workable_repo(ui, new_workspace, repo)?
    };

    let sparsity = match args.sparse_patterns {
        SparseInheritance::Full => None,
        SparseInheritance::Empty => Some(vec![]),
        SparseInheritance::Copy => {
            let sparse_patterns = old_workspace_command
                .working_copy()
                .sparse_patterns()?
                .to_vec();
            Some(sparse_patterns)
        }
    };

    let sparsity_started = Instant::now();
    if let Some(sparse_patterns) = sparsity
        && new_workspace_command.working_copy().sparse_patterns()? != sparse_patterns.as_slice()
    {
        let (mut locked_ws, _wc_commit) =
            new_workspace_command.start_working_copy_mutation().await?;
        locked_ws
            .locked_wc()
            .set_sparse_patterns(sparse_patterns)
            .await
            .map_err(|err| internal_error_with_message("Failed to set sparse patterns", err))?;
        let operation_id = locked_ws.locked_wc().old_operation_id().clone();
        locked_ws.finish(operation_id).await?;
    }
    if snapshot {
        tracing::debug!(
            elapsed = ?sparsity_started.elapsed(),
            "initialized snapshot workspace sparse patterns"
        );
    }

    let mut tx = new_workspace_command.start_transaction();

    // If no parent revisions are specified, create a working-copy commit based
    // on the parent of the current working-copy commit.
    let parents = if args.revisions.is_empty() {
        // Check out parents of the current workspace's working-copy commit, or the
        // root if there is no working-copy commit in the current workspace.
        if let Some(old_wc_commit_id) = tx
            .base_repo()
            .view()
            .get_wc_commit_id(old_workspace_command.workspace_name())
        {
            tx.repo()
                .store()
                .get_commit_async(old_wc_commit_id)
                .await?
                .parents()
                .await?
        } else {
            vec![tx.repo().store().root_commit()]
        }
    } else {
        try_join_all(
            old_workspace_command
                .resolve_some_revsets(ui, &args.revisions)
                .await?
                .iter()
                .map(|id| tx.repo().store().get_commit_async(id)),
        )
        .await?
    };

    let tree = merge_commit_trees(tx.repo(), &parents).await?;
    let parent_ids = parents.iter().ids().cloned().collect_vec();
    let mut commit_builder = tx.repo_mut().new_commit(parent_ids, tree).detach();
    let mut description = join_message_paragraphs(&args.message_paragraphs);
    if !description.is_empty() {
        // The first trailer would become the first line of the description.
        // Also, a commit with no description is treated in a special way in jujutsu: it
        // can be discarded as soon as it's no longer the working copy. Adding a
        // trailer to an empty description would break that logic.
        commit_builder.set_description(description);
        description = add_trailers(ui, &tx, &commit_builder).await?;
    }
    commit_builder.set_description(&description);
    let new_wc_commit = commit_builder.write(tx.repo_mut()).await?;

    tx.edit(&new_wc_commit)?;
    let checkout_started = Instant::now();
    tx.finish(
        ui,
        format!(
            "create initial working-copy commit in workspace {name}",
            name = workspace_name.as_symbol()
        ),
    )
    .await?;
    if snapshot {
        tracing::debug!(
            elapsed = ?checkout_started.elapsed(),
            "checked out snapshot workspace"
        );
    }
    drop(lifecycle_lock);
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn initialize_snapshot_awacs_root(
    ui: &Ui,
    root: &Path,
    parent_root: &Path,
) -> Result<Option<SnapshotIdentity>, CommandError> {
    // Synthetic CLI tests model the scan response without a real Btrfs
    // snapshot lineage or manager database.
    if std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_some() {
        return Ok(None);
    }
    let mut progress_writer = ProgressWriter::new(ui, "AWACS worktree init");
    let mut final_counts = None;
    let initialized =
        initialize_descendant_root(root, parent_root, None, |progress| match progress {
            InitProgress::Phase(phase) => {
                let _status_result = writeln!(ui.status(), "AWACS worktree init: {phase}...");
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
        .map_err(|err| user_error(format!("Failed to initialize AWACS worktree: {err}")))?;
    drop(progress_writer);
    if let Some(counts) = final_counts {
        writeln!(
            ui.status(),
            "AWACS worktree init: indexed {} directories, {} objects, {} paths.",
            counts.directories,
            counts.objects,
            counts.references,
        )?;
    }
    Ok(Some(initialized.snapshot_identity))
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
async fn seed_snapshot_workspace_tree(
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
    source_commit: &jj_lib::commit::Commit,
) -> Result<(), CommandError> {
    let operation_id = workspace_command.repo().op_id().clone();
    let (mut locked_workspace, _commit) = workspace_command
        .unchecked_start_working_copy_mutation()
        .await?;
    if !seed_local_working_copy_tree(locked_workspace.locked_wc(), &source_commit.tree())
        .await
        .map_err(|err| internal_error_with_message("Failed to seed snapshot workspace tree", err))?
    {
        return Err(internal_error_with_message(
            "Failed to seed snapshot workspace tree",
            "new workspace did not reload as snapshot-backed",
        ));
    }
    locked_workspace.finish(operation_id).await?;
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
async fn establish_snapshot_workspace_baseline(
    ui: &Ui,
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
) -> Result<(), CommandError> {
    workspace_command
        .snapshot_for_subvolume_enable(ui, &|_| Ok(()))
        .await
}

/// Attempts a Btrfs snapshot, then checks whether `source` is a subvolume
/// only if the snapshot fails. Returns `Ok(false)` when it is not so auto
/// mode can fall back to a normal workspace.
fn create_btrfs_snapshot(
    ui: &Ui,
    source: &Path,
    destination: &Path,
) -> Result<Option<SnapshotPreparation>, CommandError> {
    let total_started = Instant::now();
    let operation_started = Instant::now();
    let output = btrfs_command()
        .args(["subvolume", "snapshot"])
        .arg(source)
        .arg(destination)
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                user_error("Failed to create Btrfs snapshot: `btrfs` command is not installed")
            } else {
                user_error(format!("Failed to create Btrfs snapshot: {err}"))
            }
        })?;
    if !output.status.success() {
        if !is_btrfs_subvolume(source)? {
            return Ok(None);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(user_error(format!(
            "Failed to create Btrfs snapshot: {stderr}"
        )));
    }
    tracing::debug!(
        elapsed = ?operation_started.elapsed(),
        "created Btrfs snapshot"
    );
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    let initialized_awacs_snapshot = initialize_snapshot_awacs_root(ui, destination, source)?;
    #[cfg(not(all(target_os = "linux", feature = "awacs")))]
    let _ = ui;

    let operation_started = Instant::now();
    remove_copied_metadata(&destination.join(".jj"))?;
    tracing::debug!(
        elapsed = ?operation_started.elapsed(),
        "removed copied .jj metadata"
    );
    let operation_started = Instant::now();
    remove_copied_metadata(&destination.join(".git"))?;
    tracing::debug!(
        elapsed = ?operation_started.elapsed(),
        "removed copied .git metadata"
    );

    tracing::debug!(
        elapsed = ?total_started.elapsed(),
        "prepared Btrfs snapshot for workspace initialization"
    );
    Ok(Some(SnapshotPreparation {
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        initialized_awacs_snapshot,
    }))
}

fn remove_copied_metadata(path: &Path) -> Result<(), CommandError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context(path).map_err(Into::into),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).context(path)?;
    } else {
        fs::remove_file(path).context(path)?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(super) struct GitWorktreePlan {
    pub(super) git_repo_path: PathBuf,
    pub(super) git_executable: PathBuf,
    pub(super) checkout_commit_id: CommitId,
}

pub(super) fn create_git_worktree(
    plan: &GitWorktreePlan,
    workspace_root: &Path,
    lock_root: &Path,
) -> Result<(), CommandError> {
    let lock_path = lock_root.join("git_import_export.lock");
    let _lock = FileLock::lock(lock_path.clone()).map_err(|err| {
        user_error(format!(
            "Failed to take lock for Git import/export at {}: {err}",
            lock_path.display()
        ))
    })?;

    let mut cmd = Command::new(&plan.git_executable);
    cmd.arg("--git-dir")
        .arg(&plan.git_repo_path)
        .args([
            "worktree",
            "add",
            "--force",
            "--no-checkout",
            "--detach",
            "--quiet",
        ])
        .arg(workspace_root)
        .arg(plan.checkout_commit_id.hex());
    let output = cmd.output().map_err(|err| {
        user_error(format!(
            "Failed to create Git worktree using {}: {err}",
            plan.git_executable.display()
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(user_error(format!(
            "Failed to create Git worktree: {stderr}"
        )));
    }
    Ok(())
}

/// Creates a fresh Git worktree identity without overwriting files already
/// materialized in `workspace_root` by a filesystem snapshot.
fn create_git_worktree_with_existing_files(
    plan: &GitWorktreePlan,
    workspace_root: &Path,
    lock_root: &Path,
) -> Result<(), CommandError> {
    let total_started = Instant::now();
    let parent = workspace_root
        .parent()
        .ok_or_else(|| user_error("Workspace root has no parent directory"))?;
    let temporary_worktree = tempfile::Builder::new()
        .prefix(".jj-workspace-add-git-")
        .tempdir_in(parent)
        .context(parent)?;

    let operation_started = Instant::now();
    create_git_worktree(plan, temporary_worktree.path(), lock_root)?;
    tracing::debug!(
        elapsed = ?operation_started.elapsed(),
        "created temporary Git worktree for snapshot"
    );

    let temporary_dot_git = temporary_worktree.path().join(".git");
    let dot_git_path = workspace_root.join(".git");
    let operation_started = Instant::now();
    // A Btrfs subvolume boundary makes rename() fail with EXDEV even though
    // the gitlink is only a small plain-text file. Copy its contents into the
    // snapshot instead; the temporary worktree cleanup removes the source.
    fs::copy(&temporary_dot_git, &dot_git_path).context(&dot_git_path)?;
    tracing::debug!(
        elapsed = ?operation_started.elapsed(),
        "installed snapshot Git worktree pointer"
    );

    // The Git worktree admin directory still points to the temporary path.
    // Repair it after moving the gitlink into the snapshot.
    let operation_started = Instant::now();
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
    tracing::debug!(
        elapsed = ?operation_started.elapsed(),
        "repaired snapshot Git worktree metadata"
    );
    tracing::debug!(
        elapsed = ?total_started.elapsed(),
        "fixed up colocated Git worktree for snapshot"
    );
    Ok(())
}
