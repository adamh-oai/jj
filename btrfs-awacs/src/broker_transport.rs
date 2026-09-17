//! Socket-free privileged broker transport.
//!
//! Managers publish bounded request files in a private shared directory. The
//! privileged broker receives the request file and a kernel-authenticated
//! requester pidfd through fanotify, duplicates the requested descriptors with
//! pidfd_getfd, and sends its framed response through an anonymous pipe.

use std::collections::VecDeque;
use std::ffi::CString;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::mem::size_of;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use uuid::Uuid;

use crate::broker::BrokerError;
use crate::broker::FRAME_HEADER_SIZE;
use crate::broker::Frame;
use crate::broker::MAX_FRAME_FDS;
use crate::broker::MAX_FRAME_PAYLOAD;
use crate::broker::PeerCredentials;
use crate::broker::ReceivedFrame;
use crate::broker::decode_header;
use crate::broker::encode_header;

pub const DEFAULT_BROKER_DIRECTORY: &str = "/tmp/btrfs-awacs-broker";
const REQUEST_PREFIX: &str = "request-";
const MAX_REQUEST_BYTES: usize =
    FRAME_HEADER_SIZE + MAX_FRAME_PAYLOAD + size_of::<i32>() * (MAX_FRAME_FDS + 1);
const FANOTIFY_BUFFER_WORDS: usize = 1024;

#[derive(Debug, Clone)]
pub struct FileBrokerClient {
    directory: PathBuf,
}

#[derive(Debug)]
pub struct FileBrokerListener {
    fanotify: OwnedFd,
    manager_uid: u32,
    pending: VecDeque<FileBrokerRequest>,
}

#[derive(Debug)]
pub struct FileBrokerRequest {
    pub peer: PeerCredentials,
    pub received: ReceivedFrame,
    response: OwnedFd,
}

#[derive(Debug)]
struct RequestPath {
    path: PathBuf,
}

impl Drop for RequestPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl FileBrokerClient {
    pub fn connect(directory: &Path) -> Result<Self, BrokerError> {
        validate_manager_directory(directory, unsafe { libc::geteuid() })?;
        Ok(Self {
            directory: directory.to_owned(),
        })
    }

    pub fn exchange(
        &self,
        frame: &Frame,
        descriptors: &[BorrowedFd<'_>],
    ) -> Result<ReceivedFrame, BrokerError> {
        if descriptors.len() > MAX_FRAME_FDS {
            return Err(BrokerError::new(
                "too many file descriptors in broker frame",
            ));
        }
        let (mut response_reader, response_writer) = response_pipe()?;
        let path = RequestPath {
            path: self
                .directory
                .join(format!("{REQUEST_PREFIX}{}", Uuid::new_v4())),
        };
        let mut request = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path.path)
            .map_err(|error| BrokerError::new(format!("create broker request file: {error}")))?;
        request
            .write_all(&encode_header(frame, descriptors.len())?)
            .and_then(|()| request.write_all(&frame.payload))
            .and_then(|()| request.write_all(&response_writer.as_raw_fd().to_be_bytes()))
            .map_err(|error| BrokerError::new(format!("write broker request file: {error}")))?;
        for descriptor in descriptors {
            request
                .write_all(&descriptor.as_raw_fd().to_be_bytes())
                .map_err(|error| {
                    BrokerError::new(format!("write broker request descriptor: {error}"))
                })?;
        }
        // FAN_CLOSE_WRITE is the publication boundary: all descriptor numbers
        // and frame bytes are complete before the broker sees this request.
        drop(request);

        let mut header = [0_u8; FRAME_HEADER_SIZE];
        // Keep the manager's write end alive until receiving the response
        // header: that proves pidfd_getfd already duplicated it into the
        // broker. Drop it afterward so a truncated response becomes EOF.
        read_first_response_bytes(&mut response_reader, &mut header)?;
        drop(response_writer);
        let (opcode, payload_len, fd_count) = decode_header(&header)?;
        if fd_count != 0 {
            return Err(BrokerError::new(
                "broker response pipe declared transferred descriptors",
            ));
        }
        let mut payload = vec![0_u8; payload_len];
        response_reader
            .read_exact(&mut payload)
            .map_err(|error| BrokerError::new(format!("read broker response payload: {error}")))?;
        Ok(ReceivedFrame {
            frame: Frame::new(opcode, payload)?,
            fds: Vec::new(),
        })
    }
}

