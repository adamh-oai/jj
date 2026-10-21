# Snapshot-baseline working-copy state

Author: [Adam Hupp](mailto:adamh@openai.com)

**Summary:** Replace the protobuf `tree_state` and intermediate SQLite
`file_states` store with a small baseline journal that can recover from a
crash. The journal connects retained immutable snapshot A to the last saved
Jujutsu working-copy tree X. It also records the sparse and ignore inputs
used to interpret that snapshot.

The next status obtains snapshot B and a complete A→B delta. It reads only
changed paths in B, applies their values to X, and saves the new
`(tree Y, snapshot B)` pair. It saves no per-path rows for mtime, size, type,
or conflict markers. An ordinary command that uses snapshots fails if the
baseline is missing or invalid, the delta is incomplete, or interpretation
inputs change. It asks the user to rebuild the baseline explicitly.
Initialization and rebuilding may perform a full scan.

## Objective

`tree_state` records the physical working copy. The commit stores semantic
content and tree IDs. The old `tree_state` file stores scan metadata:
per-path mtime, size, type, sparse patterns, and the filesystem-monitor cursor.
This lets a later status avoid hashing every file.

That cache is useful on ordinary mutable filesystems. A backend that retains
an authoritative immutable snapshot already provides the physical state.
If snapshot A matches Jujutsu tree X, paths unchanged in A→B keep their values
from X. Jujutsu needs to read only the paths in the complete delta from B.

The SQLite prototype removes O(N) writes, but each command still loads,
decodes, reconstructs, and sorts every file-state row. A measured incremental
status for one changed file takes about 9.6 seconds on a 1.15-million-row
working copy. SQLite is an intermediate implementation. It does not achieve
the target startup cost.

The desired steady-state costs are:

| Snapshot shape | Target work |
| --- | --- |
| No change A→B | Update the small journal; load no per-path state |
| k changed paths | Complete A→B delta plus O(k) reads/hashes from B |
| Changed directory/prefix | Enumerate only the affected prefix in B and the matching subtree in X |
| Missing or pruned A, incomplete delta, changed interpretation inputs | Fail ordinary commands that use snapshots; require an explicit baseline rebuild |
| Checkout, reset, recover, or sparse materialization | Establish a new physical baseline after successful materialization, or invalidate the old baseline |
| No authoritative retained-snapshot backend | Conservative full scan |

## Current state and why it is insufficient

The legacy `TreeState` protobuf saves:

- current tree IDs and conflict labels;
- each tracked path's mtime, size, file type, and optional materialized
  conflict-marker length;
- sparse patterns;
- a filesystem-monitor cursor tagged with its backend.

The SQLite implementation puts the same state in one metadata row and one
row per tracked path. Saving a cursor or a small change writes only the
changed state. Loading still reconstructs the entire in-memory `FileStatesMap`
before the scanner can use an incremental matcher. Every new command thus
keeps the O(N) cost of reading, decoding, allocating, checking, and sorting
that state.

Retained immutable snapshots make those per-path rows unnecessary:

- Tree X supplies semantic values for paths unchanged since A.
- Snapshot A records the physical state of the last accepted working copy.
- Snapshot B and the complete A→B delta identify paths whose physical state
  may need to be interpreted again.

This design removes the durable per-path cache.

## Scope and assumptions

The O(delta) path requires an authoritative backend that can:

1. retain the baseline snapshot named in the durable journal;
2. prove that new snapshot B descends from that exact A;
3. return every Jujutsu-observable change in A→B, or report that a full scan
   is required;
4. keep B available until Jujutsu durably saves it as the next baseline;
5. recover safely after crashes between retaining B and saving the local
   journal, including when recovery is retried.

AWACS is the first backend with this contract. Ordinary `none` and Watchman
backends do not retain an immutable baseline. After the per-path table is
removed, they perform conservative full scans. A future backend may narrow
a scan only if it provides an equivalent authoritative contract.

All backends stop saving per-path state. Only backends with the required
snapshot contract can scan incrementally.

