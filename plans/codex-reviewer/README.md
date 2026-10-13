# Codex 审查者接入 agent-bridge（2026-10-13）

## 决定（用户，2026-10-13）
- 本机的 Codex CLI 作为 agent `codex` 接入 agent-bridge，承担每个阶段的审查。以前是用户手动把交流文档转给网页版 ChatGPT，现在由 Codex 代替。
  - Codex 位置：`E:\Tools\npm`，版本 0.162.1，用户的 ChatGPT Plus 账号登录。
  - 工作目录：`E:\rust-agent-browser\reviews`。
- **Codex 的结论权限等同于以前 ChatGPT 的结论**：Codex 明确同意即视同用户同意。sudo、登录、合并 main 仍然要用户本人决定。
- **Codex 不做 review-gate 的审核通道**：review-gate 保持 Gemini API 为主、web-gemini 为回退。
- 不自动操作 chatgpt.com 网页：那样违反 10-12 的合规选择（a），也违反 OpenAI 条款。Codex CLI 是官方提供的程序化入口。

## 设计
- `crates/agent-bridge/clients/codex/codex-worker.py`（Python，只用标准库）用 codex 自己的 token 连 bridge，处理流程：
  1. 发给 codex 的任务：先回 `ack`；
  2. 运行 `codex exec -s read-only`，任务标题和正文从 stdin 输入；
  3. 回 `result`：成功是 `done`，正文是 Codex 的回答，第一行为 `VERDICT: APPROVE | APPROVE WITH CONDITIONS | REWORK | BLOCK`；失败是 `blocked`，正文写明错误。
- 收到 `verdict: rework`：用整个对话重新审查（先回 `progress`，因为 Working 状态不允许 ack）。
- **为什么材料要全部放进任务正文**：Codex 在 Windows 上的只读沙箱会拒绝一切命令，包括读文件。实测过 `C:\…`、`\\wsl.localhost\…` 和 `E:\rust-agent-browser` 三个位置，都被拒绝。所以要审查的内容必须都在任务正文里，由 worker 从 stdin 交给 Codex。好处是 Codex 不运行任何命令，也改不了任何文件。
- **崩溃恢复**：bridge 有"隐式已读"：codex 一发 ack，任务消息就算已读了。如果 worker 在 ack 之后、出结果之前崩溃，只看收件箱就再也找不到这个任务。所以每轮还会扫描任务列表，把发给 codex、状态为 acked 或 working（即还欠结果）的任务重新审查。每条回复都带由所回复消息推导出的 `client_msg_id`，重试不会重复发。
- 部署：
  - systemd 用户服务 `codex-worker.service`（模板在 `crates/agent-bridge/deploy/`）；
  - token 在 `~/.config/agent-bridge/codex.token`（0600），哈希加进 `agents.json`（修改前已备份）。
  - **重启 agent-bridge 时，bridge 上与 DSH 的未结任务为 0。**

## 证据
- `tests.txt`：5 个测试，都跑在**私有** bridge 服务上（独立的状态目录和 agents 文件，显式传入空的 `--notify`，不会碰到真实的 DSH），Codex 是假的：
  - 正常审查；
  - Codex 失败时回 `blocked`；
  - rework 后用整个对话重审；
  - 已经答复过的任务，重启后不会再答一次；
  - ack 之后崩溃的任务，重启后补审。
- `mutation.txt`：
  | 变异 | 结果 |
  |---|---|
  | W2 失败报成 `done` | 被抓到 |
  | W3 忽略 rework | 被抓到 |
  | W4 不扫描欠结果的任务 | 被抓到 |
  | W1 result 不带 `client_msg_id` | **没被抓到**：bridge 的状态机本身就拒绝第二个 result，补审扫描也只会处理还欠结果的任务。`client_msg_id` 只对"请求已经写入但响应丢了"的重试有用，这种情况没有测试 |
