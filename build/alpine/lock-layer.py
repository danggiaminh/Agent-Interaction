#!/usr/bin/env python3
"""Write a layer's lock file (guest/dev.lock, guest/tools.lock): pin every package the layer adds, by version and sha256.

  lock-layer.py --resolved FILE --aports DIR --upstream DIR --mirror URL --branch v3.24 --arch x86_64 --out dev.lock \
                --what "the development layer adds to the base image" --pkgs guest/dev.pkgs --make make-dev.sh \
                [--exceptions guest/tools.exceptions]

FILE has one "name version repo origin" line per package, as resolved by guest/layerlock.sh against the
live repositories. Every package must be built from an aport of the vendored tree at exactly that
version: where the live index has moved ahead of the vendored aports (the repository keeps changing
after the release tag), the vendored version is locked instead, if the mirror still serves it. Each
file is downloaded into --upstream/<repo>/<arch>/ and its sha256 is recorded. The mirror is not
trusted: the layer build checks the sha256 again and apk verifies the signature against the image's keys.

--exceptions names the one way out when the mirror has retired the vendored version: a line
"origin locked-version vendored-version reason..." pins every package of that aport at its live version instead. It
applies only while the vendored aport still is at the listed vendored version and the live index still
is at the listed locked version; the lock header records every exception that was used.
"""
import argparse
import hashlib
import os
import subprocess
import sys

import apkbuild


def vendored_version(aports, repo, origin):
    path = os.path.join(aports, repo, origin, "APKBUILD")
    if not os.path.isfile(path):
        sys.exit(f"lock-layer: no vendored aport {repo}/{origin} (needed by the layer)")
    return apkbuild.version(path)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    for k in ("resolved", "aports", "upstream", "mirror", "branch", "arch", "out", "what", "pkgs", "make"):
        ap.add_argument(f"--{k}", required=True)
    ap.add_argument("--exceptions")
    a = ap.parse_args()

    exceptions = {}  # origin aport -> (locked version, vendored version, reason)
    if a.exceptions and os.path.isfile(a.exceptions):
        for line in open(a.exceptions, encoding="utf-8"):
            if line.strip() and not line.startswith("#"):
                f = line.split(None, 3)
                if len(f) != 4 or f[0] in exceptions:
                    sys.exit(f"lock-layer: {a.exceptions}: need 'origin locked-version vendored-version reason' once per aport: {line.strip()}")
                exceptions[f[0]] = (f[1], f[2], f[3].strip())
    used = []

    rows = []
    for line in open(a.resolved):
        if not line.strip():
            continue
        name, ver, repo, origin = line.split()
        want = vendored_version(a.aports, repo, origin)
        if ver != want and origin in exceptions:
            locked, vendored, reason = exceptions[origin]
            if vendored != want or locked != ver:
                sys.exit(f"lock-layer: {name}: the exception for {origin} says vendored {vendored} / live {locked}, but the vendored aport is {want} and the index has {ver}: review {a.exceptions}")
            print(f"lock-layer: {name}: vendored aport {repo}/{origin} is {want}, exception: locking {ver} ({reason})", file=sys.stderr)
            used.append(f"{name} {ver} (vendored aport {origin} is {want}: {reason})")
        elif ver != want:
            print(f"lock-layer: {name}: index has {ver}, vendored aport {repo}/{origin} is {want}: locking {want}", file=sys.stderr)
            ver = want
        d = os.path.join(a.upstream, repo, a.arch)
        os.makedirs(d, exist_ok=True)
        path = os.path.join(d, f"{name}-{ver}.apk")
        if not os.path.isfile(path):
            url = f"{a.mirror}/{a.branch}/{repo}/{a.arch}/{name}-{ver}.apk"
            tmp = path + ".part"
            r = subprocess.run(["curl", "--fail", "--silent", "--show-error", "--location", "--retry", "3", "--output", tmp, url])
            if r.returncode != 0:
                if os.path.exists(tmp):
                    os.unlink(tmp)
                sys.exit(f"lock-layer: cannot download {name}-{ver}.apk (retired from the mirror?): {url}")
            os.replace(tmp, path)
        rows.append((name, ver, repo, origin, sha256(path)))

    rows.sort()
    with open(a.out, "w", encoding="utf-8") as fh:
        fh.write(
            f"# Packages {a.what} (resolved from {a.pkgs}), one per line:\n"
            "#   name version repo origin sha256-of-the-.apk\n"
            f"# Official signed binary packages of the Alpine {a.branch} repositories. Every version equals the pkgver-pkgrel of its\n"
            f"# origin aport in vendor/alpine-aports. {a.make} checks each sha256, apk verifies each signature against the\n"
            f"# Alpine release keys of the image. Refresh deliberately: {a.make} --refresh-lock, then review the diff.\n"
        )
        for u in used:
            fh.write(f"# Exception (see {os.path.basename(a.exceptions)}): {u}\n")
        for r in rows:
            fh.write(" ".join(r) + "\n")
    print(f"lock-layer: wrote {len(rows)} packages to {a.out}", file=sys.stderr)


main()
