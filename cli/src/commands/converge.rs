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

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Write as _;

use clap_complete::ArgValueCompleter;
use futures::StreamExt as _;
use futures::TryStreamExt as _;
use itertools::Itertools as _;
use jj_lib::backend::ChangeId;
use jj_lib::backend::CommitId;
use jj_lib::backend::Signature;
use jj_lib::commit::Commit;
use jj_lib::converge::CommitsByChangeId;
use jj_lib::converge::ConvergeResult;
use jj_lib::converge::ConvergedAttribute;
use jj_lib::converge::TruncatedEvolutionGraph;
use jj_lib::converge::apply_solution;
use jj_lib::converge::converge_change;
use jj_lib::converge::find_divergent_changes;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::revset::RevsetExpression;
use serde::Serialize;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::cli_util::short_change_hash;
use crate::cli_util::short_commit_hash;
use crate::command_error::CommandError;
use crate::command_error::internal_error;
use crate::command_error::user_error;
use crate::complete;
use crate::description_util::edit_description;
use crate::description_util::join_message_paragraphs;
use crate::ui::Ui;

/// Clean divergent changes and content conflicts from a stack
///
/// The divergent revisions become evolution predecessors of the new revision.
/// They do not become commit-graph parents. The solution always has exactly one
/// commit-graph parent; use --onto to choose that parent when it cannot be
/// inferred automatically. By default, the command never prompts or opens an
/// editor; use --interactive to opt into that behavior. With --json, the
/// command processes the next bottom-most issue: safe divergence is converged
/// automatically, while conflicts and ambiguous divergence are reported with
/// machine-readable instructions.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct ConvergeArgs {
    /// Stack tips whose ancestors should be cleaned
    ///
    /// In normal non-interactive mode, issues are selected from
    /// `(divergent() | conflicts()) & ancestors(<revset>)`. If omitted, all
    /// divergent revisions and conflicted revisions are considered. Interactive
    /// mode and explicit resolution options keep the exact-revision behavior
    /// and require exactly one divergent change ID.
    #[arg(long = "revision", short, value_name = "REVSETS", alias = "revisions")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    revisions: Vec<RevisionArg>,

    /// Canonical parent for the solution revision
    ///
    /// This accepts exactly one revision. It is useful when divergent versions
    /// were rebased onto different parents and no unique linear parent can be
    /// inferred.
    #[arg(long, short = 'o', value_name = "REVSET")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    onto: Option<RevisionArg>,

    /// Revision whose author should be used for the solution revision
    ///
    /// This must select exactly one of the divergent revisions. The full author
    /// signature, including its timestamp, is copied from that revision.
    #[arg(long, value_name = "REVSET")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    author_from: Option<RevisionArg>,

    /// Description for the solution revision
    ///
    /// If the divergent descriptions cannot be merged automatically and this
    /// option is omitted, normal mode reports the unresolved choice. With
    /// --interactive, an editor opens with the competing descriptions.
    #[arg(long = "message", short, value_name = "MESSAGE")]
    message_paragraphs: Option<Vec<String>>,

    /// Revision whose description should be used for the solution revision
    ///
    /// This must select exactly one of the divergent revisions.
    #[arg(long, value_name = "REVSET", conflicts_with = "message_paragraphs")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    message_from: Option<RevisionArg>,

    /// Prompt for unresolved author, description, or parent choices
    ///
    /// Without this option, unresolved choices are reported without changing
    /// history. Explicit options such as --onto, --author-from, --message-from,
    /// and --message take precedence over interactive choices.
    #[arg(long, short = 'i', conflicts_with = "dry_run")]
    interactive: bool,

    /// Do not prompt for unresolved choices (the default)
    #[arg(long = "no-interactive", conflicts_with = "interactive")]
    _no_interactive: bool,

    /// Report the convergence plan without changing history
    #[arg(long)]
    dry_run: bool,

    /// Emit a machine-readable cleanup result as JSON
    ///
    /// Without --dry-run, safe bottom-most divergence is converged
    /// automatically. Ambiguous divergence and content conflicts are reported
    /// without changing history.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct ConvergePlan {
    change_id: String,
    divergent_commits: Vec<String>,
    ready: bool,
    unresolved: Vec<UnresolvedChoice>,
    blocked: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    would_rebase_descendants: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    may_conflict: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ConflictPlan {
    kind: &'static str,
    status: &'static str,
    commit_id: String,
    change_id: String,
    paths: Vec<String>,
    resolve_with: ConflictResolutionInstructions,
}

#[derive(Debug, Serialize)]
struct ConflictResolutionInstructions {
    start: String,
    inspect: &'static str,
    finish: &'static str,
}

#[derive(Debug, Serialize)]
struct ResolvedDivergence {
    kind: &'static str,
    status: &'static str,
    change_id: String,
    commit_id: String,
    rebased_descendants: usize,
    has_conflicts: bool,
}

#[derive(Debug)]
struct AppliedConvergence {
    change_id: ChangeId,
    commit_id: CommitId,
    rebased_descendants: usize,
    has_conflicts: bool,
}

#[derive(Debug)]
enum AutomaticConvergeResult {
    Plan(ConvergePlan),
    Applied(AppliedConvergence),
}

#[derive(Clone, Debug)]
enum CleanupIssue {
    Divergence(ChangeId),
    Conflict(Commit),
}

#[derive(Debug)]
enum CleanupSelection {
    Clean,
    Next(CleanupIssue),
    Blocked(CleanupBlockedPlan),
}

#[derive(Debug, Serialize)]
struct CleanupBlockedPlan {
    kind: &'static str,
    status: &'static str,
    reason: &'static str,
    cycle: Vec<CleanupDependency>,
}

#[derive(Clone, Debug, Serialize)]
struct CleanupDependency {
    ancestor: CleanupDependencyEndpoint,
    descendant: CleanupDependencyEndpoint,
}

#[derive(Clone, Debug, Serialize)]
struct CleanupDependencyEndpoint {
    kind: &'static str,
    change_id: String,
    commit_id: String,
}

#[derive(Debug, Serialize)]
struct UnresolvedChoice {
    kind: &'static str,
    resolve_with: &'static str,
    candidates: Vec<ChoiceCandidate>,
}

#[derive(Debug, Serialize)]
struct ChoiceCandidate {
    commit_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
}

