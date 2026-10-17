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

use std::slice;
use std::time::Duration;
use std::time::SystemTime;

use jj_lib::repo::Repo as _;
use jj_lib::simple_op_store::SimpleOpStore;

use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::user_error;
#[cfg(feature = "git")]
use crate::command_error::user_error_with_message;
use crate::ui::Ui;

/// Run backend-dependent garbage collection.
///
/// To garbage-collect old operations and the commits/objects referenced by
/// them, run `jj op abandon ..<some old operation>` before `jj util gc`.
#[derive(clap::Args, Clone, Debug)]
pub struct UtilGcArgs {
    /// Time threshold
    ///
    /// By default, only obsolete objects and operations older than 2 weeks are
    /// pruned.
    ///
    /// Only the string "now" can be passed to this parameter. Support for
    /// arbitrary absolute and relative timestamps will come in a subsequent
    /// release.
    #[arg(long)]
    expire: Option<String>,
}

pub async fn cmd_util_gc(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &UtilGcArgs,
) -> Result<(), CommandError> {
    if !command.is_at_head_operation() {
        return Err(user_error(
            "Cannot garbage collect from a non-head operation",
        ));
    }
    let keep_newer = match args.expire.as_deref() {
        None => SystemTime::now() - Duration::from_secs(14 * 86400),
        Some("now") => SystemTime::now() - Duration::ZERO,
        _ => return Err(user_error("--expire only accepts 'now'")),
    };
    let workspace_command = command.workspace_helper(ui).await?;

    let repo = workspace_command.repo();
    if repo.op_store().name() == SimpleOpStore::SYNC_NAME {
        #[cfg(feature = "git")]
        {
            return collect_synced_repo(repo, workspace_command.repo_path(), keep_newer).await;
        }
        #[cfg(not(feature = "git"))]
        {
            return Err(user_error(
                "Synchronization GC requires a Git-enabled jj build",
            ));
        }
    }
    repo.op_store()
        .gc(slice::from_ref(repo.op_id()), keep_newer)
        .await?;
    #[cfg(feature = "git")]
    if let Ok(git_backend) = jj_lib::git::get_git_backend(repo.store()) {
        git_backend.gc(repo.index(), keep_newer)?;
    }
    Ok(())
}

#[cfg(feature = "git")]
async fn collect_synced_repo(
    repo: &jj_lib::repo::ReadonlyRepo,
    repo_path: &std::path::Path,
    keep_newer: SystemTime,
) -> Result<(), CommandError> {
    use jj_lib::sync::anchor::AnchorStore;
    use jj_lib::sync::gc::collect_roots;
    use jj_lib::sync::state::SyncStateStore;

    // Missing state in an enabled repository is corruption, not a new empty
    // store. Hold the same lock used to establish transfer/import roots until
    // both collectors have finished; no network work occurs under this lock.
    let state_store = SyncStateStore::load(repo_path);
    let state = state_store.lock().map_err(|err| {
        user_error_with_message("Cannot read synchronization garbage-collection roots", err)
    })?;
    let anchors = AnchorStore::load(repo_path);
    let mut raw_heads = repo.loader().op_heads_store().get_op_heads().await?;
    raw_heads.push(repo.op_id().clone());
    let plan = collect_roots(
        repo.op_store().as_ref(),
        &anchors,
        state.state(),
        &raw_heads,
    )
    .await
    .map_err(|err| {
        user_error_with_message("Cannot resolve retained synchronization history", err)
    })?;
    let retained = plan.build_index(repo).await.map_err(|err| {
        user_error_with_message("Cannot index retained synchronization history", err)
    })?;
    let git_backend = jj_lib::git::get_git_backend(repo.store())?;
    // Old conflict terms may exist only in jj metadata, without physical Git
    // commit-tree edges. Refresh their independent Git roots before pruning.
    git_backend.replace_sync_tree_roots(retained.tree_roots.iter())?;
    let raw_roots = plan.raw_operations.into_iter().collect::<Vec<_>>();
    repo.op_store().gc(&raw_roots, keep_newer).await?;
    git_backend.gc(retained.index.as_index(), keep_newer)?;
    Ok(())
}
