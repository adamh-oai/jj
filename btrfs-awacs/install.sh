#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
: "${CARGO_TARGET_DIR:=$repo_dir/target}"
export CARGO_TARGET_DIR
binary_dir=/usr/local/bin
cargo_bin_dir=${CARGO_HOME:-$HOME/.cargo}/bin
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
if [ "${1-}" = dev ]; then
  cargo build -p jj-cli -p btrfs-awacs
  build_dir=debug
else
  cargo build --release -p jj-cli -p btrfs-awacs
  build_dir=release
fi

install -d -m 0755 "$cargo_bin_dir"
install -m 0755 "$CARGO_TARGET_DIR/$build_dir/jj" "$cargo_bin_dir/jj"
run_as_root install -d -m 0755 "$binary_dir"
run_as_root install -m 0755 "$CARGO_TARGET_DIR/$build_dir/awacs" "$binary_dir/awacs"
run_as_root ln -sf awacs "$binary_dir/git-fsmonitor-awacs"
run_as_root rm -f /usr/local/libexec/btrfs-awacs/btrfs-awacs /usr/local/libexec/btrfs-awacs/git-fsmonitor-hook /usr/local/libexec/btrfs-awacs/watchman
run_as_root rmdir /usr/local/libexec/btrfs-awacs 2>/dev/null || true

run_as_root install -m 0644 packaging/btrfs-awacs-broker.service "$unit_dir/$unit_name"

run_as_root install -d -m 0755 "$config_dir"
if ! run_as_root test -e "$config_dir/broker.env"; then
  run_as_root install -m 0644 packaging/broker.env.example \
    "$config_dir/broker.env"
fi

run_as_root systemctl daemon-reload
run_as_root systemctl enable "$unit_name"
run_as_root systemctl restart "$unit_name"
