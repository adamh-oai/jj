use crate::btrfs::{
    ChangedObjectsIoctlResult, FilesystemInfo, ROOT_INODE, SubvolumeInfo, changed_objects_v2,
    filesystem_info, send_changed_objects, subvolume_info,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Condvar, Mutex};
use uuid::Uuid;

pub const BROKER_PROTOCOL_VERSION: u16 = 4;
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;
pub const MAX_FRAME_FDS: usize = 4;
const FRAME_MAGIC: &[u8; 4] = b"BAWB";
const FRAME_HEADER_SIZE: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Opcode {
    Handshake = 1,
    ChangedObjects = 4,
}

impl Opcode {
    pub(crate) fn decode(value: u16) -> Result<Self, BrokerError> {
        match value {
            1 => Ok(Self::Handshake),
            4 => Ok(Self::ChangedObjects),
            _ => Err(BrokerError::new(format!("unknown broker opcode {value}"))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub opcode: Opcode,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(opcode: Opcode, payload: Vec<u8>) -> Result<Self, BrokerError> {
        if payload.len() > MAX_FRAME_PAYLOAD {
            return Err(BrokerError::new(format!(
                "broker payload is {} bytes, limit is {MAX_FRAME_PAYLOAD}",
                payload.len()
            )));
        }
        Ok(Self { opcode, payload })
    }
}

#[derive(Debug)]
pub struct ReceivedFrame {
    pub frame: Frame,
    pub fds: Vec<OwnedFd>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedSubvolume {
    pub filesystem_uuid: [u8; 16],
    pub subvolume_uuid: [u8; 16],
    pub root_id: u64,
    pub generation: u64,
    pub ctransid: u64,
    pub otransid: u64,
    pub parent_uuid: Option<[u8; 16]>,
    pub received_uuid: Option<[u8; 16]>,
    pub readonly: bool,
}

impl ExpectedSubvolume {
    pub fn from_observed(filesystem: &FilesystemInfo, subvolume: &SubvolumeInfo) -> Self {
        Self {
            filesystem_uuid: filesystem.fs_uuid,
            subvolume_uuid: subvolume.uuid,
            root_id: subvolume.root_id,
            generation: subvolume.generation,
            ctransid: subvolume.ctransid,
            otransid: subvolume.otransid,
            parent_uuid: subvolume.parent_uuid,
            received_uuid: subvolume.received_uuid,
            readonly: subvolume.readonly(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedObjectsExecution {
    pub parent: ExpectedSubvolume,
    pub target: ExpectedSubvolume,
    pub output_owner_uid: u32,
    pub max_output_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedObjectsResult {
    pub output_bytes: u64,
    pub manifest_hash: [u8; 32],
    /// Present only when the dedicated v2 ioctl succeeded. Legacy send-flag
    /// fallback has no kernel completion counters to prove.
    pub v2_ioctl: Option<ChangedObjectsIoctlResult>,
}

pub const MAX_CHANGED_OBJECT_OUTPUT: u64 = 1024 * 1024 * 1024;

pub fn execute_changed_objects(
    request: &ChangedObjectsExecution,
    parent_fd: BorrowedFd<'_>,
    target_fd: BorrowedFd<'_>,
    output_fd: BorrowedFd<'_>,
) -> Result<ChangedObjectsResult, BrokerError> {
    if request.max_output_bytes == 0 || request.max_output_bytes > MAX_CHANGED_OBJECT_OUTPUT {
        return Err(BrokerError::new(format!(
            "changed-object output limit must be between 1 and {MAX_CHANGED_OBJECT_OUTPUT}"
        )));
    }
    if request.parent.filesystem_uuid != request.target.filesystem_uuid
        || request.parent.root_id == request.target.root_id
        || !request.parent.readonly
        || !request.target.readonly
    {
        return Err(BrokerError::new(
            "changed-object endpoints must be distinct read-only roots on one filesystem",
        ));
    }
    let parent_before = verify_subvolume(parent_fd, &request.parent)?;
    let target_before = verify_subvolume(target_fd, &request.target)?;
    verify_output_file(output_fd, request.output_owner_uid, true)?;

    let v2_ioctl = match changed_objects_v2(
        target_fd,
        parent_fd,
        output_fd,
        request.max_output_bytes,
        100_000_000,
    ) {
        Ok(result) => {
            if result.output_bytes > request.max_output_bytes {
                return Err(BrokerError::new(
                    "v2 changed-object ioctl exceeded its output limit",
                ));
            }
            Some(result)
        }
        Err(error) if error.raw_os_error() == Some(libc::ENOTTY) => {
            // Transitional compatibility for kernels carrying only the
            // original private send flag. V2-capable kernels never fall back
            // after a validation, limit, or execution error.
            send_changed_objects(target_fd, parent_before.root_id, output_fd).map_err(
                |fallback| {
                    BrokerError::new(format!(
                        "v2 changed-object ioctl is unavailable ({error}); legacy ioctl failed: {fallback}"
                    ))
                },
            )?;
            None
        }
        Err(error) => {
            return Err(BrokerError::new(format!(
                "run fd-anchored changed-object ioctl: {error}"
            )));
        }
    };
    // A successful return must be durable before the manager can promote the
    // manifest from its fence-specific .part name.
    if unsafe { libc::fsync(output_fd.as_raw_fd()) } != 0 {
        return Err(BrokerError::io("fsync changed-object manifest"));
    }
    let parent_after = verify_subvolume(parent_fd, &request.parent)?;
    let target_after = verify_subvolume(target_fd, &request.target)?;
    if parent_after != parent_before || target_after != target_before {
        return Err(BrokerError::new(
            "changed-object endpoint metadata changed during comparison",
        ));
    }

    // Recheck the attributes which the manager could have changed while the
    // ioctl was running. The file is no longer empty, but it must still be the
    // same private, single-link, read-write regular file.
    verify_output_file(output_fd, request.output_owner_uid, false)?;
    let metadata = fd_metadata(output_fd)?;
    let output_bytes = u64::try_from(metadata.st_size)
        .map_err(|_| BrokerError::new("changed-object output has negative size"))?;
    if output_bytes > request.max_output_bytes {
        return Err(BrokerError::new(format!(
            "changed-object output is {output_bytes} bytes, limit is {}",
            request.max_output_bytes
        )));
    }
    if v2_ioctl.is_some_and(|result| result.output_bytes != output_bytes) {
        return Err(BrokerError::new(
            "v2 changed-object ioctl byte count differs from output length",
        ));
    }
    let manifest_hash = hash_fd(output_fd, output_bytes)?;
    Ok(ChangedObjectsResult {
        output_bytes,
        manifest_hash,
        v2_ioctl,
    })
}

fn verify_subvolume(
    fd: BorrowedFd<'_>,
    expected: &ExpectedSubvolume,
) -> Result<SubvolumeInfo, BrokerError> {
    let metadata = fd_metadata(fd)?;
    if metadata.st_ino != ROOT_INODE || metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(BrokerError::new(
            "broker subvolume fd is not directory inode 256",
        ));
    }
    let filesystem = filesystem_info(fd)
        .map_err(|error| BrokerError::new(format!("inspect Btrfs filesystem: {error}")))?;
    let subvolume = subvolume_info(fd)
        .map_err(|error| BrokerError::new(format!("inspect Btrfs subvolume: {error}")))?;
    if ExpectedSubvolume::from_observed(&filesystem, &subvolume) != *expected {
        return Err(BrokerError::new(
            "broker subvolume fd does not match the expected immutable identity",
        ));
    }
    Ok(subvolume)
}

fn verify_output_file(
    fd: BorrowedFd<'_>,
    expected_uid: u32,
    require_empty: bool,
) -> Result<(), BrokerError> {
    let metadata = fd_metadata(fd)?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_nlink != 1
        || metadata.st_uid != expected_uid
        || (require_empty && metadata.st_size != 0)
        || metadata.st_mode & 0o077 != 0
    {
        return Err(BrokerError::new(
            "broker output must be a new, private, single-link regular file owned by the manager",
        ));
    }
    // SAFETY: F_GETFL does not modify memory and fd remains borrowed.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(BrokerError::io("read changed-object output flags"));
    }
    if flags & libc::O_ACCMODE != libc::O_RDWR {
        return Err(BrokerError::new(
            "changed-object output fd must be open read-write",
        ));
    }
    Ok(())
}

fn fd_metadata(fd: BorrowedFd<'_>) -> Result<libc::stat, BrokerError> {
    // SAFETY: fstat initializes the provided stat buffer for a valid borrowed
    // descriptor and does not retain its address.
    unsafe {
        let mut metadata: libc::stat = zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut metadata) != 0 {
            return Err(BrokerError::io("fstat broker fd"));
        }
        Ok(metadata)
    }
}

fn hash_fd(fd: BorrowedFd<'_>, length: u64) -> Result<[u8; 32], BrokerError> {
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut offset = 0_u64;
    while offset < length {
        let wanted = usize::try_from((length - offset).min(buffer.len() as u64))
            .expect("bounded by local buffer");
        // SAFETY: pread writes at most wanted bytes into the live buffer and
        // does not change the shared file offset.
        let read = unsafe {
            libc::pread(
                fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                wanted,
                offset
                    .try_into()
                    .map_err(|_| BrokerError::new("manifest offset exceeds off_t"))?,
            )
        };
        if read < 0 {
            return Err(BrokerError::io("read changed-object manifest for hashing"));
        }
        if read == 0 {
            return Err(BrokerError::new(
                "changed-object manifest ended while hashing",
            ));
        }
        hash.update(&buffer[..read as usize]);
        offset += read as u64;
    }
    Ok(hash.finalize().into())
}

#[derive(Debug)]
pub struct SeqPacket {
    fd: OwnedFd,
}

#[derive(Debug)]
pub struct SeqPacketListener {
    fd: OwnedFd,
}

impl SeqPacket {
    pub fn pair() -> Result<(Self, Self), BrokerError> {
        let mut fds = [-1; 2];
        // SAFETY: fds points to two writable integers. On success ownership of
        // both returned descriptors is transferred into OwnedFd below.
        let result = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if result != 0 {
            return Err(BrokerError::io("create broker socketpair"));
        }
        // SAFETY: socketpair succeeded and returned two unique owned fds.
        Ok(unsafe {
            (
                Self {
                    fd: OwnedFd::from_raw_fd(fds[0]),
                },
                Self {
                    fd: OwnedFd::from_raw_fd(fds[1]),
                },
            )
        })
    }

    pub fn connect(path: &Path) -> Result<Self, BrokerError> {
        let (address, length) = unix_socket_address(path)?;
        // SAFETY: socket returns one owned descriptor on success.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(BrokerError::io("create broker client socket"));
        }
        // SAFETY: ownership of the just-created descriptor is transferred.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: address and its computed initialized length remain live.
        if unsafe { libc::connect(fd.as_raw_fd(), (&raw const address).cast(), length) } != 0 {
            return Err(BrokerError::io("connect broker socket"));
        }
        Ok(Self { fd })
    }

    pub fn send(&self, frame: &Frame, fds: &[BorrowedFd<'_>]) -> Result<(), BrokerError> {
        if frame.payload.len() > MAX_FRAME_PAYLOAD {
            return Err(BrokerError::new("broker payload exceeds frame limit"));
        }
        if fds.len() > MAX_FRAME_FDS {
            return Err(BrokerError::new(
                "too many file descriptors in broker frame",
            ));
        }
        let header = encode_header(frame, fds.len())?;
        let iovecs = [
            libc::iovec {
                iov_base: header.as_ptr().cast_mut().cast(),
                iov_len: header.len(),
            },
            libc::iovec {
                iov_base: frame.payload.as_ptr().cast_mut().cast(),
                iov_len: frame.payload.len(),
            },
        ];
        // This buffer is aligned for cmsghdr by using usize elements.
        let mut control = [0_usize; 16];
        // SAFETY: every pointer and length in msg refers to a live buffer for
        // the duration of sendmsg. SCM_RIGHTS contains valid borrowed fds.
        let sent = unsafe {
            let mut message: libc::msghdr = zeroed();
            message.msg_iov = iovecs.as_ptr().cast_mut();
            message.msg_iovlen = iovecs.len();
            if !fds.is_empty() {
                let control_len = libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) as usize;
                if control_len > std::mem::size_of_val(&control) {
                    return Err(BrokerError::new("SCM_RIGHTS control buffer overflow"));
                }
                message.msg_control = control.as_mut_ptr().cast();
                message.msg_controllen = control_len;
                let cmsg = libc::CMSG_FIRSTHDR(&message);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as usize;
                let destination = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for (index, fd) in fds.iter().enumerate() {
                    destination.add(index).write(fd.as_raw_fd());
                }
            }
            libc::sendmsg(self.fd.as_raw_fd(), &message, libc::MSG_NOSIGNAL)
        };
        if sent < 0 {
            return Err(BrokerError::io("send broker frame"));
        }
        let expected = header.len() + frame.payload.len();
        if sent as usize != expected {
            return Err(BrokerError::new(format!(
                "short broker packet write: {sent} of {expected} bytes"
            )));
        }
        Ok(())
    }

    pub fn receive(&self) -> Result<ReceivedFrame, BrokerError> {
        let mut bytes = vec![0_u8; FRAME_HEADER_SIZE + MAX_FRAME_PAYLOAD];
        let mut control = [0_usize; 16];
        // SAFETY: msg points at writable live byte/control buffers. Any fds
        // returned by the kernel are immediately wrapped or closed below.
        let (received, flags, raw_fds) = unsafe {
            let mut iovec = libc::iovec {
                iov_base: bytes.as_mut_ptr().cast(),
                iov_len: bytes.len(),
            };
            let mut message: libc::msghdr = zeroed();
            message.msg_iov = &mut iovec;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = std::mem::size_of_val(&control);
            let received = libc::recvmsg(self.fd.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC);
            if received < 0 {
                return Err(BrokerError::io("receive broker frame"));
            }
            let mut raw_fds = Vec::new();
            let mut cmsg = libc::CMSG_FIRSTHDR(&message);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
                    for fd in raw_fds {
                        libc::close(fd);
                    }
                    return Err(BrokerError::new(
                        "broker frame contains unsupported ancillary data",
                    ));
                }
                let header_len = libc::CMSG_LEN(0) as usize;
                let data_len = (*cmsg).cmsg_len.checked_sub(header_len).ok_or_else(|| {
                    BrokerError::new("broker frame has malformed ancillary length")
                })?;
                if data_len % size_of::<RawFd>() != 0 {
                    for fd in raw_fds {
                        libc::close(fd);
                    }
                    return Err(BrokerError::new(
                        "broker SCM_RIGHTS payload has partial descriptor",
                    ));
                }
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for index in 0..data_len / size_of::<RawFd>() {
                    raw_fds.push(data.add(index).read());
                }
                cmsg = libc::CMSG_NXTHDR(&message, cmsg);
            }
            (received as usize, message.msg_flags, raw_fds)
        };

        let fds: Vec<OwnedFd> = raw_fds
            .into_iter()
            .map(|fd| {
                // SAFETY: each descriptor was transferred exactly once by
                // SCM_RIGHTS and is now uniquely owned by this vector.
                unsafe { OwnedFd::from_raw_fd(fd) }
            })
            .collect();
        if flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(BrokerError::new(
                "broker frame or ancillary data was truncated",
            ));
        }
        if received < FRAME_HEADER_SIZE {
            return Err(BrokerError::new("truncated broker frame header"));
        }
        bytes.truncate(received);
        let (opcode, payload_len, fd_count) = decode_header(&bytes[..FRAME_HEADER_SIZE])?;
        if received != FRAME_HEADER_SIZE + payload_len {
            return Err(BrokerError::new(format!(
                "broker frame length mismatch: packet={received}, payload={payload_len}"
            )));
        }
        if fds.len() != fd_count {
            let actual = fds.len();
            return Err(BrokerError::new(format!(
                "broker frame declared {fd_count} descriptors but carried {}",
                actual
            )));
        }
        Ok(ReceivedFrame {
            frame: Frame {
                opcode,
                payload: bytes[FRAME_HEADER_SIZE..].to_vec(),
            },
            fds,
        })
    }

    pub fn peer_credentials(&self) -> Result<PeerCredentials, BrokerError> {
        // SAFETY: getsockopt writes one ucred to the initialized buffer and
        // updates its length. self owns a connected AF_UNIX socket.
        unsafe {
            let mut credentials: libc::ucred = zeroed();
            let mut length = size_of::<libc::ucred>() as libc::socklen_t;
            let result = libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            );
            if result != 0 {
                return Err(BrokerError::io("read broker peer credentials"));
            }
            if length as usize != size_of::<libc::ucred>() {
                return Err(BrokerError::new("unexpected SO_PEERCRED length"));
            }
            Ok(PeerCredentials {
                pid: credentials
                    .pid
                    .try_into()
                    .map_err(|_| BrokerError::new("SO_PEERCRED returned a negative process ID"))?,
                uid: credentials.uid,
                gid: credentials.gid,
            })
        }
    }
}

impl SeqPacketListener {
    /// Binds a new socket path. Existing entries are never unlinked.
    pub fn bind(path: &Path, mode: u32) -> Result<Self, BrokerError> {
        if mode & !0o777 != 0 {
            return Err(BrokerError::new(
                "broker socket mode contains unsupported bits",
            ));
        }
        let (address, length) = unix_socket_address(path)?;
        // SAFETY: socket returns one owned descriptor on success.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(BrokerError::io("create broker listener socket"));
        }
        // SAFETY: ownership of the just-created descriptor is transferred.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: address and its computed initialized length remain live.
        if unsafe { libc::bind(fd.as_raw_fd(), (&raw const address).cast(), length) } != 0 {
            return Err(BrokerError::io("bind broker socket"));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| BrokerError::new(format!("set broker socket mode: {error}")))?;
        // SAFETY: fd is a bound SOCK_SEQPACKET socket.
        if unsafe { libc::listen(fd.as_raw_fd(), 128) } != 0 {
            return Err(BrokerError::io("listen on broker socket"));
        }
        Ok(Self { fd })
    }

