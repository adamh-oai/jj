use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::io::Read as _;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const FS_NOCOMP_FL: u32 = 0x0000_0400;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MigrationOptions {
    pub compression: Option<bool>,
    pub keep_temporary_on_drop: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotPlan {
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Debug)]
struct PreparedSubvolume {
    destination: PathBuf,
    temporary: PathBuf,
}

#[derive(Debug)]
pub struct SubvolumeMigration {
    replacements: Vec<PreparedSubvolume>,
    pending_snapshots: Vec<SnapshotPlan>,
    keep_temporary_on_drop: bool,
    committed: bool,
}

#[derive(Debug)]
pub struct CommittedMigration {
    displaced: Vec<PathBuf>,
}

impl CommittedMigration {
    pub fn displaced_path(&self) -> Option<&Path> {
        self.displaced.first().map(PathBuf::as_path)
    }

    pub fn discard_displaced(mut self) -> Result<(), MigrationError> {
        while let Some(path) = self.displaced.pop() {
            remove_existing_destination(&path)?;
        }
        Ok(())
    }
}

impl SubvolumeMigration {
    /// Prepares the root and its .git/.jj directories as separate subvolumes.
    ///
    /// All copies are prepared before publication. Without compression changes,
    /// existing nested metadata subvolumes are left for the caller to snapshot
    /// recursively into the temporary root. Other copies preserve encountered
    /// subvolume boundaries while rewriting their files when requested.
    pub fn prepare(
        destination: impl AsRef<Path>,
        options: MigrationOptions,
    ) -> Result<Self, MigrationError> {
        let destination = destination.as_ref();
        if !destination.is_dir() {
            return Err(MigrationError::new(format!(
                "subvolume migration destination is not a directory: {}",
                destination.display()
            )));
        }
        let mut migration = Self {
            replacements: Vec::new(),
            pending_snapshots: Vec::new(),
            keep_temporary_on_drop: options.keep_temporary_on_drop,
            committed: false,
        };
        let mut metadata_directories = Vec::new();
        for name in [".git", ".jj"] {
            let path = destination.join(name);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(MigrationError::new(format!(
                        "cannot migrate symbolic-link metadata at {}",
                        path.display()
                    )));
                }
                Ok(metadata) if metadata.is_dir() => metadata_directories.push((name, path)),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(MigrationError::io("inspect metadata directory", error)),
            }
        }

        if !is_btrfs_subvolume(destination)? {
            let temporary = unique_sibling(destination, "tmp")?;
            let mut guard =
                TemporaryPathGuard::new(temporary.clone(), options.keep_temporary_on_drop);
            create_btrfs_subvolume(&temporary)?;
            if let Some(compression) = options.compression {
                set_btrfs_compression(&temporary, compression)?;
            }
            let excluded = metadata_directories
                .iter()
                .map(|(name, _)| OsStr::new(name))
                .collect::<Vec<_>>();
            copy_children_except_names(
                destination,
                &temporary,
                &excluded,
                options.compression.is_none(),
            )?;
            for (name, source) in metadata_directories {
                let target = temporary.join(name);
                if options.compression.is_none() && is_btrfs_subvolume(&source)? {
                    migration.pending_snapshots.push(SnapshotPlan {
                        source,
                        destination: target,
                    });
                } else {
                    create_btrfs_subvolume(&target)?;
                    if let Some(compression) = options.compression {
                        set_btrfs_compression(&target, compression)?;
                    }
                    copy_children(&source, &target, options.compression.is_none())?;
                    let metadata = fs::symlink_metadata(&source)
                        .map_err(|error| MigrationError::io("inspect metadata source", error))?;
                    if let Some(source_subvolume) = open_real_subvolume(&source, &metadata)? {
                        preserve_subvolume_readonly(&source_subvolume, &target)?;
                    }
                }
            }
            migration.replacements.push(PreparedSubvolume {
                destination: destination.to_owned(),
                temporary,
            });
            guard.disarm();
        } else {
            for (_, metadata) in metadata_directories {
                if is_btrfs_subvolume(&metadata)? {
                    continue;
                }
                let temporary = unique_sibling(&metadata, "tmp")?;
                let mut guard =
                    TemporaryPathGuard::new(temporary.clone(), options.keep_temporary_on_drop);
                create_btrfs_subvolume(&temporary)?;
                if let Some(compression) = options.compression {
                    set_btrfs_compression(&temporary, compression)?;
                }
                copy_children(&metadata, &temporary, options.compression.is_none())?;
                migration.replacements.push(PreparedSubvolume {
                    destination: metadata,
                    temporary,
                });
                guard.disarm();
            }
        }
        Ok(migration)
    }

    pub fn is_noop(&self) -> bool {
        self.replacements.is_empty()
    }

    pub fn pending_snapshots(&self) -> &[SnapshotPlan] {
        &self.pending_snapshots
    }

    /// Publishes the prepared subvolumes, restoring earlier replacements if
    /// any rename fails. Displaced originals remain available until the caller
    /// explicitly discards them.
    pub fn commit(mut self) -> Result<CommittedMigration, MigrationError> {
        for plan in &self.pending_snapshots {
            if !plan.destination.is_dir() || !is_btrfs_subvolume(&plan.destination)? {
                return Err(MigrationError::new(format!(
                    "required snapshot destination is not a subvolume: {}",
                    plan.destination.display()
                )));
            }
        }
        let mut published = Vec::new();
        for index in 0..self.replacements.len() {
            let destination = self.replacements[index].destination.clone();
            let temporary = self.replacements[index].temporary.clone();
            let displaced = match unique_sibling(&destination, "source") {
                Ok(displaced) => displaced,
                Err(error) => return Err(self.rollback_published(&published, error)),
            };
            if let Err(error) = fs::rename(&destination, &displaced) {
                let error =
                    MigrationError::io(format!("move {} aside", destination.display()), error);
                return Err(self.rollback_published(&published, error));
            }
            if let Err(error) = fs::rename(&temporary, &destination) {
                let error = match fs::rename(&displaced, &destination) {
                    Ok(()) => MigrationError::io("publish temporary subvolume", error),
                    Err(restore_error) => {
                        self.keep_temporary_on_drop = true;
                        MigrationError::new(format!(
                            "publish temporary subvolume: {error}; restore {} from {}: \
                             {restore_error}",
                            destination.display(),
                            displaced.display()
                        ))
                    }
                };
                return Err(self.rollback_published(&published, error));
            }
            published.push((index, displaced));
        }
        self.committed = true;
        Ok(CommittedMigration {
            displaced: published.into_iter().map(|(_, path)| path).collect(),
        })
    }

    fn rollback_published(
        &mut self,
        published: &[(usize, PathBuf)],
        error: MigrationError,
    ) -> MigrationError {
        let mut failures = Vec::new();
        for (index, displaced) in published.iter().rev() {
            let replacement = &self.replacements[*index];
            if let Err(error) = fs::rename(&replacement.destination, &replacement.temporary) {
                failures.push(format!(
                    "move {} back: {error}",
                    replacement.destination.display()
                ));
                continue;
            }
            if let Err(error) = fs::rename(displaced, &replacement.destination) {
                failures.push(format!(
                    "restore {}: {error}",
                    replacement.destination.display()
                ));
            }
        }
        if failures.is_empty() {
            error
        } else {
            // Leave every remaining copy intact for manual recovery.
            self.keep_temporary_on_drop = true;
            MigrationError::new(format!("{error}; rollback failed: {}", failures.join("; ")))
        }
    }
}

