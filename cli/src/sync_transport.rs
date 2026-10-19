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

//! Local and SSH subprocesses carrying the same framed synchronization stream.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::io::BufReader;
use std::io::Read as _;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStderr;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use jj_lib::sync::protocol::Message;
use jj_lib::sync::protocol::ProtocolError;
use jj_lib::sync::protocol::Session;
use thiserror::Error;

use crate::ui::Ui;

const MAX_STDERR_BYTES: usize = 64 * 1024;

/// An existing peer repository, reached locally or with OpenSSH.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Endpoint {
    Local(PathBuf),
    Ssh {
        destination: String,
        port: Option<u16>,
        repository_path: Vec<u8>,
    },
}

impl Endpoint {
    /// Accepts filesystem paths and explicit `ssh://[user@]host[:port]/path` URLs.
    /// SCP-like strings remain ordinary local paths, avoiding ambiguous colons.
    pub(crate) fn parse(value: &OsStr) -> Result<Self, TransportError> {
        if let Some(url) = value
            .to_str()
            .and_then(|value| value.strip_prefix("ssh://"))
        {
            Self::parse_ssh(url)
        } else {
            if value.is_empty() {
                return Err(endpoint_error("repository path is empty"));
            }
            Ok(Self::Local(std::path::absolute(Path::new(value))?))
        }
    }

    fn parse_ssh(url: &str) -> Result<Self, TransportError> {
        let (authority, path) = url
            .split_once('/')
            .ok_or_else(|| endpoint_error("SSH URL must include an absolute repository path"))?;
        if authority.is_empty() || path.is_empty() {
            return Err(endpoint_error(
                "SSH URL must include a host and repository path",
            ));
        }
        if path.contains(['?', '#']) {
            return Err(endpoint_error(
                "encode literal '?' and '#' in SSH repository paths as %3F and %23",
            ));
        }
        let (user, host_port) = match authority.rsplit_once('@') {
            Some((user, host)) => {
                if user.is_empty()
                    || user.starts_with('-')
                    || !user.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                    })
                {
                    return Err(endpoint_error("invalid SSH user name"));
                }
                (Some(user), host)
            }
            None => (None, authority),
        };
        let (host, port_text) = if let Some(ipv6) = host_port.strip_prefix('[') {
            let (host, suffix) = ipv6
                .split_once(']')
                .ok_or_else(|| endpoint_error("unclosed IPv6 address in SSH URL"))?;
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(endpoint_error("invalid IPv6 address in SSH URL"));
            }
            let port = if suffix.is_empty() {
                None
            } else {
                Some(
                    suffix
                        .strip_prefix(':')
                        .ok_or_else(|| endpoint_error("unexpected text after IPv6 address"))?,
                )
            };
            (host, port)
        } else {
            let (host, port) = match host_port.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (host_port, None),
            };
            if host.is_empty()
                || host.starts_with('-')
                || !host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
            {
                return Err(endpoint_error("invalid host in SSH URL"));
            }
            (host, port)
        };
        let port = port_text
            .map(|text| {
                text.parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| endpoint_error("SSH port must be between 1 and 65535"))
            })
            .transpose()?;
        validate_percent_escapes(path)?;
        let mut repository_path = vec![b'/'];
        repository_path.extend(percent_encoding::percent_decode_str(path));
        if repository_path.contains(&0) {
            return Err(endpoint_error("repository path contains a NUL byte"));
        }
        let destination = match user {
            Some(user) => format!("{user}@{host}"),
            None => host.to_owned(),
        };
        Ok(Self::Ssh {
            destination,
            port,
            repository_path,
        })
    }

    /// Path bytes sent as data in `Hello`, never as command-line arguments.
    pub(crate) fn repository_path(&self) -> Vec<u8> {
        match self {
            Self::Local(path) => encode_path(path),
            Self::Ssh {
                repository_path, ..
            } => repository_path.clone(),
        }
    }

    fn command(
        &self,
        local_executable: &Path,
        ssh_executable: &OsStr,
        debug: bool,
        quiet: bool,
    ) -> Command {
        match self {
            Self::Local(_) => {
                let mut command = Command::new(local_executable);
                if debug {
                    command.arg("--debug");
                }
                if quiet {
                    command.arg("--quiet");
                }
                command.args(["util", "sync-server"]);
                command
            }
            Self::Ssh {
                destination, port, ..
            } => {
                let mut command = Command::new(ssh_executable);
                command.arg("-T");
                if let Some(port) = port {
                    command.arg("-p").arg(port.to_string());
                }
                // Only this constant is interpreted by the remote shell. In
                // particular, the repository path never enters shell syntax.
                command
                    .arg("--")
                    .arg(destination)
                    .arg(match (debug, quiet) {
                        (false, false) => "jj util sync-server",
                        (true, false) => "jj --debug util sync-server",
                        (false, true) => "jj --quiet util sync-server",
                        (true, true) => "jj --debug --quiet util sync-server",
                    });
                command
            }
        }
    }
}

