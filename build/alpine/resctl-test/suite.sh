# The resource-control suite: drives the resctl binary (runtime/resctl) as black box on whatever cgroup layout the
# machine has, and prints one KEY=VALUE fact per behaviour. Sourced after common.sh and cgroup.sh by priv.sh (the
# cloud host, cgroup v1 limits with v2 freeze/kill/pressure) and guest.sh (the test bed guest, cgroup v2 only).
#
#   rc_suite PREFIX LIFECYCLE [SECTION...]
#
# LIFECYCLE is resctl's --lifecycle (auto | v1 | v2). Sections: layout normal limits throttle pressure isolation
# terminate recover process critical lifecycle (all of them when none is named). Every fact is "ok <evidence>",
# "fail:<what did not behave>", "unsupported:<why this layout cannot do it>" or "denied:<the refusal>"; unsupported is
# never a pass, and it carries a measured reason (an `unbound=` line of resctl, or the feature table of `resctl probe`).
#
# Needs: RC_DIR (resctl, the *.policy files), CT (the static ctr-test binary), CG_BLKDEV (a whole block device).
# Nothing here names a cgroup path: the hierarchy of each feature is read from `resctl probe`. Tolerances are wide
# enough for software emulation (TCG), where only the order of magnitude of timings is meaningful.
RC_DIR="${RC_DIR:-/work}"
RC_BIN="${RC_BIN:-$RC_DIR/resctl}"
RC_POLICY="$RC_DIR/test.policy"
RC_LC=auto

# rc ARGS...: resctl under the suite's policy and lifecycle. stdout in $T/rc.out, stderr in $T/rc.err, status in RS.
rc() {
	"$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" --state-dir "$RC_STATE" "$@" >"$T/rc.out" 2>"$T/rc.err"
	RS=$?
}
# rcx ARGS...: the same, attached to the caller's stdout and stderr (for `run` in the foreground).
rcx() { "$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" --state-dir "$RC_STATE" "$@"; }
# rcbg ARGS...: only ever as `rcbg ... &`: the background job execs resctl, which execs the command, so that $! is the
# process of the workload itself and not of a shell that waits for it.
rcbg() { exec "$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" --state-dir "$RC_STATE" "$@"; }
# rcerr: what resctl said on stderr, one line.
rcerr() { tail -n 1 "$T/rc.err" | cut -c1-200; }
# rcl LIFECYCLE ARGS...: one command under another lifecycle.
rcl() {
	l="$1"
	shift
	"$RC_BIN" --policy "$RC_POLICY" --lifecycle "$l" --state-dir "$RC_STATE" "$@" >"$T/rc.out" 2>"$T/rc.err"
	RS=$?
}
# rc_pol POLICYFILE ARGS...: one command under another policy.
rc_pol() {
	pf="$1"
	shift
	"$RC_BIN" --policy "$pf" --lifecycle "$RC_LC" --state-dir "$RC_STATE" "$@" >"$T/rc.out" 2>"$T/rc.err"
	RS=$?
}

f() { kv "$RC_P$1" "$2"; }

# ---- what `resctl probe` says about the layout: the hierarchy that serves each feature
rc_feat() { sed -n "s/^feature\.$1=//p" "$T/probe.$RC_P"; }
rc_api() { rc_feat "$1" | sed 's/:.*//'; }
rc_root() { rc_feat "$1" | sed -n 's/^v[12]://p'; }
# rc_dir FEATURE REL: the directory of the domain agent-interaction/REL in the hierarchy that serves the feature.
rc_dir() {
	r="$(rc_root "$1")"
	[ -n "$r" ] && echo "$r/agent-interaction/$2"
}
rc_roots() { sed -n 's/^feature\.[a-z.]*=v[12]://p' "$T/probe.$RC_P" | sort -u; }
rc_bound() { case "$(rc_feat "$1")" in v[12]:*) return 0 ;; esac; return 1; }

# ---- the state of a workload, from resctl
rc_val() {
	rc status "$1"
	sed -n "s/^$2=//p" "$T/rc.out"
}
# rc_mem_hit DOMAIN LIMIT_BYTES: did the memory limit of the domain bind? Prints "hit ..." or "not ..." with the evidence.
# Where the machine counts the hits (v2 memory.events max; v1 failcnt while swap is not pinned to the limit) that counter
# decides; where it does not (v1 with memory+swap pinned to the limit: the kernel counts the failure nowhere, resctl
# leaves the key out) the peak decides: usage that reached the limit (within 1 MiB) is a bind, usage below it is not.
rc_mem_hit() {
	_ev="$(rc_val "$1" memory.max_events)"
	_pk="$(rc_val "$1" memory.peak)"
	if [ -n "$_ev" ]; then
		if [ "$_ev" -ge 1 ]; then echo "hit counter=$_ev peak=${_pk:-?}"; else echo "not counter=0 peak=${_pk:-?}"; fi
	elif [ "${_pk:-0}" -ge $(($2 - 1048576)) ]; then echo "hit peak=${_pk} of $2, no counter"
	else echo "not peak=${_pk:-?} of $2, no counter"; fi
}
rc_procs() { rc_val "$1" procs; }
rc_empty() { [ "$(rc_procs "$1")" = 0 ]; }
# rc_has WORKLOAD PROCS STATE: `resctl list` shows exactly that line.
rc_has() {
	rc list
	grep -q "^$1 procs=$2 state=$3\$" "$T/rc.out"
}
# rc_gone WORKLOAD: `resctl list` does not show the workload at all.
rc_gone() {
	rc list
	! grep -q "^$1 " "$T/rc.out"
}
# rc_drop WORKLOAD [PID]: remove the workload with everything in it, and reap the background job that ran in it.
rc_drop() {
	rc remove "$1"
	[ -z "${2:-}" ] || wait "$2" 2>/dev/null
}

# ---- processes
# rc_state PID: the state letter of the process, "gone" when there is none.
rc_state() {
	s="$(sed -n 's/^[0-9]* (.*) \([A-Za-z]\) .*/\1/p' "/proc/$1/stat" 2>/dev/null)"
	echo "${s:-gone}"
}
rc_alive() {
	case "$(rc_state "$1")" in gone | Z | X) return 1 ;; esac
	return 0
}
# rc_ticks PID: user plus system CPU time in clock ticks.
rc_ticks() { sed 's/^.*) //' "/proc/$1/stat" 2>/dev/null | awk '{ print $12 + $13 }'; }
# rc_find PREFIX: the number of live processes whose command line begins with PREFIX (one word per argument).
rc_find() {
	n=0
	for cf in /proc/[0-9]*/cmdline; do
		c="$(tr '\0' ' ' <"$cf" 2>/dev/null)"
		case "$c" in "$1"*) n=$((n + 1)) ;; esac
	done
	echo "$n"
}
rc_secs() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", b - a }'; }
# rc_weight REL: the CPU weight of a domain, read from the file of the hierarchy that serves it (cgroup v1 shares are
# weight * 10.24).
rc_weight() {
	d="$(rc_dir cpu.weight "$1")"
	case "$(rc_api cpu.weight)" in
	v2) cat "$d/cpu.weight" 2>/dev/null ;;
	v1) awk '{ printf "%d", $1 / 10.24 + 0.5 }' "$d/cpu.shares" 2>/dev/null ;;
	esac
}

# ---- the system canary: a process in the system domain that nothing resctl does to a workload may touch
canary_ready() { [ "$(rc_procs system)" = "$((1 + ${CANARY_EXTRA:-0}))" ]; }
canary_start() {
	rcbg enter system -- sleep 600 >/dev/null 2>&1 &
	CANARY=$!
	waitfor 10 canary_ready
}
canary_stop() {
	[ -z "${CANARY:-}" ] || kill -9 "$CANARY" 2>/dev/null
	wait "$CANARY" 2>/dev/null
	CANARY=
}
# canary_check: succeeds while the canary runs in the system domain, of every serving hierarchy. CANARY_WHY says why not.
canary_check() {
	CANARY_WHY=
	if ! rc_alive "$CANARY"; then CANARY_WHY="the canary is $(rc_state "$CANARY")"; return 1; fi
	rc status system
	if ! grep -q "^procs=$((1 + ${CANARY_EXTRA:-0}))\$" "$T/rc.out" || ! grep -q '^state=running$' "$T/rc.out"; then
		CANARY_WHY="status system: $(grep -E '^(procs|state)=' "$T/rc.out" | tr '\n' ' ')"
		return 1
	fi
	want_n="$(rc_roots | wc -l)"
	have_n="$(grep -c '/agent-interaction/system$' "/proc/$CANARY/cgroup")"
	if [ "$have_n" -ne "$want_n" ]; then CANARY_WHY="the canary is in the system domain of $have_n of $want_n hierarchies"; return 1; fi
	return 0
}
# canary_note LABEL: remember a failed check, with the step that was running.
canary_note() { canary_check || CANARY_BAD="$CANARY_BAD $1($CANARY_WHY)"; }
# canary_report FACT: the verdict over everything since the last report.
canary_report() {
	canary_note "$1"
	if [ -z "$CANARY_BAD" ]; then f "$1" "ok the canary in agent-interaction/system kept running (pid $CANARY)"
	else f "$1" "fail:$CANARY_BAD"; fi
	CANARY_BAD=
}

# rc_domain_dirs: how many agent-interaction directories exist in the hierarchies that serve a feature.
rc_domain_dirs() {
	n=0
	for r in $(rc_roots); do
		[ ! -d "$r/agent-interaction" ] || n=$((n + $(find "$r/agent-interaction" -type d | wc -l)))
	done
	echo "$n"
}

rc_suite() {
	RC_P="$1"
	RC_LC="$2"
	shift 2
	SECTIONS="$*"
	RC_POLICY="$RC_DIR/test.policy"
	RC_STATE="$T/state.$RC_P"
	CANARY=
	CANARY_BAD=
	CANARY_EXTRA=0
	cg_detect
	mkdir -p "$T"
	if ! "$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" probe >"$T/probe.$RC_P" 2>"$T/probe.$RC_P.err"; then
		f layout "fail:resctl probe: $(tail -n 1 "$T/probe.$RC_P.err" | cut -c1-200)"
		return 0
	fi
	# the prologue: start from nothing, so that the first init is a real one
	rc teardown
	pre="$(rc_domain_dirs)"
	rc init
	INIT_RS=$RS
	cp "$T/rc.out" "$T/init1.$RC_P"
	cp "$T/rc.err" "$T/init1.$RC_P.err"
	INIT_PRE=$pre
	canary_start

	rc_s_layout
	rc_s_normal
	rc_s_limits
	rc_s_throttle
	rc_s_pressure
	rc_s_isolation
	rc_s_terminate
	rc_s_recover
	rc_s_process
	rc_s_critical
	rc_s_stress
	rc_s_lifecycle

	# the epilogue: nothing of this suite stays behind
	canary_stop
	rc teardown
	left="$(rc_domain_dirs)"
	if [ "$RS" = 0 ] && [ "$left" = 0 ]; then f leftovers "ok no agent-interaction directory is left in any hierarchy"
	else f leftovers "fail:teardown rc=$RS, $left agent-interaction directories left: $(rcerr)"; fi
}

# ------------------------------------------------------------------------------------------------ layout
rc_s_layout() {
	section layout || return 0
	mode="$(sed -n 's/^layout=//p' "$T/probe.$RC_P")"
	if [ "$mode" = "$CG_MODE" ]; then f layout "ok resctl sees a $mode layout, the mounts say $CG_MODE"
	else f layout "fail:resctl says layout=$mode, the mounts say $CG_MODE"; fi
	# the layout as the mounts and the controller lists say it, independent of resctl: the measured cause of a limit
	cg_layout_facts "$RC_P"
	missing=
	for ft in memory pids cpu.quota cpu.weight cpuset io freeze kill pressure; do
		name="feature_$(echo "$ft" | tr . _)"
		v="$(rc_feat "$ft")"
		f "$name" "${v:-fail:no such line in the probe}"
		case "$v" in
		v[12]:*) ;;
		unbound:*) [ "$ft $RC_LC" = "pressure v1" ] || missing="$missing $ft" ;;
		*) missing="$missing $ft" ;;
		esac
	done
	if [ -z "$missing" ]; then f layout_bound "ok every feature is served by a hierarchy (lifecycle $RC_LC)"
	else f layout_bound "fail:not served by any hierarchy:$missing"; fi
	# swap accounting: the file that can deny swap exists in the memory hierarchy
	d="$(rc_dir memory workload)"
	case "$(rc_api memory)" in
	v2) [ -e "$d/memory.swap.max" ] && sw="yes memory.swap.max" || sw="no memory.swap.max in the v2 memory hierarchy" ;;
	v1) [ -e "$d/memory.memsw.limit_in_bytes" ] && sw="yes memory.memsw.limit_in_bytes" || sw="no memory.memsw.limit_in_bytes in the v1 memory hierarchy (swapaccount=0 or no swap support)" ;;
	*) sw="no memory hierarchy" ;;
	esac
	f swap_accounting "$sw"
}

# ------------------------------------------------------------------------------------------------ normal operation
rc_s_normal() {
	section normal || return 0
	# init: the domains exist in every serving hierarchy and a second init changes nothing
	rc init
	rs2=$RS
	bad=
	[ "$INIT_RS" = 0 ] || bad="$bad first-init-rc=$INIT_RS"
	[ "$rs2" = 0 ] || bad="$bad second-init-rc=$rs2"
	cmp -s "$T/rc.out" "$T/init1.$RC_P" || bad="$bad second-init-output-differs"
	[ "$INIT_PRE" = 0 ] || bad="$bad before-init-$INIT_PRE-directories-existed"
	for r in $(rc_roots); do
		for dm in system workload workload/interactive workload/batch; do
			[ -d "$r/agent-interaction/$dm" ] || bad="$bad missing:$r/agent-interaction/$dm"
		done
	done
	if [ -z "$bad" ]; then f init "ok system, workload and both classes exist in $(rc_roots | wc -l) hierarchies; init twice gives the same $(grep -c . "$T/init1.$RC_P") lines"
	else f init "fail:$bad"; fi
	# the limits of the policy are what the kernel holds
	bad=
	for spec in workload:536870912:512 workload/interactive:268435456:256 workload/batch:134217728:128; do
		dm="${spec%%:*}"
		rest="${spec#*:}"
		mem="${rest%%:*}"
		pids="${rest#*:}"
		st="${dm#workload}"
		st="${st#/}"
		gm="$(rc_val "${st:-workload}" memory.max)"
		gp="$(rc_val "${st:-workload}" pids.max)"
		[ "$gm" = "$mem" ] || bad="$bad $dm:memory.max=$gm(want $mem)"
		[ "$gp" = "$pids" ] || bad="$bad $dm:pids.max=$gp(want $pids)"
	done
	if [ -z "$bad" ]; then f init_limits "ok memory.max 512M/256M/128M and pids.max 512/256/128 for workload/interactive/batch"
	else f init_limits "fail:$bad"; fi
	ws="$(rc_weight system) $(rc_weight workload/interactive) $(rc_weight workload/batch)"
	if [ "$ws" = "1000 800 100" ]; then f init_weights "ok cpu weight system/interactive/batch = $ws (through $(rc_api cpu.weight) files)"
	else f init_weights "fail:cpu weight system/interactive/batch = $ws (wanted 1000 800 100)"; fi
	# the system domain is protected from reclaim where the kernel can do that
	case "$(rc_api memory)" in
	v2)
		low="$(cat "$(rc_dir memory system)/memory.low" 2>/dev/null)"
		if [ "$low" = 67108864 ]; then f system_protection "ok memory.low=$low (memory.reserve = 64M) on agent-interaction/system"
		else f system_protection "fail:memory.low=${low:-?} (wanted 67108864)"; fi
		;;
	*)
		line="$(grep '^unbound=system memory.low' "$T/init1.$RC_P")"
		if [ -n "$line" ]; then f system_protection "unsupported:$line"
		else f system_protection "fail:the memory hierarchy is $(rc_api memory) and init did not report memory.low as unbound"; fi
		;;
	esac

	# run: the exit status of the command is the exit status of resctl, and the command is in the domain
	rc run batch/n1 -- sh -c 'exit 7'
	rs=$RS
	rc list
	line="$(grep '^batch/n1 ' "$T/rc.out")"
	if [ "$rs" = 7 ] && [ "$line" = "batch/n1 procs=0 state=empty" ]; then f run_rc "ok exit 7 came through; the domain stays, empty ($line)"
	else f run_rc "fail:rc=$rs (wanted 7), list says '$line' ($(rcerr))"; fi
	rc_drop batch/n1
	rc run batch/m1 -- cat /proc/self/cgroup
	want_n="$(rc_roots | wc -l)"
	have_n="$(grep -c '/agent-interaction/workload/batch/m1$' "$T/rc.out")"
	if [ "$RS" = 0 ] && [ "$have_n" = "$want_n" ]; then f run_member "ok the command ran in agent-interaction/workload/batch/m1 of $have_n hierarchies"
	else f run_member "fail:rc=$RS, the command was in the workload domain of $have_n of $want_n hierarchies"; fi
	rc_drop batch/m1

	# list and status of a running workload
	rcbg run batch/x1 -- sleep 600 >/dev/null 2>&1 &
	px=$!
	if waitfor 10 rc_has batch/x1 1 running; then
		rc status batch/x1
		cur="$(sed -n 's/^pids.current=//p' "$T/rc.out")"
		mem="$(sed -n 's/^memory.current=//p' "$T/rc.out")"
		dom="$(sed -n 's/^domain=//p' "$T/rc.out")"
		if [ "$cur" = 1 ] && [ "${mem:-0}" -gt 0 ] && [ "$dom" = agent-interaction/workload/batch/x1 ]; then f list_status "ok list: procs=1 state=running; status: pids.current=1 memory.current=$mem"
		else f list_status "fail:pids.current=${cur:-?} memory.current=${mem:-?} domain=${dom:-?}"; fi
	else f list_status "fail:list never showed batch/x1 with one running process: $(grep x1 "$T/rc.out")"; fi
	rc_drop batch/x1 "$px"

	# create, a duplicate, remove
	rc create batch/c1
	r1=$RS
	made="$(grep -c '^created=batch/c1$' "$T/rc.out")"
	cdir="$(rc_dir memory workload/batch/c1)"
	[ -d "$cdir" ] && ex=yes || ex=no
	rc create batch/c1
	r2=$RS
	rc remove batch/c1
	r3=$RS
	[ -d "$cdir" ] && ex2=yes || ex2=no
	if [ "$r1 $made $ex $r2 $r3 $ex2" = "0 1 yes 1 0 no" ]; then f create_remove "ok create 0 and created=, directory exists; duplicate create 1; remove 0 and the directory is gone"
	else f create_remove "fail:create=$r1 created-lines=$made dir=$ex duplicate=$r2 remove=$r3 dir-after=$ex2 (wanted 0 1 yes 1 0 no)"; fi

	# the policy bounds every workload: a start may raise the defaults of its class (96M, 64) up to the class ceiling
	# (128M, 128), and more is refused before anything is made
	bad=
	rc create batch/p1 --memory-max 129M
	[ "$RS" = 2 ] || bad="$bad 129M=rc$RS"
	rc create batch/p1 --pids-max 129
	[ "$RS" = 2 ] || bad="$bad pids129=rc$RS"
	rc_gone batch/p1 || bad="$bad p1-was-created"
	rc create batch/p2 --memory-max 128M --pids-max 128
	[ "$RS" = 0 ] || bad="$bad 128M=rc$RS"
	rc status batch/p2
	pm="$(sed -n 's/^memory.max=//p' "$T/rc.out") $(sed -n 's/^pids.max=//p' "$T/rc.out")"
	[ "$pm" = "134217728 128" ] || bad="$bad raised-limits=$pm"
	rc remove batch/p2
	rc create batch/p3
	[ "$RS" = 0 ] || bad="$bad default=rc$RS"
	rc status batch/p3
	pm="$(sed -n 's/^memory.max=//p' "$T/rc.out") $(sed -n 's/^pids.max=//p' "$T/rc.out")"
	[ "$pm" = "100663296 64" ] || bad="$bad default-limits=$pm"
	rc remove batch/p3
	if [ -z "$bad" ]; then f policy_refusal "ok --memory-max 129M and --pids-max 129 refused with 2 and nothing created; 128M and 128 (the class ceiling) accepted; without flags a workload gets the class defaults 96M and 64"
	else f policy_refusal "fail:$bad"; fi

	# what is not a workload, a class or a program is a usage error
	bad=
	for a in "create system/x" "create workload/x" "create nosuch/x" "create batch/../x" "create ../x" "status ../x" "kill batch" "run nosuch/x -- true" "run batch/z -- /nonexistent/program"; do
		rc $a
		[ "$RS" = 2 ] || bad="$bad '$a'=rc$RS"
	done
	rc_gone batch/z || bad="$bad batch/z-was-created"
	rc kill batch/never-made
	[ "$RS" != 0 ] || bad="$bad kill-of-a-missing-workload=rc0"
	rc frobnicate
	[ "$RS" = 2 ] || bad="$bad unknown-command=rc$RS"
	if [ -z "$bad" ]; then f usage_errors "ok system/x, workload/x, an unknown class, path tricks, a class as target, an unknown program and an unknown command all give 2; a missing workload is refused ($RS)"
	else f usage_errors "fail:$bad"; fi
	canary_note normal
}

