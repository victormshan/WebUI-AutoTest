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
use crate::model::{Kind, Message, State as TaskState};
use crate::notify::{self, Target};
use crate::user::{self, Decisions, ItemStatus, PendingItem};
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
    /// Nudge the receiver of an unfinished task after this long without a message from it (P2).
    pub nudge_after: chrono::Duration,
    /// Nudges per stretch of silence; after that the user is told instead (P2).
    pub nudge_max: u32,
    /// Unread messages for an agent without push and without a waiter go to the user after this
    /// long (P5: only the user can bring Claude back).
    pub unread_alert_after: chrono::Duration,
    /// An undecided needs_user item nobody has asked the user about is overdue after this (Q5).
    pub ask_overdue_after: chrono::Duration,
    /// A task announced as restarting but not back after this goes to the user: a restart that
    /// never happened must not leave the task silent forever (it gets no nudge and no stall).
    pub restart_overdue_after: chrono::Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(60),
            rewake_after: chrono::Duration::minutes(15),
            stall_after: chrono::Duration::hours(24),
            stall_cooldown: chrono::Duration::hours(24),
            // Agreed with DSH in dsh-auto-continue: 20 minutes, three times, then the user.
            nudge_after: chrono::Duration::minutes(20),
            nudge_max: 3,
            unread_alert_after: chrono::Duration::minutes(30),
            ask_overdue_after: chrono::Duration::hours(12),
            restart_overdue_after: chrono::Duration::hours(1),
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

/// What each agent's side looks like from here (P9, Q4).
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Presence {
    /// Last authenticated request from the agent itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_request_at: Option<DateTime<Utc>>,
    /// Last push to the agent and what came of it — a push reaching it is not the agent being up.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_push_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_push_result: Option<String>,
    /// What the agent declares about its platform (declared, not verified).
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform_at: Option<DateTime<Utc>>,
}

/// Nudges sent for a task during the receiver's current silence (keyed by its last message).
#[derive(Debug, Clone)]
struct Nudges {
    since_n: u32,
    count: u32,
    last_at: DateTime<Utc>,
}

/// What a push adds to its ledger line besides the outcome.
#[derive(Default)]
struct PushExtra {
    /// Dedup identity for the endpoint (see `notify::wake`).
    key: Option<String>,
    ledger: Option<Value>,
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
    presence: BTreeMap<String, Presence>,
    /// Outcome of the push made for (task, n): true = the agent was woken. A message whose
    /// push woke the agent is never pushed again (dedup by identity, bridge-wake-dedup B).
    pushed: BTreeMap<(String, u32), bool>,
    nudges: BTreeMap<String, Nudges>,
    /// Who asked the user about which needs_user item, and when (Q5).
    asked: BTreeMap<String, (String, DateTime<Utc>)>,
    /// Items the monitor raised for the user on its last pass: unread messages nobody can be
    /// woken for (P5) and receivers still silent after their nudges (P2).
    alerts: Vec<PendingItem>,
}

pub struct AppState {
    inner: Mutex<Inner>,
    /// (agent, sha256(token))
    tokens: Vec<(String, [u8; 32])>,
    /// (observer, sha256(token)): read-only, not participants (`observe.rs`).
    observers: Vec<(String, [u8; 32])>,
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
    urgent: bool,
    extra: PushExtra,
}

fn bridge_root_for_asked(ledger: &std::path::Path) -> &std::path::Path {
    ledger.parent().unwrap_or(std::path::Path::new("."))
}

