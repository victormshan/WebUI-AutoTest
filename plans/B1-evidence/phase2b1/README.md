# B1 阶段 2b-1 证据：AX → C1 快照、uid 映射与 StaleUid（2026-10-13）

ChatGPT 准入 2b 时建议：先提交 AX → C1、uid 映射和 StaleUid，审核通过后再做 click/fill 和 CLI。本批同时落实 ChatGPT 对 2a 提出的三项检查。
环境：WSL2，Chrome 154（Chrome for Testing），chrome-devtools-mcp 1.10.1。

## 当前状态（第三轮返工之后，以本节为准）
下面第一到第七节是逐轮的历史记录，其中的数字和表述只描述当时那一轮。当前代码以本节为准：
- **清理**：
  - 结束 Chrome 进程组只由 runtime 负责（Browser 的 Drop，包括启动失败或取消时半成品 Browser 的 Drop）；
  - `ProfileGuard` **从不发信号**，只用三态探测 `Liveness`（只有 `ESRCH` 才算 Gone）来决定 profile：确认 Gone 才删除，Alive 或 Unknown 都保留并报告；
  - `shutdown` 会传播删除失败，错误里带路径；
  - `release()`、`kill_group`、`launching` 都已删除。
- **启动取消的顺序**（Codex 第三轮的放行条件）：
  - 新增测试钩子 `LaunchPause`：runtime 的 `pause_before_handshake` 和 NativeBackend 的 `pause_before_setup`；
  - 两个**确定性**测试让启动停在这两处，然后 abort，并等待取消完成（`task.await`，取消时的析构在此期间执行）；**取消完成后立即检查**（不再轮询）进程组已经没有进程、profile 已删除；
  - guard 只在确认 Gone 时才删除，所以"profile 已删除"同时证明了"runtime 先结束了进程组，guard 才去探测"。
  - 从 spawn 到 Browser 结构体构造完成，**所示代码路径里**没有 await，也没有返回错误的分支（代码见消息附带的节选），所以不存在"已经 spawn、但还没有 Browser 负责清理"的窗口。`spawn_reader`（`std::thread::spawn`）等步骤的 panic 路径没有核实；
  - Browser 的 Drop：先 SIGKILL 整个组，再 `kill` 加 `wait` 主进程（`wait` 没有超时），最后最多轮询 3 秒等进程组消失。3 秒只约束最后这次轮询，**不是**整个析构的上限；
  - 第二个钩子位于 setup **开始之前**。setup 内部被取消时的清理顺序由代码结构支持（Browser 是同一个局部变量），没有专门测试；
- **导航**：只有本次导航**已经提交**之后，同一 frame 再出现另一个 loader 才算"被替换"（`NavWatch`）。
- **uid**：
  - 结束时会再检查代次（线性化点在结束时），有确定性并发测试；
  - 解析过程中的错误按类型分类：code -32000 并且是 4 条"已不存在"消息之一才算 Stale，其余保留原义；4 条消息都有单元测试，其中 `Could not find object with given id` 在本机实测里没有出现，是为其他 Chrome 版本保留的；
  - 不是所有 `DOM.resolveNode` 失败都算 Stale。
- **数字**：一律按 JS Number（f64）输出；21 个边界值与浏览器 `String(Number(x))` 对照一致。这只覆盖这些样例，**不声称**所有 f64 都已证明一致。
- **iframe**：只有确认是跨进程的才留空；其他读取失败让整个快照失败。
- **N2**：uid 代次的并发已经有确定性测试；runtime 断连锁存的竞态（2a 的 N2）仍然没有确定性测试，延期。
- **Codex 第四轮（2026-10-13）：APPROVE**，放行条件解除（bridge 任务 `b1-2b1-review-r4`，固定审查线程 `01a13521-…`）。按用户决定，Codex 的批准等同于 ChatGPT 的批准；批准范围仅限 2b-1，不包括 click/fill、延期的 runtime 竞态测试和 CI。
- **测试总数**（本轮原始输出见 `raw/`）：agent 单元测试 52、runtime 单元测试 6；ignored 测试 27 个（契约 8、native 生命周期 13、MCP 崩溃 1、A1 5）。

