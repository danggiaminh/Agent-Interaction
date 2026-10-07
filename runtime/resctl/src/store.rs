//! The state of the lifecycle subsystem: a record for every supervised workload, the audit trail, the registry of
//! critical processes and the log of starts. All of it lives in one directory (`/run/agent-interaction` unless
//! `--state-dir` says otherwise):
//!
//! ```text
//! DIR/                   0700 and owned by the user that runs resctl; checked every time the store is opened
//!   store.lock           held while a file below is changed
//!   records/CLASS.ID     the record of a workload (`life::Record`), replaced in one step
//!   locks/CLASS.ID.sup   held by the supervisor of the workload for as long as it lives
//!   critical             the registry of protected processes: pid, start time, name
//!   starts               the uptime of every recent start, for the global start rate
//!   audit.log[.1]        the audit trail: two generations of `AUDIT_LINES` lines
//!   seq                  the sequence number of the last audit line
//!   quarantine/          files that did not read back, kept for inspection
//! ```
//!
//! The directory is what keeps a workload from forging its own state: a workload runs as another user, so it can
//! neither write a record nor add itself to the registry of critical processes.
//!
//! A change is audited first and made second, and when the change fails the trail says so (`warn`): the trail never
//! claims more than was done. Whether an audit that cannot be written stops the change depends on the change
//! (`Audit`): protecting a process needs its trail, ending a workload must not wait for it.

use crate::domain::{Error, Result, Workload};
use crate::fs::{starttime_of, Guard, Host, Lock};
use crate::life::{start_wait, tidy, Record};
use crate::policy::valid_name;
use std::cell::RefCell;
use std::io;
use std::ops::Deref;

/// Records that exist at the same time, ended workloads included.
pub const MAX_RECORDS: usize = 256;
/// Processes in the registry of critical processes.
pub const MAX_CRITICAL: usize = 64;
/// Lines of one generation of the audit trail; the trail keeps this many and up to twice as many.
pub const AUDIT_LINES: u64 = 1024;
/// Files kept in `quarantine/`; a file that does not fit is removed.
const MAX_QUARANTINE: usize = 32;
/// How long a change waits for another resctl that is changing the state.
const LOCK_WAIT_MS: u64 = 10_000;
const LOCK_POLL_MS: u64 = 5;
/// Attempts to take the lock of a supervisor that a reader may be probing at that moment.
const CLAIM_TRIES: u32 = 20;
const MINUTE_MS: u64 = 60_000;
/// Starts the log keeps; more than any rate the policy allows.
const START_KEEP: usize = 128;
/// The registry and the log of starts are small; a larger file is not one of ours.
const STATE_BYTES: usize = 8192;
const WARNINGS_MAX: usize = 16;
/// What reading `/proc/PID/stat` reports of a process that ended while it was being read.
const ESRCH: i32 = 3;

const RECORDS: &str = "records";
const LOCKS: &str = "locks";
const QUARANTINE: &str = "quarantine";
const AUDIT: &str = "audit.log";
const AUDIT_OLD: &str = "audit.log.1";
const SEQ: &str = "seq";
const CRITICAL: &str = "critical";
const STARTS: &str = "starts";
const STORE_LOCK: &str = "store.lock";

/// Trouble with the state is an `Error::State`; a refusal of the machine to touch it is a `Denied`.
fn state_err(what: &str, e: &io::Error) -> Error {
    let msg = format!("{what}: {e}");
    match e.kind() {
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => Error::Denied(msg),
        _ => Error::State(msg),
    }
}

fn missing(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

/// What a line of the audit trail is about. The words are the vocabulary of `resctl audit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Init,
    Create,
    Run,
    Enter,
    Freeze,
    Thaw,
    Stop,
    Kill,
    Remove,
    Recover,
    Reset,
    Teardown,
    Supervise,
    /// A workload changed its phase.
    Phase,
    /// An operator asked for a stop or a kill.
    Intent,
    Start,
    /// The main process of a workload ended, and how.
    Exit,
    Restart,
    /// A request was refused.
    Refuse,
    Protect,
    Unprotect,
    /// A critical process was taken out of a workload domain.
    Rescue,
    Quarantine,
    /// Something could not be done or not be recorded.
    Warn,
}

impl Event {
    pub const ALL: [Event; 24] = [
        Event::Init,
        Event::Create,
        Event::Run,
        Event::Enter,
        Event::Freeze,
        Event::Thaw,
        Event::Stop,
        Event::Kill,
        Event::Remove,
        Event::Recover,
        Event::Reset,
        Event::Teardown,
        Event::Supervise,
        Event::Phase,
        Event::Intent,
        Event::Start,
        Event::Exit,
        Event::Restart,
        Event::Refuse,
        Event::Protect,
        Event::Unprotect,
        Event::Rescue,
        Event::Quarantine,
        Event::Warn,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Event::Init => "init",
            Event::Create => "create",
            Event::Run => "run",
            Event::Enter => "enter",
            Event::Freeze => "freeze",
            Event::Thaw => "thaw",
            Event::Stop => "stop",
            Event::Kill => "kill",
            Event::Remove => "remove",
            Event::Recover => "recover",
            Event::Reset => "reset",
            Event::Teardown => "teardown",
            Event::Supervise => "supervise",
            Event::Phase => "phase",
            Event::Intent => "intent",
            Event::Start => "start",
            Event::Exit => "exit",
            Event::Restart => "restart",
            Event::Refuse => "refuse",
            Event::Protect => "protect",
            Event::Unprotect => "unprotect",
            Event::Rescue => "rescue",
            Event::Quarantine => "quarantine",
            Event::Warn => "warn",
        }
    }

    pub fn parse(s: &str) -> Option<Event> {
        Event::ALL.iter().copied().find(|e| e.name() == s)
    }
}

/// One line of the audit trail: `SEQ EPOCH ACTOR EVENT SUBJECT DETAIL`. The actor is `PID:ROLE` of the process that
/// wrote the line, the subject a `CLASS/ID`, a process or `-`, and the detail is free text on the rest of the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub seq: u64,
    pub at: u64,
    pub actor: String,
    pub event: Event,
    pub subject: String,
    pub detail: String,
}

impl Line {
    pub fn render(&self) -> String {
        let mut s = format!(
            "{} {} {} {} {}",
            self.seq,
            self.at,
            self.actor,
            self.event.name(),
            self.subject
        );
        if !self.detail.is_empty() {
            s.push(' ');
            s.push_str(&self.detail);
        }
        s
    }

    pub fn parse(line: &str) -> Option<Line> {
        let mut f = line.splitn(6, ' ');
        let seq = f.next()?.parse().ok()?;
        let at = f.next()?.parse().ok()?;
        let actor = f.next().filter(|a| !a.is_empty())?;
        let event = Event::parse(f.next()?)?;
        let subject = f.next().filter(|s| !s.is_empty())?;
        Some(Line {
            seq,
            at,
            actor: actor.into(),
            event,
            subject: subject.into(),
            detail: f.next().unwrap_or("").into(),
        })
    }
}

/// One word of the audit trail, from text that may come from outside: printable, bounded, no spaces.
fn token(s: &str) -> String {
    let t: String = tidy(s)
        .chars()
        .map(|c| if c == ' ' { '_' } else { c })
        .collect();
    if t.is_empty() {
        "-".into()
    } else {
        t
    }
}

/// Whether a change may go ahead when its line cannot be added to the audit trail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audit {
    /// No: the change is refused. For what makes a process protected, and for starting a workload.
    Required,
    /// Yes, with a warning. For what ends a workload or repairs the state: that must never wait for the trail.
    Optional,
}

/// A record as the store finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stored {
    Missing,
    Record(Record),
    /// The file does not read back as the record of its workload; why.
    Corrupt(String),
}

/// A file of `records/`.
#[derive(Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    /// The workload the name stands for; `None` for a file that is not named like a record (what an interrupted write
    /// leaves behind).
    pub workload: Option<Workload>,
    pub stored: Stored,
}

/// Whether a workload has a supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sup {
    /// Its lock is held.
    Alive,
    /// Its lock exists and nobody holds it: the supervisor ended without finishing its work.
    Lost,
    /// No lock exists: the workload was never supervised, or its state was removed.
    Absent,
}

/// A protected process: a pid alone could be taken over by another process, so the start time goes with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub pid: i32,
    pub start: u64,
    pub name: String,
}

/// The registry of critical processes, checked against what runs now.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Critical {
    /// Still the process that was registered.
    pub live: Vec<Entry>,
    /// Ended, or the pid belongs to another process now.
    pub stale: Vec<Entry>,
}

