//! Error categories (subset of the baseline's model, §3.4). Messages carry no page content,
//! cookies or prompt text.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The configuration is unusable (path missing, profile dir invalid, …).
    #[error("invalid configuration: {0}")]
    Config(String),
    /// Chrome could not be started.
    #[error("failed to launch Chrome: {0}")]
    Launch(String),
    /// The pipe to Chrome closed (Chrome exited, crashed or was killed).
    #[error("browser disconnected")]
    BrowserDisconnected,
    /// A command got no response in time. The command's effect is unknown.
    #[error("CDP command {method} timed out after {millis} ms (effect unknown)")]
    Timeout { method: String, millis: u128 },
    /// Chrome answered with a protocol error.
    #[error("CDP error {code} on {method}: {message}")]
    Protocol {
        method: String,
        code: i64,
        message: String,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Whether retrying the same operation may help. A timeout is not retryable blindly:
    /// the command may have taken effect.
    pub fn retryable(&self) -> bool {
        matches!(self, Error::Launch(_))
    }
}