## Goals and non-goals

### Goals

- Remove the stored table of per-path mtime, size, type, and conflict-marker
  state.
- Replace the legacy protobuf `tree_state` and SQLite `file_states` database
  with one small, versioned baseline journal.
- Connect a semantic Jujutsu tree to an exact immutable physical snapshot
  in one durable record.
- Make steady-state status reads and writes proportional to the complete
  filesystem delta, rather than the number of tracked paths.
- Recover safely from crashes while retaining snapshots and saving the journal.
- Preserve the working-copy lock and single-writer model.
- Stop safely when retention is missing, lineage is broken, or deltas are
  incomplete. Do the same for changed sparse or ignore inputs, corruption,
  and unsupported downgrades.
- Explicitly update or invalidate physical baselines during checkout, reset,
  recover, sparse changes, and colocated Git transitions.

### Non-goals

- Providing incremental status for `none` or ordinary Watchman without an
  authoritative retained immutable baseline.
- Saving file contents, per-path stat rows, an untracked-path catalog, or
  a second semantic tree in the journal.
- Making live working-copy writes and a filesystem snapshot one transaction.
- Replacing the working-copy lock with backend or database locking.
- Treating snapshot UUID equality as semantic content equality.
- Silently using a best-effort or incomplete change list for an incremental
  baseline transition.

## Correctness model

The durable clean state is:

~~~text
Clean {
  semantic_tree: X,
  baseline_snapshot: A,
  interpretation_inputs: I,
}
~~~

X contains the exact merged tree IDs and conflict labels that Jujutsu produced
by reading A. A contains the retained immutable physical snapshot, backend
identity, and lineage token needed to prove the next transition. I contains
the sparse view and a fingerprint of external inputs that affect how Jujutsu
interprets paths.

The normal transition is:

~~~text
load Clean(X, A, I)
validate A is retained and I is unchanged
BeginScan(A) -> leased B + CompleteDelta(A→B) | full-scan-required
if CompleteDelta:
  apply only B reads for changed paths/prefixes to X
else:
  fail and request an explicit baseline rebuild
produce semantic tree Y
durably retain/promote B
atomically publish Clean(Y, B, I)
release A
~~~

The following invariants are required:

1. A clean journal entry names an immutable snapshot and records enough
   backend state to reopen it or prove its identity. If the backend cannot
   do this, it must require a full scan. Ordinary commands that use snapshots
   then fail. A backend that hard-pins snapshots also retains A until the
   next clean record is durable.
2. Jujutsu produces the journal's semantic tree only from the named snapshot
   and recorded interpretation inputs.
3. Jujutsu uses a delta only when the backend proves complete continuity
   from the exact retained A to B.
4. The delta covers every Jujutsu-observable difference in A→B. Otherwise,
   the backend requires a full scan.
5. Jujutsu saves B as clean only after every read used to produce Y came
   from B and the backend durably retains B.
6. Jujutsu releases A only after the clean B record is durable.
7. A command that can change live files must invalidate or replace the
   baseline before those files can be mistaken for A.
8. `@` alone is not a physical baseline. The journal records exact tree IDs,
   operation identity, and snapshot identity. Semantic and physical state
   can differ during checkout, reset, recovery, or Git import and export.
9. No clean transition depends on saved per-path rows.

## Durable baseline journal

One small record under `.jj/working_copy/` is the authoritative state.
Jujutsu replaces it atomically. It must not reuse `tree_state` to store
per-path data.

Normal local working copies replace `checkout` with a combined journal that
starts with magic bytes. This saves the operation identity and baseline
in one atomic update. Standalone `TreeState` users such as `jj run` have
no checkout identity and use `working_copy_state`.

Conceptually it contains:

~~~text
format_version
workspace_name
working_copy_operation_id
working_copy_root_identity
tree_ids
conflict_labels
sparse_patterns
interpretation_input_fingerprint
backend_kind
baseline_snapshot_identity?
baseline_lineage_token?
baseline_retention_token?
generation
phase
~~~

