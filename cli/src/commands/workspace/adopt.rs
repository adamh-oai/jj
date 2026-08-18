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

use jj_lib::backend::CommitId;
use jj_lib::git;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use jj_lib::local_working_copy::{
    LocalWorkingCopy, seed_local_working_copy_tree, snapshot_mode_has_committed_baseline,
};
#[cfg(all(target_os = "linux", feature = "awacs"))]
use jj_lib::merged_tree::MergedTree;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use jj_lib::working_copy::WorkingCopy as _;
use jj_lib::workspace::Workspace;
use jj_lib::workspace_store::SimpleWorkspaceStore;
use jj_lib::workspace_store::WorkspaceStore as _;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use crate::commands::btrfs::begin_subvolume_mode;
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
    git_dir: PathBuf,
    common_dir: PathBuf,
    head_id: CommitId,
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
struct PendingJjConsumerSeed {
    owner_id: [u8; 16],
    snapshot_identity: btrfs_awacs::manager::SnapshotIdentity,
    working_copy_state_path: PathBuf,
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
const AWACS_JJ_PENDING_CONSUMER_MARKER: &str = "awacs-jj-pending-consumer";
#[cfg(all(target_os = "linux", feature = "awacs"))]
const AWACS_JJ_PENDING_WORKING_COPY_STATE: &str = "awacs-jj-pending-working-copy";

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
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    let pending_jj_seed = if snapshot {
        Some(read_pending_jj_consumer_seed(&worktree)?)
    } else {
        None
    };
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    let snapshot_seed_tree = if snapshot {
        if std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_some() {
            None
        } else {
            let pending_state_path = &pending_jj_seed
                .as_ref()
                .expect("snapshot adoption validated pending seed")
                .working_copy_state_path;
            if !snapshot_mode_has_committed_baseline(pending_state_path).map_err(|err| {
                user_error(format!(
                    "Failed to validate pending JJ AWACS baseline: {err}"
                ))
            })? {
                return Err(user_error(
                    "Cannot adopt snapshot worktree without a committed source JJ AWACS baseline",
                ));
            }
            let copied_working_copy = LocalWorkingCopy::load(
                main_workspace.repo_loader().store().clone(),
                worktree.root.clone(),
                pending_state_path.clone(),
                &main_settings,
            )
            .map_err(|err| user_error(format!("Failed to read pending JJ working copy: {err}")))?;
            Some(
                copied_working_copy
                    .tree()
                    .map_err(|err| user_error(format!("Failed to read copied JJ tree: {err}")))?
                    .clone(),
            )
        }
    } else {
        None
    };
    let workspace_name = workspace_name(&worktree.root, args)?;
    // Adoption publishes both repository state and an on-disk workspace-store
    // entry before its first working-copy snapshot can run. Keep that whole
    // transition serialized so a failed snapshot can roll back the same name
    // without racing another workspace lifecycle command.
    let lifecycle_store = SimpleWorkspaceStore::load(main_workspace.repo_path())?;
    let _lifecycle_lock = lifecycle_store.lock_lifecycle()?;
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

    macro_rules! adoption_try {
        ($result:expr) => {
            match $result {
                Ok(value) => value,
                Err(error) => {
                    let mut error = CommandError::from(error);
                    if let Err(rollback_error) = rollback_failed_adoption(
                        command,
                        &main_workspace,
                        &lifecycle_store,
                        &worktree.root,
                        &workspace_name,
                    )
                    .await
                    {
                        error.add_hint(format!(
                            "Failed to roll back partial workspace adoption: {}",
                            rollback_error.error
                        ));
                    }
                    return Err(error);
                }
            }
        };
    }

    if snapshot {
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        adoption_try!(begin_subvolume_mode(&worktree.root));
        #[cfg(not(all(target_os = "linux", feature = "awacs")))]
        adoption_try!(set_subvolume_mode(&worktree.root, true));
    }

