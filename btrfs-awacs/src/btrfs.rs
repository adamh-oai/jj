use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Instant;

pub const ROOT_INODE: u64 = 256;
pub const TOP_LEVEL_ROOT_ID: u64 = 5;
const ROOT_TREE_OBJECT_ID: u64 = 1;
const ROOT_REF_KEY: u32 = 156;
/// Input flag for `BTRFS_IOC_SNAP_CREATE_V2`.
pub const SUBVOL_RDONLY: u64 = 1 << 1;
/// Input flag for `BTRFS_IOC_SNAP_DESTROY_V2` that selects a subvolume by root
/// ID. Callers must still protect its parent namespace against concurrent
/// renames.
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
const BTRFS_IOC_START_SYNC: libc::c_ulong = ioctl_number(IOC_READ, 24, size_of::<u64>());
const BTRFS_IOC_WAIT_SYNC: libc::c_ulong = ioctl_number(IOC_WRITE, 22, size_of::<u64>());
const BTRFS_IOC_SEND: libc::c_ulong = ioctl_number(IOC_WRITE, 38, size_of::<BtrfsIoctlSendArgs>());
const BTRFS_IOC_SNAP_DESTROY_V2: libc::c_ulong =
    ioctl_number(IOC_WRITE, 63, size_of::<BtrfsIoctlVolArgsV2>());
const BTRFS_IOC_CHANGED_OBJECTS: libc::c_ulong = ioctl_number(
    IOC_READ | IOC_WRITE,
    66,
    size_of::<BtrfsIoctlChangedObjectsArgs>(),
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
    let ioctl_started = Instant::now();
    let result = ioctl(
        destination_parent,
        BTRFS_IOC_SNAP_CREATE_V2,
        &mut args,
        "BTRFS_IOC_SNAP_CREATE_V2",
    );
    tracing::debug!(
        ioctl = "BTRFS_IOC_SNAP_CREATE_V2",
        elapsed_us = ioctl_started.elapsed().as_micros() as u64,
        success = result.is_ok(),
        "Btrfs snapshot ioctl completed"
    );
    result
}

/// Commits pending subvolume cleanup before an immutable identity is captured.
///
/// Creating a read-only snapshot runs orphan cleanup after committing the
/// snapshot itself. Cleanup of copied open-unlinked inodes can leave another
/// transaction pending. A subsequent send commits that transaction, changing
/// the snapshot's reported root generation despite its read-only status.
///
/// START_SYNC runs orphan cleanup for this root and starts a commit of the
/// current filesystem transaction; WAIT_SYNC waits for that exact transaction.
/// Unlike a directory fsync, this guarantees the root item is committed. It
/// does not request the filesystem-wide delalloc flush performed by
/// BTRFS_IOC_SYNC. Callers must observe the snapshot's identity after this call.
pub fn sync_subvolume(fd: BorrowedFd<'_>) -> Result<(), BtrfsError> {
    let mut transid = 0_u64;
    ioctl(
        fd,
        BTRFS_IOC_START_SYNC,
        &mut transid,
        "BTRFS_IOC_START_SYNC",
    )?;
    ioctl(fd, BTRFS_IOC_WAIT_SYNC, &mut transid, "BTRFS_IOC_WAIT_SYNC")
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

/// Requests subvolume deletion by root ID through the v2 ioctl.
///
/// Callers must first authorize and verify the root identity and protect its
/// parent namespace. Some kernels resolve the ID to a pathname and then look
/// that name up again, so this flag alone does not prevent replacement races.
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

/// Removes a file, symlink, directory tree, or Btrfs subvolume without
/// following symlinks or crossing mount points below the supplied parent
/// directory.
///
/// Subvolume contents are removed by `SNAP_DESTROY_V2`, never by unlinking
/// their ordinary files. A nonempty-subvolume error permits searching for
/// nested subvolumes and destroying those first; every other ioctl failure is
/// terminal. Missing paths return `NotFound`, just like the ordinary filesystem
/// operations.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    let (parent, name) = removal_parent(path)?;
    remove_tree_entry(parent.as_fd(), name)
}

