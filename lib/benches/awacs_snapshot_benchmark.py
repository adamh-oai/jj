#!/usr/bin/env python3
"""Run already-built control/treatment TreeState examples on identical fixtures."""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import math
from pathlib import Path
import statistics
import subprocess
import tempfile


def checked_path(value: str) -> bool:
    if not isinstance(value, str) or not value or value.startswith("/"):
        raise ValueError("inventory paths must be nonempty relative strings")
    if any(char in value for char in "\t\r\n\0"):
        raise ValueError("inventory paths must not contain tabs, newlines, or NUL")
    parts = value.split("/")
    if any(part in ("", ".", "..") for part in parts):
        raise ValueError("inventory path contains an unsafe component")
    return not any(part in (".git", ".jj") for part in parts)


def parents(path: str):
    parts = path.split("/")
    for length in range(1, len(parts)):
        yield "/".join(parts[:length])


def sample(values: list[str], count: int) -> set[str]:
    """Take an evenly spaced, deterministic sample of an already sorted list."""
    count = min(count, len(values))
    if count == 0:
        return set()
    return {values[index * len(values) // count] for index in range(count)}


def prepare_inventory(args, destination: Path) -> dict:
    exact = json.loads(args.paths_json.read_text())
    if not isinstance(exact, list):
        raise ValueError("--paths-json must contain an array of paths")
    if len(set(exact)) != len(exact):
        raise ValueError("--paths-json contains duplicate paths")
    paths = sorted(path for path in exact if checked_path(path))
    topology = {}
    for line in args.topology_tsv.read_text().splitlines():
        kind, path = line.split("\t", 1)
        if kind not in ("f", "d", "s"):
            raise ValueError("topology kinds must be f, d, or s")
        if checked_path(path):
            topology[path] = kind
    records = {path: topology.get(path, "x") for path in paths}
    files = [path for path, kind in records.items() if kind == "f"]
    keep_count = min(len(files), max(1, math.ceil(len(files) * args.scale)))
    if args.limit is not None:
        keep_count = min(keep_count, args.limit)
    if not files:
        raise ValueError("inventory has no regular files")
    if keep_count < len(files):
        selected = sample(files, keep_count)
        ratio = keep_count / len(files)
        symlinks = [path for path, kind in records.items() if kind == "s"]
        selected.update(sample(symlinks, math.ceil(len(symlinks) * ratio)))
        absent = [path for path, kind in records.items() if kind == "x"]
        absent_parents = {parent for path in absent for parent in parents(path)}
        absent_leaves = [path for path in absent if path not in absent_parents]
        # A removed child beneath a new file needs that target file to remain
        # selected too; otherwise the sample changes the replay's path shape.
        absent_leaves = [
            path
            for path in absent_leaves
            if all(records.get(parent) != "f" or parent in selected for parent in parents(path))
        ]
        selected.update(sample(absent_leaves, math.ceil(len(absent_leaves) * ratio)))
        selected.update(parent for path in tuple(selected) for parent in parents(path))
        records = {path: kind for path, kind in records.items() if path in selected}
    content = "".join(f"{kind}\t{path}\n" for path, kind in records.items())
    with destination.open("x") as output:
        output.write(content)
    return {
        "input_exact_paths": len(exact),
        "excluded_reserved_paths": len(exact) - len(paths),
        "selected_paths": len(records),
        "selected_kinds": dict(Counter(records.values())),
        "inventory_sha256": hashlib.sha256(content.encode()).hexdigest(),
    }


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--control", type=Path)
    parser.add_argument("--treatment", type=Path)
    parser.add_argument("--paths-json", type=Path)
    parser.add_argument("--topology-tsv", type=Path)
    parser.add_argument("--ignore-paths", type=Path, help="JSON array of supplemental .gitignore placements")
    parser.add_argument("--inventory-out", type=Path)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--files", type=int, default=10_000)
    parser.add_argument("--scale", type=float, default=1.0)
    parser.add_argument("--limit", type=int)
    parser.add_argument("--bytes", type=int, default=20_480)
    parser.add_argument("--seed", type=int, default=17)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--mode", choices=("all", "files-only", "both"), default="both")
    parser.add_argument("--output", type=Path, help="New JSONL result file; existing files are refused")
    parser.add_argument("--allow-treatment-duplicates", action="store_true")
    args = parser.parse_args()
    if (args.paths_json is None) != (args.topology_tsv is None):
        parser.error("--paths-json and --topology-tsv must be supplied together")
    if not 0 < args.scale <= 1 or args.limit is not None and args.limit < 1:
        parser.error("scale must be in (0, 1], and limit must be positive")
    if args.files < 1 or args.runs < 1 or args.bytes < 128:
        parser.error("files/runs must be positive and bytes must be at least 128")
    if args.prepare_only:
        if args.paths_json is None or args.inventory_out is None:
            parser.error("prepare-only requires paths-json, topology-tsv, and inventory-out")
    elif args.control is None or args.treatment is None:
        parser.error("benchmark runs require --control and --treatment")
    return args


def main():
    args = arguments()
    with tempfile.TemporaryDirectory(prefix="jj-awacs-snapshot-benchmark-") as temporary:
        inventory = None
        metadata = {"fixture": "synthetic-depth-5"}
        if args.paths_json:
            inventory = args.inventory_out or Path(temporary) / "inventory.tsv"
            metadata = prepare_inventory(args, inventory)
        if args.prepare_only:
            print(json.dumps(metadata, sort_keys=True))
            return
        modes = ("all", "files-only") if args.mode == "both" else (args.mode,)
        count = max(1, math.ceil(args.files * args.scale))
        if args.limit is not None:
            count = min(count, args.limit)
        shared = ["--files", str(count), "--bytes", str(args.bytes), "--seed", str(args.seed)]
        if inventory:
            shared += ["--inventory", str(inventory.resolve())]
        if args.ignore_paths:
            placements = json.loads(args.ignore_paths.read_text())
            if not isinstance(placements, list) or not all(isinstance(path, str) for path in placements):
                raise ValueError("ignore-paths must contain an array of relative .gitignore paths")
            shared += ["--ignore-paths", str(args.ignore_paths.resolve())]
            metadata["ignore_paths_file_sha256"] = hashlib.sha256(args.ignore_paths.read_bytes()).hexdigest()
            metadata["supplied_ignore_paths"] = len(placements)
        executables = {"control": args.control.resolve(), "treatment": args.treatment.resolve()}
        for path in executables.values():
            if not path.is_file():
                raise ValueError(f"benchmark executable does not exist: {path}")
        provenance = {
            variant: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
            for variant, path in executables.items()
        }
        output = args.output.open("x") if args.output else None
        try:
            def emit(value):
                text = json.dumps(value, sort_keys=True)
                print(text, flush=True)
                if output:
                    output.write(text + "\n")
                    output.flush()

            emit({"kind": "provenance", "executables": provenance, "inventory": metadata})
            records = []
            reference = {}
            semantic_reference = None
            variant_counts = {}
            invariant_fields = (
                "fixture_sha256", "exact_paths_sha256", "base_tree", "expected_tree",
                "ignore_layout", "ignore_layout_sha256",
                "result_tree", "validated_content_sha256", "covered_paths_sha256",
                "regular_files", "deleted_files", "ignore_files", "target_files",
                "exact_paths", "unique_file_visits", "seed", "mean_file_bytes_parameter",
            )
            for mode in modes:
                for run in range(args.runs):
                    order = ("control", "treatment") if run % 2 == 0 else ("treatment", "control")
                    for variant in order:
                        command = [str(executables[variant]), *shared, "--mode", mode]
                        completed = subprocess.run(command, text=True, capture_output=True, check=False)
                        if completed.returncode:
                            raise RuntimeError(
                                f"{variant}/{mode} exited {completed.returncode}:\n{completed.stderr[-8000:]}"
                            )
                        result = json.loads(completed.stdout)
                        if result["schema"] != "awacs-snapshot-benchmark-v1":
                            raise ValueError("unexpected benchmark schema")
                        if result["result_tree"] != result["expected_tree"]:
                            raise AssertionError("benchmark returned a different semantic tree")
                        semantics = tuple(result[key] for key in (
                            "fixture_sha256", "base_tree", "expected_tree", "result_tree",
                            "validated_content_sha256",
                        ))
                        if semantic_reference is not None and semantic_reference != semantics:
                            raise AssertionError("the two scan modes have different semantic fixtures")
                        semantic_reference = semantics
                        if variant == "treatment" and not args.allow_treatment_duplicates:
                            if result["file_visits"] != result["unique_file_visits"]:
                                raise AssertionError("treatment still processes at least one file more than once")
                        invariants = {key: result[key] for key in invariant_fields}
                        if mode in reference and reference[mode] != invariants:
                            raise AssertionError("control/treatment fixture, semantic result, or visited path coverage differs")
                        reference[mode] = invariants
                        count_key = (mode, variant)
                        counts = result["visit_counts_sha256"]
                        if count_key in variant_counts and variant_counts[count_key] != counts:
                            raise AssertionError("file visit counts changed between repeated runs")
                        variant_counts[count_key] = counts
                        record = {"kind": "run", "variant": variant, "run": run, **result}
                        records.append(record)
                        emit(record)
            metrics = (
                "setup_seconds", "snapshot_seconds", "snapshot_user_seconds",
                "snapshot_system_seconds", "snapshot_rchar", "snapshot_read_bytes",
                "file_visits", "unique_file_visits", "max_file_visits", "processed_file_bytes",
            )
            summaries = {}
            for mode in modes:
                summaries[mode] = {}
                for variant in executables:
                    selected = [r for r in records if r["mode"] == mode and r["variant"] == variant]
                    summaries[mode][variant] = {
                        metric: {
                            "median": statistics.median(r[metric] for r in selected),
                            "min": min(r[metric] for r in selected),
                            "max": max(r[metric] for r in selected),
                        }
                        for metric in metrics
                    }
                control = summaries[mode]["control"]["snapshot_seconds"]["median"]
                treatment = summaries[mode]["treatment"]["snapshot_seconds"]["median"]
                summaries[mode]["snapshot_speedup"] = control / treatment
                summaries[mode]["snapshot_reduction_fraction"] = 1 - treatment / control
            emit({"kind": "summary", "runs_per_variant_per_mode": args.runs, "modes": summaries})
        finally:
            if output:
                output.close()


if __name__ == "__main__":
    main()
