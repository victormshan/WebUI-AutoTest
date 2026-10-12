# B1：webtest 第二后端（NativeBackend）任务书 — 修订版 r3

- 状态：**ChatGPT 评审 r2 结论 APPROVE WITH CONDITIONS（2026-10-12，用户转达）**。r3 落实其中的条件。批准范围是**受控开发和分阶段验收**（§5.6），**不是**迁移批准，也不批准把 NativeBackend 设为默认后端。
- 修订依据：ChatGPT 对 r1 的评审意见（原则同意，修订后再开始）；ChatGPT 对 r2 的评审意见（有条件通过，见 §0.1）。
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

### 0.1 r3：落实 ChatGPT 对 r2 的条件
| ChatGPT 条件 | 本稿位置 |
|---|---|
| `diagnostics()` 区分"确实没有错误"和"没读到"：失败、不支持、部分结果都要显式标出，绝不能当成"没有错误" | §3.1 |
| 快照契约比较节点语义而不是文本：AX→DOM 映射、隐藏节点按实测比较、旧 uid 不得指向新节点 | §3.2 |
| 工具 schema 一致不等于行为一致；做不到的能力不得空实现或伪成功 | §3.3 |
| 五类结果在报告里保持区分；状态未知时可以核实状态，但不得重放原操作 | §3.4 |
| profile 隔离、并发安全、资源清理要有证据 | §3.5 |
| §4 不再写"低风险"，改为基于环境约束的风险描述，约束可执行 | §4 |
| 补充测试：诊断失败、部分诊断、并发隔离、登录态往返、错误传播、资源清理 | §5.5 |
| 5 轮对照不能单独证明稳定性；变异测试要记录完整 | §5.2、§5.3 |
| 分五个阶段推进，每段有 gate；先交接口与 McpBackend 基线 | §5.6 |
| 审核强度如实记录、不满足就标未满足、关键路径另加独立复核 | §7 |

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
- **`diagnostics()` 是唯一的结构性改动**：把 D6 的行解析从 agent 移到后端适配层，返回结构化的 `{console_errors, failed_requests, console, network}`。这样做的理由：避免 NativeBackend 去模仿 chrome-devtools-mcp 的文本格式。
- **诊断的覆盖状态（r3）**：`console`、`network` 两个通道各带一个 `Coverage`，说明列表能说明什么：

  | Coverage | 含义 | 空列表是否等于"没有错误" |
  |---|---|---|
  | `complete` | 完整读取 | 是 |
  | `partial{reason}` | 读到了，但可能缺条目（例如工具只返回了一页） | 否 |
  | `unsupported` | 该后端读不了这个通道 | 否 |
  | `failed{reason}` | 读取失败或超时 | 否 |
  | `not_recorded` | 没读过（旧轨迹文件、尚未采集） | 否 |

  - **replay**（诊断参与 PASS 判定）：任何一个通道不是 `complete`，都记一条 `diagnostics incomplete: ...` 失败，用例判为非 PASS；
  - **run**（PASS 由大模型判定，诊断只是附带输出）：判定规则不变，打印 `diagnostics incomplete: ...`；
  - **录制 flow**：允许列表来自不完整诊断时记警告。允许列表偏短只会让回放多报错，不会漏报；
  - **McpBackend 的解析**：识别 chrome-devtools-mcp 的 `Showing a-b of N` 表头和两种空列表文案（`<no console messages found>`、`No requests found.`）。工具报错 → `failed`；表头显示不全或条目数对不上 → `partial`；**不认识的输出（例如服务端升级改了措辞）→ `failed`**，不当成空列表；
  - **`diagnostics()` 整体不会失败（阶段 1 收尾，按 ChatGPT 的阶段 1 审核意见）**：trait 签名是 `async fn diagnostics(&self) -> Diagnostics`，不返回 `Result`。底层调用返回 `Err`（传输错误、浏览器断连）时，只把**该通道**标为 `failed`，另一个通道照常读取；两个都读不了，两个都标 `failed`。这样任何后端都不能用一个 `Err` 绕过覆盖状态；
  - **浏览器中途重启（阶段 1 收尾时发现）**：Chrome 死掉后，chrome-devtools-mcp 会**静默启动一个新的 Chrome**，并且只在下一次工具调用的结果里提示一次（`structuredContent.reconnected` 和一行 `Note: the browser was restarted or reconnected ...`）。新浏览器的诊断列表是干净、完整的样子，但重启前的错误已经丢了。如果重启发生在断言通过之后、读诊断之前，replay 会**误判 PASS**。处理办法：mcp-client 在任何工具结果里看到这个提示就锁存 `restarted()`；锁存之后 `diagnostics()` 把两个通道都标为 `failed`（原因：重启前的条目已丢失）；
  - **McpBackend 的行为变化**（只有这些）：以前诊断工具返回 `is_error`、输出格式不认识、浏览器中途被静默重启时，会被当成"没有错误"；现在都报为诊断不完整，replay 判非 PASS。以前底层调用 `Err` 会让整个 replay 以错误中止，现在改为该通道 `failed`、用例判非 PASS。正常输出的行为不变，由 e2e 测试守住（§5.6 阶段 1 证据）。
  - 已知限制：工具调用本身没有超时，MCP 服务端挂起时诊断读取也会挂起（现有行为，所有工具都一样）。超时归入 §3.4 的"超时（结果未知）"类，在阶段 3/4 处理。
  - 不为此建立通用诊断框架。
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
- **语义比较的细则（r3）**：
  - **AX→DOM 映射**：每个可定位节点都要映射到能实际执行点击、输入的 DOM 元素（`backendDOMNodeId`）。没有对应 DOM 节点的 AX 节点不分配 uid。契约测试要在两个后端上**实际执行**定位和操作，而不只比较快照文本；
  - **`role+name+nth`**：在重名元素、无名称元素和动态插入删除的页面上，两个后端的 `resolve` 要指向同一个元素；
  - **隐藏节点**：`display:none`、`aria-hidden`、`visibility:hidden` 的行为**以 McpBackend 的实测结果为准**逐项比较，不预设二者一致；不一致的地方作为发现记录下来；
  - **层级**：AX 树不保证有完整的 DOM 层级。被无障碍树忽略的节点、匿名容器、嵌套 iframe 按可观察的节点语义和实际定位行为验收，不要求内部结构逐项相同；
  - **uid 失效**：`backendNodeId` 映射只在本代快照内使用；导航、重新取快照、执行上下文销毁之后，旧 uid 报 `StaleUid`。`StaleUid` 不得被吞掉，也不得自动改去点击另一个"看起来相似"的元素。
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

