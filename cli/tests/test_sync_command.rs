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
use jj_lib::sync::protocol::Direction;
use jj_lib::sync::protocol::Hello;
use jj_lib::sync::protocol::Message;
use jj_lib::sync::protocol::PROTOCOL_VERSION;
use jj_lib::sync::protocol::Session;
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

#[test_case("--send"; "send")]
#[test_case("--bidi"; "bidirectional")]
fn test_sync_initializes_missing_peer_at_exact_path(direction: &str) {
    let env = TestEnvironment::default();
    let source = init_repo(&env, "source");
    describe_and_bookmark(&source, "source");
    let source_wc = revision_id(&source, "@");
    let source_op = source.current_operation_id();
    // The destination's parent is itself a repository. Loading an ancestor
    // instead of initializing this exact path would silently sync with source.
    let receiver = source.dir("new-peer");
    source
        .run_jj(["--ignore-working-copy", "sync", direction, "new-peer"])
        .success();
    // TestEnvironment sets git.colocate=false; destination creation overrides it.
    assert!(receiver.root().join(".git").is_dir());
    assert!(receiver.root().join(".jj").is_dir());
    assert_eq!(bookmarks(&receiver), BTreeSet::from(["source".to_owned()]));
    assert_imported_history(&source, &receiver, &source_op);
    assert_eq!(revision_id(&source, "@"), source_wc);
    assert_ne!(revision_id(&receiver, "@"), source_wc);
    assert!(!receiver.root().join("source.txt").exists());
    assert_eq!(
        receiver
            .run_jj([
                "--ignore-working-copy",
                "file",
                "show",
                "-r",
                "source",
                "source.txt",
            ])
            .success()
            .stdout
            .raw(),
        "source contents\n",
    );

    let received = receiver.current_operation_id();
    source
        .run_jj(["--ignore-working-copy", "sync", direction, "new-peer"])
        .success();
    assert_eq!(receiver.current_operation_id(), received);
    receiver.run_jj(["undo"]).success();
    assert!(bookmarks(&receiver).is_empty());
    receiver.run_jj(["redo"]).success();
    assert_eq!(bookmarks(&receiver), BTreeSet::from(["source".to_owned()]));
}

#[test_case("--receive", false; "receive_explicit_missing_repository")]
#[test_case("--receive", true; "receive_empty_current_directory")]
#[test_case("--bidi", false; "bidirectional_explicit_missing_repository")]
fn test_sync_initializes_missing_local_receiver(direction: &str, use_cwd: bool) {
    let env = TestEnvironment::default();
    let source = init_repo(&env, "source");
    describe_and_bookmark(&source, "source");
    let source_op = source.current_operation_id();
    let receiver = env.work_dir("receiver");
    if use_cwd {
        fs::create_dir(receiver.root()).unwrap();
        receiver.run_jj(["sync", direction, "../source"]).success();
    } else {
        env.run_jj_in(".", ["-R", "receiver", "sync", direction, "source"])
            .success();
    }
    assert!(receiver.root().join(".git").is_dir());
    assert_eq!(bookmarks(&receiver), BTreeSet::from(["source".to_owned()]));
    assert_imported_history(&source, &receiver, &source_op);
    assert!(!receiver.root().join("source.txt").exists());
}

#[test_case(false; "snapshot_existing_files")]
#[test_case(true; "leave_existing_files_unsnapshotted")]
fn test_sync_new_receiver_preserves_existing_files(ignore: bool) {
    let env = TestEnvironment::default();
    let source = init_repo(&env, "source");
    describe_and_bookmark(&source, "source");
    let receiver = env.work_dir("receiver");
    receiver.write_file("existing.txt", "local destination contents\n");
    let mut args = vec!["sync", "--receive", "../source"];
    if ignore {
        args.push("--ignore-working-copy");
    }
    receiver.run_jj(args).success();
    assert!(receiver.root().join(".git").is_dir());
    assert_eq!(
        receiver.read_file("existing.txt"),
        "local destination contents\n"
    );
    assert!(!receiver.root().join("source.txt").exists());
    let contents = receiver.run_jj([
        "--ignore-working-copy",
        "file",
        "show",
        "-r",
        "@",
        "existing.txt",
    ]);
    if ignore {
        assert!(!contents.status.success());
    } else {
        assert_eq!(
            contents.success().stdout.raw(),
            "local destination contents\n"
        );
    }
    assert_eq!(bookmarks(&receiver), BTreeSet::from(["source".to_owned()]));
}

#[test]
fn test_sync_new_receiver_matches_source_object_hash() {
    let env = TestEnvironment::default();
    env.run_jj_in(".", ["git", "init", "--object-hash", "sha256", "source"])
        .success();
    let source = env.work_dir("source");
    describe_and_bookmark(&source, "source");
    // The receiver must inherit the source's format rather than its default SHA-1.
    env.run_jj_in(".", ["-R", "receiver", "sync", "--receive", "source"])
        .success();
    let receiver = env.work_dir("receiver");
    assert_eq!(revision_id(&source, "source").len(), 64);
    assert_eq!(
        revision_id(&receiver, "source"),
        revision_id(&source, "source")
    );
    assert_eq!(revision_id(&receiver, "@").len(), 64);
    assert!(receiver.root().join(".git").is_dir());
}

#[test]
fn test_sync_initializes_destination_with_existing_git_repository() {
    let env = TestEnvironment::default();
    let source = init_repo(&env, "source");
    describe_and_bookmark(&source, "source");
    let receiver = env.work_dir("receiver");
    let git_repo = testutils::git::init(receiver.root());
    let config_path = git_repo.path().join("config");
    let existing_config = fs::read(&config_path).unwrap();
    source.run_jj(["sync", "--send", "../receiver"]).success();
    assert_eq!(fs::read(&config_path).unwrap(), existing_config);
    assert_eq!(bookmarks(&receiver), BTreeSet::from(["source".to_owned()]));
    assert!(receiver.root().join(".git").is_dir());
    assert!(!receiver.root().join("source.txt").exists());
}

#[test]
fn test_sync_does_not_initialize_existing_git_with_incompatible_object_hash() {
    let env = TestEnvironment::default();
    env.run_jj_in(".", ["git", "init", "--object-hash", "sha256", "source"])
        .success();
    let source = env.work_dir("source");
    describe_and_bookmark(&source, "source");
    let receiver = env.work_dir("receiver");
    let git_repo = testutils::git::init(receiver.root());
    let config_path = git_repo.path().join("config");
    let existing_config = fs::read(&config_path).unwrap();
    let output = source.run_jj(["sync", "--send", "../receiver"]);
    assert!(!output.status.success());
    assert!(output.stderr.raw().contains("object hash"));
    assert!(!receiver.root().join(".jj").exists());
    assert_eq!(fs::read(config_path).unwrap(), existing_config);
}