# ------------------------------------------------------------------------------------------------ resource limits
rc_s_limits() {
	section limits || return 0
	# memory: a 32M workload that wants 200M is killed by the kernel, and the domain counted it
	rc run batch/oom --memory-max 32M -- "$CT" mem 200
	rs=$RS
	kills="$(rc_val batch/oom memory.oom_kill)"
	peak="$(rc_val batch/oom memory.peak)"
	if [ "$rs" = 137 ] && [ "${kills:-0}" -ge 1 ] && [ "${peak:-0}" -le 37748736 ]; then f memory_oom "ok rc=137 oom_kill=$kills peak=$peak limit=33554432"
	else f memory_oom "fail:rc=$rs oom_kill=${kills:-?} peak=${peak:-?} (wanted 137, >=1, <=37748736)"; fi
	rc_drop batch/oom

	# pids: at most 8 tasks; the 9th fork is refused and counted
	rc run batch/pids --pids-max 8 -- "$CT" fork 30
	n="$(sed -n 's/^forked=\([0-9]*\) of 30 (refused)$/\1/p' "$T/rc.out")"
	hit="$(rc_val batch/pids pids.refused)"
	if [ -n "$n" ] && [ "$n" -le 8 ] && [ "${hit:-0}" -ge 1 ]; then f pids_limit "ok forked=$n of 30 limit=8 pids.refused=$hit"
	else f pids_limit "fail:$(tail -n 1 "$T/rc.out") pids.refused=${hit:-?} (wanted forked<=8, refused>=1)"; fi
	rc kill batch/pids
	rc_drop batch/pids

	# cpuset: a workload confined to CPU 1 sees only CPU 1
	if ! rc_bound cpuset; then f cpuset "unsupported:$(rc_feat cpuset)"
	elif [ "$(nproc --all)" -lt 2 ]; then f cpuset "unsupported:the system has $(nproc --all) CPU"
	else
		rc run batch/cs --cpus 1 -- sh -c 'sed -n "s/^Cpus_allowed_list:[[:space:]]*//p" /proc/self/status'
		got="$(tail -n 1 "$T/rc.out")"
		if [ "$RS" = 0 ] && [ "$got" = 1 ]; then f cpuset "ok Cpus_allowed_list=$got"
		else f cpuset "fail:rc=$RS Cpus_allowed_list=${got:-?} $(rcerr) (wanted 1)"; fi
		rc_drop batch/cs
	fi

	# swap: --swap none is enforced, or reported as not enforceable; it is never dropped silently
	rc create batch/sw --swap none
	rs=$RS
	line="$(grep -E '^(set|unbound)=memory.swap' "$T/rc.out")"
	d="$(rc_dir memory workload/batch/sw)"
	case "$(rc_api memory):$line" in
	v2:set=*)
		v="$(cat "$d/memory.swap.max" 2>/dev/null)"
		if [ "$rs" = 0 ] && [ "$v" = 0 ]; then f swap_denied "ok memory.swap.max=0 ($line)"
		else f swap_denied "fail:rc=$rs memory.swap.max=${v:-?} ($line)"; fi
		;;
	v1:set=*)
		m="$(cat "$d/memory.limit_in_bytes" 2>/dev/null)"
		ms="$(cat "$d/memory.memsw.limit_in_bytes" 2>/dev/null)"
		if [ "$rs" = 0 ] && [ -n "$m" ] && [ "$m" = "$ms" ]; then f swap_denied "ok memory.memsw.limit_in_bytes=$ms equals memory.limit_in_bytes ($line)"
		else f swap_denied "fail:rc=$rs memsw=${ms:-?} limit=${m:-?} ($line)"; fi
		;;
	v1:unbound=*)
		if [ "$rs" = 0 ] && [ ! -e "$d/memory.memsw.limit_in_bytes" ]; then f swap_denied "unsupported:$line"
		else f swap_denied "fail:rc=$rs reported '$line' but the file exists: $(ls "$d" | grep -c memsw) memsw files"; fi
		;;
	*) f swap_denied "fail:rc=$rs, resctl reported nothing about the swap of the workload ($(rcerr))" ;;
	esac
	rc_drop batch/sw

	# oom group: with it a kill of the kernel takes the whole workload, without it only one process
	if [ "$(rc_api memory)" = v2 ]; then
		rc run batch/og --memory-max 48M --oom-group yes -- sh -c "sleep 4711 & exec '$CT' mem 200"
		a=$RS
		waitfor 5 sh -c '[ "$(cat /proc/[0-9]*/cmdline 2>/dev/null | tr "\0" " " | grep -c "^sleep 4711 ")" = 0 ]'
		alive_group="$(rc_find 'sleep 4711 ')"
		rc_drop batch/og
		rc run batch/ogc --memory-max 48M --oom-group no -- sh -c "sleep 4712 & exec '$CT' mem 200"
		b=$RS
		alive_ctl="$(rc_find 'sleep 4712 ')"
		rc_drop batch/ogc
		pkill -f 'sleep 4712' 2>/dev/null
		if [ "$a $b $alive_group $alive_ctl" = "137 137 0 1" ]; then f oom_group "ok --oom-group yes killed the sibling too, --oom-group no left it running"
		else f oom_group "fail:rc yes/no=$a/$b sibling alive with group=$alive_group without=$alive_ctl (wanted 137/137, 0, 1)"; fi
	else
		rc create batch/og --oom-group yes
		line="$(grep '^unbound=memory.oom.group' "$T/rc.out")"
		rs=$RS
		rc_drop batch/og
		if [ "$rs" = 0 ] && [ -n "$line" ]; then f oom_group "unsupported:$line"
		else f oom_group "fail:rc=$rs and no unbound=memory.oom.group line on a $(rc_api memory) memory hierarchy"; fi
	fi
	# the bias of a workload toward the OOM killer is its floor: a workload that runs as another user cannot lower it. The
	# kernel keeps a value as the minimum only when the writer holds CAP_SYS_RESOURCE, which is measured here too.
	capeff="$(sed -n 's/^CapEff:[[:space:]]*//p' /proc/self/status)"
	if [ $((0x${capeff:-0} >> 24 & 1)) = 1 ]; then capres=yes; else capres=no; fi
	f cap_sys_resource "$capres CapEff=$capeff"
	lk_dir="$T/${RC_P}lk"
	mkdir -p "$lk_dir"
	chmod 777 "$lk_dir"
	{ grep -v '^allow.root' "$RC_POLICY"; printf 'uid = 65534\ngid = 65534\n'; } >"$lk_dir/policy"
	rc_pol "$lk_dir/policy" supervise batch/lk --restart never -- sh -c 'if echo 0 >/proc/self/oom_score_adj; then b=lowered; else b=kept; fi; echo "$(id -u) $b $(cat /proc/self/oom_score_adj)" >"$0"' "$lk_dir/out"
	lk_rc=$RS
	lk="$(cat "$lk_dir/out" 2>/dev/null)"
	rc reset batch/lk
	if [ "$lk_rc $lk" = "0 65534 kept 1000" ]; then f oom_bias_locked "ok a workload running as uid 65534 could not lower its oom_score_adj from 1000 (CAP_SYS_RESOURCE: $capres)"
	elif [ "$lk_rc $lk" = "0 65534 lowered 0" ] && [ "$capres" = no ]; then f oom_bias_locked "unsupported:CAP_SYS_RESOURCE is not in the capability set: the kernel does not keep the bias as a minimum, and a workload running as uid 65534 lowered it to 0"
	else f oom_bias_locked "fail:rc=$lk_rc '$lk' with CAP_SYS_RESOURCE $capres (wanted 0 '65534 kept 1000')"; fi
	canary_note limits
}

# ------------------------------------------------------------------------------------------------ throttling
rc_s_throttle() {
	section throttle || return 0
	# a quota of half a CPU throttles a busy loop to about half of the wall time, and the domain counted it
	if rc_bound cpu.quota; then
		rc run batch/cq --cpu-max 50% -- taskset -c 0 "$CT" cpu 3
		ratio="$(sed -n 's/^cpu-ratio=//p' "$T/rc.out")"
		thr="$(rc_val batch/cq cpu.throttled)"
		if [ -n "$ratio" ] && cg_between "$ratio" 0.30 0.70 && [ "${thr:-0}" -ge 1 ]; then f cpu_quota "ok cpu-ratio=$ratio quota=0.50 cpu.throttled=$thr"
		else f cpu_quota "fail:cpu-ratio=${ratio:-?} cpu.throttled=${thr:-?} (wanted 0.30-0.70 and >=1) $(rcerr)"; fi
		rc_drop batch/cq
	else f cpu_quota "unsupported:$(rc_feat cpu.quota)"; fi

	# the CPU is shared between classes by weight: 800 (interactive) to 100 (batch) on one CPU
	if rc_bound cpu.weight && [ "$(nproc --all)" -ge 1 ]; then
		rcbg run interactive/w1 -- taskset -c 0 "$CT" cpu 5 >"$T/w1.out" 2>&1 &
		a=$!
		rcbg run batch/w2 -- taskset -c 0 "$CT" cpu 5 >"$T/w2.out" 2>&1 &
		b=$!
		wait "$a" "$b"
		r1="$(sed -n 's/^cpu-ratio=//p' "$T/w1.out")"
		r2="$(sed -n 's/^cpu-ratio=//p' "$T/w2.out")"
		ratio="$(cg_ratio "${r1:-0}" "${r2:-0}")"
		if cg_between "$ratio" 2.5 20; then f cpu_weight "ok interactive got cpu-ratio $r1, batch $r2: ratio $ratio for weights 800:100"
		else f cpu_weight "fail:cpu-ratio interactive=${r1:-?} batch=${r2:-?} ratio=$ratio (wanted 2.5-20 for weights 800:100)"; fi
		rc_drop interactive/w1
		rc_drop batch/w2
	else f cpu_weight "unsupported:$(rc_feat cpu.weight)"; fi

	# io: 1 MiB/s of direct writes to a block device turns a 4 MiB write from milliseconds into seconds
	if ! rc_bound io; then f io_throttle "unsupported:$(rc_feat io)"
	elif [ ! -b "${CG_BLKDEV:-}" ]; then f io_throttle "fail:no block device to test with (CG_BLKDEV=${CG_BLKDEV:-unset})"
	else
		ids="$(cg_blkdev_ids)"
		t0="$(cg_uptime)"
		rc run batch/iof -- dd if=/dev/zero of="$CG_BLKDEV" bs=1M count=4 oflag=direct
		t1="$(cg_uptime)"
		rc run batch/io --io "$CG_BLKDEV wbps=1048576" -- dd if=/dev/zero of="$CG_BLKDEV" bs=1M count=4 oflag=direct
		t2="$(cg_uptime)"
		rs=$RS
		free="$(rc_secs "$t0" "$t1")"
		slow="$(rc_secs "$t1" "$t2")"
		d="$(rc_dir io workload/batch/io)"
		case "$(rc_api io)" in
		v2) wbytes="$(sed -n "s/^$ids .*wbytes=\([0-9]*\).*/\1/p" "$d/io.stat")" ;;
		v1) wbytes="$(awk -v id="$ids" '$1 == id && $2 == "Write" { s += $3 } END { print s + 0 }' "$d/blkio.throttle.io_service_bytes")" ;;
		esac
		if [ "$rs" = 0 ] && cg_between "$slow" 2.5 30 && cg_slower "$slow" "$free" 3 && [ "${wbytes:-0}" -ge 4194304 ]; then
			f io_throttle "ok $ids wbps=1048576: 4 MiB took ${slow}s throttled, ${free}s unthrottled; accounted wbytes=$wbytes"
		else f io_throttle "fail:rc=$rs $ids throttled ${slow}s unthrottled ${free}s wbytes=${wbytes:-?} (wanted >=2.5s, >=3x slower, wbytes>=4194304) $(rcerr)"; fi
		rc_drop batch/io
		rc_drop batch/iof
	fi
	canary_note throttle
}

# ------------------------------------------------------------------------------------------------ pressure
rc_s_pressure() {
	section pressure || return 0
	# PSI: two busy tasks on one CPU stall each other, and the domain's pressure says so
	if rc_bound pressure; then
		rc run batch/ps -- sh -c "taskset -c 0 '$CT' cpu 3 & taskset -c 0 '$CT' cpu 3 & wait"
		rs=$RS
		tot="$(rc_val batch/ps pressure.cpu.some_total)"
		if [ "$rs" = 0 ] && [ "${tot:-0}" -ge 100000 ]; then f psi_cpu "ok pressure.cpu some total=${tot}us after two burners shared one CPU for 3 s"
		else f psi_cpu "fail:rc=$rs pressure.cpu.some_total=${tot:-?} (wanted >=100000) $(rcerr)"; fi
		rc_drop batch/ps
	else f psi_cpu "unsupported:$(rc_feat pressure)"; fi

	# memory.high throttles a workload that goes over it instead of killing it
	if [ "$(rc_api memory)" = v2 ]; then
		timeout 120 "$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" --state-dir "$RC_STATE" run batch/mh --memory-high 24M --memory-max 96M -- "$CT" mem 40 >"$T/mh.out" 2>&1
		rs=$?
		d="$(rc_dir memory workload/batch/mh)"
		high="$(sed -n 's/^high //p' "$d/memory.events")"
		oom="$(sed -n 's/^oom_kill //p' "$d/memory.events")"
		if [ "$rs" = 0 ] && grep -q '^mem-ok' "$T/mh.out" && [ "${high:-0}" -ge 1 ] && [ "${oom:-1}" = 0 ]; then f memory_high "ok mem 40 finished over memory.high=24M; high events=$high oom_kill=$oom"
		else f memory_high "fail:rc=$rs high=${high:-?} oom_kill=${oom:-?} $(tail -n 1 "$T/mh.out" | cut -c1-120) (wanted rc 0, mem-ok, high>=1, oom_kill 0)"; fi
		rc_drop batch/mh
	else
		rc create batch/mh --memory-high 24M
		line="$(grep '^unbound=memory.high' "$T/rc.out")"
		rs=$RS
		rc_drop batch/mh
		if [ "$rs" = 0 ] && [ -n "$line" ]; then f memory_high "unsupported:$line"
		else f memory_high "fail:rc=$rs and no unbound=memory.high line on a $(rc_api memory) memory hierarchy"; fi
	fi
	canary_note pressure
}

# ------------------------------------------------------------------------------------------------ workload isolation
rc_s_isolation() {
	section isolation || return 0
	# no command of resctl can reach the system domain or the workload domain as a whole
	bad=
	for c in kill stop freeze thaw remove; do
		for t in system workload system/x workload/x; do
			rc "$c" "$t"
			[ "$RS" = 2 ] || bad="$bad $c:$t=rc$RS"
		done
	done
	for a in "run system/x -- true" "run workload/x -- true" "enter workload -- true" "enter system/x -- true"; do
		rc $a
		[ "$RS" = 2 ] || bad="$bad '$a'=rc$RS"
	done
	rc status system
	[ "$RS" = 0 ] || bad="$bad status-system=rc$RS"
	canary_check || bad="$bad canary:$CANARY_WHY"
	if [ -z "$bad" ]; then f system_unreachable "ok kill, stop, freeze, thaw, remove on system, workload, system/x and workload/x are refused with 2; run and enter too; the canary runs"
	else f system_unreachable "fail:$bad"; fi

	# a workload that exhausts its memory takes nothing else with it, not even a workload of its own class
	rcbg run batch/sb -- sleep 600 >/dev/null 2>&1 &
	pb=$!
	waitfor 10 rc_has batch/sb 1 running
	rc run batch/sa --memory-max 32M -- "$CT" mem 200
	ra=$RS
	kills_a="$(rc_val batch/sa memory.oom_kill)"
	rc status batch/sb
	kills_b="$(sed -n 's/^memory.oom_kill=//p' "$T/rc.out")"
	st_b="$(sed -n 's/^state=//p' "$T/rc.out")"
	hit_b="$(rc_mem_hit batch/sb 100663296)"
	case "$hit_b" in not*) hit_b_ok=yes ;; *) hit_b_ok=no ;; esac
	if [ "$ra" = 137 ] && [ "${kills_a:-0}" -ge 1 ] && [ "$kills_b" = 0 ] && [ "$hit_b_ok" = yes ] && [ "$st_b" = running ] && rc_alive "$pb"; then
		f sibling_isolation "ok batch/sa was OOM-killed (oom_kill=$kills_a); batch/sb is $st_b with oom_kill=$kills_b, its memory limit not hit ($hit_b)"
	else f sibling_isolation "fail:sa rc=$ra oom_kill=${kills_a:-?}; sb state=${st_b:-?} oom_kill=${kills_b:-?} limit: $hit_b"; fi
	rc_drop batch/sa
	rc_drop batch/sb "$pb"
	canary_note sibling_isolation

	# the class ceiling binds two workloads that are each inside their own limit (80M each of 96M, 128M for the class)
	rcbg run batch/ca --memory-max 96M -- "$CT" mem 80 >"$T/ca.out" 2>&1 &
	a=$!
	rcbg run batch/cb --memory-max 96M -- "$CT" mem 80 >"$T/cb.out" 2>&1 &
	b=$!
	wait "$a"
	ra=$?
	wait "$b"
	rb=$?
	chit="$(rc_mem_hit batch 134217728)"
	cpeak="$(rc_val batch memory.peak)"
	la="$(rc_mem_hit batch/ca 100663296)"
	lb="$(rc_mem_hit batch/cb 100663296)"
	ka="$(rc_val batch/ca memory.oom_kill)"
	kb="$(rc_val batch/cb memory.oom_kill)"
	kills=$((${ka:-0} + ${kb:-0}))
	ok=no
	[ "$ra" = 137 ] || [ "$rb" = 137 ] && ok=yes
	case "$chit:$la:$lb" in
	hit*:not*:not*) [ "$ok" = yes ] && [ "$kills" -ge 1 ] && [ "${cpeak:-0}" -le 138412032 ] || ok=no ;;
	*) ok=no ;;
	esac
	if [ "$ok" = yes ]; then
		f class_ceiling "ok rc=$ra/$rb; the class limit of 134217728 bound ($chit), one workload was OOM-killed (oom_kill=$kills), the workloads' own limits were not hit (ca: $la; cb: $lb)"
	else f class_ceiling "fail:rc=$ra/$rb class: $chit; ca: $la; cb: $lb; oom_kill=$kills (wanted one 137, class hit, leaves not hit, a kill counted, peak<=138412032)"; fi
	rc_drop batch/ca
	rc_drop batch/cb

	# the aggregate ceiling of the workload domain binds before either class does (96M for 2 x 60M)
	# (peaks are high-water marks: the domains are made new, so that the class ceiling run above leaves no trace in them)
	rc teardown
	RT=$RS
	RC_POLICY="$RC_DIR/agg.policy"
	rc init
	if [ "$RT" != 0 ]; then f aggregate_ceiling "fail:teardown before agg.policy: rc $RT"
	elif [ "$RS" != 0 ]; then f aggregate_ceiling "fail:init under agg.policy: $(rcerr)"
	else
		rcbg run interactive/ia --memory-max 64M -- "$CT" mem 60 >"$T/ia.out" 2>&1 &
		a=$!
		rcbg run batch/ib --memory-max 64M -- "$CT" mem 60 >"$T/ib.out" 2>&1 &
		b=$!
		wait "$a"
		ra=$?
		wait "$b"
		rb=$?
		whit="$(rc_mem_hit workload 100663296)"
		wpeak="$(rc_val workload memory.peak)"
		ci="$(rc_mem_hit interactive 83886080)"
		cb="$(rc_mem_hit batch 83886080)"
		ka="$(rc_val interactive/ia memory.oom_kill)"
		kb="$(rc_val batch/ib memory.oom_kill)"
		kills=$((${ka:-0} + ${kb:-0}))
		ok=no
		[ "$ra" = 137 ] || [ "$rb" = 137 ] && ok=yes
		case "$whit:$ci:$cb" in
		hit*:not*:not*) [ "$ok" = yes ] && [ "$kills" -ge 1 ] && [ "${wpeak:-0}" -le 100663296 ] || ok=no ;;
		*) ok=no ;;
		esac
		if [ "$ok" = yes ]; then
			f aggregate_ceiling "ok rc=$ra/$rb; the workload ceiling of 100663296 bound ($whit), one workload was OOM-killed (oom_kill=$kills), neither class limit was hit (interactive: $ci; batch: $cb)"
		else f aggregate_ceiling "fail:rc=$ra/$rb workload: $whit; interactive: $ci; batch: $cb; oom_kill=$kills (wanted one 137, workload hit, classes not hit, a kill counted, peak<=100663296)"; fi
		rc_drop interactive/ia
		rc_drop batch/ib
	fi
	RC_POLICY="$RC_DIR/test.policy"
	rc init
	[ "$RS" = 0 ] || f aggregate_ceiling_restore "fail:init under test.policy: $(rcerr)"
	canary_note ceilings

	# a fork bomb in a workload is held at its pids limit, and the system domain keeps forking
	rcbg run batch/fb --pids-max 32 -- "$CT" fork 500 >"$T/fb.out" 2>&1 &
	pf=$!
	rcx enter system -- sh -c 'i=0; : >"$0"; while [ $i -lt 20 ]; do sleep 1 & echo $! >>"$0"; i=$((i + 1)); done; wait; echo "distinct=$(sort -u "$0" | wc -l)"' "$T/fb.pids" >"$T/fb.sys" 2>&1
	srs=$?
	wait "$pf"
	brs=$?
	n="$(sed -n 's/^forked=\([0-9]*\) of 500 (refused)$/\1/p' "$T/fb.out")"
	refused="$(rc_val batch/fb pids.refused)"
	dist="$(sed -n 's/^distinct=//p' "$T/fb.sys")"
	if [ "$srs" = 0 ] && [ "$dist" = 20 ] && [ -n "$n" ] && [ "$n" -le 32 ] && [ "${refused:-0}" -ge 1 ]; then
		f fork_bomb_isolated "ok the bomb got $n of 500 forks (limit 32, pids.refused=$refused); a system job started $dist of 20 processes meanwhile"
	else f fork_bomb_isolated "fail:bomb rc=$brs forked=${n:-?} refused=${refused:-?}; system job rc=$srs distinct=${dist:-?} $(tail -n 1 "$T/fb.sys" | cut -c1-100) (wanted forked<=32, refused>=1, 20 processes)"; fi
	rc kill batch/fb
	rc_drop batch/fb
	canary_report system_canary
}

# ------------------------------------------------------------------------------------------------ termination
# rc_ready FILE: succeeds once a workload script has created FILE (it does that after setting up its signal handling).
rc_ready() { [ -e "$1" ]; }
rc_pids_ge() { [ "$(rc_val "$1" pids.current)" -ge "$2" ] 2>/dev/null; }
rc_find_is() { [ "$(rc_find "$1")" = "$2" ]; }

