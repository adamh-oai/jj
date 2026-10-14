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
use btrfs_awacs::bootstrap::InitProgress;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::bootstrap::initialize_descendant_root_with_consumer;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::bootstrap::initialize_descendant_root_with_watch_consumer;
#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
use btrfs_awacs::git_index_handoff::GitIndexHandoff;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::manager::SnapshotIdentity;
#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
use btrfs_awacs::scan::Invalidation;
use futures::future::try_join_all;
use itertools::Itertools as _;
use jj_lib::backend::CommitId;
use jj_lib::commit::CommitIteratorExt as _;
use jj_lib::file_util;
use jj_lib::file_util::IoResultExt as _;
#[cfg(feature = "git")]
use jj_lib::git;
#[cfg(feature = "git")]
use jj_lib::git::GitSubprocessOptions;
#[cfg(feature = "git")]
use jj_lib::git::create_worktree;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use jj_lib::local_working_copy::seed_local_working_copy_tree_with_sparse_patterns;
use jj_lib::lock::FileLock;
#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
use jj_lib::matchers::Matcher;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::simple_workspace_store::SimpleWorkspaceStore;
use jj_lib::workspace::Workspace;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use crate::commands::btrfs::begin_subvolume_mode;
use crate::commands::btrfs::delete_btrfs_subvolume;
use crate::commands::btrfs::is_btrfs_subvolume;
use crate::commands::btrfs::is_subvolume_mode_enabled;
use crate::commands::btrfs::recursive_btrfs_snapshot;
use crate::commands::btrfs::remove_directory_tree;
use crate::commands::btrfs::set_subvolume_mode;
#[cfg(feature = "git")]
use crate::commands::git::maybe_add_gitignore;
use crate::description_util::add_trailers;
use crate::description_util::join_message_paragraphs;
use crate::ui::Ui;

struct SnapshotPreparation {
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    initialized_awacs_snapshot: Option<SnapshotIdentity>,
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    awacs_workspace_owner: [u8; 16],
    #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
    initialized_awacs_git_consumer: bool,
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
struct SnapshotSource {
    path: PathBuf,
    untracked: jj_lib::local_working_copy::UntrackedScanState,
    sparse_patterns: Vec<jj_lib::repo_path::RepoPathBuf>,
    _snapshot: btrfs_awacs::btrfs::OpenedSubvolume,
    _working_copy_lock: Box<dyn jj_lib::working_copy::LockedWorkingCopy>,
}

#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
struct SnapshotGitIndex {
    handoff: GitIndexHandoff,
    git_executable: PathBuf,
    config: Vec<u8>,
    ignored_jj_metadata: bool,
}

/// How to handle sparse patterns when creating a new workspace.
#[derive(clap::ValueEnum, Clone, Debug, Eq, PartialEq)]
pub(crate) enum SparseInheritance {
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

    /// Create a corresponding Git worktree for this workspace
    ///
    /// By default, a Git worktree is created when the current workspace is
    /// colocated and the [git.colocate config] is `true`.
    ///
    /// [git.colocate config]:
    ///     https://docs.jj-vcs.dev/latest/config/#default-colocation
    #[cfg(feature = "git")]
    #[arg(long, conflicts_with = "no_colocate")]
    colocate: bool,

