#!/bin/sh
# Runs inside the sandbox as root: assemble the base system root filesystem from the locally built
# packages ONLY (no network, no upstream repository, no host files) into /build/images/rootfs.
#   env: BASE_PACKAGE ALPINE_ARCH ALPINE_BRANCH ALPINE_MIRROR SOURCE_DATE_EPOCH (BUILDER_USER, default builder)
set -eu
: "${BASE_PACKAGE:?}" "${ALPINE_ARCH:?}" "${ALPINE_BRANCH:?}" "${ALPINE_MIRROR:?}" "${SOURCE_DATE_EPOCH:?}"
R=/build/images/rootfs
builder="${BUILDER_USER:-builder}"

# Trust exactly one key: the one that signed the local packages. It is not copied into the image;
# the image trusts only the Alpine release keys that its own alpine-keys package installs.
keys=/tmp/keys
rm -rf "$R" "$keys"
mkdir -p "$R" "$keys"
cp "/home/$builder"/.abuild/*.rsa.pub "$keys"/

# Pseudo-filesystems that maintainer scripts may need while running chrooted in the new root.
# They exist only in this sandbox's mount namespace and are unmounted before the image is sealed.
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

apk --root "$R" --initdb --arch "$ALPINE_ARCH" --keys-dir "$keys" \
	--repositories-file /dev/null --repository /build/packages/main \
	--no-network --no-cache add "$BASE_PACKAGE"

cleanup
trap - EXIT
# apk's transcript of this run holds the wall-clock time and sandbox-internal paths: build residue that
# would also make the image differ from one assembly to the next. apk recreates it on first use.
rm -f "$R/var/log/apk.log"
# Nothing of the scaffolding may remain: the image ships /dev and /proc as empty directories (the
# kernel or the container runtime provides device nodes).
if grep -q " $R" /proc/mounts; then echo "mkroot: mounts left under $R" >&2; exit 1; fi
for d in dev proc; do
	[ -z "$(ls -A "$R/$d")" ] || { echo "mkroot: /$d is not empty in the image" >&2; exit 1; }
done

# busybox's post-install adds the klogd user with adduser, which stamps today's day number into the
# "last password change" field of /etc/shadow. Pin it to the day of SOURCE_DATE_EPOCH so the image does
# not depend on the date it was assembled.
day=$((SOURCE_DATE_EPOCH / 86400))
for f in "$R/etc/shadow" "$R/etc/shadow-"; do
	[ ! -f "$f" ] || sed -i -E "s/^([^:]*:[^:]*:)[0-9]+:/\1$day:/" "$f"
done

# --- image configuration (everything the packages do not already provide) ---
# Repositories pinned to this Alpine release, so the image can add packages from the same release.
printf '%s\n' "$ALPINE_MIRROR/$ALPINE_BRANCH/main" "$ALPINE_MIRROR/$ALPINE_BRANCH/community" \
	>"$R/etc/apk/repositories"

# Standard Alpine runlevel services: the set from upstream's own image script
# (vendor/alpine-aports/scripts/genapkovl-dhcp.sh) minus `modloop`, which only exists on the ISO.
# Same effect as `rc-update add <svc> <level>`.
enable() {
	level="$1"
	shift
	mkdir -p "$R/etc/runlevels/$level"
	for s in "$@"; do
		[ -x "$R/etc/init.d/$s" ] || { echo "mkroot: no init script for service '$s'" >&2; exit 1; }
		ln -sf "/etc/init.d/$s" "$R/etc/runlevels/$level/$s"
	done
}
enable sysinit devfs dmesg mdev hwdrivers
enable boot hwclock modules sysctl hostname bootmisc syslog
enable shutdown mount-ro killprocs savecache
