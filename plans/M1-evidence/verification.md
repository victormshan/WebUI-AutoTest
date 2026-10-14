# M1 验证报告：模型 A 下的权限与执行边界（2026-10-15，第 4 版）

依据：`plans/milestone-audit-plan.md` 第 5 节 M1 的验收项。

第 1–3 版经 Codex 阶段审查都判为 REWORK（bridge 任务 codex-m1-review，消息 3、10、13）；**第 4 版经 Codex 判为 APPROVE（消息 16）**，6 条都是非阻断意见，本版已按其中可在 M1 内处理的几条修正（见文末）。本版按三次审查意见修改了实现，补做了第 4–6 轮验证，并补了 Codex 工具面和上下文的抓包证据（`codex-surface/`）。**结论栏只写证据能支持的内容，并逐项注明验证对象。**

**验证对象**
- **已安装的服务**：用户 auditor（uid 997），systemd 单元 `audit-runner.socket` / `audit-runner.service`，root 所有的程序 `/usr/local/lib/audit-runner/audit-runner`。
- **临时单元**：一部分步骤用同样的沙箱属性另起一个临时单元，在表中注明。原因有两个：
  - 真正的作业只接受来自 `review-gate.service` 的连接，而 gate 侧要到 M2 才实现；
  - 每次只能把一个可控的调用方放进允许的 cgroup。
- **测试**：用 `systemd-socket-activate` 的 FD 测试，以及假 Codex 的集成测试。它们只证明代码行为，不作为部署验证的依据。
- **抓包**：Codex CLI 0.162.1 的模型提供方被重定向到本地的假 Responses API（`codex-surface/tools/`），得到模型实际收到的工具和上下文，不耗额度。本地抓包用同版本的 Linux Codex；部署环境的抓包（I2）走已安装的 runner、root 所有的 Codex 和真实的 auditor 主目录。

**执行方式**
- 需要 root、auditor、reviewgate 身份的步骤由 DSH（DeepSeek）执行。授权来源：
  - 用户授权 M1 验证（"可以交给dsh"）；
  - 第 4 轮另有用户对两项 root 变更的决定（见第 4 轮）。
- 任务书和原始输出：`raw/task*.md` → `raw/dsh-raw*.txt`。
- 实施方本人（administrator，uid 1000，不用 sudo）能做的检查由 Claude 执行，见 `raw/implementer-checks*.txt`。
- 脱敏：`dsh-raw.txt` 中 `ls /mnt/c` 的输出只保留条目数（那是用户 C 盘的目录清单）。

