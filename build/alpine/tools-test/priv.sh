#!/bin/sh
# Runs inside the tools image as the real root of the host in private mount, PID, IPC, UTS, network and cgroup
# namespaces (tools-test.sh -> tools-run.sh --privileged), offline. Exercises what needs host privileges: loop
# devices and kernel mounts, FUSE, ftrace and perf tracepoints, eBPF, network namespaces and firewalls, cgroup
# limits, libvirt and Docker. Prints one KEY=VALUE line per fact; tools-test.sh judges them against inventory.tsv.
#   priv.sh [section...]     default: every section (useful for iterating: priv.sh loop net)
cd /work || exit 1
. /work/common.sh

# waitfor SECONDS CMD...: poll CMD every 0.5 s until it succeeds.
waitfor() {
	n=$(($1 * 2))
	shift
	while [ "$n" -gt 0 ]; do
		"$@" >/dev/null 2>&1 && return 0
		sleep 0.5
		n=$((n - 1))
	done
	return 1
}

# mountfs DIR: the file system type mounted on DIR, from /proc/mounts.
mountfs() { awk -v d="$1" '$2 == d { t = $3 } END { print t ? t : "none" }' /proc/mounts; }
# mounterr KEY MOUNT-ARGS...: "ok", or "fail:" and mount's own error line.
mounterr() {
	k="$1"
	shift
	if mount "$@" >"$T/$k.log" 2>&1; then kv "$k" ok; else kv "$k" "fail:$(grep -m 1 '^mount:' "$T/$k.log" | cut -c1-200)"; fi
}

# The static test program of the container, VM and cgroup tests.
gcc -static -Os -o "$T/ctr-test" ctr/ctr-test.c 2>"$T/ctr-test-cc.log" || kv ctr_test_build "fail:$(tail -n 1 "$T/ctr-test-cc.log")"

# ------------------------------------------------------------------------------------------------ the session
if section session; then
	kv real_root "$(awk '$1 == 0 && $2 == 0 && $3 >= 4294967295 { print "yes" }' /proc/self/uid_map)"
	kv hostname "$(hostname)"
	kv pid_namespace "$(tr '\0' ' ' </proc/1/cmdline | grep -q inner-privileged && echo private || echo shared)"
	kv net_namespace_ifaces "$(ls /sys/class/net | tr '\n' ' ')"
	kv cgroup_layout "$(mountfs /sys/fs/cgroup/memory) $(mountfs /sys/fs/cgroup/unified)"
	kv tracefs "$(mountfs /sys/kernel/tracing)"
	kv lo_state "$(ip -o link show lo | sed -n 's/.*<\([^>]*\)>.*/\1/p')"
fi

