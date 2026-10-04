//! Minimal LLM abstraction: one system prompt + one user message -> text.
//!
//! Backends (see [`Settings::from_env`] for how one is chosen):
//! - [`AnthropicApi`]: Anthropic Messages protocol — Claude, or any compatible
//!   endpoint via a custom base URL (e.g. DeepSeek's `/anthropic`).
//! - [`OpenAiCompat`]: OpenAI Chat Completions protocol — DeepSeek, Qwen,
//!   vLLM/Ollama and other compatible servers.
//! - [`ClaudeCli`]: shells out to `claude -p`, reusing the local Claude Code login.

use std::process::Stdio;
use std::time::Duration;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
    ClaudeCli,
    /// Any external command: prompt on stdin, answer on stdout (e.g. `review-gate ask`).
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// `x-api-key: <key>` (Anthropic default)
    ApiKey,
    /// `Authorization: Bearer <key>`
    Bearer,
}

/// Resolved backend configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub provider: Provider,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub auth: Auth,
    pub timeout: Duration,
    /// Shell command for [`Provider::Command`].
    pub command: Option<String>,
}

impl Settings {
    /// Reads the process environment. `model` (e.g. `--model`) wins over env.
    pub fn from_env(model: Option<String>) -> Result<Self> {
        Self::from_vars(&|k| std::env::var(k).ok().filter(|v| !v.is_empty()), model)
    }

    /// Resolution rules:
    ///
    /// | `WEBTEST_LLM_PROVIDER` | protocol | base URL default | key |
    /// |---|---|---|---|
    /// | `anthropic` | Messages | `ANTHROPIC_BASE_URL` or api.anthropic.com | `ANTHROPIC_API_KEY` (x-api-key) / `ANTHROPIC_AUTH_TOKEN` (Bearer) |
    /// | `openai` | Chat Completions | `OPENAI_BASE_URL` or api.openai.com/v1 | `OPENAI_API_KEY` |
    /// | `deepseek` | Chat Completions | api.deepseek.com | `DEEPSEEK_API_KEY` |
    /// | `claude-cli` | `claude -p` | – | Claude Code login |
    /// | `command` | `sh -c $WEBTEST_LLM_COMMAND`, prompt on stdin | – | – |
    ///
    /// `WEBTEST_LLM_BASE_URL` / `WEBTEST_LLM_API_KEY` override the defaults.
    /// Without a provider: an Anthropic key selects `anthropic`, else `claude-cli`.
    /// Model: `model` arg, else `WEBTEST_MODEL`, else the provider default.
    pub fn from_vars(var: &dyn Fn(&str) -> Option<String>, model: Option<String>) -> Result<Self> {
        let generic_key = var("WEBTEST_LLM_API_KEY");
        let provider_name = var("WEBTEST_LLM_PROVIDER").map(|p| p.to_lowercase());
        let provider_name = provider_name.as_deref().unwrap_or_else(|| {
            if generic_key.is_some()
                || var("ANTHROPIC_API_KEY").is_some()
                || var("ANTHROPIC_AUTH_TOKEN").is_some()
            {
                "anthropic"
            } else {
                "claude-cli"
            }
        });

        let (provider, base_default, key, auth, model_default) = match provider_name {
            "anthropic" => {
                let (key, auth) = match (var("ANTHROPIC_API_KEY"), var("ANTHROPIC_AUTH_TOKEN")) {
                    (Some(k), _) => (Some(k), Auth::ApiKey),
                    (None, Some(t)) => (Some(t), Auth::Bearer),
                    _ => (None, Auth::ApiKey),
                };
                let base =
                    var("ANTHROPIC_BASE_URL").unwrap_or_else(|| "https://api.anthropic.com".into());
                (Provider::Anthropic, base, key, auth, Some(DEFAULT_MODEL))
            }
            "openai" => {
                let base =
                    var("OPENAI_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".into());
                (
                    Provider::OpenAi,
                    base,
                    var("OPENAI_API_KEY"),
                    Auth::Bearer,
                    None,
                )
            }
            "deepseek" => (
                Provider::OpenAi,
                "https://api.deepseek.com".into(),
                var("DEEPSEEK_API_KEY"),
                Auth::Bearer,
                Some("deepseek-chat"),
            ),
            "claude-cli" | "claude" => (
                Provider::ClaudeCli,
                String::new(),
                None,
                Auth::ApiKey,
                Some(DEFAULT_MODEL),
            ),
            "command" => (
                Provider::Command,
                String::new(),
                None,
                Auth::ApiKey,
                Some("external"),
            ),
            other => bail!(
                "unknown WEBTEST_LLM_PROVIDER `{other}` (expected anthropic, openai, deepseek, claude-cli or command)"
            ),
        };

        let model = model
            .or_else(|| var("WEBTEST_MODEL"))
            .or_else(|| model_default.map(str::to_string))
            .with_context(|| {
                format!(
                    "provider `{provider_name}` needs a model: pass --model or set WEBTEST_MODEL"
                )
            })?;
        let api_key = generic_key.or(key);
        let command = var("WEBTEST_LLM_COMMAND");
        if provider == Provider::Command && command.is_none() {
            bail!(
                "provider `command` needs WEBTEST_LLM_COMMAND (a shell command reading the prompt on stdin)"
            );
        }
        if !matches!(provider, Provider::ClaudeCli | Provider::Command) && api_key.is_none() {
            bail!(
                "provider `{provider_name}` needs an API key (WEBTEST_LLM_API_KEY or the provider's key variable)"
            );
        }
        let timeout = Duration::from_secs(
            var("WEBTEST_LLM_TIMEOUT_SECS")
                .and_then(|s| s.parse().ok())
                // External commands may relay to slow channels (web-gemini ~20-90s per attempt).
                .unwrap_or(if provider == Provider::Command {
                    600
                } else {
                    180
                }),
        );
        Ok(Settings {
            provider,
            model,
            base_url: var("WEBTEST_LLM_BASE_URL")
                .unwrap_or(base_default)
                .trim_end_matches('/')
                .to_string(),
            api_key,
            auth,
            timeout,
            command,
        })
    }

