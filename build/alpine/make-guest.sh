#!/usr/bin/env bash
# Build the guest of the cgroup v2 test bed: a pinned, signed Alpine kernel and a tiny stage-1 initramfs, to be
# booted under QEMU by tools-test (see README, "Cgroup v2 test bed"). The cloud host's own kernel offers only a
# hybrid cgroup v1/v2 hierarchy; this guest is how the tools image's cgroup v2 behaviour is exercised anyway.
#   make-guest.sh           build .build/guest/bed/{vmlinuz,initramfs.cpio.gz,manifest} from the pinned kernel
#   make-guest.sh --check   verify the pins and that the bed in .build/guest/bed is exactly what they produce
#   make-guest.sh --clean   remove .build/guest
# Inputs (all pinned and checked): guest/kernel.lock (the official linux-lts package, sha256), guest/kernel.config
# (options it must have), guest/kernel.modules (modules the initramfs loads), the tools image (busybox and musl
# for the initramfs), testbed/init and testbed/mkinitramfs.py. The kernel package is downloaded on first use.
# The kernel is the guest of the tests only: nothing of it is installed into any image.
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root

mode="${1:-build}"
case "$mode" in
build | --check) ;;
--clean)
	rm -rf "${GUEST_DIR:?}"
	log "removed the test bed guest"
	exit 0
	;;
*) die "usage: make-guest.sh [--check | --clean]" ;;
esac

bed="$GUEST_DIR/bed"
tools_root="$IMAGES_DIR/tools-rootfs"
tools_tar="$IMAGES_DIR/$TOOLS_IMAGE_NAME.rootfs.tar.gz"
[ -f "$tools_tar" ] && [ -d "$tools_root" ] || die "no tools image; run make-tools.sh first"
tools_sha="$(sha256sum "$tools_tar" | cut -d' ' -f1)"
[ "$(sed -n 's/^rootfs_tar_sha256=//p' "$IMAGES_DIR/$TOOLS_IMAGE_NAME.manifest")" = "$tools_sha" ] ||
	die "the tools image archive does not match its manifest"

fail=0
ok() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }

# ---- the pin: one row, an official package, exactly the locked bytes, a valid Alpine signature
rows="$(grep -v '^#' "$KERNEL_LOCK" | awk 'NF')"
[ "$(printf '%s\n' "$rows" | wc -l)" -eq 1 ] || die "$(basename "$KERNEL_LOCK") must hold exactly one kernel package"
read -r kname kver krepo korigin ksha <<<"$rows"
[ "$krepo" = main ] || die "the guest kernel must come from the main repository, not $krepo"
apk_file="$UPSTREAM_DIR/$krepo/$ALPINE_ARCH/$kname-$kver.apk"
if [ ! -f "$apk_file" ]; then
	mkdir -p "$(dirname "$apk_file")"
	log "downloading $kname-$kver.apk"
	curl --fail --silent --show-error --location --retry 3 --output "$apk_file.part" \
		"$ALPINE_MIRROR/$ALPINE_BRANCH/$krepo/$ALPINE_ARCH/$kname-$kver.apk" ||
		{ rm -f "$apk_file.part"; die "cannot download $kname-$kver.apk"; }
	mv "$apk_file.part" "$apk_file"
fi
[ "$(sha256sum "$apk_file" | cut -d' ' -f1)" = "$ksha" ] && ok "$kname-$kver.apk is the pinned file (sha256 in $(basename "$KERNEL_LOCK"))" ||
	die "$kname-$kver.apk does not match the sha256 in $(basename "$KERNEL_LOCK")"
if out="$("$ENTER" --ephemeral --user root --offline -- apk --keys-dir /build/images/tools-rootfs/etc/apk/keys verify \
	"/build/upstream/$krepo/$ALPINE_ARCH/$kname-$kver.apk" 2>&1)"; then
	ok "$kname-$kver.apk carries a valid signature of the Alpine release keys"
else
	die "apk verify of $kname-$kver.apk failed: $(printf '%s\n' "$out" | head -n 3)"
fi

# ---- the vendored aport: same version, or a one-line reason why not (and no reason that no longer applies)
vendored="$(python3 -c 'import sys; sys.path.insert(0, sys.argv[1]); import apkbuild; print(apkbuild.version(sys.argv[2]))' \
	"$ALPINE_BUILD_DIR" "$REPO_ROOT/$APORTS_DIR/$krepo/$korigin/APKBUILD")"
exc="$(grep -v '^#' "$KERNEL_EXCEPTIONS" | awk -v o="$korigin" '$1 == o' || true)"
if [ "$kver" = "$vendored" ]; then
	[ -z "$exc" ] && ok "$kname $kver is the version of the vendored aport" ||
		bad "$(basename "$KERNEL_EXCEPTIONS") has an entry for $korigin, but the lock equals the vendored version $vendored: stale"
