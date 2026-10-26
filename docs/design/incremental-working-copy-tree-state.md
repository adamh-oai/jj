# Snapshot-baseline working-copy state

Author [Adam Hupp](mailto:adamh@openai.com)

A working-copy tree records the contents and conflicts that JJ understands.
An AWACS snapshot records the files that existed at one instant. JJ needs
both to determine which paths to read on the next scan.

This draft describes version 4 of the working-copy journal on
`dev/adamh/awacs-snapshots`. The journal saves a JJ tree together with the
snapshot it used. It also records work left over from checkout. The scan
backend supplies the snapshot, the changed paths, and the pins that keep it
available. [Direct AWACS snapshot scans](awacs-snapshot-scans.md)

## Why replace the file-state table

The legacy `tree_state` protobuf stores tree IDs, conflict labels, sparse
patterns, a monitor cursor, and per-path scan metadata. That metadata helps a
scanner of live files avoid hashing every unchanged file.

The intermediate SQLite prototype made small writes cheaper, but each command
still loaded, decoded, rebuilt, and sorted the whole `FileStatesMap`. In the
measured prototype, incremental status for one changed file took about
9.6 seconds on a working copy with 1.15 million rows. The journal described
here still needs an equivalent measurement.

Call the saved filesystem snapshot A, its JJ tree X, and the new snapshot B.
If a path is unchanged between A and B and its interpretation still applies, JJ
can reuse its value from X.
The complete delta selects the paths that need new reads. Storing another
stat table for all tracked paths duplicates work that the immutable baseline
and semantic tree already support.

The target cost depends on more than the number of changed files.

| Situation | Required work |
| --- | --- |
| Empty delta, no deferred or untracked paths | Read and replace the journal without loading per-path stat rows |
| k affected paths | Process the complete delta and read affected paths in B |
| Changed prefix | Enumerate that prefix in B and its tracked subtree in X |
| Deferred checkout paths | Add all saved pending paths and prefixes to the next scan |
| Remembered untracked paths | Revisit those candidates and save the remaining set |
| Missing baseline, incomplete delta, or incompatible semantic inputs | Stop an ordinary subvolume command and require explicit recovery |
| Initialization or explicit rebuild | Permit a full scan of immutable B |
| No authoritative retained snapshot | Use the mutable backend's own correctness strategy |

The steady-state goal is to avoid work proportional to all tracked paths.
The journal stores pending scan work
and remembered untracked candidates. If P paths or prefixes are pending and U candidates
remain untracked, loading and saving those lists costs O(P + U). Large prefixes
can also require many filesystem reads. Performance reports must include these
sets instead of describing every incremental scan as O(k).

## Scope and assumptions

The snapshot path requires a backend that can do the following.

1. Retain the exact baseline named by the journal.
2. Prove a complete transition from that baseline A to immutable B.
3. Keep B available while JJ reads it and saves its replacement state.
4. Keep A until the replacement journal is durable.
5. Recover a retention handoff after a crash or a lost response.

AWACS is the first backend with this contract. A Watchman clock and changed
names do not provide it. On the current AWACS branch, `none` and ordinary
Watchman perform full live scans and do not save a reusable immutable baseline.
Concurrent writes can change a file between two reads in these scans.

The compact format applies to snapshot-mode working copies. Ordinary working
copies can continue to use their legacy persistence format. Removing all
legacy types or changing their mutable-file cache is separate work.

The existing working-copy lock still serializes JJ writers. AWACS's root lock
and manager transactions coordinate backend state. They do not replace the
JJ lock or prevent another application from changing live files during checkout.

## What the journal must preserve

The design must connect explicit JJ tree IDs to an exact physical baseline in
one durable record. It must preserve sparse state, pending work, and enough
interpretation information to decide whether unchanged values can be reused.
An interrupted checkout, reset, recovery, or Git transition must not make JJ
report an unproved state as clean.

Snapshot mode drops the stat row for each tracked file. The journal refers to
existing trees and keeps a list of omitted untracked files for later scans.

