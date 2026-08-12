//! Shared clock and conservative path-projection primitives for the focused
//! Watchman and Git transports.

use crate::index::{Event, EventKind};
use crate::manager::QueryLeaseReservation;
use crate::store::{decode_u64, Store};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;

const CLOCK_PREFIX: &str = "c:btrfs-awacs:1:";
const CLOCK_PAYLOAD_BYTES: usize = 113;
const CLOCK_MAC_BYTES: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClockClaims {
    pub format_version: u32,
    pub store_uuid: [u8; 16],
    pub watch_id: [u8; 16],
    pub clock_epoch: [u8; 16],
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
    ProvedWorktreeSeed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientFlavor {
    Jj,
    Git,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Projection {
    pub fresh_instance: bool,
    pub paths: Vec<Vec<u8>>,
}

pub fn encode_clock(claims: &ClockClaims, key: &[u8; 32]) -> String {
    let payload = encode_claims(claims);
    let mac = hmac_sha256(key, &payload);
    let mut authenticated = payload;
    authenticated.extend_from_slice(&mac);
    format!("{CLOCK_PREFIX}{}", base64url_encode(&authenticated))
}

pub fn decode_clock(token: &str, key: &[u8; 32]) -> Result<ClockClaims, CompatError> {
    let encoded = token
        .strip_prefix(CLOCK_PREFIX)
        .ok_or_else(|| CompatError::new("clock has an invalid prefix"))?;
    if encoded.is_empty()
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(CompatError::new("clock has invalid base64url characters"));
    }
    let authenticated = base64url_decode(encoded)?;
    if authenticated.len() != CLOCK_PAYLOAD_BYTES + CLOCK_MAC_BYTES {
        return Err(CompatError::new("clock has an invalid payload length"));
    }
    let (payload, supplied_mac) = authenticated.split_at(CLOCK_PAYLOAD_BYTES);
    if !constant_time_eq(supplied_mac, &hmac_sha256(key, payload)) {
        return Err(CompatError::new("clock authentication failed"));
    }
    decode_claims(payload)
}

/// Converts canonical semantic events into the conservative re-stat set used
/// by both clients. Snapshot-only directory witnesses deliberately become a
/// fresh jj result because jj treats returned names as exact paths.
pub fn project_events(events: &[Event], flavor: ClientFlavor) -> Projection {
    let mut paths = BTreeSet::new();
    let mut fresh = false;
    for event in events {
        match event.kind {
            EventKind::DirectoryDirtyWitness => {
                for path in event.old_path.iter().chain(&event.new_path) {
                    if excluded(path, flavor) {
                        continue;
                    }
                    match flavor {
                        ClientFlavor::Jj => fresh = true,
                        ClientFlavor::Git => {
                            paths.insert(directory_prefix(path));
                        }
                    }
                }
            }
            EventKind::SubtreeMoved => {
                let relevant = event
                    .old_path
                    .iter()
                    .chain(&event.new_path)
                    .filter(|path| !excluded(path, flavor))
                    .collect::<Vec<_>>();
                match flavor {
                    // Descendant expansion belongs to the query engine. Until
                    // it is supplied, jj must crawl instead of treating a
                    // relevant directory name as recursive. A move wholly
                    // beneath .git or .jj is a proven nonmatch.
                    ClientFlavor::Jj if !relevant.is_empty() => fresh = true,
                    ClientFlavor::Jj => {}
                    ClientFlavor::Git => {
                        for path in relevant {
                            paths.insert(directory_prefix(path));
                        }
                    }
                }
            }
            EventKind::PathAdded | EventKind::PathRemoved | EventKind::PathChanged => {
                for path in event.old_path.iter().chain(&event.new_path) {
                    if !path.is_empty() && !excluded(path, flavor) {
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
        }
    } else {
        Projection {
            fresh_instance: false,
            paths: paths.into_iter().collect(),
        }
    }
}

impl Store {
    /// Projects every committed adjacent comparison in `(from, target]`.
    /// Missing, expired, future, or non-contiguous ranges become a fresh
    /// baseline instead of a partial incremental success.
    pub fn project_ready_cut_range(
        &self,
        watch_id: [u8; 16],
        from_sequence: Option<i64>,
        target_sequence: i64,
        flavor: ClientFlavor,
    ) -> Result<Projection, CompatError> {
        self.project_ready_cut_range_with_lease(
            watch_id,
            from_sequence,
            target_sequence,
            flavor,
            None,
        )
    }

    pub fn project_ready_cut_range_with_lease(
        &self,
        watch_id: [u8; 16],
        from_sequence: Option<i64>,
        target_sequence: i64,
        flavor: ClientFlavor,
        lease: Option<&QueryLeaseReservation>,
    ) -> Result<Projection, CompatError> {
        let head: Option<(i64, i64)> = self
            .connection()
            .query_row(
                "SELECT indexed_seq, replay_floor_seq FROM watches \
                 WHERE id = ?1 AND state = 'active'",
                [watch_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| CompatError::context("load watch projection head", error))?;
        let Some((indexed_head, replay_floor)) = head else {
            return Err(CompatError::new("watch is absent or inactive"));
        };
        if target_sequence > indexed_head || target_sequence < 0 {
            return Err(CompatError::new("target cut is not a ready indexed head"));
        }
        let Some(from_sequence) = from_sequence else {
            return Ok(fresh_projection());
        };
        if from_sequence < replay_floor || from_sequence < 0 || from_sequence > target_sequence {
            return Ok(fresh_projection());
        }
        let expected = target_sequence - from_sequence;
        let ready_count: i64 = self
            .connection()
            .query_row(
                r#"SELECT count(*) FROM watch_cuts
                    WHERE watch_id = ?1 AND sequence > ?2 AND sequence <= ?3
                      AND state = 'ready' AND fresh_instance = 0
                      AND comparison_id IS NOT NULL"#,
                rusqlite::params![watch_id.as_slice(), from_sequence, target_sequence],
                |row| row.get(0),
            )
            .map_err(|error| CompatError::context("count ready cut range", error))?;
        if ready_count != expected {
            return Ok(fresh_projection());
        }
        let mut statement = self
            .connection()
            .prepare(
                r#"SELECT e.event_kind, e.ino, e.old_generation,
                          e.new_generation, e.change_mask, e.old_path, e.new_path
                     FROM watch_cuts c
                     JOIN change_events e ON e.comparison_id = c.comparison_id
                    WHERE c.watch_id = ?1 AND c.sequence > ?2 AND c.sequence <= ?3
                      AND c.state = 'ready'
                    ORDER BY c.sequence, e.ordinal"#,
            )
            .map_err(|error| CompatError::context("prepare cut-range events", error))?;
        let rows = statement
            .query_map(
                rusqlite::params![watch_id.as_slice(), from_sequence, target_sequence],
                |row| {
                    let kind: String = row.get(0)?;
                    let decode_optional = |column| -> rusqlite::Result<Option<u64>> {
                        let bytes: Option<Vec<u8>> = row.get(column)?;
                        bytes
                            .as_deref()
                            .map(|bytes| {
                                decode_u64(bytes).map_err(|error| {
                                    rusqlite::Error::FromSqlConversionFailure(
                                        column,
                                        rusqlite::types::Type::Blob,
                                        Box::new(error),
                                    )
                                })
                            })
                            .transpose()
                    };
                    Ok(Event {
                        kind: decode_event_kind(&kind).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                        ino: decode_optional(1)?.ok_or_else(|| {
                            rusqlite::Error::InvalidColumnType(
                                1,
                                "ino".to_owned(),
                                rusqlite::types::Type::Null,
                            )
                        })?,
                        old_generation: decode_optional(2)?,
                        new_generation: decode_optional(3)?,
                        change_mask: u64::try_from(row.get::<_, i64>(4)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        old_path: row.get(5)?,
                        new_path: row.get(6)?,
                    })
                },
            )
            .map_err(|error| CompatError::context("read cut-range events", error))?;
        let mut events = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| CompatError::context("decode cut-range events", error))?;
        let Some((guard_epoch, from_guard, to_guard)) = lease.and_then(|lease| lease.guard) else {
            return Ok(project_events(&events, flavor));
        };
        if lease.is_some_and(|lease| {
            lease.watch_id != watch_id
                || lease.from_sequence != Some(from_sequence)
                || lease.to_sequence != target_sequence
        }) {
            return Err(CompatError::new(
                "query lease does not match projected range",
            ));
        }

        // A complete journal interval replaces only the coarse namespace
        // witnesses. Immutable object/ref events remain authoritative and are
        // still unioned below.
        events.retain(|event| event.kind != EventKind::DirectoryDirtyWitness);
        let mut projection = project_events(&events, flavor);
        let mut statement = self
            .connection()
            .prepare(
                r#"SELECT sequence, event_kind, path FROM mutation_events
                    WHERE watch_id = ?1 AND guard_epoch = ?2
                      AND sequence > ?3 AND sequence <= ?4
                    ORDER BY sequence"#,
            )
            .map_err(|error| CompatError::context("prepare precision events", error))?;
        let rows = statement
            .query_map(
                rusqlite::params![
                    watch_id.as_slice(),
                    guard_epoch.as_slice(),
                    from_guard,
                    to_guard,
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                },
            )
            .map_err(|error| CompatError::context("read precision events", error))?;
        let mut paths = projection.paths.into_iter().collect::<BTreeSet<_>>();
        let mut expected_sequence = from_guard;
        for row in rows {
            let (sequence, kind, path) =
                row.map_err(|error| CompatError::context("decode precision event", error))?;
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or_else(|| CompatError::new("precision sequence overflow"))?;
            if sequence != expected_sequence {
                return Ok(fresh_projection());
            }
            match (kind.as_str(), path) {
                ("path" | "object", Some(path)) if !excluded(&path, flavor) => {
                    paths.insert(path);
                }
                ("directory-prefix", Some(path)) if !excluded(&path, flavor) => match flavor {
                    ClientFlavor::Jj => projection.fresh_instance = true,
                    ClientFlavor::Git => {
                        paths.insert(directory_prefix(&path));
                    }
                },
                ("full-invalidation", None) => projection.fresh_instance = true,
                ("path" | "object" | "directory-prefix", Some(_)) => {}
                _ => return Ok(fresh_projection()),
            }
        }
        if expected_sequence != to_guard {
            return Ok(fresh_projection());
        }
        if projection.fresh_instance {
            Ok(fresh_projection())
        } else {
            Ok(Projection {
                fresh_instance: false,
                paths: paths.into_iter().collect(),
            })
        }
    }
}

fn fresh_projection() -> Projection {
    Projection {
        fresh_instance: true,
        paths: vec![b"/".to_vec()],
    }
}

fn decode_event_kind(value: &str) -> Result<EventKind, CompatError> {
    match value {
        "path-added" => Ok(EventKind::PathAdded),
        "path-removed" => Ok(EventKind::PathRemoved),
        "path-changed" => Ok(EventKind::PathChanged),
        "subtree-moved" => Ok(EventKind::SubtreeMoved),
        "directory-dirty-witness" => Ok(EventKind::DirectoryDirtyWitness),
        _ => Err(CompatError::new(format!("unknown event kind {value:?}"))),
    }
}

fn excluded(path: &[u8], flavor: ClientFlavor) -> bool {
    component_prefix(path, b".git")
        || (flavor == ClientFlavor::Jj && component_prefix(path, b".jj"))
}

fn component_prefix(path: &[u8], component: &[u8]) -> bool {
    path == component
        || path
            .strip_prefix(component)
            .is_some_and(|rest| rest.starts_with(b"/"))
}

fn directory_prefix(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b"/".to_vec();
    }
    let mut result = path.to_vec();
    if !result.ends_with(b"/") {
        result.push(b'/');
    }
    result
}

fn encode_claims(claims: &ClockClaims) -> Vec<u8> {
    let mut output = Vec::with_capacity(CLOCK_PAYLOAD_BYTES);
    output.extend_from_slice(&claims.format_version.to_be_bytes());
    output.extend_from_slice(&claims.store_uuid);
    output.extend_from_slice(&claims.watch_id);
    output.extend_from_slice(&claims.clock_epoch);
    output.extend_from_slice(&claims.cut_sequence.to_be_bytes());
    output.extend_from_slice(&claims.owner_grant_id);
    output.extend_from_slice(&claims.monitor_session_id);
    output.push(match claims.boundary_kind {
        BoundaryKind::Cut => 0,
        BoundaryKind::ProvedWorktreeSeed => 1,
    });
    output.extend_from_slice(&claims.algorithm_version.to_be_bytes());
    output.extend_from_slice(&claims.target_snapshot_uuid);
    debug_assert_eq!(output.len(), CLOCK_PAYLOAD_BYTES);
    output
}

fn decode_claims(payload: &[u8]) -> Result<ClockClaims, CompatError> {
    let mut offset = 0;
    let mut take = |length: usize| {
        let bytes = &payload[offset..offset + length];
        offset += length;
        bytes
    };
    let format_version = u32::from_be_bytes(take(4).try_into().expect("fixed field"));
    if format_version != 1 {
        return Err(CompatError::new("unsupported clock format version"));
    }
    let store_uuid = take(16).try_into().expect("fixed field");
    let watch_id = take(16).try_into().expect("fixed field");
    let clock_epoch = take(16).try_into().expect("fixed field");
    let cut_sequence = u64::from_be_bytes(take(8).try_into().expect("fixed field"));
    let owner_grant_id = take(16).try_into().expect("fixed field");
    let monitor_session_id = take(16).try_into().expect("fixed field");
    let boundary_kind = match take(1)[0] {
        0 => BoundaryKind::Cut,
        1 => BoundaryKind::ProvedWorktreeSeed,
        _ => return Err(CompatError::new("unknown clock boundary kind")),
    };
    let algorithm_version = u32::from_be_bytes(take(4).try_into().expect("fixed field"));
    let target_snapshot_uuid = take(16).try_into().expect("fixed field");
    Ok(ClockClaims {
        format_version,
        store_uuid,
        watch_id,
        clock_epoch,
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

    fn context(context: impl fmt::Display, error: impl fmt::Display) -> Self {
        Self::new(format!("{context}: {error}"))
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
    fn clock_round_trips_and_rejects_tampering() {
        let claims = ClockClaims {
            format_version: 1,
            store_uuid: [1; 16],
            watch_id: [2; 16],
            clock_epoch: [3; 16],
            cut_sequence: 42,
            owner_grant_id: [4; 16],
            monitor_session_id: [5; 16],
            boundary_kind: BoundaryKind::Cut,
            algorithm_version: 1,
            target_snapshot_uuid: [6; 16],
        };
        let key = [7; 32];
        let token = encode_clock(&claims, &key);
        assert_eq!(decode_clock(&token, &key).unwrap(), claims);
        assert!(token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b":._-".contains(&byte)));
        let mut tampered = token.into_bytes();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(decode_clock(std::str::from_utf8(&tampered).unwrap(), &key).is_err());
    }

    #[test]
    fn snapshot_only_directory_witness_is_fresh_for_jj_and_prefix_for_git() {
        let events = vec![
            event(EventKind::PathChanged, b"hardlink"),
            event(EventKind::DirectoryDirtyWitness, b"dir"),
            event(EventKind::PathChanged, b".git/index"),
        ];
        assert_eq!(
            project_events(&events, ClientFlavor::Jj),
            Projection {
                fresh_instance: true,
                paths: vec![b"/".to_vec()]
            }
        );
        assert_eq!(
            project_events(&events, ClientFlavor::Git),
            Projection {
                fresh_instance: false,
                paths: vec![b"dir/".to_vec(), b"hardlink".to_vec()]
            }
        );
    }

    #[test]
    fn jj_ignores_a_subtree_move_wholly_inside_its_metadata_directories() {
        let events = vec![event(EventKind::SubtreeMoved, b".jj/repo/op_store")];
        assert_eq!(
            project_events(&events, ClientFlavor::Jj),
            Projection {
                fresh_instance: false,
                paths: Vec::new(),
            }
        );
        assert_eq!(
            project_events(&events, ClientFlavor::Git),
            Projection {
                fresh_instance: false,
                paths: vec![b".jj/repo/op_store/".to_vec()],
            }
        );
    }
}