/// Converts an existing directory to separate root/.git/.jj subvolumes.
///
/// Both `awacs init` and JJ's explicit enable command use this entry point so
/// topology conversion is not duplicated across binaries.
pub fn convert_subvolume_root(
    destination: impl AsRef<Path>,
    options: MigrationOptions,
    mut progress: impl FnMut(&'static str),
) -> Result<CommittedMigration, MigrationError> {
    let destination = destination.as_ref().to_owned();
    progress("preparing Btrfs subvolume conversion");
    let migration = SubvolumeMigration::prepare(&destination, options)?;
    for plan in migration.pending_snapshots() {
        progress("recursively snapshotting existing subvolumes");
        let source = crate::btrfs::OpenedSubvolume::open(&plan.source)
            .map_err(|error| MigrationError::new(format!("open source subvolume: {error}")))?;
        // These roots are descendants of the converted checkout, so retain
        // their own read-only flags as well as those of deeper snapshots.
        copy_subvolume(&plan.source, &plan.destination, source, true)?;
    }
    progress("publishing Btrfs subvolume conversion");
    let committed = migration.commit()?;
    progress("enabling Git fsmonitor");
    enable_git_fsmonitor(&destination)?;
    Ok(committed)
}

/// Enables Git's selected fsmonitor implementation only when the worktree
/// has no explicit local setting. Roots without usable Git metadata remain
/// valid, and no hook path or protocol version is installed.
fn enable_git_fsmonitor(destination: &Path) -> Result<(), MigrationError> {
    if !destination.join(".git").exists() {
        return Ok(());
    }
    let probe = Command::new("git")
        .arg("-C")
        .arg(destination)
        .args(["rev-parse", "--git-dir"])
        .output()
        .map_err(|error| MigrationError::io("probe Git worktree", error))?;
    if !probe.status.success() {
        return Ok(());
    }
    let existing = Command::new("git")
        .arg("-C")
        .arg(destination)
        .args(["config", "--local", "--get", "core.fsmonitor"])
        .output()
        .map_err(|error| MigrationError::io("read local Git fsmonitor configuration", error))?;
    if existing.status.success() {
        return Ok(());
    }
    if existing.status.code() != Some(1) {
        return Err(MigrationError::new(format!(
            "read local Git fsmonitor configuration exited with {}",
            existing.status
        )));
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(destination)
        .args(["config", "--local", "core.fsmonitor", "true"])
        .status()
        .map_err(|error| MigrationError::io("enable Git fsmonitor", error))?;
    if !status.success() {
        return Err(MigrationError::new(format!(
            "enable Git fsmonitor exited with {status}"
        )));
    }
    Ok(())
}

impl Drop for SubvolumeMigration {
    fn drop(&mut self) {
        if self.committed || self.keep_temporary_on_drop {
            return;
        }
        for replacement in &self.replacements {
            let _ = remove_existing_destination(&replacement.temporary);
        }
    }
}

struct TemporaryPathGuard {
    path: Option<PathBuf>,
    keep_on_drop: bool,
}

impl TemporaryPathGuard {
    fn new(path: PathBuf, keep_on_drop: bool) -> Self {
        Self {
            path: Some(path),
            keep_on_drop,
        }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TemporaryPathGuard {
    fn drop(&mut self) {
        if self.keep_on_drop {
            return;
        }
        if let Some(path) = self.path.take() {
            let _ = remove_existing_destination(&path);
        }
    }
}

/// Copies children while preserving nested Btrfs subvolume boundaries and their
/// read-only flags. Snapshot copies share extents; disabling reflinks rewrites
/// files into new subvolumes that inherit the destination's compression policy.
pub fn copy_children(
    source: &Path,
    destination: &Path,
    allow_reflinks: bool,
) -> Result<(), MigrationError> {
    for entry in fs::read_dir(source).map_err(|error| MigrationError::io("read source", error))? {
        let entry = entry.map_err(|error| MigrationError::io("read source entry", error))?;
        let target = destination.join(entry.file_name());
        copy_entry(&entry.path(), &target, allow_reflinks)?;
    }
    Ok(())
}

pub fn copy_children_except(
    source: &Path,
    destination: &Path,
    excluded_name: &OsStr,
    allow_reflinks: bool,
) -> Result<(), MigrationError> {
    copy_children_except_names(source, destination, &[excluded_name], allow_reflinks)
}

pub fn copy_children_except_names(
    source: &Path,
    destination: &Path,
    excluded_names: &[&OsStr],
    allow_reflinks: bool,
) -> Result<(), MigrationError> {
    for entry in fs::read_dir(source).map_err(|error| MigrationError::io("read source", error))? {
        let entry = entry.map_err(|error| MigrationError::io("read source entry", error))?;
        if excluded_names.contains(&entry.file_name().as_os_str()) {
            continue;
        }
        copy_entry(
            &entry.path(),
            &destination.join(entry.file_name()),
            allow_reflinks,
        )?;
    }
    Ok(())
}

fn copy_entry(
    source: &Path,
    destination: &Path,
    allow_reflinks: bool,
) -> Result<(), MigrationError> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| MigrationError::io(format!("inspect {}", source.display()), error))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        remove_existing_non_directory(destination)?;
        let target = fs::read_link(source)
            .map_err(|error| MigrationError::io(format!("read {}", source.display()), error))?;
        std::os::unix::fs::symlink(target, destination).map_err(|error| {
            MigrationError::io(format!("create {}", destination.display()), error)
        })?;
    } else if file_type.is_dir() {
        if let Some(subvolume) = open_real_subvolume(source, &metadata)? {
            return copy_subvolume(source, destination, subvolume, allow_reflinks);
        }
        if destination.exists() {
            if !fs::symlink_metadata(destination)
                .map_err(|error| MigrationError::io("inspect destination", error))?
                .file_type()
                .is_dir()
            {
                remove_existing_destination(destination)?;
                fs::create_dir(destination)
                    .map_err(|error| MigrationError::io("create destination", error))?;
            }
        } else {
            fs::create_dir(destination)
                .map_err(|error| MigrationError::io("create destination", error))?;
        }
        copy_children(source, destination, allow_reflinks)?;
        fs::set_permissions(destination, metadata.permissions())
            .map_err(|error| MigrationError::io("copy directory permissions", error))?;
    } else if file_type.is_file() {
        remove_existing_non_directory(destination)?;
        if !allow_reflinks {
            copy_file_by_reading(source, destination)?;
        } else if !try_reflink_file(source, destination)? {
            fs::copy(source, destination)
                .map_err(|error| MigrationError::io("copy regular file", error))?;
        }
        fs::set_permissions(destination, metadata.permissions())
            .map_err(|error| MigrationError::io("copy file permissions", error))?;
    } else if file_type.is_socket() {
        return Ok(());
    } else {
        return Err(MigrationError::new(format!(
            "cannot copy special file during subvolume migration: {}",
            source.display()
        )));
    }
    Ok(())
}

/// Ordinary directories avoid both helper processes and filesystem ioctls.
/// Inode 256 alone is not sufficient on another filesystem, so verify Btrfs
/// before interpreting the candidate as a subvolume root. Errors on an actual
/// Btrfs root must not silently flatten it into an ordinary directory.
fn open_real_subvolume(
    source: &Path,
    metadata: &fs::Metadata,
) -> Result<Option<crate::btrfs::OpenedSubvolume>, MigrationError> {
    if metadata.ino() != crate::btrfs::ROOT_INODE {
        return Ok(None);
    }
    let file = fs::File::open(source)
        .map_err(|error| MigrationError::io("open candidate subvolume", error))?;
    // SAFETY: fstatfs initializes the supplied structure using a live fd.
    let mut filesystem = unsafe { std::mem::zeroed::<libc::statfs>() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut filesystem) } != 0 {
        return Err(MigrationError::io(
            "inspect candidate subvolume filesystem",
            io::Error::last_os_error(),
        ));
    }
    if filesystem.f_type != libc::BTRFS_SUPER_MAGIC {
        return Ok(None);
    }
    crate::btrfs::OpenedSubvolume::open(source)
        .map(Some)
        .map_err(|error| MigrationError::new(format!("open source subvolume: {error}")))
}

