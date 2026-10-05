#!/usr/bin/env bash
# Build a package twice from scratch and compare the resulting .apk files byte for byte.
#   repro-check.sh [<repo>/<pkg>]      default: main/zlib
# Both builds use the same sandbox, pinned inputs, SOURCE_DATE_EPOCH and signing key.
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
pkg="${1:-main/zlib}"
[[ "$pkg" =~ ^[a-z]+/[A-Za-z0-9._+-]+$ ]] || die "package must look like main/zlib, got '$pkg'"
repo="${pkg%%/*}"
name="${pkg##*/}"
require_root
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
for run in 1 2; do
	rm -rf "${PKG_DIR:?}/$repo" "${WORK_DIR:?}/$repo/$name"
	"$ALPINE_BUILD_DIR/build-pkg.sh" "$pkg" >"$tmp/build$run.log" 2>&1 || { tail -n 40 "$tmp/build$run.log"; die "build $run of $pkg failed"; }
	(cd "$PKG_DIR/$repo/$ALPINE_ARCH" && sha256sum -- *.apk) >"$tmp/sums$run"
	log "build $run: $(wc -l <"$tmp/sums$run") apk files"
done
cat "$tmp/sums1"
if diff -u "$tmp/sums1" "$tmp/sums2"; then
	log "REPRODUCIBLE: both builds of $pkg are byte-identical"
else
	die "NOT reproducible: $pkg differs between two clean builds"
fi
