#!/usr/bin/env bash
# Check of the session deadline of the sandbox runners (tools-run.sh and dev-run.sh --deadline), which check-resctl.sh
# and tools-test.sh give every session they run:
#   a command still running at its deadline is stopped (SIGTERM, SIGKILL 10 s later), also when it ignores SIGTERM, has
#       a process outside its process group or froze one, and the runner exits 124 and says so on stderr;
#   nothing of it is left on the host afterwards: no process, cgroup directory, loop device or sandbox, privileged or not;
#   a command that ends before its deadline keeps its own exit status, 124 included: only the runner's deadline is
#       reported as one;
#   a session of check-resctl.sh stopped at its deadline is reported as stopped, and the host is checked after it.
# First the comparison of the host before and after a run (host_snapshot and host_compare of lib.sh, which check-resctl.sh
# and tools-test.sh rely on) must see leftovers planted on purpose, one of each kind the cases could leave: a check of the
# cleanup that cannot see a leftover would pass whatever the runners do. For every case the outcome of the deadline and
# that of the cleanup are reported apart. Every case runs under a safety timeout of its own, so a deadline that does not
# work ends as a FAIL, not as a hang, and what a failed case left of its own is removed afterwards (and reported). The
# cases run on the base image (a few MiB, extracted at once): the deadline is the runner's, not the image's; the stopped
# session of check-resctl.sh runs on the tools image, as it always does.
#   check-deadline.sh [--defect NAME]
#     --defect NAME  negative control: this check runs on a copy of build/alpine (and of the crate, which the stopped
#                    session builds) that has the deliberate defect NAME of the runners, the repository's scripts are never
#                    changed, and the run passes only if the copy's check FAILs what the defect breaks; NAME is one of
#                    no-thaw (the privileged runner does not thaw what the command froze), no-message (it does not say
#                    that the deadline stopped the command) and no-dev-deadline (the unprivileged runner has no deadline)
# Exit status 1 if anything FAILs. Like check-resctl.sh it is one compound command, read whole before it runs.
{
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root
defect=""
case "${1:-}" in
'') ;;
--defect) defect="${2:?--defect needs a name}" ;;
*) die "unknown option $1" ;;
esac
image="$IMAGES_DIR/$BASE_IMAGE_NAME.rootfs.tar.gz"
[ -f "$image" ] || die "no base image at $image; run make-base.sh first"

work="$(mktemp -d "$BUILD_ROOT/deadline-check.XXXXXX")"
trap 'rm -rf "${work:?}"' EXIT
fail=0

if [ -n "$defect" ]; then
	# the defects: the file of build/alpine, the text, its replacement, and the start of the FAIL line it must cause
	table="$work/defects.py"
	cat >"$table" <<'EOF'
import sys
DEFECTS = {
    "no-thaw": ("tools-run.sh",
                '\twhile read -r d; do\n\t\tif [ -f "$d/freezer.state" ]; then echo THAWED >"$d/freezer.state" 2>/dev/null || true; fi\n'
                '\t\tif [ -f "$d/cgroup.freeze" ]; then echo 0 >"$d/cgroup.freeze" 2>/dev/null || true; fi\n\tdone <"$cg_before.new"\n',
                '\t: no thaw\n', "FAIL  tools-run-hang: cleanup: "),
    "no-message": ("tools-run.sh",
                   'echo "tools-run: deadline: the command was still running after ${deadline} s: stopped" >&2',
                   ': no message', "FAIL  tools-run-hang: deadline: "),
    "no-dev-deadline": ("dev-run.sh",
                        'if [ "$deadline" -gt 0 ]; then limit=(timeout --kill-after=10 "$deadline"); fi',
                        'if false; then limit=(timeout --kill-after=10 "$deadline"); fi', "FAIL  dev-run-hang: deadline: "),
}
name, what = sys.argv[1], sys.argv[2]
if name not in DEFECTS:
    sys.exit("check-deadline: no defect named %s (one of: %s)" % (name, ", ".join(DEFECTS)))