## 结论
| 验收项（方案第 5 节） | 结论 | 验证对象与证据 |
|---|---|---|
| auditor 读不到 gate 的**密钥** | 满足 | 已安装环境。B1–B4：`/etc/review-gate/env`、`client.token`、`state/keys/gate.ed25519` 均不可读，`state/` 不可列出。B5：auditor 不在 reviewgate 组 |
| auditor 读不到 gate 的**日志** | 满足（第 4 轮起） | 已安装环境。G3.1：gate 的 journal 不可读，auditor 不在 adm、systemd-journal 组。gate 的 relay 跟踪目录原来对所有用户可读（第 3 轮 G3.3 实测 READABLE），按用户决定改为 `reviewgate:administrator 0750`（H3）。之后 auditor 既不能读其中文件，也不能列目录（H3.4、H3.5）；实施方仍可读（`raw/implementer-checks-r4.txt` 末尾）。`crates/review-gate/deploy/install.sh` 已同步修改 |
| auditor 读不到 gate 的**备份** | 未发现备份；核查范围有限 | `raw/implementer-checks-r4.txt`：<br>① review-gate 源码和部署脚本里没有 backup / .bak / snapshot / archive；<br>② `/var/backups` 里没有相关内容；<br>③ 全盘按名字查找（不含 /proc、/sys、/mnt）只找到程序、配置、状态目录、gate 的 PrivateTmp 目录，以及实施方自己的文件。<br>**局限**：查找以 administrator 身份进行，看不到无权进入的目录（如 /root、/home/auditor）。gate 的签名记录推送到 GitHub notes，本身就是公开记录 |
| auditor 读不到 gate **继承的描述符** | 满足 | 已安装程序加临时单元（H5）。systemd 的 transient socket 与 service 用与正式单元相同的沙箱属性激活 runner，假 Codex 列出它持有的全部描述符，**只有 4 个**：stdin 管道、自己的 events/stderr 文件、脚本本身，**没有 socket**。<br>代码层面：激活时给 fd 3 设置 FD_CLOEXEC，设置失败就退出（不会回退到 `--socket`）。测试 `codex_inherits_no_socket_under_socket_activation` 逐个归类描述符，缺少工具时测试失败而不是跳过（变异 A6 可抓到） |
| 未授权的身份不能派发作业 | 满足 | 已安装服务。<br>C1/F2.1：auditor 被拒，原因 `peer uid 997 is not allowed`。<br>F2.2/G2/H2.6：reviewgate 身份但不在 gate 服务里的进程被拒，原因 `peer cgroup /non-systemd is not /system.slice/review-gate.service`。<br>administrator 连接 socket 得到 EACCES。被拒的请求不建作业目录 |
| 允许的来源能被接受 | 机制满足；**真实 gate 未核实** | 临时单元（F3）。在允许 cgroup 里的 uid 999 进程被接受（F3.3），单元外的同一 uid 被拒（F3.4）。<br>gate 实际单元：`ControlGroup=/system.slice/review-gate.service`、`Delegate=no`，没有 drop-in（`implementer-checks-r4.txt`）。<br>**真实 gate 的正向连接要到 M2 才能实测**。这是"服务来源白名单"：授权的是该服务里、uid 相符的进程，包括它的子进程，**不等于程序白名单** |
| 不能冒充 runner | 部分满足 | 已安装环境。socket 由 systemd 以 root 身份创建，所在目录 `/run/audit-runner` 归 root、权限 0755，其他用户不能在那里新建或替换 socket（A5）。<br>gate 对 runner 的反向核对（对端 uid 997，以及 audit-runner.service 的 cgroup）**属于 M2 验收项，尚未实现** |
| 配置确实阻止 Codex 执行命令（证据来自 CLI 日志） | 满足（第 4 轮起；工具面第 5 轮起核实） | **① 工具面**（`codex-surface/capture-summary.txt`）：<br>• 不加 `NO_TOOLS` 时，模型拿到 `exec_command`、`write_stdin` 等；<br>• 加上后，默认模型 gpt-6.1-sol 拿到的只有 code mode 的 `exec`、`wait`、`request_user_input`、`clock.sleep` 和 `collaboration.*`（子代理）；<br>• gpt-5 只拿到 `request_user_input`、`view_image` 和 goals 工具。<br>部署环境抓包（I2，真实 auditor 主目录）的默认模型工具面与本地一致。<br>**② 逐项关闭的依据**（假模型强行调用，CLI 的处理结果）：<br>• `exec_command`：对照组（不加 `NO_TOOLS`）里命令**确实执行了**（c-32）；加上后 CLI 回 `unsupported call: exec_command`（c-33/34）；<br>• code mode `exec`：CLI 回 `code-mode host is disabled`（c-31）；<br>• 派生子代理：**能成功派生**（c-35），子代理拿到的工具与父代理相同，同样没有可用的命令执行入口；<br>• MCP：auditor 配置的 MCP 服务器因忽略 config.toml 不加载，抓包里没有；文件修改（`apply_patch`）只出现在 code mode `exec` 的嵌套工具里，随 `exec` 一起失败。<br>**③ 真实作业**（H4，临时单元，已安装程序和 root 所有的 Codex）：请 Codex 执行 6 条命令，CLI 日志有 7 条 `ERROR codex_core::tools::router: error=code-mode host is disabled`，事件流里 `command_execution` 为 0，`commands` 为空，也没有标记文件。7 条日志与 6 次尝试无法一一对应（模型可能多试了一次），结论不依赖这个计数。<br>**结论的范围**：在所抓取的配置（CLI 0.162.1、NO_TOOLS、默认模型 gpt-6.1-sol 与 gpt-5）下，命令执行入口都被拒绝；这**不是**"没有工具"或"没有子代理"——模型仍有 `request_user_input`、`wait`、`clock.sleep`、子代理等工具。CLI 或模型的工具面变化时必须重新抓包。<br>**第 1 版的结论是错的**：那时只用只读沙箱，`id`、`ls`、`test` 都执行成功了（D3） |
| 程序和配置不能经普通路径被替换 | 满足（第 4 轮起），附模型 A 限制 | 已安装环境。<br>E1/E2：auditor、reviewgate 对 runner 程序、目录、两份单元文件、`/etc/systemd/system`、`/usr/local/bin/review-gate` 都没有写权限，实施方同样没有。<br>Codex CLI 原来装在 auditor 自己的目录里（E3，auditor 可以替换），按用户决定改装到 root 所有的 `/usr/local/lib/auditor-codex`：auditor 在其中找不到任何可写路径（H1.3），版本和登录状态正常（H1.4、H1.5）。node 是 root 所有的 `/usr/bin/node`。<br>**仍归 auditor 所有**：`/home/auditor/.codex`，里面有登录凭据，也可能被放进 AGENTS.md、技能等指令文件。<br>• **它们原本确实会进到模型输入**：本地抓包里，故意放进去的全局 AGENTS.md 和技能描述都出现了（c-21）。`--ignore-user-config`、`--ignore-rules` 管不到它们，也没有配置项能关掉（`--strict-config` 拒绝了所试的键）。<br>• **现在的做法**：每个作业换用一个新建的 Codex 主目录（`--run-home`），里面只放一份登录副本。作业中刷新过的登录写回原处，之后整个目录删除；登录副本不在作业目录里，不进证据包。<br>• **验证**：本地经 runner 抓包，放进去的 AGENTS.md、技能和 MCP 都不再出现（c-41）。部署环境抓包（I2）中模型输入只有 CLI 自带的内容：基础指令、只来自新主目录 `.system` 的 4 个自带技能、权限说明、协作模式、多代理角色、环境信息和作业输入，没有 AGENTS.md 指令。真实作业（I3）能登录并完成；run home 事后已删除，作业记录里没有 auth.json，原登录文件哈希前后一致。<br>• **失败路径**：准备 run home 中途失败、或刷新的登录写不回去时，run home 也会被删除，作业报 failed；写回失败时那次刷新的登录会丢失，auditor 可能需要重新登录（报错文字里写明）。<br>• 测试 `codex_sees_a_clean_home_with_only_the_login`、`a_login_refreshed_during_the_job_is_kept`、`the_run_home_is_removed_when_the_job_cannot_start_or_the_login_cannot_be_saved`（变异 A15、A16、A21、A22 可抓到） |
| 如实记录 auditor 能访问哪些 Windows 路径、能否用互操作 | 已记录 | 见"发现 2" |