    // `init_workspace_with_existing_repo()` starts at the root commit. Replace
    // that placeholder with an empty working-copy commit on the existing Git
    // HEAD, then reset only jj's working-copy metadata. The files and Git index
    // are already materialized by Git and must not be checked out again.
    let mut tx = repo.start_transaction();
    let wc_commit = adoption_try!(
        tx.repo_mut()
            .check_out(workspace_name.clone(), &head_commit)
            .await
    );
    tx.repo_mut()
        .set_git_head_target(&workspace_name, RefTarget::normal(worktree.head_id.clone()));
    adoption_try!(tx.repo_mut().rebase_descendants().await);
    let unpublished = adoption_try!(
        tx.write(format!(
            "adopt existing Git worktree as workspace '{}'",
            workspace_name.as_symbol()
        ))
        .await
    );
    let repo = if command.should_commit_transaction() {
        adoption_try!(unpublished.publish().await)
    } else {
        unpublished.leave_unpublished()
    };

    #[cfg(all(target_os = "linux", feature = "awacs"))]
    if snapshot {
        let pending_jj_seed = pending_jj_seed.expect("snapshot adoption validated pending seed");
        // Workspace initialization happened before the marker existed and
        // therefore owns the ordinary working-copy implementation. Reload
        // after the marker is durable so the compact journal is selected.
        drop(workspace);
        let mut workspace_command = adoption_try!(
            command
                .workspace_helper_no_snapshot_at(ui, &worktree.root)
                .await
        );
        // The pending tree describes immutable child snapshot A. Git may have
        // checked out a different HEAD into the live worktree after A was
        // created; in that case the final snapshot below reconciles A -> B
        // against the working-copy commit already based on that Git HEAD.
        let synthetic_seed_tree = wc_commit.tree();
        adoption_try!(
            seed_snapshot_adopt_tree(
                &mut workspace_command,
                snapshot_seed_tree.as_ref().unwrap_or(&synthetic_seed_tree),
            )
            .await
        );
        adoption_try!(
            workspace_command
                .seed_initialized_snapshot_workspace_awacs_baseline(
                    ui,
                    &pending_jj_seed.snapshot_identity,
                    Some(pending_jj_seed.owner_id),
                )
                .await
        );
        adoption_try!(set_subvolume_mode(&worktree.root, true));
        // This is also the checkout-reconciliation step for a Git worktree
        // whose requested HEAD differs from the inherited source HEAD.
        adoption_try!(workspace_command.maybe_snapshot(ui).await);
        clear_pending_jj_consumer_seed(&worktree)?;
        writeln!(
            ui.status(),
            "Adopted Git worktree as workspace '{}'",
            workspace_name.as_symbol()
        )?;
        return Ok(());
    }

    let mut locked_workspace = adoption_try!(workspace.start_working_copy_mutation().await);
    adoption_try!(locked_workspace.locked_wc().reset(&wc_commit).await);
    adoption_try!(locked_workspace.finish(repo.op_id().clone()).await);

    let mut workspace_command = adoption_try!(command.for_workable_repo(ui, workspace, repo));
    adoption_try!(workspace_command.maybe_snapshot(ui).await);
    writeln!(
        ui.status(),
        "Adopted Git worktree as workspace '{}'",
        workspace_name.as_symbol()
    )?;
    Ok(())
}

async fn rollback_failed_adoption(
    command: &CommandHelper,
    main_workspace: &Workspace,
    lifecycle_store: &SimpleWorkspaceStore,
    worktree_root: &Path,
    workspace_name: &WorkspaceNameBuf,
) -> Result<(), CommandError> {
    let repo = main_workspace.repo_loader().load_at_head().await?;
    if repo.view().get_wc_commit_id(workspace_name).is_some() {
        let mut tx = repo.start_transaction();
        tx.repo_mut().remove_workspace(workspace_name).await?;
        tx.repo_mut().rebase_descendants().await?;
        let unpublished = tx
            .write(format!(
                "roll back failed workspace adoption '{}'",
                workspace_name.as_symbol()
            ))
            .await?;
        if command.should_commit_transaction() {
            unpublished.publish().await?;
        } else {
            unpublished.leave_unpublished();
        }
    }
    lifecycle_store.forget(&[workspace_name.as_ref()])?;
    let jj_dir = worktree_root.join(".jj");
    match fs::remove_dir_all(&jj_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(user_error(format!(
                "Failed to remove partial workspace metadata at {}: {error}",
                jj_dir.display()
            )));
        }
    }
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
        git_dir,
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

