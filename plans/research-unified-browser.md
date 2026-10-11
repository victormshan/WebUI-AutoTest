# 统一浏览器方案研究材料

**用途**：交给 ChatGPT，与《Rust Agent Browser Runtime (Chrome) 项目架构与开发治理方案 v1.2》（下称"Runtime 方案"）一起研究。目标是形成一个**和谐统一**的方案，把下面几件事收进同一套设计：Runtime 方案、现有 webtest、review-gate 的外部 AI 审核通道（web-gemini，以及计划中的 web-chatgpt）。

整理者：Claude Code（本仓库的实现方）｜日期：2026-10-11｜仓库：WebUI-AutoTest（Rust workspace）

> 本文只写**已经核实的事实**和明确标注的**设想**。凡是"未验证"的地方都会写明。文中不含任何凭据、token 或账号信息。

---

## 0. 请 ChatGPT 做什么

1. 读完本文和 Runtime 方案 v1.2。
2. 判断一个统一方案能否**同时**满足两条主线：
   - **A. 外部 AI 审核**：review-gate 通过网页版 Gemini / ChatGPT 审核代码；
   - **B. 网页自动化测试**：取代或增强 webtest 现在的 chrome-devtools-mcp 后端。
3. 给出统一方案的建议，包括分层、阶段划分、各自的第一个使用方，以及与现有组件的迁移关系。逐条回答第 7 节的问题。
4. 输出格式见第 8 节。凡是需要实测才能下结论的，请标为"需验证"，不要假定。

---

## 1. 参与方与协作方式

| 角色 | 是谁 | 职责 |
|---|---|---|
| 用户 | 人 | 最终决策：部署、合并、重启授权、账号与风险取舍 |
| Claude Code | Anthropic Claude（WSL 里的 CLI） | 实现本仓库代码；审核 DSH 的运行时代码（协议 P8） |
| DSH 主 agent | 另一套 AI agent（Windows 宿主 + 插件 dsh-web-relay） | 运营 web-gemini 通道、Chrome 扩展与本地桥接；它自己的任务也要用网页 AI |
| 外部审核 AI | Gemini 网页版（现有）、ChatGPT 网页版（计划） | 审核 Claude 实现的每一个版本，必须和实现方是不同厂商 |

协作规则（与 Runtime 方案 §8/§9 的精神一致）：
- **三方协议**：实现方 → 外部 AI 审核 → 用户验收。版本能不能提交由 review-gate 决定，不由实现方决定。
- **P8**：DSH 的运行时代码改动必须先把 diff 送 Claude 审，审过才能交付。
- **证据优先**：测试全过 ≠ 通过验收；未执行的不写 PASS；关键修复要做**变异验证**（故意改回错误实现，确认测试会失败）。本周多个缺陷就是靠变异验证或实测抓出来的，见 §3.4。
- Claude 与 DSH 通过 **agent-bridge**（本仓库的服务，127.0.0.1:7879）互派任务、送审、回报。

---

## 2. 运行环境（已核实）

| 项 | 事实 | 影响 |
|---|---|---|
| 主机 | Windows（DSH 宿主与日常 Chrome 运行在 Windows） | — |
| 开发环境 | WSL2 Ubuntu；本仓库、review-gate、agent-bridge 都跑在 WSL | — |
| WSL 网络 | 无 `.wslconfig`，默认 **NAT** 模式 | WSL **访问不到** Windows 的 127.0.0.1；agent-bridge 推送给 DSH 时只能借道 Windows `curl.exe` |
| WSLg | 可用（`DISPLAY=:0`） | WSL 里的 Chrome 可以显示有界面的窗口，用户能在里面手动登录、处理验证码 |
| Windows Chrome | 154.x | Chrome 136 起，**默认用户目录不允许开远程调试**，无法用 CDP 接管日常已登录的浏览器，必须用独立 profile |
| WSL Chrome | Chrome for Testing（`~/.cache/chrome`），webtest 使用 | 能否在其中正常登录 Google / OpenAI：**需验证** |
| Rust | 1.99，edition 2024；workspace 依赖 tokio、rmcp、reqwest、clap、tracing | — |

---

