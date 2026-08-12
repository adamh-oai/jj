# Deferred Watchman/jj trigger support

Watchman triggers are **not currently supported**. The intended trigger-disabled
jj configuration is:

```toml
fsmonitor.backend = "watchman"
fsmonitor.watchman.register-snapshot-trigger = false
```

`src/watchman.rs` rejects trigger registration and listing, and its
`trigger-del` response is only an incomplete compatibility placeholder.
`src/manager.rs` contains reserved durable trigger state and grant/fence
helpers; `src/trigger.rs` is dormant scaffolding and is not compiled through
`src/lib.rs`. Their presence is not evidence of working trigger support.

Implement all of the following before advertising even the narrow jj trigger
subset. Do not restore general Watchman trigger programs or execute arbitrary
client-supplied commands.

## 1. Pin and capture the actual jj client contract

The inspected jj checkout uses `watchman_client` 0.9.0. Its initialization
first resolves the root with `watch-project` and then:

- With `register-snapshot-trigger = false`, calls `trigger-del` for
  `jj-background-monitor`.
- With the setting enabled, calls `trigger-list`; if the fixed name is absent,
  calls `trigger` with the exact command `jj --quiet util snapshot`.
- Supplies an expression excluding `.git`, `.jj`, and their descendants, with
  `stdout` and `stderr` set to `>/dev/null` on Linux.
- Calls `trigger-list` separately from some diagnostic paths, including paths
  reached when registration is disabled.

Capture real BSER-v2 requests and responses from the pinned jj revision and
library. Specify the accepted root representation, object keys, optional
fields, expression shape, command vector, redirection values, response types,
and duplicate-registration semantics. Keep the compatibility contract pinned
to reviewed versions and fail closed on unsupported drift.

## 2. Implement truthful `trigger-del`

Decode and validate exactly the expected command arity, root, and fixed trigger
name. Resolve the actual authenticated sender to the current active watch and
grant; reject foreign roots, revoked grants, unsupported names, and malformed
fields.

Call the durable grant-scoped deletion path and return `deleted: true` only
when the caller's existing trigger row was actually removed. Return
`deleted: false` for a genuine idempotent absence. Never delete another
principal's registration, another watch's trigger, or a stale grant's state.
Do not create a snapshot merely to delete a trigger.

Define how deletion races with an already-claimed process. Either revoke the
claim before execution or fence the command and completion so deletion cannot
silently resurrect state or report a completed run under a replaced grant.

## 3. Implement grant-scoped `trigger-list`

Return the BSER object and trigger metadata expected by `watchman_client`,
including the exact fixed trigger name and any fields required for
deserialization. Show only active rows belonging to the authenticated root and
current authorization generation. An absent registration returns an empty
list; malformed/foreign roots and revoked grants fail explicitly.

Listing is observational: it must not take a cut, enqueue work, create a
registration, reveal another principal's command, or revive stale durable
state. Support jj's diagnostic calls as well as enabled-registration discovery.

## 4. Implement one tightly validated `trigger` registration

Accept only the pinned jj request shape for `jj-background-monitor`. Validate:

- The exact watched root, actual sender identity, active grant, and root/mount
  binding.
- The fixed name and exact argument vector `jj --quiet util snapshot`; define
  whether the executable must be an approved absolute path or a safely resolved
  executable in a trusted per-user `PATH`.
- The supported `.git`/`.jj` exclusion expression, including every nested
  operand and component boundary.
- Supported `stdout`/`stderr` values and all optional request fields; reject
  shell snippets, pipes, redirections other than the reviewed fixed behavior,
  environment injection, `chdir`, `relative_root`, globbing, subscriptions, and
  unknown keys.

Persist registration atomically under `(watch_id, owner_grant_id, fixed name)`.
Return the exact response fields expected by the pinned client. Re-registering
an identical definition is idempotent; changing a definition must follow a
reviewed, fenced update rule. Initial registration queues exactly the
unconditional evaluation that jj expects.

## 5. Grant trigger permission to dynamically registered roots

