#!/usr/bin/env python3
"""Static checks of a staged Alpine image (rootfs directory). Read-only; prints PASS/FAIL lines.

  check-image.py --root DIR --lock base.lock --origins base.origins --aports DIR --arch x86_64 \
                 --manifest FILE [--needles FILE] [--layer "label,lock,pkgs[,exceptions]"]... [--setid PATH:MODE]...
                 [--non-x86 PATH]... [--dangling PATH]... [--needle-ok PATH:VALUE]...

Checks: package set == pinned lock; versions == vendored APKBUILDs; dependency closure (everything
satisfied, nothing extra); every path owned by a package or an explicit image-config path; ELF
architecture; uid/gid consistency; no dangling links; no host state (secrets, host names, paths).

Each --layer adds a layer on top of the base system, in order (development layer, then tools layer): the
package set is base.lock + every layer's lock, the closure is rooted at alpine-base and the packages of
every layer's .pkgs, and each layer's versions are compared with the vendored APKBUILDs of their origins.
A layer's exceptions file ("origin locked-version vendored-version reason...") allows one origin aport to
be locked at a version other than the vendored one, only while the vendored aport is still exactly at the
listed vendored version and the lock is exactly at the listed locked version.
--setid PATH:MODE names a file other than busybox-suid that may carry setuid/setgid bits, with that exact mode.

Documented exemptions, each for a package-owned path and each checked for staleness: the set of paths that
trip a check must equal the exemption set exactly, so an exemption that stops applying fails the check.
--non-x86 PATH      an ELF file that is not x86-64 by design (firmware the package ships for another CPU)
--dangling PATH     a symlink whose target the package itself does not provide (an upstream defect)
--needle-ok PATH:VALUE   a host-path needle that matches a package file only as a documentation example
                    (never applies to the other needle labels: wall-clock, secrets, host name)
Entries of /etc/ssl/certs that the ca-certificates trigger creates (ca-cert-NAME.pem links and OpenSSL hash
links) are not package-owned; they are verified against /etc/ca-certificates.conf and then accepted.
"""
import argparse
import mmap
import os
import re
import stat
import subprocess
import sys