# ------------------------------------------------------------------------------------------------ loop devices and kernel mounts
if section loop; then
	L="$T/loop"
	mkdir -p "$L/tree/etc" "$L/tree/data" "$L/mnt"
	printf 'hello image\n' >"$L/tree/etc/motd"
	seq 1 20000 >"$L/tree/data/numbers"
	mke2fs -q -t ext4 -d "$L/tree" -L loopfs "$L/ext4.img" 16M
	dev="$(losetup --find --show "$L/ext4.img" 2>"$T/losetup.log")"
	kv losetup_attach "${dev:-fail:$(tail -n 1 "$T/losetup.log")}"
	found losetup_list "$L/ext4.img" losetup -a
	found loop_blkid 'TYPE="ext4"' blkid "$dev"
	found loop_lsblk '"name": *"loop' lsblk -J "$dev"
	run loop_mount_ext4 mount "$dev" "$L/mnt"
	found loop_read '^hello image$' cat "$L/mnt/etc/motd"
	run loop_write sh -c "echo written >$L/mnt/data/new && sync"
	found loop_findmnt "$L/mnt" findmnt -n -o TARGET,FSTYPE "$L/mnt"
	found loop_df 'loop' df "$L/mnt"
	run loop_umount umount "$L/mnt"
	run loop_fsck_after e2fsck -fn "$dev"
	found loop_written 'new' debugfs -R 'ls /data' "$L/ext4.img"
	run losetup_detach losetup -d "$dev"

	# several mounts of one image, a mount with -o loop, bind and remount
	run mount_o_loop mount -o loop,ro "$L/ext4.img" "$L/mnt"
	run mount_ro_refuses_write sh -c "! touch $L/mnt/etc/x 2>/dev/null"
	mkdir -p "$T/bindtarget"
	run mount_bind mount --bind "$L/mnt/etc" "$T/bindtarget"
	found mount_bind_read '^hello image$' cat "$T/bindtarget/motd"
	umount "$T/bindtarget"
	umount "$L/mnt"

	# squashfs and erofs through the kernel
	mksquashfs "$L/tree" "$L/a.sqsh" -quiet -noappend -all-root >/dev/null 2>&1
	run mount_squashfs mount -t squashfs -o loop,ro "$L/a.sqsh" "$L/mnt"
	found squashfs_read '^hello image$' cat "$L/mnt/etc/motd"
	umount "$L/mnt" 2>/dev/null
	mkfs.erofs -zlz4hc "$L/a.erofs" "$L/tree" >/dev/null 2>&1
	run mount_erofs mount -t erofs -o loop,ro "$L/a.erofs" "$L/mnt"
	found erofs_read '^hello image$' cat "$L/mnt/etc/motd"
	umount "$L/mnt" 2>/dev/null

	# overlayfs and tmpfs
	mkdir -p "$L/lower" "$L/upper" "$L/work" "$L/merged"
	cp "$L/tree/etc/motd" "$L/lower/motd"
	run mount_overlay mount -t overlay overlay -o "lowerdir=$L/lower,upperdir=$L/upper,workdir=$L/work" "$L/merged"
	run overlay_copy_up sh -c "echo more >>$L/merged/motd && grep -q more $L/upper/motd && ! grep -q more $L/lower/motd"
	umount "$L/merged" 2>/dev/null
	run mount_tmpfs_kernel sh -c "mount -t tmpfs -o size=1m tmpfs $L/mnt && findmnt -n $L/mnt >/dev/null && umount $L/mnt"

	# partitioned disk image: this kernel has no GPT/MSDOS partition parser, so -P finds nothing; partx reads the
	# table in user space and adds the partitions through BLKPG
	truncate -s 64M "$L/disk.img"
	printf 'label: gpt\nstart=2048, size=16384, type=L\nsize=+, type=U\n' | sfdisk -q "$L/disk.img"
	pdev="$(losetup --find --show -P "$L/disk.img" 2>/dev/null)"
	sleep 0.5
	kv loop_partscan_nodes "$(ls "$pdev"p* 2>/dev/null | sed "s|^$pdev||" | tr '\n' ' ')"
	losetup -d "$pdev"
	pdev="$(losetup --find --show "$L/disk.img" 2>/dev/null)"
	run partx_add partx -a "$pdev"
	waitfor 5 test -b "${pdev}p1"
	kv loop_partitions "$(ls "$pdev"p* 2>/dev/null | sed "s|^$pdev||" | tr '\n' ' ')"
	found partx_show_device '^ *2 +18432' partx --show "$pdev"
	run mkfs_on_partition mkfs.ext4 -q -F "${pdev}p1"
	found partition_blkid 'TYPE="ext4"' blkid "${pdev}p1"
	run partition_mount mount "${pdev}p1" "$L/mnt"
	run partition_umount umount "$L/mnt"
	run partx_delete partx -d "$pdev"
	run partition_detach losetup -d "$pdev"

	# filesystems whose tools run on images but whose kernel drivers this host lacks
	truncate -s 320M "$L/xfs.img"
	mkfs.xfs -q "$L/xfs.img" 2>/dev/null
	mounterr mount_xfs -o loop "$L/xfs.img" "$L/mnt"
	truncate -s 128M "$L/btrfs.img"
	mkfs.btrfs -q -f "$L/btrfs.img" >/dev/null 2>&1
	mounterr mount_btrfs -o loop "$L/btrfs.img" "$L/mnt"
	truncate -s 16M "$L/fat.img"
	mkfs.fat -F 16 "$L/fat.img" >/dev/null 2>&1
	mounterr mount_vfat -o loop "$L/fat.img" "$L/mnt"
	found filesystems_in_kernel 'ext4' cat /proc/filesystems
	kv kernel_filesystems "$(awk '{print $NF}' /proc/filesystems | sort | tr '\n' ' ' | cut -c1-200)"
	umount "$L/mnt" 2>/dev/null
	kv loop_leftover "$(losetup -a | wc -l)"
