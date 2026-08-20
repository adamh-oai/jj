# jj converge Command Design

Authors: [David Rieber](mailto:drieber@google.com),
[Martin von Zweigbergk](mailto:martinvonz@google.com)

**Summary:** This document is a proposal for a new `jj converge` command to help
users resolve (or reduce) divergence. The command uses heuristics to rewrite the
N visible commits for a given change with a single new commit, without
introducing new divergence in the process. By default the command is
non-interactive: it either converges deterministically or reports every choice
that still needs an explicit command-line argument. `--interactive` (`-i`) opts
into prompts and an editor for human-driven resolution.

## Objective

A [divergent change] occurs when multiple [visible commits] have the same change
ID. Divergence is not a desirable state, but is not a bad state either. In this
regard divergence is similar to conflicts: the user can choose when and how to
deal with divergence. The [Handling divergent commits] guide has some useful
tips, but nevertheless divergence is confusing to our users. We can do better
than that. It should be possible to solve divergence (after the fact) in many
scenarios with the help of this command. Solving divergence means rewriting the
commit graph to end up with a single visible commit for the given change id. For
the purposes of this design doc we call this commit the *"solution"*.

The command should produce informative messages to summarize any changes made.
It must not read stdin or open an editor unless `--interactive` is passed. The
user may of course not like the solution. `jj undo` can be used in that case.

## Differences from the existing RFC

This design intentionally changes the original interactive-first proposal in
these ways:

*   Normal `jj converge` never prompts or opens an editor. If author,
    description, or canonical parent cannot be inferred, it exits without a
    convergence transaction and reports all unresolved choices at once.
*   `--interactive` (`-i`) preserves the original prompt/editor flow as an
    explicit human-only mode. Explicit resolution flags always take precedence.
*   Each promptable attribute has a deterministic override: `--onto` chooses
    the single canonical parent, `--author-from` copies the full author
    signature from an exact divergent commit, and `--message-from` or `-m`
    chooses the description.
*   `--dry-run --json` exposes a machine-readable convergence plan with exact
    commit IDs, unresolved choices, candidates, descendant-rebase count, and
    predicted tree-conflict state. Agents can rerun the plan with additional
    overrides until `ready` is true, then invoke the same command without
    `--dry-run`.
*   A solution always has exactly one commit-graph parent. Divergent commits
    become evolution predecessors, never multiple commit-graph parents. If no
    safe linear parent is obvious, the caller must provide `--onto` or stop.
*   Content conflicts are preserved in the solution and reported after a
    successful convergence; they do not trigger an interactive merge inside
    `jj converge`.

[divergent change]: ../glossary.md#divergent-change
[visible commits]: ../glossary.md#visible-commits
[Handling divergent commits]: ../guides/divergence.md

## Command-line contract

The non-interactive form is:

```sh
jj converge -r 'change_id(<change-id>)' \
  [--onto <parent>] \
  [--author-from <divergent-commit>] \
  [--message-from <divergent-commit> | -m <message>]
```

If any attribute remains unresolved, the command reports every unresolved
attribute and exits without rewriting commits. The report names the flag that
resolves each choice and lists exact candidate commit IDs. `/0` and `/1` are not
stable identifiers and should not be emitted as machine-facing candidates.

The planning form is:

```sh
jj converge -r 'change_id(<change-id>)' --dry-run --json
```

Dry-run never performs the convergence transaction. JSON output includes the
change ID, divergent commit IDs, `ready`, unresolved choices with their
`resolve_with` flags and candidates, hard `blocked` reasons,
`would_rebase_descendants`, and `may_conflict` once all choices are resolved.
Callers may supply only some overrides, rerun dry-run, inspect the remaining
choices, and apply once the plan is ready. Each invocation recomputes the plan
from current repository state; no partial convergence state is persisted.

The human-driven form is:

```sh
jj converge -i -r 'change_id(<change-id>)'
```

