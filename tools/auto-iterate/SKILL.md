---
name: auto-iterate
version: 3
description: Autonomously evolve a target repo through N versions under the dsh-web-relay three-party protocol — a main agent implements, an external AI from another vendor reviews each version, and the independent review-gate service (its own system user, private store, signed attestations) decides whether a version may be committed, with a reviewer-strength gate, 3-strike circuit breaker and commit+tag per approved version. Vendor-neutral: the main agent may be Claude, opencode or Codex. Use when the user asks to auto-iterate / 自动迭代 / 自动演进 a project through multiple versions with minimal supervision. Do not use for a single one-off change.
---

# Auto-Iterate（实施者中立版，v3）

对齐 dsh-web-relay 的三方协议：**主 agent**（实施者，任一厂商）／**外部审核者**（另一家厂商的模型）／**用户**（定目标与验收、外部审核不可用时人工把关、终态验收）。

审核门是独立服务 **review-gate**（Rust，`reviewgate` 系统用户运行，`127.0.0.1:7878`）：它自己调用外部 AI、自己保存审核记录、自己核对提交、给通过的版本签名。实施方只能通过 `review-gate` CLI/MCP 发请求，**读不到也改不了**它的状态。

两套状态各管各的：
- **review-gate**：版本门、强度门、熔断、审核记录、签名证明——**唯一判定依据**，也是权威历史。
- **step-relay**（Claude 适配器，可选）：Step List 与可见轨迹；外部审核条目由 review-gate 写。没有 step-relay 时，用 gate 的 `task show`/history 记账，**不得因此跳过审核**。

两边用同一个 `exprId` 关联（`review-gate task link`），仅在使用 step-relay 时需要。

## 身份与令牌

`implementer_family` 是**认证身份的属性**，不是请求参数。每个 agent 用自己的 gate 令牌；管理员在 `/etc/review-gate/agents.json` 里把令牌映射到**模型厂商**（`claude`、`deepseek`、`openai`…），服务端据此填家族。`--implementer-family` 只是可选的一致性校验，填错会 400；**不要试图自报家族**。

未列入映射的令牌回落到 `--token-file`，被视为 `claude`（兼容单 agent 安装；这是一个共享密钥，不是隔离）。换令牌/轮换后重启服务。

## 启动

```
review-gate task init --id <task-id> --goal "<一句话目标>" \
  --acceptance "<最终验收标准，越具体越好>" \
  --iterations <1-10> --repo "<目标仓库路径>" \
  [--min-reviewer web-gemini]   # external-api(4) > web-gemini(3) > implementer-subagent(2) > self-review(1)；manual(5)=用户亲审
```

启动前 `git -C <repo> status --short` 必须干净；不干净就如实告知用户，由其决定先提交/搁置，**不要吞掉用户未提交的工作**。

用 step-relay 时：`step_relay_start` → `review-gate task link` → `step_relay_set_steps`（V1..VN）。不用时跳过，直接进入每轮。

提交守卫（可选，便于所有 agent）：`review-gate guard --install --repo <repo>` 写 `.git/hooks/pre-commit`，任何 `git commit` 都按同一规则检查；它可被 `--no-verify` 绕过，**不是安全边界**，真正的边界是 record + CI `verify-range`。

## 每一轮

1. `review-gate task show --id <task-id>`：读 `status`、`current_iteration`、`goal`、`final_acceptance`、`history`。`status != running` → 把 `stop_reason` 讲给用户，结束。
2. （可选）`step_relay_update_step` 标记 executing。
3. **实施**：按目标、验收标准与上一轮打回意见改代码；只做这一版。上一轮审核全文在 `history`（或 step-relay 轨迹）里。
4. **验证**：能跑测试就跑；不过先修。关键输出写入证据文件（标明“实施方自述”）。
5. **审核**（审核门发起；实施方不写审核提示、不转述结论）：
   ```
   review-gate review run --id <task-id> [--evidence <证据文件>]
   ```
   命令会 `git add -A` 暂存并交 tree/base；审核门只读取 diff、分段送外部 AI、写记录，直到出结论。**审核期间和之后都不要再改工作区**——记录绑定暂存 tree，提交必须一字不差。
   - 退出码 3（无外部 AI）或反复失败：默认门槛下 **停下等人**，不要降级自审；告诉用户启动 bridge/扩展或配置 `/etc/review-gate/env`。仅在用户明确授权替代时，L2 驱动可用 `--fallback-cmd` 起一个全新 reviewer 命令写 `VERDICT:`，再由驱动 `review submit` 提交；门仍按强度门判定，`self-review` 通常会被打回或暂停。
   - 用户 init 时显式允许 `--min-reviewer implementer-subagent`：可用**实施方本家族**的全新子 agent 审，首行 `VERDICT: …`，原文写入文件后 `review-gate review submit --id <task-id> --channel implementer-subagent --file <文件>`。旧名 `claude-subagent` 仍作为别名可读，强度不变（2）。
   - 人工审核只能由用户本人以 reviewgate 身份执行 `review-gate admin manual …`；agent **不能也不得**代填。
6. **记账**：
   - `approved` →
     ```
     git -C <repo> commit -m "<本版做了什么>"      # 只提交暂存区，不加 -a，不再 add
     git -C <repo> tag auto-iterate/<task-id>/v<current_iteration>
     review-gate record --id <task-id> --review <record.id> --commit HEAD --tag auto-iterate/<task-id>/v<current_iteration>
     ```
     审核门核对：记录属于本版且未用过、通道强度够、提交 tree == 被审 tree、恰好一个父提交且等于审核时 HEAD、tag 指向该提交。不符就按报错修正，**不要绕过**。通过后签名证明挂到 `refs/notes/review-gate`。
   - `rejected` → **不 commit**：`review-gate record --id <task-id> --review <record.id>`；`git -C <repo> reset -q` 后按意见继续，版本不变。
