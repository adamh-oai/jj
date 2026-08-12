---
title: "3. Initialize a snapshot worktree"
description: "Follow workspace add from the source snapshot through Btrfs copy-on-write, fresh Jujutsu and Git identities, initial checkout, and watch initialization."
sidebar:
  order: 3
---

The first two walkthroughs follow one working checkout. This one introduces a
second checkout created with `jj workspace add`. The destination is a **writable
Btrfs snapshot**, but it must become an independent Jujutsu workspace and, when
colocated, an independent Git linked worktree. Sharing file extents is useful;
sharing mutable workspace identity, cursors, or indexes is incorrect.

:::caution[Implementation status]
This walkthrough distinguishes the intended lifecycle from behavior that the
current source does not implement correctly. Automatic fallback can create a
mass-deletion working copy, sparse widening can omit tracked files, and direct
AWACS currently rejects every sibling workspace. Those failures are identified
at the exact steps where they occur.
:::

## Starting conditions and names

Assume the source and destination will be **directory siblings** on the same
Btrfs filesystem:

```text
/projects/
    main/                         existing writable Btrfs subvolume
        .jj/repo/                 physical shared Jujutsu repository
        .jj/working_copy/         source-only checkout and tree state
        .git/                     physical Git repository, if colocated
        src/app.rs
        target/                   ignored build output

    feature/                      does not exist yet

/awacs-managed/                   separate private managed-snapshot directory
```

The example command is:

```sh
jj workspace add --btrfs-snapshot=true ../feature
```

`--name` overrides the destination basename. `--revision` chooses explicit
working-copy parents, and `--sparse-patterns` selects `copy`, `full`, or
`empty`. Configuration can instead request Btrfs behavior with
`btrfs.enabled = true`, `false`, or `"auto"`.

Three different kinds of “snapshot” appear in this walkthrough:

1. A **Jujutsu working-copy snapshot** reads files and updates a commit/tree.
2. A **writable Btrfs workspace snapshot** creates `feature/` with shared
   copy-on-write extents.
3. A **read-only AWACS cut** freezes one workspace root for indexing or a
   direct immutable scan.

They have different owners, identities, persistence boundaries, and lifetimes.

```mermaid
sequenceDiagram
    participant User as "jj workspace add"
    participant Source as "Source Jujutsu workspace"
    participant Btrfs as "Btrfs writable roots"
    participant Git as "Shared Git repository"
    participant Child as "Destination Jujutsu workspace"
    participant Watch as "AWACS Watchman registration"

    User->>Source: "Lock and snapshot source working copy"
    Source-->>User: "Source working-copy commit and shared repository"
    User->>Btrfs: "Create writable snapshot at sibling destination"
    Btrfs-->>User: "New root UUID with copied files and metadata"
    User->>Child: "Delete copied .jj and .git identity"
    User->>Git: "Register temporary detached linked worktree"
    Git-->>Child: "Copy .git pointer and repair linked-worktree path"
    User->>Child: "Create private .jj state and shared-repo pointer"
    User->>Child: "Reset to copied source tree and mark monitor baseline"
    opt "Watchman backend only"
        Child->>Watch: "Resolve destination root and obtain fresh clock"
    end
    User->>Child: "Apply sparse policy and create independent commit"
    User->>Child: "Check out the chosen parent tree"
```

## Step 1: Snapshot and lock the source workspace

**Owner:** Jujutsu CLI, `cli/src/commands/workspace/add.rs::cmd_workspace_add`
and `cli/src/cli_util.rs::CommandHelper::workspace_helper`.

Before choosing a destination, the command loads the **source** workspace,
acquires the shared `git_import_export.lock` when applicable, and snapshots its
working copy unless global options disable snapshotting. This is a normal
Jujutsu snapshot; if direct AWACS is enabled, the source may already perform a
complete immutable scan transaction.

The resulting source commit describes the tracked tree against which the
filesystem optimization will establish its temporary destination baseline.
Ignored build outputs can also be physically copied by Btrfs even though they
are not present in that commit.

**Invariant:** The source command must not accidentally treat destination
state as source state. A source Jujutsu commit, source AWACS cursor, source
workspace name, and source Git worktree identity are four distinct pieces of
authority that cannot simply be copied into a usable child.

## Step 2: Resolve the snapshot policy and destination identity

