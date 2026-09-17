//! Carry fsmonitor invalidations across a copied Git index's baseline change.

use std::collections::BTreeSet;

use crate::scan::Invalidation;
use crate::status::git_index_layout;
use crate::status::read_be_u32;
use crate::status::visit_git_index_paths;

/// ORs replayed paths into Git's persisted dirty bitmap. A set bit means dirty;
/// clearing an inherited bit would silently skip work Git had not validated.
///
/// The input and output include FSMN's four-byte length before the EWAH data.
/// Unsupported index layouts or malformed data return an error so the caller
/// can invalidate the complete index instead of trusting incomplete entries.
pub fn invalidate_fsmonitor_bitmap(
    index_bytes: &[u8],
    hash_len: usize,
    inherited_bitmap: &[u8],
    invalidation: &Invalidation,
) -> Result<Vec<u8>, String> {
    let layout = git_index_layout(index_bytes, hash_len)?;
    if layout.has_required_extensions() || layout.has_sparse_directories {
        return Err(
            "cannot map fsmonitor bits in a split, sparse, or extended Git index".to_owned(),
        );
    }
    let mut dirty = decode_bitmap(inherited_bitmap, layout.entries)?;
    match invalidation {
        Invalidation::Full => dirty.fill(u64::MAX),
        Invalidation::ExactPaths(paths) | Invalidation::Prefixes(paths) => {
            let mut prefixes = BTreeSet::new();
            for path in paths {
                // The root is a conservative full invalidation. A trailing
                // slash is how directory events may be represented.
                let path = path.strip_suffix(b"/").unwrap_or(path);
                if path.is_empty() || path == b"." {
                    dirty.fill(u64::MAX);
                    return encode_bitmap(layout.entries, &dirty);
                }
                validate_path(path)?;
                prefixes.insert(path);
            }
            if prefixes.is_empty() {
                return Ok(inherited_bitmap.to_vec());
            }
            visit_git_index_paths(index_bytes, hash_len, |entry, path| {
                validate_path(path)?;
                // Treat exact directory events conservatively too. A rename
                // may invalidate a directory and all indexed descendants.
                if prefixes.contains(path)
                    || path
                        .iter()
                        .enumerate()
                        .any(|(end, byte)| *byte == b'/' && prefixes.contains(&path[..end]))
                {
                    dirty[entry as usize / 64] |= 1 << (entry % 64);
                }
                Ok(())
            })?;
        }
    }
    encode_bitmap(layout.entries, &dirty)
}

fn validate_path(path: &[u8]) -> Result<(), String> {
    if path.contains(&0)
        || path
            .split(|byte| *byte == b'/')
            .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err("noncanonical path in Git index invalidation".to_owned());
    }
    Ok(())
}

fn decode_bitmap(bytes: &[u8], entries: u32) -> Result<Vec<u64>, String> {
    let size = read_be_u32(bytes)? as usize;
    if size.checked_add(4) != Some(bytes.len()) || size < 20 {
        return Err("invalid Git fsmonitor EWAH length".to_owned());
    }
    let bits = read_be_u32(&bytes[4..])?;
    let compressed_words = read_be_u32(&bytes[8..])? as usize;
    if bits > entries
        || compressed_words == 0
        || compressed_words
            .checked_mul(8)
            .and_then(|n| n.checked_add(12))
            != Some(size)
    {
        return Err("invalid Git fsmonitor EWAH dimensions".to_owned());
    }
    let word_bytes = &bytes[12..bytes.len() - 4];
    let word_at =
        |index: usize| u64::from_be_bytes(word_bytes[index * 8..index * 8 + 8].try_into().unwrap());
    let rlw_position = read_be_u32(&bytes[bytes.len() - 4..])? as usize;
    let logical_words = (bits as usize).div_ceil(64);
    let mut dirty = vec![0; (entries as usize).div_ceil(64)];
    let mut compressed = 0usize;
    let mut expanded = 0usize;
    let mut last_rlw = 0usize;
    while compressed < compressed_words {
        last_rlw = compressed;
        let rlw = word_at(compressed);
        compressed += 1;
        let run_words = ((rlw >> 1) & u64::from(u32::MAX)) as usize;
        let literal_words = (rlw >> 33) as usize;
        let expanded_end = expanded
            .checked_add(run_words)
            .and_then(|n| n.checked_add(literal_words))
            .filter(|n| *n <= logical_words)
            .ok_or_else(|| "Git fsmonitor EWAH run exceeds its bit length".to_owned())?;
        let compressed_end = compressed
            .checked_add(literal_words)
            .filter(|n| *n <= compressed_words)
            .ok_or_else(|| "truncated Git fsmonitor EWAH literals".to_owned())?;
        dirty[expanded..expanded + run_words].fill(if rlw & 1 == 0 { 0 } else { u64::MAX });
        expanded += run_words;
        for word in &mut dirty[expanded..expanded_end] {
            *word = word_at(compressed);
            compressed += 1;
        }
        debug_assert_eq!(compressed, compressed_end);
        expanded = expanded_end;
    }
    if expanded != logical_words || rlw_position != last_rlw {
        return Err("invalid Git fsmonitor EWAH run directory".to_owned());
    }
    if bits % 64 != 0 && dirty[logical_words - 1] >> (bits % 64) != 0 {
        return Err("Git fsmonitor EWAH has dirty bits beyond its bit length".to_owned());
    }
    Ok(dirty)
}