    /// Do not create a Git worktree for this workspace
    #[cfg(feature = "git")]
    #[arg(long, conflicts_with = "colocate")]
    no_colocate: bool,

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
    let result = cmd_workspace_add_impl(ui, command, args).await;
    if let Err(err) = &result {
        tracing::warn!(error_kind = ?err.kind, "workspace add failed");
    }
    result
}

async fn cmd_workspace_add_impl(
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
    #[cfg_attr(not(all(target_os = "linux", feature = "awacs")), allow(unused_mut))]
    let mut snapshot_source_sparse_patterns = if snapshot {
        Some(
            old_workspace_command
                .working_copy()
                .sparse_patterns()?
                .to_vec(),
        )
    } else {
        None
    };
    // Initializing a snapshot workspace must publish and update its new
    // working copy. Reject incompatible global modes before creating any
    // destination or registering the workspace.
    if snapshot && !command.should_commit_transaction() {
        return Err(user_error(
            "Cannot create a snapshot-backed workspace with --no-integrate-operation.",
        ));
    }
    if snapshot && !command.is_at_head_operation() {
        return Err(user_error(
            "Cannot create a snapshot-backed workspace at a historical operation.",
        ));
    }
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
    #[cfg(feature = "git")]
    let should_colocate = if args.colocate {
        true
    } else if args.no_colocate {
        false
    } else {
        old_workspace_command.working_copy_shared_with_git()
            && old_workspace_command.settings().get_bool("git.colocate")?
    };
    #[cfg(feature = "git")]
    let git_worktree_plan = if should_colocate {
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
    #[cfg(not(feature = "git"))]
    let git_worktree_plan: Option<GitWorktreePlan> = None;
    let working_copy_factory = command.get_working_copy_factory()?;
    let mut snapshot_source_commit = None;
    #[cfg_attr(
        not(all(target_os = "linux", feature = "awacs")),
        allow(unused_variables)
    )]
    let mut snapshot_preparation = None;
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    let mut snapshot_source = None;
    #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
    let mut snapshot_git_index = None;
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
            #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
            if let Some(plan) = &git_worktree_plan
                && std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_none()
                && crate::git_util::git_uses_awacs_fsmonitor(
                    &plan.git_executable,
                    old_workspace_command.workspace_root(),
                )?
            {
                // Capture Git independently of JJ's committed physical seed.
                // A persisted Git cursor can outlive its named snapshot; the
                // shared handoff leases a fresh cut and retains replay from it.
                // Take this lock before acquiring the working-copy journal lock.
                let _git_lock = FileLock::lock(
                    old_workspace_command
                        .repo_path()
                        .join("git_import_export.lock"),
                )
                .map_err(|err| user_error(format!("Failed to lock source Git index: {err}")))?;
                let source_git = gix::open(old_workspace_command.workspace_root())
                    .map_err(|err| user_error(format!("Failed to open source Git index: {err}")))?;
                if source_git.index_path().is_file()
                    && let Some(config) = snapshot_git_index_config(
                        &plan.git_executable,
                        old_workspace_command.workspace_root(),
                        &source_git,
                    )?
                {
                    let handoff = GitIndexHandoff::capture(
                        old_workspace_command.workspace_root(),
                        &source_git.index_path(),
                        source_git.object_hash().len_in_bytes(),
                    )
                    .map_err(|err| {
                        user_error(format!("Failed to capture source Git index: {err}"))
                    })?;
                    snapshot_git_index = Some(SnapshotGitIndex {
                        handoff,
                        git_executable: plan.git_executable.clone(),
                        config,
                        ignored_jj_metadata: git_ignores_jj_metadata(
                            &plan.git_executable,
                            old_workspace_command.workspace_root(),
                        )?,
                    });
                }
            }
            #[cfg(all(target_os = "linux", feature = "awacs"))]
            {
                snapshot_source =
                    prepare_snapshot_source(&old_workspace_command, &source_commit).await?;
                if let Some(source) = &snapshot_source {
                    snapshot_source_sparse_patterns = Some(source.sparse_patterns.clone());
                }
            }
            #[cfg(all(target_os = "linux", feature = "awacs"))]
            let physical_source = snapshot_source
                .as_ref()
                .map_or(old_workspace_command.workspace_root(), |source| {
                    source.path.as_path()
                });
            #[cfg(not(all(target_os = "linux", feature = "awacs")))]
            let physical_source = old_workspace_command.workspace_root();
            match create_btrfs_snapshot(physical_source, &destination_path) {
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
    let workspace_setup_result = (|| {
        if let Some(plan) = &git_worktree_plan {
            if snapshot {
                create_git_worktree_with_existing_files(
                    plan,
                    &destination_path,
                    old_workspace_command.repo_path(),
                )?;
            } else {
                #[cfg(feature = "git")]
                {
                    let subprocess_options =
                        GitSubprocessOptions::from_settings(old_workspace_command.settings())?;
                    create_worktree(repo.store(), subprocess_options, &destination_path)?;
                }
            }
            writeln!(ui.status(), "Created Git worktree for the new workspace.")?;
        }
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        if snapshot {
            // The recursive snapshot's copied metadata belongs to the source.
            // Resolve the child state root after replacing it with the real
            // linked-worktree gitfile, before initializing JJ or its checkout.
            let preparation = snapshot_preparation
                .as_mut()
                .expect("snapshot workspaces always retain their preparation");
            #[cfg(feature = "git")]
            let retain_git_consumer = if let Some(plan) = &git_worktree_plan {
                crate::git_util::git_uses_awacs_fsmonitor(&plan.git_executable, &destination_path)?
            } else {
                false
            };
            #[cfg(not(feature = "git"))]
            let retain_git_consumer = false;
            #[cfg(feature = "git")]
            {
                preparation.initialized_awacs_git_consumer = retain_git_consumer;
            }
            preparation.initialized_awacs_snapshot = initialize_snapshot_awacs_root(
                ui,
                &destination_path,
                snapshot_source
                    .as_ref()
                    .map_or(old_workspace_command.workspace_root(), |source| {
                        source.path.as_path()
                    }),
                retain_git_consumer,
                preparation.awacs_workspace_owner,
            )?;
        }
        Result::<(), CommandError>::Ok(())
    })();
    // The child now retains its own A. Release the source journal lock before
    // any workspace registration or repository transaction can update it.
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    let inherited_untracked = snapshot_source
        .as_ref()
        .map(|source| source.untracked.clone());
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    drop(snapshot_source);
    if let Err(mut error) = workspace_setup_result {
        if snapshot
            && let Err(cleanup_error) = cleanup_failed_snapshot_workspace(
                &destination_path,
                git_worktree_plan.as_ref(),
                old_workspace_command.repo_path(),
            )
        {
            error.add_hint(format!(
                "Failed to remove partial snapshot workspace: {cleanup_error:?}"
            ));
        }
        return Err(error);
    }

    let repo_path = old_workspace_command.repo_path();
    let initialization_started = Instant::now();
    // If we add per-workspace configuration, we'll need to reload settings for
    // the new workspace.
    let (new_workspace, repo) = match Workspace::init_workspace_with_existing_repo(
        &destination_path,
        repo_path,
        repo,
        working_copy_factory,
        workspace_name.clone(),
    )
    .await
    {
        Ok(initialized) => initialized,
        Err(error) => {
            let mut error = CommandError::from(error);
            if snapshot
                && let Err(cleanup_error) = cleanup_failed_snapshot_workspace(
                    &destination_path,
                    git_worktree_plan.as_ref(),
                    old_workspace_command.repo_path(),
                )
            {
                error.add_hint(format!(
                    "Failed to remove partial snapshot workspace: {cleanup_error:?}"
                ));
            }
            return Err(error);
        }
    };
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
        tracing::info!(
            phase = "recording snapshot tree in repository",
            "initializing snapshot workspace baseline"
        );
        #[cfg(feature = "git")]
        let _git_import_export_lock = git_worktree_plan
            .as_ref()
            .map(|_| {
                FileLock::lock(
                    old_workspace_command
                        .repo_path()
                        .join("git_import_export.lock"),
                )
                .map_err(|err| user_error(format!("Failed to lock snapshot Git state: {err}")))
            })
            .transpose()?;
        let mut tx = repo.start_transaction();
        #[cfg(feature = "git")]
        if git_worktree_plan.is_some() {
            // This HEAD was installed while creating the child worktree. Record
            // it without resetting the copied tree: the first normal snapshot
            // must not mistake initialization for an external Git checkout and
            // discard the child's newly seeded AWACS baseline.
            git::import_head(tx.repo_mut(), &workspace_name, &destination_path).await?;
        }
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
        tracing::info!(elapsed = ?baseline_started.elapsed(), "recorded snapshot tree in repository");
        repo
    } else {
        repo
    };

    #[cfg(not(all(target_os = "linux", feature = "awacs")))]
    if snapshot {
        return Err(user_error(
            "Btrfs snapshot workspaces require Linux with AWACS support",
        ));
    }

    let mut new_workspace_command = if snapshot {
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        {
            // Workspace::init_workspace_with_existing_repo() selected the ordinary
            // working-copy implementation before the marker above existed. Reload
            // after the marker is durable so the child gets snapshot-backed
            // journal state, then pair that journal with the independently cloned
            // AWACS baseline before any checkout mutation asks to advance it.
            drop(new_workspace);
            let started = Instant::now();
            let mut phase = "loading child workspace";
            tracing::info!(
                ignore_working_copy = command.global_args().ignore_working_copy,
                phase,
                "initializing snapshot workspace baseline"
            );
            let result = async {
                let mut workspace_command = command
                    .workspace_helper_no_snapshot_at(ui, &destination_path)
                    .await?;
                workspace_command.allow_new_snapshot_workspace_initialization();
                let source_commit = snapshot_source_commit
                    .as_ref()
                    .expect("snapshot workspaces always retain the source commit");
                phase = "seeding working-copy tree";
                tracing::info!(phase, "initializing snapshot workspace baseline");
                seed_snapshot_workspace_tree(
                    &mut workspace_command,
                    source_commit,
                    snapshot_source_sparse_patterns
                        .as_deref()
                        .expect("snapshot source has sparse patterns"),
                )
                .await?;
                if let Some(preparation) = snapshot_preparation.as_ref()
                    && let Some(snapshot_identity) = preparation.initialized_awacs_snapshot.as_ref()
                {
                    phase = "publishing initialized AWACS baseline";
                    tracing::info!(phase, "initializing snapshot workspace baseline");
                    workspace_command
                        .seed_initialized_snapshot_workspace_awacs_baseline(
                            ui,
                            snapshot_identity,
                            Some(preparation.awacs_workspace_owner),
                            inherited_untracked.as_ref(),
                        )
                        .await?;
                } else {
                    phase = "seeding AWACS baseline";
                    tracing::info!(phase, "initializing snapshot workspace baseline");
                    workspace_command
                        .seed_snapshot_workspace_awacs_baseline(ui)
                        .await?;
                    phase = "establishing snapshot workspace baseline";
                    tracing::info!(phase, "initializing snapshot workspace baseline");
                    establish_snapshot_workspace_baseline(ui, &mut workspace_command).await?;
                }
                phase = "committing subvolume mode marker";
                tracing::info!(phase, "initializing snapshot workspace baseline");
                set_subvolume_mode(&destination_path, true)?;
                Ok::<_, CommandError>(workspace_command)
            }
            .await;
            match result {
                Ok(workspace_command) => {
                    tracing::info!(elapsed = ?started.elapsed(), "initialized snapshot workspace baseline");
                    tracing::debug!("recorded snapshot tree as workspace baseline");
                    workspace_command
                }
                Err(err) => {
                    tracing::warn!(phase, error_kind = ?err.kind, elapsed = ?started.elapsed(), "snapshot workspace baseline initialization failed");
                    return Err(err);
                }
            }
        }
        #[cfg(not(all(target_os = "linux", feature = "awacs")))]
        unreachable!("snapshot mode was rejected above")
    } else {
        command.for_workable_repo(ui, new_workspace, repo)?
    };
    // Keep the colocated Git worktree from tracking Jujutsu's own files.
    #[cfg(feature = "git")]
    maybe_add_gitignore(&new_workspace_command)?;

