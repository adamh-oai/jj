// Copyright 2024 The Jujutsu Authors
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

use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use futures::executor::block_on_stream;
use gix::objs::Exists as _;
use itertools::Itertools as _;
use jj_lib::backend::Backend as _;
use jj_lib::backend::ChangeId;
use jj_lib::backend::CommitId;
use jj_lib::backend::CopyRecord;
use jj_lib::backend::TreeId;
use jj_lib::commit::Commit;
use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigSource;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::git_backend::GitBackend;
use jj_lib::git_backend::GitObjectHeader;
use jj_lib::git_backend::GitObjectRecord;
use jj_lib::git_backend::JJ_TREES_COMMIT_HEADER;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::stacked_table::TableSegment as _;
use jj_lib::stacked_table::TableStore;
use jj_lib::store::Store;
use jj_lib::transaction::Transaction;
use maplit::hashmap;
use maplit::hashset;
use pollster::FutureExt as _;
use prost::Message as _;
use testutils::CommitBuilderExt as _;
use testutils::TestRepo;
use testutils::TestRepoBackend;
use testutils::TestResult;
use testutils::assert_tree_eq;
use testutils::base_user_config;
use testutils::commit_with_tree;
use testutils::create_random_commit;
use testutils::create_single_tree;
use testutils::create_tree;
use testutils::is_external_tool_installed;
use testutils::repo_path;
use testutils::repo_path_buf;
use testutils::write_random_commit;
use testutils::write_random_commit_with_parents;

fn get_git_backend(repo: &Arc<ReadonlyRepo>) -> &GitBackend {
    repo.store().backend_impl().unwrap()
}

fn collect_no_gc_refs(git_repo_path: &Path) -> HashSet<CommitId> {
    // Load fresh git repo to isolate from false caching issue. Here we want to
    // ensure that the underlying data is correct. We could test the in-memory
    // data as well, but we don't have any special handling in our code.
    let git_repo = gix::open(git_repo_path).unwrap();
    let git_refs = git_repo.references().unwrap();
    let no_gc_refs_iter = git_refs.prefixed("refs/jj/keep/").unwrap();
    no_gc_refs_iter
        .map(|git_ref| CommitId::from_bytes(git_ref.unwrap().id().as_bytes()))
        .collect()
}

fn get_copy_records(
    store: &Store,
    paths: Option<&[RepoPathBuf]>,
    a: &Commit,
    b: &Commit,
) -> HashMap<String, String> {
    let stream = store.get_copy_records(paths, a.id(), b.id()).unwrap();
    let mut res: HashMap<String, String> = HashMap::new();
    for CopyRecord { target, source, .. } in block_on_stream(stream).filter_map(|r| r.ok()) {
        res.insert(
            target.as_internal_file_string().into(),
            source.as_internal_file_string().into(),
        );
    }
    res
}

fn make_commit(
    tx: &mut Transaction,
    parents: Vec<CommitId>,
    content: &[(&RepoPath, &str)],
) -> Commit {
    let tree = create_tree(tx.base_repo(), content);
    tx.repo_mut().new_commit(parents, tree).write_unwrap()
}

fn list_dir(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
        .sorted()
        .collect()
}

struct ShortReader<R>(R);

impl<R: io::Read> io::Read for ShortReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let length = buffer.len().min(3);
        self.0.read(&mut buffer[..length])
    }
}

impl<R: io::Seek> io::Seek for ShortReader<R> {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.0.seek(position)
    }
}

fn transfer_object_closure(
    source: &GitBackend,
    destination: &GitBackend,
    id: gix::ObjectId,
) -> TestResult {
    let mut pending = vec![id];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let record = source.export_object(&id)?;
        match gix::objs::ObjectRef::from_bytes(&record.data, record.kind, id.kind()).unwrap() {
            gix::objs::ObjectRef::Commit(commit) => {
                pending.push(commit.tree());
                pending.extend(commit.parents());
                let extras = jj_lib::protos::git_store::Commit::decode(
                    record.commit_extras.as_deref().unwrap(),
                )?;
                pending.extend(
                    extras
                        .predecessors
                        .iter()
                        .chain(&extras.root_tree)
                        .map(|bytes| gix::ObjectId::from_bytes_or_panic(bytes)),
                );
            }
            gix::objs::ObjectRef::Tree(tree) => {
                pending.extend(
                    tree.entries
                        .iter()
                        .filter(|entry| !entry.mode.is_commit())
                        .map(|entry| entry.oid.to_owned()),
                );
            }
            gix::objs::ObjectRef::Tag(tag) => pending.push(tag.target()),
            gix::objs::ObjectRef::Blob(_) => {}
        }
        let header = source.export_object_header(&id)?;
        assert_eq!(header.kind, record.kind);
        assert_eq!(header.size, record.data.len() as u64);
        assert_eq!(header.commit_extras, record.commit_extras);
        let mut streamed = Vec::new();
        source.export_object_stream(&header, &mut streamed)?;
        assert_eq!(streamed, record.data);
        destination
            .validate_object_stream(&header, &mut ShortReader(io::Cursor::new(&streamed)))?;
        destination.import_object_stream(&header, &mut ShortReader(io::Cursor::new(&streamed)))?;
        destination.validate_object_import(&record)?;
        destination.import_object(&record)?;
        // Importing an identical record is idempotent, including its metadata.
        destination.import_object(&record)?;
        assert_eq!(destination.export_object(&id)?, record);
    }
    Ok(())
}

