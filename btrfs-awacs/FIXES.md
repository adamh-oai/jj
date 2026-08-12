# AWACS and Jujutsu remediation tracker

This is the current prioritized remediation plan for the direct AWACS
immutable-snapshot backend and its companion checkout at `../jj`. The
authoritative architecture and support boundary are in [`SPEC.md`](SPEC.md).
The [documentation site](docs/) organizes the same lifecycle by component.

The active findings below retain their original stable IDs. IDs that described
removed compatibility paths are intentionally absent rather than reused.
Range headings are tracking umbrellas only: each numbered bullet under one is
an independent finding and fixing one does not close its siblings. Where two
findings touch the same code path, an explicit overlap note names the invariant
owned by each one.

## Scope and direct client contract

The supported integration boundary is the direct Jujutsu scan:

- Jujutsu receives an authenticated cursor, conservative invalidation,
  revocable lease, and read-only snapshot directory descriptor.
- Jujutsu reads exactly that immutable snapshot through `/proc/self/fd/N`.
- Cursor persistence is valid only with the tree state and external-input
  fingerprint derived from the same immutable scan.
- Any unproved descriptor, cursor, path, continuity, or lease condition must
  abort or conservatively force a full traversal.

Descriptor validation, session renewal, external-input fingerprints, and
transactional Finish are correctness requirements, not optional performance
features.

## P0: release blockers and silent data loss

### C-01: Removing the primary workspace can destroy every workspace

**Status:** Implemented in the current remediation change; keep open until the
acceptance cases below pass in the supported checkout matrix.

The earlier implementation rejected only the currently selected workspace
name. From a secondary workspace, removing the primary could delete the shared
`.jj/repo` and colocated Git object database used by every surviving workspace.
The current change preflights from a no-snapshot view, rejects shared
Jujutsu/Git storage and survivor ancestry/aliases, then holds a repository-wide
workspace lifecycle lock while it repeats that proof, deletes the target,
forgets its registration, and publishes the repo transaction. Add, rename, and
forget take the same lock across their full lifecycle transitions.
Registration and operation history are committed only after deletion succeeds.
C-19 still owns a pathname replacement after the final identity check; this
C-01 remediation does not claim descriptor-relative deletion.

**Remediate:** Resolve the target identity and every shared operation/object/Git
store before deletion. Refuse a target containing storage required by a
surviving workspace; protect against ancestry and aliases; serialize the
registered workspace topology across deletion and publication. C-19 separately
owns an after-validation pathname replacement race.

**Accept when:** Removing a shared-store owner fails without changing
registrations, history, operations, filesystem contents, or Git objects.

**Overlap:** C-01 owns deletion of a path that contains shared repository or
Git storage. C-18 through C-20 cover distinct dirty-target, path-replacement,
and failed-deletion ordering hazards.

### C-02: Optional snapshot fallback fabricates a populated baseline

**Status:** Implemented in the current remediation change; keep open until the
fallback matrix below passes on supported non-Btrfs and cross-filesystem hosts.

The earlier `../jj/cli/src/commands/workspace/add.rs` path could retain a
snapshot-only source baseline after optional Btrfs snapshot creation fell back
to an ordinary empty directory. The current change records that inherited
baseline only after a verified physical snapshot succeeds. Required snapshot
mode still fails creation; auto fallback follows ordinary checkout and
materialization.

**Remediate:** Associate the inherited baseline only with a verified physical
snapshot. When `btrfs.enabled = true` or `--btrfs-snapshot=true` requires a
snapshot, any snapshot failure must fail workspace creation. Only auto mode may
fall back, and that fallback must use stock workspace initialization and
materialize the desired checkout before recording a cursor.

**Accept when:** Nonempty sources survive auto fallback on non-Btrfs,
cross-filesystem, absent-tooling, and existing-empty-destination cases.

**Overlap:** C-02 owns the unsafe baseline after fallback. C-28 and C-29 own
whether auto mode reaches the supported fallback path at all.

### C-03 (retired as P0): Standalone companion packaging needs a declared topology

The original “nonexistent sibling” claim is stale in the supplied layout:
`../jj/Cargo.toml` points at `../bsend-watch`, that sibling exists, and
`cargo metadata --no-deps --format-version 1` resolves here. The remaining
concern is distribution topology: a Jujutsu-only clean checkout without that
sibling still cannot resolve an optional path dependency during Cargo metadata,
even when AWACS is disabled.

**Decide/remediate:** Declare sibling checkouts as the supported development
and release topology and make setup explicit, or use an independently
resolvable dependency/overlay while preserving feature-disabled and non-Linux
builds.

