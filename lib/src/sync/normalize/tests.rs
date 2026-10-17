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

use pollster::FutureExt as _;
use tempfile::TempDir;

use super::*;
use crate::backend::CommitId;
use crate::op_store::RefTarget;
use crate::op_store::RootOperationData;
use crate::op_store::View;
use crate::ref_name::GitRefName;
use crate::ref_name::RefName;
use crate::ref_name::WorkspaceName;
use crate::simple_op_store::SimpleOpStore;

struct Fixture {
    _directory: TempDir,
    operations: SimpleOpStore,
    anchors: AnchorStore,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let op_path = directory.path().join("op_store");
        std::fs::create_dir(&op_path).unwrap();
        let operations = SimpleOpStore::init(
            &op_path,
            RootOperationData {
                root_commit_id: CommitId::from_bytes(&[0; 20]),
            },
        )
        .unwrap();
        let anchors = AnchorStore::init(directory.path()).unwrap();
        Self {
            _directory: directory,
            operations,
            anchors,
        }
    }

    fn root(&self) -> OperationId {
        self.operations.root_operation_id().clone()
    }

    fn view(&self, operation: &OperationId) -> View {
        let data = self
            .operations
            .read_operation(operation)
            .block_on()
            .unwrap();
        self.operations.read_view(&data.view_id).block_on().unwrap()
    }

    fn operation(
        &self,
        parents: &[OperationId],
        view: &View,
        description: &str,
        integration: Option<&SharedEventId>,
    ) -> OperationId {
        let mut operation = self
            .operations
            .read_operation(&self.root())
            .block_on()
            .unwrap();
        operation.parents = parents.to_vec();
        operation.view_id = self.operations.write_view(view).block_on().unwrap();
        operation.metadata.description = description.to_owned();
        operation.commit_predecessors = Some(BTreeMap::new());
        if let Some(anchor) = integration {
            operation
                .metadata
                .attributes
                .insert(INTEGRATION_ANCHOR_ATTRIBUTE.to_owned(), anchor.hex());
        }
        self.operations
            .write_operation(&operation)
            .block_on()
            .unwrap()
    }

    fn normalize(&self, id: &OperationId) -> SharedEventId {
        Normalizer::new(&self.operations, &self.anchors)
            .normalize(id)
            .block_on()
            .unwrap()
    }

    fn join(&self, parents: Vec<SharedEventId>, view: &View) -> SharedEventId {
        self.anchors
            .write(&SharedEvent::new(
                parents,
                SharedView::from_view(view),
                None,
            ))
            .unwrap()
    }
}

#[test]
fn independent_histories_and_three_peer_causality() {
    let f = Fixture::new();
    let root = f.root();
    let view = f.view(&root);
    let a = f.operation(std::slice::from_ref(&root), &view, "independent A", None);
    let b = f.operation(std::slice::from_ref(&root), &view, "independent B", None);
    let c = f.operation(&[root], &view, "independent C", None);
    let (a_shared, b_shared, c_shared) = (f.normalize(&a), f.normalize(&b), f.normalize(&c));
    assert_ne!(a_shared, b_shared);
    let ab = f.join(vec![b_shared.clone(), a_shared.clone()], &view);
    let ia = f.operation(&[a], &view, "sync A", Some(&ab));
    let ib = f.operation(&[b], &view, "sync B", Some(&ab));
    assert_eq!(f.normalize(&ia), ab);
    assert_eq!(f.normalize(&ib), ab);
    let abc = f.join(vec![ab.clone(), c_shared.clone()], &view);
    let ic = f.operation(&[c], &view, "sync C", Some(&abc));
    assert_eq!(f.normalize(&ic), abc);
    assert_eq!(
        f.anchors.heads(&[ab, abc.clone(), c_shared]).unwrap(),
        vec![abc.clone()]
    );
    assert!(f.anchors.is_ancestor(&a_shared, &abc).unwrap());
    assert!(f.anchors.is_ancestor(&b_shared, &abc).unwrap());
}

