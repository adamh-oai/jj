//! Direct Git fsmonitor hook-v2 protocol over the synchronized facade.

use crate::bser::{decode_frame, encode_frame, Limits, Value};
use crate::compat::ClientFlavor;
use crate::facade::FacadeService;
use crate::watchman_transport::CredentialedStream;
use std::collections::BTreeMap;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const MAX_GIT_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub fn run_hook_v2(
    facade: &mut FacadeService,
    watch_id: [u8; 16],
    argv: &[Vec<u8>],
    requester_uid: u32,
    requester_gid: u32,
    now_ns: i64,
) -> Result<Vec<u8>, GitFsmonitorError> {
    if argv.len() != 2 || argv[0] != b"2" {
        return Err(GitFsmonitorError::new(
            "git fsmonitor hook requires protocol version 2 and an old token",
        ));
    }
    let old_clock = if argv[1].is_empty() || argv[1].iter().all(u8::is_ascii_digit) {
        None
    } else {
        std::str::from_utf8(&argv[1]).ok()
    };
    let result = facade
        .query(
            watch_id,
            old_clock,
            ClientFlavor::Git,
            requester_uid,
            requester_gid,
            now_ns,
        )
        .map_err(|error| GitFsmonitorError::context("run Git fsmonitor query", error))?;
    encode_response(&result.clock, &result.projection.paths)
}

pub fn run_hook_over_socket(
    socket: &Path,
    root: &Path,
    argv: &[Vec<u8>],
) -> Result<Vec<u8>, GitFsmonitorError> {
    if argv.len() != 2 || argv[0] != b"2" {
        return Err(GitFsmonitorError::new(
            "git fsmonitor hook requires protocol version 2 and an old token",
        ));
    }
    let stream = UnixStream::connect(socket)
        .map_err(|error| GitFsmonitorError::context("connect to btrfs-awacs daemon", error))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(60))))
        .map_err(|error| GitFsmonitorError::context("set daemon socket deadline", error))?;
    let mut stream = CredentialedStream::new(stream)
        .map_err(|error| GitFsmonitorError::context("authenticate daemon transport", error))?;
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| GitFsmonitorError::context("canonicalize Git worktree", error))?;
    let mut options = BTreeMap::from([
        (b"expression".to_vec(), git_expression()),
        (
            b"fields".to_vec(),
            Value::Array(vec![Value::Bytes(b"name".to_vec())]),
        ),
        (b"sync_timeout".to_vec(), Value::Integer(60_000)),
    ]);
    if !argv[1].is_empty() && !argv[1].iter().all(u8::is_ascii_digit) {
        options.insert(b"since".to_vec(), Value::Bytes(argv[1].clone()));
    }
    let request = Value::Array(vec![
        Value::Bytes(b"query".to_vec()),
        Value::Bytes(canonical_root.as_os_str().as_bytes().to_vec()),
        Value::Object(options),
    ]);
    let limits = Limits::default();
    let frame = encode_frame(&request, limits)
        .map_err(|error| GitFsmonitorError::context("encode daemon query", error))?;
    stream
        .send_frame(&frame, limits)
        .map_err(|error| GitFsmonitorError::context("send daemon query", error))?;
    let response = stream
        .receive_frame(limits)
        .map_err(|error| GitFsmonitorError::context("receive daemon query", error))?;
    let response = decode_frame(&response.bytes, limits)
        .map_err(|error| GitFsmonitorError::context("decode daemon response", error))?;
    let Value::Object(response) = response else {
        return Err(GitFsmonitorError::new("daemon response is not an object"));
    };
    if let Some(Value::Bytes(error)) = response.get(b"error".as_slice()) {
        return Err(GitFsmonitorError::new(format!(
            "daemon query failed: {}",
            String::from_utf8_lossy(error)
        )));
    }
    let Value::Bytes(clock) = response
        .get(b"clock".as_slice())
        .ok_or_else(|| GitFsmonitorError::new("daemon response omitted its clock"))?
    else {
        return Err(GitFsmonitorError::new("daemon clock is not a string"));
    };
    let clock = std::str::from_utf8(clock)
        .map_err(|_| GitFsmonitorError::new("daemon clock is not ASCII"))?;
    let Value::Array(files) = response
        .get(b"files".as_slice())
        .ok_or_else(|| GitFsmonitorError::new("daemon response omitted its files"))?
    else {
        return Err(GitFsmonitorError::new("daemon files are not an array"));
    };
    let paths = files
        .iter()
        .map(|file| match file {
            Value::Bytes(path) => Ok(path.clone()),
            _ => Err(GitFsmonitorError::new(
                "daemon returned a non-string Git path",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    encode_response(clock, &paths)
}

fn git_expression() -> Value {
    Value::Array(vec![
        Value::Bytes(b"not".to_vec()),
        Value::Array(vec![
            Value::Bytes(b"dirname".to_vec()),
            Value::Bytes(b".git".to_vec()),
        ]),
    ])
}

pub fn encode_response(clock: &str, paths: &[Vec<u8>]) -> Result<Vec<u8>, GitFsmonitorError> {
    if clock.is_empty() || clock.as_bytes().contains(&0) {
        return Err(GitFsmonitorError::new("Git fsmonitor clock is invalid"));
    }
    let response_bytes = paths.iter().try_fold(clock.len() + 1, |total, path| {
        total.checked_add(path.len())?.checked_add(1)
    });
    let response_bytes = response_bytes
        .filter(|bytes| *bytes <= MAX_GIT_RESPONSE_BYTES)
        .ok_or_else(|| GitFsmonitorError::new("Git fsmonitor response exceeds its byte limit"))?;
    let mut output = Vec::with_capacity(response_bytes);
    output.extend_from_slice(clock.as_bytes());
    output.push(0);
    for path in paths {
        let fresh_sentinel = path == b"/";
        let normal = path.strip_suffix(b"/").unwrap_or(path);
        if path.is_empty() || path.contains(&0) || (!fresh_sentinel && invalid_relative(normal)) {
            return Err(GitFsmonitorError::new("Git fsmonitor path is invalid"));
        }
        output.extend_from_slice(path);
        output.push(0);
    }
    Ok(output)
}

fn invalid_relative(path: &[u8]) -> bool {
    path.is_empty()
        || path.starts_with(b"/")
        || path
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
}

#[derive(Debug)]
pub struct GitFsmonitorError {
    message: String,
}

impl GitFsmonitorError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn context(context: impl fmt::Display, error: impl fmt::Display) -> Self {
        Self::new(format!("{context}: {error}"))
    }
}

impl fmt::Display for GitFsmonitorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GitFsmonitorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_is_nul_safe_and_preserves_raw_names_and_prefixes() {
        let response = encode_response(
            "c:btrfs-awacs:1:token",
            &[vec![0xff, b'x'], b"directory/".to_vec()],
        )
        .unwrap();
        assert_eq!(
            response,
            b"c:btrfs-awacs:1:token\0\xffx\0directory/\0".to_vec()
        );
        assert!(encode_response("clock", &[b"../escape".to_vec()]).is_err());
        assert!(encode_response("clock", &[vec![b'x'; MAX_GIT_RESPONSE_BYTES]]).is_err());
    }
}
