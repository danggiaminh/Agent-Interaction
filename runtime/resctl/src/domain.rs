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
//!
//! Critical processes live in `system`, outside every kill domain. The controller is given the list of them
//! (`with_critical`) and applies it wherever a process is signalled, killed or moved: one that is found inside a workload
//! domain is moved out before the domain is touched, and one that cannot be moved out is stepped around, never killed.

use crate::fs::Host;
use crate::layout::{Api, Feature, Layout};
use crate::policy::{valid_name, Dev, IoLimit, Limits, Mem, Num, Orphans, Policy, CPU_PERIOD_US};
use std::cell::RefCell;
use std::fmt;
use std::io;

pub const BASE: &str = "agent-interaction";
pub const SYSTEM: &str = "agent-interaction/system";
pub const WORKLOAD: &str = "agent-interaction/workload";
/// The `oom_score_adj` of every process that resctl puts into a workload: the first choice of the OOM killer when the
/// whole machine runs out of memory (an OOM inside a domain chooses among the processes of that domain anyway). Children
/// inherit it, and when resctl runs as root a workload cannot lower it again: the kernel keeps it as the minimum.
pub const OOM_BIAS: i32 = 1000;

/// Whether resctl can lower an `oom_score_adj`, which is what makes the kernel keep the bias of a workload as its minimum
/// and what the OOM preference of critical processes needs: CAP_SYS_RESOURCE in the initial user namespace (that of a
/// user namespace does not count for these settings). The reason when it cannot.
pub fn can_lower_oom(host: &dyn Host) -> std::result::Result<(), String> {
    let status = host
        .read("/proc/self/status")
        .map_err(|e| format!("cannot read /proc/self/status: {e}"))?;
    let caps = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|h| u64::from_str_radix(h.trim(), 16).ok())
        .ok_or("no CapEff in /proc/self/status")?;
    if (caps >> 24) & 1 == 0 {
        return Err("no CAP_SYS_RESOURCE".into());
    }
    let map = host.read("/proc/self/uid_map").unwrap_or_default();
    if map.split_whitespace().collect::<Vec<_>>() != ["0", "0", "4294967295"] {
        return Err("the CAP_SYS_RESOURCE of a user namespace, which these settings ignore".into());
    }
    Ok(())
}

/// What `probe` says about the OOM settings of this machine.
pub fn oom_report(host: &dyn Host) -> Vec<String> {
    let mut out = vec![format!("oom.workload={OOM_BIAS}")];
    out.push(match can_lower_oom(host) {
        Ok(()) => "oom.lower=yes".into(),
        Err(why) => format!(
            "oom.lower=no: {why} (a workload of another user can lower its bias, which its supervisor raises again; \
             critical processes keep their score)"
        ),
    });
    out
}

const FREEZE_MS: u64 = 5_000;
const KILL_MS: u64 = 10_000;
const REMOVE_MS: u64 = 3_000;
/// Rounds of the kill that signals one process at a time (used only around a critical process that cannot be moved out).
const KILL_ROUNDS: u64 = 10;
/// What follows "left alone, so" when a critical process cannot be moved out of a domain that is being killed or removed.
const STAYS: &str = "the domain stays";

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
    /// The request does not fit where the workload is in its lifecycle (an illegal change of phase, a failed workload
    /// that was not reset, a workload that is supervised already).
    Lifecycle(String),
    /// The lifecycle state (records, audit trail, registry) cannot be read, written or locked.
    State(String),
}

