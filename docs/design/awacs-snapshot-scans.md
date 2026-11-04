# Direct AWACS snapshot scans

AWACS gives Jujutsu an immutable filesystem snapshot and a complete list of
paths that may have changed since its last scan. JJ reads affected paths from
that snapshot and updates its saved tree. A file created while JJ is scanning
belongs to the next snapshot.

This draft describes the direct AWACS client on `dev/adamh/awacs-snapshots`.
The recovery section identifies two ways JJ can save a reference to a snapshot
and then lose the pin that keeps it available. The companion RFC explains what
JJ writes to disk. [Working-copy journal](incremental-working-copy-tree-state.md) ·
[AWACS implementation specification](../../btrfs-awacs/SPEC.md)

## Why the scan needs an immutable root

A monitor response and a later read of live files can describe different
states. Consider an endpoint comparison that reports changes from B to C.

1. JJ receives the monitor boundary B.
2. A path appears in the live working copy.
3. JJ reads it and includes it in its tree.
4. The path disappears before boundary C.
5. B and C both lack the path, so their comparison need not report it.

JJ has saved a state that neither endpoint contains. Reading immutable B
prevents this race. JJ cannot see a file created after B. A change that is
made and then undone between B and C does not need to appear in their delta.

Watchman supplies a clock and changed names. Its standard protocol does not
supply an immutable read root, a scan lease, or an acknowledgment that JJ has
saved the result. Those extra requirements belong in a separate AWACS API.
Watchman remains useful for mutable working copies on other filesystems.

## State of the feature

The public `fsmonitor.backend` values are `none` and `watchman`. Subvolume mode
selects `FsmonitorSettings::Awacs(AwacsConfig)` internally. There is no public
`fsmonitor.backend = awacs` setting or daemon socket to configure.

The embedded AWACS library opens the initialized state for the live root.
Short-lived callers coordinate through `root.lock` and the durable manager
state. A privileged broker performs the allowed Btrfs operations. JJ does not
manage broker requests, snapshot names, or replay files itself.

An ordinary command in subvolume mode must advance from a valid baseline
without traversing the whole working copy. If it cannot, the command stops.
Initialization and `jj util subvolume enable --rebuild-baseline` may establish
a baseline with a full scan of an immutable snapshot. Errors in setup,
authorization, or filesystem support can require a different repair.

AWACS is optional and Linux-only. Ordinary JJ installations do not require
Btrfs, the AWACS broker, or its kernel support. In the current snapshot branch,
`none` and ordinary Watchman scan the full live root because that branch does
not use a persistent per-path cache for those scans. They do not establish an
AWACS baseline or make concurrent live reads consistent.

## Filesystem scope

Checkouts, sparse updates, and `jj file untrack` write to the live working
copy. The snapshot supplies a read-only source for scans. The integration
requires a supported Btrfs layout. It rejects nested subvolumes, descendant
mounts, fscrypt, and roots that cross filesystems.

## Correctness model

A normal semantic scan has this shape.

~~~text
live working copy        W
saved JJ tree            X
retained baseline        A
new immutable snapshot   B
complete invalidation    D(A, B)
new JJ tree              Y = update X with affected reads from B
saved baseline           B
~~~

The delta can contain extra paths. It must not omit a path whose changed
filesystem state can affect JJ's result. JJ uses its semantic tree to find
tracked paths and deletions. It does not need saved mtime or size rows for
paths unchanged since A.

Every read of a working-copy path that contributes a new value to Y must use B.
This includes
metadata, file contents, symlink targets, worktree ignore files, and files used
for content conversion. Values inherited from X rely on the complete delta
and the interpretation rules recorded with A.

The saved tree and physical baseline can temporarily differ after checkout.
JJ records every path that still needs interpretation as pending scan work.
The next scan includes that work even when AWACS reports an empty new delta.
The journal RFC specifies this case. AWACS must retain the named physical
baseline whether or not JJ has finished interpreting every affected path.

A scan may advance the baseline only under these rules.

1. JJ accepts a delta only for its exact saved A and the returned B.
2. AWACS proves the root binding and snapshot identities. It does not trust
   UUIDs or pathname identity supplied by the caller without checking them.
3. Every working-copy path read by the scan uses B. The live root remains
   the destination for state updates and working-copy writes.
4. A session failure cannot turn B into a usable saved baseline.
5. AWACS keeps A until JJ's replacement record is durable. It keeps B after
   that record is durable, even when the scan session ends.
6. JJ records pending work or invalidates the baseline when a command changes
   its tree without proving that the files match that tree.
7. A root-wide invalidation, missing replay interval, or unsupported topology
   cannot silently become an incremental scan.

`jj status` reports the state at B under these rules. Writes after B can appear
in the next command. They do not change the files read by the current scan.

## Library interface

