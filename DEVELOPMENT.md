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
  revision: 378 PASS, 6 DENIED, 6 LIMIT, 0 FAIL, self-test 99 of 99, host and vendored tree unchanged, in 13.9 minutes
  (sessions of 55, 248 and 521 s; every session prints its duration).
- **verified**: targeted runs. `--areas LIST` runs only those sections of the suite (and `layout`, which measures the causes
  of the host's limits), `--no-guest` skips the test bed, and the judge's `--partial` mode judges whatever ran as strictly as
  a full run, never proves a limit without its guest proof and never passes for a full verdict (7 self-test cases).
  Measured: the process and critical sections on the host in 4.4 minutes; the stress section alone in sessions of 63 and
  67 s.
- **verified**: `check-resctl.sh` is one compound command, read whole before it runs: an edit of the script during a run had
  made bash execute a fragment of the new text and abort that run (exit 127).
- **verified**: negative controls live in the repository. `resctl-mutants.py` holds eleven deliberate defects (ten of
  resctl, one of the suite itself, written `suite:PATH`) and the rows each must break; `check-resctl.sh --mutant NAME`
  builds the defect into a copy of the crate or of the suite, runs its areas on the host (and in the guest when only the
  guest can show it) and passes only if the judge fails those rows. In the last series (54 minutes) all eleven were
  caught and no run left anything on the host; the nine defects of resctl's logic fail its unit tests as well, the slow
  `state` and the blind observer only the suite.
- **verified**: bounded test execution. Every crash-loop supervisor of the suite runs under a time limit, the unit tests
  under 900 s, and the fake host of the supervisor tests fails a test that starts 1000 processes. Found by the negative
  controls: the unbounded-restart defect held `cargo test` for 42 minutes, and earlier the suite itself.
- **open**: the unprivileged session compiles the crate from scratch on every run (55 to 128 s, three profiles). A target
  directory kept between runs needs a key per copy of the crate: two copies at different paths that shared one made cargo
  test the first copy's binaries.
- **open**: a whole session has no wall-clock bound in `check-resctl.sh`; the bounds are inside the sessions.
- **partly verified**: stability of the timing rows. In every run on this host so far the longest stall was 0.01 to 0.04 s
  (bound 0.5 s), the slowest `state` 0.02 s (2 s), the kill 0.04 to 0.06 s (5 s) and the OOM crash loop 2.0 to 2.5 s
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
    acts; admission control is limited to the policy's ceilings and the supervision and start-rate caps.
  - **Control plane under pressure** — **partly verified**: `resctl state` answers within 0.02 s while workloads exhaust
    memory and the CPUs (`stress_control`, bound 2 s; negative control `slow-state`); a supervisor that cannot allocate
    aborts and its reaper and `recover` take over (`proc_lost_reaper`, `proc_lost_recover`). **Open**: no memory reserve
    for the control plane itself beyond the workload ceiling.
  - **OOM detection and recovery** — **partly verified**: an OOM kill inside a domain is recorded (`oom=yes`,
    `crashed:9`; `proc_oom_crash`, `stress_oom_loop`), restarts stay bounded and the domain is swept (`stress_kill`,
    `leftovers`). The tests tell a machine-wide OOM from a domain's own (the kernel's global counter, the domain's limit
    not reached); **open**: resctl's record does not.
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
- **verified**: unit tests (274), the three sessions as integration tests, the full check as regression, fault injection
  (corrupt records and registry, killed supervisors and reapers, a command that cannot start), exhaustion (the stress
  area) and the Alpine kernel of the test bed (QEMU, emulated: functional, not timing).
- **not applicable**: physical hardware (none in this environment).

### Self-judge and negative controls
- **verified**: `resctl-judge.py` and its self-test (99 cases); the eleven negative controls.
- **verified**: the timing oracles are checked without stalling the machine: a periodic task stopped on purpose for a
  second must report it in every run (`stress_observer`: 999 to 1049 ms), a blind observer (`blind-beat`) and a `state`
  that takes 2.5 s (`slow-state`) are caught. A real stall of `system` is still not produced on this shared host.

## Next
1. Tell a machine-wide OOM kill from a domain's own in the record of a supervised workload (the tests already can: the
   kernel's global counter, and the domain's limit not reached), so that `state` and the trail say which one it was.
2. A wall-clock bound per session of `check-resctl.sh` that terminates the whole session and verifies the cleanup; the
   destructive tests are already bounded (QEMU runs under `timeout -s KILL`, every allocation is bounded or stops).
3. An early pressure response: a supervisor that watches PSI or `memory.events` of its domain and acts before the kernel
   has to (design first: it must not allocate or wait without bound on that path).
4. A key per crate copy for a kept cargo target directory, then measure the unprivileged session again.
