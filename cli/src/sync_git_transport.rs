// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Native Git smart transport over an already authenticated jj connection.
//!
//! Both Git repositories are disposable and contain only controlled refs and
//! configuration. Git's remote-ext helper starts a local bridge, never SSH.
//! The peer starts the requested service locally and relays the same bytes.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Child;
use std::process::ChildStderr;
use std::process::ChildStdin;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use jj_lib::sync::protocol::Message;
use jj_lib::sync::protocol::ProtocolError;
use jj_lib::sync::protocol::Session;
use thiserror::Error;

use crate::command_error::CommandError;
use crate::command_error::user_error_with_message;

const CHUNK_SIZE: usize = 64 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_TERMINAL_ERROR_BYTES: usize = 2048;
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_LENGTH: usize = 32;

#[derive(Clone, Copy, Debug)]
pub(crate) enum GitAction {
    Push,
    Fetch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitService {
    ReceivePack,
    UploadPack,
}

impl GitService {
    fn marker(self) -> u8 {
        match self {
            Self::ReceivePack => 1,
            Self::UploadPack => 2,
        }
    }

    fn parse(name: &str) -> io::Result<Self> {
        match name {
            "git-receive-pack" => Ok(Self::ReceivePack),
            "git-upload-pack" => Ok(Self::UploadPack),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported native Git synchronization service",
            )),
        }
    }
}

/// Protocol bytes exclude jj's fixed-size outer frame headers.
#[derive(Debug)]
pub(crate) struct GitTransferReport {
    pub(crate) sent_bytes: u64,
    pub(crate) received_bytes: u64,
    pub(crate) stderr: String,
}

#[derive(Debug, Error)]
enum GitTransportError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("Peer's native Git synchronization failed: {0}")]
    Peer(String),
    #[error("Native Git synchronization process exited with {status}{diagnostic}")]
    Process {
        status: ExitStatus,
        diagnostic: String,
    },
    #[error("Native Git transfer failed{diagnostic}")]
    Transfer {
        source: Box<Self>,
        diagnostic: String,
    },
}

fn failed(error: GitTransportError) -> CommandError {
    user_error_with_message("Native Git synchronization failed", error)
}

/// Runs push or fetch through a local bridge and the current framed connection.
pub(crate) fn run_client<R: Read + Send, W: Write>(
    git: &Path,
    repository: &Path,
    action: GitAction,
    max_received_bytes: u64,
    reader: &mut R,
    writer: &mut W,
) -> Result<GitTransferReport, CommandError> {
    run_client_inner(git, repository, action, max_received_bytes, reader, writer).map_err(failed)
}

fn run_client_inner<R: Read + Send, W: Write>(
    git: &Path,
    repository: &Path,
    action: GitAction,
    max_received_bytes: u64,
    reader: &mut R,
    writer: &mut W,
) -> Result<GitTransferReport, GitTransportError> {
    // Keep the socket path short enough for sockaddr_un even when the user's
    // TMPDIR is long. TempDir creates a private directory with mode 0700.
    let directory = tempfile::Builder::new()
        .prefix("jj-git-")
        .tempdir_in("/tmp")?;
    let socket_path = directory.path().join("s");
    let listener = UnixListener::bind(&socket_path)?;
    listener.set_nonblocking(true)?;
    let token = format!("{:032x}", rand::random::<u128>());
    let executable = std::env::current_exe()?;
    let url = bridge_url(&executable, &socket_path, &token);
    let mut command = git_command(git, repository);
    match action {
        GitAction::Push => {
            command.args([
                "-c",
                "push.followTags=false",
                "-c",
                "push.negotiate=false",
                "push",
                "--progress",
                "--porcelain",
                "--atomic",
                "--no-verify",
                "--no-signed",
                "--recurse-submodules=no",
            ]);
            command
                .arg(url)
                .arg("refs/jj-sync/export/*:refs/jj-sync/incoming/*");
        }
        GitAction::Fetch => {
            configure_fetch(&mut command);
            command
                .arg(url)
                .arg("+refs/jj-sync/export/*:refs/jj-sync/incoming/*");
        }
    }
    command.stdin(Stdio::null()).stdout(Stdio::null());
    let process = GitChild::spawn(command)?;
    let service = match action {
        GitAction::Push => GitService::ReceivePack,
        GitAction::Fetch => GitService::UploadPack,
    };
    let connection = match accept_bridge(&listener, &process, &token, service) {
        Ok(connection) => connection,
        Err(error) => return Err(process.abort(error)),
    };
    let input = GitInput::Socket(connection.try_clone()?);
    let cancel_socket = connection.try_clone()?;
    let counts = match relay(
        reader,
        writer,
        connection,
        input,
        &process,
        Some(cancel_socket),
        max_received_bytes,
    ) {
        Ok(counts) => counts,
        Err(error) => return Err(process.abort(error)),
    };
    process.finish(counts)
}

