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
use crate::common::create_commit_with_files;

fn create_simple_divergence(work_dir: &TestWorkDir) {
    work_dir.run_jj(["describe", "-m", "left"]).success();
    work_dir
        .run_jj(["describe", "-m", "right", "--at-op=@-"])
        .success();
    // Integrate the concurrent operations before invoking converge.
    work_dir.run_jj(["status"]).success();
}

fn create_automatic_divergence(work_dir: &TestWorkDir, description: &str) {
    work_dir.run_jj(["describe", "-m", description]).success();
    work_dir
        .run_jj([
            "metaedit",
            "--author-timestamp",
            "2001-02-03T04:05:14+07:00",
        ])
        .success();
    work_dir
        .run_jj([
            "metaedit",
            "--author-timestamp",
            "2001-02-03T04:05:15+07:00",
            "--at-op=@-",
        ])
        .success();
    // Integrate the concurrent operations before invoking converge.
    work_dir.run_jj(["status"]).success();
}

fn create_two_automatic_divergences(work_dir: &TestWorkDir) {
    create_automatic_divergence(work_dir, "first");
    work_dir.run_jj(["new", "root()", "-m", "second"]).success();
    create_automatic_divergence(work_dir, "second");
}

fn create_stacked_automatic_divergences(work_dir: &TestWorkDir) {
    work_dir.run_jj(["describe", "-m", "parent"]).success();
    work_dir
        .run_jj(["bookmark", "create", "-r@", "parent"])
        .success();
    work_dir.run_jj(["new", "-m", "child"]).success();
    work_dir
        .run_jj([
            "metaedit",
            "--author-timestamp",
            "2001-02-03T04:05:14+07:00",
            "parent",
        ])
        .success();
    work_dir
        .run_jj([
            "metaedit",
            "--author-timestamp",
            "2001-02-03T04:05:15+07:00",
            "parent",
            "--at-op=@-",
        ])
        .success();
    // Integrate the concurrent operations before invoking converge.
    work_dir.run_jj(["status"]).success();
}

