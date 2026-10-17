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

//! Durable, receiver-local synchronization tracking and publication receipts.
//!
//! This store does not update active operation heads or enable synchronization
//! on a repository. Enabling the operation-store format requires a separate,
//! quiescent transition; a tracking file cannot exclude already running readers.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::file_util::IoResultExt as _;
use crate::file_util::PathError;
use crate::file_util::persist_temp_file;
use crate::lock::FileLock;
use crate::lock::FileLockError;

const FORMAT_VERSION: u32 = 1;
const ID_HEX_LENGTH: usize = 128;

/// A synchronization tracking or publication error.
#[derive(Debug, Error)]
pub enum SyncStateError {
    /// Filesystem access failed.
    #[error(transparent)]
    Path(#[from] PathError),
    /// Another process's tracking transaction could not be locked.
    #[error("Failed to lock synchronization state")]
    Lock(#[from] FileLockError),
    /// The state file is not valid TOML for this schema.
    #[error("Invalid synchronization state")]
    Decode(#[from] toml_edit::de::Error),
    /// The state could not be encoded.
    #[error("Failed to encode synchronization state")]
    Encode(#[from] toml_edit::ser::Error),
    /// Another schema version must not be interpreted as this one.
    #[error("Unsupported synchronization state version {0}")]
    UnsupportedVersion(u32),
    /// A state invariant or publication transition was violated.
    #[error("Invalid synchronization state: {0}")]
    Invalid(String),
}

/// Immutable peer histories retained outside active operation heads.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerState {
    /// Original operation IDs, encoded as lowercase hexadecimal.
    pub raw_heads: BTreeSet<String>,
    /// Shared causal event IDs, encoded as lowercase hexadecimal.
    pub shared_heads: BTreeSet<String>,
}

/// The last verified immutable raw/shared normalization boundary at this endpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalAnchor {
    /// Original operation represented by this boundary.
    pub raw_head: String,
    /// Shared causal state, including any retained history-rewrite checkpoint.
    pub shared_anchor: String,
}

/// Progress through one receiver's local publication boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    /// Incoming roots are pinned; publication might not have occurred yet.
    Pending,
    /// The intended operation was observed durably published.
    Published,
    /// Tracking and local finalization have completed.
    Completed,
}

/// Write-ahead state for a single receiving endpoint, never a distributed lock.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncSession {
    /// Local peer key; it is not interpreted as a path.
    pub peer: String,
    /// Receiver's baseline after ordinary snapshotting.
    pub baseline_operation: String,
    /// Original incoming operations protected before publication.
    pub incoming_raw_heads: BTreeSet<String>,
    /// Incoming shared events protected before publication.
    pub incoming_shared_heads: BTreeSet<String>,
    /// Selected applied anchor, once reconciliation has completed.
    pub applied_anchor: Option<String>,
    /// Intended result, persisted before changing active operation heads.
    pub result_operation: Option<String>,
    /// Durable progress at this endpoint.
    pub phase: SessionPhase,
}

/// What recovery must do after checking the repository's actual durable history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionRecovery {
    /// There is no proposed publication yet.
    ContinueTransfer,
    /// The proposed operation is not published; keep its pinned data and retry.
    ResumePublication,
    /// Publication succeeded; complete local finalization without republishing.
    FinishPublished,
    /// The session has already completed.
    Completed,
}

impl SyncSession {
    /// Classifies recovery without guessing from a missing acknowledgment.
    ///
    /// The caller must check whether `result_operation` is in durable published
    /// history, including ancestors of current heads, rather than just comparing
    /// it with the current tip. A negative observation after recorded publication
    /// is inconsistent and requires inspection, not an automatic rollback.
    pub fn recovery(&self, result_is_published: bool) -> Result<SessionRecovery, SyncStateError> {
        match self.phase {
            SessionPhase::Completed => Ok(SessionRecovery::Completed),
            SessionPhase::Published if !result_is_published => Err(SyncStateError::Invalid(
                "recorded publication is absent from durable history".to_owned(),
            )),
            SessionPhase::Published => Ok(SessionRecovery::FinishPublished),
            SessionPhase::Pending if self.result_operation.is_none() => {
                if result_is_published {
                    return Err(SyncStateError::Invalid(
                        "publication observed without a proposed operation".to_owned(),
                    ));
                }
                Ok(SessionRecovery::ContinueTransfer)
            }
            SessionPhase::Pending if result_is_published => Ok(SessionRecovery::FinishPublished),
            SessionPhase::Pending => Ok(SessionRecovery::ResumePublication),
        }
    }
}