else
	read -r _ elver evver _ <<<"$exc"
	if [ -n "$exc" ] && [ "$elver" = "$kver" ] && [ "$evver" = "$vendored" ]; then
		ok "$kname $kver differs from the vendored aport ($vendored): excepted in $(basename "$KERNEL_EXCEPTIONS")"
	else
		bad "$kname $kver differs from the vendored aport $vendored with no matching entry in $(basename "$KERNEL_EXCEPTIONS")"
	fi
fi
[ "$fail" = 0 ] || die "the guest kernel pin is not consistent"

# ---- extract boot/ and lib/modules/ of the verified package into a fresh directory
build_dir="$(mktemp -d "$BUILD_ROOT/guest-build.XXXXXX")"
trap 'rm -rf "${build_dir:?}"' EXIT
mkdir "$build_dir/x" "$build_dir/out"
tar -xzf "$apk_file" -C "$build_dir/x" --no-same-owner --no-same-permissions --wildcards 'boot/*' 'lib/modules/*' 2>"$build_dir/tar.log" ||
	{ cat "$build_dir/tar.log" >&2; die "cannot extract $kname-$kver.apk"; }
set -- "$build_dir"/x/boot/vmlinuz-*
[ $# -eq 1 ] && [ -f "$1" ] || die "$kname-$kver.apk does not hold exactly one kernel image"
vmlinuz="$1"
set -- "$build_dir"/x/boot/config-*
[ $# -eq 1 ] && [ -f "$1" ] || die "$kname-$kver.apk does not hold exactly one kernel config"
kconfig="$1"
set -- "$build_dir"/x/lib/modules/*
[ $# -eq 1 ] && [ -d "$1" ] || die "$kname-$kver.apk does not hold exactly one module tree"
moddir="$1"

# ---- the kernel has what the test bed needs
nopt=0
while read -r opt state; do
	case "$opt" in '#'* | '') continue ;; esac
	nopt=$((nopt + 1))
	grep -qx "CONFIG_$opt=$state" "$kconfig" || bad "the kernel is built without CONFIG_$opt=$state (guest/kernel.config)"
done <"$KERNEL_CONFIG"
[ "$fail" = 0 ] && ok "the kernel has all $nopt options of $(basename "$KERNEL_CONFIG")" || die "the guest kernel lacks required options"

# ---- the initramfs: busybox and musl of the tools image, the init script, the module closure
python3 -I "$TESTBED_DIR/mkinitramfs.py" --image-root "$tools_root" --kernel-modules "$moddir" --modules "$KERNEL_MODULES" \
	--init "$TESTBED_DIR/init" --out "$build_dir/out/initramfs.cpio.gz" --epoch "$SOURCE_DATE_EPOCH" ||
	die "cannot assemble the initramfs"
cp "$vmlinuz" "$build_dir/out/vmlinuz"
{
	echo "kernel_package=$kname-$kver"
	echo "kernel_apk_sha256=$ksha"
	echo "kernel_release=$(basename "$moddir")"
	echo "vmlinuz_sha256=$(sha256sum "$build_dir/out/vmlinuz" | cut -d' ' -f1)"
	echo "initramfs_sha256=$(sha256sum "$build_dir/out/initramfs.cpio.gz" | cut -d' ' -f1)"
	echo "tools_image_sha256=$tools_sha"
	echo "source_date_epoch=$SOURCE_DATE_EPOCH"
	echo "modules=$(grep -v '^#' "$KERNEL_MODULES" | awk 'NF' | tr '\n' ' ' | sed 's/ $//')"
} >"$build_dir/out/manifest"

if [ "$mode" = --check ]; then
	[ -f "$bed/manifest" ] || die "no test bed in $bed; run make-guest.sh"
	for f in vmlinuz initramfs.cpio.gz manifest; do
		cmp -s "$bed/$f" "$build_dir/out/$f" && ok "bed/$f is reproduced byte for byte from the pins" ||
			bad "bed/$f differs from what the pins produce (run make-guest.sh)"
	done
	[ "$fail" = 0 ]
	exit
fi

rm -rf "${bed:?}"
mkdir -p "$GUEST_DIR"
cp -a "$build_dir/out" "$bed"
chmod -R a+rX "$GUEST_DIR"
ok "test bed guest: $(du -h "$bed/vmlinuz" | cut -f1) kernel $(basename "$moddir"), $(du -h "$bed/initramfs.cpio.gz" | cut -f1) initramfs"
