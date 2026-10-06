//! The host the controller runs on: files, directories and signals. `RealHost` is the machine; `fake::FakeHost`
//! (tests only) models the cgroup rules of the kernel, so the control logic is tested without privileges and the
//! rules that bit it on real kernels stay pinned (no internal processes, empty cpusets, memsw ordering, frozen tasks).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::thread;
use std::time::Duration;

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
    fn signal(&self, pid: i32, sig: i32) -> io::Result<()>;
    /// Wait; every timeout of the controller is counted in these waits.
    fn pause(&self, ms: u64);
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
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

    fn signal(&self, pid: i32, sig: i32) -> io::Result<()> {
        // SAFETY: kill(2) takes two integers and touches no memory of this process.
        if unsafe { kill(pid, sig) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn pause(&self, ms: u64) {
        thread::sleep(Duration::from_millis(ms));
    }
}

#[cfg(test)]
pub mod fake {
    use super::Host;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io;

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
    }

    #[derive(Default)]
    pub struct FakeHost {
        s: RefCell<State>,
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
            h.s.borrow_mut()
                .files
                .insert("/proc/mounts".into(), String::new());
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
    }

    impl Host for FakeHost {
        fn read(&self, path: &str) -> io::Result<String> {
            let s = self.s.borrow();
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

        fn signal(&self, pid: i32, sig: i32) -> io::Result<()> {
            let mut s = self.s.borrow_mut();
            s.signals.push((pid, sig));
            if !s.procs.contains_key(&pid) {
                return Err(errno(ESRCH));
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
            Ok(())
        }

        fn pause(&self, _ms: u64) {}
    }
}
