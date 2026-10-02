//! End-to-end replay tests against `fixtures/shop` (no LLM needed).
//!
//! They need Node.js (npx) and a Chrome, so they are ignored by default:
//! `cargo test -p webtest -- --ignored`

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Output};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `fixtures/shop/server.py` on a free port (deliberately not the one the flows were recorded on).
struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start() -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new("python3")
            .arg(root().join("fixtures/shop/server.py"))
            .arg(port.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("python3 is required for e2e tests");
        for _ in 0..50 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Self { child, port }
    }

    fn url(&self, page: &str) -> String {
        format!("http://127.0.0.1:{}/{page}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn webtest(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_webtest"))
        .current_dir(root())
        .args(args)
        .output()
        .unwrap()
}

fn replay(url: &str) -> Output {
    webtest(&[
        "replay",
        "flows/checkout.yaml",
        "--no-heal",
        "--timeout",
        "2",
        "--url",
        url,
    ])
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
#[ignore = "needs Chrome + npx"]
fn passes_on_original_site_under_other_origin() {
    let s = Server::start();
    let o = replay(&s.url(""));
    assert!(
        o.status.success(),
        "{}{}",
        stdout(&o),
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
#[ignore = "needs Chrome + npx"]
fn renamed_button_fails_without_healing() {
    let s = Server::start();
    let o = replay(&s.url("v2-renamed.html"));
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stdout(&o).contains(r#"element button "登录" not found"#),
        "{}",
        stdout(&o)
    );
}

#[test]
#[ignore = "needs Chrome + npx"]
fn wrong_total_fails_assertion() {
    let s = Server::start();
    let o = replay(&s.url("v3-bug.html"));
    assert_eq!(o.status.code(), Some(1));
    assert!(stdout(&o).contains("assertion failed"), "{}", stdout(&o));
}

#[test]
#[ignore = "needs Chrome + npx"]
fn new_console_error_fails() {
    let s = Server::start();
    let o = replay(&s.url("v4-console.html"));
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stdout(&o).contains("new console error: [error] pricing"),
        "{}",
        stdout(&o)
    );
}

#[test]
#[ignore = "needs Chrome + npx"]
fn reuses_saved_login_state() {
    let s = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("alice.json");
    let state = state.to_str().unwrap();

    let o = webtest(&[
        "login",
        "--flow",
        "flows/login.yaml",
        "--url",
        &s.url(""),
        "--save-state",
        state,
    ]);
    assert!(
        o.status.success(),
        "{}{}",
        stdout(&o),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(stdout(&o).contains("saved 1 cookies"), "{}", stdout(&o));

    // Starts on the shop page: only works if the HttpOnly session cookie was restored.
    let o = webtest(&[
        "--storage-state",
        state,
        "replay",
        "flows/checkout_logged_in.yaml",
        "--no-heal",
        "--timeout",
        "3",
        "--url",
        &s.url(""),
    ]);
    assert!(
        o.status.success(),
        "{}{}",
        stdout(&o),
        String::from_utf8_lossy(&o.stderr)
    );

    // Without the state the same flow cannot find the shop.
    let o = webtest(&[
        "--storage-state",
        "/nonexistent.json",
        "replay",
        "flows/checkout_logged_in.yaml",
        "--no-heal",
        "--url",
        &s.url(""),
    ]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "missing state file is a usage error"
    );
}

#[test]
#[ignore = "needs Chrome + npx"]
fn refreshes_expired_session_and_retries() {
    // Cookies are not port-scoped: a session from server A is sent to server B,
    // which does not know it — exactly what an expired session looks like.
    let a = Server::start();
    let b = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("alice.json");
    let state = state.to_str().unwrap();

    let o = webtest(&[
        "login",
        "--flow",
        "flows/login.yaml",
        "--url",
        &a.url(""),
        "--save-state",
        state,
    ]);
    assert!(o.status.success(), "{}", stdout(&o));

    let o = webtest(&[
        "--storage-state",
        state,
        "--login-flow",
        "flows/login.yaml",
        "replay",
        "flows/checkout_logged_in.yaml",
        "--no-heal",
        "--timeout",
        "2",
        "--url",
        &b.url(""),
    ]);
    assert!(
        o.status.success(),
        "{}{}",
        stdout(&o),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        stdout(&o).contains("PASS (session refreshed)"),
        "{}",
        stdout(&o)
    );
}

#[test]
#[ignore = "needs Chrome + npx"]
fn creates_missing_state_via_login_flow() {
    let s = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("new.json");
    let o = webtest(&[
        "--storage-state",
        state.to_str().unwrap(),
        "--login-flow",
        "flows/login.yaml",
        "replay",
        "flows/checkout_logged_in.yaml",
        "--no-heal",
        "--url",
        &s.url(""),
    ]);
    assert!(
        o.status.success(),
        "{}{}",
        stdout(&o),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(state.exists());
}