f, old, new, breaks = DEFECTS[name]
if what == "breaks":
    print(breaks)
else:
    path = what + "/" + f
    text = open(path).read()
    if text.count(old) != 1:
        sys.exit("check-deadline: %s: the text to replace occurs %d times in %s, not once: the table must follow the "
                 "runners" % (name, text.count(old), f))
    open(path, "w").write(text.replace(old, new))
EOF
	breaks="$(python3 "$table" "$defect" breaks)" || exit 1
	copy="$work/copy"
	mkdir -p "$copy/build" "$copy/runtime" "$copy/.build/images"
	cp -a "$ALPINE_BUILD_DIR" "$copy/build/alpine"
	cp -a "$REPO_ROOT/runtime/resctl" "$copy/runtime/resctl"
	rm -rf "${copy:?}/runtime/resctl/target"
	# the images as hard links: the copy's runners extract the same archives
	ln "$image" "$IMAGES_DIR/$TOOLS_IMAGE_NAME.rootfs.tar.gz" "$copy/.build/images/"
	chmod 755 "$work" "$copy" "$copy/build" "$copy/.build"
	python3 "$table" "$defect" "$copy/build/alpine" || exit 1
	echo "== negative control $defect: this check on a copy of build/alpine with the defect"
	host_snapshot >"$work/defect.before"
	rc=0
	"$copy/build/alpine/check-deadline.sh" >"$work/defect.log" 2>&1 || rc=$?
	grep '^FAIL' "$work/defect.log" | sed 's/^/      the copy: /'
	if [ "$rc" = 1 ] && grep -q "^$breaks" "$work/defect.log"; then
		ok "negative control $defect: caught: $(grep -m 1 "^$breaks" "$work/defect.log" | cut -c7-)"
	else
		bad "negative control $defect: the copy's check exited $rc without '$breaks...': the defect was not caught"
	fi
	host_snapshot >"$work/defect.after"
	host_compare "$work/defect.before" "$work/defect.after" || fail=1
	exit "$fail"
fi
tag="check-deadline-$$" # the name of everything the cases create
nap=4917                # the sleep of the cases: its command line marks their processes
deadline=5
grace=10 # the runners send SIGKILL this long after the SIGTERM of the deadline
ok() { echo "PASS  $*"; }
bad() { echo "FAIL  $*"; fail=1; }
host_snapshot >"$work/host.start"

# The command of the cases, in the image. case.sh MODE TAG NAP [STATUS]: exit ends at once with STATUS; hang puts a
# process outside its process group, ignores SIGTERM (and so does what it runs) and sleeps; hang-root first freezes a
# process in a cgroup of its own (a frozen process does not even die of SIGKILL) and attaches a loop device: what only a
# privileged session can leave behind.
mkdir "$work/case"
cat >"$work/case/case.sh" <<'EOF'
set -eu
mode="$1" tag="$2" nap="$3"
mkdir -p /out
date +%s >/out/start
if [ "$mode" = exit ]; then exit "$4"; fi
if [ "$mode" = hang-root ]; then
	if [ -d /sys/fs/cgroup/freezer ]; then
		g="/sys/fs/cgroup/freezer/$tag"
		mkdir "$g"
		sleep "$nap" &
		echo "$!" >"$g/cgroup.procs"
		echo FROZEN >"$g/freezer.state"
		frozen() { [ "$(cat "$g/freezer.state")" = FROZEN ]; }
	else
		g=/sys/fs/cgroup
		[ -f "$g/cgroup.controllers" ] || g=/sys/fs/cgroup/unified
		g="$g/$tag"
		mkdir "$g"
		sleep "$nap" &
		echo "$!" >"$g/cgroup.procs"
		echo 1 >"$g/cgroup.freeze"
		frozen() { grep -qx 'frozen 1' "$g/cgroup.events"; }
	fi
	i=0
	until frozen; do
		i=$((i + 1))
		if [ "$i" -gt 10 ]; then echo "case: $g did not freeze" >&2; exit 1; fi
		sleep 1
	done
	echo "$g" >/out/frozen
	dd if=/dev/zero of="/tmp/$tag.img" bs=1024 count=1024 2>/dev/null
	losetup -f "/tmp/$tag.img"
	losetup -a | grep -F "$tag" >/out/loop