fn render_critical(entries: &[Entry]) -> String {
    let mut s = String::from("v=1\n");
    for e in entries {
        s.push_str(&format!("{} {} {}\n", e.pid, e.start, e.name));
    }
    s
}

fn parse_critical(text: &str) -> std::result::Result<Vec<Entry>, String> {
    if text.len() > STATE_BYTES {
        return Err(format!("larger than {STATE_BYTES} bytes"));
    }
    let mut lines = text.lines();
    if lines.next() != Some("v=1") {
        return Err("it does not start with v=1".into());
    }
    let mut out: Vec<Entry> = Vec::new();
    for (n, line) in lines.enumerate() {
        let at = n + 2;
        if out.len() >= MAX_CRITICAL {
            return Err(format!("more than {MAX_CRITICAL} processes"));
        }
        let f: Vec<&str> = line.split(' ').collect();
        let [pid, start, name] = f[..] else {
            return Err(format!("line {at}: `{}` is not PID START NAME", tidy(line)));
        };
        let pid: i32 = pid
            .parse()
            .ok()
            .filter(|&p| p >= 2)
            .ok_or(format!("line {at}: `{}` is not a process id", tidy(pid)))?;
        let start: u64 = start
            .parse()
            .map_err(|_| format!("line {at}: `{}` is not a start time", tidy(start)))?;
        if !valid_name(name) {
            return Err(format!("line {at}: `{}` is not a usable name", tidy(name)));
        }
        if out.iter().any(|e| e.pid == pid) {
            return Err(format!("line {at}: process {pid} is listed twice"));
        }
        out.push(Entry {
            pid,
            start,
            name: name.into(),
        });
    }
    Ok(out)
}

fn parse_starts(text: &str) -> std::result::Result<Vec<u64>, String> {
    if text.len() > STATE_BYTES {
        return Err(format!("larger than {STATE_BYTES} bytes"));
    }
    text.lines()
        .enumerate()
        .map(|(n, l)| {
            l.parse::<u64>()
                .map_err(|_| format!("line {}: `{}` is not a time", n + 1, tidy(l)))
        })
        .collect()
}

/// `CLASS.ID`: a class and a workload name hold no dot, so the first dot is the separator.
fn parse_name(name: &str) -> Option<Workload> {
    let (class, id) = name.split_once('.')?;
    Workload::new(class, id).ok()
}

pub struct Store<'a> {
    host: &'a dyn Host,
    dir: String,
    actor: String,
    warnings: RefCell<Vec<String>>,
}

impl<'a> Store<'a> {
    fn new(host: &'a dyn Host, dir: &str, actor: &str) -> Result<Store<'a>> {
        let dir = dir.trim_end_matches('/');
        if !dir.starts_with('/')
            || dir.len() > 200
            || dir.split('/').any(|c| c == "..")
            || dir.chars().any(char::is_control)
        {
            return Err(Error::Usage(format!(
                "`{}` is not a usable state directory (an absolute path without `..`)",
                tidy(dir)
            )));
        }
        if actor.is_empty() || actor.len() > 40 || !actor.chars().all(|c| c.is_ascii_graphic()) {
            return Err(Error::Usage(format!(
                "`{}` is not a usable name for the audit trail",
                tidy(actor)
            )));
        }
        Ok(Store {
            host,
            dir: dir.into(),
            actor: actor.into(),
            warnings: RefCell::new(Vec::new()),
        })
    }

    /// The store, created when it does not exist. The directory must belong to the user running resctl and be closed to
    /// everybody else: anything else is not state that can be trusted.
    pub fn open(host: &'a dyn Host, dir: &str, actor: &str) -> Result<Store<'a>> {
        let store = Store::new(host, dir, actor)?;
        for sub in ["", RECORDS, LOCKS, QUARANTINE] {
            let path = store.path(sub);
            host.mkdirs_private(&path)
                .map_err(|e| state_err(&format!("cannot create the state directory {path}"), &e))?;
            store.check_private(&path)?;
        }
        Ok(store)
    }

    /// The store if it exists (a command that only reads must not create it).
    pub fn existing(host: &'a dyn Host, dir: &str, actor: &str) -> Result<Option<Store<'a>>> {
        let store = Store::new(host, dir, actor)?;
        if !host.exists(&store.dir) {
            return Ok(None);
        }
        store.check_private(&store.dir)?;
        Ok(Some(store))
    }

    fn check_private(&self, path: &str) -> Result<()> {
        let o = self
            .host
            .owner(path)
            .map_err(|e| state_err(&format!("cannot examine the state directory {path}"), &e))?;
        let me = self.host.euid();
        if !o.dir {
            return Err(Error::State(format!("{path} is not a directory")));
        }
        if o.uid != me {
            return Err(Error::State(format!(
                "{path} belongs to user {}, not to user {me} who runs resctl: that state cannot be trusted",
                o.uid
            )));
        }
        if o.mode & 0o077 != 0 {
            return Err(Error::State(format!(
                "{path} is open to other users (mode {:o}): that state cannot be trusted",
                o.mode
            )));
        }
        Ok(())
    }

    pub fn dir(&self) -> &str {
        &self.dir
    }

    fn path(&self, rel: &str) -> String {
        if rel.is_empty() {
            self.dir.clone()
        } else {
            format!("{}/{rel}", self.dir)
        }
    }

    fn record_path(&self, w: &Workload) -> String {
        self.path(&format!("{RECORDS}/{}.{}", w.class, w.id))
    }

    fn sup_path(&self, w: &Workload) -> String {
        self.path(&format!("{LOCKS}/{}.{}.sup", w.class, w.id))
    }

    /// What could not be recorded and did not stop the work: for the caller to print.
    pub fn take_warnings(&self) -> Vec<String> {
        self.warnings.take()
    }

    fn warn_out(&self, msg: String) {
        let mut w = self.warnings.borrow_mut();
        if w.len() < WARNINGS_MAX && !w.contains(&msg) {
            w.push(msg);
        }
    }

    /// Exclusive access to the state. A holder that does not let go (it was stopped, say) fails the change after
    /// `LOCK_WAIT_MS` instead of hanging it. Never block on a supervisor lock while holding this one.
    pub fn lock(&self) -> Result<Locked<'_, 'a>> {
        let path = self.path(STORE_LOCK);
        let deadline = self.host.now_ms().saturating_add(LOCK_WAIT_MS);
        loop {
            match self.host.lock(&path, Lock::Try) {
                Ok(Some(guard)) => {
                    return Ok(Locked {
                        store: self,
                        _guard: guard,
                    })
                }
                Ok(None) if self.host.now_ms() >= deadline => {
                    return Err(Error::State(format!(
                        "the state in {} stayed locked by another resctl for {} s",
                        self.dir,
                        LOCK_WAIT_MS / 1000
                    )))
                }
                Ok(None) => self.host.pause(LOCK_POLL_MS),
                Err(e) => return Err(state_err("cannot lock the state", &e)),
            }
        }
    }

    /// The record of a workload. Files are replaced in one step, so this needs no lock.
    pub fn peek(&self, w: &Workload) -> Result<Stored> {
        let text = match self.host.read(&self.record_path(w)) {
            Ok(t) => t,
            Err(e) if missing(&e) => return Ok(Stored::Missing),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                return Ok(Stored::Corrupt("not text".into()))
            }
            Err(e) => return Err(state_err(&format!("cannot read the record of {w}"), &e)),
        };
        Ok(match Record::parse(&text) {
            Ok(r) if r.workload == w.to_string() => Stored::Record(r),
            Ok(r) => Stored::Corrupt(format!(
                "it is the record of {}, not of {w}",
                tidy(&r.workload)
            )),
            Err(why) => Stored::Corrupt(why),
        })
    }

    /// Every file of `records/`, by name.
    pub fn list(&self) -> Result<Vec<Listed>> {
        let names = match self.host.files(&self.path(RECORDS)) {
            Ok(n) => n,
            Err(e) if missing(&e) => return Ok(Vec::new()),
            Err(e) => return Err(state_err("cannot list the lifecycle records", &e)),
        };
        let mut out = Vec::new();
        for name in names {
            let Some(w) = parse_name(&name) else {
                out.push(Listed {
                    name,
                    workload: None,
                    stored: Stored::Corrupt("not the name of a record".into()),
                });
                continue;
            };
            match self.peek(&w)? {
                Stored::Missing => {}
                stored => out.push(Listed {
                    name,
                    workload: Some(w),
                    stored,
                }),
            }
        }
        Ok(out)
    }

    /// Whether the supervisor of the workload lives. The probe shares the lock, so two probes never take each other for
    /// a supervisor.
    pub fn sup_state(&self, w: &Workload) -> Result<Sup> {
        let path = self.sup_path(w);
        if !self.host.exists(&path) {
            return Ok(Sup::Absent);
        }
        match self.host.lock(&path, Lock::TryShared) {
            Ok(Some(_probe)) => Ok(Sup::Lost),
            Ok(None) => Ok(Sup::Alive),
            Err(e) => Err(state_err(
                &format!("cannot examine the supervisor lock of {w}"),
                &e,
            )),
        }
    }

    /// Wait until the supervisor of the workload is gone, and take over its lock. For the helper that cleans up after a
    /// supervisor that was killed; never call it while holding the state lock.
    pub fn wait_sup(&self, w: &Workload) -> Result<Guard> {
        match self.host.lock(&self.sup_path(w), Lock::Wait) {
            Ok(Some(g)) => Ok(g),
            Ok(None) => Err(Error::State(format!(
                "the supervisor lock of {w} was not granted"
            ))),
            Err(e) => Err(state_err(
                &format!("cannot wait for the supervisor of {w}"),
                &e,
            )),
        }
    }

    /// The last `n` lines of the audit trail, oldest first.
    pub fn audit_tail(&self, n: usize) -> Result<Vec<String>> {
        let mut lines: Vec<String> = Vec::new();
        for name in [AUDIT_OLD, AUDIT] {
            match self.host.read(&self.path(name)) {
                Ok(t) => lines.extend(t.lines().map(str::to_string)),
                Err(e) if missing(&e) => {}
                Err(e) => return Err(state_err("cannot read the audit trail", &e)),
            }
        }
        let keep = lines.len().saturating_sub(n);
        Ok(lines.split_off(keep))
    }

    /// The text of the registry of critical processes; `None` when there is none; `Err` when it cannot be read.
    fn registry_text(&self) -> Result<Option<String>> {
        match self.host.read(&self.path(CRITICAL)) {
            Ok(t) => Ok(Some(t)),
            Err(e) if missing(&e) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => Ok(Some("\u{0}".into())),
            Err(e) => Err(state_err(
                "cannot read the registry of critical processes",
                &e,
            )),
        }
    }

    /// Why the registry of critical processes is corrupt, if it is.
    pub fn registry_corrupt(&self) -> Result<Option<String>> {
        Ok(match self.registry_text()? {
            Some(t) => parse_critical(&t).err(),
            None => None,
        })
    }

    /// Every process of the registry, live or not. A corrupt registry is an error, not an empty one: nothing that kills
    /// may go ahead on a registry it cannot trust.
    pub fn registry(&self) -> Result<Vec<Entry>> {
        match self.registry_text()? {
            None => Ok(Vec::new()),
            Some(t) => parse_critical(&t).map_err(|why| {
                Error::State(format!(
                    "the registry of critical processes is corrupt ({why}): `resctl recover` rebuilds it"
                ))
            }),
        }
    }

    /// The start time of a process, or `None` when it does not exist. A process that exists but cannot be examined, or
    /// whose start time cannot be told, is an error and never an absence.
    fn start_of(&self, pid: i32) -> Result<Option<u64>> {
        match self.host.read(&format!("/proc/{pid}/stat")) {
            Ok(t) => match starttime_of(&t) {
                Some(start) => Ok(Some(start)),
                None => Err(Error::State(format!(
                    "cannot tell when process {pid} started"
                ))),
            },
            Err(e) if missing(&e) || e.raw_os_error() == Some(ESRCH) => Ok(None),
            Err(e) => Err(state_err(&format!("cannot examine process {pid}"), &e)),
        }
    }

    /// Whether the entry is still the process that was registered. Only proof that it is gone makes it stale: a
    /// process that cannot be examined stays protected.
    fn registered(&self, e: &Entry) -> bool {
        match self.start_of(e.pid) {
            Ok(Some(start)) => start == e.start,
            Ok(None) => false,
            Err(_) => true,
        }
    }

    /// The registry, checked against what runs now.
    pub fn critical(&self) -> Result<Critical> {
        let mut c = Critical::default();
        for e in self.registry()? {
            if self.registered(&e) {
                c.live.push(e);
            } else {
                c.stale.push(e);
            }
        }
        Ok(c)
    }

    /// The pids that no kill may reach.
    pub fn critical_pids(&self) -> Result<Vec<i32>> {
        Ok(self.critical()?.live.iter().map(|e| e.pid).collect())
    }

    fn starts(&self) -> Vec<u64> {
        match self.host.read(&self.path(STARTS)) {
            Ok(t) => parse_starts(&t).unwrap_or_else(|why| {
                self.warn_out(format!("the log of starts is corrupt ({why}): ignored"));
                Vec::new()
            }),
            Err(e) if missing(&e) => Vec::new(),
            Err(e) => {
                self.warn_out(format!("cannot read the log of starts: {e}"));
                Vec::new()
            }
        }
    }
}

/// The state, locked: what changes it. It derefs to the store, whose reads need no lock.
pub struct Locked<'s, 'a> {
    store: &'s Store<'a>,
    _guard: Guard,
}

impl<'a> Deref for Locked<'_, 'a> {
    type Target = Store<'a>;

    fn deref(&self) -> &Store<'a> {
        self.store
    }
}

