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
use crate::lock::FileLockError;
use crate::object_id::ObjectId as _;
use crate::op_heads_store::OpHeadsStoreError;
use crate::op_store::OpStore;
use crate::op_store::OpStoreError;
use crate::op_store::OperationId;
use crate::repo::ReadonlyRepo;
use crate::repo::Repo as _;
use crate::repo_lock::RepoFormatLock;
use crate::simple_op_store::SimpleOpStore;

/// Failure to capture or finalize one local synchronization endpoint.
#[derive(Debug, Error)]
pub enum EndpointError {
    /// The operation-store format could not be locked.
    #[error(transparent)]
    RepositoryLock(#[from] FileLockError),
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

/// A frozen ordinary operation frontier for the native synchronization format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeCapture {
    /// Ordinary operation roots, including all published imported history.
    pub raw_heads: Vec<OperationId>,
    /// The receiver's operation after ordinary working-copy snapshotting.
    pub current_raw: OperationId,
}

/// Exclusive access while a deployed legacy synchronization store is migrated.
pub struct NativeUpgrade {
    repo_path: PathBuf,
    _lock: RepoFormatLock,
}

impl NativeUpgrade {
    /// Publishes the marker only after native history and state are durable.
    pub fn finish(self) -> EndpointResult<()> {
        let state = SyncStateStore::load(&self.repo_path).read()?;
        if !state.is_native() {
            return Err(
                SyncError::InvalidData("native migration has not completed".to_owned()).into(),
            );
        }
        write_format_marker(&self.repo_path, SimpleOpStore::NATIVE_SYNC_NAME)?;
        Ok(())
    }
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

    /// Enables native synchronization, or leaves a deployed legacy store ready
    /// for migration. Existing plain-format handles must be dropped first.
    pub fn enable_native(repo_path: &Path) -> EndpointResult<Self> {
        let _format_lock = RepoFormatLock::exclusive(repo_path)?;
        let current = fs::read_to_string(repo_path.join("op_store/type"))?;
        match current.as_str() {
            SimpleOpStore::NAME => {
                let state = SyncStateStore::init(repo_path)?;
                let mut locked = state.lock()?;
                locked.state_mut().finish_native_migration();
                locked.save()?;
                write_format_marker(repo_path, SimpleOpStore::NATIVE_SYNC_NAME)?;
            }
            SimpleOpStore::SYNC_NAME => {
                // Do not discard legacy roots: the caller first publishes them
                // as ordinary operation ancestry under a NativeUpgrade guard.
                SyncStateStore::load(repo_path).read()?;
            }
            SimpleOpStore::NATIVE_SYNC_NAME => {
                if !SyncStateStore::load(repo_path).read()?.is_native() {
                    return Err(SyncError::InvalidData(
                        "native store has legacy synchronization state".to_owned(),
                    )
                    .into());
                }
            }
            _ => return Err(EndpointError::WrongRepository),
        }
        Ok(Self::load(repo_path))
    }

    /// Serializes migration of a deployed legacy synchronization format.
    pub fn begin_native_upgrade(&self) -> EndpointResult<NativeUpgrade> {
        Ok(NativeUpgrade {
            repo_path: self.repo_path.clone(),
            _lock: RepoFormatLock::exclusive(&self.repo_path)?,
        })
    }

    /// Captures and temporarily pins ordinary history without creating a second
    /// causal graph. Native pruning has its usual operation-log semantics.
    pub async fn capture_native(
        &self,
        repo: &Arc<ReadonlyRepo>,
        transfer: &str,
    ) -> EndpointResult<NativeCapture> {
        self.check_repository(repo)?;
        let mut locked = self.state.lock()?;
        if !locked.state().is_native() {
            return Err(EndpointError::NotEnabled);
        }
        ensure_current(repo).await?;
        let capture = NativeCapture {
            // Pure receiver wrappers carry only local workspace observations.
            // Advertise the ordinary authored result they already represent.
            raw_heads: super::native::frontier(
                repo.op_store().as_ref(),
                std::slice::from_ref(repo.op_id()),
            )
            .await?,
            current_raw: repo.op_id().clone(),
        };
        if capture.raw_heads.len() != 1 {
            return Err(SyncError::InvalidData(
                "one operation must represent one authored result".to_owned(),
            )
            .into());
        }
        let mut pinned = capture.raw_heads.clone();
        pinned.push(capture.current_raw.clone());
        locked
            .state_mut()
            .pin_transfer(transfer.to_owned(), roots(&pinned, &[]))?;
        locked.save()?;
        Ok(capture)
    }