**Accept when:** Metadata, ordinary builds, and supported Linux
`--features awacs` builds resolve from every topology the project claims to
support. Do not count this as a current P0 unless standalone Jujutsu checkouts
are part of that claim.

### C-05: Invalid immutable targets can permanently wedge a watch

**Status:** Implemented in the current remediation change; keep open until the
kernel-backed injected-invalid-target acceptance cases below pass.

The earlier `src/service.rs` path called `publish_validated_physical_cut`
before nested-subvolume, dirty-witness/manifest, fscrypt, and target-object
checks completed. The current change keeps a newly recorded target staged
while it validates endpoint identity, legacy and v2 nested-boundary/fscrypt
policy, target objects, and adjacent-manifest applicability. Only a validated
target advances the physical head. Deterministic invalid targets are recorded
as failed gaps, release the cut lease/admissions/pins, and leave the old
physical/indexed heads serviceable; retryable broker/spool failures leave the
unpublished operation available for recovery.

**Remediate:** Separate deterministic policy rejection from transient
I/O/spool/comparison failures. Perform rejection-sensitive validation before
publishing the physical head, or define one fenced atomic terminal-failure or
quarantine transition that releases admissions and pins and leaves future
clients conservative rather than wedged.

**Accept when:** Injected invalid nested-boundary, fscrypt, identity, and
manifest targets leave no stuck heads, operations, admissions, pins, or
permanently unserviceable direct clients after restart; transient failures
remain retryable.

**Overlap:** C-23 owns proving that a v2 stream describes the admitted
endpoints. C-05 owns when that proof is required relative to physical-head
publication and what durable state follows rejection.

### P-01: Production has no snapshot or history garbage collection

**Status:** Production wiring is implemented in the current retention change;
keep open until the kernel-backed recovery/latency acceptance matrix passes.

The scan daemon now starts a named periodic maintenance worker on its own
store/broker handle, so filesystem deletion never holds the request-facade
mutex. Each tick expires bounded query, retention, and historical-comparison
leases, processes a round-robin bounded watch slice, applies the configured
replay windows, reclaims bounded orphan work independently of watch length,
and drives bounded one-at-a-time snapshot deletion through the existing
durable broker fences with live reconciliation of post-effect rows. The tick
reports elapsed time, expired leases, watches, reclaimed history rows, deleted
snapshots, and whether more work remains.

**Remediate:** Keep the bounded worker observable and verify that maintenance
honors heads, surviving boundaries, leases, grants, pins, operations, and
broker fences under restart and sustained load.

**Dependency:** P-01 acceptance remains gated on C-06's crash/restart
acceptance.

### P-02: Each successful direct scan or GC delete can flush unrelated writes

`src/broker.rs` calls `syncfs` after snapshot creation and deletion, waiting
for unrelated writes on the same filesystem. This is on successful direct
AWACS cut and snapshot-GC paths, not every ordinary Jujutsu status.

**Remediate:** Prove creation ordering and crash-durable deletion receipts
separately, use the narrowest primitive each requires, and measure the cost
under unrelated write pressure.

## P1: correctness, isolation, and lifecycle defects

### C-06: History compaction violates retained-boundary foreign keys

**Status:** Implemented in the current retention change; keep open until the
crash/restart acceptance matrix passes.

The earlier retention path committed boundary removal separately, wrote an
unsupported `replay-boundary` pin kind, then deleted every older `watch_cuts`
row even when a retained `fsmonitor_boundaries` row still named it through the
composite foreign key. The current path treats surviving boundaries as the
ownership authority, expires stale query pins, re-reads active source/target
endpoints under the same writer transaction, and deletes only bounded
boundary/cut/operation groups that have no surviving boundary. Comparison and
revision orphan cleanup run as separate bounded work after that atomic
boundary transaction, so a crash can leak rows but cannot leave an invalid
replay set.

**Remediate:** Make retained boundary ownership and deletion order
foreign-key-safe and transactionally atomic: co-retain the parent cut rows or
change the ownership model, and never delete an active query's exact boundary.

**Accept when:** Retention with non-newest retained and active-query
boundaries succeeds without foreign-key errors or partial state, and a crash at
any retention step preserves a valid replay set.

**Dependency:** This is the correctness prerequisite for P-01 production
maintenance.

### C-07: External ignore inputs can be read twice

The companion Jujutsu checkout can read the same ignore inputs once for a
fingerprint and again for traversal, pairing a tree with a different input
bundle after a concurrent edit.

**Remediate:** Build one immutable external-input bundle and use it for both
fingerprinting and traversal.

### C-08: Relative global-ignore precedence can be reversed

