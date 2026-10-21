# jj converge Command Design

Authors: [David Rieber](mailto:drieber@google.com),
[Martin von Zweigbergk](mailto:martinvonz@google.com)

**Summary:** This proposal defines a `jj converge` command that cleans up
divergence and content conflicts, starting with ancestors. For divergence, the
command uses heuristics to replace the N visible commits for a change with one
new commit. It must not introduce new divergence. For a content conflict, it
reports the exact next step for the user to take. It does not edit the files.
By default, the command runs without interaction. It resolves one safe
divergence with no unresolved issue below it, or reports the next choice or
conflict that needs human input. `--interactive` (`-i`) enables prompts and an
editor to resolve a specific divergence.

## Objective

A [divergent change] has multiple [visible commits] with the same change ID.
Divergence is a supported state. As with conflicts, the user can choose when
and how to resolve it. The [Handling divergent commits] guide gives advice,
but users still find divergence confusing. This command should help resolve
many cases after they occur. It rewrites the commit graph so that the change
has one visible commit. This document calls that commit the *"solution"*.

The command should describe the changes it makes or the next manual step to
resolve a conflict. It must not read stdin or open an editor unless the user
passes `--interactive`. The user can run `jj undo` to reject the solution.

## Differences from the existing RFC

This design changes the original proposal, which used interaction by default:

*   Normal `jj converge` never prompts or opens an editor. Its default cleanup
    revset is `divergent() | conflicts()`. It handles only the lowest
    outstanding issue. It resolves safe divergence automatically. For an
    ambiguous divergence or a content conflict, it stops and gives exact
    instructions. The caller reruns the command after each result. The command
    plans and reports descendants only after their ancestors are clean.
*   In normal non-interactive mode, `-r <revset>` selects stack tips. Cleanup
    covers `(divergent() | conflicts()) & ancestors(<revset>)`. Thus `-r @`
    cleans the current stack. The original proposal selected one change per
    invocation. Interactive mode and explicit resolution flags still select
    exact revisions and require one divergent change ID.
*   `--interactive` (`-i`) retains the original prompts and editor for human
    use. Explicit resolution flags always take precedence.
*   Each choice has a flag. `--onto` selects the solution's single parent.
    `--author-from` copies the full author signature from an exact divergent
    commit. `--message-from` or `-m` selects the description.
*   `--json` works with or without `--dry-run`. Without `--dry-run`, it
    resolves a safe divergence at the bottom and returns `status: "resolved"`.
    Otherwise, it returns instructions for the next manual issue, reports
    that the cleanup order is blocked, or returns `status: "clean"`.
    `--dry-run --json` reports the same next issue or blockage without
    rewriting history. It omits descendants that must wait. The next
    invocation plans their cleanup from the repository's current state.
*   A solution always has exactly one commit-graph parent. The divergent
    commits become evolution predecessors. They do not become multiple
    commit-graph parents. If the command cannot infer a safe single parent,
    the caller must provide `--onto` or stop.
*   `jj converge` never merges content conflicts. For a conflict at the bottom,
    it returns `status: "needs_manual_resolution"`, the exact commit ID and
    paths, and the `jj new` / `jj resolve --list` /
    `jj squash --use-destination-message` workflow. If convergence creates
    conflicts, it returns `has_conflicts: true`. The next invocation reports
    that conflict before any divergent descendant.

[divergent change]: ../glossary.md#divergent-change
[visible commits]: ../glossary.md#visible-commits
[Handling divergent commits]: ../guides/divergence.md

## Command-line contract

The cleanup form is:

```sh
jj converge [-r <stack-tip-revset>] [--dry-run] [--json]
```

Without `-r`, the scope is `divergent() | conflicts()`. With `-r`, it includes
issues from that default revset in the matching revisions and their ancestors.
If any visible version of a divergent change is in scope, the command
considers all its versions together. Each invocation selects one issue with no blocking ancestor. It
uses a deterministic rule to select between eligible issues.

