# Compact TreeState snapshot benchmark

This benchmark calls the actual public `TreeState::snapshot` implementation with
`FsmonitorSettings::TestAwacs`. Its source, Git store, and journal are temporary.
It does not access or change a real worktree. The synthetic monitor isolates JJ
reconciliation from AWACS/Btrfs comparison and command startup.

The example uses workspace dependencies, including serde_json as a dev dependency.
Build the **same example source** against both the pinned control checkout and
the treatment checkout, using identical profile/features. Build/run scheduling
is intentionally separate from the Python runner.

```sh
cargo build --release -p jj-lib --example awacs_snapshot_benchmark
```

Run the already-built binaries:

```sh
python3 lib/benches/awacs_snapshot_benchmark.py \
  --control /absolute/path/control-awacs-snapshot-benchmark \
  --treatment /absolute/path/treatment-awacs-snapshot-benchmark \
  --files 10000 --bytes 20480 --mode both --runs 3 \
  --output /tmp/awacs-snapshot-comparison.jsonl
```

The default fixture has five shared directory levels and variable-sized,
deterministic, compressible contents with distinct old/new values. It includes
added/modified/unchanged files, removed files/directories, and a tracked
`.gitignore` with harmless rules at every present directory. This default is an
ignore-heavy stress fixture, not representative repository ignore placement.
All old and target blobs are already in a real temporary Git object store before timing, matching
the case where an external Git checkout already has the target objects.

`all` reports changed files and their exact directory entries, exercising
overlapping traversal. `files-only` reports the same changed leaves without
present directory entries, keeping repeated ancestor ignore-context work visible
after directory deduplication. Both modes should produce the same semantic target.
Deleted ancestor boundaries remain in both modes so the synthetic exact delta
still proves why their descendant files are absent.

To use an externally supplied replay shape without checking private repository
paths into this repository, provide a JSON array of exact relative paths plus a
TSV target topology (`f`, `d`, or `s`, tab, relative path):

```sh
python3 lib/benches/awacs_snapshot_benchmark.py \
  --control /absolute/path/control-awacs-snapshot-benchmark \
  --treatment /absolute/path/treatment-awacs-snapshot-benchmark \
  --paths-json /absolute/path/replay-paths.json \
  --topology-tsv /absolute/path/target-topology.tsv \
  --limit 10000 --bytes 20480 --mode both --runs 3 \
  --output /tmp/awacs-replay-shape-comparison.jsonl
```

This reuses path shape, **not historical file contents or sizes**. File bytes
remain deterministic synthetic data. Missing target paths are modeled as
deletions. Reserved `.git`/`.jj` paths are excluded and counted. `--scale` selects
a fraction in `(0, 1]`; `--limit` caps the regular-file sample. Selection is
deterministic and retains relevant exact ancestors. Omitting both uses the full
inventory. Symlinks point to a synthetic missing target inside the fixture.

For a closer approximation of repository ignore placement, add
`--ignore-paths /absolute/path/current-ignore-paths.json` to the runner (or Rust
example). The JSON array contains relative `.gitignore` paths observed through
metadata only. Supplemental harmless ignore files are created only at listed
paths whose parent directories already occur in the fixture; no unrelated
directories are added. Exact inventory `.gitignore` files remain ordinary
fixture files even when absent from this optional list. An empty list disables
supplemental ignore files. Original rule contents are never copied, so this
models placement rather than real ignore semantics or parsing volume. Output
records the layout mode and hash, and includes them in the fixture fingerprint.

The preprocessing step can also run without binaries:

```sh
python3 lib/benches/awacs_snapshot_benchmark.py --prepare-only \
  --paths-json /absolute/path/replay-paths.json \
  --topology-tsv /absolute/path/target-topology.tsv --limit 10000 \
  --inventory-out /tmp/awacs-benchmark-inventory.tsv
```

Every timed sample starts from an independently constructed retained semantic
baseline and freshly prepared target filesystem. Setup includes object-store
construction, filesystem population, baseline journal preparation, and
TreeState loading. These costs are reported separately. The timer surrounds
only `TreeState::snapshot`, including directed scope preparation, file
processing, tree reconstruction, and the direct caller's synthetic lease abort.

The progress callback counts actual file processing, using a prebuilt path
lookup and relaxed atomic counters. The benchmark reports total/unique/maximum
visits and a byte total weighted by those measured visits. Callback accounting
adds common overhead to both variants. This byte total excludes ignore reads,
old Git blob reads, and filesystem metadata.

Unix CPU times, page faults, and context switches come from `getrusage` around
the snapshot. Linux `/proc/self/io` additionally supplies `rchar` (read-like
bytes including cached reads) and `read_bytes` (storage I/O); neither is a
direct old-versus-new content attribution. Metrics collection is outside the
wall timer. Setup touches the data, but caches are neither explicitly flushed
nor pinned. Storage I/O and page faults remain possible and are reported; do not
categorize a run as warm-cache solely because setup preceded it. Git objects are
loose, not production packfiles.

After timing, the example checks the complete resulting semantic tree against
an independently built target and reads every resulting blob/symlink to verify
its expected hash, length, mode, and content. The runner also requires control
and treatment to agree on fixture hashes, semantic tree IDs, content hashes,
and the exact set of visited files. Visit counts must be stable across repeats;
treatment must visit each covered file once, unless the explicit
`--allow-treatment-duplicates` option is supplied for diagnostic comparisons.

Runs alternate control/treatment ordering and report medians/ranges. The JSONL
output records binary hashes, fixture hashes, each validated run, and a final
summary; existing output and inventory files are refused. An initial small
fixture is useful for validating the harness before a full replay-shaped run.
