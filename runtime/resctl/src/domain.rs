//! The domains and their lifecycle.
//!
//! ```text
//! agent-interaction/system                       critical services: limits and protection only; no kill, freeze or stop reaches it
//! agent-interaction/workload                     every workload together: the ceiling that keeps them off the system's memory
//! agent-interaction/workload/<class>             one class: its ceiling and its CPU share against the other classes
//! agent-interaction/workload/<class>/<id>        one workload: its own limits and its own kill domain
//! ```
//!
//! The tree is created in every hierarchy that serves a feature (see `layout`), and a process is attached to the same
//! directory in all of them. Everything that ends processes takes a `Workload`, which cannot name `system`.

use crate::fs::Host;
use crate::layout::{Api, Feature, Layout};
use crate::policy::{valid_name, Dev, IoLimit, Limits, Mem, Num, Orphans, Policy, CPU_PERIOD_US};
use std::fmt;
use std::io;

pub const BASE: &str = "agent-interaction";
pub const SYSTEM: &str = "agent-interaction/system";
pub const WORKLOAD: &str = "agent-interaction/workload";

const FREEZE_MS: u64 = 5_000;
const KILL_MS: u64 = 10_000;
const REMOVE_MS: u64 = 3_000;

#[derive(Debug)]
pub enum Error {
    /// The policy is not valid for this machine.
    Policy(Vec<String>),
    Usage(String),
    /// The request is valid but the policy does not allow it.
    Refused(String),
    /// This machine cannot do it (missing hierarchy, controller or kernel feature).
    Unsupported(String),
    /// The kernel said no to this user.
    Denied(String),
    Exists(String),
    Timeout(String),
    Io(String),
}

impl Error {
    /// Exit code: 2 for what the caller got wrong, 3 for what the machine cannot or will not do, 1 for the rest.
    pub fn code(&self) -> i32 {
        match self {
            Error::Policy(_) | Error::Usage(_) | Error::Refused(_) => 2,
            Error::Unsupported(_) | Error::Denied(_) => 3,
            Error::Exists(_) | Error::Timeout(_) | Error::Io(_) => 1,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Error::Policy(_) => "policy",
            Error::Usage(_) => "usage",
            Error::Refused(_) => "refused",
            Error::Unsupported(_) => "unsupported",
            Error::Denied(_) => "denied",
            Error::Exists(_) => "exists",
            Error::Timeout(_) => "timeout",
            Error::Io(_) => "error",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Policy(p) => write!(f, "{}", p.join("; ")),
            Error::Usage(m)
            | Error::Refused(m)
            | Error::Unsupported(m)
            | Error::Denied(m)
            | Error::Exists(m)
            | Error::Timeout(m)
            | Error::Io(m) => {
                write!(f, "{m}")
            }
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn io_err(what: &str, e: &io::Error) -> Error {
    let msg = format!("{what}: {e}");
    match e.raw_os_error() {
        Some(1 | 13 | 30) => Error::Denied(msg),
        Some(2 | 19) => Error::Unsupported(msg),
        _ if e.kind() == io::ErrorKind::InvalidInput => Error::Usage(msg),
        _ => Error::Io(msg),
    }
}

/// One workload: a class of the policy and a name that is unique within it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workload {
    pub class: String,
    pub id: String,
}

impl Workload {
    pub fn new(class: &str, id: &str) -> Result<Workload> {
        for (what, name) in [("class", class), ("workload", id)] {
            if !valid_name(name) {
                return Err(Error::Usage(format!(
                    "`{name}` is not a usable {what} name (letters, digits, - and _; at most 32)"
                )));
            }
        }
        if class == "system" || class == "workload" {
            return Err(Error::Usage(format!("`{class}` is not a workload class")));
        }
        Ok(Workload {
            class: class.into(),
            id: id.into(),
        })
    }

    /// `CLASS/ID`
    pub fn parse(s: &str) -> Result<Workload> {
        match s.split_once('/') {
            Some((c, i)) => Workload::new(c, i),
            None => Err(Error::Usage(format!("`{s}` is not CLASS/ID"))),
        }
    }

    pub fn rel(&self) -> String {
        format!("{WORKLOAD}/{}/{}", self.class, self.id)
    }
}

impl fmt::Display for Workload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.class, self.id)
    }
}

/// Where a process may be attached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    System,
    Workload(Workload),
}

impl Target {
    pub fn rel(&self) -> String {
        match self {
            Target::System => SYSTEM.into(),
            Target::Workload(w) => w.rel(),
        }
    }
}

/// What was set, and what this machine cannot enforce (advisory settings only; a missing limit is an error).
#[derive(Debug, Default)]
pub struct Report {
    pub set: Vec<String>,
    pub unbound: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stopped {
    Empty,
    Graceful,
    Killed(usize),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub workload: Workload,
    pub procs: usize,
    pub state: &'static str,
}

#[derive(Debug, Default)]
pub struct Recovered {
    pub removed: Vec<String>,
    pub killed: Vec<String>,
    pub kept: Vec<String>,
}

fn tokens(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

/// A v1 memory limit at or above this is "no limit" (the kernel shows it as the page-rounded maximum).
const V1_UNLIMITED_FROM: u64 = 1 << 62;

/// The value of `key` in a flat keyed file (`memory.events`, `cpu.stat`, `pids.events`).
fn keyed(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        (w.next() == Some(key))
            .then(|| w.next()?.parse().ok())
            .flatten()
    })
}

/// The `total=` of the `some` line of a pressure file.
fn pressure_total(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|w| w.strip_prefix("total=")?.parse().ok())
}

pub struct Controller<'a> {
    host: &'a dyn Host,
    layout: &'a Layout,
    policy: Policy,
    mem_total: u64,
}

impl<'a> Controller<'a> {
    /// `policy` is parsed but not yet bound to this machine; it is checked against `mem_total` here.
    pub fn new(
        host: &'a dyn Host,
        layout: &'a Layout,
        policy: &Policy,
        mem_total: u64,
    ) -> Result<Controller<'a>> {
        let policy = policy.resolve(mem_total).map_err(Error::Policy)?;
        Ok(Controller {
            host,
            layout,
            policy,
            mem_total,
        })
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    // ---- primitives ----

    fn path(&self, h: usize, rel: &str) -> String {
        let root = &self.layout.hiers[h].root;
        if rel.is_empty() {
            root.clone()
        } else {
            format!("{root}/{rel}")
        }
    }

    fn put(&self, h: usize, rel: &str, file: &str, data: &str) -> Result<()> {
        let path = format!("{}/{file}", self.path(h, rel));
        self.host
            .write(&path, data)
            .map_err(|e| io_err(&format!("write {data:?} to {path}"), &e))
    }

