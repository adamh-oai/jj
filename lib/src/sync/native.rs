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

//! Synchronization using ordinary operation ancestry and projected view merges.
//!
//! Published integrations retain their local baseline and incoming history as
//! ordinary operation parents. Their marker is only a convergence optimization:
//! retention never follows attributes or a second causal graph.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::LocalBoxFuture;

use super::SyncError;
use super::SyncResult;
use super::shared::SharedView;
use crate::backend::CommitId;
use crate::content_hash::blake2b_hash;
use crate::hex_util::encode_hex;
use crate::object_id::ObjectId as _;
use crate::op_store;
use crate::op_store::OpStore;
use crate::op_store::OperationId;
use crate::op_walk;
use crate::repo::MutableRepo;
use crate::repo::ReadonlyRepo;
use crate::repo::Repo as _;
use crate::transaction::Transaction;
use crate::view::View;

/// The receiver's operation before applying a synchronization result.
pub const LOCAL_PARENT_ATTRIBUTE: &str = "jj.sync.local-parent";
/// Digest of an integration's parents and intended shared result.
pub const MERGE_VIEW_ATTRIBUTE: &str = "jj.sync.merge-view";

/// Computes the marker checked before treating an integration as transparent.
///
/// Including parents makes native history rewriting an authored event, even if
/// it leaves the view unchanged. Workspace and physical Git state are omitted.
pub fn merge_view_digest(parents: &[OperationId], view: &op_store::View) -> String {
    let mut parents = parents.to_vec();
    parents.sort_unstable();
    parents.dedup();
    let shared = SharedView::from_view(view);
    encode_hex(&blake2b_hash(&(
        "jj-sync-native-integration-v1".to_owned(),
        parents,
        shared,
    )))
}

/// Marks a prepared integration before ordinary workspace finalization.
///
/// Finalization that changes shared state invalidates the digest, so the
/// resulting operation remains a real authored event in future exchanges.
/// The marker is only a hint: `frontier()` separately proves that an actual
/// parent already contains the whole history and the same shared view.
pub fn mark_integration(tx: &mut Transaction) {
    let parents = tx
        .parent_ops()
        .iter()
        .map(|op| op.id().clone())
        .collect::<Vec<_>>();
    let digest = merge_view_digest(&parents, tx.repo().view().store_view());
    tx.set_attribute(LOCAL_PARENT_ATTRIBUTE.to_owned(), parents[0].hex());
    tx.set_attribute(MERGE_VIEW_ATTRIBUTE.to_owned(), digest);
}

/// Checks ordinary operation ancestry, including the operation itself.
pub async fn contains(
    operations: &dyn OpStore,
    ancestor: &OperationId,
    descendant: &OperationId,
) -> SyncResult<bool> {
    let mut pending = vec![descendant.clone()];
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if &id == ancestor {
            return Ok(true);
        }
        if visited.insert(id.clone()) {
            let operation = operations.read_operation(&id).await?;
            // Check all immediate edges before descending into another branch.
            // Wrapper proofs commonly ask about a direct merge parent.
            if operation.parents.contains(ancestor) {
                return Ok(true);
            }
            pending.extend(operation.parents);
        }
    }
    Ok(false)
}

/// Returns the authored frontier represented by ordinary operation heads.
///
/// A pure integration contributes its parents' frontiers instead of its own ID.
/// This lets independently published integrations recognize the same exchange
/// despite different receiver-local workspace state. Ordinary operations are
/// never discarded merely because their projected views happen to be equal.
/// A peer-provided digest alone is not proof: a transparent operation must copy
/// its shared view from a parent that already includes every other parent's
/// verified authored history.
pub async fn frontier(
    operations: &dyn OpStore,
    heads: &[OperationId],
) -> SyncResult<Vec<OperationId>> {
    let mut walker = FrontierWalker::new(operations);
    let mut authored = BTreeSet::new();
    for head in heads {
        authored.extend(walker.normalize(head.clone()).await?);
    }
    let mut result = Vec::new();
    for head in &authored {
        let mut ancestor = false;
        for other in &authored {
            if head != other && walker.contains(head, other).await? {
                ancestor = true;
                break;
            }
        }
        if !ancestor {
            result.push(head.clone());
        }
    }
    Ok(result)
}

