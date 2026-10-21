//! HTTP service. Runs as the `reviewgate` system user; the implementer talks to it over
//! loopback with a bearer token. There is deliberately no "approve" endpoint: verdicts only
//! come from an external model (`POST /tasks/:id/reviews`, asynchronous) or from an explicitly
//! weaker channel (`.../reviews/submit`, which the strength gate will not let through by default).
//! Human (`manual`) reviews are accepted only via the local admin command, never over HTTP.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Extension, Path, Request, State};
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
    /// Bearer token -> implementer family, maintained by the user (admin), not by the caller.
    agents: HashMap<String, String>,
    /// Legacy single token (`--token-file`); it identifies a `claude` implementer.
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
            agents: HashMap::new(),
            token,
            jobs: Mutex::new(HashMap::new()),
            next_job: AtomicU64::new(1),
            write: Mutex::new(()),
        }
    }

    /// Installs the token -> implementer-family map the client identity comes from.
    ///
    /// The map is validated here (not only in [`read_agents`]) so no caller can install a blank
    /// family that would later be normalized to `claude`, or a trivially short token.
    pub fn with_agents(mut self, agents: HashMap<String, String>) -> Result<Self, GateError> {
        self.agents = validate_agents(agents)?;
        Ok(self)
    }

    /// The implementer family behind a bearer token: the admin map first, then the legacy token.
    ///
    /// Every entry is compared (no early return) so the map lookup does not reveal a matching
    /// prefix position through timing.
    fn family_for(&self, given: &str) -> Option<&str> {
        let mut mapped = None;
        for (token, family) in &self.agents {
            if const_eq(token, given) {
                mapped = Some(family.as_str());
            }
        }
        let legacy = const_eq(&self.token, given);
        if mapped.is_some() {
            mapped
        } else if legacy {
            Some("claude")
        } else {
            None
        }
    }
}

/// Reads the admin-owned `{ "token": "family", … }` map (JSON).
///
/// The file is expected to be readable only by the service account and its group (0600/0640):
/// every value is a raw bearer token.
pub fn read_agents(path: &std::path::Path) -> Result<HashMap<String, String>, GateError> {
    let meta = std::fs::metadata(path).map_err(|e| {
        GateError::Io(anyhow::Error::new(e).context(format!("reading {}", path.display())))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o007 != 0 {
            return Err(GateError::Invalid(format!(
                "agents file {} is accessible by others (mode {:o}); use 0600 or 0640",
                path.display(),
                mode & 0o777
            )));
        }
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        GateError::Io(anyhow::Error::new(e).context(format!("reading {}", path.display())))
    })?;
    let agents: HashMap<String, String> = serde_json::from_str(&text)
        .map_err(|e| GateError::Invalid(format!("agents file {}: {e}", path.display())))?;
    validate_agents(agents)
        .map_err(|e| GateError::Invalid(format!("agents file {}: {e}", path.display())))
}

/// Validates and normalizes a token -> family map. Families are trimmed and lowercased so the
/// value stored on the task is exactly the value compared against the token's identity.
fn validate_agents(agents: HashMap<String, String>) -> Result<HashMap<String, String>, GateError> {
    let mut out = HashMap::with_capacity(agents.len());
    for (token, family) in agents {
        if token.len() < 32 || !token.chars().all(|c| c.is_ascii_graphic()) {
            return Err(GateError::Invalid(
                "each token must be at least 32 printable, non-whitespace characters".into(),
            ));
        }
        let family = family.trim().to_ascii_lowercase();
        if family.is_empty()
            || !family
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(GateError::Invalid(format!(
                "invalid implementer family {family:?}; use [a-z0-9_-]+"
            )));
        }
        out.insert(token, family);
    }
    Ok(out)
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
        .route("/check", post(check_commit))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route("/health", get(health))
        .route("/pubkey", get(pubkey))
        .merge(api)
        .with_state(state)
}

/// Constant-time comparison of secrets.
fn const_eq(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
fn token_ok(expected: &str, header_value: Option<&str>) -> bool {
    header_value
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|given| const_eq(expected, given))
}

/// The implementer family is a property of the authenticated identity, never of the request body.
#[derive(Clone)]
struct Family(String);

async fn auth(State(s): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let family = header
        .and_then(|h| h.strip_prefix("Bearer "))
        .and_then(|given| s.family_for(given))
        .map(str::to_string);
    if let Some(family) = family {
        req.extensions_mut().insert(Family(family));
        return next.run(req).await;
    }
    ApiError(
        StatusCode::UNAUTHORIZED,
        "missing or wrong bearer token".into(),
    )
    .into_response()
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
    /// Optional consistency check: when present it must equal the family implied by the token.
    #[serde(default)]
    implementer_family: Option<String>,
}

