# M2 第 8 步运行手册：部署 review-gate + audit-runner + agent-bridge 并做 E1/E2/E4

> **状态：待审（2026-10-26）。未经 Codex 复核本版、且用户未在第 0.3 节逐项确认之前，不要执行任何步骤。**
> 本版取代 b7a3a91 的手册（Codex n8 REWORK）。拟采用的部署方式是用户倾向的**计划内中断**
> （见第 0.2 节风险框），**不是**已证明的无损排空：旧 gate（d7a030cb）没有可核验的应用层接纳边界。

## 0. 执行前

### 0.1 两个修订（分开记录）

| | 修订 | 内容 |
|---|---|---|
| **代码 X**（二进制来源） | `b423a579139fb5e7f47e64e976755a900ade547c`（tree `b68f064fa0f8dcbdf6f71ec0e64b3bd5b1072a5c`），`m2-step1` 与 `m2-identity-integration` 的 head | M2 里程碑审计；drain；gate 自身匿名 git；身份整合（主体、属主、/check 不借用、agents-file 一致性校验、管理员文件检查、writers、`admin task assign`、迁移转换器、guard）；audit-runner 测试的 CI 修复 |
| **部署产物 Y** | 本手册所在的提交（在 X 之上，只改 `deploy/m2/` 与 `scripts/m2-e2e/`） | 本手册、drop-in、配置样例、E1/E2/E4 脚本 |

三个二进制都从 X 构建（第 1 节）。E4 的自举 head = X，结论只能称"X 代码修订的自举"，**不**审计 Y 自身。

gate 签名链：`review-gate verify-range --pubkey ITU9Oc3AbiC/Q/LYPwld2b+OC2rxlqHbvCO7IQcGdXw= 3cc5491..b423a57` → 30 verified, 0 failed（截至 b423a57）。

### 0.2 风险框：计划内中断（必须先读）

用户 2026-10-26 在两条路之间表达的偏好，原话：「倾向 2。」——即接受计划内中断的风险，而不是继续设计无损排空。这是**偏好**，不是对下面具体方案的批准；执行前必须按 0.3 逐项确认。

停旧 gate 时可能发生、**不能**排除的情况（Codex n32、n34）：
1. 已被旧进程接纳的 M1 请求可能没有完成；客户端可能不知道请求是否成功。
2. 旧进程的 jobs 表在内存中，停服后丢失；`GET /jobs/<id>` 不再可查。
3. 外部审计（gemini-api / web-gemini）可能已经调用，甚至已经产生费用，但结果未落盘。
4. store 的单个 JSON 用"临时文件 + rename"替换，**只**降低该文件被截断的风险：没有跨文件事务，没有 fsync。可能残留 `.tmp`，可能 review 已写入而任务/作业状态不一致，task/cache/review/relay/Git 相关操作可能部分完成。
5. `systemctl stop` 的实际行为（线上 unit，2026-10-26 读取）：`KillMode=control-group`、`KillSignal=15`、`TimeoutStopUSec=1min 30s`、`SendSIGKILL=yes`：先对整个 cgroup（gate 及其 git、curl 子进程）发 SIGTERM，90 秒后仍未退出的进程被 SIGKILL。

处理原则：停服后**先记录、再判断**；保留全部 state、`.tmp`、日志，不删除、不截断、不恢复旧快照。被中断的审查**不自动重做**：先核对已有 review、cache、任务记录和外部作业，再由用户决定恢复或重新审查，不能当作普通重试，也不能为了拿到 APPROVE 而重做。

不在本风险范围内（不因此授权）：中断仍在运行的 audit-runner/Codex 审计、强杀子进程、恢复旧二进制作为常规回滚（见第 10 节）。

### 0.3 执行前，用户逐项确认（写进日志）

- [ ] 接受 0.2 列出的中断风险，维护窗口为 ____（开始/结束时间）。
- [ ] 受影响的客户端清单（4.1）已知，并同意在窗口内停用。
- [ ] `milestones.json` 的 `writers` 用 `["claude"]`（建议值；`implementers` 仍是 `["claude","opencode"]`）。
- [ ] 部署后，属主未知的旧任务一律保持只读，直到用户逐个决定 `admin task assign`（第 8.2 节）。
- [ ] 执行者：用户本人，或用户指定的代理（逐行执行，记录全部输出，遇到与预期不符的输出立即停止）。

### 0.4 记录

```
script -a /home/administrator/m2-step8-$(date -u +%Y%m%dT%H%M%SZ).log
```

日志里不得出现任何令牌、`/etc/review-gate/env` 的内容或 `Environment=` 的取值。用 `| tee` 记录脚本输出时先 `set -o pipefail`，否则脚本失败会被 tee 的成功掩盖。

### 0.5 仍然有效的用户决定与偏离

1. **U-d 厂商语义**：保留标签（`codex-collab`→`codex`、`opencode`→`opencode`），并接受"标签判定与实际厂商不一致"的偏离：`codex-collab` 与 `auditor=openai` 实际同为 OpenAI；`opencode` 实际运行 DeepSeek。E4 结论写成"**按批准的标签判为独立，实际厂商相同**"。
2. **repo_id** = `github.com/victormshan/WebUI-AutoTest`，首次登记后不可改。
3. **implementers** = `["claude","opencode"]`（bridge agent 名，来源完整性用）。gate 主体与 bridge 名的对照见第 3.1 节。
4. **U-a**：部署 m2-step1 的固定修订（现为 X），**不合并 main**。
5. 身份：Codex 选项 (i)，保留线上 `--agents-file` 的命令行形式，身份以 `--identity-config` 为准；ai-neutral step B 不在本次。
6. 不使用 `safe.directory=*`；只用精确路径。

