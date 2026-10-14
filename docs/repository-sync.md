# Synchronizing repositories

`jj sync` transfers Jujutsu operations, their original views, and the commit
objects and Jujutsu metadata those histories need. Use it to exchange work
between repositories when Git fetch and push would omit operation history.

Choose a direction explicitly:

```shell
# Incorporate another repository's history here.
jj sync --receive ../other-workspace

# Incorporate this repository's history in the other repository.
jj sync --send ../other-workspace

# Exchange and reconcile both captured histories.
jj sync --bidi ../other-workspace
```

The peer can also be a workspace accessed over SSH:

```shell
jj sync --bidi 'ssh://user@example.com/home/user/project'
jj sync --receive 'ssh://user@example.com:2222/home/user/project'
```

The remote machine needs a compatible `jj` executable on its command search
path. Both endpoints must support native operation-graph synchronization; older
sync implementations are rejected before a destination is initialized or an
existing repository is upgraded. Local paths and SSH use the same protocol. Quote paths
containing spaces as you would for other shell commands.

SSH URL paths are absolute on the remote machine: `ssh://host/code/project`
selects `/code/project`. For a repository under the remote user's home, use
its full path, such as `ssh://host/home/user/code/project`. Tildes and shell
variables in the URL are passed literally.

Synchronization describes its current local phase and waits for the peer on
stderr. These messages cover history collection, comparison, transfer,
validation, and integration, and appear even when output is redirected.
Routine helper output is suppressed; helper diagnostics are included when
synchronization fails. Interactive terminals also show transfer counters. Set
`ui.progress-indicator = false` to disable those interactive updates while
keeping phase descriptions. Use `--quiet` or `ui.quiet = true` to suppress
status messages; errors remain visible.

Use `jj --debug sync ...` to trace endpoint paths, negotiation, and transfer
phases, including helper diagnostics. Debug logs follow the usual logging
configuration:

```shell
jj --debug sync --send 'ssh://host/home/user/code/project'
```

## Git object transfer

When both peers support native packed transfers and have Git available, Unix
endpoints use Git's ordinary push or fetch transport for Git objects. Temporary
refs cover the heads of the frozen retained history, including disconnected
histories needed only by old operations or predecessor metadata, and legacy
conflict trees. Ancestor commits do not need individual refs. These refs live in
disposable bare repositories; they do not change either repository's bookmarks
or ordinary Git refs.

Git negotiates objects already present at the receiver and transfers compressed
packs with deltas. Received objects stay packed during quarantine and after
installation, instead of being staged as expanded blob bodies. Both local paths
and SSH carry Git's protocol over the existing synchronization connection;
receiving never requires a reverse SSH connection. Bidirectional sync runs the
two transfers in sequence.

Original operations, views, and exact Jujutsu commit metadata
travel separately. Operations and views are content-addressed files in the
operation store; commit metadata lives in the Git backend's extra metadata
tables. They are not stored as Git objects.

Peers supporting incremental commit metadata compare hashes of the exact
metadata in progressively smaller commit-ID ranges. Matching ranges need no
metadata payloads. A known Git commit ID alone cannot prove that its Jujutsu
change ID, predecessors, or legacy conflict data match. Installation still
checks current local metadata under its writer lock before accepting incoming
data. Compatible peers without this optimization send every commit's metadata.

When both Unix endpoints support immutable file transfers and have `rsync` 3.1
or newer available, original operations and views travel as
immutable files. The sender prepares a temporary tree containing exactly the
captured history. The receiver prepares a read-only comparison snapshot of its
pinned retained history, including operations imported by earlier exchanges.
Rsync first lists missing files without copying their contents. The receiver
checks their names, sizes, count, and total disk budget before requesting them.
Rsync then copies only those files into a private incoming directory, with
compression enabled. Original content identities are checked after copying.
The transfer adds objects; it does not propagate deletions or copy mutable
operation heads, synchronization tracking, repository settings, or workspaces.
Both local paths and SSH carry rsync's protocol over the existing synchronization
connection, without another SSH connection.

Received files are validated under their original content identities and retained
before the receiver merges shared state and publishes its integration as an
ordinary operation with local and received history as parents. Per-commit
metadata still uses its separate negotiation and writer lock; Git objects still
use Git's packed transfer.

Peers without this optional capability, including those without `rsync`, retain
the previous ID-inventory and missing-record transfer. That path's bounded
known-ID cache can cause omitted records to be transferred again. Rsync file
lists and local history collection still examine existing history, so a small
change transfers only missing file payloads without making total synchronization
time proportional only to the change. Complete dependency validation also walks
the history before objects are retained and the integration is published.

