# AWACS and Jujutsu remediation tracker

This is the current prioritized remediation plan for the direct AWACS
immutable-snapshot backend and its companion checkout at `../jj`. The
authoritative architecture and support boundary are in [`SPEC.md`](SPEC.md).
The [documentation site](docs/) organizes the same lifecycle by component.

The active findings below retain their original stable IDs. IDs that described
removed compatibility paths are intentionally absent rather than reused.

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

`../jj/cli/src/commands/workspace/remove.rs` rejects only the currently
selected workspace name. From a secondary workspace, removing the primary can
delete the shared `.jj/repo` and colocated Git object database used by every
surviving workspace.

**Remediate:** Resolve the target identity and every shared operation/object/Git
store before deletion. Refuse a target containing storage required by a
surviving workspace; protect against ancestry, aliases, replaced symlinks, and
concurrent workspace changes.

**Accept when:** Removing a shared-store owner fails without changing
registrations, history, operations, filesystem contents, or Git objects.

### C-02: Optional snapshot fallback fabricates a populated baseline

`../jj/cli/src/commands/workspace/add.rs` can retain a snapshot-only source
baseline after optional Btrfs snapshot creation falls back to an ordinary empty
directory. The next scan then records inherited tracked files as deleted or
attaches a direct cursor to nonexistent contents.

**Remediate:** Associate the inherited baseline only with a verified physical
snapshot. Ordinary fallback must use stock workspace initialization and
materialize the desired checkout before recording a cursor.

**Accept when:** Nonempty sources survive auto fallback on non-Btrfs,
cross-filesystem, absent-tooling, and existing-empty-destination cases.

### C-03: The companion checkout cannot resolve its workspace dependencies

`../jj/Cargo.toml` points the path dependency at a nonexistent sibling. Cargo
resolves that dependency before any build or test, even when AWACS is disabled.

**Remediate:** Correct the path or use an independently resolvable integration
strategy while preserving feature-disabled and non-Linux builds.

**Accept when:** Jujutsu Cargo metadata, ordinary builds, and supported Linux
`--features awacs` builds resolve from a clean checkout.

### C-05: Invalid immutable targets can permanently wedge a watch

`src/service.rs` advances the physical snapshot head before all
nested-subvolume, fscrypt, manifest, and target-object checks complete. A
permanently invalid target can leave physical and indexed heads inconsistent.

**Remediate:** Perform rejection-sensitive validation before publishing the
physical head, or define one atomic terminal-failure/quarantine transition.

**Accept when:** Injected invalid targets leave no stuck heads, operations,
admissions, pins, or permanently unserviceable direct clients after restart.

### P-01: Production has no snapshot or history garbage collection

`src/service.rs` implements maintenance helpers, but daemon startup and request
processing do not invoke them. Long-lived use retains snapshots, revisions,
events, SQLite/WAL storage, and copy-on-write extents without a bound.

**Remediate:** Repair retained-boundary foreign keys, then run bounded,
observable production maintenance that honors heads, leases, grants, pins,
operations, and broker fences.

### P-02: Every status can flush unrelated filesystem-wide writes

`src/broker.rs` calls `syncfs` after snapshot creation and deletion, waiting for
unrelated writes on the same filesystem.

**Remediate:** Use the narrowest durable ordering primitive that preserves the
changed-object contract and measure the cost under unrelated write pressure.

## P1: correctness, isolation, and lifecycle defects

### C-06: History compaction violates retained-boundary foreign keys

`src/manager.rs` retains client boundaries while deleting older parent cut
rows. SQLite rejects maintenance after partial compaction work has committed.

**Remediate:** Make retained boundary ownership and deletion order
foreign-key-safe and transactionally atomic.

### C-07-C-09: External ignore handling changes or poisons Jujutsu state

The companion Jujutsu checkout can read the same ignore inputs twice, reverse
relative global-ignore precedence, or omit worktree-relative global excludes
from `jj run` and external diff-edit paths. A tree can then be paired with the
wrong fingerprint or include/exclude private files even with AWACS disabled.

**Remediate:** Build one immutable external-input bundle and apply stock ignore
precedence consistently across ordinary and direct scans.

**Accept when:** `none` and AWACS match stock behavior for global excludes,
repository excludes, `jj run`, external diff editing, and concurrent ignore
changes.