/// Removes a verified Btrfs subvolume and its nested subvolumes.
///
/// Unlike [`remove_tree`], this rejects ordinary directories, files, symlinks,
/// and the empty inode-2 placeholders left by snapshots of nested subvolumes.
pub fn remove_subvolume_tree(path: &Path) -> io::Result<()> {
    let (parent, name) = removal_parent(path)?;
    let directory = open_directory_at(parent.as_fd(), name)?;
    let stat = directory_stat(directory.as_fd())?;
    if !is_subvolume_directory(directory.as_fd(), &stat)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "removal target is not a Btrfs subvolume root",
        ));
    }
    remove_subvolume_entry(parent.as_fd(), name, directory.as_fd())
}

/// Removes an already-opened directory, preserving its caller-verified
/// identity.
///
/// `name` must be one component in `parent`. Both traversal and final removal
/// remain anchored to these descriptors, and a replaced entry fails closed.
pub fn remove_directory_at(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    directory: BorrowedFd<'_>,
) -> io::Result<()> {
    validate_removal_name(name)?;
    let stat = directory_stat(directory)?;
    verify_directory_entry(parent, name, &stat)?;
    verify_same_mount(parent, directory)?;
    if is_subvolume_directory(directory, &stat)? {
        return remove_subvolume_entry(parent, name, directory);
    }
    for child in directory_names(directory)? {
        match remove_tree_entry(directory, &child) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    verify_directory_entry(parent, name, &stat)?;
    unlink_entry(parent, name, libc::AT_REMOVEDIR)
}

fn removal_parent(path: &Path) -> io::Result<(File, &OsStr)> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "removal path has no basename")
    })?;
    validate_removal_name(name)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok((File::open(parent)?, name))
}

fn validate_removal_name(name: &OsStr) -> io::Result<()> {
    validate_basename(name.as_bytes()).map_err(btrfs_io_error)
}

fn remove_tree_entry(parent: BorrowedFd<'_>, name: &OsStr) -> io::Result<()> {
    validate_removal_name(name)?;
    let stat = entry_stat(parent, name)?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return unlink_entry(parent, name, 0);
    }
    let directory = open_directory_at(parent, name)?;
    let opened = directory_stat(directory.as_fd())?;
    verify_stat_identity(&stat, &opened)?;
    remove_directory_at(parent, name, directory.as_fd())
}

fn remove_subvolume_entry(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    directory: BorrowedFd<'_>,
) -> io::Result<()> {
    let stat = directory_stat(directory)?;
    verify_directory_entry(parent, name, &stat)?;
    verify_same_mount(parent, directory)?;
    let identity = subvolume_info(directory).map_err(btrfs_io_error)?;
    if identity.root_id < ROOT_INODE {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refuse to remove a reserved Btrfs root",
        ));
    }
    // Btrfs forbids moving a read-only subvolume into the private removal
    // directory, and a read-only parent cannot have nested subvolumes removed.
    // Change only the already-verified target and restore it if deletion fails.
    if identity.readonly() {
        set_subvolume_readonly(directory, false).map_err(btrfs_io_error)?;
    }
    let result = remove_claimed_directory(parent, name, &stat, |claimed_parent, claimed_name| {
        destroy_with_nested_cleanup(
            || {
                verify_directory_entry(claimed_parent, claimed_name, &stat)?;
                let current = open_directory_at(claimed_parent, claimed_name)?;
                let current_identity = subvolume_info(current.as_fd()).map_err(btrfs_io_error)?;
                if current_identity.root_id != identity.root_id
                    || current_identity.uuid != identity.uuid
                {
                    return Err(io::Error::other(
                        "subvolume identity changed before deletion",
                    ));
                }
                destroy_snapshot(claimed_parent, claimed_name.as_bytes()).map_err(btrfs_io_error)
            },
            || remove_nested_subvolumes(directory),
        )
    });
    if let Err(error) = result {
        if identity.readonly()
            && let Err(restore_error) = set_subvolume_readonly(directory, true)
        {
            return Err(io::Error::other(format!(
                "{error}; cannot restore failed removal target's read-only flag: {restore_error}"
            )));
        }
        return Err(error);
    }
    Ok(())
}

