//! review-gate: the independent review gate for auto-iterate, aligned with the dsh-web-relay
//! three-party protocol. Claude (implementer) changes code; a model from another vendor
//! reviews each version; this gate — running as its own system user, with a private store —
//! decides whether a version may be committed.

pub mod gate;
pub mod git;
pub mod model;
pub mod review;
pub mod reviewer;
pub mod store;

use model::Status;

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("invalid id: {0:?} (letters, digits, '.', '_', '-'; no leading dot; max 64)")]
    InvalidId(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("no such task: {0}")]
    NoSuchTask(String),
    #[error("no such review: {0}")]
    NoSuchReview(String),
    #[error("task {0} is {1:?}, not running")]
    NotRunning(String, Status),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("rejected: {0}")]
    Mismatch(String),
    #[error("git: {0}")]
    Git(String),
    #[error(transparent)]
    Io(#[from] anyhow::Error),
}
