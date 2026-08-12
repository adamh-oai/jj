---
title: "Non-goals and unsupported behavior"
description: "Explicit boundaries of the prototype, required kernel support, and unsafe assumptions."
sidebar:
  order: 3
---
The current implementation is not:

- A filesystem-independent watcher.
- A supported client of upstream/unmodified Btrfs kernels without the reviewed
  local changed-object and dirty-witness extensions.
- A claim that recursive inotify observes every content mutation mechanism.
- A proof that configurable retention, physical GC, or all crash recovery
  paths currently run in production.
- A safe reason to share mutable `.jj`, Git refs, indexes, or working-copy state
  between filesystem snapshots.
- A validated replacement for ordinary Jujutsu behavior until the defects and
  acceptance gates above are addressed.
