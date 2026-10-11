# Rust Agent Browser Runtime (Chrome)

## 项目架构与开发治理方案

-   文档版本：1.2（吸收生产化与安全建议）
-   状态：架构基线草案，须随阶段评审迭代
-   实施原则：小步交付、证据验收、允许基于证据修订架构

------------------------------------------------------------------------

## 1. 本次优化的核心结论

原方案方向正确，但需要从"技术功能清单"升级为"可执行、可验证、可演进的工程方案"。主要调整：

1.  **避免把未来设想全部提前固化为必须实现的架构。**
    先定义边界和契约，具体 crate、trait、目录和实现方式由阶段设计决定。
2.  **将"模块目录树"从强制结构改为建议结构。**
    不要为了符合目录树而创建大量空模块、空 trait。
3.  **区分必需能力、可选能力和后续能力。** 特别是 MCP、Agent
    Loop、调度平台、Stealth、分布式 Lease，不应默认全部属于最小可用
    Runtime。
4.  **重新定义验收等级。** `cargo check`、`cargo test`
    只能证明编译和已执行的测试通过，不能证明模块完整或生产可用。
5.  **为每个 Phase
    加入明确的范围、非目标、交付物、测试场景和退出条件。**
6.  **明确浏览器自动化的正确性优先级。**
    Tab/Frame/Navigation/Locator/Wait
    的生命周期与并发正确性先于吞吐量和高级恢复。
7.  **把安全作为贯穿式要求，而不是只放在后期 P18。**
    域名限制、凭据保护、下载/上传限制、危险操作授权和日志脱敏应在对应功能首次出现时落实。
8.  **区分 Browser Runtime 与 Agent Platform。** Runtime
    应可独立使用；MCP、LLM、Scheduler/Queue 是可组合的上层服务。
9.  **建立变更控制，但不把架构文档变成不可修改的"技术宪法"。** Claude
    Code 不能擅自偏离基线，但发现设计缺陷时必须提出 ADR
    并允许评审后修订。
10. **不预先承诺没有测量依据的性能数字。** 先建立基准场景，再确定目标。
11. **明确 Chrome 进程所有权和生命周期。** 自启动 Chrome 与连接外部 Chrome 必须采用不同的清理策略；不能假设 Rust 子进程句柄能自动回收整个 Chrome 进程树。
12. **区分恢复、自愈与轮换。** 有限重试、页面/元素重新定位和 Chrome 实例轮换是不同机制；不得无限重试，也不得在任务状态未知时盲目重放有副作用的动作。
13. **测试抽象保持最小。** 用纯状态转换测试和必要的小型接口验证关键逻辑；Mock 不替代真实 Chrome 集成测试。
14. **可观测性先轻后重。** 先建立结构化 `tracing`、关联 ID 和关键耗时，再按部署需求接入 OpenTelemetry/OTLP，不把完整遥测后端设为 P0 前置条件。
15. **将网页内容视为不可信数据。** Prompt Injection 防御不能只依赖 Chrome flags；导航、重定向、下载/上传、工具参数和高风险操作均须按策略校验。

------------------------------------------------------------------------

## 2. 产品定义与边界

### 2.1 产品目标

构建一个 Rust 实现的 Chrome/Chromium Browser Runtime，为自动化程序和 AI
Agent 提供稳定、可测试、可观测的浏览器控制能力。

核心职责：

-   管理 Chrome 进程或连接到已有的 Chrome CDP endpoint
-   维护 Browser、Target/Tab、Page、Frame 的生命周期映射
-   提供导航、页面观察、元素定位、等待和交互 API
-   对超时、目标销毁、导航中断、元素失效等情况提供明确错误和有限恢复机制
-   为上层 MCP Server、Agent、任务调度器提供稳定接口
-   提供集成测试、故障测试、诊断日志和可复现的验收证据

### 2.2 分层架构