fi

# ------------------------------------------------------------------------------------------------ FUSE
if section fuse; then
	U="$T/fuse"
	mkdir -p "$U/tree/etc" "$U/mnt"
	printf 'hello image\n' >"$U/tree/etc/motd"
	mke2fs -q -t ext4 -d "$U/tree" "$U/ext4.img" 16M
	kv priv_dev_fuse "$([ -c /dev/fuse ] && echo present || echo absent)"
	run fuse2fs_mount fuse2fs -o rw "$U/ext4.img" "$U/mnt"
	waitfor 5 findmnt -n "$U/mnt"
	found fuse2fs_findmnt 'fuse' findmnt -n -o FSTYPE "$U/mnt"
	found fuse2fs_read '^hello image$' cat "$U/mnt/etc/motd"
	run fuse2fs_write sh -c "echo fused >$U/mnt/etc/fused && sync"
	run fuse2fs_umount umount "$U/mnt"
	found fuse2fs_persisted 'fused' debugfs -R 'ls /etc' "$U/ext4.img"
	run fuse2fs_fsck e2fsck -fn "$U/ext4.img"
fi

# ------------------------------------------------------------------------------------------------ ftrace and perf tracepoints
if section trace; then
	TR=/sys/kernel/tracing
	I="$TR/instances/toolsprobe"
	kv tracefs_files "$([ -e "$TR/available_tracers" ] && echo present || echo absent)"
	found ftrace_tracers 'function' cat "$TR/available_tracers"
	# a private trace instance: own buffer, own enables, removed afterwards
	run ftrace_instance mkdir "$I"
	echo 1 >"$I/events/sched/sched_switch/enable" 2>/dev/null
	echo 1 >"$I/tracing_on" 2>/dev/null
	sleep 0.3
	echo 0 >"$I/tracing_on" 2>/dev/null
	found ftrace_events 'sched_switch:' cat "$I/trace"
	echo 0 >"$I/events/sched/sched_switch/enable" 2>/dev/null
	{ echo function >"$I/current_tracer"; } 2>/dev/null
	{ echo 'vfs_read' >"$I/set_ftrace_filter"; } 2>/dev/null
	echo 1 >"$I/tracing_on" 2>/dev/null
	cat /etc/hostname >/dev/null
	echo 0 >"$I/tracing_on" 2>/dev/null
	# the function tracer is refused by some cloud kernels (EPERM): the fact then names the tracer that stayed active
	fn="$(grep -E -m 1 'vfs_read' "$I/trace" | cut -c1-200)"
	kv ftrace_function "${fn:-tracer:$(cat "$I/current_tracer")}"
	echo nop >"$I/current_tracer" 2>/dev/null
	run ftrace_instance_remove rmdir "$I"
	found perf_tracepoint_stat 'sched:sched_switch' perf stat -e sched:sched_switch -a -- sleep 0.3
	found perf_tracepoint_record '[0-9]+ samples' perf record -e sched:sched_switch -a -o "$T/tp.data" -- sleep 0.3
	found perf_tracepoint_script 'sched_switch' perf script -i "$T/tp.data"
	run perf_trace_syscalls perf trace -o "$T/ptrace.txt" -- cat /etc/hostname
	found perf_trace_output 'openat|open' cat "$T/ptrace.txt"
	found perf_cpu_clock '[0-9]+ samples' perf record -e cpu-clock -F 999 -a -o "$T/cc.data" -- sleep 0.5
	found perf_hw_cycles_priv 'cycles' perf stat -e cycles -- true
	found perf_list_tracepoints 'sched:sched_switch' perf list tracepoint
	found strace_attach 'read\(|nanosleep|clock_nanosleep' sh -c "sleep 3 & p=\$!; sleep 0.3; strace -p \$p -o $T/attach.txt & s=\$!; sleep 0.5; kill \$p; wait \$s 2>/dev/null; cat $T/attach.txt"
fi

# ------------------------------------------------------------------------------------------------ eBPF
if section bpf; then
	P="$T/bpf"
	mkdir -p "$P"
	cat >"$P/prog.c" <<'EOF'
