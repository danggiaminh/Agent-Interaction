#!/bin/sh
# Runs inside the tools image as root of an unprivileged user namespace (check-resctl.sh -> tools-run.sh), offline:
# no cgroup hierarchy is mounted there and none can be. Builds and tests the resctl crate (/crate), then shows what
# resctl does on a machine where it cannot control anything: it says so, refuses the commands that need a hierarchy
# with the "unsupported" code, and creates nothing. Prints one KEY=VALUE line per fact (u_*); check-resctl.sh judges
# them against checks.tsv. Nothing here fails the script: a failed step is reported as its own value.
#   unpriv.sh [section...]     default: every section (useful for iterating: unpriv.sh layout isolation)
cd /work || exit 1
. /work/common.sh

export CARGO_TARGET_DIR=/tmp/target
RC="$CARGO_TARGET_DIR/release/resctl"

# ensure_resctl: the release binary, built once per session (sections may run alone).
ensure_resctl() {
	[ -x "$RC" ] && return 0
	(cd /crate && cargo build --release --offline --locked) >"$T/ensure_resctl.log" 2>&1
}

# ------------------------------------------------------------------------------------------------ build
if section build; then
	cd /crate || exit 1
	run u_fmt cargo fmt --check
	run u_clippy cargo clippy --offline --locked --all-targets -- -D warnings
	if cargo test --offline --locked >"$T/u_unit.log" 2>&1; then
		n="$(sed -n 's/^test result: ok\. \([0-9][0-9]*\) passed; 0 failed.*/\1/p' "$T/u_unit.log" | awk '{ s += $1 } END { print s + 0 }')"
		kv u_unit "ok $n tests"
	else kv u_unit "fail:$(grep -E 'FAILED|panicked|^error' "$T/u_unit.log" | head -n 1 | cut -c1-200)"; fi
	if cargo build --release --offline --locked >"$T/u_release.log" 2>&1 && [ -x "$RC" ]; then
		mkdir -p /out
		cp "$RC" /out/resctl
		kv u_release "ok $(stat -c %s "$RC") bytes"
	else kv u_release "fail:$(tail -n 1 "$T/u_release.log" | cut -c1-200)"; fi
	pk="$(grep -c '^name = ' Cargo.lock)"
	deps="$(grep -c '^dependencies' Cargo.lock)"
	if [ "$pk" = 1 ] && [ "$deps" = 0 ]; then kv u_deps "ok the lock file lists resctl alone: no dependency to fetch or audit"
	else kv u_deps "fail:Cargo.lock lists $pk packages and $deps dependency lists"; fi
	cd /work || exit 1
fi

# ------------------------------------------------------------------------------------------------ layout
if section layout; then
	ensure_resctl
	"$RC" probe >"$T/probe.out" 2>"$T/probe.err"
	prc=$?
	mode="$(sed -n 's/^layout=//p' "$T/probe.out")"
	nfeat="$(grep -c '^feature\.' "$T/probe.out")"
	nbound="$(grep '^feature\.' "$T/probe.out" | grep -vc '=unbound')"
	if [ "$prc" = 0 ] && [ "$mode" = none ] && [ "$nfeat" -ge 9 ] && [ "$nbound" = 0 ]; then
		kv u_layout "ok layout=none: all $nfeat features unbound, probe changed nothing (rc 0)"
	else kv u_layout "fail:probe rc=$prc layout=${mode:-?} features=$nfeat bound=$nbound"; fi
	if [ -d /sys/fs/cgroup ] && [ -n "$(ls /sys/fs/cgroup 2>/dev/null)" ]; then kv u_no_hierarchy "fail:/sys/fs/cgroup is populated in the unprivileged session"
	else kv u_no_hierarchy "ok no cgroup hierarchy is mounted in the unprivileged session"; fi
fi

