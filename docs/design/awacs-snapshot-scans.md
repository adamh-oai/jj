# Direct AWACS snapshot scans

Status: draft

## Summary

This document proposes direct integration between Jujutsu and the embedded
AWACS library. AWACS coordinates a read-only Btrfs snapshot. It records a
logical cut and the paths that changed since the previous baseline. It then
returns the complete delta between the two states. Jujutsu reads this immutable
snapshot when it scans the working copy.

Watchman returns a clock and names that may have changed. Its protocol does
not provide a snapshot lease, a read root, or an acknowledgment that the scan
has finished. Jujutsu needs all three to build its tree from one immutable
filesystem state. AWACS should therefore have a separate backend.

## State of the feature

`fsmonitor.backend` accepts `none` and `watchman`. Watchman returns a clock and
a list of changed names. Once the durable per-path cache is removed, that list
cannot prove which paths in the live working copy are unchanged. The compact
journal implementation therefore scans the full working copy with Watchman.
It does not save the Watchman clock as a reusable physical baseline.

Subvolume mode uses AWACS internally. AWACS supplies an immutable scan root
and can prove that a delta is complete. Ordinary commands fail if they would
need a full scan. Initialization or an explicit baseline rebuild may perform
one.

Querying a monitor before scanning a live directory has this race:

1. Jujutsu queries the monitor and receives clock B.
2. Jujutsu scans the live working copy.
3. A path appears, the scan reads it, and it disappears before the next
   monitor boundary C.
4. Comparing only the states at B and C can show no change, although Jujutsu
   saved the intermediate state.

Reading an immutable snapshot B prevents this race. Writes after B cannot
change what the scan reads. If a path changes between B and C and returns to
its state at B, Jujutsu never sees the intermediate state.

Watchman remains useful on non-Btrfs filesystems and where an ordinary event
monitor is needed. It cannot supply the retained immutable baseline required
for incremental scans without a per-path cache.

## Goals and non-goals

### Goals

* Build each successful AWACS-backed Jujutsu snapshot from one read-only
  Btrfs snapshot. Save a durable record that connects the Jujutsu tree to
  that snapshot.
* Scan incrementally from a retained baseline. Given exact snapshot A, AWACS
  must return a complete A→B delta or report that a full scan is required.
  When the delta is complete, Jujutsu reads only affected paths from B.
* Keep Jujutsu state, locks, checkout writes, and colocated Git metadata on
  the live working-copy root.
* Reject an unavailable, expired, pruned, ambiguous, or incompatible AWACS
  session. Such a session must not save a clean baseline or report that the
  tree represents a valid snapshot.
* Keep AWACS optional and Linux-only. Jujutsu must not require Btrfs or a
  custom kernel.
* Keep the Watchman backend correct through conservative full scans.

### Non-goals

* Making AWACS a general Watchman replacement.
* Sending snapshot paths or lease identifiers through the Watchman protocol.
* Using a read-only snapshot for checkout, sparse-pattern updates,
  `jj file untrack`, or other working-copy writes.
* Making reads consistent with concurrent writes in the `none` and
  `watchman` backends.
* Requiring AWACS for ordinary Jujutsu installations.
* Supporting nested subvolumes, fscrypt, cross-filesystem working copies,
  or other layouts that AWACS rejects.

## Correctness model

After a semantic snapshot, the AWACS baseline names the exact immutable
snapshot that Jujutsu read to build its current tree and sparse view.
Materializing files in the working copy can retain a newer physical baseline.
The journal must also save any changed paths that Jujutsu has not yet scanned.
The next scan must process those paths even if AWACS reports no new changes.
For each baseline, the backend must prove that the named snapshot is still
available or report that a full scan is required.

For one successful snapshot:

~~~
live root:       W
baseline:        tree X ↔ retained snapshot A
AWACS cut:       B
delta:           complete A→B delta
scan root:       read-only snapshot B
persisted state: tree Y ↔ retained snapshot B
~~~

