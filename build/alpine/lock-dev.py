#!/usr/bin/env python3
"""Write guest/dev.lock: pin every package the development layer adds, by version and sha256.

  lock-dev.py --resolved FILE --aports DIR --upstream DIR --mirror URL --branch v3.24 --arch x86_64 --out dev.lock

FILE has one "name version repo origin" line per package, as resolved by guest/devlock.sh against the
live repositories. Every package must be built from an aport of the vendored tree at exactly that
version: where the live index has moved ahead of the vendored aports (the repository keeps changing
after the release tag), the vendored version is locked instead, if the mirror still serves it. Each
file is downloaded into --upstream/<repo>/<arch>/ and its sha256 is recorded. The mirror is not
trusted: make-dev.sh checks the sha256 again and apk verifies the signature against the image's keys.
"""
import argparse
import hashlib
import os
import re
import subprocess
import sys


def vendored_version(aports, repo, origin):
    path = os.path.join(aports, repo, origin, "APKBUILD")
    if not os.path.isfile(path):
        sys.exit(f"lock-dev: no vendored aport {repo}/{origin} (needed by the development layer)")
    t = open(path, encoding="utf-8").read()
    ver = re.search(r"^pkgver=(\S+)", t, re.M).group(1).strip("\"'")
    rel = re.search(r"^pkgrel=(\S+)", t, re.M).group(1).strip("\"'")
    return f"{ver}-r{rel}"


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    for k in ("resolved", "aports", "upstream", "mirror", "branch", "arch", "out"):
        ap.add_argument(f"--{k}", required=True)
    a = ap.parse_args()

    rows = []
    for line in open(a.resolved):
        if not line.strip():
            continue
        name, ver, repo, origin = line.split()
        want = vendored_version(a.aports, repo, origin)
        if ver != want:
            print(f"lock-dev: {name}: index has {ver}, vendored aport {repo}/{origin} is {want}: locking {want}", file=sys.stderr)
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
                sys.exit(f"lock-dev: cannot download {name}-{ver}.apk (retired from the mirror?): {url}")
            os.replace(tmp, path)
        rows.append((name, ver, repo, origin, sha256(path)))

    rows.sort()
    with open(a.out, "w", encoding="utf-8") as fh:
        fh.write(
            "# Packages the development layer adds to the base image (resolved from guest/dev.pkgs), one per line:\n"
            "#   name version repo origin sha256-of-the-.apk\n"
            f"# Official signed binary packages of the Alpine {a.branch} repositories. Every version equals the pkgver-pkgrel of its\n"
            "# origin aport in vendor/alpine-aports. make-dev.sh checks each sha256, apk verifies each signature against the\n"
            "# Alpine release keys of the image. Refresh deliberately: make-dev.sh --refresh-lock, then review the diff.\n"
        )
        for r in rows:
            fh.write(" ".join(r) + "\n")
    print(f"lock-dev: wrote {len(rows)} packages to {a.out}", file=sys.stderr)


main()
