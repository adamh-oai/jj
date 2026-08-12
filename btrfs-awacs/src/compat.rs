//! Authenticated direct-scan cursors and conservative path projections.

use crate::index::{Event, EventKind};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
#[cfg(test)]
use std::fmt;

const DIRECT_SCAN_CURSOR_PREFIX: &str = "c:btrfs-awacs:scan:1:";
const DIRECT_SCAN_CURSOR_DOMAIN: &[u8] = b"btrfs-awacs:scan:1\0";
const CURSOR_PAYLOAD_BYTES: usize = 113;
#[cfg(test)]
const CURSOR_MAC_BYTES: usize = 32;

/// Claims bound into one opaque direct-scan cursor.
///
/// The durable manager still calls the epoch field clock_epoch in its schema.
/// The cursor authenticates only the immutable boundary needed for the next
/// scan.
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
pub(crate) fn encode_direct_scan_cursor(claims: &CursorClaims, key: &[u8; 32]) -> Vec<u8> {
    let payload = encode_claims(claims);
    let mut mac_input = DIRECT_SCAN_CURSOR_DOMAIN.to_vec();
    mac_input.extend_from_slice(&payload);
    let mac = hmac_sha256(key, &mac_input);
    let mut authenticated = payload;
    authenticated.extend_from_slice(&mac);
    format!(
        "{DIRECT_SCAN_CURSOR_PREFIX}{}",
        base64url_encode(&authenticated)
    )
    .into_bytes()
}

/// Verifies and decodes an opaque direct-scan cursor.
#[cfg(test)]
pub(crate) fn decode_direct_scan_cursor(
    token: &[u8],
    key: &[u8; 32],
) -> Result<CursorClaims, CompatError> {
    let token = std::str::from_utf8(token)
        .map_err(|_| CompatError::new("direct scan cursor is not UTF-8"))?;
    let encoded = token
        .strip_prefix(DIRECT_SCAN_CURSOR_PREFIX)
        .ok_or_else(|| CompatError::new("direct scan cursor has an invalid prefix"))?;
    if encoded.is_empty()
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(CompatError::new(
            "direct scan cursor has invalid base64url characters",
        ));
    }
    let authenticated = base64url_decode(encoded)?;
    if authenticated.len() != CURSOR_PAYLOAD_BYTES + CURSOR_MAC_BYTES {
        return Err(CompatError::new(
            "direct scan cursor has an invalid payload length",
        ));
    }
    let (payload, supplied_mac) = authenticated.split_at(CURSOR_PAYLOAD_BYTES);
    let mut mac_input = DIRECT_SCAN_CURSOR_DOMAIN.to_vec();
    mac_input.extend_from_slice(payload);
    if !constant_time_eq(supplied_mac, &hmac_sha256(key, &mac_input)) {
        return Err(CompatError::new("direct scan cursor authentication failed"));
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

#[cfg(test)]
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

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0_u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = block;
    let mut outer_pad = block;
    for byte in &mut inner_pad {
        *byte ^= 0x36;
    }
    for byte in &mut outer_pad {
        *byte ^= 0x5c;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[cfg(test)]
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatError {
    message: String,
}

#[cfg(test)]
impl CompatError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[cfg(test)]
impl fmt::Display for CompatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[cfg(test)]
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
        let key = [7; 32];
        let cursor = encode_direct_scan_cursor(&claims(), &key);
        assert_eq!(decode_direct_scan_cursor(&cursor, &key).unwrap(), claims());
        assert!(
            cursor
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b":._-".contains(byte))
        );
        let mut tampered = cursor;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(decode_direct_scan_cursor(&tampered, &key).is_err());
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
