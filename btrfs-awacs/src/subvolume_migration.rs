use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub const GIT_FSMONITOR_PATH: &str = "/usr/local/bin/git-fsmonitor-awacs";

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
pub struct SubvolumeMigration {
    destination: PathBuf,
    temporary: Option<PathBuf>,
    pending_snapshot: Option<SnapshotPlan>,
    keep_temporary_on_drop: bool,
    committed: bool,
}

#[derive(Debug)]
pub struct CommittedMigration {
    displaced: Option<PathBuf>,
}

impl CommittedMigration {
    pub fn displaced_path(&self) -> Option<&Path> {
        self.displaced.as_deref()
    }

    pub fn discard_displaced(mut self) -> Result<(), MigrationError> {
        if let Some(path) = self.displaced.take() {
            remove_existing_destination(&path)?;
        }
        Ok(())
    }
}

impl SubvolumeMigration {
    /// Prepares an existing destination checkout for conversion in place.
    ///
    /// A non-subvolume root is copied into a temporary sibling subvolume. If
    /// the root is already a subvolume, only a plain nested .git directory is
    /// migrated. Existing subvolume boundaries are never copied: callers can
    /// satisfy pending_snapshot() before commit() if a nested .git snapshot
    /// must be installed in the temporary root.
    pub fn prepare(
        destination: impl AsRef<Path>,
        options: MigrationOptions,
    ) -> Result<Self, MigrationError> {
        let destination = destination.as_ref().to_owned();
        if !destination.is_dir() {
            return Err(MigrationError::new(format!(
                "subvolume migration destination is not a directory: {}",
                destination.display()
            )));
        }
        let git = destination.join(".git");
        let git_is_dir = git.is_dir();
        let root_is_subvolume = is_btrfs_subvolume(&destination)?;
        let git_is_subvolume = git_is_dir && is_btrfs_subvolume(&git)?;

        if !root_is_subvolume {
            let temporary = unique_sibling(&destination, "tmp")?;
            let mut guard =
                TemporaryPathGuard::new(temporary.clone(), options.keep_temporary_on_drop);
            create_btrfs_subvolume(&temporary)?;
            if let Some(compression) = options.compression {
                set_btrfs_compression(&temporary, compression)?;
            }
            if git_is_dir {
                copy_children_except(
                    &destination,
                    &temporary,
                    OsStr::new(".git"),
                    options.compression.is_none(),
                )?;
            } else {
                copy_children(&destination, &temporary, options.compression.is_none())?;
            }

            let pending_snapshot = if git_is_subvolume {
                Some(SnapshotPlan {
                    source: git,
                    destination: temporary.join(".git"),
                })
            } else if git_is_dir {
                let temporary_git = temporary.join(".git");
                create_btrfs_subvolume(&temporary_git)?;
                if let Some(compression) = options.compression {
                    set_btrfs_compression(&temporary_git, compression)?;
                }
                copy_children(&git, &temporary_git, options.compression.is_none())?;
                None
            } else {
                None
            };
            guard.disarm();
            return Ok(Self {
                destination,
                temporary: Some(temporary),
                pending_snapshot,
                keep_temporary_on_drop: options.keep_temporary_on_drop,
                committed: false,
            });
        }

        if git_is_dir && !git_is_subvolume {
            let temporary = unique_sibling(&git, "tmp")?;
            let mut guard =
                TemporaryPathGuard::new(temporary.clone(), options.keep_temporary_on_drop);
            create_btrfs_subvolume(&temporary)?;
            if let Some(compression) = options.compression {
                set_btrfs_compression(&temporary, compression)?;
            }
            copy_children(&git, &temporary, options.compression.is_none())?;
            guard.disarm();
            return Ok(Self {
                destination: git,
                temporary: Some(temporary),
                pending_snapshot: None,
                keep_temporary_on_drop: options.keep_temporary_on_drop,
                committed: false,
            });
        }

        Ok(Self {
            destination,
            temporary: None,
            pending_snapshot: None,
            keep_temporary_on_drop: options.keep_temporary_on_drop,
            committed: false,
        })
    }

    pub fn is_noop(&self) -> bool {
        self.temporary.is_none()
    }

    pub fn pending_snapshot(&self) -> Option<&SnapshotPlan> {
        self.pending_snapshot.as_ref()
    }

    /// Publishes the prepared temporary subvolume at the destination.
    ///
    /// The original destination is first renamed to a fresh sibling. If
    /// publication fails, it is renamed back before this method returns.
    pub fn commit(mut self) -> Result<CommittedMigration, MigrationError> {
        let Some(temporary) = self.temporary.take() else {
            self.committed = true;
            return Ok(CommittedMigration { displaced: None });
        };
        if let Some(plan) = &self.pending_snapshot
            && !plan.destination.exists()
        {
            self.temporary = Some(temporary);
            return Err(MigrationError::new(format!(
                "required snapshot destination was not populated: {}",
                plan.destination.display()
            )));
        }
        let displaced = match unique_sibling(&self.destination, "source") {
            Ok(displaced) => displaced,
            Err(error) => {
                self.temporary = Some(temporary);
                return Err(error);
            }
        };
        if let Err(error) = fs::rename(&self.destination, &displaced) {
            self.temporary = Some(temporary);
            return Err(MigrationError::io(
                format!("move {} aside", self.destination.display()),
                error,
            ));
        }
        if let Err(error) = fs::rename(&temporary, &self.destination) {
            let restore_error = fs::rename(&displaced, &self.destination).err();
            self.temporary = Some(temporary);
            return Err(match restore_error {
                Some(restore_error) => MigrationError::new(format!(
                    "publish temporary subvolume: {error}; restore destination: {restore_error}"
                )),
                None => MigrationError::io("publish temporary subvolume", error),
            });
        }
        self.committed = true;
        Ok(CommittedMigration {
            displaced: Some(displaced),
        })
    }
}

