#!/usr/bin/env bash
# Extend the development image with the tools layer (debugging, tracing, profiling, benchmarking, pressure,
# filesystem and disk-image, QEMU and libvirt, Docker): official, checksum-pinned Alpine v3.24 packages
# listed in guest/tools.pkgs and locked in guest/tools.lock. See make-layer.sh.
#   make-tools.sh                 assemble the tools image from the development image and the lock
#   make-tools.sh --clean         remove the tools image outputs (the .build/upstream cache is kept)
#   make-tools.sh --refresh-lock  re-resolve tools.pkgs against the live v3.24 repositories, rewrite tools.lock
#                                 and fill the cache; review the diff, then rebuild
# Output under .build/images/: tools-rootfs/, alpine-tools-<version>-<arch>.rootfs.tar.gz (+ .sha256) and .manifest.
exec "$(dirname "${BASH_SOURCE[0]}")/make-layer.sh" tools "$@"
