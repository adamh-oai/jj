# Snapshot-baseline working-copy state

Author: [Adam Hupp](mailto:adamh@openai.com)

**Summary:** Replace both the monolithic protobuf `tree_state` and the
intermediate SQLite `file_states` store with a small crash-safe baseline
journal. When an authoritative backend retains immutable snapshot A, the
journal binds A to the last durable Jujutsu working-copy tree X plus the
sparse and ignore inputs used to interpret it. The next status obtains
snapshot B and a complete A→B delta, inspects only changed paths from B,
applies those changes to X, and publishes a new `(tree Y, snapshot B)`
binding. No per-path mtime, size, type, or conflict-marker rows are persisted.
If retention, lineage, delta completeness, or interpretation-input continuity
cannot be proved, Jujutsu performs a full scan and establishes a new baseline.

## Objective

`tree_state` is durable physical-working-copy state, not a duplicate commit.
The commit stores semantic content and tree IDs. The old `tree_state` file
stores scan metadata such as per-path mtime, size, type, sparse patterns, and
the filesystem-monitor cursor so a later status can avoid hashing everything.

That representation is useful for ordinary mutable filesystems, but it is the
wrong source of truth when the filesystem backend already retains an
authoritative immutable snapshot. If snapshot A is known to correspond to
Jujutsu tree X, unchanged paths in A→B retain their semantic values from X.
Only paths in a complete A→B delta need to be read from B.

The SQLite prototype removes O(N) durable writes but not O(N) command startup:
it still selects, decodes, reconstructs, and sorts every file-state row. On a
measured 1.15-million-row working copy, a one-file incremental status remains
about 9.6 seconds. SQLite is therefore an intermediate implementation, not
the target architecture.

The desired steady-state costs are:

| Snapshot shape | Target work |
| --- | --- |
| No change A→B | Bounded journal update; no per-path load |
| k changed paths | Complete A→B delta plus O(k) reads/hashes from B |
| Changed directory/prefix | Enumerate only the affected prefix in B and the matching subtree in X |
| Missing or pruned A, incomplete delta, changed interpretation inputs | Full scan of immutable B, then establish B as the new baseline |
| Checkout, reset, recover, or sparse materialization | Establish a new physical baseline after successful materialization, or invalidate |
| No authoritative retained-snapshot backend | Conservative full scan |

## Current state and why it is insufficient

The legacy `TreeState` protobuf persists:

- current tree IDs and conflict labels;
- every tracked path's mtime, size, file type, and optional materialized
  conflict-marker length;
- sparse patterns;
- a backend-tagged filesystem-monitor cursor.

The SQLite implementation moves the same logical state into one metadata row
and one row per tracked path. It makes a cursor-only or small-change save
proportional to the durable delta, but loading still reconstructs the complete
in-memory `FileStatesMap` before the scanner can use an incremental matcher.
It therefore preserves the O(N) read, decode, allocation, validation, and sort
cost on every new command.

With retained immutable snapshots, those rows duplicate facts already
available from two authoritative sources:

- tree X is the semantic truth for paths unchanged since A;
- snapshot A is the physical truth for the last accepted working-copy state;
- snapshot B plus a complete A→B delta identifies the only paths whose
  physical state may need to be reinterpreted.

The target removes the durable per-path cache instead of making it more
queryable.

## Scope and assumptions

The O(delta) path requires an authoritative backend that can:

1. retain the baseline snapshot named by the durable journal;
2. prove that a new snapshot B descends from that exact A;
3. return a complete Jujutsu-observable A→B delta, or explicitly return
   `Full`;
4. keep B available until Jujutsu durably publishes it as the next baseline;
5. recover idempotently from crashes between backend retention and local
   journal publication.

AWACS is the first such backend. Ordinary `none` and Watchman backends do
not provide an immutable retained baseline, so after the per-path table is
removed they conservatively full-scan. They may continue to narrow a scan
only if a future backend supplies an equivalent authoritative contract.

This design removes durable per-path state. It does not require every
working-copy backend to become incremental.

## Goals and non-goals

### Goals

- Eliminate the persisted per-path mtime/size/type/conflict-marker table.
- Replace both the legacy protobuf `tree_state` payload and SQLite
  `file_states` database with one small versioned baseline journal.
- Bind a semantic Jujutsu tree and an exact physical immutable snapshot in one
  durable record.
- Make steady-state status reads and writes proportional to the complete
  filesystem delta rather than total tracked-path count.
