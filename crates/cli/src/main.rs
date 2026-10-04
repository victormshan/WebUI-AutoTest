use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use agent::explore::{ExploreConfig, Explorer};
use agent::flow::{Flow, suggest_assertions};
use agent::replay::{ReplayOptions, ReplayReport, junit_xml, markdown_summary, replay, status};
use agent::{Agent, AgentConfig, Trace};
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use mcp_client::{Browser, BrowserConfig, StorageState};
use tokio::io::AsyncBufReadExt;

#[derive(Parser)]
#[command(name = "webtest", about = "Web testing driven by chrome-devtools-mcp")]
struct Cli {
    #[command(flatten)]
    browser: BrowserArgs,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct BrowserArgs {
    /// Chrome executable (defaults to ~/.local/bin/chrome-wsl when present)
    #[arg(long, env = "WEBTEST_CHROME", global = true)]
    chrome: Option<PathBuf>,
    /// Show the browser window instead of running headless
    #[arg(long, global = true)]
    headed: bool,
    /// npm spec for the MCP server
    #[arg(
        long,
        env = "WEBTEST_MCP_PACKAGE",
        default_value = "chrome-devtools-mcp@latest",
        global = true
    )]
    mcp_package: String,
    /// Saved login state (from `webtest login`) loaded into every browser.
    /// For `replay` it only replaces the path of flows that declare `storage_state`;
    /// flows without it start logged out.
    #[arg(long, env = "WEBTEST_STORAGE_STATE", global = true)]
    storage_state: Option<PathBuf>,
    /// Login flow used to (re)create the storage state when it is missing or
    /// a flow fails; for `replay` it replaces the path of flows that declare `login_flow`.
    #[arg(long, env = "WEBTEST_LOGIN_FLOW", global = true)]
    login_flow: Option<PathBuf>,
}

impl BrowserArgs {
    fn config(&self) -> Result<BrowserConfig> {
        let mut cfg = self.base_config();
        if let Some(p) = &self.storage_state {
            cfg.storage_state = Some(load_state(p)?);
        }
        Ok(cfg)
    }

    fn base_config(&self) -> BrowserConfig {
        let chrome = self.chrome.clone().or_else(|| {
            let p = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/chrome-wsl");
            p.exists().then_some(p)
        });
        BrowserConfig {
            package: self.mcp_package.clone(),
            chrome_path: chrome,
            headless: !self.headed,
            ..Default::default()
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// List the tools exposed by chrome-devtools-mcp
    Tools,
    /// Open a URL and print its accessibility snapshot
    Snapshot { url: String },
    /// Let the LLM agent accomplish a goal on a page and record a trace
    Run(RunArgs),
    /// Turn a successful trace into a replayable flow (YAML)
    Save(SaveArgs),
    /// Replay flows deterministically; the LLM is only used to heal broken locators
    Replay(ReplayArgs),
    /// Log in once and save cookies + localStorage for reuse (--storage-state)
    Login(LoginArgs),
    /// Discover user tasks automatically and record them as verified flows
    Explore(ExploreArgs),
    /// Show which LLM backend is configured and send it a test prompt
    CheckLlm {
        #[arg(long, env = "WEBTEST_MODEL")]
        model: Option<String>,
        /// Check the cross-reviewer (WEBTEST_REVIEW_* / --review-command) instead
        #[arg(long)]
        reviewer: bool,
        #[arg(long)]
        review_command: Option<String>,
    },
    /// Read `<tool> [json-args]` lines from stdin and print each result (debugging aid)
    Repl,
}

#[derive(Args)]
struct RunArgs {
    /// Start URL
    #[arg(long)]
    url: String,
    /// What the agent should accomplish, in natural language
    #[arg(long)]
    goal: String,
    #[arg(long, default_value_t = 25)]
    max_steps: usize,
    /// Model id (backend: Anthropic API if ANTHROPIC_API_KEY is set, else `claude -p`)
    #[arg(long, env = "WEBTEST_MODEL")]
    model: Option<String>,
    /// Refuse actions on elements containing these keywords (comma-separated)
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "删除,delete,remove,注销账号"
    )]
    deny: Vec<String>,
    /// Directory for trace JSON files
    #[arg(long, default_value = "runs")]
    out: PathBuf,
    /// On success, also save a replayable flow to this YAML file
    #[arg(long)]
    save: Option<PathBuf>,
}

