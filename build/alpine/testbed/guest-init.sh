#!/bin/sh
# Stage 2 of the cgroup v2 test bed: runs as PID 1 of the guest, from the tools image over the read-only 9p root
# (stage 1 is testbed/init). Brings the guest to the state the guest tests expect, runs them, powers off.
#   kernel command line: ai.run=<test script inside the image>
# What the guest gives the tests that the cloud host cannot: a unified cgroup hierarchy (the kernel is booted with
# cgroup_no_v1=all, so /sys/fs/cgroup is cgroup2 and nothing else), root privileges over all of it, and a kernel
# that was built with the controllers, PSI, schedstat and function tracing that the host kernel lacks.
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin HOME=/root LANG=C.UTF-8 TZ=UTC TERM=dumb
umask 022
run=""
for a in $(cat /proc/cmdline); do
	case "$a" in ai.run=*) run="${a#ai.run=}" ;; esac
done
finish() {
	sync
	poweroff -f
}
say() { echo "guest-init: $*" >/dev/console; }
mount -t tmpfs -o mode=1777 tmpfs /tmp || say "no /tmp"
mount -t tmpfs -o mode=0755 tmpfs /run || say "no /run"
mount -t 9p -o trans=virtio,version=9p2000.L,msize=262144,cache=none out /mnt || { say "cannot mount the output share"; finish; }
mount -t cgroup2 -o nsdelegate cgroup2 /sys/fs/cgroup || mount -t cgroup2 cgroup2 /sys/fs/cgroup || say "cannot mount cgroup2"
# Hand every controller that the kernel has to the first level of the hierarchy.
ctl=""
for c in $(cat /sys/fs/cgroup/cgroup.controllers); do ctl="$ctl +$c"; done
[ -z "$ctl" ] || echo "$ctl" >/sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || say "cannot enable controllers:$ctl"
mount -t tracefs tracefs /sys/kernel/tracing 2>/dev/null
mount -t debugfs debugfs /sys/kernel/debug 2>/dev/null
mount -t bpf bpf /sys/fs/bpf 2>/dev/null
hostname alpine-guest
ip link set lo up 2>/dev/null
[ -n "$run" ] && [ -f "$run" ] || { say "no test script ($run)"; echo "g_boot=fail:no test script" >/mnt/guest.facts; finish; }
cd "$(dirname "$run")" || finish
sh "$run" >/mnt/guest.facts 2>/mnt/guest.err </dev/null
echo "$?" >/mnt/guest.rc
finish
