---
title: "P0 · Critical correctness"
description: "P0 correctness review findings, affected components, failure mechanisms, and observed impact."
sidebar:
  order: 2
---
This page contains **4 P0 correctness findings**. Identifiers correspond to the reviewed source specification.

**C-01 — Removing the primary workspace can delete the entire shared
repository.**
`../jj/cli/src/commands/workspace/remove.rs`
rejects only removal of the current workspace *name*. From any secondary
workspace, `jj workspace remove default` therefore removes the primary
registration and recursively deletes the primary directory or Btrfs
subvolume. `../jj/lib/src/workspace.rs` shows that
secondary workspaces merely point to the primary `.jj/repo`; deleting the
primary removes the shared operation store, repository history, and possibly
the colocated Git object database for every remaining workspace. No target
ancestry/shared-store check prevents this ordinary destructive command. A
disposable-repository reproduction exited successfully, deleted the primary
`.jj` and `.git`, and left the surviving secondary unable to open its shared
repository.

---

**C-02 — Automatic snapshot fallback can silently create a mass-deletion
workspace.**
`../jj/cli/src/commands/workspace/add.rs`
captures a source commit before attempting an optional Btrfs snapshot. When
auto mode falls back to an ordinary empty destination, it clears the snapshot
boolean but retains that snapshot-only source baseline. It then resets the new
working-copy tree to the source commit without writing its files.
`../jj/lib/src/local_working_copy.rs`
confirms that `TreeState::reset` updates state without materializing content.
The next full scan sees missing tracked files as deletions, and a newly
recorded direct baseline can hide the mismatch.
The existing fallback test uses an empty source tree, masking the defect. In a
live non-Btrfs fallback reproduction, workspace creation succeeded while an
inherited tracked file was absent; its first `jj status` recorded that file as
deleted.

---

**C-03 — The companion Jujutsu checkout cannot resolve any Cargo build.**
`../jj/Cargo.toml` points its workspace dependency to the
nonexistent `../bsend-watch` instead of this actual `../btrfs-awacs` sibling.
Cargo resolves workspace path dependencies even when AWACS is optional or
disabled. `cargo metadata --no-deps --format-version 1` fails before any
Jujutsu build or test can begin.

---

**C-05 — An invalid snapshot can permanently wedge every backend for a watch.**
`src/service.rs` publishes the physical snapshot head before
all nested-subvolume and fscrypt validation. A validation error leaves the
operation in `manifest_ready` and the invalid immutable target as the physical
head. The existing `fail_cut_comparison` terminal transition is not called by
production; restart repeatedly retries the permanently invalid snapshot.