fn configure_fetch(command: &mut Command) {
    command.args([
        "-c",
        "fetch.unpackLimit=0",
        "fetch",
        // Hide successful ref updates, while forced progress still enables
        // upload-pack progress. Git errors and failure status remain visible.
        "--quiet",
        "--progress",
        "--no-tags",
        "--no-recurse-submodules",
        "--no-auto-maintenance",
        "--no-write-fetch-head",
        "--no-write-commit-graph",
        "--refmap=",
    ]);
}

/// Runs the peer's Git service locally; its protocol is carried by jj, not SSH.
pub(crate) fn run_server<R: Read + Send, W: Write>(
    git: &Path,
    repository: &Path,
    service: GitService,
    max_received_bytes: u64,
    reader: &mut R,
    writer: &mut W,
) -> Result<GitTransferReport, CommandError> {
    run_server_inner(git, repository, service, max_received_bytes, reader, writer).map_err(failed)
}

fn run_server_inner<R: Read + Send, W: Write>(
    git: &Path,
    repository: &Path,
    service: GitService,
    max_received_bytes: u64,
    reader: &mut R,
    writer: &mut W,
) -> Result<GitTransferReport, GitTransportError> {
    let mut command = git_command(git, repository);
    match service {
        GitService::ReceivePack => {
            command.args(["-c", "receive.unpackLimit=0", "receive-pack"]);
        }
        GitService::UploadPack => {
            command.args(["upload-pack", "--strict"]);
        }
    }
    command
        .arg(repository)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let process = GitChild::spawn(command)?;
    let (input, output) = {
        let mut child = process.child.lock().unwrap();
        (
            GitInput::Pipe(child.stdin.take()),
            child.stdout.take().unwrap(),
        )
    };
    let counts = match relay(
        reader,
        writer,
        output,
        input,
        &process,
        None,
        max_received_bytes,
    ) {
        Ok(counts) => counts,
        Err(error) => return Err(process.abort(error)),
    };
    process.finish(counts)
}