## 3. 现有组件

### 3.1 webtest（网页自动化测试，已可用）

```
webtest (Rust) ──stdio/MCP──► chrome-devtools-mcp (Node) ──CDP──► Chrome (headless)
      └──► Claude（Messages API 或本机 claude -p）
```

- crate：`mcp-client`（启动 chrome-devtools-mcp、调用工具）、`llm`（Llm trait，两种后端）、`agent`（观察→决策→执行循环、快照压缩、语义定位器、安全拦截、诊断、flow、回放、自愈）、`cli`（`webtest` 命令）。全仓 Rust 约 1.76 万行（含 review-gate 与 agent-bridge）。
- 能力：
  - `run`：给目标，agent 读无障碍快照自主操作，输出 PASS/FAIL、证据、console 错误、失败请求，可保存为 flow；
  - `replay`：按 flow 确定性回放，不调用大模型；定位失败时才调用大模型自愈（结果标 PASS (healed)，断言失败和新错误**不**自愈）；
  - `explore`：广度优先自动探索站点、批量生成并验证用例；
  - `login`：保存/注入 Cookie（含 HttpOnly，经 CDP `Storage.getCookies/setCookies`）与 localStorage；
  - JUnit 报告、GitHub Actions 工作流。
- flow 里的定位器与 uid 无关：使用 `role + name + nth`，回放时在最新快照里解析。
- **实际用到的 chrome-devtools-mcp 工具**（约 13 个）：`navigate_page`、`take_snapshot`、`click`、`fill`、`fill_form`、`hover`、`press_key`、`wait_for`、`evaluate_script`、`list_pages`、`list_console_messages`、`list_network_requests`、`handle_dialog`。
- 每个 flow 用一个全新的浏览器（临时用户目录），用例之间互不影响。

### 3.2 review-gate（独立审核门禁，已上线）

- 以独立系统用户 `reviewgate` 运行（127.0.0.1:7878）。实现方没有权限写它的存储和签名密钥。
- 流程：实现方 `review run` → 门禁对 `base..tree` 取 diff → 请外部 AI 审核 → 记录结果 → 审核通过后，实现方 commit + tag → `record` → 门禁签发 ed25519 证明并写成 git note → CI 用固定公钥 `verify-range` 验签，未签名的代码提交无法进 main。
- **审核通道与强度**（`record` 时强度必须 ≥ 任务门槛，默认门槛是 web-gemini；不够就暂停交给人，绝不静默降级）：

  | 通道 | 强度 | 说明 |
  |---|---|---|
  | manual | 5 | 人工裁决 |
  | external-api | 4 | Gemini API / OpenAI 兼容 API（已实现，目前没配 key） |
  | web-gemini | 3 | 经 DSH 的 Chrome 扩展桥接（现在实际在用的通道） |
  | claude-subagent | 2 | 同厂商新上下文 |
  | self-review | 1 | 自审 |

- 没有外部 AI 可用 → 报 `unavailable`（退出码 3），不回退到 Claude；连续 3 次拒绝 → 熔断。
- **长 diff 分段审**：网页 Gemini 对长提示不稳定（实测 35KB 每次失败，10KB 偶尔失败），所以按整文件把 diff 切块，逐块审，任一块被拒则整体拒。
- 最近的变更：每次审核都**新开对话**（`newConversation:true`），原因见 §3.4 第 6 条。
- 也被 webtest 用作"交叉审核"入口：`review-gate ask`（stdin 输入提示，stdout 输出回答）。

### 3.3 agent-bridge（Claude ⇄ DSH 任务通道，已上线 v2.8）

只作为背景：它是一个任务状态机服务，负责送审、回报、nudge 催办、记录用户决定，推送唤醒 DSH，Claude 一侧用长轮询加 Claude Code 的 asyncRewake hook 收消息。**它与浏览器方案无直接耦合**；如果审核通道换了实现，它不需要改动。

### 3.4 web-gemini 通道（DSH 维护，现网在用）

