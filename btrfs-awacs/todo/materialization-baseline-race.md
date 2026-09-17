# Concurrent edits lost from JJ state during materialization baseline handoff

Status: known correctness bug; fix deferred by request on 2026-09-01.

## Problem

In a snapshot-backed JJ working copy, restore/checkout can accept an AWACS
filesystem baseline that contains concurrent editor changes without recording
those changes in JJ's semantic tree. The bytes can remain correct on disk while
`jj status` and `jj diff` omit the edit. A later snapshot need not repair the
mismatch, because the edit is already part of the accepted filesystem baseline.

The observed failure is in JJ's materialization baseline handoff. AWACS can
report the changed path correctly; JJ skips interpreting that change when it
accepts the post-materialization snapshot. A read-only Btrfs snapshot provides
stable bytes to read, but does not prove that they equal the intended checkout
tree.

## Race sequence

Let A be the committed filesystem snapshot paired with JJ's recorded tree, and
let T be the tree JJ intends to materialize.

1. JJ preserves A as the pending baseline and starts restore/checkout.
2. JJ writes the requested files and updates its in-memory semantic tree to T.
3. An editor writes a tracked file before the post-materialization snapshot B
   is captured. The edit can affect a different file from the restored paths,
   or a restored file after JJ has finished writing it.
4. AWACS captures B, which includes both the materialization and the editor
   write, and supplies the changes from A to B.
5. JJ accepts B without traversing those changed paths, pairing B with T even
   though interpreting B would produce a different tree.
6. A later ordinary snapshot compares B with a newer snapshot C. If that file
   has not changed again and the invalidation scope does not otherwise revisit
   it or a covering prefix, the recorded tree remains stale. A later broader
   invalidation can discover the edit; the mismatch is not necessarily permanent.

This violates the required relationship between an accepted filesystem
baseline and the semantic tree interpreted from it. The working-copy lock
serializes cooperating JJ operations; it does not exclude editor writes.

