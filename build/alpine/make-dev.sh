#!/usr/bin/env bash
# Extend the sealed base image with the Rust + C development layer.
#   make-dev.sh                 assemble the development image from the base image and guest/dev.lock
#   make-dev.sh --clean         remove the development image outputs (the .build/upstream cache is kept)
#   make-dev.sh --refresh-lock  re-resolve guest/dev.pkgs against the live v3.24 repositories, rewrite
#                               guest/dev.lock and fill the cache; review the diff, then rebuild
# Output (all under .build/images/, gitignored; nothing is written to the vendored tree):
#   dev-rootfs/                        the staged root filesystem
#   <image>.rootfs.tar.gz              deterministic archive of dev-rootfs/
#   <image>.rootfs.tar.gz.sha256       its checksum
#   <image>.manifest                   image provenance and one line per installed package
# Inputs: the base image archive (make-base.sh) and the checksum-pinned official packages of guest/dev.lock
# (.build/upstream, downloaded on first use; the mirror is not trusted, every sha256 is checked and apk
# verifies every package signature against the Alpine release keys of the image).
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root

R="$IMAGES_DIR/dev-rootfs"
base_tar="$IMAGES_DIR/$BASE_IMAGE_NAME.rootfs.tar.gz"
base_manifest="$IMAGES_DIR/$BASE_IMAGE_NAME.manifest"
tarball="$IMAGES_DIR/$DEV_IMAGE_NAME.rootfs.tar.gz"
manifest="$IMAGES_DIR/$DEV_IMAGE_NAME.manifest"

case "${1:-}" in
--clean)
	rm -rf "${R:?}" "$tarball" "$tarball.sha256" "$manifest"
	log "removed the development image outputs"
	exit 0
	;;
--refresh-lock)
	msg="$(vendor_pristine)" || die "vendored aports not pristine: $msg"
	[ -f "$base_tar" ] || die "no base image; run build-base.sh and make-base.sh first"
	resolved="$(mktemp)"
	trap 'rm -f "$resolved"' EXIT
	log "resolving $(grep -vc '^#' "$DEV_PKGS") top-level packages against the live $ALPINE_BRANCH repositories"
	"$ENTER" --ephemeral --user root --env "BASE_IMAGE_NAME=$BASE_IMAGE_NAME" -- /bin/sh /guest/devlock.sh >"$resolved"
	python3 "$ALPINE_BUILD_DIR/lock-dev.py" --resolved "$resolved" --aports "$REPO_ROOT/$APORTS_DIR" --upstream "$UPSTREAM_DIR" \
		--mirror "$ALPINE_MIRROR" --branch "$ALPINE_BRANCH" --arch "$ALPINE_ARCH" --out "$DEV_LOCK"
	msg="$(vendor_pristine)" || die "vendored aports was MODIFIED: $msg"
	log "wrote $DEV_LOCK ($(grep -vc '^#' "$DEV_LOCK") packages); review the diff"
	exit 0
	;;
"") ;;
*) die "usage: make-dev.sh [--clean | --refresh-lock]" ;;
esac

msg="$(vendor_pristine)" || die "vendored aports not pristine before the image build: $msg"
[ -f "$base_tar" ] && [ -f "$base_manifest" ] || die "no base image; run build-base.sh and make-base.sh first"
(cd "$IMAGES_DIR" && sha256sum --check --quiet "$BASE_IMAGE_NAME.rootfs.tar.gz.sha256") || die "base image does not match its checksum"
base_sha="$(sha256sum "$base_tar" | cut -d' ' -f1)"
[ "$(sed -n 's/^rootfs_tar_sha256=//p' "$base_manifest")" = "$base_sha" ] || die "base manifest does not match the base image"