The Rust API lives in `btrfs-awacs/src/scan.rs`. Its principal values are shown
below. This is a summary of the types, with ownership details omitted.

~~~text
BeginScanRequest {
  live_root: PathBuf,
  baseline_owner_id: [u8; 16],
  previous_baseline: Option<SnapshotBaseline>,
  allow_full_invalidation: bool,
}

SnapshotBaseline {
  identity: SnapshotIdentity,
  continuity_token: Vec<u8>,
  retention_token: Vec<u8>,
}

SnapshotIdentity {
  filesystem_uuid: [u8; 16],
  subvolume_uuid: [u8; 16],
  read_only: bool,
}

SnapshotLease {
  next_baseline: SnapshotBaseline,
  invalidation: Full | Paths { exact: Vec<Vec<u8>>, prefixes: Vec<Vec<u8>> },
  expires_boottime_ns: u64,
  scan_root: File,
  session: ScanSession,
}
~~~

`ScanClient::begin_scan()` returns the lease. The directory descriptor stays
owned by that lease and is exposed through `scan_root()`. The caller must
validate that it names a directory on the expected filesystem and subvolume,
and that the subvolume is read-only. `ScanClient::validate_scan_root()` supplies
this check.

The continuity and retention tokens are opaque to JJ. The filesystem and
subvolume UUIDs identify the physical snapshot. The stable owner ID identifies
whose durable baseline AWACS retains. Neither a mutable `@` commit nor a
workspace pathname is a suitable owner ID.

### Opened roots and path binding

AWACS opens the requested live subvolume for each request. It records its
filesystem and subvolume UUIDs, device and inode identity, and mount identity.
It also checks the process root and mount namespace and rejects descendant
mounts. Snapshot creation uses that opened source descriptor. Reopening a
pathname after validating it could select a different root after a rename or
mount change.

The returned B descriptor is opened and checked before AWACS releases its root
lock. Snapshot content must remain private and read-only during the lease. A
caller-controlled pathname alone does not provide this guarantee.

JJ's scanner still accepts paths. It currently reads through
`/proc/self/fd/N` while holding the descriptor for the whole lease. JJ must not
canonicalize that path into a snapshot pathname. AWACS already uses opened
roots for its own snapshot and namespace work. Passing directory descriptors
through all of JJ's traversal remains a future change.

### Complete invalidation

A single scan can contain both exact paths and recursive prefixes. JJ validates
each repository-relative path before using it. When AWACS reports a move by
its two roots, JJ checks the old and new subtrees. The parent directories also
have changed entries. Their exact paths let JJ check the parents without
reading files in sibling subtrees.

The invalidation must cover all physical differences relevant to JJ.

- File creation, deletion, bytes, size, and executable bits.
- Symlink creation, deletion, and target changes.
- File-to-directory and directory-to-file changes.
- Renames, hardlink aliases, and changed directory references.
- Directory entries needed to find additions and removals.
- Worktree ignore files and other files used to interpret content.
- Git-submodule entries where the working-copy backend supports them.

JJ applies its own sparse, tracking, ignore, and content-conversion rules to
that physical scope. An ignore-file change does not by itself mean every
unchanged descendant must be scanned. The current classification rule and
external-input fingerprint are defined in the journal RFC.

AWACS returns `Full` only when the caller allows full invalidation. Ordinary
subvolume commands set `allow_full_invalidation = false`. Missing history,
overflow, unsupported topology, or inability to prove a complete delta then
causes an error. JJ also rejects a scope that expands to the whole root.
This includes a root prefix even if the response did not use `Full`.

Allowing `Full` does not authorize JJ to trust a malformed or unrelated previous
baseline. An explicit rebuild must establish suitable state before scanning.

### Lease renewal

`SnapshotLease::renew()` extends the active scan. JJ renews while it reads B
and while it waits to save the working-copy state. The lease reports its
deadline using boot time. The renewal interval is one third of the remaining
lease time, clamped between 1 nanosecond and 60 seconds.

Before saving, JJ stops the renewal worker, checks whether it failed, and
calls `promote()` to check the active session. The current promotion does not
extend the deadline. Renewal failure prevents a new usable baseline. It does
not permit JJ to continue from an older cursor with the new tree.

### Retaining and finishing a baseline

The direct client stages a durable pending pin for B during `begin_scan()`.
It keeps the committed pin for A. This is separate from the temporary scan
lease that protects the open read root and active comparison.

`SnapshotLease::promote()` is the pre-save check in the API. In the current
direct implementation it checks that the session is still active. It does
not create the pending pin because Begin already created it. JJ checks the
renewal result and promotes before it writes the replacement journal.

`SnapshotLease::finish(Committed)` means JJ durably saved a record naming B.
That record can include pending scan paths after materialization. AWACS then
makes the pending B pin committed and releases A's committed pin in one
manager transaction. Ending the temporary lease must not release B's durable
baseline pin.

