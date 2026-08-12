# Btrfs send scalar first-reference lookup experiment

The prototype is preserved as kernel commit `cb675033e9b1` in
`~/code/linux`. It was originally developed independently of the inode cache
and other scalar-lookup changes.

## Target

`get_first_ref()` searches from `(ino, BTRFS_INODE_REF_KEY, 0)` and selects
the first matching normal inode reference, or the first extended reference if
there is no normal one. It needs only:

- the parent inode number;
- at most 255 bytes of basename;
- optionally, the parent generation obtained by a later inode lookup.

The normal send path clones the searched leaf so the inode-reference pointer
remains valid after `btrfs_search_slot_for_read()` releases
`commit_root_sem`. A miss at the end of a leaf can also traverse to the next
leaf and clone again.

## New lifetime

The patch:

1. allocates the send path before locking;
2. disables its automatic semaphore and clone handling;
3. holds `commit_root_sem` across the complete search, including possible
   next-leaf traversal;
4. copies the parent and selected name into a 255-byte stack buffer;
5. releases the tree path, restores its normal flag, and unlocks;
6. appends the copied name to `fs_path` and performs the optional cached
   parent-generation lookup after unlocking.

Moving both operations after the unlock matters. `fs_path_add()` may allocate
when the path buffer must grow, and `get_inode_gen()` performs another
commit-root search. Recursively taking the read semaphore can deadlock behind
a queued writer.

Normal inode-reference items are tree-checked to have names of 1–255 bytes.
The extended-reference checker verifies item bounds but does not explicitly
enforce the filesystem name limit. The patch rejects an extended name longer
than `BTRFS_NAME_LEN` with `-EUCLEAN` before copying it to the bounded buffer.

## Validation

The standalone patch:

- applies cleanly to `3dab139d4795`;
- passes `git diff --check`;
- passes strict `checkpatch.pl` with zero errors, warnings, or checks;
- builds as a UML kernel when composed with the inode cache and earlier scalar
  patches.

On the scoped `~/code/openai` fixture:

- all streams remain byte-identical at 21,074,818 bytes with SHA-256
  `e1035cd0d887bf27b8135e1b1b206ba2bdc11db6b1fa4107b5ce3e327f3af317`;
- `get_first_ref()` remains at 124,718 calls;
- `btrfs_search_slot()` remains at 817,418 calls;
- `btrfs_clone_extent_buffer()` falls from 243,921 to 117,809 calls, removing
  126,112 clones, or 51.70%;
- the extra 1,394 removed clones beyond the call count come from searches that
  cross a leaf boundary;
- sampled `get_first_ref()` clone stacks fall from 1,320 to zero;
- a warmed one-vCPU UML A/B/A run measures 4.632 ± 0.183 seconds for the first
  control, 3.748 ± 0.294 seconds for the patch, and 4.762 ± 0.408 seconds for
  the second control;
- relative to the 4.697-second mean of all ten flanking control samples, the
  patch is an observed 20.2% faster.

Before proposing the change upstream, add focused coverage for normal and
extended references, packed hardlinks, maximum-length names, a forced
next-leaf search, malformed extended-reference lengths, allocation and search
failure injection, and concurrent relocation under lockdep and KASAN.