#include <linux/bpf.h>
#define SEC(n) __attribute__((section(n), used))
SEC("socket") int accept_all(struct __sk_buff *skb) { (void)skb; return 0; }
char _license[] SEC("license") = "GPL";
EOF
	run bpf_compile clang -O2 -target bpf -c "$P/prog.c" -o "$P/prog.o"
	found bpf_object_file 'eBPF' file "$P/prog.o"
	found bpf_feature_prog 'eBPF program_type socket_filter is available' bpftool feature probe kernel
	run bpffs_mount mount -t bpf bpf /sys/fs/bpf
	run bpf_prog_load bpftool prog load "$P/prog.o" /sys/fs/bpf/accept_all
	found bpf_prog_show 'socket_filter' bpftool prog show pinned /sys/fs/bpf/accept_all
	found bpf_prog_list 'accept_all' bpftool prog list
	run bpf_map_create bpftool map create /sys/fs/bpf/tmap type array key 4 value 4 entries 4 name toolsmap
	run bpf_map_update bpftool map update pinned /sys/fs/bpf/tmap key 0 0 0 0 value 42 0 0 0
	found bpf_map_dump '"value": *\[ *42|value: 2a' bpftool map dump pinned /sys/fs/bpf/tmap
	run bpf_cleanup rm -f /sys/fs/bpf/accept_all /sys/fs/bpf/tmap
fi

# ------------------------------------------------------------------------------------------------ network namespaces, firewalls, packet capture
if section net; then
	N="$T/net"
	mkdir -p "$N"
	run veth_create ip link add veth0 type veth peer name veth1
	run netns_create ip netns add toolsa
	run veth_move ip link set veth1 netns toolsa
	run veth_configure sh -c 'ip addr add 10.77.0.1/24 dev veth0 && ip link set veth0 up && ip netns exec toolsa sh -c "ip addr add 10.77.0.2/24 dev veth1 && ip link set veth1 up && ip link set lo up"'
	found netns_list 'toolsa' ip netns list
	found netns_exec_iface 'veth1' ip netns exec toolsa ip -o link show
	found ping_veth '1 packets received' ping -c 1 -W 2 10.77.0.2
	# traffic between the namespaces, captured on the way
	tcpdump -i veth0 -n -c 8 -w "$N/cap.pcap" >"$N/tcpdump.log" 2>&1 &
	waitfor 5 grep -q 'listening on veth0' "$N/tcpdump.log"
	ip netns exec toolsa iperf3 -s -p 5202 >"$N/iperf-server.log" 2>&1 &
	srv=$!
	waitfor 5 sh -c "ip netns exec toolsa ss -ltn | grep -q :5202"
	found netns_listener ':5202' ip netns exec toolsa ss -ltn
	found netns_lsof 'iperf3' ip netns exec toolsa lsof -nP -iTCP:5202 -sTCP:LISTEN
	found iperf3_veth '"bits_per_second":[[:space:]]*[1-9]' iperf3 -c 10.77.0.2 -p 5202 -t 2 -P 2 -J
	found iperf3_udp '"bits_per_second":[[:space:]]*[1-9]' iperf3 -c 10.77.0.2 -p 5202 -t 1 -u -b 50M -J
	wait %1 2>/dev/null
	found tcpdump_veth '10\.77\.0\.1\.[0-9]+ > 10\.77\.0\.2\.5202' tcpdump -nr "$N/cap.pcap"
	# firewall: an nftables rule inside the namespace drops the client's packets
	run nft_table ip netns exec toolsa nft add table inet toolsprobe
	run nft_chain ip netns exec toolsa nft add chain inet toolsprobe input '{ type filter hook input priority 0 ; }'
	run nft_rule ip netns exec toolsa nft add rule inet toolsprobe input tcp dport 5202 counter drop
	found nft_list 'tcp dport 5202 counter packets [0-9]+ bytes [0-9]+ drop' ip netns exec toolsa nft list ruleset
	found nft_blocks 'unable to connect|Connection timed out|connect failed|error' iperf3 -c 10.77.0.2 -p 5202 -t 1 --connect-timeout 1500
	found nft_counted 'counter packets [1-9]' ip netns exec toolsa nft list ruleset
	run nft_delete ip netns exec toolsa nft delete table inet toolsprobe
	found nft_unblocked '"bits_per_second":[[:space:]]*[1-9]' iperf3 -c 10.77.0.2 -p 5202 -t 1 -J
	kill "$srv" 2>/dev/null
	# iptables in the namespace: drop ICMP, then allow it again
	run iptables_drop ip netns exec toolsa iptables -A INPUT -p icmp -j DROP
	found iptables_list '-A INPUT -p icmp -j DROP' ip netns exec toolsa iptables -S
	found iptables_blocks '100% packet loss' ping -c 1 -W 1 10.77.0.2
	run iptables_flush ip netns exec toolsa iptables -F
	found iptables_allows '1 packets received' ping -c 1 -W 2 10.77.0.2
	found iptables_backend '\((legacy|nf_tables)\)' iptables --version
	# other link types and queueing disciplines
	run bridge_create ip link add br0 type bridge
	run veth_pair_two ip link add vb0 type veth peer name vb1
	run bridge_enslave ip link set vb0 master br0
	found bridge_slave 'master br0' ip -d link show vb0
	found bridge_tool 'vb0' bridge link show
	run tuntap_create ip tuntap add dev tun9 mode tun
	found tuntap_show 'tun9' ip -d link show tun9
	found tc_show 'qdisc' tc qdisc show dev veth0
	run tc_tbf tc qdisc add dev veth0 root tbf rate 10mbit burst 32kbit latency 400ms
	run tc_netem tc qdisc add dev veth0 root netem delay 10ms
	run dummy_link ip link add dm0 type dummy
	found ip_route 'default|10.77.0.0/24' ip route show
