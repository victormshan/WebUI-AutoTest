# B1 阶段 1 证据：接口与 McpBackend 基线（2026-10-12）

环境：WSL2（Linux 6.18.40.1-microsoft-standard-WSL2），rustc 1.99.0，Node v22.22.1，chrome-devtools-mcp 1.10.1，Chrome 154（无头），fixtures/shop 跑在 127.0.0.1:8765。
基线提交：`cece30a`（B1 r2 任务书，没有代码改动）。

## 改了什么
- 新增 `crates/agent/src/backend.rs`：
  - `BrowserBackend` trait：`tools`、`call`、`navigate`、`snapshot`、`storage_state`、`apply_storage_state`、`diagnostics`、`close`；
  - `McpBackend` = `mcp_client::Browser`，前七个方法原样转发；
  - `Diagnostics` 增加 `console`、`network` 两个 `Coverage`（complete / partial / unsupported / failed / not_recorded）。
- D6 的诊断解析从 `lib.rs::collect_diagnostics` 搬到 `McpBackend::diagnostics`。条目过滤和规范化的代码不变（`strip_id`、`strip_arg_count`、`http_failed`）；新增覆盖状态的判断。
- `Agent`、`replay`、`reach` 改成接收 `&dyn BrowserBackend`。CLI 和 explore 仍然启动 `mcp_client::Browser`，传参时自动转换。
- 用到诊断结果的三个地方：
  - replay：任何一个通道不是 complete，就记失败 `diagnostics incomplete: <通道>: <原因>`；
  - run：打印同样的提示，判定规则不变；
  - 录制 flow：记警告。
- README 新增"运行环境限制（调试端口）"一节；任务书升到 r3。

## McpBackend 唯一的行为变化
以前诊断工具返回 `is_error`，或者输出格式不认识时，会被静默解析成"没有错误"。现在这两种情况都报为诊断不完整，replay 判非 PASS。正常输出的行为不变。

## 证据文件
| 文件 | 内容 | 结果 |
|---|---|---|
| `checks.txt` | fmt、clippy `-D warnings`、`cargo test --workspace`、agent 测试清单 | 全部通过；agent 测试 24 → 31（2 个随代码移入 backend，新增 6 个 backend 测试、1 个 replay 测试） |
| `e2e-mcp-before-cece30a.txt` / `e2e-mcp-after.txt` | 同一台机器上，基线提交和本次改动分别跑 `cargo test -p webtest --test e2e -- --ignored` | 前后都是 7/7 通过：通过、改名、金额 bug、新 console 错误、登录态复用、会话过期刷新、自动创建登录态 |
| `mcp-diagnostics-output.txt` | chrome-devtools-mcp 两个 list 工具的原始输出（完整、分页、参数错误） | 单元测试里的输出样本都取自这里，原样照抄 |
| `mutation.txt` | 4 个变异 | 每个变异都让测试变红；恢复后 31/31 |
| `port-audit.txt` | McpBackend 运行时的调试端口复核 | 运行时监听 `127.0.0.1:<随机端口>`，Windows 能连上（True）；关闭后端口、进程都消失，Windows 连不上（False） |

## 变异测试明细
| 变异 | 预期变红的测试 | 实际 | 恢复后 |
|---|---|---|---|
| M1 去掉 `is_error` 判断，并把不认识的输出当成 complete（即旧行为） | `a_failed_read_is_not_an_empty_list` | 先只去掉 `is_error` 判断时仍是 31/31：报错文本也是"不认识的输出"，照样记为 failed，两层保护里任意一层都能兜住。两层都去掉后 1 个失败 | 31/31 |
| M2 只把不认识的输出当成 complete | `a_failed_read_is_not_an_empty_list` | 1 个失败 | 31/31 |
| M3 去掉分页（`Showing a-b of N`）检查 | `a_paged_or_short_list_is_partial` | 1 个失败 | 31/31 |
| M4 replay 忽略覆盖缺口 | `unread_diagnostics_never_pass` | 1 个失败 | 31/31 |

