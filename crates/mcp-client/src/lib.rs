//! Thin async wrapper around the `chrome-devtools-mcp` server.
//!
//! The server is spawned as a child process (stdio transport) and driven
//! through the official Rust MCP SDK (`rmcp`). Chrome additionally exposes a
//! DevTools port so browser-level state (cookies) can be read and restored
//! via [`cdp`].

pub mod cdp;
mod storage;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ContentBlock},
    service::{RoleClient, RunningService},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde::Serialize;
use serde_json::{Map, Value, json};
pub use storage::{OriginStorage, StorageState};

/// How to launch `chrome-devtools-mcp` and the browser behind it.
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    /// npm package spec run via `npx`.
    pub package: String,
    /// Chrome executable; `None` lets the server find a system Chrome.
    pub chrome_path: Option<PathBuf>,
    pub headless: bool,
    /// Profile directory; `None` uses a throwaway temp profile per launch.
    pub user_data_dir: Option<PathBuf>,
    pub viewport: Option<String>,
    /// Extra flags passed straight to Chrome (e.g. `--no-sandbox`).
    pub chrome_args: Vec<String>,
    /// Cookies / localStorage injected right after launch (login reuse).
    pub storage_state: Option<StorageState>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            package: "chrome-devtools-mcp@latest".into(),
            chrome_path: None,
            headless: true,
            user_data_dir: None,
            viewport: Some("1280x800".into()),
            chrome_args: vec!["--no-sandbox".into()],
            storage_state: None,
        }
    }
}

impl BrowserConfig {
    fn server_args(&self, profile: &Path) -> Vec<String> {
        let mut args = vec![
            "-y".to_string(),
            self.package.clone(),
            "--no-usage-statistics".into(),
            "--no-performance-crux".into(),
            // One agent drives one page, so tools need no explicit pageId.
            "--no-page-id-routing".into(),
        ];
        if self.headless {
            args.push("--headless".into());
        }
        args.push(format!("--userDataDir={}", profile.display()));
        // Port 0: Chrome picks one and records it in <profile>/DevToolsActivePort.
        args.push("--chromeArg=--remote-debugging-port=0".into());
        if let Some(path) = &self.chrome_path {
            args.push(format!("--executablePath={}", path.display()));
        }
        if let Some(vp) = &self.viewport {
            args.push(format!("--viewport={vp}"));
        }
        for a in &self.chrome_args {
            args.push(format!("--chromeArg={a}"));
        }
        args
    }
}

/// A tool exposed by the MCP server, in a provider-neutral shape.
#[derive(Debug, Clone, Serialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Flattened result of a tool call.
#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub text: String,
    /// Base64 images returned by the tool (e.g. screenshots).
    pub images: Vec<(String, String)>,
    pub is_error: bool,
}

pub struct Browser {
    service: RunningService<RoleClient, ()>,
    profile: PathBuf,
    /// Keeps a throwaway profile alive until the browser is dropped.
    _temp_profile: Option<tempfile::TempDir>,
}

