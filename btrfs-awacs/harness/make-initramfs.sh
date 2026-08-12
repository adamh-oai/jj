#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
base=${BASE:-/tmp/btrfs-awacs-uml}
root=$base/initramfs-root
archive=$base/initramfs.cpio.gz
jj_bin=${JJ_BIN:-$(command -v jj || true)}

[[ -x $jj_bin ]] || {
  echo "set JJ_BIN to an executable jj for the real-client UML test" >&2
  exit 1
}

cargo build --release --manifest-path "$repo_dir/Cargo.toml"

rm -rf "$root"
mkdir -p "$root"/{bin,dev,etc,host,proc,source,sys,usr/bin}

install -m 0755 /usr/bin/busybox "$root/bin/busybox"
install -m 0755 /usr/bin/btrfs "$root/usr/bin/btrfs"
install -m 0755 /usr/bin/git "$root/usr/bin/git"
install -m 0755 "$jj_bin" "$root/usr/bin/jj"
install -m 0755 /usr/bin/setpriv "$root/usr/bin/setpriv"
install -m 0755 "$repo_dir/target/release/btrfs-awacs" \
  "$root/usr/bin/btrfs-awacs"
ln -s btrfs-awacs "$root/usr/bin/watchman"
ln -s btrfs-awacs "$root/usr/bin/git-fsmonitor-hook"
install -m 0755 "$repo_dir/harness/sudo-root" "$root/usr/bin/sudo"
install -m 0755 "$repo_dir/harness/jj-trigger-wrapper" \
  "$root/usr/bin/jj-trigger-wrapper"
install -m 0755 "$repo_dir/harness/init" "$root/init"
mkdir -p "$root/usr/lib" "$root/usr/share/git-core"
cp -a /usr/lib/git-core "$root/usr/lib/"
cp -a /usr/share/git-core/templates "$root/usr/share/git-core/"
cc -O2 -Wall -Wextra -Werror \
  -o "$root/usr/bin/send-ioctl" \
  "$repo_dir/harness/patches/send-ioctl.c"

while IFS= read -r library; do
  [[ -n $library ]] || continue
  mkdir -p "$root$(dirname "$library")"
  cp -L "$library" "$root$library"
done < <(
  ldd /usr/bin/btrfs /usr/bin/git /usr/bin/setpriv /usr/lib/git-core/git "$jj_bin" \
    "$repo_dir/target/release/btrfs-awacs" |
    awk '/=> \// { print $3 } $1 ~ /^\// && $1 !~ /:$/ { print $1 }' |
    sort -u
)

(
  cd "$root"
  find . -print0 |
    cpio --null --create --format=newc --quiet |
    gzip -9
) > "$archive"

echo "initramfs: $archive"
