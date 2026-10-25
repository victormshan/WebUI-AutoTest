//! audit-runner over a real unix socket with a fake Codex.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use audit_runner::{Config, Request, Response, serve, serve_with, sha256_hex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Fake `codex exec`: the input text chooses what it does, since the runner clears the
/// environment. Its event stream has the shape of Codex CLI 0.162.1's, with one declined
/// command execution; its answer reports its environment and the files in its Codex home.
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
  *MODE:hangcmd*)
    echo '{"type":"thread.started","thread_id":"t1"}'
    echo '{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"touch x","status":"in_progress"}}'
    sleep 4749 ;;
  *MODE:noevents*) echo "VERDICT: APPROVE" > "$out"; exit 0 ;;
  *MODE:badevents*) echo '{"type":"thread.started"}'; echo 'garbage'; echo '{"type":"turn.completed"}'; echo "VERDICT: APPROVE" > "$out"; exit 0 ;;
  *MODE:noend*) echo '{"type":"thread.started","thread_id":"t1"}'; echo '{"type":"turn.started"}'; echo "VERDICT: APPROVE" > "$out"; exit 0 ;;
  *MODE:refresh*) echo '{"refreshed":true}' > "$CODEX_HOME/auth.json" ;;
esac
echo '{"type":"thread.started","thread_id":"t1"}'
echo '{"type":"turn.started"}'
echo '{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"id","status":"declined"}}'
echo '{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"done"}}'
echo '{"type":"turn.completed","usage":{}}'
printf 'VERDICT: APPROVE\nenv-interop=%s\nhome=%s\ncodex-home=%s\nfiles=%s\nargs=%s\n' "${WSL_INTEROP:-unset}" "$HOME" "$CODEX_HOME" "$(ls -A "$CODEX_HOME" | tr '\n' ' ')" "$*" > "$out"
"#;

fn my_uid() -> u32 {
    std::fs::metadata("/proc/self").unwrap().uid()
}

struct Rig {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    workdir: PathBuf,
    codex_home: PathBuf,
    run_home: PathBuf,
}

async fn rig(allow_uid: u32) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let codex = dir.path().join("codex");
    std::fs::write(&codex, FAKE_CODEX).unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    let socket = dir.path().join("runner.sock");
    let workdir = dir.path().join("jobs");
    std::fs::create_dir_all(&workdir).unwrap();
    // The auditor's own Codex home: a login, plus context that must not reach the model.
    let codex_home = dir.path().join("home/.codex");
    std::fs::create_dir_all(codex_home.join("skills/planted")).unwrap();
    std::fs::write(codex_home.join("auth.json"), r#"{"login":1}"#).unwrap();
    std::fs::write(codex_home.join("AGENTS.md"), "PLANTED").unwrap();
    std::fs::write(codex_home.join("skills/planted/SKILL.md"), "PLANTED").unwrap();
    let run_home = dir.path().join("home/codex-run");
    let cfg = Config {
        allow_uid,
        allow_cgroup: None,
        codex,
        codex_home: codex_home.clone(),
        run_home: run_home.clone(),
        workdir: workdir.clone(),
        default_timeout: Duration::from_secs(20),
        reply_timeout: Duration::from_secs(5),
        own_cgroup: false,
        reasoning_effort: "high".into(),
    };
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(cfg, listener));
    Rig {
        _dir: dir,
        socket,
        workdir,
        codex_home,
        run_home,
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
    // Codex gets every switch that removes its tools.
    let args = resp
        .answer
        .lines()
        .find_map(|l| l.strip_prefix("args="))
        .unwrap();
    assert!(args.contains(&audit_runner::NO_TOOLS.join(" ")), "{args}");
    assert!(resp.evidence_complete);
    assert_eq!(resp.cleanup, "gone");
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
    // The reply comes only after the whole group is confirmed gone (ESRCH, which also covers
    // zombies), and no process of it is left.
    assert_eq!(resp.cleanup, "gone", "{resp:?}");
    assert_eq!(left(), 0, "processes left behind after the timeout");
    // The runner is free again for the next job.
    assert_eq!(
        ask(&r.socket, &job("j7", "ok", None)).await.status,
        "completed"
    );
}

#[tokio::test]
async fn a_timeout_keeps_the_record_of_what_codex_started() {
    let r = rig(my_uid()).await;
    let resp = ask(&r.socket, &job("j8", "MODE:hangcmd", Some(1))).await;
    assert_eq!(resp.status, "timed_out", "{resp:?}");
    assert!(
        resp.error.as_deref().unwrap().contains("incomplete"),
        "{resp:?}"
    );
    assert!(!resp.evidence_complete);
    assert_eq!(resp.commands, vec!["touch x -> never completed"]);
    let events = std::fs::read_to_string(r.workdir.join("j8/events.jsonl")).unwrap();
    assert!(events.contains("touch x"), "{events}");
}

