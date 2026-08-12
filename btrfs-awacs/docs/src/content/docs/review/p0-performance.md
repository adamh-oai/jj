---
title: "P0 · Critical performance"
description: "P0 performance review findings, affected components, failure mechanisms, and observed impact."
sidebar:
  order: 3
---
This page contains **2 P0 performance findings**. Identifiers correspond to the reviewed source specification.

**P-01 — Production snapshots are never garbage-collected.**
`src/service.rs` defines `garbage_collect` and
`maintain_history`, but no daemon path invokes them. Every status/query creates
a managed snapshot, and configured replay retention fields are never enforced.
Long-lived use therefore retains snapshots, indexes, events, SQLite rows, and
copy-on-write extents without a bound.

---

**P-02 — Every clean or dirty status can flush the entire Btrfs filesystem.**
`src/broker.rs` calls `syncfs` after snapshot creation and
deletion. This waits for unrelated writes on the same filesystem, not merely
the monitored checkout or snapshot transaction. A nominally cheap clean
`jj status` can therefore block behind arbitrary concurrent filesystem traffic.
