//! Fixed, bounded manager-to-broker protocol for privileged read operations.
//!
//! Large tree-search results are written to a manager-created private file
//! passed with SCM_RIGHTS. The seqpacket response contains only its length and
//! digest, so namespace-sized indexes never cross the control socket.

use crate::broker::{
    execute_changed_objects, execute_full_index, execute_snapshot_create, execute_snapshot_delete,
    execute_target_object_lookup, execute_worktree_rename, ChangedObjectsExecution,
    ChangedObjectsResult, EffectKind, ExpectedManagedDirectory, ExpectedReservation,
    ExpectedSubvolume, Frame, Opcode, PeerCredentials, ReceiptRequest, SeqPacket, SessionGate,
    SnapshotCreateExecution, SnapshotCreateResult, SnapshotDeleteExecution, SnapshotDeleteResult,
    StoredBrokerRequest, WorktreeRenameExecution, WorktreeRenameResult,
};
use crate::btrfs::{filesystem_info, subvolume_info};
use crate::index::{Index, Object};
use crate::manifest::Reference;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::mem::zeroed;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::Mutex;

use crate::broker::BrokerError;
use crate::store::BrokerJournal;

const RESPONSE_OK: u8 = 0;
const RESPONSE_ERROR: u8 = 1;
const INDEX_MAGIC: &[u8; 8] = b"BAWIDX01";
const MAX_INDEX_OUTPUT: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug)]
pub struct BrokerDispatcher {
    manager_uid: u32,
    gate: SessionGate,
    journal: Option<Mutex<BrokerJournal>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconciledEffect {
    SnapshotCreated(SnapshotCreateResult),
    SnapshotDeleted(SnapshotDeleteResult),
    WorktreePublished(WorktreeRenameResult),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnresolvedEffect {
    pub opcode: Opcode,
    pub operation_id: [u8; 16],
    pub operation_fence: i64,
}

impl BrokerDispatcher {
    pub fn new(manager_uid: u32) -> Self {
        Self {
            manager_uid,
            gate: SessionGate::default(),
            journal: None,
        }
    }

    pub fn with_journal(manager_uid: u32, journal: BrokerJournal) -> Self {
        Self {
            manager_uid,
            gate: SessionGate::default(),
            journal: Some(Mutex::new(journal)),
        }
    }

    /// Receives, authenticates, executes, and replies to exactly one request.
    pub fn serve_one(&self, socket: &SeqPacket) -> Result<(), BrokerError> {
        let peer = socket.peer_credentials()?;
        let received = socket.receive()?;
        let opcode = received.frame.opcode;
        let response = match self.dispatch(peer, received.frame, &received.fds) {
            Ok(payload) => success_payload(payload),
            Err(error) => error_payload(&error),
        };
        socket.send(&Frame::new(opcode, response)?, &[])
    }

    fn dispatch(
        &self,
        peer: PeerCredentials,
        frame: Frame,
        fds: &[std::os::fd::OwnedFd],
    ) -> Result<Vec<u8>, BrokerError> {
        if peer.uid != self.manager_uid {
            return Err(BrokerError::new(
                "broker peer is not the configured manager UID",
            ));
        }
        let mut decoder = Decoder::new(&frame.payload);
        if frame.opcode == Opcode::Handshake {
            require_fd_count(fds, 0)?;
            let store_uuid = decoder.array::<16>()?;
            if decoder.remaining() == 16 {
                let session_id = decoder.array::<16>()?;
                decoder.finish()?;
                self.gate.join(store_uuid, session_id)?;
                return Ok(session_id.to_vec());
            }
            decoder.finish()?;
            return Ok(self.gate.handshake(store_uuid).to_vec());
        }

        let store_uuid = decoder.array::<16>()?;
        let session_id = decoder.array::<16>()?;
        // Retain this permit through the complete dispatch, including receipt
        // commit and any ioctl. A newer handshake cannot return its recovery
        // barrier until this permit drops.
        let _session_permit = self.gate.authorize(store_uuid, session_id)?;
        match frame.opcode {
            Opcode::InspectSubvolume => {
                require_fd_count(fds, 1)?;
                decoder.finish()?;
                let filesystem = filesystem_info(fds[0].as_fd())
                    .map_err(|error| BrokerError::new(format!("inspect filesystem: {error}")))?;
                let subvolume = subvolume_info(fds[0].as_fd())
                    .map_err(|error| BrokerError::new(format!("inspect subvolume: {error}")))?;
                let observed = ExpectedSubvolume::from_observed(&filesystem, &subvolume);
                let mut output = Encoder::default();
                output.expected_subvolume(&observed);
                Ok(output.finish())
            }
            Opcode::ChangedObjects => {
                require_fd_count(fds, 3)?;
                let request = ChangedObjectsExecution {
                    parent: decoder.expected_subvolume()?,
                    target: decoder.expected_subvolume()?,
                    output_owner_uid: decoder.u32()?,
                    max_output_bytes: decoder.u64()?,
                };
                decoder.finish()?;
                let result = execute_changed_objects(
                    &request,
                    fds[0].as_fd(),
                    fds[1].as_fd(),
                    fds[2].as_fd(),
                )?;
                Ok(encode_file_result(
                    result.output_bytes,
                    result.manifest_hash,
                ))
            }
            Opcode::FullIndex => {
                require_fd_count(fds, 2)?;
                let expected = decoder.expected_subvolume()?;
                let output_owner_uid = decoder.u32()?;
                let max_output_bytes = decoder.u64()?;
                decoder.finish()?;
                validate_index_limit(max_output_bytes)?;
                verify_private_output(fds[1].as_fd(), output_owner_uid)?;
                let index = execute_full_index(&expected, fds[0].as_fd())?;
                let bytes = encode_index(&index)?;
                let result = write_index_output(fds[1].as_fd(), &bytes, max_output_bytes)?;
                Ok(encode_file_result(
                    result.output_bytes,
                    result.manifest_hash,
                ))
            }
            Opcode::TargetObjectLookup => {
                require_fd_count(fds, 2)?;
                let expected = decoder.expected_subvolume()?;
                let output_owner_uid = decoder.u32()?;
                let max_output_bytes = decoder.u64()?;
                let count = usize::try_from(decoder.u32()?)
                    .map_err(|_| BrokerError::new("target inode count exceeds usize"))?;
                if count > 1_000_000 {
                    return Err(BrokerError::new(
                        "target inode count exceeds protocol limit",
                    ));
                }
                let mut inodes = BTreeSet::new();
                for _ in 0..count {
                    if !inodes.insert(decoder.u64()?) {
                        return Err(BrokerError::new("target inode request contains duplicates"));
                    }
                }
                decoder.finish()?;
                validate_index_limit(max_output_bytes)?;
                verify_private_output(fds[1].as_fd(), output_owner_uid)?;
                let objects = execute_target_object_lookup(&expected, fds[0].as_fd(), &inodes)?;
                let bytes = encode_objects(&objects)?;
                let result = write_index_output(fds[1].as_fd(), &bytes, max_output_bytes)?;
                Ok(encode_file_result(
                    result.output_bytes,
                    result.manifest_hash,
                ))
            }
            Opcode::CreateSnapshot => {
                require_fd_count(fds, 2)?;
                let execution = decoder.snapshot_create()?;
                decoder.finish()?;
                verify_receipt_session(&execution.receipt, store_uuid, session_id)?;
                let mut journal = self.journal()?;
                journal.record_request_payload(
                    &execution.receipt,
                    Opcode::CreateSnapshot,
                    &frame.payload,
                )?;
                let result = execute_snapshot_create(
                    &self.gate,
                    &mut journal,
                    &execution,
                    fds[0].as_fd(),
                    fds[1].as_fd(),
                )?;
                let mut output = Encoder::default();
                output.expected_subvolume(&result.snapshot);
                output.array(result.result_hash);
                Ok(output.finish())
            }
            Opcode::DeleteSnapshot => {
                require_fd_count(fds, 1)?;
                let execution = decoder.snapshot_delete()?;
                decoder.finish()?;
                verify_receipt_session(&execution.receipt, store_uuid, session_id)?;
                let mut journal = self.journal()?;
                journal.record_request_payload(
                    &execution.receipt,
                    Opcode::DeleteSnapshot,
                    &frame.payload,
                )?;
                let result =
                    execute_snapshot_delete(&self.gate, &mut journal, &execution, fds[0].as_fd())?;
                let mut output = Encoder::default();
                output.array(result.deleted_subvolume_uuid);
                output.array(result.result_hash);
                Ok(output.finish())
            }
            Opcode::PublishWorktree => {
                require_fd_count(fds, 2)?;
                let execution = decoder.worktree_rename()?;
                decoder.finish()?;
                verify_receipt_session(&execution.receipt, store_uuid, session_id)?;
                let mut journal = self.journal()?;
                journal.record_request_payload(
                    &execution.receipt,
                    Opcode::PublishWorktree,
                    &frame.payload,
                )?;
                let result = execute_worktree_rename(
                    &self.gate,
                    &mut journal,
                    &execution,
                    fds[0].as_fd(),
                    fds[1].as_fd(),
                )?;
                let mut output = Encoder::default();
                output.array(result.worktree_subvolume_uuid);
                output.array(result.result_hash);
                Ok(output.finish())
            }
            Opcode::ReconcileReceipt => {
                if decoder.remaining() == 0 {
                    require_fd_count(fds, 0)?;
                    let journal = self.journal()?;
                    let count = journal.unresolved_receipts(store_uuid)?.len();
                    let mut output = Encoder::default();
                    output.u64(
                        u64::try_from(count).map_err(|_| {
                            BrokerError::new("unresolved receipt count exceeds u64")
                        })?,
                    );
                    return Ok(output.finish());
                }
                if decoder.remaining() == 1 && decoder.u8()? == 1 {
                    require_fd_count(fds, 0)?;
                    decoder.finish()?;
                    let journal = self.journal()?;
                    let requests = journal.unresolved_requests(store_uuid)?;
                    let mut output = Encoder::default();
                    output.u32(
                        u32::try_from(requests.len()).map_err(|_| {
                            BrokerError::new("unresolved request count exceeds u32")
                        })?,
                    );
                    for request in requests {
                        output.u16(request.opcode as u16);
                        output.array(request.receipt.operation_id);
                        output.i64(request.receipt.operation_fence);
                    }
                    if output.bytes.len() > crate::broker::MAX_FRAME_PAYLOAD {
                        return Err(BrokerError::new(
                            "unresolved request list exceeds broker frame limit",
                        ));
                    }
                    return Ok(output.finish());
                }
                if decoder.remaining() == 27 && decoder.u8()? == 2 {
                    require_fd_count(fds, 0)?;
                    let effect_opcode = Opcode::decode(decoder.u16()?)?;
                    let operation_id = decoder.array::<16>()?;
                    let operation_fence = decoder.i64()?;
                    decoder.finish()?;
                    let journal = self.journal()?;
                    let exists = journal
                        .request_for_operation(
                            store_uuid,
                            operation_id,
                            operation_fence,
                            effect_opcode,
                        )?
                        .is_some();
                    Ok(vec![u8::from(exists)])
                } else {
                    let effect_opcode = Opcode::decode(decoder.u16()?)?;
                    let operation_id = decoder.array::<16>()?;
                    let operation_fence = decoder.i64()?;
                    decoder.finish()?;
                    let mut journal = self.journal()?;
                    let stored = journal
                        .request_for_operation(
                            store_uuid,
                            operation_id,
                            operation_fence,
                            effect_opcode,
                        )?
                        .ok_or_else(|| {
                            BrokerError::new("unresolved broker request was not found")
                        })?;
                    reconcile_stored_request(
                        &self.gate,
                        &mut journal,
                        stored,
                        store_uuid,
                        session_id,
                        fds,
                    )
                }
            }
            Opcode::Handshake => unreachable!("handled before authorization envelope"),
        }
    }

    fn journal(&self) -> Result<std::sync::MutexGuard<'_, BrokerJournal>, BrokerError> {
        self.journal
            .as_ref()
            .ok_or_else(|| BrokerError::new("broker mutation journal is not configured"))?
            .lock()
            .map_err(|_| BrokerError::new("broker journal mutex is poisoned"))
    }
}

fn verify_receipt_session(
    receipt: &ReceiptRequest,
    store_uuid: [u8; 16],
    session_id: [u8; 16],
) -> Result<(), BrokerError> {
    if receipt.manager_store_uuid != store_uuid || receipt.manager_session_id != session_id {
        return Err(BrokerError::new(
            "effect receipt does not match the authenticated request session",
        ));
    }
    Ok(())
}

fn reconcile_stored_request(
    gate: &SessionGate,
    journal: &mut BrokerJournal,
    stored: StoredBrokerRequest,
    store_uuid: [u8; 16],
    session_id: [u8; 16],
    fds: &[std::os::fd::OwnedFd],
) -> Result<Vec<u8>, BrokerError> {
    let mut decoder = Decoder::new(&stored.payload);
    let payload_store = decoder.array::<16>()?;
    let _old_session = decoder.array::<16>()?;
    if payload_store != store_uuid {
        return Err(BrokerError::new(
            "stored broker payload belongs to another manager store",
        ));
    }
    match stored.opcode {
        Opcode::CreateSnapshot => {
            require_fd_count(fds, 2)?;
            let mut execution = decoder.snapshot_create()?;
            decoder.finish()?;
            verify_stored_receipt(&stored, &execution.receipt)?;
            execution.receipt.manager_session_id = session_id;
            let result =
                execute_snapshot_create(gate, journal, &execution, fds[0].as_fd(), fds[1].as_fd())?;
            let mut output = Encoder::default();
            output.expected_subvolume(&result.snapshot);
            output.array(result.result_hash);
            Ok(output.finish())
        }
        Opcode::DeleteSnapshot => {
            require_fd_count(fds, 1)?;
            let mut execution = decoder.snapshot_delete()?;
            decoder.finish()?;
            verify_stored_receipt(&stored, &execution.receipt)?;
            execution.receipt.manager_session_id = session_id;
            let result = execute_snapshot_delete(gate, journal, &execution, fds[0].as_fd())?;
            let mut output = Encoder::default();
            output.array(result.deleted_subvolume_uuid);
            output.array(result.result_hash);
            Ok(output.finish())
        }
        Opcode::PublishWorktree => {
            require_fd_count(fds, 2)?;
            let mut execution = decoder.worktree_rename()?;
            decoder.finish()?;
            verify_stored_receipt(&stored, &execution.receipt)?;
            execution.receipt.manager_session_id = session_id;
            let result =
                execute_worktree_rename(gate, journal, &execution, fds[0].as_fd(), fds[1].as_fd())?;
            let mut output = Encoder::default();
            output.array(result.worktree_subvolume_uuid);
            output.array(result.result_hash);
            Ok(output.finish())
        }
        _ => Err(BrokerError::new(
            "stored request is not a reconcilable effect opcode",
        )),
    }
}

fn verify_stored_receipt(
    stored: &StoredBrokerRequest,
    decoded: &ReceiptRequest,
) -> Result<(), BrokerError> {
    let receipt = &stored.receipt;
    if decoded.id != receipt.id
        || decoded.manager_store_uuid != receipt.manager_store_uuid
        || decoded.operation_id != receipt.operation_id
        || decoded.operation_fence != receipt.operation_fence
        || decoded.effect_kind != receipt.effect_kind
        || decoded.filesystem_uuid != receipt.filesystem_uuid
        || decoded.target_locator_hash != receipt.target_locator_hash
        || decoded.request_hash() != receipt.request_hash
    {
        return Err(BrokerError::new(
            "stored request payload does not match its unresolved receipt",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct BrokerClient {
    socket: SeqPacket,
    store_uuid: [u8; 16],
    session_id: [u8; 16],
}

struct IndexRequest<'a> {
    opcode: Opcode,
    expected: &'a ExpectedSubvolume,
    snapshot: BorrowedFd<'a>,
    output: BorrowedFd<'a>,
    output_owner_uid: u32,
    max_output_bytes: u64,
    tail: &'a [u8],
}

impl BrokerClient {
    pub fn connect(socket: SeqPacket, store_uuid: [u8; 16]) -> Result<Self, BrokerError> {
        let frame = Frame::new(Opcode::Handshake, store_uuid.to_vec())?;
        socket.send(&frame, &[])?;
        let response = receive_response(&socket, Opcode::Handshake)?;
        let session_id = response.try_into().map_err(|value: Vec<u8>| {
            BrokerError::new(format!(
                "handshake response has {} bytes, expected 16",
                value.len()
            ))
        })?;
        Ok(Self {
            socket,
            store_uuid,
            session_id,
        })
    }

    pub fn connect_existing(
        socket: SeqPacket,
        store_uuid: [u8; 16],
        session_id: [u8; 16],
    ) -> Result<Self, BrokerError> {
        let mut payload = Vec::with_capacity(32);
        payload.extend_from_slice(&store_uuid);
        payload.extend_from_slice(&session_id);
        socket.send(&Frame::new(Opcode::Handshake, payload)?, &[])?;
        let response = receive_response(&socket, Opcode::Handshake)?;
        if response.as_slice() != session_id {
            return Err(BrokerError::new(
                "broker joined a different manager session",
            ));
        }
        Ok(Self {
            socket,
            store_uuid,
            session_id,
        })
    }

    pub fn inspect_subvolume(&self, fd: BorrowedFd<'_>) -> Result<ExpectedSubvolume, BrokerError> {
        let payload = self.auth_payload();
        self.socket
            .send(&Frame::new(Opcode::InspectSubvolume, payload)?, &[fd])?;
        let response = receive_response(&self.socket, Opcode::InspectSubvolume)?;
        let mut decoder = Decoder::new(&response);
        let observed = decoder.expected_subvolume()?;
        decoder.finish()?;
        Ok(observed)
    }

    pub fn changed_objects(
        &self,
        request: &ChangedObjectsExecution,
        parent: BorrowedFd<'_>,
        target: BorrowedFd<'_>,
        output: BorrowedFd<'_>,
    ) -> Result<ChangedObjectsResult, BrokerError> {
        let mut encoder = self.auth_encoder();
        encoder.expected_subvolume(&request.parent);
        encoder.expected_subvolume(&request.target);
        encoder.u32(request.output_owner_uid);
        encoder.u64(request.max_output_bytes);
        self.socket.send(
            &Frame::new(Opcode::ChangedObjects, encoder.finish())?,
            &[parent, target, output],
        )?;
        let response = receive_response(&self.socket, Opcode::ChangedObjects)?;
        let (output_bytes, manifest_hash) = decode_file_result(&response)?;
        Ok(ChangedObjectsResult {
            output_bytes,
            manifest_hash,
        })
    }

    pub fn full_index(
        &self,
        expected: &ExpectedSubvolume,
        snapshot: BorrowedFd<'_>,
        output: BorrowedFd<'_>,
        output_owner_uid: u32,
        max_output_bytes: u64,
    ) -> Result<ChangedObjectsResult, BrokerError> {
        self.index_request(IndexRequest {
            opcode: Opcode::FullIndex,
            expected,
            snapshot,
            output,
            output_owner_uid,
            max_output_bytes,
            tail: &[],
        })
    }

    pub fn target_objects(
        &self,
        expected: &ExpectedSubvolume,
        snapshot: BorrowedFd<'_>,
        output: BorrowedFd<'_>,
        output_owner_uid: u32,
        max_output_bytes: u64,
        inodes: &BTreeSet<u64>,
    ) -> Result<ChangedObjectsResult, BrokerError> {
        let count = u32::try_from(inodes.len())
            .map_err(|_| BrokerError::new("target inode count exceeds u32"))?;
        let mut tail = Encoder::default();
        tail.u32(count);
        for ino in inodes {
            tail.u64(*ino);
        }
        let tail = tail.finish();
        self.index_request(IndexRequest {
            opcode: Opcode::TargetObjectLookup,
            expected,
            snapshot,
            output,
            output_owner_uid,
            max_output_bytes,
            tail: &tail,
        })
    }

    pub fn create_snapshot(
        &self,
        execution: &SnapshotCreateExecution,
        source: BorrowedFd<'_>,
        destination_parent: BorrowedFd<'_>,
    ) -> Result<SnapshotCreateResult, BrokerError> {
        self.verify_execution_session(&execution.receipt)?;
        let mut encoder = self.auth_encoder();
        encoder.snapshot_create(execution)?;
        self.socket.send(
            &Frame::new(Opcode::CreateSnapshot, encoder.finish())?,
            &[source, destination_parent],
        )?;
        let response = receive_response(&self.socket, Opcode::CreateSnapshot)?;
        let mut decoder = Decoder::new(&response);
        let result = SnapshotCreateResult {
            snapshot: decoder.expected_subvolume()?,
            result_hash: decoder.array()?,
        };
        decoder.finish()?;
        Ok(result)
    }

    pub fn delete_snapshot(
        &self,
        execution: &SnapshotDeleteExecution,
        destination_parent: BorrowedFd<'_>,
    ) -> Result<SnapshotDeleteResult, BrokerError> {
        self.verify_execution_session(&execution.receipt)?;
        let mut encoder = self.auth_encoder();
        encoder.snapshot_delete(execution)?;
        self.socket.send(
            &Frame::new(Opcode::DeleteSnapshot, encoder.finish())?,
            &[destination_parent],
        )?;
        let response = receive_response(&self.socket, Opcode::DeleteSnapshot)?;
        let mut decoder = Decoder::new(&response);
        let result = SnapshotDeleteResult {
            deleted_subvolume_uuid: decoder.array()?,
            result_hash: decoder.array()?,
        };
        decoder.finish()?;
        Ok(result)
    }

    pub fn publish_worktree(
        &self,
        execution: &WorktreeRenameExecution,
        staging_parent: BorrowedFd<'_>,
        destination_root: BorrowedFd<'_>,
    ) -> Result<WorktreeRenameResult, BrokerError> {
        self.verify_execution_session(&execution.receipt)?;
        let mut encoder = self.auth_encoder();
        encoder.worktree_rename(execution)?;
        self.socket.send(
            &Frame::new(Opcode::PublishWorktree, encoder.finish())?,
            &[staging_parent, destination_root],
        )?;
        let response = receive_response(&self.socket, Opcode::PublishWorktree)?;
        let mut decoder = Decoder::new(&response);
        let result = WorktreeRenameResult {
            worktree_subvolume_uuid: decoder.array()?,
            result_hash: decoder.array()?,
        };
        decoder.finish()?;
        Ok(result)
    }

    pub fn unresolved_receipt_count(&self) -> Result<u64, BrokerError> {
        self.socket.send(
            &Frame::new(Opcode::ReconcileReceipt, self.auth_payload())?,
            &[],
        )?;
        let response = receive_response(&self.socket, Opcode::ReconcileReceipt)?;
        let mut decoder = Decoder::new(&response);
        let count = decoder.u64()?;
        decoder.finish()?;
        Ok(count)
    }

    pub fn unresolved_effects(&self) -> Result<Vec<UnresolvedEffect>, BrokerError> {
        let mut encoder = self.auth_encoder();
        encoder.u8(1);
        self.socket.send(
            &Frame::new(Opcode::ReconcileReceipt, encoder.finish())?,
            &[],
        )?;
        let response = receive_response(&self.socket, Opcode::ReconcileReceipt)?;
        let mut decoder = Decoder::new(&response);
        let count = usize::try_from(decoder.u32()?)
            .map_err(|_| BrokerError::new("unresolved effect count exceeds usize"))?;
        let mut effects = Vec::with_capacity(count);
        for _ in 0..count {
            effects.push(UnresolvedEffect {
                opcode: Opcode::decode(decoder.u16()?)?,
                operation_id: decoder.array()?,
                operation_fence: decoder.i64()?,
            });
        }
        decoder.finish()?;
        Ok(effects)
    }

    pub fn has_stored_effect(
        &self,
        effect_opcode: Opcode,
        operation_id: [u8; 16],
        operation_fence: i64,
    ) -> Result<bool, BrokerError> {
        let mut encoder = self.auth_encoder();
        encoder.u8(2);
        encoder.u16(effect_opcode as u16);
        encoder.array(operation_id);
        encoder.i64(operation_fence);
        self.socket.send(
            &Frame::new(Opcode::ReconcileReceipt, encoder.finish())?,
            &[],
        )?;
        let response = receive_response(&self.socket, Opcode::ReconcileReceipt)?;
        match response.as_slice() {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(BrokerError::new(
                "broker returned an invalid effect-existence response",
            )),
        }
    }

    pub fn reconcile_snapshot_create(
        &self,
        operation_id: [u8; 16],
        operation_fence: i64,
        source: BorrowedFd<'_>,
        destination_parent: BorrowedFd<'_>,
    ) -> Result<SnapshotCreateResult, BrokerError> {
        let response = self.reconcile_request(
            Opcode::CreateSnapshot,
            operation_id,
            operation_fence,
            &[source, destination_parent],
        )?;
        let mut decoder = Decoder::new(&response);
        let result = SnapshotCreateResult {
            snapshot: decoder.expected_subvolume()?,
            result_hash: decoder.array()?,
        };
        decoder.finish()?;
        Ok(result)
    }

    pub fn reconcile_snapshot_delete(
        &self,
        operation_id: [u8; 16],
        operation_fence: i64,
        destination_parent: BorrowedFd<'_>,
    ) -> Result<SnapshotDeleteResult, BrokerError> {
        let response = self.reconcile_request(
            Opcode::DeleteSnapshot,
            operation_id,
            operation_fence,
            &[destination_parent],
        )?;
        let mut decoder = Decoder::new(&response);
        let result = SnapshotDeleteResult {
            deleted_subvolume_uuid: decoder.array()?,
            result_hash: decoder.array()?,
        };
        decoder.finish()?;
        Ok(result)
    }

    pub fn reconcile_worktree_publish(
        &self,
        operation_id: [u8; 16],
        operation_fence: i64,
        staging_parent: BorrowedFd<'_>,
        destination_root: BorrowedFd<'_>,
    ) -> Result<WorktreeRenameResult, BrokerError> {
        let response = self.reconcile_request(
            Opcode::PublishWorktree,
            operation_id,
            operation_fence,
            &[staging_parent, destination_root],
        )?;
        let mut decoder = Decoder::new(&response);
        let result = WorktreeRenameResult {
            worktree_subvolume_uuid: decoder.array()?,
            result_hash: decoder.array()?,
        };
        decoder.finish()?;
        Ok(result)
    }

    fn reconcile_request(
        &self,
        effect_opcode: Opcode,
        operation_id: [u8; 16],
        operation_fence: i64,
        fds: &[BorrowedFd<'_>],
    ) -> Result<Vec<u8>, BrokerError> {
        let mut encoder = self.auth_encoder();
        encoder.u16(effect_opcode as u16);
        encoder.array(operation_id);
        encoder.i64(operation_fence);
        self.socket.send(
            &Frame::new(Opcode::ReconcileReceipt, encoder.finish())?,
            fds,
        )?;
        receive_response(&self.socket, Opcode::ReconcileReceipt)
    }

    fn index_request(
        &self,
        request: IndexRequest<'_>,
    ) -> Result<ChangedObjectsResult, BrokerError> {
        let mut encoder = self.auth_encoder();
        encoder.expected_subvolume(request.expected);
        encoder.u32(request.output_owner_uid);
        encoder.u64(request.max_output_bytes);
        encoder.bytes.extend_from_slice(request.tail);
        self.socket.send(
            &Frame::new(request.opcode, encoder.finish())?,
            &[request.snapshot, request.output],
        )?;
        let response = receive_response(&self.socket, request.opcode)?;
        let (output_bytes, manifest_hash) = decode_file_result(&response)?;
        Ok(ChangedObjectsResult {
            output_bytes,
            manifest_hash,
        })
    }

    fn auth_encoder(&self) -> Encoder {
        let mut encoder = Encoder::default();
        encoder.array(self.store_uuid);
        encoder.array(self.session_id);
        encoder
    }

    fn auth_payload(&self) -> Vec<u8> {
        self.auth_encoder().finish()
    }

    fn verify_execution_session(&self, receipt: &ReceiptRequest) -> Result<(), BrokerError> {
        verify_receipt_session(receipt, self.store_uuid, self.session_id)
    }

    pub fn session_id(&self) -> [u8; 16] {
        self.session_id
    }
}

pub fn decode_index(bytes: &[u8]) -> Result<Index, BrokerError> {
    let (objects, references) = decode_index_parts(bytes)?;
    let index = Index {
        objects,
        references,
    };
    index
        .validate()
        .map_err(|error| BrokerError::new(format!("broker index is invalid: {error}")))?;
    Ok(index)
}

pub fn decode_objects(bytes: &[u8]) -> Result<BTreeMap<u64, Object>, BrokerError> {
    let (objects, references) = decode_index_parts(bytes)?;
    if !references.is_empty() {
        return Err(BrokerError::new("target-object output contains references"));
    }
    Ok(objects)
}

fn encode_index(index: &Index) -> Result<Vec<u8>, BrokerError> {
    encode_index_parts(&index.objects, &index.references)
}

fn encode_objects(objects: &BTreeMap<u64, Object>) -> Result<Vec<u8>, BrokerError> {
    encode_index_parts(objects, &BTreeSet::new())
}

fn encode_index_parts(
    objects: &BTreeMap<u64, Object>,
    references: &BTreeSet<Reference>,
) -> Result<Vec<u8>, BrokerError> {
    let mut encoder = Encoder::default();
    encoder.bytes.extend_from_slice(INDEX_MAGIC);
    encoder.u64(u64::try_from(objects.len()).map_err(|_| BrokerError::new("too many objects"))?);
    encoder
        .u64(u64::try_from(references.len()).map_err(|_| BrokerError::new("too many references"))?);
    for object in objects.values() {
        encoder.object(object);
    }
    for reference in references {
        encoder.u64(reference.ino);
        encoder.u64(reference.parent_ino);
        encoder.byte_string(&reference.name)?;
    }
    Ok(encoder.finish())
}

fn decode_index_parts(
    bytes: &[u8],
) -> Result<(BTreeMap<u64, Object>, BTreeSet<Reference>), BrokerError> {
    let mut decoder = Decoder::new(bytes);
    if decoder.take(INDEX_MAGIC.len())? != INDEX_MAGIC {
        return Err(BrokerError::new("invalid broker index magic"));
    }
    let object_count = bounded_count(decoder.u64()?, "object")?;
    let reference_count = bounded_count(decoder.u64()?, "reference")?;
    let mut objects = BTreeMap::new();
    for _ in 0..object_count {
        let object = decoder.object()?;
        let ino = object.ino;
        if objects.insert(ino, object).is_some() {
            return Err(BrokerError::new("broker index contains duplicate inode"));
        }
    }
    let mut references = BTreeSet::new();
    for _ in 0..reference_count {
        let reference = Reference {
            ino: decoder.u64()?,
            parent_ino: decoder.u64()?,
            name: decoder.byte_string()?,
        };
        if !references.insert(reference) {
            return Err(BrokerError::new(
                "broker index contains duplicate reference",
            ));
        }
    }
    decoder.finish()?;
    Ok((objects, references))
}

fn bounded_count(value: u64, kind: &str) -> Result<usize, BrokerError> {
    if value > 100_000_000 {
        return Err(BrokerError::new(format!(
            "broker {kind} count exceeds limit"
        )));
    }
    usize::try_from(value)
        .map_err(|_| BrokerError::new(format!("broker {kind} count exceeds usize")))
}

fn write_index_output(
    fd: BorrowedFd<'_>,
    bytes: &[u8],
    limit: u64,
) -> Result<ChangedObjectsResult, BrokerError> {
    let length = u64::try_from(bytes.len()).map_err(|_| BrokerError::new("index exceeds u64"))?;
    if length > limit {
        return Err(BrokerError::new(format!(
            "index output is {length} bytes, limit is {limit}"
        )));
    }
    let mut offset = 0;
    while offset < bytes.len() {
        // SAFETY: pwrite reads from the live byte slice and does not retain it.
        let written = unsafe {
            libc::pwrite(
                fd.as_raw_fd(),
                bytes[offset..].as_ptr().cast(),
                bytes.len() - offset,
                offset as libc::off_t,
            )
        };
        if written < 0 {
            return Err(io_error("write broker index output"));
        }
        if written == 0 {
            return Err(BrokerError::new("short broker index write"));
        }
        offset += written as usize;
    }
    if unsafe { libc::fsync(fd.as_raw_fd()) } != 0 {
        return Err(io_error("fsync broker index output"));
    }
    Ok(ChangedObjectsResult {
        output_bytes: length,
        manifest_hash: Sha256::digest(bytes).into(),
    })
}

fn verify_private_output(fd: BorrowedFd<'_>, owner_uid: u32) -> Result<(), BrokerError> {
    // SAFETY: fstat and fcntl initialize/inspect local values for a live fd.
    unsafe {
        let mut metadata: libc::stat = zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut metadata) != 0 {
            return Err(io_error("stat broker index output"));
        }
        if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
            || metadata.st_uid != owner_uid
            || metadata.st_nlink != 1
            || metadata.st_size != 0
            || metadata.st_mode & 0o077 != 0
        {
            return Err(BrokerError::new(
                "broker index output must be an empty private single-link regular file owned by the manager",
            ));
        }
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 {
            return Err(io_error("inspect broker index output flags"));
        }
        if flags & libc::O_ACCMODE != libc::O_RDWR {
            return Err(BrokerError::new(
                "broker index output must be open read-write",
            ));
        }
    }
    Ok(())
}

fn validate_index_limit(limit: u64) -> Result<(), BrokerError> {
    if limit == 0 || limit > MAX_INDEX_OUTPUT {
        return Err(BrokerError::new(format!(
            "index output limit must be between 1 and {MAX_INDEX_OUTPUT}"
        )));
    }
    Ok(())
}

fn require_fd_count(fds: &[std::os::fd::OwnedFd], expected: usize) -> Result<(), BrokerError> {
    if fds.len() != expected {
        return Err(BrokerError::new(format!(
            "broker opcode requires {expected} descriptors, received {}",
            fds.len()
        )));
    }
    Ok(())
}

fn success_payload(mut payload: Vec<u8>) -> Vec<u8> {
    payload.insert(0, RESPONSE_OK);
    payload
}

fn error_payload(error: &BrokerError) -> Vec<u8> {
    let message = error.to_string();
    let bytes = message.as_bytes();
    let available = crate::broker::MAX_FRAME_PAYLOAD.saturating_sub(1);
    let mut payload = Vec::with_capacity(1 + bytes.len().min(available));
    payload.push(RESPONSE_ERROR);
    payload.extend_from_slice(&bytes[..bytes.len().min(available)]);
    payload
}

fn receive_response(socket: &SeqPacket, opcode: Opcode) -> Result<Vec<u8>, BrokerError> {
    let response = socket.receive()?;
    if response.frame.opcode != opcode || !response.fds.is_empty() {
        return Err(BrokerError::new(
            "broker returned a mismatched response frame",
        ));
    }
    let (&status, payload) = response
        .frame
        .payload
        .split_first()
        .ok_or_else(|| BrokerError::new("broker returned an empty response"))?;
    match status {
        RESPONSE_OK => Ok(payload.to_vec()),
        RESPONSE_ERROR => Err(BrokerError::new(format!(
            "broker rejected request: {}",
            String::from_utf8_lossy(payload)
        ))),
        _ => Err(BrokerError::new(
            "broker returned an unknown response status",
        )),
    }
}

fn encode_file_result(length: u64, hash: [u8; 32]) -> Vec<u8> {
    let mut encoder = Encoder::default();
    encoder.u64(length);
    encoder.array(hash);
    encoder.finish()
}

fn decode_file_result(bytes: &[u8]) -> Result<(u64, [u8; 32]), BrokerError> {
    let mut decoder = Decoder::new(bytes);
    let length = decoder.u64()?;
    let hash = decoder.array::<32>()?;
    decoder.finish()?;
    Ok((length, hash))
}

fn io_error(context: &str) -> BrokerError {
    BrokerError::new(format!("{context}: {}", io::Error::last_os_error()))
}

#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn array<const N: usize>(&mut self, value: [u8; N]) {
        self.bytes.extend_from_slice(&value);
    }

    fn byte_string(&mut self, value: &[u8]) -> Result<(), BrokerError> {
        self.u32(
            u32::try_from(value.len()).map_err(|_| BrokerError::new("byte string exceeds u32"))?,
        );
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn expected_subvolume(&mut self, value: &ExpectedSubvolume) {
        self.array(value.filesystem_uuid);
        self.array(value.subvolume_uuid);
        self.u64(value.root_id);
        self.u64(value.generation);
        self.u64(value.ctransid);
        self.u64(value.otransid);
        self.optional_uuid(value.parent_uuid);
        self.optional_uuid(value.received_uuid);
        self.u8(u8::from(value.readonly));
    }

    fn managed_directory(&mut self, value: &ExpectedManagedDirectory) {
        self.array(value.filesystem_uuid);
        self.u64(value.device);
        self.u64(value.inode);
        self.u32(value.owner_uid);
        self.u32(value.mode);
        self.array(value.security_context_hash);
    }

    fn receipt(&mut self, value: &ReceiptRequest) {
        self.array(value.id);
        self.array(value.manager_store_uuid);
        self.array(value.manager_session_id);
        self.array(value.operation_id);
        self.i64(value.operation_fence);
        self.u8(match value.effect_kind {
            EffectKind::SnapshotCreate => 1,
            EffectKind::WorktreeRename => 2,
            EffectKind::SnapshotDelete => 3,
        });
        self.array(value.filesystem_uuid);
        self.array(value.target_locator_hash);
        self.array(value.effect_arguments_hash);
        self.array(value.boot_id);
        self.i64(value.started_ns);
    }

    fn reservation(&mut self, value: &ExpectedReservation) -> Result<(), BrokerError> {
        self.byte_string(&value.name)?;
        self.u64(value.device);
        self.u64(value.inode);
        self.u32(value.owner_uid);
        self.array(value.nonce);
        Ok(())
    }

    fn snapshot_create(&mut self, value: &SnapshotCreateExecution) -> Result<(), BrokerError> {
        self.receipt(&value.receipt);
        self.expected_subvolume(&value.source);
        self.managed_directory(&value.destination_parent);
        self.byte_string(&value.destination_name)?;
        self.u8(u8::from(value.readonly));
        Ok(())
    }

    fn snapshot_delete(&mut self, value: &SnapshotDeleteExecution) -> Result<(), BrokerError> {
        self.receipt(&value.receipt);
        self.expected_subvolume(&value.target);
        self.managed_directory(&value.destination_parent);
        self.byte_string(&value.destination_name)
    }

    fn worktree_rename(&mut self, value: &WorktreeRenameExecution) -> Result<(), BrokerError> {
        self.receipt(&value.receipt);
        self.expected_subvolume(&value.worktree);
        self.managed_directory(&value.staging_parent);
        self.byte_string(&value.staging_name)?;
        self.managed_directory(&value.destination_parent);
        self.expected_subvolume(&value.destination_root);
        self.managed_directory(&value.destination_root_directory);
        self.byte_string(&value.destination_relative_parent)?;
        self.byte_string(&value.destination_name)?;
        self.reservation(&value.reservation)?;
        self.array(value.authorization_hash);
        Ok(())
    }

    fn optional_uuid(&mut self, value: Option<[u8; 16]>) {
        self.u8(u8::from(value.is_some()));
        if let Some(value) = value {
            self.array(value);
        }
    }

    fn object(&mut self, value: &Object) {
        self.u64(value.ino);
        self.u64(value.generation);
        self.u32(value.mode);
        self.u32(value.nlink);
        self.u64(value.uid);
        self.u64(value.gid);
        self.u64(value.rdev);
        self.u64(value.privilege_flags);
        self.array(value.security_xattr_hash);
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], BrokerError> {
        let end = self
            .cursor
            .checked_add(length)
            .ok_or_else(|| BrokerError::new("broker payload offset overflow"))?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or_else(|| BrokerError::new("truncated broker payload"))?;
        self.cursor = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, BrokerError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, BrokerError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("fixed slice"),
        ))
    }

    fn u16(&mut self) -> Result<u16, BrokerError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("fixed slice"),
        ))
    }

    fn u64(&mut self) -> Result<u64, BrokerError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("fixed slice"),
        ))
    }

    fn i64(&mut self) -> Result<i64, BrokerError> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("fixed slice"),
        ))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], BrokerError> {
        Ok(self.take(N)?.try_into().expect("fixed slice"))
    }

    fn byte_string(&mut self) -> Result<Vec<u8>, BrokerError> {
        let length = usize::try_from(self.u32()?)
            .map_err(|_| BrokerError::new("byte string length exceeds usize"))?;
        Ok(self.take(length)?.to_vec())
    }

    fn optional_uuid(&mut self) -> Result<Option<[u8; 16]>, BrokerError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.array()?)),
            _ => Err(BrokerError::new("invalid optional UUID tag")),
        }
    }

    fn expected_subvolume(&mut self) -> Result<ExpectedSubvolume, BrokerError> {
        let value = ExpectedSubvolume {
            filesystem_uuid: self.array()?,
            subvolume_uuid: self.array()?,
            root_id: self.u64()?,
            generation: self.u64()?,
            ctransid: self.u64()?,
            otransid: self.u64()?,
            parent_uuid: self.optional_uuid()?,
            received_uuid: self.optional_uuid()?,
            readonly: match self.u8()? {
                0 => false,
                1 => true,
                _ => return Err(BrokerError::new("invalid read-only flag")),
            },
        };
        Ok(value)
    }

    fn managed_directory(&mut self) -> Result<ExpectedManagedDirectory, BrokerError> {
        Ok(ExpectedManagedDirectory {
            filesystem_uuid: self.array()?,
            device: self.u64()?,
            inode: self.u64()?,
            owner_uid: self.u32()?,
            mode: self.u32()?,
            security_context_hash: self.array()?,
        })
    }

    fn receipt(&mut self) -> Result<ReceiptRequest, BrokerError> {
        Ok(ReceiptRequest {
            id: self.array()?,
            manager_store_uuid: self.array()?,
            manager_session_id: self.array()?,
            operation_id: self.array()?,
            operation_fence: self.i64()?,
            effect_kind: match self.u8()? {
                1 => EffectKind::SnapshotCreate,
                2 => EffectKind::WorktreeRename,
                3 => EffectKind::SnapshotDelete,
                _ => return Err(BrokerError::new("invalid broker effect kind tag")),
            },
            filesystem_uuid: self.array()?,
            target_locator_hash: self.array()?,
            effect_arguments_hash: self.array()?,
            boot_id: self.array()?,
            started_ns: self.i64()?,
        })
    }

    fn reservation(&mut self) -> Result<ExpectedReservation, BrokerError> {
        Ok(ExpectedReservation {
            name: self.byte_string()?,
            device: self.u64()?,
            inode: self.u64()?,
            owner_uid: self.u32()?,
            nonce: self.array()?,
        })
    }

    fn snapshot_create(&mut self) -> Result<SnapshotCreateExecution, BrokerError> {
        Ok(SnapshotCreateExecution {
            receipt: self.receipt()?,
            source: self.expected_subvolume()?,
            destination_parent: self.managed_directory()?,
            destination_name: self.byte_string()?,
            readonly: match self.u8()? {
                0 => false,
                1 => true,
                _ => return Err(BrokerError::new("invalid snapshot read-only flag")),
            },
        })
    }

    fn snapshot_delete(&mut self) -> Result<SnapshotDeleteExecution, BrokerError> {
        Ok(SnapshotDeleteExecution {
            receipt: self.receipt()?,
            target: self.expected_subvolume()?,
            destination_parent: self.managed_directory()?,
            destination_name: self.byte_string()?,
        })
    }

    fn worktree_rename(&mut self) -> Result<WorktreeRenameExecution, BrokerError> {
        Ok(WorktreeRenameExecution {
            receipt: self.receipt()?,
            worktree: self.expected_subvolume()?,
            staging_parent: self.managed_directory()?,
            staging_name: self.byte_string()?,
            destination_parent: self.managed_directory()?,
            destination_root: self.expected_subvolume()?,
            destination_root_directory: self.managed_directory()?,
            destination_relative_parent: self.byte_string()?,
            destination_name: self.byte_string()?,
            reservation: self.reservation()?,
            authorization_hash: self.array()?,
        })
    }

    fn object(&mut self) -> Result<Object, BrokerError> {
        Ok(Object {
            ino: self.u64()?,
            generation: self.u64()?,
            mode: self.u32()?,
            nlink: self.u32()?,
            uid: self.u64()?,
            gid: self.u64()?,
            rdev: self.u64()?,
            privilege_flags: self.u64()?,
            security_xattr_hash: self.array()?,
        })
    }

    fn finish(self) -> Result<(), BrokerError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(BrokerError::new("broker payload contains trailing bytes"))
        }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{MODE_DIRECTORY, ROOT_INO};
    use std::os::fd::AsFd;
    use std::thread;

    fn object(ino: u64, mode: u32) -> Object {
        Object {
            ino,
            generation: 7,
            mode,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            privilege_flags: 0,
            security_xattr_hash: [ino as u8; 32],
        }
    }

    #[test]
    fn index_wire_round_trips_hardlinks_and_raw_names() {
        let mut index = Index::default();
        index
            .objects
            .insert(ROOT_INO, object(ROOT_INO, MODE_DIRECTORY | 0o700));
        index.objects.insert(300, object(300, 0o100600));
        index.references.insert(Reference {
            ino: 300,
            parent_ino: ROOT_INO,
            name: b"a".to_vec(),
        });
        index.references.insert(Reference {
            ino: 300,
            parent_ino: ROOT_INO,
            name: b"b\xff".to_vec(),
        });
        assert_eq!(decode_index(&encode_index(&index).unwrap()).unwrap(), index);
    }

    #[test]
    fn handshake_authenticates_peer_and_fences_old_session() {
        let uid = unsafe { libc::geteuid() };
        let (client_socket, server_socket) = SeqPacket::pair().unwrap();
        match server_socket.peer_credentials() {
            Ok(_) => {}
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => return,
            Err(error) => panic!("read test peer credentials: {error}"),
        }
        let server = thread::spawn(move || {
            let dispatcher = BrokerDispatcher::new(uid);
            dispatcher.serve_one(&server_socket).unwrap();
            dispatcher.serve_one(&server_socket).unwrap();
        });
        let client = BrokerClient::connect(client_socket, [9; 16]).unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(client.inspect_subvolume(null.as_fd()).is_err());
        server.join().unwrap();
    }

    #[test]
    fn target_object_wire_rejects_trailing_data() {
        let objects = BTreeMap::from([(300, object(300, 0o100600))]);
        let mut bytes = encode_objects(&objects).unwrap();
        bytes.push(0);
        assert!(decode_objects(&bytes).is_err());
    }
}
