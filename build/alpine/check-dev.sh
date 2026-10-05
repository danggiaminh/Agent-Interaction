#!/usr/bin/env bash
# Validate the development image produced by make-dev.sh (base system + Rust and C toolchain). Exit
# status 0 only if every check passes. Rebuilds the image from a clean state once to prove it is
# reproduced byte for byte, then compiles and runs the C, Rust and C-from-Rust test projects in it.
#   check-dev.sh
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root
fail=0
ok() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }

R="$IMAGES_DIR/dev-rootfs"
tarball="$IMAGES_DIR/$DEV_IMAGE_NAME.rootfs.tar.gz"
manifest="$IMAGES_DIR/$DEV_IMAGE_NAME.manifest"
base_tar="$IMAGES_DIR/$BASE_IMAGE_NAME.rootfs.tar.gz"
scratch="$IMAGES_DIR/check-dev"
[ -f "$tarball" ] || die "no development image yet; run make-dev.sh first"

echo "== pinned source"
if msg="$(vendor_pristine)"; then ok "vendored aports is the pinned tree ($APORTS_TREE) and pristine"; else bad "vendored aports not pristine: $msg"; fi
if [ "$(sed -n 's/^aports_commit=//p' "$manifest")" = "$APORTS_COMMIT" ] && [ "$(sed -n 's/^alpine_version=//p' "$manifest")" = "$ALPINE_VERSION" ] &&
	[ "$(sed -n 's/^base_rootfs_tar_sha256=//p' "$manifest")" = "$(sha256sum "$base_tar" | cut -d' ' -f1)" ]; then
	ok "manifest records aports commit $APORTS_COMMIT, Alpine $ALPINE_VERSION and the sha256 of the base image it extends"
else bad "manifest does not record the pinned source and base image"; fi
if [ "$(sort -u "$DEV_PKGS" | grep -vc '^#')" = "$(grep -vc '^#' "$DEV_PKGS")" ] && [ "$(grep -vc '^#' "$DEV_PKGS")" -gt 0 ]; then
	ok "dev.pkgs lists $(grep -v '^#' "$DEV_PKGS" | tr '\n' ' ')(no duplicates); everything else in the layer is their pinned closure"
else bad "dev.pkgs is empty or has duplicates"; fi

echo "== locked packages and installed root (in the sandbox, as root, offline)"
rc=0
out="$("$ENTER" --ephemeral --user root --offline --env "BASE_IMAGE_NAME=$BASE_IMAGE_NAME" -- /bin/sh /guest/check-dev.sh 2>&1)" || rc=$?
printf '%s\n' "$out"
if [ "$rc" != 0 ] || grep -q '^FAIL' <<<"$out"; then fail=1; fi

echo "== image contents"
rm -rf "${scratch:?}"
mkdir -p "$scratch"
needles="$scratch/needles"
# Strings that would betray host state: hostname, host paths, proxy and secret-looking env values.
# Written to a private file and never printed.
python3 - "$needles" "$REPO_ROOT" "$SOURCE_DATE_EPOCH" <<'PYEOF'
import os, re, socket, sys, time
out, repo, sde = sys.argv[1], sys.argv[2], int(sys.argv[3])
n = {("host-path", repo), ("host-path", "/tmp/claude-0"), ("host-path", "/root/.ccr"), ("host-path", "/home/user")}
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
PYEOF
rc=0
out="$(python3 "$ALPINE_BUILD_DIR/check-image.py" --root "$R" --lock "$BASE_LOCK" --origins "$BASE_ORIGINS" \
	--dev-lock "$DEV_LOCK" --dev-pkgs "$DEV_PKGS" \
	--aports "$REPO_ROOT/$APORTS_DIR" --arch "$ALPINE_ARCH" --manifest "$manifest" --needles "$needles" 2>&1)" || rc=$?
rm -f "$needles"
printf '%s\n' "$out"
if [ "$rc" != 0 ] || grep -q '^FAIL' <<<"$out"; then fail=1; fi

# The layer only adds: every path, mode, owner, link target and byte of the base image is still there
# unchanged, except the package database and the world file that apk rewrites.
rc=0
out="$(python3 - "$base_tar" "$tarball" <<'PYEOF' 2>&1
import hashlib, sys, tarfile

def load(path):
    d = {}
    with tarfile.open(path) as tf:
        for m in tf:
            h = ""
            if m.isreg():
                h = hashlib.sha256(tf.extractfile(m).read()).hexdigest()
            d[m.name.removeprefix("./")] = (m.type, m.mode, m.uid, m.gid, m.linkname, h)
    return d

base, dev = load(sys.argv[1]), load(sys.argv[2])
rewritten = ("lib/apk/db/", "etc/apk/world")
changed = [n for n in base if n in dev and base[n] != dev[n] and not n.startswith(rewritten)]
missing = [n for n in base if n not in dev]
print(f"{len(base)} base paths, {len(dev) - len(base) + len(missing)} added by the layer, {len(changed)} changed, {len(missing)} removed")
if changed or missing:
    print("changed:", changed[:8], "removed:", missing[:8])
    sys.exit(1)
PYEOF
)" || rc=$?
if [ "$rc" = 0 ]; then ok "base image preserved: $out (apart from lib/apk/db and etc/apk/world)"; else bad "layer modified the base image: $out"; fi

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

echo "== deterministic rebuild from a clean state"
before="$(sha256sum "$tarball" | cut -d' ' -f1)"
cp "$manifest" "$scratch.manifest.before"
"$ALPINE_BUILD_DIR/make-dev.sh" --clean >/dev/null 2>&1 || die "make-dev.sh --clean failed"
[ ! -e "$R" ] && [ ! -e "$tarball" ] || bad "make-dev.sh --clean left the development image behind"
"$ALPINE_BUILD_DIR/make-dev.sh" >/dev/null 2>&1 || die "rebuild with make-dev.sh failed"
after="$(sha256sum "$tarball" | cut -d' ' -f1)"
if [ "$before" = "$after" ]; then ok "clean rebuild from the base image and the pinned packages gives the identical archive ($after)"; else bad "image is not deterministic: $before != $after"; fi
if cmp -s "$scratch.manifest.before" "$manifest"; then ok "manifest identical after the rebuild"; else bad "manifest differs after the rebuild"; fi
rm -f "$scratch.manifest.before"

echo "== compile and run (C, Rust, C from Rust)"
rc=0
out="$("$ALPINE_BUILD_DIR/dev-test.sh" 2>&1)" || rc=$?
printf '%s\n' "$out"
if [ "$rc" != 0 ] || grep -q '^FAIL' <<<"$out"; then fail=1; fi

echo "== host state"
if [ "$(grep -c "$BUILD_ROOT" /proc/mounts)" = 0 ]; then ok "no sandbox or test mounts leaked into the host"; else bad "mounts leaked into the host"; fi
if msg="$(vendor_pristine)"; then ok "vendored aports still pristine after all checks"; else bad "vendored aports modified: $msg"; fi
if [ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all -- "$APORTS_DIR")" ]; then ok "git sees no change under $APORTS_DIR"; else bad "git sees changes under $APORTS_DIR"; fi

if [ "$fail" = 0 ]; then echo "RESULT: development image OK"; else echo "RESULT: development image check FAILED"; fi
exit "$fail"