`watch-project` currently creates/adopts dynamic roots with `READ | CUT`, while
`Store::register_fixed_jj_trigger` requires `READ | CUT | TRIGGER`. Future
support cannot simply re-enable the dormant registration code.

Define an explicit opt-in policy for granting `TRIGGER` only to an authenticated
per-user daemon with approved execution configuration. Apply that policy both
to the initial watch and to every subsequently initialized or adopted root.
Ensure reused watches, replacement grants, descendant snapshots, and daemon
reconnects receive the intended permission without escalation.

Never grant trigger execution to a system-wide/root broker, another UID, a
foreign namespace, or a caller that cannot prove its exact monitored root.
Grant revocation must atomically invalidate pending and running claims.

## 6. Build an active, bounded scheduler

Compile reviewed trigger code intentionally; do not uncomment an obsolete
scheduler without establishing its lifecycle. The scheduler must:

- Start only in an opted-in per-user daemon running as the owning UID/GID.
- Observe durable registrations and committed changed paths without holding
  the global facade mutex during a cut, process execution, or blocking wait.
- Coalesce repeated mutations and join an authorized in-flight cut when safe.
- Preserve per-watch fairness, maximum concurrency, bounded queues, debounce,
  backoff, run timeout, and resource limits.
- Avoid rescheduling solely because `jj` updated `.jj` or Git updated `.git`.
- Avoid unbounded clean polling, repeated full-repository crawls, and a
  snapshot-per-timer-tick when nothing relevant changed.
- Shut down cleanly, cancel or fence in-flight work, and make restarts safe.

Define a fallback when the optional precision guard is unavailable: explicitly
disable low-latency trigger execution, or use a bounded documented periodic
policy. Do not claim prompt notifications without a real change source.

## 7. Integrate precision-guard wakeups without weakening correctness

The optional guard is a recursive inotify-backed durable exact-name journal;
it is distinct from the mandatory namespace/root continuity monitor. Use its
readiness descriptors only while its epoch is active and complete.

Drain and certify a private marker, persist mutation events and cursor updates
atomically, and wake the scheduler only for relevant non-metadata paths. Marker
creation/deletion must not wake the scheduler into a self-triggering loop.
Directory watch loss, overflow, denied traversal, move ambiguity, mount changes,
or guard restart must gap the epoch and transition the scheduler to its
documented degraded mode.

Do not infer complete file-content detection from inotify alone: writable
`mmap` and other mutation mechanisms still require the snapshot comparison
contract. Preserve exact snapshot/cursor ordering and never advance a trigger
past an incompletely observed interval.

## 8. Fence durable claims, evaluation, and subprocess execution

Use the existing durable fields deliberately: `pending_through_seq`,
`last_evaluated_seq`, `run_owner`, `run_fence`, and `run_expires_ns`.

Claim work in a short writer transaction after verifying the current watch,
grant, principal, clock epoch, namespace binding, and positive boot-scoped
lease. Revalidate immediately before the subprocess starts. Execute outside
SQLite transactions and facade locks. Finish with a compare-and-swap on the
same owner/fence/grant; advance `last_evaluated_seq` only after successful
execution.

Mutations arriving during a run must remain pending and schedule a subsequent
evaluation. Failed or timed-out runs must release ownership, retain pending
work, apply bounded retry/backoff, and avoid starving other watches. A stolen,
expired, revoked, restarted, or deleted claim must never commit stale success.

Integrate the `CLOCK_BOOTTIME` deadline fix described in [FIXES.md](FIXES.md)
before relying on persisted run expiry.

## 9. Keep execution strictly inside the per-user security boundary

Never execute triggers in the privileged broker or root/system daemon. Confirm
the runner's effective UID/GID exactly match the grant principal and reject UID
0. Resolve the watched root through the authenticated live sender namespace,
recheck ancestor/mount continuity, and ensure the subprocess working directory
is the exact registered root.