### C-11: Server lease expiry and advertised client deadline disagree

`src/scan_facade.rs` derives durable expiry before an expensive cut but
advertises a later boot-time deadline after the cut.

**Remediate:** Establish one boot-scoped monotonic deadline after the lease can
actually be returned and communicate that exact deadline to both peers.

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

### C-17-C-22: Workspace lifecycle violates stock safety

Sparse widening can record missing files as deletions; removal can destroy
unsnapshotted sibling edits, follow replaced symlinks, forget registrations
before failed deletion, fail auto mode when `btrfs` is absent, or create nested
subvolumes that violate AWACS boundaries.

**Remediate:** Preserve stock fallback semantics, verify target identity and
shared storage, protect dirty workspaces, check deletion capability first, and
materialize the requested sparse destination before recording a baseline.

### C-23: Parsed kernel identities and completion counters are incomplete

`src/service.rs` and `src/broker.rs` do not reconcile every advertised
filesystem/source/target identity, transaction/root ID, record count, and
output-byte count before publication.

**Remediate:** Carry authenticated endpoint expectations through normal and
recovered manifest parsing and reject every independent inconsistency.

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

### P-04-P-05: Adjacent deltas are recomputed and cuts fail to coalesce

The facade can repeat an already-published adjacent comparison, while manager
admission joins only the fleeting planned phase.

**Remediate:** Reuse pinned published deltas and keep authorized batches
joinable through expensive snapshot/index phases.

### P-06-P-12: Connections, sessions, and cleanup need hard bounds

The daemon creates one OS thread per connection; packet buffers, sessions,
tombstones, and cleanup scans are not bounded enough for sustained load.

**Remediate:** Bound clients, workers, buffers, queue depth, in-flight cuts,
deadlines, tombstones, and cleanup work.

### P-07: Full freshness and directory moves over-crawl

`src/manager.rs` can enumerate whole trees when a Full sentinel would suffice
and hydrate checkpoints before checking whether they are already ready.

**Remediate:** Persist a durable Full sentinel, inspect readiness first, and
apply conservative component-aware invalidation without unnecessary allocation.

### P-08: The advertised end-to-end runner cannot build its target

`run_e2e.sh` requests an undeclared `btrfs-awacs-e2e` binary.

**Remediate:** Declare and maintain a real Linux/Btrfs direct-scan target or
remove the runner and document the supported command.

### P-09: Snapshot workspace creation rewrites copied metadata

Workspace add snapshots the complete source and then recursively removes copied
`.jj` and `.git` metadata, causing large copy-on-write metadata churn.

**Remediate:** Separate mutable repository metadata from the snapshotted tree or
construct the destination without recursive rewriting.

### P-10-P-11: Each direct command repeats external work

The client repeatedly parses sparse/ignore state, probes executable policy,
runs discovery, opens a connection, and creates a renewal thread.

**Remediate:** Reuse validated immutable input bundles and bounded connection or
renewal infrastructure without sharing stale authority.

### P-13: Install entry points are not normally discoverable

Both installers place `btrfs-awacs` under `libexec` rather than a default
`PATH`, so direct discovery needs deployment-specific `PATH` or
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

### C-28-C-30: Optional workspace behavior diverges from stock

Auto snapshot creation rejects supported existing empty destinations, fails
instead of falling back across filesystems, and leaves stale linked Git
worktree state after colocated workspace removal.

**Remediate:** Preserve stock behavior in auto mode and remove linked worktree
administration together with the workspace.

## Required acceptance and support boundaries

Run kernel-dependent tests on Linux with the supported modified Btrfs kernel,
privileged broker, disposable eligible Btrfs subvolumes, and a real
AWACS-enabled Jujutsu binary. Ordinary unit tests, macOS execution, schema
inspection, and environment-skipped integrations cannot establish this
boundary.

1. **Build and deployment:** Resolve both checkouts with AWACS enabled and
   disabled; run the supported direct end-to-end target, both installers,
   broker activation, direct discovery, permissions, and clean startup.
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

Until these gates pass, the defensible support claim is limited to the reviewed
custom-kernel ABI, eligible Btrfs root and mount topology, authorized broker,
direct AWACS feature/build combination, supported path representation, and
documented configuration.
