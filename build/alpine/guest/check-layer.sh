#!/bin/sh
# In-sandbox checks of a layered image (development or tools), run as root, offline. Prints PASS/FAIL lines, exits 1 on any FAIL.
#   env: ALPINE_ARCH PARENT_IMAGE_NAME LAYER_ROOT LAYER_LOCK
: "${ALPINE_ARCH:?}" "${PARENT_IMAGE_NAME:?}" "${LAYER_ROOT:?}" "${LAYER_LOCK:?}"
lockname="$(basename "$LAYER_LOCK")"
fail=0
ok() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }
R="/build/images/$LAYER_ROOT"
keys="$R/etc/apk/keys"

# Every locked package file is the pinned one and carries a valid signature of the Alpine release keys that
# the image itself ships (the same check apk made when it installed the file). A package of repo "local" is
# pinned by the datahash of its payload and signed with this environment's package key, trusted here only.
n=0 nlocal=0 files="" vkeys="$keys"
while read -r name ver repo origin sha; do
	case "$name" in '#'* | '') continue ;; esac
	n=$((n + 1))
	if [ "$repo" = local ]; then
		f="/build/packages/main/$ALPINE_ARCH/$name-$ver.apk"
		nlocal=$((nlocal + 1))
		if [ ! -f "$f" ] || [ "$(tar -xzOf "$f" .PKGINFO | sed -n 's/^datahash = //p')" != "$sha" ]; then
			bad "$name-$ver.apk (local) does not match its datahash in $lockname"
		fi
		if [ "$vkeys" = "$keys" ]; then
			vkeys=/tmp/verify-keys
			rm -rf "$vkeys" && mkdir "$vkeys"
			cp "$keys"/* "$vkeys"/
			cp "/home/${BUILDER_USER:-builder}"/.abuild/*.rsa.pub "$vkeys"/
		fi
	else
		f="/build/upstream/$repo/$ALPINE_ARCH/$name-$ver.apk"
		if [ "$(sha256sum "$f" | cut -d' ' -f1)" != "$sha" ]; then bad "$name-$ver.apk does not match its sha256 in $lockname"; fi
	fi
	files="$files $f"
done <"$LAYER_LOCK"
# shellcheck disable=SC2086
if out="$(apk --keys-dir "$vkeys" verify $files 2>&1)"; then
	ok "apk verify: all $n locked packages match $lockname and carry a valid signature ($nlocal of the local repository, pinned by datahash)"
else
	bad "apk verify failed: $(printf '%s\n' "$out" | head -n 5)"
fi

# The installed files are what the packages shipped: nothing owned by a package is modified or missing.
audit="$(apk --root "$R" audit --full 2>&1)" || true
changed="$(printf '%s\n' "$audit" | grep -E '^[^A ] ' | grep -vE '^d ' || true)"
if [ -z "$changed" ]; then
	ok "apk audit: no package-owned file modified or missing ($(printf '%s\n' "$audit" | grep -c '^A ') unowned entries are checked separately)"
else
	bad "apk audit reports changes: $(printf '%s\n' "$changed" | head -n 5)"
fi

# The package manager considers the installed set consistent, without any repository.
if out="$(apk --root "$R" --repositories-file /dev/null --no-network --no-cache fix --simulate 2>&1)"; then
	ok "apk fix --simulate: installed set is consistent ($(printf '%s\n' "$out" | tail -n 1))"
else
	bad "apk fix --simulate: $(printf '%s\n' "$out" | tail -n 3)"
fi

# Package sources: still only the pinned v3.24 repositories, exactly as the parent image configured them.
parent="/build/images/$PARENT_IMAGE_NAME.rootfs.tar.gz"
if [ "$(tar -xzOf "$parent" ./etc/apk/repositories | sha256sum)" = "$(sha256sum <"$R/etc/apk/repositories")" ]; then
	ok "/etc/apk/repositories is unchanged from the parent image"
else
	bad "/etc/apk/repositories differs from the parent image"
fi
[ "$fail" = 0 ]