The journal does not make live writes, backend retention, and a local file
rename one transaction. It orders those operations so that recovery can keep
usable state or stop with an explicit repair requirement. It must never use an
incomplete delta as if it proved that omitted paths were unchanged.

## Correctness model

A fully reconciled baseline can be written conceptually as follows.

~~~text
CleanBaseline {
  tree: X,
  snapshot: A,
  semantic_inputs: I,
  pending_scan_scope: empty,
  untracked_candidates: U,
}
~~~

X contains exact merged tree IDs and conflict labels. A identifies the retained
immutable filesystem state and includes the token needed to prove the next
transition. I records the sparse view and the semantic input fingerprint.
U records files omitted from X that still need consideration on later scans.

The tree is not a fresh interpretation of every file under the latest ignore
policy. Unchanged paths keep their established tracked or untracked status.
That rule matters when an ignore file or auto-tracking configuration changes.
The section on interpretation inputs defines when JJ may reuse that status.

A normal transition proceeds as follows.

~~~text
load tree X, baseline A, semantic inputs I, and pending/untracked scope
validate A and the inputs needed to reuse unchanged values
BeginScan(A) -> leased B and complete delta D(A, B)
combine D with pending work, untracked candidates, and explicit tracking
read the selected paths in B under the sparse view
apply those results to X to produce Y
stop the renewal worker and check the active lease
durably save Y, B, the new untracked set, and no pending scan work
finish the backend handoff from A to B
~~~

The required invariants are as follows.

1. A usable baseline names the exact immutable endpoint for the next delta.
2. New values come from B. Unchanged values come from X under a complete
   delta and compatible interpretation rules.
3. Any physical changes not yet interpreted remain in the pending scan scope.
4. The saved operation identity, tree, sparse state, and baseline belong to
   one journal generation.
5. A remains retained until the replacement record is durable. B remains
   retained after that save, including when cleanup fails.
6. A command that changes files or the semantic tree records its intent before
   it can leave those states out of agreement.
7. Ordinary subvolume commands reject an unusable baseline or a scope that
   requires traversal of the whole root.
8. `@`, a pathname, and a snapshot UUID each identify different things. None
   alone proves that a semantic tree matches the physical working copy.

## Durable baseline journal

Snapshot-mode local working copies store the combined record in
`.jj/working_copy/checkout`. Combining checkout identity and tree state avoids
a crash between two independent file replacements. Standalone `TreeState`
users have no checkout identity and use `working_copy_state` when they write
this format.

The file starts with `\0JJ-WORKING-COPY-STATE\0v1\n`, followed by the
`WorkingCopyState` protobuf. The magic identifies the container. Its `v1`
suffix is separate from the protobuf's current `format_version`, which is 4.

The schema groups the following fields.

| Fields | Meaning |
| --- | --- |
| `format_version`, `generation` | Format and monotonically advanced save generation |
| `operation_id`, `workspace_name` | JJ operation and workspace identity |
| `tree_ids`, `conflict_labels`, `sparse_patterns` | Current semantic state |
| `fsmonitor_cursor` | Legacy mutable-monitor cursor, separate from AWACS state |
| `baseline` | Typed AWACS snapshot paired with the tree and any pending scope |
| `awacs_baseline_owner_id` | Random stable 16-byte owner of this workspace's AWACS pins |
| `phase`, `no_baseline_reason`, `mutation_kind` | Current transition and diagnostic state |
| `pending_tree_ids`, `pending_conflict_labels`, `pending_sparse_patterns` | Intended result of a materialization |
| `pending_baseline`, `transition_id` | Retained transition state |
| `pending_scan_paths`, `pending_scan_prefixes` | Physical changes not yet interpreted |
| `untracked_scan_paths`, `untracked_scan_complete` | Omitted candidates and proof that the record is complete |

`AwacsSnapshotBaseline` contains `filesystem_uuid`, `subvolume_uuid`,
`continuity_token`, `retention_token`, and
`interpretation_input_fingerprint`. JJ checks the shape of these values.
AWACS verifies their relationship to the requested root, authorization, and
retained history. The journal does not contain a separate trusted pathname
or mount-identity field that can replace those checks.

