//! External reviewers: models from a vendor other than the implementer.
//!
//! Tried in order (`auto`), skipping unavailable ones:
//! - `gemini-api`  Google Gemini REST (`GEMINI_API_KEY`, `REVIEW_GATE_GEMINI_MODEL`)
//! - `openai`      OpenAI-compatible chat API (`REVIEW_GATE_BASE_URL` + `REVIEW_GATE_API_KEY` +
//!   `REVIEW_GATE_MODEL`, or `DEEPSEEK_API_KEY` for DeepSeek), via webtest's `llm` crate
//! - `web-gemini`  dsh-web-relay bridge → Chrome extension → gemini.google.com (`DSH_RELAY_BRIDGE`,
//!   default `http://localhost:8899`). From WSL, where the bridge's Windows loopback is not
//!   reachable, requests go through Windows `curl.exe`.

use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::identity::canonical_family;
use crate::model::{Channel, external_family_ok};

/// An external model's answer and where it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub provider: String,
    pub model: Option<String>,
    pub family: String,
    pub channel: Channel,
    pub answer: String,
    /// Providers tried before this one in `auto`, with why they failed: a fallback to a weaker
    /// channel must be visible, never silent.
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AskError {
    /// No provider is configured or reachable.
    #[error("no external AI available ({0})")]
    Unavailable(String),
    #[error("external AI failed ({0})")]
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct ReviewerConfig {
    pub gemini_api_key: Option<String>,
    pub gemini_model: String,
    /// Gemini REST endpoint (overridable for tests and proxies).
    pub gemini_base_url: String,
    /// (base URL, API key, model)
    pub openai: Option<(String, String, String)>,
    /// The model vendor behind the OpenAI-compatible endpoint, as configured by the
    /// administrator (`REVIEW_GATE_OPENAI_FAMILY`). `None` means unknown: such an endpoint may
    /// not act as an independent external reviewer. Protocol names are never used as vendors.
    pub openai_family: Option<String>,
    pub bridge: String,
    pub web_attempts: u32,
    pub web_timeout: Duration,
    pub retry_pause: Duration,
    pub poll: Duration,
    /// Waits before retrying an API call that answered 503 (overloaded) or 429 (rate limited);
    /// one retry per entry. Other errors are not retried.
    pub api_retry_pauses: Vec<Duration>,
}

impl ReviewerConfig {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let configured_family = var("REVIEW_GATE_OPENAI_FAMILY");
        let (openai, openai_family) = match (
            var("REVIEW_GATE_BASE_URL"),
            var("REVIEW_GATE_API_KEY"),
            var("REVIEW_GATE_MODEL"),
        ) {
            (Some(b), Some(k), Some(m)) => {
                // Only an official endpoint gets a default, and it is the vendor's own name.
                // Any other base URL stays unknown unless the administrator names the vendor.
                let family = configured_family.or_else(|| official_family(&b));
                (Some((b, k, m)), family)
            }
            _ => match var("DEEPSEEK_API_KEY") {
                Some(k) => (
                    Some((
                        "https://api.deepseek.com".into(),
                        k,
                        var("REVIEW_GATE_MODEL").unwrap_or("deepseek-chat".into()),
                    )),
                    Some("deepseek".into()),
                ),
                None => (None, None),
            },
        };
        Self {
            gemini_api_key: var("GEMINI_API_KEY"),
            // gemini-2.5-flash answers 404 for keys created since late 2026.
            gemini_model: var("REVIEW_GATE_GEMINI_MODEL").unwrap_or("gemini-3.8-flash".into()),
            gemini_base_url: var("REVIEW_GATE_GEMINI_BASE_URL")
                .unwrap_or("https://generativelanguage.googleapis.com".into()),
            openai,
            openai_family,
            bridge: var("DSH_RELAY_BRIDGE")
                .unwrap_or("http://localhost:8899".into())
                .trim_end_matches('/')
                .into(),
            web_attempts: 4,
            web_timeout: Duration::from_secs(420), // beyond the bridge's length-scaled processing timeout (≤ ~370s)
            retry_pause: Duration::from_secs(5),
            poll: Duration::from_secs(3),
            api_retry_pauses: var("REVIEW_GATE_API_RETRY_PAUSES")
                .map(|v| {
                    v.split(',')
                        .filter_map(|x| x.trim().parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .collect()
                })
                .unwrap_or_else(|| vec![Duration::from_secs(15), Duration::from_secs(45)]),
        }
    }
}

pub const PROVIDERS: &[&str] = &["gemini-api", "openai", "web-gemini"];

/// The vendor of a known official endpoint, derived from the parsed URL (never from substrings:
/// `api.openai.com.evil.example` or `/x/api.openai.com` must not count). Only HTTPS.
fn official_family(base: &str) -> Option<String> {
    let url = reqwest::Url::parse(base).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    match url.host_str()? {
        "api.openai.com" => Some("openai".into()),
        "api.deepseek.com" => Some("deepseek".into()),
        _ => None,
    }
}

/// The canonical family a provider is known to call, before any network request.
/// `None` means the service configuration does not establish a vendor.
pub fn provider_family(cfg: &ReviewerConfig, provider: &str) -> Option<&'static str> {
    match provider {
        "gemini-api" | "web-gemini" => Some("gemini"),
        "openai" => cfg.openai_family.as_deref().and_then(canonical_family),
        _ => None,
    }
}

