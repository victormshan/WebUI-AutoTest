//! `serve` start-up with observers (observe.rs): which file is read, and that a bad one stops the
//! service before it creates or writes any state.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agent_bridge::service::{hex, sha256};

const BIN: &str = env!("CARGO_BIN_EXE_agent-bridge");

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".config/agent-bridge");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("agents.json"),
            format!(
                r#"{{"claude":"{}","dsh":"{}"}}"#,
                hex(&sha256("claude-token")),
                hex(&sha256("dsh-token"))
            ),
        )
        .unwrap();
        Home { dir }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn file(&self, name: &str, content: &str) -> PathBuf {
        let p = self.path().join(name);
        std::fs::write(&p, content).unwrap();
        p
    }
    fn serve(&self, args: &[&str]) -> Child {
        Command::new(BIN)
            .arg("serve")
            .args(["--listen", "127.0.0.1:0"])
            .args(args)
            .env("HOME", self.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

/// Stderr up to the "listening" line (then the service is stopped), or all of it if the
/// process exits first; and whether it was still running.
fn run(mut c: Child) -> (String, bool) {
    let err = c.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines() {
            let Ok(l) = line else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    let mut out = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(l) => {
                let listening = l.contains("listening on");
                out.push_str(&l);
                out.push('\n');
                if listening {
                    c.kill().unwrap();
                    c.wait().unwrap();
                    return (out, true);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let st = c.wait().unwrap();
                assert!(!st.success(), "exited 0 without listening: {out}");
                return (out, false);
            }
            Err(_) if Instant::now() > deadline => {
                c.kill().unwrap();
                panic!("neither listening nor exiting: {out}");
            }
            Err(_) => {}
        }
    }
}

fn valid() -> String {
    format!(r#"{{"review-gate":"{}"}}"#, hex(&sha256("observer-token")))
}

#[test]
fn an_isolated_state_never_reads_the_home_observers() {
    let h = Home::new();
    h.file(".config/agent-bridge/observers.json", &valid());
    let state = h.path().join("state");
    let (out, up) = run(h.serve(&["--state", state.to_str().unwrap()]));
    assert!(up, "{out}");
    assert!(out.contains("read-only observers []"), "{out}");
    // Even a broken one there is not read.
    h.file(".config/agent-bridge/observers.json", "{");
    let (out, up) = run(h.serve(&["--state", state.to_str().unwrap()]));
    assert!(up && out.contains("read-only observers []"), "{out}");
}

#[test]
fn the_default_state_reads_the_default_file_and_an_explicit_path_is_used() {
    let h = Home::new();
    // Without the file: no observers, as before.
    let (out, up) = run(h.serve(&[]));
    assert!(up && out.contains("read-only observers []"), "{out}");
    h.file(".config/agent-bridge/observers.json", &valid());
    let (out, up) = run(h.serve(&[]));
    assert!(
        up && out.contains(r#"read-only observers ["review-gate"]"#),
        "{out}"
    );
    let p = h.file("obs.json", &valid());
    let state = h.path().join("state2");
    let (out, up) = run(h.serve(&[
        "--state",
        state.to_str().unwrap(),
        "--observers",
        p.to_str().unwrap(),
    ]));
    assert!(
        up && out.contains(r#"read-only observers ["review-gate"]"#),
        "{out}"
    );
}

#[test]
fn a_bad_observers_file_stops_start_up_before_any_state_is_written() {
    let h = Home::new();
    let agent_hash = hex(&sha256("dsh-token"));
    let marker = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let cases: Vec<(&str, String)> = vec![
        (
            "duplicate key",
            format!(r#"{{"g":"{marker}","g":"{}"}}"#, hex(&sha256("x"))),
        ),
        ("empty", String::new()),
        ("empty object", "{}".into()),
        ("non-ASCII", format!(r#"{{"g":"{}"}}"#, "é".repeat(32))),
        ("63 digits", format!(r#"{{"g":"{}"}}"#, &marker[..63])),
        ("agent's hash", format!(r#"{{"g":"{agent_hash}"}}"#)),
        ("agent's name", format!(r#"{{"dsh":"{marker}"}}"#)),
        ("not a string", format!(r#"{{"g":["{marker}"]}}"#)),
    ];
    for (i, (name, content)) in cases.iter().enumerate() {
        let p = h.file(&format!("obs{i}.json"), content);
        let state = h.path().join(format!("state{i}"));
        let (out, up) = run(h.serve(&[
            "--state",
            state.to_str().unwrap(),
            "--observers",
            p.to_str().unwrap(),
        ]));
        assert!(!up, "{name}: started: {out}");
        assert!(!state.exists(), "{name}: state created");
        assert!(
            !out.contains(&marker[..20]),
            "{name}: value in the error: {out}"
        );
        if *name == "agent's hash" {
            assert!(!out.contains(&agent_hash[..20]), "{name}: {out}");
        }
    }
    let state = h.path().join("state-missing");
    let (out, up) = run(h.serve(&[
        "--state",
        state.to_str().unwrap(),
        "--observers",
        h.path().join("no-such.json").to_str().unwrap(),
    ]));
    assert!(!up && !state.exists(), "explicit missing file: {out}");
}

#[test]
fn an_unreadable_observers_file_stops_start_up() {
    use std::os::unix::fs::PermissionsExt;
    let h = Home::new();
    let p = h.file("obs.json", &valid());
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&p).is_ok() {
        eprintln!(
            "SKIPPED an_unreadable_observers_file_stops_start_up: this user can read a 0000 file"
        );
        return;
    }
    let state = h.path().join("state");
    let (out, up) = run(h.serve(&[
        "--state",
        state.to_str().unwrap(),
        "--observers",
        p.to_str().unwrap(),
    ]));
    assert!(!up && !state.exists(), "{out}");
    assert!(out.contains("permission denied"), "{out}");
}