fn git_command(git: &Path, repository: &Path) -> Command {
    let mut command = Command::new(git);
    // Never inherit caller-controlled repository selection or command injection
    // configuration into these disposable transport repositories.
    for (name, _) in std::env::vars_os() {
        let bytes = name.as_bytes();
        if bytes.starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    command
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .arg("--git-dir")
        .arg(repository)
        .args([
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.ext.allow=always",
            "-c",
            "protocol.version=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .stderr(Stdio::piped());
    command
}

/// remote-ext has its own small percent-escape grammar, with no shell involved.
fn bridge_url(executable: &Path, socket: &Path, token: &str) -> OsString {
    let mut result = b"ext::".to_vec();
    push_ext_argument(&mut result, executable.as_os_str());
    result.extend_from_slice(b" util sync-git-bridge --socket ");
    push_ext_argument(&mut result, socket.as_os_str());
    result.extend_from_slice(b" --token ");
    result.extend_from_slice(token.as_bytes());
    result.extend_from_slice(b" %S");
    OsString::from_vec(result)
}

fn push_ext_argument(result: &mut Vec<u8>, value: &OsStr) {
    for byte in value.as_bytes() {
        match byte {
            b'%' => result.extend_from_slice(b"%%"),
            b' ' => result.extend_from_slice(b"% "),
            _ => result.push(*byte),
        }
    }
}

fn accept_bridge(
    listener: &UnixListener,
    process: &GitChild,
    token: &str,
    service: GitService,
) -> Result<UnixStream, GitTransportError> {
    let deadline = Instant::now() + BRIDGE_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut connection, _)) => {
                connection.set_read_timeout(Some(BRIDGE_TIMEOUT))?;
                let mut handshake = [0; TOKEN_LENGTH + 1];
                connection.read_exact(&mut handshake)?;
                if &handshake[..TOKEN_LENGTH] != token.as_bytes()
                    || handshake[TOKEN_LENGTH] != service.marker()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "native Git bridge authentication failed",
                    )
                    .into());
                }
                connection.set_read_timeout(None)?;
                return Ok(connection);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if let Some(status) = process.child.lock().unwrap().try_wait()? {
                    return Err(GitTransportError::Process {
                        status,
                        diagnostic: String::new(),
                    });
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "native Git did not connect its synchronization bridge",
                    )
                    .into());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

enum GitInput {
    Socket(UnixStream),
    Pipe(Option<ChildStdin>),
}

impl GitInput {
    fn finish(&mut self) -> io::Result<()> {
        match self {
            Self::Socket(socket) => socket.shutdown(Shutdown::Write),
            Self::Pipe(pipe) => {
                drop(pipe.take());
                Ok(())
            }
        }
    }
}

impl Write for GitInput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Socket(socket) => socket.write(buffer),
            Self::Pipe(Some(pipe)) => pipe.write(buffer),
            Self::Pipe(None) => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Socket(socket) => socket.flush(),
            Self::Pipe(Some(pipe)) => pipe.flush(),
            Self::Pipe(None) => Ok(()),
        }
    }
}

fn relay<R: Read + Send, W: Write, O: Read>(
    reader: &mut R,
    writer: &mut W,
    mut output: O,
    mut input: GitInput,
    process: &GitChild,
    cancel_socket: Option<UnixStream>,
    max_received_bytes: u64,
) -> Result<(u64, u64), GitTransportError> {
    thread::scope(|scope| {
        let child = process.child.clone();
        let incoming_failure = Arc::new(Mutex::new(None::<String>));
        let failure_writer = incoming_failure.clone();
        let incoming = scope.spawn(move || {
            let mut stream = Session::new(reader, io::sink());
            let result = (|| {
                let mut total = 0u64;
                loop {
                    match stream.receive_bounded(CHUNK_SIZE + 4096)? {
                        Message::Record(bytes)
                            if !bytes.is_empty() && bytes.len() <= CHUNK_SIZE =>
                        {
                            total = total.checked_add(bytes.len() as u64).ok_or_else(|| {
                                io::Error::other("native Git byte count overflow")
                            })?;
                            if total > max_received_bytes {
                                return Err(io::Error::other(
                                    "native Git transfer exceeds the local \
                                     sync.max-quarantine-bytes budget",
                                )
                                .into());
                            }
                            input.write_all(&bytes)?;
                        }
                        Message::Done(bytes) if bytes.is_empty() => {
                            input.finish()?;
                            return Ok(total);
                        }
                        Message::Error(message) => return Err(GitTransportError::Peer(message)),
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "unexpected message during native Git synchronization",
                            )
                            .into());
                        }
                    }
                }
            })();
            if let Err(error) = &result {
                // Set the failure before killing Git closes its output. The
                // outgoing relay must notify the peer of cancellation instead
                // of reporting a successful directional EOF and abandoning its
                // reader while the peer is still writing pack data.
                *failure_writer.lock().unwrap() = Some(terminal_error(&error.to_string()));
                kill_child(&child);
                if let Some(socket) = cancel_socket {
                    let _shutdown_result = socket.shutdown(Shutdown::Both);
                }
            }
            result
        });
        let mut stream = Session::new(io::empty(), writer);
        let outgoing = (|| {
            let mut buffer = [0; CHUNK_SIZE];
            let mut total = 0u64;
            loop {
                let length = match output.read(&mut buffer) {
                    Ok(length) => length,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => return Err(GitTransportError::Io(err)),
                };
                if length == 0 {
                    let failure = incoming_failure.lock().unwrap().clone();
                    match failure {
                        Some(error) => stream.send(&Message::Error(error))?,
                        None => stream.send(&Message::Done(Vec::new()))?,
                    }
                    return Ok(total);
                }
                stream.send(&Message::Record(buffer[..length].to_vec()))?;
                total = total
                    .checked_add(length as u64)
                    .ok_or_else(|| io::Error::other("native Git byte count overflow"))?;
            }
        })();
        if let Err(err) = &outgoing {
            kill_child(&process.child);
            // If the writer still works, wake the peer's receive loop and
            // terminate its Git child before waiting for the reader to finish.
            let _send_error_result = stream.send(&Message::Error(terminal_error(&err.to_string())));
        }
        let received = incoming
            .join()
            .map_err(|_| io::Error::other("native Git input relay panicked"))??;
        Ok((outgoing?, received))
    })
}

