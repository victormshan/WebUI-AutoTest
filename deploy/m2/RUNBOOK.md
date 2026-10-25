# M2 第 8 步运行手册：部署 review-gate + audit-runner + agent-bridge 并做 E1/E2/E4

> **BLOCKED（2026-10-25）：不要执行本手册的任何步骤。**
>
> 1. Codex 实现复核 n8 为 REWORK（停机竞态、gate 自身拉取的匿名性、E2 打印与沙箱）。代码部分
>    （drain、匿名 git）已在后续提交修复；本手册的停机、E2 与构建段尚未按修复重写。
> 2. **新发现，待核实**：线上 `/usr/local/bin/review-gate` 已不是第 2 节记录的 `0b4f1e1e…`，而是
>    2026-10-22 01:03 安装的 `d7a030cb…`，ExecStart 带 `--agents-file /etc/review-gate/agents.json`
>    （来源待核实：opencode 的身份绑定构建）。按本手册部署会回退线上身份绑定。Codex n12：先在隔离
>    分支整合并核实来源，重写第 2 节、unit、回滚与状态兼容矩阵。
> 3. 线上旧 gate 不支持 drain，首次安装的排空（bootstrap）仍无可核验方案：部署段在解决前保持阻断。

状态：**待用户执行**。本目录的文件由 opencode 准备并自测；所有 `sudo`/root 命令只在这里列出，
**没有执行过**。线上服务、`/etc`、`/usr/local`、`/var/lib/reviewgate`、`~/.local/bin` 均未改动。

固定提交（完整 40 位）：`bbc5aeb74005d76ad8fde20b89a5e9337f5c7aec`
这是 **`m2-step1` 分支的 head**（`git -C /home/administrator/projects/Chrome-MCP rev-parse m2-step1`
应输出该值）。它包含线上 bridge 线 `4e34e25` 与只读 observer 的整合（提交 `4951e92`）。
`deploy/m2/` 与 `scripts/m2-e2e/` 的产物提交在它之上（worktree `Chrome-MCP-m2-deploy`，分支
`m2-step8-deploy-final`，基线 `1ff89a8`），**不改变二进制对应的源码**，所以三个二进制都对应
`bbc5aeb…`。

## 0. 执行前：记录日志、确认仍未变的事

**必须记录全过程**：在维护窗口开始时先打开一个记录会话（二选一），把每条命令和**完整输出**
保存下来；出现任何与本手册预期不符的输出，**立即停止并报告**，不要继续下一步。

```
script -a /home/administrator/m2-step8-$(date -u +%Y%m%dT%H%M%SZ).log
```

或每条命令使用 `2>&1 | tee -a <log>`。日志里不得出现任何令牌或 `/etc/review-gate/env` 的内容。

仍需用户确认/已知的偏离：

1. **U-d 厂商语义**：用户已于 2026-10-20 明确批准保留现有标签
   （`codex-collab` → `codex`、`opencode` → `opencode`），并接受“标签判定与实际厂商不一致”的偏离：
   `codex-collab` 与 `auditor=openai` 实际同为 OpenAI；`opencode` 实际运行 DeepSeek。
   E4 的结论必须写成“**按批准的标签判为独立，实际厂商相同**”，不得写成“厂商独立”。
   换其他厂商审计者也不在本 runner 范围内。
2. **`repo_id`**：bridge q1 已答复（2026-10-20）：用 `github.com/victormshan/WebUI-AutoTest`。
   样例已按此值写好。它进入每一条签名记录，**首次登记后不能改**；用户在安装 `/etc` 前做最终确认。
3. **implementers**：按已知事实配置为 `["claude","opencode"]`（Codex n5 第 3 条）。
   缩减为 `["claude"]` 是来源完整性标准的变更，须用户明确决定，不能当作等价配置。
4. **U-a=1 部署基线**：部署 `m2-step1` 的固定提交
   `bbc5aeb74005d76ad8fde20b89a5e9337f5c7aec`，**不合并 main**。main 上的 CI 不检查这个修订；
   不要把 main 的 CI 结果当作本次部署的验收。
5. **维护窗口**：第 6 节替换 agent-bridge 会中断所有 agent 的 worker turn（之后会自动重连）。
   整个第 4–7 节必须在同一维护窗口内完成。
6. 本手册不包含任何 `safe.directory=*` 的用户侧命令；只允许精确仓库路径。

## 1. 构建清单（manifest，三个二进制，两次独立构建）

**三个二进制由两次独立构建产生，每次都用新的空 `CARGO_TARGET_DIR`**，两次都在同一个
**干净的 detached worktree**、HEAD 为 `bbc5aeb…`：

- (a) `cargo build --release --locked -p review-gate -p audit-runner` → gate + runner；
- (b) `cargo build --release --locked -p agent-bridge` → 待部署的 agent-bridge。

**为什么 agent-bridge 必须单独构建**：把它和 review-gate/audit-runner 放在同一次 Cargo 调用里，
Cargo 会合并共享依赖的 feature，产出另一个二进制（`04a2260e…`）；那个二进制**从未经过**
observer 的真实二进制验收。通过验收、必须部署的是 (b) 的单包产物 `d9ab3e50…`。
两个构建可以共用 worktree，但**不能共用 target 目录**。

（Cargo 产物不保证跨目录/跨机器逐字节可复现，部署只能安装本清单记录的那次构建。）

**本次已经完成这两次构建并记录了下面的值；不要重跑下面的命令块**（它只是可从零复现的记录），
直接核对表格和 sha256 即可。只有需要独立复核时，才另建**新的空** target 目录重跑。