**schema 相同不代表行为相同（r3）**。以下行为都要有两个后端的实际对照证据（§5.5）：click 的目标选择、滚动和遮挡处理；fill 对 input、textarea 和受控表单组件的效果；wait_for 的条件、超时和完成语义；evaluate_script 的执行上下文和错误传播；handle_dialog 的阻塞与关闭；navigate 失败时的 `is_error`；`storage_state` / `apply_storage_state` 对 HttpOnly Cookie 和 localStorage 的处理。

`ACTION_TOOLS` 里的工具必须真实实现。NativeBackend 做不到的能力**不得空实现或返回伪成功**：要么不出现在工具目录里，要么调用时返回 `is_error` 并说明不支持。

### 3.4 结果分类与重试（按 ChatGPT 意见）
- 每个工具调用的结果分为五类：**成功 / 业务断言失败 / 执行错误（含 StaleUid、不可交互）/ 超时（结果未知）/ 浏览器断连**。
- 超时和断连之后，执行状态未知：**不自动重试有副作用的工具**（click、fill、fill_form、press_key、handle_dialog、带写操作的 evaluate_script）；只读工具（snapshot、list_*、wait_for）允许一次有界重试。
- 用例的最终判定沿用 webtest 现有规则；新增的"结果未知"一律判为非 PASS。
- **报告里五类保持区分（r3）**：最终都可以判为非 PASS，但报告要写明属于哪一类，不能压成同一种失败。尤其是业务断言失败和执行错误：前者说明执行有效、结果不对，后者说明操作本身没完成。
- **状态未知后可以核实，但不重放（r3）**：可以做安全的状态核实（重新取快照、只读查询），但核实**不等于**自动重放原操作。核实结果只用于报告，不用来把"结果未知"改判成 PASS。

