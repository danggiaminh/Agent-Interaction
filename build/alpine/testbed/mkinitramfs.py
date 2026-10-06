#!/usr/bin/env python3
"""Assemble the stage-1 initramfs of the cgroup v2 test bed, byte for byte reproducibly.

    python3 -I mkinitramfs.py --image-root DIR --kernel-modules DIR --modules FILE --init FILE --out FILE --epoch N

The initramfs holds busybox and musl of the tools image (DIR), the init script, and the kernel modules listed in
FILE together with their dependencies (from modules.dep of the locked kernel, DIR/modules.dep), decompressed and
numbered in load order. It is a gzip-compressed cpio archive in "newc" format: sorted names, owner 0:0, every
timestamp EPOCH, no gzip timestamp. Nothing is taken from the host; the inputs are the pinned image and kernel.
"""
import argparse
import gzip
import os
import stat
import struct
import sys

APPLETS = ["sh", "mount", "umount", "mkdir", "cat", "echo", "sleep", "insmod", "switch_root", "poweroff", "ls", "dmesg", "sed", "grep", "ln", "mknod"]


def die(msg):
    sys.exit("mkinitramfs: " + msg)


def elf_needed(path):
    """DT_NEEDED entries of a 64-bit little-endian ELF file."""
    with open(path, "rb") as f:
        d = f.read()
    if d[:4] != b"\x7fELF" or d[4] != 2 or d[5] != 1:
        die("%s is not a 64-bit little-endian ELF file" % path)
    phoff, = struct.unpack_from("<Q", d, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", d, 0x36)
    loads, dyn = [], None
    for i in range(phnum):
        p_type, _, p_off, p_vaddr, _, p_filesz, _, _ = struct.unpack_from("<IIQQQQQQ", d, phoff + i * phentsize)
        if p_type == 1:
            loads.append((p_vaddr, p_off, p_filesz))
        elif p_type == 2:
            dyn = (p_off, p_filesz)
    if dyn is None:
        return []

    def off(v):
        for va, fo, sz in loads:
            if va <= v < va + sz:
                return fo + v - va
        die("%s: address %#x is in no segment" % (path, v))

    needed, strtab = [], None
    for o in range(dyn[0], dyn[0] + dyn[1], 16):
        tag, val = struct.unpack_from("<qQ", d, o)
        if tag == 0:
            break
        if tag == 1:
            needed.append(val)
        elif tag == 5:
            strtab = off(val)
    out = []
    for n in needed:
        s = strtab + n
        out.append(d[s:d.index(b"\0", s)].decode())
    return out


def read_dep(moddir):
    deps = {}
    with open(os.path.join(moddir, "modules.dep")) as f:
        for ln in f:
            if ":" not in ln:
                continue
            mod, rest = ln.rstrip("\n").split(":", 1)
            deps[mod] = rest.split()
    return deps


def modname(rel):
    return os.path.basename(rel).split(".ko")[0].replace("-", "_")


def resolve(moddir, wanted):
    deps = read_dep(moddir)
    byname = {modname(m): m for m in deps}
    with open(os.path.join(moddir, "modules.builtin")) as f:
        builtin = {modname(ln.strip()) for ln in f if ln.strip()}
    order = []

    def visit(rel, chain=()):
        if rel in order:
            return
        if rel in chain:
            die("dependency cycle at " + rel)
        for dep in deps[rel]:
            visit(dep, chain + (rel,))
        order.append(rel)

    for name in wanted:
        n = name.replace("-", "_")
        if n in builtin:
            continue
        if n not in byname:
            die("module %s is neither built into the kernel nor in modules.dep" % name)
        visit(byname[n])
    return order


class Cpio:
    def __init__(self, epoch):
        self.entries = {}
        self.epoch = epoch

    def add(self, name, mode, data=b"", rdev=(0, 0)):
        name = name.strip("/")
        parts = name.split("/")
        for i in range(1, len(parts)):
            d = "/".join(parts[:i])
            self.entries.setdefault(d, (stat.S_IFDIR | 0o755, b"", (0, 0)))
        if name in self.entries and not stat.S_ISDIR(mode):
            die("duplicate entry " + name)
        self.entries[name] = (mode, data, rdev)

    def dump(self):
        out = bytearray()
        for ino, name in enumerate(sorted(self.entries), 1):
            mode, data, rdev = self.entries[name]
            nlink = 2 if stat.S_ISDIR(mode) else 1
            nm = name.encode() + b"\0"
            hdr = "070701" + "".join("%08X" % v for v in (
                ino, mode, 0, 0, nlink, self.epoch, len(data), 0, 0, rdev[0], rdev[1], len(nm), 0))
            out += hdr.encode() + nm
            out += b"\0" * (-len(out) % 4)
            out += data
            out += b"\0" * (-len(out) % 4)
        trailer = b"TRAILER!!!\0"
        out += ("070701" + "".join("%08X" % v for v in (0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, len(trailer), 0))).encode() + trailer
        out += b"\0" * (-len(out) % 4)
        return bytes(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--image-root", required=True)
    ap.add_argument("--kernel-modules", required=True)
    ap.add_argument("--modules", required=True)
    ap.add_argument("--init", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--epoch", type=int, required=True)
    a = ap.parse_args()

    c = Cpio(a.epoch)
    for d in ("proc", "sys", "dev", "newroot", "mods", "bin", "lib", "tmp"):
        c.add(d, stat.S_IFDIR | 0o755)
    c.add("dev/console", stat.S_IFCHR | 0o600, rdev=(5, 1))
    c.add("dev/null", stat.S_IFCHR | 0o666, rdev=(1, 3))

    bb = os.path.join(a.image_root, "bin/busybox")
    if not os.path.isfile(bb) or os.path.islink(bb):
        die("no regular /bin/busybox in the image root")
    with open(bb, "rb") as f:
        c.add("bin/busybox", stat.S_IFREG | 0o755, f.read())
    libs = elf_needed(bb)
    if libs != ["libc.musl-x86_64.so.1"]:
        die("busybox needs %s; the stage-1 initramfs carries musl only" % libs)
    musl = os.path.realpath(os.path.join(a.image_root, "lib/ld-musl-x86_64.so.1"))
    if not musl.startswith(os.path.realpath(a.image_root) + "/") or not os.path.isfile(musl):
        die("no musl loader in the image root")
    with open(musl, "rb") as f:
        musl_data = f.read()
    c.add("lib/ld-musl-x86_64.so.1", stat.S_IFREG | 0o755, musl_data)
    c.add("lib/libc.musl-x86_64.so.1", stat.S_IFLNK | 0o777, b"ld-musl-x86_64.so.1")
    for ap_name in APPLETS:
        c.add("bin/" + ap_name, stat.S_IFLNK | 0o777, b"busybox")

    with open(a.modules) as f:
        wanted = [ln.split("#", 1)[0].strip() for ln in f]
    wanted = [w for w in wanted if w]
    load = []
    for i, rel in enumerate(resolve(a.kernel_modules, wanted)):
        path = os.path.join(a.kernel_modules, rel)
        with open(path, "rb") as f:
            raw = f.read()
        data = gzip.decompress(raw) if rel.endswith(".gz") else raw
        fname = "%03d-%s.ko" % (i, modname(rel))
        c.add("mods/" + fname, stat.S_IFREG | 0o644, data)
        load.append(fname)
    c.add("modules.load", stat.S_IFREG | 0o644, ("\n".join(load) + "\n").encode())

    with open(a.init, "rb") as f:
        c.add("init", stat.S_IFREG | 0o755, f.read())

    raw = c.dump()
    with open(a.out, "wb") as f:
        with gzip.GzipFile(filename="", mode="wb", fileobj=f, compresslevel=9, mtime=0) as g:
            g.write(raw)
    print("initramfs: %d entries, %d modules (%s), %d bytes cpio" % (len(c.entries), len(load), " ".join(load), len(raw)))


if __name__ == "__main__":
    main()