```
git -C /home/administrator/projects/Chrome-MCP-m2-deploy worktree add --detach /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb bbc5aeb74005d76ad8fde20b89a5e9337f5c7aec
cd /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb
source ~/.cargo/env
test -z "$(ls -A /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-gr 2>/dev/null)" || { echo 'target-gr not empty'; exit 1; }
test -z "$(ls -A /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-bridge 2>/dev/null)" || { echo 'target-bridge not empty'; exit 1; }
CARGO_TARGET_DIR=/home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-gr cargo build --release --locked -p review-gate -p audit-runner
CARGO_TARGET_DIR=/home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-bridge cargo build --release --locked -p agent-bridge
```

如果 worktree 或 target 目录已经存在（本次准备的产物），**不要删除或重建**，直接核对下面的值；
要独立复核，另建两个新的空 target 目录重跑。

固定提交与构建身份（本清单的实际值）：

| 项 | 值 |
|---|---|
| commit | `bbc5aeb74005d76ad8fde20b89a5e9337f5c7aec` |
| tree | `5f264f4d0b1237d271cdc48084e6f24c151e621f` |
| `git status --porcelain --untracked-files=all` | 空 |
| `rustc -Vv` | `rustc 1.99.0 (b940084d7 2026-09-28)` / `host: x86_64-unknown-linux-gnu` / `LLVM 23.1.1` |
| `cargo -V` | `cargo 1.99.0 (5f94df478 2026-08-27)` |

`rustc -Vv` 的完整输出：

```
rustc 1.99.0 (b940084d7 2026-09-28)
binary: rustc
commit-hash: b940084d7eb6a299eb4bfeb8e34901bc051e7ac4
commit-date: 2026-09-28
host: x86_64-unknown-linux-gnu
release: 1.99.0
LLVM version: 23.1.1
```

`cargo -V`：`cargo 1.99.0 (5f94df478 2026-08-27)`。

本次构建结果：

| 产物 | 路径 | 字节 | sha256 |
|---|---|---|---|
| review-gate | `…-target-gr/release/review-gate` | 12864728 | `378af8c0b5c3e44ce22463321dc9c00c9a6fb31b43811bbcfd11eb358f364ae5` |
| audit-runner | `…-target-gr/release/audit-runner` | 1675296 | `c9a66da9442758d85c8411064d1a67753339efab854fe491bcfc79b2293940b3` |
| agent-bridge | `…-target-bridge/release/agent-bridge` | 11748640 | `d9ab3e50895ed729b076d031719738e8157516084b3ae8e2aa2620366b41bca2` |

（表中 `…` 指 `/home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb`。）

**判定规则**：待部署的 agent-bridge **必须**是 `d9ab3e50…`；(a) 的 gate/runner 必须与上表一致。
本次 (a) 的 gate/runner 值与三包构建相同。任何一项不同：**停止并报告**，不要用参考值或旧的
target 目录顶替。`04a2260e…` 是三包一起构建的产物，**不要部署**，只作为“为什么必须分两次构建”的说明。

**实际启用的 features**（不是把“默认”当成空）：

```
cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; [print(p["name"], p["features"]) for p in json.load(sys.stdin)["packages"] if p["name"] in ("review-gate","audit-runner","agent-bridge")]'
```

本次输出：`agent-bridge {}`、`audit-runner {"fault-injection": []}`、`review-gate {"fault-injection": []}`。
三个 crate 都**没有声明 `default` feature**；review-gate / audit-runner 唯一的 feature 是
`fault-injection`，上面的 release 构建没有加 `--features`，所以**实际启用集合为空**。
Cargo.toml 可复核：`crates/{review-gate,audit-runner,agent-bridge}/Cargo.toml`。

部署前逐项复核（必须与上表完全一致；不一致就停止，不要安装）：

```
cd /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb
git rev-parse HEAD HEAD^{tree}
git status --porcelain --untracked-files=all
sha256sum \
  /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-gr/release/review-gate \
  /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-gr/release/audit-runner \
  /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-bridge/release/agent-bridge
strings -a /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-gr/release/review-gate | grep -c REVIEW_GATE_FAULT
strings -a /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-gr/release/audit-runner | grep -c AUDIT_RUNNER_FAULT
```

预期：HEAD/tree 与上表一致；status 无输出；三个 sha256 与上表一致；两个 `grep -c` 都是 `0`
（找不到字符串时 grep 退出 1，`grep -c` 仍会打印 0，可忽略它的退出码）。

**observer 真实二进制自检（部署前必跑）**：`scripts/m2-e2e/bridge_observer_e2e.sh` 是
`/home/administrator/projects/m2-tools/bridge-observer-e2e.sh` 的原样副本（sha256
`c2e1887c8087bd93a7200dc8ae2d93d8d902820ab21ffcd60f1f74b1e88adb5d`），用临时 HOME 和独立端口，
不需要 sudo，不碰线上 bridge。第 6.3 节安装前必须对 (b) 的二进制跑一次，见该节。

**对照：带 fault-injection 的独立构建**（同一修订、另一个**空** target 目录；绝不覆盖待部署产物）：

```
test -z "$(ls -A /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-fault 2>/dev/null)" || { echo 'fault target dir not empty'; exit 1; }
cd /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb
CARGO_TARGET_DIR=/home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-fault cargo build --release --locked -p review-gate -p audit-runner --features review-gate/fault-injection,audit-runner/fault-injection
strings -a /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-fault/release/review-gate | grep -c REVIEW_GATE_FAULT
strings -a /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-fault/release/audit-runner | grep -c AUDIT_RUNNER_FAULT
sha256sum /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-fault/release/review-gate /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-fault/release/audit-runner
```

本次对照结果：review-gate 有 1 处 `REVIEW_GATE_FAULT`
（sha256 `f8cb7c7977684104673ccad0923d671993af64514f404a82813a38e6f2244e90`），
audit-runner 有 3 处 `AUDIT_RUNNER_FAULT`
（sha256 `2b60911447f85d178b60d07d19a80bc8e65d1738fc8890bee73825fe24dc42a0`）。
这证明第 1 节的字符串检查确实能查出来。fault 变量名来自源码：
`REVIEW_GATE_FAULT`（`crates/review-gate/src/milestone/serve.rs`），
`AUDIT_RUNNER_FAULT`（`crates/audit-runner/src/lib.rs`，`cleanup-unconfirmed` / `sweep-procs=`）。
fault 构建只用于自测/字符串对照，**不部署**。

