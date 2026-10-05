#!/usr/bin/env bash
# Validate the development image produced by make-dev.sh (base system + Rust and C toolchain). Exit
# status 0 only if every check passes. Rebuilds the image from a clean state once to prove it is
# reproduced byte for byte, then compiles and runs the C, Rust and C-from-Rust test projects in it.
#   check-dev.sh
exec "$(dirname "${BASH_SOURCE[0]}")/check-layer.sh" dev "$@"
