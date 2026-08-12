use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub const ROOT_INODE: u64 = 256;
pub const TOP_LEVEL_ROOT_ID: u64 = 5;
const ROOT_TREE_OBJECT_ID: u64 = 1;
const ROOT_REF_KEY: u32 = 156;
/// Input flag for `BTRFS_IOC_SNAP_CREATE_V2`.
pub const SUBVOL_RDONLY: u64 = 1 << 1;
/// Input flag for `BTRFS_IOC_SNAP_DESTROY_V2` that binds deletion to a
/// verified root ID instead of a pathname which can be replaced concurrently.
pub const SUBVOL_SPEC_BY_ID: u64 = 1 << 4;
/// Root-item flag returned by `BTRFS_IOC_GET_SUBVOL_INFO`.
pub const ROOT_SUBVOL_RDONLY: u64 = 1 << 0;
pub const SUBVOL_NAME_MAX: usize = 4039;

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

const BTRFS_IOC_SNAP_CREATE_V2: libc::c_ulong =
    ioctl_number(IOC_WRITE, 23, size_of::<BtrfsIoctlVolArgsV2>());
const BTRFS_IOC_FS_INFO: libc::c_ulong =
    ioctl_number(IOC_READ, 31, size_of::<BtrfsIoctlFsInfoArgs>());
const BTRFS_IOC_GET_SUBVOL_INFO: libc::c_ulong =
    ioctl_number(IOC_READ, 60, size_of::<BtrfsIoctlGetSubvolInfoArgs>());
const BTRFS_IOC_TREE_SEARCH_V2: libc::c_ulong = ioctl_number(
    IOC_READ | IOC_WRITE,
    17,
    size_of::<BtrfsIoctlSearchArgsV2>(),
);
const BTRFS_IOC_INO_PATHS: libc::c_ulong =
    ioctl_number(IOC_READ | IOC_WRITE, 35, size_of::<BtrfsIoctlInoPathArgs>());
const BTRFS_IOC_SUBVOL_SETFLAGS: libc::c_ulong = ioctl_number(IOC_WRITE, 26, size_of::<u64>());
const BTRFS_IOC_SEND: libc::c_ulong = ioctl_number(IOC_WRITE, 38, size_of::<BtrfsIoctlSendArgs>());
const BTRFS_IOC_SNAP_DESTROY_V2: libc::c_ulong =
    ioctl_number(IOC_WRITE, 63, size_of::<BtrfsIoctlVolArgsV2>());
const BTRFS_IOC_CHANGED_OBJECTS: libc::c_ulong = ioctl_number(
    IOC_READ | IOC_WRITE,
    66,
    size_of::<BtrfsIoctlChangedObjectsArgs>(),
);
const BTRFS_IOC_INO_REFS_BATCH: libc::c_ulong = ioctl_number(
    IOC_READ | IOC_WRITE,
    67,
    size_of::<BtrfsIoctlInoRefsBatchArgs>(),
);
const FS_IOC_GETVERSION: libc::c_ulong = ((IOC_READ << IOC_DIRSHIFT)
    | ((b'v' as u32) << IOC_TYPESHIFT)
    | (1 << IOC_NRSHIFT)
    | ((size_of::<libc::c_long>() as u32) << IOC_SIZESHIFT))
    as libc::c_ulong;

const SEND_FLAG_NO_FILE_DATA: u64 = 0x1;
const SEND_FLAG_CHANGED_OBJECTS: u64 = 0x100;
const CHANGED_OBJECTS_VERSION: u32 = 2;
const CHANGED_OBJECTS_STATUS_COMPLETE: u32 = 0;

const _: [(); 4096] = [(); size_of::<BtrfsIoctlVolArgsV2>()];
const _: [(); 1024] = [(); size_of::<BtrfsIoctlFsInfoArgs>()];
const _: [(); 504] = [(); size_of::<BtrfsIoctlGetSubvolInfoArgs>()];
const _: [(); 112] = [(); size_of::<BtrfsIoctlSearchArgsV2>()];
const _: [(); 56] = [(); size_of::<BtrfsIoctlInoPathArgs>()];
const _: [(); 72] = [(); size_of::<BtrfsIoctlInoRefsBatchArgs>()];
const _: [(); 72] = [(); size_of::<BtrfsIoctlSendArgs>()];
const _: [(); 128] = [(); size_of::<BtrfsIoctlChangedObjectsArgs>()];

#[repr(C)]
struct BtrfsIoctlVolArgsV2 {
    fd: i64,
    transid: u64,
    flags: u64,
    unused: [u64; 4],
    name: [u8; SUBVOL_NAME_MAX + 1],
}

