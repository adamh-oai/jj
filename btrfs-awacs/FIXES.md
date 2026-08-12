# AWACS correctness, compatibility, and performance fixes

This document records issues identified by inspecting the implementation. The
project and its tests were not executed on the review machine. References name
the implementation boundary rather than unstable source line numbers.

AWACS is a focused Btrfs-backed filesystem monitor. Its intended compatibility
target is the subset of the Watchman BSER protocol used by a pinned jj client
and Git's fsmonitor hook protocol version 2. It is not a general Watchman
server, Git's built-in fsmonitor daemon, or a filesystem-independent watcher.
Repository roots must satisfy the supported Btrfs-subvolume, custom-kernel,
mount-namespace, and authorization constraints.

Adoption of an already-created Btrfs snapshot descendant is supported. Watchman
trigger registration, listing, and background execution are not supported;
their future implementation is tracked in [TODO.md](TODO.md).

## Correctness and client-visible compatibility

### P0: A discarded directory witness can produce a falsely clean status

`src/index.rs` emits `DirectoryDirtyWitness` when an ancestor changed without a
surviving named endpoint change. `src/compat.rs::project_events` unconditionally
discards that witness. The comment assumes the client's cached tree exactly
matches its authenticated snapshot clock, but neither jj nor Git proves that
the filesystem stops changing while it crawls or refreshes its cached state.

One failing sequence is:

1. AWACS creates snapshot B and returns its clock.
2. A temporary file appears after B and the client observes it during its
   crawl or stat pass.
3. The file disappears before snapshot C.
4. B and C have the same endpoint names; the surviving parent-directory witness
   is discarded.
5. AWACS returns an empty non-fresh response although the client still caches
   the removed file.

jj can immediately persist C's clock when the matcher is empty. Git can retain
stale tracked, untracked-cache, or fsmonitor-valid state. The same class
includes a temporary directory subtree, an overwritten name, a hardlink alias,
and changes observed during post-response client work.

**Fix:** Keep every directory witness until consumer-specific projection. For
Git, invalidate the affected directory prefix or return `/` when the prefix
cannot be represented safely. For jj, expand the affected subtree against the
relevant immutable index, use a proven-complete exact-name journal interval,
or return `is_fresh_instance = true`. Establish an explicit client-baseline
contract; do not infer that equal snapshot endpoints imply equal client state.

**Tests:** Pause a real jj or Git client after it receives clock B but before it
finishes crawling. Create and remove a file, a nested subtree, and a hardlink
before cut C. Compare the monitored result with an independent full scan and
verify that no falsely clean result advances the client clock.

### P0: The optional precision guard records a journal that queries ignore

The **precision guard** in `src/precision.rs` is an optional recursive Linux
inotify watcher, separate from the mandatory root-path/mount-namespace
continuity monitor. It watches each reachable directory, writes exact mutation
hints into the durable SQLite `mutation_events` table, and associates those
hints with a guard epoch and monotonically increasing cursor. A private marker
and explicit drain establish that a cursor covers a complete interval; lost
watches, overflow, or marker failure mark the epoch gapped. It is enabled only
when `BTRFS_AWACS_PRECISION_GUARD=1` and complete directory traversal is
possible. It is not a replacement for snapshot comparison: mechanisms such as
writable `mmap` are outside a complete inotify-only content contract.

`src/manager.rs` persists and pins complete guard intervals, and
`src/compat.rs::project_ready_cut_range_with_lease` already knows how to merge
their exact names with immutable snapshot events. The actual client path in
`src/facade.rs::prepare_query_after_cut` instead performs a direct historical
comparison and calls `project_events`. Consequently the recorded journal is
orphaned from jj/Git responses and cannot repair the directory-witness race.

**Fix:** Route query projection through the lease-pinned range projector when
both boundaries share a complete guard epoch and all guard records are
contiguous. Merge exact names with authoritative snapshot object/reference
changes. If the guard is absent, gapped, incomplete, over budget, or lacks
coverage, apply the conservative directory-witness behavior above. Keep
mandatory namespace continuity independent of recursive guard availability.

**Tests:** Exercise complete, gapped, overflowed, restarted, and reclaimed
guard epochs; marker creation/deletion; directory moves; metadata exclusions;
and transient names observed by a client between snapshot and crawl. Verify
the same correctness guarantees with the guard disabled.

### P0: A compacted clock can be resolved to the wrong baseline

`src/facade.rs::replay_boundary_for_clock` accepts a retained boundary with
`cut_sequence <= old.cut_sequence`. Its claim that comparing an older retained
snapshot with the current head can only over-report paths is incorrect.

