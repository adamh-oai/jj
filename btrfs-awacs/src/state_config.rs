//! The repository owns the AWACS state location after initialization.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use crate::bootstrap::BootstrapError;
use crate::bootstrap::git_admin_directory;

const STATE_DIR_KEY: &str = "awacs.stateDir";

/// Reads only the path saved in this repository. Environment settings cannot
/// redirect an initialized repository to a different database.
pub fn state_root_for_root(root: &Path) -> Result<PathBuf, BootstrapError> {
    let config = repository_config(root)?;
    read_state_root(&config)?.ok_or_else(|| missing_state_root(&config))
}

/// Saves the initial path in the shared Git config. Linked worktrees use that
/// same setting and keep separate state directories keyed by subvolume UUID.
pub fn configure_state_root(root: &Path) -> Result<PathBuf, BootstrapError> {
    configure_state_root_with(root, env::var_os("BTRFS_AWACS_STATE_DIR"), false)
}

/// Checks a new or saved path before a staged conversion. A rejected initial
/// choice must not become the repository's saved configuration.
pub fn configure_state_root_for_conversion(root: &Path) -> Result<PathBuf, BootstrapError> {
    configure_state_root_with(root, env::var_os("BTRFS_AWACS_STATE_DIR"), true)
}

/// A staged conversion moves the checkout. Its saved absolute storage path
/// must therefore refer to a directory outside that checkout. Existing roots
/// may still use internal storage until a conversion is requested.
pub fn require_state_root_outside_checkout(
    root: &Path,
    state_root: &Path,
) -> Result<(), BootstrapError> {
    let root = fs::canonicalize(root).map_err(|error| {
        BootstrapError::new(format!("resolve checkout {}: {error}", root.display()))
    })?;
    let state_root = resolve_future_directory(state_root)?;
    if state_root.starts_with(&root) {
        return Err(BootstrapError::new(format!(
            "AWACS state directory {} is inside checkout {}; converting this checkout would move \
             its saved absolute state path. Configure an AWACS state directory outside the \
             checkout before conversion; preserve any existing AWACS state",
            state_root.display(),
            root.display()
        )));
    }
    Ok(())
}

fn resolve_future_directory(path: &Path) -> Result<PathBuf, BootstrapError> {
    use std::path::Component;

    if !path.is_absolute() {
        return Err(BootstrapError::new(
            "AWACS state directory must be absolute",
        ));
    }
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            Component::Normal(name) => {
                resolved.push(name);
                match fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        resolved = fs::canonicalize(&resolved).map_err(|error| {
                            BootstrapError::new(format!(
                                "resolve AWACS state directory {}: {error}",
                                resolved.display()
                            ))
                        })?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(BootstrapError::new(format!(
                            "inspect AWACS state directory {}: {error}",
                            resolved.display()
                        )));
                    }
                }
            }
            Component::Prefix(_) => unreachable!("Unix paths have no prefix"),
        }
    }
    Ok(resolved)
}