fi
setsid sleep "$nap" &
trap '' TERM
echo ready >/out/ready
sleep "$nap"
EOF
chmod -R a+rX "$work"

# run_case NAME SAFETY [tools-run.sh options] -- [case.sh arguments]: the case through tools-run.sh, under a safety
# timeout of SAFETY seconds. Sets rc, end (when the runner returned) and took (its seconds); the runner's stderr is
# $work/NAME.err, what the case wrote to /out is in $work/NAME.out.
run_case() {
	local name="$1" safety="$2" opts=() t0
	shift 2
	while [ "$1" != -- ]; do opts+=("$1"); shift; done
	shift
	log "case $name"
	rc=0
	t0=$(date +%s)
	timeout --kill-after=10 "$safety" "$ALPINE_BUILD_DIR/tools-run.sh" --image "$image" ${opts[@]+"${opts[@]}"} \
		--copy "$work/case:/case" --out "/out:$work/$name.out" -- sh /case/case.sh "$@" \
		>"$work/$name.log" 2>"$work/$name.err" || rc=$?
	end=$(date +%s)
	took=$((end - t0))
}

# What a case left of its own, found by its marks (the cgroup and loop file named $tag, the processes sleeping $nap).
own_leftovers() {
	local n cg lo
	n="$(ps -eo args= | grep -cx "sleep $nap" || true)"
	cg="$(find /sys/fs/cgroup -name "$tag" -type d 2>/dev/null | tr '\n' ' ')"
	lo="$({ losetup -a 2>/dev/null || true; } | grep -F "$tag" | cut -d: -f1 | tr '\n' ' ')"
	[ "$n" = 0 ] || printf ' %s processes of the command;' "$n"
	[ -z "$cg" ] || printf ' cgroup %s;' "$cg"
	[ -z "$lo" ] || printf ' loop device %s;' "$lo"
}

# sweep BEFORE: removes what a failed case left of its own, so that the next case and the host start clean (it was
# reported already), then waits up to 15 s for the processes of its sandbox to be gone (as many as in BEFORE).
sweep() {
	local d l want
	while read -r d; do
		echo THAWED >"$d/freezer.state" 2>/dev/null || true
		echo 0 >"$d/cgroup.freeze" 2>/dev/null || true
	done < <(find /sys/fs/cgroup -name "$tag" -type d 2>/dev/null)
	pkill -9 -fx "sleep $nap" 2>/dev/null || true
	sleep 1
	find /sys/fs/cgroup -depth -name "$tag" -type d -exec rmdir {} \; 2>/dev/null || true
	{ losetup -a 2>/dev/null || true; } | grep -F "$tag" | cut -d: -f1 | while read -r l; do losetup -d "$l" 2>/dev/null || true; done || true
	want="$(grep '^sandbox processes:' "$1")"
	for _ in $(seq 1 15); do
		[ "$(host_snapshot | grep '^sandbox processes:')" != "$want" ] || break
		sleep 1
	done
	echo "      removed by this check; of the case's own state there is left:$(own_leftovers); $(host_snapshot | grep '^sandbox processes:') (before: ${want#*: })"
}

echo "== the comparison of the host sees what a run leaves behind (positive control)"
host_snapshot >"$work/plant.before"
mkdir "$BUILD_ROOT/tools-run.$tag"
unshare --user -- sleep "$nap" &
planted=$!
for _ in $(seq 1 40); do
	[ "$(readlink "/proc/$planted/ns/user" 2>/dev/null)" != "$(readlink /proc/self/ns/user)" ] && break
	sleep 0.05
