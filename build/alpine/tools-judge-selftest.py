#!/usr/bin/env python3
"""Prove that tools-judge.py cannot be satisfied by a report that hides a defect or a limitation.

    tools-judge-selftest.py UNPRIVILEGED.facts PRIVILEGED.facts GUEST.facts

The three files are the facts of a real run that the judge accepts. Each case below damages a copy of them the way a broken
toolchain or a dishonest report would, runs the judge on the copy, and requires it to FAIL with a message that names the
row or capability concerned. Two cases go the other way: a host with a pure cgroup v2 hierarchy (synthetic: the guest's
cgroup v2 results stand in for the host's) must be accepted, and its cgroup v1 rows become limits of the host.
Prints one PASS or FAIL line per case; exit status 1 if any case FAILs.
"""
import os
import subprocess
import sys
import tempfile

JUDGE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "tools-judge.py")
SESSIONS = ("unprivileged", "privileged", "guest")


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


def judge(sessions):
    with tempfile.TemporaryDirectory(prefix="judge-selftest.") as d:
        paths = []
        for name in SESSIONS:
            path = os.path.join(d, name + ".facts")
            with open(path, "w") as f:
                f.write("\n".join(sessions[name].lines) + "\n")
            paths.append(path)
        p = subprocess.run([sys.executable, JUDGE] + paths, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                           universal_newlines=True)
    return p.returncode, p.stdout


def read(path):
    with open(path, errors="replace") as f:
        return [ln.rstrip("\n") for ln in f if ln.strip()]


def pure_v2(s):
    """The privileged session of a host with a unified cgroup v2 hierarchy (synthetic, built from the guest's facts)."""
    p, g = s["privileged"], s["guest"]
    p.set("cgroup_mode", "v2")
    p.set("cgroup_v1_controllers", "none")
    p.set("cgroup_v2_controllers", g.get("g_cgroup_v2_controllers"))
    for fact in ("memory", "pids", "cpu_quota", "cpu_weight", "cpuset", "io"):
        p.set("cgv2_" + fact, g.get("g_cgv2_" + fact))
    for fact, ctl in (("memory", "memory"), ("pids", "pids"), ("cpu_quota", "cpu"), ("cpu_weight", "cpu"),
                      ("cpuset", "cpuset"), ("io", "blkio")):
        p.set("cgv1_" + fact, "unsupported:no cgroup v1 hierarchy has the %s controller mounted" % ctl)


def sub(facts, key, old, new):
    value = facts.get(key)
    assert old in value, "%s does not contain %s" % (key, old)
    facts.set(key, value.replace(old, new))


def case_deny_leaked(s):
    s["unprivileged"].set("deny_loop_device", "leaked:losetup attached /dev/loop0")


def case_deny_reported_ok(s):
    s["unprivileged"].set("deny_mount_cgroup2", "ok")


def case_none_denied(s):
    s["unprivileged"].set("userns_pid_ns", "denied:unshare: Operation not permitted")


def case_kvm_hw_present(s):
    s["privileged"].set("host_cpu_virt_flags", "vmx")


def case_kvm_in_kernel(s):
    sub(s["privileged"], "host_config", "KVM_INTEL=unset", "KVM_INTEL=y")


def case_v2_controller_present(s):
    s["privileged"].set("cgroup_v2_controllers", "cpu cpuset hugetlb io memory pids")


def case_proof_broken(s):
    s["guest"].set("g_cgv2_memory", "fail:boom")


def case_plain_failure(s):
    s["unprivileged"].set("qemu_tcg_serial", "fail:boom")


def case_missing_fact(s):
    s["unprivileged"].drop("perf_stat_sw")


def case_wrong_message(s):
    s["privileged"].set("mount_xfs", "unsupported:mount: some other error")


def case_leak_not_limit(s):
    s["privileged"].set("priv_dev_kvm", "leaked:/dev/kvm is a character device")


