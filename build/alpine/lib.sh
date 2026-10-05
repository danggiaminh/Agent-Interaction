#!/usr/bin/env bash
# Shared definitions for the Alpine build environment scripts. Source this file; do not execute it.
set -euo pipefail

ALPINE_BUILD_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$ALPINE_BUILD_DIR/../.." && pwd)"
# shellcheck disable=SC1091
. "$ALPINE_BUILD_DIR/config.env"

BUILD_ROOT="$REPO_ROOT/.build" # everything generated lives here (gitignored)
CACHE_DIR="$BUILD_ROOT/cache"
ROOTFS_DIR="$BUILD_ROOT/rootfs"
WORK_DIR="$BUILD_ROOT/work"
PKG_DIR="$BUILD_ROOT/packages"
DISTFILES_DIR="$BUILD_ROOT/distfiles"
IMAGES_DIR="$BUILD_ROOT/images"
UPSTREAM_DIR="$BUILD_ROOT/upstream" # checksum-pinned upstream .apk files of the development layer
LOGS_DIR="$BUILD_ROOT/logs"
LOCK_FILE="$ALPINE_BUILD_DIR/guest/toolchain.lock" # visible inside the sandbox as /guest/toolchain.lock
BASE_ORIGINS="$ALPINE_BUILD_DIR/guest/base.origins" # aports to build, in order (repo/name)
BASE_LOCK="$ALPINE_BUILD_DIR/guest/base.lock"       # name=version of every package in the base image
DEV_PKGS="$ALPINE_BUILD_DIR/guest/dev.pkgs"         # top-level packages of the development layer
DEV_LOCK="$ALPINE_BUILD_DIR/guest/dev.lock"         # name version repo origin sha256 of every package the layer adds
ENTER="$ALPINE_BUILD_DIR/enter.sh"

ROOTFS_DIGEST_FILE="$BUILD_ROOT/rootfs.digest" # digest of the toolchain rootfs right after bootstrap

# Digest of the toolchain rootfs without the paths every sandbox run legitimately touches (mount
# points, pseudo-filesystems, the resolver copy). Any other difference means the toolchain drifted.
rootfs_digest() {
	python3 "$ALPINE_BUILD_DIR/tree-digest.py" "$ROOTFS_DIR" \
		--exclude proc --exclude dev --exclude tmp --exclude run --exclude aports --exclude guest \
		--exclude build --exclude etc/resolv.conf
}

log() { printf '[alpine-env] %s\n' "$*"; }
die() { printf '[alpine-env] ERROR: %s\n' "$*" >&2; exit 1; }
require_root() { [ "$(id -u)" -eq 0 ] || die "root is required (mount namespaces + chroot); re-run with sudo"; }
# boot-test.sh and dev-run.sh run as an unprivileged host id (100000, root of a user namespace); it must be
# able to walk down to the scripts and the scratch directory, so every directory on the way needs "x" for others.
require_traversable() {
	local d="$1"
	while [ "$d" != / ]; do
		[ "$(( 0$(stat -c %a "$d") & 1 ))" -eq 1 ] || die "$d is not searchable by other users (mode $(stat -c %a "$d")); the unprivileged namespace id cannot reach $1. Keep the repository under a world-searchable path or run: chmod o+x $d"
		d="$(dirname "$d")"
	done
}

# Succeeds only if the vendored aports tree is exactly the pinned git tree and nothing was added
# or changed. Ignored files count: upstream's .gitignore hides src/, pkg/, *.apk and similar.
vendor_pristine() {
	local tree dirty
	tree="$(git -C "$REPO_ROOT" rev-parse "HEAD:$APORTS_DIR" 2>/dev/null)" || { echo "cannot resolve HEAD:$APORTS_DIR"; return 1; }
	[ "$tree" = "$APORTS_TREE" ] || { echo "tree $tree != pinned $APORTS_TREE"; return 1; }
	dirty="$(git -C "$REPO_ROOT" status --porcelain --ignored --untracked-files=all -- "$APORTS_DIR")"
	[ -z "$dirty" ] || { printf '%s\n' "$dirty" | head -n 10; return 1; }
}