#[test_case("version")]
#[test_case("backend")]
#[test_case("operation_store")]
#[test_case("object_hash")]
#[test_case("required_capability")]
#[test_case("missing_capability")]
fn test_sync_helper_rejects_incompatible_request_before_initialization(field: &str) {
    let env = TestEnvironment::default();
    let receiver = env.env_root().join("receiver");
    let capabilities: Vec<_> = [
        "raw-history-v1",
        "shared-causal-v1",
        "streaming-objects-v1",
        "local-workspaces-v1",
    ]
    .map(str::to_owned)
    .into();
    let mut request = Hello {
        version: PROTOCOL_VERSION,
        direction: Direction::Send,
        repository_path: receiver.as_os_str().as_encoded_bytes().to_vec(),
        ignore_working_copy: false,
        backend: "git".to_owned(),
        object_hash: "sha1".to_owned(),
        operation_store: "simple-op-v1".to_owned(),
        capabilities: capabilities.clone(),
        required_capabilities: capabilities,
    };
    let diagnostic = match field {
        "version" => "protocol version",
        "backend" => {
            request.backend = "unknown-backend".to_owned();
            "backend"
        }
        "operation_store" => {
            request.operation_store = "unknown-store".to_owned();
            "operation store format"
        }
        "object_hash" => {
            request.object_hash = "sha512".to_owned();
            "object hash"
        }
        "required_capability" => {
            request.capabilities.push("unknown-required".to_owned());
            request
                .required_capabilities
                .push("unknown-required".to_owned());
            "capability"
        }
        "missing_capability" => {
            request.capabilities.pop();
            request.required_capabilities.pop();
            "capability"
        }
        _ => unreachable!(),
    };
    let mut session = Session::new(io::empty(), Vec::new());
    session.send(&Message::Hello(request)).unwrap();
    let mut input = session.into_inner().1;
    if field == "version" {
        // The sender validates versions too. Alter an otherwise valid wire frame
        // to exercise rejection by the receiving process, with a valid checksum.
        const HEADER_LEN: usize = 8 + 1 + 8 + 32;
        input[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&(PROTOCOL_VERSION + 1).to_be_bytes());
        let mut digest = gix::hash::hasher(gix::hash::Kind::Sha256);
        digest.update(&input[8..17]);
        digest.update(&input[HEADER_LEN..]);
        input[17..HEADER_LEN].copy_from_slice(digest.try_finalize().unwrap().as_bytes());
    }
    let output = env
        .new_jj_cmd()
        .args(["util", "sync-server"])
        .write_stdin(input)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let mut response = Session::new(io::Cursor::new(output.stdout), io::sink());
    let Message::Error(message) = response.receive().unwrap() else {
        panic!("helper accepted incompatible {field}");
    };
    assert!(message.contains(diagnostic), "{message}");
    assert!(!receiver.exists());
}

#[test]
fn test_sync_does_not_initialize_missing_source() {
    let env = TestEnvironment::default();
    let receiver = init_repo(&env, "receiver");
    let missing_peer = env.env_root().join("missing-peer");
    assert!(
        !receiver
            .run_jj(["sync", "--receive", "../missing-peer"])
            .status
            .success()
    );
    assert!(!missing_peer.exists());
    assert!(
        !env.run_jj_in(
            ".",
            ["-R", "missing-source", "sync", "--send", "missing-peer"],
        )
        .status
        .success()
    );
    assert!(!env.env_root().join("missing-source").exists());
    assert!(!missing_peer.exists());
    assert!(
        !env.run_jj_in(
            ".",
            ["-R", "missing-local", "sync", "--bidi", "missing-peer"],
        )
        .status
        .success()
    );
    assert!(!env.env_root().join("missing-local").exists());
    assert!(!missing_peer.exists());
    // Initialization follows `jj git init`: the destination's parent must exist.
    assert!(
        !receiver
            .run_jj(["sync", "--send", "../missing-parent/peer"])
            .status
            .success()
    );
    assert!(!env.env_root().join("missing-parent").exists());
}

#[test_case(false; "jj_file")]
#[test_case(true; "malformed_jj_directory")]
fn test_sync_does_not_overwrite_malformed_destination(jj_directory: bool) {
    let env = TestEnvironment::default();
    let source = init_repo(&env, "source");
    let receiver = env.work_dir("receiver");
    let marker = if jj_directory { ".jj/sentinel" } else { ".jj" };
    receiver.write_file(marker, "existing invalid metadata\n");
    let output = if jj_directory {
        env.run_jj_in(".", ["-R", "receiver", "sync", "--receive", "source"])
    } else {
        source.run_jj(["sync", "--send", "../receiver"])
    };
    assert!(!output.status.success());
    assert_eq!(receiver.read_file(marker), "existing invalid metadata\n");
    assert!(!receiver.root().join(".git").exists());
    assert!(!receiver.root().join(".jj/repo").exists());
}

