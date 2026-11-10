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

//! Preserve Git index extensions through the upstream index writer.
//!
//! Upstream gitoxide can read FSMN and UNTR but cannot write them yet. Use the
//! pinned cache codec only at this byte boundary, so all repository operations
//! continue using upstream gitoxide. The caller must hold the index lock from
//! before reading the old index through publishing the new one.

use std::error::Error;
use std::io;
use std::io::Write;
use std::path::Path;
use std::time::Instant;
use std::time::SystemTime;

#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::scan::BeginScanRequest;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::scan::Invalidation;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::scan::ScanClient as _;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::scan::ScanOutcome;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::scan::SnapshotLease;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::scan_facade::DirectScanClient;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::status::decode_git_fsmonitor_token;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use btrfs_awacs::status::encode_git_fsmonitor_token;

/// An index encoding or I/O error.
pub type ErrorBox = Box<dyn Error + Send + Sync>;

fn cache_hash(kind: gix::hash::Kind) -> Result<gix_hash_cache::Kind, ErrorBox> {
    match kind {
        gix::hash::Kind::Sha1 => Ok(gix_hash_cache::Kind::Sha1),
        gix::hash::Kind::Sha256 => Ok(gix_hash_cache::Kind::Sha256),
        _ => Err(io::Error::other("unsupported Git index hash kind").into()),
    }
}

fn cache_index(index: &gix::index::File) -> Result<gix_index_cache::File, ErrorBox> {
    let mut bytes = Vec::new();
    let started = Instant::now();
    let result = index.write_to(&mut bytes, gix::index::write::Options::default());
    tracing::debug!(
        phase = "git_index_cache_conversion_encode",
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        succeeded = result.is_ok(),
        entry_count = index.entries().len(),
        "Git index phase completed"
    );
    result?;
    let kind = cache_hash(index.object_hash())?;
    let started = Instant::now();
    let result = gix_index_cache::State::from_bytes(
        &bytes,
        filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
        kind,
        gix_index_cache::decode::Options::default(),
    );
    tracing::debug!(
        phase = "git_index_cache_conversion_decode",
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        succeeded = result.is_ok(),
        entry_count = index.entries().len(),
        "Git index phase completed"
    );
    let (state, _) = result?;
    Ok(gix_index_cache::File::from_state(state, index.path()))
}

fn read_cache_index(
    path: &Path,
    kind: gix_hash_cache::Kind,
) -> Result<Option<gix_index_cache::File>, ErrorBox> {
    // The codec hashes by slicing the trailer, so reject short/truncated input
    // before opening it. The lock prevents cooperating writers changing it.
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() < (12 + kind.len_in_bytes()) as u64 => {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated Git index").into());
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    Ok(Some(gix_index_cache::File::at(
        path,
        kind,
        false,
        gix_index_cache::decode::Options::default(),
    )?))
}

// The index codec uses a different gitoxide version. Bridge object lookup by
// object ID and raw object bytes, keeping all repository operations on the
// upstream version.
struct TreeObjects<'a>(&'a gix::Repository);

impl gix_object_cache::Find for TreeObjects<'_> {
    fn try_find<'a>(
        &self,
        id: &gix_hash_cache::oid,
        buffer: &'a mut Vec<u8>,
    ) -> Result<Option<gix_object_cache::Data<'a>>, gix_object_cache::find::Error> {
        let id = gix::ObjectId::from_bytes_or_panic(id.as_bytes());
        let Some(object) = self.0.try_find_object(id)? else {
            return Ok(None);
        };
        let kind = match object.kind {
            gix::object::Kind::Tree => gix_object_cache::Kind::Tree,
            gix::object::Kind::Blob => gix_object_cache::Kind::Blob,
            gix::object::Kind::Commit => gix_object_cache::Kind::Commit,
            gix::object::Kind::Tag => gix_object_cache::Kind::Tag,
        };
        buffer.clear();
        buffer.extend_from_slice(&object.data);
        Ok(Some(gix_object_cache::Data {
            kind,
            object_hash: cache_hash(self.0.object_hash())?,
            data: buffer,
        }))
    }
}

