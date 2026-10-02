//! Deterministic replay of a [`Flow`] — no LLM unless a locator breaks.
//!
//! Self-healing only repairs *locators* (an element was renamed or moved).
//! Failed assertions and new console/network errors are never healed: they
//! are the regressions the test exists to catch.

use std::time::{Duration, Instant};

use anyhow::Result;
use llm::{Llm, extract_json};
use mcp_client::Browser;
use serde::Serialize;

use crate::flow::{Assertion, Flow, map_uids, placeholder, relativize};
use crate::{Locator, collect_diagnostics, snapshot};

#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// How long to wait for an element / assertion to appear.
    pub timeout: Duration,
    pub poll: Duration,
    /// Replaces the flow's start URL (e.g. run against staging).
    pub url_override: Option<String>,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            poll: Duration::from_millis(400),
            url_override: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Heal {
    pub step: usize,
    pub intent: String,
    pub from: Locator,
    pub to: Locator,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    pub name: String,
    pub passed: bool,
    pub steps_run: usize,
    pub steps_total: usize,
    pub failures: Vec<String>,
    pub heals: Vec<Heal>,
    /// Assertions that did not hold (also listed in `failures`).
    pub failed_assertions: Vec<Assertion>,
    pub duration_ms: u128,
    /// The flow with healed locators applied (only when something was healed).
    #[serde(skip)]
    pub healed_flow: Option<Flow>,
    /// Page snapshot at the moment of failure, for debugging.
    #[serde(skip)]
    pub failure_snapshot: Option<String>,
}

pub async fn replay(
    browser: &Browser,
    flow: &Flow,
    healer: Option<&dyn Llm>,
    opts: &ReplayOptions,
) -> Result<ReplayReport> {
    let started = Instant::now();
    let mut report = ReplayReport {
        name: flow.name.clone(),
        passed: false,
        steps_run: 0,
        steps_total: flow.steps.len(),
        failures: Vec::new(),
        heals: Vec::new(),
        failed_assertions: Vec::new(),
        duration_ms: 0,
        healed_flow: None,
        failure_snapshot: None,
    };
    let mut healed = flow.clone();

    let url = opts.url_override.as_deref().unwrap_or(&flow.start_url);
    let nav = browser.navigate(url).await?;
    if nav.is_error {
        report
            .failures
            .push(format!("could not open {url}: {}", nav.text));
    } else {
        run_steps(browser, flow, healer, opts, &mut report, &mut healed).await?;
    }

    if report.failures.is_empty() {
        check_assertions(browser, &flow.assertions, opts, &mut report).await?;
        check_diagnostics(browser, flow, url, &mut report).await?;
    }

    report.passed = report.failures.is_empty();
    if !report.passed {
        report.failure_snapshot = browser.snapshot().await.ok();
    }
    if !report.heals.is_empty() {
        report.healed_flow = Some(healed);
    }
    report.duration_ms = started.elapsed().as_millis();
    Ok(report)
}

/// Opens the flow's start page and runs its steps without healing, assertions
/// or diagnostics — used to bring a browser into a known state.
/// Returns the first failure, if any.
pub async fn reach(browser: &Browser, flow: &Flow, opts: &ReplayOptions) -> Result<Option<String>> {
    let url = opts.url_override.as_deref().unwrap_or(&flow.start_url);
    let nav = browser.navigate(url).await?;
    if nav.is_error {
        return Ok(Some(format!("could not open {url}: {}", nav.text)));
    }
    let mut report = ReplayReport {
        name: flow.name.clone(),
        passed: false,
        steps_run: 0,
        steps_total: flow.steps.len(),
        failures: Vec::new(),
        heals: Vec::new(),
        failed_assertions: Vec::new(),
        duration_ms: 0,
        healed_flow: None,
        failure_snapshot: None,
    };
    let mut scratch = flow.clone();
    run_steps(browser, flow, None, opts, &mut report, &mut scratch).await?;
    Ok(report.failures.into_iter().next())
}

