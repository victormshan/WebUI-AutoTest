//! LLM-driven browser agent: observe (a11y snapshot) -> decide (LLM) -> act (MCP tool).
//!
//! Every executed step is recorded with a uid-free [`Locator`] so the trace can
//! later be replayed deterministically without the LLM.

pub mod flow;
pub mod replay;
pub mod snapshot;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use llm::{Llm, extract_json};
use mcp_client::{Browser, ToolInfo};
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use snapshot::Locator;

/// Tools the agent may use to act on the page. Observation tools are called
/// by the harness itself, so the model never needs `take_snapshot`.
pub const ACTION_TOOLS: &[&str] = &[
    "click",
    "hover",
    "fill",
    "fill_form",
    "type_text",
    "press_key",
    "navigate_page",
    "wait_for",
    "handle_dialog",
];

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_steps: usize,
    /// Case-insensitive keywords; actions on elements whose snapshot line
    /// contains one are refused (e.g. "删除", "delete").
    pub deny: Vec<String>,
    pub snapshot_budget: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 25,
            deny: Vec::new(),
            snapshot_budget: 30_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub index: usize,
    pub thought: String,
    pub tool: String,
    pub args: Value,
    /// Semantic targets of the uids in `args` (same order as they appear).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<Target>,
    pub ok: bool,
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub uid: String,
    pub locator: Locator,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    pub console_errors: Vec<String>,
    pub failed_requests: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trace {
    pub goal: String,
    pub start_url: String,
    pub model: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub success: bool,
    pub summary: String,
    pub evidence: String,
    pub steps: Vec<Step>,
    pub diagnostics: Diagnostics,
    /// Raw snapshot the model judged the outcome on (basis for assertions).
    #[serde(default)]
    pub final_snapshot: String,
}

pub struct Agent<'a> {
    browser: &'a Browser,
    llm: &'a dyn Llm,
    cfg: AgentConfig,
}

/// What the model asked for this turn.
enum Decision {
    Act {
        thought: String,
        tool: String,
        args: Value,
    },
    Done {
        thought: String,
        success: bool,
        summary: String,
        evidence: String,
    },
}

impl<'a> Agent<'a> {
    pub fn new(browser: &'a Browser, llm: &'a dyn Llm, cfg: AgentConfig) -> Self {
        Self { browser, llm, cfg }
    }

    pub async fn run(&self, goal: &str, start_url: &str) -> Result<Trace> {
        let mut trace = Trace {
            goal: goal.to_string(),
            start_url: start_url.to_string(),
            model: self.llm.describe(),
            started_at: Utc::now(),
            finished_at: None,
            success: false,
            summary: String::new(),
            evidence: String::new(),
            steps: Vec::new(),
            diagnostics: Diagnostics::default(),
            final_snapshot: String::new(),
        };

        let nav = self.browser.navigate(start_url).await?;
        if nav.is_error {
            bail!("could not open {start_url}: {}", nav.text);
        }

        let tools: Vec<ToolInfo> = self
            .browser
            .tools()
            .await?
            .into_iter()
            .filter(|t| ACTION_TOOLS.contains(&t.name.as_str()))
            .collect();
        let system = system_prompt(&tools);
        let mut parse_failures = 0;

        loop {
            if trace.steps.len() >= self.cfg.max_steps {
                trace.summary = format!("gave up after {} steps", self.cfg.max_steps);
                break;
            }
            let raw_snapshot = self.browser.snapshot().await?;
            let prompt = user_prompt(
                goal,
                &trace.steps,
                &snapshot::compact(&raw_snapshot, self.cfg.snapshot_budget),
            );
            let reply = self.llm.complete(&system, &prompt).await?;

            let decision = match parse_decision(&reply) {
                Ok(d) => d,
                Err(e) => {
                    parse_failures += 1;
                    tracing::warn!("unusable model reply ({parse_failures}/3): {e:#}");
                    if parse_failures >= 3 {
                        bail!("model repeatedly returned unusable replies: {e:#}");
                    }
                    continue;
                }
            };
            parse_failures = 0;

            match decision {
                Decision::Done {
                    thought,
                    success,
                    summary,
                    evidence,
                } => {
                    tracing::info!(success, "done: {summary}");
                    tracing::debug!("final thought: {thought}");
                    trace.success = success;
                    trace.summary = summary;
                    trace.evidence = evidence;
                    trace.final_snapshot = raw_snapshot;
                    break;
                }
                Decision::Act {
                    thought,
                    tool,
                    args,
                } => {
                    let step = self
                        .act(trace.steps.len() + 1, thought, tool, args, &raw_snapshot)
                        .await?;
                    trace.steps.push(step);
                }
            }
        }

        trace.diagnostics = collect_diagnostics(self.browser).await?;
        trace.finished_at = Some(Utc::now());
        Ok(trace)
    }

