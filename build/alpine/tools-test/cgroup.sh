# Layout-neutral cgroup tests, sourced after common.sh by priv.sh (the cloud host) and guest.sh (the test bed guest).
# Nothing here assumes a layout: cg_detect reads /proc/mounts and /proc/cgroups, and each of the two drivers prints the
# same facts for the API it drives, so the inventory can ask the same questions of cgroup v1 and of cgroup v2.
#
#   cg_detect                     CG_MODE = v2 | hybrid | v1 | none, CG_V2 = the cgroup2 mount point (or empty)
#   cg_layout_facts PREFIX        PREFIXcgroup_mode, PREFIXcgroup_v1_controllers, PREFIXcgroup_v2_controllers
#   cg_v2 PREFIX                  the cgroup v2 API:  PREFIXmemory pids cpu_quota cpu_weight cpuset io freeze kill psi
#   cg_v1 PREFIX                  the cgroup v1 API:  PREFIXmemory pids cpu_quota cpu_weight cpuset io
# Each of those facts is "ok <evidence>", "fail:<what did not behave>" or "unsupported:<why this layout cannot do it>".
# unsupported is never a pass: a layout that cannot provide a controller says so, with the proof (which hierarchy
# the controller is bound to). Needs CT (the static ctr-test binary) and, for the io test, CG_BLKDEV (a block device).
# Tolerances are wide enough for software emulation (TCG), where only the order of magnitude of timings is meaningful.

# cg_detect: what is mounted, from /proc/mounts.
cg_detect() {
	CG_V2="$(awk '$3 == "cgroup2" { print $2; exit }' /proc/mounts)"
	n1="$(awk '$3 == "cgroup"' /proc/mounts | wc -l)"
	if [ -n "$CG_V2" ] && [ "$n1" -eq 0 ]; then CG_MODE=v2
	elif [ -n "$CG_V2" ]; then CG_MODE=hybrid
	elif [ "$n1" -gt 0 ]; then CG_MODE=v1
	else CG_MODE=none; fi
}

# cg_v1dir CONTROLLER: the mount point of the v1 hierarchy that has the controller.
cg_v1dir() { awk -v c="$1" '$3 == "cgroup" && ("," $4 ",") ~ ("," c ",") { print $2; exit }' /proc/mounts; }

cg_layout_facts() {
	p="$1"
	kv "${p}cgroup_mode" "$CG_MODE"
	v1="$(awk '$3 == "cgroup" { n = split($4, o, ","); for (i = 1; i <= n; i++) if (o[i] !~ /^(rw|ro|relatime|nosuid|nodev|noexec|nsdelegate|memory_recursiveprot|clone_children|noprefix|release_agent=.*|cpuset_v2_mode)$/) printf "%s ", o[i] }' /proc/mounts | tr ' ' '\n' | sort -u | tr '\n' ' ' | sed 's/ $//')"
	kv "${p}cgroup_v1_controllers" "${v1:-none}"
	v2="$([ -n "$CG_V2" ] && cat "$CG_V2/cgroup.controllers" 2>/dev/null | tr ' ' '\n' | sort | tr '\n' ' ' | sed 's/ $//')"
	kv "${p}cgroup_v2_controllers" "${v2:-none}"
}