async fn init_task(
    State(s): State<Arc<AppState>>,
    Extension(Family(family)): Extension<Family>,
    Json(b): Json<InitBody>,
) -> ApiResult<Value> {
    if let Some(claimed) = b
        .implementer_family
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        && claimed != family
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "implementer_family {claimed:?} does not match this token's identity {family:?}"
            ),
        ));
    }
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
        implementer_family: Some(family),
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
struct CheckBody {
    repo: String,
    tree: String,
    base: String,
}

async fn check_commit(
    State(s): State<Arc<AppState>>,
    Json(b): Json<CheckBody>,
) -> ApiResult<crate::gate::CommitCheck> {
    Ok(Json(s.gate.check_commit(&b.repo, &b.tree, &b.base)?))
}

/// The attestation verification key (public; pin it in CI).
async fn pubkey(State(s): State<Arc<AppState>>) -> ApiResult<Value> {
    match s.gate.public_key() {
        Some(k) => Ok(Json(json!({ "algorithm": "ed25519", "key": k }))),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            "this gate does not sign attestations".into(),
        )),
    }
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
    let attestation = out.task.history.last().and_then(|h| h.attestation.clone());
    Ok(Json(
        json!({ "action": out.action, "task": out.task, "attestation": attestation }),
    ))
}

