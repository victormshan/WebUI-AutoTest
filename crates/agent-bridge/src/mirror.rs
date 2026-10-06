//! Read-only mirror of every message as a file, for people (and for DSH, whose notify handoff
//! points at `D:\cc-tasks\claude-bridge\tasks\<taskId>\`): `<dir>/<task>/msg-<n>-<from>.md`.
//!
//! The mirror is **not** a data source (design b5): editing a mirror file changes nothing, and
//! the self-check reports any message file in the mirror that the store does not have — someone
//! edited the mirror, or is still hand-writing the old file protocol — instead of adopting it.
//!
//! The mirror usually lives on a Windows drive, where renaming over a file that someone has open
//! fails (EPERM/EBUSY/EACCES; measured by DSH: 130 of 145 attempts while a reader held the file).
//! Such failures are retried for about a second and then reported, never silently dropped.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::model::Message;

/// Total time to keep retrying a rename that fails because the target is in use.
const RENAME_BUDGET: Duration = Duration::from_millis(1000);

pub struct Mirror {
    dir: PathBuf,
}

/// Files in the mirror that the store does not know (b5 self-check).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Drift {
    pub unknown: Vec<String>,
}

/// Outcome counters of the copy writer (mirror files and step-relay notes).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CopyStats {
    pub written: u64,
    pub failed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Message files in the mirror the store does not have (b5): reported, never adopted.
    pub drift: Drift,
    /// Jobs queued but not yet done.
    pub queued: u64,
}

enum Job {
    Mirror(Message),
    Trace {
        dir: PathBuf,
        expr: String,
        msg: Message,
    },
    Sync {
        messages: Vec<Message>,
        known: HashSet<(String, u32, String)>,
        done: mpsc::Sender<()>,
    },
}

/// One thread does all copy I/O, in order. Request handlers only enqueue (never wait, never
/// hold the service lock across I/O), and a slow or stuck Windows drive delays copies, not the
/// service; the thread count stays at one however many messages arrive.
pub struct Copier {
    tx: Mutex<mpsc::Sender<Job>>,
    stats: Arc<Mutex<CopyStats>>,
    mirror_dir: Option<PathBuf>,
    /// Test hook: extra time each job takes, to stand in for a slow drive.
    slow_ms: Arc<std::sync::atomic::AtomicU64>,
}

fn bump(stats: &Mutex<CopyStats>, r: Result<()>) {
    let mut st = stats.lock().unwrap_or_else(|p| p.into_inner());
    st.queued = st.queued.saturating_sub(1);
    match r {
        Ok(()) => st.written += 1,
        Err(e) => {
            eprintln!("agent-bridge: copy failed: {e:#}");
            st.failed += 1;
            st.last_error = Some(format!("{e:#}"));
        }
    }
}