#[repr(C)]
struct BtrfsIoctlFsInfoArgs {
    max_id: u64,
    num_devices: u64,
    fsid: [u8; 16],
    nodesize: u32,
    sectorsize: u32,
    clone_alignment: u32,
    csum_type: u16,
    csum_size: u16,
    flags: u64,
    generation: u64,
    metadata_uuid: [u8; 16],
    reserved: [u8; 944],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BtrfsIoctlTimespec {
    sec: u64,
    nsec: u32,
}

#[repr(C)]
struct BtrfsIoctlGetSubvolInfoArgs {
    treeid: u64,
    name: [u8; 256],
    parent_id: u64,
    dirid: u64,
    generation: u64,
    flags: u64,
    uuid: [u8; 16],
    parent_uuid: [u8; 16],
    received_uuid: [u8; 16],
    ctransid: u64,
    otransid: u64,
    stransid: u64,
    rtransid: u64,
    ctime: BtrfsIoctlTimespec,
    otime: BtrfsIoctlTimespec,
    stime: BtrfsIoctlTimespec,
    rtime: BtrfsIoctlTimespec,
    reserved: [u64; 8],
}

/// Fixed header for BTRFS_IOC_TREE_SEARCH_V2. The UAPI declares the result
/// buffer as a flexible array immediately after this header, so the ioctl
/// number is derived from this type while calls use the fixed buffer below.
#[repr(C)]
struct BtrfsIoctlSearchArgsV2 {
    key: BtrfsIoctlSearchKey,
    buf_size: u64,
}

#[repr(C)]
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

/// One root-ref item is enough for a yes/no answer. Root-ref payloads contain
/// a bounded subvolume name, so 4 KiB comfortably covers one returned item
/// while keeping the broker request allocation fixed.
#[repr(C)]
struct BtrfsIoctlSearchArgsV2Buffer {
    header: BtrfsIoctlSearchArgsV2,
    buffer: [u64; 512],
}

/// Fixed UAPI argument for BTRFS_IOC_INO_PATHS. fspath points at a
/// caller-owned btrfs_data_container whose flexible val[] payload is
/// represented by an aligned byte buffer below.
#[repr(C)]
struct BtrfsIoctlInoPathArgs {
    inum: u64,
    size: u64,
    reserved: [u64; 4],
    fspath: u64,
}

/// Fixed UAPI argument for BTRFS_IOC_INO_REFS_BATCH. inodes points at an
/// exact input set and refs points at caller-owned packed output records.
#[repr(C)]
struct BtrfsIoctlInoRefsBatchArgs {
    inodes: u64,
    refs: u64,
    inode_count: u32,
    refs_size: u32,
    refs_bytes: u32,
    ref_count: u32,
    flags: u64,
    reserved: [u64; 4],
}

const INO_REFS_BATCH_MAX_INODES: usize = 64;
const INO_REFS_BATCH_BUFFER_START: usize = 64 * 1024;
const INO_REFS_BATCH_BUFFER_MAX: usize = 16 * 1024 * 1024;
const INO_REF_RECORD_HEADER_SIZE: usize = 24;

const INO_PATH_BUFFER_START: usize = 4 * 1024;
// The kernel clamps BTRFS_IOC_INO_PATHS to one 4 KiB data container.
const INO_PATH_BUFFER_MAX: usize = 4 * 1024;
const DATA_CONTAINER_HEADER_SIZE: usize = 16;

#[repr(C)]
struct BtrfsIoctlSendArgs {
    send_fd: i64,
    clone_sources_count: u64,
    clone_sources: u64,
    parent_root: u64,
    flags: u64,
    version: u32,
    reserved: [u8; 28],
}

#[repr(C)]
struct BtrfsIoctlChangedObjectsArgs {
    source_fd: i64,
    output_fd: i64,
    flags: u64,
    max_output_bytes: u64,
    max_records: u64,
    output_bytes: u64,
    output_records: u64,
    version: u32,
    status: u32,
    reserved: [u8; 64],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangedObjectsIoctlResult {
    pub output_bytes: u64,
    pub output_records: u64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InodeReference {
    pub ino: u64,
    pub parent_ino: u64,
    pub name: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemInfo {
    pub fs_uuid: [u8; 16],
    pub metadata_uuid: [u8; 16],
    pub generation: u64,
    pub max_device_id: u64,
    pub device_count: u64,
    pub node_size: u32,
    pub sector_size: u32,
    pub clone_alignment: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubvolumeInfo {
    pub root_id: u64,
    pub parent_root_id: Option<u64>,
    pub containing_dir_ino: Option<u64>,
    pub generation: u64,
    pub flags: u64,
    pub uuid: [u8; 16],
    pub parent_uuid: Option<[u8; 16]>,
    pub received_uuid: Option<[u8; 16]>,
    pub ctransid: u64,
    pub otransid: u64,
    pub stransid: u64,
    pub rtransid: u64,
}

impl SubvolumeInfo {
    pub fn readonly(&self) -> bool {
        self.flags & ROOT_SUBVOL_RDONLY != 0
    }

    pub fn is_top_level(&self) -> bool {
        self.root_id == TOP_LEVEL_ROOT_ID
    }
}

#[derive(Debug)]
pub struct OpenedSubvolume {
    file: File,
    pub filesystem: FilesystemInfo,
    pub subvolume: SubvolumeInfo,
}

impl OpenedSubvolume {
    pub fn open(path: &Path) -> Result<Self, BtrfsError> {
        let file = File::open(path)
            .map_err(|error| BtrfsError::context(format!("open {}", path.display()), error))?;
        let metadata = file
            .metadata()
            .map_err(|error| BtrfsError::context(format!("stat {}", path.display()), error))?;
        if !metadata.is_dir() || metadata.ino() != ROOT_INODE {
            return Err(BtrfsError::new(format!(
                "{} is not a Btrfs subvolume root (directory inode {})",
                path.display(),
                ROOT_INODE
            )));
        }
        let filesystem = filesystem_info(file.as_fd())?;
        let subvolume = subvolume_info(file.as_fd())?;
        Ok(Self {
            file,
            filesystem,
            subvolume,
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    pub fn revalidate(&self) -> Result<(), BtrfsError> {
        let filesystem = filesystem_info(self.file.as_fd())?;
        let subvolume = subvolume_info(self.file.as_fd())?;
        if filesystem.fs_uuid != self.filesystem.fs_uuid
            || filesystem.metadata_uuid != self.filesystem.metadata_uuid
            || subvolume != self.subvolume
        {
            return Err(BtrfsError::new(
                "subvolume identity or transaction metadata changed",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct BtrfsError {
    message: String,
    raw_os_error: Option<i32>,
}

impl BtrfsError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            raw_os_error: None,
        }
    }

    fn context(context: impl fmt::Display, error: io::Error) -> Self {
        Self {
            message: format!("{context}: {error}"),
            raw_os_error: error.raw_os_error(),
        }
    }

    pub fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }
}

impl fmt::Display for BtrfsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BtrfsError {}

pub fn filesystem_info(fd: BorrowedFd<'_>) -> Result<FilesystemInfo, BtrfsError> {
    let mut args = BtrfsIoctlFsInfoArgs {
        max_id: 0,
        num_devices: 0,
        fsid: [0; 16],
        nodesize: 0,
        sectorsize: 0,
        clone_alignment: 0,
        csum_type: 0,
        csum_size: 0,
        flags: 0,
        generation: 0,
        metadata_uuid: [0; 16],
        reserved: [0; 944],
    };
    ioctl(fd, BTRFS_IOC_FS_INFO, &mut args, "BTRFS_IOC_FS_INFO")?;
    if args.reserved.iter().any(|byte| *byte != 0) {
        return Err(BtrfsError::new(
            "BTRFS_IOC_FS_INFO returned nonzero reserved bytes",
        ));
    }
    Ok(FilesystemInfo {
        fs_uuid: args.fsid,
        metadata_uuid: args.metadata_uuid,
        generation: args.generation,
        max_device_id: args.max_id,
        device_count: args.num_devices,
        node_size: args.nodesize,
        sector_size: args.sectorsize,
        clone_alignment: args.clone_alignment,
    })
}

/// Returns the VFS inode generation used to detect delete/recreate races.
pub fn inode_generation(fd: BorrowedFd<'_>) -> Result<u64, BtrfsError> {
    let mut generation: libc::c_long = 0;
    // SAFETY: generation is a writable long matching FS_IOC_GETVERSION and fd
    // remains live for the duration of the ioctl.
    if unsafe { libc::ioctl(fd.as_raw_fd(), FS_IOC_GETVERSION, &mut generation) } != 0 {
        return Err(BtrfsError::context(
            "FS_IOC_GETVERSION",
            io::Error::last_os_error(),
        ));
    }
    u64::try_from(generation).map_err(|_| BtrfsError::new("inode generation is negative"))
}

pub fn subvolume_info(fd: BorrowedFd<'_>) -> Result<SubvolumeInfo, BtrfsError> {
    let mut args = BtrfsIoctlGetSubvolInfoArgs {
        treeid: 0,
        name: [0; 256],
        parent_id: 0,
        dirid: 0,
        generation: 0,
        flags: 0,
        uuid: [0; 16],
        parent_uuid: [0; 16],
        received_uuid: [0; 16],
        ctransid: 0,
        otransid: 0,
        stransid: 0,
        rtransid: 0,
        ctime: BtrfsIoctlTimespec::default(),
        otime: BtrfsIoctlTimespec::default(),
        stime: BtrfsIoctlTimespec::default(),
        rtime: BtrfsIoctlTimespec::default(),
        reserved: [0; 8],
    };
    ioctl(
        fd,
        BTRFS_IOC_GET_SUBVOL_INFO,
        &mut args,
        "BTRFS_IOC_GET_SUBVOL_INFO",
    )?;
    if args.uuid == [0; 16] {
        return Err(BtrfsError::new(
            "BTRFS_IOC_GET_SUBVOL_INFO returned an empty subvolume UUID",
        ));
    }
    if args.reserved.iter().any(|value| *value != 0) {
        return Err(BtrfsError::new(
            "BTRFS_IOC_GET_SUBVOL_INFO returned nonzero reserved fields",
        ));
    }
    Ok(SubvolumeInfo {
        root_id: args.treeid,
        parent_root_id: nonzero(args.parent_id),
        containing_dir_ino: nonzero(args.dirid),
        generation: args.generation,
        flags: args.flags,
        uuid: args.uuid,
        parent_uuid: nonzero_uuid(args.parent_uuid),
        received_uuid: nonzero_uuid(args.received_uuid),
        ctransid: args.ctransid,
        otransid: args.otransid,
        stransid: args.stransid,
        rtransid: args.rtransid,
    })
}

/// Returns whether root_id directly references any child subvolume.
///
/// A Btrfs root-ref key is a fast index of subvolumes referenced by one root.
/// Searching the root tree for one ROOT_REF_KEY with object ID root_id
/// therefore answers the nested-subvolume yes/no question without walking the
/// directory tree.
pub fn has_nested_subvolumes(fd: BorrowedFd<'_>, root_id: u64) -> Result<bool, BtrfsError> {
    if root_id == 0 {
        return Err(BtrfsError::new("subvolume root ID must not be zero"));
    }
    let mut args = BtrfsIoctlSearchArgsV2Buffer {
        header: BtrfsIoctlSearchArgsV2 {
            key: BtrfsIoctlSearchKey {
                tree_id: ROOT_TREE_OBJECT_ID,
                min_objectid: root_id,
                max_objectid: root_id,
                min_offset: 0,
                max_offset: u64::MAX,
                min_transid: 0,
                max_transid: u64::MAX,
                min_type: ROOT_REF_KEY,
                max_type: ROOT_REF_KEY,
                nr_items: 1,
                unused: 0,
                unused1: 0,
                unused2: 0,
                unused3: 0,
                unused4: 0,
            },
            buf_size: (512 * size_of::<u64>()) as u64,
        },
        buffer: [0; 512],
    };
    ioctl(
        fd,
        BTRFS_IOC_TREE_SEARCH_V2,
        &mut args,
        "BTRFS_IOC_TREE_SEARCH_V2 nested subvolumes",
    )?;
    if args.header.key.nr_items > 1 {
        return Err(BtrfsError::new(
            "BTRFS_IOC_TREE_SEARCH_V2 returned more root refs than requested",
        ));
    }
    Ok(args.header.key.nr_items == 1)
}

/// Reads immediate inode references for one exact-ID kernel batch.
///
/// The dedicated ioctl performs one B-tree lookup per requested inode inside
/// the kernel. Unlike a TREE_SEARCH_V2 range, sparse IDs never scan unrelated
/// objects between the requested IDs.
pub fn inode_refs_batch(
    fd: BorrowedFd<'_>,
    inodes: &[u64],
) -> Result<BTreeMap<u64, Vec<InodeReference>>, BtrfsError> {
    if inodes.is_empty() || inodes.len() > INO_REFS_BATCH_MAX_INODES {
        return Err(BtrfsError::new(format!(
            "inode-ref batch needs between 1 and {INO_REFS_BATCH_MAX_INODES} inode IDs"
        )));
    }
    let mut requested = BTreeSet::new();
    for &ino in inodes {
        if ino == 0 || !requested.insert(ino) {
            return Err(BtrfsError::new(
                "inode-ref batch needs unique nonzero inode IDs",
            ));
        }
    }
    let mut size = INO_REFS_BATCH_BUFFER_START;
    loop {
        let word_count = size.div_ceil(size_of::<u64>());
        let mut buffer = vec![0_u64; word_count];
        let mut args = BtrfsIoctlInoRefsBatchArgs {
            inodes: inodes.as_ptr() as usize as u64,
            refs: buffer.as_mut_ptr() as usize as u64,
            inode_count: u32::try_from(inodes.len()).expect("bounded inode count fits u32"),
            refs_size: u32::try_from(size).expect("bounded ref buffer fits u32"),
            refs_bytes: 0,
            ref_count: 0,
            flags: 0,
            reserved: [0; 4],
        };
        match ioctl(
            fd,
            BTRFS_IOC_INO_REFS_BATCH,
            &mut args,
            "BTRFS_IOC_INO_REFS_BATCH",
        ) {
            Ok(()) => {
                if args.flags != 0 || args.reserved != [0; 4] {
                    return Err(BtrfsError::new(
                        "inode-ref batch returned nonzero reserved fields",
                    ));
                }
                let refs_bytes = usize::try_from(args.refs_bytes)
                    .map_err(|_| BtrfsError::new("inode-ref byte count exceeds usize"))?;
                if refs_bytes > size {
                    return Err(BtrfsError::new("inode-ref output exceeds supplied buffer"));
                }
                // SAFETY: buffer owns initialized aligned storage and the
                // kernel reported refs_bytes within the supplied allocation.
                let bytes =
                    unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), refs_bytes) };
                return parse_inode_ref_batch_records(bytes, args.ref_count, &requested);
            }
            Err(error) if error.raw_os_error() == Some(libc::EOVERFLOW) => {
                size = size
                    .checked_mul(2)
                    .ok_or_else(|| BtrfsError::new("inode-ref buffer size overflow"))?;
                if size > INO_REFS_BATCH_BUFFER_MAX {
                    return Err(BtrfsError::new(format!(
                        "inode-ref batch result exceeds {INO_REFS_BATCH_BUFFER_MAX} bytes"
                    )));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn parse_inode_ref_batch_records(
    bytes: &[u8],
    count: u32,
    requested: &BTreeSet<u64>,
) -> Result<BTreeMap<u64, Vec<InodeReference>>, BtrfsError> {
    let mut cursor = 0_usize;
    let mut references: BTreeMap<u64, BTreeSet<InodeReference>> = BTreeMap::new();
    for _ in 0..count {
        let header_end = cursor
            .checked_add(INO_REF_RECORD_HEADER_SIZE)
            .ok_or_else(|| BtrfsError::new("inode-ref record header offset overflow"))?;
        if header_end > bytes.len() {
            return Err(BtrfsError::new("inode-ref record header is truncated"));
        }
        let ino = read_ne_u64(bytes, cursor)?;
        let parent_ino = read_ne_u64(bytes, cursor + 8)?;
        let name_len = usize::try_from(read_ne_u32(bytes, cursor + 16)?)
            .map_err(|_| BtrfsError::new("inode-ref name length exceeds usize"))?;
        if read_ne_u32(bytes, cursor + 20)? != 0 {
            return Err(BtrfsError::new(
                "inode-ref record has nonzero reserved field",
            ));
        }
        let end = header_end
            .checked_add(name_len)
            .ok_or_else(|| BtrfsError::new("inode-ref name range overflow"))?;
        let name = bytes
            .get(header_end..end)
            .ok_or_else(|| BtrfsError::new("inode-ref name is truncated"))?;
        if !requested.contains(&ino)
            || parent_ino == 0
            || name.is_empty()
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(BtrfsError::new(
                "inode-ref record has invalid inode, parent, or name",
            ));
        }
        references.entry(ino).or_default().insert(InodeReference {
            ino,
            parent_ino,
            name: name.to_vec(),
        });
        cursor = end;
    }
    if cursor != bytes.len() {
        return Err(BtrfsError::new("inode-ref output has trailing bytes"));
    }
    Ok(references
        .into_iter()
        .map(|(ino, refs)| (ino, refs.into_iter().collect()))
        .collect())
}

/// Resolves every path naming one inode inside the subvolume rooted at fd.
///
/// Btrfs returns paths relative to that root. The root inode itself has the
/// single empty relative path. Hard links may return more than one path.
pub fn inode_paths(fd: BorrowedFd<'_>, ino: u64) -> Result<Vec<Vec<u8>>, BtrfsError> {
    if ino == 0 {
        return Err(BtrfsError::new("inode path lookup inode must not be zero"));
    }
    if ino == ROOT_INODE {
        return Ok(vec![Vec::new()]);
    }
    let mut size = INO_PATH_BUFFER_START;
    loop {
        // u64 backing keeps the flexible data-container payload aligned for
        // the kernel while the parser below stays byte-oriented and bounds
        // checks every returned offset.
        let word_count = size.div_ceil(size_of::<u64>());
        let mut buffer = vec![0_u64; word_count];
        let mut args = BtrfsIoctlInoPathArgs {
            inum: ino,
            size: u64::try_from(size).expect("bounded inode-path buffer fits u64"),
            reserved: [0; 4],
            fspath: buffer.as_mut_ptr() as usize as u64,
        };
        ioctl(fd, BTRFS_IOC_INO_PATHS, &mut args, "BTRFS_IOC_INO_PATHS")?;
        if args.reserved != [0; 4] {
            return Err(BtrfsError::new(
                "BTRFS_IOC_INO_PATHS returned nonzero reserved fields",
            ));
        }
        // SAFETY: buffer owns word_count initialized u64s, so viewing exactly
        // size bytes from its aligned storage is valid for this parse.
        let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), size) };
        let (paths, bytes_missing, elem_missed) = parse_inode_paths_container(bytes)?;
        if bytes_missing == 0 && elem_missed == 0 {
            return Ok(paths);
        }
        let missing = usize::try_from(bytes_missing)
            .map_err(|_| BtrfsError::new("inode-path missing byte count exceeds usize"))?;
        let next = size
            .checked_add(missing.max(size))
            .ok_or_else(|| BtrfsError::new("inode-path buffer size overflow"))?;
        if next > INO_PATH_BUFFER_MAX {
            return Err(BtrfsError::new(format!(
                "inode-path result exceeds {INO_PATH_BUFFER_MAX} bytes"
            )));
        }
        size = next;
    }
}

fn parse_inode_paths_container(bytes: &[u8]) -> Result<(Vec<Vec<u8>>, u32, u32), BtrfsError> {
    if bytes.len() < DATA_CONTAINER_HEADER_SIZE {
        return Err(BtrfsError::new("inode-path data container is truncated"));
    }
    let bytes_missing = read_ne_u32(bytes, 4)?;
    let elem_count = usize::try_from(read_ne_u32(bytes, 8)?)
        .map_err(|_| BtrfsError::new("inode-path element count exceeds usize"))?;
    let elem_missed = read_ne_u32(bytes, 12)?;
    let offsets_end = DATA_CONTAINER_HEADER_SIZE
        .checked_add(
            elem_count
                .checked_mul(size_of::<u64>())
                .ok_or_else(|| BtrfsError::new("inode-path offset count overflow"))?,
        )
        .ok_or_else(|| BtrfsError::new("inode-path offset range overflow"))?;
    if offsets_end > bytes.len() {
        return Err(BtrfsError::new(
            "inode-path data container has truncated offsets",
        ));
    }
    let mut paths = Vec::with_capacity(elem_count);
    for index in 0..elem_count {
        // The kernel publishes each value relative to the beginning of
        // fspath->val, not to the beginning of btrfs_data_container.
        let relative = usize::try_from(read_ne_u64(
            bytes,
            DATA_CONTAINER_HEADER_SIZE + index * size_of::<u64>(),
        )?)
        .map_err(|_| BtrfsError::new("inode-path offset exceeds usize"))?;
        let offset = DATA_CONTAINER_HEADER_SIZE
            .checked_add(relative)
            .ok_or_else(|| BtrfsError::new("inode-path offset overflow"))?;
        if offset < offsets_end || offset >= bytes.len() {
            return Err(BtrfsError::new(
                "inode-path data container has invalid offset",
            ));
        }
        let tail = &bytes[offset..];
        let end = tail
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| BtrfsError::new("inode-path string is not NUL terminated"))?;
        paths.push(tail[..end].to_vec());
    }
    paths.sort();
    paths.dedup();
    Ok((paths, bytes_missing, elem_missed))
}

fn read_ne_u32(bytes: &[u8], offset: usize) -> Result<u32, BtrfsError> {
    let value = bytes
        .get(offset..offset + size_of::<u32>())
        .ok_or_else(|| BtrfsError::new("Btrfs ioctl output is truncated"))?;
    Ok(u32::from_ne_bytes(value.try_into().expect("fixed slice")))
}

fn read_ne_u64(bytes: &[u8], offset: usize) -> Result<u64, BtrfsError> {
    let value = bytes
        .get(offset..offset + size_of::<u64>())
        .ok_or_else(|| BtrfsError::new("Btrfs ioctl output is truncated"))?;
    Ok(u64::from_ne_bytes(value.try_into().expect("fixed slice")))
}

pub fn create_snapshot(
    source: BorrowedFd<'_>,
    destination_parent: BorrowedFd<'_>,
    name: &[u8],
    readonly: bool,
) -> Result<(), BtrfsError> {
    validate_basename(name)?;
    let mut args = BtrfsIoctlVolArgsV2 {
        fd: i64::from(source.as_raw_fd()),
        transid: 0,
        flags: if readonly { SUBVOL_RDONLY } else { 0 },
        unused: [0; 4],
        name: [0; SUBVOL_NAME_MAX + 1],
    };
    args.name[..name.len()].copy_from_slice(name);
    ioctl(
        destination_parent,
        BTRFS_IOC_SNAP_CREATE_V2,
        &mut args,
        "BTRFS_IOC_SNAP_CREATE_V2",
    )
}

pub fn destroy_snapshot(destination_parent: BorrowedFd<'_>, name: &[u8]) -> Result<(), BtrfsError> {
    validate_basename(name)?;
    let mut args = BtrfsIoctlVolArgsV2 {
        fd: 0,
        transid: 0,
        flags: 0,
        unused: [0; 4],
        name: [0; SUBVOL_NAME_MAX + 1],
    };
    args.name[..name.len()].copy_from_slice(name);
    ioctl(
        destination_parent,
        BTRFS_IOC_SNAP_DESTROY_V2,
        &mut args,
        "BTRFS_IOC_SNAP_DESTROY_V2",
    )
}

/// Destroys the exact subvolume root ID through the v2 ioctl.
///
/// Callers must first authorize and verify the root identity. The kernel then
/// resolves that immutable root ID at ioctl time, so a concurrent pathname
/// replacement cannot redirect deletion to a different subvolume.
pub fn destroy_snapshot_by_id(
    filesystem_fd: BorrowedFd<'_>,
    root_id: u64,
) -> Result<(), BtrfsError> {
    if root_id < ROOT_INODE {
        return Err(BtrfsError::new("refuse to destroy reserved Btrfs root ID"));
    }
    let mut args = snapshot_destroy_by_id_args(root_id);
    ioctl(
        filesystem_fd,
        BTRFS_IOC_SNAP_DESTROY_V2,
        &mut args,
        "BTRFS_IOC_SNAP_DESTROY_V2 by ID",
    )
}

fn snapshot_destroy_by_id_args(root_id: u64) -> BtrfsIoctlVolArgsV2 {
    let mut args = BtrfsIoctlVolArgsV2 {
        fd: 0,
        transid: 0,
        flags: SUBVOL_SPEC_BY_ID,
        unused: [0; 4],
        name: [0; SUBVOL_NAME_MAX + 1],
    };
    // subvolid is the first field of the final C union, which shares storage
    // with name. Keep the Rust wire struct simple and fill the union bytes
    // explicitly rather than pretending the ID is transid.
    args.name[..std::mem::size_of::<u64>()].copy_from_slice(&root_id.to_ne_bytes());
    args
}

/// Changes a subvolume's read-only property using ordinary owner permissions.
///
/// Unprivileged Btrfs deletion checks MAY_WRITE on the target root even with
/// user_subvol_rm_allowed, so a retained read-only baseline must be made
/// writable only after it is unpinned and immediately before destroy.
pub fn set_subvolume_readonly(subvolume: BorrowedFd<'_>, readonly: bool) -> Result<(), BtrfsError> {
    let mut flags = if readonly { SUBVOL_RDONLY } else { 0 };
    ioctl(
        subvolume,
        BTRFS_IOC_SUBVOL_SETFLAGS,
        &mut flags,
        "BTRFS_IOC_SUBVOL_SETFLAGS",
    )
}

pub fn send_changed_objects(
    target: BorrowedFd<'_>,
    parent_root_id: u64,
    output: BorrowedFd<'_>,
) -> Result<(), BtrfsError> {
    if parent_root_id == 0 {
        return Err(BtrfsError::new(
            "changed-object parent root ID must not be zero",
        ));
    }
    send_specialized(target, parent_root_id, output, SEND_FLAG_CHANGED_OBJECTS)
}

pub fn changed_objects_v2(
    target: BorrowedFd<'_>,
    source: BorrowedFd<'_>,
    output: BorrowedFd<'_>,
    max_output_bytes: u64,
    max_records: u64,
) -> Result<ChangedObjectsIoctlResult, BtrfsError> {
    if max_output_bytes == 0 || max_records == 0 {
        return Err(BtrfsError::new(
            "changed-object v2 limits must both be nonzero",
        ));
    }
    let mut args = BtrfsIoctlChangedObjectsArgs {
        source_fd: i64::from(source.as_raw_fd()),
        output_fd: i64::from(output.as_raw_fd()),
        flags: 0,
        max_output_bytes,
        max_records,
        output_bytes: 0,
        output_records: 0,
        version: CHANGED_OBJECTS_VERSION,
        status: CHANGED_OBJECTS_STATUS_COMPLETE,
        reserved: [0; 64],
    };
    ioctl(
        target,
        BTRFS_IOC_CHANGED_OBJECTS,
        &mut args,
        "BTRFS_IOC_CHANGED_OBJECTS",
    )?;
    if args.status != CHANGED_OBJECTS_STATUS_COMPLETE || args.reserved != [0; 64] {
        return Err(BtrfsError::new(
            "BTRFS_IOC_CHANGED_OBJECTS returned invalid completion fields",
        ));
    }
    Ok(ChangedObjectsIoctlResult {
        output_bytes: args.output_bytes,
        output_records: args.output_records,
    })
}

/// Proves that the running kernel recognizes the dedicated changed-objects
/// ioctl on a Btrfs subvolume fd.
///
/// The probe intentionally supplies the same root as both endpoints and
/// /dev/null as output. A supporting kernel rejects those operands during
/// validation, but an older kernel returns ENOTTY before it can inspect
/// them. Therefore every result except ENOTTY proves that the ioctl dispatch
/// exists without creating snapshots or mutating filesystem state.
pub fn supports_changed_objects_v2(target: BorrowedFd<'_>) -> Result<bool, BtrfsError> {
    let output = File::options()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|error| BtrfsError::context("open changed-object probe output", error))?;
    match changed_objects_v2(target, target, output.as_fd(), 1, 1) {
        Err(error) if error.raw_os_error() == Some(libc::ENOTTY) => Ok(false),
        Ok(_) | Err(_) => Ok(true),
    }
}

fn send_specialized(
    target: BorrowedFd<'_>,
    parent_root_id: u64,
    output: BorrowedFd<'_>,
    specialized_flag: u64,
) -> Result<(), BtrfsError> {
    let mut args = BtrfsIoctlSendArgs {
        send_fd: i64::from(output.as_raw_fd()),
        clone_sources_count: 0,
        clone_sources: 0,
        parent_root: parent_root_id,
        flags: SEND_FLAG_NO_FILE_DATA | specialized_flag,
        version: 0,
        reserved: [0; 28],
    };
    ioctl(
        target,
        BTRFS_IOC_SEND,
        &mut args,
        "BTRFS_IOC_SEND specialized",
    )
}

fn validate_basename(name: &[u8]) -> Result<(), BtrfsError> {
    if name.is_empty() || name.len() > SUBVOL_NAME_MAX {
        return Err(BtrfsError::new(format!(
            "snapshot basename length must be between 1 and {SUBVOL_NAME_MAX} bytes"
        )));
    }
    if name.contains(&b'/') || name.contains(&b'\0') || name == b"." || name == b".." {
        return Err(BtrfsError::new(
            "snapshot destination must be one safe basename",
        ));
    }
    Ok(())
}

fn nonzero(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

fn nonzero_uuid(value: [u8; 16]) -> Option<[u8; 16]> {
    (value != [0; 16]).then_some(value)
}

fn ioctl<T>(
    fd: BorrowedFd<'_>,
    request: libc::c_ulong,
    argument: &mut T,
    name: &str,
) -> Result<(), BtrfsError> {
    // SAFETY: the request constants encode the exact sizes asserted above;
    // argument points to an initialized repr(C) UAPI structure for the entire
    // call and fd remains borrowed for the call.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), request, argument) };
    // ioctl(2) reports failure as -1 with errno. Most Btrfs ioctls return 0
    // on success, but kernels may return a positive success value for
    // metadata queries such as GET_SUBVOL_INFO; do not turn that into a stale
    // errno error.
    if result >= 0 {
        Ok(())
    } else {
        Err(BtrfsError::context(name, io::Error::last_os_error()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn ioctl_numbers_and_layouts_match_linux_uapi() {
        assert_eq!(size_of::<BtrfsIoctlVolArgsV2>(), 4096);
        assert_eq!(size_of::<BtrfsIoctlFsInfoArgs>(), 1024);
        assert_eq!(size_of::<BtrfsIoctlGetSubvolInfoArgs>(), 504);
        assert_eq!(size_of::<BtrfsIoctlSearchArgsV2>(), 112);
        assert_eq!(BTRFS_IOC_SNAP_CREATE_V2, 0x5000_9417);
        assert_eq!(BTRFS_IOC_FS_INFO, 0x8400_941f);
        assert_eq!(BTRFS_IOC_GET_SUBVOL_INFO, 0x81f8_943c);
        assert_eq!(BTRFS_IOC_TREE_SEARCH_V2, 0xc070_9411);
        assert_eq!(BTRFS_IOC_INO_PATHS, 0xc038_9423);
        assert_eq!(BTRFS_IOC_INO_REFS_BATCH, 0xc048_9443);
        assert_eq!(BTRFS_IOC_SEND, 0x4048_9426);
        assert_eq!(BTRFS_IOC_SNAP_DESTROY_V2, 0x5000_943f);
        assert_eq!(BTRFS_IOC_CHANGED_OBJECTS, 0xc080_9442);
        assert_eq!(FS_IOC_GETVERSION, 0x8008_7601);
        assert_ne!(SUBVOL_RDONLY, ROOT_SUBVOL_RDONLY);
    }

    #[test]
    fn rejects_unsafe_snapshot_names_before_ioctl() {
        let file = File::open("/dev/null").unwrap();
        for name in [
            b"".as_slice(),
            b".".as_slice(),
            b"..".as_slice(),
            b"a/b".as_slice(),
        ] {
            assert!(create_snapshot(file.as_fd(), file.as_fd(), name, true).is_err());
            assert!(destroy_snapshot(file.as_fd(), name).is_err());
        }
    }

    #[test]
    fn non_btrfs_fd_fails_without_mutation() {
        let file = File::open("/dev/null").unwrap();
        let error = filesystem_info(file.as_fd()).unwrap_err();
        assert!(matches!(
            error.raw_os_error(),
            Some(libc::ENOTTY) | Some(libc::EINVAL)
        ));
    }

    #[test]
    fn rejects_zero_parent_before_changed_object_ioctl() {
        let file = File::open("/dev/null").unwrap();
        assert!(
            send_changed_objects(file.as_fd(), 0, file.as_fd())
                .unwrap_err()
                .to_string()
                .contains("must not be zero")
        );
    }

    #[test]
    fn parses_inode_path_data_container() {
        let mut bytes = vec![0_u8; 64];
        bytes[8..12].copy_from_slice(&2_u32.to_ne_bytes());
        bytes[16..24].copy_from_slice(&16_u64.to_ne_bytes());
        bytes[24..32].copy_from_slice(&24_u64.to_ne_bytes());
        bytes[32..36].copy_from_slice(b"a/b\0");
        bytes[40..42].copy_from_slice(b"c\0");
        assert_eq!(
            parse_inode_paths_container(&bytes).unwrap(),
            (vec![b"a/b".to_vec(), b"c".to_vec()], 0, 0)
        );
    }

    #[test]
    fn rejects_inode_path_offset_into_header() {
        let mut bytes = vec![0_u8; 32];
        bytes[8..12].copy_from_slice(&1_u32.to_ne_bytes());
        bytes[16..24].copy_from_slice(&0_u64.to_ne_bytes());
        assert!(parse_inode_paths_container(&bytes).is_err());
    }

    #[test]
    fn parses_packed_inode_refs_for_batched_lookup() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&300_u64.to_ne_bytes());
        payload.extend_from_slice(&ROOT_INODE.to_ne_bytes());
        payload.extend_from_slice(&3_u32.to_ne_bytes());
        payload.extend_from_slice(&0_u32.to_ne_bytes());
        payload.extend_from_slice(b"one");
        payload.extend_from_slice(&300_u64.to_ne_bytes());
        payload.extend_from_slice(&ROOT_INODE.to_ne_bytes());
        payload.extend_from_slice(&3_u32.to_ne_bytes());
        payload.extend_from_slice(&0_u32.to_ne_bytes());
        payload.extend_from_slice(b"two");
        let requested = BTreeSet::from([300]);
        let references = parse_inode_ref_batch_records(&payload, 2, &requested).unwrap();
        assert_eq!(
            references
                .get(&300)
                .unwrap()
                .iter()
                .map(|reference| reference.name.clone())
                .collect::<Vec<_>>(),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
    }

    #[test]
    fn rejects_inode_ref_record_for_unrequested_inode() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&301_u64.to_ne_bytes());
        payload.extend_from_slice(&ROOT_INODE.to_ne_bytes());
        payload.extend_from_slice(&4_u32.to_ne_bytes());
        payload.extend_from_slice(&0_u32.to_ne_bytes());
        payload.extend_from_slice(b"name");
        let requested = BTreeSet::from([300]);
        assert!(parse_inode_ref_batch_records(&payload, 1, &requested).is_err());
    }

    #[test]
    fn rejects_zero_root_before_nested_subvolume_ioctl() {
        let file = File::open("/dev/null").unwrap();
        assert!(
            has_nested_subvolumes(file.as_fd(), 0)
                .unwrap_err()
                .to_string()
                .contains("must not be zero")
        );
    }

    #[test]
    fn rejects_reserved_root_before_destroy_by_id_ioctl() {
        let file = File::open("/dev/null").unwrap();
        assert!(
            destroy_snapshot_by_id(file.as_fd(), TOP_LEVEL_ROOT_ID)
                .unwrap_err()
                .to_string()
                .contains("reserved Btrfs root ID")
        );
    }

    #[test]
    fn destroy_by_id_places_subvolume_id_in_final_uapi_union() {
        let root_id = 0x1122_3344_5566_7788;
        let args = snapshot_destroy_by_id_args(root_id);
        assert_eq!(args.transid, 0);
        assert_eq!(args.flags, SUBVOL_SPEC_BY_ID);
        assert_eq!(
            u64::from_ne_bytes(args.name[..size_of::<u64>()].try_into().unwrap()),
            root_id
        );
    }

    #[test]
    #[ignore = "requires JJ_TEST_BTRFS_ROOT and Btrfs snapshot deletion permissions"]
    fn open_snapshot_fd_remains_traversable_after_path_deletion() {
        let root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
            .map(PathBuf::from)
            .expect("JJ_TEST_BTRFS_ROOT must name a writable directory on real Btrfs");
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let fixture = root.join(format!("awacs-open-fd-{suffix}"));
        let source = fixture.join("source");
        let snapshot = fixture.join("snapshot");
        std::fs::create_dir(&fixture).unwrap();
        assert!(
            Command::new("btrfs")
                .args(["subvolume", "create"])
                .arg(&source)
                .status()
                .unwrap()
                .success(),
            "failed to create Btrfs source subvolume"
        );
        std::fs::create_dir(source.join("nested")).unwrap();
        std::fs::write(source.join("nested/file"), b"still reachable\n").unwrap();

        let parent = File::open(&fixture).unwrap();
        let source_fd = File::open(&source).unwrap();
        create_snapshot(source_fd.as_fd(), parent.as_fd(), b"snapshot", true).unwrap();
        let scan_fd = File::open(&snapshot).unwrap();
        let delete_fd = File::open(&snapshot).unwrap();

        // Production cleanup performs this transition before destroy because
        // unprivileged Btrfs deletion rejects a read-only target. The scan fd
        // was opened while the snapshot was immutable and must remain a
        // usable fd-relative root after its pathname is unlinked.
        set_subvolume_readonly(delete_fd.as_fd(), false).unwrap();
        destroy_snapshot(parent.as_fd(), b"snapshot").unwrap();
        assert!(!snapshot.exists());

        let relative = CString::new("nested/file").unwrap();
        // SAFETY: scan_fd remains open for the call and relative is a
        // NUL-terminated relative path. Ownership of a successful descriptor
        // is immediately transferred into File.
        let child_fd = unsafe {
            libc::openat(
                scan_fd.as_raw_fd(),
                relative.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        assert!(
            child_fd >= 0,
            "openat through unlinked snapshot fd failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: child_fd was returned by a successful openat above and is
        // owned exactly once by this File.
        let mut child = unsafe { File::from_raw_fd(child_fd) };
        let mut contents = String::new();
        child.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "still reachable\n");

        drop(child);
        drop(delete_fd);
        drop(scan_fd);
        drop(source_fd);
        assert!(
            Command::new("btrfs")
                .args(["subvolume", "delete"])
                .arg(&source)
                .status()
                .unwrap()
                .success(),
            "failed to delete Btrfs source subvolume"
        );
        std::fs::remove_dir(&fixture).unwrap();
    }
}
