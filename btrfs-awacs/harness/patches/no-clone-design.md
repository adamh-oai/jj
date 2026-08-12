# Btrfs send no-clone experiment

Kernel commit `a86246a467a3` is deliberately independent from
`BTRFS_SEND_FLAG_NO_FILE_DATA`.

- Existing `NO_FILE_DATA` callers are unchanged and can still get `CLONE`
  commands.
- `NO_CLONE` alone suppresses clone discovery and falls back to
  `WRITE`/`ENCODED_WRITE`.
- `NO_CLONE | NO_FILE_DATA` suppresses both payload data and clone discovery,
  so changed data ranges are represented by `UPDATE_EXTENT`.
- The stream version is unchanged because `WRITE`, `ENCODED_WRITE`, and
  `UPDATE_EXTENT` already exist.
- The current unpatched kernel rejects bit `0x20` with `EOPNOTSUPP` (some
  older kernels used `EINVAL` for unknown send flags). Callers may report that
  error or explicitly retry with ordinary `NO_FILE_DATA`; retrying restores
  the old, slower semantics.

The patch intentionally leaves clone-root setup alone. That setup also pins
roots and participates in flushing delalloc/refreshing commit roots, so
removing it is a broader correctness-sensitive refactor. The hot operation
shown by the profile is the per-extent call to `find_extent_clone()`, which the
new guard bypasses.

## Direct ioctl test

The installed `btrfs` command will not know the experimental option. Build the
small driver next to this file:

```sh
cc -O2 -Wall -Wextra -Werror -o send-ioctl send-ioctl.c
parent_id=$(btrfs inspect-internal rootid /mnt/fixture/PARENT)
./send-ioctl no-clone /mnt/fixture/CURRENT "$parent_id" no-clone.send
btrfs receive --dump < no-clone.send > no-clone.dump
```

The helper passes no explicit clone-source array. The parent root is still
used by the kernel to compute the incremental tree difference. Like
btrfs-progs, it gives the ioctl a pipe and drains that pipe to the requested
output in a child process; character devices such as `/dev/null` are not
always directly writable through the kernel's `kernel_write()` path.

## Correctness checks

1. On an unpatched kernel, the helper must fail with `EOPNOTSUPP`.
2. On a patched kernel, the dump must contain no `clone`, `write`, or
   `encoded_write` commands and must contain `update_extent` commands.
3. Normalize each baseline `clone` and `update_extent` to its destination
   `(path, offset, length)`, coalesce adjacent ranges, and compare that coverage
   with the no-clone stream. Also require the final path set produced by
   `bwatch-send changes` to be identical. Do not require the raw command lists
   to match: the clone path can emit an intermediate `TRUNCATE` solely to make
   an unaligned clone legal at the receiver, while the no-clone path does not
   need that command.
4. Exercise `NO_CLONE` without `NO_FILE_DATA` using a variant of the helper,
   receive both a full and an incremental stream, and compare file contents,
   holes, xattrs, modes, ownership, and timestamps with the sources.
5. Send without the new flag on both kernels and require byte-identical
   streams for an unchanged fixture. This catches accidental behavior changes
   to the existing ABI.

## Performance checks

Run the same warmed incremental send under both kernels and record:

- wall time and CPU time;
- calls to `find_extent_clone()` and `iterate_extent_inodes()`;
- calls/samples in `btrfs_search_slot()`, `get_inode_info()`,
  `process_recorded_refs()`, and `btrfs_clone_extent_buffer()`;
- stream byte and command counts.

With `NO_CLONE`, `find_extent_clone()` and its backreference walk should have
zero calls from the send. Remaining `btrfs_search_slot()` work is the cost of
the tree diff, inode/path lookup, and reference processing, and becomes the
next optimization target.