    let sparsity = match args.sparse_patterns {
        SparseInheritance::Full if snapshot => Some(vec![jj_lib::repo_path::RepoPathBuf::root()]),
        SparseInheritance::Full => None,
        SparseInheritance::Empty => Some(vec![]),
        SparseInheritance::Copy => {
            let sparse_patterns = if let Some(patterns) = &snapshot_source_sparse_patterns {
                patterns.clone()
            } else {
                old_workspace_command
                    .working_copy()
                    .sparse_patterns()?
                    .to_vec()
            };
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
    #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
    if let Some(preparation) = snapshot_preparation.as_ref()
        && preparation.initialized_awacs_git_consumer
        && let Some(snapshot_identity) = preparation.initialized_awacs_snapshot.as_ref()
    {
        // Retain both consumers' A through checkout, then publish Git's first
        // cursor. Otherwise checkout's directory metadata changes make Git
        // invalidate every entry below those directories on its first query.
        seed_snapshot_git_index(
            &new_workspace_command,
            snapshot_source_commit
                .as_ref()
                .expect("snapshot workspaces always retain the source commit"),
            &new_wc_commit,
            snapshot_identity,
            snapshot_source_sparse_patterns
                .as_deref()
                .expect("snapshot source has sparse patterns"),
            snapshot_git_index.as_mut(),
        )?;
    }
    drop(lifecycle_lock);
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
async fn prepare_snapshot_source(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    source_commit: &jj_lib::commit::Commit,
) -> Result<Option<SnapshotSource>, CommandError> {
    use btrfs_awacs::scan::SnapshotBaseline;
    use btrfs_awacs::status::RootStatus;
    use jj_lib::local_working_copy::LockedLocalWorkingCopy;

    if std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_some() {
        return Ok(None);
    }
    let working_copy = workspace_command.working_copy();
    let source_tree = source_commit.tree();
    let locked = working_copy.start_mutation().await?;
    if locked.old_tree().tree_ids_and_labels() != source_tree.tree_ids_and_labels()
        || working_copy.tree()?.tree_ids_and_labels() != source_tree.tree_ids_and_labels()
        || locked.old_operation_id() != working_copy.operation_id()
    {
        return Err(user_error(
            "Source working copy changed before snapshot creation. Try again.",
        ));
    }
    let local = locked
        .downcast_ref::<LockedLocalWorkingCopy>()
        .ok_or_else(|| user_error("Snapshot creation requires a local working-copy journal"))?;
    let journal = local.journal_status()?;
    if journal.phase != "clean-baseline" || journal.baseline_backend.as_deref() != Some("awacs") {
        return Err(user_error(
            "Snapshot creation requires a committed AWACS source baseline",
        ));
    }
    if journal.has_pending_scan {
        return Err(user_error(
            "Snapshot creation requires source filesystem changes to be recorded first",
        )
        .hinted("Run `jj status` in the source workspace, then retry."));
    }
    let untracked = local.untracked_scan_state()?.ok_or_else(|| {
        user_error(
            "Snapshot creation requires a complete source AWACS baseline; run jj util subvolume \
             enable --rebuild-baseline in the source workspace",
        )
    })?;
    let source = journal
        .baseline
        .ok_or_else(|| user_error("Snapshot creation has no committed AWACS source baseline"))?;
    if source.owner_id != source.retention_token {
        return Err(user_error(
            "AWACS source baseline does not belong to the source working copy",
        ));
    }
    let baseline =
        SnapshotBaseline {
            identity: btrfs_awacs::scan::SnapshotIdentity {
                filesystem_uuid: source.filesystem_uuid.try_into().map_err(|_| {
                    user_error("AWACS source baseline has an invalid filesystem UUID")
                })?,
                subvolume_uuid: source.subvolume_uuid.try_into().map_err(|_| {
                    user_error("AWACS source baseline has an invalid snapshot UUID")
                })?,
                read_only: true,
            },
            continuity_token: source.continuity_token,
            retention_token: source.retention_token,
        };
    let status = RootStatus::open(workspace_command.workspace_root())
        .map_err(|err| user_error(format!("Failed to inspect AWACS source baseline: {err}")))?;
    let (path, snapshot) = status
        .open_retained_baseline_snapshot(&baseline)
        .map_err(|err| user_error(format!("Failed to retain AWACS source snapshot: {err}")))?;
    // Clone S itself rather than the live source. Raw filesystem writes do
    // not obey JJ's journal lock and can otherwise enter child A after the
    // source semantic tree was recorded, making an unchanged entry look clean.
    Ok(Some(SnapshotSource {
        path,
        untracked,
        sparse_patterns: locked.sparse_patterns()?.to_vec(),
        _snapshot: snapshot,
        _working_copy_lock: locked,
    }))
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn initialize_snapshot_awacs_root(
    ui: &Ui,
    root: &Path,
    parent_root: &Path,
    retain_git_consumer: bool,
    workspace_owner: [u8; 16],
) -> Result<Option<SnapshotIdentity>, CommandError> {
    // Synthetic CLI tests model the scan response without a real Btrfs
    // snapshot lineage or manager database.
    if std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_some() {
        return Ok(None);
    }
    let progress = |progress| match progress {
        InitProgress::Phase(phase) => {
            let _status_result = writeln!(ui.status(), "awacs: {phase}...");
        }
    };
    let initialized = if retain_git_consumer {
        initialize_descendant_root_with_watch_consumer(
            root,
            parent_root,
            None,
            Some(workspace_owner),
            progress,
        )
    } else {
        initialize_descendant_root_with_consumer(root, parent_root, None, workspace_owner, progress)
    }
    .map_err(|err| user_error(format!("Failed to initialize AWACS worktree: {err}")))?;
    Ok(Some(initialized.snapshot_identity))
}

#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
fn seed_snapshot_git_index(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    source_commit: &jj_lib::commit::Commit,
    final_commit: &jj_lib::commit::Commit,
    snapshot_identity: &SnapshotIdentity,
    source_sparse_patterns: &[jj_lib::repo_path::RepoPathBuf],
    inherited: Option<&mut SnapshotGitIndex>,
) -> Result<(), CommandError> {
    use btrfs_awacs::scan::BeginScanRequest;
    use btrfs_awacs::scan::ScanClient as _;
    use btrfs_awacs::scan::ScanOutcome;
    use btrfs_awacs::scan_facade::DirectScanClient;
    use btrfs_awacs::status::encode_git_fsmonitor_token;
    use jj_lib::matchers::PrefixMatcher;

    let source_tree = source_commit.tree();
    let Some(tree_id) = source_tree.tree_ids().as_resolved() else {
        // Materialized conflicts need a different proof than a resolved Git
        // tree. Retain the ordinary conservative first-status behavior.
        return Ok(());
    };
    let root = workspace_command.workspace_root();
    let git_repo = gix::open(root)
        .map_err(|err| user_error(format!("Failed to open snapshot Git worktree: {err}")))?;
    let lock_path = workspace_command.repo_path().join("git_import_export.lock");
    let _git_lock = FileLock::lock(lock_path.clone()).map_err(|err| {
        user_error(format!(
            "Failed to take lock for Git import/export at {}: {err}",
            lock_path.display()
        ))
    })?;
    if let Some(inherited) = inherited
        && source_tree.tree_ids() == final_commit.tree().tree_ids()
        && source_sparse_patterns == [jj_lib::repo_path::RepoPathBuf::root()]
        && workspace_command.working_copy().sparse_patterns()?
            == [jj_lib::repo_path::RepoPathBuf::root()]
        && reuse_snapshot_git_index(inherited, &git_repo, root, tree_id, snapshot_identity)?
    {
        return Ok(());
    }
    let index_lock = gix::lock::File::acquire_to_update_resource(
        git_repo.index_path(),
        gix::lock::acquire::Fail::Immediately,
        None,
    )
    .map_err(|err| user_error(format!("Failed to lock snapshot Git index: {err}")))?;
    // The semantic source tree describes the immutable physical source, while
    // the source Git index may have staged changes or an arbitrarily old token.
    // Keep the final checkout index and trust only entries that match that
    // source proof and were not invalidated between child A and this cut.
    let index_from_tree = |tree_id: &jj_lib::backend::TreeId| {
        if tree_id == workspace_command.repo().store().empty_tree_id() {
            Ok(gix::index::File::from_state(
                gix::index::State::new(git_repo.object_hash()),
                git_repo.index_path(),
            ))
        } else {
            git_repo
                .index_from_tree(&gix::ObjectId::from_bytes_or_panic(tree_id.as_bytes()))
                .map_err(|err| user_error(format!("Failed to seed snapshot Git index: {err}")))
        }
    };
    let source_index = index_from_tree(tree_id)?;
    let mut index = match git_repo.open_index() {
        Ok(index) => index,
        Err(gix::worktree::open_index::Error::IndexFile(gix::index::file::init::Error::Io(
            err,
        ))) if err.kind() == std::io::ErrorKind::NotFound
            && git_repo
                .index_path()
                .symlink_metadata()
                .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
        {
            // If the requested revision already matches the source, JJ can
            // skip Git checkout entirely. The no-checkout worktree then has
            // no index; the new empty working-copy commit is its parent tree.
            let final_tree = final_commit.tree();
            let Some(tree_id) = final_tree.tree_ids().as_resolved() else {
                return Ok(());
            };
            index_from_tree(tree_id)?
        }
        Err(err) => {
            return Err(user_error(format!(
                "Failed to read snapshot Git index: {err}"
            )));
        }
    };

    let mut client = DirectScanClient::for_root(root)
        .map_err(|err| user_error(format!("Failed to open AWACS Git index baseline: {err}")))?;
    let initial = client
        .initial_baseline(root)
        .map_err(|err| user_error(format!("Failed to load AWACS Git index baseline: {err}")))?;
    if initial.identity.filesystem_uuid != snapshot_identity.fs_uuid
        || initial.identity.subvolume_uuid != snapshot_identity.subvol_uuid
        || !initial.identity.read_only
    {
        return Err(user_error(
            "AWACS Git index baseline does not match the initialized snapshot",
        ));
    }
    let owner_id = initial
        .retention_token
        .as_slice()
        .try_into()
        .map_err(|_| user_error("AWACS Git index baseline has an invalid owner"))?;
    let mut lease = client
        .begin_scan(&BeginScanRequest {
            live_root: root.to_owned(),
            baseline_owner_id: owner_id,
            previous_baseline: Some(initial),
            allow_full_invalidation: false,
        })
        .map_err(|err| {
            user_error(format!(
                "Failed to establish AWACS Git index baseline: {err}"
            ))
        })?;
    let result = (|| {
        client.validate_scan_root(&lease).map_err(|err| {
            user_error(format!(
                "Failed to validate AWACS Git index baseline: {err}"
            ))
        })?;
        let changed = snapshot_git_invalidation_matcher(&lease.invalidation);
        let materialized = PrefixMatcher::new(source_sparse_patterns);
        let mut source_entries = source_index.entries().iter().peekable();
        let dirty = index
            .entries()
            .iter()
            .map(|entry| {
                let path = entry.path(&index);
                // Both indexes are sorted. Walk them once instead of looking
                // up each entry or reading each worktree file.
                while source_entries
                    .peek()
                    .is_some_and(|source| source.path(&source_index) < path)
                {
                    source_entries.next();
                }
                let matches_source = source_entries.peek().is_some_and(|source| {
                    source.path(&source_index) == path
                        && source.stage() == entry.stage()
                        && source.id == entry.id
                        && source.mode == entry.mode
                });
                !matches_source
                    || entry
                        .flags
                        .contains(gix::index::entry::Flags::INTENT_TO_ADD)
                    || snapshot_git_entry_is_dirty(
                        path,
                        entry.mode,
                        entry.stage(),
                        changed.as_ref(),
                        &materialized,
                    )
            })
            .collect_vec();
        let token = encode_git_fsmonitor_token(&lease.next_baseline).map_err(|err| {
            user_error(format!("Failed to encode AWACS Git index baseline: {err}"))
        })?;
        let monitor = gix::index::extension::FsMonitor::from_token(token, &dirty)
            .ok_or_else(|| user_error("Cannot represent snapshot Git index validity bitmap"))?;
        index.set_fs_monitor(monitor);
        // Git compares cleaned contents rather than JJ's raw blobs. Inspect
        // the final checkout's attributes before publishing any clean bits.
        invalidate_snapshot_git_conversions_in_index(&git_repo, &mut index)?;
        let mut index_lock = std::io::BufWriter::with_capacity(64 * 1024, index_lock);
        index
            .write_to(&mut index_lock, gix::index::write::Options::default())
            .map_err(|err| user_error(format!("Failed to write snapshot Git index: {err}")))?;
        let index_lock = index_lock
            .into_inner()
            .map_err(|err| user_error(format!("Failed to flush snapshot Git index: {err}")))?;
        index_lock
            .commit()
            .map_err(|err| user_error(format!("Failed to publish snapshot Git index: {err}")))?;
        client
            .observe_persisted_git_index(root, &lease.next_baseline)
            .map_err(|err| user_error(format!("Failed to retain published Git index: {err}")))?;
        lease.promote().map_err(|err| {
            user_error(format!("Failed to promote AWACS Git index baseline: {err}"))
        })?;
        lease
            .finish(ScanOutcome::Committed)
            .map_err(|err| user_error(format!("Failed to commit AWACS Git index baseline: {err}")))
    })();
    if result.is_err() {
        drop(lease.finish(ScanOutcome::Aborted));
    }
    result
}

/// Compare Git's effective configuration, including conditional includes. A
/// worktree-specific conversion rule cannot authorize inherited cache claims.
#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
fn snapshot_git_index_config(
    executable: &Path,
    root: &Path,
    git_repo: &gix::Repository,
) -> Result<Option<Vec<u8>>, CommandError> {
    let output = Command::new(executable)
        .arg("-C")
        .arg(root)
        .args(["config", "--null", "--list"])
        .output()
        .map_err(|err| user_error(format!("Failed to inspect Git index configuration: {err}")))?;
    if !output.status.success() {
        return Err(user_error("Failed to inspect Git index configuration"));
    }
    // External attributes and custom clean/process filters cannot authorize
    // inherited claims. Even identical filter commands can depend on cwd or
    // worktree-local state. Keep the semantic seeding path, which checks
    // conversions explicitly, without scanning every path on the common path.
    if git_repo.common_dir().join("info/attributes").exists()
        || output.stdout.split(|byte| *byte == 0).any(|entry| {
            entry
                .split(|byte| *byte == b'\n')
                .next()
                .is_some_and(|key| {
                    key.eq_ignore_ascii_case(b"core.attributesfile")
                        || (key
                            .get(..7)
                            .is_some_and(|section| section.eq_ignore_ascii_case(b"filter."))
                            && key.rsplit(|byte| *byte == b'.').next().is_some_and(|name| {
                                name.eq_ignore_ascii_case(b"clean")
                                    || name.eq_ignore_ascii_case(b"process")
                            }))
                })
        })
    {
        return Ok(None);
    }
    Ok(Some(output.stdout))
}

#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
fn git_ignores_jj_metadata(executable: &Path, root: &Path) -> Result<bool, CommandError> {
    let output = Command::new(executable)
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "--quiet", "--no-index", "--", ".jj/"])
        .output()
        .map_err(|err| user_error(format!("Failed to check Git metadata exclusion: {err}")))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(user_error("Failed to check Git metadata exclusion")),
    }
}

#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
#[instrument(skip_all)]
fn reuse_snapshot_git_index(
    inherited: &mut SnapshotGitIndex,
    git_repo: &gix::Repository,
    root: &Path,
    tree_id: &jj_lib::backend::TreeId,
    snapshot_identity: &SnapshotIdentity,
) -> Result<bool, CommandError> {
    use btrfs_awacs::git_index_handoff::clone_or_copy_index;
    use btrfs_awacs::git_index_handoff::seed_git_fsmonitor_index;
    use btrfs_awacs::scan::BeginScanRequest;
    use btrfs_awacs::scan::ScanClient as _;
    use btrfs_awacs::scan::ScanOutcome;
    use btrfs_awacs::scan_facade::DirectScanClient;
    use gix::index::entry::Flags;
    use gix::index::entry::Stage;

    if snapshot_git_index_config(&inherited.git_executable, root, git_repo)?.as_ref()
        != Some(&inherited.config)
    {
        return Ok(false);
    }
    let temporary = tempfile::Builder::new()
        .prefix("jj-index-handoff-")
        .tempdir_in(git_repo.path())
        .map_err(|err| user_error(format!("Failed to prepare Git index handoff: {err}")))?;
    let index_path = temporary.path().join("index");
    inherited
        .handoff
        .copy_index(&index_path)
        .map_err(user_error)?;
    let index = match gix::index::File::at(
        index_path.clone(),
        git_repo.object_hash(),
        false,
        gix::index::decode::Options::default(),
    ) {
        Ok(index) => index,
        Err(err) => {
            // A split index needs adjacent sharedindex files that this capture
            // deliberately does not inherit. Unsupported or damaged indexes
            // also lose only this optimization, as with a missing TREE proof.
            tracing::debug!(%err, "captured Git index is ineligible for reuse");
            return Ok(false);
        }
    };
    // TREE is invalidated by staged edits. A valid full root proves that this
    // index describes JJ's requested tree without reconstructing every entry.
    if !index.tree().is_some_and(|tree| {
        tree.name.is_empty()
            && tree.id.as_bytes() == tree_id.as_bytes()
            && tree.num_entries.map(|count| count as usize) == Some(index.entries().len())
    }) || index.is_sparse()
        || index.entries().iter().any(|entry| {
            entry.stage() != Stage::Unconflicted
                || entry
                    .flags
                    .intersects(Flags::INTENT_TO_ADD | Flags::SKIP_WORKTREE)
        })
    {
        return Ok(false);
    }
    drop(index);
    let ignored_jj_metadata =
        inherited.ignored_jj_metadata && git_ignores_jj_metadata(&inherited.git_executable, root)?;

    let mut index_lock = gix::lock::File::acquire_to_update_resource(
        git_repo.index_path(),
        gix::lock::acquire::Fail::Immediately,
        None,
    )
    .map_err(|err| user_error(format!("Failed to lock snapshot Git index: {err}")))?;
    let mut client = DirectScanClient::for_root(root).map_err(|err| user_error(err.to_string()))?;
    let initial = client
        .initial_baseline(root)
        .map_err(|err| user_error(err.to_string()))?;
    if initial.identity.filesystem_uuid != snapshot_identity.fs_uuid
        || initial.identity.subvolume_uuid != snapshot_identity.subvol_uuid
        || !initial.identity.read_only
    {
        return Err(user_error(
            "AWACS Git index baseline does not match the initialized snapshot",
        ));
    }
    let owner_id = initial
        .retention_token
        .as_slice()
        .try_into()
        .map_err(|_| user_error("AWACS Git index baseline has an invalid owner"))?;
    let mut lease = client
        .begin_scan(&BeginScanRequest {
            live_root: root.to_owned(),
            baseline_owner_id: owner_id,
            previous_baseline: Some(initial),
            allow_full_invalidation: false,
        })
        .map_err(|err| user_error(err.to_string()))?;
    let result = (|| {
        client
            .validate_scan_root(&lease)
            .map_err(|err| user_error(err.to_string()))?;
        let mut invalidation = inherited
            .handoff
            .reconcile(&mut client, &mut lease, root)
            .map_err(user_error)?;
        // JJ reserves this directory, but Git can still expose it as
        // untracked. Its replacement is irrelevant to UNTR only if Git
        // excludes the directory in both worktrees.
        match &mut invalidation {
            Invalidation::ExactPaths(paths) | Invalidation::Prefixes(paths)
                if ignored_jj_metadata =>
            {
                paths.retain(|path| path != b".jj" && !path.starts_with(b".jj/"));
            }
            _ => {}
        }
        seed_git_fsmonitor_index(
            &index_path,
            root,
            git_repo.object_hash().len_in_bytes(),
            &lease.next_baseline,
            &invalidation,
        )
        .map_err(user_error)?;
        let source = fs::File::open(&index_path)
            .map_err(|err| user_error(format!("Failed to open reconciled Git index: {err}")))?;
        index_lock
            .with_mut(|destination| clone_or_copy_index(&source, destination))
            .map_err(|err| user_error(format!("Failed to write snapshot Git index: {err}")))?;
        index_lock
            .commit()
            .map_err(|err| user_error(format!("Failed to publish snapshot Git index: {err}")))?;
        client
            .observe_persisted_git_index(root, &lease.next_baseline)
            .map_err(|err| user_error(format!("Failed to retain published Git index: {err}")))?;
        lease.promote().map_err(|err| user_error(err.to_string()))?;
        lease
            .finish(ScanOutcome::Committed)
            .map_err(|err| user_error(err.to_string()))?;
        Ok(true)
    })();
    if result.is_err() {
        drop(lease.finish(ScanOutcome::Aborted));
    }
    result
}

#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
fn snapshot_git_invalidation_matcher(invalidation: &Invalidation) -> Box<dyn Matcher> {
    use jj_lib::matchers::EverythingMatcher;
    use jj_lib::matchers::FilesMatcher;
    use jj_lib::matchers::PrefixMatcher;
    use jj_lib::repo_path::RepoPathBuf;

    let paths = match invalidation {
        Invalidation::Full => return Box::new(EverythingMatcher),
        Invalidation::ExactPaths(paths) | Invalidation::Prefixes(paths) => paths,
    };
    let Some(paths) = paths
        .iter()
        .map(|path| {
            let path = std::str::from_utf8(path).ok()?;
            RepoPathBuf::from_internal_string(path).ok()
        })
        .collect::<Option<Vec<_>>>()
    else {
        // Unsupported path representations cannot authorize a clean entry.
        return Box::new(EverythingMatcher);
    };
    match invalidation {
        Invalidation::ExactPaths(_) => Box::new(FilesMatcher::new(paths)),
        Invalidation::Prefixes(_) => Box::new(PrefixMatcher::new(paths)),
        Invalidation::Full => unreachable!(),
    }
}

#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
fn snapshot_git_entry_is_dirty(
    path: &[u8],
    mode: gix::index::entry::Mode,
    stage: gix::index::entry::Stage,
    changed: &dyn Matcher,
    materialized: &dyn Matcher,
) -> bool {
    use gix::index::entry::Mode;
    use gix::index::entry::Stage;
    use jj_lib::repo_path::RepoPath;

    if stage != Stage::Unconflicted
        || !matches!(mode, Mode::FILE | Mode::FILE_EXECUTABLE | Mode::SYMLINK)
    {
        return true;
    }
    let Some(path) = std::str::from_utf8(path)
        .ok()
        .and_then(|path| RepoPath::from_internal_string(path).ok())
    else {
        return true;
    };
    !materialized.matches(path) || changed.matches(path)
}

#[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
fn invalidate_snapshot_git_conversions_in_index(
    git_repo: &gix::Repository,
    index: &mut gix::index::State,
) -> Result<bool, CommandError> {
    use gix::attrs::StateRef;
    use gix::index::entry::Flags;
    use gix::index::entry::Mode;
    use gix::worktree::stack::state::attributes::Source;

    let Some(monitor) = index.fs_monitor() else {
        return Ok(false);
    };
    let mut dirty = vec![false; index.entries().len()];
    let mut changed = monitor
        .for_each_dirty_entry(|position| {
            *dirty.get_mut(position)? = true;
            Some(())
        })
        .is_none();
    if changed {
        // A malformed bitmap cannot authorize any clean entries.
        dirty.fill(true);
    }
    // "input" and invalid values both require conservative handling. Only
    // absent or explicitly false autocrlf allows raw content to prove validity.
    let autocrlf = !matches!(
        git_repo.config_snapshot().try_boolean("core.autocrlf"),
        Ok(None | Some(false))
    );
    let mut attributes = git_repo
        .attributes_only(index, Source::WorktreeThenIdMapping)
        .map_err(|err| user_error(format!("Failed to inspect snapshot Git attributes: {err}")))?;
    let mut matches = attributes.selected_attribute_matches([
        "crlf",
        "ident",
        "filter",
        "eol",
        "text",
        "working-tree-encoding",
    ]);
    for (entry, dirty) in index.entries().iter().zip(&mut dirty) {
        if *dirty || !matches!(entry.mode, Mode::FILE | Mode::FILE_EXECUTABLE) {
            continue;
        }
        let conversion = if autocrlf {
            true
        } else {
            // The stack reads per-directory attributes, never the tracked file
            // itself, and expands attribute macros without executing filters.
            attributes
                .at_entry(entry.path(index), Some(entry.mode))
                .map_err(|err| {
                    user_error(format!("Failed to inspect snapshot Git attributes: {err}"))
                })?
                .matching_attributes(&mut matches);
            matches.iter_selected().any(|matched| {
                matches!(matched.assignment.state, StateRef::Set | StateRef::Value(_))
                    || (matched.assignment.name.as_str() == "working-tree-encoding"
                        && matched.assignment.state == StateRef::Unset)
            })
        };
        if conversion {
            *dirty = true;
            changed = true;
        }
    }
    if changed {
        // Preserve the existing cursor and every prior dirty bit; this guard
        // only revokes validity and must never establish a newer baseline.
        index.edit_preserving_fs_monitor(|index| {
            for (entry, dirty) in index.entries_mut().iter_mut().zip(dirty) {
                if dirty {
                    entry.flags.remove(Flags::FSMONITOR_VALID);
                }
            }
        });
    }
    Ok(changed)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
async fn seed_snapshot_workspace_tree(
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
    source_commit: &jj_lib::commit::Commit,
    source_sparse_patterns: &[jj_lib::repo_path::RepoPathBuf],
) -> Result<(), CommandError> {
    let operation_id = workspace_command.repo().op_id().clone();
    let (mut locked_workspace, _commit) = workspace_command
        .unchecked_start_working_copy_mutation()
        .await?;
    if !seed_local_working_copy_tree_with_sparse_patterns(
        locked_workspace.locked_wc(),
        &source_commit.tree(),
        Some(source_sparse_patterns),
    )
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
        .snapshot_for_subvolume_enable(ui, &|_| Ok(()), None)
        .await
}

/// Attempts a Btrfs snapshot, then checks whether `source` is a subvolume
/// only if the snapshot fails. Returns `Ok(false)` when it is not so auto
/// mode can fall back to a normal workspace.
fn create_btrfs_snapshot(
    source: &Path,
    destination: &Path,
) -> Result<Option<SnapshotPreparation>, CommandError> {
    let total_started = Instant::now();
    let operation_started = Instant::now();
    if let Err(error) = recursive_btrfs_snapshot(source, destination) {
        if !is_btrfs_subvolume(source)? {
            return Ok(None);
        }
        return Err(user_error(format!(
            "Failed to create recursive Btrfs snapshot: {error}"
        )));
    }
    tracing::debug!(
        elapsed = ?operation_started.elapsed(),
        "created Btrfs snapshot"
    );
    let operation_started = Instant::now();
    if let Err(mut error) = remove_copied_metadata(&destination.join(".jj")) {
        if let Err(cleanup_error) = remove_failed_snapshot_destination(destination) {
            error.add_hint(format!(
                "Failed to remove partial snapshot workspace: {cleanup_error:?}"
            ));
        }
        return Err(error);
    }
    tracing::debug!(
        elapsed = ?operation_started.elapsed(),
        "removed copied .jj metadata"
    );
    let operation_started = Instant::now();
    if let Err(mut error) = remove_copied_metadata(&destination.join(".git")) {
        if let Err(cleanup_error) = remove_failed_snapshot_destination(destination) {
            error.add_hint(format!(
                "Failed to remove partial snapshot workspace: {cleanup_error:?}"
            ));
        }
        return Err(error);
    }
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
        initialized_awacs_snapshot: None,
        // Retain JJ's A independently before Git can advance its lane and GC
        // the initialization cut. The child journal adopts this exact owner.
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        awacs_workspace_owner: rand::random(),
        #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
        initialized_awacs_git_consumer: false,
    }))
}

