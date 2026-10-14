//! NativeBackend: Chrome over a CDP pipe ([`browser_runtime`]), no MCP server and no
//! debugging port (B1). Opt-in only; [`crate::McpBackend`] stays the default.
//!
//! Done so far: lifecycle (phase 2a) and the snapshot with uid generations (phase 2b-1).
//! Everything else reports that it is not implemented (an error or `is_error`), never a
//! made-up success.
//!
//! A browser that dies stays dead: every later call fails as disconnected. Nothing here
//! starts a replacement Chrome, unlike chrome-devtools-mcp (see `mcp_client::Browser::restarted`).

pub mod ax;
pub mod nav;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use browser_runtime::browser::{Liveness, group_liveness};
use browser_runtime::{Browser, LaunchConfig};
use mcp_client::{StorageState, ToolInfo, ToolOutput};
use serde_json::{Value, json};

use crate::backend::{BrowserBackend, Coverage, Diagnostics};

#[derive(Debug, Clone)]
pub struct NativeConfig {
    /// Chrome executable (or a wrapper that execs it).
    pub chrome: PathBuf,
    pub headless: bool,
    /// Where the per-backend throwaway profiles are created (default: the system temp dir).
    pub profile_root: Option<PathBuf>,
    /// Further Chrome flags (e.g. `--no-sandbox`); a debugging port is refused.
    pub chrome_args: Vec<String>,
    /// Per-CDP-command timeout; also bounds waiting for a page load.
    pub command_timeout: Duration,
    /// Test hooks: pause the launch right before Chrome's first command (inside the runtime)
    /// or right before the page setup, so a test can cancel exactly there.
    #[doc(hidden)]
    pub pause_before_handshake: Option<Arc<browser_runtime::LaunchPause>>,
    #[doc(hidden)]
    pub pause_before_setup: Option<Arc<browser_runtime::LaunchPause>>,
}

impl NativeConfig {
    pub fn new(chrome: impl Into<PathBuf>) -> Self {
        Self {
            chrome: chrome.into(),
            headless: true,
            profile_root: None,
            chrome_args: Vec::new(),
            command_timeout: Duration::from_secs(30),
            pause_before_handshake: None,
            pause_before_setup: None,
        }
    }
}

/// Why a uid cannot be acted on. Never answered by picking a similar element instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UidError {
    /// From an older snapshot, or its element is no longer in the current document.
    Stale { uid: String, reason: String },
    /// In the current snapshot but has no DOM node (e.g. a purely computed AX node).
    NotActionable { uid: String },
    /// Not a uid of any snapshot this backend took.
    Unknown { uid: String },
}

impl std::fmt::Display for UidError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UidError::Stale { uid, reason } => write!(
                f,
                "uid {uid} is stale ({reason}); take a new snapshot and locate the element again"
            ),
            UidError::NotActionable { uid } => {
                write!(f, "uid {uid} has no DOM element to act on")
            }
            UidError::Unknown { uid } => {
                write!(f, "uid {uid} does not exist in the current snapshot")
            }
        }
    }
}

impl std::error::Error for UidError {}

/// The uids of the latest snapshot. Taking a snapshot starts a new generation and
/// invalidates every earlier uid.
#[derive(Debug, Default)]
struct Uids {
    generation: u64,
    /// Main frame and its document (loaderId) when the snapshot was taken.
    frame_id: String,
    loader_id: String,
    nodes: HashMap<String, Option<i64>>,
}

pub struct NativeBackend {
    // Field order is drop order: Chrome (whose Drop kills its process group and waits) goes
    // before the guard that then decides about the profile.
    browser: Option<Browser>,
    /// Flat-mode CDP session of the page this backend drives.
    session: String,
    command_timeout: Duration,
    uids: Mutex<Uids>,
    /// Test hook: make iframe snapshots fail (see `fail_iframe_snapshots`).
    fail_iframes: AtomicBool,
    /// Test hook: pause the next `resolve_uid` before its final check (see `hold_next_resolve`).
    hold_resolve: AtomicBool,
    resolve_held: tokio::sync::Notify,
    resolve_release: tokio::sync::Notify,
    guard: ProfileGuard,
}

