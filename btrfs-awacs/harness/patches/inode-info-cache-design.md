# Btrfs send fixed inode-info cache experiment

The prototype is preserved as kernel commit `4b0ed36b3a5b` in
`~/code/linux`. It replaced the earlier `btrfs_lru_cache` experiment and is
stacked on the Ubuntu HWE kernel branch.

## Why replace the LRU

The measured workload makes 1,380,373 `get_inode_info()` calls per send. The
1,024-entry `btrfs_lru_cache` experiment reduced that to 238,131 underlying
reads and reduced elapsed time from about 14.25 seconds to about 8.87 seconds.
That proves useful temporal locality, but a miss still adds an allocation,
maple-tree insertion, and possible LRU eviction to the original B-tree lookup.
Hits also traverse the generic cache data structures.

The fact that 256, 4,096, and 16,384 entries were all slower than 1,024 makes
constant overhead and cache footprint worth testing separately from hit rate.
This version retains 1,024 entries while replacing the generic cache with a
fixed array.

## Layout and lookup

The cache has 512 sets and two ways per set:

```text
hash(inode, 9 bits) -> set -> [way 0, way 1]
```

Each entry contains:

- the `struct btrfs_root *` and inode number forming the exact key;
- all fields returned by `get_inode_info()`;
- the result, which is either success or `-ENOENT`.

On x86-64 an entry is 88 bytes and a set is 184 bytes including its
replacement byte and padding. The complete array is 94,208 bytes (92 KiB).
It is allocated and zeroed once with `kvzalloc` when the send context is
created, then freed with that context. There are no per-miss allocations.
Failure to allocate the cache is not a send failure; lookups transparently
use the old B-tree path.

`hash_64(ino, 9)` selects a set. Parent-root lookups flip the high bit of the
set index so a send-root/parent-root pair for the same inode does not
immediately contend for the same two ways. Entries still compare both root
pointer and inode number, so the index transform cannot create a false hit.

A hit probes at most two adjacent entries and does not update replacement
state. On a miss, an unused way is preferred. Once both ways are occupied,
the set alternates victims with one byte of round-robin state. This is FIFO
within a colliding set rather than a true LRU: it deliberately accepts more
conflict misses in exchange for no list operations, no tree operations, and
no cache-line write on hits.

## Scope and lifetime

Only the send root and parent root are admitted. An arbitrary clone source
falls through to `read_inode_info()` unless it is also one of those roots.
This avoids allowing a large clone-source array to churn the small cache and
covers the repeated lookups in tree comparison, path construction, reference
processing, and current-inode finalization.

Both roots are read-only and pinned with `send_in_progress` for the ioctl.
Lookups use their commit roots. Success and absence are therefore stable for
the life of this per-send cache. Relocation may replace physical tree blocks,
but does not change the cached logical inode fields or turn an inode into an
absent one. The cache is private to the synchronous send context, so it needs
no locking.

Errors other than `-ENOENT` are never cached. This preserves retry/error
behavior for allocation failures, I/O errors, and detected corruption.

## Covered call sites

As in the earlier experiment, the patch passes `send_ctx` through every
inode-generation lookup. It covers:

- both roots in `get_cur_inode_state()`;
- parent generations in `get_first_ref()` and path construction;
- overwrite handling, delayed directory moves, and ancestry checks;
- reference recording and directory generation lookups;
- clone-source EOF checks when the source is the send or parent root;
- final send/parent attribute lookups for the current inode.

The signature changes in `get_first_ref()`, `check_ino_in_path()`, and
`is_ancestor()` are plumbing only. Stream-generation decisions do not change.

## Expected tradeoffs

Relative to the 1,024-entry LRU prototype, this should make every hit cheaper
and remove all miss-side cache allocation and maple-tree work. It may perform
more than 238,131 underlying reads because unrelated keys that hash to the
same set can evict each other even when other sets are empty. Two ways were
chosen over direct mapping because the extra comparison is small compared
with a B-tree miss and avoids the worst one-collision ping-pong behavior.

The useful comparison is therefore elapsed CPU time, not hit rate alone. A
moderate increase in misses is acceptable if removing generic-cache overhead
more than pays for it. If misses rise enough to erase the gain, the next
low-overhead variants to test are:

1. 1,024 two-way sets (2,048 total entries);
2. separate direct-mapped banks for the send and parent roots;
3. admission only from `get_inode_gen()` and `get_cur_inode_state()`, while
   allowing every call site to consume an existing entry.

## Validation

The committed implementation passes:

```sh
git -C ~/code/linux show --check 4b0ed36b3a5b

make ARCH=um O=/tmp/btrfs-awacs-uml/kernel-inode-1024 \
  -j2 fs/btrfs/send.o
```

The UML compile used the same configuration as the baseline harness.

Runtime measurement collected:

1. 1,380,373 cache-facing lookups and 256,874 underlying reads, an 81.4% hit
   rate;
2. 817,418 `btrfs_search_slot()` calls, down 57.9%;
3. 792,389 `btrfs_clone_extent_buffer()` calls, down 58.6%;
4. a reduction from 15.056 seconds to 8.034 seconds in the adjacent
   five-sample A/B;
5. byte-identical output with the unmodified UML kernel.

See `../RESULTS.md` for the complete fixture, timing, profile, and exact-count
data.

An ENOENT-heavy rename/delete fixture is also important because negative
entries are cached. A full data send should be checked in addition to
`--no-data`; the cache itself is independent of the command selected after
metadata comparison.
