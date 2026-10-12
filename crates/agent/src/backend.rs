//! The browser seam: everything the agent, replay and explore need from a
//! browser, behind one trait so a second backend can be compared against the
//! default one (B1).
//!
//! [`McpBackend`] (chrome-devtools-mcp, i.e. [`mcp_client::Browser`]) is the
//! default and only backend so far; its behavior is unchanged except that a
//! diagnostics read that fails is now reported instead of read as "no errors".

use anyhow::Result;
use async_trait::async_trait;
use mcp_client::{StorageState, ToolInfo, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The chrome-devtools-mcp backend.
pub type McpBackend = mcp_client::Browser;

#[async_trait]
pub trait BrowserBackend: Send + Sync {
    /// Tool catalog (names + JSON Schema of args) shown to the LLM.
    async fn tools(&self) -> Result<Vec<ToolInfo>>;
    /// Calls a tool. Tool-level failures come back as `is_error`, not `Err`.
    async fn call(&self, tool: &str, args: Value) -> Result<ToolOutput>;
    async fn navigate(&self, url: &str) -> Result<ToolOutput>;
    /// Accessibility-tree snapshot; element uids in it are valid until the next snapshot.
    async fn snapshot(&self) -> Result<String>;
    /// All cookies (including HttpOnly) plus localStorage of the current origin.
    async fn storage_state(&self) -> Result<StorageState>;
    async fn apply_storage_state(&self, state: &StorageState) -> Result<()>;
    /// Console errors and failed requests since the page was opened, normalized
    /// (ids and arg counts stripped) so runs can be compared.
    ///
    /// Never fails as a whole: a channel that cannot be read (tool error,
    /// transport error, disconnected browser) is reported in its [`Coverage`],
    /// and the other channel is still read.
    async fn diagnostics(&self) -> Diagnostics;
    async fn close(self: Box<Self>) -> Result<()>;
}

/// Built-in checks that need no assertions: console errors and HTTP failures.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    pub console_errors: Vec<String>,
    pub failed_requests: Vec<String>,
    /// Whether `console_errors` is the whole story. An empty list only means
    /// "no console errors" when this is [`Coverage::Complete`].
    #[serde(default)]
    pub console: Coverage,
    #[serde(default)]
    pub network: Coverage,
}

/// How much of one diagnostics channel was actually read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Coverage {
    /// Read in full; the list holds every error there was.
    Complete,
    /// Read, but some entries may be missing.
    Partial { reason: String },
    /// The backend cannot read this channel at all.
    Unsupported,
    /// Reading failed or timed out; the list says nothing.
    Failed { reason: String },
    /// Never read (traces written before coverage was recorded, or not collected yet).
    #[default]
    NotRecorded,
}

impl Diagnostics {
    /// Channels that were not read in full, as `"<channel>: <why>"`.
    pub fn gaps(&self) -> Vec<String> {
        [("console", &self.console), ("network", &self.network)]
            .into_iter()
            .filter_map(|(name, c)| c.gap().map(|why| format!("{name}: {why}")))
            .collect()
    }
}

impl Coverage {
    fn gap(&self) -> Option<String> {
        match self {
            Coverage::Complete => None,
            Coverage::Partial { reason } => Some(format!("partial ({reason})")),
            Coverage::Unsupported => Some("not supported by this backend".into()),
            Coverage::Failed { reason } => Some(format!("could not be read ({reason})")),
            Coverage::NotRecorded => Some("not recorded".into()),
        }
    }
}

