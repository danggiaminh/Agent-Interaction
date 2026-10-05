#!/usr/bin/env python3
"""Static checks of a staged Alpine base image (rootfs directory). Read-only; prints PASS/FAIL lines.

  check-image.py --root DIR --lock base.lock --origins base.origins --aports DIR --arch x86_64 \
                 --manifest FILE [--needles FILE]

Checks: package set == pinned lock; versions == vendored APKBUILDs; dependency closure (everything
satisfied, nothing extra); every path owned by a package or an explicit image-config path; ELF
architecture; uid/gid consistency; no dangling links; no host state (secrets, host names, paths).
"""
import argparse
import os
import re
import stat
import subprocess
import sys

fails = 0


def ok(msg):
    print(f"PASS  {msg}")


def bad(msg):
    global fails
    fails += 1
    print(f"FAIL  {msg}")


def check(cond, msg, detail=""):
    if cond:
        ok(msg)
    else:
        bad(msg + (f": {detail}" if detail else ""))


def parse_db(path):
    """Parse an apk 'installed' database into a list of package dicts."""
    pkgs, cur, cwd = [], None, None
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh.read().split("\n"):
            if not line:
                if cur:
                    pkgs.append(cur)
                cur, cwd = None, None
                continue
            k, _, v = line.partition(":")
            if cur is None:
                cur = {"files": [], "dirs": []}
            if k == "F":
                cwd = v
                cur["dirs"].append(v)
            elif k == "R":
                cur["files"].append((cwd + "/" + v) if cwd else v)
            elif k in ("D", "p", "i"):
                cur.setdefault(k, []).extend(v.split())
            elif k in ("P", "V", "A", "o"):
                cur[k] = v
    if cur:
        pkgs.append(cur)
    return pkgs


def provides_of(name, byname):
    p = byname[name]
    return {name} | {dep_name(x) for x in p.get("p", [])}


def dep_name(tok):
    return re.split(r"[<>=~]", tok.lstrip("!"), maxsplit=1)[0]


