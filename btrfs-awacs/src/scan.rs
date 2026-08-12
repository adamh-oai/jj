//! Transport-independent client API for immutable snapshot scans.
//!
//! Consumers such as Jujutsu use this module instead of speaking AWACS's
//! broker or discovery protocols directly. Implementations may use a local
//! service, a Unix socket, or a test double, but every successful begin keeps
//! one read-only directory handle alive until the lease is finished.

use std::error::Error;
use std::ffi::CString;
use std::fmt;
use std::fs::File;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A request to cut or reuse one immutable scan snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeginScanRequest {
    /// Absolute live working-copy root whose identity AWACS must validate.
    pub live_root: PathBuf,
    /// Opaque cursor persisted after a previously committed scan, if any.
    pub previous_cursor: Option<Vec<u8>>,
}

/// The paths which a client may scan incrementally inside the leased root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Invalidation {
    /// The client must scan every path selected by its sparse matcher.
    Full,
    /// Exact repository-relative raw-byte paths which may have changed.
    ExactPaths(Vec<Vec<u8>>),
    /// Repository-relative raw-byte prefixes which may have changed.
    Prefixes(Vec<Vec<u8>>),
}

/// Identity metadata for the immutable directory handle returned by AWACS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotIdentity {
    /// Btrfs filesystem UUID containing the snapshot.
    pub filesystem_uuid: [u8; 16],
    /// Btrfs subvolume UUID of the exact leased snapshot.
    pub subvolume_uuid: [u8; 16],
    /// Whether AWACS verified the snapshot as read-only.
    pub read_only: bool,
}

/// How a client completed work derived from a scan lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanOutcome {
    /// The client durably persisted state paired with this lease's cursor.
    Committed,
    /// The client did not persist this lease's cursor.
    Aborted,
}

/// A class of scan failure that callers can handle conservatively.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanErrorKind {
    /// The live root is not a supported Btrfs layout.
    UnsupportedFilesystem,
    /// No AWACS service could be reached or activated.
    Unavailable,
    /// The previous cursor cannot be reused; callers may retry from `Full`.
    InvalidPreviousCursor,
    /// Continuity was lost and the next valid lease must be a full scan.
    FullScanRequired,
    /// The active lease expired or could not be renewed.
    LeaseExpired,
    /// The caller is not authorized to cut or read the requested root.
    Unauthorized,
    /// The client and service do not share a supported API version.
    VersionMismatch,
    /// AWACS returned malformed or internally inconsistent data.
    MalformedResponse,
    /// A service-specific failure not covered by a more precise kind.
    Other,
}

/// An error returned by a scan client or lease.
#[derive(Debug)]
pub struct ScanError {
    kind: ScanErrorKind,
    message: String,
}