fn configure_state_root_with(
    root: &Path,
    initial: Option<OsString>,
    conversion: bool,
) -> Result<PathBuf, BootstrapError> {
    let config = repository_config(root)?;
    if let Some(path) = read_state_root(&config)? {
        if conversion {
            require_state_root_outside_checkout(root, &path)?;
        }
        return Ok(path);
    }
    let initial = initial.ok_or_else(|| {
        BootstrapError::new(format!(
            "{} has no {STATE_DIR_KEY}; BTRFS_AWACS_STATE_DIR is unset. Set it to the absolute \
             AWACS state directory before initializing this repository. For an existing AWACS \
             repository, use its existing state directory; no path will be guessed",
            config.display()
        ))
    })?;
    let path = validate_path(PathBuf::from(initial), "BTRFS_AWACS_STATE_DIR")?;
    if conversion {
        require_state_root_outside_checkout(root, &path)?;
    }

    // Use Git's config lock so simultaneous initialization cannot save two
    // different directories. Read again after locking before choosing a path.
    let lock_path = config.with_extension("lock");
    let mut lock_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|error| {
            BootstrapError::new(format!(
                "lock Git configuration {}: {error}",
                config.display()
            ))
        })?;
    let mut lock = ConfigLock(Some(lock_path.clone()));
    if let Some(path) = read_state_root(&config)? {
        if conversion {
            require_state_root_outside_checkout(root, &path)?;
        }
        return Ok(path);
    }
    match fs::read(&config) {
        Ok(contents) => {
            lock_file.write_all(&contents).map_err(config_write_error)?;
            let permissions = fs::metadata(&config)
                .map_err(config_write_error)?
                .permissions();
            lock_file
                .set_permissions(permissions)
                .map_err(config_write_error)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(config_write_error(error)),
    }
    lock_file.sync_all().map_err(config_write_error)?;
    drop(lock_file);
    let output = config_command(&lock_path)
        .args(["--replace-all", STATE_DIR_KEY])
        .arg(&path)
        .output()
        .map_err(|error| BootstrapError::new(format!("write {STATE_DIR_KEY}: {error}")))?;
    if !output.status.success() {
        return Err(BootstrapError::new(format!(
            "write {STATE_DIR_KEY} in {}: {}",
            config.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    fs::File::open(&lock_path)
        .and_then(|file| file.sync_all())
        .map_err(config_write_error)?;
    fs::rename(&lock_path, &config).map_err(config_write_error)?;
    lock.0 = None;
    fs::File::open(config.parent().expect("Git config parent"))
        .and_then(|directory| directory.sync_all())
        .map_err(config_write_error)?;
    Ok(path)
}

struct ConfigLock(Option<PathBuf>);

impl Drop for ConfigLock {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn config_write_error(error: std::io::Error) -> BootstrapError {
    BootstrapError::new(format!("save AWACS Git configuration: {error}"))
}

fn missing_state_root(config: &Path) -> BootstrapError {
    BootstrapError::new(format!(
        "{} has no {STATE_DIR_KEY}; for an existing AWACS repository, save its actual absolute \
         state directory with git config --local {STATE_DIR_KEY} /absolute/existing/state. For a \
         new repository, set BTRFS_AWACS_STATE_DIR before initialization; no path will be guessed",
        config.display()
    ))
}

fn validate_path(path: PathBuf, source: &str) -> Result<PathBuf, BootstrapError> {
    if !path.is_absolute() {
        return Err(BootstrapError::new(format!(
            "{source} must contain a nonempty absolute AWACS state directory path"
        )));
    }
    if let Ok(metadata) = fs::metadata(&path)
        && !metadata.is_dir()
    {
        return Err(BootstrapError::new(format!(
            "{source} points to {}, which is not a directory",
            path.display()
        )));
    }
    Ok(path)
}

fn config_command(config: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .args(["config", "--file"])
        .arg(config)
        .arg("--no-includes")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS");
    command
}

fn read_state_root(config: &Path) -> Result<Option<PathBuf>, BootstrapError> {
    let output = config_command(config)
        .args(["--null", "--get-all", STATE_DIR_KEY])
        .output()
        .map_err(|error| BootstrapError::new(format!("read {STATE_DIR_KEY}: {error}")))?;
    if output.status.code() == Some(1) && output.stdout.is_empty() {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(BootstrapError::new(format!(
            "read {STATE_DIR_KEY} in {}: {}",
            config.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let values = output.stdout.strip_suffix(&[0]).ok_or_else(|| {
        BootstrapError::new(format!("invalid Git config response for {STATE_DIR_KEY}"))
    })?;
    if values.contains(&0) {
        return Err(BootstrapError::new(format!(
            "{} contains multiple {STATE_DIR_KEY} values; keep exactly one absolute state \
             directory",
            config.display()
        )));
    }
    validate_path(
        PathBuf::from(OsString::from_vec(values.to_vec())),
        &format!("{STATE_DIR_KEY} in {}", config.display()),
    )
    .map(Some)
}

fn repository_config(root: &Path) -> Result<PathBuf, BootstrapError> {
    let git_dir = match git_admin_directory(root)? {
        Some(git_dir) => git_dir,
        None => jj_git_directory(root)?.ok_or_else(|| {
            BootstrapError::new(format!(
                "{} has no Git repository configuration; initialize a Git-backed repository \
                 before configuring AWACS",
                root.display()
            ))
        })?,
    };
    let common = git_dir.join("commondir");
    let common_dir = match fs::read(&common) {
        Ok(contents) => {
            let bytes = contents.strip_suffix(b"\n").unwrap_or(&contents);
            let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
            if bytes.is_empty() {
                return Err(BootstrapError::new(format!(
                    "{} is empty",
                    common.display()
                )));
            }
            git_dir.join(OsString::from_vec(bytes.to_vec()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir,
        Err(error) => {
            return Err(BootstrapError::new(format!(
                "read Git common directory {}: {error}",
                common.display()
            )));
        }
    };
    let common_dir = fs::canonicalize(&common_dir).map_err(|error| {
        BootstrapError::new(format!(
            "resolve Git common directory {}: {error}",
            common_dir.display()
        ))
    })?;
    Ok(common_dir.join("config"))
}

fn jj_git_directory(root: &Path) -> Result<Option<PathBuf>, BootstrapError> {
    let jj_dir = root.join(".jj");
    let repo = jj_dir.join("repo");
    let repo = match fs::metadata(&repo) {
        Ok(metadata) if metadata.is_dir() => repo,
        Ok(metadata) if metadata.is_file() => jj_dir.join(read_path(&repo)?),
        Ok(_) => {
            return Err(BootstrapError::new(format!(
                "{} is neither a JJ repository directory nor a repository pointer",
                repo.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BootstrapError::new(format!(
                "inspect JJ repository {}: {error}",
                repo.display()
            )));
        }
    };
    let store = repo.join("store");
    let target = store.join(read_path(&store.join("git_target"))?);
    if target.is_file() {
        let contents = fs::read(&target).map_err(config_write_error)?;
        let path = crate::bootstrap::parse_gitfile_directory(&target, &contents)?;
        Ok(Some(target.parent().expect("gitfile parent").join(path)))
    } else {
        Ok(Some(target))
    }
}

fn read_path(path: &Path) -> Result<PathBuf, BootstrapError> {
    let bytes = fs::read(path).map_err(|error| {
        BootstrapError::new(format!(
            "read repository pointer {}: {error}",
            path.display()
        ))
    })?;
    if bytes.is_empty() {
        return Err(BootstrapError::new(format!("{} is empty", path.display())));
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configure(root: &Path, initial: Option<OsString>) -> Result<PathBuf, BootstrapError> {
        configure_state_root_with(root, initial, false)
    }

    fn repository() -> tempfile::TempDir {
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir(fixture.path().join(".git")).unwrap();
        fs::write(
            fixture.path().join(".git/config"),
            b"[core]\n\trepositoryformatversion = 0\n\tfsmonitor = true\n",
        )
        .unwrap();
        fixture
    }

    #[test]
    fn initialization_saves_path_and_runtime_does_not_need_environment() {
        let fixture = repository();
        let state = fixture.path().join("external space\nquoted\"directory");
        assert_eq!(
            configure(fixture.path(), Some(state.clone().into_os_string())).unwrap(),
            state
        );
        assert_eq!(state_root_for_root(fixture.path()).unwrap(), state);
        assert_eq!(configure(fixture.path(), None).unwrap(), state);
        assert!(
            !state.exists(),
            "saving the setting does not initialize state"
        );
        let config = fs::read_to_string(fixture.path().join(".git/config")).unwrap();
        assert!(config.contains("fsmonitor = true"));
        assert!(!fixture.path().join(".git/config.lock").exists());
    }

    #[test]
    fn saved_path_wins_over_environment_and_is_not_rewritten() {
        let fixture = repository();
        let state = fixture.path().join("existing");
        configure(fixture.path(), Some(state.clone().into_os_string())).unwrap();
        let before = fs::read(fixture.path().join(".git/config")).unwrap();
        assert_eq!(
            configure(fixture.path(), Some(OsString::from("relative-invalid"))).unwrap(),
            state
        );
        assert_eq!(
            fs::read(fixture.path().join(".git/config")).unwrap(),
            before
        );
    }

    #[test]
    fn missing_configuration_never_selects_a_default_state_path() {
        let fixture = repository();
        let before = fs::read(fixture.path().join(".git/config")).unwrap();
        let error = configure(fixture.path(), None).unwrap_err().to_string();
        assert!(error.contains("BTRFS_AWACS_STATE_DIR is unset"), "{error}");
        assert!(error.contains("no path will be guessed"), "{error}");
        let error = state_root_for_root(fixture.path()).unwrap_err().to_string();
        assert!(error.contains("has no awacs.stateDir"), "{error}");
        assert!(
            error.contains("git config --local awacs.stateDir"),
            "{error}"
        );
        assert_eq!(
            fs::read(fixture.path().join(".git/config")).unwrap(),
            before
        );
        assert!(!fixture.path().join(".git/awacs").exists());
    }

    #[test]
    fn initialization_rejects_missing_absolute_path_without_writing_config() {
        let fixture = repository();
        let before = fs::read(fixture.path().join(".git/config")).unwrap();
        for value in ["", "relative/path"] {
            let error = configure(fixture.path(), Some(OsString::from(value)))
                .unwrap_err()
                .to_string();
            assert!(error.contains("nonempty absolute"), "{error}");
        }
        assert_eq!(
            fs::read(fixture.path().join(".git/config")).unwrap(),
            before
        );
    }

    #[test]
    fn invalid_saved_path_and_duplicate_settings_do_not_fall_back_to_environment() {
        let fixture = repository();
        for contents in [
            "[awacs]\nstateDir = relative/path\n",
            "[awacs]\nstateDir =\n",
            "[awacs]\nstateDir = /one\nstateDir = /two\n",
        ] {
            fs::write(fixture.path().join(".git/config"), contents).unwrap();
            let error = configure(
                fixture.path(),
                Some(fixture.path().join("valid").into_os_string()),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("awacs.stateDir"), "{error}");
            assert_eq!(
                fs::read_to_string(fixture.path().join(".git/config")).unwrap(),
                contents
            );
        }
    }

    #[test]
    fn linked_worktrees_use_the_common_config() {
        let fixture = repository();
        let root = fixture.path().join("linked");
        let git_dir = fixture.path().join(".git/worktrees/linked");
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(root.join(".git"), b"gitdir: ../.git/worktrees/linked\n").unwrap();
        fs::write(git_dir.join("commondir"), b"../..\n").unwrap();
        let state = fixture.path().join("external");
        configure(&root, Some(state.clone().into_os_string())).unwrap();
        assert_eq!(state_root_for_root(fixture.path()).unwrap(), state);
        assert_eq!(state_root_for_root(&root).unwrap(), state);
        assert!(!git_dir.join("config").exists());
    }

    #[test]
    fn jj_internal_and_linked_workspaces_share_backend_config() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("main");
        let store = root.join(".jj/repo/store");
        fs::create_dir_all(store.join("git")).unwrap();
        fs::write(store.join("git_target"), b"git").unwrap();
        fs::write(store.join("git/config"), b"[core]\nbare = true\n").unwrap();
        let state = fixture.path().join("external");
        configure(&root, Some(state.clone().into_os_string())).unwrap();
        let linked = fixture.path().join("linked");
        fs::create_dir_all(linked.join(".jj")).unwrap();
        fs::write(linked.join(".jj/repo"), b"../../main/.jj/repo").unwrap();
        assert_eq!(state_root_for_root(&linked).unwrap(), state);
        assert_eq!(configure(&linked, None).unwrap(), state);
    }

    #[test]
    fn config_lock_failure_preserves_existing_file() {
        let fixture = repository();
        let before = fs::read(fixture.path().join(".git/config")).unwrap();
        let lock = fixture.path().join(".git/config.lock");
        fs::write(&lock, b"another writer").unwrap();
        let error = configure(
            fixture.path(),
            Some(fixture.path().join("external").into_os_string()),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("lock Git configuration"), "{error}");
        assert_eq!(
            fs::read(fixture.path().join(".git/config")).unwrap(),
            before
        );
        assert_eq!(fs::read(lock).unwrap(), b"another writer");
    }

    #[test]
    fn conversion_rejects_internal_state_through_symlinks_and_missing_parents() {
        let fixture = repository();
        let alias = fixture.path().join("alias");
        std::os::unix::fs::symlink(fixture.path().join(".git"), &alias).unwrap();
        for state in [
            fixture.path().to_owned(),
            fixture.path().join(".git/new/state"),
            alias.join("new/../state"),
        ] {
            let error = require_state_root_outside_checkout(fixture.path(), &state)
                .unwrap_err()
                .to_string();
            assert!(error.contains("inside checkout"), "{error}");
        }
        let outside = fixture.path().join("../new-state/nested/../store");
        require_state_root_outside_checkout(fixture.path(), &outside).unwrap();
    }

    #[test]
    fn conversion_rejects_an_internal_initial_path_without_saving_it() {
        let fixture = repository();
        let before = fs::read(fixture.path().join(".git/config")).unwrap();
        let initial = fixture.path().join(".git/awacs");
        let error =
            configure_state_root_with(fixture.path(), Some(initial.clone().into_os_string()), true)
                .unwrap_err()
                .to_string();
        assert!(error.contains("inside checkout"), "{error}");
        assert_eq!(
            fs::read(fixture.path().join(".git/config")).unwrap(),
            before
        );
        assert!(!initial.exists());
        assert!(!fixture.path().join(".git/config.lock").exists());
    }
}
