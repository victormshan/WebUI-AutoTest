//! Real-Chrome integration tests (L2/L3). Run with `cargo test -p browser-runtime -- --ignored`;
//! Chrome from `BROWSER_RUNTIME_CHROME`, else `~/.local/bin/chrome-wsl`.

use std::time::Duration;

use browser_runtime::browser::group_alive;
use browser_runtime::{Browser, Error, LaunchConfig};
use serde_json::json;

/// A broken ownership invariant tends to hang (waiting on processes that never go): fail instead.
async fn bounded(f: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .expect("test did not finish within 60 s");
}

fn chrome() -> String {
    std::env::var("BROWSER_RUNTIME_CHROME").unwrap_or_else(|_| {
        format!(
            "{}/.local/bin/chrome-wsl",
            std::env::var("HOME").unwrap_or_default()
        )
    })
}

fn config(dir: &std::path::Path) -> LaunchConfig {
    LaunchConfig::new(chrome(), dir.join("profile"))
}

/// Pids of the process group, from /proc.
fn group_pids(pgid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // Fields after the ")" of the command name: state ppid pgrp …
        let rest = &stat[stat.rfind(')').unwrap() + 2..];
        if rest.split_whitespace().nth(2) == Some(&pgid.to_string()) {
            out.push(pid);
        }
    }
    out
}

/// Listening TCP sockets owned by any of `pids` (inode match through /proc/<pid>/fd).
fn listening_sockets_of(pids: &[i32]) -> Vec<String> {
    let mut listen_inodes = Vec::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in std::fs::read_to_string(f)
            .unwrap_or_default()
            .lines()
            .skip(1)
        {
            let c: Vec<&str> = line.split_whitespace().collect();
            if c.get(3) == Some(&"0A") {
                listen_inodes.push((c[1].to_string(), c[9].to_string()));
            }
        }
    }
    let mut found = Vec::new();
    for pid in pids {
        for fd in std::fs::read_dir(format!("/proc/{pid}/fd"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let Ok(link) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let l = link.to_string_lossy();
            if let Some(ino) = l.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')) {
                for (addr, i) in &listen_inodes {
                    if i == ino {
                        found.push(format!("pid {pid} listens on {addr}"));
                    }
                }
            }
        }
    }
    found
}

#[tokio::test]
#[ignore = "needs Chrome"]
async fn cdp_over_the_pipe_works_and_no_port_is_opened() {
    bounded(cdp_over_the_pipe_works_and_no_port_is_opened_body()).await
}

async fn cdp_over_the_pipe_works_and_no_port_is_opened_body() {
    let d = tempfile::tempdir().unwrap();
    let b = Browser::launch(config(d.path())).await.unwrap();
    let v = b.call("Browser.getVersion", json!({})).await.unwrap();
    assert!(v["product"].as_str().unwrap().starts_with("Chrome/"), "{v}");
    let mut ev = b.events();
    b.call("Target.setDiscoverTargets", json!({ "discover": true }))
        .await
        .unwrap();
    let t = b
        .call("Target.createTarget", json!({ "url": "about:blank" }))
        .await
        .unwrap();
    let target = t["targetId"].as_str().unwrap().to_string();
    let seen = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let e = ev.recv().await.unwrap();
            if e.method == "Target.targetCreated" && e.params["targetInfo"]["targetId"] == target {
                return true;
            }
        }
    })
    .await;
    assert_eq!(
        seen,
        Ok(true),
        "the targetCreated event for our tab arrives"
    );
    let pids = group_pids(b.pgid());
    assert!(
        pids.len() > 1,
        "Chrome runs helper processes in its group: {pids:?}"
    );
    assert_eq!(
        listening_sockets_of(&pids),
        Vec::<String>::new(),
        "no TCP listener"
    );
    // The profile is private to the running user.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(d.path().join("profile"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "profile dir mode {mode:o}");
    let pgid = b.pgid();
    assert_eq!(b.close().await.unwrap(), "graceful");
    assert!(!group_alive(pgid), "no process of the group is left");
}

#[tokio::test]
#[ignore = "needs Chrome"]
async fn a_crashed_chrome_is_a_disconnect_and_close_still_cleans_up() {
    bounded(a_crashed_chrome_is_a_disconnect_and_close_still_cleans_up_body()).await
}

async fn a_crashed_chrome_is_a_disconnect_and_close_still_cleans_up_body() {
    let d = tempfile::tempdir().unwrap();
    let b = Browser::launch(config(d.path())).await.unwrap();
    let pgid = b.pgid();
    // Kill only the browser process, as a crash would; helpers may linger.
    unsafe {
        libc::kill(pgid, libc::SIGKILL);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let e = b.call("Browser.getVersion", json!({})).await.unwrap_err();
    assert!(matches!(e, Error::BrowserDisconnected), "{e}");
    b.close().await.unwrap();
    assert!(!group_alive(pgid), "close reaps what the crash left");
}

#[tokio::test]
#[ignore = "needs Chrome"]
async fn dropping_the_browser_never_leaves_chrome_behind() {
    bounded(dropping_the_browser_never_leaves_chrome_behind_body()).await
}

async fn dropping_the_browser_never_leaves_chrome_behind_body() {
    let d = tempfile::tempdir().unwrap();
    let b = Browser::launch(config(d.path())).await.unwrap();
    let pgid = b.pgid();
    drop(b);
    assert!(!group_alive(pgid));
}

#[tokio::test]
#[ignore = "needs Chrome"]
async fn a_profile_in_use_refuses_a_second_browser() {
    bounded(a_profile_in_use_refuses_a_second_browser_body()).await
}

async fn a_profile_in_use_refuses_a_second_browser_body() {
    let d = tempfile::tempdir().unwrap();
    let first = Browser::launch(config(d.path())).await.unwrap();
    let mut c = config(d.path());
    c.command_timeout = Duration::from_secs(5);
    let second = Browser::launch(c).await;
    assert!(
        matches!(&second, Err(Error::Launch(_))),
        "{:?}",
        second.as_ref().err()
    );
    first.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs Chrome"]
async fn a_command_without_an_answer_times_out_and_says_the_effect_is_unknown() {
    bounded(a_command_without_an_answer_times_out_and_says_the_effect_is_unknown_body()).await
}

async fn a_command_without_an_answer_times_out_and_says_the_effect_is_unknown_body() {
    let d = tempfile::tempdir().unwrap();
    let mut c = config(d.path());
    c.command_timeout = Duration::from_secs(2);
    let b = Browser::launch(c).await.unwrap();
    let t = b
        .call("Target.createTarget", json!({ "url": "about:blank" }))
        .await
        .unwrap();
    let s = b
        .call(
            "Target.attachToTarget",
            json!({ "targetId": t["targetId"], "flatten": true }),
        )
        .await
        .unwrap();
    let session = s["sessionId"].as_str().unwrap();
    let e = b
        .call_in(
            Some(session),
            "Runtime.evaluate",
            json!({ "expression": "new Promise(() => {})", "awaitPromise": true }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Timeout { method, .. } if method == "Runtime.evaluate"),
        "{e}"
    );
    assert!(e.to_string().contains("effect unknown"));
    // The browser is still usable after a timed-out command.
    b.call("Browser.getVersion", json!({})).await.unwrap();
    b.close().await.unwrap();
}
