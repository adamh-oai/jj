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

//! Bounded framing and capability negotiation for repository synchronization.
//!
//! Frames protect the bytes in transit. Object identities, including operation
//! IDs whose hash is not a hash of their protobuf bytes, are validated by the
//! receiving object store. Nothing in this module installs repository data.

use std::io;
use std::io::Read;
use std::io::Write;

use sha2::Digest as _;
use sha2::Sha256;
use thiserror::Error;

/// The only currently supported synchronization wire protocol.
pub const PROTOCOL_VERSION: u32 = 1;
/// Maximum payload size of a single frame, checked before allocating a buffer.
///
/// This admits a bounded object record containing a 64 MiB object and metadata.
/// Larger transfers use multiple records rather than an unbounded frame.
pub const MAX_FRAME_LEN: usize = 128 * 1024 * 1024;
/// Maximum length of a name or repository path inside a control message.
pub const MAX_CONTROL_FIELD_LEN: usize = 1024 * 1024;
const MAX_CAPABILITIES: usize = 256;
const MAGIC: &[u8; 8] = b"JJSYNC\0\x01";
const HEADER_LEN: usize = 8 + 1 + 8 + 32;

/// Synchronization direction, relative to the initiating endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    /// Import the helper endpoint's captured history into the initiator.
    Receive,
    /// Import the initiator's captured history into the helper endpoint.
    Send,
    /// Import both captured histories at their opposite endpoints.
    Bidirectional,
}

impl Direction {
    fn encode(self) -> u8 {
        match self {
            Self::Receive => 0,
            Self::Send => 1,
            Self::Bidirectional => 2,
        }
    }

    fn decode(value: u8) -> Result<Self, ProtocolError> {
        match value {
            0 => Ok(Self::Receive),
            1 => Ok(Self::Send),
            2 => Ok(Self::Bidirectional),
            _ => Err(invalid("unknown synchronization direction")),
        }
    }
}

/// Parameters exchanged before either endpoint installs repository data.
///
/// `repository_path` is opaque path data interpreted only by the helper, never
/// part of the SSH command. The reply can leave it empty. Backend, hash, and
/// operation-store values identify data formats, not implementation versions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hello {
    /// Protocol version supported by this session.
    pub version: u32,
    /// Requested direction, echoed unchanged by the helper.
    pub direction: Direction,
    /// The helper repository's path, encoded by the transport.
    pub repository_path: Vec<u8>,
    /// Skip both endpoints' working-copy snapshots and checkout updates.
    pub ignore_working_copy: bool,
    /// Commit backend data format, for example `git`.
    pub backend: String,
    /// Commit object hash, for example `sha1` or `sha256`.
    pub object_hash: String,
    /// Operation and view record format understood by this endpoint.
    pub operation_store: String,
    /// Capabilities this endpoint implements.
    pub capabilities: Vec<String>,
    /// Capabilities the peer must implement for the requested session.
    pub required_capabilities: Vec<String>,
}

