#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
binary_dir=/usr/local/libexec/btrfs-awacs
unit_name=btrfs-awacs-broker.service
unit_dir=/etc/systemd/system
config_dir=/etc/btrfs-awacs

if [ "$(id -u)" -eq 0 ]; then
  run_as_root() {
    "$@"
  }
else
  run_as_root() {
    sudo -- "$@"
  }
fi

cd "$repo_dir"
cargo build --release

run_as_root install -d -m 0755 "$binary_dir"
run_as_root install -m 0755 target/release/btrfs-awacs \
  "$binary_dir/btrfs-awacs"
for entry in watchman git-fsmonitor-hook; do
  run_as_root ln -sfn btrfs-awacs "$binary_dir/$entry"
done

unit_file=$(mktemp)
trap 'rm -f "$unit_file"' EXIT HUP INT TERM
sed 's|/usr/libexec/btrfs-awacs/btrfs-awacs|/usr/local/libexec/btrfs-awacs/btrfs-awacs|' \
  packaging/btrfs-awacs-broker.service >"$unit_file"
run_as_root install -m 0644 "$unit_file" "$unit_dir/$unit_name"

run_as_root install -d -m 0755 "$config_dir"
if ! run_as_root test -e "$config_dir/broker.env"; then
  run_as_root install -m 0644 packaging/broker.env.example \
    "$config_dir/broker.env"
fi

run_as_root systemctl daemon-reload
run_as_root systemctl enable --now "$unit_name"