Compatible endpoints without native Git support use the object-record
transfer. That path expands each object it sends, so both endpoints need the
packed-transfer capability to avoid expansion. The optional
transport capability does not change the stored synchronization format.

Native Git uses the backend object database through alternates and has the
same trust assumptions as ordinary Git push and fetch. Choose peers trusted
with this repository's object data; temporary refs do not isolate that data
from a malicious peer.

## Creating a destination

Sending to a path without a Jujutsu repository initializes a colocated
Git-backed repository there, then synchronizes normally. This works for local
paths and SSH destinations, with both `--send` and `--bidi`:

```shell
jj sync --send ../new-repository
jj sync --bidi 'ssh://user@example.com/home/user/new-repository'
```

To receive or sync bidirectionally into a new local destination, select it with
`-R`, or run from the destination directory outside any existing Jujutsu
workspace:

```shell
jj -R ../new-repository sync --receive ../source-repository
```

Initialization follows `jj git init --colocate`, even when `git.colocate` is
disabled. The destination's parent directory must already exist. New Git
storage uses the source's object hash algorithm. Existing files are preserved
and participate in the usual working-copy snapshot unless
`--ignore-working-copy` is given. Existing repositories are opened normally;
an unreadable or broken `.jj` is an error. A missing source is also an error
and is never initialized automatically. At least one endpoint must already
contain a repository.

The destination has its own initialized workspace; synchronization does not
check out the source's `@`. If a transfer fails after initialization, the new
repository remains available for retry. Restoring an earlier operation restores
its view, rather than deleting the initialized repository.

## Shared history and local workspaces

Synchronization reconciles visible commit heads, bookmarks, tags, and remote
reference state. It preserves original operation and view objects, including
commit identities, change IDs, and predecessor metadata. Imported original
operations can be inspected by their operation IDs.

This transfers retained history, including hidden or abandoned commits and
snapshot content referenced by original operations. Choose peers with whom you
intend to share that history.

Active workspaces remain local. A peer's workspace names, working-copy commit
map, Git HEAD observations, and observations of physical Git refs do not replace
those of this repository. Two independently created repositories can both have
a workspace named `default` and keep different `@` commits. Their original
historical views still describe the workspaces that existed when those
operations were created. Remote URLs, repository configuration, and working-copy
files are not copied as repository settings or workspace state.

Independent clones are supported. There is currently no common-ancestry check:
completely unrelated repositories can also be synchronized. This retains their
separate commit roots; it does not create a commit merging their file contents.
Concurrent changes to the same bookmark or tag can produce the usual reference
conflicts.

Reconciliation uses Jujutsu's ordinary operation-merge behavior, including
rebasing descendants of rewritten commits. For example, suppose both machines
start with commit `P`. Machine A rewrites it as `P'`, while machine B creates
child `C` on the original `P`. Sync can rebase `C` onto `P'`, producing `C'`.
The imported original commits and operations remain available in history.
Concurrent rewrites of the same change remain divergent versions; sync does not
choose one as the winner. File conflicts from rebasing are recorded normally.

Each receiving repository publishes an ordinary merge operation. Its first
parent is the receiver's previous local operation; the other parents retain the
received history. Original operation and view IDs are unchanged. The resulting
view reconciles shared fields while retaining local workspace and Git
observations. Imported operations are then ordinary ancestors: inspecting
history and retaining it during garbage collection use the same operation graph
as local work.

`jj undo` and `jj op revert` do not cross merge operations, including sync
merges. There is no separate `jj sync undo` command. To recover the view from
before a sync, use `jj op log` to find the previous local operation, inspect it
with `jj --at-op=<operation ID> log`, then run:

```shell
jj op restore <previous-local-operation-ID>
```

This creates an ordinary local operation; it does not delete downloaded objects
or the sync merge from history. Later syncs respect that restore as new work,
so receiving the same unchanged peer does not reapply the old result. Ordinary
undo and redo still work on later operations that have a single parent.

Repeated exchanges without new work do not create more merge operations.
Receivers can produce different operation IDs because their workspace state
and local parent order differ. Sync recognizes a local publication wrapper only
when its recorded digest still matches its parents and shared view, and one of
its actual parents has the same shared view and already contains the verified
authored history of every other parent. That wrapper adds no shared work beyond
the parent, so sync follows that parent's history to compare the authored frontier. A digest
alone is insufficient proof.

