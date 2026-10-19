//! audit-runner: runs one milestone-audit job at a time with the auditor's Codex CLI and hands
//! the raw result back to review-gate (plans/milestone-audit-plan.md, M1).
//!
//! It holds **no keys**: review-gate builds the input, checks the result and is the only signer.
//! The runner only checks who is calling, runs Codex read-only on exactly the bytes it was
//! given, and reports what happened, with its own digest of the input so the gate never has to
//! trust the model to echo it.
//!
//! Threat model A (accountability): this keeps accidental misuse out (a wrong caller, a model
//! that tries to run commands, a leaked `WSL_INTEROP`); it does not stop a root-capable
//! implementer.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Leaves Codex no command-execution entry point: shell, unified exec, code-mode host (code mode
/// then fails closed), hooks, plugins, apps, browser/computer use, web search off; own config and
/// rules ignored. Captured for CLI 0.162.1 (M1-evidence/codex-surface): request_user_input, wait,
/// sleep and sub-agents (same surface) remain; recapture for a new CLI or model.
pub const NO_TOOLS: &[&str] = &[
    "--ignore-user-config",
    "--ignore-rules",
    "-c",
    "web_search=\"disabled\"",
    "--disable",
    "shell_tool",
    "--disable",
    "unified_exec",
    "--disable",
    "code_mode_host",
    "--disable",
    "multi_agent",
    "--disable",
    "hooks",
    "--disable",
    "plugins",
    "--disable",
    "remote_plugin",
    "--disable",
    "apps",
    "--disable",
    "browser_use",
    "--disable",
    "browser_use_external",
    "--disable",
    "computer_use",
    "--disable",
    "in_app_browser",
    "--disable",
    "skill_mcp_dependency_install",
];

/// Largest input accepted, in bytes (a phase diff plus evidence is a few hundred KB).
pub const MAX_INPUT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    /// The only uid allowed to submit jobs (review-gate's).
    pub allow_uid: u32,
    /// Service-origin allowlist: if set, the submitting process must belong to this cgroup (`/proc/<pid>/cgroup`), e.g.
    /// `/system.slice/review-gate.service`. Only root can move a process into a unit's cgroup.
    /// (`/proc/<pid>/exe` is not used: reading it needs ptrace access, which the runner has
    /// not, and must not have, over the gate's uid.)
    pub allow_cgroup: Option<String>,
    pub codex: PathBuf,
    /// The auditor's Codex home: only its login (`auth.json`) is used.
    pub codex_home: PathBuf,
    /// Codex's home during a job: emptied and recreated for every job with nothing but a copy of
    /// the login, so no AGENTS.md, skills, prompts or other context from the auditor's own home
    /// reach the model. A refreshed login is copied back afterwards; the directory is removed.
    /// It lies outside the job directories, so the login never enters a job's record.
    pub run_home: PathBuf,
    /// One subdirectory per job: the input, Codex's stdout events and its answer.
    pub workdir: PathBuf,
    pub default_timeout: Duration,
    /// How long sending a reply may take before the runner gives up on that connection.
    pub reply_timeout: Duration,
    /// The runner has its own systemd service cgroup (production). After every job it then
    /// also ends every other process in that cgroup (e.g. one that left Codex's process group
    /// with setsid) and confirms it is the only one left; "gone" requires both.
    pub own_cgroup: bool,
    pub reasoning_effort: String,
}

/// One job, sent as a single JSON line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub job_id: String,
    /// The complete prompt for Codex (instructions and material), built by the gate.
    pub input: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// The runner's report, sent back as a single JSON line. Everything except `answer` is
/// filled in by the runner; the model's text appears only in `answer`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub job_id: String,
    /// SHA-256 of the input bytes the runner actually received (lower-case hex).
    pub input_sha256: String,
    /// "completed" (Codex exited 0 and wrote an answer), "failed" or "timed_out", or
    /// "rejected" (the request itself was refused).
    pub status: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// Codex's final message (`-o` file), verbatim.
    pub answer: String,
    /// Counts of the item types Codex reported (`--json` events), e.g. how many command
    /// executions it attempted: the evidence that tool use was blocked by configuration.
    pub item_types: Vec<(String, usize)>,
    /// Command executions Codex attempted, with how each ended (as reported in its events).
    pub commands: Vec<String>,
    /// Whether `item_types` and `commands` come from Codex's whole event stream. False when
    /// the job was refused, stopped (timeout) or its record could not be read.
    #[serde(default)]
    pub evidence_complete: bool,
    /// What became of Codex's process group: "gone" (confirmed: the group no longer exists),
    /// "not confirmed: …", or "" when nothing was started. When not confirmed, the runner exits
    /// after replying, so systemd ends what is left in its cgroup before the next job.
    #[serde(default)]
    pub cleanup: String,
    pub runner_version: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

