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

use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigSource;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::GitRefName;
use jj_lib::ref_name::RefName;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::Repo as _;
use jj_lib::repo::RepoLoader;
use jj_lib::sync::native;
use jj_lib::sync::shared::SharedView;
use pollster::FutureExt as _;
use testutils::CommitBuilderExt as _;
use testutils::TestRepo;
use testutils::TestResult;
use testutils::base_user_config;
use testutils::create_random_commit;
use testutils::user_settings_from_config;
use testutils::write_random_commit;

#[test]
fn native_integrations_preserve_local_maps_and_repeat_without_operations() -> TestResult {
    let test = TestRepo::init();
    let base = &test.repo;
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
    let a = tx_a.write("A local state").block_on()?.leave_unpublished();

    let mut tx_b = base.start_transaction();
    let commit_b = write_random_commit(tx_b.repo_mut());
    tx_b.repo_mut()
        .set_wc_commit(WorkspaceName::DEFAULT.to_owned(), commit_b.id().clone())?;
    tx_b.repo_mut().set_wc_commit(
        WorkspaceName::new("peer-only").to_owned(),
        commit_b.id().clone(),
    )?;
    tx_b.repo_mut().set_local_bookmark_target(
        RefName::new("peer-work"),
        RefTarget::normal(commit_b.id().clone()),
    );
    let b = tx_b.write("B local state").block_on()?.leave_unpublished();
    let selected = native::reconcile(&a, &[a.op_id().clone(), b.op_id().clone()]).block_on()?;
    let mut other_config = base_user_config();
    other_config.add_layer(ConfigLayer::parse(
        ConfigSource::User,
        r#"
        operation.username = "another-user"
        operation.hostname = "another-machine"
        debug.operation-timestamp = "2030-01-01T00:00:00Z"
        "#,
    )?);
    let other_settings = user_settings_from_config(other_config);
    let other_loader = RepoLoader::init_from_file_system(
        &other_settings,
        test.repo_path(),
        &test.env.default_backend_factories(),
    )?;
    let other_b = other_loader.load_at(b.operation()).block_on()?;
    assert_eq!(
        native::reconcile(&other_b, &[b.op_id().clone(), a.op_id().clone()]).block_on()?,
        selected
    );
    let selected_op = a.loader().load_operation(&selected).block_on()?;
    assert_eq!(selected_op.parent_ids().len(), 2);
    assert!(
        !selected_op
            .metadata()
            .attributes
            .contains_key(native::MERGE_VIEW_ATTRIBUTE)
    );
    assert!(selected_op.view().block_on()?.wc_commit_ids().is_empty());
    assert!(!test.repo_path().join("sync/events").exists());

    let a_tx = native::prepare_integration(&a, std::slice::from_ref(&selected))
        .block_on()?
        .unwrap();
    assert_eq!(a_tx.parent_ops()[0].id(), a.op_id());
    assert_eq!(a_tx.parent_ops()[1].id(), &selected);
    assert_eq!(a_tx.repo().view().wc_commit_ids(), a.view().wc_commit_ids());
    assert_eq!(a_tx.repo().view().all_git_heads(), a.view().all_git_heads());
    assert_eq!(a_tx.repo().view().git_refs(), a.view().git_refs());
    let a_sync = a_tx.write("A sync").block_on()?.leave_unpublished();
    assert_eq!(
        a_sync.operation().metadata().attributes[native::LOCAL_PARENT_ATTRIBUTE],
        a.op_id().hex()
    );
    let b_sync = native::prepare_integration(&b, std::slice::from_ref(&selected))
        .block_on()?
        .unwrap()
        .write("B sync")
        .block_on()?
        .leave_unpublished();
    assert_eq!(b_sync.view().wc_commit_ids(), b.view().wc_commit_ids());
    assert_eq!(
        SharedView::from_view(a_sync.view().store_view()),
        SharedView::from_view(b_sync.view().store_view())
    );
    assert_ne!(a_sync.op_id(), b_sync.op_id());
    assert_eq!(
        native::frontier(a.op_store().as_ref(), &[a_sync.op_id().clone()]).block_on()?,
        native::frontier(b.op_store().as_ref(), &[b_sync.op_id().clone()]).block_on()?
    );
    assert!(
        native::prepare_integration(&a_sync, &[b_sync.op_id().clone()])
            .block_on()?
            .is_none()
    );
    assert!(
        native::prepare_integration(&b_sync, &[a_sync.op_id().clone()])
            .block_on()?
            .is_none()
    );
    assert_eq!(
        native::reconcile(&a_sync, &[a_sync.op_id().clone(), b_sync.op_id().clone()]).block_on()?,
        selected
    );
    Ok(())
}

