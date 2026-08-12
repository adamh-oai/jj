#!/usr/bin/env bash
set -euo pipefail

linux_dir=${LINUX_DIR:-/home/dev-user/code/linux}
output_dir=${OUTPUT_DIR:-/tmp/btrfs-fast-snap-debs}
abi=${ABI:-2801}
build_name=${BUILD_NAME:-btrfs-fast-snap}
build_revision=${BUILD_REVISION:-2}
jobs=${JOBS:-$(nproc)}
skip_clean=${SKIP_CLEAN:-0}
debian_dir=$linux_dir/debian.hwe-7.0
changelog=$debian_dir/changelog
common_vars=$linux_dir/debian/rules.d/0-common-vars.mk

die()
{
	echo "error: $*" >&2
	exit 1
}

for command in \
	bindgen-0.65 dpkg-deb dpkg-parsechangelog fakeroot gcc-13 make \
	pahole python3 rustc-1.91 sha256sum
do
	command -v "$command" >/dev/null ||
		die "missing $command; install the Ubuntu HWE kernel build dependencies"
done

[[ -d $linux_dir/.git ]] || die "$linux_dir is not a Git checkout"
[[ -f $linux_dir/debian/rules ]] || die "$linux_dir is not an Ubuntu kernel source tree"
[[ -f $changelog ]] || die "$debian_dir is missing"
[[ -f $common_vars ]] || die "$common_vars is missing"
[[ $abi =~ ^[1-9][0-9]*$ ]] || die "ABI must be a positive integer"
[[ $build_name =~ ^[a-z0-9][a-z0-9.-]*$ ]] ||
	die "BUILD_NAME must contain only lowercase letters, digits, dots, and hyphens"
[[ $build_revision =~ ^[1-9][0-9]*$ ]] ||
	die "BUILD_REVISION must be a positive integer"
[[ $jobs =~ ^[1-9][0-9]*$ ]] || die "JOBS must be a positive integer"
[[ $skip_clean == 0 || $skip_clean == 1 ]] ||
	die "SKIP_CLEAN must be 0 or 1"

if ! git -C "$linux_dir" diff --quiet ||
	! git -C "$linux_dir" diff --cached --quiet
then
	die "kernel checkout has tracked changes; commit or restore them before packaging"
fi