- Make snapshot retention handoff and local publication crash-safe.
- Preserve the existing working-copy lock and single-writer model.
- Fail conservatively on missing retention, broken lineage, incomplete
  deltas, changed sparse/ignore interpretation, corruption, or downgrade.
- Make checkout, reset, recover, sparse changes, and colocated Git transitions
  explicit physical-baseline boundaries.

### Non-goals

- Providing incremental status for `none` or ordinary Watchman without an
  authoritative retained immutable baseline.
- Persisting file contents, per-path stat rows, an untracked-path catalog, or
  a second semantic tree in the journal.
- Making live working-copy writes transactional with a filesystem snapshot.
- Replacing the existing working-copy lock with backend or database locking.
- Treating a snapshot UUID as semantic content equality.
- Silently accepting a best-effort or incomplete change list as an
  incremental baseline transition.

## Correctness model

The durable clean state is:

~~~text
Clean {
  semantic_tree: X,
  baseline_snapshot: A,
  interpretation_inputs: I,
}
~~~

X is the exact merged tree IDs and conflict labels that Jujutsu produced by
reading A. A is the retained immutable physical snapshot plus the backend
identity and lineage token needed to prove the next transition. I is the
sparse view and fingerprint of inputs outside the snapshot that affect how
paths are interpreted.

The normal transition is:

~~~text
load Clean(X, A, I)
validate A is retained and I is unchanged
BeginScan(A) -> leased B + CompleteDelta(A→B) | Full
if CompleteDelta:
  apply only B reads for changed paths/prefixes to X
else:
  full-scan B
produce semantic tree Y
durably retain/promote B
atomically publish Clean(Y, B, I)
release A
~~~

The following invariants are required:

1. A clean journal entry names an immutable snapshot identity plus enough
   backend proof state to either reopen/prove that snapshot or force `Full`.
   A hard-pinned backend additionally retains that snapshot until the next
   clean binding is durable.
2. The journal's semantic tree was produced only from that named snapshot and
   the recorded interpretation inputs.
3. A delta is used only when the backend proves complete continuity from the
   exact retained A to B.
4. Every Jujutsu-observable difference in A→B is either represented in the
   delta or causes `Full`.
5. Jujutsu never publishes B as clean until all reads used to produce Y came
   from B and B is durably retained.
6. Jujutsu does not release A until the clean B binding is durable.
7. A command that may change live materialization invalidates or replaces the
   baseline before the changed physical state can be mistaken for A.
8. `@` alone is not a physical baseline. The journal binds exact tree IDs,
   operation identity, and snapshot identity because semantic and physical
   state can diverge during checkout, reset, recovery, or Git import/export.
9. No clean transition depends on persisted per-path rows.

## Durable baseline journal

The authoritative state is one small, atomically replaced record under
`.jj/working_copy/`. It must not reuse `tree_state` as a per-path store.
Normal local working copies replace the existing `checkout` record with a
magic-prefixed combined journal so operation identity and baseline publication
are committed together. Standalone `TreeState` users such as `jj run`, which
have no checkout identity, use `working_copy_state`.

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

`working_copy_root_identity` includes enough path, filesystem, mount, and
subvolume identity to reject a journal copied to or opened from a different
physical root. `interpretation_input_fingerprint` covers inputs not captured
inside snapshot B, including global excludes and colocated Git metadata such
as `.git/info/exclude` or sparse-checkout configuration.

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

`NoBaseline` is valid for non-authoritative backends and conservative
fallbacks. It preserves the semantic working-copy tree but requires a full
scan before another incremental transition. The journal stores references,
fingerprints, and small tree metadata only; it never contains one row per
path.

### Downgrade behavior

Removing `tree_state` outright is unsafe if an older binary treats a missing
file as an uninitialized working copy. The new format must make older readers
fail closed before they can initialize empty state. Acceptable mechanisms are:

- a versioned replacement for `checkout` whose first bytes are deliberately
  invalid as the legacy Checkout protobuf; or
- a temporary, non-authoritative poison `tree_state` tombstone during the
  compatibility window.

The tombstone is a downgrade tripwire, not tree state. It can be removed once
the working-copy format version itself prevents old readers from opening the
workspace.

## Authoritative delta contract

`ExactPaths` and `Prefixes` are not advisory hints in this design. They are
complete delta shapes. If the backend cannot prove completeness, it returns
`Full`.

The delta must cover every change that can alter Jujutsu's interpretation:

- existence, creation, deletion, and file/directory transitions;
- regular-file bytes and size-affecting writes;
- executable-bit changes;
- symlink creation, deletion, and target changes;
- Git-submodule entries where supported;
- directory entry changes needed to discover additions and deletions;
- renames, hardlink aliases, and reference changes;
- ignore-file changes and the affected prefixes they can reveal or hide;
- nested-subvolume or mount-topology changes that require `Full`;
- any uncertainty, overflow, pruned lineage, daemon restart, or unsupported
  filesystem behavior.

An exact-path delta causes Jujutsu to read only those paths from B. A prefix
delta causes Jujutsu to enumerate the affected prefix in B and enumerate the
matching semantic subtree in X so additions, deletions, and type transitions
are handled without a per-path cache. A `Full` result scans all sparse paths
from B and establishes B as a new baseline.

The semantic tree supplies tracked membership and unchanged values. It replaces
the old file-state vector for deletion detection under changed prefixes.

## Incremental status algorithm

For an authoritative retained-snapshot backend:

1. Load the journal and require `Clean(X, A, I)`.
2. Verify root identity, backend compatibility, sparse patterns, external
   interpretation fingerprint, and A's durable retention.
3. Request B and a complete A→B delta.
4. If the result is `Full`, enumerate the sparse view from B and the
   matching tracked paths from X, then rebuild Y without per-path rows.
5. If the result is exact paths or prefixes:
   - start from semantic tree X;
   - use X to identify tracked entries under changed prefixes;
   - read metadata/content only for affected B paths;
   - remove paths absent from B;
   - apply changed values to produce Y.
6. If the result contains untracked or ignored outcomes that are not
   represented in Y and must be reported again on a later command, publish
   `NoBaseline(Y, requires_full_scan)` rather than a reusable clean baseline.
7. Publish Y↔B with the crash-safe handoff below.

Conflict materialization and executable-bit preservation must not reintroduce
durable per-path rows:

- For changed conflicted paths, derive the expected materialized conflict
  representation from X and the active conflict-marker settings, then compare
  only that changed path in B.
- For checkout, capture the old live-path executable bit before overwriting it,
  or read it from the current retained snapshot when that snapshot is proven
  to match the live pre-mutation state.

For non-authoritative backends, load X from the journal and full-scan the live
root. Such a scan may update X and sparse state, but it does not publish a
reusable A→B baseline.

## Crash-safe baseline publication

Local file rename and backend retention cannot be one transaction. The state
machine therefore orders them so a crash may leak a harmless retained snapshot
but can never leave a clean journal pointing at a pruned snapshot.

For accepted scan B:

1. Build candidate tree Y from B while A remains retained.
2. Ask AWACS to stage B as a pending owner pin while retaining committed A.
3. After promotion succeeds, atomically write `Clean(Y, B, I)`.
4. Acknowledge/finish the scan and atomically replace A with B.

Recovery rules:

- Crash before B promotion: continue from clean A.
- Crash after B promotion but before clean publication: the journal still
  names A, so the next Begin discards pending B and keeps A.
- Crash after clean B publication but before A release: use B and release A
  during the next Begin reconciliation.
- Lost completion response after clean publication is cleanup only; it cannot
  invalidate the durable B binding.

The backend protocol must therefore support committed-baseline retention or an
equivalent pin/handoff. A short-lived scan lease that releases B at
`FinishScan(Committed)` is insufficient.

### Durable AWACS owner

The journal stores one random, stable `awacs_baseline_owner_id` for the JJ
workspace. It is not the root path, workspace name, or mutable `@` commit.
AWACS uses it to keep one committed baseline pin and, during handoff, one
pending candidate pin. The exact snapshot UUID in the journal binds that
physical state to the semantic tree; the owner ID only scopes retention.

## Working-copy mutations

Snapshot-backed status reads immutable B. Checkout, reset, recover, sparse
materialization, and colocated Git import/export still mutate or reason about
the live root. These operations are physical-baseline boundaries.

Before a mutation that can change physical files or reinterpret them:

1. Abort any pending scan.
2. Atomically write `PendingMaterialization` with the prior journal state and
   intended semantic tree/configuration.
3. Invalidate the reusable baseline before live writes begin.
4. Materialize against the live root.
5. On success, either cut and publish a new authoritative baseline or write
   `NoBaseline(intended_tree)` so the next status full-scans.
