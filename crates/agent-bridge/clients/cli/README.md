# Codex / OpenCode 双向协作通道

本分支将现有 agent-bridge 扩展为 Claude Code、DSH、Codex、OpenCode 可同时使用的任务通道。每个任务仍有一个 requester 和一个 receiver，服务继续强制检查角色、状态、参与方、用户决策和消息上限。没有重写或替换原有 Claude/DSH 协议。

## 新增能力

- `GET /v1/identity`、CLI `agent-bridge identity`、MCP `bridge_identity`：以服务认证结果核对当前身份并发现可派发的 agent，不返回 token。
- MCP `bridge_send`、`bridge_post` 支持可选 `client_msg_id`，重试可沿用同一个幂等键。
- `needs_user.relay` 增加 `codex` / `opencode`，老的 `claude` / `dsh` / `either` 和裸字符串保持有效。自定义 agent id 可省略 relay，此时由提出者负责转达。
- `clients/cli/bridge_agents.py`：保留式注册、原生客户端配置生成、临时 MCP 注入、可选后台 worker。`clients/cli/bridge_adapters.py` 把 Codex/OpenCode 的参数、配置、session 与流式事件解析隔在显式适配器接口之后。
- 注册不覆盖已有 token、身份或 notify 目标，不自动重启服务。旧 install.sh 也改为保留式合并，不再删除其他身份。

## 独立身份与原有 Codex 审查者

每个参与方必须使用自己的 token，不能复用 Claude 或 DSH token。原有 `clients/codex/codex-worker.py` 是专门的审查者，会消费 `codex` 收件箱；本次没有修改它。

如果已运行这个审查者，**为新的 Codex 协作客户端注册 `codex-collab`**，保留旧 `codex` 审查者。不要让两个独立消费者共用一个 agent 身份。下面以 codex-collab 和 opencode 为例；没有旧审查者时可自行使用 codex。

```bash
cd /home/administrator/projects/Chrome-MCP-bridge-agents
cargo build --release -p agent-bridge
python3 crates/agent-bridge/clients/cli/bridge_agents.py register --agent codex-collab
python3 crates/agent-bridge/clients/cli/bridge_agents.py register --agent opencode
```

注册只准备 `~/.config/agent-bridge/agents.json` 和对应 `<agent>.token`（0600），输出只有路径。已有身份但 token 文件丢失或 hash 不匹配时会拒绝自动轮换。先检查配置，再按自己的部署流程安装新二进制、重启服务加载注册表。本分支的开发/测试不会自动修改在线配置或重启原通道。

仅配置新客户端而未升级服务时，identity 接口不存在，连接器会明确失败。需先升级 agent-bridge 服务和 CLI，再接入新客户端。增加的新 relay 值写入日志后，旧二进制不能保证理解；回滚前应备份并检查日志兼容性，不要用旧二进制静默重放新格式记录。

## 交互派发

两个客户端都使用同一套 bridge 工具。模型必须先确认 bridge_identity，然后可以主动 bridge_send，也可以 bridge_inbox/bridge_show 接受别人派来的任务。`task → ack → question/answer → progress → result → verdict → close` 的规则不变；派发不等于对方已开始执行。

打印配置片段（不会读取模型凭据或改写用户配置）：

```bash
python3 crates/agent-bridge/clients/cli/bridge_agents.py config \
  --client codex --agent codex-collab \
  --token-file ~/.config/agent-bridge/codex-collab.token
python3 crates/agent-bridge/clients/cli/bridge_agents.py config \
  --client opencode --token-file ~/.config/agent-bridge/opencode.token
```

Codex 输出 TOML，合并到其 MCP 配置；OpenCode 输出 JSON，合并 `mcp.agent_bridge`，不要覆盖模型或已有浏览器 MCP 配置。输出默认是 WSL/Linux Codex 配置；Windows codex.exe 请使用下面原生连接器自动生成 wsl.exe 启动方式。

也可以不持久修改客户端配置，直接启动一次原生会话（继承该客户端现有模型和权限设置，没有 --yolo、--auto 或审批绕过）：

```bash
python3 crates/agent-bridge/clients/cli/bridge_agents.py run \
  --client opencode --token-file ~/.config/agent-bridge/opencode.token \
  --workdir /home/administrator/projects/Chrome-MCP \
  '先确认身份，再把接口检查任务派给 codex-collab，并报告 task id。'
```