#[test]
fn test_object_transfer_exports_unmaterialized_empty_tree() -> TestResult {
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let pinned = TestRepo::init_with_backend(TestRepoBackend::Git);
    let source_backend = get_git_backend(&source.repo);
    let id = gix::ObjectId::from_bytes_or_panic(source_backend.empty_tree_id().as_bytes());
    assert!(!source_backend.git_repo().objects.exists(&id));
    let record = source_backend.export_object(&id)?;
    assert_eq!(record.kind, gix::objs::Kind::Tree);
    assert!(record.data.is_empty());
    assert!(record.commit_extras.is_none());
    let header = source_backend.export_object_header(&id)?;
    assert_eq!(header.kind, gix::objs::Kind::Tree);
    assert_eq!(header.size, 0);
    assert!(header.commit_extras.is_none());
    let mut streamed = Vec::new();
    source_backend.export_object_stream(&header, &mut streamed)?;
    assert!(streamed.is_empty());
    assert!(!source_backend.git_repo().objects.exists(&id));
    let destination_backend = get_git_backend(&destination.repo);
    destination_backend.validate_object_import(&record)?;
    destination_backend.import_object(&record)?;
    assert!(destination_backend.git_repo().objects.exists(&id));
    assert_eq!(destination_backend.export_object(&id)?, record);

    // Pinning a logically valid empty tree also creates its physical object,
    // even if no commit has ever needed to materialize it.
    let pinned_backend = get_git_backend(&pinned.repo);
    pinned_backend.pin_sync_trees([pinned_backend.empty_tree_id()])?;
    assert!(pinned_backend.git_repo().objects.exists(&id));
    Ok(())
}

#[test]
fn test_object_stream_large_loose_and_packed_blob() -> TestResult {
    if !is_external_tool_installed("git") {
        return Ok(());
    }
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let source_backend = get_git_backend(&source.repo);
    let destination_backend = get_git_backend(&destination.repo);
    let size = 65 * 1024 * 1024u64;
    let mut body = tempfile::NamedTempFile::new()?;
    let mut input = io::repeat(42).take(size);
    io::copy(&mut input, &mut body)?;
    let mut hash = gix::hash::hasher(source_backend.git_repo().object_hash());
    hash.update(&gix::objs::encode::loose_header(
        gix::objs::Kind::Blob,
        size,
    ));
    let block = [42; 64 * 1024];
    for _ in 0..size / block.len() as u64 {
        hash.update(&block);
    }
    let id = hash.try_finalize().unwrap();
    let header = GitObjectHeader {
        id,
        kind: gix::objs::Kind::Blob,
        size,
        commit_extras: None,
    };
    source_backend.import_object_stream(&header, &mut body.reopen()?)?;
    assert_eq!(source_backend.export_object_header(&id)?, header);
    destination_backend.validate_object_stream(&header, &mut body.reopen()?)?;
    assert!(
        destination_backend
            .git_repo()
            .try_find_header(id)?
            .is_none()
    );
    destination_backend.import_object_stream(&header, &mut body.reopen()?)?;

    struct CompareWriter<R> {
        expected: R,
        size: u64,
    }
    impl<R: io::Read> io::Write for CompareWriter<R> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            assert!(bytes.len() <= 64 * 1024);
            let mut expected = [0u8; 64 * 1024];
            self.expected.read_exact(&mut expected[..bytes.len()])?;
            assert_eq!(bytes, &expected[..bytes.len()]);
            self.size += bytes.len() as u64;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = CompareWriter {
        expected: body.reopen()?,
        size: 0,
    };
    source_backend.export_object_stream(&header, &mut output)?;
    assert_eq!(output.size, size);

    // Force the same object into a pack, then exercise exactly the same export
    // path. The test itself retains only fixed-size comparison buffers.
    source_backend.git_repo().reference(
        "refs/jj/test-stream-blob",
        id,
        gix::refs::transaction::PreviousValue::Any,
        "stream test",
    )?;
    source_backend.gc(source.repo.index(), SystemTime::now())?;
    let packed = gix::open(source_backend.git_repo_path())?
        .try_find_header(id)?
        .unwrap();
    assert!(packed.num_deltas().is_some());
    assert_eq!(source_backend.export_object_header(&id)?, header);
    let mut output = CompareWriter {
        expected: body.reopen()?,
        size: 0,
    };
    source_backend.export_object_stream(&header, &mut output)?;
    assert_eq!(output.size, size);
    assert_eq!(destination_backend.export_object_header(&id)?, header);
    Ok(())
}

#[test]
fn test_object_stream_large_commit_header_and_message() -> TestResult {
    use gix::objs::Write as _;
    if !is_external_tool_installed("git") {
        return Ok(());
    }
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let source_backend = get_git_backend(&source.repo);
    let destination_backend = get_git_backend(&destination.repo);
    let change_id = ChangeId::from_bytes(&[42; 16]);
    let prefix = format!(
        "tree {}\nauthor A <a@example.com> 0 +0000\ncommitter C <c@example.com> 0 \
         +0000\nchange-id {}\ngpgsig ",
        source_backend.empty_tree_id().hex(),
        change_id.reverse_hex(),
    );
    let signature_size = 65 * 1024 * 1024u64;
    let suffix = "\n\nA message whose bytes remain immutable.\n";
    let size = prefix.len() as u64 + signature_size + suffix.len() as u64;
    let mut input = io::Cursor::new(prefix.as_bytes())
        .chain(io::repeat(b'a').take(signature_size))
        .chain(io::Cursor::new(suffix.as_bytes()));
    let id = source_backend
        .git_repo()
        .objects
        .write_stream(gix::objs::Kind::Commit, size, &mut input)
        .unwrap();
    // No metadata entry exists: header export must recover the change ID while
    // discarding the arbitrarily large signature, without an import or rewrite.
    let header = source_backend.export_object_header(&id)?;
    let extras =
        jj_lib::protos::git_store::Commit::decode(header.commit_extras.as_deref().unwrap())?;
    assert_eq!(extras.change_id, change_id.as_bytes());
    assert_eq!(header.size, size);
    let metadata = TableStore::load(source.repo_path().join("store/extra"), id.as_bytes().len());
    assert!(metadata.get_head()?.get_value(id.as_bytes()).is_none());
    let mut body = tempfile::NamedTempFile::new()?;
    source_backend.export_object_stream(&header, &mut body)?;
    destination_backend.validate_object_stream(&header, &mut body.reopen()?)?;
    destination_backend.import_object_stream(&header, &mut body.reopen()?)?;
    assert_eq!(destination_backend.export_object_header(&id)?, header);
    Ok(())
}

