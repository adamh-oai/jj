#!/bin/sh
set -eu

prefix=${PREFIX:-/usr}
destdir=${DESTDIR:-}
binary=${1:-target/release/btrfs-awacs}

install -d -m 0755 "$destdir$prefix/libexec/btrfs-awacs"
install -m 0755 "$binary" "$destdir$prefix/libexec/btrfs-awacs/btrfs-awacs"
ln -sfn btrfs-awacs "$destdir$prefix/libexec/btrfs-awacs/git-fsmonitor-hook"
ln -sfn btrfs-awacs "$destdir$prefix/libexec/btrfs-awacs/btrfs-awacs-watchman"
ln -sfn btrfs-awacs "$destdir$prefix/libexec/btrfs-awacs/watchman"

install -d -m 0755 "$destdir$prefix/lib/systemd/system"
install -m 0644 packaging/btrfs-awacs-broker.service \
  "$destdir$prefix/lib/systemd/system/btrfs-awacs-broker.service"

install -d -m 0755 "$destdir/etc/btrfs-awacs"
if [ ! -e "$destdir/etc/btrfs-awacs/broker.env" ]; then
  install -m 0644 packaging/broker.env.example "$destdir/etc/btrfs-awacs/broker.env"
fi

# Ensure every installed multicall name resolves inside the destination tree.
for entry in git-fsmonitor-hook btrfs-awacs-watchman watchman; do
  test -x "$destdir$prefix/libexec/btrfs-awacs/$entry"
done
