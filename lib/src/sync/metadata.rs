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

//! Exact commit-metadata identities and transient prefix reconciliation.
//!
//! Prefixes refer to the physical commit ID, while fingerprints include the
//! exact jj sidecar bytes. A shared Git commit alone does not establish that
//! two repositories have the same jj interpretation of it.

use std::collections::BTreeMap;
use std::collections::btree_map;
use std::ops::Bound;

use blake2::Blake2b;
use blake2::Digest as _;
use blake2::digest::consts::U32;
use serde::Deserialize;
use serde::Serialize;

use super::SyncError;
use super::SyncResult;
use crate::git_backend::GitCommitMetadata;
use crate::hex_util::encode_hex;

type MetadataHasher = Blake2b<U32>;

/// Content identity of one commit ID and its exact serialized jj sidecar.
pub type MetadataFingerprint = [u8; 32];

/// Commit sidecars established to be identical at both endpoints.
pub type KnownCommitMetadata = BTreeMap<gix::ObjectId, MetadataFingerprint>;

/// Hashes the commit identity and length-delimited sidecar with a distinct domain.
pub fn fingerprint(metadata: &GitCommitMetadata) -> MetadataFingerprint {
    let mut hasher = MetadataHasher::new();
    hasher.update(b"jj-sync-commit-metadata-v1\0");
    hasher.update([metadata.id.as_bytes().len() as u8]);
    hasher.update(metadata.id.as_bytes());
    hasher.update((metadata.extras.len() as u64).to_le_bytes());
    hasher.update(&metadata.extras);
    hasher.finalize().into()
}

/// An aggregate over sorted commit IDs and exact metadata fingerprints.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataSummary {
    /// A canonical lowercase hexadecimal commit-ID prefix, possibly empty.
    pub prefix: String,
    /// Number of distinct commit IDs covered by the prefix.
    pub count: usize,
    /// Canonical lowercase hexadecimal BLAKE2b-256 aggregate.
    pub digest: String,
}

impl MetadataSummary {
    /// Checks the wire encoding; callers separately enforce their record budget.
    pub fn validate(&self, hash: gix::hash::Kind) -> SyncResult<()> {
        validate_prefix(&self.prefix, hash)?;
        if self.digest.len() != 64 || !is_lower_hex(self.digest.as_bytes()) {
            return Err(invalid("invalid commit metadata summary digest"));
        }
        Ok(())
    }
}

/// A batch's metadata identities, without any persisted trie or mutable state.
///
/// Only the entries are retained. Requested prefix ranges are hashed on demand,
/// so memory is linear in the number of commits rather than their ID length.
#[derive(Debug)]
pub struct MetadataInventory {
    hash: gix::hash::Kind,
    entries: KnownCommitMetadata,
}

impl MetadataInventory {
    /// Creates an empty inventory for one negotiated Git object format.
    pub fn new(hash: gix::hash::Kind) -> Self {
        Self {
            hash,
            entries: BTreeMap::new(),
        }
    }

    /// Adds an exact identity, rejecting conflicting duplicates or another format.
    pub fn insert(
        &mut self,
        id: gix::ObjectId,
        fingerprint: MetadataFingerprint,
    ) -> SyncResult<()> {
        if id.kind() != self.hash {
            return Err(invalid(
                "commit metadata inventory uses the wrong Git object format",
            ));
        }
        if self
            .entries
            .get(&id)
            .is_some_and(|existing| existing != &fingerprint)
        {
            return Err(invalid(
                "conflicting commit metadata fingerprints for one Git object",
            ));
        }
        self.entries.insert(id, fingerprint);
        Ok(())
    }