The command either resolves a safe divergence or reports one of these results:

*   Every unresolved choice for one divergent change.
*   One content conflict.
*   A cycle between cleanup issues.
*   A clean scope, with no issues remaining.

`/0` and `/1` are not stable identifiers. The command never uses them to
identify candidates in output for programs.

The exact single-divergence form is:

```sh
jj converge -r 'change_id(<change-id>)' \
  [--onto <parent>] \
  [--author-from <divergent-commit>] \
  [--message-from <divergent-commit> | -m <message>]
```

Explicit resolution flags select exact-revision mode and require one divergent
change ID. Use them after cleanup JSON reports `status: "needs_input"`.
Dry-run never performs a convergence transaction.

Divergence JSON includes the change ID, exact divergent commit IDs, `ready`,
and `status`. It also includes unresolved choices, their `resolve_with` flags
and candidates, reasons that prevent resolution in `blocked`, and
`would_rebase_descendants`. Once all choices are resolved, it includes
`may_conflict`.

Conflict JSON includes `kind: "conflict"` and
`status: "needs_manual_resolution"`. It gives the exact commit and change IDs,
conflicted paths, and resolution commands. When a non-dry-run JSON invocation
resolves divergence, it emits `kind: "divergence"` and `status: "resolved"`.
It also gives the solution commit ID, the count of rebased descendants, and
`has_conflicts`.

If issues remain but none has a valid cleanup order, ordering JSON includes
`kind: "ordering"`, `status: "blocked"`, and
`reason: "cyclic_issue_dependencies"`. Its `cycle` array identifies the
ancestor and descendant for each dependency. Each entry gives the issue
`kind` (`"divergence"` or `"conflict"`), change ID, and exact commit ID.
Normal execution exits unsuccessfully in this case. Dry-run returns the
diagnostic plan successfully. Neither mode rewrites history when the order
is blocked.

Human-readable divergence dry-run output uses compact `READY`, `BLOCKED`, and
`NEEDS_INPUT` status blocks. Description and parent choices include `jj log`
commands that the user can copy and run. These commands use exact candidate
commit IDs to show the full descriptions and possible parents.

Callers can provide some overrides, rerun dry-run, and inspect the remaining
choices. They can apply the plan when it is ready. Each invocation recomputes
the plan from the repository's current state. It does not save partial
convergence state.

The human-driven form is:

```sh
jj converge -i -r 'change_id(<change-id>)'
```

In interactive mode, the command prompts for unresolved author and parent
choices. It opens an editor for unresolved descriptions. Explicit flags take
precedence and suppress interaction for their attributes. The user cannot
combine `--interactive` with `--dry-run`. The revset must select one divergent
change ID.

The command cannot resolve these safety failures through prompts:

*   There is no divergent change.
*   The operation would rewrite immutable commits.
*   The operation would converge the root commit.
*   The `--onto` commit is hidden, has the divergent change ID, or descends
    from a divergent commit.

Normal mode reports these conditions as errors. Dry-run lists failures that
would rewrite immutable commits in `blocked`. Invalid explicit arguments
still fail immediately.

## Divergent changes

Divergent commits with the same change ID can differ in these fields:

*   Commit description, including tags.
*   Commit tree.
*   Parents. For example, commits *B/0* and *B/1* for change *B* can have
    different parents.
*   Commit author.
*   Timestamp. Two divergent commits can be identical except for their
    timestamps.

A *predecessor / successor* relationship describes how a change evolved.
An *ancestor / descendant* relationship describes the commit graph. Keep these
relationships separate when reading the examples.

### Some divergence scenarios

The following examples give background on how divergence can occur. They are
rough examples, not a complete or precise account of every case.

*   Run `jj describe` in one terminal and leave the editor open. In another
    terminal, rewrite the commit. For example, modify a file and run `jj log`
    to take a snapshot. Then save the description. When `jj describe`
    completes, two visible commits have the same change ID.