    fn get(&self, h: usize, rel: &str, file: &str) -> Option<String> {
        self.host
            .read(&format!("{}/{file}", self.path(h, rel)))
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// The hierarchy and directory of a feature, `None` when no hierarchy serves it.
    fn at(&self, f: Feature, rel: &str) -> Option<(Api, usize)> {
        self.layout
            .index(f)
            .map(|i| (self.layout.hiers[i].api, i))
            .filter(|&(_, i)| self.host.exists(&self.path(i, rel)))
    }

    fn need(&self, f: Feature, what: &str) -> Result<usize> {
        self.layout
            .index(f)
            .ok_or_else(|| Error::Unsupported(format!("{what}: {}", self.layout.why(f))))
    }

    fn wait(&self, limit_ms: u64, what: &str, mut cond: impl FnMut() -> bool) -> Result<()> {
        let mut waited = 0;
        loop {
            if cond() {
                return Ok(());
            }
            if waited >= limit_ms {
                return Err(Error::Timeout(format!(
                    "{what} did not finish within {limit_ms} ms"
                )));
            }
            self.host.pause(20);
            waited += 20;
        }
    }

    /// The error for a domain that is not there: with no hierarchy at all the machine cannot hold any domain, which is
    /// "unsupported" like every other command that needs one; with a hierarchy it is a mistake of the caller.
    fn missing(&self, msg: String) -> Error {
        if self.layout.members().is_empty() {
            Error::Unsupported(format!(
                "{msg}: this machine has no cgroup hierarchy to hold it"
            ))
        } else {
            Error::Usage(msg)
        }
    }

    fn exists_anywhere(&self, rel: &str) -> bool {
        self.layout
            .members()
            .into_iter()
            .any(|h| self.host.exists(&self.path(h, rel)))
    }

    /// Directories below `rel` (and `rel` itself) in one hierarchy, children before parents.
    fn tree(&self, h: usize, rel: &str) -> Vec<String> {
        let mut out = Vec::new();
        for sub in self.host.subdirs(&self.path(h, rel)).unwrap_or_default() {
            out.extend(self.tree(h, &format!("{rel}/{sub}")));
        }
        out.push(rel.to_string());
        out
    }

    /// Processes in the domain and everything below it, whichever hierarchy lists them.
    pub fn pids(&self, rel: &str) -> Vec<i32> {
        let mut pids = std::collections::BTreeSet::new();
        for h in self.layout.members() {
            for dir in self.tree(h, rel) {
                for line in self
                    .get(h, &dir, "cgroup.procs")
                    .unwrap_or_default()
                    .lines()
                {
                    if let Ok(p) = line.trim().parse::<i32>() {
                        pids.insert(p);
                    }
                }
            }
        }
        pids.into_iter().collect()
    }

    // ---- tree and limits ----

    /// A v1 cpuset child starts with no CPUs and no memory nodes and refuses tasks until it has some: take the parent's.
    fn inherit_cpuset(&self, h: usize, rel: &str) -> Result<()> {
        let parent = rel.rsplit_once('/').map_or("", |p| p.0);
        for file in ["cpuset.mems", "cpuset.cpus"] {
            if self.get(h, rel, file).is_some_and(|v| v.is_empty()) {
                if let Some(v) = self.get(h, parent, file).filter(|v| !v.is_empty()) {
                    self.put(h, rel, file, &v)?;
                }
            }
        }
        Ok(())
    }

    /// The v2 controllers a hierarchy serves.
    fn v2_wanted(&self, h: usize) -> Vec<&'static str> {
        let names = [
            (Feature::Memory, "memory"),
            (Feature::Pids, "pids"),
            (Feature::CpuQuota, "cpu"),
            (Feature::CpuWeight, "cpu"),
            (Feature::Cpuset, "cpuset"),
            (Feature::Io, "io"),
        ];
        let mut out = Vec::new();
        for (f, c) in names {
            if self.layout.index(f) == Some(h) && !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    /// Hand the controllers of the directory on to its children (v2). Only a directory without processes can do this.
    fn enable(&self, h: usize, rel: &str) -> Result<()> {
        let have = tokens(&self.get(h, rel, "cgroup.controllers").unwrap_or_default());
        let on = tokens(
            &self
                .get(h, rel, "cgroup.subtree_control")
                .unwrap_or_default(),
        );
        let add: Vec<String> = self
            .v2_wanted(h)
            .into_iter()
            .filter(|c| have.iter().any(|x| x == c) && !on.iter().any(|x| x == c))
            .map(|c| format!("+{c}"))
            .collect();
        if add.is_empty() {
            return Ok(());
        }
        self.put(h, rel, "cgroup.subtree_control", &add.join(" "))
    }

    fn mk(&self, rel: &str, branch: bool) -> Result<()> {
        for h in self.layout.members() {
            let dir = self.path(h, rel);
            match self.host.mkdir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_err(&format!("create {dir}"), &e)),
            }
            match self.layout.hiers[h].api {
                Api::V1 if self.layout.hiers[h].controllers.contains("cpuset") => {
                    self.inherit_cpuset(h, rel)?
                }
                Api::V1 => {}
                Api::V2 if branch => self.enable(h, rel)?,
                Api::V2 => {}
            }
        }
        Ok(())
    }

    /// Create the domain tree and set the limits of the policy. Safe to run again and again.
    pub fn init(&self) -> Result<Report> {
        for (f, what) in [
            (Feature::Memory, "memory limits"),
            (Feature::Pids, "process limits"),
            (Feature::Kill, "termination"),
        ] {
            self.need(f, what)?;
        }
        let mut rep = Report::default();
        for h in self.layout.members() {
            if self.layout.hiers[h].api == Api::V2 {
                // the root may already hand its controllers on (systemd does); either way the base must offer them
                let _ = self.enable(h, "");
            }
        }
        self.mk(BASE, true)?;
        for h in self.layout.members() {
            let hier = &self.layout.hiers[h];
            if hier.api == Api::V1 && hier.controllers.contains("memory") {
                // one limit on `workload` bounds the sum of its workloads only with hierarchical accounting
                let _ = self.put(h, BASE, "memory.use_hierarchy", "1");
                if self.get(h, BASE, "memory.use_hierarchy").as_deref() != Some("1") {
                    return Err(Error::Unsupported(format!(
                        "{}: memory.use_hierarchy is off and cannot be enabled",
                        self.path(h, BASE)
                    )));
                }
            }
            if hier.api == Api::V2
                && !self
                    .host
                    .exists(&format!("{}/cgroup.kill", self.path(h, BASE)))
                && self.layout.api(Feature::Kill) == Some(Api::V2)
            {
                return Err(Error::Unsupported("cgroup.kill is missing: the cgroup v2 kill needs Linux 5.14; use --lifecycle v1".into()));
            }
        }
        self.mk(SYSTEM, false)?;
        self.mk(WORKLOAD, true)?;
        for class in self.policy.classes.keys() {
            self.mk(&format!("{WORKLOAD}/{class}"), true)?;
        }
        self.protect_system(&mut rep)?;
        self.apply(WORKLOAD, &self.policy.aggregate, &mut rep)?;
        for (name, class) in &self.policy.classes {
            self.apply(&format!("{WORKLOAD}/{name}"), &class.aggregate, &mut rep)?;
        }
        Ok(rep)
    }

    /// The system domain: a heavy CPU weight, and memory protection (v2 only; on v1 the reserve is kept by the ceiling on
    /// `workload`, which policy resolution keeps below MemTotal minus the reserve).
    fn protect_system(&self, rep: &mut Report) -> Result<()> {
        if let Some(w) = self.policy.system.cpu_weight {
            self.cpu_weight(SYSTEM, w, rep)?;
        }
        if let Some(Mem::Bytes(b)) = self.policy.system.memory_reserve {
            match self.at(Feature::Memory, SYSTEM) {
                Some((Api::V2, h)) => {
                    // protection is effective only as far as every ancestor protects too
                    for rel in [BASE, SYSTEM] {
                        self.put(h, rel, "memory.low", &b.to_string())?;
                    }
                    rep.set.push("system memory.low".into());
                }
                _ => rep
                    .unbound
                    .push("system memory.low: needs the cgroup v2 memory controller".into()),
            }
        }
        Ok(())
    }

    fn cpu_weight(&self, rel: &str, w: u32, rep: &mut Report) -> Result<()> {
        match self.at(Feature::CpuWeight, rel) {
            Some((Api::V2, h)) => self.put(h, rel, "cpu.weight", &w.to_string())?,
            Some((Api::V1, h)) => self.put(
                h,
                rel,
                "cpu.shares",
                &(u64::from(w) * 1024 / 100).max(2).to_string(),
            )?,
            None => {
                rep.unbound.push(format!(
                    "cpu.weight of {rel}: {}",
                    self.layout.why(Feature::CpuWeight)
                ));
                return Ok(());
            }
        }
        rep.set.push(format!("cpu.weight of {rel}"));
        Ok(())
    }

    fn dev_ids(&self, d: &Dev) -> Result<(u32, u32)> {
        match d {
            Dev::Ids(a, b) => Ok((*a, *b)),
            Dev::Path(p) => self
                .host
                .devno(p)
                .map_err(|e| io_err(&format!("device {p}"), &e)),
        }
    }

