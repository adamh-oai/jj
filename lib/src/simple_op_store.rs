// Copyright 2020 The Jujutsu Authors
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

#![expect(missing_docs)]

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::fs;
use std::io;
use std::io::ErrorKind;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use async_trait::async_trait;
use itertools::Itertools as _;
use pollster::FutureExt as _;
use prost::Message as _;
use smallvec::SmallVec;
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::backend::BackendInitError;
use crate::backend::CommitId;
use crate::backend::MillisSinceEpoch;
use crate::backend::Timestamp;
use crate::content_hash::blake2b_hash;
use crate::dag_walk_async;
use crate::file_util::IoResultExt as _;
use crate::file_util::PathError;
use crate::file_util::persist_content_addressed_temp_file;
use crate::object_id::HexPrefix;
use crate::object_id::ObjectId;
use crate::object_id::PrefixResolution;
use crate::op_store;
use crate::op_store::OpStore;
use crate::op_store::OpStoreError;
use crate::op_store::OpStoreResult;
use crate::op_store::Operation;
use crate::op_store::OperationId;
use crate::op_store::OperationMetadata;
use crate::op_store::RefTarget;
use crate::op_store::RemoteRef;
use crate::op_store::RemoteRefState;
use crate::op_store::RemoteView;
use crate::op_store::RootOperationData;
use crate::op_store::TimestampRange;
use crate::op_store::View;
use crate::op_store::ViewId;
use crate::ref_name::GitRefNameBuf;
use crate::ref_name::RefNameBuf;
use crate::ref_name::RemoteNameBuf;
use crate::ref_name::WorkspaceName;
use crate::ref_name::WorkspaceNameBuf;

// BLAKE2b-512 hash length in bytes
const OPERATION_ID_LENGTH: usize = 64;
const VIEW_ID_LENGTH: usize = 64;

