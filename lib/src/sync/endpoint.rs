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

//! Receiver-local capture, retention, and normalization boundaries.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use tempfile::NamedTempFile;
use thiserror::Error;

use super::SyncError;
use super::anchor::AnchorStore;
use super::normalize::Normalizer;
use super::reconcile::Reconciler;
use super::shared::SharedEvent;
use super::shared::SharedEventId;
use super::shared::SharedView;
use super::state::LocalAnchor;
use super::state::LockedSyncState;
use super::state::PeerState;
use super::state::SessionPhase;
use super::state::SessionRecovery;
use super::state::SyncStateError;
use super::state::SyncStateStore;
use crate::file_util::persist_temp_file;
use crate::lock::FileLock;
use crate::object_id::ObjectId as _;
use crate::op_heads_store::OpHeadsStoreError;
use crate::op_store::OpStore;
use crate::op_store::OpStoreError;
use crate::op_store::OperationId;
use crate::repo::ReadonlyRepo;
use crate::repo::Repo as _;
use crate::simple_op_store::SimpleOpStore;

/// Failure to capture or finalize one local synchronization endpoint.
#[derive(Debug, Error)]
pub enum EndpointError {
    /// Shared history or referenced data is invalid.
    #[error(transparent)]
    Sync(#[from] SyncError),
    /// Durable tracking state could not be read or updated.
    #[error(transparent)]
    State(#[from] SyncStateError),
    /// Original operations could not be read.
    #[error(transparent)]
    Operation(#[from] OpStoreError),
    /// Current operation heads could not be read.
    #[error(transparent)]
    Heads(#[from] OpHeadsStoreError),
    /// Filesystem access failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The caller must refresh/snapshot the current repository and retry.
    #[error("Repository changed while capturing synchronization state; refresh and retry")]
    StaleCapture,
    /// The explicit format transition has not occurred.
    #[error("Repository synchronization has not been enabled")]
    NotEnabled,
    /// The supplied repository and endpoint do not address the same stores.
    #[error("Synchronization endpoint does not match the repository's operation store")]
    WrongRepository,
}

/// Result from one receiver-local endpoint action.
pub type EndpointResult<T> = Result<T, EndpointError>;

/// An immutable, durably pinned advertisement after ordinary local snapshotting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capture {
    /// Frozen original history frontier.
    pub raw_heads: Vec<OperationId>,
    /// Frozen shared causal frontier.
    pub shared_heads: Vec<SharedEventId>,
    /// Actual raw operation normalized at capture time.
    pub current_raw: OperationId,
    /// Its actual projected state and causal knowledge.
    pub current_shared: SharedEventId,
}

/// Stores belonging to one repository; no lock survives a method call.
pub struct Endpoint {
    repo_path: PathBuf,
    anchors: AnchorStore,
    state: SyncStateStore,
}

impl Endpoint {
    /// Addresses existing storage without initializing missing state.
    pub fn load(repo_path: &Path) -> Self {
        Self {
            repo_path: repo_path.to_owned(),
            anchors: AnchorStore::load(repo_path),
            state: SyncStateStore::load(repo_path),
        }
    }

    /// Enables the format after the caller has stopped other repository processes.
    ///
    /// Quiescence is a precondition, not something this local lock can prove.
    /// The marker rejects future old-binary opens; it cannot revoke a repository
    /// already opened by an older process. No active operation head is changed.
    pub fn enable_quiescent(repo_path: &Path) -> EndpointResult<Self> {
        let marker = repo_path.join("op_store/type");
        let current = fs::read_to_string(&marker)?;
        if current == SimpleOpStore::SYNC_NAME {
            let endpoint = Self::load(repo_path);
            endpoint.state.read()?;
            if !repo_path.join("sync/events").is_dir() {
                return Err(SyncError::InvalidData(
                    "enabled repository has no shared-event store".to_owned(),
                )
                .into());
            }
            return Ok(endpoint);
        }
        if current != SimpleOpStore::NAME {
            return Err(EndpointError::WrongRepository);
        }
        let state = SyncStateStore::init(repo_path)?;
        AnchorStore::init(repo_path)?;
        let _guard = state.lock()?;
        #[cfg(unix)]
        {
            fs::File::open(repo_path.join("sync/events"))?.sync_all()?;
            fs::File::open(repo_path.join("sync"))?.sync_all()?;
        }
        let permissions = fs::metadata(&marker)?.permissions();
        let mut temporary = NamedTempFile::new_in(marker.parent().unwrap())?;
        temporary.write_all(SimpleOpStore::SYNC_NAME.as_bytes())?;
        temporary.as_file().set_permissions(permissions)?;
        persist_temp_file(temporary, &marker)?;
        #[cfg(unix)]
        fs::File::open(marker.parent().unwrap())?.sync_all()?;
        Ok(Self::load(repo_path))
    }

    /// Shared immutable history storage for object transfer and reconciliation.
    pub fn anchors(&self) -> &AnchorStore {
        &self.anchors
    }

    /// Tracking/WAL storage for callers coordinating native publication.
    pub fn state_store(&self) -> &SyncStateStore {
        &self.state
    }

    /// Opaque, persistent identity for peer tracking, never a relatedness check.
    pub fn identity(&self) -> EndpointResult<String> {
        Ok(self.state.read()?.repository_id)
    }

    /// Excludes retirement from before receipt preparation through publication.
    /// Publishers acquire this lease before creating a pending session, and keep
    /// it while ordinary finalization runs without holding the state lock.
    pub fn acquire_session_lease(&self, session: &str) -> EndpointResult<FileLock> {
        Ok(FileLock::lock(self.session_lease_path(session)).map_err(SyncStateError::from)?)
    }

    /// Captures the actual current raw operation, never an advertised cached tip.
    ///
    /// The caller performs ordinary workspace snapshotting first. Multiple or
    /// changed active heads require refresh/retry, before history rewriting can
    /// be inferred. The durable pin lets the network run after releasing the lock.
    pub async fn capture(
        &self,
        repo: &Arc<ReadonlyRepo>,
        transfer: &str,
    ) -> EndpointResult<Capture> {
        self.check_repository(repo)?;
        let mut locked = self.state.lock()?;
        ensure_current(repo).await?;
        let previous = locked
            .state()
            .local_anchor
            .as_ref()
            .map(parse_local_anchor)
            .transpose()?;
        let mut normalizer = Normalizer::new(repo.op_store().as_ref(), &self.anchors);
        if !locked.state().pending_pruned_shared_heads.is_empty() {
            normalizer.allow_reparented_integrations();
        }
        let mut shared = normalizer
            .normalize_current(
                repo.op_id(),
                previous.as_ref().map(|(raw, shared)| (raw, shared)),
            )
            .await?;
        let mut retained = locked
            .state()
            .pending_pruned_shared_heads
            .iter()
            .map(|value| parse_shared_id(value))
            .collect::<EndpointResult<Vec<_>>>()?;
        if !retained.is_empty() {
            retained.push(shared.clone());
            let parents = self.anchors.heads(&retained)?;
            if parents.as_slice() != std::slice::from_ref(&shared) {
                let view = self.anchors.read(&shared)?.view;
                shared = self.anchors.write(&SharedEvent::new(
                    parents,
                    view,
                    Some(repo.op_id().clone()),
                ))?;
            }
        }
        ensure_current(repo).await?;
        let capture = Capture {
            raw_heads: vec![repo.op_id().clone()],
            shared_heads: vec![shared.clone()],
            current_raw: repo.op_id().clone(),
            current_shared: shared.clone(),
        };
        let state = locked.state_mut();
        state.pin_transfer(
            transfer.to_owned(),
            roots(&capture.raw_heads, &capture.shared_heads),
        )?;
        state.local_anchor = Some(LocalAnchor {
            raw_head: repo.op_id().hex(),
            shared_anchor: shared.hex(),
        });
        state.shared_anchors.insert(shared.hex());
        for (raw, shared) in normalizer.mappings() {
            state.normalized_operations.insert(raw.hex(), shared.hex());
        }
        state
            .normalized_operations
            .insert(repo.op_id().hex(), shared.hex());
        state.pending_pruned_shared_heads.clear();
        locked.save()?;
        Ok(capture)
    }

    /// Retains actual current knowledge before native operation-history pruning.
    ///
    /// This needs only immutable operation/view stores. Keep the returned GC lock
    /// through reparenting and head publication, then drop it before taking any
    /// working-copy lock. No network or backend access occurs here.
    pub async fn preserve_pruned_heads(
        &self,
        operations: &dyn OpStore,
        raw_heads: &[OperationId],
    ) -> EndpointResult<LockedSyncState<'_>> {
        self.check_operation_store(operations)?;
        if raw_heads.is_empty() {
            return Err(SyncError::InvalidData(
                "cannot preserve an empty operation frontier".to_owned(),
            )
            .into());
        }
        let mut locked = self.state.lock()?;
        let previous = locked
            .state()
            .local_anchor
            .as_ref()
            .map(parse_local_anchor)
            .transpose()?;
        let mut normalizer = Normalizer::new(operations, &self.anchors);
        // This is an explicit native-pruning boundary, including branches
        // revisiting integrations reparented by an earlier completed prune.
        normalizer.allow_reparented_integrations();
        let mut shared_heads = Vec::new();
        if let [head] = raw_heads {
            shared_heads.push(
                normalizer
                    .normalize_current(head, previous.as_ref().map(|(raw, shared)| (raw, shared)))
                    .await?,
            );
        } else {
            if let Some((raw, shared)) = &previous {
                normalizer.reuse_boundary(raw, shared).await?;
                shared_heads.push(shared.clone());
            }
            for head in raw_heads {
                shared_heads.push(normalizer.normalize(head).await?);
            }
        }
        let state = locked.state_mut();
        state
            .pending_pruned_shared_heads
            .extend(shared_heads.iter().map(|id| id.hex()));
        for (raw, shared) in normalizer.mappings() {
            state.normalized_operations.insert(raw.hex(), shared.hex());
        }
        locked.save()?;
        Ok(locked)
    }

    /// Pins already present immutable roots; never use this for absent incoming data.
    pub fn pin_transfer(
        &self,
        transfer: &str,
        raw_heads: &[OperationId],
        shared_heads: &[SharedEventId],
    ) -> EndpointResult<()> {
        let mut locked = self.state.lock()?;
        locked
            .state_mut()
            .pin_transfer(transfer.to_owned(), roots(raw_heads, shared_heads))?;
        locked.save()?;
        Ok(())
    }

    /// Releases a finished transfer's temporary roots after acknowledgment/retention.
    pub fn release_transfer(&self, transfer: &str) -> EndpointResult<()> {
        let mut locked = self.state.lock()?;
        locked.state_mut().transfer_pins.remove(transfer);
        locked.save()?;
        Ok(())
    }

    /// Retains installed histories; local pruning never requests peer deletion.
    pub fn retain_installed(
        &self,
        peer: &str,
        raw_heads: &[OperationId],
        shared_heads: &[SharedEventId],
    ) -> EndpointResult<()> {
        let mut locked = self.state.lock()?;
        locked
            .state_mut()
            .retain_peer(peer, roots(raw_heads, shared_heads));
        locked.save()?;
        Ok(())
    }

    /// Installs immutable objects and retains their roots under one local GC lock.
    #[cfg(feature = "git")]
    pub fn install_and_retain(
        &self,
        repo: &Arc<ReadonlyRepo>,
        quarantine: &super::objects::Quarantine,
        inventory: &super::objects::Inventory,
        peer: &str,
    ) -> EndpointResult<()> {
        self.check_repository(repo)?;
        let mut locked = self.state.lock()?;
        let operations = repo
            .op_store()
            .downcast_ref::<SimpleOpStore>()
            .ok_or(EndpointError::WrongRepository)?;
        let backend = crate::git::get_git_backend(repo.store())
            .map_err(|_| EndpointError::WrongRepository)?;
        let objects = super::objects::RepositoryObjects::new(operations, &self.anchors, backend);
        quarantine.install(&objects, inventory)?;
        locked.state_mut().retain_peer(
            peer,
            roots(
                &inventory.raw_heads.iter().cloned().collect::<Vec<_>>(),
                &inventory.shared_heads.iter().cloned().collect::<Vec<_>>(),
            ),
        );
        locked.save()?;
        Ok(())
    }

    /// Computes a shared result while keeping local workspace/Git maps out of it.
    pub async fn reconcile(
        &self,
        repo: &Arc<ReadonlyRepo>,
        current: &SharedEventId,
        incoming: &[SharedEventId],
    ) -> EndpointResult<SharedEventId> {
        self.check_repository(repo)?;
        let mut locked = self.state.lock()?;
        let mut frontiers = incoming.to_vec();
        frontiers.push(current.clone());
        let result = Reconciler::new(repo, &self.anchors)
            .reconcile(&frontiers)
            .await?;
        locked.state_mut().shared_anchors.insert(result.hex());
        locked.save()?;
        Ok(result)
    }

    /// Records a verified immutable boundary after publication/finalization.
    pub async fn record_local_anchor(
        &self,
        repo: &Arc<ReadonlyRepo>,
        raw: &OperationId,
        shared: &SharedEventId,
    ) -> EndpointResult<()> {
        self.check_repository(repo)?;
        let mut locked = self.state.lock()?;
        let operation = repo.op_store().read_operation(raw).await?;
        let view = repo.op_store().read_view(&operation.view_id).await?;
        if self.anchors.read(shared)?.view != SharedView::from_view(&view) {
            return Err(SyncError::InvalidData(
                "local anchor does not represent its original operation".to_owned(),
            )
            .into());
        }
        locked.state_mut().local_anchor = Some(LocalAnchor {
            raw_head: raw.hex(),
            shared_anchor: shared.hex(),
        });
        locked.state_mut().shared_anchors.insert(shared.hex());
        locked.save()?;
        Ok(())
    }

    /// Completes already published WAL receipts before starting a new exchange.
    pub async fn recover_published(&self, repo: &Arc<ReadonlyRepo>) -> EndpointResult<Vec<String>> {
        self.check_repository(repo)?;
        let mut locked = self.state.lock()?;
        let heads = repo.loader().op_heads_store().get_op_heads().await?;
        let sessions = locked.state().sessions.clone();
        let mut completed = Vec::new();
        for (key, session) in sessions {
            if session.phase == SessionPhase::Completed {
                continue;
            }
            let published = if let Some(value) = &session.result_operation {
                published_contains(repo, &heads, &parse_raw_id(value)?).await?
            } else {
                false
            };
            if session.recovery(published)? == SessionRecovery::FinishPublished {
                let result = session.result_operation.as_ref().unwrap();
                locked.state_mut().record_publication(&key, result)?;
                locked.state_mut().complete_session(&key)?;
                completed.push(key);
            }
        }
        locked.save()?;
        Ok(completed)
    }

    /// Retires a pending receipt whose proposed result was never published.
    ///
    /// Publishers must hold the same state lock through WAL persistence and
    /// operation-head publication. Incoming histories and the selected anchor
    /// remain retained, allowing a fresh reconciliation after a failed attempt.
    /// Published receipts are completed by `recover_published` instead.
    /// A nonblocking process lease excludes live publishers, including the time
    /// before they acquire the publication lock. Live sessions are left intact.
    pub async fn retire_unpublished(
        &self,
        repo: &Arc<ReadonlyRepo>,
        session: &str,
    ) -> EndpointResult<bool> {
        self.check_repository(repo)?;
        let Some(_lease) =
            FileLock::try_lock(self.session_lease_path(session)).map_err(SyncStateError::from)?
        else {
            return Ok(false);
        };
        let mut locked = self.state.lock()?;
        let Some(receipt) = locked.state().sessions.get(session).cloned() else {
            return Ok(false);
        };
        if receipt.phase != SessionPhase::Pending {
            return Ok(false);
        }
        let heads = repo.loader().op_heads_store().get_op_heads().await?;
        if let Some(result) = &receipt.result_operation
            && published_contains(repo, &heads, &parse_raw_id(result)?).await?
        {
            return Ok(false);
        }
        let state = locked.state_mut();
        state.retain_peer(
            &receipt.peer,
            PeerState {
                raw_heads: receipt.incoming_raw_heads,
                shared_heads: receipt.incoming_shared_heads,
            },
        );
        state.shared_anchors.extend(receipt.applied_anchor);
        state.sessions.remove(session);
        locked.save()?;
        Ok(true)
    }

    fn session_lease_path(&self, session: &str) -> PathBuf {
        let digest = crate::content_hash::blake2b_hash(session.as_bytes());
        self.repo_path.join("sync").join(format!(
            "session-{}.lock",
            crate::hex_util::encode_hex(&digest)
        ))
    }

    fn check_repository(&self, repo: &ReadonlyRepo) -> EndpointResult<()> {
        self.check_operation_store(repo.op_store().as_ref())
    }

    fn check_operation_store(&self, operations: &dyn OpStore) -> EndpointResult<()> {
        let operations = operations
            .downcast_ref::<SimpleOpStore>()
            .ok_or(EndpointError::WrongRepository)?;
        if fs::canonicalize(operations.store_path())?
            != fs::canonicalize(self.repo_path.join("op_store"))?
        {
            return Err(EndpointError::WrongRepository);
        }
        if fs::read_to_string(self.repo_path.join("op_store/type"))? != SimpleOpStore::SYNC_NAME {
            return Err(EndpointError::NotEnabled);
        }
        Ok(())
    }
}

async fn ensure_current(repo: &ReadonlyRepo) -> EndpointResult<()> {
    let heads = repo.loader().op_heads_store().get_op_heads().await?;
    if heads.as_slice() != std::slice::from_ref(repo.op_id()) {
        return Err(EndpointError::StaleCapture);
    }
    Ok(())
}

fn roots(raw: &[OperationId], shared: &[SharedEventId]) -> PeerState {
    PeerState {
        raw_heads: raw.iter().map(|id| id.hex()).collect(),
        shared_heads: shared.iter().map(|id| id.hex()).collect(),
    }
}

fn parse_raw_id(value: &str) -> EndpointResult<OperationId> {
    OperationId::try_from_hex(value)
        .filter(|id| id.as_bytes().len() == 64)
        .ok_or_else(|| {
            SyncError::InvalidData("invalid original operation boundary".to_owned()).into()
        })
}

fn parse_local_anchor(anchor: &LocalAnchor) -> EndpointResult<(OperationId, SharedEventId)> {
    Ok((
        parse_raw_id(&anchor.raw_head)?,
        parse_shared_id(&anchor.shared_anchor)?,
    ))
}

fn parse_shared_id(value: &str) -> EndpointResult<SharedEventId> {
    SharedEventId::try_from_hex(value)
        .filter(|id| id.as_bytes().len() == 64)
        .ok_or_else(|| {
            SyncError::InvalidData("invalid shared operation boundary".to_owned()).into()
        })
}

async fn published_contains(
    repo: &ReadonlyRepo,
    heads: &[OperationId],
    target: &OperationId,
) -> EndpointResult<bool> {
    let mut pending = heads.to_vec();
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if &id == target {
            return Ok(true);
        }
        if visited.insert(id.clone()) {
            pending.extend(repo.op_store().read_operation(&id).await?.parents);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestResult;
    use crate::tests::new_temp_dir;

    fn make_repository(path: &Path, marker: &str) -> std::io::Result<()> {
        fs::create_dir(path.join("op_store"))?;
        fs::write(path.join("op_store/type"), marker)?;
        fs::create_dir(path.join("op_heads"))?;
        fs::write(path.join("op_heads/local-head"), b"original active head")
    }

    #[test]
    fn enabling_is_explicit_durable_and_idempotent() -> TestResult {
        let directory = new_temp_dir();
        make_repository(directory.path(), SimpleOpStore::NAME)?;
        assert!(Endpoint::load(directory.path()).identity().is_err());
        let endpoint = Endpoint::enable_quiescent(directory.path())?;
        let identity = endpoint.identity()?;
        assert_eq!(identity.len(), 32);
        assert_eq!(
            fs::read_to_string(directory.path().join("op_store/type"))?,
            SimpleOpStore::SYNC_NAME
        );
        assert!(directory.path().join("sync/events").is_dir());
        assert_eq!(
            fs::read(directory.path().join("op_heads/local-head"))?,
            b"original active head"
        );
        assert_eq!(
            Endpoint::enable_quiescent(directory.path())?.identity()?,
            identity
        );
        Ok(())
    }

    #[test]
    fn enabled_missing_state_is_not_reinitialized() -> TestResult {
        let directory = new_temp_dir();
        make_repository(directory.path(), SimpleOpStore::SYNC_NAME)?;
        assert!(Endpoint::enable_quiescent(directory.path()).is_err());
        assert!(!directory.path().join("sync").exists());
        Ok(())
    }

    #[test]
    fn enabling_rejects_an_unrecognized_store_without_writes() -> TestResult {
        let directory = new_temp_dir();
        make_repository(directory.path(), "other backend")?;
        assert!(matches!(
            Endpoint::enable_quiescent(directory.path()),
            Err(EndpointError::WrongRepository)
        ));
        assert!(!directory.path().join("sync").exists());
        Ok(())
    }

    #[test]
    fn transfer_pins_survive_reopen_and_release_after_peer_retention() -> TestResult {
        let directory = new_temp_dir();
        make_repository(directory.path(), SimpleOpStore::NAME)?;
        let endpoint = Endpoint::enable_quiescent(directory.path())?;
        let raw = OperationId::new(vec![1; 64]);
        let shared = SharedEventId::new(vec![2; 64]);
        endpoint.pin_transfer(
            "outgoing",
            std::slice::from_ref(&raw),
            std::slice::from_ref(&shared),
        )?;
        let reopened = Endpoint::load(directory.path());
        assert!(
            reopened
                .state_store()
                .read()?
                .gc_roots()
                .raw_operations
                .contains(&raw.hex())
        );
        reopened.retain_installed(
            "peer",
            std::slice::from_ref(&raw),
            std::slice::from_ref(&shared),
        )?;
        reopened.release_transfer("outgoing")?;
        let state = reopened.state_store().read()?;
        assert!(state.transfer_pins.is_empty());
        assert!(state.gc_roots().raw_operations.contains(&raw.hex()));
        assert!(state.gc_roots().shared_events.contains(&shared.hex()));
        assert_eq!(
            fs::read(directory.path().join("op_heads/local-head"))?,
            b"original active head"
        );
        Ok(())
    }
}
