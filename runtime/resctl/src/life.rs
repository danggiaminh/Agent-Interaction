//! The lifecycle of a supervised workload: its phases, the transitions that are allowed between them, how a run ended, and
//! what happens next. Nothing in here touches the host, so every rule is tested without one.
//!
//! ```text
//! created -> starting -> running -> stopping -> killing -> sweeping -> stopped    (an operator asked)
//!                            |          |          |          ^    -> finished   (it ended cleanly, no restart)
//!                            +----------+----------+----------+    -> failed     (it ended unexpectedly, no restart left)
//!                                                                   -> backoff -> starting   (restarted, within the limits)
//! any phase that is not final -> recovering -> stopped | failed      (its supervisor is gone; `recover` finishes the job)
//! ```
//!
//! `stopped` and `finished` are the ends of a run that may be started again; `failed` is quarantined until an operator
//! resets it, so a crash loop stops being restarted and cannot be restarted by accident.

use crate::policy::{valid_name, Restart, RestartMode};

/// Where a supervised workload is in its life. Every change goes through [`Record::advance`], which refuses the changes
/// that make no sense.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The record exists; nothing has been started.
    Created,
    /// The kill domain is being built and the workload launched.
    Starting,
    /// The main process is alive inside its kill domain.
    Running,
    /// An operator asked for a stop: the processes were asked to end and the grace period is running.
    Stopping,
    /// A forced termination is in progress.
    Killing,
    /// The main process is gone: what it left behind is killed, and the kill domain removed.
    Sweeping,
    /// Waiting before an automatic restart.
    Backoff,
    /// `recover` is cleaning up after a supervisor that is gone.
    Recovering,
    /// Final: ended because an operator asked.
    Stopped,
    /// Final: ended by itself, cleanly, and is not restarted.
    Finished,
    /// Final: ended unexpectedly and will not be restarted; stays this way until `reset`.
    Failed,
}

impl Phase {
    pub const ALL: [Phase; 11] = [
        Phase::Created,
        Phase::Starting,
        Phase::Running,
        Phase::Stopping,
        Phase::Killing,
        Phase::Sweeping,
        Phase::Backoff,
        Phase::Recovering,
        Phase::Stopped,
        Phase::Finished,
        Phase::Failed,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Phase::Created => "created",
            Phase::Starting => "starting",
            Phase::Running => "running",
            Phase::Stopping => "stopping",
            Phase::Killing => "killing",
            Phase::Sweeping => "sweeping",
            Phase::Backoff => "backoff",
            Phase::Recovering => "recovering",
            Phase::Stopped => "stopped",
            Phase::Finished => "finished",
            Phase::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Phase> {
        Phase::ALL.into_iter().find(|p| p.name() == s)
    }

    /// The ends of a run: no supervisor is working on the workload any more.
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Stopped | Phase::Finished | Phase::Failed)
    }

    /// The phases this one may go to.
    pub fn next(self) -> &'static [Phase] {
        use Phase::*;
        match self {
            Created => &[Starting, Recovering],
            Starting => &[Running, Sweeping, Recovering],
            Running => &[Stopping, Killing, Sweeping, Recovering],
            Stopping => &[Killing, Sweeping, Recovering],
            Killing => &[Sweeping, Recovering],
            Sweeping => &[Backoff, Stopped, Finished, Failed, Recovering],
            Backoff => &[Starting, Stopped, Failed, Recovering],
            Recovering => &[Stopped, Failed],
            Stopped | Finished => &[Starting],
            Failed => &[],
        }
    }

    pub fn can_go(self, to: Phase) -> bool {
        self.next().contains(&to)
    }
}

/// What an operator asked for. It is recorded (and audited) before any signal is sent, so an end that follows it is
/// known to be intentional. A stop can be escalated to a kill, never the other way round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Ask the processes to end, and kill what is left after the grace period.
    Stop,
    /// Kill the whole domain now.
    Kill,
}

impl Intent {
    pub fn name(self) -> &'static str {
        match self {
            Intent::Stop => "stop",
            Intent::Kill => "kill",
        }
    }

    pub fn parse(s: &str) -> Option<Intent> {
        match s {
            "stop" => Some(Intent::Stop),
            "kill" => Some(Intent::Kill),
            _ => None,
        }
    }
}

/// How the main process of a run ended, as the kernel reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl Exit {
    pub fn render(self) -> String {
        match self {
            Exit::Code(n) => format!("exit:{n}"),
            Exit::Signal(s) => format!("signal:{s}"),
        }
    }

    pub fn parse(s: &str) -> Option<Exit> {
        let (kind, n) = s.split_once(':')?;
        let n = n.parse::<i32>().ok()?;
        match kind {
            "exit" if (0..=255).contains(&n) => Some(Exit::Code(n)),
            "signal" if (1..=64).contains(&n) => Some(Exit::Signal(n)),
            _ => None,
        }
    }
}

/// Why a run ended: the distinction between an end that somebody asked for and one that nobody did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// Exit status 0, nobody asked.
    Clean,
    /// A non-zero exit status, nobody asked.
    Failed(i32),
    /// Killed by a signal that no operator sent (a crash, or the kernel's OOM killer).
    Crashed(i32),
    /// An operator asked for it; the raw status is kept next to it in the record.
    Requested(Intent),
    /// Its supervisor vanished; nobody saw how it ended.
    Lost,
    /// It never started.
    LaunchFailed,
}