/// Job ids become directory names: keep them short and plain.
pub fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether the peer may submit jobs: its uid must match, and if a cgroup is required, the
/// peer's cgroup v2 path must be exactly it.
pub fn peer_allowed(cfg: &Config, uid: u32, pid: Option<i32>) -> Result<(), String> {
    if uid != cfg.allow_uid {
        return Err(format!("peer uid {uid} is not allowed"));
    }
    if let Some(want) = &cfg.allow_cgroup {
        let Some(pid) = pid else {
            return Err("peer pid unknown".into());
        };
        let actual = cgroup_of(pid).map_err(|e| format!("cannot read peer cgroup: {e}"))?;
        if &actual != want {
            return Err(format!("peer cgroup {actual} is not {want}"));
        }
    }
    Ok(())
}

/// The cgroup v2 path of a process (the `0::` line of `/proc/<pid>/cgroup`).
pub fn cgroup_of(pid: i32) -> std::io::Result<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::to_owned)
        .ok_or_else(|| std::io::Error::other("no cgroup v2 entry"))
}

/// Summarises Codex's `--json` event stream: item types and attempted commands.
pub fn summarize_events(stdout: &str) -> (Vec<(String, usize)>, Vec<String>) {
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut commands = Vec::new();
    // Commands that started but never completed (e.g. stopped by Codex's own sandbox) are
    // still reported, so the record shows every attempt.
    let mut open: Vec<(String, String)> = Vec::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let item = &v["item"];
        let ty = item["type"].as_str().unwrap_or("unknown").to_string();
        let id = item["id"].as_str().unwrap_or("").to_string();
        let command = item["command"].as_str().unwrap_or("?").to_string();
        if v["type"] == "item.started" {
            if ty.contains("command") {
                open.push((id, command));
            }
            continue;
        }
        if v["type"] != "item.completed" {
            continue;
        }
        match counts.iter_mut().find(|(t, _)| *t == ty) {
            Some((_, n)) => *n += 1,
            None => counts.push((ty.clone(), 1)),
        }
        if ty.contains("command") {
            if let Some(i) = open.iter().position(|(o, _)| !id.is_empty() && *o == id) {
                open.remove(i);
            }
            commands.push(format!(
                "{command} -> {}",
                item["status"]
                    .as_str()
                    .or(item["exit_code"].as_i64().map(|_| "exited"))
                    .unwrap_or("?")
            ));
        }
    }
    commands.extend(
        open.into_iter()
            .map(|(_, c)| format!("{c} -> never completed")),
    );
    (counts, commands)
}

fn response(job_id: &str, input: &[u8], started: DateTime<Utc>) -> Response {
    Response {
        job_id: job_id.to_string(),
        input_sha256: sha256_hex(input),
        status: "rejected".into(),
        exit_code: None,
        error: None,
        answer: String::new(),
        item_types: Vec::new(),
        commands: Vec::new(),
        evidence_complete: false,
        cleanup: String::new(),
        runner_version: env!("CARGO_PKG_VERSION").into(),
        started_at: started,
        finished_at: started,
    }
}

/// Runs one job and reports. Never panics on bad input: everything becomes a status.
pub async fn run_job(cfg: &Config, req: &Request) -> Response {
    let started = Utc::now();
    let out = response(&req.job_id, req.input.as_bytes(), started);
    let finish = |mut r: Response, status: &str, error: Option<String>| {
        r.status = status.into();
        r.error = error;
        r.finished_at = Utc::now();
        r
    };
    if !valid_job_id(&req.job_id) {
        return finish(out, "rejected", Some("invalid job id".into()));
    }
    if req.input.len() > MAX_INPUT {
        return finish(
            out,
            "rejected",
            Some(format!("input over {MAX_INPUT} bytes")),
        );
    }
    let dir = cfg.workdir.join(&req.job_id);
    if dir.exists() {
        // A job id is used once: a repeated id is a replay, not a retry.
        return finish(out, "rejected", Some("job id already used".into()));
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return finish(out, "failed", Some(format!("workdir: {e}")));
    }
    if let Err(e) = std::fs::write(dir.join("input.txt"), &req.input) {
        return finish(out, "failed", Some(format!("saving input: {e}")));
    }
    // Codex's events and stderr go straight to files, so the record survives a timeout.
    let events_file = dir.join("events.jsonl");
    let (events, errors) = match (
        std::fs::File::create_new(&events_file),
        std::fs::File::create_new(dir.join("stderr.log")),
    ) {
        (Ok(o), Ok(e)) => (o, e),
        (Err(e), _) | (_, Err(e)) => {
            return finish(out, "failed", Some(format!("creating event files: {e}")));
        }
    };
    let run_codex = match prepare_run_home(cfg) {
        Ok(p) => p,
        Err(e) => {
            // Nothing may be left half-made (a copied login included).
            let _ = std::fs::remove_dir_all(&cfg.run_home);
            return finish(out, "failed", Some(format!("preparing codex home: {e}")));
        }
    };
    let out = run_codex_job(cfg, req, &dir, &run_codex, events, errors, out).await;
    match close_run_home(cfg) {
        Ok(()) => out,
        Err(e) => {
            let mut out = out;
            out.status = "failed".into();
            out.error = Some(format!("restoring the login after the job: {e}"));
            out
        }
    }
}