fn remove_copied_metadata(path: &Path) -> Result<(), CommandError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context(path).map_err(Into::into),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        remove_directory_tree(path).context(path)?;
    } else {
        fs::remove_file(path).context(path)?;
    }
    Ok(())
}

fn cleanup_failed_snapshot_workspace(
    destination: &Path,
    git_worktree_plan: Option<&GitWorktreePlan>,
    lock_root: &Path,
) -> Result<(), CommandError> {
    remove_failed_snapshot_destination(destination)?;
    if let Some(plan) = git_worktree_plan {
        prune_git_worktree_metadata(plan, lock_root)?;
    }
    Ok(())
}

fn remove_failed_snapshot_destination(destination: &Path) -> Result<(), CommandError> {
    if !destination.exists() {
        return Ok(());
    }
    if !delete_btrfs_subvolume(destination)? {
        return Err(user_error(format!(
            "Failed to remove partial snapshot workspace at {}: path is not on Btrfs",
            destination.display()
        )));
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

    let mut cmd = git_worktree_add_command(plan, workspace_root);
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

fn git_worktree_add_command(plan: &GitWorktreePlan, workspace_root: &Path) -> Command {
    let mut cmd = Command::new(&plan.git_executable);
    // JJ already owns the physical snapshot and only needs Git to allocate a
    // linked-worktree identity. A source repository may enable Git's own
    // AWACS snapshot mode, whose required path rejects this intentional
    // no-checkout temporary worktree. Command-scoped false is accepted by
    // stock Git as an unknown config key and bypasses the outer mechanism in
    // snapshot-aware Git.
    cmd.args(["-c", "worktree.btrfsSnapshot=false"])
        .arg("--git-dir")
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
    cmd
}

fn prune_git_worktree_metadata(
    plan: &GitWorktreePlan,
    lock_root: &Path,
) -> Result<(), CommandError> {
    let lock_path = lock_root.join("git_import_export.lock");
    let _lock = FileLock::lock(lock_path.clone()).map_err(|err| {
        user_error(format!(
            "Failed to take lock for Git import/export at {}: {err}",
            lock_path.display()
        ))
    })?;
    let output = Command::new(&plan.git_executable)
        .arg("--git-dir")
        .arg(&plan.git_repo_path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
    fn snapshot_git_metadata_exclusion_requires_git_ignore_rule() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        gix::ThreadSafeRepository::init_opts(
            &root,
            gix::create::Kind::WithWorktree,
            gix::create::Options::default(),
            gix::open::Options::isolated(),
        )
        .unwrap();
        fs::create_dir(root.join(".jj")).unwrap();
        fs::write(root.join(".jj/metadata"), "source metadata").unwrap();
        let executable = temp.path().join("isolated-git");
        fs::write(
            &executable,
            b"#!/bin/sh\nexport GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null\nexec git \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!git_ignores_jj_metadata(&executable, &root).unwrap());
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::write(root.join(".git/info/exclude"), ".jj/\n").unwrap();
        assert!(git_ignores_jj_metadata(&executable, &root).unwrap());
        fs::write(root.join(".git/info/exclude"), ".jj-other/\n").unwrap();
        assert!(!git_ignores_jj_metadata(&executable, &root).unwrap());
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
    fn snapshot_git_index_config_rejects_custom_filters_even_when_profiles_match() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let child = temp.path().join("child");
        let open_repo = |root: &Path| {
            fs::create_dir(root).unwrap();
            gix::ThreadSafeRepository::init_opts(
                root,
                gix::create::Kind::WithWorktree,
                gix::create::Options::default(),
                gix::open::Options::isolated(),
            )
            .unwrap()
            .to_thread_local()
        };
        let source_repo = open_repo(&source);
        let child_repo = open_repo(&child);
        let executable = temp.path().join("git-config");
        fs::write(&executable, b"#!/bin/sh\ncat \"$0.config\"\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let config_path = temp.path().join("git-config.config");

        for (config, eligible) in [
            (b"core.autocrlf\ntrue\0core.eol\ncrlf\0".as_slice(), true),
            (
                b"filter.local.clean\ncase \"$PWD\" in */source) cat;; *) sed s/before/after/;; esac\0".as_slice(),
                false,
            ),
            (b"filter.Local.With.Dots.process\nhelper\0".as_slice(), false),
            (b"FiLtEr.Local.CLEAN\nhelper\0".as_slice(), false),
            (b"filter.local.smudge\nhelper\0".as_slice(), true),
            (b"core.attributesfile\n/attributes\0".as_slice(), false),
        ] {
            fs::write(&config_path, config).unwrap();
            let source_profile =
                snapshot_git_index_config(&executable, &source, &source_repo).unwrap();
            let child_profile =
                snapshot_git_index_config(&executable, &child, &child_repo).unwrap();
            assert_eq!(source_profile, child_profile);
            assert_eq!(source_profile.is_some(), eligible);
        }
    }

    #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
    fn conversion_test_index(
        repo: &gix::Repository,
        entries: &[(&str, gix::index::entry::Mode)],
        dirty: &[bool],
    ) -> gix::index::State {
        let mut index = gix::index::State::new(repo.object_hash());
        for (path, mode) in entries {
            index.dangerously_push_entry(
                gix::index::entry::Stat::default(),
                gix::ObjectId::null(repo.object_hash()),
                gix::index::entry::Flags::empty(),
                *mode,
                gix::bstr::BStr::new(path),
            );
        }
        index.sort_entries();
        index.set_fs_monitor(
            gix::index::extension::FsMonitor::from_token("unchanged-cursor", dirty).unwrap(),
        );
        index
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
    fn snapshot_git_conversion_guard_uses_final_attributes_and_preserves_monitor() {
        use gix::index::entry::Mode;

        let temp = tempfile::tempdir().unwrap();
        let repo = gix::ThreadSafeRepository::init_opts(
            temp.path(),
            gix::create::Kind::WithWorktree,
            gix::create::Options::default(),
            gix::open::Options::isolated(),
        )
        .unwrap()
        .to_thread_local();
        let mut index = conversion_test_index(
            &repo,
            &[
                ("binary", Mode::FILE),
                ("converted", Mode::FILE_EXECUTABLE),
                ("dir/file", Mode::FILE),
                ("link", Mode::SYMLINK),
                ("plain", Mode::FILE),
                ("prior", Mode::FILE),
            ],
            &[false, false, false, false, false, true],
        );
        fs::create_dir(temp.path().join("dir")).unwrap();
        fs::write(
            temp.path().join(".gitattributes"),
            "* binary linguist-generated\n",
        )
        .unwrap();
        assert!(!invalidate_snapshot_git_conversions_in_index(&repo, &mut index).unwrap());

        // These are the attributes after checkout, which need not match the
        // source attributes used when the index was seeded. No tracked file is
        // materialized here: the guard must not open or stat their contents.
        fs::write(
            temp.path().join(".gitattributes"),
            "* binary linguist-generated\nconverted filter=test\nlink text\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("dir/.gitattributes"),
            "file working-tree-encoding=UTF-16\n",
        )
        .unwrap();
        assert!(invalidate_snapshot_git_conversions_in_index(&repo, &mut index).unwrap());
        let monitor = index.fs_monitor().unwrap();
        assert_eq!(monitor.token().unwrap(), "unchanged-cursor");
        let mut dirty_positions = Vec::new();
        monitor
            .for_each_dirty_entry(|position| {
                dirty_positions.push(position);
                Some(())
            })
            .unwrap();
        assert_eq!(dirty_positions, [1, 2, 5]);
        assert!(!invalidate_snapshot_git_conversions_in_index(&repo, &mut index).unwrap());
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
    fn snapshot_git_conversion_guard_handles_attribute_states_and_autocrlf() {
        use gix::index::entry::Mode;

        let temp = tempfile::tempdir().unwrap();
        let repo = gix::ThreadSafeRepository::init_opts(
            temp.path(),
            gix::create::Kind::WithWorktree,
            gix::create::Options::default(),
            gix::open::Options::isolated(),
        )
        .unwrap()
        .to_thread_local();
        for (attributes, conversion) in [
            ("* -text -crlf -ident -filter -eol\n", false),
            ("* !working-tree-encoding\n", false),
            ("* -working-tree-encoding\n", true),
            ("* text\n", true),
            ("* text=auto\n", true),
            ("* crlf\n", true),
            ("* ident\n", true),
            ("* filter=test\n", true),
            ("* eol=lf\n", true),
            ("* working-tree-encoding=UTF-16\n", true),
            ("[attr]converted filter=test\n* converted\n", true),
        ] {
            fs::write(temp.path().join(".gitattributes"), attributes).unwrap();
            let mut index = conversion_test_index(&repo, &[("file", Mode::FILE)], &[false]);
            assert_eq!(
                invalidate_snapshot_git_conversions_in_index(&repo, &mut index).unwrap(),
                conversion,
                "{attributes:?}",
            );
        }
        fs::remove_file(temp.path().join(".gitattributes")).unwrap();
        for (autocrlf, conversion) in [
            ("false", false),
            ("true", true),
            ("input", true),
            ("invalid", true),
        ] {
            let repo = gix::open_opts(
                temp.path(),
                gix::open::Options::isolated().cli_overrides([format!("core.autocrlf={autocrlf}")]),
            )
            .unwrap();
            let mut index = conversion_test_index(
                &repo,
                &[("file", Mode::FILE), ("link", Mode::SYMLINK)],
                &[false, false],
            );
            assert_eq!(
                invalidate_snapshot_git_conversions_in_index(&repo, &mut index).unwrap(),
                conversion,
                "core.autocrlf={autocrlf}",
            );
            index
                .fs_monitor()
                .unwrap()
                .for_each_dirty_entry(|position| {
                    assert_eq!(position, 0, "symlinks bypass conversion");
                    Some(())
                })
                .unwrap();
        }
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
    fn snapshot_git_dirty_mask_respects_exact_paths_and_prefix_boundaries() {
        use gix::index::entry::Mode;
        use gix::index::entry::Stage;
        use jj_lib::matchers::EverythingMatcher;

        let dirty = |path: &[u8], changed: &dyn Matcher| {
            snapshot_git_entry_is_dirty(
                path,
                Mode::FILE,
                Stage::Unconflicted,
                changed,
                &EverythingMatcher,
            )
        };
        let exact = snapshot_git_invalidation_matcher(&Invalidation::ExactPaths(vec![
            b"dir/file".to_vec(),
        ]));
        assert!(dirty(b"dir/file", exact.as_ref()));
        assert!(!dirty(b"dir/file/child", exact.as_ref()));
        assert!(!dirty(b"dir/file-other", exact.as_ref()));
        let prefix =
            snapshot_git_invalidation_matcher(&Invalidation::Prefixes(vec![b"dir".to_vec()]));
        assert!(dirty(b"dir", prefix.as_ref()));
        assert!(dirty(b"dir/file", prefix.as_ref()));
        assert!(!dirty(b"directory/file", prefix.as_ref()));
        assert!(!dirty(b"other/dir/file", prefix.as_ref()));
        assert!(dirty(
            b"unchanged",
            snapshot_git_invalidation_matcher(&Invalidation::Full).as_ref(),
        ));
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "awacs", feature = "git"))]
    fn snapshot_git_dirty_mask_keeps_sparse_and_unsupported_entries_dirty() {
        use gix::index::entry::Mode;
        use gix::index::entry::Stage;
        use jj_lib::matchers::PrefixMatcher;
        use jj_lib::repo_path::RepoPathBuf;

        let unchanged = snapshot_git_invalidation_matcher(&Invalidation::ExactPaths(vec![]));
        let materialized =
            PrefixMatcher::new([RepoPathBuf::from_internal_string("visible").unwrap()]);
        let dirty = |path: &[u8], mode, stage| {
            snapshot_git_entry_is_dirty(path, mode, stage, unchanged.as_ref(), &materialized)
        };
        for mode in [Mode::FILE, Mode::FILE_EXECUTABLE, Mode::SYMLINK] {
            assert!(!dirty(b"visible/file", mode, Stage::Unconflicted));
        }
        assert!(dirty(
            b"visible-other/file",
            Mode::FILE,
            Stage::Unconflicted
        ));
        assert!(dirty(b"excluded/file", Mode::FILE, Stage::Unconflicted));
        for mode in [Mode::DIR, Mode::COMMIT] {
            assert!(dirty(b"visible/file", mode, Stage::Unconflicted));
        }
        for stage in [Stage::Base, Stage::Ours, Stage::Theirs] {
            assert!(dirty(b"visible/file", Mode::FILE, stage));
        }
        assert!(dirty(b"visible/\xff", Mode::FILE, Stage::Unconflicted));
        let unsupported = snapshot_git_invalidation_matcher(&Invalidation::ExactPaths(vec![
            b"invalid/\xff".to_vec(),
        ]));
        assert!(snapshot_git_entry_is_dirty(
            b"visible/file",
            Mode::FILE,
            Stage::Unconflicted,
            unsupported.as_ref(),
            &materialized,
        ));
    }

    #[test]
    fn git_worktree_add_disables_git_snapshot_mode() {
        let plan = GitWorktreePlan {
            git_repo_path: PathBuf::from("/repo/.git"),
            git_executable: PathBuf::from("git"),
            checkout_commit_id: CommitId::from_hex("1234"),
        };

        let command = git_worktree_add_command(&plan, Path::new("/repo/worktree"));
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect_vec();

        assert_eq!(
            &args[..2],
            ["-c", "worktree.btrfsSnapshot=false"],
            "JJ's internal no-checkout worktree must bypass Git snapshot mode"
        );
    }
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
