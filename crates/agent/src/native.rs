//! NativeBackend: Chrome over a CDP pipe ([`browser_runtime`]), no MCP server and no
//! debugging port (B1). Opt-in only; [`crate::McpBackend`] stays the default.
//!
//! Phase 2a covers the lifecycle: one throwaway profile per backend, launch, attach to the
//! page, navigate, close and cleanup. Everything else reports that it is not implemented
//! (an error or `is_error`), never a made-up success.
//!
//! A browser that dies stays dead: every later call fails as disconnected. Nothing here
//! starts a replacement Chrome, unlike chrome-devtools-mcp (see `mcp_client::Browser::restarted`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use browser_runtime::{Browser, LaunchConfig};
use mcp_client::{StorageState, ToolInfo, ToolOutput};
use serde_json::{Value, json};

use crate::backend::{BrowserBackend, Coverage, Diagnostics};

#[derive(Debug, Clone)]
pub struct NativeConfig {
    /// Chrome executable (or a wrapper that execs it).
    pub chrome: PathBuf,
    pub headless: bool,
    /// Where the per-backend throwaway profiles are created (default: the system temp dir).
    pub profile_root: Option<PathBuf>,
    /// Further Chrome flags (e.g. `--no-sandbox`); a debugging port is refused.
    pub chrome_args: Vec<String>,
    /// Per-CDP-command timeout; also bounds waiting for a page load.
    pub command_timeout: Duration,
}

impl NativeConfig {
    pub fn new(chrome: impl Into<PathBuf>) -> Self {
        Self {
            chrome: chrome.into(),
            headless: true,
            profile_root: None,
            chrome_args: Vec::new(),
            command_timeout: Duration::from_secs(30),
        }
    }
}

pub struct NativeBackend {
    // Field order is drop order: Chrome must be gone before its profile is removed.
    browser: Browser,
    /// Flat-mode CDP session of the page this backend drives.
    session: String,
    command_timeout: Duration,
    profile: tempfile::TempDir,
}

impl NativeBackend {
    /// Starts Chrome on a fresh profile and attaches to its page. On any failure the
    /// Chrome process group and the profile are removed before the error returns.
    pub async fn launch(cfg: &NativeConfig) -> Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("webtest-native-");
        let profile = match &cfg.profile_root {
            Some(root) => {
                std::fs::create_dir_all(root)?;
                builder.tempdir_in(root)?
            }
            None => builder.tempdir()?,
        };
        let mut lc = LaunchConfig::new(&cfg.chrome, profile.path().join("chrome"));
        lc.headless = cfg.headless;
        lc.extra_args = cfg.chrome_args.clone();
        lc.command_timeout = cfg.command_timeout;
        let browser = Browser::launch(lc).await.context("launching Chrome")?;
        let session = attach_to_page(&browser)
            .await
            .context("attaching to the page")?;
        for domain in ["Page.enable", "Runtime.enable"] {
            browser.call_in(Some(&session), domain, json!({})).await?;
        }
        tracing::info!(pgid = browser.pgid(), "native backend ready");
        Ok(Self {
            browser,
            session,
            command_timeout: cfg.command_timeout,
            profile,
        })
    }

    /// Chrome's process group.
    pub fn pgid(&self) -> i32 {
        self.browser.pgid()
    }

    /// This backend's throwaway profile (removed on close or drop).
    pub fn profile_dir(&self) -> &Path {
        self.profile.path()
    }

    pub fn is_disconnected(&self) -> bool {
        self.browser.is_disconnected()
    }

    /// Closes Chrome (gracefully, then by signal) and removes the profile.
    /// Returns how Chrome ended: "graceful", "sigterm" or "sigkill".
    pub async fn shutdown(self) -> Result<&'static str> {
        let how = self.browser.close().await?;
        let Self {
            browser, profile, ..
        } = self;
        drop(browser);
        profile.close().context("removing the profile")?;
        Ok(how)
    }

    async fn page(&self, method: &str, params: Value) -> Result<Value> {
        Ok(self
            .browser
            .call_in(Some(&self.session), method, params)
            .await?)
    }
}