/// Which providers are usable right now.
pub async fn probe(cfg: &ReviewerConfig) -> Vec<(&'static str, bool)> {
    let mut out = Vec::new();
    for p in PROVIDERS {
        out.push((*p, available(cfg, p).await));
    }
    out
}

async fn available(cfg: &ReviewerConfig, provider: &str) -> bool {
    match provider {
        "gemini-api" => cfg.gemini_api_key.is_some(),
        "openai" => cfg.openai.is_some(),
        "web-gemini" => Bridge::connect(&cfg.bridge).await.is_some(),
        _ => false,
    }
}

/// Asks `provider` (or the first available one for `auto`).
///
/// `implementer_family` is the family frozen on the task. Providers from that family are never
/// contacted: `auto` skips them silently-but-visibly (the reason lands in the error/fallback
/// list), and an explicit request is refused with a clear error. A provider whose vendor is
/// unknown (an unconfigured OpenAI-compatible endpoint) is refused the same way.
pub async fn ask(
    cfg: &ReviewerConfig,
    prompt: &str,
    provider: &str,
    implementer_family: &str,
) -> Result<Reply, AskError> {
    let names: Vec<&str> = if provider == "auto" {
        PROVIDERS.to_vec()
    } else {
        vec![provider]
    };
    let mut errors = Vec::new();
    // Only providers that were tried and failed: an unconfigured one is not a fallback.
    let mut failed = Vec::new();
    let mut any_available = false;
    for name in names {
        if !PROVIDERS.contains(&name) {
            return Err(AskError::Failed(format!("unknown provider {name}")));
        }
        if !available(cfg, name).await {
            errors.push(format!("{name}: unavailable"));
            continue;
        }
        let Some(family) = provider_family(cfg, name) else {
            let why = format!(
                "{name}: the model family of this endpoint is not configured; an unknown vendor cannot be an independent reviewer"
            );
            if provider != "auto" {
                return Err(AskError::Failed(why));
            }
            errors.push(why);
            continue;
        };
        if let Err(e) = external_family_ok(implementer_family, family) {
            let why = format!("{name}: {e}");
            if provider != "auto" {
                return Err(AskError::Failed(why));
            }
            errors.push(why);
            continue;
        }
        any_available = true;
        let res = match name {
            "gemini-api" => gemini_api(cfg, prompt, implementer_family).await,
            "openai" => openai(cfg, prompt, implementer_family).await,
            _ => web_gemini(cfg, prompt, implementer_family).await,
        };
        match res {
            Ok(mut r) => {
                r.fallbacks = failed;
                return Ok(r);
            }
            Err(e) => {
                eprintln!("[review-gate] provider {name} failed: {e:#}");
                failed.push(format!("{name}: {e:#}"));
                errors.push(format!("{name}: {e:#}"));
                if provider != "auto" {
                    break;
                }
            }
        }
    }
    let msg = errors.join("; ");
    Err(if any_available {
        AskError::Failed(msg)
    } else {
        AskError::Unavailable(msg)
    })
}

/// Re-checks the family rule at every retry, not only when the provider was selected.
fn check_retry_family(
    provider: &str,
    family: &str,
    implementer_family: &str,
) -> anyhow::Result<()> {
    external_family_ok(implementer_family, family)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{provider}: {e}"))
}

