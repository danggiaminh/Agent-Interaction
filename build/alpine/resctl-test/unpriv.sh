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
	# bounded: a test that never ends fails the fact instead of holding the session (libtest names it after 60 s)
	timeout 900 cargo test --offline --locked >"$T/u_unit.log" 2>&1
	urc=$?
	if [ "$urc" = 0 ]; then
		n="$(sed -n 's/^test result: ok\. \([0-9][0-9]*\) passed; 0 failed.*/\1/p' "$T/u_unit.log" | awk '{ s += $1 } END { print s + 0 }')"
		kv u_unit "ok $n tests"
	elif [ "$urc" = 124 ] || [ "$urc" = 143 ]; then
		kv u_unit "fail:the unit tests did not end within 900 s: $(grep -m 1 'has been running for over' "$T/u_unit.log" | cut -c1-160)"
	else kv u_unit "fail:$(grep -E 'FAILED|panicked|^error' "$T/u_unit.log" | head -n 1 | cut -c1-200)"; fi
	if cargo build --release --offline --locked >"$T/u_release.log" 2>&1 && [ -x "$RC" ]; then
		mkdir -p /out
		cp "$RC" /out/resctl
		kv u_release "ok $(stat -c %s "$RC") bytes"
	else kv u_release "fail:$(tail -n 1 "$T/u_release.log" | cut -c1-200)"; fi
	# no unsafe code: both crate roots forbid it (the compiler refuses the build otherwise) and no line of the sources, a
	# comment apart, uses the keyword
	nforbid="$(grep -l '^#!\[forbid(unsafe_code)\]' src/lib.rs src/main.rs | wc -l | tr -d ' ')"
	nuse="$(grep -rnwE 'unsafe' src | grep -vE '^[^:]*:[0-9]+:[[:space:]]*//|#!\[forbid\(unsafe_code\)\]' | wc -l | tr -d ' ')"
	nsrc="$(find src -name '*.rs' | wc -l | tr -d ' ')"
	if [ "$nforbid" = 2 ] && [ "$nuse" = 0 ] && [ "$nsrc" -ge 9 ]; then
		kv u_no_unsafe "ok both crate roots forbid unsafe_code and none of the $nsrc source files uses the keyword"
	else kv u_no_unsafe "fail:crate roots forbidding unsafe=$nforbid/2, lines using unsafe=$nuse, sources=$nsrc"; fi
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
		class.batch.memory.max limits.batch.pids.max stop.grace_ms recover.orphans restart.mode restart.max restart.window_ms \
		restart.total restart.backoff_ms restart.backoff_max_ms restart.rate supervise.workloads run.allow_root; do
		grep -q "^$k=" "$T/policy.out" && have=$((have + 1))
	done
	total="$(sed -n 's/^memtotal=//p' "$T/policy.out")"
	if [ "$prc" = 0 ] && [ "$have" = 18 ] && [ -n "$total" ]; then
		kv u_policy "ok the built-in policy binds to this machine (memtotal=$total): 18 of 18 settings printed"
	else kv u_policy "fail:policy rc=$prc settings=$have/18 memtotal=${total:-?}"; fi
	"$RC" --policy /work/test.policy policy >"$T/policy2.out" 2>&1
	frc=$?
	if [ "$frc" = 0 ] && grep -qx 'workload.memory.max=536870912' "$T/policy2.out" && grep -qx 'stop.grace_ms=2000' "$T/policy2.out" \
		&& grep -qx 'restart.max=3' "$T/policy2.out" && grep -qx 'supervise.workloads=8' "$T/policy2.out"; then
		kv u_policy_file "ok --policy FILE replaces the built-in policy (workload.memory.max=512M as 536870912, stop.grace_ms=2000, restart.max=3, supervise.workloads=8)"
	else kv u_policy_file "fail:$(tail -n 1 "$T/policy2.out" | cut -c1-200)"; fi
	"$RC" help >"$T/help.out" 2>&1
	hrc=$?
	if [ "$hrc" = 0 ] && [ "$(grep -cE '^  (probe|policy|init|create|run|supervise|enter|freeze|stop|list|status|state|audit|protect|unprotect|critical|reset|recover|teardown)[ |]' "$T/help.out")" -ge 19 ]; then
		kv u_help "ok help lists every command and exits 0"
	else kv u_help "fail:help rc=$hrc $(tail -n 1 "$T/help.out" | cut -c1-100)"; fi
	# the restart policy is bounded by caps that a file cannot widen: a value above a cap or below a floor is refused with 2
	# (nothing is bound), the values at the caps and the floors are accepted and bound as written
	rbad=
	nref=0
	for e in 's/^max = 3$/max = 11/' 's/^max = 3$/max = 0/' 's/^total = 10$/total = 101/' 's/^total = 10$/total = 0/' \
		's/^rate = 120$/rate = 121/' 's/^rate = 120$/rate = 0/' 's/^window = 60s$/window = 999ms/' \
		's/^backoff = 100ms$/backoff = 9ms/' 's/^backoff.max = 400ms$/backoff.max = 50ms/' \
		's/^workloads = 8$/workloads = 65/' 's/^workloads = 8$/workloads = 0/' 's/^mode = on-failure$/mode = forever/'; do
		sed "$e" /work/test.policy >"$T/rb.policy"
		"$RC" --policy "$T/rb.policy" policy >"$T/rb.out" 2>"$T/rb.err"
		rrc=$?
		nref=$((nref + 1))
		if [ "$rrc" != 2 ] || [ -s "$T/rb.out" ] || ! grep -q '^resctl: policy: ' "$T/rb.err"; then rbad="$rbad [$e]=rc$rrc"; fi
	done
	nacc=0
	for pair in 's/^max = 3$/max = 10/|restart.max=10' 's/^total = 10$/total = 100/|restart.total=100' \
		's/^window = 60s$/window = 1s/|restart.window_ms=1000' 's/^workloads = 8$/workloads = 64/|supervise.workloads=64' \
		's/^mode = on-failure$/mode = always/|restart.mode=always' 's/^mode = on-failure$/mode = never/|restart.mode=never'; do
		sed "${pair%|*}" /work/test.policy >"$T/rb.policy"
		"$RC" --policy "$T/rb.policy" policy >"$T/rb.out" 2>"$T/rb.err"
		rrc=$?
		nacc=$((nacc + 1))
		if [ "$rrc" != 0 ] || ! grep -qx "${pair#*|}" "$T/rb.out"; then rbad="$rbad [${pair%|*}]=rc$rrc"; fi
	done
	if [ -z "$rbad" ]; then
		kv u_restart_bounds "ok $nref values above a cap or below a floor are refused with 2 and bind nothing; the $nacc values at a cap or a floor are accepted"
	else kv u_restart_bounds "fail:$rbad"; fi
	# who a supervised workload runs as: the built-in policy drops to an unprivileged user and never to root; a file cannot
	# name root without saying so, nor change only the user
	rbad=
	grep -qx 'run.uid=65534' "$T/policy.out" && grep -qx 'run.gid=65534' "$T/policy.out" && grep -qx 'run.allow_root=no' "$T/policy.out" \
		|| rbad="$rbad built-in:$(grep '^run\.' "$T/policy.out" | tr '\n' ' ')"
	for e in 's/^allow.root = yes$/allow.root = yes\nuid = 65534/' 's/^allow.root = yes$/allow.root = no\nuid = 0\ngid = 0/' \
		's/^allow.root = yes$/allow.root = yes\ngid = 65534/'; do
		sed "$e" /work/test.policy >"$T/rb.policy"
		"$RC" --policy "$T/rb.policy" policy >"$T/rb.out" 2>"$T/rb.err"
		rrc=$?
		if [ "$rrc" != 2 ] || [ -s "$T/rb.out" ] || ! grep -q '^resctl: policy: \[run\] ' "$T/rb.err"; then rbad="$rbad [$e]=rc$rrc"; fi
	done
	sed 's/^allow.root = yes$/allow.root = no\nuid = 65534\ngid = 65534/' /work/test.policy >"$T/rb.policy"
	"$RC" --policy "$T/rb.policy" policy >"$T/rb.out" 2>"$T/rb.err"
	rrc=$?
	if [ "$rrc" != 0 ] || ! grep -qx 'run.uid=65534' "$T/rb.out" || ! grep -qx 'run.allow_root=no' "$T/rb.out"; then rbad="$rbad [nobody]=rc$rrc"; fi
	if [ -z "$rbad" ]; then
		kv u_run_policy "ok the built-in policy runs workloads as 65534:65534 and never as root; a uid alone, a gid alone and root without allow.root are refused with 2"
	else kv u_run_policy "fail:$rbad"; fi
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
	# the commands that need a hierarchy are denied with 3 and leave no trace: no workload, no marker, no state directory
	rm -f "$T/marker"
	rm -rf "$T/state.u"
	for c in init run supervise enter protect; do
		case "$c" in
		init) "$RC" --policy /work/test.policy --state-dir "$T/state.u" init >"$T/d_$c.out" 2>"$T/d_$c.err" ;;
		run) "$RC" --policy /work/test.policy --state-dir "$T/state.u" run batch/u1 -- touch "$T/marker" >"$T/d_$c.out" 2>"$T/d_$c.err" ;;
		supervise) "$RC" --policy /work/test.policy --state-dir "$T/state.u" supervise batch/u1 --restart never -- touch "$T/marker" >"$T/d_$c.out" 2>"$T/d_$c.err" ;;
		enter) "$RC" --policy /work/test.policy --state-dir "$T/state.u" enter system -- touch "$T/marker" >"$T/d_$c.out" 2>"$T/d_$c.err" ;;
		protect) "$RC" --policy /work/test.policy --state-dir "$T/state.u" protect 1234 >"$T/d_$c.out" 2>"$T/d_$c.err" ;;
		esac
		drc=$?
		if [ "$drc" = 3 ] && [ ! -e "$T/marker" ] && [ ! -e "$T/state.u" ]; then
			kv "u_${c}_denied" "denied:$(head -n 1 "$T/d_$c.err" | cut -c1-160)"
		else kv "u_${c}_denied" "fail:rc=$drc marker=$([ -e "$T/marker" ] && echo present || echo absent) state=$([ -e "$T/state.u" ] && echo present || echo absent) $(head -n 1 "$T/d_$c.err" | cut -c1-120)"; fi
	done
	bad=
	nden=0
	for a in "create batch/u2" "stop batch/u2" "kill batch/u2" "freeze batch/u2" "thaw batch/u2" "remove batch/u2" "unprotect 1234" \
		"reset batch/u2" "recover" "teardown"; do
		# shellcheck disable=SC2086
		"$RC" --policy /work/test.policy --state-dir "$T/state.u" $a >/dev/null 2>"$T/m.err"
		mrc=$?
		nden=$((nden + 1))
		[ "$mrc" = 3 ] && [ "$(head -n 1 "$T/m.err")" = "resctl: unsupported: ${a%% *}: this machine has no cgroup hierarchy to hold it" ] \
			|| bad="$bad '$a'=rc$mrc"
	done
	[ ! -e "$T/state.u" ] || bad="$bad state-directory-created"
	if [ -z "$bad" ]; then
		kv u_mutating_denied "ok $nden more commands that change a workload, a domain or the registry are refused with 3, naming the command; no state directory was created"
	else kv u_mutating_denied "fail:$bad"; fi
	# the commands that only read work without a hierarchy and create nothing
	bad=
	for a in list state audit "audit 5" critical; do
		# shellcheck disable=SC2086
		"$RC" --policy /work/test.policy --state-dir "$T/state.u" $a >"$T/r.out" 2>"$T/r.err"
		rrc=$?
		if [ "$rrc" != 0 ] || [ -s "$T/r.out" ] || [ -s "$T/r.err" ]; then bad="$bad '$a'=rc$rrc"; fi
	done
	[ ! -e "$T/state.u" ] || bad="$bad state-directory-created"
	if [ -z "$bad" ]; then kv u_read_only "ok list, state, audit and critical report nothing with 0 and create no state directory"
	else kv u_read_only "fail:$bad"; fi
	# a policy cannot grant protected status, wherever it names it: each variant is refused with 2 and says why, binds nothing
	bad=
	npol=0
	for v in protect protected critical class system workload; do
		case "$v" in
		protect) { cat /work/test.policy; printf '\n[protect]\npid = 1\n'; } >"$T/pp.policy" ;;
		protected) { cat /work/test.policy; printf '\n[protected]\npid = 1\n'; } >"$T/pp.policy" ;;
		critical) { cat /work/test.policy; printf '\n[critical.workloads]\nname = x\n'; } >"$T/pp.policy" ;;
		class) awk '{ print } /^\[class\.batch\]$/ { print "protected = yes" }' /work/test.policy >"$T/pp.policy" ;;
		system) awk '{ print } /^\[system\]$/ { print "critical = yes" }' /work/test.policy >"$T/pp.policy" ;;
		workload) awk '{ print } /^\[workload\]$/ { print "protect = yes" }' /work/test.policy >"$T/pp.policy" ;;
		esac
		"$RC" --policy "$T/pp.policy" policy >"$T/pp.out" 2>"$T/pp.err"
		prc=$?
		npol=$((npol + 1))
		if [ "$prc" != 2 ] || [ -s "$T/pp.out" ] || ! grep -q 'a policy cannot grant protected status' "$T/pp.err"; then bad="$bad $v:rc=$prc"; fi
	done
	"$RC" --policy /work/test.policy policy >/dev/null 2>&1 || bad="$bad control-policy-refused"
	if [ -z "$bad" ]; then
		kv u_policy_protection "ok $npol policies that name protected status in a section or in a key are refused with 2 and bind nothing; the policy without them is accepted"
	else kv u_policy_protection "fail:$bad"; fi
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
