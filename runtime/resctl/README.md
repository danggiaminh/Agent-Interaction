# resctl: resource control for Agent-Interaction workloads

`resctl` gives every AI workload an explicit resource domain (memory, CPU, process count, I/O) and a lifecycle it can be
stopped, frozen, killed and recovered by, and keeps the critical services of the machine outside every one of those kill
domains. One Rust crate without dependencies (`Cargo.lock` lists `resctl` alone), Linux, cgroup v2 first with a
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
the policy is loaded. A policy that cannot mean what it says on this machine is refused, never rounded: a limit above the
limit of its domain, a workload ceiling that leaves no memory to `system`, a `memory.high` above `memory.max`, a missing
`memory.max` or `pids.max` (every workload is bounded in memory and process count).

| section            | keys                                                                                       |
|--------------------|--------------------------------------------------------------------------------------------|
| `[system]`         | `cpu.weight`, `memory.reserve` (protected from reclaim where the kernel allows it)         |
| `[workload]`       | `memory.max`, `pids.max`, `cpu.max`: the aggregate ceiling                                 |
| `[class.NAME]`     | `memory.max`, `pids.max`, `cpu.max`, `cpu.weight`: the class ceiling and its CPU share     |
| `[limits.NAME]`    | what one workload of the class gets by default: `memory.max`, `memory.high`, `pids.max`, `cpu.max`, `cpu.weight`, `memory.swap` (`none`\|`allow`), `oom.group`, `io.max` (`DEV rbps=N wbps=N riops=N wiops=N`, repeatable), `cpuset.cpus` |
| `[stop]`           | `grace`: the time between SIGTERM and the kill                                             |
| `[recover]`        | `orphans`: `kill` or `keep`                                                                |

A start may raise any limit above the class default, up to the class ceiling (`[class.NAME]`), which is itself bounded by
`[workload]`. Anything above is refused with rc 2 before a directory exists. `resctl policy` prints the policy as bound to
this machine.

## Commands

```text
resctl [--policy FILE] [--lifecycle auto|v1|v2] COMMAND
  probe | policy | init | create CLASS/ID [LIMITS] | run CLASS/ID [LIMITS] -- CMD... | enter system|CLASS/ID -- CMD...
  freeze|thaw|kill|remove CLASS/ID | stop CLASS/ID [--grace 5s] | list | status [DOMAIN] | recover [--orphans kill|keep] | teardown
LIMITS: --memory-max V --memory-high V --pids-max N|max --cpu-max P%|max --cpu-weight W --io 'DEV k=N ...' --cpus LIST --swap none|allow --oom-group yes|no
```

- `probe` shows which hierarchy serves which feature (`feature.memory=v1:/sys/fs/cgroup/memory`, ...) and changes
  nothing; `init` creates the domains and sets their limits, and is safe to repeat.
- `run` creates the domain and execs the program in it; the exit status is the program's, and the domain stays, empty,
  until `remove` or `recover`. A program that cannot be resolved is refused with rc 2 before a domain is created; one that
  fails to exec afterwards leaves an empty domain that `recover` removes.
- `stop` thaws a frozen workload, sends SIGTERM, waits for the grace period and then kills (`stop=graceful` or
  `stop=killed:N` on stdout). `remove` kills, then removes the directory in every hierarchy.
- `recover` is for a supervisor that died: it runs `init`, removes empty domains, kills (the default) or keeps
  (`--orphans keep`) workloads that still have processes, and removes empty classes the policy no longer names.
- Exit status: 0 done; 1 exists, timeout or I/O error; 2 the request is wrong or the policy refuses it; 3 this machine
  cannot (no hierarchy, controller or kernel feature) or this user may not. Messages are `resctl: <label>: <message>`.

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
- Unprivileged (a user namespace has no hierarchy): `probe` and `policy` work; every command that needs a hierarchy exits
  3 with `unsupported` and creates nothing.
- The binary is built for musl and is dynamic (PIE): it runs inside the tools image and the guest, not on a glibc host.

## Validation

```sh
build/alpine/check-resctl.sh [--image FILE] [--facts DIR]   # as root, three sessions and the judge; see build/alpine/README.md
```
