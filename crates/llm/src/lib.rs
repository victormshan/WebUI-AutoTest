//! Minimal LLM abstraction: one system prompt + one user message -> text.
//!
//! Two backends:
//! - [`AnthropicApi`]: Claude Messages API (needs `ANTHROPIC_API_KEY`).
//! - [`ClaudeCli`]: shells out to `claude -p`, reusing the local Claude Code login.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

#[async_trait]
pub trait Llm: Send + Sync {
    async fn complete(&self, system: &str, user: &str) -> Result<String>;
    fn describe(&self) -> String;
}

/// Picks the API backend when `ANTHROPIC_API_KEY` is set, otherwise the CLI.
pub fn from_env(model: Option<String>) -> Box<dyn Llm> {
    let model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.is_empty() => Box::new(AnthropicApi::new(key, model)),
        _ => Box::new(ClaudeCli::new(model)),
    }
}

pub struct AnthropicApi {
    http: reqwest::Client,
    key: String,
    model: String,
    max_tokens: u32,
}

impl AnthropicApi {
    pub fn new(key: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            key,
            model,
            max_tokens: 4096,
        }
    }
}

#[async_trait]
impl Llm for AnthropicApi {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system,
            "messages": [{ "role": "user", "content": user }],
        });
        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .context("Anthropic API request failed")?;
        let status = resp.status();
        let v: Value = resp
            .json()
            .await
            .context("invalid Anthropic API response")?;
        if !status.is_success() {
            bail!("Anthropic API error {status}: {v}");
        }
        let text = v["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        Ok(text)
    }

    fn describe(&self) -> String {
        format!("anthropic-api ({})", self.model)
    }
}

pub struct ClaudeCli {
    model: String,
    binary: String,
}

impl ClaudeCli {
    pub fn new(model: String) -> Self {
        Self {
            model,
            binary: std::env::var("WEBTEST_CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
        }
    }
}

#[async_trait]
impl Llm for ClaudeCli {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let mut child = tokio::process::Command::new(&self.binary)
            .args([
                "-p",
                "--output-format",
                "json",
                "--model",
                &self.model,
                "--system-prompt",
                system,
                // Pure text completion: no built-in tools, no MCP servers.
                "--tools",
                "",
                "--strict-mcp-config",
                "--no-session-persistence",
            ])
            // Neutral cwd so no project CLAUDE.md leaks into the prompt.
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to run `{}` (is Claude Code installed?)",
                    self.binary
                )
            })?;

        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin.write_all(user.as_bytes()).await?;
        drop(stdin);

        let out = child.wait_with_output().await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let v: Value = serde_json::from_str(stdout.trim()).with_context(|| {
            format!(
                "unexpected `claude -p` output (exit {}): {}{}",
                out.status,
                stdout.chars().take(500).collect::<String>(),
                String::from_utf8_lossy(&out.stderr)
            )
        })?;
        if v["is_error"].as_bool().unwrap_or(false) {
            bail!("claude -p returned an error: {}", v["result"]);
        }
        Ok(v["result"].as_str().unwrap_or_default().to_string())
    }

    fn describe(&self) -> String {
        format!("claude-cli ({})", self.model)
    }
}

/// Extracts the first balanced top-level JSON object from model output
/// (tolerates code fences and surrounding prose).
pub fn extract_json(text: &str) -> Result<Value> {
    let bytes = text.as_bytes();
    let mut start = None;
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            match b {
                _ if esc => esc = false,
                b'\\' => esc = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' if start.is_some() => in_str = true,
            b'{' => {
                start.get_or_insert(i);
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let s = &text[start.unwrap()..=i];
                    return serde_json::from_str(s)
                        .with_context(|| format!("model returned invalid JSON: {s}"));
                }
            }
            _ => {}
        }
    }
    bail!(
        "no JSON object in model output: {}",
        text.chars().take(300).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_fenced_json() {
        let v = extract_json("Sure:\n```json\n{\"a\": {\"b\": \"}\"}}\n```").unwrap();
        assert_eq!(v["a"]["b"], "}");
    }

    #[test]
    fn errors_without_json() {
        assert!(extract_json("no json here").is_err());
    }
}
