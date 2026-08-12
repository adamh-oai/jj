---
title: "Source reading guide"
description: "Suggested entry points through AWACS scan, facade, service, manager, and Jujutsu working-copy internals."
sidebar:
  order: 1
---
Start with `src/scan.rs` for the public direct client contract,
then `src/scan_facade.rs` for daemon-side ownership,
`src/facade.rs` for authenticated cuts and query pins,
`src/service.rs` for snapshot/index orchestration, and
`src/manager.rs` for durable state transitions.

On the Jujutsu side, read
`../jj/lib/src/fsmonitor.rs` for configuration and
fingerprints, `../jj/cli/src/cli_util.rs` for the
actual external inputs, and
`../jj/lib/src/local_working_copy.rs` for
immutable traversal, pending leases, backend-tagged cursors, and the
save-before-Finish transaction boundary.