async fn gemini_api(
    cfg: &ReviewerConfig,
    prompt: &str,
    implementer_family: &str,
) -> anyhow::Result<Reply> {
    let key = cfg.gemini_api_key.as_deref().unwrap_or_default();
    // The key goes in a header, never the URL: a reqwest error prints its URL, and that error
    // ends up in logs and in the review record.
    let url = format!(
        "{}/v1beta/models/{}:generateContent",
        cfg.gemini_base_url.trim_end_matches('/'),
        cfg.gemini_model
    );
    let body = json!({ "contents": [{ "role": "user", "parts": [{ "text": prompt }] }] });
    let mut attempt = 0;
    let v: Value = loop {
        // Every retry revalidates: a family change or a misconfiguration cannot slip through.
        check_retry_family("gemini-api", "gemini", implementer_family)?;
        let resp = reqwest::Client::new()
            .post(&url)
            .header("x-goog-api-key", key)
            .timeout(Duration::from_secs(180))
            .json(&body)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(reqwest::Error::without_url)?;
        if status.is_success() {
            break v;
        }
        let transient = status.as_u16() == 503 || status.as_u16() == 429;
        if transient && let Some(pause) = cfg.api_retry_pauses.get(attempt) {
            eprintln!(
                "[review-gate] gemini-api {status}, retrying in {}s",
                pause.as_secs()
            );
            tokio::time::sleep(*pause).await;
            attempt += 1;
            continue;
        }
        anyhow::bail!(
            "gemini-api {status} after {} attempt(s): {}",
            attempt + 1,
            v.to_string().chars().take(300).collect::<String>()
        );
    };
    let answer = v["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["text"].as_str())
        .collect::<String>();
    Ok(Reply {
        provider: "gemini-api".into(),
        model: Some(cfg.gemini_model.clone()),
        family: "gemini".into(),
        channel: Channel::ExternalApi,
        answer,
        fallbacks: Vec::new(),
    })
}

async fn openai(
    cfg: &ReviewerConfig,
    prompt: &str,
    implementer_family: &str,
) -> anyhow::Result<Reply> {
    use llm::Llm;
    let (base, key, model) = cfg.openai.clone().expect("checked by available()");
    // The vendor comes from the administrator's configuration, never from the URL or model name.
    let family = cfg
        .openai_family
        .as_deref()
        .and_then(canonical_family)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "openai: the model family of {base} is not configured (REVIEW_GATE_OPENAI_FAMILY); an unknown vendor cannot be an independent reviewer"
            )
        })?;
    check_retry_family("openai", family, implementer_family)?;
    let settings = llm::Settings {
        provider: llm::Provider::OpenAi,
        model: model.clone(),
        base_url: base.trim_end_matches('/').to_string(),
        api_key: Some(key),
        auth: llm::Auth::Bearer,
        timeout: Duration::from_secs(180),
        command: None,
    };
    let answer = llm::OpenAiCompat::new(&settings)
        .complete("You are an independent code reviewer.", prompt)
        .await?;
    Ok(Reply {
        provider: "openai".into(),
        model: Some(model),
        family: family.to_string(),
        channel: Channel::ExternalApi,
        answer,
        fallbacks: Vec::new(),
    })
}

// ---------------------------------------------------------------- web-gemini

/// How to reach the bridge: directly, or through Windows `curl.exe` (WSL → Windows loopback).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Transport {
    Native,
    CurlExe,
}

/// Windows `curl.exe`: `REVIEW_GATE_CURL_EXE`, else on PATH, else the WSL mount of System32 —
/// services (systemd) do not inherit the interactive PATH that includes `/mnt/c/...`.
fn curl_exe() -> std::path::PathBuf {
    curl_exe_from(
        std::env::var_os("REVIEW_GATE_CURL_EXE"),
        std::env::var_os("PATH"),
        std::path::Path::new("/mnt/c/Windows/System32/curl.exe"),
    )
}

