#!/usr/bin/env bash
# Validate a layered image: the development image (base system + Rust and C toolchain, made by make-dev.sh)
# or the tools image (development image + systems tools, made by make-tools.sh). Exit status 0 only if every
# check passes. Rebuilds the image from a clean state once to prove it is reproduced byte for byte, then
# runs the layer's functional tests in it. Use check-dev.sh or check-tools.sh, not this script.
#   check-layer.sh dev|tools
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root
fail=0
ok() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }

layer="${1:-}"
case "$layer" in
dev)
	label="development image" layer_label="development layer" parent_label="base image" make=make-dev.sh test=dev-test.sh
	test_label="compile and run (C, Rust, C from Rust)"
	name="$DEV_IMAGE_NAME" parent="$BASE_IMAGE_NAME" parent_key=base pkgs="$DEV_PKGS" lock="$DEV_LOCK"
	layer_args=(--layer "development layer,$DEV_LOCK,$DEV_PKGS")
	setid_args=()
	exempt_args=()
	;;
tools)
	label="tools image" layer_label="tools layer" parent_label="development image" make=make-tools.sh test=tools-test.sh
	test_label="functional tests of the tools"
	name="$TOOLS_IMAGE_NAME" parent="$DEV_IMAGE_NAME" parent_key=dev pkgs="$TOOLS_PKGS" lock="$TOOLS_LOCK"
	layer_args=(--layer "development layer,$DEV_LOCK,$DEV_PKGS" --layer "tools layer,$TOOLS_LOCK,$TOOLS_PKGS,$TOOLS_EXCEPTIONS")
	# the real util-linux mount and umount replace busybox's links to bbsuid; wall is setgid tty; qemu-bridge-helper is
	# setuid root, group qemu (only that group may run it)
	setid_args=(--setid bin/mount:4755 --setid bin/umount:4755 --setid usr/bin/wall:2755 --setid usr/lib/qemu/qemu-bridge-helper:4710)
	# Package-owned paths that trip a check for a reason that is the package's, not the image's (each is verified to
	# still apply by check-image.py, so a stale entry fails):
	#   seabios firmware: 32-bit x86 code that QEMU loads as a BIOS, never run by the image's own kernel
	#   c-index-test: the clang-extra-tools link points at ../lib/llvm22/bin/c-index-test, which no package provides (the
	#     vendored clang22 APKBUILD builds that tool only under want_check); check-image.py verifies that no installed
	#     package owns the missing target
	#   host-path needles: example paths in upstream documentation and strings, not a path of this host
	exempt_args=(--non-x86 usr/share/seabios/bios-coreboot.bin
		--dangling usr/bin/c-index-test
		--needle-ok usr/share/cmake/Modules/FindDoxygen.cmake:/home/user
		--needle-ok usr/share/cmake/Help/variable/CMAKE_EXPORT_SARIF.rst:/home/user
		--needle-ok usr/share/cmake/Help/variable/CMAKE_EXPORT_COMPILE_COMMANDS.rst:/home/user
		--needle-ok usr/libexec/docker/cli-plugins/docker-buildx:/home/user
		--needle-ok usr/bin/gdb:/home/user)
	;;
*) die "usage: check-layer.sh dev|tools (use check-dev.sh or check-tools.sh)" ;;
esac
pkgs_file="$(basename "$pkgs")" lock_file="$(basename "$lock")"

R="$IMAGES_DIR/$layer-rootfs"
tarball="$IMAGES_DIR/$name.rootfs.tar.gz"
manifest="$IMAGES_DIR/$name.manifest"
parent_tar="$IMAGES_DIR/$parent.rootfs.tar.gz"
scratch="$IMAGES_DIR/check-$layer"
[ -f "$tarball" ] || die "no $label yet; run $make first"

