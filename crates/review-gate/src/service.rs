//! HTTP service. Runs as the `reviewgate` system user; the implementer talks to it over
//! loopback with a bearer token. There is deliberately no "approve" endpoint: verdicts only
//! come from an external model (`POST /tasks/:id/reviews`, asynchronous) or from an explicitly
//! weaker channel (`.../reviews/submit`, which the strength gate will not let through by default).
//! Human (`manual`) reviews are accepted only via the local admin command, never over HTTP.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::GateError;
use crate::gate::{Gate, NewTask};
use crate::model::{Channel, ReviewRecord};
use crate::review::{self, ReviewError};
use crate::reviewer::{AskError, ReviewerConfig};

pub struct AppState {
    pub gate: Gate,
    pub reviewers: ReviewerConfig,
    token: String,
    jobs: Mutex<HashMap<String, Job>>,
    next_job: AtomicU64,
    /// Serializes state mutations (task files are read-modify-write).
    write: Mutex<()>,
}

impl AppState {
    pub fn new(gate: Gate, reviewers: ReviewerConfig, token: String) -> Self {
        Self {
            gate,
            reviewers,
            token,
            jobs: Mutex::new(HashMap::new()),
            next_job: AtomicU64::new(1),
            write: Mutex::new(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub task: String,
    /// running | done | failed
    pub status: String,
    #[serde(default)]
    pub record: Option<ReviewRecord>,
    #[serde(default)]
    pub error: Option<String>,
    /// No external model was reachable (the caller should wait for a human, not downgrade).
    #[serde(default)]
    pub unavailable: bool,
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<GateError> for ApiError {
    fn from(e: GateError) -> Self {
        let code = match e {
            GateError::InvalidId(_) | GateError::Invalid(_) => StatusCode::BAD_REQUEST,
            GateError::NoSuchTask(_) | GateError::NoSuchReview(_) => StatusCode::NOT_FOUND,
            GateError::NotRunning(..) | GateError::Conflict(_) | GateError::Mismatch(_) => {
                StatusCode::CONFLICT
            }
            GateError::Git(_) => StatusCode::UNPROCESSABLE_ENTITY,
            GateError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(code, e.to_string())
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/tasks", get(list_tasks).post(init_task))
        .route("/tasks/{id}", get(show_task))
        .route("/tasks/{id}/link", post(link_task))
        .route("/tasks/{id}/reviews", post(request_review))
        .route("/tasks/{id}/reviews/submit", post(submit_review))
        .route("/tasks/{id}/record", post(record))
        .route("/jobs/{job}", get(job_status))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route("/health", get(health))
        .merge(api)
        .with_state(state)
}

/// Constant-time comparison of the bearer token.
fn token_ok(expected: &str, header_value: Option<&str>) -> bool {
    let Some(given) = header_value.and_then(|h| h.strip_prefix("Bearer ")) else {
        return false;
    };
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(State(s): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if !token_ok(&s.token, header) {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or wrong bearer token".into(),
        )
        .into_response();
    }
    next.run(req).await
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "service": "review-gate", "version": env!("CARGO_PKG_VERSION") }))
}

#[derive(Deserialize)]
struct InitBody {
    id: String,
    goal: String,
    acceptance: String,
    iterations: u32,
    repo: String,
    #[serde(default)]
    min_reviewer: Option<String>,
    #[serde(default)]
    review_provider: Option<String>,
}

async fn init_task(State(s): State<Arc<AppState>>, Json(b): Json<InitBody>) -> ApiResult<Value> {
    let min = match b.min_reviewer.as_deref() {
        None => Channel::WebGemini,
        Some(c) => Channel::parse(c)
            .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, format!("unknown channel {c}")))?,
    };
    let _w = s.write.lock().await;
    let task = s.gate.init(NewTask {
        id: b.id,
        goal: b.goal,
        final_acceptance: b.acceptance,
        repo: b.repo,
        iterations: b.iterations,
        min_reviewer: min,
        review_provider: b.review_provider.unwrap_or_else(|| "auto".into()),
    })?;
    Ok(Json(json!(task)))
}

async fn list_tasks(State(s): State<Arc<AppState>>) -> ApiResult<Value> {
    let tasks = s
        .gate
        .store()
        .list_tasks()
        .map_err(|e| ApiError::from(GateError::Io(e)))?;
    Ok(Json(json!(tasks)))
}

async fn show_task(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(json!(s.gate.store().load_task(&id)?)))
}

#[derive(Deserialize)]
struct LinkBody {
    expr_id: String,
}

