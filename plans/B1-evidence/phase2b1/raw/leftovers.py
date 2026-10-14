"""Lists browser-test leftovers by reading /proc, so this script's own command line never matches."""
import os, sys
me = {os.getpid(), os.getppid()}
def procs():
    for d in os.listdir("/proc"):
        if not d.isdigit() or int(d) in me:
            continue
        try:
            with open(f"/proc/{d}/cmdline", "rb") as f:
                yield int(d), f.read().replace(b"\0", b" ").decode(errors="replace")
        except OSError:
            pass
checks = {
    "Chrome for Testing processes (chrome-linux64/chrome)": lambda c: "chrome-linux64/chrome" in c,
    "chrome-devtools-mcp servers (node)": lambda c: "chrome-devtools-mcp" in c and ("node" in c or "npm" in c),
    "processes using a webtest-native-* profile": lambda c: "webtest-native-" in c,
}
for name, f in checks.items():
    hits = [(p, c[:120]) for p, c in procs() if f(c)]
    print(f"{name}: {len(hits)}")
    for p, c in hits:
        print(f"  {p}: {c}")
left = [n for n in os.listdir("/tmp") if n.startswith(("webtest-native-", "webtest-profile-"))]
print(f"/tmp webtest-native-*/webtest-profile-* directories: {len(left)}")