impl FileBrokerListener {
    pub fn new(directory: &Path, manager_uid: u32) -> Result<Self, BrokerError> {
        validate_manager_directory(directory, manager_uid)?;
        // FAN_REPORT_PIDFD gives the broker a kernel-generated descriptor for
        // the actual event producer; no manager-supplied PID is trusted.
        let raw = unsafe {
            libc::fanotify_init(
                libc::FAN_CLOEXEC | libc::FAN_CLASS_NOTIF | libc::FAN_REPORT_PIDFD,
                (libc::O_RDONLY | libc::O_CLOEXEC) as u32,
            )
        };
        if raw < 0 {
            return Err(last_os_error("initialize pidfd-aware broker fanotify"));
        }
        let fanotify = unsafe { OwnedFd::from_raw_fd(raw) };
        let directory_name = CString::new(directory.as_os_str().as_bytes())
            .map_err(|_| BrokerError::new("broker request directory contains NUL"))?;
        let result = unsafe {
            libc::fanotify_mark(
                fanotify.as_raw_fd(),
                libc::FAN_MARK_ADD | libc::FAN_MARK_ONLYDIR,
                libc::FAN_CLOSE_WRITE | libc::FAN_EVENT_ON_CHILD,
                libc::AT_FDCWD,
                directory_name.as_ptr(),
            )
        };
        if result != 0 {
            return Err(last_os_error("watch broker request directory"));
        }
        Ok(Self {
            fanotify,
            manager_uid,
            pending: VecDeque::new(),
        })
    }

    pub fn receive(&mut self) -> Result<FileBrokerRequest, BrokerError> {
        loop {
            if let Some(request) = self.pending.pop_front() {
                return Ok(request);
            }
            let mut aligned = [0_u64; FANOTIFY_BUFFER_WORDS];
            let received = unsafe {
                libc::read(
                    self.fanotify.as_raw_fd(),
                    aligned.as_mut_ptr().cast(),
                    size_of::<[u64; FANOTIFY_BUFFER_WORDS]>(),
                )
            };
            if received < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(BrokerError::new(format!(
                    "read broker fanotify event: {error}"
                )));
            }
            if received == 0 {
                return Err(BrokerError::new("broker fanotify event stream closed"));
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(aligned.as_ptr().cast::<u8>(), received as usize)
            };
            let mut offset = 0;
            while offset < bytes.len() {
                let metadata_size = size_of::<libc::fanotify_event_metadata>();
                if bytes.len() - offset < metadata_size {
                    return Err(BrokerError::new("truncated broker fanotify event metadata"));
                }
                let metadata = unsafe {
                    std::ptr::read_unaligned(
                        bytes
                            .as_ptr()
                            .add(offset)
                            .cast::<libc::fanotify_event_metadata>(),
                    )
                };
                let event_len = metadata.event_len as usize;
                if event_len < metadata_size || event_len > bytes.len() - offset {
                    return Err(BrokerError::new("invalid broker fanotify event length"));
                }
                let event_bytes = &bytes[offset..offset + event_len];
                match self.decode_event(metadata, event_bytes) {
                    Ok(Some(request)) => self.pending.push_back(request),
                    Ok(None) => {}
                    Err(error) => log::warn!("ignored invalid AWACS broker request: {error}"),
                }
                offset += event_len;
            }
        }
    }

    fn decode_event(
        &self,
        metadata: libc::fanotify_event_metadata,
        bytes: &[u8],
    ) -> Result<Option<FileBrokerRequest>, BrokerError> {
        if metadata.vers != libc::FANOTIFY_METADATA_VERSION {
            return Err(BrokerError::new(
                "unsupported broker fanotify metadata version",
            ));
        }
        if metadata.mask & libc::FAN_Q_OVERFLOW != 0 {
            return Err(BrokerError::new("broker fanotify event queue overflowed"));
        }
        if metadata.fd < 0 {
            return Err(BrokerError::new(
                "broker fanotify event has no request file",
            ));
        }
        let request_file = unsafe { File::from_raw_fd(metadata.fd) };
        let mut pidfd = None;
        let mut cursor = usize::from(metadata.metadata_len);
        if cursor < size_of::<libc::fanotify_event_metadata>() || cursor > bytes.len() {
            return Err(BrokerError::new(
                "invalid broker fanotify metadata header length",
            ));
        }
        while cursor < bytes.len() {
            let header_size = size_of::<libc::fanotify_event_info_header>();
            if bytes.len() - cursor < header_size {
                return Err(BrokerError::new("truncated broker fanotify info header"));
            }
            let info = unsafe {
                std::ptr::read_unaligned(
                    bytes
                        .as_ptr()
                        .add(cursor)
                        .cast::<libc::fanotify_event_info_header>(),
                )
            };
            let info_len = usize::from(info.len);
            if info_len < header_size || info_len > bytes.len() - cursor {
                return Err(BrokerError::new("invalid broker fanotify info length"));
            }
            if info.info_type == libc::FAN_EVENT_INFO_TYPE_PIDFD {
                if info_len < size_of::<libc::fanotify_event_info_pidfd>() {
                    return Err(BrokerError::new("truncated broker fanotify pidfd record"));
                }
                let record = unsafe {
                    std::ptr::read_unaligned(
                        bytes
                            .as_ptr()
                            .add(cursor)
                            .cast::<libc::fanotify_event_info_pidfd>(),
                    )
                };
                if record.pidfd < 0 || pidfd.is_some() {
                    return Err(BrokerError::new(
                        "invalid or duplicate broker requester pidfd",
                    ));
                }
                pidfd = Some(unsafe { OwnedFd::from_raw_fd(record.pidfd) });
            }
            cursor += info_len;
        }
        if metadata.mask & libc::FAN_CLOSE_WRITE == 0 {
            return Ok(None);
        }
        let pidfd =
            pidfd.ok_or_else(|| BrokerError::new("broker request has no requester pidfd"))?;
        let pid = u32::try_from(metadata.pid)
            .map_err(|_| BrokerError::new("broker request has an invalid requester PID"))?;
        decode_request(request_file, pidfd, pid, self.manager_uid).map(Some)
    }
}

