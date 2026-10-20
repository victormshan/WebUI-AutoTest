use std::sync::Arc;

use agent_bridge::{bridge::Bridge, client::Client, mcp::BridgeMcp, service};
use reqwest::Client as Http;
use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde_json::{Value, json};

const AGENTS: [&str; 4] = ["claude", "dsh", "codex", "opencode"];

async fn start() -> (tempfile::TempDir, String, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let bridge = Bridge::open(dir.path().join("state"), &AGENTS).unwrap();
    let tokens = AGENTS
        .iter()
        .map(|a| (a.to_string(), service::sha256(&token(a))))
        .collect();
    let state = Arc::new(service::AppState::new(bridge, tokens).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        service::serve(state, listener).await.unwrap();
    });
    (dir, url, task)
}

fn token(agent: &str) -> String {
    format!("{agent}-test-token-0123456789abcdef0123456789")
}

async fn mcp_call(
    c: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    args: Value,
) -> (bool, Value) {
    let response = c
        .call_tool(
            CallToolRequestParams::new(name.to_owned())
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    let text = response
        .content
        .first()
        .and_then(|v| v.as_text())
        .map(|v| v.text.clone())
        .unwrap_or_default();
    (
        response.is_error != Some(true),
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[tokio::test]
async fn all_four_agents_complete_tasks_in_both_directions_over_mcp() {
    let (_dir, url, server) = start().await;
    let mut clients = Vec::new();
    let mut sessions = Vec::new();
    for agent in AGENTS {
        let (st, ct) = tokio::io::duplex(1 << 18);
        let gateway = BridgeMcp::new(Client::new(&url, &token(agent)));
        sessions.push(tokio::spawn(async move {
            gateway.serve(st).await.unwrap().waiting().await.unwrap();
        }));
        let client = ().serve(ct).await.unwrap();
        let (ok, id) = mcp_call(&client, "bridge_identity", json!({})).await;
        assert!(ok);
        assert_eq!(id["agent"], agent);
        assert_eq!(id["agents"].as_array().unwrap().len(), 4);
        clients.push(client);
    }
    for (i, from) in AGENTS.iter().enumerate() {
        for (j, to) in AGENTS.iter().enumerate() {
            if i == j {
                continue;
            }
            let id = format!("{from}-to-{to}");
            let send = json!({"id":id,"to":to,"title":"two-way check","body":"verify receipt","client_msg_id":format!("send-{id}")});
            let (ok, first) = mcp_call(&clients[i], "bridge_send", send.clone()).await;
            assert!(ok, "{first}");
            assert_eq!(first["from"], *from);
            assert_eq!(
                mcp_call(&clients[i], "bridge_send", send).await.1["n"],
                first["n"]
            );
            let (ok, inbox) = mcp_call(&clients[j], "bridge_inbox", json!({})).await;
            assert!(ok);
            assert!(
                inbox["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["task"] == id)
            );
            // A registered third agent still cannot read this pair's task.
            let other = (0..4).find(|k| *k != i && *k != j).unwrap();
            assert!(
                !mcp_call(&clients[other], "bridge_show", json!({"task":id}))
                    .await
                    .0
            );
            assert!(
                mcp_call(&clients[j], "bridge_post", json!({"task":id,"kind":"ack"}))
                    .await
                    .0
            );
            assert!(
                !mcp_call(
                    &clients[j],
                    "bridge_post",
                    json!({"task":id,"kind":"verdict","judgement":"pass"})
                )
                .await
                .0
            );
            assert!(mcp_call(&clients[j], "bridge_post", json!({"task":id,"kind":"question","body":"which evidence?","questions":[{"id":"q","text":"which evidence?","blocking":true}]})).await.0);
            assert!(
                mcp_call(
                    &clients[i],
                    "bridge_post",
                    json!({"task":id,"kind":"answer","body":"test output"})
                )
                .await
                .0
            );
            let result = json!({"task":id,"kind":"result","outcome":"done","results":[{"item":"receipt","status":"done","evidence":"MCP test"}],"client_msg_id":format!("result-{id}")});
            let (ok, reply) = mcp_call(&clients[j], "bridge_post", result.clone()).await;
            assert!(ok, "{reply}");
            assert_eq!(
                mcp_call(&clients[j], "bridge_post", result).await.1["n"],
                reply["n"]
            );
            assert!(
                mcp_call(
                    &clients[i],
                    "bridge_post",
                    json!({"task":id,"kind":"verdict","judgement":"pass"})
                )
                .await
                .0
            );
            assert!(
                mcp_call(
                    &clients[i],
                    "bridge_post",
                    json!({"task":id,"kind":"close"})
                )
                .await
                .0
            );
            assert_eq!(
                mcp_call(&clients[j], "bridge_show", json!({"task":id}))
                    .await
                    .1["task"]["state"],
                "closed"
            );
        }
    }
    for c in clients {
        c.cancel().await.unwrap();
    }
    for s in sessions {
        s.abort();
    }
    server.abort();
}

#[tokio::test]
async fn identity_is_authenticated_and_new_relays_survive_restart() {
    let (dir, url, server) = start().await;
    assert_eq!(
        Http::new()
            .get(format!("{url}/v1/identity"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let claude = Client::new(&url, &token("claude"));
    let codex = Client::new(&url, &token("codex"));
    claude.post("/v1/tasks", json!({"id":"relay","kind":"task","body":"check","meta":{"to":"codex","title":"relay"},"from":"opencode"})).await.unwrap();
    assert_eq!(
        codex.get("/v1/tasks/relay").await.unwrap()["messages"][0]["from"],
        "claude",
        "token identity wins over body spoofing"
    );
    codex
        .post("/v1/tasks/relay/messages", json!({"kind":"ack"}))
        .await
        .unwrap();
    codex.post("/v1/tasks/relay/messages", json!({"kind":"progress","needs_user":[{"text":"ask via Codex","relay":"codex"},{"text":"ask via OpenCode","relay":"opencode"}]})).await.unwrap();
    let pending = codex.get("/v1/user/pending").await.unwrap();
    assert_eq!(pending["items"][0]["relay"], "codex");
    assert_eq!(pending["items"][1]["relay"], "opencode");
    let reloaded = Bridge::open(dir.path().join("state"), &AGENTS).unwrap();
    assert_eq!(reloaded.bad_lines, 0);
    assert_eq!(
        reloaded
            .messages("relay")
            .unwrap()
            .last()
            .unwrap()
            .needs_user
            .len(),
        2
    );
    server.abort();
}

#[tokio::test]
async fn empty_parent_token_uses_selected_participants_file() {
    let (dir, url, server) = start().await;
    let file = dir.path().join("codex.token");
    std::fs::write(&file, token("codex")).unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("identity")
        .env("AGENT_BRIDGE_URL", &url)
        .env("AGENT_BRIDGE_TOKEN", "")
        .env("AGENT_BRIDGE_TOKEN_FILE", &file)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let id: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(id["agent"], "codex");
    server.abort();
}
