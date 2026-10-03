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

use crate::crossreview::{self, FlowReview, Proposal, ProposalReview};
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
    /// Login flow recorded in generated flows (for session refresh on replay).
    pub login_flow_path: Option<String>,
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
    /// Who proposed the task: "claude" or "external" (added by the cross-reviewer).
    #[serde(default = "claude_source")]
    pub source: String,
    /// External AI review of the flow's assertions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<FlowReview>,
}

fn claude_source() -> String {
    "claude".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExploreReport {
    pub start_url: String,
    pub states: Vec<StateInfo>,
    pub tasks: Vec<TaskOutcome>,
    /// The external cross-reviewer, when one was configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposal_reviews: Vec<ProposalReview>,
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
    /// External AI (another vendor) that cross-reviews proposals and flows.
    reviewer: Option<&'a dyn Llm>,
    cfg: ExploreConfig,
}

/// Actions that end a login session. With a shared saved login state, one task doing this
/// invalidates the session for every other task and for replay verification.
pub const SESSION_ENDING_KEYWORDS: &[&str] = &["退出登录", "登出", "logout", "log out", "sign out"];

/// The deny list actually used: with a shared login state, session-ending actions are added
/// (the list is also shown to the task proposer and the cross-reviewer as off-limits).
pub fn effective_deny(cfg: &ExploreConfig) -> Vec<String> {
    let mut deny = cfg.agent.deny.clone();
    if cfg.storage_state_path.is_some() {
        for k in SESSION_ENDING_KEYWORDS {
            if !deny.iter().any(|d| d.eq_ignore_ascii_case(k)) {
                deny.push((*k).to_string());
            }
        }
    }
    deny
}

impl<'a> Explorer<'a> {
    pub fn new(llm: &'a dyn Llm, mut cfg: ExploreConfig) -> Self {
        cfg.agent.deny = effective_deny(&cfg);
        Self {
            llm,
            reviewer: None,
            cfg,
        }
    }

    pub fn with_reviewer(mut self, reviewer: Option<&'a dyn Llm>) -> Self {
        self.reviewer = reviewer;
        self
    }

    /// Explores from `start_url`, writing verified flows to `out_dir`.
    pub async fn run(&self, start_url: &str, out_dir: &Path) -> Result<ExploreReport> {
        std::fs::create_dir_all(out_dir)?;
        let mut report = ExploreReport {
            start_url: start_url.to_string(),
            reviewer: self.reviewer.map(|r| r.describe()),
            ..Default::default()
        };
        let root = Flow {
            name: "start".into(),
            goal: String::new(),
            start_url: start_url.to_string(),
            storage_state: self.cfg.storage_state_path.clone(),
            login_flow: self.cfg.login_flow_path.clone(),
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

            let mut proposals = match self
                .propose(&snap, &known_goals, budget.min(self.cfg.tasks_per_state))
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("task proposal failed for state {id}: {e:#}");
                    continue;
                }
            };
            if let Some(reviewer) = self.reviewer {
                let (reviewed, record) = self
                    .cross_review_proposals(reviewer, &snap, &id, proposals, &known_goals, budget)
                    .await;
                proposals = reviewed;
                report.proposal_reviews.push(record);
            }
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
            source: p.source.clone(),
            review: None,
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
        flow.login_flow = prefix.login_flow.clone();
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

        let mut verified = self.verify(name, &mut flow, outcome).await?;
        if let (true, Some(reviewer)) = (verified, self.reviewer) {
            verified = self
                .review_flow(reviewer, name, &trace, &mut flow, outcome)
                .await?;
        }
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
    /// External AI reviews Claude's proposals for one state: may veto (with a reason) and add.
    /// On reviewer failure the proposals are kept unchanged and the error is recorded.
    async fn cross_review_proposals(
        &self,
        reviewer: &dyn Llm,
        snap: &str,
        state_id: &str,
        proposals: Vec<Proposal>,
        known: &[String],
        budget: usize,
    ) -> (Vec<Proposal>, ProposalReview) {
        let mut record = ProposalReview {
            state: state_id.to_string(),
            claude: proposals.iter().map(|p| p.name.clone()).collect(),
            ..Default::default()
        };
        let max_additions = (self.cfg.tasks_per_state.div_ceil(2))
            .min(budget.saturating_sub(proposals.len()).max(1));
        let (system, user) = crossreview::proposal_review_prompt(
            snap,
            &self.cfg.context,
            &self.cfg.agent.deny,
            &proposals,
            known,
            max_additions,
        );
        tracing::info!(
            "  external review of {} proposals ({})",
            proposals.len(),
            reviewer.describe()
        );
        let reply = match reviewer
            .complete(&system, &user)
            .await
            .and_then(|t| extract_json(&t))
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("  proposal review failed, keeping Claude's proposals: {e:#}");
                record.error = Some(format!("{e:#}"));
                return (proposals, record);
            }
        };
        let (kept, dropped, added) =
            crossreview::apply_proposal_review(proposals, &reply, max_additions);
        let (kept, skipped) = crossreview::fit_to_budget(kept, budget);
        if !skipped.is_empty() {
            tracing::info!("  task budget: not running {}", skipped.join(", "));
        }
        record.skipped_for_budget = skipped;
        for d in &dropped {
            tracing::info!("  reviewer dropped {}: {}", d.item, d.reason);
        }
        for a in &added {
            tracing::info!("  reviewer added {a}");
        }
        record.dropped = dropped;
        record.added = added
            .into_iter()
            .filter(|a| kept.iter().any(|k| &k.name == a))
            .collect();
        (kept, record)
    }

    /// External AI reviews a verified flow's assertions. Changes are applied only if the
    /// flow still passes LLM-free verification; otherwise the original assertions are restored.
    async fn review_flow(
        &self,
        reviewer: &dyn Llm,
        name: &str,
        trace: &crate::Trace,
        flow: &mut Flow,
        outcome: &mut TaskOutcome,
    ) -> Result<bool> {
        let (system, user) =
            crossreview::flow_review_prompt(&flow.goal, &trace.final_snapshot, &flow.assertions);
        tracing::info!(
            "task {name}: external review of {} assertions",
            flow.assertions.len()
        );
        let reply = match reviewer
            .complete(&system, &user)
            .await
            .and_then(|t| extract_json(&t))
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("task {name}: flow review failed, keeping assertions: {e:#}");
                outcome.review = Some(FlowReview {
                    error: Some(format!("{e:#}")),
                    ..Default::default()
                });
                return Ok(true);
            }
        };
        let before = flow.assertions.clone();
        let (after, mut review) =
            crossreview::apply_flow_review(&before, &reply, &trace.final_snapshot);
        if after != before {
            flow.assertions = after;
            if !self.verify(name, flow, outcome).await? {
                tracing::warn!("task {name}: reviewed assertions failed verification, reverting");
                flow.assertions = before;
                review.reverted = true;
                review.dropped.clear();
                review.added.clear();
            }
        }
        outcome.review = Some(review);
        Ok(true)
    }

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