#[cfg(unix)]
#[test_case(".jj")]
#[test_case(".git")]
fn test_sync_does_not_replace_dangling_destination_marker(marker: &str) {
    use std::os::unix::fs::symlink;

    let env = TestEnvironment::default();
    let source = init_repo(&env, "source");
    let receiver = env.work_dir("receiver");
    fs::create_dir(receiver.root()).unwrap();
    let target = env.env_root().join("missing-marker-target");
    let marker_path = receiver.root().join(marker);
    symlink(&target, &marker_path).unwrap();
    let output = source.run_jj(["sync", "--send", "../receiver"]);
    assert!(!output.status.success());
    assert_eq!(fs::read_link(marker_path).unwrap(), target);
    assert!(!target.exists());
    assert!(!receiver.root().join(".jj/repo").exists());
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

#[cfg(unix)]
#[test_case("--send"; "remote_native_receiver")]
#[test_case("--receive"; "local_native_receiver")]
fn test_sync_native_git_budget_failure_cleans_up_and_allows_retry(direction: &str) {
    let env = TestEnvironment::default();
    env.add_config("snapshot.max-new-file-size = '25MiB'\n");
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    let (source, receiver) = if direction == "--receive" {
        (&right, &left)
    } else {
        (&left, &right)
    };
    describe_and_bookmark(receiver, "receiver");
    // The compressed pack must exceed both the receiver's 1 MiB storage
    // budget and the extra 16 MiB allowed for native protocol overhead.
    let mut seed = 42u64;
    let contents: Vec<u8> = (0..20 * 1024 * 1024)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 32) as u8
        })
        .collect();
    fs::write(source.root().join("budget.bin"), &contents).unwrap();
    describe_and_bookmark(source, "source");
    let source_commit = revision_id(source, "source");
    let source_op = source.current_operation_id();
    let source_git = gix::open(source.root().join(".jj/repo/store/git")).unwrap();
    let tree = source_git
        .find_commit(gix::ObjectId::from_hex(source_commit.as_bytes()).unwrap())
        .unwrap()
        .tree()
        .unwrap();
    let blob = tree
        .iter()
        .map(Result::unwrap)
        .find(|entry| entry.filename() == b"budget.bin")
        .unwrap()
        .oid()
        .to_owned();
    receiver.write_file(
        ".jj/repo/config.toml",
        "sync.max-quarantine-bytes = '1MiB'\n",
    );
    let before = receiver.current_operation_id();
    let before_view = operation(receiver, &before).view_id;
    let git_refs = || {
        let git = gix::open(receiver.root().join(".jj/repo/store/git")).unwrap();
        let refs: BTreeSet<_> = git
            .references()
            .unwrap()
            .all()
            .unwrap()
            .map(Result::unwrap)
            .map(|reference| {
                (
                    reference.name().as_bstr().to_string(),
                    format!("{:?}", reference.target()),
                )
            })
            .collect();
        refs
    };
    let before_refs = git_refs();
    let started = Instant::now();
    let output = left.run_jj_with(|command| {
        command
            .args(["--debug", "sync", direction, "../right"])
            .timeout(Duration::from_secs(20))
    });
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(!output.status.success());
    let stderr = anstream::adapter::strip_str(output.stderr.raw()).to_string();
    assert!(
        stderr.contains("Transferring Git objects with native Git"),
        "{stderr}"
    );
    assert!(stderr.contains("sync.max-quarantine-bytes"), "{stderr}");
    assert_eq!(receiver.current_operation_id(), before);
    assert_eq!(operation(receiver, &before).view_id, before_view);
    assert_eq!(bookmarks(receiver), BTreeSet::from(["receiver".to_owned()]));
    assert_eq!(git_refs(), before_refs);
    assert!(
        !receiver
            .root()
            .join(format!(".jj/repo/op_store/operations/{source_op}"))
            .exists()
    );
    let receiver_git = gix::open(receiver.root().join(".jj/repo/store/git")).unwrap();
    assert!(receiver_git.try_find_header(blob).unwrap().is_none());

    // The failed quarantine must release its processes, locks, and temporary
    // storage so the same transfer can succeed with a sufficient local budget.
    receiver.write_file(
        ".jj/repo/config.toml",
        "sync.max-quarantine-bytes = '100MiB'\n",
    );
    left.run_jj_with(|command| {
        command
            .args(["sync", direction, "../right"])
            .timeout(Duration::from_secs(20))
    })
    .success();
    assert_eq!(revision_id(receiver, "source"), source_commit);
    assert_imported_history(source, receiver, &source_op);
    let receiver_git = gix::open(receiver.root().join(".jj/repo/store/git")).unwrap();
    assert_eq!(
        receiver_git.try_find_header(blob).unwrap().unwrap().size(),
        contents.len() as u64
    );
    assert!(!receiver.root().join("budget.bin").exists());
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
    // while the worktree still records the previous operation and tree.
    let rebased = revision_id(&left, "@");
    assert_ne!(rebased, old_wc);
    assert_eq!(revision_id(&left, "@-"), rewritten);
    assert_eq!(left.read_file("base"), "old base contents\n");
    assert_eq!(left.read_file("child"), "local child contents\n");
    left.write_file("child", "user edits after interruption\n");

    // The default stale-workspace policy requires explicit recovery. A failed
    // retry must leave the published operation and physical edits intact.
    let output = left.run_jj(["sync", "--receive", "../right"]);
    assert!(!output.status.success());
    assert!(
        output.stderr.raw().contains("working copy is stale"),
        "{output}"
    );
    assert_eq!(left.current_operation_id(), published_op);
    assert_eq!(left.read_file("base"), "old base contents\n");
    assert_eq!(left.read_file("child"), "user edits after interruption\n");

    left.run_jj(["workspace", "update-stale"]).success();
    left.run_jj(["status"]).success();
    assert_eq!(left.read_file("base"), "rewritten base contents\n");
    assert_eq!(revision_id(&left, "@-"), rewritten);

    // Native stale recovery saves edits against the old workspace operation
    // before reconciling it with the published rewrite. The saved edit may be
    // a divergent change rather than the commit selected as the new @.
    let operations = left
        .run_jj([
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "-T",
            "id ++ \"\\n\"",
        ])
        .success();
    let old_op_id = OperationId::try_from_hex(&old_op).unwrap();
    let snapshots = operations
        .stdout
        .raw()
        .lines()
        .filter(|id| {
            let op = operation(&left, id);
            op.parents == [old_op_id.clone()] && op.metadata.description == "snapshot working copy"
        })
        .collect::<Vec<_>>();
    assert_eq!(snapshots.len(), 1);
    for (path, saved, published) in [
        ("base", "old base contents\n", "rewritten base contents\n"),
        (
            "child",
            "user edits after interruption\n",
            "local child contents\n",
        ),
    ] {
        assert_eq!(
            left.run_jj([
                "--ignore-working-copy",
                "--at-op",
                snapshots[0],
                "file",
                "show",
                "-r",
                "@",
                path,
            ])
            .success()
            .stdout
            .raw(),
            saved,
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
    left.run_jj(["sync", "--receive", "../right"]).success();
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
fn configure_fake_sync_ssh(env: &mut TestEnvironment) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = env.env_root().join("fake-bin");
    fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    fs::write(
        &ssh,
        indoc::indoc! {r#"
            #!/bin/sh
            printf '%s\n' "$@" > "$JJ_SYNC_SSH_ARGUMENTS"
            if [ -n "$JJ_SYNC_SSH_DIAGNOSTIC" ]; then
                printf '%s\n' "$JJ_SYNC_SSH_DIAGNOSTIC" >&2
            fi
            while [ "$#" -gt 1 ]; do shift; done
            case "$1" in
                'jj util sync-server') exec "$JJ_SYNC_REAL_JJ" util sync-server ;;
                'jj --debug util sync-server') exec "$JJ_SYNC_REAL_JJ" --debug util sync-server ;;
                'jj --quiet util sync-server') exec "$JJ_SYNC_REAL_JJ" --quiet util sync-server ;;
                'jj --debug --quiet util sync-server') exec "$JJ_SYNC_REAL_JJ" --debug --quiet util sync-server ;;
                *) exit 1 ;;
            esac
        "#},
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let arguments = env.env_root().join("ssh-arguments");
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    env.add_env_var("PATH", path);
    env.add_env_var(
        "JJ_SYNC_REAL_JJ",
        assert_cmd::cargo::cargo_bin!("jj").as_os_str(),
    );
    env.add_env_var("JJ_SYNC_SSH_ARGUMENTS", arguments.as_os_str());
    arguments
}

