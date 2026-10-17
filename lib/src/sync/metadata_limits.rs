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

//! Allocation-free limits before decoding peer-supplied protobuf metadata.
//!
//! Even an empty repeated field allocates an element in prost. Count wire
//! fields, including known nested messages, before any such allocation. Opaque
//! bytes and strings are skipped as bytes, never interpreted as protobuf.
//! Unknown fields retain normal protobuf forward compatibility.

use super::SyncError;
use super::SyncResult;

const MAX_DEPTH: usize = 16;

/// Checks an original operation, including metadata and predecessor messages.
pub(super) fn check_operation(bytes: &[u8], max_fields: usize) -> SyncResult<()> {
    check(bytes, Schema::Operation, max_fields)
}

/// Checks an original or embedded shared view, including all legacy layouts.
pub(super) fn check_view(bytes: &[u8], max_fields: usize) -> SyncResult<()> {
    check(bytes, Schema::View, max_fields)
}

/// Checks a Git commit's exact jj metadata, independently of its object body.
pub(super) fn check_git_extras(bytes: &[u8], max_fields: usize) -> SyncResult<()> {
    check(bytes, Schema::GitExtras, max_fields)
}

#[derive(Clone, Copy)]
enum Schema {
    Operation,
    Metadata,
    Timestamp,
    Predecessors,
    View,
    Bookmark,
    RemoteBookmark,
    RefTarget,
    LegacyConflict,
    Conflict,
    Term,
    GitRef,
    GitHead,
    RemoteView,
    RemoteRef,
    Tag,
    MapEntry,
    GitExtras,
    Unknown,
}

impl Schema {
    /// Message-valued fields in simple_op_store.proto and git_store.proto.
    /// All other length-delimited fields are opaque strings, IDs, or unknowns.
    fn child(self, number: u32) -> Option<Self> {
        Some(match (self, number) {
            (Self::Operation, 3) => Self::Metadata,
            (Self::Operation, 4) => Self::Predecessors,
            (Self::Metadata, 1 | 2) => Self::Timestamp,
            (Self::Metadata, 6) => Self::MapEntry,
            (Self::View, 3) => Self::GitRef,
            (Self::View, 5) => Self::Bookmark,
            (Self::View, 6) => Self::Tag,
            (Self::View, 8) => Self::MapEntry,
            (Self::View, 9) => Self::RefTarget,
            (Self::View, 11) => Self::RemoteView,
            (Self::View, 13) => Self::GitHead,
            (Self::Bookmark, 2) => Self::RefTarget,
            (Self::Bookmark, 3) => Self::RemoteBookmark,
            (Self::RemoteBookmark, 2) => Self::RefTarget,
            (Self::RefTarget, 2) => Self::LegacyConflict,
            (Self::RefTarget, 3) => Self::Conflict,
            (Self::Conflict, 1 | 2) => Self::Term,
            (Self::GitRef, 3) => Self::RefTarget,
            (Self::GitHead | Self::Tag, 2) => Self::RefTarget,
            (Self::RemoteView, 2 | 3) => Self::RemoteRef,
            (Self::RemoteRef, 2) => Self::Term,
            _ => return None,
        })
    }
}

fn check(bytes: &[u8], schema: Schema, max_fields: usize) -> SyncResult<()> {
    let mut cursor = bytes;
    let mut remaining = max_fields;
    fields(&mut cursor, schema, None, 0, &mut remaining)
}

/// `group` is used only to skip unknown protobuf groups without allocation.
fn fields(
    cursor: &mut &[u8],
    schema: Schema,
    group: Option<u32>,
    depth: usize,
    remaining: &mut usize,
) -> SyncResult<()> {
    if depth > MAX_DEPTH {
        return Err(invalid(
            "protobuf metadata nesting exceeds the synchronization limit",
        ));
    }
    while !cursor.is_empty() {
        if *remaining == 0 {
            return Err(invalid(
                "protobuf metadata exceeds the local sync.max-dependencies budget; increase that \
                 setting",
            ));
        }
        *remaining -= 1;
        let key = varint(cursor)?;
        let number = key >> 3;
        if number == 0 || number > 0x1fff_ffff {
            return Err(invalid("invalid protobuf metadata field number"));
        }
        let number = number as u32;
        match key & 7 {
            0 => {
                varint(cursor)?;
            }
            1 => {
                take(cursor, 8)?;
            }
            2 => {
                let length = usize::try_from(varint(cursor)?)
                    .map_err(|_| invalid("protobuf metadata field length overflows"))?;
                let bytes = take(cursor, length)?;
                if let Some(child) = schema.child(number) {
                    let mut nested = bytes;
                    fields(&mut nested, child, None, depth + 1, remaining)?;
                }
            }
            3 => fields(cursor, Schema::Unknown, Some(number), depth + 1, remaining)?,
            4 => {
                return if group == Some(number) {
                    Ok(())
                } else {
                    Err(invalid("unexpected protobuf metadata end group"))
                };
            }
            5 => {
                take(cursor, 4)?;
            }
            _ => return Err(invalid("invalid protobuf metadata wire type")),
        }
    }
    if group.is_some() {
        return Err(invalid("unterminated protobuf metadata group"));
    }
    Ok(())
}