    /// Settings for a second, independent model (the cross-reviewer), read from the
    /// `WEBTEST_REVIEW_*` twins of the `WEBTEST_*` variables (`WEBTEST_REVIEW_LLM_PROVIDER`,
    /// `WEBTEST_REVIEW_LLM_COMMAND`, `WEBTEST_REVIEW_MODEL`, ...). Provider key variables such as
    /// `DEEPSEEK_API_KEY` are shared. `None` when no reviewer is configured.
    pub fn reviewer_from_env() -> Option<Result<Self>> {
        Self::reviewer_from_vars(&|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
    }

    pub fn reviewer_from_vars(var: &dyn Fn(&str) -> Option<String>) -> Option<Result<Self>> {
        let mapped = |k: &str| match k.strip_prefix("WEBTEST_") {
            Some(rest) => var(&format!("WEBTEST_REVIEW_{rest}")),
            None => var(k),
        };
        let has_command = mapped("WEBTEST_LLM_COMMAND").is_some();
        if mapped("WEBTEST_LLM_PROVIDER").is_none() && !has_command {
            return None;
        }
        let with_default_provider = |k: &str| {
            if k == "WEBTEST_LLM_PROVIDER" && has_command {
                mapped(k).or_else(|| Some("command".into()))
            } else {
                mapped(k)
            }
        };
        Some(Self::from_vars(&with_default_provider, None))
    }

    /// A [`Provider::Command`] backend for `command` (e.g. a CLI flag).
    pub fn command(command: &str) -> Self {
        Settings {
            provider: Provider::Command,
            model: "external".into(),
            base_url: String::new(),
            api_key: None,
            auth: Auth::ApiKey,
            timeout: Duration::from_secs(600),
            command: Some(command.to_string()),
        }
    }

    /// Builds the backend, wrapped with a per-call timeout and retries.
    pub fn build(&self) -> Box<dyn Llm> {
        let inner: Box<dyn Llm> = match self.provider {
            Provider::Anthropic => Box::new(AnthropicApi::new(self)),
            Provider::OpenAi => Box::new(OpenAiCompat::new(self)),
            Provider::ClaudeCli => Box::new(ClaudeCli::new(self.model.clone())),
            Provider::Command => Box::new(CommandLlm {
                command: self.command.clone().unwrap_or_default(),
            }),
        };
        Box::new(Resilient {
            inner,
            timeout: self.timeout,
            retries: 2,
        })
    }
}

/// Backend from the environment (see [`Settings::from_env`]).
pub fn from_env(model: Option<String>) -> Result<Box<dyn Llm>> {
    Ok(Settings::from_env(model)?.build())
}

/// Bounds every call with a timeout and retries failures. Dropping a timed-out
/// call kills the `claude -p` child (`kill_on_drop`).
pub struct Resilient {
    inner: Box<dyn Llm>,
    timeout: Duration,
    retries: usize,
}

#[async_trait]
impl Llm for Resilient {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let mut last = None;
        for attempt in 0..=self.retries {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
            }
            match tokio::time::timeout(self.timeout, self.inner.complete(system, user)).await {
                Ok(Ok(text)) => return Ok(text),
                Ok(Err(e)) => {
                    tracing::warn!("LLM call failed (attempt {}): {e:#}", attempt + 1);
                    last = Some(e);
                }
                Err(_) => {
                    tracing::warn!(
                        "LLM call timed out after {:?} (attempt {})",
                        self.timeout,
                        attempt + 1
                    );
                    last = Some(anyhow::anyhow!(
                        "LLM call timed out after {:?}",
                        self.timeout
                    ));
                }
            }
        }
        Err(last.expect("at least one attempt"))
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }
}

