#!/bin/sh
# Boot the cgroup v2 test bed under QEMU and print the facts the guest produced. Runs inside the tools image,
# unprivileged (tools-test.sh -> tools-run.sh), offline; expects /work (tools-test), /testbed (this directory)
# and /bed (make-guest.sh: vmlinuz, initramfs.cpio.gz).
#   launch.sh [GUEST-SCRIPT] [TIMEOUT-SECONDS]    default: /work/guest.sh, 900 s
# Facts on stdout (KEY=VALUE), the guest console on stderr. The accelerator is KVM only if /dev/kvm can be opened
# read-write in this sandbox; otherwise it is TCG (software emulation: functional, not timing-accurate). Which one
# ran is taken from QEMU's own answer (qmp.py), never assumed.
script="${1:-/work/guest.sh}"
limit="${2:-900}"
S=/tmp/share
kv() { printf '%s=%s\n' "$1" "$2"; }
for f in /bed/vmlinuz /bed/initramfs.cpio.gz /testbed/guest-init.sh "$script"; do
	[ -f "$f" ] || { kv guest_boot "fail:missing $f"; exit 0; }
done
rm -rf "$S" && mkdir -p "$S"
# the static helper of the cgroup tests is built here, with the image's compiler; the guest only runs it
gcc -static -Os -o /work/ctr/ctr-test /work/ctr/ctr-test.c 2>"$S/cc.log" || { kv guest_boot "fail:ctr-test: $(tail -n 1 "$S/cc.log")"; exit 0; }
clang -O2 -target bpf -c /work/bpf/guest.c -o /work/bpf/guest.o 2>"$S/bpf.log" || { kv guest_boot "fail:bpf program: $(tail -n 1 "$S/bpf.log")"; exit 0; }
truncate -s 64M "$S/disk.img"
# -c first: opening a missing path with <> would create a regular file there, and QEMU would try it as KVM
if [ -c /dev/kvm ] && { : <>/dev/kvm; } 2>/dev/null; then
	want=kvm cpu=host
else
	want=tcg cpu=max
fi
append="console=ttyS0 loglevel=4 panic=-1 cgroup_no_v1=all psi=1 ai.init=/testbed/guest-init.sh ai.run=$script"
timeout -s KILL "$limit" qemu-system-x86_64 -machine "q35,accel=$want" -cpu "$cpu" -smp 2 -m 1024 \
	-nodefaults -no-user-config -display none -no-reboot -monitor none \
	-serial file:/tmp/console.log -qmp unix:/tmp/qmp.sock,server=on,wait=off \
	-kernel /bed/vmlinuz -initrd /bed/initramfs.cpio.gz -append "$append" \
	-fsdev local,id=root,path=/,security_model=none,readonly=on,multidevs=remap -device virtio-9p-pci,fsdev=root,mount_tag=root \
	-fsdev local,id=out,path="$S",security_model=none -device virtio-9p-pci,fsdev=out,mount_tag=out \
	-drive file="$S/disk.img",format=raw,if=none,id=d0 -device virtio-blk-pci,drive=d0,serial=aidisk0 \
	>/tmp/qemu.log 2>&1 &
qpid=$!
python3 /testbed/qmp.py /tmp/qmp.sock >"$S/qmp.facts" 2>"$S/qmp.err"
wait "$qpid"
rc=$?
kv guest_qemu_rc "$rc"
cat "$S/qmp.facts"
accel=tcg
grep -qx 'qmp_kvm_enabled=yes' "$S/qmp.facts" && accel=kvm
if ! grep -qx 'qmp=ok' "$S/qmp.facts"; then
	kv guest_accel "unknown:no answer from QEMU"
elif [ "$accel" != "$want" ]; then
	kv guest_accel "mismatch:asked $want, QEMU runs $accel"
else
	kv guest_accel "$accel"
fi
if [ -f "$S/guest.rc" ]; then kv guest_boot ok; else kv guest_boot "fail:the guest did not finish ($(tail -n 1 /tmp/console.log | cut -c1-150))"; fi
[ ! -f "$S/guest.rc" ] || kv guest_script_rc "$(cat "$S/guest.rc")"
[ ! -s /tmp/qemu.log ] || kv guest_qemu_stderr "$(head -n 1 /tmp/qemu.log | cut -c1-200)"
cat "$S/guest.facts" 2>/dev/null
{
	echo "--- console"
	cat /tmp/console.log
	echo "--- guest stderr"
	cat "$S/guest.err" 2>/dev/null
} >&2
exit 0
