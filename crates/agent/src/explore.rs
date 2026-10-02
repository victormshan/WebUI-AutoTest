//! Autonomous exploration: discover user tasks, record them as verified flows,
//! and map the page states they lead to.
//!
//! ```text
//! frontier = [start page]
//! for each state (breadth-first, depth <= max_depth):
//!     replay the state's prefix in a fresh browser  -> snapshot
//!     LLM proposes new tasks that start from this page
//!     for each task (concurrently, fresh browser each):
//!         replay prefix -> agent pursues goal -> flow = prefix + task steps
//!         verify: replay the flow deterministically in another fresh browser
//!         save if verified; a new end state joins the frontier
//! ```

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures::StreamExt;
use llm::{Llm, extract_json};
use mcp_client::{Browser, BrowserConfig};
use serde::{Deserialize, Serialize};

use crate::flow::{Flow, suggest_assertions};
use crate::replay::{ReplayOptions, reach, replay};
use crate::{Agent, AgentConfig, snapshot};

#[derive(Debug, Clone)]
pub struct ExploreConfig {
    pub browser: BrowserConfig,
    pub agent: AgentConfig,
    pub replay: ReplayOptions,
    /// Path recorded in generated flows when `browser.storage_state` is set.
    pub storage_state_path: Option<String>,
    /// Free-form test data the LLM may use (accounts, addresses, ...).
    pub context: String,
    pub max_tasks: usize,
    pub tasks_per_state: usize,
    pub max_depth: usize,
    /// Tasks run concurrently, each in its own browser.
    pub jobs: usize,
    /// Consecutive clean LLM-free replays required before a flow is saved
    /// as verified; assertions that fail in between are dropped as flaky.
    pub verify_runs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateInfo {
    pub id: String,
    pub label: String,
    pub depth: usize,
    /// Task that first reached this state (None for the start page).
    pub reached_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub name: String,
    pub goal: String,
    pub kind: String,
    pub from_state: String,
    pub to_state: Option<String>,
    pub success: bool,
    /// The saved flow replayed cleanly without the LLM.
    pub verified: bool,
    /// The agent hit the deny list (not a finding about the site).
    #[serde(default)]
    pub blocked: bool,
    /// Assertions removed because they did not hold on every replay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flaky_assertions: Vec<String>,
    pub flow: Option<PathBuf>,
    pub summary: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExploreReport {
    pub start_url: String,
    pub states: Vec<StateInfo>,
    pub tasks: Vec<TaskOutcome>,
}

#[derive(Debug, Clone, Deserialize)]
struct Proposal {
    name: String,
    goal: String,
    #[serde(default = "positive")]
    kind: String,
}

fn positive() -> String {
    "positive".into()
}

/// A new state a task ended in, with the flow that reaches it.
struct Reached {
    id: String,
    label: String,
    flow: Flow,
}

/// A state waiting to be explored, with the flow that reaches it.
struct Pending {
    id: String,
    depth: usize,
    prefix: Flow,
}

pub struct Explorer<'a> {
    llm: &'a dyn Llm,
    cfg: ExploreConfig,
}

impl<'a> Explorer<'a> {
    pub fn new(llm: &'a dyn Llm, cfg: ExploreConfig) -> Self {
        Self { llm, cfg }
    }

    /// Explores from `start_url`, writing verified flows to `out_dir`.
    pub async fn run(&self, start_url: &str, out_dir: &Path) -> Result<ExploreReport> {
        std::fs::create_dir_all(out_dir)?;
        let mut report = ExploreReport {
            start_url: start_url.to_string(),
            ..Default::default()
        };
        let root = Flow {
            name: "start".into(),
            goal: String::new(),
            start_url: start_url.to_string(),
            storage_state: self.cfg.storage_state_path.clone(),
            steps: Vec::new(),
            assertions: Vec::new(),
            allowed_console_errors: Vec::new(),
            allowed_failed_requests: Vec::new(),
        };

        let mut known_goals: Vec<String> = Vec::new();
        let mut used_names: HashMap<String, usize> = HashMap::new();
        let mut frontier = VecDeque::from([Pending {
            id: String::new(),
            depth: 0,
            prefix: root,
        }]);

        while let Some(state) = frontier.pop_front() {
            let budget = self.cfg.max_tasks.saturating_sub(report.tasks.len());
            if budget == 0 {
                tracing::info!("task budget exhausted");
                break;
            }

            // Observe the state in a fresh browser.
            let snap = match self.observe(&state.prefix).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("could not reach state {}: {e:#}", state.id);
                    continue;
                }
            };
            let id = snapshot::fingerprint(&snap);
            if state.id.is_empty() {
                report.states.push(StateInfo {
                    id: id.clone(),
                    label: snapshot::label(&snap),
                    depth: 0,
                    reached_by: None,
                });
            }
            tracing::info!(
                "exploring state {id} [{}] at depth {}",
                snapshot::label(&snap),
                state.depth
            );