#[instrument(skip_all)]
pub(crate) async fn cmd_converge(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &ConvergeArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    if !args.interactive && !has_explicit_resolution(args) {
        return converge_cleanup(ui, &mut workspace_command, args).await;
    }

    let target_expr = if args.revisions.is_empty() {
        workspace_command.parse_revset(ui, &RevisionArg::from("divergent()".to_owned()))?
    } else {
        workspace_command.parse_union_revsets(ui, &args.revisions)?
    }
    .resolve()?;

    let divergent_changes = find_divergent_changes(workspace_command.repo(), target_expr.clone())
        .await
        .map_err(internal_error)?;
    if divergent_changes.is_empty() {
        if !args.revisions.is_empty()
            && let Some((_, change_id)) = RevsetExpression::divergent()
                .intersection(&target_expr)
                .evaluate(workspace_command.repo().as_ref())?
                .commit_change_ids()
                .try_next()
                .await?
        {
            writeln!(
                ui.hint_default(),
                "Try again by selecting all revisions to converge for some divergent change ID, \
                 such as: jj converge -r 'change_id({})'",
                short_change_hash(&change_id)
            )?;
        }
        return Err(user_error("No divergent changes found"));
    }
    if divergent_changes.len() > 1 {
        return converge_multiple_changes(ui, &mut workspace_command, args, divergent_changes)
            .await;
    }

    let (_change_id, divergent_commits) = divergent_changes.into_iter().next().unwrap();
    let divergent_commit_ids = divergent_commits
        .iter()
        .map(|commit| commit.id().clone())
        .collect_vec();

    // apply_solution() rewrites the divergent commits and their descendants.
    // Reject immutable descendants up front instead of partially rewriting a
    // stack across the immutability boundary.
    let to_rewrite = RevsetExpression::commits(divergent_commit_ids.clone()).descendants();
    let divergent_commit_id_set: HashSet<_> = divergent_commit_ids.iter().cloned().collect();
    let rewrite_commit_ids: Vec<_> = to_rewrite
        .clone()
        .evaluate(workspace_command.repo().as_ref())
        .map_err(internal_error)?
        .stream()
        .try_collect()
        .await
        .map_err(internal_error)?;
    let would_rebase_descendants = rewrite_commit_ids
        .iter()
        .filter(|commit_id| !divergent_commit_id_set.contains(*commit_id))
        .count();

    let graph = TruncatedEvolutionGraph::new(workspace_command.repo().clone(), divergent_commits)
        .await
        .map_err(internal_error)?;
    let change_id = graph.change_id().clone();

    let parent_override = if let Some(revision) = &args.onto {
        Some(vec![
            workspace_command
                .resolve_single_rev(ui, revision)
                .await?
                .id()
                .clone(),
        ])
    } else {
        None
    };
    let author_override = if let Some(revision) = &args.author_from {
        Some(
            resolve_divergent_source_commit(
                ui,
                &workspace_command,
                revision,
                &divergent_commit_id_set,
                "--author-from",
            )
            .await?
            .author()
            .clone(),
        )
    } else {
        None
    };
    let description_override = if let Some(message_paragraphs) = &args.message_paragraphs {
        Some(join_message_paragraphs(message_paragraphs))
    } else if let Some(revision) = &args.message_from {
        Some(
            resolve_divergent_source_commit(
                ui,
                &workspace_command,
                revision,
                &divergent_commit_id_set,
                "--message-from",
            )
            .await?
            .description()
            .to_owned(),
        )
    } else {
        None
    };

    let proposed = converge_change(
        &graph,
        author_override,
        description_override,
        parent_override,
        None,
    )
    .await
    .map_err(internal_error)?;
    let inferred_author = match &proposed.author {
        ConvergedAttribute::Solved(_) => None,
        ConvergedAttribute::Unsolved { base_commit, .. } => {
            infer_author_without_prompt(&graph, base_commit).await?
        }
    };
    let unresolved = collect_unresolved_choices(
        &workspace_command,
        &graph,
        &proposed,
        inferred_author.as_ref(),
    )
    .await?;

    if let Err(error) = workspace_command.check_rewritable_expr(&to_rewrite).await {
        if args.dry_run || args.json {
            let plan = make_plan(
                &graph,
                unresolved,
                vec!["immutable commit in rewrite set".to_owned()],
                would_rebase_descendants,
                None,
            );
            write_divergence_plan(ui, &plan, args.json)?;
            if args.dry_run {
                return Ok(());
            }
        }
        return Err(error);
    }

    if !args.interactive && !unresolved.is_empty() {
        let plan = make_plan(&graph, unresolved, vec![], would_rebase_descendants, None);
        if args.dry_run || args.json {
            write_divergence_plan(ui, &plan, args.json)?;
            if args.dry_run {
                return Ok(());
            }
        }
        return Err(non_interactive_error(&plan));
    }

    let author = match proposed.author {
        ConvergedAttribute::Solved(author) => author,
        ConvergedAttribute::Unsolved { base_commit, .. } => {
            if let Some(author) = inferred_author {
                author
            } else {
                choose_author(ui, &graph, &base_commit).await?
            }
        }
    };
    let description = match proposed.description {
        ConvergedAttribute::Solved(description) => description,
        ConvergedAttribute::Unsolved {
            base_commit,
            excluded_divergent_commits,
        } => {
            edit_merged_description(
                &workspace_command,
                &graph,
                &base_commit,
                &excluded_divergent_commits,
            )
            .await?
        }
    };
    let parents = match proposed.parents {
        ConvergedAttribute::Solved(parents) if parents.len() > 1 && args.interactive => {
            choose_linear_parent(ui, &workspace_command, &graph, &HashSet::new()).await?
        }
        ConvergedAttribute::Solved(parents) => parents,
        ConvergedAttribute::Unsolved {
            excluded_divergent_commits,
            ..
        } => {
            choose_linear_parent(ui, &workspace_command, &graph, &excluded_divergent_commits)
                .await?
        }
    };
    let parent_id = require_linear_parent(&parents)?;
    validate_solution_parent(&workspace_command, &graph, parent_id).await?;

    // Recompute the tree after all user choices have been resolved. The first
    // call intentionally skips tree convergence when the parents were unsolved.
    let solved = converge_change(
        &graph,
        Some(author.clone()),
        Some(description.clone()),
        Some(parents.clone()),
        None,
    )
    .await
    .map_err(internal_error)?;
    let tree = solved
        .tree
        .ok_or_else(|| internal_error("converge did not produce a solution tree"))?;

    if args.dry_run {
        let may_conflict = tree
            .to_merged_tree(workspace_command.repo().store())
            .has_conflict();
        let plan = make_plan(
            &graph,
            vec![],
            vec![],
            would_rebase_descendants,
            Some(may_conflict),
        );
        write_divergence_plan(ui, &plan, args.json)?;
        return Ok(());
    }

    let mut tx = workspace_command.start_transaction();
    let (solution, num_rebased) = apply_solution(
        author,
        description,
        parents,
        tree,
        change_id.clone(),
        &divergent_commit_ids,
        tx.repo_mut(),
    )
    .await
    .map_err(internal_error)?;

    writeln!(
        ui.status(),
        "Converged change {} into commit {}.",
        short_change_hash(&change_id),
        short_commit_hash(solution.id())
    )?;
    if num_rebased > 0 {
        writeln!(ui.status(), "Rebased {num_rebased} descendant commits.")?;
    }
    if solution.has_conflict() {
        writeln!(
            ui.warning_default(),
            "The converged commit has conflicts. Resolve them with jj resolve."
        )?;
    }
    tx.finish(
        ui,
        format!("converge divergent change {}", change_id.reverse_hex()),
    )
    .await?;
    if args.json {
        write_resolved_divergence(
            ui,
            &AppliedConvergence {
                change_id,
                commit_id: solution.id().clone(),
                rebased_descendants: num_rebased,
                has_conflicts: solution.has_conflict(),
            },
        )?;
    }
    Ok(())
}