impl ScanError {
    /// Creates an error with a stable classification and actionable detail.
    pub fn new(kind: ScanErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Returns the stable error classification.
    pub fn kind(&self) -> ScanErrorKind {
        self.kind
    }

    /// Returns the user-facing error detail.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ScanError {}

/// Private lifecycle operations retained by a [`ScanLease`].
///
/// Implementations hide transport identifiers and fencing tokens behind this
/// trait. `finish()` must be idempotent for a retried completion after a lost
/// response.
pub trait ScanSession: Send {
    /// Extends the active lease while the caller still owns the scan root.
    fn renew(&mut self) -> Result<(), ScanError>;

    /// Reports whether state paired with this cursor was durably committed.
    fn finish(&mut self, outcome: ScanOutcome) -> Result<(), ScanError>;
}

/// One immutable scan root and the lease which keeps it valid.
pub struct ScanLease {
    /// Opaque cursor to persist only after a committed scan.
    pub cursor: Vec<u8>,
    /// Safe incremental narrowing hints for reads from `scan_root`.
    pub invalidation: Invalidation,
    /// Identity metadata which callers validate before scanning.
    pub identity: SnapshotIdentity,
    /// Lease deadline on the boot-time monotonic clock.
    pub expires_boottime_ns: u64,
    scan_root: File,
    session: Box<dyn ScanSession>,
    finished_outcome: Option<ScanOutcome>,
}

impl ScanLease {
    /// Creates a lease from a validated read-only directory handle and its
    /// private completion session.
    pub fn new(
        cursor: Vec<u8>,
        invalidation: Invalidation,
        identity: SnapshotIdentity,
        expires_boottime_ns: u64,
        scan_root: File,
        session: Box<dyn ScanSession>,
    ) -> Self {
        Self {
            cursor,
            invalidation,
            identity,
            expires_boottime_ns,
            scan_root,
            session,
            finished_outcome: None,
        }
    }

    /// Returns the directory handle which must remain open for the lease.
    pub fn scan_root(&self) -> &File {
        &self.scan_root
    }

    /// Renews the active lease.
    pub fn renew(&mut self) -> Result<(), ScanError> {
        self.session.renew()
    }

    /// Returns the conservative client renewal cadence for this lease:
    /// min(60 seconds, one third of the remaining boot-time TTL).
    pub fn renewal_interval(&self) -> Duration {
        const MAX_RENEW_INTERVAL_NS: u64 = 60_000_000_000;
        let ttl_ns = self.expires_boottime_ns.saturating_sub(boottime_now_ns());
        Duration::from_nanos((ttl_ns / 3).min(MAX_RENEW_INTERVAL_NS).max(1))
    }

    /// Finishes the active lease with the caller's durable outcome.
    pub fn finish(&mut self, outcome: ScanOutcome) -> Result<(), ScanError> {
        if let Some(previous) = self.finished_outcome {
            if previous == outcome {
                return Ok(());
            }
            return Err(ScanError::new(
                ScanErrorKind::Other,
                "AWACS scan lease was already finished with a different outcome",
            ));
        }
        self.session.finish(outcome)?;
        self.finished_outcome = Some(outcome);
        Ok(())
    }
}

pub(crate) fn boottime_now_ns() -> u64 {
    // SAFETY: clock_gettime writes one initialized timespec on success.
    unsafe {
        let mut time: libc::timespec = zeroed();
        if libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) != 0 {
            return 0;
        }
        (time.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(time.tv_nsec as u64)
    }
}

/// Verifies that a returned scan-root fd is the exact read-only Btrfs
/// subvolume described by the service response.
///
/// Consumers should call this before traversing the fd. The service metadata
/// is not trusted merely because it arrived over an authenticated socket.
pub fn validate_scan_root(scan_root: &File, expected: SnapshotIdentity) -> Result<(), ScanError> {
    if !scan_root
        .metadata()
        .map_err(|err| {
            ScanError::new(
                ScanErrorKind::MalformedResponse,
                format!("inspect AWACS scan root metadata: {err}"),
            )
        })?
        .is_dir()
    {
        return Err(ScanError::new(
            ScanErrorKind::MalformedResponse,
            "AWACS scan root fd is not a directory",
        ));
    }
    let filesystem = crate::btrfs::filesystem_info(scan_root.as_fd()).map_err(|err| {
        ScanError::new(
            ScanErrorKind::MalformedResponse,
            format!("inspect AWACS scan root filesystem: {err}"),
        )
    })?;
    let subvolume = crate::btrfs::subvolume_info(scan_root.as_fd()).map_err(|err| {
        ScanError::new(
            ScanErrorKind::MalformedResponse,
            format!("inspect AWACS scan root subvolume: {err}"),
        )
    })?;
    if filesystem.fs_uuid != expected.filesystem_uuid
        || subvolume.uuid != expected.subvolume_uuid
        || !expected.read_only
        || !subvolume.readonly()
    {
        return Err(ScanError::new(
            ScanErrorKind::MalformedResponse,
            "AWACS scan root identity or read-only flag does not match its response",
        ));
    }
    Ok(())
}

/// A transport-independent AWACS scan client.
pub trait ScanClient: Send {
    /// Starts one immutable scan lease for the requested live root.
    fn begin_scan(&mut self, request: &BeginScanRequest) -> Result<ScanLease, ScanError>;

    /// Validates the scan-root fd before a consumer traverses it.
    ///
    /// Real clients use the Btrfs identity check. In-process test doubles may
    /// override this when their synthetic root is an ordinary directory.
    fn validate_scan_root(&self, lease: &ScanLease) -> Result<(), ScanError> {
        validate_scan_root(lease.scan_root(), lease.identity)
    }
}

/// Result returned by a daemon-side scan request handler.
pub struct ServerScanLease {
    /// Private session identifier used only for Renew and Finish.
    pub session_id: Vec<u8>,
    /// Opaque cursor returned to the client for later persistence.
    pub cursor: Vec<u8>,
    /// Safe scan narrowing hints for the immutable root.
    pub invalidation: Invalidation,
    /// Lease deadline on the boot-time monotonic clock.
    pub expires_boottime_ns: u64,
    /// Identity metadata paired with scan_root.
    pub identity: SnapshotIdentity,
    /// Read-only directory fd transferred on successful Begin.
    pub scan_root: File,
}

/// Daemon-side implementation behind the private scan socket protocol.
pub trait ScanRequestHandler: Send {
    /// Cuts or reuses an immutable snapshot and returns a pinned session.
    fn begin_scan(&mut self, request: BeginScanRequest) -> Result<ServerScanLease, ScanError>;
    /// Renews one still-active private session.
    fn renew_scan(&mut self, session_id: &[u8]) -> Result<(), ScanError>;
    /// Finishes one private session with the client's durable outcome.
    fn finish_scan(&mut self, session_id: &[u8], outcome: ScanOutcome) -> Result<(), ScanError>;
}

const SCAN_MAGIC: &[u8; 4] = b"BAWS";
const SCAN_VERSION: u16 = 1;
const SCAN_HEADER_SIZE: usize = 16;
const SCAN_MAX_PAYLOAD: usize = 1024 * 1024;
const SCAN_MAX_FDS: usize = 1;
const FLAG_RESPONSE: u16 = 1;
const OP_BEGIN: u16 = 1;
const OP_RENEW: u16 = 2;
const OP_FINISH: u16 = 3;

/// Production scan client for an explicit AWACS scan socket.
///
/// Discovery and activation remain library-owned and can be layered on this
/// client. Consumers do not need to know the private packet framing.
pub struct SocketScanClient {
    socket: Arc<Mutex<ScanSocket>>,
}

impl SocketScanClient {
    /// Connects to one absolute namespace-specific scan socket.
    pub fn connect(path: &Path) -> Result<Self, ScanError> {
        if !path.is_absolute() {
            return Err(ScanError::new(
                ScanErrorKind::Unavailable,
                "AWACS scan socket path must be absolute",
            ));
        }
        Ok(Self {
            socket: Arc::new(Mutex::new(ScanSocket::connect(path)?)),
        })
    }

    /// Connects to an explicit override or discovers this mount namespace's
    /// standard AWACS scan socket.
    pub fn connect_for_root(
        live_root: &Path,
        socket_override: Option<&Path>,
    ) -> Result<Self, ScanError> {
        match socket_override {
            Some(path) => Self::connect(path),
            None => Self::connect(&discover_scan_socket(live_root)?),
        }
    }
}

fn discover_scan_socket(live_root: &Path) -> Result<PathBuf, ScanError> {
    let command = std::env::var_os("BTRFS_AWACS_COMMAND")
        .unwrap_or_else(|| std::ffi::OsString::from("btrfs-awacs"));
    let output = Command::new(command)
        .arg("scan-sockname")
        .arg(live_root)
        .output()
        .map_err(|err| {
            ScanError::new(
                ScanErrorKind::Unavailable,
                format!("run AWACS scan discovery command: {err}"),
            )
        })?;
    if !output.status.success() {
        return Err(ScanError::new(
            ScanErrorKind::Unavailable,
            format!(
                "AWACS scan discovery command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let Some(path) = output.stdout.strip_suffix(&[0]) else {
        return Err(ScanError::new(
            ScanErrorKind::MalformedResponse,
            "AWACS scan discovery response is not NUL-terminated",
        ));
    };
    if path.is_empty() || path.contains(&0) {
        return Err(ScanError::new(
            ScanErrorKind::MalformedResponse,
            "AWACS scan discovery response has invalid path bytes",
        ));
    }
    let path = PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()));
    if !path.is_absolute() {
        return Err(ScanError::new(
            ScanErrorKind::MalformedResponse,
            "AWACS scan discovery response is not absolute",
        ));
    }
    Ok(path)
}

impl ScanClient for SocketScanClient {
    fn begin_scan(&mut self, request: &BeginScanRequest) -> Result<ScanLease, ScanError> {
        let mut payload = Encoder::default();
        payload.bytes(path_bytes(&request.live_root))?;
        payload.optional_bytes(request.previous_cursor.as_deref())?;
        let response = request_response(&self.socket, OP_BEGIN, payload.finish())?;
        let mut decoder = Decoder::new(&response.payload);
        decode_status(&mut decoder)?;
        if response.fds.len() != 1 {
            return Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                format!(
                    "successful AWACS BeginScan carried {} descriptors, expected 1",
                    response.fds.len()
                ),
            ));
        }
        let session_id = decoder.bytes()?;
        let cursor = decoder.bytes()?;
        let invalidation = decoder.invalidation()?;
        let expires_boottime_ns = decoder.u64()?;
        let identity = SnapshotIdentity {
            filesystem_uuid: decoder.array()?,
            subvolume_uuid: decoder.array()?,
            read_only: decoder.bool()?,
        };
        decoder.finish()?;
        let mut fds = response.fds;
        let scan_root = File::from(fds.pop().expect("fd count validated"));
        Ok(ScanLease::new(
            cursor,
            invalidation,
            identity,
            expires_boottime_ns,
            scan_root,
            Box::new(SocketScanSession {
                socket: self.socket.clone(),
                session_id,
            }),
        ))
    }
}

struct SocketScanSession {
    socket: Arc<Mutex<ScanSocket>>,
    session_id: Vec<u8>,
}

impl ScanSession for SocketScanSession {
    fn renew(&mut self) -> Result<(), ScanError> {
        let mut payload = Encoder::default();
        payload.bytes(&self.session_id)?;
        let response = request_response(&self.socket, OP_RENEW, payload.finish())?;
        let mut decoder = Decoder::new(&response.payload);
        decode_status(&mut decoder)?;
        if !response.fds.is_empty() {
            return Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS RenewScan response carried descriptors",
            ));
        }
        decoder.finish()
    }

    fn finish(&mut self, outcome: ScanOutcome) -> Result<(), ScanError> {
        let mut payload = Encoder::default();
        payload.bytes(&self.session_id)?;
        payload.u8(match outcome {
            ScanOutcome::Committed => 1,
            ScanOutcome::Aborted => 2,
        });
        let response = request_response(&self.socket, OP_FINISH, payload.finish())?;
        let mut decoder = Decoder::new(&response.payload);
        decode_status(&mut decoder)?;
        if !response.fds.is_empty() {
            return Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS FinishScan response carried descriptors",
            ));
        }
        decoder.finish()
    }
}

