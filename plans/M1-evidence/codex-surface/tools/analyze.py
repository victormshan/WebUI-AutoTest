#!/usr/bin/env python3
"""Summarise a captured Responses request: tools, and which planted markers reach the model."""
import json, sys
r = json.load(open(sys.argv[1]))
def name(t):
    if t.get("type") == "function": return "function:" + (t.get("name") or t.get("function",{}).get("name","?"))
    if t.get("type") == "namespace" or "tools" in t: return f"{t.get('type')}:{t.get('name','?')}[" + ",".join(name(x) for x in t.get("tools",[])) + "]"
    return f"{t.get('type')}:{t.get('name','')}"
print("keys:", sorted(r.keys()))
print("model:", r.get("model"))
print("tools:", [name(t) for t in r.get("tools", [])])
blob = json.dumps(r, ensure_ascii=False)
for m in ["PLANTED-GLOBAL-AGENTS", "PLANTED-PARENT-AGENTS", "PLANTED-JOBS-AGENTS", "PLANTED-SKILL-DESC", "PLANTED-SKILL-BODY", "planted-skill", "planted", "mcp"]:
    print(f"marker {m}: {'PRESENT' if m in blob else 'absent'}")
print("input items:", [ (i.get('role'), i.get('type')) for i in r.get("input", [])])