/// Why a profile was not removed.
#[derive(Debug)]
pub enum DisposeError {
    /// Chrome's process group was not confirmed gone (still running, or the check failed):
    /// the profile is kept at this path.
    Kept(PathBuf, Liveness),
    /// Chrome was gone but removing the profile failed.
    RemoveFailed(PathBuf, std::io::Error),
}

impl std::fmt::Display for DisposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisposeError::Kept(p, Liveness::Unknown(errno)) => write!(
                f,
                "could not check whether Chrome exited (errno {errno}); profile kept at {}",
                p.display()
            ),
            DisposeError::Kept(p, _) => write!(
                f,
                "Chrome processes were still running; profile kept at {}",
                p.display()
            ),
            DisposeError::RemoveFailed(p, e) => {
                write!(f, "removing the profile at {} failed: {e}", p.display())
            }
        }
    }
}

impl std::error::Error for DisposeError {}

/// Removes the profile only when Chrome's group is confirmed gone; otherwise keeps it and
/// says where.
fn dispose_profile(profile: tempfile::TempDir, chrome: Liveness) -> Result<(), DisposeError> {
    if chrome != Liveness::Gone {
        return Err(DisposeError::Kept(profile.keep(), chrome));
    }
    let path = profile.path().to_path_buf();
    profile
        .close()
        .map_err(|e| DisposeError::RemoveFailed(path, e))
}

/// Owns the throwaway profile until Chrome is known to be gone, on every path: shutdown,
/// drop, a failed launch and a launch whose future is dropped midway.
///
/// It never signals Chrome. Ending the group is the runtime's job on every path (the
/// Browser's Drop, including the half-built one of a failed or cancelled launch); the guard
/// only probes the group afterwards, so a group id is never signalled once its owner has
/// let go of it.
struct ProfileGuard {
    profile: Option<tempfile::TempDir>,
    /// Chrome's process group, written by the runtime as soon as Chrome is spawned (0: not
    /// spawned).
    pgid: Arc<AtomicI32>,
    /// How the group is probed (injectable for tests).
    probe: fn(i32) -> Liveness,
}

impl ProfileGuard {
    fn dispose(&mut self) -> Result<(), DisposeError> {
        let Some(profile) = self.profile.take() else {
            return Ok(());
        };
        let pgid = self.pgid.load(Ordering::SeqCst);
        let chrome = if pgid <= 0 {
            Liveness::Gone
        } else {
            (self.probe)(pgid)
        };
        dispose_profile(profile, chrome)
    }
}

impl Drop for ProfileGuard {
    fn drop(&mut self) {
        if let Err(e) = self.dispose() {
            tracing::error!("native backend cleanup incomplete: {e}");
        }
    }
}

/// CDP errors (code -32000) that mean a node, a remote object or its execution context no
/// longer exists. Messages as Chrome 154 sends them (probed 2026-10-13); the objects passed
/// are ones this backend just obtained, so "invalid" here means "gone".
const GONE_MESSAGES: &[&str] = &[
    "No node with given id found",
    "Invalid remote object id",
    "Cannot find context with specified id",
    "Could not find object with given id",
];

/// An error from a CDP call made on a uid's node. Only a "gone" protocol error makes the
/// uid stale; a disconnect stays a disconnect, a timeout stays a timeout (outcome unknown),
/// and any other protocol error (invalid parameters, unknown method, …) keeps its meaning.
fn uid_call_error(uid: &str, e: anyhow::Error, disconnected: bool) -> anyhow::Error {
    if disconnected {
        return browser_runtime::Error::BrowserDisconnected.into();
    }
    match e.downcast_ref::<browser_runtime::Error>() {
        Some(browser_runtime::Error::Protocol { code, message, .. })
            if *code == -32000 && GONE_MESSAGES.iter().any(|m| message.contains(m)) =>
        {
            UidError::Stale {
                uid: uid.into(),
                reason: format!("its element went away ({message})"),
            }
            .into()
        }
        _ => e,
    }
}