In interactive mode, unresolved authors and parents use prompts and unresolved
descriptions use an editor. Explicit flags still win and suppress interaction
for their attributes. `--interactive` and `--dry-run` are mutually exclusive.

Some conditions are safety failures, not promptable choices: no divergent
change, multiple divergent change IDs without an exact `-r`, immutable commits
that would be rewritten, root-commit convergence, or an invalid `--onto` that is
hidden, has the divergent change ID, or descends from a divergent commit.
Normal mode reports these as errors. Dry-run includes rewrite-immutability
failures in `blocked`; invalid explicit arguments still fail immediately.

## Divergent changes

Divergent commits (for the same change-id) can differ:

*   In their commit description (including tags)
*   In their commit trees
*   In the parent(s) of the commits (commits *B/0* and *B/1* for change *B* have
    different parents)
*   In the commit author
*   It is also possible divergence involves two commits with different
    timestamps that are otherwise identical

As you read this design doc it is important to not confuse the
*predecessor / successor* relationship versus the *ancestor / descendant*
relationship.

### Some divergence scenarios

Divergence can be introduced in many ways. This document does not aim to explain
any / all of those scenarios accurately, this section is only meant to be rough
background material. Here are some examples:

*   In one terminal you type `jj describe` to edit a commit description and
    while the editor is open you take a coffee break, when you come back you
    open another terminal and do something that rewrites the commit (for example
    you modify a file and run `jj log`, causing a snapshot). When you save the
    new description `jj describe` completes and you end up with 2 visible
    commits with the same change id.

*   In general any interactive jj command (`jj split -i`, `jj squash -i`, etc.)
    can lead to divergence in a similar way.

*   You can introduce divergence by making some hidden predecessor of your
    change visible again. There are many ways this could happen.

*   Divergence can happen when mutating two workspaces. For example, assume you
    have workspaces w1 and w2 with working copy commits *A* and *B*
    respectively, where *B* is a child of *A*. In w2 you run `jj git fetch` and
    then rebase the whole branch onto main. Go back to w1 (which is now stale),
    modify some file on disk and take a snapshot (e.g. run `jj log`). This
    introduces divergence.

*   When using the Git backend jj propagates change-id. The change-id is stored
    in the commit header, so after jj git fetch you can end up with a second
    commit with the same change-id.

*   There is a Google-specific jj upload command to upload a commit to Google's
    review / test / submit system, and there is an associated Google-specific
    command to "download" a change from that system back to your jj repo. This
    can introduce divergence very much like in the Git scenario.

*   At Google, snapshotting operations happen concurrently on different machines
    (e.g. two terminals, or more commonly, a terminal and an IDE). Often times
    they end up snapshotting the same content. Google's backend does not hold
    locks while snapshotting because it's a distributed filesystem, so locking
    would be slow.

## Strawman proposal

We look at some examples to illustrate what the command should do, starting with
simple cases and moving on to more complex ones.

### Examples and expected behavior (with basic evolution graph)

The first few examples assume commits *B/0* and *B/1* are visible commits for
change *B*. First we assume *B/0* and *B/1* evolve directly from a common
predecessor commit *P*, which is now hidden (no longer visible). Later we look
at more complex evolution graphs. Note that *P*'s change id is also *B*.

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

We will write `A⁻` to denote the parent trees of commit `A`.

#### Example 1: two commits for change *B*, same parent

```console
$ jj log
B/0
|
| B/1
|/
A
```

In this simple case it is clear the solution should be a child of *A*:

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

Let's now consider two cases: when *P*'s parent is *A*, and when *P* has some
other parent. First, if *P*'s parent is *A* we have:

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

Here *P*, *B/0* and *B/1* are siblings. The command needs to determine the
description, parents, tree and author of the solution. It uses a simple data
structure for this purpose:

```rust
struct MergedState {
  author: Merge<Signature>,
  description: Merge<String>,
  parents: Merge<Vec<CommitId>>,
  tree: Merge<MergedTree>,
}
```

