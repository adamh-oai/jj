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

//! Content-addressed shared causal history, separate from active operation heads.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use tempfile::NamedTempFile;

use super::SyncError;
use super::SyncResult;
use super::shared::SharedEvent;
use super::shared::SharedEventId;
use super::shared::SharedView;
use crate::object_id::ObjectId as _;
use crate::op_store::OperationId;
use crate::simple_op_store;

const MAGIC: &[u8; 8] = b"JJSYNC\0\x01";
const ID_LENGTH: usize = 64;
const MAX_PARENTS: usize = 4096;
const MAX_EVENT_SIZE: usize = 64 * 1024 * 1024;

/// Immutable shared events stored under a repository's synchronization directory.
#[derive(Debug)]
pub struct AnchorStore {
    path: PathBuf,
}

impl AnchorStore {
    /// Directory containing the original immutable event files.
    #[cfg(unix)]
    pub(super) fn directory(&self) -> &Path {
        &self.path
    }

    /// Selects an existing store without creating missing directories.
    pub fn load(repo_path: &Path) -> Self {
        Self {
            path: repo_path.join("sync").join("events"),
        }
    }

    /// Opens or creates the shared-event directory.
    pub fn init(repo_path: &Path) -> SyncResult<Self> {
        let store = Self::load(repo_path);
        fs::create_dir_all(&store.path)?;
        Ok(store)
    }

    /// Reads and verifies an immutable event by its complete identity.
    pub fn read(&self, id: &SharedEventId) -> SyncResult<SharedEvent> {
        validate_id(id.as_bytes())?;
        let mut bytes = Vec::new();
        fs::File::open(self.path.join(id.hex()))?
            .take((MAX_EVENT_SIZE + 1) as u64)
            .read_to_end(&mut bytes)?;
        Self::decode(id, &bytes)
    }

    /// Writes an immutable event without changing any operation head.
    pub fn write(&self, event: &SharedEvent) -> SyncResult<SharedEventId> {
        let bytes = Self::encode(event)?;
        let id = event.id();
        let destination = self.path.join(id.hex());
        let mut temporary = NamedTempFile::new_in(&self.path)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if self.read(&id)? != *event {
                    return Err(invalid("existing shared event has different content"));
                }
            }
            Err(err) => return Err(err.error.into()),
        }
        #[cfg(unix)]
        fs::File::open(&self.path)?.sync_all()?;
        Ok(id)
    }

    /// Encodes an event for storage or transfer. Parent ordering is canonical.
    pub fn encode(event: &SharedEvent) -> SyncResult<Vec<u8>> {
        if event.parents.len() > MAX_PARENTS
            || !event.parents.windows(2).all(|pair| pair[0] < pair[1])
        {
            return Err(invalid("noncanonical shared event parents"));
        }
        let view = simple_op_store::encode_view(event.view.as_view());
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(event.parents.len() as u32).to_le_bytes());
        for parent in &event.parents {
            validate_id(parent.as_bytes())?;
            bytes.extend_from_slice(parent.as_bytes());
        }
        bytes.push(u8::from(event.origin.is_some()));
        if let Some(origin) = &event.origin {
            validate_id(origin.as_bytes())?;
            bytes.extend_from_slice(origin.as_bytes());
        }
        bytes.extend_from_slice(&(view.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&view);
        if bytes.len() > MAX_EVENT_SIZE {
            return Err(invalid("shared event exceeds size limit"));
        }
        Ok(bytes)
    }

    /// Decodes an incoming event, validating its identity and excluded fields.
    pub fn decode(id: &SharedEventId, bytes: &[u8]) -> SyncResult<SharedEvent> {
        validate_id(id.as_bytes())?;
        let EncodedEventFields {
            parents,
            origin,
            view,
        } = event_fields(bytes)?;
        let parents = parents
            .iter()
            .map(|parent| SharedEventId::from_bytes(parent))
            .collect();
        let origin = origin.map(OperationId::from_bytes);
        let view = simple_op_store::decode_view(view)
            .map_err(|err| invalid(format!("invalid shared view: {err}")))?;
        let shared = SharedView::from_view(&view);
        if shared.as_view() != &view {
            return Err(invalid(
                "shared event contains local workspace or Git state",
            ));
        }
        let event = SharedEvent {
            parents,
            view: shared,
            origin,
        };
        if event.id() != *id {
            return Err(invalid("shared event identity mismatch"));
        }
        Ok(event)
    }

    /// Borrows a structurally checked event's protobuf view for allocation limits.
    #[cfg(feature = "git")]
    pub(crate) fn encoded_view_bytes(bytes: &[u8]) -> SyncResult<&[u8]> {
        Ok(event_fields(bytes)?.view)
    }

    /// Whether `ancestor` is included in `descendant`'s causal history.
    pub fn is_ancestor(
        &self,
        ancestor: &SharedEventId,
        descendant: &SharedEventId,
    ) -> SyncResult<bool> {
        self.read(ancestor)?;
        self.read(descendant)?;
        let mut pending = vec![descendant.clone()];
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if &id == ancestor {
                return Ok(true);
            }
            if visited.insert(id.clone()) {
                pending.extend(self.read(&id)?.parents);
            }
        }
        Ok(false)
    }

    /// Removes causal ancestors and duplicates from an advertised frontier.
    pub fn heads(&self, ids: &[SharedEventId]) -> SyncResult<Vec<SharedEventId>> {
        for id in ids {
            self.read(id)?;
        }
        let mut heads = ids.iter().cloned().collect::<BTreeSet<_>>();
        for candidate in ids {
            for other in ids {
                if candidate != other && self.is_ancestor(candidate, other)? {
                    heads.remove(candidate);
                    break;
                }
            }
        }
        Ok(heads.into_iter().collect())
    }

    /// Returns a complete dependency closure in parent-before-child order.
    pub fn closure(
        &self,
        heads: &[SharedEventId],
    ) -> SyncResult<Vec<(SharedEventId, SharedEvent)>> {
        let mut pending = heads
            .iter()
            .rev()
            .map(|id| (id.clone(), false))
            .collect::<Vec<_>>();
        let mut completed = BTreeSet::new();
        let mut visiting = BTreeSet::new();
        let mut records = BTreeMap::new();
        let mut ordered = Vec::new();
        while let Some((id, expanded)) = pending.pop() {
            if completed.contains(&id) {
                continue;
            }
            if expanded {
                let event = records.remove(&id).unwrap();
                visiting.remove(&id);
                completed.insert(id.clone());
                ordered.push((id, event));
            } else {
                if !visiting.insert(id.clone()) {
                    return Err(invalid("cycle in shared event history"));
                }
                let event = self.read(&id)?;
                pending.push((id.clone(), true));
                pending.extend(event.parents.iter().rev().map(|id| (id.clone(), false)));
                records.insert(id, event);
            }
        }
        Ok(ordered)
    }
}

