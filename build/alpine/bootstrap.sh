#!/usr/bin/env bash
# Create the pinned Alpine build rootfs under .build/rootfs.
#
#   bootstrap.sh [--force] [--refresh-lock]
#
#   --force         discard and recreate .build/rootfs (the cached, verified tarball is reused)
#   --refresh-lock  install unpinned toolchain packages and rewrite toolchain.lock
#
# Steps: fetch minirootfs -> verify sha256 + GPG signature against the pinned values -> extract ->
# pin apk repositories to the v3.24 branch -> install the toolchain (exact versions from
# toolchain.lock when present) -> create the builder user and signing key -> record the lock.
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

force=0
refresh=0
for a in "$@"; do
	case "$a" in
	--force) force=1 ;;
	--refresh-lock) refresh=1 ;;
	*) die "usage: bootstrap.sh [--force] [--refresh-lock]" ;;
	esac
done
require_root
for t in curl tar sha256sum gpg unshare mount chroot git; do command -v "$t" >/dev/null || die "missing host tool: $t"; done
mkdir -p "$CACHE_DIR" "$WORK_DIR" "$PKG_DIR" "$DISTFILES_DIR"
chown "$BUILDER_UID:$BUILDER_GID" "$WORK_DIR" "$PKG_DIR" "$DISTFILES_DIR"

if [ -f "$ROOTFS_DIR/.bootstrap-complete" ] && [ "$force" = 0 ]; then
	log "rootfs already bootstrapped (use --force to recreate)"
	exit 0
fi

# 1. Fetch and verify the pinned base.
for f in "$ROOTFS_FILE" "$ROOTFS_FILE.asc"; do
	[ -s "$CACHE_DIR/$f" ] || { log "fetching $f"; curl -sS -f -o "$CACHE_DIR/$f" "$ROOTFS_BASE_URL/$f"; }
done
echo "$ROOTFS_SHA256  $CACHE_DIR/$ROOTFS_FILE" | sha256sum -c - >/dev/null ||
	die "sha256 mismatch for $CACHE_DIR/$ROOTFS_FILE (delete it to refetch)"
gnupg_home="$(mktemp -d)"
trap 'rm -rf "$gnupg_home"' EXIT
gpg --homedir "$gnupg_home" --batch --quiet --import "$ALPINE_BUILD_DIR/ncopa.asc" 2>/dev/null
status="$(gpg --homedir "$gnupg_home" --batch --status-fd 1 --verify "$CACHE_DIR/$ROOTFS_FILE.asc" "$CACHE_DIR/$ROOTFS_FILE" 2>/dev/null)" ||
	die "GPG signature check failed for $ROOTFS_FILE"
signer="$(printf '%s\n' "$status" | awk '$2=="VALIDSIG"{print $NF}')"
[ "$signer" = "$ROOTFS_SIGNER_FPR" ] || die "signed by '$signer', expected $ROOTFS_SIGNER_FPR"
log "verified $ROOTFS_FILE: sha256 ok, GPG signature by $ROOTFS_SIGNER_FPR"

# 2. Extract a fresh rootfs.
[ "$ROOTFS_DIR" = "$REPO_ROOT/.build/rootfs" ] || die "refusing to wipe unexpected path $ROOTFS_DIR"
rm -rf "$ROOTFS_DIR" "${BUILD_ROOT:?}"/overlay.* "$ROOTFS_DIGEST_FILE"
mkdir -p "$ROOTFS_DIR"
tar -xzf "$CACHE_DIR/$ROOTFS_FILE" -C "$ROOTFS_DIR" --numeric-owner

# 3. Pin the package repositories to the release branch (no edge, no testing).
printf '%s\n' "$ALPINE_MIRROR/$ALPINE_BRANCH/main" "$ALPINE_MIRROR/$ALPINE_BRANCH/community" >"$ROOTFS_DIR/etc/apk/repositories"

# 4. Install the toolchain, create builder + signing key.
provision_env=(--env "BUILDER_USER=$BUILDER_USER" --env "BUILDER_UID=$BUILDER_UID" --env "BUILDER_GID=$BUILDER_GID"
	--env "TOOLCHAIN_PKGS=$TOOLCHAIN_PKGS" --env "REFRESH_LOCK=$refresh")
"$ENTER" --user root "${provision_env[@]}" -- /bin/sh /guest/provision.sh

# 5. Record the lock when none exists yet (or when refreshing).
if [ ! -s "$LOCK_FILE" ] || [ "$refresh" = 1 ]; then
	"$ENTER" --user root -- /bin/sh /guest/lock.sh >"$LOCK_FILE.tmp"
	grep -v '^#' "$LOCK_FILE.tmp" | grep -qv '=' && die "malformed lock entries in $LOCK_FILE.tmp"
	mv "$LOCK_FILE.tmp" "$LOCK_FILE"
	log "wrote $LOCK_FILE ($(grep -vc '^#' "$LOCK_FILE") packages)"
fi

printf 'alpine=%s rootfs_sha256=%s aports_tree=%s\n' "$ALPINE_VERSION" "$ROOTFS_SHA256" "$APORTS_TREE" >"$ROOTFS_DIR/.bootstrap-complete"
# 6. Fingerprint the finished toolchain so later runs can prove it did not drift.
rootfs_digest >"$ROOTFS_DIGEST_FILE"
log "toolchain rootfs digest $(cat "$ROOTFS_DIGEST_FILE")"
log "bootstrap complete: $ROOTFS_DIR"
