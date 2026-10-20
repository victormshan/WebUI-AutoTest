//! HTTP service. Runs as the `reviewgate` system user; the implementer talks to it over
//! loopback with a bearer token. There is deliberately no "approve" endpoint: verdicts only
//! come from an external model (`POST /tasks/:id/reviews`, asynchronous) or from an explicitly
//! weaker channel (`.../reviews/submit`, which the strength gate will not let through by default).
//! Human (`manual`) reviews are accepted only via the local admin command, never over HTTP.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Extension;
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
use crate::identity::{Identity, IdentityConfig};
use crate::milestone::flow::{Flow, FlowError, valid_job_id};
use crate::model::{Channel, ReviewRecord};
use crate::review::{self, ReviewError};
use crate::reviewer::{AskError, ReviewerConfig};

pub struct AppState {
    pub gate: Gate,
    pub reviewers: ReviewerConfig,
    identities: IdentityConfig,
    jobs: Mutex<HashMap<String, Job>>,
    next_job: AtomicU64,
    /// Serializes state mutations (task files are read-modify-write).
    write: Mutex<()>,
    /// The milestone flow (6c), when `serve --milestones` is given.
    milestones: Option<Arc<Flow>>,
}

impl AppState {
    /// Legacy constructor: one plaintext bearer token → `legacy-claude` / `claude`.
    pub fn new(gate: Gate, reviewers: ReviewerConfig, token: String) -> Self {
        Self::with_identities(gate, reviewers, IdentityConfig::legacy(token))
    }

    /// Constructor with an administrator-managed identity map.
    pub fn with_identities(
        gate: Gate,
        reviewers: ReviewerConfig,
        identities: IdentityConfig,
    ) -> Self {
        Self {
            gate,
            reviewers,
            identities,
            jobs: Mutex::new(HashMap::new()),
            next_job: AtomicU64::new(1),
            write: Mutex::new(()),
            milestones: None,
        }
    }

    pub fn with_milestones(mut self, flow: Arc<Flow>) -> Self {
        self.milestones = Some(flow);
        self
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
    /// The principal that started the job; only it (or the administrator) may read it.
    #[serde(default)]
    pub principal: String,
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
            GateError::Forbidden(_) => StatusCode::FORBIDDEN,
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

/// Resolves the bearer token to a principal + family, and carries it in the request context.
/// Unknown, disabled and (in hashed mode) legacy tokens are all simply unauthorized: there is
/// no fallback from a configured identity map to the legacy token.
async fn auth(State(s): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    let Some(who) = given.and_then(|t| s.identities.authenticate(t)) else {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or wrong bearer token".into(),
        )
        .into_response();
    };
    req.extensions_mut().insert(who);
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
    /// Optional self-report: only checked against the authenticated identity, never trusted.
    #[serde(default)]
    implementer_family: Option<String>,
}

async fn init_task(
    State(s): State<Arc<AppState>>,
    Extension(who): Extension<Identity>,
    Json(b): Json<InitBody>,
) -> ApiResult<Value> {
    let min = match b.min_reviewer.as_deref() {
        None => Channel::WebGemini,
        Some(c) => Channel::parse(c)
            .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, format!("unknown channel {c}")))?,
    };
    let _w = s.write.lock().await;
    let task = s.gate.init(NewTask {
        id: b.id,
        principal: who.principal.clone(),
        implementer_family: who.family.clone(),
        credential: who.credential.clone(),
        config_digest: who.config_digest.clone(),
        claimed_family: b.implementer_family,
        goal: b.goal,
        final_acceptance: b.acceptance,
        repo: b.repo,
        iterations: b.iterations,
        min_reviewer: min,
        review_provider: b.review_provider.unwrap_or_else(|| "auto".into()),
    })?;
    Ok(Json(json!(task)))
}

async fn list_tasks(
    State(s): State<Arc<AppState>>,
    Extension(who): Extension<Identity>,
) -> ApiResult<Value> {
    let tasks = s
        .gate
        .store()
        .list_tasks()
        .map_err(|e| ApiError::from(GateError::Io(e)))?
        .into_iter()
        .filter(|t| s.gate.can_access(t, &who))
        .collect::<Vec<_>>();
    Ok(Json(json!(tasks)))
}