The following invariants are required:

1. Jujutsu saves B as a clean baseline only if every filesystem read used to
   build tree Y came from B. If materialization advances the physical
   baseline, Jujutsu must keep every intervening changed path as pending scan
   work. It must not claim that the tree already describes B.
2. After changing the working-copy tree, Jujutsu must do one of three things:
   materialize matching files, keep the work needed to reconcile the tree,
   or invalidate the baseline. It must do this before using the baseline again.
3. AWACS retains the logical A→B event range while it derives B. It returns
   an open descriptor for B before Jujutsu saves the clean B record. AWACS
   must prove A for each delta or report that a full scan is required.
4. A failed or abandoned session does not advance the saved baseline.
5. During a scan session, Jujutsu uses the live root only to manage state or
   change the working copy.
6. Before creating B, AWACS must check the live root's path binding, mount
   topology, filesystem UUID, and subvolume UUID. If it cannot prove
   continuity, Jujutsu must fail and require an explicit baseline rebuild.
7. The scan root must stay private and immutable throughout the lease. A
   caller-controlled path that can be renamed or mounted over is invalid.
8. Exact-path and prefix deltas must describe all changes that Jujutsu can
   observe. Uncertainty requires a full scan or an error.

`jj status` then reports the state at snapshot B. Writes after B appear in
its next command, rather than changing the result of the current scan.

## Proposed user interface

Subvolume mode selects AWACS internally. Jujutsu embeds the AWACS library
and directly opens the initialized state for each root. There is no daemon
socket to find or configure. Unsupported platforms and uninitialized roots
produce errors that explain what the user must do.

The first version should not expose trigger configuration. Background
snapshot triggers can be designed later for the direct protocol. They should
not copy `fsmonitor.watchman.register-snapshot-trigger`.

## btrfs-awacs library API

Jujutsu should use the versioned `btrfs-awacs` Rust API. The library manages
coordination for each root, cursors, and the lifetime of snapshot file
descriptors. `jj-lib` must not implement these details. Only the Btrfs broker
requires privileges and setup specific to the deployment. The library keeps
that broker's allowed operations restricted.

AWACS needs a scan lease and a way to retain the next committed baseline.
These operations should require only ordinary read/cut permission. They
should retain only baseline A and candidate B, which `BeginScan` selects.
Promoting B must be safe to retry. Jujutsu must not receive permission to
retain or select arbitrary snapshots.

The existing Watchman response fence protects the response only until it is
written. Jujutsu needs a longer lifetime: it must hold B while it saves the
local journal and keep B for the next command.

### Begin scan

~~~
BeginScan {
  live_root,
  baseline_owner_id,
  previous_baseline?,
  allow_full_invalidation,
}

BeginScanResult {
  candidate_baseline,
  snapshot_fd,
  delta = Full | ExactPaths([...]) | Prefixes([...]),
  expires_boottime_ns,
}
~~~

`snapshot_fd` contains an open, read-only directory descriptor and its
identity metadata:

~~~
SnapshotLocator {
  fd,
  filesystem_uuid,
  subvolume_uuid,
  read_only = true,
}
~~~

`TreeState::snapshot()` currently uses paths. Jujutsu may therefore use
`/proc/self/fd/N` as a temporary path, provided it holds the descriptor for
the full lease. It must not canonicalize that path. A later scanner should
use `openat()` directly. A filesystem path alone is insufficient: it can be
renamed, mounted over, or become inaccessible during the scan.

`ExactPaths` and `Prefixes` contain repository-relative byte strings. Each
must describe a complete delta. If AWACS cannot prove that the incremental
delta is complete, it reports that a full scan is required. A caller that
explicitly allows full invalidation may receive `Full`. An ordinary command
in subvolume mode instead fails and tells the user to rebuild the baseline.
`Prefixes` lets the direct API invalidate a whole directory. It does not
extend Watchman.

### Renew scan

