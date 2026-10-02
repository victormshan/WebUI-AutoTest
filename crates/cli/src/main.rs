use std::path::PathBuf;

use agent::{Agent, AgentConfig};
use anyhow::Result;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,webtest=info,agent=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let browser = Browser::launch(&cli.browser.config()).await?;
    let result = run(&browser, cli.cmd).await;
    browser.close().await?;
    result
}

async fn run(browser: &Browser, cmd: Cmd) -> Result<()> {
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
        Cmd::Run(args) => {
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
            if !trace.success {
                std::process::exit(1);
            }
        }
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
    }
    Ok(())
}
