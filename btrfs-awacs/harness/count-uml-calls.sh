#!/usr/bin/env bash
set -euo pipefail

dry_run=false
case ${1:-} in
  --dry-run)
    dry_run=true
    shift
    ;;
  '')
    ;;
  *)
    echo "usage: $0 [--dry-run]" >&2
    exit 2
    ;;
esac
if (($#)); then
  echo "usage: $0 [--dry-run]" >&2
  exit 2
fi

base=${BASE:-/tmp/btrfs-awacs-uml}
kernel=${KERNEL:-$base/kernel/linux}
initramfs=${INITRAMFS:-$base/initramfs.cpio.gz}
image=${IMAGE:-$base/openai.raw}
cpus=${CPUS:-2}
send_mode=${SEND_MODE:-default}
results=$base/results

case "$send_mode" in
  default | profile-default | profile-default-v2 | no-clone | discard-commands | changed-objects)
    ;;
  *)
    echo "unsupported SEND_MODE: $send_mode" >&2
    exit 2
    ;;
esac

logical_symbols=(
  btrfs_search_slot
  btrfs_clone_extent_buffer
  get_inode_info
  get_first_ref
  process_recorded_refs
  get_cur_path
  find_extent_clone
  send_cmd
  tlv_put
  crc32c
  kernel_write
  process_extent
)
optional_logical_symbols=(
  # Introduced by the inode-info cache experiment. On the baseline kernel,
  # get_inode_info itself is the raw B-tree lookup.
  read_inode_info
  # Useful for attributing the metadata-only output and timestamp experiments.
  flush_send_output
  send_utimes
  # Introduced by the scalar directory-item lookup experiment.
  lookup_dir_item_key
  # Introduced by the direct changed-object experiment.
  changed_object_cb
  emit_changed_refs
  flush_changed_object
  send_changed_objects
)

for command in bpftrace getent nm setpriv taskset sudo tee; do
  command -v "$command" >/dev/null || {
    echo "missing required command: $command" >&2
    exit 1
  }
done

for path in "$base" "$kernel" "$initramfs" "$image"; do
  if [[ ! $path =~ ^/[[:alnum:]_./+-]+$ ]]; then
    echo "path contains characters unsafe for bpftrace -c: $path" >&2
    exit 1
  fi
done
if [[ ! $cpus =~ ^[0-9]+([,-][0-9]+)*$ ]]; then
  echo "invalid taskset CPU list: $cpus" >&2
  exit 1
fi

[[ -x $kernel ]] || {
  echo "missing UML kernel: $kernel" >&2
  exit 1
}
if ! $dry_run; then
  [[ -r $initramfs ]] || {
    echo "missing initramfs: $initramfs" >&2
    exit 1
  }
  [[ -r $image ]] || {
    echo "missing fixture: $image" >&2
    exit 1
  }

  mkdir -p "$results" "$base/tmp"
  install -m 0755 "$(dirname "$0")/guest-run.sh" "$base/guest-run.sh"
fi

mapfile -t text_symbols < <(
  nm -an --defined-only "$kernel" |
    awk '$2 == "t" || $2 == "T" { print $3 }'
)

