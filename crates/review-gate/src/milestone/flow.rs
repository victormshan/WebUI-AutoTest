//! The milestone request flow inside the gate (plans/M2-design.md §3–§5; the 6b plan agreed with
//! Codex in bridge task m2-step6b-plan-20261018). The interfaces (HTTP, admin commands, CLI) are
//! step 6c; this module is what they call.
//!
//! Every operation follows one order:
//! 1. **sync**: fetch the remote into the private cache and import what it has beyond the local
//!    log, classified again under the lock ([`Ledger::import`]);
//! 2. **recover**: complete any group a crash cut short ([`Flow::recover`]): confirm groups
//!    first, then job results;
//! 3. **prepare** outside the lock: git, the bridge, the input;
//! 4. **commit** with [`Ledger::transact`]: under the lock the preconditions are checked again on
//!    the log as it is, and the whole group is appended or nothing is. A request also needs the
//!    log tail it was prepared on; if anything was written since, it is prepared again (at most
//!    [`ATTEMPTS`] times).
//!
//! A request is refused up front, with every reason, when the implementation sources are unknown
//! or the auditor is not independent (§3.8): nothing is recorded and no audit is spent.
//!
//! Only the service ([`Flow::open_service`]) dispatches jobs. It holds `owner.lock` for its whole
//! life, so there is never a second one, and only it, once at start, closes a Pending request that
//! has no job as failed: holding the lock proves the previous owner is gone. Admin commands and
//! status never do that; they only complete groups from records already written, which is
//! idempotent.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use agent_bridge::observe::Collected;
use anyhow::{Context, Result, bail};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::answer::{self, Parsed};
use super::derive::{MALFORMED, MAX_SUPERSESSION, PhaseState, RequestState, THRESHOLD, derive};
use super::input::{self, Isolated, Material, Trusted};
use super::ledger::{Ledger, Snapshot, Tx, TxError};
use super::record::{Draft, Record, render_log, sha256_hex};
use super::runner::{self, Expected};
use super::sources::{self, Attestation, CommitCheck, Vendors};
use super::sync::{self, Sync};
use crate::attest::{self, Keypair};

/// Prepare-and-commit rounds of a request before it gives up on a log that keeps changing.
pub const ATTEMPTS: usize = 3;
/// The runner reads one request line of at most its input limit plus this (audit-runner
/// src/lib.rs, `MAX_INPUT + 64 * 1024`); `runner::call` adds the newline.
pub const RUNNER_LINE_SLACK: usize = 64 * 1024;

/// One bridge read as the gate's observer (sources::read_bridge in production).
pub type BridgeRead =
    Arc<dyn Fn() -> std::result::Result<Collected, String> + Send + std::marker::Sync>;

pub struct Config {
    /// `state/milestones/`.
    pub dir: PathBuf,
    pub repo_id: String,
    /// The implementer's repository (read only, through `input::locate` and `Isolated`).
    pub repo: PathBuf,
    /// The fixed public repository whose notes hold the published log (§2).
    pub remote_url: String,
    pub runner_socket: PathBuf,
    pub runner: Expected,
    pub job_timeout: Duration,
    pub auditor: String,
    pub implementers: BTreeSet<String>,
    /// Key that commit attestations must verify with (the gate's own key in production).
    pub attestation_key: VerifyingKey,
}

/// An implementer's application to register a phase (§3.1), kept until the user confirms it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Application {
    pub phase: String,
    pub base: String,
    pub task_book: String,
    /// Computed by the gate from `base`, never taken from the implementer.
    pub task_book_sha256: String,
    pub evidence_dir: String,
    pub conditions: Vec<String>,
    pub supersedes: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    /// Refused, with every reason; nothing was written.
    #[error("refused: {}", .0.join("; "))]
    Refused(Vec<String>),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<std::io::Error> for FlowError {
    fn from(e: std::io::Error) -> Self {
        FlowError::Other(e.into())
    }
}

impl From<TxError> for FlowError {
    fn from(e: TxError) -> Self {
        match e {
            TxError::Refused(why) => FlowError::Refused(vec![why]),
            TxError::Stopped(why) => FlowError::Refused(vec![format!("log stopped: {why}")]),
            TxError::Other(e) => FlowError::Other(e),
        }
    }
}

fn refused(why: impl Into<String>) -> FlowError {
    FlowError::Refused(vec![why.into()])
}

/// Coordination points for tests: called at fixed places so a test can make a halt happen at
/// exactly that moment.
#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    /// Job thread: after the snapshot checks, before taking the admission lock to send.
    before_send: Option<Arc<dyn Fn() + Send + std::marker::Sync>>,
    /// Job thread: holding the admission lock, just before writing the request line.
    while_sending: Option<Arc<dyn Fn() + Send + std::marker::Sync>>,
    /// Request: holding the admission lock, after the final `halted` check, before the commit.
    before_commit: Option<Arc<dyn Fn() + Send + std::marker::Sync>>,
    /// Status: the state is derived, the result not yet returned.
    in_status: Option<Arc<dyn Fn() + Send + std::marker::Sync>>,
    /// Any milestone HTTP route: the call has returned, the response is not yet built.
    in_call: Option<Arc<dyn Fn() + Send + std::marker::Sync>>,
    /// wait_halted: holding the latch's lock, about to wait on it.
    waiting: Option<Arc<dyn Fn() + Send + std::marker::Sync>>,
    /// This many next fetches of the remote fail.
    failed_fetches: std::sync::atomic::AtomicUsize,
}

/// The result of `POST /milestones/published`: the `published` record is written; `archived`
/// says whether the remote is proven to hold that position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishOutcome {
    pub published: bool,
    pub archived: bool,
    /// The checkpoint after the attempt.
    pub checkpoint: u64,
    pub archive_error: Option<String>,
}

/// An error text for a client: one line, bounded.
fn short(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(300).collect()
}

/// What became of sending one job (under the admission lock).
enum Sending {
    /// The service had halted: not sent, nothing recorded.
    Halted(String),
    /// Not sent, for a reason to record as the job's error.
    Failed(String),
    Sent(std::os::unix::net::UnixStream),
}

/// Status of a phase: the derived state on one synced, recovered snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub state: PhaseState,
    pub sync: String,
    /// Tail of the snapshot the state was derived from.
    pub seq: u64,
    pub hash: String,
    pub released: bool,
    pub reasons: Vec<String>,
}

pub struct Flow {
    cfg: Config,
    ledger: Ledger,
    kp: Keypair,
    bridge: BridgeRead,
    /// One lock per phase in this process (requests, results and admin steps of one phase).
    phases: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    /// Request ids whose job is running in this process (service only).
    running: Mutex<BTreeSet<String>>,
    /// Set by a result that could not be written: no more dispatching or release.
    halted: Mutex<Option<String>>,
    /// Notified (under `halted`) when the latch is set; see `wait_halted`.
    halted_cv: Condvar,
    /// Admission of new jobs (Codex review of 83bb705): halting takes it, and a request's final
    /// commit and its dispatch happen under it, so no job is admitted once the service has
    /// halted, whatever was being prepared meanwhile. Held only for that short commit and the
    /// thread spawn, never across git, the bridge or the runner.
    admit: Mutex<()>,
    #[cfg(test)]
    hooks: Mutex<TestHooks>,
    service: bool,
    _owner: Option<File>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// audit-runner's `valid_job_id`: request ids become its job directories.
pub fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn draft(kind: &str, phase: &str, body: Value) -> Draft {
    Draft {
        kind: kind.into(),
        phase: phase.into(),
        body,
    }
}

/// A body without the keys recovery adds, for comparing what is on file with what was due.
fn plain(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("recovered");
    }
    v
}

/// The latest `request` record of `phase`.
fn current_request<'a>(records: &'a [Record], phase: &str) -> Option<&'a Record> {
    records
        .iter()
        .rev()
        .find(|r| r.phase == phase && r.kind == "request")
}

fn find<'a>(
    records: &'a [Record],
    phase: &str,
    kind: &str,
    request_id: &str,
) -> Option<&'a Record> {
    records
        .iter()
        .find(|r| r.phase == phase && r.kind == kind && s(&r.body, "request_id") == request_id)
}

/// The vendor table in force: the last `vendors_changed`, else the first table.
fn vendors(records: &[Record]) -> Result<Vendors> {
    match records.iter().rev().find(|r| r.kind == "vendors_changed") {
        None => Ok(Vendors::first()),
        Some(r) => serde_json::from_value(r.body.get("table").cloned().unwrap_or(Value::Null))
            .with_context(|| format!("record {}: vendor table unreadable", r.seq)),
    }
}

/// What a job produced, as its record body: the runner's reply as is, or why there is none.
pub fn job_body(request_id: &str, reply: std::result::Result<runner::Reply, String>) -> Value {
    match reply {
        Err(e) => json!({ "request_id": request_id, "error": e }),
        Ok(r) => match serde_json::from_str::<Value>(&r.line) {
            Ok(resp) => json!({
                "request_id": request_id,
                "response": resp,
                "sender": { "pid": r.pid, "uid": r.uid, "cgroup": r.cgroup },
            }),
            Err(e) => json!({
                "request_id": request_id,
                "error": format!("reply is not JSON: {e}"),
                "sender": { "pid": r.pid, "uid": r.uid, "cgroup": r.cgroup },
            }),
        },
    }
}

/// The records a job's result calls for after the job record itself (the recovery table of the
/// 6b plan): `audit` (when the runner completed), then one `condition_resolved` per resolution
/// of a valid audit, then `request_closed`. `before` is the chain before the request (it gives
/// the conditions open when the request was made); `audit_seq` is the seq the audit has or will
/// have. Decided by the job body and the request record alone.
pub fn rest_after_job(
    phase: &str,
    request: &Record,
    job: &Value,
    before: &[Record],
    audit_seq: u64,
) -> Vec<Draft> {
    let id = s(&request.body, "request_id");
    let closed = |state: &str, why: &str| {
        draft(
            "request_closed",
            phase,
            json!({ "request_id": id, "state": state, "why": why }),
        )
    };
    if let Some(e) = job.get("error").and_then(Value::as_str) {
        return vec![closed("failed", e)];
    }
    let resp = job.get("response").cloned().unwrap_or(Value::Null);
    match s(&resp, "status") {
        "completed" => {}
        "timed_out" => return vec![closed("timed_out", "the runner timed out")],
        other => {
            return vec![closed(
                "failed",
                &format!("runner status {other:?}: {}", s(&resp, "error")),
            )];
        }
    }
    let invalid = |reason: String| {
        vec![
            draft(
                "audit",
                phase,
                json!({ "request_id": id, "valid": false, "reason": reason }),
            ),
            closed("failed", "no valid audit"),
        ]
    };
    let want_sha = request
        .body
        .pointer("/snapshot/input_sha256")
        .and_then(Value::as_str)
        .unwrap_or("");
    if s(&resp, "job_id") != id {
        return invalid(format!(
            "job_id {:?} is not the request {id:?}",
            s(&resp, "job_id")
        ));
    }
    if s(&resp, "input_sha256") != want_sha || want_sha.is_empty() {
        return invalid(format!(
            "input_sha256 {} is not the sealed input {want_sha}",
            s(&resp, "input_sha256")
        ));
    }
    let open: BTreeSet<String> = derive(before, phase, 0)
        .conditions
        .into_iter()
        .filter(|c| c.closed.is_none())
        .map(|c| c.id)
        .collect();
    match answer::parse(s(&resp, "answer"), &open) {
        Parsed::Invalid { reason } => invalid(reason),
        Parsed::Valid {
            verdict,
            conditions,
            resolved,
        } => {
            let conds: Vec<Value> = conditions
                .iter()
                .enumerate()
                .map(|(k, c)| {
                    json!({ "id": format!("{phase}/A{audit_seq}-{}", k + 1),
                            "blocking": c.blocking, "text": c.text })
                })
                .collect();
            let res: Vec<Value> = resolved
                .iter()
                .map(|r| json!({ "condition": r.condition, "basis": r.basis }))
                .collect();
            let mut out = vec![draft(
                "audit",
                phase,
                json!({ "request_id": id, "valid": true, "verdict": verdict,
                        "conditions": conds, "resolved": res }),
            )];
            for r in &resolved {
                out.push(draft(
                    "condition_resolved",
                    phase,
                    json!({ "condition": r.condition, "audit_seq": audit_seq, "basis": r.basis }),
                ));
            }
            out.push(closed("completed", ""));
            out
        }
    }
}

/// What recovery must append so every group cut short by a crash is complete (confirm groups
/// first, then job results), or why it cannot: a record on file that is not what was due means
/// the log is inconsistent and needs the user.
pub fn recovery(records: &[Record]) -> std::result::Result<Vec<Draft>, String> {
    let mut out = Vec::new();
    // Confirm groups: a registration that supersedes O, then O's phase_superseded, then the
    // cancellation of O's request that was Pending at the registration.
    for reg in records.iter().filter(|r| r.kind == "phase_registered") {
        let Some(old) = reg.body.get("supersedes").and_then(Value::as_str) else {
            continue;
        };
        let new = reg.phase.as_str();
        let after = &records[records.partition_point(|x| x.seq <= reg.seq)..];
        if !after
            .iter()
            .any(|r| r.phase == old && r.kind == "phase_superseded")
        {
            out.push(draft("phase_superseded", old, json!({ "by": new })));
        }
        let before = &records[..records.partition_point(|x| x.seq < reg.seq)];
        let st = derive(before, old, 0);
        if let Some(c) = st.current.filter(|c| c.state == RequestState::Pending)
            && find(after, old, "request_closed", &c.request_id).is_none()
        {
            out.push(draft(
                "request_closed",
                old,
                json!({ "request_id": c.request_id, "state": "cancelled",
                        "why": format!("superseded by {new}") }),
            ));
        }
    }
    if !out.is_empty() {
        return Ok(out);
    }
    // Job results: the current, Pending request of a phase not superseded, with a job.
    let phases: BTreeSet<&str> = records
        .iter()
        .filter(|r| r.kind == "request")
        .map(|r| r.phase.as_str())
        .collect();
    let mut next_seq = records.len() as u64 + 1;
    for phase in phases {
        let st = derive(records, phase, 0);
        let Some(c) = st.current.as_ref() else {
            continue;
        };
        if c.state != RequestState::Pending || st.superseded_by.is_some() {
            continue;
        }
        let Some(job) = find(records, phase, "job", &c.request_id) else {
            continue;
        };
        let request = &records[c.seq as usize - 1];
        let before = &records[..(c.seq - 1) as usize];
        let audit = find(records, phase, "audit", &c.request_id);
        let audit_seq = audit.map_or(next_seq, |a| a.seq);
        let due = rest_after_job(phase, request, &job.body, before, audit_seq);
        // What is on file after the job, for this request, in order.
        let on_file: Vec<&Record> = records
            .iter()
            .filter(|r| r.phase == phase && r.seq > job.seq)
            .filter(|r| match r.kind.as_str() {
                "audit" | "request_closed" => s(&r.body, "request_id") == c.request_id,
                // Without an audit on file, any resolution after the job is out of place: it is
                // kept here so the comparison below refuses it.
                "condition_resolved" => match audit {
                    Some(a) => r.body.get("audit_seq").and_then(Value::as_u64) == Some(a.seq),
                    None => true,
                },
                _ => false,
            })
            .collect();
        if on_file.len() > due.len() {
            return Err(format!(
                "recovery mismatch: request {} has more records than its result calls for",
                c.request_id
            ));
        }
        for (have, want) in on_file.iter().zip(&due) {
            if have.kind != want.kind || plain(&have.body) != plain(&want.body) {
                return Err(format!(
                    "recovery mismatch at seq {}: {} on file is not what the job result calls for",
                    have.seq, have.kind
                ));
            }
        }
        for mut d in due.into_iter().skip(on_file.len()) {
            if let Some(o) = d.body.as_object_mut() {
                o.insert("recovered".into(), Value::Bool(true));
            }
            out.push(d);
            next_seq += 1;
        }
    }
    Ok(out)
}

/// The phase-level preconditions of a new request, on one snapshot.
struct Ready {
    st: PhaseState,
    base: String,
    baseline_commit: String,
    task_book: String,
    task_book_sha256: String,
    evidence_dir: String,
    previous_head: Option<String>,
}

