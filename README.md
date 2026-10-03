# webtest — 基于 chrome-devtools-mcp 的 Rust 网页测试环境

Rust 通过 MCP（`rmcp`）驱动 [chrome-devtools-mcp](https://github.com/ChromeDevTools/chrome-devtools-mcp)，
分两个阶段：

1. **探索**（`run`）：Claude 读取页面无障碍树、自主操作，完成自然语言描述的目标，生成轨迹和测试用例（flow）。
2. **回放**（`replay`）：按 flow 确定性执行，不调用大模型，几秒跑完，适合放进 CI。元素被改名或移动时，才调用大模型修复定位（自愈）。

```
webtest (Rust) ──stdio/MCP──► chrome-devtools-mcp ──CDP──► Chrome (headless)
      │
      └──► Claude（ANTHROPIC_API_KEY → Messages API；否则 → 本机 `claude -p`）
```

## 目录

| crate | 作用 |
|---|---|
| `crates/mcp-client` | 启动 chrome-devtools-mcp，调用工具，`navigate` / `snapshot` 便捷方法 |
| `crates/llm` | `Llm` trait，Anthropic API 与 Claude Code CLI 两种后端，JSON 提取 |
| `crates/agent` | 观察→决策→执行循环、快照压缩、语义定位器、安全拦截、自动诊断；`flow`（测试用例格式）、`replay`（回放、断言、自愈、JUnit） |
| `crates/cli` | `webtest` 命令行 |
| `fixtures/shop` | 演示站点：`server.py`（静态页面 + HttpOnly Cookie 会话），登录 / 加购 / 结算，用 localStorage 记住收货地址，内置一个 console 错误和一个 404。`make_variants.py` 生成以下变体：`v2-renamed`（按钮改名）、`v3-bug`（金额算错）、`v4-console`（新增 console 错误） |
| `flows/` | 测试用例 YAML |

## 环境准备（WSL，无需 sudo）

```bash
scripts/setup-wsl.sh        # Rust + zig 链接器 + Chrome for Testing（装在 ~/.local 下）
cargo build --release
```

`webtest` 默认使用 `~/.local/bin/chrome-wsl`，也可用 `--chrome` 或 `WEBTEST_CHROME` 指定。

## 用法

```bash
# 演示站点（静态页面 + Cookie 会话登录接口）
python3 fixtures/shop/server.py 8765 &

webtest tools                               # 列出 MCP 工具
webtest snapshot http://127.0.0.1:8765/     # 打印页面无障碍快照
webtest run --url http://127.0.0.1:8765/ \
  --goal "用账号 alice / 密码 secret123 登录，把机械键盘和无线鼠标加入购物车，填写收货地址后结算，确认订单已提交且合计金额正确"
```

`run` 输出 PASS/FAIL、结论、页面证据、console 错误和失败请求，失败时退出码为 1；
完整轨迹写入 `runs/<时间>.json`，每一步都带有与 uid 无关的定位器（`role` + `name` + `nth`），用于后续确定性回放。

### 回放

```bash
# 探索并保存 flow（大模型自动挑选断言，每条断言都会先在录制时的页面上验证一遍）
webtest run --url http://127.0.0.1:8765/ --goal "..." --save flows/checkout.yaml
# 或从已有轨迹生成：webtest save runs/xxx.json flows/checkout.yaml [--no-llm]

webtest replay flows/*.yaml                      # 回放；找不到元素时调用大模型自愈
webtest replay flows/*.yaml --no-heal            # 纯确定性回放，完全不调用大模型
webtest replay flows/*.yaml --url https://staging.example.com/ --junit report.xml
```

flow 示例：

```yaml
steps:
- intent: 点击登录按钮
  tool: click
  args: { uid: $0 }              # $n 对应 targets[n]，回放时在最新快照中解析
  targets:
  - { role: button, name: 登录 }
assertions:
- element: { role: heading, name: 订单已提交 }
- text: 共 2 件，合计 ¥528
- absent_text: 用户名或密码错误
allowed_console_errors: [...]    # 录制时已存在的错误；回放时出现新的错误即判失败
allowed_failed_requests: [...]   # 同源请求只记录路径，配合 --url 使用时仍能匹配
```

判定规则：

- **PASS**：所有步骤执行成功，断言全部通过，没有新出现的 console 错误或失败请求。
- **PASS (healed)**：有元素定位被大模型修复。修复结果写入 `<name>.healed.yaml`，加 `--update` 则直接覆盖原文件。
- **FAIL**：元素找不到且无法修复、步骤报错、断言失败或出现新错误。断言失败和新错误**不会**被自愈，它们正是要抓的回归问题。

每个 flow 使用一个全新的浏览器（临时用户目录），用例之间不会互相影响。

### 自动探索

不给目标，只给测试数据，让 agent 自己摸清站点并批量生成用例：

```bash
webtest explore --url http://127.0.0.1:8765/ --context "测试账号 alice / 密码 secret123" \
  --max-tasks 8 --max-depth 2 --jobs 3 --out flows/explored
```

流程（广度优先）：

1. 在新浏览器里回放到达某个页面状态的前置步骤，取快照。
2. 大模型根据当前页面提出可执行的用户任务，包括正向任务和反向/校验任务；破坏性操作和缺少数据的任务会跳过，已知任务去重。
3. 每个任务用独立的浏览器并发执行：先回放前置步骤，再由 agent 完成任务。生成的 flow 由"前置步骤 + 任务步骤"组成。
4. **验证**：生成的 flow 立刻用不带大模型的方式回放一遍。通过的写入 `<out>/`，没通过的写入 `<out>/unverified/`。
5. 正向任务到达的新状态加入队列，继续向下探索。

页面状态按"URL 路径 + 标题和控件集合"做指纹，忽略件数、金额等动态文本。

输出：每个任务一个 flow，`explore.md`（Mermaid 状态图、结果表、每个任务的目标和结论），`explore.json`。

结果为 **needs review** 的任务，表示页面没有满足大模型提出的期望，需要人来判断是缺陷还是目标写得过细。

### 登录态复用

先登录一次，把 Cookie（包括 HttpOnly）和 localStorage 存成文件，之后每个浏览器启动时先注入它，就不用每个任务都从登录开始：

```bash
webtest login --flow flows/login.yaml --save-state auth/alice.json          # 回放已录好的登录用例（不调用大模型，适合 CI）
webtest login --url <url> --goal "用 alice / secret123 登录" --save-state auth/alice.json   # 由 agent 完成登录
webtest login --url <url> --manual --save-state auth/alice.json             # 弹出浏览器手动登录（适用于验证码、两步验证），完成后按回车

webtest --storage-state auth/alice.json replay flows/checkout_logged_in.yaml
webtest --storage-state auth/alice.json explore --url <url>                 # 探索直接从登录后的页面开始
```

- 用例 YAML 里可以写 `storage_state: auth/alice.json`。`replay` 时会自动加载；命令行的 `--storage-state` 优先级更高。探索生成的用例会自动带上这一项。
- Cookie 通过 Chrome DevTools 协议（`Storage.getCookies` / `setCookies`）读写。localStorage 只保存执行 `login` 结束时所在页面的那个站点。
- 状态文件里是有效的会话凭据：权限设为 600，并且 `auth/` 已加入 `.gitignore`。
- **自动刷新**：在用例里写 `login_flow: flows/login.yaml`，或者在命令行加 `--login-flow`，就会自动处理两种情况：
  - 状态文件不存在时，先回放登录用例生成它；
  - 用到登录态的用例失败时，先刷新登录态再重试一次。重试后通过的标为 **PASS (session refreshed)**；如果是真 bug，重试后仍然 FAIL。每个状态文件在一次运行中最多刷新一次。
- 如果没有配置登录用例，失败时会提示用 `webtest login` 手动刷新。
- 演示站点的会话只保存在服务端内存里，重启服务端后旧的状态文件就失效了。

### CI（GitHub Actions）

工作流文件放在 `ci/webtest.yml`。启用方法：复制到 `.github/workflows/webtest.yml` 并提交（这一步需要有 `workflow` 权限的账号）。启用后，它会在 push、PR、每天定时和手动触发时运行，包含三个 job：

| job | 内容 |
|---|---|
| `checks` | `cargo fmt --check`、`clippy -D warnings`、单元测试 |
| `e2e` | 框架自身的端到端测试（`cargo test -- --ignored`），使用 runner 自带的 Chrome |
| `regression` | 运行 `scripts/ci.sh`：启动演示站点 → 回放 `flows/*.yaml` 和 `flows/explored/*.yaml` → 输出 JUnit 和 Markdown 报告（摘要显示在 Actions 运行页面上），并上传报告 |

- 回放默认不调用大模型（`--no-heal`）。在仓库 Secrets 里配置了 `ANTHROPIC_API_KEY` 时，会开启自愈。
- 登录态通过 `login_flow` 自动生成，不需要把任何凭据提交到仓库。
- 本地复现：运行 `scripts/ci.sh`，报告输出在 `reports/`，失败时的页面快照也会复制到这里。用 `WEBTEST_FLOWS` 可以指定要回放的用例。
- 测自己的站点：把 `ci.sh` 里启动演示站点的那一段换成你的部署地址，或者在回放时加 `--url https://staging...`。

### 大模型后端

探索、录制和自愈都需要大模型。用哪个后端由环境变量决定，配置好后可以用 `webtest check-llm` 验证：

| `WEBTEST_LLM_PROVIDER` | 协议 | 默认接口地址 | 密钥 | 默认模型 |
|---|---|---|---|---|
| （不设置，且没有任何密钥） | 本机 `claude -p` | – | Claude Code 的登录 | claude-sonnet-5-5 |
| `anthropic`（设置了 `ANTHROPIC_API_KEY` 或 `ANTHROPIC_AUTH_TOKEN` 时自动选用） | Anthropic Messages | `ANTHROPIC_BASE_URL`，否则为 api.anthropic.com | `ANTHROPIC_API_KEY`（通过 x-api-key 头发送）或 `ANTHROPIC_AUTH_TOKEN`（通过 Bearer 发送） | claude-sonnet-5-5 |
| `deepseek` | OpenAI Chat Completions | api.deepseek.com | `DEEPSEEK_API_KEY` | deepseek-chat |
| `openai` | OpenAI Chat Completions | `OPENAI_BASE_URL`，否则为 api.openai.com/v1 | `OPENAI_API_KEY` | 无，必须指定 |

- `WEBTEST_LLM_BASE_URL` 和 `WEBTEST_LLM_API_KEY` 可以覆盖上表中的默认值。
- 模型的优先级：`--model` 参数 > `WEBTEST_MODEL` > 上表中的默认模型。
- 每次调用的超时由 `WEBTEST_LLM_TIMEOUT_SECS` 控制，默认 180 秒，超时或出错会重试 2 次。

示例：

```bash
WEBTEST_LLM_PROVIDER=deepseek DEEPSEEK_API_KEY=sk-... webtest check-llm
ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic ANTHROPIC_AUTH_TOKEN=sk-... WEBTEST_MODEL=deepseek-chat webtest check-llm
WEBTEST_LLM_PROVIDER=openai WEBTEST_LLM_BASE_URL=http://localhost:11434/v1 WEBTEST_LLM_API_KEY=x webtest --model qwen3 check-llm
```

### 在 Claude Code 里直接使用

仓库里带了一个技能文件 `.claude/skills/webtest/SKILL.md`。在本仓库目录下打开 Claude Code，直接说"测一下 https://… ，账号 …"即可。Claude 会按技能里的流程操作：准备环境 → 登录 → 探索或录制 → 回放验证 → 如实汇报，并遵守其中的安全规则。

### 从其他 harness 派发

见 `examples/dispatch/`。有三种模式：
- `replay`：直接调用命令行回放，不调用大模型；
- `explore`：由 webtest 直接探索，可以用 DeepSeek 等任意后端；
- `agent`：交给 Claude Code 执行，用 claude-step-relay 追踪进度，并输出结构化结果。

### 测试

```bash
cargo test                                   # 单元测试
cargo test -p webtest -- --ignored           # 端到端：通过、改名、金额 bug、新 console 错误、登录态复用、会话过期自动刷新、自动创建登录态
```

### 其他参数

`run` 的常用参数：`--max-steps`、`--model`（默认 `claude-sonnet-5-5`）、`--deny 删除,delete`（拒绝操作含这些关键词的元素）、`--headed`、`--out`。

调试：`webtest repl` 从标准输入读取 `<工具名> <JSON参数>`，逐行调用并打印结果。