/// Create a verified index and a TREE cache for a resolved Git tree.
///
/// Build entries and their cache-tree extension in the same traversal, using
/// the repository's effective path protections.
pub fn index_from_tree(
    repo: &gix::Repository,
    tree: &gix::hash::oid,
) -> Result<gix::index::File, ErrorBox> {
    let config = repo.config_snapshot();
    let protect_windows = config.try_boolean("gitoxide.core.protectWindows");
    let protect_hfs = config.try_boolean("core.protectHFS");
    let protect_ntfs = config.try_boolean("core.protectNTFS");
    if protect_windows.is_err() || protect_hfs.is_err() || protect_ntfs.is_err() {
        // Upstream doesn't expose its config leniency policy. Let it reject
        // malformed protection settings in strict repositories, or validate
        // with their defaults in lenient repositories, before using defaults
        // here. Valid configurations only need the single traversal below.
        repo.index_from_tree(tree)?;
    }
    let options = gix_validate_cache::path::component::Options {
        protect_windows: protect_windows.ok().flatten().unwrap_or(cfg!(windows)),
        protect_hfs: protect_hfs
            .ok()
            .flatten()
            .unwrap_or(cfg!(target_os = "macos")),
        protect_ntfs: protect_ntfs.ok().flatten().unwrap_or(true),
    };
    let tree = gix_hash_cache::ObjectId::from_bytes_or_panic(tree.as_bytes());
    let started = Instant::now();
    let result = gix_index_cache::State::from_tree(&tree, TreeObjects(repo), options);
    tracing::debug!(
        phase = "git_index_from_tree_traversal",
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        succeeded = result.is_ok(),
        entry_count = result.as_ref().map_or(0, |index| index.entries().len()),
        "Git index phase completed"
    );
    let state = match result {
        Ok(state) => state,
        Err(gix_index_cache::init::from_tree::Error::InvalidComponent { path, source }) => {
            use gix::error::ErrorExt as _;

            return Err(source
                .and_raise(gix::error::message!("The path \"{path}\" is invalid").validation())
                .into());
        }
        Err(err) => return Err(err.into()),
    };
    let index = gix_index_cache::File::from_state(state, repo.index_path());
    let entry_count = index.entries().len();
    let mut bytes = Vec::new();
    let started = Instant::now();
    let result = index.write_to(&mut bytes, gix_index_cache::write::Options::default());
    tracing::debug!(
        phase = "git_index_tree_conversion_encode",
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        succeeded = result.is_ok(),
        entry_count,
        "Git index phase completed"
    );
    result?;
    let started = Instant::now();
    let result = gix::index::State::from_bytes(
        &bytes,
        filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
        repo.object_hash(),
        gix::index::decode::Options::default(),
    );
    tracing::debug!(
        phase = "git_index_tree_conversion_decode",
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        succeeded = result.is_ok(),
        entry_count,
        "Git index phase completed"
    );
    let (state, _) = result?;
    Ok(gix::index::File::from_state(state, index.path()))
}