A Btrfs transaction ID cannot establish ownership of the writes. Transactions
batch changes across the filesystem, so JJ and an editor can contribute to the
same `transid`. It is not a per-write sequence number or proof that the live
working tree remained unchanged. See the [Btrfs transaction definitions](https://btrfs.readthedocs.io/en/latest/btrfs-man5.html)
and [kernel transaction lifecycle](https://raw.githubusercontent.com/torvalds/linux/v6.16/fs/btrfs/transaction.c).

## Code to revisit

The materialization and normal snapshot paths are in
[`lib/src/local_working_copy.rs`](../../lib/src/local_working_copy.rs):

- `TreeState::begin_materialization()` preserves A and currently assumes that
  the controlled mutation accounts for every A-to-B change.
- `LockedLocalWorkingCopy::check_out()` writes the target tree and calls
  `finish_materialization()`.
- `TreeState::finish_snapshot_materialization()` obtains B through
  `make_snapshot_scan()`, deliberately omits traversal, and calls
  `publish_scan_baseline()`. The scan completion is retained for finalization.
- AWACS still supplies an immutable cut, validates its identity, and binds the
  invalidation to that cut. The skipped step is JJ's interpretation of the
  returned paths, not AWACS snapshot or delta validation.
- Ordinary snapshot traversal must interpret the reported paths before its
  resulting tree and baseline can be accepted together.

Checkout finalization is in `WorkspaceCommandHelper::finish_transaction()` in
[`cli/src/cli_util.rs`](../../cli/src/cli_util.rs), which publishes the intended
repository operation before physical checkout. Reset, recovery, and sparse
materialization also call the shared finish path and need a separate scope
audit when this is addressed; their behavior was not validated by this
experiment.

## Reproduction and correctness evidence

A native Btrfs experiment used two tracked 512-byte files in a snapshot-backed,
colocated JJ/Git repository. A deterministic hook paused restore after all
checkout writes and before either baseline acceptance or re-verification.
The test wrote and fsynced the concurrent edit, then released the hook.

Two scenarios were tested with the fast path and with experimental
re-verification:

- Restore one file while editing the other tracked file.
- Edit the restored file after JJ has finished writing it and before B.

In both control cases, disk bytes contained the editor change but the recorded
JJ tree was stale immediately afterward and remained stale after ordinary
status. In both treatment cases, the recorded tree contained the edit before
any later status command and remained correct afterward. Verification read
the recorded commit without triggering another snapshot, so a recovery scan
could not hide the failure.

This is distinct from an editor write that JJ subsequently overwrites before
B is captured. Re-reading B cannot recover bytes already overwritten; that
conflict needs its own policy and validation.

## Re-verification experiment and cost

The isolated experiment retained A through materialization and called the
ordinary snapshot pipeline to obtain and interpret B, instead of accepting B
first. If the observed tree differed from T, CLI finalization recorded the
concurrent changes in the working-copy commit before finishing the baseline
journal. Calling ordinary status only after blindly accepting B would not
provide the same protection.

Both variants used the same optimized `dev-opt` binary, with
`JJ_BENCH_REVERIFY_MATERIALIZATION=1` selecting the experimental treatment.
The pause hook was disabled during timing. The experiment was based on JJ
revision `b04fc3f777b5cd5e7fca5367a0e201c7d05fa4a7`; it was not installed as a
production fix.

The timing fixtures used native Btrfs, loose Git objects, 4 KiB regular files,
and 100 files per directory. Every tracked file changed, and the total tracked
file count equaled the changed-file count. Each operation/size had one warmup
pair and five measured pairs. Both variants used the same direction within a
pair; A-to-B/B-to-A direction and variant order alternated between pairs.
Setup, source-file reading, and verification were outside the timed command.
Every measured result checked the disk contents, recorded tree, and retained
fixture revisions.

| Changed files | Restore: median paired added time | Checkout: median paired added time |
|---:|---:|---:|
| 1,000 measured | 0.595 s | 0.865 s |
| 10,000 measured | 8.375 s | 9.395 s |
| 100,000 estimated | approximately 85 s | approximately 95 s |

Added time is the median of treatment minus control within each matched pair,
not the difference between the two overall medians. At 10k, overall command
medians were 14.614 s versus 22.859 s for restore, and 19.102 s versus 24.361 s
for checkout. Linear extrapolation of the paired added times gives the rounded
100k estimates above. The 100k run was canceled during setup; no 100k timing
sample completed.

These are rough workload-specific figures. No caches were dropped, but source
reading did not guarantee immutable snapshot or Git object cache residency.
Major page faults and shared-host I/O variation occurred; all valid samples
were retained, including a 32.555 s control outlier at 1k restore. Direction
and run order were coupled. The experiment does not estimate performance for
a much larger mostly unchanged monorepo. Original temporary raw timing logs
and harness files were lost during a host restart; these numbers are recovered
previously computed summaries, not rerun measurements or confidence bounds.

## Deferred work and acceptance criteria

Defer the implementation for now. The experiment demonstrates the failure and
the cost of one corrective approach; it is not a selected production design.
Until addressed, avoid concurrent edits during restore/checkout, and do not
treat a clean status after known overlap as proof that all disk edits have
been recorded.

When revisiting the fix:

- Preserve the baseline/tree relationship by interpreting B before acceptance,
  or provide an equally strong proof that every skipped path still matches T.
  A path being in the materializer's write set is insufficient for a same-file
  edit after JJ's write.
- Retain normal snapshot interpretation inputs, including ignores,
  auto-tracking, sparse patterns, file-size limits, and their fingerprints.
- Record any observed concurrent changes before binding B to the final JJ
  operation and corresponding Git state.
- Preserve pending-baseline recovery and lease handling across errors,
  cancellation, and crashes; never publish a clean mismatched baseline/tree.
- Reproduce both races deterministically and inspect the recorded tree before
  any recovery snapshot. Also cover edits after B, intended paths skipped by
  checkout, and the reset/recovery/sparse callers of the shared finish path.
  The experiment rejected skipped checkout files and did not validate those
  other operations or conflict, symlink, submodule, and untrack behavior.
- Benchmark representative repository shapes as well as changed-file counts,
  and retain raw results and harnesses in durable storage. Capture only
  allowlisted trace measurements, without inherited environment fields.