Each of the fields of `MergedState` are populated by doing a merge of the
corresponding fields of *P*, *B/0* and *B/1*. Loosely speaking each merge can be
expressed as `P + (B/0 - P) + (B/1 - P)` for each of the fields. The command
attempts to resolve the various Merge objects trivially, using `same_change:
SameChange::Accept` (later on in this design doc we will tweak the merge
algorithm a bit).

The description is merged as a String value. If the description does not
trivially resolve, normal mode reports `--message-from <commit-id>` and
`-m <message>` as the ways to resolve it. With `--interactive`, the user's
merge tool is invoked with conflict markers. If author does not trivially
resolve, normal mode reports exact `--author-from <commit-id>` candidates;
interactive mode presents those options to the user. Once all choices are
resolved we have our solution commit *B*.
All descendants of *B/0* and *B/1* are rebased onto *B*. The command records the
operation in the operation log with a new View where *B* is a visible commit
with *{B/0, B/1}* as predecessors. *B/0* and *B/1* become hidden commits.

Note that in some cases the solution may be identical to either *B/0* or *B/1*
(in all regards except the commit timestamp): we choose to create a new commit
*B* to make the evolution graph and op log more clearly show that jj converge
was invoked. Alternatively we could keep the matching commit instead of creating
a new commit (this could result in cycles in the evolog).

#### Example 2: two commits for change *B* with same parent (predecessor has a different parent)

Now lets consider the case where *P* has a different parent:

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

In this case we first rebase *P* onto *A* (in-memory) to produce `P' = A + (P -
P⁻)`. This essentially reduces the problem to the previous case. We now produce
the solution as before: `B = P' + (B/0 - P') + (B/1 - P')`. Note that again the
parent of the solution is *A*.

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

In this case it is not immediately obvious which commit should be the parent of
the solution. Let's first consider the case where *P* is a child of *A*.

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

We determine the parent(s) of the solution as follows:

```
parents = P⁻ + (B/0⁻ - P⁻) + (B/1⁻ - P⁻)
```

In this example the expression evaluates to `{A} + ({A} - {A}) + ({C} - {A}) =
{C}`. Since this expression resolves trivially to *{C}*, we use that as the
parents of the solution.

Note that this simple algorithm produces the desired output in example 1 and
example 2. In example 2, the expression looks like this:

```
parents = P⁻ + (B/0⁻ - P⁻) + (B/1⁻ - P⁻)
        = {X} + ({A} - {X}) + ({A} - {X})
        = {A} + ({A} - {X})
```

That expression resolves trivially to *{A}* when using SameChange::Accept.

#### Example 4: divergent commits with different parents, must choose one parent

If instead *P* is a child of some other commit *X*, the story is a bit
different:

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

In this case parents will be

```
{X} + ({A} - {X}) + ({C} - {X}) = {A} + ({C} - {X})
```

Since this does not trivially resolve, normal mode exits without rewriting and
reports *A* and *C* as exact `--onto` candidates. With `--interactive`, the
command prompts the user to select the desired parent for the solution: either
*A* or *C*.

Assume the user chooses *{C}*. The command then rebases (in memory) *B/0*, *B/1*
and *P* onto the chosen parents:

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

As a result we obtain *B/0'*, *B/1'* and *P'*, and these are sibling commits. At
this point the command does a 3-way merge of `MergedTree` objects to produce
`MergedState::tree` (in reality it is enough to rebase the commit *trees*).

#### Example 5: more than 2 divergent commits

There can be more than 2 visible commits for a given change-id. We are assuming
here *B/0*, *B/1* and *B/2* are all direct successors of commit *P* (which is
invisible).

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

This is completely analogous to the first example, we simply have more terms on
all merges. The same thing applies to all previous examples, in all cases we can
deal with any number of divergent commits for change *B*.

### Examples and expected behavior (with arbitrary evolution graph)

