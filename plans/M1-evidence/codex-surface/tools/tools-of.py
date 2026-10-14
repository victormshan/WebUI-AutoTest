#!/usr/bin/env python3
"""Every tool the model is offered: top-level `tools`, `additional_tools` items, and the nested
tools that code mode's `exec` description lists (declared as `tools.<name>` or `declare const tools`)."""
import json, re, sys
r = json.load(open(sys.argv[1]))
def walk(ts, prefix=""):
    for t in ts:
        n = t.get("name") or t.get("type")
        yield prefix + n
        if "tools" in t: yield from walk(t["tools"], prefix + n + ".")
names = list(walk(r.get("tools", [])))
for it in r.get("input", []):
    if it.get("type") == "additional_tools": names += list(walk(it["tools"]))
print("offered:", names)
blob = json.dumps(r)
nested = sorted(set(re.findall(r"tools\.([a-zA-Z_][a-zA-Z0-9_]*)\(", blob)))
print("named in descriptions as tools.X(...):", nested)
for it in r.get("input", []):
    if it.get("type") == "additional_tools":
        for ns in it["tools"]:
            for t in ns.get("tools", []):
                d = t.get("description", "")
                i = d.find("declare const tools")
                if i >= 0: print("exec nested tool declarations:\n", d[i:i+1500])