rc_s_terminate() {
	section terminate || return 0
	# freeze holds a busy workload still, thaw lets it go on
	rcbg run batch/ft -- "$CT" cpu 60 >/dev/null 2>&1 &
	pj=$!
	waitfor 10 rc_has batch/ft 1 running
	rc freeze batch/ft
	r1=$RS
	rc status batch/ft
	st1="$(sed -n 's/^state=//p' "$T/rc.out")"
	k1="$(rc_ticks "$pj")"
	sleep 1
	k2="$(rc_ticks "$pj")"
	rc thaw batch/ft
	r2=$RS
	sleep 1
	k3="$(rc_ticks "$pj")"
	rc status batch/ft
	st2="$(sed -n 's/^state=//p' "$T/rc.out")"
	if [ "$r1 $r2 $st1 $st2" = "0 0 frozen running" ] && [ "$k2" = "$k1" ] && [ $((k3 - k2)) -ge 10 ]; then
		f freeze_thaw "ok state=frozen and no CPU time in 1 s ($k1 -> $k2 ticks); after thaw state=running and $((k3 - k2)) ticks in 1 s"
	else f freeze_thaw "fail:freeze rc=$r1 state=$st1 ticks while frozen $k1 -> $k2; thaw rc=$r2 state=$st2 ticks after $k2 -> $k3 (wanted frozen, +0, running, >=+10)"; fi
	rc_drop batch/ft "$pj"
	canary_note freeze_thaw

	# stop: SIGTERM first, and a workload that handles it is gone as soon as it has
	rcbg run batch/sg -- sh -c 'trap "exit 0" TERM; : >"$0"; while :; do sleep 0.2; done' "$T/${RC_P}sg.ready" >/dev/null 2>&1 &
	pj=$!
	waitfor 10 rc_ready "$T/${RC_P}sg.ready"
	t0="$(cg_uptime)"
	rc stop batch/sg
	rs=$RS
	t1="$(cg_uptime)"
	how="$(sed -n 's/^stop=//p' "$T/rc.out")"
	el="$(rc_secs "$t0" "$t1")"
	wait "$pj" 2>/dev/null
	left="$(rc_procs batch/sg)"
	if [ "$rs $how $left" = "0 graceful 0" ] && cg_between "$el" 0 1.9; then f stop_graceful "ok stop=$how after ${el}s (grace 2s); the domain is empty"
	else f stop_graceful "fail:rc=$rs stop=${how:-?} after ${el}s, ${left:-?} processes left (wanted 0, graceful, <2 s, 0)"; fi
	rc_drop batch/sg
	canary_note stop_graceful

	# stop: a workload that ignores SIGTERM is killed when the grace period is over
	rcbg run batch/sf -- sh -c 'trap "" TERM; : >"$0"; while :; do sleep 1; done' "$T/${RC_P}sf.ready" >/dev/null 2>&1 &
	pj=$!
	waitfor 10 rc_ready "$T/${RC_P}sf.ready"
	t0="$(cg_uptime)"
	rc stop batch/sf --grace 2s
	rs=$RS
	t1="$(cg_uptime)"
	how="$(sed -n 's/^stop=//p' "$T/rc.out")"
	el="$(rc_secs "$t0" "$t1")"
	wait "$pj" 2>/dev/null
	left="$(rc_procs batch/sf)"
	case "$how" in killed:[1-9]*) n=yes ;; *) n=no ;; esac
	if [ "$rs $n $left" = "0 yes 0" ] && cg_between "$el" 2 10; then f stop_forced "ok stop=$how after ${el}s: SIGTERM was ignored, the kill came after the 2 s grace; the domain is empty"
	else f stop_forced "fail:rc=$rs stop=${how:-?} after ${el}s, ${left:-?} processes left (wanted 0, killed:N, 2-10 s, 0) $(rcerr)"; fi
	rc_drop batch/sf
	canary_note stop_forced

	# kill: processes that left the process group and session are still in the domain, and are killed with it
	rcbg run batch/ke -- sh -c 'for i in 1 2 3 4; do setsid sleep 472$i & done; wait' >/dev/null 2>&1 &
	pj=$!
	waitfor 10 rc_find_is 'sleep 472' 4
	before="$(rc_find 'sleep 472')"
	rc kill batch/ke
	rs=$RS
	waitfor 5 rc_find_is 'sleep 472' 0
	after="$(rc_find 'sleep 472')"
	wait "$pj" 2>/dev/null
	left="$(rc_procs batch/ke)"
	if [ "$rs $before $after $left" = "0 4 0 0" ]; then f kill_escapees "ok 4 setsid'd sleeps ran, kill ended them all; the domain is empty"
	else f kill_escapees "fail:kill rc=$rs sleeps before=$before after=$after, ${left:-?} processes in the domain (wanted 0 4 0 0) $(rcerr)"; fi
	rc_drop batch/ke
	canary_note kill_escapees

	# kill: a workload that has filled its pids limit with a fork loop is ended, every process of it (the shell of the
	# loop dies at the first refused fork, the sleeps it started stay)
	rcbg run batch/fl --pids-max 60 -- sh -c 'while :; do sleep 4731 & done' >/dev/null 2>&1 &
	pj=$!
	waitfor 15 rc_pids_ge batch/fl 50
	busy="$(rc_val batch/fl pids.current)"
	refused="$(rc_val batch/fl pids.refused)"
	rc kill batch/fl
	rs=$RS
	wait "$pj" 2>/dev/null
	left="$(rc_procs batch/fl)"
	waitfor 5 rc_find_is 'sleep 4731 ' 0
	after="$(rc_find 'sleep 4731 ')"
	if [ "$rs $left $after" = "0 0 0" ] && [ "${busy:-0}" -ge 50 ] && [ "${refused:-0}" -ge 1 ]; then f kill_pidslimit "ok the loop held $busy processes at the limit of 60 (pids.refused=$refused) when kill came; 0 processes and 0 sleeps are left"
	else f kill_pidslimit "fail:kill rc=$rs busy=${busy:-?} refused=${refused:-?} processes left=${left:-?} sleeps left=$after (wanted 0, >=50, >=1, 0, 0) $(rcerr)"; fi
	rc_drop batch/fl
	canary_note kill_pidslimit

	# kill and stop reach a frozen workload
	rcbg run batch/kf -- sleep 4741 >/dev/null 2>&1 &
	pj=$!
	waitfor 10 rc_has batch/kf 1 running
	rc freeze batch/kf
	a=$RS
	rc kill batch/kf
	b=$RS
	wait "$pj" 2>/dev/null
	left="$(rc_procs batch/kf)"
	after="$(rc_find 'sleep 4741 ')"
	if [ "$a $b $left $after" = "0 0 0 0" ]; then f kill_frozen "ok kill of a frozen workload: rc 0, the domain is empty"
	else f kill_frozen "fail:freeze rc=$a kill rc=$b processes left=${left:-?} sleeps left=$after (wanted 0 0 0 0)"; fi
	rc_drop batch/kf
	rcbg run batch/sz -- sleep 4742 >/dev/null 2>&1 &
	pj=$!
	waitfor 10 rc_has batch/sz 1 running
	rc freeze batch/sz
	a=$RS
	t0="$(cg_uptime)"
	rc stop batch/sz
	b=$RS
	t1="$(cg_uptime)"
	how="$(sed -n 's/^stop=//p' "$T/rc.out")"
	wait "$pj" 2>/dev/null
	left="$(rc_procs batch/sz)"
	if [ "$a $b $how $left" = "0 0 graceful 0" ] && cg_between "$(rc_secs "$t0" "$t1")" 0 1.9; then f stop_frozen "ok stop of a frozen workload thawed it first: stop=$how in $(rc_secs "$t0" "$t1")s"
	else f stop_frozen "fail:freeze rc=$a stop rc=$b stop=${how:-?} processes left=${left:-?} in $(rc_secs "$t0" "$t1")s (wanted 0 0 graceful 0, <2 s)"; fi
	rc_drop batch/sz
	canary_note stop_frozen

	# remove takes a workload that is still running: the process and the directories are gone
	rcbg run batch/rp -- sleep 4751 >/dev/null 2>&1 &
	pj=$!
	waitfor 10 rc_has batch/rp 1 running
	rc remove batch/rp
	rs=$RS
	wait "$pj" 2>/dev/null
	still=0
	for r in $(rc_roots); do [ ! -d "$r/agent-interaction/workload/batch/rp" ] || still=$((still + 1)); done
	after="$(rc_find 'sleep 4751 ')"
	if [ "$rs $still $after" = "0 0 0" ] && rc_gone batch/rp; then f remove_populated "ok remove of a running workload: rc 0, no directory in any hierarchy, the process is gone"
	else f remove_populated "fail:rc=$rs directories left=$still sleeps left=$after (wanted 0 0 0) $(rcerr)"; fi
	canary_note remove_populated

	# nothing is left running in any workload
	rc list
	n="$(grep -c . "$T/rc.out")"
	busy=0
	for r in $(rc_roots); do
		[ -d "$r/agent-interaction/workload" ] || continue
		for p in $(find "$r/agent-interaction/workload" -name cgroup.procs -o -name tasks 2>/dev/null); do
			busy=$((busy + $(grep -c . "$p" 2>/dev/null)))
		done
	done
	if [ "$n $busy" = "0 0" ]; then f terminate_clean "ok list is empty and no process is left in any workload domain"
	else f terminate_clean "fail:list shows $n workloads ($(tr '\n' ',' <"$T/rc.out" | cut -c1-120)), $busy processes are left in workload domains"; fi
	canary_report system_canary_terminate
}

# ------------------------------------------------------------------------------------------------ recovery
# rc_orphan WORKLOAD SLEEP: a supervisor starts a workload, then dies by SIGKILL; the sleep goes on, with nobody watching it.
rc_orphan() {
	sh -c "'$RC_BIN' --policy '$RC_POLICY' --lifecycle '$RC_LC' --state-dir '$RC_STATE' run $1 -- sleep $2 >/dev/null 2>&1; true" &
	SUP=$!
	waitfor 10 rc_has "$1" 1 running
	kill -9 "$SUP" 2>/dev/null
	wait "$SUP" 2>/dev/null
}

rc_s_recover() {
	section recover || return 0
	# a workload whose supervisor died: list still shows it, and recover kills and removes it
	rc_orphan batch/or1 4761
	rc list
	line="$(grep '^batch/or1 ' "$T/rc.out")"
	rc recover
	rs=$RS
	got="$(grep -c '^killed=batch/or1$' "$T/rc.out")"
	after="$(rc_find 'sleep 4761 ')"
	if [ "$line" = "batch/or1 procs=1 state=running" ] && [ "$rs $got" = "0 1" ] && [ "$after" = 0 ] && rc_gone batch/or1; then
		f recover_orphan "ok list showed the orphan ($line); recover printed killed=batch/or1; the process and the domain are gone"
	else f recover_orphan "fail:list said '$line'; recover rc=$rs killed-lines=$got sleeps left=$after $(rcerr)"; fi
	canary_note recover_orphan

	# with --orphans keep the running orphan is left alone, and named
	rc_orphan batch/or2 4762
	rc recover --orphans keep
	rs=$RS
	got="$(grep -c '^kept=batch/or2$' "$T/rc.out")"
	after="$(rc_find 'sleep 4762 ')"
	if [ "$rs $got $after" = "0 1 1" ] && rc_has batch/or2 1 running; then f recover_keep "ok recover --orphans keep printed kept=batch/or2; the process still runs in its domain"
	else f recover_keep "fail:rc=$rs kept-lines=$got sleeps alive=$after (wanted 0 1 1) $(rcerr)"; fi
	rc remove batch/or2
	[ "$(rc_find 'sleep 4762 ')" = 0 ] || f recover_keep "fail:remove did not end the kept orphan"
	canary_note recover_keep

	# a second recover finds nothing to do
	rc recover
	rs=$RS
	n="$(grep -c . "$T/rc.out")"
	if [ "$rs $n" = "0 0" ]; then f recover_idempotent "ok recover on a clean system: rc 0 and no output"
	else f recover_idempotent "fail:rc=$rs and $n lines of output: $(head -n 1 "$T/rc.out")"; fi

	# a domain that was created and never used is empty, and recover removes it
	rc create batch/bad
	rs=$RS
	rc list
	line="$(grep '^batch/bad ' "$T/rc.out")"
	rc recover
	got="$(grep -c '^removed=batch/bad$' "$T/rc.out")"
	if [ "$rs" = 0 ] && [ "$line" = "batch/bad procs=0 state=empty" ] && [ "$got" = 1 ] && rc_gone batch/bad; then
		f recover_empty "ok create gave '$line'; recover printed removed=batch/bad"
	else f recover_empty "fail:create rc=$rs, list said '$line', removed-lines=$got (wanted 0, 'batch/bad procs=0 state=empty', 1)"; fi

	# a class the policy no longer names goes away when it is empty; the one it names stays
	RC_POLICY="$RC_DIR/solo.policy"
	rc recover
	rs=$RS
	gone=0
	kept=0
	for r in $(rc_roots); do
		[ -d "$r/agent-interaction/workload/batch" ] || gone=$((gone + 1))
		[ -d "$r/agent-interaction/workload/interactive" ] && kept=$((kept + 1))
	done
	want_n="$(rc_roots | wc -l)"
	RC_POLICY="$RC_DIR/test.policy"
	rc init
	back=$RS
	if [ "$rs $back $gone $kept" = "0 0 $want_n $want_n" ]; then f recover_class "ok under a policy with only the interactive class, recover removed the empty batch class in $gone hierarchies and kept interactive; init restored it"
	else f recover_class "fail:recover rc=$rs, batch gone in $gone of $want_n hierarchies, interactive kept in $kept; re-init rc=$back"; fi
	canary_report system_canary_recover

	# teardown removes everything of the workload side and keeps the system domain while it holds a process
	rcbg run batch/td -- sleep 4771 >/dev/null 2>&1 &
	pj=$!
	waitfor 10 rc_has batch/td 1 running
	rc teardown
	rs=$RS
	keptl="$(grep -c '^kept=system' "$T/rc.out")"
	done_l="$(grep -c '^teardown=done$' "$T/rc.out")"
	wait "$pj" 2>/dev/null
	left=0
	sys=0
	for r in $(rc_roots); do
		[ ! -d "$r/agent-interaction/workload" ] || left=$((left + 1))
		[ -d "$r/agent-interaction/system" ] && sys=$((sys + 1))
	done
	after="$(rc_find 'sleep 4771 ')"
	if [ "$rs $keptl $done_l $left $sys $after" = "0 1 1 0 $want_n 0" ] && canary_check; then
		f teardown_keeps_system "ok teardown killed the running workload, removed workload/ in all hierarchies, printed kept=system and teardown=done; the canary still runs in system/"
	else f teardown_keeps_system "fail:rc=$rs kept-lines=$keptl done-lines=$done_l workload dirs left=$left system dirs=$sys of $want_n sleeps left=$after canary: ${CANARY_WHY:-ok}"; fi

	# once the canary is gone, teardown leaves nothing at all
	canary_stop
	rc teardown
	rs=$RS
	keptl="$(grep -c '^kept=' "$T/rc.out")"
	left="$(rc_domain_dirs)"
	if [ "$rs $keptl $left" = "0 0 0" ]; then f teardown_clean "ok teardown without a process in system/: no kept= line, no agent-interaction directory in any hierarchy"
	else f teardown_clean "fail:rc=$rs kept-lines=$keptl directories left=$left $(rcerr)"; fi

	# and the domains come back, with workloads in them
	rc init
	a=$RS
	rc run batch/after -- true
	b=$RS
	rc_drop batch/after
	if [ "$a $b" = "0 0" ] && [ "$(rc_domain_dirs)" -ge 4 ]; then f reinit_after_teardown "ok init and run work after a teardown ($(rc_domain_dirs) directories)"
	else f reinit_after_teardown "fail:init rc=$a run rc=$b directories=$(rc_domain_dirs) $(rcerr)"; fi
	canary_start
	canary_report system_canary_reinit
}

# ------------------------------------------------------------------------------------------------ process lifecycle
# chk NAME GOT WANT: remember what differs from what was wanted (in BAD), for one fact. verdict FACT TEXT: the fact is ok
# with TEXT when nothing differed, and fails with what did else.
BAD=
chk() { [ "$2" = "$3" ] || BAD="$BAD $1='$2' (wanted '$3')"; }
verdict() {
	if [ -z "$BAD" ]; then f "$1" "ok $2"; else f "$1" "fail:$BAD"; fi
	BAD=
}
yn() { "$@" && echo yes || echo no; }
# like STRING GLOB: the string matches the shell pattern.
like() {
	case "$1" in $2) return 0 ;; esac
	return 1
}
# matches STRING ERE: the string matches the extended regular expression.
matches() { printf '%s\n' "$1" | grep -qE "$2"; }
# lines FILE: the number of lines of the file (one line per start of a workload that logs its starts).
lines() { echo $(($(wc -l <"$1") + 0)); }
# rc_out: what resctl said on stdout, on one line.
rc_out() { tr '\n' ' ' <"$T/rc.out" | sed 's/ *$//'; }

# rc_sf WORKLOAD KEY...: the named fields of `resctl state WORKLOAD`, space separated ("-" where the line has no such field).
rc_sf() {
	sfw="$1"
	shift
	rc state "$sfw"
	awk -v keys="$*" 'BEGIN { m = split(keys, k, " ") }
		NR == 1 {
			out = ""
			for (j = 1; j <= m; j++) {
				v = "-"
				for (i = 2; i <= NF; i++) if (index($i, k[j] "=") == 1) v = substr($i, length(k[j]) + 2)
				if (j > 1) out = out " "
				out = out v
			}
			print out
		}' "$T/rc.out"
}
rc_phase_is() { [ "$(rc_sf "$1" phase)" = "$2" ]; }
rc_at() { [ "$(rc_sf "$1" phase gen)" = "$2 $3" ]; }
# rc_pid_ok PID: a number, and a live process.
rc_pid_ok() {
	case "$1" in '' | *[!0-9]*) return 1 ;; esac
	rc_alive "$1"
}
# rc_in PID REL: the process is in the domain agent-interaction/REL of every serving hierarchy.
rc_in() {
	ri="$(grep -c "/agent-interaction/$2\$" "/proc/$1/cgroup" 2>/dev/null)"
	[ "${ri:-0}" -eq "$(rc_roots | wc -l)" ]
}

# rc_phases WORKLOAD: the phases the audit trail records for the workload, in order; "|" marks a reset.
rc_phases() {
	rc audit 2048
	awk -v who="$1" '
		$5 == who && $4 == "reset" { s = s "| " }
		$5 == who && $6 ~ /^phase=/ { s = s substr($6, 7) " " }
		END { sub(/ +$/, "", s); print s }' "$T/rc.out"
}
# rc_edges: "ok", or the changes of phase in the audit trail that the lifecycle table does not allow.
rc_edges() {
	rc audit 2048
	awk 'BEGIN {
			m = split("created>starting created>recovering starting>running starting>sweeping starting>recovering running>stopping running>killing running>sweeping running>recovering stopping>killing stopping>sweeping stopping>recovering killing>sweeping killing>recovering sweeping>backoff sweeping>stopped sweeping>finished sweeping>failed sweeping>recovering backoff>starting backoff>stopped backoff>failed backoff>recovering recovering>stopped recovering>failed stopped>starting finished>starting", t, " ")
			for (i = 1; i <= m; i++) allowed[t[i]] = 1
			ep = 0
		}
		$4 == "reset" { delete last[ep "|" $5]; next }
		$4 == "teardown" { ep++; next }
		$6 ~ /^phase=/ {
			key = ep "|" $5
			ph = substr($6, 7)
			if ((key in last) && !((last[key] ">" ph) in allowed)) bad = bad " " $5 ":" last[key] ">" ph "(seq " $1 ")"
			last[key] = ph
		}
		END { if (bad == "") print "ok"; else print "bad" bad }' "$T/rc.out"
}
# rc_order WORKLOAD: "ok" when the audit trail has the operator's intent (from the command line), then the supervisor's
# stop, exit and stopped phase, in that order.
rc_order() {
	rc audit 2048
	awk -v who="$1" '
		$5 != who { next }
		$4 == "intent" && !hi { hi = 1; ni = $1; ai = $3 }
		$4 == "stop" && !hs { hs = 1; ns = $1 }
		$4 == "exit" && !he { he = 1; ne = $1; ae = $3 }
		$4 == "phase" && $6 == "phase=stopped" && !hp { hp = 1; np = $1 }
		END {
			if (hi && hs && he && hp && ni + 0 < ns + 0 && ns + 0 < ne + 0 && ne + 0 < np + 0 && ai ~ /:cli$/ && ae ~ /:super$/) print "ok"
			else print "intent " ni " (" ai ") stop " ns " exit " ne " (" ae ") stopped " np
		}' "$T/rc.out"
}
# rc_ev WORKLOAD EVENT [DETAIL-ERE]: how many audit lines the event has for the workload; the detail is matched from its start.
rc_ev() {
	rc audit 2048
	if [ -n "${3:-}" ]; then grep -cE "^[0-9]+ [0-9]+ [0-9]+:[a-z]+ $2 $1 $3" "$T/rc.out"
	else grep -cE "^[0-9]+ [0-9]+ [0-9]+:[a-z]+ $2 $1( |\$)" "$T/rc.out"; fi
}

