#!/usr/bin/env python3
"""Judge the facts of the resource-control sessions against resctl-test/checks.tsv and resctl-test/limits.tsv.

    resctl-judge.py [--checks FILE] [--limits FILE] FACTS_FILE...

One facts file per session, named after it: unprivileged.facts (resctl-test/unpriv.sh, a user namespace without a cgroup
hierarchy), privileged.facts (resctl-test/priv.sh, real root of this host) and guest.facts (resctl-test/guest.sh inside the
QEMU test bed, a cgroup v2 only kernel, plus the facts of the test bed itself). Each holds KEY=VALUE lines. The privileged
session runs the suite twice and prefixes the facts: h_ (lifecycle auto) and v_ (the v1 freezer forced).

checks.tsv has one row per behaviour, tab separated:  id  area  resource  envs  fact  expectation  description
`envs` (comma separated) are the environments the row applies to; the row becomes one row "<id>.<env>" per environment and
reads the fact "<prefix><fact>" of that environment's session:
    unpriv  u_  unprivileged session        host  h_  this host, lifecycle auto       bed  (no prefix) the test bed itself
    guest   g_  cgroup v2 guest             hostv1  v_  this host, v1 freezer lifecycle
`area` is the part of the suite (build layout normal limits throttle pressure isolation terminate recover lifecycle bed) and
`resource` the domain that is controlled (memory cpu processes io lifecycle protection policy platform). Expectations are
those of tools-test/inventory.tsv: ok | =VALUE | ~REGEX | deny:REGEX (see tools-judge.py).

Every row ends as PASS, DENIED (a deny: row: the refusal that must happen), LIMIT or FAIL. A failed row becomes a LIMIT
only through limits.tsv (id class accepts cause proof requires explanation): the value matches `accepts`, every condition
of `cause` holds on the measured facts of any session, and every row of `proof` is PASS and is the same fact in the guest
(the behaviour works wherever the cause does not apply). Only rows of the areas normal, limits, throttle and pressure of the
host environments can be limited: the kill domains, the system domain, termination and recovery have no exception, and a
leaked value (an operation that worked where it had to be refused) is never a limit.

The judge also fails when a fact has no row or a row no fact, when a fact comes from the wrong session, and when an
environment does not exercise every area it must (or the host and guest every resource, or a system-domain row that must
exist). Exit status 1 on any FAIL.
"""
import argparse
import collections
import importlib.util
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
TESTS = os.path.join(HERE, "resctl-test")
CHECKS = os.path.join(TESTS, "checks.tsv")
LIMITS = os.path.join(TESTS, "limits.tsv")

_spec = importlib.util.spec_from_file_location("tools_judge", os.path.join(HERE, "tools-judge.py"))
tj = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tj)

# environment -> (session, fact prefix, title)
ENVS = collections.OrderedDict([
    ("unpriv", ("unprivileged", "u_", "unprivileged session: a user namespace, no cgroup hierarchy")),
    ("host", ("privileged", "h_", "this host as root, lifecycle auto")),
    ("hostv1", ("privileged", "v_", "this host as root, the v1 freezer as lifecycle")),
    ("guest", ("guest", "g_", "cgroup v2 test bed guest")),
    ("bed", ("guest", "", "the test bed session itself")),
])
SESSIONS = ("unprivileged", "privileged", "guest")
AREAS = ("build", "layout", "normal", "limits", "throttle", "pressure", "isolation", "terminate", "recover", "lifecycle", "bed")
RESOURCES = ("memory", "cpu", "processes", "io", "lifecycle", "protection", "policy", "platform")
FULL = ("layout", "normal", "limits", "throttle", "pressure", "isolation", "terminate", "recover", "lifecycle")
MUST_AREAS = {
    "unpriv": ("build", "layout", "normal", "isolation"),
    "host": FULL,
    "hostv1": ("layout", "normal", "isolation", "terminate", "recover", "lifecycle"),
    "guest": FULL,
    "bed": ("bed",),
}
MUST_RESOURCES = {env: ("memory", "cpu", "processes", "io", "lifecycle", "protection") for env in ("host", "guest")}
# the system domain stays outside every kill domain: these rows must exist, and pass, wherever a hierarchy exists
SYSTEM_ROWS = ("system_unreachable", "system_canary", "system_canary_terminate", "system_canary_recover",
               "system_canary_reinit", "system_canary_lifecycle", "teardown_keeps_system")
LIMIT_ENVS = ("host", "hostv1")
LIMIT_AREAS = ("normal", "limits", "throttle", "pressure")
LIMIT_CLASSES = tuple(c for c in tj.CLASSES if c != "upstream")


