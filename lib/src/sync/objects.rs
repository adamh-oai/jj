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

//! Immutable dependency inventories and bounded, disk-backed import quarantine.
//!
//! No API in this module publishes operation heads or installs a live view.
//! Callers protect advertised source roots while exporting, and hold the
//! receiver's sync/GC lock while installing and pinning a validated closure.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::Write as _;
use std::path::Path;

use prost::Message as _;
use tempfile::NamedTempFile;
use tempfile::TempDir;

use super::SyncError;
use super::SyncResult;
use super::anchor::AnchorStore;
use super::normalize::INTEGRATION_ANCHOR_ATTRIBUTE;
use super::packed::GitPackReader;
use super::packed::GitPackRepository;
use super::shared::SharedEventId;
use crate::backend::Backend as _;
use crate::backend::CommitId;
use crate::backend::TreeId;
use crate::git_backend::GitBackend;
use crate::git_backend::GitCommitMetadata;
use crate::git_backend::GitObjectHeader;
use crate::git_backend::GitObjectRecord;
use crate::object_id::ObjectId as _;
use crate::op_store::OperationId;
use crate::op_store::ViewId;
use crate::simple_op_store::SimpleOpStore;
use crate::view::View;

/// Maximum combined payload and jj metadata size for one transfer record.
pub const MAX_RECORD_SIZE: usize = 64 * 1024 * 1024;
/// Maximum complete object-codec body, excluding transport framing.
pub const MAX_ENCODED_RECORD_SIZE: usize = MAX_RECORD_SIZE + 83;
/// Maximum body bytes in one streaming frame, independent of total object size.
pub const MAX_CHUNK_SIZE: usize = 1024 * 1024;
const FILE_OVERHEAD: u64 = 4096;

/// Receiver-controlled staging and validation budgets, independent of frame size.
#[derive(Clone, Debug)]
pub struct QuarantineLimits {
    /// Physical staging bytes, including metadata and per-file allocation overhead.
    /// Raw transfers stage uncompressed bodies; native transfers retain packs.
    pub max_staged_bytes: u64,
    /// Maximum staging reservations or distinct validated records.
    pub max_records: usize,
    /// Maximum dependency entries awaiting validation.
    pub max_dependencies: usize,
    /// Free space to preserve while staging and validating temporary bodies.
    pub min_free_bytes: u64,
}

impl QuarantineLimits {
    /// Derives a byte budget from local free space, allowing an installation copy.
    /// Callers may override these limits using trusted local configuration.
    pub fn for_directory(directory: &Path) -> SyncResult<Self> {
        let available = available_space(directory)?;
        let reserve = (1024 * 1024 * 1024).min(available / 10);
        Ok(Self {
            max_staged_bytes: (available - reserve) / 2,
            max_records: 1_000_000,
            max_dependencies: 1_000_000,
            min_free_bytes: reserve,
        })
    }

    pub(super) fn check_space(&self, directory: &Path, additional: u64) -> SyncResult<()> {
        if available_space(directory)?.saturating_sub(self.min_free_bytes) < additional {
            return Err(invalid(
                "insufficient free space for synchronization; free disk space or adjust the local \
                 sync.max-quarantine-bytes budget",
            ));
        }
        Ok(())
    }
}

#[cfg(unix)]
fn available_space(directory: &Path) -> std::io::Result<u64> {
    let space = rustix::fs::statvfs(directory)?;
    Ok(space.f_bavail.saturating_mul(space.f_frsize))
}

#[cfg(windows)]
fn available_space(directory: &Path) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt as _;
    let path = directory
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut available = 0;
    // SAFETY: The path is NUL-terminated and the output pointer is valid for the call.
    #[expect(unsafe_code)]
    let success = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if success == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

/// A complete typed identity, never interpreted as a caller-supplied path.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecordId {
    /// An original immutable operation.
    Operation(OperationId),
    /// An original immutable view.
    View(ViewId),
    /// A normalized shared causal event.
    SharedEvent(SharedEventId),
    /// A physical Git commit, tree, blob, or annotated tag.
    Git(gix::ObjectId),
}

impl RecordId {
    fn filename(&self) -> String {
        match self {
            Self::Operation(id) => format!("operation-{}", id.hex()),
            Self::View(id) => format!("view-{}", id.hex()),
            Self::SharedEvent(id) => format!("shared-{}", id.hex()),
            Self::Git(id) => format!("git-{id}"),
        }
    }
}

/// One bounded record. Exporters and importers retain at most one payload at a time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Record {
    /// The operation's exact stored bytes.
    Operation(OperationId, Vec<u8>),
    /// The view's exact stored bytes, before compatibility migrations.
    View(ViewId, Vec<u8>),
    /// An authenticated shared-event representation.
    SharedEvent(SharedEventId, Vec<u8>),
    /// Exact Git object bytes and the exact per-commit jj metadata entry.
    Git(GitObjectRecord),
    /// Exact jj commit interpretation; its physical body travels in a native pack.
    GitCommitMetadata(GitCommitMetadata),
    /// Announces an arbitrarily large physical object, followed by body chunks.
    GitHeader(GitObjectHeader),
    /// Contiguous body data for a previously announced physical object.
    GitChunk {
        /// The announced physical object.
        id: gix::ObjectId,
        /// Number of previously supplied body bytes.
        offset: u64,
        /// At most [`MAX_CHUNK_SIZE`] body bytes.
        data: Vec<u8>,
    },
    /// Completes a stream; staging verifies its complete hash before accepting it.
    GitEnd(gix::ObjectId),
}

impl Record {
    /// Returns the record's complete typed identity.
    pub fn id(&self) -> RecordId {
        match self {
            Self::Operation(id, _) => RecordId::Operation(id.clone()),
            Self::View(id, _) => RecordId::View(id.clone()),
            Self::SharedEvent(id, _) => RecordId::SharedEvent(id.clone()),
            Self::Git(record) => RecordId::Git(record.id),
            Self::GitCommitMetadata(metadata) => RecordId::Git(metadata.id),
            Self::GitHeader(header) => RecordId::Git(header.id),
            Self::GitChunk { id, .. } | Self::GitEnd(id) => RecordId::Git(*id),
        }
    }

    /// Encodes one record independently of any transport framing.
    pub fn encode(&self) -> SyncResult<Vec<u8>> {
        let mut bytes = Vec::new();
        self.write_to(&mut bytes)?;
        Ok(bytes)
    }

    /// Decodes a bounded body. Staging separately authenticates its object ID.
    pub fn decode(bytes: &[u8]) -> SyncResult<Self> {
        if bytes.len() < 3 || bytes.len() > MAX_ENCODED_RECORD_SIZE || bytes[0] != 1 {
            return Err(invalid("unsupported or oversized object record"));
        }
        let kind = bytes[1];
        let id_length = bytes[2] as usize;
        if !(match kind {
            1..=3 => id_length == 64,
            4..=11 => matches!(id_length, 20 | 32),
            _ => false,
        }) {
            return Err(invalid("invalid object record kind or identity length"));
        }
        let header_size = 3 + id_length + 16;
        if bytes.len() < header_size {
            return Err(invalid("truncated object record header"));
        }
        let id = &bytes[3..3 + id_length];
        let lengths = &bytes[3 + id_length..header_size];
        let data_length = u64::from_le_bytes(lengths[..8].try_into().unwrap());
        let extras_length = u64::from_le_bytes(lengths[8..].try_into().unwrap());
        let total = data_length.checked_add(if extras_length == u64::MAX {
            0
        } else {
            extras_length
        });
        if total != Some((bytes.len() - header_size) as u64)
            || total.unwrap() > MAX_RECORD_SIZE as u64
        {
            return Err(invalid(
                "truncated or oversized object payload, or trailing data",
            ));
        }
        let split = header_size + data_length as usize;
        let data = bytes[header_size..split].to_vec();
        let extras = (extras_length != u64::MAX).then(|| bytes[split..].to_vec());
        match kind {
            1 if extras.is_none() => Ok(Self::Operation(OperationId::from_bytes(id), data)),
            2 if extras.is_none() => Ok(Self::View(ViewId::from_bytes(id), data)),
            3 if extras.is_none() => Ok(Self::SharedEvent(SharedEventId::from_bytes(id), data)),
            kind @ 4..=7 => Ok(Self::Git(GitObjectRecord {
                id: git_id(id)?,
                kind: match kind {
                    4 => gix::objs::Kind::Commit,
                    5 => gix::objs::Kind::Tree,
                    6 => gix::objs::Kind::Blob,
                    _ => gix::objs::Kind::Tag,
                },
                data,
                commit_extras: extras,
            })),
            8 if data.len() == 9 => Ok(Self::GitHeader(GitObjectHeader {
                id: git_id(id)?,
                kind: git_kind(data[0])?,
                size: u64::from_le_bytes(data[1..].try_into().unwrap()),
                commit_extras: extras,
            })),
            9 if data.len() >= 8 && data.len() <= MAX_CHUNK_SIZE + 8 && extras.is_none() => {
                Ok(Self::GitChunk {
                    id: git_id(id)?,
                    offset: u64::from_le_bytes(data[..8].try_into().unwrap()),
                    data: data[8..].to_vec(),
                })
            }
            10 if data.is_empty() && extras.is_none() => Ok(Self::GitEnd(git_id(id)?)),
            11 if data.is_empty() && extras.is_some() => {
                Ok(Self::GitCommitMetadata(GitCommitMetadata {
                    id: git_id(id)?,
                    extras: extras.unwrap(),
                }))
            }
            _ => Err(invalid("only Git commits can carry jj metadata")),
        }
    }

    fn write_to(&self, writer: &mut impl std::io::Write) -> SyncResult<()> {
        self.check_size()?;
        match self {
            Self::GitHeader(header) => {
                let mut data = [0; 9];
                data[0] = git_kind_code(header.kind);
                data[1..].copy_from_slice(&header.size.to_le_bytes());
                return write_fields(
                    writer,
                    8,
                    header.id.as_bytes(),
                    &data,
                    header.commit_extras.as_deref(),
                );
            }
            Self::GitChunk { id, offset, data } => {
                let mut body = Vec::with_capacity(data.len() + 8);
                body.extend_from_slice(&offset.to_le_bytes());
                body.extend_from_slice(data);
                return write_fields(writer, 9, id.as_bytes(), &body, None);
            }
            Self::GitEnd(id) => return write_fields(writer, 10, id.as_bytes(), &[], None),
            _ => {}
        }
        let (kind, data, extras) = self.parts();
        let id = match self {
            Self::Operation(id, _) => id.as_bytes(),
            Self::View(id, _) => id.as_bytes(),
            Self::SharedEvent(id, _) => id.as_bytes(),
            Self::Git(record) => record.id.as_bytes(),
            Self::GitCommitMetadata(metadata) => metadata.id.as_bytes(),
            _ => unreachable!(),
        };
        if (kind <= 3 && id.len() != 64) || (kind > 3 && !matches!(id.len(), 20 | 32)) {
            return Err(invalid("invalid object record identity length"));
        }
        write_fields(writer, kind, id, data, extras)
    }

    fn parts(&self) -> (u8, &[u8], Option<&[u8]>) {
        match self {
            Self::Operation(_, bytes) => (1, bytes, None),
            Self::View(_, bytes) => (2, bytes, None),
            Self::SharedEvent(_, bytes) => (3, bytes, None),
            Self::GitCommitMetadata(metadata) => (11, &[], Some(&metadata.extras)),
            Self::Git(record) => (
                match record.kind {
                    gix::objs::Kind::Commit => 4,
                    gix::objs::Kind::Tree => 5,
                    gix::objs::Kind::Blob => 6,
                    gix::objs::Kind::Tag => 7,
                },
                &record.data,
                record.commit_extras.as_deref(),
            ),
            _ => unreachable!("streaming records have a separate bounded representation"),
        }
    }

    fn check_size(&self) -> SyncResult<()> {
        match self {
            Self::GitHeader(header) => {
                return if header.commit_extras.as_ref().map_or(0, Vec::len) > MAX_RECORD_SIZE - 9 {
                    Err(invalid(
                        "streaming object metadata exceeds the record limit",
                    ))
                } else {
                    Ok(())
                };
            }
            Self::GitChunk { data, .. } => {
                return if data.is_empty() || data.len() > MAX_CHUNK_SIZE {
                    Err(invalid("invalid streaming object chunk size"))
                } else {
                    Ok(())
                };
            }
            Self::GitEnd(_) => return Ok(()),
            _ => {}
        }
        let (_, data, extras) = self.parts();
        if data.len().saturating_add(extras.map_or(0, <[u8]>::len)) > MAX_RECORD_SIZE {
            return Err(invalid("transfer record exceeds the per-record size limit"));
        }
        Ok(())
    }
}

/// An inventory contains identities and retention roots, never payload buffers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Inventory {
    /// Original operation roots whose complete closure has been validated.
    pub raw_heads: BTreeSet<OperationId>,
    /// Shared causal roots whose complete closure has been validated.
    pub shared_heads: BTreeSet<SharedEventId>,
    /// Optional physical roots, for example annotated tags requested explicitly.
    /// Their retention outside raw/shared history belongs to the primitive's caller.
    pub git_roots: BTreeSet<gix::ObjectId>,
    /// Every non-synthetic record in the complete dependency closure.
    pub records: BTreeSet<RecordId>,
    /// Physical commits, including predecessors present only in jj extras.
    pub git_commits: BTreeSet<CommitId>,
    /// Legacy extras-only conflict trees requiring independent retention refs.
    pub legacy_tree_roots: BTreeSet<TreeId>,
}