`working_copy_root_identity` records enough path, filesystem, mount, and
subvolume identity to reject a journal used from a different physical root.
This includes a journal copied there. `interpretation_input_fingerprint`
covers inputs outside snapshot B. Examples include global excludes and
colocated Git metadata such as `.git/info/exclude` or sparse-checkout
configuration.

`phase` is one of:

~~~text
Clean {
  tree: X,
  baseline: A,
}

PendingBaselineCommit {
  prior_clean: (X, A),
  candidate_tree: Y,
  candidate_snapshot: B,
  transition_id,
}

PendingMaterialization {
  prior_state: Clean(X, A) | NoBaseline(X),
  intended_tree: Z,
  mutation_kind,
}

NoBaseline {
  tree: X,
  reason,
}
~~~

`NoBaseline` supports non-authoritative backends and conservative fallbacks.
It keeps the semantic working-copy tree, but requires a full scan before
another incremental transition. The journal contains only references,
fingerprints, and small tree metadata. It has no row for each path.

### Downgrade behavior

An older binary may treat a missing `tree_state` file as an uninitialized
working copy. Removing the file alone is therefore unsafe. The new format
must make older readers stop before they initialize empty state. Two
acceptable mechanisms are:

- Replace `checkout` with a versioned format whose first bytes cannot be
  decoded as the legacy Checkout protobuf.
- Temporarily keep a `tree_state` tombstone that causes older readers to
  reject the workspace. It must not serve as tree state.

Keep the tombstone during the compatibility window. It can be removed when
the working-copy format version itself prevents old readers from opening
the workspace.

## Authoritative delta contract

`ExactPaths` and `Prefixes` each describe a complete delta. If the backend
cannot prove completeness, it reports that a full scan is required. A caller
that allows full invalidation may receive `Full`. An ordinary command that uses
snapshots fails instead.

The delta must cover every change that can affect how Jujutsu interprets files:

- existence, creation, deletion, and file/directory transitions;
- regular-file bytes and writes that change size;
- executable-bit changes;
- symlink creation, deletion, and target changes;
- Git-submodule entries where supported;
- directory entry changes needed to find additions and deletions;
- renames, hardlink aliases, and reference changes;
- ignore-file changes and prefixes they can reveal or hide;
- nested-subvolume or mount-topology changes that require `Full`;
- uncertainty, overflow, pruned lineage, daemon restart, or unsupported
  filesystem behavior.

For an exact-path delta, Jujutsu reads only those paths from B. For a prefix
delta, it enumerates both the affected prefix in B and the matching subtree
in X. Comparing those entries handles additions, deletions, and type changes
without a per-path cache. Explicit initialization or rebuilding can scan all
sparse paths in B and make B the new baseline.

The semantic tree supplies tracked membership and unchanged values. Jujutsu
uses it to find deletions under changed prefixes, replacing the old
file-state vector.

## Incremental status algorithm

For an authoritative backend that retains snapshots:

1. Load the journal and require `Clean(X, A, I)`.
2. Check root identity, backend compatibility, sparse patterns, the external
   interpretation fingerprint, and A's durable retention.
3. Request B and a complete A→B delta.
4. If a full scan is required, fail the ordinary command that uses snapshots.
   Ask the user to rebuild the baseline explicitly.
5. If the delta contains exact paths or prefixes:
   - start from tree X;
   - use X to identify tracked entries under changed prefixes;
   - read metadata and content only for affected paths in B;
   - remove paths absent from B;
   - apply changed values to produce Y.
6. Save Y↔B with the crash-safe handoff below.

Conflict materialization and executable-bit handling must not reintroduce
saved per-path rows:

- For each changed conflicted path, derive the expected conflict representation
  from X and the active conflict-marker settings. Compare it with that path
  in B.
- During checkout, capture the live path's executable bit before overwriting
  it. Alternatively, read the bit from the retained snapshot if that snapshot
  is proved to match the live state before the write.