## 1. 构建清单（manifest）

三个二进制来自**两次独立构建**，都在 X 的干净 detached worktree 上、各用**新的空** `CARGO_TARGET_DIR`；另有一次 fault-injection 对照构建（第三个空目录，不部署）。agent-bridge 必须单包构建：与 gate/runner 一起构建时 Cargo 合并共享依赖的 feature，会得到另一个从未验收过的二进制。

已完成的构建（不要重跑；要独立复核就另建新的空目录）：

```
git -C /home/administrator/projects/Chrome-MCP-m2 worktree add --detach /home/administrator/projects/Chrome-MCP-m2-build-b423a57 b423a579139fb5e7f47e64e976755a900ade547c
cd /home/administrator/projects/Chrome-MCP-m2-build-b423a57
CARGO_TARGET_DIR=/home/administrator/projects/Chrome-MCP-m2-build-b423a57-target-gr cargo build --release --locked -p review-gate -p audit-runner
CARGO_TARGET_DIR=/home/administrator/projects/Chrome-MCP-m2-build-b423a57-target-bridge cargo build --release --locked -p agent-bridge
CARGO_TARGET_DIR=/home/administrator/projects/Chrome-MCP-m2-build-b423a57-target-fault cargo build --release --locked -p review-gate -p audit-runner --features review-gate/fault-injection,audit-runner/fault-injection
```

| 项 | 值 |
|---|---|
| commit / tree | `b423a579139fb5e7f47e64e976755a900ade547c` / `b68f064fa0f8dcbdf6f71ec0e64b3bd5b1072a5c` |
| `git status --porcelain --untracked-files=all` | 空 |
| rustc / cargo | `rustc 1.99.0 (b940084d7 2026-09-28)` / `cargo 1.99.0 (5f94df478 2026-08-27)` |
| 实际启用的 features | gate/runner/bridge 的 release 构建都没加 `--features`；三个 crate 都没有 `default` feature，所以启用集合为空 |

| 产物（部署） | 路径（`…` = `/home/administrator/projects/Chrome-MCP-m2-build-b423a57`） | 字节 | sha256 |
|---|---|---|---|
| review-gate | `…-target-gr/release/review-gate` | 13167568 | `69b3d3a6b5e0b5e982a8191d47a5dcfde6d6d34f5caa0accade53e7c07184f07` |
| audit-runner | `…-target-gr/release/audit-runner` | 1675296 | `c9a66da9442758d85c8411064d1a67753339efab854fe491bcfc79b2293940b3` |
| agent-bridge | `…-target-bridge/release/agent-bridge` | 11748640 | `d9ab3e50895ed729b076d031719738e8157516084b3ae8e2aa2620366b41bca2` |

| 对照（不部署） | 路径 | sha256 |
|---|---|---|
| review-gate（fault） | `…-target-fault/release/review-gate` | `3b564eab65571034489bfa67c0f45db3b2f211358244b806295d096b734390ee` |
| audit-runner（fault） | `…-target-fault/release/audit-runner` | `2b60911447f85d178b60d07d19a80bc8e65d1738fc8890bee73825fe24dc42a0` |

部署前复核（任何一项不符就停止）：

```
cd /home/administrator/projects/Chrome-MCP-m2-build-b423a57
git rev-parse HEAD HEAD^{tree}
git status --porcelain --untracked-files=all
sha256sum ../Chrome-MCP-m2-build-b423a57-target-gr/release/review-gate ../Chrome-MCP-m2-build-b423a57-target-gr/release/audit-runner ../Chrome-MCP-m2-build-b423a57-target-bridge/release/agent-bridge
strings -a ../Chrome-MCP-m2-build-b423a57-target-gr/release/review-gate | grep -c REVIEW_GATE_FAULT
strings -a ../Chrome-MCP-m2-build-b423a57-target-gr/release/audit-runner | grep -c AUDIT_RUNNER_FAULT
strings -a ../Chrome-MCP-m2-build-b423a57-target-fault/release/review-gate | grep -c REVIEW_GATE_FAULT
strings -a ../Chrome-MCP-m2-build-b423a57-target-fault/release/audit-runner | grep -c AUDIT_RUNNER_FAULT
```

预期：HEAD/tree 如上；status 空；三个 sha256 如表；部署产物的两个 `grep -c` 为 `0`；fault 构建的两个为非 0（本次分别为 1 与 3，证明检查有效）。

observer 真实二进制自检（不需要 sudo，临时 HOME 与端口，不碰线上 bridge），安装 agent-bridge 前必跑：

```
bash /home/administrator/projects/Chrome-MCP-m2/scripts/m2-e2e/bridge_observer_e2e.sh /home/administrator/projects/Chrome-MCP-m2-build-b423a57-target-bridge/release/agent-bridge
```

预期最后一行 `ALL OK`，`binary sha256:` 等于上表 agent-bridge。

## 2. 线上现状（2026-10-26 已核实；执行前再读一次，不同就停止）