impl FileBrokerRequest {
    pub fn respond(self, frame: &Frame) -> Result<(), BrokerError> {
        let mut response = File::from(self.response);
        response
            .write_all(&encode_header(frame, 0)?)
            .and_then(|()| response.write_all(&frame.payload))
            .map_err(|error| BrokerError::new(format!("write broker response pipe: {error}")))
    }
}

fn decode_request(
    mut request_file: File,
    pidfd: OwnedFd,
    pid: u32,
    manager_uid: u32,
) -> Result<FileBrokerRequest, BrokerError> {
    let metadata = request_file
        .metadata()
        .map_err(|error| BrokerError::new(format!("stat broker request file: {error}")))?;
    if !metadata.is_file()
        || metadata.uid() != manager_uid
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() as usize > MAX_REQUEST_BYTES
    {
        return Err(BrokerError::new(
            "broker request must be a private manager-owned single-link regular file",
        ));
    }
    let credentials = requester_credentials(pid, manager_uid)?;
    let mut bytes = Vec::new();
    (&mut request_file)
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| BrokerError::new(format!("read broker request file: {error}")))?;
    if bytes.len() < FRAME_HEADER_SIZE + size_of::<RawFd>() {
        return Err(BrokerError::new("truncated broker request file"));
    }
    let (opcode, payload_len, fd_count) = decode_header(&bytes[..FRAME_HEADER_SIZE])?;
    let expected_len = FRAME_HEADER_SIZE + payload_len + size_of::<RawFd>() * (fd_count + 1);
    if bytes.len() != expected_len {
        return Err(BrokerError::new(
            "broker request file has an invalid frame length",
        ));
    }
    let payload_end = FRAME_HEADER_SIZE + payload_len;
    let raw_fds = &bytes[payload_end..];
    let response_fd = decode_raw_fd(&raw_fds[..size_of::<RawFd>()])?;
    let response = duplicate_process_fd(pidfd.as_fd(), response_fd)?;
    verify_response_pipe(response.as_fd())?;
    let mut descriptors = Vec::with_capacity(fd_count);
    for raw in raw_fds[size_of::<RawFd>()..].chunks_exact(size_of::<RawFd>()) {
        descriptors.push(duplicate_process_fd(pidfd.as_fd(), decode_raw_fd(raw)?)?);
    }
    // Recheck after descriptor extraction so a process that changes identity
    // while the broker is handling its event cannot bypass UID authorization.
    let after = requester_credentials(pid, manager_uid)?;
    if after != credentials {
        return Err(BrokerError::new(
            "broker requester credentials changed during request",
        ));
    }
    Ok(FileBrokerRequest {
        peer: credentials,
        received: ReceivedFrame {
            frame: Frame::new(opcode, bytes[FRAME_HEADER_SIZE..payload_end].to_vec())?,
            fds: descriptors,
        },
        response,
    })
}

