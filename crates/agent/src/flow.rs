//! Flows: deterministic, uid-free test cases distilled from agent traces.
//!
//! ```yaml
//! steps:
//!   - intent: 点击登录
//!     tool: click
//!     args: { uid: $0 }
//!     targets: [{ role: button, name: 登录 }]
//! assertions:
//!   - element: { role: heading, name: 订单已提交 }
//!   - text: 合计 ¥528
//! ```
//!
//! Element uids in `args` are replaced by `$<n>` placeholders that refer to
//! `targets[n]`; replay resolves them against a fresh snapshot.

use std::path::Path;

use anyhow::{Context, Result, bail};
use llm::{Llm, extract_json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Locator, Trace, snapshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flow {
    pub name: String,
    pub goal: String,
    pub start_url: String,
    /// Saved login state (see `webtest login`) loaded before the flow starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_state: Option<String>,
    pub steps: Vec<FlowStep>,
    /// `- element: {...}` / `- text: ...` rather than YAML `!tags`.
    #[serde(default, with = "serde_yaml_ng::with::singleton_map_recursive")]
    pub assertions: Vec<Assertion>,
    /// Console errors already present when the flow was recorded (normalized).
    #[serde(default)]
    pub allowed_console_errors: Vec<String>,
    /// Failed requests already present when the flow was recorded; same-origin
    /// URLs are stored as paths so the flow still matches under `--url`.
    #[serde(default)]
    pub allowed_failed_requests: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStep {
    /// Why this step exists; used for reports and for self-healing.
    pub intent: String,
    pub tool: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<Locator>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Assertion {
    /// An element with exactly this role and name is present.
    Element { role: String, name: String },
    /// Some element's text contains this string.
    Text(String),
    /// No element's text contains this string.
    AbsentText(String),
}

impl Assertion {
    pub fn check(&self, raw_snapshot: &str) -> bool {
        match self {
            Assertion::Element { role, name } => snapshot::has_element(raw_snapshot, role, name),
            Assertion::Text(t) => snapshot::contains_text(raw_snapshot, t),
            Assertion::AbsentText(t) => !snapshot::contains_text(raw_snapshot, t),
        }
    }
}

impl std::fmt::Display for Assertion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Assertion::Element { role, name } => write!(f, "element {role} {name:?}"),
            Assertion::Text(t) => write!(f, "text {t:?}"),
            Assertion::AbsentText(t) => write!(f, "absent text {t:?}"),
        }
    }
}

pub fn placeholder(i: usize) -> String {
    format!("${i}")
}

/// Replaces `uid` values in `args` according to `map(old) -> new`.
pub(crate) fn map_uids(args: &Value, map: &dyn Fn(&str) -> Option<String>) -> Value {
    match args {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let is_uid = k == "uid" || k.ends_with("_uid") || k.ends_with("Uid");
                    let v = match v.as_str().and_then(map) {
                        Some(new) if is_uid => Value::String(new),
                        _ => map_uids(v, map),
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|v| map_uids(v, map)).collect()),
        other => other.clone(),
    }
}

