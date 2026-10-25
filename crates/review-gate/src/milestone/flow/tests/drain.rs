//! The maintenance drain over the milestone routes (M2 step 8; Codex review of b7a3a91, n8 R1
//! and n10 §1): writes refused before any effect, a job admitted before the drain counted until
//! its result is on file, and a job that did not end well blocking a safe stop.

use super::*;
use crate::drain::Drain;
use crate::gate::Gate;
use crate::service::{AppState, serve};
use crate::store::Store;

const TOKEN: &str = "test-token-0123456789abcdef0123456789";

async fn start(flow: Arc<Flow>, root: &Path, drain: Arc<Drain>) -> String {
    let gate = Gate::new(Store::open(root.join("gate-state")).unwrap()).with_signer(kp());
    let app = AppState::new(
        gate,
        crate::reviewer::tests::cfg("http://127.0.0.1:9"),
        TOKEN.into(),
    )
    .with_drain(drain)
    .with_milestones(flow);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(serve(Arc::new(app), listener));
    url
}

async fn call(url: &str, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut r = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{url}{path}"))
        .bearer_auth(TOKEN);
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let code = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        code,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn health(url: &str) -> Value {
    reqwest::get(format!("{url}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Polls health until `pred` holds (10 s at most) and returns the last answer.
async fn health_until(url: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let mut h = Value::Null;
    for _ in 0..200 {
        h = health(url).await;
        if pred(&h) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h
}

/// Waits until the runner stand-in has read `n` request lines in full.
fn wait_lines(fx: &Fx, n: usize) {
    let (m, cv) = &*fx.lines;
    let g = m.lock().unwrap();
    let (_g, t) = cv
        .wait_timeout_while(g, Duration::from_secs(10), |c| *c < n)
        .unwrap();
    assert!(!t.timed_out(), "the runner never read request line {n}");
}

fn records(f: &Flow) -> usize {
    f.ledger().load().unwrap().records.len()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_milestone_write_is_refused_while_draining_and_the_log_is_unchanged() {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    let drain = Drain::new(None);
    let url = start(f.clone(), &fx.root, drain.clone()).await;
    drain.latch("test".into());
    let before = f.log().unwrap();
    let (seq, hash) = {
        let r = f.ledger().load().unwrap().records.last().cloned().unwrap();
        (r.seq, r.hash)
    };
    for (path, body) in [
        (
            "/milestones",
            json!({ "phase": "p2", "base": fx.base, "task_book": "plans/book.md", "evidence_dir": "ev" }),
        ),
        ("/milestones/p/requests", json!({ "head": fx.head })),
        ("/milestones/p/requests/p-q1/cancel", json!({})),
        ("/milestones/published", json!({ "seq": seq, "hash": hash })),
    ] {
        let (c, v) = call(&url, "POST", path, Some(body)).await;
        assert_eq!(c, 503, "{path}: {v}");
        assert!(
            v["error"].as_str().unwrap().starts_with("draining: test"),
            "{path}: {v}"
        );
    }
    assert_eq!(f.log().unwrap(), before, "nothing was written");
    assert_eq!(fx.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    // Reads still answer.
    let (c, v) = call(&url, "GET", "/milestones/p", None).await;
    assert_eq!(c, 200, "{v}");
    let h = health(&url).await;
    assert_eq!(h["drain"]["idle"], true, "{h}");
    assert_eq!(h["m2_pending"], 0, "{h}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_admitted_before_the_drain_is_counted_until_its_result_is_on_file() {
    let fx = Fx::new();
    let (tx, rx) = std::sync::mpsc::channel();
    *fx.mode.lock().unwrap() = Mode::Hold(Arc::new(Mutex::new(Some(rx))), approve());
    let f = fx.service();
    fx.register(&f);
    let drain = Drain::new(None);
    let url = start(f.clone(), &fx.root, drain.clone()).await;
    let (c, v) = call(
        &url,
        "POST",
        "/milestones/p/requests",
        Some(json!({ "head": fx.head })),
    )
    .await;
    assert_eq!(c, 200, "{v}");
    // The runner holds the job: the request route has returned, its job is still counted.
    tokio::task::block_in_place(|| wait_lines(&fx, 1));
    drain.latch("maintenance".into());
    let h = health(&url).await;
    assert_eq!(h["drain"]["active_ops"], 1, "{h}");
    assert_eq!(h["drain"]["idle"], false, "{h}");
    assert_eq!(h["m2_pending"], 1, "{h}");
    let (c, _) = call(
        &url,
        "POST",
        "/milestones/p/requests",
        Some(json!({ "head": fx.head })),
    )
    .await;
    assert_eq!(c, 503, "new requests are refused meanwhile");
    tx.send(()).unwrap();
    let h = health_until(&url, |h| h["drain"]["idle"] == true).await;
    assert_eq!(h["drain"]["idle"], true, "{h}");
    // Idle means persisted: the result is in the log and nothing is Pending.
    assert_eq!(h["m2_pending"], 0, "{h}");
    let st = derive(&f.ledger().load().unwrap().records, "p", 0);
    assert_ne!(st.current.unwrap().state, RequestState::Pending);
    assert!(kinds(&f).contains(&"job".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_that_was_not_sent_is_a_failure_and_blocks_a_safe_stop() {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    // Halt between the request's commit and the send: the job is not sent, nothing is recorded
    // for it, and its request stays Pending.
    let f2 = f.clone();
    f.hooks.lock().unwrap().before_send = Some(Arc::new(move || f2.halt("test halt".into())));
    let drain = Drain::new(None);
    let url = start(f.clone(), &fx.root, drain.clone()).await;
    let n = records(&f);
    let (c, v) = call(
        &url,
        "POST",
        "/milestones/p/requests",
        Some(json!({ "head": fx.head })),
    )
    .await;
    // The job thread halts on its own; the route may have answered before (200) or after
    // (503, 6c) that.
    assert!(c == 200 || c == 503, "{c} {v}");
    assert!(records(&f) > n, "the request itself was committed");
    drain.latch("maintenance".into());
    let h = health_until(&url, |h| h["drain"]["active_ops"] == 0).await;
    assert_eq!(h["drain"]["active_ops"], 0, "{h}");
    assert_eq!(h["drain"]["failure_count"], 1, "{h}");
    assert_eq!(
        h["drain"]["failures"][0], "m2 job p-q1: not sent (service halted)",
        "{h}"
    );
    assert_eq!(
        h["drain"]["idle"], false,
        "a failure blocks a safe stop: {h}"
    );
    assert_eq!(h["m2_pending"], 1, "{h}");
    assert_eq!(fx.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}
