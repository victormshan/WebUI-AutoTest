//! Tasks and messages. A task is nothing but its messages: message 1 (`kind: task`) carries the
//! task's metadata, every later message is appended, never edited. The task's current state is
//! derived by replaying them, so the index can always be rebuilt from the log.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Opens the task (requester).
    Task,
    /// Receiver read it and restates what it will do.
    Ack,
    /// Receiver asks; blocks until the requester answers.
    Question,
    /// Requester answers a question.
    Answer,
    /// Receiver reports progress (does not wake anyone).
    Progress,
    /// Receiver's outcome with evidence: done | blocked | rejected.
    Result,
    /// Requester's verification of a result: pass | rework.
    Verdict,
    /// Requester closes the task.
    Close,
    /// Requester withdraws the task.
    Cancel,
    /// A task paused for the user resumes after the user's recorded decision. Only the service
    /// writes it (from `POST /v1/user/decisions`), never an agent directly.
    Resume,
    /// Either party adds information without changing the task's state (P1). It must reply to an
    /// earlier message of the task and cannot carry a new instruction (that is a new task).
    Note,
}

impl Kind {
    /// Kinds only the task's receiver may send; the rest belong to the requester.
    pub fn from_receiver(self) -> bool {
        matches!(
            self,
            Kind::Ack | Kind::Question | Kind::Progress | Kind::Result
        )
    }

    /// Default wake level when the sender does not choose one (Q2): kinds the other side has to
    /// act on wake it; acks, progress, closing and notes are informational.
    pub fn default_wake(self) -> WakeLevel {
        match self {
            Kind::Ack | Kind::Progress | Kind::Close | Kind::Resume | Kind::Note => {
                WakeLevel::Quiet
            }
            _ => WakeLevel::Normal,
        }
    }
}

/// How hard to wake the recipient (Q2). The DSH main agent is interrupted by a wake (a turn is
/// appended), Claude only stops waiting, so the sender decides how much a message warrants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeLevel {
    /// Stored only; the recipient sees it when it next reads.
    Quiet,
    /// Pushed; the recipient may coalesce it with others under its cooldown.
    Normal,
    /// Pushed and meant to wake the recipient at once.
    Urgent,
}

impl WakeLevel {
    pub fn pushes(self) -> bool {
        self != WakeLevel::Quiet
    }
}

/// Lifecycle phases a receiver can announce in a progress message (Q1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// About to restart (deliver + restart): the channel may be unreachable for a while.
    PendingRestart,
    /// Back after the restart.
    Restarted,
}

/// Who asks the user about a needs_user item (Q5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relay {
    Claude,
    Dsh,
    Either,
}

/// Something only the user may decide. Older messages carry a bare string; that is still read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "NeedsUserRepr")]
pub struct NeedsUser {
    pub text: String,
    /// Who is responsible for asking the user; unset means whoever raised it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<Relay>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NeedsUserRepr {
    Text(String),
    Full {
        text: String,
        #[serde(default)]
        relay: Option<Relay>,
    },
}

impl From<NeedsUserRepr> for NeedsUser {
    fn from(r: NeedsUserRepr) -> Self {
        match r {
            NeedsUserRepr::Text(text) => NeedsUser { text, relay: None },
            NeedsUserRepr::Full { text, relay } => NeedsUser { text, relay },
        }
    }
}

impl From<&str> for NeedsUser {
    fn from(s: &str) -> Self {
        NeedsUser {
            text: s.into(),
            relay: None,
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            serde_json::to_value(self)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .as_deref()
                .unwrap_or("?"),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Open,
    Acked,
    Working,
    AwaitingAnswer,
    AwaitingVerdict,
    Verified,
    /// Over a limit (rounds or messages): only the user can move it on; requester may close/cancel.
    PausedForUser,
    /// The receiver announced a restart (Q1): not stalled, no reminders until it is back.
    AwaitingRestart,
    Closed,
    Cancelled,
}

impl State {
    pub fn terminal(self) -> bool {
        matches!(self, State::Closed | State::Cancelled)
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            serde_json::to_value(self)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .as_deref()
                .unwrap_or("?"),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    /// Some items delivered, the rest have follow-ups (P7).
    Partial,
    Blocked,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Judgement {
    Pass,
    Rework,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub blocking: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultItem {
    pub item: String,
    /// done | skipped | failed | needs-user
    pub status: String,
    /// Re-runnable evidence: command, output, file:line. The receiver verifies it independently.
    pub evidence: String,
    /// For an unfinished item of a partial result: the follow-up task id or needs_user text (P7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<String>,
}

impl ResultItem {
    /// done and skipped are finished; anything else (failed, needs-user, pending …) is not.
    pub fn finished(&self) -> bool {
        matches!(self.status.as_str(), "done" | "skipped")
    }
}

/// Metadata carried by message 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskMeta {
    pub to: String,
    pub title: String,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    /// claude-step-relay experiment this task belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr_id: Option<String>,
    /// The task this one was derived from (P6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// One appended message, as stored (one JSON line) and served.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub task: String,
    /// Assigned by the service: 1, 2, 3 … per task.
    pub n: u32,
    pub from: String,
    pub kind: Kind,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<TaskMeta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<Question>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<ResultItem>,
    /// Things only the user may decide; neither agent decides them for the user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs_user: Vec<NeedsUser>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judgement: Option<Judgement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u32>,
    /// An earlier message of the same sender this one replaces (kept, marked superseded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<u32>,
    /// Sender's idempotency key: re-posting the same key returns the stored message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_msg_id: Option<String>,
    /// Sender's session generation; a change means "new session resending the same intent".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_epoch: Option<String>,
    /// How hard to wake the recipient; unset means the kind's default (Q2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<WakeLevel>,
    /// Lifecycle phase announced with a progress message (Q1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    pub protocol: String,
    pub at: DateTime<Utc>,
    /// Imported from the old file protocol: kept as a record, never replayed through the
    /// state machine (those tasks were not run under its rules).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub imported: bool,
}

impl Message {
    /// The wake level that applies: the sender's choice, else the kind's default.
    pub fn effective_wake(&self) -> WakeLevel {
        self.wake.unwrap_or(self.kind.default_wake())
    }
}

impl std::fmt::Display for NeedsUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.relay {
            Some(r) => write!(
                f,
                "{}（由 {} 去问用户）",
                self.text,
                serde_json::to_value(r)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            ),
            None => f.write_str(&self.text),
        }
    }
}

/// Derived view of a task.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Task {
    pub id: String,
    pub from: String,
    pub to: String,
    pub title: String,
    pub priority: Priority,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expr_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
    pub question_rounds: u32,
    /// Rounds and messages counted from here on (moved forward when the user resumes the task).
    #[serde(skip_serializing_if = "is_zero")]
    pub limit_base_rounds: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub limit_base_messages: u32,
    pub messages: u32,
    pub last_n: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Message numbers replaced by a later `supersedes`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub superseded: Vec<u32>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}