fn create_stacked_parent_description_divergence(work_dir: &TestWorkDir) {
    work_dir.run_jj(["describe", "-m", "parent"]).success();
    work_dir
        .run_jj(["bookmark", "create", "-r@", "parent"])
        .success();
    work_dir.run_jj(["new", "-m", "child"]).success();
    work_dir
        .run_jj(["describe", "-r", "parent", "-m", "left"])
        .success();
    work_dir
        .run_jj(["describe", "-r", "parent", "-m", "right", "--at-op=@-"])
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

fn create_crossed_cleanup_dependencies(work_dir: &TestWorkDir) {
    create_commit_with_files(work_dir, "a", &[], &[]);
    create_commit_with_files(work_dir, "b", &["a"], &[]);
    let original_op = work_dir.current_operation_id();
    work_dir
        .run_jj(["rebase", "-r", "b", "-o", "root()"])
        .success();
    work_dir.run_jj(["rebase", "-r", "a", "-o", "b"]).success();
    work_dir
        .run_jj([
            "bookmark",
            "create",
            "old-b",
            "-r",
            &format!("at_operation({original_op}, b)"),
        ])
        .success();
    // Both A0 -> B0 and B1 -> A1 are now visible. The commit graph is
    // acyclic, but the dependency graph of divergent changes has a cycle.
    work_dir.run_jj(["status"]).success();
}

fn create_divergence_conflict_cleanup_dependencies(work_dir: &TestWorkDir) {
    create_commit_with_files(work_dir, "a", &[], &[("file", "base\n")]);
    create_commit_with_files(work_dir, "left", &["a"], &[("file", "left\n")]);
    create_commit_with_files(work_dir, "right", &["a"], &[("file", "right\n")]);
    create_commit_with_files(work_dir, "conflict", &["left", "right"], &[]);
    let conflict_id = converge_log_value(work_dir, "conflict", "commit_id");

    // Rebase A after its descendants, then reconnect it to the original
    // conflict. Discard the rewritten side branch to leave A0 -> C -> A1.
    work_dir
        .run_jj(["rebase", "-r", "a", "-o", "conflict"])
        .success();
    let rewritten_side =
        converge_log_value(work_dir, "left | right | conflict", "commit_id ++ '\n'");
    work_dir
        .run_jj(["rebase", "-r", "a", "-o", &conflict_id])
        .success();
    work_dir
        .run_jj(["bookmark", "delete", "left", "right", "conflict"])
        .success();
    work_dir
        .run_jj_with(|cmd| cmd.arg("abandon").args(rewritten_side.lines()))
        .success();
    work_dir
        .run_jj(["bookmark", "create", "conflict", "-r", &conflict_id])
        .success();
    work_dir
        .run_jj(["new", "a", "-m", "working copy"])
        .success();
    work_dir.run_jj(["status"]).success();
}

fn cleanup_log_value(work_dir: &TestWorkDir, revision: &str, template: &str) -> String {
    work_dir
        .run_jj([
            "log",
            "--ignore-working-copy",
            "-r",
            revision,
            "--no-graph",
            "-T",
            template,
        ])
        .success()
        .stdout
        .raw()
        .to_owned()
}

fn cleanup_repository_state(work_dir: &TestWorkDir) -> (String, String, String, Option<Vec<u8>>) {
    (
        work_dir.current_operation_id(),
        cleanup_log_value(
            work_dir,
            "all()",
            "commit_id ++ ' ' ++ change_id ++ ' ' ++ parents.map(|p| p.commit_id()).join(' ') ++ \
             ' ' ++ bookmarks ++ '\n'",
        ),
        cleanup_log_value(work_dir, "@", "commit_id"),
        std::fs::read(work_dir.root().join("file")).ok(),
    )
}

fn cleanup_endpoint_key(endpoint: &serde_json::Value) -> (&str, &str) {
    let kind = endpoint["kind"].as_str().unwrap();
    let identity = match kind {
        "divergence" => endpoint["change_id"].as_str().unwrap(),
        "conflict" => endpoint["commit_id"].as_str().unwrap(),
        _ => panic!("unexpected cleanup issue kind: {endpoint}"),
    };
    (kind, identity)
}

fn assert_cleanup_cycle(work_dir: &TestWorkDir, plan: &serde_json::Value) {
    assert_eq!(plan["kind"], "ordering");
    assert_eq!(plan["status"], "blocked");
    assert_eq!(plan["reason"], "cyclic_issue_dependencies");
    let cycle = plan["cycle"].as_array().unwrap();
    assert_eq!(cycle.len(), 2, "{plan}");
    for (index, edge) in cycle.iter().enumerate() {
        let ancestor = &edge["ancestor"];
        let descendant = &edge["descendant"];
        let ancestor_id = ancestor["commit_id"].as_str().unwrap();
        let descendant_id = descendant["commit_id"].as_str().unwrap();
        assert_ne!(ancestor_id, descendant_id, "{plan}");
        assert_eq!(
            cleanup_log_value(
                work_dir,
                &format!("{ancestor_id} & ancestors({descendant_id})"),
                "commit_id",
            ),
            ancestor_id,
            "cycle edge must report exact commits with real ancestry: {plan}"
        );
        for endpoint in [ancestor, descendant] {
            let commit_id = endpoint["commit_id"].as_str().unwrap();
            assert_eq!(commit_id.len(), 40, "{endpoint}");
            assert_eq!(
                cleanup_log_value(work_dir, commit_id, "change_id"),
                endpoint["change_id"].as_str().unwrap()
            );
            let issue_revset = match endpoint["kind"].as_str().unwrap() {
                "divergence" => "divergent()",
                "conflict" => "conflicts()",
                _ => unreachable!(),
            };
            assert_eq!(
                cleanup_log_value(
                    work_dir,
                    &format!("{issue_revset} & {commit_id}"),
                    "commit_id"
                ),
                commit_id,
                "cycle witness must belong to the reported issue: {plan}"
            );
        }
        let next_edge = &cycle[(index + 1) % cycle.len()];
        assert_eq!(
            cleanup_endpoint_key(descendant),
            cleanup_endpoint_key(&next_edge["ancestor"]),
            "cycle edges must connect by cleanup issue identity: {plan}"
        );
    }
}

fn assert_cleanup_is_blocked_without_mutation(work_dir: &TestWorkDir) -> serde_json::Value {
    let before = cleanup_repository_state(work_dir);
    let mut previous_json = None;
    let mut blocked_plan = None;
    for dry_run in [false, true] {
        for json in [false, true, true] {
            let mut args = vec!["converge"];
            if dry_run {
                args.push("--dry-run");
            }
            if json {
                args.push("--json");
            }
            let output = work_dir.run_jj(args);
            assert_eq!(output.status.success(), dry_run, "{output}");
            if json {
                let plan: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
                assert_cleanup_cycle(work_dir, &plan);
                if let Some(previous) = &previous_json {
                    assert_eq!(output.stdout.raw(), previous);
                }
                previous_json = Some(output.stdout.raw().to_owned());
                blocked_plan = Some(plan);
            } else {
                assert!(
                    output
                        .stderr
                        .raw()
                        .contains("Cannot order cleanup issues: cyclic dependencies"),
                    "{output}"
                );
                assert!(
                    !output.stderr.raw().contains("No divergent changes"),
                    "{output}"
                );
            }
            assert_eq!(cleanup_repository_state(work_dir), before, "{output}");
        }
    }
    // An explicit ancestor scope containing the same issues must preserve
    // the blocked result, as well as the repository state.
    let output = work_dir.run_jj(["converge", "-r", "heads(all())", "--json"]);
    assert!(!output.status.success(), "{output}");
    assert_eq!(output.stdout.raw(), previous_json.as_ref().unwrap());
    assert_eq!(cleanup_repository_state(work_dir), before, "{output}");
    blocked_plan.unwrap()
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
        "change_id.short(8) ++ \" \" ++ parents.len() ++ \" \" ++ description.first_line() ++ \
         \"\\n\"",
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
fn test_converge_dry_run_formats_needs_and_inspection_commands() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_merge_parent_divergence(&work_dir);

    let output = work_dir.run_jj(["converge", "--dry-run"]);
    assert!(output.status.success(), "{output}");
    assert!(output.stderr.raw().contains("  NEEDS_INPUT"), "{output}");
    assert!(output.stderr.raw().contains("  needs:"), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("description: --message-from <commit-id> or -m <message>"),
        "{output}"
    );
    assert!(
        output
            .stderr
            .raw()
            .contains("parent     : --onto <commit-id>"),
        "{output}"
    );
    assert!(output.stderr.raw().contains("  inspect:"), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("descriptions: jj log -r 'commit_id("),
        "{output}"
    );
    assert!(
        output
            .stderr
            .raw()
            .contains("parents    : jj log -r 'commit_id("),
        "{output}"
    );
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
fn test_converge_multiple_changes_non_interactively() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_two_automatic_divergences(&work_dir);

    let output = work_dir.run_jj(["converge", "-r", "divergent()"]);
    assert!(output.status.success(), "{output}");
    assert_eq!(
        output.stderr.raw().matches("Converged change").count(),
        1,
        "{output}"
    );
    let output = work_dir.run_jj(["converge", "-r", "divergent()"]);
    assert!(output.status.success(), "{output}");
    assert_eq!(output.stderr.raw().matches("Converged change").count(), 1);

    let output = work_dir.run_jj(["log", "-r", "divergent()", "--no-graph"]);
    assert_eq!(output.stdout.raw(), "");
}

#[test]
fn test_converge_multiple_changes_processes_ancestors_first() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_stacked_automatic_divergences(&work_dir);

    let output = work_dir.run_jj([
        "log",
        "-r",
        "divergent()",
        "--no-graph",
        "-T",
        "description.first_line() ++ \" \" ++ change_id.short(8) ++ \"\\n\"",
    ]);
    let parent_change_id = output
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("parent "))
        .unwrap();
    let child_change_id = output
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("child "))
        .unwrap();

    let output = work_dir.run_jj(["converge", "-r", "divergent()"]);
    assert!(output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains(&format!("Converged change {parent_change_id}")),
        "{output}"
    );
    assert!(
        !output
            .stderr
            .raw()
            .contains(&format!("Converged change {child_change_id}")),
        "{output}"
    );
    let output = work_dir.run_jj(["converge", "-r", "divergent()"]);
    assert!(output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains(&format!("Converged change {child_change_id}")),
        "{output}"
    );

    let output = work_dir.run_jj(["log", "-r", "divergent()", "--no-graph"]);
    assert_eq!(output.stdout.raw(), "");
}

