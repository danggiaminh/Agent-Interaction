//! The resource policy: what every domain may use. A small INI-like text file, parsed and checked without touching the
//! host (`Policy::parse`), then bound to the amount of memory of the machine it runs on (`Policy::resolve`).
//!
//! ```text
//! [system]            critical services; never in a kill domain
//! cpu.weight = 1000
//! memory.reserve = 128M
//! [workload]          the ceiling on every workload together (memory.max and pids.max are required)
//! memory.max = 75%
//! pids.max = 4096
//! [class.batch]       the ceiling on one class, and its share of the CPU against the other classes
//! cpu.weight = 100
//! [limits.batch]      what one workload of the class gets unless its start says otherwise
//! memory.max = 1G
//! pids.max = 256
//! ```

use std::collections::{BTreeMap, BTreeSet};

/// Smallest memory limit that is accepted: the kernel rounds limits to pages, and below this a process cannot start.
pub const MIN_MEMORY: u64 = 1 << 20;
/// The scheduling period of every CPU quota, in microseconds.
pub const CPU_PERIOD_US: u64 = 100_000;

/// A memory amount: unlimited, bytes, or a percentage of the memory of the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mem {
    Max,
    Bytes(u64),
    Percent(u32),
}

/// A count (processes) or a percentage of one CPU (150 = one and a half CPUs), or unlimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Num {
    Max,
    N(u64),
}

/// A block device: major and minor number, or a path that is resolved when the limit is applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dev {
    Ids(u32, u32),
    Path(String),
}

/// Throttle of one block device: bytes and operations per second, `None` = not throttled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoLimit {
    pub dev: Dev,
    pub rbps: Option<u64>,
    pub wbps: Option<u64>,
    pub riops: Option<u64>,
    pub wiops: Option<u64>,
}

/// Limits of one domain. `None` leaves the kernel's default (the limit of the ancestors still applies).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    pub memory_max: Option<Mem>,
    pub memory_high: Option<Mem>,
    pub swap_allowed: Option<bool>,
    pub oom_group: Option<bool>,
    pub pids_max: Option<Num>,
    pub cpu_max: Option<Num>,
    pub cpu_weight: Option<u32>,
    pub io: Vec<IoLimit>,
    pub cpus: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SystemPolicy {
    pub cpu_weight: Option<u32>,
    pub memory_reserve: Option<Mem>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Class {
    /// The ceiling on the class as a whole (memory.max, pids.max, cpu.max) and its CPU weight against the other classes.
    pub aggregate: Limits,
    /// What each workload of the class gets by default.
    pub workload: Limits,
}

/// What `recover` does with a workload that is still running but has no supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orphans {
    Kill,
    Keep,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    pub system: SystemPolicy,
    pub aggregate: Limits,
    pub classes: BTreeMap<String, Class>,
    pub stop_grace_ms: u64,
    pub orphans: Orphans,
}

impl Mem {
    pub fn parse(s: &str) -> Result<Mem, String> {
        if s == "max" {
            return Ok(Mem::Max);
        }
        if let Some(p) = s.strip_suffix('%') {
            return match p.parse::<u32>() {
                Ok(p) if (1..=100).contains(&p) => Ok(Mem::Percent(p)),
                _ => Err(format!("`{s}` is not a percentage between 1% and 100%")),
            };
        }
        let b = parse_size(s)?;
        if b < MIN_MEMORY {
            return Err(format!("`{s}` is below the smallest memory limit (1M)"));
        }
        Ok(Mem::Bytes(b))
    }

    /// Bytes, or `None` for unlimited.
    pub fn bytes(self, total: u64) -> Option<u64> {
        match self {
            Mem::Max => None,
            Mem::Bytes(b) => Some(b),
            Mem::Percent(p) => Some((u128::from(total) * u128::from(p) / 100) as u64),
        }
    }
}

impl Num {
    pub fn parse(s: &str, what: &str, percent: bool) -> Result<Num, String> {
        if s == "max" {
            return Ok(Num::Max);
        }
        let digits = if percent {
            s.strip_suffix('%')
                .ok_or(format!("`{s}` is not a percentage of one CPU like 150%"))?
        } else {
            s
        };
        match digits.parse::<u64>() {
            Ok(n) if n >= 1 && n <= if percent { 100_000 } else { 4_194_304 } => Ok(Num::N(n)),
            _ => Err(format!("`{s}` is not a valid {what}")),
        }
    }
}

