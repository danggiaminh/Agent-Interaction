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
build/alpine/check-tools.sh            # validate the tools image, rebuild it from clean, rebuild the cgroup v2 test bed, then run every tool
build/alpine/tools-test.sh             # only the functional run (3 sessions): PASS / DENIED / LIMIT / UPSTREAM / FAIL per fact, exit 1 on FAIL
build/alpine/tools-judge-selftest.py UNPRIV.facts PRIV.facts GUEST.facts   # prove the judge fails on damaged facts (tools-test.sh runs it)
build/alpine/make-guest.sh [--check|--clean]   # build / verify / remove the cgroup v2 test bed (guest kernel + initramfs) in .build/guest
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
- **Definition:** `guest/tools.pkgs` names 69 packages; `guest/tools.lock` pins the 221 packages they add to the 58 of the dev image
  (279 in all), each with exact version, origin aport and the sha256 of its `.apk` (one, `musl-dbg`, is built locally: see
  Provenance). Packages the dev or base layer already provides
  (`gcc`, `rust`, `cargo`, `make`, `musl`, `zlib`, ...) are not listed again. Nothing is duplicated: `ninja` comes from `samurai`,
  `losetup`, `mount`, `sfdisk`, ... from the util-linux split packages, not from busybox (`/usr/bin` wins in `PATH`).
