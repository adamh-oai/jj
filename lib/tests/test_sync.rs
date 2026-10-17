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

use std::collections::BTreeSet;

use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::RefTarget;
use jj_lib::op_walk;
use jj_lib::ref_name::GitRefName;
use jj_lib::ref_name::RefName;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::Repo as _;
use jj_lib::sync::anchor::AnchorStore;
use jj_lib::sync::endpoint::Endpoint;
use jj_lib::sync::endpoint::EndpointError;
use jj_lib::sync::normalize::Normalizer;
use jj_lib::sync::reconcile::Reconciler;
use jj_lib::sync::reconcile::prepare_integration;
use jj_lib::sync::state::SessionPhase;
use jj_lib::sync::state::SyncSession;
use pollster::FutureExt as _;
use testutils::TestRepo;
use testutils::TestResult;
use testutils::write_random_commit;

#[test]
fn merge_and_local_integration_preserve_workspace_maps_and_undo() -> TestResult {
    let test = TestRepo::init();
    let base = &test.repo;
    let anchors = AnchorStore::init(test.repo_path())?;
    let mut tx_a = base.start_transaction();
    let commit_a = write_random_commit(tx_a.repo_mut());
    tx_a.repo_mut()
        .set_wc_commit(WorkspaceName::DEFAULT.to_owned(), commit_a.id().clone())?;
    tx_a.repo_mut().set_git_head_target(
        WorkspaceName::DEFAULT,
        RefTarget::normal(commit_a.id().clone()),
    );
    tx_a.repo_mut().set_git_ref_target(
        GitRefName::new("refs/heads/local"),
        RefTarget::normal(commit_a.id().clone()),
    );
    let a = tx_a.commit("A local workspace").block_on()?;

    let mut tx_b = base.start_transaction();
    let commit_b = write_random_commit(tx_b.repo_mut());
    tx_b.repo_mut()
        .set_wc_commit(WorkspaceName::DEFAULT.to_owned(), commit_b.id().clone())?;
    tx_b.repo_mut().set_wc_commit(
        WorkspaceName::new("only-on-peer").to_owned(),
        commit_b.id().clone(),
    )?;
    tx_b.repo_mut().set_git_head_target(
        WorkspaceName::DEFAULT,
        RefTarget::normal(commit_b.id().clone()),
    );
    tx_b.repo_mut().set_local_bookmark_target(
        RefName::new("peer-work"),
        RefTarget::normal(commit_b.id().clone()),
    );
    let b = tx_b.commit("B local workspace").block_on()?;

    let mut normalizer = Normalizer::new(a.op_store().as_ref(), &anchors);
    let a_shared = normalizer.normalize(a.op_id()).block_on()?;
    let b_shared = normalizer.normalize(b.op_id()).block_on()?;
    let selected = Reconciler::new(&a, &anchors)
        .reconcile(&[a_shared.clone(), b_shared])
        .block_on()?;
    let mut tx = prepare_integration(&a, &anchors, &a_shared, &selected)
        .block_on()?
        .unwrap();
    tx.repo_mut().rebase_descendants().block_on()?;
    assert_eq!(tx.parent_ops().len(), 1);
    assert_eq!(tx.parent_ops()[0].id(), a.op_id());
    assert_eq!(tx.repo().view().wc_commit_ids(), a.view().wc_commit_ids());
    assert_eq!(tx.repo().view().all_git_heads(), a.view().all_git_heads());
    assert_eq!(tx.repo().view().git_refs(), a.view().git_refs());
    assert_eq!(
        tx.repo()
            .view()
            .get_local_bookmark(RefName::new("peer-work")),
        &RefTarget::normal(commit_b.id().clone())
    );
    let integrated = tx.commit("sync receive").block_on()?;
    assert_eq!(
        Normalizer::new(integrated.op_store().as_ref(), &anchors)
            .normalize(integrated.op_id())
            .block_on()?,
        selected
    );
    assert!(
        prepare_integration(&integrated, &anchors, &selected, &selected)
            .block_on()?
            .is_none()
    );

    let mut undo = integrated.start_transaction();
    undo.repo_mut().set_view(a.view().store_view().clone());
    let undone = undo.commit("undo: restore local baseline").block_on()?;
    let undo_anchor = Normalizer::new(undone.op_store().as_ref(), &anchors)
        .normalize(undone.op_id())
        .block_on()?;
    assert!(anchors.is_ancestor(&selected, &undo_anchor)?);
    assert_eq!(
        Reconciler::new(&undone, &anchors)
            .reconcile(&[selected, undo_anchor.clone()])
            .block_on()?,
        undo_anchor
    );
    assert_eq!(undone.view(), a.view());
    Ok(())
}