#[tokio::test]
async fn codex_sees_a_clean_home_with_only_the_login() {
    let r = rig(my_uid()).await;
    let resp = ask(&r.socket, &job("h1", "x", None)).await;
    assert_eq!(resp.status, "completed", "{resp:?}");
    let field = |k: &str| {
        resp.answer
            .lines()
            .find_map(|l| l.strip_prefix(k))
            .unwrap()
            .trim()
            .to_string()
    };
    assert_eq!(field("files="), "auth.json", "{}", resp.answer);
    assert_eq!(Path::new(&field("home=")), r.run_home);
    assert_eq!(Path::new(&field("codex-home=")), r.run_home.join(".codex"));
    // The run home is gone afterwards, and the login never entered the job record.
    assert!(!r.run_home.exists());
    assert!(!r.workdir.join("h1").join("auth.json").exists());
    assert!(
        r.codex_home.join("AGENTS.md").exists(),
        "the auditor's own home is untouched"
    );
}

#[tokio::test]
async fn a_login_refreshed_during_the_job_is_kept() {
    let r = rig(my_uid()).await;
    let resp = ask(&r.socket, &job("h2", "MODE:refresh", None)).await;
    assert_eq!(resp.status, "completed", "{resp:?}");
    assert_eq!(
        std::fs::read_to_string(r.codex_home.join("auth.json")).unwrap(),
        "{\"refreshed\":true}\n"
    );
}

#[tokio::test]
async fn an_incomplete_or_invalid_event_record_is_never_completed() {
    let r = rig(my_uid()).await;
    for (id, mode, why) in [
        ("e1", "MODE:noevents", "no events"),
        ("e2", "MODE:badevents", "not JSON"),
        ("e3", "MODE:noend", "not turn.completed"),
    ] {
        let resp = ask(&r.socket, &job(id, mode, None)).await;
        assert_eq!(resp.status, "failed", "{mode}: {resp:?}");
        assert!(!resp.evidence_complete, "{mode}");
        let error = resp.error.unwrap();
        assert!(error.contains(why), "{mode}: {error}");
    }
}

/// Under socket activation (fd 3 from systemd) Codex inherits no socket: it could otherwise
/// accept the gate's next connection itself.
#[tokio::test]
async fn codex_inherits_no_socket_under_socket_activation() {
    // Not skipped when the tool is missing: a skipped check must not count as a pass.
    let activate = which("systemd-socket-activate").expect("systemd-socket-activate is needed");
    // Unix socket paths are short; keep this one under /tmp.
    let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let codex = dir.path().join("codex");
    std::fs::write(
        &codex,
        "#!/bin/sh\nout=\"\"; prev=\"\"\nfor a in \"$@\"; do [ \"$prev\" = \"-o\" ] && out=\"$a\"; prev=\"$a\"; done\ncat >/dev/null\n(for f in /proc/$$/fd/*; do echo \"${f##*/} $(readlink $f)\"; done) > \"$out\"\nprintf '%s\\n' '{\"type\":\"thread.started\"}' '{\"type\":\"turn.completed\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
    std::fs::write(dir.path().join(".codex/auth.json"), "{}").unwrap();
    let socket = dir.path().join("r.sock");
    let mut runner = tokio::process::Command::new(activate)
        .arg("-l")
        .arg(&socket)
        .arg(env!("CARGO_BIN_EXE_audit-runner"))
        .args(["--allow-uid", &my_uid().to_string(), "--codex"])
        .arg(&codex)
        .arg("--codex-home")
        .arg(dir.path().join(".codex"))
        .arg("--run-home")
        .arg(dir.path().join("run"))
        .arg("--workdir")
        .arg(dir.path().join("jobs"))
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while !socket.exists() && std::time::Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let resp = ask(&socket, &job("fd1", "x", None)).await;
    runner.kill().await.unwrap();
    assert_eq!(resp.status, "completed", "{resp:?}");
    // Every descriptor Codex holds is accounted for: its stdin pipe, its event and stderr
    // files, and the script the fake Codex's shell reads. (The listing runs in a subshell, so
    // its own redirections do not show up.)
    let job_dir = dir.path().join("jobs/fd1");
    let mut seen = 0;
    for line in resp.answer.lines() {
        let (fd, target) = line.split_once(' ').unwrap();
        seen += 1;
        let ok = match fd {
            "0" => target.starts_with("pipe:"),
            "1" => Path::new(target) == job_dir.join("events.jsonl"),
            "2" => Path::new(target) == job_dir.join("stderr.log"),
            _ => Path::new(target) == codex,
        };
        assert!(
            ok,
            "unexpected descriptor {fd} -> {target}\n{}",
            resp.answer
        );
    }
    assert!(seen >= 3, "{}", resp.answer);
}

fn which(name: &str) -> Result<PathBuf, ()> {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.exists())
        .ok_or(())
}

