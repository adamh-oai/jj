// Copyright 2022 The Jujutsu Authors
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

use itertools::Itertools as _;
use testutils::TestResult;

use crate::common::CommandOutput;
use crate::common::TestEnvironment;
use crate::common::TestWorkDir;

#[test]
fn test_concurrent_operation_divergence() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    work_dir.run_jj(["describe", "-m", "message 1"]).success();
    work_dir
        .run_jj(["describe", "-m", "message 2", "--at-op", "@-"])
        .success();

    // "--at-op=@" disables op heads merging, and prints head operation ids.
    let output = work_dir.run_jj(["op", "log", "--at-op=@"]);
    insta::assert_snapshot!(output, @r#"
    ------- stderr -------
    Error: The "@" expression resolved to more than one operation
    Hint: Try specifying one of the operations by ID: ea7c53a69ce3, 4633795cf208
    [EOF]
    [exit status: 1]
    "#);

    // "op log --at-op" should work without merging the head operations
    let output = work_dir.run_jj(["op", "log", "--at-op=4633795cf208"]);
    insta::assert_snapshot!(output, @"
    @  4633795cf208 test-username@host.example.com default@ 2001-02-03 04:05:09.000 +07:00 - 2001-02-03 04:05:09.000 +07:00
    │  describe commit e8849ae12c709f2321908879bc724fdb2ab8a781
    │  args: jj describe -m 'message 2' --at-op @-
    ○  f63ee16f9553 test-username@host.example.com 2001-02-03 04:05:07.000 +07:00 - 2001-02-03 04:05:07.000 +07:00
    │  add workspace 'default'
    ○  000000000000 root()
    [EOF]
    ");

    // We should be informed about the concurrent modification
    let output = get_log_output(&work_dir);
    insta::assert_snapshot!(output, @"
    @  message 1
    │ ○  message 2
    ├─╯
    ◆
    [EOF]
    ------- stderr -------
    Concurrent modification detected, resolving automatically.
    [EOF]
    ");
    Ok(())
}

#[test]
fn test_concurrent_operations_auto_rebase() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    work_dir.write_file("file", "contents");
    work_dir.run_jj(["describe", "-m", "initial"]).success();
    work_dir.run_jj(["describe", "-m", "rewritten"]).success();
    work_dir
        .run_jj(["new", "--at-op=@-", "-m", "new child"])
        .success();

    // We should be informed about the concurrent modification
    let output = get_log_output(&work_dir);
    insta::assert_snapshot!(output, @"
    ○  new child
    @  rewritten
    ◆
    [EOF]
    ------- stderr -------
    Concurrent modification detected, resolving automatically.
    Rebased 1 descendant commits onto commits rewritten by other operation.
    [EOF]
    ");
}

#[test]
fn test_concurrent_operations_wc_modified() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    work_dir.write_file("file", "contents\n");
    work_dir.run_jj(["describe", "-m", "initial"]).success();
    work_dir.run_jj(["new", "-m", "new child1"]).success();
    work_dir
        .run_jj(["new", "--at-op=@-", "-m", "new child2"])
        .success();
    work_dir.write_file("file", "modified\n");

    // We should be informed about the concurrent modification
    let output = get_log_output(&work_dir);
    insta::assert_snapshot!(output, @"
    @  new child1
    │ ○  new child2
    ├─╯
    ○  initial
    ◆
    [EOF]
    ------- stderr -------
    Concurrent modification detected, resolving automatically.
    [EOF]
    ");
    let output = work_dir.run_jj(["diff", "--git"]);
    insta::assert_snapshot!(output, @"
    diff --git a/file b/file
    index 12f00e90b6..2e0996000b 100644
    --- a/file
    +++ b/file
    @@ -1,1 +1,1 @@
    -contents
    +modified
    [EOF]
    ");

    // The working copy should be committed after merging the operations
    let output = work_dir.run_jj(["op", "log", "-Tdescription"]);
    insta::assert_snapshot!(output, @"
    @  snapshot working copy
    ○    reconcile divergent operations
    ├─╮
    ○ │  new empty commit
    │ ○  new empty commit
    ├─╯
    ○  describe commit 9a462e35578a347e6a3951bf7a58ad7146959a8b
    ○  snapshot working copy
    ○  add workspace 'default'
    ○
    [EOF]
    ");
}

#[test]
fn test_concurrent_snapshot_wc_reloadable() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    let op_heads_dir = work_dir
        .root()
        .join(".jj")
        .join("repo")
        .join("op_heads")
        .join("heads");

    work_dir.write_file("base", "");
    work_dir.run_jj(["commit", "-m", "initial"]).success();

    // Create new commit and checkout it.
    work_dir.write_file("child1", "");
    work_dir.run_jj(["commit", "-m", "new child1"]).success();

    let template = r#"id.short() ++ "\n" ++ description ++ "\n" ++ attributes"#;
    let output = work_dir.run_jj(["op", "log", "-T", template]);
    insta::assert_snapshot!(output, @"
    @  19e98b8a85da
    │  commit c91a0909a9d3f3d8392ba9fab88f4b40fc0810ee
    │  args: jj commit -m 'new child1'
    ○  55121d9434d0
    │  snapshot working copy
    │  args: jj commit -m 'new child1'
    ○  9f9c49403b99
    │  commit 9af4c151edead0304de97ce3a0b414552921a425
    │  args: jj commit -m initial
    ○  42ddf0a4e4f1
    │  snapshot working copy
    │  args: jj commit -m initial
    ○  f63ee16f9553
    │  add workspace 'default'
    ○  000000000000

    [EOF]
    ");
    let template = r#"id ++ "\n""#;
    let output = work_dir.run_jj(["op", "log", "--no-graph", "-T", template]);
    let [op_id_after_snapshot, _, op_id_before_snapshot] =
        output.stdout.raw().lines().next_array().unwrap();
    insta::assert_snapshot!(op_id_after_snapshot[..12], @"19e98b8a85da");
    insta::assert_snapshot!(op_id_before_snapshot[..12], @"9f9c49403b99");

    // Simulate a concurrent operation that began from the "initial" operation
    // (before the "child1" snapshot) but finished after the "child1"
    // snapshot and commit.
    std::fs::rename(
        op_heads_dir.join(op_id_after_snapshot),
        op_heads_dir.join(op_id_before_snapshot),
    )?;
    work_dir.write_file("child2", "");
    let output = work_dir.run_jj(["describe", "-m", "new child2"]);
    insta::assert_snapshot!(output, @"
    ------- stderr -------
    Working copy  (@) now at: kkmpptxz 493da83e new child2
    Parent commit (@-)      : rlvkpnrz 15bd889d new child1
    [EOF]
    ");

    // Since the repo can be reloaded before snapshotting, "child2" should be
    // a child of "child1", not of "initial".
    let output = work_dir.run_jj(["log", "-T", "description", "-s"]);
    insta::assert_snapshot!(output, @"
    @  new child2
    │  A child2
    ○  new child1
    │  A child1
    ○  initial
    │  A base
    ◆
    [EOF]
    ");
    Ok(())
}

#[test]
fn test_git_head_race_condition() -> TestResult {
    // Test for race condition where concurrent jj processes create divergent
    // operations when importing/exporting Git HEAD. This test spawns two
    // processes in parallel: one running `jj debug snapshot` repeatedly
    // (which imports Git HEAD) and another running `jj next/prev` repeatedly
    // (which exports Git HEAD). Without the fix, this would create divergent
    // operations.

    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "repo"])
        .success();
    let work_dir = test_env.work_dir("repo");

    // Create a large initial working copy to make snapshotting slower
    for j in 0..200 {
        let filename = format!("file_{j}");
        let content = format!("content for file {j}\n").repeat(100);
        work_dir.write_file(&filename, &content);
    }
    work_dir.run_jj(["commit", "-m", "initial"]).success();

    // Remember the initial commit for later verification
    let initial_commit = work_dir
        .run_jj(["log", "--no-graph", "-T=commit_id", "-r=@-"])
        .success()
        .stdout
        .into_raw();
    let initial_commit = initial_commit.trim().to_owned();

    // Create additional commits to iterate through with jj next/prev
    for i in 0..10 {
        for j in 0..50 {
            let filename = format!("file_{i}_{j}");
            let content = format!("content for commit {i} file {j}\n").repeat(100);
            work_dir.write_file(&filename, &content);
        }
        work_dir
            .run_jj(["commit", "-m", &format!("commit {i}")])
            .success();
    }

    // Extract environment from TestEnvironment to use in spawned processes
    let base_cmd = test_env.new_jj_cmd();
    let jj_bin = base_cmd.get_program().to_owned();
    let base_env: Vec<_> = base_cmd
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
        // Filter out timestamp and randomness seed so each command gets different values
        .filter(|(k, _)| k != "JJ_TIMESTAMP" && k != "JJ_OP_TIMESTAMP" && k != "JJ_RANDOMNESS_SEED")
        .collect();

    let repo_path = work_dir.root();
    let duration = std::time::Duration::from_secs(5);
    let start = std::time::Instant::now();

    std::thread::scope(|s| {
        s.spawn(|| {
            while start.elapsed() < duration {
                // Snapshot repeatedly without delay
                let mut cmd = std::process::Command::new(&jj_bin);
                cmd.current_dir(repo_path);
                cmd.args(["debug", "snapshot"]);
                for (key, value) in &base_env {
                    cmd.env(key, value);
                }
                let output = cmd.output().expect("Failed to spawn jj");
                assert!(output.status.success(), "jj debug snapshot failed");
            }
        });

        s.spawn(|| {
            let mut count = 0;
            while start.elapsed() < duration {
                // Go through commit history back and forth
                let direction = if count % 20 < 10 { "prev" } else { "next" };
                let mut cmd = std::process::Command::new(&jj_bin);
                cmd.current_dir(repo_path);
                cmd.arg(direction);
                for (key, value) in &base_env {
                    cmd.env(key, value);
                }
                let output = cmd.output().expect("Failed to spawn jj");
                assert!(output.status.success(), "jj {direction} failed");
                count += 1;
            }
        });
    });

    const IMPORT_GIT_HEAD: &str = "import git head";

    // Check for concurrent operations
    let output = work_dir.run_jj(["op", "log", "-T", "description", "--ignore-working-copy"]);
    let concurrent_count = output
        .stdout
        .raw()
        .lines()
        .filter(|line| line.contains(IMPORT_GIT_HEAD))
        .count();

    if concurrent_count > 0 {
        eprintln!("Found {concurrent_count} concurrent operations:");
        eprintln!("{}", output.stdout.raw());
        panic!("Race condition detected: {concurrent_count} concurrent operations found");
    }

    // Verify the operation description for importing Git HEAD hasn't changed
    // First ensure we're not already on the initial commit (prev/next loop may have
    // ended there)
    work_dir.run_jj(["new"]).success();
    // Use git to checkout the initial commit, then trigger import
    std::process::Command::new("git")
        .current_dir(work_dir.root())
        .args(["checkout", "-q", &initial_commit])
        .status()?;

    let last_op = work_dir
        .run_jj(["op", "log", "-T", "description", "--limit=1"])
        .success()
        .stdout
        .into_raw();

    assert!(
        last_op.contains(IMPORT_GIT_HEAD),
        "Expected last operation to contain '{IMPORT_GIT_HEAD}', got: {last_op:?}"
    );
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "awacs", feature = "test-fakes", debug_assertions))]
#[test]
fn test_snapshot_workspace_index_seeding_only_blocks_its_worktree() -> TestResult {
    use std::process::Command;
    use std::time::Duration;

    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    let child = test_env.work_dir("child");
    main.write_file("file", "contents\n");
    main.run_jj(["commit", "-m", "first"]).success();
    main.write_file(".jj/working_copy/subvolume_mode", "snapshot-backed\n");
    let path = crate::test_workspaces::install_fake_btrfs(
        &test_env,
        r#"#!/bin/sh
if [ "$1" = "subvolume" ] && [ "$2" = "show" ]; then
    exit 0
fi
if [ "$1" = "inspect-internal" ] && [ "$2" = "rootid" ]; then
    echo 5
    exit 0
fi
if [ "$1" = "subvolume" ] && [ "$2" = "snapshot" ]; then
    /bin/cp -a "$3" "$4"
    exit $?
fi
exit 1
"#,
    );
    main.run_jj_with(|cmd| {
        cmd.env("PATH", &path)
            .env("JJ_TEST_AWACS_SCAN_ROOT", main.root())
            .args(["util", "subvolume", "enable", "--rebuild-baseline"])
    })
    .success();

    let pause = test_env.env_root().join("pause-index-seeding");
    std::fs::create_dir(&pause)?;
    let spawn = |work_dir: &TestWorkDir, label: &str, args: &[&str], paused: bool| {
        let template = test_env.new_jj_cmd();
        let stdout_path = test_env.env_root().join(format!("{label}.stdout"));
        let stderr_path = test_env.env_root().join(format!("{label}.stderr"));
        let mut command = Command::new(template.get_program());
        command.env_clear();
        for (key, value) in template.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        command
            .current_dir(work_dir.root())
            .env("PATH", &path)
            .env("JJ_LOG", "info")
            .env("JJ_TEST_AWACS_SCAN_ROOT", work_dir.root())
            .args(args)
            .stdout(std::fs::File::create(&stdout_path)?)
            .stderr(std::fs::File::create(&stderr_path)?);
        if paused {
            command.env("JJ_TEST_SNAPSHOT_SEED_GIT_INDEX_PAUSE_DIR", &pause);
        }
        Ok::<_, std::io::Error>(TestChild {
            child: command.spawn()?,
            stdout_path,
            stderr_path,
        })
    };
    let mut creator = spawn(
        &main,
        "snapshot-workspace-creator",
        &["workspace", "add", "--colocate", "../child"],
        true,
    )?;
    wait_for_child_event(&mut creator, || pause.join("ready").exists());

    // The new workspace is visible, but index seeding holds its worktree
    // gate. Wait for an actual lock-wait message, not just a process start.
    let mut waiter = spawn(&child, "snapshot-workspace-waiter", &["status"], false)?;
    let waiter_stderr = waiter.stderr_path.clone();
    wait_for_child_event(&mut waiter, || {
        std::fs::read_to_string(&waiter_stderr).is_ok_and(|log| {
            log.lines()
                .any(|line| line.contains("Waiting for lock") && line.contains("git-worktree-"))
        })
    });

    // A status in the source worktree must complete while both the creator
    // and the child-worktree waiter are still blocked at the barrier.
    main.run_jj_with(|cmd| {
        cmd.timeout(Duration::from_secs(10))
            .env("PATH", &path)
            .env("JJ_TEST_AWACS_SCAN_ROOT", main.root())
            .arg("status")
    })
    .success();
    assert!(creator.child.try_wait()?.is_none());
    assert!(waiter.child.try_wait()?.is_none());
    std::fs::write(pause.join("release"), b"release\n")?;
    creator.assert_success();
    waiter.assert_success();
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_slow_snapshot_does_not_block_other_worktrees() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("base", "base contents\n");
    main.run_jj(["commit", "-m", "base"]).success();
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let secondary = test_env.work_dir("secondary");
    main.write_file("from-main", "main contents\n");
    secondary.write_file("from-secondary", "secondary contents\n");

    let pause_dir = test_env.env_root().join("pause-snapshot");
    std::fs::create_dir(&pause_dir)?;
    let waiter_pause_dir = test_env.env_root().join("pause-waiter");
    std::fs::create_dir(&waiter_pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());

    // The scanner is paused with its working-copy lock held. A second command
    // in the same worktree must wait, but must not hold the shared Git lock
    // while it does so.
    let mut waiter = spawn_traced_jj(
        &test_env,
        &main,
        "waiter",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_PAUSE_DIR", &waiter_pause_dir)),
    )?;
    wait_for_child_event(&mut waiter, || {
        waiter_log_mentions(&test_env, "waiter", "git-worktree-")
    });
    let output = secondary.run_jj_with(|cmd| {
        cmd.timeout(std::time::Duration::from_secs(10))
            .arg("status")
    });
    output.success();
    assert!(
        scanner.child.try_wait()?.is_none(),
        "scanner was not paused"
    );
    assert!(waiter.child.try_wait()?.is_none(), "waiter was not blocked");
    assert_eq!(
        secondary
            .run_jj(["--ignore-working-copy", "file", "show", "from-secondary"])
            .success()
            .stdout
            .raw(),
        "secondary contents\n"
    );

    std::fs::write(pause_dir.join("release"), b"release\n")?;
    scanner.assert_success();
    // Pause the waiter before it can scan, so this assertion proves that the
    // first scanner itself committed A's edit despite B's concurrent change.
    wait_for_child_event(&mut waiter, || waiter_pause_dir.join("ready").exists());
    assert_eq!(
        main.run_jj(["--ignore-working-copy", "file", "show", "from-main"])
            .success()
            .stdout
            .raw(),
        "main contents\n"
    );
    std::fs::write(waiter_pause_dir.join("release"), b"release\n")?;
    waiter.assert_success();
    assert_eq!(
        secondary
            .run_jj(["--ignore-working-copy", "file", "show", "from-secondary"])
            .success()
            .stdout
            .raw(),
        "secondary contents\n"
    );
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_slow_snapshot_finish_does_not_block_other_worktrees() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("base", "base\n");
    main.run_jj(["commit", "-m", "base"]).success();
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let secondary = test_env.work_dir("secondary");
    main.write_file("from-main", "main contents\n");
    secondary.write_file("from-secondary", "secondary contents\n");

    let pause_dir = test_env.env_root().join("pause-finish");
    std::fs::create_dir(&pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &[
            "log",
            "--no-graph",
            "-r",
            "all()",
            "-T",
            "description ++ \"\\n\"",
        ],
        Some(("JJ_TEST_SNAPSHOT_FINISH_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());
    // The first command still owns its journal but has published its repo
    // operation. A different workspace can publish an operation in the gap.
    secondary
        .run_jj_with(|cmd| {
            cmd.timeout(std::time::Duration::from_secs(10)).args([
                "describe",
                "-m",
                "secondary changed",
            ])
        })
        .success();
    assert!(scanner.child.try_wait()?.is_none());
    std::fs::write(pause_dir.join("release"), b"release\n")?;
    scanner.assert_success();
    assert!(std::fs::read_to_string(&scanner.stdout_path)?.contains("secondary changed"));
    for (work_dir, path, contents) in [
        (&main, "from-main", "main contents\n"),
        (&secondary, "from-secondary", "secondary contents\n"),
    ] {
        assert_eq!(
            work_dir
                .run_jj(["--ignore-working-copy", "file", "show", path])
                .success()
                .stdout
                .raw(),
            contents
        );
    }
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_snapshot_finish_verification_preserves_journal() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("saved", "contents\n");
    let pause_dir = test_env.env_root().join("pause-after-finish");
    std::fs::create_dir(&pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_AFTER_FINISH_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());
    let journal = main.root().join(".jj/working_copy");
    let saved_checkout = std::fs::read(journal.join("checkout"))?;
    let saved_tree_state = std::fs::read(journal.join("tree_state"))?;
    let operation = || {
        main.run_jj([
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "-T",
            "id",
            "-n",
            "1",
        ])
        .success()
        .stdout
        .into_raw()
    };
    let saved_operation = operation();
    std::fs::write(pause_dir.join("release"), b"release\n")?;
    scanner.assert_success();
    assert_eq!(std::fs::read(journal.join("checkout"))?, saved_checkout);
    assert_eq!(std::fs::read(journal.join("tree_state"))?, saved_tree_state);
    assert_eq!(operation(), saved_operation);
    Ok(())
}

#[cfg(all(
    feature = "test-fakes",
    feature = "awacs",
    target_os = "linux",
    debug_assertions
))]
#[test]
fn test_snapshot_finish_verification_preserves_awacs_journal() -> TestResult {
    let mut test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let root = test_env.env_root().join("main");
    test_env.add_env_var("JJ_TEST_AWACS_SCAN_ROOT", &root);
    let main = test_env.work_dir("main");
    main.write_file("saved", "contents\n");
    main.run_jj(["new"]).success();
    std::fs::write(
        main.root().join(".jj/working_copy/subvolume_mode"),
        b"snapshot-backed\n",
    )?;
    main.run_jj(["util", "subvolume", "enable", "--rebuild-baseline"])
        .success();

    let pause_dir = test_env.env_root().join("pause-after-finish");
    std::fs::create_dir(&pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_AFTER_FINISH_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());
    let journal = main.root().join(".jj/working_copy/checkout");
    let saved_journal = std::fs::read(&journal)?;
    assert!(saved_journal.starts_with(b"\0JJ-WORKING-COPY-STATE\0v1\n"));
    let saved_operation = main.current_operation_id();
    std::fs::write(pause_dir.join("release"), b"release\n")?;
    scanner.assert_success();
    assert_eq!(std::fs::read(journal)?, saved_journal);
    assert_eq!(main.current_operation_id(), saved_operation);
    Ok(())
}

#[cfg(all(
    feature = "test-fakes",
    feature = "awacs",
    target_os = "linux",
    debug_assertions
))]
#[test]
fn test_awacs_rechecks_sparse_patterns_before_git_import() -> TestResult {
    for pause_variable in [
        "JJ_TEST_SNAPSHOT_PREFLIGHT_PAUSE_DIR",
        "JJ_TEST_IMPORT_HEAD_PRE_JOURNAL_PAUSE_DIR",
    ] {
        let mut test_env = TestEnvironment::default();
        test_env
            .run_jj_in(".", ["git", "init", "--colocate", "main"])
            .success();
        let root = test_env.env_root().join("main");
        test_env.add_env_var("JJ_TEST_AWACS_SCAN_ROOT", &root);
        test_env.add_env_var("JJ_TEST_AWACS_REAL_FINGERPRINT", "1");
        let main = test_env.work_dir("main");
        main.write_file("subset/file", "old contents\n");
        main.run_jj(["commit", "-m", "old"]).success();
        main.write_file("subset/file", "new contents\n");
        main.run_jj(["commit", "-m", "new"]).success();
        let old_commit = main
            .run_jj([
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                "@--",
                "-T",
                "commit_id",
            ])
            .success()
            .stdout
            .into_raw()
            .trim()
            .to_owned();
        std::fs::write(
            main.root().join(".jj/working_copy/subvolume_mode"),
            b"snapshot-backed\n",
        )?;
        main.run_jj(["util", "subvolume", "enable", "--rebuild-baseline"])
            .success();

        // Let the sparse command finish its own snapshot first. It can then
        // update the journal while the scanner waits to run its preflight.
        let sparse_pause = test_env.env_root().join("pause-sparse");
        std::fs::create_dir(&sparse_pause)?;
        let mut sparse = spawn_traced_jj(
            &test_env,
            &main,
            "sparse",
            &["sparse", "set", "--clear", "--add", "subset"],
            Some(("JJ_TEST_SPARSE_PRE_MUTATION_PAUSE_DIR", &sparse_pause)),
        )?;
        wait_for_child_event(&mut sparse, || sparse_pause.join("ready").exists());
        // The new sparse patterns invalidate the baseline. A stale fingerprint
        // must not let this external Git checkout be imported before the error.
        let template = test_env.new_jj_cmd();
        let mut git = std::process::Command::new("git");
        git.env_clear();
        for (key, value) in template.get_envs() {
            if let Some(value) = value {
                git.env(key, value);
            }
        }
        let output = git
            .current_dir(main.root())
            .args(["checkout", "--detach", "--quiet", &old_commit])
            .output()?;
        assert!(
            output.status.success(),
            "git checkout failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let preflight_pause = test_env.env_root().join("pause-preflight");
        std::fs::create_dir(&preflight_pause)?;
        let mut scanner = spawn_traced_jj(
            &test_env,
            &main,
            "scanner",
            &["status"],
            Some((pause_variable, &preflight_pause)),
        )?;
        wait_for_child_event(&mut scanner, || preflight_pause.join("ready").exists());
        std::fs::write(sparse_pause.join("release"), b"release\n")?;
        sparse.assert_success();

        let saved_operation = main.current_operation_id();
        let journal = main.root().join(".jj/working_copy/checkout");
        let saved_journal = std::fs::read(&journal)?;
        std::fs::write(preflight_pause.join("release"), b"release\n")?;
        scanner.assert_failure_containing("--rebuild-baseline");
        assert_eq!(main.current_operation_id(), saved_operation);
        assert_eq!(std::fs::read(journal)?, saved_journal);
    }
    Ok(())
}

#[cfg(all(
    feature = "test-fakes",
    feature = "awacs",
    target_os = "linux",
    debug_assertions
))]
#[test]
fn test_awacs_rechecks_journal_when_importing_deleted_git_head() -> TestResult {
    for (delete_during_reread, integrate) in [(false, true), (true, true), (false, false)] {
        let mut test_env = TestEnvironment::default();
        test_env
            .run_jj_in(".", ["git", "init", "--colocate", "main"])
            .success();
        let root = test_env.env_root().join("main");
        test_env.add_env_var("JJ_TEST_AWACS_SCAN_ROOT", &root);
        test_env.add_env_var("JJ_TEST_AWACS_REAL_FINGERPRINT", "1");
        let import_pause = test_env.env_root().join("pause-import");
        std::fs::create_dir(&import_pause)?;
        if delete_during_reread {
            test_env.add_env_var("JJ_TEST_IMPORT_HEAD_PAUSE_DIR", &import_pause);
        }
        let main = test_env.work_dir("main");
        main.write_file("subset/file", "old contents\n");
        main.run_jj(["commit", "-m", "old"]).success();
        main.write_file("subset/file", "new contents\n");
        main.run_jj(["commit", "-m", "new"]).success();
        let old_commit = main
            .run_jj([
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                "@--",
                "-T",
                "commit_id",
            ])
            .success()
            .stdout
            .into_raw()
            .trim()
            .to_owned();
        std::fs::write(
            main.root().join(".jj/working_copy/subvolume_mode"),
            b"snapshot-backed\n",
        )?;
        main.run_jj(["util", "subvolume", "enable", "--rebuild-baseline"])
            .success();

        let sparse_pause = test_env.env_root().join("pause-sparse");
        std::fs::create_dir(&sparse_pause)?;
        let mut sparse = spawn_traced_jj(
            &test_env,
            &main,
            "sparse",
            &["sparse", "set", "--clear", "--add", "subset"],
            Some(("JJ_TEST_SPARSE_PRE_MUTATION_PAUSE_DIR", &sparse_pause)),
        )?;
        wait_for_child_event(&mut sparse, || sparse_pause.join("ready").exists());
        let run_git = |args: &[&str]| -> TestResult {
            let template = test_env.new_jj_cmd();
            let mut git = std::process::Command::new("git");
            git.env_clear();
            for (key, value) in template.get_envs() {
                if let Some(value) = value {
                    git.env(key, value);
                }
            }
            let output = git.current_dir(main.root()).args(args).output()?;
            assert!(
                output.status.success(),
                "git command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(())
        };
        run_git(&["checkout", "--detach", "--quiet", &old_commit])?;
        if !delete_during_reread {
            run_git(&["symbolic-ref", "HEAD", "refs/heads/missing"])?;
        }

        let transaction_pause = test_env.env_root().join("pause-import-transaction");
        std::fs::create_dir(&transaction_pause)?;
        let args = if integrate {
            vec!["status"]
        } else {
            vec!["--no-integrate-operation", "status"]
        };
        let mut scanner = spawn_traced_jj(
            &test_env,
            &main,
            "scanner",
            &args,
            Some((
                "JJ_TEST_IMPORT_HEAD_TRANSACTION_PRE_JOURNAL_PAUSE_DIR",
                &transaction_pause,
            )),
        )?;
        if delete_during_reread {
            wait_for_child_event(&mut scanner, || import_pause.join("ready").exists());
            run_git(&["symbolic-ref", "HEAD", "refs/heads/missing"])?;
            std::fs::write(import_pause.join("release"), b"release\n")?;
        }
        wait_for_child_event(&mut scanner, || transaction_pause.join("ready").exists());
        std::fs::write(sparse_pause.join("release"), b"release\n")?;
        sparse.assert_success();
        let saved_operation = main.current_operation_id();
        let journal = main.root().join(".jj/working_copy/checkout");
        let saved_journal = std::fs::read(&journal)?;
        std::fs::write(transaction_pause.join("release"), b"release\n")?;
        scanner.assert_failure_containing("working copy changed");
        assert_eq!(main.current_operation_id(), saved_operation);
        assert_eq!(std::fs::read(journal)?, saved_journal);
    }
    Ok(())
}

#[cfg(all(
    feature = "test-fakes",
    feature = "awacs",
    target_os = "linux",
    debug_assertions
))]
#[test]
fn test_import_deleted_git_head_accepts_fresh_lagging_journal() -> TestResult {
    for use_awacs in [false, true] {
        let mut test_env = TestEnvironment::default();
        test_env
            .run_jj_in(".", ["git", "init", "--colocate", "main"])
            .success();
        if use_awacs {
            let root = test_env.env_root().join("main");
            test_env.add_env_var("JJ_TEST_AWACS_SCAN_ROOT", &root);
            test_env.add_env_var("JJ_TEST_AWACS_REAL_FINGERPRINT", "1");
        }
        let main = test_env.work_dir("main");
        main.write_file("saved", "contents\n");
        main.run_jj(["commit", "-m", "base"]).success();
        if use_awacs {
            std::fs::write(
                main.root().join(".jj/working_copy/subvolume_mode"),
                b"snapshot-backed\n",
            )?;
            main.run_jj(["util", "subvolume", "enable", "--rebuild-baseline"])
                .success();
        }
        let journal = main.root().join(".jj/working_copy/checkout");
        let before_journal = std::fs::read(&journal)?;
        let before_operation = main.current_operation_id();
        main.run_jj([
            "--ignore-working-copy",
            "bookmark",
            "set",
            "other",
            "-r",
            "@-",
        ])
        .success();
        assert_ne!(main.current_operation_id(), before_operation);
        assert_eq!(std::fs::read(&journal)?, before_journal);

        let template = test_env.new_jj_cmd();
        let mut git = std::process::Command::new("git");
        git.env_clear();
        for (key, value) in template.get_envs() {
            if let Some(value) = value {
                git.env(key, value);
            }
        }
        let output = git
            .current_dir(main.root())
            .args(["symbolic-ref", "HEAD", "refs/heads/missing"])
            .output()?;
        assert!(output.status.success());
        main.run_jj(["status"]).success();
        assert_eq!(main.read_file("saved"), "contents\n");
    }
    Ok(())
}

