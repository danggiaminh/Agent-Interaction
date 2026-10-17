# resctl: resource control and process lifecycle for Agent-Interaction workloads

`resctl` gives every AI workload an explicit resource domain (memory, CPU, process count, I/O) and an explicit lifecycle,
supervises it, ends it, restarts it within bounds and recovers after a failure, and keeps the critical services of the
machine outside every one of those kill domains. One Rust crate without dependencies (`Cargo.lock` lists `resctl`
alone) and without `unsafe` code (both crate roots carry `#![forbid(unsafe_code)]`); Linux, cgroup v2 first with a
cgroup v1 fallback for the hybrid layout of the cloud host. It is built and validated inside the tools image of
`build/alpine/` (`build/alpine/check-resctl.sh`).

## Domains

```text
agent-interaction/system                      critical services: limits and protection only
agent-interaction/workload                    every workload together: the ceiling that keeps them off the system's memory
agent-interaction/workload/<class>            one class (interactive, batch, ...): its ceiling and its CPU share
agent-interaction/workload/<class>/<id>       one workload: its own limits, and its own kill domain
```

The tree is created in every hierarchy that serves a feature and a process is attached to the same directory in all of
them. Nothing that ends or suspends processes (`kill`, `stop`, `freeze`, `thaw`, `remove`, `recover`, `teardown`) takes
anything but a `CLASS/ID`: the type that names a target cannot hold `system` or `workload`, a class is never named
`system` or `workload`, and every attempt is refused with rc 2 before anything is touched. `teardown` leaves `system`
alone while it holds a process. A workload's kill domain is its directory: `kill` ends every process in it, including the
ones that called `setsid`, double-forked, or filled the process limit.

## Policy

`policy/default.policy` is built in; `--policy FILE` replaces it. Percentages are of the machine's `MemTotal`, bound when
the policy is loaded. A policy that cannot mean what it says on this machine is refused with rc 2, never rounded or
clamped: a limit above the limit of its domain, a workload ceiling that leaves no memory to `system`, a `memory.high`
above `memory.max`, a missing `memory.max` or `pids.max` (every workload is bounded in memory and process count), a
restart limit above its cap, a duplicate section, and any attempt to grant protected status. The one OOM setting a policy
may make is `[system] oom.score_adj`, the preference that critical processes get when an operator registers them; it
grants nothing, -1000 (immune to the OOM killer) is refused, and so is any `oom_score` key in another section.

| section            | keys                                                                                       |
|--------------------|--------------------------------------------------------------------------------------------|
| `[system]`         | `cpu.weight`, `memory.reserve` (protected from reclaim where the kernel allows it), `oom.score_adj` (-999 to 0) |
| `[workload]`       | `memory.max`, `pids.max`, `cpu.max`: the aggregate ceiling                                 |
| `[class.NAME]`     | `memory.max`, `pids.max`, `cpu.max`, `cpu.weight`: the class ceiling and its CPU share     |
| `[limits.NAME]`    | what one workload of the class gets by default: `memory.max`, `memory.high`, `pids.max`, `cpu.max`, `cpu.weight`, `memory.swap` (`none`\|`allow`), `oom.group`, `io.max` (`DEV rbps=N wbps=N riops=N wiops=N`, repeatable), `cpuset.cpus` |
| `[stop]`           | `grace`: the time between SIGTERM and the kill                                             |
| `[recover]`        | `orphans`: `kill` or `keep`                                                                |
| `[restart]`        | `mode` (`never`\|`on-failure`\|`always`), `max`, `window`, `total`, `backoff`, `backoff.max`, `rate` |
| `[supervise]`      | `workloads`: how many workloads one machine supervises at once                             |
| `[run]`            | `uid`, `gid` (go together), `allow.root`: who a supervised workload runs as                |

A start may raise any limit above the class default, up to the class ceiling (`[class.NAME]`), which is itself bounded by
`[workload]`. Anything above is refused with rc 2 before a directory exists. `resctl policy` prints the policy as bound to
this machine.

## Lifecycle

