//! `resctl`: the command line of the resource controller. See ../README.md.

#![forbid(unsafe_code)]

use resctl::domain::{Controller, Error, Result, Target, Workload, SYSTEM, WORKLOAD};
use resctl::fs::{Host, RealHost};
use resctl::layout::{Layout, Lifecycle, Mode};
use resctl::life::{tidy, Intent, Phase};
use resctl::ops;
use resctl::policy::{
    parse_bool, parse_cpus, parse_duration, parse_io, parse_weight, valid_name, Limits, Mem, Num,
    Orphans, Policy, RestartMode,
};
use resctl::store::{Event, Store, Sup};
use resctl::supervise::{self, Ctx, Env, Job, Launch, RealProcs};
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

const DEFAULT_POLICY: &str = include_str!("../policy/default.policy");

/// Where the lifecycle records, the audit trail and the registry of critical processes are kept unless `--state-dir`
/// says otherwise: a volatile directory, private to the user that runs the commands.
const STATE_DIR: &str = "/run/agent-interaction";
/// How many lines of the audit trail `audit` shows when it is not told, and the most it can be asked for.
const AUDIT_DEFAULT: usize = 20;
const AUDIT_MAX: usize = 2048;

const USAGE: &str = "\
usage: resctl [--policy FILE] [--lifecycle auto|v1|v2] [--state-dir DIR] COMMAND

  probe                       the cgroup layout and which feature each hierarchy serves (changes nothing)
  policy                      the policy as bound to this machine (changes nothing)
  init                        create the domains and set their limits (safe to repeat)
  create CLASS/ID [LIMITS]    create a workload domain
  run CLASS/ID [LIMITS] -- CMD...   create a workload domain and become CMD inside it
  supervise CLASS/ID [LIMITS] [--restart never|on-failure|always] -- CMD...
                              run CMD in a new workload domain under supervision: it is restarted only
                              within the limits of the policy, and nothing is left behind when it ends
  enter system|CLASS/ID -- CMD...   become CMD inside an existing domain
  freeze|thaw|kill|remove CLASS/ID
  stop CLASS/ID [--grace 5s]  SIGTERM, then the kill after the grace period
  list                        every workload domain
  status [DOMAIN]             counters of system, workload, a CLASS or a CLASS/ID
  state [CLASS/ID]            the lifecycle of the supervised workloads (changes nothing)
  audit [N]                   the last N lines of the audit trail, 20 by default (changes nothing)
  protect PID [--name NAME]   put a process into system and register it as critical
  unprotect PID               withdraw the registration of a critical process
  critical                    the registered critical processes (changes nothing)
  reset CLASS/ID              clear the failed or unreadable lifecycle of a workload
  recover [--orphans kill|keep]     deal with workloads left by a supervisor that died
  teardown                    remove every workload and the empty domains

LIMITS: --memory-max V  --memory-high V  --pids-max N|max  --cpu-max P%|max  --cpu-weight W
        --io 'DEV rbps=N wbps=N riops=N wiops=N'  --cpus LIST  --swap none|allow  --oom-group yes|no
";

#[derive(Debug, PartialEq)]
enum Cmd {
    Help,
    Probe,
    Policy,
    Init,
    Create(Workload, Limits),
    Run(Workload, Limits, Vec<String>),
    Supervise(Workload, Limits, Option<RestartMode>, Vec<String>),
    Enter(Target, Vec<String>),
    Freeze(Workload),
    Thaw(Workload),
    Kill(Workload),
    Remove(Workload),
    Stop(Workload, Option<u64>),
    List,
    Status(String),
    State(Option<Workload>),
    Audit(usize),
    Protect(i32, Option<String>),
    Unprotect(i32),
    Critical,
    Reset(Workload),
    Recover(Option<Orphans>),
    Teardown,
    /// The main process of a supervised workload: started by its supervisor, never typed.
    HelperEnter(Workload, Vec<String>),
    /// The reaper of a supervised workload: started by its supervisor, never typed.
    HelperReap(Workload),
}

impl Cmd {
    fn name(&self) -> &'static str {
        match self {
            Cmd::Help => "help",
            Cmd::Probe => "probe",
            Cmd::Policy => "policy",
            Cmd::Init => "init",
            Cmd::Create(..) => "create",
            Cmd::Run(..) => "run",
            Cmd::Supervise(..) => "supervise",
            Cmd::Enter(..) => "enter",
            Cmd::Freeze(_) => "freeze",
            Cmd::Thaw(_) => "thaw",
            Cmd::Kill(_) => "kill",
            Cmd::Remove(_) => "remove",
            Cmd::Stop(..) => "stop",
            Cmd::List => "list",
            Cmd::Status(_) => "status",
            Cmd::State(_) => "state",
            Cmd::Audit(_) => "audit",
            Cmd::Protect(..) => "protect",
            Cmd::Unprotect(_) => "unprotect",
            Cmd::Critical => "critical",
            Cmd::Reset(_) => "reset",
            Cmd::Recover(_) => "recover",
            Cmd::Teardown => "teardown",
            Cmd::HelperEnter(..) => "__enter",
            Cmd::HelperReap(_) => "__reap",
        }
    }

    /// Whether the command changes anything: it is then refused inside a workload, needs the state directory to
    /// write to and goes through the audit trail.
    fn mutates(&self) -> bool {
        !matches!(
            self,
            Cmd::Help
                | Cmd::Probe
                | Cmd::Policy
                | Cmd::List
                | Cmd::Status(_)
                | Cmd::State(_)
                | Cmd::Audit(_)
                | Cmd::Critical
        )
    }

    /// The commands a supervisor starts for the workload it runs.
    fn helper(&self) -> bool {
        matches!(self, Cmd::HelperEnter(..) | Cmd::HelperReap(_))
    }

    /// The part this process plays, as the audit trail names it.
    fn role(&self) -> &'static str {
        match self {
            Cmd::Supervise(..) => "super",
            Cmd::HelperEnter(..) => "main",
            Cmd::HelperReap(_) => "reap",
            _ => "cli",
        }
    }
}

#[derive(Debug, PartialEq)]
struct Invocation {
    policy: Option<String>,
    lifecycle: Lifecycle,
    state_dir: Option<String>,
    cmd: Cmd,
}

/// `--flag=value` as (`--flag`, Some(`value`)); anything else as is.
fn split_flag(a: &str) -> (&str, Option<&str>) {
    match a.split_once('=') {
        Some((f, v)) if f.starts_with("--") => (f, Some(v)),
        _ => (a, None),
    }
}