    pub fn accept(&self) -> Result<SeqPacket, BrokerError> {
        // SAFETY: accept4 returns one new owned descriptor and no peer address
        // is requested.
        let raw = unsafe {
            libc::accept4(
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(BrokerError::io("accept broker connection"));
        }
        // SAFETY: accept4 returned a fresh descriptor.
        Ok(SeqPacket {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        })
    }
}

fn unix_socket_address(path: &Path) -> Result<(libc::sockaddr_un, libc::socklen_t), BrokerError> {
    let bytes = path.as_os_str().as_bytes();
    if !path.is_absolute() || bytes.contains(&0) {
        return Err(BrokerError::new(
            "broker socket path must be absolute and contain no NUL",
        ));
    }
    // SAFETY: zero is a valid initialization for sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if bytes.len() >= address.sun_path.len() {
        return Err(BrokerError::new("broker socket path exceeds sockaddr_un"));
    }
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        .checked_add(bytes.len() + 1)
        .and_then(|value| libc::socklen_t::try_from(value).ok())
        .ok_or_else(|| BrokerError::new("broker socket address length overflow"))?;
    Ok((address, length))
}

fn encode_header(frame: &Frame, fd_count: usize) -> Result<[u8; FRAME_HEADER_SIZE], BrokerError> {
    let payload_len = u32::try_from(frame.payload.len())
        .map_err(|_| BrokerError::new("broker payload length exceeds u32"))?;
    let fd_count = u16::try_from(fd_count)
        .map_err(|_| BrokerError::new("broker descriptor count exceeds u16"))?;
    let mut header = [0; FRAME_HEADER_SIZE];
    header[..4].copy_from_slice(FRAME_MAGIC);
    header[4..6].copy_from_slice(&BROKER_PROTOCOL_VERSION.to_be_bytes());
    header[6..8].copy_from_slice(&(frame.opcode as u16).to_be_bytes());
    header[8..10].copy_from_slice(&0_u16.to_be_bytes());
    header[10..12].copy_from_slice(&fd_count.to_be_bytes());
    header[12..16].copy_from_slice(&payload_len.to_be_bytes());
    Ok(header)
}

