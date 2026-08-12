---
title: "One workspace: concurrent changes and scans"
description: "Trace edits racing with AWACS cuts, immutable direct scans, concurrent clients, leases, and durable commit boundaries."
sidebar:
  order: 2
---

Real working directories do not stop changing when `jj status` begins.
Editors, formatters, generators, and other commands can modify files while
AWACS creates a Btrfs cut or Jujutsu traverses a tree. This walkthrough
identifies which concurrency is safe, where the boundaries are, and which
reviewed direct-scan paths still need stronger guarantees.

Read [One workspace: changes in sequence](/walkthroughs/single-workspace-serial/)
first for the state notation and normal publication flow.

## Four independent concurrency domains

| Domain | Owner | Serialized operations | Operations that can still race |
| --- | --- | --- | --- |
| Live workspace | Editors, build tools, and filesystem callers. | Filesystem operations receive their normal kernel semantics. | Any external writer can run before, during, or after a cut or traversal. |
| Jujutsu working-copy state | `LockedLocalWorkingCopy`. | Jujutsu commands updating the same workspace's recorded state hold its workspace lock. | External editors and clients with independent state are not excluded. |
| AWACS watch history | Manager SQLite and fenced broker operations. | Cut ordering, indexed revision publication, and grants for a watch. | Different direct clients can request cuts or retain earlier snapshots. |
| Direct scan sessions | `FacadeScanHandler` and query leases. | Session registration, renewal, completion, and snapshot pins. | A later cut may be published while an older client still reads its pinned snapshot. |

**Important distinction:** Jujutsu's workspace lock protects Jujutsu metadata.
It is not a filesystem-wide writer lock and does not prevent an editor from
changing the live root.

### Stage-by-stage concurrency ledger

| Stage | Component and function | Concurrent read or write root | Durable state at risk | Required invariant |
| --- | --- | --- | --- | --- |
| Cut admission | AWACS `manager.rs: admit_planned_cut` and `service.rs: Service::changes` | Writers continue changing live root `L`. | Fenced operation, cut admission, ordered `watch_cuts`. | Each response joins an eligible operation or receives a later ordered cut. |
| Snapshot capture | AWACS `broker.rs` and `service.rs: finish_cut` | Read live `L` once to create immutable `Sn`. | Broker receipt, managed snapshot, physical and indexed heads. | `Sn` freezes one coherent filesystem boundary and is validated before client publication. |
| Direct invalidation | AWACS `scan_facade.rs: FacadeScanHandler::begin_scan` | Compare immutable endpoints and bind the selected snapshot. | Authenticated cursor, invalidation, query lease, and snapshot pin. | The cursor, invalidation, identity, and returned descriptor describe the same immutable target. |
| Direct traversal | Jujutsu `local_working_copy.rs: TreeState::snapshot_with_pending` | Read only immutable `/proc/self/fd/N`; external writers modify `L`. | Pending tree, file-state cache, direct cursor, and fingerprint. | Every worktree read belongs to the same pinned snapshot. |
| Lease maintenance | AWACS `scan_facade.rs: renew_scan`; Jujutsu `AwacsScanSession` | New cuts can advance the head while earlier snapshots remain open. | Durable query lease, snapshot pin, active session. | Renew and Finish remain available and use a deadline consistent with server expiry. |
| External input capture | Jujutsu `cli_util.rs: snapshot_options_with_start_tracking_matcher` and `awacs_input_fingerprint` | Read mutable ignore files and sparse indexes outside the immutable worktree. | Saved direct-cursor fingerprint. | Matcher bytes and fingerprint bytes represent the same observed external inputs. |
| Transaction completion | Jujutsu `local_working_copy.rs: LockedLocalWorkingCopy::finish` | Write live `.jj/working_copy`; immutable snapshot remains pinned. | Saved tree, file-state record, direct cursor, and operation state. | Persist the coherent tree/cursor pair before `FinishScan(Committed)`. |

## Race 1: An edit lands before or after a cut

**Where:** Btrfs snapshot creation in AWACS `src/broker.rs` and
`src/service.rs`; direct traversal in Jujutsu
`lib/src/local_working_copy.rs`.

Consider a direct scan that captures immutable snapshot `S7`.

```text
time ------------------------------->

editor A     write old-to-new bytes
AWACS                         create immutable S7
Jujutsu                                       traverse S7
editor B                                            write newer bytes
```

