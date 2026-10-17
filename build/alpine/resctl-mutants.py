#!/usr/bin/env python3
"""Negative controls of the resource-control suite: deliberate defects of resctl that the suite must catch.

    resctl-mutants.py list                       the names of the defects, one per line
    resctl-mutants.py show NAME                  what the defect is, the areas it concerns and the rows it must break
    resctl-mutants.py areas NAME                 the areas of the suite that check-resctl.sh runs for it
    resctl-mutants.py guest NAME                 yes if the defect can only be shown in the guest too (the test bed)
    resctl-mutants.py apply NAME CRATE_DIR [RUN_DIR]
                                                 puts the defect into a copy of the crate (never into runtime/resctl), or
                                                 into a copy of the suite (RUN_DIR, check-resctl.sh's /work) for an edit
                                                 of a file named suite:PATH
    resctl-mutants.py verify NAME STATUS_FILE    PASS if the judge failed every row the defect must break

`check-resctl.sh --mutant NAME` runs the whole cycle on a copy of the crate: apply, build and unit-test it (unprivileged
session), run the areas of the defect on this host (privileged session) and, where the table says so, in the guest,
judge the facts (resctl-judge.py --partial --status) and verify. A row named BASE must FAIL in every environment where it ran; BASE.ENV names one environment only.
A row that did not run does not count as caught. A defect whose text is no longer in the sources is an error, not a pass:
the table must follow the code.
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CRATE = os.path.realpath(os.path.join(HERE, "..", "..", "runtime", "resctl"))
# the sources of the suite in the repository: a defect of the suite (an edit of suite:PATH) goes into a copy only
SUITE = [os.path.realpath(os.path.join(HERE, d)) for d in ("resctl-test", "tools-test")]

# name -> what, areas, rows the defect must break, edits (file, text, replacement)
MUTANTS = {
    "shield-off": {
        "what": "no kill is told which processes are critical",
        "areas": ["critical"],
        "breaks": ["unit", "crit_misplaced", "crit_freeze", "crit_registry", "crit_recover", "crit_teardown", "crit_audit",
                   "crit_survival"],
        "edits": [("src/store.rs",
                   "        Ok(self.critical()?.live.iter().map(|e| e.pid).collect())\n    }\n\n"
                   "    /// The processes a registry that is corrupt",
                   "        Ok(Vec::new())\n    }\n\n    /// The processes a registry that is corrupt")],
    },
    "protect-workload": {
        "what": "a process that runs inside a workload may be protected (moved to system and registered)",
        "areas": ["critical"],
        "breaks": ["unit", "crit_no_obtain", "crit_registry", "crit_teardown", "crit_audit", "crit_survival",
                   "system_canary_critical"],
        "edits": [("src/ops.rs",
                   "        Ok(p @ Placement::Workload(_)) => {\n"
                   "            return Err(Error::Refused(format!(\n"
                   "                \"process {pid} runs in the workload {}: a workload process is never protected\",\n"
                   "                p.workload().unwrap_or_default()\n"
                   "            )))\n"
                   "        }",
                   "        Ok(Placement::Workload(_)) => {\n"
                   "            c.attach(&Target::System, pid)?;\n"
                   "            true\n"
                   "        }")],
    },
    "guard-off": {
        "what": "a command asked from inside a workload is carried out",
        "areas": ["critical"],
        "breaks": ["unit", "crit_no_request", "crit_uid_drop", "crit_audit", "crit_survival"],
        "edits": [("src/ops.rs",
                   "    match confined(host) {\n        Ok(None) => Ok(()),",
                   "    match confined(host).map(|_: Option<String>| None::<String>) {\n        Ok(None) => Ok(()),")],
    },
    "intent-ignored": {
        "what": "an end that an operator asked for is classified by its raw status, so it looks like a crash",
        "areas": ["process"],
        "breaks": ["unit", "proc_stop", "proc_unexpected", "proc_forced_kill", "proc_stop_forced", "proc_isolation"],
        "edits": [("src/life.rs",
                   "        (Some(i), _) => Ending::Requested(i),\n",
                   "        (Some(_), Exit::Signal(s)) => Ending::Crashed(s),\n"
                   "        (Some(_), Exit::Code(n)) if n != 0 => Ending::Failed(n),\n"
                   "        (Some(i), _) => Ending::Requested(i),\n")],
    },
    "unbounded": {
        "what": "no limit on automatic restarts: neither the window nor the total is enforced",
        "areas": ["process"],
        "breaks": ["unit", "proc_crash_loop", "proc_reset_recover", "proc_restart_always", "proc_restart_total"],
        "edits": [("src/life.rs", "    if h.total >= r.total {", "    if false && h.total >= r.total {"),
                  ("src/life.rs", "    if recent >= r.max {", "    if false && recent >= r.max {")],
    },
    "no-oom-bias": {
        "what": "a process put into a workload is not made the first choice of the OOM killer (no oom_score_adj)",
        "areas": ["stress"],
        "guest": True,
        "breaks": ["unit", "oom_bias", "global_oom"],
        "edits": [("src/domain.rs",
                   "        if matches!(t, Target::Workload(_)) {\n"
                   "            // before the move: no process is ever inside a workload without the bias",
                   "        if false {\n"
                   "            // before the move: no process is ever inside a workload without the bias")],
    },
    "no-bias-restore": {
        "what": "a supervisor leaves a lowered OOM bias of its workload as it is",
        "areas": ["stress"],
        "breaks": ["unit", "oom_bias_restored"],
        "edits": [("src/supervise.rs",
                   "        if polls.is_multiple_of(RESTORE_POLLS) {",
                   "        if false && polls.is_multiple_of(RESTORE_POLLS) {")],
    },
    "no-critical-bias": {
        "what": "a registered critical process does not get the OOM preference of the policy",
        "areas": ["limits", "stress"],
        "guest": True,
        "breaks": ["unit", "critical_oom", "global_oom_no_workload"],
        "edits": [("src/domain.rs",
                   "        let want = self.policy.system.oom_score_adj?;",
                   "        let want = self.policy.system.oom_score_adj.filter(|_| false)?;")],
    },
    "slow-state": {
        "what": "resctl state takes 2.5 s to answer: a control plane that stalls",
        "areas": ["stress"],
        "breaks": ["stress_control"],
        "edits": [("src/main.rs",
                   "        Cmd::State(which) => {\n",
                   "        Cmd::State(which) => {\n            std::thread::sleep(std::time::Duration::from_millis(2500));\n")],
    },
    "blind-beat": {
        "what": "the periodic task that observes the system domain never reports a stall (a defect of the observer)",
        "areas": ["stress"],
        "breaks": ["stress_observer"],
        "edits": [("suite:ctr/ctr-test.c", "\t\t\tif (stall > worst)\n", "\t\t\tif (0 && stall > worst)\n")],
    },
    "oom-blame-workload": {
        "what": "every OOM kill of a supervised run is put on the workload's own limit, whatever the evidence says",
        "areas": ["stress"],
        "guest": True,
        "breaks": ["unit", "oom_origin_class", "oom_origin_outside"],
        "edits": [("src/supervise.rs",
                   "    rec.oom_origin = v.origin;\n",
                   "    rec.oom_origin = v.origin.map(|_| oom::Origin::Workload);\n")],
    },
    "kmsg-user-records": {
        "what": "a record a process wrote to the kernel log counts as one of the OOM killer",
        "areas": ["stress"],
        "guest": True,
        "breaks": ["unit", "oom_origin_local.guest"],
        "edits": [("src/oom.rs",
                   "            if by_kernel && msg.starts_with(\"oom-kill:\") {\n",
                   "            if (by_kernel || !msg.is_empty()) && msg.starts_with(\"oom-kill:\") {\n")],
    },
    "no-memory-limit": {
        "what": "the memory limit of a single workload is not written (the class ceilings and the aggregate ceiling still are)",
        "areas": ["limits", "stress"],
        "breaks": ["unit", "memory_oom", "stress_memory"],
        "edits": [("src/domain.rs",
                   "        if let Some(m) = l.memory_max {\n            let swap_held = self.memory_max(rel, m, ",
                   "        if let Some(m) = l.memory_max.filter(|_| rel.matches('/').count() < 3) {\n"
                   "            let swap_held = self.memory_max(rel, m, ")],
    },
}


def mutant(name):
    if name not in MUTANTS:
        sys.exit("resctl-mutants: no defect named %s (one of: %s)" % (name, ", ".join(MUTANTS)))
    return MUTANTS[name]


def apply(name, root, run=None):
    m = mutant(name)
    root = os.path.realpath(root)
    if root == CRATE or root.startswith(CRATE + os.sep):
        sys.exit("resctl-mutants: %s is the crate of the repository; a defect only goes into a copy" % root)
    if run is not None:
        run = os.path.realpath(run)
        if any(run == d or run.startswith(d + os.sep) for d in SUITE):
            sys.exit("resctl-mutants: %s is the suite of the repository; a defect only goes into a copy" % run)
    for rel, old, new in m["edits"]:
        if rel.startswith("suite:"):
            if run is None:
                sys.exit("resctl-mutants: %s edits the suite (%s): give the copy of the suite as RUN_DIR" % (name, rel))
            path = os.path.join(run, rel[len("suite:"):])
        else:
            path = os.path.join(root, rel)
        with open(path, encoding="utf-8") as f:
            text = f.read()
        if text.count(old) != 1:
            sys.exit("resctl-mutants: %s: %s: the text to replace occurs %d times, not once; the table must follow the "
                     "code: %r" % (name, rel, text.count(old), old[:80]))
        with open(path, "w", encoding="utf-8") as f:
            f.write(text.replace(old, new))
    print("resctl-mutants: %s applied to %s (%s)" % (name, root, m["what"]))


def verify(name, status_file):
    m = mutant(name)
    status = {}
    with open(status_file) as f:
        for ln in f:
            if ln.strip():
                rid, st = ln.rstrip("\n").split("\t")
                status[rid] = st
    bad = 0
    if not m["breaks"]:
        print("FAIL  negative control %s: the table names no row the defect must break" % name)
        bad += 1
    for want in m["breaks"]:
        rows = [rid for rid in status if rid == want or ("." not in want and rid.split(".")[0] == want)]
        ran = [rid for rid in rows if status[rid] != "NOTRUN"]
        held = [rid for rid in ran if status[rid] != "FAIL"]
        if not ran:
            print("FAIL  negative control %s: %s did not run, so the defect is not shown to be caught" % (name, want))
            bad += 1
        elif held:
            print("FAIL  negative control %s: %s did not catch the defect (%s)" % (
                name, want, ", ".join("%s=%s" % (r, status[r]) for r in held)))
            bad += 1
        else:
            print("PASS  negative control %s: %s fails (%s)" % (name, want, ", ".join(ran)))
    caught = sorted(rid for rid, st in status.items() if st == "FAIL")
    print("      every row that failed with the defect: %s" % (", ".join(caught) or "none"))
    return 1 if bad else 0


def main(argv):
    if argv[:1] == ["list"] and len(argv) == 1:
        print("\n".join(MUTANTS))
        return 0
    if argv[:1] == ["show"] and len(argv) == 2:
        m = mutant(argv[1])
        print("%s: %s\n  areas: %s%s\n  breaks: %s" % (argv[1], m["what"], " ".join(m["areas"]),
                                                     " (and in the guest)" if m.get("guest") else "", " ".join(m["breaks"])))
        return 0
    if argv[:1] == ["guest"] and len(argv) == 2:
        print("yes" if mutant(argv[1]).get("guest") else "no")
        return 0
    if argv[:1] == ["areas"] and len(argv) == 2:
        print(" ".join(mutant(argv[1])["areas"]))
        return 0
    if argv[:1] == ["apply"] and len(argv) in (3, 4):
        apply(argv[1], argv[2], argv[3] if len(argv) == 4 else None)
        return 0
    if argv[:1] == ["verify"] and len(argv) == 3:
        return verify(argv[1], argv[2])
    print(__doc__.strip().split("\n\n")[1], file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
