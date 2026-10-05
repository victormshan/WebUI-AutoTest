//! HTTP API `/v1`. Every request except `/v1/health` carries `Authorization: Bearer <token>`; the
//! token identifies the agent, so the sender of a message is never taken from the request body.
//! The service keeps only SHA-256 hashes of the tokens.
//!
//! Long polling (`GET /v1/inbox?wait=…`) is for agents that keep a waiter running (Claude Code in
//! the background); an agent without one (the DSH main agent) is pushed a wake-up instead
//! (`notify.rs`). A monitor re-wakes an agent that has not read a blocking message after 15
//! minutes, and marks a task stalled after a day without progress. Every push attempt, woken or
//! not, is appended to `wakes.jsonl` and summarised in `/v1/health`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

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
use crate::mirror::{Copier, Mirror};
use crate::model::{Message, State as TaskState};
use crate::notify::{self, Target};
use crate::user::{self, Decisions, ItemStatus};
use crate::{BridgeError, PROTOCOL};

/// Longest a single long poll may wait.
pub const MAX_WAIT_SECS: u64 = 120;
/// Most messages returned by one inbox call.
pub const INBOX_BATCH: usize = 100;

/// When the monitor acts (shortened in tests).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub tick: Duration,
    /// Re-wake once if a blocking message is still unread after this long.
    pub rewake_after: chrono::Duration,
    /// A task without progress for this long is stalled.
    pub stall_after: chrono::Duration,
    /// At most one stall notice per task per this long.
    pub stall_cooldown: chrono::Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(60),
            rewake_after: chrono::Duration::minutes(15),
            stall_after: chrono::Duration::hours(24),
            stall_cooldown: chrono::Duration::hours(24),
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
struct WakeStats {
    count: u64,
    woken: u64,
    failed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last: Option<Value>,
}

struct Inner {
    bridge: Bridge,
    cursors: Cursors,
    decisions: Decisions,
    /// (task, n) already re-woken once (in memory: after a restart one more re-wake is allowed).
    rewoken: BTreeSet<(String, u32)>,
    stalled: BTreeSet<String>,
    stall_notice: BTreeMap<String, DateTime<Utc>>,
    /// Long polls currently waiting, per agent.
    waiters: BTreeMap<String, usize>,
    wakes: WakeStats,
}

pub struct AppState {
    inner: Mutex<Inner>,
    /// (agent, sha256(token))
    tokens: Vec<(String, [u8; 32])>,
    /// Woken whenever a message is stored.
    news: Notify,
    /// Agents woken by push (agent → notify target).
    targets: BTreeMap<String, Target>,
    timing: Timing,
    ledger: PathBuf,
    /// All copy I/O (mirror files, step-relay notes) on one thread, outside the service lock.
    copier: Copier,
    /// claude-step-relay data dir: messages of tasks with an exprId are noted in its trace.
    step_relay: Option<PathBuf>,
}

