#!/usr/bin/env python3
"""Codex reviewer on agent-bridge: answers review tasks sent to the agent `codex` with the
local Codex CLI (`codex exec`, signed in to the user's ChatGPT account).

    AGENT_BRIDGE_TOKEN_FILE=~/.config/agent-bridge/codex.token crates/agent-bridge/clients/codex/codex-worker.py [--once]

For each task addressed to `codex`:
  1. post `ack`;
  2. run `codex exec` read-only with the task's title and body on stdin (Codex's Windows
     sandbox blocks file reads, so everything it reviews must be in the task text);
  3. post `result` (outcome done) with Codex's final message, or `blocked` with the error.
A `verdict: rework` from the requester re-runs the review on the whole thread.
Other messages (close, notes, …) are only marked read.

Replies carry a client_msg_id derived from the message they answer, so a retry never posts
the same answer twice; reviews that were started but never answered (worker died after its
ack, which already marked the task read) are found again from the task list and re-run.

Environment:
  AGENT_BRIDGE_TOKEN_FILE  the codex agent's token (required; never the claude token)
  AGENT_BRIDGE_URL         default http://127.0.0.1:7879
  AGENT_BRIDGE_BIN         default agent-bridge
  CODEX_EXE                Codex CLI (a Windows codex.exe is fine; called through WSL interop)
  CODEX_WORKDIR            where each review gets its own directory (material.md, the answer,
                           Codex's log), as a WSL path; for a Windows codex.exe it must be on a
                           Windows drive (e.g. /mnt/e/rust-agent-browser/reviews)
  CODEX_MODEL              optional model override
  CODEX_EFFORT             reasoning effort, default high
  CODEX_TIMEOUT            seconds per review, default 1800
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

INSTRUCTIONS = """你是独立审查者（与提交方 Claude Code 不同厂商的模型），负责审查一个软件项目的阶段材料。

规则：
- 要审查的全部材料都在 <stdin> 里。不要运行任何命令，也不要尝试读取文件；材料里没有的内容就说"材料中没有"。
- 用中文回复。
- 第一行必须是下面四个之一：
  VERDICT: APPROVE
  VERDICT: APPROVE WITH CONDITIONS
  VERDICT: REWORK
  VERDICT: BLOCK
- 然后逐条给出结论和理由：
  - 每条注明【阻断】或【非阻断】；条件和必须修改的项要具体、可验证；
  - 材料里的说法如果无法根据材料本身核实（例如只有结论没有原始输出），要标明"未核实"；
  - 不要因为"测试通过"就默认实现正确；检查测试是否真的覆盖了声称的行为。
- 如果材料里提出了需要你回答的问题，逐一回答。
- 涉及 sudo、账号登录、合并到 main 等只能由用户本人决定的事项，指出来交给用户，不要代为决定。
"""


def env(name, default=None):
    v = os.environ.get(name, default)
    if v is None:
        sys.exit(f"codex-worker: {name} is not set")
    return v


def bridge(*args, input_text=None):
    """Runs the agent-bridge client and returns its parsed JSON output (or None)."""
    r = subprocess.run(
        [env("AGENT_BRIDGE_BIN", "agent-bridge"), *args],
        input=input_text,
        capture_output=True,
        text=True,
        timeout=300,
    )
    if r.returncode != 0:
        raise RuntimeError(f"agent-bridge {args[0]} failed: {r.stderr.strip() or r.stdout.strip()}")
    out = r.stdout.strip()
    return json.loads(out) if out.startswith(("{", "[")) else None


def post(task, message, state_dir):
    """Posts a full message (JSON) to a task."""
    state_dir.mkdir(parents=True, exist_ok=True)
    f = state_dir / f"post-{task}.json"
    f.write_text(json.dumps(message, ensure_ascii=False))
    try:
        bridge("post", "--task", task, "--json", str(f))
    finally:
        f.unlink(missing_ok=True)


def to_windows(path):
    """WSL path -> Windows path, for a Windows codex.exe."""
    r = subprocess.run(["wslpath", "-w", str(path)], capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 else str(path)


def run_codex(material, job):
    """Runs one review; returns Codex's final message."""
    exe = env("CODEX_EXE")
    workdir = Path(env("CODEX_WORKDIR")) / job
    workdir.mkdir(parents=True, exist_ok=True)
    last = workdir / "last-message.md"
    last.unlink(missing_ok=True)
    windows = exe.lower().endswith(".exe")
    conv = to_windows if windows else str
    cmd = [
        exe,
        "exec",
        "-s",
        "read-only",
        "--skip-git-repo-check",
        "-C",
        conv(workdir),
        "-o",
        conv(last),
        "-c",
        f'model_reasoning_effort="{os.environ.get("CODEX_EFFORT", "high")}"',
    ]
    if os.environ.get("CODEX_MODEL"):
        cmd += ["-m", os.environ["CODEX_MODEL"]]
    cmd.append(INSTRUCTIONS)
    (workdir / "material.md").write_text(material)
    r = subprocess.run(
        cmd,
        input=material,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=workdir,
        timeout=int(os.environ.get("CODEX_TIMEOUT", "1800")),
    )
    (workdir / "codex-stderr.log").write_text(r.stderr)
    if r.returncode != 0:
        raise RuntimeError(f"codex exec exited {r.returncode}: {r.stderr.strip()[-800:]}")
    text = last.read_text(encoding="utf-8").strip() if last.exists() else r.stdout.strip()
    if not text:
        raise RuntimeError("codex exec returned an empty answer")
    return text


