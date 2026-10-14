//! Snapshot contract (B1 r3 §3.2, §5.1): the same page in McpBackend and NativeBackend
//! must give the same nodes (role, name, attributes, nesting) and the same answers from
//! the agent's snapshot functions; native uids must resolve to the right element and
//! go stale on navigation, re-snapshot and removal. Needs Chrome + npx, so ignored:
//! `cargo test -p agent -- --ignored`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent::native::{NativeBackend, NativeConfig, UidError};
use agent::snapshot;
use agent::{BrowserBackend, McpBackend};
use mcp_client::BrowserConfig;
use serde_json::json;

fn chrome() -> PathBuf {
    std::env::var_os("WEBTEST_CHROME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".local/bin/chrome-wsl")
        })
}

/// Serves `fixtures/contract` over HTTP on a random local port.
async fn serve_contract() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/contract");
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let h = tokio::spawn(async move {
        while let Ok((mut sock, _)) = l.accept().await {
            let dir = dir.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let path = path
                    .split(['?', '#'])
                    .next()
                    .unwrap_or("/")
                    .trim_start_matches('/');
                let file = dir.join(if path.is_empty() { "index.html" } else { path });
                if path == "oopif.html" {
                    // A frame from another site ("localhost" vs "127.0.0.1"): Chrome puts it
                    // in its own process.
                    let port = sock.local_addr().unwrap().port();
                    let page = format!(
                        "<!doctype html><title>Cross-site frame</title><h1>Outer</h1>\
                         <iframe src=\"http://localhost:{port}/frame.html\" title=\"Other site frame\"></iframe>"
                    );
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        page.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(page.as_bytes()).await;
                    return;
                }
                let (status, body) = match std::fs::read(&file) {
                    Ok(b) if file.starts_with(&dir) => ("200 OK", b),
                    _ => ("404 Not Found", b"not found".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });
    (base, h)
}

/// Snapshot lines with uids replaced, for comparing two backends.
fn without_uids(s: &str) -> Vec<String> {
    s.lines()
        .filter(|l| l.trim_start().starts_with("uid="))
        .map(|l| {
            let indent = l.len() - l.trim_start().len();
            let rest = l.trim_start().split_once(' ').map_or("", |(_, r)| r);
            format!("{}{}", " ".repeat(indent), rest)
        })
        .collect()
}

async fn mcp() -> McpBackend {
    McpBackend::launch(&BrowserConfig {
        chrome_path: Some(chrome()),
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn native(root: &Path) -> NativeBackend {
    NativeBackend::launch(&NativeConfig {
        profile_root: Some(root.to_path_buf()),
        ..NativeConfig::new(chrome())
    })
    .await
    .unwrap()
}

/// What the agent's snapshot functions say about a snapshot, uid-free, so two backends
/// can be compared (C2).
fn semantics(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines().filter(|l| l.trim_start().starts_with("uid=")) {
        let uid = line.trim_start()[4..].split(' ').next().unwrap();
        let loc = snapshot::locator_for(raw, uid);
        // Every node must have a locator and a description, and its locator must lead back
        // to this very uid (not merely to a node that looks the same).
        let back = loc.as_ref().and_then(|l| snapshot::resolve(raw, l));
        let same =
            loc.is_some() && snapshot::describe(raw, uid).is_some() && back.as_deref() == Some(uid);
        out.push(format!("{loc:?} resolves_back={same}"));
    }
    out.push(format!("landmarks={:?}", snapshot::landmarks(raw)));
    out.push(format!("fingerprint={}", snapshot::fingerprint(raw)));
    out.push(format!("label={}", snapshot::label(raw)));
    for (role, name) in [
        ("button", "Add to cart"),
        ("button", "Display none button"),
        ("button", "Aria hidden button"),
        ("button", "Visibility hidden button"),
        ("button", "Hidden attribute button"),
        ("button", "Frame button"),
        ("textbox", "Prefilled"),
        ("checkbox", "Mixed box"),
    ] {
        out.push(format!(
            "has_element({role}, {name})={}",
            snapshot::has_element(raw, role, name)
        ));
    }
    for text in [
        "Widget A costs 10.00",
        "Inside the frame",
        "Panel content",
        "Footer text",
    ] {
        out.push(format!(
            "contains_text({text})={}",
            snapshot::contains_text(raw, text)
        ));
    }
    out
}

const HIDDEN: [&str; 4] = [
    "Display none button",
    "Aria hidden button",
    "Visibility hidden button",
    "Hidden attribute button",
];

fn assert_same(what: &str, mcp: &str, native: &str) {
    let (a, b) = (without_uids(mcp), without_uids(native));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).map_or("", |s| s.as_str()),
            b.get(i).map_or("", |s| s.as_str()),
        );
        if x != y {
            eprintln!("{what} line {i}:\n  mcp:    {x}\n  native: {y}");
        }
    }
    assert_eq!(a, b, "{what}: snapshot text differs (lines above)");
    let (sm, sn) = (semantics(mcp), semantics(native));
    assert_eq!(sm, sn, "{what}: agent functions differ");
    // Equal is not enough: every locator must actually lead back to its own node.
    let broken: Vec<_> = sn
        .iter()
        .filter(|l| l.contains("resolves_back=false"))
        .collect();
    assert!(
        broken.is_empty(),
        "{what}: locators that do not resolve back: {broken:?}"
    );
    for name in HIDDEN {
        assert!(!mcp.contains(name), "{what}: mcp shows hidden {name:?}");
        assert!(
            !native.contains(name),
            "{what}: native shows hidden {name:?}"
        );
    }
    eprintln!(
        "{what}: {} nodes, text and agent functions identical",
        a.len()
    );
}

/// The role and name Chrome computes for a DOM node, to check what a uid points at.
async fn ax_of(n: &NativeBackend, backend_node_id: i64) -> (String, String) {
    let r = n
        .cdp(
            "Accessibility.getPartialAXTree",
            json!({ "backendNodeId": backend_node_id, "fetchRelatives": false }),
        )
        .await
        .unwrap();
    let node = &r["nodes"][0];
    (
        node["role"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        node["name"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

/// The element's `data-testid` attribute.
async fn testid_of(n: &NativeBackend, backend_node_id: i64) -> Option<String> {
    let d = n
        .cdp(
            "DOM.describeNode",
            json!({ "backendNodeId": backend_node_id }),
        )
        .await
        .unwrap();
    let attrs: Vec<String> =
        serde_json::from_value(d["node"]["attributes"].clone()).unwrap_or_default();
    attrs
        .chunks(2)
        .find(|kv| kv[0] == "data-testid")
        .map(|kv| kv[1].clone())
}

fn stale(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<UidError>(), Some(UidError::Stale { .. }))
}

fn uid_of(raw: &str, role: &str, name: &str, nth: usize) -> String {
    snapshot::resolve(
        raw,
        &agent::Locator {
            role: role.into(),
            name: name.into(),
            nth,
        },
    )
    .unwrap_or_else(|| panic!("{role} {name:?} #{nth} not in snapshot"))
}

#[tokio::test]
#[ignore = "needs Chrome + npx"]
async fn both_backends_snapshot_the_contract_page_alike() {
    let (base, server) = serve_contract().await;
    let root = tempfile::tempdir().unwrap();
    let m = mcp().await;
    let n = native(root.path()).await;

    for page in ["index.html", "second.html", "frame.html"] {
        let url = format!("{base}/{page}");
        assert!(!BrowserBackend::navigate(&m, &url).await.unwrap().is_error);
        assert!(!n.navigate(&url).await.unwrap().is_error);
        let ms = BrowserBackend::snapshot(&m).await.unwrap();
        let ns = n.snapshot().await.unwrap();
        assert_same(page, &ms, &ns);
    }

    // Dynamic insertion and removal, without navigating.
    let url = format!("{base}/index.html");
    assert!(!BrowserBackend::navigate(&m, &url).await.unwrap().is_error);
    assert!(!n.navigate(&url).await.unwrap().is_error);
    let insert = "() => { contract.insert('Dyn A'); contract.insert('Dyn B'); return true; }";
    let out = BrowserBackend::call(&m, "evaluate_script", json!({ "function": insert }))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text);
    n.evaluate(&format!("({insert})()")).await.unwrap();
    let (ms, ns) = (
        BrowserBackend::snapshot(&m).await.unwrap(),
        n.snapshot().await.unwrap(),
    );
    assert!(snapshot::has_element(&ns, "button", "Dyn B"));
    assert_same("after insert", &ms, &ns);
    let clear = "() => { contract.clear(); return true; }";
    assert!(
        !BrowserBackend::call(&m, "evaluate_script", json!({ "function": clear }))
            .await
            .unwrap()
            .is_error
    );
    n.evaluate(&format!("({clear})()")).await.unwrap();
    let (ms, ns) = (
        BrowserBackend::snapshot(&m).await.unwrap(),
        n.snapshot().await.unwrap(),
    );
    assert!(!snapshot::has_element(&ns, "button", "Dyn B"));
    assert_same("after removal", &ms, &ns);

    BrowserBackend::close(Box::new(m)).await.unwrap();
    n.shutdown().await.unwrap();
    server.abort();
}

/// Every uid of a native snapshot points at the element its line describes.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn every_native_uid_resolves_to_its_own_element() {
    let (base, server) = serve_contract().await;
    let root = tempfile::tempdir().unwrap();
    let n = native(root.path()).await;
    assert!(
        !n.navigate(&format!("{base}/index.html"))
            .await
            .unwrap()
            .is_error
    );
    let raw = n.snapshot().await.unwrap();
    let mut checked = 0;
    for line in raw.lines() {
        let t = line.trim_start();
        let uid = t[4..].split(' ').next().unwrap();
        let loc = snapshot::locator_for(&raw, uid).unwrap();
        let backend = match n.resolve_uid(uid).await {
            Ok(b) => b,
            Err(e) => panic!("{uid} ({loc}) did not resolve: {e:#}"),
        };
        let (role, name) = ax_of(&n, backend).await;
        // Chrome's internal role names differ for a few roles (e.g. "image" vs "img");
        // the name must match exactly whenever the line has one.
        if !loc.name.is_empty() {
            assert_eq!(
                name, loc.name,
                "{uid}: line says {loc}, element is {role} {name:?}"
            );
        }
        if loc.role != "StaticText" && loc.role != "RootWebArea" {
            assert!(
                role.eq_ignore_ascii_case(&loc.role)
                    || matches!(
                        (role.as_str(), loc.role.as_str()),
                        ("img", "image") | ("image", "img")
                    ),
                "{uid}: line says {loc}, element is {role} {name:?}"
            );
        }
        checked += 1;
    }
    eprintln!("{checked} uids resolved to their own elements");
    // Same role and name twice, and no name at all: the DOM's own markers show each uid
    // points at exactly its element (not merely at some element of that role and name).
    for (role, name, nth, testid) in [
        ("button", "Add to cart", 0, "add-a"),
        ("button", "Add to cart", 1, "add-b"),
        ("button", "", 0, "icon-button"),
    ] {
        let backend = n.resolve_uid(&uid_of(&raw, role, name, nth)).await.unwrap();
        assert_eq!(
            testid_of(&n, backend).await.as_deref(),
            Some(testid),
            "{role} {name:?} #{nth}"
        );
    }
    n.shutdown().await.unwrap();
    server.abort();
}

/// uids are valid for one snapshot of one document: a new snapshot, a navigation (even to a
/// page with an element of the same role and name), a reload or removing the element makes
/// them stale, and a stale uid never maps to another element.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn native_uids_go_stale_and_never_move() {
    let (base, server) = serve_contract().await;
    let root = tempfile::tempdir().unwrap();
    let n = native(root.path()).await;
    let index = format!("{base}/index.html");
    assert!(!n.navigate(&index).await.unwrap().is_error);

    // 1. A newer snapshot.
    let s1 = n.snapshot().await.unwrap();
    let old = uid_of(&s1, "button", "Add to cart", 0);
    let s2 = n.snapshot().await.unwrap();
    let e = n.resolve_uid(&old).await.unwrap_err();
    assert!(stale(&e), "re-snapshot: {e:#}");
    let current = uid_of(&s2, "button", "Add to cart", 0);
    assert_ne!(old, current);
    n.resolve_uid(&current).await.unwrap();

    // 2. Navigation to a page that has its own "Add to cart" button, without a new snapshot.
    assert!(
        !n.navigate(&format!("{base}/second.html"))
            .await
            .unwrap()
            .is_error
    );
    let e = n.resolve_uid(&current).await.unwrap_err();
    assert!(stale(&e), "navigation: {e:#}");

    // 3. Reload of the same URL.
    assert!(!n.navigate(&index).await.unwrap().is_error);
    let s3 = n.snapshot().await.unwrap();
    let before_reload = uid_of(&s3, "button", "Add to cart", 0);
    assert!(!n.navigate(&index).await.unwrap().is_error);
    let e = n.resolve_uid(&before_reload).await.unwrap_err();
    assert!(stale(&e), "reload: {e:#}");

    // 4. The element is removed from the page (same document, same snapshot).
    n.evaluate("contract.insert('Dyn A')").await.unwrap();
    let s4 = n.snapshot().await.unwrap();
    let dynamic = uid_of(&s4, "button", "Dyn A", 0);
    n.resolve_uid(&dynamic).await.unwrap();
    n.evaluate("contract.clear()").await.unwrap();
    let e = n.resolve_uid(&dynamic).await.unwrap_err();
    assert!(stale(&e), "removal: {e:#}");
    // ... even when an identical element takes its place.
    n.evaluate("contract.insert('Dyn A')").await.unwrap();
    let e = n.resolve_uid(&dynamic).await.unwrap_err();
    assert!(stale(&e), "replaced by an identical element: {e:#}");

    // 5. Made-up uids.
    let gen_now = s4.lines().next().unwrap()[4..]
        .split('_')
        .next()
        .unwrap()
        .to_string();
    let e = n.resolve_uid(&format!("{gen_now}_9999")).await.unwrap_err();
    assert!(
        matches!(e.downcast_ref::<UidError>(), Some(UidError::Unknown { .. })),
        "{e:#}"
    );
    let e = n.resolve_uid("not-a-uid").await.unwrap_err();
    assert!(e.downcast_ref::<UidError>().is_some(), "{e:#}");

    n.shutdown().await.unwrap();
    server.abort();
}

/// A load event from another navigation never completes this one: the first navigation to
/// a page that never loads is replaced by a second one, and must not report success.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn a_navigation_is_only_completed_by_its_own_load() {
    use tokio::io::AsyncWriteExt;
    let (base, server) = serve_contract().await;
    // Sends headers and part of a body, then stalls: committed, never loaded.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hang = format!("http://{}/", l.local_addr().unwrap());
    let stall = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut s, _)) = l.accept().await {
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<p>partial")
                .await;
            held.push(s);
        }
    });
    let root = tempfile::tempdir().unwrap();
    let n = std::sync::Arc::new(native(root.path()).await);
    let first = {
        let n = n.clone();
        tokio::spawn(async move { n.navigate(&hang).await })
    };
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let second = n.navigate(&format!("{base}/second.html")).await.unwrap();
    assert!(!second.is_error, "{}", second.text);
    let first = first.await.unwrap();
    match &first {
        Ok(out) => panic!("the replaced navigation reported success: {}", out.text),
        Err(e) => {
            eprintln!("replaced navigation: {e:#}");
            assert!(
                format!("{e:#}").contains("replaced by another navigation"),
                "{e:#}"
            );
        }
    }
    let n = std::sync::Arc::into_inner(n).unwrap();
    n.shutdown().await.unwrap();
    stall.abort();
    server.abort();
}