fn request_response(
    socket: &Arc<Mutex<ScanSocket>>,
    opcode: u16,
    payload: Vec<u8>,
) -> Result<ReceivedPacket, ScanError> {
    let socket = socket
        .lock()
        .map_err(|_| ScanError::new(ScanErrorKind::Other, "AWACS scan socket lock poisoned"))?;
    socket.send(opcode, 0, &payload, &[])?;
    let response = socket.receive()?;
    if response.opcode != opcode || response.flags != FLAG_RESPONSE {
        return Err(ScanError::new(
            ScanErrorKind::MalformedResponse,
            "AWACS scan response opcode or flags mismatch",
        ));
    }
    if response.payload.first() == Some(&1) && !response.fds.is_empty() {
        return Err(ScanError::new(
            ScanErrorKind::MalformedResponse,
            "AWACS error response carried descriptors",
        ));
    }
    Ok(response)
}

/// Serves one authenticated connection packet for a daemon-owned handler.
///
/// The caller owns peer-credential and mount-namespace authentication before
/// handing a connected socket to this dispatcher.
pub struct SocketScanDispatcher<H> {
    handler: Mutex<H>,
}

impl<H: ScanRequestHandler> SocketScanDispatcher<H> {
    /// Creates a dispatcher around one durable scan-session handler.
    pub fn new(handler: H) -> Self {
        Self {
            handler: Mutex::new(handler),
        }
    }

