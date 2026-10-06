#!/bin/sh
# Runs as the last step of the cgroup v2 test bed (testbed/launch.sh -> QEMU -> testbed/guest-init.sh): real root of a
# kernel that was booted with cgroup_no_v1=all, so the whole of cgroup v2 is available and nothing else is. Prints
# one KEY=VALUE line per fact (prefix g_); tools-test.sh judges them against inventory.tsv like the facts of the
# unprivileged and privileged sessions. What this session adds is what the cloud host cannot provide: a unified
# hierarchy with every controller, privileges over all of it, and a kernel with the tracing, schedstat and BPF
# options the host kernel lacks. When QEMU runs it with software emulation (no /dev/kvm) the checks are functional,
# not timing-accurate; launch.sh reports which accelerator ran (guest_accel).
cd /work || exit 1
. /work/common.sh
. /work/cgroup.sh
CT=/work/ctr/ctr-test
CG_BLKDEV=/dev/vda

# rtt_avg FILE: the average round trip of a busybox or iputils ping, in ms.
rtt_avg() { sed -n 's|.*min/avg/max[a-z/]* = [0-9.]*/\([0-9.]*\)/.*|\1|p' "$1" | head -n 1; }

# ------------------------------------------------------------------------------------------------ the guest itself
kv g_kernel "$(uname -r)"
last="$(cat /proc/sys/kernel/cap_last_cap)"
mask="$(printf '%016x' $(((1 << (last + 1)) - 1)))"
caps="$(sed -n 's/^CapEff:[[:space:]]*//p' /proc/self/status)"
if [ "$(id -u)" = 0 ] && [ "$caps" = "$mask" ]; then kv g_privileges "ok uid=0 CapEff=$caps (all $((last + 1)) capabilities)"
else kv g_privileges "fail:uid=$(id -u) CapEff=$caps (wanted $mask)"; fi
kv g_cpus "$(nproc --all)"
# a process of the guest root can raise its hard limits again and set the clock: what the unprivileged sandbox is refused
if sh -c 'ulimit -n 1024 && ulimit -Hn 2048' >"$T/fdraise.log" 2>&1; then kv g_fd_hard_raise "ok the hard nofile limit was lowered to 1024 and raised again to 2048"; else kv g_fd_hard_raise "fail:$(lastline "$T/fdraise.log")"; fi
if python3 -c 'import time; t = time.clock_gettime(time.CLOCK_REALTIME); time.clock_settime(time.CLOCK_REALTIME, t); print("set")' >"$T/clock.log" 2>&1; then kv g_set_clock "ok clock_settime(CLOCK_REALTIME) succeeded"; else kv g_set_clock "fail:$(lastline "$T/clock.log")"; fi
# loadable modules: the guest kernel loads its modules from the initramfs, which the host kernel (no module support) cannot
n="$(wc -l </proc/modules)"
if [ "$n" -gt 0 ]; then kv g_modules "ok $n loaded"; else kv g_modules "fail:/proc/modules is empty, the guest kernel loaded no module"; fi

# ------------------------------------------------------------------------------------------------ cgroup v2
cg_detect
cg_layout_facts g_
cg_v2 g_cgv2_