fn decode_header(header: &[u8]) -> Result<(Opcode, usize, usize), BrokerError> {
    if header.len() != FRAME_HEADER_SIZE || &header[..4] != FRAME_MAGIC {
        return Err(BrokerError::new(
            "invalid broker frame magic or header length",
        ));
    }
    let version = u16::from_be_bytes(header[4..6].try_into().expect("fixed header"));
    if version != BROKER_PROTOCOL_VERSION {
        return Err(BrokerError::new(format!(
            "unsupported broker protocol version {version}"
        )));
    }
    let opcode = Opcode::decode(u16::from_be_bytes(
        header[6..8].try_into().expect("fixed header"),
    ))?;
    let flags = u16::from_be_bytes(header[8..10].try_into().expect("fixed header"));
    if flags != 0 {
        return Err(BrokerError::new(format!(
            "unsupported broker frame flags {flags:#x}"
        )));
    }
    let fd_count = usize::from(u16::from_be_bytes(
        header[10..12].try_into().expect("fixed header"),
    ));
    if fd_count > MAX_FRAME_FDS {
        return Err(BrokerError::new(
            "broker frame descriptor count exceeds limit",
        ));
    }
    let payload_len = u32::from_be_bytes(header[12..16].try_into().expect("fixed header")) as usize;
    if payload_len > MAX_FRAME_PAYLOAD {
        return Err(BrokerError::new("broker frame payload exceeds limit"));
    }
    Ok((opcode, payload_len, fd_count))
}