``` text
应用 / LLM Agent / 自动化工作流
            │
     可选 MCP Gateway
            │
     Agent Runtime（可选）
            │
     Browser Runtime API
            │
 ┌──────────┼────────────────────┐
 │          │                    │
Lifecycle  Context            Interaction
 │          │                    │
Tabs/Frames Locator/Wait      Input/Actions
 └──────────┼────────────────────┘
            │
     CDP Adapter / Session
            │
       Chrome / Chromium

独立的可选控制平面：
Scheduler → Queue → Workers → Persistence → Notifications
```

### 2.3 关键边界

-   **CDP**：Rust Runtime 与 Chrome 之间的浏览器控制协议。
-   **MCP**：上层 Agent 与工具服务之间的协议。MCP
    是接入方式，不是浏览器控制层。
-   **Browser Runtime**：不依赖特定 LLM，也不要求必须通过 MCP 使用。
-   **Agent Runtime**：负责 Observe → Plan/LLM → Tool → Observe
    的循环，可单独启用。
-   **Control
    Plane**：任务队列、Cron、持久化、通知等运维能力。不得与单个 Browser
    Session 的核心状态强耦合。

### 2.4 非目标（除非后续明确立项）

-   不承诺绕过网站反自动化或 CAPTCHA。
-   不宣称 100% stealth、100% self-healing 或零崩溃。
-   不在第一阶段实现分布式调度集群。
-   不在没有需求与测试的情况下实现通用插件市场、复杂租户系统或自研 LLM。
-   不把 MCP 当作唯一公开 API。

------------------------------------------------------------------------

## 3. 架构原则

### 3.1 先定义契约，再决定抽象

不要一开始为每个文件都定义
trait。只有在存在多个实现、需要隔离外部依赖、或测试替身确有价值时才引入
trait。

每个模块应定义： - 职责与不负责的事项 - 输入/输出与错误类型 -
生命周期归属 - 并发与取消语义 - 可观察性要求 - 测试策略

### 3.2 生命周期与 ID

至少明确区分： - `BrowserId` - `SessionId` - 应用层 `TabId` - CDP
`TargetId` - CDP `SessionId` - `FrameId` - 一次操作的 `OperationId` /
`TraceId`。如存在上层请求或任务，还应保留 `RequestId` / `JobId`，并在日志和 tracing 上下文中关联。

CDP 不会自动传播应用层 `TraceId`。关联 CDP 命令与事件时，应使用 Runtime 的 Span/日志上下文以及 Browser、Target、Session、Frame 和操作标识，不得假定 Chrome 会原样传播 OpenTelemetry Trace Context。

不得把时间戳当作唯一标识。映射应有创建、更新、销毁和断连清理规则。所有缓存对象都必须有失效策略。

### 3.3 并发与锁

-   不得在持有全局 registry/state mutex 时等待 CDP、网络、文件 I/O 或
    LLM 响应。
-   锁只保护短暂的内存状态读取/更新；先复制所需句柄或数据，再释放锁并执行异步操作。
-   不能假设"每个 Tab 一个 Tokio 任务"就等于"每个 Tab 一个线程"。
-   明确同一 Tab
    上哪些操作允许并发、哪些必须串行。例如导航、输入与点击之间可能需要操作级协调。
-   对任务并发、浏览器数量、Tab 数量和每个 Session 的操作并发设置上限。
-   明确取消任务时如何中断等待、释放资源和标记结果。

### 3.4 错误模型

错误需要包含稳定的类别和是否可重试的判断，例如： - BrowserDisconnected -
TargetClosed - FrameDetached - NavigationTimeout - LocatorNotFound -
ElementNotInteractable - ProtocolError - OperationCancelled -
PermissionDenied - StorageError

错误信息不得泄露 cookie、密码、token、完整敏感 URL
或页面中的敏感内容。不要用 `let _ = ...` 静默吞掉关键错误。

