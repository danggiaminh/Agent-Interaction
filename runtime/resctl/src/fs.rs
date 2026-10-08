//! The host the controller runs on: files, directories, locks, signals and the clock. `RealHost` is the machine;
//! `fake::FakeHost` (tests only) models the cgroup rules of the kernel, so the control logic is tested without
//! privileges and the rules that bit it on real kernels stay pinned (no internal processes, empty cpusets, memsw
//! ordering, frozen tasks).
//!
//! The crate contains no `unsafe` code: a signal goes out through the kill tool of the system (the only way the standard
//! library offers to signal a process that is not its own child), and the locks are the ones of `std::fs::File`.

use std::any::Any;
use std::fs::{self, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How a lock is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lock {
    /// Exclusive; wait until it is held.
    Wait,
    /// Exclusive, or `None` when somebody holds the lock.
    Try,
    /// Shared with other readers, or `None` when somebody holds it exclusively.
    TryShared,
}

/// A held lock. It is released when the guard is dropped, and by the kernel when the process ends however it ends.
pub struct Guard {
    _held: Box<dyn Any>,
}

impl Guard {
    pub fn new<T: 'static>(held: T) -> Guard {
        Guard {
            _held: Box::new(held),
        }
    }
}

/// Who owns a file or directory, and with which permission bits (`mode & 0o7777`). `dir` is false for a symlink: the
/// ownership of the link itself is what is reported, never that of its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub mode: u32,
    pub dir: bool,
}

pub trait Host {
    fn read(&self, path: &str) -> io::Result<String>;
    /// Write to a file that exists (a control file is never created or truncated by a write).
    fn write(&self, path: &str, data: &str) -> io::Result<()>;
    fn mkdir(&self, path: &str) -> io::Result<()>;
    fn rmdir(&self, path: &str) -> io::Result<()>;
    fn exists(&self, path: &str) -> bool;
    /// Names of the sub-directories, sorted.
    fn subdirs(&self, path: &str) -> io::Result<Vec<String>>;
    /// Major and minor number of a block device.
    fn devno(&self, path: &str) -> io::Result<(u32, u32)>;
    /// Send SIGKILL (9), SIGTERM (15) or the existence probe (0) to every process of `pids`; none below 2 is ever signalled.
    /// Every process is tried; the error is that of the first one that could not be signalled (usually: gone already).
    fn signal(&self, pids: &[i32], sig: i32) -> io::Result<()>;
    /// Wait; every timeout of the controller is counted in these waits.
    fn pause(&self, ms: u64);

    /// Milliseconds on a clock that never goes back and is shared by every process of the machine (the uptime).
    fn now_ms(&self) -> u64;
    /// Seconds since 1970, for the stamps of the audit trail.
    fn epoch_secs(&self) -> u64;
    /// The effective user id of this process.
    fn euid(&self) -> u32;
    /// Owner and permission bits of a file or directory (a symlink is not followed).
    fn owner(&self, path: &str) -> io::Result<Owner>;
    /// Create the directory and its missing parents, readable by the owner only.
    fn mkdirs_private(&self, path: &str) -> io::Result<()>;
    /// Replace the file in one step (a reader sees the old or the new content, never a mix); the owner alone may read it.
    fn put_file(&self, path: &str, data: &str) -> io::Result<()>;
    /// Add to the end of the file, creating it for the owner alone.
    fn append_file(&self, path: &str, data: &str) -> io::Result<()>;
    fn remove_file(&self, path: &str) -> io::Result<()>;
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    /// Names of the files that are not directories, sorted.
    fn files(&self, dir: &str) -> io::Result<Vec<String>>;
    /// Lock the file (created for the owner alone when it is missing). `None`: a `Try` lock somebody holds.
    fn lock(&self, path: &str, how: Lock) -> io::Result<Option<Guard>>;
}

/// The kill tools tried, in this order. Absolute paths only: a search of PATH would let the environment pick the tool.
const KILL_TOOLS: [&str; 2] = ["/bin/kill", "/usr/bin/kill"];
/// Process ids per run of the kill tool, so that no command line grows with the size of a domain.
const KILL_BATCH: usize = 256;

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// The arguments of the kill tool for `pids` and `sig`. A process id below 2 would signal a process group, every process
/// or the init process, and a signal other than 0, 9 and 15 is not one this crate ever has a use for.
pub fn kill_argv(pids: &[i32], sig: i32) -> io::Result<Vec<String>> {
    if !matches!(sig, 0 | 9 | 15) {
        return Err(invalid(format!("signal {sig} is not one of 0, 9 and 15")));
    }
    if let Some(p) = pids.iter().find(|&&p| p < 2) {
        return Err(invalid(format!(
            "{p} is not a process that may be signalled"
        )));
    }
    let mut argv = vec![format!("-{sig}")];
    argv.extend(pids.iter().map(i32::to_string));
    Ok(argv)
}

/// The effective user id from the text of `/proc/self/status`.
pub fn euid_of(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Milliseconds from the first field of `/proc/uptime`.
pub fn uptime_ms(text: &str) -> Option<u64> {
    let secs = text.split_whitespace().next()?;
    let (whole, frac) = secs.split_once('.').unwrap_or((secs, "0"));
    let frac: String = frac.chars().chain("000".chars()).take(3).collect();
    Some(whole.parse::<u64>().ok()?.checked_mul(1000)? + frac.parse::<u64>().ok()?)
}

/// The start time of a process (field 22 of `/proc/PID/stat`, in clock ticks since boot). The name of the process, the
/// second field, may hold spaces and brackets, so the fields are counted from its last closing bracket.
pub fn starttime_of(stat: &str) -> Option<u64> {
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn held(r: Result<(), TryLockError>) -> io::Result<bool> {
    match r {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

pub struct RealHost;

impl Host for RealHost {
    fn read(&self, path: &str) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn write(&self, path: &str, data: &str) -> io::Result<()> {
        OpenOptions::new()
            .write(true)
            .open(path)?
            .write_all(data.as_bytes())
    }

    fn mkdir(&self, path: &str) -> io::Result<()> {
        fs::create_dir(path)
    }

    fn rmdir(&self, path: &str) -> io::Result<()> {
        fs::remove_dir(path)
    }

    fn exists(&self, path: &str) -> bool {
        fs::metadata(path).is_ok()
    }

    fn subdirs(&self, path: &str) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    fn devno(&self, path: &str) -> io::Result<(u32, u32)> {
        let md = fs::metadata(path)?;
        if !md.file_type().is_block_device() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path} is not a block device"),
            ));
        }
        let r = md.rdev();
        let major = ((r >> 8) & 0xfff) | ((r >> 32) & !0xfff);
        let minor = (r & 0xff) | ((r >> 12) & !0xff);
        Ok((major as u32, minor as u32))
    }

    fn signal(&self, pids: &[i32], sig: i32) -> io::Result<()> {
        kill_argv(pids, sig)?;
        if pids.is_empty() {
            return Ok(());
        }
        let tool = KILL_TOOLS
            .iter()
            .find(|t| Path::new(t).is_file())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "no kill tool at /bin/kill or /usr/bin/kill",
                )
            })?;
        let mut first = None;
        for chunk in pids.chunks(KILL_BATCH) {
            let out = Command::new(tool)
                .args(kill_argv(chunk, sig)?)
                .env_clear()
                .stdin(Stdio::null())
                .output()?;
            if !out.status.success() && first.is_none() {
                let said = String::from_utf8_lossy(&out.stderr);
                first = Some(io::Error::other(format!("kill -{sig}: {}", said.trim())));
            }
        }
        first.map_or(Ok(()), Err)
    }

    fn pause(&self, ms: u64) {
        thread::sleep(Duration::from_millis(ms));
    }

    fn now_ms(&self) -> u64 {
        static START: OnceLock<Instant> = OnceLock::new();
        match fs::read_to_string("/proc/uptime")
            .ok()
            .as_deref()
            .and_then(uptime_ms)
        {
            Some(ms) => ms,
            // no /proc: the controller cannot work anyway; this keeps one process's waits sane
            None => u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis())
                .unwrap_or(u64::MAX),
        }
    }

    fn epoch_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn euid(&self) -> u32 {
        // unreadable: root, which is the stricter reading for every caller (owner checks fail, the user rules apply)
        fs::read_to_string("/proc/self/status")
            .ok()
            .as_deref()
            .and_then(euid_of)
            .unwrap_or(0)
    }

    fn owner(&self, path: &str) -> io::Result<Owner> {
        let md = fs::symlink_metadata(path)?;
        Ok(Owner {
            uid: md.uid(),
            mode: md.mode() & 0o7777,
            dir: md.is_dir(),
        })
    }

    fn mkdirs_private(&self, path: &str) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }

    fn put_file(&self, path: &str, data: &str) -> io::Result<()> {
        let tmp = format!("{path}.tmp{}", std::process::id());
        let written = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(data.as_bytes()))
            .and_then(|()| fs::rename(&tmp, path));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written
    }

    fn append_file(&self, path: &str, data: &str) -> io::Result<()> {
        OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)?
            .write_all(data.as_bytes())
    }

    fn remove_file(&self, path: &str) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn files(&self, dir: &str) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    fn lock(&self, path: &str, how: Lock) -> io::Result<Option<Guard>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        let got = match how {
            Lock::Wait => {
                file.lock()?;
                true
            }
            Lock::Try => held(file.try_lock())?,
            Lock::TryShared => held(file.try_lock_shared())?,
        };
        Ok(got.then(|| Guard::new(file)))
    }
}

