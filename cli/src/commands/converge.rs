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
use std::io::Write as _;

use clap_complete::ArgValueCompleter;
use futures::TryStreamExt as _;
use itertools::Itertools as _;
use jj_lib::backend::CommitId;
use jj_lib::backend::Signature;
use jj_lib::commit::Commit;
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

/// Converge divergent versions of a change into one canonical revision
///
/// The divergent revisions become evolution predecessors of the new revision.
/// They do not become commit-graph parents. The solution always has exactly one
/// commit-graph parent; use --onto to choose that parent when it cannot be
/// inferred automatically. By default, the command never prompts or opens an
/// editor; use --interactive to opt into that behavior.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct ConvergeArgs {
    /// Revisions containing the divergent change to converge
    ///
    /// The revset must identify exactly one divergent change ID. If omitted,
    /// all divergent revisions are considered, which succeeds only if the
    /// repository has exactly one divergent change.
    #[arg(long = "revision", short, value_name = "REVSETS")]
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

    /// Report the convergence plan without changing history
    #[arg(long)]
    dry_run: bool,

    /// Emit the dry-run convergence plan as JSON
    #[arg(long, requires = "dry_run")]
    json: bool,
}

#[derive(Debug, Serialize)]
struct ConvergePlan {
    change_id: String,
    divergent_commits: Vec<String>,
    ready: bool,
    unresolved: Vec<UnresolvedChoice>,
    blocked: Vec<String>,
    would_rebase_descendants: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    may_conflict: Option<bool>,
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
    let target_expr = if args.revisions.is_empty() {
        workspace_command.parse_revset(ui, &RevisionArg::from("divergent()".to_owned()))?
    } else {
        workspace_command.parse_union_revsets(ui, &args.revisions)?
    }
    .resolve()?;

    let divergent_changes = find_divergent_changes(workspace_command.repo(), target_expr)
        .await
        .map_err(internal_error)?;
    if divergent_changes.is_empty() {
        return Err(user_error("No divergent changes found"));
    }
    if divergent_changes.len() > 1 {
        let mut changes = divergent_changes
            .keys()
            .map(short_change_hash)
            .sorted()
            .collect_vec();
        changes.truncate(5);
        let mut err = user_error("Revset resolved to multiple divergent changes");
        err.add_hint("Select one change with jj converge -r 'change_id(<change-id>)'.");
        err.add_hint(format!("Matching change IDs: {}", changes.join(", ")));
        return Err(err);
    }

    let (_change_id, commits_by_id) = divergent_changes.into_iter().next().unwrap();
    let mut divergent_commits = commits_by_id.into_values().collect_vec();
    divergent_commits.sort_by_key(|commit| commit.id().hex());
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
        if args.dry_run {
            let plan = make_plan(
                &graph,
                unresolved,
                vec!["One or more commits that would be rewritten are immutable.".to_owned()],
                would_rebase_descendants,
                None,
            );
            write_plan(ui, &plan, args.json)?;
            return Ok(());
        }
        return Err(error);
    }

    if !args.interactive && !unresolved.is_empty() {
        let plan = make_plan(&graph, unresolved, vec![], would_rebase_descendants, None);
        if args.dry_run {
            write_plan(ui, &plan, args.json)?;
            return Ok(());
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
        write_plan(ui, &plan, args.json)?;
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
    Ok(())
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
        would_rebase_descendants,
        may_conflict,
    }
}

fn write_plan(ui: &mut Ui, plan: &ConvergePlan, json: bool) -> Result<(), CommandError> {
    if json {
        let output = serde_json::to_string_pretty(plan).map_err(internal_error)?;
        writeln!(ui.stdout(), "{output}")?;
        return Ok(());
    }

    writeln!(ui.status(), "Converge plan for change {}:", plan.change_id)?;
    if plan.ready {
        writeln!(ui.status(), "Ready to converge without interactive input.")?;
        if let Some(may_conflict) = plan.may_conflict {
            writeln!(ui.status(), "May contain conflicts: {may_conflict}")?;
        }
    } else {
        writeln!(
            ui.status(),
            "Not ready to converge without interactive input."
        )?;
        for blocker in &plan.blocked {
            writeln!(ui.status(), "Blocked: {blocker}")?;
        }
        for choice in &plan.unresolved {
            writeln!(
                ui.status(),
                "Unresolved {}: use {}.",
                choice.kind,
                choice.resolve_with
            )?;
        }
    }
    writeln!(
        ui.status(),
        "Would rebase {} descendant commits.",
        plan.would_rebase_descendants
    )?;
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
    if parent.is_hidden(repo.as_ref()).await? {
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
        "JJ: Resolve the divergent descriptions below into one description.\n\
         JJ: Remove every conflict-marker line before saving.\n",
    );
    template.push_str("<<<<<<< divergent descriptions\n");
    template.push_str(&format!(
        "%%%%%%% base {}\n",
        short_commit_hash(base_commit_id)
    ));
    push_description(&mut template, base.description());
    for commit in graph
        .divergent_commits()
        .iter()
        .filter(|commit| !excluded_divergent_commits.contains(commit.id()))
    {
        template.push_str(&format!(
            "+++++++ version {}\n",
            short_commit_hash(commit.id())
        ));
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
