//! Append-only storage: one JSON Lines file per task, `<root>/tasks/<id>.jsonl`.
//!
//! A message is durable before anyone is told it was accepted: the line is written and the file
//! fsynced (and, for a new task, the directory entry too) before `append` returns. A sender that
//! got "accepted" can rely on the receiver eventually seeing it — the failure mode the bridge
//! exists to remove is "sender sure it was delivered, receiver never saw it".
//!
//! Reading is forgiving: an over-long or unparsable line is skipped and counted, never allowed to
//! make the whole task unreadable.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::model::Message;
use crate::valid_id;

/// Longest accepted line (one message). Longer lines are refused on write and skipped on read.
pub const MAX_LINE: usize = 1 << 20;

pub struct Store {
    root: PathBuf,
}

/// What was read back from one task file.
#[derive(Debug, Default)]
pub struct Loaded {
    pub messages: Vec<Message>,
    /// Lines skipped because they were too long or not a valid message.
    pub bad_lines: usize,
}

impl Store {
    /// Opens (creating) the store with private permissions (0700).
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("tasks"))
            .with_context(|| format!("creating {}", root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for d in [root.clone(), root.join("tasks")] {
                fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, task: &str) -> Result<PathBuf> {
        anyhow::ensure!(valid_id(task), "invalid task id {task:?}");
        Ok(self.root.join("tasks").join(format!("{task}.jsonl")))
    }

    /// Task ids present on disk, sorted.
    pub fn task_ids(&self) -> Result<Vec<String>> {
        let mut ids: Vec<String> = fs::read_dir(self.root.join("tasks"))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".jsonl"))
                    .map(str::to_string)
            })
            .filter(|id| valid_id(id))
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// Appends one message durably. `create` must be true exactly for a task's first message,
    /// and fails if the task file already exists.
    pub fn append(&self, msg: &Message, create: bool) -> Result<()> {
        let path = self.path(&msg.task)?;
        let mut line = serde_json::to_string(msg)?;
        anyhow::ensure!(
            line.len() <= MAX_LINE,
            "message is {} bytes, limit {MAX_LINE}",
            line.len()
        );
        line.push('\n');
        let mut opts = OpenOptions::new();
        opts.append(true);
        if create {
            opts.create_new(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        f.write_all(line.as_bytes())?;
        f.sync_all()
            .with_context(|| format!("fsync {}", path.display()))?;
        if create {
            // The new directory entry must be durable too, or a crash can lose the whole file.
            File::open(self.root.join("tasks"))?.sync_all()?;
        }
        Ok(())
    }

    pub fn load(&self, task: &str) -> Result<Loaded> {
        let path = self.path(task)?;
        let f = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let mut out = Loaded::default();
        let mut r = BufReader::new(f);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            // Read at most MAX_LINE+1 bytes of a line, then drain the rest of an over-long one.
            let n = (&mut r)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            let complete = buf.last() == Some(&b'\n');
            if !complete && buf.len() > MAX_LINE {
                let mut skip = Vec::new();
                r.read_until(b'\n', &mut skip)?;
                out.bad_lines += 1;
                continue;
            }
            let text = String::from_utf8_lossy(&buf);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            match serde_json::from_str::<Message>(text) {
                Ok(m) if m.task == task => out.messages.push(m),
                _ => out.bad_lines += 1,
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    fn msg(task: &str, n: u32, body: &str) -> Message {
        Message {
            task: task.into(),
            n,
            from: "claude".into(),
            kind: if n == 1 { Kind::Task } else { Kind::Progress },
            body: body.into(),
            meta: None,
            questions: vec![],
            results: vec![],
            needs_user: vec![],
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
    fn appends_are_durable_private_and_create_is_exclusive() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("st")).unwrap();
        s.append(&msg("t1", 1, "a"), true).unwrap();
        assert!(
            s.append(&msg("t1", 1, "again"), true).is_err(),
            "second create must fail"
        );
        s.append(&msg("t1", 2, "b"), false).unwrap();
        assert!(
            s.append(&msg("nope", 2, "b"), false).is_err(),
            "append to a missing task must fail"
        );
        let l = s.load("t1").unwrap();
        assert_eq!(
            l.messages
                .iter()
                .map(|m| m.body.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(s.task_ids().unwrap(), ["t1"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&d.path().join("st/tasks/t1.jsonl")), 0o600);
            assert_eq!(mode(&d.path().join("st/tasks")), 0o700);
        }
        assert!(s.append(&msg("../x", 1, "a"), true).is_err());
    }

    #[test]
    fn bad_and_oversized_lines_are_skipped_and_counted() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        s.append(&msg("t", 1, "first"), true).unwrap();
        let path = d.path().join("tasks/t.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{not json").unwrap();
        writeln!(f, "{}", "x".repeat(MAX_LINE + 10)).unwrap();
        writeln!(
            f,
            "{}",
            serde_json::to_string(&msg("other", 9, "wrong task")).unwrap()
        )
        .unwrap();
        drop(f);
        s.append(&msg("t", 2, "after"), false).unwrap();
        let l = s.load("t").unwrap();
        assert_eq!(l.bad_lines, 3);
        assert_eq!(l.messages.iter().map(|m| m.n).collect::<Vec<_>>(), [1, 2]);
        assert!(
            s.append(&msg("t", 3, &"y".repeat(MAX_LINE)), false)
                .is_err(),
            "oversized write refused"
        );
    }
}
