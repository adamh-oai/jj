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

//! Normalize original operations into shared causal events.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use super::SyncError;
use super::SyncResult;
use super::anchor::AnchorStore;
use super::shared::SharedEvent;
use super::shared::SharedEventId;
use super::shared::SharedView;
use crate::object_id::ObjectId as _;
use crate::op_store::OpStore;
use crate::op_store::OpStoreError;
use crate::op_store::Operation;
use crate::op_store::OperationId;

/// Operation attribute recording the shared result applied by an integration.
pub const INTEGRATION_ANCHOR_ATTRIBUTE: &str = "jj.sync.shared-anchor";

/// A normalization pass over immutable raw operations.
///
/// The cache is scoped to this pass. A persisted advertised anchor must never
/// replace normalization of the actual current operation: an undo, even one
/// leaving the same view, carries new causal knowledge.
pub struct Normalizer<'a> {
    op_store: &'a dyn OpStore,
    anchors: &'a AnchorStore,
    normalized: BTreeMap<OperationId, SharedEventId>,
    rewritten_history: bool,
}

impl<'a> Normalizer<'a> {
    /// Starts a pass over an endpoint's original history.
    pub fn new(op_store: &'a dyn OpStore, anchors: &'a AnchorStore) -> Self {
        Self {
            op_store,
            anchors,
            normalized: BTreeMap::new(),
            rewritten_history: false,
        }
    }

    /// Reuses a verified historical boundary without interpreting other branches.
    pub(super) async fn reuse_boundary(
        &mut self,
        raw: &OperationId,
        shared: &SharedEventId,
    ) -> SyncResult<()> {
        let event = self.anchors.read(shared)?;
        match self.op_store.read_operation(raw).await {
            Ok(operation) => {
                let view =
                    SharedView::from_view(&self.op_store.read_view(&operation.view_id).await?);
                if event.view != view {
                    return Err(invalid(
                        "normalization boundary does not match its operation",
                    ));
                }
                self.normalized.insert(raw.clone(), shared.clone());
            }
            Err(OpStoreError::ObjectNotFound { .. }) => {}
            Err(err) => return Err(err.into()),
        }
        Ok(())
    }

    /// Native pruning durably recorded that integrations may have new parents.
    pub(super) fn allow_reparented_integrations(&mut self) {
        self.rewritten_history = true;
    }

    /// Normalizes a raw operation and all missing ancestors, without recursion.
    pub async fn normalize(&mut self, head: &OperationId) -> SyncResult<SharedEventId> {
        enum Pending {
            Visit(OperationId),
            Finish(OperationId, Box<Operation>),
        }
        let mut pending = vec![Pending::Visit(head.clone())];
        let mut visiting = BTreeSet::new();
        while let Some(item) = pending.pop() {
            match item {
                Pending::Visit(id) => {
                    if self.normalized.contains_key(&id) {
                        continue;
                    }
                    if !visiting.insert(id.clone()) {
                        return Err(invalid("cycle in original operation history"));
                    }
                    let operation = self.op_store.read_operation(&id).await?;
                    let parents = operation.parents.clone();
                    pending.push(Pending::Finish(id, Box::new(operation)));
                    pending.extend(parents.into_iter().rev().map(Pending::Visit));
                }
                Pending::Finish(id, operation) => {
                    let parents = operation
                        .parents
                        .iter()
                        .map(|parent| self.normalized[parent].clone())
                        .collect::<Vec<_>>();
                    let view =
                        SharedView::from_view(&self.op_store.read_view(&operation.view_id).await?);
                    let shared_id = if let Some(value) = operation
                        .metadata
                        .attributes
                        .get(INTEGRATION_ANCHOR_ATTRIBUTE)
                    {
                        let anchor = SharedEventId::try_from_hex(value)
                            .filter(|id| id.as_bytes().len() == 64)
                            .ok_or_else(|| invalid("invalid integration anchor"))?;
                        let applied = self.anchors.read(&anchor)?;
                        if parents.len() != 1 {
                            return Err(invalid("integration does not include its local baseline"));
                        }
                        if !self.anchors.is_ancestor(&parents[0], &anchor)? {
                            if !self.rewritten_history {
                                return Err(invalid(
                                    "integration does not include its local baseline",
                                ));
                            }
                            // op abandon retains attributes when reparenting
                            // history, but the attribute alone cannot establish
                            // that this operation previously applied the anchor.
                            // Trusted checkpoint/frontier state retains pruned
                            // knowledge independently of this authored event.
                            self.anchors.write(&SharedEvent::new(
                                parents,
                                view,
                                Some(id.clone()),
                            ))?
                        } else if applied.view == view {
                            anchor
                        } else {
                            // Checkout/export finalization can add real shared effects.
                            self.anchors.write(&SharedEvent::new(
                                vec![anchor],
                                view,
                                Some(id.clone()),
                            ))?
                        }
                    } else {
                        // Keep authored events even if their shared state is unchanged.
                        self.anchors
                            .write(&SharedEvent::new(parents, view, Some(id.clone())))?
                    };
                    visiting.remove(&id);
                    self.normalized.insert(id, shared_id);
                }
            }
        }
        Ok(self.normalized[head].clone())
    }

    /// Records the current state causally after the last accepted exchange.
    ///
    /// `jj op abandon` can remove the old normalization boundary from active
    /// raw ancestry. In that case retain the prior shared history, while making
    /// the rewritten current state the new causal checkpoint. This does not
    /// request removal of peer history.
    pub async fn normalize_current(
        &mut self,
        head: &OperationId,
        previous: Option<(&OperationId, &SharedEventId)>,
    ) -> SyncResult<SharedEventId> {
        if let Some((previous_raw, previous_anchor)) = previous {
            self.reuse_boundary(previous_raw, previous_anchor).await?;
            if self.raw_is_ancestor(previous_raw, head).await? {
                return self.normalize(head).await;
            }
        } else {
            return self.normalize(head).await;
        }
        let (previous_raw, previous_anchor) = previous.unwrap();
        self.normalized.remove(previous_raw);
        self.rewritten_history = true;
        let current = self.normalize(head).await;
        self.rewritten_history = false;
        let current = current?;
        self.anchors.read(previous_anchor)?;
        let view = self.anchors.read(&current)?.view;
        let checkpoint = self.anchors.write(&SharedEvent::new(
            self.anchors.heads(&[previous_anchor.clone(), current])?,
            view,
            Some(head.clone()),
        ))?;
        self.normalized.insert(head.clone(), checkpoint.clone());
        Ok(checkpoint)
    }

    /// Returns the mapping produced by this pass for durable tracking.
    pub fn mappings(&self) -> &BTreeMap<OperationId, SharedEventId> {
        &self.normalized
    }

    async fn raw_is_ancestor(
        &self,
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
                pending.extend(self.op_store.read_operation(&id).await?.parents);
            }
        }
        Ok(false)
    }
}

fn invalid(message: impl Into<String>) -> SyncError {
    SyncError::InvalidData(message.into())
}

#[cfg(test)]
mod tests;