#[derive(Args)]
struct SaveArgs {
    /// Trace JSON written by `webtest run`
    trace: PathBuf,
    /// Output flow YAML
    flow: PathBuf,
    /// Flow name (defaults to the output file stem)
    #[arg(long)]
    name: Option<String>,
    #[arg(long, env = "WEBTEST_MODEL")]
    model: Option<String>,
    /// Don't use the LLM to pick assertions; use page headings/alerts instead
    #[arg(long)]
    no_llm: bool,
}

#[derive(Args)]
struct ReplayArgs {
    /// Flow YAML files
    #[arg(required = true)]
    flows: Vec<PathBuf>,
    /// Override every flow's start URL (e.g. a staging server)
    #[arg(long)]
    url: Option<String>,
    /// Never call the LLM; a missing element fails the flow
    #[arg(long)]
    no_heal: bool,
    /// Write healed locators back into the flow files (default: `<name>.healed.yaml`)
    #[arg(long)]
    update: bool,
    /// Seconds to wait for elements and assertions
    #[arg(long, default_value_t = 5)]
    timeout: u64,
    /// Write a JUnit XML report
    #[arg(long)]
    junit: Option<PathBuf>,
    /// Write a Markdown summary (e.g. append to $GITHUB_STEP_SUMMARY)
    #[arg(long)]
    markdown: Option<PathBuf>,
    #[arg(long, env = "WEBTEST_MODEL")]
    model: Option<String>,
}

#[derive(Args)]
struct LoginArgs {
    /// Where to write the storage state (contains live session secrets)
    #[arg(long)]
    save_state: PathBuf,
    /// Page to open (required with --goal / --manual; defaults to the flow's start URL)
    #[arg(long)]
    url: Option<String>,
    /// Let the LLM agent log in, e.g. "用 alice / secret123 登录"
    #[arg(long, conflicts_with_all = ["flow", "manual"])]
    goal: Option<String>,
    /// Replay a recorded login flow (no LLM; good for CI)
    #[arg(long, conflicts_with = "manual")]
    flow: Option<PathBuf>,
    /// Open a visible browser, log in by hand, then press Enter here
    #[arg(long)]
    manual: bool,
    #[arg(long, env = "WEBTEST_MODEL")]
    model: Option<String>,
}

#[derive(Args)]
struct ExploreArgs {
    /// Start URL
    #[arg(long)]
    url: String,
    /// External AI cross-reviewer: shell command reading the prompt on stdin and printing the
    /// answer, e.g. "review-gate ask". Without it the WEBTEST_REVIEW_* environment variables are
    /// used (see README); none = no cross-review.
    #[arg(long)]
    review_command: Option<String>,
    /// Test data the agent may use, e.g. "账号 alice / 密码 secret123"
    #[arg(long, default_value = "")]
    context: String,
    /// Total number of tasks to attempt
    #[arg(long, default_value_t = 8)]
    max_tasks: usize,
    /// New tasks proposed per page state
    #[arg(long, default_value_t = 4)]
    tasks_per_state: usize,
    /// How many state transitions deep to explore
    #[arg(long, default_value_t = 2)]
    max_depth: usize,
    /// Tasks run in parallel (one browser each)
    #[arg(long, default_value_t = 2)]
    jobs: usize,
    /// Per-task step limit for the agent
    #[arg(long, default_value_t = 20)]
    max_steps: usize,
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "删除,delete,remove,注销账号"
    )]
    deny: Vec<String>,
    /// Consecutive clean replays required before a flow counts as verified
    #[arg(long, default_value_t = 3)]
    verify_runs: usize,
    /// Seconds to wait for elements and assertions during verification
    #[arg(long, default_value_t = 5)]
    timeout: u64,
    #[arg(long, env = "WEBTEST_MODEL")]
    model: Option<String>,
    /// Output directory for flows and the exploration report
    #[arg(long, default_value = "flows/explored")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,webtest=info,agent=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match dispatch(Cli::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Returns whether the command "passed".
