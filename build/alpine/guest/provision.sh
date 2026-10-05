#!/bin/sh
# Runs inside the sandbox as root during bootstrap: install the toolchain, create the unprivileged
# builder user and its package-signing key.
set -eu
: "${BUILDER_USER:?}" "${BUILDER_UID:?}" "${BUILDER_GID:?}" "${TOOLCHAIN_PKGS:?}" "${JOBS:?}" "${PACKAGER:?}"

apk update
if [ -s /guest/toolchain.lock ] && [ "${REFRESH_LOCK:-0}" != 1 ]; then
	echo "installing pinned toolchain from toolchain.lock"
	# shellcheck disable=SC2046
	apk add $(grep -v '^#' /guest/toolchain.lock)
else
	echo "installing toolchain (no lock in use): $TOOLCHAIN_PKGS"
	# shellcheck disable=SC2086
	apk add $TOOLCHAIN_PKGS
fi

addgroup -g "$BUILDER_GID" "$BUILDER_USER"
adduser -D -u "$BUILDER_UID" -G "$BUILDER_USER" -s /bin/sh "$BUILDER_USER"
addgroup "$BUILDER_USER" abuild

# Per-environment signing key (never committed; it lives under .build/rootfs).
su-exec "$BUILDER_USER" env HOME="/home/$BUILDER_USER" abuild-keygen -a -n
cp "/home/$BUILDER_USER"/.abuild/*.rsa.pub /etc/apk/keys/
printf 'export JOBS=%s\nexport MAKEFLAGS=-j%s\n' "$JOBS" "$JOBS" >>"/home/$BUILDER_USER/.abuild/abuild.conf"