/// Immutable records and proven reductions are reused within one frontier walk.
struct FrontierWalker<'a> {
    operations: &'a dyn OpStore,
    records: BTreeMap<OperationId, op_store::Operation>,
    views: BTreeMap<op_store::ViewId, op_store::View>,
    normalized: BTreeMap<OperationId, Vec<OperationId>>,
    visiting: BTreeSet<OperationId>,
}

impl<'a> FrontierWalker<'a> {
    fn new(operations: &'a dyn OpStore) -> Self {
        Self {
            operations,
            records: BTreeMap::new(),
            views: BTreeMap::new(),
            normalized: BTreeMap::new(),
            visiting: BTreeSet::new(),
        }
    }

    async fn read_operation(&mut self, id: &OperationId) -> SyncResult<op_store::Operation> {
        if let Some(operation) = self.records.get(id) {
            return Ok(operation.clone());
        }
        let operation = self.operations.read_operation(id).await?;
        self.records.insert(id.clone(), operation.clone());
        Ok(operation)
    }

    async fn read_view(&mut self, id: &op_store::ViewId) -> SyncResult<op_store::View> {
        if let Some(view) = self.views.get(id) {
            return Ok(view.clone());
        }
        let view = self.operations.read_view(id).await?;
        self.views.insert(id.clone(), view.clone());
        Ok(view)
    }

