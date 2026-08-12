---
title: "External inputs and fingerprints"
description: "Ignore files, sparse profiles, executable policy, tracking configuration, and versioned scan fingerprints."
sidebar:
  order: 6
---
An immutable worktree snapshot does not freeze inputs stored outside the
worktree or ignored `.git`/`.jj` metadata. Jujutsu therefore stores a
domain-separated SHA-256 fingerprint beside each direct AWACS cursor.

The version-one fingerprint covers:

1. The selected absolute `core.excludesFile` or XDG Git ignore file and bytes.
2. The colocated Git `info/exclude` path and bytes.
3. Git sparse mode and effective index-derived sparse prefixes.
4. Jujutsu sparse prefixes.
5. The resolved `snapshot.auto-track` expression.
6. Fileset alias names and expressions.
7. The effective maximum new-file size.
8. End-of-line conversion policy.
9. Effective executable-bit policy.

Lists and aliases are canonicalized where appropriate. A changed fingerprint,
unknown fingerprint version, missing fingerprint, or backend change invalidates
the prior AWACS cursor.

Worktree-relative `core.excludesFile` is deliberately read from the selected
scan root instead of fingerprinted from the mutable live root. This distinction
must remain scoped to AWACS-aware snapshot construction; callers of existing
`base_ignores()` must continue to receive complete stock ignore behavior.

The fingerprint is meaningful only if it represents **the exact external bytes
used by the scan**. Reading an ignore file once to build the matcher and again
later to hash it creates a time-of-check/time-of-use race. The current
implementation has that race; see finding C-07.