For non-authoritative backends, load X from the journal and scan the full
live root. This scan may update X and sparse state. It does not establish
a reusable A→B baseline.

## Crash-safe baseline publication

Renaming a local file and retaining a backend snapshot cannot use one
transaction. The state machine orders these steps so a crash can leave an
extra retained snapshot, but cannot leave a clean journal that names a
pruned snapshot.

For accepted scan B:

1. Build candidate tree Y from B while retaining A.
2. Ask AWACS to pin B as pending for this owner. Keep the committed pin for A.
3. After promotion succeeds, atomically write `Clean(Y, B, I)`.
4. Acknowledge and finish the scan. Atomically replace retained A with B.

Recovery rules:

- If the process crashes before B promotion, continue from clean A.
- If it crashes after promotion but before saving the clean record, the
  journal still names A. The next Begin call discards pending B and keeps A.
- If it crashes after saving clean B but before releasing A, use B. The next
  Begin call reconciles the pins and releases A.
- If the completion response is lost after the clean record is saved, only
  cleanup remains. The lost response cannot invalidate the durable B record.

The backend must retain committed baselines or provide an equivalent pin
handoff. A scan lease that releases B at `FinishScan(Committed)` is too short.

### Durable AWACS owner

The journal stores one random, stable `awacs_baseline_owner_id` for the
JJ workspace. This ID is independent of the root path, workspace name,
and mutable `@` commit. AWACS uses it to keep one committed baseline pin
and, during handoff, one pending candidate pin. The journal's exact snapshot
UUID connects the physical state to the semantic tree. The owner ID only
identifies whose snapshots AWACS must retain.

## Working-copy mutations

Status with a snapshot backend reads immutable B. Checkout, reset, recover,
sparse materialization, and colocated Git import/export still operate on the
live root. Each must explicitly update or invalidate the physical baseline.

For an operation that can change physical files or reinterpret them:

1. Abort any pending scan.
2. Atomically write `PendingMaterialization`. Record the prior journal state,
   intended semantic tree, and intended configuration.
3. Invalidate the reusable baseline before writing live files.
4. Materialize files against the live root.
5. On success, take and save a new authoritative baseline, or write
   `NoBaseline(intended_tree)`. Ordinary status with a snapshot backend fails if
   no reusable baseline remains.
6. After an interruption, recover from `PendingMaterialization` before
   importing Git HEAD, snapshotting, or reporting a clean working copy.

This journal replaces the existing `pending_checkout` TODO. It prevents
Jujutsu from reporting clean state after the semantic tree changes but
before matching files on disk are proved.

Changes to sparse patterns or external ignore inputs must invalidate the
baseline. The backend may preserve it only if it proves a complete transition
under the new interpretation. Colocated Git HEAD import/export failures and
tree resets have the same requirement.

## Fallback, migration, and cleanup

### Fallback

Do not reuse an uncertain delta. An explicit full scan may be needed when:

- A is missing, pruned, expired, or cannot be opened;
- filesystem, subvolume, mount, root, or lineage identity differs;
- the backend returns `Full` or cannot prove that the delta is complete;
- sparse patterns or external interpretation fingerprints changed;
- an accepted scan lease fails before the baseline is saved;
- Jujutsu cannot reconcile a pending materialization.

With a snapshot backend, ordinary snapshots fail when the baseline is unusable or
a full traversal is required. They direct the user to
`jj util subvolume enable --rebuild-baseline`. Other backend or filesystem
errors may need a different fix. Initialization and rebuilding may scan all
of immutable B and make it the next baseline. With `none`, ordinary Watchman,
or another non-authoritative backend, scan the full live root and save
`NoBaseline`.

### Migration from the legacy protobuf

Migration must not read or copy the O(N) file-state rows:

1. Read only the old semantic tree IDs, conflict labels, sparse patterns,
   operation identity, and backend cursor and metadata. Skip the removed
   row field during protobuf decoding, without allocating its rows.
