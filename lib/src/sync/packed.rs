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

//! Disposable native Git repositories for negotiated packed-object transfers.
//!
//! Only frozen synchronization refs are advertised. The live object database is
//! an alternate, so these repositories are not a confidentiality boundary from
//! an authenticated peer. Native Git verifies received packs before the typed
//! dependency validator inspects their headers and non-blob bodies.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io;
use std::io::BufRead as _;
use std::io::BufReader;
use std::io::Read;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use gix::objs::Exists as _;
use gix::objs::Write as _;
use tempfile::NamedTempFile;
use tempfile::TempDir;

use super::SyncError;
use super::SyncResult;
use super::objects::Inventory;
use super::objects::QuarantineLimits;
use super::objects::RecordId;
use crate::git_backend::GitBackend;
use crate::object_id::ObjectId as _;

const FILE_OVERHEAD: u64 = 4096;

/// A disposable bare repository borrowing the live Git object database.
///
/// Callers keep advertised source roots alive and hold the sync/GC lock while
/// promoting received objects and publishing their durable retention roots.
#[derive(Debug)]
pub struct GitPackRepository {
    directory: TempDir,
    repository: gix::ThreadSafeRepository,
    git_executable: OsString,
    backend_objects: PathBuf,
    empty_tree_file: PathBuf,
    empty_tree_bytes: Vec<u8>,
    validated: Mutex<Option<ValidatedFiles>>,
}

#[derive(Clone, Debug)]
struct ValidatedFiles {
    packs: Vec<(PathBuf, PathBuf)>,
    empty_tree: PathBuf,
    physical_bytes: u64,
    limits: QuarantineLimits,
}

impl GitPackRepository {
    /// Captures all physical export roots under `refs/jj-sync/export/`.
    pub fn sender(
        parent: &Path,
        backend: &GitBackend,
        inventory: &Inventory,
        limits: &QuarantineLimits,
    ) -> SyncResult<Self> {
        Self::create(parent, backend, inventory, limits, "export")
    }

    /// Captures the receiver's known roots under `refs/jj-sync/have/`.
    pub fn receiver(
        parent: &Path,
        backend: &GitBackend,
        inventory: &Inventory,
        limits: &QuarantineLimits,
    ) -> SyncResult<Self> {
        Self::create(parent, backend, inventory, limits, "have")
    }

