//! What an operator (or a workload) asks of the controller, the lifecycle records and the registry of critical
//! processes together: the confinement guard, protection, and the end of a workload that is supervised or not.

use std::io;

use crate::domain::{
    confined, io_err, Controller, Error, Placement, Result, Stopped, Target, Workload,
};
use crate::fs::Host;
use crate::life::{tidy, Intent, Phase};
use crate::policy::valid_name;
use crate::store::{Audit, Event, Store, Stored, Sup};
use crate::supervise::{self, failed_err, flush_rescues, Ctx, Terminated};

/// The name under which this process writes to the audit trail: its process id and the part it plays (`cli`,
/// `super`, `main` or `reap`).
pub fn actor(role: &str) -> String {
    format!("{}:{role}", std::process::id())
}

/// Refuses a command that changes anything when it is asked from inside a workload: a workload never controls
/// workloads, and never asks for protected status. A process whose place cannot be read is refused as well, since
/// the guard must not open for a process it cannot place. The refusal is written to the audit trail when there is
/// one to write to (the guard never creates a state directory of its own).
pub fn guard(host: &dyn Host, dir: &str, actor: &str, what: &str) -> Result<()> {
    match confined(host) {
        Ok(None) => Ok(()),
        Ok(Some(w)) => {
            if let Ok(Some(s)) = Store::existing(host, dir, actor) {
                note(&s, Event::Refuse, what, &format!("from workload {w}"));
            }
            Err(Error::Refused(format!(
                "{what} is refused inside the workload {w}: a workload does not control workloads or protected processes"
            )))
        }
        Err(e) => Err(io_err(
            "cannot tell whether this process runs inside a workload",
            &e,
        )),
    }
}

/// Writes a line to the audit trail when it can; a trail that cannot be written is reported as a warning, and the
/// command goes on. Never called while the store is locked.
pub fn note(s: &Store<'_>, event: Event, subject: &str, detail: &str) {
    match s.lock() {
        Ok(l) => {
            if let Err(e) = l.audit(Audit::Optional, event, subject, detail) {
                s.warn_out(format!("{subject}: not audited: {e}"));
            }
        }
        Err(e) => s.warn_out(format!("{subject}: not audited: {e}")),
    }
}

/// Writes a line to the audit trail, or fails: for what must not happen without a record of it. Never called while
/// the store is locked.
pub fn require(s: &Store<'_>, event: Event, subject: &str, detail: &str) -> Result<()> {
    s.lock()?
        .audit(Audit::Required, event, subject, detail)
        .map(|_| ())
}

/// Records that a command was refused, with the reason in one line.
pub fn refused(s: &Store<'_>, command: &str, why: &Error) {
    note(s, Event::Refuse, command, &tidy(&why.to_string()));
}

/// Records the outcome of a command that has run (how it went, or why it failed) and hands the result back.
pub fn done<T>(
    s: &Store<'_>,
    event: Event,
    subject: &str,
    how: &str,
    result: Result<T>,
) -> Result<T> {
    let detail = match (&result, how.is_empty()) {
        (Ok(_), true) => "ok".to_string(),
        (Ok(_), false) => how.to_string(),
        (Err(e), true) => format!("failed: {}", tidy(&e.to_string())),
        (Err(e), false) => format!("{how} failed: {}", tidy(&e.to_string())),
    };
    note(s, event, subject, &detail);
    result
}

/// How a stop ended, the way the command prints it.
pub fn stopped_text(st: &Stopped) -> String {
    match st {
        Stopped::Empty => "empty".to_string(),
        Stopped::Graceful => "graceful".to_string(),
        Stopped::Killed(n) => format!("killed:{n}"),
    }
}

fn event_of(i: Intent) -> Event {
    match i {
        Intent::Stop => Event::Stop,
        Intent::Kill => Event::Kill,
    }
}

/// Whether a workload id is free to be created or supervised: no supervisor is starting it, and no lifecycle record
/// says it is running, failed or unreadable (a failed or unreadable record is cleared with `reset`).
pub fn vacant(s: &Store<'_>, w: &Workload) -> Result<()> {
    match s.peek(w)? {
        Stored::Missing => match s.sup_state(w)? {
            Sup::Alive => Err(Error::Lifecycle(format!(
                "{w} is being started by its supervisor"
            ))),
            Sup::Lost | Sup::Absent => Ok(()),
        },
        Stored::Record(r) if r.phase == Phase::Failed => Err(failed_err(w, &r)),
        Stored::Record(r) if !r.phase.is_terminal() => Err(Error::Lifecycle(format!(
            "{w} is supervised already (phase {}): stop it first",
            r.phase.name()
        ))),
        Stored::Record(_) => Ok(()),
        Stored::Corrupt(why) => Err(Error::Lifecycle(format!(
            "{w} has an unreadable lifecycle record ({}): `resctl reset {w}` first",
            tidy(&why)
        ))),
    }
}