`SnapshotLease::finish(Aborted)` means JJ did not save a record for B. AWACS
removes the pending pin and keeps A. The lease API permits repeated completion
with the same outcome and rejects a different outcome on the same lease.
If the process exits or loses a response, the next Begin reconciles durable
pins with the baseline in JJ's journal. This recovery requires the pending pin
to survive until that reconciliation.

| Journal at the next Begin | Recovery action |
| --- | --- |
| Names committed A | Keep A and discard pending B |
| Names pending B | Make B committed and release A |
| Names no matching retained baseline | Reject incremental use |

`ScanClient::release_baseline(owner_id)` removes that owner's durable pins.
Mode changes and workspace cleanup use this operation. Read/cut permission
must only permit retaining the authorized A and the candidate selected by
Begin. It must not permit choosing or retaining arbitrary snapshots.

### Replay storage

The current coordinator derives changed paths from immutable endpoints through
the broker. It does not keep a full persistent inode-to-path graph or a chain
of immutable path-map overlays. For each adjacent cut, it resolves the changed
objects at both endpoints and produces conservative path events.

A private spool holds the broker-verified changed-object data and the derived
replay events. AWACS hashes and syncs the payload before committing its replay
reference. The manager transaction records the cut sequence, endpoint IDs,
spool pathname, payload hash, and event count and advances the indexed head.

To answer A→D, AWACS reads and unions the retained events for each adjacent
cut after A through D. It need not reopen every intermediate snapshot. A
missing spool, hash failure, sequence gap, or cursor below the replay floor
prevents incremental use.

Retention protects the complete replay interval needed by each durable
consumer. JJ's physical endpoint pins and active scan leases have separate
lifetimes. Git also uses AWACS, but its hook response does not prove that Git
saved a cursor. Its index and in-flight response retention rules are described
in the implementation specification and must not be inferred from JJ's
explicit `Committed` acknowledgment.

Cleanup removes catalog references transactionally, then unlinks unreferenced
spools and eligible snapshots. A crash can leave extra files for later cleanup.
It must not leave committed replay metadata pointing to a payload that was
never made durable.

### Errors

`ScanErrorKind` distinguishes `UnsupportedFilesystem`, `Unavailable`,
`InvalidPreviousBaseline`, `FullScanRequired`, `LeaseExpired`, `Unauthorized`,
`MalformedResponse`, and `Other`. Diagnostics must also explain incompatible
library or stored-state versions.

Before a lease is accepted, an error leaves the saved baseline unchanged.
After acceptance, JJ aborts the scan if it cannot read or safely save the
result. If the pre-save lease check fails, the current JJ finish path may save
the semantic tree without an AWACS baseline, then return an error. The next
ordinary subvolume command cannot reuse that state incrementally.

The required behavior after a durable journal save is to treat failed
completion as cleanup. JJ currently warns and lets a later Begin reconcile
the pins. That is safe only if B remains retained. The expiry path described
below still needs to meet this requirement.

A future option could explicitly permit a full live scan after an AWACS
failure. It would need to clear the baseline and warn that the result did not
come from one immutable snapshot. No such fallback is part of the ordinary
subvolume contract.

## Jujutsu integration

`SnapshotScan` separates the read root from the selected scope. It contains
`scan_root`, `scope`, `fsmonitor_cursor`, `baseline`, `completion`, and
`warning`. `ScanScope` is either `Full` or `Changed { exact, prefixes }`.
`PendingScan` keeps the AWACS session alive until
`LockedLocalWorkingCopy::finish()`.

JJ combines the new AWACS delta with pending materialization paths, remembered
untracked paths, and any explicit tracking request. It then intersects that
scope with the applicable sparse view. It reads new values from B and applies
them to X. The journal RFC explains why each extra set is necessary.

Every worktree read that can affect the tree must use `scan_root`. This includes
`DirectoryToVisit`, tracked-file lookup, recursion, ignore files inside the
worktree, symlink reads, metadata, EOL conversion, and new-file size checks.

The live `working_copy_path` remains the location for the journal, working-copy
lock, checkout and sparse writes, reset and recovery, colocated Git metadata,
and the workspace path shown to the user. External configuration is read
separately and checked under the journal's interpretation rules.

The snapshot method builds the candidate tree in memory. The finish method
saves the tree, operation identity, and baseline together. A command that
writes files between those calls must enter the materialization state machine
instead of saving the earlier scan as if it still described the live result.

## Implementation map

