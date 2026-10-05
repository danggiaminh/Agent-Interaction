#!/usr/bin/env bash
# Validate the base system image produced by build-base.sh + make-base.sh. Exit status 0 only if every
# check passes. Re-runs make-base.sh once to prove the image is rebuilt byte for byte.
#   check-base.sh
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root
fail=0
ok() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }

R="$IMAGES_DIR/rootfs"
tarball="$IMAGES_DIR/$BASE_IMAGE_NAME.rootfs.tar.gz"
manifest="$IMAGES_DIR/$BASE_IMAGE_NAME.manifest"
scratch="$IMAGES_DIR/check"
[ -f "$tarball" ] || die "no image yet; run build-base.sh and make-base.sh first"

echo "== pinned source"
if msg="$(vendor_pristine)"; then ok "vendored aports is the pinned tree ($APORTS_TREE) and pristine"; else bad "vendored aports not pristine: $msg"; fi
if [ "$(sed -n 's/^aports_commit=//p' "$manifest")" = "$APORTS_COMMIT" ] && [ "$(sed -n 's/^alpine_version=//p' "$manifest")" = "$ALPINE_VERSION" ]; then
	ok "manifest records aports commit $APORTS_COMMIT and Alpine $ALPINE_VERSION"
else bad "manifest does not record the pinned source"; fi

echo "== local repository and staged root (in the sandbox, as root)"
rc=0
out="$("$ENTER" --ephemeral --user root --env "BUILDER_USER=$BUILDER_USER" -- /bin/sh /guest/check-base.sh 2>&1)" || rc=$?
printf '%s\n' "$out"
if [ "$rc" != 0 ] || grep -q '^FAIL' <<<"$out"; then fail=1; fi

echo "== image contents"
rm -rf "${scratch:?}"
mkdir -p "$scratch"
needles="$scratch/needles"
# Strings that would betray host state: hostname, host paths, proxy and secret-looking env values.
# Written to a private file outside the repo's tracked tree and never printed.
python3 - "$needles" "$REPO_ROOT" "$SOURCE_DATE_EPOCH" <<'EOF'
import os, re, socket, sys, time
out, repo, sde = sys.argv[1], sys.argv[2], int(sys.argv[3])
n = {("host-path", repo), ("host-path", "/root/.ccr"), ("host-path", "/home/user")}
host = socket.gethostname()
if len(host) >= 6 and host != "localhost":
    n.add(("hostname", host))
for k, v in os.environ.items():
    if re.search(r"TOKEN|SECRET|KEY|PASS|CRED|AUTH|PROXY|SESSION", k, re.I) and len(v) >= 8:
        n.add((f"env:{k}", v))
        m = re.match(r"^[a-z]+://(?:[^@/]*@)?([^/:]+)", v)
        if m and "." in m.group(1) and len(m.group(1)) >= 8:
            n.add((f"env:{k}:host", m.group(1)))
# Wall-clock leaks: today's date, and today's day number as apk/adduser would stamp it (not the pinned day).
now = time.time()
if int(now // 86400) != sde // 86400:
    n.add(("wall-clock", f":{int(now // 86400)}:"))
    n.add(("wall-clock", time.strftime("%Y-%m-%d", time.gmtime(now))))
# A loopback address says nothing about this host (and is in every /etc/hosts), so it is not a needle.
n = {(l, v) for l, v in n if len(v) >= 4 and "\n" not in v and "\t" not in v and v not in ("localhost", "127.0.0.1", "::1") and not v.startswith("127.")}
fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
os.write(fd, "".join(f"{l}\t{v}\n" for l, v in sorted(n)).encode())
os.close(fd)
EOF
rc=0
out="$(python3 "$ALPINE_BUILD_DIR/check-image.py" --root "$R" --lock "$BASE_LOCK" --origins "$BASE_ORIGINS" \
	--aports "$REPO_ROOT/$APORTS_DIR" --arch "$ALPINE_ARCH" --manifest "$manifest" --needles "$needles" 2>&1)" || rc=$?
rm -f "$needles"
printf '%s\n' "$out"
if [ "$rc" != 0 ] || grep -q '^FAIL' <<<"$out"; then fail=1; fi

echo "== sealed archive"
if (cd "$IMAGES_DIR" && sha256sum -c "$(basename "$tarball").sha256" >/dev/null 2>&1) &&
	[ "$(sed -n 's/^rootfs_tar_sha256=//p' "$manifest")" = "$(sha256sum "$tarball" | cut -d' ' -f1)" ]; then
	ok "archive sha256 matches its .sha256 file and the manifest"
else bad "archive checksum mismatch"; fi
if gzip -t "$tarball" 2>/dev/null; then ok "gzip stream intact"; else bad "gzip stream damaged"; fi
bad_members="$(tar -tzf "$tarball" | grep -E '(^|/)\.\.(/|$)|^/' || true)"
[ -z "$bad_members" ] && ok "no absolute or parent-relative member names" || bad "unsafe member names: $bad_members"
mkdir -p "$scratch/x"
tar --extract --gzip --file "$tarball" --directory "$scratch/x" --numeric-owner --same-permissions
if [ "$(python3 "$ALPINE_BUILD_DIR/tree-digest.py" "$scratch/x")" = "$(python3 "$ALPINE_BUILD_DIR/tree-digest.py" "$R")" ]; then
	ok "extracted archive is identical to the staged root (paths, modes, owners, links, contents)"
else bad "extracted archive differs from the staged root"; fi
rm -rf "${scratch:?}"

echo "== deterministic rebuild"
before="$(sha256sum "$tarball" | cut -d' ' -f1)"
"$ALPINE_BUILD_DIR/make-base.sh" >/dev/null 2>&1 || die "second make-base.sh run failed"
after="$(sha256sum "$tarball" | cut -d' ' -f1)"
if [ "$before" = "$after" ]; then ok "re-assembling the image from the packages gives the identical archive ($after)"; else bad "image is not deterministic: $before != $after"; fi

echo "== boot"
rc=0
out="$("$ALPINE_BUILD_DIR/boot-test.sh" 2>&1)" || rc=$?
printf '%s\n' "$out"
if [ "$rc" != 0 ] || grep -q '^FAIL' <<<"$out"; then fail=1; fi

echo "== host state"
if [ "$(grep -c "$BUILD_ROOT" /proc/mounts)" = 0 ]; then ok "no sandbox or boot-test mounts leaked into the host"; else bad "mounts leaked into the host"; fi
if msg="$(vendor_pristine)"; then ok "vendored aports still pristine after all checks"; else bad "vendored aports modified: $msg"; fi

if [ "$fail" = 0 ]; then echo "RESULT: base image OK"; else echo "RESULT: base image check FAILED"; fi
exit "$fail"