#[cfg(all(
    feature = "test-fakes",
    feature = "awacs",
    target_os = "linux",
    debug_assertions
))]
#[test]
fn test_import_deleted_git_head_rejects_changed_subvolume_mode() -> TestResult {
    for initial_awacs in [false, true] {
        let mut test_env = TestEnvironment::default();
        test_env
            .run_jj_in(".", ["git", "init", "--colocate", "main"])
            .success();
        let root = test_env.env_root().join("main");
        test_env.add_env_var("JJ_TEST_AWACS_SCAN_ROOT", &root);
        let main = test_env.work_dir("main");
        main.write_file("saved", "contents\n");
        main.run_jj(["commit", "-m", "base"]).success();
        let marker = main.root().join(".jj/working_copy/subvolume_mode");
        if initial_awacs {
            std::fs::write(&marker, b"snapshot-backed\n")?;
            main.run_jj(["util", "subvolume", "enable", "--rebuild-baseline"])
                .success();
        }
        let template = test_env.new_jj_cmd();
        let mut git = std::process::Command::new("git");
        git.env_clear();
        for (key, value) in template.get_envs() {
            if let Some(value) = value {
                git.env(key, value);
            }
        }
        let output = git
            .current_dir(main.root())
            .args(["symbolic-ref", "HEAD", "refs/heads/missing"])
            .output()?;
        assert!(output.status.success());

        let pause = test_env.env_root().join("pause-transaction");
        std::fs::create_dir(&pause)?;
        let mut scanner = spawn_traced_jj(
            &test_env,
            &main,
            "scanner",
            &["status"],
            Some((
                "JJ_TEST_IMPORT_HEAD_TRANSACTION_PRE_JOURNAL_PAUSE_DIR",
                &pause,
            )),
        )?;
        wait_for_child_event(&mut scanner, || pause.join("ready").exists());
        let saved_operation = main.current_operation_id();
        let journal = main.root().join(".jj/working_copy/checkout");
        let saved_journal = std::fs::read(&journal)?;
        if initial_awacs {
            std::fs::remove_file(&marker)?;
        } else {
            std::fs::write(&marker, b"snapshot-backed\n")?;
        }
        std::fs::write(pause.join("release"), b"release\n")?;
        scanner.assert_failure_containing("subvolume mode changed");
        assert_eq!(main.current_operation_id(), saved_operation);
        assert_eq!(std::fs::read(journal)?, saved_journal);
    }
    Ok(())
}

