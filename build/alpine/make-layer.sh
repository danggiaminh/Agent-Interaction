#!/usr/bin/env bash
# Extend a sealed image with one more layer of official, checksum-pinned Alpine packages. Shared by
# make-dev.sh (Rust and C toolchain on the base image) and make-tools.sh (systems tools on the development
# image); use those, not this script.
#   make-layer.sh dev|tools                 assemble the layer's image from its parent image and the layer's lock
#   make-layer.sh dev|tools --clean         remove the layer's image outputs (the .build/upstream cache is kept)
#   make-layer.sh dev|tools --refresh-lock  re-resolve the layer's .pkgs against the live v3.24 repositories,
#                                           rewrite its .lock and fill the cache; review the diff, then rebuild
# Output (all under .build/images/, gitignored; nothing is written to the vendored tree):
#   <layer>-rootfs/                    the staged root filesystem
#   <image>.rootfs.tar.gz              deterministic archive of <layer>-rootfs/
#   <image>.rootfs.tar.gz.sha256       its checksum
#   <image>.manifest                   image provenance and one line per installed package
# Inputs: the parent image archive and the checksum-pinned official packages of the layer's lock
# (.build/upstream, downloaded on first use; the mirror is not trusted, every sha256 is checked and apk
# verifies every package signature against the Alpine release keys of the image).
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root

layer="${1:-}"
[ $# -gt 0 ] && shift
case "$layer" in
dev)
	label="development layer" parent_label="base image" make=make-dev.sh
	name="$DEV_IMAGE_NAME" parent="$BASE_IMAGE_NAME" parent_key=base pkgs="$DEV_PKGS" lock="$DEV_LOCK" exceptions=""
	what="the development layer adds to the base image"
	parent_hint="run build-base.sh and make-base.sh first"
	;;
tools)
	label="tools layer" parent_label="development image" make=make-tools.sh
	name="$TOOLS_IMAGE_NAME" parent="$DEV_IMAGE_NAME" parent_key=dev pkgs="$TOOLS_PKGS" lock="$TOOLS_LOCK" exceptions="$TOOLS_EXCEPTIONS"
	what="the tools layer adds to the development image"
	parent_hint="run make-dev.sh first"
	;;
*) die "usage: make-layer.sh dev|tools [--clean | --refresh-lock] (use make-dev.sh or make-tools.sh)" ;;
esac
R="$IMAGES_DIR/$layer-rootfs"
parent_tar="$IMAGES_DIR/$parent.rootfs.tar.gz"
parent_manifest="$IMAGES_DIR/$parent.manifest"
tarball="$IMAGES_DIR/$name.rootfs.tar.gz"
manifest="$IMAGES_DIR/$name.manifest"
pkgs_file="$(basename "$pkgs")" lock_file="$(basename "$lock")"

case "${1:-}" in
--clean)
	rm -rf "${R:?}" "$tarball" "$tarball.sha256" "$manifest"
	log "removed the $label image outputs"
	exit 0
	;;
--refresh-lock)
	msg="$(vendor_pristine)" || die "vendored aports not pristine: $msg"
	[ -f "$parent_tar" ] || die "no $parent_label; $parent_hint"
	resolved="$(mktemp)"
	trap 'rm -f "$resolved"' EXIT
	log "resolving $(grep -vc '^#' "$pkgs") top-level packages against the live $ALPINE_BRANCH repositories"
	"$ENTER" --ephemeral --user root --env "PARENT_IMAGE_NAME=$parent" --env "PKGS_FILE=/guest/$pkgs_file" -- /bin/sh /guest/layerlock.sh >"$resolved"
	python3 "$ALPINE_BUILD_DIR/lock-layer.py" --resolved "$resolved" --aports "$REPO_ROOT/$APORTS_DIR" --upstream "$UPSTREAM_DIR" \
		--mirror "$ALPINE_MIRROR" --branch "$ALPINE_BRANCH" --arch "$ALPINE_ARCH" --out "$lock" \
		--what "$what" --pkgs "guest/$pkgs_file" --make "$make" ${exceptions:+--exceptions "$exceptions"}
	msg="$(vendor_pristine)" || die "vendored aports was MODIFIED: $msg"
	log "wrote $lock ($(grep -vc '^#' "$lock") packages); review the diff"
	exit 0
	;;
"") ;;
*) die "usage: $make [--clean | --refresh-lock]" ;;
esac