    fn io_limit(&self, rel: &str, l: &IoLimit) -> Result<()> {
        let h = self.need(Feature::Io, "io.max")?;
        let (maj, min) = self.dev_ids(&l.dev)?;
        let rates = [
            ("rbps", "read_bps", l.rbps),
            ("wbps", "write_bps", l.wbps),
            ("riops", "read_iops", l.riops),
            ("wiops", "write_iops", l.wiops),
        ];
        match self.layout.hiers[h].api {
            Api::V2 => {
                let set: Vec<String> = rates
                    .iter()
                    .filter_map(|(k, _, v)| v.map(|v| format!("{k}={v}")))
                    .collect();
                self.put(h, rel, "io.max", &format!("{maj}:{min} {}", set.join(" ")))
            }
            Api::V1 => {
                for (_, file, v) in rates {
                    if let Some(v) = v {
                        self.put(
                            h,
                            rel,
                            &format!("blkio.throttle.{file}_device"),
                            &format!("{maj}:{min} {v}"),
                        )?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Set the memory limit; true when swap is held to it too (or allowed), false when this hierarchy cannot account swap.
    fn memory_max(&self, rel: &str, m: Mem, swap: bool) -> Result<bool> {
        let h = self.need(Feature::Memory, "memory.max")?;
        let bytes = m.bytes(self.mem_total);
        match self.layout.hiers[h].api {
            Api::V2 => {
                self.put(
                    h,
                    rel,
                    "memory.max",
                    &bytes.map_or("max".into(), |b| b.to_string()),
                )?;
                // swap would let a workload page out instead of failing at its limit
                match self.put(h, rel, "memory.swap.max", if swap { "max" } else { "0" }) {
                    Ok(()) => {}
                    Err(Error::Unsupported(_)) => return Ok(swap),
                    Err(e) => return Err(e),
                }
            }
            Api::V1 => {
                let limit = bytes.map_or("-1".into(), |b| b.to_string());
                let memsw = self.host.exists(&format!(
                    "{}/memory.memsw.limit_in_bytes",
                    self.path(h, rel)
                ));
                // memory+swap must never sit below the limit: raise it, set the limit, then pin it to the limit
                if memsw {
                    self.put(h, rel, "memory.memsw.limit_in_bytes", "-1")?;
                }
                self.put(h, rel, "memory.limit_in_bytes", &limit)?;
                if memsw && !swap {
                    self.put(h, rel, "memory.memsw.limit_in_bytes", &limit)?;
                }
                return Ok(memsw || swap);
            }
        }
        Ok(true)
    }

    /// Set what `l` sets on the domain. A limit this machine cannot enforce is an error; advisory settings are reported.
    fn apply(&self, rel: &str, l: &Limits, rep: &mut Report) -> Result<()> {
        if let Some(m) = l.memory_max {
            let swap_held = self.memory_max(rel, m, l.swap_allowed.unwrap_or(false))?;
            rep.set.push(format!("memory.max of {rel}"));
            // the default is no swap too; only a request for it is reported, so the usual workload stays quiet
            if l.swap_allowed == Some(false) {
                if swap_held {
                    rep.set.push(format!("memory.swap of {rel}"));
                } else {
                    rep.unbound.push(format!(
                        "memory.swap of {rel}: this memory hierarchy has no swap accounting"
                    ));
                }
            }
        }
        if let Some(m) = l.memory_high {
            match self.at(Feature::Memory, rel) {
                Some((Api::V2, h)) => {
                    let v = m
                        .bytes(self.mem_total)
                        .map_or("max".into(), |b| b.to_string());
                    self.put(h, rel, "memory.high", &v)?;
                    rep.set.push(format!("memory.high of {rel}"));
                }
                _ => rep.unbound.push(format!(
                    "memory.high of {rel}: needs the cgroup v2 memory controller"
                )),
            }
        }
        if let Some(g) = l.oom_group {
            match self.at(Feature::Memory, rel) {
                Some((Api::V2, h)) => {
                    self.put(h, rel, "memory.oom.group", if g { "1" } else { "0" })?;
                    rep.set.push(format!("memory.oom.group of {rel}"));
                }
                _ => rep.unbound.push(format!(
                    "memory.oom.group of {rel}: needs the cgroup v2 memory controller"
                )),
            }
        }
        if let Some(n) = l.pids_max {
            let h = self.need(Feature::Pids, "pids.max")?;
            self.put(
                h,
                rel,
                "pids.max",
                &match n {
                    Num::Max => "max".to_string(),
                    Num::N(v) => v.to_string(),
                },
            )?;
            rep.set.push(format!("pids.max of {rel}"));
        }
        if let Some(n) = l.cpu_max {
            let h = self.need(Feature::CpuQuota, "cpu.max")?;
            let quota = match n {
                Num::Max => None,
                Num::N(pct) => Some(pct * CPU_PERIOD_US / 100),
            };
            match self.layout.hiers[h].api {
                Api::V2 => self.put(
                    h,
                    rel,
                    "cpu.max",
                    &format!(
                        "{} {CPU_PERIOD_US}",
                        quota.map_or("max".into(), |q| q.to_string())
                    ),
                )?,
                Api::V1 => {
                    self.put(h, rel, "cpu.cfs_period_us", &CPU_PERIOD_US.to_string())?;
                    self.put(
                        h,
                        rel,
                        "cpu.cfs_quota_us",
                        &quota.map_or("-1".into(), |q| q.to_string()),
                    )?;
                }
            }
            rep.set.push(format!("cpu.max of {rel}"));
        }
        if let Some(w) = l.cpu_weight {
            self.cpu_weight(rel, w, rep)?;
        }
        for io in &l.io {
            self.io_limit(rel, io)?;
            rep.set.push(format!("io.max of {rel}"));
        }
        if let Some(cpus) = &l.cpus {
            let h = self.need(Feature::Cpuset, "cpuset.cpus")?;
            self.put(h, rel, "cpuset.cpus", cpus)?;
            rep.set.push(format!("cpuset.cpus of {rel}"));
        }
        Ok(())
    }

    // ---- workloads ----

    /// Create a workload domain with the limits of its class, overridden by `over`, inside the ceilings of the policy.
    pub fn create(&self, w: &Workload, over: &Limits) -> Result<Report> {
        let class =
            self.policy.classes.get(&w.class).ok_or_else(|| {
                Error::Usage(format!("`{}` is not a class of the policy", w.class))
            })?;
        let mut over = over.clone();
        over.resolve(self.mem_total);
        let eff = class.workload.overlay(&over);
        let mut bad = eff.check_within(&class.aggregate, &format!("{w}"));
        bad.extend(eff.check_within(&self.policy.aggregate, &format!("{w}")));
        if let (Some(Mem::Bytes(hi)), Some(Mem::Bytes(max))) = (eff.memory_high, eff.memory_max) {
            if hi >= max {
                bad.push(format!(
                    "{w}: memory.high {hi} is not below memory.max {max}"
                ));
            }
        }
        if !bad.is_empty() {
            return Err(Error::Refused(bad.join("; ")));
        }
        let class_rel = format!("{WORKLOAD}/{}", w.class);
        if !self.exists_anywhere(&class_rel) {
            return Err(Error::Unsupported(format!(
                "{class_rel} does not exist: run `resctl init` first"
            )));
        }
        if self.exists_anywhere(&w.rel()) {
            return Err(Error::Exists(format!("workload {w} already exists")));
        }
        self.mk(&w.rel(), false)?;
        let mut rep = Report::default();
        if let Err(e) = self.apply(&w.rel(), &eff, &mut rep) {
            let _ = self.remove_tree(&w.rel());
            return Err(e);
        }
        Ok(rep)
    }

    /// The `cgroup.procs` files that take a process into the target, one per hierarchy.
    pub fn procs_paths(&self, t: &Target) -> Result<Vec<String>> {
        let rel = t.rel();
        if !self.exists_anywhere(&rel) {
            return Err(self.missing(format!(
                "{rel} does not exist: run `resctl init` and create the workload first"
            )));
        }
        Ok(self
            .layout
            .members()
            .into_iter()
            .map(|h| format!("{}/cgroup.procs", self.path(h, &rel)))
            .collect())
    }

    pub fn attach(&self, t: &Target, pid: i32) -> Result<()> {
        if pid < 1 {
            return Err(Error::Usage(format!("{pid} is not a process id")));
        }
        if matches!(t, Target::Workload(_)) && pid == 1 {
            return Err(Error::Refused(
                "process 1 is never moved into a workload".into(),
            ));
        }
        for path in self.procs_paths(t)? {
            self.host
                .write(&path, &pid.to_string())
                .map_err(|e| io_err(&format!("attach {pid} to {path}"), &e))?;
        }
        Ok(())
    }

    fn require(&self, w: &Workload) -> Result<String> {
        let rel = w.rel();
        if self.exists_anywhere(&rel) {
            Ok(rel)
        } else {
            Err(self.missing(format!("workload {w} does not exist")))
        }
    }

    pub fn is_frozen(&self, rel: &str) -> bool {
        match self.at(Feature::Freeze, rel) {
            Some((Api::V2, h)) => {
                self.get(h, rel, "cgroup.events")
                    .and_then(|e| keyed(&e, "frozen"))
                    == Some(1)
            }
            Some((Api::V1, h)) => matches!(
                self.get(h, rel, "freezer.state").as_deref(),
                Some("FROZEN" | "FREEZING")
            ),
            None => false,
        }
    }

    fn set_frozen(&self, rel: &str, frozen: bool) -> Result<()> {
        let Some((api, h)) = self.at(Feature::Freeze, rel) else {
            return Err(Error::Unsupported(format!(
                "freeze: {}",
                self.layout.why(Feature::Freeze)
            )));
        };
        let what = if frozen { "freezing" } else { "thawing" };
        match api {
            Api::V2 => {
                self.put(h, rel, "cgroup.freeze", if frozen { "1" } else { "0" })?;
                self.wait(FREEZE_MS, what, || {
                    self.get(h, rel, "cgroup.events")
                        .and_then(|e| keyed(&e, "frozen"))
                        == Some(u64::from(frozen))
                })
            }
            Api::V1 => {
                let want = if frozen { "FROZEN" } else { "THAWED" };
                self.put(h, rel, "freezer.state", want)?;
                self.wait(FREEZE_MS, what, || {
                    self.get(h, rel, "freezer.state").as_deref() == Some(want)
                })
            }
        }
    }

    pub fn freeze(&self, w: &Workload) -> Result<()> {
        let rel = self.require(w)?;
        self.set_frozen(&rel, true)
    }

    pub fn thaw(&self, w: &Workload) -> Result<()> {
        let rel = self.require(w)?;
        self.set_frozen(&rel, false)
    }

    /// End every process of the domain, forks included. v2 does it in one write; the v1 freezer cannot (a frozen task does
    /// not die until it is thawed), so it freezes the domain so nothing forks, signals everything, thaws, and checks.
    fn kill_rel(&self, rel: &str) -> Result<()> {
        if self.pids(rel).is_empty() {
            return Ok(());
        }
        let Some((api, h)) = self.at(Feature::Kill, rel) else {
            return Err(Error::Unsupported(format!(
                "kill: {}",
                self.layout.why(Feature::Kill)
            )));
        };
        match api {
            Api::V2 => {
                self.put(h, rel, "cgroup.kill", "1")?;
                self.wait(KILL_MS, "killing", || self.pids(rel).is_empty())
            }
            Api::V1 => {
                for _ in 0..5 {
                    let _ = self.set_frozen(rel, true);
                    for pid in self.pids(rel) {
                        let _ = self.host.signal(pid, 9);
                    }
                    self.set_frozen(rel, false)?;
                    if self
                        .wait(KILL_MS / 5, "killing", || self.pids(rel).is_empty())
                        .is_ok()
                    {
                        return Ok(());
                    }
                }
                Err(Error::Timeout(format!(
                    "{rel} still has processes after killing"
                )))
            }
        }
    }

    pub fn kill(&self, w: &Workload) -> Result<()> {
        let rel = self.require(w)?;
        self.kill_rel(&rel)
    }

    /// SIGTERM to everything, `grace_ms` to exit, then the kill.
    pub fn stop(&self, w: &Workload, grace_ms: u64) -> Result<Stopped> {
        let rel = self.require(w)?;
        let pids = self.pids(&rel);
        if pids.is_empty() {
            return Ok(Stopped::Empty);
        }
        if self.is_frozen(&rel) {
            self.set_frozen(&rel, false)?;
        }
        for pid in pids {
            let _ = self.host.signal(pid, 15);
        }
        if self
            .wait(grace_ms, "stopping", || self.pids(&rel).is_empty())
            .is_ok()
        {
            return Ok(Stopped::Graceful);
        }
        let left = self.pids(&rel).len();
        self.kill_rel(&rel)?;
        Ok(Stopped::Killed(left))
    }

    /// Remove the domain and everything below it, deepest first. A directory that is still busy is retried: the kernel
    /// releases a killed task a moment after its last write.
    fn remove_tree(&self, rel: &str) -> Result<()> {
        for h in self.layout.members() {
            for dir in self.tree(h, rel) {
                let path = self.path(h, &dir);
                self.wait(REMOVE_MS, &format!("removing {path}"), || {
                    match self.host.rmdir(&path) {
                        Ok(()) => true,
                        Err(e) => e.kind() == io::ErrorKind::NotFound,
                    }
                })
                .map_err(|_| {
                    Error::Io(format!(
                        "cannot remove {path}: {}",
                        self.host
                            .rmdir(&path)
                            .err()
                            .map_or("busy".into(), |e| e.to_string())
                    ))
                })?;
            }
        }
        Ok(())
    }

    /// Kill the workload and remove its domain.
    pub fn remove(&self, w: &Workload) -> Result<()> {
        let rel = self.require(w)?;
        self.kill_rel(&rel)?;
        self.remove_tree(&rel)
    }

    pub fn state(&self, rel: &str) -> &'static str {
        if self.pids(rel).is_empty() {
            "empty"
        } else if self.is_frozen(rel) {
            "frozen"
        } else {
            "running"
        }
    }

    fn union_subdirs(&self, rel: &str) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        for h in self.layout.members() {
            names.extend(self.host.subdirs(&self.path(h, rel)).unwrap_or_default());
        }
        names.into_iter().collect()
    }

    /// Every workload domain that exists, whatever its class is called.
    pub fn list(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        for class in self.union_subdirs(WORKLOAD) {
            for id in self.union_subdirs(&format!("{WORKLOAD}/{class}")) {
                if let Ok(workload) = Workload::new(&class, &id) {
                    let rel = workload.rel();
                    out.push(Entry {
                        procs: self.pids(&rel).len(),
                        state: self.state(&rel),
                        workload,
                    });
                }
            }
        }
        out
    }

    /// Counters of a domain as `key=value` pairs; what the machine does not count is left out, never zero.
    pub fn status(&self, rel: &str) -> Result<Vec<(String, String)>> {
        if !self.exists_anywhere(rel) {
            return Err(self.missing(format!("{rel} does not exist")));
        }
        let pids = self.pids(rel);
        let mut out = vec![
            ("domain".to_string(), rel.to_string()),
            ("state".to_string(), self.state(rel).to_string()),
            ("procs".to_string(), pids.len().to_string()),
        ];
        let mut add = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                out.push((k.to_string(), v));
            }
        };
        let num = |v: Option<u64>| v.map(|v| v.to_string());
        match self.at(Feature::Memory, rel) {
            Some((Api::V2, h)) => {
                let ev = self.get(h, rel, "memory.events").unwrap_or_default();
                add("memory.current", self.get(h, rel, "memory.current"));
                add("memory.peak", self.get(h, rel, "memory.peak"));
                add("memory.max", self.get(h, rel, "memory.max"));
                add("memory.oom_kill", num(keyed(&ev, "oom_kill")));
                add("memory.high_events", num(keyed(&ev, "high")));
                add("memory.max_events", num(keyed(&ev, "max")));
            }
            Some((Api::V1, h)) => {
                add("memory.current", self.get(h, rel, "memory.usage_in_bytes"));
                add("memory.peak", self.get(h, rel, "memory.max_usage_in_bytes"));
                add("memory.max", self.get(h, rel, "memory.limit_in_bytes"));
                add(
                    "memory.oom_kill",
                    num(self
                        .get(h, rel, "memory.oom_control")
                        .and_then(|t| keyed(&t, "oom_kill"))),
                );
                // failcnt counts the hits of the memory limit. While memory+swap is pinned to a limit (--swap none),
                // the kernel fails the charge on the memory+swap counter first and counts it nowhere: zero would
                // be a wrong answer there, so the key is left out
                let memsw_binds = self
                    .get(h, rel, "memory.memsw.limit_in_bytes")
                    .and_then(|v| v.parse::<u64>().ok())
                    .is_some_and(|v| v < V1_UNLIMITED_FROM);
                if !memsw_binds {
                    add("memory.max_events", self.get(h, rel, "memory.failcnt"));
                }
            }
            None => {}
        }
        if let Some((_, h)) = self.at(Feature::Pids, rel) {
            add("pids.current", self.get(h, rel, "pids.current"));
            add("pids.max", self.get(h, rel, "pids.max"));
            add(
                "pids.refused",
                num(self
                    .get(h, rel, "pids.events")
                    .and_then(|t| keyed(&t, "max"))),
            );
        }
        if let Some((api, h)) = self.at(Feature::CpuQuota, rel) {
            let stat = self.get(h, rel, "cpu.stat").unwrap_or_default();
            add("cpu.throttled", num(keyed(&stat, "nr_throttled")));
            if api == Api::V2 {
                add("cpu.usage_usec", num(keyed(&stat, "usage_usec")));
            }
        }
        if let Some((_, h)) = self.at(Feature::Pressure, rel) {
            for (name, file) in [
                ("cpu", "cpu.pressure"),
                ("memory", "memory.pressure"),
                ("io", "io.pressure"),
            ] {
                add(
                    &format!("pressure.{name}.some_total"),
                    num(self.get(h, rel, file).and_then(|t| pressure_total(&t))),
                );
            }
        }
        Ok(out)
    }

    /// For a supervisor that starts with no workload of its own running: every workload domain found is an orphan.
    /// Empty ones are removed; running ones are killed and removed, or kept, as the policy says. `system` is never touched.
    pub fn recover(&self, orphans: Orphans) -> Result<Recovered> {
        self.init()?;
        let mut done = Recovered::default();
        for e in self.list() {
            let name = e.workload.to_string();
            if e.procs == 0 {
                self.remove(&e.workload)?;
                done.removed.push(name);
            } else if orphans == Orphans::Kill {
                self.remove(&e.workload)?;
                done.killed.push(name);
            } else {
                done.kept.push(name);
            }
        }
        // a class the policy no longer names, left without workloads
        for class in self.union_subdirs(WORKLOAD) {
            if !self.policy.classes.contains_key(&class)
                && self
                    .union_subdirs(&format!("{WORKLOAD}/{class}"))
                    .is_empty()
            {
                self.remove_tree(&format!("{WORKLOAD}/{class}"))?;
            }
        }
        Ok(done)
    }

    /// Remove every workload (killed), every class and the workload domain. `system` is kept as long as it holds
    /// processes. Returns what was kept.
    pub fn teardown(&self) -> Result<Vec<String>> {
        let mut kept = Vec::new();
        for e in self.list() {
            self.remove(&e.workload)?;
        }
        if self.exists_anywhere(WORKLOAD) {
            self.kill_rel(WORKLOAD)?;
            self.remove_tree(WORKLOAD)?;
        }
        if self.exists_anywhere(SYSTEM) {
            let n = self.pids(SYSTEM).len();
            if n == 0 && self.union_subdirs(SYSTEM).is_empty() {
                self.remove_tree(SYSTEM)?;
            } else {
                kept.push(format!("system: {n} process(es) stay in their domain"));
            }
        }
        if self.exists_anywhere(BASE) && self.union_subdirs(BASE).is_empty() {
            self.remove_tree(BASE)?;
        }
        Ok(kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::fake::FakeHost;
    use crate::layout::Lifecycle;
    use crate::policy::{parse_io, Mem, Num};

    const GIB: u64 = 1 << 30;
    const POLICY: &str = "\
[system]
cpu.weight = 1000
memory.reserve = 64M
[workload]
memory.max = 512M
pids.max = 256
[class.interactive]
cpu.weight = 400
memory.max = 256M
pids.max = 128
[limits.interactive]
memory.max = 96M
pids.max = 32
[class.batch]
cpu.weight = 100
memory.max = 160M
pids.max = 96
cpu.max = 150%
[limits.batch]
memory.max = 64M
memory.high = 48M
pids.max = 16
cpu.max = 50%
[stop]
grace = 1500ms
";

    fn policy() -> Policy {
        Policy::parse(POLICY).unwrap()
    }

    fn v2_host() -> FakeHost {
        let h = FakeHost::new();
        h.add_v2("/c", &["cpuset", "cpu", "io", "memory", "pids"]);
        h
    }

    fn hybrid_host() -> FakeHost {
        let h = FakeHost::new();
        h.add_v1("/c/systemd", &[]);
        for c in [
            "memory", "pids", "cpu", "cpuacct", "cpuset", "blkio", "freezer",
        ] {
            h.add_v1(&format!("/c/{c}"), &[c]);
        }
        h.add_v2("/c/unified", &["hugetlb"]);
        h
    }

    fn setup<'a>(h: &'a FakeHost, l: &'a Layout) -> Controller<'a> {
        Controller::new(h, l, &policy(), 4 * GIB).unwrap()
    }

    fn w(class: &str, id: &str) -> Workload {
        Workload::new(class, id).unwrap()
    }

    #[test]
    fn init_builds_the_tree_and_sets_the_ceilings_on_v2() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        for d in [
            "agent-interaction/system",
            "agent-interaction/workload",
            "agent-interaction/workload/batch",
            "agent-interaction/workload/interactive",
        ] {
            assert!(h.dir_exists(&format!("/c/{d}")), "{d}");
        }
        assert_eq!(
            h.value("/c/agent-interaction/workload/memory.max"),
            (512u64 << 20).to_string()
        );
        assert_eq!(h.value("/c/agent-interaction/workload/pids.max"), "256");
        assert_eq!(
            h.value("/c/agent-interaction/workload/batch/memory.max"),
            (160u64 << 20).to_string()
        );
        assert_eq!(
            h.value("/c/agent-interaction/workload/batch/cpu.max"),
            "150000 100000"
        );
        assert_eq!(
            h.value("/c/agent-interaction/workload/batch/cpu.weight"),
            "100"
        );
        assert_eq!(
            h.value("/c/agent-interaction/workload/interactive/cpu.weight"),
            "400"
        );
        assert_eq!(
            h.value("/c/agent-interaction/workload/memory.swap.max"),
            "0"
        );
        // the controllers are handed down the branches, and the system leaf holds no controller of its own
        assert_eq!(
            h.value("/c/agent-interaction/cgroup.subtree_control"),
            "cpu cpuset io memory pids"
        );
        assert_eq!(
            h.value("/c/agent-interaction/system/cgroup.subtree_control"),
            ""
        );
        assert_eq!(h.value("/c/agent-interaction/system/cpu.weight"), "1000");
    }

    #[test]
    fn the_system_domain_is_protected_at_every_level_on_v2() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        setup(&h, &l).init().unwrap();
        let reserve = (64u64 << 20).to_string();
        assert_eq!(h.value("/c/agent-interaction/memory.low"), reserve);
        assert_eq!(h.value("/c/agent-interaction/system/memory.low"), reserve);
    }

    #[test]
    fn init_is_idempotent() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        c.create(&w("batch", "a"), &Limits::default()).unwrap();
        h.proc_in(10, &["/c/agent-interaction/workload/batch/a"]);
        let again = c.init().unwrap();
        assert!(!again.set.is_empty());
        assert_eq!(
            h.procs_of("/c/agent-interaction/workload/batch/a"),
            vec![10]
        );
    }

