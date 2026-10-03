#!/usr/bin/env python3
"""Build a webtest task for dsh-web-relay's Claude Code channel (cc-tasks) and enqueue it.

The dsh cc channel works through files: a task contract dropped into
<CC_TASKS>/queue/<taskId>.task.json is picked up by cc-watchdog, run by
runner.sh as `claude -p` (cwd = <CC_TASKS>), and settled into
<CC_TASKS>/tasks/<taskId>/{result.json, done.flag, out/}.

This script writes a v2-schema contract (kind=review) that tells Claude to run
webtest per this repo's skill and to deliver into out/:

  review.md            first line `VERDICT: APPROVED|REJECTED` (parsed by cc-channel parseCcVerdict)
  webtest-result.json  machine-readable result (skill's RESULT_FILE schema)
  report.md            human-readable report
  summary.md, junit.xml / explore.md, flows/   (as applicable)

Tool or environment errors produce no done.flag, so runner.sh marks the task
failed and dsh's existing fallback/failure classification applies.

Usage:
  make_webtest_task.py --url http://127.0.0.1:8765/ --mode replay --flows "flows/*.yaml" \\
      --login-flow flows/login.yaml [--dry-run]
  make_webtest_task.py --url https://staging.example.com --mode explore \\
      --context "账号 alice / 密码 x" --max-tasks 4
  make_webtest_task.py --url ... --mode run --goal "登录后把商品加入购物车并结算，确认显示订单已提交"

Environment: CC_TASKS (default /mnt/d/cc-tasks), DSH_WEB_RELAY (default /mnt/d/dsh-web-relay).
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
CC_TASKS = Path(os.environ.get("CC_TASKS", "/mnt/d/cc-tasks"))
RELAY = Path(os.environ.get("DSH_WEB_RELAY", "/mnt/d/dsh-web-relay"))
VALIDATOR = RELAY / "scripts" / "task-schema-cli.mjs"


def build_task(a):
    task_id = a.task_id or f"webtest-{time.strftime('%Y%m%d-%H%M%S')}"
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}", task_id):
        sys.exit(f"invalid task id: {task_id}")
    task_dir = CC_TASKS / "tasks" / task_id
    out = task_dir / "out"
    wt = REPO / "target" / "release" / "webtest"

    params = [f"- 被测网址：{a.url}", f"- 模式：{a.mode}"]
    if a.goal:
        params.append(f"- 目标：{a.goal}")
    if a.flows:
        params.append(f"- 回放用例（相对仓库根）：{a.flows}")
    if a.login_flow:
        if a.mode == "replay":
            params.append(f"- 登录用例：{a.login_flow}（只加全局参数 --login-flow {a.login_flow}；不要加 --storage-state——用例自己声明是否需要登录态）")
        else:
            params.append(f"- 登录用例：{a.login_flow}（先 `webtest login --flow {a.login_flow} --save-state auth/{task_id}.json`，再给 run/explore 加全局参数 --storage-state auth/{task_id}.json --login-flow {a.login_flow}）")
    if a.context:
        params.append(f"- 测试数据：{a.context}")
    if a.deny:
        params.append(f"- 禁止操作含这些关键词的元素：{a.deny}")
    if a.mode == "explore":
        params.append(f"- 探索任务数上限：{a.max_tasks}")
    if a.notes:
        params.append(f"- 其他要求：{a.notes}")

    prompt = f"""你是网页测试执行方（Claude Code headless，由 dsh-web-relay cc 通道派发）。用 webtest 工具完成一次网页测试，并按下面的约定交付。

【工具与规范】
- webtest 仓库根：{REPO}（所有 webtest 命令都在这里执行：`cd {REPO}`）
- 先完整读取并遵守技能文件：{REPO}/.claude/skills/webtest/SKILL.md（可用 Read 或 cat）。其中的安全规则必须遵守；本任务无人值守，不提问，信息不足时按 error 交付。
- 可执行文件：{wt}（不存在时按技能「准备」一节构建）。
- 不要 git commit，不要修改仓库里已有的文件；临时产物只允许写在仓库的 target/、auth/、runs/、reports/ 下。

【任务参数】
{chr(10).join(params)}