For example, an older retained snapshot contains `a`, the client's actual
snapshot contains `b` after `a -> b`, and the current snapshot again contains
`a` after `b -> a`. Comparing the retained endpoint with the current endpoint
cannot reveal that the client's cached `b` must be removed. The same false
negative occurs when a name exists only in a reclaimed intermediate baseline.

**Fix:** Resolve exactly the authenticated token's watch, epoch, owner grant,
monitor session, boundary kind, cut sequence, and target snapshot UUID. If the
exact baseline remains available, replay a complete pinned adjacent event
range or compare that exact snapshot with the target. If it is unavailable,
return a fresh/full invalidation. Never substitute an older retained boundary.
Coordinate reclamation and query pinning transactionally.

**Tests:** Force compaction of the client's exact baseline while keeping an
older boundary. Cover `a -> b -> a`, create/delete, same-name replacement,
hardlink alias deletion, directory moves, copied/foreign tokens, future tokens,
restart, and concurrent GC. Require all necessary paths or an explicit fresh
response; an empty incremental result is never acceptable.

### P0: Compaction contradicts retained-boundary foreign keys and lookup

`src/manager.rs::retain_exponential_replay_checkpoints` keeps a sparse set of
`fsmonitor_boundaries`, but its final
`reclaim_unreferenced_cut_comparisons` step attempts to delete every
`watch_cuts` row older than the newest cut. Each retained boundary has a
composite foreign key to its exact `watch_cuts` row. With the configured
`PRAGMA foreign_keys = ON`, deleting those parent rows therefore fails with a
foreign-key violation instead of completing maintenance. Earlier boundary and
revision reclamation occurs in separately committed transactions, so this
failure can leave a partially compacted history.

There is a second coupling: `src/manager.rs::historical_snapshot_sequence`
determines watch membership and sequence using `watch_cuts` and, in a limited
case, the current watch head. It never consults a retained boundary. If a
future schema change or legacy state permits boundary retention after its cut
row disappears, exact historical queries fail with "source snapshot is not
retained on this watch" even when the boundary and snapshot still exist.

Repairing either foreign keys or membership lookup alone would expose the
preceding older-baseline false negative unless exact clock resolution is fixed
at the same time.

**Fix:** Preserve every `watch_cuts` row referenced by a retained boundary, or
redesign boundaries to own independent durable snapshot/sequence membership
without a foreign key to reclaimable cut rows. Make the chosen membership
source authoritative for historical lookup and retain every adjacent event
required by an exact client clock; when that exact history is unavailable,
return fresh. Validate the complete `(watch, clock epoch, cut sequence,
snapshot UUID, boundary kind)` tuple and physical snapshot state. Perform
retention, floor changes, boundary removal, event reclamation, and query pin
checks in one atomic transaction or a fenced recoverable maintenance plan.

**Tests:** Compact a watch with several retained older checkpoints and verify
foreign-key integrity, successful maintenance, exact-baseline lookup, and no
partial committed cleanup. Then prune each exact boundary, snapshot, history
segment, or epoch separately and verify a fresh response. Repeat under
concurrent query pinning, injected transaction failure, and daemon restart.

### P0: An invalid immutable cut advances the physical head too early

`src/manager.rs::publish_validated_physical_cut` requires the caller to reject
nested-subvolume boundaries and unsupported fscrypt metadata before publication.
`src/service.rs::finish_cut` publishes the physical snapshot head first and only
later parses boundary records or materializes target objects. A nested
subvolume or fscrypt inode can therefore leave the physical head ahead of the
indexed head while its operation remains in `manifest_ready`.

`src/manager.rs::fail_cut_comparison` is the existing terminal-failure
transition. It atomically marks the `watch_cuts` row and operation failed,
records the error, releases operation-owned snapshot pins, and abandons waiting
cut admissions. Production never calls it; only a manager test does. Thus a
validation failure currently returns an error without performing that durable
cleanup. Calling the helper alone is insufficient because it does not rewind or
replace the already-advanced physical head, and the rejected immutable snapshot
cannot become valid during restart.

**Fix:** Prefer validating the immutable target and all required endpoint
metadata before `publish_validated_physical_cut`, so rejected snapshots never
become the physical head. If publication must precede validation, atomically
terminally fail the operation, release pins/admissions, establish a recoverable
physical/indexed-head invariant, quarantine or delete the rejected snapshot,
and permit the next valid cut or full-fresh recovery to advance. Recovery must
not retry the same permanently invalid snapshot forever.

