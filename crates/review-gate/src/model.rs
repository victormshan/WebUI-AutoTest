//! Data model: tasks (one auto-iterate run), review records and history.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::identity::canonical_family;

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
    /// Signed attestation text (approved versions, when the gate has a signing key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<String>,
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
    /// The authenticated principal that created the task; frozen at creation. Tasks from older
    /// versions have no such field and belong to `legacy-claude` (see [`Task::owner_principal`]).
    #[serde(default)]
    pub principal: String,
    /// Non-secret accountability: which credential created the task and which configuration
    /// content was in force. Older tasks have neither.
    #[serde(default)]
    pub credential: String,
    #[serde(default)]
    pub config_digest: Option<String>,
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

impl Task {
    /// The principal that owns this task. Unfrozen legacy tasks belong only to `legacy-claude`;
    /// they cannot be claimed by any other identity.
    pub fn owner_principal(&self) -> &str {
        if self.principal.is_empty() {
            crate::identity::LEGACY_PRINCIPAL
        } else {
            &self.principal
        }
    }

    pub fn canonical_implementer_family(&self) -> Option<&'static str> {
        canonical_family(&self.implementer_family)
    }
}

/// The single eligibility rule for a review channel under implementer neutrality.
///
/// - `external-api` and `web-gemini` must come from a **known** family that is not the frozen
///   implementer family; unknown families are no independent review, and same-family is refused.
/// - `claude-subagent` and `self-review` belong to the implementer's own family, with their
///   existing strengths (2 and 1); a mismatched claim is replaced by the frozen family.
/// - `manual` is limited to the administrator.
///
/// On success returns the canonical family the record may carry.
pub fn review_eligibility(
    implementer_family: &str,
    channel: Channel,
    reviewer_family: &str,
    caller_is_admin: bool,
) -> Result<String, String> {
    let implementer = canonical_family(implementer_family).ok_or_else(|| {
        format!(
            "unknown implementer family {implementer_family:?}: a task must freeze a model vendor"
        )
    })?;
    match channel {
        Channel::Manual => {
            if !caller_is_admin {
                return Err("manual reviews are accepted only from the administrator".into());
            }
            Ok("human".into())
        }
        Channel::ExternalApi | Channel::WebGemini => {
            // One rule: do not keep a second copy of the vendor comparison here.
            external_family_ok(implementer_family, reviewer_family).map(str::to_string)
        }
        Channel::ClaudeSubagent | Channel::SelfReview => Ok(implementer.to_string()),
    }
}

/// The external-only part of the rule, also used before a provider is contacted.
pub fn external_family_ok(
    implementer_family: &str,
    reviewer_family: &str,
) -> Result<&'static str, String> {
    let implementer = canonical_family(implementer_family).ok_or_else(|| {
        format!(
            "unknown implementer family {implementer_family:?}: a task must freeze a model vendor"
        )
    })?;
    let reviewer = canonical_family(reviewer_family).ok_or_else(|| {
        format!(
            "unknown reviewer family {reviewer_family:?} is not a known model family; the administrator must configure the model vendor of this endpoint"
        )
    })?;
    if reviewer == implementer {
        return Err(format!(
            "reviewer family {reviewer} is the implementer's family; a review from the same vendor is not independent"
        ));
    }
    Ok(reviewer)
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
