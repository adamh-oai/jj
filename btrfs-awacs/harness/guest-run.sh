#!/bin/busybox sh
set -eu
set -o pipefail

parent=$1
current=$2
results=/host/results
runs=${RUNS:-10}
warmup=${WARMUP:-1}
send_mode=${SEND_MODE:-default}

mkdir -p "$results"

echo "parent=$parent" | tee "$results/fixture.txt"
echo "current=$current" | tee -a "$results/fixture.txt"
uname -a | tee "$results/uname.txt"

case "$send_mode" in
  default)
    ;;
  profile-default | profile-default-v2 | no-clone | discard-commands | changed-objects)
    parent_id=$(btrfs inspect-internal rootid "$parent")
    ;;
  detector-timing)
    mkdir -p /work/.btrfs-awacs
    mount --bind /source /work/.btrfs-awacs
    if ! /usr/bin/btrfs-awacs compare /work \
      > "$results/detector-timing.txt" 2>&1; then
      cat "$results/detector-timing.txt"
      exit 1
    fi
    cat "$results/detector-timing.txt"
    exit 0
    ;;
  btrfs-ioctl-smoke)
    mkdir -p /run/btrfs-awacs /var/lib/btrfs-awacs/broker
    chmod 0750 /run/btrfs-awacs
    chmod 0700 /var/lib/btrfs-awacs/broker
    /usr/bin/btrfs-awacs broker-serve \
      /run/btrfs-awacs/broker.sock \
      /var/lib/btrfs-awacs/broker/receipts.sqlite3 0 0 &
    broker_pid=$!
    mkdir -p /var/lib/btrfs-awacs/user-broker /run/user/1000 /tmp/client-spool
    chmod 0700 /var/lib/btrfs-awacs/user-broker /run/user/1000 /tmp/client-spool
    chown 1000:1000 /run/user/1000 /tmp/client-spool
    chgrp 1000 /run/btrfs-awacs
    /usr/bin/btrfs-awacs broker-serve \
      /run/btrfs-awacs/user-broker.sock \
      /var/lib/btrfs-awacs/user-broker/receipts.sqlite3 1000 1000 &
    user_broker_pid=$!
    for _attempt in $(seq 1 100); do
      [ -S /run/btrfs-awacs/broker.sock ] && break
      sleep 0.01
    done
    if [ ! -S /run/btrfs-awacs/broker.sock ]; then
      echo "external broker socket did not appear" >&2
      kill "$broker_pid" 2>/dev/null || true
      exit 1
    fi
    for _attempt in $(seq 1 100); do
      [ -S /run/btrfs-awacs/user-broker.sock ] && break
      sleep 0.01
    done
    if [ ! -S /run/btrfs-awacs/user-broker.sock ]; then
      echo "user broker socket did not appear" >&2
      kill "$broker_pid" "$user_broker_pid" 2>/dev/null || true
      exit 1
    fi
    /usr/bin/btrfs-awacs __btrfs-inspect "$parent" |
      tee "$results/btrfs-inspect-parent.txt"
    /usr/bin/btrfs-awacs __btrfs-inspect "$current" |
      tee "$results/btrfs-inspect-current.txt"
    /usr/bin/btrfs-awacs __broker-full-index "$current" |
      tee "$results/broker-full-index-summary.txt"
    BTRFS_AWACS_BROKER_SOCKET=/run/btrfs-awacs/broker.sock \
      /usr/bin/btrfs-awacs __service-recovery-smoke \
      /source/recovery-live /source/recovery-managed \
      /tmp/recovery-spool /tmp/recovery-manager.sqlite3 |
      tee "$results/service-recovery-smoke-summary.txt"
    BTRFS_AWACS_BROKER_SOCKET=/run/btrfs-awacs/broker.sock \
      /usr/bin/btrfs-awacs __nested-boundary-smoke \
      /source/nested-live /source/nested-managed \
      /tmp/nested-spool /tmp/nested-manager.sqlite3 |
      tee "$results/nested-boundary-smoke-summary.txt"
    BTRFS_AWACS_BROKER_SOCKET=/run/btrfs-awacs/broker.sock \
      /usr/bin/btrfs-awacs __service-smoke \
      /source/live /source/managed /tmp/service-spool \
      /tmp/service-manager.sqlite3 /tmp/service-broker.sqlite3 |
      tee "$results/service-smoke-summary.txt"

    as_client()
    {
      /usr/bin/setpriv --reuid=1000 --regid=1000 --clear-groups \
        env HOME=/home/test-user XDG_RUNTIME_DIR=/run/user/1000 \
        PATH=/usr/bin:/bin \
        BTRFS_AWACS_ROOT=/source/client-live \
        BTRFS_AWACS_MANAGED_DIR=/source/client-managed \
        BTRFS_AWACS_SPOOL_DIR=/tmp/client-spool \
        BTRFS_AWACS_MANAGER_DB=/tmp/client-manager.sqlite3 \
        BTRFS_AWACS_BROKER_SOCKET=/run/btrfs-awacs/user-broker.sock \
        BTRFS_AWACS_EXPERIMENTAL_DIRTY_WITNESS=1 \
        BTRFS_AWACS_PRECISION_GUARD=1 \
        BTRFS_AWACS_JJ=/usr/bin/jj-trigger-wrapper \
        BTRFS_AWACS_TRIGGER_INTERVAL_MS=5000 \
        "$@"
    }

    as_client jj --version | tee "$results/jj-version.txt"
    as_client git --version | tee "$results/git-version.txt"
    as_client jj git init /source/client-live
    as_client jj config set --user user.name 'UML Test'
    as_client jj config set --user user.email uml@example.invalid
    as_client jj config set --user fsmonitor.backend watchman
    as_client jj config set --user fsmonitor.watchman.register-snapshot-trigger true
    as_client sh -c 'printf "initial\n" > /source/client-live/tracked'
    as_client git -C /source/client-live config user.name 'UML Test'
    as_client git -C /source/client-live config user.email uml@example.invalid
    as_client git -C /source/client-live add tracked
    as_client git -C /source/client-live commit -m initial
    as_client git -C /source/client-live config core.fsmonitor /usr/bin/git-fsmonitor-hook
    as_client git -C /source/client-live config core.fsmonitorHookVersion 2
    as_client git -C /source/client-live status --porcelain \
      > "$results/git-fsmonitor-initial.txt"
    rm -f /tmp/btrfs-awacs-jj-trigger-ran
    as_client jj -R /source/client-live status \
      > "$results/jj-watchman-status.txt"
    for _attempt in $(seq 1 140); do
      [ -s /tmp/btrfs-awacs-jj-trigger-ran ] && break
      sleep 0.05
    done
    if [ ! -s /tmp/btrfs-awacs-jj-trigger-ran ]; then
      echo "jj trigger did not run after registration" >&2
      exit 1
    fi
    rm -f /tmp/btrfs-awacs-jj-trigger-ran
    # The runner logs only after jj exits. Give the scheduler time to return to
    # its poll, then require the precision event to beat the five-second
    # periodic correctness interval by a wide margin.
    sleep 0.2
    as_client sh -c 'printf "modified\n" > /source/client-live/tracked'
    for _attempt in $(seq 1 40); do
      [ -s /tmp/btrfs-awacs-jj-trigger-ran ] && break
      sleep 0.05
    done
    if [ ! -s /tmp/btrfs-awacs-jj-trigger-ran ]; then
      echo "jj trigger did not finish after a precision-guard early wake" >&2
      exit 1
    fi
    as_client git -C /source/client-live status --porcelain \
      > "$results/git-fsmonitor-modified.txt"
    if ! grep -q '^ M tracked$' "$results/git-fsmonitor-modified.txt"; then
      echo "real Git fsmonitor did not report the tracked modification" >&2
      cat "$results/git-fsmonitor-modified.txt" >&2
      exit 1
    fi
    printf 'jj_watchman=true git_fsmonitor=true trigger_precision_wakeup=true\n' |
      tee "$results/real-client-summary.txt"
    if as_client /usr/bin/btrfs-awacs __broker-changed-objects \
      "$parent" "$current" /tmp/client-spool/unprivileged.objects \
      > "$results/unprivileged-changed-objects.txt" 2>&1; then
      echo "unprivileged changed-objects ioctl unexpectedly succeeded" >&2
      exit 1
    fi
    if ! grep -q 'Operation not permitted' \
      "$results/unprivileged-changed-objects.txt"; then
      echo "unprivileged changed-objects failure was not CAP_SYS_ADMIN denial" >&2
      cat "$results/unprivileged-changed-objects.txt" >&2
      exit 1
    fi
    printf 'readable_snapshot_unprivileged_ioctl=denied\n' |
      tee "$results/unprivileged-changed-objects-summary.txt"
    if BTRFS_AWACS_TEST_MAX_OUTPUT_BYTES=160 \
      /usr/bin/btrfs-awacs __broker-changed-objects \
      "$parent" "$current" /tmp/limited.objects \
      > "$results/limited-changed-objects.txt" 2>&1; then
      echo "changed-objects byte limit unexpectedly produced a successful stream" >&2
      exit 1
    fi
    if ! grep -Eq 'File too large|output limit' \
      "$results/limited-changed-objects.txt"; then
      echo "changed-objects byte-limit failure was not explicit" >&2
      cat "$results/limited-changed-objects.txt" >&2
      exit 1
    fi
    rm -f /tmp/limited.objects
    printf 'changed_objects_output_limit=denied\n' |
      tee "$results/limited-changed-objects-summary.txt"
    /usr/bin/btrfs-awacs __broker-changed-objects \
      "$parent" "$current" /tmp/broker-changed.objects |
      tee "$results/broker-changed-objects-summary.txt"
    cp /tmp/broker-changed.objects "$results/broker-changed.objects"
    /usr/bin/btrfs-awacs __broker-create-snapshot \
      /source/live /source/managed cut-ro ro /tmp/broker-receipts.sqlite3 |
      tee "$results/broker-create-snapshot-summary.txt"
    /usr/bin/btrfs-awacs __btrfs-inspect /source/managed/cut-ro |
      tee "$results/btrfs-inspect-created.txt"
    /usr/bin/btrfs-awacs __broker-delete-snapshot \
      /source/managed/cut-ro /source/managed cut-ro \
      /tmp/broker-delete-receipts.sqlite3 |
      tee "$results/broker-delete-snapshot-summary.txt"
    if [ -e /source/managed/cut-ro ]; then
      echo "broker-deleted snapshot is still visible" >&2
      exit 1
    fi
    /usr/bin/btrfs-awacs __broker-create-snapshot \
      "$current" /source/managed stage-rw rw \
      /tmp/broker-stage-receipts.sqlite3 |
      tee "$results/broker-create-worktree-summary.txt"
    /usr/bin/btrfs-awacs __btrfs-inspect /source/managed/stage-rw |
      tee "$results/btrfs-inspect-staged-worktree.txt"
    printf 0123456789abcdef0123456789abcdef \
      > /source/worktrees/reservation
    chmod 0600 /source/worktrees/reservation
    /usr/bin/btrfs-awacs __broker-publish-worktree \
      /source/managed/stage-rw /source/managed stage-rw \
      /source/worktrees worktree reservation \
      /tmp/broker-worktree-receipts.sqlite3 |
      tee "$results/broker-publish-worktree-summary.txt"
    if [ -e /source/managed/stage-rw ] || \
       [ -e /source/worktrees/reservation ]; then
      echo "worktree staging or reservation entry remains" >&2
      exit 1
    fi
    /usr/bin/btrfs-awacs __btrfs-inspect /source/worktrees/worktree |
      tee "$results/btrfs-inspect-published-worktree.txt"
    echo writable > /source/worktrees/worktree/worktree-created
    exit 0
    ;;
  *)
    echo "unknown send mode: $send_mode" >&2
    exit 2
    ;;
