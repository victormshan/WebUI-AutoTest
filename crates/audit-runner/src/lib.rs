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

/// Largest input accepted, in bytes (a phase diff plus evidence is a few hundred KB).
pub const MAX_INPUT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    /// The only uid allowed to submit jobs (review-gate's).
    pub allow_uid: u32,
    /// If set, the submitting process must run this executable (`/proc/<pid>/exe`).
    pub allow_exe: Option<PathBuf>,
    pub codex: PathBuf,
    pub codex_home: PathBuf,
    /// One subdirectory per job: the input, Codex's stdout events and its answer.
    pub workdir: PathBuf,
    pub default_timeout: Duration,
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

/// Whether the peer may submit jobs: its uid must match, and if an executable is required,
/// `/proc/<pid>/exe` must point at it.
pub fn peer_allowed(cfg: &Config, uid: u32, pid: Option<i32>) -> Result<(), String> {
    if uid != cfg.allow_uid {
        return Err(format!("peer uid {uid} is not allowed"));
    }
    if let Some(exe) = &cfg.allow_exe {
        let Some(pid) = pid else {
            return Err("peer pid unknown".into());
        };
        let actual = std::fs::read_link(format!("/proc/{pid}/exe"))
            .map_err(|e| format!("cannot read peer executable: {e}"))?;
        if &actual != exe {
            return Err(format!(
                "peer executable {} is not {}",
                actual.display(),
                exe.display()
            ));
        }
    }
    Ok(())
}

/// Summarises Codex's `--json` event stream: item types and attempted commands.
pub fn summarize_events(stdout: &str) -> (Vec<(String, usize)>, Vec<String>) {
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut commands = Vec::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["type"] != "item.completed" {
            continue;
        }
        let item = &v["item"];
        let ty = item["type"].as_str().unwrap_or("unknown").to_string();
        match counts.iter_mut().find(|(t, _)| *t == ty) {
            Some((_, n)) => *n += 1,
            None => counts.push((ty.clone(), 1)),
        }
        if ty.contains("command") {
            commands.push(format!(
                "{} -> {}",
                item["command"].as_str().unwrap_or("?"),
                item["status"]
                    .as_str()
                    .or(item["exit_code"].as_i64().map(|_| "exited"))
                    .unwrap_or("?")
            ));
        }
    }
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
        runner_version: env!("CARGO_PKG_VERSION").into(),
        started_at: started,
        finished_at: started,
    }
}

/// Runs one job and reports. Never panics on bad input: everything becomes a status.
pub async fn run_job(cfg: &Config, req: &Request) -> Response {
    let started = Utc::now();
    let mut out = response(&req.job_id, req.input.as_bytes(), started);
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
    let _ = std::fs::write(dir.join("input.txt"), &req.input);
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
    .arg(&dir)
    .arg("-o")
    .arg(&answer_file)
    .args(["-c", "approval_policy=\"never\""])
    .arg("-c")
    .arg(format!(
        "model_reasoning_effort=\"{}\"",
        cfg.reasoning_effort
    ))
    .arg("-")
    // A clean environment: no WSL_INTEROP, no inherited credentials or proxies.
    .env_clear()
    .env("PATH", "/usr/local/bin:/usr/bin:/bin")
    .env("HOME", cfg.codex_home.parent().unwrap_or(Path::new("/")))
    .env("CODEX_HOME", &cfg.codex_home)
    .current_dir(&dir)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
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
    tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
        let _ = stdin.shutdown().await;
    });
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let reading = async {
        let (mut o, mut e) = (String::new(), String::new());
        let _ = tokio::join!(stdout.read_to_string(&mut o), stderr.read_to_string(&mut e));
        let status = child.wait().await;
        (o, e, status)
    };
    let limit = Duration::from_secs(req.timeout_secs.unwrap_or(cfg.default_timeout.as_secs()));
    let (o, e, status) = match tokio::time::timeout(limit, reading).await {
        Ok(r) => r,
        Err(_) => {
            // End the whole group (kill_on_drop alone would only end Codex itself).
            if let Some(pgid) = pgid {
                // SAFETY: plain syscall on the process group this function created.
                unsafe {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
            return finish(
                out,
                "timed_out",
                Some(format!("no result within {limit:?}")),
            );
        }
    };
    let _ = std::fs::write(dir.join("events.jsonl"), &o);
    let _ = std::fs::write(dir.join("stderr.log"), &e);
    (out.item_types, out.commands) = summarize_events(&o);
    out.exit_code = status.as_ref().ok().and_then(|s| s.code());
    let answer = std::fs::read_to_string(&answer_file).unwrap_or_default();
    out.answer = answer.clone();
    match status {
        Ok(s) if s.success() && !answer.trim().is_empty() => finish(out, "completed", None),
        Ok(s) if s.success() => finish(out, "failed", Some("codex wrote no answer".into())),
        Ok(s) => finish(out, "failed", Some(format!("codex exited with {s}"))),
        Err(err) => finish(out, "failed", Some(format!("waiting for codex: {err}"))),
    }
}

/// Serves one connection: peer check, one request line, one response line.
pub async fn handle(cfg: &Config, stream: UnixStream) -> std::io::Result<()> {
    let cred = stream.peer_cred()?;
    let (read, mut write) = stream.into_split();
    let started = Utc::now();
    if let Err(why) = peer_allowed(cfg, cred.uid(), cred.pid()) {
        let mut r = response("", b"", started);
        r.error = Some(why);
        write
            .write_all(format!("{}\n", serde_json::to_string(&r)?).as_bytes())
            .await?;
        return Ok(());
    }
    let mut line = String::new();
    let mut reader = BufReader::new(read.take((MAX_INPUT + 64 * 1024) as u64));
    reader.read_line(&mut line).await?;
    let resp = match serde_json::from_str::<Request>(&line) {
        Ok(req) => run_job(cfg, &req).await,
        Err(e) => {
            let mut r = response("", line.as_bytes(), started);
            r.error = Some(format!("bad request: {e}"));
            r
        }
    };
    write
        .write_all(format!("{}\n", serde_json::to_string(&resp)?).as_bytes())
        .await?;
    Ok(())
}

/// Accepts connections one after another: one Codex run at a time.
pub async fn serve(cfg: Config, listener: UnixListener) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        if let Err(e) = handle(&cfg, stream).await {
            eprintln!("audit-runner: connection failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(uid: u32, exe: Option<PathBuf>) -> Config {
        Config {
            allow_uid: uid,
            allow_exe: exe,
            codex: "/nonexistent".into(),
            codex_home: "/nonexistent/.codex".into(),
            workdir: std::env::temp_dir(),
            default_timeout: Duration::from_secs(5),
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
    fn only_the_allowed_uid_and_executable_pass() {
        let me = std::process::id() as i32;
        let my_exe = std::fs::read_link("/proc/self/exe").unwrap();
        assert!(peer_allowed(&cfg(5, None), 5, None).is_ok());
        assert!(peer_allowed(&cfg(5, None), 6, Some(me)).is_err());
        assert!(peer_allowed(&cfg(5, Some(my_exe.clone())), 5, Some(me)).is_ok());
        assert!(peer_allowed(&cfg(5, Some("/usr/bin/true".into())), 5, Some(me)).is_err());
        assert!(peer_allowed(&cfg(5, Some(my_exe)), 5, None).is_err());
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
}