- 首轮写 W1 时还发现了上面那个"ack 之后崩溃就再也找不到任务"的真实缺陷：当时的"重启不重复答复"测试因为隐式已读而形同虚设。已经修复，加了 W4 和对应的测试。
- `e2e-codex-e2e-1.json`：用真实 Codex、真实 bridge 和 systemd 服务联调。从发出任务到 Codex 的 result 回到 bridge，约 34 秒。Codex 给出 `APPROVE WITH CONDITIONS`，把材料里无法核实的地方都标了"未核实"。它的两条阻断意见在 bridge 的设计里不成立，我在 verdict 里逐条说明了原因，然后结案。

## 固定审查线程（2026-10-13，用户选"按建议做"）
- worker 不再每次新开会话，所有审查都在**同一个 Codex 线程**里进行（`codex exec resume <id>`，用 `-c sandbox_mode="read-only"` 保持只读）。Codex 因此记得之前各阶段、各轮审过什么、提过什么条件。
- 同一任务返工时，如果上一轮就是在这个线程里审的，只发新的那条消息；否则（新线程，或线程恢复失败后另开了一个）发整个任务对话。
- 线程 id 保存在 `~/.local/state/codex-worker/threads.json`，每次审查的目录里有 `thread.txt`，每条审查结果末尾也写明线程 id。**用户想当面讨论时，在自己的 Codex 窗口运行 `codex resume <id>`，就进入同一个线程**。审查正在进行时请不要同时打开它，以免两个进程同时写同一个线程。
- 实测（2026-10-13）：恢复后的线程记得之前对话里的内容（暗号测试），`-c sandbox_mode="read-only"` 下命令仍然全部被拦截，线程 id 不变。
- 测试（假 Codex，现在共 7 个）：后续审查接着同一线程；同一线程里返工只发新消息；线程恢复失败时另开新线程并发完整材料；结果末尾带线程 id。变异 T1–T5 全部被抓到（`mutation.txt` 末尾）。
- **真实联调发现并修复了一个缺陷**：`codex exec resume` 不会像 `exec` 那样把 stdin 附加到提示词参数后面。第一次联调 2/2 时，Codex 恢复了线程却没收到材料（回复"没有提供 <stdin> 审查材料"，见 `e2e-thread-2.json` 消息 3）。修复方法：恢复时提示词用 `-`，把规则提醒和材料一起从 stdin 输入。假 Codex 也改成同样的行为（恢复时只有 `-` 才读 stdin），变异 T5（改回旧写法）会让 2 个测试失败。
- 修复后的真实联调（`e2e-thread-1.json`、`e2e-thread-2.json`）：任务 2 是另一个任务，在同一线程里返工时只发了新消息，Codex 答出了任务 1 的标记词"琥珀灯塔"和函数名 `clamp`。

## 当面审查（共享目录，按需使用）
需要 Codex 亲自核实源码，或者用户想当面讨论时，用这个方式。它用的是用户自己的 Codex 窗口，那个窗口有完整权限：
1. Claude 把材料写进 `E:\rust-agent-browser\reviews\<阶段>\`：
   - `request.md`：审查范围、任务书路径、**源码仓库路径**（`\\wsl.localhost\Ubuntu\home\administrator\projects\Chrome-MCP`）、基线提交、当前树哈希、重点问题；
   - `changes.diff`：本阶段完整 diff，包含新增文件；
   - `evidence.md`：实际执行过的命令、结果、原始日志路径和已知限制。
2. 用户在 Codex 窗口发一句："审查 reviews/<阶段>，结论写到该目录的 verdict.md，不要修改源码仓库。"
3. Claude 在后台等 `verdict.md` 出现，读取后返工。
4. **完整性核对**：那个窗口有完整权限，能改被审的代码。所以 Claude 在交出材料前记下 `HEAD`、`git status --porcelain` 和树哈希，拿到 verdict 后再核对一遍。有任何变化就报告给用户，这份 verdict 不作为审查结论使用。
5. 这种方式的结论权限与 worker 的审查相同（等同 ChatGPT），但要用户每次发起，不是全自动。

## 限制
- Codex 没有用户以前和网页版 ChatGPT 的对话上下文。固定线程之后，它保留的是本线程里之前各次审查的上下文。
- 使用用户 Plus 账号的额度。
- 依赖 WSL 互操作调用 Windows 上的 codex.exe：用户服务里实测可用。
