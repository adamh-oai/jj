---
title: "System overview"
description: "What AWACS implements, its runtime requirements, and why immutable direct scans differ from live Watchman monitoring."
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

## The problem and the two different answers

Ordinary Jujutsu snapshots read files from the live working-copy directory.
Without a monitor, Jujutsu traverses the selected repository paths. With
Watchman, it requests changed paths and a clock, narrows the traversal, and
still reads the live directory.

AWACS offers two materially different integrations:

| Integration | What the client receives | Where Jujutsu/Git reads files | Main correctness obligation |
| --- | --- | --- | --- |
| Focused Watchman compatibility | Changed names and an authenticated clock | Mutable live checkout | Report every path the client might have cached after its previous clock, including transient namespace changes. |
| Native Git hook v2 | An authenticated token and NUL-delimited changed names | Mutable live checkout/index refresh | Conservatively invalidate Git's tracked/untracked/index state, including directory and transient changes. |
| Direct Jujutsu backend | A pinned read-only snapshot directory fd, authenticated cursor, invalidation, and lease | Immutable snapshot via `/proc/self/fd/N` | Read one exact immutable snapshot; persist its cursor only with the tree state derived from that same snapshot. |

The direct backend is **not** an alternate encoding of the Watchman protocol.
Its immutable scan root eliminates a class of live-crawl races, but introduces
descriptor identity, lease lifetime, transaction, and external-input
fingerprint requirements.