# rc_restart FILE MODE MAX WINDOW TOTAL BACKOFF BACKOFFMAX [RATE [WORKLOADS]]: a variant of the suite policy with other
# restart limits (its own part of the policy starts at the line that test.policy marks).
rc_restart() {
	sed '/^# ---- supervision/,$d' "$RC_DIR/test.policy" >"$1"
	printf '%s\n' '[restart]' "mode = $2" "max = $3" "window = $4" "total = $5" "backoff = $6" "backoff.max = $7" "rate = ${8:-120}" '' '[supervise]' "workloads = ${9:-8}" '' '[run]' 'allow.root = yes' >>"$1"
}
# rcs STATEDIR POLICY ARGS...: one command with another state directory and policy (a throwaway store).
rcs() {
	sd="$1"
	pf="$2"
	shift 2
	"$RC_BIN" --policy "$pf" --lifecycle "$RC_LC" --state-dir "$sd" "$@" >"$T/rc.out" 2>"$T/rc.err"
	RS=$?
}
# rcbgs STATEDIR POLICY ARGS...: only ever as `rcbgs ... &`, like rcbg.
rcbgs() {
	sd="$1"
	pf="$2"
	shift 2
	exec "$RC_BIN" --policy "$pf" --lifecycle "$RC_LC" --state-dir "$sd" "$@"
}
# sup_bg POLICY WORKLOAD ARGS...: `resctl supervise` in the background. Its pid is SUPPID, its output goes to
# $T/<prefix><id>.out and .err.
sup_bg() {
	spf="$1"
	sw="$2"
	shift 2
	sbo="$T/$RC_P${sw#*/}"
	rcbgs "$RC_STATE" "$spf" supervise "$sw" "$@" >"$sbo.out" 2>"$sbo.err" &
	SUPPID=$!
}
rc_dead() { ! rc_alive "$1"; }
# rc_reap PID [SECONDS]: wait for a background supervisor to be done (kill it when it hangs); its status is SPRC, SPHUNG says
# whether it had to be killed.
rc_reap() {
	waitfor "${2:-15}" rc_dead "$1"
	if rc_alive "$1"; then
		kill -9 "$1" 2>/dev/null
		SPHUNG=yes
		# a process that does not die (frozen with its domain, say) cannot be waited for: the fact fails and the cleanup that
		# follows ends it, instead of the suite waiting for ever
		if ! waitfor 5 rc_dead "$1"; then
			SPRC=137
			return 0
		fi
	else SPHUNG=no; fi
	wait "$1" 2>/dev/null
	SPRC=$?
}
# rc_pol_b SECONDS POLICYFILE ARGS...: rc_pol for a supervisor that must end by itself. A restart loop that nothing bounds would
# never end, so after SECONDS it is killed (SPHUNG=yes, RS=137) and its workload is left to the reaper and to the cleanup.
rc_pol_b() {
	rbs="$1"
	pf="$2"
	shift 2
	"$RC_BIN" --policy "$pf" --lifecycle "$RC_LC" --state-dir "$RC_STATE" "$@" >"$T/rc.out" 2>"$T/rc.err" &
	rbp=$!
	rc_reap "$rbp" "$rbs"
	RS=$SPRC
}
# rc_pid_of WORDS: the pids of the processes whose command line contains WORDS.
rc_pid_of() {
	for cf in /proc/[0-9]*/cmdline; do
		c="$(tr '\0' ' ' <"$cf" 2>/dev/null)"
		case "$c" in
		*"$1"*)
			pp="${cf#/proc/}"
			echo "${pp%/cmdline}"
			;;
		esac
	done
}
rc_reapers() { rc_pid_of ' __reap ' | wc -l; }
rc_no_reapers() { [ "$(rc_reapers)" -eq 0 ]; }
# rc_scrub: whatever a scenario that failed left behind, so that the next one starts clean (done after the facts are in).
rc_scrub() {
	for sp in $(rc_pid_of ' --state-dir '); do kill -9 "$sp" 2>/dev/null; done
	for sp in $(rc_pid_of 'sleep 48'); do kill -9 "$sp" 2>/dev/null; done
	rc list
	for sw in $(grep -E '^(batch|interactive)/' "$T/rc.out" | sed 's/ .*//'); do rc remove "$sw"; done
	rc state
	for sw in $(grep -E '^[a-z]+/[A-Za-z0-9_-]+ phase=failed ' "$T/rc.out" | sed 's/ .*//'); do rc reset "$sw"; done
}

rc_s_process() {
	section process || return 0
	pdirs="$(rc_domain_dirs)"
	seen=

	# a supervised workload goes from starting to running; the process that supervises it is the one that was started, its
	# main process is in the domain of the workload (of every hierarchy)
	sup_bg "$RC_POLICY" batch/pl -- sleep 4801
	pl=$SUPPID
	waitfor 20 rc_phase_is batch/pl running
	waitfor 20 rc_find_is 'sleep 4801 ' 1
	st="$(rc_sf batch/pl phase gen restarts supervisor domain)"
	mp="$(rc_sf batch/pl main)"
	sp="$(rc_sf batch/pl sup)"
	inw=no
	if rc_pid_ok "$mp" && rc_in "$mp" workload/batch/pl; then inw=yes; fi
	chk state "$st" "running 1 0 alive present"
	chk supervisor_pid "$sp" "$pl"
	chk main_in_domain "$inw" yes
	chk sleeps "$(rc_find 'sleep 4801 ')" 1
	verdict proc_run "phase=running gen=1 restarts=0 with the supervisor pid $sp alive; the main process $mp is in agent-interaction/workload/batch/pl of every hierarchy"
	canary_note proc_run

	# an operator's stop: the intent is recorded first, then stopping -> sweeping -> stopped, the end is labelled as requested
	rc stop batch/pl
	rs=$RS
	ou="$(rc_out)"
	rc_reap "$pl"
	sv="$(cat "$T/${RC_P}pl.out" 2>/dev/null)"
	st="$(rc_sf batch/pl phase ended status supervisor domain main)"
	ph="$(rc_phases batch/pl)"
	seen="$seen $ph"
	pl_order="$(rc_order batch/pl)"
	waitfor 5 rc_find_is 'sleep 4801 ' 0
	chk rc "$rs" 0
	chk stdout "$ou" "stop=graceful phase=stopped"
	chk supervisor_rc "$SPRC" 0
	chk supervisor_hung "$SPHUNG" no
	chk supervisor_said "$sv" "phase=stopped gen=1 restarts=0 reason=stopped on request"
	chk state "$st" "stopped requested:stop signal:15 none absent -"
	chk phases "$ph" "starting running stopping sweeping stopped"
	chk sleeps "$(rc_find 'sleep 4801 ')" 0
	chk domain_gone "$(yn rc_gone batch/pl)" yes
	chk audit_order "$pl_order" ok
	verdict proc_stop "stop=graceful: intent (command line), stop, exit and stopped phase in that order in the audit trail; phases $ph; ended=requested:stop status=signal:15; the domain is gone"
	canary_note proc_stop

	# a command that ends by itself and cleanly is finished, and is not restarted
	rc supervise batch/pc -- true
	rs=$RS
	ou="$(rc_out)"
	st="$(rc_sf batch/pc phase gen restarts ended status supervisor domain)"
	ph="$(rc_phases batch/pc)"
	seen="$seen $ph"
	chk rc "$rs" 0
	chk stdout "$ou" "phase=finished gen=1 restarts=0 reason=the workload ended cleanly"
	chk state "$st" "finished 1 0 clean exit:0 none absent"
	chk phases "$ph" "starting running sweeping finished"
	chk domain_gone "$(yn rc_gone batch/pc)" yes
	verdict proc_exit_clean "a command that exits 0 ends finished (ended=clean status=exit:0), without a restart; phases $ph; the domain is gone"
	canary_note proc_exit_clean

	# the ways a run ends are told apart: an exit code, a signal that nobody asked for, and (above) an operator's request
	rc supervise batch/px --restart never -- sh -c 'exit 3'
	rs=$RS
	st="$(rc_sf batch/px phase ended status restarts reason)"
	rc reset batch/px
	rr=$RS
	ro="$(rc_out)"
	rc supervise batch/px --restart never -- sh -c 'kill -9 $$'
	r2=$RS
	st2="$(rc_sf batch/px phase ended status restarts reason)"
	ph="$(rc_phases batch/px)"
	seen="$seen $ph"
	rc reset batch/px
	chk exit_rc "$rs" 1
	chk exit_state "$st" "failed failed:3 exit:3 0 the_restart_mode_is_never"
	chk reset "$rr $ro" "0 reset=batch/px was=failed"
	chk signal_rc "$r2" 1
	chk signal_state "$st2" "failed crashed:9 signal:9 0 the_restart_mode_is_never"
	chk phases "$ph" "starting running sweeping failed | starting running sweeping failed"
	verdict proc_exit_labels "exit 3 is failed:3 (exit:3), an unrequested SIGKILL is crashed:9 (signal:9); with --restart never both end failed without a restart, and reset clears them"
	canary_note proc_exit_labels

	# an unexpected exit (somebody else kills the main process) is a crash that is restarted, and its label differs from the
	# label of an operator's stop although the signal is the same
	sup_bg "$RC_POLICY" batch/pu -- sleep 4802
	pu=$SUPPID
	waitfor 20 rc_at batch/pu running 1
	waitfor 20 rc_find_is 'sleep 4802 ' 1
	mp="$(rc_sf batch/pu main)"
	kill -TERM "$mp" 2>/dev/null
	waitfor 20 rc_at batch/pu running 2
	waitfor 20 rc_find_is 'sleep 4802 ' 1
	st="$(rc_sf batch/pu phase gen restarts supervisor)"
	mp2="$(rc_sf batch/pu main)"
	x1="$(rc_ev batch/pu exit 'gen=1 ended=crashed:15 status=signal:15 oom=no')"
	x2="$(rc_ev batch/pu restart 'phase=backoff gen=1 crashed:15; restart 1 in ')"
	fresh=no
	if [ "$mp2" != "$mp" ] && rc_pid_ok "$mp2" && rc_in "$mp2" workload/batch/pu; then fresh=yes; fi
	rc stop batch/pu
	rs=$RS
	rc_reap "$pu"
	x3="$(rc_ev batch/pu exit 'gen=2 ended=requested:stop status=signal:15 oom=no')"
	x4="$(rc_ev batch/pu start)"
	fin="$(rc_sf batch/pu phase restarts ended)"
	ph="$(rc_phases batch/pu)"
	seen="$seen $ph"
	chk after_kill "$st" "running 2 1 alive"
	chk new_main "$fresh" yes
	chk crash_exit_event "$x1" 1
	chk crash_restart_event "$x2" 1
	chk stop_rc "$rs $SPRC" "0 0"
	chk stop_exit_event "$x3" 1
	chk start_events "$x4" 2
	chk final "$fin" "stopped 1 requested:stop"
	chk phases "$ph" "starting running sweeping backoff starting running stopping sweeping stopped"
	verdict proc_unexpected "SIGTERM by an outsider: exit gen=1 ended=crashed:15 and one restart (main $mp -> $mp2); the same signal after the operator's stop is ended=requested:stop; phases $ph"
	canary_note proc_unexpected

	# a crash loop is bounded: max restarts, paced by the back-off, then failed, and nothing restarts it any more
	loopf="$T/${RC_P}loop.policy"
	rc_restart "$loopf" on-failure 2 60s 6 100ms 200ms
	: >"$T/${RC_P}cl.starts"
	t0="$(cg_uptime)"
	rc_pol_b 60 "$loopf" supervise batch/cl -- sh -c 'echo s >>"$0"; exit 1' "$T/${RC_P}cl.starts"
	t1="$(cg_uptime)"
	rs=$RS
	ou="$(rc_out)"
	er="$(rcerr)"
	el="$(rc_secs "$t0" "$t1")"
	ns="$(lines "$T/${RC_P}cl.starts")"
	st="$(rc_sf batch/cl phase gen restarts supervisor domain ended status)"
	e1="$(rc_ev batch/cl start)"
	e2="$(rc_ev batch/cl restart)"
	e3="$(rc_ev batch/cl exit 'gen=[0-9]+ ended=failed:1 status=exit:1 oom=no')"
	sleep 1
	ns2="$(lines "$T/${RC_P}cl.starts")"
	chk rc "$rs" 1
	chk stdout "$ou" "phase=failed gen=3 restarts=2 reason=2 restarts within 60s are used up"
	chk stderr "$er" "resctl: lifecycle: batch/cl failed: 2 restarts within 60s are used up"
	chk starts "$ns" 3
	chk starts_a_second_later "$ns2" 3
	chk state "$st" "failed 3 2 none absent failed:1 exit:1"
	chk audit_events "$e1 $e2 $e3" "3 2 3"
	chk paced "$(yn cg_between "$el" 0.25 10)" yes
	chk domain_gone "$(yn rc_gone batch/cl)" yes
	verdict proc_crash_loop "max=2: 3 starts, 2 restarts paced by the back-off ($el s for 100 ms + 200 ms), then phase=failed, rc 1; no start in the second after"
	canary_note proc_crash_loop

	# a failed workload stays failed until an operator resets it, then it can be supervised again
	rc supervise batch/cl -- true
	rs=$RS
	er="$(rcerr)"
	rc reset batch/cl
	rr=$RS
	ro="$(rc_out)"
	rc supervise batch/cl -- true
	r3=$RS
	o3="$(rc_out)"
	ph="$(rc_phases batch/cl)"
	seen="$seen $ph"
	rv="$(rc_ev batch/cl reset 'was failed')"
	chk refused_rc "$rs" 2
	chk refused_says "$(yn like "$er" '*reset batch/cl*first*')" yes
	chk reset "$rr $ro" "0 reset=batch/cl was=failed"
	chk reset_event "$rv" 1
	chk again "$r3 $o3" "0 phase=finished gen=1 restarts=0 reason=the workload ended cleanly"
	chk phases "$ph" "starting running sweeping backoff starting running sweeping backoff starting running sweeping failed | starting running sweeping finished"
	verdict proc_reset_recover "a failed workload is refused (rc 2) until reset; after reset it runs again from gen=1 and finishes; phases $ph"
	canary_note proc_reset_recover

	# mode always counts clean exits as restarts, and a restart loop of clean exits is bounded too
	: >"$T/${RC_P}ca.starts"
	rc_pol_b 60 "$RC_POLICY" supervise batch/ca --restart always -- sh -c 'echo s >>"$0"; exit 0' "$T/${RC_P}ca.starts"
	rs=$RS
	ou="$(rc_out)"
	ns="$(lines "$T/${RC_P}ca.starts")"
	st="$(rc_sf batch/ca phase gen restarts ended status domain)"
	chk rc "$rs" 1
	chk stdout "$ou" "phase=failed gen=4 restarts=3 reason=3 restarts within 60s are used up"
	chk starts "$ns" 4
	chk state "$st" "failed 4 3 clean exit:0 absent"
	verdict proc_restart_always "--restart always: a command that exits 0 is started 4 times (max=3 restarts), then phase=failed"
	canary_note proc_restart_always
	rc reset batch/ca

	# the cap on all restarts of a run holds for a crasher that is too slow for the window to count it
	totf="$T/${RC_P}total.policy"
	rc_restart "$totf" on-failure 2 1s 3 100ms 100ms
	: >"$T/${RC_P}rt.starts"
	t0="$(cg_uptime)"
	rc_pol_b 120 "$totf" supervise batch/rt -- sh -c 'echo s >>"$0"; sleep 1.2; exit 1' "$T/${RC_P}rt.starts"
	t1="$(cg_uptime)"
	rs=$RS
	ou="$(rc_out)"
	el="$(rc_secs "$t0" "$t1")"
	ns="$(lines "$T/${RC_P}rt.starts")"
	st="$(rc_sf batch/rt phase gen restarts domain)"
	chk rc "$rs" 1
	chk stdout "$ou" "phase=failed gen=4 restarts=3 reason=3 restarts since the supervisor started are used up"
	chk starts "$ns" 4
	chk state "$st" "failed 4 3 absent"
	chk lived "$(yn cg_between "$el" 4.8 30)" yes
	verdict proc_restart_total "window 1s, total 3: a crasher that lives 1.2 s per run never fills the window, the total stops it after 4 starts ($el s)"
	canary_note proc_restart_total
	rc reset batch/rt

	# the start rate is limited: the first start over the limit is refused, leaves no record and no domain
	rsd="$T/${RC_P}ratestate"
	rm -rf "$rsd"
	ratef="$T/${RC_P}rate.policy"
	rc_restart "$ratef" on-failure 3 60s 10 100ms 400ms 3 8
	done3=0
	for k in 1 2 3; do
		rcs "$rsd" "$ratef" supervise "batch/r$k" -- true
		[ "$RS" != 0 ] || done3=$((done3 + 1))
	done
	rcs "$rsd" "$ratef" supervise batch/r4 -- true
	rs=$RS
	er="$(rcerr)"
	rcs "$rsd" "$ratef" state batch/r4
	rsr=$RS
	chk first_three "$done3" 3
	chk fourth_rc "$rs" 2
	chk fourth_says "$(yn matches "$er" '3 starts within the last minute: try again in [0-9]+s')" yes
	chk no_record "$rsr" 2
	chk domain_gone "$(yn rc_gone batch/r4)" yes
	verdict proc_start_rate "rate=3: three starts ran, the fourth was refused ($er), with no record and no domain"
	canary_note proc_start_rate
	waitfor 10 rc_no_reapers
	rm -rf "$rsd"

	# the number of workloads that are supervised at a time is limited, and a finished supervisor frees its place
	capf="$T/${RC_P}cap.policy"
	rc_restart "$capf" on-failure 3 60s 10 100ms 400ms 120 2
	sup_bg "$capf" batch/k1 -- sleep 4851
	k1=$SUPPID
	sup_bg "$capf" batch/k2 -- sleep 4852
	k2=$SUPPID
	waitfor 20 rc_phase_is batch/k1 running
	waitfor 20 rc_phase_is batch/k2 running
	rc_pol "$capf" supervise batch/k3 -- true
	rs=$RS
	er="$(rcerr)"
	rc state batch/k3
	rsr=$RS
	gone3="$(yn rc_gone batch/k3)"
	rc stop batch/k1
	rc stop batch/k2
	rc_reap "$k1"
	q1=$SPRC
	rc_reap "$k2"
	q2=$SPRC
	rc_pol "$capf" supervise batch/k3 -- true
	r3=$RS
	chk third_rc "$rs" 2
	chk third_says "$(yn like "$er" '*2 workloads are supervised already*')" yes
	chk no_record "$rsr" 2
	chk domain_gone "$gone3" yes
	chk supervisors "$q1 $q2" "0 0"
	chk after_stop "$r3" 0
	verdict proc_supervise_cap "workloads=2: with two supervised the third was refused ($er), with no record and no domain; after both stopped a third ran"
	canary_note proc_supervise_cap

	# kill: everything in the domain ends, whatever it did to escape; a supervised workload is killed by its supervisor, as the
	# killing phase, and is not restarted
	sup_bg "$RC_POLICY" batch/fk -- sh -c 'for i in 1 2 3 4; do setsid sleep 4821 & done; sleep 4822 & wait'
	fk=$SUPPID
	waitfor 20 rc_find_is 'sleep 482' 5
	before="$(rc_find 'sleep 482')"
	rc kill batch/fk
	rs=$RS
	ou="$(rc_out)"
	rc_reap "$fk"
	waitfor 5 rc_find_is 'sleep 482' 0
	after="$(rc_find 'sleep 482')"
	st="$(rc_sf batch/fk phase ended status supervisor domain)"
	ph="$(rc_phases batch/fk)"
	seen="$seen $ph"
	kk="$(rc_ev batch/fk kill 'phase=killing gen=1 an operator asked for it')"
	chk sleeps_before "$before" 5
	chk rc "$rs" 0
	chk stdout "$ou" "phase=stopped"
	chk supervisor "$SPRC $SPHUNG" "0 no"
	chk sleeps_after "$after" 0
	chk state "$st" "stopped requested:kill signal:9 none absent"
	chk phases "$ph" "starting running killing sweeping stopped"
	chk kill_event "$kk" 1
	verdict proc_forced_kill "kill ended 5 processes (4 of them setsid'd) at once: phases $ph, ended=requested:kill status=signal:9, not restarted"
	canary_note proc_forced_kill

	# stop of a workload that ignores SIGTERM: the kill follows when the grace period is over
	rm -f "$T/${RC_P}fs.ready"
	sup_bg "$RC_POLICY" batch/fs -- sh -c 'trap "" TERM; : >"$0"; while :; do sleep 1; done' "$T/${RC_P}fs.ready"
	fs=$SUPPID
	waitfor 20 rc_ready "$T/${RC_P}fs.ready"
	t0="$(cg_uptime)"
	rc stop batch/fs
	rs=$RS
	t1="$(cg_uptime)"
	ou="$(rc_out)"
	el="$(rc_secs "$t0" "$t1")"
	rc_reap "$fs"
	st="$(rc_sf batch/fs phase ended status supervisor domain)"
	left="$(rc_pid_of "${RC_P}fs.ready" | wc -l)"
	chk rc "$rs" 0
	chk stdout_ends "$(yn like "$ou" 'stop=killed:[1-9]* phase=stopped')" yes
	chk grace "$(yn cg_between "$el" 2 10)" yes
	chk supervisor "$SPRC $SPHUNG" "0 no"
	chk state "$st" "stopped requested:stop signal:9 none absent"
	chk left "$left" 0
	verdict proc_stop_forced "SIGTERM was ignored: $ou after ${el}s (grace 2 s), ended=requested:stop status=signal:9; nothing is left"
	canary_note proc_stop_forced

	# the grace period of a supervised workload is the one of the policy
	sup_bg "$RC_POLICY" batch/gp -- sleep 4803
	gp=$SUPPID
	waitfor 20 rc_at batch/gp running 1
	rc stop batch/gp --grace 1s
	rs=$RS
	er="$(rcerr)"
	st="$(rc_sf batch/gp phase gen)"
	rc stop batch/gp
	r2=$RS
	rc_reap "$gp"
	chk refused_rc "$rs" 2
	chk refused_says "$(yn like "$er" '*is supervised: it is stopped with the grace period of the policy, not with --grace*')" yes
	chk still_running "$st" "running 1"
	chk stop "$r2 $SPRC" "0 0"
	verdict proc_grace_policy "stop --grace of a supervised workload is refused (rc 2: $er) and changes nothing; a plain stop ends it"
	canary_note proc_grace_policy

	# a stop during the back-off ends the workload at once, without another start
	slowf="$T/${RC_P}slow.policy"
	rc_restart "$slowf" on-failure 3 60s 10 5s 5s
	: >"$T/${RC_P}sb.starts"
	sup_bg "$slowf" batch/sb -- sh -c 'echo s >>"$0"; exit 1' "$T/${RC_P}sb.starts"
	sb=$SUPPID
	waitfor 20 rc_phase_is batch/sb backoff
	t0="$(cg_uptime)"
	rc stop batch/sb
	rs=$RS
	t1="$(cg_uptime)"
	ou="$(rc_out)"
	el="$(rc_secs "$t0" "$t1")"
	rc_reap "$sb"
	st="$(rc_sf batch/sb phase ended reason supervisor domain)"
	ns="$(lines "$T/${RC_P}sb.starts")"
	ph="$(rc_phases batch/sb)"
	seen="$seen $ph"
	chk rc "$rs" 0
	chk stdout_ends "$(yn like "$ou" '*phase=stopped')" yes
	chk prompt "$(yn cg_between "$el" 0 3)" yes
	chk supervisor "$SPRC $SPHUNG" "0 no"
	chk state "$st" "stopped failed:1 stopped_during_back-off none absent"
	chk starts "$ns" 1
	chk phases "$ph" "starting running sweeping backoff stopped"
	verdict proc_stop_backoff "a stop 5 s into a back-off ended it in ${el}s: phases $ph, one start only"
	canary_note proc_stop_backoff

	# orphans: what the main process leaves behind in the domain is swept when the run ends, whichever way it got away
	rm -f "$T/${RC_P}og.ready" "$T/${RC_P}oh.ready"
	rc supervise batch/og --restart never -- sh -c '(: >"$0"; exec sleep 4831) & while [ ! -e "$0" ]; do sleep 0.1; done; exit 3' "$T/${RC_P}og.ready"
	rs=$RS
	st="$(rc_sf batch/og phase ended domain)"
	waitfor 5 rc_find_is 'sleep 4831 ' 0
	lg="$(rc_find 'sleep 4831 ')"
	gg="$(yn rc_gone batch/og)"
	rc reset batch/og
	rc supervise batch/oh -- sh -c '(: >"$0"; exec setsid sleep 4832) & while [ ! -e "$0" ]; do sleep 0.1; done; exit 0' "$T/${RC_P}oh.ready"
	r2=$RS
	st2="$(rc_sf batch/oh phase ended domain)"
	waitfor 5 rc_find_is 'sleep 4832 ' 0
	lh="$(rc_find 'sleep 4832 ')"
	gh="$(yn rc_gone batch/oh)"
	chk crashed_rc "$rs" 1
	chk crashed_state "$st" "failed failed:3 absent"
	chk orphan_started "$(yn test -e "$T/${RC_P}og.ready")" yes
	chk orphan_left "$lg" 0
	chk domain_gone "$gg" yes
	chk clean_rc "$r2" 0
	chk clean_state "$st2" "finished clean absent"
	chk setsid_orphan_started "$(yn test -e "$T/${RC_P}oh.ready")" yes
	chk setsid_orphan_left "$lh" 0
	chk setsid_domain_gone "$gh" yes
	verdict proc_orphans "a background sleep started by the main process was swept when the main ended, after a failure and after a clean exit, also one in its own session; the domains are gone"
	canary_note proc_orphans

	# a command that cannot start: no process, no stale domain, no unrecoverable state
	printf '#!/nonexistent/interpreter\n' >"$T/${RC_P}bad.sh"
	chmod 755 "$T/${RC_P}bad.sh"
	dirs0="$(rc_domain_dirs)"
	rc run batch/lf -- "$T/${RC_P}bad.sh"
	rs=$RS
	er="$(rcerr)"
	gl="$(yn rc_gone batch/lf)"
	dirs1="$(rc_domain_dirs)"
	rc state batch/lf
	rsr=$RS
	lf="$(rc_ev batch/lf run 'failed: the command did not start')"
	rc_pol_b 60 "$RC_POLICY" supervise batch/lg -- "$T/${RC_P}bad.sh"
	r2=$RS
	st="$(rc_sf batch/lg phase gen restarts ended status supervisor domain)"
	g2="$(yn rc_gone batch/lg)"
	s4="$(rc_ev batch/lg start)"
	dirs2="$(rc_domain_dirs)"
	rc reset batch/lg
	chk run_rc "$rs" 1
	chk run_says "$(yn like "$er" '*cannot run*')" yes
	chk run_domain_gone "$gl" yes
	chk run_dirs "$dirs1" "$dirs0"
	chk run_no_record "$rsr" 2
	chk run_audit "$lf" 1
	chk supervise_rc "$r2" 1
	chk supervise_state "$st" "failed 4 3 failed:127 exit:127 none absent"
	chk supervise_domain_gone "$g2" yes
	chk supervise_starts "$s4" 4
	chk supervise_dirs "$dirs2" "$dirs0"
	verdict proc_launch_failure "run of a command that cannot start: rc 1 ($er), no domain, no record, audited; supervised: 4 starts (3 restarts) ended failed:127, no domain left"
	canary_note proc_launch_failure

	# a supervisor that is killed: its reaper sweeps the workload and records that the supervisor was lost
	sup_bg "$RC_POLICY" batch/lo -- sleep 4841
	lo=$SUPPID
	waitfor 20 rc_at batch/lo running 1
	waitfor 20 rc_find_is 'sleep 4841 ' 1
	kill -9 "$lo" 2>/dev/null
	wait "$lo" 2>/dev/null
	waitfor 30 rc_phase_is batch/lo failed
	st="$(rc_sf batch/lo phase ended supervisor domain main reason)"
	waitfor 5 rc_find_is 'sleep 4841 ' 0
	left="$(rc_find 'sleep 4841 ')"
	rc supervise batch/lo -- true
	rs=$RS
	er="$(rcerr)"
	rc reset batch/lo
	rr=$RS
	ro="$(rc_out)"
	ph="$(rc_phases batch/lo)"
	seen="$seen $ph"
	chk state "$st" "failed lost none absent - supervisor_lost:_the_workload_was_cleaned_up"
	chk sleeps "$left" 0
	chk refused_rc "$rs" 2
	chk refused_says "$(yn like "$er" '*reset batch/lo*first*')" yes
	chk reset "$rr $ro" "0 reset=batch/lo was=failed"
	verdict proc_lost_reaper "kill -9 of the supervisor: the reaper swept the workload and recorded phase=failed ended=lost (phases $ph); it is refused until reset"
	canary_note proc_lost_reaper

	# a supervisor and its reaper both killed: recover finds the workload, sweeps it and records the same
	sup_bg "$RC_POLICY" batch/lp -- sleep 4842
	lp=$SUPPID
	waitfor 20 rc_at batch/lp running 1
	waitfor 20 rc_find_is 'sleep 4842 ' 1
	reapers="$(rc_pid_of '__reap batch/lp ')"
	for rp in $reapers; do kill -9 "$rp" 2>/dev/null; done
	kill -9 "$lp" 2>/dev/null
	wait "$lp" 2>/dev/null
	sa="$(rc_sf batch/lp phase supervisor)"
	rc list
	listed="$(grep -c '^batch/lp procs=1 state=running$' "$T/rc.out")"
	rc recover
	rs=$RS
	swept="$(grep -c '^swept=batch/lp phase=failed$' "$T/rc.out")"
	st="$(rc_sf batch/lp phase ended supervisor domain)"
	left="$(rc_find 'sleep 4842 ')"
	rc supervise batch/lp -- true
	r2=$RS
	rc reset batch/lp
	ph="$(rc_phases batch/lp)"
	seen="$seen $ph"
	chk reaper_found "$(yn test -n "$reapers")" yes
	chk before_recover "$sa $listed" "running lost 1"
	chk recover_rc "$rs" 0
	chk swept "$swept" 1
	chk state "$st" "failed lost none absent"
	chk sleeps "$left" 0
	chk refused_rc "$r2" 2
	verdict proc_lost_recover "supervisor and reaper killed: the workload was still running (supervisor=lost), recover swept it (swept=batch/lp phase=failed); phases $ph"
	canary_note proc_lost_recover

	# a lifecycle record that cannot be read is quarantined, never trusted and never an obstacle
	printf 'garbage\n' >"$RC_STATE/records/batch.cr"
	rc state
	rs=$RS
	listed="$(grep -c '^batch.cr corrupt=' "$T/rc.out")"
	rc supervise batch/cr -- true
	r2=$RS
	o2="$(rc_out)"
	qe="$(rc_ev batch.cr quarantine)"
	qf="$(ls "$RC_STATE/quarantine" 2>/dev/null | grep -c '^batch\.cr\.')"
	st="$(rc_sf batch/cr phase gen)"
	printf 'garbage\n' >"$RC_STATE/records/batch.cq"
	rc recover
	r3=$RS
	qd="$(grep -c '^quarantined=batch.cq$' "$T/rc.out")"
	qg="$(ls "$RC_STATE/quarantine" 2>/dev/null | grep -c '^batch\.cq\.')"
	rc supervise batch/cq -- true
	r4=$RS
	printf 'garbage\n' >"$RC_STATE/records/batch.cx"
	rc create batch/cx
	r5=$RS
	er="$(rcerr)"
	rc reset batch/cx
	r6=$RS
	o6="$(rc_out)"
	rc create batch/cx
	r7=$RS
	rc remove batch/cx
	r8=$RS
	chk listed "$rs $listed" "0 1"
	chk supervise "$r2 $o2" "0 phase=finished gen=1 restarts=0 reason=the workload ended cleanly"
	chk quarantine_event "$qe" 1
	chk quarantine_file "$qf" 1
	chk new_record "$st" "finished 1"
	chk recover "$r3 $qd $qg" "0 1 1"
	chk after_recover "$r4" 0
	chk create_refused "$r5" 2
	chk create_says "$(yn like "$er" '*unreadable lifecycle record*')" yes
	chk reset "$r6 $o6" "0 reset=batch/cx was=unrecorded"
	chk create_after_reset "$r7 $r8" "0 0"
	verdict proc_corrupt_record "an unreadable record is listed as corrupt, supervise quarantines it and starts clean, recover quarantines another, create is refused over a third until reset removes it"
	canary_note proc_corrupt_record

	# isolation: the end of one workload (a kill, a crash loop) is of no concern to another one
	sup_bg "$RC_POLICY" interactive/ib -- sleep 4861
	ib=$SUPPID
	sup_bg "$RC_POLICY" batch/ia -- sleep 4862
	ia=$SUPPID
	waitfor 20 rc_at interactive/ib running 1
	waitfor 20 rc_at batch/ia running 1
	waitfor 20 rc_find_is 'sleep 4861 ' 1
	waitfor 20 rc_find_is 'sleep 4862 ' 1
	ibm="$(rc_sf interactive/ib main)"
	rc kill batch/ia
	r1=$RS
	rc_reap "$ia"
	rc_pol_b 60 "$RC_POLICY" supervise batch/ic -- sh -c 'exit 1'
	r2=$RS
	sti="$(rc_sf interactive/ib phase gen restarts supervisor main)"
	sta="$(rc_sf batch/ia phase ended)"
	stc="$(rc_sf batch/ic phase ended domain)"
	ibd="$(yn rc_pid_ok "$ibm")"
	rc stop interactive/ib
	r3=$RS
	rc_reap "$ib"
	rc reset batch/ic
	chk kill_rc "$r1" 0
	chk crash_loop_rc "$r2" 1
	chk neighbour "$sti" "running 1 0 alive $ibm"
	chk neighbour_alive "$ibd" yes
	chk killed "$sta" "stopped requested:kill"
	chk crashed "$stc" "failed failed:1 absent"
	chk neighbour_stop "$r3 $SPRC" "0 0"
	verdict proc_isolation "interactive/ib (main $ibm) kept running, gen=1 restarts=0, while batch/ia was killed and batch/ic crash-looped to failed"
	canary_note proc_isolation

	# a workload killed for exceeding its memory limit is a crash that the record says was out of memory
	rc supervise batch/oo --memory-max 32M --restart never -- "$CT" mem 200
	rs=$RS
	st="$(rc_sf batch/oo phase ended status oom restarts)"
	rc reset batch/oo
	chk rc "$rs" 1
	chk state "$st" "failed crashed:9 signal:9 yes 0"
	verdict proc_oom_crash "a 200 MiB allocation under --memory-max 32M: ended=crashed:9 status=signal:9 oom=yes, distinct from ended=requested:kill"
	canary_note proc_oom_crash

	# nothing is left: no workload, no domain, no lifecycle that is not terminal, no process of resctl
	waitfor 10 rc_no_reapers
	rc list
	listed="$(grep -cE '^(batch|interactive)/' "$T/rc.out")"
	dirs="$(rc_domain_dirs)"
	rc state
	nonterm="$(awk '$2 !~ /^phase=(stopped|finished|failed)$/ || $0 !~ / supervisor=none / { n++ } END { print n + 0 }' "$T/rc.out")"
	nstate="$(wc -l <"$T/rc.out" | tr -d ' ')"
	procs="$(rc_pid_of ' --state-dir ' | wc -l)"
	sleeps="$(rc_find 'sleep 48')"
	chk workloads "$listed" 0
	chk domain_dirs "$dirs" "$pdirs"
	chk not_terminal "$nonterm" 0
	chk resctl_processes "$procs" 0
	chk sleeps "$sleeps" 0
	verdict proc_cleanup "after $nstate lifecycle records: no workload is listed, the $dirs agent-interaction directories are the ones from before, every record is terminal with no supervisor, no resctl process and no workload process is left"
	canary_note proc_cleanup

	# the audit trail: complete, ordered, and its phase changes follow the table
	rc audit 2048
	gaps="$(awk 'NR > 1 && $1 != prev + 1 { n++ } { prev = $1 } END { print n + 0 }' "$T/rc.out")"
	odd="$(grep -vcE '^[0-9]+ [0-9]+ [0-9]+:[a-z]+ [a-z]+ [^ ]+' "$T/rc.out")"
	missing="$(awk 'BEGIN { m = split("supervise start intent stop kill exit restart phase reset recover quarantine", w, " ") } { have[$4] = 1 } END { for (i = 1; i <= m; i++) if (!(w[i] in have)) out = out " " w[i]; print out }' "$T/rc.out")"
	nlines="$(wc -l <"$T/rc.out" | tr -d ' ')"
	edges="$(rc_edges)"
	chk sequence_gaps "$gaps" 0
	chk malformed_lines "$odd" 0
	chk events_missing "$missing" ""
	chk order "$pl_order" ok
	chk phase_changes "$edges" ok
	verdict proc_audit "$nlines audit lines: sequence numbers without a gap, every line well formed, supervise/start/intent/stop/kill/exit/restart/phase/reset/recover/quarantine all present, the stop of batch/pl in the order intent -> stop -> exit -> stopped, every change of phase allowed by the lifecycle table"
	canary_note proc_audit

	# every phase of the lifecycle was entered and audited
	lack=
	for w in starting running stopping killing sweeping backoff recovering stopped finished failed; do
		case " $seen " in *" $w "*) ;; *) lack="$lack $w" ;; esac
	done
	chk phases_not_seen "$lack" ""
	verdict proc_phases "starting running stopping killing sweeping backoff recovering stopped finished failed were all entered and audited in the scenarios above"
	rc_scrub
	canary_report system_canary_process
}

