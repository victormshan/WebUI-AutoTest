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
use crate::drain::{Drain, OpGuard};
use crate::gate::{Gate, NewTask};
use crate::milestone::flow::{Flow, FlowError, valid_job_id};
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
    /// The milestone flow (6c), when `serve --milestones` is given.
    milestones: Option<Arc<Flow>>,
    /// Maintenance drain: every external write is admitted through it.
    drain: Arc<Drain>,
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
            milestones: None,
            drain: Drain::new(None),
        }
    }

    /// Must come before `with_milestones`, so the flow carries the same drain.
    pub fn with_drain(mut self, drain: Arc<Drain>) -> Self {
        self.drain = drain;
        self
    }

    pub fn with_milestones(mut self, flow: Arc<Flow>) -> Self {
        flow.set_drain(self.drain.clone());
        self.milestones = Some(flow);
        self
    }

    pub fn drain(&self) -> &Arc<Drain> {
        &self.drain
    }
}

/// Admits one external write, or 503 once draining (before any effect).
fn admit(s: &AppState, what: &str) -> Result<OpGuard, ApiError> {
    s.drain
        .enter(what)
        .map_err(|why| ApiError(StatusCode::SERVICE_UNAVAILABLE, format!("draining: {why}")))
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
        .route("/milestones", post(ms_apply))
        .route("/milestones/log", get(ms_log))
        .route("/milestones/published", post(ms_published))
        .route("/milestones/{phase}", get(ms_status))
        .route("/milestones/{phase}/requests", post(ms_request))
        .route("/milestones/{phase}/requests/{id}/cancel", post(ms_cancel))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route("/health", get(health))
        .route("/pubkey", get(pubkey))
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

/// Liveness plus the drain view an operator stops on: `drain.idle` is true only when the latch
/// is set, no admitted operation is unfinished and nothing failed; `m2_pending` counts phases
/// whose current request is still Pending in the log (null when it cannot be read).
async fn health(State(s): State<Arc<AppState>>) -> Json<Value> {
    let d = s.drain.snapshot();
    let m1_running = s
        .jobs
        .lock()
        .await
        .values()
        .filter(|j| j.status == "running")
        .count();
    let m2_pending = match s.milestones.clone() {
        None => json!(0),
        Some(f) => match tokio::task::spawn_blocking(move || f.pending_requests()).await {
            Ok(Ok(n)) => json!(n),
            _ => Value::Null,
        },
    };
    Json(json!({
        "ok": true, "service": "review-gate", "version": env!("CARGO_PKG_VERSION"),
        "drain": {
            "configured": d.configured, "draining": d.draining, "reason": d.reason,
            "active_ops": d.active_ops, "failure_count": d.failure_count,
            "failures": d.failures, "idle": d.idle(),
        },
        "m1_jobs_running": m1_running,
        "m2_pending": m2_pending,
    }))
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
    let _op = admit(&s, "init task")?;
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
    let _op = admit(&s, "link task")?;
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
    let op = admit(&s, "review request")?;
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
    // Counted until the job's status is final (after review::run has stored its record).
    let carried = op.carry(&format!("m1 review job {job_id}"));
    drop(op);
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
        drop(jobs);
        drop(carried);
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
    let _op = admit(&s, "review submit")?;
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
    // Not a write, but refused while draining all the same: a refusal never lets a commit by.
    let _op = admit(&s, "check")?;
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
    let _op = admit(&s, "record")?;
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

/// `serve`, ending gracefully once `shutdown` fires (the milestone halt watchdog).
pub async fn serve_until(
    state: Arc<AppState>,
    listener: tokio::net::TcpListener,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
        })
        .await
}

// ---- milestones (M2 step 6c) ----

fn halted_response(why: String) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "the milestone service has halted", "halted": why })),
    )
        .into_response()
}

fn check_phase(phase: &str) -> Result<(), Box<Response>> {
    if agent_bridge::valid_id(phase) && phase.len() <= 56 {
        Ok(())
    } else {
        Err(Box::new(
            ApiError(StatusCode::BAD_REQUEST, format!("invalid phase {phase:?}")).into_response(),
        ))
    }
}