### Phases and pending work

`WorkingCopyStatePhase` has four values.

| Phase | Interpretation |
| --- | --- |
| `NoBaseline` | The tree is known, but no reusable physical baseline is available |
| `CleanBaseline` | A baseline can drive the next delta, together with any saved pending scope |
| `PendingMaterialization` | A command recorded its intended tree or sparse view before changing files |
| `PendingBaselineCommit` | A serialized transition value retained in the schema and reader |

`CleanBaseline` does not necessarily mean the semantic tree has consumed every
change in the named snapshot. Nonempty `pending_scan_paths` or
`pending_scan_prefixes` make that outstanding work explicit. A source is fully
reconciled only when those sets are empty.

The current normal scan path does not save a `PendingBaselineCommit` record
before committing B. It leaves the prior durable record in place, relies on
AWACS's pending B pin, and atomically saves the replacement record. The reader
still handles serialized pending phases. A future transition protocol must
specify its recovery behavior before it starts using `transition_id` or this
phase for a different purpose.

### Format compatibility and downgrade behavior

The reader accepts journal versions 2, 3, and 4. Version 3 added pending scan
paths and prefixes. Version 4 added remembered untracked paths and their
completeness flag. Readers must reject versions they do not understand, so
they cannot silently discard pending or untracked scope. Decoding an old
journal does not prove that its omitted-file record is complete. Ordinary subvolume scans reject that state until an explicit full
baseline scan establishes completeness.

Pending and untracked paths must be normalized relative repository paths.
Root paths and malformed paths are rejected. A `CleanBaseline` record must
contain valid filesystem and subvolume UUIDs and a continuity token. Strict
subvolume mode rejects a missing usable baseline except through an explicit
repair or a supported pending-materialization recovery.

Removing `tree_state` alone is unsafe because older binaries can treat its
absence as an uninitialized working copy. The magic-prefixed `checkout`
intentionally fails legacy Checkout protobuf decoding. JJ first makes the new
record durable, then removes the old `tree_state` payload and syncs the state
directory. Older binaries stop when they read the new `checkout` record.
The design therefore needs no separate `tree_state` tombstone.

## Interpretation inputs

A physical delta says which filesystem paths changed. JJ must also account for
settings that can change the semantic value of an unchanged tracked path.
The current semantic fingerprint covers effective Git sparse state, JJ sparse
prefixes, EOL conversion mode, and executable-bit policy.

The implementation uses the SHA-256 domain
`jj:awacs-semantic-input-fingerprint:v2\0`. The input struct remains named
`AwacsInputFingerprintV1`. A compatibility calculation can recognize the
original fingerprint once and advance it to the current model without a
replacement full scan, when the old input fingerprint still matches.

Ignore and auto-tracking policy are handled differently. Changes to
`core.excludesFile`, `.git/info/exclude`, `snapshot.auto-track`, fileset aliases,
and the new-file size limit do not by themselves invalidate the baseline.
Untouched paths keep their existing classification. A `.gitignore` delta does
not automatically expand to its whole parent directory and start tracking
previously ignored files that have not changed.

Newly affected paths are evaluated under the current policy. Remembered
untracked candidates are also reconsidered, and an explicit tracking request
can add its own scope. Worktree ignore files come from B. Global excludes and
Git administrative inputs can live outside B and are read separately. Failure
to read a required input is an error.

A changed semantic fingerprint prevents an ordinary incremental scan. Sparse
materialization follows the transition rules below. An explicit rebuild can
establish a new baseline under new inputs when recovery permits it. This
separation prevents a policy-only change from forcing a whole-tree crawl while
still rejecting reuse when unchanged tracked values could be reinterpreted.

## Incremental status algorithm

JJ loads X, A, and the journal's pending and untracked sets. It checks the
strict-mode marker, pending sparse state, baseline, completeness flag, backend,
and semantic fingerprint before accepting an ordinary scan.

AWACS then returns B and a complete delta. Exact paths select individual reads.
Prefixes select both their subtree in B and the matching tracked subtree in X.
The latter is necessary to remove a tracked file that no longer exists in B.
The semantic tree supplies tracked membership and unchanged values.