**Tests:** Introduce a nested subvolume or fscrypt object after initialization.
Inspect both watch heads, operation state, `watch_cuts`, admissions, pins,
snapshot lifecycle, daemon restart, and the next valid cut. Inject failure at
every stage before and after physical publication and validate recovery.

### P1: Full-fresh fallback is not propagated through the client response

`src/manager.rs::publish_full_fresh_checkpoint` records
`watch_cuts.fresh_instance = 1`, but `PublishedCut` does not carry that flag.
`src/facade.rs` still attempts a historical comparison for a supplied old clock.
On a legacy kernel without the required dirty-witness capability, that second
comparison fails for the same reason that caused full-fresh fallback, so the
client receives an error rather than a valid `/` invalidation.

**Fix:** Carry freshness explicitly through `PublishedCut`, admission polling,
facade completion, Watchman encoding, and Git hook encoding. When the newly
committed cut is full-fresh, skip historical comparison and return its new
clock with a full invalidation. Do not lose or reinterpret the committed
freshness state during shared-cut admission or recovery.

**Tests:** Use legacy-only and malformed/incomplete v2 stream fixtures, then
issue initial and incremental jj/Git queries. Verify successful fresh
responses, committed clocks, follower responses, restart behavior, and the
next successful incremental cut.

### P1: Client-side baseline resets can invalidate otherwise genuine clocks

An authenticated clock proves which snapshot AWACS compared; it does not prove
that jj's expected tree or Git's index still corresponds to that snapshot.
jj resets/imports or colocated Git operations can replace its expected tree
while excluded `.git`/`.jj` metadata hides the cause from the monitor. Git can
likewise replace its index or fsmonitor-valid bitmap independently.

**Fix:** Audit the pinned jj `TreeState::reset`, recovery, checkout, import, and
colocated Git integration paths and clear the saved clock whenever the expected
tree changes. Verify Git invalidates its fsmonitor bitmap on index replacement,
configuration changes, and repository transitions. Server-side token binding
is necessary but cannot repair an unrelated stale client baseline.

**Tests:** Reset or replace client state without changing monitored user files,
then compare monitored jj/Git status with fsmonitor-disabled full scans.
Include interrupted checkouts, colocated repositories, sparse-index transitions,
and copied state databases.

### P1: Trigger commands are intentionally unsupported and deletion is not truthful

The pinned jj client calls `trigger-del` even when
`fsmonitor.watchman.register-snapshot-trigger = false`; when enabled, it calls
`trigger-list` and registers `jj-background-monitor`. AWACS currently returns a
synthetic `deleted: false` for `trigger-del`, rejects `trigger-list` and
`trigger`, and does not execute a background scheduler. Some jj diagnostic
paths call `trigger-list` even when automatic registration is disabled.

**Fix:** Until trigger support exists, document trigger-disabled operation
accurately, return truthful errors for unsupported registration/listing, and
make the minimal deletion lifecycle safe and accurately scoped. Implement the
complete future protocol, authorization, scheduling, persistence, and security
requirements before claiming trigger support; see [TODO.md](TODO.md).

**Tests:** Run real pinned jj initialization, repeated initialization, status,
and trigger diagnostics with trigger registration disabled. Separately verify
the unsupported enabled configuration fails clearly without partially
registering or executing a command.

## Authorization, time, and kernel trust boundaries

### P1: Connected peer identity is reused for a different frame sender

`src/watchman_transport.rs` authenticates a Unix-stream connector once using
`SO_PEERCRED` and `SO_PEERPIDFD`. `recv_authenticated` then reads bytes with
plain `recv` and clones the original connector's identity for every frame.
A connected descriptor inherited or passed to a same-UID process in another
mount namespace or chroot can be used to send a request that is checked against
the connector's namespace rather than the actual sending process.

**Fix:** Enable `SO_PASSCRED`, receive request data with `recvmsg`, and bind
each complete frame to kernel-supplied `SCM_CREDENTIALS` plus a verified
`SCM_PIDFD` or equivalent live process handle when the supported Linux
transport can prove those credentials describe the actual sender. Reject
missing credentials, mixed-identity byte spans, and frame boundaries that
cannot be tied to one sender. If the Watchman-compatible Unix-stream transport
cannot provide that guarantee, redesign requests around an authenticated
per-request namespace/root handle or another provably nondelegable authority.
Reject process exit, identity changes, unsafe descriptor delegation, and
namespace/root mismatches before invoking privileged operations or writing a
successful response. Merely rereading `SO_PEERCRED` does not solve descriptor
passing.