Spawn directly without a shell. Use an explicitly reviewed executable and
argument vector, clear inherited environment, and set only bounded reviewed
values such as `HOME`, trusted `PATH`, `WATCHMAN_SOCK`, `WATCHMAN_ROOT`, and
`WATCHMAN_TRIGGER`. Avoid untrusted executable substitution, arbitrary cwd,
descriptor leakage, network side effects introduced by inherited state, and
cross-principal command execution.

Apply output handling, process-group cleanup, timeouts, resource limits, and
safe logging without leaking repository data or authentication material.
Handle grant revocation and namespace changes that occur between claim and
spawn, and decide whether an already-started child must be terminated.

## 10. Make persistence, recovery, and configuration explicit

Persist complete reviewed command metadata and authorization generation; reject
or migrate stale rows that cannot be interpreted safely. On daemon restart,
invalidate old process/session claims, recover pending work, rotate boot-scoped
deadlines after a boot change, and fence already-running subprocesses.

Coordinate root replacement, mount changes, facade invalidation, grant
revocation, watch deletion, database compaction, historical retention, and
broker restart with trigger state. Preserve or cancel pending evaluations
according to an explicit durable policy; never claim successful execution when
the process result is unknown.

Expose a disabled-by-default configuration, supported client versions, current
registration count, pending backlog, running claims, retries, guard gaps,
execution latency, and last successful sequence. Keep unsupported command
errors truthful until the entire path is available.

## 11. Add the required remote Linux/Btrfs tests

1. **Pinned wire fixtures:** Byte-exact BSER capture/replay for enabled and
   disabled jj startup, `trigger-del`, empty and populated `trigger-list`,
   initial registration, duplicate registration, exact response shapes,
   diagnostics, malformed requests, unsupported fields, and raw root bytes.
2. **Actual jj behavior:** Run the pinned jj client with both trigger settings,
   multiple status invocations, trigger diagnostics, colocated Git, externally
   created Btrfs snapshot descendants, and multiple supported jj versions.
   Verify a real background `jj --quiet util snapshot` occurs only for relevant
   filesystem changes.
3. **Root and grant isolation:** Register several roots/principals, including
   dynamically initialized/adopted roots; verify `READ | CUT | TRIGGER` is
   granted only when opted in, lists are isolated, deletion is truthful, and
   no cross-user or cross-watch command runs.
4. **Namespace and sender security:** Pass/inherit sockets across namespaces,
   change chroots, rename watched roots/ancestors, replace mounts, revoke
   grants, expire claims, and restart monitors immediately before spawn.
   Verify no command executes under stale authority.
5. **Concurrency and fencing:** Race registration, duplicate registration,
   deletion, grant revocation, multiple schedulers, simultaneous watches,
   shared cuts, arriving mutations, lease expiry, and process completion.
   Assert exactly the intended runs and durable compare-and-swap transitions.
6. **Precision and degraded operation:** Verify marker readiness,
   metadata-only changes, nested directory creation/rename, overflow, watch
   exhaustion, permission denial, restart, and guard gaps. Prove no self-wakeup
   loop and no false promise of low-latency wakeups when the guard is absent.
7. **Failure and restart:** Kill the daemon or subprocess before claim, after
   claim, after spawn, after exit, and before durable completion; inject SQLite,
   broker, executable-resolution, and output failures. Verify fenced recovery,
   bounded retry, and retention of unprocessed sequences.
8. **Execution hardening:** Try arbitrary names/argv, shell metacharacters,
   environment manipulation, untrusted `PATH`, relative executables, invalid
   stdout/stderr, foreign cwd, root execution, leaked descriptors, and hung
   subprocesses. Every unapproved request fails without command execution.
9. **Performance bounds:** Measure clean idle behavior, metadata churn,
   high-frequency mutations, many registered watches, slow snapshots, large
   repositories, watch-count limits, thread/file-descriptor use, fairness,
   backoff, and p50/p95/p99 wakeup-to-snapshot latency.

Only after these gates pass in the supported remote environment may the project
describe `jj-background-monitor` as supported. General Watchman trigger
programs remain outside the intended compatibility scope.