/// Roots to combine with active local history during garbage collection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncGcRoots {
    /// Original operation roots, including unfinished local publications.
    pub raw_operations: BTreeSet<String>,
    /// Shared causal roots, which live outside original operation ancestry.
    pub shared_events: BTreeSet<String>,
}

/// Versioned synchronization metadata for one repository.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncState {
    format_version: u32,
    /// Opaque endpoint identity for peer tracking, independent of paths/ancestry.
    pub repository_id: String,
    /// Retained original and shared peer histories.
    pub peers: BTreeMap<String, PeerState>,
    /// Current locally represented shared causal roots.
    pub shared_anchors: BTreeSet<String>,
    /// Reconstructible raw-to-shared normalization cache, not additional roots.
    pub normalized_operations: BTreeMap<String, String>,
    /// Pending and completed receiver-local publication records.
    pub sessions: BTreeMap<String, SyncSession>,
    /// The actual operation last normalized, independent of peer advertisements.
    pub local_anchor: Option<LocalAnchor>,
    /// Frozen outgoing or installed roots retained across unlocked network waits.
    pub transfer_pins: BTreeMap<String, PeerState>,
    /// Actual pre-pruning knowledge awaiting a checkpoint at the next capture.
    pub pending_pruned_shared_heads: BTreeSet<String>,
}

impl Default for SyncState {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            repository_id: format!("{:032x}", rand::random::<u128>()),
            peers: BTreeMap::new(),
            shared_anchors: BTreeSet::new(),
            normalized_operations: BTreeMap::new(),
            sessions: BTreeMap::new(),
            local_anchor: None,
            transfer_pins: BTreeMap::new(),
            pending_pruned_shared_heads: BTreeSet::new(),
        }
    }
}

impl SyncState {
    /// Returns roots retained by tracking and unfinished publication records.
    ///
    /// Cache entries do not pin otherwise abandoned history. Shared-event source
    /// references and anchor references in original integration metadata must be
    /// followed by the GC caller in addition to these explicit roots.
    pub fn gc_roots(&self) -> SyncGcRoots {
        let mut roots = SyncGcRoots::default();
        roots
            .shared_events
            .extend(self.shared_anchors.iter().cloned());
        roots
            .shared_events
            .extend(self.pending_pruned_shared_heads.iter().cloned());
        if let Some(anchor) = &self.local_anchor {
            roots.raw_operations.insert(anchor.raw_head.clone());
            roots.shared_events.insert(anchor.shared_anchor.clone());
        }
        for peer in self.peers.values().chain(self.transfer_pins.values()) {
            roots.raw_operations.extend(peer.raw_heads.iter().cloned());
            roots
                .shared_events
                .extend(peer.shared_heads.iter().cloned());
        }
        for session in self.sessions.values() {
            if session.phase == SessionPhase::Completed {
                continue;
            }
            roots
                .raw_operations
                .insert(session.baseline_operation.clone());
            roots
                .raw_operations
                .extend(session.result_operation.iter().cloned());
            roots
                .raw_operations
                .extend(session.incoming_raw_heads.iter().cloned());
            roots
                .shared_events
                .extend(session.incoming_shared_heads.iter().cloned());
            roots
                .shared_events
                .extend(session.applied_anchor.iter().cloned());
        }
        roots
    }

    /// Pins one immutable transfer snapshot; reusing a key cannot move its roots.
    ///
    /// Pins survive process exit and have no automatic expiration. A completed or
    /// abandoned transfer must be released explicitly after its data is safe.
    pub fn pin_transfer(&mut self, key: String, roots: PeerState) -> Result<(), SyncStateError> {
        if let Some(existing) = self.transfer_pins.get(&key) {
            if existing != &roots {
                return Err(SyncStateError::Invalid(
                    "transfer key was reused for different roots".to_owned(),
                ));
            }
        } else {
            self.transfer_pins.insert(key, roots);
        }
        Ok(())
    }

    /// Retains installed peer roots without interpreting local pruning as deletion.
    pub fn retain_peer(&mut self, peer: &str, roots: PeerState) {
        let retained = self.peers.entry(peer.to_owned()).or_default();
        retained.raw_heads.extend(roots.raw_heads);
        retained.shared_heads.extend(roots.shared_heads);
    }

