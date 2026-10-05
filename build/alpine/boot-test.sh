#!/usr/bin/env bash
# Boot the sealed base image and check that init brings the system up and shuts it down.
#   boot-test.sh [image.rootfs.tar.gz]      default: .build/images/<image>.rootfs.tar.gz
#
# The image's own /sbin/init (busybox init -> /etc/inittab -> OpenRC) runs as PID 1 inside new
# user, mount, PID, UTS, IPC and network namespaces, with only /proc, /sys, /dev and /run provided.
# The user namespace maps ids 0-65535 of the image onto an unprivileged host range, so the booted
# system holds no real privileges on this host: it cannot touch the host clock, sysctls, kernel log,
# devices or modules, and its files are owned by ids the host does not use. This validates
# everything above the kernel (init, OpenRC, service scripts, runlevels, shutdown); a real kernel
# and bootloader are not part of the base system and are not exercised.
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if [ "${1:-}" = "--inner" ]; then
	# ---- PID 1 of the new PID namespace, root of the (already mapped) user namespace ----
	R="$2"
	# Extract here so the files get the mapped (unprivileged) owners.
	tar --extract --gzip --file "$3" --directory "$R" --numeric-owner --same-permissions
	mount --make-rprivate /
	hostname alpine-boot
	mount -t proc proc "$R/proc"
	mount -t sysfs sysfs "$R/sys" 2>/dev/null || echo "[boot-test] note: sysfs not mountable here" >&2
	mount -t tmpfs -o mode=0755 tmpfs "$R/dev"
	for n in null zero full random urandom tty; do
		: >"$R/dev/$n"
		mount --bind "/dev/$n" "$R/dev/$n"
	done
	mount -t tmpfs -o mode=0755 tmpfs "$R/run"
	mount -t tmpfs -o mode=1777 tmpfs "$R/tmp"
	# "container=lxc" in init's environment makes OpenRC treat the system as a container and skip
	# services that need real hardware (hwclock, modules, mdev, ...).
	exec chroot "$R" /usr/bin/env -i container=lxc HOME=/ TERM=linux PATH=/usr/sbin:/usr/bin:/sbin:/bin /sbin/init
fi

require_root
fail=0
ok() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }

tarball="${1:-$IMAGES_DIR/$BASE_IMAGE_NAME.rootfs.tar.gz}"
[ -f "$tarball" ] || die "no image tarball at $tarball"
scratch="$IMAGES_DIR/boot-test"
console="$scratch/console.log" # everything init and OpenRC print
rm -rf "${scratch:?}"
mkdir -p "$scratch/root"
chown 100000:100000 "$scratch/root"

# User namespace first, with ids 0-65535 of the image mapped to the unprivileged host range
# 100000-165535. `newuidmap` is not installed, so a holder process creates the namespace and the maps
# are written here, as host root. Entering it afterwards (nsenter switches to uid 0 before exec) gives
# the boot process full capabilities inside the namespace and none outside.
unshare --user -- sleep 3600 >/dev/null 2>&1 &
holder=$!
cleanup() {
	[ -z "${unshare_pid:-}" ] || { kill -9 "$unshare_pid" 2>/dev/null || true; wait "$unshare_pid" 2>/dev/null || true; }
	kill -9 "$holder" 2>/dev/null || true
	wait "$holder" 2>/dev/null || true
	rm -rf "${scratch:?}/root"
}
trap cleanup EXIT
for _ in $(seq 1 100); do
	[ "$(readlink "/proc/$holder/ns/user" 2>/dev/null)" != "$(readlink /proc/$$/ns/user)" ] && break
	sleep 0.05
done
printf '0 100000 65536\n' >"/proc/$holder/uid_map"
printf '0 100000 65536\n' >"/proc/$holder/gid_map"
nsenter --user="/proc/$holder/ns/user" --setuid 0 --setgid 0 -- \
	unshare --mount --pid --ipc --uts --net --fork --kill-child -- \
	"$ALPINE_BUILD_DIR/boot-test.sh" --inner "$scratch/root" "$tarball" >"$console" 2>&1 &
unshare_pid=$!

