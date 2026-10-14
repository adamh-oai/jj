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

//! Bounded subprocess I/O shared by synchronization transports.

use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Child;
use std::process::ChildStderr;
use std::process::ChildStdin;
use std::process::Command;
use std::process::ExitStatus;
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

const CHUNK_SIZE: usize = 64 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_TERMINAL_ERROR_BYTES: usize = 2048;
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_LENGTH: usize = 32;

/// Reports cumulative bytes without jj's fixed-size outer frame headers.
/// Calls are serialized across the two transfer directions.
pub(crate) type ProgressCallback<'a> = dyn Fn(u64, u64) + Sync + 'a;

#[derive(Clone, Copy)]
pub(crate) struct ProcessTransferOptions<'a> {
    pub(crate) max_received_bytes: u64,
    pub(crate) progress: Option<&'a ProgressCallback<'a>>,
}

/// Protocol bytes exclude jj's fixed-size outer frame headers.
#[derive(Debug)]
pub(crate) struct ProcessTransferReport {
    pub(crate) sent_bytes: u64,
    pub(crate) received_bytes: u64,
    pub(crate) stderr: String,
}

#[derive(Debug, Error)]
pub(crate) enum ProcessError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("Peer's synchronization subprocess failed: {0}")]
    Peer(String),
    #[error("{name} synchronization process exited with {status}{diagnostic}")]
    Process {
        name: &'static str,
        status: ExitStatus,
        diagnostic: String,
    },
    #[error("Synchronization subprocess transfer failed{diagnostic}")]
    Transfer {
        source: Box<Self>,
        diagnostic: String,
    },
}

pub(crate) fn accept_bridge(
    listener: &UnixListener,
    process: &ProcessChild,
    token: &str,
    marker: u8,
) -> Result<UnixStream, ProcessError> {
    let deadline = Instant::now() + BRIDGE_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut connection, _)) => {
                connection.set_read_timeout(Some(BRIDGE_TIMEOUT))?;
                let mut handshake = [0; TOKEN_LENGTH + 1];
                connection.read_exact(&mut handshake)?;
                if &handshake[..TOKEN_LENGTH] != token.as_bytes()
                    || handshake[TOKEN_LENGTH] != marker
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "synchronization bridge authentication failed",
                    )
                    .into());
                }
                connection.set_read_timeout(None)?;
                return Ok(connection);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if let Some(status) = process.child.lock().unwrap().try_wait()? {
                    return Err(ProcessError::Process {
                        name: process.name,
                        status,
                        diagnostic: String::new(),
                    });
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "subprocess did not connect its synchronization bridge",
                    )
                    .into());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

pub(crate) enum ProcessInput {
    Socket(UnixStream),
    Pipe(Option<ChildStdin>),
}