# ------------------------------------------------------------------------------------------------ namespaces
# Each kind of namespace isolates one resource view; the evidence is a different namespace id and a changed view.
ns_id() { readlink "/proc/self/ns/$1"; }
mkdir -p "$T/ns"
# pid: the first process of a new pid namespace is 1 and sees only its own processes
out="$(unshare --fork --pid --mount-proc sh -c 'echo $$; ls /proc | grep -c "^[0-9][0-9]*$"' 2>&1 | tr '\n' ' ')"
vis_in="${out#* }"
vis_in="${vis_in% }"
vis_out="$(ls /proc | grep -c '^[0-9][0-9]*$')"
case "$out" in
"1 "[0-9]*) if [ "$vis_in" -le 3 ] && [ "$vis_in" -lt "$vis_out" ]; then kv g_ns_pid "ok pid=1 sees $vis_in processes, the guest has $vis_out"; else kv g_ns_pid "fail:pid=1 but sees $vis_in processes (guest $vis_out)"; fi ;;
*) kv g_ns_pid "fail:$out" ;;
esac
# mount: a mount made inside is not visible outside
mkdir -p "$T/ns/m"
unshare --mount sh -c "mount -t tmpfs tmpfs $T/ns/m && touch $T/ns/m/inside" >"$T/ns/m.log" 2>&1
if [ ! -e "$T/ns/m/inside" ] && ! grep -q " $T/ns/m " /proc/mounts; then kv g_ns_mnt "ok a tmpfs mounted in a new mount namespace is not visible outside"
else kv g_ns_mnt "fail:$(tail -n 1 "$T/ns/m.log")"; fi
# uts: the host name set inside stays inside
inside="$(unshare --uts sh -c 'hostname isolated && hostname' 2>&1)"
if [ "$inside" = isolated ] && [ "$(hostname)" != isolated ]; then kv g_ns_uts "ok hostname=$inside inside, $(hostname) outside"; else kv g_ns_uts "fail:inside=$inside outside=$(hostname)"; fi
# ipc: a different namespace
other="$(unshare --ipc readlink /proc/self/ns/ipc 2>&1)"
if [ -n "$other" ] && [ "$other" != "$(ns_id ipc)" ]; then kv g_ns_ipc "ok $other (guest: $(ns_id ipc))"; else kv g_ns_ipc "fail:$other"; fi
# net: a new network namespace has only a loopback
links="$(unshare --net sh -c 'ip -o link show | cut -d: -f2 | tr -d " \n"' 2>&1)"
if [ "$links" = lo ]; then kv g_ns_net "ok a new network namespace has only: $links"; else kv g_ns_net "fail:links=$links"; fi
# user: root inside is an unprivileged id outside (an ordinary user creates the namespace)
out="$(su -s /bin/sh nobody -c "unshare --user --map-root-user sh -c 'id -u; cat /proc/self/uid_map'" 2>&1 | tr -s ' \n' ' ')"
case "$out" in
"0 0 65534 1 "*) kv g_ns_user "ok uid 65534 is root inside a user namespace (uid_map: 0 65534 1)" ;;
*) kv g_ns_user "fail:$out" ;;
esac
# cgroup: a process in a sub-cgroup sees its cgroup as the root of its own namespace
mkdir -p "$CG_V2/toolsns" 2>/dev/null
mine="$(cg_run "$CG_V2/toolsns" cat /proc/self/cgroup 2>&1)"
view="$(cg_run "$CG_V2/toolsns" unshare --cgroup cat /proc/self/cgroup 2>&1)"
if [ "$mine" = "0::/toolsns" ] && [ "$view" = "0::/" ]; then kv g_ns_cgroup "ok the cgroup $mine is seen as $view from a new cgroup namespace"; else kv g_ns_cgroup "fail:outside=$mine inside=$view"; fi
cg_rmdir "$CG_V2/toolsns"