### 3.5 进程与资源所有权

- 明确 Browser Runtime 是**自启动 Chrome**，还是**连接外部 Chrome**。两种模式的关闭责任不同。
- 对自启动 Chrome，必须定义进程树回收策略。保存 `Child` 句柄本身不保证 Chrome 所有后代进程都会退出。
- Windows 可评估 Job Object；Linux 可评估进程组/会话，并在部署环境适合时使用 systemd、容器或其他监督机制。平台方案应以实际验证为准。
- 优雅关闭应包含停止接收新任务、取消或排空进行中的操作、关闭 CDP 资源、回收自有进程，并为关闭设置上限；异常终止场景需单独测试。
- 对外部 Chrome，Runtime 不得擅自终止不属于自身所有的进程。
- 区分进程存活、CDP 连接可用、事件循环持续推进和单次操作超时；这些是不同的健康信号，应有不同的诊断和恢复路径。

### 3.6 测试抽象

- 优先将状态转换、事件处理和策略判断设计为可单元测试的逻辑。
- 只有在隔离外部依赖或提供真实测试价值时，才为关键 CDP 操作增加小型接口或测试替身。
- Mock 事件流用于验证 Target/Frame 状态机、断连、重复/乱序事件和取消；真实 Chrome 集成测试仍是协议和生命周期行为的必要证据。
- 不为 Mock 而预先构建庞大、重复实现 Chromium 协议的通用模拟层。

### 3.7 可观测性

- 使用结构化 `tracing` 记录操作开始/结束、耗时、结果、错误类别及关联标识。
- 在适用路径中关联 `RequestId`、`JobId`、`OperationId`、`BrowserId`、`TargetId`、`FrameId`；避免把敏感页面内容、Cookie、凭据或完整敏感 URL 写入日志。
- 先定义稳定的日志字段和关键指标；按部署需求再增加 OpenTelemetry/OTLP 导出、采样和后端集成。
- 可观测性必须能区分超时、取消、目标销毁、连接断开和浏览器崩溃，不能只记录一个笼统的“操作失败”。

### 3.8 依赖管理

-   首选维护活跃、兼容 Tokio、许可符合项目要求的依赖。
-   具体 crate 版本以实施时锁定的 `Cargo.lock`
    和实际编译结果为准，不在总方案中承诺永久版本。
-   采用 `chromiumoxide` 时，优先使用成熟的高层 API；确有缺口时再通过其
    CDP 执行接口补充。
-   直接调用 CDP 的代码必须集中在适配层，避免协议细节散落到 MCP/Agent
    代码中。

------------------------------------------------------------------------

## 4. 推荐的目标模块（建议而非硬性目录）

``` text
src/
  main.rs
  config/
  error/
  browser/
    runtime.rs
    lifecycle.rs
    tabs.rs
    frames.rs
    navigation.rs
    session.rs
  cdp/
    client.rs
    events.rs
  context/
    ax_tree.rs
    dom.rs
    screenshot.rs
    builder.rs
  locator/
  wait/
  interaction/
  mcp/
  agent/
  control_plane/
    queue.rs
    worker.rs
    cron.rs
    lease.rs
  storage/
  notification/
  observability/
  security/
tests/
  unit/
  integration/
  failure/
  stress/
```

只有在模块开始承担实际职责时才创建文件。允许按实现需要调整结构，但需在阶段交付说明原因和影响。

------------------------------------------------------------------------

## 5. 重新整理的开发路线图

路线图是依赖建议，不代表所有阶段都必须无条件实施。每阶段先确认需求、风险、接口与退出条件。

### Phase 0 --- 工程骨架与开发规范

目标： - 建立 Cargo
项目、配置加载基础、错误类型基础、日志初始化、测试布局、README、格式化与
lint 约定。 - 若使用 GitHub/GitLab 等 CI，再添加最小 CI。

