//! agent-bridge command line.
//!
//! `serve` runs the service; `hash-token` prints the SHA-256 the service stores for a bearer
//! token (token on stdin, so it never appears in argv or shell history).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use agent_bridge::bridge::Bridge;
use agent_bridge::service::{self, AppState};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agent-bridge",
    version,
    about = "Two-way task dispatch and collaboration between agents (Claude Code ⇄ DSH)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP service
    Serve {
        /// State directory (created 0700). Default: ~/.local/state/agent-bridge
        #[arg(long)]
        state: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:7879")]
        listen: String,
        /// JSON object agent → sha256 of its token (see hash-token). Default: ~/.config/agent-bridge/agents.json
        #[arg(long)]
        agents: Option<PathBuf>,
    },
    /// Read a token on stdin and print the hash to put in agents.json
    HashToken,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::HashToken => {
            let mut t = String::new();
            std::io::stdin().read_to_string(&mut t)?;
            let t = t.trim();
            anyhow::ensure!(t.len() >= 32, "token too short (need >= 32 chars)");
            println!("{}", service::hex(&service::sha256(t)));
            Ok(())
        }
        Cmd::Serve {
            state,
            listen,
            agents,
        } => {
            let state = state.unwrap_or_else(|| home().join(".local/state/agent-bridge"));
            let agents_file =
                agents.unwrap_or_else(|| home().join(".config/agent-bridge/agents.json"));
            let raw = std::fs::read_to_string(&agents_file)
                .with_context(|| format!("reading {}", agents_file.display()))?;
            let map: BTreeMap<String, String> = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", agents_file.display()))?;
            anyhow::ensure!(
                map.len() >= 2,
                "{} must name at least two agents",
                agents_file.display()
            );
            let mut tokens = Vec::new();
            for (agent, h) in &map {
                let h = service::parse_hex32(h)
                    .with_context(|| format!("agent {agent}: expected a 64-hex-digit sha256"))?;
                tokens.push((agent.clone(), h));
            }
            let names: Vec<&str> = map.keys().map(String::as_str).collect();
            let bridge = Bridge::open(&state, &names)?;
            if bridge.bad_lines > 0 {
                eprintln!(
                    "agent-bridge: skipped {} unreadable line(s) while loading",
                    bridge.bad_lines
                );
            }
            let app = Arc::new(AppState::new(bridge, tokens)?);
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .with_context(|| format!("binding {listen}"))?;
            eprintln!(
                "agent-bridge listening on {listen}, state {}, agents {names:?}",
                state.display()
            );
            service::serve(app, listener).await?;
            Ok(())
        }
    }
}