## 一、2a 遗留的三项检查
| ChatGPT 的要求 | 落实 |
|---|---|
| 终止之后确认进程已退出，再删除 profile；终止失败不能报告清理成功 | browser-runtime 的 `close()`：SIGKILL 之后进程组仍然存在时返回 `Error::Shutdown`（以前是不管结果都报 `"sigkill"`）。NativeBackend 的 `shutdown`、`Drop` 和启动失败路径都改走 `release()`：先结束 Chrome（runtime 的 Drop 会 kill 整个进程组并等待），再查进程组；**还有存活的进程就保留 profile**，`shutdown` 返回错误并写明 profile 的位置，Drop 记 error 日志。"进程组"包括辅助进程：浏览器主进程已经退出、子进程还在，也算没清理完（`group_alive` = `killpg(pgid, 0)`）。主进程被 kill、辅助进程残留的情况，由 A1 的 `a_crashed_chrome_is_a_disconnect_and_close_still_cleans_up` 覆盖，本批重跑通过。**限制**：进程在 SIGKILL 之后仍然存活（例如卡在 D 状态）的分支没法在测试里制造，这条只靠代码审查 |
| 加载事件不能误归属 | `navigate` 改为等待 `Page.lifecycleEvent`，要求 `frameId` 和 `loaderId` 与本次 `Page.navigate` 的返回值一致，并且 `name == "load"`（启动时开启 `Page.setLifecycleEventsEnabled`）。同一 frame 上出现另一个 loaderId 的 `Page.frameNavigated`，就报"被另一次导航替换"（`Err`）。测试见下文第三节 |
| 诊断状态只读取一次 | `diagnostics()` 先读一次 `is_disconnected()`，再用这个结果构造两个通道 |

## 二、改了什么（2b-1 主体）
- **`crates/agent/src/native/ax.rs`**（纯函数，有单元测试）：逐行移植 chrome-devtools-mcp 1.10.1 的快照逻辑：
  - Puppeteer `Accessibility.snapshot({interestingOnly: true})` 的节点筛选规则：`isInteresting`、`isLeafNode`、`isControl`、`isLandmark`，以及 `serialize` 输出的属性；
  - chrome-devtools-mcp `SnapshotFormatter` 的输出格式：属性按字母排序；布尔属性前面加能力词（disableable、expandable、focusable、selectable）；三态属性（checked、pressed）；数字属性按 JS 的写法输出；option 用自己的文字作为 value；role 为 `none` 时打印成 `ignored`。
- **`NativeBackend::snapshot()`**：
  - 读取主 frame 的 `getFullAXTree`；用 `DOM.describeNode` 找到同进程 iframe 的 frameId，再取它的 AX 树，最多嵌套 8 层。跨进程 iframe 留空，是 B1 已知的限制；
  - 取快照前后各读一次主 frame 的 loaderId，不一致就报错（`Err`），避免把两个文档的内容混在一起；
  - AX 树为空 → `Err`。
- **uid**：
  - 格式为 `<代次>_<序号>`，按前序遍历编号；每取一次快照代次加一，旧代的 uid 一律失效。
  - 每个节点都有 uid，因为 C1 每一行都要以 `uid=` 开头。
  - `resolve_uid` 依次检查：
    1. 代次不对 → `Stale`；
    2. 本代没有这个 uid → `Unknown`；
    3. 节点没有对应的 DOM 节点 → `NotActionable`；
    4. 主 frame 的 loaderId 变了（导航或刷新）→ `Stale`；
    5. `DOM.resolveNode` 失败 → `Stale`；
    6. `isConnected` 为假（元素已从页面移除）→ `Stale`。
  - **从不改去找"相似"的元素**。