fn terminal_error(message: &str) -> String {
    if message.len() <= MAX_TERMINAL_ERROR_BYTES {
        return message.to_owned();
    }
    let suffix = "\n(truncated; see helper stderr)";
    let mut end = MAX_TERMINAL_ERROR_BYTES - suffix.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &message[..end])
}

/// The only entry point used by Git's local remote-ext subprocess.
pub(crate) fn run_bridge(socket: &Path, token: &str, service: &str) -> io::Result<()> {
    if token.len() != TOKEN_LENGTH || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid native Git bridge token",
        ));
    }
    let service = GitService::parse(service)?;
    let mut connection = UnixStream::connect(socket)?;
    connection.write_all(token.as_bytes())?;
    connection.write_all(&[service.marker()])?;
    bridge_stdio(connection)
}

/// A service can close stdout before Git closes stdin. Poll both directions so
/// that early service EOF is delivered immediately instead of waiting for an
/// uncancellable stdin reader. Each pending buffer is at most one chunk.
fn bridge_stdio(connection: UnixStream) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    bridge_descriptors(connection, stdin.as_raw_fd(), stdout.as_raw_fd())
}

#[expect(unsafe_code)]
fn bridge_descriptors(
    connection: UnixStream,
    input_fd: libc::c_int,
    output_fd: libc::c_int,
) -> io::Result<()> {
    let socket_fd = connection.as_raw_fd();
    let _input_flags = NonblockingFlags::set(input_fd)?;
    let _output_flags = NonblockingFlags::set(output_fd)?;
    connection.set_nonblocking(true)?;
    let mut to_socket = PendingBytes::new();
    let mut to_stdout = PendingBytes::new();
    let mut input_closed = false;
    let mut output_closed = false;
    let mut write_closed = false;
    loop {
        if output_closed && to_stdout.is_empty() {
            return Ok(());
        }
        if input_closed && to_socket.is_empty() && !write_closed {
            connection.shutdown(Shutdown::Write)?;
            write_closed = true;
        }
        let input_active = !input_closed && to_socket.is_empty();
        let output_active = !to_stdout.is_empty();
        let mut descriptors = [
            libc::pollfd {
                fd: if input_active { input_fd } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: socket_fd,
                events: if to_stdout.is_empty() && !output_closed {
                    libc::POLLIN
                } else {
                    0
                } | if !to_socket.is_empty() {
                    libc::POLLOUT
                } else {
                    0
                },
                revents: 0,
            },
            libc::pollfd {
                fd: if output_active { output_fd } else { -1 },
                events: libc::POLLOUT,
                revents: 0,
            },
        ];
        // SAFETY: All descriptors remain owned by the handles above, and the
        // array is writable and contains exactly its declared number of entries.
        if unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) } < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if descriptors
            .iter()
            .any(|fd| fd.revents & libc::POLLNVAL != 0)
        {
            return Err(io::Error::other("native Git bridge descriptor closed"));
        }
        if descriptors[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            input_closed = to_socket.read_from(input_fd)?;
        }
        if descriptors[1].revents & libc::POLLOUT != 0 {
            to_socket.write_to(socket_fd)?;
        }
        if to_stdout.is_empty()
            && !output_closed
            && descriptors[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
        {
            output_closed = to_stdout.read_from(socket_fd)?;
        }
        if descriptors[2].revents & (libc::POLLOUT | libc::POLLHUP | libc::POLLERR) != 0 {
            to_stdout.write_to(output_fd)?;
        }
    }
}