fn take<'a>(
    it: &mut impl Iterator<Item = &'a str>,
    flag: &str,
    inline: Option<&str>,
) -> std::result::Result<String, String> {
    match inline {
        Some(v) => Ok(v.to_string()),
        None => it
            .next()
            .map(str::to_string)
            .ok_or_else(|| format!("{flag} needs a value")),
    }
}

fn domain_of(s: &str) -> std::result::Result<String, String> {
    match s {
        "system" => Ok(SYSTEM.into()),
        "workload" => Ok(WORKLOAD.into()),
        _ if s.contains('/') => Workload::parse(s)
            .map(|w| w.rel())
            .map_err(|e| e.to_string()),
        _ if valid_name(s) => Ok(format!("{WORKLOAD}/{s}")),
        _ => Err(format!(
            "`{s}` is not system, workload, a class or CLASS/ID"
        )),
    }
}

/// What `create`, `run` and `supervise` read: the workload, its limits, the restart mode and the command.
type Parsed = (Workload, Limits, Option<RestartMode>, Vec<String>);

/// A workload, its limit flags and, after `--`, a command. `--restart` is for `supervise` only.
fn workload_args<'a>(
    it: &mut impl Iterator<Item = &'a str>,
    restart_ok: bool,
) -> std::result::Result<Parsed, String> {
    let mut id = None;
    let mut l = Limits::default();
    let mut mode = None;
    let mut argv = Vec::new();
    while let Some(a) = it.next() {
        if a == "--" {
            argv = it.by_ref().map(str::to_string).collect();
            break;
        }
        let (flag, inline) = split_flag(a);
        match flag {
            "--memory-max" => l.memory_max = Some(Mem::parse(&take(it, flag, inline)?)?),
            "--memory-high" => l.memory_high = Some(Mem::parse(&take(it, flag, inline)?)?),
            "--pids-max" => {
                l.pids_max = Some(Num::parse(
                    &take(it, flag, inline)?,
                    "process count",
                    false,
                )?)
            }
            "--cpu-max" => {
                l.cpu_max = Some(Num::parse(
                    &take(it, flag, inline)?,
                    "CPU percentage",
                    true,
                )?)
            }
            "--cpu-weight" => l.cpu_weight = Some(parse_weight(&take(it, flag, inline)?)?),
            "--io" => l.io.push(parse_io(&take(it, flag, inline)?)?),
            "--cpus" => l.cpus = Some(parse_cpus(&take(it, flag, inline)?)?),
            "--oom-group" => l.oom_group = Some(parse_bool(&take(it, flag, inline)?)?),
            "--swap" => {
                l.swap_allowed = Some(match take(it, flag, inline)?.as_str() {
                    "none" => false,
                    "allow" => true,
                    v => return Err(format!("`{v}` is not none or allow")),
                })
            }
            "--restart" if restart_ok => mode = Some(RestartMode::parse(&take(it, flag, inline)?)?),
            f if f.starts_with("--") => return Err(format!("unknown option {f}")),
            _ if id.is_none() => id = Some(Workload::parse(a).map_err(|e| e.to_string())?),
            _ => return Err(format!("unexpected argument `{a}`")),
        }
    }
    let id = id.ok_or("a workload (CLASS/ID) is needed")?;
    Ok((id, l, mode, argv))
}

fn only<'a>(it: &mut impl Iterator<Item = &'a str>, cmd: Cmd) -> std::result::Result<Cmd, String> {
    match it.next() {
        None => Ok(cmd),
        Some(a) => Err(format!("unexpected argument `{a}`")),
    }
}

fn one_workload<'a>(
    it: &mut impl Iterator<Item = &'a str>,
) -> std::result::Result<Workload, String> {
    let w = it.next().ok_or("a workload (CLASS/ID) is needed")?;
    let w = Workload::parse(w).map_err(|e| e.to_string())?;
    match it.next() {
        None => Ok(w),
        Some(a) => Err(format!("unexpected argument `{a}`")),
    }
}

fn pid_of(a: &str) -> std::result::Result<i32, String> {
    a.parse::<i32>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| format!("`{a}` is not a process id"))
}

