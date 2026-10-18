//! The local milestone log and its checkpoint (plans/M2-design.md §3.5, §3.7).
//!
//! `state/milestones/log.jsonl` holds the record chain, one canonical record per line, only ever
//! appended. `checkpoint.json` holds the last archived position. Every write goes through
//! [`Ledger::append`] under one cross-process lock (`log.lock`): read and verify the whole chain,
//! append, fsync. `archived` records are appended *before* the checkpoint file is replaced, so
//! after a crash the log can only be ahead of the checkpoint file, never behind.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::record::{Draft, Record, parse_log, seal, verify_chain};
use crate::attest::Keypair;

/// The last archived position: every record up to `seq` is known to be on the remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub seq: u64,
    pub hash: String,
}

/// A verified view of the log, taken under the lock.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub records: Vec<Record>,
    /// Archived up to this seq (0: nothing archived yet).
    pub checkpoint: u64,
    /// Set when the log and the checkpoint cannot be reconciled locally: release stops for every
    /// phase until the remote proves the checkpoint or the user decides (§3.7).
    pub stop: Option<String>,
    /// The checkpoint file was rebuilt from the log's last `archived` record (normal crash gap).
    pub rebuilt: bool,
}

pub struct Ledger {
    dir: PathBuf,
    repo: String,
    key: VerifyingKey,
}

