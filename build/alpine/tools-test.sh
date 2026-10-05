#!/usr/bin/env bash
# Functional validation of the tools image: every tool of guest/tools.pkgs is run, in two sessions of
# tools-run.sh, and its result is judged against tools-test/inventory.tsv by tools-judge.py.
#   unprivileged session (tools-test/unpriv.sh): compile, link, format, lint, test, debug, trace, profile,
#       benchmark, pressure, filesystem and disk-image tools, QEMU emulation
#   privileged session   (tools-test/priv.sh):   loop devices and kernel mounts, FUSE, ftrace and perf
#       tracepoints, eBPF, network namespaces, firewalls, cgroup limits, libvirt with QEMU, Docker, compose
# Every row ends as PASS, LIMIT (the cloud host lacks a capability the tool needs: reported as a limitation,
# never as operational) or FAIL. Exit status 1 only if something FAILs. After the sessions the host is
# checked for leftovers: sandboxes, mounts, loop devices, cgroup directories, nftables tables, daemons.
#   tools-test.sh [--image FILE.rootfs.tar.gz] [--facts DIR]    --facts keeps the raw output of both sessions
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root

image=()
keep=""
while [ $# -gt 0 ]; do
	case "$1" in
	--image) image=(--image "${2:?--image needs a file}"); shift 2 ;;
	--facts) keep="${2:?--facts needs a directory}"; shift 2 ;;
	*) die "unknown option $1" ;;
	esac
done

work="$(mktemp -d "$BUILD_ROOT/tools-test.XXXXXX")"
trap 'rm -rf "${work:?}"' EXIT
fail=0

# What the host looks like, one line per kind of state a session could leave behind.
snapshot() {
	echo "sandbox directories: $(find "$BUILD_ROOT" -maxdepth 1 -name 'tools-run.*' | wc -l)"
	echo "sandbox mounts: $(grep -c "$BUILD_ROOT/tools-run" /proc/mounts || true)"
	echo "loop devices: $(losetup -a 2>/dev/null | cut -d: -f1 | LC_ALL=C sort | tr '\n' ' ')"
	echo "cgroup directories: $(find /sys/fs/cgroup -mindepth 2 -type d 2>/dev/null | LC_ALL=C sort | md5sum | cut -c1-12)"
	echo "nftables tables: $(nft list tables 2>/dev/null | LC_ALL=C sort | tr '\n' ';')"
	echo "daemons: $(ps -eo pid=,comm= | awk '$2 ~ /^(dockerd|containerd|libvirtd|virtlogd|virtlockd|dnsmasq|qemu-system|runc)/ { printf "%s:%s ", $1, $2 }')"
}
snapshot >"$work/host.before"

run_session() { # run_session <name> <script> [tools-run.sh options]
	local name="$1" script="$2"
	shift 2
	log "running the $name session ($script)"
	if "$ALPINE_BUILD_DIR/tools-run.sh" "$@" ${image[@]+"${image[@]}"} --copy "$ALPINE_BUILD_DIR/tools-test:/work" -- sh "/work/$script" >"$work/$name.facts" 2>"$work/$name.err"; then
		echo "PASS  the $name session ran to the end"
	else
		echo "FAIL  the $name session exited non-zero: $(tail -n 1 "$work/$name.err")"
		fail=1
	fi
}
run_session unprivileged unpriv.sh
run_session privileged priv.sh --privileged

echo "== results"
rc=0
python3 "$ALPINE_BUILD_DIR/tools-judge.py" "$work/unprivileged.facts" "$work/privileged.facts" || rc=$?
[ "$rc" = 0 ] || fail=1

echo "== host state after the sessions"
snapshot >"$work/host.after"
while IFS= read -r line; do
	before="$(grep -F -x -- "$line" "$work/host.before" || true)"
	if [ -n "$before" ]; then echo "PASS  unchanged: ${line%%:*}"; else echo "FAIL  left behind: $line"; fail=1; fi
done <"$work/host.after"

if [ -n "$keep" ]; then
	mkdir -p "$keep"
	cp "$work"/*.facts "$work"/*.err "$keep/"
fi
exit "$fail"
