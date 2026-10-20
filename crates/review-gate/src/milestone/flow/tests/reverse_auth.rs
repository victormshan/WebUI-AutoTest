//! E3 (M2 step 8 plan r2, Codex n5 condition 4): reverse authentication of the runner through
//! the whole request flow, with the runner stand-in in a separate process (its own pid and
//! credentials). The request passes every earlier check (signatures, sources, independence:
//! the positive control below completes), so each failure can only come from authenticating the
//! reply. Cases: (a) uid not the runner's; (b) uid right, cgroup not the runner's; (c) uid and
//! cgroup right, but the sender has exited and been reaped before its liveness is checked.

use std::io::BufRead;
use std::process::{Child, Command, Stdio};

use super::*;
use crate::milestone::runner::{BEFORE_LIVENESS, Expected, cgroup_of};

/// A runner stand-in: one connection, a complete, well-formed reply to the request it reads.
/// Prints "ready" once listening. Mode "linger": after the reply it waits until the gate closes
/// the connection (which happens only after authentication), so it is alive whenever it is
/// checked. Mode "exit": it exits right after the reply (case c, with the reaping barrier).
const STAND_IN: &str = r#"
import hashlib, json, socket, sys
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(1)
print("ready", flush=True)
c, _ = s.accept()
buf = b""
while not buf.endswith(b"\n"):
    part = c.recv(1 << 16)
    if not part: break
    buf += part
req = json.loads(buf)
resp = {"job_id": req["job_id"], "input_sha256": hashlib.sha256(req["input"].encode()).hexdigest(),
        "status": "completed", "exit_code": 0, "error": None,
        "answer": "VERDICT: APPROVE\nRESOLVED:\n- p/R1: done\n", "item_types": [], "commands": [],
        "evidence_complete": True, "cleanup": "gone", "runner_version": "stand-in",
        "started_at": "2026-10-20T00:00:00Z", "finished_at": "2026-10-20T00:00:01Z"}
c.sendall((json.dumps(resp) + "\n").encode())
if sys.argv[2] == "linger":
    c.recv(1)
c.close()
"#;

struct StandIn {
    child: Child,
    pid: i32,
    cgroup: String,
}

fn stand_in(sock: &Path, mode: &str) -> StandIn {
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(STAND_IN)
        .arg(sock)
        .arg(mode)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "ready");
    let pid = child.id() as i32;
    let cgroup = cgroup_of(pid).unwrap();
    StandIn { child, pid, cgroup }
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// One request through a service whose runner is the stand-in on `sock`, expecting `expect`.
fn run(fx: &Fx, sock: &Path, expect: Expected) -> (Arc<Flow>, String) {
    let mut cfg = fx.cfg();
    cfg.runner_socket = sock.to_path_buf();
    cfg.runner = expect;
    let f = Flow::open_service(cfg, kp(), fx.reader()).unwrap();
    fx.register(&f);
    let id = f.request("p", &fx.head).unwrap();
    (f, id)
}

