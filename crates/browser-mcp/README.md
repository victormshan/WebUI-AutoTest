# Rust browser MCP gateway

Linux/WSL only, matching browser-runtime. One temporary profile and one page per MCP process. Uses the Rust Runtime directly over a CDP pipe; no Node MCP server or LLM is required.

Build: `cargo build -p browser-mcp`.
Run: `target/debug/browser-mcp --chrome /home/administrator/.local/bin/chrome-wsl`.
Protocol stdout is reserved for MCP; diagnostics go to stderr. EOF, SIGTERM and Ctrl-C close the owned Chrome process group. Browser failure is reported, never silently relaunched.

OpenCode in WSL: merge `opencode.browser.json` into your project `opencode.json`, then run `opencode mcp list`. Paths in the sample are specific to this workstation. With OpenCode on Windows, use command `["wsl.exe", "-d", "Ubuntu", "-e", "/home/administrator/projects/Chrome-MCP/target/debug/browser-mcp", "--chrome", "/home/administrator/.local/bin/chrome-wsl"]`. Do not overwrite other MCP entries.

Official configuration reference: https://opencode.ai/docs/mcp-servers/

Tools: browser_navigate, browser_snapshot, browser_click, browser_fill. Navigation acknowledges initiation; use snapshots to verify the expected URL and content. Snapshots truncate text and element lists. Actions use CSS selectors with exactly one match and DOM events, so sites requiring trusted input may reject them. No automatic retries for actions. Requests are serialized per page; cancellation does not imply that an action did not execute.

This local development gateway can access URLs reachable by Chrome, including localhost. It is not a remote or multi-tenant service and does not enforce domain allowlists. No arbitrary JavaScript, credential export, file upload or generic CDP tool is exposed. Treat webpage content as untrusted. Close the MCP process to discard the temporary browser profile.

Verification: `python3 crates/browser-mcp/smoke.py` (requires the built debug binary and Chrome). The project `opencode.json` enables rust_browser on this workstation. Run OpenCode in the project directory; to check connection only, `npx --yes --package opencode-ai opencode mcp list`. No model credentials are needed for the connection check.