    /// Receives, validates, dispatches, and replies to exactly one packet.
    pub fn serve_one(&self, socket: &ScanSocket) -> Result<(), ScanError> {
        let request = socket.receive()?;
        if request.flags != 0 || !request.fds.is_empty() {
            return Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS scan request has invalid flags or descriptors",
            ));
        }
        let mut handler = self
            .handler
            .lock()
            .map_err(|_| ScanError::new(ScanErrorKind::Other, "AWACS handler lock poisoned"))?;
        match request.opcode {
            OP_BEGIN => {
                let mut decoder = Decoder::new(&request.payload);
                let live_root = decoder.bytes()?;
                let previous_cursor = decoder.optional_bytes()?;
                decoder.finish()?;
                let live_root = PathBuf::from(std::ffi::OsString::from_vec(live_root));
                match handler.begin_scan(BeginScanRequest {
                    live_root,
                    previous_cursor,
                }) {
                    Ok(lease) => {
                        let mut payload = Encoder::default();
                        payload.u8(0);
                        payload.bytes(&lease.session_id)?;
                        payload.bytes(&lease.cursor)?;
                        payload.invalidation(&lease.invalidation)?;
                        payload.u64(lease.expires_boottime_ns);
                        payload
                            .bytes
                            .extend_from_slice(&lease.identity.filesystem_uuid);
                        payload
                            .bytes
                            .extend_from_slice(&lease.identity.subvolume_uuid);
                        payload.u8(u8::from(lease.identity.read_only));
                        let result = socket.send(
                            OP_BEGIN,
                            FLAG_RESPONSE,
                            &payload.finish(),
                            &[lease.scan_root.as_raw_fd()],
                        );
                        #[cfg(debug_assertions)]
                        if result.is_ok() {
                            maybe_exit_after_begin_response();
                        }
                        result
                    }
                    Err(err) => send_error(socket, OP_BEGIN, &err),
                }
            }
            OP_RENEW => {
                let mut decoder = Decoder::new(&request.payload);
                let session_id = decoder.bytes()?;
                decoder.finish()?;
                match handler.renew_scan(&session_id) {
                    Ok(()) => socket.send(OP_RENEW, FLAG_RESPONSE, &[0], &[]),
                    Err(err) => send_error(socket, OP_RENEW, &err),
                }
            }
            OP_FINISH => {
                let mut decoder = Decoder::new(&request.payload);
                let session_id = decoder.bytes()?;
                let outcome = match decoder.u8()? {
                    1 => ScanOutcome::Committed,
                    2 => ScanOutcome::Aborted,
                    _ => {
                        return Err(ScanError::new(
                            ScanErrorKind::MalformedResponse,
                            "AWACS FinishScan request has invalid outcome",
                        ));
                    }
                };
                decoder.finish()?;
                match handler.finish_scan(&session_id, outcome) {
                    Ok(()) => socket.send(OP_FINISH, FLAG_RESPONSE, &[0], &[]),
                    Err(err) => send_error(socket, OP_FINISH, &err),
                }
            }
            _ => Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS scan request has unknown opcode",
            )),
        }
    }
}

/// Debug-build integration hook which simulates a daemon restart after the
/// client has accepted a Begin response but before it can finish the lease.
#[cfg(debug_assertions)]
fn maybe_exit_after_begin_response() {
    let Some(control_dir) = std::env::var_os("BTRFS_AWACS_SCAN_TEST_CONTROL_DIR") else {
        return;
    };
    let marker = PathBuf::from(control_dir).join("exit-after-begin-response");
    if std::fs::remove_file(marker).is_ok() {
        std::process::exit(0);
    }
}

fn send_error(socket: &ScanSocket, opcode: u16, error: &ScanError) -> Result<(), ScanError> {
    let mut payload = Encoder::default();
    payload.u8(1);
    payload.u8(encode_error_kind(error.kind()));
    payload.bytes(error.message().as_bytes())?;
    socket.send(opcode, FLAG_RESPONSE, &payload.finish(), &[])
}

fn encode_error_kind(kind: ScanErrorKind) -> u8 {
    match kind {
        ScanErrorKind::UnsupportedFilesystem => 1,
        ScanErrorKind::Unavailable => 2,
        ScanErrorKind::InvalidPreviousCursor => 3,
        ScanErrorKind::FullScanRequired => 4,
        ScanErrorKind::LeaseExpired => 5,
        ScanErrorKind::Unauthorized => 6,
        ScanErrorKind::VersionMismatch => 7,
        ScanErrorKind::MalformedResponse => 8,
        ScanErrorKind::Other => 9,
    }
}

fn decode_status(decoder: &mut Decoder<'_>) -> Result<(), ScanError> {
    match decoder.u8()? {
        0 => Ok(()),
        1 => {
            let kind = decode_error_kind(decoder.u8()?);
            let message = decoder.bytes()?;
            let message = String::from_utf8(message).map_err(|_| {
                ScanError::new(
                    ScanErrorKind::MalformedResponse,
                    "AWACS error message is not UTF-8",
                )
            })?;
            Err(ScanError::new(kind, message))
        }
        _ => Err(ScanError::new(
            ScanErrorKind::MalformedResponse,
            "AWACS scan response has unknown status",
        )),
    }
}

fn decode_error_kind(value: u8) -> ScanErrorKind {
    match value {
        1 => ScanErrorKind::UnsupportedFilesystem,
        2 => ScanErrorKind::Unavailable,
        3 => ScanErrorKind::InvalidPreviousCursor,
        4 => ScanErrorKind::FullScanRequired,
        5 => ScanErrorKind::LeaseExpired,
        6 => ScanErrorKind::Unauthorized,
        7 => ScanErrorKind::VersionMismatch,
        8 => ScanErrorKind::MalformedResponse,
        _ => ScanErrorKind::Other,
    }
}