#[test]
fn undo_and_redo_are_new_events_even_when_views_match() {
    let f = Fixture::new();
    let root = f.root();
    let original = f.view(&root);
    let a = f.operation(std::slice::from_ref(&root), &original, "A", None);
    let mut changed = original.clone();
    changed.local_bookmarks.insert(
        RefName::new("work").to_owned(),
        RefTarget::normal(CommitId::from_bytes(&[1; 20])),
    );
    let b = f.operation(&[root], &changed, "B", None);
    let a_shared = f.normalize(&a);
    let b_shared = f.normalize(&b);
    let shared = f.join(vec![a_shared, b_shared], &changed);
    let integration = f.operation(&[a], &changed, "sync", Some(&shared));
    assert_eq!(f.normalize(&integration), shared);
    let undo = f.operation(&[integration], &original, "undo: restore A", None);
    let undone = f.normalize(&undo);
    assert!(f.anchors.is_ancestor(&shared, &undone).unwrap());
    assert_eq!(
        f.anchors.heads(&[shared.clone(), undone.clone()]).unwrap(),
        vec![undone.clone()]
    );
    assert_eq!(
        f.anchors.read(&undone).unwrap().view,
        SharedView::from_view(&original)
    );
    let redo = f.operation(&[undo], &changed, "redo", None);
    let redone = f.normalize(&redo);
    assert_ne!(redone, shared);
    assert!(f.anchors.is_ancestor(&undone, &redone).unwrap());
}

#[test]
fn workspace_only_events_survive_without_sharing_local_maps() {
    let f = Fixture::new();
    let root = f.root();
    let original = f.view(&root);
    let mut local = original.clone();
    let commit = CommitId::from_bytes(&[2; 20]);
    local
        .wc_commit_ids
        .insert(WorkspaceName::DEFAULT.to_owned(), commit.clone());
    local.git_heads.insert(
        WorkspaceName::DEFAULT.to_owned(),
        RefTarget::normal(commit.clone()),
    );
    local.git_refs.insert(
        GitRefName::new("refs/heads/work").to_owned(),
        RefTarget::normal(commit),
    );
    let event = f.operation(std::slice::from_ref(&root), &local, "workspace-only", None);
    let before = f.normalize(&root);
    let after = f.normalize(&event);
    assert_ne!(before, after);
    let shared = f.anchors.read(&after).unwrap();
    assert_eq!(shared.view, SharedView::from_view(&original));
    assert_eq!(shared.view.with_local_state(&local), local);
    let integration = f.operation(&[event], &local, "same-view sync", Some(&after));
    assert_eq!(f.normalize(&integration), after);
    let undo = f.operation(&[integration], &local, "same-view undo", None);
    let undone = f.normalize(&undo);
    assert_ne!(undone, after);
    assert!(f.anchors.is_ancestor(&after, &undone).unwrap());
}

#[test]
fn finalization_changes_descend_from_applied_anchor() {
    let f = Fixture::new();
    let root = f.root();
    let view = f.view(&root);
    let a = f.operation(&[root], &view, "before", None);
    let shared = f.normalize(&a);
    let mut final_view = view;
    final_view.head_ids.insert(CommitId::from_bytes(&[3; 20]));
    let integration = f.operation(&[a], &final_view, "sync plus finalization", Some(&shared));
    let result = f.normalize(&integration);
    assert_ne!(result, shared);
    assert_eq!(f.anchors.read(&result).unwrap().parents, vec![shared]);
    assert_eq!(
        f.anchors.read(&result).unwrap().view,
        SharedView::from_view(&final_view)
    );
}