注意：Cargo 的产物不保证跨目录/跨机器逐字节可复现。**部署时必须安装本次构建的这三个文件**，
并在 `install` 前再次比对 sha256；不要重新构建后继续用上面的哈希。

## 2. 需要记住的线上现状（只读核对）

- `review-gate.service`：`User=reviewgate`（uid 999），当前 ExecStart 为
  `/usr/local/bin/review-gate serve --state /var/lib/reviewgate/state --listen 127.0.0.1:7878 --token-file /etc/review-gate/client.token --relay-dir /var/lib/reviewgate/relay`；
  `Restart=on-failure`、`RestartSec=3`、`ProtectSystem=strict`、`ProtectHome=read-only`、
  `ReadWritePaths=/var/lib/reviewgate`、`UMask=0022`。安装本目录的 drop-in 后，重启/退避参数
  按第 7 节的 U-c=1 设置覆盖。
- `audit-runner.socket`：`/run/audit-runner/runner.sock`，`auditor:reviewgate 0660`。
- `audit-runner.service`：`User=auditor`（uid 997），`Requires=audit-runner.socket`，
  `--allow-uid 999 --allow-cgroup /system.slice/review-gate.service --own-cgroup`。
- `audit-runner.service` 是**常驻监听循环**：空闲时 `active` 是正常的，`active` 不等于有在途作业；
  它只在“作业进程组未能确认清空”后退出，由 systemd 清理 cgroup。不要等它变 inactive。
- 作业目录：`/home/auditor/jobs/<job_id>/`，每个作业一个目录且**完成后不删除**；
  运行中的作业可能还没有 `answer.md`。因此“有目录”不等于“在途”，要结合进程、连接和目录时间判断。
- 当前远端是 https（无需改写 ssh）：`https://github.com/victormshan/WebUI-AutoTest.git`
  （`git -C /home/administrator/projects/Chrome-MCP remote get-url origin`）。
- 当前旧二进制 sha256（**仅供备份命名参考，执行前再算**）：
  `/usr/local/bin/review-gate` 为 `0b4f1e1effbd…`，
  `/usr/local/lib/audit-runner/audit-runner` 为 `69983378df3b…`。
- **agent-bridge 线上现状**：`/home/administrator/.local/bin/agent-bridge`，
  当前 sha256 `dbca255f1a9bfb0b7256f481515070ab25fda2c1cda8b23b8be5abbe6c5bb31b`，
  `agent-bridge --version` 为 `agent-bridge 0.1.0`，服务 `agent-bridge.service` 为 `active`，
  ExecStart 为 `serve --listen 127.0.0.1:7879 --mirror /mnt/d/cc-tasks/claude-bridge/tasks --step-relay-dir /home/administrator/.claude/step-relay`。
  这个二进制对应 bridge 线 **`4e34e25e77165af3689b13a1b0835568d643988a`**，**不包含** observer
  （所以 observer 只能在第 6 节替换后生效）。第 6 节会先证明线上二进制确实是该构建；证不出来就停止。
- `agent-bridge-cli@codex-collab.service`、`agent-bridge-cli@opencode.service` 是 agent-bridge 的
  原生 CLI worker，均为 `active`；替换 bridge 时按第 6 节一起重启。
- `job_timeout_secs=1800` 取自 `crates/audit-runner/src/main.rs` 的
  `--timeout-secs` 默认值 `1800`；gate 发给 runner 的等待是 `job_timeout + 60s`。

## 3. 准备 /etc 文件、drop-in 与 observer 配置

核对 `repo_id`（bridge q1 已定；用户在安装前最终确认，首次登记后不可改）：

```
grep -n repo_id /home/administrator/projects/Chrome-MCP-m2-deploy/deploy/m2/milestones.sample.json
```

核对现有配置目录并安装 `milestones.json`（root:reviewgate 0640）。目录已存在，**不要**用
`install -d` 改写它的 owner/mode：

```
sudo stat -c '%U %G %a %n' /etc/review-gate
sudo install -m 0640 -o root -g reviewgate /home/administrator/projects/Chrome-MCP-m2-deploy/deploy/m2/milestones.sample.json /etc/review-gate/milestones.json
sudo stat -c '%U %G %a %n' /etc/review-gate/milestones.json
```

预期目录仍为原来的 owner/mode；文件为 `root reviewgate 640 /etc/review-gate/milestones.json`。
失败时：若 owner/mode 不符，重新执行 `sudo chown root:reviewgate …` 和
`sudo chmod 0640 …`；gate 会拒绝 group/other 可读的令牌文件，但配置文件只做字段检查。

**observer 令牌**（owner 必须是服务的有效 UID `reviewgate`，mode 0600）。
全程**只打印哈希，不打印令牌**：令牌在同一个 root shell 里生成、写入并计算 SHA-256。

```
sudo install -m 0600 -o reviewgate -g reviewgate /dev/null /etc/review-gate/bridge-observer.token
sudo sh -c 'umask 077
T=$(head -c 48 /dev/urandom | base64 -w0)
printf "%s" "$T" > /etc/review-gate/bridge-observer.token
printf "observer token sha256: %s\n" "$(printf "%s" "$T" | sha256sum | cut -d" " -f1)"
unset T'
sudo stat -c '%u %U %G %a %n' /etc/review-gate/bridge-observer.token
sudo sha256sum /etc/review-gate/bridge-observer.token
```

预期 uid 999、`reviewgate reviewgate 600`；文件内容至少 32 字符；`sha256sum` 打印的哈希与上面
`observer token sha256:` 一致。第一条 `install` 先以 0600 建空文件，第二条在 `umask 077` 下写入，
**不存在**内容先以 0644 暴露的窗口。