    #[test]
    fn init_on_a_hybrid_machine_limits_through_v1_and_keeps_the_lifecycle_on_v2() {
        let h = hybrid_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let rep = setup(&h, &l).init().unwrap();
        let b = "agent-interaction/workload/batch";
        assert_eq!(
            h.value(&format!("/c/memory/{b}/memory.limit_in_bytes")),
            (160u64 << 20).to_string()
        );
        assert_eq!(
            h.value(&format!("/c/memory/{b}/memory.memsw.limit_in_bytes")),
            (160u64 << 20).to_string()
        );
        assert_eq!(h.value(&format!("/c/pids/{b}/pids.max")), "96");
        assert_eq!(h.value(&format!("/c/cpu/{b}/cpu.cfs_quota_us")), "150000");
        assert_eq!(h.value(&format!("/c/cpu/{b}/cpu.shares")), "1024");
        assert_eq!(
            h.value("/c/cpu/agent-interaction/workload/interactive/cpu.shares"),
            "4096"
        );
        assert_eq!(
            h.value("/c/cpu/agent-interaction/system/cpu.shares"),
            "10240"
        );
        assert!(h.dir_exists(&format!("/c/unified/{b}")));
        assert!(
            !h.dir_exists(&format!("/c/freezer/{b}")),
            "the v1 freezer is not used while v2 serves the lifecycle"
        );
        // the v1 cpuset child took its parent's CPUs and nodes, or no process could be attached
        assert_eq!(h.value(&format!("/c/cpuset/{b}/cpuset.cpus")), "0-3");
        assert_eq!(h.value(&format!("/c/cpuset/{b}/cpuset.mems")), "0");
        assert!(
            rep.unbound.iter().any(|u| u.contains("memory.low")),
            "{:?}",
            rep.unbound
        );
    }

