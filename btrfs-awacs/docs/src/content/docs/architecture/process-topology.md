---
title: "Process topology and authority"
description: "Clients, namespace daemons, compatibility endpoints, the privileged broker, and kernel boundaries."
sidebar:
  order: 2
---
```mermaid
flowchart TD
    JJ["Jujutsu command and working-copy transaction"]
    GIT["Git fsmonitor hook v2"]
    WMCLIENT["Jujutsu Watchman client"]
    SCANCLIENT["AWACS direct ScanClient"]
    DISCOVERY["Namespace-scoped daemon discovery"]
    WATCHSOCK["watchman.sock: focused BSER stream"]
    SCANSOCK["scan.sock: private sequenced-packet API"]
    ENDPOINT["WatchmanEndpoint and Git adapter"]
    SESSION["FacadeScanHandler: snapshot leases"]
    FACADE["FacadeService: clocks, continuity, query pins"]
    SERVICE["Service: initialize, cuts, indexing, recovery"]
    STORE["Manager SQLite: watches, revisions, grants, pins"]
    BROKER["Privileged broker and receipt journal"]
    KERNEL["Linux Btrfs snapshots and changed-object ioctl"]

    JJ --> WMCLIENT
    JJ --> SCANCLIENT
    WMCLIENT --> DISCOVERY
    SCANCLIENT --> DISCOVERY
    GIT --> WATCHSOCK
    DISCOVERY --> WATCHSOCK
    DISCOVERY --> SCANSOCK
    WMCLIENT --> WATCHSOCK
    SCANCLIENT --> SCANSOCK
    WATCHSOCK --> ENDPOINT
    SCANSOCK --> SESSION
    ENDPOINT --> FACADE
    SESSION --> FACADE
    FACADE --> SERVICE
    SERVICE --> STORE
    SERVICE --> BROKER
    BROKER --> KERNEL
```

There are three authority domains:

1. **The client** owns the Jujutsu working-copy state or Git index.
2. **The per-user namespace daemon** owns watches, projections, scan sessions,
   continuity monitors, and the manager database connection.
3. **The privileged broker** performs constrained Btrfs operations and keeps a
   separate receipt journal for replayable filesystem effects.

Neither a Watchman request nor a direct-scan request is allowed to supply the
broker's underlying authority, manager database, managed-snapshot directory, or
arbitrary commands.
