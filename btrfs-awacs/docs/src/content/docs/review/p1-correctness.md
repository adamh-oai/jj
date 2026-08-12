---
title: "P1 · Correctness and compatibility"
description: "P1 correctness review findings, affected components, failure mechanisms, and observed impact."
sidebar:
  order: 4
---
This page contains **16 P1 correctness findings**. Identifiers correspond to the reviewed source specification.

**C-06 — Retained-boundary cleanup is implemented; crash acceptance remains.**
`src/manager.rs` now treats surviving filesystem-monitor boundaries as the
ownership authority, deletes only boundary/cut/operation groups with no
surviving boundary, and does that work under one writer transaction while
excluding active query endpoints. Bounded orphan cleanup runs separately. The
remaining gate is crash/restart acceptance under the kernel-backed matrix.

---

**C-07 — External ignore fingerprinting can permanently poison a direct-scan
baseline.** `../jj/cli/src/cli_util.rs` first reads
Git ignore files into `base_ignores`, then rereads the same files for the AWACS
fingerprint. A change between those reads pairs a tree derived from the old
ignore contents with a cursor fingerprint representing the new contents. The
next command sees the same new fingerprint and no worktree event, so a newly
unignored file can remain missing or newly ignored private content can be
tracked.

---

**C-08 — Relative `core.excludesFile` regresses ordinary Jujutsu behavior.**
`../jj/cli/src/cli_util.rs` removes worktree-relative
global excludes from `base_ignores()` for every backend. The normal snapshot
path reapplies them through `scan_root_ignores`, but
`../jj/cli/src/commands/run.rs` and
`../jj/cli/src/merge_tools/diff_working_copies.rs`
explicitly provide an empty list. `jj run` and external diff-edit snapshots can
therefore include previously ignored generated or sensitive files even when
AWACS is disabled.

---

**C-09 — Relative global-ignore precedence is reversed for every backend.**
`../jj/cli/src/cli_util.rs` now chains repository
`info/exclude` into `base_ignores` before deferring a relative global
`core.excludesFile`.
`../jj/lib/src/local_working_copy.rs`
later appends that relative global file on top. Since Jujutsu's ignore matcher
uses the newest applicable rule, the lower-priority global ignore incorrectly
overrides the higher-priority repository exclude. This changes tracking or
silently includes/excludes private files under `none` and AWACS. A
live `fsmonitor.backend = "none"` comparison showed that Git and the installed
Jujutsu both reported an unignored candidate, while the current implementation
incorrectly reported a clean working copy.

---

**C-11 — Server lease expiry and advertised client deadline disagree.**
`src/scan_facade.rs` records wall-clock `now` before the
expensive snapshot cut, derives the durable/server expiry from that old time,
and advertises a fresh boot-time deadline only after the cut. A slow cut,
wall-clock adjustment, or suspend can expire the real server lease long before
Jujutsu's advertised renewal deadline.

---

**C-12 — A connected descriptor can carry the wrong namespace authority.**
`src/main.rs` authenticates the original socket connector rather than each
later sending process. An inherited or transferred connected descriptor can
therefore be reused by a same-UID process with a different mount namespace or
chroot. The direct endpoint can additionally transfer a private managed
snapshot fd under the original connector's authority.

---

**C-14 — The optional precision journal is recorded but not used by direct invalidation.**
`src/facade.rs` certifies and pins guard cursors, but projects
direct `historical_changes` using `project_events` rather than the existing
lease-aware precision-range projector in `src/compat.rs`.
Consequently the recursive inotify overhead does not improve direct
invalidation precision.

---

**C-16 — One slow direct Begin can expire unrelated active scans.**
`src/scan.rs` holds the global dispatcher mutex while
`src/scan_facade.rs` holds the shared facade mutex across
snapshot creation and historical comparison. Renew and Finish requests cannot
proceed until the entire cut completes. Once the blocked renewal finally runs,
session cleanup may already have removed the expired lease. The packet
transport has no read deadline, and Jujutsu joins the blocked renewal thread
while finishing or dropping its working-copy transaction; a stalled daemon can
therefore hang the command indefinitely while retaining its working-copy lock.

