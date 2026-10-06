//! agent-bridge command line.
//!
//! `serve` runs the service; `hash-token` prints the SHA-256 the service stores for a bearer
//! token (token on stdin, so it never appears in argv or shell history). The other commands are
//! the client side (token from AGENT_BRIDGE_TOKEN[_FILE] or ~/.config/agent-bridge/token);
//! `wait` is what Claude Code runs in the background so that a message wakes it.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use agent_bridge::bridge::Bridge;
use agent_bridge::client::Client;
use agent_bridge::service::{self, AppState};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

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
        /// Push wake-ups: JSON object agent → {url, token_file, transport: auto|native|curl-exe}.
        /// Default: ~/.config/agent-bridge/notify.json if it exists (agents not listed long-poll)
        #[arg(long)]
        notify: Option<PathBuf>,
        /// Read-only mirror of every message (e.g. /mnt/d/cc-tasks/claude-bridge/tasks)
        #[arg(long)]
        mirror: Option<PathBuf>,
        /// claude-step-relay data dir: messages of tasks with an exprId are noted in its trace
        #[arg(long)]
        step_relay_dir: Option<PathBuf>,
    },
    /// Import finished tasks of the old file protocol (run while the service is stopped)
    Import {
        #[arg(long)]
        state: Option<PathBuf>,
        #[arg(long)]
        agents: Option<PathBuf>,
        /// Directory holding <taskId>/msg-<n>-(claude.md|dsh.json)
        #[arg(long)]
        dir: PathBuf,
    },
    /// Read a token on stdin and print the hash to put in agents.json
    HashToken,
    /// Service status
    Health,
    /// Tasks you take part in
    List {
        /// Only unfinished tasks
        #[arg(long)]
        open: bool,
    },
    /// One task with all its messages
    Show { task: String },
    /// Hand another agent a new task
    Send {
        #[arg(long)]
        id: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        title: String,
        /// Task text (Markdown); use --body-file for longer text
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<PathBuf>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        expr_id: Option<String>,
    },
    /// Post a message to a task. --json takes a file with the full message (questions, results, …)
    Post {
        #[arg(long)]
        task: String,
        /// ack | question | answer | progress | result | verdict | close | cancel
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<PathBuf>,
        /// result: done | blocked | rejected
        #[arg(long)]
        outcome: Option<String>,
        /// verdict: pass | rework
        #[arg(long)]
        judgement: Option<String>,
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Unread messages addressed to you (does not mark them read)
    Inbox {
        #[arg(long, default_value_t = 0)]
        wait: u64,
    },
    /// Mark a task's messages handled up to n
    Ack {
        #[arg(long)]
        task: String,
        #[arg(long)]
        n: u32,
    },
    /// Block until a message for you arrives, print it and exit 0 (exit 3 on timeout).
    /// Meant to run in the background so its exit wakes the agent.
    Wait {
        /// Give up after this many seconds (default 7000, just under a 2-hour job limit)
        #[arg(long, default_value_t = 7000)]
        timeout: u64,
    },
    /// MCP server on stdio (the agent's own token)
    Mcp,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).expect("serializable"));
}