# ------------------------------------------------------------------------------------------------ critical processes
# A service entered into system (or registered with protect) is a critical process: it has to stay out of every workload kill
# domain whatever happens to a workload. The service of this section is a shell script that appends the uptime to a file about
# once a second; its pid and start time, its place in system of every hierarchy and its entry in the registry are checked
# after every scenario. A keeper process that resctl did not start stands for a service that was registered with protect.

crit_write_service() {
	cat >"$T/critd" <<'EOF'
#!/bin/sh
beat="$1"
[ -p "$beat.fifo" ] || exit 4
exec 3<>"$beat.fifo"
while :; do
	read up rest </proc/uptime
	echo "$up" >>"$beat"
	read -t 1 x <&3
	[ $? -le 1 ] || exit 3
done
EOF
	chmod 755 "$T/critd"
}
crit_start_time() { sed 's/^.*) //' "/proc/$1/stat" 2>/dev/null | awk '{ print $20 }'; }
crit_beats() { [ -r "$T/critd.beat" ] || { echo 0; return 0; }; echo $(($(wc -l <"$T/critd.beat") + 0)); }
crit_beat_gt() { [ "$(crit_beats)" -gt "$1" ]; }
crit_filled() { [ -s "$1" ]; }
crit_outsider() { exec sleep "$1"; }
crit_extra() { CANARY_EXTRA=$((${CRITD_UP:-0} + ${KEEPER_UP:-0})); }
# rcq: resctl, with its own output files (the ones of rc are clobbered by every helper)
rcq() { "$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" --state-dir "$RC_STATE" "$@" >"$T/rcq.out" 2>"$T/rcq.err"; QS=$?; }

# crit_start: the service, entered into system by resctl (which registers it under its file name)
crit_start() {
	crit_write_service
	rm -f "$T/critd.beat" "$T/critd.beat.fifo"
	: >"$T/critd.beat"
	mkfifo "$T/critd.beat.fifo"
	rcbg enter system -- "$T/critd" "$T/critd.beat" >/dev/null 2>&1 &
	CRITD=$!
	CRITD_UP=1
	crit_extra
	waitfor 10 crit_beat_gt 0
	CRITD_START="$(crit_start_time "$CRITD")"
}
# crit_keeper: a process that resctl did not start; the caller registers it with protect
crit_keeper() {
	crit_outsider 4900 &
	KEEPER=$!
	KEEPER_START="$(crit_start_time "$KEEPER")"
}

# crit_check_one NAME PID START: alive, the same process as before, in system of every hierarchy, listed live and placed in
# system (in $T/rcq.out); the problems are added to cr_bad
crit_check_one() {
	co_st="$(rc_state "$2")"
	case "$co_st" in
	gone | Z | X)
		cr_bad="$cr_bad $1:$co_st"
		return 0
		;;
	esac
	[ "$(crit_start_time "$2")" = "$3" ] || cr_bad="$cr_bad $1:restarted"
	rc_in "$2" system || cr_bad="$cr_bad $1:not-in-system"
	grep -qE "^[A-Za-z0-9_-]+ pid=$2 state=live placed=system start=$3\$" "$T/rcq.out" || cr_bad="$cr_bad $1:not-listed-live"
	return 0
}
# crit_state: ok, or what is wrong with the services (use it in $(...))
crit_state() {
	cr_bad=
	rcq critical
	[ "$QS" = 0 ] || cr_bad=" critical_rc=$QS"
	crit_check_one critd "$CRITD" "$CRITD_START"
	if [ "${KEEPER_UP:-0}" = 1 ]; then crit_check_one keeper "$KEEPER" "$KEEPER_START"; fi
	echo "${cr_bad:-ok}"
}
# crit_mv PID REL: put a process into agent-interaction/REL of every hierarchy, the way a mistake would
crit_mv() {
	for cr_r in $(rc_roots); do
		{ echo "$1" >"$cr_r/agent-interaction/$2/cgroup.procs"; } 2>/dev/null
	done
}
# crit_misplace REL [critd]: put the services (only critd with the argument) into the kill domain agent-interaction/REL and
# check that the registry sees them there; prints ok, or the problems (use it in $(...))
crit_misplace() {
	cm_bad=
	crit_mv "$CRITD" "$1"
	rc_in "$CRITD" "$1" || cm_bad="$cm_bad critd-not-moved"
	if [ "${2:-}" != critd ] && [ "${KEEPER_UP:-0}" = 1 ]; then
		crit_mv "$KEEPER" "$1"
		rc_in "$KEEPER" "$1" || cm_bad="$cm_bad keeper-not-moved"
	fi
	rcq critical
	grep -qE "^critd pid=$CRITD state=live placed=workload:${1#workload/} " "$T/rcq.out" || cm_bad="$cm_bad critd-not-listed-in-the-workload"
	echo "${cm_bad:-ok}"
}
# crit_sup WORKLOAD SECONDS [--restart never]: a supervised `sleep SECONDS`, running; its supervisor is CS_PID
crit_sup() {
	cs_w="$1"
	cs_n="$2"
	shift 2
	sup_bg "$RC_POLICY" "$cs_w" "$@" -- sleep "$cs_n"
	CS_PID=$SUPPID
	waitfor 20 rc_phase_is "$cs_w" running
	waitfor 20 rc_find_is "sleep $cs_n " 1
}
# crit_unmanaged WORKLOAD SECONDS: `sleep SECONDS` run by `resctl run` (it is CRIT_PJ), the domain has it
crit_unmanaged() {
	rcbg run "$1" -- sleep "$2" >/dev/null 2>&1 &
	CRIT_PJ=$!
	waitfor 10 rc_has "$1" 1 running
}
# crit_drop WORKLOAD [PID]: remove the workload, with what is left in it, and reap the job that ran in it (bounded)
crit_drop() {
	rc remove "$1"
	CD_RS=$RS
	[ -z "${2:-}" ] || rc_reap "$2" 10
}
# crit_rescues PID REL [ACTOR]: how often the audit trail says that the process was moved out of agent-interaction/REL
crit_rescues() {
	rc audit 2048
	grep -cE "^[0-9]+ [0-9]+ [0-9]+:${3:-[a-z]+} rescue pid$1 found in agent-interaction/$2 moved=true\$" "$T/rc.out"
}
# crit_rescued REL [ACTOR]: "N N", the rescues of critd and of the keeper
crit_rescued() { echo "$(crit_rescues "$CRITD" "$1" "${2:-}") $(crit_rescues "$KEEPER" "$1" "${2:-}")"; }
# crit_try_protect PID: the operator's protect of a process that runs in a workload, by name and without, is refused
crit_try_protect() {
	rc protect "$1"
	chk "plain_$1" "$RS $(yn like "$(rcerr)" '*runs in the workload*a workload process is never protected*')" "2 yes"
	rc protect "$1" --name evil
	chk "named_$1" "$RS $(yn like "$(rcerr)" '*runs in the workload*a workload process is never protected*')" "2 yes"
}
# crit_chk LABEL: the services are as they were; when they are not, they are put back so that the next scenario is its own
crit_chk() {
	cr_st="$(crit_state)"
	chk "$1" "$cr_st" ok
	[ "$cr_st" = ok ] || crit_heal
}
crit_heal() {
	CRIT_RESTARTS=$((CRIT_RESTARTS + 1))
	if rc_alive "$CRITD"; then
		crit_mv "$CRITD" system
		rc protect "$CRITD" --name critd
	else
		rc_reap "$CRITD" 10
		crit_start
		CRIT_BEAT=0
	fi
	if [ "${KEEPER_UP:-0}" = 1 ]; then
		if rc_alive "$KEEPER"; then
			crit_mv "$KEEPER" system
			rc protect "$KEEPER" --name keeper
		else
			rc_reap "$KEEPER" 10
			crit_keeper
			rc protect "$KEEPER" --name keeper
		fi
	fi
}
# crit_snap: add the audit lines since the last snapshot to $T/crit.audit (never use it in $(...))
crit_snap() {
	rcq audit 2048
	awk -v last="$CRIT_SEQ" '$1 + 0 > last + 0' "$T/rcq.out" >>"$T/crit.audit"
	CRIT_SEQ="$(awk -v last="$CRIT_SEQ" '{ if ($1 + 0 > s) s = $1 + 0 } END { if (s < last + 0) s = last + 0; print s + 0 }' "$T/crit.audit")"
}
# crit_count EVENT SUBJECT [DETAIL-ERE]: lines of the snapshot
crit_count() {
	if [ -n "${3:-}" ]; then
		grep -cE "^[0-9]+ [0-9]+ [0-9]+:[a-z]+ $1 $2 $3" "$T/crit.audit"
	else
		grep -cE "^[0-9]+ [0-9]+ [0-9]+:[a-z]+ $1 $2( |\$)" "$T/crit.audit"
	fi
}
# crit_verdict FACT TEXT: the services are as they were (and beat since the last fact), the fact is ok with TEXT when nothing
# differed, and the canary of the system domain is still where it was
crit_verdict() {
	waitfor 10 crit_beat_gt "$CRIT_BEAT"
	cr_st="$(crit_state)"
	chk critical_services "$cr_st" ok
	[ "$cr_st" = ok ] || crit_heal
	chk heartbeat "$(yn crit_beat_gt "$CRIT_BEAT")" yes
	verdict "$1" "$2"
	CRIT_BEAT="$(crit_beats)"
	crit_snap
	canary_note "$1"
}
# crit_refusals FILE WORKLOAD: the lines "LABEL rc=N message" of the requests a workload made (crit_write_inside): each must
# be refused with rc 2 by the guard that names the request and the workload; prints "requests refused problems"
crit_refusals() {
	awk -v w="$2" '
		/^ro:/ { next }
		{
			cmd = $1
			sub(/-.*$/, "", cmd)
			n++
			if ($2 == "rc=2" && index($0, "resctl: refused: " cmd " is refused inside the workload " w ": a workload does not control workloads or protected processes") > 0) {
				ok++
			} else {
				bad = bad " " $1
			}
		}
		END {
			if (bad == "") bad = "-"
			print n + 0, ok + 0, bad
		}
	' "$1"
}
# crit_write_inside: the script a workload runs to ask for what it must not get; one line per request in its output file
crit_write_inside() {
	cat >"$T/crit_inside.sh" <<'EOF'
out="$1"; bin="$2"; pol="$3"; lc="$4"; st="$5"; crit="$6"; other="$7"
: >"$out"
try() {
	l="$1"; shift
	"$bin" --policy "$pol" --lifecycle "$lc" --state-dir "$st" "$@" >"$out.o" 2>"$out.e"
	r=$?
	echo "$l rc=$r $(head -n 1 "$out.e" | cut -c1-240)" >>"$out"
}
try protect-self protect $$
try protect-critd protect "$crit"
try protect-name protect "$crit" --name evil
try unprotect-critd unprotect "$crit"
try enter-system enter system -- true
try kill-other kill "$other"
try stop-other stop "$other"
try remove-other remove "$other"
try freeze-other freeze "$other"
try thaw-other thaw "$other"
try reset-other reset "$other"
try supervise-new supervise batch/kss -- true
try recover recover
try teardown teardown
try init init
try create-new create batch/kcr
try run-new run batch/krn -- true
try ro:critical critical
try ro:list list
try ro:state state
try ro:audit audit 3
try ro:status status "$other"
exit 0
EOF
}
# crit_write_ud: the script of a workload that runs as nobody (policy uid/gid): what can it do to system and the registry
crit_write_ud() {
	cat >"$T/crit_ud.sh" <<'EOF'
out="$1"; bin="$2"; pol="$3"; st="$4"; crit="$5"; shift 5
: >"$out"
echo "ids uid=$(id -u) gid=$(id -g)" >>"$out"
before="$(cat /proc/self/cgroup)"
for r in "$@"; do
	if { echo "$$" >"$r/agent-interaction/system/cgroup.procs"; } 2>/dev/null; then
		echo "system-write-ok $r" >>"$out"
	else
		echo "system-write-denied $r" >>"$out"
	fi
done
after="$(cat /proc/self/cgroup)"
if [ "$before" = "$after" ]; then echo cgroup-unchanged >>"$out"; else echo cgroup-CHANGED >>"$out"; fi
if [ -w "$st/critical" ]; then echo registry-write-ok >>"$out"; else echo registry-write-denied >>"$out"; fi
if ls "$st" >/dev/null 2>&1; then echo state-list-ok >>"$out"; else echo state-list-denied >>"$out"; fi
"$bin" --policy "$pol" --state-dir "$st" protect "$crit" >"$out.p" 2>&1
rcp=$?
echo "protect rc=$rcp $(head -n 1 "$out.p" | cut -c1-240)" >>"$out"
"$bin" --policy "$pol" --state-dir "$st" critical >"$out.p" 2>&1
rcp=$?
echo "critical rc=$rcp $(head -n 1 "$out.p" | cut -c1-240)" >>"$out"
{ kill -9 "$crit"; } 2>/dev/null
rcp=$?
echo "kill-critical rc=$rcp" >>"$out"
exit 0
EOF
}
# cr_scrub: nothing of this section is left (the services are ended before)
cr_scrub() {
	for cr_p in $(rc_pid_of ' --state-dir '); do kill -9 "$cr_p" 2>/dev/null; done
	for cr_g in 491 492 493 494 495 496 497; do
		for cr_p in $(rc_pid_of "sleep $cr_g"); do kill -9 "$cr_p" 2>/dev/null; done
	done
	rc_scrub
}

