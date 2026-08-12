//! Privileged prototype full-index reader for the current kernel.
//!
//! This intentionally isolates private Btrfs tree formats behind one module.
//! The production interface in the design replaces it with the versioned
//! full-index stream, but Initialize needs an exact generation/ref index on the
//! prototype kernel rather than a permission-filtered pathname crawl.

use crate::index::{Index, Object, MODE_TYPE_MASK, ROOT_INO};
use crate::manifest::{Reference, TargetObjectMetadata};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd};

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const BTRFS_IOCTL_MAGIC: u32 = 0x94;

const fn ioctl_number(direction: u32, number: u32, size: usize) -> libc::c_ulong {
    ((direction << IOC_DIRSHIFT)
        | (BTRFS_IOCTL_MAGIC << IOC_TYPESHIFT)
        | (number << IOC_NRSHIFT)
        | ((size as u32) << IOC_SIZESHIFT)) as libc::c_ulong
}

const BTRFS_IOC_TREE_SEARCH_V2: libc::c_ulong = ioctl_number(
    IOC_READ | IOC_WRITE,
    17,
    size_of::<BtrfsIoctlSearchArgsV2>(),
);
const INODE_ITEM_KEY: u32 = 1;
const INODE_REF_KEY: u32 = 12;
const INODE_EXTREF_KEY: u32 = 13;
const XATTR_ITEM_KEY: u32 = 24;
const SEARCH_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_SEARCH_BUFFER_BYTES: usize = 16 * 1024 * 1024;
const SEARCH_HEADER_BYTES: usize = 32;
const INODE_ITEM_REQUIRED_BYTES: usize = 64;
const INODE_REF_HEADER_BYTES: usize = 10;
const INODE_EXTREF_HEADER_BYTES: usize = 18;
const DIR_ITEM_HEADER_BYTES: usize = 30;
const FT_XATTR: u8 = 8;
const FT_ENCRYPTED: u8 = 0x80;

pub const PRIVILEGE_SETUID: u64 = 1 << 0;
pub const PRIVILEGE_SETGID: u64 = 1 << 1;
pub const PRIVILEGE_DEVICE: u64 = 1 << 2;
pub const PRIVILEGE_CAPABILITY: u64 = 1 << 3;
pub const PRIVILEGE_SECURITY_XATTR: u64 = 1 << 4;
pub const PRIVILEGE_TRUSTED_XATTR: u64 = 1 << 5;
pub const PRIVILEGE_FSCRYPT: u64 = 1 << 6;

#[repr(C)]
#[derive(Clone, Copy)]
struct BtrfsIoctlSearchKey {
    tree_id: u64,
    min_objectid: u64,
    max_objectid: u64,
    min_offset: u64,
    max_offset: u64,
    min_transid: u64,
    max_transid: u64,
    min_type: u32,
    max_type: u32,
    nr_items: u32,
    unused: u32,
    unused1: u64,
    unused2: u64,
    unused3: u64,
    unused4: u64,
}

#[repr(C)]
struct BtrfsIoctlSearchArgsV2 {
    key: BtrfsIoctlSearchKey,
    buf_size: u64,
}

const _: [(); 104] = [(); size_of::<BtrfsIoctlSearchKey>()];
const _: [(); 112] = [(); size_of::<BtrfsIoctlSearchArgsV2>()];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SearchItemKey {
    objectid: u64,
    item_type: u32,
    offset: u64,
}

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
pub struct FullIndexError {
    message: String,
    raw_os_error: Option<i32>,
}

impl FullIndexError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            raw_os_error: None,
        }
    }

    fn io(context: &str) -> Self {
        let error = std::io::Error::last_os_error();
        Self {
            message: format!("{context}: {error}"),
            raw_os_error: error.raw_os_error(),
        }
    }

    pub fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }
}

impl fmt::Display for FullIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FullIndexError {}