    fn create(
        parent: &Path,
        backend: &GitBackend,
        inventory: &Inventory,
        limits: &QuarantineLimits,
        namespace: &str,
    ) -> SyncResult<Self> {
        if inventory.records.len() > limits.max_records {
            return Err(invalid("packed inventory exceeds the record limit"));
        }
        fs::create_dir_all(parent)?;
        limits.check_space(parent, FILE_OVERHEAD)?;
        let directory = tempfile::Builder::new()
            .prefix("jj-sync-pack-")
            .tempdir_in(parent)?;
        let object_hash = backend.git_repo().object_hash();
        let repository = gix::ThreadSafeRepository::init_opts(
            directory.path(),
            gix::create::Kind::Bare,
            gix::create::Options {
                object_hash: Some(object_hash),
                ..Default::default()
            },
            gix::open::Options::isolated(),
        )
        .map_err(|err| invalid(format!("failed to create packed Git quarantine: {err}")))?;
        let format_version = u8::from(object_hash == gix::hash::Kind::Sha256);
        let mut config = format!(
            "[core]
    repositoryFormatVersion = {format_version}
    bare = true
    hooksPath = hooks
    alternateRefsCommand = true --
[gc]
    auto = 0
[maintenance]
    auto = false
[receive]
    autoGC = false
    unpackLimit = 0
    fsckObjects = true
    maxInputSize = {}
[fetch]
    unpackLimit = 0
    fsckObjects = true
    writeCommitGraph = false
[transfer]
    unpackLimit = 0
    fsckObjects = true
",
            limits.max_staged_bytes.max(1),
        );
        if object_hash == gix::hash::Kind::Sha256 {
            config.push_str("[extensions]\n    objectFormat = sha256\n");
        }
        fs::write(directory.path().join("config"), config)?;
        fs::create_dir_all(directory.path().join("hooks"))?;

        // Materialize the virtual empty tree before installing the alternate.
        // It is the only loose object this module ever promotes.
        let empty_tree = gix::ObjectId::empty_tree(object_hash);
        repository
            .to_thread_local()
            .objects
            .write_buf_with_known_id(gix::objs::Kind::Tree, &[], empty_tree)
            .map_err(|err| invalid(format!("failed to materialize the empty tree: {err}")))?;
        drop(repository);
        let empty_tree_name = empty_tree.to_string();
        let empty_tree_file = directory
            .path()
            .join("objects")
            .join(&empty_tree_name[..2])
            .join(&empty_tree_name[2..]);
        let empty_tree_bytes = fs::read(&empty_tree_file)?;

        let live_repo = backend.git_repo();
        let objects = dunce::canonicalize(live_repo.objects.store_ref().path())?;
        let mut alternate = crate::file_util::path_to_bytes(&objects)
            .map_err(|_| invalid("Git alternate path cannot be encoded"))?
            .to_vec();
        // Git's alternates file accepts one path per line. Refuse ambiguous
        // names instead of accidentally borrowing a different object database.
        if alternate.contains(&b'\n') || alternate.contains(&b'\r') {
            return Err(invalid("Git alternate path contains a line break"));
        }
        alternate.push(b'\n');
        fs::create_dir_all(directory.path().join("objects/info"))?;
        fs::write(directory.path().join("objects/info/alternates"), alternate)?;
        let repository =
            gix::ThreadSafeRepository::open_opts(directory.path(), gix::open::Options::isolated())
                .map_err(|err| invalid(format!("failed to open packed Git quarantine: {err}")))?;
        let frozen = repository.to_thread_local();
        let mut roots = BTreeMap::new();
        for id in &inventory.git_commits {
            insert_root(
                &mut roots,
                id.as_bytes(),
                object_hash,
                Some(gix::objs::Kind::Commit),
            )?;
        }
        for id in &inventory.legacy_tree_roots {
            insert_root(
                &mut roots,
                id.as_bytes(),
                object_hash,
                Some(gix::objs::Kind::Tree),
            )?;
        }
        for id in &inventory.git_roots {
            insert_root(&mut roots, id.as_bytes(), object_hash, None)?;
        }
        if roots.len() > limits.max_records {
            return Err(invalid("packed roots exceed the record limit"));
        }
        // A ref per historical commit is useful for native negotiation, but a
        // loose file per ref would dominate scratch space on large histories.
        let header = b"# pack-refs with: sorted\n";
        let line_size =
            format!("{empty_tree} refs/jj-sync/{namespace}/{empty_tree}\n").len() as u64;
        let reference_bytes = (roots.len() as u64)
            .checked_mul(line_size)
            .and_then(|bytes| bytes.checked_add(header.len() as u64))
            .ok_or_else(|| invalid("packed refs exceed sync.max-quarantine-bytes"))?;
        add_budget(0, reference_bytes.saturating_add(FILE_OVERHEAD), limits)?;
        limits.check_space(
            directory.path(),
            reference_bytes.saturating_add(FILE_OVERHEAD),
        )?;
        let mut refs = io::BufWriter::new(File::create(directory.path().join("packed-refs"))?);
        refs.write_all(header)?;
        for (id, expected_kind) in roots {
            let header = frozen
                .try_find_header(id)
                .map_err(|err| invalid(format!("failed to inspect packed root {id}: {err}")))?
                .ok_or_else(|| invalid(format!("packed root {id} is missing")))?;
            if expected_kind.is_some_and(|kind| kind != header.kind()) {
                return Err(invalid(format!("packed root {id} has an unexpected type")));
            }
            writeln!(refs, "{id} refs/jj-sync/{namespace}/{id}")?;
        }
        refs.flush()?;
        scratch_bytes(directory.path(), limits)?;
        limits.check_space(directory.path(), 0)?;
        Ok(Self {
            directory,
            repository,
            git_executable: backend.git_executable_path().as_os_str().to_owned(),
            backend_objects: objects,
            empty_tree_file,
            empty_tree_bytes,
            validated: Mutex::new(None),
        })
    }