/// Bytes from `64M`, `64MiB`, `2G`, `4096` (powers of 1024).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s
        .strip_suffix("iB")
        .or_else(|| s.strip_suffix('B'))
        .unwrap_or(s);
    let (digits, shift) = match t.chars().last() {
        Some('K' | 'k') => (&t[..t.len() - 1], 10),
        Some('M' | 'm') => (&t[..t.len() - 1], 20),
        Some('G' | 'g') => (&t[..t.len() - 1], 30),
        Some('T' | 't') => (&t[..t.len() - 1], 40),
        _ => (t, 0),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(1 << shift))
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("`{s}` is not a size like 64M, 2G or 4096"))
}

/// Milliseconds from `500ms`, `5s`, `2m` or a plain number of seconds.
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let (digits, mult) = if let Some(d) = s.strip_suffix("ms") {
        (d, 1)
    } else if let Some(d) = s.strip_suffix('s') {
        (d, 1000)
    } else if let Some(d) = s.strip_suffix('m') {
        (d, 60_000)
    } else {
        (s, 1000)
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|&n| n <= 3_600_000)
        .ok_or_else(|| format!("`{s}` is not a duration like 500ms, 5s or 2m (at most 60m)"))
}

pub fn parse_weight(s: &str) -> Result<u32, String> {
    match s.parse::<u32>() {
        Ok(w) if (1..=10_000).contains(&w) => Ok(w),
        _ => Err(format!("`{s}` is not a weight between 1 and 10000")),
    }
}

pub fn parse_cpus(s: &str) -> Result<String, String> {
    let ok = !s.is_empty()
        && s.split(',').all(|part| {
            let mut ends = part.splitn(2, '-');
            let a = ends.next().and_then(|x| x.parse::<u32>().ok());
            match (a, ends.next()) {
                (Some(_), None) => true,
                (Some(a), Some(b)) => b.parse::<u32>().is_ok_and(|b| a <= b),
                _ => false,
            }
        });
    if ok {
        Ok(s.to_string())
    } else {
        Err(format!("`{s}` is not a CPU list like 0-1,3"))
    }
}

pub fn parse_bool(s: &str) -> Result<bool, String> {
    match s {
        "yes" | "true" => Ok(true),
        "no" | "false" => Ok(false),
        _ => Err(format!("`{s}` is not yes or no")),
    }
}

/// `DEVICE rbps=N wbps=N riops=N wiops=N` where DEVICE is MAJOR:MINOR or a path.
pub fn parse_io(s: &str) -> Result<IoLimit, String> {
    let mut words = s.split_whitespace();
    let dev = words.next().ok_or("io.max needs a device")?;
    let dev = match dev.split_once(':') {
        Some((a, b)) if !dev.starts_with('/') => match (a.parse::<u32>(), b.parse::<u32>()) {
            (Ok(a), Ok(b)) => Dev::Ids(a, b),
            _ => return Err(format!("`{dev}` is not MAJOR:MINOR or a device path")),
        },
        _ if dev.starts_with('/') => Dev::Path(dev.to_string()),
        _ => return Err(format!("`{dev}` is not MAJOR:MINOR or a device path")),
    };
    let mut io = IoLimit {
        dev,
        rbps: None,
        wbps: None,
        riops: None,
        wiops: None,
    };
    for w in words {
        let (k, v) = w.split_once('=').ok_or(format!("`{w}` is not key=value"))?;
        let slot = match k {
            "rbps" => &mut io.rbps,
            "wbps" => &mut io.wbps,
            "riops" => &mut io.riops,
            "wiops" => &mut io.wiops,
            _ => return Err(format!("unknown io.max key `{k}` (rbps wbps riops wiops)")),
        };
        if slot.is_some() {
            return Err(format!("io.max sets {k} twice"));
        }
        *slot = Some(parse_size(v)?);
    }
    if io.rbps.is_none() && io.wbps.is_none() && io.riops.is_none() && io.wiops.is_none() {
        return Err("io.max sets no rate".into());
    }
    Ok(io)
}

