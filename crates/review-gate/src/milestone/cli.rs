//! Command-line sides of the milestone flow (M2 step 6c).
//!
//! - `admin milestone …` (the user, as the reviewgate user): every write is a dry run unless
//!   `--yes` is given. A dry run only reads its arguments, the pending application and the
//!   implementer's repository (read-only git); it loads no key, opens no ledger and creates,
//!   recovers or fetches nothing (Codex n4 §2). `--yes` is only the tool's switch to execute:
//!   the decision behind a waive, resume or revoke is the user's own, never an agent's.
//! - `admin milestone status` is an offline diagnostic: nothing is written, the remote is not
//!   checked, nothing is recovered, and `released` is always false.
//! - `milestone publish` (the implementer): the gate's key is compared with the pinned key
//!   before anything is written locally or pushed.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use ed25519_dalek::VerifyingKey;
use serde_json::{Value, json};

use super::derive::derive;
use super::flow::{Application, Flow, FlowError, PublishOutcome};
use super::ledger::read_only;
use super::record::Record;
use super::serve::MilestoneConfig;
use super::sources::Vendors;
use crate::client::Client;

#[derive(Debug, Clone, clap::Subcommand)]
pub enum AdminOp {
    /// Confirm a pending registration application
    Confirm { phase: String },
    /// Accept a new task book version for a phase (from commit `--base`)
    Rebaseline {
        phase: String,
        #[arg(long)]
        base: String,
        #[arg(long)]
        reason: String,
    },
    /// Waive one open condition
    Waive {
        phase: String,
        #[arg(long)]
        condition: String,
        #[arg(long)]
        reason: String,
    },
    /// Drop one source from the phase's checks
    DropSource {
        phase: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        reason: String,
    },
    /// Link a bridge task to the phase
    LinkTask {
        phase: String,
        #[arg(long)]
        task: String,
        #[arg(long)]
        reason: String,
    },
    /// Ignore a bridge task for the phase
    IgnoreTask {
        phase: String,
        #[arg(long)]
        task: String,
        #[arg(long)]
        reason: String,
    },
    /// Revoke the phase's release
    Revoke {
        phase: String,
        #[arg(long)]
        reason: String,
    },
    /// Resume a paused phase
    Resume {
        phase: String,
        #[arg(long)]
        reason: String,
    },
    /// Replace the vendor table (JSON file with a higher version)
    VendorsSet {
        #[arg(long)]
        file: std::path::PathBuf,
        #[arg(long)]
        reason: String,
    },
}

/// Read-only git in the implementer's repository: `rev-parse` of a commit, or nothing.
fn resolve_commit(repo: &Path, rev: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{rev}^{{commit}}"))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn read_vendors(file: &Path) -> Result<Vendors> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    serde_json::from_str(&text).with_context(|| format!("vendor table {}", file.display()))
}

/// What a write would do, shown without writing (no key, no ledger, nothing created).
pub fn preview(op: &AdminOp, cfg: &MilestoneConfig, state: &Path) -> Result<Value> {
    let note = "dry run: nothing written. Run again with --yes to write; the real seq/hash are shown after writing.";
    let v = match op {
        AdminOp::Confirm { phase } => {
            let path = state
                .join("milestones")
                .join("pending")
                .join(format!("{phase}.json"));
            let application: Option<Application> = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            json!({ "command": "confirm", "phase": phase,
                    "application": application.map_or(json!("none on file"), |a| json!(a)) })
        }
        AdminOp::Rebaseline {
            phase,
            base,
            reason,
        } => json!({ "command": "rebaseline", "phase": phase, "base": base,
                    "resolved": resolve_commit(&cfg.repo, base).unwrap_or_else(|| "not resolved".into()),
                    "reason": reason }),
        AdminOp::Waive {
            phase,
            condition,
            reason,
        } => {
            json!({ "command": "waive", "phase": phase, "condition": condition, "reason": reason })
        }
        AdminOp::DropSource {
            phase,
            source,
            reason,
        } => {
            json!({ "command": "drop-source", "phase": phase, "source": source, "reason": reason })
        }
        AdminOp::LinkTask {
            phase,
            task,
            reason,
        } => json!({ "command": "link-task", "phase": phase, "task": task, "reason": reason }),
        AdminOp::IgnoreTask {
            phase,
            task,
            reason,
        } => json!({ "command": "ignore-task", "phase": phase, "task": task, "reason": reason }),
        AdminOp::Revoke { phase, reason } => {
            json!({ "command": "revoke", "phase": phase, "reason": reason })
        }
        AdminOp::Resume { phase, reason } => {
            json!({ "command": "resume", "phase": phase, "reason": reason })
        }
        AdminOp::VendorsSet { file, reason } => {
            let t = read_vendors(file)?;
            json!({ "command": "vendors-set", "table": t,
                    "table_sha256": t.content_sha256()?, "reason": reason })
        }
    };
    Ok(json!({ "dry_run": true, "would": v, "note": note }))
}