# ------------------------------------------------------------------------------------------------ network isolation
N="$T/net"
mkdir -p "$N"
if ip netns add toolsa 2>"$N/setup.log" &&
	ip link add veth0 type veth peer name veth1 2>>"$N/setup.log" &&
	ip link set veth1 netns toolsa 2>>"$N/setup.log" &&
	ip addr add 10.77.0.1/24 dev veth0 2>>"$N/setup.log" && ip link set veth0 up 2>>"$N/setup.log" &&
	ip netns exec toolsa sh -c 'ip addr add 10.77.0.2/24 dev veth1 && ip link set veth1 up && ip link set lo up' 2>>"$N/setup.log"; then
	ping -c 3 -W 2 10.77.0.2 >"$N/ping.log" 2>&1
	if grep -q ' 0% packet loss' "$N/ping.log"; then kv g_veth_ping "ok 3 of 3 packets through a veth pair into a network namespace, rtt avg $(rtt_avg "$N/ping.log") ms"
	else kv g_veth_ping "fail:$(tail -n 2 "$N/ping.log" | tr '\n' ' ')"; fi
	# a namespace without a link to the first one cannot reach it
	ip netns add toolsb 2>/dev/null
	ip netns exec toolsb ping -c 1 -W 1 10.77.0.1 >"$N/ping-b.log" 2>&1
	if grep -qE 'Network (is )?unreachable|100% packet loss' "$N/ping-b.log"; then kv g_netns_isolated "ok a namespace with no link cannot reach 10.77.0.1: $(tail -n 1 "$N/ping-b.log")"
	else kv g_netns_isolated "fail:$(tail -n 1 "$N/ping-b.log")"; fi
	ip netns del toolsb 2>/dev/null
	# nftables inside the namespace drops ICMP, then lets it through again
	if ip netns exec toolsa nft -f - >"$N/nft.log" 2>&1 <<'EOF'
add table inet toolsprobe
add chain inet toolsprobe input { type filter hook input priority 0 ; }
add rule inet toolsprobe input meta l4proto icmp counter drop
EOF
	then
		ping -c 2 -W 1 10.77.0.2 >"$N/ping-drop.log" 2>&1
		counted="$(ip netns exec toolsa nft list ruleset | sed -n 's/.*counter packets \([0-9]*\) bytes.*/\1/p')"
		if grep -q '100% packet loss' "$N/ping-drop.log" && [ "${counted:-0}" -ge 1 ]; then kv g_nft_drop "ok ICMP dropped by an nft rule in the namespace (counter packets=$counted)"
		else kv g_nft_drop "fail:$(tail -n 1 "$N/ping-drop.log") counter=${counted:-?}"; fi
		ip netns exec toolsa nft delete table inet toolsprobe
		ping -c 2 -W 2 10.77.0.2 >"$N/ping-allow.log" 2>&1
		if grep -q ' 0% packet loss' "$N/ping-allow.log"; then kv g_nft_allow "ok traffic flows again after the rule is deleted"; else kv g_nft_allow "fail:$(tail -n 1 "$N/ping-allow.log")"; fi
	else
		kv g_nft_drop "fail:$(tail -n 1 "$N/nft.log")"
		kv g_nft_allow "fail:no rule to delete"
	fi
	# netem: 20 ms of delay on each end of the link is at least 40 ms of round trip
	if tc qdisc add dev veth0 root netem delay 20ms 2>"$N/netem.log" && ip netns exec toolsa tc qdisc add dev veth1 root netem delay 20ms 2>>"$N/netem.log"; then
		ping -c 4 -W 2 10.77.0.2 >"$N/ping-delay.log" 2>&1
		avg="$(rtt_avg "$N/ping-delay.log")"
		if [ -n "$avg" ] && cg_between "$avg" 39 400; then kv g_netem_delay "ok rtt avg ${avg} ms with 2 x 20 ms netem delay"; else kv g_netem_delay "fail:rtt avg ${avg:-?} ms (wanted 39-400)"; fi
		tc qdisc del dev veth0 root
		ip netns exec toolsa tc qdisc del dev veth1 root
	else kv g_netem_delay "fail:$(tail -n 1 "$N/netem.log")"; fi
	# netem rate: a 10 Mbit/s limit holds a TCP stream to about 10 Mbit/s
	if tc qdisc add dev veth0 root netem rate 10mbit 2>"$N/rate.log"; then
		ip netns exec toolsa iperf3 -s -1 -p 5202 >"$N/iperf-server.log" 2>&1 &
		waitfor 5 sh -c "ip netns exec toolsa ss -ltn | grep -q :5202"
		iperf3 -c 10.77.0.2 -p 5202 -t 4 -f m >"$N/iperf.log" 2>&1
		mbit="$(sed -n 's/.*[[:space:]]\([0-9.]*\) Mbits\/sec.*receiver$/\1/p' "$N/iperf.log" | tail -n 1)"
		if [ -n "$mbit" ] && cg_between "$mbit" 5 12; then kv g_netem_rate "ok iperf3 receiver measured $mbit Mbit/s through a 10 Mbit/s netem rate limit"; else kv g_netem_rate "fail:iperf3 measured ${mbit:-?} Mbit/s (wanted 5-12) $(tail -n 1 "$N/iperf.log")"; fi
		wait 2>/dev/null
		tc qdisc del dev veth0 root
	else kv g_netem_rate "fail:$(tail -n 1 "$N/rate.log")"; fi
	ip link add dm0 type dummy 2>"$N/dummy.log" && ip addr add 10.78.0.1/24 dev dm0 && ip link set dm0 up
	if ip -o addr show dev dm0 | grep -q '10.78.0.1/24'; then kv g_dummy_link "ok dummy link dm0 with 10.78.0.1/24"; else kv g_dummy_link "fail:$(tail -n 1 "$N/dummy.log")"; fi
	ip link add br0 type bridge 2>"$N/bridge.log" && ip link set veth0 master br0 2>>"$N/bridge.log"
	if ip -d link show veth0 | grep -q 'master br0'; then kv g_bridge "ok veth0 enslaved to bridge br0"; else kv g_bridge "fail:$(tail -n 1 "$N/bridge.log")"; fi
	# the rule libvirt installs on a virtual network bridge to fill in DHCP checksums
	tc qdisc add dev veth0 ingress >"$N/csum.log" 2>&1 &&
		tc filter add dev veth0 parent ffff: protocol ip prio 1 u32 match ip protocol 17 0xff match ip dport 68 0xffff action csum udp >>"$N/csum.log" 2>&1
	if tc filter show dev veth0 parent ffff: 2>/dev/null | grep -q csum; then kv g_tc_csum "ok a tc u32 filter with action csum on veth0"; else kv g_tc_csum "fail:$(lastline "$N/csum.log")"; fi
	ip netns del toolsa 2>/dev/null
