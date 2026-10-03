#!/usr/bin/env python3
"""Dispatch web-testing tasks to webtest from another harness (e.g. a DeepSeek agent).

Three modes, all printing ONE JSON object on stdout (progress goes to stderr):

  replay   Run recorded flows directly with the webtest CLI. No LLM, seconds per flow.
           dispatch_webtest.py replay --flows 'flows/*.yaml' [--url URL] [--login-flow flows/login.yaml]

  agent    Hand the task to Claude Code (`claude -p`) using the repo's webtest skill.
           Progress is tracked in claude-step-relay: this script creates the relay
           task first, so the harness knows the exprId up front and can poll
           <STEP_RELAY_DIR>/experiments/<exprId>.json while Claude works.
           dispatch_webtest.py agent --task task.json      (or --task - for stdin)

  explore  Run `webtest explore` directly with any LLM backend (e.g. DeepSeek),
           no Claude involved.
           dispatch_webtest.py explore --url URL --context "账号 alice / 密码 x" --out flows/site

Task JSON for `agent` mode:
  {
    "title":   "测试 Demo Shop 结账",                 (required)
    "url":     "http://127.0.0.1:8765/",             (required)
    "mode":    "explore" | "run" | "replay",          (default: explore)
    "goal":    "……",                                  (for mode=run)
    "flows":   "flows/*.yaml",                        (for mode=replay)
    "context": "测试账号 alice / 密码 secret123",
    "deny":    ["删除", "支付"],
    "notes":   "额外要求，原样转给 Claude",
    "max_tasks": 6
  }

Environment:
  WEBTEST_REPO      repo root (default: two levels above this file)
  STEP_RELAY_HOME   claude-step-relay checkout (default: /mnt/d/dsh/claude-step-relay)
  STEP_RELAY_DIR    relay data dir; default: the claude-step-relay entry's
                    env.STEP_RELAY_DIR in ~/.claude.json (where the MCP server writes)
  CLAUDE_BIN        claude executable (default: claude)
  WEBTEST_LLM_*     LLM backend for `explore` mode (see README)
"""

import argparse
import glob
import json
import os
import subprocess
import sys
import threading
import time
import xml.etree.ElementTree as ET
from pathlib import Path

REPO = Path(os.environ.get("WEBTEST_REPO", Path(__file__).resolve().parents[2]))
RELAY_HOME = Path(os.environ.get("STEP_RELAY_HOME", "/mnt/d/dsh/claude-step-relay"))
def _relay_dir():
    """Same data dir the MCP server writes to: env, else the MCP config, else ./step-relay."""
    if os.environ.get("STEP_RELAY_DIR"):
        return Path(os.environ["STEP_RELAY_DIR"])
    try:
        cfg = json.loads((Path.home() / ".claude.json").read_text(encoding="utf-8"))
        return Path(cfg["mcpServers"]["claude-step-relay"]["env"]["STEP_RELAY_DIR"])
    except (OSError, ValueError, KeyError, TypeError):
        return REPO / "step-relay"


RELAY_DIR = _relay_dir()
WEBTEST = REPO / "target" / "release" / "webtest"


def log(msg):
    print(f"[dispatch] {msg}", file=sys.stderr, flush=True)


def ensure_built():
    if WEBTEST.exists():
        return
    log("building webtest (release)…")
    env = dict(os.environ)
    env["PATH"] = f"{Path.home() / '.cargo' / 'bin'}:{env.get('PATH', '')}"
    subprocess.run(["cargo", "build", "--release", "-p", "webtest"], cwd=REPO, env=env, check=True)


def webtest(args, timeout=3600):
    """Runs webtest; returns (exit code, stdout)."""
    ensure_built()
    p = subprocess.run([str(WEBTEST), *args], cwd=REPO, capture_output=True, text=True, timeout=timeout)
    sys.stderr.write(p.stderr[-4000:])
    return p.returncode, p.stdout


# ---------------------------------------------------------------- replay mode

