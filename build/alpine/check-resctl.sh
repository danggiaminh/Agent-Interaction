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
#   check-resctl.sh [--image FILE.rootfs.tar.gz] [--facts DIR] [--areas LIST] [--no-guest] [--mutant NAME] [--deadline S]
#     --facts DIR    keeps the raw output of the sessions and the status of every row
#     --deadline S   stops each session after S seconds instead of its own deadline (unprivileged 1200, privileged 2400,
#                    guest 2100: several times what a session takes, and above the 1500 s the guest's QEMU may run);
#                    a stopped session is a FAIL, and what it left behind is checked like after any other run
#     --areas LIST   runs only these areas of the suite (resctl-judge.py --list-areas) in the privileged and guest sessions,
#                    and layout, which measures what the causes of the host's limits name; the unprivileged session always
#                    runs in full, it builds and tests the binary the others run
#     --no-guest     does not boot the test bed guest
#     --mutant NAME  negative control: the crate is built with the deliberate defect NAME of resctl-mutants.py, the areas
#                    that the defect concerns (or --areas) run on this host, and in the guest only for a defect that only
#                    the guest can show, and the run passes only if the judge fails every row the defect must break
# A run with --areas, --no-guest or --mutant is partial: the judge reports what ran (resctl-judge.py --partial), the
# self-test of the judge, which needs the facts of a full run, does not run, and the result is never a full verdict.
# The whole script is one compound command, so bash has read all of it before it runs any of it: an edit of this file
# during a run (a run takes minutes) cannot change what that run does.
{
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root

image=()
keep=""
deadline=""
areas=""
guest=yes
mutant=""
while [ $# -gt 0 ]; do
	case "$1" in
	--image) image=(--image "${2:?--image needs a file}"); shift 2 ;;
	--facts) keep="${2:?--facts needs a directory}"; shift 2 ;;
	--areas) areas="${2:?--areas needs a list}"; shift 2 ;;
	--no-guest) guest=no; shift ;;
	--mutant) mutant="${2:?--mutant needs a name (resctl-mutants.py list)}"; shift 2 ;;
	--deadline)
		case "${2:-}" in '' | *[!0-9]* | 0) die "--deadline needs a number of seconds" ;; esac
		deadline="$2"
		shift 2
		;;
	*) die "unknown option $1" ;;
	esac
done
mutants=(python3 "$ALPINE_BUILD_DIR/resctl-mutants.py")
if [ -n "$mutant" ]; then
	"${mutants[@]}" show "$mutant" >/dev/null || exit 1
	[ -n "$areas" ] || areas="$("${mutants[@]}" areas "$mutant")"
	[ "$("${mutants[@]}" guest "$mutant")" = yes ] || guest=no
fi
known="$(python3 "$ALPINE_BUILD_DIR/resctl-judge.py" --list-areas)"
areas="$(echo "$areas" | tr ',' ' ' | xargs)"
for a in $areas; do
	case " $known " in *" $a "*) ;; *) die "--areas: $a is not an area of the suite ($known)" ;; esac
done
# layout always runs with other areas: it measures the facts that the causes of the host's limits name
case " $areas " in "  " | *" layout "*) ;; *) areas="layout $areas" ;; esac
partial=no
if [ -n "$areas" ] || [ "$guest" = no ]; then partial=yes; fi

work="$(mktemp -d "$BUILD_ROOT/resctl-check.XXXXXX")"
trap 'rm -rf "${work:?}"' EXIT
fail=0

host_snapshot >"$work/host.before"
# the host and the vendored tree compared with their state before the sessions: after every run, also one that stops
# early because a session failed or was stopped at its deadline
host_check() {
	echo "== host state after the sessions"
	host_snapshot >"$work/host.after"
	host_compare "$work/host.before" "$work/host.after" || fail=1
	if msg="$(vendor_pristine)"; then echo "PASS  unchanged: vendor/alpine-aports"; else echo "FAIL  vendor/alpine-aports: $msg"; fail=1; fi
}

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
if [ -n "$mutant" ]; then
	"${mutants[@]}" apply "$mutant" "$src" "$run" || die "the defect $mutant does not apply to the sources: update resctl-mutants.py"