fn copy_subvolume(
    source: &Path,
    destination: &Path,
    source_subvolume: crate::btrfs::OpenedSubvolume,
    allow_reflinks: bool,
) -> Result<(), MigrationError> {
    match fs::symlink_metadata(destination) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(MigrationError::io("inspect subvolume destination", error)),
        Ok(_) => {
            return Err(MigrationError::new(format!(
                "cannot replace existing destination while copying subvolume: {}",
                destination.display()
            )));
        }
    }
    if allow_reflinks {
        crate::recursive_snapshot::recursive_snapshot(source, destination)
            .map_err(|error| MigrationError::io("recursively copy nested subvolume", error))?;
    } else {
        // Btrfs inherits positive compression properties across subvolume
        // boundaries, but neither the "none" property nor NOCOMPRESS flags.
        // Carry that policy explicitly before creating files or descendants.
        create_btrfs_subvolume(destination)?;
        let parent = destination
            .parent()
            .ok_or_else(|| MigrationError::new("copied subvolume has no parent"))?;
        if inode_flags(parent)
            .map_err(|error| MigrationError::io("inspect destination compression flags", error))?
            & FS_NOCOMP_FL
            != 0
        {
            set_btrfs_compression(destination, false)?;
        }
        copy_children(source, destination, false)?;
        let permissions = fs::metadata(source)
            .map_err(|error| MigrationError::io("inspect copied subvolume permissions", error))?
            .permissions();
        fs::set_permissions(destination, permissions)
            .map_err(|error| MigrationError::io("copy subvolume permissions", error))?;
    }
    preserve_subvolume_readonly(&source_subvolume, destination)
}

fn inode_flags(path: &Path) -> io::Result<u32> {
    // Linux UAPI: FS_IOC_GETFLAGS = _IOR('f', 1, long). Despite the encoded
    // size, filesystems return a u32, including on 64-bit architectures.
    const FS_IOC_GETFLAGS: libc::c_ulong =
        0x8000_0000 | ((size_of::<libc::c_long>() as libc::c_ulong) << 16) | (0x66 << 8) | 1;
    let file = fs::File::open(path)?;
    let mut flags = 0_u32;
    // SAFETY: the live fd and writable u32 satisfy the GETFLAGS ioctl ABI.
    if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags)
}

fn preserve_subvolume_readonly(
    source: &crate::btrfs::OpenedSubvolume,
    destination: &Path,
) -> Result<(), MigrationError> {
    if source.subvolume.readonly() {
        let copied = crate::btrfs::OpenedSubvolume::open(destination)
            .map_err(|error| MigrationError::new(format!("open copied subvolume: {error}")))?;
        crate::btrfs::set_subvolume_readonly(copied.as_fd(), true).map_err(|error| {
            MigrationError::new(format!("preserve copied subvolume read-only flag: {error}"))
        })?;
    }
    Ok(())
}