async fn dispatch(cli: Cli) -> Result<bool> {
    match cli.cmd {
        Cmd::Save(args) => save(args).await,
        Cmd::CheckLlm {
            model,
            reviewer,
            review_command,
        } => check_llm(model, reviewer || review_command.is_some(), review_command).await,
        Cmd::Login(args) => login(&cli.browser, args).await,
        Cmd::Replay(args) => replay_all(&cli.browser, args).await,
        Cmd::Explore(args) => {
            let state = cli
                .browser
                .storage_state
                .as_ref()
                .map(|p| p.display().to_string());
            let login = cli
                .browser
                .login_flow
                .as_ref()
                .map(|p| p.display().to_string());
            explore(cli.browser.config()?, state, login, args).await
        }
        cmd => {
            let browser = Browser::launch(&cli.browser.config()?).await?;
            let result = with_browser(&browser, cmd).await;
            browser.close().await?;
            result
        }
    }
}

async fn with_browser(browser: &Browser, cmd: Cmd) -> Result<bool> {
    match cmd {
        Cmd::Tools => {
            for t in browser.tools().await? {
                let summary = t.description.lines().next().unwrap_or_default();
                println!("{:<32} {summary}", t.name);
            }
        }
        Cmd::Snapshot { url } => {
            let nav = browser.navigate(&url).await?;
            anyhow::ensure!(!nav.is_error, "navigation failed: {}", nav.text);
            println!("{}", browser.snapshot().await?);
        }
        Cmd::Run(args) => return run(browser, args).await,
        Cmd::Repl => {
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Some(line) = lines.next_line().await? {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let (tool, args) = line.split_once(' ').unwrap_or((line, "{}"));
                let out = browser.call(tool, serde_json::from_str(args)?).await?;
                println!(
                    "=== {tool}{} ===\n{}",
                    if out.is_error { " (error)" } else { "" },
                    out.text
                );
            }
        }
        Cmd::Save(_) | Cmd::Replay(_) | Cmd::Explore(_) | Cmd::Login(_) | Cmd::CheckLlm { .. } => {
            unreachable!("handled without a shared browser")
        }
    }
    Ok(true)
}

async fn run(browser: &Browser, args: RunArgs) -> Result<bool> {
    let llm = llm::from_env(args.model)?;
    tracing::info!("model backend: {}", llm.describe());
    let cfg = AgentConfig {
        max_steps: args.max_steps,
        deny: args.deny,
        ..Default::default()
    };
    let trace = Agent::new(browser, llm.as_ref(), cfg)
        .run(&args.goal, &args.url)
        .await?;

    std::fs::create_dir_all(&args.out)?;
    let path = args
        .out
        .join(format!("{}.json", trace.started_at.format("%Y%m%d-%H%M%S")));
    std::fs::write(&path, serde_json::to_string_pretty(&trace)?)?;

    println!("{}", if trace.success { "PASS" } else { "FAIL" });
    println!("summary : {}", trace.summary);
    if !trace.evidence.is_empty() {
        println!("evidence: {}", trace.evidence);
    }
    println!("steps   : {}", trace.steps.len());
    for e in &trace.diagnostics.console_errors {
        println!("console : {e}");
    }
    for r in &trace.diagnostics.failed_requests {
        println!("network : {r}");
    }
    println!("trace   : {}", path.display());

    if let (true, Some(flow_path)) = (trace.success, &args.save) {
        let flow = build_flow(&trace, flow_path, None, Some(llm.as_ref())).await?;
        flow.save(flow_path)?;
        println!(
            "flow    : {} ({} steps, {} assertions)",
            flow_path.display(),
            flow.steps.len(),
            flow.assertions.len()
        );
    }
    Ok(trace.success)
}

async fn build_flow(
    trace: &Trace,
    path: &Path,
    name: Option<String>,
    llm: Option<&dyn llm::Llm>,
) -> Result<Flow> {
    let name = name.unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "flow".into())
    });
    let mut flow = Flow::from_trace(trace, &name)?;
    flow.assertions = suggest_assertions(trace, llm).await;
    Ok(flow)
}

async fn save(args: SaveArgs) -> Result<bool> {
    let text = std::fs::read_to_string(&args.trace)
        .with_context(|| format!("reading {}", args.trace.display()))?;
    let trace: Trace = serde_json::from_str(&text)?;
    let llm = (!args.no_llm)
        .then(|| llm::from_env(args.model))
        .transpose()?;
    let flow = build_flow(&trace, &args.flow, args.name, llm.as_deref()).await?;
    flow.save(&args.flow)?;
    println!(
        "{}: {} steps, {} assertions",
        args.flow.display(),
        flow.steps.len(),
        flow.assertions.len()
    );
    for a in &flow.assertions {
        println!("  - {a}");
    }
    Ok(true)
}