/// Empties `run_home` and gives it a Codex home holding only a copy of the login.
pub fn prepare_run_home(cfg: &Config) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    match std::fs::remove_dir_all(&cfg.run_home) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let codex = cfg.run_home.join(".codex");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&codex)?;
    let login = std::fs::read(cfg.codex_home.join("auth.json"))?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(codex.join("auth.json"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, &login))?;
    Ok(codex)
}

/// Copies a login Codex refreshed during the job back to the auditor's home, then removes
/// `run_home`, also when the copy fails (the error is returned; a refreshed login that could
/// not be saved is then lost, and the auditor may have to log in again).
pub fn close_run_home(cfg: &Config) -> std::io::Result<()> {
    let saved = save_refreshed_login(cfg);
    let removed = std::fs::remove_dir_all(&cfg.run_home);
    saved.and(removed)
}

fn save_refreshed_login(cfg: &Config) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let used = cfg.run_home.join(".codex/auth.json");
    let master = cfg.codex_home.join("auth.json");
    if let Ok(now) = std::fs::read(&used) {
        let changed = std::fs::read(&master).map(|m| m != now).unwrap_or(true);
        if changed && serde_json::from_slice::<serde_json::Value>(&now).is_ok() {
            let tmp = cfg.codex_home.join("auth.json.runner-tmp");
            let _ = std::fs::remove_file(&tmp);
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .and_then(|mut f| std::io::Write::write_all(&mut f, &now))?;
            std::fs::rename(&tmp, &master)?;
        }
    }
    Ok(())
}

