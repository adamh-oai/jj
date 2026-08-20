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

use crate::common::TestEnvironment;
use crate::common::TestWorkDir;

fn create_simple_divergence(work_dir: &TestWorkDir) {
    work_dir.run_jj(["describe", "-m", "left"]).success();
    work_dir
        .run_jj(["describe", "-m", "right", "--at-op=@-"])
        .success();
    // Integrate the concurrent operations before invoking converge.
    work_dir.run_jj(["status"]).success();
}

fn create_merge_parent_divergence(work_dir: &TestWorkDir) {
    work_dir.run_jj(["describe", "-m", "first"]).success();
    work_dir
        .run_jj(["bookmark", "create", "-r@", "first"])
        .success();
    work_dir.run_jj(["new", "root()", "-m", "second"]).success();
    work_dir
        .run_jj(["bookmark", "create", "-r@", "second"])
        .success();
    work_dir
        .run_jj(["new", "first", "second", "-m", "base"])
        .success();
    work_dir.run_jj(["describe", "-m", "left"]).success();
    work_dir
        .run_jj(["describe", "-m", "right", "--at-op=@-"])
        .success();
    work_dir.run_jj(["status"]).success();
}

fn create_author_divergence(work_dir: &TestWorkDir) {
    work_dir
        .run_jj(["metaedit", "--author", "Left <left@example.com>"])
        .success();
    work_dir
        .run_jj([
            "metaedit",
            "--author",
            "Right <right@example.com>",
            "--at-op=@-",
        ])
        .success();
    work_dir.run_jj(["status"]).success();
}

#[test]
fn test_converge_creates_one_linear_solution() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_simple_divergence(&work_dir);

    let output = work_dir.run_jj(["converge", "-m", "merged"]);
    assert!(output.status.success(), "{output}");
    assert!(
        output.stderr.raw().contains("Converged change qpvuntsm"),
        "{output}"
    );

    let output = work_dir.run_jj([
        "log",
        "-r@",
        "--no-graph",
        "-T",
        "change_id.short(8) ++ \" \" ++ parents.len() ++ \" \" ++ description.first_line() ++ \"\\n\"",
    ]);
    assert_eq!(output.stdout.raw(), "qpvuntsm 1 merged\n");

    let output = work_dir.run_jj(["log", "-r", "divergent()", "--no-graph"]);
    assert_eq!(output.stdout.raw(), "");

    let output = work_dir.run_jj([
        "evolog",
        "-r@",
        "--no-graph",
        "-T",
        "predecessors.len() ++ \"\\n\"",
    ]);
    assert_eq!(output.stdout.raw().lines().next(), Some("2"));
}

#[test]
fn test_converge_edits_conflicting_descriptions() {
    let mut test_env = TestEnvironment::default();
    let edit_script = test_env.set_up_fake_editor();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_simple_divergence(&work_dir);
    std::fs::write(
        edit_script,
        ["dump converge-editor", "write\nmerged from editor"].join("\0"),
    )
    .unwrap();

    let output = work_dir.run_jj(["converge", "-i"]);
    assert!(output.status.success(), "{output}");
    let editor_input =
        std::fs::read_to_string(test_env.env_root().join("converge-editor")).unwrap();
    assert!(
        editor_input.contains("<<<<<<< divergent descriptions\n%%%%%%% base"),
        "{editor_input}"
    );

    let output = work_dir.run_jj([
        "log",
        "-r@",
        "--no-graph",
        "-T",
        "description.first_line() ++ \"\\n\"",
    ]);
    assert_eq!(output.stdout.raw(), "merged from editor\n");
}

#[test]
fn test_converge_reports_unresolved_description_non_interactively() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_simple_divergence(&work_dir);

    let output = work_dir.run_jj(["converge"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("Cannot converge non-interactively"),
        "{output}"
    );
    assert!(
        output
            .stderr
            .raw()
            .contains("Unresolved description: use --message-from <commit-id> or -m <message>"),
        "{output}"
    );

    let output = work_dir.run_jj(["log", "-r", "divergent()", "--no-graph"]);
    assert_ne!(output.stdout.raw(), "");
}

#[test]
fn test_converge_dry_run_json_reports_unresolved_choices() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_simple_divergence(&work_dir);

    let output = work_dir.run_jj(["converge", "--dry-run", "--json"]);
    assert!(output.status.success(), "{output}");
    let plan: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(plan["ready"], false);
    assert_eq!(plan["unresolved"][0]["kind"], "description");
    assert_eq!(
        plan["unresolved"][0]["resolve_with"],
        "--message-from <commit-id> or -m <message>"
    );
    assert_eq!(plan["blocked"].as_array().unwrap().len(), 0);
    assert_eq!(plan["divergent_commits"].as_array().unwrap().len(), 2);
}

#[test]
fn test_converge_dry_run_json_reports_ready_plan_without_rewriting() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_simple_divergence(&work_dir);

    let output = work_dir.run_jj(["converge", "--dry-run", "--json", "-m", "merged"]);
    assert!(output.status.success(), "{output}");
    let plan: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(plan["ready"], true);
    assert_eq!(plan["may_conflict"], false);
    assert_eq!(plan["unresolved"].as_array().unwrap().len(), 0);

    let output = work_dir.run_jj(["log", "-r", "divergent()", "--no-graph"]);
    assert_ne!(output.stdout.raw(), "");
}

#[test]
fn test_converge_message_from_resolves_description() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_simple_divergence(&work_dir);

    let output = work_dir.run_jj(["converge", "--message-from", "@"]);
    assert!(output.status.success(), "{output}");

    let output = work_dir.run_jj(["log", "-r", "divergent()", "--no-graph"]);
    assert_eq!(output.stdout.raw(), "");
}

#[test]
fn test_converge_author_from_resolves_author() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_author_divergence(&work_dir);

    let output = work_dir.run_jj(["converge"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("Unresolved author: use --author-from <commit-id>"),
        "{output}"
    );

    let output = work_dir.run_jj(["converge", "--author-from", "@"]);
    assert!(output.status.success(), "{output}");
    let output = work_dir.run_jj(["log", "-r", "divergent()", "--no-graph"]);
    assert_eq!(output.stdout.raw(), "");
}

#[test]
fn test_converge_reports_implicit_merge_solution_non_interactively() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_merge_parent_divergence(&work_dir);

    let output = work_dir.run_jj(["converge", "-m", "merged"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("Unresolved parent: use --onto <commit-id>"),
        "{output}"
    );
}

#[test]
fn test_converge_onto_linearizes_merge_parent_solution() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_merge_parent_divergence(&work_dir);

    let output = work_dir.run_jj(["converge", "-m", "merged", "--onto", "first"]);
    assert!(output.status.success(), "{output}");

    let output = work_dir.run_jj([
        "log",
        "-r@",
        "--no-graph",
        "-T",
        "parents.len() ++ \" \" ++ parents.first().description().first_line() ++ \"\\n\"",
    ]);
    assert_eq!(output.stdout.raw(), "1 first\n");
}

#[test]
fn test_converge_is_the_only_command_name() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    let output = work_dir.run_jj(["resolve-divergence"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("unrecognized subcommand 'resolve-divergence'"),
        "{output}"
    );
}