#[test]
fn test_object_stream_rejects_invalid_bodies_before_writing() -> TestResult {
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let backend = get_git_backend(&destination.repo);
    let data = b"valid streamed blob";
    let id = gix::objs::compute_hash(
        backend.git_repo().object_hash(),
        gix::objs::Kind::Blob,
        data,
    )
    .unwrap();
    let header = GitObjectHeader {
        id,
        kind: gix::objs::Kind::Blob,
        size: data.len() as u64,
        commit_extras: None,
    };
    for body in [
        &data[..data.len() - 1],
        b"valid streamed blop",
        b"valid streamed blob trailing",
    ] {
        assert!(
            backend
                .validate_object_stream(&header, &mut io::Cursor::new(body))
                .is_err()
        );
        assert!(
            backend
                .import_object_stream(&header, &mut io::Cursor::new(body))
                .is_err()
        );
        assert!(backend.git_repo().try_find_header(id)?.is_none());
    }
    let mut invalid = header.clone();
    invalid.commit_extras = Some(vec![]);
    assert!(
        backend
            .import_object_stream(&invalid, &mut io::Cursor::new(data))
            .is_err()
    );
    assert!(backend.git_repo().try_find_header(id)?.is_none());
    Ok(())
}

#[test]
fn test_object_transfer_preserves_commit_identity_and_conflicts() -> TestResult {
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let mut tx = source.repo.start_transaction();
    let predecessor = write_random_commit(tx.repo_mut());
    let trees = ["base", "left", "right"]
        .map(|text| create_single_tree(&source.repo, &[(repo_path("file"), text)]));
    let tree = MergedTree::new(
        source.repo.store().clone(),
        Merge::from_vec(trees.iter().map(|tree| tree.id().clone()).collect_vec()),
        ConflictLabels::from_vec(vec!["base".into(), "left".into(), "right".into()]),
    );
    let commit = tx
        .repo_mut()
        .new_commit(vec![source.repo.store().root_commit_id().clone()], tree)
        .set_predecessors(vec![predecessor.id().clone()])
        .write_unwrap();
    let source_backend = get_git_backend(&source.repo);
    let destination_backend = get_git_backend(&destination.repo);
    assert!(
        source_backend
            .sync_legacy_tree_roots([commit.id(), predecessor.id()])?
            .is_empty()
    );
    let old_operation = destination.repo.op_id().clone();
    let old_refs = collect_no_gc_refs(destination_backend.git_repo_path());
    transfer_object_closure(
        source_backend,
        destination_backend,
        gix::ObjectId::from_bytes_or_panic(commit.id().as_bytes()),
    )?;
    assert_eq!(
        source_backend.read_commit(commit.id()).block_on()?,
        destination_backend.read_commit(commit.id()).block_on()?,
    );
    assert_eq!(
        source_backend.read_commit(predecessor.id()).block_on()?,
        destination_backend
            .read_commit(predecessor.id())
            .block_on()?,
    );
    assert_eq!(destination.repo.op_id(), &old_operation);
    assert_eq!(
        collect_no_gc_refs(destination_backend.git_repo_path()),
        old_refs
    );
    Ok(())
}

#[test]
fn test_object_transfer_preserves_legacy_conflict_metadata() -> TestResult {
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let source_backend = get_git_backend(&source.repo);
    let git_repo = source_backend.git_repo();
    let trees = ["base", "left", "right"]
        .map(|text| create_single_tree(&source.repo, &[(repo_path("file"), text)]));
    let ordinary = commit_with_tree(
        source.repo.store(),
        MergedTree::resolved(source.repo.store().clone(), trees[0].id().clone()),
    );
    let mut raw: gix::objs::Commit = git_repo
        .find_commit(gix::ObjectId::from_bytes_or_panic(ordinary.id().as_bytes()))?
        .decode()
        .unwrap()
        .try_into()
        .unwrap();
    raw.message = "legacy conflict".into();
    let git_id = git_repo.write_object(&raw)?.detach();
    let commit_id = CommitId::from_bytes(git_id.as_bytes());
    let extras = jj_lib::protos::git_store::Commit {
        change_id: vec![42; 16],
        root_tree: trees.iter().map(|tree| tree.id().to_bytes()).collect(),
        predecessors: vec![ordinary.id().to_bytes()],
        uses_tree_conflict_format: true,
        ..Default::default()
    }
    .encode_to_vec();
    let table_store = TableStore::load(
        source.repo_path().join("store/extra"),
        commit_id.as_bytes().len(),
    );
    let (table, _lock) = table_store.get_head_locked()?;
    let mut mutation = table.start_mutation();
    mutation.add_entry(commit_id.to_bytes(), extras.clone());
    table_store.save_table(mutation)?;
    drop(_lock);

    let record = source_backend.export_object(&git_id)?;
    assert_eq!(record.commit_extras, Some(extras));
    assert_eq!(
        source_backend.sync_legacy_tree_roots([
            &commit_id,
            ordinary.id(),
            source.repo.store().root_commit_id(),
        ])?,
        trees.iter().map(|tree| tree.id().clone()).collect(),
    );
    transfer_object_closure(source_backend, get_git_backend(&destination.repo), git_id)?;
    let imported = get_git_backend(&destination.repo)
        .read_commit(&commit_id)
        .block_on()?;
    assert_eq!(imported.change_id.as_bytes(), &[42; 16]);
    assert_eq!(imported.predecessors, vec![ordinary.id().clone()]);
    assert_eq!(
        imported.root_tree,
        Merge::from_vec(trees.iter().map(|tree| tree.id().clone()).collect_vec()),
    );
    Ok(())
}

