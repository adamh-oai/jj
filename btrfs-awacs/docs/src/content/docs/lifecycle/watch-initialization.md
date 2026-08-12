---
title: "Watch initialization"
description: "Binding a live root, creating the initial immutable cut, indexing it, and establishing durable watch identity."
sidebar:
  order: 1
---
The first registration of an unindexed root proceeds as follows:

1. Resolve and verify the exact Btrfs subvolume root and its filesystem.
2. Authorize the requesting UID/GID and reserve a watch, grant, fenced
   operation, and deterministic managed snapshot destination.
3. Ask the broker to create a read-only snapshot of the live root.
4. Reopen the snapshot and verify filesystem UUID, subvolume UUID, parent
   identity, read-only status, and expected destination.
5. Build a complete index from the immutable snapshot, not from the changing
   live root.
6. Validate graph connectivity, aliases, ownership/security metadata, supported
   boundaries, and canonical checkpoint data.
7. Publish revision zero, independent indexed-head and physical-head pins, the
   active watch, and its initial grant.
8. Arm the mandatory root-path and mount-topology monitors before exposing any
   client clock.

Sequence zero initializes the **core watch**. It is not, by itself, a
Watchman/direct-scan clock boundary. The first client-visible clock is created
by a subsequent synchronized cut and facade finalization.

An already-created Btrfs snapshot descendant can sometimes adopt a retained
parent revision. This reuses index data but still creates an independent watch,
grant, identity, and eventual client boundary. It does not make two Jujutsu
workspaces share mutable `.jj` or `.git` state.
