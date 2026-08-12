---
title: "Watchman compatibility"
description: "Focused Watchman commands, BSER transport, authenticated clocks, dirty paths, and live-crawl correctness."
sidebar:
  order: 1
---
The supported configuration is:

```toml
[fsmonitor]
backend = "watchman"

[fsmonitor.watchman]
register-snapshot-trigger = false
```

The AWACS multicall `watchman` entry point supports discovery. The namespace
daemon publishes `watchman.sock`, and Jujutsu's existing `watchman_client`
connects through the normal Watchman client path.

The intentionally limited command set is:

1. `watch-project(ROOT)`: validate or dynamically register/adopt an exact root.
2. `query(ROOT, OPTIONS)`: create a synchronized cut, resolve the prior clock,
   project changed names, apply a restricted expression, and return a new clock.
3. `clock(ROOT, OPTIONS)`: publish a synchronized clock without returning a
   changed-name list.
4. Fixed `trigger-del`: currently return a compatibility-only synthetic
   `deleted: false` response.

Queries are limited to the fields and expressions expected by the reviewed
Jujutsu/Git clients. This is not a general Watchman server. `trigger`,
`trigger-list`, subscriptions, SCM clocks, arbitrary expressions, and
background-trigger execution are unsupported.

On an ordinary Watchman failure, Jujutsu can warn and fall back to a live full
scan without trusting an unproved monitor clock. That fallback is distinct
from the direct AWACS backend's fail-closed behavior.

The facade allocates a `PreparedQueryResult`, pins the relevant immutable
inputs, and is expected to release its query lease only after response
serialization/writing finishes. Error paths after response allocation need the
same release guarantee.