An edit visible when Btrfs creates `S7` belongs to the state represented by
`S7`. An edit that occurs after the snapshot boundary is not visible through
the descriptor for `S7`; it belongs to a later cut.

```mermaid
sequenceDiagram
    autonumber
    participant Editor as "Concurrent editor"
    participant Live as "Mutable live root"
    participant Daemon as "AWACS direct daemon"
    participant Snap as "Pinned immutable snapshot"
    participant JJ as "Jujutsu direct scan"

    Editor->>Live: "Write the first version of app.rs"
    JJ->>Daemon: "BeginScan from the previous cursor"
    Daemon->>Snap: "Capture and pin snapshot S7"
    Daemon-->>JJ: "Return fd and cursor C7"
    Editor->>Live: "Replace app.rs with a newer version"
    JJ->>Snap: "Read the first version through the fd"
    JJ->>JJ: "Save tree T7 with cursor C7"
    JJ->>Daemon: "Finish the committed scan"
    JJ->>Daemon: "A later command requests the next cut"
    Daemon-->>JJ: "Return a snapshot containing the newer version"
```

This is not a missed change. The first command is defined to report the
coherent state of `S7`, not a constantly updated view of `L`. The later edit
must appear in the next authenticated comparison against `S7`.

**Required invariants:**

1. Every directory entry, file, symlink, executable bit, and in-worktree ignore
   file for the scan comes from the same immutable `S7`.
2. The returned descriptor and advertised UUID identify the same read-only
   Btrfs subvolume.
3. The server-side lease keeps `S7` pinned for the entire traversal and durable
   state save.
4. The saved cursor identifies `S7`, not the later state of the live root.
5. The next command uses that exact cursor as its baseline or safely requests
   a full scan.

Jujutsu contains a debug-only integration hook that mutates the live tree after
`BeginScan`; this boundary is the appropriate place to verify that immutable
traversal still reads the originally leased snapshot.

## Race 2: A transient edit exists only between cuts

**Where:** AWACS `src/service.rs`, `src/index.rs`, and
`src/scan_facade.rs`; Jujutsu's direct branch in
`lib/src/local_working_copy.rs`.

A generator can create and remove a pathname between two immutable cuts. A
direct client must not cache that transient name unless it exists in the
snapshot the client actually traverses.

Follow a transient file `generated/tmp.rs`:

1. AWACS captures `S7`. The transient file does not exist.
2. A generator creates and then removes `generated/tmp.rs` in the live root.
3. Jujutsu continues traversing the pinned descriptor for `S7`, so it never
   observes the transient file.
4. AWACS later captures `S8`. If the transient is already gone, both
   endpoint snapshots omit the pathname.
5. The next direct scan may therefore receive no invalidation for that name,
   which is safe because no committed direct tree could have cached it.

```mermaid
sequenceDiagram
    autonumber
    participant Generator as "Concurrent generator"
    participant Live as "Mutable live root"
    participant Daemon as "AWACS direct daemon"
    participant Snap as "Pinned snapshot S7"
    participant JJ as "Jujutsu direct scan"

    Daemon->>Snap: "Capture S7 before the transient exists"
    Daemon-->>JJ: "Return fd and cursor C7"
    Generator->>Live: "Create generated/tmp.rs"
    Generator->>Live: "Delete generated/tmp.rs"
    JJ->>Snap: "Traverse S7; transient is absent"
    Daemon->>Daemon: "Capture S8 after the transient is gone"
    Daemon-->>JJ: "No transient path is required for the next scan"
```

The direct contract avoids a false-clean cache here because traversal never
switches back to the mutable live root after receiving its cursor.

**Required invariant:** a persisted direct tree contains only paths observed
through its leased immutable descriptor. If a pathname exists in the selected
snapshot, the scan must see it; if it exists only between cuts, no direct
cursor may claim that a client cached it.

## Race 3: Two status commands overlap

**Where:** Jujutsu `LockedLocalWorkingCopy`; AWACS `src/manager.rs`,
`src/service.rs`, and `src/scan_facade.rs`.

For two Jujutsu commands updating the **same** workspace, the local
working-copy lock serializes durable tree-state changes. The second command
must observe a coherent saved state after the first command finishes rather
than independently overwrite it with an older cursor.