#[cfg(all(
    feature = "test-fakes",
    feature = "awacs",
    target_os = "linux",
    debug_assertions
))]
#[test]
fn test_awacs_non_colocated_snapshot_rejects_changed_sparse_patterns() -> TestResult {
    let mut test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "main"]).success();
    let root = test_env.env_root().join("main");
    test_env.add_env_var("JJ_TEST_AWACS_SCAN_ROOT", &root);
    test_env.add_env_var("JJ_TEST_AWACS_REAL_FINGERPRINT", "1");
    let main = test_env.work_dir("main");
    main.write_file("subset/file", "contents\n");
    main.run_jj(["new"]).success();
    std::fs::write(
        main.root().join(".jj/working_copy/subvolume_mode"),
        b"snapshot-backed\n",
    )?;
    main.run_jj(["util", "subvolume", "enable", "--rebuild-baseline"])
        .success();

    let sparse_pause = test_env.env_root().join("pause-sparse");
    std::fs::create_dir(&sparse_pause)?;
    let mut sparse = spawn_traced_jj(
        &test_env,
        &main,
        "sparse",
        &["sparse", "set", "--clear", "--add", "subset"],
        Some(("JJ_TEST_SPARSE_PRE_MUTATION_PAUSE_DIR", &sparse_pause)),
    )?;
    wait_for_child_event(&mut sparse, || sparse_pause.join("ready").exists());
    let snapshot_pause = test_env.env_root().join("pause-snapshot");
    std::fs::create_dir(&snapshot_pause)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_PRE_JOURNAL_PAUSE_DIR", &snapshot_pause)),
    )?;
    wait_for_child_event(&mut scanner, || snapshot_pause.join("ready").exists());
    std::fs::write(sparse_pause.join("release"), b"release\n")?;
    sparse.assert_success();
    let saved_operation = main.current_operation_id();
    let journal = main.root().join(".jj/working_copy/checkout");
    let saved_journal = std::fs::read(&journal)?;
    std::fs::write(snapshot_pause.join("release"), b"release\n")?;
    scanner.assert_failure_containing("sparse patterns changed");
    assert_eq!(main.current_operation_id(), saved_operation);
    assert_eq!(std::fs::read(journal)?, saved_journal);
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_snapshot_finish_retries_after_sparse_patterns_change() -> TestResult {
    for pause_variable in [
        "JJ_TEST_SNAPSHOT_PRE_JOURNAL_PAUSE_DIR",
        "JJ_TEST_SNAPSHOT_AFTER_FINISH_PAUSE_DIR",
    ] {
        let test_env = TestEnvironment::default();
        test_env
            .run_jj_in(".", ["git", "init", "--colocate", "main"])
            .success();
        let main = test_env.work_dir("main");
        main.write_file("subset/file", "contents\n");
        main.run_jj(["status"]).success();
        let initial_operation = main
            .run_jj([
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "-T",
                "id",
                "-n",
                "1",
            ])
            .success()
            .stdout
            .into_raw();

        // This process has finished its helper snapshot, and no longer owns the
        // worktree gate, but has not yet locked the journal for sparse changes.
        let sparse_pause = test_env.env_root().join("pause-sparse");
        std::fs::create_dir(&sparse_pause)?;
        let mut sparse = spawn_traced_jj(
            &test_env,
            &main,
            "sparse",
            &["sparse", "set", "--clear", "--add", "subset"],
            Some(("JJ_TEST_SPARSE_PRE_MUTATION_PAUSE_DIR", &sparse_pause)),
        )?;
        wait_for_child_event(&mut sparse, || sparse_pause.join("ready").exists());
        let finish_pause = test_env.env_root().join("pause-after-finish");
        std::fs::create_dir(&finish_pause)?;
        let mut scanner = spawn_traced_jj(
            &test_env,
            &main,
            "scanner",
            &["sparse", "list"],
            Some((pause_variable, &finish_pause)),
        )?;
        wait_for_child_event(&mut scanner, || finish_pause.join("ready").exists());
        std::fs::write(sparse_pause.join("release"), b"release\n")?;
        sparse.assert_success();
        let final_operation = main
            .run_jj([
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "-T",
                "id",
                "-n",
                "1",
            ])
            .success()
            .stdout
            .into_raw();
        assert_eq!(initial_operation, final_operation);
        std::fs::write(finish_pause.join("release"), b"release\n")?;
        scanner.assert_success();
        assert_eq!(std::fs::read_to_string(&scanner.stdout_path)?, "subset\n");
    }
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_snapshot_finish_reacquires_after_older_process() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("edited", "edited\n");

    let pause_dir = test_env.env_root().join("pause-finish");
    std::fs::create_dir(&pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_FINISH_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());
    let helper_dir = test_env.env_root().join("old-lock-helper");
    std::fs::create_dir(&helper_dir)?;
    let mut old_holder = spawn_old_lock_holder(&test_env, &main, &helper_dir)?;
    wait_for_child_event(&mut old_holder, || helper_dir.join("waiting").exists());
    assert!(!helper_dir.join("acquired").exists());
    std::fs::write(pause_dir.join("release"), b"release\n")?;
    old_holder.assert_success();
    scanner.assert_success();
    assert!(helper_dir.join("acquired").exists());
    assert_eq!(
        main.run_jj(["--ignore-working-copy", "file", "show", "edited"])
            .success()
            .stdout
            .raw(),
        "edited\n"
    );
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_snapshot_finish_releases_lock_for_untracked_reports() -> TestResult {
    for (config, file, contents) in [
        ("snapshot.auto-track='none()'", "excluded", "file\n"),
        ("snapshot.max-new-file-size=1", "large", "large file\n"),
    ] {
        let test_env = TestEnvironment::default();
        test_env
            .run_jj_in(".", ["git", "init", "--colocate", "main"])
            .success();
        let main = test_env.work_dir("main");
        main.write_file(file, contents);
        let pause_dir = test_env.env_root().join("pause-finish");
        std::fs::create_dir(&pause_dir)?;
        let mut scanner = spawn_traced_jj(
            &test_env,
            &main,
            "scanner",
            &["--config", config, "status"],
            Some(("JJ_TEST_SNAPSHOT_FINISH_PAUSE_DIR", &pause_dir)),
        )?;
        wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());
        assert!(
            jj_lib::lock::FileLock::try_lock(main.root().join(".jj/repo/git_import_export.lock"))?
                .is_some(),
            "snapshot with {file} held the shared lock"
        );
        std::fs::write(pause_dir.join("release"), b"release\n")?;
        scanner.assert_success();
        let stdout = std::fs::read_to_string(&scanner.stdout_path)?;
        let stderr = std::fs::read_to_string(&scanner.stderr_path)?;
        assert!(
            stdout.contains(file) || stderr.contains(file),
            "{stdout}\n{stderr}"
        );
    }
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_snapshot_finish_retry_reports_current_untracked_paths() -> TestResult {
    let test_awacs = cfg!(all(target_os = "linux", feature = "awacs", debug_assertions));
    for snapshot_backed in [false, true] {
        if snapshot_backed && !test_awacs {
            continue;
        }
        let mut test_env = TestEnvironment::default();
        if snapshot_backed {
            let root = test_env.env_root().join("main");
            test_env.add_env_var("JJ_TEST_AWACS_SCAN_ROOT", root);
        }
        test_env.add_config("snapshot.max-new-file-size = 1");
        test_env
            .run_jj_in(".", ["git", "init", "--colocate", "main"])
            .success();
        let main = test_env.work_dir("main");
        main.run_jj(["workspace", "add", "--colocate", "../secondary"])
            .success();
        let secondary = test_env.work_dir("secondary");
        main.write_file("kept", "large contents\n");
        main.write_file("removed", "large contents\n");
        if snapshot_backed {
            main.write_file(".jj/working_copy/subvolume_mode", "snapshot-backed\n");
            main.run_jj(["util", "subvolume", "enable", "--rebuild-baseline"])
                .success();
        }
        let pause_dir = test_env.env_root().join("pause-after-finish");
        std::fs::create_dir(&pause_dir)?;
        let mut scanner = spawn_traced_jj(
            &test_env,
            &main,
            "scanner",
            &["status"],
            Some(("JJ_TEST_SNAPSHOT_AFTER_FINISH_PAUSE_DIR", &pause_dir)),
        )?;
        wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());
        // Advance the shared operation after the first scan is committed so the
        // scanner must retry. Removed paths must not be reported as still present.
        std::fs::remove_file(main.root().join("removed"))?;
        secondary
            .run_jj(["bookmark", "set", "unrelated", "-r", "@"])
            .success();
        std::fs::write(pause_dir.join("release"), b"release\n")?;
        scanner.assert_success();
        let stderr = std::fs::read_to_string(&scanner.stderr_path)?;
        assert!(stderr.contains("  kept:"), "{stderr}");
        assert!(!stderr.contains("  removed:"), "{stderr}");
    }
    Ok(())
}