/// One push to make, prepared under the lock and sent outside it.
struct Push {
    agent: String,
    task: String,
    n: u32,
    kind: String,
    why: &'static str,
    summary: String,
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
        let decisions = Decisions::open(bridge.root())?;
        let ledger = bridge.root().join("wakes.jsonl");
        Ok(Self {
            inner: Mutex::new(Inner {
                bridge,
                cursors,
                decisions,
                rewoken: BTreeSet::new(),
                stalled: BTreeSet::new(),
                stall_notice: BTreeMap::new(),
                waiters: BTreeMap::new(),
                wakes: WakeStats::default(),
            }),
            tokens,
            news: Notify::new(),
            targets: BTreeMap::new(),
            timing: Timing::default(),
            ledger,
            copier: Copier::spawn(None),
            step_relay: None,
        })
    }

    /// Writes a read-only copy of every message under `dir` (see `mirror.rs`).
    pub fn with_mirror(mut self, dir: impl Into<PathBuf>) -> Self {
        self.copier = Copier::spawn(Some(Mirror::new(dir)));
        self
    }

    pub fn with_step_relay(mut self, dir: impl Into<PathBuf>) -> Self {
        self.step_relay = Some(dir.into());
        self
    }

    /// Writes mirror files that are missing (imported messages keep their original files) and
    /// runs the consistency self-check, on the copier thread: only a snapshot is taken under the
    /// lock. Called at start-up and on every monitor pass; the receiver fires when it is done.
    pub fn sync_mirror(&self) -> std::sync::mpsc::Receiver<()> {
        let snapshot: Vec<Message> = {
            let g = self.lock();
            let ids: Vec<String> = g.bridge.list(None).iter().map(|t| t.id.clone()).collect();
            ids.iter()
                .flat_map(|id| g.bridge.messages(id).unwrap_or(&[]).to_vec())
                .collect()
        };
        self.copier.sync(snapshot)
    }

    /// Agents without a waiter of their own are woken by push.
    pub fn with_push(mut self, targets: BTreeMap<String, Target>) -> Self {
        self.targets = targets;
        self
    }

    pub fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    /// Runs the monitor (re-wakes, stalls) until the process ends.
    pub fn spawn_monitor(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let s = self.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(s.timing.tick);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                iv.tick().await;
                s.tick(Utc::now());
                let _ = s.sync_mirror();
            }
        })
    }

    /// One monitor pass at `now`.
    pub fn tick(self: &Arc<Self>, now: DateTime<Utc>) {
        let mut pushes = Vec::new();
        {
            let mut g = self.lock();
            let mut stalled_now = BTreeSet::new();
            let tasks: Vec<crate::model::Task> = g
                .bridge
                .list(None)
                .into_iter()
                .filter(|t| !t.state.terminal())
                .cloned()
                .collect();
            for t in tasks {
                let Some(last) = g
                    .bridge
                    .messages(&t.id)
                    .ok()
                    .and_then(|ms| ms.last().cloned())
                else {
                    continue;
                };
                let to = if last.from == t.from {
                    t.to.clone()
                } else {
                    t.from.clone()
                };
                if last.kind.wakes()
                    && g.cursors.get(&to, &t.id) < last.n
                    && now - last.at >= self.timing.rewake_after
                    && self.targets.contains_key(&to)
                    && g.rewoken.insert((t.id.clone(), last.n))
                {
                    pushes.push(push_for(&t, &last, &to, "rewake"));
                }
                if now - t.updated_at >= self.timing.stall_after {
                    stalled_now.insert(t.id.clone());
                    let due = g
                        .stall_notice
                        .get(&t.id)
                        .is_none_or(|at| now - *at >= self.timing.stall_cooldown);
                    if due {
                        g.stall_notice.insert(t.id.clone(), now);
                        for party in [&t.from, &t.to] {
                            if self.targets.contains_key(party) {
                                let mut p = push_for(&t, &last, party, "stalled");
                                p.kind = "stalled".into();
                                pushes.push(p);
                            }
                        }
                    }
                }
            }
            g.stalled = stalled_now;
        }
        for p in pushes {
            self.send_push(p);
        }
    }

    /// After a message is stored: wake long polls, and push to the recipient if it needs one.
    fn stored(self: &Arc<Self>, m: &Message) {
        self.news.notify_waiters();
        self.record_copies(m);
        if !m.kind.wakes() {
            return;
        }
        let push = {
            let g = self.lock();
            g.bridge.task(&m.task).map(|t| {
                let to = if m.from == t.from {
                    t.to.clone()
                } else {
                    t.from.clone()
                };
                push_for(t, m, &to, "new")
            })
        };
        if let Some(p) = push.filter(|p| self.targets.contains_key(&p.agent)) {
            self.send_push(p);
        }
    }

    /// Mirror file and step-relay trace entry for a stored message: queued to the copier
    /// (failures are counted and reported there, never allowed to fail the request).
    fn record_copies(self: &Arc<Self>, m: &Message) {
        let expr = self
            .lock()
            .bridge
            .task(&m.task)
            .and_then(|t| t.expr_id.clone());
        self.copier.mirror(m.clone());
        if let (Some(dir), Some(expr)) = (&self.step_relay, expr) {
            self.copier.trace(dir.clone(), expr, m.clone());
        }
    }

    fn send_push(self: &Arc<Self>, p: Push) {
        let Some(target) = self.targets.get(&p.agent).cloned() else {
            return;
        };
        let s = self.clone();
        tokio::spawn(async move {
            let o = notify::wake(&target, &p.task, p.n, &p.kind, &p.summary).await;
            let entry = json!({
                "at": Utc::now(), "agent": p.agent, "task": p.task, "n": p.n, "kind": p.kind, "why": p.why,
                "agentWoken": o.agent_woken, "coalesced": o.coalesced, "status": o.status,
                "transport": o.transport, "reason": o.reason,
            });
            if let Err(e) = s.append_ledger(&entry) {
                eprintln!("agent-bridge: wake ledger write failed: {e:#}");
            }
            if !o.agent_woken {
                eprintln!(
                    "agent-bridge: wake {} for {}#{} not delivered: {}",
                    p.agent, p.task, p.n, o.reason
                );
            }
            let mut g = s.lock();
            g.wakes.count += 1;
            if o.agent_woken {
                g.wakes.woken += 1
            } else {
                g.wakes.failed += 1
            }
            g.wakes.last = Some(entry);
        });
    }

    fn append_ledger(&self, entry: &Value) -> anyhow::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&self.ledger)?;
        f.write_all(format!("{entry}\n").as_bytes())?;
        f.sync_all()?;
        Ok(())
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

