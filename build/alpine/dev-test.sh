#!/usr/bin/env bash
# Build and run C, Rust and mixed C+Rust projects inside the development image and check what the
# toolchain reports (versions equal the lock, target, linker, files, environment, isolation).
#   dev-test.sh [image.rootfs.tar.gz]      default: .build/images/<dev image>.rootfs.tar.gz
# Exit status 0 only if every check passes.
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root
fail=0
ok() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }

image="${1:-$IMAGES_DIR/$DEV_IMAGE_NAME.rootfs.tar.gz}"
[ -f "$image" ] || die "no development image at $image; run make-dev.sh first"

facts="$(mktemp)"
trap 'rm -f "$facts"' EXIT
log "building the test projects inside the image (offline, unprivileged user namespace)"
"$ALPINE_BUILD_DIR/dev-run.sh" --image "$image" --copy "$ALPINE_BUILD_DIR/dev-test:/work" -- /bin/sh /work/inside.sh >"$facts" || bad "in-image test run exited non-zero"

get() { sed -n "s/^$1=//p" "$facts" | head -n 1; }
# lock_version <package>: the pinned upstream version without the -rN release.
lock_version() { awk -v p="$1" '$1 == p { sub(/-r[0-9]+$/, "", $2); print $2 }' "$DEV_LOCK"; }
expect() { # expect <label> <fact> <wanted>
	local got
	got="$(get "$2")"
	if [ "$got" = "$3" ]; then ok "$1: $got"; else bad "$1: got '$got', wanted '$3'"; fi
}

echo "== toolchain versions equal the lock"
expect "gcc" gcc_version "$(lock_version gcc)"
expect "cc is gcc" cc_version "$(lock_version gcc)"
expect "binutils ld" ld_version "$(lock_version binutils)"
expect "binutils as" as_version "$(lock_version binutils)"
expect "binutils ar" ar_version "$(lock_version binutils)"
expect "make" make_version "$(lock_version make)"
expect "rustc" rustc_version "$(lock_version rust)"
expect "cargo" cargo_version "$(lock_version cargo)"

echo "== target architecture"
expect "gcc target" gcc_machine "$ALPINE_ARCH-alpine-linux-musl"
expect "rustc host" rustc_host "$ALPINE_ARCH-alpine-linux-musl"
expect "rustc target_env" rustc_target_env_musl 1
expect "rust targets installed (host only)" rustlib_targets "$ALPINE_ARCH-alpine-linux-musl "
expect "rustc sysroot" rustc_sysroot /usr
expect "cc resolves to" cc_path /usr/bin/cc

echo "== compiler, linker and development files"
for f in crt1.o crti.o crtn.o libc.a libm.a libgcc.a; do expect "$f" "file_$f" present; done
for h in stdio.h stdlib.h string.h stdint.h unistd.h; do expect "/usr/include/$h" "header_$h" present; done
[ "$(get rust_std_musl)" -ge 1 ] 2>/dev/null && ok "Rust standard library for the musl target present" || bad "Rust standard library missing"
expect "C -Werror is enforced" c_werror_enforced yes
expect "C link errors are reported" c_link_error_reported yes

echo "== C project (make + cc)"
expect "build" c_build ok
expect "output" c_run "c-ok 42"
expect "ELF machine" c_machine "Advanced Micro Devices X86-64"
expect "ELF class" c_class ELF64
expect "musl dynamic loader" c_interp "/lib/ld-musl-$ALPINE_ARCH.so.1"
expect "needs only libc.musl" c_needed "libc.musl-$ALPINE_ARCH.so.1 "
expect "rebuild is byte-identical" c_rebuild_identical yes

echo "== Rust project (cargo build + test, locked, offline)"
expect "build" rust_build ok
expect "tests" rust_test ok
expect "unit test ran" rust_tests_passed "1 "
expect "output" rust_run "rust-ok 42"
expect "ELF machine" rust_machine "Advanced Micro Devices X86-64"
expect "musl dynamic loader" rust_interp "/lib/ld-musl-$ALPINE_ARCH.so.1"
# Rust's std unwinds through libgcc_s (package libgcc, part of the layer); nothing else besides musl may be needed.
expect "needs only libc.musl and libgcc_s" rust_needed "libc.musl-$ALPINE_ARCH.so.1 libgcc_s.so.1 "
expect "rebuild is byte-identical" rust_rebuild_identical yes

echo "== C + Rust project (build.rs runs cc and ar, rustc links the archive)"
expect "build" ffi_build ok
expect "output" ffi_run "ffi-ok 42"
expect "ELF machine" ffi_machine "Advanced Micro Devices X86-64"

echo "== environment and isolation"
expect "runs unprivileged on the host (root of a user namespace)" user 0
expect "network namespace has loopback only" interfaces "lo "
# Only the names dev-run.sh sets (plus what the shell adds itself); nothing inherited from the host.
want="CARGO_HOME CARGO_INCREMENTAL CARGO_NET_OFFLINE CARGO_TERM_COLOR HOME LANG PATH SOURCE_DATE_EPOCH TERM TZ"
got="$(get env_names | tr ' ' '\n' | grep -vx -e PWD -e OLDPWD -e SHLVL -e _ -e '' | tr '\n' ' ')"
[ "$got" = "$want " ] && ok "environment holds exactly the contract variables ($want)" || bad "environment variables: '$got', wanted '$want '"
if [ "$(grep -c "$BUILD_ROOT" /proc/mounts)" = 0 ]; then ok "no mounts leaked into the host"; else bad "mounts leaked into the host"; fi
leftover="$(find "$BUILD_ROOT" -maxdepth 1 -name 'dev-run.*' | head -n 1)"
[ -z "$leftover" ] && ok "scratch root removed" || bad "scratch root left behind"

if [ "$fail" = 0 ]; then echo "RESULT: C and Rust build OK"; else echo "RESULT: dev test FAILED"; fi
exit "$fail"