impl Inventory {
    /// Omits known payloads only after validating their complete dependencies.
    ///
    /// Commit records are always requested: the physical Git ID alone does not
    /// authenticate the sender's jj metadata. Skipping those records would hide
    /// incompatible change identities behind an otherwise known commit hash.
    pub fn missing<'a>(
        &'a self,
        known: &'a BTreeSet<RecordId>,
    ) -> impl Iterator<Item = &'a RecordId> {
        self.records.iter().filter(|id| {
            let commit_metadata = match id {
                RecordId::Git(id) => self
                    .git_commits
                    .contains(&CommitId::from_bytes(id.as_bytes())),
                _ => false,
            };
            !known.contains(*id) || commit_metadata
        })
    }
}

/// Read-only immutable-object access for one repository.
pub struct RepositoryObjects<'a> {
    operations: &'a SimpleOpStore,
    anchors: &'a AnchorStore,
    backend: &'a GitBackend,
}

impl<'a> RepositoryObjects<'a> {
    /// Uses existing stores without creating directories or mutating roots.
    pub fn new(
        operations: &'a SimpleOpStore,
        anchors: &'a AnchorStore,
        backend: &'a GitBackend,
    ) -> Self {
        Self {
            operations,
            anchors,
            backend,
        }
    }

    /// Exports and authenticates one record; roots are virtual and have no record.
    pub fn export_record(&self, id: &RecordId) -> SyncResult<Record> {
        let record = match id {
            RecordId::Operation(id) => Record::Operation(
                id.clone(),
                self.operations
                    .export_operation_bounded(id, MAX_RECORD_SIZE)?,
            ),
            RecordId::View(id) => Record::View(
                id.clone(),
                self.operations.export_view_bounded(id, MAX_RECORD_SIZE)?,
            ),
            RecordId::SharedEvent(id) => {
                Record::SharedEvent(id.clone(), AnchorStore::encode(&self.anchors.read(id)?)?)
            }
            RecordId::Git(id) => {
                let header = self.backend.export_object_header(id)?;
                if header
                    .size
                    .saturating_add(header.commit_extras.as_ref().map_or(0, Vec::len) as u64)
                    > MAX_RECORD_SIZE as u64
                {
                    Record::GitHeader(header)
                } else {
                    Record::Git(self.backend.export_object(id)?)
                }
            }
        };
        self.validate_record(&record)?;
        Ok(record)
    }

    /// Emits one inline record or an ordered header/chunk/end stream.
    pub fn export_records(
        &self,
        id: &RecordId,
        mut emit: impl FnMut(Record) -> SyncResult<()>,
    ) -> SyncResult<()> {
        let record = self.export_record(id)?;
        if let Record::GitHeader(header) = record {
            emit(Record::GitHeader(header.clone()))?;
            let mut writer = ChunkWriter {
                id: header.id,
                offset: 0,
                buffer: Vec::with_capacity(MAX_CHUNK_SIZE),
                emit: &mut emit,
                failure: None,
            };
            let result = self.backend.export_object_stream(&header, &mut writer);
            writer.finish()?;
            result?;
            emit(Record::GitEnd(header.id))
        } else {
            emit(record)
        }
    }

    /// Emits jj records and mandatory per-commit sidecars for a native Git transfer.
    /// Knowing the Git object does not authenticate its sender's jj interpretation.
    pub fn export_metadata_records(
        &self,
        inventory: &Inventory,
        known: &BTreeSet<RecordId>,
        mut emit: impl FnMut(Record) -> SyncResult<()>,
    ) -> SyncResult<()> {
        for id in inventory.missing(known) {
            match id {
                RecordId::Git(id)
                    if inventory
                        .git_commits
                        .contains(&CommitId::from_bytes(id.as_bytes())) =>
                {
                    let record =
                        Record::GitCommitMetadata(self.backend.export_commit_metadata(id)?);
                    self.validate_record(&record)?;
                    emit(record)?;
                }
                RecordId::Git(_) => {}
                _ => emit(self.export_record(id)?)?,
            }
        }
        Ok(())
    }

    /// Walks Git dependencies through headers and native batch reads, never blob bodies.
    pub fn inventory_packed(
        &self,
        raw_heads: &[OperationId],
        shared_heads: &[SharedEventId],
    ) -> SyncResult<Inventory> {
        self.inventory_packed_with_git_roots(raw_heads, shared_heads, &[])
    }

    /// Includes explicit physical roots in a native packed dependency inventory.
    pub fn inventory_packed_with_git_roots(
        &self,
        raw_heads: &[OperationId],
        shared_heads: &[SharedEventId],
        git_roots: &[gix::ObjectId],
    ) -> SyncResult<Inventory> {
        let reader = RefCell::new(GitPackReader::new(
            self.backend.git_executable_path().as_os_str(),
            self.backend.git_repo_path(),
        )?);
        build_inventory(
            self,
            raw_heads,
            shared_heads,
            git_roots,
            None,
            InventoryReaders {
                read: |id: &RecordId| match id {
                    RecordId::Git(id) => Ok(Record::GitCommitMetadata(
                        self.backend.export_commit_metadata(id)?,
                    )),
                    _ => self.export_record(id),
                },
                stream: |_: &GitObjectHeader, _: &mut dyn std::io::Write| {
                    Err(invalid("packed inventory cannot stream raw Git records"))
                },
                packed: Some(&reader),
            },
        )
    }

    /// Validates all roots and dependencies, including records a peer already knows.
    pub fn inventory(
        &self,
        raw_heads: &[OperationId],
        shared_heads: &[SharedEventId],
    ) -> SyncResult<Inventory> {
        self.inventory_with_git_roots(raw_heads, shared_heads, &[])
    }

    /// Includes explicit physical roots such as annotated tags and their targets.
    pub fn inventory_with_git_roots(
        &self,
        raw_heads: &[OperationId],
        shared_heads: &[SharedEventId],
        git_roots: &[gix::ObjectId],
    ) -> SyncResult<Inventory> {
        build_inventory(
            self,
            raw_heads,
            shared_heads,
            git_roots,
            None,
            InventoryReaders {
                read: |id: &RecordId| self.export_record(id),
                stream: |header: &GitObjectHeader, to: &mut dyn std::io::Write| {
                    self.backend
                        .export_object_stream(header, to)
                        .map_err(Into::into)
                },
                packed: None,
            },
        )
    }

    /// Checks a record and any existing receiver metadata without installing it.
    pub fn validate_record(&self, record: &Record) -> SyncResult<()> {
        record.check_size()?;
        match record {
            Record::Operation(id, bytes) => {
                self.operations.decode_raw_operation(id, bytes)?;
            }
            Record::View(id, bytes) => {
                self.operations.decode_raw_view(id, bytes)?;
            }
            Record::SharedEvent(id, bytes) => {
                AnchorStore::decode(id, bytes)?;
            }
            Record::Git(record) => self.backend.validate_object_import(record)?,
            Record::GitCommitMetadata(metadata) => {
                self.backend.validate_commit_metadata_encoding(metadata)?;
            }
            Record::GitHeader(header) => {
                if header.id.kind() != self.backend.git_repo().object_hash()
                    || (header.kind == gix::objs::Kind::Commit) != header.commit_extras.is_some()
                {
                    return Err(invalid("invalid streaming object header"));
                }
            }
            Record::GitChunk { id, .. } | Record::GitEnd(id) => {
                if id.kind() != self.backend.git_repo().object_hash() {
                    return Err(invalid("streamed object uses the wrong Git format"));
                }
            }
        }
        Ok(())
    }
}

/// A disposable on-disk batch; dropping it removes all quarantined payloads.
pub struct Quarantine {
    directory: TempDir,
    root_commit_id: CommitId,
    staged: BTreeSet<RecordId>,
    streams: BTreeMap<gix::ObjectId, PendingStream>,
    limits: QuarantineLimits,
    reserved_bytes: u64,
    reserved_records: usize,
    packed: Option<GitPackRepository>,
}

struct PendingStream {
    header: GitObjectHeader,
    body: NamedTempFile,
    received: u64,
}

impl Quarantine {
    /// Creates a private staging directory, outside all active repository stores.
    pub fn new(parent: &Path, root_commit_id: CommitId) -> SyncResult<Self> {
        Self::with_limits(
            parent,
            root_commit_id,
            QuarantineLimits::for_directory(parent)?,
        )
    }

    /// Creates staging with budgets selected by the receiver, never by the peer.
    pub fn with_limits(
        parent: &Path,
        root_commit_id: CommitId,
        limits: QuarantineLimits,
    ) -> SyncResult<Self> {
        Ok(Self {
            directory: tempfile::Builder::new()
                .prefix("jj-sync-quarantine-")
                .tempdir_in(parent)?,
            root_commit_id,
            staged: BTreeSet::new(),
            streams: BTreeMap::new(),
            limits,
            reserved_bytes: 0,
            reserved_records: 0,
            packed: None,
        })
    }

    /// Attaches the disposable Git receiver after native pack transfer.
    pub fn set_git_pack_repository(&mut self, repository: GitPackRepository) -> SyncResult<()> {
        if self.packed.is_some()
            || !self.streams.is_empty()
            || self.staged.iter().any(|id| matches!(id, RecordId::Git(_)))
        {
            return Err(invalid(
                "packed Git receiver must be attached before staging Git metadata",
            ));
        }
        if repository.git_repo().object_hash().len_in_bytes()
            != self.root_commit_id.as_bytes().len()
        {
            return Err(invalid(
                "packed Git receiver uses a different object format",
            ));
        }
        self.packed = Some(repository);
        Ok(())
    }

    /// Byte allowance left for native packs after immutable metadata staging.
    pub fn remaining_packed_bytes(&self) -> u64 {
        self.limits
            .max_staged_bytes
            .saturating_sub(self.reserved_bytes)
    }

    /// Validates one bounded record against the receiver before saving it to disk.
    pub fn stage(&mut self, receiver: &RepositoryObjects<'_>, record: &Record) -> SyncResult<()> {
        self.check_receiver(receiver)?;
        record.check_size()?;
        match (self.packed.is_some(), record) {
            (false, Record::GitCommitMetadata(_)) => {
                return Err(invalid("packed commit metadata was not negotiated"));
            }
            (
                true,
                Record::Git(_) | Record::GitHeader(_) | Record::GitChunk { .. } | Record::GitEnd(_),
            ) => {
                return Err(invalid(
                    "raw Git body received during native packed transfer",
                ));
            }
            _ => {}
        }
        match record {
            Record::GitHeader(header) => {
                if self.streams.contains_key(&header.id) || self.staged.contains(&record.id()) {
                    return Err(invalid("streaming object identity was announced twice"));
                }
                if !self.streams.is_empty() {
                    return Err(invalid("only one Git body stream may be open at a time"));
                }
                let cost = record_size(record)?
                    .checked_add(header.size)
                    .and_then(|size| size.checked_add(FILE_OVERHEAD * 2))
                    .ok_or_else(|| {
                        invalid("streamed object size overflows the quarantine budget")
                    })?;
                self.reserve(cost)?;
                preflight_metadata(record, self.limits.max_dependencies)?;
                receiver.validate_record(record)?;
                let stream = PendingStream {
                    header: header.clone(),
                    body: NamedTempFile::new_in(self.directory.path())?,
                    received: 0,
                };
                self.streams.insert(header.id, stream);
                return Ok(());
            }
            Record::GitChunk { id, offset, data } => {
                receiver.validate_record(record)?;
                let stream = self
                    .streams
                    .get_mut(id)
                    .ok_or_else(|| invalid("object chunk has no announced header"))?;
                if *offset != stream.received
                    || (*offset)
                        .checked_add(data.len() as u64)
                        .is_none_or(|end| end > stream.header.size)
                {
                    return Err(invalid(
                        "object chunk offset or length does not match its header",
                    ));
                }
                self.limits
                    .check_space(self.directory.path(), data.len() as u64)?;
                stream.body.write_all(data)?;
                stream.received += data.len() as u64;
                return Ok(());
            }
            Record::GitEnd(id) => {
                receiver.validate_record(record)?;
                let mut stream = self
                    .streams
                    .remove(id)
                    .ok_or_else(|| invalid("object completion has no announced header"))?;
                if stream.received != stream.header.size {
                    return Err(invalid("streamed object body is incomplete"));
                }
                stream.body.as_file_mut().rewind()?;
                receiver
                    .backend
                    .validate_object_stream(&stream.header, stream.body.as_file_mut())?;
                stream.body.as_file().sync_all()?;
                stream
                    .body
                    .persist_noclobber(self.body_path(id))
                    .map_err(|err| SyncError::Io(err.error))?;
                return self.save_record(&Record::GitHeader(stream.header));
            }
            _ => {}
        }
        if let RecordId::Git(id) = record.id()
            && self.streams.contains_key(&id)
        {
            return Err(invalid(
                "inline object conflicts with an open Git body stream",
            ));
        }
        if !self.staged.contains(&record.id()) {
            self.reserve(record_size(record)?.saturating_add(FILE_OVERHEAD))?;
        }
        preflight_metadata(record, self.limits.max_dependencies)?;
        receiver.validate_record(record)?;
        self.save_record(record)
    }