else
	for f in veth_ping netns_isolated nft_drop nft_allow netem_delay netem_rate dummy_link bridge tc_csum; do kv "g_$f" "fail:network namespace setup: $(tail -n 1 "$N/setup.log")"; done
fi

# ------------------------------------------------------------------------------------------------ block device, file systems, loop
sz="$(blockdev --getsize64 /dev/vda 2>&1)"
ser="$(cat /sys/block/vda/serial 2>&1)"
if [ "$sz" = 67108864 ] && [ "$ser" = aidisk0 ]; then kv g_virtio_blk "ok /dev/vda serial=$ser size=$sz"; else kv g_virtio_blk "fail:size=$sz serial=$ser"; fi
mkdir -p "$T/mnt"
if mkfs.ext4 -q -F /dev/vda >"$T/mkfs.log" 2>&1 && mount /dev/vda "$T/mnt" && echo guest-data >"$T/mnt/probe" && sync && umount "$T/mnt" &&
	e2fsck -fn /dev/vda >"$T/fsck.log" 2>&1 && mount -o ro /dev/vda "$T/mnt" && [ "$(cat "$T/mnt/probe")" = guest-data ]; then
	kv g_ext4 "ok mkfs.ext4, mount, write, umount, e2fsck clean, read-only remount reads it back"
