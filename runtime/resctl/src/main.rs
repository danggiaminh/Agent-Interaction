//! `resctl`: the command line of the resource controller. See ../README.md.

use resctl::domain::{Controller, Error, Result, Stopped, Target, Workload, SYSTEM, WORKLOAD};
use resctl::fs::{Host, RealHost};
use resctl::layout::{Layout, Lifecycle};
use resctl::policy::{
    parse_bool, parse_cpus, parse_duration, parse_io, parse_weight, valid_name, Limits, Mem, Num,
    Orphans, Policy,
};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

const DEFAULT_POLICY: &str = include_str!("../policy/default.policy");

const USAGE: &str = "\
usage: resctl [--policy FILE] [--lifecycle auto|v1|v2] COMMAND

  probe                       the cgroup layout and which feature each hierarchy serves (changes nothing)
  policy                      the policy as bound to this machine (changes nothing)
  init                        create the domains and set their limits (safe to repeat)
  create CLASS/ID [LIMITS]    create a workload domain
  run CLASS/ID [LIMITS] -- CMD...   create a workload domain and become CMD inside it
  enter system|CLASS/ID -- CMD...   become CMD inside an existing domain
  freeze|thaw|kill|remove CLASS/ID
  stop CLASS/ID [--grace 5s]  SIGTERM, then the kill after the grace period
  list                        every workload domain
  status [DOMAIN]             counters of system, workload, a CLASS or a CLASS/ID
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
    Enter(Target, Vec<String>),
    Freeze(Workload),
    Thaw(Workload),
    Kill(Workload),
    Remove(Workload),
    Stop(Workload, Option<u64>),
    List,
    Status(String),
    Recover(Option<Orphans>),
    Teardown,
}

#[derive(Debug, PartialEq)]
struct Invocation {
    policy: Option<String>,
    lifecycle: Lifecycle,
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

/// A workload, its limit flags and, after `--`, a command.
fn workload_args<'a>(
    it: &mut impl Iterator<Item = &'a str>,
) -> std::result::Result<(Workload, Limits, Vec<String>), String> {
    let mut id = None;
    let mut l = Limits::default();
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
            f if f.starts_with("--") => return Err(format!("unknown option {f}")),
            _ if id.is_none() => id = Some(Workload::parse(a).map_err(|e| e.to_string())?),
            _ => return Err(format!("unexpected argument `{a}`")),
        }
    }
    let id = id.ok_or("a workload (CLASS/ID) is needed")?;
    Ok((id, l, argv))
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

