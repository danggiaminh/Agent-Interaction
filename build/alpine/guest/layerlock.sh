#!/bin/sh
# Runs inside the sandbox as root WITH network: resolve the top-level packages of a layer (PKGS_FILE, e.g.
# guest/dev.pkgs) against the image it extends and the live Alpine repositories, and print
# "name version repo origin" for every package that has to be added, plus the aport it is built from.
# Packages the parent image already provides are not listed.
# A package named in LOCAL_FILE (guest/tools.local) is not taken from the mirror: it is reported as repo "local",
# with the version of the locally built package in /build/packages (the dependency closure is still the live one).
#   env: ALPINE_ARCH ALPINE_BRANCH ALPINE_MIRROR PARENT_IMAGE_NAME PKGS_FILE [LOCAL_FILE]
set -eu
: "${ALPINE_ARCH:?}" "${ALPINE_BRANCH:?}" "${ALPINE_MIRROR:?}" "${PARENT_IMAGE_NAME:?}" "${PKGS_FILE:?}"
localnames=""
[ -z "${LOCAL_FILE:-}" ] || localnames="$(grep -v '^#' "$LOCAL_FILE" | awk 'NF{print $1}')"
stage=/tmp/layerlock
rm -rf "$stage" && mkdir -p "$stage/root"
tar -xzf "/build/images/$PARENT_IMAGE_NAME.rootfs.tar.gz" -C "$stage/root" --numeric-owner
repos="main community"
args=""
for r in $repos; do args="$args --repository $ALPINE_MIRROR/$ALPINE_BRANCH/$r"; done
# shellcheck disable=SC2046,SC2086
apk --root "$stage/root" --keys-dir "$stage/root/etc/apk/keys" --repositories-file /dev/null $args \
	--no-cache --simulate add $(grep -v '^#' "$PKGS_FILE") >"$stage/simulate.out" 2>&1 ||
	{ cat "$stage/simulate.out" >&2; echo "layerlock: apk cannot resolve $PKGS_FILE against the live repositories" >&2; exit 1; }
sed -n 's/^([ 0-9]*\/[0-9]*) Installing \(.*\) (\(.*\))$/\1 \2/p' "$stage/simulate.out" |
	while read -r name ver; do
		found="" origin=""
		if printf '%s\n' "$localnames" | grep -qx "$name"; then
			set -- /build/packages/main/"$ALPINE_ARCH/$name"-[0-9]*.apk
			[ $# -eq 1 ] && [ -f "$1" ] || { echo "layerlock: $name is listed in $LOCAL_FILE but the local repository has no single $name package" >&2; exit 1; }
			lver="${1##*/}"
			lver="${lver#"$name"-}"
			lver="${lver%.apk}"
			lorigin="$(tar -xzOf "$1" .PKGINFO | sed -n 's/^origin = //p')"
			echo "$name $lver local $lorigin"
			continue
		fi
		for r in $repos; do
			line="$(apk --repositories-file /dev/null --repository "$ALPINE_MIRROR/$ALPINE_BRANCH/$r" --no-cache list -a "$name" 2>/dev/null |
				grep "^$name-$ver " || true)"
			if [ -n "$line" ]; then
				found="$found $r"
				origin="$(printf '%s\n' "$line" | sed -n 's/^[^ ]* [^ ]* {\(.*\)} .*$/\1/p')"
			fi
		done
		set -- $found
		[ $# -eq 1 ] && [ -n "$origin" ] || { echo "layerlock: $name-$ver is in ${found:-no} repositories (origin '$origin')" >&2; exit 1; }
		echo "$name $ver $1 $origin"
	done
