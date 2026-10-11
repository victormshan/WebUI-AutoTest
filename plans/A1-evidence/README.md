# A1 原始证据

| 文件 | 内容 |
|---|---|
| `tests.txt` | 单元测试 4/4；真实 Chrome 集成测试 5/5（命令、环境、输出），之后残留 Chrome 进程为 0 |
| `no-tcp-listener.txt` | 探针运行期间的快照：Chrome 进程组（18 个进程）没有任何 TCP 监听，`ss -ltnp` 原始输出 |
| `gemini-probe-run-{1,2,3}.json` | 有界面、pipe 接管已登录的 profile：答案与随机数一致，含信号时间线 |
| `gemini-probe-run-h.json` | 无头模式，同上 |
| `gemini-probe-run-5.json` | 无头模式：页面始终没有就绪（当时探针还不记录最后的观测，页面状态未知） |
| `gemini-probe-run-{6,7}.json` | 无头模式：被重定向到 `google.com/sorry`（Google 异常流量验证页）；查询参数已脱敏 |

时间线里只有计数和长度，没有页面文本；最终答案只是三个汉字。