The ordinary merge that actually reconciles shared changes remains a real
operation, including any descendant rebases it produced. Local wrappers around
that same result can differ while representing the same authored frontier. If
both endpoints already represent that frontier, there is nothing to merge.
Sync advertises that ordinary result directly; a receiver-only wrapper becomes
part of transferred history when a later authored operation descends from it.
Ordinary operations, including restores and workspace renames, contribute their
own IDs even when their shared views are unchanged. Rewriting a wrapper's
parents invalidates its digest and makes it a new operation for synchronization.
This convergence check belongs to `jj sync`; running `jj converge` is unnecessary.

## Working copies and snapshots

Like other Jujutsu commands, synchronization snapshots the selected workspace
before capturing its history. This happens on both endpoints, including the
source of a one-way transfer. Each endpoint uses its own repository settings.
Only the workspace selected by the endpoint path participates; other workspaces
in that repository are not automatically snapshotted.

Use the existing global option to skip snapshots and working-copy updates on
both endpoints:

```shell
jj sync --bidi ../other-workspace --ignore-working-copy
```

Normal finalization updates the participating workspace where needed. It does
not check out the peer's `@` commit. Changes created while synchronization is in
progress remain local work and can require another exchange after the captured
history has been transferred.

## Compatibility, retries, and retention

Synchronization supports Git-backed repositories with Jujutsu's built-in
operation store. First use enables the native synchronization format,
`simple_op_store_sync_v2`, in each participating repository. Commands using the
original format hold a shared repository lock. The first sync waits for those
commands to finish, changes the format under an exclusive lock, and reopens the
repository. New commands wait until that transition finishes. Repositories
already using a synchronization format do not retain this shared lock, so
simultaneous syncs can proceed independently.

Binaries predating this locking protocol do not participate in it. An already
running old process can race with the first transition because an advisory lock
cannot exclude a process that never acquires it. Older binaries reject the new
operation-store type on later opens.

Repositories using the deployed `simple_op_store_sync_v1` format are upgraded
automatically after both endpoints confirm native synchronization support.
Migration preserves the current local view and every retained original
operation and view ID. Legacy shared events that carried additional historical
views become ordinary operations, and an ordinary merge connects all retained
history to the local operation graph. After that publication, completed
acknowledgments and legacy shared-event roots no longer retain history.
Interrupted transfer pins and publication journals remain protected until their
session is finished or retired. Migration does not alter original operation or
view bytes.

Upgrade both machines before the next exchange. A native-format repository
cannot be opened by an older sync binary, and a native endpoint rejects an old
peer before initialization or migration. The protocol still negotiates optional
packed Git, immutable-file, and incremental commit-metadata transports between
compatible native endpoints.

An exchange is not a transaction spanning two machines. One endpoint can finish
before the connection fails or the other endpoint reports an error. Retry the
same command: object identities and causal tracking make repeated transfers
idempotent. A success covers the captured histories, rather than promising that
both native operation tips are identical or that later concurrent work has
already been exchanged.

If checkout was interrupted after publication, normal Jujutsu recovery retains
the physical files as working-copy edits, including edits made after the
interruption, while the published operation remains available in history.

Garbage collection follows ordinary operation parents and views, including
imported history and legacy conflict trees referenced by Jujutsu metadata.
Temporary transfer pins and unfinished publication journals protect work in
progress. Completed syncs do not add permanent roots outside the operation
graph. As with local history, `jj op abandon` can remove ancestry and allow its
objects to be collected. Pruning can also remove the ancestry proving that a
peer's work was already received; a later sync may receive that work again.

## Receiver resource limits

Each receiving endpoint applies its own resource limits. Set
`sync.max-quarantine-bytes` to a positive byte count or a human-readable size
such as `"10GiB"` to limit staged incoming data. Its default budget is derived
from available disk space. The positive integer settings `sync.max-records`
and `sync.max-dependencies` limit staged records and dependency processing.
`sync.max-dependencies` also limits the total wire fields in each protobuf
metadata record, including nested fields, before that record is decoded.
Operators can raise these limits for larger transfers. The sender cannot
override the receiver's settings.

The immutable-file transport checks the planned uncompressed payload and file
count against local staging limits before copying files, including space for
canonical installation. It permits at most 16 MiB of file-list and protocol
overhead beyond the relevant payload budget. Copied files must match the plan
and pass dependency validation before installation. Very large file lists can
require a larger payload budget even when little new file content is transferred.
Rsync checks declared file sizes before decompression; Jujutsu checks actual
sizes after copying. Hard disk bounds during decompression require an
operating-system storage quota, as with Git's native transfer.
The older transport's bounded cache
of known object IDs only avoids retransmission: IDs omitted from that cache are
transferred again, and its size does not limit repository size.