impl NativeBackend {
    /// Starts Chrome on a fresh profile and attaches to its page. On any failure Chrome's
    /// process group is ended before the profile is removed.
    pub async fn launch(cfg: &NativeConfig) -> Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("webtest-native-");
        let profile = match &cfg.profile_root {
            Some(root) => {
                std::fs::create_dir_all(root)?;
                builder.tempdir_in(root)?
            }
            None => builder.tempdir()?,
        };
        let pgid = Arc::new(AtomicI32::new(0));
        let mut lc = LaunchConfig::new(&cfg.chrome, profile.path().join("chrome"));
        lc.headless = cfg.headless;
        lc.extra_args = cfg.chrome_args.clone();
        lc.command_timeout = cfg.command_timeout;
        lc.spawned_pgid = Some(pgid.clone());
        lc.pause_before_handshake = cfg.pause_before_handshake.clone();
        // From here on, an error return or a dropped future drops the guard after the
        // runtime has ended Chrome's group; it then removes the profile only if the group
        // is confirmed gone.
        let guard = ProfileGuard {
            profile: Some(profile),
            pgid,
            probe: group_liveness,
        };
        let browser = Browser::launch(lc).await.context("launching Chrome")?;
        if let Some(p) = &cfg.pause_before_setup {
            p.pause().await;
        }
        let session = setup_page(&browser)
            .await
            .context("attaching to the page")?;
        tracing::info!(pgid = browser.pgid(), "native backend ready");
        Ok(Self {
            browser: Some(browser),
            session,
            command_timeout: cfg.command_timeout,
            uids: Mutex::default(),
            fail_iframes: AtomicBool::new(false),
            hold_resolve: AtomicBool::new(false),
            resolve_held: tokio::sync::Notify::new(),
            resolve_release: tokio::sync::Notify::new(),
            guard,
        })
    }

    fn browser(&self) -> &Browser {
        self.browser.as_ref().expect("browser present until drop")
    }

    /// Chrome's process group.
    pub fn pgid(&self) -> i32 {
        self.browser().pgid()
    }

    /// This backend's throwaway profile (removed on close or drop).
    pub fn profile_dir(&self) -> &Path {
        self.guard
            .profile
            .as_ref()
            .expect("profile present until shutdown")
            .path()
    }

    pub fn is_disconnected(&self) -> bool {
        self.browser().is_disconnected()
    }

    /// Closes Chrome (gracefully, then by signal) and removes the profile.
    /// Returns how Chrome ended: "graceful", "sigterm" or "sigkill". It is an error if
    /// Chrome's process group outlives SIGKILL (the profile is then kept) or if removing
    /// the profile fails; both errors name the profile's path.
    pub async fn shutdown(mut self) -> Result<&'static str> {
        let closed = self.browser().close().await;
        drop(self.browser.take());
        if let Err(e) = self.guard.dispose() {
            return Err(match closed {
                Err(c) => anyhow::Error::new(e).context(format!("closing Chrome: {c}")),
                Ok(_) => e.into(),
            });
        }
        Ok(closed?)
    }

    /// Test hook: the next `resolve_uid` pauses right before its final check until
    /// `release_resolve`; `resolve_held` returns once it has paused. Lets a test take a
    /// snapshot in the middle of a resolution, deterministically.
    #[doc(hidden)]
    pub fn hold_next_resolve(&self) {
        self.hold_resolve.store(true, Ordering::SeqCst);
    }

    #[doc(hidden)]
    pub async fn resolve_held(&self) {
        self.resolve_held.notified().await;
    }

    #[doc(hidden)]
    pub fn release_resolve(&self) {
        self.resolve_release.notify_one();
    }

    /// Test hook: while on, every iframe snapshot fails, to check that such a failure is
    /// reported instead of yielding a snapshot without the iframe's content.
    #[doc(hidden)]
    pub fn fail_iframe_snapshots(&self, on: bool) {
        self.fail_iframes.store(on, Ordering::SeqCst);
    }

    /// A raw CDP command on the page's session. For tests and diagnostics; the agent
    /// only uses the [`BrowserBackend`] methods.
    pub async fn cdp(&self, method: &str, params: Value) -> Result<Value> {
        self.page(method, params).await
    }

    /// Evaluates a JavaScript expression in the page and returns its JSON value.
    /// A thrown exception is an `Err`.
    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        let r = self
            .page(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
            )
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            bail!(
                "evaluation threw: {}",
                ex["exception"]["description"].as_str().unwrap_or("?")
            );
        }
        Ok(r["result"]["value"].clone())
    }

    async fn page(&self, method: &str, params: Value) -> Result<Value> {
        Ok(self
            .browser()
            .call_in(Some(&self.session), method, params)
            .await?)
    }

    /// Main frame id and its current document's loaderId.
    async fn main_frame(&self) -> Result<(String, String)> {
        let tree = self.page("Page.getFrameTree", json!({})).await?;
        let frame = &tree["frameTree"]["frame"];
        match (frame["id"].as_str(), frame["loaderId"].as_str()) {
            (Some(id), Some(loader)) => Ok((id.to_string(), loader.to_string())),
            _ => bail!("Page.getFrameTree returned no main frame"),
        }
    }

    /// Frame ids of out-of-process iframes (their own CDP targets).
    async fn cross_process_frames(&self) -> Result<HashSet<String>> {
        let targets = self.browser().call("Target.getTargets", json!({})).await?;
        Ok(targets["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "iframe")
            .filter_map(|t| t["targetId"].as_str().map(String::from))
            .collect())
    }

    /// Accessibility tree of one frame (`None`: the main frame) with its iframes filled in.
    /// Only out-of-process iframes are left empty (a known B1 limit), and only when they are
    /// positively identified as such; any other failure to read an iframe fails the whole
    /// snapshot rather than silently dropping its content.
    async fn ax_snapshot(
        &self,
        frame_id: Option<String>,
        depth: usize,
        cross_process: &HashSet<String>,
    ) -> Result<Option<ax::SerNode>> {
        let params = match &frame_id {
            Some(f) => json!({ "frameId": f }),
            None => json!({}),
        };
        let nodes = self.page("Accessibility.getFullAXTree", params).await?;
        let Some(mut tree) = ax::AxTree::new(nodes["nodes"].as_array().map_or(&[][..], |v| v))
        else {
            return Ok(None);
        };
        for (index, backend) in tree.iframes() {
            if depth >= 8 {
                bail!("iframes nested deeper than 8 levels are not supported");
            }
            let desc = self
                .page("DOM.describeNode", json!({ "backendNodeId": backend }))
                .await
                .context("describing an iframe")?;
            // No content document (e.g. not loaded): nothing to show, as in Puppeteer.
            let Some(child) = desc["node"]["frameId"].as_str().map(String::from) else {
                continue;
            };
            if cross_process.contains(&child) {
                tracing::warn!(frame = %child, "out-of-process iframe left empty (B1 limit)");
                continue;
            }
            // Test hook: ask Chrome for a frame that does not exist, so the real CDP call fails.
            let read = if self.fail_iframes.load(Ordering::SeqCst) {
                "no-such-frame (test hook)".to_string()
            } else {
                child.clone()
            };
            tree.nodes[index].iframe =
                Box::pin(self.ax_snapshot(Some(read), depth + 1, cross_process))
                    .await
                    .with_context(|| format!("snapshot of iframe {child}"))?;
        }
        Ok(tree.snapshot())
    }

    /// Maps a uid of the latest snapshot to its DOM node, or says why it cannot.
    /// A uid only ever maps to the node it was issued for.
    ///
    /// Linearization point: the end. The uid must still belong to the latest snapshot when
    /// the checks are done; a snapshot taken while it was being resolved makes it stale.
    pub async fn resolve_uid(&self, uid: &str) -> Result<i64> {
        let now = self.main_frame().await?;
        let (backend, generation) = {
            let u = self.uids.lock().unwrap();
            (u.lookup(uid, &now)?, u.generation)
        };
        let stale = |reason: &str| -> anyhow::Error {
            UidError::Stale {
                uid: uid.into(),
                reason: reason.into(),
            }
            .into()
        };
        let resolved = self
            .page("DOM.resolveNode", json!({ "backendNodeId": backend }))
            .await
            .map_err(|e| uid_call_error(uid, e, self.is_disconnected()))?;
        let Some(object) = resolved["object"]["objectId"].as_str() else {
            return Err(stale("its element could not be resolved"));
        };
        let connected = self
            .page(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object,
                    "functionDeclaration": "function() { return this.isConnected; }",
                    "returnByValue": true,
                }),
            )
            .await
            .map_err(|e| uid_call_error(uid, e, self.is_disconnected()));
        let _ = self
            .page("Runtime.releaseObject", json!({ "objectId": object }))
            .await;
        if connected?["result"]["value"] != Value::Bool(true) {
            return Err(stale("its element was removed from the page"));
        }
        if self.hold_resolve.swap(false, Ordering::SeqCst) {
            self.resolve_held.notify_one();
            self.resolve_release.notified().await;
        }
        if self.uids.lock().unwrap().generation != generation {
            return Err(stale(
                "a newer snapshot was taken while it was being resolved",
            ));
        }
        Ok(backend)
    }
}

