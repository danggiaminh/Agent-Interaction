#!/usr/bin/env bash
# Functional validation of the resource-control subsystem (runtime/resctl, the resctl tool): the domains, policies and
# limits of AI workloads (memory, CPU, process count, I/O) and their lifecycle (freeze, stop, kill, recover), in three
# sessions whose results are judged against resctl-test/checks.tsv and limits.tsv by resctl-judge.py:
#   unprivileged session (resctl-test/unpriv.sh): tools-run.sh, uid 0 of a user namespace with no cgroup hierarchy: the
#       crate is built, formatted, linted and unit-tested, and resctl must say that it cannot control anything and refuse
#       the commands that need a hierarchy (deny: rows) without creating anything
#   privileged session   (resctl-test/priv.sh):   tools-run.sh --privileged, real root on this host: every class of
#       behaviour on the hierarchies this host has (cgroup v1 for memory, pids and cpu; cgroup v2 for freeze, kill and
#       pressure), then the sections that depend on the lifecycle backend again with the v1 freezer forced
#   guest session        (resctl-test/guest.sh):  the same suite inside the cgroup v2 test bed (make-guest.sh): the
#       memory.high, memory.oom.group and system memory.low rows the host's cgroup v1 memory controller cannot show
# Every row ends as PASS, DENIED (refused, as it must be), LIMIT (the host lacks something: named in limits.tsv with its
# cause measured and a proof that it works where the cause does not apply) or FAIL; a limitation is never reported as a
# working capability, and the kill domains and the system domain have no exception. After the judge,
# resctl-judge-selftest.py damages copies of the facts and of the tables in the ways a broken subsystem or a dishonest
# report would and requires the judge to fail every one of them. Exit status 1 only if something FAILs.
# After the sessions the host is checked for leftovers: sandboxes, mounts, loop devices, cgroup directories, resctl
# domains, nftables tables, daemons; and the vendored tree for changes.
#   check-resctl.sh [--image FILE.rootfs.tar.gz] [--facts DIR]    --facts keeps the raw output of the sessions
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

work="$(mktemp -d "$BUILD_ROOT/resctl-check.XXXXXX")"
trap 'rm -rf "${work:?}"' EXIT
fail=0

# What the host looks like, one line per kind of state a session could leave behind.
snapshot() {
	echo "sandbox directories: $(find "$BUILD_ROOT" -maxdepth 1 -name 'tools-run.*' | wc -l)"
	echo "sandbox mounts: $(grep -c "$BUILD_ROOT/tools-run" /proc/mounts || true)"
	echo "loop devices: $(losetup -a 2>/dev/null | cut -d: -f1 | LC_ALL=C sort | tr '\n' ' ')"
	echo "cgroup directories: $(find /sys/fs/cgroup -mindepth 2 -type d 2>/dev/null | LC_ALL=C sort | md5sum | cut -c1-12)"
	echo "resctl domains: $(find /sys/fs/cgroup -maxdepth 4 -name agent-interaction -type d 2>/dev/null | wc -l)"
	echo "nftables tables: $(nft list tables 2>/dev/null | LC_ALL=C sort | tr '\n' ';')"
	echo "daemons: $(ps -eo pid=,comm= | awk '$2 ~ /^(dockerd|containerd|libvirtd|virtlogd|virtlockd|dnsmasq|qemu-system|runc|resctl)/ { printf "%s:%s ", $1, $2 }')"
}
snapshot >"$work/host.before"

