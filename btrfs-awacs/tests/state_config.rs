use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn awacs(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_awacs"));
    command
        .current_dir(root)
        .env_remove("BTRFS_AWACS_STATE_DIR")
        .env_remove("BTRFS_AWACS_MANAGED_DIR")
        .env_remove("AWACS_LOG")
        .env("AWACS_LOG_FILE", root.join("awacs-test.log"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    command
}

#[test]
fn init_without_state_directory_fails_before_conversion() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    git(root, &["init", "--quiet"]);
    fs::write(root.join("keep"), "unchanged").unwrap();
    let original_config = fs::read(root.join(".git/config")).unwrap();
    let original_inode = fs::metadata(root).unwrap().ino();

    let output = awacs(root).args(["init", "."]).output().unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("BTRFS_AWACS_STATE_DIR"), "{error}");
    assert!(error.contains("awacs.stateDir"), "{error}");
    assert_eq!(fs::read(root.join(".git/config")).unwrap(), original_config);
    assert_eq!(fs::metadata(root).unwrap().ino(), original_inode);
    assert_eq!(fs::read_to_string(root.join("keep")).unwrap(), "unchanged");
    assert!(!root.join(".git/awacs").exists());
}

#[test]
fn gc_reads_saved_config_from_a_subdirectory_without_environment() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    let state = fixture.path().join("saved state");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    git(&root, &["init", "--quiet"]);
    git(
        &root,
        &["config", "awacs.stateDir", state.to_str().unwrap()],
    );

    for wrong_env in [None, Some(fixture.path().join("wrong state"))] {
        let mut command = awacs(&root.join("nested"));
        if let Some(path) = &wrong_env {
            command.env("BTRFS_AWACS_STATE_DIR", path);
        }
        let output = command.args(["gc", "--dry-run"]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(state.to_str().unwrap()));
        if let Some(path) = wrong_env {
            assert!(!path.exists());
        }
    }
}

#[test]
fn gc_requires_config_or_an_explicit_state_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    let state = fixture.path().join("state");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    git(&root, &["init", "--quiet"]);
    let output = awacs(&root)
        .env("BTRFS_AWACS_STATE_DIR", &state)
        .args(["gc", "--dry-run"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("awacs.stateDir"));
    assert!(!root.join(".git/awacs").exists());

    let output = awacs(fixture.path())
        .args(["gc", "--dry-run", "--state-dir"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
