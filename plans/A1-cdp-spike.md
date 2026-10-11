# A1 — 最小 CDP 试验（spike）任务书

状态：**进行中**（A0 已完成：ADR-0 已接受；SITE = Gemini 网页 gemini.google.com）。

## 1. Purpose
在**本机真实环境**里验证 A 线的技术前提能不能成立，用证据回答"能 / 不能 / 卡在哪里"，**不交付审核功能**。

## 2. Dependencies
A0 已确认；第一个站点已选定（下文记为 SITE），并且用户已同意以人工方式登录专用账号。

## 3. Scope（必须验证的项目）
| # | 项目 | 判定 |
|---|---|---|
| S1 | 在现有 workspace 新建 `crates/browser-runtime`（P0 骨架：配置加载、错误类别、tracing 初始化、测试布局、README） | `cargo fmt --check` / `clippy -D warnings` / `test` 全部通过 |
| S2 | **pipe 传输**：用 `--remote-debugging-pipe` 启动 Chrome，在 fd 3/4 上收发 CDP 消息（以 NUL 分隔的 JSON），能执行命令、能收到事件。评估两条路：给 chromiumoxide 接自定义传输，或者写一个最小的自有适配层 | 能执行 `Browser.getVersion`、`Target.createTarget`，能收到 `Target.targetCreated` 事件；全程**没有监听任何 TCP 端口**（用 `ss -ltnp` 取证） |
| S3 | **专用 profile**：独立的 `--user-data-dir`；自定义启动参数（**不带** `--enable-automation`，**不无条件带** `--disable-dev-shm-usage` / `--no-sandbox`） | 启动参数清单留档；重启后 profile 仍有效 |
| S4 | **人工登录**：在专用 profile 里人工登录 SITE，退出后再启动，登录态仍在 | 结论为 PASS，或给出被拦截的现场证据（页面截图、提示文字） |
| S5 | **reviewgate 用户能否显示窗口**：以 `reviewgate` 身份（或模拟同等权限）运行有界面的 Chrome，窗口能出现在 WSLg 桌面 | PASS；如果 FAIL，记录原因并给出替代方案（例如：登录阶段由用户人工完成，运行阶段 headless） |
| S6 | **真实输入与读取**：在 SITE 新开对话 → `Input.insertText` 填入一段**无害的固定提示** → 发送 → 条件等待 → 读出回答 | 回答与预期一致（用一次性随机数做校验，判据参照 web-gemini 探针的做法） |
| S7 | **完成判定与回答提取的原始数据**（已知现场：2026-10-11 web-gemini 审核 adr-0 时，第 3 段回答混入了页面建议按钮文字“Want me to check consistency in other parts of the plan?Yes”，说明提取边界不干净）：记录"回答节点出现 / 文本稳定 / 生成中标志 / 错误或验证码页面"等信号的时间线 | 时间线留档（用于 A2 设计完成判定，A1 不定阈值） |
| S8 | **进程回收**：正常关闭、Runtime 被 kill、Chrome 崩溃三种情况下，检查残留进程 | 三种情况的 `ps` 证据；有残留就记录平台限制，并提出回收方案（例如进程组） |
| S9 | **异常**：Chrome 路径错误、profile 被占用（SingletonLock）、pipe 断开、命令超时 | 每种情况都有明确的错误类别，不 panic、不挂起 |

## 4. Non-goals
不接 review-gate；不做站点适配器的产品化；不做 AXTree、Locator、Recovery、MCP、Agent；不定性能目标；不做任何反检测或验证码绕过；不碰 DSH 的 web-gemini 扩展通道。

## 5. API / Data Contracts
只定内部最小接口，例如 `Browser::launch(config)`、`Browser::new_page`、`Page::navigate`、`Page::insert_text`、`Page::wait_for(cond, timeout)`、`Page::read_text(selector)`、`Browser::close`。错误类别取 v1.2 §3.4 中用得到的子集。A2 之前允许修改。

## 6. Concurrency / Cancellation
单 Chrome、单页面、串行。所有等待都有超时；取消时释放页面并关闭 Chrome。不得在持锁期间 await CDP。

## 7. Security
- 不监听任何调试端口（S2 取证）；
- profile 目录权限 0700，只属于运行用户；
- 日志不记录 Cookie、提示词全文和回答全文（只记长度和哈希）；
- 提示内容固定且无害，不包含仓库代码。

## 8. Tests
- L0 构建；
- L1：pipe 消息编解码、错误分类；
- L2：真实 Chrome（S2–S6）；
- L3：S8、S9 故障注入；
- L4、L5：N/A。

## 9. Acceptance Criteria
S1–S9 每一项都给出 **PASS / FAIL / BLOCKED** 和证据（命令、环境、输出、截图）。允许 FAIL 或 BLOCKED，前提是写清原因和对 A2 的影响。**A1 的目的是得到可信的结论，不是全部 PASS。**

## 10. Deliverables
`crates/browser-runtime`（最小实现与测试）、`plans/A1-report.md`（逐项结论与证据、对 ADR-0 的修订建议）。代码按 review-gate 三方流程提交。

## 11. Known Limitations（预期）
站点改版会让选择器失效；登录与验证码依赖人工处理；条款风险由用户承担和决策。

## 12. Exit Decision
PASS（进入 A2）/ PASS WITH CONDITIONS（列出条件）/ REJECT（A 线改用 API 或维持现状）。由用户在看完 A1 报告后决定。