So far we only considered simple cases where all divergent commits are direct
successors of a common predecessor *P*. Now we extend the ideas to arbitrary
evolution history. To that end we introduce the *"truncated evolution graph for
B/0, B/1, ... , B/n"*, where *B/0, B/1, ... , B/n* are two or more commits with
the same change-id *B*. This is a directed graph. Its nodes are commits for
change-id *B* and the edges are from a commit to its (immediate) predecessor(s),
ignoring predecessors with unrelated change-ids. The graph is built by
traversing the operation log and associated View objects, adding nodes and
predecessor edges as needed. Nodes are added this way until a single most-recent
common predecessor commit is found. We call the most-recent common predecessor
the *"evolution fork point of B/0, ... , B/n"*. The traversal keeps track of
visited commits to avoid infinite loops [^footnote-about-evolog-cycles]
[^virtual-evolution-fork-point].

#### Example 6: a two-level evolution graph

We continue by looking at a truncated evolution graph that is slightly more
complex than the basic 3-commit case. This will serve as motivation for the
general case. Here is our truncated evolution graph (remember the edges here
represent change evolution, not parent-child relations):

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

Commit *P* evolved into *Q* and *B/1*, and *Q* evolved into *B/0*. As before
*B/0* and *B/1* are visible, *P* and *Q* are not. Since both sides of the
evolution transitioned from "v1" to "v2", and then one side further transitioned
to "v3", it seems a good heuristic to take "v3" as the description of the
solution. Note that this observation would not be possible if the algorithm only
considered the leafs (*B/0*, *B/1*) and their evolution fork point (*P*).

Note: Why do we care about divergence producing two commits with the exact same
change? It may seem this would be a very uncommon scenario, however, as
mentioned in the last bullet point in the "Some divergence scenarios" section,
this is in fact fairly common at Google due to the distributed nature of
Google's backend filesystem.

To implement the heuristic we outlined above in example 6 (i.e. to produce
"v3"), we propose introducing a new *try_resolve_deduplicating_same_diffs*
method to `Merge<T>`, and using that in calls to attempt to resolve the
MergedState::description, MergedState::parent, MergedState::author and
MergedState::trees. try_resolve_deduplicate_same_diffs is similar to
resolve_trivial, but it counts multiple identical *(X - Y)* terms exactly once,
otherwise it follows the same flow as resolve_trivial with SameChange::Accept.

We illustrate what *try_resolve_deduplicating_same_diffs* does when resolving
the description merge for the case in example 6. We build a `Merge<String>`,
starting with the description of the evolution fork point *P*, then adding
*desc(Y) - desc(X)* terms for each *X->Y* edge in the truncated evolution graph.
Then we call *try_resolve_deduplicating_same_diffs* to get:

```
P + (Q - P) + (B/1 - P) + (B/0 - Q) =
       = v1 + (v2 - v1) + (v2 - v1) + (v3 - v2)    <== collapse duplicate
                                                       (v2 - v1) edge/term
       = v1 + (v2 - v1) + (v3 - v2)
       = v3
```

*try_resolve_deduplicating_same_diffs* returns our desired value "v3". Note that
resolve_trivial (with either SameChange value) would return none. Here is
another example:

```
Truncated evolution graph:

B/0     ( foo.txt contents: "v3" )
|
|  B/1  ( foo.txt contents: "v2" )
Q /     ( foo.txt contents: "v1" )
|/
P       ( foo.txt contents: "v1" )
```

In this case *try_resolve_deduplicating_same_diffs* produces none. `jj converge`
cannot automatically resolve this merge. Normal mode reports the exact commits
that can be selected with `--message-from`, or the caller can pass a merged
description with `-m`. With `--interactive`, the command invokes the user's
merge tool with base "v1" and sides "v2"/"v3".

### Edge cases when choosing the parents of the solution