pub fn read_full_index(root: BorrowedFd<'_>) -> Result<Index, FullIndexError> {
    let mut objects = BTreeMap::new();
    let mut references = BTreeSet::new();
    let mut xattrs: BTreeMap<u64, BTreeMap<Vec<u8>, Vec<u8>>> = BTreeMap::new();

    search_items(root, SearchItemKey::MIN, SearchItemKey::MAX, |key, data| {
        match key.item_type {
            INODE_ITEM_KEY => {
                let object = parse_inode_item(key.objectid, data)?;
                if objects.insert(object.ino, object).is_some() {
                    return Err(FullIndexError::new(format!(
                        "full index contains duplicate inode item {}",
                        key.objectid
                    )));
                }
            }
            INODE_REF_KEY => {
                parse_inode_refs(key.objectid, key.offset, data, &mut references)?;
            }
            INODE_EXTREF_KEY => {
                parse_inode_extrefs(key.objectid, data, &mut references)?;
            }
            XATTR_ITEM_KEY => parse_xattrs(key.objectid, data, &mut xattrs)?,
            _ => {}
        }
        Ok(())
    })?;

    for ino in xattrs.keys() {
        if !objects.contains_key(ino) {
            return Err(FullIndexError::new(format!(
                "xattr item names absent inode {ino}"
            )));
        }
    }
    let mut index = Index {
        objects: BTreeMap::new(),
        references,
    };
    for raw in objects.into_values() {
        let inode_xattrs = xattrs.remove(&raw.ino).unwrap_or_default();
        index
            .objects
            .insert(raw.ino, materialize_object(raw, &inode_xattrs));
    }
    index
        .validate()
        .map_err(|error| FullIndexError::new(format!("full index is invalid: {error}")))?;
    Ok(index)
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

/// Converts the target inode fields and exact security-relevant xattr set from
/// the changed-objects v2 stream into the canonical userspace object row.
pub fn materialize_stream_object(
    ino: u64,
    metadata: &TargetObjectMetadata,
    xattrs: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<Object, FullIndexError> {
    let mode = u32::try_from(metadata.mode)
        .map_err(|_| FullIndexError::new(format!("inode {ino} mode exceeds u32")))?;
    let nlink = u32::try_from(metadata.nlink)
        .map_err(|_| FullIndexError::new(format!("inode {ino} link count exceeds u32")))?;
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

/// Reads the inode item and security-relevant xattrs for an exact set of
/// objects.  This is the O(changed objects) companion to `read_full_index`.
pub fn read_target_objects(
    root: BorrowedFd<'_>,
    inodes: &BTreeSet<u64>,
) -> Result<BTreeMap<u64, Object>, FullIndexError> {
    let mut raw_objects = BTreeMap::new();
    let mut xattrs: BTreeMap<u64, BTreeMap<Vec<u8>, Vec<u8>>> = BTreeMap::new();
    for &ino in inodes {
        if ino == 0 {
            return Err(FullIndexError::new("target object set contains inode zero"));
        }
        let minimum = SearchItemKey {
            objectid: ino,
            item_type: 0,
            offset: 0,
        };
        let maximum = SearchItemKey {
            objectid: ino,
            item_type: u8::MAX.into(),
            offset: u64::MAX,
        };
        search_items(root, minimum, maximum, |key, data| {
            match key.item_type {
                INODE_ITEM_KEY => {
                    let object = parse_inode_item(key.objectid, data)?;
                    if raw_objects.insert(object.ino, object).is_some() {
                        return Err(FullIndexError::new(format!(
                            "target lookup contains duplicate inode item {}",
                            key.objectid
                        )));
                    }
                }
                XATTR_ITEM_KEY => parse_xattrs(key.objectid, data, &mut xattrs)?,
                _ => {}
            }
            Ok(())
        })?;
    }

    let mut objects = BTreeMap::new();
    for &ino in inodes {
        let raw = raw_objects.remove(&ino).ok_or_else(|| {
            FullIndexError::new(format!("target snapshot has no inode item for {ino}"))
        })?;
        let inode_xattrs = xattrs.remove(&ino).unwrap_or_default();
        objects.insert(ino, materialize_object(raw, &inode_xattrs));
    }
    Ok(objects)
}

impl SearchItemKey {
    const MIN: Self = Self {
        objectid: 0,
        item_type: 0,
        offset: 0,
    };

    const MAX: Self = Self {
        objectid: u64::MAX,
        item_type: u8::MAX as u32,
        offset: u64::MAX,
    };
}

fn search_items(
    root: BorrowedFd<'_>,
    mut minimum: SearchItemKey,
    maximum: SearchItemKey,
    mut consume: impl FnMut(SearchItemKey, &[u8]) -> Result<(), FullIndexError>,
) -> Result<(), FullIndexError> {
    if minimum > maximum {
        return Err(FullIndexError::new("tree-search range is inverted"));
    }
    let mut buffer_bytes = SEARCH_BUFFER_BYTES;
    loop {
        let header_bytes = size_of::<BtrfsIoctlSearchArgsV2>();
        let allocation_bytes = header_bytes
            .checked_add(buffer_bytes)
            .ok_or_else(|| FullIndexError::new("tree-search allocation overflow"))?;
        let word_count = allocation_bytes.div_ceil(size_of::<u64>());
        let mut words = vec![0_u64; word_count];
        let args = words.as_mut_ptr().cast::<BtrfsIoctlSearchArgsV2>();
        // SAFETY: words is aligned for the repr(C) header and has the complete
        // header plus declared output buffer for the ioctl call.
        unsafe {
            (*args).key = BtrfsIoctlSearchKey {
                tree_id: 0,
                min_objectid: minimum.objectid,
                max_objectid: maximum.objectid,
                min_offset: minimum.offset,
                max_offset: maximum.offset,
                min_transid: 0,
                max_transid: u64::MAX,
                min_type: minimum.item_type,
                max_type: maximum.item_type,
                nr_items: u32::MAX,
                unused: 0,
                unused1: 0,
                unused2: 0,
                unused3: 0,
                unused4: 0,
            };
            (*args).buf_size = buffer_bytes as u64;
        }
        // SAFETY: the request layout is asserted above and args points at the
        // aligned, writable allocation for the full call.
        let result = unsafe { libc::ioctl(root.as_raw_fd(), BTRFS_IOC_TREE_SEARCH_V2, args) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EOVERFLOW) {
                // SAFETY: the kernel updates buf_size in the live header on
                // EOVERFLOW without writing beyond the supplied allocation.
                let required = unsafe { (*args).buf_size };
                let required = usize::try_from(required)
                    .map_err(|_| FullIndexError::new("tree-search item exceeds usize"))?;
                if required <= buffer_bytes || required > MAX_SEARCH_BUFFER_BYTES {
                    return Err(FullIndexError::new(format!(
                        "tree-search requires invalid buffer size {required}"
                    )));
                }
                buffer_bytes = required;
                continue;
            }
            return Err(FullIndexError::io("BTRFS_IOC_TREE_SEARCH_V2"));
        }
        // SAFETY: the successful ioctl initialized the header and at most the
        // declared buffer. nr_items controls the bounded parser below.
        let returned = unsafe { (*args).key.nr_items } as usize;
        if returned == 0 {
            return Ok(());
        }
        let bytes = words_as_bytes(&words);
        let mut cursor = header_bytes;
        let end = header_bytes + buffer_bytes;
        let mut last_key = None;
        for _ in 0..returned {
            let header_end = cursor
                .checked_add(SEARCH_HEADER_BYTES)
                .ok_or_else(|| FullIndexError::new("tree-search header offset overflow"))?;
            if header_end > end || header_end > bytes.len() {
                return Err(FullIndexError::new(
                    "tree-search returned a truncated header",
                ));
            }
            let header = &bytes[cursor..header_end];
            let key = SearchItemKey {
                objectid: native_u64(&header[8..16]),
                offset: native_u64(&header[16..24]),
                item_type: native_u32(&header[24..28]),
            };
            let length = native_u32(&header[28..32]) as usize;
            let item_end = header_end
                .checked_add(length)
                .ok_or_else(|| FullIndexError::new("tree-search item offset overflow"))?;
            if item_end > end || item_end > bytes.len() {
                return Err(FullIndexError::new("tree-search returned a truncated item"));
            }
            if key < minimum || key > maximum || last_key.is_some_and(|last| key <= last) {
                return Err(FullIndexError::new(
                    "tree-search keys are out of range or not increasing",
                ));
            }
            consume(key, &bytes[header_end..item_end])?;
            last_key = Some(key);
            cursor = item_end;
        }
        let last_key = last_key.expect("nonempty result has a key");
        let Some(next) = next_key(last_key) else {
            return Ok(());
        };
        if next > maximum {
            return Ok(());
        }
        minimum = next;
    }
}

fn parse_inode_item(ino: u64, data: &[u8]) -> Result<RawObject, FullIndexError> {
    if data.len() < INODE_ITEM_REQUIRED_BYTES {
        return Err(FullIndexError::new(format!(
            "inode item {ino} has {} bytes, expected at least {INODE_ITEM_REQUIRED_BYTES}",
            data.len()
        )));
    }
    Ok(RawObject {
        ino,
        generation: little_u64(&data[0..8]),
        nlink: little_u32(&data[40..44]),
        uid: u64::from(little_u32(&data[44..48])),
        gid: u64::from(little_u32(&data[48..52])),
        mode: little_u32(&data[52..56]),
        rdev: little_u64(&data[56..64]),
    })
}

fn parse_inode_refs(
    ino: u64,
    parent_ino: u64,
    data: &[u8],
    references: &mut BTreeSet<Reference>,
) -> Result<(), FullIndexError> {
    let mut cursor = 0;
    while cursor < data.len() {
        let header_end = cursor + INODE_REF_HEADER_BYTES;
        if header_end > data.len() {
            return Err(FullIndexError::new("packed inode-ref header is truncated"));
        }
        let name_len = usize::from(little_u16(&data[cursor + 8..header_end]));
        let name_end = header_end
            .checked_add(name_len)
            .ok_or_else(|| FullIndexError::new("inode-ref name offset overflow"))?;
        if name_end > data.len() {
            return Err(FullIndexError::new("packed inode-ref name is truncated"));
        }
        insert_reference(ino, parent_ino, &data[header_end..name_end], references)?;
        cursor = name_end;
    }
    Ok(())
}

fn parse_inode_extrefs(
    ino: u64,
    data: &[u8],
    references: &mut BTreeSet<Reference>,
) -> Result<(), FullIndexError> {
    let mut cursor = 0;
    while cursor < data.len() {
        let header_end = cursor + INODE_EXTREF_HEADER_BYTES;
        if header_end > data.len() {
            return Err(FullIndexError::new(
                "packed inode-extref header is truncated",
            ));
        }
        let parent_ino = little_u64(&data[cursor..cursor + 8]);
        let name_len = usize::from(little_u16(&data[cursor + 16..header_end]));
        let name_end = header_end
            .checked_add(name_len)
            .ok_or_else(|| FullIndexError::new("inode-extref name offset overflow"))?;
        if name_end > data.len() {
            return Err(FullIndexError::new("packed inode-extref name is truncated"));
        }
        insert_reference(ino, parent_ino, &data[header_end..name_end], references)?;
        cursor = name_end;
    }
    Ok(())
}

fn insert_reference(
    ino: u64,
    parent_ino: u64,
    name: &[u8],
    references: &mut BTreeSet<Reference>,
) -> Result<(), FullIndexError> {
    // Btrfs stores a synthetic `..` inode-ref from the subvolume root back to
    // itself.  It is filesystem bookkeeping rather than a visible namespace
    // edge; retaining it would make every logical path cyclic.
    if ino == ROOT_INO && parent_ino == ROOT_INO && name == b".." {
        return Ok(());
    }
    let reference = Reference {
        ino,
        parent_ino,
        name: name.to_vec(),
    };
    if !references.insert(reference) {
        return Err(FullIndexError::new(
            "full index contains a duplicate reference",
        ));
    }
    Ok(())
}

fn parse_xattrs(
    ino: u64,
    data: &[u8],
    xattrs: &mut BTreeMap<u64, BTreeMap<Vec<u8>, Vec<u8>>>,
) -> Result<(), FullIndexError> {
    let mut cursor = 0;
    while cursor < data.len() {
        let header_end = cursor + DIR_ITEM_HEADER_BYTES;
        if header_end > data.len() {
            return Err(FullIndexError::new("packed xattr header is truncated"));
        }
        let data_len = usize::from(little_u16(&data[cursor + 25..cursor + 27]));
        let name_len = usize::from(little_u16(&data[cursor + 27..cursor + 29]));
        let item_type = data[cursor + 29];
        if item_type & !FT_ENCRYPTED != FT_XATTR {
            return Err(FullIndexError::new(format!(
                "xattr item has unexpected directory-item type {item_type:#x}"
            )));
        }
        let name_end = header_end
            .checked_add(name_len)
            .ok_or_else(|| FullIndexError::new("xattr name offset overflow"))?;
        let value_end = name_end
            .checked_add(data_len)
            .ok_or_else(|| FullIndexError::new("xattr value offset overflow"))?;
        if value_end > data.len() {
            return Err(FullIndexError::new("packed xattr name/value is truncated"));
        }
        let name = data[header_end..name_end].to_vec();
        let value = data[name_end..value_end].to_vec();
        let existing = xattrs.entry(ino).or_default().insert(name, value.clone());
        if existing.is_some_and(|existing| existing != value) {
            return Err(FullIndexError::new(
                "full index contains conflicting duplicate xattrs",
            ));
        }
        cursor = value_end;
    }
    Ok(())
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

fn next_key(key: SearchItemKey) -> Option<SearchItemKey> {
    if key.offset != u64::MAX {
        Some(SearchItemKey {
            offset: key.offset + 1,
            ..key
        })
    } else if key.item_type != u32::from(u8::MAX) {
        Some(SearchItemKey {
            objectid: key.objectid,
            item_type: key.item_type + 1,
            offset: 0,
        })
    } else if key.objectid != u64::MAX {
        Some(SearchItemKey {
            objectid: key.objectid + 1,
            item_type: 0,
            offset: 0,
        })
    } else {
        None
    }
}

fn words_as_bytes(words: &[u64]) -> &[u8] {
    // SAFETY: u8 has alignment 1 and the result spans the same initialized
    // allocation as the input words.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), std::mem::size_of_val(words)) }
}

fn native_u32(bytes: &[u8]) -> u32 {
    u32::from_ne_bytes(bytes.try_into().expect("fixed-width field"))
}

fn native_u64(bytes: &[u8]) -> u64 {
    u64::from_ne_bytes(bytes.try_into().expect("fixed-width field"))
}

fn little_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("fixed-width field"))
}

fn little_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("fixed-width field"))
}