    fn reserve(&mut self, bytes: u64) -> SyncResult<()> {
        let reserved = self
            .reserved_bytes
            .checked_add(bytes)
            .filter(|value| *value <= self.limits.max_staged_bytes)
            .ok_or_else(|| {
                invalid(
                    "synchronization exceeds the local sync.max-quarantine-bytes budget; increase \
                     that setting or transfer less history",
                )
            })?;
        if self.reserved_records >= self.limits.max_records {
            return Err(invalid(
                "synchronization exceeds the local sync.max-records budget; increase that setting",
            ));
        }
        self.limits.check_space(self.directory.path(), bytes)?;
        self.reserved_bytes = reserved;
        self.reserved_records += 1;
        Ok(())
    }

    fn save_record(&mut self, record: &Record) -> SyncResult<()> {
        let id = record.id();
        if self.staged.contains(&id) {
            if matches!(record, Record::Git(_) | Record::GitCommitMetadata(_))
                && self.read_record(&id)? != *record
            {
                return Err(invalid(
                    "different Git metadata supplied twice for one staged identity",
                ));
            }
            return Ok(());
        }
        self.limits.check_space(
            self.directory.path(),
            record_size(record)?.saturating_add(FILE_OVERHEAD),
        )?;
        let mut file = NamedTempFile::new_in(self.directory.path())?;
        record.write_to(&mut file)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(self.directory.path().join(id.filename()))
            .map_err(|err| SyncError::Io(err.error))?;
        self.staged.insert(id);
        Ok(())
    }

    /// Reads one staged record, bounding allocation before decoding any payload.
    pub fn read_record(&self, id: &RecordId) -> SyncResult<Record> {
        let mut bytes = Vec::new();
        fs::File::open(self.directory.path().join(id.filename()))?
            .take((MAX_ENCODED_RECORD_SIZE + 1) as u64)
            .read_to_end(&mut bytes)?;
        let record = Record::decode(&bytes)?;
        if record.id() != *id {
            return Err(invalid("quarantined record has the wrong identity"));
        }
        Ok(record)
    }

    /// Validates the staged/receiver union, including dependencies omitted as known.
    pub fn validate(
        &self,
        receiver: &RepositoryObjects<'_>,
        raw_heads: &[OperationId],
        shared_heads: &[SharedEventId],
    ) -> SyncResult<Inventory> {
        self.validate_with_git_roots(receiver, raw_heads, shared_heads, &[])
    }

    /// Validates explicit physical roots in the same staged/receiver closure.
    pub fn validate_with_git_roots(
        &self,
        receiver: &RepositoryObjects<'_>,
        raw_heads: &[OperationId],
        shared_heads: &[SharedEventId],
        git_roots: &[gix::ObjectId],
    ) -> SyncResult<Inventory> {
        self.check_receiver(receiver)?;
        if !self.streams.is_empty() {
            return Err(invalid("object transfer has unfinished body streams"));
        }
        let packed_reader = self
            .packed
            .as_ref()
            .map(|repository| repository.read_objects())
            .transpose()?
            .map(RefCell::new);
        let inventory = build_inventory(
            receiver,
            raw_heads,
            shared_heads,
            git_roots,
            Some((&self.limits, self.directory.path())),
            InventoryReaders {
                read: |id: &RecordId| {
                    if self.staged.contains(id) {
                        self.read_record(id)
                    } else if self.packed.is_some() && matches!(id, RecordId::Git(_)) {
                        Err(invalid(
                            "native Git transfer omitted mandatory commit metadata",
                        ))
                    } else {
                        receiver.export_record(id)
                    }
                },
                stream: |header: &GitObjectHeader, to: &mut dyn std::io::Write| {
                    if self.staged.contains(&RecordId::Git(header.id)) {
                        std::io::copy(&mut fs::File::open(self.body_path(&header.id))?, to)?;
                        Ok(())
                    } else {
                        receiver
                            .backend
                            .export_object_stream(header, to)
                            .map_err(Into::into)
                    }
                },
                packed: packed_reader.as_ref(),
            },
        )?;
        if let Some(packed) = &self.packed {
            let mut limits = self.limits.clone();
            limits.max_staged_bytes = limits
                .max_staged_bytes
                .checked_sub(self.reserved_bytes)
                .ok_or_else(|| invalid("metadata exceeds native Git quarantine budget"))?;
            packed.validate_received(receiver.backend, &inventory, &limits)?;
        }
        Ok(inventory)
    }

    /// Installs a complete validated closure and its retention refs, without activation.
    /// The caller holds the receiver sync/GC lock through this call and root publication.
    pub fn install(
        &self,
        receiver: &RepositoryObjects<'_>,
        inventory: &Inventory,
    ) -> SyncResult<()> {
        let verified = self.validate_with_git_roots(
            receiver,
            &inventory.raw_heads.iter().cloned().collect::<Vec<_>>(),
            &inventory.shared_heads.iter().cloned().collect::<Vec<_>>(),
            &inventory.git_roots.iter().copied().collect::<Vec<_>>(),
        )?;
        if verified != *inventory {
            return Err(invalid("inventory changed before immutable installation"));
        }
        let _packed_promotion = self
            .packed
            .as_ref()
            .map(|packed| packed.promote(receiver.backend))
            .transpose()?;
        if let Some(packed) = &self.packed {
            let reader = RefCell::new(packed.read_objects()?);
            receiver.backend.import_commit_metadata(
                inventory
                    .git_commits
                    .iter()
                    .map(|id| gix::ObjectId::from_bytes_or_panic(id.as_bytes())),
                |id| match self
                    .read_record(&RecordId::Git(id.to_owned()))
                    .map_err(|err| crate::backend::BackendError::Other(err.into()))?
                {
                    Record::GitCommitMetadata(metadata) => Ok(metadata.extras),
                    _ => Err(crate::backend::BackendError::Other(
                        "packed commit sidecar is missing".into(),
                    )),
                },
                |id, to| {
                    reader
                        .borrow_mut()
                        .with_object(id, |kind, _, from| {
                            if kind != gix::objs::Kind::Commit {
                                return Err(invalid(
                                    "commit metadata points to a non-commit object",
                                ));
                            }
                            std::io::copy(from, to)?;
                            Ok(())
                        })
                        .map_err(|err| crate::backend::BackendError::Other(err.into()))
                },
            )?;
        }
        for id in inventory.records.intersection(&self.staged) {
            match self.read_record(id)? {
                Record::Operation(id, bytes) => {
                    receiver.operations.import_operation(&id, &bytes)?;
                }
                Record::View(id, bytes) => {
                    receiver.operations.import_view(&id, &bytes)?;
                }
                Record::SharedEvent(id, bytes) => {
                    receiver.anchors.write(&AnchorStore::decode(&id, &bytes)?)?;
                }
                Record::Git(record) => receiver.backend.import_object(&record)?,
                Record::GitHeader(header) => receiver.backend.import_object_stream(
                    &header,
                    &mut fs::File::open(self.body_path(&header.id))?,
                )?,
                Record::GitCommitMetadata(_) => {}
                Record::GitChunk { .. } | Record::GitEnd(_) => {
                    return Err(invalid(
                        "unfinished streaming record in validated inventory",
                    ));
                }
            }
        }
        receiver
            .backend
            .import_head_commits(inventory.git_commits.iter())?;
        receiver
            .backend
            .pin_sync_trees(inventory.legacy_tree_roots.iter())?;
        Ok(())
    }

    fn check_receiver(&self, receiver: &RepositoryObjects<'_>) -> SyncResult<()> {
        if &self.root_commit_id != receiver.backend.root_commit_id() {
            return Err(invalid(
                "quarantine and receiver use different Git object formats",
            ));
        }
        Ok(())
    }

    fn body_path(&self, id: &gix::ObjectId) -> std::path::PathBuf {
        self.directory.path().join(format!("body-{id}"))
    }
}

struct InventoryReaders<'a, R, S> {
    read: R,
    stream: S,
    packed: Option<&'a RefCell<GitPackReader>>,
}

