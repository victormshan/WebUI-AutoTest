# B1：webtest 第二后端（NativeBackend）任务书 — 修订版 r2

- 状态：**修订稿，待 ChatGPT 评审**。评审通过前**不开始大规模实现**。
- 修订依据：ChatGPT 对 r1 的评审意见（2026-10-12，用户转达）：原则同意，修订后再开始。
- 依据文档：ADR-0；A1 报告（A1 = PASS WITH CONDITIONS；A 线暂停，pipe Runtime 转给 B 线）。

## 0. 本次修订对照 ChatGPT 意见
| ChatGPT 要求 | 本稿位置 |
|---|---|
| 最小后端接口；`McpBackend` 行为不变；`NativeBackend` 默认不启用 | §2、§3.1 |
| 先盘点快照/uid 依赖，定义最小快照契约和适配层 | §2（已盘点）、§3.2 |
| 快照语义兼容（角色、名称、层级、可定位性、隐藏节点、交互状态），uid 按代次失效 | §3.2、§5.1 |
| Cookie / 调试端口安全审计 | §4（已完成初步审计） |
| 对照测试 + 故障注入，真缺陷绝不判成 PASS | §5.2、§5.3 |
| 区分断言失败、执行错误、超时、结果未知；不无条件重试有副作用的操作 | §3.4 |
| 结果不一致必须调查；NOT RUN/BLOCKED/失败/证据不足都不算 PASS | §5.4 |
| 审核通道边界不变 | §7 |
| 交付物与验收 | §6 |
| 需要扩大范围先提 ADR | §8 |

## 1. Purpose
在 webtest 现有后端 `McpBackend`（Rust → MCP → Node chrome-devtools-mcp → CDP）之外，增加一个 `NativeBackend`（Rust → browser-runtime → CDP **pipe**）。在同一批用例上对照，**用数据决定**要不要迁移。允许的结论包括"维持双后端"和"放弃 NativeBackend"。

## 2. 依赖盘点（已完成，只读代码）
agent 对浏览器的依赖分成 7 类：

| # | 依赖 | 位置 | 对 NativeBackend 的要求 |
|---|---|---|---|
| D1 | **快照文本语法**：每个节点一行，`<2 空格 × 深度>uid=<id> <role> "<name>" <attrs>`；根节点为 `RootWebArea "<title>" url="..."`；属性如 `focused`、`disableable disabled` | `snapshot.rs` 的 `parse_line`、`merged`（合并 StaticText、丢弃 LineBreak） | 契约 C1（§3.2） |
| D2 | **在快照上的语义操作**：`compact`（发给 LLM）、`locator_for` / `resolve`（uid ↔ `role+name+nth`）、`describe`、`has_element`、`contains_text`、`landmarks`、`fingerprint`（探索去重）、`label` | `snapshot.rs`；调用方是 `lib.rs`、`replay.rs`、`flow.rs`、`explore.rs`、`crossreview.rs` | 契约 C2：同一页面在两个后端上，这些函数的结果要一致（§5.1） |
| D3 | **uid**：不透明字符串，只用来回传给工具；回放时从新快照里重新解析 | `lib.rs:233`，`replay.rs:154-223` | 契约 C3：uid 只在本代快照内有效 |
| D4 | **工具目录**：`tools()` 按 `ACTION_TOOLS` 过滤后，把名称和参数 schema 写进 LLM 的系统提示 | `lib.rs:147-154`、`system_prompt` | 契约 C4：工具名和参数 schema 与现有一致（LLM 提示不变） |
| D5 | **工具输出**：`ToolOutput { text, images, is_error }`；导航失败看 `is_error` | `lib.rs:124,263`，`replay.rs:90-200` | C4 的一部分 |
| D6 | **诊断文本格式**：console 取以 `msgid=` 开头、含 ` [error] ` 的行；网络取以 `reqid=` 开头、HTTP 失败的行 | `lib.rs:295-315` | 契约 C5：输出同格式，或者把这部分解析移进后端适配层（见 §3.1） |
| D7 | **登录态**：`storage_state` / `apply_storage_state`（Cookie 含 HttpOnly + localStorage） | `mcp-client/src/lib.rs:168-230` | 契约 C6（§4） |

## 3. Scope

