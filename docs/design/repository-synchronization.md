# Native Jujutsu repository synchronization

A laptop and a build machine can each have a workspace called `default` and
point `@` at different commits. Suppose the laptop rewrites a commit while the
build machine adds a child to the old version. `jj sync` lets the two machines
exchange those changes, including the operations that produced them. Each machine
keeps its own checkout. Git fetch and push alone cannot carry this history.

This RFC describes the native operation graph on `dev/adamh/repo-sync`. The
first design used a second graph and supported `jj undo` after a sync. The
migration section explains how jj reads repositories that used that design.

## What a sync does

Choose one direction. The path identifies a workspace and, through it, the
repository. An SSH peer needs a compatible `jj` on its command search path.

| Command | Who receives the history |
| --- | --- |
| `jj sync --receive ../build` | The invoking repository receives from `../build`. |
| `jj sync --send ../build` | The repository at `../build` receives from the invoking repository. |
| `jj sync --bidi ssh://user@host/home/user/project` | Both repositories apply one result for the histories they captured. |

Both endpoints first recover and snapshot the selected workspace using their own
settings. Even a send can record a new operation at the source. Each endpoint
snapshots and updates only the selected working directory.
If sync rebases a commit in another local workspace, that workspace's recorded
commit can move while its files stay in place. Use `--ignore-working-copy` to
skip snapshots and checkout updates at both ends throughout the exchange. Sync needs current operation
heads and rejects `--at-operation` and `--no-integrate-operation`.

At least one repository must exist. A missing destination can be initialized as
a colocated, Git-backed repository if its parent directory exists. For example,
`jj -R ../new-build sync --receive ../laptop` creates the receiver and uses
the source's Git object hash format. A new workspace has its own `@`. Files
already in the directory participate in its normal snapshot. A missing source
or a broken repository marker is an error.

## What the peer can see

| Data | What jj does |
| --- | --- |
| Visible commit heads, local bookmarks and tags, and remote reference state | Merge them with the corresponding fields from the other repository. |
| Operations, views, commits and their dependencies | Transfer original IDs and bytes, including change IDs, predecessor metadata, conflict trees, and hidden or abandoned work still in retained history. |
| Active workspace names and checkout pointers, per-workspace Git HEAD, and observations of physical Git refs | Use the receiver's local values when building its new view. |
| Workspace registrations, physical working files, repository settings, remote URLs, hooks, and credentials in local configuration | Leave them on the machine that owns them. |

A file saved in a working-copy snapshot travels as commit content. A historical
view can contain workspace names and working-copy commit IDs even though sync
never assigns the peer's `@` to a local workspace. Choose a peer that can see
the entire retained history. Native Git transfer gives the peer the same access
to Git object data as an ordinary Git push or fetch. Temporary transfer refs do
not restrict a malicious peer to the listed heads.

Physical Git refs need separate handling. If sync changes a shared remote tag
while the physical ref stays put, the next Git import must recognize that the
physical ref has not moved. Each endpoint keeps its own last observation and
compares it with the actual ref. It initializes remote-tag observations before
applying incoming state. Later fetches, deletions, and remote renames can then
detect real local Git changes.

## How the two histories merge

In the laptop example, let `P` be the original commit. The laptop rewrites it
as `P′`. The build machine creates child `C` on `P`. Sync can rebase `C`
onto `P′`, producing `C′`. Both endpoints can now refer to the same `C′`.
The original `P`, `C`, and their operations are still available in the
received history.

1. Each endpoint captures a fixed operation and protects the objects reachable
   from it. If the loaded operation is no longer the only active head, jj
   reloads it and resolves concurrent heads through the ordinary operation log.
   It makes at most eight capture attempts before asking the user to retry.
   The sender's first offer must match the history it advertised.
2. The receiver finds the common operation ancestors. It merges multiple bases
   recursively and uses the empty shared view when there is no base. At each
   merge it extracts the shared fields from the base and both inputs, builds
   the required commit index, then applies jj's reference and rewrite rules.
   This keeps a peer workspace move from being interpreted as a local rewrite.
3. For `--bidi`, the initiator chooses one result and sends that operation
   and any commits created by the merge. The peer checks that it covers both
   captured histories and confirms the exact operation ID before either end
   applies it. Computing `C′` separately could give the machines different IDs.
4. Each receiver checks its current operation again. If another command ran
   during transfer, jj merges that work with the selected result. When there
   is new history to apply, the receiver records an ordinary merge operation.
   Its first parent is the previous local operation. Its other parents connect
   the received history to the local operation log.

Independent clones and unrelated repositories can exchange history. Unrelated
projects keep their separate commit roots. Concurrent moves of the same
bookmark can produce a reference conflict. Two rewrites of the same change can
remain divergent. Rebasing a child can produce a file conflict, just as it can
outside sync.