msg="$(vendor_pristine)" || die "vendored aports not pristine before the image build: $msg"
[ -f "$parent_tar" ] && [ -f "$parent_manifest" ] || die "no $parent_label; $parent_hint"
(cd "$IMAGES_DIR" && sha256sum --check --quiet "$parent.rootfs.tar.gz.sha256") || die "$parent_label does not match its checksum"
parent_sha="$(sha256sum "$parent_tar" | cut -d' ' -f1)"
[ "$(sed -n 's/^rootfs_tar_sha256=//p' "$parent_manifest")" = "$parent_sha" ] || die "$parent_label manifest does not match the $parent_label"

# Every locked package must be in the cache with exactly the locked checksum. A missing file is
# downloaded (host curl, the configured proxy and CA bundle) and only kept if the checksum matches.
log "checking $(grep -vc '^#' "$lock") locked packages"
while read -r pname ver repo origin sha; do
	case "$pname" in '#'* | '') continue ;; esac
	f="$UPSTREAM_DIR/$repo/$ALPINE_ARCH/$pname-$ver.apk"
	if [ ! -f "$f" ]; then
		mkdir -p "$(dirname "$f")"
		log "downloading $pname-$ver.apk"
		curl --fail --silent --show-error --location --retry 3 --output "$f.part" \
			"$ALPINE_MIRROR/$ALPINE_BRANCH/$repo/$ALPINE_ARCH/$pname-$ver.apk" ||
			{ rm -f "$f.part"; die "cannot download $pname-$ver.apk (retired from the mirror? see README, Limits)"; }
		mv "$f.part" "$f"
	fi
	[ "$(sha256sum "$f" | cut -d' ' -f1)" = "$sha" ] || die "$pname-$ver.apk does not match the checksum in $lock_file"
done <"$lock"

log "installing the $label on the $parent_label (offline, locked files only)"
"$ENTER" --ephemeral --user root --offline --env "PARENT_IMAGE_NAME=$parent" --env "LAYER_ROOT=$layer-rootfs" \
	--env "PKGS_FILE=/guest/$pkgs_file" --env "LAYER_LOCK=/guest/$lock_file" -- /bin/sh /guest/mklayer.sh

log "sealing $(basename "$tarball")"
LC_ALL=C tar --create --file - --directory "$R" --sort=name --format=posix \
	--pax-option='exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime' \
	--mtime="@$SOURCE_DATE_EPOCH" --clamp-mtime --numeric-owner --xattrs --xattrs-include='*' . |
	gzip -n -9 >"$tarball"
(cd "$IMAGES_DIR" && sha256sum "$(basename "$tarball")" >"$(basename "$tarball").sha256")

# Manifest: provenance of the image and of every package in it. Packages of the parent image keep the
# checksums recorded in the parent's manifest; the layer's packages come from its lock.
{
	echo "image=$name"
	echo "alpine_version=$ALPINE_VERSION"
	echo "arch=$ALPINE_ARCH"
	echo "aports_commit=$APORTS_COMMIT"
	echo "aports_tree=$APORTS_TREE"
	echo "source_date_epoch=$SOURCE_DATE_EPOCH"
	echo "${parent_key}_image=$parent"
	echo "${parent_key}_rootfs_tar_sha256=$parent_sha"
	echo "rootfs_tar_sha256=$(cut -d' ' -f1 "$tarball.sha256")"
	echo "# package version origin apk_sha256 datahash"
	awk -F: '/^P:/{p=$2} /^V:/{v=$2} /^o:/{o=$2} /^$/{if(p!="")print p, v, o; p=""; v=""; o=""} END{if(p!="")print p, v, o}' \
		"$R/lib/apk/db/installed" | LC_ALL=C sort | while read -r p v o; do
		if line="$(awk -v p="$p" -v v="$v" '$1==p && $2==v {print $3, $4, $5}' "$lock" | head -n 1)" && [ -n "$line" ]; then
			read -r repo _ sha <<<"$line"
			apk="$UPSTREAM_DIR/$repo/$ALPINE_ARCH/$p-$v.apk"
			dh="$(tar -xzOf "$apk" .PKGINFO 2>/dev/null | sed -n 's/^datahash = //p')"
			echo "$p $v $o $sha $dh"
		elif line="$(awk -v p="$p" -v v="$v" '$1==p && $2==v {print $3, $4, $5}' "$parent_manifest")" && [ -n "$line" ]; then
			echo "$p $v $line"
		else
			die "installed package $p-$v is in neither $lock_file nor the $parent_label manifest"
		fi
	done
} >"$manifest"

msg="$(vendor_pristine)" || die "vendored aports was MODIFIED by the image build: $msg"
log "image: $tarball ($(du -h "$tarball" | cut -f1)), $(grep -vc '^[a-z0-9_]*=\|^#' "$manifest") packages; vendored tree unchanged"