impl Uids {
    /// The checks that need no browser: the uid is from this snapshot, exists, has a DOM
    /// node, and the main frame (`now`: frame id, loaderId) still shows the snapshot's
    /// document. Whether the node is still attached is checked against the page after.
    fn lookup(&self, uid: &str, now: &(String, String)) -> Result<i64, UidError> {
        let generation = uid.split_once('_').and_then(|(g, _)| g.parse::<u64>().ok());
        let node = match (generation, self.nodes.get(uid)) {
            (Some(g), _) if g != self.generation => {
                return Err(UidError::Stale {
                    uid: uid.into(),
                    reason: format!("from snapshot {g}, the current one is {}", self.generation),
                });
            }
            (_, None) => return Err(UidError::Unknown { uid: uid.into() }),
            (_, Some(node)) => *node,
        };
        let Some(backend) = node else {
            return Err(UidError::NotActionable { uid: uid.into() });
        };
        if now.0 != self.frame_id || now.1 != self.loader_id {
            return Err(UidError::Stale {
                uid: uid.into(),
                reason: "the page has navigated since the snapshot".into(),
            });
        }
        Ok(backend)
    }
}

/// Attaches to the page target and enables what navigation and snapshots need.
async fn setup_page(browser: &Browser) -> Result<String> {
    let targets = browser.call("Target.getTargets", json!({})).await?;
    let page = targets["targetInfos"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["type"] == "page")
        .ok_or_else(|| anyhow!("Chrome has no page target"))?;
    let attached = browser
        .call(
            "Target.attachToTarget",
            json!({ "targetId": page["targetId"], "flatten": true }),
        )
        .await?;
    let session = attached["sessionId"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow!("Target.attachToTarget returned no sessionId"))?;
    for (method, params) in [
        ("Page.enable", json!({})),
        ("Page.setLifecycleEventsEnabled", json!({ "enabled": true })),
        ("Runtime.enable", json!({})),
    ] {
        browser.call_in(Some(&session), method, params).await?;
    }
    Ok(session)
}