---

**C-17 — Widening a sparse snapshot workspace can commit missing files as
deletions.**
`../jj/cli/src/commands/workspace/add.rs`
records a full source-commit baseline before applying destination sparsity. A
Btrfs snapshot of a sparse source physically lacks its excluded tracked files,
but `--sparse-patterns=full` selects no later sparsity update, and
`../jj/lib/src/local_working_copy.rs`
only invents file-state entries during reset. Files unchanged between source
and destination are never materialized; a subsequent scan records them as
deletions, contrary to stock full-workspace behavior.

---

**C-18 — Workspace removal silently destroys unsnapshotted sibling edits.**
`../jj/cli/src/commands/workspace/remove.rs`
snapshots only the invoking workspace. It does not open, lock, snapshot,
inspect, or request confirmation for the target workspace before removing its
working-copy commit and recursively deleting its files. Tracked modifications
and untracked files created since the target's last Jujutsu command can be
irretrievably lost. A disposable-workspace reproduction confirmed that an
unsnapshotted file disappeared without any warning or confirmation.

---

**C-19 — Workspace removal follows replaced symlinks to unrelated directories.**
`../jj/cli/src/commands/workspace/remove.rs`
canonicalizes the stored target path and follows symlinks, but never reloads or
verifies the target workspace identity. A replaced workspace directory can
therefore cause recursive deletion of the active checkout or another unrelated
directory under the caller's permissions. A disposable-workspace reproduction
replaced the registered path with a symlink; removal reported success and
deleted an unrelated directory while leaving the actual workspace elsewhere.

---

**C-20 — Auto removal cannot remove ordinary directories on Btrfs.**
`../jj/cli/src/commands/workspace/remove.rs`
forgets the workspace before trying subvolume deletion for every target.
`../jj/cli/src/commands/btrfs.rs` classifies
an ordinary directory *on* Btrfs as an operation error instead of the
`Ok(false)` fallback case. The command leaves the directory behind after
deleting its durable workspace registration.

---

**C-21 — Optional Btrfs mode fails hard when the Btrfs executable is absent.**
`../jj/cli/src/commands/btrfs.rs` and
`../jj/cli/src/commands/workspace/add.rs`
convert a missing `btrfs` executable into an unconditional error. Consequently
`btrfs.enabled = "auto"` does not preserve ordinary add/clone/remove behavior on
systems without the tool; removal has already forgotten the workspace before
discovering the error.

---

**C-22 — Snapshot workspaces can violate the monitored parent's Btrfs
boundary invariant.**
`../jj/cli/src/commands/workspace/add.rs`
permits a destination underneath the current workspace. With snapshot mode,
that creates a nested Btrfs subvolume under the AWACS-monitored parent.
`src/service.rs` rejects nested-subvolume transitions on a
subsequent parent cut, so creating a child can break monitoring of the source
workspace as well as failing direct registration of the new child.

---

**C-23 — Parsed kernel stream identity proof is implemented; injected-stream
acceptance remains.** `src/service.rs` now carries v2 endpoint/header and
completion proof through normal and recovered manifests and compares
filesystem/source/target identities, ctransids, root IDs, persisted bytes,
footer counters, and ioctl counters before publication. Legacy streams remain
explicitly proof-less. The remaining gate is kernel-backed injected-stream
acceptance.

---

**C-25 — A failed Begin response can leave a snapshot pinned indefinitely.**
`src/scan_facade.rs` inserts an active session and retains
its prepared query before `src/scan.rs` sends the Begin response
and descriptor. A failed response disconnects without aborting that inserted
session. Expired sessions are now reclaimed by the independent bounded
maintenance scheduler as well as request traffic, so an idle daemon bounds the
leak. The response-failure path should still abort the inserted session
immediately.
