# ADR-0：统一浏览器控制能力（browser-runtime）

- 状态：**已接受**（2026-10-11，用户原话：「用Gemini API吧，确认方案C，开始A1」）
- 日期：2026-10-11
- 依据：
  - `plans/RUST_AGENT_BROWSER_RUNTIME.md` v1.2（架构与治理基线）
  - `plans/research-unified-browser.md`（Claude 整理的仓库现状与设想）
  - ChatGPT 对两者的评审（2026-10-11，用户转达）
  - 本 ADR 里标"已核实"的条目，都是 Claude 在本机或公开源码上实际查过的。

## 1. 背景

本仓库有两条线在控制浏览器，各自都有维护成本：

| 线 | 现在的做法 | 问题 |
|---|---|---|
| A. 外部 AI 审核（review-gate 的 web-gemini 通道） | Chrome 扩展（MV3）+ Windows 本地桥接 `bridge-server.mjs`（8899）+ watchdog | 2026-10-07～10 这几天暴露了 8 类问题：合成点击不被页面认可（isTrusted）、扩展重载后 content script 失联、bg 与 content 版本错配、MV3 环境差异、跨进程超时链、和 DSH 共用标签页、排队与旧超时冲突、每次改动都要用户手动重载（详见 research 文档 §3.4）。计划中的 web-chatgpt 会把这套维护面再复制一份。 |
| B. 网页自动化测试（webtest） | Rust → MCP stdio → Node `chrome-devtools-mcp` → CDP | 能用而且成熟；`agent` crate 已经有 agent 循环、快照压缩、语义定位、回放、自愈。实际只用到约 13 个 MCP 工具。 |

## 2. 选项

- **A：维持现状，不做 Native Runtime。** 成本最低，但 A 线的维护面继续存在，web-chatgpt 还要再复制一份。
- **B：一次做完完整 Runtime（P0–P15），再替换所有调用。** 长期统一，但前期投入大、迁移风险高，也会重做 webtest 已经有的能力。
- **C：先做最小 Native Runtime，先接 A 线的一个审核通道做竖切；验证通过后，再作为 webtest 的第二个后端，用数据决定是否迁移。**

## 3. 决定

**采用 C。**

1. 新建独立 crate `crates/browser-runtime`，放在现有 workspace 里，复用现有的依赖和工程约定，不另起工程。它是**浏览器控制基础设施**：Chrome 生命周期与 profile、Target/Frame、导航与条件等待、真实输入、DOM/AX 观察、统一 CDP 适配层、tracing、安全策略边界。**它不是**审核系统、Agent 平台或调度系统。
2. **使用方各自写适配层：**
   - A：review-gate 新增审核通道，Gemini / ChatGPT 的差异（选择器、完成判定、新对话方式）放在**站点适配器**里，不进入通用 Runtime；
   - B：在 webtest 的 `agent` 下抽出浏览器后端接口，现有的 `McpBackend` 和新的 `NativeBackend` 并存。
3. **不重写：** review-gate 的门禁、强度分级、签名与 CI 验签；webtest 的 flow、断言、报告、agent 主循环、自愈；agent-bridge。
4. **不共用 Chrome 实例：** 审核用**专用的持久 profile**（人工登录、有界面、串行，只归 review-gate 所有）；测试用**临时 profile**（无状态、可并发、可 headless）。两者的安全边界、并发和恢复策略各自定义。
5. **迁移看数据，不看"新架构已完成"。** 如果 chrome-devtools-mcp 在某些方面更可靠，就保留它作为 webtest 的一个后端，这也是统一架构下合理的结果。现有的 web-gemini 扩展通道在迁移期间保留，作为对照和回退，DSH 自己的网页任务也继续使用它。
6. v1.2 的 **P11–P13**（Persistence、Queue、Cron、Notifications）**不在本项目范围内**，它们和 agent-bridge、DSH 调度重叠。完整 AXTree、通用 Agent Runtime、复杂自愈、Browser Recycling、OTLP 后端都**推迟**到 B2 再按数据决定。

**成功标准：** 两条线能共享一套稳定的浏览器能力，同时各自保持独立演进；不要求两条线最终都必须用 Native Runtime。

## 4. 前置条件与关键约束（A0 必须先处理）

### 4.1 合规（决定 A 线的技术选择，优先级最高）
- **OpenAI**：个人用户条款限制以自动或程序化方式提取服务输出。**自动读取 ChatGPT 网页回答不能作为确定的交付目标**，除非用户核实后接受这个风险。审核要用 OpenAI 的模型，**优先评估官方 API**：review-gate 已经内置 `openai` 通道，强度属于 external-api（4），配置一个 key 就能用，不需要开发。
- **Google（Gemini 网页）**：条款同样对自动化访问有限制，具体适用范围要由用户核查。现有的 web-gemini 通道已经在运行，**是否继续、是否用 Native Runtime 改造它，由用户在核查后决定**。
- 账号：如果使用网页通道，建议用专门的账号，不用主账号。
- 规则：碰到 CAPTCHA、拒绝访问或限流**一律停下交给用户**，不做反检测、不做绕过（继承 v1.2 §2.4）。

