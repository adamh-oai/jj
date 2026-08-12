---
title: "Jujutsu snapshot transactions"
description: "Immutable traversal, descriptor validation, backend-tagged persistence, scan matchers, and save-before-Finish."
sidebar:
  order: 5
---
```mermaid
sequenceDiagram
    participant CLI as "Jujutsu CLI"
    participant WC as "LockedLocalWorkingCopy"
    participant Client as "AWACS ScanClient"
    participant Daemon as "FacadeScanHandler"
    participant State as "Live .jj working-copy state"

    CLI->>CLI: "Build ignore rules, sparse matchers, and input fingerprint"
    CLI->>WC: "snapshot with prior AWACS baseline"
    WC->>Client: "BeginScan(live root, prior baseline)"
    Client->>Daemon: "Begin on scan.sock"
    Daemon-->>Client: "Pinned snapshot fd, next baseline, invalidation, deadline"
    Client-->>WC: "Validated immutable SnapshotLease"
    WC->>WC: "Start renewal owner, scan only /proc/self/fd/N"
    WC->>WC: "Build tree/file state from the immutable snapshot"
    WC->>WC: "Retain pending lease until working-copy finish"
    WC->>State: "Atomically save tree and matching AWACS baseline"
    WC->>Client: "FinishScan(Committed) after successful save"
    Client->>Daemon: "Release snapshot/query pin"
```

`TreeState` constructs a `SnapshotScan` with a selected scan root, optional
changed-path matcher, typed AWACS baseline, and optional pending completion.
The `none` branch uses the normal live working-copy path. The direct AWACS
branch:

1. Connects an injected test client or discovers/connects `SocketScanClient`.
2. Requires the versioned external-input fingerprint.
3. Sends the previous AWACS baseline only when its fingerprint still
   match the current inputs.
4. Receives and validates the snapshot directory descriptor and identity.
5. Uses `/proc/self/fd/<descriptor>` as the scan root while retaining the fd.
6. Converts `ExactPaths` or `Prefixes` into ordinary Jujutsu matchers.
7. Reads `.gitignore`, directory entries, symlinks, file contents, executable
   bits, deletions, and tracked-state candidates from the immutable scan root.
8. Continues writing locks and `.jj/working_copy/tree_state` under the live
   workspace metadata path.
9. Starts a lease-renewal owner while the immutable scan remains active.
10. Keeps `PendingScan` on `LockedLocalWorkingCopy`, rather than dropping it at
    the end of `TreeState::snapshot`.
11. Saves the new tree state and AWACS baseline together before sending
    `FinishScan(Committed)`.

The ordering matters: `TreeState::snapshot` computes state but does not itself
persist it. `LockedLocalWorkingCopy::finish` is the durable boundary.

If traversal fails, an active renewal fails, the caller drops the transaction,
or checkout/reset/recovery/sparse mutation invalidates the immutable baseline,
the pending session must be aborted and its baseline cleared. Untracked paths
that Jujutsu cannot cache also prevent baseline persistence, forcing a fresh scan
later. A failure to acknowledge Finish after a successful state save is cleanup
failure; the already-saved tree/baseline pair remains the client-side durable
result, and the daemon must eventually expire its pin.

## Typed baseline persistence

The working-copy protobuf now contains:

```text
WorkingCopyState {
    baseline: AwacsSnapshotBaseline,
}

AwacsSnapshotBaseline {
    filesystem_uuid,
    subvolume_uuid,
    continuity_token,
    retention_token,
    interpretation_input_fingerprint,
}
```

The baseline is valid only with its exact immutable snapshot identity and
matching external-input fingerprint. `FsmonitorCursor` remains reserved for
Watchman-style mutable monitors.

## Matcher and invalidation contract

The effective scan matcher intersects the Jujutsu sparse matcher with the union
of backend invalidation and explicitly force-tracked paths.

- `Full` selects every eligible sparse path.
- `ExactPaths` selects only those changed paths.
- `Prefixes` selects entire changed subtrees.
- A changed `.gitignore` adds its parent subtree to the rescan set.
- A worktree-relative global excludes file is read from the immutable scan root
  and currently forces a conservative full traversal.
- An empty incremental invalidation can safely skip tree traversal only when
  the prior baseline, tree state, and fingerprint all describe the same
  baseline.

Malformed, absolute, nonrepresentable, or parent-escaping invalidation paths
must fail closed or force `Full`; silently dropping such a path and then
advancing the baseline would lose changes.
