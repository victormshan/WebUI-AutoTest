# M1 证据（第一部分）：audit-runner（2026-10-15）

依据：`plans/milestone-audit-plan.md` 第 4、5 节（模型 A，首版 M1）。

> **说明**：从"已经完成的环境准备"到"变异测试"各节，是 audit-runner 首次提交（f4085de）时的记录，保留作历史。其中的安装路径、登录状态、测试数量都已过时。现状见"后续状态"一节和 `verification.md`（第 3 版）。

## 已经完成的环境准备（由 DSH 执行，经用户授权，bridge 任务 dsh-m1-auditor-setup）
- 系统用户 `auditor`：uid 997，`/home/auditor` 权限 0700。
- 以 auditor 身份安装 Codex CLI 0.162.1（Linux x64）。安装成了 npm 的本地布局，可执行文件是 `/home/auditor/.local/node_modules/.bin/codex`。
- **还没做**：auditor 的 Codex 登录（需要用户本人执行 `codex login --device-auth`）、服务安装、M1 权限验证报告。

## audit-runner（`crates/audit-runner`）
- **不持有任何密钥。** 签名只由 review-gate 做（方案第 4 节第 1 点）。
- 只监听本地 unix socket。生产环境由 systemd socket 激活创建 socket：属主 auditor，属组 reviewgate，权限 0660（`deploy/audit-runner.socket`）。
- **调用方检查**：用 SO_PEERCRED 核对对端 uid 必须是 `--allow-uid`（reviewgate，999）；如果配置了 `--allow-cgroup`，还要核对对端的 cgroup（`/proc/<pid>/cgroup`，部署为 `/system.slice/review-gate.service`）。最初的 `--allow-exe` 读 `/proc/<pid>/exe`，跨 uid 需要 ptrace 权限，验证时被 DSH 发现会把真实 gate 也拒掉，已改为 cgroup（见 verification.md）。不符合就直接拒绝，**不会建作业目录，也不会启动 Codex**。
- 每个连接只处理一个作业：一行 JSON 请求（job_id、input、timeout），一行 JSON 回复。
  - job_id 只允许字母、数字和 `-_.`，**每个 id 只能用一次**，重复提交会被拒绝；
  - 输入上限 4 MB；
  - **runner 自己计算输入的 SHA-256**，不依赖模型回显。
- **Codex 的运行方式**：
  - `codex exec --json -s read-only --skip-git-repo-check -c approval_policy="never" -o answer.md -`，输入从 stdin 进；
  - **环境变量全部清空**，只设置 PATH、HOME、CODEX_HOME，所以 `WSL_INTEROP` 和凭据类变量都传不进去；
  - Codex 在独立的进程组里运行，**超时就 kill 整个进程组**；
  - 每个作业的目录里保存：输入、事件流（events.jsonl）、stderr、答复。
- **回复**：作业 id、输入摘要、状态（completed / failed / timed_out / rejected）、退出码、答复原文、事件里各类条目的计数，以及 Codex 尝试执行过的命令和它们的结果（M1 权限验证会用这些证明"命令是被配置拒绝的"）。
  - Codex 非零退出，或者没有写出答复：一律 failed，**绝不报 completed**。
- **service 单元**（`deploy/audit-runner.service`）：User/Group=auditor；NoNewPrivileges；ProtectSystem=strict；`/home` 只挂进 `/home/auditor`（ProtectHome=tmpfs 加 BindPaths）；PrivateTmp；UMask 0077。程序应安装为 root 所有的 `/usr/local/lib/audit-runner/audit-runner`。

## 测试（`tests-audit-runner.txt`：单元测试 4 个、集成测试 6 个，全部通过）
集成测试使用真实的 unix socket 和假 Codex，覆盖：
- 正常作业：runner 计算的摘要正确，环境里没有 `WSL_INTEROP`，记录了一次被拒绝的命令执行；
- 未授权的调用方：被拒，没有建作业目录；
- 作业 id 重放：被拒；
- 非法 id 和坏请求：被拒；
- Codex 失败或没有答复：报 failed；
- 卡住的 Codex：报 timed_out，**整个进程组都结束了**，runner 之后能接着处理下一个作业。

**如实记录一次失败**：第一次运行这套测试时，超时测试失败了一次（等了 3 秒还有残留的 sleep 进程），时间点正好在刚编译完、机器负载较高的时候。之后整套测试连续跑了 3 + 10 次，全部通过，没能复现。我把等待上限放宽到 10 秒，没有改动被测的逻辑。

## 变异测试（`mutation-audit-runner.txt`）
| 变异 | 结果 |
|---|---|
| A1 不检查调用方 uid | 单元测试失败 |
| A2 不清空环境 | 集成测试失败（`WSL_INTEROP` 传了进去） |
| A3 超时只杀顶层进程 | 超时测试失败（有残留进程）。这次变异留下的 sleep 进程随后已经清理掉了 |
| A4 作业 id 可以重复使用 | 重放测试失败。第一次跑受到 A3 残留进程的干扰，多失败了一个测试；清理之后单独重跑，只有重放测试失败 |

## 后续状态（2026-10-15）
- 用户已登录 auditor 的 Codex；DSH 按用户授权安装了 runner。
- 权限验证共 5 轮，见 `verification.md`（第 3 版），原始输出在 `raw/`。
- Codex 工具面和上下文的抓包证据在 `codex-surface/`。
- Codex 阶段审查两次判 REWORK，都已按意见修改：
  - 执行类工具全部关闭；
  - 每个作业使用干净的 Codex 主目录；
  - 超时也保留执行记录；
  - 事件流完整性校验；
  - 进程组清理只认 ESRCH；
  - 描述符逐个归类；
  - 按用户决定，Codex CLI 改为 root 所有，gate 跟踪目录不让 auditor 读。
- 测试客户端：`crates/audit-runner/tools/client.py`。
- Codex 阶段审查第 4 轮 APPROVE（codex-m1-review 消息 16）。下一步：过 gate 提交。
