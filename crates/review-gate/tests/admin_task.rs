//! The real binary (Codex review of the identity integration): `admin task assign --yes` only
//! writes while it holds the state lock, and `serve` refuses milestone writers that are not
//! enabled principals.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use review_gate::gate::{Gate, NewTask};
use review_gate::identity::token_hash;
use review_gate::model::Channel;
use review_gate::store::Store;

const TOKEN: &str = "opencode-token-0123456789abcdef0123456789abcdef";
const LEGACY: &str = "legacy-token-0123456789abcdef0123456789abcdef";

fn private(p: &Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// An identity configuration with one enabled principal, `opencode`.
fn identity(dir: &Path) -> std::path::PathBuf {
    let etc = dir.join("etc");
    std::fs::create_dir(&etc).unwrap();
    private(&etc, 0o750);
    let p = etc.join("identity.json");
    std::fs::write(
        &p,
        format!(
            r#"{{"version": 1, "principals": [{{"id": "opencode", "family": "deepseek", "credentials": [{{"id": "o1", "sha256": "{}"}}]}}]}}"#,
            token_hash(TOKEN)
        ),
    )
    .unwrap();
    private(&p, 0o640);
    p
}

fn repo(dir: &Path) -> std::path::PathBuf {
    let r = dir.join("repo");
    std::fs::create_dir(&r).unwrap();
    let git = |a: &[&str]| {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&r)
                .args(a)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "base",
    ]);
    r
}

#[test]
fn assign_writes_only_while_it_holds_the_state_lock() {
    let d = tempfile::tempdir().unwrap();
    let state = d.path().join("state");
    let ids = identity(d.path());
    let gate = Gate::new(Store::open(&state).unwrap());
    let mut task = gate
        .init(NewTask {
            id: "t".into(),
            principal: "x".into(),
            implementer_family: "claude".into(),
            credential: "x-1".into(),
            config_digest: None,
            claimed_family: None,
            goal: "g".into(),
            final_acceptance: "a".into(),
            repo: repo(d.path()).to_string_lossy().into(),
            iterations: 1,
            min_reviewer: Channel::WebGemini,
            review_provider: "auto".into(),
        })
        .unwrap();
    task.principal.clear();
    gate.store().save_task(&task).unwrap();
    let listed = gate.store().load_task("t").unwrap();
    let at = listed.updated_at.to_rfc3339();
    let assign = |yes: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_review-gate"));
        c.args(["admin", "task", "--state"]).arg(&state).args([
            "assign",
            "--id",
            "t",
            "--principal",
            "opencode",
            "--reason",
            "user decided",
            "--expect-updated-at",
            &at,
        ]);
        c.arg("--identity-config").arg(&ids);
        if yes {
            c.arg("--yes");
        }
        c.output().unwrap()
    };
    // Someone holds the state (a running service, or another assignment in progress).
    let held = Store::open(&state)
        .unwrap()
        .try_service_lock()
        .unwrap()
        .unwrap();
    let out = assign(true);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("a gate service holds this state"), "{err}");
    assert_eq!(
        gate.store().load_task("t").unwrap(),
        listed,
        "nothing written"
    );
    // A dry run needs no lock and writes nothing.
    let out = assign(false);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(gate.store().load_task("t").unwrap(), listed);
    // Released: the write goes through, owner and audit together.
    drop(held);
    let mut out = assign(true);
    for _ in 0..50 {
        // A forking test thread can hold the lock's file description for an instant.
        if out.status.success() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
        out = assign(true);
    }
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = gate.store().load_task("t").unwrap();
    assert_eq!(after.owner_principal(), Some("opencode"));
    assert_eq!(after.owner_assigned.unwrap().reason, "user decided");
}

/// `serve` with a milestone writer that is not an enabled principal stops before listening;
/// with a valid writer it starts.
#[test]
fn serve_refuses_a_milestone_writer_that_is_not_an_enabled_principal() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let ids = identity(root);
    let token = root.join("client.token");
    std::fs::write(&token, LEGACY).unwrap();
    private(&token, 0o600);
    let bt = root.join("bridge.token");
    std::fs::write(&bt, "bridge-observer-token-0123456789abcdef").unwrap();
    private(&bt, 0o600);
    let repo = repo(root);
    let config = |writer: &str| {
        let p = root.join(format!("m-{writer}.json"));
        let c = serde_json::json!({
            "repo_id": "repo", "repo": repo, "remote_url": "file:///nowhere",
            "runner_socket": root.join("runner.sock"), "runner_uid": 0, "runner_cgroup": "/x",
            "job_timeout_secs": 60, "auditor": "openai", "implementers": ["claude"],
            "bridge_url": "http://127.0.0.1:9", "bridge_token_file": bt,
            "writers": [writer],
        });
        std::fs::write(&p, c.to_string()).unwrap();
        p
    };
    let serve = |m: &Path, state: &str| {
        Command::new(env!("CARGO_BIN_EXE_review-gate"))
            .args(["serve", "--listen", "127.0.0.1:0", "--state"])
            .arg(root.join(state))
            .arg("--token-file")
            .arg(&token)
            .arg("--identity-config")
            .arg(&ids)
            .arg("--milestones")
            .arg(m)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut bad = serve(&config("ghost"), "s1");
    let t0 = Instant::now();
    let status = loop {
        if let Some(s) = bad.try_wait().unwrap() {
            break s;
        }
        if t0.elapsed() > Duration::from_secs(10) {
            bad.kill().unwrap();
            bad.wait().unwrap();
            panic!("serve started with a writer that is not an enabled principal");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    let mut err = String::new();
    std::io::Read::read_to_string(bad.stderr.as_mut().unwrap(), &mut err).unwrap();
    assert!(
        err.contains("writer \"ghost\" is not an enabled principal"),
        "{err}"
    );
    // A valid writer: still running after a while (it listens), then stopped.
    let mut good = serve(&config("opencode"), "s2");
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(2) {
        if let Some(s) = good.try_wait().unwrap() {
            let mut e = String::new();
            std::io::Read::read_to_string(good.stderr.as_mut().unwrap(), &mut e).unwrap();
            panic!("serve exited with {s}: {e}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    good.kill().unwrap();
    good.wait().unwrap();
}