impl Locked<'_, '_> {
    /// The sequence number of the last line, and of the next one. A lost or garbled `seq` file is rebuilt from the
    /// trail; the number is never used twice.
    fn next_seq(&self) -> Result<(u64, u64)> {
        let prev = match self.host.read(&self.path(SEQ)) {
            Ok(t) => t.trim().parse::<u64>().unwrap_or_else(|_| {
                self.warn_out("the audit sequence file is garbled: rebuilt from the trail".into());
                self.last_logged()
            }),
            Err(e) if missing(&e) => self.last_logged(),
            Err(e) => return Err(state_err("cannot read the audit sequence", &e)),
        };
        let next = prev
            .checked_add(1)
            .ok_or_else(|| Error::State("the audit sequence is used up".into()))?;
        Ok((prev, next))
    }

    fn last_logged(&self) -> u64 {
        [AUDIT, AUDIT_OLD]
            .iter()
            .filter_map(|n| self.host.read(&self.path(n)).ok())
            .find_map(|t| t.lines().rev().find_map(|l| Line::parse(l).map(|l| l.seq)))
            .unwrap_or(0)
    }

    /// Add a line to the trail and return its number. The number is written down first: when the line then fails to be
    /// added the trail shows a gap, which is true, instead of a number used twice.
    fn append(&self, event: Event, subject: &str, detail: &str) -> Result<u64> {
        let (prev, seq) = self.next_seq()?;
        self.host
            .put_file(&self.path(SEQ), &seq.to_string())
            .map_err(|e| state_err("cannot write the audit sequence", &e))?;
        if prev / AUDIT_LINES < seq / AUDIT_LINES && self.host.exists(&self.path(AUDIT)) {
            if let Err(e) = self.host.rename(&self.path(AUDIT), &self.path(AUDIT_OLD)) {
                self.warn_out(format!("the audit trail was not rotated: {e}"));
            }
        }
        let line = Line {
            seq,
            at: self.host.epoch_secs(),
            actor: self.actor.clone(),
            event,
            subject: token(subject),
            detail: tidy(detail),
        };
        self.host
            .append_file(&self.path(AUDIT), &format!("{}\n", line.render()))
            .map_err(|e| state_err("cannot add to the audit trail", &e))?;
        Ok(seq)
    }

    /// Add a line to the audit trail. The number of the line; 0 when it could not be added and `a` allows going on.
    pub fn audit(&self, a: Audit, event: Event, subject: &str, detail: &str) -> Result<u64> {
        match self.append(event, subject, detail) {
            Ok(seq) => Ok(seq),
            Err(e) if a == Audit::Required => Err(e),
            Err(e) => {
                self.warn_out(format!("audit trail: {e}"));
                Ok(0)
            }
        }
    }

    /// What was not done, for the trail; it never stops anything.
    pub fn note_failure(&self, subject: &str, what: &str) {
        let _ = self.audit(
            Audit::Optional,
            Event::Warn,
            subject,
            &format!("not done: {what}"),
        );
    }

    /// Audit first, then change; when the change fails the trail says so.
    fn guarded<T>(
        &self,
        a: Audit,
        event: Event,
        subject: &str,
        detail: &str,
        change: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.audit(a, event, subject, detail)?;
        change().inspect_err(|e| self.note_failure(subject, &e.to_string()))
    }

