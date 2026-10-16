#!/usr/bin/env python3
"""Prove that resctl-judge.py cannot be satisfied by a report that hides a defect or a limitation.

    resctl-judge-selftest.py UNPRIVILEGED.facts PRIVILEGED.facts GUEST.facts

The three files are the facts of a real run that the judge accepts. Each case below damages a copy of them, or of
resctl-test/checks.tsv and limits.tsv, the way a broken resource control or a dishonest report would, runs the judge on the
copy, and requires it to FAIL with a message that names the row concerned. Three cases go the other way: the unmodified
facts must be accepted, and so must a host whose unified hierarchy has the memory controller and whose root holds
CAP_SYS_RESOURCE (synthetic: the guest's results stand in for the host's), where the five limits of the host disappear,
and a partial run (judge --partial) that reports
what it ran. The partial cases also require that a partial run judges whatever ran as strictly as a full run, and that it
can neither prove a limitation without the guest nor pass for a full verdict. Prints one PASS or FAIL line per case; exit
status 1 if any case FAILs.
"""
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
JUDGE = os.path.join(HERE, "resctl-judge.py")
CHECKS = os.path.join(HERE, "resctl-test", "checks.tsv")
LIMITS = os.path.join(HERE, "resctl-test", "limits.tsv")
SESSIONS = ("unprivileged", "privileged", "guest")
LIMIT_CLASS = "host-config"
LIMIT_REQUIRES = "a cgroup v2 memory controller in the unified hierarchy"


class Facts:
    def __init__(self, lines):
        self.lines = list(lines)

    def get(self, key):
        for ln in self.lines:
            if ln.startswith(key + "="):
                return ln[len(key) + 1:]
        raise KeyError(key)

    def set(self, key, value):
        for i, ln in enumerate(self.lines):
            if ln.startswith(key + "="):
                self.lines[i] = "%s=%s" % (key, value)
                return
        self.lines.append("%s=%s" % (key, value))

    def drop(self, key):
        n = len(self.lines)
        self.lines = [ln for ln in self.lines if not ln.startswith(key + "=")]
        assert len(self.lines) < n, "no fact %s to drop" % key


def read(path):
    with open(path, errors="replace") as f:
        return [ln.rstrip("\n") for ln in f if ln.strip()]


class Case:
    """The sessions' facts and the texts of the two tables, which a case may damage."""

    def __init__(self, base):
        self.s = {k: Facts(v) for k, v in base.items()}
        self.checks = read_raw(CHECKS)
        self.limits = read_raw(LIMITS)
        self.args = []  # extra arguments of the judge
        self.omit = set()  # sessions whose facts file is not passed (they did not run)

    def drop_checks(self, keep):
        """Keep the data rows of checks.tsv for which keep(columns) holds."""
        self.checks = [ln for ln in self.checks if ln.startswith("#") or not ln.strip() or keep(ln.split("\t"))]

    def edit_check(self, base, column, value):
        out = []
        found = False
        for ln in self.checks:
            cols = ln.split("\t")
            if not ln.startswith("#") and cols[0] == base:
                cols[column] = value
                ln = "\t".join(cols)
                found = True
            out.append(ln)
        assert found, "no checks.tsv row %s" % base
        self.checks = out

    def add_limit(self, rid, accepts, cause, proof, requires=LIMIT_REQUIRES, klass=LIMIT_CLASS):
        self.limits.append("\t".join((rid, klass, accepts, cause, proof, requires, "added by the selftest")))

    def edit_limit(self, rid, column, value):
        out = []
        found = False
        for ln in self.limits:
            cols = ln.split("\t")
            if not ln.startswith("#") and cols[0] == rid:
                cols[column] = value
                ln = "\t".join(cols)
                found = True
            out.append(ln)
        assert found, "no limits.tsv row %s" % rid
        self.limits = out


def read_raw(path):
    with open(path) as f:
        return [ln.rstrip("\n") for ln in f]