    /// Returns the number of distinct commit IDs.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the inventory has no commit identities.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Summarizes a canonical prefix using its sorted entries and exact bytes.
    pub fn summarize(&self, prefix: &str) -> SyncResult<MetadataSummary> {
        let entries = self.prefix_entries(prefix)?;
        let mut hasher = MetadataHasher::new();
        hasher.update(b"jj-sync-commit-metadata-prefix-v1\0");
        hasher.update([self.hash.len_in_bytes() as u8, prefix.len() as u8]);
        hasher.update(prefix.as_bytes());
        let mut count = 0usize;
        for (id, fingerprint) in entries {
            hasher.update(id.as_bytes());
            hasher.update(fingerprint);
            count += 1;
        }
        hasher.update((count as u64).to_le_bytes());
        Ok(MetadataSummary {
            prefix: prefix.to_owned(),
            count,
            digest: encode_hex(&hasher.finalize()),
        })
    }

    /// Adds a range after both endpoints have established an equal summary.
    pub fn append_matching(&self, prefix: &str, known: &mut KnownCommitMetadata) -> SyncResult<()> {
        for (id, fingerprint) in self.prefix_entries(prefix)? {
            known.insert(*id, *fingerprint);
        }
        Ok(())
    }

    fn prefix_entries(
        &self,
        prefix: &str,
    ) -> SyncResult<btree_map::Range<'_, gix::ObjectId, MetadataFingerprint>> {
        validate_prefix(prefix, self.hash)?;
        let mut lower = vec![b'0'; self.hash.len_in_bytes() * 2];
        lower[..prefix.len()].copy_from_slice(prefix.as_bytes());
        let lower_id = gix::ObjectId::from_hex(&lower)
            .map_err(|_| invalid("invalid commit metadata summary prefix"))?;
        let upper = prefix
            .as_bytes()
            .iter()
            .rposition(|byte| *byte != b'f')
            .map(|position| {
                let mut upper = lower;
                upper[position] = match upper[position] {
                    b'9' => b'a',
                    byte => byte + 1,
                };
                upper[position + 1..].fill(b'0');
                gix::ObjectId::from_hex(&upper)
                    .map(Bound::Excluded)
                    .map_err(|_| invalid("invalid commit metadata summary prefix"))
            })
            .transpose()?
            .unwrap_or(Bound::Unbounded);
        Ok(self.entries.range((Bound::Included(lower_id), upper)))
    }
}

fn validate_prefix(prefix: &str, hash: gix::hash::Kind) -> SyncResult<()> {
    if prefix.len() > hash.len_in_bytes() * 2 || !is_lower_hex(prefix.as_bytes()) {
        return Err(invalid("invalid commit metadata summary prefix"));
    }
    Ok(())
}

