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

//! Additive immutable-file copying through rsync over jj's existing connection.
//!
//! The receiver first asks rsync for a dry-run missing-file plan. After checking
//! local resource budgets, it requests exactly those files. Rsync starts a local
//! bridge instead of SSH; the peer runs a fixed sender with private local paths.

use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::thread;

use crate::command_error::CommandError;
use crate::command_error::user_error_with_message;
use crate::sync_process::ProcessChild;
use crate::sync_process::ProcessError;
use crate::sync_process::ProcessInput;
use crate::sync_process::ProcessTransferReport;
use crate::sync_process::accept_bridge;
use crate::sync_process::kill_child;
use crate::sync_process::relay;

// Protocol 31 is supported by rsync 3.1 and later. Fixing the protocol keeps
// both locally constructed commands independent of the peer's installed version.
const PROTOCOL_VERSION: u32 = 31;
const RECEIVE_MARKER: u8 = 4;

/// The receiver always drives rsync, even when jj's initiating command sends.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TransferMode<'a> {
    /// Produce `%l %n` lines without creating incoming object files.
    Plan { output: &'a Path, max_bytes: u64 },
    /// Pull only the validated, NUL-separated paths reserved by the receiver.
    Receive {
        files_from: &'a Path,
        max_file_size: u64,
    },
    /// Serve the frozen source's file listing without sending object bodies.
    SendPlan,
    /// Serve only files requested by the receiver's validated files-from list.
    Send,
}

impl TransferMode<'_> {
    fn is_sender(self) -> bool {
        matches!(self, Self::SendPlan | Self::Send)
    }

    fn is_plan(self) -> bool {
        matches!(self, Self::Plan { .. } | Self::SendPlan)
    }
}

fn failed(error: ProcessError) -> CommandError {
    user_error_with_message("Immutable-file synchronization failed", error)
}

/// Only advertise this optional transport when a usable rsync is installed.
pub(crate) fn available(rsync: &Path) -> bool {
    let Ok(output) = rsync_command(rsync).arg("--version").output() else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .and_then(|line| line.rsplit_once("protocol version"))
            .and_then(|(_, protocol)| protocol.trim().parse::<u32>().ok())
            .is_some_and(|protocol| protocol >= PROTOCOL_VERSION)
}

/// Runs either rsync role over the current framed connection, without extra SSH.
pub(crate) fn run<R: Read + Send, W: Write>(
    rsync: &Path,
    directory: &Path,
    basis: Option<&Path>,
    mode: TransferMode<'_>,
    max_received_bytes: u64,
    reader: &mut R,
    writer: &mut W,
) -> Result<ProcessTransferReport, CommandError> {
    if mode.is_sender() {
        run_sender(rsync, directory, mode, max_received_bytes, reader, writer)
    } else {
        run_receiver(
            rsync,
            directory,
            basis,
            mode,
            max_received_bytes,
            reader,
            writer,
        )
    }
    .map_err(failed)
}

fn run_receiver<R: Read + Send, W: Write>(
    rsync: &Path,
    directory: &Path,
    basis: Option<&Path>,
    mode: TransferMode<'_>,
    max_received_bytes: u64,
    reader: &mut R,
    writer: &mut W,
) -> Result<ProcessTransferReport, ProcessError> {
    // The rsh string has rsync's own word-splitting grammar. A private alias
    // gives the executable a short ASCII path, including when jj's real path
    // contains spaces, quotes, shell punctuation, or non-UTF-8 bytes.
    let bridge_directory = tempfile::Builder::new()
        .prefix("jj-rsync-")
        .tempdir_in("/tmp")?;
    let executable = bridge_directory.path().join("jj");
    symlink(std::env::current_exe()?, &executable)?;
    let socket_path = bridge_directory.path().join("s");
    let listener = UnixListener::bind(&socket_path)?;
    listener.set_nonblocking(true)?;
    let token = format!("{:032x}", rand::random::<u128>());
    let rsh = format!(
        "{} util sync-rsync-bridge --socket {} --token {}",
        executable.display(),
        socket_path.display(),
        token,
    );
    let mut command = receiver_command(rsync, directory, basis, mode, &rsh);
    command.stdin(Stdio::null());
    if mode.is_plan() {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::null());
    }
    let process = ProcessChild::spawn(command, "rsync")?;
    let connection = match accept_bridge(&listener, &process, &token, RECEIVE_MARKER) {
        Ok(connection) => connection,
        Err(error) => return Err(process.abort(error)),
    };
    let input = ProcessInput::Socket(connection.try_clone()?);
    let cancel_socket = connection.try_clone()?;
    let capture = if let TransferMode::Plan { output, max_bytes } = mode {
        let mut output = File::create(output)?;
        let mut stdout = process.child.lock().unwrap().stdout.take().unwrap();
        let child = process.child.clone();
        let cancel = connection.try_clone()?;
        Some(
            thread::Builder::new()
                .name("jj-sync-rsync-plan".to_owned())
                .spawn(move || {
                    let result = capture_plan_output(&mut stdout, &mut output, max_bytes);
                    if result.is_err() {
                        kill_child(&child);
                        let _shutdown_result = cancel.shutdown(Shutdown::Both);
                    }
                    result
                })?,
        )
    } else {
        None
    };
    let counts = relay(
        reader,
        writer,
        connection,
        input,
        &process,
        Some(cancel_socket),
        max_received_bytes,
    );
    let captured = capture.map(|thread| {
        thread
            .join()
            .map_err(|_| io::Error::other("rsync plan output reader panicked"))?
    });
    if let Some(Err(error)) = captured {
        return Err(process.abort(error.into()));
    }
    match counts {
        Ok(counts) => process.finish(counts),
        Err(error) => Err(process.abort(error)),
    }
}