/// Atomically moves the entry into an owner-only directory before checking its
/// identity and deleting it. A writer of the original parent can replace the
/// public name, but cannot redirect the ioctl inside the protected directory.
/// Processes sharing our uid remain part of the same trust boundary.
fn remove_claimed_directory(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    expected: &libc::stat,
    remove: impl FnOnce(BorrowedFd<'_>, &OsStr) -> io::Result<()>,
) -> io::Result<()> {
    let claim_name = OsString::from(format!(".awacs-removing-{}", uuid::Uuid::new_v4()));
    let claim_c_name = CString::new(claim_name.as_bytes())?;
    // SAFETY: parent and the NUL-terminated component remain valid for mkdirat.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), claim_c_name.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let claim = open_directory_at(parent, &claim_name).map_err(|error| {
        io::Error::other(format!(
            "cannot open removal directory {claim_name:?}: {error}; directory retained"
        ))
    })?;
    let claim_stat = directory_stat(claim.as_fd())?;
    // A replacement owned by a different uid must never become our protected
    // parent. mkdir's mode also masks inherited ACL permissions for other users.
    if claim_stat.st_uid != unsafe { libc::geteuid() } || claim_stat.st_mode & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "removal directory {claim_name:?} is not private to the current user"
        )));
    }
    verify_directory_entry(parent, &claim_name, &claim_stat)?;
    verify_same_mount(parent, claim.as_fd())?;
    let claimed_name = OsStr::new("target");
    let result = match rename_entry_noreplace(parent, name, claim.as_fd(), claimed_name) {
        Ok(()) => {
            let result = verify_directory_entry(claim.as_fd(), claimed_name, expected)
                .and_then(|()| remove(claim.as_fd(), claimed_name));
            if let Err(error) = &result
                && let Err(restore_error) =
                    rename_entry_noreplace(claim.as_fd(), claimed_name, parent, name)
            {
                // Keep the claimed entry intact if another process occupies
                // the original name. Never replace it during error recovery.
                return Err(io::Error::other(format!(
                    "{error}; cannot restore {claim_name:?}/target to {name:?}: {restore_error}; \
                     removal target retained in the removal directory"
                )));
            }
            result
        }
        Err(error) => Err(error),
    };
    let cleanup = verify_directory_entry(parent, &claim_name, &claim_stat)
        .and_then(|()| unlink_entry(parent, &claim_name, libc::AT_REMOVEDIR));
    match (result, cleanup) {
        (result, Ok(())) => result,
        (Ok(()), Err(error)) => Err(io::Error::other(format!(
            "target removed but empty removal directory {claim_name:?} remains: {error}"
        ))),
        (Err(error), Err(cleanup_error)) => Err(io::Error::other(format!(
            "{error}; cannot remove empty removal directory {claim_name:?}: {cleanup_error}"
        ))),
    }
}