/// A runner with an injected job outcome: `cleanup` is what every job reports. Returns the
/// socket, the serve task and how many jobs ran.
fn injected(
    dir: &Path,
    cleanup: &'static str,
    answer_bytes: usize,
    job_delay: Duration,
    reply_timeout: Duration,
) -> (
    PathBuf,
    tokio::task::JoinHandle<std::io::Result<()>>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let socket = dir.join("inj.sock");
    let cfg = Config {
        allow_uid: my_uid(),
        allow_cgroup: None,
        codex: "/nonexistent".into(),
        codex_home: dir.join("home/.codex"),
        run_home: dir.join("run"),
        workdir: dir.join("jobs"),
        default_timeout: Duration::from_secs(5),
        reply_timeout,
        own_cgroup: false,
        reasoning_effort: "high".into(),
    };
    let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = runs.clone();
    let listener = UnixListener::bind(&socket).unwrap();
    let task = tokio::spawn(serve_with(cfg, listener, move |_cfg, req: Request| {
        let counted = counted.clone();
        async move {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(job_delay).await;
            Response {
                job_id: req.job_id,
                input_sha256: String::new(),
                status: "completed".into(),
                exit_code: Some(0),
                error: None,
                answer: "x".repeat(answer_bytes),
                item_types: vec![],
                commands: vec![],
                evidence_complete: false,
                cleanup: cleanup.into(),
                runner_version: "test".into(),
                started_at: chrono::Utc::now(),
                finished_at: chrono::Utc::now(),
            }
        }
    }));
    (socket, task, runs)
}

/// After an unconfirmed cleanup the runner takes no further job: the serve loop has ended
/// with an error and the listener is closed.
async fn assert_stopped(
    socket: &Path,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    runs: &std::sync::atomic::AtomicUsize,
) {
    let ended = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the runner keeps serving after an unconfirmed cleanup");
    assert!(ended.unwrap().is_err());
    // A further job is not taken: the connection is refused, or (if the closed listener is
    // still being torn down) nothing answers it.
    if let Ok(s) = UnixStream::connect(socket).await {
        let (r, mut w) = s.into_split();
        let _ = w
            .write_all(format!("{}\n", job("next", "x", None)).as_bytes())
            .await;
        let mut reply = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(1),
            BufReader::new(r).read_line(&mut reply),
        )
        .await;
        assert!(
            reply.is_empty(),
            "a further job was answered: {reply} ({read:?})"
        );
    }
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_unconfirmed_cleanup_stops_the_runner_after_its_reply() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, task, runs) = injected(
        dir.path(),
        "not confirmed: injected",
        10,
        Duration::ZERO,
        Duration::from_secs(5),
    );
    let resp = ask(&socket, &job("u1", "x", None)).await;
    assert_eq!(resp.cleanup, "not confirmed: injected");
    assert_stopped(&socket, task, &runs).await;
}

#[tokio::test]
async fn an_unconfirmed_cleanup_stops_the_runner_when_the_caller_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, task, runs) = injected(
        dir.path(),
        "not confirmed: injected",
        10,
        Duration::from_millis(300),
        Duration::from_secs(5),
    );
    let mut s = UnixStream::connect(&socket).await.unwrap();
    s.write_all(format!("{}\n", job("u2", "x", None)).as_bytes())
        .await
        .unwrap();
    drop(s); // the caller hangs up before the reply
    assert_stopped(&socket, task, &runs).await;
}

#[tokio::test]
async fn an_unconfirmed_cleanup_stops_the_runner_when_the_reply_blocks() {
    let dir = tempfile::tempdir().unwrap();
    // A reply far larger than the socket buffer, to a caller that never reads.
    let (socket, task, runs) = injected(
        dir.path(),
        "not confirmed: injected",
        32 * 1024 * 1024,
        Duration::ZERO,
        Duration::from_millis(300),
    );
    let mut s = UnixStream::connect(&socket).await.unwrap();
    s.write_all(format!("{}\n", job("u3", "x", None)).as_bytes())
        .await
        .unwrap();
    assert_stopped(&socket, task, &runs).await;
    drop(s);
}