2. Write the new journal as `NoBaseline(X)`. The backend may instead use A
   if it verifies and durably promotes the old cursor's exact snapshot.
3. Establish a fresh clean baseline with an explicit scan of an immutable
   snapshot. Ordinary status with a snapshot backend must fail until a reusable
   baseline exists.
4. After the new journal is durable, remove the legacy `tree_state` payload.

### State cleanup

The target working-copy journal needs no database, WAL, compaction, row count,
or path-range index. Remove:

- legacy `tree_state` protobuf payloads;
- `FileStatesMap`, `FileStates`, and `TreeStateDelta`.

## Alternatives considered

### Keep the protobuf and stream or compress it

Streaming avoids a clone, and compression reduces bytes. Both still require
O(N) encoding, writing, reading, and decoding. Neither uses the retained
physical baseline to supply unchanged state.

### Immutable full blob or range chunks

A manifest with an immutable file-state blob still stores a per-path index.
Custom copy-on-write chunks do the same. They need balancing, compaction,
recovery, and garbage collection. Retaining snapshots already supplies the
physical index.

### Append-only per-path delta journal

A per-path journal makes small writes cheap. Loading and compaction then
need to manage its history. This design only needs the small journal that
records the current baseline transition.

### Treat snapshot identity as tree identity

Snapshot UUIDs identify physical cuts. They do not describe Jujutsu's
content normalization, executable policy, ignore interpretation, conflict
materialization, copy IDs, or Git-submodule semantics. The journal must
connect the snapshot identity to explicit Jujutsu tree IDs.

## Implementation plan

1. **Introduce the small journal and downgrade barrier.**
   Add a versioned `working_copy_state` record, or replace `checkout` with
   one atomic record. Include operation and workspace identity, tree IDs
   and labels, sparse state, interpretation fingerprint, baseline identity,
   and phase. Add a rejecting marker or version check so older binaries
   stop safely when `tree_state` disappears.

2. **Add pending materialization recovery first.**
   Use `PendingMaterialization` for checkout, reset, recover, sparse changes,
   and colocated Git transitions. If a phase was interrupted, recover or
   require an explicit rebuild before importing HEAD or snapshotting.
   An interrupted sparse change requires manual reconciliation. Jujutsu
   cannot distinguish partial materialization from user deletions.

3. **Extend the AWACS retention contract.**
   Retain A while deriving A→B. Lease B during the scan and promote it with
   a transition ID that makes retries safe. Save the local clean record,
   then release A. Report missing lineage or an incomplete delta as
   requiring a full scan.

4. **Refactor snapshotting around semantic tree plus delta.**
   Use `MergedTree` lookups and B reads at exact paths or changed prefixes.
   Replace the file-state logic for tracked membership, deletion detection,
   and mtime shortcuts. Keep full scans as the reference result for tests
   and for explicit initialization or rebuilding.

5. **Remove the remaining FileState-only behavior.**
   Derive conflict materialization for changed paths from the semantic tree.
   Capture or read old executable bits during checkout without saving rows.
   Remove public and debug consumers of `file_states`.

6. **Migrate without loading rows.**
   Read only legacy protobuf metadata. Write `NoBaseline(X)`, then establish
   a fresh retained baseline with an explicit full scan of an immutable
   snapshot.

7. **Delete the old implementations.**
   Remove per-path proto messages, `FileStatesMap`, and `TreeStateDelta`.

8. **Roll out behind validation and measure the real target.**
   Enable the authoritative path only after crash, lineage, mutation, and
   full-scan comparison tests pass. Benchmark a million-path workspace.
   Require one-file status to avoid O(N) state loading, sorting, and
   filesystem scanning.

### Concrete implementation seams

- `lib/src/local_working_copy.rs`: replace `TreeState` persistence and
  `FileStatesMap`. Remove file-state dependencies from the scanner and
  metadata dependencies from checkout.
- `lib/src/working_copy.rs`: document and enforce baseline and pending-phase
  behavior across `snapshot()`, `finish()`, reset, and recovery.