非目标： - 不启动 Chrome。 - 不创建未使用的大量模块、空 trait
或未来业务占位实现。 - 不实现 MCP、Agent、数据库、Cron、Webhook。

退出条件： - `cargo fmt --check` 通过 - `cargo check` 通过 -
`cargo test` 通过 - 配置错误有明确提示 - README 能说明构建与测试方法 -
无未解释的 warning；若有，逐项记录

### Phase 1 --- Chrome Runtime 与 CDP 生命周期

目标：
- 启动本地 Chrome，或连接已有 CDP endpoint，并明确两种模式的资源所有权。
- 将 CDP 细节封装在内部边界中；配置 Chrome 路径、连接方式、启动参数和超时。
- 支持显式选择有头/无头模式以及 CI/容器配置；不要把环境特定参数无条件写死。
- 定义健康检查、优雅关闭、关闭超时、CDP 断连检测和事件循环停滞的诊断方式。
- 对自启动 Chrome 定义平台相关的进程树清理策略；连接外部 Chrome 时不得误杀外部进程。
- 默认限制 DevTools 端点暴露范围；优先使用受控的动态端口分配，避免端口冲突和未授权远程控制。

测试：
- 启动并连接成功；Chrome 不存在/路径错误；endpoint 不可用；端口冲突。
- Chrome 或连接异常退出；CDP 请求超时/断连；优雅关闭与关闭超时。
- 验证自启动 Chrome 的子进程/进程树清理；对外部 Chrome 验证 Runtime 不会终止外部进程。
- 在目标 CI/容器环境验证所选 Headless 与 sandbox 配置。

安全说明：
- 不把 `--no-sandbox` 作为默认参数。优先修正 CI/容器权限和运行环境；只有受控隔离的测试环境确有需要时，才显式配置该例外。
- `--disable-dev-shm-usage` 等参数按目标容器的共享内存和运行条件决定，不无条件启用。

退出条件：真实 Chrome 集成测试通过；启动/连接失败可诊断；关闭路径有可重复证据；自有进程回收策略已验证或记录平台限制；调试端点未被不必要地暴露。

### Phase 2 --- Tab / Target Manager

目标： - 列出、创建、切换、关闭 Tab。 - 处理 Target
创建、信息更新、销毁及断连。 - 建立稳定的应用 TabId 与 CDP TargetId
映射。

测试： - 多 Tab 创建/切换/关闭 - 用户手动关闭 Tab - Target
事件乱序或重复 - 当前 Tab 被关闭 - 关闭后不再返回陈旧对象
- 通过纯状态转换/必要 Mock 测试覆盖事件序列，并以真实 Chrome 集成测试验证实际事件行为。

退出条件：Tab registry 在重复事件和生命周期变化下保持一致；关闭或销毁后的对象不会被继续当作有效对象使用。

### Phase 3 --- Frame Manager 与 Navigation

目标： - 主 Frame、iframe、嵌套 iframe 的识别与生命周期。 -
导航、后退、前进、刷新及导航超时。 - 处理 frame detach、跨域 frame
和导航期间对象失效。

测试： - 主页面导航 - iframe 导航/移除 - 页面跳转期间并发操作 -
超时、失败与取消

退出条件：Frame/Navigation 的对象归属与错误语义明确；不得误把已失效
Frame 当作有效对象。

### Phase 4 --- Context Engine（AXTree / DOM / Screenshot）

目标： - 默认优先提供精简、结构化的可访问性上下文。 - 必要时按需读取 DOM
或截图。 - 明确字段映射、节点引用生命周期和上下文大小限制。

测试： - 常见控件、隐藏元素、无障碍标签、动态页面 - AXTree 不足时的 DOM
fallback - 超大页面的截断与限制 - 输出稳定性与 token/字节大小测量

退出条件：上下文结果有 schema 和大小限制；不宣称未经测量的压缩比例。

### Phase 5 --- Locator Engine

