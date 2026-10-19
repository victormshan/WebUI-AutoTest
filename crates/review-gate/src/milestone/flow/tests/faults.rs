//! Flow service-level fault tests F1–F6 (M2 step 7; plan m2-step7-plan-20261020 n1 with
//! Codex's conditions n3). Each drives the service path (request → runner stand-in → result →
//! publish → `published_then_archive` / `status_archived`) and checks what is released, what is
//! written, and that release comes back once the fault is gone (through publish and archive,
//! never a shortcut). These are integration tests at the Flow service level, not a deployment
//! end-to-end test (that is step 8).

use super::*;

const REWORK: &str = "VERDICT: REWORK\n";

/// Waits until the runner stand-in has read `n` request lines in full.
fn wait_lines(fx: &Fx, n: usize) {
    let (m, cv) = &*fx.lines;
    let g = m.lock().unwrap();
    let (_g, t) = cv
        .wait_timeout_while(g, Duration::from_secs(10), |c| *c < n)
        .unwrap();
    assert!(!t.timed_out(), "the runner never read request line {n}");
}

/// The records of `kind` naming request `id`.
fn of(f: &Flow, kind: &str, id: &str) -> Vec<Record> {
    f.ledger()
        .load()
        .unwrap()
        .records
        .into_iter()
        .filter(|r| r.kind == kind && r.body["request_id"] == id)
        .collect()
}