async fn run_codex_job(
    cfg: &Config,
    req: &Request,
    dir: &Path,
    run_codex: &Path,
    events: std::fs::File,
    errors: std::fs::File,
    mut out: Response,
) -> Response {
    let finish = |mut r: Response, status: &str, error: Option<String>| {
        r.status = status.into();
        r.error = error;
        r.finished_at = Utc::now();
        r
    };
    let events_file = dir.join("events.jsonl");
    let answer_file = dir.join("answer.md");
    let mut cmd = tokio::process::Command::new(&cfg.codex);
    cmd.args([
        "exec",
        "--json",
        "-s",
        "read-only",
        "--skip-git-repo-check",
        "-C",
    ])
    .arg(dir)
    .arg("-o")
    .arg(&answer_file)
    .args(["-c", "approval_policy=\"never\""])
    .arg("-c")
    .arg(format!(
        "model_reasoning_effort=\"{}\"",
        cfg.reasoning_effort
    ))
    .args(NO_TOOLS)
    .arg("-")
    // A clean environment: no WSL_INTEROP, no inherited credentials or proxies.
    .env_clear()
    .env("PATH", "/usr/local/bin:/usr/bin:/bin")
    .env("HOME", &cfg.run_home)
    .env("CODEX_HOME", run_codex)
    .current_dir(dir)
    .stdin(Stdio::piped())
    .stdout(events)
    .stderr(errors)
    // Its own process group, so a timeout can end Codex and everything it started.
    .process_group(0)
    .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return finish(out, "failed", Some(format!("cannot start codex: {e}"))),
    };
    let pgid = child.id().map(|p| p as i32);
    let input = req.input.clone().into_bytes();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let running = async {
        let fed = async move {
            stdin.write_all(&input).await?;
            // Closing stdin (dropping it) is what ends Codex's input; shutdown() does not.
            drop(stdin);
            std::io::Result::Ok(())
        };
        let (fed, status) = tokio::join!(fed, child.wait());
        (fed, status)
    };
    let limit = Duration::from_secs(req.timeout_secs.unwrap_or(cfg.default_timeout.as_secs()));
    let result = tokio::time::timeout(limit, running).await;
    // Codex has exited or is stopped now; either way its whole group must be gone before the
    // reply (kill_on_drop alone would only end Codex itself).
    let group = match pgid {
        Some(pgid) => end_group(pgid, &mut child).await,
        None => Cleanup::NotConfirmed("no process group id".into()),
    };
    let swept = if cfg.own_cgroup {
        sweep_own_cgroup(Duration::from_secs(5)).await
    } else {
        Cleanup::Gone
    };
    out.cleanup = match (group, swept) {
        (Cleanup::Gone, Cleanup::Gone) => "gone".into(),
        (Cleanup::NotConfirmed(why), _) | (_, Cleanup::NotConfirmed(why)) => {
            format!("not confirmed: {why}")
        }
    };
    // Test builds only: lets a deployment test make a job's cleanup unconfirmed, to check that
    // the runner then stops and systemd ends what is left in its cgroup.
    #[cfg(feature = "fault-injection")]
    if std::env::var("AUDIT_RUNNER_FAULT").as_deref() == Ok("cleanup-unconfirmed") {
        out.cleanup = "not confirmed: injected fault".into();
    }
    // The record: whatever Codex wrote, also when it was stopped.
    let read = std::fs::read_to_string(&events_file);
    if let Ok(o) = &read {
        (out.item_types, out.commands) = summarize_events(o);
    }
    let (fed, status) = match result {
        Ok(r) => r,
        Err(_) => {
            let note = match &read {
                Ok(_) => "record kept, incomplete".to_string(),
                Err(e) => format!("record unreadable: {e}"),
            };
            return finish(
                out,
                "timed_out",
                Some(format!("no result within {limit:?}; {note}")),
            );
        }
    };
    out.exit_code = status.as_ref().ok().and_then(|s| s.code());
    let events = match read {
        Ok(t) => t,
        Err(e) => return finish(out, "failed", Some(format!("reading events: {e}"))),
    };
    if let Err(e) = fed {
        return finish(out, "failed", Some(format!("input not delivered: {e}")));
    }
    let answer = std::fs::read_to_string(&answer_file).unwrap_or_default();
    out.answer = answer.clone();
    match status {
        Ok(s) if !s.success() => {
            return finish(out, "failed", Some(format!("codex exited with {s}")));
        }
        Err(err) => return finish(out, "failed", Some(format!("waiting for codex: {err}"))),
        Ok(_) => {}
    }
    if answer.trim().is_empty() {
        return finish(out, "failed", Some("codex wrote no answer".into()));
    }
    // A complete, valid event stream is part of the result: without it there is no evidence
    // of what Codex did, so the job is not completed.
    if let Err(why) = check_events(&events) {
        return finish(out, "failed", Some(format!("event record invalid: {why}")));
    }
    if out.cleanup != "gone" {
        let why = format!("cleanup {}", out.cleanup);
        return finish(out, "failed", Some(why));
    }
    out.evidence_complete = true;
    finish(out, "completed", None)
}

/// Whether Codex's `--json` event stream is whole: every line a JSON event, starting with
/// `thread.started`, ending with `turn.completed`, and every started item completed. Checked
/// against the format of the pinned Codex CLI (0.162.1).
pub fn check_events(text: &str) -> Result<(), String> {
    let mut types = Vec::new();
    let mut open: Vec<String> = Vec::new();
    for (i, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let v: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: not JSON ({e})", i + 1))?;
        let ty = v["type"]
            .as_str()
            .ok_or_else(|| format!("line {}: no event type", i + 1))?
            .to_string();
        let id = v["item"]["id"].as_str().unwrap_or("").to_string();
        match ty.as_str() {
            "item.started" => open.push(id),
            "item.completed" => open.retain(|o| *o != id),
            _ => {}
        }
        types.push(ty);
    }
    match (types.first(), types.last()) {
        (None, _) => Err("no events".into()),
        (Some(f), _) if f != "thread.started" => {
            Err(format!("starts with {f}, not thread.started"))
        }
        (_, Some(l)) if l != "turn.completed" => Err(format!("ends with {l}, not turn.completed")),
        _ if !open.is_empty() => Err(format!("{} item(s) started but not completed", open.len())),
        _ => Ok(()),
    }
}