7. 按 `record` 的 `action`：
   - `start_round` → 下一版，回第 1 步。
   - `retry_same_round` → 按审核意见回第 3 步。
   - `finalize` → 汇报各版 commit/tag + 提醒人工端到端验收；**自动化的终点不是发布的终点**。推送须用户同意，并带证明：`git push origin <branch> refs/notes/review-gate`。
   - `pause` → 熔断/强度不足，把 `stop_reason` 与最近意见讲给用户。

## 驱动

通用循环（L2，任何宿主可用）：

```
tools/auto-iterate/auto-iterate.sh --task <task-id> \
  --agent-cmd '<跑一轮实施+测试的命令>' --message '<提交说明模板>'
```

它循环：读任务 → 跑 agent → 可选 verify → stage → review run → commit/tag/record → 按 action 继续；把上一轮审核全文通过 `$AUTO_ITERATE_FEEDBACK` 交给下一轮（通过后清空），把 goal/acceptance/task 通过 `$AUTO_ITERATE_GOAL`、`$AUTO_ITERATE_ACCEPTANCE`、`$AUTO_ITERATE_TASK` 交给实施者，把 `--evidence` 路径通过 `$AUTO_ITERATE_EVIDENCE` 交给实施者。启动时默认拒绝脏工作区（`--allow-dirty` 可放开），并用内核 `flock` 锁住每个 worktree（`--lock-timeout`，第二个驱动退出 2；进程退出自动释放）；每轮写 `agent → staged → reviewing → reviewed → committed`（或 `rejected`）状态，崩溃后带 `--resume` 可补记/重审/重跑，未带 `--resume` 会拒绝继续；打回后清理本轮新增的未跟踪文件（开始时干净时）。`--verify-cmd` 在 agent 后独立验收，失败保留改动并退出 1；`--version-plan` 把每版验收文本经 `$AUTO_ITERATE_VERSION_ACCEPTANCE` 交给实施者并写入证据。`--notify-cmd` 在驱动退出时发送 finalized/paused/failed/stopped/done/blocked 通知（`--notify-timeout` 限时，失败不影响退出码）；每个通过版本原子追加到 git dir 的 `auto-iterate/versions.ndjson`（`--manifest FILE` 可改路径并随状态恢复）；`--verify-range` 在 finalize 后调用 `review-gate verify-range` 校验任务提交链（`--verify-range-timeout` 限时）。Codex 宿主用 `tools/auto-iterate/codex-agent.sh`（WSL 仓库优先 native Linux CLI）；opencode 宿主用 `tools/auto-iterate/opencode-agent.sh`（把 prompt 作为消息传给 `opencode run`，默认 `--auto`）。两者都只负责跑一轮并把改动留在工作区，守卫 HEAD/tag/无改动；驱动用 `--adapter codex|opencode` 直接选择（与 `--agent-cmd` 互斥）。`/loop` + `ScheduleWakeup`、`step_relay_*` 都只是 Claude 侧的可选适配器，不是协议前提。

## 审核者不可用时的替代（需用户明确授权）

默认仍是不降级、停下等人。只有在用户**明确**授权时（例如 Claude 额度耗尽、DSH 也失败，用户指示由实施方自己完成并用其子 agent 复核）：

1. 把替代关系写进任务/提交/评审记录：谁因何不可用、由谁替代、复核者是谁。
2. 替代复核必须是**全新上下文**的子 agent，只给 diff、验收标准与证据，不共享实施对话。
3. 绝不把替代复核伪装成外部 AI；`manual` 仍只由用户本人执行。
4. 在最终汇报里再次注明替代链。

## 安装与版本

权威文本在本仓库 `tools/auto-iterate/SKILL.md`（`version: 3`）。各宿主从仓库链接，不要各自分叉：

```bash
tools/auto-iterate/install.sh \
  --skill-dir ~/.claude/skills/auto-iterate \
  --bin-dir ~/.local/bin
```

`--check` 只校验链接，`--dry-run` 只打印计划，`--force` 才替换已有普通文件或不同链接。
默认不会覆盖普通文件；安装器测试：`tools/auto-iterate/test-install.sh`。
opencode / 其他宿主使用各自 skill 目录（或直接引用本文件）；没有 skill 机制时用 L2 驱动。宿主适配器（`codex-agent.sh`、`opencode-agent.sh`）可直接从仓库路径调用，不需要复制。
升级后核对 `head -5 tools/auto-iterate/SKILL.md` 的 `version`，三家应一致。

## 硬性约束

- 从不口头上报 verdict，从不自己撰写/改写审核结论——只认审核门的记录。
- 从不在审核后改动将要提交的内容。
- 默认门槛下外部 AI 不可用就停下等人，不得静默降级；替代必须按上一节留痕。
- `manual` 只记录用户亲口结论，且只能由用户本人执行。
- 从不使用保留角色写 step-relay 轨迹，从不直接改轨迹文件。
- 连续 3 次打回必然 `pause`，必然唤醒用户。
- `done` 之后从不擅自 push / 发布 / 合并。
- 使用 step-relay 时，不跳过记账；不使用时，gate history 即账本。
