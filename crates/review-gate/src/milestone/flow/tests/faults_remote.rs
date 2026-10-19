//! Remote faults at the flow service layer (M2 step 7, F7–F11): a truncated, forked, tampered,
//! legitimately longer or unreachable remote. Every case starts from a phase that really released,
//! injects the fault into the bare test remote only, and then checks status, requests and the
//! local state files.

use std::process::{Command, Stdio};

use super::*;
use crate::milestone::record::{parse_log, render_log, seal};

/// A phase that walked the real path to a release: register, a completed request, publish, then
/// `published_then_archive`. `remote` is the remote log at that point, `log`/`cp` the local bytes.
struct Started {
    fx: Fx,
    f: Arc<Flow>,
    remote: String,
    log: Vec<u8>,
    cp: Vec<u8>,
}

fn state_bytes(fx: &Fx) -> (Vec<u8>, Vec<u8>) {
    (
        fs::read(fx.root.join("state/log.jsonl")).unwrap(),
        fs::read(fx.root.join("state/checkpoint.json")).unwrap(),
    )
}

fn started() -> Started {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    f.request("p", &fx.head).unwrap();
    let st = settled(&f, "p");
    assert_eq!(
        st.current.as_ref().unwrap().state,
        RequestState::Completed,
        "the common start needs a completed request"
    );
    fx.publish(&f);
    let snap = f.ledger().load().unwrap();
    let last = snap.records.last().unwrap().clone();
    let out = f.published_then_archive(last.seq, &last.hash).unwrap();
    assert!(out.archived, "the published position is archived");
    let st = f.status_archived("p").unwrap();
    assert!(st.released, "the common start releases: {:?}", st.reasons);
    let remote = remote_text(&fx);
    let (log, cp) = state_bytes(&fx);
    Started {
        fx,
        f,
        remote,
        log,
        cp,
    }
}

fn assert_released(f: &Flow) {
    let st = f.status_archived("p").unwrap();
    assert!(st.released, "still stopped: {:?}", st.reasons);
}

/// Status stopped the release for `needle`, a request is refused, and neither state file moved.
fn assert_stopped(r: &Started, needle: &str) {
    // Check the files before unwrapping the status: a broken sync imports or archives first and
    // only then fails verification, and that must be reported as a write, not as a panic.
    let status = r.f.status_archived("p");
    let (log, cp) = state_bytes(&r.fx);
    assert_eq!(log, r.log, "log.jsonl was written while stopped");
    assert_eq!(cp, r.cp, "checkpoint.json was written while stopped");
    let st = status.expect("status failed while stopped");
    assert!(!st.released, "released: {:?}", st.reasons);
    assert!(
        st.reasons.iter().any(|x| x.contains("release stopped")),
        "no release-stopped reason: {:?}",
        st.reasons
    );
    assert!(
        st.reasons.iter().any(|x| x.contains(needle)),
        "no {needle:?} reason: {:?}",
        st.reasons
    );
    let e = r.f.request("p", &r.fx.head).unwrap_err().to_string();
    assert!(e.contains("release stopped"), "request not refused: {e}");
    let (log, cp) = state_bytes(&r.fx);
    assert_eq!(log, r.log, "log.jsonl was written while stopped");
    assert_eq!(cp, r.cp, "checkpoint.json was written while stopped");
}

/// One git command in `dir`, with stdin, an identity (a bare notes commit needs one) and no user
/// or system configuration.
fn git_in(dir: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input)
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap().trim().to_string()
}

fn remote_text(fx: &Fx) -> String {
    sync::read_log(&fx.remote, sync::NOTES_REF)
        .unwrap()
        .expect("the remote has a milestone log")
}

/// Rewrites the bare test remote's notes ref by force (the fault, not the implementer's path).
fn set_remote_text(fx: &Fx, text: &str) {
    let anchor = sync::anchor(&fx.remote).unwrap();
    let blob = git_in(
        &fx.remote,
        &["hash-object", "-w", "--stdin"],
        text.as_bytes(),
    );
    git_in(
        &fx.remote,
        &[
            "notes",
            "--ref",
            sync::NOTES_REF,
            "add",
            "-f",
            "-C",
            &blob,
            &anchor,
        ],
        b"",
    );
}