pub async fn serve(state: Arc<AppState>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::client::Client;
    use crate::reviewer::tests::{cfg, mock_bridge};
    use crate::store::Store;
    use crate::{git, model::Verdict};
    use std::fs;
    use std::time::Duration;

    pub(crate) const TOKEN: &str = "test-token-0123456789abcdef0123456789";

    pub(crate) struct Env {
        pub(crate) _dir: tempfile::TempDir,
        pub(crate) repo: std::path::PathBuf,
        pub(crate) url: String,
        pub(crate) client: Client,
    }

    pub(crate) async fn start(bridge: &str) -> Env {
        start_with(bridge, &[]).await
    }

    pub(crate) async fn start_with(bridge: &str, agents: &[(&str, &str)]) -> Env {
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
        let gate = Gate::new(Store::open(dir.path().join("state")).unwrap())
            .with_signer(crate::attest::Keypair::load_or_create(&dir.path().join("state")).unwrap())
            .with_relay(crate::relay::RelayTrace::new(dir.path().join("relay")));
        let mut state = AppState::new(gate, cfg(bridge), TOKEN.into());
        if !agents.is_empty() {
            state = state
                .with_agents(
                    agents
                        .iter()
                        .map(|(t, f)| ((*t).to_string(), (*f).to_string()))
                        .collect(),
                )
                .unwrap();
        }
        let state = Arc::new(state);
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
    async fn the_implementer_family_comes_from_the_token_not_the_request() {
        let oc_token = "opencode-token-0123456789abcdef0123456789abcdef";
        let e = start_with("http://127.0.0.1:9", &[(oc_token, "opencode")]).await;
        let t = e.client.get::<Value>("/tasks/t").await.unwrap();
        assert_eq!(t["implementer_family"], "claude", "legacy token");

        let oc = Client::new(&e.url, oc_token);
        let posted = oc
            .post::<Value>(
                "/tasks",
                json!({ "id": "u", "goal": "g", "acceptance": "a", "iterations": 1, "repo": e.repo }),
            )
            .await
            .unwrap();
        assert_eq!(posted["implementer_family"], "opencode");

        let err = e
            .client
            .post::<Value>(
                "/tasks",
                json!({ "id": "v", "goal": "g", "acceptance": "a", "iterations": 1,
                        "repo": e.repo, "implementer_family": "opencode" }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        assert!(err.to_string().contains("400"), "{err}");

        let matching = oc
            .post::<Value>(
                "/tasks",
                json!({ "id": "w", "goal": "g", "acceptance": "a", "iterations": 1,
                        "repo": e.repo, "implementer_family": "opencode" }),
            )
            .await
            .unwrap();
        assert_eq!(matching["implementer_family"], "opencode");
    }

    #[tokio::test]
    async fn full_flow_over_http_external_review_then_record() {
        let (bridge, prompts) = mock_bridge(vec![Ok("VERDICT: APPROVED\n\n无")]).await;
        let e = start(&bridge).await;
        e.client
            .post::<Value>("/tasks/t/link", json!({ "expr_id": "x1" }))
            .await
            .unwrap();
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

        // The attestation verifies against /pubkey once attached as a note, and only for this commit.
        let pk: Value = reqwest::get(format!("{}/pubkey", e.url))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let key = crate::attest::parse_public_key(pk["key"].as_str().unwrap()).unwrap();
        let note = out["attestation"]
            .as_str()
            .expect("approved record is attested");
        assert!(
            crate::attest::verify_commit(&e.repo, "HEAD", &key).is_err(),
            "no note yet"
        );
        git::git(
            &e.repo,
            &["notes", "--ref=review-gate", "add", "-m", note, "HEAD"],
        )
        .unwrap();
        let st = crate::attest::verify_commit(&e.repo, "HEAD", &key).unwrap();
        assert_eq!(
            (st.version.as_str(), st.review.as_str()),
            ("1/1", rec.id.as_str())
        );
        let (ok, bad) = crate::attest::verify_range(&e.repo, "HEAD~1..HEAD", &key, false).unwrap();
        assert_eq!((ok.len(), bad.len()), (1, 0));
        // An unreviewed commit on top fails the range check.
        fs::write(e.repo.join("a.txt"), "sneaky\n").unwrap();
        git::git(&e.repo, &["commit", "-qam", "sneaky"]).unwrap();
        let (_, bad) = crate::attest::verify_range(&e.repo, "HEAD~2..HEAD", &key, false).unwrap();
        assert_eq!(bad.len(), 1, "{bad:?}");
        // Copying the note onto an amended commit does not help.
        git::git(
            &e.repo,
            &["notes", "--ref=review-gate", "add", "-m", note, "HEAD"],
        )
        .unwrap();
        assert!(crate::attest::verify_commit(&e.repo, "HEAD", &key).is_err());

        let trace = fs::read_to_string(e._dir.path().join("relay/traces/x1.gate.md")).unwrap();
        assert!(
            trace.contains("审核记录 v1-1") && trace.contains("第 1/1 版通过"),
            "{trace}"
        );
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

    #[cfg(unix)]
    fn mode(p: &std::path::Path, bits: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(bits)).unwrap();
    }

    #[test]
    fn agents_file_is_a_token_to_family_map() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("agents.json");
        let t1 = "token-1-0123456789abcdef0123456789abcdef";
        let t2 = "token-2-0123456789abcdef0123456789abcdef";
        fs::write(
            &p,
            format!(r#"{{ "{t1}": " OpenCode ", "{t2}": "deepseek" }}"#),
        )
        .unwrap();
        #[cfg(unix)]
        mode(&p, 0o600);
        let m = super::read_agents(&p).unwrap();
        assert_eq!(m.get(t1).map(String::as_str), Some("opencode"));
        assert_eq!(m.get(t2).map(String::as_str), Some("deepseek"));
        fs::write(&p, "not json").unwrap();
        assert!(super::read_agents(&p).is_err());
    }

    #[test]
    fn agents_file_rejects_blank_families_short_tokens_and_open_permissions() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("agents.json");
        let t = "token-1-0123456789abcdef0123456789abcdef";
        let check = |body: &str, bits: u32| {
            fs::write(&p, body).unwrap();
            #[cfg(unix)]
            mode(&p, bits);
            super::read_agents(&p)
        };
        let err = check(&format!(r#"{{ "{t}": "  " }}"#), 0o600).unwrap_err();
        assert!(
            err.to_string().contains("invalid implementer family"),
            "{err}"
        );
        let err = check(r#"{ "short": "opencode" }"#, 0o600).unwrap_err();
        assert!(err.to_string().contains("at least 32"), "{err}");
        let err = check(&format!(r#"{{ " {t}": "opencode" }}"#), 0o600).unwrap_err();
        assert!(err.to_string().contains("at least 32"), "{err}");
        let err = check(&format!(r#"{{ "{t}": "bad family" }}"#), 0o600).unwrap_err();
        assert!(
            err.to_string().contains("invalid implementer family"),
            "{err}"
        );
        #[cfg(unix)]
        {
            let err = check(&format!(r#"{{ "{t}": "opencode" }}"#), 0o644).unwrap_err();
            assert!(err.to_string().contains("accessible by others"), "{err}");
            assert!(check(&format!(r#"{{ "{t}": "opencode" }}"#), 0o640).is_ok());
        }
        assert!(super::read_agents(&d.path().join("missing.json")).is_err());
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