    /// Write the record, and the line about it. A new record is refused when `MAX_RECORDS` exist already. The record
    /// is read back through the parser first: what the store writes, it can read.
    pub fn commit(&self, rec: &mut Record, a: Audit, event: Event, detail: &str) -> Result<()> {
        let w = Workload::parse(&rec.workload)?;
        let path = self.record_path(&w);
        if !self.host.exists(&path) {
            let n = self
                .host
                .files(&self.path(RECORDS))
                .map_err(|e| state_err("cannot list the lifecycle records", &e))?
                .len();
            if n >= MAX_RECORDS {
                return Err(Error::Lifecycle(format!(
                    "{MAX_RECORDS} lifecycle records exist: `resctl reset` or `resctl teardown` removes the ended ones"
                )));
            }
        }
        let was = rec.seq;
        let seq = self.audit(a, event, &rec.workload, detail)?;
        if seq != 0 {
            rec.seq = seq;
        }
        let text = rec.render();
        let put = match Record::parse(&text) {
            Ok(_) => self
                .host
                .put_file(&path, &text)
                .map_err(|e| state_err(&format!("cannot write the record of {w}"), &e)),
            Err(why) => Err(Error::State(format!(
                "the record of {w} would not read back ({why})"
            ))),
        };
        if let Err(e) = &put {
            rec.seq = was;
            self.note_failure(&rec.workload, &e.to_string());
        }
        put
    }

    /// Take the lock of the supervisor of the workload, or `None` when a supervisor holds it. Only the caller that holds
    /// the lock of the state may start or clean up after a supervisor, so nobody else can take it at the same time.
    pub fn claim(&self, w: &Workload) -> Result<Option<Guard>> {
        let path = self.sup_path(w);
        for _ in 0..CLAIM_TRIES {
            match self.host.lock(&path, Lock::Try) {
                Ok(Some(g)) => return Ok(Some(g)),
                Ok(None) => self.host.pause(LOCK_POLL_MS),
                Err(e) => {
                    return Err(state_err(
                        &format!("cannot take the supervisor lock of {w}"),
                        &e,
                    ))
                }
            }
        }
        Ok(None)
    }

    /// Remove the record and the supervisor lock of a workload that has no supervisor. A live supervisor is refused.
    pub fn forget(&self, w: &Workload) -> Result<()> {
        let lock = self.sup_path(w);
        let held = if self.host.exists(&lock) {
            match self.claim(w)? {
                Some(g) => Some(g),
                None => {
                    return Err(Error::Lifecycle(format!(
                        "{w} has a supervisor that is alive: stop it first"
                    )))
                }
            }
        } else {
            None
        };
        for path in [self.record_path(w), lock] {
            match self.host.remove_file(&path) {
                Ok(()) => {}
                Err(e) if missing(&e) => {}
                Err(e) => return Err(state_err(&format!("cannot remove the state of {w}"), &e)),
            }
        }
        drop(held);
        Ok(())
    }

    /// Move a file of the state aside, because it does not read back. `rel` is its path in the state directory.
    pub fn quarantine(&self, rel: &str, why: &str) -> Result<()> {
        if rel.split('/').any(|c| c.is_empty() || c == "..") {
            return Err(Error::Usage(format!("`{}` is not in the state", tidy(rel))));
        }
        let from = self.path(rel);
        let base = rel.rsplit('/').next().unwrap_or(rel);
        let full = self
            .host
            .files(&self.path(QUARANTINE))
            .map_err(|e| state_err("cannot list the quarantine", &e))?
            .len()
            >= MAX_QUARANTINE;
        let seq = self.audit(
            Audit::Optional,
            Event::Quarantine,
            base,
            &format!("{rel}: {why}"),
        )?;
        let moved = if full {
            self.host.remove_file(&from)
        } else {
            self.host
                .rename(&from, &self.path(&format!("{QUARANTINE}/{base}.{seq}")))
        };
        match moved {
            Ok(()) => Ok(()),
            Err(e) if missing(&e) => Ok(()),
            Err(e) => {
                let err = state_err(&format!("cannot move {rel} aside"), &e);
                self.note_failure(base, &err.to_string());
                Err(err)
            }
        }
    }

    fn put_registry(&self, entries: &[Entry]) -> Result<()> {
        self.host
            .put_file(&self.path(CRITICAL), &render_critical(entries))
            .map_err(|e| state_err("cannot write the registry of critical processes", &e))
    }

    /// Register a process as critical. It must exist now; a process that ended or was replaced is dropped from the
    /// registry on the way. The change is audited first and refused when the trail cannot take it.
    pub fn protect(&self, pid: i32, name: &str) -> Result<()> {
        if pid < 2 {
            return Err(Error::Usage(format!(
                "{pid} is not a process that can be protected"
            )));
        }
        if !valid_name(name) {
            return Err(Error::Usage(format!(
                "`{}` is not a usable name (letters, digits, - and _; at most 32)",
                tidy(name)
            )));
        }
        let start = self
            .start_of(pid)?
            .ok_or_else(|| Error::Refused(format!("process {pid} does not exist")))?;
        let mut keep: Vec<Entry> = self
            .registry()?
            .into_iter()
            .filter(|e| e.pid != pid && self.registered(e))
            .collect();
        if keep.len() >= MAX_CRITICAL {
            return Err(Error::Lifecycle(format!(
                "{MAX_CRITICAL} critical processes are registered already"
            )));
        }
        keep.push(Entry {
            pid,
            start,
            name: name.into(),
        });
        keep.sort_by_key(|e| e.pid);
        self.guarded(
            Audit::Required,
            Event::Protect,
            name,
            &format!("pid={pid}"),
            || self.put_registry(&keep),
        )
    }

    /// Take a process out of the registry; whether it was in it.
    pub fn unprotect(&self, pid: i32) -> Result<bool> {
        let all = self.registry()?;
        let Some(gone) = all.iter().find(|e| e.pid == pid).cloned() else {
            return Ok(false);
        };
        let keep: Vec<Entry> = all.into_iter().filter(|e| e.pid != pid).collect();
        self.guarded(
            Audit::Required,
            Event::Unprotect,
            &gone.name,
            &format!("pid={pid}"),
            || self.put_registry(&keep),
        )?;
        Ok(true)
    }

    /// Drop the entries of processes that ended or were replaced; those that were dropped.
    pub fn prune_critical(&self) -> Result<Vec<Entry>> {
        let c = self.critical()?;
        if c.stale.is_empty() {
            return Ok(c.stale);
        }
        let detail = format!(
            "dropped {}",
            c.stale
                .iter()
                .map(|e| format!("{}={}", e.name, e.pid))
                .collect::<Vec<_>>()
                .join(" ")
        );
        self.guarded(Audit::Optional, Event::Recover, "critical", &detail, || {
            self.put_registry(&c.live)
        })?;
        Ok(c.stale)
    }

    /// Write the registry anew, from the processes that stand in the system domain: the repair of a corrupt one.
    pub fn replace_critical(&self, mut entries: Vec<Entry>) -> Result<()> {
        entries.truncate(MAX_CRITICAL);
        entries.sort_by_key(|e| e.pid);
        let detail = format!("{} processes", entries.len());
        self.guarded(Audit::Optional, Event::Recover, "critical", &detail, || {
            self.put_registry(&entries)
        })
    }

    /// Record that a workload started, for the start rate of all workloads together. A start that cannot be recorded is
    /// an error: without the record the rate would not hold.
    pub fn note_start(&self, now_ms: u64) -> Result<()> {
        let mut starts: Vec<u64> = self
            .starts()
            .into_iter()
            .filter(|&t| t <= now_ms && now_ms - t < MINUTE_MS)
            .collect();
        starts.push(now_ms);
        let keep = starts.len().saturating_sub(START_KEEP);
        let text: String = starts[keep..].iter().map(|t| format!("{t}\n")).collect();
        self.host
            .put_file(&self.path(STARTS), &text)
            .map_err(|e| state_err("cannot record the start", &e))
    }

