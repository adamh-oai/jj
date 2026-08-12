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
const DIRECT_SCAN_CURSOR_PREFIX: &str = "c:btrfs-awacs:scan:1:";
const DIRECT_SCAN_CURSOR_DOMAIN: &[u8] = b"btrfs-awacs:scan:1\0";
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

/// Wraps an authenticated facade clock in a separately authenticated direct
/// scan cursor domain. The inner clock keeps the existing exact-boundary
/// claims; the outer MAC prevents a token from another protocol domain from
/// being accepted as a direct scan cursor.
pub(crate) fn encode_direct_scan_cursor(inner_clock: &str, key: &[u8; 32]) -> Vec<u8> {
    let payload = inner_clock.as_bytes();
    let mut mac_input = DIRECT_SCAN_CURSOR_DOMAIN.to_vec();
    mac_input.extend_from_slice(payload);
    let mac = hmac_sha256(key, &mac_input);
    let mut authenticated = payload.to_vec();
    authenticated.extend_from_slice(&mac);
    format!(
        "{DIRECT_SCAN_CURSOR_PREFIX}{}",
        base64url_encode(&authenticated)
    )
    .into_bytes()
}

/// Verifies and unwraps a direct scan cursor to the existing authenticated
/// facade clock representation.
pub(crate) fn decode_direct_scan_cursor(
    token: &[u8],
    key: &[u8; 32],
) -> Result<String, CompatError> {
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
    if authenticated.len() <= CLOCK_MAC_BYTES {
        return Err(CompatError::new(
            "direct scan cursor has an invalid payload length",
        ));
    }
    let (payload, supplied_mac) = authenticated.split_at(authenticated.len() - CLOCK_MAC_BYTES);
    let mut mac_input = DIRECT_SCAN_CURSOR_DOMAIN.to_vec();
    mac_input.extend_from_slice(payload);
    if !constant_time_eq(supplied_mac, &hmac_sha256(key, &mac_input)) {
        return Err(CompatError::new("direct scan cursor authentication failed"));
    }
    let inner_clock = std::str::from_utf8(payload)
        .map_err(|_| CompatError::new("direct scan cursor payload is not UTF-8"))?;
    if !inner_clock.starts_with(CLOCK_PREFIX) {
        return Err(CompatError::new(
            "direct scan cursor payload is not a facade clock",
        ));
    }
    Ok(inner_clock.to_owned())
}

/// Converts canonical semantic events into endpoint path changes exposed
/// through Watchman and the Git hook adapter.
///
/// FIXME: Dropping directory witnesses is unsafe when a client observed a
/// transient path after receiving its previous clock. Preserve those witnesses
/// through consumer-specific projection as described in FIXES.md.
pub fn project_events(events: &[Event]) -> Projection {
    let mut paths = BTreeSet::new();
    let mut fresh = false;
    for event in events {
        match event.kind {
            EventKind::DirectoryDirtyWitness => {
                // FIXME: This witness can identify a transient mutation still
                // present in a client's cached state; see FIXES.md.
            }
            EventKind::SubtreeMoved => {
                // A directory rename changes descendant paths without
                // emitting one endpoint event per descendant. Until the
                // query engine can expand those descendants, a generic
                // Watchman response must conservatively ask for a crawl.
                fresh = event.old_path.is_some() || event.new_path.is_some();
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
    ) -> Result<Projection, CompatError> {
        self.project_ready_cut_range_with_lease(watch_id, from_sequence, target_sequence, None)
    }

    pub fn project_ready_cut_range_with_lease(
        &self,
        watch_id: [u8; 16],
        from_sequence: Option<i64>,
        target_sequence: i64,
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
            return Ok(project_events(&events));
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
        let mut projection = project_events(&events);
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
                ("path" | "object", Some(path)) => {
                    paths.insert(path);
                }
                ("directory-prefix", Some(_)) => projection.fresh_instance = true,
                ("full-invalidation", None) => projection.fresh_instance = true,
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
    fn direct_scan_cursor_has_its_own_authenticated_domain() {
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
        let clock = encode_clock(&claims, &key);
        let cursor = encode_direct_scan_cursor(&clock, &key);
        let decoded = decode_direct_scan_cursor(&cursor, &key).unwrap();
        assert_eq!(decode_clock(&decoded, &key).unwrap(), claims);
        assert!(decode_direct_scan_cursor(clock.as_bytes(), &key).is_err());
        let mut tampered = cursor;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(decode_direct_scan_cursor(&tampered, &key).is_err());
    }

    #[test]
    fn directory_witness_does_not_change_endpoint_projection() {
        let events = vec![
            event(EventKind::PathChanged, b"hardlink"),
            event(EventKind::DirectoryDirtyWitness, b"dir"),
            event(EventKind::PathChanged, b".git/index"),
        ];
        assert_eq!(
            project_events(&events),
            Projection {
                fresh_instance: false,
                paths: vec![b".git/index".to_vec(), b"hardlink".to_vec()]
            }
        );
    }

    #[test]
    fn subtree_move_requires_a_generic_fresh_projection() {
        let events = vec![event(EventKind::SubtreeMoved, b".jj/repo/op_store")];
        assert_eq!(
            project_events(&events),
            Projection {
                fresh_instance: true,
                paths: vec![b"/".to_vec()],
            }
        );
    }
}