#[test]
fn same_view_causal_integration_writes_an_undoable_operation() -> TestResult {
    let test = TestRepo::init();
    let base = &test.repo;
    let anchors = AnchorStore::init(test.repo_path())?;
    let a = base.start_transaction().commit("same state A").block_on()?;
    let b = base.start_transaction().commit("same state B").block_on()?;
    let mut normalizer = Normalizer::new(a.op_store().as_ref(), &anchors);
    let a_shared = normalizer.normalize(a.op_id()).block_on()?;
    let b_shared = normalizer.normalize(b.op_id()).block_on()?;
    let selected = Reconciler::new(&a, &anchors)
        .reconcile(&[a_shared.clone(), b_shared])
        .block_on()?;
    let mut tx = prepare_integration(&a, &anchors, &a_shared, &selected)
        .block_on()?
        .unwrap();
    tx.repo_mut().rebase_descendants().block_on()?;
    assert!(!tx.repo().has_changes());
    let integrated = tx.commit("same-view sync").block_on()?;
    assert_eq!(integrated.operation().parent_ids(), &[a.op_id().clone()]);
    assert_eq!(
        Normalizer::new(integrated.op_store().as_ref(), &anchors)
            .normalize(integrated.op_id())
            .block_on()?,
        selected
    );
    Ok(())
}

#[test]
fn endpoint_capture_rejects_stale_and_parallel_heads_without_advancing_boundary() -> TestResult {
    let test = TestRepo::init();
    let endpoint = Endpoint::enable_quiescent(test.repo_path())?;
    let base = &test.repo;
    let original = endpoint.capture(base, "initial").block_on()?;
    let newer = base.start_transaction().commit("newer").block_on()?;
    assert!(matches!(
        endpoint.capture(base, "stale").block_on(),
        Err(EndpointError::StaleCapture)
    ));
    let parallel = base.start_transaction().commit("parallel").block_on()?;
    assert!(matches!(
        endpoint.capture(&newer, "parallel").block_on(),
        Err(EndpointError::StaleCapture)
    ));
    assert!(matches!(
        endpoint.capture(&parallel, "parallel").block_on(),
        Err(EndpointError::StaleCapture)
    ));
    let state = endpoint.state_store().read()?;
    assert_eq!(
        state.local_anchor.unwrap().raw_head,
        original.current_raw.hex()
    );
    assert_eq!(state.transfer_pins.len(), 1);
    assert_eq!(
        state.shared_anchors,
        BTreeSet::from([original.current_shared.hex()])
    );
    Ok(())
}

