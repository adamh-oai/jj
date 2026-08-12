#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
: "${CARGO_TARGET_DIR:=$repo_dir/target}"
export CARGO_TARGET_DIR
binary_dir=/usr/local/bin
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
run_as_root install -m 0755 "$CARGO_TARGET_DIR/release/awacs" "$binary_dir/awacs"
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

# scan-serve is a per-user daemon, not a systemd unit. Its long-lived broker
# connection points at the process we just replaced, so leaving it alive makes
# the next scan fail with EPIPE before discovery has a chance to start a fresh
# daemon. Stop every scan daemon owned by the installing user; the next AWACS
# client request recreates the namespace-specific daemon on demand.
if command -v pkill >/dev/null 2>&1; then
  if pkill -TERM -u "$(id -u)" -f '[a]wacs scan-serve'; then
    echo "Stopped stale per-user AWACS scan daemon(s)."
  fi
fi
