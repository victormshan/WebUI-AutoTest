//! Push wake-ups for agents that keep no waiter running (the DSH main agent): a POST to the
//! agent's notify endpoint (dsh-web-relay `/dsh-web-relay/bridge/notify`, D1).
//!
//! The service lives in WSL and DSH's endpoint listens on Windows loopback, which WSL cannot reach
//! directly (NAT). So `auto` tries a direct request and falls back to Windows `curl.exe`, which
//! runs on Windows and therefore arrives from 127.0.0.1 as the endpoint requires. The bearer token
//! goes to curl.exe on stdin (`-H @-`), never on its command line.
//!
//! An HTTP 200 is not a wake: the endpoint reports `agentWoken`, and that is what is recorded.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    #[default]
    Auto,
    Native,
    CurlExe,
}

/// How to wake one agent (`notify.json`: agent → target).
#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub url: String,
    pub token_file: PathBuf,
    #[serde(default)]
    pub transport: Transport,
}

/// What a wake attempt achieved, as recorded in the wake ledger.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    pub agent_woken: bool,
    pub coalesced: bool,
    /// HTTP status, or null when the endpoint was not reached.
    pub status: Option<u16>,
    pub transport: Transport,
    pub reason: String,
}

/// Windows `curl.exe`: `AGENT_BRIDGE_CURL_EXE`, else on PATH, else the WSL mount of System32 —
/// a systemd service does not inherit the interactive PATH (same lookup as review-gate).
pub fn curl_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("AGENT_BRIDGE_CURL_EXE").filter(|p| !p.is_empty()) {
        return p.into();
    }
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            let c = d.join("curl.exe");
            if c.is_file() {
                return c;
            }
        }
    }
    let fallback = Path::new("/mnt/c/Windows/System32/curl.exe");
    if fallback.is_file() {
        fallback.to_path_buf()
    } else {
        "curl.exe".into()
    }
}

fn read_token(p: &Path) -> Result<String, String> {
    let t = std::fs::read_to_string(p).map_err(|e| format!("token file {}: {e}", p.display()))?;
    let t = t.trim().to_string();
    if t.len() < 32 {
        return Err(format!("token in {} is too short", p.display()));
    }
    Ok(t)
}

fn interpret(status: u16, body: &str, transport: Transport) -> Outcome {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let agent_woken = status == 200 && v["agentWoken"] == true;
    let coalesced = v["coalesced"] == true;
    let reason = v["reason"].as_str().map(str::to_string).unwrap_or_else(|| {
        if v.is_null() {
            format!("HTTP {status}, unreadable body")
        } else {
            format!("HTTP {status}")
        }
    });
    Outcome {
        agent_woken,
        coalesced,
        status: Some(status),
        transport,
        reason,
    }
}

async fn native(url: &str, token: &str, body: &Value) -> Result<Outcome, String> {
    let resp = reqwest::Client::new()
        .post(url)
        .bearer_auth(token)
        .json(body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("direct request failed: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok(interpret(status, &text, Transport::Native))
}

async fn curl(url: &str, token: &str, body: &Value) -> Result<Outcome, String> {
    let exe = curl_exe();
    let args = vec![
        "-s".to_string(),
        "-m".into(),
        "15".into(),
        "-X".into(),
        "POST".into(),
        // Headers from stdin: keeps the token out of the process list.
        "-H".into(),
        "@-".into(),
        "-H".into(),
        "content-type: application/json".into(),
        "--data-binary".into(),
        body.to_string(),
        "-w".into(),
        "\n%{http_code}".into(),
        url.to_string(),
    ];
    let header = format!("Authorization: Bearer {token}\n");
    let out = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut child = std::process::Command::new(&exe)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(header.as_bytes())
                .map_err(|e| e.to_string())?;
        }
        child.wait_with_output().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
    match code.trim().parse::<u16>() {
        Ok(0) | Err(_) => Err(format!(
            "curl.exe could not reach {url} (exit {:?})",
            out.status.code()
        )),
        Ok(status) => Ok(interpret(status, body, Transport::CurlExe)),
    }
}

/// Wakes the agent behind `t` about message `n` of `task`. Never fails: problems are reported
/// in the outcome so they end up in the ledger instead of disappearing.
/// `urgent` asks the endpoint to wake at once rather than coalesce (Q2).
pub async fn wake(
    t: &Target,
    task: &str,
    n: u32,
    kind: &str,
    summary: &str,
    urgent: bool,
) -> Outcome {
    let token = match read_token(&t.token_file) {
        Ok(tok) => tok,
        Err(reason) => {
            return Outcome {
                agent_woken: false,
                coalesced: false,
                status: None,
                transport: t.transport,
                reason,
            };
        }
    };
    let summary: String = summary.chars().take(480).collect();
    let body = json!({ "taskId": task, "n": n, "kind": kind, "summary": summary, "source": "agent-bridge", "urgent": urgent });
    let attempt = match t.transport {
        Transport::Native => native(&t.url, &token, &body).await,
        Transport::CurlExe => curl(&t.url, &token, &body).await,
        Transport::Auto => match native(&t.url, &token, &body).await {
            Ok(o) => Ok(o),
            Err(direct) => curl(&t.url, &token, &body)
                .await
                .map_err(|c| format!("{direct}; {c}")),
        },
    };
    attempt.unwrap_or_else(|reason| Outcome {
        agent_woken: false,
        coalesced: false,
        status: None,
        transport: t.transport,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_200_is_not_a_wake_unless_the_endpoint_says_so() {
        let o = interpret(
            200,
            r#"{"ok":true,"agentWoken":true,"coalesced":false,"reason":"woke"}"#,
            Transport::Native,
        );
        assert!(o.agent_woken && o.reason == "woke");
        let o = interpret(
            200,
            r#"{"ok":true,"agentWoken":false,"reason":"sessionId 缺失"}"#,
            Transport::Native,
        );
        assert!(!o.agent_woken && o.reason.contains("sessionId"));
        let o = interpret(
            401,
            r#"{"ok":false,"agentWoken":false,"reason":"token 未配置"}"#,
            Transport::CurlExe,
        );
        assert_eq!((o.agent_woken, o.status), (false, Some(401)));
        assert!(!interpret(200, "garbage", Transport::Native).agent_woken);
    }

    #[tokio::test]
    async fn missing_or_short_token_is_reported_not_sent() {
        let d = tempfile::tempdir().unwrap();
        let t = Target {
            url: "http://127.0.0.1:9/x".into(),
            token_file: d.path().join("none"),
            transport: Transport::Native,
        };
        let o = wake(&t, "t", 1, "task", "s", false).await;
        assert!(!o.agent_woken && o.status.is_none() && o.reason.contains("token file"));
        std::fs::write(d.path().join("short"), "abc").unwrap();
        let t = Target {
            token_file: d.path().join("short"),
            ..t
        };
        assert!(
            wake(&t, "t", 1, "task", "s", false)
                .await
                .reason
                .contains("too short")
        );
    }
}