def resolve_in_root(root, rel, depth=0):
    """Resolve a path as if chrooted in root; returns the absolute host path (may not exist)."""
    if depth > 40:
        return None
    parts = [p for p in rel.split("/") if p]
    cur = root
    stack = []
    for i, part in enumerate(parts):
        if part == "..":
            if stack:
                stack.pop()
            cur = os.path.join(root, *stack) if stack else root
            continue
        nxt = os.path.join(cur, part)
        if os.path.islink(nxt):
            tgt = os.readlink(nxt)
            if tgt.startswith("/"):
                new = tgt
            else:
                new = "/" + "/".join(stack + [tgt])
            rest = "/".join(parts[i + 1:])
            return resolve_in_root(root, new + ("/" + rest if rest else ""), depth + 1)
        stack.append(part)
        cur = nxt
    return cur


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--lock", required=True)
    ap.add_argument("--origins", required=True)
    ap.add_argument("--aports", required=True)
    ap.add_argument("--arch", required=True)
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--needles", help="file with one host-state string per line to search for")
    a = ap.parse_args()
    root = os.path.abspath(a.root)

    pkgs = parse_db(os.path.join(root, "lib/apk/db/installed"))
    byname = {p["P"]: p for p in pkgs}
    print(f"== package set ({len(pkgs)} installed)")

    # 1. package set == pinned lock
    installed = sorted(f"{p['P']}={p['V']}" for p in pkgs)
    locked = sorted(l.strip() for l in open(a.lock) if l.strip() and not l.startswith("#"))
    check(installed == locked, "installed package set matches base.lock",
          f"only installed: {sorted(set(installed) - set(locked))}; only locked: {sorted(set(locked) - set(installed))}")

    # 2. every package was built from the vendored aports (origin listed, version == APKBUILD)
    origins = [l.strip().split("/", 1) for l in open(a.origins) if l.strip() and not l.startswith("#")]
    originset = {o for _, o in origins}
    stray = sorted(p["P"] for p in pkgs if p.get("o") not in originset)
    check(not stray, "every package originates from an aport in base.origins", str(stray))
    mism = []
    for o_repo, o in origins:
        t = open(os.path.join(a.aports, o_repo, o, "APKBUILD"), encoding="utf-8").read()
        ver = re.search(r"^pkgver=(\S+)", t, re.M).group(1)
        rel = re.search(r"^pkgrel=(\S+)", t, re.M).group(1)
        for p in pkgs:
            if p.get("o") == o and p["V"] != f"{ver}-r{rel}":
                mism.append(f"{p['P']} {p['V']} != {ver}-r{rel}")
    check(not mism, "installed versions equal the vendored APKBUILD pkgver-pkgrel", "; ".join(mism))

    # 3. manifest agrees with the installed db (name, version, origin)
    mf = [l.split() for l in open(a.manifest) if l.strip() and not re.match(r"^[a-z0-9_]+=|^#", l)]
    check(sorted(f"{x[0]}={x[1]}" for x in mf) == installed, "manifest lists exactly the installed packages")
    check(all(len(x) == 5 and re.fullmatch(r"[0-9a-f]{64}", x[3]) and re.fullmatch(r"[0-9a-f]{64}", x[4]) for x in mf),
          "manifest has apk sha256 and datahash for every package")

    # 4. architecture of packages
    badarch = sorted(p["P"] for p in pkgs if p.get("A") not in (a.arch, "noarch"))
    check(not badarch, f"every package is {a.arch} or noarch", str(badarch))

    # 5. dependency closure: satisfied, and nothing beyond what alpine-base needs
    print("== dependency closure")
    provides = {}
    for p in pkgs:
        provides.setdefault(p["P"], set()).add(p["P"])
        for tok in p.get("p", []):
            provides.setdefault(dep_name(tok), set()).add(p["P"])
    unsat = []
    for p in pkgs:
        for tok in p.get("D", []):
            if tok.startswith("!"):
                continue
            if dep_name(tok) not in provides:
                unsat.append(f"{p['P']} -> {tok}")
    check(not unsat, "every dependency is satisfied inside the image", "; ".join(unsat[:5]))
    seen, todo = set(), ["alpine-base"]
    while True:
        while todo:
            n = todo.pop()
            if n in seen or n not in byname:
                continue
            seen.add(n)
            for tok in byname[n].get("D", []):
                if tok.startswith("!"):
                    continue
                nm = dep_name(tok)
                todo.extend([nm] if nm in byname else sorted(provides.get(nm, ())))
        # apk also installs a package whose install_if conditions are all met (ssl_client: busybox + libssl3).
        have = {q for s in seen for q in provides_of(s, byname)}
        more = [p["P"] for p in pkgs if p["P"] not in seen and p.get("i") and all(dep_name(c) in have for c in p["i"])]
        if not more:
            break
        todo.extend(more)
    extra = sorted(set(byname) - seen)
    check("alpine-base" in byname and not extra,
          "image is exactly the closure of alpine-base, dependencies plus install_if (no extra packages)", str(extra))

    # 6. ownership: every path is package-owned or an explicit image-configuration path
    print("== filesystem")
    owned = set()
    for p in pkgs:
        for d in p["dirs"]:
            owned.add(d)
        for f in p["files"]:
            owned.add(f)
    allowed = (
        "etc/apk/repositories", "etc/apk/world", "etc/apk/arch", "etc/apk/protected_paths.d",
        "lib/apk/db/", "lib/apk/exec", "etc/runlevels/",
        # copies made by busybox adduser/addgroup in busybox's post-install (klogd user and group)
        "etc/passwd-", "etc/group-", "etc/shadow-",
    )
    unowned = []
    for dp, dns, fns in os.walk(root):
        rel_dir = os.path.relpath(dp, root)
        for n in dns + fns:
            rel = n if rel_dir == "." else f"{rel_dir}/{n}"
            if rel in owned or any(rel == x.rstrip("/") or rel.startswith(x) for x in allowed):
                continue
            unowned.append(rel)
    # Symlinks that busybox's trigger creates for its applets are not in the apk database.
    applet = []
    for rel in list(unowned):
        full = os.path.join(root, rel)
        if os.path.islink(full) and os.readlink(full) in ("/bin/busybox", "/bin/bbsuid", "busybox", "/bin/bbsuid", "../bin/busybox", "../../bin/busybox", "../../bin/bbsuid", "../bin/bbsuid"):
            applet.append(rel)
            unowned.remove(rel)
    check(len(applet) > 0, f"busybox applet links created by its trigger ({len(applet)})")
    check(not unowned, "no file outside package ownership and the documented image configuration",
          f"{len(unowned)}: {unowned[:12]}")

    # 7. ELF architecture
    elfs, wrong = 0, []
    for dp, dns, fns in os.walk(root):
        for n in fns:
            full = os.path.join(dp, n)
            if os.path.islink(full) or not os.path.isfile(full):
                continue
            with open(full, "rb") as fh:
                h = fh.read(20)
            if h[:4] == b"\x7fELF":
                elfs += 1
                if not (h[4] == 2 and h[5] == 1 and int.from_bytes(h[18:20], "little") == 0x3E):
                    wrong.append(os.path.relpath(full, root))
    check(elfs > 0 and not wrong, f"all {elfs} ELF objects are 64-bit little-endian x86-64", str(wrong[:5]))

    # 8. ownership ids and link sanity
    pw = {int(l.split(":")[2]) for l in open(os.path.join(root, "etc/passwd")) if l.count(":") >= 6}
    gr = {int(l.split(":")[2]) for l in open(os.path.join(root, "etc/group")) if l.count(":") >= 3}
    bad_ids, dangling, setuid = [], [], []
    for dp, dns, fns in os.walk(root):
        for n in dns + fns:
            full = os.path.join(dp, n)
            st = os.lstat(full)
            rel = os.path.relpath(full, root)
            if st.st_uid not in pw or st.st_gid not in gr:
                bad_ids.append(f"{rel} {st.st_uid}:{st.st_gid}")
            if stat.S_ISLNK(st.st_mode):
                tgt = resolve_in_root(root, os.path.join("/", os.path.dirname(rel), os.readlink(full))
                                      if not os.readlink(full).startswith("/") else os.readlink(full))
                lt = os.readlink(full)
                norm = os.path.normpath(lt if lt.startswith("/") else os.path.join("/", os.path.dirname(rel), lt))
                runtime = norm.startswith(("/proc/", "/dev/", "/sys/", "/run/"))
                if (tgt is None or not os.path.lexists(tgt)) and not runtime:
                    dangling.append(f"{rel} -> {os.readlink(full)}")
            elif stat.S_ISREG(st.st_mode) and st.st_mode & 0o6000:
                setuid.append(f"{rel} {oct(st.st_mode & 0o7777)}")
    check(not bad_ids, "every uid/gid in the image exists in its own passwd/group", str(bad_ids[:5]))
    check(not dangling, "no dangling symlinks (runtime /proc,/dev,/sys,/run targets excepted)", str(dangling[:5]))
    check(setuid == ["bin/bbsuid 0o4111"] or all(s.startswith("bin/bbsuid") for s in setuid),
          "only busybox-suid carries setuid/setgid bits", str(setuid))

    # 9. no host state
    print("== host state")
    unwanted = [p for p in ("etc/resolv.conf", "etc/machine-id", "root/.ash_history", "root/.bash_history", "var/cache/apk", "var/log/apk.log")
                if os.path.lexists(os.path.join(root, p)) and (not os.path.isdir(os.path.join(root, p)) or os.listdir(os.path.join(root, p)))]
    check(not unwanted, "no resolver config, machine-id, shell history, package cache or apk transcript", str(unwanted))
    for d in ("root", "home", "tmp", "dev", "proc", "sys"):
        p = os.path.join(root, d)
        if os.path.isdir(p) and os.listdir(p):
            bad(f"/{d} is not empty: {os.listdir(p)[:5]}")
    keys = sorted(os.listdir(os.path.join(root, "etc/apk/keys")))
    check(keys and not any("agent-interaction" in k for k in keys) and all(f"etc/apk/keys/{k}" in owned for k in keys),
          f"/etc/apk/keys holds only Alpine release keys from alpine-keys ({len(keys)})", str(keys))
    if a.needles:
        needles = []
        for line in open(a.needles, encoding="utf-8"):
            label, _, value = line.rstrip("\n").partition("\t")
            if value:
                needles.append((label, value.encode()))
        hits = []
        for dp, dns, fns in os.walk(root):
            for n in fns:
                full = os.path.join(dp, n)
                if os.path.islink(full) or not os.path.isfile(full):
                    continue
                with open(full, "rb") as fh:
                    data = fh.read()
                for label, value in needles:
                    if value in data:
                        hits.append(f"{os.path.relpath(full, root)} [{label}]")  # label only, never the value
        check(not hits, f"none of {len(needles)} host-state strings (host paths, proxy, secrets, today's date) appears in any file",
              "; ".join(hits[:8]))

    print(f"RESULT: image static checks {'OK' if not fails else 'FAILED'}")
    sys.exit(1 if fails else 0)


main()