impl ProcessInput {
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

impl Write for ProcessInput {
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

pub(crate) fn relay<R: Read + Send, W: Write, O: Read>(
    reader: &mut R,
    writer: &mut W,
    mut output: O,
    mut input: ProcessInput,
    process: &ProcessChild,
    cancel_socket: Option<UnixStream>,
    options: ProcessTransferOptions<'_>,
) -> Result<(u64, u64), ProcessError> {
    let progress_counts = Mutex::new((0u64, 0u64));
    let report_progress = |sent: u64, received: u64| {
        if let Some(progress) = options.progress {
            // Keep updates and notification under one lock so concurrent
            // reads and writes never report older directional counters.
            let mut counts = progress_counts.lock().unwrap();
            counts.0 += sent;
            counts.1 += received;
            progress(counts.0, counts.1);
        }
    };
    thread::scope(|scope| {
        let child = process.child.clone();
        let incoming_failure = Arc::new(Mutex::new(None::<String>));
        let failure_writer = incoming_failure.clone();
        let report_progress = &report_progress;
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
                                io::Error::other("synchronization subprocess byte count overflow")
                            })?;
                            if total > options.max_received_bytes {
                                return Err(io::Error::other(
                                    "synchronization subprocess transfer exceeds the local \
                                     sync.max-quarantine-bytes budget",
                                )
                                .into());
                            }
                            input.write_all(&bytes)?;
                            report_progress(0, bytes.len() as u64);
                        }
                        Message::Done(bytes) if bytes.is_empty() => {
                            input.finish()?;
                            return Ok(total);
                        }
                        Message::Error(message) => return Err(ProcessError::Peer(message)),
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "unexpected message during synchronization subprocess transfer",
                            )
                            .into());
                        }
                    }
                }
            })();
            if let Err(error) = &result {
                // Set the failure before killing the child closes its output. The
                // outgoing relay must notify the peer of cancellation instead
                // of reporting a successful directional EOF and abandoning its
                // reader while the peer is still writing subprocess data.
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
                    Err(err) => return Err(ProcessError::Io(err)),
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
                total = total.checked_add(length as u64).ok_or_else(|| {
                    io::Error::other("synchronization subprocess byte count overflow")
                })?;
                report_progress(length as u64, 0);
            }
        })();
        if let Err(err) = &outgoing {
            kill_child(&process.child);
            // If the writer still works, wake the peer's receive loop and
            // terminate its child before waiting for the reader to finish.
            let _send_error_result = stream.send(&Message::Error(terminal_error(&err.to_string())));
        }
        let received = incoming
            .join()
            .map_err(|_| io::Error::other("synchronization subprocess input relay panicked"))??;
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

/// Connects a local transport helper to its parent's authenticated socket.
pub(crate) fn run_bridge(socket: &Path, token: &str, marker: u8) -> io::Result<()> {
    if token.len() != TOKEN_LENGTH || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid synchronization bridge token",
        ));
    }
    let mut connection = UnixStream::connect(socket)?;
    connection.write_all(token.as_bytes())?;
    connection.write_all(&[marker])?;
    bridge_stdio(connection)
}

/// A service can close stdout before its client closes stdin. Poll both directions so
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
            return Err(io::Error::other("synchronization bridge descriptor closed"));
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

pub(crate) struct ProcessChild {
    pub(crate) child: Arc<Mutex<Child>>,
    name: &'static str,
    stderr: Arc<Mutex<Vec<u8>>>,
    stop_stderr: Arc<AtomicBool>,
    stderr_thread: Option<thread::JoinHandle<io::Result<()>>>,
    finished: bool,
}