fi
# the sections of the suite that priv.sh and guest.sh run (all of them when the file is absent)
if [ -n "$areas" ]; then echo "$areas" >"$run/areas"; fi
chmod -R a+rX "$work"
chmod a+rwx "$work/bin"

# run_session <name> <deadline> [tools-run.sh options] -- <command...>: facts on stdout, console on stderr. The session
# is stopped at its deadline (tools-run.sh --deadline), and a stopped session is reported apart from one that failed.
run_session() {
	local name="$1" limit="${deadline:-$2}" opts=() t0=$SECONDS
	shift 2
	while [ "$1" != -- ]; do opts+=("$1"); shift; done
	shift
	log "running the $name session (deadline $limit s)"
	if "$ALPINE_BUILD_DIR/tools-run.sh" --deadline "$limit" ${opts[@]+"${opts[@]}"} ${image[@]+"${image[@]}"} -- "$@" >"$work/$name.facts" 2>"$work/$name.err"; then
		echo "PASS  the $name session ran to the end ($((SECONDS - t0)) s)"
	elif grep -qE '^(tools|dev)-run: deadline: ' "$work/$name.err"; then
		echo "FAIL  the $name session was stopped at its deadline of $limit s (what it left behind is checked below)"
		fail=1
	else
		echo "FAIL  the $name session exited non-zero after $((SECONDS - t0)) s: $(tail -n 1 "$work/$name.err")"
		fail=1
	fi
}

run_session unprivileged 1200 --copy "$run:/work" --copy "$src:/crate" --out "/out:$work/bin" -- sh /work/unpriv.sh
# the release binary the unprivileged session built (and tested) is the one the other sessions run
if [ -x "$work/bin/resctl" ]; then
	cp "$work/bin/resctl" "$run/resctl"
	chmod a+rx "$run/resctl"
	echo "PASS  the privileged and guest sessions run the binary the unprivileged session built ($(stat -c %s "$run/resctl") bytes)"
else
	echo "FAIL  the unprivileged session built no resctl binary; the privileged and guest sessions cannot run"
	fail=1
	host_check
	if [ -n "$keep" ]; then
		mkdir -p "$keep"
		cp "$work"/*.facts "$work"/*.err "$keep/"
	fi
	exit 1
fi
run_session privileged 2400 --privileged --copy "$run:/work" -- sh /work/priv.sh
facts=("$work/unprivileged.facts" "$work/privileged.facts")
# the guest of the cgroup v2 test bed: booted by QEMU inside an unprivileged session, so the host is never made to
# do what only the guest kernel may
if [ "$guest" = yes ]; then
	[ -f "$GUEST_DIR/bed/vmlinuz" ] && [ -f "$GUEST_DIR/bed/initramfs.cpio.gz" ] || die "no test bed guest; run make-guest.sh first"
	run_session guest 2100 --copy "$run:/work" --copy "$GUEST_DIR/bed:/bed" --copy "$TESTBED_DIR:/testbed" -- sh /testbed/launch.sh /work/guest.sh 1500
	facts+=("$work/guest.facts")
fi

echo "== results"
judge=(--status "$work/status.tsv")
if [ "$partial" = yes ]; then judge+=(--partial); fi
if [ -n "$areas" ]; then judge+=(--areas "$areas"); fi
rc=0
python3 "$ALPINE_BUILD_DIR/resctl-judge.py" "${judge[@]}" "${facts[@]}" || rc=$?
if [ -n "$mutant" ]; then
	# the defect must make the judge fail: what is judged here is whether it failed the rows it must
	echo "== negative control: $mutant"
	rc=0
	"${mutants[@]}" verify "$mutant" "$work/status.tsv" || rc=$?
fi
[ "$rc" = 0 ] || fail=1

if [ "$partial" = no ]; then
	echo "== judge self-test"
	rc=0
	python3 "$ALPINE_BUILD_DIR/resctl-judge-selftest.py" "${facts[@]}" || rc=$?
	[ "$rc" = 0 ] || fail=1
else
	echo "== judge self-test: not run (it damages the facts of a full run; this run is partial)"
fi

host_check

if [ -n "$keep" ]; then
	mkdir -p "$keep"
	cp "$work"/*.facts "$work"/*.err "$work/status.tsv" "$keep/"
fi
exit "$fail"
}