fn varint(cursor: &mut &[u8]) -> SyncResult<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = take(cursor, 1)?[0];
        if shift == 63 && byte > 1 {
            return Err(invalid("overflowing protobuf metadata varint"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(invalid("overflowing protobuf metadata varint"))
}

fn take<'a>(cursor: &mut &'a [u8], length: usize) -> SyncResult<&'a [u8]> {
    if cursor.len() < length {
        return Err(invalid("truncated protobuf metadata field"));
    }
    let (value, rest) = cursor.split_at(length);
    *cursor = rest;
    Ok(value)
}

fn invalid(message: impl Into<String>) -> SyncError {
    SyncError::InvalidData(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(number: u8, bytes: &[u8]) -> Vec<u8> {
        assert!(number < 16 && bytes.len() < 128);
        let mut result = vec![(number << 3) | 2, bytes.len() as u8];
        result.extend_from_slice(bytes);
        result
    }

    #[test]
    fn repeated_empty_fields_are_bounded_before_decoding() {
        let parents = [0x12, 0].repeat(100);
        assert!(check_operation(&parents, 99).is_err());
        check_operation(&parents, 100).unwrap();
        let heads = [0x0a, 0].repeat(100);
        assert!(check_view(&heads, 99).is_err());
        check_view(&heads, 100).unwrap();
        assert!(check_git_extras(&heads, 99).is_err());
        check_git_extras(&heads, 100).unwrap();
    }

    #[test]
    fn nested_and_legacy_messages_share_one_field_budget() {
        // View -> Bookmark -> RemoteBookmark -> RefTarget -> Conflict -> Term.
        let term = message(1, &[0xff, 0, 0xff]);
        let conflict = message(2, &term);
        let target = message(3, &conflict);
        let remote_bookmark = message(2, &target);
        let bookmark = message(3, &remote_bookmark);
        let view = message(5, &bookmark);
        check_view(&view, 6).unwrap();
        assert!(check_view(&view, 5).is_err());

        let legacy_conflict = [0x0a, 0].repeat(12);
        let view = message(9, &message(2, &legacy_conflict));
        check_view(&view, 14).unwrap();
        assert!(check_view(&view, 13).is_err());

        let attributes = message(1, b"key");
        let operation = message(3, &message(6, &attributes));
        check_operation(&operation, 3).unwrap();
        assert!(check_operation(&operation, 2).is_err());
    }

    #[test]
    fn opaque_fields_and_unknown_fields_are_not_parsed_as_messages() {
        let arbitrary = [0xff, 0, 0xff, 0, 0xff, 0];
        check_operation(&message(1, &arbitrary), 1).unwrap();
        check_operation(&message(3, &message(3, &arbitrary)), 2).unwrap();
        check_view(&message(15, &arbitrary), 1).unwrap();
        check_git_extras(&message(4, &arbitrary), 1).unwrap();
        // Unknown varint, fixed64, fixed32, and an old-style protobuf group.
        let fields = [
            0x78, 1, 0x79, 0, 0, 0, 0, 0, 0, 0, 0, 0x7d, 0, 0, 0, 0, 0x7b, 0x08, 0, 0x7c,
        ];
        check_view(&fields, 6).unwrap();
    }

    #[test]
    fn malformed_fields_and_deep_unknown_groups_are_rejected() {
        for bytes in [
            &[0][..],
            &[0xff; 10],
            &[0x0a, 0xff],
            &[0x0a, 2, 0],
            &[0x0e],
            &[0x0c],
            &[0x0b],
            &[0x0b, 0x14],
        ] {
            assert!(check_view(bytes, 100).is_err(), "accepted {bytes:?}");
        }
        let mut groups = vec![0x7b; MAX_DEPTH + 1];
        groups.extend(std::iter::repeat_n(0x7c, MAX_DEPTH + 1));
        assert!(check_view(&groups, 100).is_err());
        check_view(&[], 0).unwrap();
        assert!(check_view(&[0x08, 0], 0).is_err());
    }
}