def thread_material(task):
    """The whole task thread, for a re-review after `rework`."""
    t = bridge("show", task)
    parts = []
    for m in t.get("messages", []):
        head = f"## 消息 {m['n']}（{m['from']} · {m['kind']}"
        if m.get("judgement"):
            head += f" · {m['judgement']}"
        if m.get("outcome"):
            head += f" · {m['outcome']}"
        parts.append(head + "）\n\n" + m.get("body", ""))
    return "\n\n".join(parts)


def review(task, answers_n, title, material, state_dir, first):
    """Acks (first review) or reports progress (re-review), runs Codex and posts its result;
    answers_n keys the idempotent replies."""
    post(task, {"kind": "ack" if first else "progress",
                "body": f"Codex 收到（消息 {answers_n}），开始审查。", "wake": "quiet",
                "client_msg_id": f"codex-start-{task}-{answers_n}"}, state_dir)
    try:
        answer = run_codex(f"# {title}\n\n{material}", f"{task}-{answers_n}")
        reply = {"kind": "result", "outcome": "done", "body": answer}
    except Exception as e:  # report every failure; never pretend the review happened
        reply = {"kind": "result", "outcome": "blocked", "body": f"Codex 审查没有完成：{e}"}
    reply.update({"wake": "normal", "client_msg_id": f"codex-result-{task}-{answers_n}"})
    post(task, reply, state_dir)


def owed(state_dir):
    """Re-runs reviews Codex started but never answered (e.g. the worker died after its ack).
    Posting the ack already moved the read cursor, so the inbox alone would never show them
    again. A task to codex that is acked or working is waiting for a result."""
    for t in (bridge("list", "--open") or {}).get("tasks", []):
        if t.get("to") != "codex" or t.get("state") not in ("acked", "working"):
            continue
        thread = bridge("show", t["id"])
        requests = [m for m in thread.get("messages", [])
                    if m["kind"] == "task" or (m["kind"] == "verdict" and m.get("judgement") == "rework")]
        if not requests:
            continue
        last = requests[-1]
        title = thread.get("task", {}).get("title", t["id"])
        if last["kind"] == "task":
            review(t["id"], last["n"], title, last.get("body", ""), state_dir, True)
        else:
            review(t["id"], last["n"], title, thread_material(t["id"]), state_dir, False)


def poll(state_dir, wait):
    """One round: new messages first, then reviews still owed."""
    inbox = bridge("inbox", "--wait", str(wait)) or {}
    for m in sorted(inbox.get("messages", []), key=lambda m: (m["task"], m["n"])):
        handle(m, state_dir)
    owed(state_dir)


def handle(m, state_dir):
    task, kind = m["task"], m["kind"]
    if kind == "task":
        review(task, m["n"], (m.get("meta") or {}).get("title", task), m.get("body", ""), state_dir, True)
    elif kind == "verdict" and m.get("judgement") == "rework":
        title = bridge("show", task).get("task", {}).get("title", task)
        review(task, m["n"], title, thread_material(task), state_dir, False)
    bridge("ack", "--task", task, "--n", str(m["n"]))


def main():
    once = "--once" in sys.argv[1:]
    env("AGENT_BRIDGE_TOKEN_FILE")
    env("CODEX_EXE")
    env("CODEX_WORKDIR")
    state_dir = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state")) / "codex-worker"
    while True:
        try:
            poll(state_dir, 120)
        except Exception as e:
            print(f"codex-worker: {e}", file=sys.stderr, flush=True)
            if once:
                sys.exit(1)
            time.sleep(30)
            continue
        if once:
            return


if __name__ == "__main__":
    main()
