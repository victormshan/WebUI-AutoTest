//! The milestone flow inside `review-gate serve` (M2 step 6c; plans/M2-design.md §6, the 6c
//! plan and its r2 with Codex's conditions).
//!
//! Configuration is a JSON file (`serve --milestones <file>`). Without it the milestone routes
//! answer 404 and the gate behaves as before. Any invalid setting refuses to start: it never
//! falls back to running without milestones.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;

use super::flow::{BridgeRead, Config, Flow};
use super::runner::Expected;
use super::sources;

/// How long a halted service may take to stop before it is ended anyway.
pub const HALT_DEADLINE: Duration = Duration::from_secs(5);
/// Exit status of a service that stopped because the milestone log could not be written.
pub const HALT_EXIT: i32 = 3;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MilestoneConfig {
    pub repo_id: String,
    /// The implementer's repository (read only).
    pub repo: PathBuf,
    /// The fixed public repository whose notes hold the published log.
    pub remote_url: String,
    pub runner_socket: PathBuf,
    pub runner_uid: u32,
    pub runner_cgroup: String,
    pub job_timeout_secs: u64,
    pub auditor: String,
    pub implementers: BTreeSet<String>,
    /// agent-bridge, read as an observer for the sources check only.
    pub bridge_url: String,
    /// The observer token: used for nothing but reading the bridge.
    pub bridge_token_file: PathBuf,
}

impl MilestoneConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading milestone config {}", path.display()))?;
        let c: Self = serde_json::from_str(&text)
            .with_context(|| format!("milestone config {}", path.display()))?;
        for (name, p) in [
            ("repo", &c.repo),
            ("runner_socket", &c.runner_socket),
            ("bridge_token_file", &c.bridge_token_file),
        ] {
            if !p.is_absolute() {
                bail!("milestone config: {name} must be an absolute path");
            }
        }
        if c.repo_id.trim().is_empty()
            || c.remote_url.trim().is_empty()
            || c.auditor.trim().is_empty()
            || c.bridge_url.trim().is_empty()
            || c.runner_cgroup.trim().is_empty()
            || c.implementers.is_empty()
        {
            bail!("milestone config: every field must be set");
        }
        if c.job_timeout_secs == 0 {
            bail!("milestone config: job_timeout_secs must be positive");
        }
        Ok(c)
    }

    /// The flow's configuration; commit attestations verify with the gate's own key.
    pub fn flow_config(&self, state: &Path, attestation_key: VerifyingKey) -> Config {
        Config {
            dir: state.join("milestones"),
            repo_id: self.repo_id.clone(),
            repo: self.repo.clone(),
            remote_url: self.remote_url.clone(),
            runner_socket: self.runner_socket.clone(),
            runner: Expected {
                uid: self.runner_uid,
                cgroup: self.runner_cgroup.clone(),
            },
            job_timeout: Duration::from_secs(self.job_timeout_secs),
            auditor: self.auditor.clone(),
            implementers: self.implementers.clone(),
            attestation_key,
        }
    }
}

/// Reads a token from one opened file: its owner and mode are checked on that same file, then
/// its content is read. The only trusted owner is the user this process runs as (the
/// `reviewgate` user in production; not root, not anyone else). Errors never contain the content.
pub fn read_token(path: &Path) -> Result<String> {
    read_token_owned_by(path, unsafe { libc::geteuid() })
}

fn read_token_owned_by(path: &Path, owner: u32) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let meta = f.metadata()?;
    if meta.uid() != owner {
        bail!(
            "{} is owned by uid {}, not by this service's user (uid {owner}): refusing it",
            path.display(),
            meta.uid()
        );
    }
    let mode = meta.mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} is readable by group or others (mode {:o}): refusing it",
            path.display(),
            mode & 0o777
        );
    }
    let mut t = String::new();
    f.read_to_string(&mut t)
        .with_context(|| format!("reading {}", path.display()))?;
    let t = t.trim().to_string();
    if t.len() < 32 {
        bail!("the token in {} is too short", path.display());
    }
    Ok(t)
}

/// One bridge read per call, on `handle`'s runtime. Called only from blocking threads (the
/// service runs every flow call in `spawn_blocking`), never on an async worker.
pub fn bridge_reader(handle: tokio::runtime::Handle, url: String, token: String) -> BridgeRead {
    let http = reqwest::Client::new();
    Arc::new(move || {
        handle
            .block_on(sources::read_bridge(&http, &url, &token))
            .map_err(|e| e.to_string())
    })
}

