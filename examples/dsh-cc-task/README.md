# 通过 dsh-web-relay 的 cc 通道派发 webtest 任务

dsh（DeepSeek Harness）的 dsh-web-relay 插件已经有一条通往 Claude Code 的文件队列通道：

```
dsh 主 agent → <CC_TASKS>/queue/<taskId>.task.json
  → cc-watchdog（systemd 用户服务，每 5 秒轮询；先用 v2 校验器检查任务）
  → runner.sh: timeout 900 claude -p …（工作目录为 <CC_TASKS>）
  → tasks/<taskId>/{out/, done.flag, result.json}
```

本目录的作用是让 webtest 测试也走这条通道。这样不需要另起一套派发机制，dsh 已有的配额管理、失败分类、
自愈和 v2 校验都能直接沿用。

## 投递任务

```bash
# 回放（不调用大模型，最省额度）
examples/dsh-cc-task/make_webtest_task.py --url http://127.0.0.1:8765/ --mode replay \
    --flows "flows/*.yaml" --login-flow flows/login.yaml

# 按目标录制
examples/dsh-cc-task/make_webtest_task.py --url https://staging.example.com --mode run \
    --goal "用 alice/xxx 登录，把商品加入购物车并结算，确认显示订单已提交"

# 自动探索
examples/dsh-cc-task/make_webtest_task.py --url https://staging.example.com --mode explore \
    --context "测试账号 alice / 密码 xxx" --max-tasks 4

# 只打印任务内容，不入队
… --dry-run
```

脚本会生成 v2 格式的任务（`kind: review`），先用 relay 的 `task-schema-cli.mjs validate-task` 校验，
再用"先写临时文件、再改名"的方式入队，避免 watchdog 读到写了一半的文件。

dsh 主 agent 也可以不用这个脚本，自己按同样的格式写任务（字段见 `--dry-run` 的输出）。

## 结果约定

| 文件 | 内容 |
|---|---|
| `result.json` | 由 runner 写入：`done` 或 `failed`，加上 errorCode（配额、超时、权限、缺少完成标记等） |
| `out/review.md` | 第一行是 `VERDICT: APPROVED`（仅当测试全部通过）或 `VERDICT: REJECTED`，可直接用 cc-channel 的 `parseCcVerdict` 解析 |
| `out/webtest-result.json` | 机器可读的结果，格式见技能文件第 4 节（status 为 passed、failed、needs_review 或 error） |
| `out/report.md` | 给人看的报告 |
| `out/summary.md`、`out/junit.xml` | 回放报告 |
| `out/flows/` | run 或 explore 生成的用例和 explore.md |

- **工具或环境故障（status=error）时不会写 `done.flag`**：runner 会把任务判为 failed，dsh 现有的降级逻辑会接手。
- **测试失败不等于通道失败**：被测网站有缺陷时，任务照常写 `done.flag`，通道状态是 done，结论是 REJECTED。

## 用 dsh 自己的模块检查结果

```bash
node examples/dsh-cc-task/check_result.mjs <taskId>
```

这个脚本依次调用 relay 的 `validate-result`、`readReviewOut` 和 `parseCcVerdict`，
并核对 VERDICT 和 webtest-result.json 的 status 是否一致。一致时退出码为 0。

## 注意

- **技能不会自动加载**：runner 的工作目录是 `<CC_TASKS>`，不在 webtest 仓库里，所以任务提示里写明了技能文件的绝对路径。
- **权限比 agent 模式宽**：runner 放行了全部 `Read,Write,Edit,Bash`。约束只靠任务说明和技能文件里的规则：
  不 commit、只在指定目录下写文件、遵守安全规则。
- **用不了 claude-step-relay**：runner 没有放行 MCP 工具，所以这条通道里不能用 step relay 记录进度。
  进度和留痕由 dsh 自己的 expr 和三方轨迹负责。
- **被测站点要先能访问**：例如演示站点需要先运行 `python3 fixtures/shop/server.py 8765`。站点打不开时，任务按 error 处理。