#[cfg(unix)]
fn ssh_sync_url(peer: &TestWorkDir<'_>) -> String {
    let encoded = percent_encoding::utf8_percent_encode(
        peer.root().to_str().unwrap(),
        percent_encoding::NON_ALPHANUMERIC,
    )
    .to_string()
    .replace("%2F", "/");
    format!("ssh://user@example.com:2222{encoded}")
}

#[cfg(unix)]
#[test_case(false; "native_git")]
#[test_case(true; "raw_fallback")]
fn test_sync_metadata_lock_count_does_not_grow_with_history(raw_fallback: bool) {
    let mut env = TestEnvironment::default();
    if raw_fallback {
        env.add_env_var("JJ_TEST_SYNC_DISABLE_NATIVE_GIT", "all");
    }
    let source = init_repo(&env, "source");
    let small = init_repo(&env, "small");
    let large = init_repo(&env, "large");
    let lock_path = source.root().join(".jj/repo/store/extra/lock");
    let lock_path = lock_path.to_str().unwrap();
    let count_locks = |stderr: &str| {
        let stderr = anstream::adapter::strip_str(stderr).to_string();
        stderr
            .lines()
            .filter(|line| line.contains("Attempting to lock") && line.contains(lock_path))
            .count()
    };
    for i in 0..4 {
        source
            .run_jj(["new", "-m", &format!("history {i}")])
            .success();
    }
    let output = source
        .run_jj(["--debug", "sync", "--send", small.root().to_str().unwrap()])
        .success();
    let small_locks = count_locks(output.stderr.raw());
    assert!(
        small_locks > 0,
        "metadata lock instrumentation was not enabled"
    );

    for i in 4..36 {
        source
            .run_jj(["new", "-m", &format!("history {i}")])
            .success();
    }
    let source_op = source.current_operation_id();
    let output = source
        .run_jj(["--debug", "sync", "--send", large.root().to_str().unwrap()])
        .success();
    let large_locks = count_locks(output.stderr.raw());
    // A small allowance covers fixed setup work. Traversing another 32 commits
    // must not acquire the metadata lock for every commit or validation pass.
    assert!(
        large_locks <= small_locks + 4,
        "metadata lock acquisitions grew from {small_locks} to {large_locks} with history"
    );
    assert_imported_history(&source, &large, &source_op);
}

#[cfg(unix)]
#[test_case("--send", false; "local_send")]
#[test_case("--receive", false; "local_receive")]
#[test_case("--bidi", false; "local_bidirectional")]
#[test_case("--send", true; "ssh_send")]
#[test_case("--receive", true; "ssh_receive")]
#[test_case("--bidi", true; "ssh_bidirectional")]
fn test_sync_native_git_preserves_packed_history_on_repeat(direction: &str, ssh: bool) {
    let mut env = TestEnvironment::default();
    env.add_config("snapshot.max-new-file-size = '10MiB'\n");
    // Exhaust the raw-record cache without constructing 65,536 objects. Git's
    // own negotiation must still avoid sending known historical blob bodies.
    env.add_env_var("JJ_TEST_SYNC_KNOWN_RECORD_IDS", "1");
    if ssh {
        configure_fake_sync_ssh(&mut env);
    }
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    let (source, receiver) = if direction == "--receive" {
        (&right, &left)
    } else {
        (&left, &right)
    };
    describe_and_bookmark(receiver, "receiver");
    // Keep enough distinct data that resending the entire compressed pack
    // exceeds the repeat-transfer limit, while most of the blob compresses.
    let mut seed = 42u64;
    let mut contents: String = (0..128 * 1024)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            char::from(b' ' + ((seed >> 32) % 95) as u8)
        })
        .collect();
    contents.push_str(&"retained historical content compresses well\n".repeat(128 * 1024));
    source.write_file("historical.txt", &contents);
    source
        .run_jj(["describe", "-m", "historical blob"])
        .success();
    source
        .run_jj(["bookmark", "create", "historical"])
        .success();
    let historical = revision_id(source, "historical");
    source.run_jj(["new"]).success();
    fs::remove_file(source.root().join("historical.txt")).unwrap();
    describe_and_bookmark(source, "source");
    let source_op = source.current_operation_id();
    let receiver_wc = revision_id(receiver, "@");
    let peer = if ssh {
        ssh_sync_url(&right)
    } else {
        right.root().to_str().unwrap().to_owned()
    };
    let output = left.run_jj(["--debug", "sync", direction, &peer]).success();
    let stderr = anstream::adapter::strip_str(output.stderr.raw()).to_string();
    for phase in [
        "Transferring Git objects with native Git",
        "Native Git object transfer completed",
    ] {
        assert!(stderr.contains(phase), "missing {phase}: {stderr}");
    }
    assert!(stderr.contains("sending=true"), "{stderr}");
    assert!(stderr.contains("sending=false"), "{stderr}");
    assert_eq!(revision_id(receiver, "historical"), historical);
    assert_imported_history(source, receiver, &source_op);
    assert_eq!(revision_id(receiver, "@"), receiver_wc);
    assert!(!receiver.root().join("historical.txt").exists());
    let received = receiver
        .run_jj(["file", "show", "-r", "historical", "historical.txt"])
        .success();
    assert!(
        received.stdout.raw() == contents,
        "received historical blob contents changed"
    );
    if direction == "--bidi" {
        assert!(bookmarks(source).contains("receiver"));
    }

    let git = gix::open(receiver.root().join(".jj/repo/store/git")).unwrap();
    let tree = git
        .find_commit(gix::ObjectId::from_hex(historical.as_bytes()).unwrap())
        .unwrap()
        .tree()
        .unwrap();
    let blob = tree
        .iter()
        .map(Result::unwrap)
        .find(|entry| entry.filename() == b"historical.txt")
        .unwrap()
        .oid()
        .to_owned()
        .to_string();
    let object_directory = receiver.root().join(".jj/repo/store/git/objects");
    assert!(
        !object_directory.join(&blob[..2]).join(&blob[2..]).exists(),
        "native transfer should keep the historical blob packed"
    );
    let pack_bytes = || {
        fs::read_dir(object_directory.join("pack"))
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pack"))
            .map(|entry| entry.metadata().unwrap().len())
            .sum::<u64>()
    };
    let before = pack_bytes();
    assert!(
        before > 64 * 1024,
        "fixture pack must exceed the allowed repeat-transfer size"
    );
    assert!(
        before < contents.len() as u64 / 20,
        "historical blob expanded to {before} bytes instead of staying compressed"
    );
    let receiver_op = receiver.current_operation_id();
    let repeated = left.run_jj(["--debug", "sync", direction, &peer]).success();
    assert_eq!(receiver.current_operation_id(), receiver_op);
    let repeated_stderr = anstream::adapter::strip_str(repeated.stderr.raw()).to_string();
    assert!(
        repeated_stderr.contains("Native Git object transfer completed"),
        "{repeated_stderr}"
    );
    let byte_fields = regex::Regex::new(r"(?:sent_bytes|received_bytes)=(\d+)").unwrap();
    let mut reports = 0;
    for captures in byte_fields.captures_iter(&repeated_stderr) {
        let bytes = captures[1].parse::<u64>().unwrap();
        assert!(
            bytes < 64 * 1024,
            "repeat synchronization resent {bytes} bytes of native Git data"
        );
        reports += 1;
    }
    assert!(reports > 0, "no transfer sizes reported: {repeated_stderr}");
    assert!(
        pack_bytes() <= before + 16 * 1024,
        "repeat synchronization duplicated historical Git objects"
    );
}

