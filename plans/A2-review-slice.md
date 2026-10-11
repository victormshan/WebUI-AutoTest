# A2：审核竖切任务书（草案）

状态：**草案，未获批**。前提是 `compliance-gemini-web.md` 由用户选了（b）。如果选（a）或（c），本阶段取消。

## 1. Purpose
在 review-gate 里新增 `web-native` 通道：用 browser-runtime 驱动 reviewgate 自有的 Gemini 网页 profile，完成一次可审计的审核。定位是 Gemini API 不可用时的**低频回退**。

## 2. Dependencies
- A1 的验收决定已经记录；
- `runbook-review-profile.md` 获批，**S5（reviewgate 能否显示窗口）验证通过**，用于首次登录；
- 合规选择为（b），频率上限 N 和冷却时长已定。

## 3. Scope
1. browser-runtime 补最小页面能力：新标签页、条件等待、`insertText`、真实按键、读取指定节点的文本、读取目标 URL。只做 A2 用得到的部分。
2. Gemini 站点适配器（放在 review-gate 里，不放进 runtime）：
   - 就绪检测：输入框出现，或者登录入口出现、跳转到 `sorry`；
   - 新开对话；
   - 提交；
   - 完成判定：新的回答节点出现 + "停止"按钮消失 + 文本连续 N 次采样稳定 + 总超时；
   - 提取：只取最后一个 `model-response` 里的 `message-content`。
3. 分段：沿用 review-gate 现有的分段机制，每段一个新对话。
4. 频率限制和冷却：超过上限，或被风控之后，本通道报 unavailable。
5. 和现有 web-gemini 通道并存，auto 的顺序由用户定。

## 4. 失败语义（不可妥协）

| 情况 | 结果 | 记入审核记录 |
|---|---|---|
| 有效的 `VERDICT: APPROVED`（首行，唯一） | 通过（本段） | 是 |
| 有效的 `VERDICT: REJECTED` | 拒绝（本段） | 是 |
| VERDICT 缺失、重复、互相矛盾、被截断 | **无效**：先在同一对话里要求按格式重答一次，仍无效就报"通道失败" | 是（标为无效） |
| 登录失效、`sorry` 页、验证码、限流 | **通道不可用**，本通道冷却 | 记为【通道回退】原因 |
| 总超时、回答没有完成 | **通道失败**，结果未知 | 同上 |
| 浏览器断连、启动失败 | **通道失败** | 同上 |
| 任一分段失败或缺失 | 整版**不能**判为通过 | — |

- 任何失败都不得产生 PASS，不得静默回退；review-gate 已有的"不回退到 Claude 自审"规则保持不变。
- 审核绑定到 `base..tree`，现有机制不变。

## 5. Non-goals
AXTree、通用定位器、Agent、队列、Cron、OpenTelemetry；webtest；任何反检测。

## 6. Tests（适用等级 L0–L3）
- 单元：VERDICT 解析的全部无效形态；分段汇总规则；频率限制。
- 集成：用一个**本地假页面**模拟 Gemini 的 DOM，覆盖正常、拒绝、无效格式、`sorry` 重定向、登录失效、超时。**不用真实 Gemini 做自动化回归**，以免触发风控。
- 真实站点：每个关键路径只做一次人工监督下的验收，严格遵守频率上限。
- 所有修复都做变异验证。

## 7. Exit Decision
由用户决定：PASS / PASS WITH CONDITIONS / REJECT。
