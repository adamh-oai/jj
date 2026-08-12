---
title: "P2 · Compatibility and lifecycle"
description: "P2 correctness review findings, affected components, failure mechanisms, and observed impact."
sidebar:
  order: 6
---
This page contains **4 P2 correctness findings**. Identifiers correspond to the reviewed source specification.

**C-26 — Malformed direct invalidations are silently dropped.**
`../jj/lib/src/local_working_copy.rs`
uses `filter_map` when converting raw direct invalidation paths to Jujutsu
repository paths. A malformed/nonrepresentable entry can become an empty
matcher while its new cursor is still committed. Direct responses must reject
invalid paths or conservatively force `Full`.

---

**C-28 — Auto snapshot creation rejects supported existing empty destinations.**
`../jj/cli/src/commands/workspace/add.rs`
rejects an existing destination before checking whether an optional snapshot
should fall back to ordinary creation. Stock Jujutsu accepts an existing empty
workspace directory, so optional optimization changes existing behavior.

---

**C-29 — Auto snapshots fail instead of falling back across filesystems.**
`../jj/cli/src/commands/workspace/add.rs`
checks only whether the source is a Btrfs subvolume after a failed snapshot.
When the destination is on another filesystem, the source still passes that
check and the optional mode reports an error rather than using ordinary
workspace creation.

---

**C-30 — Removing a colocated workspace leaves a stale Git worktree.**
`../jj/cli/src/commands/workspace/add.rs`
registers an independent linked Git worktree, but
`../jj/cli/src/commands/workspace/remove.rs`
deletes its directory without running Git worktree removal or pruning the
matching administrative state.