def case_extra_fact(s):
    s["unprivileged"].set("bogus_fact", "ok")


def case_v1_present_but_unsupported(s):
    pure_v2(s)
    s["privileged"].set("cgroup_v1_controllers", "blkio cpu cpuacct cpuset memory pids")


def case_v1_failure(s):
    pure_v2(s)
    s["privileged"].set("cgv1_pids", "fail:boom")


def case_ftrace_cause_gone(s):
    s["privileged"].set("host_ftrace_filter_open", "ok")


def case_fd_cap_present(s):
    s["privileged"].set("host_cap_sys_resource", "yes")


# name, mutation, expected rc, text that must appear in the output
CASES = (
    ("the unmodified facts are accepted", lambda s: None, 0, "tools-test:"),
    ("pure cgroup v2 host is accepted and v1 becomes a limit of the host", pure_v2, 0, "cgv1_memory:"),
    ("an unprivileged operation that worked is a failure, not a pass", case_deny_leaked, 1, "deny_loop_device"),
    ("a privileged-only capability reported as working to the unprivileged session fails", case_deny_reported_ok, 1,
     "deny_mount_cgroup2"),
    ("a capability that needs no privilege cannot be denied", case_none_denied, 1, "userns_pid_ns"),
    ("no KVM limit when the CPU exposes virtualization", case_kvm_hw_present, 1, "priv_qemu_kvm"),
    ("no KVM limit when the host kernel has KVM", case_kvm_in_kernel, 1, "priv_qemu_kvm"),
    ("no cgroup v2 limit when the controller is in the unified hierarchy", case_v2_controller_present, 1, "cgv2_memory"),
    ("no limit when the guest does not prove the tool works", case_proof_broken, 1, "cgv2_memory"),
    ("an unexplained failure stays a failure", case_plain_failure, 1, "qemu_tcg_serial"),
    ("a missing fact is a failure", case_missing_fact, 1, "perf_stat_sw"),
    ("a limit needs its own message", case_wrong_message, 1, "mount_xfs"),
    ("a leak is never a limit", case_leak_not_limit, 1, "priv_dev_kvm"),
    ("a fact without an inventory row is a failure", case_extra_fact, 1, "bogus_fact"),
    ("no cgroup v1 limit while a cgroup v1 hierarchy exists", case_v1_present_but_unsupported, 1, "cgv1_memory"),
    ("a failing cgroup v1 row on a pure v2 host stays a failure", case_v1_failure, 1, "cgv1_pids"),
    ("no ftrace limit when the filter files open", case_ftrace_cause_gone, 1, "ftrace_function"),
    ("no file-descriptor limit when the session has CAP_SYS_RESOURCE", case_fd_cap_present, 1, "priv_fd_hard_raise"),
)


def main(argv):
    if len(argv) != 3:
        print("usage: tools-judge-selftest.py UNPRIVILEGED.facts PRIVILEGED.facts GUEST.facts", file=sys.stderr)
        return 2
    base = {name: read(path) for name, path in zip(SESSIONS, argv)}
    failed = 0
    for name, mutate, want_rc, text in CASES:
        sessions = {k: Facts(v) for k, v in base.items()}
        try:
            mutate(sessions)
        except (AssertionError, KeyError) as e:
            print("FAIL  judge selftest: %s: the case cannot be built: %s" % (name, e))
            failed += 1
            continue
        rc, out = judge(sessions)
        fails = [ln for ln in out.splitlines() if ln.startswith("FAIL")]
        if want_rc == 0:
            good = rc == 0 and text in out
            why = "the judge returned %d and reported %d failures" % (rc, len(fails))
        else:
            good = rc == 1 and any(text in ln for ln in fails)
            why = "the judge returned %d and did not fail on %s (%d failures)" % (rc, text, len(fails))
        if good:
            print("PASS  judge selftest: %s" % name)
        else:
            print("FAIL  judge selftest: %s: %s" % (name, why))
            failed += 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
