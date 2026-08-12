---
title: "Component responsibilities"
description: "Source ownership and responsibilities across the AWACS daemon and companion Jujutsu implementation."
sidebar:
  order: 3
---
## AWACS source ownership

| Component | Source | Responsibilities |
| --- | --- | --- |
| Btrfs identity and kernel calls | `src/btrfs.rs` | Open subvolume roots, inspect filesystem/subvolume UUIDs and generation, create/destroy snapshots, and invoke legacy/v2 changed-object interfaces. |
| Kernel stream parser | `src/manifest.rs` | Parse versioned records, changed-object masks, hardlink/reference changes, target attributes, nested-subvolume transitions, and completion data. |
| Immutable namespace inspection | `src/tree_index.rs` | Read a complete immutable inode/reference index or materialize requested target objects and security metadata. |
| Logical inode graph | `src/index.rs` | Represent objects and reference edges, validate reachability, resolve all hardlink aliases, apply manifests, and produce semantic path events. |
| Database bootstrap | `src/store.rs` | Create/open manager and broker SQLite databases, configure WAL/foreign keys, extract the normative SQL schema from the design document, migrate, and load clock-key metadata. |
| Durable manager | `src/manager.rs` | Own watches, grants, snapshot identities/pins, operation fencing, cut admission, revisions/checkpoints/overlays, client boundaries, query leases, retention, and recovery transitions. |
| Privileged filesystem execution | `src/broker.rs` | Validate expected fds/identities, execute constrained filesystem effects, fence sessions, and persist root-owned operation receipts. |
| Broker wire protocol | `src/broker_protocol.rs` | Authenticate broker sessions and encode fd-passing requests for snapshot creation/deletion, full indexing, target lookup, and changed-object comparison. |
| Core orchestration | `src/service.rs` | Initialize a watch, adopt an existing snapshot descendant, create cuts, stage/apply comparisons, recover unfinished effects, and expose maintenance helpers. |
| Mandatory namespace continuity | `src/namespace.rs` | Bind the exact root and mount view, watch ancestor/root-path identity, observe mount-topology changes, and reject ABA/continuity loss. |
| Optional precision journal | `src/precision.rs` | Recursively watch directories with inotify, certify ordered marker-delimited intervals, and persist exact mutation hints or mark an epoch gapped. |
| Clock and path compatibility | `src/compat.rs` | Authenticate opaque Watchman clocks and domain-separated direct cursors, project semantic events, and provide the presently unused precision-aware range projector. |
| Client-visible snapshot facade | `src/facade.rs` | Activate a monitored view, verify continuity, request cuts, resolve exact historical baselines, mint clocks, pin response inputs, and release/renew query leases. |
| BSER codec | `src/bser.rs` | Bound and encode/decode the small BSER-v2 value subset used by the Watchman endpoint. |
| Watchman semantics | `src/watchman.rs` | Register roots, dynamically adopt/initialize compatible roots, implement `watch-project`, `clock`, restricted `query`, and compatibility-only `trigger-del`. |
| Watchman transport | `src/watchman_transport.rs` | Frame Unix-stream BSER requests, inspect connected-peer identity, authorize namespace/root access, and bound response writes. |
| Git integration | `src/git_fsmonitor.rs` | Validate hook protocol v2, issue focused Watchman registration/query requests, exclude `.git`, and produce NUL-framed Git responses. |
| Public direct-scan API | `src/scan.rs` | Define request/result/error traits, discover the scan socket, authenticate private packet framing, pass one snapshot fd with `SCM_RIGHTS`, and issue Begin/Renew/Finish requests. |
| Direct-scan daemon bridge | `src/scan_facade.rs` | Bind requests to a live root, retain pinned prepared responses in an active-session registry, convert projections, renew/release leases, and remember completed session IDs. |
| Executable and activation | `src/main.rs` | Provide multicall CLI entry points, start/discover the namespace daemon, configure broker/state/snapshot paths, publish both sockets, authenticate clients, and dispatch connections. |

`src/trigger.rs` is dormant scaffolding: it is not exported by
`src/lib.rs`, and its presence does not imply functioning
Watchman trigger execution.

## Jujutsu source ownership

The corresponding checkout contains two related but distinct additions:
Btrfs-backed workspace materialization and the direct AWACS snapshot backend.

| Component | Source | Responsibilities |
| --- | --- | --- |
| Optional dependency and feature | `../jj/Cargo.toml`, `../jj/lib/Cargo.toml`, `../jj/cli/Cargo.toml` | Declare `btrfs-awacs`, expose `jj-lib/awacs`, and expose the CLI's nondefault `awacs` feature. |
| Monitor settings and fingerprint | `../jj/lib/src/fsmonitor.rs` | Parse `none`, `watchman`, and `awacs`; represent the AWACS socket/client; compute versioned, canonical SHA-256 external-input fingerprints. |
| Public snapshot options | `../jj/lib/src/working_copy.rs` | Carry ignore sources, sparse/tracking matchers, size limits, and the optional AWACS fingerprint into a working-copy snapshot. |
| Persisted monitor cursors | `../jj/lib/src/protos/local_working_copy.proto` | Store a backend-tagged `FsmonitorCursor`; preserve a deprecated legacy Watchman field for reading older state. |
| Working-copy scan and transaction | `../jj/lib/src/local_working_copy.rs` | Choose the live or immutable scan root, translate invalidations into matchers, traverse/read files, validate descriptors, renew scan leases, and save cursor/tree state transactionally. |
| CLI snapshot preparation | `../jj/cli/src/cli_util.rs` | Assemble Git/Jujutsu ignore rules, sparse state, tracking policy, effective executable/EOL policy, and AWACS fingerprint before command snapshotting. |
| Btrfs detection and operations | `../jj/cli/src/commands/btrfs.rs` | Inspect Btrfs paths/subvolume roots, invoke the `btrfs` CLI, and identify mount options relevant to unprivileged deletion. |
| Btrfs-backed workspace creation | `../jj/cli/src/commands/workspace/add.rs` | Optionally snapshot the source checkout, replace copied `.jj` identity, create an independent linked Git worktree, and establish the new workspace baseline. |
| Workspace removal | `../jj/cli/src/commands/workspace/remove.rs` | Forget the workspace, then remove a Btrfs subvolume or ordinary directory according to configuration. |
| Subvolume-backed cloning | `../jj/cli/src/commands/git/clone.rs` | Optionally create a new clone destination as a Btrfs subvolume. |

The direct backend must be optional and must not alter existing `none` or
ordinary Watchman behavior. Several current deviations from that requirement
are documented below.