哈希格式与 `agent-bridge hash-token` 一致：该命令读取 stdin 后先 `trim`，再取 SHA-256 的
64 位小写十六进制（`crates/agent-bridge/src/main.rs` 的 `Cmd::HashToken`）。这里的令牌是
`base64 -w0` 的输出，不含首尾空白，所以 `printf '%s' "$T" | sha256sum` 的字节与 `hash-token`
所见完全相同。这里已经直接算出同一个值，不需要再通过该命令读取令牌文件。

把该哈希加入 `~/.config/agent-bridge/observers.json`（只读 observer，不是 agent；observer 只能用
`GET /v1/observe/tasks[/<id>]`，不能收发任务）。当前该文件不存在；若已存在，用同一段代码合并：

```
read -r -p 'observer hash: ' HASH
HASH="$HASH" python3 -c 'import json,os,pathlib; p=pathlib.Path(os.path.expanduser("~/.config/agent-bridge/observers.json")); d=json.loads(p.read_text()) if p.exists() else {}; d["review-gate"]=os.environ["HASH"]; p.write_text(json.dumps(d,indent=2,ensure_ascii=False)+"\n")'
chmod 0600 ~/.config/agent-bridge/observers.json
python3 -m json.tool ~/.config/agent-bridge/observers.json
```

预期 `review-gate` 的值就是上面的哈希；文件不包含任何令牌明文。
observer 名字/token 哈希不能与任何 agent 相同（`check_distinct`），所以必须用新令牌，不要复用
`opencode.token`。观察者令牌只用于 `sources::read_bridge` 的元数据读取；它不授予任何写权限。
**这里不重启 bridge**：线上 bridge（`4e34e25`）还不认识 observer；第 6 节替换二进制后重启，
启动日志才会出现 `read-only observers ["review-gate"]`。

安装 drop-in（清空并重写 ExecStart，并加入 U-c=1 的重启限制；其余参数保持现状）：

```
sudo mkdir -p /etc/systemd/system/review-gate.service.d
sudo install -m 0644 -o root -g root /home/administrator/projects/Chrome-MCP-m2-deploy/deploy/m2/review-gate.service.d-milestones.conf /etc/systemd/system/review-gate.service.d/milestones.conf
sudo systemctl cat review-gate.service
```

预期最后一段包含 `ExecStart=`（空）和带 `--milestones /etc/review-gate/milestones.json` 的新行，
且旧参数一个不少；同时应看到 `[Unit]` 的 `StartLimitIntervalSec=3600`、`StartLimitBurst=5`
和 `[Service]` 的 `Restart=on-failure`、`RestartSec=10`、`RestartSteps=5`、
`RestartMaxDelaySec=300`。失败时不要启动服务：`sudo systemctl cat review-gate.service` 对照第 2 节；
参数写错会导致 gate 启动失败（`--milestones` 文件不存在或字段错误都会拒绝启动）。

## 4. 停机排空（gate / audit-runner；回退也走这一节）

原则：先阻止新请求，再排空 gate 的两条作业通道（M2 milestone 与 M1 review），stop gate，
再确认 runner 无在途作业；等待有上限，**到时不强杀活动审计**，停止操作并报告。

1. 通知各客户端暂停（Claude/opencode/DSH/webtest 等），不要再发起 review 或 milestone 请求。
2. 确认没有客户端操作：

```
pgrep -af 'review-gate (review|milestone)'
```

预期无输出（只有这条 `pgrep` 自身可能匹配时也会显示；排除即可）。

3. 确认 milestone 各阶段没有 Pending，M1 review 通道也没有在跑的作业。
   对每个已登记阶段执行（示例为 `m2`；其余阶段逐个替换）：

```
sudo REVIEW_GATE_TOKEN_FILE=/etc/review-gate/client.token /usr/local/bin/review-gate milestone status --phase m2 | python3 -m json.tool
```

预期 `.state.current` 为 null，或 `.state.current.state` 不是 `Pending`；`.released` 可以为 false。
CLI 在未释放时**退出码为 1**，这是“尚未 released”的诊断结果，不是查询失败；以 JSON 为准。
若出现 `Pending`，等它的 job 完成或被取消，不要停服务。

M1 的在途 review 作业没有列表接口；用日志和连接核对：

```
sudo journalctl -u review-gate.service --since '-30 min' --no-pager | grep -Ei 'job|review'
sudo ss -tnp | grep ':7878'
```

预期日志里最近没有仍在运行的 job；`:7878` 上允许客户端的空闲 keep-alive 连接，但不能有正在等待
review 的请求。拿不准就继续等，并在报告里写明。

注意：这里与 `stop` 之间仍有竞态（n5 第 1 条）：检查之后、服务停止之前，客户端仍可能提交一个
请求。本节的第 1 步“通知暂停”是主要防线；**权威检查是 stop 之后本节第 6 步的 runner 检查**。
若本节第 6 步发现竞态期间被接纳的作业，按本节第 7 步等它完成（上限 30 分钟），不要把它当成
泄漏或强杀。

4. 建立排空时间标记（供本节第 6 步找“新作业”）：

```
sudo touch /run/m2-drain-marker
```

5. 停止 gate：

```
sudo systemctl stop review-gate.service
sudo systemctl is-active review-gate.service
```

预期 `inactive`。`stop` 成功后不会再有新请求；gate 进程退出时不会等 detached 的作业线程，
所以必须继续查 runner。

6. 确认 runner 无活动作业（`active` 不算证据）：

```
pgrep -u auditor -af codex
sudo ss -xp | grep -F /run/audit-runner/runner.sock
sudo find /home/auditor/jobs -mindepth 1 -maxdepth 1 -type d -newer /run/m2-drain-marker -printf '%TY-%Tm-%Td %TH:%TM:%TS %p\n'
sudo find /home/auditor/jobs -mindepth 1 -maxdepth 1 -type d ! -exec test -e '{}/answer.md' ';' -print
```

