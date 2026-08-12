---
title: "Jujutsu backend configuration"
description: "Feature gates, monitor selection, namespace-scoped discovery, optional sockets, and stock defaults."
sidebar:
  order: 4
---
The defaults are:

```toml
[fsmonitor]
backend = "none"

[fsmonitor.watchman]
register-snapshot-trigger = false

[fsmonitor.awacs]
socket = ""

[btrfs]
enabled = false
```

The `jj-cli` default features are `watchman` and `git`; `awacs` is an explicit
additional feature that forwards to `jj-lib/awacs`. On Linux, a binary built
with that feature accepts:

```toml
[fsmonitor]
backend = "awacs"

[fsmonitor.awacs]
# Empty means AWACS-owned discovery for the live root and mount namespace.
socket = ""
```

An absolute socket path may be supplied instead. Other platforms, or Jujutsu
builds without the feature, reject the `awacs` setting with a configuration
error. A configured direct backend fails closed when discovery, connection,
snapshot identity, or an active lease cannot be verified.

**Current build caveat:** the companion checkout's workspace dependency
incorrectly names `../bsend-watch` instead of the actual sibling
`../btrfs-awacs`. Cargo resolves that path even when the optional feature is
disabled, so the current Jujutsu checkout cannot build any feature set until
the dependency path is corrected.