pub fn sha256(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    // ASCII first: slicing below is by byte, and 64 bytes of other text are not 64 digits.
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
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
                presence: BTreeMap::new(),
                pushed: BTreeMap::new(),
                nudges: BTreeMap::new(),
                asked: user::load_asked(bridge_root_for_asked(&ledger))?,
                alerts: Vec::new(),
            }),
            tokens,
            observers: Vec::new(),
            news: Notify::new(),
            targets: BTreeMap::new(),
            timing: Timing::default(),
            ledger,
            copier: Copier::spawn(None),
            step_relay: None,
        })
    }

    /// Read-only observers (`observe.rs`); none may share a name or token hash with an agent.
    pub fn with_observers(mut self, observers: Vec<(String, [u8; 32])>) -> anyhow::Result<Self> {
        crate::observe::check_distinct(&self.tokens, &observers)?;
        self.observers = observers;
        Ok(self)
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
            let mut alerts = Vec::new();
            // Tasks waiting for the user (an undecided needs_user item): the receiver asked and
            // moved on, as agreed in ask-dont-wait — that is not silence, so no nudge.
            let waiting_for_user: BTreeSet<String> = self
                .pending_items(&g, false)
                .into_iter()
                .filter(|i| i.source == "needs_user")
                .map(|i| i.task)
                .collect();
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
                if last.effective_wake().pushes()
                    && g.cursors.get(&to, &t.id) < last.n
                    && now - last.at >= self.timing.rewake_after
                    && self.targets.contains_key(&to)
                    && g.pushed.get(&(t.id.clone(), last.n)) != Some(&true)
                    && g.rewoken.insert((t.id.clone(), last.n))
                {
                    pushes.push(push_for(&t, &last, &to, "rewake"));
                }
                // P2: whoever's move it is went quiet — nudge them, tell the other side; once
                // the nudges are spent, the user hears of it.
                // In acked / working the move is the receiver's — also after the requester's
                // answer or a rework verdict, whose push may have been read and then dropped
                // (a session that stops at the end of its turn) — except after the receiver's
                // own note (a diff sent for review, a report): then it waits on the requester.
                // Its own progress means it is still at work, so the move stays its own.
                let (owes, other) = if last.from == t.to && last.kind == Kind::Note {
                    (&t.from, &t.to)
                } else {
                    (&t.to, &t.from)
                };
                if matches!(t.state, TaskState::Acked | TaskState::Working)
                    && !waiting_for_user.contains(&t.id)
                    && now - last.at >= self.timing.nudge_after
                {
                    let n = g.nudges.get(&t.id).filter(|x| x.since_n == last.n).cloned();
                    let due = n
                        .as_ref()
                        .is_none_or(|x| now - x.last_at >= self.timing.nudge_after);
                    let count = n.as_ref().map_or(0, |x| x.count);
                    let pushable = self.targets.contains_key(owes);
                    // A side that cannot be pushed (Claude long-polls) gets no nudge; the user
                    // hears of it after the silence a pushed side would have had in full.
                    let unpushed_spent = !pushable
                        && now - last.at
                            >= self.timing.nudge_after * (self.timing.nudge_max as i32 + 1);
                    if count < self.timing.nudge_max && pushable {
                        if due {
                            g.nudges.insert(
                                t.id.clone(),
                                Nudges {
                                    since_n: last.n,
                                    count: count + 1,
                                    last_at: now,
                                },
                            );
                            let mins = (now - last.at).num_minutes();
                            let mut p = push_for(&t, &last, owes, "nudge");
                            p.kind = "nudge".into();
                            // Measurable (dsh-auto-continue): which nudge, and since when silent.
                            p.extra = PushExtra {
                                // The silence is part of the identity: k restarts with every new silence,
                                // so `nudge:<task>:<k>` alone repeats and a deduping endpoint drops it.
                                key: Some(format!("nudge:{}:{}:{}", t.id, last.n, count + 1)),
                                ledger: Some(
                                    json!({ "nudges": count + 1, "lastMessageAt": last.at }),
                                ),
                            };
                            // Say what the nudge rests on: the last message, who sent it, the silence.
                            let ground = format!(
                                "任务 {}（{}）#{}（{} {} 发）之后 {} 分钟无新消息",
                                t.id, t.title, last.n, last.from, last.kind, mins
                            );
                            p.summary = if &last.from == owes {
                                format!(
                                    "{}{}：下一步在你，请继续，或发 progress / result 说明情况",
                                    snapshot_lead(&t.id, now),
                                    ground
                                )
                            } else {
                                format!(
                                    "{}{}：{} 在等你回复",
                                    snapshot_lead(&t.id, now),
                                    ground,
                                    last.from
                                )
                            };
                            pushes.push(p);
                            if self.targets.contains_key(other) {
                                let mut q = push_for(&t, &last, other, "nudge");
                                q.kind = "nudge_notice".into();
                                q.extra.key =
                                    Some(format!("nudge-notice:{}:{}:{}", t.id, last.n, count + 1));
                                q.summary = format!(
                                    "{}已提醒 {} 继续任务 {}（#{} 之后 {} 分钟无新消息）",
                                    snapshot_lead(&t.id, now),
                                    owes,
                                    t.id,
                                    last.n,
                                    mins
                                );
                                pushes.push(q);
                            }
                        }
                    } else if due && (pushable || unpushed_spent) {
                        alerts.push(PendingItem::new(
                            format!("{}#silent#{}", t.id, last.n),
                            &t.id,
                            "silent",
                            format!(
                                "{} 在任务 {} 上 #{}（{} 发）之后 {} 分钟没有新消息（已提醒 {} 次）：需要你叫它继续",
                                owes,
                                t.id,
                                last.n,
                                last.from,
                                (now - last.at).num_minutes(),
                                count
                            ),
                        ));
                    }
                }
                if t.state == TaskState::AwaitingRestart {
                    // Q1: a restart is a step, not a stall — but one that never completes is.
                    if now - t.updated_at >= self.timing.restart_overdue_after {
                        alerts.push(PendingItem::new(
                            format!("{}#restart-overdue#{}", t.id, t.last_n),
                            &t.id,
                            "restart-overdue",
                            format!(
                                "{} 在任务 {} 上宣布了重启，{} 分钟后仍未发 restarted：重启可能没有发生或失败了，请确认",
                                t.to,
                                t.id,
                                (now - t.updated_at).num_minutes()
                            ),
                        ));
                    }
                    continue;
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
                                // Not a message: its own dedup key, or the endpoint would take it
                                // for a repeat of message n (bridge:<task>:<n>) and drop it.
                                p.extra.key = Some(format!("stalled:{}:{}", t.id, now.timestamp()));
                                pushes.push(p);
                            }
                        }
                    }
                }
            }
            g.stalled = stalled_now;
            // P5: messages for an agent that cannot be pushed to and is not listening.
            let agents: Vec<String> = g.bridge.agents().map(str::to_string).collect();
            for a in agents {
                if self.targets.contains_key(&a) || g.waiters.get(&a).copied().unwrap_or(0) > 0 {
                    continue;
                }
                let mut seen = BTreeSet::new();
                for m in unread(&g, &a) {
                    if !m.effective_wake().pushes()
                        || now - m.at < self.timing.unread_alert_after
                        || !seen.insert(m.task.clone())
                    {
                        continue;
                    }
                    let Some(t) = g.bridge.task(&m.task) else {
                        continue;
                    };
                    if t.state == TaskState::AwaitingRestart {
                        continue;
                    }
                    let busy = g.bridge.messages(&t.id).is_ok_and(|ms| {
                        ms.iter().any(|x| {
                            x.from == a
                                && x.kind == crate::model::Kind::Progress
                                && now - x.at < chrono::Duration::hours(2)
                        })
                    });
                    if busy {
                        continue;
                    }
                    alerts.push(PendingItem::new(
                        format!("{}#unread#{}", t.id, m.n),
                        &t.id,
                        "unread",
                        format!(
                            "发给 {a} 的 {}（{}#{}）已 {} 分钟未读，{a} 不在线也无法推送：请唤起 {a}",
                            m.kind,
                            t.id,
                            m.n,
                            (now - m.at).num_minutes()
                        ),
                    ));
                }
            }
            g.alerts = alerts;
        }
        for p in pushes {
            drop(self.send_push(p));
        }
    }

    /// After a message is stored: wake long polls, and push to the recipient if it needs one.
    /// Returns the push in flight (true = recipient woken), if one was made.
    fn stored(self: &Arc<Self>, m: &Message) -> Option<tokio::task::JoinHandle<bool>> {
        self.news.notify_waiters();
        self.record_copies(m);
        if !m.effective_wake().pushes() {
            return None;
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
        push.filter(|p| self.targets.contains_key(&p.agent))
            .map(|p| self.send_push(p))
    }

    /// P5: how a just-stored message reached its recipient — kept apart from the task's own
    /// state ("woken" is not "working on it").
    async fn delivery(
        self: &Arc<Self>,
        m: &Message,
        fresh: bool,
        push: Option<tokio::task::JoinHandle<bool>>,
    ) -> Value {
        let (to, waiters, last_request_at) = {
            let g = self.lock();
            let to = g.bridge.task(&m.task).map_or(String::new(), |t| {
                if m.from == t.from {
                    t.to.clone()
                } else {
                    t.from.clone()
                }
            });
            let w = g.waiters.get(&to).copied().unwrap_or(0);
            let l = g.presence.get(&to).and_then(|p| p.last_request_at);
            (to, w, l)
        };
        let push_state = if !fresh {
            "duplicate"
        } else if !m.effective_wake().pushes() {
            "quiet"
        } else if !self.targets.contains_key(&to) {
            "no-push-target"
        } else {
            "sent"
        };
        let woken = match push {
            Some(h) => tokio::time::timeout(Duration::from_secs(5), h)
                .await
                .ok()
                .and_then(|r| r.ok()),
            None => None,
        };
        json!({ "to": to, "waiters": waiters, "push": push_state, "woken": woken, "recipientLastRequestAt": last_request_at })
    }

    fn pending_items(&self, g: &Inner, all: bool) -> Vec<PendingItem> {
        let ctx = user::PendingCtx {
            stalled: &g.stalled,
            asked: &g.asked,
            now: Utc::now(),
            ask_overdue_after: self.timing.ask_overdue_after,
        };
        let mut v = user::pending(&g.bridge, &g.decisions, &ctx, all);
        for a in &g.alerts {
            let mut p = a.clone();
            p.status = g.decisions.status(&p.item);
            p.decisions = g.decisions.for_item(&p.item);
            if all || p.status != ItemStatus::Decided {
                v.push(p);
            }
        }
        v
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

    fn send_push(self: &Arc<Self>, p: Push) -> tokio::task::JoinHandle<bool> {
        let Some(target) = self.targets.get(&p.agent).cloned() else {
            return tokio::spawn(async { false });
        };
        let s = self.clone();
        tokio::spawn(async move {
            let o = notify::wake(
                &target,
                &p.task,
                p.n,
                &p.kind,
                &p.summary,
                p.urgent,
                p.extra.key.as_deref(),
            )
            .await;
            let mut entry = json!({
                "at": Utc::now(), "agent": p.agent, "task": p.task, "n": p.n, "kind": p.kind, "why": p.why,
                "agentWoken": o.agent_woken, "coalesced": o.coalesced, "status": o.status,
                "transport": o.transport, "reason": o.reason,
            });
            if let Some(k) = &p.extra.key {
                entry["key"] = json!(k);
            }
            if let Some(Value::Object(m)) = &p.extra.ledger {
                for (k, v) in m {
                    entry[k] = v.clone();
                }
            }
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
            if matches!(p.why, "new" | "rewake") {
                let e = g.pushed.entry((p.task.clone(), p.n)).or_insert(false);
                *e = *e || o.agent_woken;
            }
            let pr = g.presence.entry(p.agent.clone()).or_default();
            pr.last_push_at = Some(Utc::now());
            pr.last_push_result = Some(if o.agent_woken {
                "woken".into()
            } else {
                format!("not woken: {}", o.reason)
            });
            g.wakes.count += 1;
            if o.agent_woken {
                g.wakes.woken += 1
            } else {
                g.wakes.failed += 1
            }
            g.wakes.last = Some(entry);
            o.agent_woken
        })
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

    /// Who a bearer token belongs to, constant-time over the tokens of both roles. Reads nothing
    /// and writes nothing else.
    fn role_for(&self, header_value: Option<&str>) -> Role {
        let Some(token) = header_value.and_then(|h| h.strip_prefix("Bearer ")) else {
            return Role::Anonymous;
        };
        let given = sha256(token);
        let mut found = Role::Invalid;
        let tagged = self
            .tokens
            .iter()
            .map(|(_, h)| (h, false))
            .chain(self.observers.iter().map(|(_, h)| (h, true)));
        for (h, observer) in tagged {
            let diff = h
                .iter()
                .zip(given.iter())
                .fold(0u8, |acc, (x, y)| acc | (x ^ y));
            if diff == 0 {
                found = if observer {
                    Role::Observer
                } else {
                    Role::Agent
                };
            }
        }
        found
    }

    /// Everything in memory a request could change besides the bridge itself (tests compare it
    /// before and after observer requests).
    #[cfg(test)]
    fn side_state(&self) -> String {
        let g = self.lock();
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            g.presence
                .iter()
                .map(|(a, p)| (a, format!("{p:?}")))
                .collect::<Vec<_>>(),
            g.waiters,
            g.wakes,
            g.pushed,
            g.nudges,
            g.rewoken,
            g.stalled,
            g.stall_notice,
            g.asked,
            g.alerts,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// A push can be read long after it was sent (the agent's queue): every push says it is a
/// snapshot and points at the task's current state, so a superseded one is not taken as news.
/// Pushes that write their own summary (nudge, notice, decision) start with this too.
fn snapshot_lead(task: &str, at: DateTime<Utc>) -> String {
    format!(
        "先读当前状态：GET /v1/tasks/{}（agent-bridge 127.0.0.1:7879）。以下是推送时（{}）的快照，可能已被后续消息取代｜",
        task,
        at.format("%Y-%m-%dT%H:%M:%SZ")
    )
}

fn push_for(t: &crate::model::Task, m: &Message, to: &str, why: &'static str) -> Push {
    let body: String = m.body.chars().take(200).collect();
    let summary = format!(
        "{}{}｜#{} {} 来自 {}：{}",
        snapshot_lead(&t.id, m.at),
        t.title,
        m.n,
        m.kind,
        m.from,
        body
    );
    Push {
        agent: to.into(),
        task: t.id.clone(),
        n: m.n,
        kind: m.kind.to_string(),
        why,
        summary,
        urgent: m.effective_wake() == crate::model::WakeLevel::Urgent,
        extra: PushExtra::default(),
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

/// An observer request the gate let through (`observer_gate`).
#[derive(Clone)]
struct ObserverCaller;

enum Role {
    Anonymous,
    Invalid,
    Agent,
    Observer,
}

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/v1/tasks", get(list_tasks).post(create_task))
        .route("/v1/tasks/{id}", get(show_task))
        .route("/v1/tasks/{id}/messages", post(post_message))
        .route("/v1/inbox", get(inbox))
        .route("/v1/inbox/ack", post(ack))
        .route("/v1/user/pending", get(user_pending))
        .route("/v1/user/decisions", post(user_decision))
        .route("/v1/user/asked", post(user_asked))
        .route("/v1/presence", post(presence))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    // Not under `auth`: only `observer_gate` admits them, and they never touch presence.
    let observe = Router::new()
        .route(OBSERVE_LIST, get(observe_list))
        .route("/v1/observe/tasks/{id}", get(observe_detail));
    Router::new()
        .route("/v1/health", get(health))
        .merge(api)
        .merge(observe)
        .fallback(|| async { ApiError(StatusCode::NOT_FOUND, "no such route".into()) })
        .layer(DefaultBodyLimit::max(2 << 20))
        // Last, so it wraps every route, the fallback and the 405 answers too.
        .layer(middleware::from_fn_with_state(state.clone(), observer_gate))
        .with_state(state)
}

const OBSERVE_LIST: &str = "/v1/observe/tasks";

/// Runs before routing results reach any handler. An observer token may be used for exactly
/// `GET /v1/observe/tasks` and `GET /v1/observe/tasks/<one segment>`: anything else (HEAD,
/// health, unknown paths, other methods) is 403 here, before a handler or a presence update.
/// Agents are refused the observe routes (403), and without a valid token they are 401. Every
/// other request goes on unchanged (agent routes keep their own `auth`).
async fn observer_gate(State(s): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let observe_path = path == "/v1/observe" || path.starts_with("/v1/observe/");
    let exact = path == OBSERVE_LIST
        || path
            .strip_prefix("/v1/observe/tasks/")
            .is_some_and(|id| !id.is_empty() && !id.contains('/'));
    let role = s.role_for(
        req.headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    );
    match role {
        Role::Observer if req.method() == axum::http::Method::GET && exact => {
            req.extensions_mut().insert(ObserverCaller);
            next.run(req).await
        }
        Role::Observer => ApiError(
            StatusCode::FORBIDDEN,
            "an observer may only GET /v1/observe/tasks[/<id>]".into(),
        )
        .into_response(),
        Role::Agent if observe_path => ApiError(
            StatusCode::FORBIDDEN,
            "the observe routes are for observers only".into(),
        )
        .into_response(),
        Role::Anonymous | Role::Invalid if observe_path => ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or wrong bearer token".into(),
        )
        .into_response(),
        _ => next.run(req).await,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveListQuery {
    after: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveDetailQuery {
    after_n: Option<u32>,
    limit: Option<usize>,
}

fn observe_limit(limit: Option<usize>, default: usize, max: usize) -> Result<usize, ApiError> {
    match limit.unwrap_or(default) {
        l @ 1.. if l <= max => Ok(l),
        _ => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("limit must be 1..={max}"),
        )),
    }
}

/// 503 when the loaded history skipped unreadable lines: no answer to build an audit on.
fn observe_incomplete(b: &Bridge) -> Option<Response> {
    (b.bad_lines > 0).then(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "history_incomplete", "badLines": b.bad_lines })),
        )
            .into_response()
    })
}