#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), ScanError> {
        let length = u32::try_from(value.len()).map_err(|_| {
            ScanError::new(
                ScanErrorKind::Other,
                "AWACS scan payload field is too large",
            )
        })?;
        self.u32(length);
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn optional_bytes(&mut self, value: Option<&[u8]>) -> Result<(), ScanError> {
        match value {
            Some(value) => {
                self.u8(1);
                self.bytes(value)
            }
            None => {
                self.u8(0);
                Ok(())
            }
        }
    }

    fn invalidation(&mut self, invalidation: &Invalidation) -> Result<(), ScanError> {
        let (kind, paths) = match invalidation {
            Invalidation::Full => {
                self.u8(0);
                return Ok(());
            }
            Invalidation::ExactPaths(paths) => (1, paths),
            Invalidation::Prefixes(paths) => (2, paths),
        };
        self.u8(kind);
        self.u32(u32::try_from(paths.len()).map_err(|_| {
            ScanError::new(
                ScanErrorKind::Other,
                "AWACS invalidation path count is too large",
            )
        })?);
        for path in paths {
            self.bytes(path)?;
        }
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ScanError> {
        let end = self.offset.checked_add(length).ok_or_else(|| {
            ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS payload length overflow",
            )
        })?;
        let value = self.bytes.get(self.offset..end).ok_or_else(|| {
            ScanError::new(
                ScanErrorKind::MalformedResponse,
                "truncated AWACS scan payload",
            )
        })?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, ScanError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, ScanError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, ScanError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn bool(&mut self) -> Result<bool, ScanError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS scan payload has invalid boolean",
            )),
        }
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ScanError> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    fn bytes(&mut self) -> Result<Vec<u8>, ScanError> {
        let length = usize::try_from(self.u32()?).unwrap();
        Ok(self.take(length)?.to_vec())
    }

    fn optional_bytes(&mut self) -> Result<Option<Vec<u8>>, ScanError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.bytes()?)),
            _ => Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS payload has invalid optional-bytes tag",
            )),
        }
    }

    fn invalidation(&mut self) -> Result<Invalidation, ScanError> {
        let kind = self.u8()?;
        if kind == 0 {
            return Ok(Invalidation::Full);
        }
        let count = usize::try_from(self.u32()?).unwrap();
        if count > 100_000 {
            return Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS invalidation has too many paths",
            ));
        }
        let paths = (0..count)
            .map(|_| self.bytes())
            .collect::<Result<Vec<_>, _>>()?;
        match kind {
            1 => Ok(Invalidation::ExactPaths(paths)),
            2 => Ok(Invalidation::Prefixes(paths)),
            _ => Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS scan payload has unknown invalidation kind",
            )),
        }
    }

    fn finish(&self) -> Result<(), ScanError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS scan payload has trailing bytes",
            ))
        }
    }
}

struct ReceivedPacket {
    opcode: u16,
    flags: u16,
    payload: Vec<u8>,
    fds: Vec<OwnedFd>,
}

/// One connected private scan-protocol seqpacket socket.
pub struct ScanSocket {
    fd: OwnedFd,
}

/// Kernel-supplied credentials for one connected scan client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanPeerCredentials {
    /// Peer process id from SO_PEERCRED.
    pub pid: u32,
    /// Peer effective user id from SO_PEERCRED.
    pub uid: u32,
    /// Peer effective group id from SO_PEERCRED.
    pub gid: u32,
}

/// Listener for the daemon-owned private scan socket.
pub struct ScanSocketListener {
    fd: OwnedFd,
}

impl ScanSocketListener {
    /// Binds a new seqpacket socket without replacing an existing path.
    pub fn bind(path: &Path, mode: u32) -> Result<Self, ScanError> {
        if mode & !0o777 != 0 {
            return Err(ScanError::new(
                ScanErrorKind::Other,
                "AWACS scan socket mode contains unsupported bits",
            ));
        }
        let (address, length) = unix_socket_address(path)?;
        // SAFETY: socket returns one newly owned descriptor on success.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(io_error("create AWACS scan listener socket"));
        }
        // SAFETY: raw is uniquely owned after successful socket().
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: address and length describe an initialized sockaddr_un.
        if unsafe { libc::bind(fd.as_raw_fd(), (&raw const address).cast(), length) } != 0 {
            return Err(io_error("bind AWACS scan socket"));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|err| {
            ScanError::new(
                ScanErrorKind::Other,
                format!("set AWACS scan socket mode: {err}"),
            )
        })?;
        // SAFETY: fd is a bound SOCK_SEQPACKET listener.
        if unsafe { libc::listen(fd.as_raw_fd(), 128) } != 0 {
            return Err(io_error("listen on AWACS scan socket"));
        }
        Ok(Self { fd })
    }

    /// Accepts one connected scan-protocol socket.
    pub fn accept(&self) -> Result<ScanSocket, ScanError> {
        // SAFETY: accept4 returns one fresh descriptor without peer address.
        let raw = unsafe {
            libc::accept4(
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io_error("accept AWACS scan connection"));
        }
        // SAFETY: accept4 returned one fresh descriptor.
        Ok(ScanSocket {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        })
    }
}