/// A class or workload name: what becomes a directory name.
pub fn valid_name(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !s.starts_with('-')
}

impl Limits {
    /// The limits of `over` where it has any, else `self`: a start overriding the defaults of its class.
    pub fn overlay(&self, over: &Limits) -> Limits {
        Limits {
            memory_max: over.memory_max.or(self.memory_max),
            memory_high: over.memory_high.or(self.memory_high),
            swap_allowed: over.swap_allowed.or(self.swap_allowed),
            oom_group: over.oom_group.or(self.oom_group),
            pids_max: over.pids_max.or(self.pids_max),
            cpu_max: over.cpu_max.or(self.cpu_max),
            cpu_weight: over.cpu_weight.or(self.cpu_weight),
            io: if over.io.is_empty() {
                self.io.clone()
            } else {
                over.io.clone()
            },
            cpus: over.cpus.clone().or_else(|| self.cpus.clone()),
        }
    }

    /// Percentages of memory become bytes.
    pub fn resolve(&mut self, mem_total: u64) {
        for m in [&mut self.memory_max, &mut self.memory_high]
            .into_iter()
            .flatten()
        {
            if let Mem::Percent(_) = m {
                *m = m
                    .bytes(mem_total)
                    .map_or(Mem::Max, |b| Mem::Bytes(b.max(MIN_MEMORY)));
            }
        }
    }