async fn replay_all(browser_args: &BrowserArgs, args: ReplayArgs) -> Result<bool> {
    let healer = (!args.no_heal)
        .then(|| llm::from_env(args.model.clone()))
        .transpose()?;
    let opts = ReplayOptions {
        timeout: Duration::from_secs(args.timeout),
        url_override: args.url.clone(),
        ..Default::default()
    };
    // Each state file is refreshed at most once per run.
    let mut refreshed: Vec<PathBuf> = Vec::new();

    let mut reports = Vec::new();
    for path in &args.flows {
        let flow = Flow::load(path)?;
        tracing::info!("replaying {} ({})", flow.name, path.display());
        let state = effective_path(
            browser_args.storage_state.as_deref(),
            flow.storage_state.as_deref(),
        );
        let login_flow = effective_path(
            browser_args.login_flow.as_deref(),
            flow.login_flow.as_deref(),
        );

        if let (Some(s), Some(l)) = (&state, &login_flow)
            && !s.exists()
            && !refreshed.contains(s)
        {
            println!(
                "  no login state at {}; creating it via {}",
                s.display(),
                l.display()
            );
            refresh_state(browser_args, l, args.url.clone(), s).await?;
            refreshed.push(s.clone());
        }

        let mut report = replay_once(
            browser_args,
            &flow,
            state.as_deref(),
            healer.as_deref(),
            &opts,
        )
        .await?;

        // A failure with a login state may just be an expired session:
        // refresh once and retry. A real regression fails again.
        if let (false, Some(s), Some(l)) = (report.passed, &state, &login_flow)
            && !refreshed.contains(s)
        {
            println!(
                "  {} failed with login state {}; refreshing via {} and retrying",
                flow.name,
                s.display(),
                l.display()
            );
            refreshed.push(s.clone());
            match refresh_state(browser_args, l, args.url.clone(), s).await {
                Ok(()) => {
                    report =
                        replay_once(browser_args, &flow, Some(s), healer.as_deref(), &opts).await?;
                    report.session_refreshed = true;
                }
                Err(e) => println!("  login refresh failed: {e:#}"),
            }
        }

        if let Some(healed) = &report.healed_flow {
            let out = if args.update {
                path.clone()
            } else {
                path.with_extension("healed.yaml")
            };
            healed.save(&out)?;
            tracing::warn!("healed flow written to {}", out.display());
        }
        print_report(&report);
        if let (false, Some(p), None) = (report.passed, &state, &login_flow) {
            println!(
                "  hint: ran with login state {0}; if the session expired, refresh it with \
                 `webtest login --flow <login flow> --save-state {0}` or set `login_flow`",
                p.display()
            );
        }
        if let Some(snap) = &report.failure_snapshot {
            std::fs::create_dir_all("runs")?;
            let p = PathBuf::from("runs").join(format!(
                "replay-{}-{}.snapshot.txt",
                report.name,
                chrono::Utc::now().format("%Y%m%d-%H%M%S")
            ));
            std::fs::write(&p, snap)?;
            println!("  page at failure: {}", p.display());
        }
        reports.push(report);
    }

    let passed = reports.iter().filter(|r| r.passed).count();
    println!("\n{passed}/{} flows passed", reports.len());
    if let Some(path) = &args.junit {
        std::fs::write(path, junit_xml(&reports))?;
        println!("junit: {}", path.display());
    }
    if let Some(path) = &args.markdown {
        std::fs::write(path, markdown_summary(&reports))?;
        println!("summary: {}", path.display());
    }
    Ok(passed == reports.len())
}

/// Login settings only apply to flows that declare them: a flow recorded
/// logged out must not start logged in. The CLI value replaces the flow's path.
fn effective_path(cli: Option<&Path>, flow: Option<&str>) -> Option<PathBuf> {
    flow.map(|f| cli.map_or_else(|| PathBuf::from(f), Path::to_path_buf))
}

