# Btrfs send profiling and UML results

## Result

The dominant avoidable work in this workload is repeated inode-item lookup
and the full metadata-leaf copy performed after each lookup, not clone-source
discovery. Two independent changes each remove about 45% in a paired A/B:

On the `~/code/openai` fixture, a fixed 1,024-entry inode-info cache reduced a
warmed `btrfs send --no-data` from 15.056 seconds to 8.034 seconds, a 46.6%
wall-time reduction. It removed 81.4% of the underlying inode-item reads,
57.9% of all `btrfs_search_slot()` calls, and 58.6% of
`btrfs_clone_extent_buffer()` calls.

An independent scalar-lookup patch kept `commit_root_sem` through the copy of
the eight requested inode fields instead of cloning the entire searched leaf.
It reduced a paired no-cache control from 16.182 seconds to 8.872 seconds,
45.2%, while leaving the B-tree search count unchanged. It removed exactly
1,380,373 extent-buffer clones, one for every inode-info lookup. Layered on
the cache, it provided a further paired reduction from 9.418 seconds to 7.754
seconds, 17.7%.

A second scalar-lookup patch consumes directory items while holding
`commit_root_sem`, copying only their location key or existence result. Layered
on the inode cache and scalar inode lookup, it reduced the extent-buffer clone
count from 535,515 to 243,921, while leaving the B-tree search count unchanged.
In a warmed, one-vCPU UML A/B/A run, the patched mean was 4.882 seconds,
between control means of 7.124 and 6.550 seconds. Relative to the mean of the
two flanking controls, this is an observed 28.6% wall-time reduction.

A third scalar-lookup patch copies the first inode reference's parent and name
while holding `commit_root_sem`, then releases it before growing the path or
looking up the parent generation. It reduced the remaining extent-buffer
clones from 243,921 to 117,809 without changing the 817,418 B-tree searches.
In another warmed, one-vCPU UML A/B/A run, its 3.748-second mean was 20.2%
below the 4.697-second mean of the flanking controls.

An independent opt-in `NO_CLONE` flag eliminated all 28,968
`find_extent_clone()` calls, but only removed 47,109 `btrfs_search_slot()`
calls. With the inode cache enabled, its 8.004-second mean was statistically
indistinguishable from the 8.034-second default-mode control. Clone discovery
is therefore not the useful optimization for this particular snapshot pair.

A command-discard profiling mode measured the upper bound available from
changing command output while preserving traversal, path construction, and
TLV calls. On the composed optimization stack, the mean of ten A/B/A control
samples was 3.987 seconds and five discard samples averaged 2.002 seconds, a
49.8% reduction. It replaced 220,605 `kernel_write()` calls with one header
write. Exact counts for tree searches, path resolution, inode lookup, extent
processing, and command construction were unchanged.

A separate compact changed-path prototype recovered essentially that entire
bound. It deduplicated 218,951 selected path attributes into 70,074 records,
stored a command bitmask per path, and emitted the records in 60 buffered
writes. Its 2.018-second mean was 49.4% below the control and only 0.8% above
discard, within run-to-run variation. Output shrank from 21,074,818 to
3,879,612 bytes, an 81.6% reduction. A binary-stream validator confirmed every
record and command mask against the corresponding attributes in the ordinary
send stream.

A metadata-only write-coalescing patch retains the ordinary receiveable stream
but batches completed commands into 64 KiB writes. It reduced
`kernel_write()` calls from 220,605 to 323 without changing any traversal,
command, CRC, or stream counts. The adjacent five-run comparison fell from
5.488 to 2.880 seconds, a 47.5% reduction, and the stream remained
byte-identical.

A separate timestamp patch copies raw inode times into the existing inode-info
cache and makes `send_utimes()` consume that cache. All 56,582 UTIMES calls
stopped cloning an inode-item leaf. Only 1,653 calls missed the cache, reducing
`btrfs_search_slot()` by 54,929 and `btrfs_clone_extent_buffer()` by exactly
56,582. Its 1.926-second mean was 22.3% below the 2.478-second mean of ten
flanking coalescing-only samples. Protocol-v1 and protocol-v2 streams were
byte-identical to their respective controls.

The detector's `changes --timing --last` mode was also exercised end-to-end
inside UML. Both paths reported 70,074 changed paths. The ordinary
send-and-dump detector took 35.519 seconds and the compact detector took 2.082
seconds. That 17.06x UML result includes the unusually expensive one-vCPU
`btrfs receive --dump` path, so it is a functional and relative harness result,
not a native-host latency prediction.

