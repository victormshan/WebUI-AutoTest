//! MCP interface (stdio) for Claude Code: thin tools that forward to the review-gate service.
//!
//! It runs as the implementer, with the implementer's token — it can do exactly what the CLI
//! can, nothing more. Verdicts still come only from the service.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::Client;
use crate::service::Job;

const INSTRUCTIONS: &str = "review-gate: the independent review gate for auto-iterate. \
Per version: edit code → gate_review_start (stages the repo; an external AI from another vendor \
reviews it) → poll gate_job until done → if approved: git commit (exactly one commit on top of \
the reviewed base) and git tag → gate_record with commit and tag (attaches the signed attestation \
note) → follow the returned action. If rejected: gate_record the review, fix, review again. \
Never report a verdict yourself; if no external AI is available, stop and tell the user.";

#[derive(Clone)]
pub struct GateMcp {
    client: Arc<Client>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct IdArgs {
    /// auto-iterate task id
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct InitArgs {
    pub id: String,
    pub goal: String,
    /// Final acceptance criteria (the user's)
    pub acceptance: String,
    /// Number of versions, 1-10
    pub iterations: u32,
    /// Absolute path of the git repository
    pub repo: String,
    /// external-api | web-gemini | claude-subagent | self-review | manual (default web-gemini)
    pub min_reviewer: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct LinkArgs {
    pub id: String,
    /// claude-step-relay exprId
    pub expr_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReviewArgs {
    pub id: String,
    /// Verification evidence (test output etc.); shown to the reviewer as unverified claims
    pub evidence: Option<String>,
    /// auto | gemini-api | openai | web-gemini
    pub provider: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct JobArgs {
    pub job: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SubmitArgs {
    pub id: String,
    /// claude-subagent | self-review (weaker than the default threshold: pauses for a human)
    pub channel: String,
    /// Review text with a `VERDICT: APPROVED|REJECTED` line
    pub text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecordArgs {
    pub id: String,
    /// Review record id, e.g. v2-1
    pub review: String,
    /// Required when the review approved
    pub commit: Option<String>,
    /// Required when the review approved
    pub tag: Option<String>,
}

fn out(r: anyhow::Result<Value>) -> Result<String, String> {
    r.map(|v| serde_json::to_string_pretty(&v).expect("serializable"))
        .map_err(|e| format!("{e:#}"))
}

impl GateMcp {
    pub fn new(client: Client) -> Self {
        Self {
            client: Arc::new(client),
        }
    }
}

#[tool_handler]
impl ServerHandler for GateMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "review-gate",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

#[tool_router]
impl GateMcp {
    #[tool(description = "List auto-iterate tasks known to the gate")]
    async fn gate_task_list(&self) -> Result<String, String> {
        out(self.client.get("/tasks").await)
    }

    #[tool(description = "Show one task: status, current version, history, stop reason")]
    async fn gate_task_show(&self, Parameters(a): Parameters<IdArgs>) -> Result<String, String> {
        out(self.client.get(&format!("/tasks/{}", a.id)).await)
    }

    #[tool(description = "Create an auto-iterate task over a git repository")]
    async fn gate_task_init(&self, Parameters(a): Parameters<InitArgs>) -> Result<String, String> {
        out(self
            .client
            .post(
                "/tasks",
                json!({ "id": a.id, "goal": a.goal, "acceptance": a.acceptance, "iterations": a.iterations,
                        "repo": a.repo, "min_reviewer": a.min_reviewer.unwrap_or_else(|| "web-gemini".into()) }),
            )
            .await)
    }

    #[tool(
        description = "Link a task to a claude-step-relay exprId (the gate writes its own trace entries)"
    )]
    async fn gate_task_link(&self, Parameters(a): Parameters<LinkArgs>) -> Result<String, String> {
        out(self
            .client
            .post(
                &format!("/tasks/{}/link", a.id),
                json!({ "expr_id": a.expr_id }),
            )
            .await)
    }

    #[tool(
        description = "Stage the task's repository (git add -A) and start an external-AI review of the current version. Returns a job; poll gate_job."
    )]
    async fn gate_review_start(
        &self,
        Parameters(a): Parameters<ReviewArgs>,
    ) -> Result<String, String> {
        let r = async {
            let (tree, base) = self.client.stage(&a.id).await?;
            let job: Job = self
                .client
                .post(
                    &format!("/tasks/{}/reviews", a.id),
                    json!({ "tree": tree, "base": base, "evidence": a.evidence.unwrap_or_default(), "provider": a.provider }),
                )
                .await?;
            Ok(serde_json::to_value(job)?)
        };
        out(r.await)
    }

    #[tool(
        description = "Status of a review job: running | done (record has the verdict) | failed (unavailable=true means no external AI: stop and tell the user)"
    )]
    async fn gate_job(&self, Parameters(a): Parameters<JobArgs>) -> Result<String, String> {
        out(self.client.get(&format!("/jobs/{}", a.job)).await)
    }

    #[tool(
        description = "Submit a review from a weaker channel (claude-subagent | self-review). Below the task's threshold it pauses for a human."
    )]
    async fn gate_review_submit(
        &self,
        Parameters(a): Parameters<SubmitArgs>,
    ) -> Result<String, String> {
        let r = async {
            let (tree, base) = self.client.stage(&a.id).await?;
            self.client
                .post(
                    &format!("/tasks/{}/reviews/submit", a.id),
                    json!({ "tree": tree, "base": base, "channel": a.channel, "text": a.text }),
                )
                .await
        };
        out(r.await)
    }

    #[tool(
        description = "Apply a review record to the task. Approved: give the commit and tag; the signed attestation is attached as a git note. Returns the next action."
    )]
    async fn gate_record(&self, Parameters(a): Parameters<RecordArgs>) -> Result<String, String> {
        out(self
            .client
            .record(&a.id, &a.review, a.commit.as_deref(), a.tag.as_deref())
            .await)
    }

    #[tool(description = "The gate's attestation public key (ed25519, base64)")]
    async fn gate_pubkey(&self) -> Result<String, String> {
        out(self.client.get("/pubkey").await)
    }
}

