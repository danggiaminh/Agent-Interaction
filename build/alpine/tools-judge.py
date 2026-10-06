#!/usr/bin/env python3
"""Judge the facts of the tools test sessions against tools-test/inventory.tsv, limits.tsv and capabilities.tsv.

    tools-judge.py FACTS_FILE...

One facts file per session, named after it: unprivileged.facts (tools-test/unpriv.sh, a user namespace without host
privileges), privileged.facts (tools-test/priv.sh, real root of the host) and guest.facts (tools-test/guest.sh inside the
QEMU test bed, which boots its own kernel). Each holds KEY=VALUE lines; a fact belongs to exactly one session.

inventory.tsv has one row per fact, tab separated:  id  packages  fact  expectation  description
`packages` are the packages the row exercises (comma separated; each must be a package of guest/tools.pkgs or
guest/dev.pkgs; `-` for a fact about the session or the host that no package provides). Expectations:
    ok              the value is "ok", or "ok " followed by the evidence the test measured
    =VALUE          the value equals VALUE (possibly empty)
    ver:PKG         the value contains the locked version of PKG (without the -rN release)
    ~REGEX          the value matches REGEX (re.search); it must not also match an empty, failed, unsupported, denied
                    or leaked value, so a vacuous pattern cannot pass
    deny:REGEX      the session is REFUSED the operation: the value is `denied:<message>` and the message matches REGEX.
                    A value that starts with `leaked:` (the operation worked) is a failure.

Every row ends as one of
    PASS      the expectation holds
    DENIED    a `deny:` row: the operation was refused, as it must be for this privilege tier
    LIMIT     the expectation fails because of a recorded limitation of the HOST (limits.tsv, class host-*)
    UPSTREAM  the expectation fails because of a recorded defect of an upstream package (limits.tsv, class upstream)
    FAIL      anything else: a toolchain defect, a missing fact, an unexplained limit
A failed row becomes LIMIT or UPSTREAM only through an entry of limits.tsv (id class accepts cause proof requires
explanation) whose three conditions all hold: the value matches `accepts` (a specific pattern, never one that matches any
failure); every condition of `cause` holds on the measured facts of any session (`FACT~REGEX` or `FACT!~REGEX`, joined by
` && `); and every row named in `proof` is PASS, which shows that the tool works wherever the cause does not apply.
There is no other way to a limit: the inventory has no escape hatch.

capabilities.tsv (capability area needs unprivileged privileged guest description) names a row per privilege tier for
each capability and checks the claims. A capability that needs root must be DENIED to the unprivileged session and never
PASS there, and must be PASS (or LIMIT) for the real root of the host, or PASS in the guest where the host must not be
harmed. A capability that needs nothing must not be DENIED. A capability that needs a host feature must be PASS or LIMIT
on the privileged host. A guest cell, when present, must be PASS. The report prints the matrix, the limits by class with
their measured cause and proof, and the host features that would remove them.

Besides the rows, the inventory must cover every package of guest/tools.pkgs, every emitted fact must have a row, every
row an emitted fact, and the test sources must not hard-code a cgroup layout. Exit status 1 on any FAIL.
"""
import collections
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
TESTS = os.path.join(HERE, "tools-test")
INVENTORY = os.path.join(TESTS, "inventory.tsv")
LIMITS = os.path.join(TESTS, "limits.tsv")
CAPABILITIES = os.path.join(TESTS, "capabilities.tsv")

CLASSES = ("host-hw", "host-kernel", "host-config", "upstream")
CLASS_TITLES = {
    "host-hw": "the host hardware or hypervisor does not expose it",
    "host-kernel": "the host kernel build or policy does not provide it",
    "host-config": "the host configuration (boot parameters, cgroup layout, capabilities, limits) does not provide it",
    "upstream": "an upstream package misbehaves (a verified workaround exists)",
}
NEEDS = ("none", "root", "host")
TAGS = {"unprivileged": "unpriv", "privileged": "priv", "guest": "guest"}
# values that mean "did not work" in every row: a pattern that matches one of them would accept a failure
PROBES = ("", "ok", "fail:x", "unsupported:x", "x y z", "denied:x", "leaked:x")
META = r"\\\[\](){}*+?|.^$"
# cgroup hierarchies are discovered by tools-test/cgroup.sh; nothing else may name a controller mount point
CGROUP_PATH = re.compile(r"/sys/fs/cgroup/(memory|pids|cpu|cpuacct|cpuset|blkio|io|devices|freezer|unified|systemd)\b")