async fn link_task(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(b): Json<LinkBody>,
) -> ApiResult<Value> {
    let _w = s.write.lock().await;
    Ok(Json(json!(s.gate.link(&id, &b.expr_id)?)))
}

#[derive(Deserialize)]
struct ReviewBody {
    tree: String,
    base: String,
    #[serde(default)]
    evidence: String,
    #[serde(default)]
    provider: Option<String>,
}

/// Starts an external review in the background (they take minutes) and returns the job.
async fn request_review(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(b): Json<ReviewBody>,
) -> ApiResult<Job> {
    // Fail fast on bad input before spawning.
    let task = s.gate.store().load_task(&id)?;
    review::staged_from(std::path::Path::new(&task.repo), &b.tree, &b.base)?;
    let job = Job {
        id: format!("{id}-{}", s.next_job.fetch_add(1, Ordering::Relaxed)),
        task: id.clone(),
        status: "running".into(),
        record: None,
        error: None,
        unavailable: false,
    };
    s.jobs.lock().await.insert(job.id.clone(), job.clone());
    let state = s.clone();
    let job_id = job.id.clone();
    tokio::spawn(async move {
        let res = review::run(
            &state.gate,
            &id,
            &b.tree,
            &b.base,
            &b.evidence,
            b.provider.as_deref(),
            &state.reviewers,
        )
        .await;
        let mut jobs = state.jobs.lock().await;
        if let Some(j) = jobs.get_mut(&job_id) {
            match res {
                Ok(r) => {
                    j.status = "done".into();
                    j.record = Some(r);
                }
                Err(e) => {
                    j.status = "failed".into();
                    j.unavailable = matches!(e, ReviewError::Ask(AskError::Unavailable(_)));
                    j.error = Some(e.to_string());
                }
            }
        }
    });
    Ok(Json(job))
}

async fn job_status(State(s): State<Arc<AppState>>, Path(job): Path<String>) -> ApiResult<Job> {
    s.jobs
        .lock()
        .await
        .get(&job)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no such job {job}")))
}

#[derive(Deserialize)]
struct SubmitBody {
    tree: String,
    base: String,
    channel: String,
    text: String,
}

/// Weaker channels only (claude-subagent, self-review). `manual` is refused over HTTP.
async fn submit_review(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(b): Json<SubmitBody>,
) -> ApiResult<ReviewRecord> {
    let channel = match Channel::parse(&b.channel) {
        Some(c @ (Channel::ClaudeSubagent | Channel::SelfReview)) => c,
        Some(Channel::Manual) => {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "manual reviews are accepted only via the local admin command run as the gate's user".into(),
            ));
        }
        _ => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "channel must be claude-subagent or self-review (external models: POST /tasks/{id}/reviews)".into(),
            ));
        }
    };
    let _w = s.write.lock().await;
    Ok(Json(review::submit(
        &s.gate, &id, &b.tree, &b.base, channel, &b.text,
    )?))
}

#[derive(Deserialize)]
struct RecordBody {
    review: String,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    tag: Option<String>,
}

async fn record(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(b): Json<RecordBody>,
) -> ApiResult<Value> {
    let _w = s.write.lock().await;
    let out = s
        .gate
        .record(&id, &b.review, b.commit.as_deref(), b.tag.as_deref())?;
    Ok(Json(json!({ "action": out.action, "task": out.task })))
}