#[tokio::test]
async fn a_confirmed_cleanup_keeps_the_runner_serving() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, task, runs) = injected(
        dir.path(),
        "gone",
        10,
        Duration::ZERO,
        Duration::from_secs(5),
    );
    assert_eq!(ask(&socket, &job("g1", "x", None)).await.cleanup, "gone");
    assert_eq!(ask(&socket, &job("g2", "x", None)).await.cleanup, "gone");
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(!task.is_finished());
    task.abort();
}

#[tokio::test]
async fn the_run_home_is_removed_when_the_job_cannot_start_or_the_login_cannot_be_saved() {
    // No login to copy: preparing the run home fails part-way.
    let r = rig(my_uid()).await;
    std::fs::remove_file(r.codex_home.join("auth.json")).unwrap();
    let resp = ask(&r.socket, &job("f1", "x", None)).await;
    assert_eq!(resp.status, "failed", "{resp:?}");
    assert!(resp.error.unwrap().contains("preparing codex home"));
    assert!(!r.run_home.exists());

    // A refreshed login that cannot be written back (the auditor's home is read-only).
    let r = rig(my_uid()).await;
    let ro = std::fs::Permissions::from_mode(0o500);
    std::fs::set_permissions(&r.codex_home, ro).unwrap();
    let resp = ask(&r.socket, &job("f2", "MODE:refresh", None)).await;
    std::fs::set_permissions(&r.codex_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(resp.status, "failed", "{resp:?}");
    assert!(resp.error.unwrap().contains("restoring the login"));
    assert!(!r.run_home.exists());
}

/// Fake Codex that leaves a process behind outside its own process group (setsid) and
/// reports that process's pid in its answer.
const ESCAPING_CODEX: &str = r#"#!/bin/sh
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
cat >/dev/null
setsid sleep 4791 </dev/null >/dev/null 2>&1 &
left=$!
# Wait until it has its own session (left the group), else the group kill still reaches it.
i=0
while [ "$(awk '{print $6}' /proc/$left/stat 2>/dev/null)" != "$left" ] && [ $i -lt 100 ]; do
  sleep 0.02; i=$((i+1))
done
printf '%s\n' '{"type":"thread.started"}' '{"type":"turn.completed"}'
printf 'VERDICT: APPROVE\nleft-pid=%s\n' "$left" > "$out"
"#;

/// Runs the real runner binary as a transient systemd user service (its own cgroup), with
/// the escaping fake Codex. Returns the reply to one job, and whether the escaped process was
/// still running once the reply came (checked before the unit is stopped: stopping it makes
/// systemd end the whole cgroup).
async fn run_as_service(own_cgroup: bool) -> (Response, bool) {
    let (resp, alive, _) = run_as_service_with(own_cgroup, None).await;
    (resp, alive)
}

/// `run_as_service` with an optional `AUDIT_RUNNER_FAULT` for the service; also returns whether
/// the runner was still running 5 s after the reply (an unconfirmed cleanup must stop it).
async fn run_as_service_with(own_cgroup: bool, fault: Option<&str>) -> (Response, bool, bool) {
    let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let codex = dir.path().join("codex");
    std::fs::write(&codex, ESCAPING_CODEX).unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
    std::fs::write(dir.path().join(".codex/auth.json"), "{}").unwrap();
    let socket = dir.path().join("s.sock");
    let unit = format!(
        "audit-runner-test-{}-{}-{}",
        std::process::id(),
        own_cgroup,
        fault.is_some()
    );
    let mut run = std::process::Command::new("systemd-run");
    run.args(["--user", "--collect", "--quiet", &format!("--unit={unit}")]);
    if let Some(f) = fault {
        run.arg(format!("--setenv=AUDIT_RUNNER_FAULT={f}"));
    }
    run.arg(env!("CARGO_BIN_EXE_audit-runner"))
        .arg("--socket")
        .arg(&socket)
        .args(["--allow-uid", &my_uid().to_string(), "--codex"])
        .arg(&codex)
        .arg("--codex-home")
        .arg(dir.path().join(".codex"))
        .arg("--run-home")
        .arg(dir.path().join("run"))
        .arg("--workdir")
        .arg(dir.path().join("jobs"));
    if own_cgroup {
        run.arg("--own-cgroup");
    }
    // Not skipped when the user manager is missing: a skipped check must not count as a pass.
    assert!(
        run.status().expect("systemd-run").success(),
        "systemd-run --user failed"
    );
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while !socket.exists() && std::time::Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let resp = ask(&socket, &job("s1", "x", None)).await;
    let pid: u32 = resp
        .answer
        .lines()
        .find_map(|l| l.strip_prefix("left-pid="))
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no left-pid: {resp:?}"));
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let alive = String::from_utf8_lossy(&cmd).contains("4791");
    let active = || {
        std::process::Command::new("systemctl")
            .args(["--user", "is-active", &unit])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
            .unwrap_or(false)
    };
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while active() && std::time::Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let serving = active();
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "stop", &unit])
        .status();
    (resp, alive, serving)
}