source_name=$(dpkg-parsechangelog -l"$changelog" -S source)
base_version=$(dpkg-parsechangelog -l"$changelog" -S version)
distribution=$(dpkg-parsechangelog -l"$changelog" -S distribution)
upstream_version=${base_version%%-*}
revision=${base_version#*-}
upload_and_series=${revision#*.}

if [[ $upload_and_series == *"~"* ]]; then
	upload=${upload_and_series%%~*}
	series_suffix=${upload_and_series#*~}
	default_version="${upstream_version}-${abi}.${upload}+build${build_revision}~${series_suffix}"
else
	default_version="${upstream_version}-${abi}.${upload_and_series}+build${build_revision}"
fi

package_version=${PACKAGE_VERSION:-$default_version}
dpkg --validate-version "$package_version" ||
	die "invalid PACKAGE_VERSION: $package_version"

# Ubuntu's kernel rules treat everything after the final hyphen as the Debian
# revision, then take its first dot-separated field as the ABI. Keep the
# package version parser-friendly, and override the parser inputs below so the
# installed release sorts as 7.0.0-2801-btrfs-fast-snap-generic.
package_revision=${package_version##*-}
package_abi=${package_revision%%.*}
[[ $package_abi == "$abi" ]] ||
	die "PACKAGE_VERSION must end in a revision beginning with ABI $abi: $package_version"
kernel_abi_release="${upstream_version}-${abi}-${build_name}"
kernel_release="${kernel_abi_release}-generic"

base_abi=${revision%%.*}
[[ $abi != "$base_abi" ]] ||
	die "ABI $abi would collide with the stock package; choose a different ABI"

mkdir -p "$output_dir"
output_dir=$(realpath "$output_dir")
backup=$(mktemp)
cp -p "$changelog" "$backup"
common_vars_backup=$(mktemp)
cp -p "$common_vars" "$common_vars_backup"

restore_changelog()
{
	cp -p "$backup" "$changelog"
	rm -f "$backup"
	cp -p "$common_vars_backup" "$common_vars"
	rm -f "$common_vars_backup"
}
trap restore_changelog EXIT

sed -i \
	"1c\\${source_name} (${package_version}) ${distribution}; urgency=medium" \
	"$changelog"
# The package ABI intentionally contains hyphens for GRUB sorting, but PKG_ABI
# is consumed as a C token. Keep that compile-time compatibility macro numeric.
sed -i \
	's/CFLAGS_MODULE="-DPKG_ABI=\$(abinum)"/CFLAGS_MODULE="-DPKG_ABI=\$(firstword \$(subst -,\$(space),\$(abinum)))"/' \
	"$common_vars"

printf 'Kernel tree: %s\n' "$linux_dir"
printf 'Kernel commit: %s\n' "$(git -C "$linux_dir" rev-parse HEAD)"
printf 'Build name: %s\n' "$build_name"
printf 'Package version: %s\n' "$package_version"
printf 'Kernel release: %s\n' "$kernel_release"
printf 'Output directory: %s\n' "$output_dir"

build_log=$output_dir/build.log
(
	cd "$linux_dir"
	# Codex/OpenAI development shells set PYTHONSAFEPATH, which prevents
	# Ubuntu's debian/scripts/misc/annotations wrapper from importing its
	# adjacent kconfig package. An inherited RUST_LOG also makes bindgen emit
	# gigabytes of debug output while Ubuntu probes and builds Rust support.
	unset PYTHONSAFEPATH
	unset RUST_LOG
	if [[ $skip_clean == 0 ]]; then
		fakeroot debian/rules \
			DEB_VERSION_UPSTREAM="$upstream_version" \
			DEB_REVISION="${abi}-${build_name}" \
			clean
	fi
	CONCURRENCY_LEVEL="$jobs" fakeroot debian/rules \
		do_tools=false \
		do_zfs=false \
		do_evdi=false \
		do_ipu6=false \
		do_ipu7=false \
		do_iwlwifi=false \
		do_v4l2loopback=false \
		do_usbio=false \
		do_vision=false \
		DEB_VERSION_UPSTREAM="$upstream_version" \
		DEB_REVISION="${abi}-${build_name}" \
		binary-headers binary-generic
) 2>&1 | tee "$build_log"

filename_version=${package_version//:/%3a}
mapfile -d '' packages < <(
	find "$linux_dir/.." -maxdepth 1 -type f \
		\( -name "*_${filename_version}_*.deb" -o \
		   -name "*_${filename_version}_*.ddeb" \) \
		-print0
)
(( ${#packages[@]} > 0 )) ||
	die "build completed but no packages for $package_version were found"

for package in "${packages[@]}"; do
	install -m 0644 "$package" "$output_dir/"
done

# Build only the core perf binary. Ubuntu's binary-perarch target also enables
# unrelated tools and optional Python/JVMTI integrations whose build
# dependencies are not needed for profiling this kernel.
make -C "$linux_dir/tools/perf" -j"$jobs" \
	NO_LIBPYTHON=1 NO_LIBPERL=1 WERROR=0
perf_package_dir=$(mktemp -d)
mkdir -p "$perf_package_dir/DEBIAN" \
	"$perf_package_dir/usr/lib/linux-tools/$kernel_release"
install -m 0755 "$linux_dir/tools/perf/perf" \
	"$perf_package_dir/usr/lib/linux-tools/$kernel_release/perf"
cat >"$perf_package_dir/DEBIAN/control" <<EOF
Package: linux-perf-tools-$kernel_abi_release
Version: $package_version
Architecture: $(dpkg --print-architecture)
Maintainer: Local kernel build <root@localhost>
Description: perf tools for $kernel_release
 Locally built perf binary from the matching kernel tree.
EOF
dpkg-deb --build "$perf_package_dir" \
	"$output_dir/linux-perf-tools-${kernel_abi_release}_${filename_version}_$(dpkg --print-architecture).deb"
rm -rf "$perf_package_dir"

# Some development kernel trees vendor btrfs-progs alongside the kernel. If
# one does, build it and package the locally built mkfs.btrfs so the matching
# filesystem formatter travels with the kernel artifacts.
btrfs_mkfs=$(find "$linux_dir" \
	-path "$linux_dir/debian/build" -prune -o \
	-type f -name mkfs.btrfs -print -quit)
btrfs_tools_dir=
if [[ -n $btrfs_mkfs ]]; then
	btrfs_tools_dir=$(dirname "$btrfs_mkfs")
else
	btrfs_tools_dir=$(find "$linux_dir" \
		-path "$linux_dir/debian/build" -prune -o \
		-type d \( -name btrfs-progs -o -name btrfs-tools \) -print -quit)
	btrfs_mkfs="$btrfs_tools_dir/mkfs.btrfs"
fi
if [[ -n $btrfs_tools_dir ]]; then
	if [[ ! -x $btrfs_mkfs ]]; then
		make -C "$btrfs_tools_dir" -j"$jobs"
	fi
	[[ -x $btrfs_mkfs ]] ||
		die "found in-tree mkfs.btrfs but could not build it: $btrfs_mkfs"

	btrfs_package_dir=$(mktemp -d)
	mkdir -p "$btrfs_package_dir/DEBIAN" "$btrfs_package_dir/usr/sbin"
	install -m 0755 "$btrfs_mkfs" "$btrfs_package_dir/usr/sbin/mkfs.btrfs"
	cat >"$btrfs_package_dir/DEBIAN/control" <<EOF
Package: btrfs-tools
Version: $package_version
Architecture: $(dpkg --print-architecture)
Maintainer: Local kernel build <root@localhost>
Description: In-tree btrfs tools for $kernel_release
 Locally built mkfs.btrfs from the matching kernel tree.
EOF
	dpkg-deb --build "$btrfs_package_dir" \
		"$output_dir/btrfs-tools_${filename_version}_$(dpkg --print-architecture).deb"
	rm -rf "$btrfs_package_dir"
fi

(
	cd "$output_dir"
	sha256sum ./*.deb ./*.ddeb 2>/dev/null > SHA256SUMS ||
		sha256sum ./*.deb > SHA256SUMS
)

printf '\nBuilt packages:\n'
for package in "$output_dir"/*.deb "$output_dir"/*.ddeb; do
	[[ -e $package ]] || continue
	printf '  %s\t%s\t%s\n' \
		"$(basename "$package")" \
		"$(dpkg-deb -f "$package" Package)" \
		"$(dpkg-deb -f "$package" Version)"
done
printf '\nChecksums: %s/SHA256SUMS\n' "$output_dir"
printf 'Build log: %s\n' "$build_log"