#[cfg(test)]
pub mod fake {
    use super::{kill_argv, Guard, Host, Lock, Owner};
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io;
    use std::rc::Rc;

    pub const ENOENT: i32 = 2;
    pub const ESRCH: i32 = 3;
    pub const EACCES: i32 = 13;
    pub const EBUSY: i32 = 16;
    pub const EEXIST: i32 = 17;
    pub const EINVAL: i32 = 22;
    pub const ENOSPC: i32 = 28;
    pub const ENOTEMPTY: i32 = 39;
    const UNLIMITED: &str = "9223372036854771712";

    fn errno(n: i32) -> io::Error {
        io::Error::from_raw_os_error(n)
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Kind {
        V2,
        V1,
    }

    struct Root {
        path: String,
        kind: Kind,
        controllers: Vec<String>,
    }

    #[derive(Default)]
    struct State {
        files: BTreeMap<String, String>,
        dirs: BTreeSet<String>,
        roots: Vec<Root>,
        /// pid -> hierarchy root -> directory (a pid not listed for a hierarchy is in its root)
        procs: BTreeMap<i32, BTreeMap<String, String>>,
        ignore_term: BTreeSet<i32>,
        /// SIGKILL sent to a task of a FROZEN v1 group: it lands when the group is thawed
        pending_kill: BTreeSet<i32>,
        devs: BTreeMap<String, (u32, u32)>,
        deny: Vec<String>,
        /// a kernel booted with swapaccount=0, or without CONFIG_MEMCG_SWAP: no memory.memsw.* files in new v1 groups
        no_memsw: bool,
        writes: Vec<(String, String)>,
        signals: Vec<(i32, i32)>,
        /// the clock `pause` advances
        clock_ms: u64,
        euid: u32,
        /// the process `/proc/self/...` stands for
        me: i32,
        /// start time of a process in clock ticks; a pid not listed has `1000 + pid`
        started: BTreeMap<i32, u64>,
        owners: BTreeMap<String, Owner>,
        /// files the store may not write: a path ending with one of these fails with EACCES
        deny_puts: Vec<String>,
        /// a rename to a path ending with one of these fails with EACCES
        deny_renames: Vec<String>,
        /// processes whose `/proc/PID/stat` fails to read, with the errno it fails with
        stat_fails: BTreeMap<i32, i32>,
        /// processes whose `/proc/PID/stat` reads, but is not a stat line
        stat_garbled: BTreeSet<i32>,
        /// files that cannot be read, with the errno
        read_fails: BTreeMap<String, i32>,
    }

    /// What holds a lock of the table: one exclusive holder or any number of shared ones.
    #[derive(Default, Clone, Copy)]
    struct Lk {
        exclusive: bool,
        shared: u32,
    }

    type Table = Rc<RefCell<BTreeMap<String, Lk>>>;

    /// A held lock; dropping it, like a process ending, releases it.
    struct Held {
        table: Table,
        path: String,
        exclusive: bool,
    }

    impl Drop for Held {
        fn drop(&mut self) {
            let mut t = self.table.borrow_mut();
            if let Some(l) = t.get_mut(&self.path) {
                if self.exclusive {
                    l.exclusive = false;
                } else {
                    l.shared -= 1;
                }
                if !l.exclusive && l.shared == 0 {
                    t.remove(&self.path);
                }
            }
        }
    }

    #[derive(Default)]
    pub struct FakeHost {
        s: RefCell<State>,
        locks: Table,
    }

    fn tokens(s: Option<&String>) -> Vec<String> {
        s.map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn parent(path: &str) -> &str {
        path.rsplit_once('/').map_or("", |p| p.0)
    }

    fn v2_files(controller: &str) -> Vec<(&'static str, &'static str)> {
        match controller {
            "memory" => vec![
                ("memory.max", "max"),
                ("memory.high", "max"),
                ("memory.low", "0"),
                ("memory.swap.max", "max"),
                ("memory.current", "0"),
                ("memory.peak", "0"),
                ("memory.events", "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n"),
                ("memory.oom.group", "0"),
            ],
            "pids" => vec![
                ("pids.max", "max"),
                ("pids.current", "0"),
                ("pids.events", "max 0\n"),
            ],
            "cpu" => vec![("cpu.max", "max 100000"), ("cpu.weight", "100")],
            "io" => vec![("io.max", ""), ("io.stat", "")],
            "cpuset" => vec![("cpuset.cpus", ""), ("cpuset.mems", "")],
            _ => vec![],
        }
    }

    fn v1_files(controller: &str) -> Vec<(&'static str, &'static str)> {
        match controller {
            "memory" => vec![
                ("memory.limit_in_bytes", UNLIMITED),
                ("memory.memsw.limit_in_bytes", UNLIMITED),
                ("memory.soft_limit_in_bytes", UNLIMITED),
                ("memory.usage_in_bytes", "0"),
                ("memory.max_usage_in_bytes", "0"),
                ("memory.failcnt", "0"),
                (
                    "memory.oom_control",
                    "oom_kill_disable 0\nunder_oom 0\noom_kill 0\n",
                ),
            ],
            "pids" => vec![
                ("pids.max", "max"),
                ("pids.current", "0"),
                ("pids.events", "max 0\n"),
            ],
            "cpu" => vec![
                ("cpu.cfs_period_us", "100000"),
                ("cpu.cfs_quota_us", "-1"),
                ("cpu.shares", "1024"),
                (
                    "cpu.stat",
                    "nr_periods 0\nnr_throttled 0\nthrottled_time 0\n",
                ),
            ],
            "blkio" => vec![
                ("blkio.throttle.read_bps_device", ""),
                ("blkio.throttle.write_bps_device", ""),
                ("blkio.throttle.read_iops_device", ""),
                ("blkio.throttle.write_iops_device", ""),
            ],
            "cpuset" => vec![("cpuset.cpus", ""), ("cpuset.mems", "")],
            "freezer" => vec![("freezer.state", "THAWED")],
            _ => vec![],
        }
    }

    impl State {
        fn root_of(&self, path: &str) -> Option<&Root> {
            self.roots
                .iter()
                .find(|r| path == r.path || path.starts_with(&format!("{}/", r.path)))
        }

        fn in_subtree(path: &str, dir: &str) -> bool {
            path == dir || path.starts_with(&format!("{dir}/"))
        }

        /// The directory a pid is in, in the hierarchy of `root`.
        fn dir_of(&self, pid: i32, root: &str) -> String {
            self.procs
                .get(&pid)
                .and_then(|m| m.get(root))
                .cloned()
                .unwrap_or_else(|| root.to_string())
        }

        fn pids_in(&self, dir: &str, recursive: bool) -> Vec<i32> {
            let Some(root) = self.root_of(dir).map(|r| r.path.clone()) else {
                return vec![];
            };
            self.procs
                .keys()
                .copied()
                .filter(|&pid| {
                    let d = self.dir_of(pid, &root);
                    d == dir || (recursive && Self::in_subtree(&d, dir))
                })
                .collect()
        }

        fn value(&self, path: &str) -> String {
            self.files
                .get(path)
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        }

        fn start_of(&self, pid: i32) -> u64 {
            self.started
                .get(&pid)
                .copied()
                .unwrap_or(1000 + u64::try_from(pid).unwrap_or(0))
        }

        /// `/proc/PID/cgroup`: one line per hierarchy, the path relative to the root of that hierarchy.
        fn cgroup_text(&self, pid: i32) -> String {
            let mut out = String::new();
            for (n, r) in self.roots.iter().enumerate() {
                let dir = self.dir_of(pid, &r.path);
                let rel = dir.strip_prefix(r.path.as_str()).unwrap_or("");
                let rel = if rel.is_empty() { "/" } else { rel };
                match r.kind {
                    Kind::V2 => out.push_str(&format!("0::{rel}\n")),
                    Kind::V1 if r.controllers.is_empty() => {
                        out.push_str(&format!("{}:name=systemd:{rel}\n", n + 1))
                    }
                    Kind::V1 => {
                        out.push_str(&format!("{}:{}:{rel}\n", n + 1, r.controllers.join(",")))
                    }
                }
            }
            out
        }

        /// The files of /proc this host synthesizes: `uptime`, and `cgroup`, `stat` and `status` of a live process (or of
        /// `self`). `None` for any other name; ENOENT for the process that is gone.
        fn proc_file(&self, rest: &str) -> Option<io::Result<String>> {
            if rest == "uptime" {
                let (s, cs) = (self.clock_ms / 1000, self.clock_ms % 1000 / 10);
                return Some(Ok(format!("{s}.{cs:02} 0.00\n")));
            }
            let (who, file) = rest.split_once('/')?;
            let pid = if who == "self" {
                self.me
            } else {
                who.parse().ok()?
            };
            if !matches!(file, "cgroup" | "stat" | "status") {
                return None;
            }
            if pid != self.me && !self.procs.contains_key(&pid) {
                return Some(Err(errno(ENOENT)));
            }
            if let Some(&code) = self.stat_fails.get(&pid).filter(|_| file == "stat") {
                return Some(Err(errno(code)));
            }
            Some(Ok(match file {
                "cgroup" => self.cgroup_text(pid),
                "stat" if self.stat_garbled.contains(&pid) => "no process here\n".to_string(),
                "stat" => format!(
                    "{pid} (fake proc) S 1 {pid} {pid} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {} 0 0\n",
                    self.start_of(pid)
                ),
                _ => format!("Name:\tfake\nUid:\t{0}\t{0}\t{0}\t{0}\n", self.euid),
            }))
        }

        fn add_owner(&mut self, path: &str, mode: u32, dir: bool) {
            let uid = self.euid;
            self.owners.insert(path.into(), Owner { uid, mode, dir });
        }

        fn frozen_v2(&self, dir: &str, root: &str) -> bool {
            let mut d = dir.to_string();
            loop {
                if self.value(&format!("{d}/cgroup.freeze")) == "1" {
                    return true;
                }
                if d == root || d.is_empty() {
                    return false;
                }
                d = parent(&d).to_string();
            }
        }

        fn pid_frozen(&self, pid: i32, only_v1: bool) -> bool {
            self.roots.iter().any(|r| {
                let d = self.dir_of(pid, &r.path);
                match r.kind {
                    Kind::V2 => !only_v1 && self.frozen_v2(&d, &r.path),
                    Kind::V1 => self.value(&format!("{d}/freezer.state")) == "FROZEN",
                }
            })
        }

        fn recompute(&mut self) {
            let ripe: Vec<i32> = self
                .pending_kill
                .iter()
                .copied()
                .filter(|&p| !self.pid_frozen(p, true))
                .collect();
            for p in ripe {
                self.pending_kill.remove(&p);
                self.procs.remove(&p);
            }
            let events: Vec<String> = self
                .files
                .keys()
                .filter(|k| k.ends_with("/cgroup.events"))
                .cloned()
                .collect();
            for path in events {
                let dir = parent(&path).to_string();
                let Some((root, kind)) = self.root_of(&dir).map(|r| (r.path.clone(), r.kind))
                else {
                    continue;
                };
                if kind != Kind::V2 {
                    continue;
                }
                let populated = !self.pids_in(&dir, true).is_empty();
                let frozen = self.frozen_v2(&dir, &root);
                self.files.insert(
                    path,
                    format!(
                        "populated {}\nfrozen {}\n",
                        u8::from(populated),
                        u8::from(frozen)
                    ),
                );
            }
        }

        fn populate(&mut self, dir: &str, kind: Kind, up: &str) {
            match kind {
                Kind::V2 => {
                    let ctl = tokens(self.files.get(&format!("{up}/cgroup.subtree_control")));
                    let mut put = |name: &str, data: &str| {
                        self.files.insert(format!("{dir}/{name}"), data.to_string());
                    };
                    put("cgroup.controllers", &ctl.join(" "));
                    for (n, d) in [
                        ("cgroup.subtree_control", ""),
                        ("cgroup.procs", ""),
                        ("cgroup.events", ""),
                        ("cgroup.freeze", "0"),
                        ("cgroup.kill", "0"),
                        ("cgroup.type", "domain"),
                        ("cpu.pressure", "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"),
                        ("memory.pressure", "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"),
                        ("io.pressure", "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"),
                        ("cpu.stat", "usage_usec 0\nuser_usec 0\nsystem_usec 0\n"),
                    ] {
                        put(n, d);
                    }
                    for c in ctl {
                        for (n, d) in v2_files(&c) {
                            put(n, d);
                        }
                    }
                }
                Kind::V1 => {
                    self.files
                        .insert(format!("{dir}/cgroup.procs"), String::new());
                    let ctl = self
                        .root_of(dir)
                        .map(|r| r.controllers.clone())
                        .unwrap_or_default();
                    for c in &ctl {
                        for (n, d) in v1_files(c) {
                            if self.no_memsw && n.starts_with("memory.memsw.") {
                                continue;
                            }
                            self.files.insert(format!("{dir}/{n}"), d.to_string());
                        }
                    }
                    if ctl.iter().any(|c| c == "memory") {
                        let inherited = self.value(&format!("{up}/memory.use_hierarchy"));
                        self.files
                            .insert(format!("{dir}/memory.use_hierarchy"), inherited);
                    }
                }
            }
        }

        fn move_pid(&mut self, dir: &str, data: &str) -> io::Result<()> {
            let pid: i32 = data.parse().map_err(|_| errno(EINVAL))?;
            if !self.procs.contains_key(&pid) {
                return Err(errno(ESRCH));
            }
            let (root, kind) = self
                .root_of(dir)
                .map(|r| (r.path.clone(), r.kind))
                .ok_or_else(|| errno(ENOENT))?;
            match kind {
                Kind::V1 => {
                    let empty = |f: &str| {
                        self.files
                            .get(&format!("{dir}/{f}"))
                            .is_some_and(|v| v.trim().is_empty())
                    };
                    if empty("cpuset.cpus") || empty("cpuset.mems") {
                        return Err(errno(ENOSPC));
                    }
                }
                Kind::V2 => {
                    if dir != root
                        && !tokens(self.files.get(&format!("{dir}/cgroup.subtree_control")))
                            .is_empty()
                    {
                        return Err(errno(EBUSY));
                    }
                }
            }
            self.procs
                .entry(pid)
                .or_default()
                .insert(root, dir.to_string());
            self.recompute();
            Ok(())
        }

        fn subtree(&mut self, dir: &str, data: &str) -> io::Result<()> {
            let root = self
                .root_of(dir)
                .map(|r| r.path.clone())
                .ok_or_else(|| errno(ENOENT))?;
            let avail = tokens(self.files.get(&format!("{dir}/cgroup.controllers")));
            let mut enabled = tokens(self.files.get(&format!("{dir}/cgroup.subtree_control")));
            let children: Vec<String> = self
                .dirs
                .iter()
                .filter(|d| parent(d) == dir)
                .cloned()
                .collect();
            for t in data.split_whitespace() {
                let (op, c) = t.split_at(1);
                match op {
                    "+" => {
                        if !avail.iter().any(|a| a == c) {
                            return Err(errno(ENOENT));
                        }
                        if dir != root && !self.pids_in(dir, false).is_empty() {
                            return Err(errno(EBUSY));
                        }
                        if !enabled.iter().any(|e| e == c) {
                            enabled.push(c.to_string());
                        }
                    }
                    "-" => {
                        if children.iter().any(|ch| {
                            tokens(self.files.get(&format!("{ch}/cgroup.subtree_control")))
                                .iter()
                                .any(|e| e == c)
                        }) {
                            return Err(errno(EBUSY));
                        }
                        enabled.retain(|e| e != c);
                    }
                    _ => return Err(errno(EINVAL)),
                }
            }
            enabled.sort();
            self.files
                .insert(format!("{dir}/cgroup.subtree_control"), enabled.join(" "));
            for ch in children {
                self.files
                    .insert(format!("{ch}/cgroup.controllers"), enabled.join(" "));
                for c in &enabled {
                    for (n, d) in v2_files(c) {
                        self.files
                            .entry(format!("{ch}/{n}"))
                            .or_insert_with(|| d.to_string());
                    }
                }
            }
            Ok(())
        }

        fn write_file(&mut self, path: &str, data: &str) -> io::Result<()> {
            self.writes
                .push((path.to_string(), data.trim().to_string()));
            if self.deny.iter().any(|d| path.ends_with(d.as_str())) {
                return Err(errno(EACCES));
            }
            if !self.files.contains_key(path) {
                return Err(errno(ENOENT));
            }
            let data = data.trim().to_string();
            let (dir, file) = path.rsplit_once('/').ok_or_else(|| errno(ENOENT))?;
            let (dir, file) = (dir.to_string(), file.to_string());
            match file.as_str() {
                "cgroup.procs" => return self.move_pid(&dir, &data),
                "cgroup.subtree_control" => return self.subtree(&dir, &data),
                "cgroup.freeze" | "cgroup.kill" => {
                    if !["0", "1"].contains(&data.as_str()) {
                        return Err(errno(EINVAL));
                    }
                    if file == "cgroup.kill" {
                        if data == "1" {
                            for pid in self.pids_in(&dir, true) {
                                self.procs.remove(&pid);
                            }
                        }
                    } else {
                        self.files.insert(path.to_string(), data);
                    }
                    self.recompute();
                    return Ok(());
                }
                "freezer.state" => {
                    if data != "FROZEN" && data != "THAWED" {
                        return Err(errno(EINVAL));
                    }
                }
                "memory.limit_in_bytes" | "memory.memsw.limit_in_bytes" => {
                    let num = |s: &str| -> i128 {
                        match s.trim().parse::<i128>() {
                            Ok(-1) => i128::MAX,
                            Ok(n) if n >= 1 << 62 => i128::MAX,
                            Ok(n) => n,
                            Err(_) => -2,
                        }
                    };
                    let v = num(&data);
                    if v == -2 {
                        return Err(errno(EINVAL));
                    }
                    let other = if file == "memory.limit_in_bytes" {
                        "memory.memsw.limit_in_bytes"
                    } else {
                        "memory.limit_in_bytes"
                    };
                    if let Some(o) = self.files.get(&format!("{dir}/{other}")) {
                        let o = num(o);
                        let bad = if file == "memory.limit_in_bytes" {
                            v > o
                        } else {
                            v < o
                        };
                        if bad {
                            return Err(errno(EINVAL));
                        }
                    }
                }
                _ => {}
            }
            self.files.insert(path.to_string(), data);
            self.recompute();
            Ok(())
        }
    }

    impl FakeHost {
        pub fn new() -> FakeHost {
            let h = FakeHost::default();
            {
                let mut s = h.s.borrow_mut();
                s.files.insert("/proc/mounts".into(), String::new());
                s.me = 4242;
            }
            h
        }

        fn mount(&self, line: String) {
            let mut s = self.s.borrow_mut();
            s.files
                .entry("/proc/mounts".into())
                .or_default()
                .push_str(&line);
        }

        /// A cgroup2 hierarchy whose root offers `controllers`.
        pub fn add_v2(&self, root: &str, controllers: &[&str]) {
            {
                let mut s = self.s.borrow_mut();
                s.dirs.insert(root.into());
                s.roots.push(Root {
                    path: root.into(),
                    kind: Kind::V2,
                    controllers: controllers.iter().map(|c| c.to_string()).collect(),
                });
                s.files
                    .insert(format!("{root}/cgroup.controllers"), controllers.join(" "));
                s.files
                    .insert(format!("{root}/cgroup.subtree_control"), String::new());
                s.files
                    .insert(format!("{root}/cgroup.procs"), String::new());
            }
            self.mount(format!(
                "cgroup2 {root} cgroup2 rw,nosuid,nodev,noexec,relatime,nsdelegate 0 0\n"
            ));
        }

        /// A cgroup v1 hierarchy with these controllers (empty = a named hierarchy such as systemd).
        pub fn add_v1(&self, root: &str, controllers: &[&str]) {
            {
                let mut s = self.s.borrow_mut();
                s.dirs.insert(root.into());
                s.roots.push(Root {
                    path: root.into(),
                    kind: Kind::V1,
                    controllers: controllers.iter().map(|c| c.to_string()).collect(),
                });
                s.files
                    .insert(format!("{root}/cgroup.procs"), String::new());
                if controllers.contains(&"memory") {
                    s.files
                        .insert(format!("{root}/memory.use_hierarchy"), "1".into());
                }
                if controllers.contains(&"cpuset") {
                    s.files.insert(format!("{root}/cpuset.cpus"), "0-3".into());
                    s.files.insert(format!("{root}/cpuset.mems"), "0".into());
                }
            }
            let opts = if controllers.is_empty() {
                "none,name=systemd".to_string()
            } else {
                controllers.join(",")
            };
            self.mount(format!(
                "cgroup {root} cgroup rw,nosuid,nodev,noexec,relatime,{opts} 0 0\n"
            ));
        }

        pub fn add_dev(&self, path: &str, major: u32, minor: u32) {
            self.s.borrow_mut().devs.insert(path.into(), (major, minor));
        }

        /// New v1 memory groups come without memory.memsw.* files.
        pub fn without_memsw(&self) {
            self.s.borrow_mut().no_memsw = true;
        }

        pub fn deny_writes_to(&self, suffix: &str) {
            self.s.borrow_mut().deny.push(suffix.into());
        }

        /// A process, in the root of every hierarchy, or in the given directories.
        pub fn proc_in(&self, pid: i32, dirs: &[&str]) {
            let mut s = self.s.borrow_mut();
            s.procs.entry(pid).or_default();
            for d in dirs {
                if let Some(root) = s.root_of(d).map(|r| r.path.clone()) {
                    s.procs.entry(pid).or_default().insert(root, d.to_string());
                }
            }
            s.recompute();
        }

        pub fn ignore_term(&self, pid: i32) {
            self.s.borrow_mut().ignore_term.insert(pid);
        }

        pub fn procs_of(&self, dir: &str) -> Vec<i32> {
            self.s.borrow().pids_in(dir, false)
        }

        pub fn alive(&self, pid: i32) -> bool {
            self.s.borrow().procs.contains_key(&pid)
        }

        pub fn value(&self, path: &str) -> String {
            self.s.borrow().value(path)
        }

        pub fn set(&self, path: &str, data: &str) {
            self.s.borrow_mut().files.insert(path.into(), data.into());
        }

        /// The data of the writes to files whose path ends with `suffix`, in order.
        pub fn writes_to(&self, suffix: &str) -> Vec<String> {
            self.s
                .borrow()
                .writes
                .iter()
                .filter(|(p, _)| p.ends_with(suffix))
                .map(|(_, d)| d.clone())
                .collect()
        }

        pub fn signals(&self) -> Vec<(i32, i32)> {
            self.s.borrow().signals.clone()
        }

        pub fn dir_exists(&self, path: &str) -> bool {
            self.s.borrow().dirs.contains(path)
        }

        /// The clock, in milliseconds; `pause` moves it on.
        pub fn clock(&self) -> u64 {
            self.s.borrow().clock_ms
        }

        pub fn set_clock(&self, ms: u64) {
            self.s.borrow_mut().clock_ms = ms;
        }

        /// The effective user id of the process under test.
        pub fn set_euid(&self, uid: u32) {
            self.s.borrow_mut().euid = uid;
        }

        /// The process `/proc/self/...` stands for (it need not be a process of the cgroup tables).
        pub fn set_self(&self, pid: i32) {
            self.s.borrow_mut().me = pid;
        }

        /// Who owns the path, and with which permission bits (it must exist; a directory when `dir`).
        pub fn set_owner(&self, path: &str, uid: u32, mode: u32, dir: bool) {
            self.s
                .borrow_mut()
                .owners
                .insert(path.into(), Owner { uid, mode, dir });
        }

        /// The start time (clock ticks) `/proc/PID/stat` reports for the process.
        pub fn set_start(&self, pid: i32, ticks: u64) {
            self.s.borrow_mut().started.insert(pid, ticks);
        }

        /// The pid is taken over by another process: same number, another start time, no cgroup placement.
        pub fn reuse_pid(&self, pid: i32) {
            let mut s = self.s.borrow_mut();
            let next = s.start_of(pid) + 1;
            s.started.insert(pid, next);
            s.procs.insert(pid, BTreeMap::new());
            s.recompute();
        }

        /// The process ends on its own (no signal was sent to it).
        pub fn exit_proc(&self, pid: i32) {
            let mut s = self.s.borrow_mut();
            s.procs.remove(&pid);
            s.pending_kill.remove(&pid);
            s.recompute();
        }

        /// A write of the store to a path ending with `suffix` fails.
        pub fn deny_puts_to(&self, suffix: &str) {
            self.s.borrow_mut().deny_puts.push(suffix.into());
        }

        /// Every write of the store works again.
        pub fn allow_puts(&self) {
            self.s.borrow_mut().deny_puts.clear();
        }

        /// A rename to a path ending with `suffix` fails.
        pub fn deny_renames_to(&self, suffix: &str) {
            self.s.borrow_mut().deny_renames.push(suffix.into());
        }

        /// Reading `/proc/PID/stat` fails with the errno: EACCES for a process that lives but cannot be examined
        /// (`hidepid`, a security module), ESRCH for one that ended while it was being read.
        pub fn fail_stat_of(&self, pid: i32, code: i32) {
            self.s.borrow_mut().stat_fails.insert(pid, code);
        }

        /// `/proc/PID/stat` of the process reads, but holds nothing a start time can be taken from.
        pub fn garble_stat_of(&self, pid: i32) {
            self.s.borrow_mut().stat_garbled.insert(pid);
        }

        /// Reading the file fails with the errno (`/proc/self/cgroup` when the kernel hides where a process runs).
        pub fn fail_read_of(&self, path: &str, code: i32) {
            self.s.borrow_mut().read_fails.insert(path.into(), code);
        }

        /// The content of a plain file, without any of the cgroup rules.
        pub fn file(&self, path: &str) -> Option<String> {
            self.s.borrow().files.get(path).cloned()
        }

        /// The names of the plain files directly in the directory, sorted.
        pub fn files_of(&self, dir: &str) -> Vec<String> {
            self.files(dir).unwrap_or_default()
        }
    }

    impl Host for FakeHost {
        fn read(&self, path: &str) -> io::Result<String> {
            let s = self.s.borrow();
            if let Some(&code) = s.read_fails.get(path) {
                return Err(errno(code));
            }
            if let Some(found) = path.strip_prefix("/proc/").and_then(|r| s.proc_file(r)) {
                return found;
            }
            if let Some(dir) = path.strip_suffix("/cgroup.procs") {
                if s.files.contains_key(path) {
                    return Ok(s
                        .pids_in(dir, false)
                        .iter()
                        .map(|p| format!("{p}\n"))
                        .collect());
                }
            }
            s.files.get(path).cloned().ok_or_else(|| errno(ENOENT))
        }

        fn write(&self, path: &str, data: &str) -> io::Result<()> {
            self.s.borrow_mut().write_file(path, data)
        }

        fn mkdir(&self, path: &str) -> io::Result<()> {
            let mut s = self.s.borrow_mut();
            let up = parent(path).to_string();
            if !s.dirs.contains(&up) {
                return Err(errno(ENOENT));
            }
            if s.dirs.contains(path) {
                return Err(errno(EEXIST));
            }
            let kind = s
                .root_of(path)
                .map(|r| r.kind)
                .ok_or_else(|| errno(ENOENT))?;
            s.dirs.insert(path.to_string());
            s.populate(path, kind, &up);
            s.recompute();
            Ok(())
        }

        fn rmdir(&self, path: &str) -> io::Result<()> {
            let mut s = self.s.borrow_mut();
            if !s.dirs.contains(path) {
                return Err(errno(ENOENT));
            }
            if s.roots.iter().any(|r| r.path == path) {
                return Err(errno(EBUSY));
            }
            if s.dirs.iter().any(|d| parent(d) == path) {
                return Err(errno(ENOTEMPTY));
            }
            if !s.pids_in(path, false).is_empty() {
                return Err(errno(EBUSY));
            }
            s.dirs.remove(path);
            let prefix = format!("{path}/");
            s.files.retain(|k, _| !k.starts_with(&prefix));
            s.recompute();
            Ok(())
        }

        fn exists(&self, path: &str) -> bool {
            let s = self.s.borrow();
            s.dirs.contains(path) || s.files.contains_key(path)
        }

        fn subdirs(&self, path: &str) -> io::Result<Vec<String>> {
            let s = self.s.borrow();
            if !s.dirs.contains(path) {
                return Err(errno(ENOENT));
            }
            Ok(s.dirs
                .iter()
                .filter(|d| parent(d) == path)
                .map(|d| d[path.len() + 1..].to_string())
                .collect())
        }

        fn devno(&self, path: &str) -> io::Result<(u32, u32)> {
            self.s
                .borrow()
                .devs
                .get(path)
                .copied()
                .ok_or_else(|| errno(ENOENT))
        }

        fn signal(&self, pids: &[i32], sig: i32) -> io::Result<()> {
            kill_argv(pids, sig)?;
            let mut s = self.s.borrow_mut();
            let mut first = None;
            for &pid in pids {
                s.signals.push((pid, sig));
                if !s.procs.contains_key(&pid) {
                    first.get_or_insert_with(|| errno(ESRCH));
                    continue;
                }
                let gone = match sig {
                    9 if s.pid_frozen(pid, true) => {
                        s.pending_kill.insert(pid);
                        false
                    }
                    9 => true,
                    15 => !s.ignore_term.contains(&pid) && !s.pid_frozen(pid, false),
                    _ => false,
                };
                if gone {
                    s.procs.remove(&pid);
                    s.recompute();
                }
            }
            first.map_or(Ok(()), Err)
        }

        fn pause(&self, ms: u64) {
            let mut s = self.s.borrow_mut();
            s.clock_ms += ms;
        }

        fn now_ms(&self) -> u64 {
            self.s.borrow().clock_ms
        }

        fn epoch_secs(&self) -> u64 {
            1_700_000_000 + self.s.borrow().clock_ms / 1000
        }

        fn euid(&self) -> u32 {
            self.s.borrow().euid
        }

        fn owner(&self, path: &str) -> io::Result<Owner> {
            let s = self.s.borrow();
            if let Some(o) = s.owners.get(path) {
                return Ok(*o);
            }
            if s.dirs.contains(path) {
                Ok(Owner {
                    uid: 0,
                    mode: 0o755,
                    dir: true,
                })
            } else if s.files.contains_key(path) {
                Ok(Owner {
                    uid: 0,
                    mode: 0o644,
                    dir: false,
                })
            } else {
                Err(errno(ENOENT))
            }
        }

        fn mkdirs_private(&self, path: &str) -> io::Result<()> {
            let mut s = self.s.borrow_mut();
            let mut at = String::new();
            for part in path.split('/').filter(|p| !p.is_empty()) {
                at.push('/');
                at.push_str(part);
                if s.files.contains_key(&at) {
                    return Err(errno(EEXIST));
                }
                if s.dirs.insert(at.clone()) {
                    s.add_owner(&at, 0o700, true);
                }
            }
            Ok(())
        }

        fn put_file(&self, path: &str, data: &str) -> io::Result<()> {
            let mut s = self.s.borrow_mut();
            if s.deny_puts.iter().any(|d| path.ends_with(d.as_str())) {
                return Err(errno(EACCES));
            }
            if !s.dirs.contains(parent(path)) || s.dirs.contains(path) {
                return Err(errno(ENOENT));
            }
            s.files.insert(path.into(), data.into());
            s.add_owner(path, 0o600, false);
            Ok(())
        }

        fn append_file(&self, path: &str, data: &str) -> io::Result<()> {
            let mut s = self.s.borrow_mut();
            if s.deny_puts.iter().any(|d| path.ends_with(d.as_str())) {
                return Err(errno(EACCES));
            }
            if !s.dirs.contains(parent(path)) || s.dirs.contains(path) {
                return Err(errno(ENOENT));
            }
            if !s.files.contains_key(path) {
                s.add_owner(path, 0o600, false);
            }
            s.files.entry(path.into()).or_default().push_str(data);
            Ok(())
        }

        fn remove_file(&self, path: &str) -> io::Result<()> {
            let mut s = self.s.borrow_mut();
            if s.files.remove(path).is_none() {
                return Err(errno(ENOENT));
            }
            s.owners.remove(path);
            Ok(())
        }

        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            let mut s = self.s.borrow_mut();
            if s.deny_renames.iter().any(|d| to.ends_with(d.as_str())) {
                return Err(errno(EACCES));
            }
            if !s.dirs.contains(parent(to)) || s.dirs.contains(to) {
                return Err(errno(ENOENT));
            }
            let data = s.files.remove(from).ok_or_else(|| errno(ENOENT))?;
            s.files.insert(to.into(), data);
            if let Some(o) = s.owners.remove(from) {
                s.owners.insert(to.into(), o);
            }
            Ok(())
        }

        fn files(&self, dir: &str) -> io::Result<Vec<String>> {
            let s = self.s.borrow();
            if !s.dirs.contains(dir) {
                return Err(errno(ENOENT));
            }
            Ok(s.files
                .keys()
                .filter(|k| parent(k) == dir)
                .map(|k| k[dir.len() + 1..].to_string())
                .collect())
        }

        fn lock(&self, path: &str, how: Lock) -> io::Result<Option<Guard>> {
            {
                let mut s = self.s.borrow_mut();
                if !s.dirs.contains(parent(path)) {
                    return Err(errno(ENOENT));
                }
                if !s.files.contains_key(path) {
                    s.files.insert(path.into(), String::new());
                    s.add_owner(path, 0o600, false);
                }
            }
            let mut table = self.locks.borrow_mut();
            let l = table.entry(path.into()).or_default();
            let exclusive = !matches!(how, Lock::TryShared);
            let free = if exclusive {
                !l.exclusive && l.shared == 0
            } else {
                !l.exclusive
            };
            if !free {
                // a blocking wait on a lock nobody of this single-threaded world can release would hang the test
                return match how {
                    Lock::Wait => Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "the fake host never waits for a lock",
                    )),
                    _ => Ok(None),
                };
            }
            if exclusive {
                l.exclusive = true;
            } else {
                l.shared += 1;
            }
            Ok(Some(Guard::new(Held {
                table: Rc::clone(&self.locks),
                path: path.into(),
                exclusive,
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeHost, ENOENT, ESRCH};
    use super::*;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Child;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A directory of its own under the temp dir, removed when the test is done.
    struct Scratch(String);

    impl Scratch {
        fn new() -> Scratch {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = format!(
                "{}/resctl-fs-{}-{}",
                std::env::temp_dir().display(),
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            );
            RealHost.mkdirs_private(&dir).unwrap();
            Scratch(dir)
        }

        fn at(&self, name: &str) -> String {
            format!("{}/{name}", self.0)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sleeper() -> Child {
        Command::new("sleep").arg("60").spawn().unwrap()
    }

    fn pid_of(c: &Child) -> i32 {
        i32::try_from(c.id()).unwrap()
    }

    fn no_kill_tool() -> bool {
        KILL_TOOLS.iter().all(|t| !Path::new(t).is_file())
    }

    #[test]
    fn kill_argv_is_the_whole_guard() {
        assert_eq!(kill_argv(&[2, 300], 9).unwrap(), ["-9", "2", "300"]);
        assert_eq!(kill_argv(&[77], 0).unwrap(), ["-0", "77"]);
        assert_eq!(kill_argv(&[77], 15).unwrap(), ["-15", "77"]);
        assert_eq!(kill_argv(&[], 9).unwrap(), ["-9"]);
        for sig in [-9, 1, 2, 3, 6, 18, 19, 10, 64] {
            let e = kill_argv(&[77], sig).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "signal {sig}");
        }
        // a pid below 2 is a process group, every process, or init; the whole list is refused, not just that pid
        for bad in [-100, -1, 0, 1] {
            let e = kill_argv(&[77, bad, 78], 9).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "pid {bad}");
        }
    }

    #[test]
    fn the_proc_texts_are_read_field_by_field() {
        let status =
            "Name:\tbash\nUmask:\t0022\nUid:\t1000\t0\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(euid_of(status), Some(0));
        assert_eq!(euid_of("Uid:\t7 8 9 10\n"), Some(8));
        assert_eq!(euid_of("Uid:\t7\n"), None);
        assert_eq!(euid_of("Uid:\tx y\n"), None);
        assert_eq!(euid_of("Name:\tx\n"), None);

        assert_eq!(uptime_ms("12345.67 8901.23\n"), Some(12_345_670));
        assert_eq!(uptime_ms("0.05 0.00"), Some(50));
        assert_eq!(uptime_ms("7.5 1"), Some(7_500));
        assert_eq!(uptime_ms("7 1"), Some(7_000));
        assert_eq!(uptime_ms("1.2345 1"), Some(1_234));
        assert_eq!(uptime_ms(""), None);
        assert_eq!(uptime_ms("soon 1"), None);
        assert_eq!(uptime_ms("18446744073709551615.00 0"), None);

        let stat = "1234 (bash) S 1 1234 1234 34816 1234 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 100 10 18446744073709551615 1 1 0\n";
        assert_eq!(starttime_of(stat), Some(987_654));
        // the name may hold spaces and brackets: the fields count from the LAST closing bracket
        let odd = "55 (a b) c) S 1 55 55 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 4242 1 1\n";
        assert_eq!(starttime_of(odd), Some(4242));
        assert_eq!(starttime_of("55 (short) S 1 55\n"), None);
        assert_eq!(starttime_of("55 no brackets\n"), None);
        assert_eq!(starttime_of(""), None);
    }

    #[test]
    fn real_files_are_private_and_replaced_whole() {
        let (h, s) = (RealHost, Scratch::new());
        let me = h.euid();
        assert_eq!(
            h.owner(&s.0).unwrap(),
            Owner {
                uid: me,
                mode: 0o700,
                dir: true
            }
        );
        h.mkdirs_private(&s.at("a/b/c")).unwrap();
        h.mkdirs_private(&s.at("a/b/c")).unwrap();
        assert_eq!(h.owner(&s.at("a/b")).unwrap().mode, 0o700);

        let f = s.at("f");
        h.put_file(&f, "one").unwrap();
        h.put_file(&f, "two").unwrap();
        assert_eq!(h.read(&f).unwrap(), "two");
        let o = h.owner(&f).unwrap();
        assert_eq!((o.uid, o.mode, o.dir), (me, 0o600, false));
        assert_eq!(h.files(&s.0).unwrap(), ["f"], "no temporary file is left");

        let log = s.at("log");
        h.append_file(&log, "a\n").unwrap();
        h.append_file(&log, "b\n").unwrap();
        assert_eq!(h.read(&log).unwrap(), "a\nb\n");
        assert_eq!(h.owner(&log).unwrap().mode, 0o600);

        h.rename(&log, &s.at("log.1")).unwrap();
        assert!(!h.exists(&log) && h.exists(&s.at("log.1")));
        assert_eq!(
            h.files(&s.0).unwrap(),
            ["f", "log.1"],
            "directories are not files"
        );
        h.remove_file(&s.at("log.1")).unwrap();
        let gone = h.remove_file(&s.at("log.1")).unwrap_err();
        assert_eq!(gone.kind(), io::ErrorKind::NotFound);

        // a failed replacement leaves neither a file nor a temporary one
        assert!(h.put_file(&s.at("nodir/f"), "x").is_err());
        assert!(!h.exists(&s.at("nodir")));
        assert_eq!(h.files(&s.0).unwrap(), ["f"]);

        // the owner of a link is the owner of the link, and a link is never a directory
        symlink(&s.0, s.at("link")).unwrap();
        assert!(!h.owner(&s.at("link")).unwrap().dir);
        assert!(h.owner(&s.0).unwrap().dir);
    }

    #[test]
    fn real_locks_conflict_like_flock_and_never_reach_a_child() {
        let (h, s) = (RealHost, Scratch::new());
        let p = s.at("x.lock");
        let a = h.lock(&p, Lock::Try).unwrap().expect("free lock");
        assert_eq!(h.owner(&p).unwrap().mode, 0o600);
        assert!(h.lock(&p, Lock::Try).unwrap().is_none());
        assert!(h.lock(&p, Lock::TryShared).unwrap().is_none());
        drop(a);
        let r1 = h.lock(&p, Lock::TryShared).unwrap().expect("shared");
        let r2 = h.lock(&p, Lock::TryShared).unwrap().expect("shared too");
        assert!(h.lock(&p, Lock::Try).unwrap().is_none());
        drop((r1, r2));
        drop(
            h.lock(&p, Lock::Wait)
                .unwrap()
                .expect("wait on a free lock"),
        );

        // the lock is bound to the guard, not to a child that was started while it was held
        let g = h.lock(&p, Lock::Try).unwrap().expect("free lock");
        let mut child = sleeper();
        drop(g);
        assert!(h.lock(&p, Lock::Try).unwrap().is_some());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn the_real_signal_goes_through_the_kill_tool() {
        let h = RealHost;
        h.signal(&[], 9).unwrap();
        let mut child = sleeper();
        let pid = pid_of(&child);
        if no_kill_tool() {
            let e = h.signal(&[pid], 0).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::Unsupported);
            child.kill().unwrap();
            child.wait().unwrap();
            return;
        }
        // refused before anything is sent: the list holds a pid that may never be signalled
        let e = h.signal(&[pid, 1], 15).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(child.try_wait().unwrap().is_none());
        h.signal(&[pid], 0).unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "the probe signals nothing"
        );
        h.signal(&[pid], 15).unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(15));
        h.signal(&[pid], 0).unwrap_err();

        // every process of the list is tried even when one of them is gone; the error is the first
        let mut done = Command::new("true").spawn().unwrap();
        let gone = pid_of(&done);
        done.wait().unwrap();
        let mut live = sleeper();
        let err = h.signal(&[gone, pid_of(&live)], 9);
        assert!(err.is_err());
        assert_eq!(live.wait().unwrap().signal(), Some(9));
    }

    #[test]
    fn the_fake_locks_follow_the_same_rules() {
        let h = FakeHost::new();
        h.mkdirs_private("/run/x").unwrap();
        let p = "/run/x/a.lock";
        assert!(!h.exists(p));
        let a = h.lock(p, Lock::Try).unwrap().expect("free lock");
        assert!(h.exists(p), "a lock creates its file");
        assert!(h.lock(p, Lock::Try).unwrap().is_none());
        assert!(h.lock(p, Lock::TryShared).unwrap().is_none());
        let wait = h
            .lock(p, Lock::Wait)
            .err()
            .expect("a wait on a busy lock would hang");
        assert_eq!(wait.kind(), io::ErrorKind::WouldBlock);
        drop(a);
        let r1 = h.lock(p, Lock::TryShared).unwrap().expect("shared");
        let r2 = h.lock(p, Lock::TryShared).unwrap().expect("shared too");
        assert!(h.lock(p, Lock::Try).unwrap().is_none());
        drop(r1);
        assert!(h.lock(p, Lock::Try).unwrap().is_none());
        drop(r2);
        assert!(h.lock(p, Lock::Wait).unwrap().is_some());
        let e = h.lock("/run/nodir/a.lock", Lock::Try).err().unwrap();
        assert_eq!(e.raw_os_error(), Some(ENOENT));
    }

    #[test]
    fn the_fake_files_keep_owners_and_fail_on_demand() {
        let h = FakeHost::new();
        h.set_euid(1000);
        h.mkdirs_private("/run/a/b").unwrap();
        let o = h.owner("/run/a/b").unwrap();
        assert_eq!((o.uid, o.mode, o.dir), (1000, 0o700, true));
        h.put_file("/run/a/f", "1").unwrap();
        h.put_file("/run/a/f", "2").unwrap();
        h.append_file("/run/a/log", "x").unwrap();
        h.append_file("/run/a/log", "y").unwrap();
        assert_eq!(h.read("/run/a/f").unwrap(), "2");
        assert_eq!(h.read("/run/a/log").unwrap(), "xy");
        assert_eq!(h.files("/run/a").unwrap(), ["f", "log"]);
        assert_eq!(h.subdirs("/run/a").unwrap(), ["b"]);
        h.rename("/run/a/log", "/run/a/log.1").unwrap();
        assert_eq!(h.owner("/run/a/log.1").unwrap().mode, 0o600);
        assert_eq!(
            h.read("/run/a/log").unwrap_err().raw_os_error(),
            Some(ENOENT)
        );
        h.remove_file("/run/a/log.1").unwrap();
        assert_eq!(
            h.remove_file("/run/a/log.1").unwrap_err().raw_os_error(),
            Some(ENOENT)
        );
        assert!(h.put_file("/run/none/f", "x").is_err());

        h.set_owner("/run/a/f", 0, 0o666, false);
        let o = h.owner("/run/a/f").unwrap();
        assert_eq!((o.uid, o.mode), (0, 0o666));
        h.deny_puts_to("/audit.log");
        assert!(h.append_file("/run/a/audit.log", "x").is_err());
        assert!(!h.exists("/run/a/audit.log"));
        h.put_file("/run/a/other", "ok").unwrap();
    }

    #[test]
    fn the_fake_clock_moves_only_when_something_waits() {
        let h = FakeHost::new();
        assert_eq!(h.now_ms(), 0);
        h.pause(12_340);
        assert_eq!(h.now_ms(), 12_340);
        assert_eq!(uptime_ms(&h.read("/proc/uptime").unwrap()), Some(12_340));
        h.set_clock(5);
        assert_eq!(h.clock(), 5);
        let before = h.epoch_secs();
        h.pause(3_000);
        assert_eq!(h.epoch_secs(), before + 3);
    }

    #[test]
    fn the_fake_proc_follows_the_cgroup_tables() {
        let h = FakeHost::new();
        h.add_v2("/c", &["memory", "pids"]);
        h.mkdir("/c/agent-interaction").unwrap();
        h.mkdir("/c/agent-interaction/system").unwrap();
        h.proc_in(7, &["/c/agent-interaction/system"]);
        h.proc_in(8, &[]);

        assert_eq!(
            h.read("/proc/7/cgroup").unwrap(),
            "0::/agent-interaction/system\n"
        );
        assert_eq!(h.read("/proc/8/cgroup").unwrap(), "0::/\n");
        assert_eq!(h.read("/proc/self/cgroup").unwrap(), "0::/\n");
        h.set_self(7);
        assert_eq!(
            h.read("/proc/self/cgroup").unwrap(),
            "0::/agent-interaction/system\n"
        );
        assert_eq!(
            h.read("/proc/9/cgroup").unwrap_err().raw_os_error(),
            Some(ENOENT)
        );
        assert!(h.read("/proc/self/status").unwrap().contains("Uid:\t0\t0"));
        h.set_euid(65534);
        assert_eq!(euid_of(&h.read("/proc/self/status").unwrap()), Some(65534));
        h.set_self(4242);

        assert_eq!(starttime_of(&h.read("/proc/7/stat").unwrap()), Some(1007));
        h.set_start(7, 55);
        assert_eq!(starttime_of(&h.read("/proc/7/stat").unwrap()), Some(55));
        // the pid is taken over by another process: another start time, and it is not in the old cgroup
        h.reuse_pid(7);
        assert_eq!(starttime_of(&h.read("/proc/7/stat").unwrap()), Some(56));
        assert_eq!(h.read("/proc/7/cgroup").unwrap(), "0::/\n");
        h.exit_proc(7);
        assert_eq!(
            h.read("/proc/7/stat").unwrap_err().raw_os_error(),
            Some(ENOENT)
        );

        let v = FakeHost::new();
        v.add_v1("/v/memory", &["memory", "pids"]);
        v.add_v1("/v/systemd", &[]);
        v.proc_in(3, &[]);
        assert_eq!(
            v.read("/proc/3/cgroup").unwrap(),
            "1:memory,pids:/\n2:name=systemd:/\n"
        );
    }

    #[test]
    fn the_fake_signal_keeps_the_rules_of_the_real_one() {
        let h = FakeHost::new();
        h.add_v2("/c", &["pids"]);
        h.proc_in(6, &[]);
        h.proc_in(9, &[]);
        let e = h.signal(&[6, 1], 9).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(
            h.signals().is_empty() && h.alive(6),
            "a refused list sends nothing"
        );
        h.signal(&[], 9).unwrap();
        h.signal(&[6], 0).unwrap();
        assert!(h.alive(6), "the probe signals nothing");
        // every process is tried; the error is the first one
        let e = h.signal(&[5, 6, 9], 9).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(ESRCH));
        assert!(!h.alive(6) && !h.alive(9));
        assert_eq!(h.signals(), [(6, 0), (5, 9), (6, 9), (9, 9)]);
    }
}
