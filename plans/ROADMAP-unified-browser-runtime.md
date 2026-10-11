# 统一浏览器 Runtime 路线图

基线：`ADR-0-unified-browser-runtime.md`（必须先确认）。v1.2 的 P0–P15 保留作**能力清单**，**按使用方重排实施顺序**，不再机械地从 P0 依次做到 P15。

| 阶段 | 内容 | 首个使用方 | 用到的 v1.2 能力 | 验收等级 | 退出条件 |
|---|---|---|---|---|---|
| **A0 决策** | ADR-0 确认；合规取舍（API / 网页、账号）；确定第一个站点 | — | — | 文档 | 用户确认 ADR 和合规取舍；没有未处理的合规阻塞项 |
| **A1 试验（spike）** | 在现有 workspace 里新建 `crates/browser-runtime`（含 P0 骨架）；验证专用 profile、pipe 传输、真实输入、条件等待、读取回答、进程回收、reviewgate 用户能否显示窗口 | review-gate（技术验证） | P0、P1 最小集 | L0–L2，以及 L3 的部分项 | 见 `A1-cdp-spike.md`：全部"必须验证"项给出 PASS / FAIL / 阻塞结论和证据 |
| **A2 竖切**（**暂停**：2026-10-12 合规选择 (a)） | review-gate 新增 `web-native` 通道 + 一个站点适配器；完整跑一次真实审核 | review-gate | P2/P3/P5(CSS)/P6/P7 的最小子集 | L0–L3 | 真实审核成功；拒绝能被正确识别；超时、异常页面、格式无效不会误判为通过；profile 隔离；Chrome 可回收；旧通道不受影响 |
| **A3 并行**（**暂停**） | 同一批审核任务分别走旧通道和新通道，独立对照（不共享对话或标签页）；故障注入 | review-gate | P8 有限恢复（如需要） | L3，L4 的部分项 | 成功率、有效结果率、超时率、误判率、人工介入次数、端到端耗时、维护成本有数据；用户批准切换或维持现状 |
| **B1 第二后端** | webtest 的 `agent` 抽出后端接口；`NativeBackend` 先实现现有 13 个工具的等价能力 | webtest | P2–P7、Log/Network、对话框、Cookie | L0–L3 | 同一组 fixture/flow 在两个后端上跑，结果和证据可对比；`McpBackend` 不受破坏 |
| **B2 扩展与生产化** | 按对照结果决定是否补完整 AXTree、Locator、Recovery、并发管理、长期运行和 Recycling | webtest | P4/P5/P8/P14/P15 | L4–L5 | 默认后端是否迁移由实测指标决定；收益不足就维持双后端 |

**当前状态（2026-10-12）**：A0、A1 完成（A1 = PASS WITH CONDITIONS）；A 线暂停；下一步 **B1**（任务书 `B1-webtest-native-backend.md`，待批准）。

**范围外**：v1.2 的 P9（MCP Gateway）和 P10（Agent Runtime）暂不立项（webtest 已经有 agent 循环）；P11–P13（Persistence、Queue、Cron、Notifications）不在本项目内。

**统一规则**（每个阶段都适用）：
- 先写任务书，用户确认后再实施；
- 实现按 review-gate 三方流程走（外部 AI 审核每个版本并签名）；
- NOT RUN ≠ PASS；关键修复做变异验证；Mock 不能代替真实 Chrome；
- 偏离基线先提 ADR。