fn little_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("fixed-width field"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::fd::AsFd;

    #[test]
    fn ioctl_layout_matches_linux_uapi() {
        assert_eq!(size_of::<BtrfsIoctlSearchKey>(), 104);
        assert_eq!(size_of::<BtrfsIoctlSearchArgsV2>(), 112);
        assert_eq!(BTRFS_IOC_TREE_SEARCH_V2, 0xc070_9411);
    }

    #[test]
    fn parses_packed_refs_and_extrefs_with_non_utf8_names() {
        let mut refs = BTreeSet::new();
        let mut packed = 1_u64.to_le_bytes().to_vec();
        packed.extend_from_slice(&3_u16.to_le_bytes());
        packed.extend_from_slice(b"one");
        packed.extend_from_slice(&2_u64.to_le_bytes());
        packed.extend_from_slice(&2_u16.to_le_bytes());
        packed.extend_from_slice(&[0xff, b'x']);
        parse_inode_refs(300, 256, &packed, &mut refs).unwrap();

        let mut extref = 400_u64.to_le_bytes().to_vec();
        extref.extend_from_slice(&3_u64.to_le_bytes());
        extref.extend_from_slice(&3_u16.to_le_bytes());
        extref.extend_from_slice(b"two");
        parse_inode_extrefs(300, &extref, &mut refs).unwrap();
        assert_eq!(refs.len(), 3);
        assert!(refs.contains(&Reference {
            ino: 300,
            parent_ino: 256,
            name: vec![0xff, b'x'],
        }));
    }

    #[test]
    fn omits_only_the_btrfs_root_dotdot_reference() {
        let mut refs = BTreeSet::new();
        insert_reference(ROOT_INO, ROOT_INO, b"..", &mut refs).unwrap();
        assert!(refs.is_empty());

        insert_reference(ROOT_INO, ROOT_INO, b"visible", &mut refs).unwrap();
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn parses_inode_and_security_xattrs() {
        let mut inode = vec![0_u8; 160];
        inode[0..8].copy_from_slice(&9_u64.to_le_bytes());
        inode[40..44].copy_from_slice(&1_u32.to_le_bytes());
        inode[44..48].copy_from_slice(&1000_u32.to_le_bytes());
        inode[48..52].copy_from_slice(&100_u32.to_le_bytes());
        inode[52..56].copy_from_slice(&0o104755_u32.to_le_bytes());
        let object = parse_inode_item(300, &inode).unwrap();
        assert_eq!(object.generation, 9);
        assert_eq!(object.uid, 1000);

        let mut xattrs = BTreeMap::new();
        xattrs.insert(b"security.capability".to_vec(), vec![1, 2]);
        let (flags, hash) = classify_xattrs(&xattrs);
        assert_ne!(flags & PRIVILEGE_CAPABILITY, 0);
        assert_ne!(hash, [0; 32]);
    }

    #[test]
    fn non_btrfs_fd_fails_without_parsing() {
        let file = File::open("/dev/null").unwrap();
        let error = read_full_index(file.as_fd()).unwrap_err();
        assert!(matches!(
            error.raw_os_error(),
            Some(libc::ENOTTY) | Some(libc::EINVAL) | Some(libc::EPERM)
        ));
    }
}