/// Another state's view of the same log grows it by `n` validly signed records: a legitimate
/// remote longer than the local one.
fn legitimate_extension(fx: &Fx, n: usize) -> String {
    let other = tempfile::tempdir().unwrap();
    fs::copy(
        fx.root.join("state/log.jsonl"),
        other.path().join("log.jsonl"),
    )
    .unwrap();
    let l = Ledger::new(other.path(), "repo", kp().verifying_key()).unwrap();
    for i in 0..n {
        l.append(
            &kp(),
            "t",
            draft("note", "p", json!({"remote_extension": i})),
        )
        .unwrap();
    }
    fs::read_to_string(other.path().join("log.jsonl")).unwrap()
}

/// The remote log with `n` further validly signed records on its tail, but with different content
/// from the local records that occupy those seqs: a fork, not a legitimate extension.
fn forked_extension(remote: &str, n: usize) -> String {
    let mut v = parse_log(remote).unwrap();
    for i in 0..n {
        v.push(
            seal(
                &kp(),
                v.last(),
                "repo",
                "t",
                draft("note", "p", json!({"fork": i})),
            )
            .unwrap(),
        );
    }
    render_log(&v).unwrap()
}

/// The same log with one record's signature bytes changed (its hash and body stay intact).
fn tamper_signature(text: &str, index: usize) -> (String, String) {
    let mut v = parse_log(text).unwrap();
    let hash = v[index].hash.clone();
    let sig = &mut v[index].sig;
    assert!(!sig.is_empty(), "record {} has no signature", index + 1);
    // Still base64 of the same length, but another signature: verification must fail.
    let first = sig.remove(0);
    sig.insert(0, if first == 'A' { 'B' } else { 'A' });
    (render_log(&v).unwrap(), hash)
}

#[test]
fn f7_a_truncated_remote_stops_release_and_a_restored_one_releases() {
    let r = started();
    let lines: Vec<&str> = r.remote.lines().collect();
    assert!(lines.len() > 2);
    // Fewer records than the checkpoint: the remote cannot prove what was archived.
    let cut = format!("{}\n{}\n", lines[0], lines[1]);
    set_remote_text(&r.fx, &cut);

    assert_stopped(&r, "truncated");

    // Restore the original notes, then release again through publish and archive.
    set_remote_text(&r.fx, &r.remote);
    r.fx.publish(&r.f);
    r.f.archive().unwrap();
    assert_released(&r.f);
}

#[test]
fn f8_a_legitimately_longer_remote_is_imported_without_stop() {
    let r = started();
    let ext = legitimate_extension(&r.fx, 2);
    let ext_lines: Vec<&str> = ext.lines().collect();
    let local_lines = String::from_utf8(r.log.clone()).unwrap().lines().count();
    assert_eq!(ext_lines.len(), local_lines + 2);
    set_remote_text(&r.fx, &ext);

    let st = r.f.status_archived("p").unwrap();
    assert!(
        st.released,
        "a legitimate extension stopped: {:?}",
        st.reasons
    );
    assert!(
        !st.reasons.iter().any(|x| x.contains("release stopped")),
        "a legitimate extension was reported as stopped: {:?}",
        st.reasons
    );
    assert_eq!(
        st.state.current.as_ref().unwrap().state,
        RequestState::Completed,
        "the state is derived from the log after the import"
    );
    assert_eq!(st.state.count, 0, "an APPROVE adds no rework to the count");

    // The two extra records were appended byte for byte, then the position archived.
    let log = fs::read_to_string(r.fx.root.join("state/log.jsonl")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines.len(),
        local_lines + 2 + 1,
        "local + 2 imported + 1 archived"
    );
    assert_eq!(lines[local_lines], ext_lines[local_lines]);
    assert_eq!(lines[local_lines + 1], ext_lines[local_lines + 1]);
    let snap = r.f.ledger().load().unwrap();
    assert_eq!(snap.records[local_lines].seq, local_lines as u64 + 1);
    assert_eq!(
        snap.records[local_lines].body,
        json!({"remote_extension": 0})
    );
    assert_eq!(
        snap.records[local_lines + 1].body,
        json!({"remote_extension": 1})
    );
    assert_eq!(snap.checkpoint, local_lines as u64 + 2);
}

