//! agent-bridge: two-way task dispatch and collaboration between agents — Claude Code and the
//! DSH main agent. Either side can hand the other a task; the service, not the agents' good
//! intentions, enforces who may say what and when (design: `DESIGN-agent-bridge-FINAL.md`,
//! agreed with DSH over the file protocol it replaces).
//!
//! It is a coordination service, not a security boundary: whether code may be committed is still
//! decided by each side's own review gate.

pub mod bridge;
pub mod client;
pub mod import;
pub mod inbox;
pub mod mcp;
pub mod mirror;
pub mod model;
pub mod notify;
pub mod service;
pub mod store;
pub mod user;

/// Message protocol spoken by this service. The file protocol it replaces was "0".
pub const PROTOCOL: &str = "2";
/// Protocols accepted: "1" (first release) stays valid while clients move to "2".
pub const PROTOCOLS: &[&str] = &["1", "2"];
/// Question/answer rounds per task before it is handed to the user.
pub const MAX_QUESTION_ROUNDS: u32 = 3;
/// Messages per task before it is handed to the user.
/// Safety cap on all messages of a task (Q6: the real limit is on question rounds).
pub const MAX_MESSAGES: u32 = 50;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error(
        "invalid id: {0:?} (letters, digits, '.', '_', '-'; must start with a letter or digit; max 64)"
    )]
    InvalidId(String),
    #[error("unknown agent: {0}")]
    UnknownAgent(String),
    #[error("no such task: {0}")]
    NoSuchTask(String),
    #[error("task already exists: {0}")]
    Exists(String),
    #[error("not allowed: {0}")]
    Forbidden(String),
    #[error("not allowed in state {state}: {kind} ({why})")]
    Transition {
        state: model::State,
        kind: model::Kind,
        why: String,
    },
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] anyhow::Error),
}

pub fn valid_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}