def read_list(path):
    with open(path) as f:
        return [ln.split("#", 1)[0].strip() for ln in f if ln.split("#", 1)[0].strip()]


def read_lock(path):
    versions = {}
    with open(path) as f:
        for ln in f:
            if ln.startswith("#") or not ln.strip():
                continue
            name, version = ln.split()[:2]
            versions[name] = re.sub(r"-r[0-9]+$", "", version)
    return versions


def read_tsv(path, ncols, problems):
    rows = []
    with open(path) as f:
        for n, ln in enumerate(f, 1):
            ln = ln.rstrip("\n")
            if not ln.strip() or ln.startswith("#"):
                continue
            cols = ln.split("\t")
            if len(cols) != ncols:
                problems.append("%s line %d has %d columns, wanted %d" % (os.path.basename(path), n, len(cols), ncols))
                continue
            rows.append((n, cols))
    return rows


def short(value, n=110):
    value = " ".join(value.split())
    return value if len(value) <= n else value[: n - 3] + "..."


def evaluate(value, expectation, versions):
    """True, False, or (None, reason) when the expectation itself is unusable."""
    if expectation == "ok":
        return value == "ok" or value.startswith("ok ")
    if expectation.startswith("="):
        return value == expectation[1:]
    if expectation.startswith("ver:"):
        pkg = expectation[4:]
        if pkg not in versions:
            return None, "no locked version for %s" % pkg
        return versions[pkg] in value
    if expectation.startswith("~"):
        return re.search(expectation[1:], value) is not None
    if expectation.startswith("deny:"):
        return value.startswith("denied:") and re.search(expectation[5:], value[len("denied:"):]) is not None
    return None, "unknown expectation syntax"


def check_expectation(expectation):
    """Problem text for an expectation that could never fail, or None."""
    if expectation.startswith(("~", "deny:")):
        rx = expectation.split(":", 1)[1] if expectation.startswith("deny:") else expectation[1:]
        try:
            compiled = re.compile(rx)
        except re.error as e:
            return "invalid regular expression: %s" % e
        if expectation.startswith("~"):
            for probe in PROBES:
                if compiled.search(probe):
                    return "the pattern matches the failure value %r, so it is vacuous" % probe
    return None


def check_accepts(rx):
    """Problem text for an `accepts` pattern that is too loose to name one specific limitation, or None."""
    if rx == "^$":
        return None
    try:
        compiled = re.compile(rx)
    except re.error as e:
        return "invalid regular expression: %s" % e
    for probe in PROBES:
        if compiled.search(probe):
            return "matches the failure value %r, so it would accept any failure" % probe
    if re.fullmatch(r"\^[^%s]+\$" % META, rx):
        return None
    if max(len(tok) for tok in re.split("[%s]+" % META, rx)) >= 5:
        return None
    return "names no literal of at least 5 characters, so it is not specific to one limitation"


COND = re.compile(r"^([a-z0-9_]+)(!?~)(.*)$")


def parse_cause(cause):
    conds = []
    for part in cause.split(" && "):
        m = COND.match(part.strip())
        if not m:
            return None, "unusable condition '%s' (wanted FACT~REGEX or FACT!~REGEX)" % part
        try:
            re.compile(m.group(3))
        except re.error as e:
            return None, "condition '%s': %s" % (part, e)
        conds.append((m.group(1), m.group(2) == "!~", m.group(3)))
    return conds, None


