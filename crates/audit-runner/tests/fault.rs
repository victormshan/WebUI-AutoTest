//! Fault injection (`--features fault-injection`): a real job whose cleanup is reported
//! unconfirmed is never `completed`, and the runner stops serving after it. Its own test
//! binary, because the fault is switched on for the whole process.
#![cfg(feature = "fault-injection")]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::Duration;

use audit_runner::{Config, Request, Response, serve};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

const FAKE_CODEX: &str = r#"#!/bin/sh
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
cat >/dev/null
printf '%s\n' '{"type":"thread.started"}' '{"type":"turn.completed"}'
echo "VERDICT: APPROVE" > "$out"
"#;

#[tokio::test]
async fn an_unconfirmed_cleanup_is_never_completed_and_stops_the_runner() {
    // SAFETY: this test binary has a single test; nothing else reads the environment
    // concurrently.
    unsafe { std::env::set_var("AUDIT_RUNNER_FAULT", "cleanup-unconfirmed") };
    let dir = tempfile::tempdir().unwrap();
    let codex = dir.path().join("codex");
    std::fs::write(&codex, FAKE_CODEX).unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(dir.path().join("home/.codex")).unwrap();
    std::fs::write(dir.path().join("home/.codex/auth.json"), "{}").unwrap();
    std::fs::create_dir_all(dir.path().join("jobs")).unwrap();
    let cfg = Config {
        allow_uid: std::fs::metadata("/proc/self").unwrap().uid(),
        allow_cgroup: None,
        codex,
        codex_home: dir.path().join("home/.codex"),
        run_home: dir.path().join("run"),
        workdir: dir.path().join("jobs"),
        default_timeout: Duration::from_secs(20),
        reply_timeout: Duration::from_secs(5),
        own_cgroup: false,
        reasoning_effort: "high".into(),
    };
    let socket = dir.path().join("f.sock");
    let task = tokio::spawn(serve(cfg, UnixListener::bind(&socket).unwrap()));
    let s = UnixStream::connect(&socket).await.unwrap();
    let (r, mut w) = s.into_split();
    let req = Request {
        job_id: "f1".into(),
        input: "x".into(),
        timeout_secs: None,
    };
    w.write_all(format!("{}\n", serde_json::to_string(&req).unwrap()).as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await.unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert_eq!(resp.status, "failed", "{resp:?}");
    assert!(!resp.evidence_complete);
    assert_eq!(resp.cleanup, "not confirmed: injected fault");
    let ended = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap();
    assert!(ended.unwrap().is_err(), "the runner must stop");
    assert!(UnixStream::connect(&socket).await.is_err());
}