/// Decodes the helper path from its initial protocol message.
pub(crate) fn decode_repository_path(bytes: &[u8]) -> Result<PathBuf, TransportError> {
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(endpoint_error(
            "repository path is empty or contains a NUL byte",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Ok(OsString::from_vec(bytes.to_vec()).into())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes.to_vec())
            .map(PathBuf::from)
            .map_err(|_| endpoint_error("repository path is not valid UTF-8 on this platform"))
    }
}

fn encode_path(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.as_os_str().as_encoded_bytes().to_vec()
    }
}

fn validate_percent_escapes(path: &str) -> Result<(), TransportError> {
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && (!bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(endpoint_error(
                "invalid percent escape in SSH repository path",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum TransportError {
    #[error("Invalid synchronization endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("Failed to start synchronization helper {program:?}: {source}")]
    Spawn {
        program: OsString,
        #[source]
        source: io::Error,
    },
    #[error("Failed to access synchronization helper: {0}")]
    Io(#[from] io::Error),
    #[error("{source}{diagnostic}")]
    Protocol {
        #[source]
        source: ProtocolError,
        diagnostic: String,
    },
    #[error("Synchronization helper exited with {status}{diagnostic}")]
    HelperFailed {
        status: ExitStatus,
        diagnostic: String,
    },
}

/// A helper process and its protocol stream. Dropping it cancels and reaps it.
/// Stderr is always drained concurrently, independently of protocol stdout.
pub(crate) struct Transport {
    child: Child,
    session: Option<Session<BufReader<ChildStdout>, ChildStdin>>,
    stderr: Arc<Mutex<StderrCapture>>,
    stderr_stop: Arc<AtomicBool>,
    stderr_thread: Option<JoinHandle<io::Result<()>>>,
    stderr_streamed: bool,
    finished: bool,
}

impl Transport {
    pub(crate) fn connect(
        endpoint: &Endpoint,
        debug: bool,
        ui: &Ui,
    ) -> Result<Self, TransportError> {
        let executable = std::env::current_exe()?;
        Self::spawn(
            endpoint.command(&executable, OsStr::new("ssh"), debug, ui.is_quiet()),
            Some(ui.stderr_writer()?),
            ui.use_progress_indicator(),
            debug,
        )
    }

    fn spawn(
        mut command: Command,
        stderr_output: Option<Box<dyn Write + Send>>,
        clear_progress: bool,
        debug: bool,
    ) -> Result<Self, TransportError> {
        tracing::debug!(
            program = ?command.get_program(),
            arguments = ?command.get_args().collect::<Vec<_>>(),
            "Starting synchronization helper"
        );
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Cancel any git/helper descendants as well as jj itself. Authentication
        // and host verification are still entirely OpenSSH's normal behavior.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|source| TransportError::Spawn {
            program: command.get_program().to_owned(),
            source,
        })?;
        tracing::debug!(pid = child.id(), "Synchronization helper started");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let child_stderr = child.stderr.take().unwrap();
        if let Err(error) = configure_stderr(&child_stderr) {
            terminate(&mut child);
            return Err(error.into());
        }
        let stderr = Arc::new(Mutex::new(StderrCapture::default()));
        let stderr_stop = Arc::new(AtomicBool::new(false));
        let stderr_writer = stderr.clone();
        let stderr_reader_stop = stderr_stop.clone();
        let stderr_streamed = stderr_output.is_some() || debug;
        let stderr_thread = thread::Builder::new()
            .name("jj-sync-stderr".to_owned())
            .spawn(move || {
                drain_stderr(
                    child_stderr,
                    &stderr_writer,
                    &stderr_reader_stop,
                    stderr_output,
                    clear_progress,
                    debug,
                )
            });
        let stderr_thread = match stderr_thread {
            Ok(thread) => thread,
            Err(err) => {
                terminate(&mut child);
                return Err(err.into());
            }
        };
        Ok(Self {
            child,
            session: Some(Session::new(BufReader::new(stdout), stdin)),
            stderr,
            stderr_stop,
            stderr_thread: Some(stderr_thread),
            stderr_streamed,
            finished: false,
        })
    }

    pub(crate) fn send(&mut self, message: &Message) -> Result<(), TransportError> {
        let result = self.active_session()?.send(message);
        result.map_err(|error| self.protocol_failure(error))
    }

    /// Borrows the framed connection for an explicitly negotiated duplex phase.
    /// The caller returns both streams before continuing ordinary commands.
    pub(crate) fn framed_io_mut(
        &mut self,
    ) -> Result<(&mut BufReader<ChildStdout>, &mut ChildStdin), TransportError> {
        Ok(self.active_session()?.split_mut())
    }

    pub(crate) fn receive(&mut self) -> Result<Message, TransportError> {
        match self.active_session()?.receive() {
            Ok(Message::Error(error)) => {
                // A framed peer error terminates the session just like a
                // broken stream. Preserve its stderr before callers return
                // early and drop the transport, including during negotiation.
                self.cancel();
                Ok(Message::Error(format!(
                    "{error}{}",
                    self.stderr_diagnostic()
                )))
            }
            Ok(message) => Ok(message),
            Err(error) => Err(self.protocol_failure(error)),
        }
    }

    /// Closes the protocol streams, waits for the helper, and checks its status.
    /// Returns bounded helper diagnostics if they weren't already streamed.
    /// The command must consume its final response before calling this method.
    pub(crate) fn finish(mut self) -> Result<String, TransportError> {
        drop(self.session.take());
        let status = self.child.wait()?;
        if !status.success() {
            // The helper may have exited while its children still hold pipes
            // or mutate the repository. Failure cancels the whole group.
            terminate(&mut self.child);
        }
        self.finished = true;
        self.join_stderr()?;
        let diagnostic = self.stderr_diagnostic();
        if status.success() {
            Ok(if self.stderr_streamed {
                String::new()
            } else {
                diagnostic
            })
        } else {
            Err(TransportError::HelperFailed { status, diagnostic })
        }
    }

    /// Cancels the helper and any subprocesses, then reaps it.
    pub(crate) fn cancel(&mut self) {
        if !self.finished {
            drop(self.session.take());
            terminate(&mut self.child);
            self.finished = true;
            // A descendant could have left the process group while retaining
            // stderr. The cancellable drainer does not depend on pipe EOF.
            drop(self.join_stderr());
        }
    }

    fn active_session(
        &mut self,
    ) -> Result<&mut Session<BufReader<ChildStdout>, ChildStdin>, TransportError> {
        self.session.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "synchronization session is closed",
            )
            .into()
        })
    }

    fn protocol_failure(&mut self, source: ProtocolError) -> TransportError {
        self.cancel();
        TransportError::Protocol {
            source,
            diagnostic: self.stderr_diagnostic(),
        }
    }

    fn join_stderr(&mut self) -> io::Result<()> {
        self.stderr_stop.store(true, Ordering::Release);
        match self.stderr_thread.take() {
            Some(thread) => {
                thread.thread().unpark();
                #[cfg(windows)]
                while !thread.is_finished() {
                    use std::os::windows::io::AsRawHandle as _;

                    use windows_sys::Win32::System::IO::CancelSynchronousIo;

                    // SAFETY: the JoinHandle keeps this thread handle live.
                    // A synchronous PeekNamedPipe can itself block; repeat
                    // cancellation to cover a read starting after shutdown.
                    let _cancel_result = unsafe { CancelSynchronousIo(thread.as_raw_handle()) };
                    thread.thread().unpark();
                    thread::sleep(Duration::from_millis(10));
                }
                thread
                    .join()
                    .map_err(|_| io::Error::other("synchronization stderr reader panicked"))?
            }
            None => Ok(()),
        }
    }

    fn stderr_diagnostic(&self) -> String {
        let stderr = self.stderr.lock().unwrap();
        if stderr.bytes.is_empty() {
            return String::new();
        }
        let prefix = if stderr.truncated {
            "\nHelper stderr (last 64 KiB):\n"
        } else {
            "\nHelper stderr:\n"
        };
        format!(
            "{prefix}{}",
            String::from_utf8_lossy(&stderr.bytes).trim_end()
        )
    }
}