- **Contents** (versions are those locked):
  - *compile, link, format, lint, test:* g++ and libstdc++-dev (gcc 15.2), clang/clang++ 22.1.3 with `compiler-rt` (sanitizers) and
    `llvm22` (the LLVM tools that clang's own links point to), `clang-tidy` and `clang-format` (`clang22-extra-tools`), cppcheck 2.21, cmake 4.2.3 with samurai, pkgconf, linux-headers,
    `rustfmt` and `clippy` 1.96.1, `cargo-nextest` 0.9.110, Google Benchmark 1.9.5.
  - *debug and trace:* gdb 16.3 (with `rust-gdb`), valgrind 3.25.1, strace 6.19, ltrace 0.7.3, `perf` and `bpftool` 7.1.5, tcpdump 4.99.6, lsof,
    `musl-dbg` (debug symbols of the image's own libc; `check-image.py` verifies the build-id and debuglink CRC of every debug file
    against the installed object it describes); ftrace and eBPF come with the kernel (tracefs, `bpf()`), driven from the tools above.
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
  One package is not downloaded but built: `musl-dbg`. The official one carries the symbols of the official `musl`, whose build-id is not
  that of the `musl` this environment builds from the vendored aport for the base image, so gdb, valgrind and perf could not use it
  (and `check-image.py` would fail it). `guest/tools.local` lists such packages (with the reason); the `local` repository type of
  `tools.lock` takes the `.apk` from `.build/packages/` (built by `build-base.sh`) and pins it by the `datahash` of its payload, which
  is the same in every environment, instead of by the `.apk` sha256, which carries this environment's signature.
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
  anywhere, the parent image preserved, every debug file matches the object it describes); a rebuild from a clean state gives the
  identical archive; the cgroup v2 test bed is removed, rebuilt from its pins and checked (`make-guest.sh`); then `tools-test.sh`.
  - The tools layer's packages add system users (`adduser` stamps today's day number into `/etc/shadow`); `guest/mklayer.sh` pins
    that field to the day of `SOURCE_DATE_EPOCH`, as `mkroot.sh` does, so the image does not depend on the day it was assembled
    (the check searches every file for today's day number and date). The parent check accepts what packages legitimately do to the
    base: busybox applet links replaced by the real tool (`mount`, `ip`, `linux32` -> `setarch`, ...), users and groups appended to
    `/etc/passwd`, `/etc/group`, `/etc/shadow` and their `-` copies, and `/bin/bash` appended to `/etc/shells` (every parent line
    still present and in order; the only edit to an existing line is a group gaining members, as `qemu` joins `kvm`).
  - Exemptions are few, named in `check-layer.sh`, only for package-owned paths, and fail when they stop applying (stale entry):
    `usr/share/seabios/bios-coreboot.bin` is 32-bit x86 firmware that QEMU loads as a BIOS, not an image executable;
    `usr/bin/c-index-test` is a link shipped by the clang packages into `../lib/llvm22/bin/`, where no Alpine package provides it
    (`clang-offload-packager`, the other such link, resolves because `llvm22` is installed); five upstream files
    (`FindDoxygen.cmake` and two CMake help pages, `docker-buildx`, `gdb`) contain the literal example path `/home/user`, which is
    otherwise a needle for host state (only that host-path needle, never a secret, host name or date). The `etc/ssl/certs`
    links made by the ca-certificates trigger (`ca-cert-NAME.pem` and OpenSSL hash links) are checked against the enabled lines of
    `/etc/ca-certificates.conf` (every link present, every target packaged, every certificate hashed) rather than excused.
- **Functional test** (`tools-test.sh`): three sessions run every tool of `tools.pkgs` on fixtures in `tools-test/` (C, C++, Rust, a
  bare-metal boot sector, a container context) and print `fact=value` lines: `tools-test/unpriv.sh` (unprivileged session: user
  namespace, uid 0 is not host root), `tools-test/priv.sh` (`--privileged`: real root of the host) and `tools-test/guest.sh` (inside the
  cgroup v2 test bed guest, see below). `tools-judge.py` judges every fact against `tools-test/inventory.tsv` (568 rows: one row per fact
  with the packages it exercises, an expectation and a description). Expectations are `ok`, `=VALUE`, `ver:PKG`, `~REGEX` or
  `deny:REGEX`. Every row ends in exactly one class:
  - **PASS**: the tool did what the row expects;
  - **DENIED**: a `deny:` row of the unprivileged session: the operation that needs real privileges (loop devices, block-file-system
    and cgroup2/tracefs mounts, `bpf()` maps, block device nodes, setting the clock, raising the hard file limit, FUSE) was refused with
    the expected error. It is the evidence that a privileged-only capability is *not* available to an unprivileged workload; if it
    ever works there, the row FAILs ("leaked"). A refusal is never reported as the capability working;
  - **LIMIT**: the row failed because the host cannot do it. Only `tools-test/limits.tsv` (35 entries) can make a failed row a LIMIT, and
    only if all three hold: the failure *message* matches the entry's `accepts` pattern (the specific error of that limitation, never
    "anything"), the entry's `cause` holds on the facts measured in that run (`host_config`, `host_cpu_virt_flags`, `host_pmu`,
    `cgroup_v2_controllers`, `host_cap_sys_resource`, ...: e.g. no KVM only while the CPU shows no vmx/svm *and* the kernel has neither
    `KVM_INTEL` nor `KVM_AMD`), and the rows named in `proof` PASS (the tool works wherever the cause does not apply, e.g. the same
    controller in the guest). A limit has a class (`host-hw`, `host-kernel`, `host-config`) and says what the host would have to provide;
  - **UPSTREAM**: a known defect of the packaged tool itself (`ltrace -e <symbol>` on musl), accepted only with its exact message;
  - **FAIL**: everything else: a failed tool, a missing fact, a fact without a row, a row without a fact, a package of `tools.pkgs`
    without a row, a failure whose message or cause does not match, a `deny:` row that worked. Exit status 1 only for FAIL.

  Host-kernel limits therefore stay apart from toolchain defects: the former can only be LIMIT with a measured cause and a passing proof
  row, the latter are FAIL or UPSTREAM. Nothing about a limit is assumed: a limit entry whose cause disappears (a host with KVM, a pure
  cgroup v2 host) turns the same failure into FAIL.
  - `tools-test/capabilities.tsv` (55 capabilities in eight areas) says what each capability needs (`none`: any workload, `root`: only
    real root, `host`: the host kernel or hardware) and which row proves it in each privilege tier (`unpriv`, `priv`, `guest`). The judge
    prints the matrix and fails if a `none` capability is not PASS or LIMIT unprivileged, a `root` capability is not DENIED unprivileged,
    a `host` capability is neither PASS nor LIMIT for root, or a `deny:` row is not referenced by any capability. Its **verdict** section counts, per area, what is testable natively
    on this host, what only in the guest, and what not at all.
  - `tools-judge-selftest.py` runs after the judge: 18 cases damage a copy of the facts the way a broken toolchain or a dishonest report
    would (a leaked privilege, a limit reported for a host that has the feature, a broken proof row, a wrong message, a missing fact, a
    pure-v2 host) and require the judge to FAIL naming the row (or, for the two pure-v2 cases, to accept). A judge that cannot fail fails
    the run.
  - After the sessions the host is compared with its state before (sandboxes, mounts, loop devices, cgroup directories, nftables tables,
    daemons).
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

