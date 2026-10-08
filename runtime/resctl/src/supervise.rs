//! Supervision of workloads: one supervisor process per workload starts it in its domain, watches it, tells an
//! unexpected exit from a requested one, restarts it within the limits of the policy and, whatever happens, leaves no
//! process and no domain behind. The state of every step is a phase of the lifecycle record (`life.rs`, `store.rs`).
//!
//! Everything that touches the machine goes through `Host` (files, signals, time), `Controller` (domains) and `Procs`
//! (the processes this supervisor starts), so that the whole of it is tested against fakes. See ../README.md.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{self, Child, Command, Stdio};

use crate::domain::{
    Controller, Error, Recovered, Result, Stopped, Target, TornDown, Workload, SYSTEM,
};
use crate::fs::{Guard, Host};
use crate::life::{
    classify, decide, tidy, Ending, Exit, History, Intent, Phase, Record, Verdict, Why,
};
use crate::policy::{Limits, Orphans, Restart, RestartMode};
use crate::store::{record_rel, Audit, Event, Listed, Locked, Store, Stored, Sup, RECORDS};

/// How often a supervisor looks at its workload and at the state.
const POLL_MS: u64 = 50;
/// How long a main process that was just killed has to show up as ended before it is killed directly.
const SETTLE_MS: u64 = 1_000;
/// How long it then has to end.
const REAP_MS: u64 = 5_000;
/// How long an operator waits for a supervisor to finish, once the grace period of a stop is over.
const FINISH_MS: u64 = 20_000;
/// The longest sleep between two looks at the state: a request is seen within this time.
const SLICE_MS: u64 = 200;

// ---------------------------------------------------------------------------------------------------------------------
// The processes of this supervisor.

/// A program to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSpec {
    pub exe: String,
    pub args: Vec<String>,
    /// Nobody waits for it and it has no output: the reaper of a workload.
    pub detach: bool,
}

/// What a supervisor does to the processes it starts. A seam: the tests run a script instead.
pub trait Procs {
    /// Start the program, in a process group of its own; its process id.
    fn spawn(&self, spec: &RunSpec) -> io::Result<i32>;
    /// `None` while it runs. A process that is not a child of this supervisor is an error.
    fn try_wait(&self, pid: i32) -> io::Result<Option<Exit>>;
    /// Kill the child itself (not its domain). A child that ended already is no error.
    fn kill(&self, pid: i32) -> io::Result<()>;
}

/// The processes of the machine.
#[derive(Default)]
pub struct RealProcs {
    children: RefCell<HashMap<i32, Child>>,
}

fn exit_of(st: process::ExitStatus) -> Exit {
    if let Some(c) = st.code() {
        Exit::Code(c.clamp(0, 255))
    } else if let Some(s) = st.signal() {
        Exit::Signal(s.clamp(1, 64))
    } else {
        Exit::Code(255)
    }
}

impl Procs for RealProcs {
    fn spawn(&self, spec: &RunSpec) -> io::Result<i32> {
        let mut cmd = Command::new(&spec.exe);
        cmd.args(&spec.args).stdin(Stdio::null()).process_group(0);
        if spec.detach {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let child = cmd.spawn()?;
        let pid = i32::try_from(child.id())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "process id out of range"))?;
        if !spec.detach {
            self.children.borrow_mut().insert(pid, child);
        }
        Ok(pid)
    }

    fn try_wait(&self, pid: i32) -> io::Result<Option<Exit>> {
        let mut kids = self.children.borrow_mut();
        let Some(child) = kids.get_mut(&pid) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("process {pid} is not a child of this supervisor"),
            ));
        };
        match child.try_wait()? {
            Some(st) => {
                kids.remove(&pid);
                Ok(Some(exit_of(st)))
            }
            None => Ok(None),
        }
    }

    fn kill(&self, pid: i32) -> io::Result<()> {
        match self.children.borrow_mut().get_mut(&pid) {
            Some(child) => child.kill(),
            None => Ok(()),
        }
    }
}

/// How a supervisor starts the helper processes of a workload: this program again, with the same global options.
#[derive(Clone, Debug)]
pub struct Env {
    pub exe: String,
    /// `--policy P --lifecycle L --state-dir D`.
    pub globals: Vec<String>,
}

impl Env {
    fn spec(&self, tail: Vec<String>, detach: bool) -> RunSpec {
        let mut args = self.globals.clone();
        args.extend(tail);
        RunSpec {
            exe: self.exe.clone(),
            args,
            detach,
        }
    }

    /// The main process: it moves itself into the domain and then becomes the workload (`prepare_main`, `exec_main`).
    pub fn enter_spec(&self, w: &Workload, argv: &[String]) -> RunSpec {
        let mut tail = vec!["__enter".to_string(), w.to_string(), "--".to_string()];
        tail.extend(argv.iter().cloned());
        self.spec(tail, false)
    }

    /// The reaper: it waits for the supervisor to end and cleans up after it if it did not finish.
    pub fn reap_spec(&self, w: &Workload) -> RunSpec {
        self.spec(vec!["__reap".to_string(), w.to_string()], true)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// What is asked, what is found.

/// What to supervise.
#[derive(Clone, Debug)]
pub struct Job {
    pub workload: Workload,
    pub limits: Limits,
    pub argv: Vec<String>,
    /// Instead of the restart mode of the policy; the limits of the policy stay.
    pub mode: Option<RestartMode>,
}

/// Everything a supervisor and an operator work with.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub host: &'a dyn Host,
    pub c: &'a Controller<'a>,
    pub s: &'a Store<'a>,
    pub p: &'a dyn Procs,
    pub env: &'a Env,
}

/// How a supervised workload ended up.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub phase: Phase,
    pub gen: u32,
    pub restarts: u32,
    pub reason: String,
}

impl Outcome {
    /// 0 when the workload was stopped or finished, 1 when it failed.
    pub fn code(&self) -> i32 {
        if self.phase == Phase::Failed {
            1
        } else {
            0
        }
    }
}

/// How `terminate` ended a workload.
#[derive(Debug, PartialEq, Eq)]
pub enum Terminated {
    /// The workload has no lifecycle record: it was started by hand. The caller ends its domain itself.
    Unmanaged,
    /// The record was in this final phase already. The caller ends what is left of the domain itself.
    Already(Phase),
    /// Its supervisor ended it and finished in this phase.
    Supervised(Phase),
    /// Its supervisor was gone: it was cleaned up from here and the record is in this final phase.
    Swept(Phase),
}

/// What `recover` did besides what the domains show.
#[derive(Debug, Default)]
pub struct Recovery {
    pub domains: Recovered,
    /// Workloads whose supervisor was gone, and the phase they are in now.
    pub swept: Vec<(String, Phase)>,
    /// Files of the state that were unreadable, moved aside.
    pub quarantined: Vec<String>,
    /// The registry of critical processes was unreadable and was rebuilt.
    pub healed: bool,
    /// Entries of the registry that named a process that is gone.
    pub pruned: usize,
}

/// What `teardown` did.
#[derive(Debug)]
pub struct Teardown {
    pub domains: TornDown,
    /// Records of workloads that are gone.
    pub forgotten: Vec<String>,
}

/// The program of a workload, and who it runs as.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

// ---------------------------------------------------------------------------------------------------------------------
// Internals.

/// An error of a supervisor: `Handled` was recorded where it belongs; for an `Unexpected` one the supervisor cleans up.
enum Fault {
    Handled(Error),
    Unexpected(Error),
}

impl From<Error> for Fault {
    fn from(e: Error) -> Fault {
        Fault::Unexpected(e)
    }
}

type Flow<T> = std::result::Result<T, Fault>;

enum Launched {
    Up(i32),
    Broke(String),
}

enum Watched {
    Exited(Exit),
    Asked,
    Lost(String),
}

enum Ran {
    /// It ended; `None`: it ended but its status could not be read.
    Exit(Option<Exit>),
    Broke(String),
    Lost(String),
}

#[derive(PartialEq)]
enum Reserved {
    Go,
    Asked,
}

enum Plan {
    Wait,
    Sweep(Guard, Record),
    Done(Terminated),
}

struct Swept {
    stuck: Option<String>,
    oom: bool,
}

fn my_pid() -> i32 {
    i32::try_from(process::id()).unwrap_or(0)
}

/// Whether the supervisor of `w` may be running. When that cannot be told, it is assumed: nothing is cleaned up on a guess.
fn alive(ctx: Ctx<'_>, w: &Workload) -> bool {
    !ctx.s.sup_state(w).is_ok_and(|s| s != Sup::Alive)
}

fn field(s: &str) -> String {
    let t = tidy(s).replace(' ', "_");
    if t.is_empty() {
        "-".to_string()
    } else {
        t
    }
}

fn show<T: ToString>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_string(), |x| field(&x.to_string()))
}

fn outcome(rec: &Record) -> Outcome {
    Outcome {
        phase: rec.phase,
        gen: rec.gen,
        restarts: rec.restarts,
        reason: rec.reason.clone(),
    }
}

fn restart_policy(ctx: Ctx<'_>, mode: Option<RestartMode>) -> Restart {
    let pol = &ctx.c.policy().restart;
    Restart {
        mode: mode.unwrap_or(pol.mode),
        ..pol.clone()
    }
}

fn try_save(
    ctx: Ctx<'_>,
    w: &Workload,
    rec: &mut Record,
    event: Event,
    detail: &str,
) -> Result<()> {
    let lock = ctx.s.lock()?;
    if !rec.phase.is_terminal() {
        if let Ok(Stored::Record(stored)) = lock.peek(w) {
            if let Some(i) = stored.intent {
                rec.ask(i);
            }
        }
    }
    lock.commit(rec, Audit::Optional, event, detail)
}

/// Write the record, with what an operator asked since merged in. A record that cannot be written is a warning, never a
/// reason to leave the workload running: the supervisor goes on with what it knows.
fn save(ctx: Ctx<'_>, w: &Workload, rec: &mut Record, event: Event, detail: &str) {
    if let Err(e) = try_save(ctx, w, rec, event, detail) {
        ctx.s
            .warn_out(format!("{w}: the lifecycle record was not written: {e}"));
    }
}

/// Change the phase and write it down. A change the lifecycle does not allow is an error and changes nothing.
fn go(
    ctx: Ctx<'_>,
    w: &Workload,
    rec: &mut Record,
    to: Phase,
    event: Event,
    why: &str,
) -> Result<()> {
    rec.advance(to, ctx.host.epoch_secs())
        .map_err(Error::Lifecycle)?;
    rec.set_reason(why);
    let detail = format!("phase={} gen={} {}", to.name(), rec.gen, tidy(why));
    save(ctx, w, rec, event, &detail);
    Ok(())
}

fn recovering(ctx: Ctx<'_>, w: &Workload, rec: &mut Record, why: &str) -> Result<()> {
    if rec.phase == Phase::Recovering {
        return Ok(());
    }
    go(ctx, w, rec, Phase::Recovering, Event::Recover, why)
}

fn peek_intent(ctx: Ctx<'_>, w: &Workload) -> Option<Intent> {
    match ctx.s.peek(w) {
        Ok(Stored::Record(r)) => r.intent,
        _ => None,
    }
}

/// Whether an operator asked to end the run; what was asked is merged into the record.
fn asked(ctx: Ctx<'_>, w: &Workload, rec: &mut Record) -> bool {
    if let Some(i) = peek_intent(ctx, w) {
        rec.ask(i);
    }
    rec.intent.is_some()
}

fn audit_opt(ctx: Ctx<'_>, event: Event, subject: &str, detail: &str) {
    match ctx.s.lock() {
        Ok(l) => {
            let _ = l.audit(Audit::Optional, event, subject, detail);
        }
        Err(e) => ctx.s.warn_out(format!("{subject}: not audited: {e}")),
    }
}