fn copy_file_by_reading(source: &Path, destination: &Path) -> Result<(), MigrationError> {
    let mut input = fs::File::open(source)
        .map_err(|error| MigrationError::io("open file for extent rewrite", error))?;
    let mut output = fs::File::create(destination)
        .map_err(|error| MigrationError::io("create file for extent rewrite", error))?;
    // fs::copy and io::copy may use copy_file_range, which can share Btrfs
    // extents. Explicit reads and writes ensure compression is reapplied.
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let length = match input.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(|error| MigrationError::io("read extent rewrite", error))?,
        };
        if length == 0 {
            return Ok(());
        }
        output
            .write_all(&buffer[..length])
            .map_err(|error| MigrationError::io("write extent rewrite", error))?;
    }
}

fn try_reflink_file(source: &Path, destination: &Path) -> Result<bool, MigrationError> {
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let source_file =
        fs::File::open(source).map_err(|error| MigrationError::io("open reflink source", error))?;
    let destination_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(destination)
        .map_err(|error| MigrationError::io("open reflink destination", error))?;
    let result = unsafe {
        libc::ioctl(
            destination_file.as_raw_fd(),
            FICLONE,
            source_file.as_raw_fd(),
        )
    };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::EINVAL) => {
            drop(destination_file);
            fs::remove_file(destination)
                .map_err(|error| MigrationError::io("remove failed reflink", error))?;
            Ok(false)
        }
        _ => Err(MigrationError::io("reflink file", error)),
    }
}

fn is_btrfs_subvolume(path: &Path) -> Result<bool, MigrationError> {
    if !is_btrfs_path(path)? {
        return Ok(false);
    }
    if fs::metadata(path)
        .map_err(|error| MigrationError::io("inspect subvolume", error))?
        .ino()
        == 256
    {
        return Ok(true);
    }
    let output = btrfs_command()
        .args(["subvolume", "show"])
        .arg(path)
        .output()
        .map_err(|error| MigrationError::io("inspect subvolume", error))?;
    Ok(output.status.success())
}

fn is_btrfs_path(path: &Path) -> Result<bool, MigrationError> {
    let output = btrfs_command()
        .args(["inspect-internal", "rootid"])
        .arg(path)
        .output()
        .map_err(|error| MigrationError::io("inspect Btrfs path", error))?;
    Ok(output.status.success())
}

