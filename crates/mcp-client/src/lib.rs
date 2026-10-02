//! Thin async wrapper around the `chrome-devtools-mcp` server.
//!
//! The server is spawned as a child process (stdio transport) and driven
//! through the official Rust MCP SDK (`rmcp`).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ContentBlock},
    service::{RoleClient, RunningService},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde::Serialize;
use serde_json::{Map, Value};

/// How to launch `chrome-devtools-mcp` and the browser behind it.
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    /// npm package spec run via `npx`.
    pub package: String,
    /// Chrome executable; `None` lets the server find a system Chrome.
    pub chrome_path: Option<PathBuf>,
    pub headless: bool,
    /// Use a throwaway profile instead of a persistent one.
    pub isolated: bool,
    pub viewport: Option<String>,
    /// Extra flags passed straight to Chrome (e.g. `--no-sandbox`).
    pub chrome_args: Vec<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            package: "chrome-devtools-mcp@latest".into(),
            chrome_path: None,
            headless: true,
            isolated: true,
            viewport: Some("1280x800".into()),
            chrome_args: vec!["--no-sandbox".into()],
        }
    }
}

impl BrowserConfig {
    fn server_args(&self) -> Vec<String> {
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
        if self.isolated {
            args.push("--isolated".into());
        }
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
}

impl Browser {
    pub async fn launch(cfg: &BrowserConfig) -> Result<Self> {
        let args = cfg.server_args();
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
        Ok(Self { service })
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
