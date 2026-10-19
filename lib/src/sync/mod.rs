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

//! Repository synchronization without sharing live workspace state.

pub mod anchor;
pub mod endpoint;
pub mod gc;
#[cfg(feature = "git")]
pub mod metadata;
#[cfg(feature = "git")]
mod metadata_limits;
pub mod normalize;
#[cfg(feature = "git")]
pub mod objects;
#[cfg(feature = "git")]
pub mod packed;
pub mod protocol;
pub mod reconcile;
pub mod shared;
pub mod state;

use thiserror::Error;

use crate::op_store::OpStoreError;

/// Failure to validate or persist synchronized history.
#[derive(Debug, Error)]
pub enum SyncError {
    /// An operation or view could not be read or written.
    #[error(transparent)]
    OpStore(#[from] OpStoreError),
    /// Repository history could not be loaded or reconciled.
    #[error(transparent)]
    Repository(#[from] crate::repo::RepoLoaderError),
    /// Referenced commit data could not be loaded or rewritten.
    #[error(transparent)]
    Backend(#[from] crate::backend::BackendError),
    /// Commit ancestry could not be indexed.
    #[error(transparent)]
    Index(#[from] crate::index::IndexError),
    /// A synchronization record could not be read or written.
    #[error("Failed to access synchronization storage")]
    Io(#[from] std::io::Error),
    /// Incoming history or local tracking data is inconsistent.
    #[error("Invalid synchronization data: {0}")]
    InvalidData(String),
}

/// Result of a synchronization storage or graph operation.
pub type SyncResult<T> = Result<T, SyncError>;