### 4.2 审核通道的隔离（Claude 新增，已核实的事实）
- review-gate 以独立系统用户 `reviewgate` 运行，目的是让实现方（Claude 所在的用户）碰不到审核方。
- **如果审核用的 Chrome 开一个本机 TCP 调试端口，同机的任何用户（包括实现方）都能连上去，控制已登录的审核会话，三方隔离就会被绕过。** 所以 A 线的 CDP 连接**必须用 `--remote-debugging-pipe`**（或者同等的、只有 reviewgate 进程能访问的通道），**不能用端口**。
- 已核实：**chromiumoxide 0.9.1（2026-02-25 发布）没有 pipe 传输**。它总是加 `--remote-debugging-port` 启动，再走 WebSocket 连接（`src/browser/config.rs:379`、`src/conn.rs:36`）。所以 A1 必须验证"在 chromiumoxide 之外补一个 pipe 传输"是否可行（底层在 fd 3/4 上收发以 NUL 结尾的 JSON 消息），或者评估其它库。
- 已核实：chromiumoxide 的默认参数里有 `--enable-automation`（可能导致 Google 登录被拦）和 `--disable-dev-shm-usage`（v1.2 §5 P1 明确要求不要无条件启用）。A 线要用 `disable_default_args` 自己定义启动参数。
- 已核实的环境：Windows Chrome 154。Chrome ≥136 对默认用户目录同时禁用 port 和 pipe，**必须用独立的 `--user-data-dir`**。WSL 是 NAT 网络，访问不到 Windows 的 127.0.0.1。WSLg 可用。WSL 里有 Chrome for Testing。
- **待验证**：作为 systemd 系统服务运行的 `reviewgate` 用户，能否把有界面的 Chrome 显示到用户的 WSLg 桌面（需要访问 X/Wayland socket），方便人工登录和处理验证码。如果不行，就要改设计（例如登录阶段由用户在 reviewgate 的 profile 上手动完成，审核阶段 headless 运行）。

### 4.3 继承并补充的不可妥协点
1. 审核方和实现方必须是不同厂商；每次审核都用干净的上下文；审核的上下文、标签页和 profile 不和其它任务共享。
2. 门禁语义不变：强度分级、不静默降级、unavailable 不回退到 Claude、签名证明、CI 验签。审核证据绑定确切的 `base..tree`（现有机制已经保证）。
3. **审核拒绝**（有效的拒绝结论）和**通道不可用**（登录失效、超时、提取失败、格式无效）严格区分。后者绝不能记成通过，也不能悄悄回退到自审。VERDICT 缺失、重复、互相矛盾或被截断，都判为无效。
4. 断连或超时之后，执行状态未知的任务不得自动当作失败或通过，要明确标"未知"，再按规则重试或交给人。
5. 外部审核的回答一律只是数据，只解析 VERDICT，绝不当作指令执行。
6. 证据优先：NOT RUN ≠ PASS；关键修复做变异验证；Mock 不能代替真实 Chrome 测试。
7. 网页通道必须有明确的合规评估和停止机制（4.1）。

## 5. 阶段（详见 `plans/ROADMAP-unified-browser-runtime.md`）

A0 决策与合规 → A1 最小 CDP 试验（spike）→ A2 审核竖切 → A3 并行试运行与故障测试 → B1 webtest 第二后端 → B2 按数据补齐能力与生产化。

每个阶段都有自己的任务书（v1.2 §7 模板），要经用户确认才能开始。实现按 review-gate 三方流程走（每个版本由外部 AI 审核并签名）。

## 6. 迁移影响

- review-gate：只新增一个通道（暂名 `web-native`，带站点参数），门禁不变；新通道的强度先和 web-gemini 一样定为 3，以后是否调整需要单独写 ADR。
- webtest：新增后端接口，默认仍是 `McpBackend`；flow 格式（`role + name + nth`）不变。
- DSH：不受影响，web-gemini 扩展通道继续服务它自己的任务。
- 回退：A 线随时可以切回现有的 web-gemini 通道（或 external-api），B 线随时可以切回 `McpBackend`。

## 7. 用户决定（2026-10-11）

- **方案 C：确认。**
- **外部 AI 审核：改用 Gemini API**（review-gate 现有的 `gemini-api` 通道，external-api 强度 4；auto 模式下它排第一，web-gemini 保留作回退）。不做 ChatGPT 网页通道；OpenAI 不接入。
- **A1：开始。** 试验站点为 **Gemini 网页**：现有扩展通道继续服务 DSH，A1 验证 Native Runtime 能否取代它那套“扩展 + 桥”。条款与账号风险由用户承担；碰到验证码或拦截就停下（4.1）。

### 原列出的待决事项（存档）

1. 是否确认采用方案 C 和本 ADR 的边界。
2. **合规取舍（4.1）**：OpenAI 用 API 还是网页？Gemini 网页通道继续用、改造，还是逐步换成 API？用专门账号还是现有账号？
3. A1 的第一个站点：在合规取舍之后确定（如果 Gemini 网页通道继续用，建议先做 Gemini；如果不继续，A 线的意义就要重新评估）。
4. 是否接受 A1 里"自己补 pipe 传输"可能带来的工作量（这是隔离的前提，见 4.2）。