## Cgroup v2
Agent-Interaction's resource management targets cgroup v2. The cloud host (Firecracker VM, kernel 6.18, 4 CPUs) cannot test it
natively, and the tests say so instead of passing around it.
- **Measured layout: hybrid.** The v1 hierarchies of blkio, cpu, cpuacct, cpuset, devices, freezer, memory, pids and `name=systemd`
  are mounted, and the unified hierarchy (`/sys/fs/cgroup/unified`) offers only `hugetlb`. A controller bound to a v1 hierarchy cannot be
  used by v2 (enabling `memory`, `pids`, `cpu`, `cpuset` or `io` in `cgroup.subtree_control` fails with ENOENT), and the harness of
  the session itself accounts memory and CPU through v1, so v1 must not be unmounted or re-mounted. `tools-run.sh --privileged` adapts: it
  mounts each hierarchy it finds (v1 controllers, the unified one) and `cgroup_mode` (`v2`, `hybrid`, `v1`) is a measured fact.
- **What works natively** (`priv.sh`, as real root, verified by effect, not by exit status): on cgroup v2 `cgroup.freeze`, `cgroup.kill` and
  PSI; on cgroup v1 the memory limit with an OOM kill (exit 137, `oom_kill` counted), `pids.max` (fork refused at the limit), CPU quota
  (50 % ratio measured 0.51) and CPU shares (3.96 for a 4:1 weight), cpuset pinning and blkio throttling (4.0 s against 0.01 s unthrottled);
  Docker (cgroup v1, cgroupfs driver) with `--memory` (kill, 137), `--pids-limit` (refused at the limit) and `--cpus` (0.50); the
  user, PID, mount, UTS, IPC and network namespaces (also unprivileged), veth pairs and nftables.
- **What does not** (`LIMIT`, `host-config`): the cgroup v2 memory, pids, cpu quota, cpu weight, cpuset and io controllers. Their
  `cgv2_*` rows fail with "is bound to cgroup v1 hierarchy"; the judge accepts that only while `cgroup_v2_controllers` really lacks the
  controller *and* the same row passes in the guest. A host with a unified hierarchy turns the failure back into FAIL, and its `cgv1_*`
  rows into LIMIT (no v1 hierarchy), as `tools-judge-selftest.py` proves on a synthetic pure-v2 host.
- **Unprivileged**: uid 0 in a user namespace cannot mount cgroup2, tracefs, block file systems, create loop devices, `mknod` a
  block device, create BPF maps, set the clock, raise the hard file limit or mount FUSE; every one is a `deny:` row (DENIED) tied to a
  capability, and the namespaces, veth, nftables and `RLIMIT_CPU` work there. CPU, memory, process, I/O and network isolation are
  therefore all testable *with* privileges (cgroup v1 natively, cgroup v2 in the guest) and namespace-based process and network
  isolation also *without*.