#[derive(Debug, Default)]
pub struct SessionGate {
    sessions: Mutex<BTreeMap<[u8; 16], SessionState>>,
    drained: Condvar,
    handshakes: Mutex<()>,
}

#[derive(Debug)]
struct SessionState {
    session_id: [u8; 16],
    in_flight: usize,
}

#[derive(Debug)]
pub struct SessionPermit<'a> {
    gate: &'a SessionGate,
    manager_store_uuid: [u8; 16],
}

impl SessionGate {
    pub fn handshake(&self, manager_store_uuid: [u8; 16]) -> [u8; 16] {
        // Only one handshake may install/wait a session at a time. Otherwise
        // two simultaneous recovery clients could each return an ID already
        // fenced by the other before either receives its response.
        let _handshake = self
            .handshakes
            .lock()
            .expect("broker handshake mutex poisoned");
        let session = *Uuid::new_v4().as_bytes();
        let mut sessions = self.sessions.lock().expect("broker session mutex poisoned");
        let in_flight = sessions
            .get(&manager_store_uuid)
            .map_or(0, |state| state.in_flight);
        sessions.insert(
            manager_store_uuid,
            SessionState {
                session_id: session,
                in_flight,
            },
        );
        while sessions
            .get(&manager_store_uuid)
            .is_some_and(|state| state.in_flight != 0)
        {
            sessions = self
                .drained
                .wait(sessions)
                .expect("broker session mutex poisoned while draining");
        }
        session
    }

