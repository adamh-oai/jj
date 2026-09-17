//! Integrity-checked direct-scan cursors and conservative path projections.

use crate::index::{Event, EventKind};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;

const DIRECT_SCAN_CURSOR_PREFIX: &str = "c:btrfs-awacs:scan:2:";
const LEGACY_DIRECT_SCAN_CURSOR_PREFIX: &str = "c:btrfs-awacs:scan:1:";
const DIRECT_SCAN_CURSOR_DOMAIN: &[u8] = b"btrfs-awacs:scan:2\0";
const CURSOR_PAYLOAD_BYTES: usize = 113;
const CURSOR_CHECKSUM_BYTES: usize = 32;

/// Claims bound into one opaque direct-scan cursor.
///
/// The durable manager still calls the epoch field clock_epoch in its schema.
/// The cursor records only the immutable boundary needed for the next scan;
/// manager, watch, grant, session, and snapshot ownership are checked against
/// durable AWACS state when it is consumed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorClaims {
    pub format_version: u32,
    pub store_uuid: [u8; 16],
    pub watch_id: [u8; 16],
    pub cursor_epoch: [u8; 16],
    pub cut_sequence: u64,
    pub owner_grant_id: [u8; 16],
    pub monitor_session_id: [u8; 16],
    pub boundary_kind: BoundaryKind,
    pub algorithm_version: u32,
    pub target_snapshot_uuid: [u8; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryKind {
    Cut,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Projection {
    pub fresh_instance: bool,
    pub paths: Vec<Vec<u8>>,
    /// Roots whose descendants may have changed without individual inode
    /// events, such as a renamed directory subtree.
    pub prefixes: Vec<Vec<u8>>,
}

/// Encodes exact immutable-boundary claims in the direct-scan cursor domain.
pub(crate) fn encode_direct_scan_cursor(claims: &CursorClaims) -> Vec<u8> {
    let payload = encode_claims(claims);
    let checksum = cursor_checksum(&payload);
    let mut checked = payload;
    checked.extend_from_slice(&checksum);
    format!("{DIRECT_SCAN_CURSOR_PREFIX}{}", base64url_encode(&checked)).into_bytes()
}

/// Verifies and decodes an opaque direct-scan cursor.
pub(crate) fn decode_direct_scan_cursor(token: &[u8]) -> Result<CursorClaims, CompatError> {
    let token = std::str::from_utf8(token)
        .map_err(|_| CompatError::new("direct scan cursor is not UTF-8"))?;
    let (encoded, legacy) = if let Some(encoded) = token.strip_prefix(DIRECT_SCAN_CURSOR_PREFIX) {
        (encoded, false)
    } else if let Some(encoded) = token.strip_prefix(LEGACY_DIRECT_SCAN_CURSOR_PREFIX) {
        // Existing cursors already survive only when their exact watch,
        // session, snapshot, and committed pin still match manager state.
        // Read their old trailing bytes without retaining the retired keyed
        // verification machinery; the next scan replaces them with v2.
        (encoded, true)
    } else {
        return Err(CompatError::new("direct scan cursor has an invalid prefix"));
    };
    if encoded.is_empty()
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(CompatError::new(
            "direct scan cursor has invalid base64url characters",
        ));
    }
    let checked = base64url_decode(encoded)?;
    if checked.len() != CURSOR_PAYLOAD_BYTES + CURSOR_CHECKSUM_BYTES {
        return Err(CompatError::new(
            "direct scan cursor has an invalid payload length",
        ));
    }
    let (payload, supplied_checksum) = checked.split_at(CURSOR_PAYLOAD_BYTES);
    if !legacy && supplied_checksum != cursor_checksum(payload) {
        return Err(CompatError::new("direct scan cursor checksum mismatch"));
    }
    decode_claims(payload)
}

/// Converts canonical snapshot-comparison events into direct-scan
/// invalidations.
///
/// Exact path events remain exact. A subtree move names its old and new roots
/// as recursive prefixes: descendants keep the same inode references, so the
/// kernel does not emit one event per moved child.
pub fn project_events(events: &[Event]) -> Projection {
    let mut paths = BTreeSet::new();
    let mut prefixes = BTreeSet::new();
    let mut fresh = false;
    for event in events {
        match event.kind {
            EventKind::DirectoryDirtyWitness => {}
            EventKind::SubtreeMoved => {
                for path in event.old_path.iter().chain(&event.new_path) {
                    if path.is_empty() {
                        fresh = true;
                    } else {
                        prefixes.insert(path.clone());
                    }
                }
            }
            EventKind::PathAdded | EventKind::PathRemoved | EventKind::PathChanged => {
                for path in event.old_path.iter().chain(&event.new_path) {
                    if !path.is_empty() {
                        paths.insert(path.clone());
                    }
                }
            }
        }
    }
    if fresh {
        Projection {
            fresh_instance: true,
            paths: vec![b"/".to_vec()],
            prefixes: Vec::new(),
        }
    } else {
        Projection {
            fresh_instance: false,
            paths: paths.into_iter().collect(),
            prefixes: prefixes.into_iter().collect(),
        }
    }
}

fn encode_claims(claims: &CursorClaims) -> Vec<u8> {
    let mut output = Vec::with_capacity(CURSOR_PAYLOAD_BYTES);
    output.extend_from_slice(&claims.format_version.to_be_bytes());
    output.extend_from_slice(&claims.store_uuid);
    output.extend_from_slice(&claims.watch_id);
    output.extend_from_slice(&claims.cursor_epoch);
    output.extend_from_slice(&claims.cut_sequence.to_be_bytes());
    output.extend_from_slice(&claims.owner_grant_id);
    output.extend_from_slice(&claims.monitor_session_id);
    output.push(match claims.boundary_kind {
        BoundaryKind::Cut => 0,
    });
    output.extend_from_slice(&claims.algorithm_version.to_be_bytes());
    output.extend_from_slice(&claims.target_snapshot_uuid);
    debug_assert_eq!(output.len(), CURSOR_PAYLOAD_BYTES);
    output
}

fn decode_claims(payload: &[u8]) -> Result<CursorClaims, CompatError> {
    let mut offset = 0;
    let mut take = |length: usize| {
        let bytes = &payload[offset..offset + length];
        offset += length;
        bytes
    };
    let format_version = u32::from_be_bytes(take(4).try_into().expect("fixed field"));
    if format_version != 1 {
        return Err(CompatError::new("unsupported cursor format version"));
    }
    let store_uuid = take(16).try_into().expect("fixed field");
    let watch_id = take(16).try_into().expect("fixed field");
    let cursor_epoch = take(16).try_into().expect("fixed field");
    let cut_sequence = u64::from_be_bytes(take(8).try_into().expect("fixed field"));
    let owner_grant_id = take(16).try_into().expect("fixed field");
    let monitor_session_id = take(16).try_into().expect("fixed field");
    let boundary_kind = match take(1)[0] {
        0 => BoundaryKind::Cut,
        _ => return Err(CompatError::new("unknown cursor boundary kind")),
    };
    let algorithm_version = u32::from_be_bytes(take(4).try_into().expect("fixed field"));
    let target_snapshot_uuid = take(16).try_into().expect("fixed field");
    Ok(CursorClaims {
        format_version,
        store_uuid,
        watch_id,
        cursor_epoch,
        cut_sequence,
        owner_grant_id,
        monitor_session_id,
        boundary_kind,
        algorithm_version,
        target_snapshot_uuid,
    })
}

fn cursor_checksum(payload: &[u8]) -> [u8; CURSOR_CHECKSUM_BYTES] {
    let mut digest = Sha256::new();
    digest.update(DIRECT_SCAN_CURSOR_DOMAIN);
    digest.update(payload);
    digest.finalize().into()
}

fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(char::from(ALPHABET[((word >> 18) & 63) as usize]));
        output.push(char::from(ALPHABET[((word >> 12) & 63) as usize]));
        if chunk.len() > 1 {
            output.push(char::from(ALPHABET[((word >> 6) & 63) as usize]));
        }
        if chunk.len() > 2 {
            output.push(char::from(ALPHABET[(word & 63) as usize]));
        }
    }
    output
}

