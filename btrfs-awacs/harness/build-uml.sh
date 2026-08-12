#!/usr/bin/env bash
set -euo pipefail

linux_dir=${LINUX_DIR:-/home/dev-user/code/linux}
out=${OUT:-/tmp/btrfs-awacs-uml/kernel}
jobs=${JOBS:-$(nproc)}

make -C "$linux_dir" O="$out" ARCH=um x86_64_defconfig

"$linux_dir/scripts/config" --file "$out/.config" \
  --enable BTRFS_FS \
  --enable BTRFS_FS_POSIX_ACL \
  --enable BLK_DEV_UBD \
  --enable BLK_DEV_INITRD \
  --enable RD_GZIP \
  --enable HOSTFS \
  --enable DEVTMPFS \
  --enable DEVTMPFS_MOUNT \
  --enable PROC_FS \
  --enable SYSFS \
  --enable TMPFS \
  --enable SMP \
  --set-val NR_CPUS 2 \
  --enable FRAME_POINTER \
  --enable DEBUG_INFO \
  --enable DEBUG_INFO_DWARF4 \
  --disable DEBUG_INFO_DWARF5 \
  --disable DEBUG_INFO_BTF \
  --enable CC_OPTIMIZE_FOR_PERFORMANCE \
  --disable CC_OPTIMIZE_FOR_SIZE \
  --disable BTRFS_DEBUG \
  --disable BTRFS_ASSERT \
  --disable BTRFS_FS_RUN_SANITY_TESTS \
  --disable GPROF \
  --disable GCOV

make -C "$linux_dir" O="$out" ARCH=um olddefconfig
make -C "$linux_dir" O="$out" ARCH=um -j"$jobs"

echo "UML kernel: $out/linux"