fn has_explicit_resolution(args: &ConvergeArgs) -> bool {
    args.onto.is_some()
        || args.author_from.is_some()
        || args.message_from.is_some()
        || args.message_paragraphs.is_some()
}

/// Cleans one bottom-most issue at a time. This is intentionally a small
/// driver around the existing divergence machinery: content conflicts still
/// require a working-copy resolution, so the command reports exact
/// instructions and stops before examining descendants.
async fn converge_cleanup(
    ui: &mut Ui,
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
    args: &ConvergeArgs,
) -> Result<(), CommandError> {
    let issue_expr = cleanup_issue_expression(ui, workspace_command, args).await?;
    let divergent_changes = find_scoped_divergent_changes(ui, workspace_command, args).await?;
    let issues = find_cleanup_issues(workspace_command, issue_expr, &divergent_changes).await?;
    let issue = match bottom_cleanup_issue(workspace_command, &issues, &divergent_changes).await? {
        CleanupSelection::Clean => {
            if args.json {
                writeln!(ui.stdout(), "{{\"status\":\"clean\"}}")?;
            } else {
                writeln!(
                    ui.status(),
                    "No divergent changes or content conflicts found."
                )?;
            }
            return Ok(());
        }
        CleanupSelection::Next(issue) => issue,
        CleanupSelection::Blocked(plan) => {
            write_cleanup_blocked_plan(ui, &plan, args.json)?;
            return if args.dry_run {
                Ok(())
            } else {
                Err(cleanup_cycle_error(&plan))
            };
        }
    };

    match issue {
        CleanupIssue::Conflict(commit) => {
            let plan = make_conflict_plan(&commit).await;
            write_conflict_plan(ui, &plan, args.json)?;
            if args.dry_run {
                Ok(())
            } else {
                Err(manual_conflict_error(&plan))
            }
        }
        CleanupIssue::Divergence(change_id) => {
            let Some(divergent_commits) =
                find_current_divergent_commits(ui, workspace_command, &change_id).await?
            else {
                return Err(internal_error(
                    "selected divergent change disappeared before cleanup",
                ));
            };
            match converge_one_automatically(ui, workspace_command, divergent_commits, args.dry_run)
                .await?
            {
                AutomaticConvergeResult::Plan(plan) => {
                    write_divergence_plan(ui, &plan, args.json)?;
                    if args.dry_run {
                        Ok(())
                    } else {
                        Err(non_interactive_error(&plan))
                    }
                }
                AutomaticConvergeResult::Applied(applied) => {
                    if args.json {
                        write_resolved_divergence(ui, &applied)?;
                    }
                    Ok(())
                }
            }
        }
    }
}

async fn find_scoped_divergent_changes(
    ui: &mut Ui,
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    args: &ConvergeArgs,
) -> Result<CommitsByChangeId, CommandError> {
    let divergent_expr = workspace_command
        .parse_revset(ui, &RevisionArg::from("divergent()".to_owned()))?
        .resolve()?;
    let mut divergent_changes = find_divergent_changes(workspace_command.repo(), divergent_expr)
        .await
        .map_err(internal_error)?;
    if args.revisions.is_empty() {
        return Ok(divergent_changes);
    }
    let scope_expr = workspace_command
        .parse_union_revsets(ui, &args.revisions)?
        .resolve()?
        .ancestors();
    let scoped_commit_ids: HashSet<_> = scope_expr
        .evaluate(workspace_command.repo().as_ref())
        .map_err(internal_error)?
        .stream()
        .try_collect()
        .await
        .map_err(internal_error)?;
    divergent_changes.retain(|_, commits| {
        commits
            .iter()
            .any(|commit| scoped_commit_ids.contains(commit.id()))
    });
    Ok(divergent_changes)
}

async fn cleanup_issue_expression(
    ui: &mut Ui,
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    args: &ConvergeArgs,
) -> Result<std::sync::Arc<jj_lib::revset::ResolvedRevsetExpression>, CommandError> {
    let issues = workspace_command
        .parse_revset(
            ui,
            &RevisionArg::from("divergent() | conflicts()".to_owned()),
        )?
        .resolve()?;
    if args.revisions.is_empty() {
        return Ok(issues);
    }
    let scope = workspace_command
        .parse_union_revsets(ui, &args.revisions)?
        .resolve()?;
    Ok(scope.ancestors().intersection(&issues))
}

async fn find_cleanup_issues(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    issue_expr: std::sync::Arc<jj_lib::revset::ResolvedRevsetExpression>,
    divergent_changes: &CommitsByChangeId,
) -> Result<Vec<CleanupIssue>, CommandError> {
    let divergent_change_ids: HashSet<_> = divergent_changes.keys().cloned().collect();
    let commit_ids: Vec<_> = issue_expr
        .evaluate(workspace_command.repo().as_ref())
        .map_err(internal_error)?
        .stream()
        .try_collect()
        .await
        .map_err(internal_error)?;
    let mut issues = divergent_change_ids
        .iter()
        .cloned()
        .map(CleanupIssue::Divergence)
        .collect_vec();
    for commit_id in commit_ids {
        let commit = workspace_command
            .repo()
            .store()
            .get_commit_async(&commit_id)
            .await?;
        // A conflicted divergent commit must be canonicalized first. Its
        // eventual solution may move or eliminate the conflict.
        if commit.has_conflict() && !divergent_change_ids.contains(commit.change_id()) {
            issues.push(CleanupIssue::Conflict(commit));
        }
    }
    Ok(issues)
}

async fn bottom_cleanup_issue(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    issues: &[CleanupIssue],
    divergent_changes: &CommitsByChangeId,
) -> Result<CleanupSelection, CommandError> {
    if issues.is_empty() {
        return Ok(CleanupSelection::Clean);
    }

    let issues = issues
        .iter()
        .sorted_by_key(|issue| cleanup_issue_sort_key(issue))
        .collect_vec();
    let mut blockers = vec![];
    for (candidate_index, candidate) in issues.iter().enumerate() {
        let mut blocker = None;
        for (other_index, other) in issues.iter().enumerate() {
            if candidate_index == other_index {
                continue;
            }
            if let Some((ancestor_id, descendant_id)) = cleanup_issue_ancestry_witness(
                workspace_command,
                other,
                candidate,
                divergent_changes,
            )
            .await?
            {
                blocker = Some((
                    other_index,
                    CleanupDependency {
                        ancestor: cleanup_dependency_endpoint(other, &ancestor_id),
                        descendant: cleanup_dependency_endpoint(candidate, &descendant_id),
                    },
                ));
                break;
            }
        }
        let Some(blocker) = blocker else {
            return Ok(CleanupSelection::Next((*candidate).clone()));
        };
        blockers.push(blocker);
    }

    // The commit graph is acyclic, but grouping divergent versions into one
    // issue can create a dependency cycle. Every remaining issue has a blocker,
    // so following one blocker per issue must eventually repeat an issue.
    let mut visited = vec![None; issues.len()];
    let mut path = vec![];
    let mut current = 0;
    let cycle_start = loop {
        if let Some(position) = visited[current] {
            break position;
        }
        visited[current] = Some(path.len());
        path.push(current);
        current = blockers[current].0;
    };
    // Blocker traversal follows descendant-to-ancestor edges. Reverse them so
    // the witness follows ancestor-to-descendant edges around the cycle.
    let cycle = path[cycle_start..]
        .iter()
        .rev()
        .map(|&index| blockers[index].1.clone())
        .collect();
    Ok(CleanupSelection::Blocked(CleanupBlockedPlan {
        kind: "ordering",
        status: "blocked",
        reason: "cyclic_issue_dependencies",
        cycle,
    }))
}

