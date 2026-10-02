//! Minimal Chrome DevTools Protocol client for the few browser-level calls
//! chrome-devtools-mcp does not expose (e.g. HttpOnly cookies).

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

/// Sends one command to the browser target and returns its `result`.
pub async fn call(ws_url: &str, method: &str, params: Value) -> Result<Value> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .with_context(|| format!("connecting to CDP at {ws_url}"))?;
    let msg = json!({ "id": 1, "method": method, "params": params });
    ws.send(Message::text(msg.to_string())).await?;
    while let Some(frame) = ws.next().await {
        let Message::Text(text) = frame? else {
            continue;
        };
        let v: Value = serde_json::from_str(&text)?;
        // Skip events; wait for the reply to our id.
        if v["id"] != 1 {
            continue;
        }
        let _ = ws.close(None).await;
        if let Some(err) = v.get("error") {
            bail!("CDP {method} failed: {err}");
        }
        return Ok(v["result"].clone());
    }
    bail!("CDP connection closed before {method} replied")
}
