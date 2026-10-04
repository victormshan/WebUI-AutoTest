//! review-gate command line.
//!
//! Service side (runs as the `reviewgate` system user): `serve`, `admin manual`.
//! Implementer side (talks to the service over HTTP): `task`, `stage`, `review`, `record`.
//! Standalone: `ask` (one question to an external AI), `verify`/`verify-range` (attestations, for CI).

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use review_gate::attest::{self, Keypair};
use review_gate::client::Client;
use review_gate::gate::Gate;
use review_gate::model::{Channel, Task};
use review_gate::relay::RelayTrace;
use review_gate::reviewer::{self, AskError, ReviewerConfig};
use review_gate::service::{self, AppState, Job};
use review_gate::store::Store;
use review_gate::{git, review};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(
    name = "review-gate",
    version,
    about = "Independent review gate for auto-iterate (three-party protocol)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP service (as the reviewgate system user)
    Serve {
        /// Private state directory (0700)
        #[arg(long, default_value = "/var/lib/reviewgate/state")]
        state: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7878")]
        listen: String,
        /// File with the bearer token clients must present
        #[arg(long, default_value = "/etc/review-gate/client.token")]
        token_file: PathBuf,
        /// claude-step-relay data dir: the gate writes `traces/<exprId>.gate.md` there
        #[arg(long)]
        relay_dir: Option<PathBuf>,
    },
    /// Print the gate's attestation public key (pin it in CI)
    Pubkey,
    /// Verify the attestation note on one commit (no service needed)
    Verify {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Base64 ed25519 public key, or a file containing it
        #[arg(long)]
        pubkey: String,
        #[arg(default_value = "HEAD")]
        rev: String,
    },
    /// Verify every commit in a range such as `origin/main..HEAD` (exit 1 if any fails)
    VerifyRange {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        pubkey: String,
        /// Skip merge commits instead of failing them
        #[arg(long)]
        allow_merges: bool,
        range: String,
    },
    /// Manage auto-iterate tasks
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Stage the task's repository (git add -A) and print {tree, base}
    Stage {
        #[arg(long)]
        id: String,
    },
    /// Reviews of the current version
    Review {
        #[command(subcommand)]
        cmd: ReviewCmd,
    },
    /// Apply a review to the task (approved needs --commit and --tag)
    Record {
        #[arg(long)]
        id: String,
        #[arg(long)]
        review: String,
        #[arg(long)]
        commit: Option<String>,
        #[arg(long)]
        tag: Option<String>,
    },
    /// Local administration of the gate's store (run as the reviewgate user)
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
    /// Ask an external AI (another vendor): prompt on stdin, answer on stdout.
    /// Usable as webtest's `--review-command "review-gate ask"`.
    Ask {
        /// auto | gemini-api | openai | web-gemini
        #[arg(long, default_value = "auto")]
        provider: String,
        /// Print {provider, model, family, channel, answer} as JSON
        #[arg(long)]
        json: bool,
        /// Only report which providers are available
        #[arg(long)]
        probe: bool,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    Init {
        #[arg(long)]
        id: String,
        #[arg(long)]
        goal: String,
        #[arg(long)]
        acceptance: String,
        #[arg(long)]
        iterations: u32,
        #[arg(long)]
        repo: PathBuf,
        /// external-api | web-gemini | claude-subagent | self-review | manual
        #[arg(long, default_value = "web-gemini")]
        min_reviewer: String,
        #[arg(long, default_value = "auto")]
        review_provider: String,
    },
    Show {
        #[arg(long)]
        id: String,
    },
    List,
    Link {
        #[arg(long)]
        id: String,
        #[arg(long)]
        expr_id: String,
    },
}

#[derive(Subcommand)]
enum ReviewCmd {
    /// Stage the version and have an external AI review it; waits for the verdict.
    /// Exit: 0 reviewed (see verdict) · 3 no external AI available · 1 failed
    Run {
        #[arg(long)]
        id: String,
        /// Implementer's verification evidence (test output etc.), shown to the reviewer as unverified
        #[arg(long)]
        evidence: Option<PathBuf>,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Submit a review from a weaker channel (claude-subagent | self-review); text needs a VERDICT line
    Submit {
        #[arg(long)]
        id: String,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminCmd {
    /// Record a human verdict (the only way to use the `manual` channel)
    Manual {
        #[arg(long, default_value = "/var/lib/reviewgate/state")]
        state: PathBuf,
        #[arg(long)]
        id: String,
        /// From `review-gate stage --id …` run by the implementer
        #[arg(long)]
        tree: String,
        #[arg(long)]
        base: String,
        /// The human's review; must contain `VERDICT: APPROVED|REJECTED`
        #[arg(long)]
        file: PathBuf,
        /// Same as `serve --relay-dir`, so the verdict also appears in the relay trace
        #[arg(long)]
        relay_dir: Option<PathBuf>,
    },
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

fn print(v: &impl serde::Serialize) {
    println!("{}", serde_json::to_string_pretty(v).expect("serializable"));
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.cmd {
        Cmd::Serve {
            state,
            listen,
            token_file,
            relay_dir,
        } => {
            let token = std::fs::read_to_string(&token_file)
                .with_context(|| format!("reading token {}", token_file.display()))?
                .trim()
                .to_string();
            anyhow::ensure!(
                token.len() >= 32,
                "token in {} is too short (need >= 32 chars)",
                token_file.display()
            );
            let mut gate =
                Gate::new(Store::open(&state)?).with_signer(Keypair::load_or_create(&state)?);
            if let Some(dir) = relay_dir {
                gate = gate.with_relay(RelayTrace::new(dir));
            }
            let app = Arc::new(AppState::new(gate, ReviewerConfig::from_env(), token));
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .with_context(|| format!("binding {listen}"))?;
            eprintln!(
                "review-gate listening on {listen}, state {}",
                state.display()
            );
            service::serve(app, listener).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Task { cmd } => {
            let c = Client::from_env()?;
            let v: Value = match cmd {
                TaskCmd::Init {
                    id,
                    goal,
                    acceptance,
                    iterations,
                    repo,
                    min_reviewer,
                    review_provider,
                } => {
                    let repo = std::fs::canonicalize(&repo)
                        .with_context(|| format!("repo {}", repo.display()))?;
                    c.post(
                        "/tasks",
                        json!({ "id": id, "goal": goal, "acceptance": acceptance, "iterations": iterations,
                                "repo": repo, "min_reviewer": min_reviewer, "review_provider": review_provider }),
                    )
                    .await?
                }
                TaskCmd::Show { id } => c.get(&format!("/tasks/{id}")).await?,
                TaskCmd::List => c.get("/tasks").await?,
                TaskCmd::Link { id, expr_id } => {
                    c.post(&format!("/tasks/{id}/link"), json!({ "expr_id": expr_id }))
                        .await?
                }
            };
            print(&v);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Stage { id } => {
            let c = Client::from_env()?;
            let (tree, base) = stage(&c, &id).await?;
            print(&json!({ "tree": tree, "base": base }));
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Review {
            cmd:
                ReviewCmd::Run {
                    id,
                    evidence,
                    provider,
                },
        } => {
            let c = Client::from_env()?;
            let (tree, base) = stage(&c, &id).await?;
            let evidence = match evidence {
                Some(p) => std::fs::read_to_string(&p)
                    .with_context(|| format!("reading {}", p.display()))?,
                None => String::new(),
            };
            let job: Job = c
                .review(
                    &id,
                    &tree,
                    &base,
                    &evidence,
                    provider.as_deref(),
                    Duration::from_secs(5),
                )
                .await?;
            print(&job);
            Ok(match (job.status.as_str(), job.unavailable) {
                ("done", _) => ExitCode::SUCCESS,
                (_, true) => ExitCode::from(3),
                _ => ExitCode::FAILURE,
            })
        }
        Cmd::Review {
            cmd: ReviewCmd::Submit { id, channel, file },
        } => {
            let c = Client::from_env()?;
            let (tree, base) = stage(&c, &id).await?;
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let rec: Value = c
                .post(
                    &format!("/tasks/{id}/reviews/submit"),
                    json!({ "tree": tree, "base": base, "channel": channel, "text": text }),
                )
                .await?;
            print(&rec);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Record {
            id,
            review,
            commit,
            tag,
        } => {
            let c = Client::from_env()?;
            let v: Value = c
                .post(
                    &format!("/tasks/{id}/record"),
                    json!({ "review": review, "commit": commit, "tag": tag }),
                )
                .await?;
            // Attach the gate's signed attestation to the commit for CI to verify.
            if let (Some(note), Some(commit)) = (
                v["attestation"].as_str(),
                v["task"]["history"]
                    .as_array()
                    .and_then(|h| h.last())
                    .and_then(|h| h["commit"].as_str()),
            ) {
                let repo = v["task"]["repo"].as_str().unwrap_or(".");
                git::git(
                    std::path::Path::new(repo),
                    &[
                        "notes",
                        &format!("--ref={}", attest::NOTES_REF),
                        "add",
                        "-f",
                        "-m",
                        note,
                        commit,
                    ],
                )
                .context("attaching the attestation note")?;
                eprintln!(
                    "attestation attached as git note ({}) on {commit}; push it with: git push origin {}",
                    attest::NOTES_REF,
                    attest::NOTES_REF
                );
            }
            print(&v);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Pubkey => {
            let v: Value = Client::from_env()?.get("/pubkey").await?;
            println!("{}", v["key"].as_str().unwrap_or_default());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Verify { repo, pubkey, rev } => {
            let key = load_pubkey(&pubkey)?;
            match attest::verify_commit(&repo, &rev, &key) {
                Ok(st) => {
                    println!(
                        "ok {} task {} version {} review {} by {}",
                        st.commit, st.task, st.version, st.review, st.reviewer
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("FAIL {e:#}");
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        Cmd::VerifyRange {
            repo,
            pubkey,
            allow_merges,
            range,
        } => {
            let key = load_pubkey(&pubkey)?;
            let (ok, bad) = attest::verify_range(&repo, &range, &key, allow_merges)?;
            for st in &ok {
                println!(
                    "ok {} task {} version {} review {} by {}",
                    st.commit, st.task, st.version, st.review, st.reviewer
                );
            }
            for e in &bad {
                println!("FAIL {e}");
            }
            println!("{} verified, {} failed", ok.len(), bad.len());
            Ok(if bad.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Cmd::Admin {
            cmd:
                AdminCmd::Manual {
                    state,
                    id,
                    tree,
                    base,
                    file,
                    relay_dir,
                },
        } => {
            let mut gate = Gate::new(Store::open(&state)?);
            if let Some(dir) = relay_dir {
                gate = gate.with_relay(RelayTrace::new(dir));
            }
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            print(&review::submit(
                &gate,
                &id,
                &tree,
                &base,
                Channel::Manual,
                &text,
            )?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Ask {
            provider,
            json,
            probe,
        } => Ok(ask(&provider, json, probe).await),
    }
}

fn load_pubkey(arg: &str) -> Result<ed25519_dalek::VerifyingKey> {
    let p = std::path::Path::new(arg);
    let text = if p.is_file() {
        std::fs::read_to_string(p)?
    } else {
        arg.to_string()
    };
    attest::parse_public_key(&text)
}

/// Stages the task's repository locally (the implementer owns it) and returns (tree, base).
async fn stage(c: &Client, id: &str) -> Result<(String, String)> {
    let task: Task = c.get(&format!("/tasks/{id}")).await?;
    Ok(git::stage_all(std::path::Path::new(&task.repo))?)
}

/// Exit codes: 0 answered · 3 no external AI available · 1 failed · 2 usage error.
async fn ask(provider: &str, json: bool, probe: bool) -> ExitCode {
    let cfg = ReviewerConfig::from_env();
    if probe {
        let map: serde_json::Map<String, Value> = reviewer::probe(&cfg)
            .await
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.into()))
            .collect();
        println!("{}", Value::Object(map));
        return ExitCode::SUCCESS;
    }
    let mut prompt = String::new();
    if std::io::stdin().read_to_string(&mut prompt).is_err() || prompt.trim().is_empty() {
        eprintln!("empty prompt on stdin");
        return ExitCode::from(2);
    }
    match reviewer::ask(&cfg, prompt.trim(), provider).await {
        Ok(r) if json => {
            println!("{}", serde_json::to_string(&r).expect("serializable"));
            ExitCode::SUCCESS
        }
        Ok(r) => {
            println!("{}", r.answer);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(if matches!(e, AskError::Unavailable(_)) {
                3
            } else {
                1
            })
        }
    }
}
