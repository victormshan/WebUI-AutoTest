---
name: webtest
description: Test a website end-to-end with the webtest tool in this repo (Rust + chrome-devtools-mcp). Use when the user asks to test, explore, smoke-test or regression-test a web page/site/URL, record or replay UI test flows, check a login/checkout/form flow in a real browser, or when a dispatched task (e.g. from a DeepSeek harness) asks for web UI testing. Covers login-state reuse, autonomous exploration, deterministic replay, reports, and progress tracking via claude-step-relay.
---

# webtest：网页端到端测试

本仓库的 `webtest` 用 chrome-devtools-mcp 驱动无头 Chrome。工作分两段：

1. **探索/录制**：需要大模型，由它理解页面、自主操作，产出用例 YAML。
2. **回放**：不需要大模型，几秒钟一个用例，可放进 CI。

完整说明见 `README.md`。

## 0. 准备（每次任务先做）

```bash
command -v cargo >/dev/null || . "$HOME/.cargo/env"
[ -x target/release/webtest ] || cargo build --release -p webtest
WT=target/release/webtest
$WT check-llm        # 只有要做探索或录制时才需要；回放不需要大模型
```

- Chrome 会自动找到：WSL 下用 `~/.local/bin/chrome-wsl`，其他环境用系统 Chrome。
- 如果找不到 cargo 或 Chrome，先运行 `scripts/setup-wsl.sh`。
- 大模型后端由环境变量决定（见 README「大模型后端」）。默认是本机的 `claude -p`。

## 1. 先弄清任务，缺什么问什么

动手前要确认以下几项；缺少必要信息就先问，不要猜：

- **目标网址**，以及它是不是测试环境。
- **测试数据**：账号、地址等。
- **测试重点**：用户给了明确目标就用 `run`；只说"测一下"就用 `explore`。
- **禁区**：不能点的东西，比如删除、支付、发消息。默认禁用关键词是 `删除,delete,remove,注销账号`，可以按站点补充。

**安全规则（必须遵守）**：

- 只测用户明确授权的站点。
- 如果是生产环境或第三方站点，先向用户确认：探索会真实提交表单、发送请求。
- 不提交 `auth/`、`runs/`、`reports/` 这三个目录：里面有会话凭据和页面内容。

## 2. 选择命令

| 场景 | 命令 |
|---|---|
| 需要登录 | `$WT login --url URL --goal "用 账号/密码 登录" --save-state auth/NAME.json`；已有登录用例时用 `--flow flows/login.yaml`（不调用大模型）；有验证码或两步验证时用 `--manual`，需要用户在场 |
| 有明确目标 | `$WT [--storage-state auth/NAME.json] run --url URL --goal "……以及预期看到的结果" --save flows/SITE/NAME.yaml` |
| 让工具自己摸索站点 | `$WT [--storage-state …] [--login-flow …] explore --url URL --context "测试数据……" --max-tasks 6 --jobs 2 --out flows/SITE` |
| 回放已有用例 | `$WT [--login-flow flows/login.yaml] replay flows/SITE/*.yaml --no-heal --junit reports/junit.xml --markdown reports/summary.md` |
| 看页面结构 | `$WT snapshot URL` |

几点注意：

- `--storage-state`、`--login-flow` 等全局参数要写在子命令**之前**。
- `replay` 时用例会自己声明是否需要登录态：全局参数只替换已声明这一项的用例所用的路径，未声明的用例照常从未登录状态开始。所以回放时一般不需要加这两个参数。
- 探索时先把任务数设小（`--max-tasks 4~6`），每个任务大约要调用 5–10 次大模型。
- **交叉评审**：如果有外部 AI 可用（先运行 `$WT check-llm --review-command "review-gate ask"`；`review-gate` 不在 PATH 上时用仓库里构建的 `target/release/review-gate ask` 确认），探索时加上同样的 `--review-command`。另一家厂商的模型会审任务提案（否决或补充）和用例断言。汇报时要说明外部 AI 否决了什么、补充了什么；凡是它否决的任务，都要连同理由一并转述。
- 回放默认加 `--no-heal`。只有当用户想让工具自动修复失效的元素定位时，才去掉它。

## 3. 读懂结果，如实汇报

**退出码**：0 = 全部通过，1 = 有用例失败，2 = 工具或配置出错。

**回放状态**：

| 状态 | 含义 |
|---|---|
| `PASS` | 通过 |
| `PASS (healed)` | 有元素定位被大模型修复，修复结果在 `*.healed.yaml`，需要人审核 |
| `PASS (session refreshed)` | 登录态过期后自动刷新，再重试通过 |
| `FAIL` | 失败。页面快照在 `runs/replay-*.snapshot.txt` |

**探索结果**：

| 标记 | 含义 |
|---|---|
| `verified` | 生成的用例回放验证通过，已保存 |
| `REVIEW` | 页面没有满足大模型提出的期望：可能是缺陷，也可能是目标写得过细，需要人判断 |
| `BLOCKED` | 碰到了禁用关键词，不算站点问题 |
| `UNVERIFIED` | 用例不稳定，回放验证没通过 |

**被测内容每次会变时**（AI 回答、推荐、随机问候语等）：

- 先检查生成的断言里有没有这类会变的文本，有就删掉，或者换成稳定的标题、标签。
- 改完至少连续回放 3 次再下结论。

**汇报内容**：

1. 测了什么（网址、模式、任务数）；
2. 通过和失败的数量，以及每个失败的原因（引用报告里的原文）；
3. 需要人判断的项目；
4. 生成的文件路径。

不要把 REVIEW 或 UNVERIFIED 说成通过。

## 4. 被派发的任务：进度追踪和结果文件

### 任务里给了 `exprId` 时

用 claude-step-relay 记录进度：

1. 开始时调用 `step_relay_set_steps`，写入本次的步骤，每步带 `acceptance`（验收标准）。
2. 每开始或完成一步，调用 `step_relay_update_step`，状态依次是 `executing` → `done`；遇到阻塞时标 `blocked` 并写明原因。
3. 关键发现（失败原因、需要人判断的项目）用 `step_relay_append_trace` 记录，`role` 填 `Claude`。
4. 结束时调用 `step_relay_finalize`，`summary` 写最终结论。

任务里没有给 `exprId`、但需要留痕时，自己用 `step_relay_start` 创建任务。

### 任务里给了 `RESULT_FILE` 时

结束前把下面这个 JSON 写入该文件，派发方会读取它：

```json
{
  "status": "passed | failed | needs_review | error",
  "summary": "一句话结论",
  "url": "被测网址",
  "mode": "replay | run | explore",
  "counts": {"passed": 0, "failed": 0, "review": 0, "blocked": 0, "unverified": 0},
  "failures": [{"flow": "名称", "reason": "报告原文"}],
  "artifacts": ["flows/...yaml", "reports/summary.md", "flows/SITE/explore.md"]
}
```

`status` 的取值规则：

- 有失败就填 `failed`；
- 没有失败、但有需要人判断或不稳定的项目，填 `needs_review`；
- 工具或环境出错，填 `error`。