fn curl_exe_from(
    explicit: Option<std::ffi::OsString>,
    path: Option<std::ffi::OsString>,
    fallback: &std::path::Path,
) -> std::path::PathBuf {
    if let Some(p) = explicit.filter(|p| !p.is_empty()) {
        return p.into();
    }
    let on_path = path
        .iter()
        .flat_map(std::env::split_paths)
        .map(|d| d.join("curl.exe"))
        .find(|p| p.is_file());
    on_path.unwrap_or_else(|| {
        if fallback.is_file() {
            fallback.to_path_buf()
        } else {
            "curl.exe".into()
        }
    })
}

struct Bridge {
    base: String,
    transport: Transport,
}

impl Bridge {
    async fn connect(base: &str) -> Option<Self> {
        for transport in [Transport::Native, Transport::CurlExe] {
            let b = Self {
                base: base.to_string(),
                transport,
            };
            if let Ok((200, body)) = b
                .http("GET", "/__token", None, None, Duration::from_secs(4))
                .await
                && serde_json::from_str::<Value>(&body).is_ok_and(|v| v["ok"] == true)
            {
                return Some(b);
            }
        }
        None
    }

    async fn http(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<String>,
        timeout: Duration,
    ) -> anyhow::Result<(u16, String)> {
        let url = format!("{}{path}", self.base);
        match self.transport {
            Transport::Native => {
                let client = reqwest::Client::new();
                let mut req = client.request(method.parse()?, &url).timeout(timeout);
                if let Some(t) = token {
                    req = req.header("x-dsh-bridge-token", t);
                }
                if let Some(b) = body {
                    req = req.header("content-type", "application/json").body(b);
                }
                let resp = req.send().await?;
                Ok((resp.status().as_u16(), resp.text().await?))
            }
            Transport::CurlExe => {
                let mut args = vec![
                    "-s".to_string(),
                    "-m".into(),
                    timeout.as_secs().max(1).to_string(),
                    "-X".into(),
                    method.into(),
                    "-w".into(),
                    "\n%{http_code}".into(),
                ];
                if let Some(t) = token {
                    args.extend(["-H".into(), format!("x-dsh-bridge-token: {t}")]);
                }
                let has_body = body.is_some();
                if has_body {
                    args.extend([
                        "-H".into(),
                        "content-type: application/json".into(),
                        "--data-binary".into(),
                        "@-".into(),
                    ]);
                }
                args.push(url);
                let out = tokio::task::spawn_blocking(move || {
                    use std::io::Write;
                    let mut child = Command::new(curl_exe())
                        .args(&args)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null())
                        .spawn()?;
                    if let (Some(b), Some(mut stdin)) = (body, child.stdin.take()) {
                        stdin.write_all(b.as_bytes())?;
                    }
                    child.wait_with_output()
                })
                .await??;
                if !out.status.success() {
                    anyhow::bail!("curl.exe exited with {}", out.status);
                }
                let text = String::from_utf8_lossy(&out.stdout).into_owned();
                let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
                Ok((code.trim().parse().unwrap_or(0), body.to_string()))
            }
        }
    }
}

async fn web_gemini(
    cfg: &ReviewerConfig,
    prompt: &str,
    implementer_family: &str,
) -> anyhow::Result<Reply> {
    let bridge = Bridge::connect(&cfg.bridge)
        .await
        .ok_or_else(|| anyhow::anyhow!("bridge unreachable"))?;
    let mut last = None;
    for attempt in 1..=cfg.web_attempts {
        check_retry_family("web-gemini", "gemini", implementer_family)?;
        match web_gemini_once(&bridge, cfg, prompt).await {
            Ok(answer) => {
                return Ok(Reply {
                    provider: "web-gemini".into(),
                    model: Some("gemini (web)".into()),
                    family: "gemini".into(),
                    channel: Channel::WebGemini,
                    answer,
                    fallbacks: Vec::new(),
                });
            }
            Err(e) => {
                eprintln!("[review-gate] web-gemini attempt {attempt} failed: {e:#}");
                last = Some(e);
                if attempt < cfg.web_attempts {
                    // A failed send can leave text in Gemini's composer (next try: INPUT_BUSY).
                    tokio::time::sleep(cfg.retry_pause).await;
                }
            }
        }
    }
    Err(last.expect("at least one attempt"))
}