    /// The bare repository path passed to native upload-pack or receive-pack.
    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    /// Opens an object handle including both received packs and the live alternate.
    pub fn git_repo(&self) -> gix::Repository {
        let mut repo = self.repository.to_thread_local();
        repo.objects.ignore_replacements = true;
        repo
    }

    /// Creates one persistent native object stream for dependency validation.
    pub fn read_objects(&self) -> SyncResult<GitPackReader> {
        GitPackReader::new(&self.git_executable, self.path())
    }

    /// Checks private received objects against the offered dependency inventory.
    ///
    /// Native Git must have completed a transfer with `fsckObjects` enabled in
    /// this repository. That verifies physical object hashes without expanding
    /// blob bodies in Rust. This pass checks index checksums, pack identities,
    /// length arithmetic, and inventory membership; it never enumerates alternates.
    /// Repaired thin-pack bases already present in the live database are allowed.
    pub fn validate_received(
        &self,
        backend: &GitBackend,
        inventory: &Inventory,
        limits: &QuarantineLimits,
    ) -> SyncResult<()> {
        *self.validated.lock().unwrap() = None;
        if inventory.records.len() > limits.max_records {
            return Err(invalid("packed inventory exceeds the record limit"));
        }
        let object_hash = self.git_repo().object_hash();
        if object_hash != backend.git_repo().object_hash() {
            return Err(invalid(
                "packed object hash format does not match the backend",
            ));
        }
        self.check_backend(backend)?;
        let physical_bytes = scratch_bytes(self.path(), limits)?;
        let mut packs = BTreeMap::<gix::ObjectId, (Option<PathBuf>, Option<PathBuf>)>::new();
        for directory in fs::read_dir(self.path().join("objects"))? {
            let directory = directory?;
            if !directory.file_type()?.is_dir() {
                return Err(invalid("unexpected file in packed object directory"));
            }
            let name = directory.file_name();
            let Some(name) = name.to_str() else {
                return Err(invalid("invalid packed object directory name"));
            };
            for entry in fs::read_dir(directory.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    return Err(invalid("unexpected directory or link in packed objects"));
                }
                match name {
                    "info" => {
                        if entry.file_name() != "alternates" {
                            return Err(invalid("unexpected packed object information file"));
                        }
                    }
                    "pack" => {
                        let path = entry.path();
                        let stem = path
                            .file_stem()
                            .and_then(OsStr::to_str)
                            .and_then(|stem| stem.strip_prefix("pack-"))
                            .ok_or_else(|| invalid("invalid received pack filename"))?;
                        let id = gix::ObjectId::from_hex(stem.as_bytes())
                            .map_err(|_| invalid("invalid received pack identity"))?;
                        if id.kind() != object_hash {
                            return Err(invalid("received pack has an unexpected hash format"));
                        }
                        let pair = packs.entry(id).or_default();
                        match path.extension().and_then(OsStr::to_str) {
                            Some("pack") => pair.0 = Some(path),
                            Some("idx") => pair.1 = Some(path),
                            Some("keep" | "rev") => {}
                            _ => return Err(invalid("unexpected file in received pack directory")),
                        }
                    }
                    _ if entry.path() == self.empty_tree_file => {
                        if fs::read(entry.path())? != self.empty_tree_bytes {
                            return Err(invalid("the materialized empty tree was modified"));
                        }
                    }
                    _ => return Err(invalid("unexpected loose object in packed quarantine")),
                }
            }
        }
        limits.check_space(self.path(), 0)?;
        let repo = self.git_repo();
        let live = backend.git_repo();
        let mut seen = BTreeSet::new();
        let mut expanded_bytes = 0_u64;
        let mut validated_packs = Vec::new();
        for (id, (pack, index)) in packs {
            let (Some(pack), Some(index)) = (pack, index) else {
                return Err(invalid("received pack is missing its data or index file"));
            };
            let index_file = gix::odb::pack::index::File::at(&index, object_hash)
                .map_err(|err| invalid(format!("invalid received pack index: {err}")))?;
            index_file
                .verify_checksum(&mut gix::utils::progress::Discard, &AtomicBool::new(false))
                .map_err(|err| invalid(format!("received pack index checksum failed: {err}")))?;
            let pack_file = gix::odb::pack::data::File::at(&pack, object_hash)
                .map_err(|err| invalid(format!("invalid received pack: {err}")))?;
            if index_file.pack_checksum() != id
                || pack_file.checksum() != id
                || index_file.num_objects() != pack_file.num_objects()
            {
                return Err(invalid("received pack identity does not match its index"));
            }
            for entry in index_file.iter() {
                if !seen.insert(entry.oid) {
                    continue;
                }
                if seen.len() > limits.max_records {
                    return Err(invalid("received packs exceed the object-count limit"));
                }
                if !inventory.records.contains(&RecordId::Git(entry.oid))
                    && !live.objects.exists(&entry.oid)
                {
                    return Err(invalid(format!(
                        "received object {} is outside the offered inventory",
                        entry.oid
                    )));
                }
                let header = repo
                    .try_find_header(entry.oid)
                    .map_err(|err| invalid(format!("failed to inspect received object: {err}")))?
                    .ok_or_else(|| invalid("received indexed object is missing"))?;
                // Packed objects do not consume one staging file per body.
                // Native Git owns decoding resources; the byte allowance is
                // for physical pack/index files, not their expanded contents.
                expanded_bytes = expanded_bytes.checked_add(header.size()).ok_or_else(|| {
                    invalid("received object lengths overflow the Git size range")
                })?;
            }
            validated_packs.push((pack, index));
        }
        *self.validated.lock().unwrap() = Some(ValidatedFiles {
            packs: validated_packs,
            empty_tree: self.empty_tree_file.clone(),
            physical_bytes,
            limits: limits.clone(),
        });
        Ok(())
    }

    /// Installs validated packs, keeping them until the returned guard is dropped.
    ///
    /// The caller holds the sync/GC lock and persists commit/tree retention roots
    /// before dropping the guard. Refs, configuration, and auxiliary Git files
    /// are never copied from the disposable repository.
    pub fn promote(&self, backend: &GitBackend) -> SyncResult<GitPackPromotion> {
        self.check_backend(backend)?;
        let files = self
            .validated
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| invalid("received packs have not been validated"))?;
        let live_repo = backend.git_repo();
        let objects = live_repo.objects.store_ref().path();
        files.limits.check_space(objects, files.physical_bytes)?;
        let pack_directory = objects.join("pack");
        fs::create_dir_all(&pack_directory)?;
        let mut protection = GitPackPromotion {
            keep_files: Vec::new(),
        };
        for (pack, index) in files.packs {
            let target_pack = pack_directory.join(pack.file_name().unwrap());
            let target_index = pack_directory.join(index.file_name().unwrap());
            let keep = target_pack.with_extension("keep");
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&keep)
            {
                Ok(mut file) => {
                    protection.keep_files.push(keep);
                    file.write_all(b"jj sync native pack promotion\n")?;
                    file.sync_data()?;
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
            install_immutable_file(&pack, &target_pack)?;
            // Install the index last so readers never discover an incomplete pack.
            install_immutable_file(&index, &target_index)?;
        }
        let name = files.empty_tree.file_name().unwrap();
        let fanout = files.empty_tree.parent().unwrap().file_name().unwrap();
        let directory = objects.join(fanout);
        fs::create_dir_all(&directory)?;
        install_immutable_file(&files.empty_tree, &directory.join(name))?;
        #[cfg(unix)]
        {
            // Retention state may be persisted immediately after promotion.
            // Make its referenced directory entries durable before returning.
            File::open(&pack_directory)?.sync_all()?;
            File::open(&directory)?.sync_all()?;
            File::open(objects)?.sync_all()?;
        }
        Ok(protection)
    }

    fn check_backend(&self, backend: &GitBackend) -> SyncResult<()> {
        if dunce::canonicalize(backend.git_repo().objects.store_ref().path())?
            != self.backend_objects
        {
            return Err(invalid(
                "packed quarantine belongs to a different object database",
            ));
        }
        Ok(())
    }
}