| Component | Responsibility |
| --- | --- |
| `btrfs-awacs/src/scan.rs` | Client types, lease ownership, root validation, and errors |
| `btrfs-awacs/src/scan_facade.rs` | Direct coordination, Begin, renew, promote, and finish |
| `btrfs-awacs/src/namespace.rs` | Opened source root and process/mount binding |
| `btrfs-awacs/src/service.rs`, `manager.rs` | Broker comparison, replay durability, pins, and cleanup |
| `lib/src/fsmonitor.rs` | Internal AWACS configuration and interpretation fingerprint |
| `lib/src/local_working_copy.rs` | Read-root selection, scan scope, tree updates, and pending leases |
| `lib/src/protos/local_working_copy.proto` | Typed baseline and versioned journal |
| `lib/src/working_copy.rs` | Snapshot, finish, and mutation contracts |
| `cli/src/cli_util.rs` | Snapshot-before-command flow and diagnostics |
| `cli/src/config-schema.json`, `docs/config.md` | Public configuration values |

## Validation and remaining work

The direct client, durable pins, compact journal, and scan-root selection exist.
Validation must cover their interaction. A fake client alone cannot establish
that the Btrfs broker and kernel produce complete deltas.

### Recovery cases still requiring resolution

The current `expire_sessions()` path calls
`finish_consumer_baseline(owner, false)` for an expired JJ session. That removes
its pending B pin. JJ stops renewal before saving, so a slow save or process
pause can run past the deadline. If the local B record became durable before
expiry was processed, a later finish on that same handler can discard the pin
needed by the record. AWACS must check which snapshot the saved journal names
before it releases that candidate.

JJ also aborts the scan when journal saving reports an error. Saving can fail
after the journal rename, such as during later directory syncing or cleanup.
At that point the new record may name B. Recovery must resolve that possibility
before an abort removes its last durable pin.

These two cases follow from the order of calls in the source. They still need
fault-injection tests. After a failure, JJ must either retain the snapshot
named by its journal or stop and require repair.

### Unit tests

- Give the live root and scan root different content. Check metadata, file
  bytes, `.gitignore`, ignored-directory handling, symlinks, executable bits,
  EOL conversion, deletion detection, and large-file checks against B.
- Keep journal and lock writes under the live state path.
- Restrict an exact-path scan to its selected paths and a prefix scan to that
  prefix and the corresponding semantic subtree.
- Reject malformed paths, mismatched UUIDs, writable roots, `Full`, and root
  prefixes in ordinary subvolume scans.
- Fail Begin, traversal, renewal, promotion, and journal saving independently.
  Each result must retain usable prior state or require explicit recovery.
- Fail completion after a durable save. Reconcile B on the next Begin.
- Preserve pending materialization work through reload and repeated checkouts.
  Reconcile it even when the next delta is empty.
- Reject workspace and adoption seeds while pending work remains.
- Preserve legacy Watchman cursor decoding without treating it as an immutable
  baseline.

### Integration tests

Use a real Btrfs subvolume, the direct coordinator, and the matching broker.
The environment-gated harness must make these dependencies explicit.

- Compare edits, additions, deletions, type changes, renames, hardlinks,
  symlinks, executable bits, supported submodules, and conflicts with reference
  scans that use the same JJ interpretation rules.
- Change the live root after Begin and verify that traversal still reads B.
- Create and remove a file, or modify and restore one, between B and C.
  Verify that no intermediate state enters JJ's saved tree.
- Interrupt the caller before and after the local journal save. Verify the
  pin reconciliation table above.
- Remove required history or expire an active lease. Require an error before
  a new baseline can be reused.
- Rename or replace the live root and change mount topology between cuts.
  Verify that the opened-root and namespace checks reject invalid continuity.
- Exercise colocated Git transitions without treating a Git response as proof
  that its index was saved.

### Performance tests

Measure unchanged status, one-file status, large untracked sets, changed
prefixes, explicit rebuilds, lease latency, and retained snapshot growth.
Report replay work, JJ filesystem work, and journal size separately. Compare
with `none` and Watchman. One-file status should avoid loading and sorting a
stat row for every tracked path. The journal RFC states the remaining costs
for deferred and untracked paths.

## Alternatives considered

Extending Watchman would still require a separate snapshot locator, lease,
and completion mechanism. Directory-prefix conventions from Git's fsmonitor
hook are not a portable Watchman subtree contract. Reporting
`is_fresh_instance` is safe, but requires a full scan.

A launcher could bind-mount B over the live root in a private mount namespace.
Commands that also write files would then need another way to select their
write root. An explicit read root keeps that choice inside JJ's working-copy
implementation.

Continuing to scan live files preserves the existing mutable-filesystem model.
It cannot make a monitor boundary and later reads describe one immutable state.

## Future possibilities

- Pass directory descriptors through all JJ traversal and use `openat()`.
- Add direct AWACS triggers for background `jj util snapshot` without copying
  Watchman's trigger configuration into this API.
- Let another immutable or virtual working-copy backend implement the same
  delta and retention contract.
- Let a virtual backend supply content without a local snapshot directory.