rc_s_critical() {
	section critical || return 0
	CRIT_RESTARTS=0
	CRIT_BEAT=0
	KEEPER_UP=0
	KEEPER=
	CRITD_UP=0
	CANARY_EXTRA=0
	cr_pes='process\(es\)'
	rc recover
	cr_dirs="$(rc_domain_dirs)"
	rc audit 1
	cr_seq0="$(awk '{ s = $1 } END { print s + 0 }' "$T/rc.out")"
	CRIT_SEQ=$cr_seq0
	: >"$T/crit.audit"
	CANARY_START="$(crit_start_time "$CANARY")"
	crit_start
	CRIT_FIRST="$CRITD $CRITD_START"
	CRIT_T0="$(cg_uptime)"

	# a service entered into system is a critical process: placed in system, listed live in the registry, audited
	rcq critical
	chk critical_rc "$QS" 0
	chk critd_line "$(grep -cE "^critd pid=$CRITD state=live placed=system start=$CRITD_START\$" "$T/rcq.out")" 1
	chk canary_line "$(grep -cE "^sleep pid=$CANARY state=live placed=system start=$CANARY_START\$" "$T/rcq.out")" 1
	chk live "$(grep -c ' state=live ' "$T/rcq.out")" 2
	chk stale "$(grep -c ' state=stale ' "$T/rcq.out")" 0
	chk critd_in_system "$(yn rc_in "$CRITD" system)" yes
	chk enter_audited "$(rc_ev system enter "cmd=$T/critd")" 1
	chk protect_audited "$(rc_ev critd protect "pid=$CRITD( |\$)")" 1
	crit_verdict crit_register "enter system registered the service under its file name (critd pid $CRITD, start $CRITD_START): live and placed=system in the registry, in system of every hierarchy, enter and protect audited, heartbeat running"

	# protect registers a process that resctl did not start: it is moved into system; a protect again keeps the name
	crit_keeper
	cr_out="$(yn rc_in "$KEEPER" system)"
	rc protect "$KEEPER"
	cr_r1="$RS $(rc_out)"
	if [ "$RS" = 0 ]; then
		KEEPER_UP=1
		crit_extra
	fi
	rc protect "$KEEPER" --name keeper
	cr_r2="$RS $(rc_out)"
	rc protect "$KEEPER"
	cr_r3="$RS $(rc_out)"
	rcq critical
	chk outsider_outside "$cr_out" no
	chk protect_new "$cr_r1" "0 protected=pid$KEEPER pid=$KEEPER placed=system moved=yes"
	chk protect_name "$cr_r2" "0 protected=keeper pid=$KEEPER placed=system moved=no"
	chk protect_again "$cr_r3" "0 protected=keeper pid=$KEEPER placed=system moved=no"
	chk keeper_line "$(grep -cE "^keeper pid=$KEEPER state=live placed=system start=$KEEPER_START\$" "$T/rcq.out")" 1
	chk live "$(grep -c ' state=live ' "$T/rcq.out")" 3
	chk stale "$(grep -c ' state=stale ' "$T/rcq.out")" 0
	chk keeper_in_system "$(yn rc_in "$KEEPER" system)" yes
	chk protect_audit "$(rc_ev "pid$KEEPER" protect "pid=$KEEPER( |\$)") $(rc_ev keeper protect "pid=$KEEPER( |\$)")" "1 2"
	KEEPER_FIRST="$KEEPER $KEEPER_START"
	crit_verdict crit_protect "protect moved the outsider $KEEPER into system and registered it (pid$KEEPER, then keeper); a protect again kept the name; the registry lists 3 live and 0 stale processes, three protects audited"

	# every way to end a workload leaves the critical processes alone
	crit_sup batch/ks1 4911
	cr_sp=$CS_PID
	rc stop batch/ks1
	rs=$RS
	ou="$(rc_out)"
	rc_reap "$cr_sp" 20
	st="$(rc_sf batch/ks1 phase ended status)"
	ph="$(rc_phases batch/ks1)"
	chk ks1_stop "$rs $ou" "0 stop=graceful phase=stopped"
	chk ks1_supervisor "$SPRC $SPHUNG" "0 no"
	chk ks1_state "$st" "stopped requested:stop signal:15"
	chk ks1_phases "$ph" "starting running stopping sweeping stopped"
	crit_chk after_supervised_stop

	crit_sup batch/ks2 4912
	cr_sp=$CS_PID
	rc kill batch/ks2
	rs=$RS
	ou="$(rc_out)"
	rc_reap "$cr_sp" 20
	st="$(rc_sf batch/ks2 phase ended status)"
	ph="$(rc_phases batch/ks2)"
	chk ks2_kill "$rs $ou" "0 phase=stopped"
	chk ks2_supervisor "$SPRC $SPHUNG" "0 no"
	chk ks2_state "$st" "stopped requested:kill signal:9"
	chk ks2_phases "$ph" "starting running killing sweeping stopped"
	crit_chk after_supervised_kill

	crit_unmanaged batch/ku1 4913
	cr_pj=$CRIT_PJ
	rc kill batch/ku1
	rs=$RS
	rc_reap "$cr_pj" 10
	chk ku1_kill "$rs $SPHUNG" "0 no"
	crit_drop batch/ku1
	crit_chk after_unmanaged_kill

	crit_unmanaged batch/ku2 4914
	cr_pj=$CRIT_PJ
	rc stop batch/ku2
	rs=$RS
	ou="$(rc_out)"
	rc_reap "$cr_pj" 10
	chk ku2_stop "$rs $ou $SPHUNG" "0 stop=graceful no"
	crit_drop batch/ku2
	crit_chk after_unmanaged_stop

	crit_unmanaged batch/ku3 4915
	cr_pj=$CRIT_PJ
	rc remove batch/ku3
	rs=$RS
	rc_reap "$cr_pj" 10
	chk ku3_remove "$rs $SPHUNG" "0 no"
	chk ku3_domain_gone "$(yn rc_gone batch/ku3)" yes
	crit_chk after_remove

	rm -f "$T/${RC_P}ku4.ready"
	rcbg run batch/ku4 -- sh -c 'trap "" TERM; : >"$0"; while :; do sleep 1; done' "$T/${RC_P}ku4.ready" >/dev/null 2>&1 &
	cr_pj=$!
	waitfor 10 rc_ready "$T/${RC_P}ku4.ready"
	rc stop batch/ku4 --grace 2s
	rs=$RS
	ou="$(rc_out)"
	rc_reap "$cr_pj" 10
	chk ku4_stop "$rs $(yn like "$ou" 'stop=killed:[1-9]*') $SPHUNG" "0 yes no"
	crit_drop batch/ku4
	crit_chk after_forced_stop

	chk sleeps_left "$(rc_find 'sleep 491')" 0
	chk domains "$(rc_domain_dirs)" "$cr_dirs"
	crit_verdict crit_terminate "supervised stop and kill, unmanaged kill, stop, remove and a stop that had to be forced (SIGTERM ignored): every workload ended, critd $CRITD and the keeper $KEEPER kept their pid and start time, stayed in system and listed live; no domain is left"

	# a workload that crashes, is killed by someone else, runs out of memory or forks without end leaves them alone
	rc_pol_b 60 "$RC_POLICY" supervise batch/kc1 -- sh -c 'exit 1'
	rs=$RS
	ou="$(rc_out)"
	rc reset batch/kc1
	chk kc1_loop "$rs $ou" "1 phase=failed gen=4 restarts=3 reason=3 restarts within 60s are used up"
	crit_chk after_crash_loop

	crit_sup batch/kc2 4917 --restart never
	cr_sp=$CS_PID
	cr_m="$(rc_sf batch/kc2 main)"
	kill -9 "$cr_m" 2>/dev/null
	rc_reap "$cr_sp" 20
	cr_hung=$SPHUNG
	st="$(rc_sf batch/kc2 phase ended status restarts)"
	rc reset batch/kc2
	chk kc2_state "$st" "failed crashed:9 signal:9 0"
	chk kc2_supervisor "$cr_hung" no
	crit_chk after_outside_sigkill

	rc supervise batch/kc3 --memory-max 32M --restart never -- "$CT" mem 200
	rs=$RS
	st="$(rc_sf batch/kc3 phase ended status oom restarts)"
	rc reset batch/kc3
	chk kc3_rc "$rs" 1
	chk kc3_state "$st" "failed crashed:9 signal:9 yes 0"
	crit_chk after_oom

	rcbg run batch/kc4 --pids-max 32 -- "$CT" fork 500 >"$T/${RC_P}kc4.out" 2>&1 &
	cr_pj=$!
	rc_reap "$cr_pj" 30
	cr_hung=$SPHUNG
	cr_n="$(sed -n 's/^forked=\([0-9]*\) of 500 (refused)$/\1/p' "$T/${RC_P}kc4.out")"
	cr_ref="$(rc_val batch/kc4 pids.refused)"
	cr_pk=bad
	if [ -n "$cr_n" ] && [ "$cr_n" -le 32 ] && [ "${cr_ref:-0}" -ge 1 ]; then cr_pk=ok; fi
	rc kill batch/kc4
	crit_drop batch/kc4
	chk kc4_parent "$cr_hung" no
	chk kc4_contained "$cr_pk" ok
	crit_chk after_fork_bomb
	chk domains "$(rc_domain_dirs)" "$cr_dirs"
	crit_verdict crit_crash "a crash loop (3 restarts, then failed), an outsider's SIGKILL (crashed:9), an out-of-memory kill (oom=yes) and a fork bomb stopped by the pids limit (${cr_n:-?} of 500 forked, ${cr_ref:-?} refused): the services were never touched"

	# a critical process that was put into a workload domain by mistake is moved back into system before anything in that
	# domain is ended, whoever ends it: the unmanaged kill, stop and remove, a supervised stop, and the sweep a supervisor makes
	# when its workload ends by itself
	crit_unmanaged batch/kmm1 4921
	cr_pj=$CRIT_PJ
	cr_mp="$(crit_misplace workload/batch/kmm1)"
	rc kill batch/kmm1
	rs=$RS
	rc_reap "$cr_pj" 10
	cr_hung=$SPHUNG
	cr_rs="$(crit_rescued workload/batch/kmm1)"
	chk kmm1_placed "$cr_mp" ok
	chk kmm1_kill "$rs $cr_hung" "0 no"
	chk kmm1_rescues "$cr_rs" "1 1"
	crit_chk after_misplaced_kill
	crit_drop batch/kmm1

	crit_unmanaged batch/kmm2 4922
	cr_pj=$CRIT_PJ
	cr_mp="$(crit_misplace workload/batch/kmm2)"
	rc stop batch/kmm2
	rs=$RS
	ou="$(rc_out)"
	rc_reap "$cr_pj" 10
	cr_hung=$SPHUNG
	cr_rs="$(crit_rescued workload/batch/kmm2)"
	chk kmm2_placed "$cr_mp" ok
	chk kmm2_stop "$rs $ou $cr_hung" "0 stop=graceful no"
	chk kmm2_rescues "$cr_rs" "1 1"
	crit_chk after_misplaced_stop
	crit_drop batch/kmm2

	crit_unmanaged batch/kmm3 4923
	cr_pj=$CRIT_PJ
	cr_mp="$(crit_misplace workload/batch/kmm3)"
	rc remove batch/kmm3
	rs=$RS
	rc_reap "$cr_pj" 10
	cr_hung=$SPHUNG
	cr_rs="$(crit_rescued workload/batch/kmm3)"
	chk kmm3_placed "$cr_mp" ok
	chk kmm3_remove "$rs $cr_hung" "0 no"
	chk kmm3_rescues "$cr_rs" "1 1"
	chk kmm3_domain_gone "$(yn rc_gone batch/kmm3)" yes
	crit_chk after_misplaced_remove

	crit_sup batch/kmm4 4924 --restart never
	cr_sp=$CS_PID
	cr_mp="$(crit_misplace workload/batch/kmm4)"
	rc stop batch/kmm4
	rs=$RS
	ou="$(rc_out)"
	rc_reap "$cr_sp" 20
	cr_hung=$SPHUNG
	cr_rc=$SPRC
	st="$(rc_sf batch/kmm4 phase ended)"
	cr_rs="$(crit_rescued workload/batch/kmm4)"
	chk kmm4_placed "$cr_mp" ok
	chk kmm4_stop "$rs $ou" "0 stop=graceful phase=stopped"
	chk kmm4_supervisor "$cr_rc $cr_hung" "0 no"
	chk kmm4_state "$st" "stopped requested:stop"
	chk kmm4_rescues "$cr_rs" "1 1"
	crit_chk after_misplaced_supervised_stop

	rm -f "$T/${RC_P}m5.go" "$T/${RC_P}m5.child"
	sup_bg "$RC_POLICY" batch/kmm5 --restart never -- sh -c 'sleep 4925 & echo $! >"$1"; while [ ! -f "$0" ]; do sleep 0.2; done' "$T/${RC_P}m5.go" "$T/${RC_P}m5.child"
	cr_sp=$SUPPID
	waitfor 20 rc_phase_is batch/kmm5 running
	waitfor 10 crit_filled "$T/${RC_P}m5.child"
	cr_mp="$(crit_misplace workload/batch/kmm5)"
	: >"$T/${RC_P}m5.go"
	rc_reap "$cr_sp" 60
	cr_hung=$SPHUNG
	cr_rc=$SPRC
	st="$(rc_sf batch/kmm5 phase ended status)"
	cr_child="$(cat "$T/${RC_P}m5.child")"
	cr_rs="$(crit_rescued workload/batch/kmm5 super)"
	chk kmm5_placed "$cr_mp" ok
	chk kmm5_supervisor "$cr_rc $cr_hung" "0 no"
	chk kmm5_state "$st" "finished clean exit:0"
	chk kmm5_neighbour "$(yn rc_alive "$cr_child")" no
	chk kmm5_rescues "$cr_rs" "1 1"
	crit_chk after_misplaced_sweep
	chk sleeps_left "$(rc_find 'sleep 492')" 0
	chk domains "$(rc_domain_dirs)" "$cr_dirs"
	crit_verdict crit_misplaced "critd and the keeper were put into the domain of the workload: a kill, a stop, a remove, a supervised stop and the supervisor's own sweep at the end of a run each moved them back into system first (one rescue audited per process and scenario) and killed only the neighbour"

	# a freeze of a domain that holds a critical process does not freeze it
	crit_unmanaged batch/kfz 4926
	cr_pj=$CRIT_PJ
	cr_mp="$(crit_misplace workload/batch/kfz)"
	cr_b0="$(crit_beats)"
	rc freeze batch/kfz
	rs=$RS
	cr_fs="$(rc_val batch/kfz state) $(rc_val batch/kfz procs)"
	waitfor 10 crit_beat_gt "$cr_b0"
	cr_hb="$(yn crit_beat_gt "$cr_b0")"
	cr_fz="$(crit_state)"
	rc thaw batch/kfz
	r2=$RS
	rc kill batch/kfz
	rc_reap "$cr_pj" 10
	cr_hung=$SPHUNG
	cr_rs="$(crit_rescued workload/batch/kfz)"
	chk kfz_placed "$cr_mp" ok
	chk kfz_freeze "$rs" 0
	chk kfz_frozen "$cr_fs" "frozen 1"
	chk kfz_services_while_frozen "$cr_fz" ok
	chk kfz_heartbeat_while_frozen "$cr_hb" yes
	chk kfz_thaw "$r2" 0
	chk kfz_neighbour "$cr_hung" no
	chk kfz_rescues "$cr_rs" "1 1"
	crit_chk after_freeze
	crit_drop batch/kfz
	crit_verdict crit_freeze "freeze of a domain with critd and the keeper in it moved them into system first: the domain was frozen with the neighbour only (state=frozen procs=1) and the heartbeat went on while it was frozen"

	# a workload cannot ask for any of it: every request that changes a workload or the registry is refused inside a workload
	# (an unmanaged one and a supervised one), audited, and changes nothing; the requests that only read work
	crit_write_inside
	crit_unmanaged batch/kother 4931
	cr_po=$CRIT_PJ
	rcq critical
	cp "$T/rcq.out" "$T/crit.before"
	rm -f "$T/${RC_P}cin1.out" "$T/${RC_P}cin2.out"
	rcbg run batch/kin1 -- sh "$T/crit_inside.sh" "$T/${RC_P}cin1.out" "$RC_BIN" "$RC_POLICY" "$RC_LC" "$RC_STATE" "$CRITD" batch/kother >/dev/null 2>&1 &
	cr_pi=$!
	rc_reap "$cr_pi" 60
	chk in1_script "$SPHUNG" no
	sup_bg "$RC_POLICY" batch/kin2 --restart never -- sh "$T/crit_inside.sh" "$T/${RC_P}cin2.out" "$RC_BIN" "$RC_POLICY" "$RC_LC" "$RC_STATE" "$CRITD" batch/kother
	cr_sp=$SUPPID
	rc_reap "$cr_sp" 60
	chk in2_supervisor "$SPHUNG" no
	chk in1_refused "$(crit_refusals "$T/${RC_P}cin1.out" batch/kin1)" "17 17 -"
	chk in2_refused "$(crit_refusals "$T/${RC_P}cin2.out" batch/kin2)" "17 17 -"
	chk in1_reads "$(grep -cE '^ro:[a-z]+ rc=0( |$)' "$T/${RC_P}cin1.out")" 5
	chk in2_reads "$(grep -cE '^ro:[a-z]+ rc=0( |$)' "$T/${RC_P}cin2.out")" 5
	rcq critical
	chk registry_unchanged "$(yn cmp -s "$T/crit.before" "$T/rcq.out")" yes
	chk no_evil "$(grep -c '^evil ' "$T/rcq.out")" 0
	chk other_workload "$(rc_val batch/kother procs) $(rc_val batch/kother state)" "1 running"
	chk other_process "$(rc_find 'sleep 4931 ')" 1
	rc list
	chk nothing_created "$(grep -cE '^batch/(kss|kcr|krn) ' "$T/rc.out")" 0
	rc kill batch/kother
	crit_drop batch/kother "$cr_po"
	chk kother_gone "$SPHUNG" no
	rc remove batch/kin1
	crit_verdict crit_no_request "17 requests to protect, unprotect, enter system, stop, kill, remove, freeze, thaw, reset, supervise, recover, teardown, init, create and run, made from inside an unmanaged and a supervised workload, were all refused (rc 2, naming the request and the workload) and the 5 read-only requests worked; the registry, the other workload and the domains are unchanged"

	# nothing a workload runs can become protected, not even when the operator asks: by name or without, its main process, a
	# child, the process of an unmanaged workload
	rm -f "$T/${RC_P}kp1.child"
	sup_bg "$RC_POLICY" batch/kp1 --restart never -- sh -c 'sleep 4941 & echo $! >"$0"; wait' "$T/${RC_P}kp1.child"
	cr_sp=$SUPPID
	waitfor 20 rc_phase_is batch/kp1 running
	waitfor 10 crit_filled "$T/${RC_P}kp1.child"
	cr_m="$(rc_sf batch/kp1 main)"
	cr_c="$(cat "$T/${RC_P}kp1.child")"
	crit_unmanaged batch/kp2 4943
	cr_pj=$CRIT_PJ
	rcq critical
	cp "$T/rcq.out" "$T/crit.before"
	crit_try_protect "$cr_m"
	crit_try_protect "$cr_c"
	crit_try_protect "$cr_pj"
	cr_au=
	for cr_x in "$cr_m" "$cr_c" "$cr_pj"; do
		cr_au="$cr_au $(rc_ev protect refuse "(refused: )?process $cr_x runs in the workload")"
	done
	cr_au="${cr_au# }"
	rcq critical
	chk registry_unchanged "$(yn cmp -s "$T/crit.before" "$T/rcq.out")" yes
	chk refusals_audited "$cr_au" "2 2 2"
	chk processes_stay "$(yn rc_in "$cr_m" workload/batch/kp1) $(yn rc_in "$cr_c" workload/batch/kp1) $(yn rc_in "$cr_pj" workload/batch/kp2)" "yes yes yes"
	chk no_pid_listed "$(grep -cE "pid=($cr_m|$cr_c|$cr_pj) |^evil " "$T/rcq.out")" 0
	rc stop batch/kp1
	rc_reap "$cr_sp" 20
	chk kp1_stop "$SPHUNG" no
	rc kill batch/kp2
	crit_drop batch/kp2 "$cr_pj"
	chk kp2_gone "$SPHUNG" no
	crit_verdict crit_no_obtain "protect, with a name and without, of the main process ($cr_m), a child ($cr_c) and the process of an unmanaged workload ($cr_pj) was refused 6 times (rc 2, a workload process is never protected) and audited; none of them is in the registry, all stay in their domains"

	# a policy cannot grant protected status: sections and keys that name it are refused wherever they are, and nothing is
	# started, created or registered under such a policy
	cr_pb="$T/${RC_P}pol"
	{ cat "$RC_POLICY"; printf '\n[protect]\npid = 1\n'; } >"$cr_pb.protect"
	{ cat "$RC_POLICY"; printf '\n[protected]\npid = 1\n'; } >"$cr_pb.protected"
	{ cat "$RC_POLICY"; printf '\n[critical.workloads]\nname = x\n'; } >"$cr_pb.critical"
	{ cat "$RC_POLICY"; printf '\n[class.extra]\ncpu.weight = 50\nprotected = yes\n\n[limits.extra]\nmemory.max = 64M\npids.max = 32\n'; } >"$cr_pb.classx"
	{ cat "$RC_POLICY"; printf '\n[class.extra]\ncpu.weight = 50\n\n[limits.extra]\nmemory.max = 64M\npids.max = 32\nprotect = yes\n'; } >"$cr_pb.limx"
	{ cat "$RC_POLICY"; printf '\n[class.extra]\ncpu.weight = 50\n\n[limits.extra]\nmemory.max = 64M\npids.max = 32\n'; } >"$cr_pb.ctl"
	awk '{ print } /^\[class\.batch\]$/ { print "protected = yes" }' "$RC_POLICY" >"$cr_pb.ins1"
	awk '{ print } /^\[system\]$/ { print "critical = yes" }' "$RC_POLICY" >"$cr_pb.ins2"
	awk '{ print } /^\[workload\]$/ { print "protect = yes" }' "$RC_POLICY" >"$cr_pb.ins3"
	cr_pbad=
	for cr_v in protect protected critical classx limx ins1 ins2 ins3; do
		rc_pol "$cr_pb.$cr_v" policy
		if [ "$RS" != 2 ] || ! grep -q 'a policy cannot grant protected status' "$T/rc.err"; then cr_pbad="$cr_pbad $cr_v:rc=$RS"; fi
	done
	chk variants_refused "${cr_pbad:-ok}" ok
	rc_pol "$cr_pb.ctl" policy
	chk control_policy "$RS" 0
	rc_pol "$RC_POLICY" policy
	chk base_policy "$RS" 0
	cr_d0="$(rc_domain_dirs)"
	rcq critical
	cp "$T/rcq.out" "$T/crit.before"
	rc_pol "$cr_pb.ins3" init
	chk init_refused "$RS $(yn grep -q 'a policy cannot grant protected status' "$T/rc.err")" "2 yes"
	rc_pol "$cr_pb.protect" supervise batch/kpl --restart never -- true
	chk supervise_refused "$RS $(yn grep -q 'a policy cannot grant protected status' "$T/rc.err")" "2 yes"
	rc_pol "$cr_pb.critical" run batch/kpl2 -- true
	chk run_refused "$RS $(yn grep -q 'a policy cannot grant protected status' "$T/rc.err")" "2 yes"
	rc list
	chk nothing_started "$(grep -cE '^batch/(kpl|kpl2) ' "$T/rc.out")" 0
	rcq critical
	chk registry_unchanged "$(yn cmp -s "$T/crit.before" "$T/rcq.out")" yes
	chk domains "$(rc_domain_dirs)" "$cr_d0"
	rm -f "$cr_pb.protect" "$cr_pb.protected" "$cr_pb.critical" "$cr_pb.classx" "$cr_pb.limx" "$cr_pb.ctl" "$cr_pb.ins1" "$cr_pb.ins2" "$cr_pb.ins3"
	crit_verdict crit_policy "8 policies that name protected status (the sections protect, protected and critical.workloads; the keys protected, protect and critical in class, limits, system and workload) are refused with rc 2 and 'a policy cannot grant protected status', the valid control is not; init, supervise and run under them changed nothing"

	# a workload that runs as nobody (the policy's uid and gid) cannot reach system, the registry or the state directory
	cr_udir="$T/${RC_P}ud"
	rm -rf "$cr_udir"
	mkdir -p "$cr_udir"
	chmod 777 "$cr_udir"
	cp "$RC_BIN" "$cr_udir/resctl"
	chmod 755 "$cr_udir/resctl"
	cp "$RC_POLICY" "$cr_udir/policy"
	chmod 644 "$cr_udir/policy"
	crit_write_ud
	rc_restart "$cr_udir/ud.base" on-failure 3 60s 10 100ms 400ms
	{ grep -v '^allow.root' "$cr_udir/ud.base"; printf 'uid = 65534\ngid = 65534\n'; } >"$T/${RC_P}ud.policy"
	rc_pol "$T/${RC_P}ud.policy" supervise batch/kud --restart never -- sh "$T/crit_ud.sh" "$cr_udir/out" "$cr_udir/resctl" "$cr_udir/policy" "$RC_STATE" "$CRITD" $(rc_roots)
	rs=$RS
	ou="$(rc_out)"
	chk ud_rc "$rs" 0
	chk ud_finished "$(yn like "$ou" 'phase=finished*')" yes
	chk ud_ids "$(grep -c '^ids uid=65534 gid=65534$' "$cr_udir/out")" 1
	chk ud_system_denied "$(grep -c '^system-write-denied ' "$cr_udir/out")" "$(rc_roots | wc -l)"
	chk ud_system_written "$(grep -c '^system-write-ok ' "$cr_udir/out")" 0
	chk ud_cgroup "$(grep -c '^cgroup-unchanged$' "$cr_udir/out")" 1
	chk ud_registry "$(grep -c '^registry-write-denied$' "$cr_udir/out")" 1
	chk ud_state_listing "$(grep -c '^state-list-denied$' "$cr_udir/out")" 1
	chk ud_protect "$(yn like "$(grep '^protect rc=' "$cr_udir/out")" 'protect rc=2 resctl: refused: protect is refused inside the workload batch/kud:*')" yes
	chk ud_critical "$(yn like "$(grep '^critical rc=' "$cr_udir/out")" 'critical rc=1 *belongs to user 0, not to user 65534*')" yes
	chk ud_kill "$(yn like "$(grep '^kill-critical rc=' "$cr_udir/out")" 'kill-critical rc=[1-9]*')" yes
	cr_ud="$(grep '^system-write' "$cr_udir/out" | sort | uniq -c | tr '\n' ';' | cut -c1-120)"
	rm -rf "$cr_udir"
	rm -f "$T/${RC_P}ud.policy"
	crit_verdict crit_uid_drop "a workload that ran as uid 65534 could not write the cgroup of system in any of the $(rc_roots | wc -l) hierarchies (its cgroup did not change), could not write the registry or list the state directory, was refused protect and could not signal critd"

	# a registry that cannot be read is healed by the next command that ends a workload: kill-type commands never kill what
	# they cannot tell is critical. All of it unreadable: rebuilt from system, the names are lost
	crit_unmanaged batch/hl 4951
	cr_pj=$CRIT_PJ
	crit_snap
	cr_rb="registry rebuilt from $((1 + CANARY_EXTRA)) $cr_pes of system\$"
	cr_q0="$(crit_count quarantine critical)"
	cr_r0="$(crit_count recover critical "$cr_rb")"
	printf 'garbage\n' >"$RC_STATE/critical"
	rcq critical
	chk garbage_listing "$QS $(head -n 1 "$T/rcq.out")" "0 registry corrupt=it does not start with v=1"
	rc kill batch/hl
	rs=$RS
	rc_reap "$cr_pj" 10
	cr_hung=$SPHUNG
	crit_snap
	cr_q1="$(crit_count quarantine critical)"
	cr_r1="$(crit_count recover critical "$cr_rb")"
	chk garbage_kill "$rs $cr_hung" "0 no"
	chk garbage_quarantined "$((cr_q1 - cr_q0))" 1
	chk garbage_rebuilt "$((cr_r1 - cr_r0))" 1
	crit_chk after_garbage
	rc protect "$CRITD" --name critd
	chk rename_critd "$RS $(rc_out)" "0 protected=critd pid=$CRITD placed=system moved=no"
	rc protect "$KEEPER" --name keeper
	chk rename_keeper "$RS $(rc_out)" "0 protected=keeper pid=$KEEPER placed=system moved=no"
	rc protect "$CANARY" --name sleep
	chk rename_canary "$RS $(rc_out)" "0 protected=sleep pid=$CANARY placed=system moved=no"
	crit_drop batch/hl
	# one readable line left, and critd in a workload at the moment of the heal: the line is kept, critd is not killed
	crit_unmanaged batch/hl2 4952
	cr_pj=$CRIT_PJ
	cr_mp="$(crit_misplace workload/batch/hl2 critd)"
	printf 'v=1\n%s %s critd\nthis line is damaged\n' "$CRITD" "$CRITD_START" >"$RC_STATE/critical"
	crit_snap
	cr_sv="registry rebuilt from $((1 + KEEPER_UP)) $cr_pes of system and 1 salvaged entry\\(ies\\)\$"
	cr_s0="$(crit_count recover critical "$cr_sv")"
	rcq critical
	chk damaged_listing "$QS $(head -n 1 "$T/rcq.out" | cut -c1-16)" "0 registry corrupt"
	rc kill batch/hl2
	rs=$RS
	rc_reap "$cr_pj" 10
	cr_hung=$SPHUNG
	cr_rs="$(crit_rescues "$CRITD" workload/batch/hl2)"
	crit_snap
	cr_s1="$(crit_count recover critical "$cr_sv")"
	chk damaged_placed "$cr_mp" ok
	chk damaged_kill "$rs $cr_hung" "0 no"
	chk damaged_salvaged "$((cr_s1 - cr_s0))" 1
	chk damaged_rescue "$cr_rs" 1
	rcq critical
	chk damaged_name_kept "$(grep -cE "^critd pid=$CRITD state=live placed=system " "$T/rcq.out")" 1
	crit_chk after_damaged
	rc protect "$KEEPER" --name keeper
	rc protect "$CANARY" --name sleep
	crit_drop batch/hl2
	crit_verdict crit_registry "an unreadable registry was quarantined and rebuilt from the processes of system by the kill that found it; a registry with one readable line kept that line (critd, which was in the workload at that moment) and was not allowed to kill it; the names were registered again"

	# a registered process that is gone is stale: listed so, never signalled, dropped by recover; unprotect only unregisters
	crit_outsider 4953 &
	cr_st1=$!
	rc protect "$cr_st1" --name stale1
	chk stale_protect "$RS $(rc_out)" "0 protected=stale1 pid=$cr_st1 placed=system moved=yes"
	kill -9 "$cr_st1" 2>/dev/null
	rc_reap "$cr_st1" 10
	rcq critical
	chk stale_listed "$(grep -cE "^stale1 pid=$cr_st1 state=stale placed=- start=[0-9]+\$" "$T/rcq.out")" 1
	rc recover
	chk stale_recover "$RS $(yn matches "$(rc_out)" '(^| )pruned=1( |$)')" "0 yes"
	chk stale_dropped "$(rc_ev critical recover "dropped stale1=$cr_st1")" 1
	crit_outsider 4954 &
	cr_up=$!
	rc protect "$cr_up" --name tmp
	rc unprotect "$cr_up"
	chk unprotect "$RS $(rc_out)" "0 unprotected=$cr_up"
	rc unprotect "$cr_up"
	chk unprotect_again "$RS $(rc_out)" "0 not-protected=$cr_up"
	chk unprotect_stays "$(yn rc_in "$cr_up" system)" yes
	rcq critical
	chk unprotect_listed "$(grep -c "pid=$cr_up " "$T/rcq.out")" 0
	chk unprotect_audit "$(rc_ev tmp unprotect "pid=$cr_up( |\$)")" 1
	kill -9 "$cr_up" 2>/dev/null
	rc_reap "$cr_up" 10
	crit_verdict crit_stale "a protected process that died is listed state=stale (stale1, pid $cr_st1), recover dropped it (pruned=1, audited), unprotect unregistered another one that stayed in system; neither is ever signalled"

	# recovery after a failure of the supervisor: the reaper, or recover, sweeps the workload, and a critical process that was in
	# its domain is moved out first
	crit_sup batch/krc1 4961
	cr_sp=$CS_PID
	cr_mp="$(crit_misplace workload/batch/krc1)"
	kill -9 "$cr_sp" 2>/dev/null
	rc_reap "$cr_sp" 10
	waitfor 30 rc_phase_is batch/krc1 failed
	st="$(rc_sf batch/krc1 phase ended domain)"
	waitfor 5 rc_find_is 'sleep 4961 ' 0
	cr_left="$(rc_find 'sleep 4961 ')"
	cr_rs="$(crit_rescued workload/batch/krc1)"
	cr_ra="$(crit_rescued workload/batch/krc1 reap)"
	rc reset batch/krc1
	chk krc1_placed "$cr_mp" ok
	chk krc1_state "$st" "failed lost absent"
	chk krc1_neighbour "$cr_left" 0
	chk krc1_rescues "$cr_rs" "1 1"
	chk krc1_rescued_by_the_reaper "$cr_ra" "1 1"
	crit_chk after_lost_supervisor

	crit_sup batch/krc2 4962
	cr_sp=$CS_PID
	for cr_p in $(rc_pid_of '__reap batch/krc2 '); do kill -9 "$cr_p" 2>/dev/null; done
	kill -9 "$cr_sp" 2>/dev/null
	rc_reap "$cr_sp" 10
	cr_mp="$(crit_misplace workload/batch/krc2)"
	rc recover
	rs=$RS
	cr_sw="$(grep -c '^swept=batch/krc2 phase=failed$' "$T/rc.out")"
	st="$(rc_sf batch/krc2 phase ended domain)"
	cr_left="$(rc_find 'sleep 4962 ')"
	cr_rs="$(crit_rescued workload/batch/krc2)"
	rc reset batch/krc2
	chk krc2_placed "$cr_mp" ok
	chk krc2_recover "$rs $cr_sw" "0 1"
	chk krc2_state "$st" "failed lost absent"
	chk krc2_neighbour "$cr_left" 0
	chk krc2_rescues "$cr_rs" "1 1"
	crit_chk after_recover_of_a_workload

	rc create batch/krc3
	cr_rs3=$RS
	crit_outsider 4963 &
	cr_nb=$!
	crit_mv "$cr_nb" workload/batch/krc3
	cr_mp="$(crit_misplace workload/batch/krc3)"
	rc recover
	rs=$RS
	ou="$(rc_out)"
	rc_reap "$cr_nb" 10
	cr_hung=$SPHUNG
	cr_rs="$(crit_rescued workload/batch/krc3)"
	chk krc3_create "$cr_rs3" 0
	chk krc3_placed "$cr_mp" ok
	chk krc3_recover "$rs $(yn matches "$ou" '(^| )killed=batch/krc3( |$)') $cr_hung" "0 yes no"
	chk krc3_rescues "$cr_rs" "1 1"
	crit_chk after_recover_of_a_domain
	rc remove batch/krc3
	chk sleeps_left "$(rc_find 'sleep 496')" 0
	chk domains "$(rc_domain_dirs)" "$cr_dirs"
	crit_verdict crit_recover "a supervisor killed with critd and the keeper in its workload (the reaper swept it and moved them out), the same with the reaper killed too (recover swept it) and a domain nobody owns (recover killed its neighbour): the services survived all three, no domain is left"

	# a teardown keeps them: they stay in system, registered
	crit_unmanaged batch/ktd 4971
	cr_pj=$CRIT_PJ
	cr_mp="$(crit_misplace workload/batch/ktd)"
	cr_td0="$(rc_ev all teardown 'forgotten=[0-9]+ kept=[0-9]+ stuck=0')"
	rc teardown
	rs=$RS
	cr_kept="$(grep -cE "^kept=system: $((1 + CANARY_EXTRA)) $cr_pes stay in their domain\$" "$T/rc.out")"
	cr_done="$(grep -c '^teardown=done$' "$T/rc.out")"
	rc_reap "$cr_pj" 10
	cr_hung=$SPHUNG
	chk ktd_placed "$cr_mp" ok
	chk ktd_teardown "$rs $cr_kept $cr_done $cr_hung" "0 1 1 no"
	chk ktd_audit "$(($(rc_ev all teardown 'forgotten=[0-9]+ kept=[0-9]+ stuck=0') - cr_td0))" 1
	crit_chk after_teardown
	rc init
	chk ktd_init "$RS" 0
	crit_chk after_init
	chk domains "$(rc_domain_dirs)" "$cr_dirs"
	crit_verdict crit_teardown "teardown with critd and the keeper in a workload kept them: kept=system: $((1 + CANARY_EXTRA)) process(es) stay in their domain, the registry survived and init found them live in system"

	# the trail of all of it
	crit_snap
	cr_nl="$(wc -l <"$T/crit.audit" | tr -d ' ')"
	chk trail_size "$(yn [ "$cr_nl" -ge 100 ])" yes
	chk trail_wellformed "$(grep -cvE '^[0-9]+ [0-9]+ [0-9]+:[a-z]+ [a-z]+ [^ ]+( .*)?$' "$T/crit.audit")" 0
	chk trail_roles "$(grep -cvE '^[0-9]+ [0-9]+ [0-9]+:(cli|super|main|reap) ' "$T/crit.audit")" 0
	chk trail_sequence "$(awk '{ print $1 }' "$T/crit.audit" | sort -n | awk -v first="$cr_seq0" 'BEGIN { want = first + 1 } { if ($1 + 0 != want) bad++; want = $1 + 1 } END { print bad + 0 }')" 0
	cr_nr="$(grep -cE '^[0-9]+ [0-9]+ [0-9]+:[a-z]+ rescue ' "$T/crit.audit")"
	cr_nrw="$(grep -cE '^[0-9]+ [0-9]+ [0-9]+:(cli|reap|super) rescue pid[0-9]+ found in agent-interaction/workload/[a-z]+/[A-Za-z0-9_-]+ moved=true$' "$T/crit.audit")"
	chk rescue_lines "$(yn [ "$cr_nr" -ge 20 ]) $cr_nrw" "yes $cr_nr"
	cr_ev=
	for cr_e in protect unprotect refuse rescue quarantine recover enter intent exit start restart; do
		[ "$(grep -cE "^[0-9]+ [0-9]+ [0-9]+:[a-z]+ $cr_e " "$T/crit.audit")" -ge 1 ] || cr_ev="$cr_ev $cr_e"
	done
	chk event_kinds "${cr_ev:-ok}" ok
	cr_ac=
	for cr_a in cli reap super; do
		[ "$(grep -cE "^[0-9]+ [0-9]+ [0-9]+:$cr_a rescue " "$T/crit.audit")" -ge 1 ] || cr_ac="$cr_ac $cr_a"
	done
	chk rescue_actors "${cr_ac:-ok}" ok
	chk intent_stop "$(crit_count intent batch/ks1 'stop asked')" 1
	chk intent_kill "$(crit_count intent batch/ks2 'kill asked')" 1
	chk exit_stop "$(crit_count exit batch/ks1 'gen=1 ended=requested:stop status=signal:15')" 1
	chk exit_kill "$(crit_count exit batch/ks2 'gen=1 ended=requested:kill status=signal:9')" 1
	chk exit_crash "$(crit_count exit batch/kc2 'gen=1 ended=crashed:9 status=signal:9')" 1
	chk crash_loop_starts "$(crit_count start batch/kc1)" 4
	chk refused_in1 "$(crit_count refuse '[a-z]+' 'from workload batch/kin1')" 17
	chk refused_in2 "$(crit_count refuse '[a-z]+' 'from workload batch/kin2')" 17
	chk refused_protect "$(crit_count refuse protect '(refused: )?process [0-9]+ runs in the workload')" 6
	chk refused_protect_inside "$(crit_count refuse protect 'from workload batch/kin[12]')" 6
	crit_verdict crit_audit "$cr_nl audit lines from #$((cr_seq0 + 1)) without a gap, all well formed; $cr_nr rescues by cli, reap and super; the stop, the kill, the crash and the crash loop of the scenarios each have their intent, exit and start lines; 34 refusals from inside workloads (6 of them protects) and 6 refused protects of workload processes"

	# all through it: the same processes, in system, beating
	cr_now="$(cg_uptime)"
	cr_el="$(awk -v a="$CRIT_T0" -v b="$cr_now" 'BEGIN { print b - a }')"
	cr_nb="$(crit_beats)"
	cr_gap="$(awk '{ if (NR > 1 && $1 - prev > g) g = $1 - prev; prev = $1 } END { print g + 0 }' "$T/critd.beat")"
	chk same_critd "$CRITD $CRITD_START" "$CRIT_FIRST"
	chk same_keeper "$KEEPER $KEEPER_START" "$KEEPER_FIRST"
	chk restarts "$CRIT_RESTARTS" 0
	chk beats "$(awk -v n="$cr_nb" -v el="$cr_el" 'BEGIN { if (n + 0 >= el / 2) print "ok"; else print n " beats in " el " s" }')" ok
	chk beat_gap "$(awk -v g="$cr_gap" 'BEGIN { if (g + 0 < 5) print "ok"; else print g " s" }')" ok
	chk final_state "$(crit_state)" ok
	canary_note crit_survival
	kill -9 "$CRITD" 2>/dev/null
	rc_reap "$CRITD" 10
	CRITD_UP=0
	if [ -n "$KEEPER" ]; then
		kill -9 "$KEEPER" 2>/dev/null
		rc_reap "$KEEPER" 10
	fi
	KEEPER_UP=0
	crit_extra
	rc recover
	chk services_pruned "$RS $(yn matches "$(rc_out)" '(^| )pruned=2( |$)')" "0 yes"
	rcq critical
	chk registry_after "$(grep -c ' state=live ' "$T/rcq.out") $(grep -c ' state=stale ' "$T/rcq.out")" "1 0"
	verdict crit_survival "critd (pid $CRITD) and the keeper (pid $KEEPER) were the same processes from the first scenario to the last, never restarted, never out of system, in the registry, and beat for ${cr_el}s ($cr_nb beats, longest gap ${cr_gap}s)"
	rm -f "$T/critd" "$T/critd.beat" "$T/critd.beat.fifo" "$T/crit_inside.sh" "$T/crit_ud.sh" "$T/crit.audit" "$T/crit.before" "$T/rcq.out" "$T/rcq.err"
	rm -f "$T/${RC_P}cin1.out" "$T/${RC_P}cin1.out.o" "$T/${RC_P}cin1.out.e" "$T/${RC_P}cin2.out" "$T/${RC_P}cin2.out.o" "$T/${RC_P}cin2.out.e"
	rm -f "$T/${RC_P}kc4.out" "$T/${RC_P}kp1.child" "$T/${RC_P}m5.go" "$T/${RC_P}m5.child" "$T/${RC_P}ku4.ready"
	cr_scrub
	canary_report system_canary_critical
}