fn create_btrfs_subvolume(path: &Path) -> Result<(), MigrationError> {
    let output = btrfs_command()
        .args(["subvolume", "create"])
        .arg(path)
        .output()
        .map_err(|error| MigrationError::io("create Btrfs subvolume", error))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(MigrationError::new(format!(
            "create Btrfs subvolume at {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn set_btrfs_compression(path: &Path, compress: bool) -> Result<(), MigrationError> {
    let value = if compress { "zstd" } else { "none" };
    let output = btrfs_command()
        .args(["property", "set"])
        .arg(path)
        .args(["compression", value])
        .output()
        .map_err(|error| MigrationError::io("set Btrfs compression", error))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(MigrationError::new(format!(
            "set Btrfs compression on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn remove_existing_non_directory(path: &Path) -> Result<(), MigrationError> {
    if path.exists() {
        if fs::symlink_metadata(path)
            .map_err(|error| MigrationError::io("inspect destination", error))?
            .file_type()
            .is_dir()
        {
            return Err(MigrationError::new(format!(
                "cannot replace directory while copying subvolume contents: {}",
                path.display()
            )));
        }
        fs::remove_file(path).map_err(|error| MigrationError::io("remove destination", error))?;
    }
    Ok(())
}

fn remove_existing_destination(path: &Path) -> Result<(), MigrationError> {
    match crate::btrfs::remove_tree(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(MigrationError::io(
            format!("remove {}", path.display()),
            error,
        )),
    }
}

fn unique_sibling(path: &Path, action: &str) -> Result<PathBuf, MigrationError> {
    let parent = path
        .parent()
        .ok_or_else(|| MigrationError::new("subvolume path has no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| MigrationError::new("subvolume path has no final component"))?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..16 {
        let candidate = parent.join(format!(
            ".{name}.awacs-subvolume-{action}-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(MigrationError::new(
        "failed to choose temporary subvolume path",
    ))
}

fn btrfs_command() -> Command {
    Command::new("btrfs")
}

#[derive(Debug)]
pub struct MigrationError {
    message: String,
}

impl MigrationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(context: impl fmt::Display, error: io::Error) -> Self {
        Self::new(format!("{context}: {error}"))
    }
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MigrationError {}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Mutex;
    use std::sync::OnceLock;

    use tempfile::tempdir;

    use super::*;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn fake_btrfs(root: &Path) -> PathBuf {
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let script = bin.join("btrfs");
        fs::write(
            &script,
            r#"#!/bin/sh
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    test -f "$3/.fake-subvolume"
    exit $?
fi
if [ "$1" = "subvolume" ] && [ "$2" = "create" ]; then
    mkdir -p "$3"
    touch "$3/.fake-subvolume"
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    rm -rf "$3"
    exit 0
fi
if [ "$1" = "property" ] && [ "$2" = "set" ]; then
    if [ "$BTRFS_MIGRATION_FAIL_PROPERTY" = "1" ]; then
        exit 1
    fi
    echo "$@" >> "$BTRFS_MIGRATION_LOG"
    exit 0
fi
exit 1
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        bin
    }

    fn with_fake_btrfs(test: impl FnOnce(&Path, &Path)) {
        let _lock = env_lock();
        let temp = tempdir().unwrap();
        let bin = fake_btrfs(temp.path());
        let log = temp.path().join("btrfs.log");
        let old_path = std::env::var_os("PATH");
        let old_log = std::env::var_os("BTRFS_MIGRATION_LOG");
        let old_fail_property = std::env::var_os("BTRFS_MIGRATION_FAIL_PROPERTY");
        let mut paths = vec![bin];
        if let Some(path) = &old_path {
            paths.extend(std::env::split_paths(path));
        }
        // SAFETY: env_lock() serializes every environment mutation in this
        // test module before any helper process is spawned.
        unsafe {
            std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
            std::env::set_var("BTRFS_MIGRATION_LOG", &log);
        }
        test(temp.path(), &log);
        // SAFETY: env_lock() remains held while the original process
        // environment is restored.
        unsafe {
            if let Some(path) = old_path {
                std::env::set_var("PATH", path);
            } else {
                std::env::remove_var("PATH");
            }
            if let Some(path) = old_log {
                std::env::set_var("BTRFS_MIGRATION_LOG", path);
            } else {
                std::env::remove_var("BTRFS_MIGRATION_LOG");
            }
            if let Some(value) = old_fail_property {
                std::env::set_var("BTRFS_MIGRATION_FAIL_PROPERTY", value);
            } else {
                std::env::remove_var("BTRFS_MIGRATION_FAIL_PROPERTY");
            }
        }
    }

    fn checkout(root: &Path) -> PathBuf {
        let destination = root.join("checkout");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("file"), b"content").unwrap();
        fs::create_dir(destination.join(".git")).unwrap();
        fs::write(destination.join(".git/HEAD"), b"ref: refs/heads/main").unwrap();
        fs::create_dir_all(destination.join(".jj/working_copy")).unwrap();
        fs::write(destination.join(".jj/working_copy/checkout"), b"journal").unwrap();
        destination
    }

    #[test]
    fn conversion_enables_git_fsmonitor_without_configuring_a_hook() {
        with_fake_btrfs(|root, _| {
            let destination = initialized_git_checkout(root);
            let committed =
                convert_subvolume_root(&destination, MigrationOptions::default(), |_| {}).unwrap();

            let output = Command::new("git")
                .arg("-C")
                .arg(&destination)
                .args(["config", "--local", "--get", "core.fsmonitor"])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "true");
            let output = Command::new("git")
                .arg("-C")
                .arg(&destination)
                .args(["config", "--local", "--get", "core.fsmonitorHookVersion"])
                .output()
                .unwrap();
            assert!(!output.status.success());
            committed.discard_displaced().unwrap();
        });
    }

    #[test]
    fn conversion_preserves_existing_git_fsmonitor_and_hook_version() {
        with_fake_btrfs(|root, _| {
            let destination = initialized_git_checkout(root);
            for (key, value) in [
                ("core.fsmonitor", "/custom/fsmonitor-hook"),
                ("core.fsmonitorHookVersion", "1"),
            ] {
                assert!(
                    Command::new("git")
                        .arg("-C")
                        .arg(&destination)
                        .args(["config", "--local", key, value])
                        .status()
                        .unwrap()
                        .success()
                );
            }
            let committed =
                convert_subvolume_root(&destination, MigrationOptions::default(), |_| {}).unwrap();

            for (key, expected) in [
                ("core.fsmonitor", "/custom/fsmonitor-hook"),
                ("core.fsmonitorHookVersion", "1"),
            ] {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(&destination)
                    .args(["config", "--local", "--get", key])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
            }
            committed.discard_displaced().unwrap();
        });
    }

    #[test]
    fn conversion_preserves_explicitly_disabled_git_fsmonitor() {
        with_fake_btrfs(|root, _| {
            let destination = initialized_git_checkout(root);
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&destination)
                    .args(["config", "--local", "core.fsmonitor", "false"])
                    .status()
                    .unwrap()
                    .success()
            );
            let committed =
                convert_subvolume_root(&destination, MigrationOptions::default(), |_| {}).unwrap();

            let output = Command::new("git")
                .arg("-C")
                .arg(&destination)
                .args(["config", "--local", "--get", "core.fsmonitor"])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "false");
            let output = Command::new("git")
                .arg("-C")
                .arg(&destination)
                .args(["config", "--local", "--get", "core.fsmonitorHookVersion"])
                .output()
                .unwrap();
            assert!(!output.status.success());
            committed.discard_displaced().unwrap();
        });
    }

    fn initialized_git_checkout(root: &Path) -> PathBuf {
        let destination = root.join("checkout");
        fs::create_dir(&destination).unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&destination)
                .args(["init", "--quiet"])
                .status()
                .unwrap()
                .success()
        );
        destination
    }

    #[test]
    fn drop_rolls_back_uncommitted_temporary_subvolumes() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            {
                let migration =
                    SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
                assert!(!migration.is_noop());
                assert!(migration.replacements[0].temporary.exists());
            }
            assert!(destination.join("file").is_file());
            assert_eq!(
                fs::read_dir(root)
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| entry
                        .file_name()
                        .to_string_lossy()
                        .contains("awacs-subvolume"))
                    .count(),
                0
            );
        });
    }

    #[test]
    fn commit_publishes_root_and_metadata_subvolumes() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            let migration =
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
            let committed = migration.commit().unwrap();
            assert!(destination.join(".fake-subvolume").is_file());
            assert!(destination.join(".git/.fake-subvolume").is_file());
            assert!(destination.join(".jj/.fake-subvolume").is_file());
            assert_eq!(
                fs::read(destination.join(".jj/working_copy/checkout")).unwrap(),
                b"journal"
            );
            assert!(destination.join("file").is_file());
            assert!(committed.displaced_path().unwrap().exists());
            committed.discard_displaced().unwrap();
        });
    }

    #[test]
    fn existing_subvolume_boundaries_are_noops() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            fs::write(destination.join(".fake-subvolume"), b"").unwrap();
            fs::write(destination.join(".git/.fake-subvolume"), b"").unwrap();
            fs::write(destination.join(".jj/.fake-subvolume"), b"").unwrap();
            let migration =
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
            assert!(migration.is_noop());
            let committed = migration.commit().unwrap();
            assert!(committed.displaced_path().is_none());
        });
    }

    #[test]
    fn nested_subvolume_is_left_for_snapshot_handling() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            fs::write(destination.join(".git/.fake-subvolume"), b"").unwrap();
            let migration =
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
            let plan = migration.pending_snapshots()[0].clone();
            assert_eq!(plan.source, destination.join(".git"));
            assert!(!plan.destination.exists());
            fs::create_dir(&plan.destination).unwrap();
            fs::write(plan.destination.join(".fake-subvolume"), b"").unwrap();
            let committed = migration.commit().unwrap();
            assert!(destination.join(".git/.fake-subvolume").is_file());
            committed.discard_displaced().unwrap();
        });
    }

    #[test]
    fn both_nested_metadata_subvolumes_are_preserved() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            for name in [".git", ".jj"] {
                fs::write(destination.join(name).join(".fake-subvolume"), b"").unwrap();
            }
            let migration =
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
            assert_eq!(migration.pending_snapshots().len(), 2);
            for plan in migration.pending_snapshots() {
                assert!(!plan.destination.exists());
                fs::create_dir(&plan.destination).unwrap();
                copy_children(&plan.source, &plan.destination, false).unwrap();
            }
            let committed = migration.commit().unwrap();
            assert_eq!(
                fs::read(destination.join(".git/HEAD")).unwrap(),
                b"ref: refs/heads/main"
            );
            assert_eq!(
                fs::read(destination.join(".jj/working_copy/checkout")).unwrap(),
                b"journal"
            );
            committed.discard_displaced().unwrap();
        });
    }

    #[test]
    fn missing_nested_jj_snapshot_prevents_publication() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            for name in [".git", ".jj"] {
                fs::write(destination.join(name).join(".fake-subvolume"), b"").unwrap();
            }
            let root_inode = fs::metadata(&destination).unwrap().ino();
            let migration =
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
            let git_plan = &migration.pending_snapshots()[0];
            fs::create_dir(&git_plan.destination).unwrap();
            copy_children(&git_plan.source, &git_plan.destination, false).unwrap();
            let temporary = migration.replacements[0].temporary.clone();
            assert!(migration.commit().is_err());
            assert_eq!(fs::metadata(&destination).unwrap().ino(), root_inode);
            assert_eq!(
                fs::read(destination.join(".jj/working_copy/checkout")).unwrap(),
                b"journal"
            );
            assert!(!temporary.exists());
        });
    }

    #[test]
    fn metadata_symlink_is_rejected_without_changing_its_target() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            let external = root.join("external-jj");
            fs::rename(destination.join(".jj"), &external).unwrap();
            std::os::unix::fs::symlink(&external, destination.join(".jj")).unwrap();
            assert!(
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).is_err()
            );
            assert!(
                fs::symlink_metadata(destination.join(".jj"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                fs::read(external.join("working_copy/checkout")).unwrap(),
                b"journal"
            );
            assert!(!destination.join(".fake-subvolume").exists());
        });
    }

    #[test]
    fn existing_root_and_git_upgrade_only_plain_jj() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            fs::write(destination.join(".fake-subvolume"), b"").unwrap();
            fs::write(destination.join(".git/.fake-subvolume"), b"").unwrap();
            let root_inode = fs::metadata(&destination).unwrap().ino();
            let git_inode = fs::metadata(destination.join(".git")).unwrap().ino();
            let migration =
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
            let committed = migration.commit().unwrap();
            assert_eq!(fs::metadata(&destination).unwrap().ino(), root_inode);
            assert_eq!(
                fs::metadata(destination.join(".git")).unwrap().ino(),
                git_inode
            );
            assert!(destination.join(".jj/.fake-subvolume").is_file());
            assert_eq!(
                fs::read(destination.join(".jj/working_copy/checkout")).unwrap(),
                b"journal"
            );
            committed.discard_displaced().unwrap();
        });
    }

    #[test]
    fn metadata_commit_failure_restores_prior_replacement() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            fs::write(destination.join(".fake-subvolume"), b"").unwrap();
            let git_inode = fs::metadata(destination.join(".git")).unwrap().ino();
            let migration =
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
            let temporary_paths = migration
                .replacements
                .iter()
                .map(|replacement| replacement.temporary.clone())
                .collect::<Vec<_>>();
            fs::rename(destination.join(".jj"), root.join("moved-jj")).unwrap();
            assert!(migration.commit().is_err());
            assert_eq!(
                fs::metadata(destination.join(".git")).unwrap().ino(),
                git_inode
            );
            assert!(!destination.join(".git/.fake-subvolume").exists());
            assert_eq!(
                fs::read(root.join("moved-jj/working_copy/checkout")).unwrap(),
                b"journal"
            );
            assert!(temporary_paths.iter().all(|path| !path.exists()));
        });
    }

    #[test]
    fn failed_commit_leaves_destination_and_drop_cleans_temporary() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            let migration =
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
            let temporary = migration.replacements[0].temporary.clone();
            fs::rename(&destination, root.join("moved-away")).unwrap();
            assert!(migration.commit().is_err());
            assert!(!temporary.exists());
            assert!(root.join("moved-away/file").is_file());
        });
    }

    #[test]
    fn failed_prepare_cleans_temporary_subvolume() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            // SAFETY: with_fake_btrfs holds the module-wide environment lock.
            unsafe { std::env::set_var("BTRFS_MIGRATION_FAIL_PROPERTY", "1") };
            assert!(
                SubvolumeMigration::prepare(
                    &destination,
                    MigrationOptions {
                        compression: Some(true),
                        keep_temporary_on_drop: false,
                    },
                )
                .is_err()
            );
            // SAFETY: with_fake_btrfs holds the module-wide environment lock.
            unsafe { std::env::remove_var("BTRFS_MIGRATION_FAIL_PROPERTY") };
            assert_eq!(
                fs::read_dir(root)
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| entry
                        .file_name()
                        .to_string_lossy()
                        .contains("awacs-subvolume"))
                    .count(),
                0
            );
        });
    }

    #[test]
    fn compression_is_applied_to_new_root_and_metadata_subvolumes() {
        with_fake_btrfs(|root, log| {
            let destination = checkout(root);
            let migration = SubvolumeMigration::prepare(
                &destination,
                MigrationOptions {
                    compression: Some(true),
                    keep_temporary_on_drop: false,
                },
            )
            .unwrap();
            let committed = migration.commit().unwrap();
            committed.discard_displaced().unwrap();
            let log = fs::read_to_string(log).unwrap();
            assert_eq!(log.matches("compression zstd").count(), 3);
        });
    }

    #[test]
    fn compression_rewrites_existing_metadata_instead_of_scheduling_snapshots() {
        with_fake_btrfs(|root, log| {
            let destination = checkout(root);
            for name in [".git", ".jj"] {
                fs::write(destination.join(name).join(".fake-subvolume"), b"").unwrap();
            }
            let migration = SubvolumeMigration::prepare(
                &destination,
                MigrationOptions {
                    compression: Some(true),
                    keep_temporary_on_drop: false,
                },
            )
            .unwrap();
            assert!(migration.pending_snapshots().is_empty());
            let committed = migration.commit().unwrap();
            assert_eq!(
                fs::read(destination.join(".jj/working_copy/checkout")).unwrap(),
                b"journal"
            );
            committed.discard_displaced().unwrap();
            assert_eq!(
                fs::read_to_string(log)
                    .unwrap()
                    .matches("compression zstd")
                    .count(),
                3
            );
        });
    }

    fn file_extent_flags(path: &Path) -> Vec<u32> {
        #[derive(Clone, Copy, Default)]
        #[repr(C)]
        struct Extent {
            logical: u64,
            physical: u64,
            length: u64,
            reserved64: [u64; 2],
            flags: u32,
            reserved: [u32; 3],
        }
        #[derive(Default)]
        #[repr(C)]
        struct Fiemap {
            start: u64,
            length: u64,
            flags: u32,
            mapped_extents: u32,
            extent_count: u32,
            reserved: u32,
            extents: [Extent; 32],
        }
        const _: [(); 56] = [(); size_of::<Extent>()];
        const _: [(); 32] = [(); std::mem::offset_of!(Fiemap, extents)];
        // Linux UAPI: _IOWR('f', 11, struct fiemap), excluding its trailing
        // extent array. Request synchronization before inspecting physical data.
        const FS_IOC_FIEMAP: libc::c_ulong = 0xc020_660b;
        let file = fs::File::open(path).unwrap();
        let mut request = Fiemap {
            length: u64::MAX,
            flags: 1,
            extent_count: 32,
            ..Fiemap::default()
        };
        // SAFETY: request has the UAPI layout and room for all requested extents.
        let result = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP, &mut request) };
        assert_eq!(result, 0, "FIEMAP: {}", io::Error::last_os_error());
        let count = request.mapped_extents as usize;
        assert!((1..=request.extents.len()).contains(&count));
        let extents = &request.extents[..count];
        assert_ne!(
            extents[count - 1].flags & 1,
            0,
            "fixture must fit in one FIEMAP query"
        );
        extents.iter().map(|extent| extent.flags).collect()
    }

    #[test]
    #[ignore = "requires JJ_TEST_BTRFS_ROOT and Btrfs snapshot creation/deletion permissions"]
    fn copy_and_conversion_preserve_recursive_subvolume_boundaries() {
        let _lock = env_lock();
        let btrfs_root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
            .map(PathBuf::from)
            .expect("JJ_TEST_BTRFS_ROOT must name a writable directory on real Btrfs");
        struct Fixture(tempfile::TempDir);
        impl Drop for Fixture {
            fn drop(&mut self) {
                if let Err(error) = crate::btrfs::remove_tree(self.0.path()) {
                    eprintln!(
                        "Failed to remove Btrfs copy fixture {}: {error}",
                        self.0.path().display()
                    );
                }
            }
        }
        let fixture = Fixture(tempfile::tempdir_in(btrfs_root).unwrap());
        let parent = Path::new(".git/ordinary/parent");
        let grandchild = parent.join("ordinary/grandchild");
        let jj_child = Path::new(".jj/ordinary/child");
        let payload = b"rewrite nested snapshot extents\n".repeat(32768);
        for (case, (subvolume_root, compression)) in [false, true]
            .into_iter()
            .flat_map(|root| [None, Some(true), Some(false)].map(|compression| (root, compression)))
            .enumerate()
        {
            let case_root = fixture.0.path().join(case.to_string());
            fs::create_dir(&case_root).unwrap();
            let source = case_root.join("source");
            fs::create_dir(&source).unwrap();
            fs::create_dir_all(source.join(parent).parent().unwrap()).unwrap();
            create_btrfs_subvolume(&source.join(parent)).unwrap();
            fs::create_dir(source.join(parent).join("ordinary")).unwrap();
            create_btrfs_subvolume(&source.join(&grandchild)).unwrap();
            fs::create_dir_all(source.join(jj_child).parent().unwrap()).unwrap();
            create_btrfs_subvolume(&source.join(jj_child)).unwrap();
            let mut original = Vec::new();
            for (relative, readonly) in [
                (grandchild.as_path(), true),
                (parent, true),
                (jj_child, false),
            ] {
                let path = source.join(relative);
                set_btrfs_compression(&path, false).unwrap();
                fs::write(path.join("payload"), &payload).unwrap();
                fs::File::open(path.join("payload"))
                    .unwrap()
                    .sync_all()
                    .unwrap();
                let opened = crate::btrfs::OpenedSubvolume::open(&path).unwrap();
                crate::btrfs::set_subvolume_readonly(opened.as_fd(), readonly).unwrap();
                original.push((relative.to_owned(), opened.subvolume.uuid, readonly));
            }

            let destination = case_root.join("destination");
            if subvolume_root {
                create_btrfs_subvolume(&destination).unwrap();
                if let Some(compression) = compression {
                    set_btrfs_compression(&destination, compression).unwrap();
                }
            } else {
                fs::create_dir(&destination).unwrap();
            }
            let allow_reflinks = compression.is_none();
            copy_children(&source, &destination, allow_reflinks).unwrap();
            let mut copied = Vec::new();
            for (relative, uuid, readonly) in &original {
                let path = destination.join(relative);
                let snapshot = crate::btrfs::OpenedSubvolume::open(&path).unwrap();
                assert_ne!(snapshot.subvolume.uuid, *uuid);
                assert_eq!(snapshot.subvolume.readonly(), *readonly);
                assert_eq!(fs::read(path.join("payload")).unwrap(), payload);
                assert_eq!(
                    snapshot.subvolume.parent_uuid,
                    allow_reflinks.then_some(*uuid)
                );
                copied.push((relative.clone(), snapshot.subvolume.uuid, *readonly));
            }
            // The generic copier must reject an occupied subvolume destination
            // without removing it or replacing its snapshots.
            assert!(
                copy_entry(
                    &source.join(parent),
                    &destination.join(parent),
                    allow_reflinks
                )
                .is_err()
            );
            assert_eq!(
                crate::btrfs::OpenedSubvolume::open(&destination.join(parent))
                    .unwrap()
                    .subvolume
                    .uuid,
                copied
                    .iter()
                    .find(|(relative, _, _)| relative == parent)
                    .unwrap()
                    .1
            );

            // Both metadata roots are still ordinary directories around the
            // copied subvolumes. Converting them used to flatten those children.
            assert_ne!(
                fs::metadata(destination.join(".git")).unwrap().ino(),
                crate::btrfs::ROOT_INODE
            );
            let migration = SubvolumeMigration::prepare(
                &destination,
                MigrationOptions {
                    compression,
                    keep_temporary_on_drop: false,
                },
            )
            .unwrap();
            assert!(migration.pending_snapshots().is_empty());
            let committed = migration.commit().unwrap();
            for metadata in [".git", ".jj"] {
                crate::btrfs::OpenedSubvolume::open(&destination.join(metadata)).unwrap();
            }
            for (relative, uuid, readonly) in copied {
                let path = destination.join(&relative);
                let snapshot = crate::btrfs::OpenedSubvolume::open(&path).unwrap();
                assert_ne!(snapshot.subvolume.uuid, uuid);
                assert_eq!(
                    snapshot.subvolume.parent_uuid,
                    allow_reflinks.then_some(uuid)
                );
                assert_eq!(snapshot.subvolume.readonly(), readonly);
                let copied_payload = path.join("payload");
                assert_eq!(fs::read(&copied_payload).unwrap(), payload);
                fs::File::open(&copied_payload).unwrap().sync_all().unwrap();
                if let Some(compression) = compression {
                    let output = btrfs_command()
                        .args(["property", "get"])
                        .arg(&path)
                        .arg("compression")
                        .output()
                        .unwrap();
                    assert!(
                        output.status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert_eq!(
                        String::from_utf8(output.stdout).unwrap().trim(),
                        if compression {
                            "compression=zstd"
                        } else {
                            "compression=none"
                        }
                    );
                    for path in [&path, &copied_payload] {
                        let flags = inode_flags(path).unwrap();
                        assert_eq!(
                            flags & FS_NOCOMP_FL != 0,
                            !compression,
                            "NOCOMPRESS policy for {}",
                            path.display()
                        );
                        assert_eq!(
                            flags & 4 != 0,
                            compression,
                            "COMPRESS policy for {}",
                            path.display()
                        );
                    }
                    // Btrfs reports compressed data with ENCODED and shared
                    // extents with SHARED. Check the actual written extents,
                    // including the negative policy on compress=zstd mounts.
                    for flags in file_extent_flags(&copied_payload) {
                        assert_eq!(
                            flags & 8 != 0,
                            compression,
                            "physical compression for {}",
                            copied_payload.display()
                        );
                        assert_eq!(
                            flags & 0x2000,
                            0,
                            "rewritten extents must not share source data"
                        );
                    }
                }
            }
            for (relative, uuid, readonly) in original {
                let original =
                    crate::btrfs::OpenedSubvolume::open(&source.join(&relative)).unwrap();
                assert_eq!(original.subvolume.uuid, uuid);
                assert_eq!(original.subvolume.readonly(), readonly);
                assert_eq!(
                    fs::read(source.join(relative).join("payload")).unwrap(),
                    payload
                );
            }
            committed.discard_displaced().unwrap();
        }

        // An existing metadata subvolume in a plain checkout takes the
        // pending-snapshot path instead of the generic directory copier.
        // Its root remains a descendant of the new checkout and retains ro.
        let destination = fixture.0.path().join("readonly-metadata");
        fs::create_dir(&destination).unwrap();
        let metadata = destination.join(".git");
        create_btrfs_subvolume(&metadata).unwrap();
        // Keep Git's discovery inside this fixture, and make fsmonitor
        // configuration a read-only probe once the metadata is protected.
        fs::create_dir(metadata.join("objects")).unwrap();
        fs::create_dir(metadata.join("refs")).unwrap();
        fs::write(metadata.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        fs::write(
            metadata.join("config"),
            b"[core]\nrepositoryformatversion = 0\nbare = false\nfsmonitor = true\n",
        )
        .unwrap();
        fs::write(metadata.join("payload"), &payload).unwrap();
        let original = crate::btrfs::OpenedSubvolume::open(&metadata).unwrap();
        crate::btrfs::set_subvolume_readonly(original.as_fd(), true).unwrap();
        let committed =
            convert_subvolume_root(&destination, MigrationOptions::default(), |_| {}).unwrap();
        let copied = crate::btrfs::OpenedSubvolume::open(&metadata).unwrap();
        assert_ne!(copied.subvolume.uuid, original.subvolume.uuid);
        assert_eq!(copied.subvolume.parent_uuid, Some(original.subvolume.uuid));
        assert!(copied.subvolume.readonly());
        assert_eq!(fs::read(metadata.join("payload")).unwrap(), payload);
        assert!(
            !crate::btrfs::OpenedSubvolume::open(&destination)
                .unwrap()
                .subvolume
                .readonly()
        );
        let retained =
            crate::btrfs::OpenedSubvolume::open(&committed.displaced_path().unwrap().join(".git"))
                .unwrap();
        assert_eq!(retained.subvolume.uuid, original.subvolume.uuid);
        assert!(retained.subvolume.readonly());
        committed.discard_displaced().unwrap();
    }
}