fn ready(records: &[Record], phase: &str) -> std::result::Result<Ready, Vec<String>> {
    let st = derive(records, phase, 0);
    let mut why = Vec::new();
    if !st.registered {
        why.push(format!(
            "phase {phase} is not registered (or not confirmed)"
        ));
    }
    if let Some(by) = &st.superseded_by {
        why.push(format!("phase {phase} is superseded by {by}"));
    }
    if st.paused {
        why.push(format!(
            "phase {phase} is paused; only the user can resume it"
        ));
    }
    if st
        .current
        .as_ref()
        .is_some_and(|c| c.state == RequestState::Pending)
    {
        why.push("a request of this phase is still pending".into());
    }
    if st.reasons.iter().any(|r| r.starts_with(MALFORMED)) {
        why.extend(
            st.reasons
                .iter()
                .filter(|r| r.starts_with(MALFORMED))
                .cloned(),
        );
    }
    if !why.is_empty() {
        return Err(why);
    }
    let reg = records
        .iter()
        .find(|r| r.phase == phase && r.kind == "phase_registered")
        .expect("registered");
    let baseline = st
        .baseline_seq
        .and_then(|q| records.get(q as usize - 1))
        .expect("a baseline");
    let (baseline_commit, sha) = if baseline.kind == "baseline_changed" {
        (
            s(&baseline.body, "commit"),
            s(&baseline.body, "task_book_sha256"),
        )
    } else {
        (s(&reg.body, "base"), s(&reg.body, "task_book_sha256"))
    };
    let previous_head = current_request(records, phase).map(|r| s(&r.body, "head").to_string());
    Ok(Ready {
        base: s(&reg.body, "base").into(),
        baseline_commit: baseline_commit.into(),
        task_book: s(&reg.body, "task_book").into(),
        task_book_sha256: sha.into(),
        evidence_dir: s(&reg.body, "evidence_dir").into(),
        previous_head,
        st,
    })
}

/// Writes a sealed snapshot directory and makes it durable before anything refers to it (the 6b
/// plan, snapshot durability): files and directory fsynced, then the parent directory.
fn seal_snapshot(inputs: &Path, input: &[u8], manifest: &[u8]) -> Result<PathBuf> {
    if !inputs.exists() {
        fs::create_dir(inputs).with_context(|| format!("creating {}", inputs.display()))?;
        File::open(inputs.parent().context("inputs has a parent")?)?.sync_all()?;
    }
    let mut rnd = [0u8; 16];
    getrandom::fill(&mut rnd).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    let name: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let dir = inputs.join(format!("s-{name}"));
    fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (file, bytes) in [("input.txt", input), ("manifest.json", manifest)] {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(file))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    File::open(&dir)?.sync_all()?;
    File::open(inputs)?.sync_all()?;
    Ok(dir)
}