resolved_symbols=()
resolve_symbol()
{
  local requested=$1
  local candidate
  local exact=
  local -a compiler_clones=()

  for candidate in "${text_symbols[@]}"; do
    if [[ $candidate == "$requested" ]]; then
      exact=$candidate
    elif [[ $candidate =~ ^${requested}\.(constprop|isra|part)\.[0-9]+$ ]]; then
      compiler_clones+=("$candidate")
    fi
  done

  if [[ -n $exact ]]; then
    resolved_symbols=("$exact")
  elif ((${#compiler_clones[@]})); then
    resolved_symbols=("${compiler_clones[@]}")
  else
    echo "no executable symbol found for $requested in $kernel" >&2
    exit 1
  fi
}

symbol_exists()
{
  local requested=$1
  local candidate

  for candidate in "${text_symbols[@]}"; do
    if [[ $candidate == "$requested" ||
          $candidate =~ ^${requested}\.(constprop|isra|part)\.[0-9]+$ ]]; then
      return 0
    fi
  done
  return 1
}

resolve_symbol btrfs_ioctl_send
if ((${#resolved_symbols[@]} != 1)); then
  echo "expected one btrfs_ioctl_send entry point, found ${#resolved_symbols[@]}" >&2
  exit 1
fi
send_entry=${resolved_symbols[0]}

mapping=$results/uml-call-symbols.tsv
if ! $dry_run; then
  printf 'logical_symbol\telf_symbol\n' > "$mapping"
  printf 'btrfs_ioctl_send\t%s\n' "$send_entry" >> "$mapping"
fi

# Track the process launched by bpftrace and every descendant. UML can use
# multiple host tasks, so filtering only on cpid would lose some guest work.
program='BEGIN { @tracked[cpid] = 1; }'
program+=$'\n''tracepoint:sched:sched_process_fork'
program+=' /@tracked[pid] || @tracked[tid]/'
program+=' { @tracked[args->child_pid] = 1; }'

# Restrict the counters to the dynamic extent of BTRFS_IOC_SEND. This excludes
# filesystem mount and boot activity. This harness asks guest-run.sh to perform
# only the final captured send because uprobes on every hot call are intrusive.
program+=$'\n'"uprobe:${kernel}:${send_entry}"
program+=' /@tracked[pid] || @tracked[tid]/'
program+=' { @in_send[tid] = 1; @send_ioctls = count(); }'
program+=$'\n'"uretprobe:${kernel}:${send_entry}"
program+=' /@tracked[pid] || @tracked[tid]/'
program+=' { delete(@in_send[tid]); }'

for logical in "${logical_symbols[@]}"; do
  resolve_symbol "$logical"
  for symbol in "${resolved_symbols[@]}"; do
    if ! $dry_run; then
      printf '%s\t%s\n' "$logical" "$symbol" >> "$mapping"
    fi
    program+=$'\n'"uprobe:${kernel}:${symbol}"
    program+=' /@in_send[tid]/'
    program+=" { @calls[\"$logical\"] = count(); }"
  done
done
for logical in "${optional_logical_symbols[@]}"; do
  symbol_exists "$logical" || continue
  resolve_symbol "$logical"
  for symbol in "${resolved_symbols[@]}"; do
    if ! $dry_run; then
      printf '%s\t%s\n' "$logical" "$symbol" >> "$mapping"
    fi
    program+=$'\n'"uprobe:${kernel}:${symbol}"
    program+=' /@in_send[tid]/'
    program+=" { @calls[\"$logical\"] = count(); }"
  done
done

program+=$'\n''END {'
program+=' clear(@tracked); clear(@in_send);'
program+=' }'

run_uid=$(id -u)
run_gid=$(id -g)
run_home=$(getent passwd "$run_uid" | cut -d: -f6)
if [[ ! $run_home =~ ^/[[:alnum:]_./+-]+$ ]]; then
  echo "home directory contains characters unsafe for bpftrace -c: $run_home" >&2
  exit 1
fi
trace_command="/usr/bin/setpriv --reuid=$run_uid --regid=$run_gid"
trace_command+=" --init-groups -- /usr/bin/env HOME=$run_home"
trace_command+=" /usr/bin/taskset -c $cpus $kernel"
trace_command+=" mem=8G ncpus=1"
trace_command+=" initrd=$initramfs rdinit=/init"
trace_command+=" hostfs=$base ubd0rd=$image"
trace_command+=" send_runs=0 send_warmup=0 send_mode=$send_mode"
# Keep the guest console on stderr so stdout contains only bpftrace's maps.
trace_command+=' con=null con0=fd:0,fd:2 ssl=null panic=1'

if $dry_run; then
  sudo --non-interactive bpftrace -d \
    -c "$trace_command" -e "$program" >/dev/null
  echo "bpftrace program and UML command validated without starting UML"
  exit 0
fi

counts=$results/uml-call-counts.txt
echo "Resolved symbols are in $mapping"
echo "Exact send-scoped counts will be written to $counts"

sudo --non-interactive bpftrace -q -B line \
  -c "$trace_command" -e "$program" |
  tee "$counts"

if ! grep -qx '@send_ioctls: 1' "$counts"; then
  echo "expected exactly one observed send ioctl; refusing these counts" >&2
  exit 1
fi