struct PendingBytes {
    bytes: Box<[u8; CHUNK_SIZE]>,
    start: usize,
    end: usize,
}

impl PendingBytes {
    fn new() -> Self {
        Self {
            bytes: Box::new([0; CHUNK_SIZE]),
            start: 0,
            end: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.start == self.end
    }

    #[expect(unsafe_code)]
    fn read_from(&mut self, descriptor: libc::c_int) -> io::Result<bool> {
        debug_assert!(self.is_empty());
        // SAFETY: The descriptor is borrowed from a live owner. The complete
        // buffer is writable for the exact length passed to read.
        let length = unsafe { libc::read(descriptor, self.bytes.as_mut_ptr().cast(), CHUNK_SIZE) };
        if length < 0 {
            let err = io::Error::last_os_error();
            if matches!(
                err.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                return Ok(false);
            }
            return Err(err);
        }
        self.start = 0;
        self.end = length as usize;
        Ok(length == 0)
    }

    #[expect(unsafe_code)]
    fn write_to(&mut self, descriptor: libc::c_int) -> io::Result<()> {
        // SAFETY: The descriptor is borrowed from a live owner. start..end is
        // within the buffer, and its remaining bytes stay live through write.
        let length = unsafe {
            libc::write(
                descriptor,
                self.bytes[self.start..self.end].as_ptr().cast(),
                self.end - self.start,
            )
        };
        if length < 0 {
            let err = io::Error::last_os_error();
            if matches!(
                err.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                return Ok(());
            }
            return Err(err);
        }
        if length == 0 && !self.is_empty() {
            return Err(io::ErrorKind::WriteZero.into());
        }
        self.start += length as usize;
        Ok(())
    }
}

struct NonblockingFlags {
    descriptor: libc::c_int,
    original: libc::c_int,
}

impl NonblockingFlags {
    #[expect(unsafe_code)]
    fn set(descriptor: libc::c_int) -> io::Result<Self> {
        // SAFETY: The caller keeps the descriptor's owner alive until this
        // guard is dropped. Both operations only inspect/change status flags.
        let original = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if original < 0
            || unsafe { libc::fcntl(descriptor, libc::F_SETFL, original | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            descriptor,
            original,
        })
    }
}

impl Drop for NonblockingFlags {
    #[expect(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: The guard does not outlive the descriptor's owner and restores
        // exactly the flags read before the bridge touched them.
        let _restore_result = unsafe { libc::fcntl(self.descriptor, libc::F_SETFL, self.original) };
    }
}

struct GitChild {
    child: Arc<Mutex<Child>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stop_stderr: Arc<AtomicBool>,
    stderr_thread: Option<thread::JoinHandle<io::Result<()>>>,
    finished: bool,
}