# ------------------------------------------------------------------------------------------------ normal
if section normal; then
	ensure_resctl
	"$RC" policy >"$T/policy.out" 2>"$T/policy.err"
	prc=$?
	have=0
	for k in system.cpu.weight system.memory.reserve workload.memory.max workload.pids.max class.interactive.cpu.weight \
		class.batch.memory.max limits.batch.pids.max stop.grace_ms recover.orphans; do
		grep -q "^$k=" "$T/policy.out" && have=$((have + 1))
	done
	total="$(sed -n 's/^memtotal=//p' "$T/policy.out")"
	if [ "$prc" = 0 ] && [ "$have" = 9 ] && [ -n "$total" ]; then
		kv u_policy "ok the built-in policy binds to this machine (memtotal=$total): 9 of 9 settings printed"
	else kv u_policy "fail:policy rc=$prc settings=$have/9 memtotal=${total:-?}"; fi
	"$RC" --policy /work/test.policy policy >"$T/policy2.out" 2>&1
	frc=$?
	if [ "$frc" = 0 ] && grep -qx 'workload.memory.max=536870912' "$T/policy2.out" && grep -qx 'stop.grace_ms=2000' "$T/policy2.out"; then
		kv u_policy_file "ok --policy FILE replaces the built-in policy (workload.memory.max=512M as 536870912, stop.grace_ms=2000)"
	else kv u_policy_file "fail:$(tail -n 1 "$T/policy2.out" | cut -c1-200)"; fi
	"$RC" help >"$T/help.out" 2>&1
	hrc=$?
	if [ "$hrc" = 0 ] && [ "$(grep -cE '^  (probe|policy|init|create|run|enter|freeze|stop|list|status|recover|teardown)[ |]' "$T/help.out")" -ge 12 ]; then
		kv u_help "ok help lists every command and exits 0"
	else kv u_help "fail:help rc=$hrc $(tail -n 1 "$T/help.out" | cut -c1-100)"; fi
fi

# ------------------------------------------------------------------------------------------------ isolation
if section isolation; then
	ensure_resctl
	# a bad policy never reaches the machine: refused before anything is touched
	cat >"$T/bad.policy" <<'EOF'
[workload]
memory.max = 64M
pids.max = 256

[class.batch]
memory.max = 128M
pids.max = 128

[limits.batch]
memory.max = 32M
pids.max = 16
EOF
	"$RC" --policy "$T/bad.policy" init >"$T/bad.out" 2>"$T/bad.err"
	brc=$?
	if [ "$brc" = 2 ] && grep -q 'class.batch' "$T/bad.err"; then
		kv u_policy_refused "ok a class ceiling above the workload ceiling is refused with 2: $(head -n 1 "$T/bad.err" | cut -c1-140)"
	else kv u_policy_refused "fail:rc=$brc $(head -n 1 "$T/bad.err" | cut -c1-140)"; fi
	# the commands that need a hierarchy are denied with 3 and leave no trace
	rm -f "$T/marker"
	for c in init run enter; do
		case "$c" in
		init) "$RC" --policy /work/test.policy init >"$T/d_$c.out" 2>"$T/d_$c.err" ;;
		run) "$RC" --policy /work/test.policy run batch/u1 -- touch "$T/marker" >"$T/d_$c.out" 2>"$T/d_$c.err" ;;
		enter) "$RC" --policy /work/test.policy enter system -- touch "$T/marker" >"$T/d_$c.out" 2>"$T/d_$c.err" ;;
		esac
		drc=$?
		if [ "$drc" = 3 ] && [ ! -e "$T/marker" ]; then
			kv "u_${c}_denied" "denied:$(head -n 1 "$T/d_$c.err" | cut -c1-160)"
		else kv "u_${c}_denied" "fail:rc=$drc marker=$([ -e "$T/marker" ] && echo present || echo absent) $(head -n 1 "$T/d_$c.err" | cut -c1-120)"; fi
	done
	# the domains of the system are not addressable at all, with or without a hierarchy
	bad=
	for a in "kill system" "stop workload" "freeze system/x" "remove workload/x" "run system/x -- true" "run workload/x -- true"; do
		# shellcheck disable=SC2086
		"$RC" --policy /work/test.policy $a >/dev/null 2>&1
		arc=$?
		[ "$arc" = 2 ] || bad="$bad '$a'=rc$arc"
	done
	if [ -z "$bad" ]; then kv u_system_unreachable "ok no command can name system or workload as a workload (rc 2 each)"
	else kv u_system_unreachable "fail:$bad"; fi
fi
