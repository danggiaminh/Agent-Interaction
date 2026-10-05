"""Read the version an aport builds, without executing the APKBUILD.

version(path) returns "pkgver-rPKGREL" for an APKBUILD. pkgver and pkgrel are usually plain assignments;
when they refer to another variable of the same file (pkgver=${_pkgver/-/.}) the reference is expanded
from that file's own plain assignments: $var, ${var}, ${var/old/new}, ${var%suffix}, ${var#prefix}.
Anything else is an error: a version is never guessed.
"""
import re


def _assignments(text):
    out = {}
    for m in re.finditer(r"^([A-Za-z_][A-Za-z0-9_]*)=(.*)$", text, re.M):
        v = m.group(2).strip()
        if len(v) >= 2 and v[0] == v[-1] and v[0] in "\"'":
            v = v[1:-1]
        out.setdefault(m.group(1), v)
    return out


def _expand(value, env, path, depth=0):
    if "$" not in value:
        return value
    if depth > 8:
        raise SystemExit(f"apkbuild: {path}: cannot evaluate '{value}'")

    def sub(m):
        name, op, a, b = m.group("name") or m.group("bare"), m.group("op"), m.group("a"), m.group("b")
        if name not in env:
            raise SystemExit(f"apkbuild: {path}: '{value}' refers to {name}, which is not a plain assignment")
        val = _expand(env[name], env, path, depth + 1)
        if op == "/":
            return val.replace(a, b or "", 1)
        if op == "%":
            return val[: -len(a)] if a and val.endswith(a) else val
        if op == "#":
            return val[len(a):] if a and val.startswith(a) else val
        return val

    out = re.sub(r"\$(?:\{(?P<name>\w+)(?:(?P<op>[/%#])(?P<a>[^/}]*)(?:/(?P<b>[^}]*))?)?\}|(?P<bare>\w+))", sub, value)
    if "$" in out:
        raise SystemExit(f"apkbuild: {path}: cannot evaluate '{value}'")
    return out


def version(path):
    with open(path, encoding="utf-8") as fh:
        env = _assignments(fh.read())
    try:
        ver, rel = env["pkgver"], env["pkgrel"]
    except KeyError as e:
        raise SystemExit(f"apkbuild: {path}: no {e.args[0]}")
    return f"{_expand(ver, env, path)}-r{_expand(rel, env, path)}"
