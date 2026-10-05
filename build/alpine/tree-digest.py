#!/usr/bin/env python3
"""Print a sha256 over (path, type, mode, uid, gid, link target or content hash) of every entry in a
directory tree. Modification times are excluded. Used to prove that two trees are identical and that
the toolchain rootfs did not drift.

  tree-digest.py DIR [--exclude REL]...      REL is a path relative to DIR; its whole subtree is skipped
"""
import hashlib
import os
import stat
import sys


def digest(root, excludes):
    h = hashlib.sha256()
    for dp, dns, fns in os.walk(root):
        rel_dir = os.path.relpath(dp, root)
        dns[:] = sorted(d for d in dns if os.path.normpath(os.path.join(rel_dir, d)) not in excludes)
        for n in sorted(dns + [f for f in fns if os.path.normpath(os.path.join(rel_dir, f)) not in excludes]):
            full = os.path.join(dp, n)
            st = os.lstat(full)
            if stat.S_ISLNK(st.st_mode):
                extra = os.readlink(full)
            elif stat.S_ISREG(st.st_mode):
                with open(full, "rb") as fh:
                    extra = hashlib.sha256(fh.read()).hexdigest()
            else:
                extra = ""
            h.update(f"{os.path.relpath(full, root)}\0{stat.S_IFMT(st.st_mode):o}\0{st.st_mode & 0o7777:o}\0"
                     f"{st.st_uid}\0{st.st_gid}\0{extra}\n".encode())
    return h.hexdigest()


if __name__ == "__main__":
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    root, excl = args[0], set()
    i = 1
    while i < len(args):
        if args[i] != "--exclude" or i + 1 >= len(args):
            sys.exit(__doc__)
        excl.add(os.path.normpath(args[i + 1]))
        i += 2
    print(digest(root, excl))
