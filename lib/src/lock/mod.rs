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

#![expect(missing_docs)]

mod backoff;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::fs::File;
use std::io;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

use thiserror::Error;

#[cfg(unix)]
pub use self::unix::FileLock;
#[cfg(windows)]
pub use self::windows::FileLock;

#[derive(Debug, Error)]
#[error("{message}: {path}")]
pub struct FileLockError {
    pub message: &'static str,
    pub path: PathBuf,
    #[source]
    pub err: io::Error,
}

/// A notice displayed while a [`FileLock`] acquisition is blocked.
///
/// The notice remains active until this guard is dropped after the lock is
/// acquired or the acquisition fails.
pub trait FileLockWaitNotice {}

/// Reports blocking [`FileLock`] acquisitions to an interactive frontend.
pub trait FileLockWaitReporter: Send + Sync {
    /// Starts a notice for a lock held by `holder`.
    fn start_wait(&self, path: &Path, holder: &str) -> Box<dyn FileLockWaitNotice>;
}

static FILE_LOCK_WAIT_REPORTER: RwLock<Option<Arc<dyn FileLockWaitReporter>>> = RwLock::new(None);

/// Configures the process-wide reporter for blocking [`FileLock`] acquisitions.
///
/// Frontends should set this only when they can display interactive progress.
/// Passing `None` disables wait notices.
pub fn set_file_lock_wait_reporter(reporter: Option<Arc<dyn FileLockWaitReporter>>) {
    match FILE_LOCK_WAIT_REPORTER.write() {
        Ok(mut current) => *current = reporter,
        Err(err) => tracing::warn!(?err, "Failed to configure file-lock wait reporter"),
    }
}

fn start_file_lock_wait_notice(path: &Path, holder: &str) -> Option<Box<dyn FileLockWaitNotice>> {
    let reporter = FILE_LOCK_WAIT_REPORTER.read().ok()?.clone()?;
    Some(reporter.start_wait(path, holder))
}

fn read_lock_owner(file: &File) -> Option<String> {
    let mut reader = file.try_clone().ok()?;
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut owner = String::new();
    reader.read_to_string(&mut owner).ok()?;
    let owner = owner.trim();
    if owner.is_empty() {
        None
    } else {
        Some(owner.to_owned())
    }
}

fn write_lock_owner(file: &mut File, process_id: u32) -> io::Result<()> {
    let owner = format!("pid={process_id}\n");
    file.seek(SeekFrom::Start(0))?;
    file.write_all(owner.as_bytes())?;
    file.set_len(owner.len() as u64)?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use std::cmp::max;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::tests::new_temp_dir;

    #[test]
    fn lock_basic() {
        let temp_dir = new_temp_dir();
        let lock_path = temp_dir.path().join("test.lock");
        assert!(!lock_path.exists());
        {
            let _lock = FileLock::lock(lock_path.clone()).unwrap();
            assert!(lock_path.exists());
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn lock_concurrent() {
        let temp_dir = new_temp_dir();
        let data_path = temp_dir.path().join("test");
        let lock_path = temp_dir.path().join("test.lock");
        fs::write(&data_path, 0_u32.to_le_bytes()).unwrap();
        let num_threads = max(num_cpus::get(), 4);
        thread::scope(|s| {
            for _ in 0..num_threads {
                s.spawn(|| {
                    let _lock = FileLock::lock(lock_path.clone()).unwrap();
                    let data = fs::read(&data_path).unwrap();
                    let value = u32::from_le_bytes(data.try_into().unwrap());
                    thread::sleep(Duration::from_millis(1));
                    fs::write(&data_path, (value + 1).to_le_bytes()).unwrap();
                });
            }
        });
        let data = fs::read(&data_path).unwrap();
        let value = u32::from_le_bytes(data.try_into().unwrap());
        assert_eq!(value, num_threads as u32);
    }

    #[test]
    fn try_lock_succeeds_when_unlocked() {
        let temp_dir = new_temp_dir();
        let lock_path = temp_dir.path().join("test.lock");
        assert!(!lock_path.exists());
        {
            let lock = FileLock::try_lock(lock_path.clone()).unwrap();
            assert!(lock.is_some());
            assert!(lock_path.exists());
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn try_lock_gives_up_when_locked() {
        let temp_dir = new_temp_dir();
        let lock_path = temp_dir.path().join("test.lock");
        let _held = FileLock::lock(lock_path.clone()).unwrap();
        // The lock is already held, so a non-blocking attempt returns `None`
        // instead of blocking. The two lock handles are independent even within
        // a single process, so the second attempt is denied.
        assert!(FileLock::try_lock(lock_path).unwrap().is_none());
    }

    #[test]
    fn lock_file_records_owner_without_waiter_truncation() {
        let temp_dir = new_temp_dir();
        let lock_path = temp_dir.path().join("test.lock");
        let _held = FileLock::lock(lock_path.clone()).unwrap();
        let owner = format!("pid={}\n", std::process::id());
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), owner);

        assert!(FileLock::try_lock(lock_path.clone()).unwrap().is_none());
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), owner);
    }
}
