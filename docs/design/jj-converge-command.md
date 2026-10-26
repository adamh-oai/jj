# Resolving divergent changes with `jj converge`

Original design by [David Rieber](mailto:drieber@google.com) and
[Martin von Zweigbergk](mailto:martinvonz@google.com).

`jj converge` helps a user resolve divergence and content conflicts in a stack.
By default, each call handles one issue after its ancestors are clear. It either replaces the visible versions of a change with one new
commit or explains what the user must resolve next.

This RFC describes the implementation on `dev/adamh/converge`. The original
proposal used interaction by default. The implementation requires
`--interactive` to prompt or open an editor. The CLI selects the next issue, and the library works out which values the
versions can share. [Command](../../cli/src/commands/converge.rs) ·
[Library](../../lib/src/converge.rs)

## What convergence does

A change is divergent when several visible commits have the same change ID.
Concurrent edits, work in stale workspaces, and imported commits can produce
divergence. Several machines can also record the same edit independently.
Divergence is a supported state. The user chooses when to resolve it, as described in the guide. [Divergent change] · [Visible commits] ·
[Handling divergent commits]

Convergence creates one visible commit for the selected change. This document
calls it the *solution*. The solution keeps the change ID and records all
selected versions as evolution predecessors. Descendants are rebased onto it.
The operation must not introduce new divergence.

Descriptions, authors, parents, and file contents need different resolution
rules. A choice of description cannot resolve a file conflict. The command
merges trees when it creates a solution, but leaves any remaining file
conflicts for the user to resolve.

[divergent change]: ../glossary.md#divergent-change
[visible commits]: ../glossary.md#visible-commits
[Handling divergent commits]: ../guides/divergence.md

## A cleanup walkthrough

Suppose the bookmark `feature` points to the tip of a stack. An ancestor has
two visible versions with different descriptions and unrelated parents. The
command cannot infer a description or a single parent for that change.

Inspect the next issue before changing history.

```sh
jj converge -r feature --dry-run
```

The following excerpt shows the diagnostic format. Names in angle brackets
stand for IDs from the output. Actual diagnostics use full IDs.

```text
<change-id>  NEEDS_INPUT
  rewrites: 2 descendants
  needs:
    description: --message-from <commit-id> or -m <message>
    parent     : --onto <commit-id>
```

The output also gives `jj log` commands for inspecting the candidate
descriptions and parents. Run those commands before choosing. For example,
the description command has this form.

```sh
jj log -r 'commit_id(<version-1>) | commit_id(<version-2>)' \
  --no-graph -T 'commit_id ++ "\n" ++ description ++ "\n"'
```

Choose the intended parent and write the description that the solution should
keep. Replace the placeholders in this command with IDs from the diagnostic.

```sh
jj converge -r 'change_id(<change-id>)' \
  --onto <parent-commit-id> \
  -m 'Describe the combined change' \
  --dry-run
```

The explicit choices make this an operation on one divergent change. The
command now reports `READY` if it has all required choices and can rewrite
the commits. The `conflicts` line says whether the proposed solution tree has
conflicts. `READY` does not mean that file contents are conflict-free.

Apply the same choices by removing `--dry-run`.

```sh
jj converge -r 'change_id(<change-id>)' \
  --onto <parent-commit-id> \
  -m 'Describe the combined change'
```

The command creates the solution and rebases its descendants. Suppose the
two versions also changed the same lines in a file and the merge leaves a
conflict. The command warns that the solution has conflicts. Run cleanup
again to get the exact commit and paths that need work.

```sh
jj converge -r feature
```

For a file named `src/config.rs`, the diagnostic has this form.

```text
<change-id>  CONFLICT
  commit: <conflicted-commit-id>
  paths:
    src/config.rs
  resolve:
    start  : jj new <conflicted-commit-id>
    inspect: jj resolve --list
    finish : jj squash --use-destination-message
```

Start a child of that commit and inspect the conflict.

```sh
jj new <conflicted-commit-id>
jj resolve --list
```

`jj resolve --list` only lists conflicts. Edit the file to resolve its conflict
markers, or use the configured merge tool.

```sh
jj resolve src/config.rs
```

After resolving all listed paths, check the list again and move the resolution
into the conflicted commit.

```sh
jj resolve --list
jj squash --use-destination-message
jj converge -r feature
```

Keep using `feature` as the cleanup scope. `jj new` moves the working copy, so
`-r @` would now select a different stack tip. Repeat cleanup and any requested
manual work until the command reports
`No divergent changes or content conflicts found.`

