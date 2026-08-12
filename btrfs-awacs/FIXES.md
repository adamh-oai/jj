# AWACS and Jujutsu remediation tracker

This is the current, prioritized remediation plan for `btrfs-awacs` and its
companion checkout at `../jj`. The authoritative architecture, complete
finding inventory, source ownership, and review-time evidence are in
[SPEC.md](SPEC.md), especially [Section 21: verified implementation gaps and
review findings](SPEC.md#21-verified-implementation-gaps-and-review-findings).
The [documentation site](docs/) organizes the same implementation and lifecycle
by component. The older [indexed change-tracking design](docs/indexed-change-tracking.md)
describes intended contracts; a schema table, dormant helper, or design proposal
does not establish that the corresponding production behavior exists.

`C-NN` and `P-NN` below refer to the correctness and performance findings in
`SPEC.md`. Related items are grouped here to keep this document actionable.
Source-backed legacy concerns without a dedicated specification finding are
identified explicitly. Previously reported substitution of an older retained
clock boundary and failure to create the automatic spool directory are fixed;
neither is an outstanding remediation item.

## Scope and noninterchangeable client contracts

AWACS exposes three materially different integration boundaries:

- **Focused Watchman compatibility:** Jujutsu receives changed names and an
  authenticated clock, then crawls the **mutable live checkout**.
- **Git fsmonitor hook v2:** Git receives a token and NUL-delimited names or
  `/`, then refreshes its index and untracked state against the **mutable live
  checkout**. The current helper internally uses the focused Watchman endpoint;
  it is not Git's built-in fsmonitor daemon.
- **Direct Jujutsu scans:** Jujutsu receives an authenticated cursor, path
  invalidation, revocable lease, and read-only snapshot directory descriptor;
  it crawls exactly that **immutable snapshot** via `/proc/self/fd/N` and must
  commit its cursor only with the tree derived from the same snapshot.

The transient live-crawl/directory-witness race is a **Watchman/Git** problem;
it does not automatically transfer to the direct immutable scan. Conversely,
descriptor validation, session renewal, external-input fingerprints, and
transactional scan completion are **direct-scan** requirements that ordinary
Watchman does not provide. Findings and acceptance tests must preserve that
distinction.

## P0: release blockers and silent data loss

### C-01: Removing the primary workspace can destroy every workspace

[`../jj/cli/src/commands/workspace/remove.rs`](../jj/cli/src/commands/workspace/remove.rs)
rejects only the *currently selected workspace name*. From a secondary
workspace, `jj workspace remove default` can recursively delete the primary
workspace containing the shared `.jj/repo` and possibly the colocated Git
object database. Secondary workspaces reference that same repository rather
than owning independent history.

**Remediate:** Resolve and verify the target's workspace identity and every
shared operation/object/Git store before any transaction or deletion. Refuse a
target containing storage required by any surviving workspace; protect against
path ancestry, aliases, replaced symlinks, and concurrent workspace changes.

**Accept when:** Removing `default` from secondary colocated and
non-colocated workspaces fails without changing registrations, history,
operations, filesystem contents, or Git objects.

### C-02: Optional snapshot fallback fabricates a populated baseline

[`../jj/cli/src/commands/workspace/add.rs`](../jj/cli/src/commands/workspace/add.rs)
captures the source commit before attempting an optional Btrfs snapshot. If
snapshot creation falls back to an ordinary empty directory, it retains that
snapshot-only source baseline. `TreeState::reset` records the source tree
without writing its files; the next scan can record every inherited tracked
file as deleted or attach a Watchman baseline to nonexistent contents.

**Remediate:** Associate the inherited baseline strictly with a successfully
created, verified physical snapshot. On ordinary fallback, use stock Jujutsu
workspace initialization and materialize the desired checkout before recording
any filesystem-monitor cursor.

**Accept when:** A nonempty source tree survives automatic fallback on a
non-Btrfs root, absent optional tooling, an existing empty directory, and an
unsupported/cross-filesystem destination; the initial monitored and
unmonitored status matches stock Jujutsu.

### C-03: The companion checkout cannot resolve its workspace dependencies

[`../jj/Cargo.toml`](../jj/Cargo.toml) points `btrfs-awacs` at nonexistent
`../bsend-watch`; the actual sibling is `../btrfs-awacs`. Cargo resolves that
workspace path dependency before any build or test, even with the optional
AWACS feature disabled.

**Remediate:** Correct the dependency path or use a deliberate independently
resolvable integration strategy. Preserve normal feature-disabled and
non-Linux builds.

**Accept when:** Jujutsu Cargo metadata, ordinary feature-disabled builds, and
supported Linux `--features awacs` builds all resolve from a clean checkout.

### C-04: Watchman and Git can report a falsely clean live checkout

[`src/compat.rs`](src/compat.rs) discards `DirectoryDirtyWitness`. A live
client can observe a temporary file after receiving clock B; that name can
disappear before cut C, leaving identical immutable endpoints and an empty
incremental result. The client then persists C while retaining incorrect
tracked, untracked, or cached state.

**Remediate:** Keep directory witnesses through consumer-specific projection.
Git must receive a safe affected directory prefix or `/`; Watchman/Jujutsu
must receive complete exact names from a proven contiguous journal interval,
complete immutable subtree expansion, or `is_fresh_instance = true`. Do not
claim endpoint equality proves what a live client observed.

**Scope:** This race affects live Watchman and Git scans. A direct Jujutsu
client reading the exact leased immutable snapshot cannot observe that
particular transient file from the live checkout.

**Accept when:** Paused real Jujutsu Watchman and Git clients survive transient
file/subtree creation, deletion, rename-away/back, overwrite, and hardlink
changes without advancing a falsely clean clock.

### C-05: Invalid immutable targets can permanently wedge a watch

[`src/service.rs`](src/service.rs) advances the physical snapshot head before
all nested-subvolume, fscrypt, manifest, and target-object checks complete.
A permanently invalid target can leave the physical and indexed heads
inconsistent and the operation in `manifest_ready`. Production does not call
the existing durable `fail_cut_comparison` transition, so restart can retry the
same invalid snapshot indefinitely.

**Remediate:** Perform all rejection-sensitive validation before publishing
the physical head, or define one atomic terminal-failure/quarantine/recovery
transition that restores a usable head, fails admissions, releases pins, and
allows the next valid cut.

**Accept when:** Injected nested-subvolume/fscrypt and staged-manifest errors
leave no stuck heads, operations, admissions, pins, or permanently unserviceable
Watchman, Git, or direct-scan clients across restart.

### P-01: Production has no snapshot or history garbage collection

[`src/service.rs`](src/service.rs) implements `garbage_collect` and
`maintain_history`, but daemon startup, request processing, and background
workers never invoke them. Every status creates another managed snapshot;
configured replay windows do not bound snapshots, revisions, SQLite/WAL
storage, event history, or copy-on-write extents.

**Remediate:** First repair retained-boundary foreign keys and exact-baseline
history ownership. Then run bounded, observable production maintenance with
explicit snapshot-count, age, and storage policies; honor live scan/query
leases, grants, physical/indexed heads, broker fences, failed operations, and
retryable delete intents.

**Accept when:** Thousands of real requests with concurrent active leases,
restart, and injected deletion failures keep snapshot count, retained bytes,
SQLite/WAL growth, and deletion backlog bounded while returning fresh whenever
an exact historical baseline has genuinely expired.

### P-02: Every status can flush unrelated filesystem-wide writes

[`src/broker.rs`](src/broker.rs) invokes `syncfs` after Btrfs snapshot creation,
deletion, and selected recovery operations. This waits for writeback on the
entire filesystem, including unrelated builds, image writes, downloads, and
other checkouts. Even an unchanged status can therefore inherit arbitrary
tail latency.

**Remediate:** Establish the exact durability/recovery contract, then replace
whole-filesystem flushes with a valid Btrfs transaction-specific barrier,
scoped durability operation, or safely batched commits. Preserve broker receipt
crash consistency.

**Accept when:** Idle and heavily contended clean/one-file status benchmarks
report snapshot/ioctl, `syncfs`/`fsync`, and p50/p95/p99 counts, and
crash-injection tests validate every remaining durability boundary.

## P1: substantial correctness and compatibility defects

### C-06: History compaction violates its retained-boundary foreign keys

[`src/manager.rs`](src/manager.rs) retains selected `fsmonitor_boundaries`
while `reclaim_unreferenced_cut_comparisons` deletes every older `watch_cuts`
parent. SQLite foreign keys reject that deletion after earlier retention and
revision changes have already committed. Historical membership lookup also
relies on the cut rows that maintenance attempts to remove.

**Remediate:** Retain every parent row needed by a surviving exact boundary,
or redesign durable boundary ownership and lookup together. Make boundary,
cut, revision, snapshot pin, replay floor, and admission reclamation atomic
or explicitly fenced and recoverable; never revive older-boundary substitution.

**Accept when:** Repeated maintenance passes succeed with retained old clocks,
active scans, concurrent queries, injected transaction failures, restart, and
`PRAGMA foreign_key_check`; a missing exact baseline yields fresh.

### C-07–C-09: External ignore handling changes or poisons Jujutsu state

**Tracked findings:** C-07, C-08, and C-09.

[`../jj/cli/src/cli_util.rs`](../jj/cli/src/cli_util.rs) reads external ignore
files once for `base_ignores`, then rereads them for the direct-scan input
fingerprint. A mutation between those reads can persist a cursor claiming the
new fingerprint for a tree produced from old ignore contents. It additionally
removes relative `core.excludesFile` from the shared base matcher: `jj run`
and external diff-edit paths never restore it, while the ordinary snapshot path
restores it after `info/exclude`, reversing Git's required precedence.
These latter regressions affect `none`, Watchman, and AWACS alike.

**Remediate:** Construct one immutable external-input bundle and derive both
the effective matcher and fingerprint from its exact bytes/configuration.
Preserve stock global-versus-repository ignore precedence and ensure every
snapshot entry point receives the same relative ignore semantics. Read
worktree-relative inputs from the leased immutable root when using AWACS.

**Accept when:** Real `none`, Watchman, AWACS, `jj run`, and external diff-edit
comparisons agree with stock Jujutsu and Git for absolute/relative global
ignores, contradictory `info/exclude` rules, and edits injected between
fingerprinting and traversal.

### C-17–C-22, C-28–C-30: Workspace lifecycle violates stock safety

**Tracked findings:** C-17, C-18, C-19, C-20, C-21, C-22, C-28, C-29, and
C-30.

[`../jj/cli/src/commands/workspace/add.rs`](../jj/cli/src/commands/workspace/add.rs)
and [`remove.rs`](../jj/cli/src/commands/workspace/remove.rs) introduce several
independent lifecycle hazards:

- A sparse-source snapshot widened to a full destination can record missing
  inherited files as deletions instead of materializing them.
- Removal deletes an unsnapshotted sibling without checking dirty tracked or
  untracked files and follows a replaced target symlink to unrelated storage.
- Removal commits the forgotten registration before learning that a normal
  Btrfs directory cannot be deleted as a subvolume or that optional Btrfs
  tooling is absent; colocated removal also leaves linked Git-worktree state.
- Optional snapshot creation rejects stock-supported existing empty
  destinations, fails instead of falling back across filesystems or missing
  tooling, and can create a nested subvolume inside a monitored parent.

**Remediate:** Lock and verify target workspace identity without following
replacement symlinks; inspect/snapshot target state and require an explicit
safe policy before destructive deletion; validate target/deletion capability
before committing removal; clean linked Git administration; materialize sparse
differences; and make every `auto` optimization observationally equivalent to
stock Jujutsu. Reject nested monitored destinations before snapshot creation.

**Accept when:** Differential lifecycle tests cover primary/secondary/sparse
workspaces, dirty siblings, path replacement, missing tools, regular Btrfs
directories, linked worktrees, nested destinations, existing empty directories,
cross-filesystem fallback, rollback/recovery, and every Btrfs mode.

### C-10: Direct scans bind a namespace daemon to its first root

[`src/main.rs`](src/main.rs) constructs one `FacadeScanHandler` with the first
canonical root and watch ID. [`src/scan_facade.rs`](src/scan_facade.rs)
unconditionally rejects subsequent roots even though daemon discovery is
mount-namespace-scoped and Watchman registration supports additional watches.
A second repository or sibling Btrfs workspace cannot use direct AWACS.

**Remediate:** Resolve and authorize each Begin request against its actual
canonical root, grant, watch, filesystem, and namespace; safely create/adopt
new watches; bind every session and fd to that exact identity.

**Additional still-live concern:** The namespace daemon also chooses its
manager database and managed-snapshot directory from the first filesystem.
Roots on a second Btrfs filesystem cannot use snapshots stored on the first;
partition services by filesystem UUID or explicitly scope discovery by
`(mount namespace, filesystem UUID)`.

**Accept when:** Independent roots, snapshot-descendant workspaces, bind
mounts, multiple Btrfs filesystems, and unauthorized namespace/root changes
route to the correct isolated service and lease.

### P-03 and C-26: Direct invalidation paths defeat incremental scans

[`src/index.rs`](src/index.rs) and [`src/compat.rs`](src/compat.rs) produce
repository-relative bytes such as `src/file.rs`.
[`src/scan_facade.rs`](src/scan_facade.rs) incorrectly requires `/src/file.rs`,
so every normal nonempty response degrades to `Invalidation::Full`. At the
client boundary,
[`../jj/lib/src/local_working_copy.rs`](../jj/lib/src/local_working_copy.rs)
uses `filter_map` and silently discards malformed paths; an empty resulting
matcher can still commit the new cursor.

**Remediate:** Specify one byte-exact repository-relative representation,
validate components and encoding at both boundaries, preserve exact/prefix
semantics, and fail closed or force `Full` on any unrepresentable path.

**Accept when:** Actual end-to-end adjacent one-file changes remain
incremental; non-UTF-8 supported paths, malformed entries, parent traversal,
absolute paths, `.gitignore`, sparse prefixes, and empty invalidation sets
cannot silently advance an incorrect cursor.

### C-11, C-16, C-25: Lease clocks, locking, and failure cleanup disagree

[`src/scan_facade.rs`](src/scan_facade.rs) captures wall-clock time before the
expensive cut, computes durable session expiry from that stale timestamp, and
advertises a fresh boot-time deadline after the cut.
[`src/scan.rs`](src/scan.rs) holds the global handler mutex across Begin and
response transmission; Begin also holds the shared facade lock while creating
and comparing snapshots. Renew/Finish requests for unrelated sessions cannot
advance. A failed Begin response leaves an already-inserted session pinned,
and expiry cleanup runs only when another request arrives. Correctness-critical
manager, admission, query, and retention leases elsewhere also use adjustable
Unix wall-clock values.

**Remediate:** Use one boot-scoped monotonic correctness clock with persisted
boot identity; establish lease expiry after the cut and communicate the same
actual deadline to both peers. Decouple expensive cuts and socket writes from
global session/renewal locks, add bounded read/write deadlines and independent
maintenance, and abort an allocated session immediately on response-send
failure or disconnect.

**Accept when:** Slow Begin, parallel Renew/Finish, wall-clock jumps,
suspend/resume, failed fd delivery, idle abandoned sessions, daemon restart,
and injected renewal failures neither invalidate an actively traversed
snapshot nor retain pins or working-copy locks indefinitely.

### C-12: Socket connector identity is reused after descriptor delegation

[`src/watchman_transport.rs`](src/watchman_transport.rs) authenticates the
original Unix-stream connector, not each later sending process; direct socket
handling in [`src/main.rs`](src/main.rs) inherits the same authority model.
A same-UID process using an inherited or passed connected descriptor can send
from a different mount namespace or chroot. The direct endpoint can additionally
transfer a private managed-snapshot directory fd.

**Remediate:** Prove per-request sender and namespace/root authority with a
supported nondelegable transport or kernel credentials/process handles. Reject
mixed frame identity, missing or unverifiable credentials, stale processes,
namespace changes, and unauthorized descriptor reuse before privileged work or
fd delivery. Rechecking `SO_PEERCRED` alone does not establish the actual
sender after descriptor passing.

**Accept when:** Passed/inherited descriptors, fork/exec, mount namespace and
chroot changes, process exit, PID reuse, revocation, and malformed ancillary
data never authorize an unintended sender or leak a snapshot fd.

### C-13–C-14: Freshness and optional precision never reach live clients

[`src/manager.rs`](src/manager.rs) records full-fresh publication, but
`PublishedCut` does not carry that state through the Watchman/Git facade.
[`src/facade.rs`](src/facade.rs) can retry the historical comparison that
already required fallback and return an error instead of a fresh `/` result.
The direct immutable client separately degrades safely to `Full`.

The optional recursive inotify precision guard persists certified exact-name
intervals, but the live query path performs `historical_changes` plus
`project_events` rather than the available lease-pinned range projector.
Consequently its overhead does not repair the Watchman/Git transient-witness
race.

**Remediate:** Propagate committed freshness explicitly through publication,
admissions, recovery, facade responses, Watchman encoding, and Git hooks.
For live clients, use only complete, contiguous, epoch-matched precision
intervals to refine directory witnesses; otherwise expand safely or return
fresh. Keep mandatory namespace continuity separate from optional precision.

**Accept when:** Legacy/incomplete kernel streams return successful fresh
responses; complete, gapped, overflowed, restarted, and disabled precision
guards never permit a falsely clean live-client response.

### C-15 and C-23: Protocol and kernel trust boundaries are incomplete

[`src/watchman.rs`](src/watchman.rs) validates expressions by evaluating an
empty path. Short-circuiting can hide malformed operands until a real result
has already allocated a durable response lease; late failure leaks pins.
[`src/service.rs`](src/service.rs), [`src/manifest.rs`](src/manifest.rs), and
[`src/broker.rs`](src/broker.rs) also fail to reconcile every parsed kernel
filesystem/source/target identity, transaction/root ID, ioctl record count,
completion count, and actual output-byte count before publication.

**Remediate:** Validate the entire Watchman expression syntax tree before
admitting a cut; guard every allocated response with unconditional cleanup.
Carry authenticated endpoint expectations through normal and recovered
manifest parsing and reject independently inconsistent identities, counters,
framing, and checksums before indexing.

**Accept when:** Hidden malformed expression branches leave no cuts/pins, and
independent mutations of each kernel header/footer/ioctl identity or count
are rejected before any indexed head advances.

### Live-client baseline resets require explicit invalidation

**Still-live legacy concern:** An authenticated Watchman clock proves an AWACS
snapshot identity, not that Jujutsu's expected tree or Git's index still
corresponds to it. Reset, interrupted checkout, import, recovery, colocated Git
operations, index replacement, and excluded `.jj`/`.git` metadata can replace
client-side baseline state without a matching monitored user-file event.

**Remediate:** Audit all Jujutsu tree reset/import/checkout/recovery paths and
clear incompatible saved Watchman clocks whenever the expected tree changes;
verify Git invalidates its fsmonitor-valid/index state on equivalent
transitions. The direct backend must continue committing its cursor only with
the exact tree produced from its leased immutable snapshot.

**Accept when:** Watchman/Git/direct enabled results match independent full
scans after baseline replacement, interrupted transactions, copied working
copy/index state, colocated imports, and sparse-index transitions.

### Snapshot-descendant lineage loss silently triggers a full initialization

**Still-live legacy concern:**
[`src/service.rs`](src/service.rs) and [`src/manager.rs`](src/manager.rs)
return `None` both for a genuinely unrelated root and for a descendant whose
known parent lacks its expected ready/present seed revision.
[`src/watchman.rs`](src/watchman.rs) treats both identically and invokes full
initialization. A retention, publication, or lifecycle defect can therefore
silently become a privileged `O(repository size)` crawl instead of preserving
or safely retrying known snapshot lineage.

**Remediate:** Return distinct `Adopted`, `NotDescendant`, and
`KnownLineageMissingSeed` outcomes. Detect known parent identity independently
of seed readiness; pin/adopt transactionally and permit full initialization
only for genuinely new roots.

**Accept when:** Missing/deleted/not-ready parent seeds and races with
publication or maintenance never call full-index initialization, while real
new roots and valid descendants retain their intended behavior.

### C-24: Watchman trigger compatibility is intentionally incomplete

[`src/watchman.rs`](src/watchman.rs) returns synthetic `deleted: false` for
`trigger-del` and rejects `trigger-list` and `trigger`. Dormant manager tables
and helper methods are not a production scheduler or trigger implementation.
Certain Jujutsu diagnostics and enabled background-monitor registration thus
remain unsupported.

**Remediate:** Keep `fsmonitor.watchman.register-snapshot-trigger = false`,
return truthful errors, and document the exact supported command subset. Do
not claim trigger support until authorization, persistence, scheduling,
execution, deletion, and real-client compatibility are implemented.

**Accept when:** Real pinned Jujutsu initialization/status succeeds with
registration disabled, enabled registration fails clearly without side
effects, and unsupported diagnostics are accurately documented.

## P1/P2: scaling, deployment, and compatibility follow-up

### P-04–P-05: Adjacent deltas are recomputed and cuts fail to coalesce

Publishing a cut already persists its adjacent changed-object events, but
[`src/facade.rs`](src/facade.rs) repeats the privileged historical kernel
comparison, spool/hash, target lookup, and SQLite work. Meanwhile
[`src/manager.rs`](src/manager.rs) permits followers only while a cut remains
`planned`; requests arriving during expensive `fs_started`/`manifest_ready`
snapshot, flush, or indexing phases cannot join the useful in-flight work.

**Remediate:** Use the already pinned published adjacent delta or a complete
lease-pinned retained range, while preserving live directory-witness
semantics. Keep an authorized per-watch batch joinable through the expensive
cut and release global locks around unrelated expensive work.

**Accept when:** Adjacent status performs one changed-object comparison;
concurrent callers at every snapshot/publication stage share one valid target
cut without crossing grants, roots, namespaces, or epochs.

### P-06 and P-12: Connections, sessions, and cleanup have no hard bounds

[`src/main.rs`](src/main.rs) spawns an operating-system thread per accepted
connection. [`src/scan.rs`](src/scan.rs) allocates an approximately 1 MiB
receive buffer before blocking on each idle direct connection, and direct
transport lacks read/write deadlines.
[`src/scan_facade.rs`](src/scan_facade.rs) scans every live session and
five-minute completion tombstone on every Begin/Renew/Finish, approaching
quadratic cleanup work under sustained traffic.

**Remediate:** Bound clients, workers, packet buffers, queue depth,
in-flight cuts, and per-connection deadlines; use indexed expiry or a
background maintenance heap; never hold a global dispatch lock across a
potentially blocked socket write.

**Accept when:** Idle/partial/nonreading clients, high command rates, renewal
storms, and disconnects produce bounded descriptors, threads, memory,
tombstones, latency, and snapshot pins.

### P-07: Full freshness, ready checkpoints, and directory moves over-crawl

[`src/manager.rs`](src/manager.rs) enumerates every path for full freshness
even though clients need only a new clock plus `/`, and hydrates/hashes whole
revisions before discovering that existing checkpoints are already ready.
[`src/compat.rs`](src/compat.rs) escalates subtree moves before applying
`.git`/`.jj` exclusions, so irrelevant metadata movement can force a full live
client crawl. Git supports compact directory-prefix invalidation, while
Watchman/Jujutsu requires safe descendant expansion or a bounded fresh result.

**Remediate:** Represent full freshness as a durable sentinel; inspect
checkpoint state before hydration; filter component-aware excluded metadata
before escalation; use Git prefixes and bounded immutable-subtree expansion
for Watchman. Ensure excluded metadata churn cannot invalidate required
namespace/security or client-baseline invariants.

**Accept when:** Large repositories, repeated maintenance, `.git`/`.jj`-only
moves, real source-tree moves, hardlink fanout, and full-fallback recovery
avoid unnecessary namespace-sized allocations/crawls while preserving correct
client state.

### P-09: Snapshot workspace creation destroys the metadata-sharing benefit

[`../jj/cli/src/commands/workspace/add.rs`](../jj/cli/src/commands/workspace/add.rs)
snapshots the complete source checkout, then recursively removes copied `.jj`
and `.git` metadata before constructing the destination workspace. On a large
colocated repository this walks and copy-on-write-modifies potentially
hundreds of thousands of metadata entries.

**Remediate:** Separate repository metadata from the snapshotted source tree,
use safe subvolume/layout boundaries, or construct the destination without
recursively rewriting shared repository/object metadata.

**Accept when:** Large colocated repositories show bounded workspace-add
metadata traversal and copy-on-write amplification while preserving correct
Git worktree ownership and AWACS nested-subvolume invariants.

### P-10–P-11: Each direct command repeats external work

[`../jj/cli/src/cli_util.rs`](../jj/cli/src/cli_util.rs) repeatedly parses Git
sparse state, reads ignore files, and probes executable-bit policy.
[`src/scan.rs`](src/scan.rs) launches `btrfs-awacs scan-sockname` for default
discovery, and
[`../jj/lib/src/local_working_copy.rs`](../jj/lib/src/local_working_copy.rs)
opens a new connection and creates/joins a renewal thread even for short clean
scans.

**Remediate:** Reuse the same validated immutable input bundle, parsed sparse
state, resolved executable-bit policy, safe namespace-scoped discovery, and
appropriately bounded renewal infrastructure without sharing stale authority
or cursors.

**Accept when:** Clean and one-file scans show fewer subprocesses, sparse/index
reads, temporary permission probes, thread creations, and socket round trips
without weakening fingerprint or lease correctness.

### Recursive precision watching has directory-scale overhead

**Still-live legacy concern:** When enabled,
[`src/precision.rs`](src/precision.rs) recursively walks reachable directories
and installs one Linux inotify watch per directory. Large/wide trees, rename
storms, unreadable paths, watch exhaustion, queue overflow, and private-marker
churn can consume unbounded time or kernel watch resources.

**Remediate:** Set explicit watch-count, traversal, queue, memory, and
re-arming budgets; surface degraded/gapped state and conservatively project
snapshot witnesses when exact coverage is unavailable.

**Accept when:** Deep/wide trees, denied traversal, exhaustion, directory
moves, overflow, restart, and disabled guard remain bounded and preserve the
same live-client correctness guarantee.

### P-08 and P-13: The advertised validation and install paths do not work

[`run_e2e.sh`](run_e2e.sh) requests nonexistent
`--bin btrfs-awacs-e2e`; [`Cargo.toml`](Cargo.toml) sets `autobins = false`
and declares only `btrfs-awacs`.
[`install.sh`](install.sh) omits the `btrfs-awacs-watchman` alias that
[`packaging/install.sh`](packaging/install.sh) creates, and both place commands
under `libexec` rather than an ordinary default `PATH`.

**Remediate:** Declare and maintain the actual Linux/Btrfs end-to-end target
or replace the script with a real supported command. Make both installers
produce the same documented entry points and a deliberate discoverable `PATH`,
`WATCHMAN_SOCK`, or `BTRFS_AWACS_COMMAND` configuration.

**Accept when:** Clean installation and documented acceptance commands work
with the supported custom kernel, disposable Btrfs filesystem, broker,
namespace-scoped daemon discovery, real pinned Jujutsu/Git versions, and direct
AWACS feature-enabled Jujutsu.

### C-27: Cursor migration is not backward compatible with stock Jujutsu

[`../jj/lib/src/local_working_copy.rs`](../jj/lib/src/local_working_copy.rs)
persists the new backend-tagged cursor without mirroring the legacy Watchman
protobuf field. Older/stock Jujutsu ignores the new field and loses the
existing baseline when binaries alternate.

**Remediate:** Define a safe dual-write/read migration or an explicitly
versioned interoperability policy; never reinterpret a direct AWACS cursor as
a Watchman clock.

**Accept when:** Alternating supported stock, Watchman-enabled, and direct
AWACS binaries preserves a compatible Watchman baseline or deliberately
performs a safe fresh crawl without mixing backend identities.

## Required acceptance and support boundaries

Run kernel-dependent tests on Linux with the supported modified Btrfs kernel,
privileged broker, disposable eligible Btrfs subvolumes, and real pinned
Jujutsu/Git clients. Ordinary unit tests, macOS execution, schema inspection,
and environment-skipped integration tests cannot establish this boundary.

1. **Build and deployment:** Resolve both checkouts with AWACS enabled and
   disabled; run the documented end-to-end target, both installers, broker
   activation, normal discovery, aliases, permissions, and clean startup.
2. **Stock Jujutsu parity:** Compare `none`, Watchman, and AWACS for workspace
   add/remove, optional Btrfs fallback, sparsity, global/repository ignores,
   `jj run`, external diff editing, colocated Git, and cursor migration.
3. **Immutable index oracle:** Differentially compare snapshots and indexed
   events against an independently generated full inode/reference graph;
   include hardlinks, inode reuse, directory moves, metadata, xattrs, reflinks,
   supported raw path bytes, nested subvolumes, and custom kernel witnesses.
4. **Live-client observation:** Pause real Watchman/Git clients between clock
   receipt and live crawl; exercise transient files/subtrees, rename reversal,
   overwrite, hardlink aliases, excluded metadata, precision gaps, and fresh
   fallback. Verify every advanced clock against a full live scan.
5. **Direct immutable transaction:** Mutate the live checkout during leased
   traversal; verify descriptor identity, immutable contents, exact/prefix
   invalidation, external-input fingerprints, renewal, failed Begin delivery,
   Finish/abort, restart, and cursor/tree atomicity.
6. **Recovery and retention:** Crash around snapshot receipts, physical/indexed
   publication, manifest staging, compaction, foreign keys, retention, exact
   baseline removal, query/session pinning, broker deletion, and clock-domain
   changes; require either complete continuity or an explicit fresh result.
7. **Workspace destruction safety:** Reject removal of shared storage, active
   or dirty sibling data, symlink-replaced targets, and unrecoverable deletion
   failures; preserve registrations/history and clean linked Git metadata.
8. **Authority and isolation:** Exercise descriptor passing/inheritance,
   process replacement, mount namespaces, chroot, multiple roots/filesystems,
   grants, revocation, stale epochs, malformed frames/expressions, kernel
   identity/count corruption, and response fd leakage.
9. **Resource and latency budgets:** Measure clean/dirty p50/p95/p99, unrelated
   filesystem writeback, `syncfs`/`fsync`, changed-object ioctl counts,
   concurrent cut coalescing, metadata traversal, full-crawl count,
   inotify watches, subprocesses, threads, buffers, tombstones, SQLite/WAL
   growth, retained snapshot bytes, and active/expired pins.

Until these gates pass, the defensible support claim is limited to the exact
reviewed custom-kernel ABI, eligible Btrfs root and mount topology, authorized
broker, pinned client versions, focused Watchman command/expression subset,
Git hook-v2 framing, direct AWACS feature/build combination, supported path
representation, and documented trigger-disabled configuration. AWACS is not a
general Watchman server, Git's built-in fsmonitor daemon, an arbitrary
filesystem watcher, or a verified drop-in replacement outside that envelope.