impl Flow {
    /// Distills a successful trace: failed/blocked steps are dropped and uids
    /// become placeholders. Assertions are added separately.
    pub fn from_trace(trace: &Trace, name: &str) -> Result<Self> {
        if !trace.success {
            bail!("trace did not succeed; refusing to turn it into a regression flow");
        }
        let mut steps = Vec::new();
        for st in trace.steps.iter().filter(|s| s.ok) {
            let uids = crate::uids_in(&st.args);
            let mut targets = Vec::new();
            for uid in &uids {
                let Some(t) = st.targets.iter().find(|t| &t.uid == uid) else {
                    bail!("step {} uses uid {uid} with no recorded locator", st.index);
                };
                targets.push(t.locator.clone());
            }
            let args = map_uids(&st.args, &|u| {
                uids.iter().position(|x| x == u).map(placeholder)
            });
            steps.push(FlowStep {
                intent: st.thought.clone(),
                tool: st.tool.clone(),
                args,
                targets,
            });
        }
        Ok(Flow {
            name: name.to_string(),
            goal: trace.goal.clone(),
            start_url: trace.start_url.clone(),
            storage_state: None,
            steps,
            assertions: Vec::new(),
            allowed_console_errors: dedup(&trace.diagnostics.console_errors),
            allowed_failed_requests: dedup(
                &trace
                    .diagnostics
                    .failed_requests
                    .iter()
                    .map(|r| relativize(r, &trace.start_url))
                    .collect::<Vec<_>>(),
            ),
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_yaml_ng::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_yaml_ng::to_string(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// `scheme://host[:port]` of a URL.
fn origin(url: &str) -> Option<&str> {
    let after_scheme = url.find("://")? + 3;
    let end = url[after_scheme..]
        .find('/')
        .map_or(url.len(), |i| after_scheme + i);
    Some(&url[..end])
}

/// Strips `base_url`'s origin from URLs in `line`.
pub(crate) fn relativize(line: &str, base_url: &str) -> String {
    match origin(base_url) {
        Some(o) => line.replace(&format!("{o}/"), "/"),
        None => line.to_string(),
    }
}

fn dedup(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for i in items {
        if !out.contains(i) {
            out.push(i.clone());
        }
    }
    out
}

/// Proposes assertions for the trace's final page. The LLM picks stable,
/// goal-relevant checks; every candidate must pass on the recorded final
/// snapshot or it is dropped. Falls back to page landmarks.
pub async fn suggest_assertions(trace: &Trace, llm: Option<&dyn Llm>) -> Vec<Assertion> {
    let snap = &trace.final_snapshot;
    let mut out: Vec<Assertion> = Vec::new();

    if let Some(llm) = llm {
        match ask_llm_for_assertions(trace, llm).await {
            Ok(candidates) => {
                for a in candidates {
                    if !a.check(snap) {
                        tracing::warn!("dropping assertion that fails on the recorded page: {a}");
                    } else if !out.contains(&a) {
                        out.push(a);
                    }
                }
            }
            Err(e) => tracing::warn!("LLM assertion generation failed, using landmarks: {e:#}"),
        }
    }
    if out.is_empty() {
        out = snapshot::landmarks(snap)
            .into_iter()
            .map(|(role, name)| Assertion::Element { role, name })
            .collect();
    }
    out
}

async fn ask_llm_for_assertions(trace: &Trace, llm: &dyn Llm) -> Result<Vec<Assertion>> {
    let system = r#"You write regression assertions for an automated UI test.
Given the test GOAL, the agent's verdict and the FINAL PAGE accessibility snapshot, choose 2-5 checks that prove the goal was achieved.
Reply with ONLY JSON: {"assertions": [ ... ]} where each item is one of
  {"element": {"role": "<role>", "name": "<exact accessible name>"}}
  {"text": "<substring of some element's text>"}
  {"absent_text": "<text that must NOT appear, e.g. an error message>"}
Rules: copy role/name/text exactly from the snapshot; avoid volatile values (order ids, timestamps, random numbers); prefer business outcomes (confirmation headings, totals, counts) over generic chrome like the site title.
Never assert the exact wording of generated or personalized content (AI/chat answers, search results, feeds, recommendations): it changes between runs. Assert its presence through stable structure instead (e.g. the heading or label that wraps the answer)."#;
    let user = format!(
        "GOAL:\n{}\n\nVERDICT: {}\nEVIDENCE: {}\n\nFINAL PAGE:\n{}\nYour JSON reply:",
        trace.goal,
        trace.summary,
        trace.evidence,
        snapshot::compact(&trace.final_snapshot, 30_000)
    );
    let v = extract_json(&llm.complete(system, &user).await?)?;
    let items = v["assertions"]
        .as_array()
        .context("missing `assertions` array")?;
    Ok(items
        .iter()
        .filter_map(|i| serde_json::from_value(i.clone()).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Diagnostics, Step, Target};

    fn trace() -> Trace {
        let loc = |role: &str, name: &str| Locator {
            role: role.into(),
            name: name.into(),
            nth: 0,
        };
        Trace {
            goal: "g".into(),
            start_url: "http://x/".into(),
            model: "m".into(),
            started_at: chrono::Utc::now(),
            finished_at: None,
            success: true,
            summary: String::new(),
            evidence: String::new(),
            steps: vec![
                Step {
                    index: 1,
                    thought: "fill".into(),
                    tool: "fill_form".into(),
                    args: serde_json::json!({"elements": [{"uid": "1_5", "value": "alice"}, {"uid": "1_8", "value": "pw"}]}),
                    targets: vec![
                        Target {
                            uid: "1_5".into(),
                            locator: loc("textbox", "用户名"),
                        },
                        Target {
                            uid: "1_8".into(),
                            locator: loc("textbox", "密码"),
                        },
                    ],
                    ok: true,
                    result: String::new(),
                },
                Step {
                    index: 2,
                    thought: "blocked".into(),
                    tool: "click".into(),
                    args: serde_json::json!({"uid": "1_9"}),
                    targets: vec![],
                    ok: false,
                    result: String::new(),
                },
            ],
            diagnostics: Diagnostics::default(),
            final_snapshot: "uid=1_0 RootWebArea \"x\"\n  uid=1_1 heading \"Done\" level=\"2\"\n"
                .into(),
        }
    }

    #[test]
    fn distills_trace_into_placeholders() {
        let flow = Flow::from_trace(&trace(), "t").unwrap();
        assert_eq!(flow.steps.len(), 1, "failed steps are dropped");
        let s = &flow.steps[0];
        assert_eq!(s.args["elements"][0]["uid"], "$0");
        assert_eq!(s.args["elements"][1]["uid"], "$1");
        assert_eq!(s.args["elements"][1]["value"], "pw");
        assert_eq!(s.targets[1].name, "密码");
    }

    #[test]
    fn relativizes_same_origin_urls() {
        let base = "http://127.0.0.1:8765/v2.html";
        assert_eq!(
            relativize("GET http://127.0.0.1:8765/api/x [404]", base),
            "GET /api/x [404]"
        );
        assert_eq!(
            relativize("GET https://cdn.example/a.js [404]", base),
            "GET https://cdn.example/a.js [404]"
        );
    }

    #[test]
    fn yaml_roundtrip() {
        let mut flow = Flow::from_trace(&trace(), "t").unwrap();
        flow.assertions = vec![
            Assertion::Element {
                role: "heading".into(),
                name: "Done".into(),
            },
            Assertion::Text("Do".into()),
        ];
        let y = serde_yaml_ng::to_string(&flow).unwrap();
        assert!(y.contains("- element:") && y.contains("- text: Do"), "{y}");
        let back: Flow = serde_yaml_ng::from_str(&y).unwrap();
        assert_eq!(back.assertions, flow.assertions);
    }

    #[tokio::test]
    async fn landmark_fallback() {
        let a = suggest_assertions(&trace(), None).await;
        assert_eq!(
            a,
            vec![Assertion::Element {
                role: "heading".into(),
                name: "Done".into()
            }]
        );
    }
}
