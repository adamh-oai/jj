# Direct AWACS snapshot scans

Status: draft

## Summary

This document proposes a direct integration between Jujutsu and the embedded
AWACS library. The library coordinates a Btrfs-backed read-only snapshot,
records a logical cut and its changed paths, and returns a complete delta from
the prior logical baseline. The integration lets Jujutsu scan an immutable
filesystem view instead of querying a filesystem monitor and then crawling the
live working copy.

The current Watchman integration is intentionally path-oriented: it returns a
clock and a set of names that may have changed. That is a good fit for an event
monitor, but it cannot express the snapshot lease, read root, and completion
acknowledgment needed to make a Jujutsu snapshot correspond to one immutable
filesystem state. AWACS should therefore be a separate backend, not a
Watchman-compatible extension.

## State of the feature

`fsmonitor.backend` accepts `none` and `watchman`. Watchman still queries a
clock and changed-name list, but after removal of the durable per-path cache
those names are not sufficient to prove unchanged semantic paths on a mutable
live root. The compact-journal implementation therefore conservatively
full-scans for Watchman and does not persist its clock as a reusable physical
baseline. Snapshot-backed subvolume mode selects AWACS internally because it
supplies an immutable scan root plus a complete delta or `Full`.

That ordering has a race:

1. Jujutsu queries the monitor and receives clock B.
2. Jujutsu scans the live working copy.
3. A path can appear, be observed by the scan, and disappear before the next
   monitor boundary C.
4. An endpoint-only B-to-C comparison can be empty even though Jujutsu cached
   the intermediate state.

An immutable B snapshot removes that race. If Jujutsu reads snapshot B, a
mutation after B cannot affect the scan. An endpoint-equal mutation between B
and C is then irrelevant to Jujutsu because the client never observed it.

The direct backend does not replace Watchman. Watchman remains useful on
non-Btrfs filesystems and for deployments that want an ordinary event monitor,
but it is not an authoritative no-row incremental baseline.

## Goals and non-goals

### Goals

* Make every successful AWACS-backed Jujutsu snapshot correspond to one
  read-only Btrfs snapshot and one durable Jujutsu baseline binding.
* Preserve incremental scans from a retained baseline: given exact snapshot A,
  AWACS returns a complete A→B delta or `Full`, and Jujutsu reads only the
  affected paths from B when completeness is proved.
* Keep all Jujutsu state, locks, checkout writes, and colocated Git metadata on
  the live working-copy root.
* Fail conservatively: an unavailable, expired, pruned, ambiguous, or
  incompatible AWACS session must not publish a clean baseline or silently
  claim snapshot-correctness.
* Keep AWACS optional and Linux-only. Jujutsu must not acquire a hard
  dependency on Btrfs or a custom kernel.
* Preserve the existing watchman backend as a correct conservative fallback.

### Non-goals

* Making AWACS a general Watchman replacement.
* Sending snapshot paths or lease identifiers through the Watchman protocol.
* Using a read-only snapshot as the target of checkout, sparse-pattern
  updates, jj file untrack, or other working-copy writes.
* Solving concurrent-write consistency for the existing none and watchman
  backends.
* Requiring AWACS for ordinary Jujutsu installations.
* Supporting nested subvolumes, fscrypt, cross-filesystem working copies, or
  other layouts that AWACS itself rejects.

## Correctness model

An AWACS baseline is valid only when it names the exact immutable snapshot
from which Jujutsu produced its current semantic tree and sparse view, and the
backend can either prove that snapshot is still available or return `Full`.

For one successful snapshot:

~~~
live root:       W
baseline:        tree X ↔ retained snapshot A
AWACS cut:       B
delta:           complete A→B delta or Full
scan root:       read-only snapshot B
persisted state: tree Y ↔ retained snapshot B
~~~

The following invariants are required:

1. Jujutsu never publishes baseline B unless all filesystem reads used to
   build tree Y came from snapshot B.
2. Jujutsu never uses a baseline after changing the working-copy semantic tree
   without materializing matching files or deliberately invalidating it.
3. AWACS retains the logical A→B event range while deriving B and returns an
   already-open B descriptor before Jujutsu publishes the clean B binding.
   AWACS must prove A for every delta or return `Full`.