The companion checkout can apply relative global excludes in an order that
differs from stock Jujutsu, changing which files are visible even with AWACS
disabled.

**Remediate:** Preserve stock global and repository ignore precedence.

### C-09: Worktree-relative excludes are omitted from secondary scan paths

`jj run` and external diff-edit paths can omit worktree-relative global
excludes, so those paths observe a different tree from ordinary status.

**Remediate:** Route ordinary scans, direct scans, `jj run`, and external
diff editing through the same immutable input bundle.

**Shared acceptance for C-07 through C-09:** `none` and AWACS match stock
behavior for global excludes, repository excludes, `jj run`, external diff
editing, and concurrent ignore changes.

### C-11: Server lease expiry and advertised client deadline disagree

The old “before an expensive cut” description is stale: Begin now renews the
durable query lease after cut preparation. The remaining mismatch is that it
stores a Unix-time expiry from one sample and then independently advertises
`boottime_now + ttl` from a later sample. Response construction, delivery, or
clock-domain skew can make the client believe a lease is live after the
durable server fence has expired.

**Remediate:** Derive the advertised boot-time deadline from the remaining
duration of the already-committed durable expiry, with an explicit safety
margin for response delivery; never advertise a deadline later than the
server's durable lease.

**Accept when:** Delayed cut preparation, delayed response delivery, and renew
tests never let the client scan or commit after the server considers the lease
expired.

**Overlap:** C-11 is deadline conversion/advertisement correctness even with
no contention. C-16 is lock scheduling that can prevent timely Renew or
Finish.

### C-12: A connected descriptor can carry the wrong namespace authority

`src/main.rs` authenticates the original socket connector rather than each
later sender. An inherited or transferred descriptor can be reused by a
same-UID process in another mount namespace or chroot and receive a private
snapshot fd.

**Remediate:** Prove per-request sender and namespace/root authority with a
nondelegable transport or verifiable credentials/process handles.

### C-14: The optional precision journal is not used by direct invalidation

`src/facade.rs` certifies and pins precision cursors but projects direct
historical changes without the lease-aware precision range projector.

**Remediate:** Use only complete, contiguous, epoch-matched precision intervals
to narrow direct invalidation; otherwise return conservative prefixes or Full.

### C-16: One slow direct Begin can expire unrelated active scans

`src/scan.rs` and `src/scan_facade.rs` hold global/shared locks across expensive
cut work, delaying Renew and Finish until active leases may already have
expired.

**Remediate:** Separate admission, cut execution, response writing, renewal,
and cleanup locks; add bounded read/write deadlines.

### C-17: Sparse widening can record missing files as deletions

Sparse widening can record files absent from the old sparse materialization as
deletions instead of materializing the requested destination first.

**Remediate:** Materialize the requested sparse destination before recording a
baseline.

### C-18: Workspace removal can destroy unsnapshotted sibling edits

Removal can delete a target workspace whose working copy contains edits not
represented by its recorded commit.

**Remediate:** Refuse dirty targets unless the user explicitly chooses a
destructive mode with a clear recovery story.

### C-19: Workspace removal can follow a replaced target path

The current lifecycle remediation rejects pre-existing symlink components and
revalidates the registered path identity immediately before deletion, but the
final deletion still names the path. A replacement after that check can cause
deletion of a different directory.

**Remediate:** Atomically claim the verified target before deletion, then
delete only the claimed object: use descriptor-relative traversal for ordinary
directories and Btrfs deletion by verified subvolume ID rather than a mutable
pathname.

### C-20: Workspace removal forgets registration before failed deletion

**Status:** Implemented in the current lifecycle change; keep open until the
supported failure matrix passes.

The earlier path could commit registration/history changes before filesystem
deletion proved it could succeed, leaving an undeleted but forgotten
workspace. The current path prepares the repo edit in memory, deletes first,
then forgets the registration and commits operation history.

**Remediate:** Check deletion capability before committing registration changes,
or make the failure recoverable without losing the registration.

### C-21: Auto snapshot mode fails when `btrfs` tooling is absent

**Status:** Implemented in the current lifecycle change; keep open until the
supported-host acceptance matrix passes.

The earlier auto path could return an error instead of using stock workspace
creation when the optional `btrfs` command was unavailable. The current path
falls back only in auto mode; required `true` mode preserves the error and
fails workspace creation.

**Remediate:** Treat absent optional tooling as a defined auto-fallback case.

### C-22: Snapshot workspace creation can introduce nested subvolumes

Workspace creation can leave nested subvolumes that violate AWACS's
boundary-free source contract.

**Remediate:** Reject or normalize nested boundaries before recording an AWACS
baseline.

