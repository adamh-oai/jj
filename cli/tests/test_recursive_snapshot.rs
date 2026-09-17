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

#![cfg(all(target_os = "linux", feature = "awacs"))]

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use btrfs_awacs::btrfs::OpenedSubvolume;
use btrfs_awacs::btrfs::ROOT_INODE;
use btrfs_awacs::btrfs::remove_tree;
use btrfs_awacs::btrfs::set_subvolume_readonly;
use testutils::TestResult;

use crate::common::TestEnvironment;

/// Own the containing directory, including retained migration sources and any
/// staging checkout left by a failed assertion. TempDir alone cannot remove
/// nested Btrfs subvolumes or read-only snapshots.
struct RecursiveSnapshotFixture {
    directory: tempfile::TempDir,
}

impl RecursiveSnapshotFixture {
    fn cleanup(&self) -> io::Result<()> {
        match remove_tree(self.directory.path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for RecursiveSnapshotFixture {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!(
                "Failed to remove recursive snapshot fixture {}: {error}",
                self.directory.path().display()
            );
        }
    }
}

fn create_subvolume(path: &Path) -> TestResult {
    let output = Command::new("btrfs")
        .args(["subvolume", "create"])
        .arg(path)
        .output()?;
    assert!(
        output.status.success(),
        "create fixture subvolume {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    OpenedSubvolume::open(path)?;
    Ok(())
}

#[test]
#[ignore = "requires JJ_TEST_BTRFS_ROOT, BTRFS_AWACS_COMMAND, and matching broker"]
fn test_subvolume_enable_preserves_recursive_metadata_snapshots() -> TestResult {
    let btrfs_root = std::env::var_os("JJ_TEST_BTRFS_ROOT")
        .map(PathBuf::from)
        .expect("JJ_TEST_BTRFS_ROOT must name a writable directory on real Btrfs");
    let awacs_command = std::env::var_os("BTRFS_AWACS_COMMAND")
        .map(PathBuf::from)
        .expect("BTRFS_AWACS_COMMAND must name the real awacs binary");
    let broker_directory = std::env::var_os("BTRFS_AWACS_BROKER_DIR");
    let broker_socket = std::env::var_os("BTRFS_AWACS_BROKER_SOCKET");
    assert!(
        btrfs_root.is_absolute(),
        "JJ_TEST_BTRFS_ROOT must be absolute"
    );
    assert!(
        awacs_command.is_absolute() && awacs_command.is_file(),
        "BTRFS_AWACS_COMMAND must name an existing absolute executable path"
    );
    assert!(
        broker_directory.is_some() || broker_socket.is_some(),
        "BTRFS_AWACS_BROKER_DIR or BTRFS_AWACS_BROKER_SOCKET must select a matching broker"
    );
    let probe = Command::new("btrfs")
        .args(["inspect-internal", "rootid"])
        .arg(&btrfs_root)
        .output()?;
    assert!(
        probe.status.success(),
        "JJ_TEST_BTRFS_ROOT is not on Btrfs: {}",
        String::from_utf8_lossy(&probe.stderr)
    );

    let fixture = RecursiveSnapshotFixture {
        directory: tempfile::Builder::new()
            .prefix("jj-recursive-migration-")
            .tempdir_in(&btrfs_root)?,
    };
    // Keep socket paths short even when JJ_TEST_BTRFS_ROOT has a long name.
    let runtime = tempfile::Builder::new().prefix("jj-rs-").tempdir()?;
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700))?;
    let hook = runtime.path().join("git-fsmonitor-awacs");
    std::os::unix::fs::symlink(&awacs_command, &hook)?;
    let mut executable_paths = vec![runtime.path().to_owned()];
    executable_paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let executable_path = std::env::join_paths(executable_paths)?;
    let log_path = fixture.directory.path().join("awacs.log");
    let configure_awacs = |command: &mut assert_cmd::Command| {
        command
            .env_remove("BTRFS_AWACS_STATE_DIR")
            .env_remove("BTRFS_AWACS_STATE_ROOT")
            .env_remove("BTRFS_AWACS_MANAGED_DIR")
            .env("PATH", &executable_path)
            .env("BTRFS_AWACS_COMMAND", &awacs_command)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("AWACS_LOG_FILE", &log_path);
        if let Some(directory) = &broker_directory {
            command.env("BTRFS_AWACS_BROKER_DIR", directory);
        }
        if let Some(socket) = &broker_socket {
            command.env("BTRFS_AWACS_BROKER_SOCKET", socket);
        }
    };