预期：没有 codex 进程；runner.sock 上没有 `ESTAB`/`CLOSE-WAIT`（gate 已停，仍在作业的连接会留在
这两个状态之一）；没有比 marker 更新的作业目录；最后一条只列出历史失败/超时作业（无 `answer.md`），
其时间应早于 marker。runner 是常驻监听循环，**不要**以 `systemctl is-active` 判断在途。

7. 等待排空，**上限 30 分钟**（从本节第 5 步 stop 完成算起）。每 30 秒重复本节第 6 步；
   也可以直接跑这个无 sudo 的循环：

```
end=$((SECONDS+1800)); while pgrep -u auditor -f codex >/dev/null || ss -xp | grep -F /run/audit-runner/runner.sock | grep -Eq 'ESTAB|CLOSE-WAIT'; do [ $SECONDS -ge $end ] && { echo 'CAP REACHED: stop and report'; break; }; date; sleep 30; done
```

到 30 分钟仍未排空：**停止操作并报告**（附 `pgrep -u auditor -af codex`、`sudo ss -xp | grep runner.sock`、
最新 job 目录和 `journalctl -u audit-runner.service`），不要 `kill` 审计进程，也不要继续本节第 8 步。

8. 排空后停止 runner 的 socket 与 service（先 socket，防止新连接激活）：

```
sudo systemctl stop audit-runner.socket
sudo systemctl stop audit-runner.service
systemctl is-active audit-runner.socket audit-runner.service
```

预期两个 `inactive`。失败时：`sudo systemctl status audit-runner.socket audit-runner.service`，
不要用 `kill -9`；把状态记下来报告。

## 5. 原子替换 review-gate 与 audit-runner

两个二进制都在**同一文件系统内** rename，避免半写状态。目标：
`/usr/local/bin/review-gate`、`/usr/local/lib/audit-runner/audit-runner`。

1. 备份旧文件（名字含日期和旧 sha 前 12 位）：

```
sudo cp -p /usr/local/bin/review-gate /usr/local/bin/review-gate.bak-20261020-$(sha256sum /usr/local/bin/review-gate | cut -c1-12)
sudo cp -p /usr/local/lib/audit-runner/audit-runner /usr/local/lib/audit-runner/audit-runner.bak-20261020-$(sha256sum /usr/local/lib/audit-runner/audit-runner | cut -c1-12)
ls -l /usr/local/bin/review-gate.bak-20261020-* /usr/local/lib/audit-runner/audit-runner.bak-20261020-*
```

记录实际文件名，回退时要用。预期每个目录只有一个备份。

2. **安装源**再验一次 sha256（必须与第 1 节 manifest 完全一致）：

```
sha256sum /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-gr/release/review-gate
sha256sum /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-gr/release/audit-runner
```

预期分别为 `378af8c0b5c3…` 和 `c9a66da94427…`。不一致就停止，不要安装。

3. 写到同目录的隐藏 `.new` 文件：

```
sudo stat -c '%U %G %a %n' /usr/local/lib/audit-runner
sudo install -o root -g root -m 0755 /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-gr/release/review-gate /usr/local/bin/.review-gate.new
sudo install -o root -g root -m 0755 /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-gr/release/audit-runner /usr/local/lib/audit-runner/.audit-runner.new
```

预期 `/usr/local/lib/audit-runner` 为 `root root 755` 且已存在（旧二进制就在里面）；不存在或
owner/mode 不符就停止，不要用 `install -d` 改写一个已有目录。

4. 核对新文件的 owner、mode、sha256：

```
sudo stat -c '%U %G %a %s %n' /usr/local/bin/.review-gate.new /usr/local/lib/audit-runner/.audit-runner.new
sudo sha256sum /usr/local/bin/.review-gate.new /usr/local/lib/audit-runner/.audit-runner.new
```

预期 `root root 755`，sha256 与第 1 节一致。不符就删除 `.new` 并停止。

5. 同一文件系统内原子替换：

```
sudo mv -f /usr/local/bin/.review-gate.new /usr/local/bin/review-gate
sudo mv -f /usr/local/lib/audit-runner/.audit-runner.new /usr/local/lib/audit-runner/audit-runner
sudo stat -c '%U %G %a %s %n' /usr/local/bin/review-gate /usr/local/lib/audit-runner/audit-runner
sudo sha256sum /usr/local/bin/review-gate /usr/local/lib/audit-runner/audit-runner
```

## 6. 替换 agent-bridge（维护窗口；会中断所有 agent worker turn）

**在维护窗口内执行**。先完成第 4 节的排空（gate 已停、runner 无在途作业）。这一步只动
`/home/administrator/.local/bin/agent-bridge`（属主是 `administrator`，**不要 sudo**）和三个
`systemctl --user` 单元。

### 6.1 预检（任一项不符就停止并报告，不覆盖）

```
BIN=/home/administrator/.local/bin/agent-bridge
ls -l "$BIN"
sha256sum "$BIN"
"$BIN" --version
systemctl --user show agent-bridge.service -p ExecStart -p ActiveState -p MainPID
systemctl --user is-active agent-bridge.service agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service
```

记录实际输出。预期：sha256 为 `dbca255f1a9bfb0b7256f481515070ab25fda2c1cda8b23b8be5abbe6c5bb31b`，
版本 `agent-bridge 0.1.0`，服务均为 `active`。

**怎么判断线上二进制确实是 bridge 线 `4e34e25`**：从 `4e34e25` 做一次**单包** release 构建，
构建目录必须是干净的 detached worktree、target 目录必须是新的空目录；然后把它的 sha256 与
线上文件比较。**本次已经用下面的目录完成构建并比对；不要重建**，直接看后面的结论；命令块只是
可从零复现的记录。