def cmd_replay(a):
    out = REPO / "reports" / f"dispatch-{time.strftime('%Y%m%d-%H%M%S')}"
    out.mkdir(parents=True, exist_ok=True)
    flows = sorted({f for pattern in a.flows.split() for f in glob.glob(str(REPO / pattern))})
    if not flows:
        return {"status": "error", "summary": f"no flows match {a.flows!r}"}
    args = []
    if a.storage_state:
        args += ["--storage-state", a.storage_state]
    if a.login_flow:
        args += ["--login-flow", a.login_flow]
    args += ["replay", *flows, "--junit", str(out / "junit.xml"), "--markdown", str(out / "summary.md")]
    if not a.heal:
        args.append("--no-heal")
    if a.url:
        args += ["--url", a.url]
    code, stdout = webtest(args)

    # The JUnit report lists exactly the replayed flows (stdout also shows
    # helper runs such as an automatic login).
    results = []
    junit = out / "junit.xml"
    if junit.exists():
        for case in ET.parse(junit).getroot().iter("testcase"):
            failure = case.find("failure")
            notes = (case.findtext("system-out") or "").strip()
            status = "FAIL" if failure is not None else ("PASS (with notes)" if notes else "PASS")
            results.append({
                "flow": case.get("name"),
                "status": status,
                "seconds": float(case.get("time", 0)),
                "failures": (failure.text or "").splitlines() if failure is not None else [],
                "notes": notes.splitlines(),
            })
    failed = [r for r in results if r["status"] == "FAIL"]
    return {
        "status": {0: "passed", 1: "failed"}.get(code, "error"),
        "summary": f"{len(results) - len(failed)}/{len(results)} flows passed",
        "mode": "replay",
        "exit_code": code,
        "results": results,
        "artifacts": [str(out / "junit.xml"), str(out / "summary.md")],
    }


# ---------------------------------------------------------------- explore mode

def cmd_explore(a):
    args = []
    if a.storage_state:
        args += ["--storage-state", a.storage_state]
    args += ["explore", "--url", a.url, "--out", a.out, "--max-tasks", str(a.max_tasks), "--jobs", str(a.jobs)]
    if a.context:
        args += ["--context", a.context]
    if a.deny:
        args += ["--deny", a.deny]
    code, stdout = webtest(args)
    report = REPO / a.out / "explore.json"
    data = json.loads(report.read_text()) if report.exists() else {}
    tasks = data.get("tasks", [])
    counts = {
        "verified": sum(t["success"] and t["verified"] for t in tasks),
        "review": sum(not t["success"] and not t.get("blocked") for t in tasks),
        "blocked": sum(bool(t.get("blocked")) for t in tasks),
        "unverified": sum(t["success"] and not t["verified"] for t in tasks),
    }
    return {
        "status": "error" if code == 2 or not tasks else ("passed" if counts["verified"] == len(tasks) else "needs_review"),
        "summary": f"{counts['verified']}/{len(tasks)} tasks became verified flows",
        "mode": "explore",
        "exit_code": code,
        "counts": counts,
        "tasks": [{k: t.get(k) for k in ("name", "kind", "success", "verified", "blocked", "flow", "summary")} for t in tasks],
        "artifacts": [str(REPO / a.out / "explore.md"), str(report)],
    }


# ---------------------------------------------------------------- agent mode

def relay_create(title, prompt):
    """Creates the relay task with the relay's own store module; returns exprId."""
    js = (
        f"import {{ createExperiment }} from {json.dumps((RELAY_HOME / 'lib' / 'store.mjs').as_uri())};"
        "const d = createExperiment({ title: process.argv[1], prompt: process.argv[2] });"
        "console.log(d.exprId);"
    )
    p = subprocess.run(
        ["node", "--input-type=module", "-e", js, title, prompt],
        capture_output=True, text=True, check=True,
        env={**os.environ, "STEP_RELAY_DIR": str(RELAY_DIR)},
    )
    return p.stdout.strip()


def relay_state(expr_id):
    f = RELAY_DIR / "experiments" / f"{expr_id}.json"
    return json.loads(f.read_text()) if f.exists() else None


def watch_progress(expr_id, stop):
    """Echoes relay step changes to stderr while Claude works."""
    seen = {}
    while not stop.wait(5):
        state = relay_state(expr_id) or {}
        for s in state.get("steps", []):
            key = (s["id"], s["status"])
            if seen.get(s["id"]) != key:
                seen[s["id"]] = key
                log(f"step {s['id']} {s['status']}: {s['title']}" + (f" — {s['note']}" if s.get("note") else ""))


def build_prompt(task, expr_id, result_file):
    mode = task.get("mode", "explore")
    lines = [
        f"使用 webtest 技能完成下面的网页测试任务（这是自动派发的任务，无人值守，不要提问；信息不足时按技能里的规则如实报告 error）。",
        f"- 标题：{task['title']}",
        f"- 网址：{task['url']}",
        f"- 模式：{mode}",
    ]
    if task.get("goal"):
        lines.append(f"- 目标：{task['goal']}")
    if task.get("flows"):
        lines.append(f"- 要回放的用例：{task['flows']}")
    if task.get("context"):
        lines.append(f"- 测试数据：{task['context']}")
    if task.get("deny"):
        lines.append(f"- 禁止操作含这些关键词的元素：{','.join(task['deny'])}")
    if task.get("max_tasks"):
        lines.append(f"- 探索任务数上限：{task['max_tasks']}")
    if task.get("notes"):
        lines.append(f"- 其他要求：{task['notes']}")
    lines += [
        f"- 进度追踪：exprId={expr_id}（claude-step-relay 任务已创建，不要再 start；先 set_steps，按步 update_step，最后 finalize）",
        f"- RESULT_FILE={result_file}（按技能里的 JSON 格式写入）",
    ]
    return "\n".join(lines)