4. A failed or abandoned session does not advance the persisted baseline.
5. The live root is used only for state and mutation operations while the scan
   session is active.
6. AWACS must validate the live root's path binding, mount topology,
   filesystem UUID, and subvolume UUID before minting B. Jujutsu must treat a
   continuity failure as a fresh/full-scan boundary.
7. The scan root must be private and immutable for the lease lifetime. A
   caller-controlled path that can be renamed or mounted over is not a valid
   scan root.
8. Exact-path and prefix deltas are authoritative complete descriptions for
   Jujutsu-observable changes; any uncertainty returns `Full`.

This model changes jj status from best effort observation of a changing
directory to status at snapshot B. Writes after B appear on the next Jujutsu
command.

## Proposed user interface

Snapshot-backed subvolume mode selects AWACS internally. Jujutsu embeds the
AWACS library and opens the initialized per-root state directly; there is no
daemon socket to discover or configure. Unsupported platforms and
uninitialized roots fail with actionable errors.

The first version should not expose trigger configuration. Background snapshot
triggers can be designed later against the direct protocol rather than copied
from fsmonitor.watchman.register-snapshot-trigger.

## btrfs-awacs library API

Jujutsu should use the versioned `btrfs-awacs` Rust library API. The library
owns direct per-root coordination, cursor handling, and snapshot-fd lifetime;
`jj-lib` must not implement those details. Only the bounded Btrfs broker
remains privileged and deployment-specific behind the library boundary.

AWACS needs a dedicated scan lease plus a committed-baseline handoff. It should
require ordinary read/cut authority, retain only the prior baseline A and the
candidate B selected by BeginScan, and expose an idempotent way to promote B
without giving Jujutsu broader retention permission or arbitrary snapshot
selection. The existing Watchman response fence is too short because it ends
when the protocol response is written, while Jujutsu must hold B through local
journal publication and keep it retained for the next command.

### Begin scan

~~~
BeginScan {
  live_root,
  baseline_owner_id,
  previous_baseline?,
}

BeginScanResult {
  candidate_baseline,
  snapshot_fd,
  delta = Full | ExactPaths([...]) | Prefixes([...]),
  expires_boottime_ns,
}
~~~

snapshot_fd is an already-open read-only directory fd plus identity metadata:

~~~
SnapshotLocator {
  fd,
  filesystem_uuid,
  subvolume_uuid,
  read_only = true,
}
~~~

Jujutsu may use /proc/self/fd/N as an ephemeral path while retaining the fd
for the full lease, because TreeState::snapshot() is path-based today. It must
not canonicalize that path. A later implementation should teach the scanner to
use openat() directly. Returning a caller-visible filesystem path is not
sufficient: it can be renamed, mounted over, or become inaccessible while the
scan is in progress.

ExactPaths and Prefixes contain repository-relative byte strings. In the
snapshot-baseline design they are authoritative complete delta shapes, not
advisory scan-narrowing hints. If AWACS cannot prove a complete incremental
range, it returns Full and Jujutsu scans all sparse paths in snapshot B.
Prefixes are a direct-API optimization for directory invalidation; they are
not a Watchman extension.

### Renew scan

~~~
SnapshotLease::renew()
~~~

Jujutsu renews a lease only while it is actively scanning or waiting to
publish the resulting baseline journal. A renewal failure aborts the session
before any new clean baseline is saved.

### Promote baseline

~~~
SnapshotLease::promote()
~~~

Promotion marks candidate B as pending while A remains committed. A crash
after promotion but before local journal publication may leave an orphan
logical handoff, but the next Begin reconciles it against the baseline named
by the journal.

### Finish scan

~~~
SnapshotLease::finish(outcome = Committed | Aborted)
~~~

Committed means Jujutsu durably published its clean tree-Y↔B journal binding.
It releases the prepared scan session. Aborted means no clean B binding was
published; AWACS may reclaim unneeded physical paths during bounded cleanup.
Finish is idempotent so a retry after an interrupted call is safe.

The compact journal persists a stable opaque owner ID beside its typed
baseline. AWACS keeps logical event history needed to replay retained cursors;
physical snapshot pathnames are not durable consumer references.

### AWACS index and replay storage

