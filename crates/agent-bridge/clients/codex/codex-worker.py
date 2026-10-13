#!/usr/bin/env python3
"""Codex reviewer on agent-bridge: answers review tasks sent to the agent `codex` with the
local Codex CLI (`codex exec`, signed in to the user's ChatGPT account).

    AGENT_BRIDGE_TOKEN_FILE=~/.config/agent-bridge/codex.token crates/agent-bridge/clients/codex/codex-worker.py [--once]

For each task addressed to `codex`:
  1. post `ack`;
  2. run `codex exec` read-only with the task's title and body on stdin (Codex's Windows
     sandbox blocks file reads, so everything it reviews must be in the task text);
  3. post `result` (outcome done) with Codex's final message, or `blocked` with the error.
A `verdict: rework` from the requester re-runs the review.
Other messages (close, notes, …) are only marked read.

All reviews run in one persistent Codex thread (`codex exec resume <id>`), so Codex keeps
the context of earlier phases and rounds. A rework of a task already reviewed in that thread
sends only the new message; otherwise (first review there, or the thread could not be
resumed and a new one was started) it sends the whole task thread. The thread id is kept in
`<state>/threads.json`, written next to each review and at the end of each answer, so the
user can open the same thread with `codex resume <id>` (not while a review is running).

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

RESUME_NOTE = """继续担任本线程的独立审查者，规则与本线程开头相同（材料都在 <stdin>，不运行命令、不读文件；中文；第一行 VERDICT；逐条注明【阻断】/【非阻断】；无法核实的标"未核实"）。新的审查材料如下。"""

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


def for_codex(path):
    """A path as Codex sees it: Windows form for a Windows codex.exe."""
    return to_windows(path) if env("CODEX_EXE").lower().endswith(".exe") else str(path)


def load_threads(state_dir):
    f = state_dir / "threads.json"
    try:
        return json.loads(f.read_text())
    except (OSError, ValueError):
        return {"thread": None, "tasks": {}}


def save_threads(state_dir, threads):
    state_dir.mkdir(parents=True, exist_ok=True)
    f = state_dir / "threads.json"
    tmp = f.with_suffix(".tmp")
    tmp.write_text(json.dumps(threads, indent=2))
    tmp.replace(f)


def codex(args, prompt, material, workdir, last):
    """Runs `codex exec …` (or `exec resume …`) once; returns (thread id, final message)."""
    exe = env("CODEX_EXE")
    cmd = [exe, "exec", *args, "--json", "--skip-git-repo-check", "-o", for_codex(last),
           "-c", f'model_reasoning_effort="{os.environ.get("CODEX_EFFORT", "high")}"']
    if os.environ.get("CODEX_MODEL"):
        cmd += ["-m", os.environ["CODEX_MODEL"]]
    cmd.append(prompt)
    last.unlink(missing_ok=True)
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
    with open(workdir / "codex-stderr.log", "a") as log:
        log.write(f"$ {' '.join(cmd[:4])} …\n{r.stderr}\n")
    if r.returncode != 0:
        raise RuntimeError(f"codex exec exited {r.returncode}: {r.stderr.strip()[-800:]}")
    thread = None
    for line in r.stdout.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if event.get("type") == "thread.started":
            thread = event.get("thread_id")
    text = last.read_text(encoding="utf-8").strip() if last.exists() else ""
    if not text:
        raise RuntimeError("codex exec returned an empty answer")
    return thread, text


def run_codex(task, job, full, delta, state_dir):
    """Runs one review in the persistent thread; returns (thread id, final message).
    `delta` (the new message only) is used when this task was already reviewed in the
    thread being resumed, `full` otherwise."""
    workdir = Path(env("CODEX_WORKDIR")) / job
    workdir.mkdir(parents=True, exist_ok=True)
    last = workdir / "last-message.md"
    threads = load_threads(state_dir)
    current = threads.get("thread")
    thread, text = None, None
    if current:
        material = delta if (delta is not None and threads["tasks"].get(task) == current) else full
        (workdir / "material.md").write_text(material)
        try:
            # `exec resume` does not append stdin to a prompt argument (unlike `exec`):
            # with "-" the whole prompt, note and material, is read from stdin.
            thread, text = codex(["resume", current, "-c", 'sandbox_mode="read-only"'],
                                 "-", f"{RESUME_NOTE}\n\n{material}", workdir, last)
            thread = thread or current
        except Exception as e:
            (workdir / "resume-failed.txt").write_text(f"thread {current}: {e}\n")
    if text is None:
        # No thread yet, or it could not be resumed: start a new one with everything.
        (workdir / "material.md").write_text(full)
        thread, text = codex(["-s", "read-only", "-C", for_codex(workdir)], INSTRUCTIONS, full,
                             workdir, last)
        if not thread:
            raise RuntimeError("codex exec did not report a thread id")
    threads["thread"] = thread
    threads.setdefault("tasks", {})[task] = thread
    save_threads(state_dir, threads)
    (workdir / "thread.txt").write_text(thread + "\n")
    return thread, text


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


def rework_message(m):
    """What a re-review in the same thread needs: the requester's new message."""
    return f"## 消息 {m['n']}（{m['from']} · verdict · rework）\n\n" + m.get("body", "")


def review(task, answers_n, title, material, state_dir, first, delta=None):
    """Acks (first review) or reports progress (re-review), runs Codex and posts its result;
    answers_n keys the idempotent replies. `delta` is the new message alone (re-reviews)."""
    post(task, {"kind": "ack" if first else "progress",
                "body": f"Codex 收到（消息 {answers_n}），开始审查。", "wake": "quiet",
                "client_msg_id": f"codex-start-{task}-{answers_n}"}, state_dir)
    try:
        head = f"# {title}\n\n"
        thread, answer = run_codex(task, f"{task}-{answers_n}", head + material,
                                   None if delta is None else head + delta, state_dir)
        footer = (f"\n\n---\n审查线程：`{thread}`（在 Codex 窗口运行 `codex resume {thread}` "
                  f"可以接着讨论；审查进行时请不要同时打开它）")
        reply = {"kind": "result", "outcome": "done", "body": answer + footer}
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
            review(t["id"], last["n"], title, thread_material(t["id"]), state_dir, False,
                   delta=rework_message(last))


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
        review(task, m["n"], title, thread_material(task), state_dir, False, delta=rework_message(m))
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
