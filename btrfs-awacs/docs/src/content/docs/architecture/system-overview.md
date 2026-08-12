---
title: "System overview"
description: "What AWACS implements, its runtime requirements, and why direct scans use immutable snapshots."
sidebar:
  order: 1
---
## Purpose and status

AWACS maintains a persistent, snapshot-based change index for Btrfs
subvolumes. Its clients can ask which repository paths changed between
authenticated immutable filesystem cuts. Jujutsu can additionally lease the
actual read-only snapshot and build its working-copy tree from that snapshot
instead of racing a mutable checkout.

This document describes the **implementation that currently exists** in this
repository and its companion Jujutsu checkout at `../jj`. It distinguishes
implemented mechanisms, intended invariants, verified defects, and unsupported
features. It does not treat a schema table, a helper function, a design
proposal, or an environment-gated test as proof that a production path works.

The older [indexed change-tracking design](/reference/indexed-change-tracking/)
contains the normative Btrfs/index/database design and SQL schema. The companion
Jujutsu scan design (`../jj/docs/design/awacs-snapshot-scans.md`) describes the
direct-scan integration. FIXES.md (`FIXES.md`) records an earlier audit, but some
of its findings have since been fixed; the current verified issues are in
[Section 21](/review/overview/).

AWACS requires Linux, Btrfs, an appropriate privileged broker, and the
experimental Btrfs changed-object/dirty-witness kernel support. The direct
Jujutsu backend additionally requires a Jujutsu binary built with its optional
`awacs` Cargo feature. The ordinary Jujutsu default remains
`fsmonitor.backend = "none"` and `btrfs.enabled = false`.

## The direct immutable-scan answer

Ordinary Jujutsu snapshots read files from the live working-copy directory.
Without AWACS, Jujutsu traverses selected repository paths in that mutable
directory. The direct AWACS backend instead receives a pinned read-only
snapshot directory fd, authenticated cursor, invalidation, and lease, then
reads the immutable snapshot through `/proc/self/fd/N`.

The core correctness obligation is simple to state and strict to implement:
Jujutsu must read one exact immutable snapshot and persist its cursor only with
the tree state derived from that same snapshot. Descriptor identity, lease
lifetime, transaction ordering, and external-input fingerprints are therefore
part of correctness rather than optional optimizations.