6. On interruption, recover from `PendingMaterialization` before importing
   Git HEAD, snapshotting, or claiming the working copy is clean.

This journal replaces the existing `pending_checkout` TODO. It also closes
the false-clean class where semantic state changes before physical
materialization is proved.

Sparse-pattern changes and changed external ignore inputs must invalidate the
baseline unless the backend can prove a complete transition under the new
interpretation. Colocated Git HEAD import/export failures and tree resets must
do the same.

## Fallback, migration, and cleanup

### Fallback

The safe fallback is a full scan, not reuse of an uncertain delta. Full scan is
required when:

- A is missing, pruned, expired, or cannot be opened;
- filesystem, subvolume, mount, root, or lineage identity differs;
- the backend returns `Full` or cannot prove delta completeness;
- sparse patterns or external interpretation fingerprints changed;
- an accepted scan lease fails before publication;
- a pending materialization cannot be reconciled;
- untracked/ignored output cannot be represented by the semantic tree;
- the backend is `none`, ordinary Watchman, or otherwise non-authoritative.

When an immutable B is available, full-scan B and establish it as the next
baseline. When no immutable backend is available, full-scan the live root and
persist `NoBaseline`.

### Migration from the legacy protobuf

Migration must not read or copy the O(N) file-state rows:

1. Read only the old semantic tree IDs/conflict labels, sparse patterns,
   operation identity, and backend cursor/metadata from the legacy protobuf.
   Protobuf decoding skips the removed row field instead of allocating it.
2. Write the new journal as `NoBaseline(X)`, unless the backend can verify
   and durably promote the old cursor's exact snapshot as A.
3. Force the next authoritative status to full-scan an immutable B and
   establish a fresh clean baseline.
4. After the new journal is durable, remove the legacy `tree_state` payload.

### State cleanup

The target has no database, WAL, compaction, row count, or path-range index.
Cleanup removes:

- legacy `tree_state` protobuf payloads;
- `FileStatesMap`, `FileStates`, and `TreeStateDelta`;

## Alternatives considered

### Keep the protobuf and stream or compress it

Streaming avoids a clone and compression reduces bytes, but both keep O(N)
encode, write, read, and decode work. They do not use the retained physical
baseline as the source of truth.

### Immutable full blob or range chunks

A manifest plus immutable file-state blob, or custom copy-on-write chunks,
still persists a per-path index and needs balancing, compaction, recovery, and
garbage collection. Snapshot retention already provides the physical index.

### Append-only per-path delta journal

A per-path journal makes small writes cheap but turns load and compaction into
history management. The only durable journal needed here is the bounded
baseline state machine.

### Treat snapshot identity as tree identity

Snapshot UUIDs identify physical cuts, not semantic Jujutsu trees. They do not
encode content normalization, executable policy, ignore interpretation,
conflict materialization, copy IDs, or Git-submodule semantics. The journal
must bind snapshot identity to explicit Jujutsu tree IDs.

## Implementation plan

1. **Introduce the small journal and downgrade barrier.**
   Add a versioned `working_copy_state` record, or replace `checkout` with
   one atomic record, containing operation/workspace identity, tree IDs and
   labels, sparse state, interpretation fingerprint, baseline identity, and
   phase. Add a poison/version tripwire so older binaries fail closed when
   `tree_state` disappears.

2. **Add pending materialization recovery first.**
   Implement `PendingMaterialization` around checkout, reset, recover,
   sparse changes, and colocated Git transitions. Recover or force full scan
   before HEAD import/snapshot if an interrupted phase is found.

3. **Extend the AWACS retention contract.**
   Add committed-baseline pin/handoff semantics: retain A while deriving A→B,
   lease B during scan, idempotently promote B with a transition ID, publish
   the local clean binding, then release A. Treat missing lineage or any
   incomplete delta as `Full`.

4. **Refactor snapshotting around semantic tree plus delta.**
   Replace file-state-driven tracked membership, deletion detection, and
   mtime shortcuts with `MergedTree` lookups and B reads limited to exact
   paths or changed prefixes. Preserve full-scan behavior as the oracle and
   fallback.

5. **Remove the remaining FileState-only behavior.**
   Derive conflict materialization for changed paths from the semantic tree and
   capture/read old executable bits during checkout without persisted rows.
   Remove public/debug consumers of `file_states`.

6. **Migrate without loading rows.**
   Read legacy protobuf metadata only, write `NoBaseline(X)`, and establish a
   fresh retained baseline on the next full immutable scan.