impl Flow {
    fn open(cfg: Config, kp: Keypair, bridge: BridgeRead, service: bool) -> Result<Self> {
        // Absolute, so every parent directory fsynced on the way (snapshots) is a real one.
        anyhow::ensure!(
            cfg.dir.is_absolute(),
            "milestone state directory {} must be absolute",
            cfg.dir.display()
        );
        let ledger = Ledger::new(&cfg.dir, &cfg.repo_id, kp.verifying_key())?;
        let owner = if service {
            let f = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(cfg.dir.join("owner.lock"))?;
            match f.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    bail!("another gate service owns {}", cfg.dir.display())
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e).context("owner.lock"),
            }
            // Informational (the lock is what counts), but written durably all the same.
            let info = cfg.dir.join("owner.json");
            fs::write(
                &info,
                serde_json::to_vec(&json!({ "pid": std::process::id(), "started_at": now() }))?,
            )?;
            File::open(&info)?.sync_all()?;
            File::open(&cfg.dir)?.sync_all()?;
            Some(f)
        } else {
            None
        };
        Ok(Self {
            cfg,
            ledger,
            kp,
            bridge,
            phases: Mutex::new(BTreeMap::new()),
            running: Mutex::new(BTreeSet::new()),
            halted: Mutex::new(None),
            halted_cv: Condvar::new(),
            admit: Mutex::new(()),
            #[cfg(test)]
            hooks: Mutex::new(TestHooks::default()),
            service,
            _owner: owner,
        })
    }

    /// The gate service: owns the milestone state for its whole life, dispatches jobs, and at
    /// start closes Pending requests that have no job (their owner is gone: it held the lock).
    pub fn open_service(cfg: Config, kp: Keypair, bridge: BridgeRead) -> Result<Arc<Self>> {
        let f = Arc::new(Self::open(cfg, kp, bridge, true)?);
        f.recover()?;
        f.ledger
            .transact(&f.kp, &now(), |snap| {
                let mut drafts = Vec::new();
                let phases: BTreeSet<&str> = snap
                    .records
                    .iter()
                    .filter(|r| r.kind == "request")
                    .map(|r| r.phase.as_str())
                    .collect();
                for phase in phases {
                    let st = derive(&snap.records, phase, 0);
                    if let Some(c) = st.current.filter(|c| c.state == RequestState::Pending)
                        && find(&snap.records, phase, "job", &c.request_id).is_none()
                    {
                        drafts.push(draft(
                            "request_closed",
                            phase,
                            json!({ "request_id": c.request_id, "state": "failed",
                                "why": "gate restarted before the reply" }),
                        ));
                    }
                }
                Ok(Tx { drafts, out: () })
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(f)
    }

    /// An admin command or a status read: never dispatches, never closes a Pending request for
    /// lack of a job.
    pub fn open_admin(cfg: Config, kp: Keypair, bridge: BridgeRead) -> Result<Arc<Self>> {
        Ok(Arc::new(Self::open(cfg, kp, bridge, false)?))
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    fn phase_lock(&self, phase: &str) -> Arc<Mutex<()>> {
        self.phases
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(phase.to_string())
            .or_default()
            .clone()
    }

    /// One read of the remote into the private cache.
    fn fetch(&self) -> sync::Remote {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering::SeqCst;
            let h = self.hooks.lock().unwrap_or_else(|p| p.into_inner());
            if h.failed_fetches.load(SeqCst) > 0 {
                h.failed_fetches.fetch_sub(1, SeqCst);
                return sync::Remote::Unreachable("test: fetch failed".into());
            }
        }
        sync::fetch_remote(&self.cfg.dir.join("remote.git"), &self.cfg.remote_url)
    }

    /// Step 1: fetch the remote and import what it has beyond the local log.
    pub fn sync(&self) -> Result<Sync> {
        self.ledger.import(&self.fetch())
    }

    /// Why this service stopped dispatching, if it did: a result that could not be written is
    /// fatal (6b plan n6 §3). Nothing is dispatched or released from then on; the service (6c)
    /// exits, and the next start recovers from what is on file.
    pub fn halted(&self) -> Option<String> {
        self.halted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Sets the latch at once (status, routes and the watchdog see it immediately, DSH's 6c
    /// review ①), then waits on the admission lock as a barrier: a send in progress under it
    /// finishes, and once halt() returns no job line is written any more (every send rechecks
    /// the latch under that lock).
    pub(crate) fn halt(&self, why: String) {
        {
            let mut h = self.halted.lock().unwrap_or_else(|p| p.into_inner());
            if h.is_none() {
                *h = Some(why);
            }
            self.halted_cv.notify_all();
        }
        drop(self.admit.lock().unwrap_or_else(|p| p.into_inner()));
    }

    /// Blocks until the service halts and returns why (the predicate is checked under the
    /// latch's own lock, so a halt is never missed).
    pub fn wait_halted(&self) -> String {
        let h = self.halted.lock().unwrap_or_else(|p| p.into_inner());
        #[cfg(test)]
        self.hook(|t| t.waiting.clone());
        let h = self
            .halted_cv
            .wait_while(h, |h| h.is_none())
            .unwrap_or_else(|p| p.into_inner());
        h.clone().unwrap_or_default()
    }

    /// Step 2: complete every group a crash cut short. A mismatch stops the log for the user.
    /// Confirm groups come first, then job results, so one pass may leave work for the next:
    /// passes repeat until nothing is left (bounded; each pass completes at least one group).
    pub fn recover(&self) -> std::result::Result<usize, FlowError> {
        let mut total = 0;
        for _ in 0..64 {
            let (n, _) = self.ledger.transact(&self.kp, &now(), |snap| {
                let drafts = recovery(&snap.records)?;
                Ok(Tx {
                    out: drafts.len(),
                    drafts,
                })
            })?;
            if n == 0 {
                return Ok(total);
            }
            total += n;
        }
        Err(refused("recovery did not settle after 64 passes"))
    }

    /// The application, checked and kept in `pending/<phase>.json` until the user confirms it.
    pub fn apply(
        &self,
        phase: &str,
        base: &str,
        task_book: &str,
        evidence_dir: &str,
        conditions: Vec<String>,
        supersedes: Option<String>,
    ) -> std::result::Result<Application, FlowError> {
        if !agent_bridge::valid_id(phase) || phase.len() > 56 {
            return Err(refused(format!(
                "phase {phase:?} is not a valid id of at most 56 characters"
            )));
        }
        let snap = self.ledger.load()?;
        if snap
            .records
            .iter()
            .any(|r| r.phase == phase && r.kind == "phase_registered")
        {
            return Err(refused(format!("phase {phase} is already registered")));
        }
        let l = input::locate(&self.cfg.repo, &[base])?;
        let g = Isolated::new(&l.objects, &l.format)?;
        let entry = g
            .entry(&l.ids[0], task_book.as_bytes())?
            .ok_or_else(|| refused(format!("task book {task_book} is not in {base}")))?;
        let sha = sha256_hex(&g.blob(&entry.2)?);
        let dir = evidence_dir.trim_end_matches('/');
        if dir
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err(refused(format!(
                "evidence directory {evidence_dir:?} is not a plain relative path"
            )));
        }
        let a = Application {
            phase: phase.into(),
            base: l.ids[0].clone(),
            task_book: task_book.into(),
            task_book_sha256: sha,
            evidence_dir: dir.into(),
            conditions,
            supersedes,
        };
        let pending = self.cfg.dir.join("pending");
        fs::create_dir_all(&pending)?;
        let tmp = pending.join(format!("{phase}.json.tmp"));
        fs::write(
            &tmp,
            serde_json::to_vec_pretty(&a).map_err(anyhow::Error::from)?,
        )?;
        File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, pending.join(format!("{phase}.json")))?;
        File::open(&pending)?.sync_all()?;
        Ok(a)
    }

    /// The user confirms an application (§3.1): `phase_registered` with condition ids, and for a
    /// superseding phase in the same group `phase_superseded` for the old one and the
    /// cancellation of its Pending request.
    pub fn confirm(&self, phase: &str) -> std::result::Result<Vec<Record>, FlowError> {
        let path = self.cfg.dir.join("pending").join(format!("{phase}.json"));
        let a: Application = serde_json::from_slice(
            &fs::read(&path).map_err(|e| refused(format!("no application for {phase}: {e}")))?,
        )
        .map_err(|e| refused(format!("application unreadable: {e}")))?;
        // Both phases' locks (the new one and the one it supersedes), always in name order.
        let mut names = vec![phase.to_string()];
        names.extend(a.supersedes.iter().filter(|o| *o != phase).cloned());
        names.sort();
        let locks: Vec<_> = names.iter().map(|n| self.phase_lock(n)).collect();
        let _guards: Vec<_> = locks
            .iter()
            .map(|l| l.lock().unwrap_or_else(|p| p.into_inner()))
            .collect();
        // The task book the application names is still at base with the hash it recorded.
        let l = input::locate(&self.cfg.repo, &[&a.base])?;
        let g = Isolated::new(&l.objects, &l.format)?;
        let entry = g.entry(&l.ids[0], a.task_book.as_bytes())?.ok_or_else(|| {
            refused(format!(
                "task book {} is no longer in {}",
                a.task_book, a.base
            ))
        })?;
        if l.ids[0] != a.base || sha256_hex(&g.blob(&entry.2)?) != a.task_book_sha256 {
            return Err(refused(format!(
                "the task book at {} is not what the application recorded",
                a.base
            )));
        }
        self.recover()?;
        let ((), written) = self.ledger.transact(&self.kp, &now(), |snap| {
            let r = &snap.records;
            if r.iter()
                .any(|x| x.phase == phase && x.kind == "phase_registered")
            {
                return Err(format!("phase {phase} is already registered"));
            }
            let conditions: Vec<Value> = a
                .conditions
                .iter()
                .enumerate()
                .map(|(k, t)| json!({ "id": format!("{phase}/R{}", k + 1), "text": t }))
                .collect();
            let mut body = json!({
                "base": a.base, "task_book": a.task_book, "task_book_sha256": a.task_book_sha256,
                "evidence_dir": a.evidence_dir, "conditions": conditions,
            });
            let mut drafts = Vec::new();
            if let Some(old) = &a.supersedes {
                if old == phase {
                    return Err("a phase cannot supersede itself".into());
                }
                let st = derive(r, old, 0);
                if !st.registered {
                    return Err(format!("phase {old} is not registered"));
                }
                if let Some(by) = st.superseded_by {
                    return Err(format!("phase {old} is already superseded by {by}"));
                }
                // How many supersession steps the new phase would inherit through.
                let mut steps = 1;
                let mut cur = old.clone();
                while let Some(prev) = r
                    .iter()
                    .find(|x| x.phase == cur && x.kind == "phase_registered")
                    .and_then(|x| x.body.get("supersedes").and_then(Value::as_str))
                {
                    steps += 1;
                    cur = prev.to_string();
                    if steps > MAX_SUPERSESSION {
                        break;
                    }
                }
                if steps > MAX_SUPERSESSION {
                    return Err(format!(
                        "superseding {old} would make a chain of more than {MAX_SUPERSESSION} steps"
                    ));
                }
                body["supersedes"] = json!(old);
                drafts.push(draft("phase_registered", phase, body));
                drafts.push(draft("phase_superseded", old, json!({ "by": phase })));
                if let Some(c) = st.current.filter(|c| c.state == RequestState::Pending) {
                    drafts.push(draft(
                        "request_closed",
                        old,
                        json!({ "request_id": c.request_id, "state": "cancelled",
                                "why": format!("superseded by {phase}") }),
                    ));
                }
            } else {
                drafts.push(draft("phase_registered", phase, body));
            }
            Ok(Tx { drafts, out: () })
        })?;
        let _ = fs::remove_file(&path);
        Ok(written)
    }

    /// One request for `phase` at `head` (§3.2, §4.1): checked, assembled and sealed outside the
    /// lock, recorded under it on the tail it was prepared on, then dispatched (service only).
    pub fn request(
        self: &Arc<Self>,
        phase: &str,
        head: &str,
    ) -> std::result::Result<String, FlowError> {
        let lock = self.phase_lock(phase);
        let _g = lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut last = None;
        for _ in 0..ATTEMPTS {
            match self.try_request(phase, head)? {
                Some((id, input, _admitted)) => {
                    // Still under the admission guard: a halt cannot fall between the commit
                    // of the request and the start of its job.
                    if self.service {
                        self.dispatch(phase, &id, input);
                    }
                    return Ok(id);
                }
                None => last = Some(()),
            }
        }
        let _ = last;
        Err(refused(format!(
            "the log kept changing during {ATTEMPTS} attempts; try again"
        )))
    }

    /// One prepare-and-commit round; `None`: the log changed since preparing.
    fn try_request(
        &self,
        phase: &str,
        head: &str,
    ) -> std::result::Result<Option<(String, String, std::sync::MutexGuard<'_, ()>)>, FlowError>
    {
        if let Some(why) = self.halted() {
            return Err(refused(format!("service halted: {why}")));
        }
        let sync = self.sync()?;
        if let Sync::Stop(why) = &sync {
            return Err(refused(format!("release stopped: {why}")));
        }
        self.recover()?;
        let snap = self.ledger.load()?;
        if let Some(why) = &snap.stop {
            return Err(refused(format!("log stopped: {why}")));
        }
        let tip = snap.records.last().map(|r| r.hash.clone());
        let r = ready(&snap.records, phase).map_err(FlowError::Refused)?;

        // Fix every object id once (§4.1, 6a plan B).
        let notes_ref = attest::NOTES_REF;
        let (l, notes_commit) = match input::locate(
            &self.cfg.repo,
            &[&r.base, head, &r.baseline_commit, notes_ref],
        ) {
            Ok(l) => {
                let n = l.ids[3].clone();
                (l, Some(n))
            }
            Err(_) => (
                input::locate(&self.cfg.repo, &[&r.base, head, &r.baseline_commit])?,
                None,
            ),
        };
        let (base, head_id, baseline) = (l.ids[0].clone(), l.ids[1].clone(), l.ids[2].clone());
        let mut g = Isolated::new(&l.objects, &l.format)?;
        let mut why = Vec::new();
        if !g.is_ancestor(&base, &head_id)? {
            why.push(format!("base {base} is not an ancestor of head {head_id}"));
        }
        if let Some(prev) = &r.previous_head
            && !g.is_ancestor(prev, &head_id)?
        {
            why.push(format!("head {head_id} does not move forward from {prev}"));
        }
        if !why.is_empty() {
            return Err(FlowError::Refused(why));
        }
        let text = |args: &[&str]| -> Result<String> {
            let a: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
            String::from_utf8(g.git(&a, args[0])?).context("git output is not UTF-8")
        };
        let range = format!("{base}..{head_id}");
        let list = text(&["rev-list", "--reverse", &range])?;
        if list.trim().is_empty() {
            return Err(refused(format!(
                "{range} holds no commits: nothing to audit"
            )));
        }
        let mut commits = Vec::new();
        for c in list.lines().filter(|l| !l.is_empty()) {
            let meta = text(&[
                "log",
                "-1",
                "-z",
                "--format=%an%x00%(trailers:key=Co-authored-by,valueonly,separator=%x1f)",
                c,
            ])?;
            let mut parts = meta.split('\0');
            let author = parts.next().unwrap_or("").to_string();
            let trailers: Vec<String> = parts
                .next()
                .unwrap_or("")
                .trim_end_matches('\n')
                .split('\x1f')
                .filter(|t| !t.trim().is_empty())
                .map(|t| t.trim().to_string())
                .collect();
            let attestation = match &notes_commit {
                None => Attestation::Missing,
                Some(n) => {
                    match attest::verify_commit_at(&g.path(), c, n, &self.cfg.attestation_key) {
                        Ok(_) => Attestation::Verified,
                        Err(e) if e.to_string().contains("no review-gate attestation note") => {
                            Attestation::Missing
                        }
                        Err(e) => Attestation::Invalid(format!("{e:#}")),
                    }
                }
            };
            commits.push(CommitCheck {
                id: c.to_string(),
                author,
                trailers,
                attestation,
            });
        }
        // Committer times of every commit in the range (merges included): they bound the
        // source window together with the registration time (sources.rs).
        let mut commit_times = Vec::new();
        for c in &commits {
            let t: i64 = text(&["log", "-1", "--format=%ct", &c.id])?
                .trim()
                .parse()
                .with_context(|| format!("commit time of {}", c.id))?;
            commit_times
                .push(chrono::DateTime::from_timestamp(t, 0).context("commit time out of range")?);
        }
        let tree = text(&["rev-parse", &format!("{head_id}^{{tree}}")])?
            .trim()
            .to_string();

        // Sources and independence, refused up front (6b plan, question 1).
        let table = vendors(&snap.records)?;
        let bridge = (self.bridge)();
        let outcome = sources::judge(&sources::Inputs {
            phase,
            records: &snap.records,
            commits: &commits,
            commit_times: &commit_times,
            bridge: bridge.as_ref().map_err(String::clone),
            implementers: &self.cfg.implementers,
            auditor: &self.cfg.auditor,
            vendors: &table,
        });
        if !outcome.sources_known || !outcome.independent {
            let mut why = outcome.sources_reasons.clone();
            why.extend(outcome.independence_reasons.clone());
            return Err(FlowError::Refused(why));
        }
        let bridge_revision = bridge
            .as_ref()
            .map(|c| c.revision.clone())
            .unwrap_or_default();
        let vendors_sha = table.content_sha256()?;
        let open: Vec<(String, bool, String)> =
            r.st.conditions
                .iter()
                .filter(|c| c.closed.is_none())
                .map(|c| (c.id.clone(), c.blocking, c.text.clone()))
                .collect();
        let request_hint = "(the final id is in the signed request record)".to_string();
        let assembled = input::assemble(
            &mut g,
            &Material {
                phase,
                request_id: &request_hint,
                base: &base,
                head: &head_id,
                baseline_commit: &baseline,
                task_book: &r.task_book,
                task_book_sha256: &r.task_book_sha256,
                evidence_dir: &r.evidence_dir,
                notes_commit: notes_commit.as_deref(),
            },
            &Trusted {
                auditor: self.cfg.auditor.clone(),
                open_conditions: open,
                participants: serde_json::to_value(&outcome).context("participants")?,
                bridge_revision: bridge_revision.clone(),
                vendors_version: table.version,
                vendors_sha256: vendors_sha.clone(),
            },
        )
        .map_err(|e| match e {
            input::InputError::TooLarge { .. } => refused(e.to_string()),
            input::InputError::Other(e) => FlowError::Other(e),
        })?;
        let input_text = String::from_utf8(assembled.bytes.clone()).context("input is UTF-8")?;
        let manifest = serde_json::to_vec_pretty(&assembled.manifest).context("manifest")?;
        let dir = seal_snapshot(&self.cfg.dir.join("inputs"), &assembled.bytes, &manifest)?;
        let snapshot = json!({
            "dir": dir.file_name().map(|n| n.to_string_lossy().to_string()),
            "manifest_sha256": sha256_hex(&manifest),
            "input_sha256": assembled.manifest.sha256,
            "size": assembled.manifest.size,
        });

        // Read the remote again before committing (6b plan n6 §4, step 4): a revocation or a
        // supersession published while preparing is imported (the tail then moves, and the
        // request is prepared again), and a remote that stopped answering refuses the request.
        if let Sync::Stop(why) = self.ledger.import(&self.fetch())? {
            return Err(refused(format!("release stopped: {why}")));
        }
        self.recover()?;

        // Commit on the tail the request was prepared on.
        let job_timeout = self.cfg.job_timeout.as_secs();
        let expected = json!({ "uid": self.cfg.runner.uid, "cgroup": self.cfg.runner.cgroup });
        // Admission: checked again, and held through the commit and the dispatch.
        let admitted = self.admit.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(why) = self.halted() {
            return Err(refused(format!("service halted: {why}")));
        }
        #[cfg(test)]
        self.hook(|h| h.before_commit.clone());
        let res = self.ledger.transact(&self.kp, &now(), |snap2| {
            if snap2.records.last().map(|r| r.hash.clone()) != tip {
                return Err("stale".into());
            }
            let r2 = ready(&snap2.records, phase).map_err(|w| w.join("; "))?;
            let n = snap2
                .records
                .iter()
                .filter(|x| x.phase == phase && x.kind == "request")
                .count()
                + 1;
            let id = format!("{phase}-q{n}");
            if !valid_job_id(&id) {
                return Err(format!("request id {id:?} is not a valid runner job id"));
            }
            // The line the runner will read, as it will be sent (plus runner::call's newline).
            let line = serde_json::to_string(&json!({
                "job_id": id, "input": input_text, "timeout_secs": job_timeout,
            }))
            .map_err(|e| e.to_string())?;
            if input_text.len() > input::MAX_INPUT
                || line.len() + 1 > input::MAX_INPUT + RUNNER_LINE_SLACK
            {
                return Err(format!(
                    "the request line would be {} bytes, over what the runner reads ({}): refused",
                    line.len() + 1,
                    input::MAX_INPUT + RUNNER_LINE_SLACK
                ));
            }
            let body = json!({
                "request_id": id, "head": head_id, "tree": tree,
                "baseline_seq": r2.st.baseline_seq, "task_book_sha256": r2.task_book_sha256,
                "snapshot": snapshot, "template": input::TEMPLATE,
                "notes_commit": notes_commit, "participants": outcome.participants,
                "vendors": { "version": table.version, "sha256": vendors_sha },
                "bridge_revision": bridge_revision, "auditor": self.cfg.auditor,
                "runner": expected, "count": r2.st.count, "threshold": THRESHOLD,
                "sources": { "known": true, "why": "" },
                "independence": { "ok": true, "why": "" },
            });
            Ok(Tx {
                drafts: vec![draft("request", phase, body)],
                out: (id, line),
            })
        });
        match res {
            Ok(((id, line), _)) => Ok(Some((id, line, admitted))),
            Err(TxError::Refused(w)) if w == "stale" => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Runs the job in the background and records its result.
    fn dispatch(self: &Arc<Self>, phase: &str, id: &str, line: String) {
        self.running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_string());
        let me = self.clone();
        let (phase, id) = (phase.to_string(), id.to_string());
        std::thread::spawn(move || {
            // When is a job sent (Codex review of e93dd92)? When its request line has been
            // written to the runner, and that write happens under the admission lock, after a
            // last look at `halted`. So halt() waits for a send in progress, and once it has
            // returned no request line is written. A job not sent by then is not sent at all,
            // and nothing is recorded for it: it stays Pending, and the next start closes it as
            // failed (it has no job). One already sent is not taken back; its reply is read
            // outside the lock.
            let checked = me.check_snapshot(&phase, &id);
            #[cfg(test)]
            me.hook(|h| h.before_send.clone());
            let sent = {
                let _admit = me.admit.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(why) = me.halted() {
                    Sending::Halted(why)
                } else if let Err(e) = checked {
                    Sending::Failed(e)
                } else {
                    #[cfg(test)]
                    me.hook(|h| h.while_sending.clone());
                    let timeout = me.cfg.job_timeout + Duration::from_secs(60);
                    match runner::send(&me.cfg.runner_socket, &line, timeout) {
                        Ok(stream) => Sending::Sent(stream),
                        Err(e) => Sending::Failed(format!("{e:#}")),
                    }
                }
            };
            let reply = match sent {
                Sending::Halted(why) => {
                    eprintln!("review-gate: {id} not sent: service halted: {why}");
                    me.running
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&id);
                    return;
                }
                Sending::Failed(e) => Err(e),
                Sending::Sent(stream) => {
                    runner::receive(&stream, &me.cfg.runner).map_err(|e| format!("{e:#}"))
                }
            };
            let job = job_body(&id, reply);
            match me.complete(&phase, &id, job) {
                Ok(()) => {}
                // A refusal is a decision on the state (for instance a job already on file),
                // not a failure to write.
                Err(FlowError::Refused(why)) => {
                    eprintln!(
                        "review-gate: result of {id} not recorded: {}",
                        why.join("; ")
                    )
                }
                Err(FlowError::Other(e)) => {
                    eprintln!("review-gate: recording the result of {id} failed: {e:#}");
                    me.halt(format!("recording the result of {id} failed: {e:#}"));
                }
            }
            me.running
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
        });
    }

    /// The sealed input is what goes out: both files are still what the request recorded.
    fn check_snapshot(&self, phase: &str, id: &str) -> std::result::Result<(), String> {
        let snap = self.ledger.load().map_err(|e| e.to_string())?;
        let req = find(&snap.records, phase, "request", id).ok_or("request not on file")?;
        let dir = req
            .body
            .pointer("/snapshot/dir")
            .and_then(Value::as_str)
            .ok_or("request names no snapshot")?;
        let sealed = self.cfg.dir.join("inputs").join(dir);
        for (file, key) in [
            ("input.txt", "input_sha256"),
            ("manifest.json", "manifest_sha256"),
        ] {
            let bytes =
                fs::read(sealed.join(file)).map_err(|e| format!("snapshot unreadable: {e}"))?;
            let want = req
                .body
                .pointer(&format!("/snapshot/{key}"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if want.is_empty() || sha256_hex(&bytes) != want {
                return Err("snapshot changed".into());
            }
        }
        Ok(())
    }

    /// Runs the `in_call` test hook (service.rs `guarded`).
    #[cfg(test)]
    pub(crate) fn test_in_call(&self) {
        self.hook(|h| h.in_call.clone());
    }

    #[cfg(test)]
    fn hook(
        &self,
        pick: impl FnOnce(&TestHooks) -> Option<Arc<dyn Fn() + Send + std::marker::Sync>>,
    ) {
        let h = pick(&self.hooks.lock().unwrap_or_else(|p| p.into_inner()));
        if let Some(h) = h {
            h();
        }
    }

    /// The snapshot check and the runner call in one, without the admission lock (tests).
    #[cfg(test)]
    fn run_job(
        &self,
        phase: &str,
        id: &str,
        line: &str,
    ) -> std::result::Result<runner::Reply, String> {
        self.check_snapshot(phase, id)?;
        let timeout = self.cfg.job_timeout + Duration::from_secs(60);
        runner::call(&self.cfg.runner_socket, line, &self.cfg.runner, timeout)
            .map_err(|e| format!("{e:#}"))
    }

    /// Records a job's result: the whole group at once, or only `job` (and `audit`) as late when
    /// the request is no longer the current, Pending one of a phase that is not superseded.
    pub fn complete(
        &self,
        phase: &str,
        id: &str,
        job: Value,
    ) -> std::result::Result<(), FlowError> {
        let lock = self.phase_lock(phase);
        let _g = lock.lock().unwrap_or_else(|p| p.into_inner());
        self.recover()?;
        self.ledger.transact(&self.kp, &now(), |snap| {
            let r = &snap.records;
            let request = find(r, phase, "request", id).ok_or("request not on file")?;
            if find(r, phase, "job", id).is_some() {
                return Err(format!("a job is already recorded for {id}"));
            }
            let st = derive(r, phase, 0);
            let live = st.superseded_by.is_none()
                && st
                    .current
                    .as_ref()
                    .is_some_and(|c| c.request_id == id && c.state == RequestState::Pending);
            let before = &r[..(request.seq - 1) as usize];
            let audit_seq = r.len() as u64 + 2;
            let mut drafts = vec![draft("job", phase, job.clone())];
            let rest = rest_after_job(phase, request, &job, before, audit_seq);
            if live {
                drafts.extend(rest);
            } else {
                // Late: archived, with no effect on any state.
                drafts.extend(rest.into_iter().filter(|d| d.kind == "audit"));
            }
            Ok(Tx { drafts, out: () })
        })?;
        Ok(())
    }

    /// The implementer withdraws a Pending request; a reply arriving later is late.
    pub fn cancel(&self, phase: &str, id: &str) -> std::result::Result<(), FlowError> {
        let lock = self.phase_lock(phase);
        let _g = lock.lock().unwrap_or_else(|p| p.into_inner());
        self.recover()?;
        self.ledger.transact(&self.kp, &now(), |snap| {
            let st = derive(&snap.records, phase, 0);
            match st.current {
                Some(c) if c.request_id == id && c.state == RequestState::Pending => Ok(Tx {
                    drafts: vec![draft(
                        "request_closed",
                        phase,
                        json!({ "request_id": id, "state": "cancelled", "why": "cancelled by the implementer" }),
                    )],
                    out: (),
                }),
                _ => Err(format!("{id} is not the pending request of {phase}")),
            }
        })?;
        Ok(())
    }

    /// The state of a phase on one synced, recovered snapshot (§6 status).
    pub fn status(&self, phase: &str) -> std::result::Result<Status, FlowError> {
        let recovered = self.recover();
        // The classification and the snapshot the state is derived from come from one lock.
        let (sync, snap): (Sync, Snapshot) = self.ledger.import_and_load(&self.fetch())?;
        let st = derive(&snap.records, phase, snap.checkpoint);
        let mut reasons = st.reasons.clone();
        if let Sync::Stop(why) = &sync {
            reasons.push(format!("release stopped: {why}"));
        }
        if let Some(why) = &snap.stop {
            reasons.push(format!("log stopped: {why}"));
        }
        if let Err(e) = recovered {
            reasons.push(format!("recovery: {e}"));
        }
        if let Some(why) = self.halted() {
            reasons.push(format!("service halted: {why}"));
        }
        let (seq, hash) = snap
            .records
            .last()
            .map_or((0, String::new()), |r| (r.seq, r.hash.clone()));
        #[cfg(test)]
        self.hook(|h| h.in_status.clone());
        Ok(Status {
            released: reasons.is_empty() && st.released,
            reasons,
            sync: format!("{sync:?}").chars().take(200).collect(),
            seq,
            hash,
            state: st,
        })
    }

    /// Status for the service (6c, Codex n6 Q1): first an attempt to archive what the remote
    /// now holds (it may recover a fetch that failed before), then a fresh status snapshot. The
    /// two are never mixed: the state comes only from the status snapshot. A failed archive is a
    /// reason, and `released` is recomputed from the final reasons.
    pub fn status_archived(&self, phase: &str) -> std::result::Result<Status, FlowError> {
        let archived = self.archive();
        let mut st = self.status(phase)?;
        if let Err(e) = archived {
            st.reasons
                .push(format!("archive: {}", short(&e.to_string())));
        }
        st.released = st.reasons.is_empty() && st.state.released;
        Ok(st)
    }

    /// `published`, then an attempt to archive. `archived` says whether the reported position
    /// is proven archived (the checkpoint covers it), not whether this call wrote a record: an
    /// already archived target, or one a concurrent publish archived, counts.
    pub fn published_then_archive(
        &self,
        seq: u64,
        hash: &str,
    ) -> std::result::Result<PublishOutcome, FlowError> {
        self.published(seq, hash)?;
        let tried = self.archive();
        let snap = self.ledger.load()?;
        let archived = snap.stop.is_none()
            && snap.checkpoint >= seq
            && snap
                .records
                .get((seq as usize).wrapping_sub(1))
                .is_some_and(|r| r.hash == hash);
        let archive_error = match (&tried, archived) {
            (_, true) => None,
            (Err(e), false) => Some(short(&e.to_string())),
            (Ok(_), false) => Some(match &snap.stop {
                Some(why) => short(&format!("log stopped: {why}")),
                None => "the remote does not hold this position yet".to_string(),
            }),
        };
        Ok(PublishOutcome {
            published: true,
            archived,
            checkpoint: snap.checkpoint,
            archive_error,
        })
    }

    /// The signed log, for publishing (§3.5).
    pub fn log(&self) -> Result<String> {
        render_log(&self.ledger.load()?.records)
    }

    /// The implementer reports what it pushed; recorded only for a position the log holds.
    pub fn published(&self, seq: u64, hash: &str) -> std::result::Result<(), FlowError> {
        self.ledger.transact(&self.kp, &now(), |snap| {
            match snap.records.get((seq as usize).wrapping_sub(1)) {
                Some(r) if r.hash == hash => Ok(Tx {
                    drafts: vec![draft("published", "", json!({ "seq": seq, "hash": hash }))],
                    out: (),
                }),
                _ => Err(format!("record {seq} with hash {hash} is not in the log")),
            }
        })?;
        Ok(())
    }

    /// Archives as far as the remote is proven to hold the local log (§3.5).
    pub fn archive(&self) -> std::result::Result<Option<u64>, FlowError> {
        let (sync, archived) = self
            .ledger
            .archive_remote(&self.kp, &now(), &self.fetch())?;
        if let Sync::Stop(why) = &sync {
            return Err(refused(format!("release stopped: {why}")));
        }
        Ok(archived.and_then(|r| r.body.get("seq").and_then(Value::as_u64)))
    }

    /// A user record on a phase that is not superseded, after a check on the current state.
    fn admin(
        &self,
        phase: &str,
        kind: &str,
        body: Value,
        check: impl FnOnce(&[Record], &PhaseState) -> std::result::Result<(), String>,
    ) -> std::result::Result<Record, FlowError> {
        let lock = self.phase_lock(phase);
        let _g = lock.lock().unwrap_or_else(|p| p.into_inner());
        self.recover()?;
        let (_, mut written) = self.ledger.transact(&self.kp, &now(), |snap| {
            let st = derive(&snap.records, phase, 0);
            if !st.registered {
                return Err(format!("phase {phase} is not registered"));
            }
            if let Some(by) = &st.superseded_by {
                return Err(format!("phase {phase} is superseded by {by}; act on {by}"));
            }
            check(&snap.records, &st)?;
            Ok(Tx {
                drafts: vec![draft(kind, phase, body)],
                out: (),
            })
        })?;
        Ok(written.remove(0))
    }

    pub fn waive(
        &self,
        phase: &str,
        condition: &str,
        reason: &str,
    ) -> std::result::Result<Record, FlowError> {
        self.admin(
            phase,
            "condition_waived",
            json!({ "condition": condition, "reason": reason }),
            |_, st| {
                st.conditions
                    .iter()
                    .any(|c| c.id == condition && c.closed.is_none())
                    .then_some(())
                    .ok_or(format!("no open condition {condition}"))
            },
        )
    }

    /// A new task-book baseline at `commit` (§4.1), its hash computed by the gate.
    pub fn rebaseline(
        &self,
        phase: &str,
        commit: &str,
        reason: &str,
    ) -> std::result::Result<Record, FlowError> {
        let snap = self.ledger.load()?;
        let reg = snap
            .records
            .iter()
            .find(|r| r.phase == phase && r.kind == "phase_registered")
            .ok_or_else(|| refused(format!("phase {phase} is not registered")))?;
        let book = s(&reg.body, "task_book").to_string();
        let l = input::locate(&self.cfg.repo, &[commit])?;
        let g = Isolated::new(&l.objects, &l.format)?;
        let e = g
            .entry(&l.ids[0], book.as_bytes())?
            .ok_or_else(|| refused(format!("task book {book} is not in {commit}")))?;
        let sha = sha256_hex(&g.blob(&e.2)?);
        self.admin(
            phase,
            "baseline_changed",
            json!({ "commit": l.ids[0], "task_book_sha256": sha, "reason": reason }),
            // The hash above was computed for this registration's task book: check again, on
            // the log under the lock, that it is still the one registered.
            |records, _| {
                let now_book = records
                    .iter()
                    .find(|r| r.phase == phase && r.kind == "phase_registered")
                    .map(|r| s(&r.body, "task_book"));
                (now_book == Some(book.as_str()))
                    .then_some(())
                    .ok_or(format!("the registration of {phase} changed; try again"))
            },
        )
    }

    pub fn drop_source(
        &self,
        phase: &str,
        source: &str,
        reason: &str,
    ) -> std::result::Result<Record, FlowError> {
        self.admin(
            phase,
            "source_dropped",
            json!({ "source": source, "reason": reason }),
            |_, _| Ok(()),
        )
    }

    pub fn link_task(
        &self,
        phase: &str,
        task: &str,
        reason: &str,
    ) -> std::result::Result<Record, FlowError> {
        let c = (self.bridge)().map_err(|e| refused(format!("bridge unreadable: {e}")))?;
        if !c.tasks.iter().any(|(t, _)| t.id == task) {
            return Err(refused(format!("bridge task {task} does not exist")));
        }
        self.admin(
            phase,
            "task_linked",
            json!({ "task": task, "reason": reason }),
            |_, _| Ok(()),
        )
    }

    pub fn ignore_task(
        &self,
        phase: &str,
        task: &str,
        reason: &str,
    ) -> std::result::Result<Record, FlowError> {
        self.admin(
            phase,
            "task_ignored",
            json!({ "task": task, "reason": reason }),
            |_, _| Ok(()),
        )
    }

    pub fn revoke(&self, phase: &str, reason: &str) -> std::result::Result<Record, FlowError> {
        self.admin(phase, "revoke", json!({ "reason": reason }), |_, _| Ok(()))
    }

    pub fn resume(&self, phase: &str, reason: &str) -> std::result::Result<Record, FlowError> {
        self.admin(phase, "resume", json!({ "reason": reason }), |_, st| {
            st.paused
                .then_some(())
                .ok_or(format!("phase {phase} is not paused"))
        })
    }

    /// A new vendor table (§3.8), recorded with its content hash.
    pub fn vendors_set(
        &self,
        table: &Vendors,
        reason: &str,
    ) -> std::result::Result<Record, FlowError> {
        let sha = table.content_sha256()?;
        let table_v = serde_json::to_value(table).map_err(anyhow::Error::from)?;
        let (_, mut written) = self.ledger.transact(&self.kp, &now(), |snap| {
            let cur = vendors(&snap.records).map_err(|e| e.to_string())?;
            if table.version <= cur.version {
                return Err(format!(
                    "vendor table version {} is not newer than {}",
                    table.version, cur.version
                ));
            }
            Ok(Tx {
                drafts: vec![draft(
                    "vendors_changed",
                    "",
                    json!({ "table": table_v, "sha256": sha, "reason": reason }),
                )],
                out: (),
            })
        })?;
        Ok(written.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixListener;
    use std::process::Command;

    const SEED: [u8; 32] = [3; 32];
    const BOOK: &str = "# Task book\nAccept when the flow works.\n";

    fn kp() -> Keypair {
        Keypair::from_seed(SEED)
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // A fixed time, so two fixtures built apart have the same commit ids; and in the
            // past, so the work is committed before the phase is registered (the normal case).
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    }

    /// Commits the work tree; with `attest`, signs a commit-review attestation note for it.
    fn commit(repo: &Path, msg: &str, attest_it: bool) -> String {
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-qm", msg]);
        let c = git(repo, &["rev-parse", "HEAD"]);
        if attest_it {
            let st = attest::Statement {
                task: "t".into(),
                version: "1/1".into(),
                commit: c.clone(),
                tree: git(repo, &["rev-parse", "HEAD^{tree}"]),
                parent: git(repo, &["rev-parse", "HEAD^"]),
                review: "v1-1".into(),
                reviewer: "test".into(),
                verdict: "approved".into(),
                at: "now".into(),
            };
            git(
                repo,
                &[
                    "notes",
                    "--ref=review-gate",
                    "add",
                    "-m",
                    &attest::sign(&kp(), &st),
                    &c,
                ],
            );
        }
        c
    }

    fn write(repo: &Path, path: &str, body: &str) {
        let p = repo.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn me() -> Expected {
        Expected {
            uid: fs::metadata("/proc/self").unwrap().uid(),
            cgroup: runner::cgroup_of(std::process::id() as i32).unwrap(),
        }
    }

    /// What the stand-in runner answers.
    #[derive(Clone)]
    enum Mode {
        Answer(String),
        Status(&'static str),
        WrongSha,
        WrongJob,
        /// Wait for a signal before answering (late results).
        Hold(Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>, String),
        /// Send this reply line verbatim (an earlier job's reply, replayed).
        Replay(String),
    }

    struct Fx {
        _dir: tempfile::TempDir,
        root: PathBuf,
        repo: PathBuf,
        remote: PathBuf,
        base: String,
        head: String,
        mode: Arc<Mutex<Mode>>,
        /// Requests the runner stand-in has received.
        calls: Arc<std::sync::atomic::AtomicUsize>,
        /// Request lines the runner stand-in has read in full (a handshake for held jobs).
        lines: Arc<(Mutex<usize>, Condvar)>,
        /// Every reply line the runner stand-in sent, in order.
        sent: Arc<Mutex<Vec<String>>>,
        bridge: Arc<Mutex<std::result::Result<Collected, String>>>,
    }

    fn approve() -> String {
        "VERDICT: APPROVE\nRESOLVED:\n- p/R1: done\n".into()
    }

    impl Fx {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            let repo = root.join("repo");
            fs::create_dir(&repo).unwrap();
            git(&repo, &["init", "-q", "-b", "main"]);
            git(&repo, &["config", "user.name", "victormshan"]);
            git(&repo, &["config", "user.email", "v@x"]);
            write(&repo, "plans/book.md", BOOK);
            let base = commit(&repo, "base", false);
            write(&repo, "ev/a.txt", "evidence a\n");
            git(&repo, &["add", "-A"]);
            git(
                &repo,
                &[
                    "commit",
                    "-qm",
                    "work\n\nCo-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>",
                ],
            );
            let head = git(&repo, &["rev-parse", "HEAD"]);
            // Attest head by hand (commit() would commit again).
            let st = attest::Statement {
                task: "t".into(),
                version: "1/1".into(),
                commit: head.clone(),
                tree: git(&repo, &["rev-parse", "HEAD^{tree}"]),
                parent: base.clone(),
                review: "v1-1".into(),
                reviewer: "test".into(),
                verdict: "approved".into(),
                at: "now".into(),
            };
            git(
                &repo,
                &[
                    "notes",
                    "--ref=review-gate",
                    "add",
                    "-m",
                    &attest::sign(&kp(), &st),
                    &head,
                ],
            );
            let remote = root.join("remote.git");
            Command::new("git")
                .args(["init", "-q", "--bare"])
                .arg(&remote)
                .output()
                .unwrap();
            fs::create_dir(root.join("state")).unwrap();
            let fx = Fx {
                _dir: dir,
                root,
                repo,
                remote,
                base,
                head,
                mode: Arc::new(Mutex::new(Mode::Answer(approve()))),
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                lines: Arc::default(),
                sent: Arc::default(),
                bridge: Arc::new(Mutex::new(Ok(Collected {
                    revision: "rev".into(),
                    tasks: vec![],
                }))),
            };
            fx.serve_runner();
            fx
        }

        fn socket(&self) -> PathBuf {
            self.root.join("runner.sock")
        }

        /// A runner stand-in: answers every request according to `mode`.
        fn serve_runner(&self) {
            let l = UnixListener::bind(self.socket()).unwrap();
            let mode = self.mode.clone();
            let calls = self.calls.clone();
            let (lines, sent) = (self.lines.clone(), self.sent.clone());
            std::thread::spawn(move || {
                for c in l.incoming() {
                    let Ok(mut c) = c else { break };
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mode = mode.lock().unwrap().clone();
                    let (lines, sent) = (lines.clone(), sent.clone());
                    std::thread::spawn(move || {
                        let mut line = String::new();
                        BufReader::new(c.try_clone().unwrap())
                            .read_line(&mut line)
                            .unwrap();
                        {
                            let (n, cv) = &*lines;
                            *n.lock().unwrap() += 1;
                            cv.notify_all();
                        }
                        if let Mode::Replay(old) = &mode {
                            c.write_all(format!("{old}\n").as_bytes()).unwrap();
                            sent.lock().unwrap().push(old.clone());
                            return;
                        }
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let input = req["input"].as_str().unwrap();
                        let mut sha = sha256_hex(input.as_bytes());
                        let (status, answer) = match mode {
                            Mode::Answer(a) => ("completed", a),
                            Mode::Status(s) => (s, String::new()),
                            Mode::WrongSha => {
                                sha = "0".repeat(64);
                                ("completed", approve())
                            }
                            Mode::WrongJob => ("completed-wrong-job", approve()),
                            Mode::Hold(rx, a) => {
                                let rx = rx.lock().unwrap().take().unwrap();
                                let _ = rx.recv();
                                ("completed", a)
                            }
                            Mode::Replay(_) => unreachable!("answered above"),
                        };
                        let (job_id, status) = if status == "completed-wrong-job" {
                            (json!("someone-else"), "completed")
                        } else {
                            (req["job_id"].clone(), status)
                        };
                        let resp = json!({
                            "job_id": job_id, "input_sha256": sha, "status": status,
                            "exit_code": 0, "error": null, "answer": answer, "item_types": [],
                            "commands": [], "evidence_complete": true, "cleanup": "gone",
                            "runner_version": "test", "started_at": "2026-10-18T00:00:00Z",
                            "finished_at": "2026-10-18T00:00:01Z",
                        });
                        c.write_all(format!("{resp}\n").as_bytes()).unwrap();
                        sent.lock().unwrap().push(resp.to_string());
                    });
                }
            });
        }

        fn cfg(&self) -> Config {
            Config {
                dir: self.root.join("state"),
                repo_id: "repo".into(),
                repo: self.repo.clone(),
                remote_url: self.remote.to_string_lossy().to_string(),
                runner_socket: self.socket(),
                runner: me(),
                job_timeout: Duration::from_secs(10),
                auditor: "auditor".into(),
                implementers: BTreeSet::from(["claude".to_string()]),
                attestation_key: kp().verifying_key(),
            }
        }

        fn reader(&self) -> BridgeRead {
            let b = self.bridge.clone();
            Arc::new(move || b.lock().unwrap().clone())
        }

        fn service(&self) -> Arc<Flow> {
            Flow::open_service(self.cfg(), kp(), self.reader()).unwrap()
        }

        fn admin(&self) -> Arc<Flow> {
            Flow::open_admin(self.cfg(), kp(), self.reader()).unwrap()
        }

        fn register(&self, f: &Flow) {
            f.apply(
                "p",
                &self.base,
                "plans/book.md",
                "ev",
                vec!["accept".into()],
                None,
            )
            .unwrap();
            f.confirm("p").unwrap();
        }

        /// Publishes the signed log to the bare remote, as the implementer would.
        fn publish(&self, f: &Flow) {
            let log = f.log().unwrap();
            sync::publish(
                &self.repo,
                &self.cfg().remote_url,
                &log,
                &kp().verifying_key(),
                "repo",
            )
            .unwrap();
        }
    }

    fn kinds(f: &Flow) -> Vec<String> {
        f.ledger()
            .load()
            .unwrap()
            .records
            .iter()
            .map(|r| r.kind.clone())
            .collect()
    }

    /// Waits until `phase`'s current request is no longer Pending.
    fn settled(f: &Flow, phase: &str) -> PhaseState {
        for _ in 0..200 {
            let st = derive(&f.ledger().load().unwrap().records, phase, 0);
            if st
                .current
                .as_ref()
                .is_some_and(|c| c.state != RequestState::Pending)
            {
                return st;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("request still pending: {:?}", kinds(f));
    }

    #[test]
    fn a_request_runs_records_and_releases_once_archived() {
        let fx = Fx::new();
        let f = fx.service();
        fx.register(&f);
        let id = f.request("p", &fx.head).unwrap();
        assert_eq!(id, "p-q1");
        let st = settled(&f, "p");
        assert_eq!(st.current.as_ref().unwrap().state, RequestState::Completed);
        assert_eq!(
            kinds(&f),
            [
                "phase_registered",
                "request",
                "job",
                "audit",
                "condition_resolved",
                "request_closed"
            ]
        );
        // The sealed snapshot is what the request names.
        let snap = f.ledger().load().unwrap();
        let req = &snap.records[1];
        let dir = req.body.pointer("/snapshot/dir").unwrap().as_str().unwrap();
        let input = fs::read(fx.root.join("state/inputs").join(dir).join("input.txt")).unwrap();
        assert_eq!(
            sha256_hex(&input),
            req.body
                .pointer("/snapshot/input_sha256")
                .unwrap()
                .as_str()
                .unwrap()
        );
        // The job record keeps the sender the kernel named.
        assert_eq!(snap.records[2].body["sender"]["pid"], std::process::id());
        // Not released until archived; publish then archive releases.
        let s1 = f.status("p").unwrap();
        assert!(!s1.released);
        assert!(
            s1.reasons.iter().any(|r| r.contains("not archived")),
            "{:?}",
            s1.reasons
        );
        fx.publish(&f);
        assert!(f.archive().unwrap().is_some());
        let s2 = f.status("p").unwrap();
        assert!(s2.released, "{:?}", s2.reasons);
    }

    #[test]
    fn a_request_without_independence_or_sources_is_refused_and_writes_nothing() {
        let fx = Fx::new();
        let f = fx.admin();
        fx.register(&f);
        let before = kinds(&f);
        // The trailer names Claude: an anthropic auditor is not independent.
        let mut cfg = fx.cfg();
        cfg.auditor = "claude".into();
        let g = Flow::open_admin(cfg, kp(), fx.reader()).unwrap();
        let e = g.request("p", &fx.head).unwrap_err().to_string();
        assert!(e.contains("shares vendor anthropic"), "{e}");
        // An unreadable bridge: sources unknown.
        *fx.bridge.lock().unwrap() = Err("bridge history incomplete".into());
        let e = f.request("p", &fx.head).unwrap_err().to_string();
        assert!(e.contains("bridge could not be read"), "{e}");
        assert_eq!(kinds(&f), before, "nothing recorded");
    }

    #[test]
    fn range_and_signature_rules_are_checked_before_anything_is_recorded() {
        let fx = Fx::new();
        let f = fx.admin();
        fx.register(&f);
        // head equal to base: an empty range.
        let e = f.request("p", "HEAD~1").unwrap_err().to_string();
        assert!(e.contains("holds no commits"), "{e}");
        // A head that does not descend from base.
        git(&fx.repo, &["checkout", "-q", "--orphan", "other"]);
        write(&fx.repo, "x.txt", "x\n");
        let orphan = commit(&fx.repo, "orphan", false);
        git(&fx.repo, &["checkout", "-q", "main"]);
        let e = f.request("p", &orphan).unwrap_err().to_string();
        assert!(e.contains("not an ancestor"), "{e}");
        // An unsigned commit on top.
        write(&fx.repo, "ev/b.txt", "b\n");
        let unsigned = commit(&fx.repo, "unsigned", false);
        let e = f.request("p", &unsigned).unwrap_err().to_string();
        assert!(e.contains("no commit-review attestation"), "{e}");
        assert_eq!(kinds(&f), ["phase_registered"]);
    }

    #[test]
    fn a_second_request_while_one_is_pending_is_refused() {
        let fx = Fx::new();
        let f = fx.admin(); // admin: records the request without dispatching it
        fx.register(&f);
        f.request("p", &fx.head).unwrap();
        let e = f.request("p", &fx.head).unwrap_err().to_string();
        assert!(e.contains("still pending"), "{e}");
    }

    #[test]
    fn head_only_moves_forward() {
        let fx = Fx::new();
        let f = fx.service();
        fx.register(&f);
        f.request("p", &fx.head).unwrap();
        settled(&f, "p");
        // A sibling of head (not descending from it) is refused.
        git(&fx.repo, &["checkout", "-q", "-b", "side", &fx.base]);
        write(&fx.repo, "ev/c.txt", "c\n");
        let side = commit(&fx.repo, "side", true);
        let e = f.request("p", &side).unwrap_err().to_string();
        assert!(e.contains("does not move forward"), "{e}");
    }

    #[test]
    fn bad_results_are_recorded_as_failed_without_effect() {
        for (mode, want) in [
            (Mode::Answer("no verdict here".into()), "no VERDICT"),
            (Mode::WrongSha, "is not the sealed input"),
            (Mode::WrongJob, "is not the request"),
        ] {
            let fx = Fx::new();
            *fx.mode.lock().unwrap() = mode;
            let f = fx.service();
            fx.register(&f);
            f.request("p", &fx.head).unwrap();
            let st = settled(&f, "p");
            assert_eq!(st.current.as_ref().unwrap().state, RequestState::Failed);
            let snap = f.ledger().load().unwrap();
            let audit = snap.records.iter().find(|r| r.kind == "audit").unwrap();
            assert_eq!(audit.body["valid"], false);
            assert!(
                audit.body["reason"].as_str().unwrap().contains(want),
                "{}",
                audit.body
            );
            assert_eq!(st.count, 0, "an invalid audit does not count");
        }
        // The runner reports a timeout.
        let fx = Fx::new();
        *fx.mode.lock().unwrap() = Mode::Status("timed_out");
        let f = fx.service();
        fx.register(&f);
        f.request("p", &fx.head).unwrap();
        assert_eq!(
            settled(&f, "p").current.unwrap().state,
            RequestState::TimedOut
        );
        assert!(!kinds(&f).contains(&"audit".to_string()));
    }

    #[test]
    fn a_runner_that_is_not_the_expected_one_is_a_failed_job() {
        let fx = Fx::new();
        let mut cfg = fx.cfg();
        cfg.runner.uid += 1;
        let f = Flow::open_service(cfg, kp(), fx.reader()).unwrap();
        fx.register(&f);
        f.request("p", &fx.head).unwrap();
        let st = settled(&f, "p");
        assert_eq!(st.current.unwrap().state, RequestState::Failed);
        let snap = f.ledger().load().unwrap();
        let job = snap.records.iter().find(|r| r.kind == "job").unwrap();
        assert!(
            job.body["error"]
                .as_str()
                .unwrap()
                .contains("not the runner's"),
            "{}",
            job.body
        );
    }

    #[test]
    fn a_result_after_cancel_is_kept_as_late_without_effect() {
        let fx = Fx::new();
        let (tx, rx) = std::sync::mpsc::channel();
        *fx.mode.lock().unwrap() = Mode::Hold(Arc::new(Mutex::new(Some(rx))), approve());
        let f = fx.service();
        fx.register(&f);
        let id = f.request("p", &fx.head).unwrap();
        f.cancel("p", &id).unwrap();
        tx.send(()).unwrap();
        for _ in 0..200 {
            if kinds(&f).contains(&"job".to_string()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            kinds(&f),
            [
                "phase_registered",
                "request",
                "request_closed",
                "job",
                "audit"
            ],
            "late: job and audit archived, no resolution, no second closure"
        );
        let st = derive(&f.ledger().load().unwrap().records, "p", 0);
        assert_eq!(st.current.unwrap().state, RequestState::Cancelled);
        assert!(
            st.conditions.iter().all(|c| c.closed.is_none()),
            "nothing resolved"
        );
    }

    #[test]
    fn only_one_service_and_a_restart_closes_jobless_pending_requests() {
        let fx = Fx::new();
        let a = fx.admin();
        fx.register(&a);
        a.request("p", &fx.head).unwrap(); // recorded, never dispatched
        let s = fx.service();
        assert!(
            Flow::open_service(fx.cfg(), kp(), fx.reader()).is_err(),
            "a second service cannot start"
        );
        let st = derive(&s.ledger().load().unwrap().records, "p", 0);
        let c = st.current.unwrap();
        assert_eq!(c.state, RequestState::Failed);
        let snap = s.ledger().load().unwrap();
        assert_eq!(
            snap.records.last().unwrap().body["why"],
            "gate restarted before the reply"
        );
        // Admin access still works next to the service, and never closes requests itself.
        let b = fx.admin();
        b.status("p").unwrap();
    }

    #[test]
    fn an_admin_process_never_closes_a_running_request() {
        let fx = Fx::new();
        let (tx, rx) = std::sync::mpsc::channel();
        *fx.mode.lock().unwrap() = Mode::Hold(Arc::new(Mutex::new(Some(rx))), approve());
        let s = fx.service();
        fx.register(&s);
        s.request("p", &fx.head).unwrap();
        // While the job runs, an admin process reads and writes.
        let a = fx.admin();
        a.status("p").unwrap();
        a.waive("p", "p/R1", "not needed").unwrap();
        assert!(!kinds(&a).contains(&"request_closed".to_string()));
        tx.send(()).unwrap();
        assert_eq!(
            settled(&s, "p").current.unwrap().state,
            RequestState::Completed
        );
    }

    #[test]
    fn an_unreachable_remote_stops_requests() {
        let fx = Fx::new();
        let mut cfg = fx.cfg();
        cfg.remote_url = fx.root.join("no-such-remote").to_string_lossy().to_string();
        let f = Flow::open_admin(cfg, kp(), fx.reader()).unwrap();
        fx.register(&f);
        let e = f.request("p", &fx.head).unwrap_err().to_string();
        assert!(e.contains("release stopped"), "{e}");
        assert!(!f.status("p").unwrap().released);
    }

    #[test]
    fn the_request_line_must_fit_what_the_runner_reads() {
        let fx = Fx::new();
        // Just under the input limit, but nearly every byte is a newline, which JSON doubles.
        // n newlines appear about three times over in the input (the evidence, and "+" lines in
        // the diff): about 3n bytes, under MAX_INPUT; JSON escapes each, so the line is about 5n.
        let body = "\n".repeat(1_300_000);
        write(&fx.repo, "ev/big.txt", &body);
        let head = commit(&fx.repo, "big", true);
        let f = fx.admin();
        fx.register(&f);
        let e = f.request("p", &head).unwrap_err().to_string();
        assert!(e.contains("over what the runner reads"), "{e}");
        assert_eq!(kinds(&f), ["phase_registered"]);
    }

    /// Records a request without dispatching it, then the job result by hand.
    fn requested(fx: &Fx) -> (Arc<Flow>, String) {
        let f = fx.admin();
        fx.register(&f);
        let id = f.request("p", &fx.head).unwrap();
        (f, id)
    }

    fn completed_job(f: &Flow, id: &str, answer: &str) -> Value {
        let snap = f.ledger().load().unwrap();
        let req = find(&snap.records, "p", "request", id).unwrap();
        json!({ "request_id": id, "response": {
            "job_id": id, "status": "completed", "answer": answer,
            "input_sha256": req.body.pointer("/snapshot/input_sha256").unwrap() } })
    }

    /// Kinds and bodies, without what differs between two runs by design: the recovery mark
    /// and the random snapshot directory name.
    fn normalized(f: &Flow) -> Vec<(String, Value)> {
        f.ledger()
            .load()
            .unwrap()
            .records
            .iter()
            .map(|r| {
                let mut b = plain(&r.body);
                if let Some(snap) = b.get_mut("snapshot").and_then(Value::as_object_mut) {
                    snap.remove("dir");
                }
                (r.kind.clone(), b)
            })
            .collect()
    }

    #[test]
    fn a_crash_inside_the_result_group_is_completed_exactly() {
        let answer = "VERDICT: APPROVE WITH CONDITIONS\nCONDITIONS:\n- [new][non-blocking] tidy\nRESOLVED:\n- p/R1: done\n";
        // The group: job, audit, condition_resolved, request_closed.
        let reference = {
            let fx = Fx::new();
            let (f, id) = requested(&fx);
            f.complete("p", &id, completed_job(&f, &id, answer))
                .unwrap();
            normalized(&f)
        };
        assert_eq!(reference.len(), 6);
        for k in 0..4 {
            let fx = Fx::new();
            let (f, id) = requested(&fx);
            f.ledger().crash_after(k);
            assert!(
                f.complete("p", &id, completed_job(&f, &id, answer))
                    .is_err()
            );
            f.ledger().crash_after(usize::MAX);
            // Before recovery nothing releases.
            let st = derive(&f.ledger().load().unwrap().records, "p", u64::MAX);
            assert!(!st.released, "k={k}");
            let g = fx.admin();
            g.recover().unwrap();
            let got = normalized(&g);
            if k == 0 {
                // Nothing of the group was written: the request is still pending, as if the
                // result never came (the service closes it at its next start).
                assert_eq!(got, reference[..2], "k=0");
            } else {
                assert_eq!(got, reference, "k={k}");
            }
        }
    }

    #[test]
    fn records_that_are_not_what_the_job_calls_for_stop_recovery() {
        let fx = Fx::new();
        let (f, id) = requested(&fx);
        f.ledger().crash_after(1);
        let _ = f.complete("p", &id, completed_job(&f, &id, &approve()));
        f.ledger().crash_after(usize::MAX);
        // A wrong audit on file, as if written by something else.
        f.ledger()
            .append(&kp(), "t", draft("audit", "p", json!({"request_id": id, "valid": true, "verdict": "BLOCK", "conditions": [], "resolved": []})))
            .unwrap();
        let e = f.recover().unwrap_err().to_string();
        assert!(e.contains("recovery mismatch"), "{e}");
        assert!(
            f.status("p")
                .unwrap()
                .reasons
                .iter()
                .any(|r| r.contains("recovery mismatch"))
        );
    }

    #[test]
    fn superseding_cancels_the_old_request_and_freezes_what_is_inherited() {
        let fx = Fx::new();
        let (f, id) = requested(&fx); // p has a Pending request
        f.apply(
            "p2",
            &fx.base,
            "plans/book.md",
            "ev",
            vec![],
            Some("p".into()),
        )
        .unwrap();
        let w = f.confirm("p2").unwrap();
        assert_eq!(
            w.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
            ["phase_registered", "phase_superseded", "request_closed"]
        );
        let inherited = derive(&f.ledger().load().unwrap().records, "p2", 0);
        // The old request's result arrives late: archived, without effect on p2.
        f.complete("p", &id, completed_job(&f, &id, &approve()))
            .unwrap();
        let after = derive(&f.ledger().load().unwrap().records, "p2", 0);
        assert_eq!(after.conditions, inherited.conditions);
        assert_eq!(after.count, inherited.count);
        assert!(!kinds(&f).contains(&"condition_resolved".to_string()));
        // Admin steps on the superseded phase are refused.
        let e = f.waive("p", "p/R1", "x").unwrap_err().to_string();
        assert!(e.contains("superseded by p2"), "{e}");
    }

    #[test]
    fn a_crash_inside_the_confirm_group_is_completed() {
        for k in 1..3 {
            let fx = Fx::new();
            let (f, _) = requested(&fx);
            f.apply(
                "p2",
                &fx.base,
                "plans/book.md",
                "ev",
                vec![],
                Some("p".into()),
            )
            .unwrap();
            f.ledger().crash_after(k);
            assert!(f.confirm("p2").is_err());
            f.ledger().crash_after(usize::MAX);
            // Something unrelated lands after the crash and before recovery: recovery must still
            // see the confirm group as unfinished.
            f.ledger()
                .append(&kp(), "t", draft("note", "", json!({"after": "crash"})))
                .unwrap();
            fx.admin().recover().unwrap();
            let ks = kinds(&f);
            let count = |k: &str| ks.iter().filter(|x| *x == k).count();
            assert_eq!(
                (count("phase_superseded"), count("request_closed")),
                (1, 1),
                "k={k}: each completed once: {ks:?}"
            );
            let st = derive(&f.ledger().load().unwrap().records, "p", 0);
            assert_eq!(st.superseded_by.as_deref(), Some("p2"));
            assert_eq!(st.current.unwrap().state, RequestState::Cancelled);
        }
    }

    #[test]
    fn admin_steps_check_their_preconditions() {
        let fx = Fx::new();
        let f = fx.admin();
        fx.register(&f);
        assert!(f.waive("p", "p/R9", "x").is_err(), "no such condition");
        assert!(f.resume("p", "x").is_err(), "not paused");
        assert!(
            f.link_task("p", "nope", "x").is_err(),
            "no such bridge task"
        );
        f.waive("p", "p/R1", "covered elsewhere").unwrap();
        assert!(f.waive("p", "p/R1", "again").is_err(), "already closed");
        let mut v = Vendors::first();
        assert!(f.vendors_set(&v, "same version").is_err());
        v.version = 2;
        f.vendors_set(&v, "next").unwrap();
        // The new table is used from now on (recorded on the chain).
        assert_eq!(
            vendors(&f.ledger().load().unwrap().records)
                .unwrap()
                .version,
            2
        );
        // A rebaseline computes the hash itself.
        write(&fx.repo, "plans/book.md", "# Task book v2\n");
        let c = commit(&fx.repo, "book v2", true);
        let r = f.rebaseline("p", &c, "revised").unwrap();
        assert_eq!(r.body["task_book_sha256"], sha256_hex(b"# Task book v2\n"));
    }

    #[test]
    fn the_state_directory_must_be_absolute() {
        let fx = Fx::new();
        let mut cfg = fx.cfg();
        cfg.dir = PathBuf::from("state");
        let e = Flow::open_admin(cfg, kp(), fx.reader())
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("must be absolute"), "{e}");
    }

    #[test]
    fn request_ids_fit_the_runner() {
        assert!(valid_job_id("p-q1"));
        assert!(!valid_job_id(&"a".repeat(65)));
        assert!(!valid_job_id("a/b"));
        let fx = Fx::new();
        let f = fx.admin();
        assert!(
            f.apply(
                &"x".repeat(57),
                &fx.base,
                "plans/book.md",
                "ev",
                vec![],
                None
            )
            .is_err()
        );
    }

    #[test]
    fn a_log_that_changed_while_preparing_is_prepared_again() {
        let fx = Fx::new();
        let f = fx.admin();
        fx.register(&f);
        // The bridge read (during preparation) lets another process write a record first.
        let other = fx.admin();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (r2, b2) = (reads.clone(), fx.bridge.clone());
        let reader: BridgeRead = Arc::new(move || {
            if r2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                other
                    .drop_source("p", "nothing", "written mid-preparation")
                    .unwrap();
            }
            b2.lock().unwrap().clone()
        });
        let g = Flow::open_admin(fx.cfg(), kp(), reader).unwrap();
        g.request("p", &fx.head).unwrap();
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "prepared twice"
        );
        assert_eq!(kinds(&g), ["phase_registered", "source_dropped", "request"]);
        // A log that changes on every round: refused after ATTEMPTS rounds, nothing recorded.
        let fx = Fx::new();
        let f = fx.admin();
        fx.register(&f);
        let other = fx.admin();
        let b2 = fx.bridge.clone();
        let reader: BridgeRead = Arc::new(move || {
            other.drop_source("p", "nothing", "again").unwrap();
            b2.lock().unwrap().clone()
        });
        let g = Flow::open_admin(fx.cfg(), kp(), reader).unwrap();
        let e = g.request("p", &fx.head).unwrap_err().to_string();
        assert!(e.contains("kept changing"), "{e}");
        assert!(!kinds(&g).contains(&"request".to_string()));
    }

    #[test]
    fn a_changed_snapshot_is_never_sent() {
        let fx = Fx::new();
        let (f, id) = requested(&fx);
        let snap = f.ledger().load().unwrap();
        let req = find(&snap.records, "p", "request", &id).unwrap();
        let dir = req.body.pointer("/snapshot/dir").unwrap().as_str().unwrap();
        let path = fx.root.join("state/inputs").join(dir).join("input.txt");
        let mut b = fs::read(&path).unwrap();
        b.push(b'!');
        fs::write(&path, b).unwrap();
        let e = f.run_job("p", &id, "{}").unwrap_err();
        assert_eq!(e, "snapshot changed");
    }

    #[test]
    fn one_recovery_completes_a_confirm_group_and_a_job_group_together() {
        // Two groups cut short with nothing in between (no operation ran recovery): p's job
        // result (only the job on file) and p2's confirm group superseding p3 (only the
        // registration on file). One recover() call completes both.
        let fx = Fx::new();
        let f = fx.admin();
        f.apply("p3", &fx.base, "plans/book.md", "ev", vec![], None)
            .unwrap();
        f.confirm("p3").unwrap();
        fx.register(&f);
        let id = f.request("p", &fx.head).unwrap();
        f.ledger().crash_after(1);
        assert!(
            f.complete("p", &id, completed_job(&f, &id, &approve()))
                .is_err()
        );
        f.ledger().crash_after(usize::MAX);
        f.ledger()
            .append(
                &kp(),
                "t",
                draft("phase_registered", "p2", json!({"supersedes": "p3", "conditions": [],
                    "base": fx.base, "task_book": "plans/book.md", "task_book_sha256": "x", "evidence_dir": "ev"})),
            )
            .unwrap();
        assert_eq!(kinds(&f).last().unwrap(), "phase_registered");
        fx.admin().recover().unwrap();
        let ks = kinds(&f);
        assert!(ks.contains(&"phase_superseded".to_string()), "{ks:?}");
        assert_eq!(
            derive(&f.ledger().load().unwrap().records, "p", 0)
                .current
                .unwrap()
                .state,
            RequestState::Completed,
            "{ks:?}"
        );
    }

    #[test]
    fn a_resolution_without_its_audit_stops_recovery() {
        let fx = Fx::new();
        let (f, id) = requested(&fx);
        f.ledger().crash_after(1);
        let _ = f.complete("p", &id, completed_job(&f, &id, &approve()));
        f.ledger().crash_after(usize::MAX);
        f.ledger()
            .append(
                &kp(),
                "t",
                draft(
                    "condition_resolved",
                    "p",
                    json!({"condition": "p/R1", "audit_seq": 99, "basis": "stray"}),
                ),
            )
            .unwrap();
        let e = f.recover().unwrap_err().to_string();
        assert!(e.contains("recovery mismatch"), "{e}");
    }

    #[test]
    fn confirm_checks_the_task_book_again() {
        let fx = Fx::new();
        let f = fx.admin();
        f.apply("p", &fx.base, "plans/book.md", "ev", vec![], None)
            .unwrap();
        // The application file is edited after apply: confirm refuses it.
        let path = fx.root.join("state/pending/p.json");
        let mut a: Application = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        a.task_book_sha256 = "0".repeat(64);
        fs::write(&path, serde_json::to_vec(&a).unwrap()).unwrap();
        let e = f.confirm("p").unwrap_err().to_string();
        assert!(e.contains("not what the application recorded"), "{e}");
        assert!(kinds(&f).is_empty());
    }

    #[test]
    fn archive_never_goes_past_what_the_remote_holds() {
        // Remote and local equal at N: archived N.
        let fx = Fx::new();
        let f = fx.admin();
        fx.register(&f);
        fx.publish(&f);
        let n = f.ledger().load().unwrap().records.len() as u64;
        assert_eq!(f.archive().unwrap(), Some(n));
        // The local log grows (a result, not published): the remote is behind, and archiving
        // stays at what the remote holds.
        let id = f.request("p", &fx.head).unwrap();
        f.complete("p", &id, completed_job(&f, &id, &approve()))
            .unwrap();
        let m = f.ledger().load().unwrap().records.len() as u64;
        assert!(m > n + 1);
        assert_eq!(f.archive().unwrap(), None, "nothing new on the remote");
        assert_eq!(f.ledger().load().unwrap().checkpoint, n);
        // Not released while the result is unarchived.
        assert!(!f.status("p").unwrap().released);
        // Published, then archived to the remote's length exactly.
        fx.publish(&f);
        let published = f.ledger().load().unwrap().records.len() as u64;
        assert_eq!(f.archive().unwrap(), Some(published));
        assert!(f.status("p").unwrap().released);
    }

    #[test]
    fn archive_imports_a_longer_remote_and_stops_at_its_length() {
        let fx = Fx::new();
        let f = fx.admin();
        fx.register(&f);
        f.drop_source("p", "x", "a second record").unwrap();
        fx.publish(&f);
        let full = fs::read_to_string(fx.root.join("state/log.jsonl")).unwrap();
        // The local log is set back to its first record (as if restored from an old copy).
        let first = full.lines().next().unwrap().to_string() + "\n";
        fs::write(fx.root.join("state/log.jsonl"), first).unwrap();
        let g = fx.admin();
        assert_eq!(
            g.archive().unwrap(),
            Some(2),
            "imported, then archived to the remote's 2"
        );
        assert_eq!(
            fs::read_to_string(fx.root.join("state/log.jsonl"))
                .unwrap()
                .lines()
                .count(),
            3
        );
    }

    /// A copy of the local log with one more signed record, published to the remote, so the
    /// remote is ahead of the local log without the local log changing.
    fn publish_remote_only(fx: &Fx, f: &Flow, kind: &str, phase: &str, body: Value) {
        let other = tempfile::tempdir().unwrap();
        fs::copy(
            fx.root.join("state/log.jsonl"),
            other.path().join("log.jsonl"),
        )
        .unwrap();
        let l = Ledger::new(other.path(), "repo", kp().verifying_key()).unwrap();
        l.append(&kp(), "t", draft(kind, phase, body)).unwrap();
        let log = fs::read_to_string(other.path().join("log.jsonl")).unwrap();
        let _ = f;
        sync::publish(
            &fx.repo,
            &fx.cfg().remote_url,
            &log,
            &kp().verifying_key(),
            "repo",
        )
        .unwrap();
    }

    #[test]
    fn a_supersession_published_while_preparing_is_seen_before_committing() {
        let fx = Fx::new();
        let f = fx.admin();
        fx.register(&f);
        fx.publish(&f);
        let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (o2, b2) = (once.clone(), fx.bridge.clone());
        let fxr: &'static Fx = Box::leak(Box::new(Fx {
            _dir: tempfile::tempdir().unwrap(),
            root: fx.root.clone(),
            repo: fx.repo.clone(),
            remote: fx.remote.clone(),
            base: fx.base.clone(),
            head: fx.head.clone(),
            mode: fx.mode.clone(),
            calls: fx.calls.clone(),
            lines: fx.lines.clone(),
            sent: fx.sent.clone(),
            bridge: fx.bridge.clone(),
        }));
        let reader: BridgeRead = Arc::new(move || {
            if !o2.swap(true, std::sync::atomic::Ordering::SeqCst) {
                // During preparation: p is superseded on the remote only.
                let probe =
                    Flow::open_admin(fxr.cfg(), kp(), Arc::new(|| Err("unused".into()))).unwrap();
                publish_remote_only(
                    fxr,
                    &probe,
                    "phase_registered",
                    "p2",
                    json!({"supersedes": "p", "conditions": [], "base": fxr.base, "task_book": "plans/book.md", "task_book_sha256": "x", "evidence_dir": "ev"}),
                );
            }
            b2.lock().unwrap().clone()
        });
        let g = Flow::open_admin(fx.cfg(), kp(), reader).unwrap();
        let e = g.request("p", &fx.head).unwrap_err().to_string();
        assert!(e.contains("superseded by p2"), "{e}");
        assert!(
            !kinds(&g).contains(&"request".to_string()),
            "nothing recorded"
        );
    }

    #[test]
    fn a_remote_lost_while_preparing_refuses_the_request() {
        let fx = Fx::new();
        let f = fx.admin();
        fx.register(&f);
        fx.publish(&f);
        let (remote, b2) = (fx.remote.clone(), fx.bridge.clone());
        let reader: BridgeRead = Arc::new(move || {
            let _ = fs::rename(&remote, remote.with_extension("gone"));
            b2.lock().unwrap().clone()
        });
        let g = Flow::open_admin(fx.cfg(), kp(), reader).unwrap();
        let e = g.request("p", &fx.head).unwrap_err().to_string();
        assert!(e.contains("release stopped"), "{e}");
        assert!(!kinds(&g).contains(&"request".to_string()));
    }

    #[test]
    fn a_result_that_cannot_be_written_halts_the_service() {
        let fx = Fx::new();
        let (tx, rx) = std::sync::mpsc::channel();
        *fx.mode.lock().unwrap() = Mode::Hold(Arc::new(Mutex::new(Some(rx))), approve());
        let s = fx.service();
        fx.register(&s);
        s.request("p", &fx.head).unwrap();
        // The request is recorded; writing its result is what fails.
        s.ledger().crash_after(0);
        tx.send(()).unwrap();
        for _ in 0..200 {
            if s.halted().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        s.ledger().crash_after(usize::MAX);
        assert!(s.halted().unwrap().contains("injected crash"));
        let e = s.request("p", &fx.head).unwrap_err().to_string();
        assert!(e.contains("service halted"), "{e}");
        let st = s.status("p").unwrap();
        assert!(!st.released && st.reasons.iter().any(|r| r.contains("service halted")));
        // The next service recovers from what is on file: nothing of the result was written,
        // so the request is closed as failed.
        // The job thread holds the service until it ends; the owner lock goes with the last one.
        for _ in 0..200 {
            if Arc::strong_count(&s) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(Arc::strong_count(&s), 1, "the job thread ended");
        drop(s);
        // flock follows the open file: a process forked at this moment by a parallel test
        // (git) briefly holds a copy of the descriptor until it execs, so the lock can take a
        // moment to be free here. In production the old service has exited first.
        let mut t = None;
        for _ in 0..50 {
            match Flow::open_service(fx.cfg(), kp(), fx.reader()) {
                Ok(f) => {
                    t = Some(f);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let t = t.expect("the next service starts once the old one is gone");
        let st = derive(&t.ledger().load().unwrap().records, "p", 0);
        assert_eq!(st.current.unwrap().state, RequestState::Failed);
        assert!(t.halted().is_none());
    }

    #[test]
    fn a_changed_manifest_is_never_sent() {
        let fx = Fx::new();
        let (f, id) = requested(&fx);
        let snap = f.ledger().load().unwrap();
        let req = find(&snap.records, "p", "request", &id).unwrap();
        let dir = req.body.pointer("/snapshot/dir").unwrap().as_str().unwrap();
        let path = fx.root.join("state/inputs").join(dir).join("manifest.json");
        let mut b = fs::read(&path).unwrap();
        b.push(b' ');
        fs::write(&path, b).unwrap();
        assert_eq!(f.run_job("p", &id, "{}").unwrap_err(), "snapshot changed");
    }

    #[test]
    fn a_second_result_for_the_same_job_is_refused() {
        let fx = Fx::new();
        let (f, id) = requested(&fx);
        let job = completed_job(&f, &id, &approve());
        // Twice at once from two threads: one is recorded, the other refused.
        let (a, b) = (f.clone(), f.clone());
        let (ja, jb) = (job.clone(), job.clone());
        let (ia, ib) = (id.clone(), id.clone());
        let t1 = std::thread::spawn(move || a.complete("p", &ia, ja).is_ok());
        let t2 = std::thread::spawn(move || b.complete("p", &ib, jb).is_ok());
        let oks = [t1.join().unwrap(), t2.join().unwrap()];
        assert_eq!(oks.iter().filter(|x| **x).count(), 1, "{oks:?}");
        assert_eq!(kinds(&f).iter().filter(|k| *k == "job").count(), 1);
        let e = f.complete("p", &id, job).unwrap_err().to_string();
        assert!(e.contains("already recorded"), "{e}");
    }

    #[test]
    fn confirm_refuses_a_chain_too_deep() {
        let fx = Fx::new();
        let f = fx.admin();
        f.apply("q0", &fx.base, "plans/book.md", "ev", vec![], None)
            .unwrap();
        f.confirm("q0").unwrap();
        for k in 1..=MAX_SUPERSESSION {
            f.apply(
                &format!("q{k}"),
                &fx.base,
                "plans/book.md",
                "ev",
                vec![],
                Some(format!("q{}", k - 1)),
            )
            .unwrap();
            f.confirm(&format!("q{k}")).unwrap();
        }
        let next = MAX_SUPERSESSION + 1;
        f.apply(
            &format!("q{next}"),
            &fx.base,
            "plans/book.md",
            "ev",
            vec![],
            Some(format!("q{}", next - 1)),
        )
        .unwrap();
        let e = f.confirm(&format!("q{next}")).unwrap_err().to_string();
        assert!(e.contains("more than"), "{e}");
    }

    /// A second registered phase, for requests from two phases at once.
    fn register_b(fx: &Fx, f: &Flow) {
        f.apply("pb", &fx.base, "plans/book.md", "ev", vec![], None)
            .unwrap();
        f.confirm("pb").unwrap();
    }

    #[test]
    fn a_halt_during_preparation_admits_no_new_job() {
        // pb's request is being prepared (in its bridge read) when the service halts.
        let fx = Fx::new();
        let me: Arc<std::sync::OnceLock<std::sync::Weak<Flow>>> = Arc::default();
        let (m2, b2) = (me.clone(), fx.bridge.clone());
        let reader: BridgeRead = Arc::new(move || {
            if let Some(f) = m2.get().and_then(std::sync::Weak::upgrade) {
                f.halt("test: a result could not be written".into());
            }
            b2.lock().unwrap().clone()
        });
        let s = Flow::open_service(fx.cfg(), kp(), reader).unwrap();
        register_b(&fx, &s);
        me.set(Arc::downgrade(&s)).unwrap();
        let e = s.request("pb", &fx.head).unwrap_err().to_string();
        assert!(e.contains("service halted"), "{e}");
        assert!(
            !kinds(&s).contains(&"request".to_string()),
            "no request after the halt"
        );
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            fx.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the runner was not called"
        );
        assert!(!s.status("pb").unwrap().released);
    }

    #[test]
    fn a_real_write_failure_stops_a_request_prepared_meanwhile() {
        // p's job is running; pb's request waits in its bridge read; p's result cannot be
        // written (nothing of it lands), the service halts; then pb goes on, and is refused.
        let fx = Fx::new();
        let (tx, rx) = std::sync::mpsc::channel();
        *fx.mode.lock().unwrap() = Mode::Hold(Arc::new(Mutex::new(Some(rx))), approve());
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (g2, a2, b2) = (gate.clone(), armed.clone(), fx.bridge.clone());
        let reader: BridgeRead = Arc::new(move || {
            if a2.load(std::sync::atomic::Ordering::SeqCst) {
                let (m, c) = &*g2;
                let mut open = m.lock().unwrap();
                while !*open {
                    open = c.wait(open).unwrap();
                }
            }
            b2.lock().unwrap().clone()
        });
        let s = Flow::open_service(fx.cfg(), kp(), reader).unwrap();
        fx.register(&s);
        register_b(&fx, &s);
        s.request("p", &fx.head).unwrap();
        armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let s2 = s.clone();
        let head = fx.head.clone();
        let pb = std::thread::spawn(move || s2.request("pb", &head));
        // p's result fails to be written: the service halts.
        std::thread::sleep(Duration::from_millis(200));
        s.ledger().crash_after(0);
        tx.send(()).unwrap();
        for _ in 0..200 {
            if s.halted().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(s.halted().is_some());
        s.ledger().crash_after(usize::MAX);
        // pb continues.
        {
            let (m, c) = &*gate;
            *m.lock().unwrap() = true;
            c.notify_all();
        }
        let e = pb.join().unwrap().unwrap_err().to_string();
        assert!(e.contains("service halted"), "{e}");
        let records = s.ledger().load().unwrap().records;
        assert!(
            !records
                .iter()
                .any(|r| r.phase == "pb" && r.kind == "request")
        );
        assert_eq!(
            fx.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only p's job went out"
        );
    }

    #[test]
    fn a_committed_job_not_yet_sent_when_halted_is_not_sent() {
        let fx = Fx::new();
        let a = fx.admin();
        fx.register(&a);
        let id = a.request("p", &fx.head).unwrap(); // recorded, not dispatched
        let s = Flow::open_admin(fx.cfg(), kp(), fx.reader()).unwrap();
        s.halt("test".into());
        s.dispatch("p", &id, "{}".into());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(fx.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            !kinds(&s).contains(&"job".to_string()),
            "nothing recorded for it"
        );
        let st = derive(&s.ledger().load().unwrap().records, "p", 0);
        assert_eq!(
            st.current.unwrap().state,
            RequestState::Pending,
            "left for the next start"
        );
    }

    type Hook = Option<Arc<dyn Fn() + Send + std::marker::Sync>>;

    /// A one-way signal between test threads.
    #[derive(Default)]
    struct Flag(Mutex<bool>, std::sync::Condvar);

    impl Flag {
        fn set(&self) {
            *self.0.lock().unwrap() = true;
            self.1.notify_all();
        }
        /// Whether the flag was set within `t`.
        fn wait(&self, t: Duration) -> bool {
            let g = self.0.lock().unwrap();
            *self.1.wait_timeout_while(g, t, |s| !*s).unwrap().0
        }
    }

    /// A thread that halts the flow, with signals for "about to call halt()" and "halt()
    /// returned" (Codex n24: orderings are arranged by signals, not by sleeping).
    #[derive(Default)]
    struct Halter {
        entered: Flag,
        done: Flag,
        thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    impl Halter {
        /// The first call starts the halting thread and waits until it is about to halt. With
        /// `blocked`, halt() must still not have returned 300 ms later (the caller holds the
        /// admission lock, so it cannot); without, it must return.
        fn hook(
            me: Arc<Self>,
            flow: Arc<std::sync::OnceLock<std::sync::Weak<Flow>>>,
            blocked: bool,
        ) -> Hook {
            Some(Arc::new(move || {
                let Some(f) = flow.get().and_then(std::sync::Weak::upgrade) else {
                    return;
                };
                let mut t = me.thread.lock().unwrap();
                if t.is_some() {
                    return;
                }
                let h = me.clone();
                *t = Some(std::thread::spawn(move || {
                    h.entered.set();
                    f.halt("test: halted".into());
                    h.done.set();
                }));
                drop(t);
                assert!(me.entered.wait(Duration::from_secs(10)), "halter started");
                if blocked {
                    assert!(
                        !me.done.wait(Duration::from_millis(300)),
                        "halt() must wait for the admission lock"
                    );
                } else {
                    assert!(me.done.wait(Duration::from_secs(10)), "halt() returned");
                }
            }))
        }

        /// A hook that holds its thread until halt() has returned.
        fn after_halt(me: Arc<Self>) -> Hook {
            Some(Arc::new(move || {
                assert!(me.done.wait(Duration::from_secs(10)), "halt() returned");
            }))
        }

        fn join(&self) {
            assert!(self.done.wait(Duration::from_secs(10)), "halt() returned");
            let t = self.thread.lock().unwrap().take().expect("the hook ran");
            t.join().unwrap();
        }
    }

    /// Waits until no job thread of this flow is running (each removes its id on the way out).
    fn jobs_finished(s: &Flow) {
        for _ in 0..500 {
            if s.running.lock().unwrap().is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("a job thread did not finish");
    }

    #[test]
    fn a_halt_just_before_the_send_keeps_the_job_from_going_out() {
        let fx = Fx::new();
        let me: Arc<std::sync::OnceLock<std::sync::Weak<Flow>>> = Arc::default();
        let halter = Arc::new(Halter::default());
        let s = Flow::open_service(fx.cfg(), kp(), fx.reader()).unwrap();
        fx.register(&s);
        me.set(Arc::downgrade(&s)).unwrap();
        s.hooks.lock().unwrap().before_send = Halter::hook(halter.clone(), me, false);
        s.request("p", &fx.head).unwrap();
        halter.join();
        jobs_finished(&s);
        assert!(s.halted().is_some());
        assert_eq!(
            fx.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "not sent"
        );
        assert!(
            !kinds(&s).contains(&"job".to_string()),
            "nothing recorded for it"
        );
        let st = derive(&s.ledger().load().unwrap().records, "p", 0);
        assert_eq!(st.current.unwrap().state, RequestState::Pending);
    }

    #[test]
    fn a_halt_while_the_line_is_written_waits_for_the_send() {
        let fx = Fx::new();
        let me: Arc<std::sync::OnceLock<std::sync::Weak<Flow>>> = Arc::default();
        let halter = Arc::new(Halter::default());
        let s = Flow::open_service(fx.cfg(), kp(), fx.reader()).unwrap();
        fx.register(&s);
        me.set(Arc::downgrade(&s)).unwrap();
        s.hooks.lock().unwrap().while_sending = Halter::hook(halter.clone(), me, true);
        s.request("p", &fx.head).unwrap();
        halter.join();
        assert!(s.halted().is_some());
        for _ in 0..500 {
            if fx.calls.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fx.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the line was written before the halt took effect"
        );
    }

    #[test]
    fn a_halt_during_the_request_commit_waits_and_the_job_is_not_sent() {
        let fx = Fx::new();
        let me: Arc<std::sync::OnceLock<std::sync::Weak<Flow>>> = Arc::default();
        let halter = Arc::new(Halter::default());
        let s = Flow::open_service(fx.cfg(), kp(), fx.reader()).unwrap();
        fx.register(&s);
        me.set(Arc::downgrade(&s)).unwrap();
        {
            let mut h = s.hooks.lock().unwrap();
            h.before_commit = Halter::hook(halter.clone(), me, true);
            // The job thread goes for the admission lock only once halt() has had it: the
            // Mutex is not FIFO, so without this the job could win it first (Codex n24).
            h.before_send = Halter::after_halt(halter.clone());
        }
        s.request("p", &fx.head).unwrap();
        halter.join();
        jobs_finished(&s);
        assert!(s.halted().is_some());
        assert!(
            kinds(&s).contains(&"request".to_string()),
            "the request was committed"
        );
        assert_eq!(
            fx.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "not sent"
        );
        assert!(!kinds(&s).contains(&"job".to_string()));
    }

    #[test]
    fn wait_halted_returns_once_halted_even_if_halted_before_waiting() {
        let fx = Fx::new();
        let s = Flow::open_service(fx.cfg(), kp(), fx.reader()).unwrap();
        // The waiter signals while it still holds the latch's lock, so halt() (which needs that
        // lock) can only set the latch once the waiter is inside the wait: only the
        // notification can wake it.
        let waiting = Arc::new(Flag::default());
        let w2 = waiting.clone();
        s.hooks.lock().unwrap().waiting = Some(Arc::new(move || w2.set()));
        let s2 = s.clone();
        let t = std::thread::spawn(move || s2.wait_halted());
        assert!(
            waiting.wait(Duration::from_secs(10)),
            "the waiter is waiting"
        );
        s.hooks.lock().unwrap().waiting = None;
        s.halt("first".into());
        assert_eq!(t.join().unwrap(), "first");
        s.halt("second".into());
        assert_eq!(s.wait_halted(), "first", "the first reason is kept");
    }

    /// The HTTP routes over a real flow (6c).
    mod routes {
        use super::*;
        use crate::gate::Gate;
        use crate::service::{AppState, serve};
        use crate::store::Store;

        const TOKEN: &str = "test-token-0123456789abcdef0123456789";

        async fn start(flow: Option<Arc<Flow>>, root: &Path) -> String {
            let gate = Gate::new(Store::open(root.join("gate-state")).unwrap()).with_signer(kp());
            let mut app = AppState::new(
                gate,
                crate::reviewer::tests::cfg("http://127.0.0.1:9"),
                TOKEN.into(),
            );
            if let Some(f) = flow {
                app = app.with_milestones(f);
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(serve(Arc::new(app), listener));
            url
        }

        async fn call(url: &str, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
            let http = reqwest::Client::new();
            let mut r = http
                .request(method.parse().unwrap(), format!("{url}{path}"))
                .bearer_auth(TOKEN);
            if let Some(b) = body {
                r = r.json(&b);
            }
            let resp = r.send().await.unwrap();
            let code = resp.status().as_u16();
            let text = resp.text().await.unwrap();
            (
                code,
                serde_json::from_str(&text).unwrap_or(Value::String(text)),
            )
        }

        fn last(f: &Flow) -> (u64, String) {
            let r = f.ledger().load().unwrap().records.last().cloned().unwrap();
            (r.seq, r.hash)
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn each_route_answers_on_a_real_flow() {
            let fx = Fx::new();
            let f = fx.service();
            let url = start(Some(f.clone()), &fx.root).await;
            let (c, v) = call(
                &url,
                "POST",
                "/milestones",
                Some(json!({
                "phase": "p", "base": fx.base, "task_book": "plans/book.md",
                "evidence_dir": "ev", "conditions": ["accept"] })),
            )
            .await;
            assert_eq!(c, 200, "{v}");
            assert_eq!(v["task_book"], "plans/book.md");
            let f2 = f.clone();
            tokio::task::spawn_blocking(move || f2.confirm("p").unwrap())
                .await
                .unwrap();
            // Refused: already registered.
            let (c, v) = call(
                &url,
                "POST",
                "/milestones",
                Some(json!({
                "phase": "p", "base": fx.base, "task_book": "plans/book.md",
                "evidence_dir": "ev" })),
            )
            .await;
            assert_eq!(c, 409, "{v}");
            assert!(
                v["refused"][0]
                    .as_str()
                    .unwrap()
                    .contains("already registered"),
                "{v}"
            );
            let (c, v) = call(
                &url,
                "POST",
                "/milestones/p/requests",
                Some(json!({ "head": fx.head })),
            )
            .await;
            assert_eq!((c, v["request_id"].as_str()), (200, Some("p-q1")), "{v}");
            let f2 = f.clone();
            tokio::task::spawn_blocking(move || settled(&f2, "p"))
                .await
                .unwrap();
            // Done, but not archived: 200 with released false (not a refusal).
            let (c, v) = call(&url, "GET", "/milestones/p", None).await;
            assert_eq!((c, v["released"].as_bool()), (200, Some(false)), "{v}");
            assert!(!v["reasons"].as_array().unwrap().is_empty());
            // The log is the signed text the flow renders.
            let (c, log) = call(&url, "GET", "/milestones/log", None).await;
            assert_eq!(c, 200);
            assert_eq!(log, Value::String(f.log().unwrap()));
            // Publish as the implementer, then report: archived.
            fx.publish(&f);
            let (seq, hash) = last(&f);
            let (c, v) = call(
                &url,
                "POST",
                "/milestones/published",
                Some(json!({ "seq": seq, "hash": hash })),
            )
            .await;
            assert_eq!(c, 200, "{v}");
            assert_eq!(
                (v["published"].as_bool(), v["archived"].as_bool()),
                (Some(true), Some(true)),
                "{v}"
            );
            // Reporting the same (already archived) position again is still archived.
            let (c, v) = call(
                &url,
                "POST",
                "/milestones/published",
                Some(json!({ "seq": seq, "hash": hash })),
            )
            .await;
            assert_eq!((c, v["archived"].as_bool()), (200, Some(true)), "{v}");
            let (c, v) = call(&url, "GET", "/milestones/p", None).await;
            assert_eq!((c, v["released"].as_bool()), (200, Some(true)), "{v}");
            // A position not in the log: refused.
            let (c, _) = call(
                &url,
                "POST",
                "/milestones/published",
                Some(json!({ "seq": 1, "hash": "00" })),
            )
            .await;
            assert_eq!(c, 409);
            // Cancel of a request that is not pending: refused; malformed ids: 400.
            let (c, _) = call(&url, "POST", "/milestones/p/requests/p-q1/cancel", None).await;
            assert_eq!(c, 409);
            let (c, _) = call(&url, "POST", "/milestones/p/requests/a%20b/cancel", None).await;
            assert_eq!(c, 400);
            let (c, _) = call(&url, "GET", "/milestones/Bad%20Phase", None).await;
            assert_eq!(c, 400);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn published_but_not_on_the_remote_is_a_partial_success() {
            let fx = Fx::new();
            let f = fx.service();
            fx.register(&f);
            let url = start(Some(f.clone()), &fx.root).await;
            let (seq, hash) = last(&f);
            // Reported without pushing: published is written, archived is false, with a reason.
            let (c, v) = call(
                &url,
                "POST",
                "/milestones/published",
                Some(json!({ "seq": seq, "hash": hash })),
            )
            .await;
            assert_eq!(c, 200, "{v}");
            assert_eq!(
                (v["published"].as_bool(), v["archived"].as_bool()),
                (Some(true), Some(false)),
                "{v}"
            );
            assert!(v["archive_error"].is_string(), "{v}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn publish_checks_the_pinned_key_before_writing_anything() {
            let fx = Fx::new();
            let f = fx.service();
            fx.register(&f);
            let url = start(Some(f.clone()), &fx.root).await;
            let c = crate::client::Client::new(&url, TOKEN);
            let remote = fx.cfg().remote_url;
            let notes = |dir: &Path| {
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(["for-each-ref", "refs/notes/"])
                    .output()
                    .unwrap()
                    .stdout
            };
            let (local0, remote0) = (notes(&fx.repo), notes(&fx.remote));
            let wrong = Keypair::from_seed([0xA5; 32]).verifying_key();
            let e = crate::milestone::cli::publish(&c, &fx.repo, &remote, &wrong, "repo")
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains("not the pinned key"), "{e}");
            assert_eq!(
                (notes(&fx.repo), notes(&fx.remote)),
                (local0, remote0),
                "nothing written"
            );
            let o = crate::milestone::cli::publish(
                &c,
                &fx.repo,
                &remote,
                &kp().verifying_key(),
                "repo",
            )
            .await
            .unwrap();
            assert!(o.published && o.archived, "{o:?}");
            assert_ne!(notes(&fx.remote), Vec::<u8>::new(), "pushed");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn without_milestones_404_once_authenticated_and_401_before() {
            let d = tempfile::tempdir().unwrap();
            let url = start(None, d.path()).await;
            let (c, _) = call(&url, "GET", "/milestones/p", None).await;
            assert_eq!(c, 404, "authenticated, milestones off");
            let r = reqwest::get(format!("{url}/milestones/p")).await.unwrap();
            assert_eq!(r.status().as_u16(), 401, "no token: 401 first");
            let fx = Fx::new();
            let url = start(Some(fx.service()), &fx.root).await;
            let r = reqwest::get(format!("{url}/milestones/log")).await.unwrap();
            assert_eq!(r.status().as_u16(), 401, "no token, milestones on");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_halted_service_answers_503_on_every_route() {
            let fx = Fx::new();
            let f = fx.service();
            let url = start(Some(f.clone()), &fx.root).await;
            f.halt("test: halted".into());
            for (m, p, b) in [
                ("GET", "/milestones/p", None),
                ("GET", "/milestones/log", None),
                (
                    "POST",
                    "/milestones/published",
                    Some(json!({ "seq": 1, "hash": "x" })),
                ),
                (
                    "POST",
                    "/milestones/p/requests",
                    Some(json!({ "head": fx.head })),
                ),
                ("POST", "/milestones/p/requests/p-q1/cancel", None),
                (
                    "POST",
                    "/milestones",
                    Some(json!({ "phase": "p", "base": fx.base,
                    "task_book": "plans/book.md", "evidence_dir": "ev" })),
                ),
            ] {
                let (c, v) = call(&url, m, p, b).await;
                assert_eq!(c, 503, "{m} {p}: {v}");
                assert_eq!(v["halted"], "test: halted");
            }
            // Refused before the call: nothing was even attempted.
            assert!(
                !fx.root.join("state/pending/p.json").exists(),
                "no application kept"
            );
            assert!(kinds(&f).is_empty(), "nothing written");
        }

        /// A halt that lands while a call runs wins over its result (Codex n4 §3).
        #[tokio::test(flavor = "multi_thread")]
        async fn a_halt_during_a_call_turns_its_answer_into_503() {
            let fx = Fx::new();
            let f = fx.service();
            fx.register(&f);
            fx.publish(&f);
            f.archive().unwrap();
            let url = start(Some(f.clone()), &fx.root).await;
            // Status: derived, then halted before it returns.
            let weak = Arc::downgrade(&f);
            let halting: Arc<dyn Fn() + Send + std::marker::Sync> = Arc::new(move || {
                if let Some(f) = weak.upgrade() {
                    f.halt("halted during the call".into());
                }
            });
            f.hooks.lock().unwrap().in_status = Some(halting.clone());
            let (c, v) = call(&url, "GET", "/milestones/p", None).await;
            assert_eq!(c, 503, "never 200 once halted: {v}");
            // log and published, on a fresh flow each, through the generic hook.
            for (m, p) in [
                ("GET", "/milestones/log"),
                ("POST", "/milestones/published"),
            ] {
                let fx = Fx::new();
                let f = fx.service();
                fx.register(&f);
                let (seq, hash) = last(&f);
                let weak = Arc::downgrade(&f);
                f.hooks.lock().unwrap().in_call = Some(Arc::new(move || {
                    if let Some(f) = weak.upgrade() {
                        f.halt("halted during the call".into());
                    }
                }));
                let url = start(Some(f.clone()), &fx.root).await;
                let body = (m == "POST").then(|| json!({ "seq": seq, "hash": hash }));
                let (c, v) = call(&url, m, p, body).await;
                assert_eq!(c, 503, "{p}: {v}");
            }
        }
    }

    /// `admin milestone` (6c, Codex n4 §2 and n6 condition 3).
    mod admin_cli {
        use super::*;
        use crate::milestone::cli::{AdminOp, execute, offline_status, preview};
        use crate::milestone::serve::MilestoneConfig;

        /// Every file under `dir` with its sha256 (the set and the contents).
        fn digest(dir: &Path) -> BTreeMap<PathBuf, String> {
            let mut out = BTreeMap::new();
            if !dir.exists() {
                return out;
            }
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                for e in fs::read_dir(&d).unwrap() {
                    let p = e.unwrap().path();
                    let m = fs::symlink_metadata(&p).unwrap();
                    if m.is_dir() {
                        out.insert(p.clone(), "dir".into());
                        stack.push(p);
                    } else if m.is_file() {
                        out.insert(p.clone(), sha256_hex(&fs::read(&p).unwrap()));
                    } else {
                        out.insert(p.clone(), "special".into());
                    }
                }
            }
            out
        }

        fn config(fx: &Fx) -> MilestoneConfig {
            MilestoneConfig {
                repo_id: "repo".into(),
                repo: fx.repo.clone(),
                remote_url: fx.cfg().remote_url,
                runner_socket: fx.socket(),
                runner_uid: 0,
                runner_cgroup: "/x".into(),
                job_timeout_secs: 10,
                auditor: "auditor".into(),
                implementers: BTreeSet::from(["claude".to_string()]),
                bridge_url: "http://127.0.0.1:9".into(),
                bridge_token_file: fx.root.join("no-token"),
            }
        }

        /// The flow's milestone dir, copied into `<new state>/milestones` as the CLI expects.
        fn as_state(fx: &Fx, name: &str) -> PathBuf {
            let state = fx.root.join(name);
            fs::create_dir_all(&state).unwrap();
            let out = std::process::Command::new("cp")
                .arg("-a")
                .arg(fx.root.join("state"))
                .arg(state.join("milestones"))
                .output()
                .unwrap();
            assert!(out.status.success());
            state
        }

        fn ops(fx: &Fx) -> Vec<AdminOp> {
            let vendors = fx.root.join("vendors.json");
            let mut t = sources::Vendors::first();
            t.version += 1;
            fs::write(&vendors, serde_json::to_string(&t).unwrap()).unwrap();
            let r = || "because".to_string();
            vec![
                AdminOp::Confirm { phase: "p2".into() },
                AdminOp::Rebaseline {
                    phase: "p".into(),
                    base: fx.head.clone(),
                    reason: r(),
                },
                AdminOp::Waive {
                    phase: "p".into(),
                    condition: "p/R1".into(),
                    reason: r(),
                },
                AdminOp::DropSource {
                    phase: "p".into(),
                    source: "s".into(),
                    reason: r(),
                },
                AdminOp::LinkTask {
                    phase: "p".into(),
                    task: "t".into(),
                    reason: r(),
                },
                AdminOp::IgnoreTask {
                    phase: "p".into(),
                    task: "t".into(),
                    reason: r(),
                },
                AdminOp::Revoke {
                    phase: "p".into(),
                    reason: r(),
                },
                AdminOp::Resume {
                    phase: "p".into(),
                    reason: r(),
                },
                AdminOp::VendorsSet {
                    file: vendors,
                    reason: r(),
                },
            ]
        }

        /// Dry runs and the offline status leave the state exactly as it was, in states where
        /// opening the flow would write: none at all, an unfinished group, a remote ahead.
        #[test]
        fn dry_runs_and_the_offline_status_write_nothing() {
            let fx = Fx::new();
            let (f, _) = requested(&fx);
            let behind = as_state(&fx, "behind"); // copied before the remote moves on
            f.apply(
                "p2",
                &fx.base,
                "plans/book.md",
                "ev",
                vec![],
                Some("p".into()),
            )
            .unwrap();
            f.ledger().crash_after(1);
            assert!(f.confirm("p2").is_err());
            f.ledger().crash_after(usize::MAX);
            let crashed = as_state(&fx, "crashed");
            // The remote holds the full log: the copy taken before is behind it.
            fx.publish(&f);
            let missing = fx.root.join("missing");
            let cfg = config(&fx);
            let key = kp().verifying_key();
            let all = ops(&fx);
            for state in [&missing, &crashed, &behind] {
                let before = digest(&fx.root);
                for op in &all {
                    let v = preview(op, &cfg, state, Some(&key)).unwrap();
                    assert_eq!(v["dry_run"], true);
                }
                let st = offline_status(state, "repo", &key, "p");
                if state == &missing {
                    assert!(st.is_err());
                } else {
                    let st = st.unwrap();
                    assert_eq!(
                        (&st["offline"], &st["released"], &st["remote_checked"]),
                        (&json!(true), &json!(false), &json!(false))
                    );
                }
                let after = digest(&fx.root);
                assert_eq!(before, after, "{}: nothing written", state.display());
            }
            assert!(!missing.exists());
            // The two states are ones where the normal path would write: status on a copy of
            // each (recovery, import) changes it.
            for name in ["crashed", "behind"] {
                let copy = fx.root.join(format!("{name}-copy"));
                std::process::Command::new("cp")
                    .arg("-a")
                    .arg(fx.root.join(name).join("milestones"))
                    .arg(&copy)
                    .status()
                    .unwrap();
                let before = digest(&copy);
                let mut c = fx.cfg();
                c.dir = copy.clone();
                Flow::open_admin(c, kp(), fx.reader())
                    .unwrap()
                    .status("p")
                    .unwrap();
                assert_ne!(before, digest(&copy), "{name}: the normal status writes");
            }
            // The preview shows what helps decide: the base resolved read-only, the application.
            let v = preview(&all[1], &cfg, &crashed, Some(&key)).unwrap();
            assert_eq!(v["would"]["resolved"], fx.head.as_str());
            let v = preview(&all[0], &cfg, &crashed, Some(&key)).unwrap();
            assert_eq!(v["would"]["application"]["supersedes"], "p");
        }

        /// The rebaseline dry run shows the registered task book at the target commit (Codex
        /// 6c impl review n5 #2), and still writes nothing.
        #[test]
        fn a_rebaseline_preview_shows_the_task_book_at_the_target_commit() {
            let fx = Fx::new();
            let f = fx.admin();
            fx.register(&f);
            write(&fx.repo, "plans/book.md", "# a new version of the book\n");
            let newer = commit(&fx.repo, "new book", false);
            let state = as_state(&fx, "s");
            let cfg = config(&fx);
            let key = kp().verifying_key();
            let before = digest(&fx.root);
            let show = |base: &str, key: Option<&VerifyingKey>| {
                let op = AdminOp::Rebaseline {
                    phase: "p".into(),
                    base: base.into(),
                    reason: "r".into(),
                };
                preview(&op, &cfg, &state, key).unwrap()["would"]["task_book"].clone()
            };
            let old = show(&fx.base, Some(&key));
            let new = show(&newer, Some(&key));
            assert_eq!(old["path"], "plans/book.md");
            assert_eq!(old["sha256"], sha256_hex(BOOK.as_bytes()), "{old}");
            assert_eq!(
                new["sha256"],
                sha256_hex(b"# a new version of the book\n"),
                "{new}"
            );
            assert_eq!(new["commit"], newer.as_str());
            // Without the pinned key, or for an unknown commit: said so, nothing guessed.
            assert!(show(&newer, None)["not_resolved"].is_string());
            assert!(show("no-such-rev", Some(&key))["not_resolved"].is_string());
            assert_eq!(before, digest(&fx.root), "nothing written");
        }

        #[test]
        fn the_offline_status_never_releases_even_a_released_phase() {
            let fx = Fx::new();
            let f = fx.service();
            fx.register(&f);
            f.request("p", &fx.head).unwrap();
            settled(&f, "p");
            fx.publish(&f);
            f.archive().unwrap();
            assert!(f.status("p").unwrap().released);
            let state = as_state(&fx, "s");
            let v = offline_status(&state, "repo", &kp().verifying_key(), "p").unwrap();
            assert_eq!(v["released"], false);
            assert_eq!(
                v["state"]["released"], true,
                "the derived state is shown as is"
            );
            assert!(
                offline_status(
                    &state,
                    "repo",
                    &Keypair::from_seed([0xA5; 32]).verifying_key(),
                    "p"
                )
                .is_err(),
                "only the pinned key verifies"
            );
        }

        #[test]
        fn yes_writes_and_shows_the_real_record() {
            let fx = Fx::new();
            let f = fx.admin();
            fx.register(&f);
            let v = execute(
                &f,
                &AdminOp::Revoke {
                    phase: "p".into(),
                    reason: "user said".into(),
                },
            )
            .unwrap();
            let last = f.ledger().load().unwrap().records.last().cloned().unwrap();
            assert_eq!(v["written"][0]["seq"], last.seq);
            assert_eq!(v["written"][0]["hash"], last.hash.as_str());
            assert_eq!(last.kind, "revoke");
        }
    }

    #[test]
    fn a_failed_archive_before_status_withholds_release() {
        let fx = Fx::new();
        let f = fx.service();
        fx.register(&f);
        f.request("p", &fx.head).unwrap();
        settled(&f, "p");
        fx.publish(&f);
        f.archive().unwrap();
        assert!(f.status_archived("p").unwrap().released);
        // The archive's fetch fails, the status's own fetch succeeds: the archive's reason
        // still withholds release (released follows the final reasons).
        f.hooks
            .lock()
            .unwrap()
            .failed_fetches
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let st = f.status_archived("p").unwrap();
        assert!(
            st.reasons.iter().any(|r| r.starts_with("archive:")),
            "{:?}",
            st.reasons
        );
        assert!(!st.released);
    }

    /// A send holding the admission lock delays halt()'s return, never the latch (DSH ①).
    #[test]
    fn the_latch_is_visible_at_once_while_halt_waits_for_a_send() {
        let fx = Fx::new();
        let me: Arc<std::sync::OnceLock<std::sync::Weak<Flow>>> = Arc::default();
        let halter = Arc::new(Halter::default());
        let s = Flow::open_service(fx.cfg(), kp(), fx.reader()).unwrap();
        fx.register(&s);
        me.set(Arc::downgrade(&s)).unwrap();
        let seen = Arc::new(Flag::default());
        let (h2, m2, seen2) = (halter.clone(), me.clone(), seen.clone());
        let inner = Halter::hook(halter.clone(), me, true).unwrap();
        s.hooks.lock().unwrap().while_sending = Some(Arc::new(move || {
            // Start the halt (it must stay blocked: this thread holds the admission lock)...
            inner();
            // ...yet the latch is already set, and the watchdog would be woken.
            let f = m2.get().and_then(std::sync::Weak::upgrade).unwrap();
            assert!(f.halted().is_some(), "latch set before halt() returns");
            assert!(
                !h2.done.wait(Duration::from_millis(50)),
                "halt() still waiting"
            );
            seen2.set();
        }));
        s.request("p", &fx.head).unwrap();
        assert!(seen.wait(Duration::from_secs(10)), "the hook ran");
        halter.join();
        jobs_finished(&s);
        assert_eq!(
            fx.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the send in progress completed"
        );
    }

    /// Flow service-level fault tests F1–F6 (M2 step 7; F7–F11 are in `faults_remote`).
    mod faults;
}
