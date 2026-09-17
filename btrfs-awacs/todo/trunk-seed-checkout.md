# Up-to-date trunk checkout as a worktree/workspace seed

Status: design intent; not implemented.

## Motivation

Btrfs makes copying a checkout cheap, but it does not make changing that copy
to a substantially different revision cheap. When the source is an old or
long-lived development checkout, creating a worktree at current trunk can
require a large Git reset. JJ adoption then has to reconcile those filesystem
changes against the inherited semantic tree and AWACS baseline.

Maintain a dedicated, up-to-date trunk checkout so ordinary worktree and
workspace creation starts close to the requested revision. Pay for advancing
trunk once per seed refresh, rather than independently in every new child.
The objective is to reduce both checkout work and subsequent change replay,
not merely the already-cheap Btrfs snapshot operation.

## Intended model

- Manage a clean Btrfs-backed seed separately from users' working checkouts.
  Identify it by repository and access scope, with an explicitly configured
  remote and trunk ref; do not assume every repository uses `origin/master`.
- Keep it near the latest verified trunk revision. Record the exact commit
  and refresh age so callers can distinguish a current seed from a stale one.
- Use a validated seed generation as the physical source for new Git
  worktrees and JJ workspaces, including the Git-worktree-then-JJ-adopt path.
- Resolve the caller's requested revision independently. A trunk seed is an
  optimization, not permission to replace a requested branch or commit with
  trunk. Materialize any remaining seed-to-target delta in the child.
- Preserve explicit workspace-fork semantics. A request to copy a parent's
  working state must not silently become a clean checkout from the trunk seed.

## Refresh and creation lifecycle

1. Resolve the configured trunk ref to an exact commit under a defined fetch
   policy. Coalesce concurrent refresh requests for the same seed.
2. Prepare or update a private generation and establish consistent VCS and
   AWACS state. Do not advertise a partially updated checkout as ready.
3. Publish the validated generation atomically. Creators pin one generation
   through snapshot creation and baseline handoff; refresh must not mutate the
   generation they are using.
4. Create an independently owned child snapshot and VCS metadata, materialize
   the exact requested target, and complete the necessary baseline
   reconciliation before exposing the child as ready.
5. Retire old generations only after creators and any retained baseline
   references no longer need them.

## Correctness and ownership requirements

- Never reset, clean, rebase, or move a user's active checkout or bookmarks to
  refresh the seed. Seed maintenance owns only its explicitly managed state.
- Keep seed contents limited to the intended repository state. Do not collect
  a developer checkout's credentials, local environment state, ignored build
  outputs, or unrelated untracked files into a reusable seed. Do not share a
  private seed across users or repository authorization boundaries.
- A ready generation's files, Git HEAD/index, and any prepared JJ semantic
  tree and AWACS baseline must describe a consistent state. Preserve the
  interpretation inputs needed to validate that state in a child.
- Each child needs independent Git index/workspace metadata and AWACS consumer
  ownership. Establish destination-bound identities through the existing
  handoff mechanisms, rather than treating a copied source cursor as a valid
  child cursor.
- Preserve edits made after the child snapshot or target checkout. In
  particular, do not skip JJ reconciliation by advancing an AWACS baseline
  without updating and validating its paired semantic tree.
- A failed refresh leaves the previous validated generation usable. Define
  whether callers may use an explicitly stale seed, fall back to ordinary
  creation, or receive an error. None of these choices may publish partial
  state or silently change the requested revision.

## Open decisions

- Who owns seed storage, fetches, refresh scheduling, and cleanup: AWACS, the
  VCS integration, or the worktree manager?
- What freshness bound is useful? Should refresh be periodic, on demand, or
  both, and should a caller wait or proceed from the last validated seed?
- What compatibility information belongs in a generation key besides the
  repository and revision, such as sparse-checkout or interpretation settings?
- How should Git-only seeds and seeds with prepared JJ state share the same
  lifecycle without weakening either consumer's baseline requirements?

Environment provisioning is separate work. This proposal does not by itself
cache virtualenvs or eliminate other per-workspace setup commands.

## Validation goals

Compare ordinary creation from a stale development checkout with creation
from a trunk seed, using the same requested target and verified final state.
Measure seed refresh separately from the child creation path, including
amortized refresh cost across multiple children. Break child time down into
snapshot creation, Git materialization, JJ/AWACS reconciliation, and any
remaining setup rather than attributing the whole operation to Btrfs.

Exercise concurrent refresh and creation, failed refresh, stale-seed policy,
non-trunk targets, explicit workspace forks, and edits made during child
initialization. Verify that the source workspace is untouched, children are
independent, and neither generation cleanup nor baseline handoff loses changes
or exposes private source state.
