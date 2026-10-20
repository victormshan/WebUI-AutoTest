//! review-gate command line.
//!
//! Service side (runs as the `reviewgate` system user): `serve`, `admin manual`.
//! Implementer side (talks to the service over HTTP): `task`, `stage`, `review`, `record`, `mcp`.
//! Standalone: `ask` (one question to an external AI), `verify`/`verify-range` (attestations, for CI).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use review_gate::attest::{self, Keypair};
use review_gate::client::Client;
use review_gate::gate::Gate;
use review_gate::identity::{DEFAULT_CONFIG, Identity, IdentityConfig};
use review_gate::milestone::cli as ms_cli;
use review_gate::milestone::flow::Flow;
use review_gate::milestone::serve::{self as ms_serve, MilestoneConfig};
use review_gate::model::Channel;
use review_gate::relay::RelayTrace;
use review_gate::reviewer::{self, AskError, ReviewerConfig};
use review_gate::service::{self, AppState, Job};
use review_gate::store::Store;
use review_gate::{hook, mcp, review};
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
        /// File with the legacy bearer token clients may present (only when no identity
        /// configuration is used). It remains a plaintext credential file.
        #[arg(long, default_value = "/etc/review-gate/client.token")]
        token_file: PathBuf,
        /// Administrator-managed identities: credential hashes → principal and canonical
        /// family. Defaults to /etc/review-gate/identity.json when that file exists; if it
        /// exists but does not load, the service refuses to start (no legacy fallback).
        #[arg(long)]
        identity_config: Option<PathBuf>,
        /// claude-step-relay data dir: the gate writes `traces/<exprId>.gate.md` there
        #[arg(long)]
        relay_dir: Option<PathBuf>,
        /// Milestone audits: JSON configuration (absent: milestone routes answer 404)
        #[arg(long)]
        milestones: Option<PathBuf>,
    },
    /// Milestone audits, as the implementer (talks to the service)
    Milestone {
        #[command(subcommand)]
        cmd: MilestoneCmd,
    },
    /// Claude Code PreToolUse hook (Bash): blocks unreviewed `git commit` and unattested `git push`
    Hook,
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
    /// MCP server on stdio for Claude Code (forwards to the service with the implementer's token)
    Mcp,
    /// Ask an external AI (another vendor): prompt on stdin, answer on stdout.
    /// Usable as webtest's `--review-command "review-gate ask"`.
    Ask {
        /// auto | gemini-api | openai | web-gemini
        #[arg(long, default_value = "auto")]
        provider: String,
        /// The implementer's model family; providers from the same family are skipped/refused
        #[arg(long, default_value = "claude")]
        implementer_family: String,
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
        /// Self-reported implementer family; only checked against the authenticated identity
        /// (a mismatch is refused), never trusted as the frozen value.
        #[arg(long)]
        implementer_family: Option<String>,
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
enum MilestoneCmd {
    /// Apply to register a phase (the user confirms it with `admin milestone confirm`)
    Register {
        #[arg(long)]
        phase: String,
        #[arg(long)]
        base: String,
        #[arg(long)]
        task_book: String,
        #[arg(long)]
        evidence_dir: String,
        #[arg(long = "condition")]
        conditions: Vec<String>,
        #[arg(long)]
        supersedes: Option<String>,
    },
    /// Request an audit of `base..head`
    Request {
        #[arg(long)]
        phase: String,
        #[arg(long)]
        head: String,
    },
    /// The phase's state (exit 1 unless released)
    Status {
        #[arg(long)]
        phase: String,
    },
    /// Cancel a pending request
    Cancel {
        #[arg(long)]
        phase: String,
        #[arg(long)]
        id: String,
    },
    /// Publish the signed log to the remote's notes (no force) and report it (exit 1 unless archived)
    Publish {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        remote: String,
        /// The gate's pinned public key (base64, or a file)
        #[arg(long)]
        pubkey: String,
        #[arg(long)]
        repo_id: String,
    },
}

#[derive(Subcommand)]
enum AdminMilestoneCmd {
    /// Offline diagnostic: reads the log without writing; never a basis for release
    Status { phase: String },
    #[command(flatten)]
    Op(review_gate::milestone::cli::AdminOp),
}

#[derive(Subcommand)]
enum AdminCmd {
    /// Milestone administration (the user). Writes are dry runs unless --yes is given.
    Milestone {
        #[arg(long, default_value = "/var/lib/reviewgate/state")]
        state: PathBuf,
        /// The same JSON configuration as `serve --milestones`
        #[arg(long)]
        config: PathBuf,
        /// Execute the write (without it, only show what would be written)
        #[arg(long)]
        yes: bool,
        /// The gate's pinned public key (base64, or a file): needed by `status`, and by a
        /// `rebaseline` dry run to show the task book it would accept
        #[arg(long)]
        pubkey: Option<String>,
        #[command(subcommand)]
        cmd: AdminMilestoneCmd,
    },
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

/// Whether the administrator identity configuration must be loaded from `path`.
///
/// Only a genuinely absent file permits the explicit legacy token. `Path::exists()` would report
/// `false` for permission or other metadata errors and silently fall back to the plaintext token,
/// so every other error refuses to start.
fn default_config_present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::anyhow!(
            "cannot inspect identity configuration {}: {e}",
            path.display()
        )),
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.cmd {
        Cmd::Serve {
            state,
            listen,
            token_file,
            identity_config,
            relay_dir,
            milestones,
        } => {
            // A configured identity map is authoritative. It exists but fails to load → refuse
            // to start; never fall back to the legacy plaintext token.
            let identities = match identity_config {
                Some(path) => IdentityConfig::load(&path)?,
                None if default_config_present(Path::new(DEFAULT_CONFIG))? => {
                    IdentityConfig::load(Path::new(DEFAULT_CONFIG))?
                }
                None => {
                    let token = std::fs::read_to_string(&token_file)
                        .with_context(|| format!("reading token {}", token_file.display()))?
                        .trim()
                        .to_string();
                    anyhow::ensure!(
                        token.len() >= 32,
                        "token in {} is too short (need >= 32 chars)",
                        token_file.display()
                    );
                    IdentityConfig::legacy(token)
                }
            };
            let mut gate =
                Gate::new(Store::open(&state)?).with_signer(Keypair::load_or_create(&state)?);
            if let Some(dir) = relay_dir {
                gate = gate.with_relay(RelayTrace::new(dir));
            }
            let mut app = AppState::with_identities(gate, ReviewerConfig::from_env(), identities);
            let mut watched = None;
            if let Some(path) = milestones {
                let mc = MilestoneConfig::load(&path)?;
                let bridge_token = ms_serve::read_token(&mc.bridge_token_file)?;
                let kp = Keypair::load_or_create(&state)?;
                let cfg = mc.flow_config(&state, kp.verifying_key());
                let bridge = ms_serve::bridge_reader(
                    tokio::runtime::Handle::current(),
                    mc.bridge_url.clone(),
                    bridge_token,
                );
                let flow = tokio::task::spawn_blocking(move || Flow::open_service(cfg, kp, bridge))
                    .await??;
                let (tx, rx) = tokio::sync::oneshot::channel();
                ms_serve::watch_halt(flow.clone(), tx);
                #[cfg(feature = "fault-injection")]
                ms_serve::fault::start(&flow, &mc.runner_socket);
                app = app.with_milestones(flow.clone());
                watched = Some((flow, rx));
            }
            let app = Arc::new(app);
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .with_context(|| format!("binding {listen}"))?;
            eprintln!(
                "review-gate listening on {listen}, state {}",
                state.display()
            );
            let Some((flow, rx)) = watched else {
                service::serve(app, listener).await?;
                return Ok(ExitCode::SUCCESS);
            };
            let served = service::serve_until(app, listener, rx).await;
            // Once halted, the process never ends normally: exit 3 now (the watchdog does it
            // at the deadline if this point is never reached).
            if flow.halted().is_some() {
                std::process::exit(ms_serve::HALT_EXIT);
            }
            served?;
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
                    implementer_family,
                } => {
                    let repo = std::fs::canonicalize(&repo)
                        .with_context(|| format!("repo {}", repo.display()))?;
                    c.post(
                        "/tasks",
                        json!({ "id": id, "goal": goal, "acceptance": acceptance, "iterations": iterations,
                                "repo": repo, "min_reviewer": min_reviewer,
                                "review_provider": review_provider,
                                "implementer_family": implementer_family }),
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
            let (tree, base) = c.stage(&id).await?;
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
            let (tree, base) = c.stage(&id).await?;
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
            let (tree, base) = c.stage(&id).await?;
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
            let v = c
                .record(&id, &review, commit.as_deref(), tag.as_deref())
                .await?;
            if let Some(n) = v["note"].as_str() {
                eprintln!("{n}");
            }
            print(&v);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Mcp => {
            mcp::serve_stdio(Client::from_env()?).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Hook => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let input: Value = serde_json::from_str(&input).context("hook input is not JSON")?;
            let client = Client::from_env().ok();
            Ok(match hook::decide(&input, client.as_ref()).await {
                None => ExitCode::SUCCESS,
                Some(reason) => {
                    eprintln!("{reason}");
                    ExitCode::from(2)
                }
            })
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
                &Identity::admin(),
                &id,
                &tree,
                &base,
                Channel::Manual,
                &text,
            )?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Admin {
            cmd:
                AdminCmd::Milestone {
                    state,
                    config,
                    yes,
                    pubkey,
                    cmd,
                },
        } => admin_milestone(state, config, yes, pubkey, cmd).await,
        Cmd::Milestone { cmd } => milestone(cmd).await,
        Cmd::Ask {
            provider,
            implementer_family,
            json,
            probe,
        } => Ok(ask(&provider, &implementer_family, json, probe).await),
    }
}

async fn admin_milestone(
    state: PathBuf,
    config: PathBuf,
    yes: bool,
    pubkey: Option<String>,
    cmd: AdminMilestoneCmd,
) -> Result<ExitCode> {
    let mc = MilestoneConfig::load(&config)?;
    let key = pubkey.as_deref().map(load_pubkey).transpose()?;
    let op = match cmd {
        AdminMilestoneCmd::Status { phase } => {
            let key = key.context("status needs --pubkey (the gate's pinned public key)")?;
            print(&ms_cli::offline_status(&state, &mc.repo_id, &key, &phase)?);
            return Ok(ExitCode::SUCCESS);
        }
        AdminMilestoneCmd::Op(op) => op,
    };
    if !yes {
        print(&ms_cli::preview(&op, &mc, &state, key.as_ref())?);
        return Ok(ExitCode::SUCCESS);
    }
    anyhow::ensure!(
        state.join("keys").join("gate.ed25519").is_file(),
        "{} holds no gate key: wrong --state?",
        state.display()
    );
    let bridge_token = ms_serve::read_token(&mc.bridge_token_file)?;
    let kp = Keypair::load_or_create(&state)?;
    let cfg = mc.flow_config(&state, kp.verifying_key());
    let bridge = ms_serve::bridge_reader(
        tokio::runtime::Handle::current(),
        mc.bridge_url.clone(),
        bridge_token,
    );
    let out = tokio::task::spawn_blocking(move || {
        let flow = Flow::open_admin(cfg, kp, bridge)?;
        ms_cli::execute(&flow, &op).map_err(anyhow::Error::from)
    })
    .await??;
    print(&out);
    Ok(ExitCode::SUCCESS)
}

async fn milestone(cmd: MilestoneCmd) -> Result<ExitCode> {
    let c = Client::from_env()?;
    match cmd {
        MilestoneCmd::Register {
            phase,
            base,
            task_book,
            evidence_dir,
            conditions,
            supersedes,
        } => {
            let v: Value = c
                .post(
                    "/milestones",
                    json!({ "phase": phase, "base": base, "task_book": task_book,
                        "evidence_dir": evidence_dir, "conditions": conditions,
                        "supersedes": supersedes }),
                )
                .await?;
            print(&v);
        }
        MilestoneCmd::Request { phase, head } => {
            let v: Value = c
                .post(
                    &format!("/milestones/{phase}/requests"),
                    json!({ "head": head }),
                )
                .await?;
            print(&v);
        }
        MilestoneCmd::Status { phase } => {
            let v: Value = c.get(&format!("/milestones/{phase}")).await?;
            print(&v);
            if v["released"] != json!(true) {
                return Ok(ExitCode::from(1));
            }
        }
        MilestoneCmd::Cancel { phase, id } => {
            let v: Value = c
                .post(
                    &format!("/milestones/{phase}/requests/{id}/cancel"),
                    json!({}),
                )
                .await?;
            print(&v);
        }
        MilestoneCmd::Publish {
            repo,
            remote,
            pubkey,
            repo_id,
        } => {
            let key = load_pubkey(&pubkey)?;
            let o = ms_cli::publish(&c, &repo, &remote, &key, &repo_id).await?;
            print(&o);
            if !o.archived {
                eprintln!(
                    "published, but the gate has not confirmed the archive yet; do not push again: \
                     the next status retries the archive"
                );
                return Ok(ExitCode::from(1));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
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

/// Exit codes: 0 answered · 3 no external AI available · 1 failed · 2 usage error.
async fn ask(provider: &str, implementer_family: &str, json: bool, probe: bool) -> ExitCode {
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
    match reviewer::ask(&cfg, prompt.trim(), provider, implementer_family).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn an_absent_default_config_is_the_only_state_that_allows_legacy() {
        let d = tempfile::tempdir().unwrap();
        let absent = d.path().join("identity.json");
        assert!(!default_config_present(&absent).unwrap());

        fs::write(&absent, "{}").unwrap();
        assert!(default_config_present(&absent).unwrap());

        // A path error that is not "not found" (here ENOTDIR) must not look like an absent file.
        let file = d.path().join("not-a-dir");
        fs::write(&file, "x").unwrap();
        let err = default_config_present(&file.join("identity.json")).unwrap_err();
        assert!(err.to_string().contains("cannot inspect"), "{err}");
    }
}