# PID 1 of the new PID namespace is the unshare child.
init_pid=""
for _ in $(seq 1 50); do
	init_pid="$(pgrep -P "$unshare_pid" | head -n 1 || true)"
	[ -n "$init_pid" ] && break
	sleep 0.2
done
[ -n "$init_pid" ] || { cat "$console"; die "init did not start"; }
inns() { nsenter --target "$init_pid" --user --mount --pid --uts --ipc --net --root --wd=/ --setuid 0 --setgid 0 -- "$@"; }

# Wait for OpenRC to reach the default runlevel with nothing still starting.
up=0
for _ in $(seq 1 150); do
	if [ "$(inns /bin/rc-status -r 2>/dev/null)" = default ] && [ -z "$(inns /bin/ls /run/openrc/starting 2>/dev/null)" ]; then up=1; break; fi
	sleep 0.3
done
procs="$(inns /bin/ps -o pid,args 2>/dev/null || true)"
status="$(inns /bin/rc-status -a 2>&1 || true)"
printf '%s\n' "$procs" | sed 's/^/        /'
printf '%s\n' "$status" | sed 's/^/        /'

grep -qE '^ *1 /sbin/init$' <<<"$procs" && ok "PID 1 is /sbin/init" || bad "PID 1 is not /sbin/init"
[ "$up" = 1 ] && ok "OpenRC ran the sysinit, boot and default runlevels and reached default" || bad "OpenRC did not reach the default runlevel"
grep -q "Runlevel: default" <<<"$status" && ok "rc-status reports runlevel default" || bad "rc-status does not report runlevel default"
if grep -qiE 'crashed|stopping' <<<"$status"; then bad "a service crashed or is stuck"; else ok "no crashed services"; fi
# In container mode OpenRC skips services whose init script declares "-lxc" or "-containers" (they
# need real hardware). Every other service of the sysinit and boot runlevels must have started.
expected=0 skipped=0
for s in $(ls "$scratch/root/etc/runlevels/sysinit" "$scratch/root/etc/runlevels/boot" | grep -v ':$'); do
	if grep -hE '^\s*keyword' "$scratch/root/etc/init.d/$s" | grep -qE -- ' -(lxc|containers)( |$)'; then
		skipped=$((skipped + 1))
	else
		expected=$((expected + 1))
		if grep -qE "^ *$s +\[ +started" <<<"$status"; then ok "service $s started"; else bad "service $s did not start"; fi
	fi
done
echo "        ($expected services expected to run, $skipped skipped as hardware-bound in container mode)"
failed_lines="$(grep -E '\[ !! \]' "$console" || true)"
[ -z "$failed_lines" ] && ok "no service reported a failure during boot" || bad "services reported failures: $(printf '%s' "$failed_lines" | head -n 3 | tr '\n' ';')"
[ "$(inns /bin/cat /etc/alpine-release)" = "$ALPINE_VERSION" ] && ok "booted system is Alpine $ALPINE_VERSION" || bad "alpine-release mismatch"
inns /sbin/apk --version | grep -q . && ok "apk runs in the booted system ($(inns /sbin/apk --version))" || bad "apk does not run"

# Orderly shutdown: SIGUSR2 asks busybox init to power off (it runs the shutdown runlevel first).
inns /bin/kill -USR2 1
gone=0
for _ in $(seq 1 150); do
	if ! kill -0 "$unshare_pid" 2>/dev/null; then gone=1; break; fi
	sleep 0.3
done
[ "$gone" = 1 ] && ok "init shut the system down and exited" || bad "init did not exit after the shutdown request"
if grep -qiE 'Terminating remaining processes|Saving apk cache|going down|Requesting system' "$console"; then ok "shutdown runlevel ran (console log)"; else bad "no shutdown activity on the console"; fi
printf '        console log: %s lines, kept at %s\n' "$(wc -l <"$console")" "$console"

trap - EXIT
cleanup
[ "$(grep -c "$scratch" /proc/mounts)" = 0 ] && ok "no boot-test mounts leaked into the host" || bad "mounts leaked into the host"
if [ "$fail" = 0 ]; then echo "RESULT: image boots"; else echo "RESULT: boot test FAILED"; fi
exit "$fail"
