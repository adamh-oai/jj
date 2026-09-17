#!/bin/sh
set -eux

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
target_dir=${CARGO_TARGET_DIR:-$repo_dir/../target}
git_source_dir=${GIT_SOURCE_DIR:-$HOME/code/git}
install_prefix=/opt/awacs
binary_dir=$install_prefix/bin
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
if [ "$#" -gt 1 ]; then
  echo "usage: $0 [profile]" >&2
  exit 2
fi
if [ ! -f "$git_source_dir/Makefile" ]; then
  echo "Git checkout not found at $git_source_dir (set GIT_SOURCE_DIR to override)" >&2
  exit 1
fi
git_build_jobs=${GIT_BUILD_JOBS:-$(getconf _NPROCESSORS_ONLN)}
# Git embeds its installation paths, so build and install with the same prefix.
make -C "$git_source_dir" -j "$git_build_jobs" prefix="$install_prefix" all

if [ "$#" -eq 1 ]; then
  cargo build --profile "$1" -p jj-cli -p btrfs-awacs
  case "$1" in
    dev | test) build_dir=debug ;;
    release | bench) build_dir=release ;;
    *) build_dir=$1 ;;
  esac
else
  cargo build --release -p jj-cli -p btrfs-awacs
  build_dir=release
fi

# Finish both builds before installing binaries or restarting the broker.
run_as_root make -C "$git_source_dir" -j "$git_build_jobs" prefix="$install_prefix" install
run_as_root install -d -m 0755 "$binary_dir"
run_as_root install -m 0755 "$target_dir/$build_dir/jj" "$target_dir/$build_dir/awacs" "$binary_dir/"
run_as_root ln -sf awacs "$binary_dir/git-fsmonitor-awacs"

run_as_root install -m 0644 packaging/btrfs-awacs-broker.service "$unit_dir/$unit_name"

run_as_root install -d -m 0755 "$config_dir"
if ! run_as_root test -e "$config_dir/broker.env"; then
  run_as_root install -m 0644 packaging/broker.env.example \
    "$config_dir/broker.env"
fi

manager_uid=$(awk -F= '$1 == "BTRFS_AWACS_MANAGER_UID" { value = $2 } END { if (value == "") exit 1; print value }' "$config_dir/broker.env")
manager_gid=$(awk -F= '$1 == "BTRFS_AWACS_MANAGER_GID" { value = $2 } END { if (value == "") exit 1; print value }' "$config_dir/broker.env")
case "$manager_uid:$manager_gid" in
  *[!0-9:]* | :* | *:)
    echo "broker manager uid and gid must be numeric" >&2
    exit 1
    ;;
esac
run_as_root systemctl daemon-reload
run_as_root systemctl enable "$unit_name"
run_as_root systemctl restart "$unit_name"

printf '%s\n' 'Installed Git, JJ, and AWACS under /opt/awacs.' \
  'To use them: export PATH="/opt/awacs/bin:$PATH"'
