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
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::command_error::CommandError;
use crate::command_error::user_error;

const SUBVOLUME_MODE_MARKER: &str = "subvolume_mode";
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

pub(super) fn set_subvolume_mode(workspace_root: &Path, enabled: bool) -> Result<(), CommandError> {
    let marker = subvolume_mode_marker(workspace_root);
    if enabled {
        fs::write(&marker, b"snapshot-backed\n")
            .map_err(|err| user_error(format!("Failed to enable subvolume mode: {err}")))?;
    } else if let Err(err) = fs::remove_file(&marker)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(user_error(format!(
            "Failed to disable subvolume mode: {err}"
        )));
    }
    Ok(())
}

pub(super) fn begin_subvolume_mode(workspace_root: &Path) -> Result<(), CommandError> {
    let marker = subvolume_mode_marker(workspace_root);
    fs::write(&marker, SUBVOLUME_MODE_ENABLING)
        .map_err(|err| user_error(format!("Failed to begin subvolume mode: {err}")))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BtrfsMountOptions {
    user_subvol_rm_allowed: bool,
}

pub(super) fn btrfs_command() -> Command {
    Command::new("btrfs")
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

pub(super) fn btrfs_user_subvol_rm_allowed(path: &Path) -> io::Result<Option<bool>> {
    Ok(btrfs_mount_options(path)?.map(|options| options.user_subvol_rm_allowed))
}

/// Attempts to delete a subvolume. Returns `Ok(false)` if the operation
/// failed because the target is not on Btrfs.
pub(super) fn delete_btrfs_subvolume(path: &Path) -> Result<bool, CommandError> {
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
        let mountinfo = "26 22 0:23 / / rw,relatime - btrfs /dev/vda rw,user_subvol_rm_allowed\n\
                         27 26 0:24 / /work rw,relatime - tmpfs tmpfs rw\n";
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