def main(argv):
    ap = argparse.ArgumentParser(description="judge the facts of the resource-control sessions")
    ap.add_argument("--checks", default=CHECKS)
    ap.add_argument("--limits", default=LIMITS)
    ap.add_argument("facts", nargs="+")
    args = ap.parse_args(argv)

    problems = []
    facts = {}  # name -> (value, session)
    sessions = []
    for path in args.facts:
        session = re.sub(r"\.facts$", "", os.path.basename(path))
        sessions.append(session)
        if session not in SESSIONS:
            problems.append("%s: not a session of the suite (%s)" % (os.path.basename(path), ", ".join(SESSIONS)))
        with open(path, errors="replace") as f:
            for ln in f:
                ln = ln.rstrip("\n")
                if not re.match(r"^[a-z0-9_]+=", ln):
                    continue
                key, value = ln.split("=", 1)
                if key in facts:
                    problems.append("fact %s is emitted twice (sessions %s and %s)" % (key, facts[key][1], session))
                facts[key] = (value, session)

    # ---- the rows: one per behaviour and environment
    rows = []
    by_id = {}
    seen_base = set()
    covered = {}  # fact name -> row id
    for n, cols in tj.read_tsv(args.checks, 7, problems):
        base, area, resource, envs, fact, expectation, description = cols
        where = "checks.tsv line %d (%s)" % (n, base)
        why = None
        if not re.fullmatch(r"[a-z0-9_]+", base):
            why = "the id is not [a-z0-9_]+"
        elif base in seen_base:
            why = "the id is used twice"
        elif area not in AREAS:
            why = "area '%s' is none of %s" % (area, ", ".join(AREAS))
        elif resource not in RESOURCES:
            why = "resource '%s' is none of %s" % (resource, ", ".join(RESOURCES))
        elif not re.fullmatch(r"[a-z0-9_]+", fact):
            why = "the fact is not [a-z0-9_]+"
        else:
            names = envs.split(",")
            unknown = [e for e in names if e not in ENVS]
            if unknown or len(set(names)) != len(names):
                why = "envs '%s' must be distinct names of %s" % (envs, ", ".join(ENVS))
            elif ("bed" in names) != (area == "bed") or (area == "bed" and names != ["bed"]):
                why = "the area bed belongs to the environment bed and to nothing else"
        if not why:
            why = tj.check_expectation(expectation)
        if not why and expectation.startswith("ver:"):
            why = "ver: is not used by this suite"
        if not why and tj.evaluate("", expectation, {}):
            why = "the expectation '%s' accepts an empty value, so it proves nothing" % expectation
        seen_base.add(base)
        if why:
            problems.append("%s: %s" % (where, why))
            continue
        for env in envs.split(","):
            session, prefix, _ = ENVS[env]
            row = {"id": "%s.%s" % (base, env), "base": base, "env": env, "session": session, "key": prefix + fact,
                   "area": area, "resource": resource, "fact": fact, "expectation": expectation,
                   "description": description}
            if row["key"] in covered:
                problems.append("%s: the fact %s is already covered by %s" % (where, row["key"], covered[row["key"]]))
                continue
            covered[row["key"]] = row["id"]
            by_id[row["id"]] = row
            rows.append(row)

    # ---- the limits
    limits = {}
    for n, cols in tj.read_tsv(args.limits, 7, problems):
        rid, klass, accepts, cause, proof, requires, explanation = cols
        where = "limits.tsv line %d (%s)" % (n, rid)
        if rid in limits:
            problems.append("%s: a second entry for the same row" % where)
            continue
        row = by_id.get(rid)
        if row is None:
            problems.append("%s: no such row" % where)
            continue
        if row["expectation"].startswith("deny:"):
            problems.append("%s: a deny: row states a refusal as the expectation, it cannot also be limited" % where)
            continue
        if row["env"] not in LIMIT_ENVS or row["area"] not in LIMIT_AREAS:
            problems.append("%s: only a row of the area %s of the environment %s can be limited, this is %s of %s; the kill "
                            "domains, the system domain, termination and recovery have no exception" % (
                                where, "/".join(LIMIT_AREAS), "/".join(LIMIT_ENVS), row["area"], row["env"]))
            continue
        if klass not in LIMIT_CLASSES:
            problems.append("%s: class '%s' is none of %s" % (where, klass, ", ".join(LIMIT_CLASSES)))
            continue
        why = tj.check_accepts(accepts)
        if why:
            problems.append("%s: accepts %s" % (where, why))
            continue
        if cause == "-":
            problems.append("%s: a limitation needs a measured cause" % where)
            continue
        conds, why = tj.parse_cause(cause)
        if why:
            problems.append("%s: %s" % (where, why))
            continue
        proofs = proof.split()
        bad = [p for p in proofs if p not in by_id or p == rid or by_id[p]["env"] != "guest" or by_id[p]["fact"] != row["fact"]]
        if not proofs or bad:
            problems.append("%s: the proof must name the same fact in the guest (%s), not: %s" % (
                where, row["fact"] + ".guest", ", ".join(bad) or "nothing"))
            continue
        if requires == "-":
            problems.append("%s: a host limitation must say what the host would have to provide" % where)
            continue
        limits[rid] = {"class": klass, "accepts": accepts, "cause": cause, "conds": conds, "proofs": proofs,
                       "requires": requires, "explanation": explanation}

    # ---- first pass: the expectation holds, or it does not
    status = {}  # row id -> PASS | DENIED | LIMIT | FAIL
    detail = {}  # row id -> printed text
    pass1 = {}
    pending = []
    for row in rows:
        rid = row["id"]
        if row["key"] not in facts:
            status[rid] = "FAIL"
            detail[rid] = "%s: fact %s was not reported (%s)" % (rid, row["key"], row["description"])
            continue
        value, session = facts[row["key"]]
        if session != row["session"]:
            status[rid] = "FAIL"
            detail[rid] = "%s: fact %s came from the session %s, not %s" % (rid, row["key"], session, row["session"])
            continue
        if tj.evaluate(value, row["expectation"], {}):
            kind = "DENIED" if row["expectation"].startswith("deny:") else "PASS"
            status[rid] = pass1[rid] = kind
            detail[rid] = "%s: %s" % (row["description"], tj.short(value))
        else:
            pending.append((row, value))

    # ---- second pass: a failed row is a limitation only when its entry is proven
    def holds(cond):
        name, negate, rx = cond
        if name not in facts:
            return False
        hit = re.search(rx, facts[name][0]) is not None
        return not hit if negate else hit

    def cause_text(entry):
        parts = collections.OrderedDict()
        for name, negate, rx in entry["conds"]:
            if name not in facts:
                text = "(not reported)"
            elif negate:
                text = tj.short(facts[name][0], 60)
            else:
                m = re.search(rx, facts[name][0])
                text = m.group(0).strip() if m and m.group(0).strip() else tj.short(facts[name][0], 60)
            if text not in parts.setdefault(name, []):
                parts[name].append(text)
        return ", ".join("%s=%s" % (k, " ".join(v)) for k, v in parts.items())

    for row, value in pending:
        rid = row["id"]
        wanted = "got '%s', wanted %s" % (tj.short(value), row["expectation"])
        entry = limits.get(rid)
        if entry is None:
            status[rid] = "FAIL"
            detail[rid] = "%s: %s: %s" % (rid, row["description"], wanted)
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
                    unmet += " (%s=%s)" % (cond[0], tj.short(facts[cond[0]][0], 60)) if cond[0] in facts else " (fact not reported)"
                    break
        if unmet is None:
            for p in entry["proofs"]:
                if pass1.get(p) != "PASS":
                    unmet = "the proof row %s is not PASS (%s)" % (p, status.get(p, "FAIL"))
                    break
        if unmet:
            status[rid] = "FAIL"
            detail[rid] = "%s: %s: %s; the limit entry is rejected: %s" % (rid, row["description"], wanted, unmet)
        else:
            status[rid] = "LIMIT"
            detail[rid] = "%s: %s" % (row["description"], tj.short(value))

    # ---- coverage: what an environment must exercise
    gaps = []
    for env, areas in MUST_AREAS.items():
        for area in areas:
            here = [r for r in rows if r["env"] == env and r["area"] == area]
            if not here:
                gaps.append("no row exercises the area %s in the environment %s" % (area, env))
            elif not any(status[r["id"]] == "PASS" for r in here):
                gaps.append("no row of the area %s passes in the environment %s" % (area, env))
    for env, resources in MUST_RESOURCES.items():
        for resource in resources:
            here = [r for r in rows if r["env"] == env and r["resource"] == resource]
            if not here:
                gaps.append("no row controls the resource %s in the environment %s" % (resource, env))
            elif not any(status[r["id"]] == "PASS" for r in here):
                gaps.append("no row of the resource %s passes in the environment %s" % (resource, env))
    for env in ("host", "hostv1", "guest"):
        for base in SYSTEM_ROWS:
            rid = "%s.%s" % (base, env)
            if rid not in by_id:
                gaps.append("the system-domain row %s does not exist" % rid)
            elif status[rid] != "PASS":
                gaps.append("the system-domain row %s is %s: the system domain must stay outside every kill domain" % (rid, status[rid]))
    if "system_unreachable.unpriv" not in by_id:
        gaps.append("the system-domain row system_unreachable.unpriv does not exist")
    for key in sorted(set(facts) - set(covered)):
        problems.append("fact %s has no row in checks.tsv" % key)

    # ---- report
    def tag(row):
        return "[%s]" % row["env"]

    counts = collections.Counter()
    per_env = collections.OrderedDict((e, collections.Counter()) for e in ENVS)
    print("== rows")
    for row in rows:
        kind = status[row["id"]]
        counts[kind] += 1
        per_env[row["env"]][kind] += 1
        print("%-8s%-9s%s" % (kind, tag(row), detail[row["id"]]))
    for p in problems:
        counts["FAIL"] += 1
        print("%-8s%s" % ("FAIL", "checks: %s" % p))
    for g in gaps:
        counts["FAIL"] += 1
        print("%-8s%s" % ("FAIL", "coverage: %s" % g))

    print("== environments")
    print("  %-8s %-13s %5s %7s %6s %5s  %s" % ("env", "session", "PASS", "DENIED", "LIMIT", "FAIL", ""))
    for env, (session, prefix, title) in ENVS.items():
        c = per_env[env]
        note = title if session in sessions else "NOT RUN (%s)" % title
        print("  %-8s %-13s %5d %7d %6d %5d  %s" % (env, session, c["PASS"], c["DENIED"], c["LIMIT"], c["FAIL"], note))
    if "guest_accel" in facts:
        print("  guest accelerator: %s%s" % (facts["guest_accel"][0],
                                             " (functional, but its timing is not accurate)" if facts["guest_accel"][0] == "tcg" else ""))

    def matrix(title, field, names):
        print("== %s (rows passing / rows, per environment; * = includes a row limited by the host)" % title)
        envs = [e for e in ENVS if e != "bed"]
        print("  %-11s" % "" + "".join(" %-8s" % e for e in envs))
        for name in names:
            cells = []
            for env in envs:
                here = [r for r in rows if r["env"] == env and r[field] == name]
                if not here:
                    cells.append("-")
                    continue
                ok = sum(1 for r in here if status[r["id"]] in ("PASS", "DENIED"))
                cells.append("%d/%d%s" % (ok, len(here), "*" if any(status[r["id"]] == "LIMIT" for r in here) else ""))
            print("  %-11s" % name + "".join(" %-8s" % c for c in cells))

    matrix("areas", "area", [a for a in AREAS if a != "bed"])
    matrix("resources", "resource", RESOURCES)

    print("== limits of the host (recorded in limits.tsv, each with its measured cause and the guest row that proves the tool works elsewhere)")
    limited = [rid for rid, kind in status.items() if kind == "LIMIT"]
    if not limited:
        print("  none")
    for klass in LIMIT_CLASSES:
        rids = [r for r in limited if limits[r]["class"] == klass]
        if not rids:
            continue
        print("  %s: %s" % (klass, tj.CLASS_TITLES[klass]))
        for rid in rids:
            entry = limits[rid]
            print("    %s: %s" % (rid, entry["explanation"]))
            print("      measured: %s" % cause_text(entry))
            print("      proof: %s" % ", ".join("%s=%s" % (p, status.get(p)) for p in entry["proofs"]))

    print("== verdict")
    for resource in RESOURCES:
        here = [r for r in rows if r["env"] == "host" and r["resource"] == resource]
        if not here:
            continue
        native = [r for r in here if status[r["id"]] in ("PASS", "DENIED")]
        lim = [r for r in here if status[r["id"]] == "LIMIT"]
        broken = [r for r in here if status[r["id"]] == "FAIL"]
        parts = ["%d of %d native on this host" % (len(native), len(here))]
        if lim:
            parts.append("%d only in the test bed guest (functional, emulated timing): %s" % (len(lim), ", ".join(r["base"] for r in lim)))
        if broken:
            parts.append("%d failing: %s" % (len(broken), ", ".join(r["base"] for r in broken)))
        print("  %s: %s" % (resource, "; ".join(parts)))
    requires = collections.OrderedDict()
    for rid in limited:
        requires.setdefault(limits[rid]["requires"], []).append(rid)
    if requires:
        print("  the host would have to provide:")
        for text, rids in requires.items():
            print("    - %s (%d %s: %s)" % (text, len(rids), "row" if len(rids) == 1 else "rows", ", ".join(rids)))
    else:
        print("  the host provides everything the resource control needs")

    print("resctl-test: %d checks passed, %d denied as expected, %d limited by the host, %d failed" % (
        counts["PASS"], counts["DENIED"], counts["LIMIT"], counts["FAIL"]))
    return 1 if counts["FAIL"] else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