- `call("take_snapshot")` 现在返回快照。为了测试，NativeBackend 增加了两个 Rust 方法（不是 LLM 工具）：`cdp()`（在页面会话上发原始 CDP 命令）和 `evaluate()`。
- **新契约页面 `fixtures/contract/`**：`index.html`、`frame.html`（同源 iframe）、`second.html`（也有一个 "Add to cart" 按钮）。覆盖：
  - landmark、重名元素、无名称元素、`role=button` 的 div；
  - disabled、checked、mixed、expanded、focused、required、multiline；
  - select/option、radio、range、img；
  - 四种隐藏方式：`display:none`、`aria-hidden`、`visibility:hidden`、`hidden` 属性；
  - 嵌套列表、表格、表单、被遮挡的按钮、带内部元素的按钮；
  - 同源 iframe、live region；
  - 动态插入和删除的钩子；
  - 已有值的输入框（2b-2 验证 fill 会替换原值时要用）。
- **任务书 §3.2 两处修正**：
  - "没有 DOM 节点的不分配 uid"做不到，改为"有 uid，但解析时报 `NotActionable`"；
  - 记录 uid 语义比 MCP 更严格：MCP 在同一个文档里会跨快照复用 uid，NativeBackend 每次快照都是新的一代。

## 三、测试结果（`raw/ignored-all.log`：agent 和 browser-runtime 的全部 ignored 测试 20/20 通过）
**契约**（`crates/agent/tests/snapshot_contract.rs`，4 个）：
- `both_backends_snapshot_the_contract_page_alike`：两个后端在 index、second、frame 三个页面，以及动态插入两个按钮之后、删除之后，**去掉 uid 后快照文本逐行相同**（首轮 74 / 3 / 5 / 76 / 74 行；返工后 index 新增三个 range，现为 77 / 3 / 5 / 79 / 77 行）。agent 的快照函数结果也完全一致：
  - 每个节点的 `locator_for`，以及 locator 能否经 `resolve` 回到同一个节点；
  - `landmarks`、`fingerprint`、`label`；
  - 8 个 `has_element`（包括四种隐藏方式的按钮）和 4 个 `contains_text`（包括 iframe 里的文字）。
  - 实测四种隐藏方式在**两个后端里都不出现**。
- `every_native_uid_resolves_to_its_own_element`：首轮 74 个、返工后 77 个 uid 全部能解析；对每个 DOM 节点用 `Accessibility.getPartialAXTree` 反查，名称和角色都与快照那一行一致；两个同名的 "Add to cart" 解析到不同的元素。
- `native_uids_go_stale_and_never_move`：下面几种情况旧 uid 都报 `Stale`：
  1. 取了新快照；
  2. 导航到另一个也有 "Add to cart" 的页面；
  3. 刷新同一个 URL；
  4. 元素被删除；
  5. 元素被删除后，**又插入一个完全相同的元素**。

  另外，编造的 uid 报 `Unknown`，格式错误的 uid 也报 `UidError`。
- `a_navigation_is_only_completed_by_its_own_load`：第一次导航到一个永远加载不完的页面，1.5 秒后第二次导航到 second.html。第二次成功；第一次返回"replaced by another navigation"，**没有报成功**。

**回归**：2a 的 10 个生命周期测试、阶段 1 的 MCP 崩溃 e2e、A1 的 5 个测试全部通过；测试结束后没有残留的 Chrome 进程。2a 里"snapshot 未实现"那条断言已过时，改成断言 `storage_state` 未实现。

`checks.txt`：fmt、clippy `-D warnings`、`cargo test --workspace` 都通过（agent 单元测试 33 → 38，新增 5 个 AX 测试）。`mcp-contract-snapshot.txt` 是 MCP 对契约页面的原始快照，作为参照。