JJ unions the delta with pending scan paths and prefixes. It adds remembered
untracked paths as prefixes because an omitted file may have become a
directory with new children. Explicit tracking and force-tracking requests can
add further scope. JJ applies its own and Git's effective sparse views.

JJ reads selected content and metadata from B, removes absent tracked paths,
and builds Y. Changed conflicts use X and the active conflict-marker settings
to derive their expected materialization. They do not need a saved row with
conflict-marker length. Checkout must capture an old executable bit before
overwriting a live path, or read it from a baseline proved to match that path.

After a successful semantic scan, JJ clears pending scan work and records the
new untracked set. It keeps existing omitted candidates that still exist even
when ignore or sparse rules prevented visiting them. An inaccessible candidate
is not evidence that it disappeared. Checks must not follow a symlink or other
non-directory ancestor to decide that the old candidate is absent. Once a
replacement directory has been fully visited, JJ can replace that directory
candidate with the omitted children found inside it.

A full or root-wide scope fails in ordinary subvolume mode. Initialization and
an explicit rebuild can scan all selected paths in B. They establish both the
baseline and a complete omitted-file record. A direct `TreeState` caller with
no working-copy finish boundary cannot commit an AWACS lease and must discard
that reusable baseline.

## Crash-safe baseline handoff

JJ's journal rename and AWACS's pin transaction are separate operations. The
safe order keeps the old endpoint until the new local record is durable.

1. Begin asks AWACS for B. AWACS stages a durable pending B pin and keeps the
   committed A pin before it returns the lease.
2. JJ builds Y in memory from the selected reads in B.
3. `LockedLocalWorkingCopy::finish()` stops the renewal worker, checks its
   result, and calls `promote()` to verify that the session is active. The
   current promotion does not extend the lease deadline.
4. JJ writes the combined journal to a temporary file, syncs its contents,
   and replaces the journal. It syncs the state directory as part of saving.
5. JJ calls `finish(Committed)`. AWACS makes B committed and releases A in one
   transaction, then ends the temporary scan lease.

The snapshot method itself does not save the record. The lease must survive
until finish. The operation ID and any workspace-name update join the tree and
baseline in the same record.

| Interruption | Safe result |
| --- | --- |
| Before B is staged | Continue from the saved A |
| After AWACS pins B, before JJ replaces the journal | The journal names A. The next Begin can discard pending B. |
| After replacement, if saving fails or a crash makes durability uncertain | Recovery must keep both pins until it reads the surviving journal. A calls for discarding pending B. B calls for committing B. |
| After the B journal is durable, before completion | The next Begin makes pending B committed and releases A |
| Completion response is lost | B must remain retained so later reconciliation can complete cleanup |

If the pre-save lease check fails, the current finish path aborts the scan,
saves the semantic tree without a reusable AWACS baseline, and returns an
error asking for a rebuild. If journal saving fails, it aborts the pending
scan and reports the save error. A failed scan must not advance a usable
baseline.

Two current paths need further work to satisfy the recovery table. AWACS
session expiry removes its pending pin, even if JJ saved B before processing
that expiry. JJ stops renewal before saving, so a slow save or process pause
can cross the deadline. A later finish call can then discard the pin for the
saved endpoint. Also, journal saving can report an error after replacing the file.
Aborting B in response can leave the new record without its required pin.
These risks follow from the current call order and have not been reproduced
by this documentation review. Recovery must retain or reconcile the possible
B record before releasing its endpoint. Fault injection must cover both cases.

The owner ID is independent of the root path, workspace name, and mutable `@`
commit. AWACS uses it to associate committed and pending pins. The snapshot
UUID and continuity token identify the endpoint. A temporary scan lease alone
is insufficient because B must survive between commands.

## Working-copy mutations

Checkout, reset, recovery, sparse changes, and colocated Git transitions can
change files or the tree that JJ believes those files represent. They must
enter a recorded transition before leaving an unproved tree/baseline pair.

