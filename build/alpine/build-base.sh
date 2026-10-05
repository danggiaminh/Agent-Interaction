#!/usr/bin/env bash
# Build every aport of the base system (guest/base.origins) from the read-only vendored tree.
#   build-base.sh [--clean] [--from <repo>/<name>]
# --clean first empties the package repository and work tree, so everything is built from source in
# this run (the distfile cache is kept; abuild re-verifies each source against the APKBUILD sha512).
# --from resumes the list at that origin (packages before it must already be in .build/packages).
# Output: .build/packages/<repo>/<arch>/*.apk and its signed APKINDEX; logs in .build/logs/.
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
require_root
clean=0
from=""
while [ $# -gt 0 ]; do
	case "$1" in
	--clean) clean=1; shift ;;
	--from) from="${2:?--from needs <repo>/<name>}"; shift 2 ;;
	*) die "usage: build-base.sh [--clean] [--from <repo>/<name>]" ;;
	esac
done
[ -z "$from" ] || [ "$clean" = 0 ] || die "--clean and --from cannot be combined"

msg="$(vendor_pristine)" || die "vendored aports not pristine: $msg"
if [ "$clean" = 1 ]; then
	log "clean: emptying $PKG_DIR and $WORK_DIR"
	find "${PKG_DIR:?}" -mindepth 1 -delete
	find "${WORK_DIR:?}" -mindepth 1 -delete
fi
mkdir -p "$LOGS_DIR"

start="$(date +%s)"
while read -r origin; do
	case "$origin" in '' | '#'*) continue ;; esac
	if [ -n "$from" ]; then
		[ "$origin" = "$from" ] || continue
		from=""
	fi
	t0="$(date +%s)"
	logf="$LOGS_DIR/${origin//\//_}.log"
	log "building $origin"
	if ! "$ALPINE_BUILD_DIR/build-pkg.sh" "$origin" >"$logf" 2>&1; then
		tail -n 40 "$logf"
		die "build of $origin failed (full log: $logf)"
	fi
	log "  ok $origin ($(($(date +%s) - t0))s)"
done <"$BASE_ORIGINS"

n="$(find "$PKG_DIR" -name '*.apk' | wc -l)"
log "built $n packages in $(($(date +%s) - start))s; vendored tree unchanged"