impl Ending {
    pub fn render(self) -> String {
        match self {
            Ending::Clean => "clean".into(),
            Ending::Failed(n) => format!("failed:{n}"),
            Ending::Crashed(s) => format!("crashed:{s}"),
            Ending::Requested(i) => format!("requested:{}", i.name()),
            Ending::Lost => "lost".into(),
            Ending::LaunchFailed => "launch-failed".into(),
        }
    }

    pub fn parse(s: &str) -> Option<Ending> {
        match s {
            "clean" => return Some(Ending::Clean),
            "lost" => return Some(Ending::Lost),
            "launch-failed" => return Some(Ending::LaunchFailed),
            _ => {}
        }
        let (kind, v) = s.split_once(':')?;
        match kind {
            "failed" => v
                .parse::<i32>()
                .ok()
                .filter(|n| (1..=255).contains(n))
                .map(Ending::Failed),
            "crashed" => v
                .parse::<i32>()
                .ok()
                .filter(|n| (1..=64).contains(n))
                .map(Ending::Crashed),
            "requested" => Intent::parse(v).map(Ending::Requested),
            _ => None,
        }
    }

    /// An end that nobody asked for and that was not a clean exit.
    pub fn unexpected(self) -> bool {
        !matches!(self, Ending::Clean | Ending::Requested(_))
    }
}

/// Classify how a run ended. An operator's recorded intent decides: whatever the raw status is (SIGTERM after a stop, SIGKILL
/// after a kill), the end was asked for. Without an intent the status speaks for itself.
pub fn classify(exit: Exit, intent: Option<Intent>) -> Ending {
    match (intent, exit) {
        (Some(i), _) => Ending::Requested(i),
        (None, Exit::Code(0)) => Ending::Clean,
        (None, Exit::Code(n)) => Ending::Failed(n),
        (None, Exit::Signal(s)) => Ending::Crashed(s),
    }
}

/// What a supervisor remembers of the restarts it has made.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// When (milliseconds of uptime) the restarts of the current window were decided.
    pub recent: Vec<u64>,
    /// Restarts made since the supervisor started, however far apart.
    pub total: u32,
}

impl History {
    pub fn in_window(&self, window_ms: u64, now_ms: u64) -> u32 {
        self.recent
            .iter()
            .filter(|&&t| now_ms.saturating_sub(t) < window_ms)
            .count() as u32
    }

    /// Count a restart decided at `now_ms`; the restarts that left the window are forgotten.
    pub fn note(&mut self, now_ms: u64, window_ms: u64) {
        self.recent
            .retain(|&t| now_ms.saturating_sub(t) < window_ms);
        self.recent.push(now_ms);
        self.total = self.total.saturating_add(1);
    }
}

/// Why a workload is not restarted any more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// The restart mode is `never`.
    Never,
    /// `max` restarts were made within one `window`.
    Window { max: u32, window_ms: u64 },
    /// `total` restarts were made since the supervisor started.
    Total { total: u32 },
    /// Nobody saw how it ended, so nobody can say that it is safe to start it again.
    Lost,
}

fn span(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

impl Why {
    pub fn text(&self) -> String {
        match self {
            Why::Never => "the restart mode is never".into(),
            Why::Window { max, window_ms } => {
                format!("{max} restarts within {} are used up", span(*window_ms))
            }
            Why::Total { total } => {
                format!("{total} restarts since the supervisor started are used up")
            }
            Why::Lost => "the supervisor was lost and nobody saw how the workload ended".into(),
        }
    }
}

/// What happens after a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Start again after `delay_ms`.
    Restart { delay_ms: u64 },
    /// It ended by itself, cleanly: done.
    Finish,
    /// An operator asked for it: done.
    Stop,
    /// It is not restarted any more; it stays failed until an operator resets it.
    Fail(Why),
}

/// Decide what follows the end of a run. Pure, and the only place that decides whether anything is started again:
/// an end that was asked for is never restarted; every other end is restarted only while the mode allows it, fewer than
/// `max` restarts happened within the `window`, and fewer than `total` since the supervisor started. The delay doubles with
/// every restart of the window, up to `backoff.max`, so a crash loop slows down before it is stopped.
pub fn decide(r: &Restart, h: &History, ending: Ending, now_ms: u64) -> Verdict {
    match ending {
        Ending::Requested(_) => return Verdict::Stop,
        Ending::Lost => return Verdict::Fail(Why::Lost),
        Ending::Clean if r.mode != RestartMode::Always => return Verdict::Finish,
        _ => {}
    }
    if r.mode == RestartMode::Never {
        return Verdict::Fail(Why::Never);
    }
    if h.total >= r.total {
        return Verdict::Fail(Why::Total { total: r.total });
    }
    let recent = h.in_window(r.window_ms, now_ms);
    if recent >= r.max {
        return Verdict::Fail(Why::Window {
            max: r.max,
            window_ms: r.window_ms,
        });
    }
    let delay_ms = r
        .backoff_ms
        .saturating_mul(1 << recent.min(32))
        .min(r.backoff_max_ms);
    Verdict::Restart { delay_ms }
}

