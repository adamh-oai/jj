#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_dir"

prefix=${PREFIX:-/usr/local}
destdir=${DESTDIR:-}
binary=${1:-target/release/awacs}

install -d -m 0755 "$destdir$prefix/bin"
install -m 0755 "$binary" "$destdir$prefix/bin/awacs"
ln -sf awacs "$destdir$prefix/bin/git-fsmonitor-awacs"

rm -f "$destdir$prefix/libexec/btrfs-awacs/btrfs-awacs" "$destdir$prefix/libexec/btrfs-awacs/git-fsmonitor-hook" "$destdir$prefix/libexec/btrfs-awacs/watchman"
rmdir "$destdir$prefix/libexec/btrfs-awacs" 2>/dev/null || true

install -d -m 0755 "$destdir$prefix/lib/systemd/system"
install -m 0644 packaging/btrfs-awacs-broker.service \
  "$destdir$prefix/lib/systemd/system/btrfs-awacs-broker.service"

install -d -m 0755 "$destdir/etc/btrfs-awacs"
if [ ! -e "$destdir/etc/btrfs-awacs/broker.env" ]; then
  install -m 0644 packaging/broker.env.example "$destdir/etc/btrfs-awacs/broker.env"
fi

# Ensure the installed program resolves inside the destination tree.
test -x "$destdir$prefix/bin/awacs"