# ------------------------------------------------------------------------------------------------ stress
# Exhaustion inside the workload domains, measured from outside them. A periodic task in the system domain (ctr-test
# beat, the stand-in for a critical service: it wakes every 20 ms and allocates) times its own stalls; a probe times the
# control plane (resctl state); and the kernel's own counters of each memory cgroup, not resctl's report of them, say
# where the OOM killer struck and how high the usage went. Every load is bounded twice, by its domain and by its own
# size, so that a broken limit cannot take the machine with it. The thresholds of the timing rows hold on the host; the
# guest is emulated (its timing is not real), so there the rows say what happened, not how fast.
STRESS_STALL_MAX=0.50
STRESS_PROBE_MAX=2.00
STRESS_KILL_MAX=5.00
STRESS_LOOP_MAX=15.00

# rc_kmem DOMAIN oom_kill|peak: the kernel's count of OOM kills of a domain's memory cgroup, or its peak usage in bytes
rc_kmem() {
	_d="$(rc_dir memory "$1")"
	case "$(rc_api memory):$2" in
	v2:oom_kill) sed -n 's/^oom_kill //p' "$_d/memory.events" 2>/dev/null ;;
	v1:oom_kill) sed -n 's/^oom_kill //p' "$_d/memory.oom_control" 2>/dev/null ;;
	v2:peak) cat "$_d/memory.peak" 2>/dev/null ;;
	v1:peak) cat "$_d/memory.max_usage_in_bytes" 2>/dev/null ;;
	esac
}
# rc_probe OUT STOP: time `resctl state` every 0.2 s until the file STOP exists (at most 600 times); the longest one,
# in seconds, goes to OUT
rc_probe() {
	pmax=0
	pn=0
	while [ ! -e "$2" ] && [ "$pn" -lt 600 ]; do
		pa="$(cg_uptime)"
		"$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" --state-dir "$RC_STATE" state >/dev/null 2>&1
		pb="$(cg_uptime)"
		pmax="$(awk -v m="$pmax" -v a="$pa" -v b="$pb" 'BEGIN { d = b - a; printf "%.2f", (d > m) ? d : m }')"
		pn=$((pn + 1))
		sleep 0.2
	done
	echo "$pn $pmax" >"$1"
}