/// How ending a process group went.
#[derive(Debug, PartialEq)]
pub enum Cleanup {
    /// Confirmed: the group no longer exists (ESRCH).
    Gone,
    NotConfirmed(String),
}

/// Whether a process group still exists.
#[derive(Debug, PartialEq)]
pub enum GroupState {
    Gone,
    Alive,
    /// The probe failed for another reason (e.g. EPERM): unknown, never taken as gone.
    Unknown(String),
}

pub fn group_state(pgid: i32) -> GroupState {
    // SAFETY: signal 0 only asks whether the group exists.
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return GroupState::Alive;
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ESRCH) => GroupState::Gone,
        _ => GroupState::Unknown(e.to_string()),
    }
}

/// Polls `probe` (killing with `kill` between polls) until the group is gone or `limit` is
/// spent. Only `Gone` confirms; `Unknown` and running out of time do not.
pub async fn settle(
    mut probe: impl FnMut() -> GroupState,
    mut kill: impl FnMut(),
    limit: Duration,
) -> Cleanup {
    let end = tokio::time::Instant::now() + limit;
    loop {
        match probe() {
            GroupState::Gone => return Cleanup::Gone,
            GroupState::Unknown(why) => {
                return Cleanup::NotConfirmed(format!("group state unknown: {why}"));
            }
            GroupState::Alive if tokio::time::Instant::now() >= end => {
                return Cleanup::NotConfirmed(format!("group still alive after {limit:?}"));
            }
            GroupState::Alive => {
                kill();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// The `cgroup.procs` file of this process's own cgroup (cgroup v2).
pub fn own_cgroup_procs() -> std::io::Result<PathBuf> {
    let path = cgroup_of(std::process::id() as i32)?;
    Ok(Path::new("/sys/fs/cgroup")
        .join(path.trim_start_matches('/'))
        .join("cgroup.procs"))
}

/// What the cgroup sweep needs from the system: reading the member list and killing a member.
/// Tests supply their own, so no test signals a process it does not own (Codex, step 7 plan n3).
pub trait CgroupOps {
    fn members(&self) -> std::io::Result<String>;
    fn kill(&self, pid: u32);
}

/// The real cgroup: its `cgroup.procs` file and kill(2).
pub struct CgroupFile(pub PathBuf);

impl CgroupOps for CgroupFile {
    fn members(&self) -> std::io::Result<String> {
        std::fs::read_to_string(&self.0)
    }
    fn kill(&self, pid: u32) {
        // SAFETY: plain kill(2) on a process of our own cgroup (and uid).
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

/// Kills every member of the cgroup except `me`, until `me` is the only one left or `limit`
/// is spent. A member list that cannot be read is never taken as empty.
pub async fn sweep_cgroup(ops: &impl CgroupOps, me: u32, limit: Duration) -> Cleanup {
    let end = tokio::time::Instant::now() + limit;
    loop {
        let others: Vec<u32> = match ops.members() {
            Ok(t) => t
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .filter(|p| *p != me)
                .collect(),
            Err(e) => return Cleanup::NotConfirmed(format!("reading the cgroup members: {e}")),
        };
        if others.is_empty() {
            return Cleanup::Gone;
        }
        if tokio::time::Instant::now() >= end {
            return Cleanup::NotConfirmed(format!(
                "{} process(es) still in the service cgroup after {limit:?}",
                others.len()
            ));
        }
        for p in others {
            ops.kill(p);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Kills every process in this process's cgroup except itself, until it is the only one left
/// or `limit` is spent.
pub async fn sweep_own_cgroup(limit: Duration) -> Cleanup {
    let procs = match own_cgroup_procs() {
        Ok(p) => p,
        Err(e) => return Cleanup::NotConfirmed(format!("own cgroup: {e}")),
    };
    // Test builds only: an input fault for the sweep's failure path (A25). The sweep reads this
    // path instead of the real member list; its logic runs unchanged.
    #[cfg(feature = "fault-injection")]
    let procs = match std::env::var("AUDIT_RUNNER_FAULT") {
        Ok(v) => match v.strip_prefix("sweep-procs=") {
            Some(p) => PathBuf::from(p),
            None => procs,
        },
        Err(_) => procs,
    };
    sweep_cgroup(&CgroupFile(procs), std::process::id(), limit).await
}

/// Kills Codex's process group, reaps Codex (the leader) and confirms the group is gone, all
/// within 5 s. What Codex started is reaped by init once killed.
async fn end_group(pgid: i32, leader: &mut tokio::process::Child) -> Cleanup {
    let kill = || {
        // SAFETY: plain syscall on the process group run_job created.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    };
    let limit = Duration::from_secs(5);
    let started = tokio::time::Instant::now();
    kill();
    match tokio::time::timeout(limit, leader.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Cleanup::NotConfirmed(format!("reaping codex: {e}")),
        Err(_) => return Cleanup::NotConfirmed(format!("codex not reaped within {limit:?}")),
    }
    settle(
        || group_state(pgid),
        kill,
        limit.saturating_sub(started.elapsed()),
    )
    .await
}

/// Serves one connection: peer check, one request line, `run` for the job, one response
/// line. Returns whether the runner must stop: a job whose process group was not confirmed
/// gone. That is decided before replying, so it holds whether the reply is delivered, fails or
/// times out.
pub async fn handle_with<F, Fut>(cfg: &Config, stream: UnixStream, run: &F) -> bool
where
    F: Fn(Config, Request) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let started = Utc::now();
    let cred = match stream.peer_cred() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("audit-runner: peer credentials: {e}");
            return false;
        }
    };
    let (read, mut write) = stream.into_split();
    let resp = if let Err(why) = peer_allowed(cfg, cred.uid(), cred.pid()) {
        let mut r = response("", b"", started);
        r.error = Some(why);
        r
    } else {
        let mut line = String::new();
        let mut reader = BufReader::new(read.take((MAX_INPUT + 64 * 1024) as u64));
        match reader.read_line(&mut line).await {
            Err(e) => {
                eprintln!("audit-runner: reading the request: {e}");
                return false;
            }
            Ok(_) => match serde_json::from_str::<Request>(&line) {
                Ok(req) => run(cfg.clone(), req).await,
                Err(e) => {
                    let mut r = response("", line.as_bytes(), started);
                    r.error = Some(format!("bad request: {e}"));
                    r
                }
            },
        }
    };
    let stop = resp.cleanup.starts_with("not confirmed");
    let reply = async {
        let text = format!("{}\n", serde_json::to_string(&resp)?);
        write.write_all(text.as_bytes()).await
    };
    match tokio::time::timeout(cfg.reply_timeout, reply).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("audit-runner: reply to {:?} failed: {e}", resp.job_id),
        Err(_) => eprintln!("audit-runner: reply to {:?} timed out", resp.job_id),
    }
    stop
}

/// Accepts connections one after another, one job at a time, with `run_job`.
pub async fn serve(cfg: Config, listener: UnixListener) -> std::io::Result<()> {
    serve_with(cfg, listener, |c, r| async move { run_job(&c, &r).await }).await
}

/// `serve` with the job runner passed in (tests inject outcomes with it). If a job's process
/// group could not be confirmed gone, it stops with an error after that connection, whatever
/// became of the reply: under systemd the service then ends, systemd kills whatever is left in
/// its cgroup, and the next connection starts a fresh runner.
pub async fn serve_with<F, Fut>(cfg: Config, listener: UnixListener, run: F) -> std::io::Result<()>
where
    F: Fn(Config, Request) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    loop {
        let (stream, _) = listener.accept().await?;
        if handle_with(&cfg, stream, &run).await {
            return Err(std::io::Error::other(
                "a job's process group was not confirmed gone; stopping so systemd ends the cgroup",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn cfg(uid: u32, cgroup: Option<&str>) -> Config {
        Config {
            allow_uid: uid,
            allow_cgroup: cgroup.map(str::to_owned),
            codex: "/nonexistent".into(),
            codex_home: "/nonexistent/.codex".into(),
            run_home: "/nonexistent/run".into(),
            workdir: std::env::temp_dir(),
            default_timeout: Duration::from_secs(5),
            reply_timeout: Duration::from_secs(5),
            own_cgroup: false,
            reasoning_effort: "high".into(),
        }
    }

    #[test]
    fn job_ids_are_plain_names() {
        assert!(valid_job_id("b1-2b2.audit_1"));
        for bad in ["", ".", "..", "a/b", "../x", "a b", &"x".repeat(65)] {
            assert!(!valid_job_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn digest_is_lowercase_hex_sha256() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn only_the_allowed_uid_and_cgroup_pass() {
        let me = std::process::id() as i32;
        let mine = cgroup_of(me).unwrap();
        assert!(peer_allowed(&cfg(5, None), 5, None).is_ok());
        assert!(peer_allowed(&cfg(5, None), 6, Some(me)).is_err());
        assert!(peer_allowed(&cfg(5, Some(&mine)), 5, Some(me)).is_ok());
        let other = peer_allowed(&cfg(5, Some("/system.slice/x.service")), 5, Some(me));
        assert!(
            other
                .unwrap_err()
                .contains("is not /system.slice/x.service")
        );
        assert!(peer_allowed(&cfg(5, Some(&mine)), 5, None).is_err());
    }

    #[test]
    fn another_users_cgroup_is_readable() {
        // The gate runs under another uid than the runner; its cgroup must still be readable
        // (its /proc/<pid>/exe is not). pid 1 belongs to root.
        assert!(cgroup_of(1).unwrap().starts_with('/'));
    }

    #[test]
    fn events_report_item_types_and_commands() {
        let events = r#"{"type":"thread.started","thread_id":"t"}
{"type":"item.completed","item":{"type":"command_execution","command":"id","status":"declined"}}
{"type":"item.completed","item":{"type":"agent_message","text":"hi"}}
{"type":"item.completed","item":{"type":"agent_message","text":"bye"}}
not json"#;
        let (types, cmds) = summarize_events(events);
        assert_eq!(
            types,
            vec![("command_execution".into(), 1), ("agent_message".into(), 2)]
        );
        assert_eq!(cmds, vec!["id -> declined"]);
    }

    #[test]
    fn a_command_that_never_completes_is_still_listed() {
        let events = r#"{"type":"item.started","item":{"id":"a","type":"command_execution","command":"id"}}
{"type":"item.completed","item":{"id":"a","type":"command_execution","command":"id","status":"completed"}}
{"type":"item.started","item":{"id":"b","type":"command_execution","command":"touch x"}}"#;
        let (_, commands) = summarize_events(events);
        assert_eq!(
            commands,
            vec!["id -> completed", "touch x -> never completed"]
        );
    }

    #[test]
    fn group_state_confirms_gone_only_on_esrch() {
        // SAFETY: getpgrp has no preconditions.
        let mine = unsafe { libc::getpgrp() };
        assert_eq!(group_state(mine), GroupState::Alive);
        // A process group id that is not in use.
        let free = (100_000..4_000_000)
            .find(|p| !Path::new(&format!("/proc/{p}")).exists())
            .unwrap();
        assert_eq!(group_state(free), GroupState::Gone);
        // A group whose members all belong to other users: the probe is refused (EPERM),
        // which is "unknown", never "gone".
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        let mut groups: std::collections::HashMap<i32, bool> = Default::default();
        for e in std::fs::read_dir("/proc").unwrap().flatten() {
            let (Ok(meta), Ok(stat)) =
                (e.metadata(), std::fs::read_to_string(e.path().join("stat")))
            else {
                continue;
            };
            let Some(pgrp) = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(2))
                .and_then(|p| p.parse::<i32>().ok())
            else {
                continue;
            };
            *groups.entry(pgrp).or_insert(true) &= meta.uid() != me;
        }
        assert!(me != 0, "run the tests as an ordinary user");
        // A group found in the scan may end before it is probed, and /proc shows non-dumpable
        // processes of our own as root's, so single groups may read Gone or Alive; but at least
        // one long-lived group of another user (e.g. a system service) is refused.
        let states: Vec<GroupState> = groups
            .into_iter()
            .filter(|&(g, only_others)| only_others && g > 0)
            .map(|(g, _)| group_state(g))
            .collect();
        assert!(
            states
                .iter()
                .any(|s| matches!(s, GroupState::Unknown(w) if w.contains("ermitted"))),
            "{states:?}"
        );
    }

    #[tokio::test]
    async fn settle_confirms_only_a_gone_group() {
        let mut seq = vec![GroupState::Gone, GroupState::Alive, GroupState::Alive];
        let mut kills = 0;
        let r = settle(|| seq.pop().unwrap(), || kills += 1, Duration::from_secs(5)).await;
        assert_eq!((r, kills), (Cleanup::Gone, 2));
        let unknown = settle(
            || GroupState::Unknown("EPERM".into()),
            || {},
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(unknown, Cleanup::NotConfirmed(w) if w.contains("unknown")));
        let stuck = settle(|| GroupState::Alive, || {}, Duration::from_millis(60)).await;
        assert!(matches!(stuck, Cleanup::NotConfirmed(w) if w.contains("still alive")));
    }

    #[test]
    fn event_records_must_be_whole() {
        let ok = "{\"type\":\"thread.started\"}\n{\"type\":\"item.started\",\"item\":{\"id\":\"a\"}}\n{\"type\":\"item.completed\",\"item\":{\"id\":\"a\"}}\n{\"type\":\"turn.completed\"}\n";
        assert_eq!(check_events(ok), Ok(()));
        assert!(check_events("").unwrap_err().contains("no events"));
        assert!(
            check_events("{\"type\":\"thread.started\"}\nxx\n")
                .unwrap_err()
                .contains("not JSON")
        );
        assert!(
            check_events("{\"x\":1}")
                .unwrap_err()
                .contains("no event type")
        );
        assert!(
            check_events("{\"type\":\"turn.completed\"}")
                .unwrap_err()
                .contains("starts with")
        );
        assert!(
            check_events("{\"type\":\"thread.started\"}\n{\"type\":\"turn.failed\"}")
                .unwrap_err()
                .contains("ends with turn.failed")
        );
        let open = "{\"type\":\"thread.started\"}\n{\"type\":\"item.started\",\"item\":{\"id\":\"a\"}}\n{\"type\":\"turn.completed\"}";
        assert!(check_events(open).unwrap_err().contains("not completed"));
    }

    /// The sweep's failure paths (A25) and its success path, without signalling any process the
    /// test does not own.
    mod sweep {
        use super::*;
        use std::sync::Mutex;

        /// A member list that never changes; kills are recorded, never sent.
        struct Stuck {
            list: String,
            killed: Mutex<Vec<u32>>,
        }

        impl CgroupOps for Stuck {
            fn members(&self) -> std::io::Result<String> {
                Ok(self.list.clone())
            }
            fn kill(&self, pid: u32) {
                self.killed.lock().unwrap().push(pid);
            }
        }

        /// The test's own child: killed and reaped (so its exit is confirmed) before the member
        /// list stops naming it.
        struct OwnChild {
            me: u32,
            child: Mutex<Option<std::process::Child>>,
        }

        impl CgroupOps for OwnChild {
            fn members(&self) -> std::io::Result<String> {
                Ok(match &*self.child.lock().unwrap() {
                    Some(c) => format!("{}\n{}\n", self.me, c.id()),
                    None => format!("{}\n", self.me),
                })
            }
            fn kill(&self, pid: u32) {
                let mut g = self.child.lock().unwrap();
                let c = g.as_mut().expect("killed only while listed");
                assert_eq!(pid, c.id(), "only the test's own child is killed");
                c.kill().unwrap();
                c.wait().unwrap();
                *g = None;
            }
        }

        #[tokio::test]
        async fn a_member_list_that_cannot_be_read_is_not_confirmed() {
            let d = tempfile::tempdir().unwrap();
            let ops = CgroupFile(d.path().join("no-such-dir/cgroup.procs"));
            match sweep_cgroup(&ops, 1, Duration::from_secs(1)).await {
                Cleanup::NotConfirmed(why) => {
                    assert!(why.contains("reading the cgroup members"), "{why}")
                }
                Cleanup::Gone => panic!("an unreadable list was taken as empty"),
            }
        }

        #[tokio::test]
        async fn members_that_outlive_the_limit_are_not_confirmed_in_bounded_time() {
            let me = 7;
            let ops = Stuck {
                list: format!("{me}\n424242\n"),
                killed: Mutex::new(vec![]),
            };
            let started = std::time::Instant::now();
            // Bounded inside the test: a sweep that ignores its limit fails here, by assertion,
            // not by a harness timeout (Codex 7 review n4).
            let out = tokio::time::timeout(
                Duration::from_secs(2),
                sweep_cgroup(&ops, me, Duration::from_millis(200)),
            )
            .await
            .expect("the sweep ends at its limit");
            let took = started.elapsed();
            match out {
                Cleanup::NotConfirmed(why) => assert!(why.contains("still in"), "{why}"),
                Cleanup::Gone => panic!("a member that never left was confirmed gone"),
            }
            assert!(
                took >= Duration::from_millis(200) && took < Duration::from_secs(2),
                "{took:?}"
            );
            let killed = ops.killed.lock().unwrap();
            assert!(
                !killed.is_empty() && killed.iter().all(|p| *p == 424242),
                "{killed:?}"
            );
        }

        #[tokio::test]
        async fn a_member_that_is_killed_and_reaped_is_gone() {
            let child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap();
            let pid = child.id();
            let ops = OwnChild {
                me: 7,
                child: Mutex::new(Some(child)),
            };
            assert!(matches!(
                sweep_cgroup(&ops, 7, Duration::from_secs(5)).await,
                Cleanup::Gone
            ));
            assert!(ops.child.lock().unwrap().is_none(), "reaped");
            let alive = std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| !c.is_empty());
            assert!(!alive, "the child is gone");
        }
    }
}
