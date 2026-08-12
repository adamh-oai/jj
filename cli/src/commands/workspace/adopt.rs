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

#[cfg(all(target_os = "linux", feature = "awacs"))]
use std::collections::BTreeSet;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(all(target_os = "linux", feature = "awacs"))]
use std::process::Command;

use jj_lib::backend::CommitId;
use jj_lib::git;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use jj_lib::local_working_copy::seed_local_working_copy_tree;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use jj_lib::lock::FileLock;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::workspace::Workspace;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
#[cfg(all(target_os = "linux", feature = "awacs"))]
use crate::commands::btrfs::begin_subvolume_mode;
use crate::commands::btrfs::is_btrfs_subvolume;
use crate::commands::btrfs::is_subvolume_mode_enabled;
use crate::commands::btrfs::set_subvolume_mode;
use crate::ui::Ui;

/// Adopt the current linked Git worktree as a jj workspace.
///
/// The worktree must belong to the Git repository backing an existing
/// Git-colocated jj workspace. Adoption creates `.jj` metadata in place and
/// preserves the existing Git worktree, files, HEAD, and index.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspaceAdoptArgs {
    /// A name for the adopted workspace
    ///
    /// To override the default, which is the basename of the worktree root.
    #[arg(long)]
    name: Option<WorkspaceNameBuf>,
}

