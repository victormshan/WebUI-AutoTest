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

use crate::model::Channel;

/// An external model's answer and where it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub provider: String,
    pub model: Option<String>,
    pub family: String,
    pub channel: Channel,
    pub answer: String,
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
    /// (base URL, API key, model)
    pub openai: Option<(String, String, String)>,
    pub bridge: String,
    pub web_attempts: u32,
    pub web_timeout: Duration,
    pub retry_pause: Duration,
    pub poll: Duration,
}

impl ReviewerConfig {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let openai = match (
            var("REVIEW_GATE_BASE_URL"),
            var("REVIEW_GATE_API_KEY"),
            var("REVIEW_GATE_MODEL"),
        ) {
            (Some(b), Some(k), Some(m)) => Some((b, k, m)),
            _ => var("DEEPSEEK_API_KEY").map(|k| {
                (
                    "https://api.deepseek.com".into(),
                    k,
                    var("REVIEW_GATE_MODEL").unwrap_or("deepseek-chat".into()),
                )
            }),
        };
        Self {
            gemini_api_key: var("GEMINI_API_KEY"),
            gemini_model: var("REVIEW_GATE_GEMINI_MODEL").unwrap_or("gemini-2.5-flash".into()),
            openai,
            bridge: var("DSH_RELAY_BRIDGE")
                .unwrap_or("http://localhost:8899".into())
                .trim_end_matches('/')
                .into(),
            web_attempts: 4,
            web_timeout: Duration::from_secs(420), // beyond the bridge's length-scaled processing timeout (≤ ~370s)
            retry_pause: Duration::from_secs(5),
            poll: Duration::from_secs(3),
        }
    }
}

pub const PROVIDERS: &[&str] = &["gemini-api", "openai", "web-gemini"];

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
pub async fn ask(cfg: &ReviewerConfig, prompt: &str, provider: &str) -> Result<Reply, AskError> {
    let names: Vec<&str> = if provider == "auto" {
        PROVIDERS.to_vec()
    } else {
        vec![provider]
    };
    let mut errors = Vec::new();
    let mut any_available = false;
    for name in names {
        if !PROVIDERS.contains(&name) {
            return Err(AskError::Failed(format!("unknown provider {name}")));
        }
        if !available(cfg, name).await {
            errors.push(format!("{name}: unavailable"));
            continue;
        }
        any_available = true;
        let res = match name {
            "gemini-api" => gemini_api(cfg, prompt).await,
            "openai" => openai(cfg, prompt).await,
            _ => web_gemini(cfg, prompt).await,
        };
        match res {
            Ok(r) => return Ok(r),
            Err(e) => {
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

async fn gemini_api(cfg: &ReviewerConfig, prompt: &str) -> anyhow::Result<Reply> {
    let key = cfg.gemini_api_key.as_deref().unwrap_or_default();
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={key}",
        cfg.gemini_model
    );
    let resp = reqwest::Client::new()
        .post(url)
        .timeout(Duration::from_secs(180))
        .json(&json!({ "contents": [{ "role": "user", "parts": [{ "text": prompt }] }] }))
        .send()
        .await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        anyhow::bail!(
            "gemini-api {status}: {}",
            v.to_string().chars().take(300).collect::<String>()
        );
    }
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
    })
}

async fn openai(cfg: &ReviewerConfig, prompt: &str) -> anyhow::Result<Reply> {
    use llm::Llm;
    let (base, key, model) = cfg.openai.clone().expect("checked by available()");
    let family = if format!("{base}{model}").to_lowercase().contains("deepseek") {
        "deepseek"
    } else {
        "openai-compatible"
    };
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
        family: family.into(),
        channel: Channel::ExternalApi,
        answer,
    })
}

// ---------------------------------------------------------------- web-gemini

/// How to reach the bridge: directly, or through Windows `curl.exe` (WSL → Windows loopback).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Transport {
    Native,
    CurlExe,
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
                    let mut child = Command::new("curl.exe")
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

async fn web_gemini(cfg: &ReviewerConfig, prompt: &str) -> anyhow::Result<Reply> {
    let bridge = Bridge::connect(&cfg.bridge)
        .await
        .ok_or_else(|| anyhow::anyhow!("bridge unreachable"))?;
    let mut last = None;
    for attempt in 1..=cfg.web_attempts {
        match web_gemini_once(&bridge, cfg, prompt).await {
            Ok(answer) => {
                return Ok(Reply {
                    provider: "web-gemini".into(),
                    model: Some("gemini (web)".into()),
                    family: "gemini".into(),
                    channel: Channel::WebGemini,
                    answer,
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
    let body = json!({ "prompt": prompt }).to_string();
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
    /// `Err(msg)` = failed. Returns (base URL, prompts received).
    pub(crate) async fn mock_bridge(
        answers: Vec<Result<&'static str, &'static str>>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
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
                        let mut s = seen.lock().unwrap();
                        s.push(serde_json::from_str::<Value>(&body).unwrap()["prompt"].as_str().unwrap().to_string());
                        json!({ "ok": true, "id": format!("t{}", s.len()) })
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
            openai: None,
            bridge: bridge.into(),
            web_attempts: 4,
            web_timeout: Duration::from_secs(5),
            retry_pause: Duration::from_millis(10),
            poll: Duration::from_millis(10),
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
        let r = ask(&cfg(&url), "审核这个", "auto").await.unwrap();
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
        let e = ask(&cfg(&url), "p", "web-gemini").await.unwrap_err();
        assert!(matches!(e, AskError::Failed(_)), "{e}");
        assert_eq!(prompts.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn nothing_configured_is_unavailable() {
        let mut c = cfg("http://127.0.0.1:9");
        c.web_attempts = 1;
        // curl.exe may exist (WSL) but cannot reach a closed port either
        assert!(matches!(
            ask(&c, "p", "auto").await,
            Err(AskError::Unavailable(_))
        ));
        assert!(probe(&c).await.iter().all(|(_, ok)| !ok));
    }
}
