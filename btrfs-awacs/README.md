# btrfs-awacs

AWACS (Always Watching All Change Streams) builds an indexed Btrfs change
stream and provides Watchman-compatible filesystem monitoring. The
`btrfs-awacs` binary also captures read-only snapshots and benchmarks two
ways of measuring changes between the two most recent snapshots.

    cargo build --release
    ./target/release/btrfs-awacs --help
    ./target/release/btrfs-awacs snap /path/to/subvolume
    ./target/release/btrfs-awacs compare /path/to/subvolume

The executable uses Clap for every ordinary command. Its default help lists
the benchmark commands, `broker-serve`, `watchman-serve`, and every diagnostic
or acceptance subcommand. The installed `watchman`, `btrfs-awacs-watchman`,
and `git-fsmonitor-hook` names are multicall entry points into this same binary
and are listed at the end of the default help.

`snap` creates a timestamped snapshot under
`/path/to/subvolume/.btrfs-awacs/`. `compare` compares the two most recent
snapshots without creating or deleting any snapshots. It exits with an error
when fewer than two snapshots exist. Every run prints total elapsed time before
exiting.

`compare` runs two detectors against the same snapshot pair. It reports the
count and elapsed time for the ordinary
`btrfs send --no-data | btrfs receive --dump` path and for the changed-object
ioctl, followed by their relative speeds. The object count is an inode count,
not a path count; raw reference records describe hardlink and rename deltas.
The latter mode requires the experimental changed-object kernel interface; an
unmodified kernel rejects it.

All Btrfs operations run through `sudo --non-interactive` internally because
snapshot and send permissions vary by system. Configure passwordless sudo for
Btrfs, or the command will fail without prompting.

The same tree now contains a service prototype implementing the persistent
inode/reference index, transactional snapshot cuts, tracked writable Worktree
clones, authenticated Watchman/jj and Git projections, root broker boundary,
receipt-backed mutations, retention, and garbage collection specified in
[Indexed Btrfs change tracking](docs/indexed-change-tracking.md). The
`broker-serve` command is the external privileged endpoint; the UML smoke test
exercises the manager through it. This remains an experimental-kernel
prototype, not an installed system service.

The focused compatibility daemon is started with `watchman-serve`; it opens an
existing manager database, connects to `broker-serve`, binds a mode-0600 socket
inside the calling user's private runtime directory, and registers one seed
watch/grant pair. Authenticated `watch-project` calls from the same UID,
process-root, and mount view can then reuse or initialize additional exact
subvolume roots (including tracked Worktrees) in that daemon. Set
`BTRFS_AWACS_EXPERIMENTAL_DIRTY_WITNESS=1` only on a
kernel whose dirty-witness behavior has passed the documented matrix. Point the
standalone `git-fsmonitor-hook` helper at that socket with
`BTRFS_AWACS_SOCKET`. Alternatively, install the focused `watchman` and Git
helper symlinks with `packaging/install.sh`. The shim derives
`$XDG_RUNTIME_DIR/btrfs-awacs/mnt-DEV-INO/watchman.sock` from the caller's mount
namespace and serializes daemon startup with `daemon.lock`. Automatic startup
requires `BTRFS_AWACS_ROOT`, `BTRFS_AWACS_MANAGED_DIR`,
`BTRFS_AWACS_SPOOL_DIR`, `BTRFS_AWACS_MANAGER_DB`,
and `BTRFS_AWACS_BROKER_SOCKET`. `BTRFS_AWACS_WATCH_ID` and
`BTRFS_AWACS_GRANT_ID` may select an existing registration; when both are
omitted, the daemon reuses the active root/uid grant or transactionally runs
Initialize. An already-running daemon needs none of these variables. Set
`BTRFS_AWACS_PRECISION_GUARD=1` to opt into the recursive precision journal.
To permit jj's optional snapshot trigger, set `BTRFS_AWACS_JJ` to the absolute
jj executable before daemon activation; the daemon otherwise rejects trigger
registration rather than accepting work it cannot run. The non-root scheduler
takes periodic synchronized cuts every
`BTRFS_AWACS_TRIGGER_INTERVAL_MS` (default 1000 ms), claims durable runs under a
fence, drops the facade lock while executing `jj --quiet util snapshot`, and
then records completion. When the precision guard is enabled, its inotify
descriptor wakes this scheduler before that maximum interval; loss of the
optional guard retains the periodic correctness path. Dynamic exact roots get
their own guard, and failed triggers rotate fairly behind less-run roots.
Moves wholly beneath `.git` or `.jj` are proven
nonmatches and do not recursively schedule jj.

The packaging directory also includes a hardened systemd broker unit and an
example numeric manager uid/gid environment file. The system service is the
only installed component that runs as root; the namespace daemon, discovery
shim, Git hook, SQLite manager, and precision journal run as the user.

## End-to-end Worktree matrix

`./run_e2e.sh` runs a privileged integration matrix on Btrfs. Each initial
snapshot-tree variation is combined with every Worktree modification variation.
Every matrix cell initializes the real service, creates its managed snapshots,
publishes a writable Worktree through `Service::worktree`, activates the proved
Worktree seed through `watch-project`, applies one modification, and verifies
the incremental paths through authenticated BSER-v2 Watchman transport and a
direct facade cross-check. The current three snapshot shapes and four
modifications produce 12 independent cases. Add an entry to either variation
table in `src/bin/btrfs-awacs-e2e.rs` to extend a complete row or column of the
matrix.

The runner builds without privilege and uses `sudo` only for the test binary.
Set `BTRFS_AWACS_E2E_ROOT` to select an existing directory on the Btrfs
filesystem. The host kernel must include the `GET_SUBVOL_INFO` fix and
changed-object ABI described in the UML harness documentation. The runner
preflights `GET_SUBVOL_INFO` once before expanding the matrix, while each matrix
cell exercises the changed-object path. Pass `--keep-failures` to preserve
failed case directories.

The [UML profiling harness](harness/README.md) reproduces the OpenAI snapshot
workload on a modified kernel. Its [results](harness/RESULTS.md) include exact
call counts plus independent inode-info cache and scalar-lookup patches, each
cutting the measured send time by about 45% in paired tests.