#[test]
fn test_converge_multiple_changes_dry_run_reports_only_bottom_issue() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_stacked_automatic_divergences(&work_dir);

    let output = work_dir.run_jj(["converge", "-r", "divergent()", "--dry-run"]);
    assert!(output.status.success(), "{output}");
    assert_eq!(
        output.stderr.raw().matches("  DEFERRED").count(),
        0,
        "{output}"
    );
    assert_eq!(
        output.stderr.raw().matches("  READY").count(),
        1,
        "{output}"
    );
}

#[test]
fn test_converge_multiple_changes_hides_descendants_of_unresolved_change() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_stacked_parent_description_divergence(&work_dir);

    let output = work_dir.run_jj(["converge", "-r", "divergent()"]);
    assert!(!output.status.success(), "{output}");
    assert_eq!(
        output.stderr.raw().matches("  DEFERRED").count(),
        0,
        "{output}"
    );
    assert!(
        output
            .stderr
            .raw()
            .contains("    description: --message-from <commit-id> or -m <message>")
    );
}

#[test]
fn test_converge_multiple_changes_skips_unresolved() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_simple_divergence(&work_dir);
    work_dir.run_jj(["new", "root()", "-m", "second"]).success();
    create_automatic_divergence(&work_dir, "second");

    let output = work_dir.run_jj(["converge"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("Cannot converge non-interactively"),
        "{output}"
    );
    assert_eq!(
        output.stderr.raw().matches("Converged change").count(),
        0,
        "{output}"
    );

    let output = work_dir.run_jj([
        "log",
        "-r",
        "divergent()",
        "--no-graph",
        "-T",
        "description.first_line() ++ \"\\n\"",
    ]);
    let mut descriptions = output.stdout.raw().lines().collect::<Vec<_>>();
    descriptions.sort_unstable();
    assert_eq!(descriptions, vec!["left", "right", "second", "second"]);
}