The dedicated changed-objects v2 ABI also passed the disposable-image UML
acceptance boot on the Ubuntu HWE 7.0.12 source. The service initialized from
the v2 full-index stream, applied adjacent and direct historical v2 deltas,
reused a crash-complete manifest stage, recovered every injected snapshot,
Worktree, GC, and full-fresh boundary, and independently proved incremental
state equal to a full checkpoint. The final one-object data delta was a
256-byte stream whose endpoint header, target inode/xattr replacement,
record/byte counts, CRC32C, and broker SHA-256 all validated. A process running
as uid 1000 could read the snapshot roots but received `EPERM` from the direct
ioctl, while the root broker completed the same request.
The same boot set the ioctl's output cap to 160 bytes for a result requiring
256 bytes and received an explicit output-limit failure without accepting a
partial stream (`changed_objects_output_limit=denied`).
Both incremental comparison and full-index traversal also poll pending signals
and return the v2 interrupted status with `EINTR`; userspace still requires the
authenticated completion footer, so cancelled partial output cannot publish.
The full-index equality gate included a real `trusted.btrfs-awacs` value; it
caught and then verified the fix for starting an xattr scan at hashed keys
rather than incorrectly treating the expected offset-zero miss as end of data.
The same boot initialized a separate boundary-free watch, created a nested
subvolume, and attempted the next cut. V2 emitted the mandatory `DIR_INDEX`
boundary transition, the service rejected the immutable target, and SQLite
remained at indexed sequence 0 (`nested_boundary_delta_rejected=true`). The
ordinary final data delta validated with `boundaries=+0/-0`.
Before enabling the optional precision producer, the facade also cut after
each of: a create/delete file, a wholly transient subtree, a mixed nested
transient plus retained create, hardlink data modify/restore, mode
modify/restore, and writable-`mmap` modify/restore. Namespace-only cases
returned jj's conservative fresh `/`; object cases returned both hardlink
aliases. The facade required the v2 `DIRTY_WITNESS` capability in addition to
the explicit experimental conformance opt-in.
The service was then restarted before facade activation. Its in-memory
capability observation was empty; activation reissued a v2 full-index request,
required that index to equal the committed SQLite revision, and only then
minted the next clock (`facade_restart_probe=true`).
Worktree admission now takes the per-filesystem topology lease, compares the
canonical final locator against every initializing, active, or blocked watch,
and performs the symmetric Initialize-versus-reservation check. It reacquires
and holds that lease across the final broker rename and metadata publication;
component-aware SQL tests cover both race orderings and avoid prefix mistakes
such as treating `/watch-two` as beneath `/watch`.

That boot also passed the focused compatibility surface with unmodified jj
`0.43.0-28e25c32bc98b6cfba430b4fa44f86141e94266a` and Git `2.43.0`: jj used
BSER discovery, `watch-project`, name-only queries, and the fixed periodic
snapshot trigger; Git used hook protocol v2 and reported the modified tracked
file. The trigger's precision-guard wake completed before its five-second
periodic fallback.

## Fixture

`create-fixture.sh` copied only the two newest read-only snapshots below
`~/code/openai/.btrfs-awacs` into a sparse Btrfs image:

- parent: `snapshot-000000000000000000001784790051829421186`
- current: `snapshot-000000000000000000001784790508601393672`

It used a full data send for the parent and an incremental full data send for
the current snapshot. This avoids copying unrelated subvolumes from the host
filesystem. Send/receive reconstructs the logical filesystem but does not
preserve the host filesystem's exact physical B-tree layout.

The resulting image is a 512 GiB sparse file with about 30 GiB allocated.
`btrfs check` found 31,509,913,600 bytes used and no errors.

The fixture is close to the native workload despite its rebuilt B-tree:

| Measurement | Native source | UML fixture |
| --- | ---: | ---: |
| No-data stream bytes | 21,191,468 | 21,074,818 |
| Dump lines | 220,603 | 220,603 |
| `update_extent` commands | 28,968 | 28,968 |
| `clone` commands | 0 | 0 |

The native and fixture streams differ because UUIDs and other reconstructed
metadata differ. Every protocol-v1 baseline and patched UML stream was
byte-identical, with SHA-256
`e1035cd0d887bf27b8135e1b1b206ba2bdc11db6b1fa4107b5ce3e327f3af317`.

## Profile interpretation

The native profile's major inclusive paths were:

| Symbol | Inclusive samples |
| --- | ---: |
| `btrfs_search_slot` | 38.38% |
| `get_inode_info` | 37.92% |
| `process_recorded_refs` | 30.37% |
| `get_cur_path` | 23.75% |
| `btrfs_clone_extent_buffer` | 20.18% |

These percentages overlap because they are nested call paths.

`btrfs_search_slot()` walks a Btrfs tree to find a key. It can submit and wait
for metadata reads when a tree block is cold, but that is not what explains
the large `btrfs_clone_extent_buffer()` path here. Btrfs send searches immutable
commit roots without holding `commit_root_sem` after the search. Before
releasing that semaphore, `finish_need_commit_sem_search()` clones the
lowest-level extent buffer so relocation cannot invalidate the caller's path.
The clone allocates folios and copies the complete metadata block. That is
kernel allocation and memory-copy work, not send-stream payload transfer.

The UML profile reproduced this shape: `get_inode_info()` accounted for
49.75% inclusive, `btrfs_search_slot()` 40.85%,
`process_recorded_refs()` 30.21%, `get_cur_path()` 28.96%, and
`btrfs_clone_extent_buffer()` 25.85%. The hottest self symbol was the memcpy
under `copy_extent_buffer_full()`.

## Exact call counts

The counts below cover exactly one send ioctl. In cache variants,
`get_inode_info()` is the cache-facing wrapper and `read_inode_info()` is a
miss that performs the B-tree lookup.

| Function | Baseline | Scalar lookup | Inode cache | Cache + scalar | + Dir-item scalar | + First-ref scalar |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `find_extent_clone` | 28,968 | 28,968 | 28,968 | 28,968 | 28,968 | 28,968 |
| `process_extent` | 28,970 | 28,970 | 28,970 | 28,970 | 28,970 | 28,970 |
| `process_recorded_refs` | 57,787 | 57,787 | 57,787 | 57,787 | 57,787 | 57,787 |
| `get_cur_path` | 123,219 | 123,219 | 123,219 | 123,219 | 123,219 | 123,219 |
| `get_first_ref` | — | — | — | — | 124,718 | 124,718 |
| `send_cmd` | 220,604 | 220,604 | 220,604 | 220,604 | 220,604 | 220,604 |
| `get_inode_info` | 1,380,373 | 1,380,373 | 1,380,373 | 1,380,373 | 1,380,373 | 1,380,373 |
| Underlying inode-item reads | 1,380,373 | 1,380,373 | 256,874 | 256,874 | 256,874 | 256,874 |
| `btrfs_search_slot` | 1,940,917 | 1,940,917 | 817,418 | 817,418 | 817,418 | 817,418 |
| `btrfs_clone_extent_buffer` | 1,915,888 | 535,515 | 792,389 | 535,515 | 243,921 | 117,809 |

The no-clone-only kernel made 1,893,808 `btrfs_search_slot()` calls, 47,109
fewer than baseline, while making the same 1,915,888
`btrfs_clone_extent_buffer()` calls. That explains why removing clone-source
discovery had little wall-time effect.

The scalar lookup and cache attack different components of the same call:
the scalar patch preserves the search but avoids cloning its returned leaf;
the cache avoids most searches entirely. Once composed, the 535,515 remaining
extent-buffer clones come from other send metadata operations, including
lookup-result preservation and compare-tree traversal.

The output experiments used the final composed stack and the direct-ioctl
helper for both control and experimental modes:

| Function | Normal output | Discard commands | Compact paths |
| --- | ---: | ---: | ---: |
| `find_extent_clone` | 28,968 | 28,968 | 28,968 |
| `process_extent` | 28,970 | 28,970 | 28,970 |
| `process_recorded_refs` | 57,787 | 57,787 | 57,787 |
| `get_cur_path` | 123,219 | 123,219 | 123,219 |
| `get_first_ref` | 124,718 | 124,718 | 124,718 |
| `send_cmd` | 220,604 | 220,604 | 220,604 |
| `get_inode_info` | 1,380,373 | 1,380,373 | 1,380,373 |
| `read_inode_info` | 256,874 | 256,874 | 256,874 |
| `btrfs_search_slot` | 791,559 | 791,559 | 791,559 |
| `btrfs_clone_extent_buffer` | 117,809 | 117,809 | 117,809 |
| `crc32c` | 1,400,518 | 1,179,914 | 1,179,917 |
| `kernel_write` | 220,605 | 1 | 60 |