async fn web_gemini_once(
    bridge: &Bridge,
    cfg: &ReviewerConfig,
    prompt: &str,
) -> anyhow::Result<String> {
    let (_, tok) = bridge
        .http("GET", "/__token", None, None, Duration::from_secs(5))
        .await?;
    let token = serde_json::from_str::<Value>(&tok)?["token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // Every review in a new conversation: the tab is shared (DSH's own tasks and probes run in
    // it), and a review appended to whatever conversation is open sees context that is not the
    // diff — the reviewer is no longer independent.
    let body = json!({ "prompt": prompt, "newConversation": true }).to_string();
    let (status, created) = bridge
        .http(
            "POST",
            "/create-task",
            Some(&token),
            Some(body),
            Duration::from_secs(15),
        )
        .await?;
    let created: Value = serde_json::from_str(&created).unwrap_or(Value::Null);
    let Some(id) = created["id"].as_str().filter(|_| created["ok"] == true) else {
        anyhow::bail!("bridge create-task failed ({status})");
    };
    let deadline = Instant::now() + cfg.web_timeout;
    while Instant::now() < deadline {
        tokio::time::sleep(cfg.poll).await;
        let (_, t) = bridge
            .http(
                "GET",
                &format!("/task-result/{id}"),
                Some(&token),
                None,
                Duration::from_secs(10),
            )
            .await?;
        let task = serde_json::from_str::<Value>(&t).unwrap_or(Value::Null)["task"].clone();
        match task["status"].as_str() {
            Some("done") => return Ok(task["answer"].as_str().unwrap_or_default().to_string()),
            Some("failed") => {
                let err: String = task["error"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect();
                anyhow::bail!("web-gemini failed: {err}");
            }
            _ => {}
        }
    }
    anyhow::bail!("web-gemini timed out after {}s", cfg.web_timeout.as_secs())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Mock dsh-web-relay bridge. `answers[i]` serves the i-th created task: `Ok(text)` = done,
    /// `Err(msg)` = failed. A create-task without `newConversation: true` is always refused.
    /// Returns (base URL, prompts received).
    pub(crate) async fn mock_bridge(
        answers: Vec<Result<&'static str, &'static str>>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        mock_bridge_refusing(0, answers).await
    }

    /// As `mock_bridge`, but the first `refused` create-task calls are refused (`ok:false`), as a
    /// bridge does when it cannot take a task; later calls create tasks served by `answers`.
    pub(crate) async fn mock_bridge_refusing(
        refused: usize,
        answers: Vec<Result<&'static str, &'static str>>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        mock_bridge_held(refused, answers, None).await
    }

    /// As `mock_bridge_refusing`; while `hold` is false every task reads as still running.
    pub(crate) async fn mock_bridge_held(
        refused: usize,
        answers: Vec<Result<&'static str, &'static str>>,
        hold: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let creates = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let prompts = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = prompts.clone();
        let answers = Arc::new(answers);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen.clone();
                let answers = answers.clone();
                let creates = creates.clone();
                let hold = hold.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head, body) = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        if let Some((h, b)) = text.split_once("\r\n\r\n") {
                            let len = h
                                .lines()
                                .find_map(|l| {
                                    l.to_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if b.len() >= len {
                                break (h.to_string(), b.to_string());
                            }
                        }
                    };
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let authed = head
                        .to_lowercase()
                        .contains(&format!("x-dsh-bridge-token: {}", "t".repeat(64)));
                    let resp = if path == "/__token" {
                        json!({ "ok": true, "token": "t".repeat(64) })
                    } else if !authed {
                        json!({ "ok": false })
                    } else if path == "/create-task" {
                        let req = serde_json::from_str::<Value>(&body).unwrap();
                        let call = creates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if req["newConversation"] != true {
                            // A review must never land in a conversation already open.
                            json!({ "ok": false, "error": "newConversation required" })
                        } else if call < refused {
                            json!({ "ok": false, "error": "bridge refused the task" })
                        } else {
                            let mut s = seen.lock().unwrap();
                            s.push(req["prompt"].as_str().unwrap().to_string());
                            json!({ "ok": true, "id": format!("t{}", s.len()) })
                        }
                    } else if hold
                        .as_ref()
                        .is_some_and(|h| !h.load(std::sync::atomic::Ordering::SeqCst))
                    {
                        json!({ "ok": true, "task": { "status": "running" } })
                    } else {
                        let n: usize = path.rsplit('t').next().unwrap().parse().unwrap();
                        match answers.get(n - 1) {
                            Some(Ok(a)) => json!({ "ok": true, "task": { "status": "done", "answer": a } }),
                            Some(Err(e)) => json!({ "ok": true, "task": { "status": "failed", "error": e } }),
                            None => json!({ "ok": true, "task": { "status": "failed", "error": "no scripted answer" } }),
                        }
                    }
                    .to_string();
                    let out = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp}",
                        resp.len()
                    );
                    let _ = sock.write_all(out.as_bytes()).await;
                });
            }
        });
        (url, prompts)
    }

    pub(crate) fn cfg(bridge: &str) -> ReviewerConfig {
        ReviewerConfig {
            gemini_api_key: None,
            gemini_model: "m".into(),
            gemini_base_url: "http://127.0.0.1:9".into(),
            openai: None,
            openai_family: None,
            bridge: bridge.into(),
            web_attempts: 4,
            web_timeout: Duration::from_secs(5),
            retry_pause: Duration::from_millis(10),
            poll: Duration::from_millis(10),
            api_retry_pauses: vec![Duration::from_millis(10), Duration::from_millis(10)],
        }
    }

    #[tokio::test]
    async fn web_gemini_retries_failed_sends_and_reports_channel() {
        let (url, prompts) = mock_bridge(vec![
            Err("INPUT_BUSY: composer"),
            Err("SEND_FAIL"),
            Ok("VERDICT: APPROVED"),
        ])
        .await;
        let r = ask(&cfg(&url), "审核这个", "auto", "claude").await.unwrap();
        assert_eq!(r.answer, "VERDICT: APPROVED");
        assert_eq!(
            (r.channel, r.provider.as_str(), r.family.as_str()),
            (Channel::WebGemini, "web-gemini", "gemini")
        );
        assert_eq!(prompts.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn gives_up_after_the_attempt_budget() {
        let (url, prompts) = mock_bridge(vec![Err("x"); 4]).await;
        let e = ask(&cfg(&url), "p", "web-gemini", "claude")
            .await
            .unwrap_err();
        assert!(matches!(e, AskError::Failed(_)), "{e}");
        assert_eq!(prompts.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn a_refused_create_task_is_a_failed_attempt_retried_not_a_panic() {
        // Refused twice, then taken: the review still completes on the third attempt.
        let (url, prompts) = mock_bridge_refusing(2, vec![Ok("VERDICT: APPROVED")]).await;
        let r = ask(&cfg(&url), "p", "web-gemini", "claude").await.unwrap();
        assert_eq!(r.answer, "VERDICT: APPROVED");
        assert_eq!(
            prompts.lock().unwrap().len(),
            1,
            "one task created, after two refusals"
        );
        // Refused every time: a failure that says so, after the attempt budget.
        let (url, prompts) = mock_bridge_refusing(4, vec![]).await;
        let e = ask(&cfg(&url), "p", "web-gemini", "claude")
            .await
            .unwrap_err();
        assert!(
            matches!(&e, AskError::Failed(m) if m.contains("create-task failed")),
            "{e}"
        );
        assert!(prompts.lock().unwrap().is_empty(), "no task was created");
    }

    #[tokio::test]
    async fn the_gemini_key_travels_in_a_header_and_never_in_a_url_or_an_error() {
        const KEY: &str = "AIzaTEST-not-a-real-key-0123456789";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(String::new()));
        let s = seen.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let n = sock.read(&mut buf).await.unwrap();
            *s.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = r#"{"candidates":[{"content":{"parts":[{"text":"VERDICT: APPROVED"}]}}]}"#;
            let out = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(out.as_bytes()).await;
        });
        let mut c = cfg("http://127.0.0.1:9");
        c.gemini_api_key = Some(KEY.into());
        c.gemini_base_url = base;
        let r = ask(&c, "p", "gemini-api", "claude").await.unwrap();
        assert_eq!(
            (r.answer.as_str(), r.channel),
            ("VERDICT: APPROVED", Channel::ExternalApi)
        );
        let req = seen.lock().unwrap().clone();
        let request_line = req.lines().next().unwrap_or_default();
        assert!(
            !request_line.contains(KEY),
            "key must not be in the URL: {request_line}"
        );
        assert!(
            req.to_lowercase()
                .contains(&format!("x-goog-api-key: {}", KEY.to_lowercase())),
            "key goes in the x-goog-api-key header"
        );
        // A failed request reports an error without the key (it lands in logs and records).
        c.gemini_base_url = "http://127.0.0.1:9".into();
        let e = ask(&c, "p", "gemini-api", "claude")
            .await
            .unwrap_err()
            .to_string();
        assert!(!e.contains(KEY), "{e}");
    }

    /// A Gemini REST stand-in answering each request with the next status: 200 carries a
    /// verdict, anything else an error body. Returns (base URL, request count).
    async fn mock_gemini(statuses: Vec<u16>) -> (String, Arc<Mutex<usize>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(0usize));
        let h = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 65536];
                let _ = sock.read(&mut buf).await;
                let i = {
                    let mut n = h.lock().unwrap();
                    *n += 1;
                    *n - 1
                };
                let status = statuses.get(i).copied().unwrap_or(500);
                let body = if status == 200 {
                    r#"{"candidates":[{"content":{"parts":[{"text":"VERDICT: APPROVED"}]}}]}"#
                        .to_string()
                } else {
                    format!(r#"{{"error":{{"code":{status},"message":"m"}}}}"#)
                };
                let out = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(out.as_bytes()).await;
            }
        });
        (base, hits)
    }

    fn gemini_cfg(base: String) -> ReviewerConfig {
        let mut c = cfg("http://127.0.0.1:9");
        c.gemini_api_key = Some("k".into());
        c.gemini_base_url = base;
        c
    }

    #[tokio::test]
    async fn an_overloaded_gemini_api_is_retried_before_anything_falls_back() {
        let (base, hits) = mock_gemini(vec![503, 503, 200]).await;
        let r = ask(&gemini_cfg(base), "p", "auto", "claude").await.unwrap();
        assert_eq!(
            (r.provider.as_str(), r.channel),
            ("gemini-api", Channel::ExternalApi)
        );
        assert!(r.fallbacks.is_empty(), "{:?}", r.fallbacks);
        assert_eq!(*hits.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn retries_are_bounded_and_say_how_many_were_made() {
        let (base, hits) = mock_gemini(vec![503, 429, 503, 200]).await;
        let e = ask(&gemini_cfg(base), "p", "gemini-api", "claude")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("503") && e.contains("after 3 attempt(s)"), "{e}");
        assert_eq!(*hits.lock().unwrap(), 3, "two retries, no more");
    }

    #[tokio::test]
    async fn other_api_errors_are_not_retried() {
        let (base, hits) = mock_gemini(vec![400, 200]).await;
        let e = ask(&gemini_cfg(base), "p", "gemini-api", "claude")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("400") && e.contains("after 1 attempt(s)"), "{e}");
        assert_eq!(*hits.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn nothing_configured_is_unavailable() {
        let mut c = cfg("http://127.0.0.1:9");
        c.web_attempts = 1;
        // curl.exe may exist (WSL) but cannot reach a closed port either
        assert!(matches!(
            ask(&c, "p", "auto", "claude").await,
            Err(AskError::Unavailable(_))
        ));
        assert!(probe(&c).await.iter().all(|(_, ok)| !ok));
    }

    #[test]
    fn curl_exe_is_found_without_the_interactive_path() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let fallback = d.path().join("System32-curl.exe");
        std::fs::write(&fallback, "").unwrap();
        let none = d.path().join("missing.exe");
        // explicit override wins
        assert_eq!(
            curl_exe_from(Some("/x/curl.exe".into()), None, &fallback),
            std::path::PathBuf::from("/x/curl.exe")
        );
        // systemd-like PATH without /mnt/c: falls back to the System32 mount
        assert_eq!(
            curl_exe_from(None, Some("/usr/bin:/bin".into()), &fallback),
            fallback
        );
        // on PATH: that one
        std::fs::write(bin.join("curl.exe"), "").unwrap();
        let path = std::env::join_paths([std::path::Path::new("/usr/bin"), &bin]).unwrap();
        assert_eq!(
            curl_exe_from(None, Some(path), &fallback),
            bin.join("curl.exe")
        );
        // nothing anywhere: bare name (spawn reports the error)
        assert_eq!(
            curl_exe_from(None, None, &none),
            std::path::PathBuf::from("curl.exe")
        );
    }
    #[test]
    fn official_endpoints_are_recognized_by_https_host_not_by_substring() {
        assert_eq!(
            official_family("https://api.openai.com").as_deref(),
            Some("openai")
        );
        assert_eq!(
            official_family("https://api.openai.com/v1/").as_deref(),
            Some("openai")
        );
        assert_eq!(
            official_family("https://api.deepseek.com/v1").as_deref(),
            Some("deepseek")
        );
        // Lookalikes, userinfo tricks, paths and plain HTTP are not an official vendor.
        for bad in [
            "https://api.openai.com.evil.example",
            "https://x/api.openai.com",
            "https://api.openai.com@evil.example",
            "http://api.openai.com",
        ] {
            assert_eq!(official_family(bad), None, "{bad}");
        }
        // The port does not change the host.
        assert_eq!(
            official_family("https://api.openai.com:8443/v1").as_deref(),
            Some("openai")
        );
        assert_eq!(
            official_family("https://api.openai.com:443/v1").as_deref(),
            Some("openai")
        );
    }

    #[tokio::test]
    async fn same_family_providers_are_skipped_before_any_request() {
        let (gemini, hits) = mock_gemini(vec![200]).await;
        let (bridge, prompts) = mock_bridge(vec![Ok("VERDICT: APPROVED")]).await;
        let mut c = cfg(&bridge);
        c.gemini_api_key = Some("k".into());
        c.gemini_base_url = gemini;
        // Implementer is Gemini: both Gemini channels are same-family.
        let e = ask(&c, "p", "auto", "gemini").await.unwrap_err();
        assert!(matches!(e, AskError::Unavailable(_)), "{e}");
        assert_eq!(*hits.lock().unwrap(), 0, "gemini-api was never contacted");
        assert!(
            prompts.lock().unwrap().is_empty(),
            "the web was never contacted"
        );
        let e = ask(&c, "p", "gemini-api", "gemini").await.unwrap_err();
        assert!(e.to_string().contains("implementer's family"), "{e}");
        assert_eq!(*hits.lock().unwrap(), 0);
        let e = ask(&c, "p", "web-gemini", "gemini").await.unwrap_err();
        assert!(e.to_string().contains("implementer's family"), "{e}");
        assert!(prompts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_endpoint_without_a_configured_vendor_is_not_an_independent_reviewer() {
        let mut c = cfg("http://127.0.0.1:9");
        c.openai = Some(("http://127.0.0.1:9".into(), "k".into(), "m".into()));
        c.openai_family = None;
        assert_eq!(provider_family(&c, "openai"), None);
        let e = ask(&c, "p", "openai", "claude").await.unwrap_err();
        assert!(e.to_string().contains("not configured"), "{e}");
        let e = ask(&c, "p", "auto", "claude").await.unwrap_err();
        assert!(matches!(e, AskError::Unavailable(_)), "{e}");
        assert!(e.to_string().contains("not configured"), "{e}");

        // A configured vendor that is the implementer's is refused too.
        c.openai_family = Some("DeepSeek".into());
        assert_eq!(provider_family(&c, "openai"), Some("deepseek"));
        let e = ask(&c, "p", "openai", "deepseek").await.unwrap_err();
        assert!(e.to_string().contains("implementer's family"), "{e}");
    }

    #[tokio::test]
    async fn a_fallback_never_lands_on_the_implementer_family() {
        let (gemini, hits) = mock_gemini(vec![200, 200]).await;
        let (bridge, prompts) = mock_bridge(vec![Ok("VERDICT: APPROVED")]).await;
        let mut c = cfg(&bridge);
        c.gemini_api_key = Some("k".into());
        c.gemini_base_url = gemini;
        // The only other-vendor channel is down; the two Gemini channels must not be tried.
        c.openai = Some(("http://127.0.0.1:9".into(), "k".into(), "m".into()));
        c.openai_family = Some("claude".into());
        let e = ask(&c, "p", "auto", "gemini").await.unwrap_err();
        assert!(matches!(e, AskError::Failed(_)), "{e}");
        let msg = e.to_string();
        assert!(msg.contains("gemini-api: reviewer family gemini"), "{msg}");
        assert!(msg.contains("web-gemini: reviewer family gemini"), "{msg}");
        assert_eq!(*hits.lock().unwrap(), 0, "no gemini-api request");
        assert!(prompts.lock().unwrap().is_empty(), "no web-gemini request");
    }
}