#[cfg(all(feature = "test-fakes", unix))]
#[test]
fn test_snapshot_finish_releases_lock_for_invalid_names() -> TestResult {
    use std::os::unix::ffi::OsStringExt as _;

    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    std::fs::write(
        main.root().join(std::ffi::OsString::from_vec(vec![0xff])),
        b"file",
    )?;
    let pause_dir = test_env.env_root().join("pause-finish");
    std::fs::create_dir(&pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_FINISH_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());
    assert!(
        jj_lib::lock::FileLock::try_lock(main.root().join(".jj/repo/git_import_export.lock"))?
            .is_some(),
        "snapshot with an invalid filename held the shared lock"
    );
    std::fs::write(pause_dir.join("release"), b"release\n")?;
    scanner.assert_success();
    assert!(std::fs::read_to_string(&scanner.stderr_path)?.contains("UTF-8"));
    Ok(())
}

#[cfg(all(feature = "test-fakes", unix))]
#[test]
fn test_snapshot_finish_retry_reports_current_invalid_names() -> TestResult {
    use std::os::unix::ffi::OsStringExt as _;

    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let secondary = test_env.work_dir("secondary");
    let kept = std::ffi::OsString::from_vec(vec![b'k', 0xff]);
    let removed = std::ffi::OsString::from_vec(vec![b'r', 0xff]);
    std::fs::write(main.root().join(&kept), b"file")?;
    std::fs::write(main.root().join(&removed), b"file")?;
    let pause_dir = test_env.env_root().join("pause-after-finish");
    std::fs::create_dir(&pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_AFTER_FINISH_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());
    std::fs::remove_file(main.root().join(&removed))?;
    secondary
        .run_jj(["bookmark", "set", "unrelated", "-r", "@"])
        .success();
    std::fs::write(pause_dir.join("release"), b"release\n")?;
    scanner.assert_success();
    let stderr = std::fs::read_to_string(&scanner.stderr_path)?;
    assert!(stderr.contains(&format!("{kept:?}")), "{stderr}");
    assert!(!stderr.contains(&format!("{removed:?}")), "{stderr}");
    Ok(())
}