#[test]
fn test_object_transfer_exports_unimported_signed_git_commit() -> TestResult {
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let backend = get_git_backend(&source.repo);
    let git_repo = backend.git_repo();
    let ordinary = commit_with_tree(
        source.repo.store(),
        MergedTree::resolved(
            source.repo.store().clone(),
            source.repo.store().empty_tree_id().clone(),
        ),
    );
    let mut raw: gix::objs::Commit = git_repo
        .find_commit(gix::ObjectId::from_bytes_or_panic(ordinary.id().as_bytes()))?
        .decode()
        .unwrap()
        .try_into()
        .unwrap();
    raw.message = "unimported signed commit".into();
    raw.extra_headers
        .push(("gpgsig".into(), "a signature\nwith another line".into()));
    let id = git_repo.write_object(&raw)?.detach();
    let table_store = TableStore::load(source.repo_path().join("store/extra"), id.as_bytes().len());
    assert!(table_store.get_head()?.get_value(id.as_bytes()).is_none());
    let source_refs = collect_no_gc_refs(backend.git_repo_path());
    let record = backend.export_object(&id)?;
    assert!(record.commit_extras.is_some());
    assert!(table_store.get_head()?.get_value(id.as_bytes()).is_none());
    assert_eq!(collect_no_gc_refs(backend.git_repo_path()), source_refs);
    transfer_object_closure(backend, get_git_backend(&destination.repo), id)?;
    let imported = get_git_backend(&destination.repo)
        .read_commit(&CommitId::from_bytes(id.as_bytes()))
        .block_on()?;
    let signature = imported.secure_sig.unwrap();
    assert_eq!(signature.sig, b"a signature\nwith another line\n");
    assert_eq!(record.data, git_repo.find_object(id)?.data);
    Ok(())
}

#[test]
fn test_object_transfer_rejects_independently_imported_change_identity() -> TestResult {
    use gix::objs::Write as _;

    let mut config = base_user_config();
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer.set_value("git.write-change-id-header", false)?;
    config.add_layer(layer);
    let settings = testutils::user_settings_from_config(config);
    let source = TestRepo::init_with_backend_and_settings(TestRepoBackend::Git, &settings);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let mut tx = source.repo.start_transaction();
    let commit = write_random_commit(tx.repo_mut());
    let id = gix::ObjectId::from_bytes_or_panic(commit.id().as_bytes());
    let source_backend = get_git_backend(&source.repo);
    let destination_backend = get_git_backend(&destination.repo);
    let record = source_backend.export_object(&id)?;
    destination_backend
        .git_repo()
        .write_buf(record.kind, &record.data)
        .unwrap();
    destination_backend.import_head_commits([commit.id()])?;
    let previously_imported = destination_backend.read_commit(commit.id()).block_on()?;
    assert_ne!(previously_imported.change_id, *commit.change_id());
    let error = destination_backend
        .validate_object_import(&record)
        .unwrap_err();
    assert!(error.to_string().contains("Incompatible jj metadata"));
    let error = destination_backend.import_object(&record).unwrap_err();
    assert!(error.to_string().contains("Incompatible jj metadata"));
    assert_eq!(
        destination_backend.read_commit(commit.id()).block_on()?,
        previously_imported,
    );
    Ok(())
}

#[test]
fn test_object_transfer_rejects_conflicting_metadata() -> TestResult {
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let mut tx = source.repo.start_transaction();
    let commit = write_random_commit(tx.repo_mut());
    let source_backend = get_git_backend(&source.repo);
    let destination_backend = get_git_backend(&destination.repo);
    let id = gix::ObjectId::from_bytes_or_panic(commit.id().as_bytes());
    let record = source_backend.export_object(&id)?;
    destination_backend.import_object(&record)?;
    for field in ["change_id", "predecessors", "root_tree"] {
        let mut incompatible = record.clone();
        let mut extras =
            jj_lib::protos::git_store::Commit::decode(record.commit_extras.as_deref().unwrap())?;
        match field {
            "change_id" => extras.change_id[0] ^= 1,
            "predecessors" => extras.predecessors.push(id.as_bytes().to_vec()),
            "root_tree" => {
                extras.root_tree = vec![vec![1; id.as_bytes().len()]; 3];
            }
            _ => unreachable!(),
        }
        incompatible.commit_extras = Some(extras.encode_to_vec());
        let header = GitObjectHeader {
            id,
            kind: incompatible.kind,
            size: incompatible.data.len() as u64,
            commit_extras: incompatible.commit_extras.clone(),
        };
        assert!(
            destination_backend
                .validate_object_stream(&header, &mut ShortReader(io::Cursor::new(&record.data)))
                .is_err()
        );
        assert!(
            destination_backend
                .import_object_stream(&header, &mut ShortReader(io::Cursor::new(&record.data)))
                .is_err()
        );
        let error = destination_backend
            .validate_object_import(&incompatible)
            .unwrap_err();
        assert!(error.to_string().contains("Incompatible jj metadata"));
        let error = destination_backend
            .import_object(&incompatible)
            .unwrap_err();
        assert!(error.to_string().contains("Incompatible jj metadata"));
        assert_eq!(destination_backend.export_object(&id)?, record);
    }
    Ok(())
}

