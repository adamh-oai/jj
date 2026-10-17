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

//! Reconcile shared history using jj's existing commit and reference merging.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::LocalBoxFuture;

use super::SyncError;
use super::SyncResult;
use super::anchor::AnchorStore;
use super::normalize::INTEGRATION_ANCHOR_ATTRIBUTE;
use super::shared::SharedEvent;
use super::shared::SharedEventId;
use super::shared::SharedView;
use crate::object_id::ObjectId as _;
use crate::op_store;
use crate::repo::ReadonlyRepo;
use crate::repo::Repo as _;
use crate::transaction::Transaction;
use crate::view::View;

/// Reconciles immutable shared events; it never publishes original operations.
pub struct Reconciler<'a> {
    repo: &'a Arc<ReadonlyRepo>,
    anchors: &'a AnchorStore,
}

impl<'a> Reconciler<'a> {
    /// Uses this endpoint's backend and index for normal jj merge behavior.
    pub fn new(repo: &'a Arc<ReadonlyRepo>, anchors: &'a AnchorStore) -> Self {
        Self { repo, anchors }
    }

    /// Reconciles all non-ancestor frontiers into one shared causal result.
    pub async fn reconcile(&self, frontiers: &[SharedEventId]) -> SyncResult<SharedEventId> {
        let heads = self.anchors.heads(frontiers)?;
        let mut iter = heads.into_iter();
        let Some(mut result) = iter.next() else {
            return Err(SyncError::InvalidData("empty shared frontier".to_owned()));
        };
        for other in iter {
            result = self.merge_pair(result, other).await?;
        }
        Ok(result)
    }

    fn merge_pair(
        &self,
        left: SharedEventId,
        right: SharedEventId,
    ) -> LocalBoxFuture<'_, SyncResult<SharedEventId>> {
        async move {
            let heads = self.anchors.heads(&[left, right])?;
            if heads.len() == 1 {
                return Ok(heads[0].clone());
            }
            let left = &heads[0];
            let right = &heads[1];
            let left_ancestors = self
                .anchors
                .closure(std::slice::from_ref(left))?
                .into_iter()
                .map(|(id, _)| id)
                .collect::<BTreeSet<_>>();
            let common = self
                .anchors
                .closure(std::slice::from_ref(right))?
                .into_iter()
                .filter_map(|(id, _)| left_ancestors.contains(&id).then_some(id))
                .collect::<Vec<_>>();
            let bases = self.anchors.heads(&common)?;
            let base_view = if bases.is_empty() {
                SharedView::from_view(&op_store::View::make_root(
                    self.repo.store().root_commit_id().clone(),
                ))
            } else {
                let mut bases = bases.into_iter();
                let mut base = bases.next().unwrap();
                for other_base in bases {
                    base = self.merge_pair(base, other_base).await?;
                }
                self.anchors.read(&base)?.view
            };
            let left_view = self.anchors.read(left)?.view;
            let right_view = self.anchors.read(right)?.view;
            let mut tx = self.repo.start_transaction();
            index_views(&mut tx, &[&base_view, &left_view, &right_view]).await?;
            tx.repo_mut().set_view(left_view.as_view().clone());
            tx.repo_mut()
                .merge_shared_view(base_view.as_view(), right_view.as_view())
                .await?;
            tx.repo_mut().rebase_descendants().await?;
            let view = SharedView::from_view(tx.repo().view().store_view());
            self.anchors.write(&SharedEvent::new(heads, view, None))
        }
        .boxed_local()
    }
}

/// Prepares a single-parent local integration without publishing it.
///
/// The caller performs ordinary workspace finalization and persists its
/// write-ahead receipt before publishing the resulting operation. The applied
/// anchor stays in metadata even if finalization adds another shared effect;
/// normalization then represents that effect as a child of the anchor.
pub async fn prepare_integration(
    repo: &Arc<ReadonlyRepo>,
    anchors: &AnchorStore,
    current: &SharedEventId,
    selected: &SharedEventId,
) -> SyncResult<Option<Transaction>> {
    let baseline = anchors.read(current)?.view;
    let target = anchors.read(selected)?.view;
    if SharedView::from_view(repo.view().store_view()) != baseline {
        return Err(SyncError::InvalidData(
            "integration baseline no longer matches current operation".to_owned(),
        ));
    }
    if current == selected {
        return Ok(None);
    }
    if !anchors.is_ancestor(current, selected)? {
        return Err(SyncError::InvalidData(
            "integration anchor does not include current history".to_owned(),
        ));
    }
    let mut tx = repo.start_transaction();
    index_views(&mut tx, &[&baseline, &target]).await?;
    tx.repo_mut()
        .merge_shared_view(baseline.as_view(), target.as_view())
        .await?;
    tx.set_attribute(INTEGRATION_ANCHOR_ATTRIBUTE.to_owned(), selected.hex());
    Ok(Some(tx))
}

async fn index_views(tx: &mut Transaction, views: &[&SharedView]) -> SyncResult<()> {
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
        commits.push(tx.repo().store().get_commit_async(&id).await?);
    }
    tx.repo_mut().index_commits(&commits).await?;
    Ok(())
}