    #[test]
    fn v1_memory_is_set_in_the_order_the_kernel_accepts() {
        let h = hybrid_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        c.create(&w("batch", "a"), &Limits::default()).unwrap();
        // lowering an existing limit must not trip over memory+swap: raise memsw, set the limit, pin memsw
        let lim = "/memory.limit_in_bytes";
        let sw = "/memory.memsw.limit_in_bytes";
        let (limit, memsw) = (h.writes_to(lim), h.writes_to(sw));
        assert!(memsw.len() > limit.len());
        assert_eq!(memsw.first().map(String::as_str), Some("-1"));
    }

    #[test]
    fn a_request_to_deny_swap_is_reported_as_set_or_as_unbound_never_silently_dropped() {
        let swap_none = Limits {
            swap_allowed: Some(false),
            ..Limits::default()
        };
        // v2 holds swap to zero
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let rep = c.create(&w("batch", "a"), &swap_none).unwrap();
        assert!(
            rep.set.iter().any(|s| s.starts_with("memory.swap")),
            "{rep:?}"
        );
        assert!(rep.unbound.iter().all(|u| !u.contains("memory.swap")));
        // v1 with memsw pins memory+swap to the limit
        let h = hybrid_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let rep = c.create(&w("batch", "a"), &swap_none).unwrap();
        assert!(
            rep.set.iter().any(|s| s.starts_with("memory.swap")),
            "{rep:?}"
        );
        // v1 without swap accounting cannot deny swap: the request is reported, the limit itself still applies
        let h = hybrid_host();
        h.without_memsw();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let rep = c.create(&w("batch", "a"), &swap_none).unwrap();
        assert!(
            rep.unbound.iter().any(|u| u.starts_with("memory.swap of")),
            "{rep:?}"
        );
        assert!(rep.set.iter().any(|s| s.starts_with("memory.max")));
        assert!(!rep.set.iter().any(|s| s.starts_with("memory.swap")));
        // the default (no request) stays quiet on every layout
        let rep = c.create(&w("batch", "b"), &Limits::default()).unwrap();
        assert!(
            rep.unbound.iter().all(|u| !u.contains("memory.swap")),
            "{rep:?}"
        );
    }

