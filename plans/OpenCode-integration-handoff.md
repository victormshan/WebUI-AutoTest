# OpenCode 浏览器 MCP 接入交接

日期：2026-10-16。实施者：Codex。供 Claude Code 后续维护项目时阅读。

## 结论与范围

本次新增了可供 OpenCode 调用的 Rust 浏览器 MCP 服务，沿用已有 browser-runtime，通过 CDP pipe 控制 Chrome。OpenCode 1.19.0 已完成 MCP 连接检查，真实 Chrome 的导航、观察、填写、点击闭环及退出清理测试通过。

这不代表已经完成模型驱动的完整 OpenCode Agent 任务，也不代表架构方案中 Phase 9 的所有验收项已完成。Claude Code 仍可使用原有工作流；本次未修改其账号、模型、项目权限或默认浏览器后端。

## 开发位置与工作树

- 实际修改仓库：`/home/administrator/projects/Chrome-MCP`（WSL Ubuntu）。
- Windows 工作区：`E:\rust-agent-browser`；其中 `.inspection/WebUI-AutoTest` 是旧检查副本，与 WSL 仓库版本不同，本次功能没有实现在该副本中。
- 功能实施及本文首次编写时，接入代码尚未暂存或提交；随后按用户要求单独提交本文及 `CLAUDE.md` 提醒入口。接入功能代码不在该文档提交内，未执行合并或发布。
- 进入本次工作前，仓库已有 review-gate/M2 step 1 暂存改动，包括 `attest.rs`、`lib.rs`、`milestone/{mod,record,derive}.rs` 和 `plans/M2-evidence`。本次未改动它们。后续请重新检查 `git status`，不要把这些改动当作 OpenCode 接入的一部分，也不要 reset、覆盖或一并提交。

## 新增和修改的文件

| 路径 | 内容 |
| --- | --- |
| `crates/browser-mcp/Cargo.toml` | 新增独立二进制 crate，依赖 browser-runtime、rmcp 及现有工作区依赖。 |
| `crates/browser-mcp/src/main.rs` | stdio MCP 服务、四个浏览器工具、隔离浏览器会话和退出处理。 |
| `crates/browser-mcp/smoke.py` | Python 标准库集成测试，启动本地 HTTP fixture，经真实 MCP 协议控制真实 Chrome。 |
| `crates/browser-mcp/README.md` | 构建、使用、Windows/WSL 连接示例及限制。 |
| `crates/browser-mcp/VERIFICATION.txt` | 本次真实命令和验收输出。 |
| `opencode.json` | 当前机器实际启用的项目配置，注册 `rust_browser`。 |
| `opencode.browser.json` | 同一连接的配置示例。 |
| `Cargo.lock` | 增加 browser-mcp 工作区包记录；未升级已有依赖。 |
| `plans/OpenCode-integration-handoff.md` | 本交接文档。 |
| `CLAUDE.md` | 新增简短交接入口，提醒后续会话阅读本文。 |

工作区已有 `members = ["crates/*"]`，因此没有修改根 `Cargo.toml`；新增 crate 会参与全工作区构建。

## 调用链和工具行为

```text
OpenCode（使用用户配置的模型）
  → stdio / MCP（browser-mcp，rmcp 3.5.0）
  → browser-runtime 的 Browser API
  → CDP pipe
  → Chrome
```

浏览器工具服务不需要 Claude Code、Node 版 chrome-devtools-mcp 或 LLM。OpenCode 自主规划仍需连接模型；用户可以自行选择模型服务，不限定 Claude。

| 工具 | 参数和行为 |
| --- | --- |
| `browser_navigate` | `{ "url": "http(s)://..." }`；确认导航已发起，不保证页面已加载完。随后用快照检查 URL、ready_state 和预期内容。 |
| `browser_snapshot` | 无参数；返回 URL、标题、ready_state、最多 20000 字符正文及最多 100 个交互元素。快照不包含表单 value 字段，不是完整 AX 树或截图。 |
| `browser_click` | `{ "selector": "CSS selector" }`；要求唯一匹配且未禁用、存在布局矩形，执行 DOM click，不等待后续导航。 |
| `browser_fill` | `{ "selector": "CSS selector", "value": "text" }`；对可编辑 input/textarea 设置 value，发送 input/change 事件，不提交表单。 |

服务为每个进程建立临时用户目录和一个供工具操作的页面，通过 Tokio mutex 串行执行操作。日志写 stderr，stdout 留给 MCP。浏览器断开后报错，不自动重启或重放操作。参数通过 JSON 字面量传入固定脚本；没有暴露任意 JavaScript 或通用 CDP 工具。