    let root = fixture.directory.path().join("repo");
    create_subvolume(&root)?;
    // Preserve the existing Git boundary. The migration must recursively
    // snapshot it while also converting the ordinary JJ metadata directory.
    create_subvolume(&root.join(".git"))?;
    let test_env = TestEnvironment::default();
    // An existing .git directory makes JJ attach to an existing repository.
    // Initialize Git inside the precreated subvolume before attaching JJ.
    let initialized = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["init", "--quiet", "--template=", "--initial-branch=main"])
        .arg(&root)
        .output()?;
    assert!(
        initialized.status.success(),
        "initialize Git inside its metadata subvolume: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    assert_eq!(fs::metadata(root.join(".git"))?.ino(), ROOT_INODE);
    let work_dir = test_env.work_dir(&root);
    work_dir
        .run_jj_with(|command| {
            configure_awacs(command);
            command.args(["git", "init", "--colocate", "."])
        })
        .success();
    work_dir.write_file("tracked", "tracked file survives recursive migration\n");
    work_dir
        .run_jj_with(|command| {
            configure_awacs(command);
            command.args(["commit", "-m", "initial"])
        })
        .success();
    assert_ne!(fs::metadata(root.join(".jj"))?.ino(), ROOT_INODE);

    let parent = Path::new(".git/custom-snapshots/parent");
    let grandchild = parent.join("ordinary/grandchild");
    let writable = Path::new(".git/custom-snapshots/writable");
    fs::create_dir_all(root.join(parent).parent().unwrap())?;
    create_subvolume(&root.join(parent))?;
    fs::create_dir(root.join(parent).join("ordinary"))?;
    create_subvolume(&root.join(&grandchild))?;
    create_subvolume(&root.join(writable))?;
    for relative in [parent, grandchild.as_path(), writable] {
        fs::write(
            root.join(relative).join("payload"),
            relative.as_os_str().as_encoded_bytes(),
        )?;
    }
    for relative in [grandchild.as_path(), parent] {
        let subvolume = OpenedSubvolume::open(&root.join(relative))?;
        set_subvolume_readonly(subvolume.as_fd(), true)?;
    }
    let topology = [
        Path::new(""),
        Path::new(".git"),
        parent,
        grandchild.as_path(),
        writable,
    ]
    .into_iter()
    .map(|relative| {
        OpenedSubvolume::open(&root.join(relative))
            .map(|subvolume| (relative.to_owned(), subvolume.subvolume))
    })
    .collect::<Result<Vec<_>, _>>()?;
    let revisions = || {
        work_dir
            .run_jj_with(|command| {
                configure_awacs(command);
                command.args([
                    "--ignore-working-copy",
                    "log",
                    "-r",
                    "@ | @-",
                    "--no-graph",
                    "-T",
                    "commit_id ++ \"\\n\"",
                ])
            })
            .success()
            .stdout
            .raw()
            .to_owned()
    };
    let original_revisions = revisions();
    work_dir
        .run_jj_with(|command| {
            configure_awacs(command);
            command.args(["util", "subvolume", "enable"])
        })
        .success();

    let status = work_dir
        .run_jj_with(|command| {
            configure_awacs(command);
            command.args(["--ignore-working-copy", "util", "subvolume", "status"])
        })
        .success();
    assert!(
        status.stdout.raw().contains("Consistency: OK"),
        "recursive migration must establish consistent AWACS state before another scan: {status}"
    );
    let retained_sources = fs::read_dir(fixture.directory.path())?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with("repo.jj-subvolume-source-")
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        retained_sources.len(),
        1,
        "retain exactly the original checkout"
    );
    let retained = &retained_sources[0];
    for (relative, original) in topology {
        let copied = OpenedSubvolume::open(&root.join(&relative))?.subvolume;
        let retained_original = OpenedSubvolume::open(&retained.join(&relative))?.subvolume;
        assert_ne!(
            copied.uuid,
            original.uuid,
            "snapshot UUID for {}",
            relative.display()
        );
        assert_eq!(
            copied.parent_uuid,
            Some(original.uuid),
            "snapshot lineage for {}",
            relative.display()
        );
        assert_eq!(
            copied.readonly(),
            original.readonly(),
            "read-only flag for {}",
            relative.display()
        );
        assert_eq!(
            retained_original.uuid,
            original.uuid,
            "retained UUID for {}",
            relative.display()
        );
        assert_eq!(
            retained_original.readonly(),
            original.readonly(),
            "retained read-only flag for {}",
            relative.display()
        );
    }
    assert_eq!(fs::metadata(root.join(".jj"))?.ino(), ROOT_INODE);
    assert_ne!(fs::metadata(retained.join(".jj"))?.ino(), ROOT_INODE);
    for checkout in [&root, retained] {
        assert_eq!(
            fs::read(checkout.join("tracked"))?,
            b"tracked file survives recursive migration\n"
        );
        for relative in [parent, grandchild.as_path(), writable] {
            assert_eq!(
                fs::read(checkout.join(relative).join("payload"))?,
                relative.as_os_str().as_encoded_bytes(),
                "recursive contents for {}",
                relative.display()
            );
        }
    }
    assert_eq!(revisions(), original_revisions);
    let diff = work_dir
        .run_jj_with(|command| {
            configure_awacs(command);
            command.args(["diff", "--summary"])
        })
        .success();
    assert!(
        diff.stdout.raw().is_empty(),
        "migration must not change tracked files: {diff}"
    );
    assert_eq!(
        revisions(),
        original_revisions,
        "a normal scan must preserve the commit"
    );
    fixture.cleanup()?;
    Ok(())
}
