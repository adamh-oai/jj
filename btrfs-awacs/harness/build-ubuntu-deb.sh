#!/usr/bin/env bash
set -euo pipefail

linux_dir=${LINUX_DIR:-/home/dev-user/code/linux}
output_dir=${OUTPUT_DIR:-/tmp/btrfs-fast-snap-debs}
abi=${ABI:-2801}
build_name=${BUILD_NAME:-btrfs-fast-snap}
build_revision=${BUILD_REVISION:-1}
jobs=${JOBS:-$(nproc)}
debian_dir=$linux_dir/debian.hwe-7.0
changelog=$debian_dir/changelog

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
[[ $abi =~ ^[1-9][0-9]*$ ]] || die "ABI must be a positive integer"
[[ $build_name =~ ^[a-z0-9][a-z0-9.-]*$ ]] ||
	die "BUILD_NAME must contain only lowercase letters, digits, dots, and hyphens"
[[ $build_revision =~ ^[1-9][0-9]*$ ]] ||
	die "BUILD_REVISION must be a positive integer"
[[ $jobs =~ ^[1-9][0-9]*$ ]] || die "JOBS must be a positive integer"

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
named_upstream_version="${upstream_version}+${build_name}"

if [[ $upload_and_series == *"~"* ]]; then
	upload=${upload_and_series%%~*}
	series_suffix=${upload_and_series#*~}
	default_version="${named_upstream_version}-${abi}.${upload}+build${build_revision}~${series_suffix}"
else
	default_version="${named_upstream_version}-${abi}.${upload_and_series}+build${build_revision}"
fi

package_version=${PACKAGE_VERSION:-$default_version}
dpkg --validate-version "$package_version" ||
	die "invalid PACKAGE_VERSION: $package_version"

# Ubuntu's kernel rules treat everything after the final hyphen as the Debian
# revision, then take its first dot-separated field as the ABI. A build name
# in that revision therefore corrupts both PKGVER and ABINUM. Keep the name in
# the upstream portion and reject overrides that would produce the wrong ABI.
package_revision=${package_version##*-}
package_upstream=${package_version%-$package_revision}
package_abi=${package_revision%%.*}
[[ $package_abi == "$abi" ]] ||
	die "PACKAGE_VERSION must end in a revision beginning with ABI $abi: $package_version"
kernel_release="${package_upstream}-${abi}-generic"

base_abi=${revision%%.*}
[[ $abi != "$base_abi" ]] ||
	die "ABI $abi would collide with the stock package; choose a different ABI"

mkdir -p "$output_dir"
output_dir=$(realpath "$output_dir")
backup=$(mktemp)
cp -p "$changelog" "$backup"

restore_changelog()
{
	cp -p "$backup" "$changelog"
	rm -f "$backup"
}
trap restore_changelog EXIT

sed -i \
	"1c\\${source_name} (${package_version}) ${distribution}; urgency=medium" \
	"$changelog"

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
	# adjacent kconfig package.
	unset PYTHONSAFEPATH
	fakeroot debian/rules clean
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
