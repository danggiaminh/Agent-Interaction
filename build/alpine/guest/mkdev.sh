#!/bin/sh
# Runs inside the sandbox as root, offline: extend the sealed base image with the development layer
# (Rust + C toolchain) into /build/images/dev-rootfs.
#   env: ALPINE_ARCH BASE_IMAGE_NAME SOURCE_DATE_EPOCH
# Inputs: the base image archive, /guest/dev.lock (packages, versions, checksums) and /build/upstream
# (the .apk files; make-dev.sh has already checked each checksum). apk checks every package's own
# signature against the Alpine release keys that the base image ships; no repository is configured,
# so nothing can be resolved or fetched from anywhere but the locked files.
set -eu
: "${ALPINE_ARCH:?}" "${BASE_IMAGE_NAME:?}" "${SOURCE_DATE_EPOCH:?}"
R=/build/images/dev-rootfs
base="/build/images/$BASE_IMAGE_NAME.rootfs.tar.gz"

rm -rf "$R"
mkdir -p "$R"
tar --extract --gzip --file "$base" --directory "$R" --numeric-owner --xattrs --xattrs-include='*'
cp "$R/etc/apk/world" /tmp/world.base

files=""
while read -r name ver repo origin sha; do
	case "$name" in '#'* | '') continue ;; esac
	f="/build/upstream/$repo/$ALPINE_ARCH/$name-$ver.apk"
	[ -f "$f" ] || { echo "mkdev: missing $f" >&2; exit 1; }
	files="$files $f"
done </guest/dev.lock

# Pseudo-filesystems that maintainer scripts and triggers may need while running chrooted in the new
# root. They exist only in this sandbox's mount namespace and are unmounted before the image is sealed.
cleanup() {
	for n in null zero full random urandom tty; do umount "$R/dev/$n" 2>/dev/null || true; done
	umount "$R/dev" 2>/dev/null || true
	umount "$R/proc" 2>/dev/null || true
}
trap cleanup EXIT
mkdir -p "$R/proc" "$R/dev"
mount -t proc proc "$R/proc"
mount -t tmpfs -o mode=0755 tmpfs "$R/dev"
for n in null zero full random urandom tty; do
	: >"$R/dev/$n"
	mount --bind "/dev/$n" "$R/dev/$n"
done

# $files is deliberately unquoted: one argument per package file (names contain no spaces).
# --force-non-repository: installing .apk files directly is otherwise refused (it would not survive a
# reboot of a diskless system; irrelevant for an installed image).
# shellcheck disable=SC2086
apk --root "$R" --arch "$ALPINE_ARCH" --keys-dir "$R/etc/apk/keys" --repositories-file /dev/null \
	--no-network --no-cache --force-non-repository add $files

cleanup
trap - EXIT
rm -f "$R/var/log/apk.log" # wall-clock transcript, recreated on first use

# Installing files pins every package in /etc/apk/world to the checksum of its file (name><checksum).
# Record what was asked for instead: the base world plus the top-level development packages. The
# rest of the layer is their dependency closure, exactly as if installed from the repository.
{
	cat /tmp/world.base
	grep -v '^#' /guest/dev.pkgs
} | LC_ALL=C sort -u >"$R/etc/apk/world"

if grep -q " $R" /proc/mounts; then echo "mkdev: mounts left under $R" >&2; exit 1; fi
for d in dev proc; do
	[ -z "$(ls -A "$R/$d")" ] || { echo "mkdev: /$d is not empty in the image" >&2; exit 1; }
done
