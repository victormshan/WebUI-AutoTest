# B1：webtest 第二后端（NativeBackend）任务书（草案）

状态：**草案，待批准**（按约定，ChatGPT 同意即视为用户同意）。依据 ADR-0、A1 报告（A 线暂停，pipe Runtime 转给 B 线）。

## 1. Purpose
让 webtest 在现有的 `McpBackend`（Rust → MCP → Node chrome-devtools-mcp → CDP）之外，多一个 `NativeBackend`（Rust → browser-runtime → CDP pipe）。在同一批用例上对比两者，**用数据决定**要不要迁移默认后端。允许的结论是"维持双后端"或者"不迁移"。

## 2. 现状（已核实）
- agent 对浏览器的依赖面很窄：`launch`、`call(工具名, 参数)`、`navigate`、`snapshot`、`tools`、`storage_state` / `apply_storage_state`、`close`。实际用到约 13 个工具：navigate_page、take_snapshot、click、fill、fill_form、hover、press_key、wait_for、evaluate_script、list_pages、list_console_messages、list_network_requests、handle_dialog。
- **关键耦合**：agent 解析 chrome-devtools-mcp 的**快照文本格式**（例如 `uid=2_6 button "加入购物车" disableable disabled`，见 `crates/agent/src/snapshot.rs`）。flow 里存的是与 uid 无关的定位器（`role + name + nth`）。
- **附带发现**：现在的 mcp-client 为了读写 Cookie，让 chrome-devtools-mcp 用 `--remote-debugging-port=0` 打开了**调试端口**（`crates/mcp-client/src/lib.rs:71`）。测试用的是临时 profile，风险较低；但 NativeBackend 用 pipe，可以顺带去掉这个端口。

## 3. Scope
1. 在 `crates/agent` 里抽出最小的后端接口（trait），方法就是上面那几个；`McpBackend` 包装现有实现，**行为不变**。
2. `NativeBackend`（只做现有工具的等价能力）：
   - **快照**：`Accessibility.getFullAXTree`（按 frame）→ 生成**与现有格式兼容**的文本；uid 映射到本次快照的 `backendNodeId`，**只在这一代快照内有效**，失效时报明确错误（ChatGPT 建议的三层身份：flow 定位器 / 一次操作的节点引用 / 重新采集后再语义解析）。
   - **交互**：click、hover 先滚动到可见区域，再用 `DOM.getNodeForLocation` 检查遮挡，然后发真实鼠标事件；fill 和 fill_form 先聚焦再 `Input.insertText`；press_key 用 `Input.dispatchKeyEvent`。
   - **观察**：wait_for（条件等待，不用固定 sleep）；evaluate_script；list_pages；console（Log、Runtime 的 console 事件）；网络失败列表（Network 域）；对话框（`Page.javascriptDialogOpening` / `handleJavaScriptDialog`）。
   - **登录态**：用 `Storage.getCookies` / `setCookies` 加 localStorage，格式沿用现有的 `StorageState`。
   - 每个 flow 一个临时 profile，headless，可以并发（每个 flow 一个 Chrome）。
3. CLI 增加 `--backend mcp|native`，默认 `mcp`。

## 4. Non-goals
不改 flow 格式、断言、报告、agent 主循环和自愈逻辑；不做 MCP Gateway、Agent Runtime、调度；不做任何第三方站点自动化（只测用户自己的站点和 fixtures）；不追求覆盖 chrome-devtools-mcp 的全部工具。

## 5. 对比方法（验收的核心）
- 用例：`fixtures/shop` 的 `flows/*.yaml` 和 `flows/explored/*.yaml`，加上 `make_variants.py` 生成的 v2-renamed、v3-bug、v4-console 三个变体。
- 两个后端各跑 N 轮（N 先定为 5），指标：
  - 通过率；
  - 判定是否一致：同一个用例在两个后端上，PASS / FAIL / healed 是否相同；
  - 自愈次数；
  - 端到端耗时（中位数和 P95）；
  - 新增 console 错误和失败请求的识别是否一致。
- **必须一致的部分**：v3-bug 必须 FAIL、v4-console 必须识别出新错误，任何后端都不能把真缺陷判成 PASS。
- 结果写成对比表，交给用户和 ChatGPT 判断。

## 6. Tests
- L0、L1：快照文本生成（AX 节点 → 行格式）、uid 失效、遮挡判定、错误分类；
- L2：真实 Chrome 跑 fixtures（`cargo test -- --ignored`）；
- L3：导航中途点击、元素消失、对话框阻塞、超时；
- 所有关键修复都做变异验证。
- L4 留到 B2。

## 7. Acceptance
`McpBackend` 的全部现有测试和 CI 保持全绿；`NativeBackend` 在第 5 节的对比中拿出完整数据；有任何真缺陷被判成 PASS，B1 就不通过。

## 8. Exit Decision
看对比数据，由用户和 ChatGPT 决定：迁移默认后端 / 维持双后端 / 放弃 NativeBackend。
