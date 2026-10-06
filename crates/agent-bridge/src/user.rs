//! What only the user may decide, and the user's decisions.
//!
//! Pending items come from three places: a message's `needs_user` entries, a task paused for the
//! user (over a limit), and a task stalled for a day. A decision is the user's own words as
//! relayed by an agent, stored append-only with its SHA-256 and the relaying agent. Nothing can
//! edit or remove it; a second, different text for the same item is kept too and marks the item
//! as a conflict for the user to settle — an agent cannot quietly "correct" what the user said.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::bridge::Bridge;
use crate::model::State;
use crate::service::{hex, sha256};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub item: String,
    pub verbatim: String,
    pub sha256: String,
    pub relayed_by: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Open,
    Decided,
    /// Two different texts were recorded for the same item.
    Conflict,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingItem {
    pub item: String,
    pub task: String,
    /// needs_user | paused | stalled
    pub source: &'static str,
    pub text: String,
    pub status: ItemStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<Decision>,
}

pub struct Decisions {
    path: PathBuf,
    list: Vec<Decision>,
    pub bad_lines: usize,
}

impl Decisions {
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join("decisions.jsonl");
        let mut list = Vec::new();
        let mut bad_lines = 0;
        if let Ok(f) = File::open(&path) {
            for line in BufReader::new(f).lines() {
                let line = line.unwrap_or_default();
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Decision>(&line) {
                    Ok(d) if hex(&sha256(&d.verbatim)) == d.sha256 => list.push(d),
                    _ => bad_lines += 1,
                }
            }
        }
        Ok(Self {
            path,
            list,
            bad_lines,
        })
    }

    pub fn for_item(&self, item: &str) -> Vec<Decision> {
        self.list
            .iter()
            .filter(|d| d.item == item)
            .cloned()
            .collect()
    }

    pub fn status(&self, item: &str) -> ItemStatus {
        let hashes: BTreeSet<&str> = self
            .list
            .iter()
            .filter(|d| d.item == item)
            .map(|d| d.sha256.as_str())
            .collect();
        match hashes.len() {
            0 => ItemStatus::Open,
            1 => ItemStatus::Decided,
            _ => ItemStatus::Conflict,
        }
    }

    /// Appends a decision durably. The same text twice is a no-op.
    pub fn record(
        &mut self,
        item: &str,
        verbatim: &str,
        by: &str,
    ) -> Result<(ItemStatus, Decision)> {
        let verbatim = verbatim.trim();
        anyhow::ensure!(
            !verbatim.is_empty(),
            "a decision is the user's words; it cannot be empty"
        );
        let d = Decision {
            item: item.into(),
            verbatim: verbatim.into(),
            sha256: hex(&sha256(verbatim)),
            relayed_by: by.into(),
            at: Utc::now(),
        };
        if let Some(same) = self
            .list
            .iter()
            .find(|x| x.item == item && x.sha256 == d.sha256)
        {
            return Ok((self.status(item), same.clone()));
        }
        let mut opts = OpenOptions::new();
        opts.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let new_file = !self.path.exists();
        let mut f = opts
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        f.write_all(format!("{}\n", serde_json::to_string(&d)?).as_bytes())?;
        f.sync_all()?;
        if new_file && let Some(dir) = self.path.parent() {
            fs::File::open(dir)?.sync_all()?;
        }
        self.list.push(d.clone());
        Ok((self.status(item), d))
    }
}

/// Everything waiting for the user (decided items too when `all`).
pub fn pending(
    bridge: &Bridge,
    decisions: &Decisions,
    stalled: &BTreeSet<String>,
    all: bool,
) -> Vec<PendingItem> {
    let mut out = Vec::new();
    let mut push = |item: String, task: &str, source: &'static str, text: String| {
        let status = decisions.status(&item);
        if all || status != ItemStatus::Decided {
            out.push(PendingItem {
                decisions: decisions.for_item(&item),
                item,
                task: task.into(),
                source,
                text,
                status,
            });
        }
    };
    for t in bridge.list(None) {
        if let Ok(ms) = bridge.messages(&t.id) {
            // Archived file-protocol messages are history: their items were handled back then.
            for m in ms.iter().filter(|m| !m.imported) {
                for (i, text) in m.needs_user.iter().enumerate() {
                    push(
                        format!("{}#{}#{}", t.id, m.n, i + 1),
                        &t.id,
                        "needs_user",
                        format!("[{}] {}", m.from, text.text),
                    );
                }
            }
        }
        if t.state == State::PausedForUser {
            push(
                format!("{}#paused#{}", t.id, t.last_n),
                &t.id,
                "paused",
                t.paused_reason.clone().unwrap_or_default(),
            );
        }
        if stalled.contains(&t.id) && !t.state.terminal() {
            push(
                format!("{}#stalled#{}", t.id, t.last_n),
                &t.id,
                "stalled",
                format!("任务 {} 超过一天没有进展（状态 {}）", t.id, t.state),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_are_append_only_hashed_and_conflicts_are_kept() {
        let d = tempfile::tempdir().unwrap();
        let mut ds = Decisions::open(d.path()).unwrap();
        assert_eq!(ds.status("t#1#1"), ItemStatus::Open);
        assert!(ds.record("t#1#1", "  ", "claude").is_err());
        let (s, first) = ds.record("t#1#1", "保留 Windows", "claude").unwrap();
        assert_eq!(
            (s, first.relayed_by.as_str()),
            (ItemStatus::Decided, "claude")
        );
        assert_eq!(
            ds.record("t#1#1", "保留 Windows", "dsh").unwrap().0,
            ItemStatus::Decided,
            "same words: no-op"
        );
        assert_eq!(
            ds.record("t#1#1", "保留 WSL", "dsh").unwrap().0,
            ItemStatus::Conflict
        );
        drop(ds);
        let ds = Decisions::open(d.path()).unwrap();
        assert_eq!(
            (ds.status("t#1#1"), ds.for_item("t#1#1").len()),
            (ItemStatus::Conflict, 2),
            "both texts kept"
        );
        // A line whose text no longer matches its hash is not trusted.
        let p = d.path().join("decisions.jsonl");
        let s = fs::read_to_string(&p)
            .unwrap()
            .replace("保留 WSL", "随便改");
        fs::write(&p, s).unwrap();
        let ds = Decisions::open(d.path()).unwrap();
        assert_eq!((ds.bad_lines, ds.status("t#1#1")), (1, ItemStatus::Decided));
    }
}