/// The job's recorded error for `id`, once it is on file.
fn job_error(f: &Flow, id: &str) -> Option<String> {
    for _ in 0..400 {
        let snap = f.ledger().load().unwrap();
        if let Some(j) = snap
            .records
            .iter()
            .find(|r| r.kind == "job" && r.body["request_id"] == id)
        {
            return j.body["error"].as_str().map(str::to_string);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("no job record for {id}: {:?}", kinds(f));
}

/// The outcome every refused case must have: the job failed for `want`, the request ended Failed,
/// nothing resolved, and the phase is not released (published and archived, so archiving is not
/// what holds it back).
fn refused(fx: &Fx, f: &Flow, id: &str, want: &str, who: &StandIn) {
    let err = job_error(f, id).expect("the job recorded an error");
    eprintln!(
        "E3 sender pid {} uid {} cgroup {}: {err}",
        who.pid,
        uid(),
        who.cgroup
    );
    assert!(err.contains(want), "expected {want:?}: {err}");
    let st = settled(f, "p");
    assert_eq!(st.current.unwrap().state, RequestState::Failed);
    assert!(
        st.conditions.iter().all(|c| c.closed.is_none()),
        "nothing resolved"
    );
    fx.publish(f);
    let (seq, hash) = {
        let r = f.ledger().load().unwrap().records.last().cloned().unwrap();
        (r.seq, r.hash)
    };
    f.published_then_archive(seq, &hash).unwrap();
    let s = f.status_archived("p").unwrap();
    assert!(!s.released, "{:?}", s.reasons);
}

#[test]
fn a_reply_from_another_uid_is_not_authenticated() {
    let fx = Fx::new();
    let sock = fx.root.join("e3a.sock");
    let mut who = stand_in(&sock, "linger");
    let (f, id) = run(
        &fx,
        &sock,
        Expected {
            uid: uid() + 1,
            cgroup: who.cgroup.clone(),
        },
    );
    refused(&fx, &f, &id, "not the runner's", &who);
    who.child.wait().unwrap();
}

#[test]
fn a_reply_from_the_right_uid_in_another_cgroup_is_not_authenticated() {
    let fx = Fx::new();
    let sock = fx.root.join("e3b.sock");
    let mut who = stand_in(&sock, "linger");
    assert_ne!(who.cgroup, "/system.slice/audit-runner.service");
    let (f, id) = run(
        &fx,
        &sock,
        Expected {
            uid: uid(),
            cgroup: "/system.slice/audit-runner.service".into(),
        },
    );
    refused(
        &fx,
        &f,
        &id,
        "not the runner's /system.slice/audit-runner.service",
        &who,
    );
    who.child.wait().unwrap();
}

#[test]
fn a_reply_whose_sender_is_gone_before_the_check_is_not_authenticated() {
    let fx = Fx::new();
    let sock = fx.root.join("e3c.sock");
    let who = stand_in(&sock, "exit");
    // The barrier: the gate's liveness check waits until the test has reaped the sender.
    let reaped = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let r2 = reaped.clone();
    BEFORE_LIVENESS.lock().unwrap().push((
        who.pid,
        Arc::new(move || {
            let (m, cv) = &*r2;
            let g = m.lock().unwrap();
            let (g, t) = cv
                .wait_timeout_while(g, Duration::from_secs(10), |done| !*done)
                .unwrap();
            assert!(*g && !t.timed_out(), "the sender was reaped first");
        }),
    ));
    let pid = who.pid;
    let StandIn {
        mut child, cgroup, ..
    } = who;
    // Both uid and cgroup are the sender's own: only its liveness can fail.
    let (f, id) = run(
        &fx,
        &sock,
        Expected {
            uid: uid(),
            cgroup: cgroup.clone(),
        },
    );
    child.wait().unwrap();
    {
        let (m, cv) = &*reaped;
        *m.lock().unwrap() = true;
        cv.notify_all();
    }
    let who = StandIn { child, pid, cgroup };
    refused(&fx, &f, &id, "is no longer running", &who);
    BEFORE_LIVENESS.lock().unwrap().retain(|(p, _)| *p != pid);
}

/// The positive control: the same stand-in with the right uid and cgroup, alive when checked,
/// is authenticated and its audit takes effect. So the refusals above come from authentication,
/// not from any check before it.
#[test]
fn the_same_stand_in_with_the_right_identity_alive_is_authenticated() {
    let fx = Fx::new();
    let sock = fx.root.join("e3d.sock");
    let mut who = stand_in(&sock, "linger");
    // "linger": the sender is alive when its liveness is checked.
    let (f, id) = run(
        &fx,
        &sock,
        Expected {
            uid: uid(),
            cgroup: who.cgroup.clone(),
        },
    );
    let st = settled(&f, "p");
    assert_eq!(
        st.current.as_ref().unwrap().state,
        RequestState::Completed,
        "{:?} {:?}",
        job_error(&f, &id),
        kinds(&f)
    );
    let snap = f.ledger().load().unwrap();
    let job = snap
        .records
        .iter()
        .find(|r| r.kind == "job" && r.body["request_id"] == id.as_str())
        .unwrap();
    assert_eq!(job.body["sender"]["pid"], who.pid);
    assert_eq!(job.body["sender"]["uid"], uid());
    eprintln!("E3 control sender {}", job.body["sender"]);
    who.child.wait().unwrap();
}
