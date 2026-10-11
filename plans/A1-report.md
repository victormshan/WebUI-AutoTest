# A1 报告：最小 CDP 试验（spike）

- 日期：2026-10-12
- 环境：WSL2 Ubuntu（WSLg 可用），Chrome for Testing 154.0.8037.92（`~/.local/bin/chrome-wsl`），Rust 1.99
- 代码：`crates/browser-runtime`（`604c1f4` + 本次提交的 examples）
- 站点：Gemini 网页（gemini.google.com），专用账号，专用 profile `~/.local/share/browser-runtime/profiles/gemini-a1`

## 逐项结论

| # | 项目 | 结论 | 证据 |
|---|---|---|---|
| S1 | crate 骨架 | **PASS** | 4 个单元测试；clippy `-D warnings`、fmt 均通过；README |
| S2 | pipe 传输 | **PASS** | 自写最小 CDP 层（fd 3/4，NUL 分隔 JSON），命令、事件、flatten session 都可用；进程组内**没有 TCP 监听**（按 /proc 核对 socket inode）。chromiumoxide 0.9.1 不支持 pipe，所以没有用它 |
| S3 | 专用 profile、显式参数 | **PASS** | 不带 `--enable-automation`、`--no-sandbox`、`--disable-dev-shm-usage`；拒绝调试端口参数；profile 目录 0700；有界面和无头两种模式都验证过 |
| S4 | 人工登录 | **PASS（有条件）** | **带 pipe 调试启动时，Google 拒绝登录**："This browser or app may not be secure"。**不带任何调试参数**以普通方式启动同一个 profile 时，登录成功，Gemini 自报为 3.6 Flash。之后用 pipe 重新启动，登录态保留。⇒ 流程定为"首次人工登录不带调试参数，之后由 Runtime 接管" |
| S5 | reviewgate 用户能否显示窗口 | **未执行**（不是 PASS） | 基于新设计（审核阶段无头），**不再是运行阶段的必要条件**；但首次登录要由 reviewgate 身份人工完成（profile 不跨用户转交），所以“reviewgate 能否显示一个普通窗口”**仍是 A2 部署前必须验证的项** |
| S6 | 真实输入与读取 | **PASS** | 用 pipe 接管已登录 profile，4 轮全部成功（有界面 3 轮、**无头 1 轮**）；一次性随机数答案全部匹配（九五三、八七一、一五一、三二八）；从回车到完成 5.6–6.6 秒。输入用 `Input.insertText`，发送用 `Input.dispatchKeyEvent` 回车（isTrusted）；全程**没有调用 `Runtime.enable`** |
| S7 | 完成判定与提取的原始数据 | **PASS（已采集）** | 时间线（第 1 轮）：回车后 +1.3 秒输入框清空、出现"停止"按钮；+2.9 秒出现用户气泡；+3.5 秒地址变为 `/app/<id>`、出现回答节点；+4.0 秒有文本；+4.5 秒"停止"按钮消失；文本连续 3 次采样不变（+5.6 秒）即判定完成。提取只取最后一个 `model-response` 里的 `message-content`，**没有混入建议按钮文字**（对照 web-gemini 曾出现的提取污染） |
| S8 | 进程回收 | **PASS** | 正常关闭、Chrome 崩溃、drop 都不留残留；**Runtime 被 kill -9 时，Chrome 因 pipe 收到 EOF 在 1 秒内自行退出**。注意：`chrome_crashpad_handler` 跑在另一个进程组，按进程组回收覆盖不到它，但浏览器退出后它会随之退出（每轮实测残留都是 0） |
| S9 | 异常 | **PASS** | profile 被占用 → `Launch` 错误（S6 准备阶段实际遇到过：普通模式 Chrome 关掉窗口后仍在后台运行，占着 profile）；命令超时 → `Timeout`（注明效果未知），之后浏览器仍可用；崩溃 → `BrowserDisconnected`；路径错误 → `Config` 错误 |

## 变异验证
- 去掉 `setsid`：最初会挂起。由此发现一个真实缺陷：`Drop` 在进程组信号送不到时，阻塞的 `wait()` 会永久卡住。已修复（同时直接 kill 主进程），并给每个集成测试加了 60 秒上限。修复后，这个变异会在约 4 秒内干净地失败。

## 对 ADR-0 / 后续阶段的影响
1. **登录流程（新增约束）：** 首次登录必须不带调试参数，由人工完成；之后由 Runtime 带 pipe 接管。登录态失效时，Runtime 必须识别出来（S6 的探针用 `signin` 信号判断），报告"通道不可用：需要人工重新登录"，不能误判为审核结果。
2. **审核阶段用无头模式。** 系统服务不需要显示窗口；S5 只影响首次登录的操作方式（例如由用户以 reviewgate 身份运行一次普通模式登录，或者把登录好的 profile 转交给 reviewgate，后者需要验证 cookie 能否迁移）。
3. **完成判定：** "新回答节点出现 + 停止按钮消失 + 文本连续稳定"在 4 轮中都可靠。A2 要补充：总超时、登录失效、限流和拒答页面的识别，以及长提示（分段）。
4. **自写 CDP 层可行。** 目前约 250 行，覆盖 A 线需要的命令和事件。B 线是否要更完整的库，留到 B1 再评估。
5. **条款与检测：** 登录环节会识别调试连接，已登录会话的使用目前没有被拦；这一点可能随时变化。A3 并行试运行时要持续观察，碰到拦截就停下，交给用户处理。

## 补充（2026-10-12，复核时发现）：网页通道被 Google 风控
- 为留存“无 TCP 监听”证据，又跑了无头探针：**第 5 轮页面始终没有就绪；第 6、7 轮被重定向到 `https://www.google.com/sorry/index?continue=https://gemini.google.com/app…`（Google 异常流量验证页）**。此前约 25 分钟内共跑了 5 轮自动探针（第 1–4 轮成功）。
- 按 ADR-0 §4.1 **立即停止**，没有重试或绕过。探针已改为识别这个页面并报 `blocked-sorry`。
- 含义：网页通道不仅在**登录**时会识别调试连接，**使用一段时间后也会触发风控**。所以 S6 的结论应修正为：**功能可行，但频率与可持续性未得到证明**，以低频的人工监督为前提。这直接影响 A2 是否值得做、以及以什么定位来做。
- 证据：`plans/A1-evidence/`。

## 验收决定
- ChatGPT 评审（2026-10-12）建议：**PASS WITH CONDITIONS**。理由：技术可行性已证明；合规、profile 所有权和完成判定的可靠性尚未证明。
- **用户决定：待确认**。在这条补充（风控）之后，用户需要在“进入 A2（低频回退定位）”和“A 线暂停、审核以 Gemini API 为主”之间做选择，见 `plans/compliance-gemini-web.md`。

## 结论建议
**PASS WITH CONDITIONS，进入 A2。** 条件：
- 首次登录流程（S4）写进运行手册；
- A2 实现登录失效和拦截的识别，碰到就停（不做反检测）；
- S5 在 A2 部署设计时解决（profile 归 reviewgate 所有）。

由用户决定。
