#!/usr/bin/env bash
# Functional validation of the tools image: every tool of guest/tools.pkgs is run, in three sessions, and its
# result is judged against tools-test/inventory.tsv, limits.tsv and capabilities.tsv by tools-judge.py.
#   unprivileged session (tools-test/unpriv.sh): tools-run.sh, uid 0 of a user namespace (not host root): compile, link,
#       format, lint, test, debug, trace, profile, benchmark, pressure, filesystem and disk-image tools, namespaces,
#       QEMU emulation; and the operations that must be refused to it (deny: rows)
#   privileged session   (tools-test/priv.sh):   tools-run.sh --privileged, real root: loop devices and kernel mounts, FUSE,
#       ftrace and perf tracepoints, eBPF, network namespaces, firewalls, cgroup v1 and v2 limits, libvirt with QEMU,
#       Docker, compose; and the measurements of the host that explain a limitation (host_* facts)
#   guest session        (tools-test/guest.sh):  the same tools inside the cgroup v2 test bed (make-guest.sh), a pinned
#       kernel booted under QEMU from the unprivileged session: everything the host kernel cannot give
# Every inventory row ends as PASS, DENIED (an unprivileged operation was refused, as it must be), LIMIT (the host lacks
# something the tool needs: named in limits.tsv, with its cause measured and a proof that the tool works elsewhere),
# UPSTREAM (a known defect of a packaged tool) or FAIL. A limitation of the host is never reported as operational and
# never counted as a defect of the toolchain; a privileged-only capability is never reported as working without
# privilege. After the judge, tools-judge-selftest.py damages copies of the facts in the ways a broken toolchain or a
# dishonest report would and requires the judge to fail every one of them. Exit status 1 only if something FAILs.
# After the sessions the host is checked for leftovers: sandboxes, mounts, loop devices, cgroup directories, nftables
# tables, daemons.
#   tools-test.sh [--image FILE.rootfs.tar.gz] [--facts DIR]    --facts keeps the raw output of the sessions
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

# The sessions run from a copy of tools-test: the copy is where the unprivileged namespace ids can read it, and the
# source stays editable while a session runs.
tests="$work/tools-test"
cp -a "$ALPINE_BUILD_DIR/tools-test" "$tests"
chmod -R a+rX "$work" "$tests"

run_session() { # run_session <name> [tools-run.sh options] -- <command...>: facts on stdout, console on stderr
	local name="$1" opts=()
	shift
	while [ "$1" != -- ]; do opts+=("$1"); shift; done
	shift
	log "running the $name session"
	if "$ALPINE_BUILD_DIR/tools-run.sh" ${opts[@]+"${opts[@]}"} ${image[@]+"${image[@]}"} --copy "$tests:/work" -- "$@" >"$work/$name.facts" 2>"$work/$name.err"; then
		echo "PASS  the $name session ran to the end"
	else
		echo "FAIL  the $name session exited non-zero: $(tail -n 1 "$work/$name.err")"
		fail=1
	fi
}
run_session unprivileged -- sh /work/unpriv.sh
run_session privileged --privileged -- sh /work/priv.sh
# the guest of the cgroup v2 test bed: booted by QEMU inside an unprivileged session, so the host is never made to
# do what only the guest kernel may
[ -f "$GUEST_DIR/bed/vmlinuz" ] && [ -f "$GUEST_DIR/bed/initramfs.cpio.gz" ] || die "no test bed guest; run make-guest.sh first"
run_session guest --copy "$GUEST_DIR/bed:/bed" --copy "$TESTBED_DIR:/testbed" -- sh /testbed/launch.sh /work/guest.sh 900

echo "== results"
rc=0
python3 "$ALPINE_BUILD_DIR/tools-judge.py" "$work/unprivileged.facts" "$work/privileged.facts" "$work/guest.facts" || rc=$?
[ "$rc" = 0 ] || fail=1

echo "== judge self-test"
rc=0
python3 "$ALPINE_BUILD_DIR/tools-judge-selftest.py" "$work/unprivileged.facts" "$work/privileged.facts" "$work/guest.facts" || rc=$?
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