~~~
SnapshotLease::renew()
~~~

Jujutsu renews a lease only while it scans or waits to save the resulting
baseline journal. If renewal fails, it aborts the session before saving a
new clean baseline.

### Promote baseline

~~~
SnapshotLease::promote()
~~~

Promotion marks B as pending and keeps A committed. If the process crashes
before it saves the local journal, the pending handoff can remain. The next
Begin call reconciles that handoff with the baseline named in the journal.

### Finish scan

~~~
SnapshotLease::finish(outcome = Committed | Aborted)
~~~

`Committed` means Jujutsu durably saved its record for B. That record includes
any paths still awaiting a scan after materialization. When no paths remain
pending, the record is the clean tree-Y↔B pair. Finishing releases the prepared
scan session.

`Aborted` means Jujutsu did not save a record for B. AWACS may reclaim physical
paths that are no longer needed during cleanup, subject to its cleanup limits.
Finish is idempotent: retrying an interrupted call is safe.

The compact journal saves a stable, opaque owner ID beside its typed baseline.
AWACS keeps the logical event history needed to replay retained cursors.
Consumers must not use physical snapshot pathnames as durable references.

### AWACS index and replay storage

AWACS has one authoritative path index per watch. For each cut, it uses that
index to find the changed-inode subset, within the cut's limits. It checks the
target and updates only the affected object, reference, and owner rows. Those
updates and the indexed head change use one SQLite transaction. Adjacent cuts
do not create immutable path-map overlays, so there is no overlay chain to
compact periodically.

A private spool file stores the replay data for each retained cut. It first
contains the changed-object stream verified by the broker. That stream is
followed by the conservative path events already derived from it. The encoded
events are hashed and fsynced. SQLite stores only the cut sequence, endpoint
snapshot IDs, spool pathname, payload hash, and event count.

To replay from a stale cursor at B to current cut D, AWACS reads and unions
the retained spool payloads for C and D. A missing file, hash failure, sequence
gap, or cursor older than the replay floor prevents incremental replay.
AWACS then requires full invalidation or returns an error, as the caller's
policy specifies.

When retention removes old logical cuts, it removes their catalog rows in
the same transaction. After commit, it unlinks the spool files that have no
references. A crash before the catalog commit may leave an unreferenced
spool file. It cannot leave a committed replay row whose payload was not
fsynced first.

### Errors

The library API must distinguish:

* unsupported filesystem or layout;
* unavailable or uninitialized AWACS root;
* invalid, expired, or pruned previous baseline;
* lost continuity that requires a fresh full scan;
* expired snapshot lease;
* authentication or authorization failure;
* incompatible library or state version;
* malformed response.

The embedded coordinator must check the process root and mount namespace.
It must not trust filesystem identity supplied by Jujutsu without verification.

An invalid, expired, or pruned previous baseline can produce
`InvalidPreviousBaseline` or `FullScanRequired`. A caller that explicitly
allows full invalidation may receive a target snapshot with `delta = Full`
to establish a new baseline. Ordinary commands in subvolume mode fail and
suggest `jj util subvolume enable --rebuild-baseline`.

Other errors before Jujutsu accepts a scan session fail the operation without
saving a new baseline. Errors after acceptance abort the session and fail the
AWACS-backed snapshot operation. Jujutsu must never continue using an older
AWACS baseline after an error.

An explicit option may allow a full scan of the live working copy after an
AWACS error. It must invalidate the AWACS baseline and warn that the scan
did not read one consistent snapshot. This is not the default for
`backend = awacs`.

## Jujutsu design

### Configuration and persisted state

Add `FsmonitorSettings::Awacs(AwacsConfig)` beside Watchman, Test, and None.
Save AWACS state in the small journal described in
[Snapshot-baseline working-copy state](incremental-working-copy-tree-state.md).
Do not put it in `tree_state` or a per-path SQLite table. The journal stores:

* semantic tree IDs and labels;
* sparse patterns;
* the interpretation-input fingerprint;
* backend kind;
* opaque baseline identity and retention token;
* the phase of the baseline update;
* pending exact paths and directory prefixes from materializations that
  have not yet been scanned.

Journal version 3 adds the required scan work. Readers accept version 2 with
no pending paths. Older binaries reject version 3, so they cannot silently
drop that work.

Migration may import an older tree state's semantic tree and cursor metadata.
It starts as `NoBaseline` unless AWACS can verify and durably promote the exact
old snapshot. Changing `fsmonitor.backend` invalidates an incompatible
baseline. Jujutsu must treat AWACS token bytes as opaque.

### Scan session abstraction

The local working-copy snapshot engine creates an internal scan session.
It keeps the session until `LockedLocalWorkingCopy::finish()`:

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

For `none`, `scan_root` is `working_copy_path` and the matcher is
`EverythingMatcher`. Watchman uses the same root and matcher. Changed names
alone cannot replace the removed per-path cache on a mutable root. For AWACS,
`scan_root` is the leased read-only snapshot. Its matcher selects the exact
paths or prefixes in the AWACS delta. Explicit initialization or rebuilding
may instead scan the full snapshot.

The scanner must receive its read root explicitly. `DirectoryToVisit` already
carries a disk directory; initialize it from `scan_root`.
`visit_tracked_files()` must resolve tracked paths against `scan_root`.
Directory recursion, `.gitignore` reads, symlink reads, metadata reads,
EOL conversion reads, and large-file checks must all use that root.

Use the live `working_copy_path` for:

* the small baseline journal;
* lock and finish operations;
* checkout and sparse-pattern materialization;
* reset and recover operations;
* colocated Git import and export;
* the workspace root shown to the user.

Read relative ignore files inside the worktree from `scan_root`. Inputs outside
the worktree may be absent from the Btrfs snapshot. These include global
excludes and colocated Git metadata, such as `.git/info/exclude` or
sparse-checkout configuration. Jujutsu should compute a fingerprint of those
inputs and save it in the baseline journal.

A changed fingerprint makes the baseline unusable. In subvolume mode, an
ordinary snapshot then fails and requires an explicit rebuild. Failure to read
an input also produces an error. These checks prevent a change in excluded
metadata from making newly visible files appear clean.

### Implementation map

Use these existing components for the first implementation:

* `lib/src/fsmonitor.rs` owns `FsmonitorSettings`, configuration parsing, and
  the Watchman client. Add AWACS settings and the `btrfs-awacs` library calls
  there. Keep snapshot-specific concepts out of the Watchman module.
* `lib/src/local_working_copy.rs` owns the baseline journal,
  `working_copy_path`, matcher construction, `DirectoryToVisit`,
  `FileSnapshotter`, and the incremental tree update. Pass `scan_root` through
  every read and remove dependencies on per-path file state.
* `lib/src/protos/local_working_copy.proto` owns the legacy tree-state and
  Watchman-clock formats. Replace them with the small versioned journal,
  or retire them after the migration window.
* `lib/src/working_copy.rs` defines `LockedWorkingCopy::snapshot()` and
  `finish()`. Document the pending baseline and materialization phases.
  Explain that a retained baseline can outlive `finish()`.
* `cli/src/cli_util.rs` runs the normal snapshot before a command. It should
  need no AWACS-specific branches beyond showing warnings.
* `cli/src/config-schema.json` and `docs/config.md` define and document the
  public backend values.
* `lib/tests/test_local_working_copy.rs` tests changed-path matchers and
  `.gitignore` subtree rescans. Extend it with a fake scan root and AWACS lease.
* `cli/tests/test_working_copy.rs` has an AWACS Watchman compatibility test
  enabled by environment variables. Extend that harness for the direct backend.

### Snapshot transaction

The algorithm is:

1. Load `Clean(X, A, I)` and pending scan paths from the baseline journal.
2. Ask the configured backend for a `ScanSession`.
3. Union the session delta with the pending exact paths and prefixes.
   Apply ignore and sparse rules to that combined scope. Read only
   `session.scan_root`, and limit reads to the selected paths.
4. Build Y by applying reads from changed B paths or prefixes to tree X.
   Do not build or save per-path file states.
5. Keep the candidate baseline and session in `LockedLocalWorkingCopy`.
6. Let the command finish any repository operation that leaves the tree
   derived from disk unchanged.
7. In `LockedLocalWorkingCopy::finish()`, atomically write
   `PendingBaselineCommit`. Promote B with an operation that is safe to retry.
   Then atomically save `Clean(Y, B, I)` and clear the pending paths that
   the scan successfully reconciled.
8. Only after that clean record is saved, send `FinishScan(Committed)` to
   release A and clean up the session.
9. Send `FinishScan(Aborted)` if any step before the clean save fails, the
   command is discarded, or a later working-copy mutation makes the tree
   differ from B. Do not save B as clean in those cases.

`LockedLocalWorkingCopy::finish()` saves the journal. The snapshot method
itself does not save it, so the lease must outlive that method.

Releasing A cleans up the old state. If release fails after the clean record
is saved, B remains valid. A can be released by an idempotent retry or backend
recovery. B must not expire merely because `finish()` returned.

### Commands and working-copy mutations

Only `LockedWorkingCopy::snapshot()` reads the AWACS scan root.
`check_out()`, `set_sparse_patterns()`, `reset()`, `recover()`, and `finish()`
continue to use the live root.

A command that snapshots and then changes files starts from tree A at one
point in time. This follows Jujutsu's existing snapshot-before-command model.
After materialization, strict subvolume mode takes a new immutable snapshot B.
It saves the complete A→B exact paths or prefixes in the journal. Another
writer may have changed an unrelated file during checkout. Jujutsu therefore
cannot assume that its materializer produced every A→B change. It commits
baseline B together with the intended tree and the pending scan work. Checkout
does not need to traverse the filesystem for this step.

The next semantic snapshot takes C. It scans both the saved A→B scope and
AWACS's B→C delta. It must scan the pending paths from C even if B→C is empty.
Repeated materializations accumulate pending paths. Only a successful semantic
snapshot clears them. Reloading the journal or recovering an interrupted
materialization must preserve those paths. In strict subvolume mode, full or
root-wide invalidation must fail before a replacement baseline is saved.

A baseline with pending scan work supports further incremental scans. Its
semantic tree cannot yet seed another workspace. Snapshot workspace creation
rejects that source until `jj status` reconciles it. Jujutsu also withholds the
Git-mediated adoption seed while work remains. Adoption rejects a copied
journal that contains pending paths.

Git HEAD import, Git export failure, and a semantic-tree reset can change
Jujutsu's cached tree. If such a change does not prove that files on disk
match the tree, it must invalidate the AWACS baseline. The Watchman clock has
the same requirement. The AWACS contract makes it explicit.

## Implementation plan

1. Add the small baseline journal and the pending-materialization state
   machine. Keep `none` and `watchman` behavior except for conservative
   full scans.
2. Separate scan-root selection from delta selection. Build Y from tree X
   and reads from B, without using the persisted file-state vector.
3. Test that every read uses a synthetic scan root. Check that the journal
   and lock stay under the live root.
4. Add the AWACS configuration variant and a fake in-process AWACS client.
   Model complete deltas, `Full` fallback, retention, promotion, and release.
5. Add the crate's direct production client and the begin/promote/finish
   state machine for baseline updates.
6. Add Linux/Btrfs integration tests enabled by explicit AWACS test
   environment variables.
7. Remove tree-state and SQLite state only after the required tests pass.
   Cover legacy-protobuf migration, rejection of legacy SQLite state,
   crash recovery, and comparison against full scans.
8. Add user documentation after the protocol and fallback behavior are stable.