fn requester_credentials(pid: u32, manager_uid: u32) -> Result<PeerCredentials, BrokerError> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))
        .map_err(|error| BrokerError::new(format!("read broker requester credentials: {error}")))?;
    let uid = parse_identity_line(&status, "Uid:")?;
    let gid = parse_identity_line(&status, "Gid:")?;
    if uid.iter().any(|candidate| *candidate != manager_uid) {
        return Err(BrokerError::new(
            "broker requester is not the configured manager UID",
        ));
    }
    Ok(PeerCredentials {
        pid,
        uid: uid[1],
        gid: gid[1],
    })
}

fn parse_identity_line(status: &str, prefix: &str) -> Result<[u32; 4], BrokerError> {
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .ok_or_else(|| BrokerError::new("broker requester status lacks identity credentials"))?;
    let values: Vec<u32> = line
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| {
            BrokerError::new("broker requester status has invalid identity credentials")
        })?;
    values
        .try_into()
        .map_err(|_| BrokerError::new("broker requester status has invalid identity count"))
}

fn duplicate_process_fd(pidfd: BorrowedFd<'_>, target: RawFd) -> Result<OwnedFd, BrokerError> {
    if target < 0 {
        return Err(BrokerError::new(
            "broker request contains a negative descriptor",
        ));
    }
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), target, 0_u32) };
    if raw < 0 {
        return Err(last_os_error("duplicate broker requester descriptor"));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw as RawFd) })
}

fn verify_response_pipe(fd: BorrowedFd<'_>) -> Result<(), BrokerError> {
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut metadata) } != 0 {
        return Err(last_os_error("stat broker response pipe"));
    }
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(last_os_error("read broker response pipe flags"));
    }
    if metadata.st_mode & libc::S_IFMT != libc::S_IFIFO || flags & libc::O_ACCMODE != libc::O_WRONLY
    {
        return Err(BrokerError::new(
            "broker response descriptor must be a write-only pipe",
        ));
    }
    Ok(())
}

