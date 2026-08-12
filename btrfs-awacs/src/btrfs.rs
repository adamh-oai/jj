use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub const ROOT_INODE: u64 = 256;
pub const TOP_LEVEL_ROOT_ID: u64 = 5;
/// Input flag for `BTRFS_IOC_SNAP_CREATE_V2`.
pub const SUBVOL_RDONLY: u64 = 1 << 1;
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
    if result == 0 {
        Ok(())
    } else {
        Err(BtrfsError::context(name, io::Error::last_os_error()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn ioctl_numbers_and_layouts_match_linux_uapi() {
        assert_eq!(size_of::<BtrfsIoctlVolArgsV2>(), 4096);
        assert_eq!(size_of::<BtrfsIoctlFsInfoArgs>(), 1024);
        assert_eq!(size_of::<BtrfsIoctlGetSubvolInfoArgs>(), 504);
        assert_eq!(BTRFS_IOC_SNAP_CREATE_V2, 0x5000_9417);
        assert_eq!(BTRFS_IOC_FS_INFO, 0x8400_941f);
        assert_eq!(BTRFS_IOC_GET_SUBVOL_INFO, 0x81f8_943c);
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
        assert!(send_changed_objects(file.as_fd(), 0, file.as_fd())
            .unwrap_err()
            .to_string()
            .contains("must not be zero"));
    }
}
