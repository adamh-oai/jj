# Btrfs send UML harness

This harness builds a User-Mode Linux kernel and reproduces the
`btrfs send --no-data` workload against an isolated copy of the two newest
`~/code/openai/.btrfs-awacs` snapshots.

See [RESULTS.md](RESULTS.md) for the profile interpretation, exact call
counts, A/B timings, kernel-history review, and remaining validation.

The fixture is made with a protocol-v2 full send followed by an incremental
send, both using `--compressed-data`. This transfers the real file data and
preserves encoded compressed extents where possible; it is not a `--no-data`
copy. It contains only the OpenAI snapshot pair, not other subvolumes from the
source filesystem. Send/receive reconstructs the logical tree, so it does not
preserve the source filesystem's exact physical B-tree layout.

Run:

```sh
./harness/create-fixture.sh
./harness/build-uml.sh
./harness/make-initramfs.sh
./harness/run-uml.sh
```

The dedicated watcher implementation starts at the bottom of the custom
Ubuntu HWE 7.0 stack in `~/code/linux` with `6486379e59f1` (`btrfs: send: add
a changed object stream`). The fd-anchored v2 ioctl 66 ABI is introduced by
`3ce9f6629cf2`; subsequent commits `882e343a5dd3`, `0d37af11ce82`, and
`9b50bfebc7bd` remove its full-index mode, distinguish file-data from
directory-entry changes, and namespace its change-mask constants. It reports
snapshot-to-snapshot deltas only, with endpoint identities, exact target
metadata/security xattrs, complete nested-subvolume boundary transitions, an
explicit dirty-witness capability, caller byte/record limits, and a CRC32C
completion record. The one-line `GET_SUBVOL_INFO` return fix needed by this HWE
base is `eaf2c9218851`. The ioctl deliberately retains `CAP_SYS_ADMIN`; the
service exercises it only in the broker. Initial index construction is a
userspace traversal rather than an ioctl mode.

To build installable Ubuntu Noble HWE packages from the currently checked-out
kernel tree:

```sh
./harness/build-ubuntu-deb.sh
```

The script temporarily gives the package a non-stock ABI, restores the HWE
changelog on exit, disables unrelated tools and out-of-tree DKMS builds, and
builds Ubuntu's `binary-headers` and `binary-generic` targets. By default it
uses ABI `2801`, build name `btrfs-fast-snap`, package revision `2`, and writes
packages, checksums, and the complete build log below
`/tmp/btrfs-fast-snap-debs`. The default package version therefore contains
the distinctive upstream suffix in
`7.0.0+btrfs-fast-snap-2801.28+build2~24.04.1`, and the kernel release is
`7.0.0+btrfs-fast-snap-2801-generic`.

The main overrides are:

```sh
LINUX_DIR=~/code/linux \
OUTPUT_DIR=/tmp/btrfs-fast-snap-debs \
ABI=2801 BUILD_NAME=btrfs-fast-snap BUILD_REVISION=2 JOBS="$(nproc)" \
  ./harness/build-ubuntu-deb.sh
```

Increment `BUILD_REVISION` when rebuilding changed sources with the same ABI,
or set `BUILD_NAME` to another lowercase, version-safe label. `PACKAGE_VERSION`
remains available when the complete Debian version needs to be overridden,
but its final hyphen-separated revision must begin with the selected ABI.

The kernel checkout must have no tracked modifications. The build host needs
the dependencies declared by the Ubuntu HWE source, including `debhelper`,
`pahole`, `bindgen-0.65`, `rustc-1.91`, `rust-1.91-src`, `rustfmt-1.91`, and
`clang-19`. The script removes inherited `PYTHONSAFEPATH` and `RUST_LOG`
settings while building so Ubuntu's source-local `kconfig` helper remains
importable and bindgen does not emit unbounded debug logs.

Artifacts are written below `/tmp/btrfs-awacs-uml`. The guest performs one
warm-up and ten measured sends, saves a no-data stream and dump below
`results/`, and records a host-side perf profile in `uml-send.perf.data`.
The guest uses one virtual CPU because ptrace-mode UML does not support SMP;
the send workload itself is single-threaded.
Set `RUNS` to change the ten timed repetitions or `WARMUP=0` to skip the
default warm-up.

The fixture script requires passwordless `sudo` for mount, unmount, and Btrfs
send/receive operations. The UML kernel itself runs unprivileged.

For exact function-entry counts, run:

```sh
./harness/count-uml-calls.sh
```

