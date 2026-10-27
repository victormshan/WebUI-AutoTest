//! The halted milestone service ends with exit 3 within the deadline (M2 step 6c, Codex n4 §1
//! and n6 conditions 1–2), checked on a real `review-gate serve` child process.
//!
//! With `--features fault-injection` the child takes faults from `REVIEW_GATE_FAULT`; every
//! ordering is confirmed by a signal (the runner stand-in has read the line, the stalled status
//! call has started) before the halt is triggered, and times are measured from the trigger.
//! Without the feature, the same variable must have no effect (the negative case).

#![cfg_attr(not(feature = "fault-injection"), allow(unused_imports, dead_code))]

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "test-token-0123456789abcdef0123456789";

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    port: u16,
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir(root.join("state")).unwrap();
    std::fs::write(root.join("client.token"), TOKEN).unwrap();
    // The test's own identities, passed with --identity-config: without it the service would
    // read the host's /etc/review-gate/identity.json when that exists (m2d/R1).
    let etc = root.join("etc");
    std::fs::create_dir(&etc).unwrap();
    std::fs::set_permissions(&etc, std::fs::Permissions::from_mode(0o750)).unwrap();
    let ids = etc.join("identity.json");
    std::fs::write(
        &ids,
        format!(
            r#"{{"version": 1, "principals": [{{"id": "legacy-claude", "family": "claude", "credentials": [{{"id": "t1", "sha256": "{}"}}]}}]}}"#,
            review_gate::identity::token_hash(TOKEN)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&ids, std::fs::Permissions::from_mode(0o640)).unwrap();
    let bt = root.join("bridge.token");
    std::fs::write(&bt, "bridge-observer-token-0123456789abcdef").unwrap();
    std::fs::set_permissions(&bt, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::create_dir(root.join("repo")).unwrap();
    git(&root.join("repo"), &["init", "-q"]);
    std::fs::create_dir(root.join("remote.git")).unwrap();
    git(&root.join("remote.git"), &["init", "-q", "--bare"]);
    let config = serde_json::json!({
        "repo_id": "repo",
        "repo": root.join("repo"),
        "remote_url": format!("file://{}", root.join("remote.git").display()),
        "runner_socket": root.join("runner.sock"),
        "runner_uid": 0,
        "runner_cgroup": "/x",
        "job_timeout_secs": 60,
        "auditor": "openai",
        "implementers": ["claude"],
        "bridge_url": "http://127.0.0.1:9",
        "bridge_token_file": bt,
        "writers": ["legacy-claude"],
    });
    std::fs::write(root.join("milestones.json"), config.to_string()).unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    Env {
        _dir: dir,
        root,
        port,
    }
}

/// The service child: killed and reaped when dropped, so a failing assertion leaves nothing
/// running (DSH's 6c review ③).
struct Service(Child);

impl std::ops::Deref for Service {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Service {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spawn(e: &Env, fault: &str) -> Service {
    let child = Command::new(env!("CARGO_BIN_EXE_review-gate"))
        .arg("serve")
        .arg("--state")
        .arg(e.root.join("state"))
        .arg("--listen")
        .arg(format!("127.0.0.1:{}", e.port))
        .arg("--token-file")
        .arg(e.root.join("client.token"))
        .arg("--milestones")
        .arg(e.root.join("milestones.json"))
        .arg("--identity-config")
        .arg(e.root.join("etc/identity.json"))
        .env("REVIEW_GATE_FAULT", fault)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Service(child)
}

/// Waits until the child answers /health.
fn wait_ready(e: &Env, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "the service exited early"
        );
        if std::net::TcpStream::connect(("127.0.0.1", e.port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the service did not start");
}

/// Waits for the child to exit within `limit`; kills it (and fails) otherwise.
fn wait_exit(child: &mut Child, limit: Duration) -> (Option<i32>, Duration) {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Some(s) = child.try_wait().unwrap() {
            return (s.code(), start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the service did not exit within {limit:?}");
}

/// A status call on another thread (it may never return).
fn status_call(port: u16) -> std::sync::mpsc::Receiver<u16> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let code = rt.block_on(async {
            reqwest::Client::new()
                .get(format!("http://127.0.0.1:{port}/milestones/p"))
                .bearer_auth(TOKEN)
                .send()
                .await
                .map(|r| r.status().as_u16())
                .unwrap_or(0)
        });
        let _ = tx.send(code);
    });
    rx
}

fn wait_file(p: &Path, limit: Duration) {
    let start = Instant::now();
    while !p.exists() {
        assert!(start.elapsed() < limit, "{} never appeared", p.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_halt_with_nothing_in_flight_exits_3_at_once() {
    let e = setup();
    let trigger = e.root.join("halt");
    let mut child = spawn(&e, &format!("halt-on={}", trigger.display()));
    wait_ready(&e, &mut child);
    std::fs::write(&trigger, "").unwrap();
    let (code, took) = wait_exit(&mut child, Duration::from_secs(15));
    eprintln!("exit {code:?} after {took:?} from the halt trigger");
    assert_eq!(code, Some(3), "exit status");
    assert!(took < Duration::from_secs(2), "graceful stop took {took:?}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_halt_with_a_stuck_call_a_silent_runner_and_a_blocked_stderr_exits_3_at_the_deadline() {
    let e = setup();
    // The runner stand-in: reads the request line, signals, and never replies.
    let listener = UnixListener::bind(e.root.join("runner.sock")).unwrap();
    let (got_line, line_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((s, _)) = listener.accept() {
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            got_line.send(line).unwrap();
            loop {
                std::thread::park();
            }
        }
    });
    let trigger = e.root.join("halt");
    let entered = e.root.join("stall-entered");
    let mut child = spawn(
        &e,
        &format!(
            "halt-on={},stall-status={},runner-probe,stall-stderr",
            trigger.display(),
            entered.display()
        ),
    );
    wait_ready(&e, &mut child);
    // Both are in flight before the halt: the runner has the whole line, the status call has
    // entered its blocking thread.
    let line = line_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    assert!(line.ends_with('\n'), "the whole line was read");
    let pending = status_call(e.port);
    wait_file(&entered, Duration::from_secs(20));
    std::fs::write(&trigger, "").unwrap();
    let (code, took) = wait_exit(&mut child, Duration::from_secs(15));
    eprintln!("exit {code:?} after {took:?} from the halt trigger");
    assert_eq!(code, Some(3), "exit status");
    assert!(
        took >= Duration::from_millis(4900) && took < Duration::from_secs(8),
        "ended by the deadline, not before and not much after: {took:?}"
    );
    // The stuck call never got a 200: the connection was dropped with the process.
    assert_ne!(pending.recv_timeout(Duration::from_secs(5)).ok(), Some(200));
}

/// Without the feature the variable does nothing: no stall, no halt.
#[cfg(not(feature = "fault-injection"))]
#[test]
fn without_the_feature_faults_are_ignored() {
    let e = setup();
    let trigger = e.root.join("halt");
    let entered = e.root.join("stall-entered");
    std::fs::write(&trigger, "").unwrap();
    let mut child = spawn(
        &e,
        &format!(
            "halt-on={},stall-status={},stall-stderr",
            trigger.display(),
            entered.display()
        ),
    );
    wait_ready(&e, &mut child);
    let code = status_call(e.port)
        .recv_timeout(Duration::from_secs(20))
        .expect("the status call returned");
    assert_ne!(code, 503, "not halted");
    assert_ne!(code, 0, "answered");
    assert!(!entered.exists(), "no stall");
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "still running");
    let _ = child.kill();
    let _ = child.wait();
}
