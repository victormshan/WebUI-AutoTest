//! Import of tasks written in the old file protocol (`claude-bridge/tasks/<id>/msg-<n>-(claude.md|dsh.json)`).
//!
//! Only finished tasks (last Claude message `kind: close`) are imported, as a closed archive:
//! an open file-protocol task must be finished where it started, so there is never a task that
//! lives half in files and half in the service (design b6). Import is idempotent: the key is
//! `(taskId, n, from)`; importing the same directory again changes nothing.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::mirror::parse_name;
use crate::model::{Kind, Message, Outcome, TaskMeta};

fn claude_kind(k: &str) -> Option<Kind> {
    Some(match k {
        "task" => Kind::Task,
        "answer" => Kind::Answer,
        "verdict" => Kind::Verdict,
        "close" => Kind::Close,
        "cancel" => Kind::Cancel,
        _ => return None,
    })
}

/// Reads one file-protocol task directory into messages (sorted by n), or explains why not.
pub fn read_task(dir: &Path) -> Result<Vec<Message>> {
    let task = dir
        .file_name()
        .and_then(|n| n.to_str())
        .context("bad dir name")?
        .to_string();
    anyhow::ensure!(crate::valid_id(&task), "invalid task id {task:?}");
    let mut out: Vec<Message> = Vec::new();
    for e in fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some((n, from)) = parse_name(&name) else {
            continue;
        };
        let path = e.path();
        let mtime: DateTime<Utc> = e
            .metadata()
            .and_then(|m| m.modified())
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let raw = raw.trim_start_matches('\u{feff}').to_string();
        let mut m = Message {
            task: task.clone(),
            n,
            from: from.clone(),
            kind: Kind::Progress,
            body: String::new(),
            meta: None,
            questions: vec![],
            results: vec![],
            needs_user: vec![],
            outcome: None,
            judgement: None,
            reply_to: None,
            supersedes: None,
            client_msg_id: Some(format!("import:{task}:{n}:{from}")),
            session_epoch: None,
            protocol: "0".into(),
            at: mtime,
            imported: true,
            wake: None,
            phase: None,
        };
        if name.ends_with(".md") {
            let (header, body) = raw.split_once('\n').unwrap_or((raw.as_str(), ""));
            let field = |k: &str| {
                header
                    .split_whitespace()
                    .skip_while(|w| *w != format!("{k}:"))
                    .nth(1)
                    .map(str::to_string)
            };
            let kind = field("kind").unwrap_or_default();
            m.kind = claude_kind(&kind)
                .with_context(|| format!("{}: unknown kind {kind:?}", path.display()))?;
            m.reply_to = field("replyTo").and_then(|r| r.parse().ok());
            m.body = body.trim_start_matches('\n').to_string();
            if m.kind == Kind::Task {
                let title = m
                    .body
                    .lines()
                    .find_map(|l| l.strip_prefix("# "))
                    .unwrap_or(&task)
                    .trim()
                    .to_string();
                m.meta = Some(TaskMeta {
                    to: "dsh".into(),
                    title,
                    priority: Default::default(),
                    deadline: None,
                    expr_id: None,
                    parent: None,
                });
            }
        } else {
            let v: Value = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            let status = v["status"].as_str().unwrap_or("progress");
            (m.kind, m.outcome) = match status {
                "ack" => (Kind::Ack, None),
                "question" => (Kind::Question, None),
                "done" => (Kind::Result, Some(Outcome::Done)),
                "blocked" => (Kind::Result, Some(Outcome::Blocked)),
                "rejected" => (Kind::Result, Some(Outcome::Rejected)),
                _ => (Kind::Progress, None),
            };
            m.reply_to = v["replyTo"].as_u64().map(|r| r as u32);
            m.supersedes = v["supersedes"].as_u64().map(|r| r as u32);
            if let Some(at) = v["at"]
                .as_str()
                .and_then(|a| DateTime::parse_from_rfc3339(a).ok())
            {
                m.at = at.with_timezone(&Utc);
            }
            m.needs_user = v["needsUser"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(crate::model::NeedsUser::from))
                        .collect()
                })
                .unwrap_or_default();
            // The whole original JSON is kept as the body: nothing of the record is lost.
            m.body = format!(
                "{}\n\n```json\n{}\n```",
                v["summary"].as_str().unwrap_or(""),
                serde_json::to_string_pretty(&v)?
            );
        }
        out.push(m);
    }
    out.sort_by_key(|m| m.n);
    for w in out.windows(2) {
        if w[0].n == w[1].n {
            bail!(
                "{task}: two messages numbered {} ({} and {})",
                w[0].n,
                w[0].from,
                w[1].from
            );
        }
    }
    match (out.first(), out.last()) {
        (Some(f), Some(l))
            if f.n == 1
                && f.kind == Kind::Task
                && f.from == "claude"
                && l.from == "claude"
                && l.kind == Kind::Close =>
        {
            Ok(out)
        }
        (Some(_), Some(l)) => bail!(
            "{task}: not finished (last message is {} {}): finish it in the file protocol first",
            l.from,
            l.kind
        ),
        _ => bail!("{task}: no messages"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Bridge;
    use crate::model::State;

    fn write(dir: &Path, name: &str, text: &str) {
        fs::write(dir.join(name), text).unwrap();
    }

    fn finished(root: &Path, id: &str) -> std::path::PathBuf {
        let d = root.join(id);
        fs::create_dir_all(&d).unwrap();
        write(
            &d,
            "msg-1-claude.md",
            &format!(
                "taskId: {id}   n: 1   from: claude   replyTo: -   kind: task   protocol: 0\n\n# 任务标题\n\n正文"
            ),
        );
        write(
            &d,
            "msg-2-dsh.json",
            r#"{"taskId":"x","n":2,"from":"dsh","status":"ack","summary":"收到","needsUser":["要用户定"],"at":"2026-10-05T12:00:00Z"}"#,
        );
        write(
            &d,
            "msg-3-dsh.json",
            r#"{"n":3,"status":"done","summary":"做完了","at":"2026-10-05T12:05:00Z"}"#,
        );
        write(
            &d,
            "msg-4-claude.md",
            &format!(
                "taskId: {id}   n: 4   from: claude   replyTo: 3   kind: close   protocol: 0\n\n结案"
            ),
        );
        d
    }

    #[test]
    fn finished_tasks_import_as_closed_archives_once() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("files");
        finished(&src, "done-task");
        let msgs = read_task(&src.join("done-task")).unwrap();
        assert_eq!(
            msgs.iter().map(|m| (m.n, m.kind)).collect::<Vec<_>>(),
            [
                (1, Kind::Task),
                (2, Kind::Ack),
                (3, Kind::Result),
                (4, Kind::Close)
            ]
        );
        assert_eq!(msgs[0].meta.as_ref().unwrap().title, "任务标题");
        assert_eq!(
            (msgs[2].outcome, msgs[3].reply_to),
            (Some(Outcome::Done), Some(3))
        );
        assert!(
            msgs[1].body.contains("收到") && msgs[1].body.contains("\"needsUser\""),
            "original JSON kept"
        );
        assert_eq!(msgs[1].needs_user[0].text, "要用户定");

        let state = d.path().join("state");
        let mut b = Bridge::open(&state, &["claude", "dsh"]).unwrap();
        assert!(b.import_task(msgs.clone()).unwrap());
        assert!(
            !b.import_task(read_task(&src.join("done-task")).unwrap())
                .unwrap(),
            "re-import is a no-op"
        );
        let t = b.task("done-task").unwrap().clone();
        assert_eq!((t.state, t.messages, t.last_n), (State::Closed, 4, 4));
        assert!(b.knows("done-task", 2, "dsh") && !b.knows("done-task", 5, "dsh"));
        drop(b);
        let b = Bridge::open(&state, &["claude", "dsh"]).unwrap();
        assert_eq!(b.task("done-task"), Some(&t), "archive survives a restart");
        assert_eq!(b.bad_lines, 0);
    }

    #[test]
    fn open_or_inconsistent_tasks_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let open = finished(d.path(), "still-open");
        fs::remove_file(open.join("msg-4-claude.md")).unwrap();
        assert!(
            read_task(&open)
                .unwrap_err()
                .to_string()
                .contains("not finished")
        );
        let dup = finished(d.path(), "dup");
        write(
            &dup,
            "msg-3-claude.md",
            "taskId: dup   n: 3   from: claude   kind: answer   protocol: 0\n\nx",
        );
        assert!(
            read_task(&dup)
                .unwrap_err()
                .to_string()
                .contains("two messages numbered 3")
        );
    }
}