fn base64url_decode(encoded: &str) -> Result<Vec<u8>, CompatError> {
    if encoded.len() % 4 == 1 {
        return Err(CompatError::new("invalid base64url length"));
    }
    let mut output = Vec::with_capacity(encoded.len() * 3 / 4);
    for chunk in encoded.as_bytes().chunks(4) {
        let mut word = 0_u32;
        for index in 0..4 {
            word <<= 6;
            if let Some(byte) = chunk.get(index) {
                word |= u32::from(base64_value(*byte)?);
            }
        }
        output.push((word >> 16) as u8);
        if chunk.len() > 2 {
            output.push((word >> 8) as u8);
        }
        if chunk.len() > 3 {
            output.push(word as u8);
        }
    }
    Ok(output)
}

fn base64_value(byte: u8) -> Result<u8, CompatError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'-' => Ok(62),
        b'_' => Ok(63),
        _ => Err(CompatError::new("invalid base64url character")),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatError {
    message: String,
}

impl CompatError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CompatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CompatError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::CHANGE_FILE_DATA;

    fn claims() -> CursorClaims {
        CursorClaims {
            format_version: 1,
            store_uuid: [1; 16],
            watch_id: [2; 16],
            cursor_epoch: [3; 16],
            cut_sequence: 42,
            owner_grant_id: [4; 16],
            monitor_session_id: [5; 16],
            boundary_kind: BoundaryKind::Cut,
            algorithm_version: 1,
            target_snapshot_uuid: [6; 16],
        }
    }

    fn event(kind: EventKind, path: &[u8]) -> Event {
        Event {
            kind,
            ino: 300,
            old_generation: Some(1),
            new_generation: Some(1),
            change_mask: CHANGE_FILE_DATA,
            old_path: Some(path.to_vec()),
            new_path: Some(path.to_vec()),
        }
    }

    #[test]
    fn direct_scan_cursor_round_trips_and_rejects_tampering() {
        let cursor = encode_direct_scan_cursor(&claims());
        assert!(cursor.starts_with(DIRECT_SCAN_CURSOR_PREFIX.as_bytes()));
        assert_eq!(decode_direct_scan_cursor(&cursor).unwrap(), claims());
        assert!(
            cursor
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b":._-".contains(byte))
        );
        let mut tampered = cursor;
        let changed = tampered.len() - 10;
        tampered[changed] = if tampered[changed] == b'A' {
            b'B'
        } else {
            b'A'
        };
        assert!(
            decode_direct_scan_cursor(&tampered)
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
    }

    #[test]
    fn legacy_direct_scan_cursor_claims_remain_readable_without_a_key() {
        let mut legacy_payload = encode_claims(&claims());
        legacy_payload.extend_from_slice(&[7; CURSOR_CHECKSUM_BYTES]);
        let legacy = format!(
            "{LEGACY_DIRECT_SCAN_CURSOR_PREFIX}{}",
            base64url_encode(&legacy_payload)
        );
        assert_eq!(
            decode_direct_scan_cursor(legacy.as_bytes()).unwrap(),
            claims()
        );
    }

    #[test]
    fn directory_witness_does_not_change_direct_projection() {
        let events = vec![
            event(EventKind::PathChanged, b"hardlink"),
            event(EventKind::DirectoryDirtyWitness, b"dir"),
            event(EventKind::PathChanged, b".git/index"),
        ];
        assert_eq!(
            project_events(&events),
            Projection {
                fresh_instance: false,
                paths: vec![b".git/index".to_vec(), b"hardlink".to_vec()],
                prefixes: Vec::new(),
            }
        );
    }

    #[test]
    fn subtree_move_projects_recursive_roots() {
        let events = vec![event(EventKind::SubtreeMoved, b".jj/repo/op_store")];
        assert_eq!(
            project_events(&events),
            Projection {
                fresh_instance: false,
                paths: Vec::new(),
                prefixes: vec![b".jj/repo/op_store".to_vec()],
            }
        );
    }
}