Different direct clients or separate workspaces can still hold overlapping
leases. They share one ordered watch history:

```text
Request A               reserves cut 11
Request B               arrives while cut 11 is in progress

Safe outcomes

    B joins the eligible in-flight cut and receives its own response lease
    or
    B receives a separately ordered later cut 12

Unsafe outcomes

    publish cut 12 before cut 11 is validated
    move a client cursor backward
    release snapshot 11 while A still scans it
    combine A's old tree with B's newer cursor
```

Manager operations and fences establish publication order. Each response lease
pins the snapshot it actually consumes even if a later cut becomes the watch
head in the meantime.

The current direct dispatcher serializes handler entry across a whole
`BeginScan`, so concurrent Begin requests can queue before they reach useful
cut coalescing. That is a throughput limitation, but it must not weaken the
ordering or per-session pin invariants.

## Race 4: A long scan overlaps newer cuts and lease renewal

**Where:** AWACS `src/scan.rs`, `src/scan_facade.rs`, and
`src/manager.rs`; Jujutsu `AwacsScanSession` in
`lib/src/local_working_copy.rs`.

Direct scans may take longer than one daemon operation. An active session has
several different lifetimes:

```text
client-side retained directory fd
        +
daemon-side active session
        +
durable manager query lease and snapshot pin
        +
Jujutsu renewal owner
        +
pending working-copy transaction
```

On `BeginScan`, AWACS publishes a boundary, opens the immutable snapshot,
extends its query lease, records an active session, and returns an advertised
lease. Jujutsu starts a renewal thread while its Rayon-backed tree traversal
runs. `Renew` extends the session and durable query pin; `Finish` releases
them after the working-copy transaction commits or aborts.

Another client may legitimately publish `S8` while the first client still reads
`S7`. Replacing the watch's indexed or physical **head** cannot replace or
delete `S7` while the first scan's query lease still pins it.

**Required invariant:** head ownership and client-lease ownership are separate.
Publishing a newer cut transfers head pins, but an active response retains its
own independently protected snapshot until completion or proven expiry.

### Lease timing crosses two clocks

After a cut is prepared, `FacadeScanHandler::begin_scan` takes a fresh
wall-clock sample, renews the durable query lease from that sample, and then
advertises a boot-time deadline for the same TTL. That avoids charging slow
cut preparation against the returned lease.

The server still reclaims sessions from wall-clock expiry while the client
schedules renewal from `CLOCK_BOOTTIME`. Wall-clock adjustments and suspend
behavior can separate those clocks. Server expiry and the client-visible
deadline should use one coherent time base, or renewal should conservatively
account for the possible drift.

### Current failure: a slow Begin blocks every Renew

The direct dispatcher takes its global handler mutex before entering
`begin_scan`. `begin_scan` then holds the shared facade mutex during cut
creation and historical comparison.

```text
Session A       needs Renew before its lease expires
Session B       owns the global dispatcher mutex during a slow Begin
Broker          waits for filesystem-wide syncfs or comparison
Renew A         waits behind Begin B
Server clock    expires A before its renewal can run
```

This is finding [C-16](/review/p1-correctness/). Because direct socket
operations also lack read/write deadlines, the Jujutsu renewal thread can
remain blocked indefinitely. `prepare_to_commit` joins that thread while the
workspace lock is still held, so the original command can hang and prevent
later Jujutsu commands on that workspace from progressing.

An unrelated slow scan must not prevent renewal, abort, or completion of an
already active session.

## Race 5: External ignore inputs change during scan setup

**Where:** Jujutsu `cli/src/cli_util.rs` and
`lib/src/local_working_copy.rs`.

The immutable Btrfs snapshot freezes worktree files, but external ignore
configuration, repository-local excludes, sparse indexes, and Jujutsu settings
can live outside that immutable root. The direct cursor therefore carries an
external-input fingerprint.

The intended transaction is:

```text
read external input exactly once
    -> build matcher from those bytes
    -> fingerprint those same bytes
    -> scan immutable Sn using that matcher
    -> save (Tn, Cn, Fn)
```

The reviewed implementation instead performs two separate reads for some
ignore inputs:

```text
first read          ignore file contains rule A
concurrent edit     ignore file changes to rule B
second read         fingerprint records rule B
tree traversal      matcher still applies rule A
saved state         tree from A paired with fingerprint for B
next command        fingerprint B appears unchanged
```