### 3.5 每个 flow 一个临时 profile、无头、可以并发。
要有证据而不是凭"用了 tokio"推断（r3）：
- 并发的 flow 不共享 profile、uid 映射或浏览器实例；
- 一个 flow 失败或超时，不会关掉别的 flow 的浏览器；
- 正常关闭、启动失败、超时、Chrome 进程异常退出、任务被取消之后，Chrome 进程和临时 profile 的清理情况可以验证；
- 并发测试记录实际并发数和每个 flow 的结果。

## 4. Cookie / 调试端口安全审计（初步结论，已核实）
| 项 | 事实 |
|---|---|
| 为什么开端口 | chrome-devtools-mcp 当前的 30 个工具里**没有 Cookie 读写工具**，HttpOnly Cookie 只能走 CDP（`webtest tools` 实测；只有 `get_network_request` 能看到请求头）。所以 mcp-client 给 Chrome 加了 `--remote-debugging-port=0` |
| 绑定地址 | Chrome 默认绑定 `127.0.0.1`，端口随机（写在 `<profile>/DevToolsActivePort`），用 `ws://127.0.0.1:<port>/...` 连接 |
| 启停时机 | **每个 webtest 浏览器都会开**，不只是用到 Cookie 的时候；在整个浏览器运行期间都开着；浏览器关闭时消失。profile 是临时目录 |
| 访问边界 | CDP 没有鉴权。**本机任何用户**都能在运行期间连上并完全控制这个测试浏览器。**实测：WSL2 默认把 127.0.0.1 端口转发给 Windows，Windows 进程也能连上**。网页本身不能连（Chrome 校验 WebSocket 的 Origin，没设 `--remote-allow-origins`） |
| 暴露的资产 | 测试账号的会话 Cookie、对测试浏览器的控制权。不是用户的个人 profile |
| 日志 | Cookie 只写进 `auth/*.json`（权限 600，已在 .gitignore 里），不进日志 |

- **结论（r3 改写，不再写"低风险"）**：**存在本机进程或本机其他用户接管测试浏览器的风险。**当前只允许在受控的单用户开发环境里保留；不允许部署到多用户或共享主机。风险大小要结合主机隔离、测试账号权限和端口的实际可达范围来评估。临时 profile 只缩短了凭据存在的时间，不能消除运行期间的控制风险。
- **B1 的约束（可执行、可复核）**：
  1. McpBackend 暂时仍是默认后端，**只在符合安全边界的环境里运行**：单用户、受控的开发机；或者每个 job 独占一台临时虚拟机的 CI（例如 GitHub 托管 runner）；
  2. **禁止**在多用户主机、共享主机或不受信任的自托管 runner 上运行 McpBackend。README 新增"运行环境限制（调试端口）"一节写明这一点（阶段 1 已加）；
  3. NativeBackend **必须用 pipe**，不得为了统一接口重新引入 TCP 调试端口。阶段 2 的测试要断言 NativeBackend 运行时 Chrome 命令行里没有 `--remote-debugging-port`，也没有 `DevToolsActivePort` 文件；
  4. **每次验收都复核**：实际监听的地址和端口（`ss -ltnp`）、从 Windows 能否连上、浏览器关闭后端口和进程是否消失。结果写进阶段报告；
  5. Cookie、认证数据和可重用的会话凭据不写进日志，也不提交到 Git（`auth/` 在 `.gitignore` 里，文件权限 600）；
  6. 后续单独跟进：chrome-devtools-mcp 能否以 pipe 方式启动（Puppeteer 支持 `pipe: true`，chrome-devtools-mcp 是否暴露这个选项**需验证**），或者以后是否提供 Cookie 工具，从而去掉这个端口。**不能**因为 NativeBackend 没有端口，就说 webtest 已经解决了这个问题。
- README 里的说明只是约束，不能作为安全问题已经解决的证据。这个问题不阻止 B1 在符合边界的环境里开发，但**阻止在不符合边界的环境里部署或迁移**。

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

