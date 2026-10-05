#!/usr/bin/env python3
"""Judge the facts printed by tools-test/unpriv.sh and tools-test/priv.sh against tools-test/inventory.tsv.

    tools-judge.py FACTS_FILE...

Each facts file holds KEY=VALUE lines. inventory.tsv has one row per fact, tab separated:
    id  packages  fact  expectation  description
`packages` are the packages the row exercises (comma separated; each must be a package of guest/tools.pkgs or
guest/dev.pkgs; `-` for a fact about the session or the host kernel that no package provides). Expectations:
    ok              the value is "ok"
    =VALUE          the value equals VALUE
    ver:PKG         the value contains the locked version of PKG (without the -rN release)
    ~REGEX          the value matches REGEX (re.search)
any of them may be followed by `|limit:REGEX`: when the expectation fails but the value matches the limit
regex, the row is a LIMIT (the cloud host lacks a capability the tool needs) instead of a FAIL.

Beyond the rows, the inventory must cover every package of guest/tools.pkgs, every emitted fact must have a row
and every row must have an emitted fact. Prints PASS, LIMIT and FAIL lines and a summary; exit status 1 on FAIL.
"""
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
INVENTORY = os.path.join(HERE, "tools-test", "inventory.tsv")


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


def short(value, n=110):
    value = " ".join(value.split())
    return value if len(value) <= n else value[: n - 3] + "..."


def evaluate(value, expectation, versions):
    """True, False or (None, reason) when the expectation itself is unusable."""
    if expectation == "ok":
        return value == "ok"
    if expectation.startswith("="):
        return value == expectation[1:]
    if expectation.startswith("ver:"):
        pkg = expectation[4:]
        if pkg not in versions:
            return None, "no locked version for %s" % pkg
        return versions[pkg] in value
    if expectation.startswith("~"):
        return re.search(expectation[1:], value) is not None
    return None, "unknown expectation syntax"


def main(argv):
    facts = {}
    problems = []
    for path in argv:
        with open(path, errors="replace") as f:
            for ln in f:
                ln = ln.rstrip("\n")
                if "=" not in ln or not re.match(r"^[a-z0-9_]+=", ln):
                    continue
                key, value = ln.split("=", 1)
                if key in facts:
                    problems.append("fact %s is emitted twice" % key)
                facts[key] = value

    versions = {}
    versions.update(read_lock(os.path.join(HERE, "guest", "dev.lock")))
    versions.update(read_lock(os.path.join(HERE, "guest", "tools.lock")))
    tools_pkgs = set(read_list(os.path.join(HERE, "guest", "tools.pkgs")))
    known_pkgs = tools_pkgs | set(read_list(os.path.join(HERE, "guest", "dev.pkgs")))

    rows = []
    with open(INVENTORY) as f:
        for n, ln in enumerate(f, 1):
            ln = ln.rstrip("\n")
            if not ln.strip() or ln.startswith("#"):
                continue
            cols = ln.split("\t")
            if len(cols) != 5:
                problems.append("inventory line %d has %d columns, wanted 5" % (n, len(cols)))
                continue
            rows.append(cols)

    seen_ids, row_facts, covered = set(), set(), set()
    npass = nlimit = nfail = 0
    limits = []

    def out(kind, text):
        print("%-5s %s" % (kind, text))

    for rid, pkgs, fact, expectation, description in rows:
        if rid in seen_ids:
            problems.append("row id %s is used twice" % rid)
        seen_ids.add(rid)
        row_facts.add(fact)
        for p in pkgs.split(","):
            p = p.strip()
            if p == "-":
                continue
            if p not in known_pkgs:
                problems.append("row %s names %s, which is in neither tools.pkgs nor dev.pkgs" % (rid, p))
            covered.add(p)
        if fact not in facts:
            nfail += 1
            out("FAIL", "%s: fact %s was not reported (%s)" % (rid, fact, description))
            continue
        value = facts[fact]
        want, _, limit = expectation.partition("|limit:")
        res = evaluate(value, want, versions)
        if isinstance(res, tuple):
            problems.append("row %s: %s" % (rid, res[1]))
            continue
        if res:
            npass += 1
            out("PASS", "%s: %s" % (description, short(value)))
        elif limit and re.search(limit, value):
            nlimit += 1
            limits.append((rid, description, value))
            out("LIMIT", "%s: %s" % (description, short(value)))
        else:
            nfail += 1
            out("FAIL", "%s: %s: got '%s', wanted %s" % (rid, description, short(value), expectation))

    for fact in sorted(set(facts) - row_facts):
        problems.append("fact %s has no inventory row" % fact)
    for p in sorted(tools_pkgs - covered):
        problems.append("package %s of tools.pkgs is exercised by no inventory row" % p)
    for p in problems:
        nfail += 1
        out("FAIL", "inventory: %s" % p)

    print("tools-test: %d checks passed, %d limited by the host, %d failed" % (npass, nlimit, nfail))
    return 1 if nfail else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
