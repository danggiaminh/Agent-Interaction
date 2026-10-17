# Development tree

The working plan of Agent-Interaction: what exists, how well it is proven on the revision that carries this file, and
what comes next. It is not a roadmap and not a target state. A branch changes state only on evidence (a check, a row of a
judge, a measurement), and the evidence is named here. The details stay where they belong: `build/alpine/README.md` (the
Alpine environment and its checks), `runtime/resctl/README.md` (resource control: design, commands, limits) and the
tables of the judges. This file is the index of their state and the order of the work.

States: **verified** (a check proves it on this revision) · **partly verified** (the unproven part is named) ·
**open** (known work, not done) · **blocked** (the blocker is named) · **not investigated** · **not applicable** (why).

## Components

| path | what | owner |
|---|---|---|
| `vendor/alpine-aports` | Alpine v3.24.2 aports at `d9d560d5`, see `vendor/alpine-aports.PROVENANCE.md` | vendor: never edited; every check proves it unchanged |
| `build/alpine` | reproducible base, dev and tools images, the cgroup v2 test bed (QEMU), the checks and their judges | project |
| `runtime/resctl` | resource domains, process lifecycle and critical-process protection (Rust, no `unsafe`, no dependency) | project |

## Tree

### Development workflow
- **verified**: the full validation of resource control is `build/alpine/check-resctl.sh`: unprivileged, privileged and guest
  sessions, the judge, its self-test, then the host and the vendored tree compared with their state before. At this
  revision: 388 PASS, 6 DENIED, 6 LIMIT, 0 FAIL, self-test 103 of 103, host (sandboxes, their mounts and processes, loop
  devices, cgroup directories, resctl domains, nftables, daemons) and vendored tree unchanged, in 13.6 minutes (sessions
  of 84, 261 and 463 s; every session prints its duration).