fn not_implemented(what: &str) -> anyhow::Error {
    anyhow!("{what} is not implemented in the native backend yet (B1 phase 2b-2/3)")
}

#[async_trait]
impl BrowserBackend for NativeBackend {
    /// No tools yet: an empty catalog rather than names that would not work.
    async fn tools(&self) -> Result<Vec<ToolInfo>> {
        Ok(Vec::new())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<ToolOutput> {
        match tool {
            "navigate_page" if args["type"] == "url" || args["type"].is_null() => {
                let Some(url) = args["url"].as_str() else {
                    return Ok(ToolOutput {
                        text: "navigate_page needs `url`".into(),
                        is_error: true,
                        ..Default::default()
                    });
                };
                self.navigate(url).await
            }
            "take_snapshot" => Ok(ToolOutput {
                text: self.snapshot().await?,
                ..Default::default()
            }),
            _ => Ok(ToolOutput {
                text: format!("`{tool}` {args} is not supported by the native backend yet"),
                is_error: true,
                ..Default::default()
            }),
        }
    }

    /// Navigates and waits for this navigation's own `load` lifecycle event (matched by
    /// frame and loaderId, so an earlier or a later navigation's load cannot count).
    /// A load that does not finish in time is an `Err` (outcome unknown); another
    /// navigation replacing this one is an `Err` too.
    async fn navigate(&self, url: &str) -> Result<ToolOutput> {
        let mut events = self.browser().events();
        let nav = self.page("Page.navigate", json!({ "url": url })).await?;
        if let Some(err) = nav["errorText"].as_str() {
            return Ok(ToolOutput {
                text: format!("navigation to {url} failed: {err}"),
                is_error: true,
                ..Default::default()
            });
        }
        if let (Some(frame), Some(loader)) = (nav["frameId"].as_str(), nav["loaderId"].as_str()) {
            let session = self.session.as_str();
            let mut watch = nav::NavWatch::new(frame, loader);
            let loaded = async {
                loop {
                    // The event channel outlives the pipe, so poll for a disconnect too.
                    if self.is_disconnected() {
                        bail!(browser_runtime::Error::BrowserDisconnected);
                    }
                    let Ok(next) =
                        tokio::time::timeout(Duration::from_millis(200), events.recv()).await
                    else {
                        continue;
                    };
                    let e = match next {
                        Ok(e) if e.session_id.as_deref() == Some(session) => e,
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            bail!(browser_runtime::Error::BrowserDisconnected)
                        }
                    };
                    match watch.on_event(&e.method, &e.params) {
                        nav::NavStep::Loaded => return Ok(()),
                        nav::NavStep::Replaced => bail!(
                            "navigation to {url} was replaced by another navigation before it loaded"
                        ),
                        nav::NavStep::Pending => {}
                    }
                }
            };
            match tokio::time::timeout(self.command_timeout, loaded).await {
                Ok(r) => r?,
                Err(_) => bail!(
                    "{url} did not finish loading within {:?} (outcome unknown)",
                    self.command_timeout
                ),
            }
        }
        Ok(ToolOutput {
            text: format!("Successfully navigated to {url}."),
            ..Default::default()
        })
    }