#[test]
fn endpoint_recapture_after_op_abandon_keeps_shared_causal_checkpoint() -> TestResult {
    let test = TestRepo::init();
    let endpoint = Endpoint::enable_quiescent(test.repo_path())?;
    let base = &test.repo;
    let a = base.start_transaction().commit("A").block_on()?;
    let mut tx = a.start_transaction();
    write_random_commit(tx.repo_mut());
    let b = tx.commit("B").block_on()?;
    let original = endpoint.capture(&b, "before-abandon").block_on()?;
    let rewritten = op_walk::reparent_range(
        b.op_store().as_ref(),
        std::slice::from_ref(a.operation()),
        std::slice::from_ref(b.operation()),
        base.operation(),
    )
    .block_on()?;
    let new_id = &rewritten.new_head_ids[0];
    b.loader()
        .op_heads_store()
        .update_op_heads(std::slice::from_ref(b.op_id()), new_id)
        .block_on()?;
    let operation = b.loader().load_operation(new_id).block_on()?;
    let current = b.loader().load_at(&operation).block_on()?;
    let captured = endpoint.capture(&current, "after-abandon").block_on()?;
    assert_ne!(captured.current_raw, original.current_raw);
    assert_ne!(captured.current_shared, original.current_shared);
    assert!(
        endpoint
            .anchors()
            .is_ancestor(&original.current_shared, &captured.current_shared)?
    );
    assert_eq!(
        endpoint.anchors().read(&captured.current_shared)?.view,
        endpoint.anchors().read(&original.current_shared)?.view
    );
    assert_eq!(
        endpoint
            .capture(&current, "repeat")
            .block_on()?
            .current_shared,
        captured.current_shared
    );
    Ok(())
}

#[test]
fn endpoint_reconcile_result_is_retained_before_the_caller_can_pin_it() -> TestResult {
    let test = TestRepo::init();
    let endpoint = Endpoint::enable_quiescent(test.repo_path())?;
    let base = &test.repo;
    let a = base.start_transaction().commit("A").block_on()?;
    let captured = endpoint.capture(&a, "outgoing").block_on()?;
    let b = base.start_transaction().commit("B").block_on()?;
    let incoming = Normalizer::new(b.op_store().as_ref(), endpoint.anchors())
        .normalize(b.op_id())
        .block_on()?;
    let selected = endpoint
        .reconcile(&a, &captured.current_shared, &[incoming])
        .block_on()?;
    endpoint.release_transfer("outgoing")?;
    let reopened = Endpoint::load(test.repo_path());
    let state = reopened.state_store().read()?;
    assert!(state.gc_roots().shared_events.contains(&selected.hex()));
    let active = a.loader().op_heads_store().get_op_heads().block_on()?;
    let plan =
        jj_lib::sync::gc::collect_roots(a.op_store().as_ref(), reopened.anchors(), &state, &active)
            .block_on()?;
    assert!(plan.shared_events.contains(&selected));
    assert_eq!(reopened.anchors().read(&selected)?.parents.len(), 2);
    Ok(())
}

#[test]
fn endpoint_recovers_a_published_receipt_from_ancestor_history_exactly_once() -> TestResult {
    let test = TestRepo::init();
    let endpoint = Endpoint::enable_quiescent(test.repo_path())?;
    let base = &test.repo;
    let initial = endpoint.capture(base, "initial").block_on()?;
    let unpublished = base.start_transaction().write("sync result").block_on()?;
    let result_id = unpublished.operation().id().clone();
    let selected = Normalizer::new(base.op_store().as_ref(), endpoint.anchors())
        .normalize(&result_id)
        .block_on()?;
    let live_publisher = endpoint.acquire_session_lease("lost-ack")?;
    let mut locked = endpoint.state_store().lock()?;
    locked.state_mut().begin_session(
        "lost-ack".to_owned(),
        SyncSession {
            peer: "peer".to_owned(),
            baseline_operation: base.op_id().hex(),
            incoming_raw_heads: BTreeSet::from([base.op_id().hex()]),
            incoming_shared_heads: BTreeSet::from([initial.current_shared.hex()]),
            applied_anchor: Some(selected.hex()),
            result_operation: Some(result_id.hex()),
            phase: SessionPhase::Pending,
        },
    )?;
    locked.save()?;
    let published = unpublished.publish().block_on()?;
    drop(locked);
    drop(live_publisher);
    let descendant = published
        .start_transaction()
        .commit("later local work")
        .block_on()?;
    assert!(
        !endpoint
            .retire_unpublished(&descendant, "lost-ack")
            .block_on()?
    );
    assert_eq!(
        endpoint.state_store().read()?.sessions["lost-ack"].phase,
        SessionPhase::Pending
    );
    assert_eq!(
        endpoint.recover_published(&descendant).block_on()?,
        vec!["lost-ack"]
    );
    assert!(
        endpoint
            .recover_published(&descendant)
            .block_on()?
            .is_empty()
    );
    assert!(
        !endpoint
            .retire_unpublished(&descendant, "lost-ack")
            .block_on()?
    );
    assert_eq!(
        endpoint.state_store().read()?.sessions["lost-ack"].phase,
        SessionPhase::Completed
    );
    assert_eq!(
        descendant
            .loader()
            .op_heads_store()
            .get_op_heads()
            .block_on()?,
        vec![descendant.op_id().clone()]
    );
    Ok(())
}

