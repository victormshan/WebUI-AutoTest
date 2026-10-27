//! The order of the milestone routes' checks (task book §6; m2b/A210-2): authentication, then
//! the path parameters (400, even once halted), then 404 without milestones and 503 once
//! halted, and only then the body (400 when it is not valid, whatever its content type).
//! Nothing is written on any of these refusals. Without milestones every write route is a 404,
//! never a 403 for the (empty) writer list (m2d/A218-1).

use super::*;
use crate::gate::Gate;
use crate::service::{AppState, serve};
use crate::store::Store;

const TOKEN: &str = "test-token-0123456789abcdef0123456789";

/// A gate with (`Some`) or without (`None`) milestones; returns its URL.
async fn start(flow: Option<Arc<Flow>>, root: &Path) -> String {
    let gate = Gate::new(Store::open(root.join("gate-state")).unwrap()).with_signer(kp());
    let mut app = AppState::new(
        gate,
        crate::reviewer::tests::cfg("http://127.0.0.1:9"),
        TOKEN.into(),
    );
    // Without --milestones there are no writers either, as in `serve` (m2d/A218-1: an empty
    // writer list answered 403 before the 404).
    if let Some(f) = flow {
        app = app
            .with_milestone_writers(["legacy-claude".to_string()])
            .with_milestones(f);
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(serve(Arc::new(app), listener));
    url
}

/// A POST whose body is sent as given, with no Content-Type header.
async fn raw_post(url: &str, path: &str, body: &'static str) -> (u16, String) {
    let r = reqwest::Client::new()
        .post(format!("{url}{path}"))
        .bearer_auth(TOKEN)
        .body(body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

fn records(f: &Flow) -> usize {
    f.ledger().load().unwrap().records.len()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_path_is_a_400_before_the_body_is_read_even_once_halted() {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    let url = start(Some(f.clone()), &fx.root).await;
    let n = records(&f);
    // Running: an invalid phase with a body that is not JSON and no Content-Type.
    let (c, v) = raw_post(&url, "/milestones/Bad%20Phase/requests", "not json").await;
    assert_eq!(c, 400, "{v}");
    assert!(v.contains("invalid phase"), "{v}");
    let (c, v) = raw_post(&url, "/milestones/p/requests/bad%20id/cancel", "").await;
    assert_eq!(c, 400, "{v}");
    assert!(v.contains("invalid request id"), "{v}");
    // Halted: still 400 for the path, 503 for a valid path whatever the body.
    let f2 = f.clone();
    tokio::task::spawn_blocking(move || f2.halt("test halt".into()))
        .await
        .unwrap();
    let (c, v) = raw_post(&url, "/milestones/Bad%20Phase/requests", "not json").await;
    assert_eq!(c, 400, "{v}");
    let (c, v) = raw_post(&url, "/milestones/p/requests/bad%20id/cancel", "").await;
    assert_eq!(c, 400, "{v}");
    for path in [
        "/milestones",
        "/milestones/p/requests",
        "/milestones/p/requests/p-q1/cancel",
        "/milestones/published",
    ] {
        let (c, v) = raw_post(&url, path, "not json").await;
        assert_eq!(c, 503, "{path}: {v}");
        assert!(v.contains("halted"), "{path}: {v}");
    }
    assert_eq!(records(&f), n, "nothing written");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_milestones_a_valid_path_is_a_404_whatever_the_body() {
    let d = tempfile::tempdir().unwrap();
    let url = start(None, d.path()).await;
    let (c, v) = raw_post(&url, "/milestones/Bad%20Phase/requests", "not json").await;
    assert_eq!(c, 400, "the path is checked first: {v}");
    for path in [
        "/milestones",
        "/milestones/p/requests",
        "/milestones/p/requests/p-q1/cancel",
        "/milestones/published",
    ] {
        let (c, v) = raw_post(&url, path, "not json").await;
        assert_eq!(c, 404, "{path}: {v}");
        assert!(v.contains("not enabled"), "{path}: {v}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_body_is_a_400_and_writes_nothing() {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    let url = start(Some(f.clone()), &fx.root).await;
    let n = records(&f);
    for (path, body) in [
        ("/milestones", "not json"),
        ("/milestones", r#"{"phase": "q"}"#),
        ("/milestones/p/requests", "not json"),
        ("/milestones/p/requests", r#"{"head": "x", "extra": 1}"#),
        ("/milestones/published", "{}"),
    ] {
        let (c, v) = raw_post(&url, path, body).await;
        assert_eq!(c, 400, "{path} {body}: {v}");
        assert!(v.contains("invalid request body"), "{path} {body}: {v}");
    }
    // A valid body is accepted without a Content-Type header.
    let (c, v) = raw_post(&url, "/milestones/Bad%20Phase/requests", r#"{"head": "x"}"#).await;
    assert_eq!(c, 400, "{v}");
    let (c, v) = raw_post(
        &url,
        "/milestones",
        r#"{"phase": "Bad Phase", "base": "x", "task_book": "b", "evidence_dir": "e"}"#,
    )
    .await;
    assert_eq!(
        c, 400,
        "the phase named in the body is checked after it is read: {v}"
    );
    assert!(v.contains("invalid phase"), "{v}");
    assert_eq!(records(&f), n, "nothing written");
}

/// A request whose body never arrives in full: the head says 100 bytes, one is sent, and the
/// connection stays open. The answer must come from the checks that run before the body is
/// read (Codex review of the fixes, item 2), within a short bound.
async fn unfinished_post(url: &str, path: &str) -> String {
    partial_post(url, path, 100).await
}

/// A POST that announces `length` bytes of body and sends one; the answer is read while the
/// rest is still owed (a server that waits for it times this out).
async fn partial_post(url: &str, path: &str, length: usize) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = url.strip_prefix("http://").unwrap();
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {length}\r\n\r\n{{"
    );
    c.write_all(head.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("an answer before the body is complete")
        .unwrap();
    String::from_utf8_lossy(&buf[..n]).to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_body_is_read_only_after_the_path_and_gate_checks() {
    let fx = Fx::new();
    let f = fx.service();
    fx.register(&f);
    let url = start(Some(f.clone()), &fx.root).await;
    let n = records(&f);
    let big = 3 * 1024 * 1024;
    // Too large: the path is still answered first, and a valid path gets a 400 for the body
    // (its announced length is over the limit), without the 3 MiB being sent.
    let a = partial_post(&url, "/milestones/Bad%20Phase/requests", big).await;
    assert!(
        a.starts_with("HTTP/1.1 400") && a.contains("invalid phase"),
        "{a}"
    );
    let a = partial_post(&url, "/milestones/p/requests", big).await;
    assert!(
        a.starts_with("HTTP/1.1 400") && a.contains("invalid request body"),
        "{a}"
    );
    // A body that never arrives: the path check answers at once.
    let a = unfinished_post(&url, "/milestones/Bad%20Phase/requests").await;
    assert!(a.starts_with("HTTP/1.1 400"), "{a}");
    // Halted: 503 before any body, too large or unfinished.
    let f2 = f.clone();
    tokio::task::spawn_blocking(move || f2.halt("test halt".into()))
        .await
        .unwrap();
    let a = partial_post(&url, "/milestones/p/requests", big).await;
    assert!(a.starts_with("HTTP/1.1 503"), "{a}");
    let a = unfinished_post(&url, "/milestones/p/requests").await;
    assert!(a.starts_with("HTTP/1.1 503"), "{a}");
    let a = unfinished_post(&url, "/milestones").await;
    assert!(a.starts_with("HTTP/1.1 503"), "{a}");
    assert_eq!(records(&f), n, "nothing written");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_milestones_an_unfinished_body_is_a_404_at_once() {
    let d = tempfile::tempdir().unwrap();
    let url = start(None, d.path()).await;
    for path in [
        "/milestones",
        "/milestones/p/requests",
        "/milestones/published",
    ] {
        let a = unfinished_post(&url, path).await;
        assert!(a.starts_with("HTTP/1.1 404"), "{path}: {a}");
    }
}
