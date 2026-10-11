//! A Chrome this process owns, driven over a CDP pipe.
//!
//! Ownership: Chrome starts in its own process group (`setsid`), so shutdown can reach every
//! helper process, not only the one `Child` handle. `close` asks Chrome to exit, waits up to
//! `close_timeout`, then sends SIGTERM and finally SIGKILL to the group.
//!
//! Concurrency: one reader thread turns frames into responses (matched by id) and events
//! (broadcast). No lock is held across an await.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

use crate::config::LaunchConfig;
use crate::error::{Error, Result};
use crate::pipe;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// A CDP event: `method`, `params`, and the target session it came from (if any).
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

pub struct Browser {
    child: Mutex<Option<Child>>,
    pgid: i32,
    writer: Mutex<std::io::PipeWriter>,
    pending: Pending,
    next_id: AtomicU64,
    events: broadcast::Sender<Event>,
    config: LaunchConfig,
}

impl Browser {
    /// Launches Chrome with the configured profile and connects over the pipe.
    pub async fn launch(config: LaunchConfig) -> Result<Self> {
        config.validate()?;
        std::fs::create_dir_all(&config.user_data_dir)?;
        restrict(&config.user_data_dir)?;
        // to_chrome: we write, Chrome reads on fd 3. from_chrome: Chrome writes on fd 4.
        let (to_chrome_r, to_chrome_w) = std::io::pipe()?;
        let (from_chrome_r, from_chrome_w) = std::io::pipe()?;
        let (r3, w4) = (to_chrome_r.as_raw_fd(), from_chrome_w.as_raw_fd());
        let mut cmd = Command::new(&config.chrome);
        cmd.args(config.args())
            .envs(config.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // dup2 clears close-on-exec on the target descriptors.
                if libc::dup2(r3, 3) < 0 || libc::dup2(w4, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| Error::Launch(format!("{}: {e}", config.chrome.display())))?;
        let pgid = child.id() as i32;
        // Our copies of Chrome's ends must close, or EOF never reaches the reader.
        drop(to_chrome_r);
        drop(from_chrome_w);

        let pending: Pending = Arc::default();
        let (events, _) = broadcast::channel(1024);
        spawn_reader(from_chrome_r, pending.clone(), events.clone());
        let b = Self {
            child: Mutex::new(Some(child)),
            pgid,
            writer: Mutex::new(to_chrome_w),
            pending,
            next_id: AtomicU64::new(1),
            events,
            config,
        };
        // The first answer proves the pipe works; a Chrome that died on start fails here.
        b.call("Browser.getVersion", json!({}))
            .await
            .map_err(|e| match e {
                Error::BrowserDisconnected | Error::Timeout { .. } => {
                    Error::Launch(format!("Chrome did not answer on the pipe ({e})"))
                }
                other => other,
            })?;
        tracing::info!(pgid, "chrome launched");
        Ok(b)
    }

    /// Chrome's process group id (= the launched process's pid).
    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Sends a browser-level command.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_in(None, method, params).await
    }

    /// Sends a command, to a target session when `session_id` is given (flatten mode).
    pub async fn call_in(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session_id {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let bytes = pipe::frame(&msg);
        let wrote = self.writer.lock().unwrap().write_all(&bytes);
        if wrote.is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(Error::BrowserDisconnected);
        }
        let started = Instant::now();
        let resp = match tokio::time::timeout(self.config.command_timeout, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => return Err(Error::BrowserDisconnected),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                return Err(Error::Timeout {
                    method: method.into(),
                    millis: started.elapsed().as_millis(),
                });
            }
        };
        if let Some(err) = resp.get("error") {
            return Err(Error::Protocol {
                method: method.into(),
                code: err["code"].as_i64().unwrap_or(0),
                message: err["message"].as_str().unwrap_or("").into(),
            });
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Asks Chrome to exit, then makes sure the whole process group is gone.
    /// Returns how it ended: "graceful", "sigterm" or "sigkill".
    pub async fn close(&self) -> Result<&'static str> {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            self.call("Browser.close", json!({})),
        )
        .await;
        if self.wait_group(self.config.close_timeout).await {
            return Ok("graceful");
        }
        self.signal(libc::SIGTERM);
        if self.wait_group(Duration::from_secs(3)).await {
            return Ok("sigterm");
        }
        self.signal(libc::SIGKILL);
        self.wait_group(Duration::from_secs(3)).await;
        Ok("sigkill")
    }

    fn signal(&self, sig: i32) {
        // SAFETY: plain syscall on a process group this runtime created.
        unsafe {
            libc::killpg(self.pgid, sig);
        }
    }

    /// Waits until no process of the group is left (and reaps the leader).
    async fn wait_group(&self, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        loop {
            if let Some(c) = self.child.lock().unwrap().as_mut() {
                let _ = c.try_wait();
            }
            if !group_alive(self.pgid) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Last resort: never leave an owned Chrome behind.
        if group_alive(self.pgid) {
            self.signal(libc::SIGKILL);
        }
        if let Some(mut c) = self.child.lock().unwrap().take() {
            // Kill the leader itself too: if the group signal failed, a bare wait() would
            // block forever (found by mutation: without setsid there is no such group).
            let _ = c.kill();
            let _ = c.wait();
        }
        // Killed helpers stay visible until their new parent reaps them: wait, bounded.
        let end = Instant::now() + Duration::from_secs(3);
        while group_alive(self.pgid) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Whether any process of the group still exists (signal 0 probes without delivering).
pub fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence and permission.
    unsafe { libc::killpg(pgid, 0) == 0 }
}

fn restrict(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn spawn_reader(mut r: std::io::PipeReader, pending: Pending, events: broadcast::Sender<Event>) {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            for f in pipe::take_frames(&mut buf) {
                let Ok(v) = serde_json::from_slice::<Value>(&f) else {
                    tracing::warn!(bytes = f.len(), "unparsable CDP frame dropped");
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(v);
                    }
                } else if let Some(m) = v.get("method").and_then(Value::as_str) {
                    let _ = events.send(Event {
                        method: m.to_string(),
                        params: v.get("params").cloned().unwrap_or(Value::Null),
                        session_id: v.get("sessionId").and_then(Value::as_str).map(String::from),
                    });
                }
            }
        }
        // Pipe closed: fail every waiting command (dropping the senders).
        pending.lock().unwrap().clear();
        tracing::info!("chrome pipe closed");
    });
}