/// Error that may occur during [`SimpleOpStore`] initialization.
#[derive(Debug, Error)]
#[error("Failed to initialize simple operation store")]
pub struct SimpleOpStoreInitError(#[from] pub PathError);

impl From<SimpleOpStoreInitError> for BackendInitError {
    fn from(err: SimpleOpStoreInitError) -> Self {
        Self(err.into())
    }
}

#[derive(Debug)]
pub struct SimpleOpStore {
    path: PathBuf,
    name: &'static str,
    root_data: RootOperationData,
    root_operation_id: OperationId,
    root_view_id: ViewId,
}

impl SimpleOpStore {
    pub const NAME: &str = "simple_op_store";
    /// Identifies repositories whose operation writes participate in sync.
    pub const SYNC_NAME: &str = "simple_op_store_sync_v1";

    /// Creates an empty OpStore. Returns error if it already exists.
    pub fn init(
        store_path: &Path,
        root_data: RootOperationData,
    ) -> Result<Self, SimpleOpStoreInitError> {
        let store = Self::new(store_path, root_data);
        store.init_base_dirs()?;
        Ok(store)
    }

    /// Load an existing OpStore
    pub fn load(store_path: &Path, root_data: RootOperationData) -> Self {
        Self::new(store_path, root_data)
    }

    /// Loads a repository that has explicitly enabled operation synchronization.
    pub fn load_sync(store_path: &Path, root_data: RootOperationData) -> Self {
        Self {
            name: Self::SYNC_NAME,
            ..Self::new(store_path, root_data)
        }
    }

    fn new(store_path: &Path, root_data: RootOperationData) -> Self {
        Self {
            path: store_path.to_path_buf(),
            name: Self::NAME,
            root_data,
            root_operation_id: OperationId::from_bytes(&[0; OPERATION_ID_LENGTH]),
            root_view_id: ViewId::from_bytes(&[0; VIEW_ID_LENGTH]),
        }
    }

    fn init_base_dirs(&self) -> Result<(), PathError> {
        for dir in [self.views_dir(), self.operations_dir()] {
            fs::create_dir(&dir).context(&dir)?;
        }
        Ok(())
    }

    fn views_dir(&self) -> PathBuf {
        self.path.join("views")
    }

    fn operations_dir(&self) -> PathBuf {
        self.path.join("operations")
    }

    /// Exports the stored bytes without re-encoding the operation or changing its ID.
    /// Synthetic root objects have no stored representation and must be skipped.
    pub fn export_operation(&self, id: &OperationId) -> OpStoreResult<Vec<u8>> {
        self.check_raw_id(id, OPERATION_ID_LENGTH)?;
        let path = self.operations_dir().join(id.hex());
        let bytes = fs::read(&path)
            .context(&path)
            .map_err(|err| io_to_read_error(err, id))?;
        self.decode_raw_operation(id, &bytes)?;
        Ok(bytes)
    }

    /// Validates and imports original operation bytes under their original ID.
    /// This does not publish an operation head or import referenced objects.
    ///
    /// In addition to current hashes, this authenticates historical layouts
    /// without workspace metadata or predecessor records. Unknown historical
    /// hash layouts are rejected rather than accepted under an unverified ID.
    pub fn import_operation(&self, id: &OperationId, bytes: &[u8]) -> OpStoreResult<Operation> {
        self.check_raw_id(id, OPERATION_ID_LENGTH)?;
        import_raw_object(&self.operations_dir(), id, bytes, |bytes| {
            self.decode_raw_operation(id, bytes)
        })
    }

    /// Exports the stored bytes without applying view migrations to those bytes.
    /// Synthetic root objects have no stored representation and must be skipped.
    pub fn export_view(&self, id: &ViewId) -> OpStoreResult<Vec<u8>> {
        self.check_raw_id(id, VIEW_ID_LENGTH)?;
        let path = self.views_dir().join(id.hex());
        let bytes = fs::read(&path)
            .context(&path)
            .map_err(|err| io_to_read_error(err, id))?;
        self.decode_raw_view(id, &bytes)?;
        Ok(bytes)
    }

    /// Validates and imports original view bytes, returning the migrated view.
    /// The stored bytes and ID remain those of the original, unmigrated object.
    ///
    /// Supported historical hashes include single Git HEAD targets and remote
    /// views without tags. Earlier layouts, such as views with the removed
    /// public-heads field, are rejected if their identity cannot be verified.
    pub fn import_view(&self, id: &ViewId, bytes: &[u8]) -> OpStoreResult<View> {
        self.check_raw_id(id, VIEW_ID_LENGTH)?;
        import_raw_object(&self.views_dir(), id, bytes, |bytes| {
            self.decode_raw_view(id, bytes)
        })
    }

    fn check_raw_id(&self, id: &impl ObjectId, length: usize) -> OpStoreResult<()> {
        if id.as_bytes().len() != length {
            return Err(to_read_error(
                PostDecodeError::InvalidHashLength {
                    expected: length,
                    actual: id.as_bytes().len(),
                }
                .into(),
                id,
            ));
        }
        if id.as_bytes().iter().all(|&byte| byte == 0) {
            return Err(to_read_error(RawObjectError::SyntheticRoot.into(), id));
        }
        Ok(())
    }

    pub(crate) fn decode_raw_operation(
        &self,
        id: &OperationId,
        bytes: &[u8],
    ) -> OpStoreResult<Operation> {
        self.check_raw_id(id, OPERATION_ID_LENGTH)?;
        let proto = crate::protos::simple_op_store::Operation::decode(bytes)
            .map_err(|err| to_read_error(err.into(), id))?;
        let mut operation =
            operation_from_proto(proto).map_err(|err| to_read_error(err.into(), id))?;
        if let Some(predecessors) = &operation.commit_predecessors {
            for (commit, previous) in predecessors {
                for commit in std::iter::once(commit).chain(previous) {
                    self.validate_commit_id(commit)
                        .map_err(|err| to_read_error(err.into(), id))?;
                }
            }
        }

        // Verify before inserting the synthetic root parent. Older operations
        // were named using their original empty parent list.
        let hash = blake2b_hash(&operation);
        let matches = hash.as_slice() == id.as_bytes()
            || (operation.metadata.workspace_name.is_none()
                && (blake2b_hash(&LegacyOperationHash {
                    operation: &operation,
                    include_predecessors: true,
                })
                .as_slice()
                    == id.as_bytes()
                    || (operation.commit_predecessors.is_none()
                        && blake2b_hash(&LegacyOperationHash {
                            operation: &operation,
                            include_predecessors: false,
                        })
                        .as_slice()
                            == id.as_bytes())));
        if !matches {
            return Err(to_read_error(RawObjectError::HashMismatch.into(), id));
        }
        if operation.parents.is_empty() {
            operation.parents.push(self.root_operation_id.clone());
        }
        Ok(operation)
    }

    pub(crate) fn decode_raw_view(&self, id: &ViewId, bytes: &[u8]) -> OpStoreResult<View> {
        self.check_raw_id(id, VIEW_ID_LENGTH)?;
        let proto = crate::protos::simple_op_store::View::decode(bytes)
            .map_err(|err| to_read_error(err.into(), id))?;
        let has_legacy_git_head = proto.git_heads.is_empty();
        // Git-tracking tag migration adds data to the in-memory view. Its ID
        // belongs to the original data, not to the result of that migration.
        let original = view_from_proto_with_migration(proto.clone(), false)
            .map_err(|err| to_read_error(err.into(), id))?;
        self.validate_view_commit_ids(&original)
            .map_err(|err| to_read_error(err.into(), id))?;
        let has_legacy_remote_views = proto.remote_views.is_empty()
            && original
                .remote_views
                .values()
                .all(|remote| remote.tags.is_empty());
        let matches = blake2b_hash(&original).as_slice() == id.as_bytes()
            || (has_legacy_git_head
                && (blake2b_hash(&LegacyViewHash {
                    view: &original,
                    include_remote_tags: true,
                })
                .as_slice()
                    == id.as_bytes()
                    || (has_legacy_remote_views
                        && blake2b_hash(&LegacyViewHash {
                            view: &original,
                            include_remote_tags: false,
                        })
                        .as_slice()
                            == id.as_bytes())));
        if !matches {
            return Err(to_read_error(RawObjectError::HashMismatch.into(), id));
        }
        view_from_proto(proto).map_err(|err| to_read_error(err.into(), id))
    }

    fn validate_commit_id(&self, id: &CommitId) -> Result<(), PostDecodeError> {
        let expected = self.root_data.root_commit_id.as_bytes().len();
        let actual = id.as_bytes().len();
        if actual == expected {
            Ok(())
        } else {
            Err(PostDecodeError::InvalidHashLength { expected, actual })
        }
    }

    fn validate_view_commit_ids(&self, view: &View) -> Result<(), PostDecodeError> {
        let targets = view
            .local_bookmarks
            .values()
            .chain(view.local_tags.values())
            .chain(view.git_refs.values())
            .chain(view.git_heads.values())
            .chain(view.remote_views.values().flat_map(|remote| {
                remote
                    .bookmarks
                    .values()
                    .chain(remote.tags.values())
                    .map(|r| &r.target)
            }));
        for id in view
            .head_ids
            .iter()
            .chain(view.wc_commit_ids.values())
            .chain(targets.flat_map(|target| target.iter().flatten()))
        {
            self.validate_commit_id(id)?;
        }
        Ok(())
    }
}

#[async_trait]
impl OpStore for SimpleOpStore {
    fn name(&self) -> &str {
        self.name
    }

    fn root_operation_id(&self) -> &OperationId {
        &self.root_operation_id
    }

    async fn read_view(&self, id: &ViewId) -> OpStoreResult<View> {
        if *id == self.root_view_id {
            return Ok(View::make_root(self.root_data.root_commit_id.clone()));
        }

        let path = self.views_dir().join(id.hex());
        let buf = fs::read(&path)
            .context(&path)
            .map_err(|err| io_to_read_error(err, id))?;

        decode_view(&buf).map_err(|err| to_read_error(err, id))
    }

    async fn write_view(&self, view: &View) -> OpStoreResult<ViewId> {
        let dir = self.views_dir();
        let temp_file = NamedTempFile::new_in(&dir)
            .context(&dir)
            .map_err(|err| io_to_write_error(err, "view"))?;

        temp_file
            .as_file()
            .write_all(&encode_view(view))
            .context(temp_file.path())
            .map_err(|err| io_to_write_error(err, "view"))?;

        let id = ViewId::new(blake2b_hash(view).to_vec());

        let new_path = dir.join(id.hex());
        persist_content_addressed_temp_file(temp_file, &new_path)
            .context(&new_path)
            .map_err(|err| io_to_write_error(err, "view"))?;
        Ok(id)
    }

    async fn read_operation(&self, id: &OperationId) -> OpStoreResult<Operation> {
        if *id == self.root_operation_id {
            return Ok(Operation::make_root(self.root_view_id.clone()));
        }

        let path = self.operations_dir().join(id.hex());
        let buf = fs::read(&path)
            .context(&path)
            .map_err(|err| io_to_read_error(err, id))?;

        let proto = crate::protos::simple_op_store::Operation::decode(&*buf)
            .map_err(|err| to_read_error(err.into(), id))?;
        let mut operation =
            operation_from_proto(proto).map_err(|err| to_read_error(err.into(), id))?;
        if operation.parents.is_empty() {
            // Repos created before we had the root operation will have an operation without
            // parents.
            operation.parents.push(self.root_operation_id.clone());
        }
        Ok(operation)
    }

    async fn write_operation(&self, operation: &Operation) -> OpStoreResult<OperationId> {
        assert!(!operation.parents.is_empty());
        let dir = self.operations_dir();
        let temp_file = NamedTempFile::new_in(&dir)
            .context(&dir)
            .map_err(|err| io_to_write_error(err, "operation"))?;

        let proto = operation_to_proto(operation);
        temp_file
            .as_file()
            .write_all(&proto.encode_to_vec())
            .context(temp_file.path())
            .map_err(|err| io_to_write_error(err, "operation"))?;

        let id = OperationId::new(blake2b_hash(operation).to_vec());

        let new_path = dir.join(id.hex());
        persist_content_addressed_temp_file(temp_file, &new_path)
            .context(&new_path)
            .map_err(|err| io_to_write_error(err, "operation"))?;
        Ok(id)
    }

    async fn resolve_operation_id_prefix(
        &self,
        prefix: &HexPrefix,
    ) -> OpStoreResult<PrefixResolution<OperationId>> {
        let op_dir = self.operations_dir();
        let find = || -> io::Result<_> {
            let matches_root = prefix.matches(&self.root_operation_id);
            let hex_prefix = prefix.hex();
            if hex_prefix.len() == OPERATION_ID_LENGTH * 2 {
                // Fast path for full-length ID
                if matches_root || op_dir.join(hex_prefix).try_exists()? {
                    let id = OperationId::from_bytes(prefix.as_full_bytes().unwrap());
                    return Ok(PrefixResolution::SingleMatch(id));
                } else {
                    return Ok(PrefixResolution::NoMatch);
                }
            }

            let mut matched = matches_root.then(|| self.root_operation_id.clone());
            for entry in op_dir.read_dir()? {
                let Ok(name) = entry?.file_name().into_string() else {
                    continue; // Skip invalid UTF-8
                };
                if !name.starts_with(&hex_prefix) {
                    continue;
                }
                let Some(id) = OperationId::try_from_hex(&name) else {
                    continue; // Skip invalid hex
                };
                if matched.is_some() {
                    return Ok(PrefixResolution::AmbiguousMatch);
                }
                matched = Some(id);
            }
            if let Some(id) = matched {
                Ok(PrefixResolution::SingleMatch(id))
            } else {
                Ok(PrefixResolution::NoMatch)
            }
        };
        find()
            .context(&op_dir)
            .map_err(|err| OpStoreError::Other(err.into()))
    }

    #[tracing::instrument(skip(self))]
    async fn gc(&self, head_ids: &[OperationId], keep_newer: SystemTime) -> OpStoreResult<()> {
        let to_op_id = |entry: &fs::DirEntry| -> Option<OperationId> {
            let name = entry.file_name().into_string().ok()?;
            OperationId::try_from_hex(name)
        };
        let to_view_id = |entry: &fs::DirEntry| -> Option<ViewId> {
            let name = entry.file_name().into_string().ok()?;
            ViewId::try_from_hex(name)
        };
        let remove_file_if_not_new = |entry: &fs::DirEntry| -> Result<(), PathError> {
            let path = entry.path();
            // Check timestamp, but there's still TOCTOU problem if an existing
            // file is renewed.
            let metadata = entry.metadata().context(&path)?;
            let mtime = metadata.modified().expect("unsupported platform?");
            if mtime > keep_newer {
                tracing::trace!(?path, "not removing");
                Ok(())
            } else {
                tracing::trace!(?path, "removing");
                fs::remove_file(&path).context(&path)
            }
        };

        // Reachable objects are resolved without considering the keep_newer
        // parameter. We could collect ancestors of the "new" operations here,
        // but more files can be added anyway after that.
        let read_op = |id: &OperationId| {
            self.read_operation(id)
                .block_on()
                .map(|data| (id.clone(), data))
        };
        let reachable_ops: HashMap<OperationId, Operation> = dag_walk_async::dfs(
            head_ids.iter().map(read_op),
            |(id, _)| id.clone(),
            |(_, data)| data.parents.iter().map(read_op).collect_vec(),
        )
        .try_collect()?;
        let reachable_views: HashSet<&ViewId> =
            reachable_ops.values().map(|data| &data.view_id).collect();
        tracing::info!(
            reachable_op_count = reachable_ops.len(),
            reachable_view_count = reachable_views.len(),
            "collected reachable objects"
        );

        let prune_ops = || -> Result<(), PathError> {
            let op_dir = self.operations_dir();
            for entry in op_dir.read_dir().context(&op_dir)? {
                let entry = entry.context(&op_dir)?;
                let Some(id) = to_op_id(&entry) else {
                    tracing::trace!(?entry, "skipping invalid file name");
                    continue;
                };
                if reachable_ops.contains_key(&id) {
                    continue;
                }
                // If the operation was added after collecting reachable_views,
                // its view mtime would also be renewed. So there's no need to
                // update the reachable_views set to preserve the view.
                remove_file_if_not_new(&entry)?;
            }
            Ok(())
        };
        prune_ops().map_err(|err| OpStoreError::Other(err.into()))?;

        let prune_views = || -> Result<(), PathError> {
            let view_dir = self.views_dir();
            for entry in view_dir.read_dir().context(&view_dir)? {
                let entry = entry.context(&view_dir)?;
                let Some(id) = to_view_id(&entry) else {
                    tracing::trace!(?entry, "skipping invalid file name");
                    continue;
                };
                if reachable_views.contains(&id) {
                    continue;
                }
                remove_file_if_not_new(&entry)?;
            }
            Ok(())
        };
        prune_views().map_err(|err| OpStoreError::Other(err.into()))?;

        Ok(())
    }
}

fn io_to_read_error(err: PathError, id: &impl ObjectId) -> OpStoreError {
    if err.source.kind() == ErrorKind::NotFound {
        OpStoreError::ObjectNotFound {
            object_type: id.object_type(),
            hash: id.hex(),
            source: Some(Box::new(err)),
        }
    } else {
        to_read_error(err.into(), id)
    }
}

fn to_read_error(
    source: Box<dyn std::error::Error + Send + Sync>,
    id: &impl ObjectId,
) -> OpStoreError {
    OpStoreError::ReadObject {
        object_type: id.object_type(),
        hash: id.hex(),
        source,
    }
}

fn io_to_write_error(err: PathError, object_type: &'static str) -> OpStoreError {
    OpStoreError::WriteObject {
        object_type,
        source: Box::new(err),
    }
}

#[derive(Debug, Error)]
enum RawObjectError {
    #[error("Synthetic root objects have no stored representation")]
    SyntheticRoot,
    #[error(
        "Content hash does not match the object ID; unsupported historical hash layouts cannot be \
         imported"
    )]
    HashMismatch,
    #[error("Existing object with this ID has different content")]
    ExistingObjectMismatch,
}

