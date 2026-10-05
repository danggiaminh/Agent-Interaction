#!/usr/bin/env bash
# Clean-environment check for the pinned Alpine build environment. Exit status 0 only if every check passes.
#   check-env.sh
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root
fail=0
ok() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }

echo "== pinned inputs (host)"
if [ -f "$CACHE_DIR/$ROOTFS_FILE" ] && echo "$ROOTFS_SHA256  $CACHE_DIR/$ROOTFS_FILE" | sha256sum -c - >/dev/null 2>&1; then
	ok "cached minirootfs matches pinned sha256"
else bad "cached minirootfs missing or sha256 mismatch"; fi
if grep -q "alpine=$ALPINE_VERSION rootfs_sha256=$ROOTFS_SHA256 aports_tree=$APORTS_TREE" "$ROOTFS_DIR/.bootstrap-complete" 2>/dev/null; then
	ok "rootfs bootstrapped from the pinned inputs"
else bad "rootfs not bootstrapped from the pinned inputs (run bootstrap.sh)"; fi
if [ -s "$ROOTFS_DIGEST_FILE" ] && [ "$(rootfs_digest)" = "$(cat "$ROOTFS_DIGEST_FILE")" ]; then
	ok "toolchain rootfs is byte-identical to its state after bootstrap (no drift)"
else bad "toolchain rootfs differs from its bootstrap state (drift): re-run bootstrap.sh --force"; fi
if msg="$(vendor_pristine)"; then ok "vendored aports is the pinned tree and pristine"; else bad "vendored aports not pristine: $msg"; fi

echo "== sandbox as builder, clean environment"
rc=0
out="$(AGENT_ENV_CANARY=leak "$ENTER" --user builder -- /bin/sh /guest/check.sh 2>&1)" || rc=$?
printf '%s\n' "$out"
if [ "$rc" != 0 ] || grep -q '^FAIL' <<<"$out"; then fail=1; fi

echo "== offline mode"
off="$("$ENTER" --offline --user builder -- /bin/sh -c 'echo "links=$(ip -o link | wc -l)"; wget -q -T 3 -O /dev/null "$ALPINE_MIRROR/" 2>/dev/null && echo REACHED' 2>&1)" || true
if grep -qx 'links=1' <<<"$off" && ! grep -q REACHED <<<"$off"; then ok "--offline: loopback only, network unreachable"; else bad "--offline not isolated: $off"; fi

echo "== host state"
if [ "$(grep -c "$ROOTFS_DIR" /proc/mounts)" = 0 ]; then ok "no sandbox mounts leaked into the host"; else bad "sandbox mounts leaked into the host"; fi
if msg="$(vendor_pristine)"; then ok "vendored aports still pristine after the checks"; else bad "vendored aports modified: $msg"; fi

if [ "$fail" = 0 ]; then echo "RESULT: environment OK"; else echo "RESULT: environment check FAILED"; fi
exit "$fail"