/// Remove the domain with everything in it. `Err`: it is stuck, and why. Never call it with the state locked.
fn clear_domain(ctx: Ctx<'_>, w: &Workload) -> std::result::Result<(), String> {
    if !ctx.c.exists(w) {
        return Ok(());
    }
    let r = ctx.c.remove(w);
    flush_rescues(ctx.c, ctx.s);
    match r {
        Ok(()) => Ok(()),
        Err(_) if !ctx.c.exists(w) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// How many processes of the domain a kill ends: the critical ones among them are moved out, never killed.
fn killable(ctx: Ctx<'_>, w: &Workload) -> usize {
    if !ctx.c.exists(w) {
        return 0;
    }
    let shield = ctx.s.critical_pids().unwrap_or_default();
    ctx.c
        .pids(&w.rel())
        .iter()
        .filter(|p| !shield.contains(p))
        .count()
}

/// Add the stuck domains that `more` names and `into` does not know yet.
fn merge_stuck(into: &mut Vec<(String, String)>, more: Vec<(String, String)>) {
    for m in more {
        if !into.iter().any(|(name, _)| *name == m.0) {
            into.push(m);
        }
    }
}

/// Audit the critical processes the controller found inside a workload domain. It locks the state itself.
pub fn flush_rescues(c: &Controller<'_>, s: &Store<'_>) {
    let found = c.take_rescues();
    if found.is_empty() {
        return;
    }
    match s.lock() {
        Ok(lock) => {
            for r in &found {
                let _ = lock.audit(
                    Audit::Optional,
                    Event::Rescue,
                    &format!("pid{}", r.pid),
                    &format!("found in {} moved={}", r.from, r.moved),
                );
            }
        }
        Err(e) => s.warn_out(format!(
            "{} critical process(es) were found in a workload domain and are not audited: {e}",
            found.len()
        )),
    }
}

/// An unreadable registry of critical processes is set aside and rebuilt from what runs in `system`: everything there is
/// critical by placement. Without it every kill would be refused. Whether it was rebuilt.
pub fn heal(c: &Controller<'_>, s: &Store<'_>) -> Result<bool> {
    if s.registry_corrupt()?.is_none() {
        return Ok(false);
    }
    let lock = s.lock()?;
    let Some(why) = lock.registry_corrupt()? else {
        return Ok(false);
    };
    lock.quarantine("critical", &why)?;
    let mut entries = Vec::new();
    for pid in c.pids(SYSTEM) {
        if pid < 2 {
            continue;
        }
        match lock.entry_for(pid, &format!("pid{pid}")) {
            Ok(Some(e)) => entries.push(e),
            Ok(None) => {}
            Err(e) => lock.warn_out(format!("pid {pid} of system is not registered: {e}")),
        }
    }
    lock.replace_critical(entries)?;
    Ok(true)
}

/// What every command that signals or ends processes does first: repair the registry of critical processes and move
/// back out of the workload domains any critical process that was found in one. Whether the registry was rebuilt.
pub fn prelude(c: &Controller<'_>, s: &Store<'_>) -> Result<bool> {
    let healed = heal(c, s)?;
    let r = c.rescue_misplaced();
    flush_rescues(c, s);
    r?;
    Ok(healed)
}

/// End a start that cannot go on (no domain was running): the record fails, nothing is swept.
fn fail_plain(ctx: Ctx<'_>, w: &Workload, rec: &mut Record, why: &str) {
    rec.ended = Some(Ending::LaunchFailed);
    rec.status = None;
    let text = format!("cannot start: {why}");
    if !rec.phase.can_go(Phase::Failed) {
        let _ = go(ctx, w, rec, Phase::Recovering, Event::Recover, &text);
    }
    let _ = go(ctx, w, rec, Phase::Failed, Event::Phase, &text);
}

/// The supervisor met an error it does not understand: whatever it started is killed and removed, and the record fails.
fn abandon(ctx: Ctx<'_>, w: &Workload, rec: &mut Record, err: &Error) {
    if let Some(pid) = rec.main {
        let _ = ctx.p.kill(pid);
    }
    if !rec.phase.is_terminal() {
        let _ = recovering(ctx, w, rec, "an internal error");
    }
    let stuck = clear_domain(ctx, w).err();
    if let Some(pid) = rec.main.take() {
        reap_main(ctx, pid);
    }
    rec.ended = Some(Ending::Lost);
    rec.status = None;
    let mut why = format!("internal error: {err}");
    if let Some(s) = stuck {
        why.push_str(&format!("; the domain was not removed: {s}"));
    }
    if !rec.phase.is_terminal() {
        let _ = go(ctx, w, rec, Phase::Failed, Event::Phase, &why);
    }
}

/// Collect the status of the main process. It should be gone with its domain; if it is not, it is killed itself.
fn reap_main(ctx: Ctx<'_>, pid: i32) -> Option<Exit> {
    let mut waited = 0;
    let mut killed = false;
    loop {
        match ctx.p.try_wait(pid) {
            Ok(Some(e)) => return Some(e),
            Ok(None) => {}
            Err(_) => return None,
        }
        if waited >= SETTLE_MS + REAP_MS {
            return None;
        }
        if waited >= SETTLE_MS && !killed {
            let _ = ctx.p.kill(pid);
            killed = true;
        }
        ctx.host.pause(POLL_MS);
        waited += POLL_MS;
    }
}

fn spawn_reaper(ctx: Ctx<'_>, w: &Workload) {
    if let Err(e) = ctx.p.spawn(&ctx.env.reap_spec(w)) {
        ctx.s.warn_out(format!(
            "{w}: no reaper was started, so the workload is not cleaned up if this supervisor is killed: {e}"
        ));
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The supervisor.

/// Start a workload and see it through to its end: the phases of its lifecycle, each one recorded; restarts within the
/// limits of the policy; and a clean domain at the end of every run. `Err`: nothing was started, or the supervisor
/// itself failed (and cleaned up what it had started).
pub fn supervise(ctx: Ctx<'_>, job: &Job) -> Result<Outcome> {
    if job.argv.is_empty() {
        return Err(Error::Usage("supervise needs a command".into()));
    }
    prelude(ctx.c, ctx.s)?;
    let (mut rec, guard) = begin(ctx, job)?;
    spawn_reaper(ctx, &job.workload);
    let result = match run(ctx, job, &mut rec, true) {
        Ok(o) => Ok(o),
        Err(Fault::Handled(e)) => Err(e),
        Err(Fault::Unexpected(e)) => {
            abandon(ctx, &job.workload, &mut rec, &e);
            Err(e)
        }
    };
    drop(guard);
    result
}

pub(crate) fn failed_err(w: &Workload, r: &Record) -> Error {
    Error::Lifecycle(format!(
        "{w} failed ({}): `resctl reset {w}` first",
        r.reason
    ))
}

/// Become the supervisor of the workload and create its domain and its record: nothing is left behind when that fails.
fn begin(ctx: Ctx<'_>, job: &Job) -> Result<(Record, Guard)> {
    let w = &job.workload;
    let guard = begin_claim(ctx, w)?;
    match begin_held(ctx, job) {
        Ok(rec) => Ok((rec, guard)),
        Err(e) => {
            drop(guard);
            if matches!(ctx.s.peek(w), Ok(Stored::Missing)) {
                if let Ok(lock) = ctx.s.lock() {
                    let _ = lock.forget(w);
                }
            }
            Err(e)
        }
    }
}

fn begin_claim(ctx: Ctx<'_>, w: &Workload) -> Result<Guard> {
    let lock = ctx.s.lock()?;
    match lock.peek(w)? {
        Stored::Record(r) if r.phase == Phase::Failed => return Err(failed_err(w, &r)),
        Stored::Corrupt(why) => lock.quarantine(&record_rel(w), &why)?,
        _ => {}
    }
    lock.claim(w)?
        .ok_or_else(|| Error::Lifecycle(format!("{w} is supervised already")))
}

/// What a lost supervisor left of the workload is cleaned up first. The record to start from, when there is one.
fn begin_prior(ctx: Ctx<'_>, w: &Workload) -> Result<Option<Record>> {
    let keep = ctx.c.policy().orphans == Orphans::Keep;
    match ctx.s.peek(w)? {
        Stored::Missing => Ok(None),
        Stored::Corrupt(why) => Err(Error::State(format!(
            "{w}: the lifecycle record is corrupt: {why}"
        ))),
        Stored::Record(mut prior) => {
            if prior.phase == Phase::Failed {
                return Err(failed_err(w, &prior));
            }
            if prior.phase.is_terminal() {
                return Ok(Some(prior));
            }
            let phase = sweep_held(ctx, w, &mut prior, None, keep)?;
            if phase == Phase::Failed {
                return Err(Error::Lifecycle(format!(
                    "{w} had a supervisor that was lost and is cleaned up, now {}: `resctl reset {w}`, then supervise it again",
                    phase.name()
                )));
            }
            Ok(Some(prior))
        }
    }
}

fn begin_held(ctx: Ctx<'_>, job: &Job) -> Result<Record> {
    let w = &job.workload;
    let reuse = begin_prior(ctx, w)?;
    let pol = ctx.c.policy();
    let lock = ctx.s.lock()?;

    let cap = usize::try_from(pol.supervise.workloads).unwrap_or(usize::MAX);
    let mut busy = 0usize;
    for l in lock.list()? {
        let Some(other) = &l.workload else { continue };
        if other == w {
            continue;
        }
        if let Stored::Record(r) = &l.stored {
            if !r.phase.is_terminal() && alive(ctx, other) {
                busy += 1;
            }
        }
    }
    if busy >= cap {
        return Err(Error::Refused(format!(
            "{busy} workloads are supervised already; the policy allows {cap} at a time"
        )));
    }

    let now = ctx.host.now_ms();
    let rate = pol.restart.rate;
    if let Some(ms) = lock.start_wait(now, rate) {
        return Err(Error::Refused(format!(
            "{rate} starts within the last minute: try again in {}s",
            ms.div_ceil(1000)
        )));
    }

    let secs = ctx.host.epoch_secs();
    let mut rec = match reuse {
        Some(mut r) => {
            r.ended = None;
            r.status = None;
            r.oom = false;
            r
        }
        None => Record::new(&w.to_string(), secs),
    };
    rec.advance(Phase::Starting, secs)
        .map_err(Error::Lifecycle)?;
    rec.sup = Some(my_pid());
    rec.set_cmd(&job.argv[0]);
    rec.set_reason("starting");

    match ctx.c.create(w, &job.limits) {
        Ok(rep) => {
            for u in rep.unbound {
                ctx.s.warn_out(format!("{w}: {u}"));
            }
        }
        Err(e) => return Err(e),
    }
    if let Err(e) = lock.note_start(now) {
        drop(lock);
        let _ = clear_domain(ctx, w);
        return Err(e);
    }
    let detail = format!("phase=starting gen={} cmd={}", rec.gen, rec.cmd);
    if let Err(e) = lock.commit(&mut rec, Audit::Required, Event::Supervise, &detail) {
        drop(lock);
        let _ = clear_domain(ctx, w);
        return Err(e);
    }
    Ok(rec)
}

/// The runs of the workload, one after the other, until the verdict is final. `created`: the domain of the first run
/// exists already.
fn run(ctx: Ctx<'_>, job: &Job, rec: &mut Record, created: bool) -> Flow<Outcome> {
    let w = &job.workload;
    let policy = restart_policy(ctx, job.mode);
    let mut hist = History::default();
    let mut created = created;
    loop {
        let precreated = std::mem::take(&mut created);
        let ran = match launch(ctx, job, rec, precreated)? {
            Launched::Broke(msg) => Ran::Broke(msg),
            Launched::Up(pid) => match watch(ctx, w, rec, pid) {
                Watched::Exited(e) => Ran::Exit(Some(e)),
                Watched::Asked => Ran::Exit(end_on_request(ctx, w, rec, pid)),
                Watched::Lost(msg) => {
                    let _ = ctx.p.kill(pid);
                    Ran::Lost(msg)
                }
            },
        };
        if matches!(ran, Ran::Exit(Some(_))) {
            asked(ctx, w, rec);
        }
        let ending = match &ran {
            Ran::Exit(Some(e)) => classify(*e, rec.intent),
            Ran::Exit(None) => rec.intent.map_or(Ending::Lost, Ending::Requested),
            Ran::Broke(_) => rec.intent.map_or(Ending::LaunchFailed, Ending::Requested),
            Ran::Lost(_) => Ending::Lost,
        };
        let status = match &ran {
            Ran::Exit(s) => *s,
            _ => None,
        };
        let swept = sweep(ctx, w, rec);
        rec.ended = Some(ending);
        rec.status = status;
        rec.oom = swept.oom;
        let reason = match &ran {
            Ran::Exit(_) => format!("the run ended: {}", ending.render()),
            Ran::Broke(m) | Ran::Lost(m) => m.clone(),
        };
        rec.set_reason(&reason);
        let detail = format!(
            "gen={} ended={} status={} oom={}",
            rec.gen,
            ending.render(),
            status.map_or_else(|| "-".to_string(), Exit::render),
            if swept.oom { "yes" } else { "no" }
        );
        save(ctx, w, rec, Event::Exit, &detail);

        if let Some(why) = swept.stuck {
            go(
                ctx,
                w,
                rec,
                Phase::Failed,
                Event::Phase,
                &format!("the domain was not removed: {why}"),
            )?;
            return Ok(outcome(rec));
        }

        let now = ctx.host.now_ms();
        let verdict = match decide(&policy, &hist, ending, now) {
            Verdict::Restart { .. } if rec.intent.is_some() => Verdict::Stop,
            v => v,
        };
        match verdict {
            Verdict::Stop => {
                go(
                    ctx,
                    w,
                    rec,
                    Phase::Stopped,
                    Event::Phase,
                    "stopped on request",
                )?;
                return Ok(outcome(rec));
            }
            Verdict::Finish => {
                go(
                    ctx,
                    w,
                    rec,
                    Phase::Finished,
                    Event::Phase,
                    "the workload ended cleanly",
                )?;
                return Ok(outcome(rec));
            }
            Verdict::Fail(why) => {
                // A supervisor that could not watch its workload says why, not only that the end was unseen.
                let text = match (&ran, &why) {
                    (Ran::Lost(m), Why::Lost) => m.clone(),
                    _ => why.text(),
                };
                go(ctx, w, rec, Phase::Failed, Event::Phase, &text)?;
                return Ok(outcome(rec));
            }
            Verdict::Restart { delay_ms } => {
                hist.note(now, policy.window_ms);
                rec.restarts = rec.restarts.saturating_add(1);
                let why = format!(
                    "{}; restart {} in {delay_ms}ms",
                    ending.render(),
                    rec.restarts
                );
                go(ctx, w, rec, Phase::Backoff, Event::Restart, &why)?;
                if asked(ctx, w, rec) || pause_for(ctx, w, rec, delay_ms) == Reserved::Asked {
                    halt(ctx, w, rec)?;
                    return Ok(outcome(rec));
                }
                if reserve(ctx, w, rec, policy.rate)? == Reserved::Asked || asked(ctx, w, rec) {
                    halt(ctx, w, rec)?;
                    return Ok(outcome(rec));
                }
                go(ctx, w, rec, Phase::Starting, Event::Phase, "restarting")?;
            }
        }
    }
}

/// An operator asked to end the workload while it waited for its next run.
fn halt(ctx: Ctx<'_>, w: &Workload, rec: &mut Record) -> Result<()> {
    rec.stop = Some("empty".to_string());
    go(
        ctx,
        w,
        rec,
        Phase::Stopped,
        Event::Phase,
        "stopped during back-off",
    )
}

/// Wait `ms`, looking at the state often enough to see a request.
fn pause_for(ctx: Ctx<'_>, w: &Workload, rec: &mut Record, ms: u64) -> Reserved {
    let mut left = ms;
    while left > 0 {
        let step = left.min(SLICE_MS);
        ctx.host.pause(step);
        left -= step;
        if asked(ctx, w, rec) {
            return Reserved::Asked;
        }
    }
    Reserved::Go
}

/// Take a place in the start rate of the whole machine (all supervisors together), waiting for one if need be.
fn reserve(ctx: Ctx<'_>, w: &Workload, rec: &mut Record, rate: u32) -> Result<Reserved> {
    loop {
        let wait = {
            let lock = ctx.s.lock()?;
            let now = ctx.host.now_ms();
            match lock.start_wait(now, rate) {
                None => {
                    lock.note_start(now)?;
                    return Ok(Reserved::Go);
                }
                Some(ms) => ms,
            }
        };
        ctx.host.pause(wait.clamp(1, SLICE_MS));
        if asked(ctx, w, rec) {
            return Ok(Reserved::Asked);
        }
    }
}

/// Start one run: the domain (a restart has none yet) and the main process.
fn launch(ctx: Ctx<'_>, job: &Job, rec: &mut Record, created: bool) -> Flow<Launched> {
    let w = &job.workload;
    if let Some(i) = rec.intent {
        return Ok(Launched::Broke(format!(
            "a {} was asked before the run started",
            i.name()
        )));
    }
    if !created {
        match ctx.c.create(w, &job.limits) {
            Ok(rep) => {
                for u in rep.unbound {
                    ctx.s.warn_out(format!("{w}: {u}"));
                }
            }
            Err(
                e @ (Error::Refused(_)
                | Error::Usage(_)
                | Error::Unsupported(_)
                | Error::Denied(_)
                | Error::Exists(_)),
            ) => {
                // Not a failure of the machine's moment: it will not get better, and the domain in the way is not ours.
                fail_plain(ctx, w, rec, &e.to_string());
                return Err(Fault::Handled(e));
            }
            Err(e) => return Ok(Launched::Broke(format!("cannot create the domain: {e}"))),
        }
    }
    match ctx.p.spawn(&ctx.env.enter_spec(w, &job.argv)) {
        Ok(pid) => {
            rec.main = Some(pid);
            go(
                ctx,
                w,
                rec,
                Phase::Running,
                Event::Start,
                &format!("main={pid}"),
            )?;
            Ok(Launched::Up(pid))
        }
        Err(e) => Ok(Launched::Broke(format!("cannot spawn: {e}"))),
    }
}

/// Wait for the main process to end or for an operator to ask for its end. An exit is looked at before a request: a
/// workload that ended by itself is not called requested because a request arrived a moment later.
fn watch(ctx: Ctx<'_>, w: &Workload, rec: &mut Record, pid: i32) -> Watched {
    loop {
        match ctx.p.try_wait(pid) {
            Ok(Some(e)) => return Watched::Exited(e),
            Ok(None) => {}
            Err(e) => return Watched::Lost(format!("cannot wait for process {pid}: {e}")),
        }
        if asked(ctx, w, rec) {
            return Watched::Asked;
        }
        ctx.host.pause(POLL_MS);
    }
}

/// An operator asked: stop or kill what runs in the domain, and collect the main process.
fn end_on_request(ctx: Ctx<'_>, w: &Workload, rec: &mut Record, pid: i32) -> Option<Exit> {
    let kill = rec.intent == Some(Intent::Kill);
    let (to, event) = if kill {
        (Phase::Killing, Event::Kill)
    } else {
        (Phase::Stopping, Event::Stop)
    };
    let _ = go(ctx, w, rec, to, event, "an operator asked for it");
    let mut text = None;
    if ctx.c.exists(w) {
        if kill {
            let before = killable(ctx, w);
            match ctx.c.kill(w) {
                Ok(()) if before == 0 => text = Some("empty".to_string()),
                Ok(()) => text = Some(format!("killed:{before}")),
                Err(e) => ctx.s.warn_out(format!("{w}: the kill failed: {e}")),
            }
        } else {
            match ctx.c.stop(w, ctx.c.policy().stop_grace_ms) {
                Ok(Stopped::Empty) => text = Some("empty".to_string()),
                Ok(Stopped::Graceful) => text = Some("graceful".to_string()),
                Ok(Stopped::Killed(n)) => text = Some(format!("killed:{n}")),
                Err(e) => ctx.s.warn_out(format!("{w}: the stop failed: {e}")),
            }
        }
        flush_rescues(ctx.c, ctx.s);
    } else {
        text = Some("empty".to_string());
    }
    rec.stop = text;
    reap_main(ctx, pid)
}

/// The run is over: remove its domain and with it everything that is left of it.
fn sweep(ctx: Ctx<'_>, w: &Workload, rec: &mut Record) -> Swept {
    let _ = go(ctx, w, rec, Phase::Sweeping, Event::Phase, "the run ended");
    let oom = ctx.c.oom_kills(&w.rel()).is_some_and(|n| n > 0);
    let stuck = clear_domain(ctx, w).err();
    rec.main = None;
    if let Some(i) = peek_intent(ctx, w) {
        rec.ask(i);
    }
    Swept { stuck, oom }
}

// ---------------------------------------------------------------------------------------------------------------------
// The operators.

fn commit_intent(
    lock: &Locked<'_, '_>,
    rec: &mut Record,
    intent: Intent,
    w: &Workload,
) -> Result<()> {
    lock.commit(
        rec,
        Audit::Optional,
        Event::Intent,
        &format!("{} asked", intent.name()),
    )
    .map_err(|e| {
        Error::State(format!(
            "cannot record the {} of {w}, so nothing is ended: {e}",
            intent.name()
        ))
    })
}

fn plan(ctx: Ctx<'_>, w: &Workload, intent: Intent) -> Result<Plan> {
    let lock = ctx.s.lock()?;
    let sup = lock.sup_state(w)?;
    let stored = lock.peek(w)?;
    if sup == Sup::Alive {
        match stored {
            Stored::Record(mut rec) => {
                if !rec.phase.is_terminal() && rec.ask(intent) {
                    commit_intent(&lock, &mut rec, intent, w)?;
                }
            }
            Stored::Missing => {
                return Err(Error::Lifecycle(format!(
                    "{w} is being started by its supervisor: try again"
                )))
            }
            Stored::Corrupt(why) => {
                lock.quarantine(&record_rel(w), &why)?;
                let mut rec = Record::new(&w.to_string(), ctx.host.epoch_secs());
                rec.ask(intent);
                commit_intent(&lock, &mut rec, intent, w)?;
            }
        }
        return Ok(Plan::Wait);
    }
    match stored {
        Stored::Record(rec) if !rec.phase.is_terminal() => match lock.claim(w)? {
            Some(guard) => Ok(Plan::Sweep(guard, rec)),
            None => Err(Error::Lifecycle(format!(
                "a supervisor of {w} appeared while it was being ended: try again"
            ))),
        },
        Stored::Record(rec) => Ok(Plan::Done(Terminated::Already(rec.phase))),
        Stored::Missing => Ok(Plan::Done(Terminated::Unmanaged)),
        Stored::Corrupt(why) => {
            lock.quarantine(&record_rel(w), &why)?;
            Ok(Plan::Done(Terminated::Unmanaged))
        }
    }
}

/// Wait for the supervisor of `w` to finish what it was asked. `Some`: the final phase. `None`: its record is not final
/// (the supervisor died on the way).
fn wait_for(ctx: Ctx<'_>, w: &Workload, intent: Intent) -> Result<Option<Phase>> {
    if intent == Intent::Kill && ctx.c.exists(w) {
        // The request is recorded already: the supervisor sees it, and labels the end as asked for.
        if let Err(e) = ctx.c.kill(w) {
            ctx.s.warn_out(format!("{w}: the kill failed: {e}"));
        }
        flush_rescues(ctx.c, ctx.s);
    }
    let grace = if intent == Intent::Stop {
        ctx.c.policy().stop_grace_ms
    } else {
        0
    };
    let limit = grace + FINISH_MS;
    let mut waited = 0;
    while ctx.s.sup_state(w)? == Sup::Alive {
        if waited >= limit {
            return Err(Error::Timeout(format!(
                "{w}: the {} was asked, but its supervisor did not finish within {}s",
                intent.name(),
                limit / 1000
            )));
        }
        ctx.host.pause(POLL_MS);
        waited += POLL_MS;
    }
    Ok(match ctx.s.peek(w)? {
        Stored::Record(r) if r.phase.is_terminal() => Some(r.phase),
        Stored::Missing => Some(Phase::Stopped),
        _ => None,
    })
}

fn terminate_core(ctx: Ctx<'_>, w: &Workload, intent: Intent) -> Result<Terminated> {
    for _ in 0..3 {
        match plan(ctx, w, intent)? {
            Plan::Done(t) => return Ok(t),
            Plan::Sweep(guard, mut rec) => {
                let phase = sweep_held(ctx, w, &mut rec, Some(intent), false)?;
                drop(guard);
                return Ok(Terminated::Swept(phase));
            }
            Plan::Wait => {
                if let Some(phase) = wait_for(ctx, w, intent)? {
                    return Ok(Terminated::Supervised(phase));
                }
            }
        }
    }
    Err(Error::Lifecycle(format!(
        "{w} could not be ended: its supervisor kept changing"
    )))
}

/// End a workload on request. A supervised one is ended by its supervisor, which records how; one whose supervisor is gone
/// is cleaned up from here. For a workload nobody supervises nothing is done: see `Terminated`.
pub fn terminate(ctx: Ctx<'_>, w: &Workload, intent: Intent) -> Result<Terminated> {
    prelude(ctx.c, ctx.s)?;
    terminate_core(ctx, w, intent)
}

/// Clean up after a supervisor that is gone, with its supervisor lock held by the caller. Whatever runs in the domain is
/// stopped or killed and the domain removed; the record ends `stopped` when an operator had asked for it, `failed`
/// otherwise. With `keep` and no request the processes are left running, and the record fails.
fn sweep_held(
    ctx: Ctx<'_>,
    w: &Workload,
    rec: &mut Record,
    intent: Option<Intent>,
    keep: bool,
) -> Result<Phase> {
    if rec.phase.is_terminal() {
        return Ok(rec.phase);
    }
    if let Some(i) = intent {
        rec.ask(i);
    }
    let intent = rec.intent;
    let before = killable(ctx, w);
    let have = before > 0;
    if keep && intent.is_none() && have {
        rec.ended = Some(Ending::Lost);
        rec.status = None;
        recovering(ctx, w, rec, "the supervisor is gone")?;
        go(
            ctx,
            w,
            rec,
            Phase::Failed,
            Event::Phase,
            "supervisor lost: the workload was kept running",
        )?;
        return Ok(Phase::Failed);
    }
    recovering(ctx, w, rec, "the supervisor is gone")?;
    let mut text = if have {
        format!("killed:{before}")
    } else {
        "empty".to_string()
    };
    if intent == Some(Intent::Stop) && have {
        match ctx.c.stop(w, ctx.c.policy().stop_grace_ms) {
            Ok(Stopped::Empty) => text = "empty".to_string(),
            Ok(Stopped::Graceful) => text = "graceful".to_string(),
            Ok(Stopped::Killed(n)) => text = format!("killed:{n}"),
            Err(e) => ctx.s.warn_out(format!("{w}: the stop failed: {e}")),
        }
    }
    flush_rescues(ctx.c, ctx.s);
    let oom = ctx.c.oom_kills(&w.rel()).is_some_and(|n| n > 0);
    let stuck = clear_domain(ctx, w).err();
    rec.status = None;
    rec.main = None;
    rec.oom = oom;
    rec.stop = Some(text);
    let (ended, phase, reason) = match intent {
        Some(i) => (Ending::Requested(i), Phase::Stopped, "stopped on request"),
        None => (
            Ending::Lost,
            Phase::Failed,
            "supervisor lost: the workload was cleaned up",
        ),
    };
    rec.ended = Some(ended);
    match stuck {
        None => {
            go(ctx, w, rec, phase, Event::Phase, reason)?;
            Ok(phase)
        }
        Some(why) => {
            let text = format!("the domain was not removed: {why}");
            go(ctx, w, rec, Phase::Failed, Event::Phase, &text)?;
            Err(Error::Io(format!("{w}: {text}")))
        }
    }
}

/// What the reaper of a workload does once its supervisor ended: nothing when the record is final, otherwise the cleanup of
/// `sweep_held`. The final phase when there was something to clean up.
pub fn reap(ctx: Ctx<'_>, w: &Workload) -> Result<Option<Phase>> {
    let guard = ctx.s.wait_sup(w)?;
    let mut rec = match ctx.s.peek(w)? {
        Stored::Record(r) if !r.phase.is_terminal() => r,
        _ => return Ok(None),
    };
    prelude(ctx.c, ctx.s)?;
    let intent = rec.intent;
    let keep = ctx.c.policy().orphans == Orphans::Keep;
    let phase = sweep_held(ctx, w, &mut rec, intent, keep)?;
    drop(guard);
    Ok(Some(phase))
}

/// Recovery after a failure of the machine or of the supervisors: workloads whose supervisor is gone are cleaned up,
/// unreadable files of the state are set aside, the domains nobody owns are removed, and the registry of critical
/// processes forgets those that ended. Workloads with a supervisor that runs are not touched.
pub fn recover(ctx: Ctx<'_>, orphans: Orphans) -> Result<Recovery> {
    let mut out = Recovery {
        healed: prelude(ctx.c, ctx.s)?,
        ..Recovery::default()
    };
    let mut claimed: Vec<(Workload, Record, Guard)> = Vec::new();
    {
        let lock = ctx.s.lock()?;
        for l in lock.list()? {
            let Listed {
                name,
                workload,
                stored,
            } = l;
            match (workload, stored) {
                (Some(x), Stored::Record(rec)) => {
                    if rec.phase.is_terminal() || alive(ctx, &x) {
                        continue;
                    }
                    match lock.claim(&x) {
                        Ok(Some(g)) => claimed.push((x, rec, g)),
                        Ok(None) => {}
                        Err(e) => ctx.s.warn_out(format!("{x}: not recovered: {e}")),
                    }
                }
                (Some(x), Stored::Corrupt(_)) if alive(ctx, &x) => {}
                (_, Stored::Corrupt(why)) => {
                    match lock.quarantine(&format!("{RECORDS}/{name}"), &why) {
                        Ok(()) => out.quarantined.push(name),
                        Err(e) => ctx.s.warn_out(format!("{name}: not set aside: {e}")),
                    }
                }
                _ => {}
            }
        }
    }
    let keep = orphans == Orphans::Keep;
    let mut stuck = Vec::new();
    for (x, mut rec, guard) in claimed {
        let intent = rec.intent;
        match sweep_held(ctx, &x, &mut rec, intent, keep) {
            Ok(phase) => out.swept.push((x.to_string(), phase)),
            Err(e) => stuck.push((x.to_string(), e.to_string())),
        }
        drop(guard);
    }
    let mut domains = ctx.c.recover_with(orphans, &|x| alive(ctx, x))?;
    flush_rescues(ctx.c, ctx.s);
    merge_stuck(&mut domains.stuck, stuck);
    out.domains = domains;
    match ctx.s.lock().and_then(|l| l.prune_critical()) {
        Ok(gone) => out.pruned = gone.len(),
        Err(e) => ctx.s.warn_out(format!(
            "the registry of critical processes is not pruned: {e}"
        )),
    }
    let d = &out.domains;
    let detail = format!(
        "swept={} quarantined={} removed={} killed={} kept={} live={} stuck={} pruned={}",
        out.swept.len(),
        out.quarantined.len(),
        d.removed.len(),
        d.killed.len(),
        d.kept.len(),
        d.live.len(),
        d.stuck.len(),
        out.pruned
    );
    audit_opt(ctx, Event::Recover, "all", &detail);
    Ok(out)
}

/// End every supervised workload, remove every domain and every record of a workload that is gone. The audit trail, the
/// quarantine and the registry of critical processes stay, and so does `system` while it holds processes.
pub fn teardown(ctx: Ctx<'_>) -> Result<Teardown> {
    prelude(ctx.c, ctx.s)?;
    let mut stuck: Vec<(String, String)> = Vec::new();
    for l in ctx.s.list()? {
        let (Some(x), Stored::Record(rec)) = (&l.workload, &l.stored) else {
            continue;
        };
        if rec.phase.is_terminal() {
            continue;
        }
        if let Err(e) = terminate_core(ctx, x, Intent::Kill) {
            stuck.push((x.to_string(), e.to_string()));
        }
    }
    let mut domains = ctx.c.teardown()?;
    flush_rescues(ctx.c, ctx.s);
    merge_stuck(&mut domains.stuck, stuck);

    let mut forgotten = Vec::new();
    {
        let lock = ctx.s.lock()?;
        for l in lock.list()? {
            let Listed {
                name,
                workload,
                stored,
            } = l;
            let Some(x) = workload else {
                match lock.quarantine(&format!("{RECORDS}/{name}"), "not the record of a workload")
                {
                    Ok(()) => forgotten.push(name),
                    Err(e) => ctx.s.warn_out(format!("{name}: not set aside: {e}")),
                }
                continue;
            };
            let name = x.to_string();
            if domains.stuck.iter().any(|(s, _)| *s == name) || alive(ctx, &x) {
                continue;
            }
            if let Stored::Corrupt(why) = &stored {
                if let Err(e) = lock.quarantine(&record_rel(&x), why) {
                    ctx.s.warn_out(format!("{name}: not set aside: {e}"));
                    continue;
                }
            }
            match lock.forget(&x) {
                Ok(()) => forgotten.push(name),
                Err(e) => ctx.s.warn_out(format!("{name}: the record is kept: {e}")),
            }
        }
    }
    let detail = format!(
        "forgotten={} kept={} stuck={}",
        forgotten.len(),
        domains.kept.len(),
        domains.stuck.len()
    );
    audit_opt(ctx, Event::Teardown, "all", &detail);
    Ok(Teardown { domains, forgotten })
}

/// Forget a failed workload so that it can be started again: whatever is left of it is ended and removed, and its record
/// goes. The phase it was in. A workload that is supervised by a supervisor that runs is not reset: stop it first.
pub fn reset(ctx: Ctx<'_>, w: &Workload) -> Result<Option<Phase>> {
    prelude(ctx.c, ctx.s)?;
    let (guard, stored) = {
        let lock = ctx.s.lock()?;
        let mut stored = lock.peek(w)?;
        if let Stored::Corrupt(why) = &stored {
            lock.quarantine(&record_rel(w), why)?;
            stored = Stored::Missing;
        } else if matches!(stored, Stored::Missing)
            && lock.sup_state(w)? == Sup::Absent
            && !ctx.c.exists(w)
        {
            return Err(Error::Lifecycle(format!(
                "{w} has no lifecycle record, supervisor or domain: nothing to reset"
            )));
        }
        match lock.claim(w)? {
            Some(g) => (g, stored),
            None => {
                return Err(Error::Lifecycle(format!(
                    "{w} has a supervisor that is alive: stop it first"
                )))
            }
        }
    };
    let mut phase = None;
    if let Stored::Record(mut rec) = stored {
        phase = Some(rec.phase);
        if !rec.phase.is_terminal() {
            let intent = rec.intent;
            phase = Some(sweep_held(ctx, w, &mut rec, intent, false)?);
        }
    }
    if let Err(why) = clear_domain(ctx, w) {
        return Err(Error::Io(format!("{w}: the domain was not removed: {why}")));
    }
    drop(guard);
    let lock = ctx.s.lock()?;
    let _ = lock.audit(
        Audit::Optional,
        Event::Reset,
        &w.to_string(),
        &format!("was {}", phase.map_or("unrecorded", Phase::name)),
    );
    lock.forget(w)?;
    Ok(phase)
}

fn render_state(ctx: Ctx<'_>, x: &Workload, r: &Record) -> String {
    let sup = if r.phase.is_terminal() {
        "none"
    } else {
        match ctx.s.sup_state(x) {
            Ok(Sup::Alive) => "alive",
            Ok(_) => "lost",
            Err(_) => "unknown",
        }
    };
    let dom = if ctx.c.exists(x) { "present" } else { "absent" };
    format!(
        "{x} phase={} gen={} restarts={} supervisor={sup} domain={dom} main={} sup={} intent={} ended={} status={} oom={} stop={} since={} cmd={} reason={}",
        r.phase.name(),
        r.gen,
        r.restarts,
        show(r.main),
        show(r.sup),
        show(r.intent.map(Intent::name)),
        show(r.ended.map(Ending::render)),
        show(r.status.map(Exit::render)),
        if r.oom { "yes" } else { "no" },
        show(r.stop.as_deref()),
        r.since,
        field(&r.cmd),
        field(&r.reason),
    )
}

/// One line for each workload that has a lifecycle record (or only for `only`), and for each file of the state that does
/// not read as one.
pub fn state_lines(ctx: Ctx<'_>, only: Option<&Workload>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for l in ctx.s.list()? {
        if let Some(o) = only {
            if l.workload.as_ref() != Some(o) {
                continue;
            }
        }
        match (&l.workload, &l.stored) {
            (Some(x), Stored::Record(r)) => out.push(render_state(ctx, x, r)),
            (_, Stored::Corrupt(why)) => out.push(format!("{} corrupt={}", l.name, field(why))),
            _ => {}
        }
    }
    if let Some(o) = only {
        if out.is_empty() {
            return Err(Error::Lifecycle(format!("{o} has no lifecycle record")));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------------------------------
// The main process.

/// Everything the main process of a workload does before it becomes the workload: it checks who it will run as, moves
/// itself into the domain and checks that it is there. Nothing of the workload runs before this succeeds.
pub fn prepare_main(
    host: &dyn Host,
    c: &Controller<'_>,
    w: &Workload,
    argv: &[String],
) -> Result<Launch> {
    if argv.is_empty() {
        return Err(Error::Usage("no command to run".into()));
    }
    let run = &c.policy().run;
    let (uid, gid) =
        if host.euid() == 0 {
            match (run.uid, run.gid) {
                (None, _) if !run.allow_root => return Err(Error::Refused(
                    "a workload does not run as root: set [run] uid and gid, or allow.root = yes"
                        .into(),
                )),
                (Some(_), None) => return Err(Error::Refused("[run] uid needs a gid".into())),
                (u, g) => (u, g),
            }
        } else {
            (None, None)
        };
    let me = my_pid();
    c.attach(&Target::Workload(w.clone()), me)?;
    let want = w.to_string();
    match c.placement(me) {
        Ok(p) if p.workload().as_deref() == Some(want.as_str()) => {}
        Ok(_) => {
            return Err(Error::Refused(format!(
                "process {me} is not inside {want} after it moved"
            )))
        }
        Err(e) => {
            return Err(Error::Refused(format!(
                "cannot tell where process {me} runs: {e}"
            )))
        }
    }
    Ok(Launch {
        argv: argv.to_vec(),
        uid,
        gid,
    })
}

/// Become the workload. Only returns when that failed.
pub fn exec_main(l: &Launch) -> io::Error {
    let Some((prog, rest)) = l.argv.split_first() else {
        return io::Error::new(io::ErrorKind::InvalidInput, "no command to run");
    };
    let mut cmd = Command::new(prog);
    cmd.args(rest);
    if let Some(g) = l.gid {
        cmd.gid(g);
    }
    if let Some(u) = l.uid {
        cmd.uid(u);
    }
    cmd.exec()
}

// ---------------------------------------------------------------------------------------------------------------------
// Tests: everything runs against FakeHost and a scripted process table; nothing here starts a real process except the
// `RealProcs` tests at the end.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::fake::FakeHost;
    use crate::layout::{Layout, Lifecycle};
    use crate::policy::Policy;
    use crate::store::Line;
    use std::cell::Cell;
    use std::collections::VecDeque;

    const GIB: u64 = 1 << 30;
    const DIR: &str = "/run/ai";
    const BASE: &str = "[system]\ncpu.weight = 1000\nmemory.reserve = 64M\n[workload]\nmemory.max = 512M\npids.max = 256\n[class.batch]\ncpu.weight = 100\nmemory.max = 160M\npids.max = 96\n[limits.batch]\nmemory.max = 64M\npids.max = 16\n[stop]\ngrace = 1500ms\n";

    fn world() -> FakeHost {
        let h = FakeHost::new();
        h.add_v2("/c", &["cpuset", "cpu", "io", "memory", "pids"]);
        h.set_clock(5_000);
        h.proc_in(my_pid(), &[]);
        h
    }

    fn text(restart: &str, supervise: &str, run: &str, rest: &str) -> String {
        format!("{BASE}[restart]\n{restart}\n[supervise]\n{supervise}\n[run]\n{run}\n{rest}")
    }

    fn restart(
        mode: &str,
        max: u32,
        total: u32,
        window: &str,
        backoff: &str,
        cap: &str,
        rate: u32,
    ) -> String {
        format!(
            "mode = {mode}\nmax = {max}\nwindow = {window}\ntotal = {total}\nbackoff = {backoff}\nbackoff.max = {cap}\nrate = {rate}\n"
        )
    }

    fn standard() -> String {
        text(
            &restart("on-failure", 3, 10, "60s", "10ms", "40ms", 30),
            "workloads = 2",
            "allow.root = yes",
            "",
        )
    }

    fn with_restart(r: &str) -> String {
        text(r, "workloads = 2", "allow.root = yes", "")
    }

    /// One start the scripted process table was asked for.
    #[derive(Clone)]
    struct Spawn {
        spec: RunSpec,
        at: u64,
        pid: Option<i32>,
    }

    struct Kid {
        script: Script,
        polls: u32,
    }

    /// What a main process does: it ends after `after` looks, or lives until it is killed.
    #[derive(Clone, Default)]
    struct Script {
        spawn_err: bool,
        exit: Option<Exit>,
        after: u32,
        kids: Vec<i32>,
        wait_err: bool,
    }

    impl Script {
        fn ends(exit: Exit, after: u32) -> Script {
            Script {
                exit: Some(exit),
                after,
                ..Script::default()
            }
        }
        fn lives() -> Script {
            Script::default()
        }
        fn cannot_spawn() -> Script {
            Script {
                spawn_err: true,
                ..Script::default()
            }
        }
        fn wait_err() -> Script {
            Script {
                wait_err: true,
                ..Script::default()
            }
        }
        fn with_kids(mut self, kids: &[i32]) -> Script {
            self.kids = kids.to_vec();
            self
        }
    }

    /// The processes of a supervisor, as a script: the main process of each run follows the next script (the last one
    /// repeats), lives in the domain it was asked to enter, and is a child of the fake host.
    type Hook<'a> = Box<dyn Fn(u32) + 'a>;

    struct FakeProcs<'a> {
        host: &'a FakeHost,
        scripts: RefCell<VecDeque<Script>>,
        kids: RefCell<HashMap<i32, Kid>>,
        next: Cell<i32>,
        spawned: RefCell<Vec<Spawn>>,
        killed: RefCell<Vec<i32>>,
        polls: Cell<u32>,
        hook: RefCell<Option<Hook<'a>>>,
        reaper_fails: Cell<bool>,
    }

    impl<'a> FakeProcs<'a> {
        fn new(host: &'a FakeHost) -> FakeProcs<'a> {
            FakeProcs {
                host,
                scripts: RefCell::new(VecDeque::new()),
                kids: RefCell::new(HashMap::new()),
                next: Cell::new(100_000),
                spawned: RefCell::new(Vec::new()),
                killed: RefCell::new(Vec::new()),
                polls: Cell::new(0),
                hook: RefCell::new(None),
                reaper_fails: Cell::new(false),
            }
        }
        fn scripts(&self, list: &[Script]) {
            *self.scripts.borrow_mut() = list.iter().cloned().collect();
        }
        fn on_poll(&self, f: impl Fn(u32) + 'a) {
            *self.hook.borrow_mut() = Some(Box::new(f));
        }
        fn pid(&self) -> i32 {
            let mut n = self.next.get();
            if n == my_pid() {
                n += 1;
            }
            self.next.set(n + 1);
            n
        }
        fn take(&self) -> Script {
            let mut q = self.scripts.borrow_mut();
            if q.len() > 1 {
                q.pop_front().unwrap_or_default()
            } else {
                q.front().cloned().unwrap_or_default()
            }
        }
        /// The attempts to start a main process.
        fn mains(&self) -> Vec<Spawn> {
            self.spawned
                .borrow()
                .iter()
                .filter(|s| !s.spec.detach)
                .cloned()
                .collect()
        }
        fn reapers(&self) -> usize {
            self.spawned
                .borrow()
                .iter()
                .filter(|s| s.spec.detach)
                .count()
        }
        /// The process ids of the main processes that were started.
        fn main_pids(&self) -> Vec<i32> {
            self.mains().iter().filter_map(|s| s.pid).collect()
        }
    }

    impl Procs for FakeProcs<'_> {
        fn spawn(&self, spec: &RunSpec) -> io::Result<i32> {
            let mut rec = Spawn {
                spec: spec.clone(),
                at: self.host.now_ms(),
                pid: None,
            };
            if spec.detach {
                if self.reaper_fails.get() {
                    self.spawned.borrow_mut().push(rec);
                    return Err(io::Error::other("no reaper for you"));
                }
                let pid = self.pid();
                rec.pid = Some(pid);
                self.spawned.borrow_mut().push(rec);
                return Ok(pid);
            }
            let script = self.take();
            if script.spawn_err {
                self.spawned.borrow_mut().push(rec);
                return Err(io::Error::other("cannot exec"));
            }
            let name = spec
                .args
                .iter()
                .position(|a| a == "__enter")
                .and_then(|i| spec.args.get(i + 1))
                .cloned()
                .unwrap_or_default();
            let w = Workload::parse(&name).map_err(|e| io::Error::other(e.to_string()))?;
            let dir = format!("/c/{}", w.rel());
            if !self.host.dir_exists(&dir) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no domain to enter",
                ));
            }
            let pid = self.pid();
            self.host.proc_in(pid, &[dir.as_str()]);
            for k in &script.kids {
                self.host.proc_in(*k, &[dir.as_str()]);
            }
            rec.pid = Some(pid);
            self.spawned.borrow_mut().push(rec);
            self.kids.borrow_mut().insert(pid, Kid { script, polls: 0 });
            Ok(pid)
        }

        fn try_wait(&self, pid: i32) -> io::Result<Option<Exit>> {
            let n = self.polls.get() + 1;
            self.polls.set(n);
            if let Some(hook) = self.hook.borrow().as_ref() {
                hook(n);
            }
            let mut kids = self.kids.borrow_mut();
            let Some(kid) = kids.get_mut(&pid) else {
                return Err(io::Error::new(io::ErrorKind::NotFound, "not a child"));
            };
            if kid.script.wait_err {
                return Err(io::Error::other("wait failed"));
            }
            kid.polls += 1;
            if let Some(exit) = kid.script.exit {
                if kid.polls > kid.script.after {
                    self.host.exit_proc(pid);
                    kids.remove(&pid);
                    return Ok(Some(exit));
                }
            }
            if !self.host.alive(pid) {
                kids.remove(&pid);
                let sig = self
                    .host
                    .signals()
                    .iter()
                    .rev()
                    .find(|&&(p, _)| p == pid)
                    .map_or(9, |&(_, s)| s);
                return Ok(Some(Exit::Signal(sig)));
            }
            Ok(None)
        }

        fn kill(&self, pid: i32) -> io::Result<()> {
            self.killed.borrow_mut().push(pid);
            self.host.exit_proc(pid);
            Ok(())
        }
    }

    /// Build the world of one test. The identifiers are passed in because a macro cannot make names the test can see.
    /// `op` is the context of an operator, `ctx` that of the supervisor; they have processes of their own, so that a
    /// hook of the supervisor's can act as the operator without borrowing the supervisor.
    macro_rules! rig {
        ($h:ident, $c:ident, $s:ident, $p:ident, $op:ident, $ctx:ident, $text:expr) => {
            let $h = world();
            let layout = Layout::detect(&$h, Lifecycle::Auto);
            let $s = Store::open(&$h, DIR, "4242:test").unwrap();
            let shield = || $s.critical_pids();
            let pol = Policy::parse(&$text).unwrap();
            let $c = Controller::new(&$h, &layout, &pol, 4 * GIB)
                .unwrap()
                .with_critical(&shield);
            $c.init().unwrap();
            let env = Env {
                exe: "resctl".into(),
                globals: vec![],
            };
            let ops = FakeProcs::new(&$h);
            let $op = Ctx {
                host: &$h,
                c: &$c,
                s: &$s,
                p: &ops,
                env: &env,
            };
            let $p = FakeProcs::new(&$h);
            let $ctx = Ctx {
                host: &$h,
                c: &$c,
                s: &$s,
                p: &$p,
                env: &env,
            };
            let _ = (&$h, &$c, &$s, &$p, &$op, &$ctx);
        };
    }

    fn w(s: &str) -> Workload {
        Workload::parse(s).unwrap()
    }

    fn job(id: &str) -> Job {
        Job {
            workload: w(&format!("batch/{id}")),
            limits: Limits::default(),
            argv: vec!["worker".into()],
            mode: None,
        }
    }

    fn dom(id: &str) -> String {
        format!("/c/agent-interaction/workload/batch/{id}")
    }

    const SYSDOM: &str = "/c/agent-interaction/system";

    fn trail(s: &Store<'_>) -> Vec<Line> {
        s.audit_tail(5000)
            .unwrap()
            .iter()
            .map(|l| Line::parse(l).unwrap_or_else(|| panic!("not a line of the trail: {l}")))
            .collect()
    }

    fn events(s: &Store<'_>) -> Vec<Event> {
        trail(s).iter().map(|l| l.event).collect()
    }

    fn count(s: &Store<'_>, e: Event) -> usize {
        trail(s).iter().filter(|l| l.event == e).count()
    }

    /// The details of the lines of one event.
    fn details(s: &Store<'_>, e: Event) -> Vec<String> {
        trail(s)
            .iter()
            .filter(|l| l.event == e)
            .map(|l| l.detail.clone())
            .collect()
    }

    fn record(s: &Store<'_>, id: &str) -> Record {
        match s.peek(&w(&format!("batch/{id}"))).unwrap() {
            Stored::Record(r) => r,
            _ => panic!("no record"),
        }
    }

    fn has_record(s: &Store<'_>, id: &str) -> bool {
        matches!(s.peek(&w(&format!("batch/{id}"))), Ok(Stored::Record(_)))
    }

    fn signalled(h: &FakeHost, pid: i32) -> bool {
        h.signals().iter().any(|&(p, _)| p == pid)
    }

    /// A workload whose supervisor is not there: its domain and its processes, a record that is not final. With `hold`
    /// the supervisor lock stays held (a supervisor that is alive), without it the lock is free (a supervisor that is lost).
    fn orphan(
        h: &FakeHost,
        c: &Controller<'_>,
        s: &Store<'_>,
        id: &str,
        pids: &[i32],
        hold: bool,
    ) -> Option<Guard> {
        let wl = w(&format!("batch/{id}"));
        c.create(&wl, &Limits::default()).unwrap();
        let d = dom(id);
        for &pid in pids {
            h.proc_in(pid, &[d.as_str()]);
        }
        let lock = s.lock().unwrap();
        let guard = lock.claim(&wl).unwrap().unwrap();
        let mut rec = Record::new(&wl.to_string(), 1_700_000_000);
        rec.advance(Phase::Starting, 1_700_000_000).unwrap();
        rec.sup = Some(4000);
        rec.set_cmd("worker");
        rec.advance(Phase::Running, 1_700_000_000).unwrap();
        rec.main = pids.first().copied();
        lock.commit(
            &mut rec,
            Audit::Required,
            Event::Supervise,
            "phase=running gen=1 cmd=worker",
        )
        .unwrap();
        drop(lock);
        if hold {
            Some(guard)
        } else {
            None
        }
    }

    /// A process that must survive: in `system`, and registered.
    fn critical(h: &FakeHost, s: &Store<'_>, pid: i32) {
        h.proc_in(pid, &[SYSDOM]);
        s.lock().unwrap().protect(pid, "svc").unwrap();
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Normal operation.

    #[test]
    fn a_workload_that_exits_cleanly_finishes_and_leaves_nothing() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[Script::ends(Exit::Code(0), 2)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Finished, 1, 0));
        assert_eq!(out.code(), 0);
        let r = record(&s, "a");
        assert_eq!(r.phase, Phase::Finished);
        assert_eq!(r.ended, Some(Ending::Clean));
        assert_eq!(r.status, Some(Exit::Code(0)));
        assert_eq!((r.main, r.sup, r.intent), (None, None, None));
        assert!(!r.oom);
        assert!(!h.dir_exists(&dom("a")));
        assert!(h.dir_exists("/c/agent-interaction/workload"));
        assert_eq!(s.sup_state(&w("batch/a")).unwrap(), Sup::Lost);
        assert!(s.take_warnings().is_empty());
        assert_eq!(p.reapers(), 1);
        let mains = p.mains();
        assert_eq!(mains.len(), 1);
        assert_eq!(mains[0].spec.args, ["__enter", "batch/a", "--", "worker"]);
        assert!(!h.alive(mains[0].pid.unwrap()));
        assert_eq!(
            events(&s),
            vec![
                Event::Supervise,
                Event::Start,
                Event::Phase,
                Event::Exit,
                Event::Phase
            ]
        );
        let exit = details(&s, Event::Exit);
        assert!(
            exit[0].contains("ended=clean status=exit:0 oom=no"),
            "{exit:?}"
        );
        // The same workload can be supervised again: a new generation of the same record.
        let again = supervise(ctx, &job("a")).unwrap();
        assert_eq!((again.phase, again.gen), (Phase::Finished, 2));
        let _ = op;
    }

    #[test]
    fn a_crash_is_labelled_unexpected_and_restarted_within_the_policy() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[
            Script::ends(Exit::Code(3), 1),
            Script::ends(Exit::Code(0), 1),
        ]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Finished, 2, 1));
        let exits = details(&s, Event::Exit);
        assert_eq!(exits.len(), 2);
        assert!(
            exits[0].contains("ended=failed:3 status=exit:3"),
            "{exits:?}"
        );
        assert!(exits[1].contains("ended=clean"), "{exits:?}");
        let restarts = details(&s, Event::Restart);
        assert_eq!(restarts.len(), 1);
        assert!(
            restarts[0].contains("failed:3; restart 1 in 10ms"),
            "{restarts:?}"
        );
        let mains = p.mains();
        assert_eq!(mains.len(), 2);
        assert!(mains[1].at >= mains[0].at + 10);
        assert!(!h.dir_exists(&dom("a")));
        assert_eq!(h.procs_of(&dom("a")), Vec::<i32>::new());
    }

    #[test]
    fn a_signal_is_a_crash_and_mode_never_does_not_restart_it() {
        rig!(
            h,
            c,
            s,
            p,
            op,
            ctx,
            with_restart(&restart("never", 3, 10, "60s", "10ms", "40ms", 30))
        );
        p.scripts(&[Script::ends(Exit::Signal(11), 0)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Failed, 1, 0));
        assert_eq!(out.code(), 1);
        assert!(
            out.reason.contains("the restart mode is never"),
            "{}",
            out.reason
        );
        let r = record(&s, "a");
        assert_eq!(r.ended, Some(Ending::Crashed(11)));
        assert_eq!(r.status, Some(Exit::Signal(11)));
        assert_eq!(p.mains().len(), 1);
        assert!(!h.dir_exists(&dom("a")));

        // A failed workload is not started again until somebody looked at it.
        let again = supervise(ctx, &job("a")).unwrap_err();
        let msg = again.to_string();
        assert!(msg.contains("failed") && msg.contains("reset"), "{msg}");
        assert_eq!(p.mains().len(), 1);

        assert_eq!(reset(op, &w("batch/a")).unwrap(), Some(Phase::Failed));
        assert!(!has_record(&s, "a"));
        assert_eq!(count(&s, Event::Reset), 1);
        p.scripts(&[Script::ends(Exit::Code(0), 0)]);
        let ok = supervise(ctx, &job("a")).unwrap();
        assert_eq!((ok.phase, ok.gen), (Phase::Finished, 1));
    }

    #[test]
    fn a_crash_loop_stops_after_the_restarts_the_policy_allows() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[Script::ends(Exit::Code(1), 0)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Failed, 4, 3));
        assert!(
            out.reason.contains("3 restarts within 60s are used up"),
            "{}",
            out.reason
        );
        let mains = p.mains();
        assert_eq!(mains.len(), 4);
        for (i, want) in [10u64, 20, 40].iter().enumerate() {
            assert!(
                mains[i + 1].at >= mains[i].at + want,
                "restart {} came after {}ms",
                i + 1,
                mains[i + 1].at - mains[i].at
            );
        }
        assert_eq!(count(&s, Event::Restart), 3);
        assert_eq!(count(&s, Event::Exit), 4);
        assert!(!h.dir_exists(&dom("a")));
        assert!(p.main_pids().iter().all(|&pid| !h.alive(pid)));
        let r = record(&s, "a");
        assert_eq!((r.phase, r.ended), (Phase::Failed, Some(Ending::Failed(1))));
        // Nothing runs again by itself.
        assert_eq!(p.mains().len(), 4);
        let _ = op;
    }

    #[test]
    fn the_restarts_of_a_run_are_capped_even_when_they_are_spread_out() {
        rig!(
            h,
            c,
            s,
            p,
            op,
            ctx,
            with_restart(&restart("on-failure", 2, 2, "1s", "600ms", "600ms", 30))
        );
        p.scripts(&[Script::ends(Exit::Code(1), 0)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.restarts), (Phase::Failed, 2));
        assert!(
            out.reason
                .contains("2 restarts since the supervisor started are used up"),
            "{}",
            out.reason
        );
        assert_eq!(p.mains().len(), 3);
        assert!(!h.dir_exists(&dom("a")));
        let _ = op;
    }

    #[test]
    fn restarts_wait_for_the_start_rate_of_the_machine() {
        rig!(
            h,
            c,
            s,
            p,
            op,
            ctx,
            with_restart(&restart("on-failure", 3, 10, "600s", "10ms", "40ms", 2))
        );
        p.scripts(&[Script::ends(Exit::Code(1), 0)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.restarts), (Phase::Failed, 3));
        let mains = p.mains();
        assert_eq!(mains.len(), 4);
        // Two starts per minute: the third start waits for the first one to be a minute old.
        assert!(
            mains[2].at >= mains[0].at + 59_000,
            "{} {}",
            mains[2].at,
            mains[0].at
        );
        assert!(
            mains[3].at >= mains[1].at + 59_000,
            "{} {}",
            mains[3].at,
            mains[1].at
        );
        assert!(!h.dir_exists(&dom("a")));
        let _ = op;
    }

    #[test]
    fn mode_always_restarts_a_clean_exit_up_to_the_bound() {
        rig!(
            h,
            c,
            s,
            p,
            op,
            ctx,
            with_restart(&restart("always", 2, 10, "60s", "10ms", "40ms", 30))
        );
        p.scripts(&[Script::ends(Exit::Code(0), 0)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.restarts), (Phase::Failed, 2));
        assert_eq!(p.mains().len(), 3);
        assert!(!h.dir_exists(&dom("a")));
        // A mode of the job replaces the mode of the policy, not its limits.
        reset(op, &w("batch/a")).unwrap();
        let mut j = job("a");
        j.mode = Some(RestartMode::Never);
        p.scripts(&[Script::ends(Exit::Code(2), 0)]);
        let out = supervise(ctx, &j).unwrap();
        assert_eq!((out.phase, out.restarts), (Phase::Failed, 0));
        assert!(out.reason.contains("never"), "{}", out.reason);
        let _ = s;
    }

    #[test]
    fn a_workload_that_cannot_be_spawned_is_retried_within_the_bound() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[Script::cannot_spawn()]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.restarts), (Phase::Failed, 3));
        assert_eq!(p.mains().len(), 4);
        assert!(out.reason.contains("3 restarts within"), "{}", out.reason);
        let r = record(&s, "a");
        assert_eq!(r.ended, Some(Ending::LaunchFailed));
        assert!(!h.dir_exists(&dom("a")));
        let _ = op;
    }

    #[test]
    fn a_reaper_that_cannot_be_started_is_a_warning_not_a_failure() {
        rig!(h, c, s, p, op, ctx, standard());
        p.reaper_fails.set(true);
        p.scripts(&[Script::ends(Exit::Code(0), 0)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!(out.phase, Phase::Finished);
        let warnings = s.take_warnings();
        assert!(
            warnings.iter().any(|m| m.contains("no reaper was started")),
            "{warnings:?}"
        );
        assert!(!h.dir_exists(&dom("a")));
        let _ = op;
    }

    #[test]
    fn a_supervisor_that_cannot_wait_for_its_workload_cleans_up_and_fails() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[Script::wait_err()]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!(out.phase, Phase::Failed);
        assert!(
            out.reason.contains("cannot wait for process"),
            "{}",
            out.reason
        );
        let r = record(&s, "a");
        assert_eq!(r.ended, Some(Ending::Lost));
        assert_eq!(p.killed.borrow().len(), 1);
        assert!(!h.alive(p.killed.borrow()[0]));
        assert!(!h.dir_exists(&dom("a")));
        let _ = op;
    }

    #[test]
    fn the_oom_killer_is_recorded_apart_from_a_crash() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[Script::ends(Exit::Signal(9), 1)]);
        let hr = &h;
        p.on_poll(move |n| {
            if n == 1 {
                hr.set(
                    &format!("{}/memory.events", dom("a")),
                    "low 0\nhigh 0\nmax 0\noom 1\noom_kill 1\n",
                );
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        // The same script repeats, so this is a crash loop that ends in `failed`; only the
        // first run saw the kernel's OOM killer, later runs get a fresh domain.
        assert_eq!(out.phase, Phase::Failed);
        assert_eq!(out.restarts, 3);
        let exits = details(&s, Event::Exit);
        assert_eq!(exits.len(), 4, "{exits:?}");
        assert!(
            exits[0].contains("ended=crashed:9 status=signal:9 oom=yes"),
            "{exits:?}"
        );
        for later in &exits[1..] {
            assert!(later.contains("oom=no"), "{exits:?}");
        }
        assert!(!record(&s, "a").oom);
        let _ = op;
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Forced termination of a supervised workload. The poll hooks play the operator: they run while the supervisor
    // looks at its workload, exactly where a real `stop` or `kill` arrives.

    #[test]
    fn a_kill_ends_the_workload_as_requested_and_it_is_not_restarted() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[Script::lives()]);
        p.on_poll(move |n| {
            if n == 3 {
                assert!(matches!(
                    plan(op, &w("batch/a"), Intent::Kill),
                    Ok(Plan::Wait)
                ));
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Stopped, 1, 0));
        assert_eq!(out.code(), 0);
        assert_eq!(out.reason, "stopped on request");
        let r = record(&s, "a");
        assert_eq!(r.ended, Some(Ending::Requested(Intent::Kill)));
        assert_eq!(r.status, Some(Exit::Signal(9)));
        assert_eq!(r.stop.as_deref(), Some("killed:1"));
        assert_eq!((r.main, r.sup, r.intent), (None, None, None));
        let mains = p.mains();
        assert_eq!(mains.len(), 1, "a requested end is never restarted");
        let pid = mains[0].pid.unwrap();
        assert!(!h.alive(pid));
        assert!(!signalled(&h, pid), "the domain is killed as a whole");
        assert!(!h.dir_exists(&dom("a")));
        assert_eq!(
            events(&s),
            vec![
                Event::Supervise,
                Event::Start,
                Event::Intent,
                Event::Kill,
                Event::Phase,
                Event::Exit,
                Event::Phase
            ]
        );
        let exit = details(&s, Event::Exit);
        assert!(exit[0].contains("ended=requested:kill"), "{exit:?}");
        assert!(s.take_warnings().is_empty());
    }

    #[test]
    fn a_kill_from_outside_resctl_is_a_crash_and_is_restarted_within_the_policy() {
        rig!(h, c, s, p, op, ctx, standard());
        let hr = &h;
        p.scripts(&[Script::lives(), Script::ends(Exit::Code(0), 1)]);
        p.on_poll(move |n| {
            if n == 2 {
                let pid = hr.procs_of(&dom("a"))[0];
                hr.signal(&[pid], 9).unwrap();
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Finished, 2, 1));
        let exits = details(&s, Event::Exit);
        assert!(
            exits[0].contains("gen=1 ended=crashed:9 status=signal:9 oom=no"),
            "{exits:?}"
        );
        // Nobody asked for it through resctl, so nothing says it was intended.
        assert_eq!(count(&s, Event::Intent), 0);
        assert_eq!(count(&s, Event::Kill), 0);
        assert_eq!(count(&s, Event::Restart), 1);
        assert_eq!(p.mains().len(), 2);
        assert!(!h.dir_exists(&dom("a")));
        let _ = op;
    }

    #[test]
    fn a_stop_ends_the_workload_gracefully_and_as_requested() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[Script::lives()]);
        p.on_poll(move |n| {
            if n == 3 {
                assert!(matches!(
                    plan(op, &w("batch/a"), Intent::Stop),
                    Ok(Plan::Wait)
                ));
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Stopped, 1, 0));
        assert_eq!(out.code(), 0);
        let r = record(&s, "a");
        assert_eq!(r.ended, Some(Ending::Requested(Intent::Stop)));
        assert_eq!(r.status, Some(Exit::Signal(15)));
        assert_eq!(r.stop.as_deref(), Some("graceful"));
        let pid = p.main_pids()[0];
        assert!(!h.alive(pid));
        let sent: Vec<i32> = h
            .signals()
            .iter()
            .filter(|&&(q, _)| q == pid)
            .map(|&(_, sig)| sig)
            .collect();
        assert_eq!(sent, vec![15], "a graceful stop asks, it does not kill");
        assert!(!h.dir_exists(&dom("a")));
        assert_eq!(
            events(&s),
            vec![
                Event::Supervise,
                Event::Start,
                Event::Intent,
                Event::Stop,
                Event::Phase,
                Event::Exit,
                Event::Phase
            ]
        );
    }

    #[test]
    fn a_workload_that_ignores_the_stop_is_killed_after_the_grace() {
        rig!(h, c, s, p, op, ctx, standard());
        let hr = &h;
        p.scripts(&[Script::lives()]);
        p.on_poll(move |n| {
            if n == 3 {
                let pid = hr.procs_of(&dom("a"))[0];
                hr.ignore_term(pid);
                assert!(matches!(
                    plan(op, &w("batch/a"), Intent::Stop),
                    Ok(Plan::Wait)
                ));
            }
        });
        let before = h.clock();
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Stopped, 1, 0));
        let r = record(&s, "a");
        assert_eq!(r.ended, Some(Ending::Requested(Intent::Stop)));
        assert_eq!(r.stop.as_deref(), Some("killed:1"));
        let pid = p.main_pids()[0];
        assert!(!h.alive(pid));
        let sent: Vec<i32> = h
            .signals()
            .iter()
            .filter(|&&(q, _)| q == pid)
            .map(|&(_, sig)| sig)
            .collect();
        assert!(
            !sent.is_empty() && sent.iter().all(|&sig| sig == 15),
            "{sent:?}"
        );
        assert!(
            h.clock() >= before + 1_500,
            "the grace of the policy was waited out"
        );
        assert!(!h.dir_exists(&dom("a")));
    }

    #[test]
    fn a_request_that_arrives_with_the_exit_labels_the_exit_as_requested() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[Script::ends(Exit::Code(1), 1)]);
        p.on_poll(move |n| {
            if n == 2 {
                assert!(matches!(
                    plan(op, &w("batch/a"), Intent::Stop),
                    Ok(Plan::Wait)
                ));
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        // The process failed at the very moment it was asked to stop: that is not a crash to restart.
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Stopped, 1, 0));
        let r = record(&s, "a");
        assert_eq!(r.ended, Some(Ending::Requested(Intent::Stop)));
        assert_eq!(r.status, Some(Exit::Code(1)));
        assert_eq!(r.stop, None);
        assert_eq!(p.mains().len(), 1);
        assert_eq!(
            events(&s),
            vec![
                Event::Supervise,
                Event::Start,
                Event::Intent,
                Event::Phase,
                Event::Exit,
                Event::Phase
            ]
        );
    }

    // -----------------------------------------------------------------------------------------------------------------
    // A supervisor that is gone: the reaper, the operator and `recover` clean up after it.

    #[test]
    fn a_lost_supervisor_is_cleaned_up_by_its_reaper() {
        rig!(h, c, s, p, op, ctx, standard());
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        assert_eq!(s.sup_state(&w("batch/a")).unwrap(), Sup::Lost);
        assert_eq!(reap(op, &w("batch/a")).unwrap(), Some(Phase::Failed));
        let r = record(&s, "a");
        assert_eq!((r.phase, r.ended), (Phase::Failed, Some(Ending::Lost)));
        assert_eq!(r.stop.as_deref(), Some("killed:2"));
        assert_eq!(r.reason, "supervisor lost: the workload was cleaned up");
        assert_eq!((r.main, r.sup, r.intent), (None, None, None));
        assert!(!h.alive(300) && !h.alive(301));
        assert!(!h.dir_exists(&dom("a")));
        // The end is not hidden: the workload failed, and has to be reset before it is supervised again.
        let msg = supervise(ctx, &job("a")).unwrap_err().to_string();
        assert!(msg.contains("reset"), "{msg}");
        assert_eq!(p.mains().len(), 0);
        // Nothing is left to clean up the second time.
        assert_eq!(reap(op, &w("batch/a")).unwrap(), None);
    }

    #[test]
    fn a_lost_supervisor_after_a_stop_request_ends_as_requested() {
        rig!(h, c, s, p, op, ctx, standard());
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        let mut r = record(&s, "a");
        r.ask(Intent::Stop);
        s.lock()
            .unwrap()
            .commit(&mut r, Audit::Optional, Event::Intent, "stop asked")
            .unwrap();
        assert_eq!(reap(op, &w("batch/a")).unwrap(), Some(Phase::Stopped));
        let r = record(&s, "a");
        assert_eq!(r.ended, Some(Ending::Requested(Intent::Stop)));
        assert_eq!(r.stop.as_deref(), Some("graceful"));
        assert_eq!(r.reason, "stopped on request");
        assert!(!h.alive(300) && !h.alive(301));
        assert!(!h.dir_exists(&dom("a")));
        let _ = ctx;
    }

    #[test]
    fn an_operator_ends_a_workload_whose_supervisor_is_lost() {
        rig!(h, c, s, p, op, ctx, standard());
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        let _ = orphan(&h, &c, &s, "b", &[310, 311], false);
        let t = terminate(op, &w("batch/a"), Intent::Kill).unwrap();
        assert!(matches!(t, Terminated::Swept(Phase::Stopped)));
        let ra = record(&s, "a");
        assert_eq!(ra.ended, Some(Ending::Requested(Intent::Kill)));
        assert_eq!(ra.stop.as_deref(), Some("killed:2"));
        assert!(!h.alive(300) && !h.alive(301));
        assert!(!h.dir_exists(&dom("a")));

        let t = terminate(op, &w("batch/b"), Intent::Stop).unwrap();
        assert!(matches!(t, Terminated::Swept(Phase::Stopped)));
        let rb = record(&s, "b");
        assert_eq!(rb.ended, Some(Ending::Requested(Intent::Stop)));
        assert_eq!(rb.stop.as_deref(), Some("graceful"));
        assert!(!h.alive(310) && !h.alive(311));
        assert!(!h.dir_exists(&dom("b")));

        // Both are over now; asking again changes nothing.
        let t = terminate(op, &w("batch/a"), Intent::Kill).unwrap();
        assert!(matches!(t, Terminated::Already(Phase::Stopped)));
        let _ = ctx;
    }

    fn keep_policy() -> String {
        text(
            &restart("on-failure", 3, 10, "60s", "10ms", "40ms", 30),
            "workloads = 2",
            "allow.root = yes",
            "[recover]\norphans = keep\n",
        )
    }

    #[test]
    fn the_policy_can_keep_the_orphans_but_an_operator_request_still_ends_them() {
        rig!(h, c, s, p, op, ctx, keep_policy());
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        assert_eq!(reap(op, &w("batch/a")).unwrap(), Some(Phase::Failed));
        let r = record(&s, "a");
        assert_eq!((r.phase, r.ended), (Phase::Failed, Some(Ending::Lost)));
        assert_eq!(r.reason, "supervisor lost: the workload was kept running");
        assert!(h.alive(300) && h.alive(301));
        assert!(h.dir_exists(&dom("a")));
        // `reset` is the explicit decision to end what was kept.
        assert_eq!(reset(op, &w("batch/a")).unwrap(), Some(Phase::Failed));
        assert!(!h.alive(300) && !h.alive(301));
        assert!(!h.dir_exists(&dom("a")));
        assert!(!has_record(&s, "a"));

        // Somebody asked for the stop: that is not a workload to keep.
        let _ = orphan(&h, &c, &s, "b", &[310], false);
        let mut rb = record(&s, "b");
        rb.ask(Intent::Stop);
        s.lock()
            .unwrap()
            .commit(&mut rb, Audit::Optional, Event::Intent, "stop asked")
            .unwrap();
        assert_eq!(reap(op, &w("batch/b")).unwrap(), Some(Phase::Stopped));
        assert!(!h.alive(310));

        let _ = orphan(&h, &c, &s, "c", &[320], false);
        let t = terminate(op, &w("batch/c"), Intent::Kill).unwrap();
        assert!(matches!(t, Terminated::Swept(Phase::Stopped)));
        assert!(!h.alive(320));
        let _ = ctx;
    }

    #[test]
    fn recover_cleans_up_what_is_orphaned_and_leaves_a_live_supervisor_alone() {
        rig!(h, c, s, p, op, ctx, standard());
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        c.create(&w("batch/b"), &Limits::default()).unwrap();
        h.proc_in(310, &[dom("b").as_str()]);
        let _held = orphan(&h, &c, &s, "c", &[320], true);
        let before = record(&s, "c").render();

        let out = recover(op, Orphans::Kill).unwrap();
        assert_eq!(out.swept, vec![("batch/a".to_string(), Phase::Failed)]);
        assert_eq!(out.domains.killed, vec!["batch/b".to_string()]);
        assert_eq!(out.domains.live, vec!["batch/c".to_string()]);
        assert!(out.domains.stuck.is_empty() && out.quarantined.is_empty());
        assert!(!out.healed);
        assert!(!h.alive(300) && !h.alive(301) && !h.alive(310));
        assert!(!h.dir_exists(&dom("a")) && !h.dir_exists(&dom("b")));
        let ra = record(&s, "a");
        assert_eq!((ra.phase, ra.ended), (Phase::Failed, Some(Ending::Lost)));
        // The supervisor that is alive does not know it happened.
        assert!(h.alive(320));
        assert!(h.dir_exists(&dom("c")));
        assert_eq!(record(&s, "c").render(), before);
        let last = details(&s, Event::Recover);
        assert_eq!(
            last.last().map(String::as_str),
            Some("swept=1 quarantined=0 removed=0 killed=1 kept=0 live=1 stuck=0 pruned=0")
        );
        let _ = ctx;
    }

    #[test]
    fn recover_can_keep_the_orphans_when_asked_to() {
        rig!(h, c, s, p, op, ctx, standard());
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        c.create(&w("batch/b"), &Limits::default()).unwrap();
        h.proc_in(310, &[dom("b").as_str()]);
        let out = recover(op, Orphans::Keep).unwrap();
        assert_eq!(out.swept, vec![("batch/a".to_string(), Phase::Failed)]);
        let mut kept = out.domains.kept.clone();
        kept.sort();
        assert_eq!(kept, ["batch/a", "batch/b"]);
        assert!(out.domains.killed.is_empty());
        assert!(h.alive(300) && h.alive(301) && h.alive(310));
        let ra = record(&s, "a");
        assert_eq!(ra.phase, Phase::Failed);
        assert_eq!(ra.reason, "supervisor lost: the workload was kept running");
        let _ = ctx;
    }

    #[test]
    fn recover_reports_a_domain_it_cannot_remove_once_and_finishes_later() {
        rig!(h, c, s, p, op, ctx, standard());
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        critical(&h, &s, 900);
        h.deny_writes_to("agent-interaction/system/cgroup.procs");
        h.proc_in(900, &[dom("a").as_str()]);

        let out = recover(op, Orphans::Kill).unwrap();
        let named = out
            .domains
            .stuck
            .iter()
            .filter(|(n, _)| n == "batch/a")
            .count();
        assert_eq!(named, 1, "{:?}", out.domains.stuck);
        assert!(
            out.domains.stuck[0].1.contains("critical"),
            "{:?}",
            out.domains.stuck
        );
        assert!(out.swept.is_empty());
        // The workload is gone, the critical process is not touched and not signalled.
        assert!(!h.alive(300) && !h.alive(301));
        assert!(h.alive(900));
        assert!(!signalled(&h, 900));
        assert_eq!(h.procs_of(&dom("a")), vec![900]);
        assert!(h.dir_exists(&dom("a")));
        let ra = record(&s, "a");
        assert_eq!(ra.phase, Phase::Failed);
        assert!(
            ra.reason.contains("domain was not removed"),
            "{}",
            ra.reason
        );

        // Once the process is gone the domain can be removed.
        h.exit_proc(900);
        let again = recover(op, Orphans::Kill).unwrap();
        assert!(again.domains.stuck.is_empty());
        assert_eq!(again.domains.removed, vec!["batch/a".to_string()]);
        assert!(!h.dir_exists(&dom("a")));
        let _ = ctx;
    }

    #[test]
    fn recover_sets_a_record_it_cannot_read_aside() {
        rig!(h, c, s, p, op, ctx, standard());
        let _ = orphan(&h, &c, &s, "a", &[300], false);
        h.set(&format!("{DIR}/records/batch.a"), "garbage");
        let out = recover(op, Orphans::Kill).unwrap();
        assert_eq!(out.quarantined, vec!["batch.a".to_string()]);
        // The domain it belonged to is nobody's now.
        assert_eq!(out.domains.killed, vec!["batch/a".to_string()]);
        assert!(!h.alive(300));
        assert!(!h.dir_exists(&dom("a")));
        assert!(!has_record(&s, "a"));
        let set_aside = h.files_of(&format!("{DIR}/quarantine"));
        assert!(
            set_aside.iter().any(|n| n.starts_with("batch.a.")),
            "{set_aside:?}"
        );
        assert_eq!(count(&s, Event::Quarantine), 1);
        let _ = ctx;
    }

    #[test]
    fn supervising_a_workload_whose_supervisor_was_lost_cleans_up_and_asks_for_a_reset() {
        rig!(h, c, s, p, op, ctx, standard());
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        let e = supervise(ctx, &job("a")).unwrap_err();
        assert!(matches!(e, Error::Lifecycle(_)), "{e}");
        let msg = e.to_string();
        assert!(msg.contains("had a supervisor that was lost"), "{msg}");
        assert!(msg.contains("reset"), "{msg}");
        let r = record(&s, "a");
        assert_eq!((r.phase, r.ended), (Phase::Failed, Some(Ending::Lost)));
        assert!(!h.alive(300) && !h.alive(301));
        assert!(!h.dir_exists(&dom("a")));
        assert_eq!(p.mains().len(), 0);
        assert_eq!(p.reapers(), 0);
        let _ = op;
    }

    #[test]
    fn the_reaper_leaves_a_finished_workload_alone() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[Script::ends(Exit::Code(0), 0)]);
        supervise(ctx, &job("a")).unwrap();
        let before = record(&s, "a").render();
        assert_eq!(reap(op, &w("batch/a")).unwrap(), None);
        assert_eq!(record(&s, "a").render(), before);
        assert!(!h.dir_exists(&dom("a")));
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Critical processes: outside every kill domain, never killed, and protected again after a failure and a recovery.

    #[test]
    fn a_critical_process_that_cannot_be_moved_out_is_left_alone_and_its_domain_is_reported() {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        h.deny_writes_to("agent-interaction/system/cgroup.procs");
        let hr = &h;
        // A failing run would be restarted, if its domain could be removed.
        p.scripts(&[Script::ends(Exit::Code(1), 1)]);
        p.on_poll(move |n| {
            if n == 1 {
                hr.proc_in(900, &[dom("a").as_str()]);
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!(out.phase, Phase::Failed);
        assert_eq!(out.code(), 1);
        assert!(
            out.reason.contains("the domain was not removed"),
            "{}",
            out.reason
        );
        assert!(out.reason.contains("critical"), "{}", out.reason);
        assert_eq!(p.mains().len(), 1, "no restart into a domain left behind");
        // The end of the run is recorded as it was seen.
        let r = record(&s, "a");
        assert_eq!(
            (r.phase, r.ended, r.status),
            (Phase::Failed, Some(Ending::Failed(1)), Some(Exit::Code(1)))
        );
        // The critical process is untouched, in the domain it could not leave, and the domain stays.
        assert!(h.alive(900));
        assert!(!signalled(&h, 900));
        assert_eq!(h.procs_of(&dom("a")), vec![900]);
        assert!(h.dir_exists(&dom("a")));
        assert!(
            trail(&s).iter().any(|l| l.event == Event::Rescue
                && l.subject == "pid900"
                && l.detail.contains("moved=false")),
            "{:?}",
            details(&s, Event::Rescue)
        );

        // Tearing down reports the domain, keeps its record and leaves the process alone.
        let td = teardown(op).unwrap();
        assert!(
            td.domains
                .stuck
                .iter()
                .any(|(n, why)| n == "batch/a" && why.contains("critical")),
            "{:?}",
            td.domains.stuck
        );
        assert!(!td.forgotten.contains(&"batch/a".to_string()));
        assert!(has_record(&s, "a"));
        assert!(h.alive(900));
        assert!(!signalled(&h, 900));
        assert!(h.dir_exists("/c/agent-interaction/workload"));

        // Once the process is gone the next teardown finishes the job.
        h.exit_proc(900);
        let td = teardown(op).unwrap();
        assert!(td.domains.stuck.is_empty(), "{:?}", td.domains.stuck);
        assert!(td.forgotten.contains(&"batch/a".to_string()));
        assert!(!has_record(&s, "a"));
        assert!(!h.dir_exists("/c/agent-interaction/workload"));
    }

    #[test]
    fn a_critical_process_found_in_the_domain_is_moved_out_and_survives_the_kill() {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        let hr = &h;
        p.scripts(&[Script::lives()]);
        p.on_poll(move |n| match n {
            2 => hr.proc_in(900, &[dom("a").as_str()]),
            3 => assert!(matches!(
                plan(op, &w("batch/a"), Intent::Kill),
                Ok(Plan::Wait)
            )),
            _ => {}
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Stopped, 1, 0));
        let r = record(&s, "a");
        // Only the process of the workload was killed: the critical one is not counted among them.
        assert_eq!(r.stop.as_deref(), Some("killed:1"));
        assert_eq!(r.ended, Some(Ending::Requested(Intent::Kill)));
        assert!(h.alive(900));
        assert!(!signalled(&h, 900));
        assert!(h.procs_of(SYSDOM).contains(&900));
        assert!(!h.dir_exists(&dom("a")));
        assert!(
            trail(&s).iter().any(|l| l.event == Event::Rescue
                && l.subject == "pid900"
                && l.detail.contains("moved=true")),
            "{:?}",
            details(&s, Event::Rescue)
        );
        assert!(s.critical_pids().unwrap().contains(&900));
    }

    #[test]
    fn a_critical_process_found_in_the_domain_is_moved_out_and_survives_the_stop() {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        let hr = &h;
        p.scripts(&[Script::lives()]);
        p.on_poll(move |n| match n {
            2 => hr.proc_in(900, &[dom("a").as_str()]),
            3 => assert!(matches!(
                plan(op, &w("batch/a"), Intent::Stop),
                Ok(Plan::Wait)
            )),
            _ => {}
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Stopped, 1, 0));
        let r = record(&s, "a");
        assert_eq!(r.stop.as_deref(), Some("graceful"));
        assert!(h.alive(900));
        assert!(!signalled(&h, 900), "{:?}", h.signals());
        assert!(h.procs_of(SYSDOM).contains(&900));
        let pid = p.main_pids()[0];
        assert!(!h.alive(pid));
        assert!(!h.dir_exists(&dom("a")));
    }

    #[test]
    fn a_crash_loop_never_touches_the_critical_or_the_other_system_processes() {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        h.proc_in(701, &[SYSDOM]);
        let hr = &h;
        p.scripts(&[Script::ends(Exit::Code(1), 1)]);
        p.on_poll(move |_| {
            assert!(hr.alive(900) && hr.alive(701));
            assert!(hr.procs_of(SYSDOM).contains(&900));
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.restarts), (Phase::Failed, 3));
        assert_eq!(p.mains().len(), 4);
        assert!(h.signals().is_empty(), "{:?}", h.signals());
        assert!(h.alive(900) && h.alive(701));
        let mut system = h.procs_of(SYSDOM);
        system.sort();
        assert_eq!(system, vec![701, 900]);
        assert!(!h.dir_exists(&dom("a")));
        let _ = op;
    }

    #[test]
    fn ending_one_workload_leaves_the_others_and_the_critical_services_alone() {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        let _held = orphan(&h, &c, &s, "b", &[310, 311], true);
        let before = record(&s, "b").render();
        p.scripts(&[Script::lives()]);
        p.on_poll(move |n| {
            if n == 3 {
                assert!(matches!(
                    plan(op, &w("batch/a"), Intent::Kill),
                    Ok(Plan::Wait)
                ));
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!(out.phase, Phase::Stopped);
        assert!(!h.dir_exists(&dom("a")));
        // The other workload, its supervisor and its processes were not part of it.
        assert_eq!(record(&s, "b").render(), before);
        let mut procs = h.procs_of(&dom("b"));
        procs.sort();
        assert_eq!(procs, vec![310, 311]);
        assert!(!signalled(&h, 310) && !signalled(&h, 311));
        assert!(h.alive(900) && !signalled(&h, 900));
        assert_eq!(s.sup_state(&w("batch/b")).unwrap(), Sup::Alive);
    }

    #[test]
    fn the_prelude_moves_a_critical_process_found_in_a_domain_back_to_system() {
        rig!(h, c, s, p, op, ctx, standard());
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        critical(&h, &s, 900);
        h.proc_in(900, &[dom("a").as_str()]);
        h.proc_in(300, &[dom("a").as_str()]);
        assert!(!prelude(&c, &s).unwrap());
        assert!(h.procs_of(SYSDOM).contains(&900));
        assert_eq!(h.procs_of(&dom("a")), vec![300]);
        assert!(
            trail(&s).iter().any(|l| l.event == Event::Rescue
                && l.subject == "pid900"
                && l.detail.contains("moved=true")),
            "{:?}",
            details(&s, Event::Rescue)
        );
        // Nothing more to find the second time.
        assert!(!prelude(&c, &s).unwrap());
        assert_eq!(count(&s, Event::Rescue), 1);
        let _ = ctx;
    }

    #[test]
    fn a_corrupt_registry_fails_closed_and_is_rebuilt_from_system() {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        h.proc_in(901, &[SYSDOM]);
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.proc_in(310, &[dom("a").as_str()]);
        h.set(&format!("{DIR}/critical"), "garbage");
        // Nobody can tell what is critical now: ending anything is refused rather than done blindly.
        let r = c.kill(&w("batch/a"));
        assert!(matches!(r, Err(Error::State(_))), "{r:?}");
        assert!(h.alive(310) && h.alive(900) && h.alive(901));

        assert!(heal(&c, &s).unwrap());
        let mut known = s.critical_pids().unwrap();
        known.sort();
        assert_eq!(known, vec![900, 901]);
        assert!(h
            .files_of(&format!("{DIR}/quarantine"))
            .iter()
            .any(|f| f.starts_with("critical.")));
        assert_eq!(
            details(&s, Event::Recover),
            vec!["registry rebuilt from 2 process(es) of system".to_string()]
        );
        // Healed once, and the kill works again, around the processes that are critical.
        assert!(!heal(&c, &s).unwrap());
        c.kill(&w("batch/a")).unwrap();
        assert!(!h.alive(310));
        assert!(h.alive(900) && h.alive(901));
        let _ = ctx;
    }

    #[test]
    fn recovery_rebuilds_a_corrupt_registry_before_it_ends_anything() {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        h.set(&format!("{DIR}/critical"), "garbage");
        let out = recover(op, Orphans::Kill).unwrap();
        assert!(out.healed);
        assert_eq!(out.swept, vec![("batch/a".to_string(), Phase::Failed)]);
        assert!(!h.alive(300) && !h.alive(301));
        assert!(h.alive(900) && !signalled(&h, 900));
        assert!(s.critical_pids().unwrap().contains(&900));
        assert!(!h.dir_exists(&dom("a")));
        let _ = ctx;
    }

    #[test]
    fn supervising_rebuilds_a_corrupt_registry_first() {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        h.set(&format!("{DIR}/critical"), "garbage");
        p.scripts(&[Script::ends(Exit::Code(0), 1)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!(out.phase, Phase::Finished);
        assert!(s.registry_corrupt().unwrap().is_none());
        assert!(s.critical_pids().unwrap().contains(&900));
        assert!(h
            .files_of(&format!("{DIR}/quarantine"))
            .iter()
            .any(|f| f.starts_with("critical.")));
        assert!(h.alive(900) && !signalled(&h, 900));
        let _ = op;
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Capacity, start rate and the failures of a start: nothing is left behind, and nothing runs that should not.

    #[test]
    fn the_policy_bounds_how_many_workloads_are_supervised_at_once() {
        rig!(h, c, s, p, op, ctx, standard());
        let _a = orphan(&h, &c, &s, "a", &[300], true);
        let _b = orphan(&h, &c, &s, "b", &[310], true);
        let r = supervise(ctx, &job("c"));
        match r {
            Err(Error::Refused(m)) => assert!(
                m.contains("2 workloads are supervised already; the policy allows 2 at a time"),
                "{m}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(!has_record(&s, "c"));
        assert!(!c.exists(&w("batch/c")));
        assert_eq!(s.sup_state(&w("batch/c")).unwrap(), Sup::Absent);
        assert!(p.mains().is_empty());
        assert_eq!(p.reapers(), 0);
        // The two that run are not disturbed by the refusal.
        assert_eq!(h.procs_of(&dom("a")), vec![300]);
        assert_eq!(h.procs_of(&dom("b")), vec![310]);
        assert!(h.signals().is_empty());
        let _ = op;
    }

    #[test]
    fn a_workload_whose_supervisor_is_lost_does_not_use_up_the_capacity() {
        rig!(h, c, s, p, op, ctx, standard());
        let _a = orphan(&h, &c, &s, "a", &[300], true);
        let _ = orphan(&h, &c, &s, "b", &[310], false);
        p.scripts(&[Script::ends(Exit::Code(0), 1)]);
        let out = supervise(ctx, &job("c")).unwrap();
        assert_eq!(out.phase, Phase::Finished);
        // Starting the third did not touch the lost one: that is the business of `recover` or of its own reaper.
        assert_eq!(record(&s, "b").phase, Phase::Running);
        assert_eq!(h.procs_of(&dom("b")), vec![310]);
        assert!(!signalled(&h, 310));
        assert_eq!(h.procs_of(&dom("a")), vec![300]);
        let _ = op;
    }

    #[test]
    fn the_start_rate_of_the_machine_also_bounds_first_starts() {
        rig!(
            h,
            c,
            s,
            p,
            op,
            ctx,
            with_restart(&restart("on-failure", 3, 10, "60s", "10ms", "40ms", 1))
        );
        p.scripts(&[Script::ends(Exit::Code(0), 1)]);
        assert_eq!(supervise(ctx, &job("a")).unwrap().phase, Phase::Finished);
        let r = supervise(ctx, &job("b"));
        match r {
            Err(Error::Refused(m)) => assert!(
                m.contains("1 starts within the last minute: try again in"),
                "{m}"
            ),
            other => panic!("{other:?}"),
        }
        // Refused means nothing: no record, no domain, no lock, no process.
        assert!(!has_record(&s, "b"));
        assert!(!c.exists(&w("batch/b")));
        assert_eq!(s.sup_state(&w("batch/b")).unwrap(), Sup::Absent);
        assert_eq!(p.mains().len(), 1);
        // A minute later the place is free again.
        h.set_clock(h.clock() + 61_000);
        assert_eq!(supervise(ctx, &job("b")).unwrap().phase, Phase::Finished);
        let _ = op;
    }

    #[test]
    fn a_domain_that_is_in_the_way_is_not_taken_over() {
        rig!(h, c, s, p, op, ctx, standard());
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.proc_in(300, &[dom("a").as_str()]);
        let r = supervise(ctx, &job("a"));
        assert!(matches!(r, Err(Error::Exists(_))), "{r:?}");
        assert!(!has_record(&s, "a"));
        assert_eq!(s.sup_state(&w("batch/a")).unwrap(), Sup::Absent);
        assert!(h.dir_exists(&dom("a")));
        assert_eq!(h.procs_of(&dom("a")), vec![300]);
        assert!(h.alive(300) && !signalled(&h, 300));
        assert!(p.mains().is_empty());
        // A start that never happened did not use up the start rate.
        assert_eq!(s.lock().unwrap().start_wait(h.clock(), 1), None);
        let _ = op;
    }

    #[test]
    fn a_class_the_policy_does_not_know_is_refused_before_anything_exists() {
        rig!(h, c, s, p, op, ctx, standard());
        let mut j = job("a");
        j.workload = w("weird/a");
        let r = supervise(ctx, &j);
        assert!(matches!(r, Err(Error::Usage(_))), "{r:?}");
        assert!(matches!(s.peek(&w("weird/a")), Ok(Stored::Missing)));
        assert!(!c.exists(&w("weird/a")));
        assert_eq!(s.sup_state(&w("weird/a")).unwrap(), Sup::Absent);
        assert!(p.mains().is_empty());
        assert_eq!(s.lock().unwrap().start_wait(h.clock(), 1), None);
        let _ = op;
    }

    #[test]
    fn a_run_that_cannot_be_set_up_fails_its_record_with_the_reason() {
        rig!(h, c, s, p, op, ctx, standard());
        let mut j = job("a");
        j.workload = w("weird/a");
        let secs = h.epoch_secs();
        let mut rec = Record::new("weird/a", secs);
        rec.advance(Phase::Starting, secs).unwrap();
        let r = launch(ctx, &j, &mut rec, false);
        assert!(
            matches!(r, Err(Fault::Handled(Error::Usage(_)))),
            "a setup that cannot get better is not retried"
        );
        assert_eq!(rec.phase, Phase::Failed);
        assert!(rec.reason.starts_with("cannot start:"), "{}", rec.reason);
        assert_eq!(rec.ended, Some(Ending::LaunchFailed));
        assert!(p.mains().is_empty());
        let _ = op;
    }

    #[test]
    fn a_workload_that_is_supervised_already_is_left_exactly_as_it_is() {
        rig!(h, c, s, p, op, ctx, standard());
        let _held = orphan(&h, &c, &s, "a", &[300], true);
        let before = record(&s, "a").render();
        let r = supervise(ctx, &job("a"));
        match r {
            Err(Error::Lifecycle(m)) => {
                assert!(m.contains("batch/a is supervised already"), "{m}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(record(&s, "a").render(), before);
        assert_eq!(h.procs_of(&dom("a")), vec![300]);
        assert!(h.alive(300) && !signalled(&h, 300));
        assert!(p.mains().is_empty());
        assert_eq!(p.reapers(), 0);
        let _ = op;
    }

    #[test]
    fn supervising_without_a_command_is_a_usage_error() {
        rig!(h, c, s, p, op, ctx, standard());
        let mut j = job("a");
        j.argv.clear();
        let r = supervise(ctx, &j);
        assert!(matches!(r, Err(Error::Usage(_))), "{r:?}");
        assert!(!has_record(&s, "a"));
        assert!(!c.exists(&w("batch/a")));
        assert!(p.mains().is_empty());
        let _ = op;
    }

    // -----------------------------------------------------------------------------------------------------------------
    // The main process: it checks, enters its domain and checks again before any of the workload runs.

    fn policy_with_run(run: &str) -> String {
        text(
            &restart("on-failure", 3, 10, "60s", "10ms", "40ms", 30),
            "workloads = 2",
            run,
            "",
        )
    }

    fn argv() -> Vec<String> {
        vec!["worker".to_string(), "--flag".to_string()]
    }

    #[test]
    fn the_main_process_enters_its_domain_before_it_becomes_the_workload() {
        rig!(h, c, s, p, op, ctx, policy_with_run("allow.root = yes"));
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.set_euid(0);
        let l = prepare_main(&h, &c, &w("batch/a"), &argv()).unwrap();
        assert_eq!(
            l,
            Launch {
                argv: argv(),
                uid: None,
                gid: None
            }
        );
        assert!(h.procs_of(&dom("a")).contains(&my_pid()));
        let _ = (&s, &p, &op, &ctx);
    }

    #[test]
    fn a_workload_does_not_run_as_root_unless_the_policy_says_so() {
        rig!(h, c, s, p, op, ctx, policy_with_run(""));
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.set_euid(0);
        let r = prepare_main(&h, &c, &w("batch/a"), &argv());
        assert!(matches!(r, Err(Error::Refused(_))), "{r:?}");
        // Refused before the move: the process never entered the domain.
        assert!(h.procs_of(&dom("a")).is_empty());
        let _ = (&s, &p, &op, &ctx);
    }

    #[test]
    fn the_main_process_drops_to_the_uid_and_gid_of_the_policy() {
        rig!(
            h,
            c,
            s,
            p,
            op,
            ctx,
            policy_with_run("uid = 1000\ngid = 1000")
        );
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.set_euid(0);
        let l = prepare_main(&h, &c, &w("batch/a"), &argv()).unwrap();
        assert_eq!((l.uid, l.gid), (Some(1000), Some(1000)));
        assert!(h.procs_of(&dom("a")).contains(&my_pid()));
        let _ = (&s, &p, &op, &ctx);
    }

    #[test]
    fn a_main_process_that_is_not_root_keeps_its_own_identity() {
        rig!(
            h,
            c,
            s,
            p,
            op,
            ctx,
            policy_with_run("uid = 1000\ngid = 1000")
        );
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.set_euid(1000);
        let l = prepare_main(&h, &c, &w("batch/a"), &argv()).unwrap();
        assert_eq!((l.uid, l.gid), (None, None));
        let _ = (&s, &p, &op, &ctx);
    }

    #[test]
    fn nothing_runs_when_the_main_process_cannot_enter_its_domain() {
        rig!(h, c, s, p, op, ctx, policy_with_run("allow.root = yes"));
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.set_euid(0);
        h.deny_writes_to("batch/a/cgroup.procs");
        let r = prepare_main(&h, &c, &w("batch/a"), &argv());
        assert!(r.is_err(), "{r:?}");
        assert!(!h.procs_of(&dom("a")).contains(&my_pid()));
        let _ = (&s, &p, &op, &ctx);
    }

    #[test]
    fn nothing_runs_when_the_domain_is_gone() {
        rig!(h, c, s, p, op, ctx, policy_with_run("allow.root = yes"));
        h.set_euid(0);
        let r = prepare_main(&h, &c, &w("batch/a"), &argv());
        assert!(r.is_err(), "{r:?}");
        let _ = (&s, &p, &op, &ctx);
    }

    #[test]
    fn a_main_process_without_a_command_is_a_usage_error() {
        rig!(h, c, s, p, op, ctx, policy_with_run("allow.root = yes"));
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        let r = prepare_main(&h, &c, &w("batch/a"), &[]);
        assert!(matches!(r, Err(Error::Usage(_))), "{r:?}");
        assert!(h.procs_of(&dom("a")).is_empty());
        let _ = (&s, &p, &op, &ctx);
    }

    #[test]
    fn a_critical_process_cannot_be_made_into_a_workload() {
        rig!(h, c, s, p, op, ctx, policy_with_run("allow.root = yes"));
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.set_euid(0);
        // Whatever the process is, being critical is stronger than being asked to enter a domain.
        critical(&h, &s, my_pid());
        let r = prepare_main(&h, &c, &w("batch/a"), &argv());
        assert!(matches!(r, Err(Error::Refused(_))), "{r:?}");
        assert!(h.procs_of(&dom("a")).is_empty());
        assert!(h.procs_of(SYSDOM).contains(&my_pid()));
        let _ = (&p, &op, &ctx);
    }

    #[test]
    fn becoming_the_workload_fails_cleanly_when_there_is_nothing_to_run() {
        let e = exec_main(&Launch {
            argv: vec![],
            uid: None,
            gid: None,
        });
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        // Only a program that does not exist is run here: exec replaces the test process when it works.
        let e = exec_main(&Launch {
            argv: vec!["/nonexistent/resctl-no-such-program".to_string()],
            uid: None,
            gid: None,
        });
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    // -----------------------------------------------------------------------------------------------------------------
    // What can be seen: the state, the audit trail. What operators do: teardown, termination, reset.

    #[test]
    fn the_state_shows_a_running_workload_and_a_finished_one() {
        let seen: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let sr = &seen;
        rig!(h, c, s, p, op, ctx, standard());
        assert!(state_lines(op, None).unwrap().is_empty());
        p.scripts(&[Script::ends(Exit::Code(0), 3)]);
        p.on_poll(move |n| {
            if n == 2 {
                *sr.borrow_mut() = state_lines(op, None).unwrap();
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!(out.phase, Phase::Finished);

        let mid = seen.borrow().clone();
        assert_eq!(mid.len(), 1, "{mid:?}");
        let parts = [
            "batch/a phase=running gen=1 restarts=0 supervisor=alive domain=present main="
                .to_string(),
            format!(
                " sup={} intent=- ended=- status=- oom=no stop=- since=",
                my_pid()
            ),
            " cmd=worker reason=main=".to_string(),
        ];
        for part in &parts {
            assert!(mid[0].contains(part.as_str()), "{part:?} not in {}", mid[0]);
        }

        let end = state_lines(op, None).unwrap();
        assert_eq!(end.len(), 1, "{end:?}");
        let parts = [
            "batch/a phase=finished gen=1 restarts=0 supervisor=none domain=absent main=- sup=- intent=- ended=clean status=exit:0 oom=no ",
            " cmd=worker reason=the_workload_ended_cleanly",
        ];
        for part in parts {
            assert!(end[0].contains(part), "{part:?} not in {}", end[0]);
        }
        // One workload, asked for by name; one that never existed is an error, not an empty answer.
        assert_eq!(state_lines(op, Some(&w("batch/a"))).unwrap(), end);
        let r = state_lines(op, Some(&w("batch/zzz")));
        assert!(
            matches!(&r, Err(Error::Lifecycle(m)) if m.contains("has no lifecycle record")),
            "{r:?}"
        );
    }

    #[test]
    fn the_state_shows_a_lost_supervisor_and_files_that_are_not_records() {
        rig!(h, c, s, p, op, ctx, standard());
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        h.set(&format!("{DIR}/records/batch.b"), "garbage");
        h.set(&format!("{DIR}/records/not-a-record"), "x");
        let before = trail(&s).len();
        let lines = state_lines(op, None).unwrap();
        assert_eq!(lines.len(), 3, "{lines:?}");
        let a = lines.iter().find(|l| l.starts_with("batch/a ")).unwrap();
        assert!(
            a.contains(" phase=running gen=1 restarts=0 supervisor=lost domain=present main=300 sup=4000 intent=-"),
            "{a}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("batch.b corrupt=")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "not-a-record corrupt=not_the_name_of_a_record"),
            "{lines:?}"
        );
        // Looking changes nothing.
        assert_eq!(trail(&s).len(), before);
        assert!(h.alive(300) && h.alive(301) && h.dir_exists(&dom("a")));
        assert!(h.signals().is_empty());
        let only = state_lines(op, Some(&w("batch/a"))).unwrap();
        assert_eq!(only.len(), 1);
        let _ = ctx;
    }

    #[test]
    fn the_audit_trail_follows_every_step_of_a_crash_and_its_restart() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[
            Script::ends(Exit::Code(1), 1),
            Script::ends(Exit::Code(0), 1),
        ]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Finished, 2, 1));
        assert_eq!(
            events(&s),
            vec![
                Event::Supervise,
                Event::Start,
                Event::Phase,
                Event::Exit,
                Event::Restart,
                Event::Phase,
                Event::Start,
                Event::Phase,
                Event::Exit,
                Event::Phase,
            ]
        );
        assert_eq!(
            details(&s, Event::Exit),
            vec![
                "gen=1 ended=failed:1 status=exit:1 oom=no".to_string(),
                "gen=2 ended=clean status=exit:0 oom=no".to_string(),
            ]
        );
        assert_eq!(
            details(&s, Event::Restart),
            vec!["phase=backoff gen=1 failed:1; restart 1 in 10ms".to_string()]
        );
        // Each line says who, about what, and in which order.
        let lines = trail(&s);
        assert!(lines
            .iter()
            .all(|l| l.actor == "4242:test" && l.subject == "batch/a"));
        for pair in lines.windows(2) {
            assert_eq!(pair[1].seq, pair[0].seq + 1, "{pair:?}");
        }
        let _ = op;
    }

    #[test]
    fn teardown_ends_everything_forgets_the_records_and_keeps_the_critical_processes() {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        p.scripts(&[Script::ends(Exit::Code(0), 1)]);
        assert_eq!(supervise(ctx, &job("f")).unwrap().phase, Phase::Finished);
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        let _ = orphan(&h, &c, &s, "c", &[302], false);
        h.set(&format!("{DIR}/records/zz-junk"), "x");

        let td = teardown(op).unwrap();
        let mut gone = td.forgotten.clone();
        gone.sort();
        assert_eq!(gone, ["batch/a", "batch/c", "batch/f", "zz-junk"]);
        assert!(td.domains.stuck.is_empty(), "{:?}", td.domains.stuck);
        assert!(
            td.domains.kept.iter().any(|k| k.starts_with("system:")),
            "{:?}",
            td.domains.kept
        );
        for pid in [300, 301, 302] {
            assert!(!h.alive(pid), "{pid}");
        }
        for id in ["a", "c", "f"] {
            assert!(!h.dir_exists(&dom(id)), "{id}");
            assert!(!has_record(&s, id), "{id}");
        }
        assert!(c.list().is_empty());
        // The critical process was neither signalled nor deregistered, and the audit trail is still there.
        assert!(h.alive(900) && !signalled(&h, 900));
        assert!(h.procs_of(SYSDOM).contains(&900));
        assert!(s.critical_pids().unwrap().contains(&900));
        assert!(count(&s, Event::Supervise) >= 1);
        assert_eq!(
            details(&s, Event::Teardown),
            vec!["forgotten=4 kept=1 stuck=0".to_string()]
        );
        // What was not a record is set aside, not deleted.
        assert!(h
            .files_of(&format!("{DIR}/quarantine"))
            .iter()
            .any(|f| f.starts_with("zz-junk.")));
        // Nothing is left to do the second time.
        let again = teardown(op).unwrap();
        assert!(again.forgotten.is_empty() && again.domains.stuck.is_empty());
        assert!(h.alive(900));
    }

    #[test]
    fn teardown_reports_a_supervisor_that_does_not_finish_and_ends_it_when_it_is_gone() {
        rig!(h, c, s, p, op, ctx, standard());
        let held = orphan(&h, &c, &s, "a", &[300, 301], true);
        let td = teardown(op).unwrap();
        // The workload is killed, but its record has an owner that is still there: it stays, and is reported.
        assert!(
            td.domains
                .stuck
                .iter()
                .any(|(n, why)| n == "batch/a" && why.contains("within 20s")),
            "{:?}",
            td.domains.stuck
        );
        assert!(!td.forgotten.contains(&"batch/a".to_string()));
        assert!(!h.alive(300) && !h.alive(301));
        assert!(has_record(&s, "a"));
        assert_eq!(record(&s, "a").intent, Some(Intent::Kill));
        assert_eq!(
            details(&s, Event::Teardown),
            vec!["forgotten=0 kept=0 stuck=1".to_string()]
        );

        drop(held);
        let td = teardown(op).unwrap();
        assert!(td.domains.stuck.is_empty(), "{:?}", td.domains.stuck);
        assert_eq!(td.forgotten, ["batch/a"]);
        assert!(!has_record(&s, "a"));
        assert!(!h.dir_exists(&dom("a")));
        assert_eq!(
            details(&s, Event::Teardown).last().map(String::as_str),
            Some("forgotten=1 kept=0 stuck=0")
        );
    }

    #[test]
    fn ending_what_nobody_supervises_changes_nothing_and_what_is_over_stays_over() {
        rig!(h, c, s, p, op, ctx, standard());
        assert_eq!(
            terminate(op, &w("batch/x"), Intent::Kill).unwrap(),
            Terminated::Unmanaged
        );
        // A domain made by hand has no lifecycle: it is not this subsystem's to end.
        c.create(&w("batch/u"), &Limits::default()).unwrap();
        h.proc_in(320, &[dom("u").as_str()]);
        for intent in [Intent::Stop, Intent::Kill] {
            assert_eq!(
                terminate(op, &w("batch/u"), intent).unwrap(),
                Terminated::Unmanaged
            );
        }
        assert!(h.alive(320) && h.dir_exists(&dom("u")));
        assert!(h.signals().is_empty());

        p.scripts(&[Script::ends(Exit::Code(0), 1)]);
        assert_eq!(supervise(ctx, &job("f")).unwrap().phase, Phase::Finished);
        p.scripts(&[Script::ends(Exit::Code(1), 1)]);
        assert_eq!(supervise(ctx, &job("g")).unwrap().phase, Phase::Failed);
        let (f, g) = (record(&s, "f").render(), record(&s, "g").render());
        for intent in [Intent::Stop, Intent::Kill] {
            assert_eq!(
                terminate(op, &w("batch/f"), intent).unwrap(),
                Terminated::Already(Phase::Finished)
            );
            assert_eq!(
                terminate(op, &w("batch/g"), intent).unwrap(),
                Terminated::Already(Phase::Failed)
            );
        }
        // A finished run is not turned into a stopped one, and no request is recorded against it.
        assert_eq!(record(&s, "f").render(), f);
        assert_eq!(record(&s, "g").render(), g);
        assert_eq!(count(&s, Event::Intent), 0);
    }

    #[test]
    fn a_supervisor_that_does_not_answer_is_waited_for_a_bounded_time_and_the_others_are_left_alone(
    ) {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        let _a = orphan(&h, &c, &s, "a", &[300, 301], true);
        let _b = orphan(&h, &c, &s, "b", &[310], true);
        let _k = orphan(&h, &c, &s, "k", &[330], true);
        let t0 = h.clock();

        // A stop is asked for, not done from here: the supervisor is the one that ends it.
        let r = terminate(op, &w("batch/a"), Intent::Stop);
        assert!(
            matches!(&r, Err(Error::Timeout(m)) if m.contains("batch/a: the stop was asked") && m.contains("within 21s")),
            "{r:?}"
        );
        assert!(h.clock() - t0 <= 25_000, "{}", h.clock() - t0);
        assert!(h.alive(300) && h.alive(301));
        assert!(h.signals().is_empty(), "{:?}", h.signals());
        assert_eq!(record(&s, "a").intent, Some(Intent::Stop));

        // A kill is done at once from here, and the supervisor is waited for.
        let r = terminate(op, &w("batch/b"), Intent::Kill);
        assert!(
            matches!(&r, Err(Error::Timeout(m)) if m.contains("batch/b: the kill was asked") && m.contains("within 20s")),
            "{r:?}"
        );
        assert!(!h.alive(310));
        assert_eq!(record(&s, "b").intent, Some(Intent::Kill));

        // A stop that was not enough becomes a kill; the other workloads and the critical process are still untouched.
        let r = terminate(op, &w("batch/a"), Intent::Kill);
        assert!(matches!(&r, Err(Error::Timeout(_))), "{r:?}");
        assert!(!h.alive(300) && !h.alive(301));
        assert_eq!(record(&s, "a").intent, Some(Intent::Kill));
        assert_eq!(count(&s, Event::Intent), 3);
        assert!(h.alive(330) && h.dir_exists(&dom("k")));
        assert_eq!(record(&s, "k").intent, None);
        assert!(h.alive(900) && !signalled(&h, 900));
        let _ = ctx;
    }

    #[test]
    fn a_request_is_recorded_once_and_a_kill_is_never_taken_back() {
        rig!(h, c, s, p, op, ctx, standard());
        let _held = orphan(&h, &c, &s, "a", &[300], true);
        let wl = w("batch/a");
        assert!(matches!(plan(op, &wl, Intent::Stop), Ok(Plan::Wait)));
        assert!(matches!(plan(op, &wl, Intent::Stop), Ok(Plan::Wait)));
        assert_eq!(details(&s, Event::Intent), vec!["stop asked".to_string()]);
        assert_eq!(record(&s, "a").intent, Some(Intent::Stop));

        assert!(matches!(plan(op, &wl, Intent::Kill), Ok(Plan::Wait)));
        assert_eq!(count(&s, Event::Intent), 2);
        assert_eq!(record(&s, "a").intent, Some(Intent::Kill));

        assert!(matches!(plan(op, &wl, Intent::Stop), Ok(Plan::Wait)));
        assert_eq!(count(&s, Event::Intent), 2);
        assert_eq!(record(&s, "a").intent, Some(Intent::Kill));
        // Nothing was done to the processes by recording it.
        assert!(h.alive(300) && h.signals().is_empty());
        let _ = ctx;
    }

    #[test]
    fn a_supervisor_that_is_still_starting_is_not_asked_and_one_with_a_corrupt_record_is() {
        rig!(h, c, s, p, op, ctx, standard());
        let wl = w("batch/a");
        let _g = s.lock().unwrap().claim(&wl).unwrap().unwrap();
        // Alive, and no record yet: there is nothing to put the request in.
        match plan(op, &wl, Intent::Stop) {
            Err(Error::Lifecycle(m)) => assert!(
                m.contains("is being started by its supervisor: try again"),
                "{m}"
            ),
            _ => panic!("a workload that is being started cannot be asked yet"),
        }
        assert!(matches!(s.peek(&wl), Ok(Stored::Missing)));
        assert_eq!(count(&s, Event::Intent), 0);

        // Alive, and a record nobody can read: it is set aside, and the request starts a new one.
        h.set(&format!("{DIR}/records/batch.a"), "garbage");
        assert!(matches!(plan(op, &wl, Intent::Kill), Ok(Plan::Wait)));
        assert!(h
            .files_of(&format!("{DIR}/quarantine"))
            .iter()
            .any(|f| f.starts_with("batch.a.")));
        assert_eq!(count(&s, Event::Quarantine), 1);
        assert_eq!(record(&s, "a").intent, Some(Intent::Kill));
        let _ = ctx;
    }

    #[test]
    fn a_record_nobody_can_read_is_set_aside_and_never_blocks_a_start_or_an_ending() {
        rig!(h, c, s, p, op, ctx, standard());
        h.set(&format!("{DIR}/records/batch.a"), "garbage");
        p.scripts(&[Script::ends(Exit::Code(0), 1)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen), (Phase::Finished, 1));

        h.set(&format!("{DIR}/records/batch.b"), "garbage");
        assert_eq!(
            terminate(op, &w("batch/b"), Intent::Kill).unwrap(),
            Terminated::Unmanaged
        );
        assert!(h.file(&format!("{DIR}/records/batch.b")).is_none());
        let aside = h.files_of(&format!("{DIR}/quarantine"));
        assert!(aside.iter().any(|f| f.starts_with("batch.a.")), "{aside:?}");
        assert!(aside.iter().any(|f| f.starts_with("batch.b.")), "{aside:?}");
        assert_eq!(count(&s, Event::Quarantine), 2);
    }

    #[test]
    fn there_is_nothing_to_reset_and_a_supervisor_that_is_alive_is_not_reset() {
        rig!(h, c, s, p, op, ctx, standard());
        let r = reset(op, &w("batch/x"));
        assert!(
            matches!(&r, Err(Error::Lifecycle(m)) if m.contains("nothing to reset")),
            "{r:?}"
        );
        let _held = orphan(&h, &c, &s, "a", &[300, 301], true);
        let before = record(&s, "a").render();
        let r = reset(op, &w("batch/a"));
        assert!(
            matches!(&r, Err(Error::Lifecycle(m)) if m.contains("has a supervisor that is alive: stop it first")),
            "{r:?}"
        );
        assert_eq!(record(&s, "a").render(), before);
        assert!(h.alive(300) && h.alive(301) && h.dir_exists(&dom("a")));
        assert_eq!(count(&s, Event::Reset), 0);
        let _ = ctx;
    }

    #[test]
    fn reset_removes_a_domain_nobody_recorded_and_a_domain_with_a_corrupt_record() {
        rig!(h, c, s, p, op, ctx, standard());
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.proc_in(300, &[dom("a").as_str()]);
        assert_eq!(reset(op, &w("batch/a")).unwrap(), None);
        assert!(!h.alive(300) && !h.dir_exists(&dom("a")));
        assert_eq!(
            details(&s, Event::Reset),
            vec!["was unrecorded".to_string()]
        );

        c.create(&w("batch/b"), &Limits::default()).unwrap();
        h.proc_in(310, &[dom("b").as_str()]);
        h.set(&format!("{DIR}/records/batch.b"), "garbage");
        assert_eq!(reset(op, &w("batch/b")).unwrap(), None);
        assert!(!h.alive(310) && !h.dir_exists(&dom("b")));
        assert!(!has_record(&s, "b"));
        assert!(h.file(&format!("{DIR}/records/batch.b")).is_none());
        assert!(h
            .files_of(&format!("{DIR}/quarantine"))
            .iter()
            .any(|f| f.starts_with("batch.b.")));
        let _ = ctx;
    }

    #[test]
    fn reset_cleans_up_after_a_lost_supervisor_and_spares_the_critical_process() {
        rig!(h, c, s, p, op, ctx, standard());
        critical(&h, &s, 900);
        let _ = orphan(&h, &c, &s, "a", &[300, 301], false);
        h.proc_in(900, &[dom("a").as_str()]);
        assert_eq!(reset(op, &w("batch/a")).unwrap(), Some(Phase::Failed));
        assert!(!h.alive(300) && !h.alive(301));
        assert!(!h.dir_exists(&dom("a")));
        assert!(!has_record(&s, "a"));
        assert!(h.alive(900) && !signalled(&h, 900));
        assert!(h.procs_of(SYSDOM).contains(&900));
        assert_eq!(details(&s, Event::Reset), vec!["was failed".to_string()]);
        let _ = ctx;
    }

    #[test]
    fn a_crash_loop_ends_failed_until_it_is_reset_and_can_then_be_supervised_again() {
        rig!(h, c, s, p, op, ctx, standard());
        p.scripts(&[Script::ends(Exit::Code(1), 1)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.restarts), (Phase::Failed, 3));
        assert_eq!(p.mains().len(), 4);

        // No more starts until someone has looked: the refusal does not start anything.
        let r = supervise(ctx, &job("a"));
        assert!(
            matches!(&r, Err(Error::Lifecycle(m)) if m.contains("failed") && m.contains("`resctl reset batch/a` first")),
            "{r:?}"
        );
        assert_eq!(p.mains().len(), 4);
        assert!(!h.dir_exists(&dom("a")));

        assert_eq!(reset(op, &w("batch/a")).unwrap(), Some(Phase::Failed));
        assert!(!has_record(&s, "a"));
        p.scripts(&[Script::ends(Exit::Code(0), 1)]);
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.gen, out.restarts), (Phase::Finished, 1, 0));
        assert_eq!(p.mains().len(), 5);
        assert!(!h.dir_exists(&dom("a")));
    }

    // -----------------------------------------------------------------------------------------------------------------
    // The steps of a run, the state that cannot be written, orphans and real processes.

    fn started(f: Flow<Launched>) -> i32 {
        match f {
            Ok(Launched::Up(pid)) => pid,
            Ok(Launched::Broke(m)) => panic!("the run did not start: {m}"),
            Err(Fault::Handled(e) | Fault::Unexpected(e)) => panic!("the run failed: {e}"),
        }
    }

    fn broke(f: Flow<Launched>) -> String {
        match f {
            Ok(Launched::Broke(m)) => m,
            Ok(Launched::Up(pid)) => panic!("the run started: {pid}"),
            Err(Fault::Handled(e) | Fault::Unexpected(e)) => panic!("the run failed: {e}"),
        }
    }

    #[test]
    fn a_run_does_not_begin_when_an_operator_asked_for_its_end_first() {
        rig!(h, c, s, p, op, ctx, standard());
        let j = job("a");
        let (mut rec, _g) = begin(ctx, &j).unwrap();
        assert!(rec.ask(Intent::Stop));
        assert_eq!(
            broke(launch(ctx, &j, &mut rec, true)),
            "a stop was asked before the run started"
        );
        assert!(p.mains().is_empty());
        assert_eq!(rec.phase, Phase::Starting);
        assert!(h.dir_exists(&dom("a")));
        // The check comes before the domain is made: with the domain gone, nothing brings it back.
        assert!(rec.ask(Intent::Kill));
        c.remove(&j.workload).unwrap();
        assert!(!h.dir_exists(&dom("a")));
        assert_eq!(
            broke(launch(ctx, &j, &mut rec, false)),
            "a kill was asked before the run started"
        );
        assert!(!c.exists(&j.workload));
        assert!(p.mains().is_empty());
    }

    #[test]
    fn a_restart_creates_a_new_domain_and_the_main_process_enters_it() {
        rig!(h, c, s, p, op, ctx, standard());
        let j = job("a");
        let (mut rec, _g) = begin(ctx, &j).unwrap();
        c.remove(&j.workload).unwrap();
        assert!(!h.dir_exists(&dom("a")));
        p.scripts(&[Script::lives()]);
        let pid = started(launch(ctx, &j, &mut rec, false));
        assert!(h.dir_exists(&dom("a")));
        assert_eq!(h.procs_of(&dom("a")), vec![pid]);
        assert_eq!((rec.phase, rec.main), (Phase::Running, Some(pid)));
        let stored = record(&s, "a");
        assert_eq!((stored.phase, stored.main), (Phase::Running, Some(pid)));
        assert_eq!(
            details(&s, Event::Start),
            vec![format!("phase=running gen=1 main={pid}")]
        );
        assert_eq!(p.main_pids(), vec![pid]);
    }

    #[test]
    fn a_restart_whose_domain_is_taken_fails_the_record_and_leaves_the_other_process_alone() {
        rig!(h, c, s, p, op, ctx, standard());
        let j = job("a");
        let (mut rec, _g) = begin(ctx, &j).unwrap();
        // Somebody else is in the domain already; the restart must not take it over.
        h.proc_in(300, &[dom("a").as_str()]);
        p.scripts(&[Script::lives()]);
        let r = launch(ctx, &j, &mut rec, false);
        assert!(matches!(r, Err(Fault::Handled(Error::Exists(_)))));
        assert_eq!(rec.phase, Phase::Failed);
        assert_eq!(rec.ended, Some(Ending::LaunchFailed));
        assert!(rec.reason.starts_with("cannot start: "), "{}", rec.reason);
        assert!(p.mains().is_empty());
        assert!(h.alive(300));
        assert_eq!(h.procs_of(&dom("a")), vec![300]);
        assert!(h.dir_exists(&dom("a")));
        assert_eq!(record(&s, "a").render(), rec.render());
    }

    #[test]
    fn a_stop_during_back_off_ends_the_workload_without_a_new_run() {
        rig!(h, c, s, p, op, ctx, standard());
        let wl = w("batch/a");
        let mut rec = Record::new(&wl.to_string(), 1_700_000_000);
        for to in [
            Phase::Starting,
            Phase::Running,
            Phase::Sweeping,
            Phase::Backoff,
        ] {
            rec.advance(to, 1_700_000_000).unwrap();
        }
        halt(ctx, &wl, &mut rec).unwrap();
        assert_eq!(rec.phase, Phase::Stopped);
        assert_eq!(rec.stop.as_deref(), Some("empty"));
        assert_eq!(rec.reason, "stopped during back-off");
        assert_eq!(record(&s, "a").render(), rec.render());
        assert_eq!(
            details(&s, Event::Phase),
            vec!["phase=stopped gen=1 stopped during back-off"]
        );
        assert!(p.mains().is_empty());
        assert!(!h.dir_exists(&dom("a")));
    }

    #[test]
    fn a_pause_is_cut_short_by_a_request_and_not_by_the_clock() {
        rig!(h, c, s, p, op, ctx, standard());
        let j = job("a");
        let wl = j.workload.clone();
        let (mut rec, _g) = begin(ctx, &j).unwrap();
        let t0 = h.clock();
        assert!(pause_for(ctx, &wl, &mut rec, 0) == Reserved::Go);
        assert_eq!(h.clock(), t0);
        assert!(pause_for(ctx, &wl, &mut rec, 1000) == Reserved::Go);
        assert_eq!(h.clock(), t0 + 1000);
        assert!(matches!(plan(op, &wl, Intent::Kill), Ok(Plan::Wait)));
        let t1 = h.clock();
        assert!(pause_for(ctx, &wl, &mut rec, 5000) == Reserved::Asked);
        assert_eq!(h.clock(), t1 + SLICE_MS);
        assert_eq!(rec.intent, Some(Intent::Kill));
    }

    #[test]
    fn a_reservation_waits_until_the_start_rate_allows_one_more_start() {
        rig!(
            h,
            c,
            s,
            p,
            op,
            ctx,
            with_restart(&restart("on-failure", 3, 10, "60s", "10ms", "40ms", 1))
        );
        let j = job("a");
        let wl = j.workload.clone();
        let (mut rec, _g) = begin(ctx, &j).unwrap();
        // `begin` took the one start of this minute; the next one has to wait for it to be a minute old.
        let t0 = h.clock();
        assert!(reserve(ctx, &wl, &mut rec, 1).unwrap() == Reserved::Go);
        assert_eq!(h.clock(), t0 + 60_000);
        assert_eq!(s.lock().unwrap().start_wait(h.now_ms(), 1), Some(60_000));
    }

    #[test]
    fn a_wait_for_the_start_rate_ends_at_a_request_and_takes_nothing() {
        rig!(
            h,
            c,
            s,
            p,
            op,
            ctx,
            with_restart(&restart("on-failure", 3, 10, "60s", "10ms", "40ms", 1))
        );
        let j = job("a");
        let wl = j.workload.clone();
        let (mut rec, _g) = begin(ctx, &j).unwrap();
        assert!(matches!(plan(op, &wl, Intent::Stop), Ok(Plan::Wait)));
        let t0 = h.clock();
        assert!(reserve(ctx, &wl, &mut rec, 1).unwrap() == Reserved::Asked);
        assert_eq!(h.clock(), t0 + SLICE_MS);
        assert_eq!(rec.intent, Some(Intent::Stop));
        // Nothing was booked for the start that did not happen: only the one of `begin` is on the log.
        let lock = s.lock().unwrap();
        assert_eq!(lock.start_wait(h.now_ms(), 2), None);
        assert_eq!(lock.start_wait(h.now_ms(), 1), Some(60_000 - SLICE_MS));
    }

    #[test]
    fn a_phase_the_lifecycle_does_not_allow_is_refused_and_leaves_no_trace() {
        rig!(h, c, s, p, op, ctx, standard());
        let wl = w("batch/a");
        let mut rec = Record::new(&wl.to_string(), 1_700_000_000);
        let before = rec.render();
        let r = go(
            ctx,
            &wl,
            &mut rec,
            Phase::Running,
            Event::Phase,
            "skipping starting",
        );
        assert!(matches!(&r, Err(Error::Lifecycle(_))), "{r:?}");
        assert_eq!(rec.render(), before);
        assert!(!has_record(&s, "a"));
        assert_eq!(count(&s, Event::Phase), 0);
    }

    #[test]
    fn a_supervisor_that_cannot_write_its_record_still_cleans_up_and_warns() {
        rig!(h, c, s, p, op, ctx, standard());
        let hr = &h;
        p.scripts(&[Script::ends(Exit::Code(0), 3)]);
        p.on_poll(move |n| {
            if n == 1 {
                hr.deny_puts_to("records/batch.a");
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        // The run is over and cleaned up whether or not anybody could write that down.
        assert_eq!(out.phase, Phase::Finished);
        assert_eq!(out.restarts, 0);
        assert_eq!(out.code(), 0);
        assert!(!h.dir_exists(&dom("a")));
        assert_eq!(p.reapers(), 1);
        let warnings = s.take_warnings();
        assert!(
            warnings
                .iter()
                .any(|m| m.contains("the lifecycle record was not written")),
            "{warnings:?}"
        );
        // What is on disk stops at the last phase that could be written, and recovery says so.
        assert_eq!(record(&s, "a").phase, Phase::Running);
        h.allow_puts();
        let rec = recover(op, Orphans::Kill).unwrap();
        assert_eq!(rec.swept, vec![("batch/a".to_string(), Phase::Failed)]);
        let stored = record(&s, "a");
        assert_eq!(stored.phase, Phase::Failed);
        assert_eq!(stored.ended, Some(Ending::Lost));
    }

    #[test]
    fn a_request_that_cannot_be_recorded_ends_nothing() {
        for (intent, word) in [(Intent::Stop, "stop"), (Intent::Kill, "kill")] {
            rig!(h, c, s, p, op, ctx, standard());
            let _held = orphan(&h, &c, &s, "a", &[300], true);
            h.deny_puts_to("records/batch.a");
            let r = terminate(op, &w("batch/a"), intent);
            assert!(
                matches!(&r, Err(Error::State(m))
                    if m.contains(&format!("cannot record the {word} of batch/a, so nothing is ended"))),
                "{r:?}"
            );
            assert!(h.alive(300));
            assert!(h.signals().is_empty());
            assert!(h.dir_exists(&dom("a")));
            // Once the state can be written again, the request is taken.
            h.allow_puts();
            assert!(matches!(
                plan(op, &w("batch/a"), Intent::Stop),
                Ok(Plan::Wait)
            ));
            assert_eq!(record(&s, "a").intent, Some(Intent::Stop));
        }
    }

    #[test]
    fn an_internal_error_ends_the_main_process_and_the_domain_and_fails_the_record() {
        rig!(h, c, s, p, op, ctx, standard());
        let j = job("a");
        let (mut rec, _g) = begin(ctx, &j).unwrap();
        p.scripts(&[Script::lives()]);
        let pid = started(launch(ctx, &j, &mut rec, true));
        abandon(ctx, &j.workload, &mut rec, &Error::State("boom".into()));
        assert!(p.killed.borrow().contains(&pid));
        assert!(!h.alive(pid));
        assert!(!h.dir_exists(&dom("a")));
        let stored = record(&s, "a");
        assert_eq!(stored.phase, Phase::Failed);
        assert!(
            stored.reason.contains("internal error: boom"),
            "{}",
            stored.reason
        );
        assert_eq!(stored.ended, Some(Ending::Lost));
        assert_eq!(rec.render(), stored.render());
    }

    #[test]
    fn an_internal_error_does_not_kill_a_critical_process_that_cannot_leave_the_domain() {
        rig!(h, c, s, p, op, ctx, standard());
        let j = job("a");
        let (mut rec, _g) = begin(ctx, &j).unwrap();
        p.scripts(&[Script::lives()]);
        let pid = started(launch(ctx, &j, &mut rec, true));
        critical(&h, &s, 900);
        h.proc_in(900, &[dom("a").as_str()]);
        h.deny_writes_to("agent-interaction/system/cgroup.procs");
        abandon(ctx, &j.workload, &mut rec, &Error::State("boom".into()));
        assert!(h.alive(900));
        assert!(!h.alive(pid));
        assert!(h.dir_exists(&dom("a")));
        let stored = record(&s, "a");
        assert_eq!(stored.phase, Phase::Failed);
        assert!(
            stored.reason.contains("the domain was not removed")
                && stored.reason.contains("critical"),
            "{}",
            stored.reason
        );
    }

    #[test]
    fn only_what_is_not_critical_counts_as_killable() {
        rig!(h, c, s, p, op, ctx, standard());
        let wl = w("batch/a");
        assert_eq!(killable(ctx, &wl), 0);
        c.create(&wl, &Limits::default()).unwrap();
        assert_eq!(killable(ctx, &wl), 0);
        critical(&h, &s, 900);
        let d = dom("a");
        h.proc_in(300, &[d.as_str()]);
        h.proc_in(301, &[d.as_str()]);
        h.proc_in(900, &[d.as_str()]);
        assert_eq!(killable(ctx, &wl), 2);
    }

    #[test]
    fn a_domain_is_reported_stuck_once() {
        let mut into = vec![("batch/a".to_string(), "first".to_string())];
        merge_stuck(
            &mut into,
            vec![
                ("batch/a".to_string(), "second".to_string()),
                ("batch/b".to_string(), "third".to_string()),
            ],
        );
        assert_eq!(
            into,
            vec![
                ("batch/a".to_string(), "first".to_string()),
                ("batch/b".to_string(), "third".to_string()),
            ]
        );
    }

    #[test]
    fn a_main_process_that_is_still_there_is_killed_after_the_settle_time() {
        rig!(h, c, s, p, op, ctx, standard());
        let j = job("a");
        let (mut rec, _g) = begin(ctx, &j).unwrap();
        p.scripts(&[Script::lives()]);
        let pid = started(launch(ctx, &j, &mut rec, true));
        let t0 = h.clock();
        assert_eq!(reap_main(ctx, pid), Some(Exit::Signal(9)));
        assert_eq!(*p.killed.borrow(), vec![pid]);
        assert_eq!(h.clock() - t0, SETTLE_MS + POLL_MS);
    }

    #[test]
    fn the_processes_a_main_process_leaves_behind_are_swept_with_its_domain() {
        rig!(h, c, s, p, op, ctx, standard());
        let hr = &h;
        p.scripts(&[Script::ends(Exit::Code(0), 1).with_kids(&[301, 302])]);
        p.on_poll(move |n| {
            if n == 1 {
                assert!(hr.alive(301) && hr.alive(302));
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!(out.phase, Phase::Finished);
        assert!(!h.alive(301));
        assert!(!h.alive(302));
        assert!(!h.dir_exists(&dom("a")));
        assert_eq!(p.main_pids().len(), 1);
    }

    #[test]
    fn the_children_of_a_crashed_run_are_gone_before_the_next_run_starts() {
        rig!(h, c, s, p, op, ctx, standard());
        let hr = &h;
        p.scripts(&[
            Script::ends(Exit::Code(1), 1).with_kids(&[301, 302]),
            Script::ends(Exit::Code(0), 1).with_kids(&[303]),
        ]);
        // Poll 3 is the first look at the second run: the first run's children must be gone by then.
        p.on_poll(move |n| {
            if n == 3 {
                assert!(!hr.alive(301) && !hr.alive(302) && hr.alive(303));
            }
        });
        let out = supervise(ctx, &job("a")).unwrap();
        assert_eq!((out.phase, out.restarts), (Phase::Finished, 1));
        assert!(!h.alive(301) && !h.alive(302) && !h.alive(303));
        assert!(!h.dir_exists(&dom("a")));
        assert_eq!(p.main_pids().len(), 2);
    }

    #[test]
    fn the_end_of_a_process_is_told_apart_into_a_code_and_a_signal() {
        let st = process::ExitStatus::from_raw;
        assert_eq!(exit_of(st(0)), Exit::Code(0));
        assert_eq!(exit_of(st(3 << 8)), Exit::Code(3));
        assert_eq!(exit_of(st(255 << 8)), Exit::Code(255));
        assert_eq!(exit_of(st(9)), Exit::Signal(9));
        // A core dump is still the signal.
        assert_eq!(exit_of(st(11 | 0x80)), Exit::Signal(11));
        // A process that is stopped, not ended, is neither a code nor a signal; it is not taken for a clean end.
        assert_eq!(exit_of(st((19 << 8) | 0x7f)), Exit::Code(255));
    }

    #[test]
    fn the_helper_processes_are_this_program_again_with_the_same_global_options() {
        let env = Env {
            exe: "resctl".into(),
            globals: vec!["--policy".into(), "/p".into()],
        };
        let enter = env.enter_spec(&w("batch/a"), &["a".to_string(), "b".to_string()]);
        assert_eq!(enter.exe, "resctl");
        assert_eq!(
            enter.args,
            ["--policy", "/p", "__enter", "batch/a", "--", "a", "b"]
        );
        assert!(!enter.detach);
        let reap = env.reap_spec(&w("batch/a"));
        assert_eq!(reap.exe, "resctl");
        assert_eq!(reap.args, ["--policy", "/p", "__reap", "batch/a"]);
        assert!(reap.detach);
    }

    #[test]
    fn only_a_failed_workload_makes_the_supervisor_exit_with_an_error() {
        for phase in Phase::ALL {
            let out = Outcome {
                phase,
                gen: 1,
                restarts: 0,
                reason: String::new(),
            };
            let want = if phase == Phase::Failed { 1 } else { 0 };
            assert_eq!(out.code(), want, "{}", phase.name());
        }
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Real processes: the only tests here that start any. They use the shell of the image.

    fn sh(script: &str) -> RunSpec {
        RunSpec {
            exe: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            detach: false,
        }
    }

    fn wait_real(p: &RealProcs, pid: i32) -> Exit {
        for _ in 0..500 {
            match p.try_wait(pid) {
                Ok(Some(e)) => return e,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(e) => panic!("cannot wait for {pid}: {e}"),
            }
        }
        panic!("process {pid} did not end");
    }

    /// The process group of a process, from `/proc` (the fields after the command name: state, parent, group).
    fn pgrp_of(pid: i32) -> i32 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_else(|e| panic!("cannot read the stat of {pid}: {e}"));
        let rest = stat.rsplit(')').next().unwrap_or("");
        let fields: Vec<&str> = rest.split_whitespace().collect();
        fields
            .get(2)
            .and_then(|f| f.parse().ok())
            .unwrap_or_else(|| panic!("no process group in: {stat}"))
    }

    #[test]
    fn real_children_report_their_exit_code_and_their_signal() {
        let p = RealProcs::default();
        let pid = p.spawn(&sh("exit 3")).unwrap();
        assert_eq!(wait_real(&p, pid), Exit::Code(3));
        // Collected once: a second look is an error, not a second exit.
        let again = p.try_wait(pid).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::NotFound);
        let pid = p.spawn(&sh("kill -9 $$")).unwrap();
        assert_eq!(wait_real(&p, pid), Exit::Signal(9));
    }

    #[test]
    fn a_real_child_runs_in_a_process_group_of_its_own_and_can_be_killed() {
        let p = RealProcs::default();
        let pid = p.spawn(&sh("exec sleep 20")).unwrap();
        assert!(matches!(p.try_wait(pid), Ok(None)));
        assert_eq!(pgrp_of(pid), pid);
        p.kill(pid).unwrap();
        assert_eq!(wait_real(&p, pid), Exit::Signal(9));
        // It has been collected: there is nothing left to kill.
        p.kill(pid).unwrap();
    }

    #[test]
    fn a_process_that_cannot_start_or_is_not_a_child_is_an_error() {
        let p = RealProcs::default();
        let missing = RunSpec {
            exe: "/nonexistent/resctl-test".into(),
            args: vec![],
            detach: false,
        };
        assert!(p.spawn(&missing).is_err());
        let r = p.try_wait(4_000_000);
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::NotFound);
        // A process that is not a child of this supervisor is never signalled.
        p.kill(4_000_000).unwrap();
    }

    #[test]
    fn a_detached_child_is_not_kept_by_the_supervisor() {
        let p = RealProcs::default();
        let spec = RunSpec {
            detach: true,
            ..sh("exit 0")
        };
        let pid = p.spawn(&spec).unwrap();
        assert!(pid > 0);
        let r = p.try_wait(pid);
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