*   Other interactive commands, such as `jj split -i` and `jj squash -i`,
    can cause divergence in the same way.

*   Make a hidden predecessor of a change visible again. Several operations
    can do this.

*   Change two workspaces. Suppose w1 and w2 have working-copy commits *A*
    and *B*, and *B* is a child of *A*. In w2, run `jj git fetch` and rebase
    the branch onto main. Then return to the stale w1 workspace. Modify a
    file and take a snapshot, for example with `jj log`. This introduces
    divergence.

*   With the Git backend, jj propagates the change ID in the commit header.
    After `jj git fetch`, a second commit can have the same change ID.

*   Google's jj commands upload commits to its review, test, and submit
    system. A related command downloads a change into the jj repository.
    This can introduce divergence in the same way as a Git fetch.

*   At Google, different machines can take snapshots concurrently. This can
    happen in two terminals, or more often in a terminal and an IDE. They
    often snapshot the same content. Google's backend uses a distributed
    filesystem. It does not hold locks while taking snapshots because those
    locks would be slow.

## Strawman proposal

The examples below show the proposed behavior, from simple cases to more
complex ones.

### Examples and expected behavior (with basic evolution graph)

The first examples use visible commits *B/0* and *B/1* for change *B*. Both
evolve directly from a common predecessor *P*. Commit *P* is now hidden and
also has change ID *B*. Later examples use more complex evolution graphs.

```
Evolution graph for examples 1, 2, 3 and 4.
B/0 and B/1 may have other predecessors for unrelated change-ids, P may have
predecessors (even for change-id B):

B/0
|
| B/1
|/
P
```

`A⁻` denotes the parent trees of commit `A`.

#### Example 1: two commits for change *B*, same parent

```console
$ jj log
B/0
|
| B/1
|/
A
```

The solution should be a child of *A*:

```console
$ jj log
 B (solution)
 |
 | B/0 (not visible)
 |/
 | B/1 (not visible)
 |/
 A
```

First, consider the case where *P* also has parent *A*. Example 2 uses a
different parent for *P*.

```console
$ jj log
B/0
|
| B/1
|/
| P (not visible)
|/
A
```

Here *P*, *B/0*, and *B/1* are siblings. The command must determine the
solution's description, parents, tree, and author. It uses this data structure:

```rust
struct MergedState {
  author: Merge<Signature>,
  description: Merge<String>,
  parents: Merge<Vec<CommitId>>,
  tree: Merge<MergedTree>,
}
```

The command fills each `MergedState` field by merging the corresponding
fields from *P*, *B/0*, and *B/1*. For each field, this is approximately
`P + (B/0 - P) + (B/1 - P)`. It tries trivial resolution of each `Merge`
with `same_change: SameChange::Accept`. The later examples refine this
merge algorithm.

The command merges the description as a String value. If trivial resolution
fails, normal mode reports `--message-from <commit-id>` and `-m <message>`
as the available choices. With `--interactive`, it opens the user's merge
tool with conflict markers.

If trivial resolution of the author fails, normal mode reports exact
`--author-from <commit-id>` candidates. Interactive mode prompts the user to
choose between those candidates. Once all choices are resolved, the command
creates the solution commit *B*.

The command rebases all descendants of *B/0* and *B/1* onto *B*. It records
the operation in the operation log with a new View. In that View, *B* is
visible and has *{B/0, B/1}* as predecessors. Commits *B/0* and *B/1*
become hidden.

The solution can match *B/0* or *B/1* in every field except the commit
timestamp. The command still creates a new commit *B*. This lets the evolution
graph and operation log show that `jj converge` ran. Keeping the matching
commit would be another option, but it could create cycles in the evolution
log.

#### Example 2: two commits for change *B* with same parent (predecessor has a different parent)

Now consider a different parent for *P*:

```console
$ jj log
B/0
|
| B/1
|/
A
| P (not visible)
|/
X
```