## 没有执行的
- NativeBackend 的所有测试（阶段 2 起）；
- §5.2 的 5 轮对照、§5.3 的故障注入（阶段 4）；
- 端口复核里的"WSL 内连接"一项：探测时浏览器已经退出，结果无效（文件里已注明）。端口在 WSL 内的可达性已由 `ss` 显示监听在 127.0.0.1 证明。

## 阶段 1 收尾（2026-10-13，按 ChatGPT 的阶段 1 审核意见）
ChatGPT 的结论是 APPROVE WITH CONDITIONS，要求补齐一点：底层诊断调用返回 `Err` 时，覆盖状态怎么体现。

**改动**
- `BrowserBackend::diagnostics()` 改成不返回 `Result`。底层 `Err` 只把该通道标为 `failed`，另一个通道照常读取。
- **收尾时发现一个真实漏洞并已修复**：Chrome 死掉后，chrome-devtools-mcp 会静默启动新的 Chrome，只在下一次工具调用里提示一次。如果这次提示被别的调用（例如取快照）先拿走，之后读到的诊断来自新浏览器，看起来"完整、没有错误"，重启前的 `before-crash` 错误丢了，replay 会误判 PASS。修复：mcp-client 锁存 `restarted()`，`diagnostics()` 发现锁存就把两个通道都标为 `failed`。探测过程：先在测试里加临时打印，确认被 kill 的只有 npm/sh 包装进程和 Chrome，真正的 node 服务进程还活着并且重新拉起了 Chrome；`list_pages` 返回了重启提示，随后 `list_console_messages` 返回 `<no console messages found>`。
- 新增端到端测试 `crates/agent/tests/mcp_backend.rs::a_browser_crash_reads_as_failed_diagnostics`，用真实的 chrome-devtools-mcp：
  1. 打开一个会产生 console 错误的页面，诊断读到 `[error] before-crash`，状态 complete；
  2. 只 kill Chrome；
  3. 先取一次快照，把一次性的重启提示消耗掉；
  4. 再读诊断：两个通道都是 `failed`，并且没有把旧错误当成新浏览器里的。
- CI 模板 `ci/webtest.yml` 和 README 的端到端命令改为 `-p webtest -p agent`，这个测试也会在 CI 里跑。

**证据**
| 文件 | 内容 | 结果 |
|---|---|---|
| `checks-closeout.txt` | fmt、clippy `-D warnings`、`cargo test --workspace`、测试清单 | 全部通过；agent 单元测试 31 → 33（新增 `a_failed_call_marks_only_its_own_channel`、`a_restarted_browser_vouches_for_nothing`） |
| `raw/e2e-closeout.log` | `cargo test -p webtest -p agent -- --ignored` 原始输出（收尾后的代码） | 8/8：原 7 例 + 崩溃用例 |
| `raw/e2e-before-cece30a.log`、`raw/e2e-after-4723c77.log` | 阶段 1 前后两次 e2e 的原始输出（上面表格里的是过滤后的版本） | 各 7/7 |
| `mutation-closeout.txt` | 3 个新变异，每个都跑了单元测试和崩溃 e2e | 见下表 |

| 变异 | 预期变红的测试 | 实际 | 恢复后 |
|---|---|---|---|
| M5 底层 `Err` 当成完整空列表 | `a_failed_call_marks_only_its_own_channel` | 单元测试 1 个失败；崩溃 e2e 仍通过（这条路径不经过 `Err`，符合预期） | 全部通过 |
| M6 `diagnostics()` 不看重启锁存 | 崩溃 e2e | 崩溃 e2e 失败；单元测试仍通过：单元测试只覆盖 `lost_to_restart` 本身，不覆盖接线，**接线只有 e2e 能守住** | 全部通过 |
| M7 mcp-client 从不锁存重启 | 崩溃 e2e | 崩溃 e2e 失败；单元测试仍通过（原因同 M6） | 全部通过 |

- 崩溃测试结束后没有残留的 Chrome 进程（`ps` 查 `webtest-crash` 结果为 0）。
- 原有的无效 WSL 探测标记保持不变，见 `port-audit.txt`。