async fn show_task(
    State(s): State<Arc<AppState>>,
    Extension(who): Extension<Identity>,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    let task = s.gate.store().load_task(&id)?;
    s.gate.ensure_owner(&task, &who)?;
    Ok(Json(json!(task)))
}

#[derive(Deserialize)]
struct LinkBody {
    expr_id: String,
}

async fn link_task(
    State(s): State<Arc<AppState>>,
    Extension(who): Extension<Identity>,
    Path(id): Path<String>,
    Json(b): Json<LinkBody>,
) -> ApiResult<Value> {
    let _w = s.write.lock().await;
    Ok(Json(json!(s.gate.link(&who, &id, &b.expr_id)?)))
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
    Extension(who): Extension<Identity>,
    Path(id): Path<String>,
    Json(b): Json<ReviewBody>,
) -> ApiResult<Job> {
    // Fail fast on bad input before spawning.
    let task = s.gate.store().load_task(&id)?;
    s.gate.ensure_owner(&task, &who)?;
    review::staged_from(std::path::Path::new(&task.repo), &b.tree, &b.base)?;
    let job = Job {
        id: format!("{id}-{}", s.next_job.fetch_add(1, Ordering::Relaxed)),
        task: id.clone(),
        status: "running".into(),
        record: None,
        error: None,
        unavailable: false,
        principal: who.principal.clone(),
    };
    s.jobs.lock().await.insert(job.id.clone(), job.clone());
    let state = s.clone();
    let job_id = job.id.clone();
    tokio::spawn(async move {
        let res = review::run(
            &state.gate,
            &who,
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

async fn job_status(
    State(s): State<Arc<AppState>>,
    Extension(who): Extension<Identity>,
    Path(job): Path<String>,
) -> ApiResult<Job> {
    let found = s.jobs.lock().await.get(&job).cloned();
    match found {
        Some(j) if who.admin || j.principal == who.principal => Ok(Json(j)),
        Some(_) => Err(ApiError(
            StatusCode::FORBIDDEN,
            format!("job {job} belongs to another principal"),
        )),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no such job {job}"),
        )),
    }
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
    Extension(who): Extension<Identity>,
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
    let task = s.gate.store().load_task(&id)?;
    s.gate.ensure_owner(&task, &who)?;
    Ok(Json(review::submit(
        &s.gate, &who, &id, &b.tree, &b.base, channel, &b.text,
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
    Extension(who): Extension<Identity>,
    Json(b): Json<CheckBody>,
) -> ApiResult<crate::gate::CommitCheck> {
    Ok(Json(s.gate.check_commit(&who, &b.repo, &b.tree, &b.base)?))
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
    Extension(who): Extension<Identity>,
    Path(id): Path<String>,
    Json(b): Json<RecordBody>,
) -> ApiResult<Value> {
    let _w = s.write.lock().await;
    let out = s
        .gate
        .record(&who, &id, &b.review, b.commit.as_deref(), b.tag.as_deref())?;
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
async fn guarded<T: Send + 'static>(
    s: &AppState,
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
    match guarded(&s, move |f| {
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
    match guarded(&s, move |f| f.request(&phase, &b.head)).await {
        Ok(id) => ok_json(json!({ "request_id": id })),
        Err(r) => *r,
    }
}

async fn ms_status(State(s): State<Arc<AppState>>, Path(phase): Path<String>) -> Response {
    if let Err(r) = check_phase(&phase) {
        return *r;
    }
    match guarded(&s, move |f| {
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
    match guarded(&s, move |f| f.cancel(&phase, &id)).await {
        Ok(()) => ok_json(json!({ "cancelled": true })),
        Err(r) => *r,
    }
}

async fn ms_log(State(s): State<Arc<AppState>>) -> Response {
    match guarded(&s, |f| f.log().map_err(FlowError::Other)).await {
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
    match guarded(&s, move |f| f.published_then_archive(b.seq, &b.hash)).await {
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
        let e = start_with_identities(bridge, IdentityConfig::legacy(TOKEN), TOKEN).await;
        e.client
            .post::<Value>(
                "/tasks",
                json!({ "id": "t", "goal": "g", "acceptance": "a", "iterations": 1, "repo": e.repo }),
            )
            .await
            .unwrap();
        e
    }

    pub(crate) async fn start_with_identities(
        bridge: &str,
        identities: IdentityConfig,
        client_token: &str,
    ) -> Env {
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
        let state = Arc::new(AppState::with_identities(gate, cfg(bridge), identities));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(state, listener));
        let client = Client::new(&url, client_token);
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

    fn three_principals() -> IdentityConfig {
        use crate::identity::token_hash;
        let text = json!({
            "version": 1,
            "principals": [
                {"id": "claude", "family": "claude", "credentials": [
                    {"id": "claude-main", "sha256": token_hash("claude-token")}]},
                {"id": "opencode", "family": "deepseek", "credentials": [
                    {"id": "opencode-main", "sha256": token_hash("opencode-token")}]},
                {"id": "codex", "family": "openai", "credentials": [
                    {"id": "codex-main", "sha256": token_hash("codex-token")}]}
            ]
        })
        .to_string();
        IdentityConfig::parse(&text).unwrap()
    }

    #[tokio::test]
    async fn three_credentials_freeze_their_principal_and_family_and_isolate_tasks() {
        let (bridge, _prompts) = mock_bridge(vec![Ok("VERDICT: APPROVED")]).await;
        let e = start_with_identities(&bridge, three_principals(), "claude-token").await;
        let claude = Client::new(&e.url, "claude-token");
        let opencode = Client::new(&e.url, "opencode-token");
        let codex = Client::new(&e.url, "codex-token");

        // In hashed mode the old plaintext token is not a fallback.
        let legacy = Client::new(&e.url, TOKEN);
        let err = legacy.get::<Value>("/tasks").await.unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");

        let created: Value = opencode
            .post(
                "/tasks",
                json!({ "id": "t", "goal": "g", "acceptance": "a", "iterations": 1,
                        "repo": e.repo, "implementer_family": "deepseek" }),
            )
            .await
            .unwrap();
        assert_eq!(created["principal"], "opencode");
        assert_eq!(created["implementer_family"], "deepseek");

        // A self-reported family that conflicts with the authenticated family is refused.
        let err = codex
            .post::<Value>(
                "/tasks",
                json!({ "id": "u", "goal": "g", "acceptance": "a", "iterations": 1,
                        "repo": e.repo, "implementer_family": "deepseek" }),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("409"), "{err}");

        // Cross-identity read and write are refused; the list shows only your own tasks.
        let err = codex
            .get::<Value>("/tasks/t")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("403"), "{err}");
        let err = codex
            .post::<Value>(
                "/tasks/t/reviews",
                json!({ "tree": "x", "base": "y", "evidence": "" }),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("403"), "{err}");
        let err = codex
            .post::<Value>("/tasks/t/link", json!({ "expr_id": "x1" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("403"), "{err}");
        let theirs: Vec<Value> = claude.get("/tasks").await.unwrap();
        assert!(theirs.is_empty(), "{theirs:?}");
        let own: Vec<Value> = opencode.get("/tasks").await.unwrap();
        assert_eq!(own.len(), 1);

        // A review job started by one principal is invisible to another.
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let job: Job = opencode
            .post(
                "/tasks/t/reviews",
                json!({ "tree": tree, "base": base, "evidence": "" }),
            )
            .await
            .unwrap();
        let err = codex
            .get::<Job>(&format!("/jobs/{}", job.id))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("403"), "{err}");
        let seen: Job = opencode.get(&format!("/jobs/{}", job.id)).await.unwrap();
        assert_eq!(seen.principal, "opencode");
    }

    #[tokio::test]
    async fn a_broken_identity_config_never_falls_back_to_the_legacy_token() {
        use crate::identity::token_hash;
        let text = json!({
            "version": 1,
            "principals": [{"id": "opencode", "family": "openai-compatible",
                "credentials": [{"id": "o", "sha256": token_hash("opencode-token")}]}]
        })
        .to_string();
        assert!(IdentityConfig::parse(&text).is_err());
        // And a valid map does not accept the legacy token (see the test above).
    }
}