### 3.1 最小后端接口
- 在 `crates/agent` 里定义 trait（暂名 `BrowserBackend`），方法正好是现有 agent 用到的那些：`call(tool, args) -> ToolOutput`、`tools()`、`navigate`、`snapshot`、`storage_state`、`apply_storage_state`、`close`，再加 `diagnostics()`。
- **`diagnostics()` 是唯一的结构性改动**：把 D6 的行解析从 agent 移到后端适配层，返回结构化的 `{console_errors, failed_requests}`。`McpBackend` 内部沿用现在的解析方式，行为不变，由现有测试守住。这样做的理由：避免 NativeBackend 去模仿 chrome-devtools-mcp 的文本格式。
- `McpBackend` = 现有 `mcp_client::Browser` 的包装，**行为不变**，**默认后端**。
- `NativeBackend` 只能通过 CLI `--backend native` 显式启用，**默认不启用**，也不移除 `McpBackend`。

### 3.2 快照契约（C1–C3）与适配
- NativeBackend 用 `Accessibility.getFullAXTree`（主文档和同源 iframe；跨域 iframe 列为已知限制，B1 不做）生成**符合 C1 语法**的文本。
- **C2 语义一致**，对象是同一个本地测试页面（第 5.1 节的契约测试页），逐项比对：

  | 维度 | 要求 |
  |---|---|
  | 角色 | 一致 |
  | 名称 | 一致 |
  | 层级 | 父子关系一致 |
  | 可定位性 | `resolve` 对每个 `role+name+nth` 得到对应的元素 |
  | 隐藏节点 | `display:none`、`aria-hidden`、`visibility:hidden` 的元素不出现在快照里 |
  | 交互状态 | disabled、focused、checked、expanded 一致 |

  **文本相同不等于语义兼容**，所以以契约测试为准。
- **C3 uid 代次**：uid 编码代次（例如 `<gen>_<n>`），映射到本代快照的 `backendNodeId`。只要发生导航、重新取快照或执行上下文变化，旧代 uid 一律失效：调用时报 `StaleUid`，交给现有的语义重定位逻辑（agent 的回放已经是"新快照 + resolve"）处理，**不会把旧 uid 静默映射到新节点**。

### 3.3 工具（C4，只做现有工具的等价能力）
- navigate_page；take_snapshot；
- click、hover：先滚动到可见区域，再用 `DOM.getNodeForLocation` 检查遮挡，遮挡时报"不可交互"，然后发真实鼠标事件；
- fill、fill_form：先聚焦，再 `Input.insertText`；
- press_key：`Input.dispatchKeyEvent`；
- wait_for：条件等待，不用固定 sleep；
- evaluate_script；list_pages；
- console、network：Runtime 和 Log 的 console 事件，Network 的失败请求；
- handle_dialog：`Page.javascriptDialogOpening` / `handleJavaScriptDialog`。

工具名和参数 schema 与 chrome-devtools-mcp 一致，以 `webtest tools` 的实际输出为准。

### 3.4 结果分类与重试（按 ChatGPT 意见）
- 每个工具调用的结果分为五类：**成功 / 业务断言失败 / 执行错误（含 StaleUid、不可交互）/ 超时（结果未知）/ 浏览器断连**。
- 超时和断连之后，执行状态未知：**不自动重试有副作用的工具**（click、fill、fill_form、press_key、handle_dialog、带写操作的 evaluate_script）；只读工具（snapshot、list_*、wait_for）允许一次有界重试。
- 用例的最终判定沿用 webtest 现有规则；新增的"结果未知"一律判为非 PASS。

### 3.5 每个 flow 一个临时 profile、无头、可以并发。

## 4. Cookie / 调试端口安全审计（初步结论，已核实）
| 项 | 事实 |
|---|---|
| 为什么开端口 | chrome-devtools-mcp 当前的 30 个工具里**没有 Cookie 读写工具**，HttpOnly Cookie 只能走 CDP（`webtest tools` 实测；只有 `get_network_request` 能看到请求头）。所以 mcp-client 给 Chrome 加了 `--remote-debugging-port=0` |
| 绑定地址 | Chrome 默认绑定 `127.0.0.1`，端口随机（写在 `<profile>/DevToolsActivePort`），用 `ws://127.0.0.1:<port>/...` 连接 |
| 启停时机 | **每个 webtest 浏览器都会开**，不只是用到 Cookie 的时候；在整个浏览器运行期间都开着；浏览器关闭时消失。profile 是临时目录 |
| 访问边界 | CDP 没有鉴权。**本机任何用户**都能在运行期间连上并完全控制这个测试浏览器。**实测：WSL2 默认把 127.0.0.1 端口转发给 Windows，Windows 进程也能连上**。网页本身不能连（Chrome 校验 WebSocket 的 Origin，没设 `--remote-allow-origins`） |
| 暴露的资产 | 测试账号的会话 Cookie、对测试浏览器的控制权。不是用户的个人 profile |
| 日志 | Cookie 只写进 `auth/*.json`（权限 600，已在 .gitignore 里），不进日志 |