## 发现
### 发现 1（DSH 在第 1 轮 C2 发现，已修复）：程序白名单跨 uid 不可用
`/proc/<pid>/exe` 跨 uid 读取需要 ptrace 权限，所以真实的 gate 也会被拒。

已改为服务来源白名单 `--allow-cgroup`。第 2 轮复验时，拒绝原因与预期逐字一致。

### 发现 2：auditor 的 Windows 访问与互操作
| 探针 | 第 1 轮，Codex 只读沙箱内（D3） | 系统沙箱，不经 Codex（D5） | 第 4 轮，工具全关后经 runner（H4） |
|---|---|---|---|
| `ls /mnt/c` | 能列出 | 能列出 | 没有执行（无工具） |
| `cmd.exe /c ver` | 失败（互操作在 Codex 沙箱内出错） | **成功** | 没有执行 |
| `wsl.exe -u root -e id` | 失败（同上） | **成功，`uid=0(root)`** | 没有执行 |

- **auditor 身份本身**（D5）可以读 Windows 盘，也可以经互操作无密码取得 root。这是选模型 A 时已知的宿主机控制限制（方案第 1、3 节），这里在实际服务身份下复现了。
- **经 runner 的路径**（H4）上，模型已经没有能执行命令的工具，所以这些路径够不着。但这不等于互操作被可靠禁用：要从根本上挡住，需要模型 B，或者在 WSL 层面关闭互操作（由用户决定）。

### 发现 3（Claude 对照验收项时发现，已修复）：监听 socket 泄漏给 Codex
fd 3 没有设置 close-on-exec。已修复；部署配置下的描述符清单见 H5。

### 发现 4：模型的自述不能当证据
- 第 1 轮 D3 里，Codex 自称执行了 `touch` 并被挡住，但事件流里**根本没有这条命令**。
- 第 4 轮里，自述与 CLI 日志、事件流一致。
- **规则**：执行情况只认 runner 记录的事件、CLI 的 stderr 和磁盘状态，不认模型自述。

### 发现 5（Codex 审查发现，已修复）：超时会丢失执行记录
- **问题**：原实现超时时直接返回，已经读到的输出被丢弃。
- **修复**：Codex 的事件和 stderr 直接写进作业目录里的文件，超时后照样读取汇总；回复中 `error` 注明"record kept, incomplete"。保存输入、建立记录文件、送入输入、读取事件中任何一步失败，都报 failed。
- **测试**：`a_timeout_keeps_the_record_of_what_codex_started`（变异 A9 可抓到）。