The three surviving `crc32c()` calls in the compact count are run-level
metadata-read variation; compact mode performs no command CRC. The important
invariant is that the traversal and command counts are identical.

The two ordinary-stream optimizations have these exact send-scoped counts:

| Function | Per-command writes | Coalesced | + Cached utimes |
| --- | ---: | ---: | ---: |
| `send_utimes` | 56,582 | 56,582 | 56,582 |
| `send_cmd` | 220,604 | 220,604 | 220,604 |
| `get_inode_info` | 1,380,373 | 1,380,373 | 1,436,955 |
| `read_inode_info` | 256,874 | 256,874 | 258,527 |
| `btrfs_search_slot` | 791,559 | 791,559 | 736,630 |
| `btrfs_clone_extent_buffer` | 117,809 | 117,809 | 61,227 |
| `crc32c` | 1,400,518 | 1,400,518 | 1,400,518 |
| `kernel_write` | 220,605 | 323 | 323 |

The utime cache adds 56,582 calls to `get_inode_info()`, but 54,929 hit an
entry already populated by path resolution or other inode work. The remaining
1,653 misses replace the old direct timestamp searches one-for-one.

## Timings

Each comparison used five measured sends after one warm-up on one ptrace-mode
UML CPU. Values are mean wall time and sample standard deviation.

The cache comparison used adjacent unmodified and cache-enabled runs:

| Kernel/mode | Mean | Sample SD | Change |
| --- | ---: | ---: | ---: |
| Unmodified, default | 15.056 s | 0.316 s | baseline |
| Fixed inode cache, default | 8.034 s | 0.084 s | -46.6% |

The scalar lookup was then measured against preserved control binaries from
the same two build trees:

| Pair | Control | Scalar lookup | Change |
| --- | ---: | ---: | ---: |
| No inode cache | 16.182 ± 0.286 s | 8.872 ± 0.330 s | -45.2% |
| With inode cache | 9.418 ± 0.412 s | 7.754 ± 0.598 s | -17.7% |

The directory-item lookup was measured between two runs of the same preserved
cache-plus-scalar control kernel:

| Run order | Kernel | Mean | Sample SD |
| --- | --- | ---: | ---: |
| A | Control before | 7.124 s | 0.396 s |
| B | Scalar directory item | 4.882 s | 0.068 s |
| A | Control after | 6.550 s | 0.082 s |

The patch is 31.5% faster than the first control and 25.5% faster than the
second. The mean of all ten flanking control samples is 6.837 ± 0.405 seconds,
making the central patched run 28.6% faster in this warmed, one-vCPU UML
fixture. This is an observed point estimate, not a confidence bound; the two
control groups show host-load drift. The stream remained byte-identical to
every previous UML stream.

The first-reference lookup used the same A/B/A structure:

| Run order | Kernel | Mean | Sample SD |
| --- | --- | ---: | ---: |
| A | Directory-scalar control before | 4.632 s | 0.183 s |
| B | Scalar first reference | 3.748 s | 0.294 s |
| A | Directory-scalar control after | 4.762 s | 0.408 s |

The mean of all ten flanking control samples is 4.697 ± 0.306 seconds, making
the central patched run an observed 20.2% faster in this fixture. All sample
streams remained byte-identical.

Host load drifted between groups, so effects should be taken from each
adjacent pair rather than by comparing means from different pairs. None of the
reported scalar A/B sample ranges overlap.

The stream-level no-clone flag was measured on the same cached kernel:
8.034 ± 0.084 seconds in default mode and 8.004 ± 0.172 seconds with
`NO_CLONE`. That 0.4% difference is below run-to-run variation and should be
treated as no measurable improvement.

The command-output experiment used the same patched kernel and direct-ioctl
pipe helper for both A and B:

| Run order | Mode | Mean | Sample SD | Output bytes |
| --- | --- | ---: | ---: | ---: |
| A | Normal output before | 3.940 s | 0.240 s | 21,074,818 |
| B | Discard commands | 2.002 s | 0.230 s | 17 |
| A | Normal output after | 4.034 s | 0.318 s | 21,074,818 |
| C | Compact changed paths | 2.018 s | 0.150 s | 3,879,612 |

The mean of all ten normal-output samples was 3.987 ± 0.287 seconds.
`cpu-clock` profiles attributed only about 6.5% of normal send samples to
output CRC and write paths, much less than the 49.8% wall-time gap. The
remaining difference is predominantly off-CPU: 220,604 tiny command writes
fill the pipe and repeatedly block/wake the producer and splice reader.
Sleeping time and host-kernel scheduling are not sampled by the UML
`cpu-clock:u` event.