**Overlap:** C-01 owns shared-store-owner deletion. C-02 owns baseline
corruption after a fallback. C-17 through C-22 are separate workspace
lifecycle failures and need independent tests.

### C-23: Parsed kernel identities and completion counters are incomplete

**Status:** Implemented in the current immutable-cut change; keep open until
kernel-backed injected-stream acceptance passes.

The earlier v2 parser validated its header and completion footer internally but
dropped that proof before service publication. The current path carries the
header and completion through normal and recovered staged manifests, carries
ioctl byte/record counters through the broker protocol, and compares FSID,
source/target UUIDs, ctransids, root IDs, file length, footer counters, and
ioctl counters before physical or indexed publication. Legacy streams remain
explicitly proof-less instead of inheriting v2 guarantees.

**Remediate:** Carry the v2 endpoint header and completion counters through
normal and recovered manifest parsing; compare FSID, source/target UUID,
ctransid, root ID, reported bytes, and reported records with the
broker-verified endpoints and ioctl result before publication. Reject every
independent inconsistency.

**Accept when:** Injected header, footer, ioctl-count, and recovered-spool
mismatches fail before any physical or indexed publication.

**Overlap:** C-23 defines the stream/endpoints proof. C-05 defines the durable
publication ordering and terminal behavior when that proof rejects a cut.

### C-25: A failed Begin response can leave a snapshot pinned indefinitely

`src/scan_facade.rs` inserts an active session before `src/scan.rs` sends the
Begin response and descriptor. A failed send can retain a pin until a later
request happens to run cleanup.

**Remediate:** Abort allocated sessions on response failure and run independent,
bounded expiry maintenance.

## P1/P2: scaling, deployment, and compatibility follow-up

### P-03: Changed direct scans become full repository crawls

`src/scan_facade.rs` currently rejects ordinary repository-relative paths that
lack a leading slash, turning normal nonempty invalidations into Full.

**Remediate:** Use one raw relative-path contract across index, projection,
transport, and Jujutsu matcher conversion.

### P-04: Published adjacent deltas can be recomputed

The facade can repeat an already-published adjacent comparison instead of
reusing the pinned result.

**Remediate:** Reuse pinned published deltas.

### P-05: Cut admission stops coalescing before expensive work

Manager admission joins only the fleeting planned phase, so authorized callers
stop coalescing through snapshot and index work.

**Remediate:** Keep authorized batches joinable through expensive
snapshot/index phases.

### P-06: Connection and in-flight work need hard bounds

The daemon creates one OS thread per connection and does not bound clients,
workers, packet buffers, queue depth, or in-flight cuts enough for sustained
load.

**Remediate:** Bound connection admission, workers, buffers, queue depth,
in-flight cuts, and read/write deadlines.

### P-07: Full freshness and directory moves over-crawl

`src/manager.rs` can enumerate whole trees when a Full sentinel would suffice
and hydrate checkpoints before checking whether they are already ready.

**Remediate:** Persist a durable Full sentinel, inspect readiness first, and
apply conservative component-aware invalidation without unnecessary allocation.

### P-08: The advertised end-to-end runner cannot build its target

`run_e2e.sh` requests an undeclared `btrfs-awacs-e2e` binary.

**Remediate:** Declare and maintain a real Linux/Btrfs direct-scan target or
remove the runner and document the supported command.

**Acceptance dependency:** Required acceptance gate 1 cannot claim direct
end-to-end coverage until this target or its documented replacement actually
builds and runs. A missing target or environment-skipped integration is a
blocked gate, not a pass.

### P-09: Snapshot workspace creation rewrites copied metadata

Workspace add snapshots the complete source and then recursively removes copied
`.jj` and `.git` metadata, causing large copy-on-write metadata churn.

**Remediate:** Separate mutable repository metadata from the snapshotted tree or
construct the destination without recursive rewriting.

### P-10: Each direct command repeats immutable input and discovery work

The client repeatedly parses sparse/ignore state, probes executable policy, and
runs discovery.

**Remediate:** Reuse validated immutable input bundles without sharing stale
authority.

### P-11: Each direct command recreates connection and renewal infrastructure

The client opens a new connection and creates a renewal thread for each direct
command.

**Remediate:** Reuse bounded connection or renewal infrastructure without
sharing stale authority.

### P-12: Sessions, tombstones, and cleanup scans need hard bounds

Active sessions, finished-session tombstones, and cleanup scans are not bounded
enough for sustained load.

**Remediate:** Bound session count, tombstone lifetime/cardinality, and cleanup
work per request or maintenance tick.

### P-13: Install entry points are normally discoverable