# cg_run DIR CMD...: move this shell into the cgroup DIR, then exec CMD there.
cg_run() { sh -c 'echo $$ >"$0/cgroup.procs" && exec "$@"' "$@"; }
# cg_mkdir DIR: a fresh, empty cgroup.
cg_mkdir() { rmdir "$1" 2>/dev/null; mkdir "$1"; }
# cg_rmdir DIR: kill what is left inside, then remove (a cgroup with tasks cannot be removed).
cg_rmdir() {
	[ -d "$1" ] || return 0
	if [ -e "$1/cgroup.kill" ]; then echo 1 >"$1/cgroup.kill" 2>/dev/null; fi
	n=20
	while [ "$n" -gt 0 ] && ! rmdir "$1" 2>/dev/null; do
		for pid in $(cat "$1/cgroup.procs" "$1/tasks" 2>/dev/null); do kill -9 "$pid" 2>/dev/null; done
		sleep 0.25
		n=$((n - 1))
	done
	[ ! -d "$1" ]
}
# cg_uptime: seconds since boot with 0.01 s resolution.
cg_uptime() { cut -d' ' -f1 /proc/uptime; }
# cg_ratio A B: A / B with two decimals (awk; 0.00 when B is 0).
cg_ratio() { awk -v a="$1" -v b="$2" 'BEGIN { if (b > 0) printf "%.2f", a / b; else printf "0.00" }'; }
# cg_between X LO HI: succeeds when LO <= X <= HI.
cg_between() { awk -v x="$1" -v lo="$2" -v hi="$3" 'BEGIN { exit !(x >= lo && x <= hi) }'; }
# cg_slower SLOW FREE FACTOR: succeeds when SLOW >= FACTOR * FREE; a FREE run too fast to measure counts as 0.01 s.
cg_slower() { awk -v s="$1" -v f="$2" -v k="$3" 'BEGIN { if (f < 0.01) f = 0.01; exit !(s >= k * f) }'; }
# cg_burn DIR SECONDS CPU: a CPU burner of ctr-test in the cgroup DIR, pinned to the CPU (output to $T/cg-burn-<n>).
cg_burn() { taskset -c "$3" sh -c 'echo $$ >"$0/cgroup.procs" && exec "$@"' "$1" "$CT" cpu "$2"; }

# cg_blkdev_ids: MAJOR:MINOR of CG_BLKDEV.
cg_blkdev_ids() { printf '%d:%d' "0x$(stat -c %t "$CG_BLKDEV")" "0x$(stat -c %T "$CG_BLKDEV")"; }