Codex 同理使用 `--client codex --agent codex-collab`。可用 `--client-bin` 指定 CLI，`--bridge-bin` 指定新服务配套的 CLI，`--model` 显式选模型；未指定时不会替用户选择模型。OpenCode 必须使用原生工具会话，不将其免费模型当作外部 completion API 代理。

Windows codex.exe 从 WSL 调用时，传入 WSL 可执行文件路径，例如 `/mnt/c/.../bin/codex.exe`；连接器通过 wsl.exe 启动 Linux 端 agent-bridge MCP。工作目录必须位于 `/mnt/<盘符>/...`；纯 Linux 工作目录请使用 Linux Codex CLI。

配置中的 AGENT_BRIDGE_TOKEN 置空，防止父进程中其他 agent 的 token 覆盖选定文件；Rust 客户端将空 token 视为未配置。token 内容不写入 MCP 配置或命令行。

## 可选后台任务处理

CLI 会话结束后不会凭空自动醒来；启用 worker 才能持续等候派发：

```bash
python3 crates/agent-bridge/clients/cli/bridge_agents.py worker \
  --client opencode --agent opencode \
  --token-file ~/.config/agent-bridge/opencode.token \
  --workdir /home/administrator/projects/Chrome-MCP \
  --state-dir ~/.local/state/agent-bridge-cli/opencode
```

Codex 使用单独的 `--agent codex-collab`、token 和 state-dir。`--once --wait 0` 可只处理一批已有消息。持续 worker 会按任务恢复原生客户端 session；不同任务不共享 session。它既处理收到的任务/返工/回答，也处理本方派发后收到的问题和结果，因此支持双方各自作为 requester/receiver。

- 新任务先 ack；模型必须通过 MCP 写入实际协议回复。CLI 退出 0 或说“完成”不等于任务完成。
- 相关事件和输出用 0600 本地 journal 保存；同一任务批次合并处理，state-dir 绑定 server/agent/client/workspace，并加单进程锁。每批按任务挑选**最新的可执行事件**驱动一次 CLI 回合，因此尾随的 progress/close/静默 note 等 quiet 事件不会掩盖更早未处理的 task/result；只有 quiet 事件时只推进已读游标、不启动 CLI。不确定的旧 journal 先于更新事件对账。接收方的首次 ack 由任务状态（open）决定，而不是由选中的那条消息决定：即使先收到 normal note，worker 也会先补 ack，模型的结果才不会被服务拒绝。
- CLI 错误、超时、中断、或没有写协议回复时标记 uncertain；不自动重跑。先检查服务消息和本地输出，确认副作用，再通过 `--retry-task <id>` 显式重试。一次授权只对应一个具体的待处理事件/尝试：它只绑定本轮轮询开始时**已经处于 uncertain 的任务**，不匹配的 flag 立即过期，在启动前即被消费；因此授权之后才失败的作业不会继承该授权，同一批 args 的后续轮询不会重复启动，之后再次失败仍保持 uncertain 由人工处置。不要删除 journal 来强行重复执行。
- worker 不自动生成 done 结果、不替 requester 自动批准。返工、问答、子任务都沿用既有状态机；结果是否可信仍由 requester 实际核验。
- 子任务使用 `bridge_send(parent=<当前任务>)`，且派发者必须是父任务参与方；父任务不会因为 child 已完成而自动变成 done，agent 应检查 child 的证据后回报父任务。
- 这是单机 Linux/WSL 的可选 CLI worker。不是向任意正在打开的 Codex/OpenCode 桌面窗口注入消息的系统，也不是分布式租约。不要同时对同一身份运行旧审查 worker、另一个 worker 和会消费同一队列的交互会话。

如需开机运行，提供了 `deploy/agent-bridge-cli@.service` 模板和 `opencode.worker.env.example`；先调整路径并安装连接器，再由用户按部署流程启用实例。不默认安装、启用或修改已有 systemd 服务。可执行任务权限取决于客户端自己的工作区/审批策略，消息中的指令不能授予额外权限。

`bridge_agents.py` 依赖同目录的 `bridge_adapters.py`（`sys.path` 只加自身目录），两个文件必须一起安装到 service `ExecStart` 引用的目录，否则 `worker`/`run` 会在导入适配器时失败：

```bash
install -Dm755 crates/agent-bridge/clients/cli/bridge_agents.py  ~/.local/share/agent-bridge/bridge_agents.py
install -Dm644 crates/agent-bridge/clients/cli/bridge_adapters.py ~/.local/share/agent-bridge/bridge_adapters.py
```