fi

# ------------------------------------------------------------------------------------------------ cgroup resource limits
if section cgroup; then
	CG=/sys/fs/cgroup
	CT="$T/ctr-test"
	for c in memory pids cpu; do
		kv "cgroup_$c" "$([ -d "$CG/$c" ] && echo mounted || echo missing)"
		mkdir -p "$CG/$c/toolsprobe"
	done
	# memory: a 32 MiB limit, a process that wants 200 MiB
	echo 33554432 >"$CG/memory/toolsprobe/memory.limit_in_bytes"
	[ -e "$CG/memory/toolsprobe/memory.memsw.limit_in_bytes" ] && echo 33554432 >"$CG/memory/toolsprobe/memory.memsw.limit_in_bytes"
	sh -c "echo \$\$ >$CG/memory/toolsprobe/cgroup.procs && exec $CT mem 200" >"$T/cg-mem.log" 2>&1
	kv cgroup_memory_oom_rc "$?"
	oomk() { sed -n 's/^oom_kill //p' "$CG/memory/toolsprobe/memory.oom_control"; }
	found cgroup_memory_oom_kill '^oom_kill [1-9]' cat "$CG/memory/toolsprobe/memory.oom_control"
	# pids: at most 8 tasks
	echo 8 >"$CG/pids/toolsprobe/pids.max"
	found cgroup_pids_limit 'forked=[0-9] of 30 \(refused\)' sh -c "echo \$\$ >$CG/pids/toolsprobe/cgroup.procs && exec $CT fork 30"
	# cpu: half of one CPU
	echo 100000 >"$CG/cpu/toolsprobe/cpu.cfs_period_us"
	echo 50000 >"$CG/cpu/toolsprobe/cpu.cfs_quota_us"
	found cgroup_cpu_quota 'cpu-ratio=' sh -c "echo \$\$ >$CG/cpu/toolsprobe/cgroup.procs && exec $CT cpu 3"
	# stress-ng inside the memory cgroup: its vm worker is OOM-killed again and again, stress-ng itself completes
	before="$(oomk)"
	found cgroup_stress_ng 'successful run completed' sh -c "echo \$\$ >$CG/memory/toolsprobe/cgroup.procs && exec stress-ng --vm 1 --vm-bytes 200M --vm-keep --timeout 3s"
	kv cgroup_stress_ng_oom_kills "$(($(oomk) - before))"
	for c in memory pids cpu; do rmdir "$CG/$c/toolsprobe" 2>/dev/null; done
fi

