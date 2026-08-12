#!/usr/bin/env bash
set -euo pipefail

source_subvolume=${1:-/home/dev-user/code/openai}
image=${2:-/tmp/btrfs-awacs-uml/openai.raw}
image_size=${IMAGE_SIZE:-512G}
snapshot_dir=$source_subvolume/.btrfs-awacs
partial=$image.partial
mount_dir=$image.mnt

mapfile -t snapshots < <(
  find "$snapshot_dir" -mindepth 1 -maxdepth 1 -type d \
    -name 'snapshot-*' -print | sort
)

if (( ${#snapshots[@]} < 2 )); then
  echo "need two snapshots below $snapshot_dir" >&2
  exit 1
fi

parent=${snapshots[-2]}
current=${snapshots[-1]}

if [[ -e $image || -e $partial ]]; then
  echo "refusing to overwrite $image or $partial" >&2
  exit 1
fi

mkdir -p "$(dirname "$image")" "$mount_dir"
truncate -s "$image_size" "$partial"

# Match the source filesystem's 16 KiB node size. A sparse image can have a
# large logical size without consuming that much host space.
mkfs.btrfs -f -n 16k -m single -d single \
  -L btrfs-awacs-uml "$partial"

cleanup() {
  if mountpoint -q "$mount_dir"; then
    sudo --non-interactive umount "$mount_dir"
  fi
}
trap cleanup EXIT

sudo --non-interactive mount -o loop,noatime "$partial" "$mount_dir"

echo "full send: $parent"
sudo --non-interactive btrfs send --proto 2 --compressed-data "$parent" |
  sudo --non-interactive btrfs receive "$mount_dir"

echo "incremental send: $current"
sudo --non-interactive btrfs send --proto 2 --compressed-data \
  -p "$parent" "$current" |
  sudo --non-interactive btrfs receive "$mount_dir"

sudo --non-interactive btrfs subvolume list -u -q -R "$mount_dir"
sudo --non-interactive btrfs filesystem usage "$mount_dir"
sync
sudo --non-interactive umount "$mount_dir"
trap - EXIT

mv "$partial" "$image"
echo "fixture: $image"
