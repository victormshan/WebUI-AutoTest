//! What only the user may decide, and the user's decisions.
//!
//! Pending items come from three places: a message's `needs_user` entries, a task paused for the
//! user (over a limit), and a task stalled for a day. A decision is the user's own words as
//! relayed by an agent, stored append-only with its SHA-256 and the relaying agent. Nothing can
//! edit or remove it; a second, different text for the same item is kept too and marks the item
//! as a conflict for the user to settle — an agent cannot quietly "correct" what the user said.
//!
//! A conflict is closed only by the user: a `settle` record follows the same append-only, hashed,
//! verbatim rules and lists the hashes it reconciles. `status` reports Decided while no text newer
//! than the last settle is left unsettled, and Conflict again as soon as one appears. The settle
//! text may repeat a text that is already on file, which is why the `record` text-dedupe is not
//! used for it. A binary that does not know `kind` counts the settle text as an ordinary decision,
//! so the item shows Conflict again after a downgrade: fail-safe, nothing is lost.

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
    /// `None` for an ordinary decision; `Some("settle")` for the user's settlement of a conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// For a settle record: the decision hashes the user settled (sorted, unique).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settles: Option<Vec<String>>,
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
    /// Decided because the user settled a conflict (so a UI can tell it apart).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub settled: bool,
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
            settled: false,
        }
    }
}

/// A settle the rules refuse (mapped to HTTP 400), as opposed to a storage failure.
#[derive(Debug)]
pub struct SettleRefused(pub String);