#[cfg(unix)]
fn rsync_is_available() -> bool {
    let Ok(output) = std::process::Command::new("rsync")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .and_then(|line| line.rsplit_once("protocol version"))
            .and_then(|(_, protocol)| protocol.trim().parse::<u32>().ok())
            .is_some_and(|protocol| protocol >= 31)
}

#[cfg(unix)]
#[test_case("--send", false; "local_send")]
#[test_case("--receive", false; "local_receive")]
#[test_case("--bidi", false; "local_bidirectional")]
#[test_case("--send", true; "ssh_send")]
#[test_case("--receive", true; "ssh_receive")]
#[test_case("--bidi", true; "ssh_bidirectional")]
fn test_sync_rsync_immutable_history_is_additive_and_preserves_undo(direction: &str, ssh: bool) {
    if !rsync_is_available() {
        return;
    }
    let mut env = TestEnvironment::default();
    // A one-entry legacy ID cache must not force immutable files to be resent.
    env.add_env_var("JJ_TEST_SYNC_KNOWN_RECORD_IDS", "1");
    if ssh {
        configure_fake_sync_ssh(&mut env);
    }
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    let (source, receiver) = if direction == "--receive" {
        (&right, &left)
    } else {
        (&left, &right)
    };
    for index in 0..16 {
        source
            .run_jj(["new", "-m", &format!("retained operation {index}")])
            .success();
    }
    describe_and_bookmark(source, "source");
    describe_and_bookmark(receiver, "receiver");
    let original_operation = source.current_operation_id();
    let peer = if ssh {
        ssh_sync_url(&right)
    } else {
        right.root().to_str().unwrap().to_owned()
    };
    let first = left.run_jj(["--debug", "sync", direction, &peer]).success();
    let stderr = anstream::adapter::strip_str(first.stderr.raw()).to_string();
    assert!(
        stderr.contains("Transferring immutable history files with rsync"),
        "{stderr}"
    );
    assert_imported_history(source, receiver, &original_operation);

    // The first repeat can exchange receiver-local integration operations that
    // were created after the original bidirectional capture. They are history,
    // even though neither repository needs another integration operation.
    left.run_jj(["sync", direction, &peer]).success();
    let before_left = left.current_operation_id();
    let before_right = right.current_operation_id();
    let repeated = left.run_jj(["--debug", "sync", direction, &peer]).success();
    let stderr = anstream::adapter::strip_str(repeated.stderr.raw()).to_string();
    let counts = regex::Regex::new(r"copied_files=(\d+)").unwrap();
    let copied: Vec<usize> = counts
        .captures_iter(&stderr)
        .map(|captures| captures[1].parse().unwrap())
        .collect();
    assert!(!copied.is_empty(), "{stderr}");
    assert!(copied.iter().all(|count| *count == 0), "{stderr}");
    assert_eq!(left.current_operation_id(), before_left);
    assert_eq!(right.current_operation_id(), before_right);

    receiver.write_file("remote.txt", "a small edit after the initial transfer\n");
    receiver.run_jj(["describe", "-m", "remote edit"]).success();
    receiver.run_jj(["bookmark", "create", "remote"]).success();
    let remote_commit = revision_id(receiver, "remote");
    let remote_operation = receiver.current_operation_id();
    let reverse = match direction {
        "--send" => "--receive",
        "--receive" => "--send",
        _ => "--bidi",
    };
    let received = left.run_jj(["--debug", "sync", reverse, &peer]).success();
    let stderr = anstream::adapter::strip_str(received.stderr.raw()).to_string();
    // Helper diagnostics appear both in live debug tracing and again in its
    // final stderr. Bound each receiving phase instead of counting it twice.
    let largest_transfer = counts
        .captures_iter(&stderr)
        .map(|captures| captures[1].parse::<usize>().unwrap())
        .max()
        .expect("debug output must report copied immutable files");
    assert!(largest_transfer > 0 && largest_transfer < 32, "{stderr}");
    assert_eq!(revision_id(source, "remote"), remote_commit);
    assert_imported_history(receiver, source, &remote_operation);
    assert!(!source.root().join("remote.txt").exists());
    source.run_jj(["undo"]).success();
    assert!(!bookmarks(source).contains("remote"));
    left.run_jj(["sync", reverse, &peer]).success();
    assert!(!bookmarks(source).contains("remote"));
    assert_imported_history(receiver, source, &remote_operation);
}

