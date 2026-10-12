# B1 阶段 2a 证据：NativeBackend 的 pipe 启动与清理（2026-10-13）

ChatGPT 准入阶段 2 时建议：先提交 pipe 启动与清理这一小批供审核，再推进 AX 快照和动作实现。2a 就是这一小批。
环境：WSL2，rustc 1.99.0，Chrome 154（Chrome for Testing，通过 `~/.local/bin/chrome-wsl` 启动），无头模式。

## 改了什么
- **browser-runtime**：新增断连锁存 `is_disconnected()`。
  - 读线程发现 pipe 关闭时，先置位标志，再让所有等待中的命令失败；
  - 之后的 `call` 在登记等待之后检查标志，立即返回 `BrowserDisconnected`；
  - 标志不会清零。runtime 从不重连，也从不重新启动 Chrome。
- **新增 `crates/agent/src/native.rs`**（`NativeBackend`，只能显式使用，CLI 还没有入口）：
  - 启动：每个后端一个独立的临时 profile（`webtest-native-*`），通过 pipe 启动 Chrome，attach 到页面 target（flatten 会话），启用 Page 和 Runtime；
  - `navigate`：
    - 导航失败（`errorText`）→ `is_error`；
    - 页面加载超时 → `Err`（结果未知）；
    - 等待加载期间每 200ms 检查一次断连，断连 → `Err(BrowserDisconnected)`；
  - 还没实现的部分都明说，不伪造成功：
    - `tools()` 返回空列表；
    - 其他工具返回 `is_error`，并写明"not supported"；
    - `snapshot`、`storage_state`、`apply_storage_state` 返回 `Err`，写明 "not implemented"；
    - `diagnostics()` 的两个通道是 `unsupported`，断连后改为 `failed`；
  - 清理：
    - 结构体字段的顺序保证先关掉 Chrome，再删除 profile；
    - `shutdown()` 依次尝试 graceful → SIGTERM → SIGKILL 关闭整个进程组，然后删除 profile；
    - drop、任务被取消、启动失败都走同样的清理路径（browser-runtime 的 Drop 会 kill 整个进程组）。
- CI 模板的 e2e job 用 `WEBTEST_CHROME=/usr/bin/google-chrome`。**这个模板没有启用，也没有在 GitHub 上实际跑过。**

## 测试（`crates/agent/tests/native_backend.rs`，10 个，使用真实 Chrome）
| 测试 | 证明 |
|---|---|
| `runs_over_a_pipe_with_no_port_and_cleans_up_on_close` | 进程组里所有进程的命令行都没有 `remote-debugging-port`；主进程带 `--remote-debugging-pipe`；进程组没有监听任何 TCP 端口（通过 /proc 的 socket inode 核对）；没有 `DevToolsActivePort` 文件；关闭方式为 graceful；关闭后进程组和 profile 都不在了 |
| `dropping_the_backend_leaves_nothing_behind` | 不调用 close 直接 drop：进程组和 profile 都不在了 |
| `a_failed_launch_leaves_nothing_behind` | 三种启动失败（程序立即退出 / 文件不存在 / 带了调试端口参数）都返回错误，并且没有留下 profile 或进程 |
| `a_crash_is_a_disconnect_and_nothing_restarts` | SIGKILL 浏览器进程后：断连在 2 秒内报出；`navigate` 立即返回 `BrowserDisconnected`；诊断两个通道都是 `failed`；等 2 秒后没有任何新的 Chrome 使用这个 profile，即**不会静默重建浏览器** |
| `aborting_the_owning_task_leaves_nothing_behind` | 持有后端的 tokio 任务被 abort 之后，进程组和 profile 都被清理 |
| `cancelling_a_launch_at_any_point_leaves_nothing_behind` | 在启动的 0/5/20/50/100/200/400 ms 处取消启动（实测都在启动途中被取消），以及 800 ms（此时已经启动完成，然后 drop）：每次都没有残留 |
| `concurrent_backends_are_isolated` | 同时启动 3 个后端：进程组和 profile 互不相同；关掉一个、kill 另一个，第三个照常 `navigate`，它的进程组和 profile 都还在 |
| `unimplemented_parts_say_so` | 没实现的部分都明确报错或报 `unsupported`；无法连接的地址 → `is_error` |
| `a_page_that_never_loads_times_out` | 两种挂起方式：服务端完全不回应时，`Page.navigate` 本身超时（Timeout，effect unknown）；只发了头和半个 body 时，等待加载超时（"outcome unknown"）。两种都是 `Err`，不是成功 |
| `a_crash_during_a_load_is_a_disconnect_right_away` | 同样两种挂起方式下，1.5 秒时 kill Chrome：约 1.6 秒就报 `BrowserDisconnected`，不会等满 30 秒的加载超时 |

原始输出：
- `raw/agent-ignored.log`：agent 的全部 ignored 测试，包括阶段 1 的 MCP 崩溃 e2e，11/11 通过；
- `raw/browser-runtime-ignored.log`：改了 browser-runtime 之后，A1 的 5 个真实 Chrome 测试，5/5 通过。

`checks.txt`：fmt、clippy `-D warnings`、`cargo test --workspace` 都通过。测试结束后没有残留的 `webtest-native-*` Chrome 进程。

## 变异测试（`mutation.txt`，每个变异都跑完整的 native 测试集）
| 变异 | 实际 | 恢复后 |
|---|---|---|
| N1 等待加载时不检查断连 | `a_crash_during_a_load_is_a_disconnect_right_away` 失败（等满了超时） | 10/10 |
| N2 runtime 的 `call` 不检查断连锁存 | **没有测试变红**。原因：Chrome 死后写 pipe 本身就会失败，或者读线程会清掉等待中的命令；锁存只防一个很窄的竞态窗口（读线程清掉等待之后、写入之前才登记的命令），这个窗口没法稳定复现。如实记录，没有为了让它变红去造测试 | 10/10 |
| N3 profile 先于 Chrome 被删除（字段顺序） | `dropping_the_backend_leaves_nothing_behind` 失败（profile 目录残留；具体是哪个文件写回的没有追查） | 10/10 |
| N4 断连后的诊断仍报 `unsupported` | `a_crash_is_a_disconnect_and_nothing_restarts` 失败 | 10/10 |
| N5 runtime 的 Drop 不 kill 进程组 | drop、abort、取消启动 3 个测试失败 | 10/10 |
| N6 允许调试端口参数 | `a_failed_launch_leaves_nothing_behind` 失败 | 10/10 |

## 没有执行或不在 2a 范围的
- AX 快照 → C1 文本、uid 代次与 `StaleUid`、click、fill、`fixtures/contract/` 契约页面：阶段 2b；
- CLI 的 `--backend native`：阶段 2b，在快照可用之后再加，否则选了也跑不了 flow；
- 端口复核（§4 第 4 条）针对的是 McpBackend，阶段 1 已做过；NativeBackend 的"无端口"由上面第一个测试在每次运行时断言；
- CI 模板没有在 GitHub 上实际跑过。
