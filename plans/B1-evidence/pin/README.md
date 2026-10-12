# 固定 chrome-devtools-mcp 版本（2026-10-13）

ChatGPT 在 B1 阶段 1 审核里的建议：单独做一个小改动，把版本固定到经过验证的版本。

- **改动**：
  - `mcp_client::DEFAULT_PACKAGE = "chrome-devtools-mcp@1.10.1"`；
  - `BrowserConfig::default()` 和 CLI `--mcp-package` 的默认值都用它，原来是 `@latest`；
  - README 新增"升级 chrome-devtools-mcp"一节：先用 `--mcp-package` 试跑 e2e 并核对输出样本，再修改常量。
- **为什么是 1.10.1**：阶段 1 单元测试里的输出样本取自这个版本；阶段 1 的 e2e 也都跑在这个版本上（当时 `@latest` 解析到的就是 1.10.1）。
- **证据**：
  - `e2e-pinned-1.10.1.log`：没有设置 `WEBTEST_MCP_PACKAGE`，`cargo test -p webtest -p agent -- --ignored` 结果 8/8 通过；
  - npx 为 `@1.10.1` 这个包规格新建了一个缓存目录（`~/.npm/_npx/ba38acecdf97096f`，2026-10-13 创建），里面安装的版本是 1.10.1；
  - `cargo test --workspace`、clippy `-D warnings`、fmt 都通过。
- **不在本次范围**：B1 架构不变。
