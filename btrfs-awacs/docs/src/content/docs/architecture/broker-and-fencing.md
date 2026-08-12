---
title: "Privileged broker and fencing"
description: "Capability boundaries, operation fencing, descriptor identity, privileged filesystem effects, and receipts."
sidebar:
  order: 6
---
The broker receives a constrained `SOCK_SEQPACKET` protocol rather than shell
commands. Its fixed operations include:

- Session handshake and manager identity fencing.
- Filesystem/subvolume inspection.
- Read-only snapshot creation and managed snapshot deletion.
- Complete immutable index construction and target-object lookup.
- Changed-object comparison of two verified immutable snapshots.
- Receipt inspection and reconciliation after interrupted operations.

Requests carry already-open fds and expected filesystem/subvolume identities.
The broker verifies source UUIDs, target locators, read-only flags, output-file
properties, and manager session ownership. Snapshot creation and deletion use
durable receipts because a Btrfs mutation and a SQLite commit cannot form one
atomic transaction.

The intended effect ordering is:

```text
persist fenced intent
    -> run one authorized filesystem effect
    -> verify its exact resulting identity
    -> make the effect sufficiently durable
    -> persist its receipt
    -> allow the manager to publish the result
```

The present implementation uses filesystem-wide `syncfs` for that durability
step; the performance consequences are described below.