架构：
```
调用方（review-gate / DSH 自己的任务 / E2E 探针）
   │ HTTP（token），/create-task → 轮询 /task-result
   ▼
bridge-server.mjs（Node，Windows，127.0.0.1:8899，watchdog 守护）
   ▲ 扩展的 service worker 每秒轮询 /next-task
   │
Chrome 扩展 dsh-web-gemini-ext（MV3：background.js + content.js）
   │ chrome.tabs.sendMessage
   ▼
gemini.google.com 标签页里的 content script：填输入框 → 发送 → 等回复 → 取回答
```

本周（v0.4.4 → v0.5.12）暴露并修掉的问题，**每一条都与"扩展 + 本地桥"这种架构直接相关**：
1. **新开对话"点不动"**：在已有对话里，扩展用合成事件点击"新对话"按钮，五连鼠标事件、键盘、原生 `el.click()` 全部无效。结论是 Gemini 的处理链只认 `isTrusted` 的真实事件。最后改成 `chrome.tabs.update` 整页导航到 `/app`。
2. **content script 失联**：重载扩展后，已打开页面里的旧脚本变成"孤儿"，投递返回 NO_RESPONSE，必须人工刷新标签页。
3. **版本错配看不见**：background 与 content 各自加载，必须加构建标记，并用测试守住"标记 = manifest 版本"。
4. **MV3 环境差异**：一版代码在 service worker 顶层读了 `process.env`，测试的沙箱补了 `process` 桩，结果测试全过；一旦交付，扩展后台会整个崩溃。是审核时在"按生产形态加载"的沙箱里复现出来的。
5. **跨进程超时链**：content → background → bridge 三层超时必须层层递增（长提示的服务端超时最长约 368 秒）。一次改动把它截成 120 秒，长审核会被误判失败。
6. **共用标签页**：review-gate 的审核任务和 DSH 的任务、探针抢同一个标签页，导致冲突，审核提示词还会接在别人的对话后面，影响审核独立性。修法是两条：门禁每次新开对话；bridge-server 在有任务 processing 时不再派新任务（排队）。
7. **排队 × 旧超时**：排队之后，等待中的任务被"pending 60 秒无人认领"这条旧规则误杀。夹具全绿，是实测才暴露出来的。修法是 pending 窗口只统计通道空闲的时间。
8. **人工依赖**：每次改 content/background 都要用户手动"重载扩展 + 刷新标签页"。

现状：v0.5.12 稳定，两轮并发实测全部成功（最长排队 137 秒）。但这套架构的**维护面**（扩展 + 桥 + watchdog + token + 跨进程超时 + 人工重载）明显偏大。

### 3.5 web-chatgpt 设想（用户已选网页版，尚未开工）

已讨论的方案 B：另做一个 `dsh-web-chatgpt-ext`，并排运行；复用同一份 `bridge-server.mjs`，另起一个实例监听 8898，Gemini 那条完全不动；review-gate 新增 `web-chatgpt` 通道（OpenAI 家族，强度与 web-gemini 同级），先作为 Gemini 不可用时的备用。
- 页面要点：输入框是 contenteditable；判断回答结束靠"停止生成"按钮消失 + 文本稳定；新对话用 `chatgpt.com/?temporary-chat=true`（临时对话不读写记忆和自定义指令，保证审核独立）。
- 风险：服务条款禁止程序化提取输出（有封号风险）；Cloudflare 人机验证；改版频繁；账号套餐的消息额度。
- **它会把 §3.4 的全部维护面再复制一份**，这正是想先研究统一方案的原因。

---

## 4. Runtime 方案 v1.2 要点与 Claude 的评审意见

Runtime 方案的核心：用 Rust 直连 CDP，管理 Chrome 生命周期和 Tab/Frame/Navigation/Locator/Wait/Interaction；MCP、Agent、调度都作为可选的上层；分阶段（P0–P15）、按等级验收（L0–L5），证据优先，偏离基线要先写 ADR。

Claude 认同的部分：先定契约再抽象；NOT RUN ≠ PASS；明确进程所有权与进程树回收；把网页内容视为不可信数据；性能目标先测量再定；不承诺反检测。