/// What `protect` did.
#[derive(Debug, PartialEq, Eq)]
pub struct Protected {
    pub pid: i32,
    pub name: String,
    /// Whether the process had to be moved into `system` (it was elsewhere).
    pub moved: bool,
}

/// Makes a process critical: it is placed in `system`, outside every workload kill domain, and registered, so that
/// no termination of a workload touches it. A process that runs inside a workload is refused, and so is one that
/// cannot be placed in `system`; nothing is registered unless the process is where it is protected.
pub fn protect(
    c: &Controller<'_>,
    s: &Store<'_>,
    name: Option<&str>,
    pid: i32,
) -> Result<Protected> {
    if pid < 2 {
        return Err(Error::Usage(format!(
            "{pid} is not a process that can be protected"
        )));
    }
    let name = name.map_or_else(|| format!("pid{pid}"), str::to_string);
    if !valid_name(&name) {
        return Err(Error::Usage(format!(
            "`{}` is not a usable name for a protected process (letters, digits, - and _; at most 32)",
            tidy(&name)
        )));
    }
    supervise::prelude(c, s)?;
    let moved = match c.placement(pid) {
        Ok(Placement::System) => false,
        Ok(Placement::Elsewhere) => {
            c.attach(&Target::System, pid)?;
            true
        }
        Ok(p @ Placement::Workload(_)) => {
            return Err(Error::Refused(format!(
                "process {pid} runs in the workload {}: a workload process is never protected",
                p.workload().unwrap_or_default()
            )))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Error::Refused(format!("process {pid} does not exist")))
        }
        Err(e) => return Err(io_err(&format!("cannot tell where process {pid} runs"), &e)),
    };
    if moved && !matches!(c.placement(pid), Ok(Placement::System)) {
        return Err(Error::Refused(format!(
            "process {pid} is not inside system after it was moved: it is not protected"
        )));
    }
    s.lock()?.protect(pid, &name)?;
    Ok(Protected { pid, name, moved })
}

/// Withdraws the protection of a process; whether it was registered. The process stays where it is.
pub fn unprotect(s: &Store<'_>, pid: i32) -> Result<bool> {
    s.lock()?.unprotect(pid)
}

/// One line per registered critical process, by pid: its name, whether it still runs, where, and when it started.
/// A registry that cannot be read is reported as one line.
pub fn critical_lines(c: &Controller<'_>, s: &Store<'_>) -> Result<Vec<String>> {
    if let Some(why) = s.registry_corrupt()? {
        return Ok(vec![format!("registry corrupt={}", tidy(&why))]);
    }
    let all = s.critical()?;
    let mut rows: Vec<(i32, String)> = Vec::new();
    for e in &all.live {
        let placed = match c.placement(e.pid) {
            Ok(Placement::System) => "system".to_string(),
            Ok(Placement::Elsewhere) => "elsewhere".to_string(),
            Ok(p) => format!("workload:{}", p.workload().unwrap_or_default()),
            Err(_) => "unknown".to_string(),
        };
        rows.push((
            e.pid,
            format!(
                "{} pid={} state=live placed={placed} start={}",
                e.name, e.pid, e.start
            ),
        ));
    }
    for e in &all.stale {
        rows.push((
            e.pid,
            format!(
                "{} pid={} state=stale placed=- start={}",
                e.name, e.pid, e.start
            ),
        ));
    }
    rows.sort();
    Ok(rows.into_iter().map(|(_, text)| text).collect())
}

/// How a workload was ended on request.
#[derive(Debug, PartialEq, Eq)]
pub struct Ended {
    /// How the stop went (`empty`, `graceful`, `killed:N`); none for a kill of a workload nothing supervises.
    pub stop: Option<String>,
    /// The phase of the lifecycle record, for a workload that has one.
    pub phase: Option<Phase>,
}

/// Records why a command did not succeed and hands the result back: a request that the lifecycle or the protection
/// turns down is a refusal, anything else that goes wrong is a failure of the command on `subject`. A mistake in the
/// command itself is for its sender only, and is not recorded. Never called while the store is locked.
pub fn failed<T>(s: &Store<'_>, event: Event, subject: &str, result: Result<T>) -> Result<T> {
    if let Err(e) = &result {
        match e {
            Error::Usage(_) => {}
            Error::Refused(_) | Error::Lifecycle(_) => refused(s, event.name(), e),
            _ => note(
                s,
                event,
                subject,
                &format!("failed: {}", tidy(&e.to_string())),
            ),
        }
    }
    result
}