- **review-gate**：`/usr/local/bin/review-gate` sha256 `d7a030cb0bcad51337f49a217297288c5ba4ea858ba5949bc9ad865299307865`，即 opencode 的身份绑定构建（`9a19094`/`50e0546`/`1e04741`；从 `1e04741` 干净重建得到同一 sha256）。运行中进程 `/proc/<MainPID>/exe` 同为 d7a030cb（用户 2026-10-26 执行 `sha256sum` 得到）。同目录备份：`review-gate.bak-20261022010102`（= 原 `0b4f1e1e…`）、`review-gate.bak-20261022010350`（= d7a030cb）。证据：`m2-tools/live-gate-provenance-20261025/`。
- **unit**：`/etc/systemd/system/review-gate.service`（User/Group reviewgate，`EnvironmentFile=-/etc/review-gate/env`，ProtectSystem=strict，ProtectHome=read-only，ReadWritePaths=/var/lib/reviewgate，PrivateTmp，NoNewPrivileges）＋ drop-in `review-gate.service.d/agents.conf`（ExecStart 带 `--agents-file /etc/review-gate/agents.json`）。`Environment=` 赋值 0 条。停服行为见 0.2 第 5 条。
- **/etc/review-gate**：目录 `root root 755`；`agents.json`、`client.token`、`env` 均为 `root reviewgate 640`；另有 `agents.json.bak-20261022010350`。
- **令牌原件**：`/home/administrator/projects/m2-tools/review-gate-deploy-20261021/tokens/{claude,opencode,codex,dsh}.token`（0600，属 administrator；明文，所有 agent 同一 OS 用户可读，只能凭据追责）。
- **audit-runner**：`audit-runner.socket`（`/run/audit-runner/runner.sock`，auditor:reviewgate 0660）、`audit-runner.service`（User=auditor，常驻监听；`active` 不等于有作业）。旧 gate 没有 milestone 路由，**不会**向 runner 发作业。
- **agent-bridge**：`/home/administrator/.local/bin/agent-bridge` sha256 `dbca255f1a9bfb0b7256f481515070ab25fda2c1cda8b23b8be5abbe6c5bb31b`（bridge 线 `4e34e25`，不含 observer）。

核对命令：

```
sudo sha256sum /usr/local/bin/review-gate /proc/$(systemctl show -p MainPID --value review-gate.service)/exe
systemctl cat review-gate.service
systemctl show review-gate.service -p KillMode -p KillSignal -p TimeoutStopUSec -p SendSIGKILL
sudo ls -l /etc/review-gate
sha256sum /home/administrator/.local/bin/agent-bridge
```

## 3. 准备 /etc（服务仍在运行；这些步骤不影响旧 gate）

### 3.1 主体与令牌对照

| gate 主体 | family | 令牌文件 | bridge agent 名 |
|---|---|---|---|
| `claude` | claude | `tokens/claude.token` | `claude` |
| `opencode` | deepseek | `tokens/opencode.token` | `opencode` |
| `codex` | openai | `tokens/codex.token` | `codex` / `codex-collab` |
| `dsh` | deepseek | `tokens/dsh.token` | `dsh` |
| `legacy-claude`（显式 legacy 条目） | claude | `/etc/review-gate/client.token` | — |

M2 的 implementers/vendors 比较 bridge 名；gate 的属主与 writers 比较主体名。两者不同名是已知事实，E4 报告照此表写。

### 3.2 生成 identity.json（root；不打印令牌）

转换器只接受 0600 或更严格的令牌文件，而线上 `client.token` 是 `root reviewgate 640`（服务要读它，原文件不改），所以先在 root 专属目录建一个 0600 副本作为 `--legacy`，转换后立即删除：

```
sudo install -d -m 0700 -o root -g root /root/m2-id
sudo install -m 0600 -o root -g root /etc/review-gate/client.token /root/m2-id/client.token.copy
sudo /home/administrator/projects/Chrome-MCP-m2-build-b423a57-target-gr/release/review-gate admin identity-from-agents \
  --credential claude=claude=/home/administrator/projects/m2-tools/review-gate-deploy-20261021/tokens/claude.token \
  --credential opencode=deepseek=/home/administrator/projects/m2-tools/review-gate-deploy-20261021/tokens/opencode.token \
  --credential codex=openai=/home/administrator/projects/m2-tools/review-gate-deploy-20261021/tokens/codex.token \
  --credential dsh=deepseek=/home/administrator/projects/m2-tools/review-gate-deploy-20261021/tokens/dsh.token \
  --legacy /root/m2-id/client.token.copy \
  --agents /etc/review-gate/agents.json \
  --out /root/m2-id/identity.json
sudo rm -f /root/m2-id/client.token.copy
sudo ls -l /root/m2-id
```

最后的 `ls` 里不应再有 `client.token.copy`。转换器若报错退出：**先**执行 `sudo rm -f /root/m2-id/client.token.copy`，再停止报告，不留明文副本。演练（同样的权限，用待部署的二进制，无 root）：`REVIEW_GATE_BIN=…-target-gr/release/review-gate bash scripts/m2-e2e/tests/test_identity_migration.sh`：直接用 0640 的 legacy 文件被拒、用 0600 副本成功、5 个哈希等于去首尾空白后的令牌 sha256、输出里没有令牌、原文件内容与权限不变（结果见 selftest-output.txt）。

预期：5 行 `<主体> <family> sha256 <前 12 位>…`，最后一行 `written … (sha256 …)`，exit 0。它同时证明线上 agents.json 的令牌集合与 family 和这 4 个令牌文件完全一致；不一致时它会按条目序号报错并退出非 0：此时**停止**（说明线上映射已变），不要手改。

```
sudo install -m 0640 -o root -g reviewgate /root/m2-id/identity.json /etc/review-gate/identity.json
sudo stat -c '%U %G %a %n' /etc/review-gate/identity.json
namei -l /etc/review-gate/identity.json
```

预期 `root reviewgate 640`；`namei -l` 列出的 `/`、`/etc`、`/etc/review-gate` 都属 root，且组和其他用户都**没有写位**（Codex：`read_admin_file` 只检查文件与直接父目录，整条路径在这里核验）。

