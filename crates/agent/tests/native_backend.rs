//! NativeBackend lifecycle against a real Chrome (B1 phase 2a): no debugging port, and no
//! process or profile left behind after close, drop, a failed launch, a crash or a
//! cancelled task. Ignored by default: `cargo test -p agent -- --ignored`.
//! Chrome from `WEBTEST_CHROME`, else `~/.local/bin/chrome-wsl`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent::native::{NativeBackend, NativeConfig};
use agent::{BrowserBackend, Coverage};
use browser_runtime::browser::group_alive;

fn chrome() -> PathBuf {
    std::env::var_os("WEBTEST_CHROME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".local/bin/chrome-wsl")
        })
}

fn config(root: &Path) -> NativeConfig {
    NativeConfig {
        profile_root: Some(root.to_path_buf()),
        ..NativeConfig::new(chrome())
    }
}

/// A broken cleanup invariant tends to hang: fail instead.
async fn bounded<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(90), f)
        .await
        .expect("test did not finish within 90 s")
}

/// Pids of a process group, from /proc.
fn group_pids(pgid: i32) -> Vec<i32> {
    all_pids()
        .into_iter()
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                // Fields after the command name: state ppid pgrp …
                let rest = &stat[stat.rfind(')').unwrap() + 2..];
                rest.split_whitespace().nth(2) == Some(&pgid.to_string())
            })
        })
        .collect()
}

fn all_pids() -> Vec<i32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
        .collect()
}

fn cmdline(pid: i32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

/// Processes whose command line mentions `needle` (e.g. a profile root).
fn processes_mentioning(needle: &Path) -> Vec<String> {
    let needle = needle.display().to_string();
    all_pids()
        .into_iter()
        .filter(|&pid| pid != std::process::id() as i32)
        .map(|pid| (pid, cmdline(pid)))
        .filter(|(_, c)| c.contains(&needle))
        .map(|(pid, c)| format!("{pid}: {}", c.chars().take(160).collect::<String>()))
        .collect()
}

/// Listening TCP sockets owned by any of `pids` (inode match through /proc/<pid>/fd).
fn listening_sockets_of(pids: &[i32]) -> Vec<String> {
    let mut listen = Vec::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in std::fs::read_to_string(f)
            .unwrap_or_default()
            .lines()
            .skip(1)
        {
            let c: Vec<&str> = line.split_whitespace().collect();
            if c.get(3) == Some(&"0A") {
                listen.push((c[1].to_string(), c[9].to_string()));
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
                for (addr, i) in &listen {
                    if i == ino {
                        found.push(format!("pid {pid} listens on {addr}"));
                    }
                }
            }
        }
    }
    found
}

fn entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Waits (bounded) for a condition that cleanup should reach soon.
async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < end, "not within 10 s: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "needs Chrome"]
async fn runs_over_a_pipe_with_no_port_and_cleans_up_on_close() {
    bounded(async {
        let root = tempfile::tempdir().unwrap();
        let b = NativeBackend::launch(&config(root.path())).await.unwrap();
        let pgid = b.pgid();
        let profile = b.profile_dir().to_path_buf();

        let nav = b
            .navigate("data:text/html,<title>t</title><p>hi</p>")
            .await
            .unwrap();
        assert!(!nav.is_error, "{}", nav.text);

        let pids = group_pids(pgid);
        assert!(!pids.is_empty());
        for pid in &pids {
            let c = cmdline(*pid);
            assert!(!c.contains("remote-debugging-port"), "pid {pid}: {c}");
        }
        assert!(
            cmdline(pgid).contains("--remote-debugging-pipe"),
            "{}",
            cmdline(pgid)
        );
        assert!(
            listening_sockets_of(&pids).is_empty(),
            "{:?}",
            listening_sockets_of(&pids)
        );
        assert!(!profile.join("chrome/DevToolsActivePort").exists());

        let how = b.shutdown().await.unwrap();
        eprintln!("closed: {how}");
        assert!(!group_alive(pgid), "group left after close");
        assert!(!profile.exists(), "profile left after close");
        assert!(processes_mentioning(root.path()).is_empty());
    })
    .await
}