## 验证

```bash
cargo test -p agent-bridge
TEST_BRIDGE_BIN=/path/to/new/agent-bridge python3 crates/agent-bridge/clients/cli/test_bridge_agents.py
# 旧 Codex 审查者回归（需要新 binary 在 PATH）
python3 crates/agent-bridge/clients/codex/test_codex_worker.py
```

Rust 集成测试实际经 HTTP/MCP 覆盖四个身份全部 12 个方向、问答、结果/核验/关闭、重试幂等和第三方隔离；Python 测试使用私有服务和假 CLI，验证 session 续接、派发、恢复与安装配置保留，共 69 项（另有 5 项本机工具授权回归，原 64 项覆盖内容如下；覆盖跨进程身份锁、运行中取消、doctor、增量上下文与原生适配器，含针对两轮评审返工的规范化 URL、整段 prompt 字节预算（组合结构化字段/转义 JSON 与不可行预算报错）、按轮条件化的 bridge_show 策略、全量省略计数与诚实 checkpoint、父进程先退出时的进程组清理、卡住监控不拖累 CLI 期限的有界探针、密钥回显脱敏与坏配置校验、doctor 客户端版本硬失败与身份 agents schema 校验）。模型无需参与这些确定性测试。诊断命令不影响任何状态：`doctor` 为只读。

可选真实 CLI 验收：`clients/cli/live_smoke.py --help`。它创建私有随机端口、临时 agent 注册和空 notify 配置，检查 Codex/OpenCode 双向真实派发、算术+随机回执和 requester 核验关闭；会消耗正常模型额度。测试绝不连接当前线上服务。

官方客户端配置参考：
- Codex MCP：https://learn.chatgpt.com/docs/extend/mcp?surface=cli
- OpenCode MCP：https://opencode.ai/docs/mcp-servers/

## 身份互斥、运行中取消与上下文预算

- **跨 state-dir 身份互斥**：`run` 和 `worker` 启动时会按“规范化 server URL + 认证身份”获取一个用户级私有锁，位置为 `$XDG_STATE_HOME/agent-bridge-cli/locks/<hash>.lock`（未设 `XDG_STATE_HOME` 时回退 `~/.local/state`）。它与 state-dir/workdir/client 无关，因此同一个身份即使换了 state-dir 也只能有一个新连接器实例在消费；不同身份或不同 server 使用不同锁、可以并存。原有的 state-dir 绑定（`identity.json`）和 `worker.lock` 仍然保留。该锁只能约束实现了锁的新连接器，**不能**排除不实现该锁的旧消费者；请勿让旧审查 worker、另一个 worker 与会消费同一队列的交互会话共用同一身份。
- **CLI 运行中取消**：worker 在原生 CLI 执行期间以固定间隔（`--cancel-poll`，默认 2 秒）只读当前任务状态（不查询收件箱、不推进游标）。若任务被 closed/cancelled，worker 会停止自己拥有（`start_new_session`）的 Linux 进程组（SIGTERM 后必要时 SIGKILL），做终端对账（journal 记为 done + reconciled 并 ack），**不伪造 result、不自动重试**。服务不可用或监控异常一律按“未取消”处理（绝不误判为取消）。进程组清理在父进程先退出、仍有抗拒 SIGTERM 的后代时同样生效：先回收父进程，再对残余组做有界宽限后 SIGKILL，绝不无限等待。原有的超时/SIGTERM 清理保留；大 stdout/stderr 通过并发读取避免死锁。交互式 `run` 没有任务监控，行为不变。Windows interop 进程及跨系统后代进程的完整清理仍未独立验收、不作保证。
- **有界/增量上下文**：每次原生 CLI 回合的 prompt 都带字节预算（`--prompt-budget`，默认 24000 字节），预算覆盖**最终整段 prompt**（指令块 + 任务的标题/元数据/结构化字段 + 转义后的 JSON），而不只是消息正文。resume 同一 session 时只发送“上次成功投递基线之后的对端变更 + 当前触发事件”；新 session 或恢复（session 失配）时发送原始任务要求 + 最近上下文。任何截断/省略都会在 payload 的 `context` 元数据里显式标注（`omitted_messages`/`truncated`/`requires_bridge_show`），并附带 `bridge_show` 全程权威提示；**不会静默丢弃当前触发事件，也不会声称全部消息已投递**。`context.delivered_through == checkpoint_n`，且与 worker 实际记录的成功基线一致（新 session 同样统计全部省略）；只有确认成功的回合才推进投递基线，失败、不确定、以及回合期间到达的对端消息都不推进。OpenCode 以 argv 传 prompt，故其预算再受保守上限（100000 字节）约束，避免超长参数失败；Codex 经 stdin 传 prompt。`canonical_url` 会对 server URL 规范化（小写 scheme/host、去默认端口与尾斜杠、拒绝凭据/查询/片段/非法端口），使等价写法共用同一身份锁（`localhost` 与 `127.0.0.1` 这类无法解析的 DNS 别名仍视为不同）。
- **只读诊断**：`doctor` 子命令输出机器可读 JSON 并返回有意义的非零退出码。它检查 CLI 可执行文件可用性/版本（受限命令，且二进制必须可执行）、workspace/token 可读性、认证身份、server/API 可达性与**协议版本 `protocol == '2'`**、以及实际调用适配器生成的 MCP 配置结构（含 Windows 约束与损坏的 OpenCode 配置）。**不打印 token、token 哈希、模型凭据或完整环境，不启动模型，不消费收件箱，不修改任何配置或权限**；任何检查细节都会对 token 及其哈希脱敏（含服务端在错误响应体里回显的密钥）。所有探针都有有界超时（监控/诊断固定短超时、不重试），服务卡住时不会长时间阻塞。它明确说明：MCP 审批由原生客户端把关，无法仅凭身份证明，也不能由连接器标志绕过。