目标： - 支持语义定位和必要的 CSS/XPath fallback。 -
明确多匹配、零匹配、过期节点、iframe 范围的处理规则。 -
区分"定位到元素"和"元素可交互"。

测试： - 唯一匹配、多匹配、零匹配 - 动态 DOM 重建 - iframe 内定位 -
隐藏、禁用、被遮挡元素

### Phase 6 --- Wait Engine

目标： - 基于条件等待：元素、文本、导航、状态及网络空闲（若 Network
能力已实现）。 - 统一超时、取消和轮询策略。 - 不用固定 sleep
作为主要同步机制。

测试： - 条件立即满足、延迟满足、永不满足 - 页面导航期间等待 -
取消等待 - 超时后错误分类正确

### Phase 7 --- Interaction Engine

目标： - 点击、输入、按键、悬停、滚动、选择等核心操作。 -
明确动作前检查、动作执行、动作后观察和超时语义。 -
对文件上传等高风险功能设置权限与限制。

测试： - 常规控件 - 元素在动作前消失 - 动作触发导航 - 输入焦点错误 -
操作取消与超时

### Phase 8 --- Recovery / Self-Healing（有限、可度量）

目标： - 识别可恢复错误。 - 刷新上下文并使用语义信息重新定位。 -
限制重试次数，避免重复执行有副作用的动作。

关键原则： -
不自动重复提交订单、付款、删除、发布等不可安全重放的操作。 -
每次恢复必须记录触发原因、重定位策略、结果和是否执行过原动作。 -
以测试数据报告恢复成功率和误恢复率。

### Phase 9 --- MCP Gateway

目标： - 将稳定的 Runtime API 暴露为 MCP tools。 - 输入 schema
校验、结构化结果、错误映射、协议兼容性测试。 - MCP handler 不直接管理
CDP socket 或绕过 Runtime。

测试： - initialize / tools list / tool call -
无效参数、未知工具、运行时错误 - stdout/stderr 分离（若使用 stdio
transport） - 客户端断开与请求取消

### Phase 10 --- Agent Runtime（可选独立组件）

目标： - 实现 Observe → LLM/Planner → Tool → Observe 循环。 -
支持最大步数、最大运行时间、工具调用预算、token 预算、取消。 - 将 LLM
provider 作为可替换依赖，不绑定单一模型厂商。

非目标： - 不把 `sleep` 或 mock 结论视为真正 Agent Loop。 - 不在 Browser
Runtime 核心层硬编码 LLM API。

### Phase 11 --- Persistence 与任务状态

目标： - 如果产品确实需要持久化任务，定义 jobs、job_runs、job_events
和迁移策略。 - 采用 SQLite 时评估
WAL、busy_timeout、索引、事务和备份恢复。 -
明确任务状态机和崩溃恢复语义。

### Phase 12 --- Queue / Worker / Retry

目标： - 有界队列、worker pool、并发限制、取消和优雅关闭。 -
有界重试、退避策略、最大重试次数与死信/终态记录。 - 若需要多进程或分布式
worker，再设计 lease、租约过期和幂等性；单进程 MVP
不应为了"企业级"而过度设计。

### Phase 13 --- Cron 与 Notifications

目标： - 每个任务真正遵守自己的 cron
expression，支持新增、修改、禁用和移除。 - 通知 provider
配置化，支持超时、失败记录和有限重试。 - Webhook secret
不进入源码、日志或普通配置导出。

### Phase 14 --- Security 与 Observability 基线完善