struct ExistingGitWorktree {
    root: PathBuf,
    common_dir: PathBuf,
    head_id: CommitId,
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
struct GitAwacsIndexState {
    baseline: btrfs_awacs::scan::SnapshotBaseline,
    force_paths: Vec<RepoPathBuf>,
    // Keep Git from rewriting the checksummed index between validation and
    // publication of the corresponding JJ baseline.
    _index_lock: FileLock,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_adopt(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspaceAdoptArgs,
) -> Result<(), CommandError> {
    let worktree = discover_linked_git_worktree(command.cwd())?;
    let main_workspace_root = worktree
        .common_dir
        .parent()
        .ok_or_else(|| user_error("Cannot locate the main Git worktree"))?;
    let (main_settings, _config_env) =
        command.settings_for_new_workspace(ui, main_workspace_root)?;
    let main_workspace = command.load_workspace_at(main_workspace_root, &main_settings)?;
    if !crate::git_util::is_colocated_git_workspace(&main_workspace) {
        return Err(user_error(
            "The main Git worktree is not an existing Git-colocated jj workspace",
        ));
    }

    let git_backend = git::get_git_backend(main_workspace.repo_loader().store()).map_err(|_| {
        internal_error_with_message(
            "Git-colocated workspace does not use a Git-backed repository",
            "missing Git backend",
        )
    })?;
    let backend_git_dir = dunce::canonicalize(git_backend.git_repo_path())
        .unwrap_or_else(|_| git_backend.git_repo_path().to_owned());
    if backend_git_dir != worktree.common_dir {
        return Err(user_error(
            "The linked Git worktree does not belong to the jj repository's Git backend",
        ));
    }
    let snapshot = is_subvolume_mode_enabled(main_workspace_root);
    if snapshot && !is_btrfs_subvolume(&worktree.root)? {
        return Err(user_error(
            "Cannot adopt a plain Git worktree while Btrfs subvolume mode is enabled",
        )
        .hinted("Adopt a linked Git worktree whose root is already a Btrfs subvolume."));
    }
    #[cfg(all(target_os = "linux", feature = "awacs"))]
    let git_awacs_index = if snapshot {
        Some(read_git_awacs_index_state(
            &worktree,
            git_backend.git_executable_path(),
        )?)
    } else {
        None
    };

    let workspace_name = workspace_name(&worktree.root, args)?;
    let op = command.resolve_operation(
        ui,
        main_workspace.repo_loader(),
        main_workspace.workspace_name(),
    )?;
    let repo = main_workspace.repo_loader().load_at(&op).await?;
    if repo.view().get_wc_commit_id(&workspace_name).is_some() {
        return Err(user_error(format!(
            "Workspace named '{name}' already exists",
            name = workspace_name.as_symbol()
        )));
    }

    if !repo.index().has_id(&worktree.head_id).await? {
        git_backend
            .import_head_commits([&worktree.head_id])
            .map_err(|err| user_error(format!("Failed to import Git HEAD: {err}")))?;
    }
    let head_commit = repo.store().get_commit_async(&worktree.head_id).await?;
    let working_copy_factory = command.get_working_copy_factory_at(main_workspace_root)?;
    let (mut workspace, repo) = Workspace::init_workspace_with_existing_repo(
        &worktree.root,
        main_workspace.repo_path(),
        &repo,
        working_copy_factory,
        workspace_name.clone(),
    )
    .await?;
    if snapshot {
        #[cfg(all(target_os = "linux", feature = "awacs"))]
        begin_subvolume_mode(&worktree.root)?;
        #[cfg(not(all(target_os = "linux", feature = "awacs")))]
        set_subvolume_mode(&worktree.root, true)?;
    }

    // `init_workspace_with_existing_repo()` starts at the root commit. Replace
    // that placeholder with an empty working-copy commit on the existing Git
    // HEAD, then reset only jj's working-copy metadata. The files and Git index
    // are already materialized by Git and must not be checked out again.
    let mut tx = repo.start_transaction();
    let wc_commit = tx
        .repo_mut()
        .check_out(workspace_name.clone(), &head_commit)
        .await?;
    tx.repo_mut()
        .set_git_head_target(&workspace_name, RefTarget::normal(worktree.head_id.clone()));
    tx.repo_mut().rebase_descendants().await?;
    let unpublished = tx
        .write(format!(
            "adopt existing Git worktree as workspace '{}'",
            workspace_name.as_symbol()
        ))
        .await?;
    let repo = if command.should_commit_transaction() {
        unpublished.publish().await?
    } else {
        unpublished.leave_unpublished()
    };

    #[cfg(all(target_os = "linux", feature = "awacs"))]
    if snapshot {
        let git_awacs_index = git_awacs_index.expect("snapshot adoption validated Git index");
        // Workspace initialization happened before the marker existed and
        // therefore owns the ordinary working-copy implementation. Reload
        // after the marker is durable so the compact journal is selected.
        drop(workspace);
        let mut workspace_command = command
            .workspace_helper_no_snapshot_at(ui, &worktree.root)
            .await?;
        seed_snapshot_adopt_tree(&mut workspace_command, &wc_commit).await?;
        workspace_command
            .seed_git_index_snapshot_workspace_awacs_baseline(ui, &git_awacs_index.baseline)
            .await?;
        set_subvolume_mode(&worktree.root, true)?;
        // The index lock is only needed until its cursor and cached paths are
        // durably bound to the compact journal. Subsequent filesystem changes
        // are covered by AWACS' cursor delta.
        let GitAwacsIndexState {
            force_paths,
            _index_lock,
            ..
        } = git_awacs_index;
        drop(_index_lock);
        workspace_command
            .maybe_snapshot_with_force_paths(ui, force_paths)
            .await?;
        writeln!(
            ui.status(),
            "Adopted Git worktree as workspace '{}'",
            workspace_name.as_symbol()
        )?;
        return Ok(());
    }

    let mut locked_workspace = workspace.start_working_copy_mutation().await?;
    locked_workspace.locked_wc().reset(&wc_commit).await?;
    locked_workspace.finish(repo.op_id().clone()).await?;

    let mut workspace_command = command.for_workable_repo(ui, workspace, repo)?;
    workspace_command.maybe_snapshot(ui).await?;
    writeln!(
        ui.status(),
        "Adopted Git worktree as workspace '{}'",
        workspace_name.as_symbol()
    )?;
    Ok(())
}

fn discover_linked_git_worktree(cwd: &Path) -> Result<ExistingGitWorktree, CommandError> {
    let repo = gix::discover(cwd)
        .map_err(|err| user_error(format!("Failed to discover Git worktree: {err}")))?;
    let root = repo
        .workdir()
        .ok_or_else(|| user_error("Cannot adopt a bare Git repository"))?;
    let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    let git_dir = dunce::canonicalize(repo.git_dir()).unwrap_or_else(|_| repo.git_dir().into());
    let common_dir =
        dunce::canonicalize(repo.common_dir()).unwrap_or_else(|_| repo.common_dir().into());
    if git_dir == common_dir {
        return Err(user_error(
            "Cannot adopt the main Git worktree; run this from a linked Git worktree",
        ));
    }
    let head_id = repo
        .head_id()
        .map(|id| CommitId::from_bytes(id.as_bytes()))
        .map_err(|err| user_error(format!("Cannot adopt a Git worktree without HEAD: {err}")))?;
    Ok(ExistingGitWorktree {
        root,
        common_dir,
        head_id,
    })
}

fn workspace_name(
    workspace_root: &Path,
    args: &WorkspaceAdoptArgs,
) -> Result<WorkspaceNameBuf, CommandError> {
    let name = if let Some(name) = &args.name {
        name.to_owned()
    } else {
        workspace_root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| user_error("Workspace root name is not valid UTF-8"))?
            .into()
    };
    if name.as_str().is_empty() {
        return Err(user_error("New workspace name cannot be empty"));
    }
    Ok(name)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
async fn seed_snapshot_adopt_tree(
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
    commit: &jj_lib::commit::Commit,
) -> Result<(), CommandError> {
    let operation_id = workspace_command.repo().op_id().clone();
    let (mut locked_workspace, _commit) = workspace_command
        .unchecked_start_working_copy_mutation()
        .await?;
    if !seed_local_working_copy_tree(locked_workspace.locked_wc(), &commit.tree())
        .await
        .map_err(|err| internal_error_with_message("Failed to seed adopted snapshot tree", err))?
    {
        return Err(internal_error_with_message(
            "Failed to seed adopted snapshot working copy",
            "new workspace did not reload as snapshot-backed",
        ));
    }
    locked_workspace.finish(operation_id).await?;
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn read_git_awacs_index_state(
    worktree: &ExistingGitWorktree,
    git_executable: &Path,
) -> Result<GitAwacsIndexState, CommandError> {
    let git_repo = gix::discover(&worktree.root)
        .map_err(|err| user_error(format!("Failed to reopen Git worktree: {err}")))?;
    let index_path = git_repo.index_path();
    let index_lock = FileLock::lock(index_path.with_extension("lock")).map_err(|err| {
        user_error(format!(
            "Cannot adopt snapshot worktree while its Git index is in use: {err}"
        ))
    })?;
    let index = git_repo
        .index()
        .map_err(|err| user_error(format!("Failed to validate Git index: {err}")))?;
    if index.link().is_some() || index.is_sparse() {
        return Err(user_error(
            "Cannot adopt snapshot worktree with a split or sparse Git index",
        ));
    }
    if index.entries().iter().any(|entry| entry.stage_raw() != 0) {
        return Err(user_error(
            "Cannot adopt snapshot worktree with unmerged Git index entries",
        ));
    }
    let bytes = fs::read(&index_path)
        .map_err(|err| user_error(format!("Failed to read Git index: {err}")))?;
    let hash_len = git_repo.object_hash().len_in_bytes();
    verify_index_checksum(&bytes, git_repo.object_hash()).map_err(user_error)?;
    let extensions = parse_index_extensions(&bytes, hash_len).map_err(|err| {
        user_error(format!(
            "Cannot adopt snapshot worktree from Git index: {err}"
        ))
    })?;
    let fsmonitor = extension_data(&extensions, b"FSMN")
        .ok_or_else(|| user_error("Git index has no fsmonitor cache extension"))?;
    let untracked = extension_data(&extensions, b"UNTR")
        .ok_or_else(|| user_error("Git index has no untracked-cache extension"))?;
    let (baseline, dirty_indices) = parse_git_awacs_fsmonitor(fsmonitor).map_err(user_error)?;
    let mut force_paths = BTreeSet::new();
    for index_position in dirty_indices {
        let entry = index
            .entries()
            .get(index_position)
            .ok_or_else(|| user_error("Git fsmonitor bitmap refers to a missing index entry"))?;
        add_repo_path(&mut force_paths, entry.path(&index))?;
    }
    for path in parse_fully_valid_untracked_cache(untracked, hash_len).map_err(user_error)? {
        add_repo_path(&mut force_paths, &path)?;
    }
    add_staged_paths(&mut force_paths, git_executable, &worktree.root)?;
    Ok(GitAwacsIndexState {
        baseline,
        force_paths: force_paths.into_iter().collect(),
        _index_lock: index_lock,
    })
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn verify_index_checksum(bytes: &[u8], hash_kind: gix::hash::Kind) -> Result<(), String> {
    let hash_len = hash_kind.len_in_bytes();
    let body_len = bytes
        .len()
        .checked_sub(hash_len)
        .ok_or_else(|| "Git index has no checksum trailer".to_owned())?;
    let mut hasher = gix::hash::hasher(hash_kind);
    hasher.update(&bytes[..body_len]);
    let actual = hasher
        .try_finalize()
        .map_err(|err| format!("failed to hash Git index: {err}"))?;
    if actual.as_slice() != &bytes[body_len..] {
        return Err("Git index checksum does not match its contents".to_owned());
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn add_staged_paths(
    paths: &mut BTreeSet<RepoPathBuf>,
    git_executable: &Path,
    worktree_root: &Path,
) -> Result<(), CommandError> {
    let output = Command::new(git_executable)
        .arg("-C")
        .arg(worktree_root)
        .args([
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--no-renames",
            "--ignore-submodules=none",
        ])
        .output()
        .map_err(|err| user_error(format!("Failed to inspect staged Git paths: {err}")))?;
    if !output.status.success() {
        return Err(user_error(format!(
            "Failed to inspect staged Git paths: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    for path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        add_repo_path(paths, path)?;
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn add_repo_path(paths: &mut BTreeSet<RepoPathBuf>, path: &[u8]) -> Result<(), CommandError> {
    let path = std::str::from_utf8(path)
        .map_err(|_| user_error("Cannot adopt snapshot worktree with non-UTF-8 Git paths"))?;
    let path = RepoPathBuf::from_internal_string(path)
        .map_err(|err| user_error(format!("Invalid Git index path: {err}")))?;
    paths.insert(path);
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn extension_data<'a>(
    extensions: &'a [([u8; 4], &'a [u8])],
    signature: &[u8; 4],
) -> Option<&'a [u8]> {
    extensions
        .iter()
        .find_map(|(found, data)| (found == signature).then_some(*data))
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn parse_index_extensions(bytes: &[u8], hash_len: usize) -> Result<Vec<([u8; 4], &[u8])>, String> {
    if bytes.len() < 12 + hash_len || &bytes[..4] != b"DIRC" {
        return Err("index header is missing or malformed".to_owned());
    }
    let version = read_be_u32(&bytes[4..8])?;
    if !(2..=4).contains(&version) {
        return Err(format!("unsupported Git index version {version}"));
    }
    let entries = read_be_u32(&bytes[8..12])? as usize;
    let mut offset = 12usize;
    for _ in 0..entries {
        let entry_start = offset;
        offset = offset
            .checked_add(40 + hash_len)
            .filter(|offset| *offset + 2 <= bytes.len())
            .ok_or_else(|| "truncated Git index entry".to_owned())?;
        let flags = u16::from_be_bytes(bytes[offset..offset + 2].try_into().unwrap());
        offset += 2;
        if flags & 0x4000 != 0 {
            offset = offset
                .checked_add(2)
                .filter(|offset| *offset <= bytes.len())
                .ok_or_else(|| "truncated extended Git index flags".to_owned())?;
        }
        if version == 4 {
            offset = skip_leb128(bytes, offset)?;
            offset = skip_nul(bytes, offset)?;
        } else {
            let path_len = (flags & 0x0fff) as usize;
            offset = if path_len == 0x0fff {
                skip_nul(bytes, offset)?
            } else {
                offset
                    .checked_add(path_len)
                    .filter(|offset| *offset <= bytes.len())
                    .ok_or_else(|| "truncated Git index path".to_owned())?
            };
            let padded_len = (offset - entry_start + 8) & !7;
            offset = entry_start
                .checked_add(padded_len)
                .filter(|offset| *offset <= bytes.len())
                .ok_or_else(|| "truncated Git index padding".to_owned())?;
        }
    }
    let extension_end = bytes
        .len()
        .checked_sub(hash_len)
        .ok_or_else(|| "Git index has no checksum trailer".to_owned())?;
    if offset > extension_end {
        return Err("Git index entries overlap checksum trailer".to_owned());
    }
    let mut extensions = Vec::new();
    while offset < extension_end {
        if extension_end - offset < 8 {
            return Err("truncated Git index extension header".to_owned());
        }
        let signature: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
        let size = read_be_u32(&bytes[offset + 4..offset + 8])? as usize;
        offset += 8;
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= extension_end)
            .ok_or_else(|| "truncated Git index extension".to_owned())?;
        extensions.push((signature, &bytes[offset..end]));
        offset = end;
    }
    Ok(extensions)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn parse_git_awacs_fsmonitor(
    data: &[u8],
) -> Result<(btrfs_awacs::scan::SnapshotBaseline, Vec<usize>), String> {
    let mut offset = 0;
    let version = take_be_u32(data, &mut offset)?;
    if version != 2 {
        return Err("Git fsmonitor cache is not protocol v2".to_owned());
    }
    let token = take_nul(data, &mut offset)?;
    let baseline = decode_git_awacs_token(token)?;
    let ewah_size = take_be_u32(data, &mut offset)? as usize;
    let ewah_end = offset
        .checked_add(ewah_size)
        .filter(|end| *end == data.len())
        .ok_or_else(|| "malformed Git fsmonitor dirty bitmap".to_owned())?;
    let (dirty, consumed) = decode_ewah(&data[offset..ewah_end])?;
    if consumed != ewah_size {
        return Err("trailing bytes in Git fsmonitor dirty bitmap".to_owned());
    }
    Ok((
        baseline,
        dirty
            .into_iter()
            .enumerate()
            .filter_map(|(index, dirty)| dirty.then_some(index))
            .collect(),
    ))
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn decode_git_awacs_token(token: &[u8]) -> Result<btrfs_awacs::scan::SnapshotBaseline, String> {
    let mut fields = token.splitn(4, |byte| *byte == b':');
    if fields.next() != Some(b"awacs-git-v1".as_slice()) {
        return Err("Git fsmonitor token is not an AWACS v1 token".to_owned());
    }
    let filesystem_uuid = parse_uuid_bytes(
        fields
            .next()
            .ok_or_else(|| "AWACS token has no filesystem UUID".to_owned())?,
    )?;
    let subvolume_uuid = parse_uuid_bytes(
        fields
            .next()
            .ok_or_else(|| "AWACS token has no subvolume UUID".to_owned())?,
    )?;
    let continuity_token = fields
        .next()
        .filter(|token| token.starts_with(b"c:btrfs-awacs:"))
        .ok_or_else(|| "AWACS token has no valid continuity proof".to_owned())?
        .to_vec();
    Ok(btrfs_awacs::scan::SnapshotBaseline {
        identity: btrfs_awacs::scan::SnapshotIdentity {
            filesystem_uuid,
            subvolume_uuid,
            read_only: true,
        },
        continuity_token,
        retention_token: Vec::new(),
    })
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn parse_uuid_bytes(value: &[u8]) -> Result<[u8; 16], String> {
    if value.len() != 36
        || value.get(8) != Some(&b'-')
        || value.get(13) != Some(&b'-')
        || value.get(18) != Some(&b'-')
        || value.get(23) != Some(&b'-')
    {
        return Err("AWACS token UUID is malformed".to_owned());
    }
    let mut bytes = [0; 16];
    let mut output = 0;
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'-' {
            index += 1;
            continue;
        }
        let high =
            hex_nibble(value[index]).ok_or_else(|| "AWACS token UUID is malformed".to_owned())?;
        let low = hex_nibble(value[index + 1])
            .ok_or_else(|| "AWACS token UUID is malformed".to_owned())?;
        bytes[output] = high << 4 | low;
        output += 1;
        index += 2;
    }
    if bytes == [0; 16] {
        return Err("AWACS token UUID is zero".to_owned());
    }
    Ok(bytes)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
#[derive(Clone)]
struct CachedUntrackedDirectory {
    name: Vec<u8>,
    entries: Vec<Vec<u8>>,
    children: Vec<usize>,
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn parse_fully_valid_untracked_cache(data: &[u8], hash_len: usize) -> Result<Vec<Vec<u8>>, String> {
    let mut offset = 0;
    let identifier_len = take_leb128(data, &mut offset)? as usize;
    take_bytes(data, &mut offset, identifier_len)?;
    // UNTR stores ctime, mtime, dev, ino, uid, gid, and size, but not the
    // regular index entry's mode field.
    take_bytes(data, &mut offset, 2 * (36 + hash_len))?;
    let dir_flags = take_be_u32(data, &mut offset)?;
    if dir_flags != 0 {
        return Err(format!(
            "Git untracked cache was not populated with --untracked-files=all (flags {dir_flags:#x})"
        ));
    }
    take_nul(data, &mut offset)?;
    let directory_count = take_leb128(data, &mut offset)? as usize;
    if directory_count == 0 {
        return Err("Git untracked cache has no directory inventory".to_owned());
    }
    let mut directories = Vec::with_capacity(directory_count);
    parse_untracked_directory(data, &mut offset, &mut directories)?;
    if directories.len() != directory_count || directories[0].name != b"" {
        return Err("Git untracked cache directory inventory is malformed".to_owned());
    }
    let (valid, consumed) = decode_ewah(&data[offset..])?;
    offset += consumed;
    let (check_only, consumed) = decode_ewah(&data[offset..])?;
    offset += consumed;
    let (_hash_valid, consumed) = decode_ewah(&data[offset..])?;
    offset += consumed;
    if valid.len() < directory_count
        || valid.iter().take(directory_count).any(|valid| !valid)
        || check_only
            .iter()
            .take(directory_count)
            .any(|check_only| *check_only)
    {
        return Err("Git untracked cache has invalid or collapsed directories".to_owned());
    }
    // The remaining bytes are per-directory stat/hash payloads. gix already
    // decoded and checksum-validated this same index before we inspected its
    // cache projections, so only ensure our parser did not run past it.
    if offset > data.len() {
        return Err("Git untracked cache payload is malformed".to_owned());
    }
    let mut paths = Vec::new();
    collect_untracked_paths(&directories, 0, &[], &mut paths)?;
    Ok(paths)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn parse_untracked_directory(
    data: &[u8],
    offset: &mut usize,
    directories: &mut Vec<CachedUntrackedDirectory>,
) -> Result<usize, String> {
    let entry_count = take_leb128(data, offset)? as usize;
    let child_count = take_leb128(data, offset)? as usize;
    let name = take_nul(data, offset)?.to_vec();
    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        entries.push(take_nul(data, offset)?.to_vec());
    }
    let index = directories.len();
    directories.push(CachedUntrackedDirectory {
        name,
        entries,
        children: Vec::with_capacity(child_count),
    });
    for _ in 0..child_count {
        let child = parse_untracked_directory(data, offset, directories)?;
        directories[index].children.push(child);
    }
    Ok(index)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn collect_untracked_paths(
    directories: &[CachedUntrackedDirectory],
    index: usize,
    parent: &[u8],
    paths: &mut Vec<Vec<u8>>,
) -> Result<(), String> {
    let directory = directories
        .get(index)
        .ok_or_else(|| "Git untracked cache child index is invalid".to_owned())?;
    let prefix = join_git_path(parent, &directory.name);
    for entry in &directory.entries {
        paths.push(join_git_path(&prefix, entry));
    }
    for child in &directory.children {
        collect_untracked_paths(directories, *child, &prefix, paths)?;
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn join_git_path(parent: &[u8], child: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        return child.to_vec();
    }
    if child.is_empty() {
        return parent.to_vec();
    }
    let mut path = Vec::with_capacity(parent.len() + 1 + child.len());
    path.extend_from_slice(parent);
    path.push(b'/');
    path.extend_from_slice(child);
    path
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn decode_ewah(data: &[u8]) -> Result<(Vec<bool>, usize), String> {
    if data.len() < 8 {
        return Err("truncated Git EWAH bitmap".to_owned());
    }
    let bit_size = read_be_u32(&data[..4])? as usize;
    let word_count = read_be_u32(&data[4..8])? as usize;
    let payload_len = 8usize
        .checked_add(
            word_count
                .checked_mul(8)
                .ok_or_else(|| "Git EWAH bitmap is too large".to_owned())?,
        )
        .and_then(|len| len.checked_add(4))
        .ok_or_else(|| "Git EWAH bitmap is too large".to_owned())?;
    if payload_len > data.len() {
        return Err("truncated Git EWAH bitmap".to_owned());
    }
    let mut words = Vec::with_capacity(word_count);
    for chunk in data[8..8 + word_count * 8].chunks_exact(8) {
        words.push(u64::from_be_bytes(chunk.try_into().unwrap()));
    }
    let mut bits = Vec::with_capacity(bit_size);
    let mut word_index = 0;
    while bits.len() < bit_size {
        let run_length_word = *words
            .get(word_index)
            .ok_or_else(|| "Git EWAH bitmap ended early".to_owned())?;
        word_index += 1;
        let repeated = run_length_word & 1 != 0;
        let run_words = ((run_length_word >> 1) & 0xffff_ffff) as usize;
        let literal_words = (run_length_word >> 33) as usize;
        for _ in 0..run_words {
            for _ in 0..64 {
                if bits.len() == bit_size {
                    break;
                }
                bits.push(repeated);
            }
        }
        for _ in 0..literal_words {
            let literal = *words
                .get(word_index)
                .ok_or_else(|| "Git EWAH literal word is missing".to_owned())?;
            word_index += 1;
            for bit in 0..64 {
                if bits.len() == bit_size {
                    break;
                }
                bits.push(literal & (1 << bit) != 0);
            }
        }
    }
    if word_index > word_count {
        return Err("Git EWAH bitmap overruns its payload".to_owned());
    }
    Ok((bits, payload_len))
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn read_be_u32(data: &[u8]) -> Result<u32, String> {
    data.get(..4)
        .map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()))
        .ok_or_else(|| "truncated Git index integer".to_owned())
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn take_be_u32(data: &[u8], offset: &mut usize) -> Result<u32, String> {
    let bytes = take_bytes(data, offset, 4)?;
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn take_leb128(data: &[u8], offset: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *take_bytes(data, offset, 1)?
            .first()
            .expect("one byte was requested");
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= 64 {
            return Err("Git index varint is too large".to_owned());
        }
    }
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn skip_leb128(data: &[u8], mut offset: usize) -> Result<usize, String> {
    take_leb128(data, &mut offset)?;
    Ok(offset)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn take_nul<'a>(data: &'a [u8], offset: &mut usize) -> Result<&'a [u8], String> {
    let tail = data
        .get(*offset..)
        .ok_or_else(|| "truncated Git index string".to_owned())?;
    let length = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| "unterminated Git index string".to_owned())?;
    let value = &tail[..length];
    *offset += length + 1;
    Ok(value)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn skip_nul(data: &[u8], mut offset: usize) -> Result<usize, String> {
    take_nul(data, &mut offset)?;
    Ok(offset)
}

#[cfg(all(target_os = "linux", feature = "awacs"))]
fn take_bytes<'a>(data: &'a [u8], offset: &mut usize, length: usize) -> Result<&'a [u8], String> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| "Git index offset overflow".to_owned())?;
    let bytes = data
        .get(*offset..end)
        .ok_or_else(|| "truncated Git index payload".to_owned())?;
    *offset = end;
    Ok(bytes)
}