impl GitChild {
    fn spawn(mut command: Command) -> io::Result<Self> {
        tracing::debug!(program = ?command.get_program(), "Starting native Git synchronization");
        // Inherit jj's process group, so cancelling the authenticated helper
        // also terminates Git, remote-ext, and index-pack descendants.
        let mut child = command.spawn()?;
        let mut stderr = child.stderr.take().unwrap();
        if let Err(err) = nonblocking_stderr(&stderr) {
            let _kill_result = child.kill();
            let _wait_result = child.wait();
            return Err(err);
        }
        let capture = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_capture = capture.clone();
        let thread_stop = stop.clone();
        let stderr_thread = thread::Builder::new()
            .name("jj-sync-git-stderr".to_owned())
            .spawn(move || {
                let mut buffer = [0; 8192];
                let mut after_stop = 0;
                loop {
                    match stderr.read(&mut buffer) {
                        Ok(0) => return Ok(()),
                        Ok(length) => {
                            let bytes = &buffer[..length];
                            tracing::debug!(
                                git_stderr = %String::from_utf8_lossy(bytes),
                                "Native Git synchronization progress"
                            );
                            let mut capture = thread_capture.lock().unwrap();
                            let excess = (capture.len() + length).saturating_sub(MAX_STDERR_BYTES);
                            capture.drain(..excess);
                            capture.extend_from_slice(bytes);
                            if thread_stop.load(Ordering::Acquire) {
                                after_stop += length;
                                if after_stop >= MAX_STDERR_BYTES {
                                    return Ok(());
                                }
                            }
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            if thread_stop.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            thread::park_timeout(Duration::from_millis(10));
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(err) => return Err(err),
                    }
                }
            });
        let stderr_thread = match stderr_thread {
            Ok(thread) => thread,
            Err(err) => {
                let _kill_result = child.kill();
                let _wait_result = child.wait();
                return Err(err);
            }
        };
        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            stderr: capture,
            stop_stderr: stop,
            stderr_thread: Some(stderr_thread),
            finished: false,
        })
    }

    fn finish(
        mut self,
        (sent_bytes, received_bytes): (u64, u64),
    ) -> Result<GitTransferReport, GitTransportError> {
        let status = self.child.lock().unwrap().wait()?;
        self.finished = true;
        self.join_stderr()?;
        if !status.success() {
            return Err(GitTransportError::Process {
                status,
                diagnostic: self.diagnostic(),
            });
        }
        let stderr = String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned();
        Ok(GitTransferReport {
            sent_bytes,
            received_bytes,
            stderr,
        })
    }

    fn diagnostic(&self) -> String {
        let stderr = self.stderr.lock().unwrap();
        if stderr.is_empty() {
            String::new()
        } else {
            format!(
                "\nGit stderr (last 64 KiB):\n{}",
                String::from_utf8_lossy(&stderr).trim_end()
            )
        }
    }

    fn abort(mut self, source: GitTransportError) -> GitTransportError {
        kill_child(&self.child);
        let _wait_result = self.child.lock().unwrap().wait();
        self.finished = true;
        let _stderr_result = self.join_stderr();
        GitTransportError::Transfer {
            source: Box::new(source),
            diagnostic: self.diagnostic(),
        }
    }

    fn join_stderr(&mut self) -> io::Result<()> {
        self.stop_stderr.store(true, Ordering::Release);
        if let Some(thread) = self.stderr_thread.take() {
            thread.thread().unpark();
            thread
                .join()
                .map_err(|_| io::Error::other("native Git stderr reader panicked"))??;
        }
        Ok(())
    }
}

impl Drop for GitChild {
    fn drop(&mut self) {
        if !self.finished {
            kill_child(&self.child);
            let _wait_result = self.child.lock().unwrap().wait();
        }
        let _stderr_result = self.join_stderr();
    }
}

fn kill_child(child: &Mutex<Child>) {
    let mut child = child.lock().unwrap();
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _kill_result = child.kill();
    }
}