- **Is the cloud environment sufficient for cgroup v2 isolation development? No, not by itself.** It is sufficient for cgroup v1
  work, namespaces, v2 freeze/kill/PSI and, through the guest, for *functional* validation of every cgroup v2 controller (the guest runs
  on a pure v2 hierarchy: memory OOM kill, pids, cpu quota and weight, cpuset, io throttling, freeze, kill and PSI all pass). It is
  not sufficient for native cgroup v2 validation or for timing-accurate behaviour. The host environment would have to provide:
  1. a unified cgroup v2 hierarchy with `memory pids cpu cpuset io` delegated (boot with `cgroup_no_v1=all`, or a cgroup2-only
     init such as systemd with `systemd.unified_cgroup_hierarchy=1` and no v1 controllers);
  2. `CAP_SYS_RESOURCE` in the session where the hard open-file limit matters;
  3. `/dev/kvm` (nested virtualization: `vmx` or `svm` exposed to the VM and `KVM_INTEL` or `KVM_AMD` in its kernel) so the guest runs
     hardware-accelerated and with a virtual PMU, which gives timing-accurate and hardware-counter results.
  Until then cgroup v2 results from this environment are labelled guest-only, and the verdict section of the judge says so.

## Cgroup v2 test bed
`make-guest.sh` builds a small guest in `.build/guest/bed/` (`vmlinuz`, `initramfs.cpio.gz`, `manifest`) that `tools-test.sh` boots under
QEMU from the tools image (`testbed/launch.sh`, unprivileged, offline) with `cgroup_no_v1=all psi=1`, so the guest sees a pure cgroup v2
hierarchy with `cpu cpuset dmem hugetlb io memory pids` on a real kernel. `tools-test/guest.sh` runs there: every cgroup v2 controller,
namespaces, veth, nftables, netem, dummy links, loop devices with ext4, xfs, btrfs and vfat, partition scanning, ftrace (function tracer
included), perf software events and tracepoints, schedstat, BPF programs and maps, kernel modules, the system clock and file-descriptor
limits; the facts come back as `g_*` and are judged like every other row.
- **Kernel:** the official, signed Alpine `linux-lts` package, pinned by sha256 in `guest/kernel.lock` and checked for the options it must
  have (`guest/kernel.config`); `guest/kernel.modules` lists the modules the initramfs loads. `linux-lts` and not `linux-virt`, because the
  virt flavour has no function tracer. Its version (6.18.55-r0) differs from the vendored aport (6.18.52-r0, retired from the mirror):
  `guest/kernel.exceptions` records that, and `make-guest.sh` fails on a stale or unused exception. The kernel is the guest of the tests
  only; it is never installed into the base, dev or tools image, which stay kernel-less.
- **Initramfs:** busybox and musl of the tools image plus `testbed/init`, assembled deterministically by `testbed/mkinitramfs.py`; the
  bed manifest records the sha256 of the tools image, so `make-guest.sh --check` fails when the bed is stale. `check-tools.sh` removes
  the bed, rebuilds it and checks it before running the tests.
- **Acceleration is measured, not assumed.** QEMU uses KVM only if `/dev/kvm` can be opened read-write in the session, and TCG
  (software emulation) otherwise; which one ran is read from QEMU's own answer over QMP (`guest_accel`). Under TCG the results are
  functional and deterministic, never timing-accurate, and no PMU exists (`g_perf_hw_cycles` is a `host-hw` LIMIT whose cause is
  `guest_accel=tcg`). On the cloud host the guest runs under TCG: a boot and the guest tests take about two minutes.
- **QEMU and libvirt, software against hardware:** `qemu-system-x86_64 -accel tcg` boots (`qemu_tcg_serial`), `-accel kvm` fails with
  "failed to initialize kvm: No such file or directory" (LIMIT `host-hw`, also as real root); libvirt starts a `<domain type='qemu'>`
  and reads its serial output (PASS), `virsh domcapabilities --virttype kvm`, a `<domain type='kvm'>` and `virt-host-validate`'s hardware
  check fail (LIMIT), while `virt-host-validate`'s cgroup checks pass. None of the KVM rows is ever reported as working here.

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
- The image boot test exercises everything above the kernel (init, OpenRC, services, shutdown); no real kernel or bootloader is run
  for the images. The cgroup v2 test bed boots a real kernel, but only as the guest of the tools tests.
