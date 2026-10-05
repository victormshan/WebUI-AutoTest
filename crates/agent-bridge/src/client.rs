//! HTTP client for agent-bridge, used by the CLI and the MCP server.
//!
//! Retries follow the agreed client contract (design §8): 4xx is final (the request is wrong and
//! will stay wrong); network errors and 5xx are retried a few times with the same `clientMsgId`,
//! so a retried post can never create a second message.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::PROTOCOL;

pub struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
}

const RETRIES: u32 = 3;

/// A fresh idempotency key: time + randomness from the OS (no extra dependency).
pub fn new_msg_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("c-{:016x}{:08x}", h.finish(), std::process::id())
}

impl Client {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// `AGENT_BRIDGE_URL` (default http://127.0.0.1:7879); token from `AGENT_BRIDGE_TOKEN`,
    /// else the file `AGENT_BRIDGE_TOKEN_FILE`, else `~/.config/agent-bridge/token`.
    pub fn from_env() -> Result<Self> {
        let url =
            std::env::var("AGENT_BRIDGE_URL").unwrap_or_else(|_| "http://127.0.0.1:7879".into());
        if let Ok(t) = std::env::var("AGENT_BRIDGE_TOKEN") {
            return Ok(Self::new(&url, &t));
        }
        let file = std::env::var("AGENT_BRIDGE_TOKEN_FILE").unwrap_or_else(|_| {
            format!(
                "{}/.config/agent-bridge/token",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        let t = std::fs::read_to_string(&file).with_context(|| {
            format!("no agent-bridge token: set AGENT_BRIDGE_TOKEN or create {file}")
        })?;
        Ok(Self::new(&url, &t))
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let mut last = None;
        for attempt in 0..RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(300 * u64::from(attempt))).await;
            }
            let mut req = self
                .http
                .request(method.clone(), format!("{}{path}", self.base))
                .bearer_auth(&self.token)
                .timeout(timeout);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    last = Some(anyhow::anyhow!(
                        "agent-bridge unreachable at {}: {e}",
                        self.base
                    ));
                    continue;
                }
            };
            let status = resp.status();
            let bytes = resp.bytes().await.unwrap_or_default();
            let v: Value = serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({ "error": String::from_utf8_lossy(&bytes) }));
            if status.is_success() {
                return Ok(v);
            }
            let msg = anyhow::anyhow!(
                "agent-bridge {status}: {}",
                v["error"].as_str().unwrap_or(&v.to_string())
            );
            if status.is_client_error() {
                return Err(msg);
            }
            last = Some(msg);
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("agent-bridge: no attempt made")))
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::GET, path, None, Duration::from_secs(30))
            .await
    }

    /// Posts with a `clientMsgId` (added if missing) and the protocol, so retries are idempotent.
    pub async fn post(&self, path: &str, mut body: Value) -> Result<Value> {
        if body.get("protocol").is_none() {
            body["protocol"] = json!(PROTOCOL);
        }
        if path != "/v1/inbox/ack" && body.get("client_msg_id").is_none() {
            body["client_msg_id"] = json!(new_msg_id());
        }
        self.call(
            reqwest::Method::POST,
            path,
            Some(&body),
            Duration::from_secs(30),
        )
        .await
    }

    /// One long poll (≤ `wait` seconds). The HTTP timeout is longer than the wait.
    pub async fn inbox(&self, wait: u64) -> Result<Vec<Value>> {
        let v = self
            .call(
                reqwest::Method::GET,
                &format!("/v1/inbox?wait={wait}"),
                None,
                Duration::from_secs(wait + 15),
            )
            .await?;
        Ok(v["messages"].as_array().cloned().unwrap_or_default())
    }

    /// Blocks until at least one message is waiting (or `timeout` passes, returning empty).
    pub async fn wait(&self, timeout: Duration, per_poll: u64) -> Result<Vec<Value>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let left = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_secs();
            if left == 0 {
                return Ok(vec![]);
            }
            match self.inbox(per_poll.min(left)).await {
                Ok(m) if !m.is_empty() => return Ok(m),
                Ok(_) => {}
                // The service may be restarting: keep waiting rather than give up the watch.
                Err(e) if e.to_string().contains("unreachable") => {
                    tokio::time::sleep(Duration::from_secs(5)).await
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn ack(&self, task: &str, n: u32) -> Result<Value> {
        self.post("/v1/inbox/ack", json!({ "task": task, "n": n }))
            .await
    }

    pub async fn health(&self) -> Result<Value> {
        let v = self.get("/v1/health").await?;
        if v["service"] != "agent-bridge" {
            bail!("not an agent-bridge at {}", self.base);
        }
        Ok(v)
    }
}