## 四、变异测试（`mutation.txt`；每个变异都跑了 agent 单元测试和契约测试）
| 变异 | 结果 | 说明 |
|---|---|---|
| S1 `resolve_uid` 不检查代次 | `native_uids_go_stale_and_never_move` 失败 | |
| S2 `resolve_uid` 不检查导航（loaderId） | 首轮：**没有测试失败**（导航或刷新之后，旧文档的节点在 `resolveNode` 或 `isConnected` 这两步就被判为失效）。审核打回后补了单元测试，重跑 S2'：`a_new_document_makes_every_uid_stale` 失败（见第五节） | 纵深防御，现在有单元测试守住 |
| S3 `resolve_uid` 不检查 `isConnected` | stale 测试失败（删除那一步） | |
| S4 加载归属 | 见下面三行 | 第一次跑 S4 时匹配模式没有命中（rustfmt 改了换行），实际只改了"被替换"检测那一半，所以重跑了一次 |
| S4a 只去掉 loaderId 匹配 | 测试通过 | 另一层（"被替换"检测）在另一次导航的 load 到来之前就报错了 |
| S4b 两层都去掉（=旧实现：只按 frame 或 session 匹配任意 load） | **被替换的导航报了成功**，测试失败 | 证明 ChatGPT 指出的误归属问题确实存在，修复有效 |
| S4c 只去掉"被替换"检测 | 第一次导航等满 30 秒超时（`Err`，没有报成功）；测试因为报错信息不同而失败 | 两层里任何一层单独存在，都能防止误判成功 |
| S5 AX 不过滤 hidden | 单元测试失败；**契约测试通过** | Chrome 的 AX 树里本来就不带这四种隐藏元素（或者标成 ignored），所以这条过滤规则只有单元测试能守住 |
| S6 不应用"控件内部的文字不重复"规则 | 单元测试和契约测试都失败 | |
| S7 取快照时不增加代次 | stale 测试失败 | |
| S8 不展开 iframe | 契约测试失败 | |

## 五、审核打回后的修改（review-gate 第 1 次：REJECTED，web-gemini，强度 3）
第 19 段判为 REJECTED，但列出的两条都标的是"非阻断"；第 10 段 APPROVED，也附了一条"非阻断"。三条都处理了：
1. **第 10 段：`DOM.resolveNode` 没有返回 objectId 时报的是通用错误** → 改为 `UidError::Stale`（"its element could not be resolved"）。没有页面能稳定造出这种返回，所以这一条没有测试，只靠代码审查（S9，见 `mutation-after-review.txt`）。
2. **第 19 段：S2 没有被任何测试抓到，建议补"旧 loader 隔离"的测试** → 把不需要浏览器的检查（代次、是否存在、有没有 DOM 节点、主 frame 和 loaderId 是否未变）提取成纯函数 `Uids::lookup`，并补了 5 个单元测试：
   - 本代 uid 映射到自己的节点；
   - 旧代次（即使序号存在）→ `Stale`；
   - 不存在或格式错误 → `Unknown`；
   - 没有 DOM 节点 → `NotActionable`（**这一分支以前没有任何测试，现在有了**）；
   - 主 frame 换了文档或换了 frame → `Stale`。

   重跑 S2'：`a_new_document_makes_every_uid_stale` 失败，恢复后 43/43 通过。
3. **第 19 段：S4 第一次匹配失败、重跑** → 这只是测试过程的记录，代码层面的防护由 S4b 证明（旧实现会报成功，新实现不会）。没有改代码。

修改之后重跑：agent 单元测试 38 → 43；agent 和 browser-runtime 的全部 ignored 测试 20/20 通过（`raw/ignored-all.log` 已更新）；没有残留进程。