# ------------------------------------------------------------------------------------------------ libvirt (QEMU driver, TCG)
if section libvirt; then
	V="$T/virt"
	mkdir -p "$V" /run/libvirt /var/log/libvirt /var/lib/libvirt /var/lib/libvirt/images
	virtlogd --daemon >"$V/virtlogd.log" 2>&1
	virtlockd --daemon >"$V/virtlockd.log" 2>&1
	libvirtd --daemon >"$V/libvirtd.log" 2>&1
	waitfor 30 test -S /run/libvirt/libvirt-sock
	VIRSH="virsh -c qemu:///system"
	found virsh_version 'Using library: libvirt' $VIRSH version
	found virsh_hypervisor 'Running hypervisor: QEMU' $VIRSH version
	found virsh_capabilities '<arch>x86_64</arch>' $VIRSH capabilities
	found virsh_domcaps_tcg '<domainCapabilities>' $VIRSH domcapabilities --virttype qemu --arch x86_64 --machine q35
	found virsh_domcaps_kvm '<domainCapabilities>|failed to get emulator capabilities|not supported|error' $VIRSH domcapabilities --virttype kvm --arch x86_64 --machine q35
	found virt_host_validate 'QEMU: Checking for hardware virtualization' virt-host-validate qemu
	# storage: a directory pool and a qcow2 volume made through libvirt
	run pool_define $VIRSH pool-define-as toolspool dir --target /var/lib/libvirt/images
	run pool_start $VIRSH pool-start toolspool
	run vol_create $VIRSH vol-create-as toolspool toolsvol.qcow2 8M --format qcow2
	found vol_list 'toolsvol.qcow2' $VIRSH vol-list toolspool
	found vol_qcow2 '"format": "qcow2"' qemu-img info --output=json /var/lib/libvirt/images/toolsvol.qcow2
	# a VM that runs the boot sector, with the serial port in a file
	as --32 -o "$V/boot.o" vm/boot.S && ld -m elf_i386 -Ttext 0x7c00 -o "$V/boot.elf" "$V/boot.o" && objcopy -O binary -j .text "$V/boot.elf" "$V/boot.bin"
	cp "$V/boot.bin" /var/lib/libvirt/images/boot.img
	truncate -s 1M /var/lib/libvirt/images/boot.img
	: >/var/lib/libvirt/images/serial.txt
	chmod 666 /var/lib/libvirt/images/serial.txt /var/lib/libvirt/images/boot.img
	sed -e 's|@DISK@|/var/lib/libvirt/images/boot.img|' -e 's|@SERIAL@|/var/lib/libvirt/images/serial.txt|' -e 's|@IFACE@||' vm/dom.xml >"$V/dom.xml"
	run domain_define $VIRSH define "$V/dom.xml"
	found domain_list_defined 'toolsprobe' $VIRSH list --all
	run domain_start $VIRSH start toolsprobe
	waitfor 90 grep -q BOOT-OK /var/lib/libvirt/images/serial.txt
	found domain_serial '^BOOT-OK' cat /var/lib/libvirt/images/serial.txt
	found domain_state 'running' $VIRSH domstate toolsprobe
	found domain_info 'CPU\(s\): *1' $VIRSH dominfo toolsprobe
	found domain_qemu_process 'qemu-system-x86_64.*-accel tcg|qemu-system-x86_64.*accel=tcg' sh -c 'ps -eo args | grep "[q]emu-system-x86_64"'
	found domain_blk 'boot.img' $VIRSH domblklist toolsprobe
	run domain_destroy $VIRSH destroy toolsprobe
	found domain_state_after 'shut off' $VIRSH domstate toolsprobe
	run domain_undefine $VIRSH undefine toolsprobe
	# libvirt's own virtual networks: its firewall setup programs a tc "csum" action for DHCP replies, which needs a
	# kernel module; the fact records whether the network starts or why it does not
	cat >"$V/net.xml" <<'EOF'
<network>
  <name>toolsnet</name>
  <forward mode='nat'/>
  <bridge name='virbr9' stp='off' delay='0'/>
  <ip address='192.168.77.1' netmask='255.255.255.0'>
    <dhcp><range start='192.168.77.10' end='192.168.77.50'/></dhcp>
  </ip>