impl ProcessChild {
    pub(crate) fn spawn(mut command: Command, name: &'static str) -> io::Result<Self> {
        tracing::debug!(program = ?command.get_program(), transport = name, "Starting synchronization subprocess");
        // Inherit jj's process group, so cancelling the authenticated helper
        // also terminates transport children and their bridge descendants.
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
            .name("jj-sync-process-stderr".to_owned())
            .spawn(move || {
                let mut buffer = [0; 8192];
                let mut after_stop = 0;
                loop {
                    match stderr.read(&mut buffer) {
                        Ok(0) => return Ok(()),
                        Ok(length) => {
                            let bytes = &buffer[..length];
                            tracing::debug!(
                                process_stderr = %String::from_utf8_lossy(bytes),
                                transport = name,
                                "Synchronization subprocess progress"
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
            name,
            stderr: capture,
            stop_stderr: stop,
            stderr_thread: Some(stderr_thread),
            finished: false,
        })
    }

    pub(crate) fn finish(
        mut self,
        (sent_bytes, received_bytes): (u64, u64),
    ) -> Result<ProcessTransferReport, ProcessError> {
        let status = self.child.lock().unwrap().wait()?;
        self.finished = true;
        self.join_stderr()?;
        if !status.success() {
            return Err(ProcessError::Process {
                name: self.name,
                status,
                diagnostic: self.diagnostic(),
            });
        }
        let stderr = String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned();
        Ok(ProcessTransferReport {
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
                "\n{} stderr (last 64 KiB):\n{}",
                self.name,
                String::from_utf8_lossy(&stderr).trim_end()
            )
        }
    }

    pub(crate) fn abort(mut self, source: ProcessError) -> ProcessError {
        kill_child(&self.child);
        let _wait_result = self.child.lock().unwrap().wait();
        self.finished = true;
        let _stderr_result = self.join_stderr();
        ProcessError::Transfer {
            source: Box::new(source),
            diagnostic: self.diagnostic(),
        }
    }

    fn join_stderr(&mut self) -> io::Result<()> {
        self.stop_stderr.store(true, Ordering::Release);
        if let Some(thread) = self.stderr_thread.take() {
            thread.thread().unpark();
            thread.join().map_err(|_| {
                io::Error::other("synchronization subprocess stderr reader panicked")
            })??;
        }
        Ok(())
    }
}

impl Drop for ProcessChild {
    fn drop(&mut self) {
        if !self.finished {
            kill_child(&self.child);
            let _wait_result = self.child.lock().unwrap().wait();
        }
        let _stderr_result = self.join_stderr();
    }
}

pub(crate) fn kill_child(child: &Mutex<Child>) {
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
    use std::io::Cursor;
    use std::process::Stdio;

    use super::*;

    fn cat_process() -> ProcessChild {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exec cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        ProcessChild::spawn(command, "test subprocess").unwrap()
    }

    fn pipe_ends(process: &ProcessChild) -> (ProcessInput, std::process::ChildStdout) {
        let mut child = process.child.lock().unwrap();
        (
            ProcessInput::Pipe(child.stdin.take()),
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
            ProcessTransferOptions {
                max_received_bytes: u64::MAX,
                progress: None,
            },
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
    fn duplex_progress_is_monotonic_and_matches_transferred_bytes() {
        let process = cat_process();
        let (input, output) = pipe_ends(&process);
        let mut reader = framed(&[
            Message::Record(vec![b'a'; CHUNK_SIZE]),
            Message::Record(vec![b'b'; CHUNK_SIZE]),
            Message::Record(b"last chunk".to_vec()),
            Message::Done(Vec::new()),
        ]);
        let mut writer = Vec::new();
        let samples = Mutex::new(Vec::new());
        let progress = |sent, received| samples.lock().unwrap().push((sent, received));
        let counts = relay(
            &mut reader,
            &mut writer,
            output,
            input,
            &process,
            None,
            ProcessTransferOptions {
                max_received_bytes: u64::MAX,
                progress: Some(&progress),
            },
        )
        .unwrap();
        process.finish(counts).unwrap();
        let samples = samples.into_inner().unwrap();
        assert!(
            samples.len() > 3,
            "expected progress throughout both directions"
        );
        assert_eq!(
            counts,
            ((CHUNK_SIZE * 2 + 10) as u64, (CHUNK_SIZE * 2 + 10) as u64)
        );
        assert_eq!(samples.last(), Some(&counts));
        for pair in samples.windows(2) {
            assert!(pair[0].0 <= pair[1].0, "sent count regressed: {pair:?}");
            assert!(pair[0].1 <= pair[1].1, "received count regressed: {pair:?}");
        }
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
                ProcessTransferOptions {
                    max_received_bytes: budget,
                    progress: None,
                },
            )
            .unwrap_err();
            assert!(
                matches!(error, ProcessError::Io(_) | ProcessError::Protocol(_)),
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
        let process = ProcessChild::spawn(command, "test subprocess").unwrap();
        let child = process.child.clone();
        let error = process.finish((0, 0)).unwrap_err();
        assert!(error.to_string().contains("useful Git failure"));
        assert_eq!(
            child.lock().unwrap().try_wait().unwrap().unwrap().code(),
            Some(7)
        );
    }
}