    /// Adds an idempotent pending session, rejecting reuse for different work.
    pub fn begin_session(
        &mut self,
        session_id: String,
        session: SyncSession,
    ) -> Result<(), SyncStateError> {
        if let Some(existing) = self.sessions.get(&session_id) {
            let mut original_request = existing.clone();
            original_request.phase = SessionPhase::Pending;
            return if original_request == session {
                Ok(())
            } else {
                Err(SyncStateError::Invalid(
                    "session ID was reused for different work".to_owned(),
                ))
            };
        }
        if session.phase != SessionPhase::Pending {
            return Err(SyncStateError::Invalid(
                "new session is not pending".to_owned(),
            ));
        }
        self.sessions.insert(session_id, session);
        Ok(())
    }

    /// Records an independently verified publication of the intended operation.
    pub fn record_publication(
        &mut self,
        session_id: &str,
        observed_operation: &str,
    ) -> Result<(), SyncStateError> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| SyncStateError::Invalid("unknown publication session".to_owned()))?;
        if session.result_operation.as_deref() != Some(observed_operation) {
            return Err(SyncStateError::Invalid(
                "published operation differs from prepared result".to_owned(),
            ));
        }
        if session.applied_anchor.is_none() {
            return Err(SyncStateError::Invalid(
                "publication has no prepared shared anchor".to_owned(),
            ));
        }
        if session.phase == SessionPhase::Pending {
            session.phase = SessionPhase::Published;
        }
        Ok(())
    }

    /// Completes a publication and retains incoming history atomically with it.
    ///
    /// Earlier peer roots are retained: local operation-history pruning does not
    /// request deletion of already received history.
    pub fn complete_session(&mut self, session_id: &str) -> Result<(), SyncStateError> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| SyncStateError::Invalid("unknown publication session".to_owned()))?;
        if session.phase == SessionPhase::Pending {
            return Err(SyncStateError::Invalid(
                "cannot complete an unconfirmed publication".to_owned(),
            ));
        }
        let peer = self.peers.entry(session.peer.clone()).or_default();
        peer.raw_heads
            .extend(session.incoming_raw_heads.iter().cloned());
        peer.shared_heads
            .extend(session.incoming_shared_heads.iter().cloned());
        self.shared_anchors
            .extend(session.applied_anchor.iter().cloned());
        session.phase = SessionPhase::Completed;
        Ok(())
    }

    fn validate(&self) -> Result<(), SyncStateError> {
        if self.format_version != FORMAT_VERSION {
            return Err(SyncStateError::UnsupportedVersion(self.format_version));
        }
        if self.repository_id.len() != 32
            || !self
                .repository_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SyncStateError::Invalid(
                "endpoint identity is not 16-byte lowercase hexadecimal".to_owned(),
            ));
        }
        let check_id = |id: &str| {
            if id.len() != ID_HEX_LENGTH
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                Err(SyncStateError::Invalid(
                    "operation/event ID is not 64-byte lowercase hexadecimal".to_owned(),
                ))
            } else {
                Ok(())
            }
        };
        for peer in self.peers.values().chain(self.transfer_pins.values()) {
            for id in peer.raw_heads.iter().chain(&peer.shared_heads) {
                check_id(id)?;
            }
        }
        if let Some(anchor) = &self.local_anchor {
            check_id(&anchor.raw_head)?;
            check_id(&anchor.shared_anchor)?;
        }
        for id in &self.shared_anchors {
            check_id(id)?;
        }
        for id in &self.pending_pruned_shared_heads {
            check_id(id)?;
        }
        for (raw_id, shared_id) in &self.normalized_operations {
            check_id(raw_id)?;
            check_id(shared_id)?;
        }
        for session in self.sessions.values() {
            check_id(&session.baseline_operation)?;
            for id in session
                .incoming_raw_heads
                .iter()
                .chain(&session.incoming_shared_heads)
                .chain(session.applied_anchor.iter())
                .chain(session.result_operation.iter())
            {
                check_id(id)?;
            }
            if session.phase != SessionPhase::Pending
                && (session.applied_anchor.is_none() || session.result_operation.is_none())
            {
                return Err(SyncStateError::Invalid(
                    "published session lacks its result or anchor".to_owned(),
                ));
            }
            if session.result_operation.is_some() && session.applied_anchor.is_none() {
                return Err(SyncStateError::Invalid(
                    "proposed publication has no shared anchor".to_owned(),
                ));
            }
            if session.phase == SessionPhase::Completed {
                let roots_retained = self.peers.get(&session.peer).is_some_and(|peer| {
                    session.incoming_raw_heads.is_subset(&peer.raw_heads)
                        && session.incoming_shared_heads.is_subset(&peer.shared_heads)
                }) && session
                    .applied_anchor
                    .as_ref()
                    .is_some_and(|anchor| self.shared_anchors.contains(anchor));
                if !roots_retained {
                    return Err(SyncStateError::Invalid(
                        "completed session has unretained incoming history".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// An atomic tracking file and a short-lived local writer lock.
#[derive(Clone, Debug)]
pub struct SyncStateStore {
    directory: PathBuf,
}

impl SyncStateStore {
    /// Creates tracking storage without changing repository format or heads.
    pub fn init(repo_path: &Path) -> Result<Self, SyncStateError> {
        let store = Self::load(repo_path);
        match fs::create_dir(&store.directory) {
            Ok(()) => sync_directory(repo_path)?,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err).context(&store.directory).map_err(Into::into),
        }
        let _lock = FileLock::lock(store.directory.join("state.lock"))?;
        match fs::read_to_string(store.state_path()) {
            Ok(contents) => {
                decode_state(&contents)?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                store.write(&SyncState::default())?;
            }
            Err(err) => return Err(err).context(store.state_path()).map_err(Into::into),
        }
        Ok(store)
    }

    /// Addresses existing tracking storage without creating or modifying it.
    pub fn load(repo_path: &Path) -> Self {
        Self {
            directory: repo_path.join("sync"),
        }
    }

    /// Reads one complete atomically published snapshot without taking a lock.
    pub fn read(&self) -> Result<SyncState, SyncStateError> {
        let contents = fs::read_to_string(self.state_path()).context(self.state_path())?;
        decode_state(&contents)
    }

    /// Acquires the writer lock and then reads the latest state.
    ///
    /// Do not retain this guard across network transfers or operation/checkout
    /// locks; save the pending roots first, then release it.
    pub fn lock(&self) -> Result<LockedSyncState<'_>, SyncStateError> {
        let lock = FileLock::lock(self.directory.join("state.lock"))?;
        let state = self.read()?;
        Ok(LockedSyncState {
            store: self,
            saved_state: state.clone(),
            state,
            _lock: lock,
        })
    }

    fn state_path(&self) -> PathBuf {
        self.directory.join("state.toml")
    }

    fn write(&self, state: &SyncState) -> Result<(), SyncStateError> {
        state.validate()?;
        let contents = toml_edit::ser::to_string(state)?;
        let mut temporary = NamedTempFile::new_in(&self.directory).context(&self.directory)?;
        temporary
            .write_all(contents.as_bytes())
            .context(temporary.path())?;
        persist_temp_file(temporary, self.state_path()).context(self.state_path())?;
        sync_directory(&self.directory)?;
        Ok(())
    }
}

/// An explicit read-modify-save transaction; dropping it discards unsaved edits.
pub struct LockedSyncState<'a> {
    store: &'a SyncStateStore,
    saved_state: SyncState,
    state: SyncState,
    _lock: FileLock,
}

impl LockedSyncState<'_> {
    /// Returns the latest tracking snapshot under the writer lock.
    pub fn state(&self) -> &SyncState {
        &self.state
    }

    /// Edits tracking metadata without changing active operation heads.
    pub fn state_mut(&mut self) -> &mut SyncState {
        &mut self.state
    }

    /// Validates and atomically persists edits while retaining the writer lock.
    pub fn save(&mut self) -> Result<(), SyncStateError> {
        if self.state != self.saved_state {
            if self.state.repository_id != self.saved_state.repository_id {
                return Err(SyncStateError::Invalid(
                    "an initialized endpoint identity cannot change".to_owned(),
                ));
            }
            self.store.write(&self.state)?;
            self.saved_state = self.state.clone();
        }
        Ok(())
    }
}

fn decode_state(contents: &str) -> Result<SyncState, SyncStateError> {
    let state: SyncState = toml_edit::de::from_str(contents)?;
    state.validate()?;
    Ok(state)
}

fn sync_directory(path: &Path) -> Result<(), PathError> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .context(path)?;
    // Match existing working-copy journaling: directory fsync is not exposed
    // through a portable std API on Windows.
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;
    use crate::tests::TestResult;
    use crate::tests::new_temp_dir;

    fn id(byte: char) -> String {
        byte.to_string().repeat(ID_HEX_LENGTH)
    }

    fn pending_session() -> SyncSession {
        SyncSession {
            peer: "other machine".to_owned(),
            baseline_operation: id('1'),
            incoming_raw_heads: BTreeSet::from([id('2')]),
            incoming_shared_heads: BTreeSet::from([id('3')]),
            applied_anchor: Some(id('4')),
            result_operation: Some(id('5')),
            phase: SessionPhase::Pending,
        }
    }

    #[test]
    fn reopen_and_discard_unsaved_changes() -> TestResult {
        let directory = new_temp_dir();
        let store = SyncStateStore::init(directory.path())?;
        {
            let mut locked = store.lock()?;
            locked.state_mut().shared_anchors.insert(id('1'));
            locked.save()?;
            locked.state_mut().shared_anchors.insert(id('2'));
        }
        let reopened = SyncStateStore::init(directory.path())?;
        assert_eq!(reopened.read()?.shared_anchors, BTreeSet::from([id('1')]));
        Ok(())
    }

    #[test]
    fn pending_publication_recovery_and_retention() -> TestResult {
        let directory = new_temp_dir();
        let store = SyncStateStore::init(directory.path())?;
        {
            let mut locked = store.lock()?;
            locked
                .state_mut()
                .begin_session("session".to_owned(), pending_session())?;
            locked.save()?;
        }
        let state = store.read()?;
        assert_eq!(
            state.sessions["session"].recovery(false)?,
            SessionRecovery::ResumePublication
        );
        // Simulate publication followed by lost acknowledgment/process exit.
        assert_eq!(
            state.sessions["session"].recovery(true)?,
            SessionRecovery::FinishPublished
        );
        let roots = state.gc_roots();
        assert!(roots.raw_operations.contains(&id('1')));
        assert!(roots.raw_operations.contains(&id('2')));
        assert!(roots.raw_operations.contains(&id('5')));
        assert!(roots.shared_events.contains(&id('3')));
        assert!(roots.shared_events.contains(&id('4')));
        {
            let mut locked = store.lock()?;
            assert!(
                locked
                    .state_mut()
                    .record_publication("session", &id('6'))
                    .is_err()
            );
            locked.state_mut().record_publication("session", &id('5'))?;
            locked.state_mut().complete_session("session")?;
            locked.save()?;
        }
        let state = store.read()?;
        assert_eq!(
            state.sessions["session"].recovery(true)?,
            SessionRecovery::Completed
        );
        assert!(state.peers["other machine"].raw_heads.contains(&id('2')));
        assert!(state.gc_roots().shared_events.contains(&id('4')));
        assert!(!state.gc_roots().raw_operations.contains(&id('1')));
        Ok(())
    }

    #[test]
    fn session_transitions_are_checked_and_idempotent() -> TestResult {
        let mut state = SyncState::default();
        state.begin_session("session".to_owned(), pending_session())?;
        state.begin_session("session".to_owned(), pending_session())?;
        assert!(state.complete_session("session").is_err());
        let mut different = pending_session();
        different.result_operation = Some(id('6'));
        assert!(
            state
                .begin_session("session".to_owned(), different)
                .is_err()
        );
        state.record_publication("session", &id('5'))?;
        assert!(state.sessions["session"].recovery(false).is_err());
        state.complete_session("session")?;
        state.record_publication("session", &id('5'))?;
        state.complete_session("session")?;
        state.begin_session("session".to_owned(), pending_session())?;
        assert_eq!(state.sessions["session"].phase, SessionPhase::Completed);
        state.validate()?;
        Ok(())
    }

    #[test]
    fn invalid_update_preserves_previous_file() -> TestResult {
        let directory = new_temp_dir();
        let store = SyncStateStore::init(directory.path())?;
        let before = fs::read(store.state_path())?;
        let mut locked = store.lock()?;
        locked
            .state_mut()
            .shared_anchors
            .insert("invalid/path".to_owned());
        assert!(locked.save().is_err());
        assert_eq!(fs::read(store.state_path())?, before);
        Ok(())
    }

    #[test]
    fn completed_session_cannot_drop_history_roots() -> TestResult {
        let mut state = SyncState::default();
        state.begin_session("session".to_owned(), pending_session())?;
        state.record_publication("session", &id('5'))?;
        state.complete_session("session")?;
        state.validate()?;
        state.peers.clear();
        assert!(state.validate().is_err());
        Ok(())
    }

    #[test]
    fn abandoned_temporary_file_is_not_recovery_state() -> TestResult {
        let directory = new_temp_dir();
        let store = SyncStateStore::init(directory.path())?;
        let original = store.read()?;
        fs::write(store.directory.join("interrupted.tmp"), "partial data")?;
        assert_eq!(store.read()?, original);
        fs::write(store.state_path(), "invalid state")?;
        assert!(store.read().is_err());
        assert!(SyncStateStore::init(directory.path()).is_err());
        Ok(())
    }

    #[test]
    fn reject_unknown_version_and_malformed_ids() -> TestResult {
        let state = SyncState::default();
        let encoded = toml_edit::ser::to_string(&state)?;
        assert!(matches!(
            decode_state(&encoded.replace("format_version = 1", "format_version = 2")),
            Err(SyncStateError::UnsupportedVersion(2))
        ));
        let mut invalid = state;
        invalid.normalized_operations.insert(id('a'), id('A'));
        assert!(invalid.validate().is_err());
        assert!(invalid.gc_roots().raw_operations.is_empty());
        Ok(())
    }

    #[test]
    fn concurrent_updates_read_after_locking() -> TestResult {
        let directory = new_temp_dir();
        let store = SyncStateStore::init(directory.path())?;
        thread::scope(|scope| -> TestResult {
            let first = scope.spawn(|| -> Result<(), SyncStateError> {
                let mut locked = store.lock()?;
                locked.state_mut().shared_anchors.insert(id('1'));
                locked.save()
            });
            let second = scope.spawn(|| -> Result<(), SyncStateError> {
                let mut locked = store.lock()?;
                locked.state_mut().shared_anchors.insert(id('2'));
                locked.save()
            });
            first.join().unwrap()?;
            second.join().unwrap()?;
            Ok(())
        })?;
        assert_eq!(
            store.read()?.shared_anchors,
            BTreeSet::from([id('1'), id('2')])
        );
        Ok(())
    }

    #[test]
    fn endpoint_identity_is_stable_and_distinct() -> TestResult {
        let first = new_temp_dir();
        let second = new_temp_dir();
        let first_store = SyncStateStore::init(first.path())?;
        let first_id = first_store.read()?.repository_id;
        assert_eq!(
            SyncStateStore::init(first.path())?.read()?.repository_id,
            first_id
        );
        assert_ne!(
            SyncStateStore::init(second.path())?.read()?.repository_id,
            first_id
        );
        let mut locked = first_store.lock()?;
        locked.state_mut().repository_id = "00".repeat(16);
        assert!(locked.save().is_err());
        assert_eq!(first_store.read()?.repository_id, first_id);
        Ok(())
    }

    #[test]
    fn normalization_boundary_and_transfer_pins_are_explicit_gc_roots() -> TestResult {
        let mut state = SyncState {
            local_anchor: Some(LocalAnchor {
                raw_head: id('1'),
                shared_anchor: id('2'),
            }),
            ..Default::default()
        };
        let pinned = PeerState {
            raw_heads: BTreeSet::from([id('3')]),
            shared_heads: BTreeSet::from([id('4')]),
        };
        state.pin_transfer("transfer".to_owned(), pinned.clone())?;
        state.pending_pruned_shared_heads.insert(id('5'));
        state.pin_transfer("transfer".to_owned(), pinned)?;
        assert!(
            state
                .pin_transfer("transfer".to_owned(), PeerState::default())
                .is_err()
        );
        assert_eq!(
            state.gc_roots().raw_operations,
            BTreeSet::from([id('1'), id('3')])
        );
        assert_eq!(
            state.gc_roots().shared_events,
            BTreeSet::from([id('2'), id('4'), id('5')])
        );
        state.transfer_pins.clear();
        assert_eq!(state.gc_roots().raw_operations, BTreeSet::from([id('1')]));
        state.validate()?;
        Ok(())
    }
}