Before changing live files, JJ aborts any earlier pending scan and saves
`PendingMaterialization`. It preserves the prior tree and baseline A, any
existing pending scan scope, the intended tree Z, the mutation kind, and any
intended sparse patterns. Preparing a checkout before the repository operation
and executing it afterward must retain the same A across both steps.

### Successful materialization

JJ writes the intended files to the live root. It then asks AWACS for B and
the complete A→B delta. Other writers may have changed unrelated files during
checkout. JJ cannot assume that every delta path came from its materializer.

The materialization path therefore saves Z and B with all A→B changes marked
as pending scan work. It also carries forward older pending work. This step
does not need to traverse the changed files or interpret them using another
command's snapshot options.

The next semantic snapshot takes C and scans the following scope.

~~~text
saved pending A→B paths
union any earlier pending paths
union new B→C delta
union remembered untracked candidates
union explicit tracking requests
~~~

It reads those paths from C even if B→C is empty. Repeated materializations
accumulate pending work. Only a successful semantic scan clears it. Full or
root-wide invalidation after materialization fails before a replacement
baseline is saved.

A baseline with pending work can support another incremental scan. It cannot
seed a new workspace as if its tree already described all of its files.
Snapshot workspace creation rejects it until a semantic scan such as
`jj status` reconciles the source. JJ also withholds the Git-mediated adoption
seed while work remains. Adoption rejects copied journals with pending paths
or an incomplete untracked record.

### Interrupted materialization

For a strict snapshot-mode transition with a valid retained A, loading keeps
the prior serialized tree and pending scan work. It does not trust the intended
Z as the actual result of an interrupted checkout. A can drive an A→current
immutable recovery scan. It cannot be saved as a clean replacement before
that scan commits its result.

An interrupted sparse update is different. A missing file could be a deliberate
user deletion, a file removed during sparse contraction, or a file not yet
written during sparse expansion. The journal does not resolve that ambiguity.
JJ must stop before snapshotting, rebuilding, or changing files. It must
preserve the working-copy files and journal for manual reconciliation.

When a non-strict transition has no retained A, recovery can keep the recorded
intended tree and sparse view but clears the baseline. A later full live scan
must reconcile the actual files. Strict subvolume mode does not silently use
that fallback.

A Git HEAD import, export failure, or semantic reset that cannot prove matching
files must either preserve enough pending work for reconciliation or invalidate
the baseline. A monitor clock cannot make that mismatch safe. Recovery must
happen before another operation treats the state as a clean snapshot.

## Migration and cleanup

The current journal writer no longer writes the full per-path `tree_state`
payload for snapshot-mode working copies. It saves the combined record first
and then removes the old payload. The reader imports legacy semantic metadata,
including tree IDs, conflict labels, sparse state, and compatible cursor data.
A legacy Watchman cursor alone cannot establish an AWACS baseline.

The existing legacy reader decodes the old protobuf and discards its file-state
vector. Avoiding allocation of those rows during migration remains work to do.
A metadata-only decoder should skip the old row field while preserving the
semantic fields. The one-time migration cost must not be confused with the
steady-state journal cost.

An older exact AWACS baseline can be reused only if the backend verifies its
identity, continuity, and retention. The direct backend can adopt an existing
valid endpoint that predates durable consumer pins. Otherwise migration needs
an explicit baseline scan. Versions without a complete untracked record also
need that scan even if the old endpoint still exists.

Intermediate SQLite working-copy markers must be rejected without treating
them as empty state or opening an unknown database as the current journal.
Whether those workspaces need a dedicated recovery tool remains an open
question. A compatible older binary may be needed to recover their data.

Removing legacy `FileStatesMap`, `FileStates`, `TreeStateDelta`, and protobuf
row types is a separate cleanup decision for their remaining users. Snapshot
mode must not recreate those structures for ordinary status. Removing JJ's
working-copy SQLite store does not remove AWACS's manager database, which
serves backend coordination and replay retention.

## Alternatives considered

Streaming or compressing the full protobuf can reduce allocation or bytes.
It still reads and writes state for all N paths. An immutable full blob or
copy-on-write range chunks also retain a per-path index and add compaction,
balancing, recovery, and cleanup work.