fn parse(args: &[String]) -> std::result::Result<Invocation, String> {
    let mut it = args.iter().map(String::as_str);
    let mut policy = None;
    let mut lifecycle = Lifecycle::Auto;
    let mut state_dir = None;
    let name = loop {
        let a = it.next().ok_or("no command")?;
        let (flag, inline) = split_flag(a);
        match flag {
            "--policy" => policy = Some(take(&mut it, flag, inline)?),
            "--lifecycle" => lifecycle = Lifecycle::parse(&take(&mut it, flag, inline)?)?,
            "--state-dir" => state_dir = Some(take(&mut it, flag, inline)?),
            _ => break a,
        }
    };
    let it = &mut it;
    let cmd = match name {
        "-h" | "--help" | "help" => Cmd::Help,
        "probe" => only(it, Cmd::Probe)?,
        "policy" => only(it, Cmd::Policy)?,
        "init" => only(it, Cmd::Init)?,
        "list" => only(it, Cmd::List)?,
        "teardown" => only(it, Cmd::Teardown)?,
        "critical" => only(it, Cmd::Critical)?,
        "create" | "run" | "supervise" => {
            let (w, l, mode, argv) = workload_args(it, name == "supervise")?;
            match (name, argv.is_empty()) {
                ("create", true) => Cmd::Create(w, l),
                ("create", false) => return Err("create takes no command (use run)".into()),
                ("run", false) => Cmd::Run(w, l, argv),
                ("run", true) => return Err("run needs a command after --".into()),
                (_, false) => Cmd::Supervise(w, l, mode, argv),
                (_, true) => return Err("supervise needs a command after --".into()),
            }
        }
        "enter" => {
            let target = match it.next().ok_or("a target (system or CLASS/ID) is needed")? {
                "system" => Target::System,
                s => Target::Workload(Workload::parse(s).map_err(|e| e.to_string())?),
            };
            if it.next() != Some("--") {
                return Err("enter needs -- and a command".into());
            }
            let argv: Vec<String> = it.by_ref().map(str::to_string).collect();
            if argv.is_empty() {
                return Err("enter needs a command after --".into());
            }
            Cmd::Enter(target, argv)
        }
        "freeze" => Cmd::Freeze(one_workload(it)?),
        "thaw" => Cmd::Thaw(one_workload(it)?),
        "kill" => Cmd::Kill(one_workload(it)?),
        "remove" => Cmd::Remove(one_workload(it)?),
        "reset" => Cmd::Reset(one_workload(it)?),
        "stop" => {
            let mut w = None;
            let mut grace = None;
            while let Some(a) = it.next() {
                let (flag, inline) = split_flag(a);
                match flag {
                    "--grace" => grace = Some(parse_duration(&take(it, flag, inline)?)?),
                    _ if w.is_none() && !a.starts_with("--") => {
                        w = Some(Workload::parse(a).map_err(|e| e.to_string())?)
                    }
                    _ => return Err(format!("unexpected argument `{a}`")),
                }
            }
            Cmd::Stop(w.ok_or("a workload (CLASS/ID) is needed")?, grace)
        }
        "status" => {
            let d = domain_of(it.next().unwrap_or("workload"))?;
            only(it, Cmd::Status(d))?
        }
        "state" => {
            let w = it
                .next()
                .map(|a| Workload::parse(a).map_err(|e| e.to_string()))
                .transpose()?;
            only(it, Cmd::State(w))?
        }
        "audit" => {
            let n = match it.next() {
                None => AUDIT_DEFAULT,
                Some(a) => a
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=AUDIT_MAX).contains(n))
                    .ok_or_else(|| {
                        format!("`{a}` is not a number of lines from 1 to {AUDIT_MAX}")
                    })?,
            };
            only(it, Cmd::Audit(n))?
        }
        "protect" => {
            let mut pid = None;
            let mut label = None;
            while let Some(a) = it.next() {
                let (flag, inline) = split_flag(a);
                match flag {
                    "--name" => label = Some(take(it, flag, inline)?),
                    f if f.starts_with("--") => return Err(format!("unknown option {f}")),
                    _ if pid.is_none() => pid = Some(pid_of(a)?),
                    _ => return Err(format!("unexpected argument `{a}`")),
                }
            }
            Cmd::Protect(pid.ok_or("a process id is needed")?, label)
        }
        "unprotect" => {
            let pid = pid_of(it.next().ok_or("a process id is needed")?)?;
            only(it, Cmd::Unprotect(pid))?
        }
        "recover" => {
            let mut orphans = None;
            while let Some(a) = it.next() {
                let (flag, inline) = split_flag(a);
                match (flag, take(it, flag, inline).as_deref()) {
                    ("--orphans", Ok("kill")) => orphans = Some(Orphans::Kill),
                    ("--orphans", Ok("keep")) => orphans = Some(Orphans::Keep),
                    ("--orphans", Ok(v)) => return Err(format!("`{v}` is not kill or keep")),
                    _ => return Err(format!("unexpected argument `{a}`")),
                }
            }
            Cmd::Recover(orphans)
        }
        "__enter" => {
            let w = it.next().ok_or("__enter needs a workload")?;
            let w = Workload::parse(w).map_err(|e| e.to_string())?;
            if it.next() != Some("--") {
                return Err("__enter needs -- and a command".into());
            }
            let argv: Vec<String> = it.by_ref().map(str::to_string).collect();
            if argv.is_empty() {
                return Err("__enter needs a command after --".into());
            }
            Cmd::HelperEnter(w, argv)
        }
        "__reap" => Cmd::HelperReap(one_workload(it)?),
        other => return Err(format!("unknown command `{other}`")),
    };
    Ok(Invocation {
        policy,
        lifecycle,
        state_dir,
        cmd,
    })
}

fn mem_total(host: &dyn Host) -> Result<u64> {
    let text = host
        .read("/proc/meminfo")
        .map_err(|e| Error::Io(format!("/proc/meminfo: {e}")))?;
    text.lines()
        .find_map(|l| {
            l.strip_prefix("MemTotal:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .map(|kb| kb * 1024)
        .ok_or_else(|| Error::Io("/proc/meminfo has no MemTotal".into()))
}

fn load_policy(path: &Option<String>) -> Result<Policy> {
    let text = match path {
        Some(p) => {
            std::fs::read_to_string(p).map_err(|e| Error::Usage(format!("policy {p}: {e}")))?
        }
        None => DEFAULT_POLICY.to_string(),
    };
    Policy::parse(&text).map_err(Error::Policy)
}

fn mem_text(m: Mem) -> String {
    match m {
        Mem::Max => "max".into(),
        Mem::Bytes(b) => b.to_string(),
        Mem::Percent(p) => format!("{p}%"),
    }
}

fn num_text(n: Num) -> String {
    match n {
        Num::Max => "max".into(),
        Num::N(v) => v.to_string(),
    }
}

fn limit_lines(prefix: &str, l: &Limits) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            out.push(format!("{prefix}.{k}={v}"));
        }
    };
    add("memory.max", l.memory_max.map(mem_text));
    add("memory.high", l.memory_high.map(mem_text));
    add("pids.max", l.pids_max.map(num_text));
    add("cpu.max", l.cpu_max.map(num_text));
    add("cpu.weight", l.cpu_weight.map(|w| w.to_string()));
    add("cpus", l.cpus.clone());
    out
}

fn policy_lines(p: &Policy) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(w) = p.system.cpu_weight {
        out.push(format!("system.cpu.weight={w}"));
    }
    if let Some(m) = p.system.memory_reserve {
        out.push(format!("system.memory.reserve={}", mem_text(m)));
    }
    if let Some(a) = p.system.oom_score_adj {
        out.push(format!("system.oom.score_adj={a}"));
    }
    out.extend(limit_lines("workload", &p.aggregate));
    for (name, c) in &p.classes {
        out.extend(limit_lines(&format!("class.{name}"), &c.aggregate));
        out.extend(limit_lines(&format!("limits.{name}"), &c.workload));
    }
    out.push(format!("stop.grace_ms={}", p.stop_grace_ms));
    out.push(format!(
        "recover.orphans={}",
        if p.orphans == Orphans::Kill {
            "kill"
        } else {
            "keep"
        }
    ));
    let r = &p.restart;
    out.push(format!("restart.mode={}", r.mode.name()));
    out.push(format!("restart.max={}", r.max));
    out.push(format!("restart.window_ms={}", r.window_ms));
    out.push(format!("restart.total={}", r.total));
    out.push(format!("restart.backoff_ms={}", r.backoff_ms));
    out.push(format!("restart.backoff_max_ms={}", r.backoff_max_ms));
    out.push(format!("restart.rate={}", r.rate));
    out.push(format!("supervise.workloads={}", p.supervise.workloads));
    if let Some(uid) = p.run.uid {
        out.push(format!("run.uid={uid}"));
    }
    if let Some(gid) = p.run.gid {
        out.push(format!("run.gid={gid}"));
    }
    out.push(format!(
        "run.allow_root={}",
        if p.run.allow_root { "yes" } else { "no" }
    ));
    out
}

