# Btrfs send scalar inode-info lookup experiment

The prototype is preserved as kernel commit `9e63650d2b2c` in
`~/code/linux`. The experiment was originally developed independently of both
the inode-info cache and the new `NO_CLONE` send flag; the committed version
contains the adaptations needed by the stacked kernel branch.

## Why the current lookup clones a leaf

`alloc_path_for_send()` configures every path to:

- search the commit root;
- skip ordinary tree locking;
- ask `btrfs_search_slot()` to manage `commit_root_sem`.

For such a path, `btrfs_search_slot()` holds `commit_root_sem` while walking
the tree. Before it unlocks, `finish_need_commit_sem_search()` clones the
lowest extent buffer and replaces the searched path with that clone. This
makes the returned path safe to consume after a transaction commit or
relocation reallocates the original tree block.

That behavior is necessary for callers which retain or iterate the returned
path. `get_inode_info()` does neither: it reads eight scalar fields from one
inode item and returns. Cloning the complete leaf for every lookup is
unnecessary.

## New locking sequence

The patch keeps `search_commit_root` and `skip_locking`, but clears
`need_commit_sem` for this path alone. It then performs this sequence:

1. acquire `root->fs_info->commit_root_sem` for reading;
2. call `btrfs_search_slot()`;
3. while still holding the semaphore, copy all requested inode fields;
4. release every extent-buffer reference in the path;
5. release `commit_root_sem`;
6. let the automatic cleanup free the now-empty path object.

Keeping the read semaphore through the scalar copies prevents transaction
commit or relocation from switching the commit root and making the searched
extent buffers reusable. Releasing the path before the semaphore is the
important ordering rule: nothing returned by the lookup can refer to the
protected tree after the unlock.

This does not add a new lock acquisition. The old
`path->need_commit_sem = true` path acquired the same semaphore inside
`btrfs_search_slot()` and held it through the search and leaf clone. The patch
makes that ownership explicit and holds it only through the much smaller
scalar copy.

## Error cleanup

Path allocation happens before taking the semaphore, so `-ENOMEM` returns
directly.

After the semaphore is acquired, every exit uses one `out` label:

- a positive search result is translated to `-ENOENT`;
- a negative search error is preserved;
- a successful existence-only lookup (`info == NULL`) preserves result zero;
- a successful value lookup copies all fields and preserves result zero.

`btrfs_search_slot()` may already release a path for a negative error.
Calling `btrfs_release_path()` again is safe and keeps the local cleanup
unconditional. The semaphore is released exactly once after the path.

## Scope and expected effect

Only `get_inode_info()` changes. Its signature, callers, return values, and
stream decisions remain unchanged. Other send paths still get the existing
clone-after-search behavior.

The expected effect is to remove the
`finish_need_commit_sem_search()`/`btrfs_clone_extent_buffer()` work associated
with inode-info lookups while leaving the number of B-tree searches unchanged.
The fixed inode-info cache is orthogonal: if the experiments are later
combined, this lookup belongs in the cache's miss helper.

The main remaining cost is one commit-root B-tree search and one read-semaphore
acquisition per call. This patch intentionally does not address that repeated
work so its effect can be measured independently.

## Validation

The committed implementation passes:

```sh
git -C ~/code/linux show --check 9e63650d2b2c
```

`checkpatch.pl` reports zero errors, warnings, or checks.

Runtime validation on the warmed `~/code/openai` fixture showed:

1. the stream remained byte-identical, with SHA-256
   `e1035cd0d887bf27b8135e1b1b206ba2bdc11db6b1fa4107b5ce3e327f3af317`;
2. `get_inode_info()` remained at 1,380,373 calls and
   `btrfs_search_slot()` remained at 1,940,917 calls;
3. `btrfs_clone_extent_buffer()` fell from 1,915,888 to 535,515 calls, a
   reduction of exactly 1,380,373;
4. a paired five-sample comparison fell from 16.182 ± 0.286 seconds to
   8.872 ± 0.330 seconds, a 45.2% reduction;
5. when integrated into the fixed inode-cache miss helper, a separate paired
   comparison fell from 9.418 ± 0.412 seconds to 7.754 ± 0.598 seconds, a
   further 17.7% reduction.

See `../RESULTS.md` for the complete profile and measurement context.