/// "External AI cross-review" section: proposal vetoes/additions per state, assertion changes per flow.
fn cross_review_markdown(r: &ExploreReport, reviewer: &str) -> String {
    let cell = |s: &str| s.replace('|', "\\|").replace('\n', " ");
    let mut m = format!(
        "\n## 外部 AI 交叉评审\n\n评审者：`{}`（实施方：Claude）\n\n### 任务提案\n\n| 状态 | Claude 提出 | 外部 AI 否决（原因） | 外部 AI 补充 | 因任务预算未执行 |\n|---|---|---|---|---|\n",
        cell(reviewer)
    );
    for pr in &r.proposal_reviews {
        let dropped = if let Some(e) = &pr.error {
            format!("评审失败：{}", cell(e))
        } else {
            pr.dropped
                .iter()
                .map(|d| format!("{}（{}）", d.item, cell(&d.reason)))
                .collect::<Vec<_>>()
                .join("<br>")
        };
        m.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            pr.state,
            pr.claude.join(", "),
            dropped,
            pr.added.join(", "),
            pr.skipped_for_budget.join(", ")
        ));
    }
    let reviewed: Vec<&TaskOutcome> = r.tasks.iter().filter(|t| t.review.is_some()).collect();
    if !reviewed.is_empty() {
        m.push_str("\n### 用例断言\n\n| 用例 | 删除（原因） | 补充 | 说明 |\n|---|---|---|---|\n");
        for t in reviewed {
            let rv = t.review.as_ref().expect("filtered");
            let dropped = rv
                .dropped
                .iter()
                .map(|d| format!("{}（{}）", cell(&d.item), cell(&d.reason)))
                .collect::<Vec<_>>()
                .join("<br>");
            let mut note = cell(&rv.summary);
            if rv.reverted {
                note = format!("改动未通过回放验证，已回退。{note}");
            }
            if !rv.rejected_additions.is_empty() {
                note = format!(
                    "{note} 未采纳（录制页面上不成立）：{}",
                    cell(&rv.rejected_additions.join("；"))
                );
            }
            if let Some(e) = &rv.error {
                note = format!("评审失败：{}", cell(e));
            }
            m.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                t.name,
                dropped,
                cell(&rv.added.join("；")),
                note
            ));
        }
    }
    m
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
    m.push_str("```\n\n| task | kind | source | result | flow |\n|---|---|---|---|---|\n");
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
        let source = if t.source == "external" {
            "外部 AI"
        } else {
            "Claude"
        };
        m.push_str(&format!(
            "| {} | {} | {source} | {result} | {flow} |\n",
            t.name, t.kind
        ));
    }
    if let Some(reviewer) = &r.reviewer {
        m.push_str(&cross_review_markdown(r, reviewer));
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

    fn cfg(storage: Option<&str>) -> ExploreConfig {
        ExploreConfig {
            browser: mcp_client::BrowserConfig::default(),
            agent: AgentConfig {
                deny: vec!["删除".into(), "Logout".into()],
                ..Default::default()
            },
            replay: ReplayOptions::default(),
            storage_state_path: storage.map(str::to_string),
            login_flow_path: None,
            context: String::new(),
            max_tasks: 1,
            tasks_per_state: 1,
            max_depth: 1,
            jobs: 1,
            verify_runs: 1,
        }
    }

    #[test]
    fn shared_login_state_puts_session_ending_actions_off_limits() {
        assert_eq!(effective_deny(&cfg(None)), ["删除", "Logout"]);
        let d = effective_deny(&cfg(Some("auth/a.json")));
        assert!(d.contains(&"退出登录".to_string()) && d.contains(&"sign out".to_string()));
        assert_eq!(
            d.iter()
                .filter(|k| k.eq_ignore_ascii_case("logout"))
                .count(),
            1,
            "no duplicates"
        );
    }

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
                source: "claude".into(),
                review: None,
                summary: "ok".into(),
            }],
            ..Default::default()
        };
        let md = markdown(&r);
        assert!(md.contains("sa -->|login| sb"));
        assert!(md.contains("| login | positive | Claude | ✅ verified | `flows/login.yaml` |"));
        assert!(!md.contains("外部 AI 交叉评审"), "no reviewer, no section");
    }

    #[test]
    fn markdown_reports_cross_review() {
        use crate::crossreview::Dropped;
        let mut t = TaskOutcome {
            name: "wrong_password".into(),
            goal: "g".into(),
            kind: "negative".into(),
            from_state: "a".into(),
            to_state: None,
            success: true,
            verified: true,
            blocked: false,
            flaky_assertions: vec![],
            flow: None,
            summary: String::new(),
            source: "external".into(),
            review: None,
        };
        t.review = Some(FlowReview {
            dropped: vec![Dropped {
                item: "text \"hi\"".into(),
                reason: "random greeting".into(),
            }],
            added: vec!["text \"错误\"".into()],
            ..Default::default()
        });
        let r = ExploreReport {
            start_url: "http://x/".into(),
            tasks: vec![t],
            reviewer: Some("command (node external-ai.mjs)".into()),
            proposal_reviews: vec![ProposalReview {
                state: "a".into(),
                claude: vec!["login".into(), "delete".into()],
                dropped: vec![Dropped {
                    item: "delete".into(),
                    reason: "irreversible".into(),
                }],
                added: vec!["wrong_password".into()],
                skipped_for_budget: vec![],
                error: None,
            }],
            ..Default::default()
        };
        let md = markdown(&r);
        assert!(md.contains("| wrong_password | negative | 外部 AI |"));
        assert!(md.contains("| a | login, delete | delete（irreversible） | wrong_password |  |"));
        assert!(md.contains("random greeting") && md.contains("text \"错误\""));
    }
}