/// Replays `flow` in a fresh browser (no state leaks between tests).
async fn replay_once(
    browser_args: &BrowserArgs,
    flow: &Flow,
    state: Option<&Path>,
    healer: Option<&dyn llm::Llm>,
    opts: &ReplayOptions,
) -> Result<ReplayReport> {
    let mut cfg = browser_args.base_config();
    if let Some(p) = state {
        cfg.storage_state = Some(load_state(p)?);
    }
    let browser = Browser::launch(&cfg).await?;
    let report = replay(&browser, flow, healer, opts).await;
    browser.close().await?;
    report
}

/// Replays `login_flow` and saves the resulting state to `state_path`.
async fn refresh_state(
    browser_args: &BrowserArgs,
    login_flow: &Path,
    url: Option<String>,
    state_path: &Path,
) -> Result<()> {
    let ok = login(
        browser_args,
        LoginArgs {
            save_state: state_path.to_path_buf(),
            url,
            goal: None,
            flow: Some(login_flow.to_path_buf()),
            manual: false,
            model: None,
        },
    )
    .await?;
    anyhow::ensure!(ok, "login flow {} failed", login_flow.display());
    Ok(())
}

fn print_report(r: &ReplayReport) {
    println!(
        "{:<14} {} — {}/{} steps, {:.1}s",
        status(r),
        r.name,
        r.steps_run,
        r.steps_total,
        r.duration_ms as f64 / 1000.0
    );
    for h in &r.heals {
        println!(
            "  healed step {}: {} -> {} ({})",
            h.step, h.from, h.to, h.reason
        );
    }
    for f in &r.failures {
        println!("  ✗ {f}");
    }
}

async fn explore(
    browser: BrowserConfig,
    storage_state_path: Option<String>,
    login_flow_path: Option<String>,
    args: ExploreArgs,
) -> Result<bool> {
    let llm = llm::from_env(args.model)?;
    tracing::info!("model backend: {}", llm.describe());
    let cfg = ExploreConfig {
        browser,
        storage_state_path,
        login_flow_path,
        agent: AgentConfig {
            max_steps: args.max_steps,
            deny: args.deny,
            ..Default::default()
        },
        replay: ReplayOptions {
            timeout: Duration::from_secs(args.timeout),
            ..Default::default()
        },
        context: args.context,
        max_tasks: args.max_tasks,
        tasks_per_state: args.tasks_per_state,
        max_depth: args.max_depth,
        jobs: args.jobs,
        verify_runs: args.verify_runs,
    };
    let reviewer = reviewer_settings(args.review_command.as_deref())?.map(|s| s.build());
    if let Some(r) = &reviewer {
        tracing::info!("cross-reviewer: {}", r.describe());
        if r.describe() == llm.describe() {
            tracing::warn!(
                "the cross-reviewer is the same model as the implementer; the review is not independent"
            );
        }
    }
    let report = Explorer::new(llm.as_ref(), cfg)
        .with_reviewer(reviewer.as_deref())
        .run(&args.url, &args.out)
        .await?;

    println!("\nstates: {}", report.states.len());
    for s in &report.states {
        let via = s.reached_by.as_deref().unwrap_or("start");
        println!("  {} depth {} via {via:<20} {}", s.id, s.depth, s.label);
    }
    println!("tasks: {}", report.tasks.len());
    for t in &report.tasks {
        let result = match (t.success, t.verified) {
            _ if t.blocked => "BLOCKED",
            (true, true) => "verified",
            (true, false) => "UNVERIFIED",
            _ => "REVIEW",
        };
        let flow = t
            .flow
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        println!("  {result:<10} [{}] {:<24} {flow}", t.kind, t.name);
        for a in &t.flaky_assertions {
            println!("             └ dropped flaky assertion: {a}");
        }
        if !t.success && !t.blocked {
            let why: String = t.summary.chars().take(160).collect();
            println!("             └ {why}");
        }
    }
    if let Some(r) = &report.reviewer {
        let vetoed: usize = report
            .proposal_reviews
            .iter()
            .map(|p| p.dropped.len())
            .sum();
        let added: usize = report.proposal_reviews.iter().map(|p| p.added.len()).sum();
        let changed = report
            .tasks
            .iter()
            .filter_map(|t| t.review.as_ref())
            .filter(|rv| !rv.dropped.is_empty() || !rv.added.is_empty())
            .count();
        println!(
            "cross-review by {r}: {vetoed} task(s) vetoed, {added} added; assertions changed in {changed} flow(s)"
        );
    }
    println!("report: {}", args.out.join("explore.md").display());
    Ok(report.tasks.iter().any(|t| t.verified))
}