const MINUTE_MS: u64 = 60_000;

/// How long a start has to wait until fewer than `rate` starts of all the workloads together happened in the last minute
/// (`None`: it does not have to wait). `starts` holds the uptime, in milliseconds, of every recent start.
pub fn start_wait(starts: &[u64], now_ms: u64, rate: u32) -> Option<u64> {
    let mut recent: Vec<u64> = starts
        .iter()
        .copied()
        .filter(|&t| t <= now_ms && now_ms - t < MINUTE_MS)
        .collect();
    recent.sort_unstable();
    let rate = rate.max(1) as usize;
    if recent.len() < rate {
        return None;
    }
    Some(recent[recent.len() - rate] + MINUTE_MS - now_ms)
}

/// The most that a record keeps of a command line or a reason; both are text from outside, kept on one printable line.
pub const TEXT_MAX: usize = 120;
const RECORD_BYTES: usize = 4096;
const RECORD_LINES: usize = 32;

/// One line of printable ASCII, bounded: what a record keeps of text that came from outside.
pub fn tidy(s: &str) -> String {
    s.chars()
        .take(TEXT_MAX)
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .collect()
}

/// What is known of one supervised workload. It is stored as text (`render`) and read back strictly (`parse`): a
/// record that does not parse is corrupt, never half-believed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// `CLASS/ID`.
    pub workload: String,
    pub phase: Phase,
    /// How many times the workload was started: the first run is 1.
    pub gen: u32,
    /// The main process of the current run.
    pub main: Option<i32>,
    /// The supervisor that works on the workload.
    pub sup: Option<i32>,
    /// What an operator asked for during the current run.
    pub intent: Option<Intent>,
    /// How the previous run ended; kept across restarts.
    pub ended: Option<Ending>,
    /// The raw status of the previous run.
    pub status: Option<Exit>,
    /// The kernel's OOM killer killed something of the previous run.
    pub oom: bool,
    /// How the last stop ended: `empty`, `graceful` or `killed:N`.
    pub stop: Option<String>,
    /// Automatic restarts since the supervisor started.
    pub restarts: u32,
    /// Seconds since the epoch of the last change of phase.
    pub since: u64,
    /// The program of the workload (without arguments).
    pub cmd: String,
    /// Why the workload is in this phase.
    pub reason: String,
    /// The audit sequence number of the last change.
    pub seq: u64,
}

impl Record {
    pub fn new(workload: &str, now_secs: u64) -> Record {
        Record {
            workload: workload.to_string(),
            phase: Phase::Created,
            gen: 0,
            main: None,
            sup: None,
            intent: None,
            ended: None,
            status: None,
            oom: false,
            stop: None,
            restarts: 0,
            since: now_secs,
            cmd: String::new(),
            reason: String::new(),
            seq: 0,
        }
    }

    /// Change the phase. An illegal change is refused and leaves the record as it was. A new run (`starting`) counts a
    /// start, and is refused while an operator's stop is pending: that stop must be seen first. The end of a run
    /// (`stopped`, `finished`, `failed`) clears what belongs to a live run.
    pub fn advance(&mut self, to: Phase, now_secs: u64) -> Result<(), String> {
        if !self.phase.can_go(to) {
            return Err(format!(
                "{} cannot go from {} to {}",
                self.workload,
                self.phase.name(),
                to.name()
            ));
        }
        if to == Phase::Starting {
            if let Some(i) = self.intent {
                return Err(format!(
                    "{} has a {} pending: it is not started again",
                    self.workload,
                    i.name()
                ));
            }
            if self.phase != Phase::Backoff {
                self.restarts = 0;
                self.stop = None;
            }
            self.gen = self.gen.saturating_add(1);
            self.main = None;
        }
        if to.is_terminal() {
            self.intent = None;
            self.main = None;
            self.sup = None;
        }
        self.phase = to;
        self.since = now_secs;
        Ok(())
    }

    /// Record what an operator asked for; a stop never replaces a kill. Returns whether the record changed.
    pub fn ask(&mut self, i: Intent) -> bool {
        if self.intent == Some(Intent::Kill) || self.intent == Some(i) {
            return false;
        }
        self.intent = Some(i);
        true
    }

    pub fn set_cmd(&mut self, argv0: &str) {
        self.cmd = tidy(argv0);
    }

    pub fn set_reason(&mut self, why: &str) {
        self.reason = tidy(why);
    }

    pub fn render(&self) -> String {
        let mut s = String::from("v=1\n");
        let mut put = |k: &str, v: String| {
            s.push_str(k);
            s.push('=');
            s.push_str(&v);
            s.push('\n');
        };
        put("workload", self.workload.clone());
        put("phase", self.phase.name().into());
        put("gen", self.gen.to_string());
        if let Some(p) = self.main {
            put("main", p.to_string());
        }
        if let Some(p) = self.sup {
            put("sup", p.to_string());
        }
        if let Some(i) = self.intent {
            put("intent", i.name().into());
        }
        if let Some(e) = self.ended {
            put("ended", e.render());
        }
        if let Some(x) = self.status {
            put("status", x.render());
        }
        put("oom", u8::from(self.oom).to_string());
        if let Some(t) = &self.stop {
            put("stop", t.clone());
        }
        put("restarts", self.restarts.to_string());
        put("since", self.since.to_string());
        if !self.cmd.is_empty() {
            put("cmd", self.cmd.clone());
        }
        if !self.reason.is_empty() {
            put("reason", self.reason.clone());
        }
        put("seq", self.seq.to_string());
        s
    }