**Tests:** Pass a connected descriptor with `SCM_RIGHTS`, inherit it across
fork/exec, switch mount namespaces and chroots, exit the original process,
attempt PID reuse, and inject missing or mixed `SCM_CREDENTIALS`/`SCM_PIDFD`
spans. Verify each individual frame is authorized against its actual sender
and that no partial successful response escapes on rejection.

### P1: Correctness-critical leases use adjustable wall-clock time

`src/main.rs`, `src/service.rs`, and `src/watchman.rs` derive timestamps from
`SystemTime`/`UNIX_EPOCH`. Those values feed cut admissions, operation leases,
query pins, historical comparisons, retention, and dormant trigger leases.
Wall-clock jumps backward can retain abandoned leases indefinitely; jumps
forward can expire active ownership and permit unsafe takeover or reclamation.

**Fix:** Introduce an explicit boot-scoped monotonic time domain using
`CLOCK_BOOTTIME` for every correctness deadline and duration. Store or verify
the boot ID alongside persisted deadlines; invalidate or recover old-boot
leases before comparing them. Keep Unix timestamps solely for logs,
human-facing diagnostics, and retention policies deliberately defined in civil
time. Avoid mixing the two domains in arithmetic or SQL predicates.

**Tests:** Inject forward/backward wall-clock jumps, suspend/resume, lease
renewal races, process restart, boot-ID changes, expired admissions, concurrent
GC, and active historical jobs. Confirm active owners remain fenced and stale
owners are eventually reclaimed.

### P1: Parsed kernel stream identities and completion counts are discarded

`src/manifest.rs` parses the v2 filesystem UUID, source/target UUIDs,
source/target root IDs, source/target transaction IDs, record count, and
completion checksum. `src/service.rs::parse_kernel_changed_objects` retains
capability and object data but does not compare the parsed endpoint identities
with the actual authenticated snapshot descriptors. `src/broker.rs` checks
the ioctl-reported byte count only against a maximum, derives the final byte
count independently from file metadata, and never verifies `output_records`.

This requires malformed or inconsistent kernel output, a corrupted staged
manifest, or another violated kernel contract; the broker already separately
checks the opened snapshot descriptors. It is still a missing defense at the
privileged kernel-to-SQLite boundary.

**Fix:** Carry the v2 header through normal and recovered comparisons. Require
exact filesystem, source/target UUID, root-ID, and transaction-ID matches with
the verified source and target descriptors. Require ioctl `output_bytes` to
match actual file length and the stream's completion framing; require
`output_records` to match the parsed/footer count, accounting explicitly for
whether the completion record is included. Reject every mismatch before
index publication or staged-manifest reuse.

**Tests:** Independently mutate each identity, root ID, transaction ID, CRC,
capability bit, ioctl byte count, footer byte count, ioctl record count,
footer record count, and output limit. Recompute valid checksums where needed
so identity/count validation cannot pass accidentally due to generic corruption
detection.

### P1: Malformed Watchman expressions can leak durable query leases

`src/watchman.rs::validate_expression` evaluates an expression against the
empty path instead of fully validating its syntax tree. A name array such as
`["name", ["", 123]]` can short-circuit successfully for the empty candidate
but fail later on a real changed pathname. By then facade finalization has
created a durable query lease and response pin; expression filtering can
return early without releasing them.

**Fix:** Implement a complete structural validator that visits every operator,
operand, string, array element, scope, and depth before admitting a cut. Use an
RAII-style response/lease guard or an explicit single cleanup path so every
post-allocation failure releases the lease, including filtering, frame
encoding, response-size fallback, sender revalidation, and socket write errors.

**Tests:** Generate malformed expressions whose invalid branch is hidden by
`anyof`, `allof`, `name` arrays, nesting, or path-dependent evaluation. Assert
no cut is admitted for invalid syntax and verify query leases, revision pins,
comparison pins, and response gates are empty after every injected failure.

## Performance and resource lifetime

### P0: Each query can flush unrelated writes across the entire filesystem

Every jj/Git clock or query creates a new read-only snapshot.
`src/broker.rs::sync_filesystem` calls `syncfs` after snapshot creation,
deletion, and selected reconciliation paths. `syncfs` flushes the whole Btrfs
filesystem, so an otherwise clean status can wait for unrelated builds,
downloads, image writes, or other repositories. Changed-object spool files also
receive multiple durability barriers across broker and manager stages.

**Fix:** Define the minimum Btrfs transaction durability needed for snapshot
receipt recovery. Replace filesystem-wide synchronization with an applicable
transaction-specific `START_SYNC`/`WAIT_SYNC` barrier, an appropriately scoped
fsync, or a safely batched commit. Batch deletion barriers and eliminate
redundant spool syncs only after proving crash-recovery equivalence.