安全与可观测性从 P0 开始，随功能逐步落地。本阶段补齐系统化策略：
- 域名/协议 allowlist（如适用），并在重定向、新 Tab、弹出窗口等适用入口重新执行策略校验。
- 防范 Runtime 被用来访问不允许的 localhost、私网和云元数据地址；具体策略按部署威胁模型定义。
- 高权限工具授权；Cookie、凭据、下载、上传和 JavaScript 执行限制。
- 对网页文本、DOM、AXTree、截图和网页提供的工具建议一律视为不可信数据；它们不能覆盖系统策略或自行授予权限。
- 日志脱敏、审计事件、资源限制，以及关键错误类别和耗时指标。
- 在需要时增加 OpenTelemetry/OTLP 导出、采样、指标和告警后端；不得把接入特定遥测后端当作 Runtime 核心功能的硬依赖。
- 验证关联 ID 在 MCP/任务（若存在）→ Runtime 操作 → Browser/Target/Frame 的日志与 Span 中可追踪；不要假设 CDP 自动传播 Trace Context。

### Phase 15 --- Benchmark 与 Production Hardening

目标： - 建立可复现的性能和长时间稳定性测试。 -
验证资源限制、恢复、升级/迁移、关闭和运维诊断。 -
根据测量结果确定性能目标，不预先捏造固定数字。

测试至少包含： - 启动时间、导航延迟、上下文生成时间、工具调用延迟 - 多
Tab / 多任务并发 - CPU、内存、队列深度、失败率 - 长时间运行、Chrome
crash、网络中断和任务取消
- 长时间运行时的内存/错误率趋势、Tab 增长和资源回收情况。
- 在受控测试中验证 Chrome 实例轮换流程（若产品决定启用）：停止分配新任务、排空/取消任务、处理未知执行状态、启动并健康检查新实例，再关闭旧实例。

Browser Recycling 原则：
- 先收集真实运行数据，再决定是否启用轮换以及阈值；不预先规定未经测量的固定任务数/运行时长阈值。
- 轮换 Chrome 进程不等于删除或重建 User Profile。Profile 的持久化、隔离与恢复策略应单独定义。
- 不得因轮换而盲目重放有副作用或执行结果未知的操作。
- 轮换属于后续生产化能力，不是 P0/P1 的强制实现项。

------------------------------------------------------------------------

## 6. 验收模型：按功能选择适用等级

不要机械要求每个 Phase 都通过全部 L0--L5。每个 Phase
的任务书应标明适用等级与证据。

-   **L0 --- Build**：格式化、编译、lint（适用时）通过。
-   **L1 --- Unit**：单元测试通过，覆盖核心分支与错误路径。
-   **L2 --- Integration**：与真实 Chrome / SQLite / MCP client
    等真实依赖集成。
-   **L3 --- Failure**：主动注入超时、断连、销毁、取消、竞争条件等故障。
-   **L4 --- Stress**：并发、负载、资源上限和长时间测试。
-   **L5 --- Release
    Acceptance**：按明确的发布场景验证安全、稳定、运维、恢复与文档。

等级不能互相替代。例如 L0/L1 通过不代表功能完成；L4/L5
也不能只凭"运行过一遍"宣布通过。

每个验收结论必须附： - 实际执行的命令 - 环境信息 - 通过/失败的测试名称 -
关键输出或日志 - 未执行项及原因 - 已知限制和风险

禁止声称未执行的测试通过。`NOT RUN` 不等于 `PASS`。

------------------------------------------------------------------------

## 7. 每个 Phase 的标准任务书模板

每个 `plans/Pxx-*.md` 至少包含：

1.  **Purpose**：本阶段解决什么问题
2.  **Dependencies**：依赖哪些已验收阶段
3.  **Scope**：本阶段必须实现的内容
4.  **Non-goals**：本阶段明确不做的内容
5.  **API / Data Contracts**：接口、输入输出、错误、生命周期
6.  **Concurrency / Cancellation**：并发、锁、取消、超时规则
7.  **Security**：本阶段相关的权限与数据保护
8.  **Tests**：单元、集成、故障、压力测试中哪些适用
9.  **Acceptance Criteria**：可观察、可重复的通过条件
10. **Deliverables**：代码、文档、测试、日志、基准
11. **Known Limitations**：已知缺口
12. **Exit Decision**：PASS / PASS WITH CONDITIONS / REJECT