done
if [ "$(cgroup_mindepth)" = 1 ]; then cgbase=/sys/fs/cgroup; else cgbase="$(dirname "$(find /sys/fs/cgroup -mindepth 2 -maxdepth 2 -name cgroup.procs | LC_ALL=C sort | head -n 1)")"; fi
mkdir "$cgbase/$tag"
dd if=/dev/zero of="$work/plant.img" bs=1024 count=1024 2>/dev/null
plant_loop="$(losetup -f --show "$work/plant.img")"
host_snapshot >"$work/plant.after"
prc=0
host_compare "$work/plant.before" "$work/plant.after" >"$work/plant.compare" || prc=$?
losetup -d "$plant_loop"
rmdir "$cgbase/$tag"
kill "$planted"
wait "$planted" 2>/dev/null || true
rmdir "$BUILD_ROOT/tools-run.$tag"
host_snapshot >"$work/plant.removed"
for kind in "sandbox directories" "sandbox processes" "cgroup directories" "loop devices"; do
	if grep -q "^FAIL  not as before: $kind:" "$work/plant.compare"; then
		ok "a planted leftover is seen: $kind ($(grep "^FAIL  not as before: $kind:" "$work/plant.compare" | cut -d: -f2- | sed 's/^ //'))"
	else
		bad "a planted leftover is not seen: $kind"
	fi
done
if [ "$prc" = 1 ]; then ok "host_compare exits 1 when something was left behind"; else bad "host_compare exited $prc with leftovers planted, not 1"; fi
if host_compare "$work/plant.before" "$work/plant.removed" >"$work/plant.compare2"; then
	ok "with the planted leftovers removed, the host compares as before"
else
	bad "with the planted leftovers removed the host still differs: $(grep '^FAIL' "$work/plant.compare2" | tr '\n' ';')"
fi

echo "== a command that ends before its deadline keeps its exit status"
for runner in dev-run tools-run; do
	opts=()
	[ "$runner" = dev-run ] || opts=(--privileged)
	for want in 3 124; do
		name="$runner-exit-$want"
		run_case "$name" 120 --deadline 60 ${opts[@]+"${opts[@]}"} -- exit "$tag" "$nap" "$want"
		if [ "$rc" = "$want" ] && ! grep -q "^$runner: deadline: " "$work/$name.err"; then
			ok "$runner: a command that exits $want at once (deadline 60 s): exit status $rc, not reported as stopped ($took s)"
		else
			bad "$runner: a command that exits $want at once (deadline 60 s): exit status $rc, $(grep -c "^$runner: deadline: " "$work/$name.err" || true) deadline lines: $(tail -n 1 "$work/$name.err")"
		fi
	done
done