#[test]
fn test_converge_multiple_changes_dry_run_json() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_two_automatic_divergences(&work_dir);

    let output = work_dir.run_jj(["converge", "--dry-run", "--json"]);
    assert!(output.status.success(), "{output}");
    let plan: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(plan["kind"], "divergence");
    assert_eq!(plan["status"], "ready");
    assert_eq!(plan["ready"], true);
    assert!(plan.get("deferred").is_none());

    let output = work_dir.run_jj(["log", "-r", "divergent()", "--no-graph"]);
    assert_ne!(output.stdout.raw(), "");
}

#[test]
fn test_converge_json_resolves_safe_divergence_then_reports_clean() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_automatic_divergence(&work_dir, "safe");

    let output = work_dir.run_jj(["converge", "--json"]);
    assert!(output.status.success(), "{output}");
    let result: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(result["kind"], "divergence");
    assert_eq!(result["status"], "resolved");
    assert_eq!(result["has_conflicts"], false);

    let output = work_dir.run_jj(["converge", "--json"]);
    assert!(output.status.success(), "{output}");
    let result: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(result["status"], "clean");
}

#[test]
fn test_converge_revision_is_stack_scope() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_stacked_automatic_divergences(&work_dir);

    let output = work_dir.run_jj(["converge", "-r", "@", "--json"]);
    assert!(output.status.success(), "{output}");
    let result: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(result["status"], "resolved");
    assert_eq!(result["kind"], "divergence");
}