impl ScanSocket {
    fn connect(path: &Path) -> Result<Self, ScanError> {
        let (address, length) = unix_socket_address(path)?;
        // SAFETY: socket returns one newly owned descriptor on success.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(io_error("create AWACS scan socket"));
        }
        // SAFETY: raw is uniquely owned after successful socket().
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: address and length describe an initialized sockaddr_un.
        if unsafe { libc::connect(fd.as_raw_fd(), (&raw const address).cast(), length) } != 0 {
            return Err(io_error("connect AWACS scan socket"));
        }
        Ok(Self { fd })
    }

    #[cfg(test)]
    fn pair() -> Result<(Self, Self), ScanError> {
        let mut fds = [-1; 2];
        // SAFETY: fds points at space for the two returned descriptors.
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        } != 0
        {
            return Err(io_error("create AWACS scan socketpair"));
        }
        // SAFETY: socketpair transferred one unique descriptor into each slot.
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

    fn send(
        &self,
        opcode: u16,
        flags: u16,
        payload: &[u8],
        fds: &[RawFd],
    ) -> Result<(), ScanError> {
        if payload.len() > SCAN_MAX_PAYLOAD || fds.len() > SCAN_MAX_FDS {
            return Err(ScanError::new(
                ScanErrorKind::Other,
                "AWACS scan packet exceeds bounded limits",
            ));
        }
        let mut header = [0_u8; SCAN_HEADER_SIZE];
        header[..4].copy_from_slice(SCAN_MAGIC);
        header[4..6].copy_from_slice(&SCAN_VERSION.to_be_bytes());
        header[6..8].copy_from_slice(&opcode.to_be_bytes());
        header[8..10].copy_from_slice(&flags.to_be_bytes());
        header[10..12].copy_from_slice(&(fds.len() as u16).to_be_bytes());
        header[12..16].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        let iovecs = [
            libc::iovec {
                iov_base: header.as_ptr().cast_mut().cast(),
                iov_len: header.len(),
            },
            libc::iovec {
                iov_base: payload.as_ptr().cast_mut().cast(),
                iov_len: payload.len(),
            },
        ];
        let mut control = [0_usize; 8];
        // SAFETY: all iovec/control pointers remain live for sendmsg.
        let sent = unsafe {
            let mut message: libc::msghdr = zeroed();
            message.msg_iov = iovecs.as_ptr().cast_mut();
            message.msg_iovlen = iovecs.len();
            if !fds.is_empty() {
                let control_len = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as usize;
                message.msg_control = control.as_mut_ptr().cast();
                message.msg_controllen = control_len;
                let cmsg = libc::CMSG_FIRSTHDR(&message);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as usize;
                libc::CMSG_DATA(cmsg).cast::<RawFd>().write(fds[0]);
            }
            libc::sendmsg(self.fd.as_raw_fd(), &message, libc::MSG_NOSIGNAL)
        };
        if sent < 0 {
            return Err(io_error("send AWACS scan packet"));
        }
        if sent as usize != SCAN_HEADER_SIZE + payload.len() {
            return Err(ScanError::new(
                ScanErrorKind::Other,
                "short AWACS scan packet write",
            ));
        }
        Ok(())
    }

    fn receive(&self) -> Result<ReceivedPacket, ScanError> {
        let mut bytes = vec![0_u8; SCAN_HEADER_SIZE + SCAN_MAX_PAYLOAD];
        let mut control = [0_usize; 8];
        // SAFETY: message points to initialized writable buffers for recvmsg.
        let (received, flags, raw_fds) = unsafe {
            let mut iovec = libc::iovec {
                iov_base: bytes.as_mut_ptr().cast(),
                iov_len: bytes.len(),
            };
            let mut message: libc::msghdr = zeroed();
            message.msg_iov = &mut iovec;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = size_of::<[usize; 8]>();
            let received = libc::recvmsg(self.fd.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC);
            if received < 0 {
                return Err(io_error("receive AWACS scan packet"));
            }
            let mut raw_fds = Vec::new();
            let mut cmsg = libc::CMSG_FIRSTHDR(&message);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
                    for fd in raw_fds {
                        libc::close(fd);
                    }
                    return Err(ScanError::new(
                        ScanErrorKind::MalformedResponse,
                        "AWACS packet has unsupported ancillary data",
                    ));
                }
                let header_len = libc::CMSG_LEN(0) as usize;
                let data_len = (*cmsg).cmsg_len.checked_sub(header_len).ok_or_else(|| {
                    ScanError::new(
                        ScanErrorKind::MalformedResponse,
                        "AWACS packet has malformed ancillary length",
                    )
                })?;
                if data_len % size_of::<RawFd>() != 0 {
                    for fd in raw_fds {
                        libc::close(fd);
                    }
                    return Err(ScanError::new(
                        ScanErrorKind::MalformedResponse,
                        "AWACS packet has partial descriptor",
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
        let fds = raw_fds
            .into_iter()
            .map(|fd| {
                // SAFETY: each received SCM_RIGHTS descriptor is uniquely owned.
                unsafe { OwnedFd::from_raw_fd(fd) }
            })
            .collect::<Vec<_>>();
        if flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || received < SCAN_HEADER_SIZE {
            return Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "truncated AWACS scan packet",
            ));
        }
        bytes.truncate(received);
        if &bytes[..4] != SCAN_MAGIC {
            return Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS scan packet magic mismatch",
            ));
        }
        let version = u16::from_be_bytes(bytes[4..6].try_into().unwrap());
        if version != SCAN_VERSION {
            return Err(ScanError::new(
                ScanErrorKind::VersionMismatch,
                "unsupported AWACS scan protocol version",
            ));
        }
        let opcode = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
        let flags = u16::from_be_bytes(bytes[8..10].try_into().unwrap());
        let fd_count = u16::from_be_bytes(bytes[10..12].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(bytes[12..16].try_into().unwrap()) as usize;
        if fd_count > SCAN_MAX_FDS
            || fds.len() != fd_count
            || received != SCAN_HEADER_SIZE + payload_len
        {
            return Err(ScanError::new(
                ScanErrorKind::MalformedResponse,
                "AWACS scan packet length or descriptor mismatch",
            ));
        }
        Ok(ReceivedPacket {
            opcode,
            flags,
            payload: bytes[SCAN_HEADER_SIZE..].to_vec(),
            fds,
        })
    }

    /// Reads kernel-authenticated Unix peer credentials.
    pub fn peer_credentials(&self) -> Result<ScanPeerCredentials, ScanError> {
        // SAFETY: getsockopt writes one ucred into the initialized buffer.
        unsafe {
            let mut credentials: libc::ucred = zeroed();
            let mut length = size_of::<libc::ucred>() as libc::socklen_t;
            if libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            ) != 0
            {
                return Err(io_error("read AWACS scan peer credentials"));
            }
            if length as usize != size_of::<libc::ucred>() {
                return Err(ScanError::new(
                    ScanErrorKind::MalformedResponse,
                    "AWACS scan peer credentials have unexpected length",
                ));
            }
            Ok(ScanPeerCredentials {
                pid: credentials.pid.try_into().map_err(|_| {
                    ScanError::new(
                        ScanErrorKind::Unauthorized,
                        "AWACS scan peer has negative process id",
                    )
                })?,
                uid: credentials.uid,
                gid: credentials.gid,
            })
        }
    }
}

fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

fn io_error(context: &str) -> ScanError {
    ScanError::new(
        ScanErrorKind::Unavailable,
        format!("{context}: {}", io::Error::last_os_error()),
    )
}

fn unix_socket_address(path: &Path) -> Result<(libc::sockaddr_un, libc::socklen_t), ScanError> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        ScanError::new(
            ScanErrorKind::Unavailable,
            "AWACS scan socket path contains NUL",
        )
    })?;
    // SAFETY: zero is a valid initialization for sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { zeroed() };
    if path.as_bytes_with_nul().len() > address.sun_path.len() {
        return Err(ScanError::new(
            ScanErrorKind::Unavailable,
            "AWACS scan socket path exceeds sockaddr_un",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(path.as_bytes_with_nul()) {
        *target = *source as libc::c_char;
    }
    let length =
        (size_of::<libc::sa_family_t>() + path.as_bytes_with_nul().len()) as libc::socklen_t;
    Ok((address, length))
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd as _;
    use std::sync::{Arc, Mutex};

    use super::*;

    struct RecordingSession {
        renewals: Arc<Mutex<usize>>,
        outcomes: Arc<Mutex<Vec<ScanOutcome>>>,
    }

    impl ScanSession for RecordingSession {
        fn renew(&mut self) -> Result<(), ScanError> {
            *self.renewals.lock().unwrap() += 1;
            Ok(())
        }

        fn finish(&mut self, outcome: ScanOutcome) -> Result<(), ScanError> {
            self.outcomes.lock().unwrap().push(outcome);
            Ok(())
        }
    }

    #[test]
    fn lease_keeps_root_and_forwards_lifecycle() {
        let temp_dir = tempfile::tempdir().unwrap();
        let scan_root = File::open(temp_dir.path()).unwrap();
        let renewals = Arc::new(Mutex::new(0));
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let session = RecordingSession {
            renewals: renewals.clone(),
            outcomes: outcomes.clone(),
        };
        let mut lease = ScanLease::new(
            b"cursor".to_vec(),
            Invalidation::Full,
            SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
            300,
            scan_root,
            Box::new(session),
        );

        assert!(lease.scan_root().metadata().unwrap().is_dir());
        lease.renew().unwrap();
        lease.finish(ScanOutcome::Committed).unwrap();
        // A caller can safely retry after a response was lost without
        // re-running completion side effects.
        lease.finish(ScanOutcome::Committed).unwrap();
        assert!(lease.finish(ScanOutcome::Aborted).is_err());

        assert_eq!(*renewals.lock().unwrap(), 1);
        assert_eq!(
            outcomes.lock().unwrap().as_slice(),
            &[ScanOutcome::Committed]
        );
    }

    #[test]
    fn error_retains_stable_classification() {
        let error = ScanError::new(ScanErrorKind::LeaseExpired, "lease expired");
        assert_eq!(error.kind(), ScanErrorKind::LeaseExpired);
        assert_eq!(error.message(), "lease expired");
        assert_eq!(error.to_string(), "lease expired");
    }

    #[test]
    fn scan_root_validation_rejects_unmatched_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let scan_root = File::open(temp_dir.path()).unwrap();
        let error = validate_scan_root(
            &scan_root,
            SnapshotIdentity {
                filesystem_uuid: [1; 16],
                subvolume_uuid: [2; 16],
                read_only: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), ScanErrorKind::MalformedResponse);
    }

    #[test]
    fn socket_client_owns_private_framing_and_fd_lifecycle() {
        let (client_socket, server_socket) = ScanSocket::pair().unwrap();
        let scan_root = tempfile::tempdir().unwrap();
        let scan_root_fd = File::open(scan_root.path()).unwrap();
        let server = std::thread::spawn(move || {
            let begin = server_socket.receive().unwrap();
            assert_eq!(begin.opcode, OP_BEGIN);
            assert_eq!(begin.flags, 0);
            assert!(begin.fds.is_empty());
            let mut request = Decoder::new(&begin.payload);
            assert_eq!(request.bytes().unwrap(), b"/tmp/live");
            assert_eq!(request.u8().unwrap(), 1);
            assert_eq!(request.bytes().unwrap(), b"previous");
            request.finish().unwrap();

            let mut response = Encoder::default();
            response.u8(0);
            response.bytes(b"session").unwrap();
            response.bytes(b"cursor").unwrap();
            response.u8(2);
            response.u32(1);
            response.bytes(b"dir").unwrap();
            response.u64(300);
            response.bytes.extend_from_slice(&[1; 16]);
            response.bytes.extend_from_slice(&[2; 16]);
            response.u8(1);
            server_socket
                .send(
                    OP_BEGIN,
                    FLAG_RESPONSE,
                    &response.finish(),
                    &[scan_root_fd.as_raw_fd()],
                )
                .unwrap();

            let renew = server_socket.receive().unwrap();
            assert_eq!(renew.opcode, OP_RENEW);
            let mut request = Decoder::new(&renew.payload);
            assert_eq!(request.bytes().unwrap(), b"session");
            request.finish().unwrap();
            server_socket
                .send(OP_RENEW, FLAG_RESPONSE, &[0], &[])
                .unwrap();

            let finish = server_socket.receive().unwrap();
            assert_eq!(finish.opcode, OP_FINISH);
            let mut request = Decoder::new(&finish.payload);
            assert_eq!(request.bytes().unwrap(), b"session");
            assert_eq!(request.u8().unwrap(), 1);
            request.finish().unwrap();
            server_socket
                .send(OP_FINISH, FLAG_RESPONSE, &[0], &[])
                .unwrap();
        });

        let mut client = SocketScanClient {
            socket: Arc::new(Mutex::new(client_socket)),
        };
        let mut lease = client
            .begin_scan(&BeginScanRequest {
                live_root: PathBuf::from("/tmp/live"),
                previous_cursor: Some(b"previous".to_vec()),
            })
            .unwrap();
        assert_eq!(lease.cursor, b"cursor");
        assert_eq!(
            lease.invalidation,
            Invalidation::Prefixes(vec![b"dir".to_vec()])
        );
        assert!(lease.scan_root().metadata().unwrap().is_dir());
        lease.renew().unwrap();
        lease.finish(ScanOutcome::Committed).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn begin_error_uses_zero_descriptors_and_keeps_error_kind() {
        let (client_socket, server_socket) = ScanSocket::pair().unwrap();
        let server = std::thread::spawn(move || {
            let begin = server_socket.receive().unwrap();
            assert_eq!(begin.opcode, OP_BEGIN);
            let mut response = Encoder::default();
            response.u8(1);
            response.u8(5);
            response.bytes(b"expired").unwrap();
            server_socket
                .send(OP_BEGIN, FLAG_RESPONSE, &response.finish(), &[])
                .unwrap();
        });
        let mut client = SocketScanClient {
            socket: Arc::new(Mutex::new(client_socket)),
        };
        let error = match client.begin_scan(&BeginScanRequest {
            live_root: PathBuf::from("/tmp/live"),
            previous_cursor: None,
        }) {
            Ok(_) => panic!("error response unexpectedly produced a lease"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ScanErrorKind::LeaseExpired);
        assert_eq!(error.message(), "expired");
        server.join().unwrap();
    }

    #[test]
    fn dispatcher_serves_client_lifecycle() {
        struct Handler {
            scan_root: PathBuf,
            outcomes: Arc<Mutex<Vec<ScanOutcome>>>,
        }

        impl ScanRequestHandler for Handler {
            fn begin_scan(
                &mut self,
                request: BeginScanRequest,
            ) -> Result<ServerScanLease, ScanError> {
                assert_eq!(request.live_root, PathBuf::from("/tmp/live"));
                assert_eq!(request.previous_cursor, Some(b"previous".to_vec()));
                Ok(ServerScanLease {
                    session_id: b"session".to_vec(),
                    cursor: b"cursor".to_vec(),
                    invalidation: Invalidation::ExactPaths(vec![b"file".to_vec()]),
                    expires_boottime_ns: u64::MAX,
                    identity: SnapshotIdentity {
                        filesystem_uuid: [1; 16],
                        subvolume_uuid: [2; 16],
                        read_only: true,
                    },
                    scan_root: File::open(&self.scan_root).unwrap(),
                })
            }

            fn renew_scan(&mut self, session_id: &[u8]) -> Result<(), ScanError> {
                assert_eq!(session_id, b"session");
                Ok(())
            }

            fn finish_scan(
                &mut self,
                session_id: &[u8],
                outcome: ScanOutcome,
            ) -> Result<(), ScanError> {
                assert_eq!(session_id, b"session");
                self.outcomes.lock().unwrap().push(outcome);
                Ok(())
            }
        }

        let (client_socket, server_socket) = ScanSocket::pair().unwrap();
        let scan_root = tempfile::tempdir().unwrap();
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = SocketScanDispatcher::new(Handler {
            scan_root: scan_root.path().to_owned(),
            outcomes: outcomes.clone(),
        });
        let server = std::thread::spawn(move || {
            dispatcher.serve_one(&server_socket).unwrap();
            dispatcher.serve_one(&server_socket).unwrap();
            dispatcher.serve_one(&server_socket).unwrap();
        });
        let mut client = SocketScanClient {
            socket: Arc::new(Mutex::new(client_socket)),
        };
        let mut lease = client
            .begin_scan(&BeginScanRequest {
                live_root: PathBuf::from("/tmp/live"),
                previous_cursor: Some(b"previous".to_vec()),
            })
            .unwrap();
        assert_eq!(
            lease.invalidation,
            Invalidation::ExactPaths(vec![b"file".to_vec()])
        );
        lease.renew().unwrap();
        lease.finish(ScanOutcome::Committed).unwrap();
        server.join().unwrap();
        assert_eq!(
            outcomes.lock().unwrap().as_slice(),
            &[ScanOutcome::Committed]
        );
    }

    #[test]
    fn listener_accepts_explicit_socket_client() {
        let temp_dir = tempfile::tempdir().unwrap();
        let socket_path = temp_dir.path().join("scan.sock");
        let listener = match ScanSocketListener::bind(&socket_path, 0o600) {
            Ok(listener) => listener,
            // Some sandboxes permit socketpair but deny pathname socket bind.
            Err(error) if error.kind() == ScanErrorKind::Unavailable => return,
            Err(error) => panic!("bind scan listener: {error}"),
        };
        let connector = std::thread::spawn({
            let socket_path = socket_path.clone();
            move || SocketScanClient::connect(&socket_path).unwrap()
        });
        let _connection = listener.accept().unwrap();
        let _client = connector.join().unwrap();
        assert_eq!(
            std::fs::metadata(&socket_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn socket_reports_kernel_peer_credentials() {
        let (left, _right) = ScanSocket::pair().unwrap();
        let credentials = left.peer_credentials().unwrap();
        assert_eq!(credentials.uid, unsafe { libc::geteuid() });
        assert_eq!(credentials.gid, unsafe { libc::getegid() });
        assert!(credentials.pid > 0);
    }
}