/// An iframe from another site lives in another process: the snapshot still succeeds and
/// leaves that frame empty (known B1 limit), while chrome-devtools-mcp shows its content.
#[tokio::test]
#[ignore = "needs Chrome + npx"]
async fn a_cross_process_iframe_is_left_empty_not_failed() {
    let (base, server) = serve_contract().await;
    let url = format!("{base}/oopif.html");
    let root = tempfile::tempdir().unwrap();
    let n = native(root.path()).await;
    assert!(!n.navigate(&url).await.unwrap().is_error);
    let ns = n.snapshot().await.unwrap();
    let m = mcp().await;
    assert!(!BrowserBackend::navigate(&m, &url).await.unwrap().is_error);
    let ms = BrowserBackend::snapshot(&m).await.unwrap();
    eprintln!("mcp:\n{ms}\nnative:\n{ns}");
    assert!(
        snapshot::has_element(&ms, "button", "Frame button"),
        "the page must really have frame content for this test to mean anything"
    );
    assert!(
        snapshot::has_element(&ns, "Iframe", "Other site frame"),
        "{ns}"
    );
    assert!(
        !snapshot::has_element(&ns, "button", "Frame button"),
        "{ns}"
    );
    BrowserBackend::close(Box::new(m)).await.unwrap();
    n.shutdown().await.unwrap();
    server.abort();
}