#[tokio::test]
#[ignore = "needs Chrome"]
async fn dropping_the_backend_leaves_nothing_behind() {
    bounded(async {
        let root = tempfile::tempdir().unwrap();
        let b = NativeBackend::launch(&config(root.path())).await.unwrap();
        let pgid = b.pgid();
        drop(b);
        assert!(!group_alive(pgid), "group left after drop");
        assert!(
            entries(root.path()).is_empty(),
            "{:?}",
            entries(root.path())
        );
        assert!(processes_mentioning(root.path()).is_empty());
    })
    .await
}

#[tokio::test]
#[ignore = "needs Chrome"]
async fn a_failed_launch_leaves_nothing_behind() {
    bounded(async {
        let root = tempfile::tempdir().unwrap();
        // A "Chrome" that exits at once, one that does not exist, and a refused port flag.
        let mut port = config(root.path());
        port.chrome_args.push("--remote-debugging-port=0".into());
        for (name, cfg) in [
            (
                "exits at once",
                NativeConfig {
                    chrome: "/bin/false".into(),
                    ..config(root.path())
                },
            ),
            (
                "missing binary",
                NativeConfig {
                    chrome: "/nonexistent/chrome".into(),
                    ..config(root.path())
                },
            ),
            ("debugging port", port),
        ] {
            let err = NativeBackend::launch(&cfg)
                .await
                .err()
                .unwrap_or_else(|| panic!("{name}: launched"));
            eprintln!("{name}: {err:#}");
            assert!(
                entries(root.path()).is_empty(),
                "{name}: {:?}",
                entries(root.path())
            );
            assert!(processes_mentioning(root.path()).is_empty(), "{name}");
        }
    })
    .await
}

/// A crash is a disconnect: calls fail fast, diagnostics say "failed", and no new Chrome
/// is started behind the caller's back.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn a_crash_is_a_disconnect_and_nothing_restarts() {
    bounded(async {
        let root = tempfile::tempdir().unwrap();
        let b = NativeBackend::launch(&config(root.path())).await.unwrap();
        let pgid = b.pgid();
        assert!(matches!(
            b.diagnostics().await.console,
            Coverage::Unsupported
        ));

        // Kill only the browser process, as a crash would.
        unsafe {
            libc::kill(pgid, libc::SIGKILL);
        }
        eventually("disconnect noticed", || b.is_disconnected()).await;

        let started = Instant::now();
        let err = b.navigate("data:text/html,<p>after</p>").await.unwrap_err();
        assert!(
            err.downcast_ref::<browser_runtime::Error>()
                .is_some_and(|e| matches!(e, browser_runtime::Error::BrowserDisconnected)),
            "{err:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );

        let d = b.diagnostics().await;
        assert!(matches!(d.console, Coverage::Failed { .. }), "{d:?}");
        assert!(matches!(d.network, Coverage::Failed { .. }), "{d:?}");

        // Give a would-be restart time to happen, then look for any Chrome on this profile.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let leader_gone = !std::path::Path::new(&format!("/proc/{pgid}")).exists()
            || std::fs::read_to_string(format!("/proc/{pgid}/stat"))
                .is_ok_and(|s| s.contains(") Z "));
        assert!(leader_gone, "the browser process came back");
        let new_chromes: Vec<_> = processes_mentioning(root.path())
            .into_iter()
            .filter(|p| {
                let pid: i32 = p.split(':').next().unwrap().parse().unwrap();
                !group_pids(pgid).contains(&pid)
            })
            .collect();
        assert!(
            new_chromes.is_empty(),
            "a new Chrome appeared: {new_chromes:?}"
        );

        b.shutdown().await.unwrap();
        assert!(!group_alive(pgid), "close reaps what the crash left");
        assert!(
            entries(root.path()).is_empty(),
            "{:?}",
            entries(root.path())
        );
    })
    .await
}