fn build_inventory(
    receiver: &RepositoryObjects<'_>,
    raw_heads: &[OperationId],
    shared_heads: &[SharedEventId],
    git_roots: &[gix::ObjectId],
    limits: Option<(&QuarantineLimits, &Path)>,
    readers: InventoryReaders<
        '_,
        impl Fn(&RecordId) -> SyncResult<Record>,
        impl Fn(&GitObjectHeader, &mut dyn std::io::Write) -> SyncResult<()>,
    >,
) -> SyncResult<Inventory> {
    let InventoryReaders {
        read,
        stream,
        packed,
    } = readers;
    if limits.is_some_and(|(limits, _)| {
        raw_heads
            .len()
            .saturating_add(shared_heads.len())
            .saturating_add(git_roots.len())
            > limits.max_dependencies
    }) {
        return Err(invalid(
            "root frontier exceeds the local sync.max-dependencies budget; increase that setting",
        ));
    }
    let mut inventory = Inventory {
        raw_heads: raw_heads.iter().cloned().collect(),
        shared_heads: shared_heads.iter().cloned().collect(),
        git_roots: git_roots.iter().copied().collect(),
        ..Inventory::default()
    };
    let mut pending = PendingObjects::new(limits.map(|(limits, _)| limits.max_dependencies));
    pending.extend(
        raw_heads
            .iter()
            .cloned()
            .map(|id| (RecordId::Operation(id), None))
            .chain(
                shared_heads
                    .iter()
                    .cloned()
                    .map(|id| (RecordId::SharedEvent(id), None)),
            )
            .chain(
                git_roots
                    .iter()
                    .copied()
                    .map(|id| (RecordId::Git(id), None)),
            ),
    )?;
    let mut git_kinds = BTreeMap::new();
    while let Some((id, expected)) = pending.pop() {
        match &id {
            RecordId::Operation(id) if id.as_bytes() == [0; 64] => continue,
            RecordId::View(id) if id.as_bytes() == [0; 64] => continue,
            RecordId::Git(id) if id.as_bytes() == receiver.backend.root_commit_id().as_bytes() => {
                if expected != Some(gix::objs::Kind::Commit) {
                    return Err(invalid(
                        "virtual root referenced as a non-commit Git object",
                    ));
                }
                continue;
            }
            _ => {}
        }
        if inventory.records.contains(&id) {
            if let (RecordId::Git(id), Some(expected)) = (&id, expected)
                && git_kinds.get(id) != Some(&expected)
            {
                return Err(invalid("Git dependency has the wrong object type"));
            }
            continue;
        }
        if limits.is_some_and(|(limits, _)| inventory.records.len() >= limits.max_records) {
            return Err(invalid(
                "dependency closure exceeds the local sync.max-records budget; increase that \
                 setting",
            ));
        }
        if let (RecordId::Git(git_id), Some(reader)) = (&id, packed) {
            let (kind, size) = reader.borrow().header(git_id)?;
            if expected.is_some_and(|expected| expected != kind) {
                return Err(invalid("Git dependency has the wrong object type"));
            }
            git_kinds.insert(*git_id, kind);
            // Native Git has already verified physical hashes. Blob bodies
            // have no dependencies and never pass through Rust staging.
            if kind != gix::objs::Kind::Blob {
                let metadata = if kind == gix::objs::Kind::Commit {
                    let record = read(&id)?;
                    if record.id() != id {
                        return Err(invalid("commit sidecar has the wrong identity"));
                    }
                    if let Some((limits, _)) = limits {
                        preflight_metadata(&record, limits.max_dependencies)?;
                    }
                    receiver.validate_record(&record)?;
                    let Record::GitCommitMetadata(metadata) = record else {
                        return Err(invalid(
                            "native Git transfer requires commit metadata sidecars",
                        ));
                    };
                    Some(metadata)
                } else {
                    None
                };
                let header = GitObjectHeader {
                    id: *git_id,
                    kind,
                    size,
                    commit_extras: metadata.as_ref().map(|metadata| metadata.extras.clone()),
                };
                reader
                    .borrow_mut()
                    .with_object(git_id, |actual_kind, actual_size, from| {
                        if actual_kind != kind || actual_size != size {
                            return Err(invalid("packed object changed during validation"));
                        }
                        if let Some(metadata) = &metadata {
                            receiver
                                .backend
                                .inspect_packed_commit(metadata, from, |from| {
                                    streamed_dependencies(
                                        &header,
                                        from,
                                        &mut pending,
                                        &mut inventory,
                                    )
                                })
                        } else {
                            streamed_dependencies(&header, from, &mut pending, &mut inventory)
                        }
                    })?;
            }
            inventory.records.insert(id);
            continue;
        }
        let record = read(&id)?;
        if record.id() != id {
            return Err(invalid("object source returned the wrong record identity"));
        }
        if let Some((limits, _)) = limits {
            preflight_metadata(&record, limits.max_dependencies)?;
        }
        receiver.validate_record(&record)?;
        match record {
            Record::Operation(id, bytes) => {
                let op = receiver.operations.decode_raw_operation(&id, &bytes)?;
                pending.extend(
                    op.parents
                        .into_iter()
                        .map(|id| (RecordId::Operation(id), None)),
                )?;
                pending.push((RecordId::View(op.view_id), None))?;
                if let Some(map) = op.commit_predecessors {
                    for (id, predecessors) in map {
                        push_commit(&mut pending, &id)?;
                        for id in predecessors {
                            push_commit(&mut pending, &id)?;
                        }
                    }
                }
                if let Some(anchor) = op.metadata.attributes.get(INTEGRATION_ANCHOR_ATTRIBUTE) {
                    let id = SharedEventId::try_from_hex(anchor)
                        .filter(|id| id.as_bytes().len() == 64)
                        .ok_or_else(|| {
                            invalid("invalid integration anchor in original operation")
                        })?;
                    pending.push((RecordId::SharedEvent(id), None))?;
                }
            }
            Record::View(id, bytes) => {
                let view = receiver.operations.decode_raw_view(&id, &bytes)?;
                push_view_commits(&mut pending, view)?;
            }
            Record::SharedEvent(id, bytes) => {
                let event = AnchorStore::decode(&id, &bytes)?;
                pending.extend(
                    event
                        .parents
                        .into_iter()
                        .map(|id| (RecordId::SharedEvent(id), None)),
                )?;
                pending.extend(
                    event
                        .origin
                        .into_iter()
                        .map(|id| (RecordId::Operation(id), None)),
                )?;
                push_view_commits(&mut pending, event.view.as_view().clone())?;
            }
            Record::Git(record) => {
                if expected.is_some_and(|kind| kind != record.kind) {
                    return Err(invalid("Git dependency has the wrong object type"));
                }
                git_kinds.insert(record.id, record.kind);
                let object =
                    gix::objs::ObjectRef::from_bytes(&record.data, record.kind, record.id.kind())
                        .map_err(|err| invalid(format!("invalid Git object: {err}")))?;
                match object {
                    gix::objs::ObjectRef::Commit(commit) => {
                        inventory
                            .git_commits
                            .insert(CommitId::from_bytes(record.id.as_bytes()));
                        pending
                            .push((RecordId::Git(commit.tree()), Some(gix::objs::Kind::Tree)))?;
                        pending.extend(
                            commit
                                .parents()
                                .map(|id| (RecordId::Git(id), Some(gix::objs::Kind::Commit))),
                        )?;
                        let extras = crate::protos::git_store::Commit::decode(
                            record
                                .commit_extras
                                .as_deref()
                                .ok_or_else(|| invalid("commit extras are missing"))?,
                        )
                        .map_err(|err| invalid(format!("invalid commit extras: {err}")))?;
                        for id in extras.predecessors {
                            push_commit(&mut pending, &CommitId::new(id))?;
                        }
                        if extras.uses_tree_conflict_format {
                            for bytes in extras.root_tree {
                                let id = git_id(&bytes)?;
                                inventory.legacy_tree_roots.insert(TreeId::new(bytes));
                                pending.push((RecordId::Git(id), Some(gix::objs::Kind::Tree)))?;
                            }
                        }
                    }
                    gix::objs::ObjectRef::Tree(tree) => {
                        for entry in tree.entries {
                            let kind = match entry.mode.kind() {
                                gix::objs::tree::EntryKind::Commit => continue,
                                gix::objs::tree::EntryKind::Tree => gix::objs::Kind::Tree,
                                _ => gix::objs::Kind::Blob,
                            };
                            pending.push((RecordId::Git(entry.oid.to_owned()), Some(kind)))?;
                        }
                    }
                    gix::objs::ObjectRef::Tag(tag) => {
                        pending.push((RecordId::Git(tag.target()), Some(tag.target_kind)))?;
                    }
                    gix::objs::ObjectRef::Blob(_) => {}
                }
            }
            Record::GitHeader(header) => {
                if expected.is_some_and(|kind| kind != header.kind) {
                    return Err(invalid("Git dependency has the wrong object type"));
                }
                git_kinds.insert(header.id, header.kind);
                let mut body = if let Some((limits, directory)) = limits {
                    let cost = header
                        .size
                        .checked_add(FILE_OVERHEAD)
                        .filter(|cost| *cost <= limits.max_staged_bytes)
                        .ok_or_else(|| {
                            invalid(
                                "validation temporary body exceeds sync.max-quarantine-bytes; \
                                 increase that local setting",
                            )
                        })?;
                    limits.check_space(directory, cost)?;
                    NamedTempFile::new_in(directory)?
                } else {
                    NamedTempFile::new()?
                };
                stream(
                    &header,
                    &mut ValidationWriter {
                        to: body.as_file_mut(),
                        remaining: header.size,
                        limits,
                    },
                )?;
                if body.as_file().metadata()?.len() != header.size {
                    return Err(invalid("streamed object length does not match its header"));
                }
                body.as_file_mut().rewind()?;
                receiver
                    .backend
                    .validate_object_stream(&header, body.as_file_mut())?;
                body.as_file_mut().rewind()?;
                streamed_dependencies(&header, body.as_file_mut(), &mut pending, &mut inventory)?;
            }
            Record::GitCommitMetadata(_) => {
                return Err(invalid(
                    "commit metadata sidecar has no native Git object source",
                ));
            }
            Record::GitChunk { .. } | Record::GitEnd(_) => {
                return Err(invalid(
                    "object source returned a body fragment instead of a complete record",
                ));
            }
        }
        inventory.records.insert(id);
    }
    Ok(inventory)
}

struct ValidationWriter<'a> {
    to: &'a mut dyn std::io::Write,
    remaining: u64,
    limits: Option<(&'a QuarantineLimits, &'a Path)>,
}

impl std::io::Write for ValidationWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other(
                "object body exceeds its declared validation size",
            ));
        }
        if let Some((limits, directory)) = self.limits {
            limits
                .check_space(directory, bytes.len() as u64)
                .map_err(std::io::Error::other)?;
        }
        let written = self.to.write(bytes)?;
        self.remaining -= written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.to.flush()
    }
}

struct PendingObjects {
    entries: Vec<(RecordId, Option<gix::objs::Kind>)>,
    limit: Option<usize>,
}

impl PendingObjects {
    fn new(limit: Option<usize>) -> Self {
        Self {
            entries: Vec::new(),
            limit,
        }
    }

    fn push(&mut self, entry: (RecordId, Option<gix::objs::Kind>)) -> SyncResult<()> {
        if self.limit.is_some_and(|limit| self.entries.len() >= limit) {
            return Err(invalid(
                "dependency queue exceeds the local sync.max-dependencies budget; increase that \
                 setting",
            ));
        }
        self.entries.push(entry);
        Ok(())
    }

    fn extend(
        &mut self,
        entries: impl IntoIterator<Item = (RecordId, Option<gix::objs::Kind>)>,
    ) -> SyncResult<()> {
        for entry in entries {
            self.push(entry)?;
        }
        Ok(())
    }

    fn pop(&mut self) -> Option<(RecordId, Option<gix::objs::Kind>)> {
        self.entries.pop()
    }
}

fn push_commit(pending: &mut PendingObjects, id: &CommitId) -> SyncResult<()> {
    pending.push((
        RecordId::Git(git_id(id.as_bytes())?),
        Some(gix::objs::Kind::Commit),
    ))
}

fn push_view_commits(pending: &mut PendingObjects, view: crate::op_store::View) -> SyncResult<()> {
    for id in View::new(view, true).all_referenced_commit_ids() {
        push_commit(pending, id)?;
    }
    Ok(())
}

fn git_id(bytes: &[u8]) -> SyncResult<gix::ObjectId> {
    gix::ObjectId::try_from(bytes).map_err(|_| invalid("invalid Git object identity length"))
}

fn git_kind_code(kind: gix::objs::Kind) -> u8 {
    match kind {
        gix::objs::Kind::Commit => 4,
        gix::objs::Kind::Tree => 5,
        gix::objs::Kind::Blob => 6,
        gix::objs::Kind::Tag => 7,
    }
}

fn git_kind(code: u8) -> SyncResult<gix::objs::Kind> {
    match code {
        4 => Ok(gix::objs::Kind::Commit),
        5 => Ok(gix::objs::Kind::Tree),
        6 => Ok(gix::objs::Kind::Blob),
        7 => Ok(gix::objs::Kind::Tag),
        _ => Err(invalid("invalid streaming Git object kind")),
    }
}

fn write_fields(
    writer: &mut impl std::io::Write,
    kind: u8,
    id: &[u8],
    data: &[u8],
    extras: Option<&[u8]>,
) -> SyncResult<()> {
    writer.write_all(&[1, kind, id.len() as u8])?;
    writer.write_all(id)?;
    writer.write_all(&(data.len() as u64).to_le_bytes())?;
    writer.write_all(
        &extras
            .map_or(u64::MAX, |value| value.len() as u64)
            .to_le_bytes(),
    )?;
    writer.write_all(data)?;
    if let Some(extras) = extras {
        writer.write_all(extras)?;
    }
    Ok(())
}

fn record_size(record: &Record) -> SyncResult<u64> {
    let identity_length = match record.id() {
        RecordId::Operation(id) => id.as_bytes().len(),
        RecordId::View(id) => id.as_bytes().len(),
        RecordId::SharedEvent(id) => id.as_bytes().len(),
        RecordId::Git(id) => id.as_bytes().len(),
    };
    let (data, extras) = match record {
        Record::GitHeader(header) => (9, header.commit_extras.as_ref().map_or(0, Vec::len)),
        _ => {
            let (_, data, extras) = record.parts();
            (data.len(), extras.map_or(0, <[u8]>::len))
        }
    };
    (identity_length as u64 + 19)
        .checked_add(data as u64)
        .and_then(|length| length.checked_add(extras as u64))
        .ok_or_else(|| invalid("record size overflows the quarantine budget"))
}

fn preflight_metadata(record: &Record, max_fields: usize) -> SyncResult<()> {
    use super::metadata_limits;
    match record {
        Record::Operation(_, bytes) => metadata_limits::check_operation(bytes, max_fields),
        Record::View(_, bytes) => metadata_limits::check_view(bytes, max_fields),
        Record::SharedEvent(_, bytes) => {
            metadata_limits::check_view(AnchorStore::encoded_view_bytes(bytes)?, max_fields)
        }
        Record::Git(record) => {
            if record.kind == gix::objs::Kind::Commit {
                check_git_header_lines(&record.data, max_fields)?;
            }
            record.commit_extras.as_deref().map_or(Ok(()), |bytes| {
                metadata_limits::check_git_extras(bytes, max_fields)
            })
        }
        Record::GitCommitMetadata(metadata) => {
            metadata_limits::check_git_extras(&metadata.extras, max_fields)
        }
        Record::GitHeader(header) => header.commit_extras.as_deref().map_or(Ok(()), |bytes| {
            metadata_limits::check_git_extras(bytes, max_fields)
        }),
        Record::GitChunk { .. } | Record::GitEnd(_) => Ok(()),
    }
}

fn check_git_header_lines(body: &[u8], max_lines: usize) -> SyncResult<()> {
    let mut remaining = max_lines;
    for line in body.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            break;
        }
        remaining = remaining.checked_sub(1).ok_or_else(|| {
            invalid(
                "Git commit headers exceed the local sync.max-dependencies budget; increase that \
                 setting",
            )
        })?;
    }
    Ok(())
}

struct ChunkWriter<'a, F> {
    id: gix::ObjectId,
    offset: u64,
    buffer: Vec<u8>,
    emit: &'a mut F,
    failure: Option<SyncError>,
}

impl<F: FnMut(Record) -> SyncResult<()>> ChunkWriter<'_, F> {
    fn emit_buffer(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let data = std::mem::replace(&mut self.buffer, Vec::with_capacity(MAX_CHUNK_SIZE));
        let length = data.len() as u64;
        if let Err(err) = (self.emit)(Record::GitChunk {
            id: self.id,
            offset: self.offset,
            data,
        }) {
            self.failure = Some(err);
            return Err(std::io::Error::other("failed to emit an object body chunk"));
        }
        self.offset += length;
        Ok(())
    }

    fn finish(&mut self) -> SyncResult<()> {
        if let Some(err) = self.failure.take() {
            return Err(err);
        }
        let result = self.emit_buffer();
        if let Some(err) = self.failure.take() {
            return Err(err);
        }
        result.map_err(Into::into)
    }
}

impl<F: FnMut(Record) -> SyncResult<()>> std::io::Write for ChunkWriter<'_, F> {
    fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
        let original_length = bytes.len();
        while !bytes.is_empty() {
            let length = bytes.len().min(MAX_CHUNK_SIZE - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..length]);
            bytes = &bytes[length..];
            if self.buffer.len() == MAX_CHUNK_SIZE {
                self.emit_buffer()?;
            }
        }
        Ok(original_length)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.emit_buffer()
    }
}