    /// Computes and pins an unpublished ordinary merge operation. Its exact
    /// commits can be sent to the other receiver before either side publishes.
    pub async fn reconcile_native(
        &self,
        repo: &Arc<ReadonlyRepo>,
        incoming: &[OperationId],
        transfer: &str,
    ) -> EndpointResult<OperationId> {
        self.check_repository(repo)?;
        let mut locked = self.state.lock()?;
        let mut heads = vec![repo.op_id().clone()];
        heads.extend_from_slice(incoming);
        let selected = super::native::reconcile(repo, &heads).await?;
        locked.state_mut().pin_transfer(
            transfer.to_owned(),
            roots(std::slice::from_ref(&selected), &[]),
        )?;
        locked.save()?;
        Ok(selected)
    }

    /// Installs validated incoming native objects and pins their ordinary roots
    /// atomically with respect to collection. The session releases this pin.
    #[cfg(feature = "git")]
    pub fn install_native(
        &self,
        repo: &Arc<ReadonlyRepo>,
        quarantine: &super::objects::Quarantine,
        inventory: &super::objects::Inventory,
        transfer: &str,
    ) -> EndpointResult<()> {
        self.check_repository(repo)?;
        let mut locked = self.state.lock()?;
        let operations = repo
            .op_store()
            .downcast_ref::<SimpleOpStore>()
            .ok_or(EndpointError::WrongRepository)?;
        let backend = crate::git::get_git_backend(repo.store())
            .map_err(|_| EndpointError::WrongRepository)?;
        let objects = super::objects::RepositoryObjects::new(operations, &self.anchors, backend)?;
        quarantine.install(&objects, inventory)?;
        locked.state_mut().pin_transfer(
            transfer.to_owned(),
            roots(
                &inventory.raw_heads.iter().cloned().collect::<Vec<_>>(),
                &[],
            ),
        )?;
        locked.save()?;
        Ok(())
    }