Each call reads the current repository state. A dry-run does not save a plan
for later execution. Immediately after a convergence operation, `jj undo`
can undo that operation if the solution is unwanted.

## Command-line contract

The command has two selection modes.

| Form | What `-r` selects | Work done by one call |
| --- | --- | --- |
| `jj converge [-r <revset>]` | Stack tips and their ancestors | Resolve one eligible divergence or report the next issue |
| `jj converge -r <revset>` with an explicit resolution flag or `-i` | Exact revisions for one divergent change | Resolve that selected divergence or report its remaining choices |

In cleanup mode, the default scope is `divergent() | conflicts()`. With `-r`,
the scope is `(divergent() | conflicts()) & ancestors(<revset>)`. If any visible
version of a change is in scope, cleanup includes all visible versions of that
change. Multiple `-r` arguments form a union.

Exact mode requires at least two selected versions of one divergent change.
Select the complete family with `-r 'change_id(<change-id>)'`. Selecting only
one version fails with `No divergent changes found` and a hint to select all
versions. Explicit choices cannot apply to several divergent changes at once.

| Option | Meaning |
| --- | --- |
| `--onto <revision>` or `-o <revision>` | Use this single commit as the solution's parent |
| `--author-from <revision>` | Copy the full author signature, including its timestamp, from a selected divergent commit |
| `--message-from <revision>` | Copy the full description from a selected divergent commit |
| `-m <message>` or `--message <message>` | Set the description. Repeated arguments become paragraphs |
| `-i` or `--interactive` | Prompt for unresolved author and parent choices and open a text editor for an unresolved description |
| `--no-interactive` | Explicitly select the default behavior without prompts |
| `--dry-run` | Report the plan without performing a convergence transaction |
| `--json` | Write the result as JSON on stdout |

`--onto`, `--author-from`, `--message-from`, `-m`, and `-i` select exact mode.
Each revision passed to a choice flag must resolve to one commit.
`--message-from` and `-m` cannot be combined. Interactive mode cannot be
combined with `--dry-run` or `--no-interactive`.

Explicit choices take precedence over inference and prompts. The user can
supply some choices, run dry-run, and inspect what remains. Interactive
description resolution uses the configured text editor. It shows a base and
the competing descriptions. Saving the description with converge conflict
markers still present is an error.

Without `--interactive`, the command does not read stdin or open an editor.

Dry-run skips the convergence transaction. Normal workspace startup can still
snapshot the working copy. The global `--ignore-working-copy` option suppresses
that startup snapshot when the caller needs to inspect the recorded state.

### Output for programs

`--json` returns one result for the selected issue. It does not return plans
for descendants that must wait. Candidate and result IDs are full IDs.
Display suffixes such as `/0` and `/1` can change and are not identifiers for
programs.

| `kind` | `status` | Result |
| --- | --- | --- |
| `divergence` | `ready` | Dry-run has all choices needed to create the solution |
| `divergence` | `needs_input` | The caller must choose an author, description, or parent |
| `divergence` | `blocked` | The rewrite would include immutable commits |
| `divergence` | `resolved` | The solution was created |
| `conflict` | `needs_manual_resolution` | The caller must resolve files in the reported commit |
| `ordering` | `blocked` | Cleanup dependencies contain a cycle |
| Absent | `clean` | Cleanup found no issues in scope |

A divergence plan includes `change_id`, `divergent_commits`, `ready`,
`unresolved`, `blocked`, and `would_rebase_descendants`. Each unresolved choice
has a `kind`, a `resolve_with` instruction, and exact candidate commit IDs.
Candidates also include the relevant description or author when available.
Once all choices are resolved, `may_conflict` reports whether the solution
tree has conflicts. It does not predict every conflict in rebased descendants.

A completed convergence returns `change_id`, the solution's `commit_id`,
`rebased_descendants`, and `has_conflicts`. A conflict result gives `commit_id`,
`change_id`, sorted `paths`, and `resolve_with.start`, `resolve_with.inspect`,
and `resolve_with.finish`. These are the three commands shown in the walkthrough.
The caller must perform the file edits between inspection and finishing.

Normal execution exits unsuccessfully for unresolved choices, manual
conflicts, immutable rewrites, and ordering cycles. It can emit their JSON
plans before returning that error. Dry-run returns those diagnostic plans
successfully. Invalid arguments and other errors can occur before any JSON
is written. Callers must handle the exit status as well as stdout.