    pub fn parse(text: &str) -> Result<Record, String> {
        if text.len() > RECORD_BYTES {
            return Err(format!("larger than {RECORD_BYTES} bytes"));
        }
        if text.lines().count() > RECORD_LINES {
            return Err(format!("more than {RECORD_LINES} lines"));
        }
        let mut r = Record::new("", 0);
        let mut seen: Vec<&str> = Vec::new();
        let (mut version, mut workload, mut phase) = (false, false, false);
        for (n, line) in text.lines().enumerate() {
            let at = n + 1;
            let (k, v) = line
                .split_once('=')
                .ok_or(format!("line {at}: `{}` is not key=value", tidy(line)))?;
            if seen.contains(&k) {
                return Err(format!("line {at}: {} appears twice", tidy(k)));
            }
            seen.push(k);
            let num = |v: &str| {
                v.parse::<u64>()
                    .map_err(|_| format!("line {at}: {k}: `{}` is not a number", tidy(v)))
            };
            let pid = |v: &str| {
                v.parse::<i32>()
                    .ok()
                    .filter(|&p| p >= 2)
                    .ok_or(format!("line {at}: {k}: `{}` is not a process id", tidy(v)))
            };
            let text_ok = |v: &str| {
                if v.chars().count() <= TEXT_MAX && v.chars().all(|c| (' '..='~').contains(&c)) {
                    Ok(v.to_string())
                } else {
                    Err(format!(
                        "line {at}: {k}: not one bounded line of printable text"
                    ))
                }
            };
            match k {
                "v" => {
                    if v != "1" {
                        return Err(format!("line {at}: unknown record version `{}`", tidy(v)));
                    }
                    version = true;
                }
                "workload" => {
                    let ok = v
                        .split_once('/')
                        .is_some_and(|(c, i)| valid_name(c) && valid_name(i));
                    if !ok {
                        return Err(format!("line {at}: `{}` is not CLASS/ID", tidy(v)));
                    }
                    r.workload = v.to_string();
                    workload = true;
                }
                "phase" => {
                    r.phase = Phase::parse(v)
                        .ok_or(format!("line {at}: `{}` is not a phase", tidy(v)))?;
                    phase = true;
                }
                "gen" => {
                    r.gen = u32::try_from(num(v)?)
                        .map_err(|_| format!("line {at}: gen is too large"))?
                }
                "main" => r.main = Some(pid(v)?),
                "sup" => r.sup = Some(pid(v)?),
                "intent" => {
                    r.intent = Some(
                        Intent::parse(v)
                            .ok_or(format!("line {at}: `{}` is not stop or kill", tidy(v)))?,
                    )
                }
                "ended" => {
                    r.ended = Some(
                        Ending::parse(v)
                            .ok_or(format!("line {at}: `{}` is not an ending", tidy(v)))?,
                    )
                }
                "status" => {
                    r.status = Some(
                        Exit::parse(v)
                            .ok_or(format!("line {at}: `{}` is not an exit status", tidy(v)))?,
                    )
                }
                "oom" => {
                    r.oom = match v {
                        "0" => false,
                        "1" => true,
                        _ => return Err(format!("line {at}: oom is 0 or 1")),
                    }
                }
                "stop" => {
                    let ok = matches!(v, "empty" | "graceful")
                        || v.strip_prefix("killed:")
                            .is_some_and(|n| n.parse::<u32>().is_ok());
                    if !ok {
                        return Err(format!("line {at}: `{}` is not a stop result", tidy(v)));
                    }
                    r.stop = Some(v.to_string());
                }
                "restarts" => {
                    r.restarts = u32::try_from(num(v)?)
                        .map_err(|_| format!("line {at}: restarts is too large"))?
                }
                "since" => r.since = num(v)?,
                "cmd" => r.cmd = text_ok(v)?,
                "reason" => r.reason = text_ok(v)?,
                "seq" => r.seq = num(v)?,
                _ => return Err(format!("line {at}: unknown key `{}`", tidy(k))),
            }
        }
        for (have, what) in [(version, "v"), (workload, "workload"), (phase, "phase")] {
            if !have {
                return Err(format!("{what} is missing"));
            }
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(mode: RestartMode) -> Restart {
        Restart {
            mode,
            ..Restart::default()
        }
    }

    fn hist(recent: &[u64], total: u32) -> History {
        History {
            recent: recent.to_vec(),
            total,
        }
    }

    #[test]
    fn every_phase_has_its_own_name() {
        for p in Phase::ALL {
            assert_eq!(Phase::parse(p.name()), Some(p));
        }
        assert_eq!(Phase::parse("sleeping"), None);
        let mut names: Vec<_> = Phase::ALL.iter().map(|p| p.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Phase::ALL.len());
    }

    #[test]
    fn the_transitions_leave_no_unrecoverable_state() {
        // every phase can be reached from `created`, and every phase that is not final reaches a final one
        let reach = |from: Phase| {
            let mut seen = vec![from];
            let mut i = 0;
            while i < seen.len() {
                for &n in seen[i].next() {
                    if !seen.contains(&n) {
                        seen.push(n);
                    }
                }
                i += 1;
            }
            seen
        };
        assert_eq!(reach(Phase::Created).len(), Phase::ALL.len());
        for p in Phase::ALL {
            assert!(!p.can_go(p), "{p:?} goes to itself");
            if !p.is_terminal() {
                assert!(
                    reach(p).iter().any(|q| q.is_terminal()),
                    "{p:?} has no way to an end"
                );
                if p != Phase::Recovering {
                    assert!(
                        p.can_go(Phase::Recovering),
                        "{p:?} cannot be recovered from a lost supervisor"
                    );
                }
            }
        }
        // a failed workload leaves failed only by being reset; a finished or stopped one only by being started
        assert!(Phase::Failed.next().is_empty());
        assert_eq!(Phase::Stopped.next(), &[Phase::Starting]);
        assert_eq!(Phase::Finished.next(), &[Phase::Starting]);
        // what must never happen
        for (from, to) in [
            (Phase::Created, Phase::Running),
            (Phase::Running, Phase::Backoff),
            (Phase::Running, Phase::Stopped),
            (Phase::Killing, Phase::Stopping),
            (Phase::Backoff, Phase::Running),
            (Phase::Stopped, Phase::Running),
            (Phase::Finished, Phase::Backoff),
            (Phase::Failed, Phase::Starting),
            (Phase::Failed, Phase::Stopped),
            (Phase::Recovering, Phase::Starting),
            (Phase::Recovering, Phase::Running),
        ] {
            assert!(!from.can_go(to), "{from:?} must not go to {to:?}");
        }
    }

    #[test]
    fn advance_follows_the_table_and_counts_runs() {
        let mut rec = Record::new("batch/a", 10);
        assert_eq!(rec.phase, Phase::Created);
        assert!(
            rec.advance(Phase::Running, 11).is_err(),
            "created -> running"
        );
        assert_eq!(
            (rec.phase, rec.since),
            (Phase::Created, 10),
            "a refusal changes nothing"
        );
        rec.advance(Phase::Starting, 11).unwrap();
        assert_eq!(rec.gen, 1);
        rec.advance(Phase::Running, 12).unwrap();
        rec.main = Some(77);
        rec.sup = Some(70);
        assert!(rec.ask(Intent::Stop));
        assert!(
            rec.advance(Phase::Starting, 13).is_err(),
            "running -> starting"
        );
        rec.advance(Phase::Stopping, 13).unwrap();
        rec.advance(Phase::Sweeping, 14).unwrap();
        rec.advance(Phase::Stopped, 15).unwrap();
        assert_eq!(
            (rec.intent, rec.main, rec.sup, rec.since),
            (None, None, None, 15),
            "the end of a run clears what belongs to a live run"
        );
        rec.restarts = 4;
        rec.stop = Some("graceful".into());
        rec.advance(Phase::Starting, 16).unwrap();
        assert_eq!(
            (rec.gen, rec.restarts, rec.stop.clone()),
            (2, 0, None),
            "a new run starts afresh"
        );
    }

    #[test]
    fn a_pending_stop_is_not_lost_to_a_restart() {
        let mut rec = Record::new("batch/a", 0);
        for p in [
            Phase::Starting,
            Phase::Running,
            Phase::Sweeping,
            Phase::Backoff,
        ] {
            rec.advance(p, 1).unwrap();
        }
        rec.restarts = 2;
        assert!(rec.ask(Intent::Stop));
        let e = rec.advance(Phase::Starting, 2).unwrap_err();
        assert!(e.contains("stop pending"), "{e}");
        assert_eq!(
            (rec.phase, rec.gen, rec.intent),
            (Phase::Backoff, 1, Some(Intent::Stop))
        );
        rec.advance(Phase::Stopped, 3).unwrap();
        // an automatic restart keeps its count; the intent of the next run starts empty
        let mut rec = Record::new("batch/a", 0);
        for p in [
            Phase::Starting,
            Phase::Running,
            Phase::Sweeping,
            Phase::Backoff,
        ] {
            rec.advance(p, 1).unwrap();
        }
        rec.restarts = 2;
        rec.advance(Phase::Starting, 2).unwrap();
        assert_eq!((rec.gen, rec.restarts), (2, 2));
    }

    #[test]
    fn a_stop_never_replaces_a_kill() {
        let mut rec = Record::new("batch/a", 0);
        assert!(rec.ask(Intent::Stop));
        assert!(!rec.ask(Intent::Stop), "asking again changes nothing");
        assert!(rec.ask(Intent::Kill), "a stop is escalated to a kill");
        assert!(!rec.ask(Intent::Stop), "a kill is not softened to a stop");
        assert_eq!(rec.intent, Some(Intent::Kill));
    }

    #[test]
    fn a_run_is_classified_by_what_was_asked_first() {
        assert_eq!(classify(Exit::Code(0), None), Ending::Clean);
        assert_eq!(classify(Exit::Code(3), None), Ending::Failed(3));
        assert_eq!(classify(Exit::Signal(11), None), Ending::Crashed(11));
        assert_eq!(classify(Exit::Signal(9), None), Ending::Crashed(9));
        for exit in [
            Exit::Code(0),
            Exit::Code(3),
            Exit::Signal(15),
            Exit::Signal(9),
        ] {
            assert_eq!(
                classify(exit, Some(Intent::Stop)),
                Ending::Requested(Intent::Stop)
            );
            assert_eq!(
                classify(exit, Some(Intent::Kill)),
                Ending::Requested(Intent::Kill)
            );
        }
        assert!(Ending::Failed(1).unexpected() && Ending::Crashed(9).unexpected());
        assert!(Ending::Lost.unexpected() && Ending::LaunchFailed.unexpected());
        assert!(!Ending::Clean.unexpected() && !Ending::Requested(Intent::Kill).unexpected());
    }

    #[test]
    fn endings_and_statuses_round_trip() {
        for e in [
            Ending::Clean,
            Ending::Failed(1),
            Ending::Failed(255),
            Ending::Crashed(9),
            Ending::Crashed(64),
            Ending::Requested(Intent::Stop),
            Ending::Requested(Intent::Kill),
            Ending::Lost,
            Ending::LaunchFailed,
        ] {
            assert_eq!(Ending::parse(&e.render()), Some(e), "{e:?}");
        }
        for bad in [
            "",
            "failed",
            "failed:0",
            "failed:256",
            "crashed:0",
            "crashed:65",
            "requested:now",
            "boom",
            "clean:1",
        ] {
            assert_eq!(Ending::parse(bad), None, "{bad:?}");
        }
        for x in [
            Exit::Code(0),
            Exit::Code(255),
            Exit::Signal(1),
            Exit::Signal(64),
        ] {
            assert_eq!(Exit::parse(&x.render()), Some(x));
        }
        for bad in [
            "exit:256",
            "exit:-1",
            "signal:0",
            "signal:65",
            "exit",
            "stop:1",
            "exit:x",
        ] {
            assert_eq!(Exit::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn what_follows_an_end_is_decided_by_who_asked_and_by_the_limits() {
        let now = 100_000;
        let none = hist(&[], 0);
        for mode in [
            RestartMode::Never,
            RestartMode::OnFailure,
            RestartMode::Always,
        ] {
            for i in [Intent::Stop, Intent::Kill] {
                assert_eq!(
                    decide(&r(mode), &none, Ending::Requested(i), now),
                    Verdict::Stop,
                    "{mode:?} {i:?}"
                );
            }
            assert_eq!(
                decide(&r(mode), &none, Ending::Lost, now),
                Verdict::Fail(Why::Lost),
                "a lost workload is never started on a guess"
            );
        }
        // a clean exit finishes unless the mode is always
        assert_eq!(
            decide(&r(RestartMode::Never), &none, Ending::Clean, now),
            Verdict::Finish
        );
        assert_eq!(
            decide(&r(RestartMode::OnFailure), &none, Ending::Clean, now),
            Verdict::Finish
        );
        assert_eq!(
            decide(&r(RestartMode::Always), &none, Ending::Clean, now),
            Verdict::Restart { delay_ms: 500 }
        );
        // a failure is restarted unless the mode is never
        for e in [Ending::Failed(1), Ending::Crashed(11), Ending::LaunchFailed] {
            assert_eq!(
                decide(&r(RestartMode::Never), &none, e, now),
                Verdict::Fail(Why::Never)
            );
            assert_eq!(
                decide(&r(RestartMode::OnFailure), &none, e, now),
                Verdict::Restart { delay_ms: 500 }
            );
            assert_eq!(
                decide(&r(RestartMode::Always), &none, e, now),
                Verdict::Restart { delay_ms: 500 }
            );
        }
    }

    #[test]
    fn the_delay_doubles_up_to_its_cap_and_the_budget_ends_the_loop() {
        let rr = Restart {
            max: 5,
            backoff_ms: 100,
            backoff_max_ms: 700,
            ..r(RestartMode::OnFailure)
        };
        let now = 10_000;
        let at = |n: usize| {
            decide(
                &rr,
                &hist(&vec![now - 1; n], n as u32),
                Ending::Failed(1),
                now,
            )
        };
        assert_eq!(at(0), Verdict::Restart { delay_ms: 100 });
        assert_eq!(at(1), Verdict::Restart { delay_ms: 200 });
        assert_eq!(at(2), Verdict::Restart { delay_ms: 400 });
        assert_eq!(at(3), Verdict::Restart { delay_ms: 700 }, "800 is capped");
        assert_eq!(at(4), Verdict::Restart { delay_ms: 700 });
        assert_eq!(
            at(5),
            Verdict::Fail(Why::Window {
                max: 5,
                window_ms: 60_000
            })
        );
        // a budget that is used up fails even a clean exit under `always`
        assert_eq!(
            decide(
                &Restart {
                    mode: RestartMode::Always,
                    ..rr.clone()
                },
                &hist(&[now - 1; 5], 5),
                Ending::Clean,
                now
            ),
            Verdict::Fail(Why::Window {
                max: 5,
                window_ms: 60_000
            })
        );
        // restarts that left the window are forgotten: one that is exactly a window old has left it ...
        let old = hist(&[1, 2, 3, 4, 5], 5);
        assert_eq!(
            decide(&rr, &old, Ending::Failed(1), 60_000),
            Verdict::Fail(Why::Window {
                max: 5,
                window_ms: 60_000
            }),
            "the oldest is 59999ms old: all five are inside the window"
        );
        assert_eq!(
            decide(&rr, &old, Ending::Failed(1), 60_001),
            Verdict::Restart { delay_ms: 700 },
            "the oldest is exactly a window old: four are inside, 1600 is capped"
        );
        assert_eq!(
            decide(&rr, &old, Ending::Failed(1), 60_003),
            Verdict::Restart { delay_ms: 400 },
            "4 and 5 are inside the window, 3 is exactly a window old"
        );
        assert_eq!(
            decide(&rr, &old, Ending::Failed(1), 60_004),
            Verdict::Restart { delay_ms: 200 },
            "only 5 is inside the window"
        );
        assert_eq!(
            decide(&rr, &old, Ending::Failed(1), 70_000),
            Verdict::Restart { delay_ms: 100 }
        );
        // ... but the total is a bound on the whole run
        let total = Restart { total: 5, ..rr };
        assert_eq!(
            decide(&total, &hist(&[], 5), Ending::Failed(1), now),
            Verdict::Fail(Why::Total { total: 5 })
        );
        assert!(Why::Total { total: 5 }.text().contains("5 restarts"));
        assert!(Why::Window {
            max: 3,
            window_ms: 60_000
        }
        .text()
        .contains("3 restarts within 60s"));
        assert!(Why::Window {
            max: 3,
            window_ms: 1500
        }
        .text()
        .contains("1500ms"));
    }

    /// Run a workload that crashes `run_ms` after every start through `decide`: how many restarts, and how long it took.
    fn crash_loop(rr: &Restart, run_ms: u64) -> (u32, u64) {
        let (mut h, mut now, mut restarts) = (History::default(), 0u64, 0u32);
        loop {
            match decide(rr, &h, Ending::Crashed(11), now) {
                Verdict::Restart { delay_ms } => {
                    h.note(now, rr.window_ms);
                    restarts += 1;
                    now += delay_ms + run_ms;
                    assert!(restarts <= MAX_TOTAL_FOR_TEST, "the loop does not end");
                }
                Verdict::Fail(_) => return (restarts, now),
                other => panic!("{other:?}"),
            }
        }
    }

    const MAX_TOTAL_FOR_TEST: u32 = 1000;

    #[test]
    fn a_crash_loop_is_bounded_whatever_its_speed() {
        use crate::policy::{MAX_RESTARTS, MAX_RESTARTS_TOTAL, MIN_BACKOFF_MS, MIN_WINDOW_MS};
        let d = Restart::default();
        // crashing at once: the window budget ends it after `max` restarts, in a few seconds
        let (n, took) = crash_loop(&d, 0);
        assert_eq!(n, d.max);
        assert!(took < 10_000, "{took}ms");
        // crashing slowly: each run outlasts the window, so only the total is left to bound it
        let (n, _) = crash_loop(&d, d.window_ms);
        assert_eq!(n, d.total);
        // at the fixed caps, the tightest delay and the narrowest window
        let wide = Restart {
            mode: RestartMode::Always,
            max: MAX_RESTARTS,
            window_ms: MIN_WINDOW_MS,
            total: MAX_RESTARTS_TOTAL,
            backoff_ms: MIN_BACKOFF_MS,
            backoff_max_ms: 30_000,
            rate: 120,
        };
        for run_ms in [0, 1, 500, 5_000] {
            let (n, _) = crash_loop(&wide, run_ms);
            assert!(n <= MAX_RESTARTS_TOTAL, "run {run_ms}ms: {n} restarts");
        }
        let (n, _) = crash_loop(&wide, 0);
        assert!(
            n >= MAX_RESTARTS,
            "the window budget alone allows {MAX_RESTARTS}"
        );
    }

    #[test]
    fn starts_are_limited_per_minute_over_all_workloads() {
        assert_eq!(start_wait(&[], 100, 3), None);
        assert_eq!(start_wait(&[0, 10], 30, 3), None, "two of three");
        assert_eq!(
            start_wait(&[0, 10, 20], 30, 3),
            Some(59_970),
            "the oldest start leaves the minute at 60000"
        );
        assert_eq!(
            start_wait(&[0, 10, 20], 60_000, 3),
            None,
            "the first start is a minute old"
        );
        assert_eq!(
            start_wait(&[0, 10, 20, 30], 40, 3),
            Some(59_970),
            "four starts: the two oldest have to leave the minute, the second one at 60010"
        );
        assert_eq!(
            start_wait(&[100_000, 100_001, 100_002], 5, 3),
            None,
            "from before a reboot: ignored"
        );
        assert_eq!(
            start_wait(&[0], 1, 0),
            Some(59_999),
            "a rate of 0 is read as 1"
        );
        // whatever the starts: after the wait there is room, and one millisecond before it there is not
        // (the starts of a real log are all in the past; the ones dated after `now` are ignored, so they are left out here)
        for all in [
            vec![0, 10, 20, 30],
            vec![5, 5, 5, 5, 5],
            vec![0, 1_000, 2_000, 59_000, 59_500, 59_900],
            vec![100, 200],
        ] {
            for rate in 1..=6 {
                for now in [300, 30_000, 59_999] {
                    let starts: Vec<u64> = all.iter().copied().filter(|&t| t <= now).collect();
                    if let Some(w) = start_wait(&starts, now, rate) {
                        assert!(w >= 1, "{starts:?} rate {rate} at {now}");
                        assert_eq!(start_wait(&starts, now + w, rate), None);
                        assert!(start_wait(&starts, now + w - 1, rate).is_some());
                    }
                }
            }
        }
    }

    fn full() -> Record {
        let mut rec = Record::new("interactive/job_1", 1_790_000_000);
        rec.phase = Phase::Backoff;
        rec.gen = 3;
        rec.main = Some(4321);
        rec.sup = Some(4300);
        rec.intent = Some(Intent::Kill);
        rec.ended = Some(Ending::Crashed(9));
        rec.status = Some(Exit::Signal(9));
        rec.oom = true;
        rec.stop = Some("killed:2".into());
        rec.restarts = 2;
        rec.set_cmd("/usr/bin/agent");
        rec.set_reason("restart 2 of 3 in 1000ms");
        rec.seq = 41;
        rec
    }

    #[test]
    fn a_record_reads_back_exactly_what_was_written() {
        for rec in [full(), Record::new("batch/a", 5)] {
            let text = rec.render();
            assert_eq!(Record::parse(&text), Ok(rec.clone()), "{text}");
            assert!(text.starts_with("v=1\n") && text.ends_with('\n'));
        }
        let minimal = Record::new("batch/a", 5).render();
        assert!(
            !minimal.contains("intent") && !minimal.contains("main=") && !minimal.contains("cmd="),
            "{minimal}"
        );
    }

    #[test]
    fn a_record_that_does_not_parse_is_corrupt() {
        let good = full().render();
        let swap = |from: &str, to: &str| good.replace(from, to);
        let long = "x".repeat(TEXT_MAX + 1);
        for (text, want) in [
            (String::new(), "v is missing"),
            ("v=1\n".to_string(), "workload is missing"),
            ("v=1\nworkload=batch/a\n".to_string(), "phase is missing"),
            (swap("v=1", "v=2"), "unknown record version"),
            (format!("{good}gen=1\n"), "appears twice"),
            (format!("{good}colour=red\n"), "unknown key"),
            (format!("{good}no equals sign\n"), "not key=value"),
            (swap("phase=backoff", "phase=asleep"), "not a phase"),
            (
                swap("workload=interactive/job_1", "workload=interactive"),
                "not CLASS/ID",
            ),
            (
                swap("workload=interactive/job_1", "workload=a/b/c"),
                "not CLASS/ID",
            ),
            (
                swap("workload=interactive/job_1", "workload=../x"),
                "not CLASS/ID",
            ),
            (swap("gen=3", "gen=-3"), "not a number"),
            (swap("gen=3", "gen=99999999999"), "too large"),
            (swap("main=4321", "main=1"), "not a process id"),
            (swap("main=4321", "main=abc"), "not a process id"),
            (swap("intent=kill", "intent=maybe"), "not stop or kill"),
            (swap("ended=crashed:9", "ended=exploded"), "not an ending"),
            (
                swap("status=signal:9", "status=signal:0"),
                "not an exit status",
            ),
            (swap("oom=1", "oom=2"), "oom is 0 or 1"),
            (swap("stop=killed:2", "stop=killed:x"), "not a stop result"),
            (
                swap("cmd=/usr/bin/agent", &format!("cmd={long}")),
                "bounded line",
            ),
            (
                swap("cmd=/usr/bin/agent", "cmd=\u{1b}[31mred"),
                "bounded line",
            ),
            (
                format!("{good}{}", "k=v\n".repeat(40)),
                "more than 32 lines",
            ),
            (
                format!("{good}reason={}\n", "y".repeat(5000)),
                "larger than 4096",
            ),
        ] {
            let e = Record::parse(&text).unwrap_err();
            assert!(e.contains(want), "{want:?} not in {e:?}");
        }
    }

    #[test]
    fn text_from_outside_is_kept_on_one_bounded_printable_line() {
        assert_eq!(tidy("a\nb\tc\u{1b}[0m\u{e9}"), "a?b?c?[0m?");
        assert_eq!(tidy(&"z".repeat(500)).len(), TEXT_MAX);
        let mut rec = Record::new("batch/a", 0);
        rec.set_cmd("evil\nphase=running");
        rec.set_reason(&"r".repeat(1000));
        let back = Record::parse(&rec.render()).unwrap();
        assert_eq!(
            back.phase,
            Phase::Created,
            "a command line cannot write a field"
        );
        assert_eq!(back.cmd, "evil?phase=running");
        assert_eq!(back.reason.len(), TEXT_MAX);
    }
}
