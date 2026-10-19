//! `admin milestone` on the real binary (M2 step 6c, Codex n4 §2): without `--yes` nothing in
//! the state changes; with it, the record is written.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn digest(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.insert(p.clone(), b"dir".to_vec());
                stack.push(p);
            } else {
                out.insert(p.clone(), std::fs::read(&p).unwrap());
            }
        }
    }
    out
}

#[test]
fn a_write_without_yes_changes_nothing_and_with_yes_writes() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let state = root.join("state");
    std::fs::create_dir_all(state.join("keys")).unwrap();
    std::fs::write(state.join("keys/gate.ed25519"), [7u8; 32]).unwrap();
    let bt = root.join("bridge.token");
    std::fs::write(&bt, "bridge-observer-token-0123456789abcdef").unwrap();
    std::fs::set_permissions(&bt, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::create_dir(root.join("repo")).unwrap();
    let config = serde_json::json!({
        "repo_id": "repo", "repo": root.join("repo"), "remote_url": "file:///nowhere",
        "runner_socket": root.join("runner.sock"), "runner_uid": 0, "runner_cgroup": "/x",
        "job_timeout_secs": 60, "auditor": "openai", "implementers": ["claude"],
        "bridge_url": "http://127.0.0.1:9", "bridge_token_file": bt,
    });
    std::fs::write(root.join("m.json"), config.to_string()).unwrap();
    let mut table = review_gate::milestone::sources::Vendors::first();
    table.version += 1;
    std::fs::write(root.join("v.json"), serde_json::to_string(&table).unwrap()).unwrap();
    let run = |yes: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_review-gate"));
        c.args(["admin", "milestone", "--state"])
            .arg(&state)
            .arg("--config")
            .arg(root.join("m.json"));
        if yes {
            c.arg("--yes");
        }
        c.args(["vendors-set", "--reason", "user decision"])
            .arg("--file")
            .arg(root.join("v.json"));
        let out = c.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let before = digest(&state);
    let v = run(false);
    assert_eq!(v["dry_run"], true, "{v}");
    assert_eq!(v["would"]["command"], "vendors-set");
    assert_eq!(digest(&state), before, "nothing written without --yes");
    let v = run(true);
    assert_eq!(v["written"][0]["kind"], "vendors_changed", "{v}");
    assert!(state.join("milestones/log.jsonl").is_file());
}
