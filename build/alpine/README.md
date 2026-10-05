# Alpine build environment (v3.24.2)

Reproducible, isolated environment for building Alpine packages from the vendored
`vendor/alpine-aports` tree (see `vendor/alpine-aports.PROVENANCE.md`), and the minimal Alpine base
system image built with it, extended by a Rust and C development layer. Linux, root, `x86_64`.

## Usage (from the repo root, as root)
```sh
build/alpine/bootstrap.sh              # fetch + verify base, install toolchain (once; --force to recreate)
build/alpine/check-env.sh              # clean-environment check, exit 0 only if everything passes
build/alpine/build-pkg.sh main/zlib    # build a package -> .build/packages/<repo>/x86_64/*.apk
build/alpine/repro-check.sh main/zlib  # build twice from scratch, compare .apk files byte for byte
build/alpine/enter.sh [--offline] [--ephemeral] [--user root] [-- cmd]   # shell in the sandbox

build/alpine/build-base.sh --clean     # build the 16 aports of the base system from source (~25 min)
build/alpine/make-base.sh              # assemble .build/images/alpine-base-3.24.2-x86_64.rootfs.tar.gz
build/alpine/check-base.sh             # validate package set, filesystem, determinism and boot

build/alpine/make-dev.sh               # add the Rust + C toolchain: .build/images/alpine-dev-3.24.2-x86_64.rootfs.tar.gz
build/alpine/check-dev.sh              # validate the dev image, rebuild it from clean, compile and run C and Rust projects
build/alpine/dev-run.sh [--copy SRC:DST] [--out DIR:HOSTDIR] -- cmd   # run a command in a throwaway copy of the dev image
```

## What is pinned (`config.env`, `guest/toolchain.lock`, `guest/base.lock`)
- Base: official `alpine-minirootfs-3.24.2-x86_64`, verified by sha256 **and** GPG signature (pinned fingerprint, key in `ncopa.asc`).
- Package repos: `v3.24` main + community only. Exact versions of all 61 installed packages: `guest/toolchain.lock`.
- Source: `vendor/alpine-aports` tree id `475841678d601c7c30d0d93e406463547e6e3dd0` (commit `d9d560d5…`).
- `SOURCE_DATE_EPOCH` = aports commit time, `JOBS`/`MAKEFLAGS`, `PACKAGER`, `TZ=UTC`, `LANG=C.UTF-8`.
- Source distfiles come from Alpine's `distfiles.alpinelinux.org/distfiles/v3.24` mirror first (`gitlab.alpinelinux.org` answers
  scripted archive downloads with a JavaScript challenge, HTTP 418). The mirror is not trusted: abuild verifies every file against
  the `sha512sums` in the APKBUILD.

## Development layer (Rust + C)
`make-dev.sh` extends the sealed base image with the toolchain Agent-Interaction needs to compile Rust and C. It adds nothing else.
- **Definition:** `guest/dev.pkgs` names five packages (`gcc`, `musl-dev`, `make`, `rust`, `cargo`); `guest/dev.lock` pins their whole
  dependency closure, 31 packages, each with its exact version, origin aport and the sha256 of its `.apk`. Everything else they need
  (`musl`, `libssl3`, `libcrypto3`, `zlib`, ...) is already in the base image. Toolchain: gcc 15.2.0 with binutils 2.45.1 (the linker
  driver rustc uses), make 4.4.1, rust and cargo 1.96.1 (LLVM 22.1.3), musl-dev 1.2.6. Target: `x86_64-alpine-linux-musl`, the only
  Rust target installed. C headers and `libc.a` come from `musl-dev`; no other `-dev` package is installed, so a project that needs one
  (e.g. OpenSSL headers) adds it to `dev.pkgs` and refreshes the lock deliberately.
- **Provenance:** official signed binary packages of the Alpine v3.24 repositories, not rebuilt from the vendored aports (building
  `rust` and `gcc` from source takes hours; see Limits). Every locked version equals `pkgver-pkgrel` of its vendored APKBUILD
  (`lock-dev.py` refuses anything else, `check-dev.sh` re-checks), so the layer is exactly what `vendor/alpine-aports` describes. The
  files are cached in `.build/upstream/`, downloaded on first use with the configured proxy and CA bundle, and only kept if the sha256
  of `dev.lock` matches. The install is offline and runs from those files alone (`apk add --force-non-repository`, no repository
  configured); apk verifies every package signature against the Alpine release keys that the base image ships.
