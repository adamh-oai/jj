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

//! Conversion of deployed shared-event retention into native operation ancestry.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::SyncError;
use super::SyncResult;
use super::anchor::AnchorStore;
use super::gc::collect_roots;
use super::native::LOCAL_PARENT_ATTRIBUTE;
use super::shared::SharedEvent;
use super::shared::SharedEventId;
use super::state::SyncState;
use crate::object_id::ObjectId as _;
use crate::op_store;
use crate::op_store::OperationId;
use crate::operation::Operation;
use crate::repo::ReadonlyRepo;
use crate::repo::Repo as _;
use crate::transaction::Transaction;

/// Prepares a view-preserving native join of all retained legacy history.
///
/// The caller holds the synchronization state lock through preparation and
/// publication, then calls `SyncState::finish_native_migration()` and saves the
/// new state before replacing the operation-store marker. Original operations
/// and views keep their bytes and identities. Shared-only event views become
/// ordinary operations so their contents remain reachable without sidecar roots.
/// Unpublished converted records are harmless if preparation is interrupted.
pub async fn prepare_legacy_migration(
    repo: &Arc<ReadonlyRepo>,
    state: &SyncState,
    anchors: &AnchorStore,
) -> SyncResult<Transaction> {
    if state.is_native() {
        return Err(invalid("synchronization history is already native"));
    }
    let mut active = repo
        .loader()
        .op_heads_store()
        .get_op_heads()
        .await
        .map_err(|err| invalid(format!("cannot read operation heads for migration: {err}")))?;
    active.push(repo.op_id().clone());
    let plan = collect_roots(repo.op_store().as_ref(), anchors, state, &active)
        .await
        .map_err(|err| {
            invalid(format!(
                "cannot resolve legacy history for migration: {err}"
            ))
        })?;

    enum Pending {
        Visit(SharedEventId),
        Finish(SharedEventId, Box<SharedEvent>),
    }
    let mut converted: BTreeMap<SharedEventId, OperationId> = BTreeMap::new();
    let mut visiting = BTreeSet::new();
    let mut pending = plan
        .shared_events
        .iter()
        .cloned()
        .map(Pending::Visit)
        .collect::<Vec<_>>();
    let root_metadata = repo
        .op_store()
        .read_operation(repo.op_store().root_operation_id())
        .await?
        .metadata;
    let mut native_operations = BTreeMap::new();
    for id in &plan.raw_operations {
        native_operations.insert(id.clone(), repo.op_store().read_operation(id).await?);
    }
    while let Some(item) = pending.pop() {
        match item {
            Pending::Visit(id) => {
                if converted.contains_key(&id) {
                    continue;
                }
                if !visiting.insert(id.clone()) {
                    return Err(invalid("cycle in legacy synchronization events"));
                }
                let event = anchors.read(&id)?;
                let parents = event.parents.clone();
                pending.push(Pending::Finish(id, Box::new(event)));
                pending.extend(parents.into_iter().rev().map(Pending::Visit));
            }
            Pending::Finish(id, event) => {
                let mut parents = event
                    .parents
                    .iter()
                    .map(|parent| converted[parent].clone())
                    .chain(event.origin)
                    .collect::<BTreeSet<_>>();
                if parents.is_empty() {
                    parents.insert(repo.op_store().root_operation_id().clone());
                }
                let mut metadata = root_metadata.clone();
                metadata.description = "migrate legacy synchronization event".to_owned();
                metadata
                    .attributes
                    .insert("jj.sync.migrated-event".to_owned(), id.hex());
                let operation = op_store::Operation {
                    view_id: repo.op_store().write_view(event.view.as_view()).await?,
                    parents: parents.into_iter().collect(),
                    metadata,
                    commit_predecessors: Some(Default::default()),
                };
                let native_id = repo.op_store().write_operation(&operation).await?;
                native_operations.insert(native_id.clone(), operation);
                converted.insert(id.clone(), native_id);
                visiting.remove(&id);
            }
        }
    }

    // Retain only the native frontier, rather than putting every historical
    // operation in one very large parent list. Keep the local baseline first
    // even if another retained frontier also descends from it.
    let mut frontier = native_operations.keys().cloned().collect::<BTreeSet<_>>();
    for operation in native_operations.values() {
        for parent in &operation.parents {
            frontier.remove(parent);
        }
    }
    frontier.remove(repo.op_id());
    let mut tx = repo.start_transaction();
    tx.set_attribute(LOCAL_PARENT_ATTRIBUTE.to_owned(), repo.op_id().hex());
    tx.set_attribute("jj.sync.migrated-history".to_owned(), "1".to_owned());
    for id in frontier {
        let data = native_operations.remove(&id).unwrap();
        tx.add_operation_parent(Operation::new(repo.op_store().clone(), id, data));
    }

    // Include every historical reference in the ordinary index before the
    // final join is published. Legacy backend predecessor metadata can retain
    // additional commits that are not physical Git parents.
    let mut pending = plan.commit_roots.into_iter().collect::<Vec<_>>();
    let mut commits = Vec::new();
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        let commit = repo.store().get_commit_async(&id).await?;
        pending.extend(commit.store_commit().predecessors.iter().cloned());
        pending.extend(commit.parent_ids().iter().cloned());
        commits.push(commit);
    }
    tx.repo_mut().index_commits(&commits).await?;
    Ok(tx)
}

