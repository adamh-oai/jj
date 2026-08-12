---
title: "P0 · Critical performance"
description: "P0 performance review findings, affected components, failure mechanisms, and observed impact."
sidebar:
  order: 3
---
This page contains **2 P0 performance findings**. Identifiers correspond to the reviewed source specification.

**P-01 — Production retention and GC need sustained acceptance.**
The scan daemon now starts a bounded periodic maintenance worker on a separate
manager handle. It expires stale leases, applies retained-boundary policy,
reclaims orphan history, and drives one-at-a-time receipt-fenced snapshot
deletion. The remaining gate is sustained kernel-backed recovery and latency
validation under load.

---

**P-02 — Every clean or dirty status can flush the entire Btrfs filesystem.**
`src/broker.rs` calls `syncfs` after snapshot creation and
deletion. This waits for unrelated writes on the same filesystem, not merely
the monitored checkout or snapshot transaction. A nominally cheap clean
`jj status` can therefore block behind arbitrary concurrent filesystem traffic.
