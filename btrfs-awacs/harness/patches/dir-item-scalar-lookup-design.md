# Btrfs send scalar directory-item lookup experiment

The prototype is preserved as kernel commit `9e4092a25535` in
`~/code/linux`. It was originally developed independently of the inode-info
cache, scalar inode-info lookup, and `NO_CLONE` send flag.

## Target

Two send paths call `btrfs_lookup_dir_item()` but retain almost none of the
returned leaf:

- `gen_unique_name()` only needs to know whether a candidate exists;
- `lookup_dir_item_inode()` only needs the matched item's location key.

The normal send path asks `btrfs_search_slot()` to acquire
`commit_root_sem`. Before releasing it, the search clones the complete lowest
extent buffer so the returned directory-item pointer remains valid if
relocation later reallocates the original leaf.

## New lifetime

`lookup_dir_item_key()` uses a caller-owned send path and:

1. asserts that the caller does not already hold `commit_root_sem`;
2. temporarily clears `path->need_commit_sem`;
3. acquires `commit_root_sem` for reading;
4. performs the normal exact-name directory lookup;
5. copies the location key, if requested, while the leaf is protected;
6. releases the path before the semaphore;
7. restores `path->need_commit_sem` for safe reuse.

No extent-buffer pointer escapes the helper. The critical section contains no
nested metadata lookup, allocation for stream output, or stream write.
Avoiding recursive read acquisition matters because a queued writer can block
a recursive reader while waiting for the original read lock.

The existing packed-item name matching remains unchanged, including handling
of directory-name hash collisions. `gen_unique_name()` still treats any item
type as occupied. `lookup_dir_item_inode()` still treats a
`BTRFS_ROOT_ITEM_KEY` location as absent.

## Validation

The standalone patch:

- applies cleanly to `3dab139d4795`;
- passes `git diff --check`;
- passes strict `checkpatch.pl` with zero errors, warnings, or checks;
- builds as a UML kernel when composed with the inode cache and scalar
  inode-info lookup.

On the scoped `~/code/openai` fixture:

- every generated stream remained byte-identical at 21,074,818 bytes with
  SHA-256
  `e1035cd0d887bf27b8135e1b1b206ba2bdc11db6b1fa4107b5ce3e327f3af317`;
- `btrfs_search_slot()` remained at 817,418 calls;
- `btrfs_clone_extent_buffer()` fell from 535,515 to 243,921 calls, removing
  291,594 clones, or 54.45%;
- an A/B/A timing run measured 7.124 ± 0.396 seconds for the first control,
  4.882 ± 0.068 seconds for the patch, and 6.550 ± 0.082 seconds for the
  second control;
- relative to the 6.837-second mean of all ten flanking control samples, the
  patch reduced wall time by an observed 28.6% in this warmed, one-vCPU UML
  fixture.

Before proposing the change upstream, add focused coverage for real Btrfs
name-hash collisions, root-item directory entries, allocation/search error
injection, and concurrent relocation under lockdep and KASAN.
