//! Connection-bound Unix-stream framing for the per-user Watchman endpoint.
//!
//! The connected peer is authenticated once with `SO_PEERCRED` and
//! `SO_PEERPIDFD`. Every byte on that Unix stream is then bound to that same
//! kernel-identified peer.

use crate::bser::{decode_frame, Limits};
use crate::facade::FacadeService;
use crate::namespace::ViewBinding;
use crate::watchman::{PreparedWatchmanFrame, WatchmanEndpoint};
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

const BSER_V2_HEADER: &[u8; 6] = b"\0\x02\0\0\0\0";
const BSER_INT8: u8 = 0x03;
const BSER_INT16: u8 = 0x04;
const BSER_INT32: u8 = 0x05;
const BSER_INT64: u8 = 0x06;
const SO_PEERPIDFD: libc::c_int = 77;
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct FrameIdentity {
    pub pid: libc::pid_t,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    pub pidfd: OwnedFd,
}

impl FrameIdentity {
    pub fn verify_view(&self, binding: &ViewBinding) -> Result<(), TransportError> {
        if self.pid <= 0 {
            return Err(TransportError::new("sender credential PID is invalid"));
        }
        let fdinfo =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", self.pidfd.as_raw_fd()))
                .map_err(|error| TransportError::context("read sender pidfd identity", error))?;
        let pidfd_pid = fdinfo
            .lines()
            .find_map(|line| line.strip_prefix("Pid:\t"))
            .ok_or_else(|| TransportError::new("pidfd info omitted its PID"))?;
        if pidfd_pid.parse::<libc::pid_t>().ok() != Some(self.pid) {
            return Err(TransportError::new(
                "SCM_PIDFD does not match SCM_CREDENTIALS",
            ));
        }

        let proc = PathBuf::from(format!("/proc/{}", self.pid));
        let mount_namespace = std::fs::metadata(proc.join("ns/mnt"))
            .map_err(|error| TransportError::context("stat sender mount namespace", error))?;
        let process_root = std::fs::metadata(proc.join("root"))
            .map_err(|error| TransportError::context("stat sender process root", error))?;
        let process_root_fd = File::open(proc.join("root"))
            .map_err(|error| TransportError::context("open sender process root", error))?;
        if mount_namespace.dev() != binding.mount_ns_dev
            || mount_namespace.ino() != binding.mount_ns_ino
            || process_root.dev() != binding.process_root_dev
            || process_root.ino() != binding.process_root_ino
            || mount_id(process_root_fd.as_raw_fd())? != binding.process_root_mnt_id
        {
            return Err(TransportError::new(
                "sender mount namespace or process root differs from the watch view",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct AuthenticatedFrame {
    pub bytes: Vec<u8>,
    pub identity: FrameIdentity,
}

pub struct CredentialedStream {
    stream: UnixStream,
    peer_identity: FrameIdentity,
}

impl CredentialedStream {
    pub fn new(stream: UnixStream) -> Result<Self, TransportError> {
        let peer_identity = connected_peer_identity(stream.as_raw_fd())?;
        stream
            .set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT))
            .map_err(|error| TransportError::context("set response write timeout", error))?;
        Ok(Self {
            stream,
            peer_identity,
        })
    }

    pub fn receive_frame(&self, limits: Limits) -> Result<AuthenticatedFrame, TransportError> {
        let mut bytes = Vec::with_capacity(4096);
        let mut identity = None;
        self.receive_exact(&mut bytes, 7, &mut identity)?;
        if bytes.get(..6) != Some(BSER_V2_HEADER) {
            return Err(TransportError::new("invalid BSER-v2 stream header"));
        }
        let integer_bytes = match bytes[6] {
            BSER_INT8 => 1,
            BSER_INT16 => 2,
            BSER_INT32 => 4,
            BSER_INT64 => 8,
            _ => return Err(TransportError::new("invalid BSER frame length marker")),
        };
        self.receive_exact(&mut bytes, integer_bytes, &mut identity)?;
        let payload_length = decode_length(&bytes[6..])?;
        let framing_length = bytes.len();
        let total_length = framing_length
            .checked_add(payload_length)
            .ok_or_else(|| TransportError::new("BSER frame length overflow"))?;
        if total_length > limits.frame_bytes {
            return Err(TransportError::new("BSER frame exceeds its byte limit"));
        }
        self.receive_exact(&mut bytes, payload_length, &mut identity)?;
        Ok(AuthenticatedFrame {
            bytes,
            identity: identity.ok_or_else(|| TransportError::new("empty authenticated frame"))?,
        })
    }

    pub fn send_frame(&mut self, frame: &[u8], limits: Limits) -> Result<(), TransportError> {
        if frame.len() > limits.frame_bytes {
            return Err(TransportError::new("response frame exceeds its byte limit"));
        }
        self.stream
            .write_all(frame)
            .map_err(|error| TransportError::context("write response frame", error))
    }

    pub fn serve_one_frame(
        &mut self,
        endpoint: &WatchmanEndpoint,
        facade: &mut FacadeService,
        now_ns: i64,
        limits: Limits,
    ) -> Result<(), TransportError> {
        let frame = self.receive_frame(limits)?;
        self.serve_authenticated_frame(endpoint, facade, frame, now_ns, limits)
    }

    pub fn serve_authenticated_frame(
        &mut self,
        endpoint: &WatchmanEndpoint,
        facade: &mut FacadeService,
        frame: AuthenticatedFrame,
        now_ns: i64,
        limits: Limits,
    ) -> Result<(), TransportError> {
        let response = self.prepare_authenticated_frame(endpoint, facade, frame, now_ns, limits)?;
        let write = self.send_prepared_frame(&response, limits);
        let release = self.finish_prepared_frame(endpoint, facade, response);
        combine_write_and_release(write, release)
    }

    pub fn prepare_authenticated_frame(
        &self,
        endpoint: &WatchmanEndpoint,
        facade: &mut FacadeService,
        frame: AuthenticatedFrame,
        now_ns: i64,
        limits: Limits,
    ) -> Result<PreparedWatchmanFrame, TransportError> {
        self.decode_and_authorize(endpoint, facade, &frame, limits)?;
        endpoint
            .prepare_frame(
                facade,
                &frame.bytes,
                frame.identity.uid,
                frame.identity.gid,
                now_ns,
                limits,
            )
            .map_err(|error| TransportError::context("dispatch Watchman frame", error))
    }

    pub fn decode_and_authorize(
        &self,
        endpoint: &WatchmanEndpoint,
        facade: &FacadeService,
        frame: &AuthenticatedFrame,
        limits: Limits,
    ) -> Result<crate::bser::Value, TransportError> {
        let request = decode_frame(&frame.bytes, limits)
            .map_err(|error| TransportError::context("decode request for authorization", error))?;
        let binding = endpoint
            .authorize_request(facade, &request, frame.identity.uid, frame.identity.gid)
            .map_err(|error| TransportError::context("authorize Watchman frame", error))?;
        frame.identity.verify_view(binding)?;
        Ok(request)
    }

    pub fn send_prepared_frame(
        &mut self,
        response: &PreparedWatchmanFrame,
        limits: Limits,
    ) -> Result<(), TransportError> {
        self.send_frame(&response.bytes, limits)
    }

    pub fn finish_prepared_frame(
        &self,
        endpoint: &WatchmanEndpoint,
        facade: &mut FacadeService,
        response: PreparedWatchmanFrame,
    ) -> Result<(), TransportError> {
        endpoint
            .finish_frame(facade, response)
            .map_err(|error| TransportError::context("release Watchman response fence", error))
    }

    fn receive_exact(
        &self,
        output: &mut Vec<u8>,
        mut remaining: usize,
        frame_identity: &mut Option<FrameIdentity>,
    ) -> Result<(), TransportError> {
        while remaining != 0 {
            let old_length = output.len();
            output.resize(old_length + remaining, 0);
            let (received, span_identity) = recv_authenticated(
                self.stream.as_raw_fd(),
                &mut output[old_length..old_length + remaining],
                &self.peer_identity,
            )?;
            if received == 0 {
                output.truncate(old_length);
                return Err(TransportError::new("connection closed within a BSER frame"));
            }
            output.truncate(old_length + received);
            remaining -= received;
            match frame_identity {
                Some(identity)
                    if identity.pid != span_identity.pid
                        || identity.uid != span_identity.uid
                        || identity.gid != span_identity.gid =>
                {
                    return Err(TransportError::new(
                        "BSER frame contains byte spans from different senders",
                    ));
                }
                Some(_) => {}
                None => *frame_identity = Some(span_identity),
            }
        }
        Ok(())
    }
}

fn combine_write_and_release(
    write: Result<(), TransportError>,
    release: Result<(), TransportError>,
) -> Result<(), TransportError> {
    match (write, release) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(write), Ok(())) => Err(write),
        (Ok(()), Err(release)) => Err(TransportError::context(
            "release Watchman response fence after write",
            release,
        )),
        (Err(write), Err(release)) => Err(TransportError::new(format!(
            "{write}; release Watchman response fence after failed write: {release}"
        ))),
    }
}

fn connected_peer_identity(fd: RawFd) -> Result<FrameIdentity, TransportError> {
    let mut credentials: libc::ucred = unsafe { zeroed() };
    let mut credentials_length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the output buffer and length are initialized and live for the syscall.
    let credentials_result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut credentials_length,
        )
    };
    if credentials_result != 0 || credentials_length as usize != size_of::<libc::ucred>() {
        return Err(TransportError::context(
            "read connected peer credentials",
            io::Error::last_os_error(),
        ));
    }

    let mut raw_pidfd: libc::c_int = -1;
    let mut pidfd_length = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the output buffer and length are initialized and live for the syscall.
    let pidfd_result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_PEERPIDFD,
            (&raw mut raw_pidfd).cast(),
            &raw mut pidfd_length,
        )
    };
    if pidfd_result != 0 || pidfd_length as usize != size_of::<libc::c_int>() || raw_pidfd < 0 {
        return Err(TransportError::context(
            "read connected peer pidfd",
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: a successful SO_PEERPIDFD getsockopt returned a new owned fd.
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw_pidfd) };
    Ok(FrameIdentity {
        pid: credentials.pid,
        uid: credentials.uid,
        gid: credentials.gid,
        pidfd,
    })
}

