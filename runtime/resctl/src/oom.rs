//! Where the OOM kills of a supervised run came from. A run that lost processes to the kernel's OOM killer is given one
//! origin:
//!
//! ```text
//! workload   the workload's own memory limit
//! class      the ceiling of its class: the workloads of the class together
//! ceiling    the ceiling of every workload together
//! outside    none of resctl's limits: the whole machine (or a NUMA node) ran out of memory, or a limit above resctl's tree
//! unknown    the evidence does not tell, or there is none
//! ```
//!
//! A run that lost none (the kernel counted no OOM kill in its domain) has no origin: if it ended by a signal, the
//! signal came from somewhere else. The evidence is taken when the run starts and when it has ended, and kept with the
//! verdict:
//!
//! - the kernel's counts at each level of the workload's memory hierarchy (the domain, its class, every workload): the
//!   processes of the domain the OOM killer ended (`oom_kill`: any OOM, the domain's own, an ancestor's or the
//!   machine's); on cgroup v2 the OOMs declared at each level's own limit (`memory.events.local` `oom`); on cgroup v1
//!   only how often each level's limit was reached (`memory.failcnt`, `memory.memsw.failcnt`), which is kept for the
//!   record but decides nothing: v1 does not count where an OOM was declared, a limit that was reached is no proof that
//!   it was the one, and a limit can be hit without the count moving (on the 6.18 kernel of the test host both stayed at 0
//!   at an OOM when the swap limit equals the memory limit, as resctl sets it when swap is not allowed);
//! - the kernel log (`/dev/kmsg`) where it can be read: the OOM killer's record of a kill names its constraint (a
//!   cgroup's limit, `CONSTRAINT_MEMCG` with that cgroup, or the machine) and the cgroup of the process it killed. Only
//!   records the kernel wrote count (a process may write to the log, but never as the kernel), and the kernel writes at
//!   most ten of them in five seconds, so a kill may have none;
//! - the machine's count of OOM kills (`/proc/vmstat` `oom_kill`), which must have grown at least as much as the domain's.
//!
//! A level is named only on evidence that agrees: records of the kernel log for every kill of the run that all name
//! the same origin and that the v2 counts do not contradict; or, without such records, the v2 counts with an OOM
//! declared at one level alone (at none: `outside`). On cgroup v1 only the kernel log names an origin. Anything else is
//! `unknown`, with the evidence there was (`attribute`).

use crate::domain::{Workload, WORKLOAD};
use crate::layout::Api;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Workload,
    Class,
    Ceiling,
    Outside,
    Unknown,
}

impl Origin {
    pub fn name(self) -> &'static str {
        match self {
            Origin::Workload => "workload",
            Origin::Class => "class",
            Origin::Ceiling => "ceiling",
            Origin::Outside => "outside",
            Origin::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Origin> {
        [
            Origin::Workload,
            Origin::Class,
            Origin::Ceiling,
            Origin::Outside,
            Origin::Unknown,
        ]
        .into_iter()
        .find(|o| o.name() == s)
    }
}

/// The levels of a workload's memory hierarchy, from the inside out: its domain, its class, every workload.
const LEVELS: [Origin; 3] = [Origin::Workload, Origin::Class, Origin::Ceiling];

/// The directories of the levels of `w`, in the order of `LEVELS`.
pub fn level_rels(w: &Workload) -> [String; 3] {
    [
        w.rel(),
        format!("{WORKLOAD}/{}", w.class),
        WORKLOAD.to_string(),
    ]
}

/// What the kernel counts at one level, since the level was created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// OOMs declared at the level's own limit (cgroup v2 `memory.events.local` `oom`); cgroup v1 does not count them
    pub declared: Option<u64>,
    /// how often the level's limit was reached (v2 `memory.events.local` `max`; v1 `memory.failcnt` and
    /// `memory.memsw.failcnt`)
    pub reached: Option<u64>,
    /// processes of the level the OOM killer ended (v2 `memory.events` `oom_kill`; v1 `memory.oom_control` `oom_kill`)
    pub kills: Option<u64>,
}