impl Ledger {
    pub fn new(dir: impl Into<PathBuf>, repo: &str, key: VerifyingKey) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            repo: repo.to_string(),
            key,
        })
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join("log.jsonl")
    }

    fn checkpoint_path(&self) -> PathBuf {
        self.dir.join("checkpoint.json")
    }

    fn lock(&self) -> Result<File> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("log.lock"))?;
        f.lock().context("locking the milestone log")?;
        Ok(f)
    }

    /// A verified snapshot (reconciling the checkpoint as §3.7 says). An unreadable, cut-off
    /// or tampered log is an error: nothing is derived from it.
    pub fn load(&self) -> Result<Snapshot> {
        let _lock = self.lock()?;
        self.load_locked()
    }

    fn load_locked(&self) -> Result<Snapshot> {
        let text = match fs::read_to_string(self.log_path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).context("reading the milestone log"),
        };
        // A cut-off last line is never truncated away: it is an error for the user (§3.7).
        let records = parse_log(&text)?;
        verify_chain(&records, &self.key, &self.repo)?;
        let mut snap = Snapshot {
            records,
            checkpoint: 0,
            stop: None,
            rebuilt: false,
        };
        self.reconcile(&mut snap)?;
        Ok(snap)
    }

    /// Reconciles the checkpoint file with the log's last `archived` record.
    fn reconcile(&self, snap: &mut Snapshot) -> Result<()> {
        // A temporary file left by a crash before its rename is not a checkpoint.
        let _ = fs::remove_file(self.dir.join("checkpoint.json.tmp"));
        let from_log = match last_archived(&snap.records) {
            Ok(c) => c,
            Err(why) => {
                snap.stop = Some(why);
                return Ok(());
            }
        };
        let file: Option<Checkpoint> = match fs::read(self.checkpoint_path()) {
            Ok(b) => match serde_json::from_slice(&b) {
                Ok(c) => Some(c),
                Err(e) => {
                    snap.stop = Some(format!(
                        "checkpoint file unreadable ({e}): check the remote"
                    ));
                    return Ok(());
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("reading the checkpoint"),
        };
        let at = |c: &Checkpoint| {
            (c.seq >= 1)
                .then(|| snap.records.get(c.seq as usize - 1))
                .flatten()
                .map(|r| r.hash.clone())
        };
        let ahead = |f: &Checkpoint, l: Option<&Checkpoint>| {
            format!(
                "checkpoint file ({}, {}) is ahead of or conflicts with the log (last archived {}): \
                 check the remote, never lower a checkpoint",
                f.seq,
                f.hash,
                l.map_or("none".to_string(), |l| format!("{} {}", l.seq, l.hash))
            )
        };
        match (from_log, file) {
            (None, None) => {}
            (Some(l), Some(f)) if l == f => snap.checkpoint = l.seq,
            // The normal crash gap: the archived record is written, the file not yet updated
            // (or not yet created). Rebuild only from a position the log itself proves.
            (Some(l), None) => {
                self.write_checkpoint(&l)?;
                snap.checkpoint = l.seq;
                snap.rebuilt = true;
            }
            (Some(l), Some(f)) if f.seq < l.seq => {
                if at(&f).as_deref() != Some(f.hash.as_str()) {
                    snap.stop = Some(format!(
                        "checkpoint file names record {} with hash {} that is not in the log: check the remote",
                        f.seq, f.hash
                    ));
                } else {
                    self.write_checkpoint(&l)?;
                    snap.checkpoint = l.seq;
                    snap.rebuilt = true;
                }
            }
            // Ahead of the log, or the same position with another hash, or a checkpoint while the
            // log has no archived record: never lowered silently, or a rolled-back remote could go
            // unnoticed (§3.7).
            (l, Some(f)) => snap.stop = Some(ahead(&f, l.as_ref())),
        }
        Ok(())
    }

    fn write_checkpoint(&self, c: &Checkpoint) -> Result<()> {
        let tmp = self.dir.join("checkpoint.json.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(serde_json::to_string(c)?.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, self.checkpoint_path())?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    /// The one append path: under the lock, verify the whole chain, seal the record on its tail,
    /// write it and fsync. Refused while the log needs the user or the remote.
    pub fn append(&self, kp: &Keypair, at: &str, draft: Draft) -> Result<Record> {
        let _lock = self.lock()?;
        let snap = self.load_locked()?;
        if let Some(why) = &snap.stop {
            bail!("the milestone log is stopped: {why}");
        }
        self.append_locked(kp, at, &snap, draft)
    }

    fn append_locked(
        &self,
        kp: &Keypair,
        at: &str,
        snap: &Snapshot,
        draft: Draft,
    ) -> Result<Record> {
        let r = seal(kp, snap.records.last(), &self.repo, at, draft)?;
        let new = !self.log_path().exists();
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())?;
        f.write_all(format!("{}\n", r.to_line()?).as_bytes())?;
        f.sync_all()?;
        if new {
            File::open(&self.dir)?.sync_all()?;
        }
        Ok(r)
    }

    /// Records that the remote holds the chain up to `seq` (with `hash`): first the `archived`
    /// record, then the checkpoint file (§3.5, §3.7).
    pub fn archive(&self, kp: &Keypair, at: &str, seq: u64, hash: &str) -> Result<Record> {
        let _lock = self.lock()?;
        let snap = self.load_locked()?;
        if let Some(why) = &snap.stop {
            bail!("the milestone log is stopped: {why}");
        }
        let target = snap
            .records
            .get((seq as usize).wrapping_sub(1))
            .filter(|r| r.hash == hash)
            .with_context(|| format!("record {seq} with hash {hash} is not in the log"))?;
        if seq < snap.checkpoint {
            bail!(
                "archiving {seq} would lower the checkpoint {}",
                snap.checkpoint
            );
        }
        let r = self.append_locked(
            kp,
            at,
            &snap,
            Draft {
                kind: "archived".into(),
                phase: String::new(),
                body: json!({"seq": seq, "hash": target.hash}),
            },
        )?;
        self.write_checkpoint(&Checkpoint {
            seq,
            hash: hash.to_string(),
        })?;
        Ok(r)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// The position named by the last `archived` record, checked against the chain.
fn last_archived(records: &[Record]) -> Result<Option<Checkpoint>, String> {
    let Some(a) = records.iter().rev().find(|r| r.kind == "archived") else {
        return Ok(None);
    };
    let seq = a.body.get("seq").and_then(|v| v.as_u64());
    let hash = a.body.get("hash").and_then(|v| v.as_str());
    let (Some(seq), Some(hash)) = (seq, hash) else {
        return Err(format!("archived record {} is malformed", a.seq));
    };
    let ok = seq >= 1 && seq < a.seq && records[seq as usize - 1].hash == hash;
    if !ok {
        return Err(format!(
            "archived record {} names ({seq}, {hash}), which is not an earlier record of the chain",
            a.seq
        ));
    }
    Ok(Some(Checkpoint {
        seq,
        hash: hash.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kp() -> Keypair {
        Keypair::from_seed([9; 32])
    }

    fn ledger(dir: &Path) -> Ledger {
        Ledger::new(dir, "repo", kp().verifying_key()).unwrap()
    }

    fn note(l: &Ledger, i: u64) -> Record {
        l.append(
            &kp(),
            "t",
            Draft {
                kind: "note".into(),
                phase: "p".into(),
                body: json!({"i": i}),
            },
        )
        .unwrap()
    }

    #[test]
    fn appends_chain_and_survive_a_reload() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        let a = note(&l, 1);
        let b = note(&l, 2);
        assert_eq!((a.seq, b.seq, b.prev.clone()), (1, 2, a.hash.clone()));
        let s = ledger(d.path()).load().unwrap();
        assert_eq!(s.records, vec![a, b]);
        assert_eq!((s.checkpoint, s.stop.clone(), s.rebuilt), (0, None, false));
    }

    #[test]
    fn archiving_writes_the_record_first_then_the_checkpoint() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        let r = note(&l, 1);
        note(&l, 2);
        let a = l.archive(&kp(), "t", r.seq, &r.hash).unwrap();
        assert_eq!(a.kind, "archived");
        let s = l.load().unwrap();
        assert_eq!((s.checkpoint, s.rebuilt, s.stop), (1, false, None));
        let cp: Checkpoint =
            serde_json::from_slice(&fs::read(d.path().join("checkpoint.json")).unwrap()).unwrap();
        assert_eq!(
            cp,
            Checkpoint {
                seq: 1,
                hash: r.hash
            }
        );
        assert!(
            l.archive(&kp(), "t", 3, "nope").is_err(),
            "unknown position"
        );
    }

    #[test]
    fn crash_gap_archived_written_checkpoint_not_yet_is_rebuilt_from_the_log() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        let r = note(&l, 1);
        l.archive(&kp(), "t", r.seq, &r.hash).unwrap();
        let r2 = note(&l, 2);
        l.archive(&kp(), "t", r2.seq, &r2.hash).unwrap();
        // Simulate the crash: the checkpoint file still holds the older position.
        fs::write(
            d.path().join("checkpoint.json"),
            serde_json::to_string(&Checkpoint {
                seq: 1,
                hash: r.hash.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let s = l.load().unwrap();
        assert_eq!((s.checkpoint, s.rebuilt, s.stop), (r2.seq, true, None));
        // Also with no checkpoint file at all.
        fs::remove_file(d.path().join("checkpoint.json")).unwrap();
        assert_eq!(l.load().unwrap().checkpoint, r2.seq);
    }

    #[test]
    fn crash_gap_temporary_checkpoint_not_renamed_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        let r = note(&l, 1);
        l.archive(&kp(), "t", r.seq, &r.hash).unwrap();
        fs::remove_file(d.path().join("checkpoint.json")).unwrap();
        fs::write(
            d.path().join("checkpoint.json.tmp"),
            "{\"seq\":99,\"hash\":\"x\"}",
        )
        .unwrap();
        let s = l.load().unwrap();
        assert_eq!((s.checkpoint, s.rebuilt, s.stop), (1, true, None));
        assert!(!d.path().join("checkpoint.json.tmp").exists());
    }

    #[test]
    fn a_checkpoint_ahead_of_a_rolled_back_log_stops_and_is_never_lowered() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        let r = note(&l, 1);
        note(&l, 2);
        l.archive(&kp(), "t", 2, &l.load().unwrap().records[1].hash)
            .unwrap();
        let cp_before = fs::read(d.path().join("checkpoint.json")).unwrap();
        // The log rolls back to its first record (e.g. restored from an old backup).
        let first = format!("{}\n", r.to_line().unwrap());
        fs::write(d.path().join("log.jsonl"), first).unwrap();
        let s = l.load().unwrap();
        assert!(
            s.stop.as_deref().unwrap().contains("never lower"),
            "{:?}",
            s.stop
        );
        assert_eq!(s.checkpoint, 0, "no release from this snapshot");
        assert_eq!(
            fs::read(d.path().join("checkpoint.json")).unwrap(),
            cp_before,
            "file untouched"
        );
        assert!(
            note_err(&l).contains("stopped"),
            "appends refused while stopped"
        );
    }

    #[test]
    fn a_checkpoint_hash_conflict_stops() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        let r = note(&l, 1);
        l.archive(&kp(), "t", r.seq, &r.hash).unwrap();
        fs::write(
            d.path().join("checkpoint.json"),
            serde_json::to_string(&Checkpoint {
                seq: 1,
                hash: "ff".repeat(32),
            })
            .unwrap(),
        )
        .unwrap();
        let s = l.load().unwrap();
        assert!(s.stop.is_some(), "same position, other hash");
        // An older checkpoint position that is not in the log stops too (not rebuilt over).
        let l2d = tempfile::tempdir().unwrap();
        let l2 = ledger(l2d.path());
        let a = note(&l2, 1);
        let b = note(&l2, 2);
        l2.archive(&kp(), "t", b.seq, &b.hash).unwrap();
        fs::write(
            l2d.path().join("checkpoint.json"),
            serde_json::to_string(&Checkpoint {
                seq: a.seq,
                hash: "ee".repeat(32),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(l2.load().unwrap().stop.is_some());
    }

    #[test]
    fn archiving_never_lowers_the_checkpoint() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        let a = note(&l, 1);
        let b = note(&l, 2);
        l.archive(&kp(), "t", b.seq, &b.hash).unwrap();
        let err = l
            .archive(&kp(), "t", a.seq, &a.hash)
            .unwrap_err()
            .to_string();
        assert!(err.contains("lower the checkpoint"), "{err}");
        assert_eq!(l.load().unwrap().checkpoint, b.seq);
    }

    #[test]
    fn an_archived_record_naming_a_position_off_the_chain_stops() {
        for body in [
            json!({"seq": 1, "hash": "ab".repeat(32)}), // right position, wrong hash
            json!({"seq": 5, "hash": "ab".repeat(32)}), // a position the chain doesn't have yet
            json!({"seq": 1}),                          // no hash
        ] {
            let d = tempfile::tempdir().unwrap();
            let l = ledger(d.path());
            note(&l, 1);
            l.append(
                &kp(),
                "t",
                Draft {
                    kind: "archived".into(),
                    phase: String::new(),
                    body: body.clone(),
                },
            )
            .unwrap();
            let s = l.load().unwrap();
            assert!(s.stop.is_some(), "{body}");
            assert_eq!(s.checkpoint, 0, "{body}");
        }
    }

    #[test]
    fn a_cut_off_line_is_an_error_and_is_not_truncated() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        note(&l, 1);
        note(&l, 2);
        let path = d.path().join("log.jsonl");
        let full = fs::read_to_string(&path).unwrap();
        let cut = &full[..full.len() - 7];
        fs::write(&path, cut).unwrap();
        assert!(l.load().is_err());
        assert!(note_err(&l).contains("incomplete"));
        assert_eq!(fs::read_to_string(&path).unwrap(), cut, "left for the user");
    }

    #[test]
    fn a_tampered_log_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        note(&l, 1);
        let path = d.path().join("log.jsonl");
        let t = fs::read_to_string(&path)
            .unwrap()
            .replace("\"i\":1", "\"i\":7");
        fs::write(&path, t).unwrap();
        assert!(l.load().is_err());
        // And a log read with another key or repository.
        let d2 = tempfile::tempdir().unwrap();
        note(&ledger(d2.path()), 1);
        let other = Ledger::new(d2.path(), "other", kp().verifying_key()).unwrap();
        assert!(other.load().is_err());
    }

    #[test]
    fn concurrent_appenders_keep_one_chain() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_path_buf();
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let l = ledger(&dir);
                    for i in 0..6 {
                        note(&l, t * 100 + i);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let s = ledger(&dir).load().unwrap();
        assert_eq!(s.records.len(), 24);
    }

    fn note_err(l: &Ledger) -> String {
        l.append(
            &kp(),
            "t",
            Draft {
                kind: "note".into(),
                phase: "p".into(),
                body: json!({}),
            },
        )
        .unwrap_err()
        .to_string()
    }
}