```
git -C /home/administrator/projects/Chrome-MCP-m2-deploy worktree add --detach /home/administrator/projects/Chrome-MCP-live-4e34e25 4e34e25e77165af3689b13a1b0835568d643988a
test -z "$(ls -A /home/administrator/projects/Chrome-MCP-live-4e34e25-target 2>/dev/null)" || { echo 'target dir not empty'; exit 1; }
cd /home/administrator/projects/Chrome-MCP-live-4e34e25
source ~/.cargo/env
CARGO_TARGET_DIR=/home/administrator/projects/Chrome-MCP-live-4e34e25-target cargo build --release --locked -p agent-bridge
sha256sum /home/administrator/projects/Chrome-MCP-live-4e34e25-target/release/agent-bridge
git status --porcelain --untracked-files=all
git rev-parse HEAD HEAD^{tree}
```

本次已核对：该构建的 sha256 是
`dbca255f1a9bfb0b7256f481515070ab25fda2c1cda8b23b8be5abbe6c5bb31b`，与线上文件**完全一致**
（HEAD `4e34e25e77165af3689b13a1b0835568d643988a`，tree `4a8e785907a067fd988f1b4c6b6549ca117d65b0`，
worktree clean）。如果线上 sha256 不同，说明有人部署了更新的版本：**停止，报告用户，不要覆盖**
（项目 CLAUDE.md：不要用旧分支的二进制覆盖线上版本）。如果无法完成这个构建/比较，也**停止**，
不要凭版本字符串或日期判断。

### 6.2 排空 agent-bridge worker turn

确认没有正在执行的 `opencode run` / `codex exec`（它们是 `agent-bridge-cli@*.service` 的子进程）。
**不要 kill**；有就等它结束。

```
systemctl --user status agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service --no-pager
pgrep -af 'opencode run --format json|codex(\.exe)? exec'
```

预期：`status` 的进程树里只有 python worker（`bridge_agents.py worker ...`），没有
`opencode run` / `codex exec` 行；`pgrep` 无输出。如果正在执行的恰好是你自己（从 bridge worker
里跑本手册），先换到普通终端；否则等待，不要把在途 turn 杀掉。

同时确认第 4 节的 gate/runner 排空仍成立（gate `inactive`、无 audit 作业）。

### 6.3 observer 自检、备份与原子替换

安装前先对**即将安装的二进制**跑一次隔离的 observer 真实二进制自检（临时 HOME、独立端口、
不需要 sudo、不碰线上 bridge，见第 1 节）：

```
bash /home/administrator/projects/Chrome-MCP-m2-deploy/scripts/m2-e2e/bridge_observer_e2e.sh \
  /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-bridge/release/agent-bridge
```

预期最后一行 `ALL OK`，且 `binary sha256:` 等于 `d9ab3e50…`。若脚本的 sha256 不是
`c2e1887c8087bd93a7200dc8ae2d93d8d902820ab21ffcd60f1f74b1e88adb5d`，或自检失败，**停止**，
不要安装。

```
BIN=/home/administrator/.local/bin/agent-bridge
TS=$(date -u +%Y%m%dT%H%M%SZ)
cp -p "$BIN" "$BIN.bak-$TS"
sha256sum "$BIN.bak-$TS"
install -m 0755 /home/administrator/projects/Chrome-MCP-m2-build-bbc5aeb-target-bridge/release/agent-bridge "$BIN.new-$TS"
sha256sum "$BIN.new-$TS"
mv -f "$BIN.new-$TS" "$BIN"
sha256sum "$BIN"
```

预期：备份 sha256 等于 6.1 记录的旧值；新文件 sha256 等于第 1 节 manifest 的
`d9ab3e50895ed729b076d031719738e8157516084b3ae8e2aa2620366b41bca2`；
替换后 `$BIN` 的 sha256 与新文件一致。备份文件名要记录下来，回退时用。不符就停止（此时旧文件
仍未动；若 `install` 后 sha 不符，删除 `.new-*` 并停止）。

### 6.4 重启顺序与健康检查

按 `agent-bridge.service` → 两个 CLI worker 的顺序重启；observer 配置（第 3 节）在启动时读取。

```
systemctl --user restart agent-bridge.service
systemctl --user is-active agent-bridge.service
curl -fsS -o /tmp/agent-bridge-health.json -w '%{http_code}\n' http://127.0.0.1:7879/v1/health
python3 -m json.tool /tmp/agent-bridge-health.json | head -20
systemctl --user restart agent-bridge-cli@codex-collab.service
systemctl --user restart agent-bridge-cli@opencode.service
systemctl --user is-active agent-bridge.service agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service
journalctl --user -u agent-bridge.service -n 30 --no-pager | grep -i 'read-only observers'
```

预期：`/v1/health` 返回 200 且 JSON 的 `ok` 为 true；三个单元都 `active`；日志出现
`agent-bridge: read-only observers ["review-gate"]`。若没有这一行，检查
`~/.config/agent-bridge/observers.json` 的名字/哈希与第 3 节是否一致；observer 名字不能与 agent 重名。
**在确认这一行之前，不要进入第 7 节启动 review-gate。**

### 6.5 agent-bridge 回退

```
BIN=/home/administrator/.local/bin/agent-bridge
ls -l "$BIN".bak-*
sha256sum "$BIN".bak-*
read -r -p 'UTC timestamp of the backup to restore: ' TS
install -m 0755 "$BIN.bak-$TS" "$BIN.rollback-$TS"
sha256sum "$BIN.rollback-$TS"
mv -f "$BIN.rollback-$TS" "$BIN"
sha256sum "$BIN"
systemctl --user restart agent-bridge.service
systemctl --user restart agent-bridge-cli@codex-collab.service
systemctl --user restart agent-bridge-cli@opencode.service
systemctl --user is-active agent-bridge.service agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service
curl -fsS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7879/v1/health
```

