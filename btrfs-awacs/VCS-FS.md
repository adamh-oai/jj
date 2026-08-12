# Filesystem primitives for large VCS repositories

This document tracks filesystem improvements that would let Git, Jujutsu,
AWACS, and similar tools avoid reconstructing repository state in userspace.
These are desired filesystem primitives, not commitments to implement them in
AWACS. AWACS should remain a proving ground and a compatibility layer while
the filesystem interfaces mature.

The common requirement is a cheap, snapshot-consistent answer to:

> What changed, which names identify it, and which unchanged structure can be
> reused without walking millions of paths?

## 1. Unprivileged deletion of read-only snapshots

### Problem

With `user_subvol_rm_allowed`, an unprivileged owner can delete a writable
subvolume, but deleting a read-only snapshot still fails the target-root
`MAY_WRITE` check. A VCS snapshot manager must therefore:

1. clear the read-only flag;
2. wait for the synchronous transaction commit performed by
   `BTRFS_IOC_SUBVOL_SETFLAGS`;
3. delete the now-writable subvolume.

The read-only-to-writable transition is not semantically useful for deletion
and can turn otherwise cheap garbage collection into a multi-second global
filesystem stall. AWACS currently avoids it with a narrowly scoped privileged
broker.

### Desired primitive

Extend the unprivileged subvolume-removal permission model so a read-only
snapshot can be deleted directly when the caller is authorized to remove that
subvolume. Deletion must not grant any ability to mutate or mount the
snapshot writable.

The authorization rule should be explicit and no broader than the writable
case:

- require write and search permission on the parent directory;
- require the existing `user_subvol_rm_allowed` mount policy, or a more
  narrowly named successor;
- require the caller to own the subvolume root, unless ordinary administrator
  privilege applies;
- preserve existing protections for mounted, busy, nested, or otherwise
  undeletable subvolumes.

The API should also support deletion by an already-verified subvolume identity
(root ID plus filesystem identity), not only by pathname. A pathname form
should resolve and authorize atomically in the kernel. This prevents a
rename/replacement race from redirecting a deletion after userspace has
checked the target.

### VCS benefit

Snapshot retention and worktree removal become bounded metadata operations.
The VCS can keep immutable baselines read-only for their entire lifetime and
does not need a privileged helper merely to collect its own snapshots.

## 2. A separate layer for untracked files

### Problem

Large working copies commonly contain a mostly stable tracked tree plus a
large, volatile set of build outputs, caches, editor artifacts, and ignored
files. Today the VCS usually discovers the latter by walking directories and
classifying names against the tracked tree and ignore rules. Even when tracked
status is incremental, untracked discovery can dominate latency.

### Desired primitive

Represent untracked files in a distinct writable namespace layer above the
tracked working-copy layer. The merged view remains an ordinary directory
tree, but the filesystem can enumerate and snapshot the layers independently.

A useful model has at least:

- an immutable or snapshot-derived tracked base;
- tracked modifications and deletions, including whiteouts, in a tracked
  writable layer;
- newly created untracked and ignored names in an untracked writable layer;
- one atomic merged snapshot identity spanning all participating layers.

The filesystem needs queries for:

- enumerate names contributed by one layer without walking the merged tree;
- map a merged path to its contributing layer and object identity;
- promote an untracked path into the tracked layer atomically;
- discard, snapshot, or clone only the untracked layer;
- report cross-layer rename, replacement, and whiteout semantics.

Directory creation, rename, and hard links need precise rules. A rename from
untracked to tracked cannot silently change classification; a hard link cannot
make one inode simultaneously have contradictory layer ownership without an
explicit representation. Ignore rules remain VCS policy, but the filesystem
should make the candidate untracked namespace cheap to enumerate.

### VCS benefit

`status` can ask for tracked changes and untracked candidates separately.
Worktree creation can clone the tracked baseline without copying disposable
outputs, while users can optionally retain or share the untracked layer.

## 3. First-class path and hard-link tracking

### Problem

An inode is not a path. One inode can have several hard-link names, a directory
rename can change millions of descendant paths, and inode numbers can be
reused. VCS tools need stable answers about names at two snapshot endpoints,
not just a stream of changed inode numbers.

AWACS currently reconstructs this as a namespace graph:

```text
(parent directory identity, name) -> child identity
```

That graph is necessary for correctness, but maintaining it in userspace is
expensive and requires conservative recovery when events are incomplete.

### Desired primitive

Make namespace edges first-class, snapshot-addressable filesystem metadata.
The filesystem should expose:

- stable object identity including filesystem, subvolume, inode, and
  generation;
- all parent/name edges for an object, rather than one arbitrary canonical
  path;
- edge additions, removals, and renames between two snapshots;
- compact subtree-move records for directory renames;
- endpoint queries that resolve a path or enumerate all aliases in a chosen
  snapshot;
- explicit mount, nested-subvolume, and directory-cycle boundaries.

Hard links should be represented as one object with many namespace edges.
Content changes belong to the object and affect every alias; link and rename
changes belong to individual edges. Consumers must be able to request either
view without rebuilding the entire reverse-reference graph.

The interface must make inode reuse safe. An inode number alone is never a
durable identity; generation and snapshot identity must participate in
comparison and lookup.

### VCS benefit

Rename detection, hard-link handling, sparse checkout, and incremental status
can consume authoritative namespace deltas. A directory rename remains one
structural change until a consumer actually needs descendant paths.

## 4. Filesystem Merkle trees

### Problem

VCS implementations repeatedly build tree objects and content hashes from a
filesystem that already knows which blocks, inodes, and directories changed.
Without a filesystem-level digest hierarchy, a VCS must re-read files and
re-enumerate directories merely to prove that most of the tree is unchanged.

### Desired primitive

Maintain a snapshot-consistent Merkle hierarchy over filesystem namespace and
content. The filesystem should expose a root digest and permit lazy descent:

- compare two root or subtree digests cheaply;
- enumerate only child entries whose digest differs;
- request a file-content digest without rereading unchanged content in
  userspace;
- request directory-entry and metadata digests separately from file-content
  digests;
- pin a digest to an immutable snapshot or transaction boundary.

Hashes should be domain-separated and versioned. The definition must specify
which metadata participates: names, file type, mode, executable bit, symlink
target, selected xattrs, submodule-like boundaries, and possibly ownership.
VCS-specific ignore rules and normalization should remain above the filesystem,
so consumers may derive their own tree IDs from filesystem proofs.

Hard links require two related identities:

- an object/content digest shared by every alias;
- a namespace/tree digest that includes each parent/name edge.

The filesystem may calculate content hashes lazily, but once returned for an
immutable snapshot they must be stable and crash-safe. Dirty or unhashed
subtrees need an explicit state so callers can choose between waiting,
descending, or falling back to ordinary reads.

### VCS benefit

A VCS can compare root hashes, descend only into changed subtrees, and reuse
known content IDs. Lazy trees become a native operation rather than a large
userspace cache that must be rebuilt after crashes or copied to every
worktree.

## Relationships and likely order

These primitives reinforce each other:

1. direct deletion of read-only snapshots removes an immediate lifecycle
   outlier;
2. first-class namespace edges provide the correctness foundation for
   hard-links, renames, and endpoint diffs;
3. a separate untracked layer makes volatile namespace state independently
   enumerable;
4. Merkle trees make unchanged tracked structure and content lazily reusable.

The path model and Merkle model should share identities and snapshot
boundaries. The untracked layer should not require a second incompatible path
API, and snapshot deletion must treat the merged snapshot/layer set as one
retention unit.