fn streamed_dependencies(
    header: &GitObjectHeader,
    from: &mut dyn std::io::Read,
    pending: &mut PendingObjects,
    inventory: &mut Inventory,
) -> SyncResult<()> {
    let mut reader = std::io::BufReader::new(from);
    match header.kind {
        gix::objs::Kind::Blob => {}
        gix::objs::Kind::Tree => loop {
            let Some(first) = read_byte(&mut reader)? else {
                break;
            };
            let mut mode = vec![first];
            loop {
                let byte =
                    read_byte(&mut reader)?.ok_or_else(|| invalid("truncated tree entry mode"))?;
                if byte == b' ' {
                    break;
                }
                if mode.len() >= 6 {
                    return Err(invalid("invalid tree entry mode"));
                }
                mode.push(byte);
            }
            let mode = std::str::from_utf8(&mode)
                .ok()
                .and_then(|text| u32::from_str_radix(text, 8).ok())
                .ok_or_else(|| invalid("invalid tree entry mode"))?;
            let mut has_name = false;
            loop {
                let byte =
                    read_byte(&mut reader)?.ok_or_else(|| invalid("truncated tree entry name"))?;
                if byte == 0 {
                    break;
                }
                has_name = true;
            }
            if !has_name {
                return Err(invalid("empty tree entry name"));
            }
            let mut bytes = vec![0; header.id.as_bytes().len()];
            reader.read_exact(&mut bytes)?;
            let kind = match mode & 0o170000 {
                0o160000 => continue,
                0o040000 => gix::objs::Kind::Tree,
                0o100000 | 0o120000 => gix::objs::Kind::Blob,
                _ => return Err(invalid("unsupported tree entry mode")),
            };
            pending.push((RecordId::Git(git_id(&bytes)?), Some(kind)))?;
        },
        gix::objs::Kind::Commit | gix::objs::Kind::Tag => {
            let (target, target_kind) = git_header_dependencies(header, &mut reader, pending)?;
            if header.kind == gix::objs::Kind::Commit {
                inventory
                    .git_commits
                    .insert(CommitId::from_bytes(header.id.as_bytes()));
                pending.push((RecordId::Git(target), Some(gix::objs::Kind::Tree)))?;
                let extras = crate::protos::git_store::Commit::decode(
                    header
                        .commit_extras
                        .as_deref()
                        .ok_or_else(|| invalid("commit extras are missing"))?,
                )
                .map_err(|err| invalid(format!("invalid commit extras: {err}")))?;
                for id in extras.predecessors {
                    push_commit(pending, &CommitId::new(id))?;
                }
                if extras.uses_tree_conflict_format {
                    for bytes in extras.root_tree {
                        pending
                            .push((RecordId::Git(git_id(&bytes)?), Some(gix::objs::Kind::Tree)))?;
                        inventory.legacy_tree_roots.insert(TreeId::new(bytes));
                    }
                }
            } else {
                pending.push((RecordId::Git(target), Some(target_kind)))?;
            }
        }
    }
    Ok(())
}

fn read_byte(reader: &mut impl std::io::Read) -> SyncResult<Option<u8>> {
    let mut byte = [0];
    match reader.read(&mut byte)? {
        0 => Ok(None),
        _ => Ok(Some(byte[0])),
    }
}

// Retain only the prefix needed for dependency headers; an object's message or
// signature may be arbitrarily large and must never become one allocation.
struct HeaderLine {
    prefix: Vec<u8>,
    length: usize,
    first_space: Option<usize>,
    signature_valid: bool,
}

fn header_line(reader: &mut impl std::io::Read) -> SyncResult<Option<HeaderLine>> {
    let mut prefix = Vec::new();
    let mut length = 0usize;
    let mut first_space = None;
    let mut first_left = None;
    let mut last_right = None;
    let mut suffix_is_time = true;
    loop {
        let Some(byte) = read_byte(reader)? else {
            return if length == 0 {
                Ok(None)
            } else {
                Err(invalid("truncated Git object header line"))
            };
        };
        if byte == b'\n' {
            return Ok(Some(HeaderLine {
                prefix,
                length,
                first_space,
                signature_valid: first_left
                    .zip(last_right)
                    .is_some_and(|(left, right)| left < right)
                    && suffix_is_time,
            }));
        }
        if byte == b' ' && first_space.is_none() {
            first_space = Some(length);
        }
        if byte == b'<' && first_left.is_none() {
            first_left = Some(length);
        }
        if byte == b'>' {
            last_right = Some(length);
            suffix_is_time = true;
        } else if last_right.is_some() && !matches!(byte, b'+' | b'-' | b'0'..=b'9' | b' ' | b'\t')
        {
            suffix_is_time = false;
        }
        if prefix.len() < 256 {
            prefix.push(byte);
        }
        length = length
            .checked_add(1)
            .ok_or_else(|| invalid("Git header length overflow"))?;
    }
}

fn required_header(reader: &mut impl std::io::Read) -> SyncResult<HeaderLine> {
    header_line(reader)?.ok_or_else(|| invalid("missing required Git object header"))
}

fn git_header_dependencies(
    header: &GitObjectHeader,
    reader: &mut impl std::io::Read,
    pending: &mut PendingObjects,
) -> SyncResult<(gix::ObjectId, gix::objs::Kind)> {
    let first = required_header(reader)?;
    if header.kind == gix::objs::Kind::Tag {
        if !first.prefix.starts_with(b"object ") {
            return Err(invalid("tag must begin with its object header"));
        }
        let target = header_object_id(&first.prefix, first.length, 7, header.id.kind())?;
        let kind = required_header(reader)?;
        let target_kind = match kind.prefix.get(..kind.length) {
            Some(b"type commit") => gix::objs::Kind::Commit,
            Some(b"type tree") => gix::objs::Kind::Tree,
            Some(b"type blob") => gix::objs::Kind::Blob,
            Some(b"type tag") => gix::objs::Kind::Tag,
            _ => return Err(invalid("invalid annotated tag target type")),
        };
        let name = required_header(reader)?;
        if !name.prefix.starts_with(b"tag ") || name.length <= 4 {
            return Err(invalid("tag is missing its name"));
        }
        if let Some(next) = header_line(reader)?
            && next.length != 0
        {
            if !next.prefix.starts_with(b"tagger ") || !next.signature_valid {
                return Err(invalid("invalid tagger header"));
            }
            if header_line(reader)?.is_some_and(|line| line.length != 0) {
                return Err(invalid("tag message is missing its separator"));
            }
        }
        return Ok((target, target_kind));
    }
    if !first.prefix.starts_with(b"tree ") {
        return Err(invalid("commit must begin with its tree header"));
    }
    let tree = header_object_id(&first.prefix, first.length, 5, header.id.kind())?;
    let author = loop {
        let line = required_header(reader)?;
        if line.prefix.starts_with(b"parent ") {
            pending.push((
                RecordId::Git(header_object_id(
                    &line.prefix,
                    line.length,
                    7,
                    header.id.kind(),
                )?),
                Some(gix::objs::Kind::Commit),
            ))?;
        } else {
            break line;
        }
    };
    if !author.prefix.starts_with(b"author ") || !author.signature_valid {
        return Err(invalid("invalid commit author header"));
    }
    let committer = required_header(reader)?;
    if !committer.prefix.starts_with(b"committer ") || !committer.signature_valid {
        return Err(invalid("invalid commit committer header"));
    }
    let mut first_extra = true;
    let mut continuation_allowed = false;
    loop {
        let line = required_header(reader)?;
        if line.length == 0 {
            break;
        }
        if line.prefix.first() == Some(&b' ') {
            if !continuation_allowed {
                return Err(invalid("unexpected continued Git header"));
            }
        } else {
            if line.first_space.is_none_or(|position| position == 0) {
                return Err(invalid("invalid extra Git header"));
            }
            continuation_allowed = !(first_extra && line.prefix.starts_with(b"encoding "));
        }
        first_extra = false;
    }
    Ok((tree, gix::objs::Kind::Tree))
}

fn header_object_id(
    prefix: &[u8],
    length: usize,
    start: usize,
    kind: gix::hash::Kind,
) -> SyncResult<gix::ObjectId> {
    if length != start + kind.len_in_bytes() * 2 {
        return Err(invalid("invalid object ID in a Git header"));
    }
    gix::ObjectId::from_hex(&prefix[start..length])
        .map_err(|_| invalid("invalid object ID in a Git header"))
}

