//! Canonical target-object materialization for changed-object streams.
//!
//! Complete snapshot path/inode indexing lives in `snapshot_walk`; this module
//! only converts kernel-provided target metadata into the durable object shape.

use crate::index::{MODE_TYPE_MASK, Object};
use crate::manifest::TargetObjectMetadata;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

pub const PRIVILEGE_SETUID: u64 = 1 << 0;
pub const PRIVILEGE_SETGID: u64 = 1 << 1;
pub const PRIVILEGE_DEVICE: u64 = 1 << 2;
pub const PRIVILEGE_CAPABILITY: u64 = 1 << 3;
pub const PRIVILEGE_SECURITY_XATTR: u64 = 1 << 4;
pub const PRIVILEGE_TRUSTED_XATTR: u64 = 1 << 5;
pub const PRIVILEGE_FSCRYPT: u64 = 1 << 6;

#[derive(Clone, Debug, Eq, PartialEq)]
struct RawObject {
    ino: u64,
    generation: u64,
    mode: u32,
    nlink: u32,
    uid: u64,
    gid: u64,
    rdev: u64,
}

#[derive(Debug)]
pub struct TargetObjectError {
    message: String,
}

impl TargetObjectError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TargetObjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TargetObjectError {}

/// Converts the target inode fields and exact security-relevant xattr set from
/// the changed-objects v2 stream into the canonical userspace object row.
pub fn materialize_stream_object(
    ino: u64,
    metadata: &TargetObjectMetadata,
    xattrs: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<Object, TargetObjectError> {
    let mode = u32::try_from(metadata.mode)
        .map_err(|_| TargetObjectError::new(format!("inode {ino} mode exceeds u32")))?;
    let nlink = u32::try_from(metadata.nlink)
        .map_err(|_| TargetObjectError::new(format!("inode {ino} link count exceeds u32")))?;
    Ok(materialize_object(
        RawObject {
            ino,
            generation: metadata.generation,
            mode,
            nlink,
            uid: metadata.uid,
            gid: metadata.gid,
            rdev: metadata.rdev,
        },
        xattrs,
    ))
}

fn materialize_object(raw: RawObject, xattrs: &BTreeMap<Vec<u8>, Vec<u8>>) -> Object {
    let (xattr_flags, security_xattr_hash) = classify_xattrs(xattrs);
    let mut privilege_flags = xattr_flags;
    if raw.mode & 0o4000 != 0 {
        privilege_flags |= PRIVILEGE_SETUID;
    }
    if raw.mode & 0o2000 != 0 {
        privilege_flags |= PRIVILEGE_SETGID;
    }
    if matches!(raw.mode & MODE_TYPE_MASK, 0o020000 | 0o060000) {
        privilege_flags |= PRIVILEGE_DEVICE;
    }
    Object {
        ino: raw.ino,
        generation: raw.generation,
        mode: raw.mode,
        nlink: raw.nlink,
        uid: raw.uid,
        gid: raw.gid,
        rdev: raw.rdev,
        privilege_flags,
        security_xattr_hash,
    }
}

fn classify_xattrs(xattrs: &BTreeMap<Vec<u8>, Vec<u8>>) -> (u64, [u8; 32]) {
    let mut flags = 0_u64;
    let mut hash = Sha256::new();
    hash.update(b"btrfs-awacs-security-xattrs-v1\0");
    for (name, value) in xattrs {
        let relevant =
            if name.as_slice() == b"fscrypt.context" || name.as_slice() == b"security.fscrypt" {
                flags |= PRIVILEGE_FSCRYPT;
                true
            } else if name.as_slice() == b"security.capability" {
                flags |= PRIVILEGE_CAPABILITY | PRIVILEGE_SECURITY_XATTR;
                true
            } else if name.starts_with(b"security.") {
                flags |= PRIVILEGE_SECURITY_XATTR;
                true
            } else if name.starts_with(b"trusted.") {
                flags |= PRIVILEGE_TRUSTED_XATTR;
                true
            } else {
                false
            };
        if relevant {
            hash.update((name.len() as u64).to_be_bytes());
            hash.update(name);
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value);
        }
    }
    (flags, hash.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_target_metadata_and_security_xattrs() {
        let metadata = TargetObjectMetadata {
            generation: 7,
            change_sequence: 8,
            transid: 9,
            mode: 0o100_600,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
        };
        let xattrs = BTreeMap::from([(b"security.capability".to_vec(), b"value".to_vec())]);
        let object = materialize_stream_object(300, &metadata, &xattrs).unwrap();
        assert_eq!(object.ino, 300);
        assert_eq!(object.generation, 7);
        assert_eq!(object.uid, 1000);
        assert!(object.privilege_flags & PRIVILEGE_CAPABILITY != 0);
    }
}