/// The program as the shell would find it: a path, or a name in PATH.
fn resolvable(prog: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let runs = |p: &std::path::Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if prog.contains('/') {
        return runs(std::path::Path::new(prog));
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| runs(&d.join(prog))))
}

/// The program of a command line, or nothing for an empty one.
fn program(argv: &[String]) -> &str {
    argv.first().map_or("", String::as_str)
}

/// Replaces this process with the command; it only returns, with the reason, when the command cannot start.
fn become_command(argv: &[String]) -> Error {
    let Some((prog, rest)) = argv.split_first() else {
        return Error::Usage("there is no command to run".into());
    };
    let err = Command::new(prog).args(rest).exec();
    Error::Io(format!("cannot run {prog}: {err}"))
}

/// This process, as the kernel numbers it.
fn me() -> i32 {
    i32::try_from(std::process::id()).unwrap_or(i32::MAX)
}

fn report(rep: &resctl::domain::Report) {
    for s in &rep.set {
        println!("set={s}");
    }
    for s in &rep.unbound {
        println!("unbound={s}");
    }
}

/// What went wrong on the side (an audit line that could not be written, for one) for whoever reads the output.
fn warnings(s: &Store<'_>) {
    for w in s.take_warnings() {
        eprintln!("resctl: warning: {w}");
    }
}

/// What every command of one invocation runs against.
struct Setup<'a> {
    host: &'a dyn Host,
    layout: &'a Layout,
    policy: &'a Policy,
    total: u64,
    /// The state directory.
    dir: &'a str,
    /// `--policy P --lifecycle L --state-dir D`: how the processes that a supervisor starts find the same setup.
    globals: Vec<String>,
}

fn globals_of(policy_file: Option<&str>, lifecycle: Lifecycle, dir: &str) -> Vec<String> {
    let mut g = Vec::new();
    if let Some(p) = policy_file {
        let path = std::fs::canonicalize(p)
            .ok()
            .and_then(|f| f.into_os_string().into_string().ok())
            .unwrap_or_else(|| p.to_string());
        g.push("--policy".to_string());
        g.push(path);
    }
    g.push("--lifecycle".to_string());
    g.push(lifecycle.name().to_string());
    g.push("--state-dir".to_string());
    g.push(dir.to_string());
    g
}

fn no_env() -> Env {
    Env {
        exe: String::new(),
        globals: Vec::new(),
    }
}

/// How a supervisor starts the processes of its workload: this program again, with the options of this invocation.
/// No other command starts any.
fn env_for(cmd: &Cmd, set: &Setup<'_>) -> Result<Env> {
    if !matches!(cmd, Cmd::Supervise(..)) {
        return Ok(no_env());
    }
    let exe = std::env::current_exe()
        .map_err(|e| Error::Io(format!("cannot find this program: {e}")))?
        .into_os_string()
        .into_string()
        .map_err(|_| Error::Io("the path of this program is not valid UTF-8".into()))?;
    Ok(Env {
        exe,
        globals: set.globals.clone(),
    })
}

fn execute(inv: Invocation) -> Result<i32> {
    let Invocation {
        policy: policy_file,
        lifecycle,
        state_dir,
        cmd,
    } = inv;
    if matches!(cmd, Cmd::Help) {
        print!("{USAGE}");
        return Ok(0);
    }
    let host = RealHost;
    let layout = Layout::detect(&host, lifecycle);
    let total = mem_total(&host)?;
    if matches!(cmd, Cmd::Probe) {
        println!("memtotal={total}");
        for line in layout.describe() {
            println!("{line}");
        }
        for line in resctl::domain::oom_report(&host) {
            println!("{line}");
        }
        return Ok(0);
    }
    // A policy that does not fit this machine is refused before anything is touched, whatever the machine is.
    let policy = load_policy(&policy_file)?;
    let bound = policy.resolve(total).map_err(Error::Policy)?;
    if matches!(cmd, Cmd::Policy) {
        println!("memtotal={total}");
        for line in policy_lines(&bound) {
            println!("{line}");
        }
        return Ok(0);
    }
    let dir = state_dir.as_deref().unwrap_or(STATE_DIR);
    let set = Setup {
        host: &host,
        layout: &layout,
        policy: &policy,
        total,
        dir,
        globals: globals_of(policy_file.as_deref(), lifecycle, dir),
    };
    if cmd.mutates() {
        mutate(cmd, &set)
    } else {
        observe(&cmd, &set).map(|()| 0)
    }
}

/// The commands that only read.
fn observe(cmd: &Cmd, set: &Setup<'_>) -> Result<()> {
    let ctl = Controller::new(set.host, set.layout, set.policy, set.total)?;
    match cmd {
        Cmd::List => {
            for e in ctl.list() {
                println!("{} procs={} state={}", e.workload, e.procs, e.state);
            }
        }
        Cmd::Status(rel) => {
            for (k, v) in ctl.status(rel)? {
                println!("{k}={v}");
            }
        }
        Cmd::State(which) => {
            let Some(store) = Store::existing(set.host, set.dir, &ops::actor("cli"))? else {
                return match which {
                    Some(w) => Err(Error::Lifecycle(format!("{w} has no lifecycle record"))),
                    None => Ok(()),
                };
            };
            let (env, procs) = (no_env(), RealProcs::default());
            let ctx = Ctx {
                host: set.host,
                c: &ctl,
                s: &store,
                p: &procs,
                env: &env,
            };
            for line in supervise::state_lines(ctx, which.as_ref())? {
                println!("{line}");
            }
            warnings(&store);
        }
        Cmd::Audit(n) => {
            if let Some(store) = Store::existing(set.host, set.dir, &ops::actor("cli"))? {
                for line in store.audit_tail(*n)? {
                    println!("{line}");
                }
                warnings(&store);
            }
        }
        Cmd::Critical => {
            if let Some(store) = Store::existing(set.host, set.dir, &ops::actor("cli"))? {
                for line in ops::critical_lines(&ctl, &store)? {
                    println!("{line}");
                }
                warnings(&store);
            }
        }
        _ => {
            return Err(Error::Usage(
                "internal: not a command that only reads".into(),
            ))
        }
    }
    Ok(())
}