- **Image:** the base archive with the layer on top, assembled by `guest/mkdev.sh` in the sandbox. `/etc/apk/world` is
  `alpine-base` + `dev.pkgs`. Output next to the base image in `.build/images/`: `dev-rootfs/`, `<image>.rootfs.tar.gz` (deterministic,
  as above), its `.sha256` and `<image>.manifest` (the base image's sha256, then name, version, origin, apk sha256, datahash of all 58
  packages). `--refresh-lock` re-resolves `dev.pkgs` against the live repositories and rewrites `dev.lock`; review the diff.
- **Using it:** `dev-run.sh` unpacks the image into a throwaway root inside user, mount, PID, IPC, UTS and network namespaces (ids 0-65535
  of the image are mapped onto an unprivileged host range) and runs a command with a fixed environment: `PATH` (system directories
  only), `HOME=/root`, `LANG=C.UTF-8`, `TZ=UTC`, `SOURCE_DATE_EPOCH`, `CARGO_HOME=/root/.cargo`, `CARGO_NET_OFFLINE=true`,
  `CARGO_INCREMENTAL=0`, `CARGO_TERM_COLOR=never`, `TERM=dumb`. No host variable is passed. There is no network, so Cargo needs vendored
  crates (`cargo vendor`, or a project without dependencies).
- **Validation** (`check-dev.sh`): installed set equals `base.lock` + `dev.lock`; every layer version equals the vendored APKBUILD;
  the image is exactly the closure of `alpine-base` + `dev.pkgs`; every locked `.apk` has its pinned sha256 and a valid Alpine
  signature; `apk audit` clean; every path package-owned; the base image is preserved path for path and byte for byte (only the apk
  database and world file differ); all ELF objects are x86-64; no host names, paths, proxy or secret values anywhere; a rebuild from a
  clean state gives the identical archive; then `dev-test.sh` compiles and runs three projects in the image (`dev-test/`):
  C (`make`, `-Wall -Wextra -Werror`), Rust (`cargo build --release --locked`, `cargo test`) and Rust calling C through a `build.rs`.
  It checks the tool versions and target, the ELF type, interpreter and `DT_NEEDED` of the results, that an identical rebuild gives
  identical binaries, and that the environment and the network are as specified above.

## Isolation
- Own mount/PID/IPC/UTS namespaces, chroot into `.build/rootfs`, environment rebuilt with `env -i` (no host variables or tokens;
  only `HTTPS_PROXY`/`NO_PROXY` and the CA bundle are passed when the host uses a proxy). `--offline` adds a network namespace.
- `--ephemeral` (used for every package build and image assembly) runs on a throwaway overlayfs of `.build/rootfs`. abuild installs
  and purges build dependencies, and their triggers leave residue (e.g. `bbsuid` links to a purged package); on the overlay that is
  discarded, so every build starts from the identical toolchain. `check-env.sh` compares `.build/rootfs` with the digest recorded at
  bootstrap (`tree-digest.py`) to prove it did not drift.