The next direct scan can then accept an empty invalidation and indefinitely
retain a tree built using obsolete ignore semantics
([C-07](/review/p1-correctness/)). The immutable snapshot descriptor cannot
repair a race involving inputs stored outside that snapshot.

**Required invariant:** fingerprint bytes and matcher bytes are one coherent
observation. If an input cannot be frozen, the implementation must verify it
again and restart or force a safe scan when it changes.

## Race 6: Failure occurs around the durable commit

**Where:** `LockedLocalWorkingCopy::finish` and `PendingScan` in Jujutsu;
`FacadeScanHandler::{renew_scan,finish_scan}` in AWACS.

The important failure boundaries are not symmetric:

| Failure point | Required outcome | Why |
| --- | --- | --- |
| Snapshot descriptor fails identity validation. | Abort the server lease and do not traverse it. | An unexpected filesystem or subvolume cannot establish the advertised cursor. |
| Traversal fails. | Clear the pending cursor and abort the scan. | A partial tree cannot be paired with a successful cut. |
| Renewal fails before save. | Save no direct cursor and request a fresh scan next time. | The immutable snapshot is no longer protected by a proven live lease. |
| Workspace state save fails. | Abort the session and return the error. | The new cursor was never durably paired with its tree. |
| Finish fails after state save. | Retain the durable tree/cursor pair and recover or expire the daemon pin. | The client transaction already committed; Finish is cleanup. |
| The caller resets, checks out, changes sparsity, or discards the transaction. | Abort the pending scan and clear the cursor when the baseline changed. | The tree no longer necessarily describes the leased immutable snapshot. |
| Jujutsu finds untracked paths it does not cache. | Do not persist a cursor that would skip revisiting those paths. | Future tracking decisions require a new scan. |

An additional daemon-side race remains: the direct handler inserts its active
session before the transport writes the successful Begin response. If the
client disconnects during that write, no client holds the returned session ID.
The independent maintenance scheduler now bounds how long that abandoned
query can keep its snapshot pin, but the transport should still abort the
session immediately on response failure ([C-25](/review/p1-correctness/)).

## Race 7: The root changes identity between checks

**Where:** AWACS `src/namespace.rs`, `src/scan_facade.rs`, and
`src/manager.rs`.

A pathname is not sufficient identity. A workspace directory can be renamed,
replaced with another subvolume, observed through a different mount namespace,
or remounted between requests.

The direct handler checks namespace and root continuity before the cut, after
the cut, and before returning a response. Boundaries are tied to the watch,
grant, epoch, process-root view, namespace identity, and exact target
subvolume UUID.

**Required invariant:** a reused pathname, changed namespace, stale grant, or
mismatched snapshot UUID must invalidate continuity. It must not be accepted
as though the old cursor still described the current workspace.

This safeguard differs from the separate reviewed socket-authority issue:
connection authentication describes the original connector, not necessarily a
later process that inherited or received its connected socket
([C-12](/review/p1-correctness/)).

## Expected direct-scan outcomes

| Concurrent situation | Direct-scan outcome | Why |
| --- | --- | --- |
| Edit finishes before the selected cut. | The immutable cut contains it. | The descriptor is captured after the edit. |
| Edit lands after the selected cut. | The current snapshot excludes it; a later cut must include it. | The saved cursor identifies the earlier immutable boundary. |
| File appears and disappears between cuts. | The direct tree never caches a transient absent from its leased snapshot. | Traversal reads only the immutable descriptor. |
| External ignore policy changes. | A coherent changed fingerprint must invalidate the old cursor. | Inputs outside the snapshot are part of the saved state tuple. |
| Direct service becomes unavailable. | Fail closed rather than silently read the live root. | A live fallback would break the immutable-tree/cursor contract. |
| An unrelated Begin becomes slow. | Existing Renew and Finish operations must remain available. | Active leases are independent of newer cut preparation. |

The cross-cutting requirement is straightforward: every persisted direct
cursor must identify the exact immutable boundary that the client-side tree is
actually safe to reuse. When that cannot be established, discard the cursor,
invalidate the affected subtree, or require a full scan.

For the next repository lifecycle phase, continue to
[Initialize a new snapshot worktree](/walkthroughs/new-snapshot-worktree/).
