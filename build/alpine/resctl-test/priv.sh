#!/bin/sh
# Runs inside the tools image as the real root of the host in private mount, PID, IPC, UTS, network and cgroup
# namespaces (check-resctl.sh -> tools-run.sh --privileged), offline. Runs the resource-control suite (suite.sh) on the
# real kernel of the machine: twice, once with the lifecycle that resctl picks itself (limits through the hierarchy
# that has the controller, freeze/kill/pressure through cgroup v2 where it exists) and once with the cgroup v1 freezer
# forced, so that both lifecycle paths are exercised on a hybrid host. Facts: h_* (auto) and v_* (forced v1).
#   priv.sh [section...]     default: every section (useful for iterating: priv.sh isolation terminate)
cd /work || exit 1
. /work/common.sh
. /work/cgroup.sh
. /work/suite.sh

RC_DIR=/work

# The static test program of the workloads, and a whole block device for the I/O limits.
if gcc -static -Os -o "$T/ctr-test" ctr/ctr-test.c 2>"$T/ctr-test-cc.log"; then
	CT="$T/ctr-test"
else
	kv h_fixture "fail:ctr-test: $(tail -n 1 "$T/ctr-test-cc.log" | cut -c1-200)"
	exit 0
fi
truncate -s 16M "$T/rcdisk.img"
CG_BLKDEV="$(losetup --find --show "$T/rcdisk.img" 2>/dev/null)"
if [ -z "$CG_BLKDEV" ]; then
	kv h_fixture "fail:no loop device for the I/O limits"
	exit 0
fi

# every section on the lifecycle resctl chooses; the forced v1 lifecycle only where it changes the outcome
WANT="$*"
rc_suite h_ auto $WANT
vsec=
for s in layout normal isolation terminate recover lifecycle; do
	case " $WANT " in *" $s "* | "  ") vsec="$vsec $s" ;; esac
done
if [ -z "$WANT" ] || [ -n "$vsec" ]; then rc_suite v_ v1 $vsec; fi

losetup -d "$CG_BLKDEV"