    /// Enables synchronization after waiting for plain operation-store users.
    ///
    /// Drop this process's existing repository handles before calling: loaders
    /// opened with the plain format keep shared access until their last clone
    /// is dropped. New loaders wait until the transition has finished. Binaries
    /// predating this lock do not participate, so their already-open handles
    /// must finish before upgrading. No active operation head is changed.
    pub fn enable(repo_path: &Path) -> EndpointResult<Self> {
        let _format_lock = RepoFormatLock::exclusive(repo_path)?;
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

    /// Owns one transfer's temporary roots until its network work has stopped.
    /// Acquire this before capture; the lease is separate from the short state
    /// writer lock and can safely remain held during network waits.
    pub fn acquire_transfer_lease(&self, transfer: &str) -> EndpointResult<FileLock> {
        let owner = transfer_owner(transfer).ok_or_else(|| {
            SyncError::InvalidData("invalid synchronization transfer owner".to_owned())
        })?;
        self.acquire_session_lease(&format!("transfer-{owner}"))
    }

    /// Releases temporary native roots left by transfers that no longer run.
    ///
    /// A nonblocking lease check excludes live transfers, including transfers
    /// that have not yet established their first pin. Unfinished publication
    /// receipts retain their own roots independently. Legacy histories keep
    /// their deployed retention semantics until native migration completes.
    pub fn retire_inactive_native_transfers(&self) -> EndpointResult<()> {
        let snapshot = self.state.read()?;
        if !snapshot.is_native() {
            return Ok(());
        }
        let owners = snapshot
            .transfer_pins
            .keys()
            .filter_map(|key| transfer_owner(key).map(str::to_owned))
            .collect::<BTreeSet<_>>();
        for owner in owners {
            let Some(_lease) =
                FileLock::try_lock(self.session_lease_path(&format!("transfer-{owner}")))
                    .map_err(SyncStateError::from)?
            else {
                continue;
            };
            let mut locked = self.state.lock()?;
            if !locked.state().is_native() {
                return Ok(());
            }
            let prefix = format!("{owner}-");
            locked
                .state_mut()
                .transfer_pins
                .retain(|key, _| key != &owner && !key.starts_with(&prefix));
            locked.save()?;
        }
        Ok(())
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

    /// Pins all retained history for a frozen receiver comparison basis.
    /// This includes imported native operations outside the current causal tip.
    pub fn pin_retained(
        &self,
        transfer: &str,
    ) -> EndpointResult<(Vec<OperationId>, Vec<SharedEventId>)> {
        let mut locked = self.state.lock()?;
        let retained = locked.state().gc_roots();
        let raw = retained
            .raw_operations
            .iter()
            .map(|id| parse_raw_id(id))
            .collect::<EndpointResult<Vec<_>>>()?;
        let shared = retained
            .shared_events
            .iter()
            .map(|id| parse_shared_id(id))
            .collect::<EndpointResult<Vec<_>>>()?;
        locked
            .state_mut()
            .pin_transfer(transfer.to_owned(), roots(&raw, &shared))?;
        locked.save()?;
        Ok((raw, shared))
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
        let objects = super::objects::RepositoryObjects::new(operations, &self.anchors, backend)?;
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
        if !state.is_native() {
            state.retain_peer(
                &receipt.peer,
                PeerState {
                    raw_heads: receipt.incoming_raw_heads,
                    shared_heads: receipt.incoming_shared_heads,
                },
            );
            state.shared_anchors.extend(receipt.applied_anchor);
        }
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
        if !matches!(
            fs::read_to_string(self.repo_path.join("op_store/type"))?.as_str(),
            SimpleOpStore::SYNC_NAME | SimpleOpStore::NATIVE_SYNC_NAME
        ) {
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

fn write_format_marker(repo_path: &Path, name: &str) -> EndpointResult<()> {
    let marker = repo_path.join("op_store/type");
    let permissions = fs::metadata(&marker)?.permissions();
    let mut temporary = NamedTempFile::new_in(marker.parent().unwrap())?;
    temporary.write_all(name.as_bytes())?;
    temporary.as_file().set_permissions(permissions)?;
    temporary.as_file().sync_all()?;
    persist_temp_file(temporary, &marker)?;
    #[cfg(unix)]
    fs::File::open(marker.parent().unwrap())?.sync_all()?;
    Ok(())
}

fn roots(raw: &[OperationId], shared: &[SharedEventId]) -> PeerState {
    PeerState {
        raw_heads: raw.iter().map(|id| id.hex()).collect(),
        shared_heads: shared.iter().map(|id| id.hex()).collect(),
    }
}

fn transfer_owner(transfer: &str) -> Option<&str> {
    let owner = transfer.split('-').next()?;
    (owner.len() == 32
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(owner)
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
        fs::create_dir(path.join("store"))?;
        fs::write(path.join("store/type"), b"git")?;
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
        let endpoint = Endpoint::enable(directory.path())?;
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
        assert_eq!(Endpoint::enable(directory.path())?.identity()?, identity);
        Ok(())
    }

    #[test]
    fn enabled_missing_state_is_not_reinitialized() -> TestResult {
        let directory = new_temp_dir();
        make_repository(directory.path(), SimpleOpStore::SYNC_NAME)?;
        assert!(Endpoint::enable(directory.path()).is_err());
        assert!(!directory.path().join("sync").exists());
        Ok(())
    }

    #[test]
    fn enabling_rejects_an_unrecognized_store_without_initializing_sync() -> TestResult {
        let directory = new_temp_dir();
        make_repository(directory.path(), "other backend")?;
        assert!(matches!(
            Endpoint::enable(directory.path()),
            Err(EndpointError::WrongRepository)
        ));
        assert!(!directory.path().join("sync").exists());
        Ok(())
    }

    #[test]
    fn transfer_pins_survive_reopen_and_release_after_peer_retention() -> TestResult {
        let directory = new_temp_dir();
        make_repository(directory.path(), SimpleOpStore::NAME)?;
        let endpoint = Endpoint::enable(directory.path())?;
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

    #[test]
    fn inactive_native_transfer_pins_retire_but_live_transfers_and_wal_remain() -> TestResult {
        let directory = new_temp_dir();
        make_repository(directory.path(), SimpleOpStore::NATIVE_SYNC_NAME)?;
        let state_store = SyncStateStore::init(directory.path())?;
        let endpoint = Endpoint::load(directory.path());
        let active = "1".repeat(32);
        let abandoned = "2".repeat(32);
        let active_root = OperationId::from_bytes(&[1; 64]);
        let abandoned_root = OperationId::from_bytes(&[2; 64]);
        let wal_root = OperationId::from_bytes(&[3; 64]);
        {
            let mut locked = state_store.lock()?;
            let state = locked.state_mut();
            state.finish_native_migration();
            for key in [active.clone(), format!("{active}-incoming")] {
                state.pin_transfer(key, roots(std::slice::from_ref(&active_root), &[]))?;
            }
            for key in [abandoned.clone(), format!("{abandoned}-selected")] {
                state.pin_transfer(key, roots(std::slice::from_ref(&abandoned_root), &[]))?;
            }
            state.begin_session(
                "pending-publication".to_owned(),
                super::super::state::SyncSession {
                    peer: "peer".to_owned(),
                    baseline_operation: wal_root.hex(),
                    incoming_raw_heads: BTreeSet::from([abandoned_root.hex()]),
                    incoming_shared_heads: BTreeSet::new(),
                    applied_anchor: None,
                    result_operation: None,
                    phase: SessionPhase::Pending,
                },
            )?;
            locked.save()?;
        }
        // A subphase key must take the owner's lease, just like retirement.
        let lease = endpoint.acquire_transfer_lease(&format!("{active}-incoming"))?;
        endpoint.retire_inactive_native_transfers()?;
        let state = state_store.read()?;
        assert_eq!(
            state.transfer_pins.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([active.clone(), format!("{active}-incoming")])
        );
        assert!(state.gc_roots().raw_operations.contains(&active_root.hex()));
        assert!(
            state
                .gc_roots()
                .raw_operations
                .contains(&abandoned_root.hex())
        );
        assert!(state.gc_roots().raw_operations.contains(&wal_root.hex()));
        drop(lease);
        endpoint.retire_inactive_native_transfers()?;
        let state = state_store.read()?;
        assert!(state.transfer_pins.is_empty());
        assert_eq!(
            state.gc_roots().raw_operations,
            BTreeSet::from([abandoned_root.hex(), wal_root.hex()])
        );
        Ok(())
    }

    #[test]
    fn inactive_transfer_retirement_does_not_change_legacy_roots() -> TestResult {
        let directory = new_temp_dir();
        make_repository(directory.path(), SimpleOpStore::NAME)?;
        let endpoint = Endpoint::enable(directory.path())?;
        endpoint.pin_transfer(
            &"a".repeat(32),
            &[OperationId::from_bytes(&[1; 64])],
            &[SharedEventId::from_bytes(&[2; 64])],
        )?;
        let before = endpoint.state_store().read()?;
        endpoint.retire_inactive_native_transfers()?;
        assert_eq!(endpoint.state_store().read()?, before);
        Ok(())
    }
}