impl Hello {
    /// Rejects an incompatible peer before transferring or installing objects.
    ///
    /// Both peers must call this with their own parameters as `self`. Unknown
    /// optional capabilities are permitted; unknown required ones are not.
    pub fn validate_peer(&self, peer: &Self) -> Result<(), ProtocolError> {
        self.validate()?;
        peer.validate()?;
        if self.direction != peer.direction {
            return Err(incompatible("peer changed the requested direction"));
        }
        if self.ignore_working_copy != peer.ignore_working_copy {
            return Err(incompatible("peer changed --ignore-working-copy"));
        }
        for (field, own, other) in [
            ("backend", &self.backend, &peer.backend),
            ("object hash", &self.object_hash, &peer.object_hash),
            (
                "operation store format",
                &self.operation_store,
                &peer.operation_store,
            ),
        ] {
            if own != other {
                return Err(incompatible(format!(
                    "{field} differs: local {own:?}, peer {other:?}"
                )));
            }
        }
        for capability in &self.required_capabilities {
            if !peer.capabilities.contains(capability) {
                return Err(incompatible(format!(
                    "peer does not support required capability {capability:?}"
                )));
            }
        }
        for capability in &peer.required_capabilities {
            if !self.capabilities.contains(capability) {
                return Err(incompatible(format!(
                    "local endpoint does not support peer's required capability {capability:?}"
                )));
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ProtocolError> {
        if self.version != PROTOCOL_VERSION {
            return Err(incompatible(format!(
                "protocol version {} is unsupported (this jj supports {PROTOCOL_VERSION}); \
                 install compatible jj versions at both endpoints",
                self.version
            )));
        }
        for (name, value) in [
            ("backend", &self.backend),
            ("object hash", &self.object_hash),
            ("operation store format", &self.operation_store),
        ] {
            if value.is_empty() || value.len() > MAX_CONTROL_FIELD_LEN {
                return Err(invalid(format!("invalid {name}")));
            }
        }
        if self.repository_path.len() > MAX_CONTROL_FIELD_LEN || self.repository_path.contains(&0) {
            return Err(invalid("invalid repository path"));
        }
        for capabilities in [&self.capabilities, &self.required_capabilities] {
            if capabilities.len() > MAX_CAPABILITIES {
                return Err(invalid("too many capabilities"));
            }
            for (index, capability) in capabilities.iter().enumerate() {
                if capability.is_empty()
                    || capability.len() > MAX_CONTROL_FIELD_LEN
                    || capabilities[..index].contains(capability)
                {
                    return Err(invalid("invalid or repeated capability name"));
                }
            }
        }
        if self
            .required_capabilities
            .iter()
            .any(|capability| !self.capabilities.contains(capability))
        {
            return Err(invalid("required capability is not locally implemented"));
        }
        Ok(())
    }
}

/// A control message or an opaque object record.
///
/// The command protocol defines the payload of `Command`, `Record`, and `Done`.
/// Those payloads may contain arbitrary bytes, including zeroes. Control message
/// strings use UTF-8 and all encoded lengths are exact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Message {
    /// Initial request, including the helper repository's path.
    Hello(Hello),
    /// Negotiation response, sent before any object records.
    HelloReply(Hello),
    /// Named protocol phase, with an opaque phase-specific body.
    Command {
        /// Phase name defined by the command protocol.
        name: String,
        /// Phase-specific binary payload.
        payload: Vec<u8>,
    },
    /// One encoded object record; object codecs perform semantic validation.
    Record(Vec<u8>),
    /// A completed protocol phase, with an opaque result body.
    Done(Vec<u8>),
    /// A peer's human-readable failure, followed by connection termination.
    Error(String),
}

/// A framing, stream, or negotiation error.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// A read, write, or flush failed.
    #[error("Synchronization stream failed: {0}")]
    Io(#[from] io::Error),
    /// The helper exited, or the stream ended in the middle of a frame.
    #[error(
        "Synchronization stream ended while reading {0}; the helper may have exited or may not \
         support sync"
    )]
    Truncated(&'static str),
    /// Unexpected stdout, including a non-protocol remote helper.
    #[error(
        "Peer did not send a jj synchronization frame; check that its jj supports sync and its \
         startup scripts do not print to stdout"
    )]
    InvalidMagic,
    /// A declared length exceeds the permitted allocation.
    #[error("Synchronization frame has {length} bytes, exceeding the limit of {maximum}")]
    FrameTooLarge {
        /// Declared frame length.
        length: u64,
        /// Largest permitted frame length.
        maximum: usize,
    },
    /// Header or payload bytes have changed in transit.
    #[error("Synchronization frame checksum does not match")]
    CorruptFrame,
    /// A decoded message violates its format.
    #[error("Invalid synchronization message: {0}")]
    InvalidMessage(String),
    /// Endpoint negotiation could not establish compatible formats.
    #[error("Incompatible synchronization endpoints: {0}")]
    Incompatible(String),
}

/// Bidirectional framed I/O. The owner is responsible for the helper process.
pub struct Session<R, W> {
    reader: R,
    writer: W,
}