fn written(records: &[Record]) -> Value {
    json!({ "written": records.iter().map(|r| json!({
        "seq": r.seq, "hash": r.hash, "kind": r.kind, "phase": r.phase })).collect::<Vec<_>>() })
}

/// Executes `op` (after `--yes`). Runs on a blocking thread.
pub fn execute(flow: &Flow, op: &AdminOp) -> std::result::Result<Value, FlowError> {
    let one = |r: Record| written(&[r]);
    Ok(match op {
        AdminOp::Confirm { phase } => written(&flow.confirm(phase)?),
        AdminOp::Rebaseline {
            phase,
            base,
            reason,
        } => one(flow.rebaseline(phase, base, reason)?),
        AdminOp::Waive {
            phase,
            condition,
            reason,
        } => one(flow.waive(phase, condition, reason)?),
        AdminOp::DropSource {
            phase,
            source,
            reason,
        } => one(flow.drop_source(phase, source, reason)?),
        AdminOp::LinkTask {
            phase,
            task,
            reason,
        } => one(flow.link_task(phase, task, reason)?),
        AdminOp::IgnoreTask {
            phase,
            task,
            reason,
        } => one(flow.ignore_task(phase, task, reason)?),
        AdminOp::Revoke { phase, reason } => one(flow.revoke(phase, reason)?),
        AdminOp::Resume { phase, reason } => one(flow.resume(phase, reason)?),
        AdminOp::VendorsSet { file, reason } => {
            let t = read_vendors(file).map_err(FlowError::Other)?;
            one(flow.vendors_set(&t, reason)?)
        }
    })
}

/// The offline diagnostic: the log read without writing, verified with the pinned `key`.
pub fn offline_status(
    state: &Path,
    repo_id: &str,
    key: &VerifyingKey,
    phase: &str,
) -> Result<Value> {
    let ro = read_only(&state.join("milestones"), repo_id, key)?;
    let st = derive(&ro.records, phase, ro.checkpoint);
    let last = ro.records.last();
    Ok(json!({
        "offline": true,
        "remote_checked": false,
        "recovered": false,
        "released": false,
        "note": "offline diagnostic: the remote was not checked and nothing was recovered; never a basis for release",
        "truncated_tail": ro.truncated_tail,
        "checkpoint": ro.checkpoint,
        "checkpoint_mismatch": ro.checkpoint_mismatch,
        "seq": last.map_or(0, |r| r.seq),
        "hash": last.map_or("", |r| r.hash.as_str()),
        "state": st,
    }))
}

/// `milestone publish`: pinned-key check first, then the log, the local publish (fast-forward
/// only, no force) and the report. Returns the gate's answer; `archived` false is reported to
/// the caller as a partial success.
pub async fn publish(
    c: &Client,
    repo: &Path,
    remote: &str,
    pinned: &VerifyingKey,
    repo_id: &str,
) -> Result<PublishOutcome> {
    let k: Value = c.get("/pubkey").await?;
    let gate_key = crate::attest::parse_public_key(k["key"].as_str().unwrap_or(""))
        .context("the gate's public key")?;
    if gate_key != *pinned {
        bail!("the gate's key is not the pinned key: refusing to publish");
    }
    let log = c.get_text("/milestones/log").await?;
    let (repo, remote, log2, repo_id2) = (
        repo.to_path_buf(),
        remote.to_string(),
        log.clone(),
        repo_id.to_string(),
    );
    let pinned2 = *pinned;
    let (seq, hash) = tokio::task::spawn_blocking(move || {
        super::sync::publish(&repo, &remote, &log2, &pinned2, &repo_id2)
    })
    .await??;
    c.post("/milestones/published", json!({ "seq": seq, "hash": hash }))
        .await
}
