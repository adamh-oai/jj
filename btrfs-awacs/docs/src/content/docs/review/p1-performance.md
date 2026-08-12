---
title: "P1 · Performance and scaling"
description: "P1 performance review findings, affected components, failure mechanisms, and observed impact."
sidebar:
  order: 5
---
This page contains **7 P1 performance findings**. Identifiers correspond to the reviewed source specification.

**P-03 — Every genuinely changed direct scan becomes a full repository crawl.**
`src/index.rs` and `src/compat.rs` produce
repository-relative paths without a leading slash.
`src/scan_facade.rs` nevertheless requires every direct
path to begin with `/`; any normal nonempty result therefore becomes
`Invalidation::Full`. Its unit test uses impossible slash-prefixed paths, so it
does not exercise the actual integration boundary.

---

**P-04 — Adjacent changes are compared twice.**
`src/facade.rs` requests `historical_changes` even when
`Service::changes` has just produced and persisted the same adjacent delta and
`PublishedCut.events`. This repeats the privileged changed-object comparison,
spooling, hashing, target lookup, and database work on the common incremental
path.

---

**P-05 — Cut coalescing misses the expensive part of the cut.**
`src/manager.rs` joins only operations still in the fleeting
`planned` state. Requests arriving after the operation becomes `fs_started`
cannot join the in-flight Btrfs snapshot, `syncfs`, or comparison, so concurrent
status calls queue more expensive cuts instead of sharing them.

---

**P-06 — Daemon connections and direct packet buffers are unbounded.**
`src/main.rs` creates one OS thread per accepted client.
`src/scan.rs` allocates a roughly 1 MiB receive buffer before
blocking on every idle direct connection. There is no direct read/write
deadline or connection cap; a nonreading peer can also hold the global handler
mutex during a blocked response write.

---

**P-07 — Required full-fresh/compaction paths perform avoidable whole-tree
work.** `src/manager.rs` enumerates every path into an event
list even when a full-invalidation sentinel would suffice, and hydrates/hashes
an entire revision before checking whether its checkpoint is already ready.
Directory moves also force global fresh traversal before irrelevant metadata
paths can be filtered.

---

**P-08 — The advertised end-to-end runner cannot build its target.**
`run_e2e.sh` requests `--bin btrfs-awacs-e2e`, but
`Cargo.toml` disables automatic binaries and declares only
`btrfs-awacs`. Existing Linux/Btrfs Jujutsu integration tests are also
environment-gated, so a passing ordinary unit invocation would not by itself
prove real-client interoperability.

---

**P-09 — Snapshot workspace creation recursively deletes copied repository
metadata.**
`../jj/cli/src/commands/workspace/add.rs`
creates a copy-on-write snapshot of the whole source root and then recursively
removes its copied `.jj` and `.git` directories. A colocated monorepo can have
hundreds of thousands of metadata/object entries, converting an intended cheap
snapshot into a large tree walk and Btrfs copy-on-write metadata churn.
