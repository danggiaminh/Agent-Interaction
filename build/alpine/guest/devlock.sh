#!/bin/sh
# Runs inside the sandbox as root WITH network: resolve the development packages (guest/dev.pkgs)
# against the base image and the live Alpine repositories, and print "name version repo origin" for every
# package that has to be added, plus the aport it is built from. Packages the base image already
# provides are not listed.
#   env: ALPINE_ARCH ALPINE_BRANCH ALPINE_MIRROR BASE_IMAGE_NAME
set -eu
: "${ALPINE_ARCH:?}" "${ALPINE_BRANCH:?}" "${ALPINE_MIRROR:?}" "${BASE_IMAGE_NAME:?}"
stage=/tmp/devlock
rm -rf "$stage" && mkdir -p "$stage/root"
tar -xzf "/build/images/$BASE_IMAGE_NAME.rootfs.tar.gz" -C "$stage/root" --numeric-owner
repos="main community"
args=""
for r in $repos; do args="$args --repository $ALPINE_MIRROR/$ALPINE_BRANCH/$r"; done
# shellcheck disable=SC2046,SC2086
apk --root "$stage/root" --keys-dir "$stage/root/etc/apk/keys" --repositories-file /dev/null $args \
	--no-cache --simulate add $(grep -v '^#' /guest/dev.pkgs) 2>&1 | sed -n 's/^([ 0-9]*\/[0-9]*) Installing \(.*\) (\(.*\))$/\1 \2/p' |
	while read -r name ver; do
		found="" origin=""
		for r in $repos; do
			line="$(apk --repositories-file /dev/null --repository "$ALPINE_MIRROR/$ALPINE_BRANCH/$r" --no-cache list -a "$name" 2>/dev/null |
				grep "^$name-$ver " || true)"
			if [ -n "$line" ]; then
				found="$found $r"
				origin="$(printf '%s\n' "$line" | sed -n 's/^[^ ]* [^ ]* {\(.*\)} .*$/\1/p')"
			fi
		done
		set -- $found
		[ $# -eq 1 ] && [ -n "$origin" ] || { echo "devlock: $name-$ver is in ${found:-no} repositories (origin '$origin')" >&2; exit 1; }
		echo "$name $ver $1 $origin"
	done
