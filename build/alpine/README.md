# Alpine build environment (v3.24.2)

Reproducible, isolated environment for building Alpine packages from the vendored
`vendor/alpine-aports` tree (see `vendor/alpine-aports.PROVENANCE.md`). Linux, root, `x86_64`.

## Usage (from the repo root, as root)
```sh
build/alpine/bootstrap.sh              # fetch + verify base, install toolchain (once; --force to recreate)
build/alpine/check-env.sh              # clean-environment check, exit 0 only if everything passes
build/alpine/build-pkg.sh main/zlib    # build a package -> .build/packages/<repo>/x86_64/*.apk
build/alpine/repro-check.sh main/zlib  # build twice from scratch, compare .apk files byte for byte
build/alpine/enter.sh [--offline] [--user root] [-- cmd]   # shell in the sandbox
```

## What is pinned (`config.env`, `guest/toolchain.lock`)
- Base: official `alpine-minirootfs-3.24.2-x86_64`, verified by sha256 **and** GPG signature (pinned fingerprint, key in `ncopa.asc`).
- Package repos: `v3.24` main + community only. Exact versions of all 61 installed packages: `guest/toolchain.lock`.
- Source: `vendor/alpine-aports` tree id `475841678d601c7c30d0d93e406463547e6e3dd0` (commit `d9d560d5…`).
- `SOURCE_DATE_EPOCH` = aports commit time, `JOBS`/`MAKEFLAGS`, `PACKAGER`, `TZ=UTC`, `LANG=C.UTF-8`.

## Isolation
- Own mount/PID/IPC/UTS namespaces, chroot into `.build/rootfs`, environment rebuilt with `env -i` (no host variables or tokens;
  only `HTTPS_PROXY`/`NO_PROXY` and the CA bundle are passed when the host uses a proxy). `--offline` adds a network namespace.
- `/aports` is a **read-only** bind mount. Builds run on a copy under `.build/work` (abuild writes `src/` and `pkg/` beside the
  APKBUILD, and upstream's `.gitignore` would hide such files inside `vendor/`). `build-pkg.sh` fails if the vendored tree changes.
- Builds run as the unprivileged `builder` user; the package-signing key is generated per environment under `.build/` and never committed.
- All generated state lives in `.build/` (gitignored).

## Limits
- Alpine retires superseded package versions, so a later `bootstrap.sh --force` may fail to install the locked versions.
  Refresh deliberately with `bootstrap.sh --force --refresh-lock` and review the diff.
- `.apk` bytes are reproducible within one environment (same signing key). Across fresh bootstraps the signatures differ.
- Source distfiles are fetched over the network (verified against `sha512sums` in the APKBUILD); there is no mirror yet.