fn text(body: Option<String>, file: Option<PathBuf>) -> Result<String> {
    match (body, file) {
        (Some(b), _) => Ok(b),
        (None, Some(f)) => {
            std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))
        }
        (None, None) => Ok(String::new()),
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.cmd {
        Cmd::HashToken => {
            let mut t = String::new();
            std::io::stdin().read_to_string(&mut t)?;
            let t = t.trim();
            anyhow::ensure!(t.len() >= 32, "token too short (need >= 32 chars)");
            println!("{}", service::hex(&service::sha256(t)));
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Health => {
            print(&Client::from_env()?.health().await?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::List { open } => {
            print(
                &Client::from_env()?
                    .get(if open {
                        "/v1/tasks?open=true"
                    } else {
                        "/v1/tasks"
                    })
                    .await?,
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Show { task } => {
            print(
                &Client::from_env()?
                    .get(&format!("/v1/tasks/{task}"))
                    .await?,
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Send {
            id,
            to,
            title,
            body,
            body_file,
            priority,
            expr_id,
        } => {
            let mut meta = json!({ "to": to, "title": title });
            if let Some(p) = priority {
                meta["priority"] = json!(p);
            }
            if let Some(e) = expr_id {
                meta["expr_id"] = json!(e);
            }
            let b =
                json!({ "id": id, "kind": "task", "body": text(body, body_file)?, "meta": meta });
            print(&Client::from_env()?.post("/v1/tasks", b).await?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Post {
            task,
            kind,
            body,
            body_file,
            outcome,
            judgement,
            json: file,
        } => {
            let mut b: Value = match file {
                Some(f) => serde_json::from_str(&std::fs::read_to_string(&f)?)
                    .with_context(|| format!("parsing {}", f.display()))?,
                None => json!({}),
            };
            anyhow::ensure!(b.is_object(), "--json must contain a JSON object");
            if let Some(k) = kind {
                b["kind"] = json!(k);
            }
            let t = text(body, body_file)?;
            if !t.is_empty() {
                b["body"] = json!(t);
            }
            if let Some(o) = outcome {
                b["outcome"] = json!(o);
            }
            if let Some(j) = judgement {
                b["judgement"] = json!(j);
            }
            anyhow::ensure!(
                b.get("kind").is_some(),
                "--kind (or kind in --json) is required"
            );
            print(
                &Client::from_env()?
                    .post(&format!("/v1/tasks/{task}/messages"), b)
                    .await?,
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Inbox { wait } => {
            print(
                &json!({ "messages": Client::from_env()?.inbox(wait.min(service::MAX_WAIT_SECS)).await? }),
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Ack { task, n } => {
            print(&Client::from_env()?.ack(&task, n).await?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Wait { timeout } => {
            let m = Client::from_env()?
                .wait(std::time::Duration::from_secs(timeout), 60)
                .await?;
            if m.is_empty() {
                eprintln!("no message within {timeout}s");
                return Ok(ExitCode::from(3));
            }
            print(&json!({ "messages": m }));
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Import { state, agents, dir } => {
            let state = state.unwrap_or_else(|| home().join(".local/state/agent-bridge"));
            let agents_file =
                agents.unwrap_or_else(|| home().join(".config/agent-bridge/agents.json"));
            let raw = std::fs::read_to_string(&agents_file)
                .with_context(|| format!("reading {}", agents_file.display()))?;
            let map: BTreeMap<String, String> = serde_json::from_str(&raw)?;
            let names: Vec<&str> = map.keys().map(String::as_str).collect();
            let mut bridge = Bridge::open(&state, &names)?;
            let mut refused = 0;
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(&dir)
                .with_context(|| format!("reading {}", dir.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            for d in dirs {
                let name = d
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                // The directory also holds the mirror of tasks that live in the service: those
                // are not file-protocol tasks and are left alone.
                if bridge
                    .messages(&name)
                    .is_ok_and(|ms| ms.first().is_some_and(|m| !m.imported))
                {
                    println!("skipped {name}: a task of the service (mirror files)");
                    continue;
                }
                match agent_bridge::import::read_task(&d)
                    .map_err(|e| format!("{e:#}"))
                    .and_then(|m| {
                        let n = m.len();
                        bridge
                            .import_task(m)
                            .map(|fresh| (fresh, n))
                            .map_err(|e| format!("{e:#}"))
                    }) {
                    Ok((true, n)) => println!("imported {name}: {n} messages"),
                    Ok((false, _)) => println!("unchanged {name}: already imported"),
                    Err(e) => {
                        refused += 1;
                        println!("refused {name}: {e}");
                    }
                }
            }
            Ok(if refused == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Cmd::Mcp => {
            agent_bridge::mcp::serve_stdio(Client::from_env()?).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Serve {
            state,
            listen,
            agents,
            notify,
            mirror,
            step_relay_dir,
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
            let notify_file = notify
                .clone()
                .unwrap_or_else(|| home().join(".config/agent-bridge/notify.json"));
            let targets: BTreeMap<String, agent_bridge::notify::Target> = if notify_file.exists() {
                let raw = std::fs::read_to_string(&notify_file)
                    .with_context(|| format!("reading {}", notify_file.display()))?;
                serde_json::from_str(&raw)
                    .with_context(|| format!("parsing {}", notify_file.display()))?
            } else {
                anyhow::ensure!(notify.is_none(), "{} does not exist", notify_file.display());
                BTreeMap::new()
            };
            for a in targets.keys() {
                anyhow::ensure!(map.contains_key(a), "notify.json names unknown agent {a}");
            }
            eprintln!(
                "agent-bridge: push wake-ups for {:?}",
                targets.keys().collect::<Vec<_>>()
            );
            let mut app = AppState::new(bridge, tokens)?.with_push(targets);
            if let Some(m) = mirror {
                eprintln!("agent-bridge: read-only mirror at {}", m.display());
                app = app.with_mirror(m);
            }
            if let Some(d) = step_relay_dir {
                app = app.with_step_relay(d);
            }
            let app = Arc::new(app);
            // Wait for the start-up mirror sync so drift is known before serving.
            let _ = app
                .sync_mirror()
                .recv_timeout(std::time::Duration::from_secs(60));
            app.spawn_monitor();
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .with_context(|| format!("binding {listen}"))?;
            eprintln!(
                "agent-bridge listening on {listen}, state {}, agents {names:?}",
                state.display()
            );
            service::serve(app, listener).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