/// A failure reading a same-process iframe fails the snapshot instead of returning one
/// without the iframe's content.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn a_failed_iframe_read_fails_the_snapshot() {
    let (base, server) = serve_contract().await;
    let root = tempfile::tempdir().unwrap();
    let n = native(root.path()).await;
    assert!(
        !n.navigate(&format!("{base}/index.html"))
            .await
            .unwrap()
            .is_error
    );
    n.fail_iframe_snapshots(true);
    let e = n.snapshot().await.unwrap_err();
    assert!(format!("{e:#}").contains("iframe"), "{e:#}");
    n.fail_iframe_snapshots(false);
    let ok = n.snapshot().await.unwrap();
    assert!(snapshot::has_element(&ok, "button", "Frame button"));
    n.shutdown().await.unwrap();
    server.abort();
}

/// The resolution's linearization point is its end: a snapshot taken while a uid is being
/// resolved makes it stale. The test hook pauses the resolution right before its final
/// check, so the interleaving is deterministic.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn a_snapshot_during_a_resolution_makes_the_uid_stale() {
    let (base, server) = serve_contract().await;
    let root = tempfile::tempdir().unwrap();
    let n = std::sync::Arc::new(native(root.path()).await);
    assert!(
        !n.navigate(&format!("{base}/index.html"))
            .await
            .unwrap()
            .is_error
    );

    // Control: paused and released with no snapshot in between, it resolves.
    let raw = n.snapshot().await.unwrap();
    let uid = uid_of(&raw, "button", "Add to cart", 0);
    n.hold_next_resolve();
    let task = {
        let (n, uid) = (n.clone(), uid.clone());
        tokio::spawn(async move { n.resolve_uid(&uid).await })
    };
    n.resolve_held().await;
    n.release_resolve();
    task.await.unwrap().unwrap();

    // A snapshot while it is paused: stale, with that reason.
    n.hold_next_resolve();
    let task = {
        let (n, uid) = (n.clone(), uid.clone());
        tokio::spawn(async move { n.resolve_uid(&uid).await })
    };
    n.resolve_held().await;
    n.snapshot().await.unwrap();
    n.release_resolve();
    let e = task.await.unwrap().unwrap_err();
    assert!(stale(&e), "{e:#}");
    assert!(format!("{e:#}").contains("newer snapshot"), "{e:#}");

    let n = std::sync::Arc::into_inner(n).unwrap();
    n.shutdown().await.unwrap();
    server.abort();
}

