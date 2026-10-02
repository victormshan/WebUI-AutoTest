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
| `fixtures/shop` | 演示站点（登录 / 加购 / 结算，内置一个 console 错误和一个 404）及其变体：`v2-renamed`（按钮改名）、`v3-bug`（金额算错）、`v4-console`（新增 console 错误） |
| `flows/` | 测试用例 YAML |

## 环境准备（WSL，无需 sudo）

```bash
scripts/setup-wsl.sh        # Rust + zig 链接器 + Chrome for Testing（装在 ~/.local 下）
cargo build --release
```

`webtest` 默认使用 `~/.local/bin/chrome-wsl`，也可用 `--chrome` 或 `WEBTEST_CHROME` 指定。

## 用法

```bash
# 演示站点
(cd fixtures/shop && python3 -m http.server 8765 &)

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

### 测试

```bash
cargo test                                   # 单元测试
cargo test -p webtest -- --ignored           # 端到端：用 fixtures 验证通过、改名、金额 bug、新 console 错误四种情况
```

### 其他参数

`run` 的常用参数：`--max-steps`、`--model`（默认 `claude-sonnet-5-5`）、`--deny 删除,delete`（拒绝操作含这些关键词的元素）、`--headed`、`--out`。

调试：`webtest repl` 从标准输入读取 `<工具名> <JSON参数>`，逐行调用并打印结果。