# ------------------------------------------------------------------------------------------------ cgroup v2
cg_v2() {
	p="$1"
	if [ -z "$CG_V2" ]; then
		for f in memory pids cpu_quota cpu_weight cpuset io freeze kill psi; do kv "$p$f" "unsupported:no cgroup2 hierarchy is mounted"; done
		return 0
	fi
	R="$CG_V2"
	D="$R/toolsprobe"
	ctls="$(cat "$R/cgroup.controllers" 2>/dev/null)"
	# why a controller is not there: where /proc/cgroups binds it (a v1 hierarchy number, or 0 = the v2 hierarchy),
	# and what the kernel answers to an attempt to enable it
	cg_why() {
		legacy="$1"
		[ "$1" != io ] || legacy=blkio # /proc/cgroups lists the cgroup v1 names
		hier="$(awk -v c="$legacy" '$1 == c { print $2 }' /proc/cgroups)"
		err="$( { echo "+$1" >"$R/cgroup.subtree_control"; } 2>&1 | head -n 1 | sed 's/^.*: //')"
		if [ -z "$hier" ]; then echo "unsupported:the kernel has no $1 controller ($legacy is not in /proc/cgroups)"
		elif [ "$hier" != 0 ]; then echo "unsupported:$1 ($legacy in /proc/cgroups) is bound to cgroup v1 hierarchy $hier, so it cannot be in the cgroup2 hierarchy at $R (it has: ${ctls:-none}); enabling it says: ${err:-nothing}"
		else echo "unsupported:$1 is not in cgroup.controllers of $R (it has: ${ctls:-none}); enabling it says: ${err:-nothing}"; fi
	}
	have() { case " $ctls " in *" $1 "*) return 0 ;; esac; return 1; }
	# the controllers are handed down one level
	for c in $ctls; do
		grep -qw "$c" "$R/cgroup.subtree_control" 2>/dev/null || echo "+$c" >"$R/cgroup.subtree_control" 2>/dev/null
	done
	if [ ! -w "$R/cgroup.subtree_control" ]; then
		for f in memory pids cpu_quota cpu_weight cpuset io freeze kill psi; do kv "$p$f" "unsupported:$R is not writable, so no cgroup can be created"; done
		return 0
	fi
	cg_rmdir "$D"
	mkdir "$D" 2>"$T/cg2-mkdir.log" || {
		for f in memory pids cpu_quota cpu_weight cpuset io freeze kill psi; do kv "$p$f" "fail:cannot create $D: $(tail -n 1 "$T/cg2-mkdir.log")"; done
		return 0
	}
	for c in $ctls; do echo "+$c" >"$D/cgroup.subtree_control" 2>/dev/null; done

	# memory: a 32 MiB limit and a process that wants 200 MiB is OOM-killed, and the cgroup counted it
	if have memory; then
		cg_mkdir "$D/mem"
		echo 33554432 >"$D/mem/memory.max"
		echo 0 >"$D/mem/memory.swap.max" 2>/dev/null
		cg_run "$D/mem" "$CT" mem 200 >"$T/cg2-mem.log" 2>&1
		rc=$?
		kills="$(sed -n 's/^oom_kill //p' "$D/mem/memory.events")"
		peak="$(cat "$D/mem/memory.peak" 2>/dev/null || echo 0)"
		if [ "$rc" = 137 ] && [ "${kills:-0}" -ge 1 ] && [ "$peak" -le 37748736 ]; then kv "${p}memory" "ok rc=137 oom_kill=$kills peak=$peak limit=33554432"
		else kv "${p}memory" "fail:rc=$rc oom_kill=${kills:-?} peak=$peak (wanted 137, >=1, <=37748736)"; fi
		cg_rmdir "$D/mem"
	else kv "${p}memory" "$(cg_why memory)"; fi

	# pids: at most 8 tasks; the 9th fork is refused and counted
	if have pids; then
		cg_mkdir "$D/pids"
		echo 8 >"$D/pids/pids.max"
		cg_run "$D/pids" "$CT" fork 30 >"$T/cg2-pids.log" 2>&1
		n="$(sed -n 's/^forked=\([0-9]*\) of 30 (refused)$/\1/p' "$T/cg2-pids.log")"
		hit="$(sed -n 's/^max //p' "$D/pids/pids.events")"
		if [ -n "$n" ] && [ "$n" -le 8 ] && [ "${hit:-0}" -ge 1 ]; then kv "${p}pids" "ok forked=$n of 30 limit=8 refused_events=$hit"
		else kv "${p}pids" "fail:$(cat "$T/cg2-pids.log" | tail -n 1) pids.events max=${hit:-?}"; fi
		cg_rmdir "$D/pids"
	else kv "${p}pids" "$(cg_why pids)"; fi

	# cpu: a quota of half a CPU throttles a busy loop to about half of the wall time, and the cgroup counted it
	if have cpu; then
		cg_mkdir "$D/quota"
		echo "50000 100000" >"$D/quota/cpu.max"
		cg_run "$D/quota" "$CT" cpu 3 >"$T/cg2-cpu.log" 2>&1
		ratio="$(sed -n 's/^cpu-ratio=//p' "$T/cg2-cpu.log")"
		thr="$(sed -n 's/^nr_throttled //p' "$D/quota/cpu.stat")"
		if [ -n "$ratio" ] && cg_between "$ratio" 0.30 0.70 && [ "${thr:-0}" -ge 1 ]; then kv "${p}cpu_quota" "ok cpu-ratio=$ratio quota=0.50 nr_throttled=$thr"
		else kv "${p}cpu_quota" "fail:cpu-ratio=${ratio:-?} nr_throttled=${thr:-?} (wanted 0.30-0.70 and >=1)"; fi
		cg_rmdir "$D/quota"
		# two cgroups on one CPU share it in proportion to cpu.weight (400 : 100)
		cg_mkdir "$D/hi"
		cg_mkdir "$D/lo"
		echo 400 >"$D/hi/cpu.weight"
		echo 100 >"$D/lo/cpu.weight"
		cg_burn "$D/hi" 4 0 >"$T/cg2-hi.log" 2>&1 &
		a=$!
		cg_burn "$D/lo" 4 0 >"$T/cg2-lo.log" 2>&1 &
		b=$!
		wait "$a" "$b"
		uh="$(sed -n 's/^usage_usec //p' "$D/hi/cpu.stat")"
		ul="$(sed -n 's/^usage_usec //p' "$D/lo/cpu.stat")"
		ratio="$(cg_ratio "${uh:-0}" "${ul:-0}")"
		if cg_between "$ratio" 2.0 8.0; then kv "${p}cpu_weight" "ok usage-ratio=$ratio weights=400:100 usage_usec=${uh}:${ul}"
		else kv "${p}cpu_weight" "fail:usage-ratio=$ratio usage_usec=${uh:-?}:${ul:-?} (wanted 2.0-8.0)"; fi
		cg_rmdir "$D/hi"
		cg_rmdir "$D/lo"
	else
		kv "${p}cpu_quota" "$(cg_why cpu)"
		kv "${p}cpu_weight" "$(cg_why cpu)"
	fi

	# cpuset: a group confined to CPU 1 sees only CPU 1
	if have cpuset; then
		if [ "$(nproc --all)" -lt 2 ]; then kv "${p}cpuset" "unsupported:the system has one CPU"
		else
			cg_mkdir "$D/set"
			echo 1 >"$D/set/cpuset.cpus"
			got="$(cg_run "$D/set" sh -c 'sed -n "s/^Cpus_allowed_list:[[:space:]]*//p" /proc/self/status' 2>&1)"
			eff="$(cat "$D/set/cpuset.cpus.effective" 2>/dev/null)"
			if [ "$got" = 1 ] && [ "$eff" = 1 ]; then kv "${p}cpuset" "ok Cpus_allowed_list=$got cpuset.cpus.effective=$eff"
			else kv "${p}cpuset" "fail:Cpus_allowed_list=${got:-?} cpuset.cpus.effective=${eff:-?} (wanted 1)"; fi
			cg_rmdir "$D/set"
		fi
	else kv "${p}cpuset" "$(cg_why cpuset)"; fi

	# io: 1 MiB/s of direct writes to a block device is a 4 MiB write that takes seconds, not milliseconds
	if have io; then
		if [ ! -b "${CG_BLKDEV:-}" ]; then kv "${p}io" "fail:no block device to test with (CG_BLKDEV=${CG_BLKDEV:-unset})"
		else
			ids="$(cg_blkdev_ids)"
			cg_mkdir "$D/iofree"
			cg_mkdir "$D/io"
			t0="$(cg_uptime)"
			cg_run "$D/iofree" dd if=/dev/zero of="$CG_BLKDEV" bs=1M count=4 oflag=direct >"$T/cg2-io0.log" 2>&1
			t1="$(cg_uptime)"
			echo "$ids wbps=1048576" >"$D/io/io.max"
			cg_run "$D/io" dd if=/dev/zero of="$CG_BLKDEV" bs=1M count=4 oflag=direct >"$T/cg2-io1.log" 2>&1
			t2="$(cg_uptime)"
			free="$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", b - a }')"
			slow="$(awk -v a="$t1" -v b="$t2" 'BEGIN { printf "%.2f", b - a }')"
			wbytes="$(sed -n "s/^$ids .*wbytes=\([0-9]*\).*/\1/p" "$D/io/io.stat")"
			if cg_between "$slow" 2.5 30 && cg_slower "$slow" "$free" 3 && [ "${wbytes:-0}" -ge 4194304 ]; then
				kv "${p}io" "ok $ids wbps=1048576: 4 MiB took ${slow}s throttled, ${free}s unthrottled; io.stat wbytes=$wbytes"
			else kv "${p}io" "fail:$ids throttled ${slow}s unthrottled ${free}s io.stat wbytes=${wbytes:-?} (wanted >=2.5s, >=3x slower, wbytes>=4194304)"; fi
			cg_rmdir "$D/io"
			cg_rmdir "$D/iofree"
		fi
	else kv "${p}io" "$(cg_why io)"; fi

	# freeze, kill and pressure are v2 interfaces (cgroup.freeze needs no controller)
	cg_mkdir "$D/fz"
	sh -c 'echo $$ >"$0/cgroup.procs"; exec sleep 60' "$D/fz" &
	sp=$!
	waitfor 5 sh -c "grep -q '^populated 1' $D/fz/cgroup.events"
	echo 1 >"$D/fz/cgroup.freeze" 2>"$T/cg2-freeze.log"
	waitfor 5 sh -c "grep -q '^frozen 1' $D/fz/cgroup.events"
	fz1="$(sed -n 's/^frozen //p' "$D/fz/cgroup.events")"
	echo 0 >"$D/fz/cgroup.freeze" 2>/dev/null
	waitfor 5 sh -c "grep -q '^frozen 0' $D/fz/cgroup.events"
	fz0="$(sed -n 's/^frozen //p' "$D/fz/cgroup.events")"
	if [ "$fz1" = 1 ] && [ "$fz0" = 0 ]; then kv "${p}freeze" "ok frozen=1 then thawed frozen=0"
	elif [ ! -e "$D/fz/cgroup.freeze" ]; then kv "${p}freeze" "unsupported:the kernel has no cgroup.freeze"
	else kv "${p}freeze" "fail:frozen=${fz1:-?} then ${fz0:-?} $(tail -n 1 "$T/cg2-freeze.log")"; fi
	if [ -e "$D/fz/cgroup.kill" ]; then
		echo 1 >"$D/fz/cgroup.kill" 2>"$T/cg2-kill.log"
		waitfor 5 sh -c "grep -q '^populated 0' $D/fz/cgroup.events"
		pop="$(sed -n 's/^populated //p' "$D/fz/cgroup.events")"
		if [ "$pop" = 0 ]; then kv "${p}kill" "ok cgroup.kill emptied the cgroup (populated 0)"
		else kv "${p}kill" "fail:populated=${pop:-?} $(tail -n 1 "$T/cg2-kill.log")"; fi
	else kv "${p}kill" "unsupported:the kernel has no cgroup.kill"; fi
	wait "$sp" 2>/dev/null
	cg_rmdir "$D/fz"
	# pressure: two busy loops pinned to one CPU stall each other, and the group they share says so (pressure files
	# belong to the cgroup core, not to the cpu controller)
	cg_mkdir "$D/psi"
	if cat "$D/psi/cpu.pressure" >"$T/cg2-psi0.log" 2>&1; then
		cg_burn "$D/psi" 3 0 >"$T/cg2-psi1.log" 2>&1 &
		a=$!
		cg_burn "$D/psi" 3 0 >"$T/cg2-psi2.log" 2>&1 &
		b=$!
		wait "$a" "$b"
		psi="$(sed -n 's/^some .* total=//p' "$D/psi/cpu.pressure")"
		if [ "${psi:-0}" -ge 100000 ] 2>/dev/null; then kv "${p}psi" "ok cpu.pressure some total=${psi}us after two busy loops shared one CPU for 3s"
		else kv "${p}psi" "fail:cpu.pressure some total=${psi:-?}us after two busy loops shared one CPU (wanted >=100000)"; fi
	else kv "${p}psi" "unsupported:cpu.pressure is not readable ($(tail -n 1 "$T/cg2-psi0.log" | sed 's/^.*: //')): the kernel has no pressure stall information (psi=0 or CONFIG_PSI off)"; fi
	cg_rmdir "$D/psi"
	cg_rmdir "$D"
}