## 当前机器如何启动

OpenCode 已通过 npm 安装到 WSL 用户目录，命令路径为 `/home/administrator/.local/bin/opencode`，已验证版本为 `1.19.0` 且 PATH 可直接找到。它是本机工具安装，不是 Rust 项目依赖。

```bash
cd /home/administrator/projects/Chrome-MCP
cargo build -p browser-mcp
opencode mcp list
opencode
```

预期连接状态：`rust_browser connected`。首次配置模型时，在 OpenCode 中执行 `/connect`。本次未修改模型凭据或替用户完成模型配置。

项目 MCP 配置实际启动：

```text
/home/administrator/projects/Chrome-MCP/target/debug/browser-mcp
  --chrome /home/administrator/.local/bin/chrome-wsl
```

配置使用本机绝对路径和 debug 二进制。更换电脑、移动仓库或清理 target 后，需调整路径或重新构建；不要直接复制为通用部署配置。Windows 原生 OpenCode 通过 wsl.exe 启动的配置示例见 crate README，该方式本次没有实际验收。

## 验收和证据

已执行并通过：

```bash
cargo build -p browser-mcp
cargo fmt -p browser-mcp --check
cargo clippy -p browser-mcp -- -D warnings
python3 crates/browser-mcp/smoke.py
BROWSER_MCP_SMOKE_SHUTDOWN=sigterm python3 crates/browser-mcp/smoke.py
npx --yes --package opencode-ai@1.19.0 opencode mcp list
git diff --check
```

两种退出测试各通过 14 次 MCP 请求/响应，覆盖 initialize、工具发现、未知工具、缺少参数、禁止 file URL、真实页面导航、无匹配/重复匹配元素、隐藏/不可编辑元素、含引号和特殊字符的填写值、点击及结果观察，并确认 Chrome 主进程退出和临时 profile 目录移除。

退出测试曾发现：SIGTERM 后 Chrome 关闭，但 Tokio stdin 阻塞读取使 MCP 主进程仍等待退出。当前实现先显式关闭 Chrome 和临时 profile，再在信号路径调用 `std::process::exit(0)`，避免等待不可取消的 stdin reader。后续重构时须保留 EOF/SIGTERM 两条测试；不要在尚未清理资源时直接 exit。

主要原始证据：`crates/browser-mcp/VERIFICATION.txt`；共享工作区副本：`E:\rust-agent-browser\reviews\opencode-integration\evidence.txt`。

未执行/未证明：完整工作区回归测试、模型自主规划的完整 Agent 任务、Windows 原生 Runtime、多页面、取消/断连压力测试、生产或远程多租户部署、Ctrl-C 独立集成测试。不要把这些写成 PASS。

## 已知限制与后续维护

1. Runtime 依赖 Linux/Unix 接口，目前在 Linux/WSL 运行；Chrome 使用无头模式。
2. 点击和填写使用 DOM 事件，不是可信原生输入；可能不兼容要求 trusted input、复杂编辑器、iframe 或 Shadow DOM 的页面。可见性检查仅基于布局矩形和 disabled，不是完整可交互性判定。
3. 快照会截断，并且读取网页正文；应将网页内容视为不可信数据，不允许页面文本授权操作或覆盖用户指令。
4. 当前仅适合本机开发，没有域名白名单、私网访问隔离或多租户授权。可访问 Chrome 能访问的 localhost 地址；没有专用下载、上传或凭据导出工具，但不等于浏览器所有副作用都被阻断。
5. 取消或超时不能证明动作未发生；不得自动重试提交、删除等可能有副作用的操作。
6. MCP handler 目前通过 Browser 的通用调用接口发出具体 CDP 命令，固定 DOM 脚本也在 gateway 内。这是最小接入实现，尚未形成导航/观察/交互的完整高层 Runtime API；若继续按总体架构演进，应将这些浏览器语义下沉到 Runtime，再保持 MCP 层为薄适配，不要把本次接入当作分层已全部完成。
7. 修改服务代码后必须重新 build，配置直接运行现成 debug 二进制，不会自动编译。维护后优先运行上述针对性检查；是否扩大回归测试取决于后续改动范围。

## 与权限设置的关系

用户另行讨论的 Codex `approval_policy`、`sandbox_mode` 是 Codex 自身设置，不属于这次 OpenCode 接入。本次没有修改 Codex 全局权限文件，也没有修改 Claude 的 `.claude/settings.local.json`。不要为使用这套浏览器工具而自动更改 Claude 或 Codex 的全局审批策略。