#[cfg(unix)]
#[test_case("helper"; "older_peer")]
#[test_case("all"; "disabled")]
#[test_case("missing"; "missing_executable")]
fn test_sync_rsync_fallback_preserves_history(disabled: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    let mut env = TestEnvironment::default();
    configure_fake_sync_ssh(&mut env);
    if disabled == "missing" {
        let rsync = env.env_root().join("fake-bin/rsync");
        fs::write(&rsync, "#!/bin/sh\nexit 127\n").unwrap();
        fs::set_permissions(&rsync, fs::Permissions::from_mode(0o700)).unwrap();
    } else {
        env.add_env_var("JJ_TEST_SYNC_DISABLE_RSYNC", disabled);
    }
    let source = init_repo(&env, "source");
    let receiver = init_repo(&env, "receiver");
    describe_and_bookmark(&source, "source");
    describe_and_bookmark(&receiver, "receiver");
    let source_operation = source.current_operation_id();
    let peer = ssh_sync_url(&receiver);
    let output = source
        .run_jj(["--debug", "sync", "--send", &peer])
        .success();
    let stderr = anstream::adapter::strip_str(output.stderr.raw()).to_string();
    assert!(stderr.contains("Transferring Git objects with native Git"));
    assert!(
        !stderr.contains("Transferring immutable history files with rsync"),
        "{stderr}"
    );
    assert_imported_history(&source, &receiver, &source_operation);
    receiver.run_jj(["undo"]).success();
    source.run_jj(["sync", "--send", &peer]).success();
    assert_eq!(
        bookmarks(&receiver),
        BTreeSet::from(["receiver".to_owned()])
    );
}

#[cfg(unix)]
#[test_case(false; "adaptive_metadata")]
#[test_case(true; "older_native_peer")]
fn test_sync_remote_edit_transfers_only_changed_commit_metadata(old_peer: bool) {
    let mut env = TestEnvironment::default();
    configure_fake_sync_ssh(&mut env);
    if old_peer {
        env.add_env_var("JJ_TEST_SYNC_DISABLE_METADATA_NEGOTIATION", "helper");
    }
    let source = init_repo(&env, "source");
    let remote = init_repo(&env, "remote");
    for index in 0..64 {
        source
            .run_jj(["new", "-m", &format!("retained history {index}")])
            .success();
    }
    source.run_jj(["bookmark", "create", "source"]).success();
    let peer = ssh_sync_url(&remote);
    source.run_jj(["sync", "--send", &peer]).success();
    remote.write_file("remote.txt", "new work on the remote machine\n");
    remote.run_jj(["describe", "-m", "remote edit"]).success();
    remote.run_jj(["bookmark", "create", "remote"]).success();
    let remote_commit = revision_id(&remote, "remote");
    let remote_op = remote.current_operation_id();
    let remote_change = remote
        .run_jj(["log", "--no-graph", "-r", "remote", "-T", "change_id"])
        .success();
    let output = source
        .run_jj(["--debug", "sync", "--receive", &peer])
        .success();
    let stderr = anstream::adapter::strip_str(output.stderr.raw()).to_string();
    let counts = regex::Regex::new(r"commit_metadata_records=(\d+)").unwrap();
    let largest_transfer = counts
        .captures_iter(&stderr)
        .map(|captures| captures[1].parse::<usize>().unwrap())
        .max()
        .expect("debug output must report actual metadata records");
    if old_peer {
        assert!(
            largest_transfer >= 64,
            "legacy metadata was skipped: {stderr}"
        );
        assert!(!stderr.contains("Commit metadata negotiation completed"));
    } else {
        assert!(
            largest_transfer < 8,
            "unchanged metadata was resent: {stderr}"
        );
        assert!(stderr.contains("Commit metadata negotiation completed"));
    }
    assert!(stderr.contains("Transferring Git objects with native Git"));
    assert_eq!(revision_id(&source, "remote"), remote_commit);
    let received_change = source
        .run_jj(["log", "--no-graph", "-r", "remote", "-T", "change_id"])
        .success();
    assert_eq!(received_change.stdout.raw(), remote_change.stdout.raw());
    assert_imported_history(&remote, &source, &remote_op);
}

#[cfg(unix)]
#[test]
fn test_sync_adaptive_metadata_rejects_incompatible_existing_sidecar() {
    use jj_lib::stacked_table::TableSegment as _;
    use jj_lib::stacked_table::TableStore;

    let env = TestEnvironment::default();
    let source = init_repo(&env, "source");
    let receiver = init_repo(&env, "receiver");
    describe_and_bookmark(&source, "base");
    let base = gix::ObjectId::from_hex(revision_id(&source, "base").as_bytes()).unwrap();
    source.run_jj(["new", "-m", "shared history"]).success();
    source.run_jj(["bookmark", "create", "source"]).success();
    let commit = gix::ObjectId::from_hex(revision_id(&source, "source").as_bytes()).unwrap();
    source.run_jj(["sync", "--send", "../receiver"]).success();
    let receiver_op = receiver.current_operation_id();
    let tables = TableStore::load(receiver.root().join(".jj/repo/store/extra"), 20);
    let head = tables.get_head().unwrap();
    let mut different = head.get_value(commit.as_bytes()).unwrap().to_vec();
    // Add a valid protobuf predecessors entry (field 2) without changing the
    // physical Git commit. Its existing base commit is already present here.
    different.extend_from_slice(&[0x12, 20]);
    different.extend_from_slice(base.as_bytes());
    let mut mutation = head.start_mutation();
    mutation.add_entry(commit.as_bytes().to_vec(), different.clone());
    tables.save_table(mutation).unwrap();

    let output = source.run_jj(["--debug", "sync", "--send", "../receiver"]);
    assert!(!output.status.success());
    assert!(
        output
            .stderr
            .raw()
            .contains("Incompatible jj metadata for Git commit"),
        "{}",
        output.stderr.raw()
    );
    assert_eq!(receiver.current_operation_id(), receiver_op);
    assert_eq!(
        tables.get_head().unwrap().get_value(commit.as_bytes()),
        Some(different.as_slice())
    );
}