#[test]
fn history_rewrite_checkpoint_survives_next_pass_and_new_operations() {
    let f = Fixture::new();
    let root = f.root();
    let old_view = f.view(&root);
    let old = f.operation(std::slice::from_ref(&root), &old_view, "old boundary", None);
    let old_anchor = f.normalize(&old);
    let mut current_view = old_view;
    current_view.head_ids.insert(CommitId::from_bytes(&[4; 20]));
    let rewritten = f.operation(&[root], &current_view, "rewritten history", None);
    let checkpoint = Normalizer::new(&f.operations, &f.anchors)
        .normalize_current(&rewritten, Some((&old, &old_anchor)))
        .block_on()
        .unwrap();
    assert!(f.anchors.is_ancestor(&old_anchor, &checkpoint).unwrap());
    assert_eq!(
        f.anchors.read(&checkpoint).unwrap().view,
        SharedView::from_view(&current_view)
    );
    let repeated = Normalizer::new(&f.operations, &f.anchors)
        .normalize_current(&rewritten, Some((&rewritten, &checkpoint)))
        .block_on()
        .unwrap();
    assert_eq!(repeated, checkpoint);
    let next = f.operation(
        std::slice::from_ref(&rewritten),
        &current_view,
        "next edit",
        None,
    );
    let next_shared = Normalizer::new(&f.operations, &f.anchors)
        .normalize_current(&next, Some((&rewritten, &checkpoint)))
        .block_on()
        .unwrap();
    assert_eq!(
        f.anchors.read(&next_shared).unwrap().parents,
        vec![checkpoint]
    );
}

#[test]
fn rejects_integration_without_local_causal_baseline() {
    let f = Fixture::new();
    let root = f.root();
    let view = f.view(&root);
    let a = f.operation(std::slice::from_ref(&root), &view, "A", None);
    let b = f.operation(&[root], &view, "B", None);
    let b_shared = f.normalize(&b);
    let forged = f.operation(&[a], &view, "invalid integration", Some(&b_shared));
    assert!(matches!(
        Normalizer::new(&f.operations, &f.anchors)
            .normalize(&forged)
            .block_on(),
        Err(SyncError::InvalidData(_))
    ));
}

#[test]
fn rewriting_and_pruning_do_not_inherit_unverified_integration_anchors() {
    let f = Fixture::new();
    let root = f.root();
    let view = f.view(&root);
    let old = f.operation(std::slice::from_ref(&root), &view, "old boundary", None);
    let old_shared = f.normalize(&old);
    let parent = f.operation(std::slice::from_ref(&root), &view, "actual parent", None);
    let parent_shared = f.normalize(&parent);
    let unrelated = f.operation(&[root], &view, "unrelated history", None);
    let unrelated_shared = f.normalize(&unrelated);

    // Matching and differing views cannot authenticate a claimed causal edge.
    let mut changed_view = view.clone();
    changed_view.head_ids.insert(CommitId::from_bytes(&[9; 20]));
    for current_view in [&view, &changed_view] {
        let forged = f.operation(
            std::slice::from_ref(&parent),
            current_view,
            "unverified integration attribute",
            Some(&unrelated_shared),
        );
        let mut pruning = Normalizer::new(&f.operations, &f.anchors);
        pruning.allow_reparented_integrations();
        let authored = pruning.normalize(&forged).block_on().unwrap();
        let event = f.anchors.read(&authored).unwrap();
        assert_eq!(event.parents, vec![parent_shared.clone()]);
        assert_eq!(event.view, SharedView::from_view(current_view));
        assert!(!f.anchors.is_ancestor(&unrelated_shared, &authored).unwrap());

        let checkpoint = Normalizer::new(&f.operations, &f.anchors)
            .normalize_current(&forged, Some((&old, &old_shared)))
            .block_on()
            .unwrap();
        assert!(f.anchors.is_ancestor(&old_shared, &checkpoint).unwrap());
        assert!(f.anchors.is_ancestor(&parent_shared, &checkpoint).unwrap());
        assert!(
            !f.anchors
                .is_ancestor(&unrelated_shared, &checkpoint)
                .unwrap()
        );
        assert_eq!(
            f.anchors.read(&checkpoint).unwrap().view,
            SharedView::from_view(current_view)
        );
    }
}

