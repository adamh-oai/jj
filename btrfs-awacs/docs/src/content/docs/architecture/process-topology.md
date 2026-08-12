---
title: "Process topology and authority"
description: "The direct client, namespace daemon, privileged broker, and kernel boundaries."
sidebar:
  order: 2
---
```mermaid
flowchart TD
    JJ["Jujutsu command and working-copy transaction"]
    SCANCLIENT["AWACS direct ScanClient"]
    DISCOVERY["Namespace-scoped daemon discovery"]
    SCANSOCK["scan.sock: private sequenced-packet API"]
    SESSION["FacadeScanHandler: snapshot leases"]
    FACADE["FacadeService: cursors, continuity, query pins"]
    SERVICE["Service: initialize, cuts, indexing, recovery"]
    STORE["Manager SQLite: watches, revisions, grants, pins"]
    BROKER["Privileged broker and receipt journal"]
    KERNEL["Linux Btrfs snapshots and changed-object ioctl"]

    JJ --> SCANCLIENT
    SCANCLIENT --> DISCOVERY
    DISCOVERY --> SCANSOCK
    SCANCLIENT --> SCANSOCK
    SCANSOCK --> SESSION
    SESSION --> FACADE
    FACADE --> SERVICE
    SERVICE --> STORE
    SERVICE --> BROKER
    BROKER --> KERNEL
```

There are three authority domains:

1. **The client** owns the Jujutsu working-copy state.
2. **The per-user namespace daemon** owns watches, projections, scan sessions,
   continuity monitors, and the manager database connection.
3. **The privileged broker** performs constrained Btrfs operations and keeps a
   separate receipt journal for replayable filesystem effects.

A direct-scan request is not allowed to supply the broker's underlying
authority, manager database, managed-snapshot directory, or arbitrary
commands.