impl<R: Read, W: Write> Session<R, W> {
    /// Wraps a pair of byte streams, without performing any negotiation.
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    /// Writes exactly one frame and flushes it for an interactive peer.
    pub fn send(&mut self, message: &Message) -> Result<(), ProtocolError> {
        let (kind, payload) = encode_message(message)?;
        let length = (payload.len() as u64).to_be_bytes();
        let checksum = checksum(kind, &length, &payload);
        self.writer.write_all(MAGIC)?;
        self.writer.write_all(&[kind])?;
        self.writer.write_all(&length)?;
        self.writer.write_all(&checksum)?;
        self.writer.write_all(&payload)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Reads a complete bounded frame, rejecting truncated or corrupt data.
    pub fn receive(&mut self) -> Result<Message, ProtocolError> {
        self.receive_bounded(MAX_FRAME_LEN)
    }

    /// Applies a smaller phase-specific allocation limit before reading a body.
    pub fn receive_bounded(&mut self, maximum: usize) -> Result<Message, ProtocolError> {
        let maximum = maximum.min(MAX_FRAME_LEN);
        let mut header = [0; HEADER_LEN];
        read_exact(&mut self.reader, &mut header[..8], "frame header")?;
        if &header[..8] != MAGIC {
            return Err(ProtocolError::InvalidMagic);
        }
        read_exact(&mut self.reader, &mut header[8..], "frame header")?;
        let kind = header[8];
        let length_bytes: [u8; 8] = header[9..17].try_into().unwrap();
        let length = u64::from_be_bytes(length_bytes);
        if length > maximum as u64 {
            return Err(ProtocolError::FrameTooLarge { length, maximum });
        }
        let mut payload = vec![0; length as usize];
        read_exact(&mut self.reader, &mut payload, "frame payload")?;
        if checksum(kind, &length_bytes, &payload).as_slice() != &header[17..] {
            return Err(ProtocolError::CorruptFrame);
        }
        decode_message(kind, &payload)
    }

    /// Returns the underlying streams, for controlled shutdown or testing.
    pub fn into_inner(self) -> (R, W) {
        (self.reader, self.writer)
    }

    /// Borrows the independent streams for a scoped, full-duplex protocol phase.
    ///
    /// Callers must finish that phase before resuming ordinary session framing.
    pub fn split_mut(&mut self) -> (&mut R, &mut W) {
        (&mut self.reader, &mut self.writer)
    }
}

fn read_exact(
    reader: &mut impl Read,
    bytes: &mut [u8],
    part: &'static str,
) -> Result<(), ProtocolError> {
    reader.read_exact(bytes).map_err(|err| {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            ProtocolError::Truncated(part)
        } else {
            ProtocolError::Io(err)
        }
    })
}

fn checksum(kind: u8, length: &[u8; 8], payload: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update([kind]);
    hash.update(length);
    hash.update(payload);
    hash.finalize().into()
}

fn encode_message(message: &Message) -> Result<(u8, Vec<u8>), ProtocolError> {
    let mut bytes = Vec::new();
    let kind = match message {
        Message::Hello(hello) | Message::HelloReply(hello) => {
            hello.validate()?;
            bytes.extend_from_slice(&hello.version.to_be_bytes());
            bytes.push(hello.direction.encode());
            bytes.push(u8::from(hello.ignore_working_copy));
            put_field(&mut bytes, &hello.repository_path, MAX_CONTROL_FIELD_LEN)?;
            for value in [&hello.backend, &hello.object_hash, &hello.operation_store] {
                put_field(&mut bytes, value.as_bytes(), MAX_CONTROL_FIELD_LEN)?;
            }
            for values in [&hello.capabilities, &hello.required_capabilities] {
                bytes.extend_from_slice(&(values.len() as u32).to_be_bytes());
                for value in values {
                    put_field(&mut bytes, value.as_bytes(), MAX_CONTROL_FIELD_LEN)?;
                }
            }
            if matches!(message, Message::Hello(_)) {
                1
            } else {
                2
            }
        }
        Message::Command { name, payload } => {
            if name.is_empty() {
                return Err(invalid("empty command name"));
            }
            put_field(&mut bytes, name.as_bytes(), MAX_CONTROL_FIELD_LEN)?;
            put_field(&mut bytes, payload, MAX_FRAME_LEN)?;
            3
        }
        Message::Record(payload) => {
            check_frame_length(payload.len())?;
            bytes.extend_from_slice(payload);
            4
        }
        Message::Done(payload) => {
            check_frame_length(payload.len())?;
            bytes.extend_from_slice(payload);
            5
        }
        Message::Error(message) => {
            put_field(&mut bytes, message.as_bytes(), MAX_CONTROL_FIELD_LEN)?;
            6
        }
    };
    check_frame_length(bytes.len())?;
    Ok((kind, bytes))
}