#[test]
fn endpoint_retires_never_published_receipt_without_losing_installed_history() -> TestResult {
    let test = TestRepo::init();
    let endpoint = Endpoint::enable_quiescent(test.repo_path())?;
    let base = &test.repo;
    let initial = endpoint.capture(base, "initial").block_on()?;
    let mut incoming_tx = base.start_transaction();
    write_random_commit(incoming_tx.repo_mut());
    let unpublished = incoming_tx.write("failed sync result").block_on()?;
    let incoming = unpublished.leave_unpublished();
    let selected = Normalizer::new(base.op_store().as_ref(), endpoint.anchors())
        .normalize(incoming.op_id())
        .block_on()?;
    let live_publisher = endpoint.acquire_session_lease("never-published")?;
    {
        let mut locked = endpoint.state_store().lock()?;
        locked.state_mut().begin_session(
            "never-published".to_owned(),
            SyncSession {
                peer: "peer".to_owned(),
                baseline_operation: base.op_id().hex(),
                incoming_raw_heads: BTreeSet::from([incoming.op_id().hex()]),
                incoming_shared_heads: BTreeSet::from([selected.hex()]),
                applied_anchor: Some(selected.hex()),
                result_operation: Some(incoming.op_id().hex()),
                phase: SessionPhase::Pending,
            },
        )?;
        locked.save()?;
    }
    assert!(endpoint.recover_published(base).block_on()?.is_empty());
    assert!(
        !endpoint
            .retire_unpublished(base, "never-published")
            .block_on()?
    );
    assert!(
        endpoint
            .state_store()
            .read()?
            .sessions
            .contains_key("never-published")
    );
    drop(live_publisher);
    assert!(
        endpoint
            .retire_unpublished(base, "never-published")
            .block_on()?
    );
    assert!(
        !endpoint
            .retire_unpublished(base, "never-published")
            .block_on()?
    );
    let state = endpoint.state_store().read()?;
    assert!(state.sessions.is_empty());
    assert!(
        state.peers["peer"]
            .raw_heads
            .contains(&incoming.op_id().hex())
    );
    assert!(state.gc_roots().shared_events.contains(&selected.hex()));
    let mut tx = prepare_integration(base, endpoint.anchors(), &initial.current_shared, &selected)
        .block_on()?
        .unwrap();
    tx.repo_mut().rebase_descendants().block_on()?;
    let retried = tx.commit("retry sync result").block_on()?;
    assert!(endpoint.recover_published(&retried).block_on()?.is_empty());
    assert_eq!(
        retried
            .loader()
            .op_heads_store()
            .get_op_heads()
            .block_on()?,
        vec![retried.op_id().clone()]
    );
    assert_eq!(
        retried.operation().parent_ids(),
        std::slice::from_ref(base.op_id())
    );
    assert_eq!(retried.view(), incoming.view());
    Ok(())
}

