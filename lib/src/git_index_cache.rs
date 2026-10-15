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
use std::time::SystemTime;

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
    index.write_to(&mut bytes, gix::index::write::Options::default())?;
    let kind = cache_hash(index.object_hash())?;
    let (state, _) = gix_index_cache::State::from_bytes(
        &bytes,
        filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
        kind,
        gix_index_cache::decode::Options::default(),
    )?;
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
/// Upstream gitoxide validates and builds the index first. The cache codec
/// walks the same tree to obtain the optional cache-tree extension. It uses
/// permissive path options because the upstream pass already validated the
/// paths using the repository's effective settings.
pub fn index_from_tree(
    repo: &gix::Repository,
    tree: &gix::hash::oid,
) -> Result<gix::index::File, ErrorBox> {
    let index = repo.index_from_tree(tree)?;
    let old_tree = gix_hash_cache::ObjectId::from_bytes_or_panic(tree.as_bytes());
    let options = gix_validate_cache::path::component::Options {
        protect_windows: false,
        protect_hfs: false,
        protect_ntfs: false,
    };
    let old_index = gix_index_cache::State::from_tree(&old_tree, TreeObjects(repo), options)?;
    let tree = old_index
        .tree()
        .expect("tree traversal builds a cache tree");
    let mut bytes = Vec::new();
    index.write_to(&mut bytes, gix::index::write::Options::default())?;
    let trailer = bytes.split_off(bytes.len() - repo.object_hash().len_in_bytes());
    tree.write_to(&mut bytes)?;
    bytes.extend_from_slice(&trailer);
    // from_bytes only decodes; the temporary trailer becomes valid when the
    // resulting index is actually written to the reserved lock file.
    let (state, _) = gix::index::State::from_bytes(
        &bytes,
        filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
        repo.object_hash(),
        gix::index::decode::Options::default(),
    )?;
    Ok(gix::index::File::from_state(state, index.path()))
}

/// Write an index, preserving its old worktree-specific caches where possible.
///
/// The caller must hold the index lock, and `out` must write to that lock, not
/// the live index. Dirty bits are remapped by path, stage, object and mode;
/// new or modified entries are dirty. Missing caches stay missing.
pub fn write_preserving_cache(
    index: &gix::index::File,
    mut out: impl Write,
) -> Result<(), ErrorBox> {
    let mut rebuilt = cache_index(index)?;
    if let Some(previous) = read_cache_index(index.path(), cache_hash(index.object_hash())?)? {
        rebuilt.inherit_fs_monitor_from(&previous);
        rebuilt.inherit_untracked_from(&previous);
    }
    rebuilt.write_to(&mut out, gix_index_cache::write::Options::default())?;
    Ok(())
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

    fn preserves_monitor(kind: gix::hash::Kind) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index");
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
        let mut bytes = Vec::new();
        write_preserving_cache(&next, &mut bytes).unwrap();
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
        let path = temp.path().join("index");
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
        let mut bytes = Vec::new();
        write_preserving_cache(&next, &mut bytes).unwrap();
        let parsed = read_bytes(&bytes, gix_hash_cache::Kind::Sha1);
        assert_eq!(
            before,
            format!("{:?}", parsed.untracked().expect("UNTR survives"))
        );
    }

    #[test]
    fn rejects_truncated_source_and_invalid_bitmap() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index");
        let index = make_index(&path, gix::hash::Kind::Sha1, &[("a", 1)]);
        assert!(write_with_monitor(&index, "token", &[], None, Vec::new()).is_err());
        std::fs::write(&path, b"DIRC").unwrap();
        assert!(write_preserving_cache(&index, Vec::new()).is_err());
    }
}