/// Converts an existing directory to the AWACS-compatible root/.git
/// subvolume layout and reports coarse interactive phases to its caller.
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
    if let Some(plan) = migration.pending_snapshot() {
        progress("snapshotting nested .git subvolume");
        create_btrfs_snapshot(&plan.source, &plan.destination)?;
    }
    progress("publishing Btrfs subvolume conversion");
    let committed = migration.commit()?;
    progress("configuring Git fsmonitor");
    configure_git_fsmonitor(&destination)?;
    Ok(committed)
}

/// Configures a real Git worktree to use AWACS' installed hook. Directories
/// with no usable Git metadata are valid AWACS roots and are left alone.
fn configure_git_fsmonitor(destination: &Path) -> Result<(), MigrationError> {
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
    let status = Command::new("git")
        .arg("-C")
        .arg(destination)
        .args(["config", "--local", "core.fsmonitor", GIT_FSMONITOR_PATH])
        .status()
        .map_err(|error| MigrationError::io("configure Git fsmonitor", error))?;
    if !status.success() {
        return Err(MigrationError::new(format!(
            "configure Git fsmonitor exited with {status}"
        )));
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(destination)
        .args(["config", "--local", "core.fsmonitorHookVersion", "2"])
        .status()
        .map_err(|error| MigrationError::io("configure Git fsmonitor hook version", error))?;
    if !status.success() {
        return Err(MigrationError::new(format!(
            "configure Git fsmonitor hook version exited with {status}"
        )));
    }
    Ok(())
}

impl Drop for SubvolumeMigration {
    fn drop(&mut self) {
        if self.committed || self.keep_temporary_on_drop {
            return;
        }
        if let Some(temporary) = self.temporary.take() {
            let _ = remove_existing_destination(&temporary);
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
    for entry in fs::read_dir(source).map_err(|error| MigrationError::io("read source", error))? {
        let entry = entry.map_err(|error| MigrationError::io("read source entry", error))?;
        if entry.file_name() == excluded_name {
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
        if !allow_reflinks || !try_reflink_file(source, destination)? {
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
    use std::os::fd::AsRawFd as _;
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

fn create_btrfs_snapshot(source: &Path, destination: &Path) -> Result<(), MigrationError> {
    let output = btrfs_command()
        .args(["subvolume", "snapshot"])
        .arg(source)
        .arg(destination)
        .output()
        .map_err(|error| MigrationError::io("create Btrfs snapshot", error))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(MigrationError::new(format!(
            "snapshot Btrfs subvolume {} to {}: {}",
            source.display(),
            destination.display(),
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
    if !path.exists() {
        return Ok(());
    }
    if fs::symlink_metadata(path)
        .map_err(|error| MigrationError::io("inspect destination", error))?
        .file_type()
        .is_dir()
    {
        if is_btrfs_subvolume(path)? {
            let output = btrfs_command()
                .args(["subvolume", "delete"])
                .arg(path)
                .output()
                .map_err(|error| MigrationError::io("delete Btrfs subvolume", error))?;
            if !output.status.success() {
                return Err(MigrationError::new(format!(
                    "delete Btrfs subvolume at {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
        } else {
            fs::remove_dir_all(path)
                .map_err(|error| MigrationError::io("remove directory", error))?;
        }
    } else {
        fs::remove_file(path).map_err(|error| MigrationError::io("remove file", error))?;
    }
    Ok(())
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
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

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
        destination
    }

    #[test]
    fn configures_real_git_worktrees_for_the_installed_awacs_hook() {
        let temp = tempdir().unwrap();
        let destination = temp.path().join("checkout");
        fs::create_dir(&destination).unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&destination)
                .arg("init")
                .status()
                .unwrap()
                .success()
        );
        configure_git_fsmonitor(&destination).unwrap();
        let output = Command::new("git")
            .arg("-C")
            .arg(&destination)
            .args(["config", "--local", "--get", "core.fsmonitor"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            GIT_FSMONITOR_PATH
        );
        let output = Command::new("git")
            .arg("-C")
            .arg(&destination)
            .args(["config", "--local", "--get", "core.fsmonitorHookVersion"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "2");
    }

    #[test]
    fn drop_rolls_back_uncommitted_temporary_subvolumes() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            {
                let migration =
                    SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
                assert!(!migration.is_noop());
                assert!(migration.temporary.as_ref().unwrap().exists());
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
    fn commit_publishes_root_and_git_subvolumes() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            let migration =
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
            let committed = migration.commit().unwrap();
            assert!(destination.join(".fake-subvolume").is_file());
            assert!(destination.join(".git/.fake-subvolume").is_file());
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
            let plan = migration.pending_snapshot().unwrap().clone();
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
    fn failed_commit_leaves_destination_and_drop_cleans_temporary() {
        with_fake_btrfs(|root, _| {
            let destination = checkout(root);
            let migration =
                SubvolumeMigration::prepare(&destination, MigrationOptions::default()).unwrap();
            let temporary = migration.temporary.as_ref().unwrap().clone();
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
    fn compression_is_applied_to_new_root_and_git_subvolumes() {
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
            assert_eq!(log.matches("compression zstd").count(), 2);
        });
    }
}
