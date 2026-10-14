//! audit-runner over a real unix socket with a fake Codex.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use audit_runner::{Config, Request, Response, serve, sha256_hex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Fake `codex exec`: the input text chooses what it does, since the runner clears the
/// environment. It reports one declined command execution, like a read-only sandbox does.
const FAKE_CODEX: &str = r#"#!/bin/sh
out=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then out="$a"; fi
  prev="$a"
done
input=$(cat)
case "$input" in
  *MODE:sleep*) sleep 4747 & sleep 4748 ;;
  *MODE:fail*) echo "boom" >&2; exit 3 ;;
  *MODE:noanswer*) exit 0 ;;
esac
echo '{"type":"thread.started","thread_id":"t1"}'
echo '{"type":"item.completed","item":{"type":"command_execution","command":"id","status":"declined"}}'
echo '{"type":"item.completed","item":{"type":"agent_message","text":"done"}}'
printf 'VERDICT: APPROVE\nenv-interop=%s\n' "${WSL_INTEROP:-unset}" > "$out"
"#;

fn my_uid() -> u32 {
    std::fs::metadata("/proc/self").unwrap().uid()
}

struct Rig {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    workdir: PathBuf,
}

async fn rig(allow_uid: u32) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let codex = dir.path().join("codex");
    std::fs::write(&codex, FAKE_CODEX).unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    let socket = dir.path().join("runner.sock");
    let workdir = dir.path().join("jobs");
    std::fs::create_dir_all(&workdir).unwrap();
    let cfg = Config {
        allow_uid,
        allow_exe: None,
        codex,
        codex_home: dir.path().join("home/.codex"),
        workdir: workdir.clone(),
        default_timeout: Duration::from_secs(20),
        reasoning_effort: "high".into(),
    };
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(cfg, listener));
    Rig {
        _dir: dir,
        socket,
        workdir,
    }
}

async fn ask(socket: &Path, line: &str) -> Response {
    let s = UnixStream::connect(socket).await.unwrap();
    let (r, mut w) = s.into_split();
    w.write_all(line.as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let mut reply = String::new();
    BufReader::new(r).read_line(&mut reply).await.unwrap();
    serde_json::from_str(&reply).unwrap()
}

fn job(id: &str, input: &str, timeout: Option<u64>) -> String {
    serde_json::to_string(&Request {
        job_id: id.into(),
        input: input.into(),
        timeout_secs: timeout,
    })
    .unwrap()
}

#[tokio::test]
async fn a_job_runs_with_its_own_digest_and_a_clean_environment() {
    let r = rig(my_uid()).await;
    let input = "audit this\nmaterial";
    let resp = ask(&r.socket, &job("j1", input, None)).await;
    assert_eq!(resp.status, "completed", "{resp:?}");
    assert_eq!(resp.job_id, "j1");
    assert_eq!(resp.input_sha256, sha256_hex(input.as_bytes()));
    assert!(resp.answer.starts_with("VERDICT: APPROVE"));
    // The runner clears the environment: WSL_INTEROP never reaches Codex.
    assert!(resp.answer.contains("env-interop=unset"), "{}", resp.answer);
    assert_eq!(resp.exit_code, Some(0));
    assert!(resp.item_types.contains(&("command_execution".into(), 1)));
    assert_eq!(resp.commands, vec!["id -> declined"]);
    assert_eq!(
        std::fs::read_to_string(r.workdir.join("j1/input.txt")).unwrap(),
        input
    );
}

#[tokio::test]
async fn an_unknown_caller_is_refused_before_anything_runs() {
    let r = rig(my_uid() + 1).await;
    let resp = ask(&r.socket, &job("j2", "x", None)).await;
    assert_eq!(resp.status, "rejected");
    assert!(resp.error.unwrap().contains("not allowed"));
    assert!(
        !r.workdir.join("j2").exists(),
        "no job directory for a refused caller"
    );
}

#[tokio::test]
async fn a_job_id_is_used_once() {
    let r = rig(my_uid()).await;
    assert_eq!(
        ask(&r.socket, &job("j3", "a", None)).await.status,
        "completed"
    );
    let again = ask(&r.socket, &job("j3", "a", None)).await;
    assert_eq!(again.status, "rejected");
    assert!(again.error.unwrap().contains("already used"));
}

#[tokio::test]
async fn bad_requests_are_rejected() {
    let r = rig(my_uid()).await;
    for bad in ["../escape", "a/b", ""] {
        let resp = ask(&r.socket, &job(bad, "x", None)).await;
        assert_eq!(resp.status, "rejected", "{bad:?}");
    }
    let resp = ask(&r.socket, "not json").await;
    assert_eq!(resp.status, "rejected");
    assert!(resp.error.unwrap().contains("bad request"));
}

#[tokio::test]
async fn failures_are_reported_never_as_completed() {
    let r = rig(my_uid()).await;
    let fail = ask(&r.socket, &job("j4", "MODE:fail", None)).await;
    assert_eq!((fail.status.as_str(), fail.exit_code), ("failed", Some(3)));
    let none = ask(&r.socket, &job("j5", "MODE:noanswer", None)).await;
    assert_eq!(none.status, "failed");
    assert!(none.error.unwrap().contains("no answer"));
}

#[tokio::test]
async fn a_hanging_codex_times_out() {
    let r = rig(my_uid()).await;
    let started = std::time::Instant::now();
    let resp = ask(&r.socket, &job("j6", "MODE:sleep", Some(1))).await;
    assert_eq!(resp.status, "timed_out", "{resp:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    // Codex and everything it started are gone, not just the top process.
    let left = || {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
            .map(|c| String::from_utf8_lossy(&c).replace('\0', " "))
            .filter(|c| c.contains("sleep 4747") || c.contains("sleep 4748"))
            .count()
    };
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while left() > 0 && std::time::Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(left(), 0, "processes left behind after the timeout");
    // The runner is free again for the next job.
    assert_eq!(
        ask(&r.socket, &job("j7", "ok", None)).await.status,
        "completed"
    );
}