esac

run_send()
{
  output=$1

  case "$send_mode" in
    default)
      btrfs send --no-data -p "$parent" "$current" -f "$output"
      ;;
    profile-default | profile-default-v2 | no-clone | discard-commands | changed-objects)
      send-ioctl "$send_mode" "$current" "$parent_id" "$output"
      ;;
  esac
}

# Warm the B-tree cache before collecting repeat measurements.
if [ "$warmup" -ne 0 ]; then
  run_send /dev/null
fi

: > "$results/timings.txt"
i=1
while [ "$i" -le "$runs" ]; do
  echo "run $i/$runs" | tee -a "$results/timings.txt"
  if [ "$send_mode" = default ]; then
    (time btrfs send --no-data -p "$parent" "$current" -f /dev/null) \
      2>> "$results/timings.txt"
  else
    (time send-ioctl "$send_mode" "$current" "$parent_id" /dev/null) \
      2>> "$results/timings.txt"
  fi
  i=$((i + 1))
done

run_send "$results/no-data.send"
sha256sum "$results/no-data.send" > "$results/no-data.send.sha256"
if [ "$send_mode" = discard-commands ]; then
  set -- $(wc -c < "$results/no-data.send")
  if [ "$1" -ne 17 ]; then
    echo "discard stream is $1 bytes, expected 17" >&2
    exit 1
  fi
  expected_hash=954930b8e473af15a6821efeba3bb150a2f3b8be57abcb219311ead9bd20f773
  actual_hash=$(cut -d ' ' -f 1 "$results/no-data.send.sha256")
  if [ "$actual_hash" != "$expected_hash" ]; then
    echo "unexpected discard stream hash: $actual_hash" >&2
    exit 1
  fi
  : > "$results/no-data.dump"
elif [ "$send_mode" = changed-objects ]; then
  magic=$(od -An -tx1 -N16 "$results/no-data.send" | tr -d ' \n')
  if [ "$magic" != 62747266732d6368616e676573000000 ]; then
    echo "unexpected changed-object manifest magic: $magic" >&2
    exit 1
  fi
  : > "$results/no-data.dump"
else
  btrfs receive --dump < "$results/no-data.send" \
    > "$results/no-data.dump"
fi
wc -c -l "$results/no-data.send" "$results/no-data.dump" \
  > "$results/sizes.txt"