    /// Accessibility snapshot in chrome-devtools-mcp's syntax. Starts a new uid
    /// generation: uids of earlier snapshots become stale.
    async fn snapshot(&self) -> Result<String> {
        let (frame_id, loader_id) = self.main_frame().await?;
        let root = self
            .ax_snapshot(None, 0, &self.cross_process_frames().await?)
            .await?
            .ok_or_else(|| anyhow!("take_snapshot: the accessibility tree is empty"))?;
        // A navigation while the tree was read would mix two documents.
        if self.main_frame().await? != (frame_id.clone(), loader_id.clone()) {
            bail!("take_snapshot: the page navigated while the snapshot was taken");
        }
        let mut u = self.uids.lock().unwrap();
        let generation = u.generation + 1;
        let (text, nodes) = ax::render(&root, generation);
        *u = Uids {
            generation,
            frame_id,
            loader_id,
            nodes: nodes.into_iter().collect(),
        };
        Ok(text)
    }

    async fn storage_state(&self) -> Result<StorageState> {
        Err(not_implemented("storage_state"))
    }

    async fn apply_storage_state(&self, _state: &StorageState) -> Result<()> {
        Err(not_implemented("apply_storage_state"))
    }

    /// Not collected yet. After a disconnect the answer is "failed", not merely
    /// "unsupported": whatever the page logged is gone with the browser. The state is
    /// read once so both channels always agree.
    async fn diagnostics(&self) -> Diagnostics {
        let coverage = if self.is_disconnected() {
            Coverage::Failed {
                reason: "browser disconnected".into(),
            }
        } else {
            Coverage::Unsupported
        };
        Diagnostics {
            console: coverage.clone(),
            network: coverage,
            ..Default::default()
        }
    }