#[test]
fn test_converge_reports_crossed_change_order_without_mutation() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_crossed_cleanup_dependencies(&work_dir);
    create_commit_with_files(&work_dir, "left", &["a"], &[("file", "left\n")]);
    create_commit_with_files(&work_dir, "right", &["a"], &[("file", "right\n")]);
    create_commit_with_files(&work_dir, "conflict", &["left", "right"], &[]);
    work_dir.run_jj(["status"]).success();

    // The conflict is blocked downstream of the cycle. The reported cycle
    // must exclude that noncyclic prefix and identify only the two changes.
    let plan = assert_cleanup_is_blocked_without_mutation(&work_dir);
    for edge in plan["cycle"].as_array().unwrap() {
        assert_eq!(edge["ancestor"]["kind"], "divergence");
        assert_eq!(edge["descendant"]["kind"], "divergence");
    }
}

#[test]
fn test_converge_reports_divergence_conflict_cycle_without_mutation() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_divergence_conflict_cleanup_dependencies(&work_dir);

    let plan = assert_cleanup_is_blocked_without_mutation(&work_dir);
    let mut issue_kinds = plan["cycle"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| edge["ancestor"]["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    issue_kinds.sort_unstable();
    assert_eq!(issue_kinds, ["conflict", "divergence"]);
}

#[test]
fn test_converge_processes_independent_issue_before_reporting_cycle() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_crossed_cleanup_dependencies(&work_dir);
    work_dir
        .run_jj(["new", "root()", "-m", "independent"])
        .success();
    create_automatic_divergence(&work_dir, "independent");
    let independent_change_id = cleanup_log_value(&work_dir, "@", "change_id");

    let before = cleanup_repository_state(&work_dir);
    let output = work_dir.run_jj(["converge", "--dry-run", "--json"]);
    assert!(output.status.success(), "{output}");
    let plan: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(plan["kind"], "divergence");
    assert_eq!(plan["status"], "ready");
    assert_eq!(plan["change_id"], independent_change_id);
    assert_eq!(cleanup_repository_state(&work_dir), before);

    let output = work_dir.run_jj(["converge", "--json"]);
    assert!(output.status.success(), "{output}");
    let result: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(result["kind"], "divergence");
    assert_eq!(result["status"], "resolved");
    assert_eq!(result["change_id"], independent_change_id);
    assert_eq!(
        cleanup_log_value(
            &work_dir,
            &format!("divergent() & {independent_change_id}"),
            "commit_id"
        ),
        ""
    );
    assert_cleanup_is_blocked_without_mutation(&work_dir);
}

#[test]
fn test_converge_reports_clean_only_for_empty_issue_scope() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    for dry_run in [false, true] {
        let mut args = vec!["converge", "--json"];
        if dry_run {
            args.push("--dry-run");
        }
        let output = work_dir.run_jj(args);
        assert!(output.status.success(), "{output}");
        let result: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
        assert_eq!(result, serde_json::json!({ "status": "clean" }));
    }

    create_crossed_cleanup_dependencies(&work_dir);
    work_dir
        .run_jj(["new", "root()", "-m", "clean tip"])
        .success();
    work_dir.run_jj(["status"]).success();
    let before = cleanup_repository_state(&work_dir);
    for dry_run in [false, true] {
        let mut args = vec!["converge", "-r", "@", "--json"];
        if dry_run {
            args.push("--dry-run");
        }
        let output = work_dir.run_jj(args);
        assert!(output.status.success(), "{output}");
        let result: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
        assert_eq!(result, serde_json::json!({ "status": "clean" }));
        assert_eq!(cleanup_repository_state(&work_dir), before);
    }
    // The excluded branches still contain the cycle.
    assert_cleanup_is_blocked_without_mutation(&work_dir);
}