import apkbuild

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
    ap.add_argument("--layer", action="append", default=[], metavar="LABEL,LOCK,PKGS[,EXCEPTIONS]",
                    help="a layer on top of the base system: label, its .lock, its .pkgs and optionally its .exceptions")
    ap.add_argument("--setid", action="append", default=[], metavar="PATH:MODE",
                    help="file allowed to carry setuid/setgid bits, with its exact mode (octal)")
    ap.add_argument("--non-x86", action="append", default=[], metavar="PATH",
                    help="package-owned ELF file that is not x86-64 by design")
    ap.add_argument("--dangling", action="append", default=[], metavar="PATH",
                    help="package-owned symlink that dangles in the upstream package")
    ap.add_argument("--needle-ok", action="append", default=[], metavar="PATH:VALUE",
                    help="package file in which a host-path needle is only a documentation example")
    a = ap.parse_args()
    root = os.path.abspath(a.root)
    allowed_setid = {"bin/bbsuid": 0o4111}
    for x in a.setid:
        path, _, mode = x.partition(":")
        allowed_setid[path] = int(mode, 8)

    layers = []  # dicts: label, lock rows, pkgs, exceptions, names of the files
    for spec in a.layer:
        f = spec.split(",")
        if len(f) not in (3, 4):
            ap.error(f"--layer needs label,lock,pkgs[,exceptions]: {spec}")
        exc = {}
        if len(f) == 4 and os.path.isfile(f[3]):
            for line in open(f[3], encoding="utf-8"):
                if line.strip() and not line.startswith("#"):
                    x = line.split(None, 3)
                    if len(x) != 4 or x[0] in exc:
                        ap.error(f"{f[3]}: need 'origin locked-version vendored-version reason' once per aport: {line.strip()}")
                    exc[x[0]] = (x[1], x[2])
        layers.append({
            "label": f[0], "lockname": os.path.basename(f[1]), "pkgsname": os.path.basename(f[2]),
            "rows": [l.split() for l in open(f[1]) if l.strip() and not l.startswith("#")],
            "pkgs": [l.strip() for l in open(f[2]) if l.strip() and not l.startswith("#")],
            "exc": exc, "excname": os.path.basename(f[3]) if len(f) == 4 else "",
        })

    pkgs = parse_db(os.path.join(root, "lib/apk/db/installed"))
    byname = {p["P"]: p for p in pkgs}
    print(f"== package set ({len(pkgs)} installed)")

    # 1. package set == pinned lock
    installed = sorted(f"{p['P']}={p['V']}" for p in pkgs)
    locked = sorted(l.strip() for l in open(a.lock) if l.strip() and not l.startswith("#"))
    dev = []  # (name, version, repo, origin, sha256) of every layer's packages
    for ly in layers:
        dev += ly["rows"]
        check(all(len(d) == 5 for d in ly["rows"]) and len({d[0] for d in ly["rows"]}) == len(ly["rows"]),
              f"{ly['lockname']} has five fields per line and no duplicate package")
        locked = sorted(set(locked) | {f"{d[0]}={d[1]}" for d in ly["rows"]})
    check(len({d[0] for d in dev}) == len(dev), "no package is locked by two layers")
    what = " + ".join(["base.lock"] + [ly["lockname"] for ly in layers])
    check(installed == locked, f"installed package set matches {what}",
          f"only installed: {sorted(set(installed) - set(locked))}; only locked: {sorted(set(locked) - set(installed))}")

    # 2. every package was built from the vendored aports (origin listed, version == APKBUILD)
    origins = [l.strip().split("/", 1) for l in open(a.origins) if l.strip() and not l.startswith("#")]
    originset = {o for _, o in origins}
    devby = {d[0]: d for d in dev}
    stray = sorted(p["P"] for p in pkgs if p.get("o") not in originset and p["P"] not in devby)
    check(not stray, "every package originates from an aport in base.origins" + (" or is locked in " + " or ".join(ly["lockname"] for ly in layers) if layers else ""), str(stray))
    for ly in layers:
        # The layer's packages are official binaries; each one must be exactly what the vendored aport describes,
        # or be covered by an exception that is still current.
        wrong, excused = [], 0
        for n, ver, repo, origin, _ in ly["rows"]:
            p = byname.get(n)
            if p is None or p.get("o") != origin:
                wrong.append(f"{n}: origin {p.get('o') if p else None} != {origin}")
                continue
            want = apkbuild.version(os.path.join(a.aports, repo, origin, "APKBUILD"))
            if ver == want:
                continue
            if origin in ly["exc"] and ly["exc"][origin] == (ver, want):
                excused += 1
            else:
                wrong.append(f"{n} {ver} != vendored {origin} {want}")
        stale = sorted(o for o in ly["exc"] if not any(d[3] == o for d in ly["rows"]))
        if stale:
            wrong.append(f"{ly['excname']} lists {stale}, which the layer does not contain")
        check(not wrong, f"{ly['label']}: {len(ly['rows'])} packages, origins as locked, versions equal the vendored APKBUILD pkgver-pkgrel"
              + (f" ({excused} excused by {ly['excname']}, which still matches the vendored aports)" if excused else ""), "; ".join(wrong))
    if layers:
        unused = [f"{ly['excname']}: {o}" for ly in layers for o in ly["exc"] if not any(d[3] == o and d[1] == ly["exc"][o][0] for d in ly["rows"])]
        check(not unused, "every exception still applies to a locked package", "; ".join(unused))
    mism = []
    for o_repo, o in origins:
        want = apkbuild.version(os.path.join(a.aports, o_repo, o, "APKBUILD"))
        for p in pkgs:
            if p.get("o") == o and p["V"] != want:
                mism.append(f"{p['P']} {p['V']} != {want}")
    check(not mism, "installed versions equal the vendored APKBUILD pkgver-pkgrel", "; ".join(mism))

    # 3. manifest agrees with the installed db (name, version, origin)
    mf = [l.split() for l in open(a.manifest) if l.strip() and not re.match(r"^[a-z0-9_]+=|^#", l)]
    check(sorted(f"{x[0]}={x[1]}" for x in mf) == installed, "manifest lists exactly the installed packages")
    if dev:
        mism = [x[0] for x in mf if x[0] in devby and (x[3] != devby[x[0]][4] or x[2] != devby[x[0]][3])]
        check(not mism, "manifest records the locked origin and .apk sha256 of every package the " + " and ".join(ly["label"] for ly in layers) + " add", str(mism))
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
    roots = ["alpine-base"]
    for ly in layers:
        roots += ly["pkgs"]
    seen, todo = set(), list(roots)
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
    check(all(r in byname for r in roots) and not extra,
          f"image is exactly the closure of {' + '.join(['alpine-base'] + [ly['pkgsname'] for ly in layers])}, dependencies plus install_if (no extra packages)", str(extra))
    if layers:
        world = sorted(l.strip() for l in open(os.path.join(root, "etc/apk/world")) if l.strip())
        check(world == sorted(roots), f"/etc/apk/world records exactly the requested packages ({' + '.join(['alpine-base'] + [ly['pkgsname'] for ly in layers])})",
              f"only in world: {sorted(set(world) - set(roots))}; only requested: {sorted(set(roots) - set(world))}")

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
    # ca-certificates' trigger (update-ca-certificates) links every enabled certificate of
    # /etc/ca-certificates.conf as ca-cert-NAME.pem and adds an OpenSSL hash link (HASH.N) for each one.
    certdir = "etc/ssl/certs/"
    if any(r.startswith(certdir) for r in unowned):
        enabled = []
        conf = os.path.join(root, "etc/ca-certificates.conf")
        if os.path.isfile(conf):
            for line in open(conf, encoding="utf-8"):
                line = line.strip()
                if line and not line.startswith(("#", "!")) and line.endswith(".crt"):
                    enabled.append(line)
        want = {f"{certdir}ca-cert-{os.path.basename(c)[:-4]}.pem": "/usr/share/ca-certificates/" + c for c in enabled}
        pems, hashes, trouble = set(), {}, []
        for rel in sorted(r for r in unowned if r.startswith(certdir)):
            full, name = os.path.join(root, rel), rel[len(certdir):]
            tgt = os.readlink(full) if os.path.islink(full) else None
            if rel in want:
                if tgt == want[rel] and want[rel].lstrip("/") in owned:
                    pems.add(rel)
                else:
                    trouble.append(f"{name} -> {tgt}")
            elif re.fullmatch(r"[0-9a-f]{8}\.[0-9]+", name):
                if tgt is not None and certdir + tgt in want:
                    hashes[rel] = certdir + tgt
                else:
                    trouble.append(f"{name} -> {tgt}")
        trouble += [f"{os.path.basename(w)} is missing" for w in sorted(set(want) - pems)]
        trouble += [f"{os.path.basename(w)} has no hash link" for w in sorted(pems - set(hashes.values()))]
        check(pems and hashes and not trouble,
              f"/etc/ssl/certs: {len(pems)} certificate links and {len(hashes)} hash links from the ca-certificates trigger are exactly "
              "what /etc/ca-certificates.conf enables, each pointing at a packaged certificate", "; ".join(trouble[:5]))
        unowned = [r for r in unowned if r not in want and r not in hashes]
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
    non_x86 = set(a.non_x86)
    check(elfs > 0 and set(wrong) <= non_x86,
          f"all {elfs} ELF objects are 64-bit little-endian x86-64" + (f" (except {sorted(non_x86)}: firmware that the package ships for another CPU)" if non_x86 else ""),
          str(sorted(set(wrong) - non_x86)[:5]))
    if non_x86:
        check(non_x86 <= set(wrong) and all(x in owned for x in non_x86),
              "every non-x86 exemption is a package-owned ELF file that is still not x86-64",
              str(sorted(non_x86 - set(wrong)) + sorted(x for x in non_x86 if x not in owned)))

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
                setuid.append((rel, st.st_mode & 0o7777))
    check(not bad_ids, "every uid/gid in the image exists in its own passwd/group", str(bad_ids[:5]))
    dang_ok = set(a.dangling)
    check(all(d.split(" -> ")[0] in dang_ok for d in dangling),
          "no dangling symlinks (runtime /proc,/dev,/sys,/run targets excepted)" + (f" other than the {len(dang_ok)} that the packages themselves ship dangling ({', '.join(sorted(dang_ok))})" if dang_ok else ""),
          str([d for d in dangling if d.split(" -> ")[0] not in dang_ok][:5]))
    if dang_ok:
        check(dang_ok == {d.split(" -> ")[0] for d in dangling} and all(x in owned for x in dang_ok),
              "every dangling-link exemption is a package-owned symlink that still dangles",
              str(sorted(dang_ok - {d.split(" -> ")[0] for d in dangling}) + sorted(x for x in dang_ok if x not in owned)))
    unexpected = [f"{r} {oct(m)}" for r, m in setuid if allowed_setid.get(r) != m]
    names = ", ".join(sorted(r for r, _ in setuid))
    check(not unexpected, f"setuid/setgid bits only on the expected files with the expected modes ({names})",
          f"unexpected: {unexpected}")

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
        hits, excused = [], set()
        needle_ok = set()
        for x in a.needle_ok:
            path, _, value = x.partition(":")
            needle_ok.add((path, value))
        for dp, dns, fns in os.walk(root):
            for n in fns:
                full = os.path.join(dp, n)
                if os.path.islink(full) or not os.path.isfile(full):
                    continue
                if os.path.getsize(full) == 0:
                    continue
                with open(full, "rb") as fh, mmap.mmap(fh.fileno(), 0, access=mmap.ACCESS_READ) as data:
                    for label, value in needles:
                        if data.find(value) != -1:
                            rel = os.path.relpath(full, root)
                            if label == "host-path" and (rel, value.decode()) in needle_ok and rel in owned:
                                excused.add((rel, value.decode()))
                            else:
                                hits.append(f"{rel} [{label}]")  # label only, never the value
        check(not hits, f"none of {len(needles)} host-state strings (host paths, proxy, secrets, today's date) appears in any file",
              "; ".join(hits[:8]))
        if needle_ok:
            check(excused == needle_ok,
                  f"the {len(needle_ok)} host-path exemptions are package files whose only match is a documentation example, and each still matches",
                  str(sorted(p for p, _ in needle_ok - excused)))

    print(f"RESULT: image static checks {'OK' if not fails else 'FAILED'}")
    sys.exit(1 if fails else 0)


main()