#[test]
fn rewritten_history_keeps_foreign_knowledge_and_reparented_integrations() {
    let f = Fixture::new();
    let root = f.root();
    let view = f.view(&root);
    let old = f.operation(std::slice::from_ref(&root), &view, "old", None);
    let old_anchor = f.normalize(&old);
    let replacement = f.operation(std::slice::from_ref(&root), &view, "new parent", None);
    let peer = f.operation(&[root], &view, "peer knowledge", None);
    let peer_anchor = f.normalize(&peer);
    let selected = f.join(vec![old_anchor.clone(), peer_anchor.clone()], &view);
    let integrated = f.operation(&[old], &view, "old integration", Some(&selected));
    assert_eq!(f.normalize(&integrated), selected);
    // op abandon copies attributes while replacing original parents.
    let reparented = f.operation(
        &[replacement],
        &view,
        "reparented integration",
        Some(&selected),
    );
    let checkpoint = Normalizer::new(&f.operations, &f.anchors)
        .normalize_current(&reparented, Some((&integrated, &selected)))
        .block_on()
        .unwrap();
    assert!(f.anchors.is_ancestor(&selected, &checkpoint).unwrap());
    assert!(f.anchors.is_ancestor(&peer_anchor, &checkpoint).unwrap());
    assert_eq!(
        f.anchors.heads(&[peer_anchor, checkpoint.clone()]).unwrap(),
        vec![checkpoint]
    );
}

#[test]
fn checkpoint_recovers_when_previous_raw_boundary_was_garbage_collected() {
    let f = Fixture::new();
    let root = f.root();
    let view = f.view(&root);
    let old = f.operation(std::slice::from_ref(&root), &view, "old origin", None);
    let anchor = f.normalize(&old);
    let integration = f.operation(
        std::slice::from_ref(&old),
        &view,
        "old integration",
        Some(&anchor),
    );
    let replacement = f.operation(&[root], &view, "replacement", None);
    f.operations
        .gc(&[replacement.clone(), old], std::time::SystemTime::now())
        .block_on()
        .unwrap();
    assert!(matches!(
        f.operations.read_operation(&integration).block_on(),
        Err(OpStoreError::ObjectNotFound { .. })
    ));
    let checkpoint = Normalizer::new(&f.operations, &f.anchors)
        .normalize_current(&replacement, Some((&integration, &anchor)))
        .block_on()
        .unwrap();
    assert!(f.anchors.is_ancestor(&anchor, &checkpoint).unwrap());
}

#[test]
fn concurrent_raw_heads_are_both_preserved() {
    let f = Fixture::new();
    let root = f.root();
    let view = f.view(&root);
    let a = f.operation(std::slice::from_ref(&root), &view, "A", None);
    let b = f.operation(&[root], &view, "B", None);
    let merged = f.operation(
        &[a.clone(), b.clone()],
        &view,
        "concurrent resolution",
        None,
    );
    let shared = f.normalize(&merged);
    let event = f.anchors.read(&shared).unwrap();
    assert_eq!(event.parents.len(), 2);
    assert!(f.anchors.is_ancestor(&f.normalize(&a), &shared).unwrap());
    assert!(f.anchors.is_ancestor(&f.normalize(&b), &shared).unwrap());
    let closure = f.anchors.closure(&[shared]).unwrap();
    let mut seen = BTreeSet::new();
    for (id, event) in closure {
        assert!(event.parents.iter().all(|parent| seen.contains(parent)));
        seen.insert(id);
    }
}

#[test]
fn encoding_rejects_corruption_and_local_state() {
    let f = Fixture::new();
    let id = f.normalize(&f.root());
    let event = f.anchors.read(&id).unwrap();
    let bytes = AnchorStore::encode(&event).unwrap();
    assert_eq!(AnchorStore::decode(&id, &bytes).unwrap(), event);
    for length in 0..bytes.len() {
        assert!(AnchorStore::decode(&id, &bytes[..length]).is_err());
    }
    let mut corrupt = bytes;
    corrupt[0] ^= 1;
    assert!(AnchorStore::decode(&id, &corrupt).is_err());
    let other = SharedEventId::from_bytes(&[1; 64]);
    assert!(AnchorStore::decode(&other, &AnchorStore::encode(&event).unwrap()).is_err());
}