------------------------------------------------------------------------

## 8. Claude Code 执行协议

每个阶段开始时，Claude Code 必须：

1.  阅读本文件和当前 Phase 任务书。
2.  检查仓库现状、Git 状态和已验收阶段。
3.  先给出简短实施计划；如果发现需求冲突或阻塞，不应猜测。
4.  仅修改当前 Phase 需要的内容。
5.  先实现必要测试，再运行规定命令。
6.  不得虚报测试，不得把未运行的测试写成 PASS。
7.  不得顺手扩展到未来阶段。
8.  如果必须修改已验收模块，说明原因、影响和回归测试。
9.  若需要偏离架构基线，先提出 ADR（Architecture Decision
    Record），列明理由、备选方案、取舍和迁移影响；经用户/评审确认后再实施。

### 每阶段交付格式

``` text
## Phase
Pxx - Name

## Summary
...

## Files Changed
...

## Design Decisions / ADR
...

## Tests Executed
命令、环境、结果、关键输出

## Acceptance Matrix
L0: PASS / FAIL / NOT RUN / N/A
L1: PASS / FAIL / NOT RUN / N/A
L2: PASS / FAIL / NOT RUN / N/A
L3: PASS / FAIL / NOT RUN / N/A
L4: PASS / FAIL / NOT RUN / N/A
L5: PASS / FAIL / NOT RUN / N/A

## Known Limitations / Risks
...

## Recommended Next Step
...
```

------------------------------------------------------------------------

## 9. 独立评审协议

ChatGPT 评审不应只看 Claude Code 的总结，而应尽可能基于： - 实际 diff -
关键源文件 - 测试代码 - 测试命令与输出 - 日志或录屏/截图（适用时） -
基准原始数据（适用时）

评审结果： - **PASS**：当前阶段验收条件全部满足。 - **PASS WITH
CONDITIONS**：核心条件满足，剩余事项明确且不阻塞下阶段。 -
**REJECT**：存在架构违例、关键测试缺失、生命周期/并发错误、安全风险或虚报结果。

任何评审结论都要列明证据、问题等级、修复要求和回归测试。若没有看到实际
diff 或测试证据，应标为"证据不足"，不能仅凭文字总结 PASS。

------------------------------------------------------------------------

## 10. 安全基线（贯穿所有阶段）

-   默认不把 CDP endpoint
    暴露到公网；远程调试端口应绑定受控接口并设置访问边界。
-   不在日志中输出密码、cookie、授权头、API key、webhook secret
    或完整敏感页面内容。
-   浏览器可访问的
    URL、重定向目标、下载路径和上传文件必须按权限策略校验。
-   JavaScript 执行、文件上传/下载、Cookie
    修改等高权限能力应明确授权并记录审计事件。
-   对 LLM 返回的工具参数一律视为不可信输入，先做 schema 和策略校验。
-   为页面大小、截图大小、超时、并发数、重试数和工具输出大小设上限。
-   对有副作用的动作采用显式确认或不可自动重试策略。
-   不将网页内容中的指令当作系统或开发者指令；网页内容是外部不可信数据。

------------------------------------------------------------------------

## 11. 性能与质量目标的制定方式

不要在没有基线时写死"P95 小于 X 毫秒"或"压缩 80%"。流程应为：

1.  选定可复现的页面和工作负载。
2.  固定操作系统、CPU/RAM、Chrome 版本、Rust 版本与运行配置。
3.  记录冷启动/热启动、单 Tab/多 Tab、空闲/高负载等场景。
4.  记录中位数、P95/P99、吞吐量、CPU、内存和错误率。
5.  先得到基线，再由产品需求确定目标。
6.  优化后在同一环境和同一场景复测。

------------------------------------------------------------------------

## 12. 初始 P0 任务书