else kv g_ext4 "fail:$(tail -n 1 "$T/mkfs.log")"; fi
umount "$T/mnt" 2>/dev/null
mkdir -p "$T/lp/tree"
echo loop-data >"$T/lp/tree/probe"
mke2fs -q -t ext4 -d "$T/lp/tree" "$T/lp/img" 8M
dev="$(losetup --find --show "$T/lp/img" 2>"$T/lp/err.log")"
if [ -n "$dev" ] && mount "$dev" "$T/mnt" 2>>"$T/lp/err.log" && [ "$(cat "$T/mnt/probe")" = loop-data ]; then kv g_loop_ext4 "ok $dev mounted and read"; else kv g_loop_ext4 "fail:${dev:-no loop device} $(tail -n 1 "$T/lp/err.log")"; fi
umount "$T/mnt" 2>/dev/null
[ -z "$dev" ] || losetup -d "$dev"
# the file systems and the partition table parsers that the cloud host's kernel lacks, with the same tools and images
# the host session uses: here the kernel has them, so a failure on the host is the host's, not the toolchain's
# fs_roundtrip KEY TYPE SIZE MKFS...: make an image of SIZE, format it with MKFS (the image path is appended), mount it as
# TYPE through a loop device, write, remount read-only, read back
fs_roundtrip() {
	k="$1"
	t="$2"
	sz="$3"
	shift 3
	img="$T/$k.img"
	truncate -s "$sz" "$img"
	if ! "$@" "$img" >"$T/$k.log" 2>&1; then kv "$k" "fail:$1: $(tail -n 1 "$T/$k.log")"
	elif mount -t "$t" -o loop "$img" "$T/mnt" 2>>"$T/$k.log" && echo guest-data >"$T/mnt/probe" && sync && umount "$T/mnt" &&
		mount -t "$t" -o loop,ro "$img" "$T/mnt" 2>>"$T/$k.log" && [ "$(cat "$T/mnt/probe")" = guest-data ]; then
		kv "$k" "ok $t: $1, loop mount, write, read-only remount, read back"
	else kv "$k" "fail:$(tail -n 1 "$T/$k.log")"; fi
	umount "$T/mnt" 2>/dev/null
	rm -f "$img"
}
fs_roundtrip g_mount_xfs xfs 320M mkfs.xfs -q
fs_roundtrip g_mount_btrfs btrfs 128M mkfs.btrfs -q -f
fs_roundtrip g_mount_vfat vfat 16M mkfs.fat -F 16
# a GPT partition table is parsed by the kernel: losetup -P creates the partition nodes without partx
truncate -s 64M "$T/lp/disk.img"
printf 'label: gpt\nstart=2048, size=16384, type=L\nsize=+, type=U\n' | sfdisk -q "$T/lp/disk.img"
pdev="$(losetup --find --show -P "$T/lp/disk.img" 2>"$T/lp/pscan.log")"
waitfor 5 test -b "${pdev}p2"
nodes="$(ls "$pdev"p* 2>/dev/null | sed "s|^$pdev||" | tr '\n' ' ' | sed 's/ $//')"
if [ "$nodes" = "p1 p2" ]; then kv g_loop_partscan "ok $pdev has the partition nodes $nodes created by the kernel's GPT parser"; else kv g_loop_partscan "fail:nodes='$nodes' $(tail -n 1 "$T/lp/pscan.log")"; fi
[ -z "$pdev" ] || losetup -d "$pdev"

# ------------------------------------------------------------------------------------------------ tracing, profiling, scheduler statistics
TR=/sys/kernel/tracing
I="$TR/instances/toolsprobe"
if mkdir "$I" 2>"$T/ftrace.log"; then
	echo 1 >"$I/events/sched/sched_switch/enable"
	echo 1 >"$I/tracing_on"
	sleep 0.3
	echo 0 >"$I/tracing_on"
	ev="$(grep -c 'sched_switch:' "$I/trace")"
	if [ "$ev" -ge 1 ]; then kv g_ftrace_events "ok $ev sched_switch events in a private trace instance"; else kv g_ftrace_events "fail:no sched_switch events"; fi
	echo 0 >"$I/events/sched/sched_switch/enable"
	echo 0 >"$I/tracing_on"
	echo vfs_read >"$I/set_ftrace_filter" 2>"$T/ftrace.log"
	echo function >"$I/current_tracer" 2>>"$T/ftrace.log"
	echo >"$I/trace"
	echo 1 >"$I/tracing_on"
	# dd reads with read(2); busybox cat copies with sendfile(2), which never reaches vfs_read
	dd if=/etc/hostname of=/dev/null bs=1 count=4 2>/dev/null
	echo 0 >"$I/tracing_on"
	fn="$(grep -c 'vfs_read' "$I/trace")"
	if [ "$fn" -ge 1 ]; then kv g_ftrace_function "ok the function tracer recorded $fn calls of vfs_read"; else kv g_ftrace_function "fail:tracer=$(cat "$I/current_tracer") $(tail -n 1 "$T/ftrace.log")"; fi
	echo nop >"$I/current_tracer"
	rmdir "$I"