fn decode_message(kind: u8, bytes: &[u8]) -> Result<Message, ProtocolError> {
    let mut decoder = Decoder(bytes);
    let message = match kind {
        1 | 2 => {
            let version = decoder.u32()?;
            let direction = Direction::decode(decoder.byte()?)?;
            let ignore_working_copy = match decoder.byte()? {
                0 => false,
                1 => true,
                _ => return Err(invalid("invalid --ignore-working-copy value")),
            };
            let hello = Hello {
                version,
                direction,
                repository_path: decoder.field(MAX_CONTROL_FIELD_LEN)?.to_vec(),
                ignore_working_copy,
                backend: decoder.string()?,
                object_hash: decoder.string()?,
                operation_store: decoder.string()?,
                capabilities: decoder.strings()?,
                required_capabilities: decoder.strings()?,
            };
            hello.validate()?;
            if kind == 1 {
                Message::Hello(hello)
            } else {
                Message::HelloReply(hello)
            }
        }
        3 => {
            let name = decoder.string()?;
            if name.is_empty() {
                return Err(invalid("empty command name"));
            }
            Message::Command {
                name,
                payload: decoder.field(MAX_FRAME_LEN)?.to_vec(),
            }
        }
        4 => return Ok(Message::Record(bytes.to_vec())),
        5 => return Ok(Message::Done(bytes.to_vec())),
        6 => Message::Error(decoder.string()?),
        _ => return Err(invalid(format!("unknown message kind {kind}"))),
    };
    if !decoder.0.is_empty() {
        return Err(invalid("trailing bytes in control message"));
    }
    Ok(message)
}

fn check_frame_length(length: usize) -> Result<(), ProtocolError> {
    if length > MAX_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge {
            length: length as u64,
            maximum: MAX_FRAME_LEN,
        });
    }
    Ok(())
}

fn put_field(bytes: &mut Vec<u8>, value: &[u8], maximum: usize) -> Result<(), ProtocolError> {
    if value.len() > maximum {
        return Err(invalid("field exceeds its size limit"));
    }
    check_frame_length(bytes.len().saturating_add(4).saturating_add(value.len()))?;
    bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

struct Decoder<'a>(&'a [u8]);

impl<'a> Decoder<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], ProtocolError> {
        if self.0.len() < length {
            return Err(invalid("truncated field in control message"));
        }
        let (value, rest) = self.0.split_at(length);
        self.0 = rest;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn field(&mut self, maximum: usize) -> Result<&'a [u8], ProtocolError> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err(invalid("field exceeds its size limit"));
        }
        self.take(length)
    }

    fn string(&mut self) -> Result<String, ProtocolError> {
        let value = self.field(MAX_CONTROL_FIELD_LEN)?;
        String::from_utf8(value.to_vec()).map_err(|_| invalid("invalid UTF-8 in control message"))
    }

    fn strings(&mut self) -> Result<Vec<String>, ProtocolError> {
        let count = self.u32()? as usize;
        if count > MAX_CAPABILITIES {
            return Err(invalid("too many capabilities"));
        }
        (0..count).map(|_| self.string()).collect()
    }
}

fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError::InvalidMessage(message.into())
}

