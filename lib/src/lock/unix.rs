// Copyright 2023 The Jujutsu Authors
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

#![expect(missing_docs)]

use std::fs::File;
use std::fs::OpenOptions;
use std::path::PathBuf;

use rustix::fs::FlockOperation;
use tracing::instrument;

use super::FileLockError;
use super::read_lock_owner;
use super::start_file_lock_wait_notice;
use super::write_lock_owner;

pub struct FileLock {
    path: PathBuf,
    file: File,
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
        loop {
            // Create lockfile, or open pre-existing one
            //
            // Do not truncate a pre-existing lockfile here. Its contents
            // identify the current holder while this process waits.
            let mut file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .map_err(|err| FileLockError {
                    message: "Failed to open lock file",
                    path: path.clone(),
                    err,
                })?;
            // First try without blocking so we can report the current holder
            // before waiting. In non-blocking mode, report that the lock is
            // unavailable instead.
            match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {}
                Err(rustix::io::Errno::WOULDBLOCK) if blocking => {
                    let holder = read_lock_owner(&file).unwrap_or_else(|| "<unknown>".to_owned());
                    tracing::info!(?path, process_id, holder = %holder, "Waiting for lock");
                    let wait_notice = start_file_lock_wait_notice(&path, &holder);
                    let result =
                        rustix::fs::flock(&file, FlockOperation::LockExclusive).map_err(|errno| {
                            FileLockError {
                                message: "Failed to lock lock file",
                                path: path.clone(),
                                err: errno.into(),
                            }
                        });
                    drop(wait_notice);
                    result?;
                }
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    let holder = read_lock_owner(&file).unwrap_or_else(|| "<unknown>".to_owned());
                    tracing::info!(?path, process_id, holder = %holder, "Lock is held");
                    return Ok(None);
                }
                Err(errno) => {
                    return Err(FileLockError {
                        message: "Failed to lock lock file",
                        path: path.clone(),
                        err: errno.into(),
                    });
                }
            }

            match rustix::fs::fstat(&file) {
                Ok(stat) => {
                    if stat.st_nlink == 0 {
                        // Lockfile was deleted, probably by the previous holder's `Drop` impl;
                        // create a new one so our ownership is visible,
                        // rather than hidden in an unlinked file. Not
                        // always necessary, since the previous holder might
                        // have exited abruptly.
                        continue;
                    }
                }
                Err(rustix::io::Errno::STALE) => {
                    // The file handle is stale.
                    // This can happen when using NFS,
                    // likely caused by a remote deletion of the lockfile.
                    // Treat this like a normal lockfile deletion and retry.
                    continue;
                }
                Err(errno) => {
                    return Err(FileLockError {
                        message: "failed to stat lock file",
                        path: path.clone(),
                        err: errno.into(),
                    });
                }
            }

            if let Err(err) = write_lock_owner(&mut file, process_id) {
                tracing::warn!(?err, ?path, process_id, "Failed to record lock owner");
            }
            tracing::info!(?path, process_id, "Locked");
            return Ok(Some(Self {
                path,
                file,
                process_id,
            }));
        }
    }
}

impl Drop for FileLock {
    #[instrument(skip_all)]
    fn drop(&mut self) {
        tracing::info!(?self.path, process_id = self.process_id, "Releasing lock");
        // Removing the file isn't strictly necessary, but reduces confusion.
        std::fs::remove_file(&self.path).ok();
        // Unblock any processes that tried to acquire the lock while we held it.
        // They're responsible for creating and locking a new lockfile, since we
        // just deleted this one.
        match rustix::fs::flock(&self.file, FlockOperation::Unlock) {
            Ok(()) => {
                tracing::info!(?self.path, process_id = self.process_id, "Released lock");
            }
            Err(err) => {
                tracing::warn!(?err, ?self.path, process_id = self.process_id, "Failed to release lock");
            }
        }
    }
}
