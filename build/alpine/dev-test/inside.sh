#!/bin/sh
# Runs inside the development image (dev-test.sh -> dev-run.sh) as root of an unprivileged user namespace,
# offline. Builds the projects in /work and prints one KEY=VALUE line per fact; dev-test.sh judges them.
# Nothing here fails the script: a failed step is reported as its own value.
cd /work || exit 1
kv() { printf '%s=%s\n' "$1" "$2"; }
first() { sed -n '1p'; }

# --- toolchain identity ---
kv gcc_version "$(gcc -dumpfullversion)"
kv cc_version "$(cc -dumpfullversion)"
kv gcc_machine "$(gcc -dumpmachine)"
kv ld_version "$(ld --version | first | sed 's/.* //')"
kv as_version "$(as --version | first | sed 's/.* //')"
kv ar_version "$(ar --version | first | sed 's/.* //')"
kv make_version "$(make --version | first | sed 's/.* //')"
kv rustc_version "$(rustc --version | cut -d' ' -f2)"
kv cargo_version "$(cargo --version | cut -d' ' -f2)"
kv rustc_host "$(rustc -vV | sed -n 's/^host: //p')"
kv rustc_sysroot "$(rustc --print sysroot)"
kv rustlib_targets "$(ls /usr/lib/rustlib | grep -v '^\(components\|manifest-.*\|multirust-.*\|rust-installer-version\|uninstall.sh\)$' | tr '\n' ' ')"
kv rustc_target_env_musl "$(rustc --print cfg | grep -c '^target_env="musl"')"
kv cc_path "$(command -v cc)"
kv user "$(id -u)"

# --- development files the toolchain needs ---
for f in crt1.o crti.o crtn.o libc.a libm.a libgcc.a; do
	p="$(gcc -print-file-name=$f)"
	case "$p" in /*) [ -f "$p" ] && kv "file_$f" present || kv "file_$f" missing ;; *) kv "file_$f" missing ;; esac
done
for h in stdio.h stdlib.h string.h stdint.h unistd.h; do
	[ -f "/usr/include/$h" ] && kv "header_$h" present || kv "header_$h" missing
done
kv rust_std_musl "$(ls /usr/lib/rustlib/x86_64-alpine-linux-musl/lib/libstd-*.rlib 2>/dev/null | wc -l)"

# --- environment and isolation ---
kv env_names "$(env | cut -d= -f1 | LC_ALL=C sort | tr '\n' ' ')"
kv interfaces "$(awk -F: 'NR > 2 { gsub(/ /, "", $1); print $1 }' /proc/net/dev | tr '\n' ' ')"
kv hostname "$(hostname)"

# --- C project ---
cd /work/c || exit 1
make clean >/dev/null 2>&1
if make >/tmp/c-build.log 2>&1; then kv c_build ok; else kv c_build failed; cat /tmp/c-build.log >&2; fi
kv c_run "$(./hello 2>&1)"
kv c_machine "$(readelf -h hello | sed -n 's/^ *Machine: *//p')"
kv c_class "$(readelf -h hello | sed -n 's/^ *Class: *//p')"
kv c_interp "$(readelf -l hello | sed -n 's/.*Requesting program interpreter: \(.*\)\]/\1/p')"
kv c_needed "$(readelf -d hello | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p' | tr '\n' ' ')"
sum1="$(sha256sum hello | cut -d' ' -f1)"
make clean >/dev/null 2>&1 && make >/dev/null 2>&1
kv c_rebuild_identical "$([ "$(sha256sum hello | cut -d' ' -f1)" = "$sum1" ] && echo yes || echo no)"
# -Werror must make a warning fail the build (proves the flags are really applied).
printf 'int main(void) { int unused; return 0; }\n' >/tmp/warn.c
kv c_werror_enforced "$(cc -Wall -Werror -o /tmp/warn /tmp/warn.c >/dev/null 2>&1 && echo no || echo yes)"
printf 'int main(void) { return undefined_symbol(); }\n' >/tmp/undef.c
kv c_link_error_reported "$(cc -o /tmp/undef /tmp/undef.c >/dev/null 2>&1 && echo no || echo yes)"

# --- Rust project ---
cd /work/rust || exit 1
if cargo build --release --locked --offline >/tmp/rust-build.log 2>&1; then kv rust_build ok; else kv rust_build failed; cat /tmp/rust-build.log >&2; fi
if cargo test --release --locked --offline >/tmp/rust-test.log 2>&1; then kv rust_test ok; else kv rust_test failed; cat /tmp/rust-test.log >&2; fi
kv rust_tests_passed "$(sed -n 's/^test result: ok\. \([0-9]*\) passed.*/\1/p' /tmp/rust-test.log | tr '\n' ' ')"
bin=target/release/hello-rust
kv rust_run "$($bin 2>&1)"
kv rust_machine "$(readelf -h $bin | sed -n 's/^ *Machine: *//p')"
kv rust_interp "$(readelf -l $bin | sed -n 's/.*Requesting program interpreter: \(.*\)\]/\1/p')"
kv rust_needed "$(readelf -d $bin | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p' | tr '\n' ' ')"
sum1="$(sha256sum $bin | cut -d' ' -f1)"
cargo clean --locked --offline >/dev/null 2>&1 && cargo build --release --locked --offline >/dev/null 2>&1
kv rust_rebuild_identical "$([ "$(sha256sum $bin | cut -d' ' -f1)" = "$sum1" ] && echo yes || echo no)"

# --- C + Rust project (build.rs: cc + ar, then linked into the Rust binary) ---
cd /work/ffi || exit 1
if cargo build --release --locked --offline >/tmp/ffi-build.log 2>&1; then kv ffi_build ok; else kv ffi_build failed; cat /tmp/ffi-build.log >&2; fi
kv ffi_run "$(target/release/hello-ffi 2>&1)"
kv ffi_machine "$(readelf -h target/release/hello-ffi | sed -n 's/^ *Machine: *//p')"
