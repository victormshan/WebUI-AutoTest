//! HTTP client for the review-gate service (used by the CLI as the implementer).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::model::Task;
use crate::service::Job;
use crate::{attest, git};

pub struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// `REVIEW_GATE_URL` (default http://127.0.0.1:7878) and the token from `REVIEW_GATE_TOKEN`,
    /// else `REVIEW_GATE_TOKEN_FILE`, else /etc/review-gate/client.token, else ~/.config/review-gate/token.
    pub fn from_env() -> Result<Self> {
        let url =
            std::env::var("REVIEW_GATE_URL").unwrap_or_else(|_| "http://127.0.0.1:7878".into());
        if let Ok(t) = std::env::var("REVIEW_GATE_TOKEN") {
            return Ok(Self::new(&url, &t));
        }
        let home = std::env::var("HOME").unwrap_or_default();
        let candidates = [
            std::env::var("REVIEW_GATE_TOKEN_FILE").ok(),
            Some("/etc/review-gate/client.token".into()),
            Some(format!("{home}/.config/review-gate/token")),
        ];
        for p in candidates.into_iter().flatten() {
            if let Ok(t) = std::fs::read_to_string(&p) {
                return Ok(Self::new(&url, &t));
            }
        }
        bail!("no review-gate token: set REVIEW_GATE_TOKEN or REVIEW_GATE_TOKEN_FILE")
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("review-gate unreachable at {}", self.base))?;
        let status = resp.status();
        let v: Value = resp
            .json()
            .await
            .context("invalid response from review-gate")?;
        if !status.is_success() {
            bail!(
                "review-gate {status}: {}",
                v["error"].as_str().unwrap_or(&v.to_string())
            );
        }
        Ok(serde_json::from_value(v)?)
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.call(reqwest::Method::GET, path, None).await
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    /// Starts an external review of the staged tree and waits for the verdict.
    pub async fn review(
        &self,
        task: &str,
        tree: &str,
        base: &str,
        evidence: &str,
        provider: Option<&str>,
        poll: Duration,
    ) -> Result<Job> {
        let job: Job = self
            .post(
                &format!("/tasks/{task}/reviews"),
                json!({ "tree": tree, "base": base, "evidence": evidence, "provider": provider }),
            )
            .await?;
        loop {
            let j: Job = self.get(&format!("/jobs/{}", job.id)).await?;
            if j.status != "running" {
                return Ok(j);
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Stages the task's repository locally (the implementer owns it) and returns (tree, base).
    pub async fn stage(&self, task: &str) -> Result<(String, String)> {
        let t: Task = self.get(&format!("/tasks/{task}")).await?;
        Ok(git::stage_all(std::path::Path::new(&t.repo))?)
    }

    /// Applies a review; for an approved version also attaches the gate's signed attestation to
    /// the commit as a git note (`refs/notes/review-gate`), which CI verifies.
    pub async fn record(
        &self,
        task: &str,
        review: &str,
        commit: Option<&str>,
        tag: Option<&str>,
    ) -> Result<Value> {
        let mut v: Value = self
            .post(
                &format!("/tasks/{task}/record"),
                json!({ "review": review, "commit": commit, "tag": tag }),
            )
            .await?;
        let committed = v["task"]["history"]
            .as_array()
            .and_then(|h| h.last())
            .and_then(|h| h["commit"].as_str())
            .map(str::to_string);
        if let (Some(note), Some(commit)) =
            (v["attestation"].as_str().map(str::to_string), committed)
        {
            let repo = v["task"]["repo"].as_str().unwrap_or(".").to_string();
            let notes_ref = format!("--ref={}", attest::NOTES_REF);
            git::git(
                std::path::Path::new(&repo),
                &["notes", &notes_ref, "add", "-f", "-m", &note, &commit],
            )
            .context("attaching the attestation note")?;
            v["note"] = json!(format!(
                "attestation attached to {commit} as a git note; push it with: git push origin {}",
                attest::NOTES_REF
            ));
        }
        Ok(v)
    }
}