Claude 提出的补充与修改：
1. **缺 ADR-0（动机与选型）**：方案没有说明为什么要做，也没提和现有 webtest / chrome-devtools-mcp 的关系。webtest 的 `agent` crate 已经实现了方案 Phase 4/5/7/8/10 的大部分能力（快照压缩、语义定位、自愈、agent 循环）。
2. **先打通一条竖切，再逐层加深**：P2–P8 横向全做完才有第一个使用方，风险很大。应该在 P1 之后马上做一个最小竖切，接到一个真实使用方上，用数据决定后续。
3. **Phase 11–13（持久化、队列、Cron、通知）** 与现有 agent-bridge、DSH 调度重叠，建议标成本项目范围之外。
4. 技术点应写进对应阶段：Chrome ≥136 的独立 profile 限制；WSL NAT 网络；跨域 iframe 要用 `Target.setAutoAttach`（flatten）为每个跨域 iframe 单独建 session；节点引用统一用 `backendNodeId` 并检测失效；点击前滚动到可见并用 `DOM.getNodeForLocation` 检查遮挡；**CDP `Input.*` 事件是 isTrusted 的**（这正好解决 §3.4 第 1 条）；评估 chromiumoxide 的维护状态，缺口集中在 CDP 适配层补。
5. §9 的独立评审可以直接接 review-gate：每个 Phase 一个 review-gate 任务，评审依据是 diff + 证据文件；"证据不足"等同于 REJECT。

---

## 5. 统一方向：Claude 的初步设想（供批判）

### 5.1 主线 A：用 Runtime 替代"扩展 + 桥"做审核通道

```
review-gate ──(进程内调用)──► browser-runtime（Rust）──CDP pipe──► 专用 Chrome profile
                                  │                                （WSL + WSLg，有界面）
                                  ├── 站点适配器：gemini（/app）
                                  └── 站点适配器：chatgpt（?temporary-chat=true）
```

- 每次审核：新标签页 → 新对话 → `Input.insertText` 填提示 → 真实回车或点击发送 → 条件等待（停止按钮消失 + 文本稳定）→ 取最后一条回答 → 关闭标签页。
- 用 `--remote-debugging-pipe` 由 runtime 自己启动 Chrome：**不开任何端口**，不受 WSL NAT 影响。
- 只需要 Runtime 方案的窄子集：P0、P1，最小的 P2/P3/P5（CSS）/P6/P7。不需要 AX 引擎、MCP、Agent、调度。
- review-gate 新增通道（暂名 `web-native`，带上站点：gemini / chatgpt），先和现有 web-gemini **并行跑**，比较成功率，再决定切换。
- §3.4 的第 1–8 条大多会消失：真实输入、没有扩展、没有跨进程桥、并发和超时都在一个进程里、审核用专属 profile 天然隔离。
- web-chatgpt 只是一个适配器，不用再复制一套扩展和桥。
- **DSH 自己的 web-gemini 用途不受影响**，继续用扩展；是否迁移以后再说。

**未验证的风险（需要先做小试验 spike）**：
- 专用 profile 里能否正常登录 Google / OpenAI（Google 可能拦截自动化启动的浏览器）。设想的办法：先不带调试参数手动启动该 profile 登录一次，再由 runtime 带 pipe 启动。
- CDP 连接是否更容易触发 chatgpt.com 的 Cloudflare 验证。方案明确**不做反检测**：遇到验证就停下，交给用户在 WSLg 窗口里处理。
- 账号与服务条款风险和扩展方案一样，建议用专门的账号。
- 页面选择器的维护成本不会因为换方案而消失。

### 5.2 主线 B：Runtime 作为 webtest 的第二个后端

- 在 `agent` crate 下抽出浏览器后端接口：`McpBackend`（现有）和 `NativeBackend`（Runtime）。
- NativeBackend 需要：带稳定 uid 的 AX 快照，按 uid 执行 click/fill/fill_form/hover/press_key，wait_for，evaluate_script，list_pages，console / 网络请求列表（Log、Network 域），对话框（`Page.javascriptDialogOpening`），以及 Cookie / localStorage 的读写。这几乎覆盖 Runtime 方案的 P1–P7，外加观察类能力。
- 用 `fixtures/shop` 及其变体（按钮改名、金额 bug、新增 console 错误）在两个后端上各跑一遍，对比成功率、耗时、自愈次数。**数据明显更好才切换**，否则保留 chrome-devtools-mcp（它由 Chrome DevTools 团队维护，比较成熟）。

