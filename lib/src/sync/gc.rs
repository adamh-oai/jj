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

//! Garbage-collection reachability across original and shared histories.
//!
//! Callers must hold the synchronization state lock from capture through both
//! operation and backend collection. Transfer/import must use the same lock
//! while establishing or changing durable roots. No network wait belongs under
//! this lock.

use std::collections::BTreeSet;

use thiserror::Error;

use super::SyncError;
use super::anchor::AnchorStore;
use super::normalize::INTEGRATION_ANCHOR_ATTRIBUTE;
use super::shared::SharedEventId;
use super::state::SyncState;
use crate::backend::BackendError;
use crate::backend::CommitId;
use crate::backend::TreeId;
use crate::commit::Commit;
use crate::index::IndexError;
use crate::index::MutableIndex;
use crate::object_id::ObjectId as _;
use crate::op_store::OpStore;
use crate::op_store::OpStoreError;
use crate::op_store::OperationId;
use crate::repo::ReadonlyRepo;
use crate::repo::Repo as _;
use crate::view::View;

/// An error aborting GC before the retained closure is known to be complete.
#[derive(Debug, Error)]
pub enum SyncGcError {
    /// An original operation or view could not be loaded.
    #[error(transparent)]
    Operation(#[from] OpStoreError),
    /// Shared history could not be verified.
    #[error(transparent)]
    Shared(#[from] SyncError),
    /// A retained commit could not be loaded.
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// A complete backend retention index could not be constructed.
    #[error(transparent)]
    Index(#[from] IndexError),
    /// A required cross-history reference is invalid.
    #[error("Invalid synchronization GC root: {0}")]
    Invalid(String),
}

/// Verified retained histories, independent of active repository visibility.
#[derive(Debug)]
pub struct GcPlan {
    /// Original operations reachable across all raw/shared metadata edges.
    pub raw_operations: BTreeSet<OperationId>,
    /// Shared events, including anchors reached only through integrations.
    pub shared_events: BTreeSet<SharedEventId>,
    /// All original/shared view and operation commit references.
    pub commit_roots: BTreeSet<CommitId>,
}

/// An in-memory backend retention index and explicitly referenced tree terms.
pub struct GcIndex {
    /// Covers all retained commits, ancestors, and legacy predecessors.
    pub index: Box<dyn MutableIndex>,
    /// Legacy conflict terms stored only in jj extras, outside Git ancestry.
    pub tree_roots: BTreeSet<TreeId>,
}

/// Resolves all cross-links without publishing foreign operation heads.
///
/// `active_heads` must include every currently active raw head, not only the
/// operation at which one workspace happens to be loaded. Missing required
/// records abort collection; they must not silently become unreachable data.
pub async fn collect_roots(
    op_store: &dyn OpStore,
    anchors: &AnchorStore,
    state: &SyncState,
    active_heads: &[OperationId],
) -> Result<GcPlan, SyncGcError> {
    if active_heads.is_empty() {
        return Err(SyncGcError::Invalid("no active operation heads".to_owned()));
    }
    let tracked = state.gc_roots();
    let mut raw_pending = active_heads.to_vec();
    for value in tracked.raw_operations {
        raw_pending.push(parse_operation_id(&value)?);
    }
    let mut shared_pending = tracked
        .shared_events
        .iter()
        .map(|value| parse_shared_id(value))
        .collect::<Result<Vec<_>, _>>()?;
    let mut plan = GcPlan {
        raw_operations: BTreeSet::new(),
        shared_events: BTreeSet::new(),
        commit_roots: BTreeSet::new(),
    };
    loop {
        while let Some(id) = raw_pending.pop() {
            if !plan.raw_operations.insert(id.clone()) {
                continue;
            }
            let operation = op_store.read_operation(&id).await?;
            let view = View::new(op_store.read_view(&operation.view_id).await?, true);
            plan.commit_roots
                .extend(view.all_referenced_commit_ids().cloned());
            if let Some(predecessors) = &operation.commit_predecessors {
                for (new_id, old_ids) in predecessors {
                    plan.commit_roots.insert(new_id.clone());
                    plan.commit_roots.extend(old_ids.iter().cloned());
                }
            }
            if let Some(value) = operation
                .metadata
                .attributes
                .get(INTEGRATION_ANCHOR_ATTRIBUTE)
            {
                shared_pending.push(parse_shared_id(value)?);
            }
            raw_pending.extend(operation.parents);
        }
        while let Some(id) = shared_pending.pop() {
            if !plan.shared_events.insert(id.clone()) {
                continue;
            }
            let event = anchors.read(&id)?;
            let view = View::new(event.view.as_view().clone(), true);
            plan.commit_roots
                .extend(view.all_referenced_commit_ids().cloned());
            shared_pending.extend(event.parents);
            raw_pending.extend(event.origin);
        }
        if raw_pending.is_empty() {
            break;
        }
    }
    Ok(plan)
}

impl GcPlan {
    /// Builds a private retention index without installing foreign live state.
    ///
    /// Reading existing indexed commits is still necessary: old backend extras
    /// may reference predecessor commits and trees absent from operation data.
    pub async fn build_index(&self, repo: &ReadonlyRepo) -> Result<GcIndex, SyncGcError> {
        enum Pending {
            Visit(CommitId),
            Finish(Commit),
        }
        let mut index = repo.readonly_index().start_modification();
        let mut completed = BTreeSet::new();
        let mut visiting = BTreeSet::new();
        let mut pending = self
            .commit_roots
            .iter()
            .cloned()
            .map(Pending::Visit)
            .collect::<Vec<_>>();
        while let Some(item) = pending.pop() {
            match item {
                Pending::Visit(id) => {
                    if completed.contains(&id) {
                        continue;
                    }
                    if !visiting.insert(id.clone()) {
                        return Err(SyncGcError::Invalid(
                            "cycle in retained commit history".to_owned(),
                        ));
                    }
                    let commit = repo.store().get_commit_async(&id).await?;
                    let dependencies = commit
                        .parent_ids()
                        .iter()
                        .chain(&commit.store_commit().predecessors)
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    pending.push(Pending::Finish(commit));
                    pending.extend(dependencies.into_iter().map(Pending::Visit));
                }
                Pending::Finish(commit) => {
                    if !index.as_index().has_id(commit.id()).await? {
                        index.add_commit(&commit).await?;
                    }
                    visiting.remove(commit.id());
                    completed.insert(commit.id().clone());
                }
            }
        }
        // Ordinary and modern conflict trees are already reachable from their
        // physical Git commits. Only older extras-only tree overrides need
        // separate refs; pinning every historical tree would duplicate the
        // complete commit graph in Git's ref namespace.
        #[cfg(feature = "git")]
        let tree_roots = if let Ok(backend) = crate::git::get_git_backend(repo.store()) {
            backend.sync_legacy_tree_roots(completed.iter())?
        } else {
            BTreeSet::new()
        };
        #[cfg(not(feature = "git"))]
        let tree_roots = BTreeSet::new();
        Ok(GcIndex { index, tree_roots })
    }
}

fn parse_operation_id(value: &str) -> Result<OperationId, SyncGcError> {
    OperationId::try_from_hex(value)
        .filter(|id| id.as_bytes().len() == 64)
        .ok_or_else(|| SyncGcError::Invalid("invalid original operation ID".to_owned()))
}

fn parse_shared_id(value: &str) -> Result<SharedEventId, SyncGcError> {
    SharedEventId::try_from_hex(value)
        .filter(|id| id.as_bytes().len() == 64)
        .ok_or_else(|| SyncGcError::Invalid("invalid shared event ID".to_owned()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::time::SystemTime;

    use pollster::FutureExt as _;

    use super::*;
    use crate::op_store;
    use crate::op_store::RootOperationData;
    use crate::simple_op_store::SimpleOpStore;
    use crate::sync::shared::SharedEvent;
    use crate::sync::shared::SharedView;
    use crate::sync::state::PeerState;
    use crate::tests::TestResult;
    use crate::tests::new_temp_dir;

    fn write_operation(
        store: &SimpleOpStore,
        description: &str,
        attributes: BTreeMap<String, String>,
    ) -> OperationId {
        let mut operation = store
            .read_operation(store.root_operation_id())
            .block_on()
            .unwrap();
        operation.parents = vec![store.root_operation_id().clone()];
        operation.metadata.description = description.to_owned();
        operation.metadata.attributes = attributes;
        store.write_operation(&operation).block_on().unwrap()
    }

    #[test]
    fn traverse_integration_anchor_and_shared_origin_before_pruning() -> TestResult {
        let directory = new_temp_dir();
        let op_path = directory.path().join("op_store");
        fs::create_dir(&op_path)?;
        let root_commit = CommitId::from_bytes(&[0; 20]);
        let store = SimpleOpStore::init(
            &op_path,
            RootOperationData {
                root_commit_id: root_commit.clone(),
            },
        )?;
        let anchors = AnchorStore::init(directory.path())?;
        let peer = write_operation(&store, "foreign original operation", BTreeMap::new());
        let orphan = write_operation(&store, "unreferenced operation", BTreeMap::new());
        let shared_only_commit = CommitId::from_bytes(&[1; 20]);
        let mut view = op_store::View::make_root(root_commit);
        view.head_ids.insert(shared_only_commit.clone());
        let shared = anchors.write(&SharedEvent::new(
            vec![],
            SharedView::from_view(&view),
            Some(peer.clone()),
        ))?;
        let active = write_operation(
            &store,
            "local integration",
            BTreeMap::from([(INTEGRATION_ANCHOR_ATTRIBUTE.to_owned(), shared.hex())]),
        );
        let plan = collect_roots(
            &store,
            &anchors,
            &SyncState::default(),
            std::slice::from_ref(&active),
        )
        .block_on()?;
        assert!(plan.raw_operations.contains(&peer));
        assert!(plan.raw_operations.contains(&active));
        assert!(plan.shared_events.contains(&shared));
        assert!(plan.commit_roots.contains(&shared_only_commit));
        assert!(!plan.raw_operations.contains(&orphan));
        store
            .gc(
                &plan.raw_operations.into_iter().collect::<Vec<_>>(),
                SystemTime::now(),
            )
            .block_on()?;
        store.read_operation(&peer).block_on()?;
        assert!(store.read_operation(&orphan).block_on().is_err());
        Ok(())
    }

    #[test]
    fn retain_all_active_and_tracked_peer_heads() -> TestResult {
        let directory = new_temp_dir();
        let op_path = directory.path().join("op_store");
        fs::create_dir(&op_path)?;
        let store = SimpleOpStore::init(
            &op_path,
            RootOperationData {
                root_commit_id: CommitId::from_bytes(&[0; 20]),
            },
        )?;
        let anchors = AnchorStore::init(directory.path())?;
        let first = write_operation(&store, "first concurrent head", BTreeMap::new());
        let second = write_operation(&store, "second concurrent head", BTreeMap::new());
        let peer = write_operation(&store, "peer-only retained history", BTreeMap::new());
        let mut state = SyncState::default();
        state.peers.insert(
            "peer".to_owned(),
            PeerState {
                raw_heads: BTreeSet::from([peer.hex()]),
                shared_heads: BTreeSet::new(),
            },
        );
        let plan =
            collect_roots(&store, &anchors, &state, &[first.clone(), second.clone()]).block_on()?;
        assert!(
            plan.raw_operations
                .is_superset(&BTreeSet::from([first, second, peer]))
        );
        Ok(())
    }

    #[test]
    fn missing_cross_history_dependency_aborts_collection() -> TestResult {
        let directory = new_temp_dir();
        let op_path = directory.path().join("op_store");
        fs::create_dir(&op_path)?;
        let store = SimpleOpStore::init(
            &op_path,
            RootOperationData {
                root_commit_id: CommitId::from_bytes(&[0; 20]),
            },
        )?;
        let anchors = AnchorStore::init(directory.path())?;
        let missing_anchor = SharedEventId::from_bytes(&[1; 64]);
        let active = write_operation(
            &store,
            "integration with missing anchor",
            BTreeMap::from([(
                INTEGRATION_ANCHOR_ATTRIBUTE.to_owned(),
                missing_anchor.hex(),
            )]),
        );
        assert!(
            collect_roots(
                &store,
                &anchors,
                &SyncState::default(),
                std::slice::from_ref(&active)
            )
            .block_on()
            .is_err()
        );
        store.read_operation(&active).block_on()?;
        Ok(())
    }

    #[test]
    fn build_index_retains_peer_only_and_shared_only_commits() -> TestResult {
        use crate::config::StackedConfig;
        use crate::settings::UserSettings;
        use crate::signing_factory::signer_from_settings;
        use crate::simple_backend::SimpleBackend;

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
        let mut peer_transaction = repo.start_transaction();
        let peer_commit = peer_transaction
            .repo_mut()
            .new_commit(
                vec![repo.store().root_commit_id().clone()],
                repo.store().root_commit().tree(),
            )
            .set_description("retained only by peer operation")
            .write()
            .block_on()?;
        let peer = peer_transaction
            .write("unpublished peer operation")
            .block_on()?
            .leave_unpublished();
        let mut shared_transaction = repo.start_transaction();
        let shared_commit = shared_transaction
            .repo_mut()
            .new_commit(vec![peer_commit.id().clone()], peer_commit.tree())
            .set_description("retained only by shared reconciliation")
            .write()
            .block_on()?;
        let anchors = AnchorStore::init(directory.path())?;
        let mut shared_view = peer.view().store_view().clone();
        shared_view.head_ids.insert(shared_commit.id().clone());
        let shared = anchors.write(&SharedEvent::new(
            vec![],
            SharedView::from_view(&shared_view),
            Some(peer.op_id().clone()),
        ))?;
        let mut state = SyncState::default();
        state.shared_anchors.insert(shared.hex());
        let plan = collect_roots(
            repo.op_store().as_ref(),
            &anchors,
            &state,
            &[repo.op_id().clone()],
        )
        .block_on()?;
        let retained = plan.build_index(&repo).block_on()?;
        assert!(!repo.index().has_id(peer_commit.id()).block_on()?);
        assert!(!repo.index().has_id(shared_commit.id()).block_on()?);
        assert!(
            retained
                .index
                .as_index()
                .has_id(peer_commit.id())
                .block_on()?
        );
        assert!(
            retained
                .index
                .as_index()
                .has_id(shared_commit.id())
                .block_on()?
        );
        assert!(retained.tree_roots.is_empty());
        // GC never makes foreign work visible or publishes foreign op heads.
        assert_eq!(
            repo.loader().op_heads_store().get_op_heads().block_on()?,
            vec![repo.op_id().clone()]
        );
        Ok(())
    }
}