/// Temporary GC protection for installed native Git packs.
///
/// Only `.keep` files created by this promotion are removed on drop. Existing
/// keep files belong to their original owners and remain untouched.
#[derive(Debug)]
pub struct GitPackPromotion {
    keep_files: Vec<PathBuf>,
}

impl Drop for GitPackPromotion {
    fn drop(&mut self) {
        for path in &self.keep_files {
            let _remove_result = fs::remove_file(path);
        }
    }
}

/// Header access and a persistent native `cat-file --batch` body stream.
#[derive(Debug)]
pub struct GitPackReader {
    repository: gix::Repository,
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    failed: bool,
}

impl GitPackReader {
    /// Uses the configured Git executable and the repository's actual Git path.
    pub fn new(git_executable: &OsStr, git_dir: &Path) -> SyncResult<Self> {
        let mut repository =
            gix::ThreadSafeRepository::open_opts(git_dir, gix::open::Options::isolated())
                .map_err(|err| invalid(format!("failed to open packed object reader: {err}")))?
                .to_thread_local();
        repository.objects.ignore_replacements = true;
        let mut command = git_command(git_executable, git_dir);
        command
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn()?;
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        Ok(Self {
            repository,
            child,
            input,
            output,
            failed: false,
        })
    }

    /// Returns object type and expanded size without inflating blob bodies.
    pub fn header(&self, id: &gix::oid) -> SyncResult<(gix::objs::Kind, u64)> {
        if id.kind() != self.repository.object_hash() || id.is_null() {
            return Err(invalid("invalid packed object identity"));
        }
        if id == gix::ObjectId::empty_tree(id.kind()).as_ref() {
            return Ok((gix::objs::Kind::Tree, 0));
        }
        let header = self
            .repository
            .try_find_header(id.to_owned())
            .map_err(|err| invalid(format!("failed to inspect packed object {id}: {err}")))?
            .ok_or_else(|| invalid(format!("packed object {id} is missing")))?;
        Ok((header.kind(), header.size()))
    }