先 `install` 到 `.rollback-$TS` 再 rename，**不消耗备份**：`.bak-$TS` 保留，之后仍可再次回退。
预期 `.rollback-$TS` 和替换后的 `$BIN` 的 sha256 都等于 6.1/6.3 备份记录的旧值 `dbca255f…`，
三个单元 `active`，健康检查 200。`observers.json` 可以保留（旧 bridge 不读它，留着无害）。

## 7. 启动顺序与健康检查

**顺序不能颠倒**：bridge 必须先完成 observer 注册且健康，review-gate（带 `--milestones`）才能启动；
否则 sources 检查读 bridge 会失败。第 6.4 节的日志已确认 observer；这里再快速核对一次。

```
systemctl --user is-active agent-bridge.service agent-bridge-cli@codex-collab.service agent-bridge-cli@opencode.service
journalctl --user -u agent-bridge.service -n 30 --no-pager | grep -i 'read-only observers'
curl -fsS http://127.0.0.1:7879/v1/health >/dev/null && echo 'bridge health ok'
sudo systemctl start audit-runner.socket
systemctl is-active audit-runner.socket
sudo systemctl daemon-reload
sudo systemctl start review-gate.service
systemctl is-active review-gate.service
curl -fsS http://127.0.0.1:7878/health
curl -fsS http://127.0.0.1:7878/pubkey
sudo journalctl -u review-gate.service -n 30 --no-pager
```

预期：三个 agent-bridge 单元 `active`，日志仍有 `read-only observers ["review-gate"]`；
socket `active`；gate `active`；`/health` 200；`/pubkey` 与 CI 中 pin 的
`ITU9Oc3AbiC/Q/LYPwld2b+OC2rxlqHbvCO7IQcGdXw=` 一致；日志有
`review-gate listening on 127.0.0.1:7878`。启动失败时先看 journal：
配置字段错误/令牌 owner 或 mode 错误都会在绑定端口前拒绝启动；修正后重试，不要跳过。

**重启策略（U-c=1）**：drop-in 设置 `Restart=on-failure`、`RestartSec=10`、
`RestartSteps=5`、`RestartMaxDelaySec=300`，并在 `[Unit]` 限制
`StartLimitIntervalSec=3600`、`StartLimitBurst=5`。gate 以 exit 3 退出（例如账本/磁盘写入失败）
会自动退避重启；但**1 小时内超过 5 次**后 unit 会停在 `failed`。此时不要反复 `start`：
先查清账本或磁盘故障是否已消除，再执行
`sudo systemctl reset-failed review-gate.service`，然后重新启动。永久故障不会被无限重启掩盖。

milestone 路由已启用的证据（认证后不是 404）：

```
sudo REVIEW_GATE_TOKEN_FILE=/etc/review-gate/client.token /usr/local/bin/review-gate milestone status --phase m2 > /tmp/m2-status.json
python3 -m json.tool /tmp/m2-status.json
```

预期 JSON 中 `.state.registered=false`，`reasons` 含
`phase not registered (or not confirmed by the user)`；这是“阶段不存在”的实际返回，不是 404。
CLI 在未释放时退出 1，不是查询失败。若路由未启用，CLI 会报 `review-gate 404`，不会产出该 JSON。
“已申请未 confirm”同样返回 `registered=false`（pending 文件不在签名日志里）。
精确的 HTTP 200 断言由 E1 在隔离实例完成。

## 8. 部署后的管理步骤（用户已决定；由 DSH 执行）

前提：第 7 节全部通过；E4 的 `register` 已执行并写入了 `/var/lib/reviewgate/state/milestones/pending/m2.json`
（没有 pending 时 `confirm` 的 dry run 会显示 `none on file`，此时**停止**，先执行 E4 register）。

**规则**：下列每条写命令都必须**先跑一次不带 `--yes` 的 dry run，把完整输出记录到日志；核对无误后
再加 `--yes` 执行一次**。`--yes` 是 `admin milestone` 子命令的选项，位置在操作子命令之前。
`admin milestone` 是离线操作：它以 `reviewgate` 用户读写 state，并读取 bridge observer 令牌；
不要用 root 直接跑（会写出 root 属主的文件）。link/ignore 的 reason 都引用用户决定 **U-g**
（2026-10-20，“U-g 按建议”：link 这 2 个、ignore 这 5 个）；`confirm` 没有 `--reason`，
它对应的是 **U-b**（phase m2 只有一个 condition，任务书 `plans/M2-design.md`，base `3cc5491`），
这一点只在下面的说明里写给日志读者，不写入签名日志。

先看 dry run 的实际结构（必须能读到 pending 申请）：

```
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json confirm m2
```

记录输出。确认 `application` 不是 `"none on file"` 后执行：

```
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes confirm m2
```

link-task（用户决定 U-g，2026-10-20，按建议 link 这 2 个）。每个任务先 dry run，再加 `--yes`：

```
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json link-task m2 --task dsh-review-m2n2 --reason 'user decision U-g (2026-10-20): link dsh-review-m2n2 to phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes link-task m2 --task dsh-review-m2n2 --reason 'user decision U-g (2026-10-20): link dsh-review-m2n2 to phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json link-task m2 --task dsh-review-m2s6b-20261019 --reason 'user decision U-g (2026-10-20): link dsh-review-m2s6b-20261019 to phase m2'
sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes link-task m2 --task dsh-review-m2s6b-20261019 --reason 'user decision U-g (2026-10-20): link dsh-review-m2s6b-20261019 to phase m2'
```

ignore-task（用户决定 U-g，2026-10-20）。每个任务先 dry run，再加 `--yes`：

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

做完后核对状态（只读）：

```
sudo REVIEW_GATE_TOKEN_FILE=/etc/review-gate/client.token /usr/local/bin/review-gate milestone status --phase m2 > /tmp/m2-status-after-admin.json
python3 -m json.tool /tmp/m2-status-after-admin.json
```

