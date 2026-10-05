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
}

impl Kind {
    /// Kinds only the task's receiver may send; the rest belong to the requester.
    pub fn from_receiver(self) -> bool {
        matches!(
            self,
            Kind::Ack | Kind::Question | Kind::Progress | Kind::Result
        )
    }

    /// Kinds the other side has to act on, and therefore wake it. Acks, progress and closing are
    /// informational and never wake anyone (design §5: no wake storms).
    pub fn wakes(self) -> bool {
        !matches!(
            self,
            Kind::Ack | Kind::Progress | Kind::Close | Kind::Resume
        )
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
    pub needs_user: Vec<String>,
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
    pub protocol: String,
    pub at: DateTime<Utc>,
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