def judge(case):
    with tempfile.TemporaryDirectory(prefix="resctl-judge-selftest.") as d:
        paths = []
        for name in SESSIONS:
            if name in case.omit:
                continue
            path = os.path.join(d, name + ".facts")
            with open(path, "w") as f:
                f.write("\n".join(case.s[name].lines) + "\n")
            paths.append(path)
        checks, limits = os.path.join(d, "checks.tsv"), os.path.join(d, "limits.tsv")
        with open(checks, "w") as f:
            f.write("\n".join(case.checks) + "\n")
        with open(limits, "w") as f:
            f.write("\n".join(case.limits) + "\n")
        p = subprocess.run([sys.executable, JUDGE, "--checks", checks, "--limits", limits] + case.args + paths,
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True)
    return p.returncode, p.stdout


def memory_v2_host(c):
    """A host whose unified hierarchy carries the memory controller and whose root holds CAP_SYS_RESOURCE (synthetic,
    built from the guest's facts)."""
    p, g = c.s["privileged"], c.s["guest"]
    for env, facts in (("h_", ("feature_memory", "cgroup_v2_controllers", "system_protection", "oom_group", "memory_high",
                               "cap_sys_resource", "oom_bias_locked")),
                       ("v_", ("feature_memory", "cgroup_v2_controllers", "system_protection"))):
        for fact in facts:
            p.set(env + fact, g.get("g_" + fact))


# ---- damaged facts
def set_fact(session, key, value):
    return lambda c: c.s[session].set(key, value)


def drop_fact(session, key):
    return lambda c: c.s[session].drop(key)


def wrong_session(c):
    line = "g_oom_group=%s" % c.s["guest"].get("g_oom_group")
    c.s["guest"].drop("g_oom_group")
    c.s["privileged"].lines.append(line)


def duplicate_fact(c):
    c.s["privileged"].lines.append("h_cpu_quota=%s" % c.s["privileged"].get("h_cpu_quota"))


def no_guest_session(c):
    c.s["guest"].lines = []


# ---- partial runs (check-resctl.sh --areas, --no-guest, --mutant)
PARTIAL_ENVS = {"host": ("privileged", "h_"), "hostv1": ("privileged", "v_"), "guest": ("guest", "g_")}


def partial(areas=None, omit=(), then=None):
    def mutate(c):
        c.args = ["--partial"] + (["--areas", areas] if areas else [])
        c.omit = set(omit)
        if then:
            then(c)
    return mutate


def drop_area_facts(area):
    """The facts an area outside the run would not have reported."""
    def mutate(c):
        for ln in c.checks:
            cols = ln.split("\t")
            if ln.startswith("#") or len(cols) < 7 or cols[1] != area:
                continue
            for env in cols[3].split(","):
                if env in PARTIAL_ENVS:
                    session, prefix = PARTIAL_ENVS[env]
                    c.s[session].lines = [x for x in c.s[session].lines if not x.startswith(prefix + cols[4] + "=")]
    return mutate


# ---- damaged tables
def no_area(area):
    return lambda c: c.drop_checks(lambda cols: cols[1] != area)


def no_resource(resource):
    return lambda c: c.drop_checks(lambda cols: cols[2] != resource)


def no_row(base):
    return lambda c: c.drop_checks(lambda cols: cols[0] != base)


def vacuous_expectation(c):
    c.edit_check("fmt", 5, "~.*")


def empty_expectation(c):
    c.edit_check("layout", 5, "=")


def limit_on(rid, proof):
    return lambda c: c.add_limit(rid, "^fail:", "h_feature_memory~^v1:", proof)


def limit_without_cause(c):
    c.edit_limit("memory_high.host", 3, "-")


def limit_proof_on_host(c):
    c.edit_limit("memory_high.host", 4, "memory_high.host")


def limit_proof_other_fact(c):
    c.edit_limit("memory_high.host", 4, "memory_oom.guest")


def limit_accepts_anything(c):
    c.edit_limit("memory_high.host", 2, ".*")


def limit_without_requirement(c):
    c.edit_limit("memory_high.host", 5, "-")