async fn attach_to_page(browser: &Browser) -> Result<String> {
    let targets = browser.call("Target.getTargets", json!({})).await?;
    let page = targets["targetInfos"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["type"] == "page")
        .ok_or_else(|| anyhow!("Chrome has no page target"))?;
    let attached = browser
        .call(
            "Target.attachToTarget",
            json!({ "targetId": page["targetId"], "flatten": true }),
        )
        .await?;
    attached["sessionId"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow!("Target.attachToTarget returned no sessionId"))
}

fn not_implemented(what: &str) -> anyhow::Error {
    anyhow!("{what} is not implemented in the native backend yet (B1 phase 2b/3)")
}

#[async_trait]
impl BrowserBackend for NativeBackend {
    /// No tools yet: an empty catalog rather than names that would not work.
    async fn tools(&self) -> Result<Vec<ToolInfo>> {
        Ok(Vec::new())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<ToolOutput> {
        match tool {
            "navigate_page" if args["type"] == "url" || args["type"].is_null() => {
                let Some(url) = args["url"].as_str() else {
                    return Ok(ToolOutput {
                        text: "navigate_page needs `url`".into(),
                        is_error: true,
                        ..Default::default()
                    });
                };
                self.navigate(url).await
            }
            _ => Ok(ToolOutput {
                text: format!("`{tool}` {args} is not supported by the native backend yet"),
                is_error: true,
                ..Default::default()
            }),
        }
    }

    /// Navigates and waits for the load event. A page that does not load in time is an
    /// `Err` (outcome unknown), not a tool error.
    async fn navigate(&self, url: &str) -> Result<ToolOutput> {
        let mut events = self.browser.events();
        let nav = self.page("Page.navigate", json!({ "url": url })).await?;
        if let Some(err) = nav["errorText"].as_str() {
            return Ok(ToolOutput {
                text: format!("navigation to {url} failed: {err}"),
                is_error: true,
                ..Default::default()
            });
        }
        if nav["loaderId"].is_string() {
            let session = self.session.as_str();
            let loaded = async {
                loop {
                    // The event channel outlives the pipe, so poll for a disconnect too.
                    if self.is_disconnected() {
                        bail!(browser_runtime::Error::BrowserDisconnected);
                    }
                    let Ok(next) =
                        tokio::time::timeout(Duration::from_millis(200), events.recv()).await
                    else {
                        continue;
                    };
                    match next {
                        Ok(e)
                            if e.method == "Page.loadEventFired"
                                && e.session_id.as_deref() == Some(session) =>
                        {
                            return Ok(());
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            bail!(browser_runtime::Error::BrowserDisconnected)
                        }
                    }
                }
            };
            match tokio::time::timeout(self.command_timeout, loaded).await {
                Ok(r) => r?,
                Err(_) => bail!(
                    "{url} did not finish loading within {:?} (outcome unknown)",
                    self.command_timeout
                ),
            }
        }
        Ok(ToolOutput {
            text: format!("Successfully navigated to {url}."),
            ..Default::default()
        })
    }

    async fn snapshot(&self) -> Result<String> {
        Err(not_implemented("take_snapshot"))
    }

    async fn storage_state(&self) -> Result<StorageState> {
        Err(not_implemented("storage_state"))
    }

    async fn apply_storage_state(&self, _state: &StorageState) -> Result<()> {
        Err(not_implemented("apply_storage_state"))
    }

    /// Not collected yet. After a disconnect the answer is "failed", not merely
    /// "unsupported": whatever the page logged is gone with the browser.
    async fn diagnostics(&self) -> Diagnostics {
        let coverage = || {
            if self.is_disconnected() {
                Coverage::Failed {
                    reason: "browser disconnected".into(),
                }
            } else {
                Coverage::Unsupported
            }
        };
        Diagnostics {
            console: coverage(),
            network: coverage(),
            ..Default::default()
        }
    }

    async fn close(self: Box<Self>) -> Result<()> {
        self.shutdown().await.map(|_| ())
    }
}