def main(argv):
    problems = []
    facts = {}  # name -> (value, session)
    sessions = []
    for path in argv:
        session = re.sub(r"\.facts$", "", os.path.basename(path))
        sessions.append(session)
        with open(path, errors="replace") as f:
            for ln in f:
                ln = ln.rstrip("\n")
                if not re.match(r"^[a-z0-9_]+=", ln):
                    continue
                key, value = ln.split("=", 1)
                if key in facts:
                    problems.append("fact %s is emitted twice (sessions %s and %s)" % (key, facts[key][1], session))
                facts[key] = (value, session)

    versions = {}
    versions.update(read_lock(os.path.join(HERE, "guest", "dev.lock")))
    versions.update(read_lock(os.path.join(HERE, "guest", "tools.lock")))
    versions.update(read_lock(os.path.join(HERE, "guest", "kernel.lock")))
    tools_pkgs = set(read_list(os.path.join(HERE, "guest", "tools.pkgs")))
    known_pkgs = tools_pkgs | set(read_list(os.path.join(HERE, "guest", "dev.pkgs")))

    rows = []  # (line, id, pkgs, fact, expectation, description)
    for n, cols in read_tsv(INVENTORY, 5, problems):
        rows.append((n, cols[0], cols[1], cols[2], cols[3], cols[4]))
    row_by_id = {}
    for r in rows:
        if r[1] in row_by_id:
            problems.append("row id %s is used twice" % r[1])
        row_by_id[r[1]] = r

    limits = {}
    for n, cols in read_tsv(LIMITS, 7, problems):
        rid, klass, accepts, cause, proof, requires, explanation = cols
        where = "limits.tsv line %d (%s)" % (n, rid)
        if rid in limits:
            problems.append("%s: a second entry for the same row" % where)
            continue
        if rid not in row_by_id:
            problems.append("%s: no such inventory row" % where)
            continue
        if row_by_id[rid][4].startswith("deny:"):
            problems.append("%s: a deny: row states a refusal as the expectation, it cannot also be limited" % where)
            continue
        if klass not in CLASSES:
            problems.append("%s: class '%s' is none of %s" % (where, klass, ", ".join(CLASSES)))
            continue
        why = check_accepts(accepts)
        if why:
            problems.append("%s: accepts %s" % (where, why))
            continue
        if cause == "-":
            if klass != "upstream":
                problems.append("%s: only an upstream limitation can go without a measured cause" % where)
                continue
            conds = []
        else:
            conds, why = parse_cause(cause)
            if why:
                problems.append("%s: %s" % (where, why))
                continue
        proofs = proof.split()
        bad = [p for p in proofs if p not in row_by_id or p == rid]
        if not proofs or bad:
            problems.append("%s: proof must name existing rows other than itself (%s)" % (where, ", ".join(bad) or "none given"))
            continue
        if klass != "upstream" and requires == "-":
            problems.append("%s: a host limitation must say what the host would have to provide" % where)
            continue
        limits[rid] = {"class": klass, "accepts": accepts, "cause": cause, "conds": conds, "proofs": proofs,
                       "requires": requires, "explanation": explanation}

    # the pairs of the rows
    status = {}  # row id -> PASS | DENIED | LIMIT | UPSTREAM | FAIL
    detail = {}  # row id -> (kind, line to print)
    row_session = {}
    pass1 = {}
    pending = []
    seen_facts, covered = set(), set()
    for line, rid, pkgs, fact, expectation, description in rows:
        seen_facts.add(fact)
        for p in pkgs.split(","):
            p = p.strip()
            if p == "-":
                continue
            if p not in known_pkgs:
                problems.append("row %s names %s, which is in neither tools.pkgs nor dev.pkgs" % (rid, p))
            covered.add(p)
        why = check_expectation(expectation)
        if why:
            problems.append("row %s: %s" % (rid, why))
            status[rid] = "FAIL"
            continue
        if fact not in facts:
            status[rid] = "FAIL"
            detail[rid] = ("FAIL", "%s: fact %s was not reported (%s)" % (rid, fact, description))
            row_session[rid] = None
            continue
        value, session = facts[fact]
        row_session[rid] = session
        res = evaluate(value, expectation, versions)
        if isinstance(res, tuple):
            problems.append("row %s: %s" % (rid, res[1]))
            status[rid] = "FAIL"
            continue
        if res:
            kind = "DENIED" if expectation.startswith("deny:") else "PASS"
            status[rid] = pass1[rid] = kind
            detail[rid] = (kind, "%s: %s" % (description, short(value)))
        else:
            pending.append((rid, fact, expectation, description, value))

    # second pass: a failed row is a limitation only when its entry in limits.tsv is proven
    def holds(cond):
        name, negate, rx = cond
        if name not in facts:
            return False
        return (re.search(rx, facts[name][0]) is None) if negate else (re.search(rx, facts[name][0]) is not None)

    def cause_text(entry):
        # the evidence of each condition: the part of the fact that matched (a fact like host_config is long), or the
        # value for a condition about something that is absent
        parts = collections.OrderedDict()
        for name, negate, rx in entry["conds"]:
            if name not in facts:
                text = "(not reported)"
            elif negate:
                text = short(facts[name][0], 60)
            else:
                m = re.search(rx, facts[name][0])
                text = m.group(0).strip() if m and m.group(0).strip() else short(facts[name][0], 60)
            if text not in parts.setdefault(name, []):
                parts[name].append(text)
        return ", ".join("%s=%s" % (n, " ".join(t)) for n, t in parts.items())

    for rid, fact, expectation, description, value in pending:
        entry = limits.get(rid)
        wanted = "got '%s', wanted %s" % (short(value), expectation)
        if entry is None:
            status[rid] = "FAIL"
            detail[rid] = ("FAIL", "%s: %s: %s" % (rid, description, wanted))
            continue
        unmet = None
        if value.startswith("leaked:"):
            unmet = "the value is a leak, never a limitation"
        elif re.search(entry["accepts"], value) is None:
            unmet = "the value does not match the accepted pattern %s" % entry["accepts"]
        else:
            for cond in entry["conds"]:
                if not holds(cond):
                    unmet = "the cause is not established: %s%s%s" % (cond[0], "!~" if cond[1] else "~", cond[2])
                    if cond[0] in facts:
                        unmet += " (%s=%s)" % (cond[0], short(facts[cond[0]][0], 60))
                    else:
                        unmet += " (fact not reported)"
                    break
        if unmet is None:
            for p in entry["proofs"]:
                if pass1.get(p) != "PASS":
                    unmet = "the proof row %s is not PASS (%s)" % (p, status.get(p, "FAIL"))
                    break
        if unmet:
            status[rid] = "FAIL"
            detail[rid] = ("FAIL", "%s: %s: %s; the limit entry is rejected: %s" % (rid, description, wanted, unmet))
            continue
        kind = "UPSTREAM" if entry["class"] == "upstream" else "LIMIT"
        status[rid] = kind
        detail[rid] = (kind, "%s: %s" % (description, short(value)))

    # the capabilities: privilege tiers must be told apart
    caps = []
    for n, cols in read_tsv(CAPABILITIES, 7, problems):
        cap, area, needs, unpriv, priv, guest, description = cols
        where = "capabilities.tsv line %d (%s)" % (n, cap)
        if needs not in NEEDS:
            problems.append("%s: needs '%s' is none of %s" % (where, needs, ", ".join(NEEDS)))
            continue
        cells = {"unpriv": unpriv, "priv": priv, "guest": guest}
        ok = True
        for tier, cell in cells.items():
            if cell != "-" and cell not in row_by_id:
                problems.append("%s: %s cell names the unknown row %s" % (where, tier, cell))
                ok = False
        if ok:
            caps.append((where, cap, area, needs, cells, description))

    referenced = set()
    cap_fail = []
    for where, cap, area, needs, cells, description in caps:
        referenced.update(c for c in cells.values() if c != "-")
        us = status.get(cells["unpriv"]) if cells["unpriv"] != "-" else None
        ps = status.get(cells["priv"]) if cells["priv"] != "-" else None
        gs = status.get(cells["guest"]) if cells["guest"] != "-" else None
        if needs == "none":
            if cells["unpriv"] == "-" or us not in ("PASS", "LIMIT"):
                cap_fail.append("%s needs no privilege, so the unprivileged session must be PASS or LIMIT, it is %s" % (cap, us or "not probed"))
        elif needs == "root":
            if cells["unpriv"] == "-" or not row_by_id[cells["unpriv"]][4].startswith("deny:"):
                cap_fail.append("%s needs root: the unprivileged cell must be a deny: row that proves the refusal" % cap)
            elif us != "DENIED":
                cap_fail.append("%s needs root but the unprivileged session is %s instead of DENIED" % (cap, us))
            # the host is never made to do what would harm it (it keeps its clock): there the guest, whose root is
            # real inside a virtual machine, is the privileged proof
            if cells["priv"] == "-":
                if gs != "PASS":
                    cap_fail.append("%s needs root: neither the privileged session nor the guest has a PASSing row" % cap)
            elif ps not in ("PASS", "LIMIT"):
                cap_fail.append("%s needs root: the privileged session must be PASS or LIMIT, it is %s" % (cap, ps or "not probed"))
        else:
            if cells["priv"] == "-" or ps not in ("PASS", "LIMIT"):
                cap_fail.append("%s needs a host feature: the privileged session must be PASS or LIMIT, it is %s" % (cap, ps or "not probed"))
            if us == "PASS" and cells["unpriv"] != "-" and row_by_id[cells["unpriv"]][4].startswith("deny:"):
                cap_fail.append("%s: a deny: row cannot be PASS" % cap)
        if cells["guest"] != "-" and gs != "PASS":
            cap_fail.append("%s: the guest session must be PASS, it is %s" % (cap, gs))
        for tier, cell in cells.items():
            if cell != "-" and status.get(cell) == "FAIL":
                cap_fail.append("%s: the %s row %s failed" % (cap, tier, cell))
    for line, rid, _, fact, expectation, _ in rows:
        if expectation.startswith("deny:") and rid not in referenced:
            problems.append("deny row %s proves a refusal but no capability in capabilities.tsv refers to it" % rid)

    # the environment-specific assumptions of the test sources
    for name in sorted(os.listdir(TESTS)):
        if not name.endswith(".sh") or name == "cgroup.sh":
            continue
        with open(os.path.join(TESTS, name), errors="replace") as f:
            for n, ln in enumerate(f, 1):
                if ln.lstrip().startswith("#"):
                    continue
                m = CGROUP_PATH.search(ln)
                if m:
                    problems.append("%s:%d hard-codes %s; the cgroup layout is detected by cgroup.sh only" % (name, n, m.group(0)))

    for fact in sorted(set(facts) - seen_facts):
        problems.append("fact %s has no inventory row" % fact)
    for p in sorted(tools_pkgs - covered):
        problems.append("package %s of tools.pkgs is exercised by no inventory row" % p)
    for rid, entry in sorted(limits.items()):
        pass  # entries for rows that pass are fine: another environment may need them

    # ---- report
    def tag(rid):
        s = row_session.get(rid)
        return "[%s]" % TAGS.get(s, s) if s else "[?]"

    print("== rows")
    counts = collections.Counter()
    per_session = collections.defaultdict(collections.Counter)
    for line, rid, pkgs, fact, expectation, description in rows:
        kind = status.get(rid, "FAIL")
        counts[kind] += 1
        per_session[row_session.get(rid) or "(not reported)"][kind] += 1
        if rid in detail:
            print("%-8s%-8s%s" % (detail[rid][0], tag(rid), detail[rid][1]))
    for p in problems:
        counts["FAIL"] += 1
        print("%-8s%s" % ("FAIL", "inventory: %s" % p))
    for p in cap_fail:
        counts["FAIL"] += 1
        print("%-8s%s" % ("FAIL", "capability: %s" % p))

    print("== environments (rows per privilege tier)")
    print("  %-14s %5s %7s %6s %9s %5s" % ("session", "PASS", "DENIED", "LIMIT", "UPSTREAM", "FAIL"))
    for session in sorted(per_session, key=lambda s: (list(TAGS).index(s) if s in TAGS else 9, s)):
        c = per_session[session]
        print("  %-14s %5d %7d %6d %9d %5d" % (session, c["PASS"], c["DENIED"], c["LIMIT"], c["UPSTREAM"], c["FAIL"]))
    missing = [s for s in TAGS if s not in sessions]
    for s in missing:
        print("  %-14s not run" % s)

    print("== capabilities (the privilege tier decides: unpriv = sandbox without host privileges, priv = host root, guest = test bed kernel)")
    areas = collections.OrderedDict()
    for where, cap, area, needs, cells, description in caps:
        areas.setdefault(area, []).append((cap, needs, cells, description))

    def cell_text(cell):
        return "-" if cell == "-" else status.get(cell, "FAIL")

    for area, items in areas.items():
        print("  %s" % area)
        print("    %-36s %-5s %-8s %-8s %-8s" % ("capability", "needs", "unpriv", "priv", "guest"))
        for cap, needs, cells, description in items:
            print("    %-36s %-5s %-8s %-8s %-8s" % (cap, needs, cell_text(cells["unpriv"]), cell_text(cells["priv"]), cell_text(cells["guest"])))

    print("== limits of the host (recorded in limits.tsv, each with its measured cause and the row that proves the tool works elsewhere)")
    by_class = collections.OrderedDict((k, []) for k in CLASSES)
    for rid, kind in status.items():
        if kind in ("LIMIT", "UPSTREAM"):
            by_class[limits[rid]["class"]].append(rid)
    if not any(by_class.values()):
        print("  none")
    for klass, rids in by_class.items():
        if not rids:
            continue
        print("  %s: %s" % (klass, CLASS_TITLES[klass]))
        for rid in rids:
            entry = limits[rid]
            print("    %s: %s" % (rid, entry["explanation"]))
            if entry["conds"]:
                print("      measured: %s" % cause_text(entry))
            print("      proof: %s" % ", ".join("%s=%s" % (p, status.get(p)) for p in entry["proofs"]))

    print("== verdict")
    host_facts = {}
    for where, cap, area, needs, cells, description in caps:
        host_cell = cells["unpriv"] if needs == "none" else cells["priv"]
        # a capability the host is never made to exercise (its clock) is proven by the guest alone
        host_status = status.get(host_cell) if host_cell != "-" else "GUEST"
        host_facts.setdefault(area, []).append((cap, host_status, status.get(cells["guest"]) if cells["guest"] != "-" else None))
    for area, items in host_facts.items():
        native = [c for c, hs, gs in items if hs == "PASS"]
        guest_only = [c for c, hs, gs in items if hs in ("LIMIT", "GUEST") and gs == "PASS"]
        lost = [c for c, hs, gs in items if hs in ("LIMIT", "GUEST") and gs != "PASS"]
        broken = [c for c, hs, gs in items if hs not in ("PASS", "LIMIT", "GUEST")]
        parts = ["%d of %d native on this host" % (len(native), len(items))]
        if guest_only:
            parts.append("%d only in the test bed guest (functional, emulated timing): %s" % (len(guest_only), ", ".join(guest_only)))
        if lost:
            parts.append("%d not testable here: %s" % (len(lost), ", ".join(lost)))
        if broken:
            parts.append("%d failing: %s" % (len(broken), ", ".join(broken)))
        print("  %s: %s" % (area, "; ".join(parts)))
    requires = collections.OrderedDict()
    for rid, kind in status.items():
        if kind == "LIMIT":
            requires.setdefault(limits[rid]["requires"], []).append(rid)
    if requires:
        print("  the host would have to provide:")
        for text, rids in requires.items():
            print("    - %s (%d %s: %s)" % (text, len(rids), "row" if len(rids) == 1 else "rows", ", ".join(rids)))
    else:
        print("  the host provides everything the tools need")

    print("tools-test: %d checks passed, %d denied as expected, %d limited by the host, %d known upstream %s, %d failed" % (
        counts["PASS"], counts["DENIED"], counts["LIMIT"], counts["UPSTREAM"],
        "defect" if counts["UPSTREAM"] == 1 else "defects", counts["FAIL"]))
    return 1 if counts["FAIL"] else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