#[test]
fn generated_rebases_are_authored_and_keep_operation_predecessors() -> TestResult {
    let test = TestRepo::init();
    let mut base_tx = test.repo.start_transaction();
    let parent = write_random_commit(base_tx.repo_mut());
    let base = base_tx.write("base parent").block_on()?.leave_unpublished();

    let mut rewrite_tx = base.start_transaction();
    let rewritten_parent = rewrite_tx
        .repo_mut()
        .rewrite_commit(&parent)
        .set_description("rewritten parent")
        .write_unwrap();
    rewrite_tx.repo_mut().rebase_descendants().block_on()?;
    let rewrite = rewrite_tx
        .write("rewrite parent")
        .block_on()?
        .leave_unpublished();

    let mut child_tx = base.start_transaction();
    let child = create_random_commit(child_tx.repo_mut())
        .set_description("concurrent child")
        .set_parents(vec![parent.id().clone()])
        .write_unwrap();
    child_tx
        .repo_mut()
        .set_local_bookmark_target(RefName::new("child"), RefTarget::normal(child.id().clone()));
    let child_repo = child_tx
        .write("add concurrent child")
        .block_on()?
        .leave_unpublished();

    let selected = native::reconcile(
        &rewrite,
        &[rewrite.op_id().clone(), child_repo.op_id().clone()],
    )
    .block_on()?;
    let selected_op = rewrite.loader().load_operation(&selected).block_on()?;
    assert!(
        !selected_op
            .metadata()
            .attributes
            .contains_key(native::MERGE_VIEW_ATTRIBUTE)
    );
    assert_eq!(
        native::frontier(rewrite.op_store().as_ref(), std::slice::from_ref(&selected))
            .block_on()?,
        vec![selected.clone()]
    );
    let selected_repo = rewrite.loader().load_at(&selected_op).block_on()?;
    let rebased_id = selected_repo
        .view()
        .get_local_bookmark(RefName::new("child"))
        .as_normal()
        .unwrap();
    let rebased = selected_repo.store().get_commit(rebased_id)?;
    assert_ne!(rebased.id(), child.id());
    assert_eq!(rebased.parent_ids(), &[rewritten_parent.id().clone()]);
    assert_eq!(
        selected_op.predecessors_for_commit(rebased.id()),
        Some(std::slice::from_ref(child.id()))
    );
    assert!(selected_repo.index().has_id(child.id()).block_on()?);
    assert!(selected_repo.index().has_id(rebased.id()).block_on()?);

    let left = native::prepare_integration(&rewrite, std::slice::from_ref(&selected))
        .block_on()?
        .unwrap()
        .write("left sync")
        .block_on()?
        .leave_unpublished();
    let right = native::prepare_integration(&child_repo, std::slice::from_ref(&selected))
        .block_on()?
        .unwrap()
        .write("right sync")
        .block_on()?
        .leave_unpublished();
    assert_eq!(
        SharedView::from_view(left.view().store_view()),
        SharedView::from_view(right.view().store_view())
    );
    assert!(
        native::prepare_integration(&left, &[right.op_id().clone()])
            .block_on()?
            .is_none()
    );
    assert!(
        native::prepare_integration(&right, &[left.op_id().clone()])
            .block_on()?
            .is_none()
    );
    Ok(())
}