#[test]
fn test_converge_json_reports_bottom_conflict_instructions() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_commit_with_files(&work_dir, "base", &[], &[("file", "base\n")]);
    create_commit_with_files(&work_dir, "a", &["base"], &[("file", "a\n")]);
    create_commit_with_files(&work_dir, "b", &["base"], &[("file", "b\n")]);
    create_commit_with_files(&work_dir, "conflict", &["a", "b"], &[]);

    let output = work_dir.run_jj(["converge", "--json"]);
    assert!(!output.status.success(), "{output}");
    let result: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(result["kind"], "conflict");
    assert_eq!(result["status"], "needs_manual_resolution");
    assert_eq!(result["paths"][0], "file");
    assert!(
        result["resolve_with"]["start"]
            .as_str()
            .unwrap()
            .starts_with("jj new ")
    );
    assert_eq!(
        result["resolve_with"]["finish"],
        "jj squash --use-destination-message"
    );
}

#[test]
fn test_converge_json_reports_conflict_before_divergent_descendant() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_commit_with_files(&work_dir, "base", &[], &[("file", "base\n")]);
    create_commit_with_files(&work_dir, "a", &["base"], &[("file", "a\n")]);
    create_commit_with_files(&work_dir, "b", &["base"], &[("file", "b\n")]);
    create_commit_with_files(&work_dir, "conflict", &["a", "b"], &[]);
    work_dir.run_jj(["new", "-m", "child"]).success();
    create_automatic_divergence(&work_dir, "child");

    let output = work_dir.run_jj(["converge", "--json"]);
    assert!(!output.status.success(), "{output}");
    let result: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(result["kind"], "conflict");
    assert_eq!(result["status"], "needs_manual_resolution");
}

#[test]
fn test_converge_multiple_changes_rejects_overrides() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_two_automatic_divergences(&work_dir);

    let output = work_dir.run_jj(["converge", "-m", "merged"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("--message cannot be used with multiple divergent changes"),
        "{output}"
    );

    let output = work_dir.run_jj(["converge", "-i"]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output
            .stderr
            .raw()
            .contains("--interactive cannot be used with multiple divergent changes"),
        "{output}"
    );

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
fn test_converge_exact_resolution_emits_json_result() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_simple_divergence(&work_dir);

    let output = work_dir.run_jj(["converge", "-m", "merged", "--json"]);
    assert!(output.status.success(), "{output}");
    let result: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert_eq!(result["kind"], "divergence");
    assert_eq!(result["status"], "resolved");
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

// Retain the upstream graph scenarios while asserting the local command's
// non-interactive cleanup contract instead of upstream's interactive output.
#[test]
fn test_converge_no_divergence() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_commit_with_files(&work_dir, "a", &[], &[("file1", "a")]);
    create_commit_with_files(&work_dir, "b", &["a"], &[("file2", "b")]);
    create_commit_with_files(&work_dir, "c", &["a"], &[("file3", "c")]);

    let before = converge_log_value(&work_dir, "all()", "commit_id ++ '\n'");
    let output = work_dir.run_jj(["converge", "--no-interactive"]).success();
    assert!(
        output
            .stderr
            .raw()
            .contains("No divergent changes or content conflicts found.")
    );
    assert_eq!(
        converge_log_value(&work_dir, "all()", "commit_id ++ '\n'"),
        before
    );
}

fn create_rebased_divergence(work_dir: &TestWorkDir) {
    create_commit_with_files(work_dir, "a", &[], &[("file1", "1")]);
    create_commit_with_files(work_dir, "b2", &["a"], &[("file2", "2")]);
    create_commit_with_files(work_dir, "c", &["a"], &[("file3", "3")]);
    work_dir.run_jj(["rebase", "-r", "b2", "-o", "c"]).success();
    work_dir
        .run_jj(["bookmark", "create", "b1", "-r", "at_operation(@-, b2)"])
        .success();
    create_commit_with_files(work_dir, "d", &["b1"], &[("file4", "4")]);
}