### 发现 6（Codex 第二、三次审查，已修复）：进程组清理和停机规则
- **问题 1（第二次审查）**：原来 `killpg(pgid, 0)` 的任何失败都被当成"已消失"；5 秒到期仍有进程时直接返回；回收 Codex 也不受期限约束。
- **问题 2（第三次审查）**：清理未确认时，"回复后退出"依赖回复成功——对端断开或回复阻塞时，runner 会继续接下一个作业。
- **修复**：
  - 新字段 `cleanup`：只有 ESRCH 才记为 `gone`；EPERM 等记为状态未知；到期仍存活、回收 Codex 失败或超时，都记为 `not confirmed: …`。清理在 Codex 正常结束后也做；回收 Codex 也受 5 秒期限约束。
  - **停机规则**：是否必须停机在回复**之前**决定。清理未确认的作业不会报 completed；runner 发送回复（最多 30 秒），**无论回复成功、失败还是超时**都停止接单并以错误退出。systemd 结束服务时按 `KillMode=control-group`（单元里已显式写明）杀掉服务 cgroup 里剩下的所有进程；下一个连接由 socket 激活一个新的 runner。
- **测试**：
  - `group_state_confirms_gone_only_on_esrch`、`settle_confirms_only_a_gone_group`：覆盖 Alive、Gone、Unknown（EPERM）和到期分支；
  - `an_unconfirmed_cleanup_stops_the_runner_after_its_reply` / `…_when_the_caller_is_gone` / `…_when_the_reply_blocks`：注入"清理未确认"，分别在正常回复、对端断开、回复阻塞（32 MB 答复、对端不读，回复期限 300 ms）三种情况下，断言 serve 以错误结束、后续作业没有执行；`a_confirmed_cleanup_keeps_the_runner_serving` 是对照；
  - `tests/fault.rs`（只在 `--features fault-injection` 测试构建里编译）：真实作业的清理被注入为未确认时，作业报 failed、runner 停止。正式程序按不带 `fault-injection` 特性构建（`grep -c AUDIT_RUNNER_FAULT` 为 0 只是辅助证据）。提交的源码不带该特性构建，得到的程序哈希与已安装的 `69983378…` 一致；
  - 变异 A13、A14、A17–A23 都被抓到。
- **部署实测（K3）**：测试构建在与正式单元相同沙箱属性的临时 socket+service 里运行，假 Codex 留下一个 `setsid` 的 `sleep`。作业回 `failed` / `not confirmed: injected fault`；runner 日志 `stopping so systemd ends the cgroup`，服务 `Result=exit-code`；**2 秒后那个 `sleep` 已不存在**（被 systemd 随 cgroup 清掉）；同一 socket 的下一次连接又拉起了新 runner 并得到回复。

### 发现 8（Claude 发现，已修复）：离开进程组的进程躲过清理
- **问题**：Codex（或它启动的进程）用 `setsid` 离开自己的进程组后，按进程组的清理杀不到它，作业却照样报 `cleanup: gone`，残留进程会跨作业存活。本地用生产程序复现过。Codex CLI 有 `daemon_auto_start` 一类会起后台进程的功能，所以这不只是理论上的问题。
- **修复**：正式服务带 `--own-cgroup` 运行。每个作业结束后，runner 杀掉自己服务 cgroup 里除自己以外的所有进程，并确认只剩自己；两步都确认才记为 `gone`。不在 `.service` cgroup 里时拒绝启动。
- **测试**：
  - `under_systemd_a_process_that_left_the_group_is_ended_too`（runner 作为 `systemd-run --user` 临时服务运行）；
  - 对照 `without_the_cgroup_sweep_an_escaped_process_survives_the_group_kill` 证明这正是进程组清理够不到的情况；
  - `own_cgroup_refuses_to_start_outside_a_service`；
  - 变异 A24、A26 被抓到；A25（忽略清扫结果）没被抓到，因为没有测试能让清扫本身失败，如实记录。
- **部署实测（K2）**：生产程序、与正式单元相同的沙箱属性，假 Codex 留下 `setsid` 的 `sleep`。作业 `completed` / `cleanup: gone`，那个 `sleep` 已不存在，runner 继续服务（`ActiveState=active`）。
- **局限**：清扫只覆盖 runner 自己的服务 cgroup；以 root 身份或经 WSL 互操作在别处启动的进程不在其中（模型 A 的已知限制，见发现 2）。

