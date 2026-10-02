use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use agent::explore::{ExploreConfig, Explorer};
use agent::flow::{Flow, suggest_assertions};
use agent::replay::{ReplayOptions, ReplayReport, junit_xml, replay};
use agent::{Agent, AgentConfig, Trace};
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use mcp_client::{Browser, BrowserConfig};
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
}

impl BrowserArgs {
    fn config(&self) -> BrowserConfig {
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
    /// Discover user tasks automatically and record them as verified flows
    Explore(ExploreArgs),
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
    #[arg(long, env = "WEBTEST_MODEL")]
    model: Option<String>,
}

#[derive(Args)]
struct ExploreArgs {
    /// Start URL
    #[arg(long)]
    url: String,
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
    let cfg = cli.browser.config();
    match cli.cmd {
        Cmd::Save(args) => save(args).await,
        Cmd::Replay(args) => replay_all(&cfg, args).await,
        Cmd::Explore(args) => explore(cfg, args).await,
        cmd => {
            let browser = Browser::launch(&cfg).await?;
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
        Cmd::Save(_) | Cmd::Replay(_) | Cmd::Explore(_) => {
            unreachable!("handled without a shared browser")
        }
    }
    Ok(true)
}

async fn run(browser: &Browser, args: RunArgs) -> Result<bool> {
    let llm = llm::from_env(args.model);
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
    let llm = (!args.no_llm).then(|| llm::from_env(args.model));
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

async fn replay_all(cfg: &BrowserConfig, args: ReplayArgs) -> Result<bool> {
    let healer = (!args.no_heal).then(|| llm::from_env(args.model.clone()));
    let opts = ReplayOptions {
        timeout: Duration::from_secs(args.timeout),
        url_override: args.url.clone(),
        ..Default::default()
    };

    let mut reports = Vec::new();
    for path in &args.flows {
        let flow = Flow::load(path)?;
        tracing::info!("replaying {} ({})", flow.name, path.display());
        // Fresh browser per flow: no state leaks between tests.
        let browser = Browser::launch(cfg).await?;
        let report = replay(&browser, &flow, healer.as_deref(), &opts).await;
        browser.close().await?;
        let report = report?;

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
    Ok(passed == reports.len())
}

fn print_report(r: &ReplayReport) {
    let status = match (r.passed, r.heals.is_empty()) {
        (true, true) => "PASS",
        (true, false) => "PASS (healed)",
        (false, _) => "FAIL",
    };
    println!(
        "{status:<14} {} — {}/{} steps, {:.1}s",
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

async fn explore(browser: BrowserConfig, args: ExploreArgs) -> Result<bool> {
    let llm = llm::from_env(args.model);
    tracing::info!("model backend: {}", llm.describe());
    let cfg = ExploreConfig {
        browser,
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
    let report = Explorer::new(llm.as_ref(), cfg)
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
    println!("report: {}", args.out.join("explore.md").display());
    Ok(report.tasks.iter().any(|t| t.verified))
}
