#!/usr/bin/env bash
# Validate the tools image produced by make-tools.sh (development image + debugging, profiling, pressure,
# filesystem and disk-image, QEMU, libvirt and Docker tools). Exit status 0 only if every check passes.
# Rebuilds the image from a clean state once to prove it is reproduced byte for byte, rebuilds the cgroup v2 test
# bed (make-guest.sh) from its pins, then runs the functional tests of tools-test.sh (three sessions, judged by
# tools-judge.py): PASS, DENIED (a privileged-only capability refused to an unprivileged workload), LIMIT (a
# capability the host lacks, with a measured cause), UPSTREAM (a known defect of a packaged tool) or FAIL.
#   check-tools.sh
exec "$(dirname "${BASH_SOURCE[0]}")/check-layer.sh" tools "$@"
