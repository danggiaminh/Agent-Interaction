#!/usr/bin/env bash
# Run a command inside the isolated Alpine build sandbox (default: interactive /bin/sh).
#
#   enter.sh [--user builder|root] [--offline] [--ephemeral] [--env NAME=VALUE]... [--] [command [args...]]
#
# Isolation:
#   * new mount, PID, IPC and UTS namespaces; chroot into .build/rootfs
#   * the vendored aports tree is bind-mounted READ-ONLY at /aports
#   * the environment is rebuilt from scratch (env -i): nothing from the host (tokens, locale,
#     proxies...) leaks in, except the HTTPS proxy and its CA bundle when the host uses one
#   * --offline adds a network namespace (loopback only)
#   * --ephemeral runs on a throwaway overlay of the rootfs: whatever the command installs or changes
#     (abuild installs and purges build dependencies, whose triggers leave residue) is discarded, so
#     every build starts from the identical, pristine toolchain
# Writable, persistent state is limited to /build/{work,packages,distfiles} (and /build/images for root);
# /tmp and /run are tmpfs.
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if [ "${1:-}" != "--inner" ]; then
	user="$BUILDER_USER"
	offline=0
	ephemeral=0
	extra=()
	while [ $# -gt 0 ]; do
		case "$1" in
		--user) user="${2:?--user needs a value}"; shift 2 ;;
		--offline) offline=1; shift ;;
		--ephemeral) ephemeral=1; shift ;;
		--env) extra+=("${2:?--env needs NAME=VALUE}"); shift 2 ;;
		--) shift; break ;;
		*) break ;;
		esac
	done
	case "$user" in root | "$BUILDER_USER") ;; *) die "--user must be root or $BUILDER_USER" ;; esac
	require_root
	[ -f "$ROOTFS_DIR/etc/alpine-release" ] || die "rootfs missing; run build/alpine/bootstrap.sh first"
	export ENTER_USER="$user" ENTER_OFFLINE="$offline" ENTER_OVERLAY=""
	ENTER_EXTRA_ENV="$(printf '%s\n' ${extra[@]+"${extra[@]}"})"
	export ENTER_EXTRA_ENV
	flags=(--mount --pid --ipc --uts --fork --kill-child)
	[ "$offline" = 1 ] && flags+=(--net)
	if [ "$ephemeral" = 1 ]; then
		mkdir -p "$BUILD_ROOT"
		ENTER_OVERLAY="$(mktemp -d "$BUILD_ROOT/overlay.XXXXXX")"
		export ENTER_OVERLAY
		trap 'rm -rf "$ENTER_OVERLAY"' EXIT
		rc=0
		unshare "${flags[@]}" -- "$ALPINE_BUILD_DIR/enter.sh" --inner "$@" || rc=$?
		exit "$rc"
	fi
	exec unshare "${flags[@]}" -- "$ALPINE_BUILD_DIR/enter.sh" --inner "$@"
fi
shift # --inner

# ---- inside the new namespaces (this process is PID 1 and reaps orphans) ----
R="$ROOTFS_DIR"
mount --make-rprivate /
hostname alpine-build
umask 022
if [ -n "$ENTER_OVERLAY" ]; then
	mkdir -p "$ENTER_OVERLAY/upper" "$ENTER_OVERLAY/work" "$ENTER_OVERLAY/merged"
	mount -t overlay overlay -o "lowerdir=$ROOTFS_DIR,upperdir=$ENTER_OVERLAY/upper,workdir=$ENTER_OVERLAY/work" "$ENTER_OVERLAY/merged"
	R="$ENTER_OVERLAY/merged"
fi

mount -t proc proc "$R/proc"
mount -t tmpfs -o mode=0755,nosuid tmpfs "$R/dev"
for n in null zero full random urandom tty; do
	[ -e "/dev/$n" ] || continue
	: >"$R/dev/$n"
	mount --bind "/dev/$n" "$R/dev/$n"