            let proposals = match self
                .propose(&snap, &known_goals, budget.min(self.cfg.tasks_per_state))
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("task proposal failed for state {id}: {e:#}");
                    continue;
                }
            };
            for p in &proposals {
                tracing::info!("  proposed [{}] {}: {}", p.kind, p.name, p.goal);
                known_goals.push(p.goal.clone());
            }

            // Unique, filesystem-safe names.
            let named: Vec<(String, Proposal)> = proposals
                .into_iter()
                .map(|p| {
                    let base = slug(&p.name);
                    let n = used_names.entry(base.clone()).or_insert(0);
                    *n += 1;
                    let name = if *n == 1 { base } else { format!("{base}_{n}") };
                    (name, p)
                })
                .collect();

            let outcomes: Vec<(TaskOutcome, Option<Reached>)> = futures::stream::iter(named)
                .map(|(name, p)| self.attempt(name, p, &state.prefix, &id, out_dir))
                .buffer_unordered(self.cfg.jobs.max(1))
                .collect()
                .await;

            for (outcome, reached) in outcomes {
                // Expand only states reached by verified, positive tasks.
                if let Some(Reached {
                    id: to_id,
                    label: to_label,
                    flow,
                }) = reached
                {
                    let new = to_id != id && !report.states.iter().any(|s| s.id == to_id);
                    if new {
                        report.states.push(StateInfo {
                            id: to_id.clone(),
                            label: to_label,
                            depth: state.depth + 1,
                            reached_by: Some(outcome.name.clone()),
                        });
                        if state.depth + 1 < self.cfg.max_depth {
                            frontier.push_back(Pending {
                                id: to_id,
                                depth: state.depth + 1,
                                prefix: flow,
                            });
                        }
                    }
                }
                report.tasks.push(outcome);
            }
        }

        std::fs::write(
            out_dir.join("explore.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        std::fs::write(out_dir.join("explore.md"), markdown(&report))?;
        Ok(report)
    }

    async fn observe(&self, prefix: &Flow) -> Result<String> {
        let browser = Browser::launch(&self.cfg.browser).await?;
        let res = async {
            if let Some(err) = reach(&browser, prefix, &self.cfg.replay).await? {
                anyhow::bail!("{err}");
            }
            browser.snapshot().await
        }
        .await;
        browser.close().await?;
        res
    }

    async fn propose(&self, snap: &str, known: &[String], limit: usize) -> Result<Vec<Proposal>> {
        let deny = if self.cfg.agent.deny.is_empty() {
            "(none)".to_string()
        } else {
            self.cfg.agent.deny.join(", ")
        };
        let system = format!(
            r#"You are a QA lead exploring a web application to build a regression suite.
Given the CURRENT PAGE (accessibility snapshot), the test CONTEXT, and tasks ALREADY KNOWN, propose up to {limit} NEW end-to-end user tasks that START from this page.

Each task must be something an automated agent can finish and verify on screen. Write the goal as a concrete instruction including the expected visible outcome, in the same language as the page, and include any test data it needs (from CONTEXT, or plausible made-up data for non-secret fields).
- Cover the main business paths first; add at most one negative/validation task per form (kind "negative").
- Never propose destructive or irreversible actions (deleting data, real payments, sending messages to real people, changing passwords).
- Skip tasks that need information not in CONTEXT (e.g. unknown credentials) and tasks whose intent duplicates one ALREADY KNOWN.
- Elements whose label contains any of these keywords are off-limits to the agent: {deny}. Do not propose tasks that need them.
- Return fewer tasks (or none) rather than padding.

Reply with ONLY JSON: {{"tasks": [{{"name": "<short_snake_case_ascii>", "goal": "<instruction>", "kind": "positive"|"negative"}}]}}"#
        );
        let known_list = if known.is_empty() {
            "(none)".to_string()
        } else {
            known
                .iter()
                .map(|g| format!("- {g}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let user = format!(
            "CONTEXT:\n{}\n\nALREADY KNOWN:\n{known_list}\n\nCURRENT PAGE:\n{}\nYour JSON reply:",
            if self.cfg.context.is_empty() {
                "(none)"
            } else {
                &self.cfg.context
            },
            snapshot::compact(snap, 30_000)
        );
        let v = extract_json(&self.llm.complete(&system, &user).await?)?;
        let tasks: Vec<Proposal> = serde_json::from_value(v["tasks"].clone())
            .context("reply has no valid `tasks` array")?;
        Ok(tasks.into_iter().take(limit).collect())
    }

    /// Runs one task from `prefix`'s end state. On success saves the combined
    /// flow; returns the reached state (id, label, flow) when it is worth expanding.
    async fn attempt(
        &self,
        name: String,
        p: Proposal,
        prefix: &Flow,
        from_state: &str,
        out_dir: &Path,
    ) -> (TaskOutcome, Option<Reached>) {
        let mut outcome = TaskOutcome {
            name: name.clone(),
            goal: p.goal.clone(),
            kind: p.kind.clone(),
            from_state: from_state.to_string(),
            to_state: None,
            success: false,
            verified: false,
            blocked: false,
            flaky_assertions: Vec::new(),
            flow: None,
            summary: String::new(),
        };
        match self
            .attempt_inner(&name, &p, prefix, out_dir, &mut outcome)
            .await
        {
            Ok(reached) => (outcome, reached),
            Err(e) => {
                tracing::warn!("task {name} errored: {e:#}");
                outcome.summary = format!("error: {e:#}");
                (outcome, None)
            }
        }
    }

    async fn attempt_inner(
        &self,
        name: &str,
        p: &Proposal,
        prefix: &Flow,
        out_dir: &Path,
        outcome: &mut TaskOutcome,
    ) -> Result<Option<Reached>> {
        tracing::info!("task {name}: start");
        let browser = Browser::launch(&self.cfg.browser).await?;
        let trace = async {
            if let Some(err) = reach(&browser, prefix, &self.cfg.replay).await? {
                anyhow::bail!("prefix replay failed: {err}");
            }
            Agent::new(&browser, self.llm, self.cfg.agent.clone())
                .run_here(&p.goal, &prefix.start_url)
                .await
        }
        .await;
        browser.close().await?;
        let trace = trace?;

        outcome.success = trace.success;
        outcome.summary = trace.summary.clone();
        outcome.blocked = trace
            .steps
            .iter()
            .any(|s| s.result.starts_with("Blocked by safety policy"));
        if !trace.success {
            tracing::warn!("task {name}: agent did not succeed: {}", trace.summary);
            return Ok(None);
        }
        let to_id = snapshot::fingerprint(&trace.final_snapshot);
        let to_label = snapshot::label(&trace.final_snapshot);
        outcome.to_state = Some(to_id.clone());

        let mut flow = Flow::from_trace(&trace, name)?;
        flow.goal = p.goal.clone();
        flow.storage_state = prefix.storage_state.clone();
        flow.steps.splice(0..0, prefix.steps.iter().cloned());
        for e in &prefix.allowed_console_errors {
            if !flow.allowed_console_errors.contains(e) {
                flow.allowed_console_errors.push(e.clone());
            }
        }
        for r in &prefix.allowed_failed_requests {
            if !flow.allowed_failed_requests.contains(r) {
                flow.allowed_failed_requests.push(r.clone());
            }
        }
        flow.assertions = suggest_assertions(&trace, Some(self.llm)).await;

        let verified = self.verify(name, &mut flow, outcome).await?;
        outcome.verified = verified;
        let path = if verified {
            out_dir.join(format!("{name}.yaml"))
        } else {
            out_dir.join("unverified").join(format!("{name}.yaml"))
        };
        flow.save(&path)?;
        outcome.flow = Some(path);
        tracing::info!("task {name}: done (verified={verified})");

        Ok((verified && p.kind != "negative").then_some(Reached {
            id: to_id,
            label: to_label,
            flow,
        }))
    }
}

impl Explorer<'_> {
    /// A flow is only useful if it replays without the LLM — repeatedly.
    /// Replays until `verify_runs` consecutive clean passes. Assertions that
    /// fail while every step succeeded are treated as flaky (e.g. exact AI
    /// wording) and dropped; any other failure makes the flow unverified.
    async fn verify(&self, name: &str, flow: &mut Flow, outcome: &mut TaskOutcome) -> Result<bool> {
        let need = self.cfg.verify_runs.max(1);
        let mut clean = 0;
        for _ in 0..need + 2 {
            let browser = Browser::launch(&self.cfg.browser).await?;
            let check = replay(&browser, flow, None, &self.cfg.replay).await;
            browser.close().await?;
            let check = check?;

            if check.passed {
                clean += 1;
                if clean >= need {
                    return Ok(true);
                }
                continue;
            }
            clean = 0;
            let only_assertions = check.failures.len() == check.failed_assertions.len();
            if !only_assertions {
                tracing::warn!(
                    "task {name}: flow failed verification: {:?}",
                    check.failures
                );
                return Ok(false);
            }
            for a in &check.failed_assertions {
                tracing::warn!("task {name}: dropping flaky assertion: {a}");
                outcome.flaky_assertions.push(a.to_string());
            }
            flow.assertions
                .retain(|a| !check.failed_assertions.contains(a));
            if flow.assertions.is_empty() {
                tracing::warn!("task {name}: no stable assertions left");
                return Ok(false);
            }
        }
        Ok(false)
    }
}

fn slug(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "task".into() } else { s }
}