    /// Streams an object to a bounded parser, then drains any unread body bytes.
    ///
    /// Dependency validators use this for commits, trees, and tags; blobs require
    /// only [`Self::header()`]. Draining also occurs when the parser returns an
    /// error, preserving framing for any later request.
    pub fn with_object<T>(
        &mut self,
        id: &gix::oid,
        callback: impl FnOnce(gix::objs::Kind, u64, &mut dyn Read) -> SyncResult<T>,
    ) -> SyncResult<T> {
        let (kind, size) = self.header(id)?;
        if id == gix::ObjectId::empty_tree(id.kind()).as_ref() {
            return callback(kind, size, &mut io::empty());
        }
        if self.failed {
            return Err(invalid("packed object reader lost stream framing"));
        }
        self.failed = true;
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{id}")?;
        input.flush()?;
        let mut line = Vec::new();
        self.output
            .by_ref()
            .take(256)
            .read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            return Err(invalid("invalid native Git object header"));
        }
        let line = std::str::from_utf8(&line)
            .map_err(|_| invalid("invalid native Git object header encoding"))?;
        let fields = line.trim_end().split(' ').collect::<Vec<_>>();
        if fields.len() != 3
            || fields[0] != id.to_string()
            || fields[1] != kind.to_string()
            || fields[2].parse::<u64>().ok() != Some(size)
        {
            return Err(invalid(format!(
                "native Git object header disagrees for {id}"
            )));
        }
        let mut body = self.output.by_ref().take(size);
        let result = callback(kind, size, &mut body);
        io::copy(&mut body, &mut io::sink())?;
        if body.limit() != 0 {
            return Err(invalid("native Git object body was truncated"));
        }
        let mut terminator = [0];
        self.output.read_exact(&mut terminator)?;
        if terminator != [b'\n'] {
            return Err(invalid("invalid native Git object body terminator"));
        }
        self.failed = false;
        result
    }
}

impl Drop for GitPackReader {
    fn drop(&mut self) {
        drop(self.input.take());
        // A failed parser must not leave a blocked producer or a zombie child.
        let _kill_result = self.child.kill();
        let _wait_result = self.child.wait();
    }
}