done
mkdir -p "$R/dev/pts"
if mount -t devpts -o newinstance,ptmxmode=0666,mode=0620 devpts "$R/dev/pts" 2>/dev/null; then
	ln -sf pts/ptmx "$R/dev/ptmx"
else
	log "warning: devpts unavailable, no pty inside the sandbox"
fi
ln -sf /proc/self/fd "$R/dev/fd"
ln -sf /proc/self/fd/0 "$R/dev/stdin"
ln -sf /proc/self/fd/1 "$R/dev/stdout"
ln -sf /proc/self/fd/2 "$R/dev/stderr"
mount -t tmpfs -o mode=1777,nosuid,nodev tmpfs "$R/tmp"
mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs "$R/run"

bind() { # bind <src> <dst> [ro]; src may be a directory or a single file
	if [ -d "$1" ]; then
		mkdir -p "$2"
	else
		mkdir -p "$(dirname "$2")"
		[ -e "$2" ] || : >"$2"
	fi
	mount --bind "$1" "$2"
	if [ "${3:-rw}" = ro ]; then mount -o remount,bind,ro "$2"; fi
}
bind "$REPO_ROOT/$APORTS_DIR" "$R/aports" ro
bind "$ALPINE_BUILD_DIR/guest" "$R/guest" ro
bind "$WORK_DIR" "$R/build/work"
bind "$PKG_DIR" "$R/build/packages"
bind "$DISTFILES_DIR" "$R/build/distfiles"
mkdir -p "$IMAGES_DIR" # root-owned: only the image tooling (run as root) writes here
bind "$IMAGES_DIR" "$R/build/images"

if [ "$ENTER_USER" = root ]; then home=/root; runner=(); else home="/home/$BUILDER_USER"; runner=(/sbin/su-exec "$BUILDER_USER"); fi
envv=(
	"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
	"HOME=$home"
	"LANG=C.UTF-8"
	"TZ=UTC"
	"TERM=dumb"
	"SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"
	"REPODEST=/build/packages"
	"SRCDEST=/build/distfiles"
	"DISTFILES_MIRROR=$DISTFILES_MIRROR"
	"PACKAGER=$PACKAGER"
	"JOBS=$BUILD_JOBS"
	"ALPINE_VERSION=$ALPINE_VERSION"
	"ALPINE_BRANCH=$ALPINE_BRANCH"
	"ALPINE_ARCH=$ALPINE_ARCH"
	"ALPINE_MIRROR=$ALPINE_MIRROR"
	"APORTS_COMMIT=$APORTS_COMMIT"
	"APORTS_TREE=$APORTS_TREE"
)

if [ "$ENTER_OFFLINE" = 1 ]; then
	chroot "$R" /sbin/ip link set lo up 2>/dev/null || true
else
	cat /etc/resolv.conf >"$R/etc/resolv.conf" 2>/dev/null || true
	# Hosts behind a TLS-intercepting HTTPS proxy: pass the proxy and trust the CA bundle it needs.
	if [ -n "${HTTPS_PROXY:-}" ]; then
		envv+=("HTTPS_PROXY=$HTTPS_PROXY" "https_proxy=$HTTPS_PROXY")
		if [ -n "${NO_PROXY:-}" ]; then envv+=("NO_PROXY=$NO_PROXY" "no_proxy=$NO_PROXY"); fi
		ca="${BUILD_CA_BUNDLE:-${SSL_CERT_FILE:-}}"
		if [ -n "$ca" ] && [ -f "$ca" ]; then bind "$ca" "$R/etc/ssl/certs/ca-certificates.crt" ro; fi
	fi
fi

mapfile -t extra_env < <(printf '%s\n' "$ENTER_EXTRA_ENV" | sed '/^$/d')
cmd=("$@")
[ ${#cmd[@]} -gt 0 ] || cmd=(/bin/sh)

set +e
chroot "$R" /usr/bin/env -i "${envv[@]}" ${extra_env[@]+"${extra_env[@]}"} ${runner[@]+"${runner[@]}"} "${cmd[@]}"
exit $?