fn encode_bitmap(entries: u32, dirty: &[u64]) -> Result<Vec<u8>, String> {
    let mut words = Vec::new();
    let mut offset = 0usize;
    // Mask the final word here so Full does not describe nonexistent entries.
    let word_at = |index: usize| {
        if index + 1 == dirty.len() && entries % 64 != 0 {
            dirty[index] & ((1 << (entries % 64)) - 1)
        } else {
            dirty[index]
        }
    };
    let last_rlw = loop {
        let last_rlw = words.len();
        words.push(0);
        let mut run_words = 0u64;
        let run_bit = offset < dirty.len() && word_at(offset) == u64::MAX;
        let run_value = if run_bit { u64::MAX } else { 0 };
        while offset < dirty.len() && word_at(offset) == run_value {
            run_words += 1;
            offset += 1;
        }
        let mut literal_words = 0u64;
        while offset < dirty.len() && !matches!(word_at(offset), 0 | u64::MAX) {
            words.push(word_at(offset));
            literal_words += 1;
            offset += 1;
        }
        words[last_rlw] = u64::from(run_bit) | (run_words << 1) | (literal_words << 33);
        if offset == dirty.len() {
            break last_rlw;
        }
    };
    let compressed_words =
        u32::try_from(words.len()).map_err(|_| "Git fsmonitor EWAH is too large")?;
    let size = compressed_words
        .checked_mul(8)
        .and_then(|n| n.checked_add(12))
        .ok_or("Git fsmonitor EWAH is too large")?;
    let mut bytes = Vec::with_capacity(size as usize + 4);
    bytes.extend_from_slice(&size.to_be_bytes());
    bytes.extend_from_slice(&entries.to_be_bytes());
    bytes.extend_from_slice(&compressed_words.to_be_bytes());
    for word in words {
        bytes.extend_from_slice(&word.to_be_bytes());
    }
    bytes.extend_from_slice(&(last_rlw as u32).to_be_bytes());
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_varint(bytes: &mut Vec<u8>, mut value: usize) {
        let mut encoded = vec![(value & 0x7f) as u8];
        while value >= 128 {
            value = (value >> 7) - 1;
            encoded.push(0x80 | (value & 0x7f) as u8);
        }
        bytes.extend(encoded.into_iter().rev());
    }

    fn index(version: u32, hash_len: usize, paths: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = b"DIRC".to_vec();
        bytes.extend_from_slice(&version.to_be_bytes());
        bytes.extend_from_slice(&(paths.len() as u32).to_be_bytes());
        let mut previous: &[u8] = &[];
        for path in paths {
            let start = bytes.len();
            bytes.resize(start + 40 + hash_len, 0);
            bytes[start + 24..start + 28].copy_from_slice(&0o100644u32.to_be_bytes());
            let flags = path.len().min(0x0fff) as u16 | if version == 3 { 0x4000 } else { 0 };
            bytes.extend_from_slice(&flags.to_be_bytes());
            if version == 3 {
                bytes.extend_from_slice(&0x4000u16.to_be_bytes());
            }
            if version == 4 {
                let common = previous
                    .iter()
                    .zip(path)
                    .take_while(|(a, b)| a == b)
                    .count();
                append_varint(&mut bytes, previous.len() - common);
                bytes.extend_from_slice(&path[common..]);
                previous = path;
            } else {
                bytes.extend_from_slice(path);
            }
            bytes.push(0);
            if version != 4 {
                bytes.resize(start + (bytes.len() - start).next_multiple_of(8), 0);
            }
        }
        bytes.resize(bytes.len() + hash_len, 0);
        bytes
    }

    fn raw_bitmap(bits: u32, words: &[u64], last_rlw: u32) -> Vec<u8> {
        let mut bytes = (12 + words.len() as u32 * 8).to_be_bytes().to_vec();
        bytes.extend_from_slice(&bits.to_be_bytes());
        bytes.extend_from_slice(&(words.len() as u32).to_be_bytes());
        for word in words {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
        bytes.extend_from_slice(&last_rlw.to_be_bytes());
        bytes
    }

    fn dirty_positions(bytes: &[u8], entries: u32) -> Vec<u32> {
        let words = decode_bitmap(bytes, entries).unwrap();
        (0..entries)
            .filter(|bit| words[*bit as usize / 64] & (1 << (bit % 64)) != 0)
            .collect()
    }

    #[test]
    fn invalidations_preserve_dirty_runs_and_literals() {
        let paths = (0..256)
            .map(|n| format!("file{n:03}").into_bytes())
            .collect::<Vec<_>>();
        // One zero word, literal 3, one all-one word, literal 2. The
        // persisted bit length may be smaller than the entry count.
        let inherited = raw_bitmap(194, &[2 | (1 << 33), 3, 3 | (1 << 33), 2], 2);
        let output = invalidate_fsmonitor_bitmap(
            &index(2, 20, &paths),
            20,
            &inherited,
            &Invalidation::ExactPaths(vec![b"file000".to_vec(), b"file254".to_vec()]),
        )
        .unwrap();
        let expected = [0, 64, 65]
            .into_iter()
            .chain(128..192)
            .chain([193, 254])
            .collect::<Vec<_>>();
        assert_eq!(dirty_positions(&output, 256), expected);
    }

    #[test]
    fn path_invalidation_handles_renames_directories_and_non_utf8() {
        let paths = [
            b"keep".as_slice(),
            b"new/file",
            b"old/file",
            b"old/sub/file",
            b"older/file",
            b"raw\xff",
            b"raw\xff/nested",
        ]
        .map(Vec::from);
        for hash_len in [20, 32] {
            for version in [2, 3, 4] {
                for invalidation in [
                    Invalidation::ExactPaths(vec![
                        b"new".to_vec(),
                        b"old/".to_vec(),
                        b"raw\xff".to_vec(),
                    ]),
                    Invalidation::Prefixes(vec![
                        b"new".to_vec(),
                        b"old/".to_vec(),
                        b"raw\xff".to_vec(),
                    ]),
                ] {
                    let output = invalidate_fsmonitor_bitmap(
                        &index(version, hash_len, &paths),
                        hash_len,
                        &raw_bitmap(1, &[1 << 33, 1], 0),
                        &invalidation,
                    )
                    .unwrap();
                    assert_eq!(dirty_positions(&output, 7), [0, 1, 2, 3, 5, 6]);
                }
            }
        }
    }

    #[test]
    fn empty_replay_preserves_bitmap_and_full_or_root_invalidates_every_entry() {
        let paths = (0..65)
            .map(|n| format!("file{n}").into_bytes())
            .collect::<Vec<_>>();
        let bytes = index(2, 20, &paths);
        let inherited = raw_bitmap(0, &[0], 0);
        assert_eq!(
            invalidate_fsmonitor_bitmap(&bytes, 20, &inherited, &Invalidation::ExactPaths(vec![]))
                .unwrap(),
            inherited
        );
        for invalidation in [
            Invalidation::Full,
            Invalidation::Prefixes(vec![vec![]]),
            Invalidation::ExactPaths(vec![b"/".to_vec()]),
        ] {
            let output =
                invalidate_fsmonitor_bitmap(&bytes, 20, &inherited, &invalidation).unwrap();
            assert_eq!(dirty_positions(&output, 65), (0..65).collect::<Vec<_>>());
        }
        assert_eq!(
            dirty_positions(
                &invalidate_fsmonitor_bitmap(
                    &index(2, 20, &[]),
                    20,
                    &inherited,
                    &Invalidation::Full
                )
                .unwrap(),
                0
            ),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn v4_paths_decode_multibyte_strip_lengths_and_long_names() {
        let paths = [vec![b'a'; 5000], b"b/nested".to_vec(), b"b/other".to_vec()];
        let output = invalidate_fsmonitor_bitmap(
            &index(4, 20, &paths),
            20,
            &raw_bitmap(0, &[0], 0),
            &Invalidation::ExactPaths(vec![b"b/other".to_vec()]),
        )
        .unwrap();
        assert_eq!(dirty_positions(&output, 3), [2]);
    }

    #[test]
    fn every_stage_of_a_path_is_invalidated() {
        let mut bytes = index(
            2,
            20,
            &[
                b"conflict".to_vec(),
                b"conflict".to_vec(),
                b"conflict".to_vec(),
            ],
        );
        for stage in 1..=3 {
            let offset = 12 + (stage - 1) * 72 + 60;
            bytes[offset..offset + 2].copy_from_slice(&(8 | ((stage as u16) << 12)).to_be_bytes());
        }
        let output = invalidate_fsmonitor_bitmap(
            &bytes,
            20,
            &raw_bitmap(0, &[0], 0),
            &Invalidation::ExactPaths(vec![b"conflict".to_vec()]),
        )
        .unwrap();
        assert_eq!(dirty_positions(&output, 3), [0, 1, 2]);
    }

    #[test]
    fn split_sparse_and_unknown_required_extensions_are_rejected() {
        for signature in [b"link", b"sdir", b"next"] {
            let mut bytes = index(2, 20, &[b"file".to_vec()]);
            bytes.truncate(bytes.len() - 20);
            bytes.extend_from_slice(signature);
            bytes.extend_from_slice(&0u32.to_be_bytes());
            bytes.resize(bytes.len() + 20, 0);
            assert!(
                invalidate_fsmonitor_bitmap(
                    &bytes,
                    20,
                    &raw_bitmap(0, &[0], 0),
                    &Invalidation::Full
                )
                .is_err()
            );
        }
        let mut sparse = index(2, 20, &[b"dir/".to_vec()]);
        sparse[36..40].copy_from_slice(&0o040000u32.to_be_bytes());
        assert!(
            invalidate_fsmonitor_bitmap(&sparse, 20, &raw_bitmap(0, &[0], 0), &Invalidation::Full)
                .is_err()
        );
    }

    #[test]
    fn malformed_bitmaps_cannot_silently_clear_dirty_entries() {
        let bytes = index(2, 20, &[b"file".to_vec()]);
        let valid = raw_bitmap(1, &[1 << 33, 1], 0);
        let mut malformed = vec![
            raw_bitmap(2, &[1 << 33, 1], 0), // more bits than entries
            raw_bitmap(1, &[0], 0),          // insufficient expanded words
            raw_bitmap(1, &[4], 0),          // oversized zero run
            raw_bitmap(1, &[1 << 33], 0),    // missing literal
            raw_bitmap(1, &[1 << 33, 1], 1), // RLW points at literal
            raw_bitmap(1, &[1 << 33, 2], 0), // set bit beyond bit length
            raw_bitmap(0, &[], 0),           // no RLW
        ];
        malformed.extend((0..valid.len()).map(|end| valid[..end].to_vec()));
        for bitmap in malformed {
            assert!(
                invalidate_fsmonitor_bitmap(&bytes, 20, &bitmap, &Invalidation::ExactPaths(vec![]))
                    .is_err(),
                "accepted malformed bitmap: {bitmap:?}"
            );
        }
    }

    #[test]
    fn malformed_paths_and_v4_prefixes_are_rejected() {
        let invalidation = Invalidation::ExactPaths(vec![b"file".to_vec()]);
        for version in [2, 3, 4] {
            for path in [
                b"../file".as_slice(),
                b"/file",
                b"dir//file",
                b"dir/./file",
                b"bad\0name",
            ] {
                assert!(
                    invalidate_fsmonitor_bitmap(
                        &index(version, 20, &[path.to_vec()]),
                        20,
                        &raw_bitmap(0, &[0], 0),
                        &invalidation
                    )
                    .is_err()
                );
            }
        }
        let original = index(4, 20, &[b"file".to_vec()]);
        let mut too_long = original.clone();
        too_long[74] = 1;
        let mut overflowing = original.clone();
        overflowing.splice(74..75, [0xff; 20]);
        let mut wrong_length = original;
        wrong_length[73] = 3;
        for bytes in [too_long, overflowing, wrong_length] {
            assert!(
                invalidate_fsmonitor_bitmap(&bytes, 20, &raw_bitmap(0, &[0], 0), &invalidation)
                    .is_err()
            );
        }
        let mut unsupported_flags = index(3, 20, &[b"file".to_vec()]);
        unsupported_flags[75] |= 1;
        assert!(
            invalidate_fsmonitor_bitmap(
                &unsupported_flags,
                20,
                &raw_bitmap(0, &[0], 0),
                &invalidation
            )
            .is_err()
        );
        let bytes = index(2, 20, &[b"file".to_vec()]);
        for path in [b"../file".as_slice(), b"dir//file", b"bad\0name"] {
            assert!(
                invalidate_fsmonitor_bitmap(
                    &bytes,
                    20,
                    &raw_bitmap(0, &[0], 0),
                    &Invalidation::ExactPaths(vec![path.to_vec()])
                )
                .is_err()
            );
        }
    }
}