    pub fn authorize(
        &self,
        manager_store_uuid: [u8; 16],
        manager_session_id: [u8; 16],
    ) -> Result<SessionPermit<'_>, BrokerError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| BrokerError::new("broker session mutex poisoned"))?;
        let Some(state) = sessions.get_mut(&manager_store_uuid) else {
            return Err(BrokerError::new("manager session has been fenced"));
        };
        if state.session_id != manager_session_id {
            return Err(BrokerError::new("manager session has been fenced"));
        }
        state.in_flight = state
            .in_flight
            .checked_add(1)
            .ok_or_else(|| BrokerError::new("broker in-flight request count overflow"))?;
        Ok(SessionPermit {
            gate: self,
            manager_store_uuid,
        })
    }

    pub fn join(
        &self,
        manager_store_uuid: [u8; 16],
        manager_session_id: [u8; 16],
    ) -> Result<(), BrokerError> {
        let permit = self.authorize(manager_store_uuid, manager_session_id)?;
        drop(permit);
        Ok(())
    }
}

impl Drop for SessionPermit<'_> {
    fn drop(&mut self) {
        let Ok(mut sessions) = self.gate.sessions.lock() else {
            return;
        };
        let Some(state) = sessions.get_mut(&self.manager_store_uuid) else {
            return;
        };
        debug_assert!(state.in_flight != 0);
        state.in_flight = state.in_flight.saturating_sub(1);
        if state.in_flight == 0 {
            self.gate.drained.notify_all();
        }
    }
}