    async fn act(
        &self,
        index: usize,
        thought: String,
        tool: String,
        args: Value,
        raw_snapshot: &str,
    ) -> Result<Step> {
        // The harness snapshots every turn; inline snapshots only bloat results.
        let mut args = args;
        if let Some(m) = args.as_object_mut() {
            m.remove("includeSnapshot");
        }
        let uids = uids_in(&args);
        let targets: Vec<Target> = uids
            .iter()
            .filter_map(|uid| {
                snapshot::locator_for(raw_snapshot, uid).map(|locator| Target {
                    uid: uid.clone(),
                    locator,
                })
            })
            .collect();
        let target_desc = targets
            .iter()
            .map(|t| t.locator.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        tracing::info!("step {index}: {tool} {args} [{target_desc}] — {thought}");

        let (ok, result) = if !ACTION_TOOLS.contains(&tool.as_str()) {
            (
                false,
                format!(
                    "Tool `{tool}` is not available. Use one of: {}",
                    ACTION_TOOLS.join(", ")
                ),
            )
        } else if let Some(hit) = self.denied(raw_snapshot, &uids) {
            (
                false,
                format!(
                    "Blocked by safety policy: element {hit} matches a denied keyword. Choose another way or finish."
                ),
            )
        } else {
            let out = self.browser.call(&tool, args.clone()).await?;
            (!out.is_error, out.text)
        };
        if !ok {
            tracing::warn!("step {index} failed: {result}");
        }
        Ok(Step {
            index,
            thought,
            tool,
            args,
            targets,
            ok,
            result,
        })
    }

    fn denied(&self, raw_snapshot: &str, uids: &[String]) -> Option<String> {
        uids.iter().find_map(|uid| {
            let line = snapshot::describe(raw_snapshot, uid)?;
            let lower = line.to_lowercase();
            self.cfg
                .deny
                .iter()
                .any(|k| lower.contains(&k.to_lowercase()))
                .then_some(line)
        })
    }
}

/// Built-in checks that need no assertions: console errors and HTTP failures.
/// Entries are normalized (ids and arg counts stripped) so runs can be compared.
pub async fn collect_diagnostics(browser: &Browser) -> Result<Diagnostics> {
    let console = browser
        .call("list_console_messages", serde_json::json!({}))
        .await?;
    let network = browser
        .call("list_network_requests", serde_json::json!({}))
        .await?;
    Ok(Diagnostics {
        console_errors: console
            .text
            .lines()
            .filter(|l| l.starts_with("msgid=") && l.contains(" [error] "))
            .map(|l| strip_arg_count(strip_id(l)).to_string())
            .collect(),
        failed_requests: network
            .text
            .lines()
            .filter(|l| l.starts_with("reqid=") && http_failed(l))
            .map(|l| strip_id(l).to_string())
            .collect(),
    })
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

/// All element uids referenced by tool arguments (`uid`, `from_uid`, `fill_form.elements[].uid`, ...).
pub(crate) fn uids_in(args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, v) in m {
                    if k == "uid" || k.ends_with("_uid") || k.ends_with("Uid") {
                        if let Some(s) = v.as_str() {
                            out.push(s.to_string());
                        }
                    } else {
                        walk(v, out);
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(args, &mut out);
    out
}

fn parse_decision(reply: &str) -> Result<Decision> {
    let v = extract_json(reply)?;
    let thought = v["thought"].as_str().unwrap_or_default().to_string();
    if let Some(done) = v.get("done").filter(|d| d.is_object()) {
        return Ok(Decision::Done {
            thought,
            success: done["success"].as_bool().unwrap_or(false),
            summary: done["summary"].as_str().unwrap_or_default().to_string(),
            evidence: done["evidence"].as_str().unwrap_or_default().to_string(),
        });
    }
    let action = &v["action"];
    let Some(tool) = action["tool"].as_str() else {
        bail!("reply has neither `action.tool` nor `done`: {v}");
    };
    let args = match &action["args"] {
        Value::Null => Value::Object(Default::default()),
        a => a.clone(),
    };
    Ok(Decision::Act {
        thought,
        tool: tool.to_string(),
        args,
    })
}

fn system_prompt(tools: &[ToolInfo]) -> String {
    let mut s = String::from(
        r#"You are a web QA agent operating a real Chrome browser to accomplish a user's GOAL.

Each turn you get the GOAL, the HISTORY of actions already taken (with results), and the CURRENT PAGE as an accessibility-tree snapshot. Every element line starts with `uid=<id>`.

Reply with ONLY one JSON object, no prose, in one of these two shapes:
{"thought": "<short reasoning>", "action": {"tool": "<tool name>", "args": { ... }}}
{"thought": "<short reasoning>", "done": {"success": true|false, "summary": "<what happened>", "evidence": "<text from the CURRENT PAGE proving the outcome>"}}

Rules:
- Exactly one action per turn. Use only uids that appear in the CURRENT PAGE snapshot; uids from earlier turns are stale.
- Prefer `fill_form` to fill several fields at once. Do not request snapshots; you get a fresh one every turn.
- Do not click disabled elements; satisfy the precondition first.
- If an action failed, read its error and try a different approach instead of repeating it.
- Declare success only when the CURRENT PAGE visibly confirms the goal. If the goal is impossible (e.g. a validation error you cannot resolve with the information given), finish with success=false and explain.
- Never invent credentials or personal data beyond what the GOAL provides; for other required fields use plausible test data.

Available tools (JSON Schema of args):
"#,
    );
    for t in tools {
        s.push_str(&format!(
            "\n### {}\n{}\nargs schema: {}\n",
            t.name,
            t.description.trim(),
            serde_json::to_string(&t.input_schema).unwrap_or_default()
        ));
    }
    s
}

fn user_prompt(goal: &str, steps: &[Step], compact_snapshot: &str) -> String {
    let mut s = format!("GOAL:\n{goal}\n\nHISTORY:\n");
    if steps.is_empty() {
        s.push_str("(none yet)\n");
    }
    for st in steps {
        let targets = st
            .targets
            .iter()
            .map(|t| t.locator.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let result: String = st.result.chars().take(400).collect();
        s.push_str(&format!(
            "{}. {} {} [{}] -> {}: {}\n   thought: {}\n",
            st.index,
            st.tool,
            st.args,
            targets,
            if st.ok { "ok" } else { "FAILED" },
            result.replace('\n', " "),
            st.thought
        ));
    }
    s.push_str(&format!(
        "\nCURRENT PAGE:\n{compact_snapshot}\nYour JSON reply:"
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nested_uids() {
        let args = serde_json::json!({"elements": [{"uid": "1_5", "value": "a"}, {"uid": "1_8", "value": "b"}]});
        assert_eq!(uids_in(&args), vec!["1_5", "1_8"]);
        assert_eq!(
            uids_in(&serde_json::json!({"from_uid": "1", "to_uid": "2"})).len(),
            2
        );
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

    #[test]
    fn parses_both_reply_shapes() {
        let act =
            parse_decision(r#"{"thought":"t","action":{"tool":"click","args":{"uid":"1_2"}}}"#)
                .unwrap();
        assert!(matches!(act, Decision::Act { ref tool, .. } if tool == "click"));
        let done = parse_decision(
            "```json\n{\"thought\":\"t\",\"done\":{\"success\":true,\"summary\":\"s\"}}\n```",
        )
        .unwrap();
        assert!(matches!(done, Decision::Done { success: true, .. }));
    }
}