**客户端令牌核对**（root；只输出名字与结论，不输出令牌或哈希全文）：每个会在部署后访问 gate 的客户端令牌文件，都必须能被 identity.json 认证。

```
T=/home/administrator/projects/m2-tools/review-gate-deploy-20261021/tokens
sudo python3 - /etc/review-gate/identity.json /home/administrator/.config/review-gate/token "$T/claude.token" "$T/opencode.token" "$T/codex.token" "$T/dsh.token" <<'PY'
import hashlib, json, sys
ids = json.load(open(sys.argv[1]))
known = {c["sha256"]: p["id"] for p in ids["principals"] for c in p["credentials"]}
for f in sys.argv[2:]:
    h = hashlib.sha256(open(f).read().strip().encode()).hexdigest()
    print(f, "->", known.get(h, "NOT IN identity.json"))
PY
```

预期 5 行：`/home/administrator/.config/review-gate/token -> legacy-claude`（Claude 当前用的令牌），以及 `claude.token -> claude`、`opencode.token -> opencode`、`codex.token -> codex`、`dsh.token -> dsh`。部署后 Claude 改用 `tokens/claude.token`（主体 `claude`，见第 7.3 节），因为 `writers` 只列 `claude`。如果 opencode、Codex 或 DSH 实际使用的令牌文件不是上面这几个，把它的路径追加到命令末尾一并核对；任何一行 `NOT IN identity.json` 都要先由用户决定（换用登记过的令牌，或补登记），再继续。

### 3.3 milestones.json

```
grep -n '"writers"\|"implementers"\|"repo_id"' /home/administrator/projects/Chrome-MCP-m2/deploy/m2/milestones.sample.json
sudo install -m 0640 -o root -g reviewgate /home/administrator/projects/Chrome-MCP-m2/deploy/m2/milestones.sample.json /etc/review-gate/milestones.json
sudo stat -c '%U %G %a %n' /etc/review-gate/milestones.json
```

预期 `"writers": ["claude"]`（0.3 已确认）、implementers `["claude","opencode"]`、repo_id 如 0.5；文件 `root reviewgate 640`。

### 3.4 observer 令牌与注册

（与上一版相同：令牌在同一个 root shell 中生成、写入、只打印 sha256；把哈希写进 `~/.config/agent-bridge/observers.json`；不复用任何 agent 令牌。）

```
sudo install -m 0600 -o reviewgate -g reviewgate /dev/null /etc/review-gate/bridge-observer.token
sudo sh -c 'umask 077
T=$(head -c 48 /dev/urandom | base64 -w0)
printf "%s" "$T" > /etc/review-gate/bridge-observer.token
printf "observer token sha256: %s\n" "$(printf "%s" "$T" | sha256sum | cut -d" " -f1)"
unset T'
sudo stat -c '%u %U %G %a %n' /etc/review-gate/bridge-observer.token
read -r -p 'observer hash: ' HASH
HASH="$HASH" python3 -c 'import json,os,pathlib; p=pathlib.Path(os.path.expanduser("~/.config/agent-bridge/observers.json")); d=json.loads(p.read_text()) if p.exists() else {}; d["review-gate"]=os.environ["HASH"]; p.write_text(json.dumps(d,indent=2,ensure_ascii=False)+"\n")'
chmod 0600 ~/.config/agent-bridge/observers.json
```

预期 uid 999、`reviewgate reviewgate 600`；observers.json 里 `review-gate` 的值就是打印的哈希。

### 3.5 drain 文件的位置（不要创建）

drain 文件是 `/etc/review-gate/drain`（drop-in 里的 `--drain-file`）。目录 `/etc/review-gate` 属 root、755，gate 只能读；只有 root 能创建或删除它。**现在不要创建**：它一旦存在，新 gate 启动后立即进入排空，拒绝一切写入。它只在以后停新 gate 时使用（第 4.6 节）。gate 启动时若该路径不可检查（非 NotFound 的错误）会拒绝启动。

### 3.6 drop-in（暂不 reload）

```
sudo install -m 0644 -o root -g root /home/administrator/projects/Chrome-MCP-m2/deploy/m2/review-gate.service.d-milestones.conf /etc/systemd/system/review-gate.service.d/milestones.conf
sudo cp -p /etc/systemd/system/review-gate.service.d/agents.conf /root/m2-id/agents.conf.bak
sudo rm /etc/systemd/system/review-gate.service.d/agents.conf
ls -l /etc/systemd/system/review-gate.service.d/
```

两个 drop-in 都写 `ExecStart=` 时只有最后一个生效，所以旧的 `agents.conf` 必须移走（原件保存在 `/root/m2-id/agents.conf.bak`）。**这里不执行 `daemon-reload`**：运行中的旧 gate 不受影响，新 unit 在第 7 节才加载。

## 4. 计划内停机

### 4.1 受影响的客户端（全部停用，记录 PID 与时间）

```
systemctl --user stop agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service
systemctl --user is-active agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service
```

另外：Claude 会话不发起 gate 请求（执行者负责通知）；DSH 停止使用 gate；用户本人不运行 `review-gate` 客户端命令。预期两个 worker `inactive`。

### 4.2 停服前的记录（不作为排空证明）

```
date -u +%FT%TZ
systemctl show review-gate.service -p MainPID -p ActiveEnterTimestamp
sudo ss -tnp '( sport = :7878 or dport = :7878 )'
ps -ef | grep -E '[r]eview-gate (review|task|record|stage|guard|mcp)'
sudo journalctl -u review-gate.service --since '-60 min' --no-pager | tail -40
```

记录所有仍然存在的连接与客户端进程。这些只是记录：它们不能证明旧进程里没有已接纳但未完成的请求（0.2 第 1–4 条）。

