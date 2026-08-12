---
title: "Direct immutable-snapshot API"
description: "The Begin, Renew, Promote, and Finish protocol, typed snapshot baselines, immutable snapshot fds, and daemon sessions."
sidebar:
  order: 3
---
## Public request/result contract

`src/scan.rs` exposes the transport-independent API consumed by
Jujutsu:

```text
BeginScanRequest {
    live_root: absolute live working-copy path,
    baseline_owner_id: stable opaque workspace identity,
    previous_baseline: optional SnapshotBaseline,
}

SnapshotBaseline {
    identity: filesystem UUID + subvolume UUID + read-only flag,
    continuity_token: opaque authenticated proof for that exact snapshot,
    retention_token: opaque durable owner capability,
}

SnapshotLease {
    next_baseline: SnapshotBaseline for the selected immutable snapshot,
    invalidation: Full | ExactPaths(raw-relative-paths) | Prefixes(raw-prefixes),
    expires_boottime_ns: advertised monotonic lease deadline,
    scan_root: open read-only snapshot directory fd,
    session: private Renew/Promote/Finish capability,
}

ScanOutcome = Committed | Aborted
```

`Full` means traverse every path selected by the existing Jujutsu sparse and
tracking policy. `ExactPaths` narrows the scan to specified repository-relative
names. `Prefixes` permits subtree invalidation where an exact-name list would
be unavailable or too expensive.

The fd must refer to the exact advertised read-only Btrfs snapshot. Jujutsu
calls `ScanClient::validate_scan_root` before using it; production validation
reopens the fd's filesystem and subvolume identity through Btrfs ioctls.

## Private transport

The direct transport uses a Unix `SOCK_SEQPACKET` socket named `scan.sock`.
Each packet has a 16-byte header containing magic `BAWS`, protocol version,
operation, flags, descriptor count, and payload length. The supported
operations are:

```text
Begin  -> session ID, next baseline token, invalidation, boot-time deadline, identity, one fd
Renew  -> extend the session's durable query lease
Promote -> pin candidate B while retaining committed A
Finish -> Committed replaces A with B; Aborted drops pending B
```

A successful Begin transfers exactly one directory descriptor using
`SCM_RIGHTS`. Errors and Renew/Finish responses must not transfer descriptors.
The maximum payload is 1 MiB.

The library's default discovery executes:

```text
btrfs-awacs scan-sockname <absolute-live-root>
```

It expects one absolute socket path terminated by a NUL byte. An explicit
absolute socket override bypasses the discovery subprocess.

## Daemon-side ownership

`FacadeScanHandler::begin_scan` currently:

1. Canonicalizes and authorizes the requested live root.
2. Calls `FacadeService::prepare_scan_query`, which creates an immutable cut.
3. Resolves its exact managed snapshot path and opens the snapshot directory.
4. Loads filesystem/subvolume identity and extends its durable query lease.
5. Converts the projected invalidation and wraps the authenticated continuity
   token with the selected snapshot identity as `SnapshotBaseline`.
6. Stores the `PreparedQueryResult` in an active-session map.
7. Returns a session ID and the open directory fd.

Renew extends the prepared query lease. Promote installs a pending
consumer-baseline pin without removing the old committed pin. Finish
atomically promotes pending B to committed and removes A, or drops pending B
on abort, then releases the query lease. A restart reconciles an orphaned
pending pin against whichever exact baseline the caller's journal names.

The snapshot path is not a workspace identifier. AWACS resolves it internally
from the snapshot row selected by the watch/cut; Jujutsu associates that
snapshot with one workspace and semantic tree through its local journal plus
the opaque baseline owner ID. A mutable at-sign commit is not used as the key.

The handler resolves, authorizes, initializes or adopts, and activates each
requested canonical root on demand. Multiple repositories and snapshot
workspaces on the daemon's configured Btrfs filesystem can therefore share one
mount-namespace daemon without sharing baseline or grant authority.