When attempting to produce the solution parents, the command applies
*try_resolve_deduplicating_same_diffs* to MergedState::parents (of type
`Merge<Vec<CommitId>>`). If the result is `Some<Vec<CommitId>>` we have a set of
possible parents for the solution. If these candidate parents are all visible
commits with change-ids other than *B*, and none of those are descendants of
*B/0, B/1, ... B/n*, then we have the desired parents for the solution commit.

On the other hand,

*   if any candidate parent is a descendant of one of the divergent commits we
    are trying to solve, or
*   if any of the candidate parents are hidden and the chain starting at visible
    roots and leading up to and including the parent has any commit for the
    divergent change we are trying to solve (*B*), or for any other visible
    commit (divergent or not), or the chain itself has two or more commits with
    the same change-id,

then we would introduce new divergence or cycles in the commit graph if we based
the solution on such candidate parents. In these edge cases the command discards
unsafe candidates. Normal mode reports the remaining exact `--onto` candidates,
or errors with no valid candidates if none remain. Interactive mode instead asks
the user to choose a parent (or quit without making any changes). Care must be
taken when picking the options to report or present: essentially the user chooses
between the parents of *B/0*, the parents of *B/1*, and so on, but we skip any
*B/i* if any of its parents descends from any *B/j*. Since the commit graph is a
DAG, at least one option is viable.

## Multiple divergent change-ids

If there are multiple divergent change-ids, the command errors without changing
history and lists matching change IDs. The caller must select one explicitly
with `-r 'change_id(<change-id>)'`; `--interactive` does not choose a change ID
because exact target selection is part of the command's safety boundary.

If the command successfully resolves divergence in the first divergent
change-id, it could continue to process the next divergent change-id, and so on.
To avoid complexity the first implementation will only deal with one divergent
change per invocation.

### Rebasing descendants and persisting

The last step is to rebase all descendants of the divergent commits on top of
the new solution commit, persist the changes and record the operation in the op
log. The command will move local bookmarks pointing to any of the rewritten
divergent commits to point to the solution commit.

## Other edge cases

When the command starts it needs to find the divergent change-ids and their
corresponding visible commits. If the portion of the visible commit graph
leading up to immutable heads is too big, the command should error out.

There could be pathological cases where the evolution history is too long. When
building the truncated evolution graph, if we have traversed too many nodes (say
50) and we have not yet completed the traversal, the algorithm will not traverse
any more commits. We could simply error out, or we could use an incomplete
truncated evolution graph by adding a virtual evolution fork point. It is
probably best to error out.

## Open questions

*   Do we ever have divergence of committer? Is it safe to mess with committer?

## Alternatives considered

### Automatically resolving divergence

It would be nice if divergence was avoided in the first place, at least in some
cases, at the point where jj is about to introduce the second (or third or
fourth, etc.) visible commit for a given change id. This should be investigated
separately.

### Resolve divergence two commits at a time

The algorithm in this proposal should work when there are any number of
divergent commits (for a given change id). In practice we expect most often
there will be just 2 or perhaps a few divergent commits. We could design an
algorithm for just 2 commits, but we chose to think about the more general case.

### Only considering the evolution fork point and visible commits

As explained in example 6 this proposal uses the truncated evolution graph and
*try_resolve_deduplicating_same_diffs* to produce the solution. That example
shows why we think this leads to a better heuristic. We could instead only
consider *P* and *B/0, B/1, ... , B/n*. That would be slightly simpler.

[^footnote-about-evolog-cycles]: It is unclear if the evolution history can
    contain cycles today, but there has been some
    discussion about `jj undo` possibly producing
    cycles. In any case, it is very easy to deal
    with that possibility, so we may as well handle
    it?
[^virtual-evolution-fork-point]: Today there should always be a single evolution
    fork point. However, we could handle cases
    where a change-id "emanates from multiple
    initial commits" by adding a single *virtual
    evolution fork point* commit with empty state:
    empty description, empty tree and empty author,
    and having the root commit as its parent, and
    treating it as a predecessor of all initial
    commits. Again, we probably don't need to worry
    about this, but it is good to know we could
    handle it.