impl Error {
    /// Exit code: 2 for what the caller got wrong, 3 for what the machine cannot or will not do, 1 for the rest.
    pub fn code(&self) -> i32 {
        match self {
            Error::Policy(_) | Error::Usage(_) | Error::Refused(_) | Error::Lifecycle(_) => 2,
            Error::Unsupported(_) | Error::Denied(_) => 3,
            Error::Exists(_) | Error::Timeout(_) | Error::Io(_) | Error::State(_) => 1,
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
            Error::Lifecycle(_) => "lifecycle",
            Error::State(_) => "state",
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
            | Error::Io(m)
            | Error::Lifecycle(m)
            | Error::State(m) => {
                write!(f, "{m}")
            }
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_err(what: &str, e: &io::Error) -> Error {
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
    /// Workloads whose supervisor is alive: not orphans, and left alone.
    pub live: Vec<String>,
    /// Workloads whose domain could not be emptied or removed, with the reason. The others were recovered all the same.
    pub stuck: Vec<(String, String)>,
}

/// What `teardown` left behind.
#[derive(Debug, Default)]
pub struct TornDown {
    /// Domains that stay on purpose (`system` while it holds processes).
    pub kept: Vec<String>,
    /// Domains that could not be emptied or removed, with the reason. The others were removed all the same.
    pub stuck: Vec<(String, String)>,
}

/// A critical process that was found inside a workload domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rescue {
    pub pid: i32,
    /// The domain it was found in (relative to the root of the hierarchy).
    pub from: String,
    /// Whether it is out of every workload domain now.
    pub moved: bool,
}

/// Where a process runs, as far as the domains tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Inside the workload domain: the directory (relative to the root of the hierarchy) of the first hierarchy that says so.
    Workload(String),
    System,
    Elsewhere,
}

impl Placement {
    /// The path below the workload domain (`CLASS/ID`, or more for a child domain of the workload), `CLASS` or `workload`
    /// for a process in a directory above a workload.
    pub fn workload(&self) -> Option<String> {
        match self {
            Placement::Workload(rel) => Some(
                rel.strip_prefix(WORKLOAD)
                    .map(|r| r.trim_start_matches('/'))
                    .filter(|r| !r.is_empty())
                    .unwrap_or("workload")
                    .to_string(),
            ),
            _ => None,
        }
    }
}

/// The processes no kill may reach, asked for at every kill so that a registration made a moment ago counts. An error
/// means the registry cannot be read: nothing that kills goes ahead on a guess.
pub type Shield<'a> = &'a dyn Fn() -> Result<Vec<i32>>;

/// `path` is `dir` or below it.
fn inside(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Where the text of a `/proc/PID/cgroup` says a process runs. A process inside the workload domain in any hierarchy
/// counts as inside it: that is the hierarchy a kill may use.
pub fn domain_of_cgroup(text: &str) -> Placement {
    let mut found = Placement::Elsewhere;
    for line in text.lines() {
        // HIERARCHY-ID:CONTROLLERS:PATH, and the path may hold colons
        let Some(path) = line.splitn(3, ':').nth(2) else {
            continue;
        };
        let path = path.strip_suffix(" (deleted)").unwrap_or(path);
        let path = path.trim_start_matches('/');
        if inside(path, WORKLOAD) {
            return Placement::Workload(path.to_string());
        }
        if inside(path, SYSTEM) {
            found = Placement::System;
        }
    }
    found
}

/// The workload domain this very process runs in, if it runs in one: what a workload is, however it came to call this
/// program. A process that cannot be placed is an error, never "not confined".
pub fn confined(host: &dyn Host) -> io::Result<Option<String>> {
    let text = host.read("/proc/self/cgroup")?;
    Ok(domain_of_cgroup(&text).workload())
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

/// A failure of one domain, as against one of the machine: the other domains can still be dealt with.
fn domain_failure(e: &Error) -> bool {
    matches!(e, Error::Refused(_) | Error::Timeout(_) | Error::Io(_))
}

pub struct Controller<'a> {
    host: &'a dyn Host,
    layout: &'a Layout,
    policy: Policy,
    mem_total: u64,
    /// The registered critical processes, when there are any to protect.
    shield: Option<Shield<'a>>,
    /// What was found inside a workload domain and moved out (or could not be) since `take_rescues`.
    rescued: RefCell<Vec<Rescue>>,
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
            shield: None,
            rescued: RefCell::new(Vec::new()),
        })
    }

    /// Protect the processes `shield` names: wherever one is found inside a workload domain that is about to be signalled,
    /// killed or entered, it is moved out first, and it is never signalled or killed.
    pub fn with_critical(mut self, shield: Shield<'a>) -> Controller<'a> {
        self.shield = Some(shield);
        self
    }

    /// The critical processes found inside workload domains (and moved out, or not) since the last call.
    pub fn take_rescues(&self) -> Vec<Rescue> {
        std::mem::take(&mut *self.rescued.borrow_mut())
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
        if matches!(t, Target::Workload(_)) && !self.critical_among(&[pid])?.is_empty() {
            return Err(Error::Refused(format!(
                "process {pid} is critical: it is never moved into a workload"
            )));
        }
        let paths = self.procs_paths(t)?;
        if matches!(t, Target::Workload(_)) {
            // before the move: no process is ever inside a workload without the bias
            let path = format!("/proc/{pid}/oom_score_adj");
            self.host.write(&path, &OOM_BIAS.to_string()).map_err(|e| {
                let what =
                    format!("make process {pid} the first choice of the OOM killer ({path})");
                if e.raw_os_error() == Some(2) {
                    Error::Io(format!("{what}: no such process"))
                } else {
                    io_err(&what, &e)
                }
            })?;
        }
        for path in paths {
            self.host
                .write(&path, &pid.to_string())
                .map_err(|e| io_err(&format!("attach {pid} to {path}"), &e))?;
        }
        Ok(())
    }

    /// Raise again the OOM bias of every process of the workload that lowered it (it can when the kernel does not keep
    /// the bias as its minimum, or when it runs as root): each such process, the score it had, and whether the raise
    /// took. A process that has gone in between is skipped.
    pub fn restore_bias(&self, w: &Workload) -> Vec<(i32, i32, bool)> {
        let mut out = Vec::new();
        for pid in self.pids(&w.rel()) {
            let path = format!("/proc/{pid}/oom_score_adj");
            let Some(now) = self
                .host
                .read(&path)
                .ok()
                .and_then(|t| t.trim().parse::<i32>().ok())
            else {
                continue;
            };
            if now < OOM_BIAS {
                let ok = self.host.write(&path, &OOM_BIAS.to_string()).is_ok();
                out.push((pid, now, ok));
            }
        }
        out
    }

    /// Give a critical process the OOM preference of the policy (`[system] oom.score_adj`); `None` when the policy sets
    /// none. A process whose score is that low already, or lower, keeps it. Lowering needs CAP_SYS_RESOURCE: without it
    /// the score stays as it is and the reason is returned.
    pub fn critical_oom(&self, pid: i32) -> Option<std::result::Result<i32, String>> {
        let want = self.policy.system.oom_score_adj?;
        let path = format!("/proc/{pid}/oom_score_adj");
        let now = match self.host.read(&path) {
            Ok(t) => t.trim().parse::<i32>().ok(),
            Err(e) => return Some(Err(format!("cannot read {path}: {e}"))),
        };
        if let Some(n) = now.filter(|&n| n <= want) {
            return Some(Ok(n));
        }
        Some(
            self.host
                .write(&path, &want.to_string())
                .map(|()| want)
                .map_err(|e| format!("cannot set {path} to {want}: {e}")),
        )
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

    /// A frozen process serves nobody: a critical process inside the domain is moved out first, and one that cannot be
    /// moved out keeps the domain from being frozen.
    pub fn freeze(&self, w: &Workload) -> Result<()> {
        let rel = self.require(w)?;
        let stuck = self.shelter(&rel)?;
        if !stuck.is_empty() {
            return Err(self.stuck_error(&rel, &stuck, "the domain is not frozen"));
        }
        self.set_frozen(&rel, true)
    }

    pub fn thaw(&self, w: &Workload) -> Result<()> {
        let rel = self.require(w)?;
        self.set_frozen(&rel, false)
    }

    // ---- critical processes ----

    /// The members of `pids` that are registered critical processes. An unreadable registry is an error: nothing that
    /// kills or enters a workload goes ahead on a guess.
    fn critical_among(&self, pids: &[i32]) -> Result<Vec<i32>> {
        let Some(shield) = self.shield else {
            return Ok(Vec::new());
        };
        let critical = shield()?;
        Ok(pids
            .iter()
            .copied()
            .filter(|p| critical.contains(p))
            .collect())
    }

    /// The processes of the domain that are not in `keep`.
    fn others(&self, rel: &str, keep: &[i32]) -> Vec<i32> {
        self.pids(rel)
            .into_iter()
            .filter(|p| !keep.contains(p))
            .collect()
    }

    fn note_rescue(&self, r: Rescue) {
        let mut log = self.rescued.borrow_mut();
        if !log.contains(&r) {
            log.push(r);
        }
    }

    /// Take a process into `system`, in every hierarchy (all of them are tried: a process left inside a workload domain in
    /// one hierarchy is still inside it).
    fn to_system(&self, pid: i32) -> Result<()> {
        let mut first = None;
        for path in self.procs_paths(&Target::System)? {
            if let Err(e) = self.host.write(&path, &pid.to_string()) {
                first.get_or_insert_with(|| io_err(&format!("move {pid} to {path}"), &e));
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Where a process runs, by its `/proc/PID/cgroup`.
    pub fn placement(&self, pid: i32) -> io::Result<Placement> {
        self.host
            .read(&format!("/proc/{pid}/cgroup"))
            .map(|t| domain_of_cgroup(&t))
    }

    /// Move the critical processes found inside `rel` out of it. Returns the ones that stay in it because they could not
    /// be moved.
    fn shelter(&self, rel: &str) -> Result<Vec<i32>> {
        let critical = self.critical_among(&self.pids(rel))?;
        let mut stuck = Vec::new();
        for pid in critical {
            let moved = self.to_system(pid).is_ok() && !self.pids(rel).contains(&pid);
            self.note_rescue(Rescue {
                pid,
                from: rel.to_string(),
                moved,
            });
            if !moved {
                stuck.push(pid);
            }
        }
        Ok(stuck)
    }

    /// Every registered critical process that runs inside a workload domain is moved out of it. For a supervisor about to
    /// start a workload, and for recovery after a failure: protection must hold again before anything else happens.
    pub fn rescue_misplaced(&self) -> Result<Vec<Rescue>> {
        let Some(shield) = self.shield else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for pid in shield()? {
            if let Ok(Placement::Workload(from)) = self.placement(pid) {
                let moved = self.to_system(pid).is_ok()
                    && !matches!(self.placement(pid), Ok(Placement::Workload(_)));
                let r = Rescue { pid, from, moved };
                self.note_rescue(r.clone());
                out.push(r);
            }
        }
        Ok(out)
    }

    fn stuck_error(&self, rel: &str, stuck: &[i32], then: &str) -> Error {
        let list: Vec<String> = stuck.iter().map(i32::to_string).collect();
        Error::Refused(format!(
            "{rel} holds critical process(es) {} that cannot be moved out: they are left alone, so {then}",
            list.join(",")
        ))
    }

    /// Kill what is in the domain except `keep`, one process at a time: `cgroup.kill` and the freezer take the whole
    /// domain, `keep` included.
    fn kill_around(&self, rel: &str, keep: &[i32]) -> Result<()> {
        if self.is_frozen(rel) {
            self.set_frozen(rel, false)?;
        }
        for _ in 0..KILL_ROUNDS {
            let rest = self.others(rel, keep);
            if rest.is_empty() {
                return Ok(());
            }
            let _ = self.host.signal(&rest, 9);
            if self
                .wait(KILL_MS / KILL_ROUNDS, "killing", || {
                    self.others(rel, keep).is_empty()
                })
                .is_ok()
            {
                return Ok(());
            }
        }
        Err(Error::Timeout(format!(
            "{rel} still has processes after killing"
        )))
    }

    /// The kernel's count of the processes its out-of-memory killer ended in the domain, where it counts them.
    pub fn oom_kills(&self, rel: &str) -> Option<u64> {
        match self.at(Feature::Memory, rel)? {
            (Api::V2, h) => keyed(&self.get(h, rel, "memory.events")?, "oom_kill"),
            (Api::V1, h) => keyed(&self.get(h, rel, "memory.oom_control")?, "oom_kill"),
        }
    }

    /// Whether the workload has a domain (in any hierarchy).
    pub fn exists(&self, w: &Workload) -> bool {
        self.exists_anywhere(&w.rel())
    }

    /// End every process of the domain, forks included. v2 does it in one write; the v1 freezer cannot (a frozen task does
    /// not die until it is thawed), so it freezes the domain so nothing forks, signals everything, thaws, and checks.
    /// A critical process inside is moved out first. One that cannot be moved is left alone: the rest of the domain is
    /// killed and the domain stays, with a `Refused` that says why.
    fn kill_rel(&self, rel: &str) -> Result<()> {
        if self.pids(rel).is_empty() {
            return Ok(());
        }
        let stuck = self.shelter(rel)?;
        if !stuck.is_empty() {
            self.kill_around(rel, &stuck)?;
            return Err(self.stuck_error(rel, &stuck, STAYS));
        }
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
                    let _ = self.host.signal(&self.pids(rel), 9);
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

    /// SIGTERM to everything, `grace_ms` to exit, then the kill. A critical process inside is moved out first and is never
    /// signalled; one that cannot be moved out is left alone, the rest of the domain still ends, and the call says so
    /// with a `Refused`.
    pub fn stop(&self, w: &Workload, grace_ms: u64) -> Result<Stopped> {
        let rel = self.require(w)?;
        if self.pids(&rel).is_empty() {
            return Ok(Stopped::Empty);
        }
        let stuck = self.shelter(&rel)?;
        let pids = self.others(&rel, &stuck);
        if pids.is_empty() {
            return if stuck.is_empty() {
                Ok(Stopped::Empty)
            } else {
                Err(self.stuck_error(&rel, &stuck, STAYS))
            };
        }
        if self.is_frozen(&rel) {
            self.set_frozen(&rel, false)?;
        }
        let _ = self.host.signal(&pids, 15);
        if self
            .wait(grace_ms, "stopping", || {
                self.others(&rel, &stuck).is_empty()
            })
            .is_ok()
        {
            return if stuck.is_empty() {
                Ok(Stopped::Graceful)
            } else {
                Err(self.stuck_error(&rel, &stuck, STAYS))
            };
        }
        let left = self.others(&rel, &stuck).len();
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
                add("memory.oom_kill", num(self.oom_kills(rel)));
                add("memory.high_events", num(keyed(&ev, "high")));
                add("memory.max_events", num(keyed(&ev, "max")));
            }
            Some((Api::V1, h)) => {
                add("memory.current", self.get(h, rel, "memory.usage_in_bytes"));
                add("memory.peak", self.get(h, rel, "memory.max_usage_in_bytes"));
                add("memory.max", self.get(h, rel, "memory.limit_in_bytes"));
                add("memory.oom_kill", num(self.oom_kills(rel)));
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
        self.recover_with(orphans, &|_| false)
    }

    /// `recover` for a machine where supervisors may be running: `live` says which workloads have one. Those are not
    /// orphans and are left alone. A domain that cannot be emptied or removed is reported and the others are dealt with
    /// all the same.
    pub fn recover_with(
        &self,
        orphans: Orphans,
        live: &dyn Fn(&Workload) -> bool,
    ) -> Result<Recovered> {
        self.init()?;
        let mut done = Recovered::default();
        for e in self.list() {
            let name = e.workload.to_string();
            if live(&e.workload) {
                done.live.push(name);
                continue;
            }
            if e.procs > 0 && orphans == Orphans::Keep {
                done.kept.push(name);
                continue;
            }
            match self.remove(&e.workload) {
                Ok(()) if e.procs == 0 => done.removed.push(name),
                Ok(()) => done.killed.push(name),
                Err(err) if domain_failure(&err) => done.stuck.push((name, err.to_string())),
                Err(err) => return Err(err),
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
    /// processes. A domain that cannot be emptied or removed is reported and the others are removed all the same.
    pub fn teardown(&self) -> Result<TornDown> {
        let mut done = TornDown::default();
        for e in self.list() {
            match self.remove(&e.workload) {
                Ok(()) => {}
                Err(err) if domain_failure(&err) => {
                    done.stuck.push((e.workload.to_string(), err.to_string()));
                }
                Err(err) => return Err(err),
            }
        }
        if done.stuck.is_empty() && self.exists_anywhere(WORKLOAD) {
            match self
                .kill_rel(WORKLOAD)
                .and_then(|()| self.remove_tree(WORKLOAD))
            {
                Ok(()) => {}
                Err(err) if domain_failure(&err) => {
                    done.stuck.push((WORKLOAD.to_string(), err.to_string()));
                }
                Err(err) => return Err(err),
            }
        }
        if self.exists_anywhere(SYSTEM) {
            let n = self.pids(SYSTEM).len();
            if n == 0 && self.union_subdirs(SYSTEM).is_empty() {
                self.remove_tree(SYSTEM)?;
            } else {
                done.kept
                    .push(format!("system: {n} process(es) stay in their domain"));
            }
        }
        if self.exists_anywhere(BASE) && self.union_subdirs(BASE).is_empty() {
            self.remove_tree(BASE)?;
        }
        Ok(done)
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
        let kept = c.teardown().unwrap().kept;
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert!(!h.alive(10) && h.alive(5));
        assert!(!h.dir_exists("/c/memory/agent-interaction/workload"));
        assert!(h.dir_exists("/c/memory/agent-interaction/system"));
        h.proc_in(5, &[]);
        h.signal(&[5], 9).unwrap();
        assert!(c.teardown().unwrap().kept.is_empty());
        for hier in ["memory", "pids", "cpu", "cpuset", "blkio", "unified"] {
            assert!(
                !h.dir_exists(&format!("/c/{hier}/agent-interaction")),
                "{hier}"
            );
        }
    }

    // ---- critical processes ----

    /// A registry that names `pids`.
    fn registry(pids: &[i32]) -> impl Fn() -> Result<Vec<i32>> {
        let pids = pids.to_vec();
        move || Ok(pids.clone())
    }

    /// A registry that cannot be read.
    fn broken_registry() -> Result<Vec<i32>> {
        Err(Error::State("the critical registry is corrupt".into()))
    }

    /// The hosts the lifecycle is tested on: a v2 machine, and a hybrid one that limits through v1.
    fn both() -> [(FakeHost, Lifecycle); 2] {
        [(v2_host(), Lifecycle::Auto), (hybrid_host(), Lifecycle::V1)]
    }

    /// The workload `batch/a`, holding the ordinary processes 10 and 12 and the process 11, which got in before it was
    /// registered as critical (or by a path that was not asked).
    fn holding_a_critical_process(h: &FakeHost, l: &Layout) -> Workload {
        let plain = setup(h, l);
        plain.init().unwrap();
        running(&plain, h, "a", &[10, 11, 12])
    }

    fn signalled(h: &FakeHost, pid: i32) -> bool {
        h.signals().iter().any(|&(p, _)| p == pid)
    }

    #[test]
    fn a_critical_process_inside_a_workload_is_moved_out_before_the_kill() {
        for (h, flag) in both() {
            let l = Layout::detect(&h, flag);
            let a = holding_a_critical_process(&h, &l);
            let shield = registry(&[11]);
            let c = setup(&h, &l).with_critical(&shield);
            c.kill(&a).unwrap();
            assert!(h.alive(11), "the critical process survives the kill");
            assert!(
                !h.alive(10) && !h.alive(12),
                "the rest of the workload ends"
            );
            assert!(!signalled(&h, 11));
            assert_eq!(c.placement(11).unwrap(), Placement::System);
            assert_eq!(
                c.take_rescues(),
                vec![Rescue {
                    pid: 11,
                    from: a.rel(),
                    moved: true
                }]
            );
            assert!(c.take_rescues().is_empty(), "a rescue is reported once");
        }
    }

    #[test]
    fn stop_never_signals_a_critical_process() {
        for stubborn in [false, true] {
            for (h, flag) in both() {
                let l = Layout::detect(&h, flag);
                let a = holding_a_critical_process(&h, &l);
                if stubborn {
                    h.ignore_term(10);
                }
                let shield = registry(&[11]);
                let c = setup(&h, &l).with_critical(&shield);
                let want = if stubborn {
                    Stopped::Killed(1)
                } else {
                    Stopped::Graceful
                };
                assert_eq!(c.stop(&a, 1500).unwrap(), want);
                assert!(h.alive(11) && !h.alive(10) && !h.alive(12));
                assert!(!signalled(&h, 11), "{:?}", h.signals());
                assert_eq!(c.placement(11).unwrap(), Placement::System);
            }
        }
    }

    #[test]
    fn remove_keeps_a_critical_process_alive_and_still_removes_the_domain() {
        for (h, flag) in both() {
            let l = Layout::detect(&h, flag);
            let a = holding_a_critical_process(&h, &l);
            let shield = registry(&[11]);
            let c = setup(&h, &l).with_critical(&shield);
            c.remove(&a).unwrap();
            assert!(h.alive(11) && !h.alive(10) && !h.alive(12));
            assert!(!c.exists(&a));
            assert_eq!(c.placement(11).unwrap(), Placement::System);
            assert!(!signalled(&h, 11));
        }
    }

    #[test]
    fn a_domain_left_with_only_a_critical_process_has_nothing_to_stop() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let plain = setup(&h, &l);
        plain.init().unwrap();
        let a = running(&plain, &h, "a", &[11]);
        let shield = registry(&[11]);
        let c = setup(&h, &l).with_critical(&shield);
        assert_eq!(c.stop(&a, 100).unwrap(), Stopped::Empty);
        assert!(h.alive(11) && h.signals().is_empty());
        assert_eq!(c.placement(11).unwrap(), Placement::System);
    }

    #[test]
    fn a_critical_process_that_cannot_be_moved_is_left_alone_and_the_rest_still_ends() {
        for op in ["kill", "stop", "stop-stubborn", "remove"] {
            let h = v2_host();
            let l = Layout::detect(&h, Lifecycle::Auto);
            let a = holding_a_critical_process(&h, &l);
            h.deny_writes_to("agent-interaction/system/cgroup.procs");
            if op == "stop-stubborn" {
                h.ignore_term(10);
            }
            let shield = registry(&[11]);
            let c = setup(&h, &l).with_critical(&shield);
            let err = match op {
                "kill" => c.kill(&a).unwrap_err(),
                "remove" => c.remove(&a).unwrap_err(),
                _ => c.stop(&a, 1500).unwrap_err(),
            };
            assert!(
                matches!(err, Error::Refused(ref m) if m.contains("11")),
                "{op}: {err}"
            );
            assert_eq!(err.code(), 2);
            assert!(h.alive(11), "{op}: the critical process is never killed");
            assert!(!h.alive(10) && !h.alive(12), "{op}: the others end");
            assert!(!signalled(&h, 11), "{op}: {:?}", h.signals());
            assert!(c.exists(&a), "{op}: the domain stays while it holds it");
            assert_eq!(
                c.take_rescues(),
                vec![Rescue {
                    pid: 11,
                    from: a.rel(),
                    moved: false
                }],
                "{op}"
            );
        }
    }

    #[test]
    fn a_critical_process_is_moved_out_before_its_domain_is_frozen_and_never_frozen_if_it_cannot_be(
    ) {
        for (h, flag) in both() {
            let l = Layout::detect(&h, flag);
            let a = holding_a_critical_process(&h, &l);
            let shield = registry(&[11]);
            let c = setup(&h, &l).with_critical(&shield);
            c.freeze(&a).unwrap();
            assert!(c.is_frozen(&a.rel()));
            assert_eq!(c.placement(11).unwrap(), Placement::System);
            assert_eq!(
                c.take_rescues(),
                vec![Rescue {
                    pid: 11,
                    from: a.rel(),
                    moved: true
                }]
            );
            c.thaw(&a).unwrap();
            assert!(!c.is_frozen(&a.rel()));
        }
        for (h, flag) in both() {
            let l = Layout::detect(&h, flag);
            let a = holding_a_critical_process(&h, &l);
            h.deny_writes_to("agent-interaction/system/cgroup.procs");
            let shield = registry(&[11]);
            let c = setup(&h, &l).with_critical(&shield);
            let err = c.freeze(&a).unwrap_err();
            assert!(
                matches!(err, Error::Refused(ref m) if m.contains("11") && m.contains("not frozen")),
                "{err}"
            );
            assert_eq!(err.code(), 2);
            assert!(!c.is_frozen(&a.rel()), "the domain is not frozen");
            assert!(h.alive(10) && h.alive(11) && h.alive(12));
        }
    }

    #[test]
    fn a_process_put_into_a_workload_is_the_first_choice_of_the_oom_killer() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let a = running(&c, &h, "a", &[10]);
        h.proc_in(11, &[]);
        c.attach(&Target::Workload(a.clone()), 11).unwrap();
        assert_eq!(h.value("/proc/11/oom_score_adj"), OOM_BIAS.to_string());
        // the system domain is not biased
        h.proc_in(12, &[]);
        c.attach(&Target::System, 12).unwrap();
        assert!(h.writes_to("/proc/12/oom_score_adj").is_empty());
        // without the bias a process does not enter at all
        h.deny_writes_to("/13/oom_score_adj");
        h.proc_in(13, &[]);
        let err = c.attach(&Target::Workload(a), 13).unwrap_err();
        assert!(
            err.to_string().contains("first choice of the OOM killer"),
            "{err}"
        );
        assert_eq!(c.placement(13).unwrap(), Placement::Elsewhere);
    }

    #[test]
    fn a_lowered_bias_is_raised_again_and_a_refused_raise_is_reported() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let a = running(&c, &h, "a", &[10, 11]);
        assert!(
            c.restore_bias(&a).is_empty(),
            "nothing lowered, nothing raised"
        );
        h.set("/proc/11/oom_score_adj", "0");
        assert_eq!(c.restore_bias(&a), vec![(11, 0, true)]);
        assert_eq!(h.value("/proc/11/oom_score_adj"), OOM_BIAS.to_string());
        h.set("/proc/10/oom_score_adj", "-5");
        h.deny_writes_to("/10/oom_score_adj");
        assert_eq!(c.restore_bias(&a), vec![(10, -5, false)]);
    }

    #[test]
    fn a_critical_process_gets_the_oom_preference_of_the_policy_and_keeps_a_lower_one() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        h.proc_in(20, &[]);
        assert_eq!(
            setup(&h, &l).critical_oom(20),
            None,
            "no oom.score_adj, no change"
        );
        let mut p = policy();
        p.system.oom_score_adj = Some(-500);
        let c = Controller::new(&h, &l, &p, 4 * GIB).unwrap();
        assert_eq!(c.critical_oom(20), Some(Ok(-500)));
        assert_eq!(h.value("/proc/20/oom_score_adj"), "-500");
        h.proc_in(21, &[]);
        h.set("/proc/21/oom_score_adj", "-900");
        assert_eq!(c.critical_oom(21), Some(Ok(-900)), "a lower score is kept");
        assert!(h.writes_to("/proc/21/oom_score_adj").is_empty());
        // lowering needs CAP_SYS_RESOURCE: the kernel refuses it, the score stays and the reason is given
        h.proc_in(22, &[]);
        h.deny_writes_to("/22/oom_score_adj");
        let e = c.critical_oom(22).unwrap().unwrap_err();
        assert!(
            e.contains("cannot set /proc/22/oom_score_adj to -500"),
            "{e}"
        );
        assert!(
            h.value("/proc/22/oom_score_adj").is_empty(),
            "the score is not changed"
        );
        assert!(
            c.critical_oom(99).unwrap().is_err(),
            "a process that is gone"
        );
    }

    #[test]
    fn the_oom_report_says_whether_resctl_can_lower_a_score() {
        let h = FakeHost::new();
        h.set("/proc/self/uid_map", "         0          0 4294967295\n");
        assert_eq!(oom_report(&h), ["oom.workload=1000", "oom.lower=yes"]);
        h.set_cap_eff(0x1ff_feff_ffff);
        assert!(oom_report(&h)[1].starts_with("oom.lower=no: no CAP_SYS_RESOURCE"));
        h.set_cap_eff(0x1ff_ffff_ffff);
        h.set("/proc/self/uid_map", "0 100000 65536\n");
        assert!(
            oom_report(&h)[1].contains("user namespace"),
            "{:?}",
            oom_report(&h)
        );
    }

    #[test]
    fn attach_never_puts_a_critical_process_into_a_workload() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let plain = setup(&h, &l);
        plain.init().unwrap();
        let a = running(&plain, &h, "a", &[10]);
        h.proc_in(5, &[]);
        let shield = registry(&[5]);
        let c = setup(&h, &l).with_critical(&shield);
        let err = c.attach(&Target::Workload(a.clone()), 5).unwrap_err();
        assert!(matches!(err, Error::Refused(_)), "{err}");
        assert!(!h
            .writes_to("workload/batch/a/cgroup.procs")
            .contains(&"5".to_string()));
        assert_eq!(c.placement(5).unwrap(), Placement::Elsewhere);
        c.attach(&Target::System, 5).unwrap();
        assert_eq!(c.placement(5).unwrap(), Placement::System);
        // an ordinary process still enters, and process 1 never does
        h.proc_in(6, &[]);
        c.attach(&Target::Workload(a.clone()), 6).unwrap();
        assert_eq!(c.placement(6).unwrap().workload(), Some("batch/a".into()));
        assert!(matches!(
            c.attach(&Target::Workload(a), 1),
            Err(Error::Refused(_))
        ));
    }

    #[test]
    fn an_unreadable_registry_stops_everything_that_would_kill_or_enter() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let a = holding_a_critical_process(&h, &l);
        let c = setup(&h, &l).with_critical(&broken_registry);
        for err in [
            c.kill(&a).unwrap_err(),
            c.stop(&a, 100).unwrap_err(),
            c.remove(&a).unwrap_err(),
            c.freeze(&a).unwrap_err(),
            c.rescue_misplaced().unwrap_err(),
        ] {
            assert!(matches!(err, Error::State(_)), "{err}");
        }
        assert!(!c.is_frozen(&a.rel()));
        assert!(h.alive(10) && h.alive(11) && h.alive(12), "nothing ended");
        assert!(h.signals().is_empty());
        assert!(c.exists(&a));
        h.proc_in(6, &[]);
        assert!(matches!(
            c.attach(&Target::Workload(a), 6),
            Err(Error::State(_))
        ));
        c.attach(&Target::System, 6).unwrap();
    }

    #[test]
    fn rescue_moves_every_critical_process_out_of_the_workload_domains() {
        for (h, flag) in both() {
            let l = Layout::detect(&h, flag);
            let plain = setup(&h, &l);
            plain.init().unwrap();
            let a = running(&plain, &h, "a", &[5, 10]);
            let b = running(&plain, &h, "b", &[6]);
            h.proc_in(7, &[]);
            h.proc_in(8, &[]);
            plain.attach(&Target::System, 8).unwrap();
            // 99 is not a process any more: it is not placed, so there is nothing to move
            let shield = registry(&[5, 6, 7, 8, 99]);
            let c = setup(&h, &l).with_critical(&shield);
            let got = c.rescue_misplaced().unwrap();
            assert_eq!(
                got,
                vec![
                    Rescue {
                        pid: 5,
                        from: a.rel(),
                        moved: true
                    },
                    Rescue {
                        pid: 6,
                        from: b.rel(),
                        moved: true
                    },
                ]
            );
            assert_eq!(c.placement(5).unwrap(), Placement::System);
            assert_eq!(c.placement(6).unwrap(), Placement::System);
            assert_eq!(c.placement(7).unwrap(), Placement::Elsewhere);
            assert_eq!(c.placement(8).unwrap(), Placement::System);
            assert!(h.alive(10), "rescue moves processes, it ends none");
            assert!(c.rescue_misplaced().unwrap().is_empty());
            assert_eq!(c.take_rescues(), got);
        }
    }

    #[test]
    fn rescue_reports_a_process_it_could_not_move() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let plain = setup(&h, &l);
        plain.init().unwrap();
        let a = running(&plain, &h, "a", &[5]);
        h.deny_writes_to("agent-interaction/system/cgroup.procs");
        let shield = registry(&[5]);
        let c = setup(&h, &l).with_critical(&shield);
        assert_eq!(
            c.rescue_misplaced().unwrap(),
            vec![Rescue {
                pid: 5,
                from: a.rel(),
                moved: false
            }]
        );
        assert!(h.alive(5));
    }

    #[test]
    fn the_placement_of_a_process_is_read_from_its_cgroup_file() {
        let wl = |s: &str| Placement::Workload(s.to_string());
        assert_eq!(
            domain_of_cgroup("0::/agent-interaction/workload/batch/a\n"),
            wl("agent-interaction/workload/batch/a")
        );
        assert_eq!(
            domain_of_cgroup("0::/agent-interaction/system\n"),
            Placement::System
        );
        assert_eq!(
            domain_of_cgroup("0::/agent-interaction/system/sub\n"),
            Placement::System
        );
        for elsewhere in [
            "",
            "0::/\n",
            "0::/user.slice\n",
            "garbage\n",
            "0::/foo/agent-interaction/workload/x\n",
            "0::/agent-interaction/workload2/x\n",
            "0::/agent-interaction/systemd\n",
            "0::/agent-interaction\n",
        ] {
            assert_eq!(
                domain_of_cgroup(elsewhere),
                Placement::Elsewhere,
                "{elsewhere:?}"
            );
        }
        // one hierarchy that puts it inside a workload is enough
        let hybrid = "5:memory:/agent-interaction/system\n\
                      4:pids:/agent-interaction/workload/batch/a\n\
                      1:name=systemd:/\n\
                      0::/\n";
        assert_eq!(
            domain_of_cgroup(hybrid),
            wl("agent-interaction/workload/batch/a")
        );
        // a path may hold colons and a mark for a removed directory
        assert_eq!(
            domain_of_cgroup("0::/agent-interaction/workload/batch/a:b (deleted)\n"),
            wl("agent-interaction/workload/batch/a:b")
        );
        assert_eq!(
            domain_of_cgroup("0::/agent-interaction/workload/batch/a/child/leaf\n"),
            wl("agent-interaction/workload/batch/a/child/leaf")
        );
    }

    #[test]
    fn a_placement_names_the_workload_it_is_in() {
        let name = |text: &str| domain_of_cgroup(text).workload();
        assert_eq!(
            name("0::/agent-interaction/workload/batch/a\n"),
            Some("batch/a".into())
        );
        assert_eq!(
            name("0::/agent-interaction/workload/batch\n"),
            Some("batch".into())
        );
        assert_eq!(
            name("0::/agent-interaction/workload\n"),
            Some("workload".into())
        );
        assert_eq!(name("0::/agent-interaction/system\n"), None);
        assert_eq!(name("0::/\n"), None);
    }

    #[test]
    fn a_process_is_confined_when_its_own_domain_is_a_workload() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let a = running(&c, &h, "a", &[10]);
        assert_eq!(
            confined(&h).unwrap(),
            None,
            "the default process is outside"
        );
        h.set_self(10);
        assert_eq!(confined(&h).unwrap(), Some(a.to_string()));
        c.attach(&Target::System, 10).unwrap();
        assert_eq!(confined(&h).unwrap(), None);
    }

    #[test]
    fn oom_kills_are_read_where_the_kernel_counts_them() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let a = running(&c, &h, "a", &[]);
        assert_eq!(c.oom_kills(&a.rel()), Some(0));
        h.set(
            "/c/agent-interaction/workload/batch/a/memory.events",
            "low 0\nhigh 0\nmax 4\noom 2\noom_kill 2\n",
        );
        assert_eq!(c.oom_kills(&a.rel()), Some(2));
        assert_eq!(c.oom_kills("agent-interaction/workload/batch/none"), None);

        let h = hybrid_host();
        let l = Layout::detect(&h, Lifecycle::V1);
        let c = setup(&h, &l);
        c.init().unwrap();
        let a = running(&c, &h, "a", &[]);
        h.set(
            "/c/memory/agent-interaction/workload/batch/a/memory.oom_control",
            "oom_kill_disable 0\nunder_oom 0\noom_kill 3\n",
        );
        assert_eq!(c.oom_kills(&a.rel()), Some(3));
    }

    #[test]
    fn recover_leaves_the_workload_of_a_live_supervisor_alone() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let c = setup(&h, &l);
        c.init().unwrap();
        let a = running(&c, &h, "a", &[10]);
        running(&c, &h, "b", &[20]);
        let done = c
            .recover_with(Orphans::Kill, &|wl: &Workload| wl.id == "a")
            .unwrap();
        assert_eq!(done.live, vec!["batch/a"]);
        assert_eq!(done.killed, vec!["batch/b"]);
        assert!(done.removed.is_empty() && done.stuck.is_empty());
        assert!(h.alive(10) && !h.alive(20) && c.exists(&a));
    }

    #[test]
    fn recover_and_teardown_keep_a_critical_process_alive_wherever_it_was_found() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let a = holding_a_critical_process(&h, &l);
        let shield = registry(&[11]);
        let c = setup(&h, &l).with_critical(&shield);
        let done = c.recover(Orphans::Kill).unwrap();
        assert_eq!(done.killed, vec![a.to_string()]);
        assert!(h.alive(11) && !h.alive(10) && !h.alive(12));
        assert_eq!(c.placement(11).unwrap(), Placement::System);
        let down = c.teardown().unwrap();
        assert!(down.stuck.is_empty());
        assert_eq!(down.kept.len(), 1, "{:?}", down.kept);
        assert!(down.kept[0].starts_with("system:"));
        assert!(h.alive(11) && !signalled(&h, 11));
    }

    #[test]
    fn recover_reports_a_stuck_workload_and_recovers_the_others() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let a = holding_a_critical_process(&h, &l);
        let plain = setup(&h, &l);
        running(&plain, &h, "b", &[20]);
        running(&plain, &h, "gone", &[]);
        h.deny_writes_to("agent-interaction/system/cgroup.procs");
        let shield = registry(&[11]);
        let c = setup(&h, &l).with_critical(&shield);
        let done = c.recover(Orphans::Kill).unwrap();
        assert_eq!(done.stuck.len(), 1, "{:?}", done.stuck);
        assert_eq!(done.stuck[0].0, a.to_string());
        assert!(done.stuck[0].1.contains("critical"), "{:?}", done.stuck);
        assert_eq!(done.killed, vec!["batch/b"]);
        assert_eq!(done.removed, vec!["batch/gone"]);
        assert!(h.alive(11) && !h.alive(10) && !h.alive(20));
        assert!(c.exists(&a));
    }

    #[test]
    fn recover_stops_at_a_failure_of_the_machine_instead_of_guessing() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let a = holding_a_critical_process(&h, &l);
        let c = setup(&h, &l).with_critical(&broken_registry);
        assert!(matches!(c.recover(Orphans::Kill), Err(Error::State(_))));
        assert!(h.alive(10) && h.alive(11) && h.alive(12));
        assert!(c.exists(&a));
    }

    #[test]
    fn teardown_reports_what_it_could_not_remove_and_removes_the_rest() {
        let h = v2_host();
        let l = Layout::detect(&h, Lifecycle::Auto);
        let a = holding_a_critical_process(&h, &l);
        let plain = setup(&h, &l);
        let b = running(&plain, &h, "b", &[20]);
        h.deny_writes_to("agent-interaction/system/cgroup.procs");
        let shield = registry(&[11]);
        let c = setup(&h, &l).with_critical(&shield);
        let down = c.teardown().unwrap();
        assert_eq!(down.stuck.len(), 1, "{:?}", down.stuck);
        assert_eq!(down.stuck[0].0, a.to_string());
        assert!(!c.exists(&b) && !h.alive(20));
        assert!(c.exists(&a) && h.alive(11) && !h.alive(10));
        assert!(
            h.dir_exists("/c/agent-interaction/workload"),
            "the workload domain stays while one of its domains does"
        );
        // once the process that held the domain is gone, the next teardown finishes the job
        h.exit_proc(11);
        let down = c.teardown().unwrap();
        assert!(down.stuck.is_empty() && down.kept.is_empty(), "{down:?}");
        assert!(!h.dir_exists("/c/agent-interaction"));
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
