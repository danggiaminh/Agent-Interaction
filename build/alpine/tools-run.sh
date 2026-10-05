#!/usr/bin/env bash
# Run a command inside the tools image (development image + debug, trace, profile, benchmark, pressure,
# filesystem, VM and container tools), offline.
#   tools-run.sh [--privileged] [--image FILE.rootfs.tar.gz] [--copy SRC:DST]... [--out DIR:HOSTDIR] -- command [args...]
#     --copy SRC:DST     copy host directory SRC to DST inside the image before the command runs
#     --out DIR:HOSTDIR  copy DIR out of the image into HOSTDIR (created) after the command
#     --privileged       run as the real root of the host in private mount, PID, IPC, UTS, network and cgroup
#                        namespaces (the image is the pivot_root'ed root of the command's mount namespace), with
#                        the host's devices, sysfs, tracefs and cgroup hierarchies: what loop
#                        mounts, libvirtd, dockerd, ftrace, eBPF and network namespaces need
#
# Without --privileged this is dev-run.sh on the tools image: an unprivileged user namespace (ids 0-65535 of
# the image mapped onto an unprivileged host range), no network, a private /dev and /tmp. Everything that
# does not need host privileges (compile, lint, debug, profile, benchmark, pressure, image building, QEMU
# emulation) runs this way.
#
# --privileged needs real root on the host and shares the kernel with it; the runner removes what it can
# leave behind: processes (the PID namespace dies with the command), mounts and network state (private
# namespaces), loop devices attached and cgroup directories created during the run, and the extracted root.
# The network namespace starts with a loopback device only; nothing reaches the network.
#
# Environment inside the image is the contract of dev-run.sh (PATH, HOME=/root, LANG=C.UTF-8, TZ=UTC,
# SOURCE_DATE_EPOCH, CARGO_*), plus hostname alpine-tools when --privileged.
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if [ "${1:-}" = "--inner-privileged" ]; then
	# ---- PID 1 of the new PID namespace, real root, new mount/IPC/UTS/network/cgroup namespaces ----
	R="$2"
	shift 2
	copies=() outs=()
	while [ "${1:-}" != "--" ]; do
		case "$1" in
		--copy) copies+=("$2"); shift 2 ;;
		--out) outs+=("$2"); shift 2 ;;
		esac
	done
	shift
	mount --make-rprivate /
	mount --rbind "$R" "$R"
	mount --make-rprivate "$R"
	hostname alpine-tools
	mount -t proc proc "$R/proc"
	mount --rbind /dev "$R/dev"
	mount -t sysfs sysfs "$R/sys"
	# The host's cgroup hierarchies, as the cgroup namespace sees them (rooted at this process's cgroup).
	if [ "$(stat -f -c %T /sys/fs/cgroup)" = cgroup2fs ]; then
		mount -t cgroup2 cgroup2 "$R/sys/fs/cgroup"
	else
		mount -t tmpfs -o mode=755 tmpfs "$R/sys/fs/cgroup"
		while IFS=: read -r _ ctl _; do
			[ -n "$ctl" ] || continue
			d="${ctl#name=}"
			d="${d%%,*}"
			mkdir -p "$R/sys/fs/cgroup/$d"
			mount -t cgroup -o "$ctl" cgroup "$R/sys/fs/cgroup/$d" 2>/dev/null || echo "tools-run: cgroup $ctl not mounted" >&2
		done </proc/self/cgroup
		if grep -q '^0::' /proc/self/cgroup && [ -d /sys/fs/cgroup/unified ]; then
			mkdir -p "$R/sys/fs/cgroup/unified"
			mount -t cgroup2 cgroup2 "$R/sys/fs/cgroup/unified" 2>/dev/null || true
		fi
	fi
	mount -t tracefs tracefs "$R/sys/kernel/tracing" 2>/dev/null || true
	mount -t tmpfs -o mode=1777 tmpfs "$R/tmp"
	mount -t tmpfs -o mode=755 tmpfs "$R/run"
	for c in ${copies[@]+"${copies[@]}"}; do
		mkdir -p "$R${c#*:}"
		cp -a "${c%%:*}/." "$R${c#*:}/"
	done
	ip link set lo up 2>/dev/null || chroot "$R" /sbin/ip link set lo up 2>/dev/null || true
	# What exists before, to remove only what this run created.
	cg_before="$(mktemp)"
	loops_before="$(mktemp)"
	find /sys/fs/cgroup -mindepth 2 -type d 2>/dev/null | LC_ALL=C sort >"$cg_before"
	losetup -a 2>/dev/null | cut -d: -f1 | LC_ALL=C sort >"$loops_before" || true
	rc=0
	# The command gets the image as the root of its own mount namespace (pivot_root, not chroot): runc and
	# nsenter place a process that joins a container at the root of that container's mount namespace, which
	# is the image root only when dockerd itself was not merely chrooted.
	unshare --mount -- sh -c '
		r="$1"; shift
		cd "$r" && pivot_root . . && umount -l . || exit 125
		exec /usr/bin/env -i "$@"' sh "$R" \
		PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin HOME=/root LANG=C.UTF-8 TZ=UTC \
		SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" CARGO_HOME=/root/.cargo CARGO_NET_OFFLINE=true \
		CARGO_INCREMENTAL=0 CARGO_TERM_COLOR=never TERM=dumb "$@" || rc=$?
	for o in ${outs[@]+"${outs[@]}"}; do
		mkdir -p "${o#*:}"
		cp -a "$R${o%%:*}/." "${o#*:}/" || true
	done
	# Leave nothing running; detach loop devices and remove cgroup directories created during the run.
	kill -9 -1 2>/dev/null || true
	sleep 0.5
	losetup -a 2>/dev/null | cut -d: -f1 | LC_ALL=C sort | comm -13 "$loops_before" - | while read -r l; do losetup -d "$l" 2>/dev/null || true; done
	find /sys/fs/cgroup -mindepth 2 -type d 2>/dev/null | LC_ALL=C sort | comm -13 "$cg_before" - | sort -r | while read -r d; do rmdir "$d" 2>/dev/null || true; done
	rm -f "$cg_before" "$loops_before"
	exit "$rc"
