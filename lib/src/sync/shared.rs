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

//! The synchronized projection of a view and its causal events.

use crate::content_hash::ContentHash;
use crate::content_hash::blake2b_hash;
use crate::object_id::ObjectId as _;
use crate::object_id::id_type;
use crate::op_store;
use crate::op_store::OperationId;

id_type!(
    /// Identity of a shared causal event, independent of any live workspace.
    pub SharedEventId { hex() }
);

/// Repository fields that may be applied to another independent clone.
///
/// Original views are retained separately. This projection deliberately omits
/// checked-out commits, Git HEADs, and observations of physical Git refs.
#[derive(Clone, Debug, Eq, PartialEq, ContentHash)]
pub struct SharedView(op_store::View);

impl SharedView {
    /// Projects a historical or live view without modifying it.
    pub fn from_view(view: &op_store::View) -> Self {
        let mut shared = view.clone();
        shared.wc_commit_ids.clear();
        shared.git_heads.clear();
        shared.git_refs.clear();
        Self(shared)
    }

    /// Returns the projected view with all local maps empty.
    pub fn as_view(&self) -> &op_store::View {
        &self.0
    }

    /// Combines shared fields with this receiver's current local maps.
    pub fn with_local_state(&self, local: &op_store::View) -> op_store::View {
        let mut view = self.0.clone();
        view.wc_commit_ids.clone_from(&local.wc_commit_ids);
        view.git_heads.clone_from(&local.git_heads);
        view.git_refs.clone_from(&local.git_refs);
        view
    }
}

/// A state and the shared events whose effects it incorporates.
///
/// `origin` identifies an original operation, including its metadata and commit
/// predecessors. It also distinguishes authored events that leave the same
/// state, such as an undo followed by a redo. Reconciliation events have no
/// originating operation and use sorted, distinct parents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SharedEvent {
    /// Direct causal parents, sorted and deduplicated.
    pub parents: Vec<SharedEventId>,
    /// Shared repository state after this event.
    pub view: SharedView,
    /// Original immutable operation, if this event represents one.
    pub origin: Option<OperationId>,
}

impl SharedEvent {
    /// Constructs an event with canonical parent order.
    pub fn new(
        mut parents: Vec<SharedEventId>,
        view: SharedView,
        origin: Option<OperationId>,
    ) -> Self {
        parents.sort_unstable();
        parents.dedup();
        Self {
            parents,
            view,
            origin,
        }
    }

    /// Computes a versioned, domain-separated content identity.
    pub fn id(&self) -> SharedEventId {
        SharedEventId::new(blake2b_hash(self).to_vec())
    }
}

impl ContentHash for SharedEvent {
    fn hash(&self, state: &mut impl crate::content_hash::DigestUpdate) {
        "jj-sync-shared-event".hash(state);
        1u32.hash(state);
        self.parents.hash(state);
        self.view.hash(state);
        self.origin.hash(state);
    }
}