fn recv_authenticated(
    fd: RawFd,
    destination: &mut [u8],
    peer_identity: &FrameIdentity,
) -> Result<(usize, FrameIdentity), TransportError> {
    // SAFETY: `destination` is a live writable byte slice and `fd` is the
    // connected Unix stream authenticated when `CredentialedStream` was made.
    let received = unsafe { libc::recv(fd, destination.as_mut_ptr().cast(), destination.len(), 0) };
    if received < 0 {
        return Err(TransportError::context(
            "receive connected peer frame span",
            io::Error::last_os_error(),
        ));
    }
    let pidfd = peer_identity
        .pidfd
        .try_clone()
        .map_err(|error| TransportError::context("clone connected peer pidfd", error))?;
    Ok((
        received as usize,
        FrameIdentity {
            pid: peer_identity.pid,
            uid: peer_identity.uid,
            gid: peer_identity.gid,
            pidfd,
        },
    ))
}

fn decode_length(encoded: &[u8]) -> Result<usize, TransportError> {
    let signed = match encoded {
        [BSER_INT8, bytes @ ..] if bytes.len() == 1 => i64::from(i8::from_le_bytes([bytes[0]])),
        [BSER_INT16, bytes @ ..] if bytes.len() == 2 => {
            i64::from(i16::from_le_bytes(bytes.try_into().unwrap()))
        }
        [BSER_INT32, bytes @ ..] if bytes.len() == 4 => {
            i64::from(i32::from_le_bytes(bytes.try_into().unwrap()))
        }
        [BSER_INT64, bytes @ ..] if bytes.len() == 8 => {
            i64::from_le_bytes(bytes.try_into().unwrap())
        }
        _ => return Err(TransportError::new("invalid BSER frame length encoding")),
    };
    usize::try_from(signed).map_err(|_| TransportError::new("negative BSER frame length"))
}