### 4.3 停旧 gate

```
sudo systemctl stop review-gate.service
systemctl is-active review-gate.service
sudo journalctl -u review-gate.service --since '-5 min' --no-pager | tail -20
```

预期 `inactive`。journal 里有没有 `Killing process … with signal SIGKILL` 要照实记录（有就说明 90 秒内没退出）。

### 4.4 停服后的盘点（先记录，什么都不删）

```
sudo bash /home/administrator/projects/Chrome-MCP-m2/scripts/m2-e2e/state_manifest.sh /var/lib/reviewgate /root/m2-id/state-M0.sha256
```

脚本在 root 的 shell 里写清单（重定向在 sudo 之内），对**每个**普通文件（包括 `*.tmp`）取 sha256，另把 `*.tmp` 列出；任何文件读不了或任何一步失败都退出非 0，且不留下清单文件；已存在的清单不覆盖。`.tmp` 若存在：记录、保留原样、报告用户（可能有写入被中断）。M0 是停服时的全量清单，第 7.2 节用它判断新 gate 写了什么。

### 4.5 runner 与外部审计（有上限，不强杀）

旧 gate 不向 runner 发作业；这里确认没有残留：

```
sudo bash /home/administrator/projects/Chrome-MCP-m2/scripts/m2-e2e/runner_wait.sh 30   # 参数单位是分钟：总上限 30 分钟
```

退出 0：除 runner 自身（`systemctl show -p MainPID audit-runner.service`；它的参数含 `--codex`，也会被 `pgrep -af codex` 匹配）外没有 auditor 的 codex 进程，runner.sock 上没有 `ESTAB`/`CLOSE-WAIT`。退出 2：`pgrep`、`ss` 或 `systemctl` 读取失败，或 MainPID 不是数字。退出 3：30 分钟到时仍有活动（脚本打印起止时间与仍在的进程/连接）。2 或 3：**停止并报告**，不 kill，不继续第 5 节。只有退出 0 才继续：

```
sudo systemctl stop audit-runner.socket
sudo systemctl stop audit-runner.service
systemctl is-active audit-runner.socket audit-runner.service
```

预期两个 `inactive`。

### 4.6 以后停新 gate（X 起）：用 drain

```
sudo touch /etc/review-gate/drain
bash /home/administrator/projects/Chrome-MCP-m2/scripts/m2-e2e/drain_wait.sh http://127.0.0.1:7878 60   # 第二个参数单位是分钟：总上限 60 分钟
```

只有退出 0 才执行 `sudo systemctl stop review-gate.service`。脚本有总截止时间，每次请求有连接/总时限，严格校验字段与类型（`drain.draining`、`drain.idle` 为布尔，`active_ops`、`failure_count`、`m2_pending` 为整数，null 拒绝），只有 draining、idle、0 在途、0 失败、0 Pending 才算可停。退出 2 = 读取或解析失败（不重试），退出 3 = 到时仍未 idle：都**停止并报告**（`drain.failures`），不停服务。drain 文件不会被脚本删除：它保留到新服务验证通过、用户决定恢复接纳时，由 root `rm /etc/review-gate/drain` 后再重启。演练：`bash scripts/m2-e2e/tests/test_drain_wait.sh`（idle、HTTP 500、坏 JSON、pending 为 null、缺字段、布尔冒充整数、有失败、到时、连接被拒，共 9 种）。

## 5. 原子替换 review-gate 与 audit-runner

```
TS=$(date -u +%Y%m%dT%H%M%SZ)
sudo cp -p /usr/local/bin/review-gate /usr/local/bin/review-gate.bak-$TS-$(sha256sum /usr/local/bin/review-gate | cut -c1-12)
sudo cp -p /usr/local/lib/audit-runner/audit-runner /usr/local/lib/audit-runner/audit-runner.bak-$TS-$(sha256sum /usr/local/lib/audit-runner/audit-runner | cut -c1-12)
sha256sum /home/administrator/projects/Chrome-MCP-m2-build-b423a57-target-gr/release/review-gate /home/administrator/projects/Chrome-MCP-m2-build-b423a57-target-gr/release/audit-runner
sudo install -o root -g root -m 0755 /home/administrator/projects/Chrome-MCP-m2-build-b423a57-target-gr/release/review-gate /usr/local/bin/.review-gate.new
sudo install -o root -g root -m 0755 /home/administrator/projects/Chrome-MCP-m2-build-b423a57-target-gr/release/audit-runner /usr/local/lib/audit-runner/.audit-runner.new
sudo sha256sum /usr/local/bin/.review-gate.new /usr/local/lib/audit-runner/.audit-runner.new
sudo mv -f /usr/local/bin/.review-gate.new /usr/local/bin/review-gate
sudo mv -f /usr/local/lib/audit-runner/.audit-runner.new /usr/local/lib/audit-runner/audit-runner
sudo sha256sum /usr/local/bin/review-gate /usr/local/lib/audit-runner/audit-runner
```

预期：备份名记下（第 10 节用）；安装源、`.new`、替换后的 sha256 都等于第 1 节；任何一处不符就删除 `.new` 并停止。

## 6. 替换 agent-bridge

只动 `/home/administrator/.local/bin/agent-bridge`（属 administrator，**不用 sudo**）与 `systemctl --user` 单元。

### 6.1 预检（不符就停止，不覆盖）

