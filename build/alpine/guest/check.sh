#!/bin/sh
# In-sandbox environment checks. Runs as the builder user; prints PASS/FAIL lines, exits 1 on any FAIL.
fail=0
ok() { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=1; }
chk() { name="$1"; shift; if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi; }
eq() { if [ "$2" = "$3" ]; then ok "$1 ($3)"; else bad "$1: got '$2', want '$3'"; fi; }

# --- pinned release ---
eq "alpine release" "$(cat /etc/alpine-release)" "$ALPINE_VERSION"
eq "apk architecture" "$(apk --print-arch)" "$ALPINE_ARCH"
eq "apk repositories pinned to branch" "$(tr '\n' ' ' </etc/apk/repositories)" "$ALPINE_MIRROR/$ALPINE_BRANCH/main $ALPINE_MIRROR/$ALPINE_BRANCH/community "
chk "toolchain matches toolchain.lock" sh -c '[ "$(/bin/sh /guest/lock.sh | grep -v "^#")" = "$(grep -v "^#" /guest/toolchain.lock)" ]'

# --- vendored source as seen from the sandbox ---
eq "alpine-base pkgver in /aports" "$(sed -n 's/^pkgver=//p' /aports/main/alpine-base/APKBUILD)" "$ALPINE_VERSION"
chk "/aports is mounted read-only" sh -c '! touch /aports/.rwtest 2>/dev/null'
chk "/aports/main/zlib/APKBUILD present" test -f /aports/main/zlib/APKBUILD

# --- isolation ---
chk "running unprivileged" sh -c '[ "$(id -u)" != 0 ]'
chk "member of abuild group" sh -c 'id -Gn | tr " " "\n" | grep -qx abuild'
chk "host env canary not visible" sh -c '[ -z "${AGENT_ENV_CANARY+x}" ]'
allowed=" ALPINE_ARCH ALPINE_BRANCH ALPINE_MIRROR ALPINE_VERSION APORTS_COMMIT APORTS_TREE DISTFILES_MIRROR HOME HTTPS_PROXY JOBS LANG NO_PROXY PACKAGER PATH PWD OLDPWD SHLVL REPODEST SOURCE_DATE_EPOCH SRCDEST TERM TZ https_proxy no_proxy _ "
leaked=""
for v in $(env | sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p'); do
	case "$allowed" in *" $v "*) ;; *) leaked="$leaked $v" ;; esac
done
if [ -z "$leaked" ]; then ok "environment holds only allow-listed variables"; else bad "unexpected variables:$leaked"; fi
chk "own PID namespace (pid 1 is the sandbox init)" sh -c "tr '\0' ' ' </proc/1/cmdline | grep -q -- '--inner'"
chk "own UTS namespace (hostname alpine-build)" sh -c '[ "$(hostname)" = alpine-build ]'
chk "/tmp is tmpfs" grep -q ' /tmp tmpfs ' /proc/mounts
for d in work packages distfiles; do chk "/build/$d writable" sh -c "touch /build/$d/.w && rm /build/$d/.w"; done
chk "/build/images is not writable by the builder" sh -c '[ -d /build/images ] && ! touch /build/images/.w 2>/dev/null'

# --- toolchain and signing ---
for t in abuild abuild-apk apk gcc g++ make git patch su-exec tar wget; do chk "tool: $t" command -v "$t"; done
chk "package signing key configured" sh -c 'grep -q "^PACKAGER_PRIVKEY=" ~/.abuild/abuild.conf'
chk "signing public key trusted by apk" sh -c 'ls /etc/apk/keys/*agent-interaction*.rsa.pub'
case "$SOURCE_DATE_EPOCH" in '' | *[!0-9]*) bad "SOURCE_DATE_EPOCH not numeric" ;; *) ok "SOURCE_DATE_EPOCH set ($SOURCE_DATE_EPOCH)" ;; esac

[ "$fail" = 0 ]