    async fn contains(
        &mut self,
        ancestor: &OperationId,
        descendant: &OperationId,
    ) -> SyncResult<bool> {
        let mut pending = vec![descendant.clone()];
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if &id == ancestor {
                return Ok(true);
            }
            if visited.insert(id.clone()) {
                let operation = self.read_operation(&id).await?;
                if operation.parents.contains(ancestor) {
                    return Ok(true);
                }
                pending.extend(operation.parents);
            }
        }
        Ok(false)
    }

    fn normalize(&mut self, id: OperationId) -> LocalBoxFuture<'_, SyncResult<Vec<OperationId>>> {
        async move {
            if let Some(frontier) = self.normalized.get(&id) {
                return Ok(frontier.clone());
            }
            if !self.visiting.insert(id.clone()) {
                return Err(invalid("cycle in native integration history"));
            }
            let operation = self.read_operation(&id).await?;
            let attributes = &operation.metadata.attributes;
            let local_parent = attributes
                .get(LOCAL_PARENT_ATTRIBUTE)
                .and_then(OperationId::try_from_hex);
            let copied_parent = if operation.parents.len() >= 2
                && local_parent
                    .as_ref()
                    .is_some_and(|local| operation.parents.contains(local))
            {
                if let Some(expected) = attributes.get(MERGE_VIEW_ATTRIBUTE) {
                    let view = self.read_view(&operation.view_id).await?;
                    if *expected == merge_view_digest(&operation.parents, &view) {
                        self.parent_copy(&operation.parents, &view).await?
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            let result = if let Some(parent) = copied_parent {
                self.normalize(parent).await?
            } else {
                vec![id.clone()]
            };
            self.visiting.remove(&id);
            self.normalized.insert(id, result.clone());
            Ok(result)
        }
        .boxed_local()
    }

    async fn parent_copy(
        &mut self,
        parents: &[OperationId],
        view: &op_store::View,
    ) -> SyncResult<Option<OperationId>> {
        let shared = SharedView::from_view(view);
        for candidate in parents {
            let parent = self.read_operation(candidate).await?;
            let parent_view = self.read_view(&parent.view_id).await?;
            if SharedView::from_view(&parent_view) != shared {
                continue;
            }
            let mut dominates = true;
            for other in parents {
                if self.contains(other, candidate).await? {
                    continue;
                }
                // Another machine's local wrapper need not be a raw ancestor
                // of the selected result. Its verified authored frontier is
                // the history that must already be represented instead.
                for ancestor in self.normalize(other.clone()).await? {
                    if !self.contains(&ancestor, candidate).await? {
                        dominates = false;
                        break;
                    }
                }
                if !dominates {
                    break;
                }
            }
            if dominates {
                return Ok(Some(candidate.clone()));
            }
        }
        Ok(None)
    }
}

async fn ordinary_heads(
    operations: &dyn OpStore,
    mut heads: Vec<OperationId>,
) -> SyncResult<Vec<OperationId>> {
    heads.sort_unstable();
    heads.dedup();
    let mut result = Vec::new();
    for head in &heads {
        let mut ancestor = false;
        for other in &heads {
            if head != other && contains(operations, head, other).await? {
                ancestor = true;
                break;
            }
        }
        if !ancestor {
            result.push(head.clone());
        }
    }
    Ok(result)
}

async fn covers(
    operations: &dyn OpStore,
    represented: &[OperationId],
    incoming: &[OperationId],
) -> SyncResult<bool> {
    for ancestor in incoming {
        let mut found = false;
        for descendant in represented {
            if contains(operations, ancestor, descendant).await? {
                found = true;
                break;
            }
        }
        if !found {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Computes one immutable, unpublished ordinary operation for a shared merge.
///
/// The caller pins the result before releasing its GC lock, then transfers it
/// and any commits created while rebasing. Both endpoints apply this same result
/// rather than independently creating potentially different rewritten commits.
pub async fn reconcile(repo: &Arc<ReadonlyRepo>, heads: &[OperationId]) -> SyncResult<OperationId> {
    if heads.is_empty() {
        return Err(invalid("cannot reconcile an empty operation frontier"));
    }
    let operations = repo.op_store().as_ref();
    let union = frontier(operations, heads).await?;
    for head in heads {
        let represented = frontier(operations, std::slice::from_ref(head)).await?;
        if covers(operations, &represented, &union).await? {
            return Ok(if let [authored] = represented.as_slice() {
                authored.clone()
            } else {
                head.clone()
            });
        }
    }
    // Synthetic receiver wrappers are local receipts. Merge the authored
    // frontier so equivalent exchanges do not need to send those receipts.
    let heads = union;
    let mut merger = ViewMerger::new(repo);
    let target = merger.merge(&heads).await?;
    let first = repo.loader().load_operation(&heads[0]).await?;
    let baseline = repo.loader().load_at(&first).await?;
    let mut tx = baseline.start_transaction();
    for head in &heads[1..] {
        let operation = repo.loader().load_operation(head).await?;
        let parent = repo.loader().load_at(&operation).await?;
        tx.repo_mut().merge_index(&parent)?;
        tx.add_operation_parent(operation);
    }
    // Recursive reconciliation can create rebased commits that occur in none
    // of the parent indexes. Index those results before writing the operation.
    index_views(tx.repo_mut(), &[&target]).await?;
    merger.record_predecessors(&mut tx).await?;
    tx.repo_mut().set_view(target.as_view().clone());
    normalize_view(&mut tx).await?;
    // The selected shared merge is a real authored operation. Only receiver
    // wrappers that provably copy a dominating parent's view are transparent.
    if !merger.generated_effects {
        // A merge that creates no commits has a deterministic shared result.
        // Give its internal operation deterministic metadata too, so concurrent
        // sessions selecting that same result produce the same operation ID.
        let mut metadata = operations
            .read_operation(operations.root_operation_id())
            .await?
            .metadata;
        let timestamp = tx
            .parent_ops()
            .iter()
            .map(|operation| operation.metadata().time.end)
            .max()
            .unwrap();
        metadata.time.start = timestamp;
        metadata.time.end = timestamp;
        metadata.is_snapshot = false;
        metadata.workspace_name = None;
        metadata.attributes.clear();
        tx.set_metadata(metadata);
    }
    let selected = tx
        .write("merge synchronized operation histories")
        .await
        .map_err(|error| SyncError::InvalidData(format!("cannot write sync merge: {error}")))?
        .leave_unpublished();
    Ok(selected.op_id().clone())
}

/// Prepares a normal local integration, preserving the receiver's local maps.
///
/// Usually `incoming` contains the selected operation returned by `reconcile`.
/// A concurrent local operation is merged with that frozen result normally.
pub async fn prepare_integration(
    repo: &Arc<ReadonlyRepo>,
    incoming: &[OperationId],
) -> SyncResult<Option<Transaction>> {
    let operations = repo.op_store().as_ref();
    let local = frontier(operations, std::slice::from_ref(repo.op_id())).await?;
    let remote = frontier(operations, incoming).await?;
    if covers(operations, &local, &remote).await? {
        return Ok(None);
    }
    let mut heads = vec![repo.op_id().clone()];
    heads.extend_from_slice(incoming);
    let mut merger = ViewMerger::new(repo);
    let target = merger.merge(&heads).await?;
    let mut tx = repo.start_transaction();
    tx.set_attribute(LOCAL_PARENT_ATTRIBUTE.to_owned(), repo.op_id().hex());
    for head in incoming {
        if tx.parent_ops().iter().any(|parent| parent.id() == head) {
            continue;
        }
        let operation = repo.loader().load_operation(head).await?;
        let parent = repo.loader().load_at(&operation).await?;
        tx.repo_mut().merge_index(&parent)?;
        tx.add_operation_parent(operation);
    }
    let local_view = SharedView::from_view(repo.view().store_view());
    index_views(tx.repo_mut(), &[&local_view, &target]).await?;
    merger.record_predecessors(&mut tx).await?;
    tx.repo_mut()
        .merge_shared_view(local_view.as_view(), target.as_view())
        .await?;
    let generated = tx.repo_mut().rebase_descendants().await?;
    normalize_view(&mut tx).await?;
    if !merger.generated_effects && generated == 0 {
        mark_integration(&mut tx);
    }
    Ok(Some(tx))
}

async fn normalize_view(tx: &mut Transaction) -> SyncResult<()> {
    let mut view = tx.repo().view().clone();
    view.normalize_heads(tx.repo().index(), tx.repo().store().root_commit_id())
        .await?;
    tx.repo_mut().set_view(view.store_view().clone());
    Ok(())
}

/// Recursive native merge bases are cached only during this computation.
struct ViewMerger<'a> {
    repo: &'a Arc<ReadonlyRepo>,
    merged: BTreeMap<Vec<OperationId>, SharedView>,
    generated_effects: bool,
    predecessors: BTreeMap<CommitId, Vec<CommitId>>,
}

impl<'a> ViewMerger<'a> {
    fn new(repo: &'a Arc<ReadonlyRepo>) -> Self {
        Self {
            repo,
            merged: BTreeMap::new(),
            generated_effects: false,
            predecessors: BTreeMap::new(),
        }
    }

    async fn record_predecessors(&self, tx: &mut Transaction) -> SyncResult<()> {
        // Recursive merge bases may create commits that are referenced only by
        // predecessor edges. Preserve both that metadata and its commit index.
        let ids = self
            .predecessors
            .iter()
            .flat_map(|(id, predecessors)| std::iter::once(id).chain(predecessors))
            .collect::<BTreeSet<_>>();
        let mut commits = Vec::with_capacity(ids.len());
        for id in ids {
            commits.push(tx.repo().store().get_commit_async(id).await?);
        }
        tx.repo_mut().index_commits(&commits).await?;
        for (id, predecessors) in &self.predecessors {
            tx.repo_mut()
                .set_predecessors(id.clone(), predecessors.clone());
        }
        Ok(())
    }

    fn merge<'b>(
        &'b mut self,
        heads: &'b [OperationId],
    ) -> LocalBoxFuture<'b, SyncResult<SharedView>> {
        async move {
            let heads = ordinary_heads(self.repo.op_store().as_ref(), heads.to_vec()).await?;
            if let Some(view) = self.merged.get(&heads) {
                return Ok(view.clone());
            }
            // Use an already selected shared result exactly when its effective
            // history covers all requested heads. Receiver-local wrappers need
            // not be raw ancestors of each other to represent that same result.
            let union = frontier(self.repo.op_store().as_ref(), &heads).await?;
            for head in &heads {
                let represented =
                    frontier(self.repo.op_store().as_ref(), std::slice::from_ref(head)).await?;
                if covers(self.repo.op_store().as_ref(), &represented, &union).await? {
                    let operation = self.repo.loader().load_operation(head).await?;
                    let view = SharedView::from_view(operation.view().await?.store_view());
                    self.merged.insert(heads, view.clone());
                    return Ok(view);
                }
            }
            let Some(first) = heads.first() else {
                return Ok(SharedView::from_view(&op_store::View::make_root(
                    self.repo.store().root_commit_id().clone(),
                )));
            };
            let first = self.repo.loader().load_operation(first).await?;
            let mut result = SharedView::from_view(first.view().await?.store_view());
            let mut parents = vec![first];
            for head in &heads[1..] {
                let other = self.repo.loader().load_operation(head).await?;
                let bases =
                    op_walk::closest_common_ancestors(parents.clone(), [other.clone()]).await?;
                let base_ids = bases
                    .iter()
                    .map(|operation| operation.id().clone())
                    .collect::<Vec<_>>();
                let base = self.merge(&base_ids).await?;
                let right = SharedView::from_view(other.view().await?.store_view());
                let mut scratch = MutableRepo::new(
                    self.repo.clone(),
                    self.repo.readonly_index(),
                    self.repo.view(),
                );
                index_views(&mut scratch, &[&base, &result, &right]).await?;
                scratch.set_view(result.as_view().clone());
                scratch
                    .merge_shared_view(base.as_view(), right.as_view())
                    .await?;
                scratch.rebase_descendants().await?;
                let (_, view, predecessors) = scratch.consume().await?;
                self.generated_effects |= !predecessors.is_empty();
                self.predecessors.extend(predecessors);
                result = SharedView::from_view(view.store_view());
                parents.push(other);
            }
            self.merged.insert(heads, result.clone());
            Ok(result)
        }
        .boxed_local()
    }
}

async fn index_views(repo: &mut MutableRepo, views: &[&SharedView]) -> SyncResult<()> {
    let ids = views
        .iter()
        .flat_map(|view| {
            View::new(view.as_view().clone(), true)
                .all_referenced_commit_ids()
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect::<BTreeSet<_>>();
    let mut commits = Vec::with_capacity(ids.len());
    for id in ids {
        commits.push(repo.store().get_commit_async(&id).await?);
    }
    repo.index_commits(&commits).await?;
    Ok(())
}

fn invalid(message: &str) -> SyncError {
    SyncError::InvalidData(message.to_owned())
}

#[cfg(test)]
mod tests {
    use pollster::FutureExt as _;
    use tempfile::TempDir;

    use super::*;
    use crate::op_store::RootOperationData;
    use crate::ref_name::WorkspaceName;
    use crate::simple_op_store::SimpleOpStore;

    struct Fixture {
        _directory: TempDir,
        operations: SimpleOpStore,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("op_store");
            std::fs::create_dir(&path).unwrap();
            let operations = SimpleOpStore::init(
                &path,
                RootOperationData {
                    root_commit_id: CommitId::from_bytes(&[0; 20]),
                },
            )
            .unwrap();
            Self {
                _directory: directory,
                operations,
            }
        }

        fn root(&self) -> OperationId {
            self.operations.root_operation_id().clone()
        }

        fn root_view(&self) -> op_store::View {
            let operation = self
                .operations
                .read_operation(&self.root())
                .block_on()
                .unwrap();
            self.operations
                .read_view(&operation.view_id)
                .block_on()
                .unwrap()
        }

        fn write(
            &self,
            parents: &[OperationId],
            view: &op_store::View,
            description: &str,
            marked: bool,
        ) -> OperationId {
            let mut operation = self
                .operations
                .read_operation(&self.root())
                .block_on()
                .unwrap();
            operation.parents = parents.to_vec();
            operation.view_id = self.operations.write_view(view).block_on().unwrap();
            operation.metadata.description = description.to_owned();
            if marked {
                operation
                    .metadata
                    .attributes
                    .insert(LOCAL_PARENT_ATTRIBUTE.to_owned(), parents[0].hex());
                operation.metadata.attributes.insert(
                    MERGE_VIEW_ATTRIBUTE.to_owned(),
                    merge_view_digest(parents, view),
                );
            }
            self.operations
                .write_operation(&operation)
                .block_on()
                .unwrap()
        }

        fn frontier(&self, heads: &[OperationId]) -> Vec<OperationId> {
            frontier(&self.operations, heads).block_on().unwrap()
        }
    }

    #[test]
    fn independent_integrations_have_the_same_frontier() {
        let f = Fixture::new();
        let root = f.root();
        let view = f.root_view();
        let a = f.write(std::slice::from_ref(&root), &view, "authored A", false);
        let b = f.write(&[root], &view, "authored B", false);
        let selected = f.write(&[a.clone(), b.clone()], &view, "selected merge", false);
        let mut a_view = view.clone();
        a_view.wc_commit_ids.insert(
            WorkspaceName::new("default").to_owned(),
            CommitId::from_bytes(&[1; 20]),
        );
        let mut b_view = view;
        b_view.wc_commit_ids.insert(
            WorkspaceName::new("default").to_owned(),
            CommitId::from_bytes(&[2; 20]),
        );
        let a_sync = f.write(&[a.clone(), selected.clone()], &a_view, "receive B", true);
        let b_sync = f.write(&[b.clone(), selected.clone()], &b_view, "receive A", true);
        assert_ne!(a_sync, b_sync);
        assert_eq!(f.frontier(&[a_sync]), f.frontier(&[b_sync]));
        assert_eq!(f.frontier(std::slice::from_ref(&selected)), vec![selected]);
        assert_eq!(f.frontier(&[a, b]).len(), 2);
    }

    #[test]
    fn authored_empty_operations_and_undo_remain_causal_events() {
        let f = Fixture::new();
        let root = f.root();
        let view = f.root_view();
        let a = f.write(std::slice::from_ref(&root), &view, "A", false);
        let b = f.write(&[root], &view, "B", false);
        let selected = f.write(&[a.clone(), b], &view, "selected merge", false);
        let sync = f.write(&[a, selected], &view, "sync", true);
        let undo = f.write(&[sync], &view, "restore previous state", false);
        assert_eq!(f.frontier(std::slice::from_ref(&undo)), vec![undo]);
    }

    #[test]
    fn finalization_shared_effect_makes_integration_an_authored_event() {
        let f = Fixture::new();
        let root = f.root();
        let view = f.root_view();
        let a = f.write(std::slice::from_ref(&root), &view, "A", false);
        let b = f.write(&[root], &view, "B", false);
        let selected = f.write(&[a.clone(), b], &view, "selected merge", false);
        let sync = f.write(&[a, selected], &view, "sync", true);
        assert_ne!(f.frontier(std::slice::from_ref(&sync)), vec![sync.clone()]);
        let mut operation = f.operations.read_operation(&sync).block_on().unwrap();
        let mut changed = view;
        changed.head_ids.insert(CommitId::from_bytes(&[3; 20]));
        operation.view_id = f.operations.write_view(&changed).block_on().unwrap();
        let finalized = f.operations.write_operation(&operation).block_on().unwrap();
        assert_eq!(
            f.frontier(std::slice::from_ref(&finalized)),
            vec![finalized]
        );
    }

    #[test]
    fn pruning_any_parent_invalidates_integration_transparency() {
        let f = Fixture::new();
        let root = f.root();
        let view = f.root_view();
        let a = f.write(std::slice::from_ref(&root), &view, "A", false);
        let b = f.write(std::slice::from_ref(&root), &view, "B", false);
        let selected = f.write(&[a.clone(), b], &view, "selected merge", false);
        let sync = f.write(&[a, selected], &view, "sync", true);
        assert_ne!(f.frontier(std::slice::from_ref(&sync)), vec![sync.clone()]);
        let mut operation = f.operations.read_operation(&sync).block_on().unwrap();
        operation.parents[1] = root;
        let pruned = f.operations.write_operation(&operation).block_on().unwrap();
        assert_eq!(f.frontier(std::slice::from_ref(&pruned)), vec![pruned]);
    }

    #[test]
    fn authored_descendant_covers_the_integrated_frontier() {
        let f = Fixture::new();
        let root = f.root();
        let view = f.root_view();
        let a = f.write(std::slice::from_ref(&root), &view, "A", false);
        let b = f.write(&[root], &view, "B", false);
        let sync = f.write(&[a.clone(), b.clone()], &view, "sync", true);
        let authored = f.write(&[sync], &view, "new change", false);
        assert_eq!(f.frontier(&[a, b, authored.clone()]), vec![authored]);
    }

    #[test]
    fn forged_digest_cannot_hide_an_arbitrary_shared_view() {
        let f = Fixture::new();
        let root = f.root();
        let view = f.root_view();
        let a = f.write(std::slice::from_ref(&root), &view, "A", false);
        let b = f.write(&[root], &view, "B", false);
        let selected = f.write(&[a.clone(), b], &view, "selected merge", false);
        let mut forged_view = view;
        forged_view.head_ids.insert(CommitId::from_bytes(&[9; 20]));
        let forged = f.write(&[a, selected], &forged_view, "forged integration", true);
        assert_eq!(f.frontier(std::slice::from_ref(&forged)), vec![forged]);
    }

    #[test]
    fn matching_view_cannot_hide_a_concurrent_parent() {
        let f = Fixture::new();
        let root = f.root();
        let view = f.root_view();
        let a = f.write(std::slice::from_ref(&root), &view, "A", false);
        let b = f.write(&[root], &view, "B", false);
        let forged = f.write(&[a, b], &view, "concurrent parent omitted", true);
        assert_eq!(f.frontier(std::slice::from_ref(&forged)), vec![forged]);
    }

    #[test]
    fn three_peer_chain_recognizes_verified_foreign_wrappers() {
        let f = Fixture::new();
        let root = f.root();
        let view = f.root_view();
        let a = f.write(std::slice::from_ref(&root), &view, "A", false);
        let b = f.write(std::slice::from_ref(&root), &view, "B", false);
        let c = f.write(&[root], &view, "C", false);
        let ab = f.write(&[a.clone(), b.clone()], &view, "selected AB", false);
        let a1 = f.write(&[a, ab.clone()], &view, "A receives AB", true);
        let b1 = f.write(&[b, ab], &view, "B receives AB", true);
        let abc = f.write(&[b1, c.clone()], &view, "selected ABC", false);
        let c1 = f.write(&[c, abc.clone()], &view, "C receives ABC", true);
        assert!(!contains(&f.operations, &a1, &abc).block_on().unwrap());
        let a2 = f.write(&[a1, abc.clone()], &view, "A receives ABC", true);
        assert_eq!(f.frontier(&[a2, c1]), vec![abc]);
    }
}
