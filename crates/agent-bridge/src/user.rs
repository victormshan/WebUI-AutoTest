//! What only the user may decide, and the user's decisions.
//!
//! Pending items come from three places: a message's `needs_user` entries, a task paused for the
//! user (over a limit), and a task stalled for a day. A decision is the user's own words as
//! relayed by an agent, stored append-only with its SHA-256 and the relaying agent. Nothing can
//! edit or remove it; a second, different text for the same item is kept too and marks the item
//! as a conflict for the user to settle — an agent cannot quietly "correct" what the user said.

use std::collections::{BTreeMap, BTreeSet};
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
    /// `verbatim` (the user's own words) or `paraphrase` (a retelling): a paraphrase must never
    /// pass as the user's words (P3). Older records carry none and were recorded as verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<String>,
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
    /// Who is to ask the user (Q5): the relay named in the message, else whoever raised it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asked_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asked_at: Option<DateTime<Utc>>,
    /// Undecided and nobody has asked the user for longer than allowed (Q5).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub overdue: bool,
}

impl PendingItem {
    pub fn new(item: String, task: &str, source: &'static str, text: String) -> Self {
        PendingItem {
            item,
            task: task.into(),
            source,
            text,
            status: ItemStatus::Open,
            decisions: vec![],
            relay: None,
            asked_by: None,
            asked_at: None,
            overdue: false,
        }
    }
}

/// Who asked the user about which item (`asked.jsonl`, append-only).
pub fn load_asked(root: &Path) -> Result<BTreeMap<String, (String, DateTime<Utc>)>> {
    let mut out = BTreeMap::new();
    if let Ok(f) = File::open(root.join("asked.jsonl")) {
        for line in BufReader::new(f).lines().map_while(|l| l.ok()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && let (Some(item), Some(by), Some(at)) =
                    (v["item"].as_str(), v["by"].as_str(), v["at"].as_str())
                && let Ok(at) = DateTime::parse_from_rfc3339(at)
            {
                out.insert(item.to_string(), (by.to_string(), at.with_timezone(&Utc)));
            }
        }
    }
    Ok(out)
}

pub fn record_asked(root: &Path, item: &str, by: &str, at: DateTime<Utc>) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(root.join("asked.jsonl"))?;
    f.write_all(
        format!(
            "{}\n",
            serde_json::json!({ "item": item, "by": by, "at": at })
        )
        .as_bytes(),
    )?;
    f.sync_all()?;
    Ok(())
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

    pub fn items(&self) -> impl Iterator<Item = &str> {
        self.list.iter().map(|d| d.item.as_str())
    }

    /// Next free `<task>#direct#<k>`.
    pub fn next_direct(&self, task: &str) -> String {
        let k = self
            .list
            .iter()
            .filter(|d| d.item.starts_with(&format!("{task}#direct#")))
            .map(|d| d.item.clone())
            .collect::<BTreeSet<_>>()
            .len()
            + 1;
        format!("{task}#direct#{k}")
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
        form: Option<&str>,
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
            form: form.map(str::to_string),
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

/// Inputs for `pending` beyond the store itself.
pub struct PendingCtx<'a> {
    pub stalled: &'a BTreeSet<String>,
    pub asked: &'a BTreeMap<String, (String, DateTime<Utc>)>,
    pub now: DateTime<Utc>,
    pub ask_overdue_after: chrono::Duration,
}

/// Everything waiting for the user (decided items too when `all`).
pub fn pending(
    bridge: &Bridge,
    decisions: &Decisions,
    ctx: &PendingCtx,
    all: bool,
) -> Vec<PendingItem> {
    let mut out = Vec::new();
    let mut push = |mut p: PendingItem| {
        p.status = decisions.status(&p.item);
        p.decisions = decisions.for_item(&p.item);
        if let Some((by, at)) = ctx.asked.get(&p.item) {
            p.asked_by = Some(by.clone());
            p.asked_at = Some(*at);
        }
        if all || p.status != ItemStatus::Decided {
            out.push(p);
        }
    };
    for t in bridge.list(None) {
        if let Ok(ms) = bridge.messages(&t.id) {
            // Archived file-protocol messages are history: their items were handled back then.
            for m in ms.iter().filter(|m| !m.imported) {
                for (i, nu) in m.needs_user.iter().enumerate() {
                    let mut p = PendingItem::new(
                        format!("{}#{}#{}", t.id, m.n, i + 1),
                        &t.id,
                        "needs_user",
                        format!("[{}] {}", m.from, nu.text),
                    );
                    p.relay = Some(match nu.relay {
                        Some(crate::model::Relay::Claude) => "claude".into(),
                        Some(crate::model::Relay::Dsh) => "dsh".into(),
                        Some(crate::model::Relay::Either) => "either".into(),
                        None => m.from.clone(),
                    });
                    p.overdue = !ctx.asked.contains_key(&p.item)
                        && decisions.status(&p.item) == ItemStatus::Open
                        && ctx.now - m.at >= ctx.ask_overdue_after;
                    push(p);
                }
            }
        }
        if t.state == State::PausedForUser {
            push(PendingItem::new(
                format!("{}#paused#{}", t.id, t.last_n),
                &t.id,
                "paused",
                t.paused_reason.clone().unwrap_or_default(),
            ));
        }
        if ctx.stalled.contains(&t.id) && !t.state.terminal() {
            push(PendingItem::new(
                format!("{}#stalled#{}", t.id, t.last_n),
                &t.id,
                "stalled",
                format!("任务 {} 超过一天没有进展（状态 {}）", t.id, t.state),
            ));
        }
    }
    // Decisions the user gave directly to one side, recorded against a task (P3).
    let direct: BTreeSet<String> = decisions
        .items()
        .filter(|i| i.contains("#direct#"))
        .map(str::to_string)
        .collect();
    for item in direct {
        let task = item
            .split("#direct#")
            .next()
            .unwrap_or_default()
            .to_string();
        push(PendingItem::new(
            item,
            &task,
            "direct",
            "用户直接给出的决定".into(),
        ));
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
        assert!(ds.record("t#1#1", "  ", "claude", None).is_err());
        let (s, first) = ds.record("t#1#1", "保留 Windows", "claude", None).unwrap();
        assert_eq!(
            (s, first.relayed_by.as_str()),
            (ItemStatus::Decided, "claude")
        );
        assert_eq!(
            ds.record("t#1#1", "保留 Windows", "dsh", None).unwrap().0,
            ItemStatus::Decided,
            "same words: no-op"
        );
        assert_eq!(
            ds.record("t#1#1", "保留 WSL", "dsh", None).unwrap().0,
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