/// Human-readable report with a Mermaid state diagram.
pub fn markdown(r: &ExploreReport) -> String {
    let esc = |s: &str| s.replace('"', "'");
    let mut m = format!(
        "# Exploration of {}\n\n```mermaid\nflowchart LR\n",
        r.start_url
    );
    for s in &r.states {
        m.push_str(&format!("  s{}[\"{}\"]\n", s.id, esc(&s.label)));
    }
    for t in &r.tasks {
        if let Some(to) = &t.to_state {
            let style = if t.verified { "-->" } else { "-.->" };
            m.push_str(&format!(
                "  s{} {style}|{}| s{}\n",
                t.from_state,
                esc(&t.name),
                to
            ));
        }
    }
    m.push_str("```\n\n| task | kind | result | flow |\n|---|---|---|---|\n");
    for t in &r.tasks {
        let result = match (t.success, t.verified) {
            _ if t.blocked => "⛔ blocked by deny list",
            (true, true) if !t.flaky_assertions.is_empty() => {
                "✅ verified (flaky assertions dropped)"
            }
            (true, true) => "✅ verified",
            (true, false) => "⚠️ unverified",
            // The page did not meet the proposed expectation: either a bug or an
            // over-specified goal. A human has to decide.
            _ => "🔍 needs review",
        };
        let flow = t
            .flow
            .as_ref()
            .map(|p| format!("`{}`", p.display()))
            .unwrap_or_default();
        m.push_str(&format!(
            "| {} | {} | {result} | {flow} |\n",
            t.name, t.kind
        ));
    }
    m.push_str("\n## Goals\n\n");
    for t in &r.tasks {
        m.push_str(&format!(
            "- **{}**: {}\n  - {}\n",
            t.name, t.goal, t.summary
        ));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Login OK!"), "login_ok");
        assert_eq!(slug("登录"), "task");
    }

    #[test]
    fn markdown_has_graph_and_table() {
        let r = ExploreReport {
            start_url: "http://x/".into(),
            states: vec![
                StateInfo {
                    id: "a".into(),
                    label: "Login".into(),
                    depth: 0,
                    reached_by: None,
                },
                StateInfo {
                    id: "b".into(),
                    label: "Shop".into(),
                    depth: 1,
                    reached_by: Some("login".into()),
                },
            ],
            tasks: vec![TaskOutcome {
                name: "login".into(),
                goal: "g".into(),
                kind: "positive".into(),
                from_state: "a".into(),
                to_state: Some("b".into()),
                success: true,
                verified: true,
                blocked: false,
                flaky_assertions: vec![],
                flow: Some("flows/login.yaml".into()),
                summary: "ok".into(),
            }],
        };
        let md = markdown(&r);
        assert!(md.contains("sa -->|login| sb"));
        assert!(md.contains("| login | positive | ✅ verified | `flows/login.yaml` |"));
    }
}