### Why a second sync can do nothing

The two machines can write different operation IDs when they apply the same
result. Their local workspace fields and parent order can differ. To decide
whether it has already received the peer's work, jj compares the operations
that actually contributed to the shared history.

jj can look through a local integration operation only when its recorded
digest matches its actual parents and shared view. One actual parent must also
have the same shared view and contain the verified history of every other
parent. An operation that passes these checks represents work already present
in that parent. A merge that rebased a commit, an ordinary empty operation, a
workspace rename, and a restore can each contribute new history. Equal shared
views alone cannot establish that two operations represent the same work.

This rule also works when a third machine joins the exchange. A successful
sync covers the advertised histories. Work created after capture can require
another exchange. In a two-repository check of the `P′` and `C′` example,
the first repeat recorded one more operation merge without changing the
commits. Two further exchanges with `--ignore-working-copy` changed neither
operation tip.

## How data reaches the receiver

Local paths and SSH both run `jj util sync-server` over a framed connection.
SSH uses its normal authentication and host checks. The remote helper command
is fixed. The workspace path travels in the protocol, so spaces and shell
characters in a path stay data. SSH URLs use an absolute path and can specify a
port. The helper reserves stdout for protocol frames and drains stderr
separately. On Unix, cancellation can stop the whole helper process group.

Before initialization or format changes, the endpoints check protocol
capabilities, backend, operation-store format, object hash, and working-copy
policy. They can choose different transfer methods for three kinds of data.

| Data | Transfer and verification |
| --- | --- |
| Git commits, trees, and blobs | On compatible Unix peers, Git negotiates compressed packs over the existing connection. Temporary refs in disposable bare repositories cover retained heads, disconnected history, and legacy conflicts. Received objects stay packed. The fallback sends object records and streams large bodies in chunks. |
| Original operations and views | With rsync 3.1 or newer on both Unix peers, the sender exposes only the captured immutable files. The receiver compares them with its pinned history, checks the list of missing files, then copies those files with compression into a private directory. Other peers exchange known IDs and send missing records. |
| Jujutsu commit metadata | Peers that support it compare hashes of exact metadata over successively smaller ranges of commit IDs. Matching ranges need no payload. Other peers send all required metadata. Identical Git commit IDs alone cannot establish that change IDs, predecessors, or legacy conflict data match. |

A checksum can detect damage in transit. The receiver also checks record
identities, historical encodings and dependencies, including objects referenced
only by old operations or predecessor metadata. An operation ID need not equal
the hash of its serialized protobuf bytes. The receiver validates the supported
identity and keeps the original bytes. It checks for conflicting immutable
commit metadata again under the writer lock. It installs objects and saves the
roots that GC must protect before making an incoming view active. The command
exchanges jj tag targets and does not promise a mirror of physical annotated
Git tags.

## If the exchange stops

One machine can finish before the other loses its connection. Retrying the
same command is safe. The receiver may download a record again or recompute a
merge. It checks the history before applying it.

| Where it stopped | What happens on recovery |
| --- | --- |
| Transfer or validation | Incoming files stay separate until they pass the checks. The active view still refers to the earlier state. |
| The receiver installed objects but has not made the operation active | Saved roots protect the objects. Recovery can retire the incomplete session after its owner exits and retry against the current local operation. |
| The operation became active, but the acknowledgment was lost | Recovery checks active operation ancestry, including newer heads. The receiver may already have applied the result. |
| One end of `--bidi` finished | That end keeps its result. Retrying lets the other end complete. |
| Checkout stopped after the operation became active | The operation stays in history. Normal checkout recovery preserves physical files as working-copy edits, including files edited after the interruption. |

A transfer pins outgoing history before waiting on the network. A receiver
takes a separate process lease before it records a pending integration and
holds it until the publishing process finishes. During finalization, jj uses
its ordinary workspace and Git locks. Under the sync/GC lock it records the
exact final operation in a write-ahead journal before making that head active.
It releases the sync/GC lock before waiting on the network or acquiring a
workspace lock. A lost response or an old timestamp cannot establish that a
session owner has exited.

To restore the view from before a completed sync, find its first parent with
`jj op log` and inspect it with `jj --at-op=<operation-id> log`. Then run

```sh
jj op restore <previous-local-operation-id>
```

This records a new local operation. The merge and its objects stay in history.
A later exchange can share the restore, and receiving the same peer again
does not reapply the changes the restore reversed. `jj undo` and `jj op revert` stop
at merge operations. Ordinary undo and redo still work on later single-parent
operations.

## Storage, pruning, and migration