**Tests:** Measure clean and one-file jj/Git status with idle storage and
concurrent unrelated buffered writes on the same Btrfs filesystem. Count
`syncfs`, `fsync`, transaction waits, ioctl calls, and p50/p95/p99 latency;
inject crashes before and after every retained durability boundary.

### P1: Incremental queries repeat the already-completed kernel comparison

Publishing a cut computes and stores its adjacent changed-object events.
`src/facade.rs::prepare_query_after_cut` then invokes
`Service::historical_changes` for the old token, repeating the kernel
comparison, manifest spool, hashing, parsing, target-object lookup, and
temporary SQLite writes. Finalization runs while the daemon-wide facade mutex
is held, serializing this expensive second comparison across clients.

**Fix:** For an exact immediately preceding baseline, project the newly
published and pinned `PublishedCut.events`. For longer complete retained
ranges, use `project_ready_cut_range_with_lease`; apply the correct witness and
guard semantics before returning. Reserve direct historical comparison for a
proved exact-baseline case that actually needs it. Keep long kernel operations
and path expansion outside the global facade lock while preserving namespace,
grant, and response fencing.

**Tests:** Assert one changed-object ioctl for an adjacent incremental request,
correct projection for multi-cut and fresh ranges, and no global serialization
of independent watches. Capture concurrent status latency, spool writes,
SQLite transactions, and lock hold times.

### P1: Missing snapshot lineage silently becomes a full repository initialization

`src/service.rs::adopt_snapshot_descendant` and
`src/manager.rs::adopt_snapshot_descendant` currently return `None` both when a
root is not an adoptable descendant and when its parent lineage exists but the
expected retained snapshot revision is unavailable. `src/watchman.rs` treats
both cases identically and falls back to `initialize`, which creates another
snapshot and performs an `O(namespace size)` full index/tree search.

This hides a retention or lifecycle error on a path that is supposed to reuse
its known parent revision. Large externally created snapshot descendants can
therefore unexpectedly cause expensive privileged repository crawls.

**Fix:** Replace `Option<InitializedWatch>` with explicit outcomes such as
`Adopted`, `NotDescendant`, and `KnownLineageMissingSeed`. Detect known parent
UUIDs independently from the presence of a ready/present seed revision. Allow
full initialization only when missing lineage is expected for a genuinely new
root; fail or safely retry known-lineage requests whose retained seed was
deleted, reclaimed, not yet committed, or raced by maintenance. Coordinate
parent lookup, seed snapshot/revision pinning, retention, and descendant
publication transactionally.

**Tests:** Register a genuine non-descendant, a descendant with a present seed,
a descendant whose known parent's seed is missing/deleted/not ready, and a
descendant racing parent publication or retention. Verify the known-lineage
failure/retry path never calls full-index creation, privileged tree search, or
fallback initialization; confirm successful adoption shares the exact parent
revision.

### P0: Production does not reclaim snapshots or enforce configured retention

`src/service.rs::garbage_collect` and `maintain_history` exist but no active
daemon lifecycle or public maintenance command invokes them. Each client query
therefore retains another read-only Btrfs snapshot, revision, and change
history. `replay_window_cuts` and `replay_window_ns` are configured but not
consumed, and `maintain_history` ignores its timestamp.

**Fix:** First repair exact-clock and retained-boundary correctness. Then add a
bounded production maintenance loop or explicit operational command that
enforces count-, age-, and storage-based retention while respecting grants,
active query pins, physical/indexed heads, failed operations, and broker
fences. Expose snapshot count, retained bytes, SQLite/WAL size, oldest lease,
history floor, failed operations, deletion backlog, and maintenance latency.

**Tests:** Issue thousands of actual jj/Git queries, advance monotonic and
retention clocks, hold/release pins, restart during deletion, and inject broker
failures. Verify bounded snapshots and database growth, preserved exact active
baselines, explicit fresh fallback after reclamation, and successful recovery.

### P1: Concurrent queries cannot join the expensive in-flight cut

`src/manager.rs::admit_planned_cut` joins only `operations.state = 'planned'`.
The leader quickly transitions to `fs_started`, before the expensive snapshot,
filesystem synchronization, delta comparison, and index publication. A second
client arriving during that long interval cannot safely join the existing cut
and may fail, spin, or create additional physical snapshots.

**Fix:** Maintain a per-watch joinable in-flight batch through the snapshot and
indexing phases, or explicitly admit followers to authorized `fs_started` and
`manifest_ready` operations. Preserve sender/root checks, grant generation,
session and epoch fences, cut ordering, waiter expiry, disconnect handling,
shared result publication, and next-cut admission. Do not return a stale
pre-request snapshot merely to improve batching.