/// Every milestone route goes through here (Codex n4 §3, n6): 404 without milestones; 503 if
/// the service has halted before the call or by the time it returns, whatever the call
/// produced; the call itself on a blocking thread. The admission boundaries stay inside the
/// flow (6b); nothing here holds them.
///
/// `op` (the drain guard) moves into the blocking call, so it is released when the call has
/// ended, even if the client goes away and this future is dropped first.
async fn guarded<T: Send + 'static>(
    s: &AppState,
    op: OpGuard,
    f: impl FnOnce(&Arc<Flow>) -> Result<T, FlowError> + Send + 'static,
) -> Result<T, Box<Response>> {
    let Some(flow) = s.milestones.clone() else {
        return Err(Box::new(
            ApiError(StatusCode::NOT_FOUND, "milestones are not enabled".into()).into_response(),
        ));
    };
    if let Some(why) = flow.halted() {
        return Err(Box::new(halted_response(why)));
    }
    let f2 = flow.clone();
    let out = tokio::task::spawn_blocking(move || {
        let r = f(&f2);
        #[cfg(test)]
        f2.test_in_call();
        drop(op);
        r
    })
    .await;
    if let Some(why) = flow.halted() {
        return Err(Box::new(halted_response(why)));
    }
    let err = match out {
        Ok(Ok(v)) => return Ok(v),
        Ok(Err(FlowError::Refused(reasons))) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "refused", "refused": reasons })),
        )
            .into_response(),
        Ok(Err(FlowError::Other(e))) => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{e}")
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(300)
                .collect(),
        )
        .into_response(),
        Err(_) => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the milestone call failed".into(),
        )
        .into_response(),
    };
    Err(Box::new(err))
}

/// A milestone write: admitted through the drain first (503 once draining, before any
/// effect), and counted until the call has returned, including the job a request dispatches
/// (the flow carries that one on).
async fn guarded_write<T: Send + 'static>(
    s: &AppState,
    what: &str,
    f: impl FnOnce(&Arc<Flow>) -> Result<T, FlowError> + Send + 'static,
) -> Result<T, Box<Response>> {
    let op = admit(s, what).map_err(|e| Box::new(e.into_response()))?;
    guarded(s, op, f).await
}

/// A milestone read: never refused, but counted while it runs (a status read may still
/// complete a crash-cut group in the log).
async fn guarded_read<T: Send + 'static>(
    s: &AppState,
    what: &str,
    f: impl FnOnce(&Arc<Flow>) -> Result<T, FlowError> + Send + 'static,
) -> Result<T, Box<Response>> {
    let op = s.drain.track(what);
    guarded(s, op, f).await
}