## 六、Codex 审查（2026-10-13，REWORK）后的返工
Codex 现在接替网页版 ChatGPT 担任阶段审查（用户 10-13 决定，权限等同 ChatGPT）。它审查 2b-1 的结论是 **REWORK**，提了 6 条阻断意见（原文见 bridge 任务 `b1-2b1-review`，以及 `E:\rust-agent-browser\reviews\b1-2b1-review-1\`）。逐条处理如下：

| # | Codex 阻断意见 | 修改 | 证据 |
|---|---|---|---|
| 1 | 启动失败、启动途中取消时，没有做"进程组还在就保留 profile"的检查 | 新增 `ProfileGuard`，从创建 profile 起就由它持有。runtime 在 spawn Chrome 的那一刻，把 pgid 写进 `LaunchConfig::spawned_pgid`。启动没完成之前（出错或 future 被丢弃），guard 自己 kill 整个进程组并等待，**确认没有进程了才删除**，否则保留 profile 并记录 error。启动完成后，kill 交给 Browser 自己的 Drop，guard 只检查不发信号：主人已经处理完的进程组 id，不会被再次 kill（避免 pid 被复用时误杀别的进程）。`release()` 已删除 | 单元测试 `a_profile_is_removed_only_when_chrome_is_gone`（决策函数 `dispose_profile`，用参数控制"进程组是否还在"）；生命周期测试 `a_failed_launch_leaves_nothing_behind`、`cancelling_a_launch_at_any_point_leaves_nothing_behind` 重跑通过 |
| 2 | `shutdown()` 不再报告 profile 删除失败 | `dispose` 返回 `DisposeError::{Kept, RemoveFailed}`，`shutdown` 会传播这个错误，错误信息里写明 profile 路径；Drop 路径记 error 日志 | 单元测试 `a_profile_that_cannot_be_removed_is_an_error_naming_it`；真实 Chrome 测试 `a_profile_that_cannot_be_removed_fails_the_shutdown`：把父目录设为只读，`shutdown` 返回带路径的错误，同时确认 Chrome 已经退出 |
| 3 | "被替换"检测会把更早一次导航排队的事件误认成替换 | 新增纯状态机 `native/nav.rs::NavWatch`。只有**本次导航自己已经提交**（看到自己 loaderId 的 `frameNavigated`）之后，同一 frame 再出现另一个 loader，才算"被替换"；在那之前出现的其他 loader 都属于更早的导航，忽略。本次导航如果一直没有提交，就等超时（结果未知），**不会报成功** | 4 个按事件顺序构造的单元测试：旧 loader 事件先到、本次随后正常加载 → 成功；本次提交后被新 loader 替换 → 失败；别的导航的 load、子 frame 的事件都不算数 |
| 4 | iframe 读取失败被统一吞掉 | 只有**确认是跨进程 iframe**（`Target.getTargets` 里 `type == "iframe"` 的 targetId）才留空。`DOM.describeNode` 失败、同进程 iframe 读 AX 树失败，都让整个快照返回错误；嵌套超过 8 层也报错，不再静默截断。iframe 元素没有内容文档时仍然跳过，与 Puppeteer 一致 | `a_failed_iframe_read_fails_the_snapshot`：打开测试钩子 `fail_iframe_snapshots` 后，向 Chrome 请求一个不存在的 frameId，让**真实的 CDP 调用**失败（不是在调用之前直接报错），快照返回错误；关掉钩子后恢复正常；`a_cross_process_iframe_is_left_empty_not_failed`：127.0.0.1 页面嵌入 localhost 的 iframe（跨站，Chrome 放进单独进程），**MCP 能显示 frame 里的按钮，native 留空，但快照成功**，作为已知限制的实证 |
| 5 | `js_number` 没有完整实现 JavaScript 的数字格式 | 按 ECMAScript Number::toString 实现 `js_f64`：最短往返数字；指数在 -7 < e < 21 范围内写成普通小数，否则写成 `1e+21`、`1.5e-7` 这种形式；`-0` 输出 "0" | 单元测试覆盖 -0、1e-7、1e-6、1e20、1e21、2.5e21、0.1+0.2、负数等；**契约页面新增三个 range（0.000001、0.0000001、-0.5）**：MCP 实际输出 `value="9.999999974752427e-7"`、`value="1.0000000116860974e-7"`（Chrome 内部用 float 存这个值），native 逐字相同。用 Rust 默认格式会输出 `0.0000009999999974752427`，对不上 |
| 6 | 解析之后到下一次调用之间，元素失效会报通用错误 | 新增 `uid_call_error`：`DOM.resolveNode`、`Runtime.callFunctionOn` 返回协议错误（对象或上下文失效）→ `Stale`；断连 → 保持断连；**超时 → 保持超时，不说成 uid 失效**（以前 resolveNode 的任何错误都被当成 Stale） | 单元测试 `errors_after_resolving_keep_their_meaning` |

非阻断意见：
- **断言比证据写得弱**：
  - `assert_same` 现在逐项要求 `resolves_back=true`（不只是两边结果相同）；
  - 显式断言四种隐藏方式在**两个后端里都不出现**；
  - 契约页面给两个同名按钮和无名称按钮加了 `data-testid`，用 `DOM.describeNode` 核对 uid 指向的正是这个元素（不只是 backend id 不同）。
- **并发语义**：`resolve_uid` 的线性化点定在**结束时**：解析完成时 uid 必须仍然属于最新快照，解析期间有人取了新快照就报 `Stale`。没有确定性的竞态测试（见变异 R7）。
- **证据**：这次的审查材料附上了原始测试输出（过滤掉编译信息之后），不再只给汇总数字。

返工的变异测试（`mutation-rework.txt`）：
| 变异 | 结果 |
|---|---|
| R1 `dispose_profile` 不看 Chrome 是否已退出 | 单元测试失败 |
| R2 `shutdown` 忽略 dispose 的结果 | 真实 Chrome 测试 `a_profile_that_cannot_be_removed_fails_the_shutdown` 失败 |
| R3 `NavWatch` 不记录"本次已提交"（任何其他 loader 都算替换） | 2 个单元测试失败；真实浏览器的导航测试仍然通过（那里的替换确实发生在本次提交之后，区分不出来），由单元测试守住 |
| R4 iframe 读取失败被吞掉 | 第一次匹配模式没有命中（rustfmt 改了换行）。而且发现原来的钩子是在调用之前直接报错，测不到真实的 `?`，于是改成向 Chrome 请求不存在的 frame。重跑后 `a_failed_iframe_read_fails_the_snapshot` 失败 |
| R5 数字按 Rust 默认格式输出 | 单元测试失败；契约测试失败（tiny range） |
| R6 解析后的所有错误都当成 Stale（超时也算） | 单元测试失败 |
| R7 `resolve_uid` 结束时不再检查代次 | **没有测试失败**：快照与解析并发的时序没有确定性测试，只靠代码审查 |
| R8 启动完成前 guard 不 kill 进程组 | 没跑：现有的失败、取消测试都是 runtime 自己的 Drop 先 kill 掉进程组，区分不出 guard 有没有 kill；guard 自己 kill 只是兜底 |

重跑结果：
- agent 单元测试 43 → 51；
- agent 和 browser-runtime 的全部 ignored 测试 23/23 通过（`raw/ignored-all.log`）：契约 6、native 生命周期 11、MCP 崩溃 1、A1 5；
- 契约测试的原始输出在 `raw/contract.log`（index 页面现在有 77 个节点，因为新增了三个 range）；
- 没有残留进程。

## 七、Codex 第二轮审查（REWORK）后的返工
第二轮确认了上轮对 shutdown 删除错误、导航归属、iframe 错误传播的修复，剩 4 条阻断意见：

| # | Codex 阻断意见 | 修改 | 证据 |
|---|---|---|---|
| 1 | guard 在 runtime 已清理之后，仍对旧 pgid 发信号 | **guard 完全不发信号**：结束进程组只由 runtime 负责（Browser 的 Drop，包括启动失败或取消时半成品 Browser 的 Drop）。guard 只通过一个**可注入的探测函数**查询进程组状态，确认 `Gone` 才删除 profile。`launching` 标志和 `kill_group` 都已删除 | 单元测试 `the_guard_removes_the_profile_only_when_chrome_is_confirmed_gone`：用假探测分别返回 Alive、Unknown、Gone，以及未 spawn 的情况，验证 drop 和 dispose 两条路径 |
| 2 | `group_alive` 把"无法确认"当成"已不存在" | runtime 新增三态 `Liveness { Alive, Gone, Unknown(errno) }`，**只有 `ESRCH` 才算 Gone**；`group_alive` = 不是 Gone（无法确认也按"还活着"处理）。runtime 的 close 因此在无法确认时报 `Error::Shutdown`，guard 也保留 profile 并写明 errno | runtime 单元测试：用 0、ESRCH、EPERM、EINVAL 测纯函数 `liveness_from`；另外对 pid 1 的进程组断言它**不是 Gone**（本机不是 root，通常得到 EPERM，但测试没有断言具体 errno） |
| 3 | 整数绕过了 JS Number 语义 | `js_number` 一律先转成 f64，和 JSON.parse 一样四舍五入，再按 ECMAScript 规则输出 | 单元测试：9007199254740993 → "9007199254740992"，u64::MAX → "18446744073709552000"；**真实浏览器对照** `numbers_match_the_browsers_own_formatting`：21 个边界数字与 Chrome 自己的 `String(Number(x))` 逐个一致 |
| 4 | 所有协议错误都当成 Stale | 只有 code -32000 **并且**消息是表示"节点、对象或上下文已不存在"的那几条才算 Stale（消息原文取自对 Chrome 154 的实测）：`No node with given id found`、`Invalid remote object id`、`Cannot find context with specified id`、`Could not find object with given id`。参数错误（-32602）、方法不存在（-32601）、其他 -32000 错误都保留为 Protocol；超时保留为 Timeout；断连保留为断连 | 单元测试用实测的原文逐条覆盖 |

非阻断意见：
- **resolve 回到原节点**：`semantics()` 现在要求 locator 和 describe 都成功，**并且 `resolve` 得到的就是原 uid**（以前只比较描述相同）。三个页面和动态变化之后，77 个节点全部满足。
- **并发**：新增测试钩子 `hold_next_resolve`、`resolve_held`、`release_resolve`，让 resolve 停在最后一步检查之前。`a_snapshot_during_a_resolution_makes_the_uid_stale` 确定性地验证：停住期间取一次新快照 → `Stale`（"newer snapshot"）；停住期间不取快照 → 正常返回。线性化保证仅限"结束时代次仍是最新"，不声称解析期间 DOM 和文档状态一直不变。
- **第二轮返工的变异测试**（`mutation-rework2.txt`），5 个全部被抓到：
  | 变异 | 结果 |
  |---|---|
  | R9 killpg 的任何失败都算 Gone（旧实现） | runtime 单元测试失败 |
  | R10 恢复整数直接输出的捷径 | 单元测试和浏览器对照测试都失败 |
  | R11 所有协议错误都算 Stale | 单元测试失败 |
  | R7' 去掉结束时的代次检查 | **现在被抓到了**：确定性并发测试失败（上一轮的 R7 没有测试能抓到） |
  | R12 guard 不看探测结果 | 2 个单元测试失败 |
- **过时说法**：节点数、"跨进程 iframe 没有测试"两处已更正。
- **版本与环境**（本轮实际使用）：Google Chrome for Testing 154.0.8037.92 ；chrome-devtools-mcp 1.10.1；rustc 1.99.0 (b940084d7 2026-09-28)。
- **版本绑定**：审查材料里的 diff 与测试输出取自同一工作树，树哈希写在交给 Codex 的消息开头。

## 没有执行或不在本批范围的
- click、fill、遮挡检查（包括合法的后代命中）、fill 替换已有值、CLI `--backend native`：2b-2；
- `NotActionable` 只有单元测试，没有真实页面的用例：契约页面上的节点（现为 77 个）都有 DOM 节点，没找到稳定能造出"AX 节点没有 DOM 节点"的页面；
- 跨进程 iframe 是 B1 已知限制：内容不展开（留空），由 `a_cross_process_iframe_is_left_empty_not_failed` 实测；展开它不在 B1 范围；
- 进程在 SIGKILL 之后仍然存活的分支，只靠代码审查；
- ChatGPT 建议的 N2 确定性竞态测试（用同步屏障控制顺序）还没做，留作后续；
- CI 模板仍然没有启用。
