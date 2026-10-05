#!/usr/bin/env bash
# Validate the tools image produced by make-tools.sh (development image + debugging, profiling, pressure,
# filesystem and disk-image, QEMU, libvirt and Docker tools). Exit status 0 only if every check passes.
# Rebuilds the image from a clean state once to prove it is reproduced byte for byte, then runs the
# functional tests of tools-test.sh, which report PASS, FAIL or LIMIT (a capability the cloud host lacks).
#   check-tools.sh
exec "$(dirname "${BASH_SOURCE[0]}")/check-layer.sh" tools "$@"
