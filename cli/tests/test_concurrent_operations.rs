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
    if let Some((env_name, pause_dir)) = pause {
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