/// Drains stderr concurrently without waiting forever for inherited descriptors.
/// After shutdown, collect the available bounded tail and close our pipe even
/// if a helper descendant continues writing or never closes its copy. Displaying
/// output can block on the UI destination, as with other command output.
fn drain_stderr(
    mut stderr: ChildStderr,
    capture: &Mutex<StderrCapture>,
    stop: &AtomicBool,
    mut output: Option<Box<dyn Write + Send>>,
    clear_progress: bool,
    debug: bool,
) -> io::Result<()> {
    let mut buffer = [0; 8192];
    let mut drained_after_stop = 0;
    let mut at_line_start = true;
    loop {
        match read_stderr(&mut stderr, &mut buffer) {
            Ok(0) => return Ok(()),
            Ok(length) => {
                capture.lock().unwrap().append(&buffer[..length]);
                // Preserve peer diagnostics in the initiator's tracing log.
                // Explicit --debug already displays these events on stderr.
                tracing::debug!(
                    helper_stderr = %String::from_utf8_lossy(&buffer[..length]),
                    "Synchronization helper stderr"
                );
                if let Some(output) = &mut output
                    && !debug
                {
                    // Display progress immediately. Failure to display it must
                    // not interrupt the protocol or stop draining diagnostics.
                    let _display_result = if clear_progress && at_line_start {
                        // Write the clear sequence and message together so a
                        // local transfer counter cannot appear between them.
                        let display = [b"\r\x1b[2K".as_slice(), &buffer[..length]].concat();
                        output.write_all(&display)
                    } else {
                        output.write_all(&buffer[..length])
                    }
                    .and_then(|()| output.flush());
                }
                at_line_start = buffer[length - 1] == b'\n';
                if stop.load(Ordering::Acquire) {
                    drained_after_stop += length;
                    if drained_after_stop >= MAX_STDERR_BYTES {
                        return Ok(());
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                thread::park_timeout(Duration::from_millis(10));
            }
            #[cfg(windows)]
            Err(err) if stop.load(Ordering::Acquire) && err.raw_os_error() == Some(995) => {
                // ERROR_OPERATION_ABORTED is expected after CancelSynchronousIo.
                return Ok(());
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(unix)]
fn configure_stderr(stderr: &ChildStderr) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let descriptor = stderr.as_raw_fd();
    // SAFETY: ChildStderr owns the live descriptor throughout these calls.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL modifies only this pipe's read-side status flags.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn configure_stderr(_stderr: &ChildStderr) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn read_stderr(stderr: &mut ChildStderr, buffer: &mut [u8]) -> io::Result<usize> {
    stderr.read(buffer)
}

#[cfg(windows)]
fn read_stderr(stderr: &mut ChildStderr, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let mut available = 0;
    // SAFETY: ChildStderr owns this anonymous pipe, which supports
    // PeekNamedPipe. The only non-null output points to `available`.
    let result = unsafe {
        PeekNamedPipe(
            stderr.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        let error = io::Error::last_os_error();
        // ERROR_BROKEN_PIPE / ERROR_NO_DATA indicate that the writer closed.
        return if matches!(error.raw_os_error(), Some(109 | 232)) {
            Ok(0)
        } else {
            Err(error)
        };
    }
    if available == 0 {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    // This thread is the only reader. Reading no more than the available
    // bytes avoids an uncancellable blocking ReadFile call.
    let length = buffer.len().min(available as usize);
    stderr.read(&mut buffer[..length])
}

#[cfg(not(any(unix, windows)))]
fn read_stderr(stderr: &mut ChildStderr, buffer: &mut [u8]) -> io::Result<usize> {
    stderr.read(buffer)
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn terminate(child: &mut Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::Signal;
        use nix::sys::signal::killpg;
        use nix::unistd::Pid;
        // The child started in its own group; no caller processes are in it.
        let _kill_group_result = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
    }
    let _kill_result = child.kill();
    let _wait_result = child.wait();
}

#[derive(Default)]
struct StderrCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl StderrCapture {
    fn append(&mut self, bytes: &[u8]) {
        if bytes.len() >= MAX_STDERR_BYTES {
            self.truncated |= !self.bytes.is_empty() || bytes.len() > MAX_STDERR_BYTES;
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&bytes[bytes.len() - MAX_STDERR_BYTES..]);
        } else {
            let excess = (self.bytes.len() + bytes.len()).saturating_sub(MAX_STDERR_BYTES);
            if excess > 0 {
                self.bytes.drain(..excess);
                self.truncated = true;
            }
            self.bytes.extend_from_slice(bytes);
        }
    }
}

fn endpoint_error(message: impl Into<String>) -> TransportError {
    TransportError::InvalidEndpoint(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    struct ChannelWriter(std::sync::mpsc::Sender<Vec<u8>>);

    #[cfg(unix)]
    impl Write for ChannelWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .send(bytes.to_vec())
                .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    struct ProcessGroupCleanup(u32);

    #[cfg(unix)]
    impl Drop for ProcessGroupCleanup {
        fn drop(&mut self) {
            use nix::sys::signal::Signal;
            use nix::sys::signal::killpg;
            use nix::unistd::Pid;
            let _kill_group_result = killpg(Pid::from_raw(self.0 as i32), Signal::SIGKILL);
        }
    }

    #[test]
    fn local_paths_are_literal_and_absolute() {
        for text in ["-repo", "a space/';$(literal)", "user@host:repo"] {
            let endpoint = Endpoint::parse(OsStr::new(text)).unwrap();
            let Endpoint::Local(path) = &endpoint else {
                panic!("local path treated as SSH: {text}");
            };
            assert!(path.is_absolute());
            assert!(path.ends_with(text));
            assert_eq!(
                decode_repository_path(&endpoint.repository_path()).unwrap(),
                *path
            );
            let command = endpoint.command(Path::new("my jj"), OsStr::new("ssh"), false, false);
            assert_eq!(command.get_program(), "my jj");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["util", "sync-server"]
            );
        }
    }

    #[test]
    fn ssh_repository_path_never_enters_the_shell_command() {
        let endpoint = Endpoint::parse(OsStr::new(
            "ssh://person@example.org:2202/a%20space/'%3B%24(literal)/-repo%23%3F",
        ))
        .unwrap();
        assert_eq!(endpoint.repository_path(), b"/a space/';$(literal)/-repo#?");
        let command = endpoint.command(Path::new("jj"), OsStr::new("ssh"), false, false);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "-T",
                "-p",
                "2202",
                "--",
                "person@example.org",
                "jj util sync-server"
            ]
        );
    }

    #[test]
    fn ssh_ipv6_and_default_port() {
        let endpoint = Endpoint::parse(OsStr::new("ssh://person@[::1]/repo")).unwrap();
        let command = endpoint.command(Path::new("jj"), OsStr::new("ssh"), false, false);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-T", "--", "person@::1", "jj util sync-server"]
        );
    }

    #[test]
    fn debug_logging_is_enabled_for_local_and_ssh_helpers() {
        let endpoint = Endpoint::parse(OsStr::new("local-repo")).unwrap();
        let command = endpoint.command(Path::new("my jj"), OsStr::new("ssh"), true, false);
        assert_eq!(command.get_program(), "my jj");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--debug", "util", "sync-server"]
        );

        let endpoint = Endpoint::parse(OsStr::new(
            "ssh://person@example.org:2202/a%20space/'%3B%24(literal)/-repo%23%3F",
        ))
        .unwrap();
        let command = endpoint.command(Path::new("jj"), OsStr::new("ssh"), true, false);
        assert_eq!(command.get_program(), "ssh");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "-T",
                "-p",
                "2202",
                "--",
                "person@example.org",
                "jj --debug util sync-server"
            ]
        );
    }

    #[test]
    fn quiet_is_propagated_to_local_and_ssh_helpers() {
        for debug in [false, true] {
            let endpoint = Endpoint::parse(OsStr::new("local-repo")).unwrap();
            let command = endpoint.command(Path::new("jj"), OsStr::new("ssh"), debug, true);
            let expected = if debug {
                vec!["--debug", "--quiet", "util", "sync-server"]
            } else {
                vec!["--quiet", "util", "sync-server"]
            };
            assert_eq!(command.get_args().collect::<Vec<_>>(), expected);

            let endpoint = Endpoint::parse(OsStr::new("ssh://example.org/repo")).unwrap();
            let command = endpoint.command(Path::new("jj"), OsStr::new("ssh"), debug, true);
            let remote_command = if debug {
                "jj --debug --quiet util sync-server"
            } else {
                "jj --quiet util sync-server"
            };
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["-T", "--", "example.org", remote_command]
            );
        }
    }

    #[test]
    fn ssh_rejects_options_and_malformed_urls() {
        for value in [
            "ssh://-oProxyCommand=bad/repo",
            "ssh://-user@host/repo",
            "ssh://user:password@host/repo",
            "ssh://user@@host/repo",
            "ssh://host:0/repo",
            "ssh://host:65536/repo",
            "ssh://host:not-a-port/repo",
            "ssh://[not-ipv6]/repo",
            "ssh://[::1]junk/repo",
            "ssh://host",
            "ssh:///repo",
            "ssh://host/",
            "ssh://host/repo%",
            "ssh://host/repo%2",
            "ssh://host/repo%ZZ",
            "ssh://host/repo%00",
            "ssh://host/repo?query",
            "ssh://host/repo#fragment",
        ] {
            assert!(
                Endpoint::parse(OsStr::new(value)).is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn stderr_capture_retains_a_bounded_tail() {
        let mut capture = StderrCapture::default();
        capture.append(&vec![b'a'; MAX_STDERR_BYTES]);
        assert!(!capture.truncated);
        capture.append(b"useful final diagnostic");
        assert_eq!(capture.bytes.len(), MAX_STDERR_BYTES);
        assert!(capture.bytes.ends_with(b"useful final diagnostic"));
        assert!(capture.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_local_paths_round_trip() {
        use std::os::unix::ffi::OsStringExt as _;
        let path = OsString::from_vec(b"/repo/\xff".to_vec());
        let endpoint = Endpoint::parse(&path).unwrap();
        assert_eq!(
            decode_repository_path(&endpoint.repository_path()).unwrap(),
            Path::new(&path)
        );
    }

    #[cfg(unix)]
    #[test]
    fn helper_stderr_is_drained_and_reported_on_stream_failure() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "i=0; while [ $i -lt 10000 ]; do echo diagnostic >&2; i=$((i+1)); done; echo 'helper \
             unavailable' >&2; exit 127",
        ]);
        let mut transport =
            Transport::spawn(command, Some(Box::new(io::sink())), false, false).unwrap();
        let error = transport.receive().unwrap_err().to_string();
        assert!(error.contains("helper unavailable"), "{error}");
        assert!(error.contains("last 64 KiB"), "{error}");
        assert!(transport.child.try_wait().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn framed_peer_errors_preserve_stderr_and_cancel_the_helper() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo 'peer initialization diagnostic' >&2; exec cat"]);
        let mut transport = Transport::spawn(command, None, false, false).unwrap();
        // The helper echoes the frame but remains blocked on stdin. Receiving
        // a terminal error must cancel it without waiting for pipe EOF.
        transport
            .send(&Message::Error("peer rejected synchronization".to_owned()))
            .unwrap();
        let Message::Error(error) = transport.receive().unwrap() else {
            panic!("expected a framed peer error");
        };
        assert!(error.contains("peer rejected synchronization"), "{error}");
        assert!(error.contains("peer initialization diagnostic"), "{error}");
        assert!(error.contains("Helper stderr:"), "{error}");
        assert!(transport.finished);
        assert!(transport.child.try_wait().unwrap().is_some());
        assert!(transport.stderr_thread.is_none());
        assert!(transport.receive().is_err());
        transport.cancel();
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_reaps_the_helper() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let mut transport = Transport::spawn(command, None, false, false).unwrap();
        transport.cancel();
        assert!(transport.child.try_wait().unwrap().is_some());
        assert!(transport.stderr_thread.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_closes_pipes_inherited_by_helper_descendants() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & printf ready; wait"]);
        let mut transport = Transport::spawn(command, None, false, false).unwrap();
        let (mut reader, writer) = transport.session.take().unwrap().into_inner();
        let mut ready = [0; 5];
        reader.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        transport.session = Some(Session::new(reader, writer));
        // The sleep process owns the stderr pipe too. Cancellation must kill
        // the group before joining the stderr drainer.
        transport.cancel();
        assert!(transport.child.try_wait().unwrap().is_some());
        assert!(transport.stderr_thread.is_none());
    }

    #[test]
    fn missing_helper_reports_the_executable() {
        let directory = tempfile::tempdir().unwrap();
        let command = Command::new(directory.path().join("missing-jj-sync-helper"));
        let error = match Transport::spawn(command, None, false, false) {
            Ok(_) => panic!("started a nonexistent helper"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("missing-jj-sync-helper"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn helper_progress_is_displayed_before_it_finishes_without_replay() {
        use std::sync::mpsc;

        let (sender, receiver) = mpsc::channel();
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'Peer: Reading operation history...\\n' >&2; IFS= read -r ready",
        ]);
        let mut transport =
            Transport::spawn(command, Some(Box::new(ChannelWriter(sender))), false, false).unwrap();
        let progress = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("helper progress was buffered until completion");
        assert_eq!(progress, b"Peer: Reading operation history...\n");
        assert!(transport.child.try_wait().unwrap().is_none());
        // The helper only exits when explicitly released after its progress was
        // observed, so this checks live delivery without relying on a sleep.
        transport
            .framed_io_mut()
            .unwrap()
            .1
            .write_all(b"\n")
            .unwrap();
        assert!(transport.finish().unwrap().is_empty());
        assert!(receiver.try_recv().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fragmented_helper_lines_are_not_erased_by_progress_clearing() {
        use std::sync::mpsc;

        let (sender, receiver) = mpsc::channel();
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'Peer: Reading' >&2; IFS= read -r ready; printf ' operation history...\\n' \
             >&2; IFS= read -r ready; printf 'Peer: Applying history...\\n' >&2; IFS= read -r \
             ready",
        ]);
        let mut transport =
            Transport::spawn(command, Some(Box::new(ChannelWriter(sender))), true, false).unwrap();
        for expected in [
            b"\r\x1b[2KPeer: Reading".as_slice(),
            b" operation history...\n",
            b"\r\x1b[2KPeer: Applying history...\n",
        ] {
            let displayed = receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("helper progress was buffered until completion");
            assert_eq!(displayed, expected);
            transport
                .framed_io_mut()
                .unwrap()
                .1
                .write_all(b"\n")
                .unwrap();
        }
        assert!(transport.finish().unwrap().is_empty());
        assert!(receiver.try_recv().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn finish_reports_status_and_successful_diagnostics() {
        use std::os::fd::AsRawFd as _;
        use std::time::Instant;

        let mut command = Command::new("sh");
        command.args(["-c", "exit 0"]);
        assert!(
            Transport::spawn(command, None, false, false)
                .unwrap()
                .finish()
                .unwrap()
                .is_empty()
        );
        let mut command = Command::new("sh");
        command.args(["-c", "echo 'peer snapshot warning' >&2; exit 0"]);
        let diagnostic = Transport::spawn(command, None, false, false)
            .unwrap()
            .finish()
            .unwrap();
        assert!(diagnostic.contains("peer snapshot warning"), "{diagnostic}");

        let mut command = Command::new("sh");
        command.args([
            "-c",
            "sleep 30 & echo 'helper cannot open repository' >&2; exit 2",
        ]);
        let mut transport = Transport::spawn(command, None, false, false).unwrap();
        let _cleanup = ProcessGroupCleanup(transport.child.id());
        // Retain stdout to prove the failed helper's child is terminated:
        // that child holds the only remaining writer after the shell exits.
        let (mut stdout, stdin) = transport.session.take().unwrap().into_inner();
        drop(stdin);
        let error = transport.finish().unwrap_err().to_string();
        assert!(error.contains("helper cannot open repository"), "{error}");
        let descriptor = stdout.get_ref().as_raw_fd();
        // SAFETY: stdout owns this live pipe descriptor throughout the test.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        assert!(flags >= 0);
        // SAFETY: F_SETFL changes only this pipe's read-side status flags.
        assert!(unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match stdout.read(&mut [0; 1]) {
                Ok(0) => break,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "failed helper child survived");
                    thread::sleep(Duration::from_millis(10));
                }
                result => panic!("unexpected helper stdout: {result:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn successful_finish_does_not_wait_for_a_descendants_stderr() {
        use std::time::Instant;

        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & echo 'helper completed' >&2; exit 0"]);
        let transport = Transport::spawn(command, None, false, false).unwrap();
        let _cleanup = ProcessGroupCleanup(transport.child.id());
        let start = Instant::now();
        let diagnostic = transport.finish().unwrap();
        assert!(diagnostic.contains("helper completed"), "{diagnostic}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "finish waited for the descendant to close stderr"
        );
    }
}