### 5.5 补充测试（r3）
| 测试 | 必须证明 |
|---|---|
| 诊断读取失败 | 不会被当成"没有错误"（阶段 1 已覆盖 McpBackend） |
| 部分诊断结果 | 不会静默丢掉没读到的条目（阶段 1 已覆盖 McpBackend） |
| uid 失效 | 旧 uid 不会静默映射到新节点 |
| 断连后的状态核实 | 不会重复执行有副作用的操作 |
| 并发 profile 隔离 | 两个 flow 不互相污染，也不会关掉对方的浏览器 |
| 登录态往返 | Cookie 和 localStorage 的保存、恢复语义与 McpBackend 一致 |
| 工具错误传播 | 执行错误不会被包装成成功的 `ToolOutput` |
| 资源清理 | 启动失败、崩溃、任务取消之后没有残留的 Chrome 进程和临时 profile |

- **§5.2 的 5 轮只是初始对照**，不能单独证明稳定性或统计显著性。要保留逐轮的原始结果、环境信息和失败记录；发现不一致就先逐例调查，再决定要不要扩大规模。
- **变异测试的记录格式**：变异点、预期变红的测试、实际结果、恢复正确实现后的结果，四项都要有。

### 5.6 分阶段实施（r3，按 ChatGPT 建议）
| 阶段 | 内容 | Gate |
|---|---|---|
| 1 接口与基线 | 定义 `BrowserBackend` 和 `diagnostics()`（含 Coverage）；McpBackend 的工具输出、诊断解析、登录态行为不变 | McpBackend 回归测试通过；接口 diff 经审核后再进入阶段 2 |
| 2 NativeBackend 基础能力 | pipe 生命周期、快照、uid 代次、基础工具；先在本地契约页面上验证 | 契约测试通过；错误和资源清理可验证；没有调试端口 |
| 3 工具与登录态兼容 | 补齐其余工具、结构化诊断、Cookie/localStorage 往返 | 工具行为和错误传播的对照测试通过 |
| 4 故障注入与双后端对照 | §5.2、§5.3、§5.5 的用例；变异测试；逐轮记录；不一致调查 | 所有强制场景都有证据；NOT RUN/BLOCKED 不计 PASS |
| 5 独立验收与迁移决策 | 汇总 diff、原始输出、限制、未执行项和审核通道，交给用户和 ChatGPT | 迁移另行批准；在那之前 McpBackend 一直是默认后端 |

每个阶段结束都写交流文档给 ChatGPT，**审核通过后再进入下一阶段**。

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
- 不绕过验证码、异常流量页面或任何访问限制；遇到就停止。
- **测试证据和代码审核证据分开（r3）**：
  - 测试证据：实际运行的命令、环境、原始输出、故障注入结果、变异测试，不依赖审核模型的判断；
  - 代码审核证据：审核通道、实际强度、审核范围和没覆盖到的风险；
  - 强度如实记录：回退到 web-gemini 就记 3，**不得**记成 4，也不为了通过门禁改阈值；
  - 如果既定的 review-gate 强度要求没有达到，就标记"审核条件未满足"，等待符合要求的复核，不把网页通道回退当成等价替代；
  - 关键路径（快照语义、uid 失效、Cookie、调试端口、工具副作用、PASS/FAIL 判定）除了 gate 审核，还要在阶段交流文档里单独列出，交给 ChatGPT 做针对性复核。

## 8. Non-goals 与变更控制
- 不重写 agent、flow、断言、报告、自愈和探索逻辑；`diagnostics()` 的移动是唯一的结构性改动；
- 不引入队列、Cron、持久化调度、MCP Gateway；跨域 iframe 和 L4 压力测试推到 B2；
- **McpBackend 一直是默认后端**，迁移要等用户根据验收报告另行批准；
- 实施中如果发现需要扩大范围的架构问题，**先提 ADR**，不自行扩展。

## 9. Exit Decision
根据 §5 的数据，由用户和 ChatGPT 决定：迁移默认后端 / 维持双后端 / 放弃 NativeBackend。

## 10. 用户决定记录（2026-10-12）
- **审核通道"维持现状"**：审核以 Gemini API 为主（免费额度，经常遇到 503/429/超时），回退到 web-gemini，强度 3。
- **"profile 先保留"**：保留 `~/.local/share/browser-runtime/profiles/gemini-a1`（已登录）。不在上面跑自动化：A1 期间跑了 5 次之后出现了 Google 的 /sorry 页面。