    async fn close(self: Box<Self>) -> Result<()> {
        self.shutdown().await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Uids {
        Uids {
            generation: 3,
            frame_id: "F".into(),
            loader_id: "L1".into(),
            nodes: [("3_0".to_string(), Some(10)), ("3_1".to_string(), None)]
                .into_iter()
                .collect(),
        }
    }

    fn here() -> (String, String) {
        ("F".into(), "L1".into())
    }

    #[test]
    fn a_current_uid_maps_to_its_own_node() {
        assert_eq!(table().lookup("3_0", &here()), Ok(10));
    }

    #[test]
    fn an_older_generation_is_stale_even_if_the_number_exists() {
        assert!(matches!(
            table().lookup("2_0", &here()),
            Err(UidError::Stale { .. })
        ));
    }

    #[test]
    fn unknown_and_malformed_uids_are_unknown() {
        assert!(matches!(
            table().lookup("3_9", &here()),
            Err(UidError::Unknown { .. })
        ));
        assert!(matches!(
            table().lookup("nonsense", &here()),
            Err(UidError::Unknown { .. })
        ));
    }

    #[test]
    fn a_node_without_dom_is_not_actionable() {
        assert!(matches!(
            table().lookup("3_1", &here()),
            Err(UidError::NotActionable { .. })
        ));
    }

    /// A new document in the main frame (navigation, reload, a bfcache restore of another
    /// page) makes every uid stale, even if its DOM node would still resolve.
    #[test]
    fn a_new_document_makes_every_uid_stale() {
        let other_doc = ("F".to_string(), "L2".to_string());
        assert!(matches!(
            table().lookup("3_0", &other_doc),
            Err(UidError::Stale { .. })
        ));
        let other_frame = ("G".to_string(), "L1".to_string());
        assert!(matches!(
            table().lookup("3_0", &other_frame),
            Err(UidError::Stale { .. })
        ));
    }

    #[test]
    fn a_profile_is_removed_only_when_chrome_is_gone() {
        let root = tempfile::tempdir().unwrap();
        for state in [Liveness::Alive, Liveness::Unknown(1)] {
            let p = tempfile::tempdir_in(root.path()).unwrap();
            let path = p.path().to_path_buf();
            match dispose_profile(p, state) {
                Err(DisposeError::Kept(kept, s)) => {
                    assert_eq!((kept.as_path(), s), (path.as_path(), state))
                }
                other => panic!("{other:?}"),
            }
            assert!(
                path.exists(),
                "{state:?}: kept while Chrome may still use it"
            );
        }
        let p = tempfile::tempdir_in(root.path()).unwrap();
        let path = p.path().to_path_buf();
        dispose_profile(p, Liveness::Gone).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_profile_that_cannot_be_removed_is_an_error_naming_it() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir_in(root.path()).unwrap();
        std::fs::write(p.path().join("file"), b"x").unwrap();
        let path = p.path().to_path_buf();
        // A read-only profile directory: its entries cannot be unlinked.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
        let r = dispose_profile(p, Liveness::Gone);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        match r {
            Err(e @ DisposeError::RemoveFailed(..)) => {
                assert!(e.to_string().contains(&path.display().to_string()), "{e}")
            }
            other => panic!("{other:?}"),
        }
    }

    fn protocol(method: &str, code: i64, message: &str) -> anyhow::Error {
        browser_runtime::Error::Protocol {
            method: method.into(),
            code,
            message: message.into(),
        }
        .into()
    }

    fn kind(e: &anyhow::Error) -> &'static str {
        if let Some(UidError::Stale { .. }) = e.downcast_ref::<UidError>() {
            return "stale";
        }
        match e.downcast_ref::<browser_runtime::Error>() {
            Some(browser_runtime::Error::Protocol { .. }) => "protocol",
            Some(browser_runtime::Error::Timeout { .. }) => "timeout",
            Some(browser_runtime::Error::BrowserDisconnected) => "disconnected",
            _ => "other",
        }
    }