fi

require_root
require_traversable "$ALPINE_BUILD_DIR"
require_traversable "$BUILD_ROOT"
image="$IMAGES_DIR/$TOOLS_IMAGE_NAME.rootfs.tar.gz"
privileged=0
pass=()
while [ $# -gt 0 ]; do
	case "$1" in
	--privileged) privileged=1; shift ;;
	--image) image="${2:?--image needs a file}"; shift 2 ;;
	--copy | --out) pass+=("$1" "${2:?$1 needs an argument}"); shift 2 ;;
	--) shift; break ;;
	*) die "unknown option $1" ;;
	esac
done
[ $# -gt 0 ] || die "no command given"
[ -f "$image" ] || die "no tools image at $image; run make-tools.sh first"

if [ "$privileged" = 0 ]; then
	exec "$ALPINE_BUILD_DIR/dev-run.sh" --image "$image" ${pass[@]+"${pass[@]}"} -- "$@"
fi

# Validate and absolutise --copy sources as dev-run.sh does.
args=()
for ((i = 0; i < ${#pass[@]}; i += 2)); do
	case "${pass[i]}" in
	--copy)
		src="${pass[i + 1]%%:*}"
		[ -d "$src" ] || die "--copy: $src is not a directory"
		case "${pass[i + 1]#*:}" in /*) ;; *) die "--copy destination must be absolute" ;; esac
		args+=(--copy "$(cd "$src" && pwd):${pass[i + 1]#*:}")
		;;
	*) args+=("${pass[i]}" "${pass[i + 1]}") ;;
	esac
done

scratch="$(mktemp -d "$BUILD_ROOT/tools-run.XXXXXX")"
chmod 755 "$scratch"
mkdir "$scratch/root"
trap 'rm -rf "${scratch:?}"' EXIT
tar --extract --gzip --file "$image" --directory "$scratch/root" --numeric-owner --same-permissions
rc=0
unshare --mount --pid --ipc --uts --net --cgroup --fork --kill-child -- \
	"$ALPINE_BUILD_DIR/tools-run.sh" --inner-privileged "$scratch/root" ${args[@]+"${args[@]}"} -- "$@" || rc=$?
exit "$rc"