```
BIN=/home/administrator/.local/bin/agent-bridge
OLD=dbca255f1a9bfb0b7256f481515070ab25fda2c1cda8b23b8be5abbe6c5bb31b
NEW_SRC=/home/administrator/projects/Chrome-MCP-m2-build-b423a57-target-bridge/release/agent-bridge
NEW=d9ab3e50895ed729b076d031719738e8157516084b3ae8e2aa2620366b41bca2
[ "$(sha256sum "$BIN" | cut -d' ' -f1)" = "$OLD" ] || { echo 'STOP: the live agent-bridge is not dbca255f'; false; }
[ "$(sha256sum "$NEW_SRC" | cut -d' ' -f1)" = "$NEW" ] || { echo 'STOP: the build is not d9ab3e50'; false; }
ps -ef | grep -E '[o]pencode run --format json|[c]odex(\.exe)? exec' || echo 'no worker turn in progress'
bash /home/administrator/projects/Chrome-MCP-m2/scripts/m2-e2e/bridge_observer_e2e.sh "$NEW_SRC"
```

预期：两个 `[ … ]` 都不打印 STOP（打印了就停止，不往下）；没有在途的 worker turn（worker 已在 4.1 停止；有就等它结束，不 kill）；自检最后一行 `ALL OK`。

### 6.2 备份与原子替换

```
TS=$(date -u +%Y%m%dT%H%M%SZ); echo "$TS"
cp -p "$BIN" "$BIN.bak-$TS"
[ "$(sha256sum "$BIN.bak-$TS" | cut -d' ' -f1)" = "$OLD" ] || { echo 'STOP: backup differs'; false; }
install -m 0755 "$NEW_SRC" "$BIN.new-$TS"
[ "$(sha256sum "$BIN.new-$TS" | cut -d' ' -f1)" = "$NEW" ] || { echo 'STOP: .new differs'; rm -f "$BIN.new-$TS"; false; }
mv -f "$BIN.new-$TS" "$BIN"
sha256sum "$BIN"
systemctl --user restart agent-bridge.service
systemctl --user is-active agent-bridge.service
curl -sS --connect-timeout 5 --max-time 10 -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7879/v1/health
journalctl --user -u agent-bridge.service -n 30 --no-pager | grep -i 'read-only observers'
```

预期：没有 STOP；替换后 sha256 = `d9ab3e50…`；`active`；health `200`；日志有 `read-only observers ["review-gate"]`。记下 `TS`（回退用）。worker 不在这里重启（第 7.3 节）。

### 6.3 agent-bridge 回退（与 gate 独立；bridge 不读 gate 的 state）

```
BIN=/home/administrator/.local/bin/agent-bridge
TS=<6.2 记下的值>
OLD=dbca255f1a9bfb0b7256f481515070ab25fda2c1cda8b23b8be5abbe6c5bb31b
[ "$(sha256sum "$BIN.bak-$TS" | cut -d' ' -f1)" = "$OLD" ] || { echo 'STOP: backup differs'; false; }
install -m 0755 "$BIN.bak-$TS" "$BIN.rollback-$TS"
[ "$(sha256sum "$BIN.rollback-$TS" | cut -d' ' -f1)" = "$OLD" ] || { echo 'STOP'; rm -f "$BIN.rollback-$TS"; false; }
mv -f "$BIN.rollback-$TS" "$BIN"
sha256sum "$BIN"
systemctl --user restart agent-bridge.service agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service
systemctl --user is-active agent-bridge.service agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service
curl -sS --connect-timeout 5 --max-time 10 -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7879/v1/health
```

先 install 到 `.rollback-$TS` 再 rename，备份 `.bak-$TS` 保留。`observers.json` 可以留着（旧 bridge 不读）。

## 7. 启动与验证（恢复接纳前）

### 7.1 启动

```
sudo systemctl daemon-reload
systemctl cat review-gate.service | tail -5
sudo systemctl start audit-runner.socket
sudo systemctl start review-gate.service
systemctl is-active audit-runner.socket review-gate.service
sudo journalctl -u review-gate.service -n 30 --no-pager
curl -fsS http://127.0.0.1:7878/health | python3 -m json.tool
curl -fsS http://127.0.0.1:7878/pubkey
```

预期：生效的 ExecStart 带 `--identity-config`、`--agents-file`、`--milestones`、`--drain-file`；两个单元 `active`；journal 有 `agents file /etc/review-gate/agents.json: 4 entries match the identity configuration` 与 `review-gate listening on 127.0.0.1:7878`；`/health` 的 `drain.configured=true`、`drain.draining=false`、`m1_jobs_running=0`、`m2_pending=0`；pubkey 等于 `ITU9Oc3AbiC/Q/LYPwld2b+OC2rxlqHbvCO7IQcGdXw=`。启动失败：看 journal（身份配置、agents-file、writers、令牌文件 owner/mode 都在监听之前检查），修正后再启；不要跳过检查，不要回到旧 drop-in。

### 7.2 新 gate 写了什么

```
sudo bash /home/administrator/projects/Chrome-MCP-m2/scripts/m2-e2e/state_manifest.sh /var/lib/reviewgate /root/m2-id/state-M1.sha256
sudo diff /root/m2-id/state-M0.sha256 /root/m2-id/state-M1.sha256
```

预期差异只有新增：`state/service.lock` 与 `state/milestones/` 下的初始化文件（owner.lock、owner.json、日志与检查点、`remote.git`、`anon-home`）。`tasks/`、`reviews/`、`keys/`、`cache/`、`relay/` **不应有任何变化**；有就停止并报告。从这一刻起回滚受第 10 节约束。

### 7.3 恢复接纳

```
install -m 0600 /home/administrator/projects/m2-tools/review-gate-deploy-20261021/tokens/claude.token /home/administrator/.config/review-gate/token.claude
REVIEW_GATE_TOKEN_FILE=/home/administrator/.config/review-gate/token.claude review-gate task list | head -3
systemctl --user start agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service
systemctl --user is-active agent-bridge.service agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service
```