#[test]
fn pruning_an_interrupted_published_integration_preserves_its_undo() -> TestResult {
    let test = TestRepo::init();
    let endpoint = Endpoint::enable_quiescent(test.repo_path())?;
    let base = &test.repo;
    let initial = endpoint.capture(base, "initial").block_on()?;
    let mut peer_tx = base.start_transaction();
    let commit = write_random_commit(peer_tx.repo_mut());
    peer_tx.repo_mut().set_local_bookmark_target(
        RefName::new("peer-work"),
        RefTarget::normal(commit.id().clone()),
    );
    let peer = peer_tx.write("peer work").block_on()?.leave_unpublished();
    let peer_shared = Normalizer::new(base.op_store().as_ref(), endpoint.anchors())
        .normalize(peer.op_id())
        .block_on()?;
    let selected = endpoint
        .reconcile(
            base,
            &initial.current_shared,
            std::slice::from_ref(&peer_shared),
        )
        .block_on()?;
    let mut tx = prepare_integration(base, endpoint.anchors(), &initial.current_shared, &selected)
        .block_on()?
        .unwrap();
    tx.repo_mut().rebase_descendants().block_on()?;
    let lease = endpoint.acquire_session_lease("interrupted")?;
    let mut state = endpoint.state_store().lock()?;
    let unpublished = tx.write("interrupted sync").block_on()?;
    state.state_mut().begin_session(
        "interrupted".to_owned(),
        SyncSession {
            peer: "peer".to_owned(),
            baseline_operation: base.op_id().hex(),
            incoming_raw_heads: BTreeSet::from([peer.op_id().hex()]),
            incoming_shared_heads: BTreeSet::from([peer_shared.hex()]),
            applied_anchor: Some(selected.hex()),
            result_operation: Some(unpublished.operation().id().hex()),
            phase: SessionPhase::Pending,
        },
    )?;
    state.save()?;
    let published = unpublished.publish().block_on()?;
    drop(state);
    drop(lease);
    // The process exited before receipt recovery and the final current capture.
    assert_eq!(
        endpoint
            .state_store()
            .read()?
            .local_anchor
            .unwrap()
            .raw_head,
        base.op_id().hex()
    );
    let mut undo = published.start_transaction();
    undo.repo_mut().set_view(base.view().store_view().clone());
    let undone = undo.commit("undo interrupted sync").block_on()?;
    let guard = endpoint
        .preserve_pruned_heads(
            base.op_store().as_ref(),
            std::slice::from_ref(undone.op_id()),
        )
        .block_on()?;
    let reparented = op_walk::reparent_range(
        base.op_store().as_ref(),
        std::slice::from_ref(published.operation()),
        std::slice::from_ref(undone.operation()),
        base.operation(),
    )
    .block_on()?;
    base.loader()
        .op_heads_store()
        .update_op_heads(
            std::slice::from_ref(undone.op_id()),
            &reparented.new_head_ids[0],
        )
        .block_on()?;
    drop(guard);
    let current = base.loader().load_at_head().block_on()?;
    assert!(endpoint.recover_published(&current).block_on()?.is_empty());
    assert!(
        endpoint
            .retire_unpublished(&current, "interrupted")
            .block_on()?
    );
    let state = endpoint.state_store().read()?;
    let active = current
        .loader()
        .op_heads_store()
        .get_op_heads()
        .block_on()?;
    let retained = jj_lib::sync::gc::collect_roots(
        base.op_store().as_ref(),
        endpoint.anchors(),
        &state,
        &active,
    )
    .block_on()?;
    assert!(retained.raw_operations.contains(published.op_id()));
    let capture = endpoint.capture(&current, "retry").block_on()?;
    assert!(
        endpoint
            .anchors()
            .is_ancestor(&selected, &capture.current_shared)?
    );
    assert_eq!(
        endpoint
            .reconcile(&current, &capture.current_shared, &[peer_shared])
            .block_on()?,
        capture.current_shared
    );
    assert!(
        current
            .view()
            .get_local_bookmark(RefName::new("peer-work"))
            .is_absent()
    );
    assert!(
        endpoint
            .state_store()
            .read()?
            .pending_pruned_shared_heads
            .is_empty()
    );
    assert_eq!(
        endpoint
            .capture(&current, "repeat")
            .block_on()?
            .current_shared,
        capture.current_shared
    );
    Ok(())
}