fn rename_entry_noreplace(
    source_parent: BorrowedFd<'_>,
    source_name: &OsStr,
    target_parent: BorrowedFd<'_>,
    target_name: &OsStr,
) -> io::Result<()> {
    let source_name = CString::new(source_name.as_bytes())?;
    let target_name = CString::new(target_name.as_bytes())?;
    // SAFETY: both descriptors and C strings are valid. Do not fall back to a
    // replacing rename if the kernel or filesystem lacks RENAME_NOREPLACE.
    if unsafe {
        libc::renameat2(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            target_parent.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn destroy_with_nested_cleanup(
    mut destroy: impl FnMut() -> io::Result<()>,
    remove_nested: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match destroy() {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::ENOTEMPTY) => {
            remove_nested()?;
            destroy()
        }
        Err(error) => Err(error),
    }
}

/// Searches ordinary directories without removing them or their files. This is
/// used only after the kernel reports nested subvolumes blocking root deletion.
/// Unreadable ordinary directories, including private broker trash, may be
/// skipped: the mandatory final destroy ioctl still rejects any hidden nested
/// subvolume that remains. Errors deleting an observed subvolume are terminal.
fn remove_nested_subvolumes(directory: BorrowedFd<'_>) -> io::Result<()> {
    let names = match directory_names(directory) {
        Ok(names) => names,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(error),
    };
    for name in names {
        let stat = match entry_stat(directory, &name) {
            Ok(stat) => stat,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            continue;
        }
        let child = match open_directory_at(directory, &name) {
            Ok(child) => child,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let opened = directory_stat(child.as_fd())?;
        verify_stat_identity(&stat, &opened)?;
        verify_same_mount(directory, child.as_fd())?;
        if is_subvolume_directory(child.as_fd(), &opened)? {
            remove_subvolume_entry(directory, &name, child.as_fd())?;
        } else {
            remove_nested_subvolumes(child.as_fd())?;
        }
    }
    Ok(())
}

fn btrfs_io_error(error: BtrfsError) -> io::Error {
    match error.raw_os_error() {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::other(error),
    }
}

fn is_subvolume_directory(directory: BorrowedFd<'_>, stat: &libc::stat) -> io::Result<bool> {
    if stat.st_ino != ROOT_INODE {
        return Ok(false);
    }
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: the descriptor is borrowed for the call and the output is a valid
    // writable statfs allocation, read only after fstatfs succeeds.
    if unsafe { libc::fstatfs(directory.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let filesystem = unsafe { filesystem.assume_init() };
    Ok(filesystem.f_type as u32 == 0x9123_683e)
}

fn open_directory_at(parent: BorrowedFd<'_>, name: &OsStr) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: name is NUL-terminated, parent remains live, and a successful
    // descriptor is immediately transferred into one owned File.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn directory_stat(directory: BorrowedFd<'_>) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: stat is writable and inspected only after successful fstat.
    if unsafe { libc::fstat(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    Ok(stat)
}

fn entry_stat(parent: BorrowedFd<'_>, name: &OsStr) -> io::Result<libc::stat> {
    let name = CString::new(name.as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: the borrowed directory and C string are valid; stat is read only
    // after fstatat initializes it. Symlinks are never followed.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() })
}

fn verify_stat_identity(expected: &libc::stat, current: &libc::stat) -> io::Result<()> {
    if expected.st_dev != current.st_dev
        || expected.st_ino != current.st_ino
        || expected.st_mode & libc::S_IFMT != current.st_mode & libc::S_IFMT
    {
        return Err(io::Error::other(
            "directory identity changed during removal",
        ));
    }
    Ok(())
}

fn verify_directory_entry(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    expected: &libc::stat,
) -> io::Result<()> {
    verify_stat_identity(expected, &entry_stat(parent, name)?)
}

fn verify_same_mount(parent: BorrowedFd<'_>, child: BorrowedFd<'_>) -> io::Result<()> {
    if mount_id(parent)? != mount_id(child)? {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    Ok(())
}

fn mount_id(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: AT_EMPTY_PATH selects the live descriptor; stat is a writable
    // statx allocation read only after the syscall succeeds.
    if unsafe {
        libc::statx(
            fd.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID,
            stat.as_mut_ptr(),
        )
    } == 0
    {
        let stat = unsafe { stat.assume_init() };
        if stat.stx_mask & libc::STATX_MNT_ID != 0 {
            return Ok(stat.stx_mnt_id);
        }
    }
    // Older kernels may not supply STATX_MNT_ID. fdinfo supplies the same
    // identity without allowing a bind mount to masquerade as a local subtree.
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
        .ok_or_else(|| io::Error::other("cannot verify directory mount identity"))
}

fn unlink_entry(parent: BorrowedFd<'_>, name: &OsStr, flags: libc::c_int) -> io::Result<()> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: parent and name remain valid throughout this descriptor-relative
    // unlink, whose flags are selected only by the typed callers above.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn directory_names(directory: BorrowedFd<'_>) -> io::Result<Vec<OsString>> {
    // Opening "." gives enumeration an independent directory offset rather
    // than changing the caller's borrowed descriptor through dup().
    let fd = open_directory_at(directory, OsStr::new("."))?.into_raw_fd();
    // SAFETY: fd is owned and points to a directory. fdopendir takes ownership
    // only on success; close it ourselves if constructing the stream fails.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(error);
    }
    struct DirectoryStream(*mut libc::DIR);
    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            // SAFETY: the stream uniquely owns the descriptor from fdopendir.
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        // SAFETY: readdir uses this live, exclusively owned stream. Clear
        // thread-local errno to distinguish end-of-directory from an error.
        unsafe { *libc::__errno_location() = 0 };
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(0) {
                Ok(names)
            } else {
                Err(error)
            };
        }
        // SAFETY: readdir returned a live dirent with a NUL-terminated name;
        // copy it before advancing the stream again.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
    }
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
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::*;

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
        assert_eq!(BTRFS_IOC_START_SYNC, 0x8008_9418);
        assert_eq!(BTRFS_IOC_WAIT_SYNC, 0x4008_9416);
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
    fn remove_tree_preserves_symlink_targets() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = fixture.path().join("outside");
        let tree = fixture.path().join("tree");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"outside").unwrap();
        std::fs::create_dir_all(tree.join("ordinary/nested")).unwrap();
        std::fs::write(tree.join("ordinary/nested/file"), b"inside").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("link")).unwrap();
        let root_link = fixture.path().join("root-link");
        std::os::unix::fs::symlink(&outside, &root_link).unwrap();

        remove_tree(&root_link).unwrap();
        remove_tree(&tree).unwrap();
        assert!(!root_link.exists());
        assert!(!tree.exists());
        assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"outside");
        remove_tree(&outside.join("keep")).unwrap();
        assert_eq!(
            remove_tree(&tree).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn remove_directory_at_rejects_replaced_entry() {
        let fixture = tempfile::tempdir().unwrap();
        let tree = fixture.path().join("tree");
        let original = fixture.path().join("original");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("keep"), b"original").unwrap();
        let parent = File::open(fixture.path()).unwrap();
        let opened = File::open(&tree).unwrap();
        std::fs::rename(&tree, &original).unwrap();
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("keep"), b"replacement").unwrap();

        let error =
            remove_directory_at(parent.as_fd(), OsStr::new("tree"), opened.as_fd()).unwrap_err();
        assert!(error.to_string().contains("identity changed"));
        assert_eq!(std::fs::read(original.join("keep")).unwrap(), b"original");
        assert_eq!(std::fs::read(tree.join("keep")).unwrap(), b"replacement");
    }

    #[test]
    fn removal_claim_rejects_and_restores_a_swapped_entry() {
        let fixture = tempfile::tempdir().unwrap();
        let tree = fixture.path().join("tree");
        let original = fixture.path().join("original");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("keep"), b"original").unwrap();
        let parent = File::open(fixture.path()).unwrap();
        let opened = File::open(&tree).unwrap();
        let expected = directory_stat(opened.as_fd()).unwrap();
        verify_directory_entry(parent.as_fd(), OsStr::new("tree"), &expected).unwrap();

        // Model a rename after the caller's last public-name check but before
        // the atomic claim. The wrong object may be claimed, never deleted.
        std::fs::rename(&tree, &original).unwrap();
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("keep"), b"replacement").unwrap();
        let error =
            remove_claimed_directory(parent.as_fd(), OsStr::new("tree"), &expected, |_, _| {
                panic!("must reject a swapped entry before deletion")
            })
            .unwrap_err();
        assert!(error.to_string().contains("identity changed"));
        assert_eq!(std::fs::read(original.join("keep")).unwrap(), b"original");
        assert_eq!(std::fs::read(tree.join("keep")).unwrap(), b"replacement");
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 2);
    }

    #[test]
    fn removal_claim_is_private_and_preserves_a_new_public_entry() {
        let fixture = tempfile::tempdir().unwrap();
        let tree = fixture.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        let parent = File::open(fixture.path()).unwrap();
        let opened = File::open(&tree).unwrap();
        let expected = directory_stat(opened.as_fd()).unwrap();
        remove_claimed_directory(
            parent.as_fd(),
            OsStr::new("tree"),
            &expected,
            |claimed_parent, claimed_name| {
                let private = directory_stat(claimed_parent)?;
                assert_eq!(private.st_mode & 0o777, 0o700);
                assert!(!tree.exists());
                std::fs::create_dir(&tree)?;
                std::fs::write(tree.join("keep"), b"replacement")?;
                unlink_entry(claimed_parent, claimed_name, libc::AT_REMOVEDIR)
            },
        )
        .unwrap();
        assert_eq!(std::fs::read(tree.join("keep")).unwrap(), b"replacement");
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_removal_claim_restores_without_clobbering_a_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let tree = fixture.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("keep"), b"original").unwrap();
        let parent = File::open(fixture.path()).unwrap();
        let opened = File::open(&tree).unwrap();
        let expected = directory_stat(opened.as_fd()).unwrap();

        let error =
            remove_claimed_directory(parent.as_fd(), OsStr::new("tree"), &expected, |_, _| {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            })
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        assert_eq!(std::fs::read(tree.join("keep")).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 1);

        let error =
            remove_claimed_directory(parent.as_fd(), OsStr::new("tree"), &expected, |_, _| {
                std::fs::create_dir(&tree)?;
                std::fs::write(tree.join("keep"), b"replacement")?;
                Err(io::Error::from_raw_os_error(libc::EPERM))
            })
            .unwrap_err();
        assert!(error.to_string().contains("cannot restore"));
        let retained = std::fs::read_dir(fixture.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .as_bytes()
                    .starts_with(b".awacs-removing-")
            })
            .unwrap();
        assert_eq!(
            std::fs::read(retained.join("target/keep")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read(tree.join("keep")).unwrap(), b"replacement");
    }

    #[test]
    fn strict_subvolume_removal_rejects_ordinary_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let tree = fixture.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("keep"), b"ordinary").unwrap();
        let error = remove_subvolume_tree(&tree).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read(tree.join("keep")).unwrap(), b"ordinary");
    }

    #[test]
    fn nested_subvolume_search_preserves_ordinary_contents() {
        let fixture = tempfile::tempdir().unwrap();
        let tree = fixture.path().join("tree");
        std::fs::create_dir_all(tree.join("ordinary/nested")).unwrap();
        std::fs::write(tree.join("ordinary/nested/keep"), b"ordinary").unwrap();
        std::os::unix::fs::symlink("ordinary/nested", tree.join("link")).unwrap();
        let directory = File::open(&tree).unwrap();

        remove_nested_subvolumes(directory.as_fd()).unwrap();
        assert_eq!(
            std::fs::read(tree.join("ordinary/nested/keep")).unwrap(),
            b"ordinary"
        );
        assert!(tree.join("link").is_symlink());
    }

    #[test]
    fn failed_subvolume_destroy_never_empties_the_directory() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("keep"), b"ordinary").unwrap();
        for errno in [
            libc::EPERM,
            libc::EACCES,
            libc::EIO,
            libc::ENOTTY,
            libc::EINVAL,
        ] {
            let error = destroy_with_nested_cleanup(
                || Err(io::Error::from_raw_os_error(errno)),
                || {
                    // A terminal ioctl error must never reach any cleanup.
                    remove_tree(fixture.path())
                },
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
            assert_eq!(
                std::fs::read(fixture.path().join("keep")).unwrap(),
                b"ordinary"
            );
        }

        let calls = std::cell::Cell::new(0);
        let error = destroy_with_nested_cleanup(
            || {
                calls.set(calls.get() + 1);
                Err(io::Error::from_raw_os_error(libc::ENOTEMPTY))
            },
            || Err(io::Error::from_raw_os_error(libc::EPERM)),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        assert_eq!(
            calls.get(),
            1,
            "do not retry the parent after nested deletion fails"
        );
        assert_eq!(
            std::fs::read(fixture.path().join("keep")).unwrap(),
            b"ordinary"
        );

        let calls = std::cell::Cell::new(0);
        let error = destroy_with_nested_cleanup(
            || {
                calls.set(calls.get() + 1);
                Err(io::Error::from_raw_os_error(libc::ENOTEMPTY))
            },
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOTEMPTY));
        assert_eq!(
            calls.get(),
            2,
            "a completed scan never substitutes for the final kernel destroy"
        );
    }

    #[test]
    #[ignore = "requires unprivileged JJ_TEST_BTRFS_ROOT and Btrfs deletion permissions"]
    fn remove_tree_skips_unreadable_directories_but_rejects_hidden_subvolumes() {
        use std::os::unix::fs::PermissionsExt as _;

        assert_ne!(
            unsafe { libc::geteuid() },
            0,
            "must run without root privileges"
        );
        let root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
            .map(PathBuf::from)
            .expect("JJ_TEST_BTRFS_ROOT must name a writable directory on real Btrfs");
        let fixture = tempfile::tempdir_in(root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(
                    self.0.join("private"),
                    std::fs::Permissions::from_mode(0o700),
                );
                let _ = remove_subvolume_tree(&self.0);
            }
        }
        let create_subvolume = |path: &Path| {
            assert!(
                Command::new("btrfs")
                    .args(["subvolume", "create"])
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        for hidden_subvolume in [false, true] {
            let tree = fixture
                .path()
                .join(if hidden_subvolume { "hidden" } else { "plain" });
            create_subvolume(&tree);
            let _cleanup = Cleanup(tree.clone());
            create_subvolume(&tree.join("visible"));
            let private = tree.join("private");
            std::fs::create_dir(&private).unwrap();
            if hidden_subvolume {
                create_subvolume(&private.join("nested"));
                std::fs::write(private.join("nested/keep"), b"hidden state").unwrap();
            }
            std::fs::write(private.join("keep"), b"ordinary private contents").unwrap();
            let mut retained = File::open(private.join("keep")).unwrap();
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o000)).unwrap();

            let result = remove_subvolume_tree(&tree);
            if hidden_subvolume {
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOTEMPTY));
                assert!(
                    tree.exists(),
                    "hidden nested subvolume must retain its parent"
                );
                std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
                assert_eq!(
                    std::fs::read(private.join("nested/keep")).unwrap(),
                    b"hidden state"
                );
            } else {
                result.unwrap();
                assert!(!tree.exists());
            }
            assert_eq!(retained.metadata().unwrap().nlink(), 1);
            let mut contents = Vec::new();
            retained.read_to_end(&mut contents).unwrap();
            assert_eq!(contents, b"ordinary private contents");
        }
    }

    #[test]
    #[ignore = "requires JJ_TEST_BTRFS_ROOT and Btrfs snapshot deletion permissions"]
    fn remove_tree_destroys_nested_subvolumes_without_unlinking_files() {
        let root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
            .map(PathBuf::from)
            .expect("JJ_TEST_BTRFS_ROOT must name a writable directory on real Btrfs");
        let fixture = tempfile::tempdir_in(root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = remove_tree(&self.0);
            }
        }
        let tree = fixture.path().join("ordinary");
        std::fs::create_dir(&tree).unwrap();
        let _cleanup = Cleanup(tree.clone());
        let source = tree.join("source");
        let metadata = source.join(".jj");
        let nested = metadata.join("ordinary/nested");
        for path in [&source, &metadata, &nested] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            assert!(
                Command::new("btrfs")
                    .args(["subvolume", "create"])
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
            assert_eq!(std::fs::metadata(path).unwrap().ino(), ROOT_INODE);
            std::fs::write(path.join("keep"), b"still present in deleted subvolume").unwrap();
        }
        let retained_files = [
            ("source", File::open(source.join("keep")).unwrap()),
            ("metadata", File::open(metadata.join("keep")).unwrap()),
            ("nested", File::open(nested.join("keep")).unwrap()),
        ];
        for (_, retained) in &retained_files {
            assert_eq!(retained.metadata().unwrap().nlink(), 1);
        }

        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
        for (label, mut retained) in retained_files {
            // Destroying the containing subvolume preserves the retained
            // inode's link count. Recursive unlinking would set it to zero.
            // Fresh path lookup through a destroyed root is kernel-dependent,
            // so inspect files opened before deletion instead.
            assert_eq!(
                retained.metadata().unwrap().nlink(),
                1,
                "{label} file was individually unlinked"
            );
            let mut contents = Vec::new();
            retained.read_to_end(&mut contents).unwrap();
            assert_eq!(contents, b"still present in deleted subvolume");
        }
    }

    #[test]
    #[ignore = "requires JJ_TEST_BTRFS_ROOT, Btrfs changed-object kernel support, and CAP_SYS_ADMIN"]
    fn synced_snapshot_identity_survives_comparison_with_open_unlinked_source() {
        use std::io::Write;

        use crate::broker::ChangedObjectsExecution;
        use crate::broker::ExpectedSubvolume;
        use crate::broker::execute_changed_objects;

        let root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
            .map(PathBuf::from)
            .expect("JJ_TEST_BTRFS_ROOT must name a writable directory on real Btrfs");
        let fixture = tempfile::tempdir_in(root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = remove_tree(&self.0);
            }
        }
        let tree = fixture.path().join("snapshots");
        std::fs::create_dir(&tree).unwrap();
        let _cleanup = Cleanup(tree.clone());
        let source = tree.join("source");
        assert!(
            Command::new("btrfs")
                .args(["subvolume", "create"])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(source.join("kept"), b"before\n").unwrap();
        let kept_ino = std::fs::metadata(source.join("kept")).unwrap().ino();
        let destination = File::open(&tree).unwrap();
        let source_fd = File::open(&source).unwrap();
        create_snapshot(source_fd.as_fd(), destination.as_fd(), b"parent", true).unwrap();
        let parent_path = tree.join("parent");
        sync_subvolume(File::open(&parent_path).unwrap().as_fd()).unwrap();
        let parent = OpenedSubvolume::open(&parent_path).unwrap();

        // Snapshot creation copies this inode while it is still open but no
        // longer linked. Cleanup in each new snapshot modifies its tree after
        // the transaction which created it has already committed.
        let orphan_path = source.join("orphan");
        let mut orphan = File::create(&orphan_path).unwrap();
        orphan.write_all(b"deleted but still open\n").unwrap();
        orphan.sync_all().unwrap();
        std::fs::remove_file(&orphan_path).unwrap();
        assert_eq!(orphan.metadata().unwrap().nlink(), 0);
        std::fs::write(source.join("kept"), b"after\n").unwrap();

        for attempt in 0..3 {
            let target_name = format!("target-{attempt}");
            create_snapshot(
                source_fd.as_fd(),
                destination.as_fd(),
                target_name.as_bytes(),
                true,
            )
            .unwrap();
            let target_path = tree.join(&target_name);
            sync_subvolume(File::open(&target_path).unwrap().as_fd()).unwrap();
            let target = OpenedSubvolume::open(&target_path).unwrap();
            let output = tempfile::NamedTempFile::new_in(fixture.path()).unwrap();
            let request = ChangedObjectsExecution {
                parent: ExpectedSubvolume::from_observed(&parent.filesystem, &parent.subvolume),
                target: ExpectedSubvolume::from_observed(&target.filesystem, &target.subvolume),
                output_owner_uid: output.as_file().metadata().unwrap().uid(),
                max_output_bytes: 1024 * 1024,
            };

            // This exercises the broker's exact before/after identity checks,
            // including generation. Without stabilization, send commits the
            // cleanup and rejects the target's changed generation afterward.
            let result = execute_changed_objects(
                &request,
                parent.as_fd(),
                target.as_fd(),
                output.as_file().as_fd(),
            )
            .unwrap();
            let bytes = std::fs::read(output.path()).unwrap();
            assert_eq!(bytes.len() as u64, result.output_bytes);
            let manifest = if result.v2_ioctl.is_some() {
                crate::manifest::parse_changed_objects_v2(&bytes)
                    .unwrap()
                    .manifest
            } else {
                crate::manifest::parse_changed_objects(&bytes).unwrap()
            };
            assert!(manifest.objects.contains_key(&kept_ino));
            parent.revalidate().unwrap();
            target.revalidate().unwrap();
        }
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