- `/aports` is a **read-only** bind mount. Builds run on a copy under `.build/work` (abuild writes `src/` and `pkg/` beside the
  APKBUILD, and upstream's `.gitignore` would hide such files inside `vendor/`). `build-pkg.sh` fails if the vendored tree changes.
- Builds run as the unprivileged `builder` user; the package-signing key is generated per environment under `.build/` and never committed.
- All generated state lives in `.build/` (gitignored).

## Base system image
- **Definition:** the runtime closure of the `alpine-base` meta package: 27 packages from the 16 aports in `guest/base.origins`
  (`musl`, `zlib`, `openssl`, `libcap`, `busybox`, `apk-tools`, `openrc`, `alpine-baselayout`, `alpine-keys`, `alpine-conf`,
  `alpine-base`, `mdev-conf`, `ca-certificates`, `pax-utils`, `ifupdown-ng`, `bridge`). Exact `name=version`: `guest/base.lock`.
  No kernel, bootloader or initramfs: those are machine specific and not part of the base system.
- **Provenance:** every package is built from the vendored aports; the image is installed from the local repository only
  (`--no-network`, upstream repositories disabled) and trusts only the local build key during assembly. The key is not copied into
  the image, which ships with the Alpine release keys from its own `alpine-keys` package and `/etc/apk/repositories` pinned to v3.24.
- **Image configuration** (everything the packages do not provide, in `guest/mkroot.sh`): the repositories file and the standard
  OpenRC runlevel services (upstream's `scripts/genapkovl-dhcp.sh` set minus the ISO-only `modloop`; sysinit: devfs dmesg mdev hwdrivers; boot: hwclock modules sysctl hostname bootmisc syslog; shutdown: mount-ro
  killprocs savecache). `/etc/inittab`, users and everything else come from the packages. As shipped by `alpine-baselayout`, `root` has an **empty password** (as in Alpine's own minirootfs); set or lock it before exposing the system. One wall-clock value written by a package script (the `klogd` day number in `/etc/shadow`) is pinned to the day of `SOURCE_DATE_EPOCH`, and the `apk.log` transcript is dropped, so the image does not depend on when it was assembled.
- **Output** (`.build/images/`): `rootfs/`, `<image>.rootfs.tar.gz` (deterministic: sorted, mtimes clamped to `SOURCE_DATE_EPOCH`,
  numeric owners), its `.sha256`, and `<image>.manifest` (package, version, origin, apk sha256, datahash).
- **Validation** (`check-base.sh`): installed set equals `base.lock` and the vendored APKBUILD versions; every dependency satisfied and
  nothing outside the closure; every path package-owned or documented image configuration; every ELF is x86-64; uid/gid exist in
  the image; no host names, paths, proxy or secret values anywhere; `apk verify` of all packages; archive matches the staged root;
  re-assembly is byte-identical; the image boots (`boot-test.sh`: init runs as PID 1 in namespaces, OpenRC reaches the default
  runlevel, orderly shutdown; a user namespace keeps the booted system from touching the host kernel).

## Build-host limitations handled (`guest/pkg-env.d/`)
This build host's kernel has no IPv6 (`/proc/net/if_inet6` is missing). Two testsuites need it, so build-pkg.sh sources a documented
per-package env file (the APKBUILD itself is never edited):
- `openssl`: `TESTS=-test_bio_dgram` excludes the one test that opens AF_INET6 sockets; the other 344 test files run.
- `alpine-conf`: `ABUILD_BOOTSTRAP=1` skips its kyua suite (three `setup_interfaces_*` cases expect `inet6` stanzas and cannot pass here;
  the other 282 cases passed). kyua cannot exclude single cases.
Remove these files on a host with IPv6.

## Limits
- Alpine retires superseded package versions, so a later `bootstrap.sh --force` may fail to install the locked versions.
  Refresh deliberately with `bootstrap.sh --force --refresh-lock` and review the diff.
- The v3.24 repository keeps moving after the 3.24.2 tag (e.g. `openssl` 3.5.9 there, 3.5.8 in the vendored aports). The image uses
  the vendored versions. Build-time headers and tools (`openssl-dev`, `libcap-dev`, ...) come from the locked toolchain, not from
  the freshly built packages.
- Reproducibility: within one environment the `.apk` files and the image archive are byte-identical when rebuilt. Across
  independent environments (fresh clone, fresh bootstrap, full rebuild; checked) all 27 package payload hashes (`datahash`) and
  the whole image filesystem are identical, except the `S:` lines of `/lib/apk/db/installed`: each `.apk` is signed with the
  environment's own key, and deflate-compressed signature bytes differ in size by a few bytes, so the recorded package sizes
  (and with them the archive checksum) differ. Sharing one signing key would remove that, but would mean committing a private key.
- The development layer is installed from Alpine's binary packages, so rustc, cargo, gcc and binutils are the official builds, not
  compiled from the vendored aports here (the `.apk` bytes are pinned by sha256 and their signatures are verified). Alpine retires
  superseded versions: if a locked file is no longer on the mirror and not in `.build/upstream/`, `make-dev.sh` stops and says so;
  refresh with `make-dev.sh --refresh-lock`. The live repository is ahead of the vendored aports (e.g. `nghttp2-libs`), and the layer
  follows the vendored versions. The development image is byte-reproducible from one base image; a different base build differs only
  in the `S:` lines described above.
- The image boot test exercises everything above the kernel (init, OpenRC, services, shutdown); no real kernel or bootloader is run.