fn mount_id(fd: RawFd) -> Result<u64, TransportError> {
    // SAFETY: statx is an all-zero output structure before the syscall.
    let mut statx: libc::statx = unsafe { zeroed() };
    // SAFETY: fd is a live directory, the empty C string is valid, and
    // AT_EMPTY_PATH asks statx to inspect that descriptor.
    let result = unsafe {
        libc::statx(
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_NO_AUTOMOUNT,
            libc::STATX_MNT_ID,
            &raw mut statx,
        )
    };
    if result == 0 {
        Ok(statx.stx_mnt_id)
    } else {
        Err(TransportError::context(
            "stat sender process-root mount ID",
            io::Error::last_os_error(),
        ))
    }
}

#[derive(Debug)]
pub struct TransportError {
    message: String,
}

impl TransportError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn context(context: impl fmt::Display, error: impl fmt::Display) -> Self {
        Self::new(format!("{context}: {error}"))
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TransportError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bser::{decode_frame, encode_frame, Value};

    #[test]
    fn receives_a_bser_frame_with_kernel_credentials_and_pidfd() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let receiver = match CredentialedStream::new(receiver) {
            Ok(receiver) => receiver,
            Err(error) if error.to_string().contains("Operation not permitted") => return,
            Err(error) => panic!("arm credentialed stream: {error}"),
        };
        let expected = Value::Array(vec![Value::Bytes(b"clock".to_vec())]);
        let bytes = encode_frame(&expected, Limits::default()).unwrap();
        sender.write_all(&bytes).unwrap();
        let received = receiver.receive_frame(Limits::default()).unwrap();
        assert_eq!(
            decode_frame(&received.bytes, Limits::default()).unwrap(),
            expected
        );
        assert_eq!(received.identity.pid, std::process::id() as libc::pid_t);
        // SAFETY: These libc accessors have no preconditions.
        assert_eq!(received.identity.uid, unsafe { libc::geteuid() });
        assert_eq!(received.identity.gid, unsafe { libc::getegid() });
    }

    #[test]
    fn accepts_a_frame_queued_before_the_receiver_is_created() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let expected = Value::Array(vec![Value::Bytes(b"clock".to_vec())]);
        let bytes = encode_frame(&expected, Limits::default()).unwrap();
        sender.write_all(&bytes).unwrap();
        let receiver = match CredentialedStream::new(receiver) {
            Ok(receiver) => receiver,
            Err(error) if error.to_string().contains("Operation not permitted") => return,
            Err(error) => panic!("authenticate connected stream: {error}"),
        };
        let received = receiver.receive_frame(Limits::default()).unwrap();
        assert_eq!(
            decode_frame(&received.bytes, Limits::default()).unwrap(),
            expected
        );
    }

    #[test]
    fn refuses_an_oversized_length_before_reading_the_payload() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let receiver = match CredentialedStream::new(receiver) {
            Ok(receiver) => receiver,
            Err(error) if error.to_string().contains("Operation not permitted") => return,
            Err(error) => panic!("arm credentialed stream: {error}"),
        };
        sender
            .write_all(b"\0\x02\0\0\0\0\x06\0\0\0\0\x01\0\0\0")
            .unwrap();
        assert!(receiver
            .receive_frame(Limits {
                frame_bytes: 128,
                ..Limits::default()
            })
            .is_err());
    }
}