fn incompatible(message: impl Into<String>) -> ProtocolError {
    ProtocolError::Incompatible(message.into())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn hello() -> Hello {
        Hello {
            version: PROTOCOL_VERSION,
            direction: Direction::Bidirectional,
            repository_path: b"/some repo/';$(literal)".to_vec(),
            ignore_working_copy: false,
            backend: "git".to_owned(),
            object_hash: "sha1".to_owned(),
            operation_store: "simple-v1".to_owned(),
            capabilities: vec!["immutable-records-v1".to_owned()],
            required_capabilities: vec!["immutable-records-v1".to_owned()],
        }
    }

    fn frame(message: &Message) -> Vec<u8> {
        let mut session = Session::new(io::empty(), Vec::new());
        session.send(message).unwrap();
        session.into_inner().1
    }

    fn read(bytes: &[u8]) -> Result<Message, ProtocolError> {
        Session::new(Cursor::new(bytes), io::sink()).receive()
    }

    #[test]
    fn messages_round_trip_in_sequence() {
        let messages = vec![
            Message::Hello(hello()),
            Message::HelloReply(hello()),
            Message::Command {
                name: "capture".to_owned(),
                payload: vec![0, 255, 1],
            },
            Message::Record(vec![0, 255, 1, 0]),
            Message::Done(Vec::new()),
            Message::Error("missing helper repository: café".to_owned()),
        ];
        let bytes: Vec<_> = messages.iter().flat_map(frame).collect();
        let mut session = Session::new(Cursor::new(bytes), io::sink());
        for expected in messages {
            assert_eq!(session.receive().unwrap(), expected);
        }
        assert!(matches!(
            session.receive(),
            Err(ProtocolError::Truncated(_))
        ));
    }

    #[test]
    fn rejects_every_truncated_frame_boundary() {
        let bytes = frame(&Message::Hello(hello()));
        for length in 0..bytes.len() {
            assert!(
                matches!(read(&bytes[..length]), Err(ProtocolError::Truncated(_))),
                "accepted frame truncated to {length} bytes"
            );
        }
        assert_eq!(read(&bytes).unwrap(), Message::Hello(hello()));
    }

    #[test]
    fn rejects_changed_payload_header_and_checksum() {
        let bytes = frame(&Message::Record(vec![0, 1, 2, 255]));
        for position in [8, 16, 17, HEADER_LEN] {
            let mut corrupt = bytes.clone();
            corrupt[position] ^= 1;
            assert!(read(&corrupt).is_err());
        }
        let mut corrupt = bytes;
        corrupt[HEADER_LEN] ^= 1;
        assert!(matches!(read(&corrupt), Err(ProtocolError::CorruptFrame)));
    }

    #[test]
    fn rejects_oversized_length_before_reading_payload() {
        let mut bytes = frame(&Message::Done(Vec::new()));
        bytes[9..17].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(matches!(
            read(&bytes),
            Err(ProtocolError::FrameTooLarge {
                length: u64::MAX,
                ..
            })
        ));
    }

    #[test]
    fn rejects_non_protocol_stdout() {
        assert!(matches!(
            read(b"a shell startup banner printed on stdout"),
            Err(ProtocolError::InvalidMagic)
        ));
    }

    #[test]
    fn validates_format_and_required_capabilities() {
        let own = hello();
        own.validate_peer(&own).unwrap();
        let mut peer = own.clone();
        peer.capabilities.push("optional-future-feature".to_owned());
        own.validate_peer(&peer).unwrap();
        peer.required_capabilities
            .push("optional-future-feature".to_owned());
        assert!(matches!(
            own.validate_peer(&peer),
            Err(ProtocolError::Incompatible(_))
        ));
        for change in 0..6 {
            let mut peer = own.clone();
            match change {
                0 => peer.version += 1,
                1 => peer.direction = Direction::Receive,
                2 => peer.ignore_working_copy = true,
                3 => peer.backend = "other".to_owned(),
                4 => peer.object_hash = "sha256".to_owned(),
                5 => peer.operation_store = "other".to_owned(),
                _ => unreachable!(),
            }
            assert!(matches!(
                own.validate_peer(&peer),
                Err(ProtocolError::Incompatible(_))
            ));
        }
    }

    #[test]
    fn validates_exact_control_fields() {
        let (kind, mut payload) = encode_message(&Message::Hello(hello())).unwrap();
        payload.push(0);
        assert!(matches!(
            decode_message(kind, &payload),
            Err(ProtocolError::InvalidMessage(_))
        ));
        let mut invalid_hello = hello();
        invalid_hello
            .capabilities
            .push(invalid_hello.capabilities[0].clone());
        assert!(encode_message(&Message::Hello(invalid_hello)).is_err());
        let mut bytes = Vec::new();
        put_field(&mut bytes, &[255], MAX_CONTROL_FIELD_LEN).unwrap();
        assert!(decode_message(6, &bytes).is_err());
        assert!(decode_message(200, &[]).is_err());
    }
}
