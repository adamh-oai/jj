// Copyright 2020 The Jujutsu Authors
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

//! Windows file locking implementation using std's `File::lock()` which maps
//! to `LockFileEx`.

use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::io;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::thread;

use tracing::instrument;

use super::FileLockError;
use super::backoff::BackoffIterator;
use super::read_lock_owner;
use super::start_file_lock_wait_notice;
use super::write_lock_owner;

const FILE_SHARE_READ: u32 = 1;
const FILE_SHARE_WRITE: u32 = 2;
const ERROR_SHARING_VIOLATION: u32 = 32;

pub struct FileLock {
    path: PathBuf,
    /// `Option` so `Drop` can close the handle before deleting.
    file: Option<File>,
    process_id: u32,
}

impl FileLock {
    /// Acquire an exclusive lock on `path`, blocking until it's available.
    pub fn lock(path: PathBuf) -> Result<Self, FileLockError> {
        // In blocking mode, `lock_inner` never returns `Ok(None)`.
        Ok(Self::lock_inner(path, true)?.expect("blocking lock should return a lock"))
    }

    /// Try to acquire an exclusive lock on `path` without blocking. Returns
    /// `Ok(None)` if the lock is currently held by another process.
    pub fn try_lock(path: PathBuf) -> Result<Option<Self>, FileLockError> {
        Self::lock_inner(path, false)
    }

    fn lock_inner(path: PathBuf, blocking: bool) -> Result<Option<Self>, FileLockError> {
        let process_id = std::process::id();
        tracing::info!(?path, process_id, "Attempting to lock");

        let mut file = try_create_file(&path).map_err(|err| FileLockError {
            message: "Failed to open lock file",
            path: path.clone(),
            err,
        })?;

        // First try without blocking so we can report the current holder
        // before waiting. In non-blocking mode, report that the lock is
        // unavailable instead.
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) if blocking => {
                let holder = read_lock_owner(&file).unwrap_or_else(|| "<unknown>".to_owned());
                tracing::info!(?path, process_id, holder = %holder, "Waiting for lock");
                let wait_notice = start_file_lock_wait_notice(&path, &holder);
                let result = file.lock().map_err(|err| FileLockError {
                    message: "Failed to lock lock file",
                    path: path.clone(),
                    err,
                });
                drop(wait_notice);
                result?;
            }
            Err(TryLockError::WouldBlock) => {
                let holder = read_lock_owner(&file).unwrap_or_else(|| "<unknown>".to_owned());
                tracing::info!(?path, process_id, holder = %holder, "Lock is held");
                return Ok(None);
            }
            Err(TryLockError::Error(err)) => {
                return Err(FileLockError {
                    message: "Failed to lock lock file",
                    path,
                    err,
                });
            }
        }

        if let Err(err) = write_lock_owner(&mut file, process_id) {
            tracing::warn!(?err, ?path, process_id, "Failed to record lock owner");
        }
        tracing::info!(?path, process_id, "Locked");
        Ok(Some(Self {
            path,
            file: Some(file),
            process_id,
        }))
    }
}

impl Drop for FileLock {
    #[instrument(skip_all)]
    fn drop(&mut self) {
        tracing::info!(?self.path, process_id = self.process_id, "Releasing lock");
        if let Some(file) = self.file.take() {
            file.unlock()
                .inspect_err(|err| tracing::warn!(?err, ?self.path, "Failed to unlock lock file"))
                .map(|()| {
                    tracing::info!(?self.path, process_id = self.process_id, "Released lock");
                })
                .ok();
            // file is dropped here, closing the handle so the delete below
            // can succeed. Another process holding the file open prevents
            // deletion (they open without FILE_SHARE_DELETE), avoiding races.
        }
        std::fs::remove_file(&self.path).ok();
    }
}

fn try_create_file(path: &Path) -> io::Result<File> {
    let mut backoff_iterator = BackoffIterator::new();
    let mut options = OpenOptions::new();
    options
        .create(true)
        .read(true)
        .write(true)
        // Don't share delete access. This ensures that std::fs::remove_file
        // (which uses DeleteFileW) will fail if any other process has the file
        // open — so deletion in Drop only succeeds when we're the last handle
        // holder.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    loop {
        match options.open(path) {
            Ok(file) => return Ok(file),
            // The file may be open for deletion in Drop.
            Err(err)
                if err.kind() == io::ErrorKind::PermissionDenied
                    || err.raw_os_error() == Some(ERROR_SHARING_VIOLATION as _) =>
            {
                let Some(duration) = backoff_iterator.next() else {
                    return Err(err);
                };
                // Ensure that a file can be created in the target directory.
                tempfile::tempfile_in(path.parent().expect("file path should have parent"))?;
                thread::sleep(duration);
            }
            Err(err) => return Err(err),
        }
    }
}