async fn run_steps(
    browser: &Browser,
    flow: &Flow,
    healer: Option<&dyn Llm>,
    opts: &ReplayOptions,
    report: &mut ReplayReport,
    healed: &mut Flow,
) -> Result<()> {
    for (i, step) in flow.steps.iter().enumerate() {
        let n = i + 1;
        let mut uids = Vec::with_capacity(step.targets.len());

        if !step.targets.is_empty() {
            let (snap, mut resolved) = wait_for_targets(browser, &step.targets, opts).await?;
            for (t, (loc, uid)) in resolved.iter_mut().enumerate() {
                if uid.is_some() {
                    continue;
                }
                let Some(llm) = healer else { break };
                tracing::warn!("step {n}: {loc} not found, asking the LLM to heal");
                match heal_locator(llm, &step.intent, &step.tool, loc, &snap).await {
                    Ok(Some((new_uid, new_loc, reason))) => {
                        tracing::warn!("step {n}: healed {loc} -> {new_loc} ({reason})");
                        report.heals.push(Heal {
                            step: n,
                            intent: step.intent.clone(),
                            from: loc.clone(),
                            to: new_loc.clone(),
                            reason,
                        });
                        healed.steps[i].targets[t] = new_loc;
                        *uid = Some(new_uid);
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("step {n}: healing failed: {e:#}"),
                }
            }
            if let Some((loc, _)) = resolved.iter().find(|(_, u)| u.is_none()) {
                report.failures.push(format!(
                    "step {n} ({}): element {loc} not found — {}",
                    step.tool, step.intent
                ));
                return Ok(());
            }
            uids = resolved.into_iter().filter_map(|(_, u)| u).collect();
        }

        let args = map_uids(&step.args, &|s| {
            (0..uids.len())
                .find(|&k| placeholder(k) == s)
                .map(|k| uids[k].clone())
        });
        tracing::info!("step {n}: {} {args} — {}", step.tool, step.intent);
        let out = browser.call(&step.tool, args).await?;
        report.steps_run = n;
        if out.is_error {
            report.failures.push(format!(
                "step {n} ({}) failed: {}",
                step.tool,
                out.text.trim()
            ));
            return Ok(());
        }
    }
    Ok(())
}

/// Polls snapshots until every locator resolves or the timeout passes.
/// Returns the last snapshot and each locator with its uid (if found).
async fn wait_for_targets(
    browser: &Browser,
    targets: &[Locator],
    opts: &ReplayOptions,
) -> Result<(String, Vec<(Locator, Option<String>)>)> {
    let deadline = Instant::now() + opts.timeout;
    loop {
        let snap = browser.snapshot().await?;
        let resolved: Vec<_> = targets
            .iter()
            .map(|l| (l.clone(), snapshot::resolve(&snap, l)))
            .collect();
        if resolved.iter().all(|(_, u)| u.is_some()) || Instant::now() >= deadline {
            return Ok((snap, resolved));
        }
        tokio::time::sleep(opts.poll).await;
    }
}

async fn check_assertions(
    browser: &Browser,
    assertions: &[Assertion],
    opts: &ReplayOptions,
    report: &mut ReplayReport,
) -> Result<()> {
    if assertions.is_empty() {
        return Ok(());
    }
    let deadline = Instant::now() + opts.timeout;
    loop {
        let snap = browser.snapshot().await?;
        let failed: Vec<_> = assertions.iter().filter(|a| !a.check(&snap)).collect();
        if failed.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            report
                .failures
                .extend(failed.iter().map(|a| format!("assertion failed: {a}")));
            report.failed_assertions = failed.into_iter().cloned().collect();
            return Ok(());
        }
        tokio::time::sleep(opts.poll).await;
    }
}

/// Only errors that were not present when the flow was recorded fail the run.
async fn check_diagnostics(
    browser: &Browser,
    flow: &Flow,
    url: &str,
    report: &mut ReplayReport,
) -> Result<()> {
    let d = collect_diagnostics(browser).await?;
    for e in d.console_errors {
        if !flow.allowed_console_errors.contains(&e) {
            report.failures.push(format!("new console error: {e}"));
        }
    }
    for r in d.failed_requests {
        let r = relativize(&r, url);
        if !flow.allowed_failed_requests.contains(&r) {
            report.failures.push(format!("new failed request: {r}"));
        }
    }
    Ok(())
}

/// Asks the LLM which element in the current page plays the role of a
/// locator that no longer matches. Returns `(uid, new locator, reason)`.
async fn heal_locator(
    llm: &dyn Llm,
    intent: &str,
    tool: &str,
    lost: &Locator,
    raw_snapshot: &str,
) -> Result<Option<(String, Locator, String)>> {
    let system = r#"You repair broken locators in an automated UI regression test.
A recorded step targets an element that no longer exists under its old role/name. Find the element in the CURRENT PAGE that serves the same purpose (e.g. a renamed button), using the step's intent.
Reply with ONLY JSON: {"uid": "<uid from CURRENT PAGE>" | null, "reason": "<short>"}
Be conservative: return null if no element clearly serves the same purpose, or if the page is in a different state than the step expects (that is a real failure, not a renamed element)."#;
    let user = format!(
        "STEP INTENT: {intent}\nTOOL: {tool}\nOLD LOCATOR: {lost}\n\nCURRENT PAGE:\n{}\nYour JSON reply:",
        snapshot::compact(raw_snapshot, 30_000)
    );
    let v = extract_json(&llm.complete(system, &user).await?)?;
    let reason = v["reason"].as_str().unwrap_or_default().to_string();
    let Some(uid) = v["uid"].as_str() else {
        tracing::warn!("healer declined: {reason}");
        return Ok(None);
    };
    match snapshot::locator_for(raw_snapshot, uid) {
        Some(loc) => Ok(Some((uid.to_string(), loc, reason))),
        None => {
            tracing::warn!("healer returned unknown uid {uid}");
            Ok(None)
        }
    }
}

/// Renders reports as JUnit XML (one testsuite, one testcase per flow).
pub fn junit_xml(reports: &[ReplayReport]) -> String {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    let failures = reports.iter().filter(|r| !r.passed).count();
    let total_ms: u128 = reports.iter().map(|r| r.duration_ms).sum();
    let mut x = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"webtest\" tests=\"{}\" failures=\"{failures}\" time=\"{:.3}\">\n",
        reports.len(),
        total_ms as f64 / 1000.0
    );
    for r in reports {
        x.push_str(&format!(
            "  <testcase name=\"{}\" time=\"{:.3}\">\n",
            esc(&r.name),
            r.duration_ms as f64 / 1000.0
        ));
        if !r.passed {
            x.push_str(&format!(
                "    <failure message=\"{}\">{}</failure>\n",
                esc(r.failures.first().map(String::as_str).unwrap_or("failed")),
                esc(&r.failures.join("\n"))
            ));
        }
        if !r.heals.is_empty() {
            let lines: Vec<String> = r
                .heals
                .iter()
                .map(|h| format!("step {}: {} -> {} ({})", h.step, h.from, h.to, h.reason))
                .collect();
            x.push_str(&format!(
                "    <system-out>healed locators:\n{}</system-out>\n",
                esc(&lines.join("\n"))
            ));
        }
        x.push_str("  </testcase>\n");
    }
    x.push_str("</testsuite>\n");
    x
}