impl Copier {
    pub fn spawn(mirror: Option<Mirror>) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let stats: Arc<Mutex<CopyStats>> = Arc::default();
        let st = stats.clone();
        let mirror_dir = mirror.as_ref().map(|m| m.dir().to_path_buf());
        let slow_ms: Arc<std::sync::atomic::AtomicU64> = Arc::default();
        let slow = slow_ms.clone();
        std::thread::Builder::new()
            .name("agent-bridge-copier".into())
            .spawn(move || {
                for job in rx {
                    let ms = slow.load(std::sync::atomic::Ordering::Relaxed);
                    if ms > 0 {
                        std::thread::sleep(Duration::from_millis(ms));
                    }
                    match job {
                        Job::Mirror(m) => {
                            if let Some(mirror) = &mirror {
                                bump(&st, mirror.write(&m));
                            }
                        }
                        Job::Trace { dir, expr, msg } => bump(&st, append_trace(&dir, &expr, &msg)),
                        Job::Sync { messages, known, done } => {
                            if let Some(mirror) = &mirror {
                                for m in messages.iter().filter(|m| !m.imported && !mirror.path(m).exists()) {
                                    let r = mirror.write(m);
                                    let mut g = st.lock().unwrap_or_else(|p| p.into_inner());
                                    match r {
                                        Ok(()) => g.written += 1,
                                        Err(e) => {
                                            g.failed += 1;
                                            g.last_error = Some(format!("{e:#}"));
                                        }
                                    }
                                }
                                let drift = mirror.check(|t, n, f| known.contains(&(t.to_string(), n, f.to_string())));
                                let mut g = st.lock().unwrap_or_else(|p| p.into_inner());
                                if !drift.unknown.is_empty() && drift != g.drift {
                                    eprintln!(
                                        "agent-bridge: mirror holds {} file(s) the store does not have (not adopted): {:?}",
                                        drift.unknown.len(),
                                        drift.unknown
                                    );
                                }
                                g.drift = drift;
                            }
                            let mut g = st.lock().unwrap_or_else(|p| p.into_inner());
                            g.queued = g.queued.saturating_sub(1);
                            drop(g);
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("spawn copier thread");
        Self {
            tx: Mutex::new(tx),
            stats,
            mirror_dir,
            slow_ms,
        }
    }

    #[doc(hidden)]
    pub fn set_slow_for_tests(&self, ms: u64) {
        self.slow_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    fn send(&self, job: Job) {
        self.stats.lock().unwrap_or_else(|p| p.into_inner()).queued += 1;
        let _ = self.tx.lock().unwrap_or_else(|p| p.into_inner()).send(job);
    }

    pub fn mirror_dir(&self) -> Option<&Path> {
        self.mirror_dir.as_deref()
    }

    /// Queues the mirror file of one message.
    pub fn mirror(&self, m: Message) {
        if self.mirror_dir.is_some() {
            self.send(Job::Mirror(m));
        }
    }

    /// Queues a note in claude-step-relay's trace of `expr`.
    pub fn trace(&self, dir: PathBuf, expr: String, msg: Message) {
        self.send(Job::Trace { dir, expr, msg });
    }

    /// Queues "write missing mirror files, then self-check" over a snapshot of the store.
    /// The receiver fires when it is done (start-up and tests may wait on it).
    pub fn sync(&self, messages: Vec<Message>) -> mpsc::Receiver<()> {
        let (done, rx) = mpsc::channel();
        let known = messages
            .iter()
            .map(|m| (m.task.clone(), m.n, m.from.clone()))
            .collect();
        self.send(Job::Sync {
            messages,
            known,
            done,
        });
        rx
    }

    pub fn stats(&self) -> CopyStats {
        self.stats.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// Appends a note to claude-step-relay's trace of `expr` (only if that trace exists).
pub fn append_trace(dir: &Path, expr: &str, m: &Message) -> Result<()> {
    anyhow::ensure!(crate::valid_id(expr), "invalid exprId {expr:?}");
    let path = dir.join("traces").join(format!("{expr}.md"));
    if !path.exists() {
        return Ok(());
    }
    let body: String = m.body.chars().take(300).collect();
    // A line looking like an entry header would split the entry: escape it, as step-relay does.
    let body = body
        .lines()
        .map(|l| {
            if l.starts_with("## [") {
                format!("\\{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let entry = format!(
        "## [{}] [Claude⇄DSH 桥接]\n\n{} #{} {}（来自 {}）：{}\n\n",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        m.task,
        m.n,
        m.kind,
        m.from,
        body.trim()
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(entry.as_bytes())?;
    Ok(())
}

pub fn file_name(m: &Message) -> String {
    format!("msg-{}-{}.md", m.n, m.from)
}

/// Markdown rendering of one message: a fixed header, the body, then structured fields.
pub fn render(m: &Message) -> String {
    let mut s = format!(
        "taskId: {}   n: {}   from: {}   kind: {}   protocol: {}   at: {}\n",
        m.task,
        m.n,
        m.from,
        m.kind,
        m.protocol,
        m.at.to_rfc3339()
    );
    for (k, v) in [
        ("replyTo", m.reply_to.map(|x| x.to_string())),
        ("supersedes", m.supersedes.map(|x| x.to_string())),
        (
            "outcome",
            m.outcome.map(|o| {
                serde_json::to_value(o)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            }),
        ),
        (
            "judgement",
            m.judgement.map(|j| {
                serde_json::to_value(j)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            }),
        ),
    ] {
        if let Some(v) = v {
            s.push_str(&format!("{k}: {v}\n"));
        }
    }
    s.push_str("<!-- 只读镜像：数据以 agent-bridge 服务为准，修改本文件无效 -->\n\n");
    if let Some(meta) = &m.meta {
        s.push_str(&format!("# {}\n\n→ {}\n\n", meta.title, meta.to));
    }
    s.push_str(m.body.trim_end());
    s.push('\n');
    if !m.questions.is_empty() {
        s.push_str("\n## 问题\n");
        for q in &m.questions {
            s.push_str(&format!(
                "- [{}]{} {}\n",
                q.id,
                if q.blocking { "（阻塞）" } else { "" },
                q.text
            ));
        }
    }
    if !m.results.is_empty() {
        s.push_str("\n## 结果\n");
        for r in &m.results {
            s.push_str(&format!(
                "- {}：{}（证据：{}）\n",
                r.item, r.status, r.evidence
            ));
        }
    }
    if !m.needs_user.is_empty() {
        s.push_str("\n## 需要用户决定\n");
        for u in &m.needs_user {
            s.push_str(&format!("- {u}\n"));
        }
    }
    s
}

fn retryable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ResourceBusy
    ) || matches!(e.raw_os_error(), Some(1 | 13 | 16)) // EPERM, EACCES, EBUSY
}

/// Temp file + rename, retrying "in use" failures within the budget, then failing loudly.
fn write_file(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("no parent")?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("msg")
    ));
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    let start = std::time::Instant::now();
    let mut wait = Duration::from_millis(20);
    loop {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if retryable(&e) && start.elapsed() < RENAME_BUDGET => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(250));
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e).with_context(|| {
                    format!(
                        "renaming into {} (gave up after {:?})",
                        path.display(),
                        start.elapsed()
                    )
                });
            }
        }
    }
}

impl Mirror {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, m: &Message) -> PathBuf {
        self.dir.join(&m.task).join(file_name(m))
    }

    /// Writes one message's mirror file (idempotent: same message, same bytes).
    pub fn write(&self, m: &Message) -> Result<()> {
        write_file(&self.path(m), &render(m))
    }

    /// Message files under the mirror that `known(task, n, from)` does not recognise. Covers
    /// both mirror files (`msg-<n>-<from>.md`) and old file-protocol files (`msg-<n>-dsh.json`).
    pub fn check(&self, known: impl Fn(&str, u32, &str) -> bool) -> Drift {
        let mut d = Drift::default();
        let Ok(tasks) = fs::read_dir(&self.dir) else {
            return d;
        };
        for t in tasks.flatten().filter(|e| e.path().is_dir()) {
            let task = t.file_name().to_string_lossy().to_string();
            let Ok(files) = fs::read_dir(t.path()) else {
                continue;
            };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let Some((n, from)) = parse_name(&name) else {
                    continue;
                };
                if !known(&task, n, &from) {
                    d.unknown.push(format!("{task}/{name}"));
                }
            }
        }
        d.unknown.sort();
        d
    }
}

/// `msg-<n>-<from>.(md|json)` → (n, from).
pub fn parse_name(name: &str) -> Option<(u32, String)> {
    let rest = name.strip_prefix("msg-")?;
    let stem = rest
        .strip_suffix(".md")
        .or_else(|| rest.strip_suffix(".json"))?;
    let (n, from) = stem.split_once('-')?;
    let n: u32 = n.parse().ok()?;
    crate::valid_id(from).then(|| (n, from.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Question, TaskMeta};

    fn msg(n: u32, from: &str) -> Message {
        Message {
            task: "t".into(),
            n,
            from: from.into(),
            kind: if n == 1 { Kind::Task } else { Kind::Question },
            body: "正文".into(),
            meta: (n == 1).then(|| TaskMeta {
                to: "dsh".into(),
                title: "标题".into(),
                priority: Default::default(),
                deadline: None,
                expr_id: None,
                parent: None,
            }),
            questions: if n == 1 {
                vec![]
            } else {
                vec![Question {
                    id: "q1".into(),
                    text: "哪个？".into(),
                    blocking: true,
                }]
            },
            results: vec![],
            needs_user: vec!["要不要重启".into()],
            outcome: None,
            judgement: None,
            reply_to: None,
            supersedes: None,
            client_msg_id: None,
            session_epoch: None,
            protocol: crate::PROTOCOL.into(),
            at: chrono::Utc::now(),
            imported: false,
            wake: None,
            phase: None,
        }
    }

    #[test]
    fn writes_readable_files_and_reports_files_it_does_not_know() {
        let d = tempfile::tempdir().unwrap();
        let mirror = Mirror::new(d.path());
        mirror.write(&msg(1, "claude")).unwrap();
        mirror.write(&msg(2, "dsh")).unwrap();
        mirror.write(&msg(2, "dsh")).unwrap(); // idempotent
        let text = fs::read_to_string(d.path().join("t/msg-1-claude.md")).unwrap();
        assert!(text.starts_with("taskId: t   n: 1   from: claude   kind: task"));
        assert!(
            text.contains("# 标题") && text.contains("只读镜像") && text.contains("要不要重启")
        );
        assert!(
            fs::read_to_string(d.path().join("t/msg-2-dsh.md"))
                .unwrap()
                .contains("[q1]（阻塞） 哪个？")
        );
        assert!(!d.path().join("t/.msg-1-claude.md.tmp").exists());

        let known = |t: &str, n: u32, from: &str| {
            t == "t" && ((n == 1 && from == "claude") || (n == 2 && from == "dsh"))
        };
        assert!(mirror.check(known).unknown.is_empty());
        // Someone hand-writes an old-protocol reply or edits the mirror into a new message.
        fs::write(d.path().join("t/msg-3-dsh.json"), "{}").unwrap();
        fs::write(d.path().join("t/notes.txt"), "ignored").unwrap();
        assert_eq!(mirror.check(known).unknown, ["t/msg-3-dsh.json"]);
        assert_eq!(parse_name("msg-12-claude.md"), Some((12, "claude".into())));
        assert_eq!(parse_name("msg-x-claude.md"), None);
        assert_eq!(parse_name("msg-1-../x.md"), None);
    }

    #[test]
    fn a_failed_write_is_an_error_not_silence() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("t"), "a file where the task dir should be").unwrap();
        assert!(Mirror::new(d.path()).write(&msg(1, "claude")).is_err());
    }
}