def limit_for_nothing(c):
    c.add_limit("no_such_row.host", "^unsupported:", "h_feature_memory~^v1:", "memory_high.guest")


def limit_cause_is_a_fiction(c):
    # a cause that no measurement can contradict is not a cause: it names a fact the sessions never report
    c.edit_limit("memory_high.host", 3, "h_no_such_fact~^v1:")


# name, mutation, expected rc, text that must appear in the output (in a FAIL line when rc is 1)
CASES = (
    ("the unmodified facts are accepted", lambda c: None, 0, "5 limited by the host, 0 failed"),
    ("a partial run is judged on what ran and says that it is not a full verdict",
     partial("process,critical", then=drop_area_facts("throttle")), 0, "== partial run: not a full verdict"),
    ("a failure in an area of a partial run fails it",
     partial("process,critical", then=set_fact("privileged", "h_proc_stop", "fail:boom")), 1, "proc_stop.host"),
    ("a missing fact of an area of a partial run fails it",
     partial("process", then=drop_fact("privileged", "h_proc_stop")), 1, "h_proc_stop"),
    ("a partial run still fails a reported failure outside its areas",
     partial("process", then=set_fact("privileged", "h_cpu_quota", "fail:boom")), 1, "cpu_quota.host"),
    ("a system-domain row of an area of a partial run must pass",
     partial("critical", then=set_fact("privileged", "h_system_canary_critical", "fail:boom")), 1,
     "system_canary_critical.host"),
    ("a partial run without the guest cannot prove a limitation of the host", partial(omit=("guest",)), 1,
     "did not run (partial run)"),
    ("a run of some areas that does not say it is partial is refused",
     lambda c: c.args.extend(["--areas", "process"]), 1, "needs --partial"),
    ("a host with the memory controller in the unified hierarchy and CAP_SYS_RESOURCE is accepted with no limit",
     memory_v2_host, 0,
     "0 limited by the host, 0 failed"),
    ("a refused init that worked is a failure, not a pass", set_fact("unprivileged", "u_init_denied", "leaked:resctl created the domains"),
     1, "init_denied.unpriv"),
    ("a workload started where it must be refused fails", set_fact("unprivileged", "u_run_denied", "leaked:the workload ran"),
     1, "run_denied.unpriv"),
    ("entering the system domain unprivileged fails when it is not refused", set_fact("unprivileged", "u_enter_denied", "ok"),
     1, "enter_denied.unpriv"),
    ("a refusal for another reason is not the expected refusal",
     set_fact("unprivileged", "u_init_denied", "denied:resctl: io: boom"), 1, "init_denied.unpriv"),
    ("the unprivileged session must have no cgroup hierarchy",
     set_fact("unprivileged", "u_no_hierarchy", "fail:/sys/fs/cgroup exists"), 1, "no_hierarchy.unpriv"),
    ("fewer than 250 unit tests is a failure", set_fact("unprivileged", "u_unit", "ok 12 tests"), 1, "unit.unpriv"),
    ("just under 250 unit tests is a failure", set_fact("unprivileged", "u_unit", "ok 249 tests"), 1, "unit.unpriv"),
    ("a missing fact is a failure", drop_fact("privileged", "h_memory_oom"), 1, "h_memory_oom"),
    ("a whole session that did not run is a failure", no_guest_session, 1, "g_oom_group"),
    ("a fact without a row is a failure", set_fact("privileged", "h_bogus", "ok"), 1, "h_bogus"),
    ("a fact from the wrong session is a failure", wrong_session, 1, "came from the session"),
    ("a fact emitted twice is a failure", duplicate_fact, 1, "emitted twice"),
    ("an unexplained failure stays a failure", set_fact("privileged", "h_cpu_quota", "fail:boom"), 1, "cpu_quota.host"),
    ("a failing memory limit stays a failure", set_fact("privileged", "h_memory_oom", "fail:boom"), 1, "memory_oom.host"),
    ("a failing I/O throttle stays a failure", set_fact("privileged", "h_io_throttle", "fail:boom"), 1, "io_throttle.host"),
    ("a leak is never a limit", set_fact("privileged", "h_oom_group", "leaked:one process was killed"), 1, "oom_group.host"),
    ("a limit needs its own message", set_fact("privileged", "h_memory_high", "unsupported:something else"), 1, "memory_high.host"),
    ("no limit when the guest does not prove the tool works", set_fact("guest", "g_oom_group", "fail:boom"), 1, "oom_group.host"),
    ("no limit when the unified hierarchy has the memory controller",
     set_fact("privileged", "h_cgroup_v2_controllers", "hugetlb memory"), 1, "memory_high.host"),
    ("no limit when memory is served by cgroup v2", set_fact("privileged", "h_feature_memory", "v2:/sys/fs/cgroup"), 1,
     "oom_group.host"),
    ("no limit of the v1 lifecycle when its memory is served by cgroup v2",
     set_fact("privileged", "v_feature_memory", "v2:/sys/fs/cgroup"), 1, "system_protection.hostv1"),
    ("the system domain canary killed on the host is a failure", set_fact("privileged", "h_system_canary", "fail:the canary died"),
     1, "system_canary.host"),
    ("the system domain canary killed by a termination is a failure",
     set_fact("guest", "g_system_canary_terminate", "fail:killed with the workload"), 1, "system_canary_terminate.guest"),
    ("the system domain canary killed by a recovery is a failure",
     set_fact("privileged", "h_system_canary_recover", "fail:recover killed it"), 1, "system_canary_recover.host"),
    ("the system domain canary killed under the v1 lifecycle is a failure",
     set_fact("privileged", "v_system_canary_lifecycle", "fail:the freezer killed it"), 1, "system_canary_lifecycle.hostv1"),
    ("a workload that reaches the system domain is a failure", set_fact("privileged", "h_system_unreachable", "leaked:killed system"),
     1, "system_unreachable.host"),
    ("a leftover domain after the run is a failure", set_fact("privileged", "h_leftovers", "fail:agent-interaction/workload/batch/x"),
     1, "leftovers.host"),
    ("a guest that did not boot is a failure", set_fact("guest", "guest_boot", "fail:no kernel"), 1, "guest_boot.bed"),
    ("a guest script that failed is a failure", set_fact("guest", "guest_script_rc", "1"), 1, "guest_script_rc.bed"),
    ("a limit on an isolation row is rejected",
     limit_on("system_canary.host", "system_canary.guest"), 1, "can be limited"),
    ("a limit on a termination row is rejected", limit_on("kill_frozen.host", "kill_frozen.guest"), 1, "can be limited"),
    ("a limit on a recovery row is rejected", limit_on("recover_orphan.host", "recover_orphan.guest"), 1, "can be limited"),
    ("a limit on a deny row is rejected", limit_on("init_denied.unpriv", "init_denied.guest"), 1, "deny:"),
    ("a limit on a row of the guest is rejected", limit_on("memory_high.guest", "memory_high.host"), 1, "can be limited"),
    ("a limit needs a measured cause", limit_without_cause, 1, "measured cause"),
    ("a limit whose cause was never measured is not accepted", limit_cause_is_a_fiction, 1, "memory_high.host"),
    ("a limit must be proven by the same fact in the guest", limit_proof_on_host, 1, "the proof must name"),
    ("a limit cannot be proven by another fact", limit_proof_other_fact, 1, "the proof must name"),
    ("a limit cannot accept every failure", limit_accepts_anything, 1, "accepts"),
    ("a limit must say what the host would have to provide", limit_without_requirement, 1, "must say what the host"),
    ("a limit for a row that does not exist is rejected", limit_for_nothing, 1, "no such row"),
    ("an environment without the area terminate is a failure", no_area("terminate"), 1, "no row exercises the area terminate"),
    ("an environment without the area pressure is a failure", no_area("pressure"), 1, "no row exercises the area pressure"),
    ("a host without a row for the resource io is a failure", no_resource("io"), 1, "no row controls the resource io"),
    ("a host without a row for the resource processes is a failure", no_resource("processes"), 1,
     "no row controls the resource processes"),
    ("a missing system-domain row is a failure", no_row("system_canary_reinit"), 1, "system_canary_reinit.host does not exist"),
    ("a vacuous expectation is a failure", vacuous_expectation, 1, "vacuous"),
    ("an expectation that accepts an empty value is a failure", empty_expectation, 1, "accepts an empty value"),
    # ---- the process lifecycle and the critical processes
    ("a crash loop that is not stopped is a failure", set_fact("privileged", "h_proc_crash_loop", "fail:restarts=7"), 1,
     "proc_crash_loop.host"),
    ("an orphan left behind by a workload is a failure", set_fact("guest", "g_proc_orphans", "fail:sleep 4801 is alive"), 1,
     "proc_orphans.guest"),
    ("a stale domain after a lifecycle run is a failure", set_fact("privileged", "v_proc_cleanup", "fail:3 directories left"), 1,
     "proc_cleanup.hostv1"),
    ("an unexpected exit taken for a requested stop is a failure",
     set_fact("privileged", "h_proc_unexpected", "fail:ended=requested:stop"), 1, "proc_unexpected.host"),
    ("an audit trail with a gap is a failure", set_fact("guest", "g_proc_audit", "fail:sequence gap at 12"), 1, "proc_audit.guest"),
    ("a restart storm is a failure", set_fact("privileged", "h_proc_start_rate", "fail:the fourth start ran"), 1,
     "proc_start_rate.host"),
    ("the system domain canary killed by a lifecycle scenario is a failure",
     set_fact("guest", "g_system_canary_process", "fail:the canary died"), 1, "system_canary_process.guest"),
    ("a critical process killed with a workload is a failure",
     set_fact("privileged", "h_crit_terminate", "fail:critd:gone"), 1, "crit_terminate.host"),
    ("a critical process frozen with a workload is a failure",
     set_fact("privileged", "v_crit_freeze", "fail:kfz_heartbeat_while_frozen='no'"), 1, "crit_freeze.hostv1"),
    ("a critical process left in a workload domain is a failure",
     set_fact("guest", "g_crit_misplaced", "fail:kmm1_rescues='0 0' (wanted '1 1')"), 1, "crit_misplaced.guest"),
    ("a workload that obtained protected status is a failure",
     set_fact("privileged", "h_crit_no_obtain", "leaked:the main process is registered"), 1, "crit_no_obtain.host"),
    ("a workload that changed the registry is a failure",
     set_fact("privileged", "v_crit_no_request", "leaked:protect from inside a workload worked"), 1, "crit_no_request.hostv1"),
    ("a policy that grants protected status is a failure",
     set_fact("privileged", "h_crit_policy", "leaked:a policy section protect was accepted"), 1, "crit_policy.host"),
    ("a workload that reached the registry as another user is a failure",
     set_fact("guest", "g_crit_uid_drop", "fail:ud_registry='1' (wanted '0')"), 1, "crit_uid_drop.guest"),
    ("a corrupt registry that killed a critical process is a failure",
     set_fact("privileged", "h_crit_registry", "fail:damaged_kill='0 no'"), 1, "crit_registry.host"),
    ("a critical process that did not survive the run is a failure",
     set_fact("guest", "g_crit_survival", "fail:same_critd='99 1' (wanted '98 1')"), 1, "crit_survival.guest"),
    ("the system domain canary killed by a critical-process scenario is a failure",
     set_fact("privileged", "h_system_canary_critical", "fail:the canary died"), 1, "system_canary_critical.host"),
    ("a missing process-lifecycle canary row is a failure", no_row("system_canary_process"), 1,
     "system_canary_process.host does not exist"),
    ("a missing critical-process canary row is a failure", no_row("system_canary_critical"), 1,
     "system_canary_critical.hostv1 does not exist"),
    ("an environment without the area process is a failure", no_area("process"), 1, "no row exercises the area process"),
    ("an environment without the area critical is a failure", no_area("critical"), 1, "no row exercises the area critical"),
    ("a limit on a process-lifecycle row is rejected",
     limit_on("proc_crash_loop.host", "proc_crash_loop.guest"), 1, "can be limited"),
    ("a limit on a critical-process row is rejected", limit_on("crit_survival.host", "crit_survival.guest"), 1, "can be limited"),
    ("an environment without the area stress is a failure", no_area("stress"), 1, "no row exercises the area stress"),
    ("a limit on an exhaustion row is rejected", limit_on("stress_stall.host", "stress_memory.guest"), 1, "can be limited"),
    ("a stall of the system domain beyond its bound is a failure",
     set_fact("privileged", "h_stress_stall", "fail:the longest stall of the beat was 2.10s"), 1, "stress_stall.host"),
    ("an OOM kill in the system domain fails a system-domain row",
     set_fact("privileged", "v_stress_system", "fail:boom"), 1, "the system-domain row stress_system.hostv1"),
    ("a control plane that was not timed is a failure", drop_fact("privileged", "h_stress_control"), 1, "h_stress_control"),
    ("a workload that can lower its OOM bias is a failure",
     set_fact("guest", "g_oom_bias_locked", "fail:rc=0 '65534 lowered 0' with CAP_SYS_RESOURCE yes"),
     1, "oom_bias_locked.guest"),
    ("a lowered OOM bias where resctl holds CAP_SYS_RESOURCE is a failure, not a limit",
     set_fact("privileged", "h_cap_sys_resource", "yes CapEff=000001ffffffffff"), 1, "oom_bias_locked.host"),
    ("a machine-wide OOM that took a critical service is a failure",
     set_fact("guest", "g_global_oom", "fail: critical_alive='no 0' (wanted 'yes 1')"), 1, "global_oom.guest"),
    ("supervising where it must be refused is a failure",
     set_fact("unprivileged", "u_supervise_denied", "leaked:the workload ran"), 1, "supervise_denied.unpriv"),
    ("registering a critical process where it must be refused is a failure",
     set_fact("unprivileged", "u_protect_denied", "ok"), 1, "protect_denied.unpriv"),
    ("a command that changes something and is not refused is a failure",
     set_fact("unprivileged", "u_mutating_denied", "fail:'teardown'=rc0"), 1, "mutating_denied.unpriv"),
    ("a policy that grants protected status is a failure unprivileged too",
     set_fact("unprivileged", "u_policy_protection", "fail:protect:rc=0"), 1, "policy_protection.unpriv"),
    ("an unbounded restart policy is a failure", set_fact("unprivileged", "u_restart_bounds", "fail:[s/^max = 3$/max = 11/]=rc0"), 1,
     "restart_bounds.unpriv"),
    ("unsafe code in the crate is a failure", set_fact("unprivileged", "u_no_unsafe", "fail:lines using unsafe=1"), 1,
     "no_unsafe.unpriv"),
)


def main(argv):
    if len(argv) != 3:
        print("usage: resctl-judge-selftest.py UNPRIVILEGED.facts PRIVILEGED.facts GUEST.facts", file=sys.stderr)
        return 2
    base = {name: read(path) for name, path in zip(SESSIONS, argv)}
    failed = 0
    for name, mutate, want_rc, text in CASES:
        case = Case(base)
        try:
            mutate(case)
        except (AssertionError, KeyError) as e:
            print("FAIL  judge selftest: %s: the case cannot be built: %s" % (name, e))
            failed += 1
            continue
        rc, out = judge(case)
        fails = [ln for ln in out.splitlines() if ln.startswith("FAIL")]
        if want_rc == 0:
            good = rc == 0 and text in out
            why = "the judge returned %d and reported %d failures" % (rc, len(fails))
        else:
            good = rc == 1 and any(text in ln for ln in fails)
            why = "the judge returned %d and did not fail on '%s' (%d failures)" % (rc, text, len(fails))
        if good:
            print("PASS  judge selftest: %s" % name)
        else:
            print("FAIL  judge selftest: %s: %s" % (name, why))
            failed += 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
