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

use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use crate::command_error::CommandError;
use crate::command_error::user_error;

const SUBVOLUME_MODE_MARKER: &str = "subvolume_mode";
#[cfg(all(target_os = "linux", feature = "awacs"))]
const SUBVOLUME_MODE_ENABLING: &[u8] = b"enabling\n";

pub(super) fn subvolume_mode_marker(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".jj")
        .join("working_copy")
        .join(SUBVOLUME_MODE_MARKER)
}

pub(super) fn is_subvolume_mode_enabled(workspace_root: &Path) -> bool {
    subvolume_mode_marker(workspace_root).is_file()
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
pub(super) fn is_subvolume_mode_committed(workspace_root: &Path) -> bool {
    fs::read(subvolume_mode_marker(workspace_root))
        .is_ok_and(|marker| marker == b"snapshot-backed\n")
}

pub(super) fn set_subvolume_mode(workspace_root: &Path, enabled: bool) -> Result<(), CommandError> {
    let marker = subvolume_mode_marker(workspace_root);
    if enabled {
        fs::write(&marker, b"snapshot-backed\n")
            .map_err(|err| user_error(format!("Failed to enable subvolume mode: {err}")))?;
        tracing::info!(state = "snapshot-backed", "updated subvolume mode marker");
    } else if let Err(err) = fs::remove_file(&marker)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(user_error(format!(
            "Failed to disable subvolume mode: {err}"
        )));
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
pub(super) fn begin_subvolume_mode(workspace_root: &Path) -> Result<(), CommandError> {
    let marker = subvolume_mode_marker(workspace_root);
    fs::write(&marker, SUBVOLUME_MODE_ENABLING)
        .map_err(|err| user_error(format!("Failed to begin subvolume mode: {err}")))?;
    tracing::info!(state = "enabling", "updated subvolume mode marker");
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BtrfsMountOptions {
    user_subvol_rm_allowed: bool,
}

pub(super) fn btrfs_command() -> Command {
    Command::new("btrfs")
}

pub(super) fn subvolume_operation_lock_path(
    workspace_root: &Path,
) -> Result<PathBuf, CommandError> {
    let parent = workspace_root
        .parent()
        .ok_or_else(|| user_error("Repository root has no parent directory"))?;
    let name = workspace_root
        .file_name()
        .ok_or_else(|| user_error("Repository root has no final component"))?;
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(name);
    lock_name.push(".jj-subvolume.lock");
    Ok(parent.join(lock_name))
}

/// Snapshots the source and all descendant subvolumes into a writable root.
pub(super) fn recursive_btrfs_snapshot(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(feature = "test-fakes")]
    pause_recursive_snapshot_for_test()?;

    #[cfg(all(target_os = "linux", feature = "awacs"))]
    if !use_test_btrfs_cli() {
        return btrfs_awacs::recursive_snapshot::recursive_snapshot(source, destination);
    }

    // Synthetic filesystem tests model the entire recursive result with their
    // snapshot command. The Linux AWACS build uses the shared ioctl backend.
    let output = btrfs_command()
        .args(["subvolume", "snapshot"])
        .arg(source)
        .arg(destination)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Btrfs snapshot failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

#[cfg(feature = "test-fakes")]
fn pause_recursive_snapshot_for_test() -> io::Result<()> {
    let Some(path) = std::env::var_os("JJ_TEST_RECURSIVE_SNAPSHOT_PAUSE_DIR") else {
        return Ok(());
    };
    let path = Path::new(&path);
    std::fs::write(path.join("ready"), b"ready\n")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !path.join("release").exists() {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "recursive snapshot test barrier timed out",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

/// Returns whether `path` is on a Btrfs filesystem.
///
/// Unlike `btrfs subvolume show`, this does not require permission to read
/// subvolume metadata. Callers use it only after a subvolume operation fails,
/// to distinguish a non-Btrfs path from an actual operation failure.
pub(super) fn is_btrfs_path(path: &Path) -> Result<bool, CommandError> {
    let output = btrfs_command()
        .args(["inspect-internal", "rootid"])
        .arg(path)
        .output()
        .map_err(|err| user_error(format!("Failed to inspect Btrfs path: {err}")))?;
    Ok(output.status.success())
}

/// Returns the ID of the Btrfs subvolume containing `path`.
///
/// The ID is stable across renames, so callers that already verified a
/// subvolume root can use it to avoid deleting a later pathname replacement.
pub(super) fn btrfs_subvolume_id(path: &Path) -> Result<Option<u64>, CommandError> {
    let output = btrfs_command()
        .args(["inspect-internal", "rootid"])
        .arg(path)
        .output()
        .map_err(|err| user_error(format!("Failed to inspect Btrfs path: {err}")))?;
    if !output.status.success() {
        return Ok(None);
    }
    let root_id = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|err| user_error(format!("Failed to parse Btrfs subvolume ID: {err}")))?;
    Ok(Some(root_id))
}

/// Returns whether `path` is the root directory of a Btrfs subvolume.
///
/// `inspect-internal rootid` reports the containing subvolume for every path
/// on Btrfs. The root directory itself is identified by its fixed inode number.
pub(super) fn is_btrfs_subvolume(path: &Path) -> Result<bool, CommandError> {
    if !is_btrfs_path(path)? {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        let metadata = fs::metadata(path)
            .map_err(|err| user_error(format!("Failed to inspect Btrfs subvolume: {err}")))?;
        if metadata.ino() == 256 {
            return Ok(true);
        }
    }

    // Some filesystem adapters do not expose the Btrfs root inode through
    // metadata. Ask btrfs-progs to verify that this exact path is a subvolume
    // root before treating the stable root ID as deletable.
    let output = btrfs_command()
        .args(["subvolume", "show"])
        .arg(path)
        .output()
        .map_err(|err| user_error(format!("Failed to inspect Btrfs subvolume: {err}")))?;
    Ok(output.status.success())
}

/// Identifies a removal target without depending on `btrfs` being installed or
/// requiring metadata-search privileges. Failed inspection on Linux is an
/// error, not evidence that deleting the directory tree is safe.
pub(super) fn is_btrfs_subvolume_for_removal(path: &Path) -> Result<bool, CommandError> {
    #[cfg(target_os = "linux")]
    {
        #[cfg(feature = "test-fakes")]
        if std::env::var_os("JJ_TEST_BTRFS_CLI").is_some_and(|value| value == "1")
            || std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_some()
        {
            return is_btrfs_subvolume(path);
        }
        use std::os::fd::AsRawFd as _;

        let metadata = fs::metadata(path)
            .map_err(|err| user_error(format!("Failed to inspect removal target: {err}")))?;
        if metadata.ino() != 256 {
            return Ok(false);
        }
        let directory = fs::File::open(path)
            .map_err(|err| user_error(format!("Failed to open removal target: {err}")))?;
        let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
        // SAFETY: the open descriptor stays live and the output is only read
        // after fstatfs has successfully initialized it.
        if unsafe { libc::fstatfs(directory.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
            return Err(user_error(format!(
                "Failed to inspect removal target filesystem: {}",
                io::Error::last_os_error()
            )));
        }
        let filesystem = unsafe { filesystem.assume_init() };
        Ok(filesystem.f_type as u32 == 0x9123_683e)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Btrfs detection remains best-effort on platforms without a native
        // Btrfs filesystem interface.
        Ok(is_btrfs_subvolume(path).unwrap_or(false))
    }
}

pub(super) fn btrfs_user_subvol_rm_allowed(path: &Path) -> io::Result<Option<bool>> {
    Ok(btrfs_mount_options(path)?.map(|options| options.user_subvol_rm_allowed))
}

/// Removes a directory tree, destroying any encountered Btrfs subvolumes rather
/// than unlinking their contents individually.
pub(super) fn remove_directory_tree(path: &Path) -> io::Result<()> {
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    {
        btrfs_awacs::btrfs::remove_tree(path)
    }
    #[cfg(not(all(target_os = "linux", feature = "awacs")))]
    {
        fs::remove_dir_all(path)
    }
}

/// Attempts to delete a subvolume, including its nested subvolumes. Returns
/// `Ok(false)` if the target is not a Btrfs subvolume root.
pub(super) fn delete_btrfs_subvolume(path: &Path) -> Result<bool, CommandError> {
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    if !use_test_btrfs_cli() {
        return match btrfs_awacs::btrfs::remove_subvolume_tree(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(false),
            Err(error) => Err(user_error(format!(
                "Failed to delete Btrfs subvolume: {error}"
            ))),
        };
    }

    // Builds without the Linux AWACS backend retain their existing adapter.
    // The production Linux backend never invokes a PATH-selected deletion
    // command; test fakes must explicitly opt into that behavior.
    let output = btrfs_command()
        .args(["subvolume", "delete"])
        .arg(path)
        .output()
        .map_err(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                user_error("Failed to delete Btrfs subvolume: `btrfs` command is not installed")
            } else {
                user_error(format!("Failed to delete Btrfs subvolume: {err}"))
            }
        })?;
    if !output.status.success() {
        if !is_btrfs_path(path)? {
            return Ok(false);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(user_error(format!(
            "Failed to delete Btrfs subvolume: {stderr}"
        )));
    }
    Ok(true)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn use_test_btrfs_cli() -> bool {
    #[cfg(feature = "test-fakes")]
    {
        std::env::var_os("JJ_TEST_BTRFS_CLI").is_some_and(|value| value == "1")
            || std::env::var_os("JJ_TEST_AWACS_SCAN_ROOT").is_some()
    }
    #[cfg(not(feature = "test-fakes"))]
    {
        false
    }
}

fn btrfs_mount_options(path: &Path) -> io::Result<Option<BtrfsMountOptions>> {
    #[cfg(target_os = "linux")]
    {
        let path = dunce::canonicalize(path)?;
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
        Ok(parse_btrfs_mount_options(&mountinfo, &path))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Ok(None)
    }
}

#[cfg(target_os = "linux")]
fn parse_btrfs_mount_options(mountinfo: &str, path: &Path) -> Option<BtrfsMountOptions> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (mount, filesystem) = line.split_once(" - ")?;
            let mut mount_fields = mount.split_whitespace();
            let mount_point = decode_mount_point(mount_fields.nth(4)?);
            let mount_options = mount_fields.next()?;
            let mut filesystem_fields = filesystem.split_whitespace();
            let is_btrfs = filesystem_fields.next()? == "btrfs";
            filesystem_fields.next()?;
            let filesystem_options = filesystem_fields.next()?;
            if !path.starts_with(&mount_point) {
                return None;
            }
            let user_subvol_rm_allowed = mount_options
                .split(',')
                .chain(filesystem_options.split(','))
                .any(|option| option == "user_subvol_rm_allowed");
            Some((
                mount_point.components().count(),
                is_btrfs,
                BtrfsMountOptions {
                    user_subvol_rm_allowed,
                },
            ))
        })
        .max_by_key(|(depth, _, _)| *depth)
        .and_then(|(_, is_btrfs, options)| is_btrfs.then_some(options))
}

#[cfg(target_os = "linux")]
fn decode_mount_point(mount_point: &str) -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let bytes = mount_point.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..=index + 3]
                .iter()
                .all(|byte| (b'0'..=b'7').contains(byte))
        {
            decoded.push(
                (bytes[index + 1] - b'0') * 64
                    + (bytes[index + 2] - b'0') * 8
                    + (bytes[index + 3] - b'0'),
            );
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    PathBuf::from(OsString::from_vec(decoded))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::path::Path;

    use super::BtrfsMountOptions;
    use super::parse_btrfs_mount_options;

    #[test]
    fn test_parse_btrfs_mount_options() {
        let mountinfo = "26 22 0:23 / / rw,relatime - btrfs /dev/vda rw,user_subvol_rm_allowed\n";
        assert_eq!(
            parse_btrfs_mount_options(mountinfo, Path::new("/work/repo")),
            Some(BtrfsMountOptions {
                user_subvol_rm_allowed: true,
            })
        );
    }

    #[test]
    fn test_parse_btrfs_mount_options_uses_innermost_mount() {
        let mountinfo = "26 22 0:23 / / rw,relatime - btrfs /dev/vda \
                         rw,user_subvol_rm_allowed\n27 26 0:24 / /work rw,relatime - tmpfs tmpfs \
                         rw\n";
        assert_eq!(
            parse_btrfs_mount_options(mountinfo, Path::new("/work/repo")),
            None
        );
    }

    #[test]
    fn test_parse_btrfs_mount_options_decodes_mount_point() {
        let mountinfo =
            "26 22 0:23 / /work\\040tree rw,user_subvol_rm_allowed - btrfs /dev/vda rw\n";
        assert_eq!(
            parse_btrfs_mount_options(mountinfo, Path::new("/work tree/repo")),
            Some(BtrfsMountOptions {
                user_subvol_rm_allowed: true,
            })
        );
    }
}
