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
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;

use crate::command_error::CommandError;
use crate::command_error::user_error_with_message;
use crate::sync_process::ProcessChild;
use crate::sync_process::ProcessError;
use crate::sync_process::ProcessInput;
use crate::sync_process::ProcessTransferReport;
use crate::sync_process::accept_bridge;
use crate::sync_process::relay;

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

fn failed(error: ProcessError) -> CommandError {
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
) -> Result<ProcessTransferReport, CommandError> {
    run_client_inner(git, repository, action, max_received_bytes, reader, writer).map_err(failed)
}

fn run_client_inner<R: Read + Send, W: Write>(
    git: &Path,
    repository: &Path,
    action: GitAction,
    max_received_bytes: u64,
    reader: &mut R,
    writer: &mut W,
) -> Result<ProcessTransferReport, ProcessError> {
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
    let process = ProcessChild::spawn(command, "native Git")?;
    let service = match action {
        GitAction::Push => GitService::ReceivePack,
        GitAction::Fetch => GitService::UploadPack,
    };
    let connection = match accept_bridge(&listener, &process, &token, service.marker()) {
        Ok(connection) => connection,
        Err(error) => return Err(process.abort(error)),
    };
    let input = ProcessInput::Socket(connection.try_clone()?);
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
) -> Result<ProcessTransferReport, CommandError> {
    run_server_inner(git, repository, service, max_received_bytes, reader, writer).map_err(failed)
}

fn run_server_inner<R: Read + Send, W: Write>(
    git: &Path,
    repository: &Path,
    service: GitService,
    max_received_bytes: u64,
    reader: &mut R,
    writer: &mut W,
) -> Result<ProcessTransferReport, ProcessError> {
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
    let process = ProcessChild::spawn(command, "native Git")?;
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

/// The only entry point used by Git's local remote-ext subprocess.
pub(crate) fn run_bridge(socket: &Path, token: &str, service: &str) -> io::Result<()> {
    let service = GitService::parse(service)?;
    crate::sync_process::run_bridge(socket, token, service.marker())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use gix::objs::Write as _;

    use super::*;

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
            ProcessChild::spawn(command, "native Git").unwrap()
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