#[test]
fn test_converge_hints_when_only_one_divergent_revision_is_selected() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_simple_divergence(&work_dir);

    let output = work_dir.run_jj([
        "converge",
        "--revisions",
        "latest(divergent())",
        "--onto",
        "root()",
    ]);
    assert!(!output.status.success(), "{output}");
    assert!(
        output.stderr.raw().contains("jj converge -r 'change_id("),
        "{output}"
    );
}


#[test]
fn test_converge_simple() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_rebased_divergence(&work_dir);

    work_dir.run_jj(["converge"]).success();
    assert_eq!(
        converge_log_value(&work_dir, "divergent()", "commit_id"),
        ""
    );
    assert_eq!(
        converge_log_value(&work_dir, "b1", "commit_id"),
        converge_log_value(&work_dir, "b2", "commit_id")
    );
    assert_eq!(
        converge_log_value(&work_dir, "b1-", "commit_id"),
        converge_log_value(&work_dir, "c", "commit_id")
    );
    assert_eq!(
        converge_log_value(&work_dir, "d-", "commit_id"),
        converge_log_value(&work_dir, "b1", "commit_id")
    );
    assert_eq!(converge_predecessor_count(&work_dir, "b1"), "2");
    for (path, contents) in [
        ("file1", "1"),
        ("file2", "2"),
        ("file3", "3"),
        ("file4", "4"),
    ] {
        assert_eq!(
            std::fs::read_to_string(work_dir.root().join(path)).unwrap(),
            contents
        );
    }
}

#[test]
fn test_converge_one_side_rebased_one_side_description_changed() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_rebased_divergence(&work_dir);
    work_dir
        .run_jj(["describe", "-r", "b1", "-m", "new description"])
        .success();

    work_dir.run_jj(["converge"]).success();
    assert_eq!(
        converge_log_value(&work_dir, "b1", "description"),
        "new description\n"
    );
    assert_eq!(
        converge_log_value(&work_dir, "b1-", "commit_id"),
        converge_log_value(&work_dir, "c", "commit_id")
    );
    assert_eq!(
        converge_log_value(&work_dir, "divergent()", "commit_id"),
        ""
    );
}

#[test]
fn test_converge_simple_with_revisions_arg_and_two_divergent_changes() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_rebased_divergence(&work_dir);
    create_commit_with_files(&work_dir, "e2", &["a"], &[("file5", "5")]);
    create_commit_with_files(&work_dir, "f", &["a"], &[("file6", "6")]);
    work_dir.run_jj(["rebase", "-r", "e2", "-o", "f"]).success();
    work_dir
        .run_jj(["bookmark", "create", "e1", "-r", "at_operation(@-, e2)"])
        .success();
    let untouched = converge_log_value(&work_dir, "e1 | e2", "commit_id ++ '\n'");

    // Only b1 is an ancestor of d. Cleanup must include its other divergent
    // version while leaving the unrelated divergent family unchanged.
    work_dir.run_jj(["converge", "--revisions", "d"]).success();
    assert_eq!(
        converge_log_value(&work_dir, "b1", "commit_id"),
        converge_log_value(&work_dir, "b2", "commit_id")
    );
    assert_eq!(
        converge_log_value(&work_dir, "e1 | e2", "commit_id ++ '\n'"),
        untouched
    );
    assert_ne!(
        converge_log_value(&work_dir, "divergent() & (e1 | e2)", "commit_id"),
        ""
    );
}