echo "== a command still running at its deadline is stopped, and nothing of it is left"
# hang NAME RUNNER MODE [tools-run.sh options]
hang() {
	local name="$1" runner="$2" mode="$3" started after safety=120
	shift 3
	host_snapshot >"$work/$name.before"
	run_case "$name" "$safety" --deadline "$deadline" "$@" -- "$mode" "$tag" "$nap"
	started="$(cat "$work/$name.out/start" 2>/dev/null || true)"
	# the deadline
	if [ "$took" -ge "$safety" ]; then
		bad "$name: deadline: the runner was still running at the safety timeout of $safety s: the deadline did not stop the command"
	elif [ "$rc" != 124 ]; then
		bad "$name: deadline: exit status $rc, not 124: $(tail -n 1 "$work/$name.err")"
	elif ! grep -q "^$runner: deadline: " "$work/$name.err"; then
		bad "$name: deadline: exit status 124 but no '$runner: deadline' line on stderr"
	elif [ -z "$started" ]; then
		bad "$name: deadline: the command never started (no /out/start)"
	else
		after=$((end - started))
		if [ "$after" -lt "$deadline" ]; then
			bad "$name: deadline: stopped $after s after the command started, before its deadline of $deadline s"
		elif [ "$after" -gt $((deadline + grace + 20)) ]; then
			bad "$name: deadline: stopped only $after s after the command started (deadline $deadline s, SIGKILL $grace s later)"
		else
			ok "$name: deadline: stopped, exit 124 and '$runner: deadline: ...', $after s after the command started (deadline $deadline s, then SIGKILL $grace s later: the command ignored SIGTERM; the runner took $took s)"
		fi
	fi
	# what the case set up, so that 'nothing left' means something
	if [ -f "$work/$name.out/ready" ]; then
		ok "$name: the command reached its hang with a process outside its process group and SIGTERM ignored"
	else
		bad "$name: the command did not reach its hang (no /out/ready): $(tail -n 1 "$work/$name.err")"
	fi
	if [ "$mode" = hang-root ]; then
		if [ -s "$work/$name.out/frozen" ] && [ -s "$work/$name.out/loop" ]; then
			ok "$name: before it hung, the command froze a process in $(cat "$work/$name.out/frozen") and attached $(cut -d: -f1 "$work/$name.out/loop")"
		else
			bad "$name: the command did not freeze a process and attach a loop device as it must"
		fi
	fi
	# the cleanup
	host_snapshot >"$work/$name.after"
	local own=""
	own="$(own_leftovers)"
	if host_compare "$work/$name.before" "$work/$name.after" >"$work/$name.compare" && [ -z "$own" ]; then
		ok "$name: cleanup: nothing of the command is left (no process, cgroup directory, loop device, sandbox or mount; the host as before)"
	else
		bad "$name: cleanup: left behind:$own $(grep '^FAIL' "$work/$name.compare" | sed 's/^FAIL  not as before: //' | tr '\n' ';')"
		sweep "$work/$name.before"
	fi
}
hang dev-run-hang dev-run hang
hang tools-run-hang tools-run hang-root --privileged

echo "== a session of check-resctl.sh stopped at its deadline"
host_snapshot >"$work/session.before"
rc=0
t0=$(date +%s)
timeout --kill-after=10 900 "$ALPINE_BUILD_DIR/check-resctl.sh" --deadline "$deadline" --no-guest --areas layout >"$work/session.log" 2>&1 || rc=$?
took=$(($(date +%s) - t0))
if [ "$took" -ge 900 ]; then
	bad "check-resctl.sh --deadline $deadline: still running at the safety timeout of 900 s"
elif [ "$rc" != 1 ]; then
	bad "check-resctl.sh --deadline $deadline: exit status $rc, not 1: $(tail -n 1 "$work/session.log")"
elif grep -qx "FAIL  the unprivileged session was stopped at its deadline of $deadline s (what it left behind is checked below)" "$work/session.log"; then
	ok "check-resctl.sh --deadline $deadline: its unprivileged session is reported as stopped at its deadline, the run fails (exit 1 after $took s)"
else
	bad "check-resctl.sh --deadline $deadline: the stopped session is not reported as such: $(grep -m 1 'unprivileged session' "$work/session.log")"
fi
if grep -q '^== host state after the sessions' "$work/session.log" && ! grep -q '^FAIL  not as before' "$work/session.log" &&
	grep -q '^PASS  unchanged: vendor/alpine-aports' "$work/session.log"; then
	ok "check-resctl.sh --deadline $deadline: after the stopped session it checked the host and the vendored tree: unchanged"
else
	bad "check-resctl.sh --deadline $deadline: no clean check of the host after the stopped session: $(grep -E '^(FAIL|== host)' "$work/session.log" | tr '\n' ';')"
fi
host_snapshot >"$work/session.after"
if host_compare "$work/session.before" "$work/session.after" >"$work/session.compare"; then
	ok "check-resctl.sh --deadline $deadline: cleanup: the host as before"
else
	bad "check-resctl.sh --deadline $deadline: cleanup: left behind: $(grep '^FAIL' "$work/session.compare" | sed 's/^FAIL  not as before: //' | tr '\n' ';')"
fi

echo "== the host after all cases"
host_snapshot >"$work/host.end"
host_compare "$work/host.start" "$work/host.end" || fail=1
if msg="$(vendor_pristine)"; then ok "unchanged: vendor/alpine-aports"; else bad "vendor/alpine-aports: $msg"; fi
exit "$fail"
}