rc_s_stress() {
	section stress || return 0
	stop="$T/${RC_P}stress.stop"
	rm -f "$stop"
	sys0="$(rc_kmem system oom_kill)"
	rcbg enter system -- "$CT" beat 20 600 "$stop" >"$T/${RC_P}beat.out" 2>&1 &
	beat=$!
	CANARY_EXTRA=1
	waitfor 10 grep -q '^beat-ready' "$T/${RC_P}beat.out"
	beat_in="$(grep -c '/agent-interaction/system$' "/proc/$beat/cgroup" 2>/dev/null)"
	rc_probe "$T/${RC_P}probe.out" "$stop" &
	probe=$!

	# a workload over its memory limit is killed by the kernel in its own domain; a neighbour keeps what it holds
	rcbg run interactive/sn --memory-max 64M -- "$CT" mem 40 8 >"$T/${RC_P}sn.out" 2>&1 &
	snp=$!
	waitfor 20 grep -q mem-ok "$T/${RC_P}sn.out"
	rc run batch/sa --memory-max 48M -- "$CT" mem 400
	sa_rc=$RS
	sa_kill="$(rc_kmem workload/batch/sa oom_kill)"
	sa_peak="$(rc_kmem workload/batch/sa peak)"
	sn_alive="$(yn rc_alive "$snp")"
	wait "$snp" 2>/dev/null
	sn_rc=$?
	sn_kill="$(rc_kmem workload/interactive/sn oom_kill)"
	rc_drop batch/sa
	rc_drop interactive/sn
	chk over_limit_rc "$sa_rc" 137
	chk over_limit_oom_kills "$([ "${sa_kill:-0}" -ge 1 ] && echo ">=1" || echo "${sa_kill:-none}")" ">=1"
	chk over_limit_peak "$([ -n "$sa_peak" ] && [ "$sa_peak" -le 54525952 ] && echo "<=48M+4M" || echo "${sa_peak:-none}")" "<=48M+4M"
	chk neighbour_held "$sn_alive $sn_rc ${sn_kill:-none} $(grep -c mem-ok "$T/${RC_P}sn.out")" "yes 0 0 1"
	verdict stress_memory "a 400 MiB allocation under --memory-max 48M ended rc 137 with $sa_kill OOM kill(s) counted by its own memory cgroup, peak $sa_peak bytes; the 40 MiB neighbour in interactive/sn kept running to its end (rc 0, no OOM kill)"
	canary_note stress_memory

	# three workloads that each fit their own limit but not, together, the ceiling of their class
	for i in 1 2 3; do
		rcbg run "interactive/sc$i" --memory-max 120M -- "$CT" mem 100 3 >/dev/null 2>&1 &
		eval "scp$i=\$!"
	done
	sc_rcs=
	sc_hung=
	for i in 1 2 3; do
		eval "p=\$scp$i"
		rc_reap "$p" 30
		sc_rcs="$sc_rcs $SPRC"
		sc_hung="$sc_hung$SPHUNG"
	done
	sc_kills=0
	for i in 1 2 3; do
		k="$(rc_kmem "workload/interactive/sc$i" oom_kill)"
		sc_kills=$((sc_kills + ${k:-0}))
		rc_drop "interactive/sc$i"
	done
	cl_peak="$(rc_kmem workload/interactive peak)"
	chk class_oom_kills "$([ "$sc_kills" -ge 1 ] && echo ">=1" || echo "$sc_kills")" ">=1"
	chk class_peak "$([ -n "$cl_peak" ] && [ "$cl_peak" -le 272629760 ] && echo "<=256M+4M" || echo "${cl_peak:-none}")" "<=256M+4M"
	chk class_ended "$sc_hung" nonono
	verdict stress_class "3 x 100 MiB in interactive (class ceiling 256M): $sc_kills OOM kill(s) in the workloads (exit statuses$sc_rcs), class peak $cl_peak bytes"
	canary_note stress_class

	# every CPU busy in a batch workload (cpu.weight 100 against the system domain's 1000): judged by the beat below
	rc run batch/su -- sh -c 'i=0; while [ "$i" -lt 8 ]; do "$0" cpu 5 & i=$((i + 1)); done; wait' "$CT"
	su_rc=$RS
	rc_drop batch/su
	canary_note stress_cpu

	# a forced kill of a workload that holds memory at its limit and burns CPU leaves nothing behind
	rcbg run batch/sk --memory-max 64M -- sh -c '"$0" mem 28 60 & "$0" mem 28 60 & "$0" cpu 60 & "$0" cpu 60 & wait' "$CT" >/dev/null 2>&1 &
	skp=$!
	waitfor 20 rc_pids_ge batch/sk 5
	sleep 1
	sk_before="$(rc_find "$CT mem 28 60") $(rc_find "$CT cpu 60")"
	ka="$(cg_uptime)"
	rc kill batch/sk
	sk_rc=$RS
	waitfor 30 rc_dead "$skp"
	waitfor 30 rc_find_is "$CT mem 28 60" 0
	waitfor 30 rc_find_is "$CT cpu 60" 0
	kb="$(cg_uptime)"
	sk_left="$(rc_find "$CT mem 28 60") $(rc_find "$CT cpu 60")"
	sk_el="$(rc_secs "$ka" "$kb")"
	rc_drop batch/sk "$skp"
	sk_gone="$(yn rc_gone batch/sk)"
	chk kill_rc "$sk_rc" 0
	chk kill_found "$sk_before" "2 2"
	chk kill_left "$sk_left" "0 0"
	chk kill_domain_gone "$sk_gone" yes
	verdict stress_kill "kill of batch/sk (2 x 28 MiB at a 64M limit, 2 CPU burners): every process gone after ${sk_el}s, the domain removed"
	canary_note stress_kill

	# a supervised workload that runs out of memory on every start ends failed within its restart budget
	la="$(cg_uptime)"
	rc_pol_b 60 "$RC_POLICY" supervise batch/so --memory-max 32M -- "$CT" mem 200
	so_rc=$RS
	lb="$(cg_uptime)"
	so_st="$(rc_sf batch/so phase gen restarts ended oom)"
	so_starts="$(rc_ev batch/so start)"
	so_el="$(rc_secs "$la" "$lb")"
	rc reset batch/so
	chk loop_rc "$so_rc" 1
	chk loop_state "$so_st" "failed 4 3 crashed:9 yes"
	chk loop_starts "$so_starts" 4
	verdict stress_oom_loop "an OOM kill on every start: 4 starts, ended crashed:9 oom=yes, failed after ${so_el}s"
	canary_note stress_oom_loop

	# every process resctl puts into a workload is the first choice of the OOM killer of the whole machine, a supervised one
	# too after it changed its user, and the system domain is not biased (the floor of the bias is a row of limits)
	rc run batch/ob -- sh -c 'cat /proc/self/oom_score_adj; sh -c "cat /proc/self/oom_score_adj"'
	ob_run="$(rc_out)"
	rc_drop batch/ob
	ob_dir="$T/${RC_P}ob"
	mkdir -p "$ob_dir"
	chmod 777 "$ob_dir"
	{ grep -v '^allow.root' "$RC_POLICY"; printf 'uid = 65534\ngid = 65534\n'; } >"$ob_dir/policy"
	rc_pol "$ob_dir/policy" supervise batch/oc --restart never -- sh -c 'echo "$(id -u) $(cat /proc/self/oom_score_adj)" >"$0"' "$ob_dir/out"
	ob_rc=$RS
	ob_sup="$(cat "$ob_dir/out" 2>/dev/null)"
	rc reset batch/oc
	ob_sys="$(cat "/proc/$CANARY/oom_score_adj" 2>/dev/null) $(cat "/proc/$beat/oom_score_adj" 2>/dev/null)"
	chk run_and_child "$ob_run" "1000 1000"
	chk supervised "$ob_rc $ob_sup" "0 65534 1000"
	chk system "$ob_sys" "0 0"
	verdict oom_bias "a run workload and its child have oom_score_adj 1000, a supervised workload running as uid 65534 has 1000, the canary and the beat in system keep 0"
	canary_note oom_bias

	# the whole machine out of memory (in the guest only, where it is safe): the OOM killer takes the workload, not the
	# larger critical service of the system domain
	if [ "${RC_GLOBAL_OOM:-no}" = yes ]; then
		avail=$(($(sed -n 's/^MemAvailable: *\([0-9]*\) kB$/\1/p' /proc/meminfo) / 1024))
		# the critical service holds more than half of what is available, so that it would be the largest process when
		# the machine runs out; the workload fits its class ceiling (256M) with room to spare; and the allocation outside
		# every domain asks for the whole of the memory, so the machine does run out whatever the estimate was worth, but
		# stops as soon as the workload is gone: what is left fits, and no second OOM follows
		big=$((avail * 55 / 100))
		wl=$((avail * 20 / 100))
		[ "$wl" -le 200 ] || wl=200
		out=$(($(sed -n 's/^MemTotal: *\([0-9]*\) kB$/\1/p' /proc/meminfo) / 1024))
		vm0="$(sed -n 's/^oom_kill //p' /proc/vmstat)"
		rcbg enter system -- "$CT" mem "$big" 120 >"$T/${RC_P}gbig.out" 2>&1 &
		gbig=$!
		CANARY_EXTRA=2
		waitfor 60 grep -q mem-ok "$T/${RC_P}gbig.out"
		rcbg run interactive/gw --memory-max "$((wl + 32))M" -- "$CT" mem "$wl" 120 >"$T/${RC_P}gw.out" 2>&1 &
		gwp=$!
		waitfor 60 grep -q mem-ok "$T/${RC_P}gw.out"
		"$CT" mem-until "$out" "$gwp" >"$T/${RC_P}gout.out" 2>&1
		out_rc=$?
		rc_reap "$gwp" 30
		gw_rc="$SPRC $SPHUNG"
		vm1="$(sed -n 's/^oom_kill //p' /proc/vmstat)"
		gw_kill="$(rc_kmem workload/interactive/gw oom_kill)"
		gw_peak="$(rc_kmem workload/interactive/gw peak)"
		big_ok="$(yn rc_alive "$gbig") $(grep -c '/agent-interaction/system$' "/proc/$gbig/cgroup" 2>/dev/null)"
		kill -9 "$gbig" 2>/dev/null
		wait "$gbig" 2>/dev/null
		CANARY_EXTRA=1
		rc_drop interactive/gw
		chk sizes "$([ "$avail" -ge 400 ] && echo enough || echo "only ${avail} MiB available")" enough
		chk workload_killed "$gw_rc $([ "${gw_kill:-0}" -ge 1 ] && echo counted || echo "oom_kill=${gw_kill:-none}")" "137 no counted"
		chk not_its_own_limit "$([ -n "$gw_peak" ] && [ "$gw_peak" -lt $(((wl + 32) << 20)) ] && echo below || echo "${gw_peak:-none}")" below
		chk machine_oom "$([ "$((${vm1:-0} - ${vm0:-0}))" -ge 1 ] && echo yes || echo no)" yes
		chk critical_alive "$big_ok" "yes $(rc_roots | wc -l)"
		chk outside_stopped "$out_rc $(grep -c 'process is gone' "$T/${RC_P}gout.out")" "0 1"
		verdict global_oom "${avail} MiB available: a critical service in system held ${big} MiB, a workload ${wl} MiB (limit $((wl + 32))M), and an allocation outside every domain asked for all ${out} MiB: the OOM killer took the workload (rc 137, oom_kill counted in its memory cgroup, its own limit not reached), the allocation stopped at $(sed -n 's/^mem-until=\([0-9]*\) .*/\1/p' "$T/${RC_P}gout.out") MiB, and the critical service kept running in system ($((${vm1:-0} - ${vm0:-0})) OOM kill)"
		canary_note global_oom
	fi

	# the measurements, and the system domain the OOM killer never touched
	: >"$stop"
	wait "$probe" 2>/dev/null
	rc_reap "$beat" 10
	beat_rc=$SPRC
	CANARY_EXTRA=0
	sys1="$(rc_kmem system oom_kill)"
	read -r pn pmax <"$T/${RC_P}probe.out" 2>/dev/null
	stall="$(sed -n 's/^beats=[0-9]* max_stall_ms=\([0-9]*\)$/\1/p' "$T/${RC_P}beat.out")"
	beats="$(sed -n 's/^beats=\([0-9]*\) .*/\1/p' "$T/${RC_P}beat.out")"
	stall_s="$(awk -v ms="${stall:-}" 'BEGIN { if (ms == "") print "none"; else printf "%.2f", ms / 1000 }')"
	chk beat_in_system "$beat_in" "$(rc_roots | wc -l)"
	chk beat_ended "$beat_rc ${beats:+yes}" "0 yes"
	chk cpu_load_rc "$su_rc" 0
	chk system_oom_kills "${sys0:-none} ${sys1:-none}" "${sys0:-none} ${sys0:-none}"
	verdict stress_system "the beat in the system domain ran through all of it, 8 CPU burners included ($beats beats, longest stall ${stall_s}s), and the system domain's memory cgroup counted no OOM kill (${sys1:-none})"
	canary_report system_canary_stress
	# the timing, where it is real (RC_TIMING=no under software emulation)
	[ "${RC_TIMING:-yes}" = yes ] || return 0
	if [ -n "$stall" ] && cg_between "$stall_s" 0 "$STRESS_STALL_MAX"; then f stress_stall "ok the longest stall of the beat was ${stall_s}s over $beats beats (at most ${STRESS_STALL_MAX}s)"
	else f stress_stall "fail:the longest stall of the beat was ${stall_s}s over ${beats:-0} beats (at most ${STRESS_STALL_MAX}s)"; fi
	if [ "${pn:-0}" -ge 10 ] && cg_between "$pmax" 0 "$STRESS_PROBE_MAX"; then f stress_control "ok resctl state answered $pn times, the slowest in ${pmax}s (at most ${STRESS_PROBE_MAX}s)"
	else f stress_control "fail:resctl state answered ${pn:-0} times, the slowest in ${pmax:-?}s (at least 10 answers, at most ${STRESS_PROBE_MAX}s)"; fi
	if [ "$sk_left" = "0 0" ] && cg_between "$sk_el" 0 "$STRESS_KILL_MAX"; then f stress_kill_time "ok the kill under load was complete after ${sk_el}s (at most ${STRESS_KILL_MAX}s)"
	else f stress_kill_time "fail:the kill under load left '$sk_left' after ${sk_el}s (wanted '0 0' within ${STRESS_KILL_MAX}s)"; fi
	if [ "$so_rc" = 1 ] && cg_between "$so_el" 0 "$STRESS_LOOP_MAX"; then f stress_loop_time "ok the OOM crash loop ended failed after ${so_el}s (at most ${STRESS_LOOP_MAX}s)"
	else f stress_loop_time "fail:the OOM crash loop ended rc ${so_rc} after ${so_el}s (wanted 1 within ${STRESS_LOOP_MAX}s)"; fi
}

# ------------------------------------------------------------------------------------------------ lifecycle layers
rc_s_lifecycle() {
	section lifecycle || return 0
	# --lifecycle v2: freeze, kill and pressure are served by the unified hierarchy
	if [ -z "$CG_V2" ]; then f lifecycle_v2 "unsupported:no cgroup2 hierarchy is mounted"
	else
		rcl v2 probe
		rs=$RS
		bad=
		for ft in freeze kill pressure; do
			v="$(sed -n "s/^feature\.$ft=//p" "$T/rc.out")"
			case "$v" in v2:*) ;; *) bad="$bad $ft=${v:-?}" ;; esac
		done
		if [ "$rs" = 0 ] && [ -z "$bad" ]; then f lifecycle_v2 "ok --lifecycle v2: freeze, kill and pressure are served by $(sed -n 's/^feature\.kill=v2://p' "$T/rc.out")"
		else f lifecycle_v2 "fail:rc=$rs not served by the unified hierarchy:${bad:- (probe failed: $(rcerr))}"; fi
	fi

	# --lifecycle v1: the v1 freezer takes over freeze and kill; where there is no v1 freezer the choice is refused
	if [ -n "$(cg_v1dir freezer)" ]; then
		rcl v1 probe
		rs=$RS
		fz="$(sed -n 's/^feature\.freeze=//p' "$T/rc.out")"
		kl="$(sed -n 's/^feature\.kill=//p' "$T/rc.out")"
		ps="$(sed -n 's/^feature\.pressure=//p' "$T/rc.out")"
		case "$fz $kl" in
		"v1:"*" v1:"*) f lifecycle_v1 "ok --lifecycle v1: freeze=$fz kill=$kl pressure=$ps" ;;
		*) f lifecycle_v1 "fail:rc=$rs freeze=${fz:-?} kill=${kl:-?} (wanted both on a v1 hierarchy)" ;;
		esac
	else
		rcl v1 init
		rs=$RS
		msg="$(rcerr)"
		if [ "$rs" = 3 ]; then f lifecycle_v1 "denied:$msg"
		else f lifecycle_v1 "fail:--lifecycle v1 without a v1 freezer gave rc=$rs, not 3: $msg"; fi
	fi
	canary_report system_canary_lifecycle
}