创建 `plans/P0-project-skeleton.md`，内容至少如下：

### Purpose

建立可编译、可测试、具备基础配置和诊断能力的 Rust 项目。

### Scope

-   Cargo 项目与最小目录结构
-   配置加载与配置错误提示
-   统一错误类型的最小基础
-   结构化日志初始化（优先建立 `tracing` 基础和基本关联字段；不要求 P0 接入完整 OpenTelemetry/OTLP 后端）
-   单元测试布局
-   README：构建、运行测试、配置说明
-   `cargo fmt --check`、`cargo check`、`cargo test`
-   若仓库已有 CI，则接入；否则说明是否创建轻量 CI

### Non-goals

-   不启动 Chrome
-   不连接 CDP
-   不实现 Tab、Frame、Locator、Wait、Interaction
-   不实现 MCP、Agent、SQLite、Queue、Cron、Webhook
-   不为未来阶段创建没有用途的空实现

### Acceptance

-   项目在目标开发环境可编译
-   测试全部通过
-   错误配置给出可理解的错误，而不是 panic
-   无未解释的编译 warning
-   README 的命令在干净检出后可执行
-   日志字段与脱敏原则有简短说明；不在 P0 实现尚未使用的复杂 tracing 架构
-   提交完整变更清单、diff、命令与输出、已知限制

P0 的验收范围为 L0、L1；L2--L5 标为 N/A，而不是 PASS。

------------------------------------------------------------------------

## 13. 推荐的首次 Claude Code 指令

``` text
请先阅读项目根目录的 RUST_AGENT_BROWSER_RUNTIME.md。

当前只执行 P0。若 plans/P0-project-skeleton.md 不存在，请先根据主方案创建该任务书，再实施 P0；不要同时创建或实现未来 Phase 的功能。

开始修改前：
1. 检查当前仓库状态和已有文件，不覆盖用户已有工作。
2. 提交一个简短的实施计划。
3. 明确本阶段 Scope 和 Non-goals。

完成后执行适用的格式化、编译和测试命令，并提交真实命令与结果。
未执行的测试标为 NOT RUN，不得写成 PASS。
不要实现 Chrome/CDP/MCP/Agent/数据库/调度等 P1 及之后的功能。
按主方案中的交付格式报告变更、测试、风险与已知限制。
```

------------------------------------------------------------------------

## 14. 项目治理原则

本文件是当前架构基线，不是不可修改的教条。核心设计变更必须有记录、有理由、有测试影响分析，并经评审后再实施。

项目成功标准不是"目录齐全"或"代码量大"，而是每个阶段都有： - 明确边界 -
可复现的实现 - 可验证的测试 - 可诊断的失败 - 清晰的已知限制 -
独立评审记录

**最终目标：先交付可靠的 Browser Runtime，再按真实需求组合 MCP、Agent
与任务控制平面。**


------------------------------------------------------------------------

## 15. v1.2 变更记录

相较 v1.1，本版本吸收生产化评审建议，主要增加：

1. Chrome 自启动与外部连接的所有权区分，以及进程树回收和关闭超时要求。
2. Headless/CI 配置原则；明确 `--no-sandbox` 不是默认值，并要求验证真实部署环境。
3. 以小型测试边界和纯状态转换测试为先，Mock 不替代真实 Chrome 集成测试。
4. `tracing`、关联 ID、错误类别与关键耗时的分阶段可观测性路线；OpenTelemetry/OTLP 按需接入。
5. 强化网页不可信输入、重定向/导航、文件操作、调试端点和高风险操作的安全要求。
6. 将 Browser Recycling 定义为基于测量与健康状态的后续生产化能力，不在 P0/P1 强制实现，也不把进程轮换等同于重建 Profile。

本版本仍保持 P0 小范围交付原则。上述后续能力必须在对应 Phase 通过任务书、测试和证据逐步实现，不得借升级基线之名一次性扩大 P0 范围。