#[cfg(all(target_os = "linux", feature = "awacs"))]
async fn seed_snapshot_adopt_tree(
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
    tree: &MergedTree,
) -> Result<(), CommandError> {
    let operation_id = workspace_command.repo().op_id().clone();
    let (mut locked_workspace, _commit) = workspace_command
        .unchecked_start_working_copy_mutation()
        .await?;
    if !seed_local_working_copy_tree(locked_workspace.locked_wc(), tree)
        .await
        .map_err(|err| internal_error_with_message("Failed to seed adopted snapshot tree", err))?
    {
        return Err(internal_error_with_message(
            "Failed to seed adopted snapshot working copy",
            "new workspace did not reload as snapshot-backed",
        ));
    }
    locked_workspace.finish(operation_id).await?;
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn read_pending_jj_consumer_seed(
    worktree: &ExistingGitWorktree,
) -> Result<PendingJjConsumerSeed, CommandError> {
    let path = worktree.git_dir.join(AWACS_JJ_PENDING_CONSUMER_MARKER);
    let contents = fs::read_to_string(&path).map_err(|err| {
        user_error(format!(
            "Cannot adopt snapshot worktree without a pending JJ AWACS seed: {err}"
        ))
    })?;
    let mut fields = contents.trim_end().split(':');
    if fields.next() != Some("awacs-jj-pending-v2") {
        return Err(user_error(
            "Pending JJ AWACS seed has an unsupported format; recreate the Git worktree",
        ));
    }
    let parse_uuid = |field: Option<&str>, name: &str| -> Result<[u8; 16], CommandError> {
        let value =
            field.ok_or_else(|| user_error(format!("Pending JJ AWACS seed lacks {name}")))?;
        parse_pending_uuid_bytes(value.as_bytes())
            .map_err(|err| user_error(format!("Pending JJ AWACS seed has invalid {name}: {err}")))
    };
    let owner_id = parse_uuid(fields.next(), "consumer owner")?;
    let fs_uuid = parse_uuid(fields.next(), "filesystem identity")?;
    let subvol_uuid = parse_uuid(fields.next(), "snapshot identity")?;
    if fields.next().is_some() {
        return Err(user_error("Pending JJ AWACS seed has trailing fields"));
    }
    Ok(PendingJjConsumerSeed {
        owner_id,
        snapshot_identity: btrfs_awacs::manager::SnapshotIdentity {
            fs_uuid,
            subvol_uuid,
            parent_uuid: None,
            received_uuid: None,
            root_id: 0,
            ctransid: 0,
            otransid: 0,
            path: Vec::new(),
            readonly: true,
            created_ns: 0,
        },
        working_copy_state_path: worktree.git_dir.join(AWACS_JJ_PENDING_WORKING_COPY_STATE),
    })
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn clear_pending_jj_consumer_seed(worktree: &ExistingGitWorktree) -> Result<(), CommandError> {
    let marker = worktree.git_dir.join(AWACS_JJ_PENDING_CONSUMER_MARKER);
    match fs::remove_file(&marker) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(user_error(format!(
                "Failed to remove consumed pending JJ AWACS marker: {err}"
            )));
        }
    }
    let state = worktree.git_dir.join(AWACS_JJ_PENDING_WORKING_COPY_STATE);
    match fs::remove_dir_all(&state) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(user_error(format!(
            "Failed to remove consumed pending JJ working-copy state: {err}"
        ))),
    }
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn parse_pending_uuid_bytes(value: &[u8]) -> Result<[u8; 16], String> {
    if value.len() != 36
        || value.get(8) != Some(&b'-')
        || value.get(13) != Some(&b'-')
        || value.get(18) != Some(&b'-')
        || value.get(23) != Some(&b'-')
    {
        return Err("UUID is malformed".to_owned());
    }
    let mut bytes = [0; 16];
    let mut output = 0;
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'-' {
            index += 1;
            continue;
        }
        let nibble = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        let high = nibble(value[index]).ok_or_else(|| "UUID is malformed".to_owned())?;
        let low = nibble(value[index + 1]).ok_or_else(|| "UUID is malformed".to_owned())?;
        bytes[output] = high << 4 | low;
        output += 1;
        index += 2;
    }
    Ok(bytes)
}
