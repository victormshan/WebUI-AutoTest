# OpenCode 录制与无模型回放验收

日期：2026-10-16。结果：PASS。

## 实际执行的链路

OpenCode 正常 MCP 工具会话 → chrome-devtools-mcp@1.10.1 → Chrome，录制真实 fill/click 调用及前后的无障碍快照。测试脚本将这些日志转换为项目 Trace，定位器来自实际快照的 role/name/nth；不是手写假动作或重放模型结论。随后通过项目现有 `webtest save --no-llm` 执行 Flow::from_trace，生成 recorded.yaml，再使用 `webtest replay --no-heal` 在新浏览器中回放。

这验证了 OpenCode 实际操作日志经一次性适配可进入现有保存/回放流程；没有新增产品级录制命令或自动接入流程。新建的 rust_browser Rust MCP 服务仍不支持直接录制 flow，且本次回放使用旧 chrome-devtools-mcp 后端，不代表 Rust Runtime 的原生回放已经实现。

## 测试结果

- 页面：本地临时 HTTP fixture，带姓名输入框、提交按钮和服务器结果。
- 姓名：OpenCode Replay Demo。
- 录制：2 个操作（fill、click），均成功；实际定位器为 textbox / Your name 和 button / Submit name。
- 自动保存：2 个断言，分别是 heading / Record replay example 和 heading / Submission accepted。
- 正常回放：PASS，2/2 steps，回放器报告 1.3 秒，退出码 0。
- 故意失败对照：服务器返回 Submission rejected，回放操作仍完成 2/2，但在 Submission accepted 断言处失败，退出码 1，符合预期。
- 回放阶段模型调用：0。使用 --no-heal，并把模型命令改成一旦调用就写错误标记并失败的检测脚本；未触发。
- 服务器记录：录制、正常回放、失败对照各收到一次正确姓名，共 3 次。最后一次故意切换为失败结果。
- 回执随机值不加入稳定断言。当前 flow 没有姓名文本断言；正确姓名由服务器记录独立核实，不要扩大宣称 flow 已自动生成该断言。

## 首次尝试的失败记录

曾尝试将 OpenCode 用作 webtest command LLM 的纯文本决策后端；当前模型服务返回 403 FreeTierError，该次尝试没有生成 flow。未修改认证头或绕过服务限制。改用 OpenCode 正常 MCP 工具会话后，录制成功。首次失败日志保留在证据目录的上一级。

## 证据与复现

WSL 证据目录：`/mnt/e/rust-agent-browser/reviews/opencode-integration/record-replay-20261016-195549/standard-tools`。
Windows 同一路径：`E:\rust-agent-browser\reviews\opencode-integration\record-replay-20261016-195549\standard-tools`。

- `opencode-record-events.jsonl`：真实 OpenCode 工具调用及最终回答。
- `traces/opencode-recorded.json`：转换后的项目 Trace；diagnostics 标记 not_recorded，未伪造完整诊断。
- `recorded.yaml`：webtest save 实际生成的 flow。
- `replay.xml`、`replay.md`：正常回放报告。
- `negative.xml`、`negative.md`：故意失败报告。
- `result.json`、`server-submissions.json`：综合判断和独立服务器记录。
- `run_example.py`：包含本地 fixture、日志转换和两种回放的完整测试脚本；不属于项目正式功能。

本地 fixture 已关闭，recorded.yaml 中端口是当次临时端口，不能在没有测试站点时直接回放。若需要复现，运行该目录的 run_example.py，会重新调用模型、创建站点、录制并回放；注意它会更新该目录内的证据文件，保留历史时应先复制到新目录。

本次未修改浏览器或回放实现，也未修改持久 OpenCode 配置。MCP 配置仅通过测试进程的 OPENCODE_CONFIG_CONTENT 临时注入。未为此次后续测试创建 Git commit。