#[async_trait]
impl BrowserBackend for McpBackend {
    async fn tools(&self) -> Result<Vec<ToolInfo>> {
        mcp_client::Browser::tools(self).await
    }
    async fn call(&self, tool: &str, args: Value) -> Result<ToolOutput> {
        mcp_client::Browser::call(self, tool, args).await
    }
    async fn navigate(&self, url: &str) -> Result<ToolOutput> {
        mcp_client::Browser::navigate(self, url).await
    }
    async fn snapshot(&self) -> Result<String> {
        mcp_client::Browser::snapshot(self).await
    }
    async fn storage_state(&self) -> Result<StorageState> {
        mcp_client::Browser::storage_state(self).await
    }
    async fn apply_storage_state(&self, state: &StorageState) -> Result<()> {
        mcp_client::Browser::apply_storage_state(self, state).await
    }
    async fn diagnostics(&self) -> Diagnostics {
        let console = mcp_client::Browser::call(self, "list_console_messages", json!({})).await;
        let network = mcp_client::Browser::call(self, "list_network_requests", json!({})).await;
        let d = mcp_diagnostics(console, network);
        // Checked after the calls: they may be the first to hear of a restart.
        if self.restarted() {
            lost_to_restart(d)
        } else {
            d
        }
    }
    async fn close(self: Box<Self>) -> Result<()> {
        mcp_client::Browser::close(*self).await
    }
}

/// How chrome-devtools-mcp prints one list tool.
struct McpList {
    /// Prefix of every entry line (`msgid=`, `reqid=`).
    entry: &'static str,
    /// What the tool prints instead of a list when there is nothing.
    empty: &'static str,
}

const CONSOLE: McpList = McpList {
    entry: "msgid=",
    empty: "<no console messages found>",
};
const NETWORK: McpList = McpList {
    entry: "reqid=",
    empty: "No requests found.",
};

/// Builds diagnostics from the two list tools' results; either call may have failed.
fn mcp_diagnostics(console: Result<ToolOutput>, network: Result<ToolOutput>) -> Diagnostics {
    let (console_errors, console) = mcp_channel(console, CONSOLE, |l| {
        l.contains(" [error] ")
            .then(|| strip_arg_count(strip_id(l)).to_string())
    });
    let (failed_requests, network) = mcp_channel(network, NETWORK, |l| {
        http_failed(l).then(|| strip_id(l).to_string())
    });
    Diagnostics {
        console_errors,
        failed_requests,
        console,
        network,
    }
}

/// After a browser restart both lists only cover the new browser, so neither
/// can vouch for the run.
fn lost_to_restart(mut d: Diagnostics) -> Diagnostics {
    let lost = || Coverage::Failed {
        reason: "the browser restarted during the run; entries from before the restart are lost"
            .into(),
    };
    d.console = lost();
    d.network = lost();
    d
}

/// Picks the errors out of a list tool's output and says how complete the
/// list was. A failed call or output we do not recognize is a failed read,
/// never "no errors".
fn mcp_channel(
    out: Result<ToolOutput>,
    list: McpList,
    pick: impl Fn(&str) -> Option<String>,
) -> (Vec<String>, Coverage) {
    let out = match out {
        Ok(out) => out,
        Err(e) => {
            let reason = format!("{e:#}").chars().take(200).collect();
            return (Vec::new(), Coverage::Failed { reason });
        }
    };
    if out.is_error {
        return (
            Vec::new(),
            Coverage::Failed {
                reason: first_line(&out.text),
            },
        );
    }
    let entries: Vec<&str> = out
        .text
        .lines()
        .filter(|l| l.starts_with(list.entry))
        .collect();
    let errors = entries.iter().filter_map(|l| pick(l)).collect();
    if entries.is_empty() && out.text.lines().any(|l| l.trim() == list.empty) {
        return (errors, Coverage::Complete);
    }
    let coverage = match out.text.lines().find_map(showing) {
        None => Coverage::Failed {
            reason: format!("unrecognized output: {}", first_line(&out.text)),
        },
        Some((from, to, total)) if from != 1 || to != total => Coverage::Partial {
            reason: format!("tool returned entries {from}-{to} of {total}"),
        },
        Some((_, to, _)) if entries.len() != to => Coverage::Partial {
            reason: format!("tool announced {to} entries, {} parsed", entries.len()),
        },
        Some(_) => Coverage::Complete,
    };
    (errors, coverage)
}