fn invalid(message: impl Into<String>) -> SyncError {
    SyncError::InvalidData(message.into())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::SystemTime;

    use pollster::FutureExt as _;

    use super::*;
    use crate::config::StackedConfig;
    use crate::settings::UserSettings;
    use crate::signing_factory::signer_from_settings;
    use crate::simple_backend::SimpleBackend;
    use crate::simple_op_store::SimpleOpStore;
    use crate::sync::shared::SharedView;
    use crate::tests::TestResult;
    use crate::tests::new_temp_dir;

    #[test]
    fn migration_keeps_raw_identity_current_view_and_shared_only_history() -> TestResult {
        let directory = new_temp_dir();
        let settings =
            UserSettings::from_config_and_home_dir(StackedConfig::with_defaults(), None)?;
        let repo = ReadonlyRepo::init(
            &settings,
            directory.path(),
            &|_settings, path| Ok(Box::new(SimpleBackend::init(path))),
            signer_from_settings(&settings)?,
            ReadonlyRepo::default_workspace_store_initializer(),
            ReadonlyRepo::default_op_store_initializer(),
            ReadonlyRepo::default_op_heads_store_initializer(),
            ReadonlyRepo::default_index_store_initializer(),
            ReadonlyRepo::default_submodule_store_initializer(),
        )
        .block_on()?;
        let local = repo
            .start_transaction()
            .commit("local state before migration")
            .block_on()?;
        let mut peer_tx = repo.start_transaction();
        let peer_commit = peer_tx
            .repo_mut()
            .new_commit(
                vec![repo.store().root_commit_id().clone()],
                repo.store().root_commit().tree(),
            )
            .set_description("retained only by original peer operation")
            .write()
            .block_on()?;
        let peer = peer_tx
            .write("original peer operation")
            .block_on()?
            .leave_unpublished();
        let raw_store = repo.op_store().downcast_ref::<SimpleOpStore>().unwrap();
        let original_bytes = raw_store.export_operation(peer.op_id())?;
        let original_view_id = &peer.operation().store_operation().view_id;
        let original_view_bytes = raw_store.export_view(original_view_id)?;
        let mut event_tx = repo.start_transaction();
        let shared_only = event_tx
            .repo_mut()
            .new_commit(vec![peer_commit.id().clone()], peer_commit.tree())
            .set_description("retained only by shared event view")
            .write()
            .block_on()?;
        let anchors = AnchorStore::init(directory.path())?;
        let source = anchors.write(&SharedEvent::new(
            vec![],
            SharedView::from_view(peer.view().store_view()),
            Some(peer.op_id().clone()),
        ))?;
        let mut event_view = peer.view().store_view().clone();
        event_view.head_ids.insert(shared_only.id().clone());
        let event = anchors.write(&SharedEvent::new(
            vec![source],
            SharedView::from_view(&event_view),
            None,
        ))?;
        let mut state = SyncState::default();
        state.shared_anchors.insert(event.hex());
        let first = prepare_legacy_migration(&local, &state, &anchors)
            .block_on()?
            .write("migrate synchronization history")
            .block_on()?
            .leave_unpublished();
        let second = prepare_legacy_migration(&local, &state, &anchors)
            .block_on()?
            .write("migrate synchronization history")
            .block_on()?
            .publish()
            .block_on()?;
        // Converted event operations have deterministic identities across a
        // retry, even though the final locally authored operation can differ.
        assert_eq!(
            first.operation().parent_ids(),
            second.operation().parent_ids()
        );
        assert_eq!(second.view().store_view(), local.view().store_view());
        assert_eq!(
            second.operation().metadata().attributes[LOCAL_PARENT_ATTRIBUTE],
            local.op_id().hex()
        );
        assert!(!second.view().heads().contains(peer_commit.id()));
        assert!(second.index().has_id(peer_commit.id()).block_on()?);
        assert!(second.index().has_id(shared_only.id()).block_on()?);
        state.finish_native_migration();
        fs::remove_dir_all(directory.path().join("sync/events"))?;
        let plan = collect_roots(
            second.op_store().as_ref(),
            &anchors,
            &state,
            std::slice::from_ref(second.op_id()),
        )
        .block_on()?;
        assert!(plan.raw_operations.contains(peer.op_id()));
        assert!(plan.shared_events.is_empty());
        assert!(plan.commit_roots.contains(shared_only.id()));
        // The normal operation-store collector can now retain all imported
        // operations using the single published native head.
        second
            .op_store()
            .gc(std::slice::from_ref(second.op_id()), SystemTime::now())
            .block_on()?;
        assert_eq!(raw_store.export_operation(peer.op_id())?, original_bytes);
        assert_eq!(
            raw_store.export_view(original_view_id)?,
            original_view_bytes
        );
        Ok(())
    }
}