pub async fn serve(state: Arc<AppState>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::reviewer::tests::{cfg, mock_bridge};
    use crate::store::Store;
    use crate::{git, model::Verdict};
    use std::fs;
    use std::time::Duration;

    const TOKEN: &str = "test-token-0123456789abcdef0123456789";

    struct Env {
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        url: String,
        client: Client,
    }

    async fn start(bridge: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        for a in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
        ] {
            git::git(&repo, a).unwrap();
        }
        fs::write(repo.join("a.txt"), "v0\n").unwrap();
        git::git(&repo, &["add", "-A"]).unwrap();
        git::git(&repo, &["commit", "-qm", "base"]).unwrap();
        let gate = Gate::new(Store::open(dir.path().join("state")).unwrap());
        let state = Arc::new(AppState::new(gate, cfg(bridge), TOKEN.into()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(state, listener));
        let client = Client::new(&url, TOKEN);
        client
            .post::<Value>(
                "/tasks",
                json!({ "id": "t", "goal": "g", "acceptance": "a", "iterations": 1, "repo": repo }),
            )
            .await
            .unwrap();
        Env {
            _dir: dir,
            repo,
            url,
            client,
        }
    }

    #[tokio::test]
    async fn health_is_open_everything_else_needs_the_token() {
        let e = start("http://127.0.0.1:9").await;
        let r = reqwest::get(format!("{}/health", e.url)).await.unwrap();
        assert_eq!(r.status(), 200);
        let r = reqwest::get(format!("{}/tasks", e.url)).await.unwrap();
        assert_eq!(r.status(), 401);
        let wrong = Client::new(&e.url, "nope-nope-nope-nope-nope-nope-nope");
        assert!(
            wrong
                .get::<Value>("/tasks")
                .await
                .unwrap_err()
                .to_string()
                .contains("401")
        );
    }

    #[tokio::test]
    async fn full_flow_over_http_external_review_then_record() {
        let (bridge, prompts) = mock_bridge(vec![Ok("VERDICT: APPROVED\n\n无")]).await;
        let e = start(&bridge).await;
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let job = e
            .client
            .review(
                "t",
                &tree,
                &base,
                "tests ok",
                None,
                Duration::from_millis(20),
            )
            .await
            .unwrap();
        assert_eq!(job.status, "done", "{:?}", job.error);
        let rec = job.record.unwrap();
        assert_eq!(
            (rec.verdict, rec.channel, rec.tree.as_str()),
            (Verdict::Approved, Channel::WebGemini, tree.as_str())
        );
        assert!(prompts.lock().unwrap()[0].contains("tests ok"));
        git::git(&e.repo, &["commit", "-qm", "v1"]).unwrap();
        git::git(&e.repo, &["tag", "v1"]).unwrap();
        let out: Value = e
            .client
            .post(
                "/tasks/t/record",
                json!({ "review": rec.id, "commit": "HEAD", "tag": "v1" }),
            )
            .await
            .unwrap();
        assert_eq!(out["action"], "finalize");
    }

    #[tokio::test]
    async fn weak_channels_only_and_never_manual_over_http() {
        let e = start("http://127.0.0.1:9").await;
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let body = |ch: &str| json!({ "tree": tree, "base": base, "channel": ch, "text": "VERDICT: APPROVED" });
        let manual = e
            .client
            .post::<Value>("/tasks/t/reviews/submit", body("manual"))
            .await
            .unwrap_err();
        assert!(manual.to_string().contains("403"), "{manual}");
        let ext = e
            .client
            .post::<Value>("/tasks/t/reviews/submit", body("web-gemini"))
            .await
            .unwrap_err();
        assert!(ext.to_string().contains("400"), "{ext}");
        let rec: ReviewRecord = e
            .client
            .post("/tasks/t/reviews/submit", body("claude-subagent"))
            .await
            .unwrap();
        let out: Value = e
            .client
            .post("/tasks/t/record", json!({ "review": rec.id }))
            .await
            .unwrap();
        assert_eq!(
            out["action"], "pause",
            "default threshold web-gemini > claude-subagent"
        );
    }

    #[tokio::test]
    async fn errors_map_to_status_codes() {
        let e = start("http://127.0.0.1:9").await;
        let missing = e
            .client
            .post::<Value>("/tasks/t/record", json!({ "review": "v1-9" }))
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("404"), "{missing}");
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        git::git(&e.repo, &["commit", "-qm", "moved on"]).unwrap();
        let stale = e
            .client
            .post::<Value>("/tasks/t/reviews", json!({ "tree": tree, "base": base }))
            .await
            .unwrap_err();
        assert!(stale.to_string().contains("409"), "{stale}");
        let nobody = e.client.get::<Value>("/jobs/none").await.unwrap_err();
        assert!(nobody.to_string().contains("404"));
    }

    #[tokio::test]
    async fn unavailable_external_ai_is_reported_not_downgraded() {
        let e = start("http://127.0.0.1:9").await;
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let job = e
            .client
            .review(
                "t",
                &tree,
                &base,
                "",
                Some("web-gemini"),
                Duration::from_millis(20),
            )
            .await
            .unwrap();
        assert_eq!(job.status, "failed");
        assert!(job.unavailable, "{:?}", job.error);
    }

    #[test]
    fn bearer_token_check() {
        assert!(token_ok("abc", Some("Bearer abc")));
        assert!(!token_ok("abc", Some("Bearer abd")));
        assert!(!token_ok("abc", Some("abc")));
        assert!(!token_ok("abc", None));
        assert!(
            !token_ok("", Some("Bearer ")),
            "an empty configured token never authenticates"
        );
    }
}