pub struct AnthropicApi {
    http: reqwest::Client,
    url: String,
    key: String,
    auth: Auth,
    model: String,
    max_tokens: u32,
}

impl AnthropicApi {
    pub fn new(s: &Settings) -> Self {
        Self {
            http: reqwest::Client::new(),
            url: format!("{}/v1/messages", s.base_url),
            key: s.api_key.clone().unwrap_or_default(),
            auth: s.auth,
            model: s.model.clone(),
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
        let req = self
            .http
            .post(&self.url)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        let req = match self.auth {
            Auth::ApiKey => req.header("x-api-key", &self.key),
            Auth::Bearer => req.bearer_auth(&self.key),
        };
        let resp = req
            .send()
            .await
            .with_context(|| format!("request to {} failed", self.url))?;
        let status = resp.status();
        let v: Value = resp
            .json()
            .await
            .with_context(|| format!("invalid response from {}", self.url))?;
        if !status.is_success() {
            bail!("{} returned {status}: {v}", self.url);
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
        format!("anthropic-messages ({} @ {})", self.model, self.url)
    }
}

/// OpenAI Chat Completions protocol (DeepSeek, Qwen, vLLM, Ollama, ...).
pub struct OpenAiCompat {
    http: reqwest::Client,
    url: String,
    key: String,
    model: String,
    max_tokens: u32,
}

impl OpenAiCompat {
    pub fn new(s: &Settings) -> Self {
        Self {
            http: reqwest::Client::new(),
            url: format!("{}/chat/completions", s.base_url),
            key: s.api_key.clone().unwrap_or_default(),
            model: s.model.clone(),
            max_tokens: 4096,
        }
    }
}

#[async_trait]
impl Llm for OpenAiCompat {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("request to {} failed", self.url))?;
        let status = resp.status();
        let v: Value = resp
            .json()
            .await
            .with_context(|| format!("invalid response from {}", self.url))?;
        if !status.is_success() {
            bail!("{} returned {status}: {v}", self.url);
        }
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("no message content in response: {v}"))
    }

    fn describe(&self) -> String {
        format!("openai-chat ({} @ {})", self.model, self.url)
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

/// Runs a shell command per call: system + user prompt on stdin, answer on stdout.
pub struct CommandLlm {
    command: String,
}

#[async_trait]
impl Llm for CommandLlm {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", &self.command])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to run `{}`", self.command))?;
        let mut stdin = child.stdin.take().expect("stdin piped");
        // A command that exits without reading the whole prompt closes the pipe; report its exit
        // status and stderr below instead of a bare "Broken pipe".
        if let Err(e) = stdin
            .write_all(format!("{system}\n\n{user}").as_bytes())
            .await
            && e.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(e.into());
        }
        drop(stdin);
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!(
                "`{}` exited with {}: {}",
                self.command,
                out.status,
                err.chars()
                    .rev()
                    .take(500)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn describe(&self) -> String {
        format!("command ({})", self.command)
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

    struct Flaky {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Llm for Flaky {
        async fn complete(&self, _: &str, _: &str) -> Result<String> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                // First call hangs past the timeout.
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Ok("ok".into())
        }
        fn describe(&self) -> String {
            "flaky".into()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_after_timeout() {
        let llm = Resilient {
            inner: Box::new(Flaky { calls: 0.into() }),
            timeout: Duration::from_secs(5),
            retries: 1,
        };
        assert_eq!(llm.complete("s", "u").await.unwrap(), "ok");
    }

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn defaults_to_claude_cli_without_keys() {
        let s = Settings::from_vars(&vars(&[]), None).unwrap();
        assert_eq!(s.provider, Provider::ClaudeCli);
        assert_eq!(s.model, DEFAULT_MODEL);
    }

    #[test]
    fn anthropic_compatible_base_url_and_bearer_token() {
        let s = Settings::from_vars(
            &vars(&[
                ("ANTHROPIC_AUTH_TOKEN", "t"),
                ("ANTHROPIC_BASE_URL", "https://api.deepseek.com/anthropic/"),
                ("WEBTEST_MODEL", "deepseek-chat"),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(s.provider, Provider::Anthropic);
        assert_eq!(s.auth, Auth::Bearer);
        assert_eq!(s.base_url, "https://api.deepseek.com/anthropic");
        assert_eq!(s.model, "deepseek-chat");
    }

    #[test]
    fn deepseek_preset_and_overrides() {
        let s = Settings::from_vars(
            &vars(&[
                ("WEBTEST_LLM_PROVIDER", "DeepSeek"),
                ("DEEPSEEK_API_KEY", "k"),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(
            (s.provider, s.base_url.as_str(), s.model.as_str()),
            (
                Provider::OpenAi,
                "https://api.deepseek.com",
                "deepseek-chat"
            )
        );
        let s = Settings::from_vars(
            &vars(&[
                ("WEBTEST_LLM_PROVIDER", "openai"),
                ("WEBTEST_LLM_BASE_URL", "http://localhost:11434/v1"),
                ("WEBTEST_LLM_API_KEY", "x"),
            ]),
            Some("qwen3".into()),
        )
        .unwrap();
        assert_eq!(
            (s.base_url.as_str(), s.model.as_str()),
            ("http://localhost:11434/v1", "qwen3")
        );
    }

    #[test]
    fn reviewer_reads_prefixed_twins_and_command_implies_provider() {
        assert!(
            Settings::reviewer_from_vars(&vars(&[("WEBTEST_LLM_PROVIDER", "deepseek")])).is_none()
        );
        let s =
            Settings::reviewer_from_vars(&vars(&[("WEBTEST_REVIEW_LLM_COMMAND", "node x.mjs")]))
                .unwrap()
                .unwrap();
        assert_eq!(s.provider, Provider::Command);
        assert_eq!(s.command.as_deref(), Some("node x.mjs"));
        assert_eq!(s.timeout, Duration::from_secs(600));
        let s = Settings::reviewer_from_vars(&vars(&[
            ("WEBTEST_REVIEW_LLM_PROVIDER", "deepseek"),
            ("DEEPSEEK_API_KEY", "k"),
            ("WEBTEST_LLM_PROVIDER", "claude-cli"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            (s.provider, s.model.as_str()),
            (Provider::OpenAi, "deepseek-chat")
        );
    }

    #[tokio::test]
    async fn command_backend_pipes_prompt_and_reports_failures() {
        let ok = Settings::command("tr a-z A-Z").build();
        assert_eq!(ok.complete("sys", "user").await.unwrap(), "SYS\n\nUSER");
        let bad = CommandLlm {
            command: "echo boom >&2; exit 3".into(),
        };
        let e = bad.complete("s", "u").await.unwrap_err().to_string();
        assert!(e.contains("boom") && e.contains("3"), "{e}");
        // Exits without reading a prompt larger than the pipe buffer: always a broken pipe.
        let big = "x".repeat(1 << 20);
        let e = bad.complete("s", &big).await.unwrap_err().to_string();
        assert!(e.contains("boom") && e.contains("3"), "{e}");
    }

    #[test]
    fn reports_missing_model_key_or_provider() {
        let e = Settings::from_vars(
            &vars(&[("WEBTEST_LLM_PROVIDER", "openai"), ("OPENAI_API_KEY", "k")]),
            None,
        );
        assert!(e.unwrap_err().to_string().contains("needs a model"));
        let e = Settings::from_vars(&vars(&[("WEBTEST_LLM_PROVIDER", "deepseek")]), None);
        assert!(e.unwrap_err().to_string().contains("needs an API key"));
        let e = Settings::from_vars(&vars(&[("WEBTEST_LLM_PROVIDER", "gemini")]), None);
        assert!(e.unwrap_err().to_string().contains("unknown"));
    }

    /// One-shot HTTP server: returns the raw request it received.
    async fn mock_server(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                response.len()
            );
            sock.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf).to_string()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn openai_protocol_on_the_wire() {
        let (url, req) =
            mock_server(r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}]}"#).await;
        let s = Settings::from_vars(
            &vars(&[
                ("WEBTEST_LLM_PROVIDER", "deepseek"),
                ("DEEPSEEK_API_KEY", "sk-1"),
                ("WEBTEST_LLM_BASE_URL", &url),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(
            OpenAiCompat::new(&s).complete("SYS", "USER").await.unwrap(),
            "hi"
        );
        let req = req.await.unwrap().to_lowercase();
        assert!(req.starts_with("post /chat/completions "), "{req}");
        assert!(req.contains("authorization: bearer sk-1"));
        let body: Value = serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body["messages"][0],
            json!({"role": "system", "content": "sys"})
        );
        assert_eq!(body["model"], "deepseek-chat");
    }

    #[tokio::test]
    async fn anthropic_protocol_on_the_wire() {
        let (url, req) = mock_server(
            r#"{"content":[{"type":"text","text":"he"},{"type":"text","text":"llo"}]}"#,
        )
        .await;
        let base = format!("{url}/anthropic");
        let s = Settings::from_vars(
            &vars(&[("ANTHROPIC_API_KEY", "k-2"), ("ANTHROPIC_BASE_URL", &base)]),
            None,
        )
        .unwrap();
        assert_eq!(
            AnthropicApi::new(&s).complete("SYS", "USER").await.unwrap(),
            "hello"
        );
        let req = req.await.unwrap().to_lowercase();
        assert!(req.starts_with("post /anthropic/v1/messages "), "{req}");
        assert!(req.contains("x-api-key: k-2") && req.contains("anthropic-version: 2023-06-01"));
        assert!(req.contains(r#""system":"sys""#));
    }

    #[test]
    fn errors_without_json() {
        assert!(extract_json("no json here").is_err());
    }
}