#[test]
fn pruning_multiple_raw_heads_preserves_each_causal_frontier_without_selecting_its_view()
-> TestResult {
    let test = TestRepo::init();
    let endpoint = Endpoint::enable_quiescent(test.repo_path())?;
    let base = &test.repo;
    endpoint.capture(base, "initial").block_on()?;
    let common = base
        .start_transaction()
        .commit("common operation")
        .block_on()?;
    let mut first = common.start_transaction();
    let first_commit = write_random_commit(first.repo_mut());
    first.repo_mut().set_local_bookmark_target(
        RefName::new("one"),
        RefTarget::normal(first_commit.id().clone()),
    );
    let first = first.commit("first head").block_on()?;
    let mut second = common.start_transaction();
    let second_commit = write_random_commit(second.repo_mut());
    second.repo_mut().set_local_bookmark_target(
        RefName::new("two"),
        RefTarget::normal(second_commit.id().clone()),
    );
    let second = second.commit("second head").block_on()?;
    let mut normalizer = Normalizer::new(base.op_store().as_ref(), endpoint.anchors());
    let first_shared = normalizer.normalize(first.op_id()).block_on()?;
    let second_shared = normalizer.normalize(second.op_id()).block_on()?;
    let heads = [first.op_id().clone(), second.op_id().clone()];
    let guard = endpoint
        .preserve_pruned_heads(base.op_store().as_ref(), &heads)
        .block_on()?;
    assert!(
        guard
            .state()
            .pending_pruned_shared_heads
            .contains(&first_shared.hex())
    );
    assert!(
        guard
            .state()
            .pending_pruned_shared_heads
            .contains(&second_shared.hex())
    );
    let reparented = op_walk::reparent_range(
        base.op_store().as_ref(),
        std::slice::from_ref(common.operation()),
        &[first.operation().clone(), second.operation().clone()],
        base.operation(),
    )
    .block_on()?;
    for (old, new) in heads.iter().zip(&reparented.new_head_ids) {
        base.loader()
            .op_heads_store()
            .update_op_heads(std::slice::from_ref(old), new)
            .block_on()?;
    }
    drop(guard);
    let current = base.loader().load_at_head().block_on()?;
    let state = endpoint.state_store().read()?;
    let plan = jj_lib::sync::gc::collect_roots(
        base.op_store().as_ref(),
        endpoint.anchors(),
        &state,
        std::slice::from_ref(current.op_id()),
    )
    .block_on()?;
    assert!(plan.raw_operations.contains(first.op_id()));
    assert!(plan.raw_operations.contains(second.op_id()));
    let captured = endpoint.capture(&current, "after-pruning").block_on()?;
    for old in [first_shared, second_shared] {
        assert!(
            endpoint
                .anchors()
                .is_ancestor(&old, &captured.current_shared)?
        );
    }
    assert_eq!(
        current.view().get_local_bookmark(RefName::new("one")),
        &RefTarget::normal(first_commit.id().clone())
    );
    assert_eq!(
        current.view().get_local_bookmark(RefName::new("two")),
        &RefTarget::normal(second_commit.id().clone())
    );
    assert_eq!(
        endpoint.anchors().read(&captured.current_shared)?.view,
        jj_lib::sync::shared::SharedView::from_view(current.view().store_view())
    );
    assert!(
        endpoint
            .state_store()
            .read()?
            .pending_pruned_shared_heads
            .is_empty()
    );
    Ok(())
}