#[cfg(all(feature = "test-fakes", feature = "watchman"))]
#[test]
fn test_snapshot_finish_releases_lock_for_monitor_warnings() -> TestResult {
    let mut test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    test_env
        .run_jj_in("main", ["workspace", "add", "--colocate", "../secondary"])
        .success();
    test_env.add_config(r#"fsmonitor.backend = "watchman""#);
    let missing_socket = test_env.env_root().join("missing-watchman.sock");
    test_env.add_env_var("WATCHMAN_SOCK", &missing_socket);
    let main = test_env.work_dir("main");
    let secondary = test_env.work_dir("secondary");
    let pause_dir = test_env.env_root().join("pause-finish");
    std::fs::create_dir(&pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_AFTER_FINISH_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());
    assert!(
        jj_lib::lock::FileLock::try_lock(main.root().join(".jj/repo/git_import_export.lock"))?
            .is_some(),
        "snapshot with a filesystem monitor warning held the shared lock"
    );
    secondary
        .run_jj(["bookmark", "set", "unrelated", "-r", "@"])
        .success();
    std::fs::write(pause_dir.join("release"), b"release\n")?;
    scanner.assert_success();
    let stderr = std::fs::read_to_string(&scanner.stderr_path)?;
    assert_eq!(stderr.matches("Failed to query filesystem monitor:").count(), 1, "{stderr}");
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_snapshot_finish_retries_after_external_git_checkout() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("checkout-file", "old contents\n");
    main.run_jj(["commit", "-m", "old"]).success();
    main.write_file("checkout-file", "new contents\n");
    main.run_jj(["commit", "-m", "new"]).success();
    let old_commit = main
        .run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@--",
            "-T",
            "commit_id",
        ])
        .success()
        .stdout
        .into_raw()
        .trim()
        .to_owned();
    main.write_file("from-main", "main contents\n");

    let pause_dir = test_env.env_root().join("pause-finish");
    std::fs::create_dir(&pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_FINISH_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());
    let template = test_env.new_jj_cmd();
    let mut git = std::process::Command::new("git");
    git.env_clear();
    for (key, value) in template.get_envs() {
        if let Some(value) = value {
            git.env(key, value);
        }
    }
    let output = git
        .current_dir(main.root())
        .args(["checkout", "--detach", "--quiet", &old_commit])
        .output()?;
    assert!(
        output.status.success(),
        "git checkout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(pause_dir.join("release"), b"release\n")?;
    scanner.assert_success();
    let parent = main
        .run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@-",
            "-T",
            "commit_id",
        ])
        .success();
    assert_eq!(parent.stdout.raw().trim(), old_commit);
    for (path, contents) in [
        ("from-main", "main contents\n"),
        ("checkout-file", "old contents\n"),
    ] {
        assert_eq!(
            main.run_jj(["--ignore-working-copy", "file", "show", path])
                .success()
                .stdout
                .raw(),
            contents
        );
    }
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_waiting_for_working_copy_does_not_block_other_worktrees() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("base", "base contents\n");
    main.run_jj(["commit", "-m", "base"]).success();
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let secondary = test_env.work_dir("secondary");
    main.write_file("from-main", "main contents\n");
    secondary.write_file("from-secondary", "secondary contents\n");

    // Simulate a slow process holding the existing working-copy journal lock.
    let lock =
        jj_lib::lock::FileLock::lock(main.root().join(".jj/working_copy/working_copy.lock"))?;
    let mut waiter = spawn_traced_jj(&test_env, &main, "journal-waiter", &["status"], None)?;
    wait_for_child_event(&mut waiter, || {
        waiter_log_mentions(&test_env, "journal-waiter", "working_copy.lock")
    });
    let output = secondary.run_jj_with(|cmd| {
        cmd.timeout(std::time::Duration::from_secs(10))
            .arg("status")
    });
    output.success();
    assert!(waiter.child.try_wait()?.is_none(), "waiter was not blocked");
    assert_eq!(
        secondary
            .run_jj(["--ignore-working-copy", "file", "show", "from-secondary"])
            .success()
            .stdout
            .raw(),
        "secondary contents\n"
    );

    drop(lock);
    waiter.assert_success();
    assert_eq!(
        main.run_jj(["--ignore-working-copy", "file", "show", "from-main"])
            .success()
            .stdout
            .raw(),
        "main contents\n"
    );
    Ok(())
}