预期 `.state.registered=true`。如果 `reasons` 里出现本清单以外任务名的
`unmatched bridge task <id>`，**不要处理**：记录该任务 id 和完整输出，停止并要求用户决定。
本清单只覆盖上面 2 个 link 和 5 个 ignore；其他任务一律不自动 link/ignore。

## 9. 验收脚本

脚本在 `scripts/m2-e2e/`，均为 bash、`set -euo pipefail`，只读或只操作临时隔离目录；
需要 root 的地方只打印命令。默认 HEAD 已改为 `bbc5aeb74005d76ad8fde20b89a5e9337f5c7aec`。

- `bridge_observer_e2e.sh`：observer 真实二进制自检。临时 HOME/端口，无 sudo，不碰线上 bridge；
  断言 observer 只读任务元数据、其余路由 403/401，最后打印 `ALL OK`。安装 agent-bridge 前必跑，
  参数是待安装二进制路径。
- `e1_failclosed.sh`：用隔离 state/端口/令牌跑坏配置，断言非零退出且不监听端口；
  再启动一个隔离实例，断言“阶段不存在”和“已申请未 confirm”都是 200 + `registered=false` + 未放行。
  不连接真实 runner。可用 `REVIEW_GATE_BIN=…` 指向第 1 节的 fault 构建。
- `e2_anonymous.sh`：只打印命令。用 `systemd-run`/`sudo -u reviewgate` 在真实沙箱里验证匿名 fetch，
  留 `remote.git` 的 FETCH_HEAD/refs 证据，并读取 base 的任务书、证据目录和
  `refs/notes/review-gate` 的审核 note。`safe.directory` 只加精确仓库路径。
- `e4_preflight.sh`：只读，输出完整 base/head、任务书完整 sha256、提交数/merges、
  `verify-range` 每条原始输出和完整公钥及来源、`git diff | wc -c`。
- `e4_run.sh`：按 register →（用户/DSH `admin milestone confirm --yes`，见第 8 节）→ request →
  轮询 status → publish → status 驱动；每步保存原始 JSON；单独提取 job 的 sender（uid/pid/cgroup）。
  publish 会写公开 notes，脚本要求显式 `--publish` 才执行。REWORK 如实记录，不重复请求。
  E4 结论必须按第 0 节写成“按批准的标签判为独立，实际厂商相同”，不能写成“厂商独立”。

## 10. 回退（明确命令，不用“按相反顺序”代替）

回退**必须先走第 4 节的停机排空**（包括 30 分钟上限），不能只 reverse。
agent-bridge 的回退单独在第 6.5 节（同样要在排空后、用同样的重启顺序）。

1. 按第 4 节停掉客户端、确认 Pending/runner 排空、stop gate、stop socket/service。
2. 恢复 gate/runner 旧二进制（备份名用第 5 节记录的实际名字；下面是按本次旧 sha 的示例）：

```
ls -l /usr/local/bin/review-gate.bak-20261020-* /usr/local/lib/audit-runner/audit-runner.bak-20261020-*
sudo install -o root -g root -m 0755 /usr/local/bin/review-gate.bak-20261020-0b4f1e1effbd /usr/local/bin/.review-gate.rollback
sudo install -o root -g root -m 0755 /usr/local/lib/audit-runner/audit-runner.bak-20261020-69983378df3b /usr/local/lib/audit-runner/.audit-runner.rollback
sudo sha256sum /usr/local/bin/.review-gate.rollback /usr/local/lib/audit-runner/.audit-runner.rollback
sudo mv -f /usr/local/bin/.review-gate.rollback /usr/local/bin/review-gate
sudo mv -f /usr/local/lib/audit-runner/.audit-runner.rollback /usr/local/lib/audit-runner/audit-runner
```

若备份名不同，用 `ls` 的唯一结果替换，不要猜。sha256 应为旧值
`0b4f1e1effbd…` / `69983378df3b…`（以你实际记录为准）。

3. 移除 drop-in（旧 gate 不认识 `--milestones`，必须去掉）：

```
sudo rm -f /etc/systemd/system/review-gate.service.d/milestones.conf
sudo systemctl cat review-gate.service
```

4. 启动并核对：

```
sudo systemctl daemon-reload
sudo systemctl start audit-runner.socket
sudo systemctl start review-gate.service
systemctl is-active audit-runner.socket review-gate.service
curl -fsS http://127.0.0.1:7878/health
sudo journalctl -u review-gate.service -n 20 --no-pager
```

预期 gate 回到旧参数、`active`；旧 gate 的 milestone 路由返回 404（功能停用），这是预期行为，
不是自动降级。

**保留**：`/var/lib/reviewgate/state/`、`/var/lib/reviewgate/state/milestones/`（含 `remote.git`、
`log.jsonl`、检查点）和远端 notes。回退**不删除、不修改检查点、不回滚远端 notes、不删除观察者
注册**（observer 只读，留着无害；若用户要求撤销，单独决定）。`/etc/review-gate/milestones.json`
与 observer 令牌也保留，便于下次启动；旧 gate 不读它们。

## 11. 本手册明确不做的事

- **编写和自测本手册时**没有执行任何 `sudo`，没有改 `/etc`、`/usr/local`、`/var/lib/reviewgate`，
  也没有 push。执行者只运行本手册列出的 `sudo` 命令，一行一行执行，不运行清单以外的 `sudo`。
- 不在真实环境做故障注入；fault 构建只用于自测和字符串对照。
- 不用 `safe.directory=*`；遇到 git 所有权检查只加精确仓库路径。
- 不为得到 APPROVE 而重复请求；E4 的审计结论与链路验收分开记录。
- 不读取、打印或记录任何令牌内容，也不 `cat`/打印 `/etc/review-gate/env`；`/etc/review-gate/env`
  只允许按变量名过滤（见 `e2_anonymous.sh`）。
- E3（反向认证）由 Claude 在 Rust 测试里完成，不在本手册内。
