#!/bin/sh
# Print the toolchain lock: exact name=version of every installed package.
echo "# Alpine toolchain lock: exact package versions of the pinned build rootfs."
echo "# Regenerate with: build/alpine/bootstrap.sh --force --refresh-lock"
apk list -I 2>/dev/null | awk '{print $1}' | sed -E 's/^(.*)-([0-9][^-]*-r[0-9]+)$/\1=\2/' | sort