    /// Error texts below are what Chrome 154 returned when probed.
    #[test]
    fn errors_after_resolving_keep_their_meaning() {
        let cases = [
            (
                protocol("DOM.resolveNode", -32000, "No node with given id found"),
                "stale",
            ),
            (
                protocol("Runtime.callFunctionOn", -32000, "Invalid remote object id"),
                "stale",
            ),
            (
                protocol(
                    "Runtime.callFunctionOn",
                    -32000,
                    "Cannot find context with specified id",
                ),
                "stale",
            ),
            // Not returned in the probe (Chrome 154 said "Invalid remote object id" there);
            // kept for other Chrome versions' wording of the same condition.
            (
                protocol(
                    "Runtime.callFunctionOn",
                    -32000,
                    "Could not find object with given id",
                ),
                "stale",
            ),
            (
                protocol("Runtime.callFunctionOn", -32602, "Invalid parameters"),
                "protocol",
            ),
            (
                protocol("Foo.bar", -32601, "'Foo.bar' wasn't found"),
                "protocol",
            ),
            (
                protocol("DOM.resolveNode", -32000, "Some other server error"),
                "protocol",
            ),
            (
                browser_runtime::Error::Timeout {
                    method: "DOM.resolveNode".into(),
                    millis: 30_000,
                }
                .into(),
                "timeout",
            ),
        ];
        for (e, want) in cases {
            let shown = format!("{e:#}");
            assert_eq!(kind(&uid_call_error("3_0", e, false)), want, "{shown}");
        }
        let gone = protocol("DOM.resolveNode", -32000, "No node with given id found");
        assert_eq!(kind(&uid_call_error("3_0", gone, true)), "disconnected");
    }

    fn guard(root: &Path, pgid: i32, probe: fn(i32) -> Liveness) -> (ProfileGuard, PathBuf) {
        let p = tempfile::tempdir_in(root).unwrap();
        let path = p.path().to_path_buf();
        let g = ProfileGuard {
            profile: Some(p),
            pgid: Arc::new(AtomicI32::new(pgid)),
            probe,
        };
        (g, path)
    }

    /// The guard asks the probe and follows it: only a confirmed "gone" removes the profile,
    /// on drop as on dispose. It has no way to signal anything.
    #[test]
    fn the_guard_removes_the_profile_only_when_chrome_is_confirmed_gone() {
        let root = tempfile::tempdir().unwrap();
        let (g, path) = guard(root.path(), 4242, |_| Liveness::Alive);
        drop(g);
        assert!(path.exists(), "alive: kept");
        let (g, path) = guard(root.path(), 4242, |_| Liveness::Unknown(libc_eperm()));
        drop(g);
        assert!(path.exists(), "unknown: kept");
        let (g, path) = guard(root.path(), 4242, |_| Liveness::Gone);
        drop(g);
        assert!(!path.exists(), "gone: removed");
        // Never spawned: nothing to wait for.
        let (g, path) = guard(root.path(), 0, |_| panic!("no probe without a group"));
        drop(g);
        assert!(!path.exists());
        // dispose reports, then drop does nothing more.
        let (mut g, path) = guard(root.path(), 4242, |_| Liveness::Alive);
        assert!(matches!(g.dispose(), Err(DisposeError::Kept(..))));
        drop(g);
        assert!(path.exists());
    }

    fn libc_eperm() -> i32 {
        1
    }
}