fn load_state(path: &Path) -> Result<StorageState> {
    if !path.exists() {
        anyhow::bail!(
            "storage state {} not found — create it with `webtest login --save-state {}`",
            path.display(),
            path.display()
        );
    }
    StorageState::load(path)
}

async fn login(browser_args: &BrowserArgs, args: LoginArgs) -> Result<bool> {
    let mut cfg = browser_args.base_config();
    if args.manual {
        cfg.headless = false;
    }
    let browser = Browser::launch(&cfg).await?;
    let result = login_with(&browser, &args).await;
    let state = match result {
        Ok(true) => browser.storage_state().await,
        Ok(false) => Err(anyhow::anyhow!("login did not succeed; no state saved")),
        Err(e) => Err(e),
    };
    browser.close().await?;
    let state = state?;
    state.save(&args.save_state)?;
    println!(
        "saved {} cookies and localStorage for {} origin(s) to {}",
        state.cookies.len(),
        state.origins.len(),
        args.save_state.display()
    );
    Ok(true)
}

async fn login_with(browser: &Browser, args: &LoginArgs) -> Result<bool> {
    if let Some(path) = &args.flow {
        let flow = Flow::load(path)?;
        let opts = ReplayOptions {
            url_override: args.url.clone(),
            ..Default::default()
        };
        let report = replay(browser, &flow, None, &opts).await?;
        print_report(&report);
        return Ok(report.passed);
    }
    let url = args
        .url
        .as_deref()
        .context("--url is required with --goal or --manual")?;
    if let Some(goal) = &args.goal {
        let llm = llm::from_env(args.model.clone())?;
        let trace = Agent::new(browser, llm.as_ref(), AgentConfig::default())
            .run(goal, url)
            .await?;
        println!(
            "{}: {}",
            if trace.success { "PASS" } else { "FAIL" },
            trace.summary
        );
        return Ok(trace.success);
    }
    anyhow::ensure!(args.manual, "pass one of --goal, --flow or --manual");
    let nav = browser.navigate(url).await?;
    anyhow::ensure!(!nav.is_error, "navigation failed: {}", nav.text);
    println!("Log in in the browser window, then press Enter here to save the session.");
    let mut line = String::new();
    tokio::io::BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await?;
    Ok(true)
}

/// The cross-reviewer from --review-command, else from WEBTEST_REVIEW_* (None if unconfigured).
fn reviewer_settings(review_command: Option<&str>) -> Result<Option<llm::Settings>> {
    match review_command {
        Some(cmd) => Ok(Some(llm::Settings::command(cmd))),
        None => llm::Settings::reviewer_from_env().transpose(),
    }
}

async fn check_llm(
    model: Option<String>,
    reviewer: bool,
    review_command: Option<String>,
) -> Result<bool> {
    let settings = if reviewer {
        reviewer_settings(review_command.as_deref())?
            .context("no reviewer configured: pass --review-command or set WEBTEST_REVIEW_LLM_COMMAND / WEBTEST_REVIEW_LLM_PROVIDER")?
    } else {
        llm::Settings::from_env(model)?
    };
    let llm = settings.build();
    println!("backend: {}", llm.describe());
    let started = std::time::Instant::now();
    let reply = llm
        .complete(
            "You are a connectivity check. Follow the instruction exactly.",
            r#"Reply with only this JSON: {"ok": true}"#,
        )
        .await?;
    println!(
        "reply ({:.1}s): {}",
        started.elapsed().as_secs_f64(),
        reply.trim()
    );
    let ok = llm::extract_json(&reply).is_ok_and(|v| v["ok"] == true);
    println!("{}", if ok { "OK" } else { "UNEXPECTED REPLY" });
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_settings_only_apply_to_flows_that_declare_them() {
        let cli = Path::new("auth/ci.json");
        assert_eq!(
            effective_path(Some(cli), None),
            None,
            "logged-out flow stays logged out"
        );
        assert_eq!(
            effective_path(Some(cli), Some("auth/a.json")),
            Some(cli.to_path_buf())
        );
        assert_eq!(
            effective_path(None, Some("auth/a.json")),
            Some(PathBuf::from("auth/a.json"))
        );
        assert_eq!(effective_path(None, None), None);
    }
}