Write coalescing was measured against the preserved per-command-write kernel:

| Kernel | Mean | Sample SD | Writes |
| --- | ---: | ---: | ---: |
| Per-command writes | 5.488 s | 0.795 s | 220,605 |
| 64 KiB command coalescing | 2.880 s | 0.306 s | 323 |

This adjacent comparison shows a 47.5% reduction. The absolute control is
slower than earlier control groups because UML host load drifted, but the
write-count reduction is exact and the output SHA-256 is unchanged.

The timestamp patch used a fresh A/B/A sequence on top of write coalescing:

| Run order | Kernel | Mean | Sample SD |
| --- | --- | ---: | ---: |
| A | Coalescing control before | 2.762 s | 0.395 s |
| B | Cached inode timestamps | 1.926 s | 0.112 s |
| A | Coalescing control after | 2.194 s | 0.125 s |

The ten flanking controls averaged 2.478 ± 0.407 seconds, making the timestamp
patch 22.3% faster. All 15 protocol-v1 samples had SHA-256
`e1035cd0d887bf27b8135e1b1b206ba2bdc11db6b1fa4107b5ce3e327f3af317`.
A separate protocol-v2 gate compared two 21,980,130-byte streams byte for byte;
both had SHA-256
`0f34af5b38d30c5616af1902ea3e0c682bdb87534e849537a89e4d95516400af`.

Before selecting the fixed cache, a generic `btrfs_lru_cache` prototype was
tested at several sizes:

| Entries | Mean |
| ---: | ---: |
| 256 | 9.590 s |
| 1,024 | 8.870 s |
| 4,096 | 9.083 s |
| 16,384 | 9.350 s |

The fixed 512-set, two-way cache is faster despite making 18,743 more
underlying inode reads than the generic 1,024-entry LRU. Its hits require at
most two comparisons; it has no per-miss allocation, maple-tree operation, or
LRU update.

## Next target

Write coalescing and cached timestamp sourcing are now implemented as separate
commits. For ordinary streams, the next small scalar-lookup candidate is the
capability xattr path. The prior post-first-ref profile contained 303 sampled
clone stacks under `btrfs_lookup_xattr()`. The remaining compare-tree clones
need a more invasive bounded-item snapshot and resume design.

The compact manifest demonstrates what a watcher-specific format can achieve,
but it is not yet a correct watcher ABI. It retains up to 128 MiB of unique
path state, reports receiver-stream-time names (including temporary orphan
names), loses operation ordering and multiplicity, and does not enumerate
hardlink aliases or descendants of renamed directories. It is restricted to
protocol v1 and cannot be received as a Btrfs stream.

The Rust detector now recognizes both the actual `dest=PATH` rename spelling
from `btrfs receive --dump` and the older test's `OLD -> NEW` spelling. The
end-to-end timing run confirmed equal normal and compact path counts.

Before the timestamp patch, 117,809 extent-buffer clones remained. That
post-first-ref profile had
`btrfs_search_slot()` at 20.15% inclusive, `get_cur_path()` at 17.84%,
`send_utimes()` at 13.41%, and `btrfs_clone_extent_buffer()` at 7.66%; these
percentages overlap.

Of 1,601 sampled remaining clone stacks, 595 contain `send_utimes()`, 303
contained `btrfs_lookup_xattr()`, and 673 contained the compare-tree
`tree_move_down()` / `replace_node_with_clone()` path. The first-reference and
directory-item paths contained zero leaf-clone samples. Cached timestamp
sourcing has now removed the 56,582 corresponding exact clone calls.
`summarize-clone-stacks.sh` produces the sampled overlapping classification
from `uml-send.perf.data`; the checked summaries are in
`results/clone-stack-samples.tsv`.

## Why `--no-data` can still emit `CLONE`

`NO_FILE_DATA` controls the fallback that would otherwise emit `WRITE` or
`ENCODED_WRITE`. The kernel performs clone-source discovery first. If it finds
a legal source, it emits `CLONE`, which contains paths, UUID/transid, offsets,
and length but no copied file payload. A receiver uses that command to
reflink the destination range from a previously available source.

If no clone source is usable, `NO_FILE_DATA` makes the fallback emit
`UPDATE_EXTENT`. Therefore changing existing `NO_FILE_DATA` behavior to skip
clone discovery would silently weaken stream semantics. Patch 0001 instead
adds an independent opt-in `NO_CLONE` bit:

- `NO_CLONE` alone falls back to `WRITE`/`ENCODED_WRITE`;
- `NO_CLONE | NO_FILE_DATA` falls back to `UPDATE_EXTENT`;
- existing `NO_FILE_DATA` callers remain unchanged.

The profiled `btrfs_clone_extent_buffer()` function is unrelated to a
send-stream `CLONE` command. It copies an in-memory B-tree block for safe
commit-root traversal.

## Kernel history

The local checkout is `v7.2-rc4-503-g3dab139d4795`; there is no local or
upstream-style `v7.12` tag in this checkout.

Between `v7.0` and this checkout, the send changes are type cleanups,
automatic `fs_path` freeing, literal boolean conversions, and unrelated UAPI
work. None caches inode items or removes the profiled commit-root searches.
Three generic B-tree-search micro-optimizations in v7.1 remove redundant
uptodate checks, one duplicate offset calculation, and tighten integer types
(`90b7d4c415b2`, `7e1e45a9e42e`, and `a5b6b23c4572`). A v7.2 change,
`23fd95663b07`, changes allocation policy for extent-buffer folios. These may
make individual operations slightly cheaper but do not remove any of the
repeated searches, allocations, or full-block copies.

If `v6.12` was intended, later relevant changes already present in the
profiled v7.0 kernel include:

- `fc746acb7aa9`: cache the current inode's path;
- `374d45af6435`: avoid current-inode path allocation when issuing commands;
- `0c8337c22043`: remove an unnecessary encoded-inline inode lookup;
- `0dc93e465289`: index the send backref cache by node number.

The first two path changes had a useful upstream full-send benchmark, but the
profiled v7.0 kernel already contains them. The encoded-inline change does not
apply to a no-data send, and the backref-cache key change was not expected to
change its hit rate. `git log -L` shows that `get_inode_info()` itself has had
only an automatic path-freeing cleanup since v6.12; its definition and ten
call sites remain. None eliminates the repeated inode-item searches measured
here.

## Test mechanism and limits

The UML harness is the best first-stage mechanism in this environment:

- it boots the exact modified kernel and mounts a real Btrfs image;
- it runs without a privileged VM or nested virtualization;
- host `perf` and bpftrace uprobes can profile and count kernel functions;
- a one-CPU ptrace UML guest is sufficient because Btrfs send is
  single-threaded.

UML's UBD path is not representative of a native block stack, so these
measurements establish CPU/control-flow improvements rather than storage
latency. A second-stage native or KVM guest should test cold-cache behavior,
relocation concurrency, lockdep/KASAN, full-data send/receive, and crash/error
paths. `/dev/kvm` is not exposed in the current environment, so a nested guest
would use slow QEMU TCG unless nested KVM is enabled.

Remaining correctness fixtures before proposing the inode cache upstream:

1. an ENOENT-heavy rename/delete workload to stress negative caching;
2. full and incremental data streams received and compared recursively;
3. concurrent relocation while sending;
4. allocation-failure injection to verify transparent cache disablement.

## Artifacts

- `README.md`: reproduction instructions;
- `a86246a467a3`: independent no-clone ABI experiment;
- `4b0ed36b3a5b`: fixed inode-info cache;
- `9e63650d2b2c`: scalar lookup with explicit
  commit-root lifetime;
- `9e4092a25535`: scalar directory-item
  lookup with explicit commit-root lifetime;
- `cb675033e9b1`: scalar first-reference lookup
  with bounded name copying;
- `ff3480e9e018`: best-effort 64 KiB
  command batching for metadata-only streams;
- `ce78bf7b1b68`: raw timestamp
  reuse through the inode-info cache;
- `5210746a1c70`: `GET_SUBVOL_INFO` offset-zero error handling;
- `91d4aeeef658`: changed-objects-v2 ioctl and stream implementation;
- `patches/send-ioctl.c`: direct ioctl helper for normal, no-clone, discard,
  compact-path, and protocol-v2 equivalence modes;
- `validate-changed-paths.py`: checks compact records and command masks against
  an ordinary binary send stream;
- `guest-run.sh` `detector-timing` mode: exercises the Rust comparison CLI in
  the UML guest;
- `results/`: raw timing samples, exact call counts, and stream fingerprints;
- `count-uml-calls.sh`: exact per-send function-entry counts.