    /// How long until fewer than `rate` starts happened in the last minute (`None`: no wait).
    pub fn start_wait(&self, now_ms: u64, rate: u32) -> Option<u64> {
        start_wait(&self.starts(), now_ms, rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::fake::{FakeHost, EACCES, ESRCH as GONE};
    use crate::life::{Intent, Phase};

    const DIR: &str = "/run/ai";

    fn world() -> FakeHost {
        let h = FakeHost::new();
        h.set_clock(5_000);
        h
    }

    fn open(h: &FakeHost) -> Store<'_> {
        Store::open(h, DIR, "4242:test").unwrap()
    }

    fn w(s: &str) -> Workload {
        Workload::parse(s).unwrap()
    }

    fn rec(s: &str) -> Record {
        Record::new(s, 1_700_000_000)
    }

    fn lines(s: &Store<'_>) -> Vec<Line> {
        s.audit_tail(5000)
            .unwrap()
            .iter()
            .map(|l| Line::parse(l).unwrap_or_else(|| panic!("not a line of the trail: {l}")))
            .collect()
    }

    // ---- the directory

    #[test]
    fn the_store_is_a_private_directory_tree() {
        let h = world();
        let s = open(&h);
        for d in ["", "records", "locks", "quarantine"] {
            let p = if d.is_empty() {
                DIR.to_string()
            } else {
                format!("{DIR}/{d}")
            };
            let o = h.owner(&p).unwrap();
            assert_eq!((o.uid, o.mode, o.dir), (0, 0o700, true), "{p}");
        }
        assert_eq!(s.dir(), DIR);
        assert!(open(&h).list().unwrap().is_empty(), "opening twice is fine");
        assert!(Store::existing(&h, "/run/none", "4242:test")
            .unwrap()
            .is_none());
        assert!(
            !h.dir_exists("/run/none"),
            "a read does not create the store"
        );
    }

    #[test]
    fn a_directory_that_is_not_ours_and_private_is_not_trusted() {
        let h = world();
        h.mkdirs_private(DIR).unwrap();
        h.set_owner(DIR, 1000, 0o700, true);
        let e = Store::open(&h, DIR, "4242:test").err().unwrap();
        assert!(matches!(e, Error::State(m) if m.contains("belongs to user 1000")));

        let h = world();
        h.mkdirs_private(DIR).unwrap();
        h.set_owner(DIR, 0, 0o750, true);
        let e = Store::open(&h, DIR, "4242:test").err().unwrap();
        assert!(matches!(e, Error::State(m) if m.contains("open to other users")));
        assert!(Store::existing(&h, DIR, "4242:test").is_err());

        // a symlink reports dir=false, whoever owns it and wherever it leads
        let h = world();
        h.mkdirs_private(DIR).unwrap();
        h.set_owner(DIR, 0, 0o777, false);
        let e = Store::open(&h, DIR, "4242:test").err().unwrap();
        assert!(matches!(e, Error::State(m) if m.contains("not a directory")));

        // a file where the directory should be
        let h = world();
        h.set(DIR, "x");
        assert!(matches!(
            Store::open(&h, DIR, "4242:test"),
            Err(Error::State(_))
        ));
    }

    #[test]
    fn a_directory_that_belongs_to_another_user_than_the_caller_is_refused_whoever_that_is() {
        let h = world();
        h.set_euid(1000);
        let s = Store::open(&h, DIR, "4242:test").unwrap();
        assert_eq!(h.owner(DIR).unwrap().uid, 1000);
        drop(s);
        h.set_euid(0);
        assert!(Store::open(&h, DIR, "4242:test").is_err());
        h.set_euid(1000);
        assert!(Store::open(&h, DIR, "4242:test").is_ok());
    }

    #[test]
    fn unusable_paths_and_names_are_refused() {
        let h = world();
        for dir in ["", "/", "run/ai", "/run/../etc", "/run/a\ni"] {
            assert!(
                matches!(Store::open(&h, dir, "4242:test"), Err(Error::Usage(_))),
                "{dir:?}"
            );
        }
        let long = "x".repeat(41);
        for actor in ["", "a b", "a\nb", long.as_str()] {
            assert!(
                matches!(Store::open(&h, DIR, actor), Err(Error::Usage(_))),
                "{actor:?}"
            );
        }
        assert!(Store::open(&h, "/run/ai/", "4242:test").is_ok());
    }

    // ---- records

    #[test]
    fn a_record_is_written_after_its_line_and_reads_back() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        let mut r = rec("batch/job1");
        r.advance(Phase::Starting, 1_700_000_001).unwrap();
        r.set_cmd("/usr/bin/job");
        l.commit(
            &mut r,
            Audit::Required,
            Event::Phase,
            "phase=starting gen=1",
        )
        .unwrap();
        assert_eq!(r.seq, 1);
        assert_eq!(s.peek(&w("batch/job1")).unwrap(), Stored::Record(r.clone()));
        let t = lines(&s);
        assert_eq!(t.len(), 1);
        assert_eq!(
            (
                t[0].seq,
                t[0].event,
                t[0].subject.as_str(),
                t[0].detail.as_str()
            ),
            (1, Event::Phase, "batch/job1", "phase=starting gen=1")
        );
        assert_eq!(t[0].actor, "4242:test");
        assert_eq!(t[0].at, 1_700_000_005);
        let o = h.owner(&format!("{DIR}/records/batch.job1")).unwrap();
        assert_eq!((o.uid, o.mode), (0, 0o600));

        r.ask(Intent::Stop);
        l.commit(&mut r, Audit::Required, Event::Intent, "stop")
            .unwrap();
        assert_eq!(r.seq, 2);
        match s.peek(&w("batch/job1")).unwrap() {
            Stored::Record(back) => assert_eq!(back.intent, Some(Intent::Stop)),
            other => panic!("{other:?}"),
        }
        assert_eq!(s.peek(&w("batch/other")).unwrap(), Stored::Missing);
    }

    #[test]
    fn a_record_that_does_not_read_back_is_corrupt_and_nothing_panics() {
        let h = world();
        let s = open(&h);
        let name = |n: &str| format!("{DIR}/records/{n}");
        h.set(&name("batch.a"), "garbage");
        h.set(&name("batch.b"), "");
        h.set(&name("batch.c"), &rec("batch/other").render());
        h.set(&name("batch.d"), &"x=1\n".repeat(2000));
        h.set(
            &name("batch.e"),
            &format!("{}phase=running\n", rec("batch/e").render()),
        );
        for n in ["a", "b", "c", "d", "e"] {
            assert!(
                matches!(
                    s.peek(&w(&format!("batch/{n}"))).unwrap(),
                    Stored::Corrupt(_)
                ),
                "{n}"
            );
        }
        match s.peek(&w("batch/c")).unwrap() {
            Stored::Corrupt(why) => assert!(why.contains("batch/other"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_list_names_every_file_including_the_leftovers_of_an_interrupted_write() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        let mut r = rec("batch/job1");
        l.commit(&mut r, Audit::Required, Event::Create, "")
            .unwrap();
        h.set(&format!("{DIR}/records/batch.job2.tmp77"), "half");
        h.set(&format!("{DIR}/records/system.x"), "no such class");
        h.set(&format!("{DIR}/records/batch.job3"), "garbage");
        let all = s.list().unwrap();
        let names: Vec<&str> = all.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(
            names,
            ["batch.job1", "batch.job2.tmp77", "batch.job3", "system.x"]
        );
        assert!(matches!(all[0].stored, Stored::Record(_)));
        assert_eq!(all[0].workload, Some(w("batch/job1")));
        assert!(all[1].workload.is_none() && matches!(all[1].stored, Stored::Corrupt(_)));
        assert!(all[2].workload.is_some() && matches!(all[2].stored, Stored::Corrupt(_)));
        assert!(all[3].workload.is_none());
    }

    #[test]
    fn records_are_capped() {
        let h = world();
        let s = open(&h);
        for i in 0..MAX_RECORDS {
            h.set(&format!("{DIR}/records/batch.j{i}"), "x");
        }
        let l = s.lock().unwrap();
        let mut r = rec("batch/new");
        let e = l
            .commit(&mut r, Audit::Required, Event::Create, "")
            .unwrap_err();
        assert!(matches!(e, Error::Lifecycle(m) if m.contains("256")));
        assert!(lines(&s).is_empty(), "a refused record leaves no line");
        // an existing record can still be changed
        let mut old = rec("batch/j3");
        l.commit(&mut old, Audit::Required, Event::Phase, "again")
            .unwrap();
    }

    #[test]
    fn a_record_that_would_not_read_back_is_not_written() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        let mut r = rec("batch/job1");
        r.stop = Some("two words".into());
        let e = l
            .commit(&mut r, Audit::Required, Event::Phase, "x")
            .unwrap_err();
        assert!(matches!(e, Error::State(m) if m.contains("would not read back")));
        assert_eq!(s.peek(&w("batch/job1")).unwrap(), Stored::Missing);
        assert_eq!(r.seq, 0, "the record is as it was");
        let t = lines(&s);
        assert_eq!(t.last().unwrap().event, Event::Warn);
        let mut bad = rec("not a workload");
        assert!(matches!(
            l.commit(&mut bad, Audit::Required, Event::Phase, "x"),
            Err(Error::Usage(_))
        ));
    }

    // ---- the order of audit and change

    #[test]
    fn a_change_that_must_be_audited_waits_for_its_line() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        h.deny_puts_to("audit.log");
        let mut r = rec("batch/job1");
        let e = l
            .commit(&mut r, Audit::Required, Event::Create, "")
            .unwrap_err();
        assert!(matches!(e, Error::Denied(_)), "{e}");
        assert_eq!(s.peek(&w("batch/job1")).unwrap(), Stored::Missing);
        // protecting a process is such a change too: no line, no entry
        h.proc_in(100, &[]);
        let e = l.protect(100, "init").unwrap_err();
        assert!(matches!(e, Error::Denied(_)), "{e}");
        assert!(s.registry().unwrap().is_empty());
        h.allow_puts();
        l.protect(100, "init").unwrap();
        assert_eq!(s.registry().unwrap().len(), 1);
    }

    #[test]
    fn a_change_that_may_not_wait_goes_ahead_and_the_warning_is_kept() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        h.deny_puts_to("audit.log");
        let mut r = rec("batch/job1");
        l.commit(&mut r, Audit::Optional, Event::Phase, "phase=stopped")
            .unwrap();
        assert_eq!(r.seq, 0, "no line, no number");
        assert!(matches!(
            s.peek(&w("batch/job1")).unwrap(),
            Stored::Record(_)
        ));
        let warned = s.take_warnings();
        assert!(
            warned.iter().any(|m| m.contains("audit trail")),
            "{warned:?}"
        );
        assert!(s.take_warnings().is_empty(), "warnings are taken once");
    }