/// Waits until request `id`'s audit is on file (the job's whole group is written by then).
fn wait_audit(f: &Flow, id: &str) -> Record {
    for _ in 0..400 {
        if let Some(a) = of(f, "audit", id).pop() {
            return a;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("no audit for {id}: {:?}", kinds(f));
}

/// How many `condition_resolved` records name condition `cond` (they carry no request id).
fn resolutions(f: &Flow, cond: &str) -> usize {
    f.ledger()
        .load()
        .unwrap()
        .records
        .iter()
        .filter(|r| r.kind == "condition_resolved" && r.body["condition"] == cond)
        .count()
}

fn last(f: &Flow) -> (u64, String) {
    let r = f.ledger().load().unwrap().records.last().cloned().unwrap();
    (r.seq, r.hash)
}

/// Publishes the log as the implementer would and reports it: the real path to archiving.
fn publish(fx: &Fx, f: &Flow) -> PublishOutcome {
    fx.publish(f);
    let (seq, hash) = last(f);
    f.published_then_archive(seq, &hash).unwrap()
}

fn state(f: &Flow, phase: &str) -> PhaseState {
    f.status_archived(phase).unwrap().state
}

fn open(st: &PhaseState, id: &str) -> bool {
    st.conditions
        .iter()
        .any(|c| c.id == id && c.closed.is_none())
}

/// A held request: the runner stand-in answers `answer` only once `release` is sent.
fn hold(fx: &Fx, answer: &str) -> std::sync::mpsc::Sender<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    *fx.mode.lock().unwrap() = Mode::Hold(Arc::new(Mutex::new(Some(rx))), answer.into());
    tx
}

/// F1: the auditor's answer cannot be parsed.
#[test]
fn f1_an_unparsable_answer_is_a_failed_job_without_effect_and_release_comes_back() {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    *fx.mode.lock().unwrap() = Mode::Answer("I think it is fine, mostly.".into());
    let id = f.request("p", &fx.head).unwrap();
    let st = settled(&f, "p");
    assert_eq!(st.current.as_ref().unwrap().state, RequestState::Failed);
    assert_eq!(of(&f, "audit", &id)[0].body["valid"], false);
    assert_eq!(resolutions(&f, "p/R1"), 0);
    assert_eq!(st.count, 0, "an invalid audit does not count");
    assert!(open(&st, "p/R1"));
    publish(&fx, &f);
    assert!(!f.status_archived("p").unwrap().released);
    // The fault gone: a valid audit, published and archived, releases.
    *fx.mode.lock().unwrap() = Mode::Answer(approve());
    f.request("p", &fx.head).unwrap();
    settled_after(&f, "p", &id);
    assert!(publish(&fx, &f).archived);
    let st = f.status_archived("p").unwrap();
    assert!(st.released, "{:?}", st.reasons);
}

/// Waits until `phase`'s current request is a different one than `old` and no longer Pending.
fn settled_after(f: &Flow, phase: &str, old: &str) -> PhaseState {
    for _ in 0..400 {
        let st = derive(&f.ledger().load().unwrap().records, phase, 0);
        if st
            .current
            .as_ref()
            .is_some_and(|c| c.request_id != old && c.state != RequestState::Pending)
        {
            return st;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("no new settled request after {old}: {:?}", kinds(f));
}

/// The three ways a result comes late (Codex n3 condition 2): the request is cancelled,
/// superseded or rebaselined while its job runs. The late answer resolves R1, to challenge the
/// closing logic; the count is non-zero beforehand, so a wrongful reset would show.
#[derive(Debug, Clone, Copy)]
enum Late {
    Cancel,
    Supersede,
    Rebaseline,
}

fn late(how: Late) {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    // A first audit that counts: count 1.
    *fx.mode.lock().unwrap() = Mode::Answer(REWORK.into());
    let first = f.request("p", &fx.head).unwrap();
    let st = settled(&f, "p");
    assert_eq!(st.count, 1, "{st:?}");
    // The held job: the runner has read its whole line before the request is overtaken.
    let release = hold(&fx, &approve());
    let id = f.request("p", &fx.head).unwrap();
    assert_ne!(id, first);
    wait_lines(&fx, 2);
    match how {
        Late::Cancel => f.cancel("p", &id).unwrap(),
        Late::Supersede => {
            f.apply(
                "p2",
                &fx.base,
                "plans/book.md",
                "ev",
                vec![],
                Some("p".into()),
            )
            .unwrap();
            f.confirm("p2").unwrap();
        }
        Late::Rebaseline => {
            f.rebaseline("p", &fx.head, "the user accepts a new book")
                .unwrap();
        }
    }
    // Cancel and supersession close the request with a record; a rebaseline ends it by
    // derivation (its baseline is no longer the phase's), with no record of its own.
    let closed_before = of(&f, "request_closed", &id);
    let want = usize::from(!matches!(how, Late::Rebaseline));
    assert_eq!(closed_before.len(), want, "{how:?}: {:?}", kinds(&f));
    let resolved_before = resolutions(&f, "p/R1");
    assert_eq!(resolved_before, 0);
    release.send(()).unwrap();
    let audit = wait_audit(&f, &id);
    assert_eq!(
        audit.body["valid"], true,
        "the late answer itself is a valid audit"
    );
    // Kept as late, without effect.
    assert_eq!(
        resolutions(&f, "p/R1"),
        resolved_before,
        "{how:?}: nothing resolved"
    );
    assert_eq!(
        of(&f, "request_closed", &id),
        closed_before,
        "{how:?}: the terminal state is not written again"
    );
    let st = derive(&f.ledger().load().unwrap().records, "p", 0);
    assert!(open(&st, "p/R1"), "{how:?}: R1 still open");
    assert_eq!(st.count, 1, "{how:?}: the count is unchanged");
    assert_ne!(
        st.current.as_ref().map(|c| c.state.clone()),
        Some(RequestState::Completed),
        "{how:?}"
    );
    publish(&fx, &f);
    assert!(!f.status_archived("p").unwrap().released, "{how:?}");
    if let Late::Supersede = how {
        // The new phase inherits the open condition and the count.
        let p2 = state(&f, "p2");
        assert!(open(&p2, "p/R1"), "p2 inherits R1: {p2:?}");
        assert_eq!(p2.count, 1, "p2 inherits the count");
        assert!(!p2.released);
    }
}

#[test]
fn f2_a_result_after_cancel_is_late_and_without_effect() {
    late(Late::Cancel);
}

#[test]
fn f2_a_result_after_supersession_is_late_and_without_effect() {
    late(Late::Supersede);
}

#[test]
fn f2_a_result_after_rebaseline_is_late_and_without_effect() {
    late(Late::Rebaseline);
}

/// F3: the runner returns an earlier job's reply, byte for byte. The binding check (job id and
/// sealed input) refuses it; the earlier approval stays ended by the new request.
#[test]
fn f3_a_replayed_earlier_reply_is_refused_and_does_not_restore_the_release() {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    let first = f.request("p", &fx.head).unwrap();
    settled(&f, "p");
    assert!(publish(&fx, &f).archived);
    assert!(f.status_archived("p").unwrap().released);
    let old = fx.sent.lock().unwrap().last().cloned().unwrap();
    assert!(old.contains(&first), "the reply to the first job");
    // A new request ends the approval; its runner replays the old APPROVE reply.
    *fx.mode.lock().unwrap() = Mode::Replay(old);
    let id = f.request("p", &fx.head).unwrap();
    let st = settled_after(&f, "p", &first);
    assert_eq!(st.current.as_ref().unwrap().request_id, id);
    assert_eq!(st.current.as_ref().unwrap().state, RequestState::Failed);
    let audit = &of(&f, "audit", &id)[0];
    assert_eq!(audit.body["valid"], false);
    assert!(
        audit.body["reason"]
            .as_str()
            .unwrap()
            .contains("is not the request"),
        "{}",
        audit.body
    );
    publish(&fx, &f);
    let s = f.status_archived("p").unwrap();
    assert!(
        !s.released,
        "a replay never restores a release: {:?}",
        s.reasons
    );
    // The fault gone: a fresh, valid reply releases again (R1 was resolved by the first audit,
    // so this approval resolves nothing).
    *fx.mode.lock().unwrap() = Mode::Answer("VERDICT: APPROVE\n".into());
    f.request("p", &fx.head).unwrap();
    settled_after(&f, "p", &id);
    assert!(publish(&fx, &f).archived);
    let s = f.status_archived("p").unwrap();
    assert!(s.released, "{:?} {:?}", s.reasons, kinds(&f));
}

/// F4: an approval that resolves only one of two open blocking conditions.
#[test]
fn f4_an_approval_that_leaves_a_blocking_condition_unresolved_does_not_release() {
    let fx = Fx::new();
    let f = fx.service();
    f.apply(
        "p",
        &fx.base,
        "plans/book.md",
        "ev",
        vec!["first".into(), "second".into()],
        None,
    )
    .unwrap();
    f.confirm("p").unwrap();
    let st = state(&f, "p");
    assert!(open(&st, "p/R1") && open(&st, "p/R2"));
    // APPROVE, resolving R1 only: R2 is left out.
    let first = f.request("p", &fx.head).unwrap();
    settled(&f, "p");
    assert_eq!((resolutions(&f, "p/R1"), resolutions(&f, "p/R2")), (1, 0));
    assert!(publish(&fx, &f).archived);
    let s = f.status_archived("p").unwrap();
    assert!(
        !open(&s.state, "p/R1") && open(&s.state, "p/R2"),
        "{:?}",
        s.state.conditions
    );
    assert!(!s.released, "R2 is still open: {:?}", s.reasons);
    // R2 resolved by a later audit: released.
    *fx.mode.lock().unwrap() = Mode::Answer("VERDICT: APPROVE\nRESOLVED:\n- p/R2: done\n".into());
    f.request("p", &fx.head).unwrap();
    settled_after(&f, "p", &first);
    assert!(publish(&fx, &f).archived);
    let s = f.status_archived("p").unwrap();
    assert!(s.released, "{:?}", s.reasons);
}

/// F5: the normal unarchived tail. The bookkeeping records that publishing writes (published,
/// archived) do not hold release back themselves.
#[test]
fn f5_a_result_not_yet_archived_releases_only_once_published_and_archived() {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    f.request("p", &fx.head).unwrap();
    settled(&f, "p");
    let s = f.status_archived("p").unwrap();
    assert!(!s.released);
    assert!(
        s.reasons.iter().any(|r| r.contains("not archived")),
        "{:?}",
        s.reasons
    );
    let o = publish(&fx, &f);
    assert!(o.published && o.archived, "{o:?}");
    let s = f.status_archived("p").unwrap();
    assert!(s.released, "{:?}", s.reasons);
    let ks = kinds(&f);
    assert_eq!(&ks[ks.len() - 2..], ["published", "archived"], "{ks:?}");
}

/// A consistent copy of a flow's state while nothing writes it: the flow is dropped first (its
/// lock released, no job thread running), then the directory is copied.
fn backup(fx: &Fx, f: Arc<Flow>, name: &str) -> PathBuf {
    let weak = Arc::downgrade(&f);
    drop(f);
    for _ in 0..400 {
        if weak.strong_count() == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(weak.strong_count(), 0, "a job thread still holds the flow");
    let to = fx.root.join(name);
    let ok = Command::new("cp")
        .arg("-a")
        .arg(fx.root.join("state"))
        .arg(&to)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    to
}

/// The service on `fx`'s state again, right after the previous one was dropped. flock follows
/// the open file: a process a parallel test forks at that moment holds a copy of the descriptor
/// until it execs, so the lock can take a moment to be free (as in
/// `a_real_write_failure_stops_a_request_prepared_meanwhile`). In production the old service has
/// exited first.
fn reopen(fx: &Fx) -> Arc<Flow> {
    for _ in 0..50 {
        if let Ok(f) = Flow::open_service(fx.cfg(), kp(), fx.reader()) {
            return f;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the next service did not start once the old one was gone");
}

/// F6: the local state is an older backup holding an approval, while the remote has moved on.
/// `revoke`: the user revoked the phase on the live gate. Otherwise: a new request is pending.
fn stale_backup(revoke: bool) {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    let first = f.request("p", &fx.head).unwrap();
    settled(&f, "p");
    assert!(publish(&fx, &f).archived);
    assert!(f.status_archived("p").unwrap().released);
    let old = backup(&fx, f, "backup");
    // The live gate moves on and publishes.
    let live = reopen(&fx);
    let release = if revoke {
        live.revoke("p", "the user revoked it").unwrap();
        None
    } else {
        // Kept pending for the whole test: the runner holds the job.
        let tx = hold(&fx, &approve());
        live.request("p", &fx.head).unwrap();
        wait_lines(&fx, 2);
        Some(tx)
    };
    let (seq, hash) = last(&live);
    fx.publish(&live);
    live.published(seq, &hash).unwrap();
    // The backup, opened as a service: opening alone changes nothing.
    let log = old.join("log.jsonl");
    let before = fs::read(&log).unwrap();
    let mut cfg = fx.cfg();
    cfg.dir = old.clone();
    let b = Flow::open_service(cfg, kp(), fx.reader()).unwrap();
    assert_eq!(
        fs::read(&log).unwrap(),
        before,
        "opening writes nothing here"
    );
    let snap = b.ledger().load().unwrap();
    assert!(
        derive(&snap.records, "p", snap.checkpoint).released,
        "the backup on its own still holds the old approval"
    );
    let s = b.status_archived("p").unwrap();
    let imported = b.ledger().load().unwrap().records;
    let want = if revoke { "revoke" } else { "request" };
    assert!(
        imported
            .iter()
            .any(|r| r.phase == "p" && r.kind == want && r.seq > first_seq(&imported, &first)),
        "the remote's {want} is imported: {:?}",
        imported.iter().map(|r| &r.kind).collect::<Vec<_>>()
    );
    assert!(
        !s.released,
        "the old approval never releases: {:?}",
        s.reasons
    );
    if !revoke {
        let cur = s.state.current.as_ref().unwrap();
        assert_ne!(cur.request_id, first, "the newer request is current");
        assert_eq!(cur.state, RequestState::Pending, "still pending");
    }
    if let Some(tx) = release {
        // Let the live gate's held job finish and land before the test's directory goes away
        // (Codex 7 review n4): its result is a normal, recorded completion.
        tx.send(()).unwrap();
        let id = live_request(&live);
        wait_audit(&live, &id);
        jobs_finished(&live);
    }
}

/// The live gate's newest request id.
fn live_request(f: &Flow) -> String {
    f.ledger()
        .load()
        .unwrap()
        .records
        .iter()
        .rev()
        .find(|r| r.kind == "request")
        .map(|r| r.body["request_id"].as_str().unwrap().to_string())
        .unwrap()
}

fn first_seq(records: &[Record], id: &str) -> u64 {
    records
        .iter()
        .find(|r| r.kind == "request" && r.body["request_id"] == id)
        .map_or(0, |r| r.seq)
}

#[test]
fn f6_a_stale_backup_does_not_release_once_the_remote_has_a_revoke() {
    stale_backup(true);
}

#[test]
fn f6_a_stale_backup_does_not_release_once_the_remote_has_a_newer_request() {
    stale_backup(false);
}