#[cfg(unix)]
#[test_case("helper"; "mixed_capabilities")]
#[test_case("all"; "raw_only")]
fn test_sync_raw_fallback_preserves_metadata_and_undo(disabled: &str) {
    let mut env = TestEnvironment::default();
    configure_fake_sync_ssh(&mut env);
    env.add_env_var("JJ_TEST_SYNC_DISABLE_NATIVE_GIT", disabled);
    let source = init_repo(&env, "source");
    let receiver = init_repo(&env, "receiver");
    describe_and_bookmark(&source, "source");
    source
        .run_jj(["describe", "-m", "rewritten source description"])
        .success();
    describe_and_bookmark(&receiver, "receiver");
    let source_commit = revision_id(&source, "source");
    let source_op = source.current_operation_id();
    let source_change = source
        .run_jj(["log", "--no-graph", "-r", "source", "-T", "change_id"])
        .success();
    let receiver_wc = revision_id(&receiver, "@");
    let output = receiver
        .run_jj(["--debug", "sync", "--receive", &ssh_sync_url(&source)])
        .success();
    let stderr = anstream::adapter::strip_str(output.stderr.raw()).to_string();
    assert!(
        stderr.contains("Synchronization transfer progress"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Transferring Git objects with native Git"),
        "{stderr}"
    );
    assert_eq!(revision_id(&receiver, "source"), source_commit);
    let received_change = receiver
        .run_jj(["log", "--no-graph", "-r", "source", "-T", "change_id"])
        .success();
    assert_eq!(received_change.stdout.raw(), source_change.stdout.raw());
    assert_imported_history(&source, &receiver, &source_op);
    receiver.run_jj(["undo"]).success();
    assert_eq!(
        bookmarks(&receiver),
        BTreeSet::from(["receiver".to_owned()])
    );
    receiver.run_jj(["redo"]).success();
    assert_eq!(
        bookmarks(&receiver),
        BTreeSet::from(["receiver".to_owned(), "source".to_owned()])
    );
    assert_eq!(revision_id(&receiver, "@"), receiver_wc);
    assert!(!receiver.root().join("source.txt").exists());
}

#[cfg(unix)]
#[test_case(false; "ordinary_failure")]
#[test_case(true; "debug_failure")]
fn test_sync_ssh_init_failure_reports_destination_and_helper_stderr(debug: bool) {
    let mut env = TestEnvironment::default();
    let arguments = configure_fake_sync_ssh(&mut env);
    env.add_env_var("JJ_SYNC_SSH_DIAGNOSTIC", "SSH helper diagnostic");
    let source = init_repo(&env, "source");
    let source_op = source.current_operation_id();
    let receiver = env.work_dir("missing-parent/peer");
    let url = ssh_sync_url(&receiver);
    let mut args = vec!["sync", "--send", &url];
    if debug {
        args.insert(0, "--debug");
    }
    let output = source.run_jj(args);
    assert!(!output.status.success());
    let stderr = output.stderr.raw();
    assert!(stderr.contains("Peer synchronization failed"), "{stderr}");
    assert!(stderr.contains("Failed to create workspace at"), "{stderr}");
    assert!(
        stderr.contains(receiver.root().to_str().unwrap()),
        "{stderr}"
    );
    assert!(stderr.contains("SSH helper diagnostic"), "{stderr}");
    if debug {
        assert!(
            stderr.contains("Starting repository synchronization"),
            "{stderr}"
        );
        assert!(
            stderr.contains("Opening peer synchronization repository"),
            "{stderr}"
        );
        assert!(
            stderr.contains("Initializing synchronization destination"),
            "{stderr}"
        );
    }
    assert_eq!(source.current_operation_id(), source_op);
    assert!(!env.env_root().join("missing-parent").exists());
    assert_eq!(
        fs::read_to_string(arguments)
            .unwrap()
            .lines()
            .last()
            .unwrap(),
        if debug {
            "jj --debug --quiet util sync-server"
        } else {
            "jj --quiet util sync-server"
        },
    );
}

#[cfg(unix)]
#[test_case("--send", false; "local_send")]
#[test_case("--receive", false; "local_receive")]
#[test_case("--send", true; "ssh_send")]
#[test_case("--receive", true; "ssh_receive")]
fn test_sync_reports_phase_progress_without_debug(direction: &str, ssh: bool) {
    let mut env = TestEnvironment::default();
    // Disabling interactive indicators must not hide local phase descriptions
    // in redirected output. Routine helper output is always suppressed.
    env.add_config("ui.progress-indicator = false\n");
    if ssh {
        configure_fake_sync_ssh(&mut env);
    }
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    describe_and_bookmark(&left, "left");
    describe_and_bookmark(&right, "right");
    let peer = if ssh {
        ssh_sync_url(&right)
    } else {
        right.root().to_str().unwrap().to_owned()
    };

    let output = left.run_jj(["sync", direction, &peer]).success();
    assert!(output.stdout.raw().is_empty(), "{}", output.stdout.raw());
    let stderr = output.stderr.raw();
    for phase in [
        "Preparing repository",
        "Reading operation history",
        "Collecting history objects",
    ] {
        assert!(
            stderr.contains(&format!("Local: {phase}")),
            "missing local phase {phase}: {stderr}"
        );
    }
    if direction == "--send" {
        assert!(stderr.contains("Local: Sending Git objects"), "{stderr}");
        assert!(
            stderr.contains("Waiting for peer to apply synchronized history"),
            "{stderr}"
        );
    } else {
        for phase in [
            "Receiving Git objects",
            "Validating received history",
            "Installing received objects",
            "Applying synchronized history",
        ] {
            assert!(
                stderr.contains(&format!("Local: {phase}")),
                "missing local receiver phase {phase}: {stderr}"
            );
        }
    }
    assert!(stderr.contains("Synchronization complete."), "{stderr}");
    assert!(!stderr.contains("Peer:"), "{stderr}");
    assert!(!stderr.contains("Helper stderr"), "{stderr}");
    assert!(!stderr.contains('\r'), "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "{stderr}");
    let receiver = if direction == "--send" { &right } else { &left };
    assert_eq!(
        bookmarks(receiver),
        BTreeSet::from(["left".to_owned(), "right".to_owned()])
    );
}

#[cfg(unix)]
#[test_case(false, false; "local_flag")]
#[test_case(true, false; "local_config")]
#[test_case(false, true; "ssh_flag")]
#[test_case(true, true; "ssh_config")]
fn test_sync_quiet_suppresses_phase_progress_on_both_endpoints(config: bool, ssh: bool) {
    let mut env = TestEnvironment::default();
    let arguments = ssh.then(|| configure_fake_sync_ssh(&mut env));
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "right");
    describe_and_bookmark(&left, "left");
    describe_and_bookmark(&right, "right");
    let peer = if ssh {
        ssh_sync_url(&right)
    } else {
        right.root().to_str().unwrap().to_owned()
    };
    // A command-line config override is not inherited by the helper. Local
    // progress follows the effective setting; the helper is always quiet.
    let quiet = if config {
        "--config=ui.quiet=true"
    } else {
        "--quiet"
    };
    let output = left.run_jj([quiet, "sync", "--send", &peer]).success();
    assert!(output.stdout.raw().is_empty(), "{}", output.stdout.raw());
    assert!(output.stderr.raw().is_empty(), "{}", output.stderr.raw());
    assert_eq!(
        bookmarks(&right),
        BTreeSet::from(["left".to_owned(), "right".to_owned()])
    );
    if let Some(arguments) = arguments {
        assert_eq!(
            fs::read_to_string(arguments)
                .unwrap()
                .lines()
                .last()
                .unwrap(),
            "jj --quiet util sync-server",
        );
    }
}

#[cfg(unix)]
#[test_case(false; "ordinary_output")]
#[test_case(true; "quiet_output")]
fn test_sync_suppresses_successful_helper_diagnostics(quiet: bool) {
    let mut env = TestEnvironment::default();
    configure_fake_sync_ssh(&mut env);
    env.add_env_var("JJ_SYNC_SSH_DIAGNOSTIC", "SSH connection warning");
    let source = init_repo(&env, "source");
    let receiver = init_repo(&env, "receiver");
    describe_and_bookmark(&source, "source");
    let url = ssh_sync_url(&receiver);
    let mut args = vec!["sync", "--send", &url];
    if quiet {
        args.insert(0, "--quiet");
    }
    let output = source.run_jj(args).success();
    assert!(output.stdout.raw().is_empty(), "{}", output.stdout.raw());
    let stderr = output.stderr.raw();
    assert!(!stderr.contains("SSH connection warning"), "{stderr}");
    assert!(!stderr.contains("Helper stderr"), "{stderr}");
    if quiet {
        assert!(stderr.is_empty(), "{stderr}");
    }
    assert_eq!(bookmarks(&receiver), BTreeSet::from(["source".to_owned()]));
}

#[cfg(unix)]
#[test_case(false; "local_helper")]
#[test_case(true; "ssh_helper")]
fn test_sync_debug_reports_helper_phases(ssh: bool) {
    let mut env = TestEnvironment::default();
    if ssh {
        configure_fake_sync_ssh(&mut env);
    }
    let source = init_repo(&env, "source");
    describe_and_bookmark(&source, "source");
    let receiver = env.work_dir("receiver");
    let peer = if ssh {
        ssh_sync_url(&receiver)
    } else {
        receiver.root().to_str().unwrap().to_owned()
    };
    let output = source
        .run_jj(["--debug", "sync", "--send", &peer])
        .success();
    let stderr = anstream::adapter::strip_str(output.stderr.raw()).to_string();
    for phase in [
        "Starting repository synchronization",
        "Opening peer synchronization repository",
        "Initializing synchronization destination",
        "Peer accepted synchronization handshake",
        "Collecting synchronization object inventory",
        "Synchronization object inventory collected",
        "Sending synchronization objects",
        "Receiving synchronization objects",
        "Synchronization transfer progress",
        "Validating received synchronization history",
        "Received synchronization history validated",
        "Installing synchronized objects",
        "Synchronized objects installed",
        "Applying synchronized history",
    ] {
        assert!(stderr.contains(phase), "missing {phase}: {stderr}");
    }
    assert!(stderr.contains("completed_objects="), "{stderr}");
    assert!(stderr.contains("total_objects=Some("), "{stderr}");
    assert!(stderr.contains("total_objects=None"), "{stderr}");
    assert!(stderr.contains("transferred_bytes="), "{stderr}");
    assert!(stderr.contains("complete=true"), "{stderr}");
    assert_eq!(bookmarks(&receiver), BTreeSet::from(["source".to_owned()]));
}

#[cfg(unix)]
#[test]
fn test_sync_ssh_paths_are_protocol_data() {
    let mut env = TestEnvironment::default();
    let arguments = configure_fake_sync_ssh(&mut env);
    let left = init_repo(&env, "left");
    let right = init_repo(&env, "peer ' ; $(touch escaped)");
    describe_and_bookmark(&right, "right");
    let url = ssh_sync_url(&right);
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
            "jj --quiet util sync-server"
        ]
    );
    assert!(!left.root().join("escaped").exists());
}

