---
title: "Validation and acceptance gates"
description: "Platform prerequisites, reproduced regressions, filesystem correctness, compatibility, scan leases, and performance."
sidebar:
  order: 2
---
## Build and platform prerequisites

Before any meaningful integration claim:

1. Correct the companion Jujutsu path dependency so plain Cargo metadata,
   default-feature builds, and AWACS-feature builds all resolve.
2. Build AWACS and AWACS-enabled Jujutsu on Linux with the required Btrfs and
   custom changed-object/dirty-witness support.
3. Keep an ordinary macOS/default Jujutsu build independent of the Linux-only
   AWACS implementation.
4. Supply a declared, runnable Linux end-to-end binary or replace the broken
   `run_e2e.sh` target with the actual supported harness.
5. Confirm installed `btrfs-awacs`, Watchman discovery, daemon, Git hook, and
   broker entry points are discoverable in the real deployment environment.

## Review-time verification

The current-source behavior was distinguished from ordinary source inspection
and from platform-limited tests:

- `cargo metadata --no-deps --format-version 1` in the actual Jujutsu checkout
  failed because `/Users/adamh/code/bsend-watch/Cargo.toml` does not exist.
  A disposable copy of the same source with only the expected sibling path
  supplied built its default-feature `jj` binary successfully; neither actual
  source checkout was changed to obtain that build.
- `cargo test --locked` for AWACS on the available macOS host reached Linux-only
  `syncfs`, socket-credential, inotify, and ABI references and failed with
  114 library compilation errors. This does not establish a Linux failure;
  AWACS-enabled execution still requires an appropriate Linux/Btrfs host.
- `cargo build --locked --bin btrfs-awacs-e2e` reported that the named target
  does not exist, directly confirming the broken advertised runner.
- With the freshly built current-source Jujutsu and disposable workspaces,
  `jj -R secondary workspace remove default` exited successfully, deleted the
  primary workspace's shared `.jj/repo` and colocated `.git`, and left the
  surviving secondary unable to open its repository.
- A disposable non-Btrfs workspace under `btrfs.enabled = "auto"` successfully
  fell back from a failed snapshot, but its inherited tracked file was absent
  despite being present in its recorded tree; the first ordinary `jj status`
  recorded the missing file as a deletion.
- Removing an unsnapshotted sibling deleted its only on-disk edit without
  warning. Replacing another registered sibling with a symlink caused workspace
  removal to delete the unrelated target directory instead.
- With `fsmonitor.backend = "none"`, a relative global ignore containing
  `candidate` and a repository `.git/info/exclude` containing `!candidate`
  produced `A candidate` with the installed Jujutsu, `?? candidate` with Git,
  and an incorrectly clean working copy with the freshly built implementation.

These reproductions verify the workspace and ignore failures independently of
the unavailable Linux-only AWACS runtime. The installed comparison Jujutsu and
current-source binary have different release versions; Git's independent
result establishes the intended ignore precedence without relying solely on
that version comparison.

## Workspace and stock-behavior regressions

On both ordinary filesystems and Btrfs, create a nonempty primary workspace and
multiple secondary workspaces. Verify that removing the primary, an ancestor
of the current workspace, a shared repository-store owner, a dirty sibling, or
a symlink-replaced target cannot destroy repository history or unsaved data.

Exercise `btrfs.enabled = "auto"` with a missing `btrfs` executable, a
non-Btrfs source, an ordinary directory on Btrfs, an existing empty
destination, and a destination on a different filesystem. Auto fallback must
materialize every tracked source/target file, preserve valid monitor
baselines, and retain workspace registration when cleanup cannot proceed.

Snapshot a source with a nontrivial sparse profile, request
`--sparse-patterns=full`, and verify every previously excluded tracked file is
materialized before the destination tree or monitor baseline is committed.

Test relative and absolute global ignore files against conflicting
`info/exclude` rules for `none`, Watchman, and AWACS. Run the same checks
through ordinary snapshots, `jj run`, and external diff editors.

## Core filesystem correctness

Exercise creation, deletion, content edits, executable bits, ownership/xattrs,
same-name replacement, inode reuse, hardlinks, hardlink alias removal,
directory moves, nested-subvolume insertion, fscrypt rejection, malformed
kernel streams, and exact historical replay. Compare immutable indexed results
with an independent complete scan of the same snapshot.

Inject crashes before/after broker intents, snapshot creation, receipt
completion, physical-head publication, comparison publication, and snapshot
deletion. Assert that restart either resumes a valid fenced operation or
terminally fails/quarantines an invalid one without wedging the watch.

## Live Watchman and Git compatibility

Run real Jujutsu and Git clients rather than only fabricated frame fixtures.
Pause a client after receiving clock B, create/delete or rename/restore files
and subtrees, then complete the live crawl before cut C. Compare monitored
results with fsmonitor-disabled full scans. Repeat with precision disabled,
enabled, gapped, overflowed, and restarted.

Cover hardlinks, `.gitignore` changes, directory moves, root/ancestor
rename-and-restore, mount-over/restore, clock copying across roots, malformed
expressions, response-write failure, trigger-disabled startup, and unsupported
trigger-enabled configurations.

## Direct Jujutsu scans

Use a real read-only Btrfs snapshot fd to verify:

- Mutations of the live root after Begin do not alter the tree read from the
  leased snapshot.
- Actual relative changed paths stay incremental rather than forcing `Full`.
- `.gitignore`, external excludes, sparse settings, EOL/exec policy,
  auto-tracking, hardlinks, symlinks, and untracked files match a full-scan
  oracle for the selected immutable snapshot.
- Ignore matcher contents and the persisted fingerprint come from the same
  immutable external-input read.
- Tree-state save failure, checkout/reset/sparse mutation, dropped commands,
  daemon restart, expired leases, response-send failure, and renewal failure
  never persist an invalid cursor or leak an indefinitely pinned snapshot.
- Multiple repositories and Btrfs snapshot workspaces in one namespace receive
  independent watches, grants, leases, and cursors from the same daemon.
- Invalid descriptor identity, malformed paths, cross-root cursors, and
  transferred/inherited connections fail closed.
- Alternating upstream/custom Jujutsu binaries preserves ordinary Watchman
  cursor compatibility.

## Performance and resource limits

Measure clean status, one-file edits, directory renames, sparse monorepos,
first initialization, adopted snapshot workspaces, full-fresh recovery,
concurrent clients, and unrelated write pressure on the same filesystem.
Report snapshot latency, changed-object calls, flush time, SQLite writer time,
full-tree traversals, retained snapshots, open fds, OS threads, session count,
tombstones, memory, and p50/p95/p99 command latency.

Run sustained workloads long enough to verify that configured retention is
actually enforced. Test slow/nonreading peers and stalled brokers, and prove
that unrelated scan renewals do not expire behind another client's cut.
