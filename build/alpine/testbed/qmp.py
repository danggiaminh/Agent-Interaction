#!/usr/bin/env python3
"""Ask a running QEMU, over its QMP socket, which accelerator it uses; print KEY=VALUE lines.

    qmp.py SOCKET

The answer comes from QEMU itself (query-kvm: is hardware virtualization compiled in, is it the active
accelerator), not from what the launcher asked for, so a guest that silently fell back to software emulation
cannot be reported as hardware-accelerated.
"""
import json
import socket
import sys
import time


def main():
    path = sys.argv[1]
    s = None
    for _ in range(200):
        try:
            s = socket.socket(socket.AF_UNIX)
            s.connect(path)
            break
        except OSError:
            s.close()
            s = None
            time.sleep(0.1)
    if s is None:
        print("qmp=fail:cannot connect to " + path)
        return 1
    f = s.makefile("rw")
    json.loads(f.readline())  # greeting

    def call(cmd):
        f.write(json.dumps({"execute": cmd}) + "\n")
        f.flush()
        while True:
            r = json.loads(f.readline())
            if "return" in r or "error" in r:
                return r

    call("qmp_capabilities")
    kvm = call("query-kvm")
    ver = call("query-version")
    st = call("query-status")
    if "return" not in kvm:
        print("qmp=fail:" + json.dumps(kvm))
        return 1
    print("qmp=ok")
    print("qmp_kvm_present=%s" % ("yes" if kvm["return"]["present"] else "no"))
    print("qmp_kvm_enabled=%s" % ("yes" if kvm["return"]["enabled"] else "no"))
    q = ver.get("return", {}).get("qemu", {})
    print("qmp_qemu_version=%s.%s.%s" % (q.get("major"), q.get("minor"), q.get("micro")))
    print("qmp_status=%s" % st.get("return", {}).get("status"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