【命令约定】
- replay：`webtest [全局参数] replay <用例> --no-heal --junit {out}/junit.xml --markdown {out}/summary.md`
- run：`webtest [全局参数] run --url <网址> --goal "<目标>" --save {out}/flows/<名称>.yaml`
- explore：`webtest [全局参数] explore --url <网址> --context "<测试数据>" --max-tasks <上限> --out {out}/flows`
- 全局参数（--storage-state / --login-flow 等）写在子命令之前。

【交付（全部写入 {out}/）】
1. webtest-result.json：技能第 4 节 RESULT_FILE 的 JSON 格式（status = passed|failed|needs_review|error）。
2. report.md：中文报告——测了什么、通过/失败数、每个失败的原因（引用报告原文）、需人工判断项、产物路径。
3. review.md：第一行严格为 `VERDICT: APPROVED`（仅当 status=passed）或 `VERDICT: REJECTED`（failed 或 needs_review），空一行后写理由要点。
4. 适用时：summary.md、junit.xml（replay）；flows/ 与 flows/explore.md（run/explore）。

【完成标记】
- status 为 passed/failed/needs_review：交付齐全后，在任务根目录写入 {task_dir}/done.flag（不是 out/ 里）。
- status 为 error（工具或环境故障，例如站点不可达、构建失败）：仍写 webtest-result.json 和 report.md 说明原因，但**不要**写 done.flag。
"""
    task = {
        "taskId": task_id,
        "kind": "review",
        "title": a.title or f"webtest {a.mode}: {a.url}",
        "prompt": prompt,
        "refs": [str(REPO / ".claude/skills/webtest/SKILL.md"), str(REPO / "README.md")],
        "acceptance": (
            "out/webtest-result.json 合法且 status ∈ passed|failed|needs_review；"
            "out/review.md 首行 VERDICT 与 status 一致（passed→APPROVED，否则 REJECTED）；"
            "out/report.md 存在；结论与 webtest 实际输出一致，不把 REVIEW/UNVERIFIED 报成通过；"
            "仓库无已跟踪文件改动。"
        ),
        "outputDir": "out",
        "expectArtifacts": ["webtest-result.json", "report.md", "review.md"],
    }
    return task


def validate(path):
    if not VALIDATOR.exists():
        print(f"[warn] validator not found: {VALIDATOR}", file=sys.stderr)
        return True
    p = subprocess.run(["node", str(VALIDATOR), "validate-task", str(path)], capture_output=True, text=True)
    if p.returncode != 0:
        print(p.stdout + p.stderr, file=sys.stderr)
    return p.returncode == 0


def main():
    ap = argparse.ArgumentParser(description="Enqueue a webtest task into dsh-web-relay's cc-tasks channel")
    ap.add_argument("--url", required=True)
    ap.add_argument("--mode", choices=["replay", "run", "explore"], default="replay")
    ap.add_argument("--goal")
    ap.add_argument("--flows", help='e.g. "flows/*.yaml" (replay)')
    ap.add_argument("--login-flow")
    ap.add_argument("--context")
    ap.add_argument("--deny")
    ap.add_argument("--max-tasks", type=int, default=4)
    ap.add_argument("--notes")
    ap.add_argument("--title")
    ap.add_argument("--task-id")
    ap.add_argument("--dry-run", action="store_true", help="print the contract, do not enqueue")
    a = ap.parse_args()
    if a.mode == "run" and not a.goal:
        ap.error("--mode run needs --goal")
    if a.mode == "replay" and not a.flows:
        ap.error("--mode replay needs --flows")

    task = build_task(a)
    text = json.dumps(task, ensure_ascii=False, indent=2)
    if a.dry_run:
        print(text)
        return

    queue = CC_TASKS / "queue"
    if (CC_TASKS / "tasks" / task["taskId"] / "task.json").exists():
        sys.exit(f"task {task['taskId']} already exists")
    # Write under a name the watchdog ignores, validate, then rename atomically.
    tmp = queue / f".{task['taskId']}.task.json.tmp"
    tmp.write_text(text, encoding="utf-8")  # UTF-8 without BOM (lesson 004)
    if not validate(tmp):
        tmp.unlink()
        sys.exit("task failed v2 schema validation; not enqueued")
    final = queue / f"{task['taskId']}.task.json"
    tmp.rename(final)
    print(json.dumps({
        "taskId": task["taskId"],
        "queued": str(final),
        "taskDir": str(CC_TASKS / "tasks" / task["taskId"]),
        "settle": "result.json status=done|failed; verdict in out/review.md; details in out/webtest-result.json",
    }, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