**Tests:** Admit clients at `planned`, `fs_started`, snapshot barrier, manifest
parse, SQLite publication, boundary finalization, revocation, and response
encoding. Verify one appropriate cut, correct shared results, independent
waiter cancellation, authorization isolation, and bounded contention.

### P1: Directory moves force unnecessary full crawls and metadata self-churn

`src/compat.rs::project_events` converts every `SubtreeMoved` event into a
fresh `/` result before `.git`/`.jj` filtering. Even a rename wholly inside
excluded client metadata can trigger a complete repository crawl. Git can
represent directory-prefix invalidations directly, while jj needs expanded
descendant names or a bounded fresh fallback. Metadata writes are filtered only
after snapshotting, kernel comparison, and indexing, so client-owned clock and
index updates can generate avoidable monitoring work.

**Fix:** Apply exact component-aware consumer exclusions before escalating a
directory move. Emit old/new directory prefixes for Git; expand old/new jj
subtrees from immutable indexes up to a clear size budget, otherwise return
fresh. Investigate safe earlier exclusion of `.git` and `.jj` churn without
weakening client-baseline reset detection, nested-subvolume validation, or
other authorization invariants.

**Tests:** Rename small and large trees, move trees into/out of excluded
metadata, change only `.git`/`.jj`, replace parents, and exercise raw byte
names. Compare monitored status with full scans and measure crawl count,
snapshot/delta work, subtree expansion time, and self-triggering loops.

### P1: History maintenance repeatedly crawls already-ready checkpoints

`src/manager.rs::retain_exponential_replay_checkpoints` attempts to compact
each retained boundary. `compact_revision` loads, hashes, and summarizes the
whole namespace before determining that an existing ready checkpoint already
satisfies the request. Once production maintenance exists, one pass can cost
`O(repository size * retained checkpoint count)` even when nothing changed.

**Fix:** Inspect compacted/checkpoint metadata before loading the inode graph.
Only compact newly selected or actually deep overlay revisions, limit work per
maintenance cycle, and preserve all pinned revision/snapshot relationships.

**Tests:** Build large repositories with many already-ready checkpoints, rerun
maintenance without changes, and count SQL reads, namespace allocations,
hashing, bytes read, wall time, and retained snapshot correctness.

### P1: Full-fresh recovery materializes an unnecessary complete event list

`src/manager.rs::full_fresh_events` expands every indexed pathname even though
jj/Git need only the newly committed baseline and a full `/` invalidation.
`publish_full_fresh_checkpoint` also duplicates objects, references, and events
into comparison staging before writing the full checkpoint. Recovery for a
large repository therefore expands every path and writes much of the namespace
more than once.

**Fix:** Represent a full-fresh comparison as a compact durable sentinel and
persist the required checkpoint without constructing per-path events. Generate
a full inventory lazily only for an explicitly supported API that needs one.
Keep replay, admissions, leases, and client freshness semantics unambiguous.

**Tests:** Force full-fresh fallback on large and high-hardlink repositories;
compare memory, SQLite writes, WAL growth, runtime, resulting clock, and
subsequent incremental correctness.

### P1: Daemon connection, frame, and protocol costs are unbounded

`src/main.rs` starts an operating-system thread for every incoming connection;
the server lacks an equivalent deadline for an indefinitely partial request
frame. Git's hook connects for every invocation and sends both `watch-project`
and a generic Watchman `query`, despite exposing a supposedly focused Git hook
interface. jj creates monitor clients repeatedly and may execute discovery when
`WATCHMAN_SOCK` is unset. The nonconcurrent daemon path can also decode the same
BSER frame three times: initial `decode_and_authorize`, another authorization
inside `prepare_authenticated_frame`, and final endpoint dispatch. Each repeat
also revisits root/grant checks and facade-lock acquisition.

**Fix:** Bound active connections, worker threads, frame read time, outstanding
cuts, queue depth, and per-client resources. Use a dedicated authenticated
single-request Git protocol or cached root registration where compatible.
Carry one decoded, authorized request through dispatch, reuse socket discovery
safely, and avoid redundant root authorization without weakening the required
final sender/grant revalidation. Preserve response-size limits, sender
identity, root binding, and grant revocation.

**Tests:** Open many idle/partial sockets, flood connections, create slow
readers/writers, repeatedly invoke the Git hook, and run parallel jj clients.
Assert bounded threads, file descriptors, memory, broker work, latency, and
fairness; verify real-client compatibility after reducing round trips.

