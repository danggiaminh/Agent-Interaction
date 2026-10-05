#!/bin/sh
# Build one package from the read-only aports mount. Runs as the builder user inside the sandbox.
#   build-pkg.sh <repo>/<pkg>      e.g. main/zlib
set -eu
pkg="${1:?usage: build-pkg.sh <repo>/<pkg>}"
repo="${pkg%%/*}"
name="${pkg##*/}"
src="/aports/$repo/$name"
dst="/build/work/$repo/$name"
[ -f "$src/APKBUILD" ] || { echo "no APKBUILD at $src" >&2; exit 1; }

# abuild writes src/ and pkg/ next to the APKBUILD, so never build inside /aports: work on a copy.
rm -rf "$dst"
mkdir -p "/build/work/$repo"
cp -a "$src" "$dst"
# File mtimes must not depend on checkout time.
stamp="$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y%m%d%H%M.%S)"
find "$dst" -exec touch -h -t "$stamp" {} +
cd "$dst"
# Documented, per-package environment adjustments for host limitations (never edits the APKBUILD).
if [ -f "/guest/pkg-env.d/$name.env" ]; then
	echo ">>> applying /guest/pkg-env.d/$name.env"
	# shellcheck disable=SC1090
	. "/guest/pkg-env.d/$name.env"
fi
abuild -r