/// The evidence at one moment: when a run starts, or when it has ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// the API of the memory hierarchy; `None` when no hierarchy has the memory controller
    pub api: Option<Api>,
    /// the counts of the levels, in the order of `LEVELS`; `None` for a level the hierarchy does not hold
    pub levels: [Option<Counts>; 3],
    /// the machine's count of OOM kills (`/proc/vmstat` `oom_kill`)
    pub machine: Option<u64>,
    /// the sequence number of the next record of the kernel log; `None` when the log cannot be read
    pub log_next: Option<u64>,
    /// the start of the run was not seen (its supervisor is gone): nothing is known of the levels before it
    pub unseen: bool,
}

impl Snapshot {
    /// The start of a run that nobody saw.
    pub fn unseen() -> Snapshot {
        Snapshot {
            unseen: true,
            ..Snapshot::default()
        }
    }
}

/// Where the OOM kills of a run came from, and why that is the answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// the processes of the domain the OOM killer ended during the run; `None` when the kernel's count cannot be read
    pub kills: Option<u64>,
    /// `None` when there was no OOM kill
    pub origin: Option<Origin>,
    /// what the verdict rests on, one line without spaces (see `attribute`)
    pub evidence: String,
}

/// A record of the kernel log as `/dev/kmsg` gives it (`PRIORITY,SEQUENCE,MICROSECONDS,FLAGS[,...];MESSAGE`, then
/// lines of the record's dictionary): its sequence number, whether the kernel wrote it (facility 0: what a process writes
/// to the log never gets that facility), and its message.
pub fn kmsg_record(raw: &str) -> Option<(u64, bool, &str)> {
    let (prefix, rest) = raw.split_once(';')?;
    let mut f = prefix.split(',');
    let prio: u32 = f.next()?.parse().ok()?;
    let seq: u64 = f.next()?.parse().ok()?;
    Some((seq, prio >> 3 == 0, rest.lines().next().unwrap_or("")))
}

/// The records of the OOM killer in the kernel log, and the sequence number of the record after the last one read: the
/// start of a run keeps that number, and only records from there on belong to the run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OomLog {
    pub next: u64,
    /// the sequence number and the message of every record of the OOM killer (`oom-kill:`) the kernel wrote, oldest first
    pub records: Vec<(u64, String)>,
}

impl OomLog {
    /// Take one record, as `/dev/kmsg` gives it.
    pub fn add(&mut self, raw: &str) {
        if let Some((seq, by_kernel, msg)) = kmsg_record(raw) {
            self.next = self.next.max(seq.saturating_add(1));
            if by_kernel && msg.starts_with("oom-kill:") {
                self.records.push((seq, msg.to_string()));
            }
        }
    }

    /// The origins the records from `since` on give for kills of `w`'s processes.
    pub fn origins(&self, since: u64, w: &Workload) -> Vec<Origin> {
        self.records
            .iter()
            .filter(|(seq, _)| *seq >= since)
            .filter_map(|(_, m)| record_origin(m, w))
            .collect()
    }
}

/// Whether the cgroup `path`, written from the root of its hierarchy (which a cgroup namespace does not see), is the
/// directory `rel` or below it.
fn under(path: &str, rel: &str) -> bool {
    let at = format!("/{rel}");
    path == at || path.ends_with(&at) || path.contains(&format!("{at}/"))
}