### P2: Recursive precision watching has unbounded directory-scale overhead

When enabled, `src/precision.rs` recursively enumerates directories and adds an
inotify watch for each one. Large trees can hit kernel watch limits, consume
substantial memory, or add expensive re-arming work after directory creation and
rename. The marker itself must not create a perpetual wakeup loop.

**Fix:** Set explicit watch-count, traversal-time, memory, and event-queue
budgets. Surface degraded/gapped state and continue with conservative snapshot
projection. Consider a kernel-native bounded mutation journal only if it
preserves exact transient-name completeness and overflow generation.

**Tests:** Exercise very deep/wide trees, watch exhaustion, denied traversal,
rename storms, overflow, marker churn, daemon restart, and fallback correctness
with the guard disabled.

## Deployment and actual client boundaries

### P1: A clean default installation does not create its spool directory

`src/main.rs::automatic_watchman_paths` creates the state and managed snapshot
directories but returns `state_dir/spool` without creating it.
`src/service.rs` immediately requires the spool directory to exist, have the
expected owner, and be private. Automatic jj/Git daemon startup therefore
fails on a clean installation unless an operator created or overrode the
directory separately.

**Fix:** Create the default spool directory with the correct principal and mode
before constructing the service, validate existing directories without
following symlinks, and apply the same checks to explicit overrides.

**Tests:** Start from empty state/runtime directories; vary umask, owner, mode,
symlink attacks, missing parents, explicit spool overrides, and broker
availability. Verify automatic discovery and the first real jj/Git query.

### P1: Installers do not provide a consistent discoverable command set

`install.sh` creates `watchman` and `git-fsmonitor-hook` aliases but omits the
documented `btrfs-awacs-watchman` alias. Both install scripts place the
discovery executable under `libexec`, which is not normally on `PATH`. A jj
client without `WATCHMAN_SOCK` may therefore fail to discover AWACS even
though installation succeeded.

**Fix:** Make both installers produce the same documented multicall aliases and
install a discoverable wrapper/symlink on an intended `PATH`, or explicitly
configure and test `WATCHMAN_SOCK`/client discovery. Validate broker service
paths, permissions, package prefixes, `DESTDIR`, and fresh-install behavior.

**Tests:** Exercise both installation methods in a clean Linux image, with and
without `WATCHMAN_SOCK`, normal `PATH`, alternate prefixes, absent runtime
directories, and real Git/jj invocations.

### P1: One namespace daemon cannot serve roots on different Btrfs filesystems

The automatic daemon socket is scoped to the mount namespace, but its manager
database and managed snapshot directory are selected from the first repository.
A second root on another Btrfs filesystem reaches the same daemon even though
its snapshot cannot be created in the first filesystem's managed directory.

**Fix:** Partition managers, broker configuration, snapshot directories, and
durable state by filesystem UUID inside the namespace daemon, or expose one
daemon/socket per `(mount namespace, filesystem UUID)`. If multi-filesystem
support is deliberately excluded, reject subsequent roots early with a clear
diagnostic and document the limitation.

**Tests:** Register two roots on the same filesystem, two distinct Btrfs
filesystems, bind mounts, changed mount namespaces, and an explicitly
configured manager directory. Verify correct routing, authorization, snapshot
placement, and independent retention.

### P1: The advertised end-to-end runner has no declared executable target

`run_e2e.sh` invokes `cargo build --bin btrfs-awacs-e2e`, but `Cargo.toml` has
`autobins = false` and declares only `btrfs-awacs`. There is no checked-in
matching integration binary. Several formerly advertised realistic acceptance
paths were excluded rather than wired into runnable Linux/Btrfs coverage.

**Fix:** Add a declared, maintained Linux/Btrfs/UML integration-test target or
replace the script with the actual supported test command. Provide a custom
kernel image, a disposable Btrfs filesystem, broker setup, real pinned jj/Git
clients, fixture capture, and deterministic race/failure hooks. Fail CI if any
documented acceptance target is missing.

**Tests:** In the supported remote Linux environment, build the documented
target from a clean checkout, provision the filesystem/broker, and run the
complete correctness, recovery, compatibility, and performance suites below.

### P1: Claimed replacement compatibility exceeds the tested support envelope

AWACS does not implement arbitrary Watchman commands, subscriptions, saved
state, SCM-aware clocks, general trigger programs, Git's built-in daemon, or
ordinary filesystem roots unrelated to eligible Btrfs subvolumes. Git's stock
Watchman sample hook and arbitrary client/library versions are not established
compatibility boundaries. Raw non-UTF-8 paths must also be traced through the
actual pinned Watchman client and jj pathname representation; preserving bytes
inside AWACS alone is not proof that the client accepts them.