预期 `task list` 正常返回（`claude` 主体只看到自己的任务；属主未知的任务不在列表中，见 8.2）；三个单元 `active`。Claude 之后用 `token.claude`。

## 8. 部署后的管理步骤

### 8.1 milestone（用户已决定 U-b、U-g；dry run 后再 `--yes`）

`admin milestone` 以 `reviewgate` 身份离线读写 state，命令与上一版相同（confirm m2；link 2 个；ignore 5 个）。前提：E4 register 已执行。每条先不带 `--yes` 记录输出，核对后再加 `--yes`。

```
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json confirm m2
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes confirm m2
```

link-task（用户决定 U-g，2026-10-20，按建议 link 这 2 个）。每个任务先 dry run，再加 `--yes`：

```
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json link-task m2 --task dsh-review-m2n2 --reason 'user decision U-g (2026-10-20): link dsh-review-m2n2 to phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes link-task m2 --task dsh-review-m2n2 --reason 'user decision U-g (2026-10-20): link dsh-review-m2n2 to phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json link-task m2 --task dsh-review-m2s6b-20261019 --reason 'user decision U-g (2026-10-20): link dsh-review-m2s6b-20261019 to phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes link-task m2 --task dsh-review-m2s6b-20261019 --reason 'user decision U-g (2026-10-20): link dsh-review-m2s6b-20261019 to phase m2'
```

ignore-task（用户决定 U-g，2026-10-20，ignore 这 5 个）。每个任务先 dry run，再加 `--yes`：

```
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json ignore-task m2 --task auto-iterate-generic-proposal-20261019 --reason 'user decision U-g (2026-10-20): ignore auto-iterate-generic-proposal-20261019 for phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes ignore-task m2 --task auto-iterate-generic-proposal-20261019 --reason 'user decision U-g (2026-10-20): ignore auto-iterate-generic-proposal-20261019 for phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json ignore-task m2 --task bridge-settle-conflict-20261018 --reason 'user decision U-g (2026-10-20): ignore bridge-settle-conflict-20261018 for phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes ignore-task m2 --task bridge-settle-conflict-20261018 --reason 'user decision U-g (2026-10-20): ignore bridge-settle-conflict-20261018 for phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json ignore-task m2 --task opencode-handover-20261019 --reason 'user decision U-g (2026-10-20): ignore opencode-handover-20261019 for phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes ignore-task m2 --task opencode-handover-20261019 --reason 'user decision U-g (2026-10-20): ignore opencode-handover-20261019 for phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json ignore-task m2 --task opencode-perm-fix-20261019-claude --reason 'user decision U-g (2026-10-20): ignore opencode-perm-fix-20261019-claude for phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes ignore-task m2 --task opencode-perm-fix-20261019-claude --reason 'user decision U-g (2026-10-20): ignore opencode-perm-fix-20261019-claude for phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json ignore-task m2 --task unified-bridge-source-provenance-20261019 --reason 'user decision U-g (2026-10-20): ignore unified-bridge-source-provenance-20261019 for phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes ignore-task m2 --task unified-bridge-source-provenance-20261019 --reason 'user decision U-g (2026-10-20): ignore unified-bridge-source-provenance-20261019 for phase m2'
```

做完后只读核对：

```
REVIEW_GATE_TOKEN_FILE=/home/administrator/.config/review-gate/token.claude /usr/local/bin/review-gate milestone status --phase m2 > /tmp/m2-status-after-admin.json
python3 -m json.tool /tmp/m2-status-after-admin.json
```

预期 `.state.registered=true`。`reasons` 里出现本清单以外任务名的 `unmatched bridge task <id>`：**不要处理**，记录该任务 id 与完整输出，停止，问用户。

### 8.1a 本手册首版漏写、真实执行（2026-10-26）时补做的两步

`status` 在没有请求时不检查来源与厂商；这两类问题只在第一次 `milestone request` 时报出（409，什么都不写）。所以下面两步必须在发请求之前做完；即使做完，请求仍可能报出新的原因，照样停止并报告。

1. **厂商表**。U-d 只决定了标签，没有写入日志；`Vendors::first()` 不含 `codex-collab`、`opencode`，也不含审计方的值 `openai`。用 `vendors-set` 写入新版本（版本号必须更大；每条匹配的规则都会计入，不是先匹配先用，所以同一名字的两条规则会冲突）。reason 写明用户的决定，以及"按批准的标签判为独立，实际厂商相同"（codex、codex-collab 与审计方都是 OpenAI；opencode 实际是 DeepSeek）。2026-10-26 写入的是 v2（seq 9）和 v3（seq 80，用户把 U-d 扩展到 codex）。

第一条是 dry run（不带 `--yes`，什么都不写，打印将写入的表和 `table_sha256`）；核对输出与文件一致后，再执行第二条（带 `--yes`，实际写入）。两条都不交互：

```
# 1) dry run: nothing is written
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json vendors-set --file <表的 JSON，reviewgate 可读> --reason '<用户的决定与实际厂商>'
# 2) write, after the dry run's output was checked
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes vendors-set --file <同上> --reason '<同上>'
```

2. **窗口内未归类的任务**。窗口从首个提交（或登记时间）延续到最后一个提交（或登记时间），所以登记后新建的 implementer 任务也在窗口内。不能按名字批量处理：逐个任务核对目的、parent 链、实际交付，以及是否进入被审范围（`git merge-base --is-ancestor`）。claude 不是参与方的任务，经用户授权后用 bridge 的 observer 元数据核对（只读，不打印令牌）。清单与逐项理由交用户批准，再逐条 `link-task` / `ignore-task`，每条都是先 dry run 再 `--yes`。