fn cleanup_dependency_endpoint(
    issue: &CleanupIssue,
    commit_id: &CommitId,
) -> CleanupDependencyEndpoint {
    let (kind, change_id) = match issue {
        CleanupIssue::Divergence(change_id) => ("divergence", change_id),
        CleanupIssue::Conflict(commit) => ("conflict", commit.change_id()),
    };
    CleanupDependencyEndpoint {
        kind,
        change_id: change_id.reverse_hex(),
        commit_id: commit_id.hex(),
    }
}

fn cleanup_issue_sort_key(issue: &CleanupIssue) -> String {
    match issue {
        CleanupIssue::Divergence(change_id) => format!("d:{}", change_id.hex()),
        CleanupIssue::Conflict(commit) => format!("c:{}", commit.id().hex()),
    }
}

async fn cleanup_issue_ancestry_witness(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    ancestor: &CleanupIssue,
    descendant: &CleanupIssue,
    divergent_changes: &CommitsByChangeId,
) -> Result<Option<(CommitId, CommitId)>, CommandError> {
    for ancestor_id in cleanup_issue_commit_ids(ancestor, divergent_changes) {
        for descendant_id in cleanup_issue_commit_ids(descendant, divergent_changes) {
            if ancestor_id == descendant_id {
                continue;
            }
            if workspace_command
                .repo()
                .index()
                .is_ancestor(&ancestor_id, &descendant_id)
                .await
                .map_err(internal_error)?
            {
                return Ok(Some((ancestor_id, descendant_id)));
            }
        }
    }
    Ok(None)
}

fn cleanup_issue_commit_ids(
    issue: &CleanupIssue,
    divergent_changes: &CommitsByChangeId,
) -> Vec<CommitId> {
    match issue {
        CleanupIssue::Divergence(change_id) => divergent_changes[change_id]
            .iter()
            .map(|commit| commit.id().clone())
            .sorted()
            .collect(),
        CleanupIssue::Conflict(commit) => vec![commit.id().clone()],
    }
}

async fn converge_multiple_changes(
    ui: &mut Ui,
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
    args: &ConvergeArgs,
    divergent_changes: CommitsByChangeId,
) -> Result<(), CommandError> {
    if args.interactive {
        return Err(multiple_changes_option_error(
            &divergent_changes,
            "--interactive",
        ));
    }
    if args.onto.is_some() {
        return Err(multiple_changes_option_error(&divergent_changes, "--onto"));
    }
    if args.author_from.is_some() {
        return Err(multiple_changes_option_error(
            &divergent_changes,
            "--author-from",
        ));
    }
    if args.message_from.is_some() {
        return Err(multiple_changes_option_error(
            &divergent_changes,
            "--message-from",
        ));
    }
    if args.message_paragraphs.is_some() {
        return Err(multiple_changes_option_error(
            &divergent_changes,
            "--message",
        ));
    }

    // Keep the original selection stable across rewrites. Process only the
    // current bottom layer so descendant plans are not computed against parent
    // divergence that an earlier layer may resolve.
    let mut remaining_change_ids = divergent_changes.keys().cloned().collect_vec();
    remaining_change_ids.sort_by_key(|change_id| change_id.hex());
    let mut plans = vec![];
    while !remaining_change_ids.is_empty() {
        let bottom_layer = bottom_divergent_change_ids(
            workspace_command,
            &divergent_changes,
            &remaining_change_ids,
        )
        .await?;
        if bottom_layer.is_empty() {
            let mut err = user_error("Cannot order selected divergent changes for convergence");
            err.add_hint("Narrow -r to one change with 'change_id(<change-id>)'.");
            return Err(err);
        }

        for change_id in &bottom_layer {
            remove_change_id(&mut remaining_change_ids, change_id);
            let Some(divergent_commits) =
                find_current_divergent_commits(ui, workspace_command, change_id).await?
            else {
                continue;
            };
            match converge_one_automatically(ui, workspace_command, divergent_commits, args.dry_run)
                .await?
            {
                AutomaticConvergeResult::Plan(plan) => plans.push(plan),
                AutomaticConvergeResult::Applied(applied) => {
                    if applied.has_conflicts {
                        return Ok(());
                    }
                }
            }
        }

        if args.dry_run {
            discard_descendant_changes(
                workspace_command,
                &divergent_changes,
                &mut remaining_change_ids,
                &bottom_layer,
            )
            .await?;
            break;
        }

        let unresolved_change_ids = plans
            .iter()
            .filter(|plan| {
                bottom_layer
                    .iter()
                    .any(|change_id| plan.change_id == change_id.reverse_hex())
            })
            .map(|plan| plan.change_id.clone())
            .collect_vec();
        if !unresolved_change_ids.is_empty() {
            let unresolved_change_ids = bottom_layer
                .iter()
                .filter(|change_id| unresolved_change_ids.contains(&change_id.reverse_hex()))
                .cloned()
                .collect_vec();
            discard_descendant_changes(
                workspace_command,
                &divergent_changes,
                &mut remaining_change_ids,
                &unresolved_change_ids,
            )
            .await?;
        }
    }

    if args.dry_run {
        write_plans(ui, &plans, args.json)?;
        return Ok(());
    }
    if !plans.is_empty() {
        for plan in &plans {
            write_plan(ui, plan, false)?;
        }
        return Err(multiple_changes_non_interactive_error(&plans));
    }
    Ok(())
}

fn multiple_changes_option_error(
    divergent_changes: &CommitsByChangeId,
    option: &str,
) -> CommandError {
    let mut changes = divergent_changes
        .keys()
        .map(short_change_hash)
        .sorted()
        .collect_vec();
    changes.truncate(5);
    let mut err = user_error(format!(
        "{option} cannot be used with multiple divergent changes"
    ));
    err.add_hint("Narrow -r to one change with 'change_id(<change-id>)'.");
    err.add_hint(format!("Matching change IDs: {}", changes.join(", ")));
    err
}

async fn bottom_divergent_change_ids(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    divergent_changes: &CommitsByChangeId,
    remaining_change_ids: &[ChangeId],
) -> Result<Vec<ChangeId>, CommandError> {
    let mut bottom_layer = vec![];
    for candidate_change_id in remaining_change_ids {
        let mut has_selected_ancestor = false;
        for other_change_id in remaining_change_ids {
            if candidate_change_id == other_change_id {
                continue;
            }
            if change_id_is_ancestor(
                workspace_command,
                divergent_changes,
                other_change_id,
                candidate_change_id,
            )
            .await?
            {
                has_selected_ancestor = true;
                break;
            }
        }
        if !has_selected_ancestor {
            bottom_layer.push(candidate_change_id.clone());
        }
    }
    bottom_layer.sort_by_key(|change_id| change_id.hex());
    Ok(bottom_layer)
}

