---
title: "Direct immutable-snapshot API"
description: "The Begin, Renew, and Finish protocol, authenticated cursors, immutable snapshot fds, and daemon sessions."
sidebar:
  order: 3
---
## Public request/result contract

`src/scan.rs` exposes the transport-independent API consumed by
Jujutsu:

```text
BeginScanRequest {
    live_root: absolute live working-copy path,
    previous_cursor: optional opaque authenticated cursor,
}

ScanLease {
    cursor: opaque cursor for the selected immutable snapshot,
    invalidation: Full | ExactPaths(raw-relative-paths) | Prefixes(raw-prefixes),
    identity: filesystem UUID + subvolume UUID + read-only flag,
    expires_boottime_ns: advertised monotonic lease deadline,
    scan_root: open read-only snapshot directory fd,
    session: private Renew/Finish capability,
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
Begin  -> session ID, cursor, invalidation, boot-time deadline, identity, one fd
Renew  -> extend the session's durable query lease
Finish -> Committed or Aborted; release the pinned prepared response
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
5. Converts the projected invalidation and wraps the authenticated cursor.
6. Stores the `PreparedQueryResult` in an active-session map.
7. Returns a session ID and the open directory fd.

Renew extends the prepared query lease. Finish releases it and records a short
idempotence tombstone. Invalid/expired cursors currently become safe full scans
when the selected target snapshot can still be leased.

The handler resolves, authorizes, initializes or adopts, and activates each
requested canonical root on demand. Multiple repositories and snapshot
workspaces on the daemon's configured Btrfs filesystem can therefore share one
mount-namespace daemon without sharing cursor or grant authority.