fn is_lower_hex(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|&byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn invalid(message: &str) -> SyncError {
    SyncError::InvalidData(message.to_owned())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::tests::TestResult;

    fn metadata(hash: gix::hash::Kind, prefix: &str, extras: &[u8]) -> GitCommitMetadata {
        let mut text = vec![b'0'; hash.len_in_bytes() * 2];
        text[..prefix.len()].copy_from_slice(prefix.as_bytes());
        GitCommitMetadata {
            id: gix::ObjectId::from_hex(&text).unwrap(),
            extras: extras.to_vec(),
        }
    }

    #[test_case(gix::hash::Kind::Sha1; "sha1")]
    #[test_case(gix::hash::Kind::Sha256; "sha256")]
    fn summaries_are_stable_sorted_and_domain_separated(hash: gix::hash::Kind) -> TestResult {
        let left = metadata(hash, "a1", &[1, 2]);
        let right = metadata(hash, "a2", &[3, 4]);
        let mut ascending = MetadataInventory::new(hash);
        ascending.insert(left.id, fingerprint(&left))?;
        ascending.insert(right.id, fingerprint(&right))?;
        let mut descending = MetadataInventory::new(hash);
        descending.insert(right.id, fingerprint(&right))?;
        descending.insert(left.id, fingerprint(&left))?;
        assert_eq!(ascending.len(), 2);
        assert!(!ascending.is_empty());
        assert_eq!(ascending.summarize("")?, descending.summarize("")?);
        let summary = ascending.summarize("a")?;
        assert_eq!(summary.count, 2);
        summary.validate(hash)?;
        assert_ne!(summary.digest, ascending.summarize("")?.digest);
        assert_eq!(ascending.summarize(&left.id.to_string())?.count, 1);
        assert_eq!(ascending.summarize("f")?.count, 0);
        assert_ne!(
            ascending.summarize("e")?.digest,
            ascending.summarize("f")?.digest
        );
        Ok(())
    }

    #[test]
    fn exact_sidecar_bytes_and_commit_ids_affect_fingerprints() -> TestResult {
        let original = metadata(gix::hash::Kind::Sha1, "aa", &[1, 2]);
        let changed = GitCommitMetadata {
            extras: vec![2, 1],
            ..original.clone()
        };
        let other_id = metadata(gix::hash::Kind::Sha1, "ab", &original.extras);
        assert_ne!(fingerprint(&original), fingerprint(&changed));
        assert_ne!(fingerprint(&original), fingerprint(&other_id));
        let mut left = MetadataInventory::new(gix::hash::Kind::Sha1);
        let mut right = MetadataInventory::new(gix::hash::Kind::Sha1);
        left.insert(original.id, fingerprint(&original))?;
        right.insert(changed.id, fingerprint(&changed))?;
        assert_ne!(left.summarize("")?.digest, right.summarize("")?.digest);
        assert!(left.insert(changed.id, fingerprint(&changed)).is_err());
        Ok(())
    }

    #[test_case(gix::hash::Kind::Sha1; "sha1")]
    #[test_case(gix::hash::Kind::Sha256; "sha256")]
    fn disjoint_prefix_ranges_cover_only_matching_entries(hash: gix::hash::Kind) -> TestResult {
        let mut inventory = MetadataInventory::new(hash);
        let entries =
            ["09", "0a", "0f", "10", "af", "b0", "ff"].map(|prefix| metadata(hash, prefix, &[1]));
        for entry in &entries {
            inventory.insert(entry.id, fingerprint(entry))?;
        }
        assert_eq!(inventory.summarize("0")?.count, 3);
        assert_eq!(inventory.summarize("0f")?.count, 1);
        assert_eq!(inventory.summarize("af")?.count, 1);
        assert_eq!(inventory.summarize("f")?.count, 1);
        let mut known = KnownCommitMetadata::new();
        for prefix in ["0", "1", "a", "b", "f"] {
            inventory.append_matching(prefix, &mut known)?;
        }
        assert_eq!(known.len(), entries.len());
        let mut subset = KnownCommitMetadata::new();
        inventory.append_matching("0a", &mut subset)?;
        assert_eq!(subset.len(), 1);
        assert!(subset.contains_key(&entries[1].id));
        Ok(())
    }

    #[test_case(gix::hash::Kind::Sha1; "sha1")]
    #[test_case(gix::hash::Kind::Sha256; "sha256")]
    fn malformed_prefixes_and_summaries_are_rejected(hash: gix::hash::Kind) -> TestResult {
        let inventory = MetadataInventory::new(hash);
        assert!(inventory.is_empty());
        for prefix in [
            "A",
            "g",
            " a",
            "a/",
            "é",
            &"0".repeat(hash.len_in_bytes() * 2 + 1),
        ] {
            assert!(inventory.summarize(prefix).is_err());
            assert!(
                inventory
                    .append_matching(prefix, &mut BTreeMap::new())
                    .is_err()
            );
        }
        let summary = inventory.summarize("")?;
        for digest in [
            String::new(),
            "0".repeat(63),
            "0".repeat(65),
            "A".repeat(64),
        ] {
            let malformed = MetadataSummary {
                digest,
                ..summary.clone()
            };
            assert!(malformed.validate(hash).is_err());
        }
        let other = if hash == gix::hash::Kind::Sha1 {
            gix::hash::Kind::Sha256
        } else {
            gix::hash::Kind::Sha1
        };
        let entry = metadata(other, "aa", &[1]);
        let mut inventory = inventory;
        assert!(inventory.insert(entry.id, fingerprint(&entry)).is_err());
        Ok(())
    }
}