/// Asks that a workload be ended (see `supervise::terminate`) for the command `event`, and records the request when it
/// is refused or fails.
fn ask(ctx: Ctx<'_>, w: &Workload, intent: Intent, event: Event) -> Result<Terminated> {
    failed(
        ctx.s,
        event,
        &w.to_string(),
        supervise::terminate(ctx, w, intent),
    )
}

/// Ends a workload on request. A supervised workload is asked through its record (its supervisor, or the sweep of a
/// supervisor that is gone, does the rest); a workload nothing supervises, or what a failed one left behind, is
/// ended by hand. A grace period is only meaningful for the second kind: the supervisor uses the one of the policy.
pub fn end(ctx: Ctx<'_>, w: &Workload, intent: Intent, grace: Option<u64>) -> Result<Ended> {
    if intent == Intent::Stop
        && grace.is_some()
        && matches!(ctx.s.peek(w)?, Stored::Record(r) if !r.phase.is_terminal())
    {
        return Err(Error::Usage(format!(
            "{w} is supervised: it is stopped with the grace period of the policy, not with --grace"
        )));
    }
    match ask(ctx, w, intent, event_of(intent))? {
        Terminated::Unmanaged => by_hand(ctx, w, intent, grace, None),
        Terminated::Already(p) => {
            if ctx.c.exists(w) {
                by_hand(ctx, w, intent, grace, Some(p))
            } else {
                note(
                    ctx.s,
                    event_of(intent),
                    &w.to_string(),
                    &format!("already phase={}", p.name()),
                );
                Ok(Ended {
                    stop: Some("empty".to_string()),
                    phase: Some(p),
                })
            }
        }
        Terminated::Supervised(p) | Terminated::Swept(p) => {
            let rec = match ctx.s.peek(w) {
                Ok(Stored::Record(r)) => Some(r),
                _ => None,
            };
            if p == Phase::Failed {
                let why = rec.map(|r| tidy(&r.reason)).unwrap_or_default();
                return Err(Error::Io(format!("{w} ended failed: {why}")));
            }
            let stop = rec
                .and_then(|r| r.stop)
                .unwrap_or_else(|| "empty".to_string());
            Ok(Ended {
                stop: Some(stop),
                phase: Some(p),
            })
        }
    }
}

fn by_hand(
    ctx: Ctx<'_>,
    w: &Workload,
    intent: Intent,
    grace: Option<u64>,
    phase: Option<Phase>,
) -> Result<Ended> {
    let result = match intent {
        Intent::Stop => ctx
            .c
            .stop(w, grace.unwrap_or(ctx.c.policy().stop_grace_ms))
            .map(|st| Some(stopped_text(&st))),
        Intent::Kill => ctx.c.kill(w).map(|()| None),
    };
    flush_rescues(ctx.c, ctx.s);
    let mut how = match phase {
        None => "unmanaged".to_string(),
        Some(p) => format!("leftover phase={}", p.name()),
    };
    if let Ok(Some(text)) = &result {
        how.push_str(" stop=");
        how.push_str(text);
    }
    done(ctx.s, event_of(intent), &w.to_string(), &how, result).map(|stop| Ended { stop, phase })
}