**Owner:** `cli/src/commands/workspace/add.rs::cmd_workspace_add`.

An explicit `--btrfs-snapshot=true` requires a snapshot; `false` requires
ordinary materialization. Without that flag, `btrfs.enabled` selects required,
disabled, or optional `"auto"` behavior. The command derives the workspace
name from `--name` or the destination basename and rejects an existing name in
the shared repository view.

For a real Btrfs snapshot, the destination must not already exist, the source
must be a writable subvolume root, and the destination must be on the same
filesystem. Crucially, **the destination must not be underneath the monitored
source**: directory siblings can have parent/child snapshot lineage without
creating an unsupported nested subvolume.

**Currently broken — C-22:** The command accepts destinations inside the
source checkout. A writable child subvolume beneath an already watched root
violates AWACS's no-nested-subvolume invariant, so a later source cut can fail
and may permanently wedge that watch as described in C-05.

**Currently broken — C-21, C-28, and C-29:** Optional mode does not preserve
stock behavior for a missing `btrfs` executable, an existing empty
destination, or an otherwise valid destination on another filesystem.
Optional optimization must fall back before changing stock semantics.

## Step 3: Create a writable copy-on-write Btrfs root

**Owner:** `cli/src/commands/workspace/add.rs::create_btrfs_snapshot`, invoking
`btrfs subvolume snapshot <source> <destination>`.

The destination receives the source's currently materialized directory tree,
including ordinary tracked paths, ignored build artifacts, and temporarily its
`.jj` and `.git` metadata. Btrfs initially shares data and metadata extents by
copy-on-write. A later edit in either root diverges without editing the other
root's live file contents.

The destination is **writable**, has its own subvolume/root identifier and UUID,
and reports the source subvolume UUID as its Btrfs snapshot `parent_uuid`.
The parent's UUID proves filesystem ancestry; it is not a Jujutsu commit ID,
an AWACS watch ID, or a directory-parent relationship.

**Invariant:** An optimization may share immutable objects and copy-on-write
extents. It must never give two active workspaces the same mutable checkout
state, Git index, AWACS watch, authorization grant, or persisted cursor.

### The dangerous optional-fallback branch

The current code captures `snapshot_source_commit` **before** attempting its
optional snapshot. If the attempt returns the ordinary-workspace fallback, it
sets `snapshot = false` but leaves that source commit populated. The
destination is then an empty normal directory, yet later initialization still
uses the snapshot-only baseline path.

`TreeState::reset` creates tracked file-state entries without writing missing
files. The first full destination scan therefore records inherited tracked
paths as deletions; a fresh Watchman clock can instead make the fabricated
tree appear clean.

**Currently broken — C-02:** This is a reproduced, release-blocking data-loss
path. When optional Btrfs creation does not actually produce a writable
snapshot, every snapshot-only baseline and monitor action must be discarded
and ordinary Jujutsu materialization must run unchanged.

## Step 4: Remove copied workspace identities

**Owner:** `create_btrfs_snapshot` and `remove_copied_metadata`.

Immediately after the writable snapshot, Jujutsu removes `feature/.jj` and
`feature/.git` from the child. It uses `symlink_metadata`, recursively removes
actual directories, and unlinks files or symbolic links without following
them.

The copied `.jj` otherwise contains the source workspace name, operation,
working-copy tree, locks, and monitor cursor. The copied `.git` otherwise
contains or points at the source's mutable Git worktree identity. Neither copy
is a valid independent workspace.

**Currently expensive — P-09:** When the source physically contains a large
`.jj/repo` or `.git/objects`, recursively deleting those copied directory trees
walks potentially enormous metadata and triggers copy-on-write updates. The
logical isolation is necessary; the current physical layout forfeits much of
the intended cheap-snapshot benefit.

## Step 5: Create an independent linked Git worktree

**Owner:** `create_git_worktree_with_existing_files` and
`create_git_worktree` in `cli/src/commands/workspace/add.rs`.

This step applies only when the source is colocated with Git. The destination
already contains materialized files, so asking Git to check out directly over
them would be incorrect. Instead, the implementation:

1. Canonicalizes the existing shared Git repository and records the source
   commit to use as the detached checkout identity.
2. Acquires the shared `git_import_export.lock`.
3. Creates an empty temporary sibling directory.
4. Executes `git worktree add --force --no-checkout --detach --quiet` for that
   temporary path.