7. **Delete the old implementations.**
   Remove per-path proto messages, `FileStatesMap`, and `TreeStateDelta`.

8. **Roll out behind validation and measure the real target.**
   Gate the authoritative path until crash, lineage, mutation, and full-scan
   oracle tests pass. Benchmark a million-path workspace and require one-file
   status to perform no O(N) state load, sort, or filesystem scan.

### Concrete implementation seams

- `lib/src/local_working_copy.rs`: replace `TreeState` persistence,
  `FileStatesMap`, scanner file-state dependencies, and checkout metadata
  dependencies.
- `lib/src/working_copy.rs`: document and enforce baseline/pending-phase
  semantics across `snapshot()`, `finish()`, reset, and recovery.
- `lib/src/fsmonitor.rs` and the AWACS client boundary: expose authoritative
  complete deltas plus committed-baseline retention.
- `lib/src/protos/local_working_copy.proto`: replace per-path tree-state
  messages with the small journal format or retire the file in favor of a new
  versioned record.
- `cli/src/commands/debug/local_working_copy.rs`: report journal phase,
  baseline identity, and fallback reason rather than file-state rows.
- `lib/tests/test_local_working_copy.rs` and AWACS integration tests: add
  baseline, crash, migration, and oracle coverage.

## Test plan

### Correctness

- Exact-path edits, additions, deletions, file/directory transitions,
  symlinks, executable-bit changes, Git submodules, conflicts, renames, and
  hardlinks match a full-scan oracle.
- Prefix invalidations enumerate only affected B prefixes plus matching X
  subtrees and handle missing directories correctly.
- Ignore-file changes, global excludes, sparse changes, and colocated Git
  metadata changes force the required full scan.
- Unchanged tracked paths are not read, hashed, decoded from rows, or sorted
  during a complete one-path transition.
- Untracked/ignored output that cannot be replayed from X prevents reuse of a
  clean baseline.
- `none` and ordinary Watchman remain correct via conservative full scan.

### Retention and crash recovery

- A is retained while B is scanned and until clean B publication is durable.
- Missing/pruned A, daemon restart, broken lineage, overflow, malformed
  delta, or unsupported topology returns `Full`.
- Fault injection before pending write, after pending write, before B
  promotion, after B promotion, before clean publication, after clean
  publication, and before A release leaves either usable A, usable B, or an
  explicit full-scan requirement.
- Retried transition IDs do not double-release A or lose B.
- Interrupted checkout/reset/recover/sparse/Git transitions are recovered
  before status can claim cleanliness.

### Migration and downgrade

- Legacy protobuf metadata migrates without reading file-state rows; legacy
  SQLite markers fail closed without opening a database.
- The first post-migration authoritative status full-scans B and establishes a
  valid clean baseline.
- New-format commands never create, open, or mutate SQLite state.
- Older binaries fail closed instead of treating missing `tree_state` as an
  empty working copy.
- No Jujutsu working-copy SQLite dependency or active `tree_state` payload
  remains. Optional AWACS internals may independently use SQLite for their
  service store.

### Performance

Measure at least:

- unchanged A→B;
- one-file edit, deletion, and rename;
- changed directory/prefix;
- 10,000 changed paths;
- missing-baseline full fallback;
- checkout/reset/recover followed by baseline establishment;
- startup latency, journal bytes written, retained-snapshot count, and
  end-to-end status latency.

For a million-path working copy, require the steady-state one-file path to
avoid O(N) SQLite/protobuf reads, full-vector reconstruction, full sort, and
full filesystem traversal.

## Open questions

- What exact AWACS API represents durable baseline promotion and idempotent
  recovery after a lost response?
- Can unchanged untracked output be represented compactly, or should any such
  output continue to force `NoBaseline`?
- Which external ignore/config inputs belong in the interpretation fingerprint?
- After a successful checkout, should Jujutsu cut a new baseline immediately
  or defer to the next status full scan?
- Should a separate recovery tool for intermediate SQLite workspaces be
  provided, or is fail-closed recovery with a compatible JJ sufficient?

## Future possibilities

- Use directory fds and `openat()` throughout snapshot traversal.
- Let other immutable or virtual working-copy backends implement the same
  retained-baseline contract.
- Add a bounded untracked-result representation only if it preserves the
  no-O(N)-state goal.
- Expose baseline generation, retention identity, phase, and fallback reason
  in `jj debug local-working-copy`.