#[expect(unsafe_code)]
fn nonblocking_stderr(stderr: &ChildStderr) -> io::Result<()> {
    // SAFETY: ChildStderr owns the descriptor through both calls. Only this
    // read-side descriptor's status flags change; the child's writer is intact.
    let flags = unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;

    use gix::objs::Write as _;

    use super::*;

    fn cat_process() -> GitChild {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exec cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        GitChild::spawn(command).unwrap()
    }

    fn pipe_ends(process: &GitChild) -> (GitInput, std::process::ChildStdout) {
        let mut child = process.child.lock().unwrap();
        (
            GitInput::Pipe(child.stdin.take()),
            child.stdout.take().unwrap(),
        )
    }

    fn framed(messages: &[Message]) -> Cursor<Vec<u8>> {
        let mut session = Session::new(io::empty(), Vec::new());
        for message in messages {
            session.send(message).unwrap();
        }
        Cursor::new(session.into_inner().1)
    }

    #[test]
    fn quiet_fetch_keeps_remote_progress_and_actionable_ref_errors() {
        let directory = tempfile::tempdir().unwrap();
        let bare = |path: &Path| {
            gix::ThreadSafeRepository::init_opts(
                path,
                gix::create::Kind::Bare,
                gix::create::Options::default(),
                gix::open::Options::isolated(),
            )
            .unwrap()
            .to_thread_local()
        };
        let source = bare(&directory.path().join("source"));
        let receiver = bare(&directory.path().join("receiver"));
        let tree = source
            .objects
            .write_buf(gix::objs::Kind::Tree, &[])
            .unwrap();
        let commit = |parent: Option<gix::ObjectId>, name: &str| {
            let parent = parent.map_or_else(String::new, |id| format!("parent {id}\n"));
            source
                .objects
                .write_buf(
                    gix::objs::Kind::Commit,
                    format!(
                        "tree {tree}\n{parent}author A <a@example.com> 0 +0000\ncommitter A \
                         <a@example.com> 0 +0000\n\n{name}\n"
                    )
                    .as_bytes(),
                )
                .unwrap()
        };
        let first = commit(None, "first");
        source
            .reference(
                "refs/jj-sync/export/first",
                first,
                gix::refs::transaction::PreviousValue::Any,
                "test",
            )
            .unwrap();
        let fetch = || {
            let mut command = git_command(Path::new("git"), receiver.path());
            // This test uses an owned local repository instead of the bridge.
            command.args(["-c", "protocol.file.allow=always"]);
            configure_fetch(&mut command);
            command
                .arg(source.path())
                .arg("+refs/jj-sync/export/*:refs/jj-sync/incoming/*")
                .env("LC_ALL", "C")
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            GitChild::spawn(command).unwrap()
        };
        let report = fetch().finish((0, 0)).unwrap();
        assert!(
            report.stderr.contains("Counting objects:"),
            "forced remote progress was lost: {}",
            report.stderr
        );
        assert!(!report.stderr.contains("[new ref]"), "{}", report.stderr);
        assert!(!report.stderr.contains(" -> "), "{}", report.stderr);
        assert_eq!(
            receiver
                .find_reference("refs/jj-sync/incoming/first")
                .unwrap()
                .id()
                .detach(),
            first
        );

        let next = commit(Some(first), "next");
        source
            .reference(
                "refs/jj-sync/export/next",
                next,
                gix::refs::transaction::PreviousValue::Any,
                "test",
            )
            .unwrap();
        // A failed ref transaction must retain its useful stderr even though
        // successful per-ref summaries are quiet.
        let lock = receiver.path().join("refs/jj-sync/incoming/next.lock");
        fs::write(lock, b"another writer").unwrap();
        let error = fetch().finish((0, 0)).unwrap_err();
        assert!(error.to_string().contains("cannot lock ref"), "{error}");
    }

    #[test]
    fn remote_ext_arguments_preserve_literal_path_bytes() {
        let executable = Path::new(OsStr::from_bytes(b"/a b/%S/';$(literal)\xff/jj"));
        let socket = Path::new("/tmp/jj socket%/s");
        assert_eq!(
            bridge_url(executable, socket, "0123456789abcdef0123456789abcdef").as_bytes(),
            b"ext::/a% b/%%S/';$(literal)\xff/jj util sync-git-bridge --socket /tmp/jj% socket%%/s --token 0123456789abcdef0123456789abcdef %S",
        );
    }

    #[test]
    fn framed_directional_eof_closes_git_stdin_and_drains_stdout() {
        let process = cat_process();
        let (input, output) = pipe_ends(&process);
        let mut reader = framed(&[
            Message::Record(b"bounded Git protocol bytes".to_vec()),
            Message::Done(Vec::new()),
        ]);
        let mut writer = Vec::new();
        let counts = relay(
            &mut reader,
            &mut writer,
            output,
            input,
            &process,
            None,
            u64::MAX,
        )
        .unwrap();
        let report = process.finish(counts).unwrap();
        assert_eq!(report.sent_bytes, 26);
        assert_eq!(report.received_bytes, 26);
        let mut result = Session::new(Cursor::new(writer), io::sink());
        let mut body = Vec::new();
        loop {
            match result.receive().unwrap() {
                Message::Record(bytes) => body.extend(bytes),
                Message::Done(bytes) if bytes.is_empty() => break,
                message => panic!("unexpected output {message:?}"),
            }
        }
        assert_eq!(body, b"bounded Git protocol bytes");
    }

    #[test]
    fn bridge_delivers_service_eof_without_waiting_for_git_stdin() {
        let (input, input_writer) = UnixStream::pair().unwrap();
        let (output, mut output_reader) = UnixStream::pair().unwrap();
        let (connection, mut service) = UnixStream::pair().unwrap();
        output_reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let bridge = thread::spawn(move || {
            bridge_descriptors(connection, input.as_raw_fd(), output.as_raw_fd())
        });
        service.write_all(b"early service failure").unwrap();
        service.shutdown(Shutdown::Write).unwrap();
        let mut received = Vec::new();
        output_reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, b"early service failure");
        bridge.join().unwrap().unwrap();
        // Retain the writer until after the bridge exits: no stdin EOF was sent.
        drop(input_writer);
    }

    #[test]
    fn oversized_git_transfers_cancel_and_reap_the_local_child() {
        for (size, budget) in [
            (CHUNK_SIZE + 1, u64::MAX),
            (CHUNK_SIZE + 4097, u64::MAX),
            (8, 4),
        ] {
            let process = cat_process();
            let (input, output) = pipe_ends(&process);
            let mut reader = framed(&[Message::Record(vec![0; size])]);
            let mut writer = Vec::new();
            let error = relay(
                &mut reader,
                &mut writer,
                output,
                input,
                &process,
                None,
                budget,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error,
                    GitTransportError::Io(_) | GitTransportError::Protocol(_)
                ),
                "{error}"
            );
            assert!(!process.child.lock().unwrap().wait().unwrap().success());
            let mut result = Session::new(Cursor::new(writer), io::sink());
            let Message::Error(message) = result.receive().unwrap() else {
                panic!("failed Git receipt must send cancellation, not successful EOF");
            };
            assert!(message.len() <= MAX_TERMINAL_ERROR_BYTES);
        }
    }

    #[test]
    fn terminal_native_errors_are_small_and_preserve_utf8() {
        let message = terminal_error(&"é".repeat(MAX_TERMINAL_ERROR_BYTES));
        assert!(message.len() <= MAX_TERMINAL_ERROR_BYTES);
        assert!(message.ends_with("(truncated; see helper stderr)"));
    }

    #[test]
    fn failed_native_service_preserves_diagnostics_and_exit_status() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf 'useful Git failure' >&2; exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let process = GitChild::spawn(command).unwrap();
        let child = process.child.clone();
        let error = process.finish((0, 0)).unwrap_err();
        assert!(error.to_string().contains("useful Git failure"));
        assert_eq!(
            child.lock().unwrap().try_wait().unwrap().unwrap().code(),
            Some(7)
        );
    }

    #[test]
    fn bridge_rejects_invalid_token_or_service_before_connecting() {
        let path = Path::new("/does-not-exist/jj-sync-socket");
        assert_eq!(
            run_bridge(path, "invalid", "git-upload-pack")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            run_bridge(path, "0123456789abcdef0123456789abcdef", "ssh")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput,
        );
    }
}
