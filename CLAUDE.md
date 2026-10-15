# 项目交接提醒

## OpenCode 浏览器 MCP 接入交接（2026-10-16）

涉及浏览器 Runtime、MCP、OpenCode、workspace 构建或本次未提交改动时，先阅读 [接入交接文档](plans/OpenCode-integration-handoff.md)。其中记录新增 browser-mcp crate、项目连接配置、真实验收证据及尚未覆盖的边界。

本次修改在 WSL 的 `/home/administrator/projects/Chrome-MCP`；Windows `.inspection/WebUI-AutoTest` 是不同版本的检查副本。保留已有 review-gate/M2 暂存改动，避免覆盖或混入接入提交。OpenCode 连接和真实浏览器工具已验证，模型驱动的完整 Agent 任务及全工作区回归测试未执行。