</network>
EOF
	run net_define $VIRSH net-define "$V/net.xml"
	found net_start 'Network toolsnet started|Failed to load TC action module' $VIRSH net-start toolsnet
	$VIRSH net-destroy toolsnet >/dev/null 2>&1
	run net_undefine $VIRSH net-undefine toolsnet
	# a VM attached to a host bridge (libvirt creates the tap device on it)
	run vbridge_create ip link add vbr0 type bridge
	run vbridge_up ip link set vbr0 up
	: >/var/lib/libvirt/images/serial.txt
	iface="<interface type='bridge'><source bridge='vbr0'/><model type='virtio'/></interface>"
	sed -e 's|@DISK@|/var/lib/libvirt/images/boot.img|' -e 's|@SERIAL@|/var/lib/libvirt/images/serial.txt|' -e "s|@IFACE@|$iface|" vm/dom.xml >"$V/dom-br.xml"
	run domain_br_define $VIRSH define "$V/dom-br.xml"
	run domain_br_start $VIRSH start toolsprobe
	waitfor 90 grep -q BOOT-OK /var/lib/libvirt/images/serial.txt
	found domain_br_serial '^BOOT-OK' cat /var/lib/libvirt/images/serial.txt
	found domain_br_iflist 'bridge +vbr0' $VIRSH domiflist toolsprobe
	found domain_br_tap 'master vbr0' sh -c 'ip -o link show | grep vnet'
	$VIRSH destroy toolsprobe >/dev/null 2>&1
	$VIRSH undefine toolsprobe >/dev/null 2>&1
	$VIRSH vol-delete toolsvol.qcow2 --pool toolspool >/dev/null 2>&1
	$VIRSH pool-destroy toolspool >/dev/null 2>&1
	$VIRSH pool-undefine toolspool >/dev/null 2>&1
fi