fn invalid(message: impl Into<String>) -> SyncError {
    SyncError::InvalidData(message.into())
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use pollster::FutureExt as _;
    use test_case::test_case;

    use super::*;
    use crate::config::StackedConfig;
    use crate::op_store;
    use crate::op_store::OpStore as _;
    use crate::op_store::RootOperationData;
    use crate::settings::UserSettings;
    use crate::sync::shared::SharedEvent;
    use crate::sync::shared::SharedView;
    use crate::tests::TestResult;
    use crate::tests::new_temp_dir;

    struct Fixture {
        directory: TempDir,
        backend: GitBackend,
        operations: SimpleOpStore,
        anchors: AnchorStore,
    }

    impl Fixture {
        fn new(hash: gix::hash::Kind) -> Self {
            let directory = new_temp_dir();
            let settings =
                UserSettings::from_config_and_home_dir(StackedConfig::with_defaults(), None)
                    .unwrap();
            let store = directory.path().join("store");
            fs::create_dir(&store).unwrap();
            let backend = GitBackend::init_internal(&settings, &store, hash).unwrap();
            let op_path = directory.path().join("op_store");
            fs::create_dir(&op_path).unwrap();
            let operations = SimpleOpStore::init(
                &op_path,
                RootOperationData {
                    root_commit_id: backend.root_commit_id().clone(),
                },
            )
            .unwrap();
            let anchors = AnchorStore::init(directory.path()).unwrap();
            Self {
                directory,
                backend,
                operations,
                anchors,
            }
        }

        fn objects(&self) -> RepositoryObjects<'_> {
            RepositoryObjects::new(&self.operations, &self.anchors, &self.backend)
        }

        fn object(
            &self,
            kind: gix::objs::Kind,
            data: Vec<u8>,
            extras: Option<Vec<u8>>,
        ) -> GitObjectRecord {
            let id = gix::objs::compute_hash(self.backend.git_repo().object_hash(), kind, &data)
                .unwrap();
            let record = GitObjectRecord {
                id,
                kind,
                data,
                commit_extras: extras,
            };
            self.backend.import_object(&record).unwrap();
            record
        }

        fn commit(
            &self,
            tree: gix::ObjectId,
            parents: &[gix::ObjectId],
            predecessors: &[gix::ObjectId],
            description: &str,
        ) -> GitObjectRecord {
            let mut data = format!("tree {tree}\n");
            for id in parents {
                writeln!(data, "parent {id}").unwrap();
            }
            data.push_str(
                "author A <a@example.com> 0 +0000\ncommitter A <a@example.com> 0 +0000\n\n",
            );
            data.push_str(description);
            let extras = crate::protos::git_store::Commit {
                change_id: vec![1; 16],
                predecessors: predecessors
                    .iter()
                    .map(|id| id.as_bytes().to_vec())
                    .collect(),
                uses_tree_conflict_format: true,
                ..Default::default()
            }
            .encode_to_vec();
            self.object(gix::objs::Kind::Commit, data.into_bytes(), Some(extras))
        }

        fn operation(
            &self,
            commit: gix::ObjectId,
            anchor: Option<&SharedEventId>,
            description: &str,
        ) -> OperationId {
            let mut view = op_store::View::make_root(self.backend.root_commit_id().clone());
            view.head_ids
                .insert(CommitId::from_bytes(commit.as_bytes()));
            let mut op = self
                .operations
                .read_operation(self.operations.root_operation_id())
                .block_on()
                .unwrap();
            op.parents = vec![self.operations.root_operation_id().clone()];
            op.view_id = self.operations.write_view(&view).block_on().unwrap();
            op.metadata.description = description.to_owned();
            if let Some(anchor) = anchor {
                op.metadata
                    .attributes
                    .insert(INTEGRATION_ANCHOR_ATTRIBUTE.to_owned(), anchor.hex());
            }
            self.operations.write_operation(&op).block_on().unwrap()
        }
    }

    fn fetch_test_pack(
        source: &GitPackRepository,
        receiver: &GitPackRepository,
        backend: &GitBackend,
    ) {
        let output = std::process::Command::new(backend.git_executable_path())
            .arg("--git-dir")
            .arg(receiver.path())
            .args([
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "--no-tags",
                "--no-auto-maintenance",
                "--no-write-fetch-head",
            ])
            .arg(source.path())
            .arg("+refs/jj-sync/export/*:refs/jj-sync/incoming/*")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test_case(gix::hash::Kind::Sha1; "sha1")]
    #[test_case(gix::hash::Kind::Sha256; "sha256")]
    fn packed_quarantine_preserves_metadata_predecessors_and_legacy_trees(
        hash: gix::hash::Kind,
    ) -> TestResult {
        let source = Fixture::new(hash);
        let destination = Fixture::new(hash);
        let empty = source.object(gix::objs::Kind::Tree, vec![], None);
        let objects_path = destination.backend.git_repo_path().join("objects");
        let empty_hex = empty.id.to_string();
        let receiver_empty_path = objects_path.join(&empty_hex[..2]).join(&empty_hex[2..]);
        if receiver_empty_path.try_exists()? {
            fs::remove_file(&receiver_empty_path)?;
        }
        let output = std::process::Command::new(destination.backend.git_executable_path())
            .arg("--git-dir")
            .arg(destination.backend.git_repo_path())
            .args([
                "-c",
                "core.compression=0",
                "hash-object",
                "-w",
                "-t",
                "tree",
                "--stdin",
            ])
            .stdin(std::process::Stdio::null())
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, format!("{empty_hex}\n").as_bytes());
        let receiver_empty_bytes = fs::read(&receiver_empty_path)?;
        let blob = source.object(gix::objs::Kind::Blob, b"legacy tree body".to_vec(), None);
        let mut bytes = b"100644 file\0".to_vec();
        bytes.extend(blob.id.as_bytes());
        let tree = source.object(gix::objs::Kind::Tree, bytes, None);
        let predecessor = source.commit(tree.id, &[], &[], "metadata-only predecessor");
        let physical = source.commit(empty.id, &[], &[], "legacy conflict");
        let extras = crate::protos::git_store::Commit {
            change_id: vec![7; 16],
            predecessors: vec![predecessor.id.as_bytes().to_vec()],
            uses_tree_conflict_format: true,
            root_tree: vec![
                tree.id.as_bytes().to_vec(),
                empty.id.as_bytes().to_vec(),
                tree.id.as_bytes().to_vec(),
            ],
            ..Default::default()
        }
        .encode_to_vec();
        let mut body = physical.data;
        body.extend(b" with detached tree terms");
        let commit = source.object(gix::objs::Kind::Commit, body, Some(extras));
        let head = source.operation(commit.id, None, "packed foreign history");
        let objects = source.objects();
        let inventory = objects.inventory_packed(std::slice::from_ref(&head), &[])?;
        assert_eq!(
            inventory,
            objects.inventory(std::slice::from_ref(&head), &[])?
        );
        let limits = QuarantineLimits::for_directory(destination.directory.path())?;
        let sender = GitPackRepository::sender(
            source.directory.path(),
            &source.backend,
            &inventory,
            &limits,
        )?;
        let receiver_inventory = destination.objects().inventory_packed(&[], &[])?;
        let packed = GitPackRepository::receiver(
            destination.directory.path(),
            &destination.backend,
            &receiver_inventory,
            &limits,
        )?;
        assert_ne!(
            receiver_empty_bytes.len(),
            fs::metadata(
                packed
                    .path()
                    .join("objects")
                    .join(&empty_hex[..2])
                    .join(&empty_hex[2..]),
            )?
            .len() as usize
        );
        fetch_test_pack(&sender, &packed, &destination.backend);
        let existing_index = if hash == gix::hash::Kind::Sha1 {
            let incoming_pack = fs::read_dir(packed.path().join("objects/pack"))?
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "pack")
                })
                .unwrap();
            assert_eq!(
                gix::odb::pack::index::File::at(incoming_pack.with_extension("idx"), hash)?
                    .version(),
                gix::odb::pack::index::Version::V2
            );
            let live_pack_directory = objects_path.join("pack");
            fs::create_dir_all(&live_pack_directory)?;
            let live_pack = live_pack_directory.join(incoming_pack.file_name().unwrap());
            fs::copy(&incoming_pack, &live_pack)?;
            let live_index = live_pack.with_extension("idx");
            let output = std::process::Command::new(destination.backend.git_executable_path())
                .arg("--git-dir")
                .arg(destination.backend.git_repo_path())
                .args(["index-pack", "--index-version=1", "-o"])
                .arg(&live_index)
                .arg(&live_pack)
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                gix::odb::pack::index::File::at(&live_index, hash)?.version(),
                gix::odb::pack::index::Version::V1
            );
            let bytes = fs::read(&live_index)?;
            assert_ne!(
                bytes.len() as u64,
                fs::metadata(incoming_pack.with_extension("idx"))?.len()
            );
            Some((live_index, bytes))
        } else {
            None
        };
        let mut quarantine = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            limits,
        )?;
        quarantine.set_git_pack_repository(packed)?;
        // Even claiming every physical object as known cannot omit its jj identity.
        let known = inventory
            .records
            .iter()
            .filter(|id| matches!(id, RecordId::Git(_)))
            .cloned()
            .collect();
        let mut sidecars = 0;
        objects.export_metadata_records(&inventory, &known, |record| {
            assert!(!matches!(
                record,
                Record::Git(_) | Record::GitHeader(_) | Record::GitChunk { .. } | Record::GitEnd(_)
            ));
            if matches!(record, Record::GitCommitMetadata(_)) {
                sidecars += 1;
            }
            assert_eq!(Record::decode(&record.encode()?)?, record);
            quarantine.stage(&destination.objects(), &record)
        })?;
        assert_eq!(sidecars, inventory.git_commits.len());
        let validated =
            quarantine.validate(&destination.objects(), std::slice::from_ref(&head), &[])?;
        assert_eq!(validated, inventory);
        quarantine.install(&destination.objects(), &validated)?;
        assert_eq!(fs::read(&receiver_empty_path)?, receiver_empty_bytes);
        if let Some((path, bytes)) = existing_index {
            assert_eq!(fs::read(path)?, bytes);
        }
        assert_eq!(destination.backend.export_object(&commit.id)?, commit);
        assert_eq!(
            destination.backend.export_object(&predecessor.id)?,
            predecessor
        );
        assert_eq!(
            destination.operations.export_operation(&head)?,
            source.operations.export_operation(&head)?
        );
        let blob_hex = blob.id.to_string();
        assert!(
            !objects_path
                .join(&blob_hex[..2])
                .join(&blob_hex[2..])
                .exists()
        );
        assert!(fs::read_dir(objects_path.join("pack"))?.any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|extension| extension == "pack")
        }));
        assert!(
            destination
                .backend
                .git_repo()
                .find_reference(format!("refs/jj/sync/trees/{}", tree.id).as_str())
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn packed_metadata_conflicts_and_missing_sidecars_fail_closed() -> TestResult {
        let source = Fixture::new(gix::hash::Kind::Sha1);
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let empty = source.object(gix::objs::Kind::Tree, vec![], None);
        let commit = source.commit(empty.id, &[], &[], "known physical commit");
        let head = source.operation(commit.id, None, "incoming");
        let inventory = source
            .objects()
            .inventory_packed(std::slice::from_ref(&head), &[])?;
        let mut existing = commit.clone();
        let mut extras =
            crate::protos::git_store::Commit::decode(existing.commit_extras.as_deref().unwrap())?;
        extras.change_id = vec![9; 16];
        existing.commit_extras = Some(extras.encode_to_vec());
        destination.backend.import_object(&existing)?;
        let limits = QuarantineLimits::for_directory(destination.directory.path())?;
        let known =
            destination
                .objects()
                .inventory_packed_with_git_roots(&[], &[], &[commit.id])?;
        let packed = GitPackRepository::receiver(
            destination.directory.path(),
            &destination.backend,
            &known,
            &limits,
        )?;
        let mut quarantine = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            limits,
        )?;
        quarantine.set_git_pack_repository(packed)?;
        source
            .objects()
            .export_metadata_records(&inventory, &BTreeSet::new(), |record| {
                if matches!(record, Record::GitCommitMetadata(_)) {
                    return Ok(());
                }
                quarantine.stage(&destination.objects(), &record)
            })?;
        assert!(
            quarantine
                .validate(&destination.objects(), std::slice::from_ref(&head), &[])
                .unwrap_err()
                .to_string()
                .contains("omitted mandatory commit metadata")
        );
        let sidecar = Record::GitCommitMetadata(source.backend.export_commit_metadata(&commit.id)?);
        quarantine.stage(&destination.objects(), &sidecar)?;
        assert!(
            quarantine
                .validate(&destination.objects(), std::slice::from_ref(&head), &[])
                .is_err()
        );
        assert_eq!(destination.backend.export_object(&commit.id)?, existing);
        Ok(())
    }

    #[test]
    fn packed_quarantine_rejects_unoffered_received_objects() -> TestResult {
        let source = Fixture::new(gix::hash::Kind::Sha1);
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let empty = source.object(gix::objs::Kind::Tree, vec![], None);
        let commit = source.commit(empty.id, &[], &[], "offered");
        let head = source.operation(commit.id, None, "incoming");
        let extra = source.object(
            gix::objs::Kind::Blob,
            b"not in captured history".to_vec(),
            None,
        );
        let inventory = source
            .objects()
            .inventory_packed(std::slice::from_ref(&head), &[])?;
        let limits = QuarantineLimits::for_directory(destination.directory.path())?;
        let sender = GitPackRepository::sender(
            source.directory.path(),
            &source.backend,
            &inventory,
            &limits,
        )?;
        fs::create_dir_all(sender.path().join("refs/jj-sync/export"))?;
        fs::write(
            sender.path().join("refs/jj-sync/export/unoffered"),
            format!("{}\n", extra.id),
        )?;
        let packed = GitPackRepository::receiver(
            destination.directory.path(),
            &destination.backend,
            &Inventory::default(),
            &limits,
        )?;
        fetch_test_pack(&sender, &packed, &destination.backend);
        let mut quarantine = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            limits,
        )?;
        quarantine.set_git_pack_repository(packed)?;
        source
            .objects()
            .export_metadata_records(&inventory, &BTreeSet::new(), |record| {
                quarantine.stage(&destination.objects(), &record)
            })?;
        assert!(
            quarantine
                .validate(&destination.objects(), std::slice::from_ref(&head), &[])
                .unwrap_err()
                .to_string()
                .contains("outside the offered inventory")
        );
        assert!(destination.backend.export_object(&extra.id).is_err());
        Ok(())
    }

    #[test]
    fn packed_quarantine_budget_counts_compressed_files_not_blob_bodies() -> TestResult {
        let source = Fixture::new(gix::hash::Kind::Sha1);
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let blob = source.object(gix::objs::Kind::Blob, vec![b'x'; 4 * 1024 * 1024], None);
        let mut bytes = b"100644 compressible\0".to_vec();
        bytes.extend(blob.id.as_bytes());
        let tree = source.object(gix::objs::Kind::Tree, bytes, None);
        let commit = source.commit(tree.id, &[], &[], "large compressed blob");
        let head = source.operation(commit.id, None, "compressed history");
        let inventory = source
            .objects()
            .inventory_packed(std::slice::from_ref(&head), &[])?;
        // Leave room for Git's administrative files and their allocation
        // overhead while staying far below the uncompressed blob's size.
        let limits = tiny_limits(512 * 1024);
        let sender = GitPackRepository::sender(
            source.directory.path(),
            &source.backend,
            &inventory,
            &limits,
        )?;
        assert!(!sender.path().join("refs/jj-sync/export").exists());
        let packed = GitPackRepository::receiver(
            destination.directory.path(),
            &destination.backend,
            &Inventory::default(),
            &limits,
        )?;
        fetch_test_pack(&sender, &packed, &destination.backend);
        let extra_scratch = packed.path().join("extra-scratch");
        let mut quarantine = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            limits,
        )?;
        quarantine.set_git_pack_repository(packed)?;
        source
            .objects()
            .export_metadata_records(&inventory, &BTreeSet::new(), |record| {
                assert!(!matches!(
                    record,
                    Record::Git(_)
                        | Record::GitHeader(_)
                        | Record::GitChunk { .. }
                        | Record::GitEnd(_)
                ));
                quarantine.stage(&destination.objects(), &record)
            })?;
        // Control files and incoming refs consume the same physical allowance
        // as object packs, even though they are never promoted to the live ODB.
        fs::write(&extra_scratch, vec![0; 512 * 1024])?;
        assert!(
            quarantine
                .validate(&destination.objects(), std::slice::from_ref(&head), &[])
                .unwrap_err()
                .to_string()
                .contains("sync.max-quarantine-bytes")
        );
        fs::remove_file(extra_scratch)?;
        let validated =
            quarantine.validate(&destination.objects(), std::slice::from_ref(&head), &[])?;
        assert!(fs::read_dir(quarantine.directory.path())?.all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("body-")
        }));
        quarantine.install(&destination.objects(), &validated)?;
        assert_eq!(destination.backend.export_object(&blob.id)?, blob);
        Ok(())
    }

    #[test_case(gix::hash::Kind::Sha1; "sha1")]
    #[test_case(gix::hash::Kind::Sha256; "sha256")]
    fn quarantine_preserves_complete_cross_history_closure(hash: gix::hash::Kind) -> TestResult {
        let source = Fixture::new(hash);
        let destination = Fixture::new(hash);
        let blob = source.object(gix::objs::Kind::Blob, b"file contents\n".to_vec(), None);
        let mut tree_data = b"100644 file\0".to_vec();
        tree_data.extend_from_slice(blob.id.as_bytes());
        // A gitlink belongs to another repository and intentionally has no object here.
        tree_data.extend_from_slice(b"160000 submodule\0");
        tree_data.extend_from_slice(&vec![9; hash.len_in_bytes()]);
        let tree = source.object(gix::objs::Kind::Tree, tree_data, None);
        let predecessor = source.commit(tree.id, &[], &[], "predecessor only in jj extras");
        let commit = source.commit(tree.id, &[], &[predecessor.id], "current");
        let original = source.operation(commit.id, None, "foreign original");
        let op = source.operations.read_operation(&original).block_on()?;
        let view = source.operations.read_view(&op.view_id).block_on()?;
        let shared = source.anchors.write(&SharedEvent::new(
            vec![],
            SharedView::from_view(&view),
            Some(original.clone()),
        ))?;
        let head = source.operation(
            commit.id,
            Some(&shared),
            "integration with shared cross-link",
        );
        let source_objects = source.objects();
        let receiver = destination.objects();
        let inventory = source_objects.inventory(std::slice::from_ref(&head), &[])?;
        assert!(
            inventory
                .records
                .contains(&RecordId::Operation(original.clone()))
        );
        assert!(
            inventory
                .records
                .contains(&RecordId::SharedEvent(shared.clone()))
        );
        assert!(
            inventory
                .git_commits
                .contains(&CommitId::from_bytes(predecessor.id.as_bytes()))
        );
        assert!(inventory.legacy_tree_roots.is_empty());

        // Excluding one known payload must still validate all of its dependencies.
        destination.backend.import_object(&blob)?;
        let known = BTreeSet::from([RecordId::Git(blob.id)]);
        assert!(
            inventory
                .missing(&BTreeSet::from([RecordId::Git(commit.id)]))
                .any(|id| id == &RecordId::Git(commit.id))
        );
        let mut staging = Quarantine::new(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
        )?;
        for id in inventory.missing(&known) {
            let record = source_objects.export_record(id)?;
            assert_eq!(Record::decode(&record.encode()?)?, record);
            staging.stage(&receiver, &record)?;
        }
        assert!(
            receiver
                .operations
                .read_operation(&head)
                .block_on()
                .is_err()
        );
        let verified = staging.validate(&receiver, std::slice::from_ref(&head), &[])?;
        assert_eq!(verified, inventory);
        staging.install(&receiver, &verified)?;
        assert_eq!(
            destination.operations.export_operation(&head)?,
            source.operations.export_operation(&head)?
        );
        assert_eq!(
            destination.operations.export_operation(&original)?,
            source.operations.export_operation(&original)?
        );
        assert_eq!(destination.backend.export_object(&commit.id)?, commit);
        assert_eq!(
            destination.backend.export_object(&predecessor.id)?,
            predecessor
        );
        assert_eq!(
            destination.anchors.read(&shared)?,
            source.anchors.read(&shared)?
        );
        assert!(!destination.directory.path().join("op_heads").exists());
        Ok(())
    }

    #[test]
    fn known_id_claim_does_not_hide_missing_dependency() -> TestResult {
        let source = Fixture::new(gix::hash::Kind::Sha1);
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let blob = source.object(gix::objs::Kind::Blob, b"contents".to_vec(), None);
        let mut tree_data = b"100644 file\0".to_vec();
        tree_data.extend_from_slice(blob.id.as_bytes());
        let tree = source.object(gix::objs::Kind::Tree, tree_data, None);
        let commit = source.commit(tree.id, &[], &[], "needs tree");
        let head = source.operation(commit.id, None, "head");
        let inventory = source
            .objects()
            .inventory(std::slice::from_ref(&head), &[])?;
        let known = BTreeSet::from([RecordId::Git(tree.id)]);
        let receiver = destination.objects();
        let mut staging = Quarantine::new(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
        )?;
        for id in inventory.missing(&known) {
            staging.stage(&receiver, &source.objects().export_record(id)?)?;
        }
        assert!(
            staging
                .validate(&receiver, std::slice::from_ref(&head), &[])
                .is_err()
        );
        assert!(
            destination
                .operations
                .read_operation(&head)
                .block_on()
                .is_err()
        );
        assert!(destination.backend.export_object(&commit.id).is_err());
        Ok(())
    }

    #[test]
    fn missing_parent_and_wrong_dependency_kind_fail_closed() -> TestResult {
        let source = Fixture::new(gix::hash::Kind::Sha1);
        let tree = source.object(gix::objs::Kind::Tree, vec![], None);
        let missing = gix::ObjectId::from_bytes_or_panic(&[9; 20]);
        let commit = source.commit(tree.id, &[missing], &[], "missing physical parent");
        let head = source.operation(commit.id, None, "head");
        assert!(source.objects().inventory(&[head], &[]).is_err());

        let mut invalid_tree = b"100644 file\0".to_vec();
        invalid_tree.extend_from_slice(tree.id.as_bytes());
        let wrong = source.object(gix::objs::Kind::Tree, invalid_tree, None);
        let commit = source.commit(wrong.id, &[], &[], "tree used as blob");
        let head = source.operation(commit.id, None, "wrong kind");
        assert!(source.objects().inventory(&[head], &[]).is_err());
        Ok(())
    }

    #[test]
    fn explicit_annotated_tags_include_their_target_closure() -> TestResult {
        let source = Fixture::new(gix::hash::Kind::Sha1);
        let blob = source.object(gix::objs::Kind::Blob, b"tagged data".to_vec(), None);
        let tag = source.object(
            gix::objs::Kind::Tag,
            format!(
                "object {}\ntype blob\ntag v1\ntagger A <a@example.com> 0 +0000\n\nmessage\n",
                blob.id
            )
            .into_bytes(),
            None,
        );
        let inventory = source
            .objects()
            .inventory_with_git_roots(&[], &[], &[tag.id])?;
        assert_eq!(
            inventory.records,
            BTreeSet::from([RecordId::Git(tag.id), RecordId::Git(blob.id)])
        );
        Ok(())
    }

    #[test]
    fn original_predecessor_records_and_shared_parents_are_dependencies() -> TestResult {
        let source = Fixture::new(gix::hash::Kind::Sha1);
        let tree = source.object(gix::objs::Kind::Tree, vec![], None);
        let visible = source.commit(tree.id, &[], &[], "visible");
        let hidden = source.commit(tree.id, &[], &[], "only in original predecessor map");
        let raw = source.operation(visible.id, None, "original");
        let mut operation = source.operations.read_operation(&raw).block_on()?;
        operation.commit_predecessors = Some(BTreeMap::from([(
            CommitId::from_bytes(hidden.id.as_bytes()),
            vec![CommitId::from_bytes(visible.id.as_bytes())],
        )]));
        let raw = source.operations.write_operation(&operation).block_on()?;
        let view = source.operations.read_view(&operation.view_id).block_on()?;
        let first = source.anchors.write(&SharedEvent::new(
            vec![],
            SharedView::from_view(&view),
            Some(raw.clone()),
        ))?;
        let next = source.anchors.write(&SharedEvent::new(
            vec![first.clone()],
            SharedView::from_view(&view),
            None,
        ))?;
        let inventory = source.objects().inventory(&[], &[next])?;
        assert!(inventory.records.contains(&RecordId::SharedEvent(first)));
        assert!(inventory.records.contains(&RecordId::Operation(raw)));
        assert!(
            inventory
                .git_commits
                .contains(&CommitId::from_bytes(hidden.id.as_bytes()))
        );
        Ok(())
    }

    #[test]
    fn legacy_extras_only_tree_terms_are_transferred_and_pinned() -> TestResult {
        let source = Fixture::new(gix::hash::Kind::Sha1);
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let empty = source.object(gix::objs::Kind::Tree, vec![], None);
        let mut terms = Vec::new();
        for contents in [b"left".as_slice(), b"right".as_slice()] {
            let blob = source.object(gix::objs::Kind::Blob, contents.to_vec(), None);
            let mut tree_data = b"100644 file\0".to_vec();
            tree_data.extend_from_slice(blob.id.as_bytes());
            terms.push(source.object(gix::objs::Kind::Tree, tree_data, None).id);
        }
        let plain = source.commit(empty.id, &[], &[], "physical commit");
        let mut data = plain.data;
        data.extend_from_slice(b" with legacy conflict metadata");
        let term_ids = vec![terms[0], empty.id, terms[1]];
        let extras = crate::protos::git_store::Commit {
            change_id: vec![3; 16],
            uses_tree_conflict_format: true,
            root_tree: term_ids.iter().map(|id| id.as_bytes().to_vec()).collect(),
            ..Default::default()
        }
        .encode_to_vec();
        let conflict = source.object(gix::objs::Kind::Commit, data, Some(extras));
        let head = source.operation(conflict.id, None, "legacy conflict");
        let inventory = source
            .objects()
            .inventory(std::slice::from_ref(&head), &[])?;
        assert_eq!(
            inventory.legacy_tree_roots,
            term_ids
                .iter()
                .map(|id| TreeId::from_bytes(id.as_bytes()))
                .collect()
        );
        let receiver = destination.objects();
        let mut staging = Quarantine::new(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
        )?;
        for id in &inventory.records {
            staging.stage(&receiver, &source.objects().export_record(id)?)?;
        }
        staging.install(&receiver, &inventory)?;
        let decoded = destination
            .backend
            .read_commit(&CommitId::from_bytes(conflict.id.as_bytes()))
            .block_on()?;
        assert_eq!(
            decoded
                .root_tree
                .iter()
                .map(|id| id.as_bytes())
                .collect::<Vec<_>>(),
            term_ids.iter().map(|id| id.as_bytes()).collect::<Vec<_>>()
        );
        for id in &term_ids {
            assert!(destination.backend.export_object(id).is_ok());
        }
        Ok(())
    }

    #[test]
    fn rejected_metadata_never_replaces_receiver_identity() -> TestResult {
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let tree = destination.object(gix::objs::Kind::Tree, vec![], None);
        let original = destination.commit(tree.id, &[], &[], "same physical commit");
        let mut conflicting = original.clone();
        let mut extras = crate::protos::git_store::Commit::decode(
            conflicting.commit_extras.as_deref().unwrap(),
        )?;
        extras.change_id = vec![2; 16];
        conflicting.commit_extras = Some(extras.encode_to_vec());
        let mut staging = Quarantine::new(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
        )?;
        assert!(
            staging
                .stage(&destination.objects(), &Record::Git(conflicting))
                .is_err()
        );
        assert!(staging.staged.is_empty());
        assert_eq!(destination.backend.export_object(&original.id)?, original);
        Ok(())
    }

    #[test]
    fn record_codec_rejects_truncation_length_overflow_and_trailing_data() -> TestResult {
        let record = Record::Operation(OperationId::from_bytes(&[1; 64]), b"payload".to_vec());
        let encoded = record.encode()?;
        for length in 0..encoded.len() {
            assert!(Record::decode(&encoded[..length]).is_err());
        }
        let mut overflow = encoded.clone();
        overflow[67..75].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Record::decode(&overflow).is_err());
        let mut trailing = encoded;
        trailing.push(0);
        assert!(Record::decode(&trailing).is_err());
        Ok(())
    }

    fn large_object(
        source: &Fixture,
        kind: gix::objs::Kind,
        prefix: &[u8],
        repeated: u64,
    ) -> GitObjectHeader {
        let size = prefix.len() as u64 + repeated;
        let mut hash = gix::hash::hasher(source.backend.git_repo().object_hash());
        hash.update(
            format!(
                "{} {size}\0",
                match kind {
                    gix::objs::Kind::Blob => "blob",
                    gix::objs::Kind::Commit => "commit",
                    _ => unreachable!(),
                }
            )
            .as_bytes(),
        );
        hash.update(prefix);
        let mut body = NamedTempFile::new_in(source.directory.path()).unwrap();
        body.write_all(prefix).unwrap();
        let buffer = [b'x'; 64 * 1024];
        let mut remaining = repeated;
        while remaining > 0 {
            let count = remaining.min(buffer.len() as u64) as usize;
            body.write_all(&buffer[..count]).unwrap();
            hash.update(&buffer[..count]);
            remaining -= count as u64;
        }
        let header = GitObjectHeader {
            id: hash.try_finalize().unwrap(),
            kind,
            size,
            commit_extras: (kind == gix::objs::Kind::Commit).then(|| {
                crate::protos::git_store::Commit {
                    change_id: vec![7; 16],
                    uses_tree_conflict_format: true,
                    ..Default::default()
                }
                .encode_to_vec()
            }),
        };
        body.as_file_mut().rewind().unwrap();
        source
            .backend
            .import_object_stream(&header, body.as_file_mut())
            .unwrap();
        header
    }

    #[test_case(gix::objs::Kind::Blob; "large blob")]
    #[test_case(gix::objs::Kind::Commit; "large commit message")]
    fn objects_larger_than_inline_limit_stream_end_to_end(kind: gix::objs::Kind) -> TestResult {
        let source = Fixture::new(gix::hash::Kind::Sha1);
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let empty = source.object(gix::objs::Kind::Tree, vec![], None);
        let prefix = if kind == gix::objs::Kind::Commit {
            format!(
                "tree {}\nauthor A <a@example.com> 0 +0000\ncommitter A <a@example.com> 0 \
                 +0000\n\n",
                empty.id
            )
            .into_bytes()
        } else {
            Vec::new()
        };
        let large = large_object(
            &source,
            kind,
            &prefix,
            (MAX_RECORD_SIZE + MAX_CHUNK_SIZE + 17) as u64,
        );
        let commit = if kind == gix::objs::Kind::Blob {
            let mut tree = b"100644 large\0".to_vec();
            tree.extend_from_slice(large.id.as_bytes());
            let tree = source.object(gix::objs::Kind::Tree, tree, None);
            source
                .commit(tree.id, &[], &[], "points to a large blob")
                .id
        } else {
            large.id
        };
        let head = source.operation(commit, None, "large object closure");
        let objects = source.objects();
        let receiver = destination.objects();
        assert!(matches!(
            objects.export_record(&RecordId::Git(large.id))?,
            Record::GitHeader(_)
        ));
        let inventory = objects.inventory(std::slice::from_ref(&head), &[])?;
        let mut staging = Quarantine::new(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
        )?;
        let mut chunks = 0;
        let mut total_bytes = 0u64;
        for id in &inventory.records {
            objects.export_records(id, |record| {
                if let Record::GitChunk { data, .. } = &record {
                    assert!(data.len() <= MAX_CHUNK_SIZE);
                    chunks += 1;
                    total_bytes += data.len() as u64;
                }
                let bytes = record.encode()?;
                assert!(bytes.len() <= MAX_ENCODED_RECORD_SIZE);
                staging.stage(&receiver, &Record::decode(&bytes)?)
            })?;
        }
        assert!(chunks > 64);
        assert_eq!(total_bytes, large.size);
        staging.install(&receiver, &inventory)?;
        assert_eq!(destination.backend.export_object_header(&large.id)?, large);
        destination
            .backend
            .export_object_stream(&large, &mut std::io::sink())?;
        assert_eq!(
            destination.operations.export_operation(&head)?,
            source.operations.export_operation(&head)?
        );
        Ok(())
    }

    #[test]
    fn streaming_offsets_completion_and_hash_are_verified_before_install() -> TestResult {
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let id =
            gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, b"abc").unwrap();
        let header = GitObjectHeader {
            id,
            kind: gix::objs::Kind::Blob,
            size: 3,
            commit_extras: None,
        };
        let receiver = destination.objects();
        let mut staging = Quarantine::new(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
        )?;
        staging.stage(&receiver, &Record::GitHeader(header.clone()))?;
        assert!(
            staging
                .stage(
                    &receiver,
                    &Record::GitChunk {
                        id,
                        offset: 1,
                        data: b"abc".to_vec()
                    }
                )
                .is_err()
        );
        assert!(
            staging
                .validate_with_git_roots(&receiver, &[], &[], &[id])
                .is_err()
        );
        staging.stage(
            &receiver,
            &Record::GitChunk {
                id,
                offset: 0,
                data: b"ab".to_vec(),
            },
        )?;
        assert!(staging.stage(&receiver, &Record::GitEnd(id)).is_err());
        staging.stage(&receiver, &Record::GitHeader(header))?;
        staging.stage(
            &receiver,
            &Record::GitChunk {
                id,
                offset: 0,
                data: b"abd".to_vec(),
            },
        )?;
        assert!(staging.stage(&receiver, &Record::GitEnd(id)).is_err());
        assert!(staging.staged.is_empty());
        assert!(destination.backend.export_object_header(&id).is_err());
        Ok(())
    }

    #[test]
    fn malformed_streamed_tree_fails_dependency_validation() -> TestResult {
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let mut body = b"100644 truncated\0".to_vec();
        body.extend_from_slice(&[1; 19]);
        let id =
            gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Tree, &body).unwrap();
        let header = GitObjectHeader {
            id,
            kind: gix::objs::Kind::Tree,
            size: body.len() as u64,
            commit_extras: None,
        };
        let receiver = destination.objects();
        let mut staging = Quarantine::new(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
        )?;
        staging.stage(&receiver, &Record::GitHeader(header))?;
        staging.stage(
            &receiver,
            &Record::GitChunk {
                id,
                offset: 0,
                data: body,
            },
        )?;
        staging.stage(&receiver, &Record::GitEnd(id))?;
        assert!(
            staging
                .validate_with_git_roots(&receiver, &[], &[], &[id])
                .is_err()
        );
        assert!(destination.backend.export_object_header(&id).is_err());
        Ok(())
    }

    fn tiny_limits(bytes: u64) -> QuarantineLimits {
        QuarantineLimits {
            max_staged_bytes: bytes,
            max_records: 10,
            max_dependencies: 10,
            min_free_bytes: 0,
        }
    }

    fn blob_record(bytes: &[u8]) -> Record {
        Record::Git(GitObjectRecord {
            id: gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
                .unwrap(),
            kind: gix::objs::Kind::Blob,
            data: bytes.to_vec(),
            commit_extras: None,
        })
    }

    #[test]
    fn quarantine_byte_budget_accounts_for_small_files_and_duplicates() -> TestResult {
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let first = blob_record(b"a");
        let second = blob_record(b"b");
        let cost = record_size(&first)? + FILE_OVERHEAD;
        let mut staging = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            tiny_limits(cost * 2),
        )?;
        let receiver = destination.objects();
        staging.stage(&receiver, &first)?;
        staging.stage(&receiver, &first)?;
        assert_eq!(staging.reserved_bytes, cost);
        assert_eq!(staging.reserved_records, 1);
        staging.stage(&receiver, &second)?;
        assert_eq!(staging.reserved_bytes, cost * 2);
        let error = staging.stage(&receiver, &blob_record(b"c")).unwrap_err();
        assert!(error.to_string().contains("sync.max-quarantine-bytes"));
        assert_eq!(fs::read_dir(staging.directory.path())?.count(), 2);
        assert_eq!(staging.staged.len(), 2);
        if let RecordId::Git(id) = first.id() {
            assert!(destination.backend.export_object_header(&id).is_err());
        }
        Ok(())
    }

    #[test]
    fn quarantine_record_budget_and_free_space_are_checked_before_creation() -> TestResult {
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let mut limits = tiny_limits(1024 * 1024);
        limits.max_records = 1;
        let mut staging = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            limits.clone(),
        )?;
        let receiver = destination.objects();
        staging.stage(&receiver, &blob_record(b"a"))?;
        assert!(
            staging
                .stage(&receiver, &blob_record(b"b"))
                .unwrap_err()
                .to_string()
                .contains("sync.max-records")
        );
        assert_eq!(fs::read_dir(staging.directory.path())?.count(), 1);
        limits.min_free_bytes = u64::MAX;
        let mut no_space = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            limits,
        )?;
        assert!(no_space.stage(&receiver, &blob_record(b"a")).is_err());
        assert_eq!(fs::read_dir(no_space.directory.path())?.count(), 0);
        assert_eq!(no_space.reserved_records, 0);
        let mut no_budget = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            tiny_limits(0),
        )?;
        let malformed = Record::Operation(OperationId::new(vec![1; 64]), vec![0; 100]);
        assert!(
            no_budget
                .stage(&receiver, &malformed)
                .unwrap_err()
                .to_string()
                .contains("sync.max-quarantine-bytes")
        );
        assert_eq!(fs::read_dir(no_budget.directory.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn quarantine_streams_reserve_the_whole_body_and_limit_open_files() -> TestResult {
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let Record::Git(blob) = blob_record(b"abc") else {
            unreachable!()
        };
        let header = GitObjectHeader {
            id: blob.id,
            kind: blob.kind,
            size: 3,
            commit_extras: None,
        };
        let announced = Record::GitHeader(header.clone());
        let cost = record_size(&announced)? + header.size + FILE_OVERHEAD * 2;
        let receiver = destination.objects();
        let mut too_small = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            tiny_limits(cost - 1),
        )?;
        assert!(too_small.stage(&receiver, &announced).is_err());
        assert!(too_small.streams.is_empty());
        assert_eq!(fs::read_dir(too_small.directory.path())?.count(), 0);
        let mut staging = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            tiny_limits(cost),
        )?;
        let mut overflowing = header.clone();
        overflowing.size = u64::MAX;
        assert!(
            staging
                .stage(&receiver, &Record::GitHeader(overflowing))
                .is_err()
        );
        assert_eq!(fs::read_dir(staging.directory.path())?.count(), 0);
        staging.stage(&receiver, &announced)?;
        let RecordId::Git(other_id) = blob_record(b"xyz").id() else {
            unreachable!()
        };
        let other = Record::GitHeader(GitObjectHeader {
            id: other_id,
            ..header.clone()
        });
        assert!(
            staging
                .stage(&receiver, &other)
                .unwrap_err()
                .to_string()
                .contains("only one")
        );
        assert!(
            staging
                .stage(&receiver, &Record::Git(blob.clone()))
                .is_err()
        );
        assert_eq!(staging.streams.len(), 1);
        assert_eq!(fs::read_dir(staging.directory.path())?.count(), 1);
        staging.stage(
            &receiver,
            &Record::GitChunk {
                id: blob.id,
                offset: 0,
                data: blob.data,
            },
        )?;
        staging.stage(&receiver, &Record::GitEnd(blob.id))?;
        assert_eq!(staging.reserved_bytes, cost);
        assert!(staging.streams.is_empty());
        assert!(
            staging
                .stage(&receiver, &other)
                .unwrap_err()
                .to_string()
                .contains("sync.max-quarantine-bytes")
        );
        assert_eq!(fs::read_dir(staging.directory.path())?.count(), 2);
        Ok(())
    }

    #[test]
    fn quarantine_validation_bounds_known_object_closure_and_pending_dependencies() -> TestResult {
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let first = destination.object(gix::objs::Kind::Blob, b"a".to_vec(), None);
        let second = destination.object(gix::objs::Kind::Blob, b"b".to_vec(), None);
        let mut bytes = b"100644 a\0".to_vec();
        bytes.extend(first.id.as_bytes());
        bytes.extend(b"100644 b\0");
        bytes.extend(second.id.as_bytes());
        let tree = destination.object(gix::objs::Kind::Tree, bytes, None);
        let mut limits = tiny_limits(1024 * 1024);
        limits.max_records = 1;
        let staging = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            limits.clone(),
        )?;
        let receiver = destination.objects();
        assert!(
            staging
                .validate_with_git_roots(&receiver, &[], &[], &[tree.id])
                .unwrap_err()
                .to_string()
                .contains("sync.max-records")
        );
        limits.max_records = 10;
        limits.max_dependencies = 1;
        let staging = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            limits,
        )?;
        assert!(
            staging
                .validate_with_git_roots(&receiver, &[], &[], &[tree.id])
                .unwrap_err()
                .to_string()
                .contains("sync.max-dependencies")
        );
        assert!(
            receiver
                .inventory_with_git_roots(&[], &[], &[tree.id])
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn metadata_collection_limits_precede_all_semantic_decoders() -> TestResult {
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let repeated_heads = [0x0a, 0].repeat(16);
        let repeated_parents = [0x12, 0].repeat(16);
        let shared = SharedEvent::new(
            Vec::new(),
            SharedView::from_view(&op_store::View::make_root(
                destination.backend.root_commit_id().clone(),
            )),
            None,
        );
        let encoded = AnchorStore::encode(&shared)?;
        let prefix = encoded.len() - AnchorStore::encoded_view_bytes(&encoded)?.len() - 4;
        let mut shared_bytes = encoded[..prefix].to_vec();
        shared_bytes.extend_from_slice(&(repeated_heads.len() as u32).to_le_bytes());
        shared_bytes.extend_from_slice(&repeated_heads);
        let id = gix::ObjectId::from_bytes_or_panic(&[1; 20]);
        let records = [
            Record::Operation(OperationId::new(vec![1; 64]), repeated_parents.clone()),
            Record::View(ViewId::new(vec![1; 64]), repeated_heads),
            Record::SharedEvent(shared.id(), shared_bytes),
            Record::Git(GitObjectRecord {
                id,
                kind: gix::objs::Kind::Commit,
                data: Vec::new(),
                commit_extras: Some(repeated_parents.clone()),
            }),
            Record::GitHeader(GitObjectHeader {
                id,
                kind: gix::objs::Kind::Commit,
                size: 0,
                commit_extras: Some(repeated_parents),
            }),
        ];
        let receiver = destination.objects();
        for record in records {
            let mut limits = tiny_limits(1024 * 1024);
            limits.max_dependencies = 4;
            let mut staging = Quarantine::with_limits(
                destination.directory.path(),
                destination.backend.root_commit_id().clone(),
                limits,
            )?;
            assert!(
                staging
                    .stage(&receiver, &record)
                    .unwrap_err()
                    .to_string()
                    .contains("sync.max-dependencies")
            );
            assert_eq!(fs::read_dir(staging.directory.path())?.count(), 0);
        }
        Ok(())
    }

    #[test]
    fn inline_git_header_collections_are_bounded_without_scanning_the_message() -> TestResult {
        let source = Fixture::new(gix::hash::Kind::Sha1);
        let destination = Fixture::new(gix::hash::Kind::Sha1);
        let tree = source.object(gix::objs::Kind::Tree, Vec::new(), None);
        let ordinary = source.commit(tree.id, &[], &[], "message\nwith\nmany\nlines\n");
        let ordinary = Record::Git(ordinary);
        let Record::Git(mut amplified) = ordinary.clone() else {
            unreachable!()
        };
        let separator = amplified
            .data
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .unwrap();
        amplified.data.splice(
            separator + 1..separator + 1,
            b"x \nx \nx \nx \nx \n".iter().copied(),
        );
        amplified.id = gix::objs::compute_hash(
            gix::hash::Kind::Sha1,
            gix::objs::Kind::Commit,
            &amplified.data,
        )
        .unwrap();
        let mut limits = tiny_limits(1024 * 1024);
        limits.max_dependencies = 4;
        let mut staging = Quarantine::with_limits(
            destination.directory.path(),
            destination.backend.root_commit_id().clone(),
            limits,
        )?;
        let receiver = destination.objects();
        let error = staging
            .stage(&receiver, &Record::Git(amplified))
            .unwrap_err();
        assert!(error.to_string().contains("Git commit headers"));
        assert_eq!(fs::read_dir(staging.directory.path())?.count(), 0);
        staging.stage(&receiver, &ordinary)?;
        assert_eq!(staging.staged.len(), 1);
        Ok(())
    }
}