fn capture_plan_output(
    reader: &mut impl Read,
    writer: &mut impl Write,
    maximum_bytes: u64,
) -> io::Result<()> {
    let mut total = 0u64;
    let mut buffer = [0; 8192];
    loop {
        let length = match reader.read(&mut buffer) {
            Ok(0) => return writer.flush(),
            Ok(length) => length,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        total = total
            .checked_add(length as u64)
            .ok_or_else(|| io::Error::other("rsync missing-file plan size overflow"))?;
        if total > maximum_bytes {
            return Err(io::Error::other(
                "rsync missing-file plan exceeds the local budget (sync.max-quarantine-bytes or \
                 sync.max-records)",
            ));
        }
        writer.write_all(&buffer[..length])?;
    }
}

fn run_sender<R: Read + Send, W: Write>(
    rsync: &Path,
    directory: &Path,
    mode: TransferMode<'_>,
    max_received_bytes: u64,
    reader: &mut R,
    writer: &mut W,
) -> Result<ProcessTransferReport, ProcessError> {
    let mut command = sender_command(rsync, directory, mode);
    command.stdin(Stdio::piped()).stdout(Stdio::piped());
    let process = ProcessChild::spawn(command, "rsync")?;
    let (input, output) = {
        let mut child = process.child.lock().unwrap();
        (
            ProcessInput::Pipe(child.stdin.take()),
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

fn rsync_command(rsync: &Path) -> Command {
    let mut command = Command::new(rsync);
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"RSYNC_") {
            command.env_remove(name);
        }
    }
    command.stderr(Stdio::piped());
    command
}

fn transfer_options(command: &mut Command) {
    command.args([
        "--whole-file",
        "--ignore-existing",
        "--size-only",
        "--compress",
        "--protocol=31",
    ]);
}

fn receiver_command(
    rsync: &Path,
    directory: &Path,
    basis: Option<&Path>,
    mode: TransferMode<'_>,
    rsh: &str,
) -> Command {
    let mut command = rsync_command(rsync);
    command.current_dir(directory).arg("--recursive");
    transfer_options(&mut command);
    if let Some(basis) = basis {
        // These are private local directories, never paths chosen by a peer.
        let mut option = std::ffi::OsString::from("--compare-dest=");
        option.push(basis);
        command.arg(option);
    }
    match mode {
        TransferMode::Plan { .. } => {
            command.args(["--dry-run", "--out-format=%l %n"]);
        }
        TransferMode::Receive {
            files_from,
            max_file_size,
        } => {
            let mut option = std::ffi::OsString::from("--files-from=");
            option.push(files_from);
            command
                .arg(option)
                .arg("--from0")
                .arg(format!("--max-size={max_file_size}"));
        }
        TransferMode::SendPlan | TransferMode::Send => unreachable!("sender uses a server command"),
    }
    command
        .arg("--rsh")
        .arg(rsh)
        .args(["--", "jj-sync-peer:./", "./"]);
    command
}

fn sender_command(rsync: &Path, directory: &Path, mode: TransferMode<'_>) -> Command {
    let mut command = rsync_command(rsync);
    command
        .current_dir(directory)
        .args(["--server", "--sender"]);
    // Advertise only baseline protocol-31 client capabilities. The fixed
    // server command cannot assume its peer supports newer checksum or
    // compression negotiation; rsync retains the shared zlib fallback.
    command.arg("-re.");
    transfer_options(&mut command);
    if mode.is_plan() {
        command.arg("--dry-run");
    } else {
        command.args(["--files-from=-", "--from0"]);
    }
    command.args([".", "./"]);
    command
}

/// Called only by the receiver's local rsync remote-shell replacement.
pub(crate) fn run_bridge(socket: &Path, token: &str) -> io::Result<()> {
    crate::sync_process::run_bridge(socket, token, RECEIVE_MARKER)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::os::unix::ffi::OsStrExt as _;

    use jj_lib::sync::protocol::Message;
    use jj_lib::sync::protocol::Session;

    use super::*;

    #[test]
    fn commands_keep_private_paths_out_of_remote_shell_arguments() {
        let directory = Path::new("/private/path with 'quotes' and $(punctuation)");
        let basis = Path::new("/private/basis with spaces");
        let files = Path::new("/private/reserved-files");
        for mode in [
            TransferMode::Plan {
                output: files,
                max_bytes: 4096,
            },
            TransferMode::Receive {
                files_from: files,
                max_file_size: 4096,
            },
        ] {
            let client =
                receiver_command(Path::new("rsync"), directory, Some(basis), mode, "bridge");
            assert_eq!(client.get_current_dir(), Some(directory));
            let arguments: Vec<_> = client.get_args().collect();
            assert!(arguments.contains(&std::ffi::OsStr::new("--ignore-existing")));
            assert!(arguments.contains(&std::ffi::OsStr::new("--whole-file")));
            assert!(arguments.contains(&std::ffi::OsStr::new("--compress")));
            assert_eq!(
                arguments.contains(&std::ffi::OsStr::new("--max-size=4096")),
                !mode.is_plan()
            );
            assert!(
                !arguments
                    .iter()
                    .any(|argument| argument.as_bytes().starts_with(b"--delete"))
            );
            let rsh_index = arguments
                .iter()
                .position(|argument| *argument == "--rsh")
                .unwrap();
            assert_eq!(arguments[rsh_index + 1], "bridge");
            assert_eq!(arguments.last().copied(), Some(std::ffi::OsStr::new("./")));
            assert!(
                arguments
                    .iter()
                    .any(|argument| argument.as_bytes().starts_with(b"--compare-dest="))
            );
            let server_mode = if mode.is_plan() {
                TransferMode::SendPlan
            } else {
                TransferMode::Send
            };
            let server = sender_command(Path::new("rsync"), directory, server_mode);
            assert_eq!(server.get_current_dir(), Some(directory));
            assert!(server.get_args().any(|argument| argument == "--sender"));
            assert_eq!(
                server.get_args().any(|argument| argument == "--dry-run"),
                mode.is_plan()
            );
            assert_eq!(
                server
                    .get_args()
                    .any(|argument| argument == "--files-from=-"),
                !mode.is_plan()
            );
        }
    }

    #[test]
    fn plan_output_is_bounded_before_writing_excess_bytes() {
        let body = b"4 operations/a\n3 views/b\n";
        let mut exact = Vec::new();
        capture_plan_output(&mut &body[..], &mut exact, body.len() as u64).unwrap();
        assert_eq!(exact, body);
        let mut rejected = Vec::new();
        let error =
            capture_plan_output(&mut &body[..], &mut rejected, body.len() as u64 - 1).unwrap_err();
        assert!(error.to_string().contains("local budget"));
        assert!(rejected.len() < body.len());
    }

    #[test]
    fn incoming_budget_and_peer_cancellation_stop_the_real_rsync_server() {
        let rsync = Path::new("rsync");
        if !available(rsync) {
            return;
        }
        for (message, budget, expected) in [
            (Message::Record(vec![0; 8]), 4, "max-quarantine-bytes"),
            (
                Message::Error("cancel transfer".to_owned()),
                u64::MAX,
                "cancel transfer",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut incoming = Session::new(io::empty(), Vec::new());
            incoming.send(&message).unwrap();
            let mut reader = Cursor::new(incoming.into_inner().1);
            let mut writer = Vec::new();
            let error = run_sender(
                rsync,
                directory.path(),
                TransferMode::SendPlan,
                budget,
                &mut reader,
                &mut writer,
            )
            .unwrap_err();
            let ProcessError::Transfer { source, .. } = error else {
                panic!("failed rsync transfer must abort and reap its server");
            };
            assert!(source.to_string().contains(expected), "{source}");
            let mut output = Session::new(Cursor::new(writer), io::sink());
            loop {
                match output.receive().unwrap() {
                    Message::Record(_) => {}
                    Message::Error(message) => {
                        assert!(message.contains(expected), "{message}");
                        break;
                    }
                    message => panic!("cancellation must reach the peer, got {message:?}"),
                }
            }
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn invalid_bridge_token_fails_before_connecting() {
        let error =
            run_bridge(Path::new("/does-not-exist/jj-rsync-socket"), "invalid").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
