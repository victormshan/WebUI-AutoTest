//! HTTP API `/v1`. Every request except `/v1/health` carries `Authorization: Bearer <token>`; the
//! token identifies the agent, so the sender of a message is never taken from the request body.
//! The service keeps only SHA-256 hashes of the tokens.
//!
//! Long polling (`GET /v1/inbox?wait=…`) is for agents that keep a waiter running (Claude Code in
//! the background); an agent without one (the DSH main agent) is pushed a wake-up instead (V4).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::bridge::{Bridge, Draft};
use crate::inbox::Cursors;
use crate::model::{Message, State as TaskState};
use crate::{BridgeError, PROTOCOL};

/// Longest a single long poll may wait.
pub const MAX_WAIT_SECS: u64 = 120;
/// Most messages returned by one inbox call.
pub const INBOX_BATCH: usize = 100;

struct Inner {
    bridge: Bridge,
    cursors: Cursors,
}

pub struct AppState {
    inner: Mutex<Inner>,
    /// (agent, sha256(token))
    tokens: Vec<(String, [u8; 32])>,
    /// Woken whenever a message is stored.
    news: Notify,
}

pub fn sha256(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

impl AppState {
    /// `tokens`: agent → SHA-256 of its bearer token. Every agent of the bridge needs one.
    pub fn new(bridge: Bridge, tokens: Vec<(String, [u8; 32])>) -> anyhow::Result<Self> {
        for a in bridge.agents() {
            anyhow::ensure!(
                tokens.iter().any(|(t, _)| t == a),
                "no token configured for agent {a}"
            );
        }
        let cursors = Cursors::open(bridge.root(), bridge.agents().map(str::to_string))?;
        Ok(Self {
            inner: Mutex::new(Inner { bridge, cursors }),
            tokens,
            news: Notify::new(),
        })
    }

    /// Constant-time over all configured tokens.
    fn agent_for(&self, header_value: Option<&str>) -> Option<String> {
        let given = sha256(header_value?.strip_prefix("Bearer ")?);
        let mut found = None;
        for (agent, h) in &self.tokens {
            let diff = h
                .iter()
                .zip(given.iter())
                .fold(0u8, |acc, (x, y)| acc | (x ^ y));
            if diff == 0 {
                found = Some(agent.clone());
            }
        }
        found
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

pub struct ApiError(StatusCode, String);

impl From<BridgeError> for ApiError {
    fn from(e: BridgeError) -> Self {
        let code = match &e {
            BridgeError::InvalidId(_) | BridgeError::Invalid(_) | BridgeError::UnknownAgent(_) => {
                StatusCode::BAD_REQUEST
            }
            BridgeError::NoSuchTask(_) => StatusCode::NOT_FOUND,
            BridgeError::Exists(_) | BridgeError::Transition { .. } => StatusCode::CONFLICT,
            BridgeError::Forbidden(_) => StatusCode::FORBIDDEN,
            BridgeError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(code, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

#[derive(Clone)]
struct Caller(String);

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/v1/tasks", get(list_tasks).post(create_task))
        .route("/v1/tasks/{id}", get(show_task))
        .route("/v1/tasks/{id}/messages", post(post_message))
        .route("/v1/inbox", get(inbox))
        .route("/v1/inbox/ack", post(ack))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route("/v1/health", get(health))
        .merge(api)
        .layer(DefaultBodyLimit::max(2 << 20))
        .with_state(state)
}

pub async fn serve(state: Arc<AppState>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

async fn auth(State(s): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let h = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    match s.agent_for(h) {
        Some(agent) => {
            req.extensions_mut().insert(Caller(agent));
            next.run(req).await
        }
        None => ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or wrong bearer token".into(),
        )
        .into_response(),
    }
}

async fn health(State(s): State<Arc<AppState>>) -> Json<Value> {
    let g = s.lock();
    let open = g
        .bridge
        .list(None)
        .iter()
        .filter(|t| !t.state.terminal())
        .count();
    Json(json!({
        "ok": true, "service": "agent-bridge", "version": env!("CARGO_PKG_VERSION"), "protocol": PROTOCOL,
        "agents": g.bridge.agents().collect::<Vec<_>>(), "openTasks": open, "badLines": g.bridge.bad_lines,
    }))
}

#[derive(Deserialize)]
struct ListQuery {
    state: Option<TaskState>,
    #[serde(default)]
    open: bool,
}

async fn list_tasks(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Value> {
    let g = s.lock();
    let tasks: Vec<Value> = g
        .bridge
        .list(Some(&me))
        .into_iter()
        .filter(|t| q.state.is_none_or(|st| t.state == st) && (!q.open || !t.state.terminal()))
        .map(|t| {
            let mut v = serde_json::to_value(t).expect("serializable");
            v["read"] = json!(g.cursors.get(&me, &t.id));
            v
        })
        .collect();
    Ok(Json(json!({ "tasks": tasks })))
}

async fn show_task(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    let g = s.lock();
    let task = g
        .bridge
        .task(&id)
        .ok_or_else(|| BridgeError::NoSuchTask(id.clone()))?;
    if task.from != me && task.to != me {
        return Err(BridgeError::Forbidden(format!("{me} is not a party to task {id}")).into());
    }
    Ok(Json(
        json!({ "task": task, "messages": g.bridge.messages(&id)?, "read": g.cursors.get(&me, &id) }),
    ))
}

#[derive(Deserialize)]
struct CreateBody {
    id: String,
    #[serde(flatten)]
    draft: Draft,
}

async fn create_task(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Json(b): Json<CreateBody>,
) -> ApiResult<Message> {
    let m = s.lock().bridge.create_task(&b.id, &me, b.draft)?;
    s.news.notify_waiters();
    Ok(Json(m))
}

async fn post_message(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Path(id): Path<String>,
    Json(d): Json<Draft>,
) -> ApiResult<Message> {
    let m = s.lock().bridge.post(&id, &me, d)?;
    s.news.notify_waiters();
    Ok(Json(m))
}

/// Messages addressed to `me` (sent by the other party) beyond `me`'s read cursor, oldest first.
fn unread(g: &Inner, me: &str) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for t in g.bridge.list(Some(me)) {
        let read = g.cursors.get(me, &t.id);
        if read >= t.last_n {
            continue;
        }
        if let Ok(ms) = g.bridge.messages(&t.id) {
            out.extend(ms.iter().filter(|m| m.n > read && m.from != me).cloned());
        }
    }
    out.sort_by(|a, b| a.at.cmp(&b.at).then(a.n.cmp(&b.n)));
    out.truncate(INBOX_BATCH);
    out
}

#[derive(Deserialize)]
struct InboxQuery {
    #[serde(default)]
    wait: u64,
}

async fn inbox(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Query(q): Query<InboxQuery>,
) -> Json<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(q.wait.min(MAX_WAIT_SECS));
    loop {
        // Register interest before looking, so a message stored in between is not missed.
        let news = s.news.notified();
        tokio::pin!(news);
        news.as_mut().enable();
        let msgs = unread(&s.lock(), &me);
        if !msgs.is_empty() {
            return Json(json!({ "messages": msgs }));
        }
        if tokio::time::timeout_at(deadline, news).await.is_err() {
            // Nothing yet is not an error (design b8): empty list and when to ask again.
            return Json(json!({ "messages": [], "retryAfter": 1 }));
        }
    }
}

#[derive(Deserialize)]
struct AckBody {
    task: String,
    n: u32,
}

async fn ack(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Json(b): Json<AckBody>,
) -> ApiResult<Value> {
    let mut g = s.lock();
    let t = g
        .bridge
        .task(&b.task)
        .ok_or_else(|| BridgeError::NoSuchTask(b.task.clone()))?;
    if t.from != me && t.to != me {
        return Err(
            BridgeError::Forbidden(format!("{me} is not a party to task {}", b.task)).into(),
        );
    }
    if b.n == 0 || b.n > t.last_n {
        return Err(
            BridgeError::Invalid(format!("task {} has messages 1..={}", b.task, t.last_n)).into(),
        );
    }
    let advanced = g.cursors.ack(&me, &b.task, b.n).map_err(BridgeError::Io)?;
    Ok(Json(
        json!({ "task": b.task, "read": g.cursors.get(&me, &b.task), "advanced": advanced }),
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const CLAUDE: &str = "claude-token-0123456789abcdef0123456789";
    pub(crate) const DSH: &str = "dsh-token-0123456789abcdef0123456789abcd";

    pub(crate) struct Env {
        pub(crate) _dir: tempfile::TempDir,
        pub(crate) url: String,
        pub(crate) http: reqwest::Client,
    }

    pub(crate) async fn start_at(dir: tempfile::TempDir) -> Env {
        let bridge = Bridge::open(dir.path().join("state"), &["claude", "dsh"]).unwrap();
        let state = Arc::new(
            AppState::new(
                bridge,
                vec![
                    ("claude".into(), sha256(CLAUDE)),
                    ("dsh".into(), sha256(DSH)),
                ],
            )
            .unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(state, listener));
        Env {
            _dir: dir,
            url,
            http: reqwest::Client::new(),
        }
    }

    pub(crate) async fn start() -> Env {
        start_at(tempfile::tempdir().unwrap()).await
    }

    impl Env {
        pub(crate) async fn call(
            &self,
            token: &str,
            method: reqwest::Method,
            path: &str,
            body: Option<Value>,
        ) -> (u16, Value) {
            let mut r = self
                .http
                .request(method, format!("{}{path}", self.url))
                .bearer_auth(token);
            if let Some(b) = body {
                r = r.json(&b);
            }
            let resp = r.send().await.unwrap();
            (
                resp.status().as_u16(),
                resp.json().await.unwrap_or(Value::Null),
            )
        }
        pub(crate) async fn post(&self, token: &str, path: &str, body: Value) -> (u16, Value) {
            self.call(token, reqwest::Method::POST, path, Some(body))
                .await
        }
        pub(crate) async fn get(&self, token: &str, path: &str) -> (u16, Value) {
            self.call(token, reqwest::Method::GET, path, None).await
        }
    }

    fn task_body(id: &str) -> Value {
        json!({ "id": id, "kind": "task", "protocol": PROTOCOL, "body": "please", "meta": { "to": "dsh", "title": "do it" } })
    }

    #[tokio::test]
    async fn health_is_open_and_everything_else_needs_a_valid_token() {
        let e = start().await;
        let r = reqwest::get(format!("{}/v1/health", e.url)).await.unwrap();
        assert_eq!(r.status(), 200);
        let h: Value = r.json().await.unwrap();
        assert_eq!(
            (h["service"].as_str(), h["protocol"].as_str()),
            (Some("agent-bridge"), Some(PROTOCOL))
        );
        assert_eq!(
            reqwest::get(format!("{}/v1/tasks", e.url))
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(e.get("wrong-token", "/v1/tasks").await.0, 401);
        assert_eq!(e.get(CLAUDE, "/v1/tasks").await.0, 200);
    }

    #[tokio::test]
    async fn the_token_decides_who_is_speaking() {
        let e = start().await;
        let (st, m) = e.post(CLAUDE, "/v1/tasks", task_body("t1")).await;
        assert_eq!(st, 200, "{m}");
        assert_eq!(
            (m["from"].as_str(), m["n"].as_u64()),
            (Some("claude"), Some(1))
        );
        // A body claiming to be someone else changes nothing: the ack comes from the token's owner.
        let (st, _) = e
            .post(
                CLAUDE,
                "/v1/tasks/t1/messages",
                json!({ "kind": "ack", "from": "dsh", "protocol": PROTOCOL }),
            )
            .await;
        assert_eq!(st, 403, "the requester cannot acknowledge its own task");
        let (st, m) = e
            .post(
                DSH,
                "/v1/tasks/t1/messages",
                json!({ "kind": "ack", "protocol": PROTOCOL }),
            )
            .await;
        assert_eq!((st, m["from"].as_str()), (200, Some("dsh")));
    }

    #[tokio::test]
    async fn errors_map_to_status_codes() {
        let e = start().await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        assert_eq!(
            e.post(CLAUDE, "/v1/tasks", task_body("t")).await.0,
            409,
            "exists"
        );
        assert_eq!(e.post(CLAUDE, "/v1/tasks", task_body("../t")).await.0, 400);
        assert_eq!(
            e.post(
                DSH,
                "/v1/tasks/t/messages",
                json!({ "kind": "progress", "protocol": PROTOCOL })
            )
            .await
            .0,
            409,
            "ack first"
        );
        assert_eq!(
            e.post(
                DSH,
                "/v1/tasks/t/messages",
                json!({ "kind": "ack", "protocol": "0" })
            )
            .await
            .0,
            400
        );
        assert_eq!(
            e.post(
                DSH,
                "/v1/tasks/nope/messages",
                json!({ "kind": "ack", "protocol": PROTOCOL })
            )
            .await
            .0,
            404
        );
        assert_eq!(e.get(DSH, "/v1/tasks/nope").await.0, 404);
        assert_eq!(
            e.post(DSH, "/v1/inbox/ack", json!({ "task": "t", "n": 5 }))
                .await
                .0,
            400
        );
    }

    #[tokio::test]
    async fn long_poll_returns_as_soon_as_a_message_arrives_and_ack_is_explicit() {
        let e = start().await;
        // Nothing yet: an empty list, not an error.
        let (st, r) = e.get(DSH, "/v1/inbox?wait=0").await;
        assert_eq!(
            (
                st,
                r["messages"].as_array().unwrap().len(),
                r["retryAfter"].as_u64()
            ),
            (200, 0, Some(1))
        );

        let waiter = {
            let (url, http) = (e.url.clone(), e.http.clone());
            tokio::spawn(async move {
                let t0 = std::time::Instant::now();
                let r: Value = http
                    .get(format!("{url}/v1/inbox?wait=30"))
                    .bearer_auth(DSH)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                (t0.elapsed(), r)
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        let (took, r) = waiter.await.unwrap();
        assert!(
            took < Duration::from_secs(5),
            "returned on arrival, took {took:?}"
        );
        assert_eq!(r["messages"][0]["task"], "t");

        // Reading does not mark read: the same message comes again until acknowledged.
        assert_eq!(
            e.get(DSH, "/v1/inbox").await.1["messages"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let (st, a) = e
            .post(DSH, "/v1/inbox/ack", json!({ "task": "t", "n": 1 }))
            .await;
        assert_eq!((st, a["advanced"].as_bool()), (200, Some(true)));
        assert_eq!(
            e.get(DSH, "/v1/inbox").await.1["messages"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        // Own messages are never in one's inbox; the reply shows up for the requester.
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "ack", "protocol": PROTOCOL }),
        )
        .await;
        assert_eq!(
            e.get(DSH, "/v1/inbox").await.1["messages"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        let mine = e.get(CLAUDE, "/v1/inbox").await.1;
        assert_eq!(
            (
                mine["messages"].as_array().unwrap().len(),
                mine["messages"][0]["kind"].as_str()
            ),
            (1, Some("ack"))
        );
        let tasks = e.get(CLAUDE, "/v1/tasks?open=true").await.1;
        assert_eq!(
            (
                tasks["tasks"][0]["state"].as_str(),
                tasks["tasks"][0]["read"].as_u64()
            ),
            (Some("acked"), Some(0))
        );
    }

    #[tokio::test]
    async fn state_and_read_cursors_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let e = start_at(dir).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/inbox/ack", json!({ "task": "t", "n": 1 }))
            .await;
        let Env { _dir, .. } = e;
        let e2 = start_at(_dir).await;
        assert!(path.join("state/cursors/dsh.json").exists());
        assert_eq!(
            e2.get(DSH, "/v1/inbox").await.1["messages"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "ack survived"
        );
        assert_eq!(e2.get(DSH, "/v1/tasks/t").await.1["task"]["state"], "open");
    }
}