fn response_pipe() -> Result<(File, OwnedFd), BrokerError> {
    let mut raw = [-1; 2];
    if unsafe { libc::pipe2(raw.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(last_os_error("create broker response pipe"));
    }
    Ok(unsafe { (File::from_raw_fd(raw[0]), OwnedFd::from_raw_fd(raw[1])) })
}

fn read_first_response_bytes(reader: &mut File, header: &mut [u8]) -> Result<(), BrokerError> {
    let mut offset = 0;
    while offset < header.len() {
        match reader.read(&mut header[offset..]) {
            Ok(0) => {
                return Err(BrokerError::new(
                    "broker response pipe closed before its header",
                ));
            }
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(BrokerError::new(format!(
                    "read broker response header: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn decode_raw_fd(bytes: &[u8]) -> Result<RawFd, BrokerError> {
    let raw = i32::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| BrokerError::new("broker request contains a malformed descriptor"))?,
    );
    if raw < 0 {
        return Err(BrokerError::new(
            "broker request contains a negative descriptor",
        ));
    }
    Ok(raw)
}

fn validate_manager_directory(path: &Path, manager_uid: u32) -> Result<(), BrokerError> {
    if !path.is_absolute() {
        return Err(BrokerError::new(
            "broker request directory must be absolute",
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| BrokerError::new(format!("stat broker request directory: {error}")))?;
    if !metadata.is_dir()
        || metadata.uid() != manager_uid
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(BrokerError::new(
            "broker request directory must be manager-owned and mode 0700",
        ));
    }
    Ok(())
}

fn last_os_error(context: &str) -> BrokerError {
    BrokerError::new(format!("{context}: {}", io::Error::last_os_error()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    use tempfile::tempdir;

    use super::*;
    use crate::broker::ExpectedSubvolume;
    use crate::broker::NestedSubvolumesExecution;
    use crate::broker::Opcode;
    use crate::broker_protocol::BrokerClient;

    #[test]
    fn file_transport_round_trips_frame_and_duplicates_request_descriptors() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let client = FileBrokerClient::connect(directory.path()).unwrap();
        let manager_uid = unsafe { libc::geteuid() };
        let client_thread = thread::spawn(move || {
            let operation_file = File::open("/dev/null").unwrap();
            let frame = Frame::new(Opcode::ChangedObjects, vec![4, 5, 6]).unwrap();
            client.exchange(&frame, &[operation_file.as_fd()]).unwrap()
        });

        let request_path = wait_for_request(directory.path());
        let request_file = File::open(&request_path).unwrap();
        let pidfd = open_current_pidfd();
        let request = decode_request(request_file, pidfd, std::process::id(), manager_uid).unwrap();
        assert_eq!(request.peer.uid, manager_uid);
        assert_eq!(request.received.frame.opcode, Opcode::ChangedObjects);
        assert_eq!(request.received.frame.payload, vec![4, 5, 6]);
        assert_eq!(request.received.fds.len(), 1);
        let flags = unsafe { libc::fcntl(request.received.fds[0].as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        request
            .respond(&Frame::new(Opcode::ChangedObjects, vec![0, 9, 8]).unwrap())
            .unwrap();

        let response = client_thread.join().unwrap();
        assert_eq!(response.frame.opcode, Opcode::ChangedObjects);
        assert_eq!(response.frame.payload, vec![0, 9, 8]);
        assert!(response.fds.is_empty());
        assert!(!request_path.exists());
    }

    #[test]
    fn file_transport_rejects_directory_without_private_mode() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let error = FileBrokerClient::connect(directory.path()).unwrap_err();
        assert!(error.to_string().contains("manager-owned and mode 0700"));
    }

    #[test]
    fn requester_credentials_reject_other_manager_uid() {
        let manager_uid = unsafe { libc::geteuid() };
        let other_uid = manager_uid.checked_add(1).unwrap_or(manager_uid - 1);
        let error = requester_credentials(std::process::id(), other_uid).unwrap_err();
        assert!(error.to_string().contains("configured manager UID"));
    }

    #[test]
    fn response_descriptor_must_be_a_write_only_pipe() {
        let regular = File::open("/dev/null").unwrap();
        let error = verify_response_pipe(regular.as_fd()).unwrap_err();
        assert!(error.to_string().contains("write-only pipe"));

        let (reader, writer) = response_pipe().unwrap();
        verify_response_pipe(writer.as_fd()).unwrap();
        let error = verify_response_pipe(reader.as_fd()).unwrap_err();
        assert!(error.to_string().contains("write-only pipe"));
    }

    #[test]
    fn identity_lines_require_all_four_kernel_credential_values() {
        assert_eq!(
            parse_identity_line("Uid:\t1\t2\t3\t4\n", "Uid:").unwrap(),
            [1, 2, 3, 4]
        );
        assert!(parse_identity_line("Uid:\t1\t2\t3\n", "Uid:").is_err());
        assert!(parse_identity_line("Uid:\t1\t2\tx\t4\n", "Uid:").is_err());
    }

    #[test]
    #[ignore = "requires a separately running root fanotify broker"]
    fn external_fanotify_broker_authenticates_and_duplicates_descriptors() {
        let path = std::env::var_os("BTRFS_AWACS_TEST_BROKER_DIRECTORY")
            .map(PathBuf::from)
            .expect("BTRFS_AWACS_TEST_BROKER_DIRECTORY must name a live root broker");
        let store_uuid = *Uuid::new_v4().as_bytes();
        let client = BrokerClient::connect_endpoint(&path, store_uuid).unwrap();
        let joined =
            BrokerClient::connect_existing_endpoint(&path, store_uuid, client.session_id())
                .unwrap();
        assert_eq!(joined.session_id(), client.session_id());
        let target = File::open("/dev/null").unwrap();
        let error = client
            .has_nested_subvolumes(
                &NestedSubvolumesExecution {
                    target: ExpectedSubvolume {
                        filesystem_uuid: [1; 16],
                        subvolume_uuid: [2; 16],
                        root_id: 256,
                        generation: 1,
                        ctransid: 1,
                        otransid: 1,
                        parent_uuid: None,
                        received_uuid: None,
                        readonly: true,
                    },
                },
                target.as_fd(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("broker rejected request"));

        let replacement = BrokerClient::connect_endpoint(&path, store_uuid).unwrap();
        assert_ne!(replacement.session_id(), client.session_id());
        let error = BrokerClient::connect_existing_endpoint(&path, store_uuid, client.session_id())
            .unwrap_err();
        assert!(error.to_string().contains("session has been fenced"));
    }

    fn wait_for_request(directory: &Path) -> PathBuf {
        let started = Instant::now();
        loop {
            if let Some(path) = fs::read_dir(directory)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .as_bytes()
                        .starts_with(REQUEST_PREFIX.as_bytes())
                })
            {
                // The manager closes the request only after writing the full
                // frame; wait for its published bytes before simulating fanotify.
                if fs::metadata(&path).unwrap().len() >= FRAME_HEADER_SIZE as u64 + 11 {
                    return path;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn open_current_pidfd() -> OwnedFd {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0_u32) };
        assert!(raw >= 0, "pidfd_open: {}", io::Error::last_os_error());
        unsafe { OwnedFd::from_raw_fd(raw as RawFd) }
    }
}