### 发现 7（Codex 第二次审查，已修复）：`evidence_complete` 只代表文件可读
- **修复**：`check_events` 按所固定 CLI 版本的事件格式校验整个事件流：每行都是带 type 的 JSON，以 `thread.started` 开头、以 `turn.completed` 结尾，每个开始的条目都有对应的完成。不满足时作业报 failed（"event record invalid"），`evidence_complete` 为 false，原始记录保留在作业目录。只有事件流有效、清理确认、Codex 正常退出并写出答复时，才报 completed 且 `evidence_complete` 为 true。
- **测试**：`event_records_must_be_whole`（单元测试）；`an_incomplete_or_invalid_event_record_is_never_completed`（集成测试，覆盖"CLI 退出 0、写出答复，但事件为空、损坏或缺少终止事件"）。变异 A12 可抓到。
- **实测**：H4、I2、I3 的真实事件流都通过校验（`evidence_complete: true`）。事件流里固定会有一条 `error` 条目（"Code Mode is unavailable…"），这是工具被关闭后 CLI 的提示，不算作业错误。

## 测试与变异
- `tests-audit-runner.txt`：带故障注入特性共 29 个测试（单元 9、集成 19、故障注入 1）通过，不带该特性 28 个通过；clippy 无警告。`test-runs-8x.txt` 是连续 8 次完整运行的原始记录（全部通过，无残留进程）。
- `mutation-audit-runner.txt`：
  - 第 3 轮：A6、A8、A9、A11 被抓到；A10 没被抓到（原因见文件）。
  - 第 4 轮：A12–A16 被抓到；A17 当时没被抓到。
  - 第 5 轮：A17–A24、A26 被抓到；A25 没被抓到（原因见文件）。文件里也如实记录了写测试时一次误用 `kill(0, SIGKILL)` 的失误和处理。
- 假 Codex 的测试只证明 runner 自己的行为（传了哪些参数、记录如何保存）。**真实 CLI 不能执行**的证据是抓包（`codex-surface/`）和 H4。

## 安装状态（第 6 轮之后）
| 项目 | 状态 |
|---|---|
| runner 程序 | sha256 `69983378df3bfc3f9fb7bd93c32c54ce5539a19879b88d6b42ba478fee79b9c4`（不含故障注入代码） |
| `audit-runner.service` | sha256 `6150c512…`：`--allow-uid 999 --allow-cgroup /system.slice/review-gate.service --codex /usr/local/lib/auditor-codex/node_modules/.bin/codex --codex-home /home/auditor/.codex --run-home /home/auditor/codex-run --own-cgroup`，`KillMode=control-group` |
| `audit-runner.socket` | sha256 `bcf19b20…`，DirectoryMode 0755 |
| Codex CLI | 0.162.1，装在 root 所有的 `/usr/local/lib/auditor-codex`。auditor 目录里的旧副本还在，不再被使用（删除需要用户决定） |
| gate 跟踪目录 | `reviewgate:administrator 0750` |
| 服务状态 | socket 处于 active；服务处于 inactive，有连接时由 socket 拉起 |

## 留给 M2
- 真实 gate 连接被接受（cgroup 实测）。
- gate 侧反向核对 runner。
- gate 只认事件、CLI 日志和磁盘状态，以及 `evidence_complete`。
- 作业目录由 gate 归档摘要。

## Codex APPROVE（消息 16）的非阻断意见与处理
1. 停机规则、cgroup 清扫：已认可；保证限于所述服务 cgroup。
2. A25（清扫失败路径）仍是测试缺口：记入 M2 待办，届时注入清扫读取失败或期限耗尽。
3. 工具面结论限定为已捕获的 CLI 与模型配置；子代理抓包来自植入环境，干净主目录的证明来自部署抓包 I2 和测试。
4. 测试数字已更正为 29 项（带特性）、28 项（不带）；"连续 8 次"补了原始记录 `test-runs-8x.txt`；`grep` 只是辅助证据。
5. `NO_TOOLS` 注释已改为与报告一致（不再写"no sub-agents"）。注释保持原有行数，所以源码构建出的程序哈希不变。
6. 本批准只覆盖 M1：真实 gate 正向连接、反向认证、证据归档和正式放行状态属于 M2；本结论不授权 sudo、登录、门禁豁免或合并 main。