- **Hard limits of the cloud host** (not toolchain defects). Each is recorded in `tools-test/limits.tsv` with the failure message it
  produces, the measured cause that must hold, the row that proves the tool works elsewhere, and what the host would have to provide.
  `tools-test.sh` reports them as LIMIT, never as PASS, and a limit whose cause is gone (a host that has the feature) is a FAIL. The tools
  themselves are installed and work as far as the host lets them; the guest of the test bed proves most of them (software emulation
  only: functional, not timing-accurate).
  - *hardware / hypervisor* (`host-hw`): the host is itself a VM (`hypervisor` flag, no `vmx`/`svm`) and its kernel has neither
    `KVM_INTEL` nor `KVM_AMD`, so there is no `/dev/kvm`. `-accel kvm` fails ("failed to initialize kvm: No such file or directory", also
    for real root), libvirt reports no KVM domain capabilities and cannot start a `<domain type='kvm'>` (domains use
    `<domain type='qemu'>`), and `virt-host-validate` fails its hardware check while its cgroup checks pass. The guest runs under TCG, which
    has no PMU. There is no hardware PMU on the host either: `perf stat -e cycles` is "not supported" (software events, tracepoints and
    `perf trace` work). Needed: nested virtualization (`vmx`/`svm` exposed, KVM in the host kernel, `/dev/kvm`) and a virtual PMU.
  - *kernel build or policy* (`host-kernel`): no loadable modules (`CONFIG_MODULES` unset, no `/lib/modules`), no `dummy` link type, no `netem`
    qdisc, no `act_csum` tc action (libvirt's NAT and isolated networks cannot start; VM networking is tested through a host bridge and a
    tap device), no `vhost-net`, no `/proc/schedstat`, no XFS, btrfs or vfat file system (those images are created, checked and read with
    their userspace tools, not mounted; ext2/3/4, squashfs, erofs, overlay and FUSE mount), no GPT or MSDOS partition parser (`losetup -P`
    creates no `loopNpM` nodes, `partx -a` does). The function tracer and the ftrace filter files answer EPERM to real root although
    tracefs mounts and lists the tracer; the kernel runs with `lockdown=integrity`, which is how other kernels withhold this, but the
    measurement does not prove that it is the cause (event tracing and eBPF work). Needed: a kernel with those options, or the guest.
  - *host configuration* (`host-config`): the cgroup layout is hybrid (see "Cgroup v2"): no cgroup v2 memory, pids, cpu, cpuset or io
    controller; Docker runs on cgroup v1 and warns about its deprecation. The session has no `CAP_SYS_RESOURCE`, so the hard
    `RLIMIT_NOFILE` cannot be raised. Needed: a unified cgroup v2 hierarchy with delegated controllers, `CAP_SYS_RESOURCE`.
  - *upstream*: `ltrace -e <symbol>` across all libraries aborts on musl (use `-e <symbol>@MAIN`). Everything else of ltrace works.
- The official `musl-dbg` does not match the locally built `musl` of the base, so the layer carries the `musl-dbg` built next to that
  `musl` (see Provenance); the symbols of libc frames match the installed libc, checked by build-id.
- One link of the official clang packages is dangling upstream: `usr/bin/c-index-test` points into `../lib/llvm22/bin/`, where no Alpine
  package provides it. It does not run (the rest of clang, clang-tidy and clang-format work); it is the only dangling link in the image and is
  named in `check-layer.sh`, which fails if it stops being dangling or another link dangles.
- Docker, libvirt, loop mounts, FUSE, ftrace, eBPF and network namespaces need `tools-run.sh --privileged`; in the unprivileged session
  `/dev/fuse`, `/dev/kvm` and `/dev/net/tun` are absent, `lo` is down and there is no network. Everything runs offline: registries are
  not reached and no image is pulled.
- The tools layer is installed from Alpine's binary packages, locked by version and sha256. The mirror retires superseded versions: on a
  fresh checkout the locked `.apk` files must still be served by the mirror or be present in `.build/upstream/` (three vendored versions
  are already gone and recorded in `guest/tools.exceptions`). `make-tools.sh --refresh-lock` re-resolves them; review the diff.
