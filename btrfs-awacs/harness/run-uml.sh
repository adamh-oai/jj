#!/usr/bin/env bash
set -euo pipefail

base=${BASE:-/tmp/btrfs-awacs-uml}
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
kernel=${KERNEL:-$base/kernel/linux}
initramfs=${INITRAMFS:-$base/initramfs.cpio.gz}
image=${IMAGE:-$base/openai.raw}
cpus=${CPUS:-2}
send_mode=${SEND_MODE:-default}
runs=${RUNS:-10}
warmup=${WARMUP:-1}

case "$send_mode" in
  default | profile-default | profile-default-v2 | no-clone | discard-commands | changed-objects | detector-timing | btrfs-ioctl-smoke)
    ;;
  *)
    echo "unsupported SEND_MODE: $send_mode" >&2
    exit 2
    ;;
esac
if [[ ! $runs =~ ^[0-9]+$ || ! $warmup =~ ^[01]$ ]]; then
  echo "RUNS must be a non-negative integer and WARMUP must be 0 or 1" >&2
  exit 2
fi

mkdir -p "$base/results" "$base/tmp"
install -m 0755 "$(dirname "$0")/guest-run.sh" "$base/guest-run.sh"

device_argument="ubd0rd=$image"
scratch_image=
if [[ $send_mode == btrfs-ioctl-smoke && ! -e $image ]]; then
  scratch_image=$(mktemp --tmpdir="$base/tmp" ioctl-smoke.XXXXXX.raw)
  trap 'rm -f -- "$scratch_image"' EXIT
  truncate -s 256M "$scratch_image"
  mkfs.btrfs -f "$scratch_image" >/dev/null
  device_argument="ubd0=$scratch_image"
fi

taskset -c "$cpus" perf record \
  -o "$base/uml-send.perf.data" \
  -F 999 -e cpu-clock:u --call-graph fp -- \
  "$kernel" \
    mem=8G ncpus=1 \
    initrd="$initramfs" rdinit=/init \
    hostfs="$base" \
    "$device_argument" \
    send_mode="$send_mode" \
    send_runs="$runs" send_warmup="$warmup" \
    con=null con0=fd:0,fd:1 ssl=null \
    panic=1

if [[ $send_mode == changed-objects ]]; then
  "$script_dir/validate-changed-objects.py" \
    "$base/results/no-data.send" |
    tee "$base/results/changed-objects-summary.txt"
elif [[ $send_mode == btrfs-ioctl-smoke ]]; then
  "$script_dir/validate-changed-objects.py" \
    "$base/results/broker-changed.objects" |
    tee "$base/results/broker-changed-objects-validated.txt"
fi