In the native format, `simple_op_store_sync_v2`, received operations become
ordinary ancestors. GC follows operation parents, views, commits, predecessors,
and legacy conflicts. Incomplete sessions and transfers have additional roots.
After a session finishes, the operation graph keeps the accepted history alive.
Missing or invalid tracking in an enabled store is an error. Recreating empty
tracking could let GC delete protected history.

`jj op abandon` can remove ancestry locally. It can make objects eligible for
GC and erase the evidence that a repository already received an operation.
A later sync may receive that operation again. Pruning the local log does not
tell other machines to prune theirs.

The first implementation (`simple_op_store_sync_v1`) kept original operations
and a second graph of shared events. Each event recorded a projected view,
sorted parent IDs, and, where applicable, its original operation ID. An
ordinary operation still produced an event if the shared view stayed the
same. In a two-way exchange the initiator chose one shared result. Each
receiver wrote a single-parent local operation so ordinary undo could restore
its earlier view. It could reuse the shared result only if the normalized
baseline was its ancestor and the final shared view matched. Finalization
that added a shared change produced a new descendant.

That second graph needed its own GC roots and pruning checkpoints. Before
pruning, v1 saved the shared history of every active native head. At the next
safe single-head capture it joined that history to the actual current view.
Imported raw tips stayed reachable through separate roots. Format v2 records
these relationships in the native graph and uses operation restore for recovery.

Both machines need a binary that understands native sync. On first use,
jj waits for commands that hold the format lock. Finish commands running older
binaries before the first sync. An older process that already opened the store
can race with the upgrade because it never took that lock. Older binaries
reject the new format on subsequent opens. For an
existing v1 store, migration preserves the current view and all retained
original operation and view IDs. It converts additional views from shared
events into ordinary operations and joins the retained histories in a merge.
Only after saving that merge can jj retire completed v1 roots. Unfinished
transfers and journals keep their protection.

## Receiver limits and cost

The receiver sets its own limits. By default it reserves the smaller of 1 GiB
or 10 percent of available disk space, then allows staging to use half of
what is left. A positive `sync.max-quarantine-bytes` overrides that budget.
Checks include per-file overhead and room for installation. Git and rsync can
decompress data before jj checks its actual size. A hard limit on disk use
during decompression requires an operating-system quota.

| Limit | What it covers |
| --- | --- |
| `sync.max-records`, default 1,000,000 | Staged records and records visited when checking dependencies, including records the receiver already has. |
| `sync.max-dependencies`, default 1,000,000 | Dependency work and protobuf fields per metadata record, including nested fields, before decoding. |
| Fallback inventory | Up to 65,536 known IDs. Each batch holds at most 512 IDs and 96 KiB of JSON. An omitted ID can cause a retransmission. |
| Wire data | A phase-control message can use 64 KiB, a protocol frame 128 MiB, and a streamed Git chunk 1 MiB. Large Git objects span chunks. |
| Immutable file transfer | jj checks file names, counts, uncompressed sizes, and installation space before copying. File-list and protocol overhead may use up to 16 MiB beyond the applicable payload budget. |

A first exchange may visit all retained history. A later exchange can send
only missing payloads and still spend time listing files, comparing metadata,
and validating dependencies. Graph and inventory memory can grow with history.
Other backends, background exchange, named peers, and deletion coordinated
across repositories would need separate designs.

## Implementation and evidence

The command and transport live in `cli/src/sync_endpoint.rs`. The merge and
repeat-sync checks live in `lib/src/sync/native.rs`. The branch tests cover
three-peer exchanges, descendant rebases, restore and pruning, v1 migration,
and interrupted transfers and checkouts. See `lib/tests/test_native_sync.rs`
and `cli/tests/test_sync_command.rs`.

The saved v1 prototype at `73fbf6e0` passed 59 library unit tests, 19 library
integration tests, 20 command and transport unit tests, and 28 command
integration tests. Its SSH tests used a stand-in helper. Windows APIs received
source review only. Failure injection exercised the identified steps for making
an operation active. Four broader working-copy and ignore tests failed, and
workspace-wide formatting and lint checks had existing issues. These results
belong to that prototype.

The original plans and decision log record the design as it evolved.
[Original plan](https://chatgpt.com/space/page_3047a1fac6088191960dd9ece015c993) ·
[Implementation plan](https://app.chatgpt.com/space/page_6e3b022b95908191b982d9d75c79392f) ·
[Decision log](https://app.chatgpt.com/space/page_69026b9d1a9881919e400e5fea6ac6c5)

[Command guide](../repository-sync.md) ·
[RFC in Pages](https://chatgpt.com/space/page_5d42524a317c819181b2914566d74923)
