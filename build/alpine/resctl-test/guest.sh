#!/bin/sh
# Runs as the last step of the cgroup v2 test bed (testbed/launch.sh -> QEMU -> testbed/guest-init.sh): real root of a
# kernel that was booted with cgroup_no_v1=all, so the whole of cgroup v2 is available (every controller, nsdelegate)
# and nothing else is. Runs the resource-control suite (suite.sh) there: the layout the host cannot offer, where
# resctl serves every feature from one hierarchy. Facts: g_*. With software emulation (no /dev/kvm) the checks are
# functional, not timing-accurate; the tolerances of the suite are wide enough for that.
cd /work || exit 1
. /work/common.sh
. /work/cgroup.sh
. /work/suite.sh

RC_DIR=/work
CT=/work/ctr/ctr-test
CG_BLKDEV=/dev/vda
# software emulation: the timing rows of the stress section are not measured here
RC_TIMING=no
# a guest of its own: the only place where the whole machine may run out of memory
RC_GLOBAL_OOM=yes

# the sections: the arguments, else those of /work/areas (check-resctl.sh --areas), else all of them
if [ $# -eq 0 ] && [ -s /work/areas ]; then set -- $(cat /work/areas); fi
rc_suite g_ auto $*