/// The halt watchdog (Codex n6 condition 1): an OS thread, independent of the async runtime and
/// of the blocking pool. Once the latch is set it first starts the deadline, then signals the
/// graceful shutdown, then hands the diagnostic line to a thread of its own (stderr may block),
/// and ends the process with [`HALT_EXIT`] at the deadline whatever is still running.
pub fn watch_halt(flow: Arc<Flow>, shutdown: tokio::sync::oneshot::Sender<()>) {
    std::thread::spawn(move || {
        let why = flow.wait_halted();
        let deadline = std::time::Instant::now() + HALT_DEADLINE;
        let _ = shutdown.send(());
        std::thread::spawn(move || {
            eprintln!("review-gate: milestone service halted: {why}; stopping (exit {HALT_EXIT})");
        });
        std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
        // _exit: no flushing of stdio buffers, which could block on a full pipe.
        unsafe { libc::_exit(HALT_EXIT) }
    });
}

/// Test-only faults for the subprocess acceptance tests (`--features fault-injection`, never in
/// a deployed build). `REVIEW_GATE_FAULT` is a comma-separated list:
/// - `halt-on=<path>`: halt once `<path>` exists;
/// - `stall-status=<path>`: a status call creates `<path>`, then never returns;
/// - `stall-stderr`: hold the stderr lock for good once `<path>` of `halt-on` exists, so the
///   diagnostic line can never be written;
/// - `runner-probe`: at start, send one line to the runner socket and wait for a reply forever.
#[cfg(feature = "fault-injection")]
pub mod fault {
    use super::*;

    fn get(name: &str) -> Option<String> {
        let v = std::env::var("REVIEW_GATE_FAULT").ok()?;
        v.split(',').find_map(|kv| {
            let (k, val) = kv.split_once('=').unwrap_or((kv, ""));
            (k == name).then(|| val.to_string())
        })
    }

    pub fn start(flow: &Arc<Flow>, runner_socket: &Path) {
        if let Some(p) = get("halt-on") {
            let f = flow.clone();
            let stall_stderr = get("stall-stderr").is_some();
            std::thread::spawn(move || {
                while !Path::new(&p).exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if stall_stderr {
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _held = std::io::stderr().lock();
                        let _ = tx.send(());
                        loop {
                            std::thread::park();
                        }
                    });
                    let _ = rx.recv();
                }
                f.halt("fault injection".into());
            });
        }
        if get("runner-probe").is_some() {
            let socket = runner_socket.to_path_buf();
            std::thread::spawn(move || {
                if let Ok(s) = super::super::runner::send(&socket, "{}", Duration::from_secs(3600))
                {
                    let mut b = [0u8; 1];
                    let _ = (&s).read(&mut b);
                }
            });
        }
    }

    /// Inside a status call (a blocking thread).
    pub fn in_status() {
        if let Some(p) = get("stall-status") {
            let _ = std::fs::write(&p, b"entered");
            loop {
                std::thread::park();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_token_file_open_to_others_or_short_is_refused_without_showing_it() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t");
        let secret = "s3cret-0123456789-0123456789-0123456789";
        std::fs::write(&p, secret).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        let e = read_token(&p).unwrap_err().to_string();
        assert!(e.contains("group or others") && !e.contains(secret), "{e}");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token(&p).unwrap(), secret);
        // Another owner: refused before anything is read, whatever the mode.
        let me = unsafe { libc::geteuid() };
        let e = read_token_owned_by(&p, me + 1).unwrap_err().to_string();
        assert!(e.contains("is owned by uid") && !e.contains(secret), "{e}");
        std::fs::write(&p, "short").unwrap();
        let e = read_token(&p).unwrap_err().to_string();
        assert!(e.contains("too short") && !e.contains("short\""), "{e}");
    }

    #[test]
    fn a_config_with_a_relative_path_or_an_empty_field_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("m.json");
        let good = serde_json::json!({
            "repo_id": "r", "repo": "/repo", "remote_url": "file:///remote",
            "runner_socket": "/run/a.sock", "runner_uid": 1, "runner_cgroup": "/x",
            "job_timeout_secs": 60, "auditor": "openai", "implementers": ["claude"],
            "bridge_url": "http://127.0.0.1:7879", "bridge_token_file": "/etc/t"
        });
        std::fs::write(&p, good.to_string()).unwrap();
        MilestoneConfig::load(&p).unwrap();
        for (k, v) in [
            ("repo", serde_json::json!("repo")),
            ("auditor", serde_json::json!(" ")),
            ("implementers", serde_json::json!([])),
            ("job_timeout_secs", serde_json::json!(0)),
        ] {
            let mut bad = good.clone();
            bad[k] = v;
            std::fs::write(&p, bad.to_string()).unwrap();
            assert!(MilestoneConfig::load(&p).is_err(), "{k}");
        }
        let mut extra = good.clone();
        extra["unknown"] = serde_json::json!(1);
        std::fs::write(&p, extra.to_string()).unwrap();
        assert!(MilestoneConfig::load(&p).is_err(), "unknown field");
    }
}