/// What a command that changes something leaves to do once it has returned.
enum Next {
    /// It is over, with this exit code.
    Exit(i32),
    /// This process becomes the command; the workload made for it (if one was) goes when the command cannot start.
    Become(Vec<String>, Option<Workload>),
    /// This process becomes the main process of a supervised workload.
    Main(Launch),
}

/// The commands that change something. They are refused inside a workload, run with the registry of critical
/// processes as a shield around every kill, and go through the audit trail.
fn mutate(cmd: Cmd, set: &Setup<'_>) -> Result<i32> {
    if let Cmd::Run(_, _, argv) | Cmd::Enter(_, argv) | Cmd::Supervise(_, _, _, argv) = &cmd {
        let prog = program(argv);
        if !resolvable(prog) {
            return Err(Error::Usage(format!(
                "{prog} is not a program that can run"
            )));
        }
    }
    if set.layout.mode == Mode::None {
        return Err(Error::Unsupported(format!(
            "{}: this machine has no cgroup hierarchy to hold it",
            cmd.name()
        )));
    }
    let who = ops::actor(cmd.role());
    ops::guard(set.host, set.dir, &who, cmd.name())?;
    let store = Store::open(set.host, set.dir, &who)?;
    let shield = || store.critical_pids();
    let ctl = Controller::new(set.host, set.layout, set.policy, set.total)?.with_critical(&shield);
    let env = env_for(&cmd, set)?;
    let procs = RealProcs::default();
    let ctx = Ctx {
        host: set.host,
        c: &ctl,
        s: &store,
        p: &procs,
        env: &env,
    };
    let next = dispatch(ctx, cmd);
    warnings(&store);
    match next? {
        Next::Exit(code) => Ok(code),
        Next::Become(argv, made) => {
            let err = become_command(&argv);
            if let Some(w) = made {
                let _ = ctl.attach(&Target::System, me());
                let _ = ctl.remove(&w);
                ops::note(
                    &store,
                    Event::Run,
                    &w.to_string(),
                    "failed: the command did not start",
                );
                warnings(&store);
            }
            Err(err)
        }
        Next::Main(launch) => {
            let err = supervise::exec_main(&launch);
            eprintln!("resctl: error: cannot run {}: {err}", program(&launch.argv));
            Ok(if err.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            })
        }
    }
}

fn dispatch(ctx: Ctx<'_>, cmd: Cmd) -> Result<Next> {
    let (c, s) = (ctx.c, ctx.s);
    match cmd {
        Cmd::Init => report(&ops::done(s, Event::Init, "all", "", c.init())?),
        Cmd::Create(w, l) => {
            let subject = w.to_string();
            ops::failed(s, Event::Create, &subject, ops::vacant(s, &w))?;
            report(&ops::done(
                s,
                Event::Create,
                &subject,
                "",
                c.create(&w, &l),
            )?);
            println!("created={w}");
        }
        Cmd::Run(w, l, argv) => {
            let subject = w.to_string();
            ops::failed(s, Event::Run, &subject, supervise::prelude(c, s))?;
            ops::failed(s, Event::Run, &subject, ops::vacant(s, &w))?;
            ops::failed(s, Event::Run, &subject, c.create(&w, &l))?;
            let started = ops::require(
                s,
                Event::Run,
                &subject,
                &format!("cmd={}", tidy(program(&argv))),
            )
            .and_then(|()| c.attach(&Target::Workload(w.clone()), me()));
            if started.is_err() {
                let _ = c.remove(&w);
            }
            ops::failed(s, Event::Run, &subject, started)?;
            return Ok(Next::Become(argv, Some(w)));
        }
        Cmd::Enter(t, argv) => {
            let subject = match &t {
                Target::System => "system".to_string(),
                Target::Workload(w) => w.to_string(),
            };
            ops::failed(s, Event::Enter, &subject, supervise::prelude(c, s))?;
            let started = ops::require(
                s,
                Event::Enter,
                &subject,
                &format!("cmd={}", tidy(program(&argv))),
            )
            .and_then(|()| c.attach(&t, me()));
            ops::failed(s, Event::Enter, &subject, started)?;
            if matches!(t, Target::System) {
                // What starts in `system` is a critical service: registered, so that no kill reaches it wherever it is.
                let name = ops::service_name(program(&argv), me());
                let registered = ops::protect(c, s, Some(&name), me()).map(|_| ());
                ops::failed(s, Event::Enter, &subject, registered)?;
            }
            return Ok(Next::Become(argv, None));
        }
        Cmd::Supervise(workload, limits, mode, argv) => {
            let subject = workload.to_string();
            let job = Job {
                workload,
                limits,
                argv,
                mode,
            };
            let out = ops::failed(
                s,
                Event::Supervise,
                &subject,
                supervise::supervise(ctx, &job),
            )?;
            let reason = tidy(&out.reason);
            println!(
                "phase={} gen={} restarts={} reason={reason}",
                out.phase.name(),
                out.gen,
                out.restarts
            );
            if out.code() != 0 {
                eprintln!("resctl: lifecycle: {subject} failed: {reason}");
            }
            return Ok(Next::Exit(out.code()));
        }
        Cmd::Freeze(w) => {
            let subject = w.to_string();
            ops::failed(s, Event::Freeze, &subject, supervise::prelude(c, s))?;
            ops::done(s, Event::Freeze, &subject, "", c.freeze(&w))?;
        }
        Cmd::Thaw(w) => {
            let subject = w.to_string();
            ops::failed(s, Event::Thaw, &subject, supervise::prelude(c, s))?;
            ops::done(s, Event::Thaw, &subject, "", c.thaw(&w))?;
        }
        Cmd::Stop(w, grace) => {
            let ended = ops::end(ctx, &w, Intent::Stop, grace)?;
            if let Some(text) = &ended.stop {
                println!("stop={text}");
            }
            if let Some(p) = ended.phase {
                println!("phase={}", p.name());
            }
        }
        Cmd::Kill(w) => {
            let ended = ops::end(ctx, &w, Intent::Kill, None)?;
            if let Some(p) = ended.phase {
                println!("phase={}", p.name());
            }
        }
        Cmd::Remove(w) => {
            ops::remove(ctx, &w)?;
        }
        Cmd::Recover(orphans) => {
            let orphans = orphans.unwrap_or(c.policy().orphans);
            let rec = ops::failed(s, Event::Recover, "all", supervise::recover(ctx, orphans))?;
            for (key, names) in [
                ("removed", &rec.domains.removed),
                ("killed", &rec.domains.killed),
                ("kept", &rec.domains.kept),
                ("live", &rec.domains.live),
            ] {
                for n in names {
                    println!("{key}={n}");
                }
            }
            for (name, phase) in &rec.swept {
                println!("swept={name} phase={}", phase.name());
            }
            for name in &rec.quarantined {
                println!("quarantined={name}");
            }
            if rec.healed {
                println!("healed=registry");
            }
            if rec.pruned > 0 {
                println!("pruned={}", rec.pruned);
            }
            for (name, why) in &rec.domains.stuck {
                println!("stuck={name}: {why}");
            }
            if !rec.domains.stuck.is_empty() {
                return Err(Error::Io(format!(
                    "{} workload domain(s) could not be removed",
                    rec.domains.stuck.len()
                )));
            }
        }
        Cmd::Teardown => {
            let down = ops::failed(s, Event::Teardown, "all", supervise::teardown(ctx))?;
            for k in &down.domains.kept {
                println!("kept={k}");
            }
            for name in &down.forgotten {
                println!("forgotten={name}");
            }
            for (name, why) in &down.domains.stuck {
                println!("stuck={name}: {why}");
            }
            if !down.domains.stuck.is_empty() {
                return Err(Error::Io(format!(
                    "{} workload domain(s) could not be removed",
                    down.domains.stuck.len()
                )));
            }
            println!("teardown=done");
        }
        Cmd::Reset(w) => {
            let was = ops::failed(s, Event::Reset, &w.to_string(), supervise::reset(ctx, &w))?;
            println!("reset={w} was={}", was.map_or("unrecorded", Phase::name));
        }
        Cmd::Protect(pid, name) => {
            let subject = format!("pid{pid}");
            let p = ops::failed(
                s,
                Event::Protect,
                &subject,
                ops::protect(c, s, name.as_deref(), pid),
            )?;
            println!(
                "protected={} pid={} placed=system moved={}",
                p.name,
                p.pid,
                if p.moved { "yes" } else { "no" }
            );
        }
        Cmd::Unprotect(pid) => {
            let subject = format!("pid{pid}");
            let was = ops::failed(s, Event::Unprotect, &subject, ops::unprotect(s, pid))?;
            println!(
                "{}={pid}",
                if was { "unprotected" } else { "not-protected" }
            );
        }
        Cmd::HelperEnter(w, argv) => {
            if !matches!(s.sup_state(&w)?, Sup::Alive) {
                return Err(Error::Refused(format!("no supervisor runs {w}")));
            }
            return Ok(Next::Main(supervise::prepare_main(ctx.host, c, &w, &argv)?));
        }
        Cmd::HelperReap(w) => {
            if let Err(e) = supervise::reap(ctx, &w) {
                ops::note(
                    s,
                    Event::Warn,
                    &w.to_string(),
                    &format!("the reaper failed: {}", tidy(&e.to_string())),
                );
            }
        }
        Cmd::Help
        | Cmd::Probe
        | Cmd::Policy
        | Cmd::List
        | Cmd::Status(_)
        | Cmd::State(_)
        | Cmd::Audit(_)
        | Cmd::Critical => {
            return Err(Error::Usage(
                "internal: not a command that changes anything".into(),
            ))
        }
    }
    Ok(Next::Exit(0))
}

