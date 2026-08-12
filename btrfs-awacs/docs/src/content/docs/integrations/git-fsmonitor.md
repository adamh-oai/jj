---
title: "Git fsmonitor integration"
description: "Native Git hook-v2 tokens, NUL-delimited invalidations, tracked state, and mutable checkout refresh."
sidebar:
  order: 2
---
Git invokes the multicall `git-fsmonitor-hook` executable with:

```text
git-fsmonitor-hook 2 OLD_TOKEN
```

The hook connects to `watchman.sock`, sends `watch-project` for the Git
worktree, then sends a restricted Watchman `query`. It translates the response
into Git's native hook-v2 framing:

```text
NEW_TOKEN NUL CHANGED_PATH NUL CHANGED_PATH NUL ...
```

An empty, numeric, unknown, or foreign token requires a fresh/full
invalidation. `.git` paths are excluded. The current Git adapter is "native"
at the client protocol boundary, but internally still depends on the focused
Watchman socket; it is not an independent single-request Git daemon protocol.

Unlike the direct scan socket, the existing Git socket wrapper sets bounded
read/write deadlines.