fn ok_json(v: impl Serialize) -> Response {
    Json(json!(v)).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyBody {
    phase: String,
    base: String,
    task_book: String,
    evidence_dir: String,
    #[serde(default)]
    conditions: Vec<String>,
    #[serde(default)]
    supersedes: Option<String>,
}

async fn ms_apply(State(s): State<Arc<AppState>>, Json(b): Json<ApplyBody>) -> Response {
    if let Err(r) = check_phase(&b.phase) {
        return *r;
    }
    match guarded_write(&s, "milestone apply", move |f| {
        f.apply(
            &b.phase,
            &b.base,
            &b.task_book,
            &b.evidence_dir,
            b.conditions,
            b.supersedes,
        )
    })
    .await
    {
        Ok(a) => ok_json(a),
        Err(r) => *r,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBody {
    head: String,
}

async fn ms_request(
    State(s): State<Arc<AppState>>,
    Path(phase): Path<String>,
    Json(b): Json<RequestBody>,
) -> Response {
    if let Err(r) = check_phase(&phase) {
        return *r;
    }
    match guarded_write(&s, "milestone request", move |f| f.request(&phase, &b.head)).await {
        Ok(id) => ok_json(json!({ "request_id": id })),
        Err(r) => *r,
    }
}

async fn ms_status(State(s): State<Arc<AppState>>, Path(phase): Path<String>) -> Response {
    if let Err(r) = check_phase(&phase) {
        return *r;
    }
    match guarded_read(&s, "milestone status", move |f| {
        #[cfg(feature = "fault-injection")]
        crate::milestone::serve::fault::in_status();
        f.status_archived(&phase)
    })
    .await
    {
        Ok(st) => ok_json(st),
        Err(r) => *r,
    }
}

async fn ms_cancel(
    State(s): State<Arc<AppState>>,
    Path((phase, id)): Path<(String, String)>,
) -> Response {
    if let Err(r) = check_phase(&phase) {
        return *r;
    }
    if !valid_job_id(&id) {
        return ApiError(
            StatusCode::BAD_REQUEST,
            format!("invalid request id {id:?}"),
        )
        .into_response();
    }
    match guarded_write(&s, "milestone cancel", move |f| f.cancel(&phase, &id)).await {
        Ok(()) => ok_json(json!({ "cancelled": true })),
        Err(r) => *r,
    }
}

async fn ms_log(State(s): State<Arc<AppState>>) -> Response {
    match guarded_read(&s, "milestone log", |f| f.log().map_err(FlowError::Other)).await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response(),
        Err(r) => *r,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedBody {
    seq: u64,
    hash: String,
}

async fn ms_published(State(s): State<Arc<AppState>>, Json(b): Json<PublishedBody>) -> Response {
    match guarded_write(&s, "milestone published", move |f| {
        f.published_then_archive(b.seq, &b.hash)
    })
    .await
    {
        Ok(o) => ok_json(o),
        Err(r) => *r,
    }
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
        start_in(bridge, Drain::new(None)).await
    }

    pub(crate) async fn start_in(bridge: &str, drain: Arc<Drain>) -> Env {
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
        let state = Arc::new(AppState::new(gate, cfg(bridge), TOKEN.into()).with_drain(drain));
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

    async fn health(url: &str) -> Value {
        reqwest::get(format!("{url}/health"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn raw(url: &str, method: &str, path: &str, body: Value) -> (u16, Value) {
        let r = reqwest::Client::new()
            .request(method.parse().unwrap(), format!("{url}{path}"))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        let code = r.status().as_u16();
        (code, r.json().await.unwrap_or(Value::Null))
    }

    /// Drain (Codex n8 R1): once latched, every M1 write answers 503 and changes nothing;
    /// reads still answer; health says it is safe to stop.
    #[tokio::test]
    async fn every_m1_write_is_refused_while_draining_and_changes_nothing() {
        let drain = Drain::new(None);
        let e = start_in("http://127.0.0.1:9", drain.clone()).await;
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let before: Value = e.client.get("/tasks/t").await.unwrap();
        let h = health(&e.url).await;
        assert_eq!(h["drain"]["draining"], false, "{h}");
        assert_eq!(h["drain"]["idle"], false, "{h}");
        drain.latch("test".into());
        let tb = json!({ "tree": tree, "base": base });
        for (path, body) in [
            (
                "/tasks",
                json!({ "id": "t2", "goal": "g", "acceptance": "a", "iterations": 1, "repo": e.repo }),
            ),
            ("/tasks/t/link", json!({ "expr_id": "x9" })),
            (
                "/tasks/t/reviews",
                json!({ "tree": tree, "base": base, "evidence": "" }),
            ),
            (
                "/tasks/t/reviews/submit",
                json!({ "tree": tree, "base": base, "channel": "self-review", "text": "VERDICT: APPROVED\n" }),
            ),
            ("/tasks/t/record", json!({ "review": "v1-1" })),
            (
                "/check",
                json!({ "repo": e.repo, "tree": tb["tree"], "base": tb["base"] }),
            ),
        ] {
            let (c, v) = raw(&e.url, "POST", path, body).await;
            assert_eq!(c, 503, "{path}: {v}");
            assert!(
                v["error"].as_str().unwrap().starts_with("draining: test"),
                "{path}: {v}"
            );
        }
        let after: Value = e.client.get("/tasks/t").await.unwrap();
        assert_eq!(before, after, "no write reached the task");
        let tasks: Value = e.client.get("/tasks").await.unwrap();
        assert_eq!(tasks.as_array().unwrap().len(), 1, "no task was created");
        let h = health(&e.url).await;
        assert_eq!(
            (
                &h["drain"]["draining"],
                &h["drain"]["active_ops"],
                &h["drain"]["idle"],
                &h["m1_jobs_running"]
            ),
            (&json!(true), &json!(0), &json!(true), &json!(0)),
            "{h}"
        );
        assert_eq!(h["drain"]["reason"], "test");
    }

    /// A review admitted before the drain keeps the drain from going idle until its status is
    /// final; then it is done and stored, and only then idle.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_review_admitted_before_the_drain_finishes_and_is_counted_until_then() {
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (bridge, _) = crate::reviewer::tests::mock_bridge_held(
            0,
            vec![Ok("VERDICT: APPROVED\n\n无")],
            Some(release.clone()),
        )
        .await;
        let drain = Drain::new(None);
        let e = start_in(&bridge, drain.clone()).await;
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let (c, job) = raw(
            &e.url,
            "POST",
            "/tasks/t/reviews",
            json!({ "tree": tree, "base": base }),
        )
        .await;
        assert_eq!(c, 200, "{job}");
        let job_id = job["id"].as_str().unwrap().to_string();
        drain.latch("maintenance".into());
        let h = health(&e.url).await;
        assert_eq!(h["drain"]["active_ops"], 1, "{h}");
        assert_eq!(h["drain"]["idle"], false, "{h}");
        assert_eq!(h["m1_jobs_running"], 1, "{h}");
        // New work is refused meanwhile.
        let (c, _) = raw(
            &e.url,
            "POST",
            "/tasks/t/reviews",
            json!({ "tree": tree, "base": base }),
        )
        .await;
        assert_eq!(c, 503);
        release.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut h = Value::Null;
        for _ in 0..200 {
            h = health(&e.url).await;
            if h["drain"]["idle"] == true {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(h["drain"]["idle"], true, "{h}");
        // Idle means persisted: the job is final and its record is in the store.
        let j: Job = e.client.get(&format!("/jobs/{job_id}")).await.unwrap();
        assert_eq!(j.status, "done", "{j:?}");
        let rec = j.record.unwrap();
        let task: Value = e.client.get("/tasks/t").await.unwrap();
        assert_eq!(task["id"], "t");
        assert_eq!(rec.verdict, Verdict::Approved);
        assert_eq!(h["m1_jobs_running"], 0, "{h}");
    }

    /// Linearization: an operation admitted just before the latch is counted in every snapshot
    /// taken after the latch, until it has finished (two threads and a barrier; the hook runs
    /// outside the drain mutex, so the latch is not blocked by it).
    #[tokio::test(flavor = "multi_thread")]
    async fn an_operation_admitted_just_before_the_latch_is_counted_until_it_ends() {
        let drain = Drain::new(None);
        let e = start_in("http://127.0.0.1:9", drain.clone()).await;
        let (in_op, go) = (
            Arc::new(std::sync::Barrier::new(2)),
            Arc::new(std::sync::Barrier::new(2)),
        );
        let (a, b) = (in_op.clone(), go.clone());
        drain.set_after_enter(Some(Arc::new(move || {
            a.wait();
            b.wait();
        })));
        let url = e.url.clone();
        let op = tokio::spawn(async move {
            raw(&url, "POST", "/tasks/t/link", json!({ "expr_id": "x2" })).await
        });
        let (d2, in2, go2) = (drain.clone(), in_op.clone(), go.clone());
        let snap = tokio::task::spawn_blocking(move || {
            in2.wait();
            d2.set_after_enter(None);
            d2.latch("now".into());
            let s = d2.snapshot();
            go2.wait();
            s
        })
        .await
        .unwrap();
        assert!(
            snap.draining && snap.active_ops == 1 && !snap.idle(),
            "{snap:?}"
        );
        let (c, v) = op.await.unwrap();
        assert_eq!(c, 200, "the admitted operation completes: {v}");
        let task: Value = e.client.get("/tasks/t").await.unwrap();
        assert_eq!(task["expr_id"], "x2", "and its write is on file");
        assert!(drain.snapshot().idle());
        let (c, _) = raw(&e.url, "POST", "/tasks/t/link", json!({ "expr_id": "x3" })).await;
        assert_eq!(c, 503);
    }
}