/// Numbers are printed as JavaScript prints them: compared with the browser's own
/// String(Number(x)) for the edge cases, including integers beyond 2^53.
#[tokio::test]
#[ignore = "needs Chrome"]
async fn numbers_match_the_browsers_own_formatting() {
    let root = tempfile::tempdir().unwrap();
    let n = native(root.path()).await;
    let cases = [
        "0",
        "-0",
        "4",
        "-3",
        "0.5",
        "123.456",
        "0.30000000000000004",
        "0.000001",
        "0.0000015",
        "1e-7",
        "1.5e-7",
        "100000000000000000000",
        "1e21",
        "2.5e21",
        "-1e21",
        "1e300",
        "9007199254740993",
        "-9007199254740993",
        "18446744073709551615",
        "9.999999974752427e-7",
        "1.0000000116860974e-7",
    ];
    for c in cases {
        let browser = n.evaluate(&format!("String(Number('{c}'))")).await.unwrap();
        let json: serde_json::Value = serde_json::from_str(c).unwrap_or_else(|_| {
            // "-0" is not valid JSON; JSON.parse gives the same Number via -0.0.
            serde_json::json!(c.parse::<f64>().unwrap())
        });
        let ours = agent::native::ax::js_number(json.as_number().unwrap());
        assert_eq!(Some(ours.as_str()), browser.as_str(), "{c}");
    }
    eprintln!("{} numbers match the browser", cases.len());
    n.shutdown().await.unwrap();
}