/// `Showing 1-3 of 3 (Page 1 of 1).` -> `(1, 3, 3)`
fn showing(line: &str) -> Option<(usize, usize, usize)> {
    let rest = line.strip_prefix("Showing ")?;
    let (range, rest) = rest.split_once(" of ")?;
    let (from, to) = range.split_once('-')?;
    let total = rest.split_whitespace().next()?.trim_end_matches('.');
    Some((from.parse().ok()?, to.parse().ok()?, total.parse().ok()?))
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

/// `msgid=3 [error] x` -> `[error] x`
fn strip_id(line: &str) -> &str {
    line.split_once(' ').map_or(line, |(_, rest)| rest)
}

/// `... (0 args)` -> `...`
fn strip_arg_count(line: &str) -> &str {
    match line.rsplit_once(" (") {
        Some((head, tail)) if tail.ends_with(" args)") || tail.ends_with(" arg)") => head,
        _ => line,
    }
}

/// `reqid=2 GET http://x/favicon.ico [404]` -> true for 4xx/5xx and network failures.
fn http_failed(line: &str) -> bool {
    let Some(status) = line.rsplit_once('[').map(|(_, s)| s.trim_end_matches(']')) else {
        return false;
    };
    match status.parse::<u16>() {
        Ok(code) => code >= 400,
        Err(_) => status.contains("fail") || status.contains("ERR"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> ToolOutput {
        ToolOutput {
            text: text.into(),
            ..Default::default()
        }
    }

    const NO_REQUESTS: &str = "## Network requests\nNo requests found.";
    const NO_MESSAGES: &str = "## Console messages\n<no console messages found>";

    /// The console half of `mcp_diagnostics`, with an empty network list.
    fn console(out: &ToolOutput) -> (Vec<String>, Coverage) {
        let d = mcp_diagnostics(Ok(out.clone()), Ok(ok(NO_REQUESTS)));
        (d.console_errors, d.console)
    }

    /// The network half of `mcp_diagnostics`, with an empty console list.
    fn network(out: &ToolOutput) -> (Vec<String>, Coverage) {
        let d = mcp_diagnostics(Ok(ok(NO_MESSAGES)), Ok(out.clone()));
        (d.failed_requests, d.network)
    }

    #[test]
    fn normalizes_diagnostic_lines() {
        let l = "msgid=4 [error] analytics: tracker not configured (1 args)";
        assert_eq!(
            strip_arg_count(strip_id(l)),
            "[error] analytics: tracker not configured"
        );
        assert_eq!(
            strip_id("reqid=2 GET http://x/a [404]"),
            "GET http://x/a [404]"
        );
    }

    #[test]
    fn classifies_requests() {
        assert!(http_failed("reqid=2 GET http://x/favicon.ico [404]"));
        assert!(!http_failed("reqid=1 GET http://x/ [200]"));
        assert!(http_failed("reqid=3 GET http://x/a [net::ERR_FAILED]"));
    }

    // Outputs below are verbatim chrome-devtools-mcp text (fixtures/shop, 2026-10-12).

    #[test]
    fn full_lists_are_complete() {
        let (errors, c) = console(&ok(
            "## Console messages\nShowing 1-3 of 3 (Page 1 of 1).\nmsgid=1 [issue] An element doesn’t have an autocomplete attribute (count: 1)\nmsgid=2 [verbose] [DOM] Input elements should have autocomplete attributes (suggested: \"current-password\"): (More info: https://goo.gl/9p2vKq) %o (0 args)\nmsgid=3 [error] Failed to load resource: the server responded with a status of 404 (File not found) (0 args)",
        ));
        assert_eq!(
            errors,
            vec![
                "[error] Failed to load resource: the server responded with a status of 404 (File not found)"
            ]
        );
        assert_eq!(c, Coverage::Complete);
        let (failed, c) = network(&ok(
            "## Network requests\nShowing 1-3 of 3 (Page 1 of 1).\nreqid=1 GET http://127.0.0.1:8765/v4-console.html [200]\nreqid=2 GET http://127.0.0.1:8765/api/me [200]\nreqid=3 GET http://127.0.0.1:8765/favicon.ico [404]",
        ));
        assert_eq!(failed, vec!["GET http://127.0.0.1:8765/favicon.ico [404]"]);
        assert_eq!(c, Coverage::Complete);
    }

    #[test]
    fn explicit_empty_lists_are_complete() {
        let (e, c) = console(&ok("## Console messages\n<no console messages found>"));
        assert!(e.is_empty());
        assert_eq!(c, Coverage::Complete);
        let (e, c) = network(&ok("## Network requests\nNo requests found."));
        assert!(e.is_empty());
        assert_eq!(c, Coverage::Complete);
    }

    #[test]
    fn a_failed_read_is_not_an_empty_list() {
        let out = ToolOutput {
            text: "Input validation error: Invalid arguments for tool list_console_messages".into(),
            is_error: true,
            ..Default::default()
        };
        let (e, c) = console(&out);
        assert!(e.is_empty());
        assert!(matches!(c, Coverage::Failed { .. }));
        // Unknown wording (e.g. a new server version) is a failed read too.
        let (_, c) = network(&ok("## Network requests\nnothing to see"));
        assert!(matches!(c, Coverage::Failed { .. }));
        let (_, c) = console(&ok(""));
        assert!(matches!(c, Coverage::Failed { .. }));
    }

    #[test]
    fn a_failed_call_marks_only_its_own_channel() {
        // e.g. the MCP server died between the two calls.
        let d = mcp_diagnostics(
            Ok(ok(
                "## Console messages\nShowing 1-1 of 1 (Page 1 of 1).\nmsgid=1 [error] boom (0 args)",
            )),
            Err(anyhow::anyhow!(
                "MCP call `list_network_requests` failed: connection closed"
            )),
        );
        assert_eq!(d.console, Coverage::Complete);
        assert_eq!(d.console_errors, vec!["[error] boom"]);
        assert!(
            matches!(&d.network, Coverage::Failed { reason } if reason.contains("connection closed")),
            "{:?}",
            d.network
        );
        assert!(d.failed_requests.is_empty());
        assert_eq!(d.gaps().len(), 1);

        // A disconnected browser: both channels failed, both say so.
        let gone = || Err(anyhow::anyhow!("transport closed"));
        let d = mcp_diagnostics(gone(), gone());
        assert!(matches!(d.console, Coverage::Failed { .. }));
        assert!(matches!(d.network, Coverage::Failed { .. }));
        assert_eq!(d.gaps().len(), 2);
    }

    #[test]
    fn a_restarted_browser_vouches_for_nothing() {
        // What the fresh browser returns: clean, complete-looking lists.
        let d = mcp_diagnostics(Ok(ok(NO_MESSAGES)), Ok(ok(NO_REQUESTS)));
        assert!(d.gaps().is_empty());
        let d = lost_to_restart(d);
        assert!(matches!(d.console, Coverage::Failed { .. }));
        assert!(matches!(d.network, Coverage::Failed { .. }));
        assert_eq!(d.gaps().len(), 2);
    }

    #[test]
    fn a_paged_or_short_list_is_partial() {
        let (e, c) = console(&ok(
            "## Console messages\nShowing 1-1 of 3 (Page 1 of 3).\nNext page: 1\nmsgid=1 [error] boom (0 args)",
        ));
        assert_eq!(e, vec!["[error] boom"]);
        assert!(matches!(c, Coverage::Partial { .. }));
        let (_, c) = network(&ok(
            "## Network requests\nShowing 1-2 of 2 (Page 1 of 1).\nreqid=1 GET http://x/ [200]",
        ));
        assert!(matches!(c, Coverage::Partial { .. }));
    }

    #[test]
    fn gaps_name_every_unread_channel() {
        let d = Diagnostics {
            console: Coverage::Complete,
            network: Coverage::Unsupported,
            ..Default::default()
        };
        assert_eq!(d.gaps(), vec!["network: not supported by this backend"]);
        assert_eq!(Diagnostics::default().gaps().len(), 2);
        let d = Diagnostics {
            console: Coverage::Complete,
            network: Coverage::Complete,
            ..Default::default()
        };
        assert!(d.gaps().is_empty());
    }

    #[test]
    fn traces_without_coverage_read_as_not_recorded() {
        let d: Diagnostics =
            serde_json::from_str(r#"{"console_errors":[],"failed_requests":[]}"#).unwrap();
        assert_eq!(d.console, Coverage::NotRecorded);
        assert_eq!(d.network, Coverage::NotRecorded);
    }
}
