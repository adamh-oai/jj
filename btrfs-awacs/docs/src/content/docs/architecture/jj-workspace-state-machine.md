---
title: Persistent state, one counter at a time
description: Follow a tiny AWACS consumer that keeps a crash-safe count of files whose names start with A.
---



## v0

The brute force solution is easy:

```
$ rg -l 'froge' | wc -l
```
But as the set of files grows this gets slower and slower, and everyone is upset.

## v1

We can avoid a full traversal by incrementally updating the counter as the tree changes, e.g with `watchman`.  This requires a little more state:

```
class FrogeState:
    clock: str
    froge_files: set[str]
```

`clock` is an opaque version that is passed back to `watchman`, so we can ask
for `files_changed_since(clock)`.

With this we can incrementally update the state, doing only `O(changes)` work instead of traversing the whole repo:

```
def initial_state() -> FrogeState:
   files = set()
   new_clock = current_clock()
   for f in traverse_all_files():
     if file_contains_froge(f):
       files.add(f)
   return FrogeState(new_clock, files)

def update_state(prev: FrogeState) -> FrogeState:
  new_files = set(prev.froge_files)
  new_clock, changes = files_changed_since(prev.clock)
  for f in changes:
    if file_contains_froge(f):
      new_files.add(f)
    else:
      new_files.remove(f)
  return FrogeState(new_clock, new_files)
```

But there a numerous ways that this can fail and require a full, slow scan of
the entire repo:

  * `watchman` is implemented with `inotify`, which only detects changes while its running; if it restarts or you reboot -> rescan
  * `inotify` watches are a finite kernel resource; if you run out -> rescan
  * `inotify` has a fixed-size buffer for changes, if it fills up -> rescan
  * If you create a new worktree -> rescan

As a separate issue, `inotify` is asynchronous and subject to race conditions
between what's actually in the filesystem and when you get the notification.



## Names used in the example

We use short labels instead of real UUIDs:

```text
repo path:          /repo
repo subvolume id:  S0
first snapshot:     A0
second snapshot:    A1
counter id:         counter-1
```

`S0` is the mutable live subvolume at `/repo`. `A0` and `A1` are immutable
read-only snapshots of `S0` at different moments. `counter-1` is just a stable
name for this counter so AWACS knows which snapshot pin belongs to it; it is
not a person or login.

For this walkthrough, assume the AWACS state root is:

```text
/state/awacs/S0/
  manager.sqlite3
  path-map.sqlite3
  managed/
    cut-<operation-id>/
      snapshot
```

The exact state-root location is deployment detail. The important part is that
each stored snapshot has a durable row and a durable path.

## The four durable pieces

| Durable place | What it answers |
| --- | --- |
| `/repo` | What does the mutable live tree look like right now? |
| `managed/.../snapshot` | What exact immutable bytes did a count observe? |
| `manager.sqlite3` | Which snapshots, revisions, cuts, and pins exist? |
| `path-map.sqlite3` | Which names refer to which objects in the latest indexed snapshot? |
| `counter.sqlite3` | Which snapshot produced the A-counter's last committed count? |

The A-counter's own database is tiny:

```sql
CREATE TABLE counter_state (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  baseline_snapshot TEXT NOT NULL,
  a_file_count INTEGER NOT NULL
);
```

AWACS does not own this table. It belongs to the consumer. AWACS owns the
snapshot history and the proof that lets the consumer advance safely.

## Step 0: the live tree exists, but no count is trusted

Start with this mutable tree:

```text
/repo/
  Alpha.md
  beta.txt
  docs/
    API.md
```

The answer appears to be `2`, but the A-counter has no durable baseline yet.
If it walks `/repo` while another process is renaming files, it can combine
bytes from different moments. So it first asks AWACS for an immutable
snapshot.

## Step 1: capture A0 and do the first full count

AWACS cuts:

```text
A0 = /state/awacs/S0/managed/cut-0000.../snapshot
```

The important operational rows are conceptually:

```text
snapshots
  id=41  subvol_uuid=A0
  path=".../managed/cut-0000.../snapshot"
  readonly=1  physical_state="present"

revisions
  id=100  snapshot_id=41  state="ready"

watches
  id=W0  live_subvol_uuid=S0  live_path="/repo"
  indexed_revision_id=100  indexed_seq=0
  last_cut_snapshot_id=41   last_cut_seq=0
```

`path-map.sqlite3` now has `mutable_head.revision_id=100`. It does not need a
flat row containing `docs/API.md`. The name graph is more like:

```text
mutable_objects
  ino=10  mode=file       ...
  ino=20  mode=file       ...
  ino=30  mode=directory  ...
  ino=40  mode=file       ...

mutable_refs
  ino=10  parent_ino=<root>  name="Alpha.md"
  ino=20  parent_ino=<root>  name="beta.txt"
  ino=30  parent_ino=<root>  name="docs"
  ino=40  parent_ino=30      name="API.md"
```

