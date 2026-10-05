#!/usr/bin/env bash
# Build one aports package inside the sandbox, from a copy of the read-only vendored tree.
#   build-pkg.sh <repo>/<pkg>      e.g. main/zlib
# Output: .build/packages/<repo>/<arch>/*.apk (signed with the per-environment key).
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
pkg="${1:?usage: build-pkg.sh <repo>/<pkg>}"
[[ "$pkg" =~ ^[a-z]+/[A-Za-z0-9._+-]+$ ]] || die "package must look like main/zlib, got '$pkg'"
require_root
msg="$(vendor_pristine)" || die "vendored aports not pristine before the build: $msg"
"$ENTER" --user builder -- /bin/sh /guest/build-pkg.sh "$pkg"
msg="$(vendor_pristine)" || die "vendored aports was MODIFIED by the build: $msg"
log "built $pkg; vendored tree unchanged"
