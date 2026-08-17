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

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::fs::DirEntry;
use std::fs::File;
use std::fs::Metadata;
use std::fs::OpenOptions;
use std::io;
use std::io::Read as _;
use std::io::Write as _;
use std::ops::Range;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use std::os::fd::AsRawFd as _;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use std::os::unix::ffi::OsStringExt as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::mpsc::Sender;
use std::sync::mpsc::channel;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use std::sync::mpsc::sync_channel;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use std::thread::JoinHandle;
#[cfg(debug_assertions)]
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use either::Either;
use futures::AsyncRead;
use futures::AsyncReadExt as _;
use futures::StreamExt as _;
use futures::io::AllowStdIo;
use itertools::Itertools as _;
use once_cell::unsync::OnceCell;
use pollster::FutureExt as _;
use prost::Message as _;
use rayon::iter::IntoParallelIterator as _;
use rayon::prelude::IndexedParallelIterator as _;
use rayon::prelude::ParallelIterator as _;
use tempfile::NamedTempFile;
use thiserror::Error;
use tracing::instrument;
use tracing::trace_span;

use crate::backend::BackendError;
use crate::backend::CopyId;
use crate::backend::FileId;
use crate::backend::MergedTreeValue;
use crate::backend::SymlinkId;
use crate::backend::TreeId;
use crate::backend::TreeValue;
use crate::commit::Commit;
use crate::config::ConfigGetError;
use crate::conflict_labels::ConflictLabels;
use crate::conflicts;
use crate::conflicts::ConflictMarkerStyle;
use crate::conflicts::ConflictMaterializeOptions;
use crate::conflicts::MIN_CONFLICT_MARKER_LEN;
use crate::conflicts::MaterializedTreeValue;
use crate::conflicts::choose_materialized_conflict_marker_len;
use crate::conflicts::materialize_merge_result_to_bytes;
use crate::conflicts::materialize_tree_value;
pub use crate::eol::EolConversionMode;
use crate::eol::TargetEolStrategy;
use crate::file_util::FileIdentity;
use crate::file_util::check_symlink_support;
use crate::file_util::copy_async_to_sync;
use crate::file_util::persist_temp_file;
use crate::file_util::symlink_file;
use crate::fsmonitor::AwacsConfig;
use crate::fsmonitor::FsmonitorSettings;
#[cfg(feature = "watchman")]
use crate::fsmonitor::WatchmanConfig;
#[cfg(feature = "watchman")]
use crate::fsmonitor::watchman;
use crate::gitignore::GitIgnoreFile;
use crate::lock::FileLock;
use crate::matchers::DifferenceMatcher;
use crate::matchers::EverythingMatcher;
use crate::matchers::FilesMatcher;
use crate::matchers::IntersectionMatcher;
use crate::matchers::Matcher;
use crate::matchers::PrefixMatcher;
use crate::matchers::UnionMatcher;
use crate::merge::Merge;
use crate::merge::MergeBuilder;
use crate::merge::SameChange;
use crate::merged_tree::MergedTree;
use crate::merged_tree::TreeDiffEntry;
use crate::merged_tree_builder::MergedTreeBuilder;
use crate::object_id::ObjectId as _;
use crate::op_store::OperationId;
use crate::ref_name::WorkspaceName;
use crate::ref_name::WorkspaceNameBuf;
use crate::repo_path::RepoPath;
use crate::repo_path::RepoPathBuf;
use crate::repo_path::RepoPathComponent;
use crate::settings::UserSettings;
use crate::store::Store;
use crate::working_copy::CheckoutError;
use crate::working_copy::CheckoutStats;
use crate::working_copy::LockedWorkingCopy;
use crate::working_copy::ResetError;
use crate::working_copy::SnapshotError;
use crate::working_copy::SnapshotOptions;
use crate::working_copy::SnapshotProgress;
use crate::working_copy::SnapshotStats;
use crate::working_copy::SnapshotWarning;
use crate::working_copy::UntrackedReason;
use crate::working_copy::WorkingCopy;
use crate::working_copy::WorkingCopyFactory;
use crate::working_copy::WorkingCopyStateError;

fn symlink_target_convert_to_store(path: &Path) -> Option<Cow<'_, str>> {
    let path = path.to_str()?;
    if std::path::MAIN_SEPARATOR == '/' {
        Some(Cow::Borrowed(path))
    } else {
        // When storing the symlink target on Windows, convert "\" to "/", so that the
        // symlink remains valid on Unix.
        //
        // Note that we don't use std::path to handle the conversion, because it
        // performs poorly with Windows verbatim paths like \\?\Global\C:\file.txt.
        Some(Cow::Owned(path.replace(std::path::MAIN_SEPARATOR_STR, "/")))
    }
}

fn symlink_target_convert_to_disk(path: &str) -> PathBuf {
    let path = if std::path::MAIN_SEPARATOR == '/' {
        Cow::Borrowed(path)
    } else {
        // Use the main separator to reformat the input path to avoid creating a broken
        // symlink with the incorrect separator "/".
        //
        // See https://github.com/jj-vcs/jj/issues/6934 for the relevant bug.
        Cow::Owned(path.replace('/', std::path::MAIN_SEPARATOR_STR))
    };
    PathBuf::from(path.as_ref())
}

/// How to propagate executable bit changes in file metadata to/from the repo.
///
/// On Windows, executable bits are always ignored, but on Unix they are
/// respected by default, but may be ignored by user settings or if we find
/// that the filesystem of the working copy doesn't support executable bits.
#[derive(Clone, Copy, Debug)]
enum ExecChangePolicy {
    Ignore,
    #[cfg_attr(windows, expect(dead_code))]
    Respect,
}

/// The executable bit change setting as exposed to the user.
#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecChangeSetting {
    Ignore,
    Respect,
    #[default]
    Auto,
}

impl ExecChangePolicy {
    /// Get the executable bit policy based on user settings and executable bit
    /// support in the working copy's state path.
    ///
    /// On Unix we check whether executable bits are supported in the working
    /// copy to determine respect/ignorance, but we default to respect.
    #[cfg_attr(windows, expect(unused_variables))]
    fn new(exec_change_setting: ExecChangeSetting, state_path: &Path) -> Self {
        #[cfg(windows)]
        return Self::Ignore;
        #[cfg(unix)]
        return match exec_change_setting {
            ExecChangeSetting::Ignore => Self::Ignore,
            ExecChangeSetting::Respect => Self::Respect,
            ExecChangeSetting::Auto => {
                match crate::file_util::check_executable_bit_support(state_path) {
                    Ok(false) => Self::Ignore,
                    Ok(true) => Self::Respect,
                    Err(err) => {
                        tracing::warn!(?err, "Error when checking for executable bit support");
                        Self::Respect
                    }
                }
            }
        };
    }
}

/// Returns the effective executable-bit policy used by local working copies in
/// a stable form suitable for external-input fingerprints.
pub fn effective_exec_bit_policy_for_fingerprint(
    user_settings: &UserSettings,
    state_path: &Path,
) -> Result<&'static str, ConfigGetError> {
    let exec_change_setting = user_settings.get("working-copy.exec-bit-change")?;
    Ok(
        match ExecChangePolicy::new(exec_change_setting, state_path) {
            ExecChangePolicy::Ignore => "ignore",
            ExecChangePolicy::Respect => "respect",
        },
    )
}

/// On-disk executable bit observed while scanning or materializing a file.
/// This does *not* necessarily equal the `executable` field of
/// [`TreeValue::File`]: the two are allowed to diverge if and only if we're
/// ignoring executable bit changes.
///
/// This will only ever be true on Windows if the repo is also being accessed
/// from a Unix version of jj, such as when accessed from WSL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecBit(bool);

impl ExecBit {
    /// Get the executable bit for a tree value to write to the repo store.
    ///
    /// If we're ignoring the executable bit, then we fallback to the previous
    /// in-repo executable bit if present.
    fn for_tree_value(
        self,
        exec_policy: ExecChangePolicy,
        prev_in_repo: impl FnOnce() -> Option<bool>,
    ) -> bool {
        match exec_policy {
            ExecChangePolicy::Ignore => prev_in_repo().unwrap_or(false),
            ExecChangePolicy::Respect => self.0,
        }
    }

    /// Set the on-disk executable bit to be written based on the in-repo bit or
    /// the previous on-disk executable bit.
    ///
    /// On Windows, we return `false` because when we later write files, we
    /// always create them anew, and the executable bit will be `false` even if
    /// shared with a Unix machine.
    ///
    /// `prev_on_disk` is a closure because it is somewhat expensive and is only
    /// used if ignoring the executable bit on Unix.
    fn new_from_repo(
        in_repo: bool,
        exec_policy: ExecChangePolicy,
        prev_on_disk: impl FnOnce() -> Option<Self>,
    ) -> Self {
        match exec_policy {
            _ if cfg!(windows) => Self(false),
            ExecChangePolicy::Ignore => prev_on_disk().unwrap_or(Self(false)),
            ExecChangePolicy::Respect => Self(in_repo),
        }
    }

    /// Load the on-disk executable bit from file metadata.
    #[cfg_attr(windows, expect(unused_variables))]
    fn new_from_disk(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        return Self(metadata.permissions().mode() & 0o111 != 0);
        #[cfg(windows)]
        return Self(false);
    }
}

/// Set the executable bit of a file on-disk. This is a no-op on Windows.
///
/// On Unix, we manually set the executable bit to the previous value on-disk.
/// This is necessary because we write all files by creating them new, so files
/// won't preserve their permissions naturally.
#[cfg_attr(windows, expect(unused_variables))]
fn set_executable(exec_bit: ExecBit, disk_path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        let mode = if exec_bit.0 { 0o755 } else { 0o644 };
        fs::set_permissions(disk_path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// The only disk metadata needed while interpreting one scanned path.
#[derive(Debug, PartialEq, Eq, Clone)]
enum ObservedDiskKind {
    Normal { exec_bit: ExecBit },
    Symlink,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct MaterializedConflictData {
    conflict_marker_len: u32,
}

/// The only semantic information the scanner needs from the prior tree while
/// examining a filesystem scope. This index is rebuilt from the tree for one
/// command and is never serialized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackedKind {
    FileLike,
    GitSubmodule,
}

#[derive(Clone, Debug)]
struct TrackedPathEntry {
    path: RepoPathBuf,
    kind: TrackedKind,
}

#[derive(Clone, Debug, Default)]
struct TrackedPathsMap {
    data: Vec<TrackedPathEntry>,
}

impl TrackedPathsMap {
    fn from_entries(mut data: Vec<TrackedPathEntry>) -> Self {
        data.sort_unstable_by(|entry1, entry2| entry1.path.cmp(&entry2.path));
        debug_assert!(data.is_sorted_by(|entry1, entry2| entry1.path < entry2.path));
        Self { data }
    }

    fn all(&self) -> TrackedPaths<'_> {
        TrackedPaths { data: &self.data }
    }
}

/// Read-only semantic tracked-path index, optionally restricted to a prefix.
#[derive(Clone, Copy, Debug)]
struct TrackedPaths<'a> {
    data: &'a [TrackedPathEntry],
}

impl<'a> TrackedPaths<'a> {
    fn prefixed(&self, base: &RepoPath) -> Self {
        let range = self.prefixed_range(base);
        Self {
            data: &self.data[range],
        }
    }

    /// Faster version of `prefixed("<dir>/<base>")`. Requires that all entries
    /// share the same prefix `dir`.
    fn prefixed_at(&self, dir: &RepoPath, base: &RepoPathComponent) -> Self {
        let range = self.prefixed_range_at(dir, base);
        Self {
            data: &self.data[range],
        }
    }

    fn get(&self, path: &RepoPath) -> Option<TrackedKind> {
        let pos = self
            .data
            .binary_search_by(|entry| entry.path.as_ref().cmp(path))
            .ok()?;
        Some(self.data[pos].kind)
    }

    fn get_at(&self, dir: &RepoPath, name: &RepoPathComponent) -> Option<TrackedKind> {
        let pos = self.exact_position_at(dir, name)?;
        Some(self.data[pos].kind)
    }

    fn exact_position_at(&self, dir: &RepoPath, name: &RepoPathComponent) -> Option<usize> {
        debug_assert!(self.paths().all(|path| path.starts_with(dir)));
        let slash_len = usize::from(!dir.is_root());
        let prefix_len = dir.as_internal_file_string().len() + slash_len;
        self.data
            .binary_search_by(|entry| {
                let tail = entry
                    .path
                    .as_internal_file_string()
                    .get(prefix_len..)
                    .unwrap_or("");
                match tail.split_once('/') {
                    // "<name>/*" > "<name>"
                    Some((pre, _)) => pre.cmp(name.as_internal_str()).then(Ordering::Greater),
                    None => tail.cmp(name.as_internal_str()),
                }
            })
            .ok()
    }

    fn prefixed_range(&self, base: &RepoPath) -> Range<usize> {
        let start = self
            .data
            .partition_point(|entry| entry.path.as_ref() < base);
        let len = self.data[start..].partition_point(|entry| entry.path.starts_with(base));
        start..(start + len)
    }

    fn prefixed_range_at(&self, dir: &RepoPath, base: &RepoPathComponent) -> Range<usize> {
        debug_assert!(self.paths().all(|path| path.starts_with(dir)));
        let slash_len = usize::from(!dir.is_root());
        let prefix_len = dir.as_internal_file_string().len() + slash_len;
        let start = self.data.partition_point(|entry| {
            let tail = entry
                .path
                .as_internal_file_string()
                .get(prefix_len..)
                .unwrap_or("");
            let entry_name = tail.split_once('/').map_or(tail, |(name, _)| name);
            entry_name < base.as_internal_str()
        });
        let len = self.data[start..].partition_point(|entry| {
            let tail = entry
                .path
                .as_internal_file_string()
                .get(prefix_len..)
                .unwrap_or("");
            let entry_name = tail.split_once('/').map_or(tail, |(name, _)| name);
            entry_name == base.as_internal_str()
        });
        start..(start + len)
    }