The A-counter walks immutable `A0`, not `/repo`:

```text
Alpha.md     counts
beta.txt     does not count
docs/API.md  counts
```

It commits:

```text
counter.sqlite3
  baseline_snapshot=A0
  a_file_count=2
```

AWACS also retains `A0` for the counter:

```text
snapshot_pins
  snapshot_id=41
  owner_kind="consumer-baseline"
  owner_id="counter-1"
  reason="committed"
```

The durable invariant is now:

```text
count=2 came from exactly A0
```

The count alone is not enough. The snapshot alone is not enough. The useful
state is the pair `(A0, 2)`.

## Step 2: the live tree changes

Later, the mutable tree becomes:

```text
/repo/
  Apple.txt       # added
  Apricot.txt     # beta.txt renamed
  docs/
    API.md
```

`Alpha.md` was deleted, `Apple.txt` was added, and `beta.txt` was renamed to
`Apricot.txt`.

Nothing about the committed count changes yet:

```text
counter.sqlite3
  baseline_snapshot=A0
  a_file_count=2

snapshot_pins
  A0 is still committed for counter-1
```

That is correct. `A0` is still the last immutable tree the A-counter actually
counted. The live tree may be ahead of it.

## Step 3: capture A1 and record the A0 to A1 proof

The next time the A-counter wants to advance, AWACS cuts:

```text
A1 = /state/awacs/S0/managed/cut-0001.../snapshot
```

AWACS records the new snapshot and the proved transition:

```text
snapshots
  id=42  subvol_uuid=A1
  path=".../managed/cut-0001.../snapshot"
  readonly=1  physical_state="present"

revisions
  id=101  snapshot_id=42
  storage_base_revision_id=100
  state="ready"

comparisons
  id=501  from_snapshot_id=41  to_snapshot_id=42
  comparison_kind="incremental"  state="index_ready"

watch_cuts
  watch_id=W0  sequence=1
  base_snapshot_id=41  target_snapshot_id=42
  comparison_id=501  state="ready"

watches
  indexed_revision_id=101  indexed_seq=1
  last_cut_snapshot_id=42  last_cut_seq=1

path-map.sqlite3
  mutable_head.revision_id=101
  mutable_objects/mutable_refs describe A1
```

The comparison proves this name-level delta:

```text
removed: Alpha.md
removed: beta.txt
added:   Apple.txt
added:   Apricot.txt
```

For this counter, that is enough:

```text
old count from A0:        2
remove Alpha.md:         -1
remove beta.txt:          0
add Apple.txt:           +1
add Apricot.txt:         +1
new count for A1:         3
```

A content-based consumer would open and read files from immutable `A1`. This
name-only consumer can update from the proved reference changes.

## Step 4: publish the new count without losing A0

Before the A-counter commits `(A1, 3)`, AWACS stages a second pin:

```text
snapshot_pins
  A0  owner_id="counter-1"  reason="committed"
  A1  owner_id="counter-1"  reason="pending"
```

Both snapshots are retained during the handoff. The A-counter then atomically
writes:

```text
counter.sqlite3
  baseline_snapshot=A1
  a_file_count=3
```

Only after that write succeeds does it tell AWACS to finish the handoff:

```text
snapshot_pins
  DELETE A0, owner_id="counter-1", reason="committed"
  UPDATE A1, owner_id="counter-1", reason="pending"
         -> reason="committed"
```

The new durable invariant is:

```text
count=3 came from exactly A1
```

The ordering is the whole safety story:

```text
stage A1 pin -> write (A1, 3) -> commit A1 pin
```

AWACS never releases the last committed snapshot before the consumer has
durably named its replacement.

## Step 5: what a crash leaves behind

| Crash point | Durable counter state | Safe recovery |
| --- | --- | --- |
| Before A1 is cut | `(A0, 2)` | Cut a new snapshot from A0 later. |
| After A1 is cut, before the counter write | `(A0, 2)` | Keep A0 authoritative; discard or expire A1's pending pin. |
| After the counter write, before finish | `(A1, 3)` | Retry the idempotent finish so A1 becomes the committed pin. |
| After finish | `(A1, 3)` | Continue from A1. |

There is never a valid state where the durable count says `3` but names no
snapshot, or where the counter releases `A0` before it has committed `A1`.

## Step 6: when AWACS cannot prove a delta

Sometimes AWACS cannot prove `A0 → A1`: history may have been collected, a
token may be invalid, or a change may be too ambiguous to express precisely.
Then the A-counter must not guess.

It can either:

1. keep the old durable state `(A0, 2)` and fail; or
2. explicitly rebuild by fully counting a fresh immutable snapshot `A2`, then
   commit `(A2, new_count)` as a new baseline.

The rule is:

```text
an exact delta may update a count;
missing proof requires a full recount.
```

That same contract scales beyond this toy counter. A consumer may build a
semantic tree, an index, or another derived artifact, but its durable state
must always name the exact immutable snapshot that produced it.
