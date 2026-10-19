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

//! Coordinates the one-way operation-store transition for synchronization.

use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;

use crate::lock::FileLockError;

/// Shared while a plain-format repository is open, exclusive while enabling
/// synchronization. The backend marker is never rewritten by this transition,
/// so every holder locks the same inode without creating additional files.
pub(crate) struct RepoFormatLock {
    _file: File,
}

impl RepoFormatLock {
    pub(crate) fn shared(repo_path: &Path) -> Result<Self, FileLockError> {
        Self::acquire(repo_path, true)
    }

    pub(crate) fn exclusive(repo_path: &Path) -> Result<Self, FileLockError> {
        Self::acquire(repo_path, false)
    }

    fn acquire(repo_path: &Path, shared: bool) -> Result<Self, FileLockError> {
        let path = repo_path.join("store/type");
        let file = if shared {
            File::open(&path)
        } else {
            // NFS can require write access for exclusive locks. Filesystems
            // such as Btrfs, ext4, and Windows also support a read-only handle.
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .or_else(|_| File::open(&path))
        }
        .map_err(|err| FileLockError {
            message: "Failed to open repository format lock",
            path: path.clone(),
            err,
        })?;
        tracing::debug!(?path, shared, "Waiting for repository format lock");
        if shared {
            file.lock_shared()
        } else {
            file.lock()
        }
        .map_err(|err| FileLockError {
            message: "Failed to lock repository format",
            path: path.clone(),
            err,
        })?;
        tracing::debug!(?path, shared, "Acquired repository format lock");
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::fs::TryLockError;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::tests::TestResult;
    use crate::tests::new_temp_dir;

    fn make_backend_marker(path: &Path) -> std::io::Result<PathBuf> {
        fs::create_dir(path.join("store"))?;
        let marker = path.join("store/type");
        fs::write(&marker, b"git")?;
        Ok(marker)
    }

    #[test]
    fn readers_share_access_without_changing_the_backend_marker() -> TestResult {
        let directory = new_temp_dir();
        let path = make_backend_marker(directory.path())?;
        #[cfg(unix)]
        let original_inode = {
            use std::os::unix::fs::MetadataExt as _;
            fs::metadata(&path)?.ino()
        };
        let first = RepoFormatLock::shared(directory.path())?;
        let second = RepoFormatLock::shared(directory.path())?;
        let probe = File::open(&path)?;
        assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
        drop(first);
        assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
        drop(second);
        probe.try_lock()?;
        drop(probe);
        drop(RepoFormatLock::exclusive(directory.path())?);
        drop(RepoFormatLock::shared(directory.path())?);
        assert_eq!(fs::read(&path)?, b"git");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            assert_eq!(fs::metadata(&path)?.ino(), original_inode);
        }
        assert_eq!(fs::read_dir(directory.path())?.count(), 1);
        assert_eq!(fs::read_dir(directory.path().join("store"))?.count(), 1);
        Ok(())
    }

    #[test]
    fn readers_do_not_need_a_writable_backend_marker() -> TestResult {
        let directory = new_temp_dir();
        let path = make_backend_marker(directory.path())?;
        let marker_permissions = fs::metadata(&path)?.permissions();
        let mut readonly = marker_permissions.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&path, readonly)?;
        let permissions = fs::metadata(directory.path().join("store"))?.permissions();
        let mut readonly = permissions.clone();
        readonly.set_readonly(true);
        fs::set_permissions(directory.path().join("store"), readonly)?;
        let result = RepoFormatLock::shared(directory.path()).map(drop);
        // Restore permissions so temporary-directory cleanup works on Windows.
        fs::set_permissions(directory.path().join("store"), permissions)?;
        fs::set_permissions(&path, marker_permissions)?;
        result?;
        assert_eq!(fs::read(&path)?, b"git");
        assert_eq!(fs::read_dir(directory.path())?.count(), 1);
        assert_eq!(fs::read_dir(directory.path().join("store"))?.count(), 1);
        Ok(())
    }

    #[test]
    fn exclusive_access_blocks_new_readers() -> TestResult {
        let directory = new_temp_dir();
        let path = make_backend_marker(directory.path())?;
        let exclusive = RepoFormatLock::exclusive(directory.path())?;
        let probe = File::open(&path)?;
        assert!(matches!(
            probe.try_lock_shared(),
            Err(TryLockError::WouldBlock)
        ));
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                started_tx.send(()).unwrap();
                let reader = RepoFormatLock::shared(directory.path()).unwrap();
                acquired_tx.send(()).unwrap();
                drop(reader);
            });
            started_rx.recv().unwrap();
            assert!(matches!(
                acquired_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(exclusive);
            acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        Ok(())
    }

    #[test]
    fn exclusive_access_waits_for_existing_readers() -> TestResult {
        let directory = new_temp_dir();
        make_backend_marker(directory.path())?;
        let reader = RepoFormatLock::shared(directory.path())?;
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                started_tx.send(()).unwrap();
                let exclusive = RepoFormatLock::exclusive(directory.path()).unwrap();
                acquired_tx.send(()).unwrap();
                drop(exclusive);
            });
            started_rx.recv().unwrap();
            assert!(matches!(
                acquired_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(reader);
            acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        Ok(())
    }
}
