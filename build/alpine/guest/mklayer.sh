#!/bin/sh
# Runs inside the sandbox as root, offline: extend a sealed parent image with one layer of official
# packages into /build/images/$LAYER_ROOT (dev: base image + Rust and C toolchain; tools: development
# image + systems tools).
#   env: ALPINE_ARCH PARENT_IMAGE_NAME LAYER_ROOT PKGS_FILE LAYER_LOCK SOURCE_DATE_EPOCH
# Inputs: the parent image archive, the layer's lock (packages, versions, checksums) and /build/upstream
# (the .apk files; make-layer.sh has already checked each checksum). apk checks every package's own
# signature against the Alpine release keys that the parent image ships; no repository is configured,
# so nothing can be resolved or fetched from anywhere but the locked files.
set -eu
: "${ALPINE_ARCH:?}" "${PARENT_IMAGE_NAME:?}" "${LAYER_ROOT:?}" "${PKGS_FILE:?}" "${LAYER_LOCK:?}" "${SOURCE_DATE_EPOCH:?}"
R="/build/images/$LAYER_ROOT"
parent="/build/images/$PARENT_IMAGE_NAME.rootfs.tar.gz"

rm -rf "$R"
mkdir -p "$R"
tar --extract --gzip --file "$parent" --directory "$R" --numeric-owner --xattrs --xattrs-include='*'
cp "$R/etc/apk/world" /tmp/world.parent

# Locked packages of repo "local" come from the locally built repository and are signed with this
# environment's package key; that one key is trusted for this run only (it is not copied into the image).
files="" keys="$R/etc/apk/keys"
while read -r name ver repo origin sha; do
	case "$name" in '#'* | '') continue ;; esac
	if [ "$repo" = local ]; then
		f="/build/packages/main/$ALPINE_ARCH/$name-$ver.apk"
		if [ "$keys" = "$R/etc/apk/keys" ]; then
			keys=/tmp/keys
			rm -rf "$keys" && mkdir "$keys"
			cp "$R"/etc/apk/keys/* "$keys"/
			cp "/home/${BUILDER_USER:-builder}"/.abuild/*.rsa.pub "$keys"/
		fi
	else
		f="/build/upstream/$repo/$ALPINE_ARCH/$name-$ver.apk"
	fi
	[ -f "$f" ] || { echo "mklayer: missing $f" >&2; exit 1; }
	files="$files $f"
done <"$LAYER_LOCK"

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
apk --root "$R" --arch "$ALPINE_ARCH" --keys-dir "$keys" --repositories-file /dev/null \
	--no-network --no-cache --force-non-repository add $files

cleanup
trap - EXIT
rm -f "$R/var/log/apk.log" # wall-clock transcript, recreated on first use

# Packages that add system users (the tools layer adds several) go through adduser, which stamps today's
# day number into the "last password change" field of /etc/shadow. Pin it to the day of SOURCE_DATE_EPOCH,
# as mkroot.sh does, so the image does not depend on the date it was assembled. A no-op for lines that
# are already pinned.
day=$((SOURCE_DATE_EPOCH / 86400))
for f in "$R/etc/shadow" "$R/etc/shadow-"; do
	[ ! -f "$f" ] || sed -i -E "s/^([^:]*:[^:]*:)[0-9]+:/\1$day:/" "$f"
done

# Installing files pins every package in /etc/apk/world to the checksum of its file (name><checksum).
# Record what was asked for instead: the parent's world plus the layer's top-level packages. The
# rest of the layer is their dependency closure, exactly as if installed from the repository.
{
	cat /tmp/world.parent
	grep -v '^#' "$PKGS_FILE"
} | LC_ALL=C sort -u >"$R/etc/apk/world"

if grep -q " $R" /proc/mounts; then echo "mklayer: mounts left under $R" >&2; exit 1; fi
for d in dev proc; do
	[ -z "$(ls -A "$R/$d")" ] || { echo "mklayer: /$d is not empty in the image" >&2; exit 1; }
done