AWACS has one authoritative path index per watch. A cut derives its bounded
changed-inode subset from that index, validates the target, and updates only
the touched object/reference/owner rows in the same SQLite transaction that
advances the indexed head. Adjacent cuts do not create immutable path-map
overlay revisions and do not periodically compact an overlay chain.

The private spool file is the canonical replay journal for a retained cut. It
contains the broker-verified changed-object stream followed by a fsynced,
hashed encoding of the already-projected conservative path events. SQLite
stores only the cut sequence, endpoint snapshot IDs, spool pathname, payload
hash, and event count. For a stale cursor at B and current cut D, replay reads
and unions the retained C and D spool payloads. A missing file, hash failure,
sequence gap, or cursor older than the replay floor returns Full.

Retention first removes the catalog rows transactionally with the old logical
cuts, then unlinks the no-longer-referenced spool files after commit. A crash
before catalog commit may leave an unreferenced spool file, but can never leave
a committed replay row whose payload was not fsynced first.

### Errors

The library API must distinguish:

* unsupported filesystem/layout;
* unavailable or uninitialized AWACS root;
* invalid, expired, or pruned previous baseline;
* fresh/full-scan-required continuity loss;
* snapshot lease expiration;
* authentication or authorization failure;
* incompatible library/state version;
* malformed response.

The embedded coordinator must validate the process root and mount namespace;
Jujutsu must not supply self-asserted filesystem identity as authority.

Invalid, expired, or pruned previous baselines are not errors: AWACS should
return a valid target snapshot with delta = Full. Other errors before a scan
session is accepted fail the AWACS-backed snapshot operation without
publishing a new baseline. Errors after a session is accepted abort that
session and fail the AWACS-backed snapshot operation. Jujutsu must never keep
using an older AWACS baseline after an error.

A live full-crawl fallback may be offered as an explicit availability option,
but it must invalidate the AWACS baseline and emit a warning that
snapshot-consistent scanning was not used. It is not the default for backend =
awacs.

## Jujutsu design

### Configuration and persisted state

Add FsmonitorSettings::Awacs(AwacsConfig) next to Watchman, Test, and None.
Persist AWACS state in the small snapshot-baseline journal described in
[Snapshot-baseline working-copy state](incremental-working-copy-tree-state.md),
not in `tree_state` or a per-path SQLite table. The journal stores the
semantic tree IDs and labels, sparse patterns, interpretation-input
fingerprint, backend kind, opaque baseline identity/retention token, and
publication phase.

Reading an older tree state may import its semantic tree and old cursor
metadata, but migration starts as `NoBaseline` unless AWACS can verify and
durably promote the exact old snapshot. Changing fsmonitor.backend invalidates
an incompatible baseline. AWACS token bytes remain opaque to Jujutsu.

### Scan session abstraction

Introduce an internal abstraction created by the local working-copy snapshot
engine and owned until LockedLocalWorkingCopy::finish():

~~~
ScanSession {
  scan_root: PathBuf,
  matcher: Box<dyn Matcher>,
  candidate_baseline: Option<AwacsBaseline>,
  transition_id: Option<TransitionId>,
  completion: Option<AwacsSessionId>,
  warning: Option<SnapshotWarning>,
}
~~~

For none, scan_root is working_copy_path and the matcher is EverythingMatcher.
For Watchman, scan_root remains working_copy_path and the matcher is
EverythingMatcher because changed names alone cannot replace the removed
per-path cache on a mutable root. For AWACS, scan_root is the leased read-only
snapshot and the matcher comes from AWACS exact paths/prefixes or a full scan
for Full.

The scanner must not infer the read root from the live working-copy path.
DirectoryToVisit already carries a disk directory and should be seeded from
scan_root. visit_tracked_files() must resolve tracked paths against scan_root,
not working_copy_path. All nested directory recursion, .gitignore reads,
symlink reads, metadata reads, EOL conversion reads, and large-file checks must
follow that same root.

The live working_copy_path remains authoritative for:

* the small baseline journal;
* lock and finish operations;
* checkout and sparse-pattern materialization;
* reset/recover operations;
* colocated Git import/export;
* user-facing workspace root reporting.