The command first rebases *P* onto *A* in memory to produce
`P' = A + (P - P⁻)`. This reduces the problem to Example 1. It then computes
`B = P' + (B/0 - P') + (B/1 - P')`. The solution's parent is again *A*.

#### Example 3: divergent commits with different parents

```console
$ jj log
B/0
|
|   B/1
|  /
| C
|/
A
```

Here the solution's parent is less obvious. First, consider the case where
*P* is a child of *A*.

```console
$ jj log
B/0
|
|   B/1
|  /
| C
|/
| P (not visible)
|/
A
```

The command computes the solution's parents as follows:

```
parents = P⁻ + (B/0⁻ - P⁻) + (B/1⁻ - P⁻)
```

The expression evaluates to `{A} + ({A} - {A}) + ({C} - {A}) = {C}`.
It resolves trivially to *{C}*, so *C* becomes the solution's parent.

This algorithm also gives the expected result in Examples 1 and 2. In
Example 2, the expression is:

```
parents = P⁻ + (B/0⁻ - P⁻) + (B/1⁻ - P⁻)
        = {X} + ({A} - {X}) + ({A} - {X})
        = {A} + ({A} - {X})
```

With `SameChange::Accept`, the expression resolves trivially to *{A}*.

#### Example 4: divergent commits with different parents, must choose one parent

The result changes if *P* is a child of another commit *X*:

```console
$ jj log
B/0
|
| B/1
| |
| C
|/
A
| P (not visible)
|/
X
```

The parent expression is:

```
{X} + ({A} - {X}) + ({C} - {X}) = {A} + ({C} - {X})
```

The expression does not resolve trivially. Normal mode exits without
rewriting history and reports *A* and *C* as exact `--onto` candidates.
With `--interactive`, the command asks the user to select *A* or *C* as the
solution's parent.

Suppose the user chooses *{C}*. The command rebases *B/0*, *B/1*, and *P*
onto *C* in memory:

```
In-memory commits after rebasing B/0, B/1 and P on top of C (edges represent
parent/child relationship):

# B/0' = C + (B/0 - A)
# B/1' = C + (B/1 - C) = B/1
# P' = C + (P - X)

B/0'
|
| B/1'
|/
| P'
|/
C
```

The resulting commits *B/0'*, *B/1'*, and *P'* are siblings. The command
then performs a three-way merge of their `MergedTree` objects to produce
`MergedState::tree`. It only needs to rebase the commit *trees* to do this.

#### Example 5: more than 2 divergent commits

A change ID can have more than two visible commits. Here, *B/0*, *B/1*, and
*B/2* are direct successors of the hidden commit *P*.

```console
$ jj log
B/0
|
| B/1
| |
| | B/2
| |/
|/
A
```

The command handles this case like Example 1, with more terms in each merge.
The same extension applies to every earlier example. Each can handle any
number of divergent commits for change *B*.

### Examples and expected behavior (with arbitrary evolution graph)

The previous examples assumed that every divergent commit was a direct
successor of a common predecessor *P*. To handle other evolution histories,
define the *"truncated evolution graph for B/0, B/1, ... , B/n"*. These are
two or more commits with the same change ID *B*.

This directed graph contains commits with change ID *B*. Each edge leads
from a commit to an immediate predecessor. The graph excludes predecessors
with other change IDs. To build it, traverse the operation log and associated
View objects. Add nodes and predecessor edges until a single most-recent
common predecessor is found. This commit is the *"evolution fork point of
B/0, ... , B/n"*. Track visited commits to avoid infinite loops
[^footnote-about-evolog-cycles] [^virtual-evolution-fork-point].

#### Example 6: a two-level evolution graph

This truncated evolution graph extends the three-commit case. Its edges show
how the change evolved, not parent-child relationships in the commit graph:

```
Truncated evolution graph. B/0, B/1 and Q may have other predecessors for
unrelated change-ids. P is the evolution fork point (it may have predecessors,
even for change-id B):

B/0     (description: "v3")
|
|  B/1  (description: "v2")
Q /     (description: "v2")
|/
P       (description: "v1")
```

Commit *P* evolved into *Q* and *B/1*. Commit *Q* then evolved into *B/0*.
Commits *B/0* and *B/1* are visible; *P* and *Q* are hidden. Both sides
changed the description from "v1" to "v2". One side then changed it to "v3".
Selecting "v3" for the solution seems a useful heuristic. An algorithm that
only examines *B/0*, *B/1*, and their evolution fork point *P* cannot
recognize this sequence.

Two commits can contain the same change. As described in "Some divergence
scenarios", this is fairly common with Google's distributed filesystem
because different machines can snapshot the same content.

To implement the heuristic in Example 6, this proposal adds
*try_resolve_deduplicating_same_diffs* to `Merge<T>`. It uses that method to
try to resolve MergedState::description, MergedState::parent,
MergedState::author, and MergedState::trees. The method
try_resolve_deduplicate_same_diffs counts identical *(X - Y)* terms only
once. Otherwise, it follows resolve_trivial with SameChange::Accept.

For Example 6, build a `Merge<String>` that starts with the description of
the evolution fork point *P*. Add *desc(Y) - desc(X)* for each *X->Y* edge
in the truncated evolution graph. Then call
*try_resolve_deduplicating_same_diffs*:

```
P + (Q - P) + (B/1 - P) + (B/0 - Q) =
       = v1 + (v2 - v1) + (v2 - v1) + (v3 - v2)    <== collapse duplicate
                                                       (v2 - v1) edge/term
       = v1 + (v2 - v1) + (v3 - v2)
       = v3
```

*try_resolve_deduplicating_same_diffs* returns "v3". The existing
resolve_trivial would return none with either SameChange value. Now consider
this example:

```
Truncated evolution graph:

B/0     ( foo.txt contents: "v3" )
|
|  B/1  ( foo.txt contents: "v2" )
Q /     ( foo.txt contents: "v1" )
|/
P       ( foo.txt contents: "v1" )
```

Here *try_resolve_deduplicating_same_diffs* returns none. The command cannot
resolve the merge automatically. Normal mode reports the exact commits that
the caller can select with `--message-from`. The caller can also provide a
merged description with `-m`. With `--interactive`, the command opens the
user's merge tool with base "v1" and sides "v2" and "v3".

### Edge cases when choosing the parents of the solution

To compute possible parents, the command applies
*try_resolve_deduplicating_same_diffs* to MergedState::parents, of type
`Merge<Vec<CommitId>>`. A `Some<Vec<CommitId>>` result gives a set of
candidates. They are the desired parents if all are visible, all have change
IDs other than *B*, and none descends from *B/0, B/1, ... B/n*.

The command rejects candidates that meet either of these conditions:

*   A candidate parent descends from one of the divergent commits being
    resolved.
*   A candidate parent is hidden, and its chain from the visible roots has
    an unsafe change ID. This includes the candidate parent itself. The
    chain is unsafe if it contains change *B*, a change ID from any other
    visible commit, or two or more commits with the same change ID.

These candidates could introduce new divergence or cycles in the commit
graph. The command discards them. Normal mode reports the remaining exact
`--onto` candidates. It returns an error if none remain. Interactive mode
asks the user to choose a parent or quit without changes.

The choices come from the parents of *B/0*, *B/1*, and so on. Skip a commit
*B/i* if any of its parents descends from any *B/j*. The commit graph is a
directed acyclic graph (DAG), so at least one option is viable.

## Bottom-up cleanup

Normal non-interactive mode can find many divergent changes and conflicted
commits in scope. It chooses only one issue at the bottom. The next invocation
finds the next issue again. This is necessary because convergence or manual
conflict resolution can rewrite descendants, remove their conflicts, or change
their plans. The command resolves a change only when it can infer its author,
description, and single parent deterministically. It does not report
descendants that must wait.