struct EncodedEventFields<'a> {
    parents: &'a [[u8; ID_LENGTH]],
    origin: Option<&'a [u8]>,
    view: &'a [u8],
}

fn event_fields(bytes: &[u8]) -> SyncResult<EncodedEventFields<'_>> {
    if bytes.len() > MAX_EVENT_SIZE || !bytes.starts_with(MAGIC) {
        return Err(invalid("unsupported or oversized shared event"));
    }
    let mut cursor = &bytes[MAGIC.len()..];
    let count = u32::from_le_bytes(take(&mut cursor, 4)?.try_into().unwrap()) as usize;
    if count > MAX_PARENTS {
        return Err(invalid("too many shared event parents"));
    }
    let parents = take(&mut cursor, count * ID_LENGTH)?
        .as_chunks::<ID_LENGTH>()
        .0;
    let mut previous = None;
    for parent in parents {
        if previous.is_some_and(|previous| previous >= parent) {
            return Err(invalid("noncanonical shared event parents"));
        }
        previous = Some(parent);
    }
    let origin = match take(&mut cursor, 1)?[0] {
        0 => None,
        1 => Some(take(&mut cursor, ID_LENGTH)?),
        _ => return Err(invalid("invalid shared event origin")),
    };
    let view_length = u32::from_le_bytes(take(&mut cursor, 4)?.try_into().unwrap()) as usize;
    if cursor.len() != view_length {
        return Err(invalid("truncated shared view or trailing event data"));
    }
    Ok(EncodedEventFields {
        parents,
        origin,
        view: cursor,
    })
}

fn validate_id(bytes: &[u8]) -> SyncResult<()> {
    if bytes.len() != ID_LENGTH {
        return Err(invalid("invalid synchronization object identity length"));
    }
    Ok(())
}

fn take<'a>(cursor: &mut &'a [u8], length: usize) -> SyncResult<&'a [u8]> {
    if cursor.len() < length {
        return Err(invalid("truncated shared event"));
    }
    let (value, rest) = cursor.split_at(length);
    *cursor = rest;
    Ok(value)
}

fn invalid(message: impl Into<String>) -> SyncError {
    SyncError::InvalidData(message.into())
}