fn main() -> ExitCode {
    let mut args = Vec::new();
    for a in std::env::args_os().skip(1) {
        match a.into_string() {
            Ok(a) => args.push(a),
            Err(_) => {
                eprintln!("resctl: usage: an argument is not valid UTF-8\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let inv = match parse(&args) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("resctl: usage: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let helper = inv.cmd.helper();
    match execute(inv) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("resctl: {}: {e}", e.label());
            // A supervisor that started a helper reads 125 as "the helper could not start the workload".
            let code = if helper { 125 } else { e.code() };
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn cmd(s: &str) -> Cmd {
        parse(&args(s)).unwrap().cmd
    }

    fn bad(s: &str) -> String {
        parse(&args(s)).unwrap_err()
    }

    #[test]
    fn global_options_come_first() {
        let i = parse(&args("--policy /p --lifecycle v1 init")).unwrap();
        assert_eq!(
            i,
            Invocation {
                policy: Some("/p".into()),
                lifecycle: Lifecycle::V1,
                state_dir: None,
                cmd: Cmd::Init
            }
        );
        let i = parse(&args("--lifecycle=v2 --policy=/q probe")).unwrap();
        assert_eq!(
            (i.policy.as_deref(), i.lifecycle, i.cmd),
            (Some("/q"), Lifecycle::V2, Cmd::Probe)
        );
        let i = parse(&args("--state-dir /s list")).unwrap();
        assert_eq!((i.state_dir.as_deref(), i.cmd), (Some("/s"), Cmd::List));
        let i = parse(&args("--state-dir=/t --policy /p list")).unwrap();
        assert_eq!(
            (i.state_dir.as_deref(), i.policy.as_deref()),
            (Some("/t"), Some("/p"))
        );
        assert!(bad("--lifecycle sideways init").contains("not a lifecycle"));
        assert!(bad("--policy").contains("needs a value"));
        assert!(bad("--state-dir").contains("needs a value"));
        assert!(bad("").contains("no command"));
    }

    #[test]
    fn create_reads_limits_and_run_reads_a_command() {
        let Cmd::Create(w, l) = cmd("create batch/job1 --memory-max 64M --pids-max=16 --cpu-max 50% --cpu-weight 20 --swap allow") else {
            panic!("not a create");
        };
        assert_eq!(w.to_string(), "batch/job1");
        assert_eq!(l.memory_max, Some(Mem::Bytes(64 << 20)));
        assert_eq!(l.pids_max, Some(Num::N(16)));
        assert_eq!(l.cpu_max, Some(Num::N(50)));
        assert_eq!(l.cpu_weight, Some(20));
        assert_eq!(l.swap_allowed, Some(true));
        let Cmd::Run(w, l, argv) = cmd("run interactive/a --memory-high 8M -- sh -c exit") else {
            panic!("not a run")
        };
        assert_eq!(
            (w.to_string(), l.memory_high, argv),
            (
                "interactive/a".into(),
                Some(Mem::Bytes(8 << 20)),
                args("sh -c exit")
            )
        );
    }

    #[test]
    fn supervise_reads_limits_a_restart_mode_and_a_command() {
        let Cmd::Supervise(w, l, mode, argv) =
            cmd("supervise batch/job --pids-max 8 --restart always -- sh -c exit")
        else {
            panic!("not a supervise")
        };
        assert_eq!(
            (w.to_string(), l.pids_max, mode, argv),
            (
                "batch/job".into(),
                Some(Num::N(8)),
                Some(RestartMode::Always),
                args("sh -c exit")
            )
        );
        let Cmd::Supervise(_, _, mode, _) = cmd("supervise batch/job -- true") else {
            panic!()
        };
        assert_eq!(mode, None);
        let Cmd::Supervise(_, _, mode, _) = cmd("supervise batch/job --restart=never -- true")
        else {
            panic!()
        };
        assert_eq!(mode, Some(RestartMode::Never));
        assert!(bad("supervise batch/x").contains("needs a command"));
        assert!(bad("supervise batch/x --restart sometimes -- true")
            .contains("not never, on-failure or always"));
        assert!(bad("supervise batch/x --restart").contains("needs a value"));
        assert!(bad("run batch/x --restart always -- true").contains("unknown option"));
        assert!(bad("create batch/x --restart always").contains("unknown option"));
    }

    #[test]
    fn io_limits_are_repeatable_values() {
        let i = parse(&[
            "create".into(),
            "batch/x".into(),
            "--io".into(),
            "8:0 wbps=1M rbps=2M".into(),
            "--io".into(),
            "/dev/sda wiops=100".into(),
        ])
        .unwrap();
        let Cmd::Create(_, l) = i.cmd else { panic!() };
        assert_eq!(l.io.len(), 2);
    }

    #[test]
    fn refusals_name_the_mistake() {
        assert!(bad("create").contains("CLASS/ID"));
        assert!(bad("create batch").contains("not CLASS/ID"));
        assert!(bad("create system/x").contains("not a workload class"));
        assert!(bad("create batch/x -- true").contains("takes no command"));
        assert!(bad("run batch/x").contains("needs a command"));
        assert!(bad("run batch/x --memory-max 12").contains("below the smallest"));
        assert!(bad("create batch/x --bogus 1").contains("unknown option"));
        assert!(bad("create batch/x batch/y").contains("unexpected argument"));
        assert!(bad("create batch/x --memory-max").contains("needs a value"));
        assert!(bad("kill").contains("CLASS/ID"));
        assert!(bad("kill system").contains("not CLASS/ID"));
        assert!(bad("kill workload/x").contains("not a workload class"));
        assert!(bad("frobnicate").contains("unknown command"));
        assert!(bad("init now").contains("unexpected argument"));
    }

    #[test]
    fn enter_takes_system_or_a_workload() {
        assert_eq!(
            cmd("enter system -- sleep 1"),
            Cmd::Enter(Target::System, args("sleep 1"))
        );
        let Cmd::Enter(Target::Workload(w), argv) = cmd("enter batch/a -- id") else {
            panic!()
        };
        assert_eq!((w.to_string(), argv), ("batch/a".into(), args("id")));
        assert!(bad("enter system sleep").contains("needs --"));
        assert!(bad("enter system --").contains("needs a command"));
        assert!(bad("enter").contains("target"));
    }

    #[test]
    fn stop_status_and_recover_options() {
        let Cmd::Stop(w, g) = cmd("stop batch/a --grace 250ms") else {
            panic!()
        };
        assert_eq!((w.to_string(), g), ("batch/a".into(), Some(250)));
        let Cmd::Stop(_, g) = cmd("stop batch/a") else {
            panic!()
        };
        assert_eq!(g, None);
        assert_eq!(cmd("status"), Cmd::Status(WORKLOAD.into()));
        assert_eq!(cmd("status system"), Cmd::Status(SYSTEM.into()));
        assert_eq!(
            cmd("status batch"),
            Cmd::Status(format!("{WORKLOAD}/batch"))
        );
        assert_eq!(
            cmd("status batch/a"),
            Cmd::Status(format!("{WORKLOAD}/batch/a"))
        );
        assert!(bad("status ../x").contains("not a usable"));
        assert!(bad("status ..").contains("not system"));
        assert_eq!(cmd("recover"), Cmd::Recover(None));
        assert_eq!(
            cmd("recover --orphans keep"),
            Cmd::Recover(Some(Orphans::Keep))
        );
        assert!(bad("recover --orphans maybe").contains("not kill or keep"));
        assert!(bad("recover now").contains("unexpected argument"));
    }

    #[test]
    fn the_lifecycle_commands_read_a_workload_or_a_count() {
        assert_eq!(cmd("state"), Cmd::State(None));
        let Cmd::State(Some(w)) = cmd("state batch/a") else {
            panic!()
        };
        assert_eq!(w.to_string(), "batch/a");
        assert!(bad("state batch").contains("not CLASS/ID"));
        assert!(bad("state batch/a batch/b").contains("unexpected argument"));
        assert_eq!(cmd("audit"), Cmd::Audit(AUDIT_DEFAULT));
        assert_eq!(cmd("audit 5"), Cmd::Audit(5));
        for n in ["0", "99999", "x", "-3"] {
            assert!(
                bad(&format!("audit {n}")).contains("from 1 to"),
                "audit {n}"
            );
        }
        assert!(bad("audit 3 4").contains("unexpected argument"));
        assert_eq!(cmd("critical"), Cmd::Critical);
        assert!(bad("critical now").contains("unexpected argument"));
        let Cmd::Reset(w) = cmd("reset batch/a") else {
            panic!()
        };
        assert_eq!(w.to_string(), "batch/a");
        assert!(bad("reset").contains("CLASS/ID"));
    }

    #[test]
    fn the_protection_commands_read_a_process_and_a_name() {
        assert_eq!(cmd("protect 42"), Cmd::Protect(42, None));
        assert_eq!(
            cmd("protect 42 --name db"),
            Cmd::Protect(42, Some("db".into()))
        );
        assert_eq!(
            cmd("protect --name=db 42"),
            Cmd::Protect(42, Some("db".into()))
        );
        assert!(bad("protect").contains("process id"));
        for p in ["0", "-3", "x"] {
            assert!(
                bad(&format!("protect {p}")).contains("not a process id"),
                "protect {p}"
            );
        }
        assert!(bad("protect 1 2").contains("unexpected argument"));
        assert!(bad("protect 1 --bogus").contains("unknown option"));
        assert!(bad("protect 1 --name").contains("needs a value"));
        assert_eq!(cmd("unprotect 42"), Cmd::Unprotect(42));
        assert!(bad("unprotect").contains("process id"));
        assert!(bad("unprotect 42 43").contains("unexpected argument"));
        assert!(bad("unprotect x").contains("not a process id"));
    }

    #[test]
    fn the_commands_of_a_supervisor_are_not_typed_ones() {
        let Cmd::HelperEnter(w, argv) = cmd("__enter batch/a -- sleep 1") else {
            panic!()
        };
        assert_eq!((w.to_string(), argv), ("batch/a".into(), args("sleep 1")));
        assert!(bad("__enter batch/a sleep").contains("needs --"));
        assert!(bad("__enter batch/a --").contains("needs a command"));
        assert!(bad("__enter").contains("needs a workload"));
        let Cmd::HelperReap(w) = cmd("__reap batch/a") else {
            panic!()
        };
        assert_eq!(w.to_string(), "batch/a");
        assert!(bad("__reap").contains("CLASS/ID"));
        assert!(!USAGE.contains("__enter") && !USAGE.contains("__reap"));
    }

    /// Every command: how it is typed, its name, whether it changes anything, and the part it plays.
    const SAMPLES: [(&str, &str, bool, &str); 25] = [
        ("help", "help", false, "cli"),
        ("probe", "probe", false, "cli"),
        ("policy", "policy", false, "cli"),
        ("init", "init", true, "cli"),
        ("create batch/a", "create", true, "cli"),
        ("run batch/a -- true", "run", true, "cli"),
        ("supervise batch/a -- true", "supervise", true, "super"),
        ("enter system -- true", "enter", true, "cli"),
        ("freeze batch/a", "freeze", true, "cli"),
        ("thaw batch/a", "thaw", true, "cli"),
        ("kill batch/a", "kill", true, "cli"),
        ("remove batch/a", "remove", true, "cli"),
        ("stop batch/a", "stop", true, "cli"),
        ("list", "list", false, "cli"),
        ("status", "status", false, "cli"),
        ("state", "state", false, "cli"),
        ("audit", "audit", false, "cli"),
        ("critical", "critical", false, "cli"),
        ("recover", "recover", true, "cli"),
        ("teardown", "teardown", true, "cli"),
        ("protect 42", "protect", true, "cli"),
        ("unprotect 42", "unprotect", true, "cli"),
        ("reset batch/a", "reset", true, "cli"),
        ("__enter batch/a -- true", "__enter", true, "main"),
        ("__reap batch/a", "__reap", true, "reap"),
    ];

    #[test]
    fn every_command_is_classified_and_the_usage_lists_exactly_the_typed_ones() {
        for (line, name, mutates, role) in SAMPLES {
            let c = cmd(line);
            assert_eq!(
                (c.name(), c.mutates(), c.role()),
                (name, mutates, role),
                "{line}"
            );
            assert_eq!(c.helper(), name.starts_with("__"), "{line}");
        }
        let typed: Vec<&str> = SAMPLES
            .iter()
            .map(|s| s.1)
            .filter(|n| !["help", "__enter", "__reap"].contains(n))
            .collect();
        let mut listed = Vec::new();
        for l in USAGE.lines() {
            if let Some(rest) = l.strip_prefix("  ").filter(|r| !r.starts_with(' ')) {
                listed.extend(rest.split_whitespace().next().unwrap_or("").split('|'));
            }
        }
        for n in &typed {
            assert!(listed.contains(n), "{n} is not in the usage");
        }
        for n in &listed {
            assert!(typed.contains(n), "{n} is in the usage, without a sample");
        }
        assert!(USAGE.contains(&format!("{AUDIT_DEFAULT} by default")));
    }

    #[test]
    fn the_built_in_policy_is_valid_and_fits_small_and_large_machines() {
        let p = Policy::parse(DEFAULT_POLICY).unwrap();
        for gib in [1u64, 2, 16, 512] {
            let bound = p
                .resolve(gib << 30)
                .unwrap_or_else(|e| panic!("{gib} GiB: {e:?}"));
            let lines = policy_lines(&bound);
            assert!(lines.iter().any(|l| l.starts_with("workload.memory.max=")));
            assert!(
                lines.iter().all(|l| !l.contains('%')),
                "percentages are bound: {lines:?}"
            );
            for want in [
                "restart.mode=on-failure",
                "restart.max=3",
                "restart.window_ms=60000",
                "restart.total=10",
                "restart.backoff_ms=500",
                "restart.backoff_max_ms=30000",
                "restart.rate=30",
                "supervise.workloads=16",
                "run.uid=65534",
                "run.gid=65534",
                "run.allow_root=no",
            ] {
                assert!(
                    lines.iter().any(|l| l.as_str() == want),
                    "{gib} GiB: {want} in {lines:?}"
                );
            }
        }
    }

    #[test]
    fn a_program_is_found_by_path_or_by_name() {
        assert!(resolvable("/bin/sh"));
        assert!(!resolvable("/nonexistent/program"));
        assert!(!resolvable("no-such-program-anywhere"));
        assert!(!resolvable("/"));
    }

    #[test]
    fn the_processes_of_a_supervisor_get_the_setup_of_the_command() {
        assert_eq!(
            globals_of(None, Lifecycle::Auto, "/run/x"),
            args("--lifecycle auto --state-dir /run/x")
        );
        assert_eq!(
            globals_of(Some("/nonexistent/p"), Lifecycle::V2, "/s"),
            args("--policy /nonexistent/p --lifecycle v2 --state-dir /s")
        );
        assert_eq!(
            globals_of(Some("/"), Lifecycle::V1, "/s"),
            args("--policy / --lifecycle v1 --state-dir /s")
        );
    }

    #[test]
    fn the_commands_a_supervisor_spawns_read_back_as_they_were_meant() {
        let env = Env {
            exe: "/bin/resctl".into(),
            globals: globals_of(Some("/"), Lifecycle::V2, "/run/x"),
        };
        let w = Workload::parse("batch/a").unwrap();
        let argv = vec!["sh".to_string(), "-c".into(), "echo a; echo b -- c".into()];
        let i = parse(&env.enter_spec(&w, &argv).args).unwrap();
        assert_eq!(
            (i.policy.as_deref(), i.lifecycle, i.state_dir.as_deref()),
            (Some("/"), Lifecycle::V2, Some("/run/x"))
        );
        assert_eq!(i.cmd, Cmd::HelperEnter(w.clone(), argv));
        let i = parse(&env.reap_spec(&w).args).unwrap();
        assert_eq!(i.cmd, Cmd::HelperReap(w));
    }
}
