# deploy/m2

M2 第 8 步的部署产物。只准备文件和脚本，不执行 root 操作。主文档是 `RUNBOOK.md`。
三个二进制来自两次独立构建（gate+runner 一起，agent-bridge 单包），agent-bridge 只安装单包产物。
代码修订 X 是 `b423a579139fb5e7f47e64e976755a900ade547c`（`m2-step1` 分支 head），三个二进制都从它构建；本目录与 `scripts/m2-e2e/` 所在的提交是部署产物 Y。部署方式是用户选定的计划内中断（RUNBOOK 第 0.2 节风险框），执行前须用户按 RUNBOOK 0.3 逐项确认。
表中 `scripts/m2-e2e/*` 是仓库根目录下的路径（本工作树为 `/home/administrator/projects/Chrome-MCP-m2/`）。

| 文件 | 用途 | 谁执行 |
|---|---|---|
| `milestones.sample.json` | `review-gate serve --milestones` 的配置样例（字段由 `MilestoneConfig` 的 `deny_unknown_fields` 决定） | `repo_id` 已由 bridge q1 定为 `github.com/victormshan/WebUI-AutoTest`，用户安装到 `/etc/review-gate/milestones.json` |
| `review-gate.service.d-milestones.conf` | systemd drop-in：清空并重写 ExecStart（`--identity-config`、`--agents-file`、`--milestones`、`--drain-file`），设置 U-c=1 的失败重启/退避上限；取代 2026-10-22 的 `agents.conf`（须移走） | 用户 `install` 到 `/etc/systemd/system/review-gate.service.d/` |
| `RUNBOOK.md` | 三二进制构建 manifest、`/etc` 与 observer 配置、停机排空、原子替换（gate/runner/agent-bridge）、启动顺序、健康检查、部署后管理步骤、回退 | 用户逐条执行 sudo；Claude/opencode 只核对 |
| `selftest-output.txt` | X（`b423a57`）构建上的 verify-range、E1（部署构建与 fault 构建各一次）、observer 真二进制自检、E4 preflight 原始输出 | 自动生成，只读 |
| `scripts/m2-e2e/e1_failclosed.sh` | E1：隔离实例的坏配置 fail-closed 与两种“未登记”状态断言 | 无 root，可直接运行 |
| `scripts/m2-e2e/bridge_observer_e2e.sh` | observer 真实二进制自检（临时 HOME/端口，无 sudo，不碰线上）：观察者只能读任务元数据，其余 403/401 | 无 root，可直接运行；安装 agent-bridge 前必跑 |
| `scripts/m2-e2e/e2_anonymous.sh` | E2：部署后以 root 执行一次的只读脚本：环境只输出名字；gate 自身匿名读取的 `sync`；按运行中 unit 解析的沙箱内读取与验签 | 用户 `sudo HEAD=<X> bash …` |
| `scripts/m2-e2e/e4_preflight.sh` | E4 前提：完整 base/head、任务书 sha256、提交数、verify-range 原始输出、pinned 公钥、diff 大小 | 无 root，可直接运行 |
| `scripts/m2-e2e/e4_run.sh` | E4 链路：register → 等用户/DSH confirm → request → status → publish → status，保存原始 JSON 与 sender | 用户/Claude 在部署后运行；`--publish` 才写远端 notes |

自动执行：`e1_failclosed.sh`、`e4_preflight.sh`（本地隔离/只读）、`bridge_observer_e2e.sh`（安装前自检）。
用户执行：所有 sudo、`/etc` 与二进制安装、agent-bridge 替换与重启、observer 注册、第 8 节的
`admin milestone`（每条先 dry run 再加 `--yes`）、E4 的 publish 决策。
Claude：E3（反向认证）Rust 测试、整步复核、bridge 记录。
