# browser-runtime

Chrome control over CDP for this workspace (ADR-0 `plans/ADR-0-unified-browser-runtime.md`).
Current state: **A1 spike** (`plans/A1-cdp-spike.md`). It launches a Chrome it owns and talks
CDP over `--remote-debugging-pipe`; there are no page, locator or site APIs yet.

Why a pipe and not a port: review-gate runs as its own user so the implementer cannot reach the
reviewer. A local debugging port would let any local user drive the reviewer's logged-in
browser. A pipe is reachable only by the process that launched Chrome. Chrome also exits when
the pipe closes, so a runtime that dies, even by SIGKILL, leaves no browser behind.

```rust
let mut cfg = LaunchConfig::new("/path/to/chrome", "/path/to/dedicated-profile");
cfg.headless = false; // e.g. a visible window on WSLg for a manual sign-in
let b = Browser::launch(cfg).await?;
let v = b.call("Browser.getVersion", serde_json::json!({})).await?;
b.close().await?; // "graceful" | "sigterm" | "sigkill"
```

- **Arguments:** `LaunchConfig` adds only `--remote-debugging-pipe`, `--user-data-dir`,
  `--no-first-run`, `--no-default-browser-check`, `--headless=new` (when headless) and
  `about:blank`. It never adds `--enable-automation`, `--no-sandbox` or
  `--disable-dev-shm-usage`. `--remote-debugging-port` is rejected.
- **Profile:** the profile directory is created with mode 0700. A profile already in use by
  another Chrome makes `launch` fail with `Error::Launch`.
- **Process ownership:** Chrome runs in its own process group. `close` asks Chrome to exit,
  then sends SIGTERM and SIGKILL to the group if needed. `Drop` kills the group as a last
  resort.
- **Errors:** `Config`, `Launch`, `BrowserDisconnected`, `Timeout` (the command's effect is
  unknown), `Protocol` and `Io`. Messages contain no page content.

## Tests

```bash
cargo test -p browser-runtime                                       # unit
cargo test -p browser-runtime -- --ignored --test-threads=1         # real Chrome
```

Chrome comes from `BROWSER_RUNTIME_CHROME`, defaulting to `~/.local/bin/chrome-wsl`
(Chrome for Testing, `scripts/setup-wsl.sh`).

## Examples (A1, see `plans/A1-report.md`)

```bash
# First sign-in by hand: a plain Chrome without any debugging flag, because Google refuses
# sign-in in a pipe-debugged Chrome.
cargo run -p browser-runtime --example login_window -- <profile-dir> https://gemini.google.com/app
# Then the runtime takes the signed-in profile over: new chat, trusted input, wait, read.
cargo run -p browser-runtime --example gemini_probe -- <profile-dir> [--headless]
```