/// Aborting the task that owns a backend (e.g. a flow's timeout) cleans up like close.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn aborting_the_owning_task_leaves_nothing_behind() {
    bounded(async {
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cfg = config(root.path());
        let task = tokio::spawn(async move {
            let b = NativeBackend::launch(&cfg).await.unwrap();
            tx.send(b.pgid()).unwrap();
            std::future::pending::<()>().await;
            drop(b);
        });
        let pgid = rx.await.unwrap();
        assert!(group_alive(pgid));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!group_alive(pgid), "group left after abort");
        assert!(
            entries(root.path()).is_empty(),
            "{:?}",
            entries(root.path())
        );
        assert!(processes_mentioning(root.path()).is_empty());
    })
    .await
}

/// Cancelling a launch at any point (before, during or after Chrome starts) leaves no
/// process and no profile.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn cancelling_a_launch_at_any_point_leaves_nothing_behind() {
    bounded(async {
        let root = tempfile::tempdir().unwrap();
        for ms in [0u64, 5, 20, 50, 100, 200, 400, 800] {
            let cfg = config(root.path());
            let outcome =
                tokio::time::timeout(Duration::from_millis(ms), NativeBackend::launch(&cfg)).await;
            let cancelled = outcome.is_err();
            if let Ok(b) = outcome {
                // Fast machine: finished in time; dropping it is the same cleanup path.
                drop(b.unwrap());
            }
            eventually(&format!("cleanup after cancel at {ms} ms"), || {
                entries(root.path()).is_empty() && processes_mentioning(root.path()).is_empty()
            })
            .await;
            eprintln!(
                "{} at {ms} ms: clean",
                if cancelled {
                    "cancelled"
                } else {
                    "launched, dropped"
                }
            );
        }
    })
    .await
}

/// Concurrent backends share nothing: separate groups and profiles, and closing or
/// crashing one leaves the others working.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn concurrent_backends_are_isolated() {
    bounded(async {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path());
        let (a, b, c) = tokio::join!(
            NativeBackend::launch(&cfg),
            NativeBackend::launch(&cfg),
            NativeBackend::launch(&cfg)
        );
        let (a, b, c) = (a.unwrap(), b.unwrap(), c.unwrap());
        let pgids = [a.pgid(), b.pgid(), c.pgid()];
        let profiles = [
            a.profile_dir().to_owned(),
            b.profile_dir().to_owned(),
            c.profile_dir().to_owned(),
        ];
        assert!(pgids[0] != pgids[1] && pgids[1] != pgids[2] && pgids[0] != pgids[2]);
        assert!(
            profiles[0] != profiles[1] && profiles[1] != profiles[2] && profiles[0] != profiles[2]
        );
        eprintln!("3 concurrent backends: pgids {pgids:?}");

        // Close one, crash another; the third must not notice.
        a.shutdown().await.unwrap();
        unsafe {
            libc::kill(pgids[1], libc::SIGKILL);
        }
        eventually("b disconnected", || b.is_disconnected()).await;
        assert!(!group_alive(pgids[0]));
        assert!(!c.is_disconnected());
        let nav = c
            .navigate("data:text/html,<p>still here</p>")
            .await
            .unwrap();
        assert!(!nav.is_error, "{}", nav.text);
        assert!(group_alive(pgids[2]));
        assert!(profiles[2].exists());

        b.shutdown().await.unwrap();
        c.shutdown().await.unwrap();
        assert!(pgids.iter().all(|&p| !group_alive(p)));
        assert!(
            entries(root.path()).is_empty(),
            "{:?}",
            entries(root.path())
        );
    })
    .await
}