- **结论**：没有暴露到不受信任的网络，但违反了"本机用户隔离"的最小权限原则，属于**低风险、需要整改**。
- **B1 的处理**：
  - NativeBackend 用 pipe，**天然没有端口**，C6 直接用 `Storage.getCookies` / `setCookies`；
  - McpBackend 保持现状（它是默认后端，行为不变），在 README 里写明这一风险；
  - 整改条件：如果 chrome-devtools-mcp 以后提供 Cookie 工具，或者支持以 pipe 方式启动（Puppeteer 支持 `pipe: true`，chrome-devtools-mcp 是否暴露这个选项**需验证**），就把端口去掉；
  - 在那之前，**禁止**把 McpBackend 部署到多用户或共享主机上，作为部署约束写进文档。

## 5. 测试与验收证据

### 5.1 快照契约测试（L1/L2）
新增一个本地契约页面 `fixtures/contract/`，覆盖：各种角色、无名称和重名元素、嵌套层级、隐藏节点（三种方式）、disabled/checked/expanded/focused、同源 iframe、动态插入和删除、导航后旧 uid 失效。两个后端各取一次快照，按 C2 逐项比较；在这两份快照上分别跑 agent 的 `locator_for`、`resolve`、`has_element`、`contains_text`、`fingerprint`，结果必须一致。

### 5.2 对照测试（L2）
- 用例：`fixtures/shop` 的 `flows/*.yaml` 和 `flows/explored/*.yaml`，加上 `make_variants.py` 生成的 `v2-renamed`、`v3-bug`、`v4-console`；
- 两个后端使用**相同**的数据、流程和断言，各跑 5 轮；
- 指标：通过率、判定是否一致、自愈次数、耗时（中位数和 P95）、新增 console 错误和失败请求的识别是否一致。

### 5.3 故障注入（L3，强制）
每一项都必须证明**不会判成 PASS**，并且归入正确的类别：

| 场景 | 期望 |
|---|---|
| 已知正确结果（基线） | PASS |
| 金额或文本错误（v3-bug） | 断言失败 → FAIL |
| 定位失效（v2-renamed） | 自愈成功记为 PASS (healed)；自愈不了 → FAIL |
| 新增 console 错误（v4-console） | FAIL |
| 操作中途页面导航 | StaleUid → 重定位或 FAIL，不得误点 |
| 快照异常（AX 树为空或截断） | 执行错误 → FAIL |
| CDP 断连（kill Chrome） | 断连 → 结果未知 → FAIL |
| 超时（页面挂起） | 超时 → 结果未知 → FAIL |

关键修复都要做变异验证（故意改回错误实现，确认对应用例会变红）。

### 5.4 统计规则
- NOT RUN、BLOCKED、执行失败、结果未知、证据不足，**都不计为 PASS**；
- 两个后端判定不一致时，必须逐例调查并记录原因；**不得**拿对自己有利的那一方作为通过依据。

## 6. 交付物
- 代码 diff（按 review-gate 三方流程逐版本提交）；
- 测试命令、环境信息、原始输出、未执行项清单；
- 快照契约测试结果、对照数据表、故障注入证据；
- 端口审计结论（§4，实施时复核）；
- B1 阶段的交流文档（给 ChatGPT）。

## 7. 审核通道边界（不变）
- Gemini API 仍是审核主通道，对 503/429 做有限重试（`bea0a22`）；
- 重试用完后回退到 DSH 扩展通道 web-gemini，**强度如实记为 3**，记录里写明回退原因；
- **不恢复 A2/A3**；B1 的 NativeBackend 只测试用户自己的站点和本地 fixtures，**不授权任何第三方网页自动化**；
- 不绕过验证码、异常流量页面或任何访问限制。

## 8. Non-goals 与变更控制
- 不重写 agent、flow、断言、报告、自愈和探索逻辑；`diagnostics()` 的移动是唯一的结构性改动；
- 不引入队列、Cron、持久化调度、MCP Gateway；跨域 iframe 和 L4 压力测试推到 B2；
- **McpBackend 一直是默认后端**，迁移要等用户根据验收报告另行批准；
- 实施中如果发现需要扩大范围的架构问题，**先提 ADR**，不自行扩展。

## 9. Exit Decision
根据 §5 的数据，由用户和 ChatGPT 决定：迁移默认后端 / 维持双后端 / 放弃 NativeBackend。