Ignore and sparse inputs need an explicit boundary. Relative ignore files that
live under the worktree must be read from scan_root. Inputs outside the
worktree, including global excludes and colocated Git metadata such as
.git/info/exclude or sparse-checkout configuration, cannot be assumed to be in
the Btrfs snapshot. Jujutsu should compute a fingerprint of those inputs and
store it with the baseline journal; a changed or unreadable fingerprint forces
Full invalidation before the baseline can be reused. This prevents an excluded
metadata change from making newly visible files appear clean.

### Implementation map

The first implementation should be centered on these existing seams:

* lib/src/fsmonitor.rs owns FsmonitorSettings, config parsing, and the
  Watchman client. Add the AWACS settings and `btrfs-awacs` library boundary
  there, but do not make the Watchman module carry snapshot-only concepts.
* lib/src/local_working_copy.rs owns the baseline journal, working_copy_path,
  matcher construction, DirectoryToVisit, FileSnapshotter, and the
  semantic-tree-plus-delta fast path. This is where scan_root must be threaded
  through every read and per-path file-state dependencies removed.
* lib/src/protos/local_working_copy.proto owns the legacy tree-state and
  Watchman-clock formats. Replace them with the small versioned journal, or
  retire them after the migration window.
* lib/src/working_copy.rs defines LockedWorkingCopy::snapshot() and finish().
  The public contract should document the pending baseline/materialization
  phases and that a retained baseline may outlive finish().
* cli/src/cli_util.rs drives the normal snapshot-before-command flow. It
  should not need AWACS-specific branches beyond surfacing warnings.
* cli/src/config-schema.json and docs/config.md own the public backend values
  and documentation.
* lib/tests/test_local_working_copy.rs already covers changed-path matchers and
  .gitignore subtree rescans; extend it with a fake scan root and fake AWACS
  lease.
* cli/tests/test_working_copy.rs already has an environment-gated AWACS
  Watchman compatibility test; extend that harness for the direct backend.

### Snapshot transaction

The algorithm is:

1. Load `Clean(X, A, I)` from the baseline journal.
2. Ask the configured backend for a ScanSession.
3. Traverse only session.scan_root, using the complete delta to limit reads.
4. Build Y from semantic tree X plus B reads for changed paths/prefixes; do
   not build or persist file states.
5. If the scan found untracked paths that require a later rescan, publish
   `NoBaseline(Y)` so the next command requests a fresh full snapshot scan.
6. Otherwise retain the candidate baseline and session in
   LockedLocalWorkingCopy.
7. Let the command finish any repository operation that does not change the
   disk-derived tree.
8. In LockedLocalWorkingCopy::finish(), atomically write
   `PendingBaselineCommit`, idempotently promote B, then atomically publish
   `Clean(Y, B, I)`.
9. Only after clean publication succeeds, send FinishScan(Committed) to
   release A and clean up the session.
10. If any step before clean publication fails, if the command is discarded,
    or if a later working-copy mutation makes the tree no longer equal
    snapshot B, send FinishScan(Aborted) and do not publish B as clean.

Snapshotting does not itself publish the journal; the persistence boundary is
LockedLocalWorkingCopy::finish(). The lease therefore cannot be a local
temporary owned only by the snapshot method.

Release is cleanup, not the local commit. If release fails after clean
publication, B remains valid and A may be released by idempotent retry or
backend recovery; B must not expire merely because finish returned.

### Commands and working-copy mutations

The AWACS scan root is used only during LockedWorkingCopy::snapshot().
check_out(), set_sparse_patterns(), reset(), recover(), and finish() continue
to use the live root.

Commands that snapshot and then mutate initially operate on a point-in-time B
tree. That is consistent with Jujutsu's existing snapshot-before-command
model. A command must not publish baseline B after it writes live working-copy
files or changes the cached tree through check_out(), reset(), recover(), or
set_sparse_patterns(); it must abort the pending session and either write
`NoBaseline` or establish a new baseline after materialization.

Colocated Git requires special care. Any Git HEAD import, Git export failure,
or semantic-tree reset that changes Jujutsu's cached tree without proving disk
materialization must invalidate the AWACS baseline. This is the same class of
requirement that already applies to the Watchman clock, but AWACS makes the
invariant explicit.