- `lib/src/fsmonitor.rs` and the AWACS client API: expose complete,
  authoritative deltas and retention of committed baselines.
- `lib/src/protos/local_working_copy.proto`: replace per-path tree-state
  messages with the small journal format, or retire the file and use a new
  versioned record.
- `cli/src/commands/debug/local_working_copy.rs`: report journal phase,
  baseline identity, and fallback reason instead of file-state rows.
- `lib/tests/test_local_working_copy.rs` and AWACS integration tests: test
  baselines, crashes, migration, and agreement with full scans.

## Test plan

### Correctness

- Compare exact-path edits, additions, deletions, file/directory transitions,
  symlinks, executable-bit changes, Git submodules, conflicts, renames,
  and hardlinks against a full scan.
- For prefix invalidation, enumerate only the affected B prefixes and
  matching X subtrees. Handle missing directories correctly.
- Change interpretation inputs to invalidate a baseline. Check that ordinary
  commands that use snapshots fail and require an explicit rebuild.
- During a complete one-path transition, do not read, hash, decode from rows,
  or sort unchanged tracked paths.
- Check that incremental snapshots repeat untracked or ignored output
  when it must be reported again.
- Keep `none` and ordinary Watchman correct through conservative full scans.

### Retention and crash recovery

- Retain A while scanning B and until the clean B record is durable.
- Report a required full scan for missing or pruned A, daemon restart,
  broken lineage, or overflow. Malformed deltas and unsupported topology
  may fail outright or require full invalidation.
- Inject faults before and after the pending write, B promotion, and clean
  record save, and before releasing A. Each failure must leave usable A,
  usable B, or an explicit requirement to rebuild the baseline.
- Retry transition IDs without releasing A twice or losing B.
- Recover interrupted checkout, reset, recover, and Git transitions before
  status can report a clean working copy. Interrupted sparse transitions
  must fail before snapshotting or changing files. Keep the files and
  journal for reconciliation.

### Migration and downgrade

- Migrate legacy protobuf metadata without reading file-state rows.
  Reject legacy SQLite markers without opening a database.
- After migration, fail ordinary status with a snapshot backend if no valid
  baseline exists. Allow an explicit rebuild when the state is suitable.
- Check that new-format commands never create, open, or change SQLite state.
- Make older binaries stop safely. They must not treat a missing
  `tree_state` as an empty working copy.
- Remove Jujutsu's working-copy SQLite dependency and active `tree_state`
  payload. Optional AWACS internals may still use SQLite for their service
  store.

### Performance

Measure at least:

- unchanged A→B;
- one-file edit, deletion, and rename;
- changed directory or prefix;
- 10,000 changed paths;
- missing-baseline failure and explicit rebuild;
- checkout, reset, or recover followed by baseline establishment;
- startup latency, journal bytes written, retained-snapshot count,
  and end-to-end status latency.

For a million-path working copy, steady-state one-file status must avoid
O(N) SQLite or protobuf reads, reconstructing the full file-state vector,
sorting that vector, and traversing the full filesystem.

## Open questions

- What exact AWACS API should durably promote a baseline and safely retry
  recovery after a lost response?
- How should later incremental scans report unchanged untracked output
  without an O(N) path catalog?
- Which external ignore and configuration inputs belong in the interpretation
  fingerprint?
- When can Jujutsu save a new baseline immediately after a successful checkout?
  When can it recover a pending materialization?
- Is a separate recovery tool needed for intermediate SQLite workspaces?
  Or is it sufficient to reject them and require a compatible JJ for recovery?

## Future possibilities

- Use directory descriptors and `openat()` throughout snapshot traversal.
- Let other immutable or virtual working-copy backends implement the same
  retained-baseline contract.
- Add a representation for untracked results with an explicit size limit,
  only if it preserves the goal of no O(N) state.
- Expose baseline generation, retention identity, phase, and fallback reason
  in `jj debug local-working-copy`.