This runs the same guest workload under bpftrace uprobes and writes
`results/uml-call-counts.txt`. Counters are enabled only while
`btrfs_ioctl_send()` is active, so boot and mount activity is excluded. Since
probing every hot function is intrusive, the count workload skips the warm-up
and timed repetitions and performs one final retained send.
`results/uml-call-symbols.tsv` records the actual ELF symbols used, including
compiler-generated `.constprop` clones.
On an inode-cache kernel it also reports `read_inode_info`, which is the
underlying B-tree lookup; that symbol is absent and therefore skipped on the
baseline kernel.

Copy or rename `results/` between kernel variants when retaining every run;
the guest intentionally overwrites the stream, dump, hash, and timing files
for the current kernel.

Exact counting needs passwordless `sudo` for bpftrace. The traced UML process
is dropped back to the invoking uid and gid before it starts.

To resolve all required symbols and compile the bpftrace program without
starting UML, use `./harness/count-uml-calls.sh --dry-run`.

To classify sampled `btrfs_clone_extent_buffer()` stacks in a retained perf
profile, run:

```sh
./harness/summarize-clone-stacks.sh LABEL /path/to/uml-send.perf.data
```

The categories can overlap because they describe symbols anywhere in the same
sampled stack. `results/clone-stack-samples.tsv` contains the control and
first-reference-patch summaries used in `RESULTS.md`.

The experimental no-clone kernel can be exercised without a matching
btrfs-progs build:

```sh
KERNEL=/tmp/btrfs-awacs-uml/kernel-no-clone/linux \
  SEND_MODE=no-clone \
  ./harness/run-uml.sh
```

`make-initramfs.sh` builds a small `send-ioctl` direct-ioctl helper. Available
`SEND_MODE` values are:

- `profile-default`: `NO_FILE_DATA`, through the same helper and pipe path as
  the experimental modes;
- `profile-default-v2`: the same metadata-only stream using protocol v2,
  primarily as a timestamp-byte equivalence gate;
- `no-clone`: `NO_FILE_DATA | NO_CLONE`;
- `discard-commands`: construct commands but skip their CRC and writes;
- `changed-objects`: emit one compact change mask per inode plus raw inode-ref
  additions and deletions, bypassing receive-command and path synthesis.
- `detector-timing`: run the current `btrfs-awacs compare` CLI against the
  fixture and report both end-to-end detector times.
- `btrfs-ioctl-smoke`: identify and revalidate both fixture snapshots through
  the Rust `FS_INFO` and `GET_SUBVOL_INFO` ioctl wrappers without invoking
  `btrfs-progs` for identity lookup. If `IMAGE` does not exist, the runner
  creates a disposable 256 MiB Btrfs image and the UML guest creates the two
  read-only snapshots itself. This mode also starts the external broker and
  per-user facade, then runs the bundled real `jj` and system `git` binaries
  through jj's Watchman backend, the snapshot trigger, and Git's hook-v2
  fsmonitor contract. It also proves that a non-admin process which can read
  both snapshots is denied the direct changed-objects ioctl, and that adding a
  nested subvolume is reported by v2 and cannot advance the indexed watch. It
  runs the snapshot-only dirty-witness matrix before enabling inotify precision. Their exact
  versions are saved in `results/jj-version.txt` and `results/git-version.txt`.

The `changed-objects` mode produces a profiling format that cannot be received.
It is a versioned prototype watcher ABI; the UML runner validates its complete framing,
cross-record invariants, and normalized reference counts after every run.

For example:

```sh
KERNEL=/tmp/btrfs-awacs-uml/kernel-changed-objects/linux \
  SEND_MODE=detector-timing RUNS=0 WARMUP=0 \
  ./harness/run-uml.sh

KERNEL=/tmp/btrfs-awacs-uml/kernel-changed-objects/linux \
  SEND_MODE=changed-objects RUNS=5 \
  ./harness/run-uml.sh

```

`validate-changed-objects.py` checks changed-object framing, masks, object/ref
relationships, mandatory boundary coverage, and reports raw and normalized
reference and nested-boundary counts.

The scalar lookup changes started as standalone experiments against
`3dab139d4795`; their authoritative versions are individual commits in
`~/code/linux`, stacked in measured order:

- `a09636e2eb5b` caches repeated inode-item
  reads;
- `2e1fe612be5b` copies scalar
  inode fields while holding `commit_root_sem`, avoiding a full leaf clone;
- `d17d9a7e67e8`
  copies a directory item's location key, or consumes its existence result,
  while holding `commit_root_sem`;
- `8153cc0f51d7` copies the
  selected inode reference's parent and bounded name while holding
  `commit_root_sem`.
- `8f10d8359ca9` batches completed
  metadata-only commands into 64 KiB writes without changing stream bytes;
- `35f414a1ed69` copies raw
  inode timestamps into the inode-info cache and makes `send_utimes()` reuse
  them.

The commits retain the adaptations needed to compose the originally
independent experiments. `RESULTS.md` records both independent and composed
measurements.