5. Copies the small generated `.git` **pointer file** into `feature/.git`.
   A rename across the Btrfs subvolume boundary can fail with `EXDEV`.
6. Runs `git -C feature worktree repair` so the shared administrative
   registration points to the real destination rather than the temporary
   directory.

The object database is shared, but the linked worktree has its own Git
administrative identity, HEAD, and mutable index. It must not keep using a
copied source `.git` directory or source worktree registration.

**Invariant:** Sharing Git objects is safe; sharing one mutable worktree index
or pretending that two checkout directories have the same Git worktree
identity is not. Registration and repair must either complete together or be
cleaned up safely after failure.

## Step 6: Create private Jujutsu state pointing at the shared store

**Owner:** `lib/src/workspace.rs::Workspace::init_workspace_with_existing_repo`.

The library creates a fresh `feature/.jj` directory, canonicalizes the
existing repository path, and writes `feature/.jj/repo` as a **file containing
a relative path** to the original physical store. It initializes a new
`feature/.jj/working_copy` tree and checkout identity, then registers the new
workspace name and root in the shared `SimpleWorkspaceStore`.

The resulting physical topology is asymmetric:

```text
/projects/main/
    .jj/
        repo/                        physical shared op, view, and object state
            workspace_store/         registration for main and feature
            git_import_export.lock   shared Git coordination
        working_copy/
            checkout                 source workspace identity and operation
            tree_state               source tree, file states, and cursor
    .git/
        objects/                     shared Git objects, if colocated
        worktrees/<feature>/         destination-only linked-worktree state

/projects/feature/
    .jj/
        repo                         relative pointer to main/.jj/repo
        working_copy/
            checkout                 independent destination identity
            tree_state               independent destination tree and cursor
    .git                             pointer to its linked-worktree admin
```

**Invariant:** Every workspace must resolve the same shared Jujutsu repository
and operation store while retaining its own name, working-copy commit,
checkout operation, tree/file-state cache, sparse policy, and monitor cursor.

**Currently dangerous — C-01:** `jj workspace remove default` from a secondary
workspace currently deletes the primary directory containing the physical
`.jj/repo` and possibly Git object database. The surviving child's pointer
then resolves to nothing. The primary store owner must be protected or
relocated before its directory can ever be deleted.

## Step 7: Record the copied source tree as a temporary baseline

**Owner:** `cmd_workspace_add`, `LockedLocalWorkingCopy::reset`, and
`LockedLocalWorkingCopy::mark_fsmonitor_baseline`.

For an actual writable snapshot, Jujutsu temporarily associates the child with
the source working-copy commit in the shared repository view. It then locks
the **child's** working copy and calls `reset(source_commit)`. Reset records
the source tree and synthetic file-state entries; it does not materialize
files. That is valid only because a successful Btrfs snapshot already copied
the expected files.

Next it invokes `mark_fsmonitor_baseline`:

| Backend | What actually happens during workspace creation |
| --- | --- |
| `none` | The child cursor is cleared; there is no monitor registration. |
| `watchman` | The child cursor is cleared, its own root is resolved, and a new child-specific Watchman clock is requested. AWACS compatibility may adopt the child's Btrfs snapshot lineage at this point. |
| `awacs` | The child cursor is cleared and no direct `BeginScan`, watch registration, or cursor initialization occurs. The first later command must establish its own direct baseline. |

Finally the child's lock is finished against the new shared operation ID.
Under no backend may the copied source cursor survive this transition.

## Step 8: Select sparsity and the destination working-copy commit

**Owner:** `cmd_workspace_add`, `merge_commit_trees`, and
`WorkspaceCommandTransaction::finish`.

With default `--sparse-patterns=copy`, Jujutsu copies the source workspace's
sparse prefixes. `empty` installs no prefixes. `full` leaves the default full
destination matcher unchanged.

The child's *final* initial commit is not necessarily the captured source
commit:

- Without `--revision`, Jujutsu uses the **parents of the source working-copy
  commit** as the new commit's parents.
- With one or more `--revision` arguments, it uses those selected commits.
- It merges the chosen parent trees, writes a new working-copy commit, edits
  that commit in the child workspace, commits the shared operation, and checks
  out the resulting tree in the child.