#[cfg(unix)]
#[test_case("--send"; "send")]
#[test_case("--bidi"; "bidirectional")]
fn test_sync_initializes_missing_ssh_peer(direction: &str) {
    let mut env = TestEnvironment::default();
    let arguments = configure_fake_sync_ssh(&mut env);
    let source = init_repo(&env, "source");
    describe_and_bookmark(&source, "source");
    let source_op = source.current_operation_id();
    let receiver = env.work_dir("peer ' ; $(touch escaped)");
    let url = ssh_sync_url(&receiver);
    let output = source.run_jj(["sync", direction, &url]).success();
    let stderr = output.stderr.raw();
    assert!(!stderr.contains("Initialized repo"), "{stderr}");
    assert!(!stderr.contains("Peer:"), "{stderr}");
    assert!(!stderr.contains("Helper stderr"), "{stderr}");
    assert_eq!(
        stderr.matches("Synchronization complete.").count(),
        1,
        "{stderr}"
    );
    assert!(receiver.root().join(".git").is_dir());
    assert_eq!(bookmarks(&receiver), BTreeSet::from(["source".to_owned()]));
    assert_imported_history(&source, &receiver, &source_op);
    assert!(!receiver.root().join("source.txt").exists());
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
            "jj --quiet util sync-server"
        ]
    );
    assert!(!source.root().join("escaped").exists());
    assert!(!env.env_root().join("escaped").exists());
}

#[test_case(false; "raw_records")]
#[cfg_attr(unix, test_case(true; "native_git"))]
fn test_sync_streams_large_packed_git_blob(native_git: bool) {
    let mut env = TestEnvironment::default();
    if !native_git {
        env.add_env_var("JJ_TEST_SYNC_DISABLE_NATIVE_GIT", "all");
    }
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
    let output = receiver
        .run_jj(["--debug", "sync", "--receive", "../source"])
        .success();
    let stderr = anstream::adapter::strip_str(output.stderr.raw()).to_string();
    assert_eq!(
        stderr.contains("Transferring Git objects with native Git"),
        native_git,
        "{stderr}"
    );
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