# ------------------------------------------------------------------------------------------------ cgroup v1
cg_v1() {
	p="$1"
	cg_why1() { echo "unsupported:no cgroup v1 hierarchy has the $1 controller mounted"; }
	M="$(cg_v1dir memory)"
	P="$(cg_v1dir pids)"
	C="$(cg_v1dir cpu)"
	A="$(cg_v1dir cpuacct)"
	S="$(cg_v1dir cpuset)"
	B="$(cg_v1dir blkio)"

	if [ -n "$M" ]; then
		cg_rmdir "$M/toolsprobe"
		mkdir "$M/toolsprobe"
		echo 33554432 >"$M/toolsprobe/memory.limit_in_bytes"
		[ ! -e "$M/toolsprobe/memory.memsw.limit_in_bytes" ] || echo 33554432 >"$M/toolsprobe/memory.memsw.limit_in_bytes" 2>/dev/null
		cg_run "$M/toolsprobe" "$CT" mem 200 >"$T/cg1-mem.log" 2>&1
		rc=$?
		kills="$(sed -n 's/^oom_kill //p' "$M/toolsprobe/memory.oom_control")"
		peak="$(cat "$M/toolsprobe/memory.max_usage_in_bytes" 2>/dev/null || echo 0)"
		if [ "$rc" = 137 ] && [ "${kills:-0}" -ge 1 ] && [ "$peak" -le 37748736 ]; then kv "${p}memory" "ok rc=137 oom_kill=$kills peak=$peak limit=33554432"
		else kv "${p}memory" "fail:rc=$rc oom_kill=${kills:-?} peak=$peak (wanted 137, >=1, <=37748736)"; fi
		cg_rmdir "$M/toolsprobe"
	else kv "${p}memory" "$(cg_why1 memory)"; fi

	if [ -n "$P" ]; then
		cg_rmdir "$P/toolsprobe"
		mkdir "$P/toolsprobe"
		echo 8 >"$P/toolsprobe/pids.max"
		cg_run "$P/toolsprobe" "$CT" fork 30 >"$T/cg1-pids.log" 2>&1
		n="$(sed -n 's/^forked=\([0-9]*\) of 30 (refused)$/\1/p' "$T/cg1-pids.log")"
		if [ -n "$n" ] && [ "$n" -le 8 ]; then kv "${p}pids" "ok forked=$n of 30 limit=8"
		else kv "${p}pids" "fail:$(tail -n 1 "$T/cg1-pids.log")"; fi
		cg_rmdir "$P/toolsprobe"
	else kv "${p}pids" "$(cg_why1 pids)"; fi

	if [ -n "$C" ]; then
		cg_rmdir "$C/toolsprobe"
		mkdir "$C/toolsprobe"
		echo 100000 >"$C/toolsprobe/cpu.cfs_period_us"
		echo 50000 >"$C/toolsprobe/cpu.cfs_quota_us"
		cg_run "$C/toolsprobe" "$CT" cpu 3 >"$T/cg1-cpu.log" 2>&1
		ratio="$(sed -n 's/^cpu-ratio=//p' "$T/cg1-cpu.log")"
		thr="$(sed -n 's/^nr_throttled //p' "$C/toolsprobe/cpu.stat")"
		if [ -n "$ratio" ] && cg_between "$ratio" 0.30 0.70 && [ "${thr:-0}" -ge 1 ]; then kv "${p}cpu_quota" "ok cpu-ratio=$ratio quota=0.50 nr_throttled=$thr"
		else kv "${p}cpu_quota" "fail:cpu-ratio=${ratio:-?} nr_throttled=${thr:-?} (wanted 0.30-0.70 and >=1)"; fi
		cg_rmdir "$C/toolsprobe"
		# cpu.shares 1024 : 256 on one CPU, usage from cpuacct (cpu and cpuacct may be one hierarchy or two)
		if [ -n "$A" ]; then
			cg_rmdir "$C/toolshi"
			cg_rmdir "$C/toolslo"
			mkdir "$C/toolshi" "$C/toolslo"
			[ "$A" = "$C" ] || { cg_rmdir "$A/toolshi"; cg_rmdir "$A/toolslo"; mkdir "$A/toolshi" "$A/toolslo"; }
			echo 1024 >"$C/toolshi/cpu.shares"
			echo 256 >"$C/toolslo/cpu.shares"
			cg_burn2() { taskset -c 0 sh -c 'echo $$ >"$0/cgroup.procs"; echo $$ >"$1/cgroup.procs"; exec "$2" cpu 4' "$1" "$2" "$CT"; }
			cg_burn2 "$C/toolshi" "$A/toolshi" >"$T/cg1-hi.log" 2>&1 &
			a=$!
			cg_burn2 "$C/toolslo" "$A/toolslo" >"$T/cg1-lo.log" 2>&1 &
			b=$!
			wait "$a" "$b"
			uh="$(cat "$A/toolshi/cpuacct.usage" 2>/dev/null)"
			ul="$(cat "$A/toolslo/cpuacct.usage" 2>/dev/null)"
			ratio="$(cg_ratio "${uh:-0}" "${ul:-0}")"
			if cg_between "$ratio" 2.0 8.0; then kv "${p}cpu_weight" "ok usage-ratio=$ratio shares=1024:256 cpuacct.usage=${uh}:${ul}"
			else kv "${p}cpu_weight" "fail:usage-ratio=$ratio cpuacct.usage=${uh:-?}:${ul:-?} (wanted 2.0-8.0)"; fi
			cg_rmdir "$C/toolshi"
			cg_rmdir "$C/toolslo"
			[ "$A" = "$C" ] || { cg_rmdir "$A/toolshi"; cg_rmdir "$A/toolslo"; }
		else kv "${p}cpu_weight" "$(cg_why1 cpuacct)"; fi
	else
		kv "${p}cpu_quota" "$(cg_why1 cpu)"
		kv "${p}cpu_weight" "$(cg_why1 cpu)"
	fi

	if [ -n "$S" ]; then
		if [ "$(nproc --all)" -lt 2 ]; then kv "${p}cpuset" "unsupported:the system has one CPU"
		else
			cg_rmdir "$S/toolsprobe"
			mkdir "$S/toolsprobe"
			cat "$S/cpuset.mems" >"$S/toolsprobe/cpuset.mems"
			echo 1 >"$S/toolsprobe/cpuset.cpus"
			got="$(cg_run "$S/toolsprobe" sh -c 'sed -n "s/^Cpus_allowed_list:[[:space:]]*//p" /proc/self/status' 2>&1)"
			if [ "$got" = 1 ]; then kv "${p}cpuset" "ok Cpus_allowed_list=$got"; else kv "${p}cpuset" "fail:Cpus_allowed_list=${got:-?} (wanted 1)"; fi
			cg_rmdir "$S/toolsprobe"
		fi
	else kv "${p}cpuset" "$(cg_why1 cpuset)"; fi

	if [ -n "$B" ]; then
		if [ ! -b "${CG_BLKDEV:-}" ]; then kv "${p}io" "fail:no block device to test with (CG_BLKDEV=${CG_BLKDEV:-unset})"
		elif [ ! -e "$B/blkio.throttle.write_bps_device" ]; then kv "${p}io" "unsupported:blkio.throttle.write_bps_device is missing (CONFIG_BLK_DEV_THROTTLING is off)"
		else
			ids="$(cg_blkdev_ids)"
			cg_rmdir "$B/toolsprobe"
			cg_rmdir "$B/toolsfree"
			mkdir "$B/toolsprobe" "$B/toolsfree"
			t0="$(cg_uptime)"
			cg_run "$B/toolsfree" dd if=/dev/zero of="$CG_BLKDEV" bs=1M count=4 oflag=direct >"$T/cg1-io0.log" 2>&1
			t1="$(cg_uptime)"
			echo "$ids 1048576" >"$B/toolsprobe/blkio.throttle.write_bps_device"
			cg_run "$B/toolsprobe" dd if=/dev/zero of="$CG_BLKDEV" bs=1M count=4 oflag=direct >"$T/cg1-io1.log" 2>&1
			t2="$(cg_uptime)"
			free="$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", b - a }')"
			slow="$(awk -v a="$t1" -v b="$t2" 'BEGIN { printf "%.2f", b - a }')"
			wbytes="$(awk -v d="$ids" '$1 == d && $2 == "Write" { print $3 }' "$B/toolsprobe/blkio.throttle.io_service_bytes")"
			if cg_between "$slow" 2.5 30 && cg_slower "$slow" "$free" 3 && [ "${wbytes:-0}" -ge 4194304 ]; then
				kv "${p}io" "ok $ids write_bps=1048576: 4 MiB took ${slow}s throttled, ${free}s unthrottled; blkio wbytes=$wbytes"
			else kv "${p}io" "fail:$ids throttled ${slow}s unthrottled ${free}s blkio wbytes=${wbytes:-?} (wanted >=2.5s, >=3x slower, wbytes>=4194304)"; fi
			cg_rmdir "$B/toolsprobe"
			cg_rmdir "$B/toolsfree"
		fi
	else kv "${p}io" "$(cg_why1 blkio)"; fi
}