fn parse(args: &[String]) -> std::result::Result<Invocation, String> {
    let mut it = args.iter().map(String::as_str);
    let mut policy = None;
    let mut lifecycle = Lifecycle::Auto;
    let name = loop {
        let a = it.next().ok_or("no command")?;
        let (flag, inline) = split_flag(a);
        match flag {
            "--policy" => policy = Some(take(&mut it, flag, inline)?),
            "--lifecycle" => lifecycle = Lifecycle::parse(&take(&mut it, flag, inline)?)?,
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
        "create" | "run" => {
            let (w, l, argv) = workload_args(it)?;
            match (name, argv.is_empty()) {
                ("create", true) => Cmd::Create(w, l),
                ("create", false) => return Err("create takes no command (use run)".into()),
                (_, false) => Cmd::Run(w, l, argv),
                (_, true) => return Err("run needs a command after --".into()),
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
        other => return Err(format!("unknown command `{other}`")),
    };
    Ok(Invocation {
        policy,
        lifecycle,
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

fn become_command(argv: &[String]) -> Error {
    let err = Command::new(&argv[0]).args(&argv[1..]).exec();
    Error::Io(format!("cannot run {}: {err}", argv[0]))
}

fn pid() -> i32 {
    std::process::id() as i32
}

fn report(rep: &resctl::domain::Report) {
    for s in &rep.set {
        println!("set={s}");
    }
    for s in &rep.unbound {
        println!("unbound={s}");
    }
}

fn execute(inv: Invocation) -> Result<()> {
    let host = RealHost;
    let layout = Layout::detect(&host, inv.lifecycle);
    let total = mem_total(&host)?;
    match inv.cmd {
        Cmd::Help => {
            print!("{USAGE}");
            return Ok(());
        }
        Cmd::Probe => {
            println!("memtotal={total}");
            for line in layout.describe() {
                println!("{line}");
            }
            return Ok(());
        }
        Cmd::Policy => {
            let policy = load_policy(&inv.policy)?
                .resolve(total)
                .map_err(Error::Policy)?;
            println!("memtotal={total}");
            for line in policy_lines(&policy) {
                println!("{line}");
            }
            return Ok(());
        }
        _ => {}
    }
    let policy = load_policy(&inv.policy)?;
    let ctl = Controller::new(&host, &layout, &policy, total)?;
    match inv.cmd {
        Cmd::Help | Cmd::Probe | Cmd::Policy => unreachable!("handled above"),
        Cmd::Init => report(&ctl.init()?),
        Cmd::Create(w, l) => {
            report(&ctl.create(&w, &l)?);
            println!("created={w}");
        }
        Cmd::Run(w, l, argv) => {
            if !resolvable(&argv[0]) {
                return Err(Error::Usage(format!(
                    "{} is not a program that can run",
                    argv[0]
                )));
            }
            ctl.create(&w, &l)?;
            if let Err(e) = ctl.attach(&Target::Workload(w.clone()), pid()) {
                let _ = ctl.remove(&w);
                return Err(e);
            }
            return Err(become_command(&argv));
        }
        Cmd::Enter(t, argv) => {
            if !resolvable(&argv[0]) {
                return Err(Error::Usage(format!(
                    "{} is not a program that can run",
                    argv[0]
                )));
            }
            ctl.attach(&t, pid())?;
            return Err(become_command(&argv));
        }
        Cmd::Freeze(w) => ctl.freeze(&w)?,
        Cmd::Thaw(w) => ctl.thaw(&w)?,
        Cmd::Kill(w) => ctl.kill(&w)?,
        Cmd::Remove(w) => ctl.remove(&w)?,
        Cmd::Stop(w, grace) => {
            let how = match ctl.stop(&w, grace.unwrap_or(ctl.policy().stop_grace_ms))? {
                Stopped::Empty => "empty".to_string(),
                Stopped::Graceful => "graceful".to_string(),
                Stopped::Killed(n) => format!("killed:{n}"),
            };
            println!("stop={how}");
        }
        Cmd::List => {
            for e in ctl.list() {
                println!("{} procs={} state={}", e.workload, e.procs, e.state);
            }
        }
        Cmd::Status(rel) => {
            for (k, v) in ctl.status(&rel)? {
                println!("{k}={v}");
            }
        }
        Cmd::Recover(orphans) => {
            let done = ctl.recover(orphans.unwrap_or(ctl.policy().orphans))?;
            for (key, names) in [
                ("removed", &done.removed),
                ("killed", &done.killed),
                ("kept", &done.kept),
            ] {
                for n in names {
                    println!("{key}={n}");
                }
            }
        }
        Cmd::Teardown => {
            for k in ctl.teardown()? {
                println!("kept={k}");
            }
            println!("teardown=done");
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let inv = match parse(&args) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("resctl: usage: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match execute(inv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("resctl: {}: {e}", e.label());
            ExitCode::from(e.code() as u8)
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
                cmd: Cmd::Init
            }
        );
        let i = parse(&args("--lifecycle=v2 --policy=/q probe")).unwrap();
        assert_eq!(
            (i.policy.as_deref(), i.lifecycle, i.cmd),
            (Some("/q"), Lifecycle::V2, Cmd::Probe)
        );
        assert!(bad("--lifecycle sideways init").contains("not a lifecycle"));
        assert!(bad("--policy").contains("needs a value"));
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
        }
    }

    #[test]
    fn a_program_is_found_by_path_or_by_name() {
        assert!(resolvable("/bin/sh"));
        assert!(!resolvable("/nonexistent/program"));
        assert!(!resolvable("no-such-program-anywhere"));
        assert!(!resolvable("/"));
    }
}
