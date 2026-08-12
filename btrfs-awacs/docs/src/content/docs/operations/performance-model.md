---
title: "Performance and resource model"
description: "Expected complexity, changed-object work, snapshot costs, SQLite contention, and monitored resource budgets."
sidebar:
  order: 1
---
Let `N` be repository namespace size, `D` the directory count, `K` the changed
path/object count, `H` hardlink alias expansion, `S` sparse Git index entries,
and `Q` the number of concurrent or recently completed scan sessions.

| Operation | Intended cost | Current implementation caveat |
| --- | --- | --- |
| First watch initialization | `O(N)` | Full immutable index construction and SQLite checkpoint import are unavoidable once per unshared root. |
| Existing snapshot-descendant adoption | Approximately independent of `N` when the parent revision exists | Missing lineage silently falls back to full `O(N)` initialization. |
| Clean status | Snapshot/cut overhead plus small metadata work | Each cut performs filesystem-wide `syncfs`; direct discovery also forks a subprocess and creates a renewal thread. |
| Small incremental change | Approximately `O(K + H)` plus cut/index overhead | A relative-path mismatch currently turns every nonempty direct invalidation into an `O(N)` Jujutsu traversal. |
| Adjacent compatibility query | Reuse already-published adjacent events | The facade re-runs a historical kernel comparison even when the cut just produced the same events. |
| Sparse AWACS status | Depend on changed paths and sparse-selection metadata | The same sparse Git index is parsed twice, with work proportional to entries and ancestor depth. |
| Directory rename | Prefix invalidation or bounded subtree expansion | Generic projection upgrades all subtree moves to a full crawl, including moves that are later filtered as metadata. |
| Optional precision guard | `O(D)` setup and bounded mutation work | One inotify watch per directory and a synchronously durable marker per certified cut; client projection does not consume the resulting journal. |
| Retained history and snapshots | Bounded by configured policy and pins | The production daemon never schedules garbage collection or retention maintenance. |
| Session cleanup | Bounded or amortized by expiry | Each Begin/Renew/Finish scans active sessions and 300-second tombstones; repeated commands can accumulate quadratic cleanup work. |

The dominant existing costs are not subtle micro-optimizations: filesystem-wide
flushes, never-reclaimed snapshots, repeated kernel comparisons, full-tree
crawls on every edit, and globally serialized leases can dominate the intended
benefit of a filesystem monitor.