pub async fn serve_stdio(client: Client) -> anyhow::Result<()> {
    GateMcp::new(client)
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git;
    use crate::reviewer::tests::mock_bridge;
    use crate::service::tests::{TOKEN, start};
    use rmcp::model::CallToolRequestParams;
    use std::fs;

    async fn call(
        c: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        name: &'static str,
        args: Value,
    ) -> (bool, String) {
        let mut p = CallToolRequestParams::new(name);
        if let Value::Object(m) = args {
            p = p.with_arguments(m);
        }
        let r = c.call_tool(p).await.unwrap();
        let text = r
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default();
        (r.is_error != Some(true), text)
    }

    #[tokio::test]
    async fn review_and_record_through_mcp_tools() {
        let (bridge, _) = mock_bridge(vec![Ok("VERDICT: APPROVED\n\n无")]).await;
        let e = start(&bridge).await;
        let (st, ct) = tokio::io::duplex(1 << 16);
        let server = GateMcp::new(Client::new(&e.url, TOKEN));
        tokio::spawn(async move { server.serve(st).await.unwrap().waiting().await.unwrap() });
        let c = ().serve(ct).await.unwrap();

        let names: Vec<String> = c
            .list_all_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for n in [
            "gate_task_show",
            "gate_review_start",
            "gate_job",
            "gate_record",
            "gate_pubkey",
        ] {
            assert!(names.iter().any(|x| x == n), "{n} missing from {names:?}");
        }
        assert!(
            c.peer_info()
                .unwrap()
                .instructions
                .as_deref()
                .unwrap_or_default()
                .contains("Never report a verdict yourself")
        );

        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (ok, job) = call(
            &c,
            "gate_review_start",
            json!({ "id": "t", "evidence": "tests ok" }),
        )
        .await;
        assert!(ok, "{job}");
        let job_id = serde_json::from_str::<Value>(&job).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let rec = loop {
            let (_, j) = call(&c, "gate_job", json!({ "job": job_id })).await;
            let j: Value = serde_json::from_str(&j).unwrap();
            if j["status"] != "running" {
                assert_eq!(j["status"], "done", "{j}");
                break j["record"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(rec["verdict"], "approved");

        // Approved but not committed yet: the gate refuses, and the error reaches Claude as a tool error.
        let (ok, err) = call(
            &c,
            "gate_record",
            json!({ "id": "t", "review": rec["id"], "commit": "HEAD", "tag": "nope" }),
        )
        .await;
        assert!(!ok && err.contains("rejected"), "{err}");

        git::git(&e.repo, &["commit", "-qm", "v1"]).unwrap();
        git::git(&e.repo, &["tag", "v1"]).unwrap();
        let (ok, out) = call(
            &c,
            "gate_record",
            json!({ "id": "t", "review": rec["id"], "commit": "HEAD", "tag": "v1" }),
        )
        .await;
        assert!(ok, "{out}");
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["action"],
            "finalize"
        );
        let note = git::git(&e.repo, &["notes", "--ref=review-gate", "show", "HEAD"]).unwrap();
        assert!(note.starts_with("review-gate-attestation: v1"), "{note}");
        let (_, pk) = call(&c, "gate_pubkey", json!({})).await;
        let key = crate::attest::parse_public_key(
            serde_json::from_str::<Value>(&pk).unwrap()["key"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        crate::attest::verify_commit(&e.repo, "HEAD", &key).unwrap();
        c.cancel().await.unwrap();
    }
}