    fn iter(&self) -> impl Iterator<Item = (&'a RepoPath, TrackedKind)> + use<'a> {
        self.data
            .iter()
            .map(|entry| (entry.path.as_ref(), entry.kind))
    }

    fn paths(&self) -> impl ExactSizeIterator<Item = &'a RepoPath> + use<'a> {
        self.data.iter().map(|entry| entry.path.as_ref())
    }
}

fn sparse_patterns_from_proto(
    path: &Path,
    proto: Option<&crate::protos::local_working_copy::SparsePatterns>,
) -> Result<Vec<RepoPathBuf>, TreeStateError> {
    let mut sparse_patterns = vec![];
    if let Some(proto_sparse_patterns) = proto {
        for prefix in &proto_sparse_patterns.prefixes {
            let prefix = RepoPathBuf::from_internal_string(prefix).map_err(|err| {
                invalid_working_copy_state(path, format!("invalid sparse prefix: {err}"))
            })?;
            sparse_patterns.push(prefix);
        }
    } else {
        // For compatibility with old working copies.
        // TODO: Delete this is late 2022 or so.
        sparse_patterns.push(RepoPathBuf::root());
    }
    Ok(sparse_patterns)
}

/// Creates intermediate directories from the `working_copy_path` to the
/// `repo_path` parent. Returns disk path for the `repo_path` file.
///
/// If an intermediate directory exists and if it is a file or symlink, this
/// function returns `Ok(None)` to signal that the path should be skipped.
/// The `working_copy_path` directory may be a symlink.
///
/// If an existing or newly-created sub directory points to ".git" or ".jj",
/// this function returns an error.
///
/// Note that this does not prevent TOCTOU bugs caused by concurrent checkouts.
/// Another process may remove the directory created by this function and put a
/// symlink there.
fn create_parent_dirs(
    working_copy_path: &Path,
    repo_path: &RepoPath,
) -> Result<Option<PathBuf>, CheckoutError> {
    let (parent_path, basename) = repo_path.split().expect("repo path shouldn't be root");
    let mut dir_path = working_copy_path.to_owned();
    for c in parent_path.components() {
        // Ensure that the name is a normal entry of the current dir_path.
        dir_path.push(c.to_fs_name().map_err(|err| err.with_path(repo_path))?);
        // A directory named ".git" or ".jj" can be temporarily created. It
        // might trick workspace path discovery, but is harmless so long as the
        // directory is empty.
        let (new_dir_created, is_dir) = match fs::create_dir(&dir_path) {
            Ok(()) => (true, true), // New directory
            Err(err) => match dir_path.symlink_metadata() {
                Ok(m) => (false, m.is_dir()), // Existing file or directory
                Err(_) => {
                    return Err(CheckoutError::Other {
                        message: format!(
                            "Failed to create parent directories for {}",
                            repo_path.to_fs_path_unchecked(working_copy_path).display(),
                        ),
                        err: err.into(),
                    });
                }
            },
        };
        // Invalid component (e.g. "..") should have been rejected.
        // The current dir_path should be an entry of dir_path.parent().
        reject_reserved_existing_path(&dir_path).inspect_err(|_| {
            if new_dir_created {
                fs::remove_dir(&dir_path).ok();
            }
        })?;
        if !is_dir {
            return Ok(None); // Skip existing file or symlink
        }
    }

    let mut file_path = dir_path;
    file_path.push(
        basename
            .to_fs_name()
            .map_err(|err| err.with_path(repo_path))?,
    );
    Ok(Some(file_path))
}

/// Removes existing file named `disk_path` if any. Returns `Ok(true)` if the
/// file was there and got removed, meaning that new file can be safely created.
///
/// If the existing file points to ".git" or ".jj", this function returns an
/// error.
fn remove_old_file(disk_path: &Path) -> Result<bool, CheckoutError> {
    reject_reserved_existing_path(disk_path)?;
    match fs::remove_file(disk_path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        // TODO: Use io::ErrorKind::IsADirectory if it gets stabilized
        Err(_) if disk_path.symlink_metadata().is_ok_and(|m| m.is_dir()) => Ok(false),
        Err(err) => Err(CheckoutError::Other {
            message: format!("Failed to remove file {}", disk_path.display()),
            err: err.into(),
        }),
    }
}

/// Removes existing submodule directory named `disk_path` if any. Returns
/// `Ok(true)` if the directory was there and got removed, meaning that new file
/// can be safely created.
///
/// The directory will not be removed if it is not empty, as it could contain
/// untracked or modified files. This is in line with Git's behavior.
fn remove_old_submodule_dir(disk_path: &Path) -> Result<bool, CheckoutError> {
    match fs::remove_dir(disk_path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(false),
        Err(err) => Err(CheckoutError::Other {
            message: format!(
                "Failed to remove submodule directory {}",
                disk_path.display()
            ),
            err: err.into(),
        }),
    }
}

/// Checks if new file or symlink named `disk_path` can be created.
///
/// If the file already exists, this function return `Ok(false)` to signal
/// that the path should be skipped.
///
/// If the path may point to ".git" or ".jj" entry, this function returns an
/// error.
///
/// This function can fail if `disk_path.parent()` isn't a directory.
fn can_create_new_file(disk_path: &Path) -> Result<bool, CheckoutError> {
    // New file or symlink will be created by caller. If it were pointed to by
    // name ".git" or ".jj", git/jj CLI could be tricked to load configuration
    // from an attacker-controlled location. So we first test the path by
    // creating an empty file.
    let new_file = match OpenOptions::new()
        .write(true)
        .create_new(true) // Don't overwrite, don't follow symlink
        .open(disk_path)
    {
        Ok(file) => Some(file),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => None,
        // Workaround for "Access is denied. (os error 5)" error on Windows.
        Err(_) => match disk_path.symlink_metadata() {
            Ok(_) => None,
            Err(err) => {
                return Err(CheckoutError::Other {
                    message: format!("Failed to stat {}", disk_path.display()),
                    err: err.into(),
                });
            }
        },
    };

    let new_file_created = new_file.is_some();

    if let Some(new_file) = new_file {
        reject_reserved_existing_file(new_file, disk_path).inspect_err(|_| {
            // We keep the error from `reject_reserved_existing_file`
            fs::remove_file(disk_path).ok();
        })?;

        fs::remove_file(disk_path).map_err(|err| CheckoutError::Other {
            message: format!("Failed to remove temporary file {}", disk_path.display()),
            err: err.into(),
        })?;
    } else {
        reject_reserved_existing_path(disk_path)?;
    }
    Ok(new_file_created)
}

const RESERVED_DIR_NAMES: &[&str] = &[".git", ".jj"];

fn file_identity_from_symlink_path(disk_path: &Path) -> io::Result<Option<FileIdentity>> {
    match FileIdentity::from_symlink_path(disk_path) {
        Ok(identity) => Ok(Some(identity)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Wrapper for [`reject_reserved_existing_file_identity`] which avoids a
/// syscall by converting the provided `file` to a `FileIdentity` via its
/// file descriptor.
///
/// See [`reject_reserved_existing_file_identity`] for more info.
fn reject_reserved_existing_file(file: File, disk_path: &Path) -> Result<(), CheckoutError> {
    // Note: since the file is open, we don't expect that it's possible for
    // `io::ErrorKind::NotFound` to be a possible error returned here.
    let file_identity = FileIdentity::from_file(file).map_err(|err| CheckoutError::Other {
        message: format!("Failed to validate path {}", disk_path.display()),
        err: err.into(),
    })?;

    reject_reserved_existing_file_identity(file_identity, disk_path)
}

/// Wrapper for [`reject_reserved_existing_file_identity`] which converts
/// the provided `disk_path` to a `FileIdentity`.
///
/// See [`reject_reserved_existing_file_identity`] for more info.
///
/// # Remarks
///
/// On Windows, this incurs an additional syscall cost to open and close the
/// file `HANDLE` for `disk_path`. On Unix, `lstat()` is used.
fn reject_reserved_existing_path(disk_path: &Path) -> Result<(), CheckoutError> {
    let Some(disk_identity) =
        file_identity_from_symlink_path(disk_path).map_err(|err| CheckoutError::Other {
            message: format!("Failed to validate path {}", disk_path.display()),
            err: err.into(),
        })?
    else {
        // If the existing disk_path pointed to the reserved path, we would have
        // gotten an identity back. Since we got nothing, the file does not exist
        // and cannot be a reserved path name.
        return Ok(());
    };

    reject_reserved_existing_file_identity(disk_identity, disk_path)
}

/// Suppose the `disk_path` exists, checks if the last component points to
/// ".git" or ".jj" in the same parent directory.
///
/// `disk_identity` is expected to be an identity of the file described by
/// `disk_path`.
///
/// # Remarks
///
/// On Windows, this incurs a syscall cost to open and close a file `HANDLE` for
/// each filename in `RESERVED_DIR_NAMES`. On Unix, `lstat()` is used.
fn reject_reserved_existing_file_identity(
    disk_identity: FileIdentity,
    disk_path: &Path,
) -> Result<(), CheckoutError> {
    let parent_dir_path = disk_path.parent().expect("content path shouldn't be root");
    for name in RESERVED_DIR_NAMES {
        let reserved_path = parent_dir_path.join(name);

        let Some(reserved_identity) =
            file_identity_from_symlink_path(&reserved_path).map_err(|err| {
                CheckoutError::Other {
                    message: format!("Failed to validate path {}", disk_path.display()),
                    err: err.into(),
                }
            })?
        else {
            // If the existing disk_path pointed to the reserved path, we would have
            // gotten an identity back. Since we got nothing, the file does not exist
            // and cannot be a reserved path name.
            continue;
        };

        if disk_identity == reserved_identity {
            return Err(CheckoutError::ReservedPathComponent {
                path: disk_path.to_owned(),
                name,
            });
        }
    }

    Ok(())
}

/// Classifies a scanned filesystem entry using only metadata needed to
/// interpret its semantic tree value.
fn observed_disk_kind(metadata: &Metadata) -> Option<ObservedDiskKind> {
    let metadata_file_type = metadata.file_type();
    if metadata_file_type.is_dir() {
        None
    } else if metadata_file_type.is_symlink() {
        Some(ObservedDiskKind::Symlink)
    } else if metadata_file_type.is_file() {
        let exec_bit = ExecBit::new_from_disk(metadata);
        Some(ObservedDiskKind::Normal { exec_bit })
    } else {
        None
    }
}

/// Inputs selected by a filesystem-monitor backend for one snapshot scan.
///
/// The scan root is intentionally separate from [`TreeState::working_copy_path`]:
/// a backend may provide an immutable read view while working-copy state and
/// mutations continue to use the live root.
struct SnapshotScan {
    scan_root: PathBuf,
    scope: ScanScope,
    fsmonitor_cursor: Option<crate::protos::local_working_copy::FsmonitorCursor>,
    baseline: Option<crate::protos::local_working_copy::AwacsSnapshotBaseline>,
    completion: Option<PendingScan>,
    warning: Option<SnapshotWarning>,
}

/// Authoritative filesystem work selected for one scan. `Changed` is used
/// only for a retained immutable snapshot delta; mutable monitors fall back to
/// `Full` because their names are advisory without per-path state.
#[derive(Clone, Debug)]
enum ScanScope {
    Full,
    Changed {
        exact: Vec<RepoPathBuf>,
        prefixes: Vec<RepoPathBuf>,
    },
}

impl ScanScope {
    fn from_delta(exact: Vec<RepoPathBuf>, mut prefixes: Vec<RepoPathBuf>) -> Self {
        // In snapshot mode, the committed baseline owns the classification of
        // unchanged paths. An ignore-file change therefore applies only to
        // paths AWACS also reported as changed; rescanning its entire parent
        // would retroactively track previously ignored files and turns a root
        // .gitignore edit into an unbounded crawl.
        prefixes.sort_unstable();
        prefixes.dedup();
        let mut normalized_prefixes = Vec::new();
        for prefix in prefixes {
            if !normalized_prefixes
                .iter()
                .any(|ancestor: &RepoPathBuf| prefix.starts_with(ancestor))
            {
                normalized_prefixes.push(prefix);
            }
        }
        let mut exact = exact;
        exact.sort_unstable();
        exact.dedup();
        exact.retain(|path| {
            !normalized_prefixes
                .iter()
                .any(|prefix| path.starts_with(prefix))
        });
        Self::Changed {
            exact,
            prefixes: normalized_prefixes,
        }
    }

    fn matcher(&self) -> Box<dyn Matcher> {
        match self {
            Self::Full => Box::new(EverythingMatcher),
            Self::Changed { exact, prefixes } => {
                if prefixes.is_empty() {
                    Box::new(FilesMatcher::new(exact))
                } else if exact.is_empty() {
                    Box::new(PrefixMatcher::new(prefixes))
                } else {
                    Box::new(UnionMatcher::new(
                        FilesMatcher::new(exact),
                        PrefixMatcher::new(prefixes),
                    ))
                }
            }
        }
    }

    fn requires_full_traversal(&self) -> bool {
        match self {
            Self::Full => true,
            Self::Changed { exact, prefixes } => {
                exact.iter().chain(prefixes).any(|path| path.is_root())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanOutcome {
    Committed,
    Aborted,
}

trait ScanSession: Send {
    fn check_healthy(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }

    /// Stops any background work that could invalidate the session after the
    /// final health check but before tree state is durably saved.
    fn prepare_to_commit(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.check_healthy()
    }

    fn finish(self: Box<Self>, outcome: ScanOutcome) -> Result<(), Box<dyn Error + Send + Sync>>;
}

/// A completion hook which aborts unless the working-copy transaction commits
/// it explicitly after saving tree state.
struct PendingScan {
    session: Option<Box<dyn ScanSession>>,
}

impl PendingScan {
    fn new(session: Box<dyn ScanSession>) -> Self {
        Self {
            session: Some(session),
        }
    }

    fn finish(mut self, outcome: ScanOutcome) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.session.take().unwrap().finish(outcome)
    }

    fn check_healthy(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.session.as_ref().unwrap().check_healthy()
    }

    fn prepare_to_commit(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.session.as_mut().unwrap().prepare_to_commit()
    }
}

struct NoopScanSession;

impl ScanSession for NoopScanSession {
    fn finish(self: Box<Self>, _outcome: ScanOutcome) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
struct AwacsScanSession {
    lease: Arc<Mutex<Option<btrfs_awacs::scan::SnapshotLease>>>,
    renewal_error: Arc<Mutex<Option<String>>>,
    stop_renewal: Option<std::sync::mpsc::SyncSender<()>>,
    renewal_thread: Option<JoinHandle<()>>,
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
impl AwacsScanSession {
    fn new(lease: btrfs_awacs::scan::SnapshotLease) -> Self {
        let renew_interval = lease.renewal_interval();
        let lease = Arc::new(Mutex::new(Some(lease)));
        let renewal_error = Arc::new(Mutex::new(None));
        let (stop_renewal, stop_receiver) = sync_channel(1);
        let thread_lease = lease.clone();
        let thread_error = renewal_error.clone();
        let renewal_thread = std::thread::spawn(move || {
            while stop_receiver.recv_timeout(renew_interval).is_err() {
                let result = thread_lease
                    .lock()
                    .expect("AWACS lease lock should not be poisoned")
                    .as_mut()
                    .expect("AWACS lease should exist while renewal is active")
                    .renew();
                if let Err(err) = result {
                    *thread_error
                        .lock()
                        .expect("AWACS renewal error lock should not be poisoned") =
                        Some(err.to_string());
                    break;
                }
            }
        });
        Self {
            lease,
            renewal_error,
            stop_renewal: Some(stop_renewal),
            renewal_thread: Some(renewal_thread),
        }
    }

    fn stop_renewing(&mut self) {
        if let Some(stop) = self.stop_renewal.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.renewal_thread.take() {
            drop(thread.join());
        }
    }

    fn renewal_error(&self) -> Option<String> {
        self.renewal_error
            .lock()
            .expect("AWACS renewal error lock should not be poisoned")
            .clone()
    }
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
impl ScanSession for AwacsScanSession {
    fn check_healthy(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        if let Some(err) = self.renewal_error() {
            return Err(format!("AWACS scan lease renewal failed: {err}").into());
        }
        Ok(())
    }

    fn prepare_to_commit(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.stop_renewing();
        self.check_healthy()?;
        self.lease
            .lock()
            .expect("AWACS lease lock should not be poisoned")
            .as_mut()
            .expect("AWACS lease should exist while committing")
            .promote()?;
        Ok(())
    }

    fn finish(
        mut self: Box<Self>,
        outcome: ScanOutcome,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.stop_renewing();
        let renewal_error = self.renewal_error();
        let outcome = if renewal_error.is_some() {
            ScanOutcome::Aborted
        } else {
            outcome
        };
        let outcome = match outcome {
            ScanOutcome::Committed => btrfs_awacs::scan::ScanOutcome::Committed,
            ScanOutcome::Aborted => btrfs_awacs::scan::ScanOutcome::Aborted,
        };
        let mut lease = self
            .lease
            .lock()
            .expect("AWACS lease lock should not be poisoned")
            .take()
            .unwrap();
        lease.finish(outcome)?;
        if let Some(err) = renewal_error {
            return Err(format!("AWACS scan lease renewal failed: {err}").into());
        }
        Ok(())
    }
}

impl Drop for PendingScan {
    fn drop(&mut self) {
        if let Some(session) = self.session.take()
            && let Err(err) = session.finish(ScanOutcome::Aborted)
        {
            tracing::warn!(?err, "failed to abort filesystem-monitor scan session");
        }
    }
}

/// Debug-build integration hook used with the AWACS daemon's short-lease
/// controls. It delays traversal only after the background renewal owner has
/// been created, so tests can deterministically exercise renewal failure.
#[cfg(debug_assertions)]
fn maybe_delay_awacs_traversal_for_test() {
    let Some(control_dir) = std::env::var_os("BTRFS_AWACS_SCAN_TEST_CONTROL_DIR") else {
        return;
    };
    let marker = PathBuf::from(control_dir).join("delay-traversal-ms");
    let delay_ms = match std::fs::read_to_string(&marker) {
        Ok(value) => value.trim().parse::<u64>().ok(),
        Err(_) => None,
    };
    let Some(delay_ms) = delay_ms else {
        return;
    };
    if std::fs::remove_file(marker).is_ok() {
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
}

fn watchman_cursor(
    clock: crate::protos::local_working_copy::WatchmanClock,
) -> crate::protos::local_working_copy::FsmonitorCursor {
    use crate::protos::local_working_copy::fsmonitor_cursor::Cursor;
    crate::protos::local_working_copy::FsmonitorCursor {
        cursor: Some(Cursor::Watchman(clock)),
    }
}

fn synthetic_test_awacs_baseline(
    token: &[u8],
    input_fingerprint: [u8; 32],
) -> crate::protos::local_working_copy::AwacsSnapshotBaseline {
    crate::protos::local_working_copy::AwacsSnapshotBaseline {
        filesystem_uuid: vec![1; 16],
        subvolume_uuid: token.to_vec(),
        continuity_token: token.to_vec(),
        retention_token: token.to_vec(),
        interpretation_input_fingerprint: input_fingerprint.to_vec(),
    }
}

/// Settings specific to the tree state of the [`LocalWorkingCopy`] backend.
#[derive(Clone, Debug)]
pub struct TreeStateSettings {
    /// Conflict marker style to use when materializing files or when checking
    /// changed files.
    pub conflict_marker_style: ConflictMarkerStyle,
    /// Configuring auto-converting CRLF line endings into LF when you add a
    /// file to the backend, and vice versa when it checks out code onto your
    /// filesystem.
    pub eol_conversion_mode: EolConversionMode,
    /// Whether to ignore changes to the executable bit for files on Unix.
    pub exec_change_setting: ExecChangeSetting,
    /// The fsmonitor (e.g. Watchman) to use, if any.
    pub fsmonitor_settings: FsmonitorSettings,
}

impl TreeStateSettings {
    /// Create [`TreeStateSettings`] from [`UserSettings`].
    pub fn try_from_user_settings(user_settings: &UserSettings) -> Result<Self, ConfigGetError> {
        Ok(Self {
            conflict_marker_style: user_settings.get("ui.conflict-marker-style")?,
            eol_conversion_mode: EolConversionMode::try_from_settings(user_settings)?,
            exec_change_setting: user_settings.get("working-copy.exec-bit-change")?,
            fsmonitor_settings: FsmonitorSettings::from_settings(user_settings)?,
        })
    }
}

pub struct TreeState {
    store: Arc<Store>,
    working_copy_path: PathBuf,
    state_path: PathBuf,
    tree: MergedTree,
    /// Scope-local semantic membership index rebuilt from `tree` for a scan.
    /// It is deliberately ephemeral and never serialized.
    tracked_paths: TrackedPathsMap,
    // Currently only path prefixes
    sparse_patterns: Vec<RepoPathBuf>,
    symlink_support: bool,

    /// Compact journal state. Per-path file states are never persisted in the
    /// new format; these fields describe whether an authoritative filesystem
    /// baseline may be reused.
    journal_phase: crate::protos::local_working_copy::WorkingCopyStatePhase,
    journal_generation: u64,
    pending_tree: Option<MergedTree>,
    pending_sparse_patterns: Option<Vec<RepoPathBuf>>,
    baseline: Option<crate::protos::local_working_copy::AwacsSnapshotBaseline>,
    pending_baseline: Option<crate::protos::local_working_copy::AwacsSnapshotBaseline>,
    awacs_baseline_owner_id: Vec<u8>,
    transition_id: Vec<u8>,
    no_baseline_reason: String,
    mutation_kind: String,

    /// The most recent mutable filesystem-monitor cursor. AWACS uses the
    /// typed snapshot baseline above instead of this Watchman-shaped state.
    fsmonitor_cursor: Option<crate::protos::local_working_copy::FsmonitorCursor>,

    conflict_marker_style: ConflictMarkerStyle,
    exec_policy: ExecChangePolicy,
    fsmonitor_settings: FsmonitorSettings,
    target_eol_strategy: TargetEolStrategy,
}

/// Small, human-facing summary of the durable working-copy journal.
///
/// This deliberately exposes no per-path scan metadata. It exists so debug
/// callers can tell whether an incremental baseline is reusable and why a
/// command will fall back to a full scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingCopyJournalStatus {
    pub phase: &'static str,
    pub generation: u64,
    pub baseline_backend: Option<String>,
    pub baseline_snapshot_identity: Option<Vec<u8>>,
    /// Whether the backend durably pins the baseline or proves it on demand.
    pub baseline_retention: Option<&'static str>,
    pub fallback_reason: Option<String>,
    pub pending_mutation: Option<String>,
}

#[derive(Debug, Error)]
pub enum TreeStateError {
    #[error("Reading tree state from {path}")]
    ReadTreeState { path: PathBuf, source: io::Error },
    #[error("Decoding tree state from {path}")]
    DecodeTreeState {
        path: PathBuf,
        source: prost::DecodeError,
    },
    #[error("Writing tree state to temporary file {path}")]
    WriteTreeState { path: PathBuf, source: io::Error },
    #[error("Persisting tree state to file {path}")]
    PersistTreeState { path: PathBuf, source: io::Error },
    #[error("Invalid working-copy state at {path}: {message}")]
    InvalidWorkingCopyState { path: PathBuf, message: String },
    #[error("Filesystem monitor error")]
    Fsmonitor {
        user_message: String,
        #[source]
        err: Box<dyn Error + Send + Sync>,
    },
}

const WORKING_COPY_STATE_MAGIC: &[u8] = b"\0JJ-WORKING-COPY-STATE\0v1\n";
const WORKING_COPY_STATE_FORMAT_VERSION: u32 = 2;
const SUBVOLUME_MODE_MARKER: &str = "subvolume_mode";
#[cfg(all(target_os = "linux", feature = "awacs"))]
const AWACS_ADOPTION_SEED_MARKER: &str = "awacs-adoption-seed";

fn is_snapshot_mode(state_path: &Path) -> bool {
    state_path.join(SUBVOLUME_MODE_MARKER).is_file()
}

fn snapshot_mode_requires_baseline(state_path: &Path) -> bool {
    fs::read(state_path.join(SUBVOLUME_MODE_MARKER))
        .is_ok_and(|marker| marker == b"snapshot-backed\n")
}

fn is_valid_awacs_snapshot_baseline(
    baseline: &crate::protos::local_working_copy::AwacsSnapshotBaseline,
) -> bool {
    baseline.filesystem_uuid.len() == 16
        && baseline.subvolume_uuid.len() == 16
        && !baseline.continuity_token.is_empty()
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn append_uuid_text(output: &mut Vec<u8>, bytes: &[u8]) {
    debug_assert_eq!(bytes.len(), 16);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            output.push(b'-');
        }
        output.push(HEX[usize::from(byte >> 4)]);
        output.push(HEX[usize::from(byte & 0x0f)]);
    }
}

/// Returns whether strict subvolume mode has the committed AWACS baseline
/// required by ordinary commands.
///
/// `jj util subvolume enable` uses this before loading the working copy so it
/// can recognize and repair a previously interrupted enable. Malformed state
/// is still rejected instead of being silently treated as recoverable.
pub fn snapshot_mode_has_committed_baseline(state_path: &Path) -> Result<bool, TreeStateError> {
    if !snapshot_mode_requires_baseline(state_path) {
        return Ok(false);
    }
    for journal_name in ["checkout", "working_copy_state"] {
        let journal_path = state_path.join(journal_name);
        let bytes = match fs::read(&journal_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(TreeStateError::ReadTreeState {
                    path: journal_path,
                    source,
                });
            }
        };
        let Some(proto) = decode_working_copy_state(&journal_path, &bytes)? else {
            continue;
        };
        let baseline = match proto.phase() {
            crate::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline => {
                proto.baseline
            }
            crate::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization => {
                // begin_materialization() moves the last committed A
                // baseline here before publishing an operation or touching
                // the live root. It remains safe for an incremental recovery
                // scan even if the checkout was interrupted halfway through.
                proto.pending_baseline
            }
            _ => None,
        };
        let Some(baseline) = baseline else {
            return Ok(false);
        };
        return Ok(is_valid_awacs_snapshot_baseline(&baseline));
    }
    Ok(false)
}

fn invalid_working_copy_state(path: &Path, message: impl Into<String>) -> TreeStateError {
    TreeStateError::InvalidWorkingCopyState {
        path: path.to_owned(),
        message: message.into(),
    }
}

fn decode_working_copy_state(
    path: &Path,
    bytes: &[u8],
) -> Result<Option<crate::protos::local_working_copy::WorkingCopyState>, TreeStateError> {
    let Some(bytes) = bytes.strip_prefix(WORKING_COPY_STATE_MAGIC) else {
        return Ok(None);
    };
    let proto =
        crate::protos::local_working_copy::WorkingCopyState::decode(bytes).map_err(|err| {
            TreeStateError::DecodeTreeState {
                path: path.to_owned(),
                source: err,
            }
        })?;
    if proto.format_version != WORKING_COPY_STATE_FORMAT_VERSION {
        return Err(invalid_working_copy_state(
            path,
            format!("unsupported format version {}", proto.format_version),
        ));
    }
    if proto.tree_ids.is_empty() {
        return Err(invalid_working_copy_state(path, "tree IDs are empty"));
    }
    Ok(Some(proto))
}

#[cfg(unix)]
fn sync_state_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_state_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

impl TreeState {
    pub fn working_copy_path(&self) -> &Path {
        &self.working_copy_path
    }

    pub fn current_tree(&self) -> &MergedTree {
        &self.tree
    }

    pub fn sparse_patterns(&self) -> &Vec<RepoPathBuf> {
        &self.sparse_patterns
    }

    /// Returns the last committed A baseline retained by an interrupted
    /// strict subvolume materialization.
    ///
    /// The live root may be anywhere between the old and intended trees, so
    /// this baseline is only useful as input to an A -> live recovery scan.
    /// It must not be serialized as a clean baseline before that scan commits
    /// its replacement B baseline.
    fn recoverable_pending_baseline(
        &self,
    ) -> Option<&crate::protos::local_working_copy::AwacsSnapshotBaseline> {
        (snapshot_mode_requires_baseline(&self.state_path)
            && self.journal_phase
                == crate::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization)
            .then_some(self.pending_baseline.as_ref())
            .flatten()
            .filter(|baseline| is_valid_awacs_snapshot_baseline(baseline))
    }

    /// Returns the baseline that may safely drive the next immutable scan.
    ///
    /// A clean baseline pairs directly with the current semantic tree. A
    /// recoverable pending baseline also pairs with it, because loading keeps
    /// the old serialized tree rather than trusting the interrupted intended
    /// tree.
    fn scan_baseline(&self) -> Option<&crate::protos::local_working_copy::AwacsSnapshotBaseline> {
        if self.journal_phase
            == crate::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline
        {
            self.baseline.as_ref()
        } else {
            self.recoverable_pending_baseline()
        }
    }

    fn journal_status(&self) -> WorkingCopyJournalStatus {
        let phase = match self.journal_phase {
            crate::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline => "no-baseline",
            crate::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline => {
                "clean-baseline"
            }
            crate::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization => {
                "pending-materialization"
            }
            crate::protos::local_working_copy::WorkingCopyStatePhase::PendingBaselineCommit => {
                "pending-baseline-commit"
            }
        };
        let scan_baseline = self.scan_baseline();
        WorkingCopyJournalStatus {
            phase,
            generation: self.journal_generation,
            baseline_backend: scan_baseline.map(|_| "awacs".to_owned()),
            baseline_snapshot_identity: scan_baseline
                .map(|baseline| baseline.subvolume_uuid.clone()),
            baseline_retention: scan_baseline.map(|baseline| {
                if baseline.retention_token.is_empty() {
                    "best-effort"
                } else {
                    "hard-pinned"
                }
            }),
            fallback_reason: (self.journal_phase
                == crate::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline
                && !self.no_baseline_reason.is_empty())
            .then(|| self.no_baseline_reason.clone()),
            pending_mutation: (!self.mutation_kind.is_empty()).then(|| self.mutation_kind.clone()),
        }
    }

    fn set_tree_from_serialized_parts(
        &mut self,
        tree_ids: Vec<Vec<u8>>,
        conflict_labels: Vec<String>,
    ) -> Result<(), TreeStateError> {
        if tree_ids.is_empty() || tree_ids.len() % 2 == 0 {
            return Err(invalid_working_copy_state(
                &self.state_path.join("checkout"),
                "tree IDs must contain an odd, non-empty merge shape",
            ));
        }
        if !conflict_labels.is_empty()
            && (tree_ids.len() == 1 || conflict_labels.len() != tree_ids.len())
        {
            return Err(invalid_working_copy_state(
                &self.state_path.join("checkout"),
                "conflict labels do not match tree-ID merge shape",
            ));
        }
        let tree_ids_builder: MergeBuilder<TreeId> =
            tree_ids.into_iter().map(TreeId::new).collect();
        self.tree = MergedTree::new(
            self.store.clone(),
            tree_ids_builder.build(),
            ConflictLabels::from_vec(conflict_labels),
        );
        Ok(())
    }

    fn read_working_copy_state(
        &mut self,
        state_path: &Path,
        proto: crate::protos::local_working_copy::WorkingCopyState,
    ) -> Result<(), TreeStateError> {
        let phase = crate::protos::local_working_copy::WorkingCopyStatePhase::try_from(proto.phase)
            .map_err(|_| {
                invalid_working_copy_state(
                    state_path,
                    format!("unsupported journal phase {}", proto.phase),
                )
            })?;
        self.set_tree_from_serialized_parts(proto.tree_ids, proto.conflict_labels)?;
        self.sparse_patterns =
            sparse_patterns_from_proto(state_path, proto.sparse_patterns.as_ref())?;
        self.fsmonitor_cursor = proto.fsmonitor_cursor;
        self.journal_generation = proto.generation;
        self.journal_phase = phase;
        self.baseline = proto.baseline;
        self.pending_baseline = proto.pending_baseline;
        self.awacs_baseline_owner_id = if proto.awacs_baseline_owner_id.len() == 16 {
            proto.awacs_baseline_owner_id
        } else {
            rand::random::<[u8; 16]>().to_vec()
        };
        self.transition_id = proto.transition_id;
        self.no_baseline_reason = proto.no_baseline_reason;
        self.mutation_kind = proto.mutation_kind;
        self.pending_sparse_patterns = if proto.pending_sparse_patterns.is_empty() {
            None
        } else {
            Some(
                proto
                    .pending_sparse_patterns
                    .iter()
                    .map(|prefix| {
                        RepoPathBuf::from_internal_string(prefix).map_err(|err| {
                            invalid_working_copy_state(
                                state_path,
                                format!("invalid pending sparse prefix: {err}"),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        let recoverable_pending_materialization = self.recoverable_pending_baseline().is_some();
        if matches!(
            self.journal_phase,
            crate::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization
                | crate::protos::local_working_copy::WorkingCopyStatePhase::PendingBaselineCommit
        ) {
            self.fsmonitor_cursor = None;
            if !recoverable_pending_materialization {
                // A non-strict transition has no retained immutable A, so
                // keep the intended tree if it was durably recorded and force
                // the next snapshot to reconcile by a full scan.
                if !proto.pending_tree_ids.is_empty() {
                    self.set_tree_from_serialized_parts(
                        proto.pending_tree_ids,
                        proto.pending_conflict_labels,
                    )?;
                }
                if let Some(pending_sparse_patterns) = self.pending_sparse_patterns.take() {
                    self.sparse_patterns = pending_sparse_patterns;
                }
                self.set_no_baseline("recovered interrupted working-copy transition");
            }
        }
        if self.journal_phase
            != crate::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline
        {
            self.fsmonitor_cursor = None;
            self.baseline = None;
        } else {
            let Some(baseline) = self.baseline.as_ref() else {
                return Err(invalid_working_copy_state(
                    state_path,
                    "clean baseline is missing retained snapshot identity",
                ));
            };
            if !is_valid_awacs_snapshot_baseline(baseline) {
                return Err(invalid_working_copy_state(
                    state_path,
                    "clean baseline is missing AWACS snapshot identity or continuity token",
                ));
            }
        }
        if snapshot_mode_requires_baseline(&self.state_path)
            && self.journal_phase
                != crate::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline
            && !recoverable_pending_materialization
        {
            return Err(invalid_working_copy_state(
                state_path,
                "subvolume mode requires a committed AWACS snapshot baseline",
            ));
        }
        Ok(())
    }

    fn working_copy_state_proto(
        &self,
        checkout_state: Option<&CheckoutState>,
    ) -> Result<crate::protos::local_working_copy::WorkingCopyState, TreeStateError> {
        let mut sparse_patterns = crate::protos::local_working_copy::SparsePatterns::default();
        for path in &self.sparse_patterns {
            sparse_patterns
                .prefixes
                .push(path.as_internal_file_string().to_owned());
        }
        let (operation_id, workspace_name) = checkout_state.map_or_else(
            || (Vec::new(), String::new()),
            |state| {
                (
                    state.operation_id.to_bytes(),
                    (*state.workspace_name).into(),
                )
            },
        );
        let generation = self.journal_generation.checked_add(1).ok_or_else(|| {
            invalid_working_copy_state(
                &self.state_path.join("checkout"),
                "journal generation overflowed",
            )
        })?;
        let (pending_tree_ids, pending_conflict_labels) = self.pending_tree.as_ref().map_or_else(
            || (Vec::new(), Vec::new()),
            |tree| {
                (
                    tree.tree_ids().iter().map(|id| id.to_bytes()).collect(),
                    tree.labels().as_slice().to_owned(),
                )
            },
        );
        let pending_sparse_patterns = self
            .pending_sparse_patterns
            .as_ref()
            .map(|patterns| {
                patterns
                    .iter()
                    .map(|path| path.as_internal_file_string().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        Ok(crate::protos::local_working_copy::WorkingCopyState {
            format_version: WORKING_COPY_STATE_FORMAT_VERSION,
            operation_id,
            workspace_name,
            tree_ids: self
                .tree
                .tree_ids()
                .iter()
                .map(|id| id.to_bytes())
                .collect(),
            conflict_labels: self.tree.labels().as_slice().to_owned(),
            sparse_patterns: Some(sparse_patterns),
            fsmonitor_cursor: if self.journal_phase
                == crate::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline
            {
                self.fsmonitor_cursor.clone()
            } else {
                None
            },
            generation,
            phase: self.journal_phase as i32,
            pending_tree_ids,
            pending_conflict_labels,
            baseline: self.baseline.clone(),
            pending_baseline: self.pending_baseline.clone(),
            transition_id: self.transition_id.clone(),
            no_baseline_reason: self.no_baseline_reason.clone(),
            mutation_kind: self.mutation_kind.clone(),
            pending_sparse_patterns,
            awacs_baseline_owner_id: self.awacs_baseline_owner_id.clone(),
        })
    }

    fn write_working_copy_state(
        &mut self,
        checkout_state: Option<&CheckoutState>,
    ) -> Result<(), TreeStateError> {
        let state_path = self.state_path.join(if checkout_state.is_some() {
            "checkout"
        } else {
            "working_copy_state"
        });
        let wrap_write_err = |source| TreeStateError::WriteTreeState {
            path: state_path.clone(),
            source,
        };
        let proto = self.working_copy_state_proto(checkout_state)?;
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        self.clear_snapshot_adoption_seed()?;
        let mut bytes = WORKING_COPY_STATE_MAGIC.to_vec();
        bytes.extend_from_slice(&proto.encode_to_vec());
        let mut temp_file = NamedTempFile::new_in(&self.state_path).map_err(wrap_write_err)?;
        temp_file
            .as_file_mut()
            .write_all(&bytes)
            .map_err(wrap_write_err)?;
        temp_file.as_file().sync_data().map_err(wrap_write_err)?;
        persist_temp_file(temp_file, &state_path).map_err(|source| {
            TreeStateError::PersistTreeState {
                path: state_path.clone(),
                source,
            }
        })?;

        // The magic-prefixed checkout journal fails old Checkout decoding
        // before an older binary can interpret a missing tree_state as empty.
        // After the new journal is durable, remove the old payload entirely.
        let tree_state_path = self.state_path.join("tree_state");
        if let Err(err) = fs::remove_file(&tree_state_path)
            && err.kind() != io::ErrorKind::NotFound
        {
            return Err(TreeStateError::WriteTreeState {
                path: tree_state_path,
                source: err,
            });
        }
        sync_state_dir(&self.state_path).map_err(wrap_write_err)?;
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        self.publish_snapshot_adoption_seed(&proto)?;
        self.journal_generation = proto.generation;
        Ok(())
    }

    #[cfg(all(target_os = "linux", feature = "awacs"))]
    fn clear_snapshot_adoption_seed(&self) -> Result<(), TreeStateError> {
        let path = self.state_path.join(AWACS_ADOPTION_SEED_MARKER);
        if let Err(source) = fs::remove_file(&path)
            && source.kind() != io::ErrorKind::NotFound
        {
            return Err(TreeStateError::WriteTreeState { path, source });
        }
        Ok(())
    }

    /// Publishes the small cross-process handoff that lets a Git-mediated
    /// Btrfs clone become a JJ workspace later.
    ///
    /// The compact journal remains JJ's source of semantic tree state. This
    /// record only says that the journal was committed at a clean immutable
    /// AWACS baseline, and names that baseline so an external worktree helper
    /// can reject stale or half-written state before Git creates anything.
    /// Removing the prior record before every journal write makes concurrent
    /// readers observe either one complete generation or no transferable
    /// state; they never pair a new journal with an old baseline identity.
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    fn publish_snapshot_adoption_seed(
        &self,
        proto: &crate::protos::local_working_copy::WorkingCopyState,
    ) -> Result<(), TreeStateError> {
        if proto.phase() != crate::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline
        {
            return Ok(());
        }
        let Some(baseline) = proto
            .baseline
            .as_ref()
            .filter(|baseline| is_valid_awacs_snapshot_baseline(baseline))
        else {
            return Ok(());
        };
        let path = self.state_path.join(AWACS_ADOPTION_SEED_MARKER);
        let mut bytes = b"jj-awacs-adoption-v2:".to_vec();
        append_uuid_text(&mut bytes, &baseline.filesystem_uuid);
        bytes.push(b':');
        append_uuid_text(&mut bytes, &baseline.subvolume_uuid);
        bytes.push(b':');
        append_uuid_text(&mut bytes, &proto.awacs_baseline_owner_id);
        bytes.push(b'\n');
        let wrap_write_err = |source| TreeStateError::WriteTreeState {
            path: path.clone(),
            source,
        };
        let mut temp_file = NamedTempFile::new_in(&self.state_path).map_err(wrap_write_err)?;
        temp_file
            .as_file_mut()
            .write_all(&bytes)
            .map_err(wrap_write_err)?;
        temp_file.as_file().sync_data().map_err(wrap_write_err)?;
        persist_temp_file(temp_file, &path).map_err(|source| TreeStateError::PersistTreeState {
            path: path.clone(),
            source,
        })?;
        sync_state_dir(&self.state_path).map_err(wrap_write_err)?;
        Ok(())
    }

    fn sparse_matcher(&self) -> Box<dyn Matcher> {
        Box::new(PrefixMatcher::new(&self.sparse_patterns))
    }

    /// Rebuilds a bounded, in-memory scan index from the semantic tree. This
    /// replaces the durable per-path table: the index exists only for the
    /// current scan and all inspected paths are read from the scan root.
    fn rebuild_ephemeral_tracked_paths(&mut self, scope: &ScanScope) -> Result<(), SnapshotError> {
        let mut entries = Vec::new();
        let mut push_value = |path: RepoPathBuf, value: MergedTreeValue| {
            if !value.is_tree() && !value.is_absent() {
                let kind = if matches!(value.as_normal(), Some(TreeValue::GitSubmodule(_))) {
                    TrackedKind::GitSubmodule
                } else {
                    TrackedKind::FileLike
                };
                entries.push(TrackedPathEntry { path, kind });
            }
        };
        match scope {
            ScanScope::Full => {
                for (path, result) in self.tree.entries_matching(self.sparse_matcher().as_ref()) {
                    push_value(path, result?);
                }
            }
            ScanScope::Changed { exact, prefixes } => {
                for path in exact {
                    // An exact delta path can replace a semantic directory
                    // with a file (or remove it). Include X's descendants so
                    // the directed scanner can emit their deletions without
                    // walking the parent directory.
                    let matcher = PrefixMatcher::new([path]);
                    for (tracked_path, result) in self.tree.entries_matching(&matcher) {
                        push_value(tracked_path, result?);
                    }
                }
                for prefix in prefixes {
                    let matcher = PrefixMatcher::new([prefix]);
                    for (path, result) in self.tree.entries_matching(&matcher) {
                        push_value(path, result?);
                    }
                }
            }
        }
        entries.sort_unstable_by(|entry1, entry2| entry1.path.cmp(&entry2.path));
        entries.dedup_by(|entry1, entry2| entry1.path == entry2.path);
        self.tracked_paths = TrackedPathsMap::from_entries(entries);
        Ok(())
    }

    pub fn init(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        tree_state_settings: &TreeStateSettings,
    ) -> Result<Self, TreeStateError> {
        let mut wc = Self::empty(store, working_copy_path, state_path, tree_state_settings);
        wc.save()?;
        Ok(wc)
    }

    /// Like `init` but does not persist the initial empty working-copy
    /// journal. Use when the caller will save state itself only after a
    /// successful operation (e.g. to use journal absence as a dirty marker).
    pub fn init_without_saving(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        tree_state_settings: &TreeStateSettings,
    ) -> Self {
        Self::empty(store, working_copy_path, state_path, tree_state_settings)
    }

    fn empty(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        TreeStateSettings {
            conflict_marker_style,
            eol_conversion_mode,
            exec_change_setting,
            fsmonitor_settings,
        }: &TreeStateSettings,
    ) -> Self {
        let exec_policy = ExecChangePolicy::new(*exec_change_setting, &state_path);
        Self {
            store: store.clone(),
            working_copy_path,
            state_path,
            tree: store.empty_merged_tree(),
            tracked_paths: TrackedPathsMap::default(),
            sparse_patterns: vec![RepoPathBuf::root()],
            symlink_support: check_symlink_support().unwrap_or(false),
            journal_phase: crate::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline,
            journal_generation: 0,
            pending_tree: None,
            pending_sparse_patterns: None,
            baseline: None,
            pending_baseline: None,
            awacs_baseline_owner_id: rand::random::<[u8; 16]>().to_vec(),
            transition_id: Vec::new(),
            no_baseline_reason: "uninitialized".to_owned(),
            mutation_kind: String::new(),
            fsmonitor_cursor: None,
            conflict_marker_style: *conflict_marker_style,
            exec_policy,
            fsmonitor_settings: fsmonitor_settings.clone(),
            target_eol_strategy: TargetEolStrategy::new(*eol_conversion_mode),
        }
    }

    pub fn load(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        tree_state_settings: &TreeStateSettings,
    ) -> Result<Self, TreeStateError> {
        if is_snapshot_mode(&state_path) {
            for journal_name in ["checkout", "working_copy_state"] {
                let journal_path = state_path.join(journal_name);
                let bytes = match fs::read(&journal_path) {
                    Ok(bytes) => bytes,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => {
                        return Err(TreeStateError::ReadTreeState {
                            path: journal_path,
                            source: err,
                        });
                    }
                };
                if let Some(proto) = decode_working_copy_state(&journal_path, &bytes)? {
                    return Self::from_working_copy_state(
                        store,
                        working_copy_path,
                        state_path,
                        tree_state_settings,
                        &journal_path,
                        proto,
                    );
                }
            }
        }
        let tree_state_path = state_path.join("tree_state");
        let file = match File::open(&tree_state_path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Self::init(store, working_copy_path, state_path, tree_state_settings);
            }
            Err(err) => {
                return Err(TreeStateError::ReadTreeState {
                    path: tree_state_path,
                    source: err,
                });
            }
            Ok(file) => file,
        };

        let mut wc = Self::empty(store, working_copy_path, state_path, tree_state_settings);
        wc.read(&tree_state_path, file)?;
        Ok(wc)
    }

    fn from_working_copy_state(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        tree_state_settings: &TreeStateSettings,
        journal_path: &Path,
        proto: crate::protos::local_working_copy::WorkingCopyState,
    ) -> Result<Self, TreeStateError> {
        let mut wc = Self::empty(store, working_copy_path, state_path, tree_state_settings);
        wc.read_working_copy_state(journal_path, proto)?;
        Ok(wc)
    }

    fn read(&mut self, tree_state_path: &Path, mut file: File) -> Result<(), TreeStateError> {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|err| TreeStateError::ReadTreeState {
                path: tree_state_path.to_owned(),
                source: err,
            })?;
        self.read_legacy_tree_state(tree_state_path, &buf)?;
        Ok(())
    }

    fn read_legacy_tree_state(
        &mut self,
        tree_state_path: &Path,
        bytes: &[u8],
    ) -> Result<(), TreeStateError> {
        let proto = crate::protos::local_working_copy::TreeState::decode(bytes).map_err(|err| {
            TreeStateError::DecodeTreeState {
                path: tree_state_path.to_owned(),
                source: err,
            }
        })?;
        #[expect(deprecated)]
        if proto.tree_ids.is_empty() {
            self.tree = MergedTree::resolved(
                self.store.clone(),
                TreeId::new(proto.legacy_tree_id.clone()),
            );
        } else {
            let tree_ids_builder: MergeBuilder<TreeId> = proto
                .tree_ids
                .iter()
                .map(|id| TreeId::new(id.clone()))
                .collect();
            self.tree = MergedTree::new(
                self.store.clone(),
                tree_ids_builder.build(),
                ConflictLabels::from_vec(proto.conflict_labels),
            );
        }
        // The old row vector is intentionally discarded. The semantic tree is
        // the tracked-path index for the next scan.
        self.sparse_patterns =
            sparse_patterns_from_proto(tree_state_path, proto.sparse_patterns.as_ref())?;
        #[expect(deprecated)]
        let legacy_watchman_clock = proto.watchman_clock;
        self.fsmonitor_cursor = proto
            .fsmonitor_cursor
            .or_else(|| legacy_watchman_clock.map(watchman_cursor));
        Ok(())
    }

    fn save_with_checkout(&mut self, checkout_state: &CheckoutState) -> Result<(), TreeStateError> {
        self.write_working_copy_state(Some(checkout_state))
    }

    pub fn save(&mut self) -> Result<(), TreeStateError> {
        if is_snapshot_mode(&self.state_path) {
            self.write_working_copy_state(None)
        } else {
            self.write_legacy_tree_state()
        }
    }

    fn write_legacy_tree_state(&mut self) -> Result<(), TreeStateError> {
        let mut proto = crate::protos::local_working_copy::TreeState {
            tree_ids: self
                .tree
                .tree_ids()
                .iter()
                .map(|id| id.to_bytes())
                .collect(),
            conflict_labels: self.tree.labels().as_slice().to_owned(),
            fsmonitor_cursor: self.fsmonitor_cursor.clone(),
            ..Default::default()
        };
        let mut sparse_patterns = crate::protos::local_working_copy::SparsePatterns::default();
        for path in &self.sparse_patterns {
            sparse_patterns
                .prefixes
                .push(path.as_internal_file_string().to_owned());
        }
        proto.sparse_patterns = Some(sparse_patterns);
        let target_path = self.state_path.join("tree_state");
        let wrap_write_err = |source| TreeStateError::WriteTreeState {
            path: target_path.clone(),
            source,
        };
        let mut temp_file = NamedTempFile::new_in(&self.state_path).map_err(wrap_write_err)?;
        temp_file
            .as_file_mut()
            .write_all(&proto.encode_to_vec())
            .map_err(wrap_write_err)?;
        persist_temp_file(temp_file, &target_path)
            .map(|_| ())
            .map_err(|source| TreeStateError::PersistTreeState {
                path: target_path,
                source,
            })
    }

    #[cfg(feature = "watchman")]
    fn watchman_clock(&self) -> Option<&crate::protos::local_working_copy::WatchmanClock> {
        use crate::protos::local_working_copy::fsmonitor_cursor::Cursor;
        match self.fsmonitor_cursor.as_ref()?.cursor.as_ref()? {
            Cursor::Watchman(clock) => Some(clock),
        }
    }

    #[cfg(feature = "watchman")]
    fn set_watchman_clock(
        &mut self,
        clock: Option<crate::protos::local_working_copy::WatchmanClock>,
    ) {
        self.fsmonitor_cursor = clock.map(watchman_cursor);
    }

    fn set_no_baseline(&mut self, reason: impl Into<String>) {
        self.journal_phase = crate::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline;
        self.fsmonitor_cursor = None;
        self.baseline = None;
        self.pending_baseline = None;
        self.transition_id.clear();
        self.pending_tree = None;
        self.pending_sparse_patterns = None;
        self.no_baseline_reason = reason.into();
        self.mutation_kind.clear();
    }

    fn publish_scan_baseline(
        &mut self,
        cursor: Option<crate::protos::local_working_copy::FsmonitorCursor>,
        baseline: Option<crate::protos::local_working_copy::AwacsSnapshotBaseline>,
    ) {
        if baseline.is_some() {
            self.fsmonitor_cursor = cursor;
            self.baseline = baseline;
            self.pending_baseline = None;
            self.transition_id.clear();
            self.pending_tree = None;
            self.pending_sparse_patterns = None;
            self.no_baseline_reason.clear();
            self.mutation_kind.clear();
            self.journal_phase =
                crate::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline;
        } else {
            self.set_no_baseline("backend has no retained authoritative baseline");
        }
    }

    fn begin_materialization(
        &mut self,
        intended_tree: &MergedTree,
        intended_sparse_patterns: Option<Vec<RepoPathBuf>>,
        mutation_kind: &str,
    ) {
        self.fsmonitor_cursor = None;
        // Preserve the committed A cursor while jj performs a controlled
        // materialization. Once the writes finish, AWACS can atomically
        // advance to B without a filesystem traversal: every A..B path was
        // written (or removed) by this mutation, and unchanged untracked
        // paths were already accounted for when A was committed.
        // CLI transactions call prepare_checkout() before publication and
        // then check_out() after publication. Keep the same A binding across
        // both halves instead of consuming it again on the second call.
        if self.pending_baseline.is_none() {
            self.pending_baseline = self.baseline.take();
        }
        self.transition_id.clear();
        self.pending_tree = Some(intended_tree.clone());
        self.pending_sparse_patterns = intended_sparse_patterns;
        self.mutation_kind = mutation_kind.to_owned();
        self.journal_phase =
            crate::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization;
    }

    fn finish_materialization(&mut self, reason: &str) {
        self.pending_tree = None;
        self.pending_sparse_patterns = None;
        self.set_no_baseline(reason);
    }

    async fn finish_snapshot_materialization(
        &mut self,
    ) -> Result<Option<PendingScan>, SnapshotError> {
        let previous_baseline =
            self.pending_baseline
                .take()
                .ok_or_else(|| SnapshotError::Other {
                    message: "Failed to advance AWACS snapshot baseline".to_owned(),
                    err: "subvolume mode has no committed baseline before materialization".into(),
                })?;
        let input_fingerprint: [u8; 32] = previous_baseline
            .interpretation_input_fingerprint
            .as_slice()
            .try_into()
            .map_err(|_| SnapshotError::Other {
                message: "Failed to advance AWACS snapshot baseline".to_owned(),
                err: "committed baseline has an invalid input fingerprint".into(),
            })?;

        // Temporarily restore A as the current committed baseline so
        // make_snapshot_scan() asks AWACS for the next immutable B snapshot.
        // We deliberately do not traverse B here. The materializer itself
        // performed the complete, deterministic A..B filesystem mutation, so
        // publishing B merely re-pairs an authoritative snapshot with the
        // already-updated semantic tree.
        self.baseline = Some(previous_baseline);
        self.journal_phase =
            crate::protos::local_working_copy::WorkingCopyStatePhase::CleanBaseline;
        let SnapshotScan {
            fsmonitor_cursor,
            baseline,
            completion,
            ..
        } = self
            .make_snapshot_scan(&self.fsmonitor_settings, Some(input_fingerprint))
            .await?;
        if baseline.is_none() || completion.is_none() {
            return Err(SnapshotError::Other {
                message: "Failed to advance AWACS snapshot baseline".to_owned(),
                err: "subvolume mode requires an authoritative AWACS snapshot lease".into(),
            });
        }
        self.pending_tree = None;
        self.pending_sparse_patterns = None;
        self.publish_scan_baseline(fsmonitor_cursor, baseline);
        Ok(completion)
    }

    fn clear_fsmonitor_cursor(&mut self) -> bool {
        let changed = self.journal_phase
            != crate::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline
            || self.fsmonitor_cursor.is_some()
            || self.baseline.is_some();
        self.set_no_baseline("filesystem-monitor baseline invalidated");
        changed
    }

    fn cursor_matches_settings(&self) -> bool {
        use crate::protos::local_working_copy::fsmonitor_cursor::Cursor;
        match (&self.fsmonitor_settings, self.fsmonitor_cursor.as_ref()) {
            (_, None) => true,
            (FsmonitorSettings::Watchman(_), Some(cursor)) => {
                matches!(cursor.cursor, Some(Cursor::Watchman(_)))
            }
            (
                FsmonitorSettings::Awacs(_)
                | FsmonitorSettings::TestAwacs { .. }
                | FsmonitorSettings::Test { .. }
                | FsmonitorSettings::None,
                Some(_),
            ) => false,
        }
    }

    fn awacs_baseline_matches_input(
        &self,
        input_fingerprint: Option<[u8; 32]>,
        compatible_input_fingerprints: &[[u8; 32]],
    ) -> bool {
        if !matches!(
            self.fsmonitor_settings,
            FsmonitorSettings::Awacs(_) | FsmonitorSettings::TestAwacs { .. }
        ) {
            return true;
        }
        let Some(baseline) = self.scan_baseline() else {
            return true;
        };
        input_fingerprint.is_some_and(|fingerprint| {
            baseline.interpretation_input_fingerprint == fingerprint
                || compatible_input_fingerprints
                    .iter()
                    .any(|compatible| baseline.interpretation_input_fingerprint == *compatible)
        })
    }

    #[cfg(feature = "watchman")]
    #[instrument(skip(self))]
    pub async fn query_watchman(
        &self,
        config: &WatchmanConfig,
    ) -> Result<(watchman::Clock, Option<Vec<PathBuf>>), TreeStateError> {
        let previous_clock = self.watchman_clock().cloned().map(watchman::Clock::from);

        let tokio_fn = async || {
            let result = async {
                let fsmonitor = watchman::Fsmonitor::init(&self.working_copy_path, config).await?;
                fsmonitor.query_changed_files(previous_clock).await
            }
            .await;
            result
                .inspect_err(|err| tracing::warn!(?err, "Watchman query failed"))
                .map_err(|err| TreeStateError::Fsmonitor {
                    user_message: err.detailed_message(),
                    err: Box::new(err),
                })
        };

        match tokio::runtime::Handle::try_current() {
            Ok(_handle) => tokio_fn().await,
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| TreeStateError::Fsmonitor {
                        user_message: err.to_string(),
                        err: Box::new(err),
                    })?;
                runtime.block_on(tokio_fn())
            }
        }
    }

    /// Records a synchronized Watchman clock after the caller has established
    /// that the current tree state matches the filesystem.
    #[cfg(feature = "watchman")]
    #[instrument(skip(self))]
    pub async fn mark_watchman_baseline(
        &mut self,
        config: &WatchmanConfig,
    ) -> Result<(), TreeStateError> {
        let tokio_fn = async || {
            let result = async {
                let fsmonitor = watchman::Fsmonitor::init(&self.working_copy_path, config).await?;
                fsmonitor.clock().await
            }
            .await;
            result
                .inspect_err(|err| tracing::warn!(?err, "Watchman clock failed"))
                .map_err(|err| TreeStateError::Fsmonitor {
                    user_message: err.detailed_message(),
                    err: Box::new(err),
                })
        };
        let clock = match tokio::runtime::Handle::try_current() {
            Ok(_handle) => tokio_fn().await?,
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| TreeStateError::Fsmonitor {
                        user_message: err.to_string(),
                        err: Box::new(err),
                    })?;
                runtime.block_on(tokio_fn())?
            }
        };
        self.set_watchman_clock(Some(clock.into()));
        Ok(())
    }

    #[cfg(feature = "watchman")]
    #[instrument(skip(self))]
    pub async fn is_watchman_trigger_registered(
        &self,
        config: &WatchmanConfig,
    ) -> Result<bool, TreeStateError> {
        let tokio_fn = async || {
            let result = async {
                let fsmonitor = watchman::Fsmonitor::init(&self.working_copy_path, config).await?;
                fsmonitor.is_trigger_registered().await
            }
            .await;
            result
                .inspect_err(|err| tracing::warn!(?err, "Watchman trigger query failed"))
                .map_err(|err| TreeStateError::Fsmonitor {
                    user_message: err.detailed_message(),
                    err: Box::new(err),
                })
        };

        match tokio::runtime::Handle::try_current() {
            Ok(_handle) => tokio_fn().await,
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| TreeStateError::Fsmonitor {
                        user_message: err.to_string(),
                        err: Box::new(err),
                    })?;
                runtime.block_on(tokio_fn())
            }
        }
    }
}

/// Functions to snapshot local-disk files to the store.
impl TreeState {
    /// Look for changes to the working copy. If there are any changes, create
    /// a new tree from it.
    #[instrument(skip_all)]
    pub async fn snapshot(
        &mut self,
        options: &SnapshotOptions<'_>,
    ) -> Result<(bool, SnapshotStats), SnapshotError> {
        let (is_dirty, stats, pending_scan) = self.snapshot_with_pending(options).await?;
        // Direct TreeState callers have no working-copy transaction boundary
        // where a lease could be committed, so conservatively abort it.
        if pending_scan.is_some() {
            self.clear_fsmonitor_cursor();
        }
        drop(pending_scan);
        Ok((is_dirty, stats))
    }

    async fn snapshot_with_pending(
        &mut self,
        options: &SnapshotOptions<'_>,
    ) -> Result<(bool, SnapshotStats, Option<PendingScan>), SnapshotError> {
        let SnapshotOptions {
            base_ignores,
            scan_root_ignores,
            external_sparse_patterns,
            progress,
            start_tracking_matcher,
            force_tracking_matcher,
            max_new_file_size,
            awacs_input_fingerprint,
            awacs_compatible_input_fingerprints,
        } = options;

        let sparse_matcher = self.sparse_matcher();
        let external_sparse_matcher = external_sparse_patterns.as_ref().map(PrefixMatcher::new);

        // Only authoritative immutable backends publish a durable baseline
        // token. Mutable Test/Watchman scans intentionally leave NoBaseline,
        // so an unchanged full scan should not dirty the tiny journal merely
        // because a monitor was configured.
        let baseline_state_needs_save = matches!(
            self.fsmonitor_settings,
            FsmonitorSettings::Awacs(_) | FsmonitorSettings::TestAwacs { .. }
        );
        let mut is_dirty = baseline_state_needs_save;
        if !self.cursor_matches_settings() {
            is_dirty |= self.clear_fsmonitor_cursor();
        }
        let mut had_committed_awacs_baseline = matches!(
            self.fsmonitor_settings,
            FsmonitorSettings::Awacs(_) | FsmonitorSettings::TestAwacs { .. }
        ) && self.scan_baseline().is_some();
        if !self.awacs_baseline_matches_input(
            *awacs_input_fingerprint,
            awacs_compatible_input_fingerprints,
        ) {
            // A semantic-input change invalidates the old cursor itself, not
            // the immutable snapshot backend. Begin without a baseline so
            // AWACS returns a fresh immutable root, then commit the rebuilt
            // tree and its replacement cursor together.
            is_dirty |= self.clear_fsmonitor_cursor();
            had_committed_awacs_baseline = false;
        }
        let SnapshotScan {
            scan_root,
            scope: scan_scope,
            fsmonitor_cursor,
            baseline,
            completion,
            warning: snapshot_warning,
        } = self
            .make_snapshot_scan(&self.fsmonitor_settings, *awacs_input_fingerprint)
            .await?;
        let (scan_scope_kind, exact_path_count, prefix_count) = match &scan_scope {
            ScanScope::Full => ("full", 0, 0),
            ScanScope::Changed { exact, prefixes } => ("changed", exact.len(), prefixes.len()),
        };
        tracing::debug!(
            scan_scope_kind,
            exact_path_count,
            prefix_count,
            had_committed_awacs_baseline,
            "selected working-copy snapshot scan scope"
        );
        #[cfg(debug_assertions)]
        if completion.is_some() {
            maybe_delay_awacs_traversal_for_test();
        }
        if had_committed_awacs_baseline && scan_scope.requires_full_traversal() {
            return Err(SnapshotError::Other {
                message: "Snapshot-backed working copy refused a full scan".to_owned(),
                err: "AWACS could not prove an incremental delta from the committed baseline; run jj util subvolume enable --rebuild-baseline to rebuild the committed baseline explicitly".into(),
            });
        }
        let mut scan_root_base_ignores = base_ignores.clone();
        for relative_path in scan_root_ignores {
            scan_root_base_ignores = scan_root_base_ignores
                .chain_with_file(RepoPath::root(), scan_root.join(relative_path))?;
        }
        let fsmonitor_matcher = scan_scope.matcher();

        let scan_matcher = UnionMatcher::new(fsmonitor_matcher.as_ref(), force_tracking_matcher);
        let sparse_scan_matcher = IntersectionMatcher::new(sparse_matcher.as_ref(), scan_matcher);
        let external_sparse_matcher: &dyn Matcher = external_sparse_matcher
            .as_ref()
            .map(|matcher| matcher as &dyn Matcher)
            .unwrap_or(&EverythingMatcher);
        let matcher = IntersectionMatcher::new(external_sparse_matcher, sparse_scan_matcher);
        self.rebuild_ephemeral_tracked_paths(&scan_scope)?;
        if matcher.visit(RepoPath::root()).is_nothing() {
            // No need to load the current tree, set up channels, etc.
            self.publish_scan_baseline(fsmonitor_cursor, baseline);
            return Ok((is_dirty, SnapshotStats::default(), completion));
        }

        let (tree_entries_tx, tree_entries_rx) = channel();
        let (untracked_paths_tx, untracked_paths_rx) = channel();
        let (deleted_files_tx, deleted_files_rx) = channel();

        let traversal_result =
            trace_span!("traverse filesystem").in_scope(|| -> Result<(), SnapshotError> {
                let snapshotter = FileSnapshotter {
                    tree_state: self,
                    scan_root: &scan_root,
                    current_tree: &self.tree,
                    matcher: &matcher,
                    start_tracking_matcher,
                    force_tracking_matcher,
                    // Move tx sides so they'll be dropped at the end of the scope.
                    tree_entries_tx,
                    untracked_paths_tx,
                    deleted_files_tx,
                    error: OnceLock::new(),
                    progress: *progress,
                    max_new_file_size: *max_new_file_size,
                };
                // Here we use scope as a queue of per-directory jobs.
                rayon::scope(|scope| {
                    snapshotter.spawn_ok(scope, |scope| match &scan_scope {
                        ScanScope::Full => {
                            let directory_to_visit = DirectoryToVisit {
                                dir: RepoPathBuf::root(),
                                disk_dir: scan_root.clone(),
                                git_ignore: scan_root_base_ignores,
                                tracked_paths: self.tracked_paths.all(),
                            };
                            snapshotter.visit_directory(directory_to_visit, scope)
                        }
                        ScanScope::Changed { exact, prefixes } => snapshotter.visit_changed_paths(
                            exact,
                            prefixes,
                            scan_root_base_ignores,
                            scope,
                        ),
                    });
                });
                snapshotter.into_result()
            });
        if let Err(err) = traversal_result {
            if completion.is_some() {
                self.clear_fsmonitor_cursor();
            }
            drop(completion);
            return Err(err);
        }
        if let Some(completion) = completion.as_ref()
            && let Err(err) = completion.check_healthy()
        {
            self.clear_fsmonitor_cursor();
            return Err(SnapshotError::Other {
                message: "AWACS snapshot scan lease renewal failed".to_string(),
                err,
            });
        }

        let stats = SnapshotStats {
            warnings: snapshot_warning.into_iter().collect(),
            untracked_paths: untracked_paths_rx.into_iter().collect(),
            invalid_utf8_paths: Default::default(),
        };
        let mut tree_builder = MergedTreeBuilder::new(self.tree.clone());
        trace_span!("process tree entries").in_scope(|| {
            for (path, tree_values) in &tree_entries_rx {
                tree_builder.set_or_remove(path, tree_values);
            }
        });
        trace_span!("process deleted tree entries").in_scope(|| {
            let deleted_files: HashSet<RepoPathBuf> = HashSet::from_iter(deleted_files_rx);
            is_dirty |= !deleted_files.is_empty();
            for file in &deleted_files {
                tree_builder.set_or_remove(file.clone(), Merge::absent());
            }
        });
        trace_span!("write tree")
            .in_scope(async || -> Result<(), BackendError> {
                let new_tree = tree_builder.write_tree().await?;
                is_dirty |= new_tree.tree_ids_and_labels() != self.tree.tree_ids_and_labels();
                self.tree = new_tree.clone();
                Ok(())
            })
            .await?;
        // Since untracked paths aren't cached in the tree state, we'll need to
        // rescan the working directory changes to report or track them later.
        // TODO: store untracked paths and update fsmonitor_cursor?
        if stats.untracked_paths.is_empty() || fsmonitor_cursor.is_none() {
            self.publish_scan_baseline(fsmonitor_cursor, baseline);
            Ok((is_dirty, stats, completion))
        } else {
            tracing::info!(
                "not updating filesystem-monitor cursor because there are untracked files"
            );
            self.clear_fsmonitor_cursor();
            self.journal_phase =
                crate::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline;
            // Dropping the completion hook aborts the lease because no cursor
            // will be persisted for this scan.
            drop(completion);
            Ok((is_dirty, stats, None))
        }
    }

    #[cfg_attr(
        not(all(target_os = "linux", feature = "awacs")),
        allow(unused_variables)
    )]
    #[instrument(skip_all)]
    async fn make_snapshot_scan(
        &self,
        fsmonitor_settings: &FsmonitorSettings,
        awacs_input_fingerprint: Option<[u8; 32]>,
    ) -> Result<SnapshotScan, SnapshotError> {
        let (
            scan_root,
            fsmonitor_cursor,
            changed_files,
            changed_prefixes,
            completion,
            warning,
            baseline,
        ) = match fsmonitor_settings {
            FsmonitorSettings::None => (
                self.working_copy_path.clone(),
                None,
                None,
                None::<Vec<PathBuf>>,
                None,
                None,
                None,
            ),
            FsmonitorSettings::Test {
                changed_files: _,
                scan_root,
            } => (
                scan_root
                    .clone()
                    .unwrap_or_else(|| self.working_copy_path.clone()),
                None,
                // Test is a mutable-root monitor, so without durable
                // per-path state it must use the conservative full scan.
                None,
                None,
                None,
                None,
                None,
            ),
            FsmonitorSettings::TestAwacs {
                scan_root,
                changed_files,
                cursor,
            } => {
                let input_fingerprint = awacs_input_fingerprint.unwrap_or([0; 32]);
                (
                    scan_root.clone(),
                    None,
                    if self.scan_baseline().is_some() {
                        changed_files.clone()
                    } else {
                        None
                    },
                    None,
                    Some(PendingScan::new(Box::new(NoopScanSession))),
                    None,
                    Some(synthetic_test_awacs_baseline(cursor, input_fingerprint)),
                )
            }
            #[cfg(all(target_os = "linux", feature = "awacs"))]
            FsmonitorSettings::Awacs(config) => {
                let client = if let Some(client) = &config.client {
                    client.clone()
                } else {
                    let client: Box<dyn btrfs_awacs::scan::ScanClient> = Box::new(
                        btrfs_awacs::scan_facade::DirectScanClient::for_root(
                            &self.working_copy_path,
                        )
                        .map_err(|err| SnapshotError::Other {
                            message: "Failed to open AWACS root state".to_string(),
                            err: Box::new(err),
                        })?,
                    );
                    Arc::new(Mutex::new(client))
                };
                let Some(awacs_input_fingerprint) = awacs_input_fingerprint else {
                    return Err(SnapshotError::Other {
                        message: "Failed to begin AWACS snapshot scan".to_string(),
                        err: "AWACS input fingerprint was not provided".into(),
                    });
                };
                // A baseline is reusable only when the durable journal binds
                // an exact AWACS snapshot to the current semantic tree.
                let previous_baseline =
                    self.scan_baseline()
                        .map(|baseline| btrfs_awacs::scan::SnapshotBaseline {
                            identity: btrfs_awacs::scan::SnapshotIdentity {
                                filesystem_uuid: baseline
                                    .filesystem_uuid
                                    .as_slice()
                                    .try_into()
                                    .expect("validated AWACS filesystem UUID"),
                                subvolume_uuid: baseline
                                    .subvolume_uuid
                                    .as_slice()
                                    .try_into()
                                    .expect("validated AWACS subvolume UUID"),
                                read_only: true,
                            },
                            continuity_token: baseline.continuity_token.clone(),
                            retention_token: baseline.retention_token.clone(),
                        });
                let can_use_delta = previous_baseline.is_some();
                let request = btrfs_awacs::scan::BeginScanRequest {
                    live_root: self.working_copy_path.clone(),
                    baseline_owner_id: self
                        .awacs_baseline_owner_id
                        .as_slice()
                        .try_into()
                        .expect("AWACS baseline owner id is always 16 bytes"),
                    previous_baseline,
                };
                let mut client = client.lock().map_err(|_| SnapshotError::Other {
                    message: "Failed to begin AWACS snapshot scan".to_string(),
                    err: "AWACS client lock was poisoned".into(),
                })?;
                let mut lease =
                    client
                        .begin_scan(&request)
                        .map_err(|err| SnapshotError::Other {
                            message: "Failed to begin AWACS snapshot scan".to_string(),
                            err: Box::new(err),
                        })?;
                if let Err(err) = client.validate_scan_root(&lease) {
                    if let Err(abort_err) = lease.finish(btrfs_awacs::scan::ScanOutcome::Aborted) {
                        tracing::warn!(
                            ?abort_err,
                            "failed to abort rejected AWACS scan root lease"
                        );
                    }
                    return Err(SnapshotError::Other {
                        message: "Failed to validate AWACS scan root".to_string(),
                        err: Box::new(err),
                    });
                }
                let scan_root =
                    PathBuf::from(format!("/proc/self/fd/{}", lease.scan_root().as_raw_fd()));
                // AWACS replays retained adjacent cut events and returns Full
                // whenever this cursor's retained lineage cannot be proven.
                let (changed_files, changed_prefixes) = if !can_use_delta {
                    (None, None)
                } else {
                    match &lease.invalidation {
                        btrfs_awacs::scan::Invalidation::Full => (None, None),
                        btrfs_awacs::scan::Invalidation::ExactPaths(paths) => (
                            Some(
                                paths
                                    .iter()
                                    .map(|path| {
                                        PathBuf::from(std::ffi::OsString::from_vec(path.clone()))
                                    })
                                    .collect(),
                            ),
                            Some(Vec::new()),
                        ),
                        btrfs_awacs::scan::Invalidation::Prefixes(paths) => (
                            Some(Vec::new()),
                            Some(
                                paths
                                    .iter()
                                    .map(|path| {
                                        PathBuf::from(std::ffi::OsString::from_vec(path.clone()))
                                    })
                                    .collect(),
                            ),
                        ),
                    }
                };
                let baseline = crate::protos::local_working_copy::AwacsSnapshotBaseline {
                    filesystem_uuid: lease.next_baseline.identity.filesystem_uuid.to_vec(),
                    subvolume_uuid: lease.next_baseline.identity.subvolume_uuid.to_vec(),
                    continuity_token: lease.next_baseline.continuity_token.clone(),
                    // Kept for journal compatibility with the transitional
                    // daemon protocol. The daemon-free coordinator does not
                    // use this field to pin a physical snapshot.
                    retention_token: lease.next_baseline.retention_token.clone(),
                    interpretation_input_fingerprint: awacs_input_fingerprint.to_vec(),
                };
                let completion = PendingScan::new(Box::new(AwacsScanSession::new(lease)));
                (
                    scan_root,
                    None,
                    changed_files,
                    changed_prefixes,
                    Some(completion),
                    None,
                    Some(baseline),
                )
            }
            #[cfg(not(all(target_os = "linux", feature = "awacs")))]
            FsmonitorSettings::Awacs(_) => {
                return Err(SnapshotError::Other {
                    message: "Failed to begin AWACS snapshot scan".to_string(),
                    err: "AWACS requires a Unix jj build with the `awacs` feature".into(),
                });
            }
            #[cfg(feature = "watchman")]
            FsmonitorSettings::Watchman(config) => match self.query_watchman(config).await {
                Ok((_watchman_clock, _changed_files)) => (
                    self.working_copy_path.clone(),
                    // Watchman has no immutable retained baseline. Without
                    // per-path state, changed names alone cannot prove the
                    // unchanged semantic tree, so conservatively full-scan.
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
                Err(TreeStateError::Fsmonitor { user_message, .. }) => (
                    self.working_copy_path.clone(),
                    None,
                    None,
                    None,
                    None,
                    Some(SnapshotWarning::FileSystemMonitor {
                        message: user_message,
                    }),
                    None,
                ),
                Err(_err) => (
                    self.working_copy_path.clone(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
            },
            #[cfg(not(feature = "watchman"))]
            FsmonitorSettings::Watchman(_) => {
                return Err(SnapshotError::Other {
                    message: "Failed to query the filesystem monitor".to_string(),
                    err: "Cannot query Watchman because jj was not compiled with the `watchman` \
                          feature (consider disabling `fsmonitor.backend`)"
                        .into(),
                });
            }
        };
        let scope = match (changed_files, changed_prefixes) {
            (None, None) => ScanScope::Full,
            (changed_files, changed_prefixes) => {
                let parsed_paths = trace_span!("processing fsmonitor paths").in_scope(|| {
                    let repo_paths = changed_files
                        .unwrap_or_default()
                        .iter()
                        .map(|path| RepoPathBuf::from_relative_path(path))
                        .collect::<Result<Vec<_>, _>>();
                    let prefixes = changed_prefixes
                        .unwrap_or_default()
                        .iter()
                        .map(|path| RepoPathBuf::from_relative_path(path))
                        .collect::<Result<Vec<_>, _>>();
                    match (repo_paths, prefixes) {
                        (Ok(repo_paths), Ok(prefixes)) => Ok((repo_paths, prefixes)),
                        (Err(err), _) | (_, Err(err)) => Err(err),
                    }
                });
                match parsed_paths {
                    Ok((repo_paths, prefixes)) => ScanScope::from_delta(repo_paths, prefixes),
                    Err(_) => {
                        // An authoritative backend must never silently drop a
                        // malformed delta path. Widen to the full immutable B
                        // scan instead.
                        ScanScope::Full
                    }
                }
            }
        };
        Ok(SnapshotScan {
            scan_root,
            scope,
            fsmonitor_cursor,
            baseline,
            completion,
            warning,
        })
    }

    #[cfg(all(target_os = "linux", feature = "awacs"))]
    async fn seed_awacs_baseline_without_scan(
        &mut self,
        input_fingerprint: [u8; 32],
    ) -> Result<Option<PendingScan>, SnapshotError> {
        if self.journal_phase
            != crate::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline
            || self.baseline.is_some()
        {
            return Err(SnapshotError::Other {
                message: "Failed to seed AWACS snapshot baseline".to_owned(),
                err: "working copy already has a committed baseline".into(),
            });
        }
        let SnapshotScan {
            fsmonitor_cursor,
            baseline,
            completion,
            ..
        } = self
            .make_snapshot_scan(&self.fsmonitor_settings, Some(input_fingerprint))
            .await?;
        if baseline.is_none() || completion.is_none() {
            return Err(SnapshotError::Other {
                message: "Failed to seed AWACS snapshot baseline".to_owned(),
                err: "snapshot-backed worktree requires an authoritative AWACS lease".into(),
            });
        }
        self.publish_scan_baseline(fsmonitor_cursor, baseline);
        Ok(completion)
    }
}

struct DirectoryToVisit<'a> {
    dir: RepoPathBuf,
    disk_dir: PathBuf,
    git_ignore: Arc<GitIgnoreFile>,
    tracked_paths: TrackedPaths<'a>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentDirEntryKind {
    Dir,
    File,
}

#[derive(Clone, Debug)]
struct PresentDirEntries {
    dirs: HashSet<String>,
    files: HashSet<String>,
}

/// Helper to scan local-disk directories and files in parallel.
struct FileSnapshotter<'a> {
    tree_state: &'a TreeState,
    scan_root: &'a Path,
    current_tree: &'a MergedTree,
    matcher: &'a dyn Matcher,
    start_tracking_matcher: &'a dyn Matcher,
    force_tracking_matcher: &'a dyn Matcher,
    tree_entries_tx: Sender<(RepoPathBuf, MergedTreeValue)>,
    untracked_paths_tx: Sender<(RepoPathBuf, UntrackedReason)>,
    deleted_files_tx: Sender<RepoPathBuf>,
    error: OnceLock<SnapshotError>,
    progress: Option<&'a SnapshotProgress<'a>>,
    max_new_file_size: u64,
}

impl FileSnapshotter<'_> {
    fn spawn_ok<'scope, F>(&'scope self, scope: &rayon::Scope<'scope>, body: F)
    where
        F: FnOnce(&rayon::Scope<'scope>) -> Result<(), SnapshotError> + Send + 'scope,
    {
        scope.spawn(|scope| {
            if self.error.get().is_some() {
                return;
            }
            match body(scope) {
                Ok(()) => {}
                Err(err) => self.error.set(err).unwrap_or(()),
            }
        });
    }

    /// Extracts the result of the snapshot.
    fn into_result(self) -> Result<(), SnapshotError> {
        match self.error.into_inner() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Visits authoritative exact/prefix deltas without walking root siblings.
    /// Exact paths use `symlink_metadata()` directly; prefixes start traversal
    /// at their own roots.
    fn visit_changed_paths<'scope>(
        &'scope self,
        exact: &[RepoPathBuf],
        prefixes: &[RepoPathBuf],
        base_ignores: Arc<GitIgnoreFile>,
        scope: &rayon::Scope<'scope>,
    ) -> Result<(), SnapshotError> {
        for prefix in prefixes {
            self.visit_prefix_root(prefix, base_ignores.clone(), scope)?;
        }
        for path in exact {
            self.visit_exact_path(path, base_ignores.clone(), scope)?;
        }
        Ok(())
    }

    fn path_has_reserved_component(path: &RepoPath) -> bool {
        path.components()
            .any(|component| RESERVED_DIR_NAMES.contains(&component.as_internal_str()))
    }

    /// Builds ignore context through `dir` without reading sibling entries.
    /// The boolean records that an ancestor was ignored, in which case nested
    /// `.gitignore` files are intentionally not interpreted.
    fn ignore_context_for_directory(
        &self,
        base_ignores: Arc<GitIgnoreFile>,
        dir: &RepoPath,
    ) -> Result<(Arc<GitIgnoreFile>, bool), SnapshotError> {
        let mut git_ignore =
            base_ignores.chain_with_file(RepoPath::root(), self.scan_root.join(".gitignore"))?;
        let mut current = RepoPathBuf::root();
        let mut ancestor_ignored = false;
        for component in dir.components() {
            current = current.join(component);
            if !ancestor_ignored && git_ignore.matches_dir(&current) {
                ancestor_ignored = true;
            }
            let disk_dir = current.to_fs_path(self.scan_root)?;
            let metadata = match disk_dir.symlink_metadata() {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    return Err(SnapshotError::Other {
                        message: format!(
                            "Refusing directed scan through missing ancestor {}",
                            disk_dir.display()
                        ),
                        err: err.into(),
                    });
                }
                Err(err) => {
                    return Err(SnapshotError::Other {
                        message: format!("Failed to stat directory {}", disk_dir.display()),
                        err: err.into(),
                    });
                }
            };
            if !metadata.is_dir() {
                return Err(SnapshotError::Other {
                    message: format!(
                        "Refusing directed scan through non-directory ancestor {}",
                        disk_dir.display()
                    ),
                    err: "directed snapshot path crossed a symlink or file boundary".into(),
                });
            }
            // Match the full scanner's nested-repository rule. Do not read
            // ignore files below a nested repository boundary.
            if RESERVED_DIR_NAMES
                .iter()
                .any(|name| disk_dir.join(name).symlink_metadata().is_ok())
            {
                return Err(SnapshotError::Other {
                    message: format!(
                        "Refusing directed scan through nested repository {}",
                        disk_dir.display()
                    ),
                    err: "directed snapshot path crossed a nested repository boundary".into(),
                });
            }
            if ancestor_ignored {
                continue;
            }
            git_ignore = git_ignore.chain_with_file(&current, disk_dir.join(".gitignore"))?;
        }
        Ok((git_ignore, ancestor_ignored))
    }

    fn emit_deleted_tracked_paths(&self, tracked_paths: TrackedPaths<'_>, keep: Option<&RepoPath>) {
        for (path, kind) in tracked_paths.iter() {
            if kind == TrackedKind::GitSubmodule || keep == Some(path) {
                continue;
            }
            if self.matcher.matches(path) {
                self.deleted_files_tx.send(path.to_owned()).ok();
            }
        }
    }

    async fn inspect_present_path(
        &self,
        path: RepoPathBuf,
        disk_path: &Path,
        git_ignore: &Arc<GitIgnoreFile>,
        ancestor_ignored: bool,
        tracked_kind: Option<TrackedKind>,
        metadata: &Metadata,
    ) -> Result<bool, SnapshotError> {
        if tracked_kind == Some(TrackedKind::GitSubmodule) {
            return Ok(true);
        }
        if tracked_kind.is_none()
            && ((ancestor_ignored || git_ignore.matches_file(&path))
                && !self.force_tracking_matcher.matches(&path))
        {
            return Ok(false);
        }
        if tracked_kind.is_none() && !self.start_tracking_matcher.matches(&path) {
            self.untracked_paths_tx
                .send((path, UntrackedReason::FileNotAutoTracked))
                .ok();
            return Ok(false);
        }
        if tracked_kind.is_none()
            && metadata.len() > self.max_new_file_size
            && !self.force_tracking_matcher.matches(&path)
        {
            self.untracked_paths_tx
                .send((
                    path,
                    UntrackedReason::FileTooLarge {
                        size: metadata.len(),
                        max_size: self.max_new_file_size,
                    },
                ))
                .ok();
            return Ok(false);
        }
        if let Some(observed_disk_kind) = observed_disk_kind(metadata) {
            if let Some(progress) = self.progress {
                progress(&path);
            }
            self.process_present_file(path, disk_path, observed_disk_kind)
                .await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn visit_exact_path<'scope>(
        &'scope self,
        path: &RepoPath,
        base_ignores: Arc<GitIgnoreFile>,
        scope: &rayon::Scope<'scope>,
    ) -> Result<(), SnapshotError> {
        if Self::path_has_reserved_component(path) {
            return Ok(());
        }
        let tracked_paths = self.tree_state.tracked_paths.all().prefixed(path);
        let disk_path = path.to_fs_path(self.scan_root)?;
        let metadata = match disk_path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.emit_deleted_tracked_paths(tracked_paths, None);
                return Ok(());
            }
            Err(err) => {
                return Err(SnapshotError::Other {
                    message: format!("Failed to stat file {}", disk_path.display()),
                    err: err.into(),
                });
            }
        };
        if metadata.is_dir() {
            return self.visit_prefix_root(path, base_ignores, scope);
        }
        if !self.matcher.matches(path) {
            return Ok(());
        }
        let parent = path.parent().unwrap_or(RepoPath::root());
        let (git_ignore, ancestor_ignored) =
            self.ignore_context_for_directory(base_ignores, parent)?;
        self.emit_deleted_tracked_paths(tracked_paths, Some(path));
        let present = self
            .inspect_present_path(
                path.to_owned(),
                &disk_path,
                &git_ignore,
                ancestor_ignored,
                tracked_paths.get(path),
                &metadata,
            )
            .block_on()?;
        if !present && tracked_paths.get(path).is_some() {
            self.deleted_files_tx.send(path.to_owned()).ok();
        }
        Ok(())
    }

    fn visit_prefix_root<'scope>(
        &'scope self,
        prefix: &RepoPath,
        base_ignores: Arc<GitIgnoreFile>,
        scope: &rayon::Scope<'scope>,
    ) -> Result<(), SnapshotError> {
        if Self::path_has_reserved_component(prefix) {
            return Ok(());
        }
        let tracked_paths = self.tree_state.tracked_paths.all().prefixed(prefix);
        let disk_path = prefix.to_fs_path(self.scan_root)?;
        let metadata = match disk_path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.emit_deleted_tracked_paths(tracked_paths, None);
                return Ok(());
            }
            Err(err) => {
                return Err(SnapshotError::Other {
                    message: format!("Failed to stat file {}", disk_path.display()),
                    err: err.into(),
                });
            }
        };
        let parent = prefix.parent().unwrap_or(RepoPath::root());
        let (parent_ignores, ancestor_ignored) = if prefix.is_root() {
            (base_ignores, false)
        } else {
            self.ignore_context_for_directory(base_ignores, parent)?
        };
        if metadata.is_dir() {
            if RESERVED_DIR_NAMES
                .iter()
                .any(|name| disk_path.join(name).symlink_metadata().is_ok())
            {
                self.emit_deleted_tracked_paths(tracked_paths, None);
                return Ok(());
            }
            if (ancestor_ignored || parent_ignores.matches_dir(prefix))
                && self.force_tracking_matcher.visit(prefix).is_nothing()
            {
                return self.visit_tracked_files(tracked_paths).block_on();
            }
            let directory_to_visit = DirectoryToVisit {
                dir: prefix.to_owned(),
                disk_dir: disk_path,
                git_ignore: parent_ignores,
                tracked_paths,
            };
            self.visit_directory(directory_to_visit, scope)
        } else {
            if !self.matcher.matches(prefix) {
                return Ok(());
            }
            self.emit_deleted_tracked_paths(tracked_paths, Some(prefix));
            let present = self
                .inspect_present_path(
                    prefix.to_owned(),
                    &disk_path,
                    &parent_ignores,
                    ancestor_ignored,
                    tracked_paths.get(prefix),
                    &metadata,
                )
                .block_on()?;
            if !present && tracked_paths.get(prefix).is_some() {
                self.deleted_files_tx.send(prefix.to_owned()).ok();
            };
            Ok(())
        }
    }

    /// Visits the directory entries, spawns jobs to recurse into sub
    /// directories.
    fn visit_directory<'scope>(
        &'scope self,
        directory_to_visit: DirectoryToVisit<'scope>,
        scope: &rayon::Scope<'scope>,
    ) -> Result<(), SnapshotError> {
        let DirectoryToVisit {
            dir,
            disk_dir,
            git_ignore,
            tracked_paths,
        } = directory_to_visit;

        let git_ignore = git_ignore.chain_with_file(&dir, disk_dir.join(".gitignore"))?;
        let dir_entries: Vec<_> = disk_dir
            .read_dir()
            .and_then(|entries| entries.try_collect())
            .map_err(|err| SnapshotError::Other {
                message: format!("Failed to read directory {}", disk_dir.display()),
                err: err.into(),
            })?;
        let (dirs, files) = dir_entries
            .into_par_iter()
            // Don't split into too many small jobs. For a small directory,
            // sequential scan should be fast enough.
            .with_min_len(100)
            .filter_map(|entry| {
                self.process_dir_entry(&dir, &git_ignore, tracked_paths, &entry, scope)
                    .block_on()
                    .transpose()
            })
            .map(|item| match item {
                Ok((PresentDirEntryKind::Dir, name)) => Ok(Either::Left(name)),
                Ok((PresentDirEntryKind::File, name)) => Ok(Either::Right(name)),
                Err(err) => Err(err),
            })
            .collect::<Result<_, _>>()?;
        let present_entries = PresentDirEntries { dirs, files };
        self.emit_deleted_files(&dir, tracked_paths, &present_entries);
        Ok(())
    }

    async fn process_dir_entry<'scope>(
        &'scope self,
        dir: &RepoPath,
        git_ignore: &Arc<GitIgnoreFile>,
        tracked_paths: TrackedPaths<'scope>,
        entry: &DirEntry,
        scope: &rayon::Scope<'scope>,
    ) -> Result<Option<(PresentDirEntryKind, String)>, SnapshotError> {
        let file_type = entry.file_type().unwrap();
        let file_name = entry.file_name();
        let name_string = file_name
            .into_string()
            .map_err(|path| SnapshotError::InvalidUtf8Path { path })?;

        if RESERVED_DIR_NAMES.contains(&name_string.as_str()) {
            return Ok(None);
        }
        let name = RepoPathComponent::new(&name_string).unwrap();
        let path = dir.join(name);
        let maybe_tracked_kind = tracked_paths.get_at(dir, name);
        if maybe_tracked_kind == Some(TrackedKind::GitSubmodule) {
            return Ok(None);
        }

        if file_type.is_dir() {
            let tracked_paths = tracked_paths.prefixed_at(dir, name);
            // If a submodule was added in commit C, and a user decides to run
            // `jj new <something before C>` from after C, then the submodule
            // files stick around but it is no longer seen as a submodule.
            // We need to ensure that it is not tracked as if it was added to
            // the main repo.
            // See https://github.com/jj-vcs/jj/issues/4349.
            // To solve this, we ignore all nested repos entirely.
            let disk_dir = entry.path();
            for &name in RESERVED_DIR_NAMES {
                if disk_dir.join(name).symlink_metadata().is_ok() {
                    return Ok(None);
                }
            }

            if git_ignore.matches_dir(&path)
                && self.force_tracking_matcher.visit(&path).is_nothing()
            {
                // If the whole directory is ignored by .gitignore, visit only
                // paths we're already tracking. This is because .gitignore in
                // ignored directory must be ignored. It's also more efficient.
                // start_tracking_matcher is NOT tested here because we need to
                // scan directory entries to report untracked paths.
                self.spawn_ok(scope, move |_| {
                    self.visit_tracked_files(tracked_paths).block_on()
                });
            } else if !self.matcher.visit(&path).is_nothing() {
                let directory_to_visit = DirectoryToVisit {
                    dir: path,
                    disk_dir,
                    git_ignore: git_ignore.clone(),
                    tracked_paths,
                };
                self.spawn_ok(scope, |scope| {
                    self.visit_directory(directory_to_visit, scope)
                });
            }
            // Whether or not the directory path matches, any child file entries
            // shouldn't be touched within the current recursion step.
            Ok(Some((PresentDirEntryKind::Dir, name_string)))
        } else if self.matcher.matches(&path) {
            if let Some(progress) = self.progress {
                progress(&path);
            }
            if maybe_tracked_kind.is_none()
                && (git_ignore.matches_file(&path) && !self.force_tracking_matcher.matches(&path))
            {
                // If it wasn't already tracked and it matches
                // the ignored paths, then ignore it.
                Ok(None)
            } else if maybe_tracked_kind.is_none() && !self.start_tracking_matcher.matches(&path) {
                // Leave the file untracked
                self.untracked_paths_tx
                    .send((path, UntrackedReason::FileNotAutoTracked))
                    .ok();
                Ok(None)
            } else {
                let metadata = entry.metadata().map_err(|err| SnapshotError::Other {
                    message: format!("Failed to stat file {}", entry.path().display()),
                    err: err.into(),
                })?;
                if maybe_tracked_kind.is_none()
                    && (metadata.len() > self.max_new_file_size
                        && !self.force_tracking_matcher.matches(&path))
                {
                    // Leave the large file untracked
                    let reason = UntrackedReason::FileTooLarge {
                        size: metadata.len(),
                        max_size: self.max_new_file_size,
                    };
                    self.untracked_paths_tx.send((path, reason)).ok();
                    Ok(None)
                } else if let Some(observed_disk_kind) = observed_disk_kind(&metadata) {
                    self.process_present_file(path, &entry.path(), observed_disk_kind)
                        .await?;
                    Ok(Some((PresentDirEntryKind::File, name_string)))
                } else {
                    // Special file is not considered present
                    Ok(None)
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Visits only paths we're already tracking.
    async fn visit_tracked_files(
        &self,
        tracked_paths: TrackedPaths<'_>,
    ) -> Result<(), SnapshotError> {
        for (tracked_path, kind) in tracked_paths.iter() {
            if kind == TrackedKind::GitSubmodule {
                continue;
            }
            if !self.matcher.matches(tracked_path) {
                continue;
            }
            let disk_path = tracked_path.to_fs_path(self.scan_root)?;
            let metadata = match disk_path.symlink_metadata() {
                Ok(metadata) => Some(metadata),
                Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                Err(err) => {
                    return Err(SnapshotError::Other {
                        message: format!("Failed to stat file {}", disk_path.display()),
                        err: err.into(),
                    });
                }
            };
            if let Some(metadata) = &metadata
                && let Some(observed_disk_kind) = observed_disk_kind(metadata)
            {
                self.process_present_file(tracked_path.to_owned(), &disk_path, observed_disk_kind)
                    .await?;
            } else {
                self.deleted_files_tx.send(tracked_path.to_owned()).ok();
            }
        }
        Ok(())
    }

    async fn process_present_file(
        &self,
        path: RepoPathBuf,
        disk_path: &Path,
        observed_disk_kind: ObservedDiskKind,
    ) -> Result<(), SnapshotError> {
        let update = self
            .get_updated_tree_value(&path, disk_path, &observed_disk_kind)
            .await?;
        if let Some(tree_value) = update {
            self.tree_entries_tx.send((path, tree_value)).ok();
        }
        Ok(())
    }

    /// Emits file paths that don't exist in the `present_entries`.
    fn emit_deleted_files(
        &self,
        dir: &RepoPath,
        tracked_paths: TrackedPaths<'_>,
        present_entries: &PresentDirEntries,
    ) {
        let tracked_path_chunks = tracked_paths.iter().chunk_by(|(path, _kind)| {
            // Extract <name> from <dir>, <dir>/<name>, or <dir>/<name>/**.
            // The semantic tree can contain <dir> as a file on a
            // file-to-directory transition.
            debug_assert!(path.starts_with(dir));
            let slash = usize::from(!dir.is_root());
            let len = dir.as_internal_file_string().len() + slash;
            let tail = path.as_internal_file_string().get(len..).unwrap_or("");
            match tail.split_once('/') {
                Some((name, _)) => (PresentDirEntryKind::Dir, name),
                None => (PresentDirEntryKind::File, tail),
            }
        });
        tracked_path_chunks
            .into_iter()
            .filter(|&((kind, name), _)| match kind {
                PresentDirEntryKind::Dir => !present_entries.dirs.contains(name),
                PresentDirEntryKind::File => !present_entries.files.contains(name),
            })
            .flat_map(|(_, chunk)| chunk)
            // Whether or not the entry exists, submodule should be ignored
            .filter(|(_, kind)| *kind != TrackedKind::GitSubmodule)
            .filter(|(path, _)| self.matcher.matches(path))
            .try_for_each(|(path, _)| self.deleted_files_tx.send(path.to_owned()))
            .ok();
    }

    async fn get_updated_tree_value(
        &self,
        repo_path: &RepoPath,
        disk_path: &Path,
        observed_disk_kind: &ObservedDiskKind,
    ) -> Result<Option<MergedTreeValue>, SnapshotError> {
        let current_tree_values = self.current_tree.path_value(repo_path).await?;
        let observed_disk_kind = if !self.tree_state.symlink_support {
            let mut observed_disk_kind = observed_disk_kind.clone();
            if matches!(observed_disk_kind, ObservedDiskKind::Normal { .. })
                && matches!(current_tree_values.as_normal(), Some(TreeValue::Symlink(_)))
            {
                observed_disk_kind = ObservedDiskKind::Symlink;
            }
            observed_disk_kind
        } else {
            observed_disk_kind.clone()
        };
        let new_tree_values = match observed_disk_kind {
            ObservedDiskKind::Normal { exec_bit } => {
                let materialized_conflict_data = self
                    .materialized_conflict_data_for_tree_value(repo_path, &current_tree_values)
                    .await?;
                self.write_path_to_store(
                    repo_path,
                    disk_path,
                    &current_tree_values,
                    exec_bit,
                    materialized_conflict_data,
                )
                .await?
            }
            ObservedDiskKind::Symlink => {
                let id = self.write_symlink_to_store(repo_path, disk_path).await?;
                Merge::normal(TreeValue::Symlink(id))
            }
        };
        if new_tree_values != current_tree_values {
            Ok(Some(new_tree_values))
        } else {
            Ok(None)
        }
    }

    /// Reconstructs the marker length used when the prior semantic value was
    /// materialized. Conflict marker state is a property of X plus the active
    /// materialization policy, not durable per-path metadata.
    async fn materialized_conflict_data_for_tree_value(
        &self,
        repo_path: &RepoPath,
        current_tree_values: &MergedTreeValue,
    ) -> Result<Option<MaterializedConflictData>, SnapshotError> {
        // Use the same semantic conflict shape, active labels, and backend
        // merge policy that checkout used to materialize X. Marker style only
        // changes the rendered syntax; the collision-avoiding marker length is
        // selected from the materialized file sides before that style is
        // applied.
        let materialized = materialize_tree_value(
            self.store(),
            repo_path,
            current_tree_values.clone(),
            self.current_tree.labels(),
        )
        .await?;
        let MaterializedTreeValue::FileConflict(file) = materialized else {
            return Ok(None);
        };
        let conflict_marker_len = choose_materialized_conflict_marker_len(&file.contents)
            .try_into()
            .map_err(|err| SnapshotError::Other {
                message: format!(
                    "Conflict marker length at {} does not fit in working-copy state",
                    repo_path.as_internal_file_string()
                ),
                err: Box::new(err),
            })?;
        Ok(Some(MaterializedConflictData {
            conflict_marker_len,
        }))
    }

    fn store(&self) -> &Store {
        &self.tree_state.store
    }

    async fn write_path_to_store(
        &self,
        repo_path: &RepoPath,
        disk_path: &Path,
        current_tree_values: &MergedTreeValue,
        exec_bit: ExecBit,
        materialized_conflict_data: Option<MaterializedConflictData>,
    ) -> Result<MergedTreeValue, SnapshotError> {
        if let Some(current_tree_value) = current_tree_values.as_resolved() {
            let id = self.write_file_to_store(repo_path, disk_path).await?;
            // On Windows, we preserve the executable bit from the current tree.
            let executable = exec_bit.for_tree_value(self.tree_state.exec_policy, || {
                if let Some(TreeValue::File {
                    id: _,
                    executable,
                    copy_id: _,
                }) = current_tree_value
                {
                    Some(*executable)
                } else {
                    None
                }
            });
            // Preserve the copy id from the current tree
            let copy_id = {
                if let Some(TreeValue::File {
                    id: _,
                    executable: _,
                    copy_id,
                }) = current_tree_value
                {
                    copy_id.clone()
                } else {
                    CopyId::placeholder()
                }
            };
            Ok(Merge::normal(TreeValue::File {
                id,
                executable,
                copy_id,
            }))
        } else if let Some(old_file_ids) = current_tree_values.to_file_merge() {
            // Safe to unwrap because the copy id exists exactly on the file variant
            let copy_id_merge = current_tree_values.to_copy_id_merge().unwrap();
            let copy_id = copy_id_merge
                .resolve_trivial(SameChange::Accept)
                .cloned()
                .flatten()
                .unwrap_or_else(CopyId::placeholder);
            let mut contents = vec![];
            let file = File::open(disk_path).map_err(|err| SnapshotError::Other {
                message: format!("Failed to open file {}", disk_path.display()),
                err: err.into(),
            })?;
            self.tree_state
                .target_eol_strategy
                .convert_eol_for_snapshot(AllowStdIo::new(file))
                .await
                .map_err(|err| SnapshotError::Other {
                    message: "Failed to convert the EOL".to_string(),
                    err: err.into(),
                })?
                .read_to_end(&mut contents)
                .await
                .map_err(|err| SnapshotError::Other {
                    message: "Failed to read the EOL converted contents".to_string(),
                    err: err.into(),
                })?;
            // If the file contained a conflict before and is a normal file on
            // disk, we try to parse any conflict markers in the file into a
            // conflict.
            let new_file_ids = conflicts::update_from_content(
                &old_file_ids,
                self.store(),
                repo_path,
                &contents,
                materialized_conflict_data.map_or(MIN_CONFLICT_MARKER_LEN, |data| {
                    data.conflict_marker_len as usize
                }),
            )
            .await?;
            match new_file_ids.into_resolved() {
                Ok(file_id) => {
                    // On Windows, we preserve the executable bit from the merged trees.
                    let executable = exec_bit.for_tree_value(self.tree_state.exec_policy, || {
                        current_tree_values
                            .to_executable_merge()
                            .as_ref()
                            .and_then(conflicts::resolve_file_executable)
                    });
                    Ok(Merge::normal(TreeValue::File {
                        id: file_id.unwrap(),
                        executable,
                        copy_id,
                    }))
                }
                Err(new_file_ids) => {
                    if new_file_ids != old_file_ids {
                        Ok(current_tree_values.with_new_file_ids(&new_file_ids))
                    } else {
                        Ok(current_tree_values.clone())
                    }
                }
            }
        } else {
            Ok(current_tree_values.clone())
        }
    }

    async fn write_file_to_store(
        &self,
        path: &RepoPath,
        disk_path: &Path,
    ) -> Result<FileId, SnapshotError> {
        let file = File::open(disk_path).map_err(|err| SnapshotError::Other {
            message: format!("Failed to open file {}", disk_path.display()),
            err: err.into(),
        })?;
        let mut contents = self
            .tree_state
            .target_eol_strategy
            .convert_eol_for_snapshot(AllowStdIo::new(file))
            .await
            .map_err(|err| SnapshotError::Other {
                message: "Failed to convert the EOL".to_string(),
                err: err.into(),
            })?;
        Ok(self.store().write_file(path, &mut contents).await?)
    }

    async fn write_symlink_to_store(
        &self,
        path: &RepoPath,
        disk_path: &Path,
    ) -> Result<SymlinkId, SnapshotError> {
        if self.tree_state.symlink_support {
            let target = disk_path.read_link().map_err(|err| SnapshotError::Other {
                message: format!("Failed to read symlink {}", disk_path.display()),
                err: err.into(),
            })?;
            let str_target = symlink_target_convert_to_store(&target).ok_or_else(|| {
                SnapshotError::InvalidUtf8SymlinkTarget {
                    path: disk_path.to_path_buf(),
                }
            })?;
            Ok(self.store().write_symlink(path, &str_target).await?)
        } else {
            let target = fs::read(disk_path).map_err(|err| SnapshotError::Other {
                message: format!("Failed to read file {}", disk_path.display()),
                err: err.into(),
            })?;
            let string_target =
                String::from_utf8(target).map_err(|_| SnapshotError::InvalidUtf8SymlinkTarget {
                    path: disk_path.to_path_buf(),
                })?;
            Ok(self.store().write_symlink(path, &string_target).await?)
        }
    }
}

/// Functions to update local-disk files from the store.
impl TreeState {
    async fn write_file(
        &self,
        disk_path: &Path,
        contents: impl AsyncRead + Send + Unpin,
        exec_bit: ExecBit,
        apply_eol_conversion: bool,
    ) -> Result<(), CheckoutError> {
        let mut file = File::options()
            .write(true)
            .create_new(true) // Don't overwrite un-ignored file. Don't follow symlink.
            .open(disk_path)
            .map_err(|err| CheckoutError::Other {
                message: format!("Failed to open file {} for writing", disk_path.display()),
                err: err.into(),
            })?;
        let contents = if apply_eol_conversion {
            self.target_eol_strategy
                .convert_eol_for_update(contents)
                .await
                .map_err(|err| CheckoutError::Other {
                    message: "Failed to convert the EOL for the content".to_string(),
                    err: err.into(),
                })?
        } else {
            Box::new(contents)
        };
        copy_async_to_sync(contents, &mut file)
            .await
            .map_err(|err| CheckoutError::Other {
                message: format!(
                    "Failed to write the content to the file {}",
                    disk_path.display()
                ),
                err: err.into(),
            })?;
        set_executable(exec_bit, disk_path)
            .map_err(|err| checkout_error_for_stat_error(err, disk_path))?;
        Ok(())
    }

    fn write_symlink(&self, disk_path: &Path, target: String) -> Result<(), CheckoutError> {
        let target = symlink_target_convert_to_disk(&target);

        if cfg!(windows) {
            // On Windows, "/" can't be part of valid file name, and "/" is also not a valid
            // separator for the symlink target. See an example of this issue in
            // https://github.com/jj-vcs/jj/issues/6934.
            //
            // We use debug_assert_* instead of assert_* because we want to avoid panic in
            // release build, and we are sure that we shouldn't create invalid symlinks in
            // tests.
            debug_assert_ne!(
                target.as_os_str().to_str().map(|path| path.contains('/')),
                Some(true),
                r#"Expect the symlink target doesn't contain "/", but got invalid symlink target: {}."#,
                target.display()
            );
        }

        // On Windows, this will create a nonfunctional link for directories,
        // but at the moment we don't have enough information in the tree to
        // determine whether the symlink target is a file or a directory.
        symlink_file(&target, disk_path).map_err(|err| CheckoutError::Other {
            message: format!(
                "Failed to create symlink from {} to {}",
                disk_path.display(),
                target.display()
            ),
            err: err.into(),
        })?;
        Ok(())
    }

    async fn write_conflict(
        &self,
        disk_path: &Path,
        contents: &[u8],
        exec_bit: ExecBit,
    ) -> Result<(), CheckoutError> {
        let contents = self
            .target_eol_strategy
            .convert_eol_for_update(contents)
            .await
            .map_err(|err| CheckoutError::Other {
                message: "Failed to convert the EOL when writing a merge conflict".to_string(),
                err: err.into(),
            })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true) // Don't overwrite un-ignored file. Don't follow symlink.
            .open(disk_path)
            .map_err(|err| CheckoutError::Other {
                message: format!("Failed to open file {} for writing", disk_path.display()),
                err: err.into(),
            })?;
        copy_async_to_sync(contents, &mut file)
            .await
            .map_err(|err| CheckoutError::Other {
                message: format!("Failed to write conflict to file {}", disk_path.display()),
                err: err.into(),
            })?;
        set_executable(exec_bit, disk_path)
            .map_err(|err| checkout_error_for_stat_error(err, disk_path))?;
        Ok(())
    }

    pub fn check_out(&mut self, new_tree: &MergedTree) -> Result<CheckoutStats, CheckoutError> {
        let old_tree = self.tree.clone();
        if old_tree.tree_ids_and_labels() != new_tree.tree_ids_and_labels()
            && self.journal_phase
                != crate::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization
        {
            self.clear_fsmonitor_cursor();
        }
        let stats = self
            .update(&old_tree, new_tree, self.sparse_matcher().as_ref())
            .block_on()?;
        self.tree = new_tree.clone();
        Ok(stats)
    }

    pub fn set_sparse_patterns(
        &mut self,
        sparse_patterns: Vec<RepoPathBuf>,
    ) -> Result<CheckoutStats, CheckoutError> {
        if self.sparse_patterns == sparse_patterns {
            return Ok(CheckoutStats::default());
        }
        if self.journal_phase
            != crate::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization
        {
            self.clear_fsmonitor_cursor();
        }
        let tree = self.tree.clone();
        let old_matcher = PrefixMatcher::new(&self.sparse_patterns);
        let new_matcher = PrefixMatcher::new(&sparse_patterns);
        let added_matcher = DifferenceMatcher::new(&new_matcher, &old_matcher);
        let removed_matcher = DifferenceMatcher::new(&old_matcher, &new_matcher);
        let empty_tree = self.store.empty_merged_tree();
        let added_stats = self.update(&empty_tree, &tree, &added_matcher).block_on()?;
        let removed_stats = self
            .update(&tree, &empty_tree, &removed_matcher)
            .block_on()?;
        self.sparse_patterns = sparse_patterns;
        assert_eq!(added_stats.updated_files, 0);
        assert_eq!(added_stats.removed_files, 0);
        assert_eq!(removed_stats.updated_files, 0);
        assert_eq!(removed_stats.added_files, 0);
        assert_eq!(removed_stats.skipped_files, 0);
        Ok(CheckoutStats {
            updated_files: 0,
            added_files: added_stats.added_files,
            removed_files: removed_stats.removed_files,
            skipped_files: added_stats.skipped_files,
        })
    }

    async fn update(
        &mut self,
        old_tree: &MergedTree,
        new_tree: &MergedTree,
        matcher: &dyn Matcher,
    ) -> Result<CheckoutStats, CheckoutError> {
        // TODO: maybe it's better not include the skipped counts in the "intended"
        // counts
        let mut stats = CheckoutStats {
            updated_files: 0,
            added_files: 0,
            removed_files: 0,
            skipped_files: 0,
        };
        let mut prev_created_path: RepoPathBuf = RepoPathBuf::root();

        let mut process_diff_entry = async |path: RepoPathBuf,
                                            before: MergedTreeValue,
                                            after: MaterializedTreeValue|
               -> Result<(), CheckoutError> {
            if after.is_absent() {
                stats.removed_files += 1;
            } else if before.is_absent() {
                stats.added_files += 1;
            } else {
                stats.updated_files += 1;
            }

            // Existing Git submodule can be a non-empty directory on disk. We
            // shouldn't attempt to manage it as a tracked path.
            //
            // TODO: It might be better to add general support for paths not
            // tracked by jj than processing submodules specially. For example,
            // paths excluded by .gitignore can be marked as such so that
            // newly-"unignored" paths won't be snapshotted automatically.
            if matches!(before.as_normal(), Some(TreeValue::GitSubmodule(_)))
                && matches!(after, MaterializedTreeValue::GitSubmodule(_))
            {
                eprintln!("ignoring git submodule at {path:?}");
                return Ok(());
            }

            // This path and the previous one we did work for may have a common prefix. We
            // can adjust the "working copy" path to the parent directory which we know
            // is already created. If there is no common prefix, this will by default use
            // RepoPath::root() as the common prefix.
            let (common_prefix, adjusted_diff_file_path) =
                path.split_common_prefix(&prev_created_path);

            let disk_path = if adjusted_diff_file_path.is_root() {
                // The path being "root" here implies that the entire path has already been
                // created.
                //
                // e.g we may have have already processed a path like: "foo/bar/baz" and this is
                // our `prev_created_path`.
                //
                // and the current path is:
                // "foo/bar"
                //
                // This results in a common prefix of "foo/bar" with empty string for the
                // remainder since its entire prefix has already been created.
                // This means that we _dont_ need to create its parent dirs
                // either.

                path.to_fs_path(self.working_copy_path())?
            } else {
                let adjusted_working_copy_path =
                    common_prefix.to_fs_path(self.working_copy_path())?;

                // Create parent directories no matter if after.is_present(). This
                // ensures that the path never traverses symlinks.
                let Some(disk_path) =
                    create_parent_dirs(&adjusted_working_copy_path, adjusted_diff_file_path)?
                else {
                    stats.skipped_files += 1;
                    return Ok(());
                };

                // Cache this path for the next iteration. This must occur after
                // `create_parent_dirs` to ensure that the path is only set when
                // no symlinks are encountered. Otherwise there could be
                // opportunity for a filesystem write-what-where attack.
                prev_created_path = path
                    .parent()
                    .map(RepoPath::to_owned)
                    .expect("diff path has no parent");

                disk_path
            };

            // Capture the live bit before removing the old path. This keeps
            // Ignore policy correct without persisting a per-path cache.
            let previous_exec_bit = disk_path
                .symlink_metadata()
                .ok()
                .filter(|metadata| metadata.is_file())
                .map(|metadata| ExecBit::new_from_disk(&metadata));

            // If the path was present, check reserved path first and delete it.
            let present_file_deleted = before.is_present()
                && if matches!(before.as_normal(), Some(TreeValue::GitSubmodule(_))) {
                    remove_old_submodule_dir(&disk_path)?
                } else {
                    remove_old_file(&disk_path)?
                };

            // If not, create temporary file to test the path validity.
            if !present_file_deleted && !can_create_new_file(&disk_path)? {
                if matches!(after, MaterializedTreeValue::GitSubmodule(_)) && disk_path.is_dir() {
                    // Failing to materialize submodule, over a directory which
                    // is presumably the submodule before it was added in a
                    // commit, is not an error.
                    // Falling through to the "after" state code, to keep the
                    // normal submodule materialization behavior.
                } else if matches!(before.as_normal(), Some(TreeValue::GitSubmodule(_)))
                    && after.is_absent()
                {
                    // Failing to delete un-tracked submodule directory is not
                    // an error, as the, possibly untracked, contents would
                    // otherwise be lost.
                    // Falling through to the "after" state code in case there
                    // are parents to be deleted.
                } else {
                    stats.skipped_files += 1;
                    return Ok(());
                }
            }

            let get_prev_exec = || previous_exec_bit;

            // TODO: Check that the file has not changed before overwriting/removing it.
            match after {
                MaterializedTreeValue::Absent | MaterializedTreeValue::AccessDenied(_) => {
                    // Reset the previous path to avoid scenarios where this path is deleted,
                    // then on the next iteration recreation is skipped because of this
                    // optimization.
                    prev_created_path = RepoPathBuf::root();

                    let mut parent_dir = disk_path.parent().unwrap();
                    loop {
                        if fs::remove_dir(parent_dir).is_err() {
                            break;
                        }

                        parent_dir = parent_dir.parent().unwrap();
                    }
                    return Ok(());
                }
                MaterializedTreeValue::File(file) => {
                    let exec_bit =
                        ExecBit::new_from_repo(file.executable, self.exec_policy, get_prev_exec);
                    self.write_file(&disk_path, file.reader, exec_bit, true)
                        .await?;
                }
                MaterializedTreeValue::Symlink { id: _, target } => {
                    if self.symlink_support {
                        self.write_symlink(&disk_path, target)?;
                    } else {
                        // The fake symlink file shouldn't be executable.
                        self.write_file(&disk_path, target.as_bytes(), ExecBit(false), false)
                            .await?;
                    }
                }
                MaterializedTreeValue::GitSubmodule(_) => {
                    eprintln!("ignoring git submodule at {path:?}");
                    // Git behavior: Create the submodule directory but don't
                    // populate/overwrite the contents.
                    match fs::create_dir(&disk_path) {
                        Ok(()) => {}
                        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(err) => eprintln!(
                            "warning: failed to create submodule directory {path:?}: {err}"
                        ),
                    }
                }
                MaterializedTreeValue::Tree(_) => {
                    panic!("unexpected tree entry in diff at {path:?}");
                }
                MaterializedTreeValue::FileConflict(file) => {
                    let conflict_marker_len =
                        choose_materialized_conflict_marker_len(&file.contents);
                    let options = ConflictMaterializeOptions {
                        marker_style: self.conflict_marker_style,
                        marker_len: Some(conflict_marker_len),
                        merge: self.store.merge_options().clone(),
                    };
                    let exec_bit = ExecBit::new_from_repo(
                        file.executable.unwrap_or(false),
                        self.exec_policy,
                        get_prev_exec,
                    );
                    let contents =
                        materialize_merge_result_to_bytes(&file.contents, &file.labels, &options);
                    self.write_conflict(&disk_path, &contents, exec_bit).await?;
                }
                MaterializedTreeValue::OtherConflict { id, labels } => {
                    // Unless all terms are regular files, we can't do much
                    // better than trying to describe the merge.
                    let contents = id.describe(&labels);
                    // Since this is a dummy file, it shouldn't be executable.
                    self.write_conflict(&disk_path, contents.as_bytes(), ExecBit(false))
                        .await?;
                }
            }
            Ok(())
        };

        let mut diff_stream = old_tree
            .diff_stream_for_file_system(new_tree, matcher)
            .map(async |TreeDiffEntry { path, values }| match values {
                Ok(diff) => {
                    let result =
                        materialize_tree_value(&self.store, &path, diff.after, new_tree.labels())
                            .await;
                    (path, result.map(|value| (diff.before, value)))
                }
                Err(err) => (path, Err(err)),
            })
            .buffered(self.store.concurrency());

        // If a conflicted file didn't change between the two trees, but the conflict
        // labels did, we still need to re-materialize it in the working copy. We don't
        // need to do this if the conflicts have different numbers of sides though since
        // these conflicts are considered different, so they will be materialized by
        // `MergedTree::diff_stream_for_file_system` already.
        let mut conflicts_to_rematerialize: HashMap<RepoPathBuf, MergedTreeValue> =
            if old_tree.tree_ids().num_sides() == new_tree.tree_ids().num_sides()
                && old_tree.labels() != new_tree.labels()
            {
                // TODO: it might be better to use an async stream here and merge it with the
                // other diff stream, but it could be difficult since the diff stream is not
                // sorted in the same order as the conflicts iterator.
                new_tree
                    .conflicts_matching(matcher)
                    .map(|(path, value)| value.map(|value| (path, value)))
                    .try_collect()?
            } else {
                HashMap::new()
            };

        while let Some((path, data)) = diff_stream.next().await {
            let (before, after) = data?;
            conflicts_to_rematerialize.remove(&path);
            process_diff_entry(path, before, after).await?;
        }
        drop(diff_stream);

        if !conflicts_to_rematerialize.is_empty() {
            for (path, conflict) in conflicts_to_rematerialize {
                let materialized =
                    materialize_tree_value(&self.store, &path, conflict.clone(), new_tree.labels())
                        .await?;
                process_diff_entry(path, conflict, materialized).await?;
            }
        }
        Ok(stats)
    }

    pub async fn reset(&mut self, new_tree: &MergedTree) -> Result<(), ResetError> {
        if self.journal_phase
            != crate::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization
        {
            self.clear_fsmonitor_cursor();
        }
        self.tree = new_tree.clone();
        Ok(())
    }

    pub async fn recover(&mut self, new_tree: &MergedTree) -> Result<(), ResetError> {
        if self.journal_phase
            != crate::protos::local_working_copy::WorkingCopyStatePhase::PendingMaterialization
        {
            self.clear_fsmonitor_cursor();
        }
        self.tree = self.store.empty_merged_tree();
        self.reset(new_tree).await
    }
}

fn checkout_error_for_stat_error(err: io::Error, path: &Path) -> CheckoutError {
    CheckoutError::Other {
        message: format!("Failed to stat file {}", path.display()),
        err: err.into(),
    }
}

/// Working copy state stored in "checkout" file.
#[derive(Clone, Debug)]
struct CheckoutState {
    operation_id: OperationId,
    workspace_name: WorkspaceNameBuf,
}

impl CheckoutState {
    /// Loads checkout identity and, for the compact format, returns the same
    /// decoded journal record so callers can build tree state from one atomic
    /// read. Reading the combined record twice could otherwise pair checkout
    /// identity from generation N with semantic tree state from N+1.
    fn load_with_journal(
        state_path: &Path,
    ) -> Result<
        (
            Self,
            Option<crate::protos::local_working_copy::WorkingCopyState>,
        ),
        WorkingCopyStateError,
    > {
        let wrap_err = |err| WorkingCopyStateError {
            message: "Failed to read checkout state".to_owned(),
            err,
        };
        let checkout_path = state_path.join("checkout");
        let buf = fs::read(&checkout_path).map_err(|err| wrap_err(err.into()))?;
        if let Some(proto) =
            decode_working_copy_state(&checkout_path, &buf).map_err(|err| wrap_err(err.into()))?
        {
            if proto.operation_id.is_empty() {
                return Err(wrap_err(
                    invalid_working_copy_state(
                        &checkout_path,
                        "combined journal operation ID is empty",
                    )
                    .into(),
                ));
            }
            if proto.workspace_name.is_empty() {
                return Err(wrap_err(
                    invalid_working_copy_state(
                        &checkout_path,
                        "combined journal workspace name is empty",
                    )
                    .into(),
                ));
            }
            let checkout_state = Self {
                operation_id: OperationId::new(proto.operation_id.clone()),
                workspace_name: proto.workspace_name.clone().into(),
            };
            return Ok((checkout_state, Some(proto)));
        }
        let proto = crate::protos::local_working_copy::Checkout::decode(&*buf)
            .map_err(|err| wrap_err(err.into()))?;
        Ok((
            Self {
                operation_id: OperationId::new(proto.operation_id),
                workspace_name: if proto.workspace_name.is_empty() {
                    // For compatibility with old working copies.
                    // TODO: Delete in mid 2022 or so
                    WorkspaceName::DEFAULT.to_owned()
                } else {
                    proto.workspace_name.into()
                },
            },
            None,
        ))
    }

    #[instrument(skip_all)]
    fn save(&self, state_path: &Path) -> Result<(), WorkingCopyStateError> {
        let wrap_err = |err| WorkingCopyStateError {
            message: "Failed to write checkout state".to_owned(),
            err,
        };
        let proto = crate::protos::local_working_copy::Checkout {
            operation_id: self.operation_id.to_bytes(),
            workspace_name: (*self.workspace_name).into(),
        };
        let mut temp_file =
            NamedTempFile::new_in(state_path).map_err(|err| wrap_err(err.into()))?;
        temp_file
            .as_file_mut()
            .write_all(&proto.encode_to_vec())
            .map_err(|err| wrap_err(err.into()))?;
        persist_temp_file(temp_file, state_path.join("checkout"))
            .map_err(|err| wrap_err(err.into()))?;
        Ok(())
    }
}

pub(crate) struct SnapshotLocalWorkingCopy {
    store: Arc<Store>,
    working_copy_path: PathBuf,
    state_path: PathBuf,
    checkout_state: CheckoutState,
    tree_state: OnceCell<TreeState>,
    tree_state_settings: TreeStateSettings,
}

#[async_trait(?Send)]
impl WorkingCopy for SnapshotLocalWorkingCopy {
    fn name(&self) -> &str {
        Self::name()
    }

    fn workspace_name(&self) -> &WorkspaceName {
        &self.checkout_state.workspace_name
    }

    fn operation_id(&self) -> &OperationId {
        &self.checkout_state.operation_id
    }

    fn tree(&self) -> Result<&MergedTree, WorkingCopyStateError> {
        Ok(self.tree_state()?.current_tree())
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        Ok(self.tree_state()?.sparse_patterns())
    }

    async fn start_mutation(&self) -> Result<Box<dyn LockedWorkingCopy>, WorkingCopyStateError> {
        let lock_path = self.state_path.join("working_copy.lock");
        let lock = FileLock::lock(lock_path).map_err(|err| WorkingCopyStateError {
            message: "Failed to lock working copy".to_owned(),
            err: err.into(),
        })?;

        // Re-read one combined journal record after taking the lock. For a
        // legacy split-format working copy, tree state stays lazy; for the
        // compact format, checkout identity and semantic tree come from the
        // exact same decoded bytes.
        let (checkout_state, compact_journal) = CheckoutState::load_with_journal(&self.state_path)?;
        let wc = Self::from_loaded_checkout(
            self.store.clone(),
            self.working_copy_path.clone(),
            self.state_path.clone(),
            checkout_state,
            compact_journal,
            self.tree_state_settings.clone(),
        )?;
        let old_operation_id = wc.operation_id().clone();
        let old_tree = wc.tree()?.clone();
        Ok(Box::new(LockedLocalWorkingCopy {
            wc,
            old_operation_id,
            old_tree,
            tree_state_dirty: false,
            pending_scan: None,
            new_workspace_name: None,
            _lock: lock,
        }))
    }
}

impl SnapshotLocalWorkingCopy {
    fn tree_state_settings(
        user_settings: &UserSettings,
    ) -> Result<TreeStateSettings, WorkingCopyStateError> {
        let mut tree_state_settings = TreeStateSettings::try_from_user_settings(user_settings)
            .map_err(|err| WorkingCopyStateError {
                message: "Failed to read the tree state settings".to_string(),
                err: err.into(),
            })?;
        // Explicit subvolume mode owns its filesystem-monitor backend.
        // User configuration must not silently select a mutable scanner
        // that cannot uphold the committed-baseline invariant. Keep this
        // override even in builds without AWACS support so those builds fail
        // with the AWACS-specific error instead of falling through to a
        // configured Watchman backend.
        #[cfg(debug_assertions)]
        if let Some(scan_root) = std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT") {
            tree_state_settings.fsmonitor_settings = FsmonitorSettings::TestAwacs {
                scan_root: PathBuf::from(scan_root),
                // Before the first baseline TestAwacs still takes the
                // initialization scan below. Once that baseline is committed,
                // model an authoritative empty incremental delta instead of
                // asking snapshot mode to fall back to a forbidden full scan.
                changed_files: Some(vec![]),
                cursor: vec![2; 16],
            };
        } else {
            tree_state_settings.fsmonitor_settings = FsmonitorSettings::Awacs(AwacsConfig {
                #[cfg(all(target_os = "linux", feature = "awacs"))]
                client: None,
            });
        }
        #[cfg(not(debug_assertions))]
        {
            tree_state_settings.fsmonitor_settings = FsmonitorSettings::Awacs(AwacsConfig {
                #[cfg(all(target_os = "linux", feature = "awacs"))]
                client: None,
            });
        }
        Ok(tree_state_settings)
    }

    fn from_loaded_checkout(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        checkout_state: CheckoutState,
        compact_journal: Option<crate::protos::local_working_copy::WorkingCopyState>,
        tree_state_settings: TreeStateSettings,
    ) -> Result<Self, WorkingCopyStateError> {
        let tree_state = if let Some(proto) = compact_journal {
            let journal_path = state_path.join("checkout");
            let tree_state = TreeState::from_working_copy_state(
                store.clone(),
                working_copy_path.clone(),
                state_path.clone(),
                &tree_state_settings,
                &journal_path,
                proto,
            )
            .map_err(|err| WorkingCopyStateError {
                message: "Failed to read working copy state".to_owned(),
                err: Box::new(err),
            })?;
            OnceCell::with_value(tree_state)
        } else {
            OnceCell::new()
        };
        Ok(Self {
            store,
            working_copy_path,
            state_path,
            checkout_state,
            tree_state,
            tree_state_settings,
        })
    }

    fn save_working_copy_state(&mut self) -> Result<(), WorkingCopyStateError> {
        let checkout_state = self.checkout_state.clone();
        if is_snapshot_mode(&self.state_path) {
            self.tree_state_mut()?
                .save_with_checkout(&checkout_state)
                .map_err(|err| WorkingCopyStateError {
                    message: "Failed to write working copy state".to_string(),
                    err: Box::new(err),
                })
        } else {
            self.tree_state_mut()?
                .save()
                .map_err(|err| WorkingCopyStateError {
                    message: "Failed to write working copy state".to_string(),
                    err: Box::new(err),
                })?;
            checkout_state.save(&self.state_path)
        }
    }

    pub fn name() -> &'static str {
        "local"
    }

    /// Initializes a new working copy at `working_copy_path`. The working
    /// copy's state will be stored in the `state_path` directory. The working
    /// copy will have the empty tree checked out.
    pub fn init(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        user_settings: &UserSettings,
    ) -> Result<Self, WorkingCopyStateError> {
        let checkout_state = CheckoutState {
            operation_id,
            workspace_name,
        };
        let tree_state_settings = Self::tree_state_settings(user_settings)?;
        let tree_state = TreeState::init_without_saving(
            store.clone(),
            working_copy_path.clone(),
            state_path.clone(),
            &tree_state_settings,
        );
        let mut wc = Self {
            store,
            working_copy_path,
            state_path,
            checkout_state,
            tree_state: OnceCell::with_value(tree_state),
            tree_state_settings,
        };
        wc.save_working_copy_state()?;
        Ok(wc)
    }

    pub fn load(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        user_settings: &UserSettings,
    ) -> Result<Self, WorkingCopyStateError> {
        let (checkout_state, compact_journal) = CheckoutState::load_with_journal(&state_path)?;
        let tree_state_settings = Self::tree_state_settings(user_settings)?;
        Self::from_loaded_checkout(
            store,
            working_copy_path,
            state_path,
            checkout_state,
            compact_journal,
            tree_state_settings,
        )
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn journal_status(&self) -> Result<WorkingCopyJournalStatus, WorkingCopyStateError> {
        Ok(self.tree_state()?.journal_status())
    }

    #[instrument(skip_all)]
    fn tree_state(&self) -> Result<&TreeState, WorkingCopyStateError> {
        self.tree_state.get_or_try_init(|| {
            TreeState::load(
                self.store.clone(),
                self.working_copy_path.clone(),
                self.state_path.clone(),
                &self.tree_state_settings,
            )
            .map_err(|err| WorkingCopyStateError {
                message: "Failed to read working copy state".to_string(),
                err: err.into(),
            })
        })
    }

    fn tree_state_mut(&mut self) -> Result<&mut TreeState, WorkingCopyStateError> {
        self.tree_state()?; // ensure loaded
        Ok(self.tree_state.get_mut().unwrap())
    }

    #[cfg(feature = "watchman")]
    pub async fn query_watchman(
        &self,
        config: &WatchmanConfig,
    ) -> Result<(watchman::Clock, Option<Vec<PathBuf>>), WorkingCopyStateError> {
        self.tree_state()?
            .query_watchman(config)
            .await
            .map_err(|err| WorkingCopyStateError {
                message: "Failed to query watchman".to_string(),
                err: err.into(),
            })
    }

    #[cfg(feature = "watchman")]
    pub async fn is_watchman_trigger_registered(
        &self,
        config: &WatchmanConfig,
    ) -> Result<bool, WorkingCopyStateError> {
        self.tree_state()?
            .is_watchman_trigger_registered(config)
            .await
            .map_err(|err| WorkingCopyStateError {
                message: "Failed to query watchman".to_string(),
                err: err.into(),
            })
    }
}

/// Standard local-disk working copy.
///
/// Ordinary repositories deliberately keep the pre-snapshot implementation
/// compatible with its legacy tree_state cache. Only an explicit
/// subvolume-mode marker selects the snapshot implementation.
pub struct LocalWorkingCopy {
    pub(crate) inner: LocalWorkingCopyInner,
    pub(crate) reload: Option<LocalWorkingCopyReload>,
}

pub(crate) enum LocalWorkingCopyInner {
    Legacy(crate::legacy_local_working_copy::LocalWorkingCopy),
    Snapshot(SnapshotLocalWorkingCopy),
}

pub(crate) struct LocalWorkingCopyReload {
    store: Arc<Store>,
    working_copy_path: PathBuf,
    state_path: PathBuf,
    user_settings: UserSettings,
}

#[async_trait(?Send)]
impl WorkingCopy for LocalWorkingCopy {
    fn name(&self) -> &str {
        Self::name()
    }

    fn workspace_name(&self) -> &WorkspaceName {
        match &self.inner {
            LocalWorkingCopyInner::Legacy(wc) => wc.workspace_name(),
            LocalWorkingCopyInner::Snapshot(wc) => wc.workspace_name(),
        }
    }

    fn operation_id(&self) -> &OperationId {
        match &self.inner {
            LocalWorkingCopyInner::Legacy(wc) => wc.operation_id(),
            LocalWorkingCopyInner::Snapshot(wc) => wc.operation_id(),
        }
    }

    fn tree(&self) -> Result<&MergedTree, WorkingCopyStateError> {
        match &self.inner {
            LocalWorkingCopyInner::Legacy(wc) => wc.tree(),
            LocalWorkingCopyInner::Snapshot(wc) => wc.tree(),
        }
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        match &self.inner {
            LocalWorkingCopyInner::Legacy(wc) => wc.sparse_patterns(),
            LocalWorkingCopyInner::Snapshot(wc) => wc.sparse_patterns(),
        }
    }

    async fn start_mutation(&self) -> Result<Box<dyn LockedWorkingCopy>, WorkingCopyStateError> {
        if let Some(reload) = &self.reload {
            let marker_enabled = is_snapshot_mode(&reload.state_path);
            let loaded_snapshot = matches!(self.inner, LocalWorkingCopyInner::Snapshot(_));
            if marker_enabled != loaded_snapshot {
                return if marker_enabled {
                    SnapshotLocalWorkingCopy::load(
                        reload.store.clone(),
                        reload.working_copy_path.clone(),
                        reload.state_path.clone(),
                        &reload.user_settings,
                    )?
                    .start_mutation()
                    .await
                } else {
                    crate::legacy_local_working_copy::LocalWorkingCopy::load(
                        reload.store.clone(),
                        reload.working_copy_path.clone(),
                        reload.state_path.clone(),
                        &reload.user_settings,
                    )?
                    .start_mutation()
                    .await
                };
            }
        }
        match &self.inner {
            LocalWorkingCopyInner::Legacy(wc) => wc.start_mutation().await,
            LocalWorkingCopyInner::Snapshot(wc) => wc.start_mutation().await,
        }
    }
}

impl LocalWorkingCopy {
    pub fn name() -> &'static str {
        "local"
    }

    pub fn init(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        user_settings: &UserSettings,
    ) -> Result<Self, WorkingCopyStateError> {
        let reload = LocalWorkingCopyReload {
            store: store.clone(),
            working_copy_path: working_copy_path.clone(),
            state_path: state_path.clone(),
            user_settings: user_settings.clone(),
        };
        let inner = if is_snapshot_mode(&state_path) {
            LocalWorkingCopyInner::Snapshot(SnapshotLocalWorkingCopy::init(
                store,
                working_copy_path,
                state_path,
                operation_id,
                workspace_name,
                user_settings,
            )?)
        } else {
            LocalWorkingCopyInner::Legacy(crate::legacy_local_working_copy::LocalWorkingCopy::init(
                store,
                working_copy_path,
                state_path,
                operation_id,
                workspace_name,
                user_settings,
            )?)
        };
        Ok(Self {
            inner,
            reload: Some(reload),
        })
    }

    pub fn load(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        user_settings: &UserSettings,
    ) -> Result<Self, WorkingCopyStateError> {
        let reload = LocalWorkingCopyReload {
            store: store.clone(),
            working_copy_path: working_copy_path.clone(),
            state_path: state_path.clone(),
            user_settings: user_settings.clone(),
        };
        let inner = if is_snapshot_mode(&state_path) {
            LocalWorkingCopyInner::Snapshot(SnapshotLocalWorkingCopy::load(
                store,
                working_copy_path,
                state_path,
                user_settings,
            )?)
        } else {
            LocalWorkingCopyInner::Legacy(crate::legacy_local_working_copy::LocalWorkingCopy::load(
                store,
                working_copy_path,
                state_path,
                user_settings,
            )?)
        };
        Ok(Self {
            inner,
            reload: Some(reload),
        })
    }

    pub fn state_path(&self) -> &Path {
        match &self.inner {
            LocalWorkingCopyInner::Legacy(wc) => wc.state_path(),
            LocalWorkingCopyInner::Snapshot(wc) => wc.state_path(),
        }
    }

    pub fn journal_status(&self) -> Result<WorkingCopyJournalStatus, WorkingCopyStateError> {
        match &self.inner {
            LocalWorkingCopyInner::Snapshot(wc) => wc.journal_status(),
            LocalWorkingCopyInner::Legacy(_) => Ok(WorkingCopyJournalStatus {
                phase: "NoBaseline",
                generation: 0,
                baseline_backend: None,
                baseline_snapshot_identity: None,
                baseline_retention: None,
                fallback_reason: Some("subvolume mode disabled".to_owned()),
                pending_mutation: None,
            }),
        }
    }

    #[cfg(feature = "watchman")]
    pub async fn query_watchman(
        &self,
        config: &WatchmanConfig,
    ) -> Result<(watchman::Clock, Option<Vec<PathBuf>>), WorkingCopyStateError> {
        match &self.inner {
            LocalWorkingCopyInner::Legacy(wc) => wc.query_watchman(config).await,
            LocalWorkingCopyInner::Snapshot(wc) => wc.query_watchman(config).await,
        }
    }

    #[cfg(feature = "watchman")]
    pub async fn is_watchman_trigger_registered(
        &self,
        config: &WatchmanConfig,
    ) -> Result<bool, WorkingCopyStateError> {
        match &self.inner {
            LocalWorkingCopyInner::Legacy(wc) => wc.is_watchman_trigger_registered(config).await,
            LocalWorkingCopyInner::Snapshot(wc) => wc.is_watchman_trigger_registered(config).await,
        }
    }
}

pub struct LocalWorkingCopyFactory {}

impl WorkingCopyFactory for LocalWorkingCopyFactory {
    fn init_working_copy(
        &self,
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        settings: &UserSettings,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        Ok(Box::new(LocalWorkingCopy::init(
            store,
            working_copy_path,
            state_path,
            operation_id,
            workspace_name,
            settings,
        )?))
    }

    fn load_working_copy(
        &self,
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        settings: &UserSettings,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        Ok(Box::new(LocalWorkingCopy::load(
            store,
            working_copy_path,
            state_path,
            settings,
        )?))
    }
}

/// A working copy that's locked on disk. The lock is held until you call
/// `finish()` or `discard()`.
pub struct LockedLocalWorkingCopy {
    wc: SnapshotLocalWorkingCopy,
    old_operation_id: OperationId,
    old_tree: MergedTree,
    tree_state_dirty: bool,
    pending_scan: Option<PendingScan>,
    new_workspace_name: Option<WorkspaceNameBuf>,
    _lock: FileLock,
}

#[async_trait]
impl LockedWorkingCopy for LockedLocalWorkingCopy {
    fn old_operation_id(&self) -> &OperationId {
        &self.old_operation_id
    }

    fn old_tree(&self) -> &MergedTree {
        &self.old_tree
    }

    async fn snapshot(
        &mut self,
        options: &SnapshotOptions,
    ) -> Result<(MergedTree, SnapshotStats), SnapshotError> {
        self.abort_pending_scan()?;
        let tree_state = self.wc.tree_state_mut()?;
        let (is_dirty, stats, pending_scan) = tree_state.snapshot_with_pending(options).await?;
        self.tree_state_dirty |= is_dirty;
        self.pending_scan = pending_scan;
        Ok((tree_state.current_tree().clone(), stats))
    }

    async fn check_out(&mut self, commit: &Commit) -> Result<CheckoutStats, CheckoutError> {
        let new_tree = commit.tree();
        let tree_changed =
            self.wc.tree_state()?.tree.tree_ids_and_labels() != new_tree.tree_ids_and_labels();
        if tree_changed {
            self.begin_materialization(&new_tree, None, "checkout")?;
            let tree_state = self.wc.tree_state_mut()?;
            let stats = tree_state.check_out(&new_tree)?;
            self.finish_materialization("checkout requires a fresh baseline")
                .await?;
            Ok(stats)
        } else {
            Ok(CheckoutStats::default())
        }
    }

    fn prepare_checkout(&mut self, commit: &Commit) -> Result<(), WorkingCopyStateError> {
        self.begin_materialization(&commit.tree(), None, "checkout")
    }

    fn rename_workspace(&mut self, new_name: WorkspaceNameBuf) {
        self.new_workspace_name = Some(new_name);
    }

    async fn reset(&mut self, commit: &Commit) -> Result<(), ResetError> {
        let new_tree = commit.tree();
        self.begin_materialization(&new_tree, None, "reset")?;
        self.wc.tree_state_mut()?.reset(&new_tree).await?;
        self.finish_materialization("reset requires a fresh baseline")
            .await?;
        Ok(())
    }

    async fn mark_fsmonitor_baseline(&mut self) -> Result<(), SnapshotError> {
        self.abort_pending_scan()?;
        self.clear_fsmonitor_cursor()?;
        let fsmonitor_settings = self.wc.tree_state()?.fsmonitor_settings.clone();
        match fsmonitor_settings {
            #[cfg(feature = "watchman")]
            FsmonitorSettings::Watchman(config) => {
                self.wc
                    .tree_state_mut()?
                    .mark_watchman_baseline(&config)
                    .await
                    .map_err(|err| SnapshotError::Other {
                        message: "Failed to record filesystem monitor baseline".to_string(),
                        err: Box::new(err),
                    })?;
                self.tree_state_dirty = true;
            }
            FsmonitorSettings::Test { .. }
            | FsmonitorSettings::TestAwacs { .. }
            | FsmonitorSettings::Awacs(_)
            | FsmonitorSettings::None => {}
            #[cfg(not(feature = "watchman"))]
            FsmonitorSettings::Watchman(_) => {}
        }
        Ok(())
    }

    async fn recover(&mut self, commit: &Commit) -> Result<(), ResetError> {
        let new_tree = commit.tree();
        self.begin_materialization(&new_tree, None, "recover")?;
        self.wc.tree_state_mut()?.recover(&new_tree).await?;
        self.finish_materialization("recover requires a fresh baseline")
            .await?;
        Ok(())
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        self.wc.sparse_patterns()
    }

    async fn set_sparse_patterns(
        &mut self,
        new_sparse_patterns: Vec<RepoPathBuf>,
    ) -> Result<CheckoutStats, CheckoutError> {
        let sparse_changed = self.wc.sparse_patterns()? != new_sparse_patterns;
        if sparse_changed {
            let tree = self.wc.tree_state()?.current_tree().clone();
            self.begin_materialization(
                &tree,
                Some(new_sparse_patterns.clone()),
                "sparse-patterns",
            )?;
        }
        let stats = self
            .wc
            .tree_state_mut()?
            .set_sparse_patterns(new_sparse_patterns)?;
        if sparse_changed {
            self.finish_materialization("sparse materialization requires a fresh baseline")
                .await?;
        }
        Ok(stats)
    }

    #[instrument(skip_all)]
    async fn finish(
        mut self: Box<Self>,
        operation_id: OperationId,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        assert!(
            self.tree_state_dirty
                || self.old_tree.tree_ids_and_labels() == self.wc.tree()?.tree_ids_and_labels()
        );
        if let Some(pending_scan) = self.pending_scan.as_mut() {
            let started = Instant::now();
            let result = pending_scan.prepare_to_commit();
            tracing::debug!(
                elapsed = ?started.elapsed(),
                succeeded = result.is_ok(),
                "prepared pending filesystem-monitor scan for commit"
            );
            if let Err(err) = result {
                // The tree produced from the immutable scan is still useful,
                // but its cursor is no longer safe to persist. Abort the
                // accepted lease and save the tree without a cursor so the
                // next snapshot must request Full.
                tracing::warn!(
                    ?err,
                    "AWACS scan lease failed before tree-state commit; clearing cursor"
                );
                self.abort_pending_scan()?;
            }
        }
        if self.old_operation_id != operation_id || self.new_workspace_name.is_some() {
            self.wc.checkout_state.operation_id = operation_id;
            if let Some(workspace_name) = self.new_workspace_name.take() {
                self.wc.checkout_state.workspace_name = workspace_name;
            }
            self.tree_state_dirty = true;
        }
        if self.tree_state_dirty {
            let started = Instant::now();
            let result = self.wc.save_working_copy_state();
            tracing::debug!(
                elapsed = ?started.elapsed(),
                succeeded = result.is_ok(),
                "saved working-copy state"
            );
            if let Err(err) = result {
                self.abort_pending_scan()?;
                return Err(err);
            }
        }
        if let Some(pending_scan) = self.pending_scan.take() {
            let started = Instant::now();
            let result = pending_scan.finish(ScanOutcome::Committed);
            tracing::debug!(
                elapsed = ?started.elapsed(),
                succeeded = result.is_ok(),
                "finished committed filesystem-monitor scan session"
            );
            if let Err(err) = result {
                // Tree state already durably contains the cursor. Completion
                // is cleanup only; a failed response is retried or expires
                // server-side.
                tracing::warn!(
                    ?err,
                    "failed to finish committed filesystem-monitor scan session"
                );
            }
        }
        // TODO: Clear the "pending_checkout" file here.
        Ok(Box::new(LocalWorkingCopy {
            inner: LocalWorkingCopyInner::Snapshot(self.wc),
            reload: None,
        }))
    }
}

impl LockedLocalWorkingCopy {
    fn begin_materialization(
        &mut self,
        intended_tree: &MergedTree,
        intended_sparse_patterns: Option<Vec<RepoPathBuf>>,
        mutation_kind: &str,
    ) -> Result<(), WorkingCopyStateError> {
        self.abort_pending_scan()?;
        if !is_snapshot_mode(&self.wc.state_path) {
            // Ordinary working copies keep the legacy split state path. The
            // compact pending-baseline journal is a subvolume-mode concern.
            self.wc.tree_state_mut()?.begin_materialization(
                intended_tree,
                intended_sparse_patterns,
                mutation_kind,
            );
            self.tree_state_dirty = true;
            return Ok(());
        }
        let checkout_state = self.wc.checkout_state.clone();
        let tree_state = self.wc.tree_state_mut()?;
        tree_state.begin_materialization(intended_tree, intended_sparse_patterns, mutation_kind);
        tree_state
            .save_with_checkout(&checkout_state)
            .map_err(|err| WorkingCopyStateError {
                message: "Failed to write pending working-copy materialization".to_owned(),
                err: Box::new(err),
            })?;
        self.tree_state_dirty = true;
        Ok(())
    }

    async fn finish_materialization(&mut self, reason: &str) -> Result<(), WorkingCopyStateError> {
        if snapshot_mode_requires_baseline(&self.wc.state_path) {
            self.pending_scan = self
                .wc
                .tree_state_mut()?
                .finish_snapshot_materialization()
                .await
                .map_err(|err| WorkingCopyStateError {
                    message: "Failed to preserve subvolume working-copy baseline".to_owned(),
                    err: Box::new(err),
                })?;
        } else {
            self.wc.tree_state_mut()?.finish_materialization(reason);
        }
        self.tree_state_dirty = true;
        Ok(())
    }

    pub fn reset_watchman(&mut self) -> Result<(), SnapshotError> {
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        self.release_awacs_baseline()?;
        self.clear_fsmonitor_cursor()?;
        Ok(())
    }

    #[cfg(all(target_os = "linux", feature = "awacs"))]
    fn release_awacs_baseline(&mut self) -> Result<(), SnapshotError> {
        self.abort_pending_scan()?;
        let (config, baseline_owner_id) = {
            let tree_state = self.wc.tree_state()?;
            let FsmonitorSettings::Awacs(config) = &tree_state.fsmonitor_settings else {
                return Ok(());
            };
            let baseline_owner_id = tree_state
                .awacs_baseline_owner_id
                .as_slice()
                .try_into()
                .expect("AWACS baseline owner id is always 16 bytes");
            (config.clone(), baseline_owner_id)
        };
        let client = if let Some(client) = config.client {
            client
        } else {
            let client: Box<dyn btrfs_awacs::scan::ScanClient> = Box::new(
                btrfs_awacs::scan_facade::DirectScanClient::for_root(&self.wc.working_copy_path)
                    .map_err(|err| SnapshotError::Other {
                        message: "Failed to open AWACS root state".to_string(),
                        err: Box::new(err),
                    })?,
            );
            Arc::new(Mutex::new(client))
        };
        client
            .lock()
            .map_err(|_| SnapshotError::Other {
                message: "Failed to release AWACS snapshot baseline".to_string(),
                err: "AWACS client lock was poisoned".into(),
            })?
            .release_baseline(baseline_owner_id)
            .map_err(|err| SnapshotError::Other {
                message: "Failed to release AWACS snapshot baseline".to_string(),
                err: Box::new(err),
            })?;
        self.wc
            .tree_state_mut()?
            .set_no_baseline("AWACS baseline released");
        self.tree_state_dirty = true;
        Ok(())
    }

    fn clear_fsmonitor_cursor(&mut self) -> Result<(), WorkingCopyStateError> {
        if self.wc.tree_state_mut()?.clear_fsmonitor_cursor() {
            self.tree_state_dirty = true;
        }
        Ok(())
    }

    fn abort_pending_scan(&mut self) -> Result<(), WorkingCopyStateError> {
        if let Some(pending_scan) = self.pending_scan.take() {
            drop(pending_scan);
            self.clear_fsmonitor_cursor()?;
        }
        Ok(())
    }
}

/// Clears monitor state for either local-disk engine.
///
/// The concrete locked type changes when an explicit subvolume marker is
/// toggled, so callers which operate during that transition must not assume
/// the snapshot implementation was already loaded.
pub fn reset_local_working_copy_fsmonitor(
    locked: &mut dyn LockedWorkingCopy,
) -> Result<bool, SnapshotError> {
    if let Some(snapshot) = locked.downcast_mut::<LockedLocalWorkingCopy>() {
        snapshot.reset_watchman()?;
        return Ok(true);
    }
    if let Some(legacy) =
        locked.downcast_mut::<crate::legacy_local_working_copy::LockedLocalWorkingCopy>()
    {
        legacy.reset_watchman()?;
        return Ok(true);
    }
    Ok(false)
}

/// Seeds snapshot-backed state with the semantic tree already checked out on
/// disk, without materializing or rewriting files.
///
/// A topology migration copies the working directory before the first AWACS
/// scan. The new compact journal has no prior tree to classify tracked paths
/// from, so callers must provide the existing checkout tree once. This is not
/// a checkout/reset operation: local modifications and untracked files remain
/// on disk for the subsequent snapshot to classify normally.
pub async fn seed_local_working_copy_tree(
    locked: &mut dyn LockedWorkingCopy,
    tree: &MergedTree,
) -> Result<bool, ResetError> {
    let Some(snapshot) = locked.downcast_mut::<LockedLocalWorkingCopy>() else {
        return Ok(false);
    };
    snapshot.wc.tree_state_mut()?.reset(tree).await?;
    // reset() deliberately rewrites the physical tracked-path baseline
    // without changing the semantic commit. Mark that state transition so
    // finish() does not mistake it for an unchecked semantic tree rewrite.
    snapshot.tree_state_dirty = true;
    Ok(true)
}

/// Publishes an AWACS baseline for a tree that the caller has already proved
/// matches the immutable child snapshot, without traversing that snapshot.
///
/// Snapshot-backed workspace creation first clones the parent's path map into
/// an independent child AWACS store and seeds the copied semantic tree into
/// the child journal. At that point a full filesystem walk would only
/// rediscover the same tree. This helper asks AWACS for an immutable child
/// cursor, records it beside the already-seeded tree, and leaves normal later
/// snapshots to consume incremental deltas.
///
/// The caller must use this only before any user-visible child mutation can
/// occur. It is not a general way to bless arbitrary working-copy state.
#[cfg(all(target_os = "linux", feature = "awacs"))]
pub async fn seed_local_working_copy_awacs_baseline(
    locked: &mut dyn LockedWorkingCopy,
    input_fingerprint: [u8; 32],
) -> Result<bool, SnapshotError> {
    let Some(snapshot) = locked.downcast_mut::<LockedLocalWorkingCopy>() else {
        return Ok(false);
    };
    snapshot.abort_pending_scan()?;
    let completion = snapshot
        .wc
        .tree_state_mut()?
        .seed_awacs_baseline_without_scan(input_fingerprint)
        .await?;
    snapshot.pending_scan = completion;
    snapshot.tree_state_dirty = true;
    Ok(true)
}

/// Publishes the already-created child AWACS snapshot as the first local
/// baseline without asking AWACS for a redundant second cut.
///
/// Snapshot workspace creation has just cloned the parent's path map and
/// published an inherited child revision for this exact immutable snapshot.
/// The local journal only needs to bind that snapshot identity to the
/// already-seeded semantic tree. Later direct scans consume the identity and
/// obtain a fresh authenticated cursor with their next cut.
#[cfg(all(target_os = "linux", feature = "awacs"))]
pub async fn seed_local_working_copy_initialized_awacs_baseline(
    locked: &mut dyn LockedWorkingCopy,
    snapshot_identity: &btrfs_awacs::manager::SnapshotIdentity,
    baseline_owner_id: Option<[u8; 16]>,
    input_fingerprint: [u8; 32],
) -> Result<bool, SnapshotError> {
    let Some(snapshot) = locked.downcast_mut::<LockedLocalWorkingCopy>() else {
        return Ok(false);
    };
    snapshot.abort_pending_scan()?;
    let tree_state = snapshot.wc.tree_state_mut()?;
    if tree_state.journal_phase
        != crate::protos::local_working_copy::WorkingCopyStatePhase::NoBaseline
        || tree_state.baseline.is_some()
    {
        return Err(SnapshotError::Other {
            message: "Failed to seed initialized AWACS snapshot baseline".to_owned(),
            err: "working copy already has a committed baseline".into(),
        });
    }
    if let Some(baseline_owner_id) = baseline_owner_id {
        tree_state.awacs_baseline_owner_id = baseline_owner_id.to_vec();
    }
    // Direct AWACS scans key replay from the retained snapshot identity. This
    // non-empty marker distinguishes the bootstrap handoff from an absent
    // baseline; the next scan replaces it with its authenticated cursor.
    let mut continuity_token = b"c:btrfs-awacs:initialized:1:".to_vec();
    continuity_token.extend_from_slice(&snapshot_identity.subvol_uuid);
    let baseline = crate::protos::local_working_copy::AwacsSnapshotBaseline {
        filesystem_uuid: snapshot_identity.fs_uuid.to_vec(),
        subvolume_uuid: snapshot_identity.subvol_uuid.to_vec(),
        continuity_token,
        retention_token: tree_state.awacs_baseline_owner_id.clone(),
        interpretation_input_fingerprint: input_fingerprint.to_vec(),
    };
    tree_state.publish_scan_baseline(None, Some(baseline));
    snapshot.tree_state_dirty = true;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingScanSession {
        outcomes: Arc<std::sync::Mutex<Vec<ScanOutcome>>>,
    }

    struct FailingPrepareScanSession {
        outcomes: Arc<std::sync::Mutex<Vec<ScanOutcome>>>,
    }

    impl ScanSession for RecordingScanSession {
        fn finish(
            self: Box<Self>,
            outcome: ScanOutcome,
        ) -> Result<(), Box<dyn Error + Send + Sync>> {
            self.outcomes.lock().unwrap().push(outcome);
            Ok(())
        }
    }

    impl ScanSession for FailingPrepareScanSession {
        fn prepare_to_commit(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
            Err("lease renewal failed".into())
        }

        fn finish(
            self: Box<Self>,
            outcome: ScanOutcome,
        ) -> Result<(), Box<dyn Error + Send + Sync>> {
            self.outcomes.lock().unwrap().push(outcome);
            Ok(())
        }
    }

    #[test]
    fn pending_scan_aborts_on_drop_and_commits_explicitly() {
        let aborted = Arc::new(std::sync::Mutex::new(Vec::new()));
        drop(PendingScan::new(Box::new(RecordingScanSession {
            outcomes: aborted.clone(),
        })));
        assert_eq!(aborted.lock().unwrap().as_slice(), &[ScanOutcome::Aborted]);

        let committed = Arc::new(std::sync::Mutex::new(Vec::new()));
        PendingScan::new(Box::new(RecordingScanSession {
            outcomes: committed.clone(),
        }))
        .finish(ScanOutcome::Committed)
        .unwrap();
        assert_eq!(
            committed.lock().unwrap().as_slice(),
            &[ScanOutcome::Committed]
        );

        let failed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut pending = PendingScan::new(Box::new(FailingPrepareScanSession {
            outcomes: failed.clone(),
        }));
        assert!(pending.prepare_to_commit().is_err());
        drop(pending);
        assert_eq!(failed.lock().unwrap().as_slice(), &[ScanOutcome::Aborted]);
    }
}
