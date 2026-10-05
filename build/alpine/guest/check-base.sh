#!/bin/sh
# In-sandbox checks of the base image, run as root. Prints PASS/FAIL lines, exits 1 on any FAIL.
fail=0
ok() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }
R=/build/images/rootfs
builder="${BUILDER_USER:-builder}"
rm -rf /tmp/keys && mkdir -p /tmp/keys && cp "/home/$builder"/.abuild/*.rsa.pub /tmp/keys/

# Every package in the local repository is signed by the environment's key and intact.
n=0
for f in /build/packages/main/"$ALPINE_ARCH"/*.apk; do
	n=$((n + 1))
	if ! out="$(apk --keys-dir /tmp/keys verify "$f" 2>&1)"; then bad "apk verify $(basename "$f"): $out"; fi
done
[ "$fail" = 0 ] && ok "apk verify: all $n local packages are intact and signed by the local key"

# The signed repository index resolves the whole base system from the local repository alone.
if apk --root /tmp/resolve --initdb --keys-dir /tmp/keys --repositories-file /dev/null \
	--repository /build/packages/main --no-network --no-cache --no-scripts add alpine-base >/tmp/resolve.log 2>&1; then
	ok "alpine-base resolves from the local repository alone (no network, no upstream repository)"
else
	bad "alpine-base does not resolve locally: $(tail -n 5 /tmp/resolve.log)"
fi

# The staged image still matches the apk database: nothing owned by a package was altered or removed.
audit="$(apk --root "$R" audit --full 2>&1)" || true
changed="$(printf '%s\n' "$audit" | grep -E '^[^A ] ' | grep -vE '^d ' || true)"
if [ -z "$changed" ]; then
	ok "apk audit: no package-owned file modified or missing ($(printf '%s\n' "$audit" | grep -c '^A ') unowned entries are checked separately)"
else
	bad "apk audit reports changes: $(printf '%s\n' "$changed" | head -n 5)"
fi
[ "$fail" = 0 ]
