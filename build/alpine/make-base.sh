#!/usr/bin/env bash
# Assemble the minimal Alpine base system image from the locally built packages (build-base.sh).
#   make-base.sh
# Output (all under .build/images/, gitignored; nothing is written to the vendored tree):
#   rootfs/                        the staged root filesystem
#   <image>.rootfs.tar.gz          deterministic archive of rootfs/ (sorted, fixed mtimes, no host names)
#   <image>.rootfs.tar.gz.sha256   its checksum
#   <image>.manifest               image provenance and one line per installed package
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root

msg="$(vendor_pristine)" || die "vendored aports not pristine before the image build: $msg"
[ -s "$PKG_DIR/main/$ALPINE_ARCH/APKINDEX.tar.gz" ] || die "no local package repository; run build-base.sh first"

R="$IMAGES_DIR/rootfs"
tarball="$IMAGES_DIR/$BASE_IMAGE_NAME.rootfs.tar.gz"
manifest="$IMAGES_DIR/$BASE_IMAGE_NAME.manifest"

log "assembling $BASE_PACKAGE from the local repository only"
"$ENTER" --ephemeral --user root --env "BASE_PACKAGE=$BASE_PACKAGE" --env "BUILDER_USER=$BUILDER_USER" -- /bin/sh /guest/mkroot.sh

log "sealing $(basename "$tarball")"
# Deterministic archive: byte-order names, mtimes clamped to SOURCE_DATE_EPOCH, numeric owners (the
# image's own uids/gids, never host names), pax headers without atime/ctime, gzip without a name/mtime.
LC_ALL=C tar --create --file - --directory "$R" --sort=name --format=posix \
	--pax-option='exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime' \
	--mtime="@$SOURCE_DATE_EPOCH" --clamp-mtime --numeric-owner --xattrs --xattrs-include='*' . |
	gzip -n -9 >"$tarball"
(cd "$IMAGES_DIR" && sha256sum "$(basename "$tarball")" >"$(basename "$tarball").sha256")

# Manifest: provenance of the image and of every package in it.
{
	echo "image=$BASE_IMAGE_NAME"
	echo "alpine_version=$ALPINE_VERSION"
	echo "arch=$ALPINE_ARCH"
	echo "aports_commit=$APORTS_COMMIT"
	echo "aports_tree=$APORTS_TREE"
	echo "source_date_epoch=$SOURCE_DATE_EPOCH"
	echo "rootfs_tar_sha256=$(cut -d' ' -f1 "$tarball.sha256")"
	echo "# package version origin apk_sha256 datahash"
	awk -F: '/^P:/{p=$2} /^V:/{v=$2} /^o:/{o=$2} /^$/{if(p!="")print p, v, o; p=""; v=""; o=""} END{if(p!="")print p, v, o}' \
		"$R/lib/apk/db/installed" | LC_ALL=C sort | while read -r p v o; do
		apk="$PKG_DIR/main/$ALPINE_ARCH/$p-$v.apk"
		[ -f "$apk" ] || die "installed package $p-$v has no .apk in the local repository"
		dh="$(tar -xzOf "$apk" .PKGINFO 2>/dev/null | sed -n 's/^datahash = //p')"
		echo "$p $v $o $(sha256sum "$apk" | cut -d' ' -f1) $dh"
	done
} >"$manifest"

msg="$(vendor_pristine)" || die "vendored aports was MODIFIED by the image build: $msg"
log "image: $tarball ($(du -h "$tarball" | cut -f1)), $(grep -vc '^[a-z0-9_]*=\|^#' "$manifest") packages; vendored tree unchanged"