## Test plan

### Unit tests

* Put different content in `scan_root` and the live root. Check that the
  snapshot records only the content in `scan_root`.
* Check that `.gitignore`, ignored-directory recursion, symlink targets,
  executable-bit changes, EOL conversion, tracked deletion, and new-file
  size checks all read from `scan_root`.
* Write baseline journals and lock files only under the live state path.
* Use incremental changed paths to restrict scanning inside the snapshot.
* Change an unrelated tracked file between the initial snapshot and checkout
  completion. Capture that change after reload, even if the next AWACS delta
  is empty.
* Accumulate pending scan paths across materializations. Preserve them during
  interrupted materialization recovery. Clear them after a successful
  semantic scan.
* Reject a source with pending scan work when seeding a snapshot workspace
  or adoption.
* Reject full invalidation during ordinary scans in subvolume mode. Allow
  explicit initialization and rebuilding to scan all sparse paths in the
  snapshot.
* Save a clean AWACS baseline only after successful B promotion and a
  successful journal save.
* Inject failures in Begin, scanning, promotion, and journal saving. Abort
  the lease or recover to a state that requires an explicit baseline rebuild.
* Fail release after a successful save. Retain B and retry cleanup of A
  idempotently.
* Invalidate incompatible baselines on backend changes, reset, recover,
  and colocated Git reset.
* Migrate legacy `watchman_clock` state without changing Watchman behavior.

### Integration tests

Use a real Btrfs subvolume, the direct AWACS coordinator, and a matching broker.
Enable tests through explicit environment variables, as the existing Watchman
compatibility test does:

* Compare tracked edits, new files, deletions, renames, hardlinks, `.gitignore`,
  sparse patterns, and ignored paths against a full scan.
* Create and delete a file, or modify and restore it, between B and C.
  Check that these changes do not affect results when Jujutsu scanned B.
* Change the live root after `BeginScan(B)` and before traversal. Check that
  the result still matches B.
* Reclaim or expire the previous baseline. Require an error and an explicit
  baseline rebuild.
* Expire the active lease during scanning. Require an aborted session and
  an error that stops the operation. If explicit live fallback is enabled,
  require its warning instead.
* Crash the calling process between begin and finish.
* Rename, replace, and restore the live root. Attach and detach a mount
  between cuts.
* Check that colocated Git HEAD import/export and stale working-copy recovery
  invalidate the baseline when matching files on disk have not been proved.

### Performance tests

Measure unchanged status, one-file status, untracked scans of large directories,
explicit baseline rebuilds, snapshot lease latency, and growth in retained
snapshots. Require one-file status to avoid loading or sorting O(N) per-path
state. Compare with `fsmonitor.backend = none` and `watchman`.

## Alternatives considered

### Extend Watchman compatibility

Watchman returns changed names and a clock. It has no standard snapshot
locator, lease, or completion acknowledgment. A side channel would make the
protocol harder to follow and still require Jujutsu-specific code.

### Return coarse directory witnesses through Watchman

Git's fsmonitor hook has directory-prefix conventions. Watchman has no
portable marker that tells Jujutsu an entire subtree is dirty. Returning
`is_fresh_instance` is safe, but loses the main benefit of scanning an
immutable snapshot.

### Bind-mount a snapshot over the live root

A launcher could use a private mount namespace to put the snapshot at the
working copy's normal path. Commands that also write the working copy would
be difficult to make safe. The launcher would also hide the distinction
between the read and write roots from Jujutsu.

### Keep scanning the live root

This preserves current behavior. It cannot make a monitor clock and a later
live scan describe one immutable filesystem state.

## Future possibilities

* Pass a directory descriptor and use `openat()` throughout the scanner.
* Add direct AWACS trigger support for background `jj util snapshot`.
* Extend `ScanSession` to other immutable or virtual working-copy providers.
* Let a virtual working-copy backend provide content directly, without a
  local read-only directory.