An append-only per-path delta journal makes small writes cheap, but loading
and compaction must still account for its accumulated history. The snapshot
baseline already supplies the physical history required by this design.
The small transition journal should not become another stat database.

Using a snapshot UUID as a tree ID would lose JJ's content normalization,
executable policy, tracking decisions, conflict representation, copy IDs,
and supported submodule semantics. The journal must connect physical identity
to explicit semantic tree IDs.

## Validation and remaining work

These checks exercise the version-4 journal, path selection, and recovery
after an interrupted scan or checkout.

### Correctness

- Compare edits, additions, deletions, type changes, symlinks, executable bits,
  supported submodules, conflicts, renames, and hardlinks with a reference scan
  that uses the same tracked membership and interpretation rules.
- For prefixes, find both new B paths and deleted X paths without visiting
  unrelated subtrees.
- Change semantic inputs and require explicit recovery. Change ignore and
  auto-tracking policy and verify the intended behavior for untouched paths,
  changed paths, remembered candidates, and explicit tracking requests.
- During a complete one-path transition with no deferred or untracked work,
  avoid reading, hashing, decoding stat rows for, or sorting all unchanged
  tracked paths.
- Report omitted untracked paths again when required. Cover a skipped file
  becoming a directory, ignored or sparse-hidden candidates, inaccessible
  paths, and symlink ancestors.
- Keep the ordinary mutable backends correct under their own scan model.

### Retention and crash recovery

- Retain A while reading B and until the replacement journal is durable.
- Inject failures around staging B, checking the lease, writing and syncing the
  journal, replacing it, and completing the backend handoff.
- Test repeated recovery after lost responses. Each result must have usable A,
  usable B, or an explicit repair requirement.
- Preserve an unrelated writer's changes during checkout and carry them into
  the next scan even when its new AWACS delta is empty.
- Preserve pending work across repeated and interrupted materializations.
- Reject workspace creation and adoption until their source is fully reconciled.
- Stop an interrupted sparse transition before scans or writes and preserve
  both the files and journal.
- Test missing endpoints, replay gaps, malformed deltas, namespace changes,
  and unsupported topology without silently falling back to a full scan.

### Migration and downgrade

- Read versions 2, 3, and 4 without losing pending work. Require a rebuild when
  the untracked completeness flag is absent.
- Reject unsupported versions and malformed journal paths.
- Verify that older binaries reject the magic-prefixed checkout instead of
  treating a missing `tree_state` as an empty working copy.
- Reject intermediate SQLite markers without opening them as current state.
- Add a metadata-only legacy decoder and verify that it does not allocate
  the old file-state rows.
- Verify that snapshot-mode commands do not create or update JJ's former
  SQLite file-state store.

### Performance

Measure unchanged status, one-file edits and deletions, renames, changed
prefixes, 10,000 changed paths, and large omitted-file sets. Also measure
missing-baseline failure, explicit rebuild, checkout followed by reconciliation,
and startup from a legacy record.

Report end-to-end latency, replay work, paths read, journal bytes, pending and
untracked counts, and retained snapshot count. On a million-path working copy,
a one-file steady-state scan with small pending and untracked sets must avoid
loading a million stat rows or traversing the full filesystem. This requirement
still needs measurements of the current implementation.

## Open questions and future possibilities

- How should a very large omitted-file set be represented without restoring
  O(N) startup work? Any limit must preserve repeated untracked reporting.
- What tool should reconcile an interrupted sparse update without confusing
  partial materialization with user changes?
- Is a dedicated recovery tool needed for intermediate SQLite workspaces?
- Which additional fault-injection tests are needed around an uncertain journal
  save result and backend pin cleanup?
- Can other immutable or virtual backends implement the same retained-baseline
  contract?

JJ already exposes journal phase, generation, baseline identity, and retention
information through its working-copy status interfaces. Further diagnostics
should make pending scope, untracked completeness, and the reason for refusing
an incremental scan easy to inspect. Directory-descriptor traversal remains
a useful improvement shared with the direct-scan RFC.