fn insert_root(
    roots: &mut BTreeMap<gix::ObjectId, Option<gix::objs::Kind>>,
    bytes: &[u8],
    object_hash: gix::hash::Kind,
    expected_kind: Option<gix::objs::Kind>,
) -> SyncResult<()> {
    if bytes.len() != object_hash.len_in_bytes() {
        return Err(invalid("packed root has an unexpected hash format"));
    }
    let id = gix::ObjectId::from_bytes_or_panic(bytes);
    if id.is_null() {
        return Ok(());
    }
    match roots.entry(id) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(expected_kind);
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            if entry
                .get()
                .is_some_and(|kind| expected_kind.is_some_and(|expected| expected != kind))
            {
                return Err(invalid("packed root has conflicting expected types"));
            }
            if expected_kind.is_some() {
                entry.insert(expected_kind);
            }
        }
    }
    Ok(())
}

// Count only this disposable repository. In particular, the alternates file is
// ordinary file content here, never a path to traverse into the live ODB.
fn scratch_bytes(root: &Path, limits: &QuarantineLimits) -> SyncResult<u64> {
    let mut bytes = 0;
    let mut directories = vec![root.to_owned()];
    let mut entries = 0_usize;
    // Native Git may create incoming refs and auxiliary files as well as packs.
    // Bound traversal work by the existing record limit, with room for each
    // record's ref and pack/index plus the fixed bare-repository structure.
    let entry_limit = limits.max_records.saturating_mul(4).saturating_add(128);
    while let Some(directory) = directories.pop() {
        if !fs::symlink_metadata(&directory)?.is_dir() {
            return Err(invalid(
                "unexpected link or file in packed scratch directories",
            ));
        }
        bytes = add_budget(bytes, FILE_OVERHEAD, limits)?;
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            entries = entries.saturating_add(1);
            if entries > entry_limit {
                return Err(invalid("packed scratch files exceed the record limit"));
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file() {
                bytes = add_budget(
                    bytes,
                    entry.metadata()?.len().saturating_add(FILE_OVERHEAD),
                    limits,
                )?;
            } else {
                return Err(invalid("unexpected link or special file in packed scratch"));
            }
        }
    }
    Ok(bytes)
}

fn add_budget(current: u64, additional: u64, limits: &QuarantineLimits) -> SyncResult<u64> {
    current
        .checked_add(additional)
        .filter(|total| *total <= limits.max_staged_bytes)
        .ok_or_else(|| invalid("packed scratch exceeds sync.max-quarantine-bytes"))
}

fn install_immutable_file(source: &Path, target: &Path) -> SyncResult<()> {
    #[cfg(unix)]
    File::open(source)?.sync_data()?;
    match fs::hard_link(source, target) {
        Ok(()) => return Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            if fs::metadata(source)?.len() != fs::metadata(target)?.len() {
                return Err(invalid(
                    "existing content-addressed Git file has a different size",
                ));
            }
            return Ok(());
        }
        Err(_) => {}
    }
    let mut temporary = NamedTempFile::new_in(target.parent().unwrap())?;
    io::copy(&mut File::open(source)?, &mut temporary)?;
    temporary.as_file().sync_data()?;
    match temporary.persist_noclobber(target) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::metadata(source)?.len() != fs::metadata(target)?.len() {
                return Err(invalid(
                    "existing content-addressed Git file has a different size",
                ));
            }
            Ok(())
        }
        Err(error) => Err(error.error.into()),
    }
}

fn git_command(executable: &OsStr, git_dir: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("--no-replace-objects")
        .arg("--git-dir")
        .arg(git_dir);
    // A sync helper must not inherit a caller's object or reference namespace.
    for variable in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_QUARANTINE_PATH",
        "GIT_NAMESPACE",
        "GIT_REPLACE_REF_BASE",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_SHALLOW_FILE",
    ] {
        command.env_remove(variable);
    }
    command.env("GIT_OPTIONAL_LOCKS", "0");
    crate::subprocess_util::suppress_console_window(&mut command);
    command
}

fn invalid(message: impl Into<String>) -> SyncError {
    SyncError::InvalidData(message.into())
}