/// A25 at the call site: the sweep cannot read the member list (an input fault; the sweep's
/// logic runs unchanged). The job's cleanup is not confirmed and the runner stops.
#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn under_systemd_a_sweep_that_cannot_read_its_cgroup_stops_the_runner() {
    let (resp, _, serving) = run_as_service_with(
        true,
        Some("sweep-procs=/nonexistent/audit-runner-test/cgroup.procs"),
    )
    .await;
    assert!(
        resp.cleanup.starts_with("not confirmed")
            && resp.cleanup.contains("reading the cgroup members"),
        "{resp:?}"
    );
    assert!(
        !serving,
        "the runner kept serving after an unconfirmed sweep"
    );
}

/// Without the feature the variable does nothing: the real sweep runs (the escaped process is
/// ended), the cleanup is confirmed and the runner keeps serving.
#[cfg(not(feature = "fault-injection"))]
#[tokio::test]
async fn under_systemd_the_sweep_fault_has_no_effect_in_a_default_build() {
    let (resp, alive, serving) = run_as_service_with(
        true,
        Some("sweep-procs=/nonexistent/audit-runner-test/cgroup.procs"),
    )
    .await;
    assert_eq!(resp.cleanup, "gone", "{resp:?}");
    assert!(!alive, "the real sweep ran and ended the escaped process");
    assert!(serving, "the runner keeps serving");
}

#[tokio::test]
async fn under_systemd_a_process_that_left_the_group_is_ended_too() {
    let (resp, alive) = run_as_service(true).await;
    assert_eq!(resp.status, "completed", "{resp:?}");
    assert_eq!(resp.cleanup, "gone", "{resp:?}");
    assert!(!alive, "the escaped process survived the job");
}

#[tokio::test]
async fn without_the_cgroup_sweep_an_escaped_process_survives_the_group_kill() {
    // The limit the sweep exists for: the process-group kill alone does not reach it.
    // (Stopping the unit afterwards ends it with the rest of the cgroup.)
    let (resp, alive) = run_as_service(false).await;
    assert_eq!(resp.cleanup, "gone", "{resp:?}");
    assert!(
        alive,
        "expected the escaped process to outlive the group kill: {resp:?}"
    );
}

/// `--own-cgroup` refuses to start outside a systemd service cgroup. The child inherits this
/// test's cgroup, so which side of the check it lands on depends on where the tests run: on a
/// developer shell (a session scope) it must refuse; under a service (a CI runner is one) it must
/// get past the check. Both outcomes are asserted; the test never skips.
#[tokio::test]
async fn own_cgroup_refuses_to_start_outside_a_service() {
    let here = audit_runner::cgroup_of(std::process::id() as i32).unwrap();
    let in_service = here.ends_with(".service");
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("x.sock");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_audit-runner"))
        .arg("--socket")
        .arg(&sock)
        .args(["--allow-uid", "1", "--codex", "/x", "--codex-home", "/x"])
        .args(["--run-home", "/x", "--workdir"])
        .arg(dir.path().join("jobs"))
        .arg("--own-cgroup")
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    if in_service {
        // Past the cgroup check the runner binds its socket: wait for that (bounded), with the
        // child still running, then stop and reap it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !sock.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                let mut err = String::new();
                let _ = tokio::io::AsyncReadExt::read_to_string(
                    child.stderr.as_mut().unwrap(),
                    &mut err,
                )
                .await;
                panic!("the runner exited in {here} before listening ({status}): {err}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the runner did not listen within 5 s in {here}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "still running while listening"
        );
        child.kill().await.unwrap();
    } else {
        let mut err = String::new();
        let mut stderr = child.stderr.take().unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), async {
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut err).await;
            child.wait().await
        })
        .await
        .expect("the runner started outside a service cgroup");
        assert!(!status.unwrap().success());
        assert!(
            err.contains("not in a systemd service cgroup"),
            "{here}: {err}"
        );
    }
}
