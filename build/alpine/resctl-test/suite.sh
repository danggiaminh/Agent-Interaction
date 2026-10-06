# The resource-control suite: drives the resctl binary (runtime/resctl) as black box on whatever cgroup layout the
# machine has, and prints one KEY=VALUE fact per behaviour. Sourced after common.sh and cgroup.sh by priv.sh (the
# cloud host, cgroup v1 limits with v2 freeze/kill/pressure) and guest.sh (the test bed guest, cgroup v2 only).
#
#   rc_suite PREFIX LIFECYCLE [SECTION...]
#
# LIFECYCLE is resctl's --lifecycle (auto | v1 | v2). Sections: layout normal limits throttle pressure isolation
# terminate recover lifecycle (all of them when none is named). Every fact is "ok <evidence>", "fail:<what did not
# behave>", "unsupported:<why this layout cannot do it>" or "denied:<the refusal>"; unsupported is never a pass, and it
# carries a measured reason (an `unbound=` line of resctl, or the feature table of `resctl probe`).
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
	"$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" "$@" >"$T/rc.out" 2>"$T/rc.err"
	RS=$?
}
# rcx ARGS...: the same, attached to the caller's stdout and stderr (for `run` in the foreground).
rcx() { "$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" "$@"; }
# rcbg ARGS...: only ever as `rcbg ... &`: the background job execs resctl, which execs the command, so that $! is the
# process of the workload itself and not of a shell that waits for it.
rcbg() { exec "$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" "$@"; }
# rcerr: what resctl said on stderr, one line.
rcerr() { tail -n 1 "$T/rc.err" | cut -c1-200; }
# rcl LIFECYCLE ARGS...: one command under another lifecycle.
rcl() {
	l="$1"
	shift
	"$RC_BIN" --policy "$RC_POLICY" --lifecycle "$l" "$@" >"$T/rc.out" 2>"$T/rc.err"
	RS=$?
}
# rc_pol POLICYFILE ARGS...: one command under another policy.
rc_pol() {
	pf="$1"
	shift
	"$RC_BIN" --policy "$pf" --lifecycle "$RC_LC" "$@" >"$T/rc.out" 2>"$T/rc.err"
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
canary_ready() { [ "$(rc_procs system)" = 1 ]; }
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
	if ! grep -q '^procs=1$' "$T/rc.out" || ! grep -q '^state=running$' "$T/rc.out"; then
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
	CANARY=
	CANARY_BAD=
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
		timeout 120 "$RC_BIN" --policy "$RC_POLICY" --lifecycle "$RC_LC" run batch/mh --memory-high 24M --memory-max 96M -- "$CT" mem 40 >"$T/mh.out" 2>&1
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
	sh -c "'$RC_BIN' --policy '$RC_POLICY' --lifecycle '$RC_LC' run $1 -- sleep $2 >/dev/null 2>&1; true" &
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

	# a run that could not start leaves an empty domain, and recover removes it
	printf '#!/nonexistent/interpreter\n' >"$T/${RC_P}bad.sh"
	chmod +x "$T/${RC_P}bad.sh"
	rc run batch/bad -- "$T/${RC_P}bad.sh"
	rs=$RS
	rc list
	line="$(grep '^batch/bad ' "$T/rc.out")"
	rc recover
	got="$(grep -c '^removed=batch/bad$' "$T/rc.out")"
	if [ "$rs" != 0 ] && [ "$line" = "batch/bad procs=0 state=empty" ] && [ "$got" = 1 ] && rc_gone batch/bad; then
		f recover_empty "ok run failed with rc $rs and left '$line'; recover printed removed=batch/bad"
	else f recover_empty "fail:run rc=$rs, list said '$line', removed-lines=$got (wanted non-zero, 'batch/bad procs=0 state=empty', 1)"; fi

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