# ------------------------------------------------------------------------------------------------ stress-ng in a memory limit
# cg_stress_ng PREFIX: stress-ng with a 200 MiB vm worker in a 32 MiB memory cgroup, through whichever API provides the memory
# controller (cgroup v2 if it has one, else v1). The worker is OOM-killed again and again, stress-ng itself completes.
# PREFIXstress_ng: ok <stress-ng's verdict> | fail:... | unsupported:...    PREFIXstress_ng_oom_kills: the OOM kills counted
cg_stress_ng() {
	p="$1"
	d=""
	if [ -n "$CG_V2" ] && grep -qw memory "$CG_V2/cgroup.controllers" 2>/dev/null; then
		echo +memory >"$CG_V2/cgroup.subtree_control" 2>/dev/null
		d="$CG_V2/toolsstress"
		cg_rmdir "$d"
		mkdir "$d" && echo 33554432 >"$d/memory.max" && echo 0 >"$d/memory.swap.max" 2>/dev/null
		oomk() { sed -n 's/^oom_kill //p' "$d/memory.events"; }
	elif [ -n "$(cg_v1dir memory)" ]; then
		d="$(cg_v1dir memory)/toolsstress"
		cg_rmdir "$d"
		mkdir "$d" && echo 33554432 >"$d/memory.limit_in_bytes"
		[ ! -e "$d/memory.memsw.limit_in_bytes" ] || echo 33554432 >"$d/memory.memsw.limit_in_bytes" 2>/dev/null
		oomk() { sed -n 's/^oom_kill //p' "$d/memory.oom_control"; }
	fi
	if [ -z "$d" ]; then
		kv "${p}stress_ng" "unsupported:no cgroup hierarchy has a memory controller"
		kv "${p}stress_ng_oom_kills" "unsupported:no cgroup hierarchy has a memory controller"
		return 0
	fi
	if [ ! -d "$d" ]; then
		kv "${p}stress_ng" "fail:cannot create $d"
		kv "${p}stress_ng_oom_kills" "fail:cannot create $d"
		return 0
	fi
	before="$(oomk)"
	cg_run "$d" stress-ng --vm 1 --vm-bytes 200M --vm-keep --timeout 3s >"$T/cg-stress.log" 2>&1
	v="$(grep -m 1 'successful run completed' "$T/cg-stress.log" | cut -c1-200)"
	if [ -n "$v" ]; then kv "${p}stress_ng" "ok $v"; else kv "${p}stress_ng" "fail:$(tail -n 1 "$T/cg-stress.log" | cut -c1-200)"; fi
	n=$(($(oomk) - ${before:-0}))
	if [ "$n" -ge 1 ]; then kv "${p}stress_ng_oom_kills" "ok $n"; else kv "${p}stress_ng_oom_kills" "fail:the limit did not kill the worker ($n OOM kills)"; fi
	cg_rmdir "$d"
}