#[test]
fn test_object_transfer_accepts_equivalent_legacy_extras() -> TestResult {
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let mut tx = source.repo.start_transaction();
    let commit = write_random_commit(tx.repo_mut());
    let id = gix::ObjectId::from_bytes_or_panic(commit.id().as_bytes());
    let record = get_git_backend(&source.repo).export_object(&id)?;
    let mut legacy = record.clone();
    let mut extras =
        jj_lib::protos::git_store::Commit::decode(record.commit_extras.as_deref().unwrap())?;
    // With no root-tree override, this flag has no effect on commit identity.
    extras.uses_tree_conflict_format = false;
    legacy.commit_extras = Some(extras.encode_to_vec());
    let backend = get_git_backend(&destination.repo);
    backend.import_object(&legacy)?;
    let header = GitObjectHeader {
        id,
        kind: record.kind,
        size: record.data.len() as u64,
        commit_extras: record.commit_extras.clone(),
    };
    backend.validate_object_stream(&header, &mut ShortReader(io::Cursor::new(&record.data)))?;
    backend.import_object_stream(&header, &mut ShortReader(io::Cursor::new(&record.data)))?;
    backend.validate_object_import(&record)?;
    backend.import_object(&record)?;
    assert_eq!(backend.export_object(&id)?, legacy);
    Ok(())
}

#[test]
fn test_object_transfer_rejects_invalid_records_before_writing() -> TestResult {
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let backend = get_git_backend(&destination.repo);
    let data = b"blob contents".to_vec();
    let id = gix::objs::compute_hash(
        backend.git_repo().object_hash(),
        gix::objs::Kind::Blob,
        &data,
    )
    .unwrap();
    let valid = GitObjectRecord {
        id,
        kind: gix::objs::Kind::Blob,
        data,
        commit_extras: None,
    };
    let mut invalid = valid.clone();
    invalid.data.push(0);
    assert!(backend.validate_object_import(&invalid).is_err());
    assert!(backend.import_object(&invalid).is_err());
    invalid = valid.clone();
    invalid.commit_extras = Some(vec![]);
    assert!(backend.validate_object_import(&invalid).is_err());
    assert!(backend.import_object(&invalid).is_err());
    assert!(backend.git_repo().try_find_object(id)?.is_none());
    backend.import_object(&valid)?;
    assert_eq!(backend.export_object(&id)?, valid);
    Ok(())
}

#[test]
fn test_object_transfer_rejects_invalid_commit_metadata_before_writing() -> TestResult {
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let mut tx = source.repo.start_transaction();
    let commit = write_random_commit(tx.repo_mut());
    let id = gix::ObjectId::from_bytes_or_panic(commit.id().as_bytes());
    let record = get_git_backend(&source.repo).export_object(&id)?;
    let backend = get_git_backend(&destination.repo);
    for metadata in [None, Some(vec![255])] {
        let mut invalid = record.clone();
        invalid.commit_extras = metadata;
        assert!(backend.validate_object_import(&invalid).is_err());
        assert!(backend.import_object(&invalid).is_err());
    }
    for field in ["predecessors", "root_tree"] {
        let mut invalid = record.clone();
        let mut extras =
            jj_lib::protos::git_store::Commit::decode(record.commit_extras.as_deref().unwrap())?;
        match field {
            "predecessors" => extras.predecessors.push(vec![1]),
            "root_tree" => extras.root_tree = vec![vec![1; id.as_bytes().len()]; 2],
            _ => unreachable!(),
        }
        invalid.commit_extras = Some(extras.encode_to_vec());
        assert!(backend.validate_object_import(&invalid).is_err());
        assert!(backend.import_object(&invalid).is_err());
    }
    assert!(backend.git_repo().try_find_object(id)?.is_none());
    Ok(())
}

#[test]
fn test_object_transfer_preflight_does_not_publish() -> TestResult {
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let mut tx = source.repo.start_transaction();
    let commit = write_random_commit(tx.repo_mut());
    let id = gix::ObjectId::from_bytes_or_panic(commit.id().as_bytes());
    let record = get_git_backend(&source.repo).export_object(&id)?;
    let backend = get_git_backend(&destination.repo);
    let extra_path = destination.repo_path().join("store/extra");
    let metadata_files = || {
        list_dir(&extra_path)
            .into_iter()
            .filter(|name| name != "lock")
            .collect_vec()
    };
    let previous_metadata = metadata_files();
    let previous_heads = list_dir(&extra_path.join("heads"));
    let previous_refs = collect_no_gc_refs(backend.git_repo_path());
    backend.validate_object_import(&record)?;
    assert!(backend.git_repo().try_find_object(id)?.is_none());
    assert_eq!(metadata_files(), previous_metadata);
    assert_eq!(list_dir(&extra_path.join("heads")), previous_heads);
    assert_eq!(collect_no_gc_refs(backend.git_repo_path()), previous_refs);

    // A successful preflight does not permit a later conflicting import. A
    // writer arriving in between must still be detected by the real import.
    let mut conflicting = record.clone();
    let mut extras =
        jj_lib::protos::git_store::Commit::decode(record.commit_extras.as_deref().unwrap())?;
    extras.change_id[0] ^= 1;
    conflicting.commit_extras = Some(extras.encode_to_vec());
    backend.import_object(&conflicting)?;
    assert!(backend.validate_object_import(&record).is_err());
    assert!(backend.import_object(&record).is_err());
    assert_eq!(backend.export_object(&id)?, conflicting);
    Ok(())
}