# ------------------------------------------------------------------------------------------------ Docker
if section docker; then
	D="$T/docker"
	export DOCKER_HOST=unix:///run/docker.sock
	mkdir -p "$D/ctx"
	cp "$T/ctr-test" "$D/ctx/ctr-test"
	cp ctr/Dockerfile ctr/compose.yaml "$D/ctx/"
	# the plain static binary also as a root filesystem for runc
	mkdir -p "$D/bundle/rootfs"
	cp "$T/ctr-test" "$D/bundle/rootfs/ctr-test"
	(cd "$D/bundle" && runc spec && sed -i -e 's/"terminal": true/"terminal": false/' -e 's|"sh"|"/ctr-test", "echo", "runc-ok"|' config.json)
	found runc_run '^runc-ok$' sh -c "cd $D/bundle && runc run toolsprobe"
	dockerd --host "$DOCKER_HOST" --pidfile /run/dockerd.pid >"$D/dockerd.log" 2>&1 &
	waitfor 60 docker info
	found docker_info_server 'Server Version: ' docker info
	found docker_storage_driver 'Storage Driver: overlay' docker info
	found docker_runtime 'Runtimes:.*runc' docker info
	found docker_cgroup 'Cgroup (Driver|Version): ' docker info
	found docker_version_server '^[0-9]+\.[0-9]+' docker version --format '{{.Server.Version}}'
	found containerd_socket 'containerd' sh -c 'ls /run/docker/containerd/ 2>/dev/null; ls /run/containerd 2>/dev/null; ps -eo args | grep "[c]ontainerd"'
	# build, load and run
	run docker_build docker build -t ctr-test:built "$D/ctx"
	found docker_images 'ctr-test:built' docker images
	found docker_run_echo '^hello container$' docker run --rm --network none ctr-test:built echo hello container
	found docker_default_cmd '^default-cmd$' docker run --rm --network none ctr-test:built
	found docker_entrypoint_override '^override-ok$' docker run --rm --network none --entrypoint /ctr-test ctr-test:built echo override-ok
	run docker_buildx docker buildx build -t ctr-test:bx "$D/ctx"
	found docker_buildx_image 'ctr-test:bx' docker images
	run docker_save docker save -o "$D/ctr.tar" ctr-test:bx
	run docker_rmi docker rmi ctr-test:bx
	found docker_load 'Loaded image: ctr-test:bx' docker load -i "$D/ctr.tar"
	mkdir -p "$D/imp"
	cp "$T/ctr-test" "$D/imp/ctr-test"
	run docker_import sh -c "tar -C $D/imp -c . | docker import - ctr-test:imported"
	found docker_import_run '^imported-ok$' docker run --rm --network none --entrypoint /ctr-test ctr-test:imported echo imported-ok
	# isolation and limits
	docker run --rm --network none --memory 32m --memory-swap 32m ctr-test:built mem 200 >"$D/oom.log" 2>&1
	kv docker_memory_limit_rc "$?"
	found docker_pids_limit 'forked=[0-9] of 30 \(refused\)' docker run --rm --network none --pids-limit 8 ctr-test:built fork 30
	found docker_cpu_limit 'cpu-ratio=' docker run --rm --network none --cpus 0.5 ctr-test:built cpu 3
	found docker_read_only 'Read-only file system' docker run --rm --network none --read-only ctr-test:built write /probe
	found docker_cap_drop 'CapEff:[[:space:]]*0000000000000000' docker run --rm --network none --cap-drop ALL ctr-test:built caps
	found docker_cap_default 'CapEff:[[:space:]]*00000000a80425fb' docker run --rm --network none ctr-test:built caps
	found docker_network_none '^lo $' docker run --rm --network none ctr-test:built ifaces
	found docker_user 'CapEff:[[:space:]]*0000000000000000' docker run --rm --network none --user 65534:65534 ctr-test:built caps
	found docker_no_new_privileges 'NoNewPrivs:[[:space:]]*1' docker run --rm --network none --security-opt no-new-privileges ctr-test:built caps
	# networking between containers: a user-defined network, a server, exec, logs and a client
	run docker_network_create docker network create probenet
	run docker_server_start docker run -d --name srv --network probenet ctr-test:built listen 8080
	waitfor 15 sh -c 'docker logs srv 2>&1 | grep -q listening'
	found docker_logs '^listening$' docker logs srv
	found docker_exec 'eth0' docker exec srv /ctr-test ifaces
	found docker_inspect_ip '"IPAddress": *"[0-9.]+"' docker inspect srv
	found docker_ps 'srv' docker ps
	found docker_client_dial '^dial got pong$' docker run --rm --network probenet ctr-test:built dial srv 8080
	docker rm -f srv >/dev/null 2>&1
	# compose: two services on one network resolving each other by name
	found compose_up 'dial got pong' docker compose -f "$D/ctx/compose.yaml" -p probe up --abort-on-container-exit --exit-code-from client --no-color
	run compose_down docker compose -f "$D/ctx/compose.yaml" -p probe down
	docker network rm probenet >/dev/null 2>&1
	found docker_prune 'Total reclaimed space' docker system prune -f
	kill "$(cat /run/dockerd.pid)" 2>/dev/null
	waitfor 20 sh -c '! kill -0 $(cat /run/dockerd.pid) 2>/dev/null'
fi

# ------------------------------------------------------------------------------------------------ what this host lacks
if section limits; then
	kv priv_dev_kvm "$([ -e /dev/kvm ] && echo present || echo absent)"
	kv priv_dev_vhost_net "$([ -e /dev/vhost-net ] && echo present || echo absent)"
	kv priv_dev_net_tun "$([ -c /dev/net/tun ] && echo present || echo absent)"
	kv priv_lib_modules "$([ -d /lib/modules ] && ls /lib/modules | head -n 1 | grep . || echo absent)"
	# With KVM the stopped machine just sits there until the timeout kills it; without, QEMU exits at once.
	kvm_out="$(timeout 3 qemu-system-x86_64 -accel kvm -display none -S -monitor none -machine q35 2>&1)" && kvm_rc=0 || kvm_rc=$?
	case "$kvm_rc" in 124 | 143) kv priv_qemu_kvm ok ;; *) kv priv_qemu_kvm "$(echo "$kvm_out" | tail -n 1 | cut -c1-200)" ;; esac
	kv priv_fd_hard_raise "$(sh -c 'ulimit -Hn 4194304' 2>&1 | head -n 1 | grep . || echo ok)"
	kv priv_hard_nofile "$(ulimit -Hn)"
	kv priv_nr_open "$(cat /proc/sys/fs/nr_open)"
	kv priv_schedstat "$([ -e /proc/schedstat ] && echo present || echo absent)"
	kv priv_perf_event_paranoid "$(cat /proc/sys/kernel/perf_event_paranoid)"
fi