/// The origin a record of the OOM killer gives for a kill of a process of `w`'s domain; `None` for any other record.
///
/// `oom-kill:constraint=CONSTRAINT_MEMCG,nodemask=(null),cpuset=/,mems_allowed=0,oom_memcg=/.../agent-interaction/
/// workload/batch/a,task_memcg=/.../agent-interaction/workload/batch/a,task=NAME,pid=N,uid=N` (one line; a machine-wide
/// OOM has `global_oom` in place of `oom_memcg=`). The name of the process comes after `,task=` and is the process's
/// own choice, so nothing after the first `,task=` is read.
pub fn record_origin(msg: &str, w: &Workload) -> Option<Origin> {
    let rest = msg.strip_prefix("oom-kill:")?;
    let head = rest.split_once(",task=").map_or(rest, |(h, _)| h);
    let (before, task) = head.rsplit_once(",task_memcg=")?;
    if !under(task, &w.rel()) {
        return None;
    }
    let constraint = before
        .split(',')
        .next()
        .and_then(|c| c.strip_prefix("constraint="));
    if constraint != Some("CONSTRAINT_MEMCG") || before.ends_with(",global_oom") {
        // the machine, or a NUMA node of a cpuset or a memory policy: no cgroup's limit
        return Some(Origin::Outside);
    }
    let Some((_, memcg)) = before.rsplit_once(",oom_memcg=") else {
        return Some(Origin::Unknown);
    };
    let [domain, class, ceiling] = level_rels(w);
    Some(if under(memcg, &domain) {
        Origin::Workload
    } else if memcg.ends_with(&format!("/{class}")) {
        Origin::Class
    } else if memcg.ends_with(&format!("/{ceiling}")) {
        Origin::Ceiling
    } else {
        Origin::Outside
    })
}

fn count(n: Option<u64>) -> String {
    n.map_or_else(|| "?".to_string(), |n| n.to_string())
}