else
	kv g_ftrace_events "fail:$(tail -n 1 "$T/ftrace.log")"
	kv g_ftrace_function "fail:$(tail -n 1 "$T/ftrace.log")"
fi
perf stat -e sched:sched_switch -a -- sleep 0.3 >"$T/perf-tp.log" 2>&1
tp="$(sed -n 's/^[[:space:]]*\([0-9][0-9,.]*\)[[:space:]]*sched:sched_switch.*/\1/p' "$T/perf-tp.log" | head -n 1)"
if [ -n "$tp" ] && [ "$tp" != 0 ]; then kv g_perf_tracepoint "ok perf counted $tp sched:sched_switch events system-wide"; else kv g_perf_tracepoint "fail:$(tail -n 1 "$T/perf-tp.log")"; fi
perf stat -e task-clock -- "$CT" cpu 1 >"$T/perf-sw.log" 2>&1
tc="$(sed -n 's/^[[:space:]]*\([0-9][0-9,.]*\)[[:space:]]*msec task-clock.*/\1/p' "$T/perf-sw.log" | head -n 1)"
if [ -n "$tc" ]; then kv g_perf_software "ok perf task-clock (a software counter) measured $tc msec"; else kv g_perf_software "fail:$(tail -n 1 "$T/perf-sw.log")"; fi
# a hardware counter exists only if the (virtual) CPU has a PMU: software emulation has none, KVM only with pmu=on
perf stat -e cycles -- true >"$T/perf-hw.log" 2>&1
if grep -qE '<not supported>|<not counted>|not supported|No such file|Permission' "$T/perf-hw.log"; then
	kv g_perf_hw_cycles "unsupported:$(grep -m 1 -E 'not supported|not counted|No such file|Permission' "$T/perf-hw.log" | sed 's/^[[:space:]]*//' | cut -c1-160)"
elif grep -qE '^[[:space:]]*[1-9][0-9,.]*[[:space:]]+(cpu_core/)?cycles' "$T/perf-hw.log"; then kv g_perf_hw_cycles "ok $(grep -m 1 -E 'cycles' "$T/perf-hw.log" | sed 's/^[[:space:]]*//' | cut -c1-120)"
else kv g_perf_hw_cycles "fail:$(tail -n 1 "$T/perf-hw.log")"; fi
# scheduler statistics: the kernel's CPU time accounting grows by about a second over a one second busy loop
echo 1 >/proc/sys/kernel/sched_schedstats 2>/dev/null
if [ -e /proc/schedstat ]; then
	ver="$(sed -n 's/^version //p' /proc/schedstat)"
	b="$(awk '/^cpu/ { s += $8 } END { print s + 0 }' /proc/schedstat)"
	"$CT" cpu 1 >/dev/null
	a="$(awk '/^cpu/ { s += $8 } END { print s + 0 }' /proc/schedstat)"
	grown="$(awk -v a="$a" -v b="$b" 'BEGIN { printf "%.0f", a - b }')"
	if [ "$grown" -ge 500000000 ]; then kv g_schedstat "ok /proc/schedstat version $ver: rq_cpu_time grew ${grown}ns over a 1s busy loop"; else kv g_schedstat "fail:rq_cpu_time grew ${grown}ns (wanted >=500000000), sched_schedstats=$(cat /proc/sys/kernel/sched_schedstats 2>&1)"; fi