impl std::fmt::Display for SettleRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SettleRefused {}

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

    fn is_settle(d: &Decision) -> bool {
        d.kind.as_deref() == Some("settle")
    }

    /// Index of the last settle record for `item`, if any.
    fn last_settle(&self, item: &str) -> Option<usize> {
        self.list
            .iter()
            .rposition(|d| d.item == item && Self::is_settle(d))
    }

    /// The texts that the last settle record does not cover (all texts when there is none).
    fn unsettled_hashes(&self, item: &str) -> BTreeSet<String> {
        let Some(last) = self.last_settle(item) else {
            return self
                .list
                .iter()
                .filter(|d| d.item == item && !Self::is_settle(d))
                .map(|d| d.sha256.clone())
                .collect();
        };
        // The settle's own words count as covered: repeating them as a normal decision is
        // the user restating the settlement, not a new disagreement.
        let mut settled: BTreeSet<&str> = self.list[last]
            .settles
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(String::as_str)
            .collect();
        settled.insert(self.list[last].sha256.as_str());
        self.list[last + 1..]
            .iter()
            .filter(|d| d.item == item && !Self::is_settle(d))
            .map(|d| d.sha256.clone())
            .filter(|h| !settled.contains(h.as_str()))
            .collect()
    }

    /// The hashes the user would settle right now (empty unless the item is in conflict).
    pub fn conflict_set(&self, item: &str) -> BTreeSet<String> {
        if self.status(item) == ItemStatus::Conflict {
            self.unsettled_hashes(item)
        } else {
            BTreeSet::new()
        }
    }

    /// Whether the item is Decided *because of* a settle record.
    pub fn is_settled(&self, item: &str) -> bool {
        self.last_settle(item).is_some() && self.status(item) == ItemStatus::Decided
    }

    pub fn status(&self, item: &str) -> ItemStatus {
        if self.last_settle(item).is_none() {
            let hashes: BTreeSet<&str> = self
                .list
                .iter()
                .filter(|d| d.item == item && !Self::is_settle(d))
                .map(|d| d.sha256.as_str())
                .collect();
            return match hashes.len() {
                0 => ItemStatus::Open,
                1 => ItemStatus::Decided,
                _ => ItemStatus::Conflict,
            };
        }
        // A settle closes the conflict; only a text it does not cover reopens it.
        if self.unsettled_hashes(item).is_empty() {
            ItemStatus::Decided
        } else {
            ItemStatus::Conflict
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
            kind: None,
            settles: None,
        };
        if let Some(same) = self
            .list
            .iter()
            .find(|x| x.item == item && !Self::is_settle(x) && x.sha256 == d.sha256)
        {
            return Ok((self.status(item), same.clone()));
        }
        self.append(&d)?;
        Ok((self.status(item), d))
    }

    /// Closes a conflict with the user's own words (append-only and hashed like any decision).
    ///
    /// `expect` is the set the caller believes it is settling: when given it must equal the
    /// conflict set right now, so a stale list cannot settle a later conflict.
    pub fn record_settle(
        &mut self,
        item: &str,
        verbatim: &str,
        by: &str,
        form: Option<&str>,
        expect: Option<&[String]>,
    ) -> Result<(ItemStatus, Decision)> {
        let verbatim = verbatim.trim();
        let refuse = |why: String| anyhow::Error::new(SettleRefused(why));
        if verbatim.is_empty() {
            return Err(refuse(
                "a settle is the user's own words; it cannot be empty".into(),
            ));
        }
        if form != Some("verbatim") {
            return Err(refuse(
                "a settle needs form=verbatim (the user's own words)".into(),
            ));
        }
        let hash = hex(&sha256(verbatim));
        if self.status(item) != ItemStatus::Conflict {
            // Retrying the same settlement is a no-op (a lost response must not become a
            // 400); a different text on a non-conflicted item is refused.
            if let Some(last) = self.last_settle(item)
                && self.list[last].sha256 == hash
            {
                return Ok((self.status(item), self.list[last].clone()));
            }
            return Err(refuse(format!(
                "item {item:?} is not in conflict; nothing to settle"
            )));
        }
        let settles: Vec<String> = self.conflict_set(item).into_iter().collect();
        if settles.is_empty() {
            return Err(refuse("no conflicting texts to settle".into()));
        }
        if let Some(expected) = expect {
            let want: BTreeSet<&str> = settles.iter().map(String::as_str).collect();
            let got: BTreeSet<&str> = expected.iter().map(String::as_str).collect();
            if got != want {
                return Err(refuse(
                    "settles does not match the current conflict set".into(),
                ));
            }
        }
        let d = Decision {
            item: item.into(),
            verbatim: verbatim.into(),
            sha256: hash.clone(),
            relayed_by: by.into(),
            at: Utc::now(),
            form: Some("verbatim".into()),
            kind: Some("settle".into()),
            settles: Some(settles.clone()),
        };
        // Dedupe on the settlement, not on the text: the user's words often repeat a text
        // that is already on file, which `record` would treat as a no-op.
        if let Some(same) = self.list.iter().find(|x| {
            x.item == item
                && Self::is_settle(x)
                && x.settles.as_ref() == Some(&settles)
                && x.sha256 == d.sha256
        }) {
            return Ok((self.status(item), same.clone()));
        }
        self.append(&d)?;
        Ok((self.status(item), d))
    }

    fn append(&mut self, d: &Decision) -> Result<()> {
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
        f.write_all(format!("{}\n", serde_json::to_string(d)?).as_bytes())?;
        f.sync_all()?;
        if new_file && let Some(dir) = self.path.parent() {
            fs::File::open(dir)?.sync_all()?;
        }
        self.list.push(d.clone());
        Ok(())
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
        p.settled = decisions.is_settled(&p.item);
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
                        Some(crate::model::Relay::Codex) => "codex".into(),
                        Some(crate::model::Relay::Opencode) => "opencode".into(),
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
    #[test]
    fn settle_needs_a_conflict_verbatim_words_and_the_current_set() {
        let d = tempfile::tempdir().unwrap();
        let mut ds = Decisions::open(d.path()).unwrap();
        assert!(
            ds.record_settle("t#1#1", "x", "claude", Some("verbatim"), None)
                .is_err(),
            "an open item cannot be settled"
        );
        ds.record("t#1#1", "保留 Windows", "claude", None).unwrap();
        assert!(
            ds.record_settle("t#1#1", "x", "claude", Some("verbatim"), None)
                .is_err(),
            "a decided item cannot be settled"
        );
        ds.record("t#1#1", "保留 WSL", "dsh", None).unwrap();
        assert_eq!(ds.status("t#1#1"), ItemStatus::Conflict);
        assert!(
            ds.record_settle("t#1#1", "x", "claude", Some("paraphrase"), None)
                .is_err(),
            "a paraphrase is not the user's own words"
        );
        let stale = vec!["00".to_string()];
        assert!(
            ds.record_settle("t#1#1", "x", "claude", Some("verbatim"), Some(&stale))
                .is_err(),
            "a stale set is refused"
        );
        let current: Vec<String> = ds.conflict_set("t#1#1").into_iter().collect();
        assert_eq!(current.len(), 2);
        // The settle text repeats a text that is already on file and is still recorded.
        let (status, first) = ds
            .record_settle(
                "t#1#1",
                "保留 Windows",
                "claude",
                Some("verbatim"),
                Some(&current),
            )
            .unwrap();
        assert_eq!(status, ItemStatus::Decided);
        assert_eq!(
            (first.kind.as_deref(), first.settles.as_ref().map(Vec::len)),
            (Some("settle"), Some(2))
        );
        assert_eq!(ds.status("t#1#1"), ItemStatus::Decided);
        assert!(ds.is_settled("t#1#1"));
        assert_eq!(ds.for_item("t#1#1").len(), 3, "the settle joins the texts");
        // A text newer than the settle reopens the conflict; the old set cannot settle it.
        ds.record("t#1#1", "先别重启", "codex", None).unwrap();
        assert_eq!(ds.status("t#1#1"), ItemStatus::Conflict);
        assert!(!ds.is_settled("t#1#1"));
        let old = first.settles.clone().unwrap();
        assert!(
            ds.record_settle("t#1#1", "先别重启", "codex", Some("verbatim"), Some(&old))
                .is_err(),
            "the old, larger set is stale now"
        );
        assert_eq!(ds.conflict_set("t#1#1").len(), 1);
        let (status, _) = ds
            .record_settle("t#1#1", "先别重启", "codex", Some("verbatim"), None)
            .unwrap();
        assert_eq!(status, ItemStatus::Decided);
        // Settling the same fact twice with the same words is a no-op.
        let count = ds.for_item("t#1#1").len();
        ds.record_settle("t#1#1", "先别重启", "codex", Some("verbatim"), None)
            .unwrap();
        assert_eq!(ds.for_item("t#1#1").len(), count);
    }

    #[test]
    fn settle_records_survive_a_reload_and_stay_append_only() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("decisions.jsonl");
        let mut ds = Decisions::open(d.path()).unwrap();
        ds.record("t#1#1", "a", "claude", None).unwrap();
        ds.record("t#1#1", "b", "dsh", None).unwrap();
        let before = fs::read_to_string(&p).unwrap().lines().count();
        ds.record_settle("t#1#1", "a", "claude", Some("verbatim"), None)
            .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert_eq!(
            text.lines().count(),
            before + 1,
            "one appended line, nothing rewritten"
        );
        drop(ds);
        let ds = Decisions::open(d.path()).unwrap();
        assert_eq!((ds.bad_lines, ds.status("t#1#1")), (0, ItemStatus::Decided));
        assert!(ds.is_settled("t#1#1"));
        assert_eq!(ds.for_item("t#1#1").len(), 3);
    }
    #[test]
    fn restating_the_settle_words_keeps_the_item_decided() {
        let d = tempfile::tempdir().unwrap();
        let mut ds = Decisions::open(d.path()).unwrap();
        ds.record("t#1#1", "a", "claude", None).unwrap();
        ds.record("t#1#1", "b", "dsh", None).unwrap();
        // The settle words are new (not one of the conflicting texts).
        ds.record_settle("t#1#1", "用新的话了结", "claude", Some("verbatim"), None)
            .unwrap();
        assert_eq!(ds.status("t#1#1"), ItemStatus::Decided);
        // Saying the same words again as an ordinary decision is not a new disagreement.
        ds.record("t#1#1", "用新的话了结", "dsh", None).unwrap();
        assert_eq!(ds.status("t#1#1"), ItemStatus::Decided);
        assert!(ds.is_settled("t#1#1"));
        // A different text is.
        ds.record("t#1#1", "改成别的", "dsh", None).unwrap();
        assert_eq!(ds.status("t#1#1"), ItemStatus::Conflict);
    }
}