impl Browser {
    pub async fn launch(cfg: &BrowserConfig) -> Result<Self> {
        let (profile, temp) = match &cfg.user_data_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                (dir.clone(), None)
            }
            None => {
                let t = tempfile::Builder::new()
                    .prefix("webtest-profile-")
                    .tempdir()?;
                (t.path().to_path_buf(), Some(t))
            }
        };
        // A stale port file from an earlier run would point at a dead browser.
        let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
        let args = cfg.server_args(&profile);
        tracing::debug!(?args, "spawning chrome-devtools-mcp");
        let transport =
            TokioChildProcess::new(tokio::process::Command::new("npx").configure(|cmd| {
                cmd.args(&args);
            }))
            .context("failed to spawn `npx chrome-devtools-mcp` (is Node.js installed?)")?;
        let service = ()
            .serve(transport)
            .await
            .context("MCP handshake with chrome-devtools-mcp failed")?;
        let browser = Self {
            service,
            profile,
            _temp_profile: temp,
        };
        if let Some(state) = &cfg.storage_state {
            browser.apply_storage_state(state).await?;
        }
        Ok(browser)
    }

    /// WebSocket URL of the browser's DevTools endpoint.
    pub async fn cdp_url(&self) -> Result<String> {
        // Chrome starts lazily on the first tool call.
        self.call("list_pages", json!({})).await?;
        let file = self.profile.join("DevToolsActivePort");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(text) = std::fs::read_to_string(&file) {
                let mut lines = text.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                    return Ok(format!("ws://127.0.0.1:{}{}", port.trim(), path.trim()));
                }
            }
            if Instant::now() >= deadline {
                bail!("Chrome did not write {}", file.display());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Captures all cookies (including HttpOnly) plus localStorage of the current page's origin.
    pub async fn storage_state(&self) -> Result<StorageState> {
        let ws = self.cdp_url().await?;
        let cookies = cdp::call(&ws, "Storage.getCookies", json!({})).await?["cookies"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut origins = Vec::new();
        let out = self
            .call(
                "evaluate_script",
                json!({ "function": "() => ({ origin: location.origin, items: Object.entries(localStorage) })" }),
            )
            .await?;
        if out.is_error {
            tracing::debug!("no localStorage captured: {}", out.text);
        } else if let Some(o) = storage::parse_origin_dump(&out.text)
            && !o.local_storage.is_empty()
        {
            origins.push(o);
        }
        Ok(StorageState { cookies, origins })
    }

    /// Restores cookies (before any navigation) and per-origin localStorage.
    pub async fn apply_storage_state(&self, state: &StorageState) -> Result<()> {
        if !state.cookies.is_empty() {
            let ws = self.cdp_url().await?;
            let cookies: Vec<Value> = state.cookies.iter().map(storage::to_cookie_param).collect();
            cdp::call(&ws, "Storage.setCookies", json!({ "cookies": cookies })).await?;
        }
        for o in &state.origins {
            let nav = self.navigate(&format!("{}/", o.origin)).await?;
            if nav.is_error {
                bail!(
                    "could not open {} to restore localStorage: {}",
                    o.origin,
                    nav.text
                );
            }
            let items = serde_json::to_string(&o.local_storage)?;
            let out = self
                .call(
                    "evaluate_script",
                    json!({ "function": format!("() => {{ for (const [k, v] of {items}) localStorage.setItem(k, v); return true; }}") }),
                )
                .await?;
            if out.is_error {
                bail!(
                    "restoring localStorage for {} failed: {}",
                    o.origin,
                    out.text
                );
            }
        }
        tracing::debug!(
            cookies = state.cookies.len(),
            origins = state.origins.len(),
            "storage state applied"
        );
        Ok(())
    }

    pub async fn tools(&self) -> Result<Vec<ToolInfo>> {
        let tools = self.service.list_all_tools().await?;
        Ok(tools
            .into_iter()
            .map(|t| ToolInfo {
                name: t.name.into_owned(),
                description: t.description.map(|d| d.into_owned()).unwrap_or_default(),
                input_schema: Value::Object((*t.input_schema).clone()),
            })
            .collect())
    }

    /// Calls a tool. Tool-level failures come back as `is_error`, not `Err`.
    pub async fn call(&self, name: &str, args: Value) -> Result<ToolOutput> {
        let arguments = match args {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            other => bail!("tool arguments must be a JSON object, got {other}"),
        };
        let res = self
            .service
            .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(arguments))
            .await
            .with_context(|| format!("MCP call `{name}` failed"))?;

        let mut out = ToolOutput {
            is_error: res.is_error.unwrap_or(false),
            ..Default::default()
        };
        for block in res.content {
            match block {
                ContentBlock::Text(t) => {
                    if !out.text.is_empty() {
                        out.text.push('\n');
                    }
                    out.text.push_str(&t.text);
                }
                ContentBlock::Image(i) => out.images.push((i.mime_type, i.data)),
                _ => {}
            }
        }
        Ok(out)
    }

    pub async fn navigate(&self, url: &str) -> Result<ToolOutput> {
        self.call(
            "navigate_page",
            serde_json::json!({ "type": "url", "url": url }),
        )
        .await
    }

    /// Accessibility-tree snapshot; element `uid`s in it are valid until the next snapshot.
    pub async fn snapshot(&self) -> Result<String> {
        let out = self.call("take_snapshot", serde_json::json!({})).await?;
        if out.is_error {
            bail!("take_snapshot failed: {}", out.text);
        }
        Ok(out.text)
    }

    pub async fn close(self) -> Result<()> {
        self.service.cancel().await?;
        Ok(())
    }
}