- **verified**: targeted runs. `--areas LIST` runs only those sections of the suite (and `layout`, which measures the causes
  of the host's limits), `--no-guest` skips the test bed, and the judge's `--partial` mode judges whatever ran as strictly as
  a full run, never proves a limit without its guest proof and never passes for a full verdict (7 self-test cases).
  Measured: the process and critical sections on the host in 4.4 minutes; the stress section with the guest in 5.7
  minutes (sessions of 64, 104 and 176 s).
- **verified**: `check-resctl.sh` is one compound command, read whole before it runs: an edit of the script during a run had
  made bash execute a fragment of the new text and abort that run (exit 127).
- **partly verified**: negative controls live in the repository. `resctl-mutants.py` holds thirteen deliberate defects
  (twelve of resctl, one of the suite itself, written `suite:PATH`) and the rows each must break; `check-resctl.sh
  --mutant NAME` builds the defect into a copy of the crate or of the suite, runs its areas on the host (and in the guest
  when only the guest can show it) and passes only if the judge fails those rows. In the last series (54 minutes) all
  eleven defects of that revision were caught and no run left anything on the host; the nine defects of resctl's logic
  fail its unit tests as well, the slow `state` and the blind observer only the suite. **Not yet run**: the two defects
  of the OOM attribution (`oom-blame-workload`, `kmsg-user-records`), and the series on this revision.
- **verified**: bounded test execution. Every crash-loop supervisor of the suite runs under a time limit, the unit tests
  under 900 s, and the fake host of the supervisor tests fails a test that starts 1000 processes. Found by the negative
  controls: the unbounded-restart defect held `cargo test` for 42 minutes, and earlier the suite itself.
- **open**: the unprivileged session compiles the crate from scratch on every run (55 to 128 s, three profiles). A target
  directory kept between runs needs a key per copy of the crate: two copies at different paths that shared one made cargo
  test the first copy's binaries.
- **verified**: every session has a deadline. `dev-run.sh` and `tools-run.sh --deadline S` stop the command from inside the
  sandbox (SIGTERM, SIGKILL 10 s later, exit 124 and a `deadline:` line), then do the cleanup of any run: thaw the
  cgroups the run created (a frozen process ignores SIGKILL and kept the PID namespace from dying), kill what is left,
  detach its loop devices, remove its cgroup directories. `check-resctl.sh` gives its sessions 1200, 2400 and 2100 s,
  `tools-test.sh` 2400, 2400 and 1500 s; a stopped session is a FAIL of its own, and the host is compared with its state
  before after every run, also one that stopped early. `check-deadline.sh` (29 checks, 61 s): a positive control of the
  host comparison (one planted leftover of each kind), early exits that keep their status (124 included), a hung
  unprivileged and a hung privileged command (SIGTERM ignored, a process outside its process group, a frozen v1 freezer
  cgroup, a loop device) stopped 15 to 16 s after a start with a 5 s deadline and leaving nothing, and
  `check-resctl.sh --deadline 5`. Negative controls of the runners: see below. `tools-test.sh` with its deadlines: 511
  PASS, 9 DENIED, 27 LIMIT, 1 UPSTREAM, 0 FAIL in 4.1 minutes (sessions of 69, 91 and 85 s), host unchanged.
- **partly verified**: stability of the timing rows. In every run on this host so far the longest stall was 0.00 to 0.04 s
  (bound 0.5 s), the slowest `state` 0.02 s (2 s), the kill 0.03 to 0.06 s (5 s) and the OOM crash loop 1.5 to 2.5 s
  (15 s); a loaded or noisier host is not investigated.

### Resource management and runtime behaviour (`runtime/resctl`)
- **Allocation** — **verified** as not applicable for a custom allocator: resctl uses the standard allocator of Rust on
  musl, allocates little and briefly, and an allocation failure aborts (`panic = "abort"`); for a supervisor that is a
  lost supervisor, which its reaper and `recover` handle (rows `proc_lost_reaper`, `proc_lost_recover`).
- **Buffering and queues** — **verified**: the persistent state is bounded (audit trail 2 × 1024 lines, quarantine 32
  files, start log one minute; unit tests); a supervised workload's stdio is inherited, resctl buffers none of it.
- **OOM protection** — **partly verified**. Victim preference (`oom_score_adj`) and containment (memory limits) are
  separate mechanisms here and are tested separately.
  - **Launch paths** — **verified**: `run`, `enter WORKLOAD` and the main process of `supervise` (also on every restart,
    through `__enter`) all go through `Controller::attach`, which writes the bias before the process enters the domain
    and refuses the move when the write fails (unit test); there is no other way into a workload (`recover` only moves
    processes out). Rows `oom_bias` (run, its child, a supervised workload after its uid changed) in host, hostv1 and
    guest.
  - **Workload score enforcement** — **partly verified**: with CAP_SYS_RESOURCE the kernel keeps 1000 as the minimum and
    a workload of another user cannot lower it (`oom_bias_locked.guest`); a lowered score (root can, and anybody where the
    floor is not kept) is raised again by the supervisor within 500 ms, children too, audited at raises 1, 2, 4, ...
    (`oom_bias_restored` in all three environments; unit test; negative control `no-bias-restore`). resctl says what it
    can do (`probe`: `oom.lower=yes|no: why`), checked against `CapEff` (`oom_bias_locked`, `critical_oom`) and in a user
    namespace (`oom_lower.unpriv`). **LIMIT** on this host: no CAP_SYS_RESOURCE, so the floor is not kept and the raise
    leaves a window of half a second. **Not done**: an unmanaged `run` has no supervisor to raise its score; dropping
    the capability from root workloads would need `prctl`, which the crate's no-`unsafe`, no-dependency rule excludes.
  - **Workload memory containment** — **verified**: the memory limit of a workload, of its class and of all workloads
    together hold under exhaustion (rows `memory_oom`, `stress_memory`, `stress_class`, `class_ceiling`,
    `aggregate_ceiling`; negative control `no-memory-limit`); `memory.high` and `memory.oom.group` in the guest, **LIMIT**
    on this host (cgroup v1 memory). A descendant stays in its domain unless something with write access to the cgroup
    files moves it (root): a documented limit.
  - **Critical-process policy** — **verified**: three classes (workloads 1000, ordinary processes 0, registered critical
    processes the policy's `[system] oom.score_adj`, default -500, never -1000), set at registration, with the
    registration standing and a warning, an audit line and `probe` saying so where the score cannot be lowered. Rows
    `critical_oom` (guest -500; **LIMIT** on this host), `global_oom_no_workload` (guest: with no workload left, a
    critical service of 482 MiB with -500 survived and the OOM killer took the allocation outside every domain that
    asked for all 964 MiB); negative control `no-critical-bias` (the critical service is then the one killed).
    `memory.low` of `system` (v2) is the policy's `memory.reserve`; `memory.min` is not used. **Open**: the memory
    critical services actually need is not measured; the reserve is a number in the policy.
  - **Early pressure response** — **open**: resctl reads neither PSI nor `memory.events` to shed load before the kernel
    acts; admission control is limited to the policy's ceilings and the supervision and start-rate caps. Found while
    preparing the measurement: PSI is there per workload on this host too, because on a hybrid host resctl puts the
    lifecycle in the cgroup v2 hierarchy, which carries `memory.pressure` for every cgroup although memory is a v1
    controller (`state` already reports `pressure.memory.some_total`).
  - **Control plane under pressure** — **partly verified**: `resctl state` answers within 0.02 s while workloads exhaust
    memory and the CPUs (`stress_control`, bound 2 s; negative control `slow-state`); a supervisor that cannot allocate
    aborts and its reaper and `recover` take over (`proc_lost_reaper`, `proc_lost_recover`). **Open**: no memory reserve
    for the control plane itself beyond the workload ceiling.
  - **OOM detection and recovery** — **verified**: an OOM kill inside a domain is recorded (`oom=yes`, `crashed:9`;
    `proc_oom_crash`, `stress_oom_loop`), restarts stay bounded and the domain is swept (`stress_kill`, `leftovers`).
  - **OOM origin attribution** — **verified**, with named limits. The record of a supervised run and the trail say where
    its OOM kills came from (`oom_origin`: workload, class, ceiling, outside or unknown) and on what evidence
    (`oom_evidence`). Per level that is the v2 count of declared OOMs (`memory.events.local`) or the v1 failcnt, plus the
    domain's kill count, the machine's (`/proc/vmstat`) and the kernel's own records of the kills (`/dev/kmsg`, kernel
    facility only). Ambiguity is `unknown` with its reason, never a guess. Rows `oom_origin_local` (in the guest with a
    forged user-space record that must not count), `oom_origin_class` and `oom_origin_external` (an outsider's SIGKILL:
    no origin) in all three environments, `oom_origin_outside` (a machine-wide OOM, guest); 15 unit tests; negative
    controls `oom-blame-workload` and `kmsg-user-records`; 4 self-test cases. Measured on this host's kernel: with
    memory+swap pinned to the limit, as resctl sets it on v1, the failcnt of the limit that was hit stays 0 at the OOM,
    so on v1 only the kernel log decides (it needs CAP_SYSLOG and keeps at most 10 records in 5 s). **Limits**: a run
    finished by a reaper or `recover` is `unknown` (no snapshot of its start outlives the supervisor); an unmanaged `run`
    becomes its command and has no record, so nobody attributes its kills; a record is matched by the end of its cgroup
    path.
  - **Fake host** — **verified**: it models `oom_score_adj` per process (0 until written, -1000 to 1000, refused writes)
    and `CapEff`; unit tests cover the restore, a refused raise, the critical preference and its refusal, and the report.
    A fake-host PASS says nothing about which process the kernel picks: that is what the guest rows are for.
  - **VM tests** — **verified** in the guest (QEMU, 1 GiB, emulated timing): a workload's own OOM, several workloads at
    their class ceiling, a machine-wide OOM with a workload to take and with none left, a lowered bias, the observer; each
    destructive allocation is bounded by its own size or stops when its target is gone. **Open**: pressure that persists
    after the first OOM kill.
- **CPU, I/O and concurrency** — **verified**: CPU weights, quotas, cpuset, `io.max` on a whole disk, CPU pressure (PSI), the
  periodic task of `system` under 8 CPU burners. **open**: stalls of `system` caused by I/O; `system` has no I/O protection.
- **Lifecycle and recovery** — **verified** in three environments: rows of the areas process (26), critical (17), terminate,
  recover and lifecycle, and the negative controls for the restart bound, the classification of a requested end, the
  shield of critical processes, the guard against requests from workloads and the refusal to protect a workload process.

### Testing and verification
- **verified**: unit tests (289), the three sessions as integration tests, the full check as regression, fault injection
  (corrupt records and registry, killed supervisors and reapers, a command that cannot start), exhaustion (the stress
  area) and the Alpine kernel of the test bed (QEMU, emulated: functional, not timing).
- **not applicable**: physical hardware (none in this environment).

### Self-judge and negative controls
- **verified**: `resctl-judge.py` and its self-test (103 cases); the negative controls of resctl and the suite (above).
- **verified**: the negative controls of the runners. `check-deadline.sh --defect NAME` runs the check on a copy of
  `build/alpine` with the defect and passes only if the copy's check fails: `no-thaw` (the frozen process kept the
  privileged sandbox alive until the safety timeout, and the cleanup check saw it and its cgroup), `no-message` (exit 124
  without the `deadline:` line) and `no-dev-deadline` (the unprivileged runner ran until the safety timeout) were all
  caught, the host unchanged after each (161, 57 and 207 s). Their first run found a defect of the control itself (its
  report functions were defined after the code that called them, so it died before comparing the host): fixed, and the
  three run again.
- **verified**: the timing oracles are checked without stalling the machine: a periodic task stopped on purpose for a
  second must report it in every run (`stress_observer`: 999 to 1049 ms), a blind observer (`blind-beat`) and a `state`
  that takes 2.5 s (`slow-state`) are caught. A real stall of `system` is still not produced on this shared host.

## Next
1. Measure the early pressure response before designing it: for fast and slow growth, how long before the OOM kill
   each signal appears (`memory.events.local` high and max, memory PSI, usage against the limit), how often each one
   fires without a kill (a workload that only fills the page cache), and what polling them costs. Then decide on a
   bounded action, or none, from the numbers. The supervisor already polls every 50 ms; it must not allocate or wait
   without bound on that path.
2. Keep the snapshot of a run's start in its record, so that a reaper or `recover` can attribute the kills of a run
   whose supervisor was lost.
3. A key per crate copy for a kept cargo target directory, then measure the unprivileged session again.