### 5.3 两条主线的关系（Claude 的建议）

- **共用同一个 `browser-runtime` crate**：A 是第一个使用方，覆盖面窄、收益明确；B 是第二个使用方，逐步加深。
- 共用部分：进程与 profile 管理、Target/Frame 生命周期、导航、等待、真实输入、错误分类、tracing。
- 差异部分：
  - A 用**专用持久 profile**（有登录态，有界面）；B 用**临时 profile**（无状态、可并发、headless）。
  - A 的定位用站点适配器里的 CSS 选择器；B 的定位用 AX 快照加语义定位。
  - A 的并发以"每站点一个标签页、串行"为主；B 要多浏览器并发。

---

## 6. 约束与不可妥协点

1. **审核独立性**：审核方必须和实现方不同厂商；每次审核用干净的上下文（新对话、临时对话）；审核通道不能和其它任务共享会话状态。
2. **门禁语义不变**：强度分级、不静默降级、unavailable 不回退到 Claude、签名证明、CI 验签。新通道只是 review-gate 里新增的通道，不改门禁本身。
3. **证据优先**：验收只认实际命令和输出；关键修复要做变异验证；真实浏览器的集成测试不能用 Mock 代替。
4. **安全**：不暴露 CDP 端口（优先用 pipe）；凭据和 Cookie 不进日志；网页内容视为不可信，回答文本只作为审核意见解析（只认 VERDICT 行），不能作为指令执行。
5. **不做反检测或绕过验证码**：碰到就停，交给人处理。
6. **最终决策归用户**：部署、合并、账号选择、条款风险由用户决定。

---

## 7. 请 ChatGPT 研究的问题

1. Runtime 方案能否作为 A、B 两条主线的共同底座？如果能，P0–P15 应该怎么裁剪、重排？哪些阶段属于 A 的最小集合，哪些属于 B？
2. A 的第一个竖切应该包括什么？退出条件和验收等级（L0–L5）怎么定？
3. "专用 Chrome profile + WSLg + `--remote-debugging-pipe`" 是否可行、安全？和"Chrome 跑在 Windows + WSL 开 mirrored 网络 + 调试端口"相比，取舍是什么？
4. Google / OpenAI 网页登录在 CDP 启动的浏览器里会遇到哪些已知障碍？有没有合规、可持续的做法（不做反检测）？
5. 网页版 Gemini / ChatGPT 做审核的可靠性：长提示上限（Gemini 实测 35KB 失败）、分段策略、回答完成的判定、拒答识别。有没有比"等停止按钮消失 + 文本稳定"更稳的完成判定？
6. B：自建 NativeBackend 相对 chrome-devtools-mcp 的真实收益和长期维护成本，是否值得？如果值得，AX 快照和 uid 的设计应当遵循什么原则才能和现有 flow 格式（`role + name + nth`）兼容？
7. chromiumoxide、其它 Rust CDP 库，还是自写 CDP 适配层：怎么取舍？
8. 现有 web-gemini 扩展通道应当保留多久？怎么设计 A 的灰度（并行双通道，按什么指标切换）？
9. 统一方案对第 6 节的约束有没有遗漏或冲突？
10. Runtime 方案里有没有应当删除、推迟或改写的部分，以便和本仓库现状和谐共存？

---

## 8. 期望的输出

请按下面的结构回复，便于 Claude Code 落成 ADR 和阶段任务书：

1. **结论**：统一方案是否可行；推荐的总体架构（一张图）。
2. **ADR-0 草案**：动机、选项、取舍、决定、迁移影响。
3. **裁剪后的阶段计划**：每个阶段的范围、非目标、第一个使用方、退出条件、验收等级。
4. **对第 7 节问题的逐条回答**，每条标注"有依据 / 需验证"。
5. **风险清单**：等级、触发条件、缓解措施、谁来决策。
6. **对 Claude 设想（第 5 节）的批评**：哪里错了，哪里过度设计，哪里遗漏了。