    /// The problems of these (resolved) limits against the limits of the domain that contains them.
    pub fn check_within(&self, ceiling: &Limits, what: &str) -> Vec<String> {
        let mut bad = Vec::new();
        let mut within = |name: &str, own: Option<u64>, unlimited: bool, up: Option<u64>| {
            let Some(up) = up else { return };
            if unlimited {
                bad.push(format!(
                    "{what}: {name} is unlimited but its domain is limited to {up}"
                ));
            } else if own.is_some_and(|o| o > up) {
                bad.push(format!(
                    "{what}: {name} {} is above the {up} of its domain",
                    own.unwrap_or(0)
                ));
            }
        };
        let mem = |m: Option<Mem>| match m {
            Some(Mem::Bytes(b)) => (Some(b), false),
            Some(Mem::Max) => (None, true),
            _ => (None, false),
        };
        let num = |n: Option<Num>| match n {
            Some(Num::N(v)) => (Some(v), false),
            Some(Num::Max) => (None, true),
            None => (None, false),
        };
        let up_mem = match ceiling.memory_max {
            Some(Mem::Bytes(b)) => Some(b),
            _ => None,
        };
        let (own, unl) = mem(self.memory_max);
        within("memory.max", own, unl, up_mem);
        for (name, mine, theirs) in [
            ("pids.max", self.pids_max, ceiling.pids_max),
            ("cpu.max", self.cpu_max, ceiling.cpu_max),
        ] {
            let (own, unl) = num(mine);
            within(
                name,
                own,
                unl,
                if let Some(Num::N(v)) = theirs {
                    Some(v)
                } else {
                    None
                },
            );
        }
        bad
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Section {
    System,
    Workload,
    Class(String),
    Limits(String),
    Stop,
    Recover,
}

impl Section {
    fn parse(name: &str) -> Result<Section, String> {
        let named = |prefix: &str| name.strip_prefix(prefix).map(str::to_string);
        match name {
            "system" => Ok(Section::System),
            "workload" => Ok(Section::Workload),
            "stop" => Ok(Section::Stop),
            "recover" => Ok(Section::Recover),
            _ => match (named("class."), named("limits.")) {
                (Some(c), _) if valid_name(&c) && c != "system" && c != "workload" => Ok(Section::Class(c)),
                (_, Some(c)) if valid_name(&c) => Ok(Section::Limits(c)),
                (Some(c), _) | (_, Some(c)) => Err(format!("`{c}` is not a usable class name (letters, digits, - and _; not system or workload)")),
                _ => Err(format!("unknown section [{name}] (system workload class.NAME limits.NAME stop recover)")),
            },
        }
    }
}

impl Policy {
    /// Parse and check the structure. Every problem is reported, with its line.
    pub fn parse(text: &str) -> Result<Policy, Vec<String>> {
        let mut errors = Vec::new();
        let mut p = Policy {
            system: SystemPolicy::default(),
            aggregate: Limits::default(),
            classes: BTreeMap::new(),
            stop_grace_ms: 5000,
            orphans: Orphans::Kill,
        };
        let mut declared = BTreeSet::new(); // class.NAME sections
        let mut limited = BTreeSet::new(); // limits.NAME sections
        let mut seen_sections = BTreeSet::new();
        let mut seen_keys = BTreeSet::new();
        let mut section: Option<Section> = None;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let at = n + 1;
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                match Section::parse(name.trim()) {
                    Ok(s) => {
                        if !seen_sections.insert(s.clone()) {
                            errors.push(format!("line {at}: section [{name}] appears twice"));
                        }
                        match &s {
                            Section::Class(c) => {
                                declared.insert(c.clone());
                                p.classes.entry(c.clone()).or_default();
                            }
                            Section::Limits(c) => {
                                limited.insert(c.clone());
                                p.classes.entry(c.clone()).or_default();
                            }
                            _ => {}
                        }
                        section = Some(s);
                    }
                    Err(e) => {
                        errors.push(format!("line {at}: {e}"));
                        section = None;
                    }
                }
                continue;
            }
            let Some(s) = &section else {
                errors.push(format!(
                    "line {at}: `{line}` is outside a section (or in an unusable one)"
                ));
                continue;
            };
            let Some((key, value)) = line.split_once('=') else {
                errors.push(format!("line {at}: `{line}` is not key = value"));
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if key != "io.max" && !seen_keys.insert((s.clone(), key.to_string())) {
                errors.push(format!("line {at}: {key} is set twice in its section"));
                continue;
            }
            if let Err(e) = p.set(s, key, value) {
                errors.push(format!("line {at}: {key}: {e}"));
            }
        }
        for c in &limited {
            if !declared.contains(c) {
                errors.push(format!("[limits.{c}] has no [class.{c}]"));
            }
        }
        if declared.is_empty() {
            errors.push("no [class.NAME] section: workloads belong to a class".into());
        }
        for (name, c) in &p.classes {
            if declared.contains(name) {
                let w = &c.workload;
                if !matches!(w.memory_max, Some(Mem::Bytes(_) | Mem::Percent(_)))
                    || !matches!(w.pids_max, Some(Num::N(_)))
                {
                    errors.push(format!("[limits.{name}] must set memory.max and pids.max: every workload is bounded in memory and processes"));
                }
            }
        }
        if !matches!(
            p.aggregate.memory_max,
            Some(Mem::Bytes(_) | Mem::Percent(_))
        ) || !matches!(p.aggregate.pids_max, Some(Num::N(_)))
        {
            errors.push("[workload] must set memory.max and pids.max: they keep the workloads away from the memory and processes of system".into());
        }
        if errors.is_empty() {
            Ok(p)
        } else {
            Err(errors)
        }
    }

    fn set(&mut self, s: &Section, key: &str, v: &str) -> Result<(), String> {
        let weight = |v: &str| parse_weight(v);
        match (s, key) {
            (Section::System, "cpu.weight") => self.system.cpu_weight = Some(weight(v)?),
            (Section::System, "memory.reserve") => {
                self.system.memory_reserve = Some(Mem::parse(v)?)
            }
            (Section::Stop, "grace") => self.stop_grace_ms = parse_duration(v)?,
            (Section::Recover, "orphans") => {
                self.orphans = match v {
                    "kill" => Orphans::Kill,
                    "keep" => Orphans::Keep,
                    _ => return Err(format!("`{v}` is not kill or keep")),
                }
            }
            (Section::Workload, "memory.max")
            | (Section::Class(_), "memory.max")
            | (Section::Limits(_), "memory.max") => {
                self.limits_of(s).memory_max = Some(Mem::parse(v)?)
            }
            (Section::Workload | Section::Class(_) | Section::Limits(_), "pids.max") => {
                self.limits_of(s).pids_max = Some(Num::parse(v, "process count", false)?)
            }
            (Section::Workload | Section::Class(_) | Section::Limits(_), "cpu.max") => {
                self.limits_of(s).cpu_max = Some(Num::parse(v, "CPU percentage", true)?)
            }
            (Section::Class(_) | Section::Limits(_), "cpu.weight") => {
                self.limits_of(s).cpu_weight = Some(weight(v)?)
            }
            (Section::Limits(_), "memory.high") => {
                self.limits_of(s).memory_high = Some(Mem::parse(v)?)
            }
            (Section::Limits(_), "memory.swap") => {
                self.limits_of(s).swap_allowed = Some(match v {
                    "none" => false,
                    "allow" => true,
                    _ => return Err(format!("`{v}` is not none or allow")),
                })
            }
            (Section::Limits(_), "oom.group") => self.limits_of(s).oom_group = Some(parse_bool(v)?),
            (Section::Limits(_), "io.max") => self.limits_of(s).io.push(parse_io(v)?),
            (Section::Limits(_), "cpuset.cpus") => self.limits_of(s).cpus = Some(parse_cpus(v)?),
            _ => return Err("is not a key of this section".into()),
        }
        Ok(())
    }

    fn limits_of(&mut self, s: &Section) -> &mut Limits {
        match s {
            Section::Class(c) => &mut self.classes.entry(c.clone()).or_default().aggregate,
            Section::Limits(c) => &mut self.classes.entry(c.clone()).or_default().workload,
            _ => &mut self.aggregate,
        }
    }

    /// Bind the policy to a machine with `mem_total` bytes of memory: percentages become bytes, and what a policy cannot
    /// mean on this machine is refused: a limit above the limit of its domain, a workload domain that leaves no memory to
    /// system, a high-water mark above the limit.
    pub fn resolve(&self, mem_total: u64) -> Result<Policy, Vec<String>> {
        let mut p = self.clone();
        let mut bad = Vec::new();
        if let Some(m) = &mut p.system.memory_reserve {
            *m = m.bytes(mem_total).map_or(Mem::Max, Mem::Bytes);
        }
        p.aggregate.resolve(mem_total);
        for c in p.classes.values_mut() {
            c.aggregate.resolve(mem_total);
            c.workload.resolve(mem_total);
        }
        if let (Some(Mem::Bytes(reserve)), Some(Mem::Bytes(cap))) =
            (p.system.memory_reserve, p.aggregate.memory_max)
        {
            if cap.saturating_add(reserve) > mem_total {
                bad.push(format!("[workload] memory.max {cap} plus the system reserve {reserve} is more than the {mem_total} bytes of this machine"));
            }
        }
        for (name, c) in &p.classes {
            bad.extend(
                c.aggregate
                    .check_within(&p.aggregate, &format!("[class.{name}]")),
            );
            bad.extend(
                c.workload
                    .check_within(&c.aggregate, &format!("[limits.{name}] in [class.{name}]")),
            );
            bad.extend(
                c.workload
                    .check_within(&p.aggregate, &format!("[limits.{name}] in [workload]")),
            );
            if let (Some(Mem::Bytes(h)), Some(Mem::Bytes(m))) =
                (c.workload.memory_high, c.workload.memory_max)
            {
                if h >= m {
                    bad.push(format!(
                        "[limits.{name}]: memory.high {h} is not below memory.max {m}"
                    ));
                }
            }
        }
        if bad.is_empty() {
            Ok(p)
        } else {
            Err(bad)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "
# a policy
[system]
cpu.weight = 1000
memory.reserve = 128M
[workload]
memory.max = 75%
pids.max = 4096
cpu.max = 300%
[class.interactive]
cpu.weight = 800
memory.max = 50%
[limits.interactive]
memory.max = 256M
memory.high = 200M
pids.max = 128
cpu.max = 100%
io.max = 8:0 wbps=10M riops=500
io.max = /dev/vda rbps=20M
cpuset.cpus = 0-1,3
oom.group = yes
[class.batch]
cpu.weight = 100
[limits.batch]
memory.max = 512M
pids.max = 64
[stop]
grace = 1500ms
[recover]
orphans = keep
";

    fn parse(t: &str) -> Vec<String> {
        Policy::parse(t).err().unwrap_or_default()
    }

    #[test]
    fn parses_the_whole_grammar() {
        let p = Policy::parse(GOOD).unwrap();
        assert_eq!(p.system.cpu_weight, Some(1000));
        assert_eq!(p.system.memory_reserve, Some(Mem::Bytes(128 << 20)));
        assert_eq!(p.aggregate.memory_max, Some(Mem::Percent(75)));
        assert_eq!(p.aggregate.cpu_max, Some(Num::N(300)));
        let i = &p.classes["interactive"];
        assert_eq!(i.aggregate.cpu_weight, Some(800));
        assert_eq!(i.workload.memory_high, Some(Mem::Bytes(200 << 20)));
        assert_eq!(i.workload.io.len(), 2);
        assert_eq!(i.workload.io[0].dev, Dev::Ids(8, 0));
        assert_eq!(i.workload.io[0].wbps, Some(10 << 20));
        assert_eq!(i.workload.io[1].dev, Dev::Path("/dev/vda".into()));
        assert_eq!(i.workload.cpus.as_deref(), Some("0-1,3"));
        assert_eq!(i.workload.oom_group, Some(true));
        assert_eq!(p.stop_grace_ms, 1500);
        assert_eq!(p.orphans, Orphans::Keep);
        assert_eq!(p.classes.len(), 2);
    }

    #[test]
    fn units() {
        assert_eq!(parse_size("64M"), Ok(64 << 20));
        assert_eq!(parse_size("64MiB"), Ok(64 << 20));
        assert_eq!(parse_size("2G"), Ok(2 << 30));
        assert_eq!(parse_size("4096"), Ok(4096));
        assert!(parse_size("0").is_err());
        assert!(parse_size("12X").is_err());
        assert!(parse_size("99999999999999T").is_err());
        assert_eq!(parse_duration("500ms"), Ok(500));
        assert_eq!(parse_duration("5s"), Ok(5000));
        assert_eq!(parse_duration("2m"), Ok(120_000));
        assert_eq!(parse_duration("3"), Ok(3000));
        assert!(parse_duration("61m").is_err());
        assert_eq!(Mem::parse("max"), Ok(Mem::Max));
        assert_eq!(Mem::parse("50%"), Ok(Mem::Percent(50)));
        assert!(Mem::parse("0%").is_err());
        assert!(Mem::parse("101%").is_err());
        assert!(
            Mem::parse("512K").is_err(),
            "below the smallest memory limit"
        );
        assert_eq!(Mem::Percent(50).bytes(1000 << 20), Some(500 << 20));
        assert_eq!(Mem::Max.bytes(1), None);
        assert!(
            valid_name("job-1_a")
                && !valid_name("")
                && !valid_name("a/b")
                && !valid_name("-x")
                && !valid_name(&"x".repeat(33))
        );
    }

    #[test]
    fn rejects_what_it_cannot_mean() {
        let cases: [(&str, &str); 14] = [
            ("[bogus]\n", "unknown section"),
            ("[class.system]\n", "not a usable class name"),
            ("[class.a/b]\n", "not a usable class name"),
            ("x = 1\n", "outside a section"),
            ("[system]\nmemory.max = 1G\n", "not a key of this section"),
            ("[system]\ncpu.weight = 0\n", "weight"),
            ("[system]\ncpu.weight = 1\ncpu.weight = 2\n", "set twice"),
            ("[system]\n[system]\n", "appears twice"),
            ("[system]\ncpu.weight\n", "not key = value"),
            (
                "[limits.a]\nmemory.max = 1G\npids.max = 1\n",
                "has no [class.a]",
            ),
            ("[class.a]\n[limits.a]\nio.max = 8:0\n", "no rate"),
            (
                "[class.a]\n[limits.a]\nio.max = sda wbps=1M\n",
                "MAJOR:MINOR",
            ),
            ("[class.a]\n[limits.a]\ncpuset.cpus = 3-1\n", "CPU list"),
            ("[stop]\ngrace = soon\n", "duration"),
        ];
        for (text, want) in cases {
            let e = parse(text);
            assert!(
                e.iter().any(|m| m.contains(want)),
                "{text:?} should say {want:?}, said {e:?}"
            );
        }
    }

    #[test]
    fn every_workload_is_bounded() {
        let e = parse("[workload]\nmemory.max = 1G\npids.max = 10\n[class.a]\n[limits.a]\nmemory.max = max\npids.max = 5\n");
        assert!(
            e.iter()
                .any(|m| m.contains("[limits.a] must set memory.max and pids.max")),
            "{e:?}"
        );
        let e = parse("[class.a]\n[limits.a]\nmemory.max = 1G\npids.max = 5\n");
        assert!(e.iter().any(|m| m.contains("[workload] must set")), "{e:?}");
        let e = parse("[workload]\nmemory.max = 1G\npids.max = 10\n");
        assert!(e.iter().any(|m| m.contains("no [class.NAME]")), "{e:?}");
    }

    #[test]
    fn resolve_binds_percentages_and_orders_the_domains() {
        let p = Policy::parse(GOOD).unwrap();
        let total = 1024 << 20;
        let r = p.resolve(total).unwrap();
        assert_eq!(r.aggregate.memory_max, Some(Mem::Bytes(768 << 20)));
        let mut over = p.clone();
        over.classes.get_mut("batch").unwrap().workload.memory_max = Some(Mem::Bytes(2 << 30));
        let e = over.resolve(total).unwrap_err();
        assert!(
            e.iter()
                .any(|m| m.contains("[limits.batch] in [workload]: memory.max")),
            "{e:?}"
        );
        let mut over = p.clone();
        over.classes
            .get_mut("interactive")
            .unwrap()
            .aggregate
            .memory_max = Some(Mem::Bytes(100 << 20));
        let e = over.resolve(total).unwrap_err();
        assert!(
            e.iter()
                .any(|m| m.contains("[limits.interactive] in [class.interactive]: memory.max")),
            "{e:?}"
        );
        let mut over = p.clone();
        over.classes
            .get_mut("interactive")
            .unwrap()
            .workload
            .memory_high = Some(Mem::Bytes(256 << 20));
        assert!(over
            .resolve(total)
            .unwrap_err()
            .iter()
            .any(|m| m.contains("memory.high")));
        let mut over = p.clone();
        over.system.memory_reserve = Some(Mem::Bytes(512 << 20));
        assert!(over
            .resolve(total)
            .unwrap_err()
            .iter()
            .any(|m| m.contains("system reserve")));
        let mut over = p;
        over.classes.get_mut("batch").unwrap().workload.cpu_max = Some(Num::Max);
        over.classes.get_mut("batch").unwrap().aggregate.cpu_max = Some(Num::N(50));
        assert!(over
            .resolve(total)
            .unwrap_err()
            .iter()
            .any(|m| m.contains("cpu.max is unlimited")));
    }

    #[test]
    fn overlay_replaces_only_what_is_set() {
        let base = Limits {
            memory_max: Some(Mem::Bytes(10 << 20)),
            pids_max: Some(Num::N(8)),
            cpus: Some("0".into()),
            ..Limits::default()
        };
        let over = Limits {
            pids_max: Some(Num::N(4)),
            ..Limits::default()
        };
        let m = base.overlay(&over);
        assert_eq!(m.memory_max, Some(Mem::Bytes(10 << 20)));
        assert_eq!(m.pids_max, Some(Num::N(4)));
        assert_eq!(m.cpus.as_deref(), Some("0"));
        let mut pct = Limits {
            memory_max: Some(Mem::Percent(10)),
            ..Limits::default()
        };
        pct.resolve(100 << 20);
        assert_eq!(pct.memory_max, Some(Mem::Bytes(10 << 20)));
    }

    #[test]
    fn the_shipped_default_policy_is_valid() {
        let text = include_str!("../policy/default.policy");
        let p = Policy::parse(text).unwrap_or_else(|e| panic!("{e:?}"));
        for total in [1u64 << 30, 16 << 30] {
            p.resolve(total)
                .unwrap_or_else(|e| panic!("{total}: {e:?}"));
        }
    }
}