    #[test]
    fn v1_aggregate_memory_needs_hierarchical_accounting() {
        // a root without hierarchical accounting is not an obstacle: the new base can switch it on for its subtree
        let h = hybrid_host();
        h.set("/c/memory/memory.use_hierarchy", "0");
        let l = Layout::detect(&h, Lifecycle::Auto);
        setup(&h, &l).init().unwrap();
        assert_eq!(
            h.value("/c/memory/agent-interaction/workload/batch/memory.use_hierarchy"),
            "1"
        );
        // but a kernel that refuses it (the base already has children from an older layout) is an error, not a quiet gap
        let h = hybrid_host();
        h.set("/c/memory/memory.use_hierarchy", "0");
        h.deny_writes_to("/memory.use_hierarchy");
        let l = Layout::detect(&h, Lifecycle::Auto);
        let e = setup(&h, &l).init().unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(ref m) if m.contains("use_hierarchy")),
            "{e:?}"
        );
    }

    #[test]
    fn a_workload_gets_the_defaults_of_its_class_and_a_start_may_only_lower_them() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        c.create(&w("batch", "a"), &Limits::default()).unwrap();
        let d = "/c/agent-interaction/workload/batch/a";
        assert_eq!(
            h.value(&format!("{d}/memory.max")),
            (64u64 << 20).to_string()
        );
        assert_eq!(
            h.value(&format!("{d}/memory.high")),
            (48u64 << 20).to_string()
        );
        assert_eq!(h.value(&format!("{d}/pids.max")), "16");
        assert_eq!(h.value(&format!("{d}/cpu.max")), "50000 100000");
        let lower = Limits {
            memory_max: Some(Mem::Bytes(32 << 20)),
            memory_high: Some(Mem::Bytes(24 << 20)),
            pids_max: Some(Num::N(8)),
            ..Limits::default()
        };
        c.create(&w("batch", "b"), &lower).unwrap();
        assert_eq!(
            h.value("/c/agent-interaction/workload/batch/b/memory.max"),
            (32u64 << 20).to_string()
        );
        assert_eq!(
            h.value("/c/agent-interaction/workload/batch/b/pids.max"),
            "8"
        );
    }

    #[test]
    fn a_workload_cannot_exceed_its_class_or_the_workload_ceiling() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        for over in [
            Limits {
                memory_max: Some(Mem::Bytes(200 << 20)),
                ..Limits::default()
            },
            Limits {
                memory_max: Some(Mem::Max),
                ..Limits::default()
            },
            Limits {
                pids_max: Some(Num::N(97)),
                ..Limits::default()
            },
            Limits {
                pids_max: Some(Num::Max),
                ..Limits::default()
            },
            Limits {
                cpu_max: Some(Num::N(200)),
                ..Limits::default()
            },
            Limits {
                memory_high: Some(Mem::Bytes(64 << 20)),
                ..Limits::default()
            },
        ] {
            let e = c.create(&w("batch", "x"), &over).unwrap_err();
            assert!(matches!(e, Error::Refused(_)), "{over:?}: {e:?}");
            assert_eq!(e.code(), 2);
        }
        assert!(
            !h.dir_exists("/c/agent-interaction/workload/batch/x"),
            "a refused workload leaves nothing behind"
        );
    }

    #[test]
    fn create_rejects_unknown_classes_duplicates_and_a_missing_init() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        assert!(matches!(
            c.create(&w("batch", "a"), &Limits::default()),
            Err(Error::Unsupported(_))
        ));
        c.init().unwrap();
        assert!(matches!(
            c.create(&w("nope", "a"), &Limits::default()),
            Err(Error::Usage(_))
        ));
        c.create(&w("batch", "a"), &Limits::default()).unwrap();
        assert!(matches!(
            c.create(&w("batch", "a"), &Limits::default()),
            Err(Error::Exists(_))
        ));
        for bad in ["a/b", "", "-x", "x y"] {
            assert!(Workload::new("batch", bad).is_err(), "{bad}");
        }
        assert!(Workload::new("workload", "a").is_err() && Workload::new("system", "a").is_err());
        assert_eq!(Workload::parse("batch/a").unwrap(), w("batch", "a"));
        assert!(Workload::parse("batch").is_err());
    }

    #[test]
    fn a_failed_limit_does_not_leave_a_half_made_workload() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        h.deny_writes_to("/pids.max");
        let e = c.create(&w("batch", "a"), &Limits::default()).unwrap_err();
        assert!(matches!(e, Error::Denied(_)) && e.code() == 3, "{e:?}");
        assert!(!h.dir_exists("/c/agent-interaction/workload/batch/a"));
    }

    #[test]
    fn io_limits_are_written_in_each_api_with_only_the_rates_that_are_set() {
        let h = v2_host();
        h.add_dev("/dev/vdx", 254, 16);
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let over = Limits {
            io: vec![parse_io("/dev/vdx wbps=1M riops=500").unwrap()],
            ..Limits::default()
        };
        c.create(&w("batch", "a"), &over).unwrap();
        assert_eq!(
            h.value("/c/agent-interaction/workload/batch/a/io.max"),
            "254:16 wbps=1048576 riops=500"
        );

        let h = hybrid_host();
        h.add_dev("/dev/vdx", 254, 16);
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        c.create(&w("batch", "a"), &over).unwrap();
        let d = "/c/blkio/agent-interaction/workload/batch/a";
        assert_eq!(
            h.value(&format!("{d}/blkio.throttle.write_bps_device")),
            "254:16 1048576"
        );
        assert_eq!(
            h.value(&format!("{d}/blkio.throttle.read_iops_device")),
            "254:16 500"
        );
        assert_eq!(h.value(&format!("{d}/blkio.throttle.read_bps_device")), "");
        let missing = Limits {
            io: vec![parse_io("/dev/none wbps=1M").unwrap()],
            ..Limits::default()
        };
        assert!(matches!(
            c.create(&w("batch", "b"), &missing),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn a_limit_the_machine_cannot_enforce_is_an_error_and_an_advisory_one_is_reported() {
        let h = FakeHost::new();
        h.add_v1("/c/memory", &["memory"]);
        h.add_v1("/c/freezer", &["freezer"]);
        let l = Layout::detect(&h, Lifecycle::Auto);
        let e = Controller::new(&h, &l, &policy(), 4 * GIB)
            .unwrap()
            .init()
            .unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(ref m) if m.contains("process limits")),
            "{e:?}"
        );

        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::V1);
        let e = setup(&h, &l).init().unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(ref m) if m.contains("termination")),
            "{e:?}"
        );

        let e = Controller::new(
            &FakeHost::new(),
            &Layout::detect(&FakeHost::new(), Lifecycle::Auto),
            &policy(),
            4 * GIB,
        )
        .unwrap()
        .init()
        .unwrap_err();
        assert_eq!(e.code(), 3);
    }

    #[test]
    fn a_policy_that_does_not_fit_the_machine_is_refused_before_anything_is_created() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let e = Controller::new(&h, &l, &policy(), 256 << 20).err().unwrap();
        assert!(matches!(e, Error::Policy(_)) && e.code() == 2);
        assert!(!h.dir_exists("/c/agent-interaction"));
    }

    #[test]
    fn a_machine_without_hierarchy_cannot_hold_a_domain_and_says_so() {
        let h = FakeHost::new();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        let a = w("batch", "a");
        let none = |e: Error| matches!(e, Error::Unsupported(_)) && e.code() == 3;
        assert!(none(c.attach(&Target::System, 5).unwrap_err()));
        assert!(none(c.attach(&Target::Workload(a.clone()), 5).unwrap_err()));
        assert!(none(c.kill(&a).unwrap_err()));
        assert!(none(c.status(SYSTEM).unwrap_err()));
        // with a hierarchy the same request is a mistake of the caller
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        let e = c.attach(&Target::System, 5).unwrap_err();
        assert!(matches!(e, Error::Usage(_)) && e.code() == 2);
        assert_eq!(c.kill(&a).unwrap_err().code(), 2);
    }

    #[test]
    fn attach_moves_a_process_into_every_hierarchy_of_the_domain() {
        let h = hybrid_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        c.create(&w("batch", "a"), &Limits::default()).unwrap();
        h.proc_in(10, &[]);
        c.attach(&Target::Workload(w("batch", "a")), 10).unwrap();
        for hier in ["memory", "pids", "cpu", "cpuset", "blkio", "unified"] {
            assert_eq!(
                h.procs_of(&format!("/c/{hier}/agent-interaction/workload/batch/a")),
                vec![10],
                "{hier}"
            );
        }
        assert_eq!(c.pids(&w("batch", "a").rel()), vec![10]);
        assert_eq!(
            c.pids(WORKLOAD),
            vec![10],
            "the workload domain sees everything below it"
        );
        assert!(c.pids(SYSTEM).is_empty());
        assert!(matches!(
            c.attach(&Target::Workload(w("batch", "a")), 99),
            Err(Error::Io(_))
        ));
        assert!(matches!(
            c.attach(&Target::Workload(w("batch", "a")), 1),
            Err(Error::Refused(_))
        ));
        assert!(matches!(c.attach(&Target::System, 0), Err(Error::Usage(_))));
        c.attach(&Target::System, 10).unwrap();
        assert_eq!(c.pids(SYSTEM), vec![10]);
    }

    #[test]
    fn a_v2_controller_cannot_be_enabled_under_a_domain_that_holds_processes() {
        // the rule that shaped the tree: processes live only in leaves
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        h.proc_in(5, &[]);
        let base = "/c/agent-interaction/cgroup.procs";
        assert!(
            matches!(h.write(base, "5"), Err(ref e) if e.raw_os_error() == Some(crate::fs::fake::EBUSY))
        );
        c.attach(&Target::System, 5).unwrap();
    }

    fn running(c: &Controller, h: &FakeHost, name: &str, pids: &[i32]) -> Workload {
        let wl = w("batch", name);
        c.create(&wl, &Limits::default()).unwrap();
        for &p in pids {
            h.proc_in(p, &[]);
            c.attach(&Target::Workload(wl.clone()), p).unwrap();
        }
        wl
    }

    #[test]
    fn freeze_and_thaw_follow_the_lifecycle_interface() {
        for (h, flag, file, on, off) in [
            (v2_host(), Lifecycle::Auto, "cgroup.freeze", "1", "0"),
            (
                hybrid_host(),
                Lifecycle::V1,
                "freezer.state",
                "FROZEN",
                "THAWED",
            ),
        ] {
            let l = Layout::detect(&h, flag);
            let c = setup(&h, &l);
            c.init().unwrap();
            let wl = running(&c, &h, "a", &[10, 11]);
            c.freeze(&wl).unwrap();
            assert!(c.is_frozen(&wl.rel()) && c.state(&wl.rel()) == "frozen");
            assert!(h.writes_to(file).contains(&on.to_string()));
            c.thaw(&wl).unwrap();
            assert!(!c.is_frozen(&wl.rel()) && c.state(&wl.rel()) == "running");
            assert!(h.writes_to(file).contains(&off.to_string()));
        }
    }

    #[test]
    fn kill_ends_everything_in_one_write_on_v2() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let a = running(&c, &h, "a", &[10, 11]);
        let b = running(&c, &h, "b", &[20]);
        c.kill(&a).unwrap();
        assert!(!h.alive(10) && !h.alive(11) && h.alive(20));
        assert!(
            h.signals().is_empty(),
            "no signal is needed: cgroup.kill reaches every task, forks included"
        );
        assert_eq!(c.state(&b.rel()), "running");
        c.kill(&a).unwrap();
    }

    #[test]
    fn kill_on_v1_thaws_a_frozen_domain_or_the_kill_never_lands() {
        let h = hybrid_host();
        let l = Layout::detect(&h, Lifecycle::V1);
        let c = setup(&h, &l);
        c.init().unwrap();
        let a = running(&c, &h, "a", &[10, 11]);
        c.freeze(&a).unwrap();
        // SIGKILL does not remove a task of a FROZEN v1 group, so the emulation signals and then thaws
        c.kill(&a).unwrap();
        assert!(!h.alive(10) && !h.alive(11));
        assert!(h.signals().iter().filter(|s| s.1 == 9).count() >= 2);
        let order = h.writes_to("freezer.state");
        assert_eq!(
            order.first().map(String::as_str),
            Some("FROZEN"),
            "{order:?}"
        );
        assert_eq!(
            order.last().map(String::as_str),
            Some("THAWED"),
            "{order:?}"
        );
    }

    #[test]
    fn stop_lets_a_process_leave_and_kills_one_that_stays() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let polite = running(&c, &h, "polite", &[10]);
        assert_eq!(c.stop(&polite, 1500).unwrap(), Stopped::Graceful);
        assert!(h.signals().contains(&(10, 15)) && !h.signals().contains(&(10, 9)));
        let stubborn = running(&c, &h, "stubborn", &[20, 21]);
        h.ignore_term(20);
        assert_eq!(c.stop(&stubborn, 1500).unwrap(), Stopped::Killed(1));
        assert!(!h.alive(20) && !h.alive(21));
        let idle = running(&c, &h, "idle", &[]);
        assert_eq!(c.stop(&idle, 100).unwrap(), Stopped::Empty);
    }

    #[test]
    fn stop_thaws_a_frozen_workload_so_it_can_hear_the_signal() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let wl = running(&c, &h, "a", &[10]);
        c.freeze(&wl).unwrap();
        assert_eq!(c.stop(&wl, 1500).unwrap(), Stopped::Graceful);
    }

    #[test]
    fn remove_kills_and_deletes_the_domain_everywhere() {
        let h = hybrid_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let a = running(&c, &h, "a", &[10]);
        c.remove(&a).unwrap();
        assert!(!h.alive(10));
        for hier in ["memory", "pids", "cpu", "cpuset", "blkio", "unified"] {
            assert!(
                !h.dir_exists(&format!("/c/{hier}/agent-interaction/workload/batch/a")),
                "{hier}"
            );
            assert!(
                h.dir_exists(&format!("/c/{hier}/agent-interaction/workload/batch")),
                "{hier}"
            );
        }
        assert!(matches!(c.remove(&a), Err(Error::Usage(_))));
    }

    #[test]
    fn nothing_that_ends_processes_can_reach_the_system_domain() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        h.proc_in(5, &[]);
        c.attach(&Target::System, 5).unwrap();
        let a = running(&c, &h, "a", &[10]);
        c.kill(&a).unwrap();
        c.stop(&running(&c, &h, "b", &[11]), 100).unwrap();
        c.recover(Orphans::Kill).unwrap();
        c.teardown().unwrap();
        assert!(
            h.alive(5),
            "the canary of the system domain survived every lifecycle operation"
        );
        assert_eq!(h.procs_of(&format!("/c/{SYSTEM}")), vec![5]);
        assert!(!h.signals().iter().any(|&(p, _)| p == 5));
        assert!(h.dir_exists(&format!("/c/{SYSTEM}")));
        // the names that would reach it are not workload names at all
        assert!(Workload::parse("system/x").is_err() && Workload::parse("workload/x").is_err());
    }

    #[test]
    fn a_workload_that_forks_many_cannot_escape_the_kill() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let pids: Vec<i32> = (100..116).collect();
        let a = running(&c, &h, "a", &pids);
        c.remove(&a).unwrap();
        assert!(pids.iter().all(|&p| !h.alive(p)));
    }

    #[test]
    fn list_and_status_report_what_is_there() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        assert!(c.list().is_empty());
        let a = running(&c, &h, "a", &[10, 11]);
        let b = running(&c, &h, "b", &[]);
        c.create(&w("interactive", "i"), &Limits::default())
            .unwrap();
        let names: Vec<String> = c
            .list()
            .iter()
            .map(|e| format!("{}:{}:{}", e.workload, e.procs, e.state))
            .collect();
        assert_eq!(
            names,
            vec![
                "batch/a:2:running",
                "batch/b:0:empty",
                "interactive/i:0:empty"
            ]
        );
        h.set(
            "/c/agent-interaction/workload/batch/a/memory.events",
            "low 0\nhigh 3\nmax 5\noom 2\noom_kill 2\n",
        );
        h.set(
            "/c/agent-interaction/workload/batch/a/pids.events",
            "max 7\n",
        );
        h.set(
            "/c/agent-interaction/workload/batch/a/cpu.stat",
            "usage_usec 900\nnr_throttled 4\n",
        );
        let st: std::collections::BTreeMap<String, String> =
            c.status(&a.rel()).unwrap().into_iter().collect();
        assert_eq!(st["procs"], "2");
        assert_eq!(st["memory.oom_kill"], "2");
        assert_eq!(st["memory.high_events"], "3");
        assert_eq!(st["pids.refused"], "7");
        assert_eq!(st["cpu.throttled"], "4");
        assert_eq!(st["cpu.usage_usec"], "900");
        assert_eq!(st["pressure.cpu.some_total"], "0");
        assert!(matches!(
            c.status("agent-interaction/workload/batch/zz"),
            Err(Error::Usage(_))
        ));
        assert!(c.status(SYSTEM).is_ok() && c.status(WORKLOAD).is_ok());
        let _ = b;
    }

    #[test]
    fn status_on_a_hybrid_machine_leaves_out_what_v1_does_not_count() {
        let h = hybrid_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let a = running(&c, &h, "a", &[10]);
        h.set(
            "/c/memory/agent-interaction/workload/batch/a/memory.oom_control",
            "oom_kill_disable 0\nunder_oom 0\noom_kill 1\n",
        );
        let st: std::collections::BTreeMap<String, String> =
            c.status(&a.rel()).unwrap().into_iter().collect();
        assert_eq!(st["memory.oom_kill"], "1");
        assert!(!st.contains_key("memory.high_events") && !st.contains_key("cpu.usage_usec"));
        assert!(
            st.contains_key("pressure.cpu.some_total"),
            "pressure still comes from v2"
        );
    }

    #[test]
    fn v1_memory_hits_are_reported_only_where_the_kernel_counts_them() {
        let h = hybrid_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let held = w("batch", "held");
        let free = w("batch", "free");
        c.create(&held, &Limits::default()).unwrap();
        c.create(
            &free,
            &Limits {
                swap_allowed: Some(true),
                ..Limits::default()
            },
        )
        .unwrap();
        for wl in [&held, &free] {
            h.set(
                &format!(
                    "/c/memory/agent-interaction/workload/batch/{}/memory.failcnt",
                    wl.id
                ),
                "4\n",
            );
        }
        let keys = |wl: &Workload| -> std::collections::BTreeMap<String, String> {
            c.status(&wl.rel()).unwrap().into_iter().collect()
        };
        // memory+swap pinned to the limit: the failure is counted on the memory+swap counter, i.e. nowhere
        assert!(!keys(&held).contains_key("memory.max_events"));
        // swap allowed: the memory limit is the one that fails, and failcnt counts it
        assert_eq!(keys(&free)["memory.max_events"], "4");
    }

    #[test]
    fn recover_clears_what_a_dead_supervisor_left() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        running(&c, &h, "alive", &[10, 11]);
        running(&c, &h, "gone", &[]);
        h.proc_in(5, &[]);
        c.attach(&Target::System, 5).unwrap();
        let done = c.recover(Orphans::Kill).unwrap();
        assert_eq!(done.killed, vec!["batch/alive"]);
        assert_eq!(done.removed, vec!["batch/gone"]);
        assert!(done.kept.is_empty() && c.list().is_empty());
        assert!(!h.alive(10) && h.alive(5));
        let done = c.recover(Orphans::Kill).unwrap();
        assert!(
            done.killed.is_empty() && done.removed.is_empty(),
            "nothing left to recover"
        );
    }

    #[test]
    fn recover_can_keep_running_orphans_and_drops_a_class_the_policy_forgot() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        running(&c, &h, "alive", &[10]);
        h.mkdir("/c/agent-interaction/workload/old").unwrap();
        let done = c.recover(Orphans::Keep).unwrap();
        assert_eq!(done.kept, vec!["batch/alive"]);
        assert!(h.alive(10) && !h.dir_exists("/c/agent-interaction/workload/old"));
    }

    #[test]
    fn teardown_removes_the_workload_tree_and_keeps_a_system_that_is_in_use() {
        let h = hybrid_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        running(&c, &h, "a", &[10]);
        h.proc_in(5, &[]);
        c.attach(&Target::System, 5).unwrap();
        let kept = c.teardown().unwrap();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert!(!h.alive(10) && h.alive(5));
        assert!(!h.dir_exists("/c/memory/agent-interaction/workload"));
        assert!(h.dir_exists("/c/memory/agent-interaction/system"));
        h.proc_in(5, &[]);
        h.signal(5, 9).unwrap();
        assert!(c.teardown().unwrap().is_empty());
        for hier in ["memory", "pids", "cpu", "cpuset", "blkio", "unified"] {
            assert!(
                !h.dir_exists(&format!("/c/{hier}/agent-interaction")),
                "{hier}"
            );
        }
    }

    #[test]
    fn errors_map_to_the_exit_codes() {
        let denied = io_err("x", &io::Error::from_raw_os_error(13));
        let missing = io_err("x", &io::Error::from_raw_os_error(2));
        let busy = io_err("x", &io::Error::from_raw_os_error(16));
        assert_eq!((denied.code(), denied.label()), (3, "denied"));
        assert_eq!((missing.code(), missing.label()), (3, "unsupported"));
        assert_eq!((busy.code(), busy.label()), (1, "error"));
        assert_eq!(Error::Usage(String::new()).code(), 2);
        assert_eq!(keyed("a 1\nb 22\n", "b"), Some(22));
        assert_eq!(
            pressure_total(
                "some avg10=0.00 avg60=0.00 avg300=0.00 total=1234\nfull avg10=0.00 total=9\n"
            ),
            Some(1234)
        );
    }
}
