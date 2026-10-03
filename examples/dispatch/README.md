# 从其他 harness（如 DeepSeek agent）派发网页测试任务

`dispatch_webtest.py` 只依赖 Python 标准库。无论哪种模式，它都只在 stdout 输出**一个 JSON 对象**，进度信息打到 stderr。返回码：0 表示 `passed` 或 `needs_review`，1 表示失败或出错。

| 模式 | 适用场景 | 是否调用大模型 | 耗时 |
|---|---|---|---|
| `replay` | 回放已有用例（日常回归） | 否 | 每个用例几秒 |
| `explore` | 由 webtest 直接自动探索，可用 DeepSeek 等任意后端 | 是（webtest 直接调用） | 约 1 分钟起 |
| `agent` | 需要判断的任务：探索、写用例、分析失败原因 | 是（交给 Claude Code，用 step relay 追踪进度） | 约 1 分钟起 |

```bash
# 回放：不调用大模型
examples/dispatch/dispatch_webtest.py replay --flows "flows/*.yaml" --login-flow flows/login.yaml

# 探索：用 DeepSeek 作为大模型
WEBTEST_LLM_PROVIDER=deepseek DEEPSEEK_API_KEY=sk-... \
  examples/dispatch/dispatch_webtest.py explore --url https://staging.example.com --context "账号 …" --out flows/staging

# 交给 Claude Code
examples/dispatch/dispatch_webtest.py agent --task examples/dispatch/task.example.json
```

## agent 模式的流程

1. **脚本先创建 relay 任务。** 它调用 claude-step-relay 自带的 `lib/store.mjs`，所以派发方一开始就拿到了 `exprId`。
2. **把任务交给 Claude Code。** 用 `claude -p` 在本仓库目录下运行，并且只放行 webtest 相关的少数工具。提示词里会带上 `exprId` 和 `RESULT_FILE`。
3. **Claude 执行任务。** 它按 `.claude/skills/webtest/SKILL.md` 的约定来做：
   - 调用 `set_steps` 定义步骤；
   - 每一步调用 `update_step` 更新状态；
   - 结束时调用 `finalize` 收口，并把结果 JSON 写入 `RESULT_FILE`。
4. **派发方在执行过程中查看进度**：读取 `$STEP_RELAY_DIR/experiments/<exprId>.json`，这个脚本自己也会每 5 秒把步骤变化打到 stderr。完整过程记录在 `traces/<exprId>.md`。
5. **脚本汇总输出**：结果 JSON、relay 中的步骤状态、Claude 的最终回复和本次花费。

`STEP_RELAY_DIR` 必须和 MCP 配置里的设置一致。默认值是 `/mnt/c/Users/Administrator/web-relay/step-relay`。

## 任务 JSON

见 `task.example.json`，各字段的含义写在 `dispatch_webtest.py` 开头的说明里。