#[derive(Debug)]
pub struct BrokerError {
    message: String,
    raw_os_error: Option<i32>,
}

impl BrokerError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            raw_os_error: None,
        }
    }

    fn io(context: &str) -> Self {
        let error = io::Error::last_os_error();
        Self {
            message: format!("{context}: {error}"),
            raw_os_error: error.raw_os_error(),
        }
    }

    pub fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }
}

impl fmt::Display for BrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BrokerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::os::fd::AsFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use tempfile::tempdir;

    #[test]
    fn seqpacket_round_trips_one_bounded_frame_and_rights() {
        let (left, right) = SeqPacket::pair().unwrap();
        let file = File::open("/dev/null").unwrap();
        let frame = Frame::new(Opcode::ChangedObjects, vec![0, 1, 2, 0xff]).unwrap();
        left.send(&frame, &[file.as_fd()]).unwrap();
        let received = right.receive().unwrap();
        assert_eq!(received.frame, frame);
        assert_eq!(received.fds.len(), 1);
        let flags = unsafe { libc::fcntl(received.fds[0].as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        match left.peer_credentials() {
            Ok(credentials) => assert_eq!(credentials.uid, unsafe { libc::geteuid() }),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                // Some sandboxes block SO_PEERCRED; production startup
                // treats this as fatal rather than bypassing authentication.
            }
            Err(error) => panic!("read peer credentials: {error}"),
        }
    }

    #[test]
    fn removed_broker_operations_are_not_protocol_opcodes() {
        assert!(Opcode::decode(2).is_err());
        assert!(Opcode::decode(3).is_err());
        assert!(Opcode::decode(5).is_err());
        assert!(Opcode::decode(7).is_err());
        assert!(Opcode::decode(9).is_err());
    }

    #[test]
    fn seqpacket_listener_connects_without_replacing_existing_paths() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("broker.sock");
        let listener = match SeqPacketListener::bind(&path, 0o600) {
            Ok(listener) => listener,
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => return,
            Err(error) => panic!("bind test broker socket: {error}"),
        };
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(SeqPacketListener::bind(&path, 0o600).is_err());
        let client = SeqPacket::connect(&path).unwrap();
        let server = listener.accept().unwrap();
        client
            .send(&Frame::new(Opcode::Handshake, vec![1, 2]).unwrap(), &[])
            .unwrap();
        assert_eq!(server.receive().unwrap().frame.payload, vec![1, 2]);
    }

    #[test]
    fn changed_object_output_requires_a_private_new_read_write_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manifest.part");
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let uid = unsafe { libc::geteuid() };
        verify_output_file(output.as_fd(), uid, true).unwrap();

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            verify_output_file(output.as_fd(), uid, true)
                .unwrap_err()
                .to_string()
                .contains("private")
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let alias = temp.path().join("manifest.alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(
            verify_output_file(output.as_fd(), uid, true)
                .unwrap_err()
                .to_string()
                .contains("single-link")
        );
    }

    #[test]
    fn changed_object_output_rejects_read_only_and_nonempty_files() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manifest.part");
        fs::write(&path, b"stale").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let uid = unsafe { libc::geteuid() };
        let read_only = File::open(&path).unwrap();
        assert!(verify_output_file(read_only.as_fd(), uid, true).is_err());

        let read_write = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(
            verify_output_file(read_write.as_fd(), uid, true)
                .unwrap_err()
                .to_string()
                .contains("new")
        );
        verify_output_file(read_write.as_fd(), uid, false).unwrap();
    }

    #[test]
    fn changed_object_execution_rejects_non_subvolume_fds_before_ioctl() {
        let null = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let endpoint = |root_id| ExpectedSubvolume {
            filesystem_uuid: [1; 16],
            subvolume_uuid: [root_id as u8; 16],
            root_id,
            generation: 1,
            ctransid: 1,
            otransid: 1,
            parent_uuid: None,
            received_uuid: None,
            readonly: true,
        };
        let request = ChangedObjectsExecution {
            parent: endpoint(10),
            target: endpoint(11),
            output_owner_uid: unsafe { libc::geteuid() },
            max_output_bytes: 1024,
        };
        assert!(
            execute_changed_objects(&request, null.as_fd(), null.as_fd(), null.as_fd())
                .unwrap_err()
                .to_string()
                .contains("inode 256")
        );
    }

    #[test]
    fn new_handshake_fences_prior_session() {
        let gate = SessionGate::default();
        let store = [1; 16];
        let first = gate.handshake(store);
        gate.join(store, first).unwrap();
        gate.authorize(store, first).unwrap();
        let second = gate.handshake(store);
        assert!(gate.authorize(store, first).is_err());
        gate.authorize(store, second).unwrap();
    }

    #[test]
    fn recovery_handshake_waits_for_an_authorized_request_to_drain() {
        let gate = std::sync::Arc::new(SessionGate::default());
        let store = [2; 16];
        let first = gate.handshake(store);
        let permit = gate.authorize(store, first).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let worker_gate = std::sync::Arc::clone(&gate);
        let worker = std::thread::spawn(move || {
            sent.send(worker_gate.handshake(store)).unwrap();
        });
        assert!(
            received
                .recv_timeout(std::time::Duration::from_millis(25))
                .is_err()
        );
        drop(permit);
        let second = received
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        worker.join().unwrap();
        assert!(gate.authorize(store, first).is_err());
        gate.authorize(store, second).unwrap();
    }
}