Selection has three outcomes:

*   `Clean`: the issue list is empty.
*   `Next(issue)`: an issue has no blocking ancestor.
*   `Blocked(cycle)`: issues remain, but none can be selected.

The command processes independent eligible issues before it reports a cycle
on a later invocation. It must not report a clean scope just because it could
not select an issue.

Cleanup dependencies connect issues. A divergent family groups all visible
versions of one change. Thus, one version of change A can precede change B,
while another version of B precedes another version of A:

```text
A0 -> B0
B1 -> A1
```

Under bottom-up cleanup, A and B must each wait for the other. A similar cycle
can occur when a content conflict lies between two versions of the same change.
The cycle connects grouped cleanup issues. The commit graph remains acyclic.
The diagnostic selects a cycle deterministically. It gives exact commit IDs
that show each ancestor relationship:

```json
{
  "kind": "ordering",
  "status": "blocked",
  "reason": "cyclic_issue_dependencies",
  "cycle": [
    {
      "ancestor": {"kind": "divergence", "change_id": "A", "commit_id": "A0"},
      "descendant": {"kind": "divergence", "change_id": "B", "commit_id": "B0"}
    },
    {
      "ancestor": {"kind": "divergence", "change_id": "B", "commit_id": "B1"},
      "descendant": {"kind": "divergence", "change_id": "A", "commit_id": "A1"}
    }
  ]
}
```

The example uses short labels; actual output uses full exact IDs. The user
must inspect these commits and choose the intended stack order. They must
then resolve one divergent family in exact-revision mode, using explicit
resolution options or `--interactive`. Using only cleanup `-r` still includes
ancestors and can find the same cycle again. The command does not choose a
stack order automatically.

`--interactive`, `--onto`, `--author-from`, `--message-from`, and `-m`
select exact-revision mode. They require exactly one divergent change ID.
These choices describe a specific change and cannot safely apply to a whole
cleanup scope.

### Rebasing descendants and persisting

Finally, the command rebases all descendants of the divergent commits onto
the solution commit. It saves the changes and records the operation in the
operation log. Local bookmarks that pointed to rewritten divergent commits
move to the solution commit.

## Other edge cases

At startup, the command finds divergent change IDs and their visible commits.
It should return an error if the part of the visible commit graph leading to
immutable heads is too large.

Evolution history can also be too long. The algorithm should stop if it
reaches a node limit before it finishes building the truncated evolution
graph. A possible limit is 50 nodes. It could then return an error or add a
virtual evolution fork point to the incomplete graph. Returning an error is
probably the better choice.

## Open questions

*   Can divergent commits have different committers? Is it safe to change
    the committer?

## Alternatives considered

### Automatically resolving divergence

Jj could prevent some divergence when it is about to create another visible
commit for a change ID. Investigate that behavior separately.

### Resolve divergence two commits at a time

The proposed algorithm should handle any number of divergent commits for a
change ID. Most cases will probably have two or a few commits. An algorithm limited to
two commits is possible, but this design covers the general case.

### Only considering the evolution fork point and visible commits

Example 6 explains why the proposal uses the truncated evolution graph and
*try_resolve_deduplicating_same_diffs*. That example suggests this approach
gives a better heuristic. Considering only *P* and *B/0, B/1, ... , B/n*
would be slightly simpler.

[^footnote-about-evolog-cycles]: We do not know whether evolution history can
    contain cycles today. There has been discussion of `jj undo` possibly
    creating them. Tracking visited commits would handle this case with
    little extra work.
[^virtual-evolution-fork-point]: There should currently be a single evolution
    fork point. If one change ID came from multiple initial commits, a virtual
    evolution fork point could connect them. It would have an empty
    description, tree, and author, with the root commit as its parent. Treat
    it as a predecessor of every initial commit. This case probably does not
    need support now, but this approach could handle it.
