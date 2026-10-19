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
path. Local paths and SSH use the same synchronization protocol. Quote paths
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

Original operations, views, shared events, and exact Jujutsu commit metadata
travel separately. Operations and views are content-addressed files in the
operation store; commit metadata lives in the Git backend's extra metadata
tables. They are not stored as Git objects.

Peers supporting incremental commit metadata compare hashes of the exact
metadata in progressively smaller commit-ID ranges. Matching ranges need no
metadata payloads. A known Git commit ID alone cannot prove that its Jujutsu
change ID, predecessors, or legacy conflict data match. Installation still
checks current local metadata under its writer lock before accepting incoming
data. Older packed-transfer peers continue to send every commit's metadata.

When both Unix endpoints support immutable file transfers and have `rsync` 3.1
or newer available, original operations, views, and shared events travel as
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
before the receiver merges shared state and publishes its local integration
operation. Per-commit metadata still uses its separate negotiation and writer
lock; Git objects still use Git's packed transfer.

Peers without this optional capability, including those without `rsync`, retain
the previous ID-inventory and missing-record transfer. That path's bounded
known-ID cache can cause omitted records to be transferred again. Rsync file
lists and local history collection still examine existing history, so a small
change transfers only missing file payloads without making total synchronization
time proportional only to the change. Complete dependency validation also walks
the history before objects are retained and the integration is published.

Older peers and endpoints without native Git support continue to use the
existing object-record transfer. That path expands each object it sends, so
upgrading both endpoints is necessary to obtain packed transfers. The optional
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
repository remains available for retry. Undoing the integration restores its
previous view, rather than deleting the initialized repository.

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

Each receiving repository records its integration as a local operation, so
`jj undo` and `jj redo` work normally. Undo is itself a new causal change;
it restores that receiver's view while retaining downloaded history and the
synchronization storage format. Each receiver's publication is independent.
Receiving an unchanged peer again does not simply restore a target that was
already received and then undone. Repeated exchanges without new work do not
need additional integration operations. Native operation IDs can differ across
repositories because their local workspace state differs.

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

The first version supports Git-backed repositories with Jujutsu's built-in
operation store. First use enables synchronization storage in each participating
repository. Commands using the original format hold a shared repository lock.
The first sync waits for those commands to finish, enables synchronization under
an exclusive lock, and reopens the repository. New commands wait until that
transition finishes. Repositories already using the synchronization format do
not retain this shared lock, so simultaneous syncs can proceed independently.

Binaries predating this locking protocol do not participate in it. Commands
already running in those binaries must finish once before the first transition;
an advisory lock cannot exclude a process that never acquires it. Older binaries
that do not support synchronization reject its operation-store type on later
opens. The deployed synchronization marker and operation/view bytes are unchanged.

An exchange is not a transaction spanning two machines. One endpoint can finish
before the connection fails or the other endpoint reports an error. Retry the
same command: object identities and causal tracking make repeated transfers
idempotent. A success covers the captured histories, rather than promising that
both native operation tips are identical or that later concurrent work has
already been exchanged.

If checkout was interrupted after publication, the workspace may be stale.
Run `jj workspace update-stale` before retrying synchronization. Normal recovery
saves local edits before reconciling the workspace with the published operation;
inspect any divergent changes reported during recovery. The published operation
remains available in history.

Garbage collection retains the histories and objects referenced by
synchronization tracking, including legacy conflict trees referenced only by
Jujutsu metadata. Abandoning an operation from the local operation log does not
by itself remove histories still required by synchronization roots.

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
