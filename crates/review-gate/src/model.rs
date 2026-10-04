//! Data model: tasks (one auto-iterate run), review records and history.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a review came from. Strength follows dsh-web-relay's `--min-reviewer` scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Channel {
    /// A model from another vendor via its API (Gemini API, OpenAI-compatible, DeepSeek).
    ExternalApi,
    /// Gemini in the browser through the dsh-web-relay bridge.
    WebGemini,
    /// A fresh context of the implementer's own model family.
    ClaudeSubagent,
    /// The implementer reviewing itself.
    SelfReview,
    /// A human verdict, accepted only from a local administrator session of the gate.
    Manual,
}

impl Channel {
    pub fn strength(self) -> u8 {
        match self {
            Channel::Manual => 5,
            Channel::ExternalApi => 4,
            Channel::WebGemini => 3,
            Channel::ClaudeSubagent => 2,
            Channel::SelfReview => 1,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::ExternalApi => "external-api",
            Channel::WebGemini => "web-gemini",
            Channel::ClaudeSubagent => "claude-subagent",
            Channel::SelfReview => "self-review",
            Channel::Manual => "manual",
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.as_str(), self.strength())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Paused,
    Done,
}

/// One review of one version's staged changes. Created only by the gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewRecord {
    /// `v<iteration>-<attempt>`, unique within the task.
    pub id: String,
    pub task: String,
    pub iteration: u32,
    pub attempt: u32,
    /// `git write-tree` of the staged changes that were reviewed.
    pub tree: String,
    /// HEAD at review time; the approved commit must have exactly this parent.
    pub base: String,
    pub verdict: Verdict,
    pub channel: Channel,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    pub family: String,
    /// sha256 of the prompt the reviewer saw (empty for submitted reviews).
    #[serde(default)]
    pub prompt_sha: String,
    pub text: String,
    pub at: DateTime<Utc>,
}

/// What the state machine did with one review record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub iteration: u32,
    pub outcome: Outcome,
    pub review: String,
    pub channel: Channel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Approved,
    Rejected,
    /// The reviewer channel was below the task's threshold: paused for a human.
    InsufficientReviewer,
}

/// One auto-iterate run over a repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    #[serde(default)]
    pub expr_id: Option<String>,
    pub goal: String,
    pub final_acceptance: String,
    pub repo: String,
    pub iterations: u32,
    pub current_iteration: u32,
    pub reject_streak: u32,
    pub min_reviewer: Channel,
    /// Reviewer provider preference (`auto`, `web-gemini`, `gemini-api`, `openai`).
    pub review_provider: String,
    /// Model family of the implementer; reviews from this family are "same-family".
    pub implementer_family: String,
    pub status: Status,
    #[serde(default)]
    pub stop_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
}

/// What the implementer must do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    StartRound,
    RetrySameRound,
    Finalize,
    Pause,
}