fn push_for(t: &crate::model::Task, m: &Message, to: &str, why: &'static str) -> Push {
    let body: String = m.body.chars().take(200).collect();
    let summary = format!(
        "{}｜{} 来自 {}：{}｜读取：GET /v1/tasks/{}（agent-bridge 127.0.0.1:7879）",
        t.title, m.kind, m.from, body, t.id
    );
    Push {
        agent: to.into(),
        task: t.id.clone(),
        n: m.n,
        kind: m.kind.to_string(),
        why,
        summary,
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
        .route("/v1/user/pending", get(user_pending))
        .route("/v1/user/decisions", post(user_decision))
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
    let agents: Vec<String> = g.bridge.agents().map(str::to_string).collect();
    let unread_by: BTreeMap<&str, usize> = agents
        .iter()
        .map(|a| (a.as_str(), unread(&g, a).len()))
        .collect();
    let pending = user::pending(&g.bridge, &g.decisions, &g.stalled, false).len();
    Json(json!({
        "ok": true, "service": "agent-bridge", "version": env!("CARGO_PKG_VERSION"), "protocol": PROTOCOL,
        "agents": agents, "openTasks": open, "badLines": g.bridge.bad_lines + g.decisions.bad_lines,
        "waiters": g.waiters, "unread": unread_by, "pendingForUser": pending, "stalled": g.stalled,
        "push": s.targets.keys().collect::<Vec<_>>(), "wakes": g.wakes,
        "mirror": s.copier.mirror_dir().map(|d| json!({ "dir": d, "stats": s.copier.stats() })),
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
    let fresh = !s.lock().bridge.task(&b.id).is_some();
    let m = s.lock().bridge.create_task(&b.id, &me, b.draft)?;
    if fresh {
        s.stored(&m);
    }
    Ok(Json(m))
}

async fn post_message(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Path(id): Path<String>,
    Json(d): Json<Draft>,
) -> ApiResult<Message> {
    let before = s.lock().bridge.task(&id).map(|t| t.last_n);
    let m = s.lock().bridge.post(&id, &me, d)?;
    // A retried post returns the stored message: no second wake for it.
    if before.is_some_and(|n| m.n > n) {
        s.stored(&m);
    }
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
    // Count this agent's live waiters while we wait (visible in /v1/health).
    struct Waiting(Arc<AppState>, String);
    impl Drop for Waiting {
        fn drop(&mut self) {
            if let Some(c) = self.0.lock().waiters.get_mut(&self.1) {
                *c = c.saturating_sub(1);
            }
        }
    }
    let _waiting = (q.wait > 0).then(|| {
        *s.lock().waiters.entry(me.clone()).or_default() += 1;
        Waiting(s.clone(), me.clone())
    });
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

#[derive(Deserialize)]
struct PendingQuery {
    #[serde(default)]
    all: bool,
}

/// What only the user may decide, across all tasks (either agent relays it to the user).
async fn user_pending(
    State(s): State<Arc<AppState>>,
    Query(q): Query<PendingQuery>,
) -> Json<Value> {
    let g = s.lock();
    Json(json!({ "items": user::pending(&g.bridge, &g.decisions, &g.stalled, q.all) }))
}

#[derive(Deserialize)]
struct DecisionBody {
    item: String,
    /// The user's own words, verbatim.
    verbatim: String,
}

/// Records the user's decision as relayed by the caller; a paused task resumes on it.
async fn user_decision(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Json(b): Json<DecisionBody>,
) -> Result<Response, ApiError> {
    if b.verbatim.trim().is_empty() {
        return Err(BridgeError::Invalid(
            "a decision is the user's own words; verbatim cannot be empty".into(),
        )
        .into());
    }
    let (status, decision, resumed, task) = {
        let mut g = s.lock();
        let all = user::pending(&g.bridge, &g.decisions, &g.stalled, true);
        let Some(item) = all.into_iter().find(|i| i.item == b.item) else {
            return Err(BridgeError::Invalid(format!(
                "no pending item {:?} (see GET /v1/user/pending?all=true)",
                b.item
            ))
            .into());
        };
        let t = g
            .bridge
            .task(&item.task)
            .cloned()
            .ok_or_else(|| BridgeError::NoSuchTask(item.task.clone()))?;
        if t.from != me && t.to != me {
            return Err(
                BridgeError::Forbidden(format!("{me} is not a party to task {}", t.id)).into(),
            );
        }
        let (status, decision) = g
            .decisions
            .record(&b.item, &b.verbatim, &me)
            .map_err(BridgeError::Io)?;
        let resumed = if item.source == "paused"
            && status == ItemStatus::Decided
            && t.state == TaskState::PausedForUser
        {
            Some(g.bridge.resume(&t.id, &me, decision.verbatim.as_str())?)
        } else {
            None
        };
        (status, decision, resumed, t)
    };
    if let Some(m) = &resumed {
        s.stored(m);
    } else {
        // Tell the other party a decision exists (push if it has a target; long polls see /v1/user/pending).
        let other = if me == task.from {
            task.to.clone()
        } else {
            task.from.clone()
        };
        let last = s
            .lock()
            .bridge
            .messages(&task.id)
            .ok()
            .and_then(|ms| ms.last().cloned());
        if let Some(last) = last {
            let mut p = push_for(&task, &last, &other, "decision");
            p.kind = "user_decision".into();
            p.summary = format!(
                "用户对 {} 做了决定（{}转达）：{}",
                b.item,
                me,
                decision.verbatim.chars().take(200).collect::<String>()
            );
            s.send_push(p);
        }
    }
    let code = if status == ItemStatus::Conflict {
        StatusCode::CONFLICT
    } else {
        StatusCode::OK
    };
    Ok((
        code,
        Json(json!({ "item": b.item, "status": status, "decision": decision, "resumed": resumed })),
    )
        .into_response())
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

    /// A stand-in for dsh-web-relay's notify endpoint: records each call, answers `woken`.
    pub(crate) async fn mock_notify(woken: bool) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
        let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let c = calls.clone();
        let app = Router::new().route(
            "/notify",
            post(
                move |headers: axum::http::HeaderMap, Json(v): Json<Value>| {
                    let c = c.clone();
                    async move {
                        let auth = headers
                            .get(header::AUTHORIZATION)
                            .and_then(|h| h.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        c.lock().unwrap().push((auth, v));
                        Json(json!({ "ok": true, "agentWoken": woken, "coalesced": false,
                                 "reason": if woken { "woke" } else { "sessionId 缺失" } }))
                    }
                },
            ),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/notify", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, calls)
    }

    /// Service with DSH woken by push at `notify_url`, and the given timing; returns the state
    /// too so tests can drive the monitor with `tick`.
    pub(crate) async fn start_push(notify_url: &str, timing: Timing) -> (Env, Arc<AppState>) {
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("dsh-notify.token");
        std::fs::write(&token_file, "notify-token-0123456789abcdef0123456789\n").unwrap();
        let bridge = Bridge::open(dir.path().join("state"), &["claude", "dsh"]).unwrap();
        let targets = BTreeMap::from([(
            "dsh".to_string(),
            Target {
                url: notify_url.into(),
                token_file,
                transport: notify::Transport::Native,
            },
        )]);
        let state = Arc::new(
            AppState::new(
                bridge,
                vec![
                    ("claude".into(), sha256(CLAUDE)),
                    ("dsh".into(), sha256(DSH)),
                ],
            )
            .unwrap()
            .with_push(targets)
            .with_timing(timing),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(state.clone(), listener));
        (
            Env {
                _dir: dir,
                url,
                http: reqwest::Client::new(),
            },
            state,
        )
    }

    pub(crate) async fn settle() {
        tokio::time::sleep(Duration::from_millis(300)).await;
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

    fn q() -> Value {
        json!({ "kind": "question", "protocol": PROTOCOL, "questions": [{ "id": "q", "text": "?", "blocking": true }] })
    }
    fn k(kind: &str) -> Value {
        json!({ "kind": kind, "protocol": PROTOCOL })
    }

    #[tokio::test]
    async fn new_messages_push_dsh_and_every_attempt_is_recorded() {
        let (notify_url, calls) = mock_notify(true).await;
        let (e, _s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await; // ack does not wake anyone
        e.post(DSH, "/v1/tasks/t/messages", q()).await; // to claude: long poll, not push
        e.post(CLAUDE, "/v1/tasks/t/messages", k("answer")).await; // to dsh: push
        settle().await;
        let c = calls.lock().unwrap().clone();
        assert_eq!(c.len(), 2, "task and answer were pushed to dsh: {c:?}");
        assert_eq!(c[0].0, "Bearer notify-token-0123456789abcdef0123456789");
        assert_eq!(
            (
                c[0].1["taskId"].as_str(),
                c[0].1["n"].as_u64(),
                c[0].1["kind"].as_str()
            ),
            (Some("t"), Some(1), Some("task"))
        );
        assert!(
            c[0].1["summary"]
                .as_str()
                .unwrap()
                .contains("GET /v1/tasks/t")
        );
        assert_eq!(c[1].1["kind"], "answer");
        let h = e.get(CLAUDE, "/v1/health").await.1;
        assert_eq!(
            (h["wakes"]["count"].as_u64(), h["wakes"]["woken"].as_u64()),
            (Some(2), Some(2))
        );
        let ledger = std::fs::read_to_string(e._dir.path().join("state/wakes.jsonl")).unwrap();
        assert_eq!(ledger.lines().count(), 2);
        // A retried post (same client_msg_id) is not pushed again.
        let body = json!({ "kind": "progress", "protocol": PROTOCOL, "client_msg_id": "p1" });
        e.post(DSH, "/v1/tasks/t/messages", body.clone()).await;
        e.post(DSH, "/v1/tasks/t/messages", body).await;
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "result", "protocol": PROTOCOL, "outcome": "done" }),
        )
        .await;
        e.post(
            CLAUDE,
            "/v1/tasks/t/messages",
            json!({ "kind": "verdict", "protocol": PROTOCOL, "judgement": "pass" }),
        )
        .await;
        settle().await;
        assert_eq!(
            calls.lock().unwrap().len(),
            3,
            "only the verdict added a push"
        );
    }

    #[tokio::test]
    async fn a_failed_wake_is_reported_not_counted_as_woken() {
        let (notify_url, _calls) = mock_notify(false).await;
        let (e, _s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        settle().await;
        let h = e.get(CLAUDE, "/v1/health").await.1;
        assert_eq!(
            (h["wakes"]["woken"].as_u64(), h["wakes"]["failed"].as_u64()),
            (Some(0), Some(1))
        );
        assert_eq!(h["wakes"]["last"]["reason"], "sessionId 缺失");
        let (e2, _s2) = start_push("http://127.0.0.1:9/notify", Timing::default()).await;
        e2.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        settle().await;
        let h = e2.get(CLAUDE, "/v1/health").await.1;
        assert_eq!(
            h["wakes"]["failed"].as_u64(),
            Some(1),
            "unreachable endpoint is a failed wake"
        );
        assert!(h["wakes"]["last"]["status"].is_null());
    }

    #[tokio::test]
    async fn monitor_rewakes_once_and_marks_stalls() {
        let (notify_url, calls) = mock_notify(true).await;
        let timing = Timing {
            tick: Duration::from_secs(3600),
            rewake_after: chrono::Duration::minutes(15),
            stall_after: chrono::Duration::hours(24),
            stall_cooldown: chrono::Duration::hours(24),
        };
        let (e, s) = start_push(&notify_url, timing).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        settle().await;
        let now = Utc::now();
        s.tick(now + chrono::Duration::minutes(5));
        settle().await;
        assert_eq!(calls.lock().unwrap().len(), 1, "too early to re-wake");
        s.tick(now + chrono::Duration::minutes(16));
        s.tick(now + chrono::Duration::minutes(30));
        settle().await;
        assert_eq!(calls.lock().unwrap().len(), 2, "re-woken exactly once");
        assert_eq!(calls.lock().unwrap()[1].1["n"], 1);

        // Read (acked) messages are not re-woken.
        e.post(CLAUDE, "/v1/tasks", json!({ "id": "u", "kind": "task", "protocol": PROTOCOL, "meta": { "to": "dsh", "title": "u" } })).await;
        e.post(DSH, "/v1/inbox/ack", json!({ "task": "u", "n": 1 }))
            .await;
        settle().await;
        let before = calls.lock().unwrap().len();
        s.tick(now + chrono::Duration::minutes(20));
        settle().await;
        assert_eq!(calls.lock().unwrap().len(), before);

        // A day without progress: stalled, the user sees it, and DSH is told once per cooldown.
        s.tick(now + chrono::Duration::hours(25));
        s.tick(now + chrono::Duration::hours(26));
        settle().await;
        let stalled: Vec<Value> = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.1["kind"] == "stalled")
            .map(|c| c.1.clone())
            .collect();
        assert_eq!(
            stalled.len(),
            2,
            "one notice for each stalled task: {stalled:?}"
        );
        let pending = e.get(CLAUDE, "/v1/user/pending").await.1;
        let items: Vec<&str> = pending["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["source"].as_str().unwrap())
            .collect();
        assert_eq!(items, ["stalled", "stalled"]);
        assert_eq!(
            e.get(CLAUDE, "/v1/health").await.1["stalled"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        // Stalls are re-evaluated on every pass: at the real present nothing is a day old.
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        s.tick(Utc::now());
        assert_eq!(
            e.get(CLAUDE, "/v1/health").await.1["stalled"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            e.get(CLAUDE, "/v1/user/pending").await.1["items"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn the_users_decision_is_recorded_verbatim_and_resumes_a_paused_task() {
        let e = start().await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        for _ in 0..3 {
            e.post(DSH, "/v1/tasks/t/messages", q()).await;
            e.post(CLAUDE, "/v1/tasks/t/messages", k("answer")).await;
        }
        e.post(DSH, "/v1/tasks/t/messages", json!({ "kind": "question", "protocol": PROTOCOL,
            "questions": [{ "id": "q4", "text": "?", "blocking": true }], "needs_user": ["要不要重启宿主？"] })).await;
        assert_eq!(
            e.get(CLAUDE, "/v1/tasks/t").await.1["task"]["state"],
            "paused_for_user"
        );
        let pending = e.get(CLAUDE, "/v1/user/pending").await.1["items"]
            .as_array()
            .unwrap()
            .clone();
        let ids: Vec<&str> = pending
            .iter()
            .map(|i| i["item"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["t#9#1", "t#paused#9"]);
        assert!(
            pending[0]["text"]
                .as_str()
                .unwrap()
                .contains("[dsh] 要不要重启宿主")
        );

        // Agents cannot resume on their own.
        assert_eq!(
            e.post(
                CLAUDE,
                "/v1/tasks/t/messages",
                json!({ "kind": "resume", "protocol": PROTOCOL, "body": "ok" })
            )
            .await
            .0,
            403
        );
        assert_eq!(
            e.post(
                CLAUDE,
                "/v1/user/decisions",
                json!({ "item": "t#nope", "verbatim": "x" })
            )
            .await
            .0,
            400
        );
        assert_eq!(
            e.post(
                CLAUDE,
                "/v1/user/decisions",
                json!({ "item": "t#paused#9", "verbatim": "  " })
            )
            .await
            .0,
            400,
            "empty words refused"
        );

        let (st, r) = e
            .post(
                CLAUDE,
                "/v1/user/decisions",
                json!({ "item": "t#paused#9", "verbatim": "可以，继续问" }),
            )
            .await;
        assert_eq!((st, r["status"].as_str()), (200, Some("decided")));
        assert_eq!(r["resumed"]["kind"], "resume");
        assert!(
            r["resumed"]["body"]
                .as_str()
                .unwrap()
                .contains("可以，继续问")
        );
        let t = e.get(DSH, "/v1/tasks/t").await.1["task"].clone();
        assert_eq!(
            (t["state"].as_str(), t["paused_reason"].is_null()),
            (Some("working"), true)
        );
        // Limits count afresh: three more rounds are possible.
        for _ in 0..3 {
            assert_eq!(e.post(DSH, "/v1/tasks/t/messages", q()).await.0, 200);
            e.post(CLAUDE, "/v1/tasks/t/messages", k("answer")).await;
        }

        // A different text for an already decided item is kept and flagged.
        let (st, r) = e
            .post(
                DSH,
                "/v1/user/decisions",
                json!({ "item": "t#9#1", "verbatim": "不要重启" }),
            )
            .await;
        assert_eq!((st, r["status"].as_str()), (200, Some("decided")));
        let (st, r) = e
            .post(
                CLAUDE,
                "/v1/user/decisions",
                json!({ "item": "t#9#1", "verbatim": "现在重启" }),
            )
            .await;
        assert_eq!((st, r["status"].as_str()), (409, Some("conflict")));
        let all = e.get(CLAUDE, "/v1/user/pending?all=true").await.1;
        let item = all["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["item"] == "t#9#1")
            .unwrap()
            .clone();
        assert_eq!(
            (
                item["status"].as_str(),
                item["decisions"].as_array().unwrap().len()
            ),
            (Some("conflict"), 2)
        );
        assert_eq!(item["decisions"][0]["relayed_by"], "dsh");
    }

    #[tokio::test]
    async fn health_counts_live_waiters() {
        let e = start().await;
        let (url, http) = (e.url.clone(), e.http.clone());
        let w = tokio::spawn(async move {
            http.get(format!("{url}/v1/inbox?wait=3"))
                .bearer_auth(CLAUDE)
                .send()
                .await
                .unwrap()
                .status()
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(e.get(DSH, "/v1/health").await.1["waiters"]["claude"], 1);
        w.await.unwrap();
        assert_eq!(e.get(DSH, "/v1/health").await.1["waiters"]["claude"], 0);
    }

    #[tokio::test]
    async fn every_message_is_mirrored_noted_in_step_relay_and_drift_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = dir.path().join("mirror");
        let relay = dir.path().join("relay");
        std::fs::create_dir_all(relay.join("traces")).unwrap();
        std::fs::write(relay.join("traces/e1.md"), "# 实验\n\n---\n\n").unwrap();
        let bridge = Bridge::open(dir.path().join("state"), &["claude", "dsh"]).unwrap();
        let state = Arc::new(
            AppState::new(
                bridge,
                vec![
                    ("claude".into(), sha256(CLAUDE)),
                    ("dsh".into(), sha256(DSH)),
                ],
            )
            .unwrap()
            .with_mirror(&mirror)
            .with_step_relay(&relay),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(state.clone(), listener));
        let e = Env {
            _dir: tempfile::tempdir().unwrap(),
            url,
            http: reqwest::Client::new(),
        };

        e.post(CLAUDE, "/v1/tasks", json!({ "id": "t", "kind": "task", "protocol": PROTOCOL, "body": "## [伪造] [条目]\n正文",
            "meta": { "to": "dsh", "title": "镜像测试", "expr_id": "e1" } })).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        settle().await;
        assert!(
            mirror.join("t/msg-1-claude.md").exists() && mirror.join("t/msg-2-dsh.md").exists()
        );
        let trace = std::fs::read_to_string(relay.join("traces/e1.md")).unwrap();
        assert_eq!(trace.matches("[Claude⇄DSH 桥接]").count(), 2, "{trace}");
        assert!(
            trace.contains("\\## [伪造]"),
            "entry-header lines in the body are escaped: {trace}"
        );

        // A file the store does not have: reported in health, not adopted.
        std::fs::write(mirror.join("t/msg-3-dsh.json"), "{}").unwrap();
        std::fs::remove_file(mirror.join("t/msg-1-claude.md")).unwrap();
        state.sync_mirror().recv().unwrap();
        let h = e.get(CLAUDE, "/v1/health").await.1;
        assert_eq!(
            h["mirror"]["stats"]["drift"]["unknown"],
            json!(["t/msg-3-dsh.json"])
        );
        assert!(
            mirror.join("t/msg-1-claude.md").exists(),
            "missing mirror files are rewritten"
        );
        assert_eq!(
            e.get(CLAUDE, "/v1/tasks/t").await.1["task"]["last_n"],
            2,
            "the stray file changed nothing"
        );
    }

    #[tokio::test]
    async fn a_slow_mirror_drive_never_holds_up_requests() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = Bridge::open(dir.path().join("state"), &["claude", "dsh"]).unwrap();
        let state = Arc::new(
            AppState::new(
                bridge,
                vec![
                    ("claude".into(), sha256(CLAUDE)),
                    ("dsh".into(), sha256(DSH)),
                ],
            )
            .unwrap()
            .with_mirror(dir.path().join("mirror")),
        );
        state.copier.set_slow_for_tests(2000);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(state.clone(), listener));
        let e = Env {
            _dir: tempfile::tempdir().unwrap(),
            url,
            http: reqwest::Client::new(),
        };

        let t0 = std::time::Instant::now();
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        let _sync = state.sync_mirror(); // a full sync queued behind it, too
        for _ in 0..5 {
            e.post(
                DSH,
                "/v1/tasks/t/messages",
                json!({ "kind": "ack", "protocol": PROTOCOL, "client_msg_id": "a" }),
            )
            .await;
            assert_eq!(e.get(CLAUDE, "/v1/tasks/t").await.0, 200);
        }
        let h = e.get(CLAUDE, "/v1/health").await.1;
        assert!(
            t0.elapsed() < Duration::from_millis(1500),
            "requests waited for the drive: {:?}",
            t0.elapsed()
        );
        assert!(
            h["mirror"]["stats"]["queued"].as_u64().unwrap() >= 2,
            "copies are queued, not done inline: {h}"
        );
        state.copier.set_slow_for_tests(0);
    }
}