## How jj builds the solution

The commit graph describes parent and child relationships. The evolution
graph describes versions of the same change. Convergence reads both graphs.
The solution has exactly one commit-graph parent. Its divergent versions
become evolution predecessors.

The library represents the proposed solution as `ConvergeResult`.

| Field | Type and meaning |
| --- | --- |
| `author` | `ConvergedAttribute<Signature>` |
| `description` | `ConvergedAttribute<String>` |
| `parents` | `ConvergedAttribute<Vec<CommitId>>` |
| `tree` | `Option<TreeIdsAndLabels>`, computed after the parents are known |

`ConvergedAttribute` is either `Solved(value)` or `Unsolved`. An unresolved
attribute carries a `base_commit` and `excluded_divergent_commits` to guide
resolution. `TreeIdsAndLabels` holds a `Merge<TreeId>` and its conflict labels.
The library can infer multiple parents. The CLI requires the user to reduce
that result to one parent before it creates a solution.

### Use the history of each value

`TruncatedEvolutionGraph` reads the selected commits' same-change predecessor
history. It excludes predecessors with other change IDs. It tracks visited
commits, and the predecessor walker rejects cycles. If the history has several
initial commits, the graph connects them through a virtual entry based on the
root commit.

For each attribute, the algorithm maps commits to their attribute values.
Equal values become one node in a *value transition graph*. Edges record how
those values changed through evolution. The algorithm finds the closest
common *dominator value* for the selected versions. Every path from the entry
to each selected value must pass through that value.

This matters when two histories include the same intermediate edit. Consider
these descriptions, with arrows from predecessor to successor.

```text
P("v1") -> Q("v2") -> B0("v3")
   \----------------> B1("v2")
```

Both histories reach `"v2"`. One then reaches `"v3"`. Combining equal values
gives a value graph with the path `"v1" -> "v2" -> "v3"`. The common dominator
value is `"v2"`. Using it as the base lets the algorithm select `"v3"`.
Looking only at `P`, `B0`, and `B1` would lose the intermediate edit in `Q`.

For base value `F` and selected values `V0` through `Vn`, the library builds
this merge expression and calls `resolve_trivial(SameChange::Accept)`.

```text
F + (V0 - F) + (V1 - F) + ... + (Vn - F)
```

This is a heuristic for combining recorded edits. It cannot establish the
user's intent. Descriptions are whole string values. Two independent changes
from `"v1"` to `"v2"` and `"v3"` require a description choice if the merge
does not resolve trivially.

Author inference uses the full signature. If that merge fails but all
versions have the same name and email, the CLI can still infer an author.
It keeps the base signature when its name and email match. Otherwise it uses
the earliest timestamp among the candidates. If the merge fails and author
identities differ, the user must supply `--author-from` or choose interactively.

### Choose one safe parent

Parent inference first excludes selected versions that descend from another
selected version. Their parents could make the rewrite depend on itself.
It then applies the value-merge algorithm to the remaining parent vectors.

If that merge is unresolved, the algorithm examines all remaining candidate
parents. When one candidate descends from every other candidate, it chooses
that candidate. For example, versions based on an older and a newer commit
on the same trunk can converge onto the newer commit automatically. Unrelated
parents may still require `--onto` or an interactive choice.

The CLI checks the selected parent before applying the result. It rejects a
parent that is hidden, has the selected change ID, or is a descendant of a
selected divergent commit. The root commit cannot itself be converged because
it has no parent. `--onto` may name another visible commit if it passes these
checks. It need not be one of the inferred candidates.

### Merge trees on the chosen parent

Trees must be compared on the same parent. Suppose the selected parent is
`C`, a historical version is `X`, and `parent_tree(X)` is the merged tree of
`X`'s parents. The algorithm rebases the tree in memory.

```text
X' = merge(tree(C) + (tree(X) - parent_tree(X)))
```

It does this for the values needed from the evolution history. It then finds
a common dominator value among the rebased tree IDs and chooses a historical
commit that produced that value. Let its rebased tree be `F'`.

```text
solution_tree = merge(F' + (B0' - F') + (B1' - F') + ... + (Bn' - F'))
```

Comparing rebased trees prevents a parent's unrelated changes from becoming
edits attributed to the divergent change. The algorithm compares tree IDs
without conflict labels when finding equal values. It keeps labels when it
builds the final tree merge. The CLI recomputes the tree after all explicit
or interactive choices have been made.