# The sessions run from copies: the copy is where the unprivileged namespace ids can read it, and the sources stay
# editable while a session runs. run/ is what the sessions see as /work (the suite, the policies, the shared helpers of
# tools-test and the two C programs the suite builds), src/ the crate as /crate.
tests="$ALPINE_BUILD_DIR/resctl-test"
run="$work/run"
src="$work/src"
mkdir -p "$run/ctr" "$run/bpf" "$src" "$work/bin"
cp "$tests"/* "$run/"
cp "$ALPINE_BUILD_DIR/tools-test/common.sh" "$ALPINE_BUILD_DIR/tools-test/cgroup.sh" "$run/"
cp "$ALPINE_BUILD_DIR/tools-test/ctr/ctr-test.c" "$run/ctr/"
cp "$ALPINE_BUILD_DIR/tools-test/bpf/guest.c" "$run/bpf/"
cp -a "$REPO_ROOT/runtime/resctl/." "$src/"
rm -rf "${src:?}/target"
chmod -R a+rX "$work"
chmod a+rwx "$work/bin"

# run_session <name> [tools-run.sh options] -- <command...>: facts on stdout, console on stderr
run_session() {
	local name="$1" opts=()
	shift
	while [ "$1" != -- ]; do opts+=("$1"); shift; done
	shift
	log "running the $name session"
	if "$ALPINE_BUILD_DIR/tools-run.sh" ${opts[@]+"${opts[@]}"} ${image[@]+"${image[@]}"} -- "$@" >"$work/$name.facts" 2>"$work/$name.err"; then
		echo "PASS  the $name session ran to the end"
	else
		echo "FAIL  the $name session exited non-zero: $(tail -n 1 "$work/$name.err")"
		fail=1
	fi
}

run_session unprivileged --copy "$run:/work" --copy "$src:/crate" --out "/out:$work/bin" -- sh /work/unpriv.sh
# the release binary the unprivileged session built (and tested) is the one the other sessions run
if [ -x "$work/bin/resctl" ]; then
	cp "$work/bin/resctl" "$run/resctl"
	chmod a+rx "$run/resctl"
	echo "PASS  the privileged and guest sessions run the binary the unprivileged session built ($(stat -c %s "$run/resctl") bytes)"
else
	echo "FAIL  the unprivileged session built no resctl binary; the privileged and guest sessions cannot run"
	fail=1
	if [ -n "$keep" ]; then
		mkdir -p "$keep"
		cp "$work"/*.facts "$work"/*.err "$keep/"
	fi
	exit 1
fi
run_session privileged --privileged --copy "$run:/work" -- sh /work/priv.sh
# the guest of the cgroup v2 test bed: booted by QEMU inside an unprivileged session, so the host is never made to
# do what only the guest kernel may
[ -f "$GUEST_DIR/bed/vmlinuz" ] && [ -f "$GUEST_DIR/bed/initramfs.cpio.gz" ] || die "no test bed guest; run make-guest.sh first"
run_session guest --copy "$run:/work" --copy "$GUEST_DIR/bed:/bed" --copy "$TESTBED_DIR:/testbed" -- sh /testbed/launch.sh /work/guest.sh 1500

echo "== results"
rc=0
python3 "$ALPINE_BUILD_DIR/resctl-judge.py" "$work/unprivileged.facts" "$work/privileged.facts" "$work/guest.facts" || rc=$?
[ "$rc" = 0 ] || fail=1

echo "== judge self-test"
rc=0
python3 "$ALPINE_BUILD_DIR/resctl-judge-selftest.py" "$work/unprivileged.facts" "$work/privileged.facts" "$work/guest.facts" || rc=$?
[ "$rc" = 0 ] || fail=1

echo "== host state after the sessions"
snapshot >"$work/host.after"
while IFS= read -r line; do
	before="$(grep -F -x -- "$line" "$work/host.before" || true)"
	if [ -n "$before" ]; then echo "PASS  unchanged: ${line%%:*}"; else echo "FAIL  left behind: $line"; fail=1; fi
done <"$work/host.after"
if msg="$(vendor_pristine)"; then echo "PASS  unchanged: vendor/alpine-aports"; else echo "FAIL  vendor/alpine-aports: $msg"; fail=1; fi

if [ -n "$keep" ]; then
	mkdir -p "$keep"
	cp "$work"/*.facts "$work"/*.err "$keep/"
fi
exit "$fail"