async fn observe_list(
    State(s): State<Arc<AppState>>,
    Extension(ObserverCaller): Extension<ObserverCaller>,
    q: Result<Query<ObserveListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(q)) = q else {
        return ApiError(StatusCode::BAD_REQUEST, "bad query".into()).into_response();
    };
    let limit = match observe_limit(
        q.limit,
        crate::observe::LIST_LIMIT_DEFAULT,
        crate::observe::LIST_LIMIT_MAX,
    ) {
        Ok(l) => l,
        Err(e) => return e.into_response(),
    };
    if q.after.as_deref().is_some_and(|a| !crate::valid_id(a)) {
        return ApiError(StatusCode::BAD_REQUEST, "after is not a task id".into()).into_response();
    }
    let g = s.lock();
    if let Some(r) = observe_incomplete(&g.bridge) {
        return r;
    }
    let all = crate::observe::observe_all(&g.bridge);
    let (tasks, next) = crate::observe::list_page(&all, q.after.as_deref(), limit);
    Json(json!({
        "revision": crate::observe::revision(&all), "complete": true, "tasks": tasks, "next": next,
    }))
    .into_response()
}

async fn observe_detail(
    State(s): State<Arc<AppState>>,
    Extension(ObserverCaller): Extension<ObserverCaller>,
    id: Result<Path<String>, axum::extract::rejection::PathRejection>,
    q: Result<Query<ObserveDetailQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let (Ok(Path(id)), Ok(Query(q))) = (id, q) else {
        return ApiError(StatusCode::BAD_REQUEST, "bad path or query".into()).into_response();
    };
    if !crate::valid_id(&id) {
        return ApiError(StatusCode::BAD_REQUEST, "not a task id".into()).into_response();
    }
    let limit = match observe_limit(
        q.limit,
        crate::observe::DETAIL_LIMIT_DEFAULT,
        crate::observe::DETAIL_LIMIT_MAX,
    ) {
        Ok(l) => l,
        Err(e) => return e.into_response(),
    };
    let g = s.lock();
    if let Some(r) = observe_incomplete(&g.bridge) {
        return r;
    }
    let all = crate::observe::observe_all(&g.bridge);
    let Some((task, msgs)) = all.iter().find(|(t, _)| t.id == id) else {
        return ApiError(StatusCode::NOT_FOUND, format!("no task {id}")).into_response();
    };
    let (page, next_n) = crate::observe::message_page(msgs, q.after_n, limit);
    Json(json!({
        "revision": crate::observe::revision(&all), "complete": true, "task": task,
        "message_metadata": page, "next_n": next_n,
    }))
    .into_response()
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
            s.lock()
                .presence
                .entry(agent.clone())
                .or_default()
                .last_request_at = Some(Utc::now());
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
    let pending = s.pending_items(&g, false).len();
    let derived = g
        .bridge
        .list(None)
        .iter()
        .filter(|t| t.parent.is_some())
        .count();
    Json(json!({
        "ok": true, "service": "agent-bridge", "version": env!("CARGO_PKG_VERSION"), "protocol": PROTOCOL,
        "agents": agents, "openTasks": open, "badLines": g.bridge.bad_lines + g.decisions.bad_lines,
        "waiters": g.waiters, "unread": unread_by, "pendingForUser": pending, "stalled": g.stalled,
        "push": s.targets.keys().collect::<Vec<_>>(), "wakes": g.wakes,
        "presence": g.presence, "derivedTasks": derived,
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
) -> ApiResult<Value> {
    let fresh = s.lock().bridge.task(&b.id).is_none();
    let m = s.lock().bridge.create_task(&b.id, &me, b.draft)?;
    let push = if fresh { s.stored(&m) } else { None };
    let mut v = serde_json::to_value(&m).expect("serializable");
    v["delivery"] = s.delivery(&m, fresh, push).await;
    Ok(Json(v))
}

async fn post_message(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Path(id): Path<String>,
    Json(d): Json<Draft>,
) -> ApiResult<Value> {
    let before = s.lock().bridge.task(&id).map(|t| t.last_n);
    let m = s.lock().bridge.post(&id, &me, d)?;
    // A retried post returns the stored message: no second wake for it.
    let fresh = before.is_some_and(|n| m.n > n);
    let push = if fresh { s.stored(&m) } else { None };
    // A receiver speaking after a nudge: record how long that took (the nudge's measured effect).
    if fresh {
        let followed = {
            let mut g = s.lock();
            let is_receiver = g.bridge.task(&id).is_some_and(|t| t.to == me);
            match g.nudges.get(&id) {
                Some(x) if is_receiver && x.since_n < m.n => {
                    let x = x.clone();
                    g.nudges.remove(&id);
                    Some(x)
                }
                _ => None,
            }
        };
        if let Some(x) = followed {
            let entry = json!({ "at": m.at, "agent": me, "task": id, "n": m.n, "kind": m.kind.to_string(),
                "why": "nudge-followed", "nudges": x.count, "afterNudgeSecs": (m.at - x.last_at).num_seconds() });
            if let Err(e) = s.append_ledger(&entry) {
                eprintln!("agent-bridge: wake ledger write failed: {e:#}");
            }
        }
    }
    // P4: posting in a task means having read it — the sender's cursor for this task (and only
    // this task) moves up to its own message; the response says which messages that covered.
    let (from, to) = {
        let mut g = s.lock();
        let from = g.cursors.get(&me, &id) + 1;
        g.cursors.ack(&me, &id, m.n).map_err(BridgeError::Io)?;
        (from, m.n)
    };
    let mut v = serde_json::to_value(&m).expect("serializable");
    if from <= to {
        v["implicit_read"] = json!({ "task": id, "from": from, "to": to });
    }
    v["delivery"] = s.delivery(&m, fresh, push).await;
    Ok(Json(v))
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
            // Imported archives are history, not news: never delivered as unread.
            out.extend(
                ms.iter()
                    .filter(|m| m.n > read && m.from != me && !m.imported)
                    .cloned(),
            );
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
    Json(json!({ "items": s.pending_items(&g, q.all) }))
}

#[derive(Deserialize)]
struct DecisionBody {
    /// A pending item — or none, with `task`, for a decision the user gave directly (P3).
    #[serde(default)]
    item: Option<String>,
    #[serde(default)]
    task: Option<String>,
    /// The user's words.
    verbatim: String,
    /// `verbatim` (default: the user's own words) or `paraphrase` (a retelling).
    #[serde(default)]
    form: Option<String>,
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
    let form = b.form.as_deref().unwrap_or("verbatim");
    if !matches!(form, "verbatim" | "paraphrase") {
        return Err(BridgeError::Invalid(format!(
            "form must be verbatim or paraphrase, not {form:?}"
        ))
        .into());
    }
    let (status, decision, resumed, task, item_id) = {
        let mut g = s.lock();
        let item = match (&b.item, &b.task) {
            (Some(id), None) => {
                let all = s.pending_items(&g, true);
                let Some(item) = all.into_iter().find(|i| &i.item == id) else {
                    return Err(BridgeError::Invalid(format!(
                        "no pending item {id:?} (see GET /v1/user/pending?all=true)"
                    ))
                    .into());
                };
                item
            }
            (None, Some(task)) => {
                if g.bridge.task(task).is_none() {
                    return Err(BridgeError::NoSuchTask(task.clone()).into());
                }
                PendingItem::new(g.decisions.next_direct(task), task, "direct", String::new())
            }
            _ => {
                return Err(BridgeError::Invalid(
                    "give either item (a pending item) or task (a decision given directly)".into(),
                )
                .into());
            }
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
            .record(&item.item, &b.verbatim, &me, Some(form))
            .map_err(BridgeError::Io)?;
        let resumed = if item.source == "paused"
            && status == ItemStatus::Decided
            && t.state == TaskState::PausedForUser
        {
            Some(g.bridge.resume(&t.id, &me, decision.verbatim.as_str())?)
        } else {
            None
        };
        (status, decision, resumed, t, item.item)
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
            p.extra.key = Some(format!(
                "decision:{}:{}",
                item_id,
                &decision.sha256[..12.min(decision.sha256.len())]
            ));
            p.summary = format!(
                "{}用户对 {} 做了决定（{}转达{}）：{}",
                snapshot_lead(&task.id, decision.at),
                item_id,
                me,
                if form == "paraphrase" {
                    "，转述"
                } else {
                    ""
                },
                decision.verbatim.chars().take(200).collect::<String>()
            );
            drop(s.send_push(p));
        }
    }
    let code = if status == ItemStatus::Conflict {
        StatusCode::CONFLICT
    } else {
        StatusCode::OK
    };
    Ok((
        code,
        Json(
            json!({ "item": item_id, "status": status, "decision": decision, "resumed": resumed }),
        ),
    )
        .into_response())
}

#[derive(Deserialize)]
struct AskedBody {
    item: String,
}

/// Q5: the caller has put a needs_user item to the user (it stops counting toward overdue).
async fn user_asked(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Json(b): Json<AskedBody>,
) -> ApiResult<Value> {
    let mut g = s.lock();
    let Some(item) = s
        .pending_items(&g, true)
        .into_iter()
        .find(|i| i.item == b.item)
    else {
        return Err(BridgeError::Invalid(format!("no pending item {:?}", b.item)).into());
    };
    let t = g
        .bridge
        .task(&item.task)
        .ok_or_else(|| BridgeError::NoSuchTask(item.task.clone()))?;
    if t.from != me && t.to != me {
        return Err(BridgeError::Forbidden(format!("{me} is not a party to task {}", t.id)).into());
    }
    if let Some((by, at)) = g.asked.get(&b.item) {
        return Ok(Json(
            json!({ "item": b.item, "askedBy": by, "askedAt": at, "advanced": false }),
        ));
    }
    let now = Utc::now();
    user::record_asked(bridge_root_for_asked(&s.ledger), &b.item, &me, now)
        .map_err(BridgeError::Io)?;
    g.asked.insert(b.item.clone(), (me.clone(), now));
    Ok(Json(
        json!({ "item": b.item, "askedBy": me, "askedAt": now, "advanced": true }),
    ))
}

#[derive(Deserialize)]
struct PresenceBody {
    platform: Value,
}

/// Q4: what an agent declares about its own platform (shown in /v1/health as declared).
async fn presence(
    State(s): State<Arc<AppState>>,
    Extension(Caller(me)): Extension<Caller>,
    Json(b): Json<PresenceBody>,
) -> ApiResult<Value> {
    let mut g = s.lock();
    let p = g.presence.entry(me.clone()).or_default();
    p.platform = Some(b.platform);
    p.platform_at = Some(Utc::now());
    Ok(Json(
        json!({ "agent": me, "presence": p.clone(), "declared": true }),
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

    fn pending_sources(v: &Value) -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["source"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_message_whose_push_woke_the_agent_is_never_pushed_again() {
        let (notify_url, calls) = mock_notify(true).await;
        let (e, s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        settle().await;
        let now = Utc::now();
        s.tick(now + chrono::Duration::minutes(16));
        s.tick(now + chrono::Duration::minutes(40));
        settle().await;
        assert_eq!(calls.lock().unwrap().len(), 1, "woken once, unread or not");
    }

    #[tokio::test]
    async fn posts_say_how_the_message_was_delivered() {
        let (notify_url, _calls) = mock_notify(true).await;
        let (e, _s) = start_push(&notify_url, Timing::default()).await;
        let (_, v) = e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        assert_eq!(v["delivery"]["to"], "dsh");
        assert_eq!(v["delivery"]["push"], "sent");
        assert_eq!(v["delivery"]["woken"], true);
        // Claude has no push target; DSH has asked something of the service, so it shows as seen.
        let (_, v) = e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        assert_eq!(v["delivery"]["push"], "quiet");
        let (_, v) = e
            .post(
                DSH,
                "/v1/tasks/t/messages",
                json!({ "kind": "question", "protocol": PROTOCOL,
                "questions": [{ "id": "q1", "text": "?", "blocking": true }] }),
            )
            .await;
        assert_eq!(v["delivery"]["to"], "claude");
        assert_eq!(v["delivery"]["push"], "no-push-target");
        assert!(v["delivery"]["woken"].is_null());
        assert!(v["delivery"]["recipientLastRequestAt"].is_string());
    }

    #[tokio::test]
    async fn the_requesters_note_on_taken_work_wakes_the_receiver() {
        // Seen live: a rework sent as a plain note was stored quiet, never pushed, and DSH sat
        // idle. A requester's note on acked/working work is something to act on.
        let (notify_url, _calls) = mock_notify(true).await;
        let (e, _s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        let note = |wake: Option<&str>| {
            let mut m =
                json!({ "kind": "note", "protocol": PROTOCOL, "reply_to": 2, "body": "rework" });
            if let Some(w) = wake {
                m["wake"] = json!(w);
            }
            m
        };
        let (_, v) = e.post(CLAUDE, "/v1/tasks/t/messages", note(None)).await;
        assert_eq!(v["delivery"]["push"], "sent", "{v}");
        assert_eq!(v["wake"], "normal", "the level is stored on the message");
        let (_, v) = e
            .post(CLAUDE, "/v1/tasks/t/messages", note(Some("quiet")))
            .await;
        assert_eq!(v["delivery"]["push"], "quiet", "an explicit choice is kept");
    }

    #[tokio::test]
    async fn a_restart_that_never_completes_reaches_the_user_and_is_never_a_stall() {
        let (notify_url, calls) = mock_notify(true).await;
        let (e, s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "progress", "protocol": PROTOCOL, "phase": "pending-restart" }),
        )
        .await;
        settle().await;
        let now = Utc::now();
        s.tick(now + chrono::Duration::minutes(50));
        assert!(pending_sources(&e.get(CLAUDE, "/v1/user/pending").await.1).is_empty());
        s.tick(now + chrono::Duration::minutes(61));
        let p = e.get(CLAUDE, "/v1/user/pending").await.1;
        assert_eq!(pending_sources(&p), ["restart-overdue"]);
        assert_eq!(p["items"][0]["item"], "t#restart-overdue#3");
        // A day later it is still that, not a stall, and nobody was nudged meanwhile.
        s.tick(now + chrono::Duration::hours(25));
        settle().await;
        assert_eq!(
            pending_sources(&e.get(CLAUDE, "/v1/user/pending").await.1),
            ["restart-overdue"]
        );
        assert!(
            e.get(CLAUDE, "/v1/health").await.1["stalled"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|c| c.1["kind"] != "nudge" && c.1["kind"] != "stalled"),
            "no nudge, no stall notice while awaiting a restart"
        );
        // Back from the restart: the alert goes.
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "progress", "protocol": PROTOCOL, "phase": "restarted", "body": "bootId 2" }),
        )
        .await;
        // A pass right after the restarted message: the task is back in working, so the
        // restart-overdue item must be gone. `back` is read after the post returned, so it is not
        // earlier than the task's updated_at (and chrono durations are signed anyway).
        let back = Utc::now();
        assert!(back >= s.lock().bridge.task("t").unwrap().updated_at);
        s.tick(back);
        assert!(pending_sources(&e.get(CLAUDE, "/v1/user/pending").await.1).is_empty());
    }

    #[tokio::test]
    async fn a_receiver_waiting_for_the_user_is_not_nudged_until_the_user_decides() {
        let (notify_url, calls) = mock_notify(true).await;
        let (e, s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "progress", "protocol": PROTOCOL, "body": "delivered",
                "needs_user": [{ "text": "授权重启宿主？", "relay": "dsh" }] }),
        )
        .await;
        settle().await;
        let now = Utc::now();
        let nudges = || {
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.1["kind"] == "nudge")
                .count()
        };
        s.tick(now + chrono::Duration::hours(3));
        settle().await;
        assert_eq!(nudges(), 0, "asking the user and moving on is not silence");
        assert!(
            pending_sources(&e.get(CLAUDE, "/v1/user/pending").await.1)
                .iter()
                .all(|x| x != "silent")
        );
        // The user decides: from then on it is the receiver's move again.
        let item = e.get(CLAUDE, "/v1/user/pending").await.1["items"][0]["item"]
            .as_str()
            .unwrap()
            .to_string();
        let (c, v) = e
            .post(
                DSH,
                "/v1/user/decisions",
                json!({ "item": item, "verbatim": "可以重启" }),
            )
            .await;
        assert_eq!(c, 200, "{v}");
        s.tick(now + chrono::Duration::hours(4));
        settle().await;
        assert_eq!(nudges(), 1, "after the decision the silence counts again");
    }

    #[tokio::test]
    async fn nudges_and_decisions_say_they_are_snapshots_like_message_pushes() {
        // Seen live: a nudge read after its task was closed said only "your move".
        let (notify_url, calls) = mock_notify(true).await;
        let (e, s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        settle().await;
        s.tick(Utc::now() + chrono::Duration::hours(3));
        settle().await;
        let summary_of = |kind: &str| {
            calls
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|c| c.1["kind"] == kind)
                .map(|c| c.1["summary"].as_str().unwrap().to_string())
                .unwrap_or_else(|| panic!("no {kind} push"))
        };
        let lead = "先读当前状态：GET /v1/tasks/t（";
        let nudge = summary_of("nudge");
        assert!(nudge.starts_with(lead) && nudge.contains("快照"), "{nudge}");
        assert!(
            nudge.contains("#2（dsh ack 发）之后") && nudge.contains("下一步在你"),
            "{nudge}"
        );
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "progress", "protocol": PROTOCOL, "body": "b",
                "needs_user": [{ "text": "重载扩展？", "relay": "dsh" }] }),
        )
        .await;
        let item = e.get(CLAUDE, "/v1/user/pending").await.1["items"][0]["item"]
            .as_str()
            .unwrap()
            .to_string();
        let (c, v) = e
            .post(
                CLAUDE,
                "/v1/user/decisions",
                json!({ "item": item, "verbatim": "我已经做了" }),
            )
            .await;
        assert_eq!(c, 200, "{v}");
        settle().await;
        let decision = summary_of("user_decision");
        assert!(
            decision.starts_with(lead) && decision.contains("快照"),
            "{decision}"
        );
        assert!(decision.contains("我已经做了"), "{decision}");
    }

    #[tokio::test]
    async fn after_a_rework_verdict_the_silent_receiver_is_nudged_too() {
        let (notify_url, calls) = mock_notify(true).await;
        let (e, s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "result", "protocol": PROTOCOL, "outcome": "done",
            "results": [{ "item": "x", "status": "done", "evidence": "e" }] }),
        )
        .await;
        // The requester sends it back: the task is working again and the next step is DSH's,
        // although the last message is the requester's.
        e.post(CLAUDE, "/v1/tasks/t/messages", json!({ "kind": "verdict", "protocol": PROTOCOL, "judgement": "rework", "body": "one more" })).await;
        assert_eq!(
            e.get(CLAUDE, "/v1/tasks/t").await.1["task"]["state"],
            "working"
        );
        settle().await;
        let now = Utc::now();
        let nudges = || {
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.1["kind"] == "nudge")
                .count()
        };
        s.tick(now + chrono::Duration::minutes(15));
        settle().await;
        assert_eq!(nudges(), 0);
        s.tick(now + chrono::Duration::minutes(21));
        settle().await;
        assert_eq!(nudges(), 1, "silence after a rework verdict is nudged");
        // Waiting for the requester's verdict is not the receiver's silence: no nudge there.
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "result", "protocol": PROTOCOL, "outcome": "done",
            "results": [{ "item": "x", "status": "done", "evidence": "e2" }] }),
        )
        .await;
        s.tick(Utc::now() + chrono::Duration::hours(2));
        settle().await;
        assert_eq!(nudges(), 1);
    }

    #[tokio::test]
    async fn after_the_receivers_own_note_the_requester_owes_the_reply_and_is_nudged() {
        // Seen live: DSH sent a diff for review (a note) and was told "your move" four times.
        let (notify_url, calls) = mock_notify(true).await;
        let (e, s) = start_push(&notify_url, Timing::default()).await;
        e.post(
            DSH,
            "/v1/tasks",
            json!({ "id": "r", "kind": "task", "protocol": PROTOCOL,
            "body": "review this", "meta": { "to": "claude", "title": "review" } }),
        )
        .await;
        e.post(CLAUDE, "/v1/tasks/r/messages", k("ack")).await;
        e.post(
            CLAUDE,
            "/v1/tasks/r/messages",
            json!({ "kind": "note", "protocol": PROTOCOL,
            "reply_to": 1, "body": "verdict needs your numbers" }),
        )
        .await;
        settle().await;
        s.tick(Utc::now() + chrono::Duration::minutes(21));
        settle().await;
        let nudges: Vec<Value> = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.1["kind"] == "nudge")
            .map(|c| c.1.clone())
            .collect();
        assert_eq!(
            nudges.len(),
            1,
            "the requester, who owes the reply, is nudged"
        );
        let summary = nudges[0]["summary"].as_str().unwrap();
        assert!(
            summary.contains("#3（claude note 发）之后") && summary.contains("claude 在等你回复"),
            "{summary}"
        );
    }

    #[tokio::test]
    async fn a_side_that_cannot_be_pushed_is_not_nudged_and_reaches_the_user_late() {
        // Claude long-polls: DSH's diff waits on Claude, nobody can push Claude, and DSH is
        // not the one to nudge. The user hears of it after a pushed side's full nudge run.
        let (notify_url, calls) = mock_notify(true).await;
        let (e, s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "note", "protocol": PROTOCOL,
            "reply_to": 1, "body": "diff for review" }),
        )
        .await;
        settle().await;
        let now = Utc::now();
        for m in [21, 42, 63] {
            s.tick(now + chrono::Duration::minutes(m));
        }
        settle().await;
        assert!(
            calls.lock().unwrap().iter().all(|c| c.1["kind"] != "nudge"),
            "DSH waits on Claude: it must not be told it is its move"
        );
        assert!(pending_sources(&e.get(CLAUDE, "/v1/user/pending").await.1).is_empty());
        s.tick(now + chrono::Duration::minutes(81));
        let p = e.get(CLAUDE, "/v1/user/pending").await.1;
        assert_eq!(pending_sources(&p), ["silent"]);
        let text = p["items"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with("claude 在任务 t 上 #3（dsh 发）之后"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_silent_receiver_is_nudged_then_the_user_is_told() {
        let (notify_url, calls) = mock_notify(true).await;
        let (e, s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        settle().await;
        let now = Utc::now();
        let nudges = || -> Vec<Value> {
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.1["kind"] == "nudge")
                .map(|c| c.1.clone())
                .collect()
        };
        let at = |m: i64| now + chrono::Duration::minutes(m);
        s.tick(at(15));
        settle().await;
        assert_eq!(nudges().len(), 0, "too early");
        s.tick(at(21));
        s.tick(at(30));
        settle().await;
        assert_eq!(nudges().len(), 1, "one nudge per 20 minutes");
        assert_eq!(
            nudges()[0]["key"],
            "nudge:t:2:1",
            "own key space (silence since message 2, first nudge), not bridge:t:<n>"
        );
        s.tick(at(42));
        s.tick(at(63));
        settle().await;
        assert_eq!(nudges().len(), 3);
        assert_eq!(nudges()[2]["key"], "nudge:t:2:3");
        assert!(pending_sources(&e.get(CLAUDE, "/v1/user/pending").await.1).is_empty());
        s.tick(at(84));
        settle().await;
        assert_eq!(nudges().len(), 3, "three, then the user");
        assert_eq!(
            pending_sources(&e.get(CLAUDE, "/v1/user/pending").await.1),
            ["silent"]
        );
        // Every nudge is in the ledger with its number and the silence it answers.
        let ledger: Vec<Value> = std::fs::read_to_string(&s.ledger)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let logged: Vec<&Value> = ledger
            .iter()
            .filter(|l| l["why"] == "nudge" && l["kind"] == "nudge")
            .collect();
        assert_eq!(logged.len(), 3);
        assert_eq!(logged[1]["nudges"], 2);
        assert!(logged[0]["lastMessageAt"].is_string());
        // DSH speaks again: the follow-up is measured, a new silence starts, the alert goes.
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "progress", "protocol": PROTOCOL, "body": "busy" }),
        )
        .await;
        let followed: Vec<Value> = std::fs::read_to_string(&s.ledger)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|l| l["why"] == "nudge-followed")
            .collect();
        assert_eq!(followed.len(), 1);
        assert_eq!(followed[0]["nudges"], 3);
        assert!(followed[0]["afterNudgeSecs"].is_i64());
        s.tick(Utc::now());
        assert!(pending_sources(&e.get(CLAUDE, "/v1/user/pending").await.1).is_empty());
        // The next silence's first nudge must not reuse the previous silence's key (seen live:
        // DSH dropped nudge 1 of a new silence as a duplicate of an earlier nudge 1).
        s.tick(Utc::now() + chrono::Duration::minutes(21));
        settle().await;
        assert_eq!(nudges().len(), 4);
        assert_eq!(
            nudges()[3]["key"],
            "nudge:t:3:1",
            "new silence, new identity"
        );
        assert_ne!(nudges()[3]["key"], nudges()[0]["key"]);
        // A task waiting for a restart is not nagged (a restart that never completes is
        // reported as restart-overdue instead, see the restart test).
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "progress", "protocol": PROTOCOL, "phase": "pending-restart" }),
        )
        .await;
        s.tick(Utc::now() + chrono::Duration::hours(5));
        settle().await;
        assert_eq!(nudges().len(), 4, "no nudge while awaiting the restart");
        assert_eq!(
            pending_sources(&e.get(CLAUDE, "/v1/user/pending").await.1),
            ["restart-overdue"]
        );
    }

    #[tokio::test]
    async fn unread_messages_for_an_unreachable_agent_reach_the_user() {
        let (notify_url, _calls) = mock_notify(true).await;
        let (e, s) = start_push(&notify_url, Timing::default()).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "question", "protocol": PROTOCOL,
            "questions": [{ "id": "q1", "text": "?", "blocking": true }] }),
        )
        .await;
        let now = Utc::now();
        s.tick(now + chrono::Duration::minutes(20));
        assert!(pending_sources(&e.get(DSH, "/v1/user/pending").await.1).is_empty());
        s.tick(now + chrono::Duration::minutes(31));
        let p = e.get(DSH, "/v1/user/pending").await.1;
        assert_eq!(pending_sources(&p), ["unread"]);
        assert_eq!(p["items"][0]["item"], "t#unread#3");
        // Claude reads it: gone on the next pass.
        e.post(CLAUDE, "/v1/inbox/ack", json!({ "task": "t", "n": 3 }))
            .await;
        s.tick(now + chrono::Duration::minutes(32));
        assert!(pending_sources(&e.get(DSH, "/v1/user/pending").await.1).is_empty());
    }

    #[tokio::test]
    async fn health_shows_who_was_seen_what_they_declare_and_derived_tasks() {
        let e = start().await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(
            CLAUDE,
            "/v1/tasks",
            json!({ "id": "t2", "kind": "task", "protocol": PROTOCOL,
            "meta": { "to": "dsh", "title": "more", "parent": "t" } }),
        )
        .await;
        let (c, _) = e
            .post(
                DSH,
                "/v1/presence",
                json!({ "platform": { "host": "4.14.6", "up": true } }),
            )
            .await;
        assert_eq!(c, 200);
        let h = e.get(CLAUDE, "/v1/health").await.1;
        assert_eq!(h["derivedTasks"], 1);
        assert!(h["presence"]["claude"]["lastRequestAt"].is_string());
        assert_eq!(h["presence"]["dsh"]["platform"]["host"], "4.14.6");
        assert!(h["presence"]["dsh"]["platformAt"].is_string());
    }

    #[tokio::test]
    async fn decisions_given_directly_are_recorded_against_the_task_with_their_form() {
        let e = start().await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        let (c, v) = e
            .post(
                DSH,
                "/v1/user/decisions",
                json!({ "task": "t", "verbatim": "重启吧" }),
            )
            .await;
        assert_eq!(c, 200, "{v}");
        assert_eq!(v["item"], "t#direct#1");
        assert_eq!(v["decision"]["form"], "verbatim");
        let (_, v) = e
            .post(
                CLAUDE,
                "/v1/user/decisions",
                json!({ "task": "t", "verbatim": "用户同意合并", "form": "paraphrase" }),
            )
            .await;
        assert_eq!(v["item"], "t#direct#2");
        assert_eq!(v["decision"]["form"], "paraphrase");
        let all = e.get(CLAUDE, "/v1/user/pending?all=true").await.1;
        assert_eq!(pending_sources(&all), ["direct", "direct"]);
        assert!(
            pending_sources(&e.get(CLAUDE, "/v1/user/pending").await.1).is_empty(),
            "already decided"
        );
        for bad in [
            json!({ "task": "t", "verbatim": "x", "form": "summary" }),
            json!({ "task": "t", "item": "t#direct#1", "verbatim": "x" }),
            json!({ "verbatim": "x" }),
        ] {
            assert_eq!(
                e.post(CLAUDE, "/v1/user/decisions", bad.clone()).await.0,
                400,
                "{bad}"
            );
        }
        assert_eq!(
            e.post(
                CLAUDE,
                "/v1/user/decisions",
                json!({ "task": "nope", "verbatim": "x" })
            )
            .await
            .0,
            404
        );
    }

    #[tokio::test]
    async fn needs_user_items_name_who_asks_and_turn_overdue_until_asked() {
        let (notify_url, _calls) = mock_notify(true).await;
        let timing = Timing {
            ask_overdue_after: chrono::Duration::zero(),
            ..Timing::default()
        };
        let (e, _s) = start_push(&notify_url, timing).await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        e.post(
            DSH,
            "/v1/tasks/t/messages",
            json!({ "kind": "progress", "protocol": PROTOCOL,
            "needs_user": ["要不要重启？", { "text": "合并吗？", "relay": "claude" }] }),
        )
        .await;
        let p = e.get(CLAUDE, "/v1/user/pending").await.1;
        let items = p["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["relay"], "dsh", "defaults to whoever raised it");
        assert_eq!(items[1]["relay"], "claude");
        assert!(items.iter().all(|i| i["overdue"] == true));
        let id = items[1]["item"].as_str().unwrap().to_string();
        let (c, v) = e
            .post(CLAUDE, "/v1/user/asked", json!({ "item": id }))
            .await;
        assert_eq!(c, 200, "{v}");
        assert_eq!(v["advanced"], true);
        let (_, v) = e.post(DSH, "/v1/user/asked", json!({ "item": id })).await;
        assert_eq!(v["advanced"], false, "first asker stays on record");
        assert_eq!(v["askedBy"], "claude");
        let p = e.get(DSH, "/v1/user/pending").await.1;
        assert!(p["items"][1]["overdue"].is_null());
        assert_eq!(p["items"][1]["asked_by"], "claude");
        assert_eq!(
            e.post(CLAUDE, "/v1/user/asked", json!({ "item": "t#9#9" }))
                .await
                .0,
            400
        );
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
        // A queued push can be read long after: it leads with "read the current state" and says
        // it is a snapshot, so a superseded message is not taken as news.
        let summary = c[0].1["summary"].as_str().unwrap();
        assert!(
            summary.starts_with("先读当前状态：GET /v1/tasks/t"),
            "{summary}"
        );
        assert!(
            summary.contains("快照") && summary.contains("#1 task 来自 claude"),
            "{summary}"
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
        // The first push does not reach the agent, so a re-wake is due (see dedup B below).
        let (notify_url, calls) = mock_notify(false).await;
        let timing = Timing {
            tick: Duration::from_secs(3600),
            rewake_after: chrono::Duration::minutes(15),
            stall_after: chrono::Duration::hours(24),
            stall_cooldown: chrono::Duration::hours(24),
            ..Timing::default()
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
        assert!(
            stalled
                .iter()
                .all(|c| c["key"].as_str().is_some_and(|k| k.starts_with("stalled:"))),
            "notices are not messages: own dedup key"
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

    #[tokio::test]
    async fn imported_archives_are_neither_unread_nor_pending_for_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let files = dir.path().join("files/old");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(
            files.join("msg-1-claude.md"),
            "taskId: old   n: 1   from: claude   kind: task   protocol: 0\n\n# 旧任务\n",
        )
        .unwrap();
        std::fs::write(
            files.join("msg-2-dsh.json"),
            r#"{"status":"done","summary":"做完","needsUser":["当时要用户定的事"]}"#,
        )
        .unwrap();
        std::fs::write(
            files.join("msg-3-claude.md"),
            "taskId: old   n: 3   from: claude   kind: close   protocol: 0\n\n结案",
        )
        .unwrap();
        let mut b = Bridge::open(dir.path().join("state"), &["claude", "dsh"]).unwrap();
        b.import_task(crate::import::read_task(&files).unwrap())
            .unwrap();
        drop(b);
        let e = start_at(dir).await;
        let h = e.get(CLAUDE, "/v1/health").await.1;
        assert_eq!(
            (
                h["unread"]["claude"].as_u64(),
                h["unread"]["dsh"].as_u64(),
                h["pendingForUser"].as_u64()
            ),
            (Some(0), Some(0), Some(0)),
            "{h}"
        );
        assert_eq!(
            e.get(DSH, "/v1/inbox").await.1["messages"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            e.get(CLAUDE, "/v1/tasks/old").await.1["messages"]
                .as_array()
                .unwrap()
                .len(),
            3,
            "still readable as history"
        );
    }

    #[tokio::test]
    async fn posting_in_a_task_marks_that_task_read() {
        let e = start().await;
        e.post(CLAUDE, "/v1/tasks", task_body("t")).await;
        e.post(CLAUDE, "/v1/tasks", task_body("u")).await;
        let (st, m) = e.post(DSH, "/v1/tasks/t/messages", k("ack")).await;
        assert_eq!(st, 200);
        assert_eq!(
            m["implicit_read"],
            json!({ "task": "t", "from": 1, "to": 2 })
        );
        let inbox = e.get(DSH, "/v1/inbox").await.1;
        let tasks: Vec<&str> = inbox["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["task"].as_str().unwrap())
            .collect();
        assert_eq!(tasks, ["u"], "only the task posted in was marked read");
        let (_, m) = e.post(DSH, "/v1/tasks/t/messages", k("progress")).await;
        assert_eq!(
            m["implicit_read"],
            json!({ "task": "t", "from": 3, "to": 3 })
        );
    }
}

#[cfg(test)]
mod observe_tests {
    use super::tests::{CLAUDE, DSH, Env};
    use super::*;
    use crate::observe::{self, CollectError};

    const OBS: &str = "observer-token-0123456789abcdef0123456789";

    fn app(dir: &std::path::Path) -> AppState {
        let bridge = Bridge::open(dir.join("state"), &["claude", "dsh"]).unwrap();
        AppState::new(
            bridge,
            vec![
                ("claude".into(), sha256(CLAUDE)),
                ("dsh".into(), sha256(DSH)),
            ],
        )
        .unwrap()
        .with_observers(vec![("review-gate".into(), sha256(OBS))])
        .unwrap()
    }

    async fn serve_app(dir: tempfile::TempDir, state: AppState) -> (Env, Arc<AppState>) {
        let state = Arc::new(state);
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

    async fn start() -> (Env, Arc<AppState>) {
        let dir = tempfile::tempdir().unwrap();
        let st = app(dir.path());
        serve_app(dir, st).await
    }

    fn task_body(id: &str) -> Value {
        json!({ "id": id, "kind": "task", "protocol": PROTOCOL, "body": "secret body text",
                "meta": { "to": "dsh", "title": "do it" } })
    }

    async fn task_with_messages(e: &Env, id: &str, acks: usize) {
        assert_eq!(e.post(CLAUDE, "/v1/tasks", task_body(id)).await.0, 200);
        if acks > 0 {
            let (st, v) = e
                .post(
                    DSH,
                    &format!("/v1/tasks/{id}/messages"),
                    json!({ "kind": "ack", "protocol": PROTOCOL, "body": "ack body" }),
                )
                .await;
            assert_eq!(st, 200, "{v}");
        }
        for i in 1..acks {
            let (st, v) = e
                .post(
                    DSH,
                    &format!("/v1/tasks/{id}/messages"),
                    json!({ "kind": "progress", "protocol": PROTOCOL, "body": format!("p{i}") }),
                )
                .await;
            assert_eq!(st, 200, "{v}");
        }
    }

    async fn raw(e: &Env, token: Option<&str>, method: reqwest::Method, path: &str) -> u16 {
        let mut r = e.http.request(method, format!("{}{path}", e.url));
        if let Some(t) = token {
            r = r.bearer_auth(t);
        }
        r.send().await.unwrap().status().as_u16()
    }

    /// One GET with `path` exactly as given (no client-side normalising); returns the status.
    async fn wire(e: &Env, token: &str, path: &str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let addr = e.url.strip_prefix("http://").unwrap();
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        c.write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        out.split(' ').nth(1).unwrap().parse().unwrap()
    }

    fn keys(v: &Value) -> BTreeSet<String> {
        v.as_object().unwrap().keys().cloned().collect()
    }

    fn set(k: &[&str]) -> BTreeSet<String> {
        k.iter().map(|s| s.to_string()).collect()
    }

    /// Every route the router registers, by method (kept in step with the source below).
    const ROUTES: &[(&str, &str)] = &[
        ("GET", "/v1/tasks"),
        ("POST", "/v1/tasks"),
        ("GET", "/v1/tasks/{id}"),
        ("POST", "/v1/tasks/{id}/messages"),
        ("GET", "/v1/inbox"),
        ("POST", "/v1/inbox/ack"),
        ("GET", "/v1/user/pending"),
        ("POST", "/v1/user/decisions"),
        ("POST", "/v1/user/asked"),
        ("POST", "/v1/presence"),
        ("GET", "/v1/health"),
        ("GET", "/v1/observe/tasks"),
        ("GET", "/v1/observe/tasks/{id}"),
    ];

    #[test]
    fn route_list_matches_the_router_source() {
        let src = include_str!("service.rs");
        let body =
            &src[src.find("pub fn router(").unwrap()..src.find("const OBSERVE_LIST").unwrap()];
        let mut found = BTreeSet::new();
        for part in body.split(".route(").skip(1) {
            let path = match part.trim_start().strip_prefix('"') {
                Some(r) => r.split('"').next().unwrap().to_string(),
                None => OBSERVE_LIST.to_string(),
            };
            found.insert(path);
        }
        let listed: BTreeSet<String> = ROUTES.iter().map(|(_, p)| p.to_string()).collect();
        assert_eq!(found, listed, "a route was added without a line in ROUTES");
    }

    #[tokio::test]
    async fn observer_reads_metadata_only() {
        let (e, _) = start().await;
        task_with_messages(&e, "p-1", 0).await;
        let mut child = task_body("t1");
        child["meta"]["parent"] = json!("p-1");
        assert_eq!(e.post(CLAUDE, "/v1/tasks", child).await.0, 200);
        for k in ["ack", "progress"] {
            let (st, v) = e
                .post(
                    DSH,
                    "/v1/tasks/t1/messages",
                    json!({ "kind": k, "protocol": PROTOCOL, "body": "ack body" }),
                )
                .await;
            assert_eq!(st, 200, "{v}");
        }
        let (st, v) = e.get(OBS, "/v1/observe/tasks?after=p-1").await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(keys(&v), set(&["revision", "complete", "tasks", "next"]));
        assert_eq!(v["complete"], true);
        assert_eq!(v["next"], Value::Null);
        let t = &v["tasks"][0];
        assert_eq!(
            keys(t),
            set(&[
                "id",
                "parent",
                "from",
                "to",
                "state",
                "created_at",
                "updated_at",
                "message_count",
                "last_n",
                "participants"
            ])
        );
        assert_eq!(
            (t["id"].as_str(), t["parent"].as_str()),
            (Some("t1"), Some("p-1"))
        );
        assert_eq!(
            (t["message_count"].as_u64(), t["last_n"].as_u64()),
            (Some(3), Some(3))
        );
        assert_eq!(t["participants"], json!(["claude", "dsh"]));
        let (st, d) = e.get(OBS, "/v1/observe/tasks/t1").await;
        assert_eq!(st, 200, "{d}");
        assert_eq!(
            keys(&d),
            set(&["revision", "complete", "task", "message_metadata", "next_n"])
        );
        assert_eq!(&d["task"], t);
        assert_eq!(d["revision"], v["revision"]);
        let ms = d["message_metadata"].as_array().unwrap();
        assert_eq!(ms.len(), 3);
        for m in ms {
            assert_eq!(keys(m), set(&["n", "from", "kind", "at"]));
        }
        assert_eq!(
            (ms[0]["kind"].as_str(), ms[1]["from"].as_str()),
            (Some("task"), Some("dsh"))
        );
        let all = format!("{v}{d}");
        for leak in ["secret body text", "ack body", "do it"] {
            assert!(!all.contains(leak), "{leak} leaked");
        }
    }

    #[tokio::test]
    async fn an_observer_token_does_nothing_else() {
        let (e, _) = start().await;
        task_with_messages(&e, "t1", 1).await;
        use reqwest::Method as M;
        let mut cases: Vec<(M, String)> = ROUTES
            .iter()
            .filter(|(_, p)| !p.starts_with("/v1/observe/"))
            .map(|(m, p)| (m.parse().unwrap(), p.replace("{id}", "t1")))
            .collect();
        for (m, p) in [
            (M::HEAD, "/v1/health"),
            (M::HEAD, "/v1/observe/tasks"),
            (M::HEAD, "/v1/observe/tasks/t1"),
            (M::POST, "/v1/observe/tasks"),
            (M::PUT, "/v1/observe/tasks"),
            (M::DELETE, "/v1/observe/tasks/t1"),
            (M::POST, "/v1/observe/tasks/t1"),
            (M::GET, "/v1/observe"),
            (M::GET, "/v1/observe/"),
            (M::GET, "/v1/observe/tasks/"),
            (M::GET, "/v1/observe/tasks/t1/"),
            (M::GET, "/v1/observe/tasks/t1/messages"),
            (M::GET, "/v1/observe/other"),
            (M::GET, "/v1/nothing-here"),
            (M::GET, "/"),
            (M::GET, "/v1/tasks/t1/messages"),
            (M::PATCH, "/v1/tasks"),
        ] {
            cases.push((m, p.to_string()));
        }
        for (m, p) in cases {
            assert_eq!(raw(&e, Some(OBS), m.clone(), &p).await, 403, "{m} {p}");
        }
        // The same paths still answer as before for everyone else.
        assert_eq!(raw(&e, None, M::GET, "/v1/health").await, 200);
        assert_eq!(raw(&e, Some(CLAUDE), M::GET, "/v1/nothing-here").await, 404);
        assert_eq!(raw(&e, Some(CLAUDE), M::PATCH, "/v1/tasks").await, 405);
        assert_eq!(raw(&e, None, M::GET, "/v1/tasks").await, 401);
    }

    #[tokio::test]
    async fn only_observers_reach_the_observe_routes() {
        use reqwest::Method as M;
        let (e, _) = start().await;
        task_with_messages(&e, "t1", 1).await;
        for p in [
            "/v1/observe/tasks",
            "/v1/observe/tasks/t1",
            "/v1/observe/tasks/nope",
            "/v1/observe",
        ] {
            assert_eq!(raw(&e, Some(CLAUDE), M::GET, p).await, 403, "agent {p}");
            assert_eq!(raw(&e, Some(DSH), M::HEAD, p).await, 403, "agent HEAD {p}");
            assert_eq!(raw(&e, None, M::GET, p).await, 401, "anonymous {p}");
            assert_eq!(raw(&e, Some("wrong"), M::GET, p).await, 401, "invalid {p}");
        }
        assert_eq!(
            raw(&e, Some(OBS), M::GET, "/v1/observe/tasks/nope").await,
            404
        );
        assert_eq!(
            raw(&e, Some(OBS), M::GET, "/v1/observe/tasks/a%2Fb").await,
            400
        );
        assert_eq!(
            raw(&e, Some(OBS), M::GET, "/v1/observe/tasks/a%20b").await,
            400
        );
        // reqwest resolves dot segments before sending; say them on the wire as they are.
        for p in [
            "/v1/observe/tasks/..",
            "/v1/observe/tasks/%2E%2E",
            "/v1/observe/tasks/.hidden",
        ] {
            assert_eq!(wire(&e, OBS, p).await, 400, "{p}");
        }
        assert_eq!(wire(&e, OBS, "/v1/observe/tasks/../../tasks").await, 403);
    }

    #[tokio::test]
    async fn limits_and_queries_are_checked() {
        use reqwest::Method as M;
        let (e, _) = start().await;
        task_with_messages(&e, "t1", 1).await;
        for (p, want) in [
            ("/v1/observe/tasks?limit=0", 400),
            ("/v1/observe/tasks?limit=1", 200),
            ("/v1/observe/tasks?limit=200", 200),
            ("/v1/observe/tasks?limit=201", 400),
            ("/v1/observe/tasks?limit=x", 400),
            ("/v1/observe/tasks?after=a%2Fb", 400),
            ("/v1/observe/tasks?after=t0", 200),
            ("/v1/observe/tasks?other=1", 400),
            ("/v1/observe/tasks/t1?limit=0", 400),
            ("/v1/observe/tasks/t1?limit=201", 200),
            ("/v1/observe/tasks/t1?limit=500", 200),
            ("/v1/observe/tasks/t1?limit=501", 400),
            ("/v1/observe/tasks/t1?after_n=-1", 400),
            ("/v1/observe/tasks/t1?after=t0", 400),
        ] {
            assert_eq!(raw(&e, Some(OBS), M::GET, p).await, want, "{p}");
        }
    }

    #[tokio::test]
    async fn paging_walks_everything_once() {
        let (e, _) = start().await;
        for id in ["c", "a", "e", "b", "d"] {
            task_with_messages(&e, id, 0).await;
        }
        task_with_messages(&e, "f", 5).await;
        let mut ids = Vec::new();
        let mut path = "/v1/observe/tasks?limit=2".to_string();
        loop {
            let (st, v) = e.get(OBS, &path).await;
            assert_eq!(st, 200);
            for t in v["tasks"].as_array().unwrap() {
                ids.push(t["id"].as_str().unwrap().to_string());
            }
            match v["next"].as_str() {
                Some(n) => path = format!("/v1/observe/tasks?limit=2&after={n}"),
                None => break,
            }
        }
        assert_eq!(ids, ["a", "b", "c", "d", "e", "f"]);
        let (_, d) = e.get(OBS, "/v1/observe/tasks/f?limit=4").await;
        assert_eq!(d["message_metadata"].as_array().unwrap().len(), 4);
        assert_eq!(d["next_n"], 4);
        let (_, d) = e.get(OBS, "/v1/observe/tasks/f?limit=4&after_n=4").await;
        let ns: Vec<u64> = d["message_metadata"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["n"].as_u64().unwrap())
            .collect();
        assert_eq!((ns, d["next_n"].clone()), (vec![5, 6], Value::Null));
    }

    fn files(dir: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.insert(p.display().to_string(), std::fs::read(&p).unwrap());
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn observer_requests_change_nothing() {
        use reqwest::Method as M;
        let (url, calls) = super::tests::mock_notify(true).await;
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("dsh-notify.token");
        std::fs::write(&token_file, "notify-token-0123456789abcdef0123456789\n").unwrap();
        let st = app(dir.path()).with_push(BTreeMap::from([(
            "dsh".to_string(),
            Target {
                url,
                token_file,
                transport: notify::Transport::Native,
            },
        )]));
        let root = dir.path().to_path_buf();
        let (e, state) = serve_app(dir, st).await;
        task_with_messages(&e, "t1", 2).await;
        super::tests::settle().await;
        state.tick(Utc::now() + chrono::Duration::hours(2));
        super::tests::settle().await;
        let (disk, mem, pushes) = (
            files(&root),
            state.side_state(),
            calls.lock().unwrap().len(),
        );
        for p in [
            "/v1/observe/tasks",
            "/v1/observe/tasks/t1",
            "/v1/observe/tasks/nope",
            "/v1/health",
            "/v1/inbox",
            "/v1/observe/tasks?limit=0",
        ] {
            raw(&e, Some(OBS), M::GET, p).await;
            raw(&e, Some(OBS), M::HEAD, p).await;
            raw(&e, Some(OBS), M::POST, p).await;
        }
        raw(&e, Some(CLAUDE), M::GET, "/v1/observe/tasks").await;
        raw(&e, Some("wrong"), M::GET, "/v1/observe/tasks").await;
        super::tests::settle().await;
        assert_eq!(state.side_state(), mem, "memory changed");
        assert!(files(&root) == disk, "disk changed");
        assert_eq!(calls.lock().unwrap().len(), pushes, "a push was made");
        // An agent's own request still records presence (the check above is not vacuous).
        e.get(CLAUDE, "/v1/tasks").await;
        assert_ne!(state.side_state(), mem);
    }

    #[tokio::test]
    async fn incomplete_history_is_refused() {
        use reqwest::Method as M;
        let dir = tempfile::tempdir().unwrap();
        {
            let st = app(dir.path());
            let mut g = st.lock();
            g.bridge
                .create_task(
                    "t1",
                    "claude",
                    serde_json::from_value(json!({
                        "protocol": PROTOCOL, "body": "b", "meta": { "to": "dsh", "title": "x" }
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("state/tasks/t1.jsonl"))
            .unwrap();
        writeln!(f, "garbage").unwrap();
        drop(f);
        let st = app(dir.path());
        let (e, _) = serve_app(dir, st).await;
        for p in [
            "/v1/observe/tasks",
            "/v1/observe/tasks/t1",
            "/v1/observe/tasks/nope",
        ] {
            let (code, v) = e.get(OBS, p).await;
            assert_eq!(
                (code, v["error"].as_str()),
                (503, Some("history_incomplete")),
                "{p}"
            );
        }
        assert_eq!(raw(&e, Some(CLAUDE), M::GET, "/v1/tasks").await, 200);
    }

    async fn via(e: &Env, path: String) -> Result<(u16, Value), String> {
        Ok(e.get(OBS, &path).await)
    }

    #[tokio::test]
    async fn collect_reads_everything_at_one_revision() {
        let (e, _) = start().await;
        for i in 0..7 {
            task_with_messages(&e, &format!("t{i}"), i).await;
        }
        let c = observe::collect(|p| via(&e, p)).await.unwrap();
        assert_eq!(c.tasks.len(), 7);
        assert_eq!(c.tasks[3].1.len(), 4);
        let (_, v) = e.get(OBS, "/v1/observe/tasks").await;
        assert_eq!(v["revision"].as_str(), Some(c.revision.as_str()));
    }

    #[tokio::test]
    async fn revision_survives_a_restart_and_follows_any_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let st = app(&root);
        let (e, state) = serve_app(dir, st).await;
        for id in ["a", "m", "z"] {
            task_with_messages(&e, id, 1).await;
        }
        let r0 = e.get(OBS, "/v1/observe/tasks?limit=1").await.1["revision"].clone();
        drop(state);
        let again = app(&root);
        let r1 = {
            let g = again.lock();
            observe::revision(&observe::observe_all(&g.bridge))
        };
        assert_eq!(r0.as_str(), Some(r1.as_str()), "restart, same content");
        // A change on the last page shows on the first.
        e.post(
            DSH,
            "/v1/tasks/z/messages",
            json!({ "kind": "progress", "protocol": PROTOCOL, "body": "x" }),
        )
        .await;
        let r2 = e.get(OBS, "/v1/observe/tasks?limit=1").await.1["revision"].clone();
        assert_ne!(r0, r2);
        // A task inserted before a cursor too.
        task_with_messages(&e, "0", 0).await;
        let r3 = e.get(OBS, "/v1/observe/tasks?limit=1&after=m").await.1["revision"].clone();
        assert_ne!(r2, r3);
    }

    #[tokio::test]
    async fn collect_rereads_after_a_change_and_gives_up_on_churn() {
        let (e, _) = start().await;
        for id in ["a", "b", "c"] {
            task_with_messages(&e, id, 1).await;
        }
        // One change after the first page: a second round succeeds, and sees it.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let c = observe::collect(|p| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let e = &e;
            async move {
                let r = e.get(OBS, &p).await;
                if n == 0 {
                    e.post(
                        DSH,
                        "/v1/tasks/c/messages",
                        json!({ "kind": "progress", "protocol": PROTOCOL, "body": "x" }),
                    )
                    .await;
                }
                Ok(r)
            }
        })
        .await
        .unwrap();
        assert_eq!(c.tasks[2].1.len(), 3);
        // A change after the last detail page is caught only by the final re-read of the list:
        // a clean read is 1 list page + 3 details + 1 re-read.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let c = observe::collect(|p| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let e = &e;
            async move {
                let r = e.get(OBS, &p).await;
                if n == 3 {
                    e.post(
                        DSH,
                        "/v1/tasks/a/messages",
                        json!({ "kind": "progress", "protocol": PROTOCOL, "body": "y" }),
                    )
                    .await;
                }
                Ok(r)
            }
        })
        .await
        .unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            10,
            "read twice"
        );
        assert_eq!(c.tasks[0].1.len(), 3);
        // A change on every round's first request: never one revision.
        let k = std::sync::atomic::AtomicUsize::new(0);
        let r = observe::collect(|p| {
            let e = &e;
            let n = k.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                let r = e.get(OBS, &p).await;
                let id = format!("n{n}");
                task_with_messages(e, &id, 0).await;
                Ok(r)
            }
        })
        .await;
        assert_eq!(r, Err(CollectError::Unstable));
    }

    type Pages = Vec<(&'static str, u16, Value)>;

    /// A fake service: answers from `pages` by path.
    async fn collect_from(
        pages: Vec<(&'static str, u16, Value)>,
    ) -> Result<observe::Collected, CollectError> {
        let pages = std::sync::Mutex::new(pages);
        observe::collect(|p| {
            let found = pages
                .lock()
                .unwrap()
                .iter()
                .find(|(k, _, _)| *k == p)
                .map(|(_, s, v)| (*s, v.clone()));
            async move { found.ok_or(format!("unexpected {p}")) }
        })
        .await
    }

    fn t(id: &str, count: u32) -> Value {
        json!({ "id": id, "parent": null, "from": "claude", "to": "dsh", "state": "open",
                "created_at": "2026-10-18T00:00:00Z", "updated_at": "2026-10-18T00:00:00Z",
                "message_count": count, "last_n": count, "participants": ["claude", "dsh"] })
    }

    fn m(n: u32) -> Value {
        json!({ "n": n, "from": "claude", "kind": "task", "at": "2026-10-18T00:00:00Z" })
    }

    const L: &str = "/v1/observe/tasks?limit=200";

    fn list(tasks: Vec<Value>, next: Value) -> Value {
        json!({ "revision": "r", "complete": true, "tasks": tasks, "next": next })
    }

    fn detail(task: Value, msgs: Vec<Value>, next: Value) -> Value {
        json!({ "revision": "r", "complete": true, "task": task, "message_metadata": msgs, "next_n": next })
    }

    #[tokio::test]
    async fn collect_refuses_what_it_cannot_trust() {
        let ok_a = (
            "/v1/observe/tasks/a?limit=500",
            200,
            detail(t("a", 1), vec![m(1)], Value::Null),
        );
        // The good case first, so the failures below are about one thing each.
        assert!(
            collect_from(vec![
                (L, 200, list(vec![t("a", 1)], Value::Null)),
                ok_a.clone()
            ])
            .await
            .is_ok()
        );
        let cases: Vec<(&str, Pages, CollectError)> = vec![
            ("503", vec![(L, 503, json!({}))], CollectError::Incomplete),
            (
                "complete false",
                vec![(
                    L,
                    200,
                    json!({ "revision": "r", "complete": false, "tasks": [], "next": null }),
                )],
                CollectError::Protocol("complete is not true".into()),
            ),
            (
                "other status",
                vec![(L, 403, json!({}))],
                CollectError::Protocol("status 403".into()),
            ),
            (
                "repeated task",
                vec![
                    (L, 200, list(vec![t("a", 1), t("a", 1)], Value::Null)),
                    ok_a.clone(),
                ],
                CollectError::Protocol("task a out of order or repeated".into()),
            ),
            (
                "cursor not moving",
                vec![
                    (L, 200, list(vec![t("a", 1)], json!("a"))),
                    (
                        "/v1/observe/tasks?limit=200&after=a",
                        200,
                        list(vec![t("a", 1)], Value::Null),
                    ),
                    ok_a.clone(),
                ],
                CollectError::Protocol("task a out of order or repeated".into()),
            ),
            (
                "next not last",
                vec![(L, 200, list(vec![t("a", 1)], json!("b")))],
                CollectError::Protocol("next is not the last task of the page".into()),
            ),
            (
                "count mismatch",
                vec![
                    (L, 200, list(vec![t("a", 2)], Value::Null)),
                    (
                        "/v1/observe/tasks/a?limit=500",
                        200,
                        detail(t("a", 2), vec![m(1)], Value::Null),
                    ),
                ],
                CollectError::Protocol("task a: 1 messages listed, message_count 2".into()),
            ),
            (
                "repeated message",
                vec![
                    (L, 200, list(vec![t("a", 2)], Value::Null)),
                    (
                        "/v1/observe/tasks/a?limit=500",
                        200,
                        detail(t("a", 2), vec![m(1), m(1)], Value::Null),
                    ),
                ],
                CollectError::Protocol("task a: message 1 out of order or repeated".into()),
            ),
            (
                "detail differs",
                vec![
                    (L, 200, list(vec![t("a", 1)], Value::Null)),
                    (
                        "/v1/observe/tasks/a?limit=500",
                        200,
                        detail(t("a", 2), vec![m(1)], Value::Null),
                    ),
                ],
                CollectError::Protocol("task a differs from the list at one revision".into()),
            ),
            (
                "unknown field",
                vec![(
                    L,
                    200,
                    list(
                        vec![{
                            let mut x = t("a", 1);
                            x["body"] = json!("b");
                            x
                        }],
                        Value::Null,
                    ),
                )],
                CollectError::Protocol(String::new()),
            ),
        ];
        for (name, pages, want) in cases {
            let got = collect_from(pages).await.unwrap_err();
            match (&want, &got) {
                (CollectError::Protocol(w), CollectError::Protocol(g)) => {
                    assert!(g.contains(w.as_str()), "{name}: {g}")
                }
                _ => assert_eq!(got, want, "{name}"),
            }
        }
    }
}