#[test]
fn test_object_transfer_rejects_conflicting_metadata_in_divergent_tables() -> TestResult {
    let source = TestRepo::init_with_backend(TestRepoBackend::Git);
    let destination = TestRepo::init_with_backend(TestRepoBackend::Git);
    let mut tx = source.repo.start_transaction();
    let commit = write_random_commit(tx.repo_mut());
    let id = gix::ObjectId::from_bytes_or_panic(commit.id().as_bytes());
    let record = get_git_backend(&source.repo).export_object(&id)?;
    let extra_path = destination.repo_path().join("store/extra");
    let table_store = TableStore::load(extra_path.clone(), id.as_bytes().len());
    let parent = table_store.get_head()?;
    let mut left = parent.start_mutation();
    left.add_entry(
        id.as_bytes().to_vec(),
        record.commit_extras.clone().unwrap(),
    );
    table_store.save_table(left)?;
    let mut right = parent.start_mutation();
    let mut conflicting =
        jj_lib::protos::git_store::Commit::decode(record.commit_extras.as_deref().unwrap())?;
    conflicting.change_id[0] ^= 1;
    right.add_entry(id.as_bytes().to_vec(), conflicting.encode_to_vec());
    table_store.save_table(right)?;
    let previous_heads = list_dir(&extra_path.join("heads"));
    assert_eq!(previous_heads.len(), 2);

    let backend = get_git_backend(&destination.repo);
    assert!(backend.validate_object_import(&record).is_err());
    assert!(backend.import_object(&record).is_err());
    assert!(backend.git_repo().try_find_object(id)?.is_none());
    assert_eq!(list_dir(&extra_path.join("heads")), previous_heads);
    Ok(())
}

#[test]
fn test_sync_tree_pins_protect_and_release_extra_metadata_trees() -> TestResult {
    if !is_external_tool_installed("git") {
        return Ok(());
    }
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let mut tx = test_repo.repo.start_transaction();
    make_commit(
        &mut tx,
        vec![test_repo.repo.store().root_commit_id().clone()],
        &[(repo_path("file"), "reachable")],
    );
    let repo = tx.commit("reachable commit").block_on()?;
    let backend = get_git_backend(&repo);
    let left = create_single_tree(&repo, &[(repo_path("file"), "legacy conflict left")]);
    let right = create_single_tree(&repo, &[(repo_path("file"), "legacy conflict right")]);
    // These terms need not be reachable from the physical Git commit tree.
    backend.pin_sync_trees([left.id()])?;
    backend.pin_sync_trees([right.id(), right.id()])?;
    let missing_tree = TreeId::from_bytes(&vec![1; backend.commit_id_length()]);
    assert!(backend.replace_sync_tree_roots([&missing_tree]).is_err());
    let find_tree = |id: &TreeId| -> TestResult<bool> {
        let git_repo = gix::open(backend.git_repo_path())?;
        Ok(git_repo
            .try_find_object(gix::ObjectId::from_bytes_or_panic(id.as_bytes()))?
            .is_some())
    };
    let expire = || SystemTime::now() + Duration::from_secs(1);
    // Exercise the ordinary, sync-unaware GC path too: it must leave this
    // separate namespace alone, even if it was started before sync enablement.
    backend.gc(repo.index(), expire())?;
    assert!(find_tree(left.id())?);
    assert!(find_tree(right.id())?);

    backend.replace_sync_tree_roots([right.id()])?;
    backend.gc(repo.index(), expire())?;
    assert!(!find_tree(left.id())?);
    assert!(find_tree(right.id())?);

    backend.replace_sync_tree_roots([])?;
    backend.gc(repo.index(), expire())?;
    assert!(!find_tree(right.id())?);
    Ok(())
}