/// Removes a workload: what runs in it is killed first, a supervisor is told to stop, and the domain goes. The
/// lifecycle record, which is the history of the workload, stays until `reset` or `teardown`. The phase of the
/// record, for a workload that has one.
pub fn remove(ctx: Ctx<'_>, w: &Workload) -> Result<Option<Phase>> {
    let phase = match ask(ctx, w, Intent::Kill, Event::Remove)? {
        Terminated::Unmanaged => None,
        Terminated::Already(p) | Terminated::Supervised(p) | Terminated::Swept(p) => Some(p),
    };
    let result = if phase.is_some() && !ctx.c.exists(w) {
        Ok(())
    } else {
        let r = ctx.c.remove(w);
        flush_rescues(ctx.c, ctx.s);
        match r {
            Err(_) if phase.is_some() && !ctx.c.exists(w) => Ok(()),
            r => r,
        }
    };
    let how = phase.map_or_else(
        || "unmanaged".to_string(),
        |p| format!("phase={}", p.name()),
    );
    done(ctx.s, Event::Remove, &w.to_string(), &how, result).map(|()| phase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::fake::{FakeHost, EACCES, ENOENT};
    use crate::layout::{Layout, Lifecycle};
    use crate::life::{Ending, Exit, Record};
    use crate::policy::{Limits, Orphans, Policy};
    use crate::store::Line;
    use crate::supervise::{Env, Procs, RunSpec};

    const GIB: u64 = 1 << 30;
    const DIR: &str = "/run/ai";
    const SYSDOM: &str = "/c/agent-interaction/system";
    const POLICY: &str = concat!(
        "[system]\ncpu.weight = 1000\nmemory.reserve = 64M\n",
        "[workload]\nmemory.max = 512M\npids.max = 256\n",
        "[class.batch]\ncpu.weight = 100\nmemory.max = 160M\npids.max = 96\n",
        "[limits.batch]\nmemory.max = 64M\npids.max = 16\n",
        "[stop]\ngrace = 1500ms\n",
        "[restart]\nmode = on-failure\nmax = 3\nwindow = 60s\ntotal = 10\nbackoff = 10ms\nbackoff.max = 40ms\nrate = 30\n",
        "[supervise]\nworkloads = 2\n",
        "[run]\nallow.root = yes\n",
    );

    fn me() -> i32 {
        i32::try_from(std::process::id()).unwrap()
    }

    fn world() -> FakeHost {
        let h = FakeHost::new();
        h.add_v2("/c", &["cpuset", "cpu", "io", "memory", "pids"]);
        h.set_clock(5_000);
        h.proc_in(me(), &[]);
        h
    }

    /// An operator starts and reaps nothing: only a supervisor does.
    struct NoProcs;

    impl Procs for NoProcs {
        fn spawn(&self, _spec: &RunSpec) -> io::Result<i32> {
            Err(io::Error::other("an operator starts nothing"))
        }
        fn try_wait(&self, _pid: i32) -> io::Result<Option<Exit>> {
            Err(io::Error::other("an operator reaps nothing"))
        }
        fn kill(&self, _pid: i32) -> io::Result<()> {
            Ok(())
        }
    }

    macro_rules! rig {
        ($h:ident, $c:ident, $s:ident, $ctx:ident) => {
            let $h = world();
            let layout = Layout::detect(&$h, Lifecycle::Auto);
            let $s = Store::open(&$h, DIR, "4242:test").unwrap();
            let shield = || $s.critical_pids();
            let pol = Policy::parse(POLICY).unwrap();
            let $c = Controller::new(&$h, &layout, &pol, 4 * GIB)
                .unwrap()
                .with_critical(&shield);
            $c.init().unwrap();
            let env = Env {
                exe: "resctl".into(),
                globals: vec![],
            };
            let procs = NoProcs;
            let $ctx = Ctx {
                host: &$h,
                c: &$c,
                s: &$s,
                p: &procs,
                env: &env,
            };
            let _ = (&$h, &$c, &$s, &$ctx);
        };
    }

    fn w(s: &str) -> Workload {
        Workload::parse(s).unwrap()
    }

    fn dom(id: &str) -> String {
        format!("/c/agent-interaction/workload/batch/{id}")
    }

    fn trail(s: &Store<'_>) -> Vec<Line> {
        s.audit_tail(5000)
            .unwrap()
            .iter()
            .map(|l| Line::parse(l).unwrap_or_else(|| panic!("not a line of the trail: {l}")))
            .collect()
    }

    fn lines_of(s: &Store<'_>, event: Event) -> Vec<Line> {
        trail(s).into_iter().filter(|l| l.event == event).collect()
    }

    fn record(s: &Store<'_>, id: &str) -> Record {
        match s.peek(&w(&format!("batch/{id}"))).unwrap() {
            Stored::Record(r) => r,
            _ => panic!("no record"),
        }
    }

    /// A supervised workload whose supervisor is gone: a record in phase running, a domain, and its processes.
    fn orphan(h: &FakeHost, c: &Controller<'_>, s: &Store<'_>, id: &str, pids: &[i32]) {
        let wl = w(&format!("batch/{id}"));
        c.create(&wl, &Limits::default()).unwrap();
        let d = dom(id);
        for &pid in pids {
            h.proc_in(pid, &[d.as_str()]);
        }
        let lock = s.lock().unwrap();
        let _guard = lock.claim(&wl).unwrap().unwrap();
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
    }

    #[test]
    fn a_process_outside_every_workload_passes_the_guard() {
        rig!(h, c, s, ctx);
        assert!(guard(&h, DIR, "4242:cli", "kill").is_ok());
        assert!(lines_of(&s, Event::Refuse).is_empty());
    }

    #[test]
    fn a_process_inside_a_workload_is_refused_and_the_refusal_is_audited() {
        rig!(h, c, s, ctx);
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.proc_in(4242, &[dom("a").as_str()]);
        let e = guard(&h, DIR, "4242:cli", "kill").unwrap_err();
        assert!(
            matches!(&e, Error::Refused(m) if m.contains("batch/a")),
            "{e}"
        );
        assert_eq!(e.code(), 2);
        let refusals = lines_of(&s, Event::Refuse);
        assert_eq!(refusals.len(), 1);
        assert_eq!(refusals[0].subject, "kill");
        assert_eq!(refusals[0].detail, "from workload batch/a");
        assert_eq!(refusals[0].actor, "4242:cli");
    }

    #[test]
    fn a_guard_that_cannot_tell_where_it_runs_fails_closed() {
        for (errno, code) in [(EACCES, 3), (ENOENT, 3), (5, 1)] {
            rig!(h, c, s, ctx);
            h.fail_read_of("/proc/self/cgroup", errno);
            let e = guard(&h, DIR, "4242:cli", "kill").unwrap_err();
            assert_eq!(e.code(), code, "errno {errno}: {e}");
            assert!(lines_of(&s, Event::Refuse).is_empty());
        }
    }

    #[test]
    fn a_refusal_creates_no_state_directory() {
        let h = world();
        h.proc_in(4242, &["/c/agent-interaction/workload/batch/a"]);
        let e = guard(&h, DIR, "4242:cli", "create").unwrap_err();
        assert_eq!(e.code(), 2, "{e}");
        assert!(!h.dir_exists(DIR));
    }

    #[test]
    fn protect_moves_a_process_into_system_and_registers_it() {
        rig!(h, c, s, ctx);
        h.proc_in(900, &[]);
        let p = protect(&c, &s, Some("db"), 900).unwrap();
        assert_eq!(
            p,
            Protected {
                pid: 900,
                name: "db".into(),
                moved: true
            }
        );
        assert_eq!(h.procs_of(SYSDOM), vec![900]);
        let live = s.critical().unwrap().live;
        assert_eq!(
            live.iter()
                .map(|e| (e.pid, e.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(900, "db")]
        );
        let lines = lines_of(&s, Event::Protect);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].subject, "db");
        assert_eq!(lines[0].detail, "pid=900");
    }

    #[test]
    fn protect_registers_a_process_already_in_system_without_moving_it() {
        rig!(h, c, s, ctx);
        h.proc_in(901, &[SYSDOM]);
        let p = protect(&c, &s, None, 901).unwrap();
        assert_eq!(
            p,
            Protected {
                pid: 901,
                name: "pid901".into(),
                moved: false
            }
        );
        assert!(!h
            .writes_to("/system/cgroup.procs")
            .iter()
            .any(|d| d == "901"));
        assert_eq!(s.critical().unwrap().live.len(), 1);
    }

    #[test]
    fn protect_refuses_a_process_that_runs_inside_a_workload() {
        rig!(h, c, s, ctx);
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.proc_in(902, &[dom("a").as_str()]);
        let e = protect(&c, &s, Some("sneak"), 902).unwrap_err();
        assert!(
            matches!(&e, Error::Refused(m) if m.contains("batch/a")),
            "{e}"
        );
        assert_eq!(e.code(), 2);
        assert!(s.critical().unwrap().live.is_empty());
        assert_eq!(h.procs_of(&dom("a")), vec![902]);
        assert!(lines_of(&s, Event::Protect).is_empty());
    }

    #[test]
    fn protect_refuses_what_cannot_be_protected_before_moving_anything() {
        rig!(h, c, s, ctx);
        h.proc_in(903, &[]);
        let long = "x".repeat(33);
        for name in ["has space", "", "-lead", long.as_str()] {
            let e = protect(&c, &s, Some(name), 903).unwrap_err();
            assert!(matches!(&e, Error::Usage(_)), "{name}: {e}");
            assert_eq!(e.code(), 2);
        }
        for pid in [1, 0, -5] {
            let e = protect(&c, &s, None, pid).unwrap_err();
            assert!(matches!(&e, Error::Usage(_)), "{pid}: {e}");
        }
        let e = protect(&c, &s, None, 99_999).unwrap_err();
        assert!(
            matches!(&e, Error::Refused(m) if m.contains("does not exist")),
            "{e}"
        );
        assert!(h.procs_of(SYSDOM).is_empty());
        assert!(s.critical().unwrap().live.is_empty());
    }

    #[test]
    fn protect_fails_when_the_process_cannot_be_moved_and_registers_nothing() {
        rig!(h, c, s, ctx);
        h.proc_in(904, &[]);
        h.deny_writes_to("/system/cgroup.procs");
        let e = protect(&c, &s, Some("db"), 904).unwrap_err();
        assert_eq!(e.code(), 3, "{e}");
        assert!(s.critical().unwrap().live.is_empty());
        assert!(lines_of(&s, Event::Protect).is_empty());
        assert!(h.procs_of(SYSDOM).is_empty());
    }

    #[test]
    fn protect_heals_a_corrupt_registry_first() {
        rig!(h, c, s, ctx);
        h.set(&format!("{DIR}/critical"), "garbage");
        assert!(s.registry_corrupt().unwrap().is_some());
        h.proc_in(905, &[]);
        protect(&c, &s, Some("db"), 905).unwrap();
        assert!(s.registry_corrupt().unwrap().is_none());
        assert!(h
            .files_of(&format!("{DIR}/quarantine"))
            .iter()
            .any(|n| n.starts_with("critical.")));
        let pids: Vec<i32> = s.critical().unwrap().live.iter().map(|e| e.pid).collect();
        assert_eq!(pids, vec![905]);
    }

    #[test]
    fn unprotect_unregisters_once_and_leaves_the_process_where_it_is() {
        rig!(h, c, s, ctx);
        h.proc_in(950, &[SYSDOM]);
        s.lock().unwrap().protect(950, "svc").unwrap();
        assert!(unprotect(&s, 950).unwrap());
        assert!(!unprotect(&s, 950).unwrap());
        assert!(s.critical().unwrap().live.is_empty());
        assert_eq!(h.procs_of(SYSDOM), vec![950]);
        let lines = lines_of(&s, Event::Unprotect);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].subject, "svc");
        assert_eq!(lines[0].detail, "pid=950");
    }

    #[test]
    fn critical_lines_say_where_each_registered_process_runs() {
        rig!(h, c, s, ctx);
        h.proc_in(960, &[SYSDOM]);
        h.proc_in(961, &[SYSDOM]);
        {
            let lock = s.lock().unwrap();
            lock.protect(960, "db").unwrap();
            lock.protect(961, "gone").unwrap();
        }
        assert_eq!(
            critical_lines(&c, &s).unwrap(),
            vec![
                "db pid=960 state=live placed=system start=1960",
                "gone pid=961 state=live placed=system start=1961",
            ]
        );
        h.exit_proc(961);
        assert_eq!(
            critical_lines(&c, &s).unwrap(),
            vec![
                "db pid=960 state=live placed=system start=1960",
                "gone pid=961 state=stale placed=- start=1961",
            ]
        );
        c.create(&w("batch/a"), &Limits::default()).unwrap();
        h.proc_in(960, &[dom("a").as_str()]);
        assert_eq!(
            critical_lines(&c, &s).unwrap()[0],
            "db pid=960 state=live placed=workload:batch/a start=1960"
        );
        h.set(&format!("{DIR}/critical"), "garbage");
        let lines = critical_lines(&c, &s).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("registry corrupt="), "{lines:?}");
    }

    #[test]
    fn an_unmanaged_workload_is_killed_by_hand_and_audited() {
        rig!(h, c, s, ctx);
        let wl = w("batch/m");
        c.create(&wl, &Limits::default()).unwrap();
        h.proc_in(700, &[dom("m").as_str()]);
        let ended = end(ctx, &wl, Intent::Kill, None).unwrap();
        assert_eq!(
            ended,
            Ended {
                stop: None,
                phase: None
            }
        );
        assert!(!h.alive(700));
        let kills = lines_of(&s, Event::Kill);
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0].subject, "batch/m");
        assert_eq!(kills[0].detail, "unmanaged");
    }

    #[test]
    fn an_unmanaged_workload_is_stopped_gracefully_or_killed_after_the_grace_period() {
        rig!(h, c, s, ctx);
        let wl = w("batch/m");
        c.create(&wl, &Limits::default()).unwrap();
        let ended = end(ctx, &wl, Intent::Stop, None).unwrap();
        assert_eq!(
            ended,
            Ended {
                stop: Some("empty".into()),
                phase: None
            }
        );
        h.proc_in(701, &[dom("m").as_str()]);
        h.ignore_term(701);
        let ended = end(ctx, &wl, Intent::Stop, Some(100)).unwrap();
        assert_eq!(ended.stop.as_deref(), Some("killed:1"));
        assert!(!h.alive(701));
        let details: Vec<String> = lines_of(&s, Event::Stop)
            .into_iter()
            .map(|l| l.detail)
            .collect();
        assert_eq!(
            details,
            vec!["unmanaged stop=empty", "unmanaged stop=killed:1"]
        );
    }

    #[test]
    fn ending_a_workload_that_never_existed_fails_and_the_failure_is_audited() {
        rig!(h, c, s, ctx);
        let e = end(ctx, &w("batch/nope"), Intent::Kill, None).unwrap_err();
        assert_eq!(e.code(), 2, "{e}");
        let kills = lines_of(&s, Event::Kill);
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0].subject, "batch/nope");
        assert!(
            kills[0].detail.starts_with("unmanaged failed:"),
            "{}",
            kills[0].detail
        );
    }

    #[test]
    fn an_end_the_lifecycle_turns_down_is_recorded_as_a_refusal() {
        rig!(h, c, s, ctx);
        let starting = w("batch/s");
        let held = {
            let lock = s.lock().unwrap();
            lock.claim(&starting).unwrap().unwrap()
        };
        let e = end(ctx, &starting, Intent::Stop, None).unwrap_err();
        assert!(
            matches!(&e, Error::Lifecycle(m) if m.contains("try again")),
            "{e}"
        );
        assert_eq!(e.code(), 2);
        let e = remove(ctx, &starting).unwrap_err();
        assert_eq!(e.code(), 2, "{e}");
        let refusals = lines_of(&s, Event::Refuse);
        let subjects: Vec<&str> = refusals.iter().map(|l| l.subject.as_str()).collect();
        assert_eq!(subjects, vec!["stop", "remove"]);
        assert!(
            refusals.iter().all(|l| l.detail.contains("batch/s")),
            "{refusals:?}"
        );
        assert!(lines_of(&s, Event::Stop).is_empty());
        assert!(lines_of(&s, Event::Remove).is_empty());
        drop(held);
        assert!(end(ctx, &starting, Intent::Stop, None).is_err());
        assert_eq!(lines_of(&s, Event::Refuse).len(), 2);
    }

    #[test]
    fn a_supervised_workload_is_stopped_through_its_record_with_the_grace_of_the_policy() {
        rig!(h, c, s, ctx);
        orphan(&h, &c, &s, "a", &[800, 801]);
        let wl = w("batch/a");
        let e = end(ctx, &wl, Intent::Stop, Some(5_000)).unwrap_err();
        assert!(
            matches!(&e, Error::Usage(m) if m.contains("supervised")),
            "{e}"
        );
        assert!(h.alive(800) && h.alive(801));
        let ended = end(ctx, &wl, Intent::Stop, None).unwrap();
        assert_eq!(
            ended,
            Ended {
                stop: Some("graceful".into()),
                phase: Some(Phase::Stopped)
            }
        );
        let rec = record(&s, "a");
        assert_eq!(rec.phase, Phase::Stopped);
        assert_eq!(rec.ended, Some(Ending::Requested(Intent::Stop)));
        assert!(!c.exists(&wl));
        assert!(!h.alive(800) && !h.alive(801));
        let again = end(ctx, &wl, Intent::Stop, None).unwrap();
        assert_eq!(
            again,
            Ended {
                stop: Some("empty".into()),
                phase: Some(Phase::Stopped)
            }
        );
        assert!(lines_of(&s, Event::Stop)
            .iter()
            .any(|l| l.detail == "already phase=stopped"));
    }

    #[test]
    fn what_a_failed_workload_left_behind_is_ended_by_hand_and_the_record_stays_failed() {
        rig!(h, c, s, ctx);
        orphan(&h, &c, &s, "a", &[810, 811]);
        let wl = w("batch/a");
        let rec = supervise::recover(ctx, Orphans::Keep).unwrap();
        assert_eq!(rec.swept, vec![("batch/a".to_string(), Phase::Failed)]);
        assert!(c.exists(&wl));
        assert!(h.alive(810));
        let ended = end(ctx, &wl, Intent::Kill, None).unwrap();
        assert_eq!(
            ended,
            Ended {
                stop: None,
                phase: Some(Phase::Failed)
            }
        );
        assert!(!h.alive(810) && !h.alive(811));
        assert!(lines_of(&s, Event::Kill)
            .iter()
            .any(|l| l.detail.contains("leftover phase=failed")));
        assert_eq!(record(&s, "a").phase, Phase::Failed);
    }

    #[test]
    fn remove_ends_what_runs_and_takes_the_domain_away() {
        rig!(h, c, s, ctx);
        let wl = w("batch/m");
        c.create(&wl, &Limits::default()).unwrap();
        h.proc_in(720, &[dom("m").as_str()]);
        assert_eq!(remove(ctx, &wl).unwrap(), None);
        assert!(!c.exists(&wl));
        assert!(!h.alive(720));
        let lines = lines_of(&s, Event::Remove);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].detail, "unmanaged");

        orphan(&h, &c, &s, "a", &[730]);
        assert_eq!(remove(ctx, &w("batch/a")).unwrap(), Some(Phase::Stopped));
        assert_eq!(record(&s, "a").phase, Phase::Stopped);
        assert!(!c.exists(&w("batch/a")));
        assert!(!h.alive(730));

        orphan(&h, &c, &s, "b", &[740]);
        supervise::recover(ctx, Orphans::Keep).unwrap();
        assert!(h.alive(740));
        assert_eq!(remove(ctx, &w("batch/b")).unwrap(), Some(Phase::Failed));
        assert_eq!(record(&s, "b").phase, Phase::Failed);
        assert!(!c.exists(&w("batch/b")));
        assert!(!h.alive(740));
    }

    #[test]
    fn removing_a_workload_that_never_existed_fails_and_the_failure_is_audited() {
        rig!(h, c, s, ctx);
        let e = remove(ctx, &w("batch/nope")).unwrap_err();
        assert_eq!(e.code(), 2, "{e}");
        let lines = lines_of(&s, Event::Remove);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].subject, "batch/nope");
        assert!(lines[0].detail.contains("failed:"), "{}", lines[0].detail);
    }

    #[test]
    fn a_workload_id_is_vacant_unless_a_record_or_a_supervisor_holds_it() {
        rig!(h, c, s, ctx);
        let free = w("batch/f");
        vacant(&s, &free).unwrap();

        let starting = w("batch/s");
        let held = {
            let lock = s.lock().unwrap();
            lock.claim(&starting).unwrap().unwrap()
        };
        let e = vacant(&s, &starting).unwrap_err();
        assert!(
            matches!(&e, Error::Lifecycle(m) if m.contains("being started")),
            "{e}"
        );
        drop(held);
        vacant(&s, &starting).unwrap();

        orphan(&h, &c, &s, "b", &[]);
        let e = vacant(&s, &w("batch/b")).unwrap_err();
        assert!(
            matches!(&e, Error::Lifecycle(m) if m.contains("supervised already") && m.contains("phase running")),
            "{e}"
        );

        orphan(&h, &c, &s, "c", &[]);
        end(ctx, &w("batch/c"), Intent::Kill, None).unwrap();
        assert_eq!(record(&s, "c").phase, Phase::Stopped);
        vacant(&s, &w("batch/c")).unwrap();

        orphan(&h, &c, &s, "d", &[820]);
        supervise::recover(ctx, Orphans::Keep).unwrap();
        assert_eq!(record(&s, "d").phase, Phase::Failed);
        let e = vacant(&s, &w("batch/d")).unwrap_err();
        assert!(
            matches!(&e, Error::Lifecycle(m) if m.contains("resctl reset")),
            "{e}"
        );

        h.set(&format!("{DIR}/records/batch.k"), "garbage");
        let e = vacant(&s, &w("batch/k")).unwrap_err();
        assert!(
            matches!(&e, Error::Lifecycle(m) if m.contains("resctl reset")),
            "{e}"
        );
    }

    #[test]
    fn a_required_audit_fails_where_an_optional_one_only_warns() {
        rig!(h, c, s, ctx);
        h.deny_puts_to("audit.log");
        assert!(require(&s, Event::Run, "batch/x", "cmd=true").is_err());
        note(&s, Event::Run, "batch/x", "cmd=true");
        assert!(!s.take_warnings().is_empty());
        h.allow_puts();
        require(&s, Event::Run, "batch/x", "cmd=true").unwrap();
        assert_eq!(lines_of(&s, Event::Run).len(), 1);
    }

    #[test]
    fn the_outcome_of_a_command_is_audited_with_a_detail_that_is_never_empty() {
        rig!(h, c, s, ctx);
        assert!(done(&s, Event::Init, "all", "", Ok(())).is_ok());
        let failed: Result<()> = Err(Error::Usage("no such thing".into()));
        assert!(done(&s, Event::Remove, "batch/x", "unmanaged", failed).is_err());
        let failed: Result<()> = Err(Error::Usage("no such thing".into()));
        assert!(done(&s, Event::Create, "batch/y", "", failed).is_err());
        assert_eq!(lines_of(&s, Event::Init)[0].detail, "ok");
        assert_eq!(
            lines_of(&s, Event::Remove)[0].detail,
            "unmanaged failed: no such thing"
        );
        assert_eq!(
            lines_of(&s, Event::Create)[0].detail,
            "failed: no such thing"
        );
    }

    #[test]
    fn a_refusal_is_audited_with_its_reason() {
        rig!(h, c, s, ctx);
        refused(&s, "protect", &Error::Refused("it is not allowed".into()));
        let lines = lines_of(&s, Event::Refuse);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].subject, "protect");
        assert_eq!(lines[0].detail, "it is not allowed");
    }

    #[test]
    fn a_stop_is_spelled_the_way_the_command_prints_it() {
        assert_eq!(stopped_text(&Stopped::Empty), "empty");
        assert_eq!(stopped_text(&Stopped::Graceful), "graceful");
        assert_eq!(stopped_text(&Stopped::Killed(3)), "killed:3");
    }

    #[test]
    fn the_actor_names_the_process_and_its_role() {
        let a = actor("cli");
        let (pid, role) = a.split_once(':').unwrap();
        assert!(
            !pid.is_empty() && pid.chars().all(|ch| ch.is_ascii_digit()),
            "{a}"
        );
        assert_eq!(role, "cli");
    }
}
