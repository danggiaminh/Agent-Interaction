#!/usr/bin/env bash
# Run a command inside the development image (Rust + C), unprivileged and offline.
#   dev-run.sh [--image FILE.rootfs.tar.gz] [--copy SRC:DST]... [--out DIR:HOSTDIR] -- command [args...]
#     --copy SRC:DST     copy host directory SRC to DST inside the image before the command runs
#     --out DIR:HOSTDIR  copy DIR out of the image into HOSTDIR (created) after the command
#
# The image archive is extracted into a scratch root under .build/ inside a user namespace that maps
# ids 0-65535 of the image onto an unprivileged host range, so nothing in it holds privileges on the
# host. The command runs chrooted with a rebuilt environment (env -i), in its own mount, PID, IPC,
# UTS and network namespaces: no network, so cargo cannot download crates (vendor them: `cargo vendor`)
# and nothing from the host leaks in. The scratch root is deleted afterwards.
#
# Environment contract inside the image:
#   PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin  HOME=/root  LANG=C.UTF-8  TZ=UTC
#   SOURCE_DATE_EPOCH=<aports commit time>  CARGO_HOME=/root/.cargo  CARGO_NET_OFFLINE=true
#   CARGO_INCREMENTAL=0  CARGO_TERM_COLOR=never  CARGO_TARGET_DIR unset (target/ beside Cargo.toml)
#   CC, CXX, CFLAGS, RUSTFLAGS unset: gcc (also installed as cc) and rustc defaults for x86_64-alpine-linux-musl
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if [ "${1:-}" = "--inner" ]; then
	# ---- root of the (already mapped) user namespace, PID 1 of the new PID namespace ----
	R="$2" tarball="$3"
	shift 3
	copies=() outs=()
	while [ "${1:-}" != "--" ]; do
		case "$1" in
		--copy) copies+=("$2"); shift 2 ;;
		--out) outs+=("$2"); shift 2 ;;
		esac
	done
	shift
	# Extract here so the files get the mapped (unprivileged) owners.
	tar --extract --gzip --file "$tarball" --directory "$R" --numeric-owner --same-permissions
	mount --make-rprivate /
	hostname alpine-dev
	mount -t proc proc "$R/proc"
	mount -t tmpfs -o mode=0755 tmpfs "$R/dev"
	for n in null zero full random urandom tty; do
		: >"$R/dev/$n"
		mount --bind "/dev/$n" "$R/dev/$n"
	done
	mount -t tmpfs -o mode=1777 tmpfs "$R/tmp"
	for c in ${copies[@]+"${copies[@]}"}; do
		mkdir -p "$R${c#*:}"
		cp -a "${c%%:*}/." "$R${c#*:}/"
	done
	ip link set lo up 2>/dev/null || true
	rc=0
	chroot "$R" /usr/bin/env -i \
		PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin HOME=/root LANG=C.UTF-8 TZ=UTC \
		SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" CARGO_HOME=/root/.cargo CARGO_NET_OFFLINE=true \
		CARGO_INCREMENTAL=0 CARGO_TERM_COLOR=never TERM=dumb "$@" || rc=$?
	for o in ${outs[@]+"${outs[@]}"}; do
		mkdir -p "${o#*:}"
		cp -a "$R${o%%:*}/." "${o#*:}/" || true
	done
	exit "$rc"
fi

require_root
require_traversable "$ALPINE_BUILD_DIR"
require_traversable "$BUILD_ROOT"
image="$IMAGES_DIR/$DEV_IMAGE_NAME.rootfs.tar.gz"
pass=()
while [ $# -gt 0 ]; do
	case "$1" in
	--image) image="${2:?--image needs a file}"; shift 2 ;;
	--copy)
		src="${2%%:*}"
		[ -d "$src" ] || die "--copy: $src is not a directory"
		case "${2#*:}" in /*) ;; *) die "--copy destination must be absolute" ;; esac
		pass+=(--copy "$(cd "$src" && pwd):${2#*:}")
		shift 2
		;;
	--out) pass+=(--out "${2:?--out needs DIR:HOSTDIR}"); shift 2 ;;
	--) shift; break ;;
	*) die "unknown option $1" ;;
	esac
done
[ $# -gt 0 ] || die "no command given"
[ -f "$image" ] || die "no development image at $image; run make-dev.sh first"

scratch="$(mktemp -d "$BUILD_ROOT/dev-run.XXXXXX")"
chmod 755 "$scratch"
mkdir "$scratch/root"
chown 100000:100000 "$scratch/root"

# `newuidmap` is not installed: a holder process creates the user namespace and its id maps are
# written here as host root (the pattern of boot-test.sh).
unshare --user -- sleep 3600 >/dev/null 2>&1 &
holder=$!
cleanup() {
	kill -9 "$holder" 2>/dev/null || true
	wait "$holder" 2>/dev/null || true
	rm -rf "${scratch:?}"
}
trap cleanup EXIT
for _ in $(seq 1 100); do
	[ "$(readlink "/proc/$holder/ns/user" 2>/dev/null)" != "$(readlink /proc/$$/ns/user)" ] && break
	sleep 0.05
done
printf '0 100000 65536\n' >"/proc/$holder/uid_map"
printf '0 100000 65536\n' >"/proc/$holder/gid_map"
# Host-side destinations of --out are written by the namespace's root (host uid 100000): hand the
# result back to the caller afterwards.
outdirs=()
for ((i = 0; i < ${#pass[@]}; i += 2)); do
	if [ "${pass[i]}" = --out ]; then
		d="${pass[i + 1]#*:}"
		mkdir -p "$d"
		chown 100000:100000 "$d"
		outdirs+=("$d")
	fi
done
rc=0
nsenter --user="/proc/$holder/ns/user" --setuid 0 --setgid 0 -- \
	unshare --mount --pid --ipc --uts --net --fork --kill-child -- \
	"$ALPINE_BUILD_DIR/dev-run.sh" --inner "$scratch/root" "$image" ${pass[@]+"${pass[@]}"} -- "$@" || rc=$?
for d in ${outdirs[@]+"${outdirs[@]}"}; do chown -R 0:0 "$d"; done
exit "$rc"