#[test]
fn test_gc() -> TestResult {
    // TODO: Better way to disable the test if git command couldn't be executed
    if !is_external_tool_installed("git") {
        eprintln!("Skipping because git command might fail to run");
        return Ok(());
    }

    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = test_repo.repo;
    let git_repo_path = get_git_backend(&repo).git_repo_path();
    let base_index = repo.readonly_index();

    // Set up commits:
    //
    //     H (predecessor: D)
    //   G |
    //   |\|
    //   | F
    //   E |
    // D | |
    // C |/
    // |/
    // B
    // A
    let mut tx = repo.start_transaction();
    let commit_a = write_random_commit(tx.repo_mut());
    let commit_b = write_random_commit_with_parents(tx.repo_mut(), &[&commit_a]);
    let commit_c = write_random_commit_with_parents(tx.repo_mut(), &[&commit_b]);
    let commit_d = write_random_commit_with_parents(tx.repo_mut(), &[&commit_c]);
    let commit_e = write_random_commit_with_parents(tx.repo_mut(), &[&commit_b]);
    let commit_f = write_random_commit_with_parents(tx.repo_mut(), &[&commit_b]);
    let commit_g = write_random_commit_with_parents(tx.repo_mut(), &[&commit_e, &commit_f]);
    let commit_h = create_random_commit(tx.repo_mut())
        .set_parents(vec![commit_f.id().clone()])
        .set_predecessors(vec![commit_d.id().clone()])
        .write_unwrap();
    let repo = tx.commit("test").block_on()?;
    assert_eq!(
        *repo.view().heads(),
        hashset! {
            commit_d.id().clone(),
            commit_g.id().clone(),
            commit_h.id().clone(),
        },
    );

    // At first, all commits have no-gc refs
    assert_eq!(
        collect_no_gc_refs(git_repo_path),
        hashset! {
            commit_a.id().clone(),
            commit_b.id().clone(),
            commit_c.id().clone(),
            commit_d.id().clone(),
            commit_e.id().clone(),
            commit_f.id().clone(),
            commit_g.id().clone(),
            commit_h.id().clone(),
        },
    );

    // Empty index, but all kept by file modification time
    // (Beware that this invokes "git gc" and refs will be packed.)
    get_git_backend(&repo).gc(base_index.as_index(), SystemTime::UNIX_EPOCH)?;
    assert_eq!(
        collect_no_gc_refs(git_repo_path),
        hashset! {
            commit_a.id().clone(),
            commit_b.id().clone(),
            commit_c.id().clone(),
            commit_d.id().clone(),
            commit_e.id().clone(),
            commit_f.id().clone(),
            commit_g.id().clone(),
            commit_h.id().clone(),
        },
    );

    // Don't rely on the exact system time because file modification time might
    // have lower precision for example.
    let now = || SystemTime::now() + Duration::from_secs(1);

    // All reachable: redundant no-gc refs will be removed
    get_git_backend(&repo).gc(repo.index(), now())?;
    assert_eq!(
        collect_no_gc_refs(git_repo_path),
        hashset! {
            commit_d.id().clone(),
            commit_g.id().clone(),
            commit_h.id().clone(),
        },
    );

    // G is no longer reachable
    let mut mut_index = base_index.start_modification();
    mut_index.add_commit(&commit_a).block_on()?;
    mut_index.add_commit(&commit_b).block_on()?;
    mut_index.add_commit(&commit_c).block_on()?;
    mut_index.add_commit(&commit_d).block_on()?;
    mut_index.add_commit(&commit_e).block_on()?;
    mut_index.add_commit(&commit_f).block_on()?;
    mut_index.add_commit(&commit_h).block_on()?;
    get_git_backend(&repo).gc(mut_index.as_index(), now())?;
    assert_eq!(
        collect_no_gc_refs(git_repo_path),
        hashset! {
            commit_d.id().clone(),
            commit_e.id().clone(),
            commit_h.id().clone(),
        },
    );

    // D|E|H are no longer reachable
    let mut mut_index = base_index.start_modification();
    mut_index.add_commit(&commit_a).block_on()?;
    mut_index.add_commit(&commit_b).block_on()?;
    mut_index.add_commit(&commit_c).block_on()?;
    mut_index.add_commit(&commit_f).block_on()?;
    get_git_backend(&repo).gc(mut_index.as_index(), now())?;
    assert_eq!(
        collect_no_gc_refs(git_repo_path),
        hashset! {
            commit_c.id().clone(),
            commit_f.id().clone(),
        },
    );

    // B|C|F are no longer reachable
    let mut mut_index = base_index.start_modification();
    mut_index.add_commit(&commit_a).block_on()?;
    get_git_backend(&repo).gc(mut_index.as_index(), now())?;
    assert_eq!(
        collect_no_gc_refs(git_repo_path),
        hashset! {
            commit_a.id().clone(),
        },
    );

    // All unreachable
    get_git_backend(&repo).gc(base_index.as_index(), now())?;
    assert_eq!(collect_no_gc_refs(git_repo_path), hashset! {});
    Ok(())
}

#[test]
fn test_gc_extra_table() -> TestResult {
    if !is_external_tool_installed("git") {
        eprintln!("Skipping because git command might fail to run");
        return Ok(());
    }

    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let extra_path = test_repo.repo_path().join("store").join("extra");
    let extra_key_size = test_repo.repo.store().root_commit_id().as_bytes().len();
    let load_repo = || {
        test_repo
            .env
            .load_repo_at_head(test_repo.repo.settings(), test_repo.repo_path())
    };
    let load_extra_table = || {
        TableStore::load(extra_path.clone(), extra_key_size)
            .get_head()
            .unwrap()
    };
    let collect_extra_segment_num_entries = || {
        let mut num_entries = load_extra_table()
            .ancestor_segments()
            .map(|table| table.segment_num_entries())
            .collect_vec();
        num_entries.reverse();
        num_entries
    };

    // Sanity check for the initial state
    assert_eq!(collect_extra_segment_num_entries(), [0]);
    assert_eq!(list_dir(&extra_path).len(), 1 + 1); // empty segment + "heads"

    // Write 4 commits
    let mut tx = test_repo.repo.start_transaction();
    for _ in 0..4 {
        write_random_commit(tx.repo_mut());
    }
    tx.commit("test").block_on()?;
    // The first 3 will be squashed into one table segment
    assert_eq!(collect_extra_segment_num_entries(), [3, 1]);
    assert_eq!(list_dir(&extra_path).len(), 5 + 1);

    // Reload repo to invalidate cache in TableStore
    let repo = load_repo();
    let index = repo.readonly_index().as_index();

    // All segments should be kept by modification time
    get_git_backend(&repo).gc(index, SystemTime::UNIX_EPOCH)?;
    assert_eq!(collect_extra_segment_num_entries(), [3, 1]);
    assert_eq!(list_dir(&extra_path).len(), 5 + 1);

    // All unreachable segments should be removed
    let now = SystemTime::now() + Duration::from_secs(1);
    get_git_backend(&repo).gc(index, now)?;
    assert_eq!(collect_extra_segment_num_entries(), [3, 1]);
    assert_eq!(list_dir(&extra_path).len(), 2 + 1);

    // Ensure that repo is still loadable
    load_repo();
    Ok(())
}

#[test]
fn test_copy_detection() -> TestResult {
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;

    let paths = &[
        repo_path_buf("file0"),
        repo_path_buf("file1"),
        repo_path_buf("file2"),
    ];

    let mut tx = repo.start_transaction();
    let commit_a = make_commit(
        &mut tx,
        vec![repo.store().root_commit_id().clone()],
        &[(&paths[0], "content")],
    );
    let commit_b = make_commit(
        &mut tx,
        vec![commit_a.id().clone()],
        &[(&paths[1], "content")],
    );
    let commit_c = make_commit(
        &mut tx,
        vec![commit_b.id().clone()],
        &[(&paths[2], "content")],
    );

    let store = repo.store();
    assert_eq!(
        get_copy_records(store, Some(paths), &commit_a, &commit_b),
        HashMap::from([("file1".to_string(), "file0".to_string())])
    );
    assert_eq!(
        get_copy_records(store, Some(paths), &commit_b, &commit_c),
        HashMap::from([("file2".to_string(), "file1".to_string())])
    );
    assert_eq!(
        get_copy_records(store, Some(paths), &commit_a, &commit_c),
        HashMap::from([("file2".to_string(), "file0".to_string())])
    );
    assert_eq!(
        get_copy_records(store, None, &commit_a, &commit_c),
        HashMap::from([("file2".to_string(), "file0".to_string())])
    );
    assert_eq!(
        get_copy_records(store, Some(&[paths[1].clone()]), &commit_a, &commit_c),
        HashMap::default(),
    );
    assert_eq!(
        get_copy_records(store, Some(paths), &commit_c, &commit_c),
        HashMap::default(),
    );
    Ok(())
}