The merge can leave file conflicts. Convergence records those conflicts in
the solution and reports them. `--message-from`, `-m`, and the interactive
description editor affect only the description. File conflicts use the
manual workflow from the walkthrough.

### Record the solution and rebase its descendants

Before it opens a convergence transaction, the command checks whether the
rewrite would include any immutable commit. This includes descendants of
every selected version. A failure stops the rewrite.

`apply_solution` creates a new commit with the selected change ID, author,
description, parent, and merged tree. It uses the normal committer identity
and timestamp for a new commit. It records the selected versions as
predecessors, marks them as replaced, and rebases their descendants. Local
bookmarks move with the rewritten commits through the usual rewrite rules.
The transaction is recorded in the operation log.

The command creates a new solution even when its contents match an existing
version. This records the convergence and its predecessors explicitly.

## Bottom-up cleanup

Cleanup groups all visible versions of one divergent change into one issue.
A conflicted commit is a separate issue unless it belongs to a divergent
family. In that case, convergence comes first because it may remove or change
the conflict.

An issue must wait if another issue contains one of its ancestors. Cleanup
selects one issue with no such dependency. It uses a deterministic order to
choose between independent eligible issues. After a rewrite or manual fix,
the next invocation builds the issue list again. Descendant conflicts and
choices may have changed.

Grouping versions can produce a cycle between issues even though the commit
graph is acyclic. In this graph, arrows run from ancestor to descendant.

```text
A0 -> B0
B1 -> A1
```

Change A must wait for B because `B1` precedes `A1`. Change B must wait for A
because `A0` precedes `B0`. A conflict between two versions of one change can
also create a cycle. Cleanup processes any independent eligible issues first.
When only blocked issues remain, it reports
`Cannot order cleanup issues: cyclic dependencies`.

The JSON result has `reason` equal to `cyclic_issue_dependencies`. Its `cycle`
array contains an `ancestor` and a `descendant` for each dependency. Each end
gives the issue kind, change ID, and exact commit ID that proves the ancestry.
The user must inspect those commits and choose the intended stack order.

Resolve one family explicitly to break the cycle.

```sh
jj converge -r 'change_id(<change-id>)' --interactive
```

Alternatively, supply `--onto` and any other required choices for that family.
Exact mode bypasses cleanup ordering for the selected change. Using only
`-r` still selects cleanup mode and can report the same cycle again. An empty
issue list is the only condition that produces `clean`.

## Evidence and remaining questions

The [CLI tests](../../cli/tests/test_converge_command.rs) check the user
workflow and its observable results. They cover explicit description and
author choices, interactive description editing, single-parent results, and
selection of an entire change. They also check that cleanup handles ancestors
first and reports only the next issue.

The conflict tests check the exact commit, paths, and manual commands in JSON.
The cycle tests check the ancestry represented by each dependency and verify
that reporting a cycle leaves the recorded repository state unchanged. A
separate case confirms that cleanup processes an independent issue before
reporting the remaining cycle. These checks distinguish a blocked scope from
a clean one.

The [library tests](../../lib/tests/test_converge.rs) cover evolution graphs,
description merges, descendant-parent inference, tree merges, and rebasing
trees onto a chosen parent. The
[value-graph implementation and tests](../../lib/src/graph_dominators.rs)
check the dominator calculations used by inference.

A dedicated CLI test is still needed for immutable descendants. It should
check that one immutable descendant prevents the whole rewrite.

The implementation has no explicit 50-node evolution limit. The earlier
proposal suggested such a limit, but did not settle the failure behavior.
Large histories and ancestry checks between many cleanup issues still need
performance measurements. A resource limit should specify what it counts
and return a clear error when reached.

The solution uses a new committer signature. Whether users need to choose
that signature is still open.

## Alternatives considered

Jj could prevent some divergence when it creates another visible version of
a change. That behavior requires a separate design because convergence here
is an operation the user chooses after divergence occurs.

Resolving two versions at a time could simplify some interfaces. The current
algorithm combines all selected versions in one operation, including shared
intermediate values from their histories. Pairwise resolution would need its
own rules for ordering and for preserving that information.

Using only a common predecessor and the visible versions would avoid reading
intermediate evolution history. It would miss the shared `"v2"` edit in the
description example. The original RFC proposed a new
`try_resolve_deduplicating_same_diffs` method for this case. The implementation
instead finds a base in the value transition graph and uses the existing
trivial-merge operation.