# Every locked package must be in the cache with exactly the locked checksum. A missing file is
# downloaded (host curl, the configured proxy and CA bundle) and only kept if the checksum matches.
log "checking $(grep -vc '^#' "$DEV_LOCK") locked packages"
while read -r name ver repo origin sha; do
	case "$name" in '#'* | '') continue ;; esac
	f="$UPSTREAM_DIR/$repo/$ALPINE_ARCH/$name-$ver.apk"
	if [ ! -f "$f" ]; then
		mkdir -p "$(dirname "$f")"
		log "downloading $name-$ver.apk"
		curl --fail --silent --show-error --location --retry 3 --output "$f.part" \
			"$ALPINE_MIRROR/$ALPINE_BRANCH/$repo/$ALPINE_ARCH/$name-$ver.apk" ||
			{ rm -f "$f.part"; die "cannot download $name-$ver.apk (retired from the mirror? see README, Limits)"; }
		mv "$f.part" "$f"
	fi
	[ "$(sha256sum "$f" | cut -d' ' -f1)" = "$sha" ] || die "$name-$ver.apk does not match the checksum in dev.lock"
done <"$DEV_LOCK"

log "installing the development layer on the base image (offline, locked files only)"
"$ENTER" --ephemeral --user root --offline --env "BASE_IMAGE_NAME=$BASE_IMAGE_NAME" -- /bin/sh /guest/mkdev.sh

log "sealing $(basename "$tarball")"
LC_ALL=C tar --create --file - --directory "$R" --sort=name --format=posix \
	--pax-option='exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime' \
	--mtime="@$SOURCE_DATE_EPOCH" --clamp-mtime --numeric-owner --xattrs --xattrs-include='*' . |
	gzip -n -9 >"$tarball"
(cd "$IMAGES_DIR" && sha256sum "$(basename "$tarball")" >"$(basename "$tarball").sha256")

# Manifest: provenance of the image and of every package in it. Base packages keep the checksums
# recorded in the base manifest; the layer's packages come from guest/dev.lock.
{
	echo "image=$DEV_IMAGE_NAME"
	echo "alpine_version=$ALPINE_VERSION"
	echo "arch=$ALPINE_ARCH"
	echo "aports_commit=$APORTS_COMMIT"
	echo "aports_tree=$APORTS_TREE"
	echo "source_date_epoch=$SOURCE_DATE_EPOCH"
	echo "base_image=$BASE_IMAGE_NAME"
	echo "base_rootfs_tar_sha256=$base_sha"
	echo "rootfs_tar_sha256=$(cut -d' ' -f1 "$tarball.sha256")"
	echo "# package version origin apk_sha256 datahash"
	awk -F: '/^P:/{p=$2} /^V:/{v=$2} /^o:/{o=$2} /^$/{if(p!="")print p, v, o; p=""; v=""; o=""} END{if(p!="")print p, v, o}' \
		"$R/lib/apk/db/installed" | LC_ALL=C sort | while read -r p v o; do
		if line="$(awk -v p="$p" -v v="$v" '$1==p && $2==v {print $3, $4, $5}' "$DEV_LOCK" | head -n 1)" && [ -n "$line" ]; then
			read -r repo _ sha <<<"$line"
			apk="$UPSTREAM_DIR/$repo/$ALPINE_ARCH/$p-$v.apk"
			dh="$(tar -xzOf "$apk" .PKGINFO 2>/dev/null | sed -n 's/^datahash = //p')"
			echo "$p $v $o $sha $dh"
		elif line="$(awk -v p="$p" -v v="$v" '$1==p && $2==v {print $3, $4, $5}' "$base_manifest")" && [ -n "$line" ]; then
			echo "$p $v $line"
		else
			die "installed package $p-$v is in neither dev.lock nor the base manifest"
		fi
	done
} >"$manifest"

msg="$(vendor_pristine)" || die "vendored aports was MODIFIED by the image build: $msg"
log "image: $tarball ($(du -h "$tarball" | cut -f1)), $(grep -vc '^[a-z0-9_]*=\|^#' "$manifest") packages; vendored tree unchanged"