#[test]
fn test_copy_detection_file_and_dir() -> TestResult {
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;

    // a -> b (file)
    // b -> a (dir)
    // c -> c/file (file)
    let mut tx = repo.start_transaction();
    let commit_a = make_commit(
        &mut tx,
        vec![repo.store().root_commit_id().clone()],
        &[
            (repo_path("a"), "content1"),
            (repo_path("b/file"), "content2"),
            (repo_path("c"), "content3"),
        ],
    );
    let commit_b = make_commit(
        &mut tx,
        vec![commit_a.id().clone()],
        &[
            (repo_path("a/file"), "content2"),
            (repo_path("b"), "content1"),
            (repo_path("c/file"), "content3"),
        ],
    );

    assert_eq!(
        get_copy_records(repo.store(), None, &commit_a, &commit_b),
        hashmap! {
            "b".to_owned() => "a".to_owned(),
            "a/file".to_owned() => "b/file".to_owned(),
            "c/file".to_owned() => "c".to_owned(),
        }
    );
    Ok(())
}

#[test]
fn test_jj_trees_header_with_one_tree() -> TestResult {
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = test_repo.repo;
    let git_backend = get_git_backend(&repo);
    let git_repo = git_backend.git_repo();

    let tree_1 = create_single_tree(&repo, &[(repo_path("file"), "aaa")]);
    let tree_2 = create_single_tree(&repo, &[(repo_path("file"), "bbb")]);

    // Create a normal commit with tree 1
    let commit = commit_with_tree(
        repo.store(),
        MergedTree::resolved(repo.store().clone(), tree_1.id().clone()),
    );
    let git_commit_id = gix::ObjectId::from_bytes_or_panic(commit.id().as_bytes());
    let git_commit = git_repo.find_commit(git_commit_id)?;

    // Add `jj:trees` with a single tree which is different from the Git commit tree
    let mut new_commit: gix::objs::Commit = git_commit.decode().unwrap().try_into().unwrap();
    new_commit.extra_headers = vec![(
        JJ_TREES_COMMIT_HEADER.into(),
        tree_2.id().to_string().into(),
    )];
    let new_commit_id = git_repo.write_object(&new_commit)?;
    let new_commit_id = CommitId::from_bytes(new_commit_id.as_bytes());

    // Import new commit into `jj` repo. This should fail, because allowing a
    // non-conflicted commit to have a different tree in `jj` than in Git could be
    // used to hide malicious code.
    insta::assert_debug_snapshot!(git_backend.import_head_commits(std::slice::from_ref(&new_commit_id)), @r#"
    Err(
        ReadObject {
            object_type: "commit",
            hash: "87df728a30166ce1de0bf883948dd66b74cf25a0",
            source: "Invalid jj:trees header",
        },
    )
    "#);
    Ok(())
}

#[test]
fn test_conflict_headers_roundtrip() -> TestResult {
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = test_repo.repo;

    let tree_1 = create_single_tree(&repo, &[(repo_path("file"), "aaa")]);
    let tree_2 = create_single_tree(&repo, &[(repo_path("file"), "bbb")]);
    let tree_3 = create_single_tree(&repo, &[(repo_path("file"), "ccc")]);
    let tree_4 = create_single_tree(&repo, &[(repo_path("file"), "ddd")]);
    let tree_5 = create_single_tree(&repo, &[(repo_path("file"), "eee")]);
    let tree_6 = create_single_tree(&repo, &[(repo_path("file"), "fff")]);
    let tree_7 = create_single_tree(&repo, &[(repo_path("file"), "ggg")]);

    // This creates a Git commit header with leading and trailing newlines to ensure
    // that it can still be parsed correctly. The resulting `jj:conflict-labels`
    // header value will look like `\nbase 1\nside 2\n\nside 3\n\n\n`.
    let merged_tree = MergedTree::new(
        repo.store().clone(),
        Merge::from_vec(vec![
            tree_1.id().clone(),
            tree_2.id().clone(),
            tree_3.id().clone(),
            tree_4.id().clone(),
            tree_5.id().clone(),
            tree_6.id().clone(),
            tree_7.id().clone(),
        ]),
        ConflictLabels::from_vec(vec![
            "".into(),
            // Test that various UTF-8 characters can be encoded and decoded. Git doesn't allow
            // storing arbitrary binary data in headers, but it does allow storing UTF-8 encoded
            // strings as long as they don't contain U+0000 codepoints.
            "base 1 \"utf8 \u{67d4}\u{8853} \u{ba4}\u{bc1}\u{bb0}\u{bc1} \u{2699}\u{fe0f}\"".into(),
            "side 2".into(),
            "".into(),
            "side 3".into(),
            "".into(),
            "".into(),
        ]),
    );

    // Create a commit with the conflicted tree.
    let commit = commit_with_tree(repo.store(), merged_tree.clone());
    // Clear cached commit to ensure it is re-read.
    repo.store().clear_caches();
    // Conflict trees and labels should be preserved on read.
    assert_tree_eq!(repo.store().get_commit(commit.id())?.tree(), merged_tree);
    Ok(())
}