#[test]
fn f9_a_forked_remote_record_stops_release() {
    let r = started();
    // The remote keeps the records the local log has, then continues with a different, validly
    // signed chain where the local log's last record is: divergence, not an extension.
    let fork = forked_extension(&r.remote, 4);
    set_remote_text(&r.fx, &fork);

    assert_stopped(&r, "diverge");

    set_remote_text(&r.fx, &r.remote);
    r.fx.publish(&r.f);
    r.f.archive().unwrap();
    assert_released(&r.f);
}

#[test]
fn f10_an_unreachable_remote_stops_release() {
    let r = started();
    // Every fetch of the same flow's configured remote fails: neither the archive attempt nor
    // the status snapshot may treat it as an empty remote.
    r.f.hooks
        .lock()
        .unwrap()
        .failed_fetches
        .store(100, std::sync::atomic::Ordering::SeqCst);

    assert_stopped(&r, "unreachable");
    // Requirement 3 also holds for the request above; check it stays so after a second status.
    let st = r.f.status_archived("p").unwrap();
    assert!(!st.released);
    let (log, cp) = state_bytes(&r.fx);
    assert_eq!((log, cp), (r.log.clone(), r.cp.clone()), "local writes");

    // Reachable again: the real archive path proves the position, and release resumes.
    r.f.hooks
        .lock()
        .unwrap()
        .failed_fetches
        .store(0, std::sync::atomic::Ordering::SeqCst);
    r.f.archive().unwrap();
    assert_released(&r.f);
}

#[test]
fn f11_a_tampered_signature_in_the_imported_tail_is_never_imported() {
    let r = started();
    // The remote legitimately holds two records beyond the local log; the first of them (the
    // first record after the local log's length) is in the range a sync would import, and its
    // signature is then altered. The index follows the local log, not a fixed count.
    let ext = legitimate_extension(&r.fx, 2);
    let first_imported = String::from_utf8(r.log.clone()).unwrap().lines().count();
    let (bad, bad_hash) = tamper_signature(&ext, first_imported);
    set_remote_text(&r.fx, &bad);

    assert_stopped(&r, "verification");

    let local_lines = String::from_utf8(r.log.clone()).unwrap().lines().count();
    let log = fs::read_to_string(r.fx.root.join("state/log.jsonl")).unwrap();
    assert_eq!(log.lines().count(), local_lines, "records were imported");
    assert!(
        !log.contains(&bad_hash),
        "the tampered record reached the local log"
    );

    // The untampered longer remote is the original, legitimate notes: the archive path imports
    // it and release resumes (a publish cannot be used here: the remote is ahead of the log).
    set_remote_text(&r.fx, &ext);
    let st = r.f.status_archived("p").unwrap();
    assert!(
        st.released,
        "restored remote still stopped: {:?}",
        st.reasons
    );
    let log = fs::read_to_string(r.fx.root.join("state/log.jsonl")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    let ext_lines: Vec<&str> = ext.lines().collect();
    assert_eq!(
        lines.len(),
        local_lines + 2 + 1,
        "local + 2 imported + 1 archived"
    );
    assert_eq!(
        lines[local_lines], ext_lines[local_lines],
        "the repaired record was imported"
    );
    assert_eq!(lines[local_lines + 1], ext_lines[local_lines + 1]);
    let snap = r.f.ledger().load().unwrap();
    assert_eq!(snap.checkpoint, local_lines as u64 + 2);
}