**Fix:** Publish an explicit support matrix naming the exact custom kernel ABI,
Btrfs root shape, filesystem/mount constraints, jj revision,
`watchman_client` version, Git versions, supported expressions/fields/token
forms, metadata exclusions, Git hook protocol, and raw-path behavior. Reject
unsupported configurations clearly instead of implying general drop-in
compatibility.

**Tests:** Capture byte-exact BSER fixtures from the pinned jj client, then run
real-client differential tests across each claimed jj/Git version and support
matrix entry. Treat unsupported features as explicit negative tests.

## Required validation before making a compatibility claim

All of the following tests require a suitable remote Linux environment with the
modified Btrfs kernel and a disposable filesystem; none were run during this
inspection.

1. **Independent inode/reference oracle:** Generate random valid trees and
   mutation sequences; prove that applying `delta(A, B)` to an independent
   full index of A matches an independent full index of B. Include hardlinks,
   multiple aliases, inode reuse, overwrite, directory moves, symlinks, chmod,
   chown, executable bits, xattrs, reflinks, arbitrary non-NUL path bytes,
   nested directories, and packed-reference cancellation.
2. **Kernel mutation coverage:** Exercise ordinary writes, truncate, fallocate,
   modify/restore, writable `mmap`, fsync, inode metadata, security/trusted
   xattrs, fscrypt, verity, dedupe against managed read-only snapshots, nested
   subvolumes, root replacement, ancestor rename, mount-over, and mutation
   witnesses across consecutive snapshot barriers.
3. **Client-observation races:** Pause actual jj/Git clients after a clock is
   issued and during crawls/re-stats. Create/remove transient files and complete
   subtrees, rename names away and back, replace parents, and change hardlinks.
   Compare every monitored state and advanced clock against a full scan.
4. **Retention and clock soundness:** Reclaim exact and intermediate boundaries,
   prune `watch_cuts`, race GC with queries, restart the daemon, copy tokens
   across watches/users/stores, alter epoch/session/UUID/sequence claims, and
   assert either a complete incremental result or an explicit fresh response.
5. **Malformed kernel output and crash recovery:** Independently alter every
   endpoint identity, count, checksum, capability, boundary, xattr reset, and
   generation. Crash around snapshot creation, manifest staging, head
   publication, index publication, query fencing, and snapshot deletion;
   inspect persistent heads, operations, leases, pins, receipts, and retries.
6. **Real Watchman protocol fixtures:** Record pinned `watchman_client` 0.9.0
   discovery, `watch-project`, `clock`, initial/incremental queries, integer and
   string clocks, exact jj expressions, trigger-disabled deletion, diagnostics,
   unsupported trigger registration/listing, semantic errors, framing limits,
   and the actual raw-path support boundary.
7. **Real Git/jj differential behavior:** Compare fsmonitor-enabled and disabled
   results for tracked/untracked paths, `.gitignore`, sparse checkout, sparse
   index, untracked cache, directory moves, hardlinks, externally linked Git
   working directories, Btrfs snapshot-descendant adoption, colocated `.jj`/`.git`,
   client-side expected-tree/index resets, and supported version upgrades.
8. **Authorization and namespaces:** Pass and inherit connected sockets, switch
   mount namespaces/chroots, revoke grants, expire/renew leases, restart
   monitors, lose root-component watches, overflow inotify, replace watched
   roots, inject partial frames, and validate every response against its actual
   sender without leaking pins.
9. **Deployment and lifecycle:** Test both installers, executable aliases,
   `PATH` and `WATCHMAN_SOCK` discovery, fresh spool creation, broker service
   permissions, absent `XDG_RUNTIME_DIR`, unsupported root shapes, independent
   Btrfs filesystems, retained database upgrades, daemon restart, and bounded
   production GC.
10. **Performance acceptance:** Measure cold/warm unchanged status, one-file
    changes, small/large directory moves, high hardlink fanout, 100k/1M-path
    initialization, concurrent clients, unrelated filesystem writeback,
    snapshots and retained bytes, SQLite/WAL growth, `syncfs`/`fsync`, kernel
    comparison counts, inotify watches, open descriptors, thread counts,
    per-watch fairness, and p50/p95/p99 latency.

The strongest defensible compatibility claim is conditional: the explicitly
tested jj Watchman subset and Git hook-v2 protocol behave conservatively for
the documented Btrfs/kernel/client support matrix. General Watchman, Git's
built-in fsmonitor daemon and Watchman triggers remain outside that claim.