A supervised workload (`resctl supervise`) has a record with an explicit phase. Every change of phase goes through one
function that refuses a transition that makes no sense, is written to the record in one step and is audited.

```text
created -> starting -> running -> stopping -> killing -> sweeping -> stopped       (an operator asked)
                           |          |          |          ^    -> finished      (it ended cleanly, no restart)
                           +----------+----------+----------+    -> failed        (it ended unexpectedly, no restart left)
                                                                  -> backoff -> starting   (restarted, within the limits)
any phase that is not final -> recovering -> stopped | failed     (its supervisor is gone; `recover` finishes the job)
```

`stopped` and `finished` may be started again; `failed` is quarantined until an operator runs `reset`, so a crash loop
stops being restarted and cannot be restarted by accident.

**An unexpected exit is told from an intentional one.** An operator's `stop` or `kill` is recorded (`intent`, audited) before
any signal is sent. When the main process ends, the end is classified: with a recorded intent it is `requested:stop` or
`requested:kill` whatever the raw status was (SIGTERM, SIGKILL); without one it is `clean` (exit 0), `failed:N` (exit N),
`crashed:S` (killed by signal S: a crash, an OOM kill, somebody else's `kill -9`), `launch-failed` (it never started) or
`lost` (its supervisor vanished and nobody saw how it ended). A requested end is never restarted; the raw status and
`oom=yes` (the kernel's `oom_kill` counter moved) stay next to the label.

**Whatever happens, nothing is left behind.** When the main process ends, for whatever reason, the supervisor sweeps: it
kills every process that is still in the kill domain, removes the domain, and only then decides what follows. A reaper
(`__reap`, one per supervised workload, in its own process group) blocks on the supervisor's lock; when the supervisor
dies, whatever the reason, it sweeps the domain and finishes the record, and `recover` does the same for what the reaper
could not reach.

**Restarts are bounded by explicit limits** (the caps are compiled in; a policy above a cap is refused):

| key              | meaning                                                                          | bound |
|------------------|----------------------------------------------------------------------------------|-----|
| `mode`           | `never`, `on-failure` (default), `always` (a clean exit is restarted too)         |     |
| `max` / `window` | automatic restarts within one window                                              | 10; window ≥ 1s |
| `total`          | automatic restarts during one supervisor run, however far apart                   | 100 |
| `backoff`        | the wait before the first restart; it doubles with each further restart of a window, up to `backoff.max` | ≥ 10ms |
| `rate`           | starts per minute of all workloads together: a restart waits for room, a manual start is refused | 120 |
| `[supervise] workloads` | workloads supervised at once                                              | 64  |

A workload that uses up its budget (`max` in a window, `total`, or the mode is `never`) ends in `failed` with the reason
recorded, and is not started again until `resctl reset CLASS/ID`. A run that ends `lost` is never restarted
automatically: nobody can say it is safe. The processes and the domain of the previous run are gone before the next run
is built, so a restart can neither accumulate processes nor leave a stale domain.

**A workload never runs with the privileges of resctl.** A supervised workload is switched to `[run] uid`/`gid`
(65534:65534 by default) before it executes; a supervisor that is root and has none refuses to start anything unless
`allow.root = yes`. It therefore cannot write the state directory (mode 0700, owned by the user that runs resctl), cannot
forge a record and cannot register itself as a critical process.

## Critical processes

A critical process is a process in `system` that is registered in the registry (`protect PID`, or `enter system -- CMD`,
which places the program in `system` and registers it under its file name). An entry is the pid, the start time of the
process (so that a reused pid is not mistaken for the service) and a name. The protection has three parts:

1. **Placement.** `system` is outside every workload kill domain; nothing that kills names it.
2. **A shield around every kill.** Every command that ends processes (`kill`, `stop`, `remove`, `recover`, `teardown`, the
   sweep of a supervisor, the reaper) works with the live registry: a registered process that is found inside a workload
   domain is first moved back to `system` (audited as `rescue`) and the kill carries on; one that cannot be moved keeps
   the domain from being killed or removed (reported as `stuck`, never killed). A domain that holds a critical process is
   never frozen. Lifecycle and recovery paths take the same shield, so a restart or a recovery never weakens it.
3. **No way in for a workload.** A mutating command is refused (`refuse CMD from workload W`, audited) when its caller
   runs inside a workload domain; `protect` refuses a process that runs in a workload domain; a policy cannot name
   protected status; the registry lives in the state directory a workload (another uid) cannot open; and a request is
   decided by where the process is, not by what the caller says.
4. **The OOM killer of the whole machine takes workloads first, ordinary processes next.** Its choice follows three
   classes:
   - **workloads**: every process resctl puts into a workload (`run`, `enter`, the main process of `supervise`) is
     given `oom_score_adj` 1000 before it enters, and its children inherit it. When resctl holds CAP_SYS_RESOURCE the
     kernel keeps 1000 as the process's minimum, so a workload of another user cannot lower it; where it can (resctl
     without the capability, or a root workload), the supervisor raises every lowered score in its domain again within
     half a second (audited at the first raise of a run and whenever the count doubles). An unmanaged `run` has no
     supervisor to do that.
   - **ordinary processes** (everything outside resctl's domains, resctl itself included): their own score, 0 by default.
   - **critical processes**: registration (`protect`, `enter system`) sets the policy's `[system] oom.score_adj` (the
     default policy says -500; a lower score a process already has is kept). The OOM killer then takes a critical process
     before an ordinary one only when it is larger by half of the memory: it is protected, not immune, so a leaking
     service cannot leave the machine with nothing to take. Lowering a score needs CAP_SYS_RESOURCE; without it the
     registration stands, the score stays, and resctl says so (stderr, a `warn` line in the trail, `probe`:
     `oom.lower=no: <why>`).
   An OOM inside a domain still chooses among the processes of that domain, and `memory.reserve` (`memory.low` of
   `system`, cgroup v2 only) keeps the memory of critical services from being reclaimed for workloads; resctl sets no
   `memory.min`, whose hard protection would move the failure to everything else.

A damaged registry is not believed: kill-type commands fail closed until it is healed. The heal (done by the prelude of
most mutating commands, and by `recover`) keeps every readable line that still names a live process of the same start
time, adds every process found in `system` (named `pid<N>`), and quarantines the damaged file.
`unprotect PID` withdraws the registration (the process stays in `system`). Registered processes that exited are `stale` in
`critical`.

## Commands

```text
resctl [--policy FILE] [--lifecycle auto|v1|v2] [--state-dir DIR] COMMAND
  probe | policy | init | create CLASS/ID [LIMITS] | run CLASS/ID [LIMITS] -- CMD...
  supervise CLASS/ID [LIMITS] [--restart never|on-failure|always] -- CMD...
  enter system|CLASS/ID -- CMD... | freeze|thaw|kill|remove CLASS/ID | stop CLASS/ID [--grace 5s]
  list | status [DOMAIN] | state [CLASS/ID] | audit [N] | critical
  protect PID [--name NAME] | unprotect PID | reset CLASS/ID | recover [--orphans kill|keep] | teardown
LIMITS: --memory-max V --memory-high V --pids-max N|max --cpu-max P%|max --cpu-weight W --io 'DEV k=N ...' --cpus LIST --swap none|allow --oom-group yes|no
```

- `probe` shows which hierarchy serves which feature (`feature.memory=v1:/sys/fs/cgroup/memory`, ...) and the OOM
  settings it can make (`oom.workload=1000`, `oom.lower=yes`, or `oom.lower=no:` and why: no CAP_SYS_RESOURCE, or the
  capability of a user namespace, which the kernel ignores for OOM scores), and changes nothing; `init` creates the domains and sets their limits, and is safe to repeat.
- `run` creates the domain and execs the program in it; the exit status is the program's, and the domain stays, empty,
  until `remove` or `recover`. It has no lifecycle record. A program that cannot be resolved is refused with rc 2 before
  a domain is created; one that fails to exec afterwards removes its domain again.
- `supervise` stays in the foreground as the supervisor: it creates the domain and the record, starts the program as the
  policy's user, watches it, restarts it within the limits and prints `phase=… gen=… restarts=… reason=…` at the end
  (rc 0 `stopped`/`finished`, rc 1 `failed`). It refuses a workload that another supervisor holds, one that is `failed`
  (until `reset`), a domain that already exists and a start over the supervision or start-rate limit. What a lost
  supervisor left of the same id is cleaned up first, and a record that does not read is quarantined.
- `stop` records the intent, thaws a frozen workload, sends SIGTERM, waits for the grace period and then kills
  (`stop=empty|graceful|killed:N` and `phase=` on stdout). `kill` does it at once. For a supervised workload the supervisor
  carries both out (phases `stopping`, `killing`) and the command waits for it, falling back to the kill from the
  command line if the supervisor does not answer. `remove` kills, then removes the directory in every hierarchy.
- `state` prints one line per supervised workload: `phase`, `gen`, `restarts`, `supervisor=alive|lost|unknown`,
  `domain=present|absent`, `main`, `sup`, `intent`, `ended`, `status`, `oom`, `stop`, `since`, `cmd`, `reason`; a record that
  does not read is reported as `corrupt=`. `list` and `status` show the domains and their counters.
- `audit [N]` shows the last N lines of the audit trail, `critical` the registry: `NAME pid=N state=live|stale
  placed=system|elsewhere|workload:CLASS/ID|- start=T`.
- `protect PID` prints `protected=NAME pid=N placed=system moved=yes|no`; `unprotect` prints `unprotected=` or `not-protected=`.
- `reset` clears a `failed` (or unreadable) record so the workload id can be used again; it ends whatever is left of the
  id first.
- `recover` is for a supervisor that died. It heals the registry, runs `init`, finishes the records of workloads whose
  supervisor is gone (`swept=… phase=…`), removes empty domains, kills (the default) or keeps (`--orphans keep`) workloads
  that still have processes and have no live supervisor, and removes empty classes the policy no longer names. It never
  touches a workload whose supervisor is alive.
- `teardown` ends every workload, removes the domains and the records and leaves the critical processes in `system`
  (`kept=system: N process(es) stay in their domain`).
- Exit status: 0 done; 1 exists, timeout, I/O error or a failed workload; 2 the request is wrong or the policy refuses it;
  3 this machine cannot (no hierarchy, controller or kernel feature) or this user may not. Messages are
  `resctl: <label>: <message>`.

## State and audit

All of it lives in the state directory, `/run/agent-interaction` unless `--state-dir` names another: mode 0700, owned by the
user that runs resctl, checked on every open (a mutating command fails closed otherwise).

```text
records/CLASS.ID     the record of a supervised workload, replaced in one step, read back strictly
locks/CLASS.ID.sup   held by the supervisor for as long as it lives (how a lost supervisor is told from a live one)
critical             the registry of critical processes
starts               the uptime of every recent start (the global start rate)
audit.log[.1]        the audit trail: two generations of 1024 lines
quarantine/          files that did not read back, kept for inspection
```

The audit trail is a line per fact: `seq at pid:role event subject detail`. Events: `init create run enter freeze thaw stop
kill remove recover reset teardown supervise phase intent start exit restart refuse protect unprotect rescue quarantine
warn`. A change is audited first and made second, and when it fails the trail says so (`warn`); protecting a process needs
its trail, ending a workload does not wait for it. The role says who acted (`cli`, `super`, `main`, `reap`).

## Which hierarchy serves what

`Layout::detect` binds each feature to a hierarchy: memory, pids, CPU, cpuset and I/O prefer cgroup v2 and fall back to
v1; freeze, kill and pressure (PSI) come from v2, or from the v1 freezer under `--lifecycle v1`. On the hybrid cloud host
the limits therefore go through v1 and the lifecycle through the unified hierarchy. What a hierarchy cannot serve is
reported by `probe` and by `init`/`create` as `unbound=<setting>: <reason>`: never applied silently, never counted as
applied.

- `memory.high`, `memory.oom.group` and the memory protection of `system` exist only in cgroup v2's memory controller. On
  a host whose memory controller is cgroup v1 they are `unbound` and an OOM kill takes one process, not the workload.
- I/O limits need a whole disk (`MAJ:MIN` of the device, not a partition). v1 shares are weight × 10.24.
- Swap: with v1 memsw the limit is set equal to the memory limit; without memsw the default is silent and an explicit
  `--swap none` is reported unbound.
- v1 `oom_kill` is counted per cgroup, and peak usage is sticky (it survives a re-`init`).
- A re-`init` that lowers a v1 CPU quota below that of existing children fails: remove the workloads first.
- `recover` and `teardown` must be given the same `--lifecycle` that created the domains.
- Unprivileged (a user namespace has no hierarchy): `probe`, `policy`, `list`, `state`, `audit` and `critical` work (and
  report nothing); every command that changes something exits 3 with `unsupported: <command>: this machine has no cgroup hierarchy to
  hold it` and creates nothing, not even the state directory (a policy that is wrong is still refused first, with rc 2).
- The binary is built for musl and is dynamic (PIE): it runs inside the tools image and the guest, not on a glibc host.

## Limits

What resctl does not do, so that nobody relies on it:

- **Same user, same trust.** The confinement of a workload holds because it runs as another user (`[run] uid/gid`) and
  because every command checks where its caller runs. A root workload with write access to the cgroup filesystem can
  move itself; a deployment where resctl runs as an ordinary user cannot keep the workload out of the state directory or
  a delegated cgroup tree (uid/gid are ignored when resctl is not root). Detection is best effort; prevention needs the
  uid/gid split. `run` and `enter` are the operator's own commands: only supervised workloads drop to the policy's user.
  The refusals of a workload that cannot even open the state directory are not audited.
- **No signal handlers** (no `unsafe`, no dependencies): a supervisor that is itself killed with SIGKILL/SIGTERM leaves
  its workload to the reaper (best effort), to `recover` or to a new `supervise`. An unmanaged `run`/`attach` workload's
  natural end is not observed; its emptied domain stays until `recover` or `remove`.
- **A critical process misplaced in a workload at the moment a registry that is entirely unsalvageable is healed** (no
  readable line, or not text) is not rebuilt into the registry and can be killed with that workload. A tiny window between
  quarantining the damaged file and writing the rebuilt one leaves no registry until the next heal or `protect`
  (the quarantined copy remains). A rebuild registers every process of `system`, named `pid<N>`.
- A critical pid that is moved into a domain while that domain is being killed can be reached by the kill (narrow race);
  children created by a critical process before it was moved stay in the workload domain; hiding other users' `/proc`
  entries (`hidepid`) can make a critical process look gone to a non-root resctl; pid 1 cannot be registered.
- **Classification limits.** `kill -9` by somebody outside resctl is "unexpected" (`crashed:9`) by design; an operator
  request that lands after the supervisor's terminal commit is dropped, and one recorded just after a self-exit can label
  that exit as requested; exit codes 125–127 are ambiguous between a launch failure and the workload's own exit; a
  workload whose main process exits while its children go on (a daemon launcher) is considered ended and its leftovers
  are swept; `oom=yes` needs the kernel's `oom_kill` counter.
- **State.** The state directory is volatile (`/run`); a workload whose record cannot be written is finished in memory
  and recovery later labels it `failed`/`lost`; audit write failures warn but do not block a termination; the audit trail
  keeps 1024 lines per generation, so the oldest generation is dropped; an adversary of the same user could flood the
  trail with refusal lines; a policy file and the state directory must not be writable by workloads, because the helper
  processes re-read them.
- **Restart accounting.** `total` is a lifetime cap even for a slow crasher; a new `supervise` of a `stopped`/`finished`
  workload gets a fresh in-memory budget (bounded globally by the persisted start rate, and by `failed` needing `reset`);
  a first start is refused rather than delayed when the per-minute rate is exceeded; failed creates do not use the rate.
- **Signalling** uses the system's `kill` tool (batches of 256, signals 0, 9 and 15 only); without it a graceful stop
  falls back to the kill and the v1 kill is unsupported. `NoNewPrivs` is not applied.
  Supervised workloads inherit the supervisor's environment and stdio (stdin is null). Non-UTF-8 arguments are refused.
- **Memory exhaustion: the boundary.** What holds is measured (the `stress` and `limits` rows): a workload, a class or
  all workloads together are held to their ceilings by the kernel's OOM killer inside their own memory cgroups, and
  meanwhile a periodic task in `system` and the control plane stay responsive (on the host, with real timing: the
  longest stall and the slowest `state` are bounded). When the whole machine runs out of memory the OOM killer takes a
  workload before a larger critical service, and with no workload left an ordinary process before a larger critical
  service (both shown in the guest only: it is not done to a shared host). What resctl does not provide:
  - a bias that a root workload cannot undo for good (`allow.root`, or `run` by root: it holds the capability to lower
    it; a supervisor raises it again within half a second, an unmanaged `run` has nobody to);
  - a floor that a workload of another user cannot lower in between when resctl lacks CAP_SYS_RESOURCE (when it is not
    root, or in a container that drops the capability, as the sandbox of the test host does: a LIMIT row there): the
    supervisor's raise closes the gap to half a second, not to nothing; the same capability is needed for the
    preference of critical processes, which otherwise keep their score (a LIMIT row too);
  - a critical service that keeps its memory once it is larger than the others by half of the memory, or when it is all
    that is left; no early reaction to pressure (resctl does not watch PSI or `memory.events` to shed load before the
    kernel acts; the limits and the OOM killer are the mechanism);
  - protection against exhaustion of kernel memory, or against stalls caused by I/O (`system` has no I/O protection,
    and `io.max` needs a whole disk).
  Where the memory controller is cgroup v1, `system` has no reclaim protection (`memory.low`), only the ceiling on
  `workload`. No userspace tool can prevent every kernel failure or every OOM of the whole machine.
- **Tests.** Failed domain creation with an unforeseen I/O error and restart delays caused by the start rate are covered
  by unit tests only (an end-to-end check would take about a minute). The timing oracles are checked from both sides
  without stalling the machine: a periodic task stopped on purpose for a second must report it (`stress_observer`), a
  blinded observer and a slow `state` are negative controls; a real stall of `system` is not produced on a shared host.

## Validation

```sh
build/alpine/check-resctl.sh [--image FILE] [--facts DIR]   # as root, three sessions and the judge; see build/alpine/README.md
build/alpine/check-resctl.sh --areas process,critical --no-guest   # a partial run: some sections, judged as strictly
build/alpine/check-resctl.sh --mutant unbounded                    # a negative control (resctl-mutants.py list)
```

The unit tests (`cargo test`: lifecycle rules, restart arithmetic, store, supervisor and registry against fakes) run in
the unprivileged session next to `fmt`, `clippy -D warnings`, a proof that the crate contains no `unsafe` and no
dependencies, and the refusals a machine without a hierarchy must give. The `process` and `critical` sections of the
privileged and guest sessions then run the real thing against real cgroups: normal lifecycle, crashes, crash loops,
forced termination, orphans, isolation, recovery, and a canary critical service that must survive all of it unchanged.
The `stress` section exhausts memory and the CPUs inside workload domains and measures from outside them: a periodic task
in `system` (`ctr-test beat`) times its own stalls, a probe times `resctl state`, and the kernel's counters of each memory
cgroup, not resctl's report, say where the OOM killer struck and how high the usage went. Every load is bounded twice, by
its domain and by its own size. The OOM policy is read back from the kernel too (`/proc/PID/oom_score_adj` of workloads,
their children and critical processes, against what `probe` claims and what `CapEff` says), and in the guest the whole
machine runs out of memory twice: with a workload to take, and with none left. `resctl-mutants.py` holds deliberate
defects (no shield for critical processes, protection of a workload process, no guard against requests from workloads,
an ignored intent, unbounded restarts, no memory limit per workload, no OOM bias, no raise of a lowered bias, no OOM
preference for critical processes, a `state` that stalls, and a blind stall observer in the suite itself) and the rows
each must break; `check-resctl.sh --mutant NAME` proves that they do.
