#!/usr/bin/env bash
# Extend the base image with the development layer (Rust and C toolchain): official, checksum-pinned
# Alpine v3.24 packages listed in guest/dev.pkgs and locked in guest/dev.lock. See make-layer.sh.
#   make-dev.sh                 assemble the development image from the base image and the lock
#   make-dev.sh --clean         remove the development image outputs (the .build/upstream cache is kept)
#   make-dev.sh --refresh-lock  re-resolve dev.pkgs against the live v3.24 repositories, rewrite dev.lock
#                               and fill the cache; review the diff, then rebuild
# Output under .build/images/: dev-rootfs/, alpine-dev-<version>-<arch>.rootfs.tar.gz (+ .sha256) and .manifest.
exec "$(dirname "${BASH_SOURCE[0]}")/make-layer.sh" dev "$@"