else kv g_schedstat "unsupported:no /proc/schedstat (CONFIG_SCHEDSTATS is off)"; fi
if cat /proc/pressure/cpu /proc/pressure/memory /proc/pressure/io >"$T/psi.log" 2>&1; then kv g_psi_system "ok $(head -n 1 /proc/pressure/cpu | cut -c1-80)"; else kv g_psi_system "fail:$(tail -n 1 "$T/psi.log")"; fi

# ------------------------------------------------------------------------------------------------ eBPF: a device filter attached to a cgroup
P="$T/bpf"
mkdir -p "$P"
mount -t bpf bpf /sys/fs/bpf 2>/dev/null
bpftool feature probe kernel >"$P/feature.log" 2>&1
if grep -q 'eBPF program_type cgroup_device is available' "$P/feature.log"; then kv g_bpf_feature "ok eBPF program type cgroup_device is available"; else kv g_bpf_feature "fail:$(tail -n 1 "$P/feature.log")"; fi
if bpftool prog load /work/bpf/guest.o /sys/fs/bpf/refuse_null >"$P/load.log" 2>&1; then
	kv g_bpf_prog_load "ok $(bpftool prog show pinned /sys/fs/bpf/refuse_null | head -n 1 | cut -c1-100)"
	cg_mkdir "$CG_V2/toolsdev"
	if bpftool cgroup attach "$CG_V2/toolsdev" device pinned /sys/fs/bpf/refuse_null >"$P/attach.log" 2>&1; then
		kv g_bpf_cgroup_attach "ok $(bpftool cgroup show "$CG_V2/toolsdev" | tail -n 1 | tr -s ' ' | cut -c1-100)"
		cg_run "$CG_V2/toolsdev" cat /dev/null >"$P/null.log" 2>&1
		nrc=$?
		cg_run "$CG_V2/toolsdev" head -c 1 /dev/zero >"$P/zero.log" 2>&1
		zrc=$?
		cat /dev/null
		orc=$?
		if [ "$nrc" != 0 ] && grep -q 'Operation not permitted' "$P/null.log" && [ "$zrc" = 0 ] && [ "$orc" = 0 ]; then kv g_bpf_device_filter "ok /dev/null is refused inside the cgroup ($(tail -n 1 "$P/null.log" | cut -c1-80)), /dev/zero and the guest are not"
		else kv g_bpf_device_filter "fail:cat /dev/null rc=$nrc ($(tail -n 1 "$P/null.log")), head /dev/zero rc=$zrc"; fi
	else
		kv g_bpf_cgroup_attach "fail:$(tail -n 1 "$P/attach.log")"
		kv g_bpf_device_filter "fail:the filter is not attached"
	fi
	cg_rmdir "$CG_V2/toolsdev"
	rm -f /sys/fs/bpf/refuse_null
else
	kv g_bpf_prog_load "fail:$(tail -n 1 "$P/load.log")"
	kv g_bpf_cgroup_attach "fail:no program"
	kv g_bpf_device_filter "fail:no program"
fi
if bpftool map create /sys/fs/bpf/tmap type array key 4 value 4 entries 4 name toolsmap >"$P/map.log" 2>&1 &&
	bpftool map update pinned /sys/fs/bpf/tmap key 0 0 0 0 value 42 0 0 0 >>"$P/map.log" 2>&1 &&
	bpftool map dump pinned /sys/fs/bpf/tmap 2>&1 | grep -qE '"value": *\[ *42|value: 2a'; then kv g_bpf_map "ok array map created, updated and dumped (value 42)"; else kv g_bpf_map "fail:$(tail -n 1 "$P/map.log")"; fi
rm -f /sys/fs/bpf/tmap
