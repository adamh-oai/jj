//! Fixed, bounded manager-to-broker protocol for privileged read operations.
//!
//! The broker receives only immutable endpoint fds plus a private output file
//! for the bounded changed-object stream.

use crate::broker::{
    ChangedObjectsExecution, ChangedObjectsResult, ExpectedSubvolume, Frame, Opcode,
    PeerCredentials, SeqPacket, SessionGate, execute_changed_objects,
};
use crate::btrfs::ChangedObjectsIoctlResult;
use std::os::fd::{AsFd, BorrowedFd};

use crate::broker::BrokerError;

const RESPONSE_OK: u8 = 0;
const RESPONSE_ERROR: u8 = 1;

#[derive(Debug)]
pub struct BrokerDispatcher {
    manager_uid: u32,
    gate: SessionGate,
}

impl BrokerDispatcher {
    pub fn new(manager_uid: u32) -> Self {
        Self {
            manager_uid,
            gate: SessionGate::default(),
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
        // Retain this permit through the complete dispatch and any ioctl. A
        // newer handshake cannot return its recovery barrier until this drops.
        let _session_permit = self.gate.authorize(store_uuid, session_id)?;
        match frame.opcode {
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
                Ok(encode_changed_objects_result(&result))
            }
            Opcode::Handshake => unreachable!("handled before authorization envelope"),
        }
    }
}

#[derive(Debug)]
pub struct BrokerClient {
    socket: SeqPacket,
    store_uuid: [u8; 16],
    session_id: [u8; 16],
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
        decode_changed_objects_result(&response)
    }

    fn auth_encoder(&self) -> Encoder {
        let mut encoder = Encoder::default();
        encoder.array(self.store_uuid);
        encoder.array(self.session_id);
        encoder
    }

    pub fn session_id(&self) -> [u8; 16] {
        self.session_id
    }
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

fn encode_changed_objects_result(result: &ChangedObjectsResult) -> Vec<u8> {
    let mut encoder = Encoder::default();
    encoder.u64(result.output_bytes);
    encoder.array(result.manifest_hash);
    match result.v2_ioctl {
        Some(ioctl) => {
            encoder.u8(1);
            encoder.u64(ioctl.output_bytes);
            encoder.u64(ioctl.output_records);
        }
        None => encoder.u8(0),
    }
    encoder.finish()
}

fn decode_changed_objects_result(bytes: &[u8]) -> Result<ChangedObjectsResult, BrokerError> {
    let mut decoder = Decoder::new(bytes);
    let output_bytes = decoder.u64()?;
    let manifest_hash = decoder.array::<32>()?;
    let v2_ioctl = match decoder.u8()? {
        0 => None,
        1 => Some(ChangedObjectsIoctlResult {
            output_bytes: decoder.u64()?,
            output_records: decoder.u64()?,
        }),
        _ => return Err(BrokerError::new("invalid changed-object result kind")),
    };
    decoder.finish()?;
    Ok(ChangedObjectsResult {
        output_bytes,
        manifest_hash,
        v2_ioctl,
    })
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

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn array<const N: usize>(&mut self, value: [u8; N]) {
        self.bytes.extend_from_slice(&value);
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

    fn optional_uuid(&mut self, value: Option<[u8; 16]>) {
        self.u8(u8::from(value.is_some()));
        if let Some(value) = value {
            self.array(value);
        }
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

    fn u64(&mut self) -> Result<u64, BrokerError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("fixed slice"),
        ))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], BrokerError> {
        Ok(self.take(N)?.try_into().expect("fixed slice"))
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
    use std::thread;

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
        });
        let _client = BrokerClient::connect(client_socket, [9; 16]).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn changed_object_result_wire_preserves_v2_ioctl_counters() {
        let result = ChangedObjectsResult {
            output_bytes: 144,
            manifest_hash: [7; 32],
            v2_ioctl: Some(ChangedObjectsIoctlResult {
                output_bytes: 144,
                output_records: 3,
            }),
        };
        assert_eq!(
            decode_changed_objects_result(&encode_changed_objects_result(&result)).unwrap(),
            result
        );
    }
}
