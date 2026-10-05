# Alpine build environment (v3.24.2)

Reproducible, isolated environment for building Alpine packages from the vendored
`vendor/alpine-aports` tree (see `vendor/alpine-aports.PROVENANCE.md`), and the minimal Alpine base
system image built with it, extended by a Rust and C development layer and a tools layer (debugging, tracing, profiling,
benchmarking, pressure, filesystem and disk-image, QEMU, libvirt and Docker tools). Linux, root, `x86_64`.

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

build/alpine/make-tools.sh             # add the tools layer: .build/images/alpine-tools-3.24.2-x86_64.rootfs.tar.gz
build/alpine/check-tools.sh            # validate the tools image, rebuild it from clean, then run every tool (tools-test.sh)
build/alpine/tools-test.sh             # only the functional run: PASS / LIMIT / FAIL per tool, exit 1 on FAIL
build/alpine/tools-run.sh [--privileged] [--copy SRC:DST] [--out DIR:HOSTDIR] -- cmd   # run a command in a throwaway copy of the tools image
```
`make-dev.sh` and `make-tools.sh` (and the two `check-*.sh`) are one mechanism, `make-layer.sh` and `check-layer.sh`, with the layer
name as first argument (`dev` or `tools`).

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
  (`lock-layer.py` refuses anything else, `check-layer.sh` re-checks), so the layer is exactly what `vendor/alpine-aports` describes. The
  files are cached in `.build/upstream/`, downloaded on first use with the configured proxy and CA bundle, and only kept if the sha256
  of `dev.lock` matches. The install is offline and runs from those files alone (`apk add --force-non-repository`, no repository
  configured); apk verifies every package signature against the Alpine release keys that the base image ships.
- **Image:** the base archive with the layer on top, assembled by `guest/mklayer.sh` in the sandbox. `/etc/apk/world` is
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
  database, the world file and that one applet link differ); all ELF objects are x86-64; no host names, paths, proxy or secret values anywhere; a rebuild from a
  clean state gives the identical archive; then `dev-test.sh` compiles and runs three projects in the image (`dev-test/`):
  C (`make`, `-Wall -Wextra -Werror`), Rust (`cargo build --release --locked`, `cargo test`) and Rust calling C through a `build.rs`.
  It checks the tool versions and target, the ELF type, interpreter and `DT_NEEDED` of the results, that an identical rebuild gives
  identical binaries, and that the environment and the network are as specified above.
  C binaries need only `libc.musl`; Rust binaries also need `libgcc_s.so.1` (unwinding), which `libgcc` in the layer provides. The layer
  replaces one base file: busybox's unowned applet link `usr/bin/strings` becomes binutils' own `strings`.

## Tools layer (debug, trace, profile, benchmark, pressure, filesystems, VMs, containers)
`make-tools.sh` extends the development image (not the base: the dev layer is its parent, unchanged and still byte-identical) with the
rest of the toolchain, using the same mechanism as the dev layer (`make-layer.sh`).
- **Definition:** `guest/tools.pkgs` names 68 packages; `guest/tools.lock` pins the 220 packages they add to the 58 of the dev image
  (278 in all), each with exact version, origin aport and the sha256 of its `.apk`. Packages the dev or base layer already provides
  (`gcc`, `rust`, `cargo`, `make`, `musl`, `zlib`, ...) are not listed again. Nothing is duplicated: `ninja` comes from `samurai`,
  `losetup`, `mount`, `sfdisk`, ... from the util-linux split packages, not from busybox (`/usr/bin` wins in `PATH`).
- **Contents** (versions are those locked):
  - *compile, link, format, lint, test:* g++ and libstdc++-dev (gcc 15.2), clang/clang++ 22.1.3 with `compiler-rt` (sanitizers),
    `clang-tidy` and `clang-format` (`clang22-extra-tools`), cppcheck 2.21, cmake 4.2.3 with samurai, pkgconf, linux-headers,
    `rustfmt` and `clippy` 1.96.1, `cargo-nextest` 0.9.110, Google Benchmark 1.9.5.
  - *debug and trace:* gdb 16.3 (with `rust-gdb`), valgrind 3.25.1, strace 6.19, ltrace 0.7.3, `perf` and `bpftool` 7.1.5, tcpdump 4.99.6, lsof;
    ftrace and eBPF come with the kernel (tracefs, `bpf()`), driven from the tools above.
  - *profile and benchmark:* `perf stat/record/report/trace/bench`, `cargo-flamegraph`, valgrind, hyperfine 1.20, sysbench 1.0.20,
    fio 3.41, Google Benchmark.
  - *pressure:* stress-ng 0.21 (CPU, memory, process, thread, file descriptor, I/O), fio and sysbench (I/O, memory), iperf3 3.20 (network),
    iproute2 and nftables/iptables (network namespaces, shaping, firewalling), cgroup limits through the kernel interface.
  - *filesystems and images:* e2fsprogs (+ `fuse2fs`), dosfstools, mtools, squashfs-tools, erofs-utils, xfsprogs, btrfs-progs, xorriso,
    qemu-img, util-linux (`losetup`, `partx`, `sfdisk`, `lsblk`, `findmnt`, `wipefs`, `blkid`, `mount`), gptfdisk.
  - *VMs:* QEMU 11.0.3 (`qemu-system-x86_64`, TCG), SeaBIOS, OVMF, libvirt 12.3 (`libvirtd`, `virsh`, QEMU driver).
  - *containers:* Docker engine and CLI 29.8.2 with compose and buildx, containerd 2.3.6, runc 1.4.3.
- **Provenance:** as for the dev layer. Official signed binary packages of v3.24 main and community, each locked version equal to the
  vendored APKBUILD `pkgver-rN` of its origin aport, checked by `lock-layer.py` and `check-image.py`. Where the live index is ahead
  of the vendored aport (libexpat, pcre2, libpng, xen-libs) the vendored version is locked. The mirror has retired three
  vendored versions (HTTP 404: `python3-pycache-pyc0` 3.14.7-r1, `containerd` 2.3.5-r5, `docker-cli`/`docker-openrc` 29.5.3-r1), so
  `guest/tools.exceptions` records, per origin aport, the next version that is locked instead (python3 3.14.8-r0, containerd 2.3.6-r0,
  docker 29.8.2-r0). An exception applies only while the aport and the live index are exactly at the recorded versions; `check-image.py`
  fails on a stale or unused one, and the lock header repeats each exception.
- **Image:** `alpine-tools-3.24.2-x86_64.rootfs.tar.gz` (+ `.sha256`, `.manifest`), deterministic like the others. `/etc/apk/world`
  is the dev world plus `tools.pkgs`. `--refresh-lock` re-resolves `tools.pkgs`; review the diff.
- **Using it:** `tools-run.sh -- cmd` is `dev-run.sh` on the tools image: unprivileged user namespace, no network, private `/dev`
  (null, zero, full, random, urandom, tty) and `/tmp`. That is enough for everything that does not need host privileges: compile, lint,
  debug (ptrace works), profile (software events), benchmark, pressure, image building (mkfs on files, `mtools`, `mksquashfs`,
  `mkfs.erofs`), QEMU emulation. `tools-run.sh --privileged -- cmd` runs as the real root of the host
  in private mount, PID, IPC, UTS (`alpine-tools`), network (loopback only), and cgroup namespaces. The image is the `pivot_root`ed root of
  the command's mount namespace (so `runc exec` and `nsenter` land in the image), `/dev`, sysfs, tracefs and the cgroup hierarchies are
  the host's, `/tmp` and `/run` are tmpfs. This is what loop devices and kernel mounts, FUSE, ftrace and perf tracepoints, eBPF,
  network namespaces, firewalls, cgroup limits, libvirt and Docker need. On exit the runner kills every process, detaches loop devices
  and removes cgroup directories created during the run, and deletes the extracted root. Run libvirt and Docker only this way:
  they must never see the host's network namespace.
- **Validation** (`check-tools.sh`): everything `check-dev.sh` checks, for the tools image (the installed set equals `base.lock` +
  `dev.lock` + `tools.lock`, `/etc/apk/world` equals the roots, versions equal the vendored APKBUILDs or a documented exception, `.apk`
  sha256 and signatures, `apk audit`, all ELF objects x86-64, setuid files equal an allowlist, no host names, paths or secret values
  anywhere, the parent image preserved); a rebuild from a clean state gives the identical archive; then `tools-test.sh`.
  - The tools layer's packages add system users (`adduser` stamps today's day number into `/etc/shadow`); `guest/mklayer.sh` pins
    that field to the day of `SOURCE_DATE_EPOCH`, as `mkroot.sh` does, so the image does not depend on the day it was assembled
    (the check searches every file for today's day number and date). The parent check accepts what packages legitimately do to the
    base: busybox applet links replaced by the real tool (`mount`, `ip`, `linux32` -> `setarch`, ...), users and groups appended to
    `/etc/passwd`, `/etc/group`, `/etc/shadow` and their `-` copies, and `/bin/bash` appended to `/etc/shells` (every parent line
    still present and in order; the only edit to an existing line is a group gaining members, as `qemu` joins `kvm`).
  - Exemptions are few, named in `check-layer.sh`, only for package-owned paths, and fail when they stop applying (stale entry):
    `usr/share/seabios/bios-coreboot.bin` is 32-bit x86 firmware that QEMU loads as a BIOS, not an image executable;
    `usr/bin/c-index-test` and `usr/bin/clang-offload-packager` are links shipped by the clang packages into `../lib/llvm22/bin/`, which
    no package provides (`c-index-test`) or only the uninstalled `llvm22` does (`clang-offload-packager`); five upstream files
    (`FindDoxygen.cmake` and two CMake help pages, `docker-buildx`, `gdb`) contain the literal example path `/home/user`, which is
    otherwise a needle for host state (only that host-path needle, never a secret, host name or date). The `etc/ssl/certs`
    links made by the ca-certificates trigger (`ca-cert-NAME.pem` and OpenSSL hash links) are checked against the enabled lines of
    `/etc/ca-certificates.conf` (every link present, every target packaged, every certificate hashed) rather than excused.
- **Functional test** (`tools-test.sh`): `tools-test/unpriv.sh` (unprivileged session) and `tools-test/priv.sh` (`--privileged`
  session) run every tool of `tools.pkgs` on fixtures in `tools-test/` (C, C++, Rust, a bare-metal boot sector, a container context),
  and print `fact=value` lines. `tools-judge.py` judges each fact against `tools-test/inventory.tsv` (one row per fact: packages
  exercised, expectation, description) and prints PASS, LIMIT or FAIL. It also fails when a fact has no row, a row no fact, or a
  package of `tools.pkgs` no row. After both sessions the host is compared with its state before (sandboxes, mounts, loop devices,
  cgroup directories, nftables tables, daemons). Exit status 1 only for FAIL.
  - Compile, link and run C, C++ (exceptions, STL) and Rust with gcc and clang, cmake + samurai, ctest, cargo build/test/nextest/clippy/fmt,
    clang-tidy, clang-format, cppcheck, sanitizers, Google Benchmark, a boot sector assembled and linked.
  - Debug with gdb (breakpoints and backtraces, Rust through `rust-gdb`), valgrind (leak detection), strace, ltrace, the gcc and clang address and
    undefined-behaviour sanitizers; profile with `perf` (software events, tracepoints), `cargo flamegraph`, hyperfine, fio, sysbench.
  - Pressure: stress-ng (cpu, vm, fork, pthread, open files, hdd, sockets), also under a cgroup memory limit; cgroup CPU quota, memory limit
    with OOM kill and pids limit; file-descriptor and memory exhaustion under `ulimit`; fio (psync, libaio, io_uring) and sysbench I/O; iperf3
    TCP and UDP across a veth pair into a network namespace; nftables and iptables rules that block and unblock traffic; `tc` shaping.
  - Images: ext2/ext4, FAT, XFS, btrfs, squashfs, erofs and ISO images built (byte-identical when rebuilt), checked, read back and, where the
    kernel allows, mounted (loop, FUSE, resize); qcow2, vmdk and vpc conversion, overlays and snapshots; GPT and MBR partition tables
    built and inspected; `wipefs`, `blkid`, `partx`.
  - VMs: QEMU TCG boots a hand-made boot sector (serial banner, power-off through `isa-debug-exit`) and runs OVMF up to the boot manager;
    libvirt defines a storage pool, a volume and a TCG domain, starts it, reads its serial output, destroys and undefines it, also
    on a host bridge with a tap device; `virt-host-validate`.
  - Containers: `docker build` and `buildx build` of a static C program on `FROM scratch` (`tools-test/ctr/`), save, load, import, run,
    with memory, pids and CPU limits, a non-root user, `no-new-privileges`, a read-only root, capabilities dropped, `--network none`, a
    user-defined bridge network with two containers talking to each other, `docker exec`, `docker compose up`; `runc run` of an OCI bundle.
  All tests are offline: images are built `FROM scratch`, nothing is pulled.

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
- **What the cloud host does not provide** (tested by `tools-test.sh` and reported as LIMIT, never as PASS; the tools themselves are
  installed and work as far as the host lets them):
  - no `/dev/kvm`: QEMU runs with TCG only (`-accel kvm` fails: "failed to initialize kvm: No such file or directory"), libvirt domains must use
    `<domain type='qemu'>`, `virsh domcapabilities --virttype kvm` fails and `virt-host-validate` reports FAIL for hardware
    virtualization; no `/dev/vhost-net`;
  - no loadable kernel modules (`/lib/modules` is absent): no `dummy` link type, no `netem` qdisc, no `act_csum` tc action, so libvirt
    virtual networks (NAT, isolated) cannot start; VM networking is tested through a host bridge and tap device instead;
  - the guest kernel has no XFS, btrfs or vfat, so those images are created, checked and read with their userspace tools (`mkfs.*`,
    `xfs_repair`, `btrfs check`, `mtools`), not mounted; ext2/3/4, squashfs, erofs, overlay and FUSE mount;
  - no GPT or MSDOS partition parser: `losetup -P` creates no `loopNpM` nodes; `partx -a` does;
  - no hardware PMU (`perf stat -e cycles` is "not supported"; software events, tracepoints and `perf trace` work), no `/proc/schedstat`;
  - ftrace's function tracer cannot be enabled (EPERM; event tracing and eBPF work);
  - the hard `RLIMIT_NOFILE` cannot be raised (no `CAP_SYS_RESOURCE`);
  - cgroups are hybrid (v1 controllers plus a cgroup2 mount): Docker runs on cgroup v1 and warns about its deprecation.
- `ltrace -e <symbol>` across all libraries aborts on musl; use `-e <symbol>@MAIN`. The official `musl-dbg` does not match the locally built
  `musl` of the base, so it is not installed (no symbols for libc frames in gdb, valgrind and perf).
- Two links of the official clang packages are dangling upstream, so `c-index-test` and `clang-offload-packager` do not run (the
  rest of clang, clang-tidy and clang-format work); they are the only dangling links in the image and are named in `check-layer.sh`.
- Docker, libvirt, loop mounts, FUSE, ftrace, eBPF and network namespaces need `tools-run.sh --privileged`; in the unprivileged session
  `/dev/fuse`, `/dev/kvm` and `/dev/net/tun` are absent, `lo` is down and there is no network. Everything runs offline: registries are
  not reached and no image is pulled.
- The tools layer is installed from Alpine's binary packages, locked by version and sha256. The mirror retires superseded versions: on a
  fresh checkout the locked `.apk` files must still be served by the mirror or be present in `.build/upstream/` (three vendored versions
  are already gone and recorded in `guest/tools.exceptions`). `make-tools.sh --refresh-lock` re-resolves them; review the diff.
