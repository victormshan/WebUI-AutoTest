# webtest — 基于 chrome-devtools-mcp 的 Rust 网页测试环境

Rust 通过 MCP（`rmcp`）驱动 [chrome-devtools-mcp](https://github.com/ChromeDevTools/chrome-devtools-mcp)，
由 Claude 读取页面无障碍树、自主决定操作，完成自然语言描述的测试目标，并记录可回放的运行轨迹。

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
| `crates/agent` | 观察→决策→执行循环、快照压缩、语义定位器、安全拦截、自动诊断 |
| `crates/cli` | `webtest` 命令行 |
| `fixtures/shop` | 演示站点（登录 / 加购 / 结算，内置一个 console 错误和一个 404） |

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

常用参数：`--max-steps`、`--model`（默认 `claude-sonnet-5-5`）、`--deny 删除,delete`（拒绝操作含这些关键词的元素）、`--headed`、`--out`。

调试：`webtest repl` 从标准输入读取 `<工具名> <JSON参数>`，逐行调用并打印结果。
