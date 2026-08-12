---
title: "P2 · Recurring overhead"
description: "P2 performance review findings, affected components, failure mechanisms, and observed impact."
sidebar:
  order: 7
---
This page contains **4 P2 performance findings**. Identifiers correspond to the reviewed source specification.

**P-10 — Sparse state and external inputs are repeatedly recomputed.**
`../jj/cli/src/cli_util.rs` parses the Git sparse
index for fingerprinting and the ordinary snapshot path parses it again. It
also rereads external ignore files and reruns executable-bit probing that
creates/chmods a temporary file even though `TreeState` already resolved that
policy.

---

**P-11 — Every direct command pays subprocess and OS-thread overhead.**
`src/scan.rs` runs a synchronous `btrfs-awacs scan-sockname`
process for default discovery.
`../jj/lib/src/local_working_copy.rs`
opens a new client and creates/joins a dedicated renewal thread even for a
short, unchanged scan.

---

**P-12 — Session cleanup scales quadratically under sustained command load.**
`src/scan_facade.rs` scans every active session and
five-minute completion tombstone on every Begin, Renew, and Finish. A high
command rate within one tombstone lifetime yields growing memory usage and
approximately quadratic cleanup work.

---

**P-13 — Install entry points are inconsistent.**
`install.sh` omits the `btrfs-awacs-watchman` symlink created by
`packaging/install.sh`. Both install executables under
`libexec` rather than a normal default `PATH`; direct discovery therefore
requires deployment-specific `PATH` or `BTRFS_AWACS_COMMAND` configuration.