def cmd_agent(a):
    task = json.load(sys.stdin if a.task == "-" else open(a.task, encoding="utf-8"))
    for key in ("title", "url"):
        if not task.get(key):
            return {"status": "error", "summary": f"task is missing `{key}`"}
    ensure_built()

    expr_id = relay_create(task["title"], json.dumps(task, ensure_ascii=False))
    out = REPO / "reports" / f"agent-{expr_id}"
    out.mkdir(parents=True, exist_ok=True)
    result_file = out / "result.json"
    prompt = build_prompt(task, expr_id, result_file)
    log(f"relay task {expr_id}; state: {RELAY_DIR / 'experiments' / (expr_id + '.json')}")

    allowed = [
        "Skill",
        "Read",
        "Write",
        "Edit",
        "Glob",
        "Grep",
        "Bash(target/release/webtest:*)",
        "Bash(./target/release/webtest:*)",
        "Bash(cargo build:*)",
        "Bash(ls:*)",
        "Bash(curl:*)",
        "Bash(cat:*)",
        "mcp__claude-step-relay",
    ]
    stop = threading.Event()
    watcher = threading.Thread(target=watch_progress, args=(expr_id, stop), daemon=True)
    watcher.start()
    try:
        p = subprocess.run(
            [os.environ.get("CLAUDE_BIN", "claude"), "-p", prompt, "--output-format", "json",
             "--allowedTools", *allowed],
            cwd=REPO, capture_output=True, text=True, timeout=a.timeout,
        )
    finally:
        stop.set()
        watcher.join()

    try:
        claude = json.loads(p.stdout)
    except json.JSONDecodeError:
        claude = {"is_error": True, "result": (p.stdout + p.stderr)[-2000:]}
    result = json.loads(result_file.read_text()) if result_file.exists() else None
    state = relay_state(expr_id) or {}
    return {
        "status": (result or {}).get("status", "error"),
        "summary": (result or {}).get("summary") or "Claude did not write a result file",
        "mode": "agent",
        "exprId": expr_id,
        "result": result,
        "relay": {
            "status": state.get("status"),
            "steps": [{k: s.get(k) for k in ("id", "title", "status", "note")} for s in state.get("steps", [])],
            "state_file": str(RELAY_DIR / "experiments" / f"{expr_id}.json"),
            "trace_file": str(RELAY_DIR / "traces" / f"{expr_id}.md"),
        },
        "claude": {
            "is_error": claude.get("is_error"),
            "reply": claude.get("result"),
            "cost_usd": claude.get("total_cost_usd"),
            "duration_ms": claude.get("duration_ms"),
        },
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="mode", required=True)

    r = sub.add_parser("replay", help="replay flows with the CLI (no LLM)")
    r.add_argument("--flows", default="flows/*.yaml", help="space-separated globs, relative to the repo")
    r.add_argument("--url", help="override every flow's start URL")
    r.add_argument("--storage-state")
    r.add_argument("--login-flow")
    r.add_argument("--heal", action="store_true", help="allow LLM healing of broken locators")

    g = sub.add_parser("agent", help="delegate to Claude Code with step-relay tracking")
    g.add_argument("--task", required=True, help="task JSON file, or - for stdin")
    g.add_argument("--timeout", type=int, default=3600)

    e = sub.add_parser("explore", help="run webtest explore directly (any LLM backend)")
    e.add_argument("--url", required=True)
    e.add_argument("--context", default="")
    e.add_argument("--deny", default="")
    e.add_argument("--out", default="flows/dispatched")
    e.add_argument("--max-tasks", type=int, default=6)
    e.add_argument("--jobs", type=int, default=2)
    e.add_argument("--storage-state")

    a = ap.parse_args()
    try:
        res = {"replay": cmd_replay, "agent": cmd_agent, "explore": cmd_explore}[a.mode](a)
    except Exception as exc:  # report, don't traceback: the caller parses stdout
        res = {"status": "error", "summary": f"{type(exc).__name__}: {exc}"}
    print(json.dumps(res, ensure_ascii=False, indent=2))
    sys.exit(0 if res.get("status") in ("passed", "needs_review") else 1)


if __name__ == "__main__":
    main()
