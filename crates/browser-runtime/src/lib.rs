//! browser-runtime: Chrome under CDP for the review channel and, later, webtest (ADR-0).
//!
//! A1 spike scope: launch a Chrome this process owns, talk CDP over `--remote-debugging-pipe`
//! (no debugging port: on a shared machine a port would let any local user drive the
//! reviewer's logged-in browser), observe events, and shut the whole process group down.
//! Site logic (selectors, completion) belongs to the callers' adapters, not here.

pub mod browser;
pub mod config;
pub mod error;
mod pipe;

pub use browser::Browser;
pub use config::LaunchConfig;
pub use error::{Error, Result};