#[tokio::test]
#[ignore = "needs Chrome"]
async fn unimplemented_parts_say_so() {
    bounded(async {
        let root = tempfile::tempdir().unwrap();
        let b = NativeBackend::launch(&config(root.path())).await.unwrap();
        assert!(b.tools().await.unwrap().is_empty());
        let out = b
            .call("click", serde_json::json!({"uid": "1_1"}))
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.text);
        assert!(format!("{:#}", b.snapshot().await.unwrap_err()).contains("not implemented"));
        assert!(b.storage_state().await.is_err());
        let d = b.diagnostics().await;
        assert_eq!(d.console, Coverage::Unsupported);
        assert_eq!(d.network, Coverage::Unsupported);
        assert_eq!(d.gaps().len(), 2);
        let bad = b.navigate("http://127.0.0.1:1/").await.unwrap();
        assert!(
            bad.is_error,
            "an unreachable page is a tool error: {}",
            bad.text
        );
        Box::new(b).close().await.unwrap();
        assert!(entries(root.path()).is_empty());
    })
    .await
}

/// How a test server hangs.
#[derive(Clone, Copy, Debug)]
enum Hang {
    /// Accepts and never answers: `Page.navigate` itself waits for the response.
    BeforeHeaders,
    /// Sends headers and part of the body, then stalls: navigation commits, but the load
    /// event never fires.
    MidBody,
}

async fn hanging_server(hang: Hang) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    let h = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut sock, _)) = l.accept().await {
            if let Hang::MidBody = hang {
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<p>partial")
                    .await;
            }
            held.push(sock);
        }
    });
    (url, h)
}

/// A page that never loads is a timeout with an unknown outcome (`Err`), not a tool
/// error and not a success, whichever stage it hangs in.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn a_page_that_never_loads_times_out() {
    bounded(async {
        let root = tempfile::tempdir().unwrap();
        for hang in [Hang::BeforeHeaders, Hang::MidBody] {
            let (url, server) = hanging_server(hang).await;
            let cfg = NativeConfig {
                command_timeout: Duration::from_secs(3),
                ..config(root.path())
            };
            let b = NativeBackend::launch(&cfg).await.unwrap();
            let started = Instant::now();
            let err = b.navigate(&url).await.unwrap_err();
            let msg = format!("{err:#}");
            // BeforeHeaders: Page.navigate itself times out. MidBody: the load wait does.
            let expected = match hang {
                Hang::BeforeHeaders => err
                    .downcast_ref::<browser_runtime::Error>()
                    .is_some_and(|e| matches!(e, browser_runtime::Error::Timeout { .. })),
                Hang::MidBody => {
                    msg.contains("did not finish loading") && msg.contains("outcome unknown")
                }
            };
            assert!(expected, "{hang:?}: {msg}");
            assert!(
                started.elapsed() >= Duration::from_secs(3),
                "{:?}",
                started.elapsed()
            );
            eprintln!("{hang:?} timeout: {msg}");
            b.shutdown().await.unwrap();
            server.abort();
        }
    })
    .await
}

/// Chrome dying while a page loads ends the wait at once as a disconnect, instead of
/// waiting out the load timeout, whichever stage the load is in.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn a_crash_during_a_load_is_a_disconnect_right_away() {
    bounded(async {
        let root = tempfile::tempdir().unwrap();
        for hang in [Hang::BeforeHeaders, Hang::MidBody] {
            let (url, server) = hanging_server(hang).await;
            let cfg = NativeConfig {
                command_timeout: Duration::from_secs(30),
                ..config(root.path())
            };
            let b = NativeBackend::launch(&cfg).await.unwrap();
            let pgid = b.pgid();
            let killer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                unsafe {
                    libc::kill(pgid, libc::SIGKILL);
                }
            });
            let started = Instant::now();
            let err = b.navigate(&url).await.unwrap_err();
            let took = started.elapsed();
            assert!(
                err.downcast_ref::<browser_runtime::Error>()
                    .is_some_and(|e| matches!(e, browser_runtime::Error::BrowserDisconnected)),
                "{hang:?}: {err:#}"
            );
            assert!(took < Duration::from_secs(5), "{hang:?}: waited {took:?}");
            eprintln!("{hang:?}: disconnect reported after {took:?}");
            killer.await.unwrap();
            b.shutdown().await.unwrap();
            server.abort();
        }
        assert!(entries(root.path()).is_empty());
    })
    .await
}
