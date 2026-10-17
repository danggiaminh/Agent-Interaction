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
# After the sessions the host is checked for leftovers (host_snapshot of lib.sh): sandboxes, their mounts and processes,
# loop devices, cgroup directories, nftables tables, daemons.
#   tools-test.sh [--image FILE.rootfs.tar.gz] [--facts DIR] [--deadline S]
#     --facts DIR    keeps the raw output of the sessions
#     --deadline S   stops each session after S seconds instead of its own deadline (unprivileged and privileged 2400,
#                    guest 1500, above the 900 s the guest's QEMU may run); a stopped session is a FAIL, and what it
#                    left behind is checked like after any other run
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root

image=()
keep=""
deadline=""
while [ $# -gt 0 ]; do
	case "$1" in
	--image) image=(--image "${2:?--image needs a file}"); shift 2 ;;
	--facts) keep="${2:?--facts needs a directory}"; shift 2 ;;
	--deadline)
		case "${2:-}" in '' | *[!0-9]* | 0) die "--deadline needs a number of seconds" ;; esac
		deadline="$2"
		shift 2
		;;
	*) die "unknown option $1" ;;
	esac
done

work="$(mktemp -d "$BUILD_ROOT/tools-test.XXXXXX")"
trap 'rm -rf "${work:?}"' EXIT
fail=0

host_snapshot >"$work/host.before"

# The sessions run from a copy of tools-test: the copy is where the unprivileged namespace ids can read it, and the
# source stays editable while a session runs.
tests="$work/tools-test"
cp -a "$ALPINE_BUILD_DIR/tools-test" "$tests"
chmod -R a+rX "$work" "$tests"

# run_session <name> <deadline> [tools-run.sh options] -- <command...>: facts on stdout, console on stderr. The session
# is stopped at its deadline (tools-run.sh --deadline), and a stopped session is reported apart from one that failed.
run_session() {
	local name="$1" limit="${deadline:-$2}" opts=() t0=$SECONDS
	shift 2
	while [ "$1" != -- ]; do opts+=("$1"); shift; done
	shift
	log "running the $name session (deadline $limit s)"
	if "$ALPINE_BUILD_DIR/tools-run.sh" --deadline "$limit" ${opts[@]+"${opts[@]}"} ${image[@]+"${image[@]}"} --copy "$tests:/work" -- "$@" >"$work/$name.facts" 2>"$work/$name.err"; then
		echo "PASS  the $name session ran to the end ($((SECONDS - t0)) s)"
	elif grep -qE '^(tools|dev)-run: deadline: ' "$work/$name.err"; then
		echo "FAIL  the $name session was stopped at its deadline of $limit s (what it left behind is checked below)"
		fail=1
	else
		echo "FAIL  the $name session exited non-zero after $((SECONDS - t0)) s: $(tail -n 1 "$work/$name.err")"
		fail=1
	fi
}
run_session unprivileged 2400 -- sh /work/unpriv.sh
run_session privileged 2400 --privileged -- sh /work/priv.sh
# the guest of the cgroup v2 test bed: booted by QEMU inside an unprivileged session, so the host is never made to
# do what only the guest kernel may
[ -f "$GUEST_DIR/bed/vmlinuz" ] && [ -f "$GUEST_DIR/bed/initramfs.cpio.gz" ] || die "no test bed guest; run make-guest.sh first"
run_session guest 1500 --copy "$GUEST_DIR/bed:/bed" --copy "$TESTBED_DIR:/testbed" -- sh /testbed/launch.sh /work/guest.sh 900

echo "== results"
rc=0
python3 "$ALPINE_BUILD_DIR/tools-judge.py" "$work/unprivileged.facts" "$work/privileged.facts" "$work/guest.facts" || rc=$?
[ "$rc" = 0 ] || fail=1

echo "== judge self-test"
rc=0
python3 "$ALPINE_BUILD_DIR/tools-judge-selftest.py" "$work/unprivileged.facts" "$work/privileged.facts" "$work/guest.facts" || rc=$?
[ "$rc" = 0 ] || fail=1

echo "== host state after the sessions"
host_snapshot >"$work/host.after"
host_compare "$work/host.before" "$work/host.after" || fail=1

if [ -n "$keep" ]; then
	mkdir -p "$keep"
	cp "$work"/*.facts "$work"/*.err "$keep/"
fi
exit "$fail"