fn import_raw_object<T: PartialEq>(
    dir: &Path,
    id: &impl ObjectId,
    bytes: &[u8],
    decode: impl Fn(&[u8]) -> OpStoreResult<T>,
) -> OpStoreResult<T> {
    let object = decode(bytes)?;
    let temp_file = NamedTempFile::new_in(dir)
        .context(dir)
        .map_err(|err| io_to_write_error(err, "raw operation-store object"))?;
    temp_file
        .as_file()
        .write_all(bytes)
        .context(temp_file.path())
        .map_err(|err| io_to_write_error(err, "raw operation-store object"))?;
    temp_file
        .as_file()
        .sync_data()
        .context(temp_file.path())
        .map_err(|err| io_to_write_error(err, "raw operation-store object"))?;
    let path = dir.join(id.hex());
    match temp_file.persist_noclobber(&path) {
        Ok(_) => Ok(object),
        Err(err) if err.error.kind() == ErrorKind::AlreadyExists => {
            // Different protobuf encodings can name the same semantic object.
            // Keep the existing bytes, but verify them rather than trusting the
            // filename. This also handles racing importers without overwrites.
            let existing_bytes = fs::read(&path)
                .context(&path)
                .map_err(|err| io_to_read_error(err, id))?;
            let existing = decode(&existing_bytes)?;
            if existing == object {
                Ok(object)
            } else {
                Err(to_read_error(
                    RawObjectError::ExistingObjectMismatch.into(),
                    id,
                ))
            }
        }
        Err(err) => Err(io_to_write_error(
            PathError {
                path,
                source: err.error,
            },
            "raw operation-store object",
        )),
    }
}

/// Encodes a view using the operation store's current protobuf representation.
pub(crate) fn encode_view(view: &View) -> Vec<u8> {
    view_to_proto(view).encode_to_vec()
}

/// Decodes a view using the same compatibility migrations as repository reads.
pub(crate) fn decode_view(bytes: &[u8]) -> Result<View, Box<dyn std::error::Error + Send + Sync>> {
    let proto = crate::protos::simple_op_store::View::decode(bytes)?;
    Ok(view_from_proto(proto)?)
}

// These layouts are retained only to authenticate objects that can exist in
// pre-sync repositories. The protobuf bytes are never rewritten to these forms.
struct LegacyOperationHash<'a> {
    operation: &'a Operation,
    include_predecessors: bool,
}

impl crate::content_hash::ContentHash for LegacyOperationHash<'_> {
    fn hash(&self, state: &mut impl crate::content_hash::DigestUpdate) {
        let operation = self.operation;
        operation.view_id.hash(state);
        operation.parents.hash(state);
        // Before workspace_name was recorded, metadata's tags (now attributes)
        // immediately followed is_snapshot. TimestampRange is hash-transparent.
        operation.metadata.time.hash(state);
        operation.metadata.description.hash(state);
        operation.metadata.hostname.hash(state);
        operation.metadata.username.hash(state);
        operation.metadata.is_snapshot.hash(state);
        operation.metadata.attributes.hash(state);
        if self.include_predecessors {
            operation.commit_predecessors.hash(state);
        }
    }
}

struct LegacyViewHash<'a> {
    view: &'a View,
    include_remote_tags: bool,
}

impl crate::content_hash::ContentHash for LegacyViewHash<'_> {
    fn hash(&self, state: &mut impl crate::content_hash::DigestUpdate) {
        let view = self.view;
        view.head_ids.hash(state);
        view.local_bookmarks.hash(state);
        view.local_tags.hash(state);
        // Before jj 0.34, each remote view contained bookmarks only.
        (view.remote_views.len() as u64).hash(state);
        for (name, remote) in &view.remote_views {
            name.hash(state);
            remote.bookmarks.hash(state);
            if self.include_remote_tags {
                remote.tags.hash(state);
            }
        }
        view.git_refs.hash(state);
        view.git_heads
            .get(WorkspaceName::DEFAULT)
            .unwrap_or(&op_store::ABSENT_REF_TARGET)
            .hash(state);
        view.wc_commit_ids.hash(state);
    }
}

#[derive(Debug, Error)]
enum PostDecodeError {
    #[error("Invalid hash length (expected {expected} bytes, got {actual} bytes)")]
    InvalidHashLength { expected: usize, actual: usize },
    #[error("Invalid remote ref state value {0}")]
    InvalidRemoteRefStateValue(i32),
    #[error("Invalid number of ref target terms {0}")]
    EvenNumberOfRefTargetTerms(usize),
    #[error("Ref target has no value")]
    MissingRefTargetValue,
    #[error("Invalid ref conflict: {removes} removes and {adds} adds")]
    InvalidRefConflict { removes: usize, adds: usize },
    #[error("Invalid Git-tracking tag migration data")]
    InvalidGitTagMigration,
}

fn operation_id_from_proto(bytes: Vec<u8>) -> Result<OperationId, PostDecodeError> {
    if bytes.len() != OPERATION_ID_LENGTH {
        Err(PostDecodeError::InvalidHashLength {
            expected: OPERATION_ID_LENGTH,
            actual: bytes.len(),
        })
    } else {
        Ok(OperationId::new(bytes))
    }
}

fn view_id_from_proto(bytes: Vec<u8>) -> Result<ViewId, PostDecodeError> {
    if bytes.len() != VIEW_ID_LENGTH {
        Err(PostDecodeError::InvalidHashLength {
            expected: VIEW_ID_LENGTH,
            actual: bytes.len(),
        })
    } else {
        Ok(ViewId::new(bytes))
    }
}

fn timestamp_to_proto(timestamp: &Timestamp) -> crate::protos::simple_op_store::Timestamp {
    crate::protos::simple_op_store::Timestamp {
        millis_since_epoch: timestamp.timestamp.0,
        tz_offset: timestamp.tz_offset,
    }
}

fn timestamp_from_proto(proto: crate::protos::simple_op_store::Timestamp) -> Timestamp {
    Timestamp {
        timestamp: MillisSinceEpoch(proto.millis_since_epoch),
        tz_offset: proto.tz_offset,
    }
}