### 8.2 属主未知的旧任务

```
sudo -u reviewgate /usr/local/bin/review-gate admin task --state /var/lib/reviewgate/state unowned
```

所有旧任务（包括 `m2-step8-deploy`、`m2-step8-identity` 等正在用的）都会列在这里，部署后**不能**被任何主体写入。逐个由用户决定是否认领；认领必须在新 gate **停止**时进行（assign 需要 state 锁）：

```
sudo -u reviewgate /usr/local/bin/review-gate admin task --state /var/lib/reviewgate/state assign --id <task> --principal <p> --reason '<用户的决定>' --expect-updated-at '<unowned 列出的 updated_at>' --identity-config /etc/review-gate/identity.json
sudo -u reviewgate /usr/local/bin/review-gate admin task --state /var/lib/reviewgate/state assign --id <task> --principal <p> --reason '<用户的决定>' --expect-updated-at '<同上>' --identity-config /etc/review-gate/identity.json --yes
```

服务在运行时，带 `--yes` 的 assign 会报 `a gate service holds this state` 并拒绝：先按 4.6 drain、停服，再 assign，再启动。

## 9. 验收脚本

- `scripts/m2-e2e/e1_failclosed.sh`（无 root）：隔离 state/端口/令牌。坏配置（相对路径、空字段、未知字段、空 implementers、**空 writers**、**writer 不是已启用主体**、令牌 0644、令牌过短）都必须非 0 退出且不监听；好实例上两种"未登记"都是 200 + `registered=false`；部署构建对 file:// 远端的 `sync` 是 `Stop("remote unreachable: the remote must be an https URL without credentials")`（匿名读取只接受 https），fault 构建则能读这个夹具远端（脚本按二进制里是否有 `REVIEW_GATE_FAULT` 区分并严格断言）。分别对第 1 节的部署构建与 fault 构建各跑一次：`REVIEW_GATE_BIN=<path> bash scripts/m2-e2e/e1_failclosed.sh`。
- 演练（无 root，部署前跑，原始输出在 selftest-output.txt）：`tests/test_drain_wait.sh`、`tests/test_state_manifest.sh`、`tests/test_runner_wait.sh`、`tests/test_identity_migration.sh`（需 `REVIEW_GATE_BIN` 指向待部署构建）。
- `scripts/m2-e2e/e2_anonymous.sh`（root，只读，部署后执行一次）：`sudo bash scripts/m2-e2e/e2_anonymous.sh b423a579139fb5e7f47e64e976755a900ade547c 2>&1 | tee -a <log>`（head 作为参数传入，不依赖 sudo 的环境变量处理）。环境只输出名字（`Environment=` 只计数）；用一次只读 status 让 gate 自己做匿名读取，报告 `sync`（`Stop(…)` 即读取失败，脚本停止）；读取在按运行中 unit 解析出的沙箱属性（`systemd-run --property=…`）下执行，每条命令检查退出码，用部署的二进制和固定公钥验签。未镜像的属性如实列出（RestrictAddressFamilies）。
- `scripts/m2-e2e/e4_preflight.sh`、`e4_run.sh`：默认 HEAD = X。E4 的 register/request 是 milestone 写入，只有 `writers` 里的主体可以：用 `REVIEW_GATE_TOKEN_FILE=/home/administrator/.config/review-gate/token.claude bash scripts/m2-e2e/e4_run.sh …`（`client.token` 是 `legacy-claude`，会得到 403）。E4 结论按 0.5 第 1 条措辞；publish 需显式 `--publish`。

## 10. 失败与回滚

**常规路径不是恢复旧二进制。** 新 gate 启动失败、或第 7 节任何检查不符：

1. 保持客户端停用；`sudo systemctl stop review-gate.service`（若已启动）。
2. 保留全部 state、`.tmp`、日志、M0/M1 清单；不删除、不截断、不恢复快照、不回滚远端 notes。
3. 记录失败输出，报告用户，前向修复（修配置或修代码、重建、重审）。

**禁止**在新 gate 写入任何任务之后启动旧二进制（d7a030cb 或更早）：旧二进制不认识 `principal`，读改写任务时会丢掉它，并重新允许跨身份操作（实证：`m2-tools/compat-ident.txt`）。"写入"按全量清单判断：只要 M1 之后的清单与 M0 相比，`tasks/`、`reviews/`、`keys/`、`cache/`、`relay/` 有任何变化，就不能恢复旧二进制。即使没有变化，恢复旧二进制也需要用户另行明确决定，并且要同时恢复原 unit/config 组合（`/root/m2-id/agents.conf.bak` 放回、移除 `milestones.conf`、`daemon-reload`），`milestones/` 目录保留不删。

agent-bridge 的回退见第 6 节（与 gate 独立；bridge 不读 gate 的 state）。

## 11. 本手册明确不做的事

- 编写本手册时没有执行任何 `sudo`，没有改 `/etc`、`/usr/local`、`/var/lib/reviewgate`。
- 不在真实环境做故障注入；fault 构建只用于对照。
- 不用 `safe.directory=*`；不读取或打印令牌、`/etc/review-gate/env` 或 `Environment=` 的取值。
- 不为得到 APPROVE 重复请求；被中断的审查不自动重做（0.2）。
- 不中断仍在运行的 runner/Codex 审计；不强杀进程。
- 不把 n31 被拒绝的"防火墙排空"当作已证明的排空。