/// Where the OOM kills of the run that started at `start` and ended at `end` came from. `log`: the origins the kernel
/// log gives for kills of the run's domain since the start (`record_origin`), `None` when it could not be read.
///
/// The evidence: `kills:N;declared:W/C/A;log:L;machine:+M` on cgroup v2, `reached:W/C/A` in place of `declared` on v1:
/// the domain's OOM kills, per level (the workload, its class, the ceiling) the OOMs declared at its limit or the times
/// its limit was reached during the run, what the kernel log holds (`unread`, `none`, or `ORIGIN*N,...`) and the
/// machine's OOM kills; `?` for what could not be read. An `unknown` verdict starts with `why:REASON;`, which a record that
/// keeps only the start of a long line does not lose.
pub fn attribute(start: &Snapshot, end: &Snapshot, log: Option<&[Origin]>) -> Verdict {
    // the domain is created for each run, so its counts start at zero; the class and the ceiling outlive runs
    let kills = end.levels[0].and_then(|c| c.kills);
    let mut ev = vec![format!("kills:{}", count(kills))];
    let unknown = |mut ev: Vec<String>, why: &str| {
        ev.insert(0, format!("why:{why}"));
        Verdict {
            kills,
            origin: Some(Origin::Unknown),
            evidence: ev.join(";"),
        }
    };
    let Some(k) = kills else {
        return unknown(ev, "no-kill-count");
    };
    if k == 0 {
        return Verdict {
            kills,
            origin: None,
            evidence: ev.join(";"),
        };
    }
    let v2 = end.api == Some(Api::V2);
    let of = |c: Counts| if v2 { c.declared } else { c.reached };
    let delta: Vec<Option<u64>> = (0..3)
        .map(|i| {
            let now = end.levels[i].and_then(of)?;
            let then = match i {
                0 => 0,
                _ if start.unseen => return None,
                _ => start.levels[i].and_then(of)?,
            };
            now.checked_sub(then)
        })
        .collect();
    ev.push(format!(
        "{}:{}",
        if v2 { "declared" } else { "reached" },
        delta
            .iter()
            .map(|d| count(*d))
            .collect::<Vec<_>>()
            .join("/")
    ));
    ev.push(match log {
        None => "log:unread".to_string(),
        Some([]) => "log:none".to_string(),
        Some(rs) => {
            let mut seen: Vec<(Origin, usize)> = Vec::new();
            for &o in rs {
                match seen.iter_mut().find(|(s, _)| *s == o) {
                    Some((_, n)) => *n += 1,
                    None => seen.push((o, 1)),
                }
            }
            let parts: Vec<String> = seen
                .iter()
                .map(|(o, n)| format!("{}*{n}", o.name()))
                .collect();
            format!("log:{}", parts.join(","))
        }
    });
    let machine = match (start.unseen, start.machine, end.machine) {
        (false, Some(a), Some(b)) => b.checked_sub(a),
        _ => None,
    };
    ev.push(format!(
        "machine:{}",
        machine.map_or_else(|| "?".to_string(), |m| format!("+{m}"))
    ));

    if machine.is_some_and(|m| m < k) {
        return unknown(ev, "machine-fewer");
    }
    // what the counts alone say: Ok(Some(o)) they name o, Ok(None) they do not tell, Err(why) they cannot be read. Only
    // cgroup v2 counts the OOMs declared at a level; what v1 counts (a limit reached) is no evidence either way: a limit
    // can be hit while its count stays where it was (memory.failcnt and memory.memsw.failcnt both did on the kernel of
    // the test host, with the swap limit equal to the memory limit)
    let counted: Result<Option<Origin>, &str> = if start.unseen {
        Err("unseen-start")
    } else if !v2 {
        Ok(None)
    } else if delta.iter().any(Option::is_none) {
        Err("no-counts")
    } else {
        let hit: Vec<Origin> = LEVELS
            .iter()
            .zip(&delta)
            .filter(|(_, d)| d.is_some_and(|d| d > 0))
            .map(|(o, _)| *o)
            .collect();
        match hit.as_slice() {
            [] => Ok(Some(Origin::Outside)),
            [o] => Ok(Some(*o)),
            _ => Ok(None),
        }
    };
    // on v2, a level the kernel log names must have declared an OOM at its limit
    let agrees = |o: Origin| match LEVELS.iter().position(|l| *l == o) {
        Some(i) if v2 => delta[i].is_none_or(|d| d > 0),
        _ => true,
    };
    let logged = match log {
        Some(rs) if !rs.is_empty() => Some(rs),
        _ => None,
    };
    if let Some(rs) = logged {
        let o = rs[0];
        if rs.iter().any(|&r| r != o) {
            return unknown(ev, "log-mixed");
        }
        if o == Origin::Unknown || !agrees(o) {
            return unknown(ev, "log-contradicts");
        }
        let all = u64::try_from(rs.len()).unwrap_or(u64::MAX) >= k;
        if all || counted == Ok(Some(o)) {
            return Verdict {
                kills,
                origin: Some(o),
                evidence: ev.join(";"),
            };
        }
        return unknown(ev, "log-partial");
    }
    match counted {
        Ok(Some(o)) => Verdict {
            kills,
            origin: Some(o),
            evidence: ev.join(";"),
        },
        Ok(None) if v2 => unknown(ev, "several-levels"),
        Ok(None) => unknown(ev, "v1-no-declared-count"),
        Err(why) => unknown(ev, why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w() -> Workload {
        Workload::new("batch", "a").unwrap()
    }

    fn v2(declared: [u64; 3], kills: u64) -> Snapshot {
        let mut s = Snapshot {
            api: Some(Api::V2),
            machine: Some(100 + kills),
            log_next: Some(7),
            ..Snapshot::default()
        };
        for (i, d) in declared.into_iter().enumerate() {
            s.levels[i] = Some(Counts {
                declared: Some(d),
                reached: Some(d * 10),
                kills: Some(if i == 0 { kills } else { 0 }),
            });
        }
        s
    }

    fn v1(reached: [u64; 3], kills: u64) -> Snapshot {
        let mut s = v2([0; 3], kills);
        s.api = Some(Api::V1);
        for (i, r) in reached.into_iter().enumerate() {
            s.levels[i] = Some(Counts {
                declared: None,
                reached: Some(r),
                kills: Some(if i == 0 { kills } else { 0 }),
            });
        }
        s
    }

    /// The start of a run: the class and the ceiling have counts of their own already (other runs, other workloads).
    fn start(api: Api) -> Snapshot {
        let mut s = if api == Api::V2 {
            v2([0, 2, 3], 0)
        } else {
            v1([0, 20, 30], 0)
        };
        s.machine = Some(100);
        s
    }

    fn origin(
        start: &Snapshot,
        end: &Snapshot,
        log: Option<&[Origin]>,
    ) -> (Option<Origin>, String) {
        let v = attribute(start, end, log);
        (v.origin, v.evidence)
    }

    #[test]
    fn a_kill_at_one_level_of_a_cgroup_v2_hierarchy_is_given_that_level() {
        let s = start(Api::V2);
        for (end, want) in [
            ([1, 2, 3], Origin::Workload),
            ([0, 3, 3], Origin::Class),
            ([0, 2, 4], Origin::Ceiling),
            ([0, 2, 3], Origin::Outside),
        ] {
            let (o, ev) = origin(&s, &v2(end, 1), None);
            assert_eq!(o, Some(want), "{ev}");
        }
        assert_eq!(
            origin(&s, &v2([1, 2, 3], 1), None).1,
            "kills:1;declared:1/0/0;log:unread;machine:+1"
        );
        // a group kill: one OOM declared, three processes ended
        assert_eq!(
            origin(&s, &v2([1, 2, 3], 3), Some(&[])).0,
            Some(Origin::Workload)
        );
    }

    #[test]
    fn a_run_without_an_oom_kill_has_no_origin_and_one_without_a_count_an_unknown_one() {
        let s = start(Api::V2);
        let v = attribute(&s, &v2([0, 2, 3], 0), None);
        assert_eq!(
            (v.kills, v.origin, v.evidence.as_str()),
            (Some(0), None, "kills:0")
        );
        let mut end = v2([1, 2, 3], 1);
        end.levels[0] = None;
        let v = attribute(&s, &end, Some(&[Origin::Workload]));
        assert_eq!(v.kills, None);
        assert_eq!(v.origin, Some(Origin::Unknown));
        assert_eq!(v.evidence, "why:no-kill-count;kills:?");
    }

    #[test]
    fn oom_declared_at_several_levels_is_unknown_unless_the_kernel_log_names_one() {
        let s = start(Api::V2);
        let end = v2([1, 3, 3], 1);
        let (o, ev) = origin(&s, &end, Some(&[]));
        assert_eq!(o, Some(Origin::Unknown));
        assert_eq!(
            ev,
            "why:several-levels;kills:1;declared:1/1/0;log:none;machine:+1"
        );
        // the class's OOM killed a neighbour; the record of this workload's kill names its own limit
        assert_eq!(
            origin(&s, &end, Some(&[Origin::Workload])).0,
            Some(Origin::Workload)
        );
        assert_eq!(
            origin(&s, &end, Some(&[Origin::Class])).0,
            Some(Origin::Class)
        );
    }

    #[test]
    fn on_cgroup_v1_only_the_kernel_log_names_a_limit() {
        let s = start(Api::V1);
        // the workload's limit was reached: an OOM there is likely, but a machine-wide one cannot be excluded
        let (o, ev) = origin(&s, &v1([812, 20, 30], 1), Some(&[]));
        assert_eq!(o, Some(Origin::Unknown));
        assert_eq!(
            ev,
            "why:v1-no-declared-count;kills:1;reached:812/0/0;log:none;machine:+1"
        );
        assert_eq!(
            origin(&s, &v1([812, 20, 30], 1), None).0,
            Some(Origin::Unknown)
        );
        assert_eq!(
            origin(&s, &v1([812, 20, 30], 1), Some(&[Origin::Workload])).0,
            Some(Origin::Workload)
        );
        // the workload was at its limit, and the kernel says the whole machine ran out
        assert_eq!(
            origin(&s, &v1([812, 20, 30], 1), Some(&[Origin::Outside])).0,
            Some(Origin::Outside)
        );
        // no limit counted as reached proves nothing on v1: the kernel can leave the count where it was (it did on the
        // test host, at a limit the kernel log said was the one)
        assert_eq!(
            origin(&s, &v1([0, 20, 30], 1), None).0,
            Some(Origin::Unknown)
        );
        let (o, ev) = origin(&s, &v1([0, 20, 30], 1), Some(&[Origin::Workload]));
        assert_eq!(o, Some(Origin::Workload));
        assert_eq!(ev, "kills:1;reached:0/0/0;log:workload*1;machine:+1");
        assert_eq!(
            origin(&s, &v1([0, 20, 30], 1), Some(&[Origin::Class])).0,
            Some(Origin::Class)
        );
    }

    #[test]
    fn a_kernel_log_that_disagrees_with_itself_or_with_the_counts_makes_the_origin_unknown() {
        let s = start(Api::V2);
        let (o, ev) = origin(
            &s,
            &v2([1, 2, 3], 2),
            Some(&[Origin::Workload, Origin::Outside]),
        );
        assert_eq!(o, Some(Origin::Unknown));
        assert!(
            ev.starts_with("why:log-mixed;") && ev.contains(";log:workload*1,outside*1;"),
            "{ev}"
        );
        // the log names the class, whose limit declared no OOM
        let (o, ev) = origin(&s, &v2([1, 2, 3], 1), Some(&[Origin::Class]));
        assert_eq!(o, Some(Origin::Unknown));
        assert!(ev.starts_with("why:log-contradicts;"), "{ev}");
        // a record whose cgroup could not be read names nothing
        let (o, ev) = origin(&s, &v2([1, 2, 3], 1), Some(&[Origin::Unknown]));
        assert_eq!(o, Some(Origin::Unknown));
        assert!(ev.starts_with("why:log-contradicts;"), "{ev}");
    }

    #[test]
    fn a_kernel_log_that_covers_some_kills_only_decides_with_the_counts() {
        // the kernel writes at most ten records in five seconds: three kills, one record
        let s = start(Api::V2);
        assert_eq!(
            origin(&s, &v2([1, 2, 3], 3), Some(&[Origin::Workload])).0,
            Some(Origin::Workload)
        );
        let (o, ev) = origin(&s, &v2([1, 3, 3], 3), Some(&[Origin::Workload]));
        assert_eq!(o, Some(Origin::Unknown));
        assert!(ev.starts_with("why:log-partial;"), "{ev}");
        let (o, _) = origin(
            &start(Api::V1),
            &v1([9, 20, 30], 3),
            Some(&[Origin::Workload]),
        );
        assert_eq!(o, Some(Origin::Unknown));
    }

    #[test]
    fn missing_or_inconsistent_counts_make_the_origin_unknown() {
        let s = start(Api::V2);
        let mut end = v2([1, 2, 3], 1);
        end.levels[1] = None;
        assert_eq!(
            origin(&s, &end, None).1,
            "why:no-counts;kills:1;declared:1/?/0;log:unread;machine:+1"
        );
        // a class that was created anew since the start counts from zero again
        let (o, ev) = origin(&s, &v2([1, 0, 3], 1), None);
        assert_eq!(o, Some(Origin::Unknown));
        assert!(ev.contains("declared:1/?/0"), "{ev}");
        // the machine cannot have counted fewer OOM kills than one of its cgroups
        let mut end = v2([1, 2, 3], 2);
        end.machine = Some(101);
        let (o, ev) = origin(&s, &end, Some(&[Origin::Workload, Origin::Workload]));
        assert_eq!(o, Some(Origin::Unknown));
        assert!(
            ev.starts_with("why:machine-fewer;") && ev.ends_with(";machine:+1"),
            "{ev}"
        );
        // a supervisor that is gone saw no start: the class and the ceiling cannot be compared, the log not bounded
        let (o, ev) = origin(&Snapshot::unseen(), &v2([1, 2, 3], 1), None);
        assert_eq!(o, Some(Origin::Unknown));
        assert_eq!(
            ev,
            "why:unseen-start;kills:1;declared:1/?/?;log:unread;machine:?"
        );
    }

    #[test]
    fn the_records_of_the_oom_killer_are_read_from_the_kernel_log() {
        let a = w();
        let rec = |memcg: &str, task: &str| {
            format!(
                "oom-kill:constraint=CONSTRAINT_MEMCG,nodemask=(null),cpuset=/,mems_allowed=0,oom_memcg={memcg},task_memcg={task},task=ctr-test,pid=4242,uid=0"
            )
        };
        let pre = "/process_api/x/agent-interaction/workload";
        let mine = format!("{pre}/batch/a");
        assert_eq!(
            record_origin(&rec(&mine, &mine), &a),
            Some(Origin::Workload)
        );
        assert_eq!(
            record_origin(&rec(&format!("{pre}/batch"), &mine), &a),
            Some(Origin::Class)
        );
        assert_eq!(record_origin(&rec(pre, &mine), &a), Some(Origin::Ceiling));
        assert_eq!(
            record_origin(&rec("/process_api/x", &mine), &a),
            Some(Origin::Outside)
        );
        // a cgroup below the domain is the domain
        let below = format!("{mine}/sub");
        assert_eq!(
            record_origin(&rec(&below, &below), &a),
            Some(Origin::Workload)
        );
        // the machine, a NUMA node
        let global = format!(
            "oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),cpuset=/,mems_allowed=0,global_oom,task_memcg={mine},task=x,pid=9,uid=0"
        );
        assert_eq!(record_origin(&global, &a), Some(Origin::Outside));
        let node = rec(&mine, &mine).replace("CONSTRAINT_MEMCG", "CONSTRAINT_CPUSET");
        assert_eq!(record_origin(&node, &a), Some(Origin::Outside));
        // the kill of another workload's process, a workload of the same name in another class, other records
        assert_eq!(
            record_origin(&rec(&format!("{pre}/batch"), &format!("{pre}/batch/b")), &a),
            None
        );
        assert_eq!(
            record_origin(&rec(&format!("{pre}/web/a"), &format!("{pre}/web/a")), &a),
            None
        );
        assert_eq!(
            record_origin(&rec(&mine, &format!("{pre}/batch/ab")), &a),
            None
        );
        assert_eq!(
            record_origin(
                "Memory cgroup out of memory: Killed process 4242 (ctr-test)",
                &a
            ),
            None
        );
    }

    #[test]
    fn a_process_cannot_write_the_origin_of_its_own_kill() {
        let a = w();
        let mine = "/agent-interaction/workload/batch/a";
        // the name of the process (at most 15 characters, its own choice) comes after `,task=`: it is not read
        let named = format!(
            "oom-kill:constraint=CONSTRAINT_MEMCG,nodemask=(null),cpuset=/,mems_allowed=0,oom_memcg={mine},task_memcg={mine},task=,global_oom,pid=1,uid=0"
        );
        assert_eq!(record_origin(&named, &a), Some(Origin::Workload));
        // what a process writes to /dev/kmsg gets facility user (1) at least, never the kernel's (0)
        let kernel = format!("3,812,1000,-;{named}");
        let user = format!("14,813,1001,-;{named}");
        assert_eq!(kmsg_record(&kernel), Some((812, true, named.as_str())));
        assert_eq!(kmsg_record(&user), Some((813, false, named.as_str())));
        assert_eq!(
            kmsg_record("6,814,1002,-;line\n SUBSYSTEM=mem\n"),
            Some((814, true, "line"))
        );
        assert_eq!(kmsg_record("no record"), None);
        assert_eq!(kmsg_record("x,1,0,-;m"), None);
    }

    #[test]
    fn origins_are_named_and_read_back() {
        for o in [
            Origin::Workload,
            Origin::Class,
            Origin::Ceiling,
            Origin::Outside,
            Origin::Unknown,
        ] {
            assert_eq!(Origin::parse(o.name()), Some(o));
        }
        assert_eq!(Origin::parse("machine"), None);
    }
}