/// Replace the index under the caller's lock and keep its worktree caches valid.
///
/// Remap dirty bits by path, stage, object and mode. If the old index has an
/// AWACS cursor, apply the pending invalidations before saving a newer cursor.
pub(crate) fn write_preserving_cache(
    repo: &gix::Repository,
    index: &gix::index::File,
    index_lock: gix::lock::File,
) -> Result<(), ErrorBox> {
    let entry_count = index.entries().len();
    let started = Instant::now();
    let result = cache_index(index);
    tracing::debug!(
        phase = "git_index_cache_write_conversion",
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        succeeded = result.is_ok(),
        entry_count,
        "Git index phase completed"
    );
    let mut rebuilt = result?;
    let kind = cache_hash(index.object_hash())?;
    let started = Instant::now();
    let result = read_cache_index(index.path(), kind);
    tracing::debug!(
        phase = "git_index_cache_read_previous",
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        succeeded = result.is_ok(),
        entry_count,
        "Git index phase completed"
    );
    let previous = result?;
    let started = Instant::now();
    if let Some(previous) = &previous {
        rebuilt.inherit_fs_monitor_from(previous);
    }
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    let mut update = previous
        .as_ref()
        .and_then(|previous| GitMonitorUpdate::prepare(repo, previous, &mut rebuilt));
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    let keep_untracked = update.as_ref().is_none_or(|update| update.keep_untracked);
    #[cfg(not(all(target_os = "linux", feature = "awacs")))]
    let keep_untracked = {
        let _ = repo;
        true
    };
    if let Some(previous) = previous.filter(|_| keep_untracked) {
        rebuilt.inherit_untracked_from(&previous);
    }
    tracing::debug!(
        phase = "git_index_cache_inherit",
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        entry_count,
        "Git index phase completed"
    );
    let result = (|| {
        let mut writer = io::BufWriter::with_capacity(64 * 1024, index_lock);
        let started = Instant::now();
        let result = rebuilt.write_to(&mut writer, gix_index_cache::write::Options::default());
        tracing::debug!(
            phase = "git_index_cache_encode_and_write",
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            succeeded = result.is_ok(),
            entry_count,
            "Git index phase completed"
        );
        result?;
        let started = Instant::now();
        let result = writer.into_inner();
        tracing::debug!(
            phase = "git_index_flush",
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            succeeded = result.is_ok(),
            entry_count,
            "Git index phase completed"
        );
        let index_lock = result.map_err(|err| err.into_error())?;
        let started = Instant::now();
        let result = index_lock.commit();
        tracing::debug!(
            phase = "git_index_commit",
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            succeeded = result.is_ok(),
            entry_count,
            "Git index phase completed"
        );
        result?;
        Ok(())
    })();
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    if let Some(update) = &mut update {
        update.finish(result.is_ok());
    }
    result
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
struct GitMonitorUpdate<'a> {
    client: DirectScanClient,
    lease: SnapshotLease,
    root: &'a Path,
    keep_untracked: bool,
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
impl<'a> GitMonitorUpdate<'a> {
    fn prepare(
        repo: &'a gix::Repository,
        previous: &gix_index_cache::File,
        rebuilt: &mut gix_index_cache::File,
    ) -> Option<Self> {
        let root = repo.workdir()?;
        let baseline = decode_git_fsmonitor_token(previous.fs_monitor()?.token()?)?;
        let owner = baseline.retention_token.as_slice().try_into().ok()?;
        if previous.is_sparse()
            || rebuilt.is_sparse()
            || repo.config_snapshot().boolean("core.ignoreCase") == Some(true)
        {
            return None;
        }
        let result = (|| {
            let mut client = DirectScanClient::for_git_index_write(root, rebuilt.path())?;
            let mut lease = client.begin_scan(&BeginScanRequest {
                live_root: root.to_owned(),
                baseline_owner_id: owner,
                previous_baseline: Some(baseline),
                allow_full_invalidation: false,
            })?;
            let applied = (|| {
                client.validate_scan_root(&lease)?;
                let token =
                    encode_git_fsmonitor_token(&lease.next_baseline).map_err(io::Error::other)?;
                apply_git_invalidations(rebuilt, &token, &lease.invalidation)
            })();
            let has_changes = match applied {
                Ok(has_changes) => has_changes,
                Err(err) => {
                    drop(lease.finish(ScanOutcome::Aborted));
                    return Err(err);
                }
            };
            // UNTR also depends on which paths are in the index, even if the
            // worktree itself did not change.
            let keep_untracked = !has_changes
                && previous.untracked().is_some()
                && previous.entries().len() == rebuilt.entries().len()
                && previous
                    .entries()
                    .iter()
                    .zip(rebuilt.entries())
                    .all(|(old, new)| {
                        old.path(previous) == new.path(rebuilt)
                            && old.mode.is_submodule() == new.mode.is_submodule()
                    });
            Ok::<_, ErrorBox>(Self {
                client,
                lease,
                root,
                keep_untracked,
            })
        })();
        match result {
            Ok(update) => Some(update),
            Err(err) => {
                tracing::debug!(%err, "keeping the previous Git fsmonitor cursor");
                None
            }
        }
    }

    fn finish(&mut self, index_written: bool) {
        if index_written {
            // A concurrent Git writer may already have replaced our index.
            // Keep the response pin until process exit and let retention
            // read the current index in that case.
            if let Err(err) = self
                .client
                .observe_persisted_git_index(self.root, &self.lease.next_baseline)
            {
                tracing::debug!(%err, "could not observe the new Git fsmonitor cursor");
            }
        }
        let outcome = if index_written {
            ScanOutcome::Committed
        } else {
            ScanOutcome::Aborted
        };
        if let Err(err) = self.lease.finish(outcome) {
            tracing::debug!(%err, "could not finish the Git index scan");
        }
    }
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn apply_git_invalidations(
    index: &mut gix_index_cache::File,
    token: &str,
    invalidation: &Invalidation,
) -> Result<bool, ErrorBox> {
    let Invalidation::Paths { exact, prefixes } = invalidation else {
        return Err(io::Error::other("Git fsmonitor requires an incremental AWACS delta").into());
    };
    let mut dirty = vec![false; index.entries().len()];
    if index
        .fs_monitor()
        .and_then(|monitor| {
            monitor.for_each_dirty_entry(|position| {
                *dirty.get_mut(position)? = true;
                Some(())
            })
        })
        .is_none()
    {
        return Err(io::Error::other("invalid Git fsmonitor dirty bitmap").into());
    }
    let mut directory = Vec::new();
    let mut has_worktree_changes = false;
    for path in exact.iter().chain(prefixes) {
        let path = path.strip_suffix(b"/").unwrap_or(path);
        if path.contains(&0)
            || path
                .split(|byte| *byte == b'/')
                .any(|component| matches!(component, b"" | b"." | b".."))
        {
            return Err(io::Error::other("invalid path in AWACS Git delta").into());
        }
        // Git's own index and lock updates do not change untracked paths.
        if path == b".git" || path.starts_with(b".git/") {
            continue;
        }
        has_worktree_changes = true;
        if let Some(range) = index.entry_range(bstr::BStr::new(path)) {
            dirty[range].fill(true);
        }
        // AWACS can report a directory as an exact path too. The slash
        // makes the prefix match only entries in that directory.
        directory.clear();
        directory.extend_from_slice(path);
        directory.push(b'/');
        if let Some(range) = index.prefixed_entries_range(bstr::BStr::new(&directory)) {
            dirty[range].fill(true);
        }
    }
    let monitor = gix_index_cache::extension::FsMonitor::from_token(token, &dirty)
        .ok_or_else(|| io::Error::other("invalid AWACS Git cursor or dirty bitmap"))?;
    index.set_fs_monitor(monitor);
    Ok(has_worktree_changes)
}

/// Write an index with the supplied filesystem-monitor token and dirty bits.
///
/// This is for a caller that has already proved which entries are clean at the
/// specified baseline. The caller must hold the index lock through publication.
/// If supplied, `previous_index` must be bytes read under that lock from the
/// same worktree's index; its untracked-cache extension will be retained.
pub fn write_with_monitor(
    index: &gix::index::File,
    token: &str,
    dirty: &[bool],
    previous_index: Option<&[u8]>,
    mut out: impl Write,
) -> Result<(), ErrorBox> {
    if dirty.len() != index.entries().len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Git index bitmap length mismatch",
        )
        .into());
    }
    let mut rebuilt = cache_index(index)?;
    if let Some(previous_index) = previous_index {
        let (previous, _) = gix_index_cache::State::from_bytes(
            previous_index,
            SystemTime::UNIX_EPOCH.into(),
            cache_hash(index.object_hash())?,
            gix_index_cache::decode::Options::default(),
        )?;
        rebuilt.inherit_untracked_from(&previous);
    }
    let monitor =
        gix_index_cache::extension::FsMonitor::from_token(token, dirty).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Git fsmonitor cursor or bitmap",
            )
        })?;
    rebuilt.set_fs_monitor(monitor);
    rebuilt.write_to(&mut out, gix_index_cache::write::Options::default())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(
        path: &Path,
        kind: gix::hash::Kind,
        strict: bool,
        config: &[String],
    ) -> gix::Repository {
        gix::ThreadSafeRepository::init_opts(
            path,
            gix::create::Kind::Bare,
            gix::create::Options {
                object_hash: Some(kind),
                ..Default::default()
            },
            gix::open::Options::isolated()
                .strict_config(strict)
                .config_overrides(config.iter().cloned()),
        )
        .unwrap()
        .to_thread_local()
    }

    fn write_tree(
        repo: &gix::Repository,
        entries: &[(&str, gix::objs::tree::EntryKind, gix::ObjectId)],
    ) -> gix::ObjectId {
        let mut entries = entries
            .iter()
            .map(|(name, mode, oid)| gix::objs::tree::Entry {
                mode: (*mode).into(),
                filename: (*name).into(),
                oid: *oid,
            })
            .collect::<Vec<_>>();
        entries.sort();
        repo.write_object(gix::objs::Tree { entries })
            .unwrap()
            .detach()
    }

    #[test]
    fn index_from_tree_preserves_entries_and_cache_tree() {
        use gix::objs::tree::EntryKind::Blob;
        use gix::objs::tree::EntryKind::BlobExecutable;
        use gix::objs::tree::EntryKind::Commit;
        use gix::objs::tree::EntryKind::Link;
        use gix::objs::tree::EntryKind::Tree;

        for kind in [gix::hash::Kind::Sha1, gix::hash::Kind::Sha256] {
            let temp = tempfile::tempdir().unwrap();
            let repo = init_repo(temp.path(), kind, true, &[]);
            let blob = repo.write_blob(b"contents").unwrap().detach();
            let empty = write_tree(&repo, &[]);
            let child = write_tree(
                &repo,
                &[
                    ("file", Blob, blob),
                    ("run", BlobExecutable, blob),
                    ("symlink", Link, blob),
                    ("vendor", Commit, gix::ObjectId::null(kind)),
                ],
            );
            let root = write_tree(
                &repo,
                &[
                    ("a", Tree, child),
                    ("a.c", Blob, blob),
                    ("empty", Tree, empty),
                    ("z", Blob, blob),
                ],
            );
            for tree in [root, empty] {
                let expected = repo.index_from_tree(&tree).unwrap();
                let actual = index_from_tree(&repo, &tree).unwrap();
                assert_eq!(actual.path(), repo.index_path());
                assert_eq!(actual.object_hash(), kind);
                assert_eq!(actual.version(), expected.version());
                assert_eq!(actual.entries().len(), expected.entries().len());
                for (actual_entry, expected_entry) in
                    actual.entries().iter().zip(expected.entries())
                {
                    assert_eq!(actual_entry.path(&actual), expected_entry.path(&expected));
                    assert_eq!(actual_entry.id, expected_entry.id);
                    assert_eq!(actual_entry.mode, expected_entry.mode);
                    assert_eq!(actual_entry.flags, expected_entry.flags);
                    assert_eq!(actual_entry.stage(), expected_entry.stage());
                    assert_eq!(actual_entry.stat, expected_entry.stat);
                }
                assert!(actual.fs_monitor().is_none());
                assert!(actual.untracked().is_none());
                let cache = actual.tree().expect("new index has a TREE cache");
                assert_eq!(cache.id, tree);
                assert_eq!(cache.num_entries, Some(actual.entries().len() as u32));
                if tree == root {
                    assert_eq!(cache.children.len(), 2);
                    assert_eq!(cache.children[0].name.as_slice(), b"a");
                    assert_eq!(cache.children[0].id, child);
                    assert_eq!(cache.children[0].num_entries, Some(4));
                    assert_eq!(cache.children[1].name.as_slice(), b"empty");
                    assert_eq!(cache.children[1].id, empty);
                    assert_eq!(cache.children[1].num_entries, Some(0));
                } else {
                    assert!(cache.children.is_empty());
                }
            }
        }
    }

    #[test]
    fn index_from_tree_uses_effective_path_protections() {
        use gix::objs::tree::EntryKind::Blob;
        use gix::objs::tree::EntryKind::Link;
        use gix::objs::tree::EntryKind::Tree;

        // Include defaults as well as every combination of explicit settings.
        let configs = std::iter::once(Vec::new()).chain((0..8).map(|bits| {
            vec![
                format!("gitoxide.core.protectWindows={}", bits & 1 != 0),
                format!("core.protectHFS={}", bits & 2 != 0),
                format!("core.protectNTFS={}", bits & 4 != 0),
            ]
        }));
        for config in configs {
            let temp = tempfile::tempdir().unwrap();
            let repo = init_repo(temp.path(), gix::hash::Kind::Sha1, true, &config);
            let blob = repo.write_blob(b"contents").unwrap().detach();
            let child = write_tree(&repo, &[("file", Blob, blob)]);
            for name in [
                "ordinary",
                ".git",
                ".gitmodules",
                "git~1",
                ".g\u{200c}it",
                "CON",
                "a\\b",
                "..",
            ] {
                for mode in [Blob, Link, Tree] {
                    let oid = if mode == Tree { child } else { blob };
                    let tree = write_tree(&repo, &[(name, mode, oid)]);
                    let expected = repo.index_from_tree(&tree);
                    let actual = index_from_tree(&repo, &tree);
                    assert_eq!(
                        actual.is_ok(),
                        expected.is_ok(),
                        "config={config:?}, name={name:?}, mode={mode:?}"
                    );
                    if let Err(err) = actual {
                        assert!(
                            err.downcast_ref::<gix::Error>()
                                .is_some_and(|err| err.is_validation()),
                            "invalid paths retain validation classification: {err:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn index_from_tree_respects_config_error_policy() {
        use gix::objs::tree::EntryKind::Blob;

        for strict in [false, true] {
            for key in [
                "gitoxide.core.protectWindows",
                "core.protectHFS",
                "core.protectNTFS",
            ] {
                for mut config in [
                    Vec::new(),
                    vec![
                        "gitoxide.core.protectWindows=true".to_owned(),
                        "core.protectHFS=true".to_owned(),
                        "core.protectNTFS=false".to_owned(),
                    ],
                ] {
                    // An invalid key defaults individually, retaining valid
                    // explicit values for the other protection settings.
                    config.push(format!("{key}=invalid-boolean"));
                    let temp = tempfile::tempdir().unwrap();
                    let repo = init_repo(temp.path(), gix::hash::Kind::Sha1, strict, &config);
                    let blob = repo.write_blob(b"contents").unwrap().detach();
                    for name in ["ordinary", "git~1", ".g\u{200c}it", "CON", "a\\b"] {
                        let tree = write_tree(&repo, &[(name, Blob, blob)]);
                        let expected = repo.index_from_tree(&tree);
                        let actual = index_from_tree(&repo, &tree);
                        assert_eq!(
                            actual.is_ok(),
                            expected.is_ok(),
                            "strict={strict}, config={config:?}, name={name:?}"
                        );
                        if strict || name == "ordinary" {
                            assert_eq!(actual.is_ok(), !strict);
                        }
                    }
                }
            }
        }
    }

    fn make_index(path: &Path, kind: gix::hash::Kind, paths: &[(&str, u8)]) -> gix::index::File {
        let mut index = gix::index::File::from_state(gix::index::State::new(kind), path);
        for &(name, object_byte) in paths {
            let bytes = vec![object_byte; kind.len_in_bytes()];
            index.dangerously_push_entry(
                gix::index::entry::Stat::default(),
                gix::ObjectId::from_bytes_or_panic(&bytes),
                gix::index::entry::Flags::empty(),
                gix::index::entry::Mode::FILE,
                name.as_bytes().into(),
            );
        }
        index.sort_entries();
        index
    }

    fn read_bytes(bytes: &[u8], kind: gix_hash_cache::Kind) -> gix_index_cache::State {
        gix_index_cache::State::from_bytes(
            bytes,
            filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
            kind,
            gix_index_cache::decode::Options::default(),
        )
        .unwrap()
        .0
    }

    fn rewrite_index(repo: &gix::Repository, index: &gix::index::File) -> Result<(), ErrorBox> {
        let lock = gix::lock::File::acquire_to_update_resource(
            index.path(),
            gix::lock::acquire::Fail::Immediately,
            None,
            0,
        )?;
        write_preserving_cache(repo, index, lock)
    }

    fn preserves_monitor(kind: gix::hash::Kind) {
        let temp = tempfile::tempdir().unwrap();
        let repo = init_repo(temp.path(), kind, true, &[]);
        let path = repo.index_path();
        let original = make_index(&path, kind, &[("a", 1), ("b", 2), ("c", 3), ("e", 5)]);
        let mut bytes = Vec::new();
        write_with_monitor(
            &original,
            "test-cursor",
            &[false, true, false, true],
            None,
            &mut bytes,
        )
        .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let next = make_index(
            &path,
            kind,
            &[("a", 1), ("b", 9), ("c", 3), ("d", 4), ("e", 5)],
        );
        rewrite_index(&repo, &next).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let parsed = read_bytes(&bytes, cache_hash(kind).unwrap());
        let monitor = parsed.fs_monitor().expect("fsmonitor survives the rewrite");
        assert_eq!(monitor.token().unwrap(), "test-cursor");
        let mut dirty = Vec::new();
        monitor
            .for_each_dirty_entry(|i| {
                dirty.push(i);
                Some(())
            })
            .unwrap();
        assert_eq!(dirty, [1, 3, 4]);
        let (new_index, _) = gix::index::State::from_bytes(
            &bytes,
            filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
            kind,
            gix::index::decode::Options::default(),
        )
        .unwrap();
        assert_eq!(new_index.entries().len(), 5);
        assert!(new_index.fs_monitor().is_some());
    }

    #[test]
    fn preserves_sha1_monitor() {
        preserves_monitor(gix::hash::Kind::Sha1);
    }

    #[test]
    fn preserves_sha256_monitor() {
        preserves_monitor(gix::hash::Kind::Sha256);
    }

    #[test]
    fn preserves_untracked_extension() {
        let temp = tempfile::tempdir().unwrap();
        let repo = init_repo(temp.path(), gix::hash::Kind::Sha1, true, &[]);
        let path = repo.index_path();
        std::fs::write(
            &path,
            include_bytes!("../tests/fixtures/untracked-cache.index"),
        )
        .unwrap();
        let previous = read_cache_index(&path, gix_hash_cache::Kind::Sha1)
            .unwrap()
            .unwrap();
        let before = format!("{:?}", previous.untracked().expect("fixture has UNTR"));
        let next = make_index(&path, gix::hash::Kind::Sha1, &[("new", 1)]);
        rewrite_index(&repo, &next).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let parsed = read_bytes(&bytes, gix_hash_cache::Kind::Sha1);
        assert_eq!(
            before,
            format!("{:?}", parsed.untracked().expect("UNTR survives"))
        );
    }

    #[test]
    fn rejects_truncated_source_and_invalid_bitmap() {
        let temp = tempfile::tempdir().unwrap();
        let repo = init_repo(temp.path(), gix::hash::Kind::Sha1, true, &[]);
        let path = repo.index_path();
        let index = make_index(&path, gix::hash::Kind::Sha1, &[("a", 1)]);
        assert!(write_with_monitor(&index, "token", &[], None, Vec::new()).is_err());
        std::fs::write(&path, b"DIRC").unwrap();
        assert!(rewrite_index(&repo, &index).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"DIRC");
    }

    #[cfg(all(target_os = "linux", feature = "awacs"))]
    #[test]
    fn advancing_monitor_keeps_dirty_bits_and_applies_directory_moves() {
        let index = make_index(
            Path::new("index"),
            gix::hash::Kind::Sha1,
            &[
                ("a", 1),
                ("dir-sibling/a", 2),
                ("dir/a", 3),
                ("dir/b", 4),
                ("file", 5),
            ],
        );
        let mut index = cache_index(&index).unwrap();
        index.set_fs_monitor(
            gix_index_cache::extension::FsMonitor::from_token(
                "old",
                &[true, false, false, false, false],
            )
            .unwrap(),
        );
        let paths = Invalidation::Paths {
            exact: vec![b"file".to_vec()],
            prefixes: vec![b"dir/".to_vec()],
        };
        assert!(apply_git_invalidations(&mut index, "new", &paths).unwrap());
        let monitor = index.fs_monitor().unwrap();
        assert_eq!(monitor.token().unwrap(), "new");
        let mut dirty = Vec::new();
        monitor
            .for_each_dirty_entry(|position| {
                dirty.push(position);
                Some(())
            })
            .unwrap();
        assert_eq!(dirty, [0, 2, 3, 4]);

        let git_metadata = Invalidation::Paths {
            exact: vec![b".git/index".to_vec(), b".git/index.lock".to_vec()],
            prefixes: vec![],
        };
        assert!(!apply_git_invalidations(&mut index, "metadata", &git_metadata).unwrap());
        let malformed = Invalidation::Paths {
            exact: vec![b".".to_vec()],
            prefixes: vec![],
        };
        assert!(apply_git_invalidations(&mut index, "bad", &malformed).is_err());
        assert_eq!(index.fs_monitor().unwrap().token().unwrap(), "metadata");
    }
}