#[cfg(all(unix, feature = "awacs", feature = "test-fakes"))]
#[test]
fn test_waiting_for_awacs_preflight_does_not_block_other_worktrees() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("base", "base contents\n");
    main.run_jj(["commit", "-m", "base"]).success();
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let secondary = test_env.work_dir("secondary");

    // Establish a synthetic committed baseline so this command reaches the
    // AWACS preflight used before importing Git HEAD.
    std::fs::write(
        main.root().join(".jj/working_copy/subvolume_mode"),
        b"enabling\n",
    )?;
    main.run_jj_with(|cmd| {
        cmd.env("JJ_TEST_AWACS_SCAN_ROOT", main.root()).args([
            "util",
            "subvolume",
            "enable",
            "--rebuild-baseline",
        ])
    })
    .success();

    let journal =
        jj_lib::lock::FileLock::lock(main.root().join(".jj/working_copy/working_copy.lock"))?;
    let mut waiter = spawn_traced_jj(
        &test_env,
        &main,
        "awacs-preflight-waiter",
        &["status"],
        Some(("JJ_TEST_AWACS_SCAN_ROOT", main.root())),
    )?;
    wait_for_child_event(&mut waiter, || {
        waiter_log_mentions(&test_env, "awacs-preflight-waiter", "working_copy.lock")
    });
    secondary.write_file("from-secondary", "secondary contents\n");
    secondary
        .run_jj_with(|cmd| {
            cmd.timeout(std::time::Duration::from_secs(10))
                .arg("status")
        })
        .success();
    assert!(waiter.child.try_wait()?.is_none(), "waiter was not blocked");
    assert_eq!(
        secondary
            .run_jj(["--ignore-working-copy", "file", "show", "from-secondary"])
            .success()
            .stdout
            .raw(),
        "secondary contents\n"
    );

    drop(journal);
    waiter.assert_success();
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_git_head_import_rejects_stale_journal_after_another_worktree_rewrites() -> TestResult {
    let mut test_env = TestEnvironment::default();
    test_env.add_env_var("GIT_OPTIONAL_LOCKS", "0");
    test_env.add_config("snapshot.auto-update-stale = true\n");
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("base", "base\n");
    main.run_jj(["commit", "-m", "blank"]).success();
    main.write_file("file", "original\n");
    main.run_jj(["new"]).success();
    let blank = main
        .run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@--",
            "-T",
            "commit_id",
        ])
        .success()
        .stdout
        .into_raw()
        .trim()
        .to_owned();
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let secondary = test_env.work_dir("secondary");
    main.write_file("file", "UNSNAPSHOTTED MAIN EDIT\n");
    secondary.remove_file("file");
    set_test_git_head(&test_env, &main, &blank)?;
    let checkout_path = main.root().join(".jj/working_copy/checkout");
    let tree_state_path = main.root().join(".jj/working_copy/tree_state");
    let checkout_before = std::fs::read(&checkout_path)?;
    let tree_state_before = std::fs::read(&tree_state_path)?;

    let lock =
        jj_lib::lock::FileLock::lock(main.root().join(".jj/working_copy/working_copy.lock"))?;
    let mut importer = spawn_traced_jj(
        &test_env,
        &main,
        "stale-head-importer",
        &["--config", "snapshot.auto-track=\"none()\"", "status"],
        None,
    )?;
    wait_for_child_event(&mut importer, || {
        waiter_log_mentions(&test_env, "stale-head-importer", "working_copy.lock")
    });
    // This rewrites main's parent, rebases its commit, and makes the journal
    // stale while the import is waiting for the journal lock.
    secondary
        .run_jj_with(|cmd| {
            cmd.timeout(std::time::Duration::from_secs(10))
                .arg("squash")
        })
        .success();
    let listed = secondary
        .run_jj(["--ignore-working-copy", "file", "list", "-r", "default@"])
        .success();
    assert!(!listed.stdout.raw().lines().any(|line| line == "file"));
    let operation_after_squash = main.current_operation_id();
    drop(lock);
    importer.assert_failure_containing("The working copy is stale");
    let stderr = std::fs::read_to_string(&importer.stderr_path)?;
    assert!(stderr.contains("Automatic stale recovery is disabled"));
    assert!(stderr.contains("operation log"));
    assert_eq!(main.read_file("file"), "UNSNAPSHOTTED MAIN EDIT\n");
    assert_eq!(std::fs::read(checkout_path)?, checkout_before);
    assert_eq!(std::fs::read(tree_state_path)?, tree_state_before);
    assert_eq!(main.current_operation_id(), operation_after_squash);
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_git_head_import_from_updated_journal_does_not_retry_forever() -> TestResult {
    let mut test_env = TestEnvironment::default();
    test_env.add_env_var("GIT_OPTIONAL_LOCKS", "0");
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    for contents in ["first\n", "second\n"] {
        main.write_file("file", contents);
        main.run_jj(["commit", "-m", contents.trim()]).success();
    }
    main.write_file("file", "third\n");
    main.run_jj(["describe", "-m", "third"]).success();
    let old_op = main.current_operation_id();
    main.run_jj(["new"]).success();
    let journal_op = main.current_operation_id();
    let first = main
        .run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@---",
            "-T",
            "commit_id",
        ])
        .success()
        .stdout
        .into_raw()
        .trim()
        .to_owned();
    // Simulate a journal pointing to an operation ahead of the published head.
    main.remove_file(format!(".jj/repo/op_heads/heads/{journal_op}"));
    main.write_file(format!(".jj/repo/op_heads/heads/{old_op}"), "");
    main.write_file("file", "local edit\n");
    set_test_git_head(&test_env, &main, &first)?;
    main.run_jj_with(|cmd| {
        cmd.timeout(std::time::Duration::from_secs(20)).args([
            "--config",
            "snapshot.auto-track=\"none()\"",
            "status",
        ])
    })
    .success();
    let parent = main
        .run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@-",
            "-T",
            "commit_id",
        ])
        .success()
        .stdout
        .into_raw();
    assert_eq!(parent.trim(), first);
    assert_eq!(
        main.run_jj(["--ignore-working-copy", "file", "show", "file"])
            .success()
            .stdout
            .raw(),
        "local edit\n"
    );
    Ok(())
}