async fn change_id_is_ancestor(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    divergent_changes: &CommitsByChangeId,
    ancestor_change_id: &ChangeId,
    descendant_change_id: &ChangeId,
) -> Result<bool, CommandError> {
    for ancestor_commit in &divergent_changes[ancestor_change_id] {
        for descendant_commit in &divergent_changes[descendant_change_id] {
            if workspace_command
                .repo()
                .index()
                .is_ancestor(ancestor_commit.id(), descendant_commit.id())
                .await
                .map_err(internal_error)?
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn discard_descendant_changes(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    divergent_changes: &CommitsByChangeId,
    remaining_change_ids: &mut Vec<ChangeId>,
    ancestor_change_ids: &[ChangeId],
) -> Result<(), CommandError> {
    let mut deferred_change_ids = vec![];
    for candidate_change_id in remaining_change_ids.iter() {
        let mut blocking_ancestors = vec![];
        for ancestor_change_id in ancestor_change_ids {
            if change_id_is_ancestor(
                workspace_command,
                divergent_changes,
                ancestor_change_id,
                candidate_change_id,
            )
            .await?
            {
                blocking_ancestors.push(ancestor_change_id.reverse_hex());
            }
        }
        if !blocking_ancestors.is_empty() {
            deferred_change_ids.push(candidate_change_id.clone());
        }
    }
    for change_id in deferred_change_ids {
        remove_change_id(remaining_change_ids, &change_id);
    }
    Ok(())
}

fn remove_change_id(change_ids: &mut Vec<ChangeId>, to_remove: &ChangeId) {
    change_ids.retain(|change_id| change_id != to_remove);
}

async fn find_current_divergent_commits(
    ui: &mut Ui,
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    change_id: &ChangeId,
) -> Result<Option<Vec<Commit>>, CommandError> {
    let revision = RevisionArg::from(format!("change_id({})", change_id.reverse_hex()));
    let target_expr = workspace_command.parse_revset(ui, &revision)?.resolve()?;
    let mut divergent_changes = find_divergent_changes(workspace_command.repo(), target_expr)
        .await
        .map_err(internal_error)?;
    Ok(divergent_changes.remove(change_id))
}

/// Converges one change without prompting or explicit attribute overrides.
///
/// Returns a plan when the change is not safe to converge automatically, or
/// when dry_run requests the plan instead of applying it. Otherwise returns
/// the applied solution so JSON callers can report a stable outcome.
async fn converge_one_automatically(
    ui: &mut Ui,
    workspace_command: &mut crate::cli_util::WorkspaceCommandHelper,
    divergent_commits: Vec<Commit>,
    dry_run: bool,
) -> Result<AutomaticConvergeResult, CommandError> {
    let divergent_commit_ids = divergent_commits
        .iter()
        .map(|commit| commit.id().clone())
        .collect_vec();
    let divergent_commit_id_set: HashSet<_> = divergent_commit_ids.iter().cloned().collect();
    let to_rewrite = RevsetExpression::commits(divergent_commit_ids.clone()).descendants();
    let rewrite_commit_ids: Vec<_> = to_rewrite
        .clone()
        .evaluate(workspace_command.repo().as_ref())
        .map_err(internal_error)?
        .stream()
        .try_collect()
        .await
        .map_err(internal_error)?;
    let would_rebase_descendants = rewrite_commit_ids
        .iter()
        .filter(|commit_id| !divergent_commit_id_set.contains(*commit_id))
        .count();

    let graph = TruncatedEvolutionGraph::new(workspace_command.repo().clone(), divergent_commits)
        .await
        .map_err(internal_error)?;
    let proposed = converge_change(&graph, None, None, None, None)
        .await
        .map_err(internal_error)?;
    let inferred_author = match &proposed.author {
        ConvergedAttribute::Solved(_) => None,
        ConvergedAttribute::Unsolved { base_commit, .. } => {
            infer_author_without_prompt(&graph, base_commit).await?
        }
    };
    let unresolved = collect_unresolved_choices(
        workspace_command,
        &graph,
        &proposed,
        inferred_author.as_ref(),
    )
    .await?;

    if workspace_command
        .check_rewritable_expr(&to_rewrite)
        .await
        .is_err()
    {
        return Ok(AutomaticConvergeResult::Plan(make_plan(
            &graph,
            unresolved,
            vec!["immutable commit in rewrite set".to_owned()],
            would_rebase_descendants,
            None,
        )));
    }
    if !unresolved.is_empty() {
        return Ok(AutomaticConvergeResult::Plan(make_plan(
            &graph,
            unresolved,
            vec![],
            would_rebase_descendants,
            None,
        )));
    }

    let author = match proposed.author {
        ConvergedAttribute::Solved(author) => author,
        ConvergedAttribute::Unsolved { .. } => {
            inferred_author.ok_or_else(|| internal_error("converge did not infer an author"))?
        }
    };
    let description = match proposed.description {
        ConvergedAttribute::Solved(description) => description,
        ConvergedAttribute::Unsolved { .. } => {
            return Err(internal_error("converge left description unresolved"));
        }
    };
    let parents = match proposed.parents {
        ConvergedAttribute::Solved(parents) => parents,
        ConvergedAttribute::Unsolved { .. } => {
            return Err(internal_error("converge left parents unresolved"));
        }
    };
    let parent_id = require_linear_parent(&parents)?;
    validate_solution_parent(workspace_command, &graph, parent_id).await?;
    let solved = converge_change(
        &graph,
        Some(author.clone()),
        Some(description.clone()),
        Some(parents.clone()),
        None,
    )
    .await
    .map_err(internal_error)?;
    let tree = solved
        .tree
        .ok_or_else(|| internal_error("converge did not produce a solution tree"))?;
    let may_conflict = tree
        .to_merged_tree(workspace_command.repo().store())
        .has_conflict();
    let plan = make_plan(
        &graph,
        vec![],
        vec![],
        would_rebase_descendants,
        Some(may_conflict),
    );
    if dry_run {
        return Ok(AutomaticConvergeResult::Plan(plan));
    }

    let change_id = graph.change_id().clone();
    let mut tx = workspace_command.start_transaction();
    let (solution, num_rebased) = apply_solution(
        author,
        description,
        parents,
        tree,
        change_id.clone(),
        &divergent_commit_ids,
        tx.repo_mut(),
    )
    .await
    .map_err(internal_error)?;
    writeln!(
        ui.status(),
        "Converged change {} into commit {}.",
        short_change_hash(&change_id),
        short_commit_hash(solution.id())
    )?;
    if num_rebased > 0 {
        writeln!(ui.status(), "Rebased {num_rebased} descendant commits.")?;
    }
    if solution.has_conflict() {
        writeln!(
            ui.warning_default(),
            "The converged commit has conflicts. Resolve them with jj resolve."
        )?;
    }
    tx.finish(
        ui,
        format!("converge divergent change {}", change_id.reverse_hex()),
    )
    .await?;
    Ok(AutomaticConvergeResult::Applied(AppliedConvergence {
        change_id,
        commit_id: solution.id().clone(),
        rebased_descendants: num_rebased,
        has_conflicts: solution.has_conflict(),
    }))
}

async fn resolve_divergent_source_commit(
    ui: &mut Ui,
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    revision: &RevisionArg,
    divergent_commit_ids: &HashSet<CommitId>,
    option: &str,
) -> Result<Commit, CommandError> {
    let commit = workspace_command.resolve_single_rev(ui, revision).await?;
    if !divergent_commit_ids.contains(commit.id()) {
        let mut err = user_error(format!(
            "{option} must select one of the divergent revisions"
        ));
        err.add_hint("Use an exact commit ID reported by jj converge --dry-run --json.");
        return Err(err);
    }
    Ok(commit)
}

async fn collect_unresolved_choices(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    graph: &TruncatedEvolutionGraph,
    proposed: &ConvergeResult,
    inferred_author: Option<&Signature>,
) -> Result<Vec<UnresolvedChoice>, CommandError> {
    let mut unresolved = vec![];
    if inferred_author.is_none() && matches!(proposed.author, ConvergedAttribute::Unsolved { .. }) {
        unresolved.push(UnresolvedChoice {
            kind: "author",
            resolve_with: "--author-from <commit-id>",
            candidates: graph
                .divergent_commits()
                .iter()
                .map(|commit| ChoiceCandidate {
                    commit_id: commit.id().hex(),
                    description: None,
                    author: Some(format!(
                        "{} <{}>",
                        commit.author().name,
                        commit.author().email
                    )),
                })
                .collect(),
        });
    }
    if matches!(proposed.description, ConvergedAttribute::Unsolved { .. }) {
        unresolved.push(UnresolvedChoice {
            kind: "description",
            resolve_with: "--message-from <commit-id> or -m <message>",
            candidates: graph
                .divergent_commits()
                .iter()
                .map(|commit| ChoiceCandidate {
                    commit_id: commit.id().hex(),
                    description: Some(commit.description().to_owned()),
                    author: None,
                })
                .collect(),
        });
    }
    match &proposed.parents {
        ConvergedAttribute::Unsolved {
            excluded_divergent_commits,
            ..
        } => {
            unresolved.push(
                make_parent_choice(workspace_command, graph, excluded_divergent_commits).await?,
            );
        }
        ConvergedAttribute::Solved(parents) if parents.len() > 1 => {
            unresolved.push(make_parent_choice(workspace_command, graph, &HashSet::new()).await?);
        }
        _ => {}
    }
    Ok(unresolved)
}

async fn make_parent_choice(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    graph: &TruncatedEvolutionGraph,
    excluded_divergent_commits: &HashSet<CommitId>,
) -> Result<UnresolvedChoice, CommandError> {
    let mut candidates = vec![];
    for parent_id in linear_parent_candidate_ids(graph, excluded_divergent_commits) {
        let parent = workspace_command
            .repo()
            .store()
            .get_commit_async(&parent_id)
            .await?;
        candidates.push(ChoiceCandidate {
            commit_id: parent_id.hex(),
            description: Some(parent.description().lines().next().unwrap_or("").to_owned()),
            author: None,
        });
    }
    Ok(UnresolvedChoice {
        kind: "parent",
        resolve_with: "--onto <commit-id>",
        candidates,
    })
}

fn make_plan(
    graph: &TruncatedEvolutionGraph,
    unresolved: Vec<UnresolvedChoice>,
    blocked: Vec<String>,
    would_rebase_descendants: usize,
    may_conflict: Option<bool>,
) -> ConvergePlan {
    ConvergePlan {
        change_id: graph.change_id().reverse_hex(),
        divergent_commits: graph
            .divergent_commits()
            .iter()
            .map(|commit| commit.id().hex())
            .collect(),
        ready: unresolved.is_empty() && blocked.is_empty(),
        unresolved,
        blocked,
        would_rebase_descendants: Some(would_rebase_descendants),
        may_conflict,
    }
}

async fn make_conflict_plan(commit: &Commit) -> ConflictPlan {
    let mut paths = commit
        .tree()
        .conflicts_stream()
        .map(|(path, _)| path.as_internal_file_string().to_owned())
        .collect::<Vec<_>>()
        .await;
    paths.sort();
    let commit_id = commit.id().hex();
    ConflictPlan {
        kind: "conflict",
        status: "needs_manual_resolution",
        commit_id: commit_id.clone(),
        change_id: commit.change_id().reverse_hex(),
        paths,
        resolve_with: ConflictResolutionInstructions {
            start: format!("jj new {commit_id}"),
            inspect: "jj resolve --list",
            finish: "jj squash --use-destination-message",
        },
    }
}

fn divergence_status(plan: &ConvergePlan) -> &'static str {
    if !plan.blocked.is_empty() {
        "blocked"
    } else if !plan.unresolved.is_empty() {
        "needs_input"
    } else {
        "ready"
    }
}

fn write_divergence_plan(ui: &mut Ui, plan: &ConvergePlan, json: bool) -> Result<(), CommandError> {
    if !json {
        return write_plan(ui, plan, false);
    }
    let mut value = serde_json::to_value(plan).map_err(internal_error)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| internal_error("convergence plan did not serialize as an object"))?;
    object.insert("kind".to_owned(), "divergence".into());
    object.insert("status".to_owned(), divergence_status(plan).into());
    let output = serde_json::to_string_pretty(&value).map_err(internal_error)?;
    writeln!(ui.stdout(), "{output}")?;
    Ok(())
}

fn write_conflict_plan(ui: &mut Ui, plan: &ConflictPlan, json: bool) -> Result<(), CommandError> {
    if json {
        let output = serde_json::to_string_pretty(plan).map_err(internal_error)?;
        writeln!(ui.stdout(), "{output}")?;
        return Ok(());
    }
    writeln!(ui.status(), "{}  CONFLICT", plan.change_id)?;
    writeln!(ui.status(), "  commit: {}", plan.commit_id)?;
    if !plan.paths.is_empty() {
        writeln!(ui.status(), "  paths:")?;
        for path in &plan.paths {
            writeln!(ui.status(), "    {path}")?;
        }
    }
    writeln!(ui.status(), "  resolve:")?;
    writeln!(ui.status(), "    start  : {}", plan.resolve_with.start)?;
    writeln!(ui.status(), "    inspect: {}", plan.resolve_with.inspect)?;
    writeln!(ui.status(), "    finish : {}", plan.resolve_with.finish)?;
    Ok(())
}

fn write_cleanup_blocked_plan(
    ui: &mut Ui,
    plan: &CleanupBlockedPlan,
    json: bool,
) -> Result<(), CommandError> {
    if json {
        let output = serde_json::to_string_pretty(plan).map_err(internal_error)?;
        writeln!(ui.stdout(), "{output}")?;
        return Ok(());
    }
    writeln!(
        ui.status(),
        "Cannot order cleanup issues: cyclic dependencies"
    )?;
    for dependency in &plan.cycle {
        let ancestor = &dependency.ancestor;
        let descendant = &dependency.descendant;
        writeln!(
            ui.status(),
            "  {} {} (commit {}) is an ancestor of {} {} (commit {})",
            ancestor.kind,
            ancestor.change_id,
            ancestor.commit_id,
            descendant.kind,
            descendant.change_id,
            descendant.commit_id,
        )?;
    }
    writeln!(
        ui.status(),
        "Inspect these exact commits and choose the intended stack order."
    )?;
    writeln!(ui.status(), "{}", cleanup_cycle_resolution_hint(plan))?;
    Ok(())
}

fn cleanup_cycle_resolution_hint(plan: &CleanupBlockedPlan) -> String {
    // A cycle must contain divergence: ancestry between individual conflicted
    // commits alone cannot cycle in a valid commit graph.
    let divergence = plan
        .cycle
        .iter()
        .map(|dependency| &dependency.ancestor)
        .find(|endpoint| endpoint.kind == "divergence")
        .expect("a cleanup dependency cycle must contain a divergent change");
    format!(
        "Resolve one change explicitly with jj converge -r 'change_id({})' --interactive, or \
         supply --onto <chosen-parent> and any other required resolution options.",
        divergence.change_id,
    )
}

fn cleanup_cycle_error(plan: &CleanupBlockedPlan) -> CommandError {
    let mut err = user_error("Cannot order cleanup issues: cyclic dependencies");
    err.add_hint(
        "Divergent versions impose conflicting cleanup orders; inspect the exact commits in the \
         cycle.",
    );
    err.add_hint(cleanup_cycle_resolution_hint(plan));
    err
}

fn write_resolved_divergence(
    ui: &mut Ui,
    applied: &AppliedConvergence,
) -> Result<(), CommandError> {
    let result = ResolvedDivergence {
        kind: "divergence",
        status: "resolved",
        change_id: applied.change_id.reverse_hex(),
        commit_id: applied.commit_id.hex(),
        rebased_descendants: applied.rebased_descendants,
        has_conflicts: applied.has_conflicts,
    };
    let output = serde_json::to_string_pretty(&result).map_err(internal_error)?;
    writeln!(ui.stdout(), "{output}")?;
    Ok(())
}

fn manual_conflict_error(plan: &ConflictPlan) -> CommandError {
    let mut err = user_error("Content conflict requires manual resolution");
    err.add_hint(format!("Start with: {}", plan.resolve_with.start));
    err.add_hint(
        "Then run jj resolve --list, resolve the files, and squash with --use-destination-message.",
    );
    err
}

fn write_plan(ui: &mut Ui, plan: &ConvergePlan, json: bool) -> Result<(), CommandError> {
    if json {
        let output = serde_json::to_string_pretty(plan).map_err(internal_error)?;
        writeln!(ui.stdout(), "{output}")?;
        return Ok(());
    }

    let status = if !plan.blocked.is_empty() {
        "BLOCKED"
    } else if !plan.unresolved.is_empty() {
        "NEEDS_INPUT"
    } else {
        "READY"
    };
    writeln!(ui.status(), "{}  {status}", plan.change_id)?;
    if let Some(would_rebase_descendants) = plan.would_rebase_descendants {
        writeln!(
            ui.status(),
            "  rewrites: {would_rebase_descendants} descendants"
        )?;
    }
    if plan.ready {
        if let Some(may_conflict) = plan.may_conflict {
            writeln!(ui.status(), "  conflicts: {may_conflict}")?;
        }
    } else {
        for blocker in &plan.blocked {
            writeln!(ui.status(), "  blocker: {blocker}")?;
        }
        if !plan.unresolved.is_empty() {
            writeln!(ui.status(), "  needs:")?;
        }
        for choice in &plan.unresolved {
            writeln!(
                ui.status(),
                "    {:<11}: {}",
                choice.kind,
                choice.resolve_with
            )?;
        }
    }
    let inspect_commands = plan
        .unresolved
        .iter()
        .filter_map(inspect_choice_command)
        .collect_vec();
    if !inspect_commands.is_empty() {
        writeln!(ui.status(), "  inspect:")?;
        for (kind, command) in inspect_commands {
            writeln!(ui.status(), "    {kind:<11}: {command}")?;
        }
    }
    Ok(())
}

fn inspect_choice_command(choice: &UnresolvedChoice) -> Option<(&'static str, String)> {
    let revset = choice
        .candidates
        .iter()
        .map(|candidate| format!("commit_id({})", candidate.commit_id))
        .join(" | ");
    if revset.is_empty() {
        return None;
    }
    match choice.kind {
        "description" => Some((
            "descriptions",
            format!(
                "jj log -r '{revset}' --no-graph -T 'commit_id ++ \"\\n\" ++ description ++ \
                 \"\\n\"'"
            ),
        )),
        "parent" => Some((
            "parents",
            format!(
                "jj log -r '{revset}' --no-graph -T 'commit_id ++ \" \" ++ change_id ++ \"\\n\" \
                 ++ description ++ \"\\n\"'"
            ),
        )),
        _ => None,
    }
}

fn write_plans(ui: &mut Ui, plans: &[ConvergePlan], json: bool) -> Result<(), CommandError> {
    if json {
        let output = serde_json::to_string_pretty(plans).map_err(internal_error)?;
        writeln!(ui.stdout(), "{output}")?;
        return Ok(());
    }
    for plan in plans {
        write_plan(ui, plan, false)?;
    }
    Ok(())
}

fn non_interactive_error(plan: &ConvergePlan) -> CommandError {
    let mut err = user_error("Cannot converge non-interactively");
    for choice in &plan.unresolved {
        let candidates = choice
            .candidates
            .iter()
            .map(|candidate| candidate.commit_id.as_str())
            .join(", ");
        let candidate_hint = if candidates.is_empty() {
            "No valid candidates were inferred.".to_owned()
        } else {
            format!("Candidates: {candidates}.")
        };
        err.add_hint(format!(
            "Unresolved {}: use {}. {candidate_hint}",
            choice.kind, choice.resolve_with
        ));
    }
    err.add_hint("Run with --dry-run --json for a machine-readable plan.");
    err.add_hint("Run with -i to choose interactively.");
    err
}

fn multiple_changes_non_interactive_error(plans: &[ConvergePlan]) -> CommandError {
    let mut err = user_error(format!(
        "Could not converge {} selected changes non-interactively",
        plans.len()
    ));
    for plan in plans {
        for blocker in &plan.blocked {
            err.add_hint(format!("Change {} is blocked: {blocker}", plan.change_id));
        }
        for choice in &plan.unresolved {
            let candidates = choice
                .candidates
                .iter()
                .map(|candidate| candidate.commit_id.as_str())
                .join(", ");
            let candidate_hint = if candidates.is_empty() {
                "No valid candidates were inferred.".to_owned()
            } else {
                format!("Candidates: {candidates}.")
            };
            err.add_hint(format!(
                "Change {} has unresolved {}: use {}. {candidate_hint}",
                plan.change_id, choice.kind, choice.resolve_with
            ));
        }
    }
    err.add_hint("All selected changes that were ready were converged.");
    err.add_hint("Narrow -r to one change to supply resolution options.");
    err
}

fn linear_parent_candidate_ids(
    graph: &TruncatedEvolutionGraph,
    excluded_divergent_commits: &HashSet<CommitId>,
) -> Vec<CommitId> {
    graph
        .divergent_commits()
        .iter()
        .filter(|commit| !excluded_divergent_commits.contains(commit.id()))
        .flat_map(|commit| commit.parent_ids().iter().cloned())
        .unique()
        .sorted_by_key(|parent| parent.hex())
        .collect()
}

async fn choose_author(
    ui: &Ui,
    graph: &TruncatedEvolutionGraph,
    base_commit_id: &CommitId,
) -> Result<Signature, CommandError> {
    if let Some(author) = infer_author_without_prompt(graph, base_commit_id).await? {
        return Ok(author);
    }

    let authors = graph
        .divergent_commits()
        .iter()
        .map(|commit| commit.author().clone())
        .unique()
        .collect_vec();
    writeln!(
        ui.stderr(),
        "Ambiguous author for converged change, choose one:"
    )?;
    let mut choices = vec![];
    for (index, author) in authors.iter().enumerate() {
        writeln!(
            ui.stderr(),
            "{}: {} <{}>",
            index + 1,
            author.name,
            author.email
        )?;
        choices.push((index + 1).to_string());
    }
    writeln!(ui.stderr(), "q: quit the prompt")?;
    choices.push("q".to_string());
    let index = ui.prompt_choice("enter the index of the author to use", &choices, None)?;
    authors
        .get(index)
        .cloned()
        .ok_or_else(|| user_error("No author selected"))
}

async fn infer_author_without_prompt(
    graph: &TruncatedEvolutionGraph,
    base_commit_id: &CommitId,
) -> Result<Option<Signature>, CommandError> {
    let authors = graph
        .divergent_commits()
        .iter()
        .map(|commit| commit.author().clone())
        .unique()
        .collect_vec();
    if let [author] = &authors[..] {
        return Ok(Some(author.clone()));
    }
    if authors
        .iter()
        .map(|author| (&author.name, &author.email))
        .all_equal()
    {
        let base = graph
            .repo()
            .store()
            .get_commit_async(base_commit_id)
            .await?;
        let first = &authors[0];
        if base.author().name == first.name && base.author().email == first.email {
            return Ok(Some(base.author().clone()));
        }
        return Ok(Some(
            authors
                .into_iter()
                .min_by_key(|author| author.timestamp)
                .unwrap(),
        ));
    }
    Ok(None)
}

async fn choose_linear_parent(
    ui: &Ui,
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    graph: &TruncatedEvolutionGraph,
    excluded_divergent_commits: &HashSet<CommitId>,
) -> Result<Vec<CommitId>, CommandError> {
    let candidates = linear_parent_candidate_ids(graph, excluded_divergent_commits);
    match candidates.as_slice() {
        [] => {
            return Err(user_error(
                "Cannot infer a single canonical parent for the converged change",
            )
            .hinted("Use --onto <revision> to choose one parent explicitly."));
        }
        [parent] => return Ok(vec![parent.clone()]),
        _ => {}
    }

    writeln!(
        ui.stderr(),
        "Ambiguous canonical parent for converged change, choose one:"
    )?;
    let mut choices = vec![];
    for (index, parent_id) in candidates.iter().enumerate() {
        let parent = workspace_command
            .repo()
            .store()
            .get_commit_async(parent_id)
            .await?;
        write!(ui.stderr(), "{}: ", index + 1)?;
        let mut formatter = ui.stderr_formatter();
        workspace_command.write_commit_summary(formatter.as_mut(), &parent)?;
        writeln!(formatter)?;
        choices.push((index + 1).to_string());
    }
    writeln!(ui.stderr(), "q: quit the prompt")?;
    choices.push("q".to_string());
    let index = ui.prompt_choice("enter the index of the parent to use", &choices, None)?;
    candidates
        .get(index)
        .map(|parent| vec![parent.clone()])
        .ok_or_else(|| user_error("No canonical parent selected"))
}

fn require_linear_parent(parents: &[CommitId]) -> Result<&CommitId, CommandError> {
    match parents {
        [parent] => Ok(parent),
        [] => Err(user_error(
            "Converging the root commit is not supported because it has no parent",
        )),
        _ => Err(
            user_error("Refusing to create a converged commit with multiple parents")
                .hinted("Use --onto <revision> to choose one canonical parent."),
        ),
    }
}

async fn validate_solution_parent(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    graph: &TruncatedEvolutionGraph,
    parent_id: &CommitId,
) -> Result<(), CommandError> {
    let repo = workspace_command.repo();
    let parent = repo.store().get_commit_async(parent_id).await?;
    if parent.change_id() == graph.change_id() {
        return Err(user_error(
            "The canonical parent cannot have the divergent change ID",
        ));
    }
    if repo
        .resolve_change_id(parent.change_id())
        .await?
        .is_none_or(|targets| !targets.has_visible(parent.id()))
    {
        return Err(user_error("The canonical parent must be visible"));
    }
    for divergent_commit_id in graph.divergent_commit_ids() {
        if divergent_commit_id == parent_id
            || repo
                .index()
                .is_ancestor(divergent_commit_id, parent_id)
                .await?
        {
            return Err(user_error(format!(
                "Cannot converge onto descendant {}",
                short_commit_hash(parent_id)
            )));
        }
    }
    Ok(())
}

async fn edit_merged_description(
    workspace_command: &crate::cli_util::WorkspaceCommandHelper,
    graph: &TruncatedEvolutionGraph,
    base_commit_id: &CommitId,
    excluded_divergent_commits: &HashSet<CommitId>,
) -> Result<String, CommandError> {
    let base = workspace_command
        .repo()
        .store()
        .get_commit_async(base_commit_id)
        .await?;
    let mut template = String::from(
        "JJ: Resolve the divergent descriptions below into one description.\nJJ: Remove every \
         conflict-marker line before saving.\n",
    );
    template.push_str("<<<<<<< divergent descriptions\n");
    writeln!(
        template,
        "%%%%%%% base {}",
        short_commit_hash(base_commit_id)
    )
    .unwrap();
    push_description(&mut template, base.description());
    for commit in graph
        .divergent_commits()
        .iter()
        .filter(|commit| !excluded_divergent_commits.contains(commit.id()))
    {
        writeln!(
            template,
            "+++++++ version {}",
            short_commit_hash(commit.id())
        )
        .unwrap();
        push_description(&mut template, commit.description());
    }
    template.push_str(">>>>>>>\n");

    let editor = workspace_command.text_editor()?;
    let description = edit_description(&editor, &template)?;
    if description.lines().any(is_description_conflict_marker) {
        return Err(user_error(
            "Description still contains converge conflict markers",
        ));
    }
    Ok(description)
}

fn push_description(output: &mut String, description: &str) {
    output.push_str(description);
    if !description.ends_with('\n') {
        output.push('\n');
    }
}

fn is_description_conflict_marker(line: &str) -> bool {
    ["<<<<<<<", "%%%%%%%", "+++++++", ">>>>>>>"]
        .iter()
        .any(|marker| line.starts_with(marker))
}
