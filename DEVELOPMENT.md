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
  sessions, the judge, its self-test, then the host and the vendored tree compared with their state before. It takes 14.5
  minutes here (sessions of 67, 248 and 548 s); every session prints its duration.
- **verified**: targeted runs. `--areas LIST` runs only those sections of the suite (and `layout`, which measures the causes
  of the host's limits), `--no-guest` skips the test bed, and the judge's `--partial` mode judges whatever ran as strictly as
  a full run, never proves a limit without its guest proof and never passes for a full verdict (7 self-test cases).
  Measured: the process and critical sections on the host in 4.4 minutes; the stress section alone in sessions of 63 and
  67 s.
- **verified**: `check-resctl.sh` is one compound command, read whole before it runs: an edit of the script during a run had
  made bash execute a fragment of the new text and abort that run (exit 127).
- **verified**: negative controls live in the repository. `resctl-mutants.py` holds seven deliberate defects of resctl and
  the rows each must break; `check-resctl.sh --mutant NAME` builds the defect into a copy of the crate, runs its areas on
  the host (and in the guest when only the guest can show it) and passes only if the judge fails those rows. All seven
  are caught, each by its unit tests as well.
- **verified**: bounded test execution. Every crash-loop supervisor of the suite runs under a time limit, the unit tests
  under 900 s, and the fake host of the supervisor tests fails a test that starts 1000 processes. Found by the negative
  controls: the unbounded-restart defect held `cargo test` for 42 minutes, and earlier the suite itself.
- **open**: the unprivileged session compiles the crate from scratch on every run (61 to 128 s, three profiles). A target
  directory kept between runs needs a key per copy of the crate: two copies at different paths that shared one made cargo
  test the first copy's binaries.
- **open**: a whole session has no wall-clock bound in `check-resctl.sh`; the bounds are inside the sessions.
- **partly verified**: stability of the timing rows. In eleven runs on this host the longest stall was 0.01 to 0.04 s
  (bound 0.5 s), the slowest `state` 0.02 s (2 s), the kill 0.04 to 0.06 s (5 s) and the OOM crash loop 2.0 to 2.5 s
  (15 s); a loaded or noisier host is not investigated.

### Resource management and runtime behaviour (`runtime/resctl`)
- **Allocation** — **verified** as not applicable for a custom allocator: resctl uses the standard allocator of Rust on
  musl, allocates little and briefly, and an allocation failure aborts (`panic = "abort"`); for a supervisor that is a
  lost supervisor, which its reaper and `recover` handle (rows `proc_lost_reaper`, `proc_lost_recover`).
- **Buffering and queues** — **verified**: the persistent state is bounded (audit trail 2 × 1024 lines, quarantine 32
  files, start log one minute; unit tests); a supervised workload's stdio is inherited, resctl buffers none of it.
- **Low memory** — **partly verified**:
  - **verified** on this host with real timing (h_, v_) and in the guest without timing (g_), rows `stress_*`: a workload
    over its memory limit is killed inside its own memory cgroup (the kernel's counter, peak at the limit) while a neighbour
    keeps what it holds; three workloads that each fit their limit are held to their class ceiling (peak exactly 256 MiB);
    a periodic task in `system` keeps waking every 20 ms and allocating (longest stall 0.01 to 0.04 s, bound 0.5 s); the
    control plane answers (slowest `resctl state` 0.02 s, bound 2 s); a kill under memory and CPU load is complete in
    0.05 s (bound 5 s); an OOM crash loop ends `failed` after 4 starts in 2 s (bound 15 s); the system domain's memory
    cgroup counts no OOM kill.
  - **verified**: memory pressure of the whole machine. Every process resctl puts into a workload carries
    `oom_score_adj` 1000 (rows `oom_bias`, all environments), so the OOM killer of the whole machine takes workloads first:
    in the guest a critical service holding 445 MiB survived while the workload of 162 MiB was taken (row `global_oom`);
    without the bias (negative control `no-oom-bias`) the critical service was the one killed.
  - **LIMIT** on this host: the memory protection of `system` (`memory.low`) needs the cgroup v2 memory controller, and the
    floor of the bias (a workload of another user cannot lower it) needs CAP_SYS_RESOURCE, which root lacks in this
    sandbox; both are proven in the guest.
  - **open**: once no workload is left to kill, the OOM killer takes the largest remaining process, which can be a critical
    service. A negative `oom_score_adj` for registered critical processes would cover that; not done (lowering a score
    needs CAP_SYS_RESOURCE, and a critical service that leaks would then exhaust the machine before the OOM killer took
    it).
  - **not applicable here**: swap (this host has none; a workload gets none by default).
- **CPU, I/O and concurrency** — **verified**: CPU weights, quotas, cpuset, `io.max` on a whole disk, CPU pressure (PSI), the
  periodic task of `system` under 8 CPU burners. **open**: stalls of `system` caused by I/O; `system` has no I/O protection.
- **Lifecycle and recovery** — **verified** in three environments: rows of the areas process (26), critical (17), terminate,
  recover and lifecycle, and the negative controls for the restart bound, the classification of a requested end, the
  shield of critical processes, the guard against requests from workloads and the refusal to protect a workload process.

### Testing and verification
- **verified**: unit tests (268), the three sessions as integration tests, the full check as regression, fault injection
  (corrupt records and registry, killed supervisors and reapers, a command that cannot start), exhaustion (the stress
  area) and the Alpine kernel of the test bed (QEMU, emulated: functional, not timing).
- **not applicable**: physical hardware (none in this environment).

### Self-judge and negative controls
- **verified**: `resctl-judge.py` and its self-test (94 cases); the seven negative controls.
- **open**: a negative control for the timing oracles (the stall of the periodic task, the latency of the control plane):
  a defect that stalls `system` cannot be produced safely on this host.

## Next
1. A key per crate copy for a kept cargo target directory, then measure the unprivileged session again (it is the largest
   fixed cost of a targeted run).
2. A wall-clock bound per session of `check-resctl.sh` that cannot leave a sandbox behind.
3. Decide on a negative `oom_score_adj` for registered critical processes (see Low memory, open) with a guest test that
   leaves no workload to kill.
