# Helpers shared by unpriv.sh and priv.sh (sourced inside the tools image). Each fact is one KEY=VALUE line.
# Nothing here fails the script: a failed step is reported as its own value.
T=/tmp/t
mkdir -p "$T"
kv() { printf '%s=%s\n' "$1" "$2"; }
# run KEY CMD...: "ok", or "fail:" and the last line of the output when the command fails.
run() {
	k="$1"
	shift
	if "$@" >"$T/$k.log" 2>&1; then kv "$k" ok; else kv "$k" "fail:$(tail -n 1 "$T/$k.log" | cut -c1-200)"; fi
}
# found KEY REGEX CMD...: the first output line (stdout and stderr) matching the extended regex, else "none:" and
# the last line of the output. The exit status of the command is not judged.
found() {
	k="$1"
	re="$2"
	shift 2
	"$@" >"$T/$k.log" 2>&1
	m="$(grep -E -m 1 -e "$re" "$T/$k.log" | cut -c1-200)"
	if [ -n "$m" ]; then kv "$k" "$m"; else kv "$k" "none:$(tail -n 1 "$T/$k.log" | cut -c1-200)"; fi
}
# lastline FILE: the last non-empty line of FILE, at most 200 characters.
lastline() { grep . "$1" | tail -n 1 | cut -c1-200; }
# firstline FILE: the first non-empty line of FILE, at most 200 characters.
firstline() { grep . "$1" | head -n 1 | cut -c1-200; }
# kvm_probe KEY: "ok" when QEMU can start a machine with -accel kvm (the stopped machine then sits there until the
# timeout kills it; without KVM QEMU exits at once), "unsupported:<QEMU's own reason>" when the host has no /dev/kvm,
# "fail:" when /dev/kvm exists and QEMU still cannot use it.
kvm_probe() {
	kvm_out="$(timeout 3 qemu-system-x86_64 -accel kvm -display none -S -monitor none -machine q35 2>&1)" && kvm_rc=0 || kvm_rc=$?
	case "$kvm_rc" in
	124 | 143) kv "$1" ok ;;
	*) if [ -e /dev/kvm ]; then kv "$1" "fail:/dev/kvm exists but qemu -accel kvm exits: $(echo "$kvm_out" | tail -n 1 | cut -c1-200)"
	else kv "$1" "unsupported:$(echo "$kvm_out" | tail -n 1 | cut -c1-200)"; fi ;;
	esac
}
# same KEY FILE FILE: yes/no, are the two files byte for byte equal.
same() { kv "$1" "$(cmp -s "$2" "$3" && echo yes || echo no)"; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
want() { [ $# -eq 0 ] || case " $* " in *" $SECTION "*) return 0 ;; *) return 1 ;; esac; }
SECTIONS="$*"
section() { SECTION="$1"; want $SECTIONS; }
# ensure_rust: the release build of the Rust fixture in $T/rust, built once per session (sections may run alone).
ensure_rust() {
	[ -x "$T/rust/target/release/burn" ] && return 0
	rm -rf "$T/rust"
	cp -a /work/rust "$T/rust"
	(cd "$T/rust" && cargo build --release --offline --locked) >"$T/ensure_rust.log" 2>&1
}
# waitfor SECONDS CMD...: poll CMD every 0.5 s until it succeeds.
waitfor() {
	n=$(($1 * 2))
	shift
	while [ "$n" -gt 0 ]; do
		"$@" >/dev/null 2>&1 && return 0
		sleep 0.5
		n=$((n - 1))
	done
	return 1
}
