use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const CHANGED_OBJECTS_MAGIC: &[u8; 16] = b"btrfs-changes\0\0\0";
pub const CHANGED_OBJECTS_VERSION: u32 = 1;
pub const CHANGED_OBJECTS_V2_MAGIC: &[u8; 16] = b"btrfs-objects-v2";
pub const CHANGED_OBJECTS_V2_VERSION: u32 = 2;

pub const CHANGE_INODE: u64 = 1 << 0;
pub const CHANGE_REF: u64 = 1 << 1;
pub const CHANGE_XATTR: u64 = 1 << 2;
pub const CHANGE_FILE_DATA: u64 = 1 << 3;
pub const CHANGE_VERITY: u64 = 1 << 4;
pub const CHANGE_CREATED: u64 = 1 << 5;
pub const CHANGE_DELETED: u64 = 1 << 6;
pub const CHANGE_DIR_ENTRIES: u64 = 1 << 7;
pub const CHANGE_MASK: u64 = CHANGE_INODE
    | CHANGE_REF
    | CHANGE_XATTR
    | CHANGE_FILE_DATA
    | CHANGE_VERITY
    | CHANGE_CREATED
    | CHANGE_DELETED
    | CHANGE_DIR_ENTRIES;

const HEADER_SIZE: usize = 24;
const RECORD_HEADER_SIZE: usize = 8;
const OBJECT_RECORD_SIZE: usize = 40;
const REF_RECORD_SIZE: usize = 24;
const RECORD_OBJECT: u16 = 1;
const RECORD_REF_ADD: u16 = 2;
const RECORD_REF_DELETE: u16 = 3;
const V2_HEADER_SIZE: usize = 112;
const V2_OBJECT_RECORD_SIZE: usize = 96;
const V2_XATTR_RESET_SIZE: usize = 16;
const V2_XATTR_RECORD_SIZE: usize = 24;
const V2_COMPLETION_SIZE: usize = 32;
const RECORD_XATTR_RESET: u16 = 4;
const RECORD_XATTR: u16 = 5;
const RECORD_BOUNDARY_ADD: u16 = 6;
const RECORD_BOUNDARY_DELETE: u16 = 7;
const RECORD_COMPLETION: u16 = 0xffff;
const RECORD_TARGET_VALID: u16 = 1;
const RECORD_OPTIONAL: u16 = 1 << 15;
const V2_BOUNDARY_RECORDS: u64 = 1 << 1;
const V2_DIRTY_WITNESS: u64 = 1 << 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangedObjectsV2Header {
    /// The kernel included every nested-subvolume DIR_INDEX transition.
    pub boundary_records: bool,
    /// The kernel promises persistent inode/directory mutation witnesses.
    pub dirty_witness: bool,
    pub fs_uuid: [u8; 16],
    pub source_uuid: [u8; 16],
    pub target_uuid: [u8; 16],
    pub source_ctransid: u64,
    pub target_ctransid: u64,
    pub source_root_id: u64,
    pub target_root_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetObjectMetadata {
    pub generation: u64,
    pub change_sequence: u64,
    pub transid: u64,
    pub mode: u64,
    pub nlink: u64,
    pub uid: u64,
    pub gid: u64,
    pub rdev: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedObjectsV2 {
    pub header: ChangedObjectsV2Header,
    pub manifest: ChangedObjectsManifest,
    pub target_objects: BTreeMap<u64, TargetObjectMetadata>,
    /// Present only for objects whose relevant target xattr set was reset.
    pub target_security_xattrs: BTreeMap<u64, BTreeMap<Vec<u8>, Vec<u8>>>,
    /// Effective endpoint boundary changes after cancelling packed-item noise.
    pub boundary_adds: BTreeSet<SubvolumeBoundary>,
    pub boundary_deletes: BTreeSet<SubvolumeBoundary>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SubvolumeBoundary {
    pub parent_ino: u64,
    pub child_root_id: u64,
    pub name: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectChange {
    pub ino: u64,
    pub old_generation: Option<u64>,
    pub new_generation: Option<u64>,
    pub change_mask: u64,
}

impl ObjectChange {
    pub fn is_created(self) -> bool {
        self.change_mask & CHANGE_CREATED != 0
    }

    pub fn is_deleted(self) -> bool {
        self.change_mask & CHANGE_DELETED != 0
    }

    pub fn is_replaced(self) -> bool {
        self.is_created() && self.is_deleted()
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Reference {
    pub ino: u64,
    pub parent_ino: u64,
    pub name: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedObjectsManifest {
    pub objects: BTreeMap<u64, ObjectChange>,
    /// References after cancelling packed-ref entries present on both sides.
    pub ref_adds: BTreeSet<Reference>,
    pub ref_deletes: BTreeSet<Reference>,
    pub raw_ref_adds: usize,
    pub raw_ref_deletes: usize,
}

impl ChangedObjectsManifest {
    pub fn canonical_hash(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"btrfs-awacs-changed-objects-v1\0");
        for change in self.objects.values() {
            hash.update(b"o");
            hash.update(change.ino.to_be_bytes());
            hash.update(change.old_generation.unwrap_or_default().to_be_bytes());
            hash.update(change.new_generation.unwrap_or_default().to_be_bytes());
            hash.update(change.change_mask.to_be_bytes());
        }
        for (kind, references) in [(b"a", &self.ref_adds), (b"d", &self.ref_deletes)] {
            for reference in references {
                hash.update(kind);
                hash.update(reference.ino.to_be_bytes());
                hash.update(reference.parent_ino.to_be_bytes());
                hash.update((reference.name.len() as u64).to_be_bytes());
                hash.update(&reference.name);
            }
        }
        hash.update((self.raw_ref_adds as u64).to_be_bytes());
        hash.update((self.raw_ref_deletes as u64).to_be_bytes());
        hash.finalize().into()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    message: String,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ParseError> {
    let bytes = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| ParseError::new("truncated u16 field"))?;
    Ok(u16::from_le_bytes(
        bytes.try_into().expect("length checked"),
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ParseError> {
    let bytes = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| ParseError::new("truncated u32 field"))?;
    Ok(u32::from_le_bytes(
        bytes.try_into().expect("length checked"),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ParseError> {
    let bytes = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| ParseError::new("truncated u64 field"))?;
    Ok(u64::from_le_bytes(
        bytes.try_into().expect("length checked"),
    ))
}

fn fixed_16(bytes: &[u8], offset: usize) -> Result<[u8; 16], ParseError> {
    bytes
        .get(offset..offset + 16)
        .ok_or_else(|| ParseError::new("truncated 16-byte field"))?
        .try_into()
        .map_err(|_| ParseError::new("invalid 16-byte field"))
}

fn crc32c(bytes: &[u8]) -> u32 {
    let mut table = [0_u32; 256];
    for (index, entry) in table.iter_mut().enumerate() {
        let mut value = index as u32;
        for _ in 0..8 {
            value = (value >> 1) ^ (0x82f6_3b78 & (0_u32.wrapping_sub(value & 1)));
        }
        *entry = value;
    }
    let mut crc = !0_u32;
    for byte in bytes {
        crc = table[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

pub fn parse_changed_objects_v2(bytes: &[u8]) -> Result<ChangedObjectsV2, ParseError> {
    if bytes.len() < V2_HEADER_SIZE + V2_COMPLETION_SIZE
        || bytes.get(..16) != Some(CHANGED_OBJECTS_V2_MAGIC)
    {
        return Err(ParseError::new(
            "changed-objects v2 header is absent or truncated",
        ));
    }
    if read_u32(bytes, 16)? != CHANGED_OBJECTS_V2_VERSION
        || usize::try_from(read_u32(bytes, 20)?).ok() != Some(V2_HEADER_SIZE)
    {
        return Err(ParseError::new("unsupported changed-objects v2 header"));
    }
    let stream_flags = read_u64(bytes, 24)?;
    if stream_flags & !(V2_BOUNDARY_RECORDS | V2_DIRTY_WITNESS) != 0 {
        return Err(ParseError::new("unknown changed-objects v2 stream flags"));
    }
    let header = ChangedObjectsV2Header {
        boundary_records: stream_flags & V2_BOUNDARY_RECORDS != 0,
        dirty_witness: stream_flags & V2_DIRTY_WITNESS != 0,
        fs_uuid: fixed_16(bytes, 32)?,
        source_uuid: fixed_16(bytes, 48)?,
        target_uuid: fixed_16(bytes, 64)?,
        source_ctransid: read_u64(bytes, 80)?,
        target_ctransid: read_u64(bytes, 88)?,
        source_root_id: read_u64(bytes, 96)?,
        target_root_id: read_u64(bytes, 104)?,
    };
    if header.fs_uuid == [0; 16]
        || header.target_uuid == [0; 16]
        || header.target_ctransid == 0
        || header.target_root_id == 0
        || header.source_uuid == [0; 16]
        || header.source_ctransid == 0
        || header.source_root_id == 0
    {
        return Err(ParseError::new(
            "invalid changed-objects v2 endpoint identity",
        ));
    }

    let mut objects = BTreeMap::new();
    let mut target_objects = BTreeMap::new();
    let mut raw_adds = BTreeSet::new();
    let mut raw_deletes = BTreeSet::new();
    let mut target_security_xattrs = BTreeMap::<u64, BTreeMap<Vec<u8>, Vec<u8>>>::new();
    let mut boundary_adds = BTreeSet::new();
    let mut boundary_deletes = BTreeSet::new();
    let mut offset = V2_HEADER_SIZE;
    let mut record_count = 0_u64;
    let completion_offset;
    loop {
        if offset + RECORD_HEADER_SIZE > bytes.len() {
            return Err(ParseError::new(
                "changed-objects v2 lacks a completion record",
            ));
        }
        let record_type = read_u16(bytes, offset)?;
        let flags = read_u16(bytes, offset + 2)?;
        let record_len = usize::try_from(read_u32(bytes, offset + 4)?)
            .map_err(|_| ParseError::new("v2 record length does not fit usize"))?;
        if record_len < RECORD_HEADER_SIZE {
            return Err(ParseError::new("invalid changed-objects v2 record length"));
        }
        let end = offset
            .checked_add(record_len)
            .ok_or_else(|| ParseError::new("changed-objects v2 record overflow"))?;
        if end > bytes.len() {
            return Err(ParseError::new("truncated changed-objects v2 record"));
        }
        if record_type == RECORD_COMPLETION {
            if flags != 0 || record_len != V2_COMPLETION_SIZE || end != bytes.len() {
                return Err(ParseError::new("invalid changed-objects v2 completion"));
            }
            completion_offset = offset;
            let declared_records = read_u64(bytes, offset + 8)?;
            let declared_bytes = usize::try_from(read_u64(bytes, offset + 16)?)
                .map_err(|_| ParseError::new("v2 stream byte count does not fit usize"))?;
            if declared_records != record_count
                || declared_bytes != completion_offset
                || read_u32(bytes, offset + 28)? != 0
                || crc32c(&bytes[..completion_offset]) != read_u32(bytes, offset + 24)?
            {
                return Err(ParseError::new("changed-objects v2 completion mismatch"));
            }
            break;
        }

        match record_type {
            RECORD_OBJECT => {
                if record_len != V2_OBJECT_RECORD_SIZE || flags & !RECORD_TARGET_VALID != 0 {
                    return Err(ParseError::new("invalid changed-object v2 object record"));
                }
                let ino = read_u64(bytes, offset + 8)?;
                let old_generation = read_u64(bytes, offset + 16)?;
                let new_generation = read_u64(bytes, offset + 24)?;
                let change_mask = read_u64(bytes, offset + 32)?;
                validate_object(ino, old_generation, new_generation, change_mask)?;
                if objects
                    .insert(
                        ino,
                        ObjectChange {
                            ino,
                            old_generation: (old_generation != 0).then_some(old_generation),
                            new_generation: (new_generation != 0).then_some(new_generation),
                            change_mask,
                        },
                    )
                    .is_some()
                {
                    return Err(ParseError::new("duplicate changed-object v2 inode"));
                }
                if flags & RECORD_TARGET_VALID != 0 {
                    let metadata = TargetObjectMetadata {
                        generation: new_generation,
                        change_sequence: read_u64(bytes, offset + 40)?,
                        transid: read_u64(bytes, offset + 48)?,
                        mode: read_u64(bytes, offset + 56)?,
                        nlink: read_u64(bytes, offset + 64)?,
                        uid: read_u64(bytes, offset + 72)?,
                        gid: read_u64(bytes, offset + 80)?,
                        rdev: read_u64(bytes, offset + 88)?,
                    };
                    if metadata.generation == 0
                        || metadata.change_sequence == 0
                        || metadata.transid == 0
                        || metadata.nlink == 0
                        || target_objects.insert(ino, metadata).is_some()
                    {
                        return Err(ParseError::new("invalid changed-object v2 target metadata"));
                    }
                }
            }
            RECORD_REF_ADD | RECORD_REF_DELETE => {
                if flags != 0
                    || !(REF_RECORD_SIZE + 1..=REF_RECORD_SIZE + 255).contains(&record_len)
                {
                    return Err(ParseError::new("invalid changed-object v2 reference"));
                }
                let reference = Reference {
                    ino: read_u64(bytes, offset + 8)?,
                    parent_ino: read_u64(bytes, offset + 16)?,
                    name: bytes[offset + REF_RECORD_SIZE..end].to_vec(),
                };
                validate_reference(&reference)?;
                let inserted = if record_type == RECORD_REF_ADD {
                    raw_adds.insert(reference)
                } else {
                    raw_deletes.insert(reference)
                };
                if !inserted {
                    return Err(ParseError::new("duplicate changed-object v2 reference"));
                }
            }
            RECORD_XATTR_RESET => {
                if flags != 0 || record_len != V2_XATTR_RESET_SIZE {
                    return Err(ParseError::new("invalid changed-object v2 xattr reset"));
                }
                let ino = read_u64(bytes, offset + 8)?;
                if ino == 0
                    || target_security_xattrs
                        .insert(ino, BTreeMap::new())
                        .is_some()
                {
                    return Err(ParseError::new("duplicate changed-object v2 xattr reset"));
                }
            }
            RECORD_XATTR => {
                if flags != 0 || record_len <= V2_XATTR_RECORD_SIZE {
                    return Err(ParseError::new("invalid changed-object v2 xattr"));
                }
                let ino = read_u64(bytes, offset + 8)?;
                let name_len = usize::try_from(read_u32(bytes, offset + 16)?)
                    .map_err(|_| ParseError::new("v2 xattr name length does not fit usize"))?;
                let value_len = usize::try_from(read_u32(bytes, offset + 20)?)
                    .map_err(|_| ParseError::new("v2 xattr value length does not fit usize"))?;
                if name_len == 0
                    || name_len > 255
                    || V2_XATTR_RECORD_SIZE + name_len + value_len != record_len
                {
                    return Err(ParseError::new("invalid changed-object v2 xattr lengths"));
                }
                let Some(xattrs) = target_security_xattrs.get_mut(&ino) else {
                    return Err(ParseError::new("v2 xattr appears before its reset"));
                };
                let name_start = offset + V2_XATTR_RECORD_SIZE;
                let value_start = name_start + name_len;
                if xattrs
                    .insert(
                        bytes[name_start..value_start].to_vec(),
                        bytes[value_start..end].to_vec(),
                    )
                    .is_some()
                {
                    return Err(ParseError::new("duplicate changed-object v2 xattr name"));
                }
            }
            RECORD_BOUNDARY_ADD | RECORD_BOUNDARY_DELETE => {
                if flags != 0
                    || !(REF_RECORD_SIZE + 1..=REF_RECORD_SIZE + 255).contains(&record_len)
                {
                    return Err(ParseError::new("invalid changed-object v2 boundary"));
                }
                let boundary = SubvolumeBoundary {
                    parent_ino: read_u64(bytes, offset + 8)?,
                    child_root_id: read_u64(bytes, offset + 16)?,
                    name: bytes[offset + REF_RECORD_SIZE..end].to_vec(),
                };
                if boundary.parent_ino == 0
                    || boundary.child_root_id == 0
                    || boundary.name.is_empty()
                    || boundary.name.contains(&b'/')
                    || boundary.name.contains(&b'\0')
                {
                    return Err(ParseError::new("invalid changed-object v2 boundary fields"));
                }
                let inserted = if record_type == RECORD_BOUNDARY_ADD {
                    boundary_adds.insert(boundary)
                } else {
                    boundary_deletes.insert(boundary)
                };
                if !inserted {
                    return Err(ParseError::new("duplicate changed-object v2 boundary"));
                }
            }
            _ => {
                if flags != RECORD_OPTIONAL {
                    return Err(ParseError::new(
                        "unknown mandatory changed-objects v2 record",
                    ));
                }
            }
        }
        record_count = record_count
            .checked_add(1)
            .ok_or_else(|| ParseError::new("changed-objects v2 record count overflow"))?;
        offset = end;
    }

    for reference in raw_adds.iter().chain(&raw_deletes) {
        if objects
            .get(&reference.ino)
            .is_none_or(|object| object.change_mask & CHANGE_REF == 0)
        {
            return Err(ParseError::new("v2 reference lacks a changed ref object"));
        }
    }
    for ino in target_security_xattrs.keys() {
        if objects.get(ino).is_none_or(|object| object.is_deleted())
            || !target_objects.contains_key(ino)
        {
            return Err(ParseError::new("v2 xattrs lack a surviving target object"));
        }
    }
    let raw_ref_adds = raw_adds.len();
    let raw_ref_deletes = raw_deletes.len();
    let common: Vec<_> = raw_adds.intersection(&raw_deletes).cloned().collect();
    for reference in common {
        raw_adds.remove(&reference);
        raw_deletes.remove(&reference);
    }
    let common_boundaries: Vec<_> = boundary_adds
        .intersection(&boundary_deletes)
        .cloned()
        .collect();
    for boundary in common_boundaries {
        boundary_adds.remove(&boundary);
        boundary_deletes.remove(&boundary);
    }
    Ok(ChangedObjectsV2 {
        header,
        manifest: ChangedObjectsManifest {
            objects,
            ref_adds: raw_adds,
            ref_deletes: raw_deletes,
            raw_ref_adds,
            raw_ref_deletes,
        },
        target_objects,
        target_security_xattrs,
        boundary_adds,
        boundary_deletes,
    })
}

pub fn parse_changed_objects(bytes: &[u8]) -> Result<ChangedObjectsManifest, ParseError> {
    if bytes.len() < HEADER_SIZE {
        return Err(ParseError::new(
            "changed-objects manifest has a truncated header",
        ));
    }
    if bytes.get(..CHANGED_OBJECTS_MAGIC.len()) != Some(CHANGED_OBJECTS_MAGIC) {
        return Err(ParseError::new(
            "changed-objects manifest has invalid magic",
        ));
    }
    let version = read_u32(bytes, 16)?;
    if version != CHANGED_OBJECTS_VERSION {
        return Err(ParseError::new(format!(
            "unsupported changed-objects manifest version {version}"
        )));
    }
    let header_len = usize::try_from(read_u32(bytes, 20)?)
        .map_err(|_| ParseError::new("changed-objects header length does not fit usize"))?;
    if header_len != HEADER_SIZE {
        return Err(ParseError::new(format!(
            "unsupported changed-objects header length {header_len}"
        )));
    }

    let mut objects = BTreeMap::new();
    let mut raw_adds = BTreeSet::new();
    let mut raw_deletes = BTreeSet::new();
    let mut offset = header_len;

    while offset < bytes.len() {
        let header_end = offset
            .checked_add(RECORD_HEADER_SIZE)
            .ok_or_else(|| ParseError::new("changed-objects record offset overflow"))?;
        if header_end > bytes.len() {
            return Err(ParseError::new(
                "changed-objects manifest has a truncated record header",
            ));
        }
        let record_type = read_u16(bytes, offset)?;
        let flags = read_u16(bytes, offset + 2)?;
        let record_len = usize::try_from(read_u32(bytes, offset + 4)?)
            .map_err(|_| ParseError::new("changed-objects record length does not fit usize"))?;
        if flags != 0 {
            return Err(ParseError::new(format!(
                "changed-objects record has unsupported flags {flags:#x}"
            )));
        }
        if record_len < RECORD_HEADER_SIZE {
            return Err(ParseError::new(format!(
                "changed-objects record has invalid length {record_len}"
            )));
        }
        let record_end = offset
            .checked_add(record_len)
            .ok_or_else(|| ParseError::new("changed-objects record length overflow"))?;
        if record_end > bytes.len() {
            return Err(ParseError::new(
                "changed-objects manifest has a truncated record",
            ));
        }

        match record_type {
            RECORD_OBJECT => {
                if record_len != OBJECT_RECORD_SIZE {
                    return Err(ParseError::new(format!(
                        "changed-object record has invalid length {record_len}"
                    )));
                }
                let ino = read_u64(bytes, offset + 8)?;
                let old_generation = read_u64(bytes, offset + 16)?;
                let new_generation = read_u64(bytes, offset + 24)?;
                let change_mask = read_u64(bytes, offset + 32)?;
                validate_object(ino, old_generation, new_generation, change_mask)?;
                let change = ObjectChange {
                    ino,
                    old_generation: (old_generation != 0).then_some(old_generation),
                    new_generation: (new_generation != 0).then_some(new_generation),
                    change_mask,
                };
                if objects.insert(ino, change).is_some() {
                    return Err(ParseError::new(format!(
                        "changed-objects manifest contains duplicate inode {ino}"
                    )));
                }
            }
            RECORD_REF_ADD | RECORD_REF_DELETE => {
                if !(REF_RECORD_SIZE + 1..=REF_RECORD_SIZE + 255).contains(&record_len) {
                    return Err(ParseError::new(format!(
                        "changed-reference record has invalid length {record_len}"
                    )));
                }
                let reference = Reference {
                    ino: read_u64(bytes, offset + 8)?,
                    parent_ino: read_u64(bytes, offset + 16)?,
                    name: bytes[offset + REF_RECORD_SIZE..record_end].to_vec(),
                };
                validate_reference(&reference)?;
                let inserted = if record_type == RECORD_REF_ADD {
                    raw_adds.insert(reference)
                } else {
                    raw_deletes.insert(reference)
                };
                if !inserted {
                    return Err(ParseError::new(
                        "changed-objects manifest contains a duplicate reference",
                    ));
                }
            }
            _ => {
                return Err(ParseError::new(format!(
                    "changed-objects manifest has unknown record type {record_type}"
                )));
            }
        }
        offset = record_end;
    }

    for reference in raw_adds.iter().chain(&raw_deletes) {
        let Some(object) = objects.get(&reference.ino) else {
            return Err(ParseError::new(
                "changed-reference record has no corresponding object",
            ));
        };
        if object.change_mask & CHANGE_REF == 0 {
            return Err(ParseError::new(
                "changed-reference object has no ref change",
            ));
        }
    }
    for object in objects.values() {
        if object.change_mask & CHANGE_REF != 0
            && !raw_adds.iter().any(|reference| reference.ino == object.ino)
            && !raw_deletes
                .iter()
                .any(|reference| reference.ino == object.ino)
        {
            return Err(ParseError::new(format!(
                "changed-reference object {} has no ref records",
                object.ino
            )));
        }
    }

    let raw_ref_adds = raw_adds.len();
    let raw_ref_deletes = raw_deletes.len();
    let common: Vec<_> = raw_adds.intersection(&raw_deletes).cloned().collect();
    for reference in common {
        raw_adds.remove(&reference);
        raw_deletes.remove(&reference);
    }

    Ok(ChangedObjectsManifest {
        objects,
        ref_adds: raw_adds,
        ref_deletes: raw_deletes,
        raw_ref_adds,
        raw_ref_deletes,
    })
}

fn validate_object(
    ino: u64,
    old_generation: u64,
    new_generation: u64,
    change_mask: u64,
) -> Result<(), ParseError> {
    if ino == 0 || change_mask == 0 || change_mask & !CHANGE_MASK != 0 {
        return Err(ParseError::new("changed-object record has invalid fields"));
    }
    let created = change_mask & CHANGE_CREATED != 0;
    let deleted = change_mask & CHANGE_DELETED != 0;
    if created && !deleted && (old_generation != 0 || new_generation == 0) {
        return Err(ParseError::new("created object has invalid generations"));
    }
    if deleted && !created && (old_generation == 0 || new_generation != 0) {
        return Err(ParseError::new("deleted object has invalid generations"));
    }
    if created
        && deleted
        && (old_generation == 0 || new_generation == 0 || old_generation == new_generation)
    {
        return Err(ParseError::new("replaced object has invalid generations"));
    }
    if !created && !deleted && ((old_generation == 0) != (new_generation == 0)) {
        return Err(ParseError::new("changed object has invalid generations"));
    }
    if !created
        && !deleted
        && change_mask & CHANGE_INODE != 0
        && (old_generation == 0 || new_generation == 0)
    {
        return Err(ParseError::new("inode-item change has no generations"));
    }
    if (created || deleted) && change_mask & CHANGE_INODE == 0 {
        return Err(ParseError::new(
            "created or deleted object has no inode change",
        ));
    }
    Ok(())
}

fn validate_reference(reference: &Reference) -> Result<(), ParseError> {
    if reference.ino == 0 || reference.parent_ino == 0 {
        return Err(ParseError::new(
            "changed-reference record has invalid inode",
        ));
    }
    if reference.name.is_empty()
        || reference.name.contains(&b'/')
        || reference.name.contains(&b'\0')
    {
        return Err(ParseError::new("changed-reference record has invalid name"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_u16(bytes: &mut Vec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u64(bytes: &mut Vec<u8>, value: u64) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn header() -> Vec<u8> {
        let mut bytes = CHANGED_OBJECTS_MAGIC.to_vec();
        bytes.extend_from_slice(&CHANGED_OBJECTS_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        bytes
    }

    fn push_object(bytes: &mut Vec<u8>, ino: u64, old: u64, new: u64, mask: u64) {
        bytes.extend_from_slice(&RECORD_OBJECT.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&(OBJECT_RECORD_SIZE as u32).to_le_bytes());
        bytes.extend_from_slice(&ino.to_le_bytes());
        bytes.extend_from_slice(&old.to_le_bytes());
        bytes.extend_from_slice(&new.to_le_bytes());
        bytes.extend_from_slice(&mask.to_le_bytes());
    }

    fn push_ref(bytes: &mut Vec<u8>, kind: u16, ino: u64, parent: u64, name: &[u8]) {
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&(REF_RECORD_SIZE as u32 + name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&ino.to_le_bytes());
        bytes.extend_from_slice(&parent.to_le_bytes());
        bytes.extend_from_slice(name);
    }

    fn v2_fixture() -> Vec<u8> {
        let mut bytes = CHANGED_OBJECTS_V2_MAGIC.to_vec();
        push_u32(&mut bytes, CHANGED_OBJECTS_V2_VERSION);
        push_u32(&mut bytes, V2_HEADER_SIZE as u32);
        push_u64(&mut bytes, V2_BOUNDARY_RECORDS | V2_DIRTY_WITNESS);
        bytes.extend_from_slice(&[1; 16]);
        bytes.extend_from_slice(&[2; 16]);
        bytes.extend_from_slice(&[3; 16]);
        push_u64(&mut bytes, 10);
        push_u64(&mut bytes, 11);
        push_u64(&mut bytes, 256);
        push_u64(&mut bytes, 257);

        push_u16(&mut bytes, RECORD_OBJECT);
        push_u16(&mut bytes, RECORD_TARGET_VALID);
        push_u32(&mut bytes, V2_OBJECT_RECORD_SIZE as u32);
        for value in [
            300,
            7,
            7,
            CHANGE_REF | CHANGE_XATTR,
            12,
            11,
            0o100_644,
            1,
            1000,
            1000,
            0,
        ] {
            push_u64(&mut bytes, value);
        }

        push_u16(&mut bytes, RECORD_REF_ADD);
        push_u16(&mut bytes, 0);
        push_u32(&mut bytes, (REF_RECORD_SIZE + 4) as u32);
        push_u64(&mut bytes, 300);
        push_u64(&mut bytes, 256);
        bytes.extend_from_slice(b"file");

        push_u16(&mut bytes, RECORD_XATTR_RESET);
        push_u16(&mut bytes, 0);
        push_u32(&mut bytes, V2_XATTR_RESET_SIZE as u32);
        push_u64(&mut bytes, 300);

        let name = b"security.test";
        let value = b"value";
        push_u16(&mut bytes, RECORD_XATTR);
        push_u16(&mut bytes, 0);
        push_u32(
            &mut bytes,
            (V2_XATTR_RECORD_SIZE + name.len() + value.len()) as u32,
        );
        push_u64(&mut bytes, 300);
        push_u32(&mut bytes, name.len() as u32);
        push_u32(&mut bytes, value.len() as u32);
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(value);

        push_u16(&mut bytes, 100);
        push_u16(&mut bytes, RECORD_OPTIONAL);
        push_u32(&mut bytes, 12);
        bytes.extend_from_slice(b"extn");

        let completion_offset = bytes.len();
        let checksum = crc32c(&bytes);
        push_u16(&mut bytes, RECORD_COMPLETION);
        push_u16(&mut bytes, 0);
        push_u32(&mut bytes, V2_COMPLETION_SIZE as u32);
        push_u64(&mut bytes, 5);
        push_u64(&mut bytes, completion_offset as u64);
        push_u32(&mut bytes, checksum);
        push_u32(&mut bytes, 0);
        bytes
    }

    #[test]
    fn parses_authenticated_v2_stream_and_skips_optional_extensions() {
        let bytes = v2_fixture();
        let parsed = parse_changed_objects_v2(&bytes).unwrap();
        assert_eq!(parsed.header.fs_uuid, [1; 16]);
        assert_eq!(parsed.header.source_uuid, [2; 16]);
        assert_eq!(parsed.header.target_uuid, [3; 16]);
        assert!(parsed.header.boundary_records);
        assert!(parsed.header.dirty_witness);
        assert_eq!(parsed.manifest.ref_adds.len(), 1);
        assert_eq!(parsed.target_objects.get(&300).unwrap().uid, 1000);
        assert_eq!(
            parsed.target_security_xattrs[&300][b"security.test".as_slice()],
            b"value"
        );
        assert!(parsed.boundary_adds.is_empty());

        let mut corrupt = bytes.clone();
        corrupt[V2_HEADER_SIZE + V2_OBJECT_RECORD_SIZE - 1] ^= 1;
        assert!(parse_changed_objects_v2(&corrupt).is_err());

        let mut mandatory = bytes;
        let extension = mandatory
            .windows(4)
            .position(|window| window == b"extn")
            .unwrap()
            - RECORD_HEADER_SIZE;
        mandatory[extension + 2..extension + 4].copy_from_slice(&0_u16.to_le_bytes());
        assert!(parse_changed_objects_v2(&mandatory).is_err());
    }

    #[test]
    fn parses_and_cancels_boundary_records() {
        let mut bytes = v2_fixture();
        bytes.truncate(bytes.len() - V2_COMPLETION_SIZE);
        for (kind, name) in [
            (RECORD_BOUNDARY_ADD, b"same".as_slice()),
            (RECORD_BOUNDARY_DELETE, b"same".as_slice()),
            (RECORD_BOUNDARY_ADD, b"nested".as_slice()),
        ] {
            push_u16(&mut bytes, kind);
            push_u16(&mut bytes, 0);
            push_u32(&mut bytes, (REF_RECORD_SIZE + name.len()) as u32);
            push_u64(&mut bytes, 256);
            push_u64(&mut bytes, 900);
            bytes.extend_from_slice(name);
        }
        let completion_offset = bytes.len();
        let checksum = crc32c(&bytes);
        push_u16(&mut bytes, RECORD_COMPLETION);
        push_u16(&mut bytes, 0);
        push_u32(&mut bytes, V2_COMPLETION_SIZE as u32);
        push_u64(&mut bytes, 8);
        push_u64(&mut bytes, completion_offset as u64);
        push_u32(&mut bytes, checksum);
        push_u32(&mut bytes, 0);

        let parsed = parse_changed_objects_v2(&bytes).unwrap();
        assert_eq!(parsed.boundary_adds.len(), 1);
        assert_eq!(parsed.boundary_adds.iter().next().unwrap().name, b"nested");
        assert!(parsed.boundary_deletes.is_empty());
    }

    #[test]
    fn parses_typed_manifest_and_cancels_packed_refs() {
        let mut bytes = header();
        push_ref(&mut bytes, RECORD_REF_ADD, 300, 256, b"same");
        push_ref(&mut bytes, RECORD_REF_DELETE, 300, 256, b"same");
        push_ref(&mut bytes, RECORD_REF_ADD, 300, 256, b"new");
        push_object(&mut bytes, 300, 7, 7, CHANGE_INODE | CHANGE_REF);
        push_object(&mut bytes, 301, 8, 0, CHANGE_INODE | CHANGE_DELETED);

        let manifest = parse_changed_objects(&bytes).unwrap();
        assert_eq!(manifest.objects.len(), 2);
        assert_eq!(manifest.raw_ref_adds, 2);
        assert_eq!(manifest.raw_ref_deletes, 1);
        assert_eq!(manifest.ref_adds.len(), 1);
        assert!(manifest.ref_deletes.is_empty());
        assert_eq!(manifest.ref_adds.iter().next().unwrap().name, b"new");
    }

    #[test]
    fn accepts_inode_reuse_and_rejects_unknown_semantics() {
        let mut bytes = header();
        push_object(
            &mut bytes,
            300,
            7,
            8,
            CHANGE_CREATED | CHANGE_DELETED | CHANGE_INODE,
        );
        assert!(parse_changed_objects(&bytes)
            .unwrap()
            .objects
            .get(&300)
            .copied()
            .unwrap()
            .is_replaced());

        let mut bytes = header();
        push_object(&mut bytes, 300, 7, 8, 1 << 63);
        assert!(parse_changed_objects(&bytes).is_err());
    }

    #[test]
    fn retains_non_utf8_names() {
        let mut bytes = header();
        push_ref(&mut bytes, RECORD_REF_ADD, 300, 256, &[0xff, b'x']);
        push_object(
            &mut bytes,
            300,
            0,
            9,
            CHANGE_INODE | CHANGE_REF | CHANGE_CREATED,
        );
        let manifest = parse_changed_objects(&bytes).unwrap();
        assert_eq!(manifest.ref_adds.iter().next().unwrap().name, [0xff, b'x']);
    }

    #[test]
    fn permits_zero_generations_when_the_inode_item_was_equal() {
        let mut bytes = header();
        push_ref(&mut bytes, RECORD_REF_DELETE, 300, 256, b"old");
        push_ref(&mut bytes, RECORD_REF_ADD, 300, 256, b"new");
        push_object(&mut bytes, 300, 0, 0, CHANGE_REF);
        let manifest = parse_changed_objects(&bytes).unwrap();
        let change = manifest.objects.get(&300).unwrap();
        assert_eq!(change.old_generation, None);
        assert_eq!(change.new_generation, None);
    }
}