#[test]
fn test_converge_with_inconsistent_parents() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_commit_with_files(&work_dir, "a", &[], &[("file1", "1")]);
    create_commit_with_files(&work_dir, "b", &[], &[("file2", "2")]);
    create_commit_with_files(&work_dir, "c", &[], &[("file3", "3")]);
    create_commit_with_files(&work_dir, "d2", &["a"], &[("file4", "4")]);
    work_dir.run_jj(["rebase", "-r", "d2", "-o", "b"]).success();
    work_dir
        .run_jj(["bookmark", "create", "d1", "-r", "at_operation(@-, d2)"])
        .success();
    work_dir.run_jj(["rebase", "-r", "d1", "-o", "c"]).success();
    let before = converge_log_value(&work_dir, "d1 | d2", "commit_id ++ '\n'");

    let output = work_dir.run_jj(["converge", "--no-interactive", "--json"]);
    assert!(!output.status.success(), "{output}");
    let plan: serde_json::Value = serde_json::from_str(output.stdout.raw()).unwrap();
    assert!(
        plan["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|choice| choice["kind"] == "parent")
    );
    assert_eq!(
        converge_log_value(&work_dir, "d1 | d2", "commit_id ++ '\n'"),
        before
    );
    work_dir.run_jj(["converge", "--onto", "b"]).success();
    assert_eq!(
        converge_log_value(&work_dir, "d1-", "commit_id"),
        converge_log_value(&work_dir, "b", "commit_id")
    );
    assert_eq!(
        converge_log_value(&work_dir, "divergent()", "commit_id"),
        ""
    );
}

#[test]
fn test_converge_one_divergent_commit_is_a_descendant_of_another_divergent_commit() {
    check_converge_related_divergent_commits(false);
}

#[test]
fn test_converge_two_divergent_commits_with_unrelated_commit_in_between() {
    check_converge_related_divergent_commits(true);
}

fn check_converge_related_divergent_commits(with_intermediate: bool) {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.run_jj(["describe", "-m", "message 1"]).success();
    work_dir.run_jj(["describe", "-m", "message 2"]).success();
    work_dir
        .run_jj(["describe", "-m", "message 3", "--at-op", "@-"])
        .success();
    work_dir
        .run_jj([
            "bookmark",
            "create",
            "-r",
            "description('message 2*')",
            "b2",
        ])
        .success();
    work_dir
        .run_jj([
            "bookmark",
            "create",
            "-r",
            "description('message 3*')",
            "b3",
        ])
        .success();
    if with_intermediate {
        work_dir.run_jj(["new", "-r", "b3", "-m", "foo"]).success();
        work_dir.run_jj(["bookmark", "create", "middle"]).success();
        work_dir
            .run_jj(["rebase", "-r", "b2", "-o", "middle", "--keep-divergent"])
            .success();
        work_dir.run_jj(["new", "-r", "b2", "-m", "bar"]).success();
        work_dir.run_jj(["bookmark", "create", "child"]).success();
    } else {
        work_dir
            .run_jj(["rebase", "-r", "b2", "-o", "b3", "--keep-divergent"])
            .success();
    }

    work_dir.run_jj(["converge", "-m", "merged"]).success();
    let solution = converge_log_value(&work_dir, "b2", "commit_id");
    assert_eq!(converge_log_value(&work_dir, "b3", "commit_id"), solution);
    assert_eq!(
        converge_log_value(&work_dir, "b2-", "commit_id"),
        converge_log_value(&work_dir, "root()", "commit_id")
    );
    assert_eq!(converge_predecessor_count(&work_dir, "b2"), "2");
    assert_eq!(
        converge_log_value(&work_dir, "divergent()", "commit_id"),
        ""
    );
    if with_intermediate {
        assert_eq!(
            converge_log_value(&work_dir, "middle-", "commit_id"),
            solution
        );
        assert_eq!(
            converge_log_value(&work_dir, "child-", "commit_id"),
            solution
        );
        assert_eq!(
            converge_log_value(&work_dir, "middle", "description"),
            "foo\n"
        );
        assert_eq!(
            converge_log_value(&work_dir, "child", "description"),
            "bar\n"
        );
    }
}

fn converge_log_value(work_dir: &TestWorkDir, revision: &str, template: &str) -> String {
    work_dir
        .run_jj(["log", "-r", revision, "--no-graph", "-T", template])
        .success()
        .stdout
        .raw()
        .to_owned()
}

fn converge_predecessor_count(work_dir: &TestWorkDir, revision: &str) -> String {
    work_dir
        .run_jj([
            "evolog",
            "-r",
            revision,
            "--no-graph",
            "--limit=1",
            "-T",
            "predecessors.len()",
        ])
        .success()
        .stdout
        .raw()
        .to_owned()
}