#[cfg(feature = "test-fakes")]
fn set_test_git_head(test_env: &TestEnvironment, work_dir: &TestWorkDir, id: &str) -> TestResult {
    let template = test_env.new_jj_cmd();
    let mut git = std::process::Command::new("git");
    git.env_clear();
    for (key, value) in template.get_envs() {
        if let Some(value) = value {
            git.env(key, value);
        }
    }
    let output = git
        .current_dir(work_dir.root())
        .args(["update-ref", "--no-deref", "HEAD", id])
        .output()?;
    assert!(
        output.status.success(),
        "git update-ref failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_git_head_import_stale_journal_without_integrating_fails_safely() -> TestResult {
    let mut test_env = TestEnvironment::default();
    test_env.add_env_var("GIT_OPTIONAL_LOCKS", "0");
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("base", "base\n");
    main.run_jj(["commit", "-m", "blank"]).success();
    main.write_file("file", "original\n");
    main.run_jj(["new"]).success();
    let blank = main
        .run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@--",
            "-T",
            "commit_id",
        ])
        .success()
        .stdout
        .into_raw()
        .trim()
        .to_owned();
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let secondary = test_env.work_dir("secondary");
    main.write_file("file", "UNSNAPSHOTTED MAIN EDIT\n");
    secondary.remove_file("file");
    secondary.run_jj(["squash"]).success();
    set_test_git_head(&test_env, &main, &blank)?;

    let output = main.run_jj([
        "--config",
        "snapshot.auto-track=\"none()\"",
        "--no-integrate-operation",
        "status",
    ]);
    assert!(
        !output.status.success(),
        "stale import unexpectedly succeeded: {output}"
    );
    assert!(
        output.stderr.raw().contains("The working copy is stale"),
        "{output}"
    );
    assert_eq!(main.read_file("file"), "UNSNAPSHOTTED MAIN EDIT\n");
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_git_head_import_does_not_restore_forgotten_workspace() -> TestResult {
    let mut test_env = TestEnvironment::default();
    test_env.add_env_var("GIT_OPTIONAL_LOCKS", "0");
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("file", "first\n");
    main.run_jj(["commit", "-m", "first"]).success();
    main.write_file("file", "second\n");
    main.run_jj(["commit", "-m", "second"]).success();
    let first = main
        .run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@--",
            "-T",
            "commit_id",
        ])
        .success()
        .stdout
        .into_raw()
        .trim()
        .to_owned();
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let secondary = test_env.work_dir("secondary");
    secondary
        .run_jj(["workspace", "forget", "default"])
        .success();
    set_test_git_head(&test_env, &main, &first)?;
    let output = main.run_jj(["status"]);
    assert!(
        !output.status.success(),
        "forgotten workspace was restored: {output}"
    );
    assert!(
        output
            .stderr
            .raw()
            .contains("doesn't have a working-copy commit"),
        "{output}"
    );
    assert!(
        !secondary
            .run_jj(["workspace", "list"])
            .success()
            .stdout
            .raw()
            .contains("default:")
    );
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_git_head_import_waiting_for_working_copy_does_not_block_other_worktrees() -> TestResult {
    check_git_head_import_wait(false)
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_git_head_import_retries_after_another_worktree_publishes() -> TestResult {
    check_git_head_import_wait(true)
}

#[cfg(feature = "test-fakes")]
fn check_git_head_import_wait(publish_other: bool) -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    for contents in ["first\n", "second\n", "third\n"] {
        main.write_file("checkout-file", contents);
        main.run_jj(["commit", "-m", contents.trim()]).success();
    }
    let commit_id = |revision| {
        main.run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            revision,
            "-T",
            "commit_id",
        ])
        .success()
        .stdout
        .into_raw()
        .trim()
        .to_owned()
    };
    let first_commit = commit_id("@---");
    let second_commit = commit_id("@--");
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let secondary = test_env.work_dir("secondary");
    secondary.run_jj(["status"]).success();
    main.write_file("from-main", "main contents\n");
    if publish_other {
        secondary.write_file("from-secondary", "secondary contents\n");
    }

    let operation_id = || {
        main.run_jj([
            "--ignore-working-copy",
            "op",
            "log",
            "--limit=1",
            "-T",
            "id",
        ])
        .success()
        .stdout
        .into_raw()
    };
    let initial_op = operation_id();

    let checkout = |commit: &str| -> TestResult {
        let template = test_env.new_jj_cmd();
        let mut git = std::process::Command::new("git");
        git.env_clear();
        for (key, value) in template.get_envs() {
            if let Some(value) = value {
                git.env(key, value);
            }
        }
        let output = git
            .current_dir(main.root())
            .args(["checkout", "--detach", "--quiet", commit])
            .output()?;
        assert!(
            output.status.success(),
            "git checkout failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    checkout(&first_commit)?;

    // A sees a changed Git HEAD before trying to take its journal lock.
    let lock =
        jj_lib::lock::FileLock::lock(main.root().join(".jj/working_copy/working_copy.lock"))?;
    let mut importer = spawn_traced_jj(&test_env, &main, "head-importer", &["status"], None)?;
    wait_for_child_event(&mut importer, || {
        waiter_log_mentions(&test_env, "head-importer", "working_copy.lock")
    });
    secondary
        .run_jj_with(|cmd| {
            cmd.timeout(std::time::Duration::from_secs(10))
                .arg("status")
        })
        .success();
    assert!(
        importer.child.try_wait()?.is_none(),
        "importer was not blocked"
    );
    assert_eq!(operation_id() != initial_op, publish_other);

    // Change HEAD while A waits. With a separate operation published, A must
    // reload it. In both cases it must import the latest HEAD, not the earlier
    // one it saw before waiting.
    checkout(&second_commit)?;
    drop(lock);
    importer.assert_success();
    assert_eq!(commit_id("@-"), second_commit);
    assert_eq!(main.read_file("checkout-file"), "second\n");
    assert_eq!(
        main.run_jj(["--ignore-working-copy", "file", "show", "from-main"])
            .success()
            .stdout
            .raw(),
        "main contents\n"
    );
    if publish_other {
        assert_eq!(
            secondary
                .run_jj(["--ignore-working-copy", "file", "show", "from-secondary"])
                .success()
                .stdout
                .raw(),
            "secondary contents\n"
        );
    }
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_git_head_import_retries_when_older_process_holds_shared_lock() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("checkout-file", "old\n");
    main.run_jj(["commit", "-m", "old"]).success();
    main.write_file("checkout-file", "new\n");
    main.run_jj(["commit", "-m", "new"]).success();
    let old_commit = main
        .run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@--",
            "-T",
            "commit_id",
        ])
        .success()
        .stdout
        .into_raw()
        .trim()
        .to_owned();
    main.write_file("from-main", "main contents\n");
    let template = test_env.new_jj_cmd();
    let mut git = std::process::Command::new("git");
    git.env_clear();
    for (key, value) in template.get_envs() {
        if let Some(value) = value {
            git.env(key, value);
        }
    }
    let output = git
        .current_dir(main.root())
        .args(["checkout", "--detach", "--quiet", &old_commit])
        .output()?;
    assert!(
        output.status.success(),
        "git checkout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let pause_dir = test_env.env_root().join("pause-import-head");
    std::fs::create_dir(&pause_dir)?;
    let mut importer = spawn_traced_jj(
        &test_env,
        &main,
        "head-importer",
        &["status"],
        Some(("JJ_TEST_IMPORT_HEAD_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut importer, || pause_dir.join("ready").exists());
    let helper_dir = test_env.env_root().join("old-lock-helper");
    std::fs::create_dir(&helper_dir)?;
    let mut old_holder = spawn_old_lock_holder(&test_env, &main, &helper_dir)?;
    wait_for_child_event(&mut old_holder, || helper_dir.join("waiting").exists());
    assert!(!helper_dir.join("acquired").exists());

    // The older process holds the shared lock while waiting for A's journal.
    // A must release the journal before waiting for the shared lock and retry.
    std::fs::write(pause_dir.join("release"), b"release\n")?;
    old_holder.assert_success();
    assert!(helper_dir.join("acquired").exists());
    importer.assert_success();
    assert_eq!(
        main.run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@-",
            "-T",
            "commit_id",
        ])
        .success()
        .stdout
        .raw()
        .trim(),
        old_commit
    );
    assert_eq!(
        main.run_jj(["--ignore-working-copy", "file", "show", "from-main"])
            .success()
            .stdout
            .raw(),
        "main contents\n"
    );
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_slow_checkout_does_not_block_other_worktrees() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("checkout-file", "old contents\n");
    main.run_jj(["commit", "-m", "old"]).success();
    main.write_file("checkout-file", "new contents\n");
    main.run_jj(["commit", "-m", "new"]).success();
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    let secondary = test_env.work_dir("secondary");
    secondary.write_file("from-secondary", "secondary contents\n");

    let pause_dir = test_env.env_root().join("pause-checkout");
    std::fs::create_dir(&pause_dir)?;
    let mut checkout = spawn_traced_jj(
        &test_env,
        &main,
        "checkout",
        &["new", "@--"],
        Some(("JJ_TEST_CHECKOUT_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut checkout, || pause_dir.join("ready").exists());
    assert_eq!(main.read_file("checkout-file"), "new contents\n");
    let journal_path = main.root().join(".jj/working_copy/working_copy.lock");
    assert!(
        jj_lib::lock::FileLock::try_lock(journal_path)?.is_none(),
        "checkout must exclude older processes that do not take the worktree gate"
    );

    // A same-worktree command waits on the gate while checkout is paused;
    // it must not prevent an independent worktree from recording its edit.
    let mut waiter = spawn_traced_jj(&test_env, &main, "checkout-waiter", &["status"], None)?;
    wait_for_child_event(&mut waiter, || {
        waiter_log_mentions(&test_env, "checkout-waiter", "git-worktree-")
    });
    secondary
        .run_jj_with(|cmd| {
            cmd.timeout(std::time::Duration::from_secs(10))
                .arg("status")
        })
        .success();
    assert!(
        checkout.child.try_wait()?.is_none(),
        "checkout was not paused"
    );
    assert!(waiter.child.try_wait()?.is_none(), "waiter was not blocked");
    assert_eq!(
        secondary
            .run_jj(["--ignore-working-copy", "file", "show", "from-secondary"])
            .success()
            .stdout
            .raw(),
        "secondary contents\n"
    );

    std::fs::write(pause_dir.join("release"), b"release\n")?;
    checkout.assert_success();
    waiter.assert_success();
    assert_eq!(main.read_file("checkout-file"), "old contents\n");
    assert_eq!(
        main.run_jj(["--ignore-working-copy", "file", "show", "checkout-file"])
            .success()
            .stdout
            .raw(),
        "old contents\n"
    );
    assert_eq!(
        secondary
            .run_jj(["--ignore-working-copy", "file", "show", "from-secondary"])
            .success()
            .stdout
            .raw(),
        "secondary contents\n"
    );
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_scan_retries_after_external_git_checkout() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("checkout-file", "old contents\n");
    main.run_jj(["commit", "-m", "old"]).success();
    main.write_file("checkout-file", "new contents\n");
    main.run_jj(["commit", "-m", "new"]).success();
    let old_commit = main
        .run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@--",
            "-T",
            "commit_id",
        ])
        .success()
        .stdout
        .into_raw()
        .trim()
        .to_owned();
    main.write_file("from-main", "main contents\n");

    let pause_dir = test_env.env_root().join("pause-snapshot");
    std::fs::create_dir(&pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());

    // Git does not honor JJ's lock. Move HEAD and materialize the old tree
    // while the first JJ scan is paused; the untracked edit must survive.
    let template = test_env.new_jj_cmd();
    let mut git = std::process::Command::new("git");
    git.env_clear();
    for (key, value) in template.get_envs() {
        if let Some(value) = value {
            git.env(key, value);
        }
    }
    let output = git
        .current_dir(main.root())
        .args(["checkout", "--detach", "--quiet", &old_commit])
        .output()?;
    assert!(
        output.status.success(),
        "git checkout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(main.read_file("checkout-file"), "old contents\n");
    assert_eq!(main.read_file("from-main"), "main contents\n");

    std::fs::write(pause_dir.join("release"), b"release\n")?;
    scanner.assert_success();
    let parent = main
        .run_jj([
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "@-",
            "-T",
            "commit_id",
        ])
        .success();
    assert_eq!(parent.stdout.raw().trim(), old_commit);
    assert_eq!(
        main.run_jj(["--ignore-working-copy", "file", "show", "from-main"])
            .success()
            .stdout
            .raw(),
        "main contents\n"
    );
    assert_eq!(
        main.run_jj(["--ignore-working-copy", "file", "show", "checkout-file"])
            .success()
            .stdout
            .raw(),
        "old contents\n"
    );
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_scan_retries_when_older_process_holds_shared_lock() -> TestResult {
    if let Some(helper_dir) = std::env::var_os("JJ_TEST_OLD_LOCK_HELPER") {
        // Re-run this test in a separate process to model an older jj which
        // takes the shared lock before waiting for the working-copy lock.
        let helper_dir = std::path::PathBuf::from(helper_dir);
        let shared_path = std::path::PathBuf::from(
            std::env::var_os("JJ_TEST_OLD_SHARED_LOCK").expect("shared lock path"),
        );
        let journal_path = std::path::PathBuf::from(
            std::env::var_os("JJ_TEST_OLD_JOURNAL_LOCK").expect("journal lock path"),
        );
        let _shared = jj_lib::lock::FileLock::lock(shared_path)?;
        std::fs::write(helper_dir.join("waiting"), b"waiting\n")?;
        let _journal = jj_lib::lock::FileLock::lock(journal_path)?;
        std::fs::write(helper_dir.join("acquired"), b"acquired\n")?;
        return Ok(());
    }

    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("base", "base contents\n");
    main.run_jj(["commit", "-m", "base"]).success();
    main.write_file("from-main", "main contents\n");

    let pause_dir = test_env.env_root().join("pause-snapshot");
    std::fs::create_dir(&pause_dir)?;
    let mut scanner = spawn_traced_jj(
        &test_env,
        &main,
        "scanner",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_PAUSE_DIR", &pause_dir)),
    )?;
    wait_for_child_event(&mut scanner, || pause_dir.join("ready").exists());

    let helper_dir = test_env.env_root().join("old-lock-helper");
    std::fs::create_dir(&helper_dir)?;
    let mut old_holder = spawn_old_lock_holder(&test_env, &main, &helper_dir)?;
    wait_for_child_event(&mut old_holder, || helper_dir.join("waiting").exists());
    assert!(!helper_dir.join("acquired").exists());

    // The scanner cannot wait for the shared lock while keeping its journal
    // lock. It must drop that lock, let the old process finish, and retry.
    std::fs::write(pause_dir.join("release"), b"release\n")?;
    old_holder.assert_success();
    assert!(helper_dir.join("acquired").exists());
    scanner.assert_success();
    assert_eq!(
        main.run_jj(["--ignore-working-copy", "file", "show", "from-main"])
            .success()
            .stdout
            .raw(),
        "main contents\n"
    );
    Ok(())
}

#[cfg(feature = "test-fakes")]
#[test]
fn test_conflicting_worktree_scans_take_turns_retrying() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env
        .run_jj_in(".", ["git", "init", "--colocate", "main"])
        .success();
    let main = test_env.work_dir("main");
    main.write_file("base", "base contents\n");
    main.run_jj(["commit", "-m", "base"]).success();
    main.run_jj(["workspace", "add", "--colocate", "../secondary"])
        .success();
    main.run_jj(["workspace", "add", "--colocate", "../unrelated"])
        .success();
    let secondary = test_env.work_dir("secondary");
    let unrelated = test_env.work_dir("unrelated");
    main.write_file("from-main", "main contents\n");
    secondary.write_file("from-secondary", "secondary contents\n");

    let main_pause = test_env.env_root().join("pause-main-scan");
    let secondary_pause = test_env.env_root().join("pause-secondary-scan");
    let retry_pause = test_env.env_root().join("pause-retry");
    for dir in [&main_pause, &secondary_pause, &retry_pause] {
        std::fs::create_dir(dir)?;
    }
    let mut first = spawn_traced_jj_with_pauses(
        &test_env,
        &main,
        "first-scan",
        &["status"],
        &[
            ("JJ_TEST_SNAPSHOT_PAUSE_DIR", &main_pause),
            ("JJ_TEST_SNAPSHOT_RETRY_PAUSE_DIR", &retry_pause),
        ],
    )?;
    wait_for_child_event(&mut first, || main_pause.join("ready").exists());
    let mut second = spawn_traced_jj(
        &test_env,
        &secondary,
        "second-scan",
        &["status"],
        Some(("JJ_TEST_SNAPSHOT_PAUSE_DIR", &secondary_pause)),
    )?;
    wait_for_child_event(&mut second, || secondary_pause.join("ready").exists());

    // Force both scans to fail their nonblocking shared-lock acquisition.
    // The first scanner keeps the retry gate while the second waits, but
    // neither can wait for it while holding its working-copy lock.
    let shared = jj_lib::lock::FileLock::lock(main.root().join(".jj/repo/git_import_export.lock"))?;
    std::fs::write(main_pause.join("release"), b"release\n")?;
    wait_for_child_event(&mut first, || retry_pause.join("ready").exists());
    std::fs::write(secondary_pause.join("release"), b"release\n")?;
    wait_for_child_event(&mut second, || {
        waiter_log_mentions(&test_env, "second-scan", "git_import_export_retry.lock")
    });
    assert!(
        jj_lib::lock::FileLock::try_lock(
            secondary.root().join(".jj/working_copy/working_copy.lock")
        )?
        .is_some(),
        "the waiting scanner must release its journal"
    );

    // A scan that does not encounter contention can still proceed while a
    // retry is in progress in other worktrees.
    drop(shared);
    unrelated
        .run_jj_with(|cmd| {
            cmd.timeout(std::time::Duration::from_secs(10))
                .arg("status")
        })
        .success();
    std::fs::write(retry_pause.join("release"), b"release\n")?;
    first.assert_success();
    second.assert_success();
    for (work_dir, file, contents) in [
        (&main, "from-main", "main contents\n"),
        (&secondary, "from-secondary", "secondary contents\n"),
    ] {
        assert_eq!(
            work_dir
                .run_jj(["--ignore-working-copy", "file", "show", file])
                .success()
                .stdout
                .raw(),
            contents
        );
    }
    Ok(())
}

// Child processes are killed and reaped even when a timeout or assertion
// fails, so a stalled lock holder cannot hang the rest of the test suite.
#[cfg(feature = "test-fakes")]
struct TestChild {
    child: std::process::Child,
    stdout_path: std::path::PathBuf,
    stderr_path: std::path::PathBuf,
}

#[cfg(feature = "test-fakes")]
impl TestChild {
    fn assert_success(&mut self) {
        let status = self.wait();
        assert!(
            status.success(),
            "child exited with {status}\nstdout:\n{}\nstderr:\n{}",
            std::fs::read_to_string(&self.stdout_path).unwrap_or_default(),
            std::fs::read_to_string(&self.stderr_path).unwrap_or_default(),
        );
    }

    fn assert_failure_containing(&mut self, expected: &str) {
        let status = self.wait();
        let stderr = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
        assert!(
            !status.success() && stderr.contains(expected),
            "unexpected child result {status}\nstdout:\n{}\nstderr:\n{stderr}",
            std::fs::read_to_string(&self.stdout_path).unwrap_or_default(),
        );
    }

    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll child process") {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child timed out\nstdout:\n{}\nstderr:\n{}",
                std::fs::read_to_string(&self.stdout_path).unwrap_or_default(),
                std::fs::read_to_string(&self.stderr_path).unwrap_or_default(),
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[cfg(feature = "test-fakes")]
impl Drop for TestChild {
    fn drop(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

#[cfg(feature = "test-fakes")]
fn spawn_traced_jj(
    test_env: &TestEnvironment,
    work_dir: &TestWorkDir,
    label: &str,
    args: &[&str],
    pause: Option<(&str, &std::path::Path)>,
) -> std::io::Result<TestChild> {
    let pauses = pause.into_iter().collect::<Vec<_>>();
    spawn_traced_jj_with_pauses(test_env, work_dir, label, args, &pauses)
}

#[cfg(feature = "test-fakes")]
fn spawn_traced_jj_with_pauses(
    test_env: &TestEnvironment,
    work_dir: &TestWorkDir,
    label: &str,
    args: &[&str],
    pauses: &[(&str, &std::path::Path)],
) -> std::io::Result<TestChild> {
    // Use the same isolated configuration and binary as run_jj(), but keep
    // the child handle so the test can observe and clean up blocked processes.
    let template = test_env.new_jj_cmd();
    let stdout_path = test_env.env_root().join(format!("{label}.stdout"));
    let stderr_path = test_env.env_root().join(format!("{label}.stderr"));
    let mut command = std::process::Command::new(template.get_program());
    command.env_clear();
    for (key, value) in template.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    command
        .current_dir(work_dir.root())
        .env("JJ_LOG", "info")
        .args(args)
        .stdout(std::fs::File::create(&stdout_path)?)
        .stderr(std::fs::File::create(&stderr_path)?);
    for (env_name, pause_dir) in pauses {
        command.env(env_name, pause_dir);
    }
    Ok(TestChild {
        child: command.spawn()?,
        stdout_path,
        stderr_path,
    })
}

#[cfg(feature = "test-fakes")]
fn spawn_old_lock_holder(
    test_env: &TestEnvironment,
    work_dir: &TestWorkDir,
    helper_dir: &std::path::Path,
) -> std::io::Result<TestChild> {
    let stdout_path = test_env.env_root().join("old-holder.stdout");
    let stderr_path = test_env.env_root().join("old-holder.stderr");
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "test_concurrent_operations::test_scan_retries_when_older_process_holds_shared_lock",
        ])
        .env("JJ_TEST_OLD_LOCK_HELPER", helper_dir)
        .env(
            "JJ_TEST_OLD_SHARED_LOCK",
            work_dir.root().join(".jj/repo/git_import_export.lock"),
        )
        .env(
            "JJ_TEST_OLD_JOURNAL_LOCK",
            work_dir.root().join(".jj/working_copy/working_copy.lock"),
        )
        .stdout(std::fs::File::create(&stdout_path)?)
        .stderr(std::fs::File::create(&stderr_path)?);
    Ok(TestChild {
        child: command.spawn()?,
        stdout_path,
        stderr_path,
    })
}

#[cfg(feature = "test-fakes")]
fn waiter_log_mentions(test_env: &TestEnvironment, label: &str, lock_name: &str) -> bool {
    let path = test_env.env_root().join(format!("{label}.stderr"));
    std::fs::read_to_string(path).is_ok_and(|log| {
        log.lines()
            .any(|line| line.contains("Attempting to lock") && line.contains(lock_name))
    })
}

#[cfg(feature = "test-fakes")]
fn wait_for_child_event(child: &mut TestChild, event: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !event() {
        assert!(
            child
                .child
                .try_wait()
                .expect("poll child process")
                .is_none(),
            "child exited before reaching barrier\nstdout:\n{}\nstderr:\n{}",
            std::fs::read_to_string(&child.stdout_path).unwrap_or_default(),
            std::fs::read_to_string(&child.stderr_path).unwrap_or_default(),
        );
        assert!(
            std::time::Instant::now() < deadline,
            "child did not reach barrier\nstdout:\n{}\nstderr:\n{}",
            std::fs::read_to_string(&child.stdout_path).unwrap_or_default(),
            std::fs::read_to_string(&child.stderr_path).unwrap_or_default(),
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        child
            .child
            .try_wait()
            .expect("poll child process")
            .is_none()
    );
}

#[must_use]
fn get_log_output(work_dir: &TestWorkDir) -> CommandOutput {
    work_dir.run_jj(["log", "-T", "description"])
}