Both installers place `awacs` on the default executable path, so direct
discovery does not need deployment-specific `PATH` or
`BTRFS_AWACS_COMMAND` configuration.

**Remediate:** Document and install one deliberate direct-scan discovery path
consistently across installers.

## P2: remaining direct compatibility defects

### C-26: Malformed direct invalidations are silently dropped

`../jj/lib/src/local_working_copy.rs` uses `filter_map` for raw invalidation
paths. A malformed entry can become an empty matcher while its cursor is still
committed.

**Remediate:** Reject malformed paths or force Full before advancing the
cursor.

### C-28: Auto snapshot mode rejects an existing empty destination

**Status:** Implemented in the current lifecycle change; keep open until the
supported-host acceptance matrix passes.

The earlier auto path rejected a destination that stock workspace creation
accepts when it already exists and is empty. The current path selects ordinary
workspace initialization for that case and materializes the requested checkout.

**Remediate:** Preserve stock existing-empty-destination behavior in auto mode.

### C-29: Auto snapshot mode does not fall back across filesystems

**Status:** Implemented in the current lifecycle change; keep open until the
supported-host acceptance matrix passes.

The earlier auto path could fail instead of using ordinary workspace creation
when source and destination could not participate in one Btrfs snapshot.

**Remediate:** Define cross-filesystem failure as an auto-fallback case. When
snapshot mode is required (`btrfs.enabled = true` or
`--btrfs-snapshot=true`), preserve the error and fail workspace creation.

### C-30: Colocated removal leaves stale linked Git worktree state

Colocated workspace removal can leave linked Git worktree administration behind.

**Remediate:** Remove linked worktree administration together with the
workspace.

**Overlap:** C-28 and C-29 are fallback reachability/stock-parity issues; C-02
owns the separate unsafe baseline if fallback occurs. C-30 is independent.

## Required acceptance and support boundaries

Run kernel-dependent tests on Linux with the supported modified Btrfs kernel,
privileged broker, disposable eligible Btrfs subvolumes, and a real
AWACS-enabled Jujutsu binary. Ordinary unit tests, macOS execution, schema
inspection, and environment-skipped integrations cannot establish this
boundary.

1. **Build and deployment:** Resolve both checkouts with AWACS enabled and
   disabled; after P-08 is repaired, run the supported direct end-to-end
   target, both installers, broker activation, direct discovery, permissions,
   and clean startup. Until then, record the direct end-to-end part of this
   gate as blocked rather than passed.
2. **Stock Jujutsu parity:** Compare `none` and AWACS for workspace add/remove,
   optional fallback, sparsity, ignores, `jj run`, external diff editing,
   colocated Git, and unsupported-cursor fallback.
3. **Immutable index oracle:** Compare snapshots and indexed events against an
   independently generated full inode/reference graph.
4. **Direct immutable transaction:** Mutate the live checkout during leased
   traversal; verify descriptor identity, invalidation, fingerprints, renewal,
   failed Begin delivery, Finish/abort, restart, and cursor/tree atomicity.
5. **Recovery and retention:** Crash around receipts, publication, compaction,
   exact baseline removal, leases, pins, broker deletion, and cursor-domain
   changes; require continuity or explicit Full.
6. **Workspace safety:** Reject removal of shared storage, dirty sibling data,
   replaced targets, and unrecoverable deletion failures.
7. **Authority and isolation:** Exercise descriptor passing, process
   replacement, mount namespaces, chroot, multiple roots, grants, revocation,
   stale epochs, malformed packets, kernel identity corruption, and fd leakage.
8. **Resource and latency budgets:** Measure clean/dirty p50/p95/p99,
   filesystem writeback, changed-object calls, coalescing, metadata traversal,
   full-crawl count, inotify watches, subprocesses, threads, buffers,
   tombstones, SQLite/WAL growth, retained snapshot bytes, and pins.

For gate 2, “unsupported-cursor fallback” is narrow: a malformed,
unauthenticated, wrong-version/domain/store/epoch, or no-longer-retained opaque
prior cursor is treated as no reusable boundary. The server must return Full
from a newly leased immutable snapshot, and Jujutsu may replace the cursor
only atomically with the tree derived from that snapshot. It must not reuse
stale incremental paths, accept or rewrite an unknown cursor, or traverse the
mutable live root while claiming an AWACS cursor. Backend-unavailable fallback,
if supported, is a separate policy and needs its own stock-`none` oracle.

Until these gates pass, the defensible support claim is limited to the reviewed
custom-kernel ABI, eligible Btrfs root and mount topology, authorized broker,
direct AWACS feature/build combination, supported path representation, and
documented configuration.