## Implementation plan

1. Add the small baseline journal and pending-materialization state machine,
   with no behavior change for none or watchman beyond conservative full scans.
2. Refactor snapshotting so scan-root selection is separate from delta
   selection, and build Y from semantic tree X plus B reads rather than a
   persisted file-state vector.
3. Add tests proving a synthetic scan root is used for every read while the
   journal and lock remain under the live root.
4. Add the AWACS config variant and a fake in-process AWACS client that models
   complete deltas, Full fallback, retention, promotion, and release.
5. Add the crate-owned direct production client and
   begin/promote/finish baseline state machine.
6. Add Linux/Btrfs integration tests behind explicit AWACS test environment
   variables.
7. Remove tree-state/SQLite state only after legacy-protobuf migration,
   fail-closed legacy-SQLite handling, crash recovery, and full-scan-oracle
   coverage pass.
8. Add user documentation only after the protocol and fallback behavior are
   stable.

## Test plan

### Unit tests

* A separate scan_root contains different content than the live root;
  snapshotting records only snapshot content.
* .gitignore, ignored-directory recursion, symlink targets, executable-bit
  changes, EOL conversion, tracked deletion, and new-file size checks all read
  from scan_root.
* The baseline journal and lock files are written only under the live state
  path.
* Incremental changed paths narrow scanning inside the snapshot.
* Full invalidation scans all sparse paths from the snapshot.
* A clean AWACS baseline is published only after successful B promotion and
  journal save.
* Begin, scan, promotion, and journal-save failures abort the lease or recover
  to a full-scan requirement.
* Release failure after a successful save retains B and relies on idempotent
  retry for A cleanup.
* Backend changes, reset, recover, and colocated Git reset invalidate
  incompatible baselines.
* Legacy watchman_clock state migrates without changing Watchman behavior.

### Integration tests

Use a real Btrfs subvolume and the direct AWACS coordinator with a matching
broker, gated by explicit environment variables like the existing Watchman
compatibility test:

* tracked edits, new files, deletions, renames, hardlinks, .gitignore, sparse
  patterns, and ignored paths match a full-scan oracle;
* create/delete and modify/restore between B and C do not change results when
  Jujutsu scanned B;
* mutate the live root after BeginScan(B) but before traversal and prove the
  result still equals snapshot B;
* reclaim or expire the previous baseline and require a fresh snapshot scan;
* expire the active lease mid-scan and require an aborted session plus a
  fail-closed error, or an explicit live-fallback warning when that mode is
  enabled;
* crash the calling process between begin and finish;
* rename/replace/restore the live root and attach/detach a mount between cuts;
* colocated Git HEAD import/export and stale working-copy recovery invalidate
  the baseline when disk materialization is not proved.

### Performance tests

Measure unchanged status, one-file status, large-directory untracked scans,
fresh fallback, snapshot lease latency, and retained-snapshot growth. Require
the one-file path to avoid loading or sorting O(N) per-path state. Compare
with fsmonitor.backend = none and watchman.

## Alternatives considered

### Extend Watchman compatibility

Watchman can return changed names and a clock, but it has no standard snapshot
locator, lease, or completion acknowledgment. A side channel would be harder
to reason about than a direct backend and would still require Jujutsu-specific
code.

### Return coarse directory witnesses through Watchman

Git's fsmonitor hook has directory-prefix conventions, but Watchman has no
portable subtree-dirty marker for Jujutsu. Returning is_fresh_instance is safe
but loses the main benefit of an immutable snapshot scan.

### Bind-mount a snapshot over the live root

A launcher could run Jujutsu in a private mount namespace where the snapshot
appears at the normal path. That is difficult to make safe for commands which
also write the working copy, and it hides the read/write split from Jujutsu
instead of making it explicit.

### Keep scanning the live root

This preserves current behavior but cannot make a monitor clock and a later
live crawl describe one immutable filesystem state.

## Future possibilities

* Pass a directory fd and use openat() throughout the scanner instead of
  trusting a pathname.
* Add direct AWACS trigger support for background jj util snapshot.
* Generalize ScanSession for other immutable or virtual working-copy
  providers.
* Let a virtual working-copy backend provide content directly without a local
  read-only directory.