The source workspace keeps its own working-copy commit. Any child checkout or
sparse-policy change that changes the tree clears the child's earlier monitor
cursor; ignored files not targeted by the checkout can remain in the writable
snapshot.

**Currently broken — C-17:** A sparse source physically lacks files outside its
sparse matcher. When the destination requests `--sparse-patterns=full`, the
current code records the complete source tree but does not materialize missing
unchanged paths. A subsequent scan interprets those missing files as tracked
deletions. Widening must materialize the difference before claiming a complete
destination baseline.

## Step 9: Understand when AWACS can adopt the child

**Owner:** `src/watchman.rs::watch_project`,
`src/service.rs::adopt_snapshot_descendant`, and
`src/manager.rs::adopt_snapshot_descendant`.

The Watchman compatibility route can dynamically register the new root. It
opens the child's Btrfs subvolume, reads its `parent_uuid`, and looks for a
ready, retained parent revision on the same filesystem. A successful adoption
transaction creates:

- A new child watch ID and authorization grant.
- A new child clock epoch and child live-root UUID.
- Child indexed-head and last-cut pins referencing the retained parent seed.
- An independent watch sequence initialized at zero.

The source revision may be reused as immutable seed data; the source's clock,
grant, live root, and cursor are never copied. Sequence zero is internal watch
initialization, not proof that the child already has a client-visible direct
scan boundary.

If no eligible seed exists, current Watchman handling performs a full
initialization. The code does not distinguish a genuinely unrelated root from
a known descendant whose retained seed unexpectedly disappeared, so lineage
loss can silently become an expensive full crawl.

**Currently broken — C-10:** This adoption path exists only for Watchman root
registration. Direct `scan.sock` handling is permanently bound to the daemon's
first live root and rejects the child's first direct `BeginScan` as
unauthorized. Creating a valid Jujutsu workspace does not repair that routing
bug.

## The four independent parent relationships

```mermaid
flowchart TB
    subgraph Filesystem["Btrfs snapshot ancestry"]
        SourceRoot["Writable main root UUID A"]
        ChildRoot["Writable feature root UUID B"]
        SourceRoot -->|"child parent UUID is A"| ChildRoot
    end

    subgraph Commits["Jujutsu commit ancestry"]
        BaseCommit["Selected parent commit P"]
        SourceCommit["Main working-copy commit M"]
        ChildCommit["Feature working-copy commit F"]
        BaseCommit --> SourceCommit
        BaseCommit --> ChildCommit
    end

    subgraph Watches["Independent AWACS watch histories"]
        SourceWatch["Main watch and clock epoch"]
        Seed["Retained immutable parent revision"]
        ChildWatch["Feature watch and new clock epoch"]
        SourceWatch --> Seed
        Seed -->|"immutable seed reuse"| ChildWatch
    end

    SourceRoot --> SourceWatch
    ChildRoot --> ChildWatch
    SourceCommit -->|"temporary copied-tree baseline"| ChildCommit
```

A Btrfs snapshot parent, a Jujutsu commit parent, an AWACS historical seed, and
a directory parent are not interchangeable. In the example, `main/` and
`feature/` are directory siblings even though the child subvolume's
`parent_uuid` identifies `main/`.

## Initialization invariants

1. Snapshot optimization must not change ordinary `jj workspace add` results.
2. Optional fallback must fully materialize stock files and discard every
   snapshot-only baseline.
3. Destination and source subvolumes must have distinct writable-root UUIDs on
   the same filesystem, with a verifiable snapshot lineage when claimed.
4. A watched workspace must not gain a nested descendant subvolume.
5. The copied `.jj` identity and copied mutable Git identity must be replaced.
6. The destination must have an independent JJ name, working-copy commit,
   checkout operation, lock, tree cache, sparse matcher, and cursor.
7. Shared repository and Git object stores must outlive every workspace that
   references them.
8. Sparse widening must create missing tracked files before recording a full
   baseline.
9. A destination AWACS watch must have its own root, grant, clock epoch,
   cursor, and authorization even when its immutable index is seeded from its
   parent's history.
10. The source workspace must remain usable if destination setup, Git repair,
    registration, or monitoring fails.

Continue with [the new worktree's first changes and snapshots](/walkthroughs/first-worktree-changes/),
or compare the [Btrfs workspace integration reference](/integrations/btrfs-workspaces/)
with the [P0 correctness findings](/review/p0-correctness/).