    #[test]
    fn a_failed_change_leaves_a_line_that_says_so_and_the_old_record() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        let mut r = rec("batch/job1");
        l.commit(&mut r, Audit::Required, Event::Create, "")
            .unwrap();
        h.deny_puts_to("records/batch.job1");
        r.advance(Phase::Starting, 1_700_000_002).unwrap();
        let e = l
            .commit(&mut r, Audit::Required, Event::Phase, "phase=starting")
            .unwrap_err();
        assert!(matches!(e, Error::Denied(_)));
        assert_eq!(
            r.seq, 1,
            "the record keeps the number of the last change that was made"
        );
        match s.peek(&w("batch/job1")).unwrap() {
            Stored::Record(old) => assert_eq!(old.phase, Phase::Created),
            other => panic!("{other:?}"),
        }
        let t = lines(&s);
        assert_eq!(
            t.iter().map(|l| l.event).collect::<Vec<_>>(),
            [Event::Create, Event::Phase, Event::Warn]
        );
        assert!(t[2].detail.starts_with("not done"), "{}", t[2].detail);
        assert_eq!(t[2].subject, "batch/job1");
    }

    // ---- the audit trail

    #[test]
    fn a_line_reads_back_and_text_from_outside_stays_on_its_line() {
        let l = Line {
            seq: 12,
            at: 1_700_000_000,
            actor: "7:supervisor".into(),
            event: Event::Exit,
            subject: "batch/job1".into(),
            detail: "crashed:11 oom=0".into(),
        };
        assert_eq!(Line::parse(&l.render()), Some(l.clone()));
        let bare = Line {
            detail: String::new(),
            ..l
        };
        assert_eq!(bare.render(), "12 1700000000 7:supervisor exit batch/job1");
        assert_eq!(Line::parse(&bare.render()), Some(bare));
        for bad in [
            "",
            "x 1 a exit s",
            "1 x a exit s",
            "1 2 a nope s",
            "1 2 a exit",
            "1 2  exit s",
            "1 2 a exit  d",
        ] {
            assert_eq!(Line::parse(bad), None, "{bad:?}");
        }

        let h = world();
        let s = open(&h);
        let lk = s.lock().unwrap();
        lk.audit(
            Audit::Required,
            Event::Refuse,
            "a b\nc",
            "x\n99 1 evil exit forged y\u{1b}[31m",
        )
        .unwrap();
        let raw = s.audit_tail(10).unwrap();
        assert_eq!(raw.len(), 1, "{raw:?}");
        let parsed = Line::parse(&raw[0]).unwrap();
        assert_eq!((parsed.seq, parsed.event), (1, Event::Refuse));
        assert_eq!(parsed.subject, "a_b?c");
        assert!(raw[0].chars().all(|c| (' '..='~').contains(&c)), "{raw:?}");
        lk.audit(Audit::Required, Event::Refuse, "", "").unwrap();
        assert_eq!(lines(&s)[1].subject, "-");
    }

    #[test]
    fn every_word_of_the_vocabulary_is_unique_and_reads_back() {
        let mut seen = std::collections::BTreeSet::new();
        for e in Event::ALL {
            assert!(seen.insert(e.name()), "{} twice", e.name());
            assert_eq!(Event::parse(e.name()), Some(e));
        }
        assert_eq!(seen.len(), 24);
        assert_eq!(Event::parse("nope"), None);
        assert_eq!(Event::parse("Stop"), None);
    }

    #[test]
    fn the_trail_numbers_its_lines_and_rotates_without_a_gap() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        for i in 0..2100 {
            l.audit(Audit::Required, Event::Phase, "batch/a", &format!("n={i}"))
                .unwrap();
        }
        assert!(h.file(&format!("{DIR}/audit.log")).is_some());
        assert!(h.file(&format!("{DIR}/audit.log.1")).is_some());
        let t = lines(&s);
        assert!(
            (AUDIT_LINES as usize..2 * AUDIT_LINES as usize).contains(&t.len()),
            "{} lines kept",
            t.len()
        );
        assert_eq!(t.last().unwrap().seq, 2100);
        for pair in t.windows(2) {
            assert_eq!(pair[1].seq, pair[0].seq + 1, "a gap in the trail");
        }
        let tail = s.audit_tail(3).unwrap();
        assert_eq!(tail.len(), 3);
        assert!(tail[2].ends_with("n=2099"), "{tail:?}");
        assert!(tail[0].ends_with("n=2097"), "{tail:?}");
        assert_eq!(s.audit_tail(0).unwrap().len(), 0);
    }

    #[test]
    fn a_lost_or_garbled_sequence_file_never_makes_a_number_twice() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        for _ in 0..5 {
            l.audit(Audit::Required, Event::Phase, "batch/a", "x")
                .unwrap();
        }
        h.remove_file(&format!("{DIR}/seq")).unwrap();
        assert_eq!(
            l.audit(Audit::Required, Event::Phase, "batch/a", "x")
                .unwrap(),
            6
        );
        h.set(&format!("{DIR}/seq"), "garbage");
        assert_eq!(
            l.audit(Audit::Required, Event::Phase, "batch/a", "x")
                .unwrap(),
            7
        );
        assert!(s.take_warnings().iter().any(|m| m.contains("garbled")));
        let seqs: Vec<u64> = lines(&s).iter().map(|l| l.seq).collect();
        assert_eq!(seqs, [1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn a_line_that_could_not_be_added_leaves_a_gap_and_its_number_is_not_reused() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        l.audit(Audit::Required, Event::Phase, "batch/a", "one")
            .unwrap();
        h.deny_puts_to("audit.log");
        assert!(l
            .audit(Audit::Required, Event::Phase, "batch/a", "two")
            .is_err());
        h.allow_puts();
        assert_eq!(
            l.audit(Audit::Required, Event::Phase, "batch/a", "three")
                .unwrap(),
            3
        );
        let seqs: Vec<u64> = lines(&s).iter().map(|l| l.seq).collect();
        assert_eq!(seqs, [1, 3]);
    }

    #[test]
    fn a_trail_that_cannot_rotate_goes_on_growing_and_says_so() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        let trail = format!("{DIR}/audit.log");
        h.set(&format!("{DIR}/seq"), &(AUDIT_LINES - 1).to_string());
        h.set(&trail, "");
        h.deny_renames_to("audit.log.1");
        assert_eq!(
            l.audit(Audit::Required, Event::Phase, "batch/a", "x")
                .unwrap(),
            AUDIT_LINES,
            "the change is not held up by the trail that cannot rotate"
        );
        let warned = s.take_warnings();
        assert!(
            warned.iter().any(|m| m.contains("not rotated")),
            "{warned:?}"
        );
        assert!(h.file(&format!("{DIR}/audit.log.1")).is_none());
        assert_eq!(lines(&s).len(), 1, "the line went to the trail there is");
        assert!(h.file(&trail).is_some_and(|t| !t.is_empty()));
    }

    // ---- locks

    #[test]
    fn a_second_change_waits_a_bounded_time_for_the_first() {
        let h = world();
        let s = open(&h);
        let first = s.lock().unwrap();
        let t0 = h.clock();
        let e = s.lock().err().unwrap();
        assert!(matches!(e, Error::State(m) if m.contains("stayed locked")));
        assert!(h.clock() - t0 >= LOCK_WAIT_MS, "it waited for the lock");
        assert!(h.clock() - t0 < LOCK_WAIT_MS + 100, "and not much longer");
        drop(first);
        assert!(s.lock().is_ok());
    }

    #[test]
    fn the_supervisor_lock_tells_alive_from_lost_from_absent() {
        let h = world();
        let s = open(&h);
        let job = w("batch/job1");
        assert_eq!(s.sup_state(&job).unwrap(), Sup::Absent);
        assert!(
            !h.dir_exists(&format!("{DIR}/locks/batch.job1.sup"))
                && h.file(&format!("{DIR}/locks/batch.job1.sup")).is_none(),
            "a probe does not create the lock"
        );
        let held = {
            let l = s.lock().unwrap();
            l.claim(&job).unwrap().unwrap()
        };
        assert_eq!(s.sup_state(&job).unwrap(), Sup::Alive);
        {
            let l = s.lock().unwrap();
            assert!(l.claim(&job).unwrap().is_none(), "one supervisor only");
        }
        drop(held);
        assert_eq!(s.sup_state(&job).unwrap(), Sup::Lost);
        let l = s.lock().unwrap();
        assert!(l.claim(&job).unwrap().is_some());
    }

    #[test]
    fn a_claim_gives_up_after_a_bounded_number_of_tries_while_a_probe_holds_the_lock() {
        let h = world();
        let s = open(&h);
        let job = w("batch/job1");
        let l = s.lock().unwrap();
        drop(l.claim(&job).unwrap().unwrap());
        // a reader that is probing holds a shared lock at that very moment
        let probe = h
            .lock(&format!("{DIR}/locks/batch.job1.sup"), Lock::TryShared)
            .unwrap()
            .unwrap();
        let t0 = h.clock();
        assert!(l.claim(&job).unwrap().is_none());
        assert_eq!(h.clock() - t0, CLAIM_TRIES as u64 * LOCK_POLL_MS);
        drop(probe);
        assert!(l.claim(&job).unwrap().is_some());
    }

    #[test]
    fn the_wait_for_a_supervisor_ends_when_it_does() {
        let h = world();
        let s = open(&h);
        let job = w("batch/job1");
        let held = s.lock().unwrap().claim(&job).unwrap().unwrap();
        // the fake host cannot wait (nobody else runs): a busy lock is an error there
        assert!(s.wait_sup(&job).is_err());
        drop(held);
        let taken = s.wait_sup(&job).unwrap();
        assert_eq!(s.sup_state(&job).unwrap(), Sup::Alive);
        drop(taken);
    }

    #[test]
    fn forget_removes_the_state_of_a_workload_without_a_supervisor_only() {
        let h = world();
        let s = open(&h);
        let job = w("batch/job1");
        let l = s.lock().unwrap();
        let mut r = rec("batch/job1");
        l.commit(&mut r, Audit::Required, Event::Create, "")
            .unwrap();
        let held = l.claim(&job).unwrap().unwrap();
        let e = l.forget(&job).unwrap_err();
        assert!(matches!(e, Error::Lifecycle(m) if m.contains("alive")));
        assert!(matches!(s.peek(&job).unwrap(), Stored::Record(_)));
        drop(held);
        l.forget(&job).unwrap();
        assert_eq!(s.peek(&job).unwrap(), Stored::Missing);
        assert_eq!(s.sup_state(&job).unwrap(), Sup::Absent);
        l.forget(&job).unwrap();
    }

    // ---- quarantine

    #[test]
    fn a_file_that_does_not_read_back_is_kept_aside_with_its_line() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        h.set(&format!("{DIR}/records/batch.job1"), "garbage");
        l.quarantine("records/batch.job1", "line 1: not key=value")
            .unwrap();
        assert_eq!(s.peek(&w("batch/job1")).unwrap(), Stored::Missing);
        assert_eq!(
            h.file(&format!("{DIR}/quarantine/batch.job1.1")).as_deref(),
            Some("garbage")
        );
        let t = lines(&s);
        assert_eq!(t[0].event, Event::Quarantine);
        assert!(
            t[0].detail.contains("records/batch.job1"),
            "{}",
            t[0].detail
        );
        l.quarantine("records/batch.job1", "again").unwrap();
        assert!(matches!(
            l.quarantine("../etc/passwd", "no"),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            l.quarantine("records/", "no"),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn the_quarantine_is_bounded() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        for i in 0..MAX_QUARANTINE + 5 {
            h.set(&format!("{DIR}/records/batch.j{i}"), "garbage");
            l.quarantine(&format!("records/batch.j{i}"), "bad").unwrap();
        }
        let kept = h.files_of(&format!("{DIR}/quarantine"));
        assert_eq!(kept.len(), MAX_QUARANTINE);
        assert!(
            h.files_of(&format!("{DIR}/records")).is_empty(),
            "every bad file left the records"
        );
    }

    // ---- the registry of critical processes

    #[test]
    fn a_process_is_protected_with_its_start_time_and_the_change_is_audited() {
        let h = world();
        let s = open(&h);
        h.proc_in(100, &[]);
        h.set_start(100, 777);
        let l = s.lock().unwrap();
        l.protect(100, "dbus").unwrap();
        let c = s.critical().unwrap();
        assert_eq!(
            c.live,
            [Entry {
                pid: 100,
                start: 777,
                name: "dbus".into()
            }]
        );
        assert!(c.stale.is_empty());
        assert_eq!(s.critical_pids().unwrap(), [100]);
        let t = lines(&s);
        assert_eq!(
            (t[0].event, t[0].subject.as_str(), t[0].detail.as_str()),
            (Event::Protect, "dbus", "pid=100")
        );
        assert_eq!(
            h.file(&format!("{DIR}/critical")).as_deref(),
            Some("v=1\n100 777 dbus\n")
        );
        let o = h.owner(&format!("{DIR}/critical")).unwrap();
        assert_eq!((o.uid, o.mode), (0, 0o600));
        assert!(l.unprotect(100).unwrap());
        assert!(!l.unprotect(100).unwrap());
        assert!(s.critical().unwrap().live.is_empty());
        assert_eq!(lines(&s).last().unwrap().event, Event::Unprotect);
    }

    #[test]
    fn a_pid_taken_over_by_another_process_is_not_protected_any_more() {
        let h = world();
        let s = open(&h);
        h.proc_in(100, &[]);
        h.proc_in(101, &[]);
        h.proc_in(102, &[]);
        let l = s.lock().unwrap();
        for (pid, name) in [(100, "a"), (101, "b"), (102, "c")] {
            l.protect(pid, name).unwrap();
        }
        h.reuse_pid(100);
        h.exit_proc(101);
        let c = s.critical().unwrap();
        assert_eq!(c.live.iter().map(|e| e.pid).collect::<Vec<_>>(), [102]);
        assert_eq!(
            c.stale.iter().map(|e| e.pid).collect::<Vec<_>>(),
            [100, 101]
        );
        assert_eq!(s.critical_pids().unwrap(), [102]);

        let dropped = l.prune_critical().unwrap();
        assert_eq!(dropped.len(), 2);
        assert_eq!(s.registry().unwrap().len(), 1);
        assert!(l.prune_critical().unwrap().is_empty());
        let last = lines(&s).pop().unwrap();
        assert_eq!(last.event, Event::Recover);
        assert!(last.detail.contains("a=100") && last.detail.contains("b=101"));
    }

    #[test]
    fn registering_a_process_again_replaces_its_entry_and_drops_the_stale_ones() {
        let h = world();
        let s = open(&h);
        h.proc_in(100, &[]);
        h.proc_in(101, &[]);
        let l = s.lock().unwrap();
        l.protect(100, "old").unwrap();
        l.protect(101, "b").unwrap();
        h.reuse_pid(100);
        l.protect(100, "new").unwrap();
        let r = s.registry().unwrap();
        assert_eq!(
            r.iter()
                .map(|e| (e.pid, e.name.as_str()))
                .collect::<Vec<_>>(),
            [(100, "new"), (101, "b")]
        );
        h.exit_proc(101);
        h.proc_in(103, &[]);
        l.protect(103, "c").unwrap();
        assert_eq!(
            s.registry()
                .unwrap()
                .iter()
                .map(|e| e.pid)
                .collect::<Vec<_>>(),
            [100, 103]
        );
    }

    #[test]
    fn what_cannot_be_protected_is_refused() {
        let h = world();
        let s = open(&h);
        h.proc_in(100, &[]);
        let l = s.lock().unwrap();
        for pid in [0, 1, -1, -100] {
            assert!(matches!(l.protect(pid, "x"), Err(Error::Usage(_))), "{pid}");
        }
        let long = "x".repeat(33);
        for name in ["", "-x", "a b", "a\n", long.as_str()] {
            assert!(
                matches!(l.protect(100, name), Err(Error::Usage(_))),
                "{name:?}"
            );
        }
        assert!(
            matches!(l.protect(999, "x"), Err(Error::Refused(m)) if m.contains("does not exist"))
        );
        assert!(lines(&s).is_empty(), "a refusal leaves no registration");
        assert!(s.registry().unwrap().is_empty());
    }

    #[test]
    fn the_registry_is_capped() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        for i in 0..MAX_CRITICAL as i32 {
            h.proc_in(200 + i, &[]);
            l.protect(200 + i, &format!("p{i}")).unwrap();
        }
        h.proc_in(900, &[]);
        assert!(matches!(l.protect(900, "extra"), Err(Error::Lifecycle(_))));
        h.exit_proc(200);
        l.protect(900, "extra").unwrap();
        assert_eq!(s.registry().unwrap().len(), MAX_CRITICAL);
    }

    #[test]
    fn a_registry_that_does_not_read_back_stops_everything_that_would_kill() {
        let h = world();
        let s = open(&h);
        h.proc_in(100, &[]);
        let reg = format!("{DIR}/critical");
        for bad in [
            "garbage",
            "",
            "v=2\n",
            "v=1\n100 5\n",
            "v=1\n100 5 a b\n",
            "v=1\n1 5 init\n",
            "v=1\nx 5 a\n",
            "v=1\n100 x a\n",
            "v=1\n100 5 -a\n",
            "v=1\n100 5 a\n100 6 b\n",
            "v=1\n\n",
        ] {
            h.set(&reg, bad);
            let e = s.critical().unwrap_err();
            assert!(
                matches!(&e, Error::State(m) if m.contains("recover")),
                "{bad:?}: {e}"
            );
            assert!(s.critical_pids().is_err(), "{bad:?}");
            assert!(s.registry_corrupt().unwrap().is_some(), "{bad:?}");
            let l = s.lock().unwrap();
            assert!(l.protect(100, "x").is_err(), "{bad:?}");
            assert!(l.unprotect(100).is_err(), "{bad:?}");
        }
        let many = format!("v=1\n{}", "200 5 a\n".repeat(MAX_CRITICAL + 1));
        h.set(&reg, &many);
        assert!(s.registry_corrupt().unwrap().is_some());
        h.set(&reg, &format!("v=1\n{}", "x".repeat(STATE_BYTES)));
        assert!(s.registry_corrupt().unwrap().is_some());
    }

    #[test]
    fn a_corrupt_registry_is_set_aside_and_rebuilt() {
        let h = world();
        let s = open(&h);
        h.proc_in(100, &[]);
        h.set(&format!("{DIR}/critical"), "garbage");
        let l = s.lock().unwrap();
        let why = s.registry_corrupt().unwrap().unwrap();
        l.quarantine("critical", &why).unwrap();
        assert!(s.registry().unwrap().is_empty());
        l.replace_critical(vec![
            Entry {
                pid: 100,
                start: 1100,
                name: "system".into(),
            },
            Entry {
                pid: 5,
                start: 1005,
                name: "system".into(),
            },
        ])
        .unwrap();
        assert!(s.registry_corrupt().unwrap().is_none());
        assert_eq!(
            s.registry()
                .unwrap()
                .iter()
                .map(|e| e.pid)
                .collect::<Vec<_>>(),
            [5, 100]
        );
        assert!(h
            .files_of(&format!("{DIR}/quarantine"))
            .iter()
            .any(|n| n.starts_with("critical.")));
    }

    #[test]
    fn only_proof_that_a_process_is_gone_makes_its_entry_stale() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        for (pid, name) in [
            (100, "a"),
            (101, "b"),
            (102, "c"),
            (103, "d"),
            (104, "e"),
            (105, "f"),
        ] {
            h.proc_in(pid, &[]);
            l.protect(pid, name).unwrap();
        }
        h.fail_stat_of(100, EACCES); // it lives, but nothing can be learnt of it
        h.garble_stat_of(101); // /proc answers with something that is no stat line
        h.fail_stat_of(102, GONE); // it ended while its stat was being read
        h.reuse_pid(103); // the number belongs to another process now
        h.exit_proc(104); // gone
        let c = s.critical().unwrap();
        let pids = |v: &[Entry]| v.iter().map(|e| e.pid).collect::<Vec<_>>();
        assert_eq!(
            pids(&c.live),
            [100, 101, 105],
            "no proof that these are gone"
        );
        assert_eq!(pids(&c.stale), [102, 103, 104]);
        assert_eq!(s.critical_pids().unwrap(), [100, 101, 105]);
        // what cannot be examined cannot be registered either
        h.proc_in(106, &[]);
        h.fail_stat_of(106, EACCES);
        assert!(matches!(l.protect(106, "g"), Err(Error::Denied(_))));
        h.garble_stat_of(107);
        h.proc_in(107, &[]);
        assert!(matches!(l.protect(107, "h"), Err(Error::State(_))));
        assert_eq!(pids(&s.registry().unwrap()), [100, 101, 102, 103, 104, 105]);
        // registering a live process drops the entries that are provably gone, and keeps those that cannot be examined
        h.proc_in(108, &[]);
        l.protect(108, "i").unwrap();
        assert_eq!(pids(&s.registry().unwrap()), [100, 101, 105, 108]);
    }

    // ---- the log of starts

    #[test]
    fn the_start_rate_counts_the_starts_of_the_last_minute() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        assert_eq!(l.start_wait(3_000, 2), None);
        l.note_start(1_000).unwrap();
        l.note_start(2_000).unwrap();
        assert_eq!(l.start_wait(3_000, 3), None);
        assert_eq!(l.start_wait(3_000, 2), Some(58_000));
        assert_eq!(l.start_wait(3_000, 1), Some(59_000));
        assert_eq!(
            l.start_wait(61_000, 2),
            None,
            "the older start left the minute: one start is below a rate of two"
        );
        assert_eq!(
            l.start_wait(61_000, 1),
            Some(1_000),
            "the newer one is still in the minute"
        );
        assert_eq!(
            l.start_wait(62_000, 1),
            None,
            "a minute later it is free again"
        );
        assert_eq!(l.start_wait(62_000, 0), None);
    }

    #[test]
    fn the_log_of_starts_keeps_the_newest_and_forgets_the_old() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        for i in 0..200u64 {
            l.note_start(10_000 + i).unwrap();
        }
        let text = h.file(&format!("{DIR}/starts")).unwrap();
        assert_eq!(text.lines().count(), START_KEEP);
        assert!(text.lines().last().unwrap() == "10199");
        l.note_start(200_000).unwrap();
        assert_eq!(h.file(&format!("{DIR}/starts")).unwrap(), "200000\n");
    }

    #[test]
    fn a_log_of_starts_that_does_not_read_back_counts_as_empty_and_is_reported() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        let huge = "1\n".repeat(STATE_BYTES);
        for bad in ["garbage\n", "1\n-2\n", huge.as_str()] {
            h.set(&format!("{DIR}/starts"), bad);
            assert_eq!(l.start_wait(1_000, 1), None);
            assert!(
                s.take_warnings()
                    .iter()
                    .any(|m| m.contains("log of starts")),
                "{:?}",
                &bad[..bad.len().min(10)]
            );
            l.note_start(5_000).unwrap();
            assert_eq!(h.file(&format!("{DIR}/starts")).unwrap(), "5000\n");
        }
    }

    #[test]
    fn a_start_that_cannot_be_recorded_is_an_error() {
        let h = world();
        let s = open(&h);
        let l = s.lock().unwrap();
        h.deny_puts_to("starts");
        assert!(l.note_start(1_000).is_err());
    }
}
