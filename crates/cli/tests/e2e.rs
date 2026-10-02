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

/// `python3 -m http.server` on a free port (deliberately not the one the flow was recorded on).
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
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
            ])
            .current_dir(root().join("fixtures/shop"))
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

fn replay(url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_webtest"))
        .current_dir(root())
        .args([
            "replay",
            "flows/checkout.yaml",
            "--no-heal",
            "--timeout",
            "2",
            "--url",
            url,
        ])
        .output()
        .unwrap()
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
