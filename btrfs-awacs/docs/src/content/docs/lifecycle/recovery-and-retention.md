---
title: "Recovery and retention"
description: "Crash replay, broker receipts, client continuity, history boundaries, pins, and intended snapshot retention."
sidebar:
  order: 3
---
Crash recovery must reconcile broker receipts and fenced manager operations
before retrying filesystem effects. A snapshot path may be adopted only when
its expected UUID, source, parent, flags, and durable intent agree. Stale spool
artifacts and unmanaged lookalike snapshots must not be silently trusted.

Facade continuity is separate from snapshot identity:

- The root-path monitor watches every relevant ancestor/component.
- The mount monitor retains and polls `/proc/self/mountinfo`.
- Root replacement, rename/restore, mount-over, monitor loss, grant revocation,
  or epoch/session replacement invalidate existing cursors.
- The optional recursive precision guard is a separate optimization. Its
  overflow or absence must not weaken the mandatory root/mount monitors.

Opaque direct cursors are HMAC-authenticated capabilities. Their claims
identify the store, watch, cursor epoch, owner grant, monitor session, exact
cut sequence, boundary kind, algorithm version, and target snapshot UUID.

Historical replay currently verifies the **exact retained cut sequence and
snapshot UUID**. The older claim in FIXES.md (`FIXES.md`) that replay accepts an
older `<=` boundary no longer describes this checkout.

The implementation contains physical garbage-collection, history-retention,
compaction, deletion-fencing, and lease-expiration helpers. However, the daemon
does not schedule the production maintenance path, and the existing compaction
cleanup violates retained-boundary foreign keys. Those features must therefore
be described as incomplete, not as enforced retention.