诊断示例：

```bash
python3 crates/agent-bridge/clients/cli/bridge_agents.py doctor \
  --client codex --agent codex-collab \
  --token-file ~/.config/agent-bridge/codex-collab.token \
  --bridge-bin ~/.local/bin/agent-bridge --client-bin codex
```

未指定 `--prompt-budget`/`--cancel-poll` 时使用默认值，无需修改 systemd 模板或 env 文件。

## Codex 原生 MCP 审批限制

Codex 的 `exec` 在工具需要审批、客户端审批策略为 never 时，会报错 `MCP tool call requires approval, but approval policy is never`。默认配置不预授权任何工具，失败仍记为 uncertain；不自动重试。

明确允许无人值守本机任务协作时，可给 `run`、`worker`、`config` 添加 `--approve-bridge-tools`。它仅适用于 Codex 和无凭据、无 query/fragment 的 HTTP(S) loopback URL，并按官方 MCP 配置为以下工具逐一设置 `approval_mode = "approve"`：identity、tasks、show、send、post、inbox、ack、pending、presence、health（实际名称均有 `bridge_` 前缀）。**不包含人工决策工具 `bridge_decide`、`bridge_asked`**，也不修改全局 approval_policy/sandbox_mode、模型、账号或文件执行权限。仅使用可信任的本机桥接服务。

systemd 可在专用 `codex-collab.worker.env` 中设置 `BRIDGE_WORKER_OPTIONS=--approve-bridge-tools`，参考 `deploy/codex-collab.worker.env.example`。默认变量为空，原 OpenCode 和旧 Codex 审查者不受影响。显式重试 uncertain 任务前仍需核对副作用。

`live_smoke.py --approve-bridge-tools` 使用私有随机端口验收两个原生客户端；不加此参数则保留原生审批行为。`--only-opencode` 使用 Codex 协议 fixture，不能作为双原生模型成功证据。官方配置说明：https://learn.chatgpt.com/docs/extend/mcp 。


## Final requester review (2026-10-17)

OpenCode implemented the five optimizations through agent-bridge; Codex reviewed two rework rounds and added one final session-checkpoint correction. The checkpoint is frozen before native launch. If a resumed CLI returns a different session ID, its saved context is invalidated and the next turn recovers the original task rather than trusting the previous session history. A private-service regression first failed (stored 29 vs sent 4), then passed with the correction and verified original-task recovery.

Final independent tests: 64 connector + 55 Rust unit + 3 four-agent MCP + 8 legacy Codex worker = 130 passed. Source remains on feature/agent-bridge-codex-opencode, committed and deployed on 2026-10-17; see plans/Agent-bridge-deployment-20261017.md for actual live validation limitations. Detailed independent logs and task transcript: E:/rust-agent-browser/reviews/opencode-five-optimizations-20261016-224315. Earlier evidence sections retain their historical per-round counts.
