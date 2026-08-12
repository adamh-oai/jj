#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
test_root=${BTRFS_AWACS_E2E_ROOT:-"$repo_dir/target/btrfs-awacs-e2e"}

cd "$repo_dir"
mkdir -p "$test_root"
cargo build --bin btrfs-awacs-e2e

if [ "$(id -u)" -eq 0 ]; then
  exec target/debug/btrfs-awacs-e2e --root "$test_root" "$@"
else
  exec sudo -- target/debug/btrfs-awacs-e2e --root "$test_root" "$@"
fi