fn operation_metadata_to_proto(
    metadata: &OperationMetadata,
) -> crate::protos::simple_op_store::OperationMetadata {
    crate::protos::simple_op_store::OperationMetadata {
        start_time: Some(timestamp_to_proto(&metadata.time.start)),
        end_time: Some(timestamp_to_proto(&metadata.time.end)),
        description: metadata.description.clone(),
        hostname: metadata.hostname.clone(),
        username: metadata.username.clone(),
        is_snapshot: metadata.is_snapshot,
        workspace_name: metadata.workspace_name.clone().map(Into::into),
        attributes: metadata
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

fn operation_metadata_from_proto(
    proto: crate::protos::simple_op_store::OperationMetadata,
) -> OperationMetadata {
    let time = TimestampRange {
        start: timestamp_from_proto(proto.start_time.unwrap_or_default()),
        end: timestamp_from_proto(proto.end_time.unwrap_or_default()),
    };
    let workspace_name = proto.workspace_name.map(Into::into);
    OperationMetadata {
        time,
        description: proto.description,
        hostname: proto.hostname,
        username: proto.username,
        is_snapshot: proto.is_snapshot,
        workspace_name,
        attributes: proto.attributes.into_iter().collect(),
    }
}

fn commit_predecessors_map_to_proto(
    map: &BTreeMap<CommitId, Vec<CommitId>>,
) -> Vec<crate::protos::simple_op_store::CommitPredecessors> {
    map.iter()
        .map(
            |(commit_id, predecessor_ids)| crate::protos::simple_op_store::CommitPredecessors {
                commit_id: commit_id.to_bytes(),
                predecessor_ids: predecessor_ids.iter().map(|id| id.to_bytes()).collect(),
            },
        )
        .collect()
}

fn commit_predecessors_map_from_proto(
    proto: Vec<crate::protos::simple_op_store::CommitPredecessors>,
) -> BTreeMap<CommitId, Vec<CommitId>> {
    proto
        .into_iter()
        .map(|entry| {
            let commit_id = CommitId::new(entry.commit_id);
            let predecessor_ids = entry
                .predecessor_ids
                .into_iter()
                .map(CommitId::new)
                .collect();
            (commit_id, predecessor_ids)
        })
        .collect()
}

fn operation_to_proto(operation: &Operation) -> crate::protos::simple_op_store::Operation {
    let (commit_predecessors, stores_commit_predecessors) = match &operation.commit_predecessors {
        Some(map) => (commit_predecessors_map_to_proto(map), true),
        None => (vec![], false),
    };
    let parents = operation.parents.iter().map(|id| id.to_bytes()).collect();
    crate::protos::simple_op_store::Operation {
        view_id: operation.view_id.as_bytes().to_vec(),
        parents,
        metadata: Some(operation_metadata_to_proto(&operation.metadata)),
        commit_predecessors,
        stores_commit_predecessors,
    }
}

fn operation_from_proto(
    proto: crate::protos::simple_op_store::Operation,
) -> Result<Operation, PostDecodeError> {
    let parents = proto
        .parents
        .into_iter()
        .map(operation_id_from_proto)
        .try_collect()?;
    let view_id = view_id_from_proto(proto.view_id)?;
    let metadata = operation_metadata_from_proto(proto.metadata.unwrap_or_default());
    let commit_predecessors = proto
        .stores_commit_predecessors
        .then(|| commit_predecessors_map_from_proto(proto.commit_predecessors));
    Ok(Operation {
        view_id,
        parents,
        metadata,
        commit_predecessors,
    })
}

fn view_to_proto(view: &View) -> crate::protos::simple_op_store::View {
    let wc_commit_ids = view
        .wc_commit_ids
        .iter()
        .map(|(name, id)| (name.into(), id.to_bytes()))
        .collect();
    // Sort head ids so they serialize in a stable order.
    let head_ids = view
        .head_ids
        .iter()
        .sorted()
        .map(|id| id.to_bytes())
        .collect();

    let bookmarks = bookmark_views_to_proto_legacy(&view.local_bookmarks, &view.remote_views);

    let local_tags = view
        .local_tags
        .iter()
        .map(|(name, target)| crate::protos::simple_op_store::Tag {
            name: name.into(),
            target: ref_target_to_proto(target),
        })
        .collect();

    let remote_views = remote_views_to_proto(&view.remote_views);

    let git_refs = view
        .git_refs
        .iter()
        .map(|(name, target)| {
            #[expect(deprecated)]
            crate::protos::simple_op_store::GitRef {
                name: name.into(),
                commit_id: Default::default(),
                target: ref_target_to_proto(target),
            }
        })
        .collect();

    let git_heads = view
        .git_heads
        .iter()
        .map(|(name, target)| crate::protos::simple_op_store::GitHead {
            name: name.as_str().to_owned(),
            target: ref_target_to_proto(target),
        })
        .collect();

    #[expect(deprecated)]
    crate::protos::simple_op_store::View {
        head_ids,
        wc_commit_id: Default::default(),
        wc_commit_ids,
        bookmarks,
        local_tags,
        remote_views,
        git_refs,
        git_head_legacy: Default::default(),
        // TODO: Remove in jj 0.51+
        git_head: view
            .git_heads
            .get(WorkspaceName::DEFAULT)
            .and_then(ref_target_to_proto),
        // New/loaded view should have been migrated to the latest format
        has_git_refs_migrated_to_remote_tags: true,
        git_heads,
    }
}

fn view_from_proto(proto: crate::protos::simple_op_store::View) -> Result<View, PostDecodeError> {
    view_from_proto_with_migration(proto, true)
}

fn view_from_proto_with_migration(
    proto: crate::protos::simple_op_store::View,
    migrate: bool,
) -> Result<View, PostDecodeError> {
    #[cfg(not(feature = "git"))]
    let _ = migrate;
    validate_view_proto(&proto)?;
    // TODO: validate commit id length?
    // For compatibility with old repos before we had support for multiple working
    // copies
    let mut wc_commit_ids = BTreeMap::new();
    #[expect(deprecated)]
    if !proto.wc_commit_id.is_empty() {
        wc_commit_ids.insert(
            WorkspaceName::DEFAULT.to_owned(),
            CommitId::new(proto.wc_commit_id),
        );
    }
    for (name, commit_id) in proto.wc_commit_ids {
        wc_commit_ids.insert(WorkspaceNameBuf::from(name), CommitId::new(commit_id));
    }
    let head_ids = proto.head_ids.into_iter().map(CommitId::new).collect();

    let (local_bookmarks, mut remote_views) = bookmark_views_from_proto_legacy(proto.bookmarks)?;

    let local_tags = proto
        .local_tags
        .into_iter()
        .map(|tag_proto| {
            let name: RefNameBuf = tag_proto.name.into();
            (name, ref_target_from_proto(tag_proto.target))
        })
        .collect();

    let git_refs: BTreeMap<_, _> = proto
        .git_refs
        .into_iter()
        .map(|git_ref| {
            let name: GitRefNameBuf = git_ref.name.into();
            let target = if git_ref.target.is_some() {
                ref_target_from_proto(git_ref.target)
            } else {
                // Legacy format
                #[expect(deprecated)]
                RefTarget::normal(CommitId::new(git_ref.commit_id))
            };
            (name, target)
        })
        .collect();

    // Use legacy remote_views only when new data isn't available (jj < 0.34)
    if !proto.remote_views.is_empty() {
        remote_views = remote_views_from_proto(proto.remote_views)?;
    }

    #[cfg(feature = "git")]
    if migrate && !proto.has_git_refs_migrated_to_remote_tags {
        tracing::info!("migrating Git-tracking tags");
        let git_tags: BTreeMap<_, _> = git_refs
            .iter()
            .filter_map(|(full_name, target)| {
                let name = full_name.as_str().strip_prefix("refs/tags/")?;
                if name.is_empty() {
                    return Some(Err(PostDecodeError::InvalidGitTagMigration));
                }
                let name: RefNameBuf = name.into();
                let remote_ref = RemoteRef {
                    target: target.clone(),
                    state: RemoteRefState::Tracked,
                };
                Some(Ok((name, remote_ref)))
            })
            .try_collect()?;
        if !git_tags.is_empty() {
            let git_view = remote_views
                .entry(crate::git::REMOTE_NAME_FOR_LOCAL_GIT_REPO.to_owned())
                .or_default();
            if !git_view.tags.is_empty() {
                return Err(PostDecodeError::InvalidGitTagMigration);
            }
            git_view.tags = git_tags;
        }
    }

    let mut git_heads: BTreeMap<WorkspaceNameBuf, RefTarget> = proto
        .git_heads
        .into_iter()
        .map(|entry| {
            (
                WorkspaceNameBuf::from(entry.name),
                ref_target_from_proto(entry.target),
            )
        })
        .collect();
    #[expect(deprecated)]
    if git_heads.is_empty() {
        let git_head = if proto.git_head.is_some() {
            ref_target_from_proto(proto.git_head)
        } else if !proto.git_head_legacy.is_empty() {
            RefTarget::normal(CommitId::new(proto.git_head_legacy))
        } else {
            RefTarget::absent()
        };
        if git_head.is_present() {
            git_heads.insert(WorkspaceName::DEFAULT.to_owned(), git_head);
        }
    }

    Ok(View {
        head_ids,
        local_bookmarks,
        local_tags,
        remote_views,
        git_refs,
        git_heads,
        wc_commit_ids,
    })
}

fn validate_view_proto(
    proto: &crate::protos::simple_op_store::View,
) -> Result<(), PostDecodeError> {
    let targets = proto
        .bookmarks
        .iter()
        .flat_map(|bookmark| {
            #[expect(deprecated)]
            bookmark.local_target.iter().chain(
                bookmark
                    .remote_bookmarks
                    .iter()
                    .flat_map(|remote| &remote.target),
            )
        })
        .chain(proto.local_tags.iter().flat_map(|tag| &tag.target))
        .chain(proto.git_refs.iter().flat_map(|git_ref| &git_ref.target))
        .chain(proto.git_heads.iter().flat_map(|head| &head.target))
        .chain({
            #[expect(deprecated)]
            &proto.git_head
        });
    for target in targets {
        match &target.value {
            None => return Err(PostDecodeError::MissingRefTargetValue),
            Some(crate::protos::simple_op_store::ref_target::Value::Conflict(conflict))
                if conflict.adds.len() != conflict.removes.len() + 1 =>
            {
                return Err(PostDecodeError::InvalidRefConflict {
                    removes: conflict.removes.len(),
                    adds: conflict.adds.len(),
                });
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn bookmark_views_to_proto_legacy(
    local_bookmarks: &BTreeMap<RefNameBuf, RefTarget>,
    remote_views: &BTreeMap<RemoteNameBuf, RemoteView>,
) -> Vec<crate::protos::simple_op_store::Bookmark> {
    op_store::merge_join_ref_views(local_bookmarks, remote_views, |view| &view.bookmarks)
        .map(|(name, bookmark_target)| {
            let local_target = ref_target_to_proto(bookmark_target.local_target);
            // TODO: Drop serialization to the old format in jj 0.40 or so.
            let remote_bookmarks = bookmark_target
                .remote_refs
                .iter()
                .map(
                    |&(remote_name, remote_ref)| crate::protos::simple_op_store::RemoteBookmark {
                        remote_name: remote_name.into(),
                        target: ref_target_to_proto(&remote_ref.target),
                        state: Some(remote_ref_state_to_proto(remote_ref.state)),
                    },
                )
                .collect();
            #[expect(deprecated)]
            crate::protos::simple_op_store::Bookmark {
                name: name.into(),
                local_target,
                remote_bookmarks,
            }
        })
        .collect()
}

type BookmarkViews = (
    BTreeMap<RefNameBuf, RefTarget>,
    BTreeMap<RemoteNameBuf, RemoteView>,
);

fn bookmark_views_from_proto_legacy(
    bookmarks_legacy: Vec<crate::protos::simple_op_store::Bookmark>,
) -> Result<BookmarkViews, PostDecodeError> {
    let mut local_bookmarks: BTreeMap<RefNameBuf, RefTarget> = BTreeMap::new();
    let mut remote_views: BTreeMap<RemoteNameBuf, RemoteView> = BTreeMap::new();
    for bookmark_proto in bookmarks_legacy {
        let bookmark_name: RefNameBuf = bookmark_proto.name.into();
        let local_target = ref_target_from_proto(bookmark_proto.local_target);
        #[expect(deprecated)]
        let remote_bookmarks = bookmark_proto.remote_bookmarks;
        for remote_bookmark in remote_bookmarks {
            let remote_name: RemoteNameBuf = remote_bookmark.remote_name.into();
            let state = match remote_bookmark.state {
                Some(n) => remote_ref_state_from_proto(n)?,
                // Legacy view saved by jj < 0.11. The proto field is not
                // changed to non-optional type because that would break forward
                // compatibility. Zero may be omitted if the field is optional.
                None => RemoteRefState::New,
            };
            let remote_view = remote_views.entry(remote_name).or_default();
            let remote_ref = RemoteRef {
                target: ref_target_from_proto(remote_bookmark.target),
                state,
            };
            remote_view
                .bookmarks
                .insert(bookmark_name.clone(), remote_ref);
        }
        if local_target.is_present() {
            local_bookmarks.insert(bookmark_name, local_target);
        }
    }
    Ok((local_bookmarks, remote_views))
}

fn remote_views_to_proto(
    remote_views: &BTreeMap<RemoteNameBuf, RemoteView>,
) -> Vec<crate::protos::simple_op_store::RemoteView> {
    remote_views
        .iter()
        .map(|(name, view)| crate::protos::simple_op_store::RemoteView {
            name: name.into(),
            bookmarks: remote_refs_to_proto(&view.bookmarks),
            tags: remote_refs_to_proto(&view.tags),
        })
        .collect()
}

fn remote_views_from_proto(
    remote_views_proto: Vec<crate::protos::simple_op_store::RemoteView>,
) -> Result<BTreeMap<RemoteNameBuf, RemoteView>, PostDecodeError> {
    remote_views_proto
        .into_iter()
        .map(|proto| {
            let name: RemoteNameBuf = proto.name.into();
            let view = RemoteView {
                bookmarks: remote_refs_from_proto(proto.bookmarks)?,
                tags: remote_refs_from_proto(proto.tags)?,
            };
            Ok((name, view))
        })
        .collect()
}

fn remote_refs_to_proto(
    remote_refs: &BTreeMap<RefNameBuf, RemoteRef>,
) -> Vec<crate::protos::simple_op_store::RemoteRef> {
    remote_refs
        .iter()
        .map(
            |(name, remote_ref)| crate::protos::simple_op_store::RemoteRef {
                name: name.into(),
                target_terms: ref_target_to_terms_proto(&remote_ref.target),
                state: remote_ref_state_to_proto(remote_ref.state),
            },
        )
        .collect()
}

fn remote_refs_from_proto(
    remote_refs_proto: Vec<crate::protos::simple_op_store::RemoteRef>,
) -> Result<BTreeMap<RefNameBuf, RemoteRef>, PostDecodeError> {
    remote_refs_proto
        .into_iter()
        .map(|proto| {
            let name: RefNameBuf = proto.name.into();
            let remote_ref = RemoteRef {
                target: ref_target_from_terms_proto(proto.target_terms)?,
                state: remote_ref_state_from_proto(proto.state)?,
            };
            Ok((name, remote_ref))
        })
        .collect()
}

fn ref_target_to_terms_proto(
    value: &RefTarget,
) -> Vec<crate::protos::simple_op_store::RefTargetTerm> {
    value
        .iter()
        .map(|term| term.as_ref().map(|id| id.to_bytes()))
        .map(|value| crate::protos::simple_op_store::RefTargetTerm { value })
        .collect()
}

fn ref_target_from_terms_proto(
    proto: Vec<crate::protos::simple_op_store::RefTargetTerm>,
) -> Result<RefTarget, PostDecodeError> {
    let terms: SmallVec<[_; 1]> = proto
        .into_iter()
        .map(|crate::protos::simple_op_store::RefTargetTerm { value }| value.map(CommitId::new))
        .collect();
    if terms.len().is_multiple_of(2) {
        Err(PostDecodeError::EvenNumberOfRefTargetTerms(terms.len()))
    } else {
        Ok(RefTarget::from_vec(terms))
    }
}

fn ref_target_to_proto(value: &RefTarget) -> Option<crate::protos::simple_op_store::RefTarget> {
    let term_to_proto =
        |term: &Option<CommitId>| crate::protos::simple_op_store::ref_conflict::Term {
            value: term.as_ref().map(|id| id.to_bytes()),
        };
    let conflict_proto = crate::protos::simple_op_store::RefConflict {
        removes: value.removes().map(term_to_proto).collect(),
        adds: value.adds().map(term_to_proto).collect(),
    };
    let proto = crate::protos::simple_op_store::RefTarget {
        value: Some(crate::protos::simple_op_store::ref_target::Value::Conflict(
            conflict_proto,
        )),
    };
    Some(proto)
}

#[expect(deprecated)]
#[cfg(test)]
fn ref_target_to_proto_legacy(
    value: &RefTarget,
) -> Option<crate::protos::simple_op_store::RefTarget> {
    if let Some(id) = value.as_normal() {
        let proto = crate::protos::simple_op_store::RefTarget {
            value: Some(crate::protos::simple_op_store::ref_target::Value::CommitId(
                id.to_bytes(),
            )),
        };
        Some(proto)
    } else if !value.is_resolved() {
        let ref_conflict_proto = crate::protos::simple_op_store::RefConflictLegacy {
            removes: value.present_removes().map(|id| id.to_bytes()).collect(),
            adds: value.present_adds().map(|id| id.to_bytes()).collect(),
        };
        let proto = crate::protos::simple_op_store::RefTarget {
            value: Some(
                crate::protos::simple_op_store::ref_target::Value::ConflictLegacy(
                    ref_conflict_proto,
                ),
            ),
        };
        Some(proto)
    } else {
        assert!(value.is_absent());
        None
    }
}

fn ref_target_from_proto(
    maybe_proto: Option<crate::protos::simple_op_store::RefTarget>,
) -> RefTarget {
    // TODO: Delete legacy format handling when we decide to drop support for views
    // saved by jj <= 0.8.
    let Some(proto) = maybe_proto else {
        // Legacy absent id
        return RefTarget::absent();
    };
    match proto.value.unwrap() {
        #[expect(deprecated)]
        crate::protos::simple_op_store::ref_target::Value::CommitId(id) => {
            // Legacy non-conflicting id
            RefTarget::normal(CommitId::new(id))
        }
        #[expect(deprecated)]
        crate::protos::simple_op_store::ref_target::Value::ConflictLegacy(conflict) => {
            // Legacy conflicting ids
            let removes = conflict.removes.into_iter().map(CommitId::new);
            let adds = conflict.adds.into_iter().map(CommitId::new);
            RefTarget::from_legacy_form(removes, adds)
        }
        crate::protos::simple_op_store::ref_target::Value::Conflict(conflict) => {
            let term_from_proto = |term: crate::protos::simple_op_store::ref_conflict::Term| {
                term.value.map(CommitId::new)
            };
            let removes = conflict.removes.into_iter().map(term_from_proto);
            let adds = conflict.adds.into_iter().map(term_from_proto);
            RefTarget::from_removes_adds(removes, adds)
        }
    }
}

fn remote_ref_state_to_proto(state: RemoteRefState) -> i32 {
    let proto_state = match state {
        RemoteRefState::New => crate::protos::simple_op_store::RemoteRefState::New,
        RemoteRefState::Tracked => crate::protos::simple_op_store::RemoteRefState::Tracked,
    };
    proto_state as i32
}

fn remote_ref_state_from_proto(proto_value: i32) -> Result<RemoteRefState, PostDecodeError> {
    let proto_state = proto_value
        .try_into()
        .map_err(|prost::UnknownEnumValue(n)| PostDecodeError::InvalidRemoteRefStateValue(n))?;
    let state = match proto_state {
        crate::protos::simple_op_store::RemoteRefState::New => RemoteRefState::New,
        crate::protos::simple_op_store::RemoteRefState::Tracked => RemoteRefState::Tracked,
    };
    Ok(state)
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use maplit::btreemap;
    use maplit::hashset;

    use super::*;
    use crate::hex_util;
    use crate::ref_name::GitRefName;
    use crate::ref_name::RefName;
    use crate::ref_name::RemoteName;
    use crate::tests::TestResult;
    use crate::tests::new_temp_dir;

    fn create_view() -> View {
        let new_remote_ref = |target: &RefTarget| RemoteRef {
            target: target.clone(),
            state: RemoteRefState::New,
        };
        let tracked_remote_ref = |target: &RefTarget| RemoteRef {
            target: target.clone(),
            state: RemoteRefState::Tracked,
        };
        let head_id1 = CommitId::from_hex("aaa111");
        let head_id2 = CommitId::from_hex("aaa222");
        let bookmark_main_local_target = RefTarget::normal(CommitId::from_hex("ccc111"));
        let bookmark_main_origin_target = RefTarget::normal(CommitId::from_hex("ccc222"));
        let bookmark_deleted_origin_target = RefTarget::normal(CommitId::from_hex("ccc333"));
        let tag_v1_local_target = RefTarget::normal(CommitId::from_hex("ddd111"));
        let tag_v1_origin_target = RefTarget::normal(CommitId::from_hex("ddd222"));
        let tag_deleted_origin_target = RefTarget::normal(CommitId::from_hex("ddd333"));
        let git_refs_main_target = RefTarget::normal(CommitId::from_hex("fff111"));
        let git_refs_feature_target = RefTarget::from_legacy_form(
            [CommitId::from_hex("fff111")],
            [CommitId::from_hex("fff222"), CommitId::from_hex("fff333")],
        );
        let default_wc_commit_id = CommitId::from_hex("abc111");
        let test_wc_commit_id = CommitId::from_hex("abc222");
        View {
            head_ids: hashset! {head_id1, head_id2},
            local_bookmarks: btreemap! {
                "main".into() => bookmark_main_local_target,
            },
            local_tags: btreemap! {
                "v1.0".into() => tag_v1_local_target,
            },
            remote_views: btreemap! {
                "origin".into() => RemoteView {
                    bookmarks: btreemap! {
                        "main".into() => tracked_remote_ref(&bookmark_main_origin_target),
                        "deleted".into() => new_remote_ref(&bookmark_deleted_origin_target),
                    },
                    tags: btreemap! {
                        "v1.0".into() => tracked_remote_ref(&tag_v1_origin_target),
                        "deleted".into() => new_remote_ref(&tag_deleted_origin_target),
                    },
                },
            },
            git_refs: btreemap! {
                "refs/heads/main".into() => git_refs_main_target,
                "refs/heads/feature".into() => git_refs_feature_target,
            },
            git_heads: btreemap! {
                WorkspaceName::DEFAULT.to_owned() => RefTarget::normal(CommitId::from_hex("fff111")),
            },
            wc_commit_ids: btreemap! {
                WorkspaceName::DEFAULT.to_owned() => default_wc_commit_id,
                "test".into() => test_wc_commit_id,
            },
        }
    }

    fn create_operation() -> Operation {
        let pad_id_bytes = |hex: &str, len: usize| {
            let mut bytes = hex_util::decode_hex(hex).unwrap();
            bytes.resize(len, b'\0');
            bytes
        };
        Operation {
            view_id: ViewId::new(pad_id_bytes("aaa111", VIEW_ID_LENGTH)),
            parents: vec![
                OperationId::new(pad_id_bytes("bbb111", OPERATION_ID_LENGTH)),
                OperationId::new(pad_id_bytes("bbb222", OPERATION_ID_LENGTH)),
            ],
            metadata: OperationMetadata {
                time: TimestampRange {
                    start: Timestamp {
                        timestamp: MillisSinceEpoch(123456789),
                        tz_offset: 3600,
                    },
                    end: Timestamp {
                        timestamp: MillisSinceEpoch(123456800),
                        tz_offset: 3600,
                    },
                },
                description: "check out foo".to_string(),
                hostname: "some.host.example.com".to_string(),
                username: "someone".to_string(),
                is_snapshot: false,
                workspace_name: Some(WorkspaceNameBuf::from("test")),
                attributes: btreemap! {
                    "key1".to_string() => "value1".to_string(),
                    "key2".to_string() => "value2".to_string(),
                },
            },
            commit_predecessors: Some(btreemap! {
                CommitId::from_hex("111111") => vec![],
                CommitId::from_hex("222222") => vec![
                    CommitId::from_hex("333333"),
                    CommitId::from_hex("444444"),
                ],
            }),
        }
    }

    #[test]
    fn test_hash_view() {
        // Test exact output so we detect regressions in compatibility
        assert_snapshot!(
            ViewId::new(blake2b_hash(&create_view()).to_vec()).hex(),
            @"b37a61a743f394241cd44e9016cc6f9b68321d7ae0e21e432d9124d08a1b5f98f08f6a1d04534ea0652ea4be74e9c4fb2075bdf00343165b5d380aa196790a14"
        );
    }

    #[test]
    fn test_hash_operation() {
        // Test exact output so we detect regressions in compatibility
        assert_snapshot!(
            OperationId::new(blake2b_hash(&create_operation()).to_vec()).hex(),
            @"f5963c593a63bb852061a86ad919c12c6ba1940eeef30a832524c39ccea6a9f768aa2aa53becec34d379eb291ec6726837c4113857849cb9dcc62dbe0a517176"
        );
    }

    #[test]
    fn test_read_write_view() -> TestResult {
        let temp_dir = new_temp_dir();
        let root_data = RootOperationData {
            root_commit_id: CommitId::from_hex("000000"),
        };
        let store = SimpleOpStore::init(temp_dir.path(), root_data)?;
        let view = create_view();
        let view_id = store.write_view(&view).block_on()?;
        let read_view = store.read_view(&view_id).block_on()?;
        assert_eq!(read_view, view);
        Ok(())
    }

    #[test]
    fn test_read_write_operation() -> TestResult {
        let temp_dir = new_temp_dir();
        let root_data = RootOperationData {
            root_commit_id: CommitId::from_hex("000000"),
        };
        let store = SimpleOpStore::init(temp_dir.path(), root_data)?;
        let operation = create_operation();
        let op_id = store.write_operation(&operation).block_on()?;
        let read_operation = store.read_operation(&op_id).block_on()?;
        assert_eq!(read_operation, operation);
        Ok(())
    }

    fn create_store(path: &Path) -> SimpleOpStore {
        SimpleOpStore::init(
            path,
            RootOperationData {
                root_commit_id: CommitId::from_hex("000000"),
            },
        )
        .unwrap()
    }

    #[test]
    fn test_raw_objects_preserve_bytes_and_ids() -> TestResult {
        let source_dir = new_temp_dir();
        let destination_dir = new_temp_dir();
        let source = create_store(source_dir.path());
        let destination = create_store(destination_dir.path());
        let view = create_view();
        let view_id = source.write_view(&view).block_on()?;
        let view_bytes = source.export_view(&view_id)?;
        assert_eq!(destination.import_view(&view_id, &view_bytes)?, view);
        assert_eq!(destination.export_view(&view_id)?, view_bytes);
        assert_eq!(destination.import_view(&view_id, &view_bytes)?, view);

        let mut operation = create_operation();
        operation.view_id = view_id;
        let operation_id = source.write_operation(&operation).block_on()?;
        let operation_bytes = source.export_operation(&operation_id)?;
        assert_eq!(
            destination.import_operation(&operation_id, &operation_bytes)?,
            operation
        );
        assert_eq!(
            destination.export_operation(&operation_id)?,
            operation_bytes
        );
        assert_eq!(
            destination.import_operation(&operation_id, &operation_bytes)?,
            operation
        );
        assert_eq!(
            destination.read_operation(&operation_id).block_on()?,
            operation
        );
        Ok(())
    }

    #[test]
    fn test_raw_import_keeps_existing_equivalent_encoding() -> TestResult {
        let dir = new_temp_dir();
        let store = create_store(dir.path());
        let view = create_view();
        let view_id = store.write_view(&view).block_on()?;
        let original = store.export_view(&view_id)?;
        let mut alternative = original.clone();
        // A valid unknown protobuf field does not affect the semantic hash.
        alternative.extend([0xf8, 0x7f, 1]);
        assert_eq!(store.import_view(&view_id, &alternative)?, view);
        assert_eq!(store.export_view(&view_id)?, original);
        Ok(())
    }

    #[test]
    fn test_raw_import_validates_before_writing() -> TestResult {
        let dir = new_temp_dir();
        let store = create_store(dir.path());
        let operation = create_operation();
        let operation_id = OperationId::new(blake2b_hash(&operation).to_vec());
        let mut changed = operation.clone();
        changed.metadata.description.push_str(" tampered");
        let wrong_bytes = operation_to_proto(&changed).encode_to_vec();
        assert!(store.import_operation(&operation_id, &wrong_bytes).is_err());
        assert!(!store.operations_dir().join(operation_id.hex()).exists());
        assert!(store.import_operation(&operation_id, &[0xff]).is_err());
        assert!(
            store
                .import_operation(store.root_operation_id(), &[])
                .is_err()
        );
        assert!(store.import_view(&store.root_view_id, &[]).is_err());
        assert!(store.export_operation(store.root_operation_id()).is_err());
        assert!(store.export_view(&store.root_view_id).is_err());
        assert!(
            store
                .import_operation(&OperationId::from_hex("ab"), &wrong_bytes)
                .is_err()
        );

        let mut view = create_view();
        let view_id = ViewId::new(blake2b_hash(&view).to_vec());
        view.head_ids.insert(CommitId::from_hex("123456"));
        assert!(store.import_view(&view_id, &encode_view(&view)).is_err());
        assert!(!store.views_dir().join(view_id.hex()).exists());

        view.head_ids.insert(CommitId::from_hex("12"));
        let view_id = ViewId::new(blake2b_hash(&view).to_vec());
        assert!(store.import_view(&view_id, &encode_view(&view)).is_err());
        assert!(!store.views_dir().join(view_id.hex()).exists());

        let mut proto = operation_to_proto(&operation);
        proto.parents = vec![vec![1]];
        assert!(
            store
                .import_operation(&operation_id, &proto.encode_to_vec())
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn test_raw_parentless_operation_keeps_original_identity() -> TestResult {
        let dir = new_temp_dir();
        let store = create_store(dir.path());
        let mut operation = create_operation();
        operation.parents.clear();
        let operation_id = OperationId::new(blake2b_hash(&operation).to_vec());
        let original = operation_to_proto(&operation).encode_to_vec();
        let imported = store.import_operation(&operation_id, &original)?;
        assert_eq!(imported.parents, vec![store.root_operation_id.clone()]);
        assert_ne!(blake2b_hash(&imported).as_slice(), operation_id.as_bytes());
        assert_eq!(store.export_operation(&operation_id)?, original);
        assert_eq!(store.read_operation(&operation_id).block_on()?, imported);
        Ok(())
    }

    #[test]
    fn test_raw_legacy_operation_metadata_and_predecessors() -> TestResult {
        let dir = new_temp_dir();
        let store = create_store(dir.path());
        let mut operation = create_operation();
        operation.metadata.workspace_name = None;
        for include_predecessors in [false, true] {
            operation.commit_predecessors = create_operation().commit_predecessors;
            if !include_predecessors {
                operation.commit_predecessors = None;
            }
            let operation_id = OperationId::new(
                blake2b_hash(&LegacyOperationHash {
                    operation: &operation,
                    include_predecessors,
                })
                .to_vec(),
            );
            let bytes = operation_to_proto(&operation).encode_to_vec();
            assert_eq!(store.import_operation(&operation_id, &bytes)?, operation);
            assert_eq!(store.export_operation(&operation_id)?, bytes);
        }
        // An old hash layout must not hide fields that have actually been set.
        operation.metadata.workspace_name = Some("new".into());
        let operation_id = OperationId::new(
            blake2b_hash(&LegacyOperationHash {
                operation: &operation,
                include_predecessors: true,
            })
            .to_vec(),
        );
        assert!(
            store
                .import_operation(
                    &operation_id,
                    &operation_to_proto(&operation).encode_to_vec()
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn test_raw_legacy_operation_golden_identity() -> TestResult {
        let dir = new_temp_dir();
        let store = create_store(dir.path());
        let mut operation = Operation::make_root(ViewId::new(vec![1; VIEW_ID_LENGTH]));
        operation.metadata.description = "legacy".to_owned();
        operation.commit_predecessors = None;
        // jj 0.29's ContentHash layout: no workspace_name or predecessors slot.
        let operation_id = OperationId::from_hex(
            "bf8e499fa491b457def3abf95722b582432f58bfa0caf0d0ad4ccf5dcad63f87d0b9f73af59f52e68fb9e30080cfee50d97661551c3b08ede146ba2041bfec7b",
        );
        let original = operation_to_proto(&operation).encode_to_vec();
        let imported = store.import_operation(&operation_id, &original)?;
        assert_eq!(imported.metadata, operation.metadata);
        assert_eq!(imported.parents, vec![store.root_operation_id.clone()]);
        assert_eq!(store.export_operation(&operation_id)?, original);
        Ok(())
    }

    #[test]
    fn test_raw_legacy_view_preserves_identity_across_migration() -> TestResult {
        let dir = new_temp_dir();
        let store = create_store(dir.path());
        let mut view = View::make_root(CommitId::from_hex("000000"));
        view.git_refs.insert(
            "refs/tags/v1".into(),
            RefTarget::normal(CommitId::from_hex("010203")),
        );
        let mut proto = view_to_proto(&view);
        proto.has_git_refs_migrated_to_remote_tags = false;
        proto.git_heads.clear();
        let original = proto.encode_to_vec();
        // jj 0.34's original single-Git-HEAD layout, before tag migration.
        let view_id = ViewId::from_hex(
            "c6d47179a4ae4d34cc7777d953389afedf1a40439baec8916f376efda538b1cbd5dd98b98fe0e3fe8dead39c4e69686d7b273ab5d5e445a5bd515e6a83400d09",
        );
        let migrated = store.import_view(&view_id, &original)?;
        if cfg!(feature = "git") {
            assert_eq!(
                migrated.remote_views[RemoteName::new("git")].tags[RefName::new("v1")].target,
                view.git_refs[GitRefName::new("refs/tags/v1")]
            );
        }
        assert_eq!(store.export_view(&view_id)?, original);
        assert_eq!(store.read_view(&view_id).block_on()?, migrated);
        Ok(())
    }

    #[test]
    #[expect(deprecated)]
    fn test_raw_legacy_view_encodings_and_remote_hash() -> TestResult {
        let dir = new_temp_dir();
        let store = create_store(dir.path());
        let target = RefTarget::normal(CommitId::from_hex("111111"));
        let proto = crate::protos::simple_op_store::View {
            head_ids: vec![CommitId::from_hex("000000").to_bytes()],
            wc_commit_id: CommitId::from_hex("333333").to_bytes(),
            git_head_legacy: CommitId::from_hex("222222").to_bytes(),
            bookmarks: vec![crate::protos::simple_op_store::Bookmark {
                name: "main".to_owned(),
                local_target: None,
                remote_bookmarks: vec![crate::protos::simple_op_store::RemoteBookmark {
                    remote_name: "origin".to_owned(),
                    target: ref_target_to_proto_legacy(&target),
                    state: None,
                }],
            }],
            ..Default::default()
        };
        // jj 0.29 hashed remote bookmarks without the later remote-tags slot.
        let view_id = ViewId::from_hex(
            "69fdd79e71d7b28eae6319e156224c2e570961b2a4afc4bb174aef9c4ecd6b65f4e78eeb30446079adc10b231746a82e33e0399cb6d8af906c85201ecfb68de8",
        );
        let bytes = proto.encode_to_vec();
        let imported = store.import_view(&view_id, &bytes)?;
        assert_eq!(
            imported.remote_views[RemoteName::new("origin")].bookmarks[RefName::new("main")].target,
            target
        );
        assert_eq!(
            imported.remote_views[RemoteName::new("origin")].bookmarks[RefName::new("main")].state,
            RemoteRefState::New
        );
        assert_eq!(
            imported.git_heads[WorkspaceName::DEFAULT],
            RefTarget::normal(CommitId::from_hex("222222"))
        );
        assert_eq!(
            imported.wc_commit_ids[WorkspaceName::DEFAULT],
            CommitId::from_hex("333333")
        );
        assert_eq!(store.export_view(&view_id)?, bytes);
        Ok(())
    }

    #[test]
    fn test_raw_export_detects_existing_corruption() -> TestResult {
        let dir = new_temp_dir();
        let store = create_store(dir.path());
        let view = create_view();
        let view_id = store.write_view(&view).block_on()?;
        let bytes = store.export_view(&view_id)?;
        fs::write(store.views_dir().join(view_id.hex()), [0xff])?;
        assert!(store.export_view(&view_id).is_err());
        assert!(store.import_view(&view_id, &bytes).is_err());
        assert_eq!(fs::read(store.views_dir().join(view_id.hex()))?, [0xff]);
        Ok(())
    }

    #[test]
    fn test_view_codec_rejects_malformed_ref_targets() {
        let mut proto = view_to_proto(&create_view());
        proto.local_tags.push(crate::protos::simple_op_store::Tag {
            name: "invalid".to_owned(),
            target: Some(crate::protos::simple_op_store::RefTarget { value: None }),
        });
        assert!(decode_view(&proto.encode_to_vec()).is_err());
        proto.local_tags.last_mut().unwrap().target =
            Some(crate::protos::simple_op_store::RefTarget {
                value: Some(crate::protos::simple_op_store::ref_target::Value::Conflict(
                    crate::protos::simple_op_store::RefConflict::default(),
                )),
            });
        assert!(decode_view(&proto.encode_to_vec()).is_err());
    }

    #[test]
    fn test_remote_views_legacy_roundtrip() {
        let mut view = create_view();
        assert!(!view.remote_views.is_empty());
        for remote_view in view.remote_views.values_mut() {
            // remote tags cannot be preserved in "legacy" format
            remote_view.tags.clear();
        }
        let mut proto = view_to_proto(&view);
        proto.remote_views.clear(); // drop "new" format
        let view_reconstructed = view_from_proto(proto).unwrap();
        assert_eq!(view.remote_views, view_reconstructed.remote_views);
    }

    #[test]
    fn test_remote_views_new_roundtrip() {
        let view = create_view();
        assert!(!view.remote_views.is_empty());
        let mut proto = view_to_proto(&view);
        for bookmark in &mut proto.bookmarks {
            #[expect(deprecated)]
            bookmark.remote_bookmarks.clear(); // drop "legacy" format
        }
        let view_reconstructed = view_from_proto(proto).unwrap();
        assert_eq!(view.remote_views, view_reconstructed.remote_views);
    }

    #[test]
    fn test_view_to_proto_sorted_head_ids() {
        let ids = (0..50).map(|i| CommitId::new(vec![i; 4])).collect_vec();
        let make_view = |order: &[CommitId]| {
            let mut view = create_view();
            view.head_ids = order.iter().cloned().collect();
            view
        };
        let proto1 = view_to_proto(&make_view(&ids));
        let proto2 = view_to_proto(&make_view(&ids.iter().rev().cloned().collect_vec()));
        assert!(proto1.head_ids.is_sorted());
        assert_eq!(proto1.head_ids, proto2.head_ids);
    }

    #[test]
    fn test_view_to_proto_deterministic_wc_commit_ids() {
        let entries = (0..50)
            .map(|i| {
                (
                    WorkspaceNameBuf::from(format!("ws{i}")),
                    CommitId::new(vec![i; 4]),
                )
            })
            .collect_vec();
        let make_view = |entries: Vec<_>| {
            let mut view = create_view();
            // Leave head_ids empty so the test only covers wc_commit_ids.
            view.head_ids.clear();
            view.wc_commit_ids = entries.into_iter().collect();
            view
        };
        let view1 = make_view(entries.clone());
        let view2 = make_view(entries.into_iter().rev().collect());
        assert_eq!(
            view_to_proto(&view1).encode_to_vec(),
            view_to_proto(&view2).encode_to_vec()
        );
    }

    #[test]
    fn test_operation_to_proto_deterministic_attributes() {
        let entries = (0..50)
            .map(|i| (format!("key{i}"), format!("value{i}")))
            .collect_vec();
        let make_operation = |entries: Vec<_>| {
            let mut operation = create_operation();
            operation.metadata.attributes = entries.into_iter().collect();
            operation
        };
        let operation1 = make_operation(entries.clone());
        let operation2 = make_operation(entries.into_iter().rev().collect());
        assert_eq!(
            operation_to_proto(&operation1).encode_to_vec(),
            operation_to_proto(&operation2).encode_to_vec()
        );
    }

    #[test]
    fn test_migrate_git_refs_to_remote_tags() {
        let tracked_remote_ref = |target: &RefTarget| RemoteRef {
            target: target.clone(),
            state: RemoteRefState::Tracked,
        };
        let git_ref_to_proto = |name: &str, ref_target| crate::protos::simple_op_store::GitRef {
            name: name.to_owned(),
            #[expect(deprecated)]
            commit_id: Default::default(),
            target: ref_target_to_proto(ref_target),
        };
        let v1_target = RefTarget::normal(CommitId::from_hex("111111"));
        let main_target = RefTarget::normal(CommitId::from_hex("222222"));
        let orig_remote_views = btreemap! {
            "git".into() => RemoteView {
                bookmarks: btreemap! {
                    "main".into() => tracked_remote_ref(&main_target),
                },
                tags: btreemap! {},
            },
        };
        let proto = crate::protos::simple_op_store::View {
            remote_views: remote_views_to_proto(&orig_remote_views),
            git_refs: vec![
                git_ref_to_proto("refs/tags/v1.0", &v1_target),
                git_ref_to_proto("refs/heads/main", &main_target),
            ],
            has_git_refs_migrated_to_remote_tags: false,
            ..Default::default()
        };

        let view = view_from_proto(proto).unwrap();
        if cfg!(feature = "git") {
            assert_eq!(
                view.remote_views,
                btreemap! {
                    "git".into() => RemoteView {
                        bookmarks: btreemap! {
                            "main".into() => tracked_remote_ref(&main_target),
                        },
                        tags: btreemap! {
                            "v1.0".into() => tracked_remote_ref(&v1_target),
                        },
                    },
                }
            );
        } else {
            assert_eq!(view.remote_views, orig_remote_views);
        }

        // Once migrated, "git" remote tags shouldn't be populated again.
        let mut proto = view_to_proto(&view);
        assert!(proto.has_git_refs_migrated_to_remote_tags);
        for view_proto in &mut proto.remote_views {
            view_proto.tags.clear();
        }
        let view = view_from_proto(proto).unwrap();
        assert_eq!(view.remote_views, orig_remote_views);
    }

    #[test]
    fn test_bookmark_views_legacy_roundtrip() {
        let new_remote_ref = |target: &RefTarget| RemoteRef {
            target: target.clone(),
            state: RemoteRefState::New,
        };
        let tracked_remote_ref = |target: &RefTarget| RemoteRef {
            target: target.clone(),
            state: RemoteRefState::Tracked,
        };
        let local_bookmark1_target = RefTarget::normal(CommitId::from_hex("111111"));
        let local_bookmark3_target = RefTarget::normal(CommitId::from_hex("222222"));
        let git_bookmark1_target = RefTarget::normal(CommitId::from_hex("333333"));
        let remote1_bookmark1_target = RefTarget::normal(CommitId::from_hex("444444"));
        let remote2_bookmark2_target = RefTarget::normal(CommitId::from_hex("555555"));
        let remote2_bookmark4_target = RefTarget::normal(CommitId::from_hex("666666"));
        let local_bookmarks = btreemap! {
            "bookmark1".into() => local_bookmark1_target.clone(),
            "bookmark3".into() => local_bookmark3_target.clone(),
        };
        let remote_views = btreemap! {
            "git".into() => RemoteView {
                bookmarks: btreemap! {
                    "bookmark1".into() => tracked_remote_ref(&git_bookmark1_target),
                },
                tags: btreemap! {},
            },
            "remote1".into() => RemoteView {
                bookmarks: btreemap! {
                    "bookmark1".into() => tracked_remote_ref(&remote1_bookmark1_target),
                },
                tags: btreemap! {},
            },
            "remote2".into() => RemoteView {
                bookmarks: btreemap! {
                    // "bookmark2" is non-tracking. "bookmark4" is tracking, but locally deleted.
                    "bookmark2".into() => new_remote_ref(&remote2_bookmark2_target),
                    "bookmark4".into() => tracked_remote_ref(&remote2_bookmark4_target),
                },
                tags: btreemap! {},
            },
        };

        let bookmarks_legacy = bookmark_views_to_proto_legacy(&local_bookmarks, &remote_views);
        assert_eq!(
            bookmarks_legacy
                .iter()
                .map(|proto| &proto.name)
                .sorted()
                .collect_vec(),
            vec!["bookmark1", "bookmark2", "bookmark3", "bookmark4"],
        );

        let (local_bookmarks_reconstructed, remote_views_reconstructed) =
            bookmark_views_from_proto_legacy(bookmarks_legacy).unwrap();
        assert_eq!(local_bookmarks_reconstructed, local_bookmarks);
        assert_eq!(remote_views_reconstructed, remote_views);
    }

    #[test]
    fn test_ref_target_change_delete_order_roundtrip() {
        let target = RefTarget::from_removes_adds(
            vec![Some(CommitId::from_hex("111111"))],
            vec![Some(CommitId::from_hex("222222")), None],
        );
        let maybe_proto = ref_target_to_proto(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);

        // If it were legacy format, order of None entry would be lost.
        let target = RefTarget::from_removes_adds(
            vec![Some(CommitId::from_hex("111111"))],
            vec![None, Some(CommitId::from_hex("222222"))],
        );
        let maybe_proto = ref_target_to_proto(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);
    }

    #[test]
    fn test_ref_target_legacy_roundtrip() {
        let target = RefTarget::absent();
        let maybe_proto = ref_target_to_proto_legacy(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);

        let target = RefTarget::normal(CommitId::from_hex("111111"));
        let maybe_proto = ref_target_to_proto_legacy(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);

        // N-way conflict
        let target = RefTarget::from_legacy_form(
            [CommitId::from_hex("111111"), CommitId::from_hex("222222")],
            [
                CommitId::from_hex("333333"),
                CommitId::from_hex("444444"),
                CommitId::from_hex("555555"),
            ],
        );
        let maybe_proto = ref_target_to_proto_legacy(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);

        // Change-delete conflict
        let target = RefTarget::from_legacy_form(
            [CommitId::from_hex("111111")],
            [CommitId::from_hex("222222")],
        );
        let maybe_proto = ref_target_to_proto_legacy(&target);
        assert_eq!(ref_target_from_proto(maybe_proto), target);
    }
}