echo "== pinned source"
if msg="$(vendor_pristine)"; then ok "vendored aports is the pinned tree ($APORTS_TREE) and pristine"; else bad "vendored aports not pristine: $msg"; fi
if [ "$(sed -n 's/^aports_commit=//p' "$manifest")" = "$APORTS_COMMIT" ] && [ "$(sed -n 's/^alpine_version=//p' "$manifest")" = "$ALPINE_VERSION" ] &&
	[ "$(sed -n "s/^${parent_key}_rootfs_tar_sha256=//p" "$manifest")" = "$(sha256sum "$parent_tar" | cut -d' ' -f1)" ] &&
	[ "$(sed -n "s/^${parent_key}_image=//p" "$manifest")" = "$parent" ]; then
	ok "manifest records aports commit $APORTS_COMMIT, Alpine $ALPINE_VERSION and the sha256 of the $parent_label it extends"
else bad "manifest does not record the pinned source and the $parent_label"; fi
if [ "$(grep -v '^#' "$pkgs" | sort -u | wc -l)" = "$(grep -vc '^#' "$pkgs")" ] && [ "$(grep -vc '^#' "$pkgs")" -gt 0 ]; then
	ok "$pkgs_file lists $(grep -vc '^#' "$pkgs") top-level packages (no duplicates); everything else in the layer is their pinned closure"
else bad "$pkgs_file is empty or has duplicates"; fi

echo "== locked packages and installed root (in the sandbox, as root, offline)"
rc=0
out="$("$ENTER" --ephemeral --user root --offline --env "PARENT_IMAGE_NAME=$parent" --env "LAYER_ROOT=$layer-rootfs" \
	--env "LAYER_LOCK=/guest/$lock_file" -- /bin/sh /guest/check-layer.sh 2>&1)" || rc=$?
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
PYEOF
rc=0
out="$(python3 "$ALPINE_BUILD_DIR/check-image.py" --root "$R" --lock "$BASE_LOCK" --origins "$BASE_ORIGINS" \
	"${layer_args[@]}" "${setid_args[@]}" "${exempt_args[@]}" \
	--aports "$REPO_ROOT/$APORTS_DIR" --arch "$ALPINE_ARCH" --manifest "$manifest" --needles "$needles" 2>&1)" || rc=$?
rm -f "$needles"
printf '%s\n' "$out"
if [ "$rc" != 0 ] || grep -q '^FAIL' <<<"$out"; then fail=1; fi

# The layer only adds: every path, mode, owner, link target and byte of the parent image is still there
# unchanged, except the package database and the world file that apk rewrites.
rc=0
out="$(python3 - "$parent_tar" "$tarball" <<'PYEOF' 2>&1
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
appendable = ("etc/passwd", "etc/group", "etc/shadow", "etc/passwd-", "etc/group-", "etc/shadow-", "etc/shells")

def displaced_applet(n):
    # busybox's applet links (-> /bin/busybox, or -> /bin/bbsuid for the setuid ones) belong to no package; a
    # real tool of the layer (binutils' strings, util-linux's mount, iproute2's ip, util-linux's linux32 as a
    # link to setarch) legitimately takes their place, exactly as on any Alpine system that has both installed.
    return (base[n][0] == tarfile.SYMTYPE and base[n][4] in ("/bin/busybox", "/bin/bbsuid")
            and (dev[n][0] == tarfile.REGTYPE or (dev[n][0] == tarfile.SYMTYPE and dev[n][4] != base[n][4])))

def lines(path, n):
    with tarfile.open(path) as tf:
        for m in tf:
            if m.name.removeprefix("./") == n:
                return tf.extractfile(m).read().decode().splitlines()
    return []

def covers(old, new):
    # The new line is the old one, or (a group line) the old one with members added: packages add their
    # system users to existing groups too (qemu joins the parent's kvm group).
    if old == new:
        return True
    o, w = old.split(":"), new.split(":")
    return (len(o) == 4 and len(w) == 4 and o[:3] == w[:3]
            and set(filter(None, o[3].split(","))) <= set(filter(None, w[3].split(","))))

def extended_file(n):
    # Packages that add a system user or group (adduser/addgroup in their install scripts) append lines to the
    # account files, add their users to existing groups and register their shell in /etc/shells; the backup
    # copies (etc/passwd- ...) hold the state before the last change. Same type, mode and owner, and every
    # line of the parent must still be there, in order, unchanged apart from added group members.
    if n not in appendable or base[n][:5] != dev[n][:5]:
        return False
    new = iter(lines(sys.argv[2], n))
    return all(any(covers(line, cand) for cand in new) for line in lines(sys.argv[1], n))

differs = [n for n in base if n in dev and base[n] != dev[n] and not n.startswith(rewritten)]
replaced = [n for n in differs if displaced_applet(n)]
extended = [n for n in differs if n not in replaced and extended_file(n)]
changed = [n for n in differs if n not in replaced and n not in extended]
missing = [n for n in base if n not in dev]
print(f"{len(base)} parent paths, {len(dev) - len(base) + len(missing)} added by the layer, {len(replaced)} busybox applet link(s) replaced by the real tool ({', '.join(sorted(replaced))}), "
      f"{len(extended)} account/shell file(s) extended by the layer ({', '.join(sorted(extended))}), {len(changed)} changed, {len(missing)} removed")
if changed or missing:
    print("changed:", changed[:8], "removed:", missing[:8])
    sys.exit(1)
PYEOF
)" || rc=$?
if [ "$rc" = 0 ]; then ok "$parent_label preserved: $out (apart from lib/apk/db and etc/apk/world)"; else bad "layer modified the $parent_label: $out"; fi

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
"$ALPINE_BUILD_DIR/$make" --clean >/dev/null 2>&1 || die "$make --clean failed"
[ ! -e "$R" ] && [ ! -e "$tarball" ] || bad "$make --clean left the $label behind"
"$ALPINE_BUILD_DIR/$make" >/dev/null 2>&1 || die "rebuild with $make failed"
after="$(sha256sum "$tarball" | cut -d' ' -f1)"
if [ "$before" = "$after" ]; then ok "clean rebuild from the $parent_label and the pinned packages gives the identical archive ($after)"; else bad "image is not deterministic: $before != $after"; fi
if cmp -s "$scratch.manifest.before" "$manifest"; then ok "manifest identical after the rebuild"; else bad "manifest differs after the rebuild"; fi
rm -f "$scratch.manifest.before"

if [ "$layer" = tools ]; then
	# the cgroup v2 test bed is made from the tools image just rebuilt: built again from nothing, then a second build must
	# reproduce it byte for byte (make-guest.sh --check)
	echo "== cgroup v2 test bed (guest kernel of the functional tests)"
	rc=0
	out="$("$ALPINE_BUILD_DIR/make-guest.sh" --clean 2>&1 && "$ALPINE_BUILD_DIR/make-guest.sh" 2>&1 && "$ALPINE_BUILD_DIR/make-guest.sh" --check 2>&1)" || rc=$?
	printf '%s\n' "$out"
	if [ "$rc" != 0 ] || grep -q '^FAIL' <<<"$out"; then bad "the cgroup v2 test bed is not reproduced from its pins"; fi
fi

echo "== $test_label"
rc=0
out="$("$ALPINE_BUILD_DIR/$test" 2>&1)" || rc=$?
printf '%s\n' "$out"
if [ "$rc" != 0 ] || grep -q '^FAIL' <<<"$out"; then fail=1; fi

echo "== host state"
if [ "$(grep -c "$BUILD_ROOT" /proc/mounts)" = 0 ]; then ok "no sandbox or test mounts leaked into the host"; else bad "mounts leaked into the host"; fi
if msg="$(vendor_pristine)"; then ok "vendored aports still pristine after all checks"; else bad "vendored aports modified: $msg"; fi
if [ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all -- "$APORTS_DIR")" ]; then ok "git sees no change under $APORTS_DIR"; else bad "git sees changes under $APORTS_DIR"; fi

if [ "$fail" = 0 ]; then echo "RESULT: $label OK"; else echo "RESULT: $label check FAILED"; fi
exit "$fail"
