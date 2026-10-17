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

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::io::Read as _;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OpStore as _;
use jj_lib::op_store::Operation;
use jj_lib::op_store::OperationId;
use jj_lib::op_store::RootOperationData;
use jj_lib::simple_op_store::SimpleOpStore;
use jj_lib::sync::anchor::AnchorStore;
use jj_lib::sync::shared::SharedEventId;
use jj_lib::sync::state::SyncStateStore;
use pollster::FutureExt as _;
use test_case::test_case;

use crate::common::TestEnvironment;
use crate::common::TestWorkDir;

fn init_repo<'a>(env: &'a TestEnvironment, name: &str) -> TestWorkDir<'a> {
    env.run_jj_in(".", ["git", "init", name]).success();
    env.work_dir(name)
}

fn revision_id(dir: &TestWorkDir<'_>, revision: &str) -> String {
    dir.run_jj([
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
    .raw()
    .trim()
    .to_owned()
}

fn bookmarks(dir: &TestWorkDir<'_>) -> BTreeSet<String> {
    dir.run_jj([
        "--ignore-working-copy",
        "bookmark",
        "list",
        "-T",
        "name ++ \"\\n\"",
    ])
    .success()
    .stdout
    .raw()
    .lines()
    .map(str::to_owned)
    .collect()
}

fn operation(dir: &TestWorkDir<'_>, id: &str) -> Operation {
    let store = SimpleOpStore::load(
        &dir.root().join(".jj/repo/op_store"),
        RootOperationData {
            root_commit_id: CommitId::from_bytes(&[0; 20]),
        },
    );
    store
        .read_operation(&OperationId::try_from_hex(id).unwrap())
        .block_on()
        .unwrap()
}

fn assert_imported_history(source: &TestWorkDir<'_>, receiver: &TestWorkDir<'_>, id: &str) {
    let relative = format!(".jj/repo/op_store/operations/{id}");
    assert_eq!(source.read_file(&relative), receiver.read_file(&relative));
    let view_id = operation(source, id).view_id.hex();
    let relative = format!(".jj/repo/op_store/views/{view_id}");
    assert_eq!(source.read_file(&relative), receiver.read_file(&relative));
}

fn describe_and_bookmark(dir: &TestWorkDir<'_>, name: &str) {
    dir.write_file(format!("{name}.txt"), format!("{name} contents\n"));
    dir.run_jj(["describe", "-m", name]).success();
    dir.run_jj(["bookmark", "create", name]).success();
}

/// Release a paused command even when an intervening assertion fails.
struct SyncBarrier(PathBuf);

impl SyncBarrier {
    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.0.join("ready").exists() {
            assert!(Instant::now() < deadline, "sync did not reach its barrier");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn release(&self) {
        fs::write(self.0.join("release"), b"release\n").unwrap();
    }
}

impl Drop for SyncBarrier {
    fn drop(&mut self) {
        drop(fs::write(self.0.join("release"), b"release\n"));
    }
}

#[test]
fn test_sync_requires_exactly_one_direction() {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    let left_before = left.current_operation_id();
    let right_before = right.current_operation_id();
    let missing = left.run_jj(["sync", "../right"]);
    assert!(!missing.status.success());
    assert!(missing.stderr.raw().contains("--receive"));
    let multiple = left.run_jj(["sync", "--receive", "--send", "../right"]);
    assert!(!multiple.status.success());
    assert_eq!(left.current_operation_id(), left_before);
    assert_eq!(right.current_operation_id(), right_before);
    assert!(!left.root().join(".jj/repo/sync").exists());
    assert!(!right.root().join(".jj/repo/sync").exists());
}

#[test_case("--receive"; "receive")]
#[test_case("--send"; "send")]
#[test_case("--bidi"; "bidirectional")]
fn test_sync_unrelated_local_repositories(direction: &str) {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    describe_and_bookmark(&left, "left");
    describe_and_bookmark(&right, "right");
    let left_wc = revision_id(&left, "@");
    let right_wc = revision_id(&right, "@");
    let left_op = left.current_operation_id();
    let right_op = right.current_operation_id();
    left.run_jj(["sync", direction, "../right"]).success();

    let both = BTreeSet::from(["left".to_owned(), "right".to_owned()]);
    if direction != "--send" {
        assert_eq!(bookmarks(&left), both);
        assert_imported_history(&right, &left, &right_op);
        assert_eq!(
            operation(&left, &left.current_operation_id()).parents.len(),
            1
        );
    } else {
        assert_eq!(bookmarks(&left), BTreeSet::from(["left".to_owned()]));
    }
    if direction != "--receive" {
        assert_eq!(bookmarks(&right), both);
        assert_imported_history(&left, &right, &left_op);
        assert_eq!(
            operation(&right, &right.current_operation_id())
                .parents
                .len(),
            1
        );
    } else {
        assert_eq!(bookmarks(&right), BTreeSet::from(["right".to_owned()]));
    }
    // Both independent repos use the ordinary name "default". Their active @
    // identities and physical worktrees remain local despite imported history.
    assert_eq!(revision_id(&left, "@"), left_wc);
    assert_eq!(revision_id(&right, "@"), right_wc);
    for dir in [&left, &right] {
        assert_eq!(
            dir.run_jj(["--ignore-working-copy", "workspace", "list", "-T", "name"])
                .success()
                .stdout
                .raw(),
            "default",
        );
    }
    assert!(!left.root().join("right.txt").exists());
    assert!(!right.root().join("left.txt").exists());
}

#[test]
fn test_sync_receive_undo_redo() {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    describe_and_bookmark(&left, "left");
    describe_and_bookmark(&right, "right");
    let wc = revision_id(&left, "@");
    left.run_jj(["sync", "--receive", "../right"]).success();
    left.run_jj(["undo"]).success();
    assert_eq!(bookmarks(&left), BTreeSet::from(["left".to_owned()]));
    let undone = left.current_operation_id();
    // Receiving the same acknowledged history does not recreate an undone
    // shared target or insert an operation that breaks ordinary redo.
    left.run_jj(["sync", "--receive", "../right"]).success();
    assert_eq!(left.current_operation_id(), undone);
    left.run_jj(["redo"]).success();
    assert_eq!(
        bookmarks(&left),
        BTreeSet::from(["left".to_owned(), "right".to_owned()])
    );
    assert_eq!(revision_id(&left, "@"), wc);
    assert!(!left.root().join("right.txt").exists());
}

#[test]
fn test_sync_workspace_only_operation_is_retained_without_workspace_changes() {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    left.run_jj(["sync", "--bidi", "../right"]).success();
    let before = left.current_operation_id();
    let before_view = operation(&left, &before).view_id;
    right
        .run_jj(["workspace", "rename", "right-only"])
        .success();
    let renamed = right.current_operation_id();
    left.run_jj(["sync", "--receive", "../right"]).success();
    let integrated = left.current_operation_id();
    assert_ne!(integrated, before);
    assert_eq!(operation(&left, &integrated).view_id, before_view);
    assert_imported_history(&right, &left, &renamed);
    assert_eq!(
        left.run_jj(["--ignore-working-copy", "workspace", "list", "-T", "name"])
            .success()
            .stdout
            .raw(),
        "default",
    );
    left.run_jj(["sync", "--receive", "../right"]).success();
    assert_eq!(left.current_operation_id(), integrated);
    left.run_jj(["undo"]).success();
    assert_eq!(
        operation(&left, &left.current_operation_id()).view_id,
        before_view
    );
    left.run_jj(["redo"]).success();
    assert_eq!(
        operation(&left, &left.current_operation_id()).view_id,
        before_view
    );
}

#[test]
fn test_sync_unrelated_bookmark_conflict_keeps_separate_commit_roots() {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    describe_and_bookmark(&left, "left");
    describe_and_bookmark(&right, "right");
    left.run_jj(["bookmark", "create", "main"]).success();
    right.run_jj(["bookmark", "create", "main"]).success();
    let left_id = revision_id(&left, "@");
    let right_id = revision_id(&right, "@");
    left.run_jj(["sync", "--bidi", "../right"]).success();
    for dir in [&left, &right] {
        let output = dir
            .run_jj([
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                "bookmarks(main)",
                "-T",
                "commit_id ++ \"\\n\"",
            ])
            .success();
        assert_eq!(
            output.stdout.raw().lines().collect::<BTreeSet<_>>(),
            BTreeSet::from([left_id.as_str(), right_id.as_str()])
        );
        assert!(
            dir.run_jj(["--ignore-working-copy", "bookmark", "list", "main"])
                .success()
                .stdout
                .raw()
                .contains("conflicted")
        );
    }
    assert_eq!(revision_id(&left, "@"), left_id);
    assert_eq!(revision_id(&right, "@"), right_id);
}

#[test_case(false; "normal_snapshots")]
#[test_case(true; "ignore_both_working_copies")]
fn test_sync_working_copy_policy(ignore: bool) {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    let left_before = revision_id(&left, "@");
    let right_before = revision_id(&right, "@");
    left.write_file("local-dirty", "left uncommitted edits");
    right.write_file("peer-dirty", "right uncommitted edits");
    let mut args = vec!["sync", "--bidi", "../right"];
    if ignore {
        args.push("--ignore-working-copy");
    }
    left.run_jj(args).success();
    let left_after = revision_id(&left, "@");
    let right_after = revision_id(&right, "@");
    assert_eq!(left_before == left_after, ignore);
    assert_eq!(right_before == right_after, ignore);
    assert_eq!(left.read_file("local-dirty"), "left uncommitted edits");
    assert_eq!(right.read_file("peer-dirty"), "right uncommitted edits");
    assert!(!left.root().join("peer-dirty").exists());
    assert!(!right.root().join("local-dirty").exists());
    let output = left.run_jj([
        "--ignore-working-copy",
        "file",
        "show",
        "-r",
        &right_after,
        "peer-dirty",
    ]);
    if ignore {
        assert!(!output.status.success());
    } else {
        assert_eq!(output.success().stdout.raw(), "right uncommitted edits");
    }
}

#[test]
fn test_sync_endpoint_snapshot_configuration() {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    left.write_file(".jj/repo/config.toml", "snapshot.auto-track = 'none()'\n");
    right.write_file(".jj/repo/config.toml", "snapshot.auto-track = 'all()'\n");
    let left_before = revision_id(&left, "@");
    let right_before = revision_id(&right, "@");
    left.write_file("local-dirty", "not automatically tracked");
    right.write_file("peer-dirty", "tracked by the peer's own settings");
    left.run_jj(["sync", "--bidi", "../right"]).success();
    assert_eq!(revision_id(&left, "@"), left_before);
    assert_ne!(revision_id(&right, "@"), right_before);
    let peer_id = revision_id(&right, "@");
    assert_eq!(
        left.run_jj([
            "--ignore-working-copy",
            "file",
            "show",
            "-r",
            &peer_id,
            "peer-dirty"
        ])
        .success()
        .stdout
        .raw(),
        "tracked by the peer's own settings",
    );
    assert_eq!(left.read_file("local-dirty"), "not automatically tracked");
}

#[test_case("--receive"; "local_receiver")]
#[test_case("--send"; "remote_receiver")]
fn test_sync_receiver_enforces_its_own_quarantine_budget(direction: &str) {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    let (source, receiver) = if direction == "--receive" {
        (&right, &left)
    } else {
        (&left, &right)
    };
    describe_and_bookmark(source, "source");
    source.write_file(
        ".jj/repo/config.toml",
        "sync.max-quarantine-bytes = '100MiB'\n",
    );
    receiver.write_file(".jj/repo/config.toml", "sync.max-quarantine-bytes = 1\n");
    let before = receiver.current_operation_id();
    let output = left.run_jj(["sync", direction, "../right"]);
    assert!(!output.status.success());
    assert!(
        output.stderr.raw().contains("sync.max-quarantine-bytes"),
        "{output}"
    );
    assert_eq!(receiver.current_operation_id(), before);
    assert!(bookmarks(receiver).is_empty());
    // Raising only the receiving repository's budget permits the same retry.
    receiver.write_file(
        ".jj/repo/config.toml",
        "sync.max-quarantine-bytes = '100MiB'\n",
    );
    left.run_jj(["sync", direction, "../right"]).success();
    assert!(bookmarks(receiver).contains("source"));
}

#[test]
fn test_sync_no_change_three_peer_convergence() {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    let third = init_repo(&env, "third");
    for (dir, name) in [(&left, "left"), (&right, "right"), (&third, "third")] {
        describe_and_bookmark(dir, name);
    }
    left.run_jj(["sync", "--bidi", "../right"]).success();
    right.run_jj(["sync", "--bidi", "../third"]).success();
    left.run_jj(["sync", "--bidi", "../third"]).success();
    left.run_jj(["sync", "--bidi", "../right"]).success();
    let expected = BTreeSet::from(["left".to_owned(), "right".to_owned(), "third".to_owned()]);
    for dir in [&left, &right, &third] {
        assert_eq!(bookmarks(dir), expected);
    }
    // The causal shared state converges even though native operation IDs and
    // workspace projections remain different on each machine.
    let before = [
        left.current_operation_id(),
        right.current_operation_id(),
        third.current_operation_id(),
    ];
    left.run_jj(["sync", "--bidi", "../right"]).success();
    right.run_jj(["sync", "--bidi", "../third"]).success();
    left.run_jj(["sync", "--bidi", "../third"]).success();
    assert_eq!(
        [
            left.current_operation_id(),
            right.current_operation_id(),
            third.current_operation_id()
        ],
        before
    );
    assert_ne!(before[0], before[1]);
}

#[test]
fn test_sync_independent_clones_preserve_remote_observations() {
    let env = TestEnvironment::default();
    let origin = testutils::git::init_bare(env.env_root().join("origin"));
    let first = testutils::git::add_commit(
        &origin,
        "refs/heads/main",
        "base",
        b"base contents\n",
        "base",
        &[],
    )
    .commit_id;
    testutils::git::set_symbolic_reference(&origin, "HEAD", "refs/heads/main");
    for name in ["left", "right"] {
        env.run_jj_in(".", ["git", "clone", "origin", name])
            .success();
    }
    let left = env.work_dir("left");
    let right = env.work_dir("right");
    describe_and_bookmark(&left, "left");
    describe_and_bookmark(&right, "right");
    let next = testutils::git::add_commit(
        &origin,
        "refs/heads/main",
        "base",
        b"new upstream contents\n",
        "upstream change",
        &[first],
    )
    .commit_id;
    right.run_jj(["git", "fetch"]).success();
    let left_wc = revision_id(&left, "@");
    let right_wc = revision_id(&right, "@");
    left.run_jj(["sync", "--bidi", "../right"]).success();
    assert_eq!(revision_id(&left, "main@origin"), next.to_string());
    assert_eq!(revision_id(&right, "main@origin"), next.to_string());
    // The left clone's physical remote-tracking ref is still its old local
    // observation. Reimporting that unchanged ref must not reset shared state.
    left.run_jj(["git", "import"]).success();
    assert_eq!(revision_id(&left, "main@origin"), next.to_string());
    assert_eq!(revision_id(&left, "@"), left_wc);
    assert_eq!(revision_id(&right, "@"), right_wc);
    assert!(bookmarks(&left).contains("right"));
    assert!(bookmarks(&right).contains("left"));
}

#[test]
fn test_sync_snapshots_only_selected_workspace() {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    right
        .run_jj(["workspace", "add", "--name", "other", "../right-other"])
        .success();
    let other = env.work_dir("right-other");
    let other_before = revision_id(&other, "@");
    other.write_file("unsnapshotted", "edits in an unselected workspace");
    right.write_file("selected", "edits in the selected workspace");
    left.run_jj(["sync", "--bidi", "../right"]).success();
    assert_eq!(revision_id(&other, "@"), other_before);
    assert_eq!(
        other.read_file("unsnapshotted"),
        "edits in an unselected workspace"
    );
    assert_eq!(
        left.run_jj(["--ignore-working-copy", "workspace", "list", "-T", "name"])
            .success()
            .stdout
            .raw(),
        "default",
    );
    let selected = revision_id(&right, "@");
    assert_eq!(
        left.run_jj([
            "--ignore-working-copy",
            "file",
            "show",
            "-r",
            &selected,
            "selected"
        ])
        .success()
        .stdout
        .raw(),
        "edits in the selected workspace",
    );
}

#[test]
fn test_sync_retained_peer_history_survives_operation_abandon_and_gc() {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    describe_and_bookmark(&left, "left");
    describe_and_bookmark(&right, "right");
    let original = left.current_operation_id();
    let commit = revision_id(&left, "@");
    right.run_jj(["sync", "--receive", "../left"]).success();
    right.run_jj(["op", "abandon", "..@-"]).success();
    right.run_jj(["util", "gc", "--expire", "now"]).success();
    assert_imported_history(&left, &right, &original);
    assert_eq!(
        right
            .run_jj([
                "--at-op",
                &original,
                "log",
                "--no-graph",
                "-r",
                &commit,
                "-T",
                "commit_id"
            ])
            .success()
            .stdout
            .raw(),
        commit,
    );
    right.run_jj(["sync", "--receive", "../left"]).success();
    assert!(bookmarks(&right).contains("left"));
}

#[test_case("--receive", true; "receive_before_publication")]
#[test_case("--receive", false; "receive_after_publication")]
#[test_case("--send", true; "send_before_publication")]
#[test_case("--send", false; "send_after_publication")]
fn test_sync_publication_failure_retry(direction: &str, before_publication: bool) {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    describe_and_bookmark(&left, "left");
    describe_and_bookmark(&right, "right");
    left.run_jj(["sync", "--bidi", "../right"]).success();
    let (source, receiver) = if direction == "--receive" {
        (&right, &left)
    } else {
        (&left, &right)
    };
    source
        .run_jj(["new", "-m", "work after synchronization"])
        .success();
    source.run_jj(["bookmark", "create", "after"]).success();
    let source_op = source.current_operation_id();
    let receiver_before = receiver.current_operation_id();
    let wc_before = revision_id(receiver, "@");
    let hook = if before_publication {
        "JJ_TEST_SYNC_FAIL_BEFORE_PUBLISH"
    } else {
        "JJ_TEST_FAIL_AFTER_OPERATION_PUBLISH"
    };
    let failure =
        left.run_jj_with(|command| command.args(["sync", direction, "../right"]).env(hook, "1"));
    assert!(!failure.status.success(), "{failure}");
    assert!(
        failure.stderr.raw().contains("Injected failure"),
        "{failure}"
    );
    assert_eq!(
        receiver.current_operation_id() == receiver_before,
        before_publication
    );
    // Even an unpublished exact operation saved to the recovery WAL must keep
    // its raw dependencies alive if GC runs before the retry.
    receiver.run_jj(["util", "gc", "--expire", "now"]).success();
    assert_imported_history(source, receiver, &source_op);
    left.run_jj(["sync", direction, "../right"]).success();
    assert!(bookmarks(receiver).contains("after"));
    assert_eq!(revision_id(receiver, "@"), wc_before);
    let finished = receiver.current_operation_id();
    left.run_jj(["sync", direction, "../right"]).success();
    assert_eq!(receiver.current_operation_id(), finished);
}

#[test_case(false; "concurrent_edits_and_gc")]
#[test_case(true; "crossed_sessions_with_concurrent_edits_and_gc")]
fn test_sync_preserves_changes_after_capture(crossed_session: bool) {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    describe_and_bookmark(&left, "left");
    describe_and_bookmark(&right, "right");
    let left_captured = left.current_operation_id();
    let right_captured = right.current_operation_id();
    let pause = env.env_root().join("after-capture");
    fs::create_dir(&pause).unwrap();
    let mut command = env.new_jj_cmd();
    command
        .current_dir(left.root())
        .args(["sync", "--bidi", "../right"])
        .env("JJ_TEST_SYNC_AFTER_CAPTURE_PAUSE_DIR", &pause)
        .timeout(Duration::from_secs(40));

    std::thread::scope(|scope| {
        let barrier = SyncBarrier(pause);
        let first = scope.spawn(move || command.output().unwrap());
        barrier.wait_ready();

        // Rewrite the captured @ commits and advance both native histories
        // while the first transfer is pinned but holds no repository locks.
        describe_and_bookmark(&left, "late-left");
        describe_and_bookmark(&right, "late-right");
        for dir in [&left, &right] {
            dir.run_jj(["op", "abandon", "..@-"]).success();
            dir.run_jj_with(|cmd| {
                cmd.args(["util", "gc", "--expire", "now"])
                    .timeout(Duration::from_secs(10))
            })
            .success();
        }
        // These edits have no operation yet. Finalization must refresh the
        // selected workspaces before applying the frozen incoming history.
        left.write_file("during-transfer", "late filesystem edit on left\n");
        right.write_file("during-transfer", "late filesystem edit on right\n");
        if crossed_session {
            right
                .run_jj_with(|cmd| {
                    cmd.args(["sync", "--bidi", "../left"])
                        .timeout(Duration::from_secs(20))
                })
                .success();
        }
        barrier.release();
        let output = first.join().unwrap();
        assert!(
            output.status.success(),
            "paused sync failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    });

    for (dir, own, text) in [
        (&left, "late-left", "late filesystem edit on left\n"),
        (&right, "late-right", "late filesystem edit on right\n"),
    ] {
        assert!(bookmarks(dir).is_superset(&BTreeSet::from([
            "left".to_owned(),
            "right".to_owned(),
            own.to_owned()
        ])));
        assert_eq!(dir.read_file("during-transfer"), text);
        assert_eq!(
            dir.run_jj([
                "--ignore-working-copy",
                "file",
                "show",
                "-r",
                "@",
                "during-transfer"
            ])
            .success()
            .stdout
            .raw(),
            text,
        );
    }
    assert_imported_history(&left, &right, &left_captured);
    assert_imported_history(&right, &left, &right_captured);
    left.run_jj(["sync", "--bidi", "../right"]).success();
    let expected = BTreeSet::from([
        "left".to_owned(),
        "right".to_owned(),
        "late-left".to_owned(),
        "late-right".to_owned(),
    ]);
    assert_eq!(bookmarks(&left), expected);
    assert_eq!(bookmarks(&right), expected);
    let before = [left.current_operation_id(), right.current_operation_id()];
    right.run_jj(["sync", "--bidi", "../left"]).success();
    assert_eq!(
        [left.current_operation_id(), right.current_operation_id()],
        before
    );
}

#[test]
fn test_sync_rewritten_ancestor_checkout_recovers_after_publication() {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    right.write_file("base", "old base contents\n");
    right.run_jj(["describe", "-m", "base"]).success();
    right.run_jj(["bookmark", "create", "base"]).success();
    left.run_jj(["sync", "--bidi", "../right"]).success();
    left.run_jj(["new", "base", "-m", "local child"]).success();
    left.write_file("child", "local child contents\n");
    left.run_jj(["describe", "-m", "local child"]).success();
    let old_wc = revision_id(&left, "@");
    let old_op = left.current_operation_id();
    right.write_file("base", "rewritten base contents\n");
    right.run_jj(["describe", "-m", "rewritten base"]).success();
    let rewritten = revision_id(&right, "@");

    let output = left.run_jj_with(|cmd| {
        cmd.args(["sync", "--receive", "../right"])
            .env("JJ_TEST_FAIL_AFTER_OPERATION_PUBLISH", "1")
    });
    assert!(!output.status.success());
    assert!(
        output
            .stderr
            .raw()
            .contains("Injected failure after operation publication"),
        "{output}"
    );
    let published_op = left.current_operation_id();
    assert_ne!(published_op, old_op);
    // Publication has committed the ancestor rewrite and descendant rebase,
    // while the worktree still needs the journaled checkout.
    let rebased = revision_id(&left, "@");
    assert_ne!(rebased, old_wc);
    assert_eq!(revision_id(&left, "@-"), rewritten);
    assert_eq!(left.read_file("base"), "old base contents\n");
    assert_eq!(left.read_file("child"), "local child contents\n");
    let pending_checkout = left.root().join(".jj/working_copy/pending_checkout");
    assert!(pending_checkout.exists());
    left.write_file("child", "user edits after interruption\n");

    left.run_jj(["sync", "--receive", "../right"]).success();
    // Normal pending-checkout recovery snapshots physical files as edits
    // against the intended tree, including writes made after the failure.
    assert_eq!(left.read_file("base"), "old base contents\n");
    assert_eq!(left.read_file("child"), "user edits after interruption\n");
    assert!(!pending_checkout.exists());
    assert_ne!(revision_id(&left, "@"), rebased);
    assert_eq!(revision_id(&left, "@-"), rewritten);
    for (path, current, published) in [
        ("base", "old base contents\n", "rewritten base contents\n"),
        (
            "child",
            "user edits after interruption\n",
            "local child contents\n",
        ),
    ] {
        assert_eq!(
            left.run_jj(["--ignore-working-copy", "file", "show", "-r", "@", path])
                .success()
                .stdout
                .raw(),
            current,
        );
        assert_eq!(
            left.run_jj([
                "--ignore-working-copy",
                "--at-op",
                &published_op,
                "file",
                "show",
                "-r",
                &rebased,
                path,
            ])
            .success()
            .stdout
            .raw(),
            published,
        );
    }
    let finished = left.current_operation_id();
    left.run_jj(["sync", "--receive", "../right"]).success();
    assert_eq!(left.current_operation_id(), finished);
}

#[test_case(false; "one_head")]
#[test_case(true; "divergent_heads")]
fn test_sync_undo_survives_pruning_interrupted_publication(divergent_heads: bool) {
    let env = TestEnvironment::default();
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    describe_and_bookmark(&right, "peer-work");
    let source_op = right.current_operation_id();
    let output = left.run_jj_with(|cmd| {
        cmd.args(["--ignore-working-copy", "sync", "--receive", "../right"])
            .env("JJ_TEST_FAIL_AFTER_OPERATION_PUBLISH", "1")
    });
    assert!(!output.status.success());
    assert!(
        output
            .stderr
            .raw()
            .contains("Injected failure after operation publication"),
        "{output}"
    );
    let published = left.current_operation_id();
    assert!(bookmarks(&left).contains("peer-work"));
    left.run_jj(["--ignore-working-copy", "undo"]).success();
    assert!(bookmarks(&left).is_empty());
    if divergent_heads {
        // Create another ordinary undo from the same integration. Abandon
        // must preserve causal knowledge from every actual native head.
        left.run_jj(["--ignore-working-copy", "--at-op", &published, "undo"])
            .success();
        assert_eq!(
            fs::read_dir(left.root().join(".jj/repo/op_heads/heads"))
                .unwrap()
                .count(),
            2
        );
    }
    left.run_jj(["--ignore-working-copy", "op", "abandon", &published])
        .success();
    left.run_jj(["--ignore-working-copy", "util", "gc", "--expire", "now"])
        .success();
    assert_imported_history(&right, &left, &source_op);

    left.run_jj(["--ignore-working-copy", "sync", "--receive", "../right"])
        .success();
    assert!(bookmarks(&left).is_empty());
    let left_repo = left.root().join(".jj/repo");
    let right_repo = right.root().join(".jj/repo");
    let received = SyncStateStore::load(&right_repo)
        .read()
        .unwrap()
        .local_anchor
        .unwrap()
        .shared_anchor;
    let current = SyncStateStore::load(&left_repo)
        .read()
        .unwrap()
        .local_anchor
        .unwrap()
        .shared_anchor;
    assert!(
        AnchorStore::load(&left_repo)
            .is_ancestor(
                &SharedEventId::try_from_hex(&received).unwrap(),
                &SharedEventId::try_from_hex(&current).unwrap(),
            )
            .unwrap()
    );
    let finished = left.current_operation_id();
    left.run_jj(["--ignore-working-copy", "util", "gc", "--expire", "now"])
        .success();
    left.run_jj(["--ignore-working-copy", "sync", "--receive", "../right"])
        .success();
    assert_eq!(left.current_operation_id(), finished);
    assert!(bookmarks(&left).is_empty());
    assert_imported_history(&right, &left, &source_op);
}

#[cfg(unix)]
#[test]
fn test_sync_ssh_paths_are_protocol_data() {
    use std::os::unix::fs::PermissionsExt as _;
    let mut env = TestEnvironment::default();
    let bin = env.env_root().join("fake-bin");
    fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    fs::write(&ssh, b"#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$JJ_SYNC_SSH_ARGUMENTS\"\nexec \"$JJ_SYNC_REAL_JJ\" util sync-server\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let arguments = env.env_root().join("ssh-arguments");
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    env.add_env_var("PATH", path);
    env.add_env_var(
        "JJ_SYNC_REAL_JJ",
        TestEnvironment::jj_executable().into_os_string(),
    );
    env.add_env_var("JJ_SYNC_SSH_ARGUMENTS", arguments.as_os_str());
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "peer ' ; $(touch escaped)");
    describe_and_bookmark(&right, "right");
    let encoded = percent_encoding::utf8_percent_encode(
        right.root().to_str().unwrap(),
        percent_encoding::NON_ALPHANUMERIC,
    )
    .to_string()
    .replace("%2F", "/");
    let url = format!("ssh://user@example.com:2222{encoded}");
    left.run_jj(["sync", "--receive", &url]).success();
    assert!(bookmarks(&left).contains("right"));
    assert_eq!(
        fs::read_to_string(arguments)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "-T",
            "-p",
            "2222",
            "--",
            "user@example.com",
            "jj util sync-server"
        ]
    );
    assert!(!left.root().join("escaped").exists());
}

#[test]
fn test_sync_streams_large_packed_git_blob() {
    let env = TestEnvironment::default();
    env.add_config("snapshot.max-new-file-size = '100MiB'\n");
    let source = init_repo(&env, "source");
    let receiver = init_repo(&env, "receiver");
    let size = 65 * 1024 * 1024u64;
    let mut file = fs::File::create(source.root().join("large.bin")).unwrap();
    io::copy(&mut io::repeat(42).take(size), &mut file).unwrap();
    drop(file);
    source.run_jj(["describe", "-m", "large file"]).success();
    source.run_jj(["bookmark", "create", "large"]).success();
    source.run_jj(["util", "gc", "--expire", "now"]).success();
    let commit = revision_id(&source, "@");
    receiver
        .run_jj(["sync", "--receive", "../source"])
        .success();
    assert_eq!(revision_id(&receiver, "large"), commit);
    let git = gix::open(receiver.root().join(".jj/repo/store/git")).unwrap();
    let tree = git
        .find_commit(gix::ObjectId::from_hex(commit.as_bytes()).unwrap())
        .unwrap()
        .tree()
        .unwrap();
    let blob = tree
        .iter()
        .map(Result::unwrap)
        .find(|entry| entry.filename() == b"large.bin")
        .unwrap()
        .oid()
        .to_owned();
    assert_eq!(git.try_find_header(blob).unwrap().unwrap().size(), size);
    assert!(!receiver.root().join("large.bin").exists());
    let done = receiver.current_operation_id();
    receiver
        .run_jj(["sync", "--receive", "../source"])
        .success();
    assert_eq!(receiver.current_operation_id(), done);
}
