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
use super::sync::{Remote, Sync, classify};
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
    /// Fault injection: a transaction stops (as a crash would) after writing this many records.
    #[cfg(test)]
    crash_after: std::sync::atomic::AtomicUsize,
}

/// What a transaction decides on the snapshot it was given.
pub struct Tx<T> {
    /// Appended in order, each sealed on the one before.
    pub drafts: Vec<Draft>,
    pub out: T,
}

#[derive(Debug, thiserror::Error)]
pub enum TxError {
    /// The log needs the user or the remote: nothing is written.
    #[error("the milestone log is stopped: {0}")]
    Stopped(String),
    /// The transaction's own checks refused, on the state under the lock: nothing is written.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Ledger {
    pub fn new(dir: impl Into<PathBuf>, repo: &str, key: VerifyingKey) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            repo: repo.to_string(),
            key,
            #[cfg(test)]
            crash_after: std::sync::atomic::AtomicUsize::new(usize::MAX),
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

    /// One transaction (§3.7, and the 6b plan): under the lock, load and verify the log, let `f`
    /// check its preconditions on that very snapshot and say what to append, then append all of
    /// it, each record fsynced. `f` refusing, or a stopped log, writes nothing. A crash in the
    /// middle leaves a prefix of the group on disk; the caller's recovery completes it.
    pub fn transact<T>(
        &self,
        kp: &Keypair,
        at: &str,
        f: impl FnOnce(&Snapshot) -> std::result::Result<Tx<T>, String>,
    ) -> std::result::Result<(T, Vec<Record>), TxError> {
        let _lock = self.lock()?;
        let mut snap = self.load_locked()?;
        if let Some(why) = &snap.stop {
            return Err(TxError::Stopped(why.clone()));
        }
        let tx = f(&snap).map_err(TxError::Refused)?;
        let mut written = Vec::new();
        for d in tx.drafts {
            #[cfg(test)]
            if written.len() == self.crash_after.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(TxError::Other(anyhow::anyhow!(
                    "injected crash after {} records",
                    written.len()
                )));
            }
            let r = self.append_locked(kp, at, &snap, d)?;
            snap.records.push(r.clone());
            written.push(r);
        }
        Ok((tx.out, written))
    }

    /// Stops the next transactions after `n` records (fault injection; `usize::MAX`: never).
    #[cfg(test)]
    pub fn crash_after(&self, n: usize) {
        self.crash_after
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Brings the remote's extra records in as they are (§3.6, "L is a prefix of R"): under the
    /// lock the remote is classified again against the log as it is now, never against an
    /// earlier read; only `RemoteAhead` writes, and it writes the remote records byte for byte
    /// (no new seal), each checked to read back with its own hash, and fsynced. Returns the
    /// classification it acted on.
    pub fn import(&self, remote: &Remote) -> Result<Sync> {
        Ok(self.import_and_load(remote)?.0)
    }

    /// [`Ledger::import`], and the snapshot as it is after it, both under one lock: state
    /// derived from this snapshot goes with exactly this classification.
    pub fn import_and_load(&self, remote: &Remote) -> Result<(Sync, Snapshot)> {
        let _lock = self.lock()?;
        let mut snap = self.load_locked()?;
        let sync = self.import_locked(&mut snap, remote)?;
        Ok((sync, snap))
    }

    fn import_locked(&self, snap: &mut Snapshot, remote: &Remote) -> Result<Sync> {
        if let Some(why) = &snap.stop {
            return Ok(Sync::Stop(format!("local log stopped: {why}")));
        }
        let cp = (snap.checkpoint > 0).then(|| Checkpoint {
            seq: snap.checkpoint,
            hash: snap.records[snap.checkpoint as usize - 1].hash.clone(),
        });
        let sync = classify(&snap.records, remote, cp.as_ref(), &self.key, &self.repo);
        if let Sync::RemoteAhead { extra } = &sync {
            let mut all = snap.records.clone();
            all.extend(extra.iter().cloned());
            verify_chain(&all, &self.key, &self.repo)?;
            let new = !self.log_path().exists();
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.log_path())?;
            for r in extra {
                let line = r.to_line()?;
                let back = parse_log(&format!("{line}\n"))?;
                if back.len() != 1 || back[0].compute_hash()? != r.hash || back[0] != *r {
                    bail!("remote record {} does not survive a round trip", r.seq);
                }
                f.write_all(format!("{line}\n").as_bytes())?;
                f.sync_all()?;
            }
            if new {
                File::open(&self.dir)?.sync_all()?;
            }
            snap.records = all;
        }
        Ok(sync)
    }

    /// Archives exactly as far as this remote read proves (Codex review of b5693bb, P1): under
    /// one lock the remote is classified against the log as it is now (importing what it has
    /// beyond it), and the position archived is the remote's own length on that classification,
    /// never a length taken from the log after the lock was released. `None`: nothing new.
    pub fn archive_remote(
        &self,
        kp: &Keypair,
        at: &str,
        remote: &Remote,
    ) -> Result<(Sync, Option<Record>)> {
        let _lock = self.lock()?;
        let mut snap = self.load_locked()?;
        let sync = self.import_locked(&mut snap, remote)?;
        let held = match &sync {
            // After an import the local log is exactly the remote's.
            Sync::Equal | Sync::RemoteAhead { .. } => snap.records.len(),
            Sync::RemoteBehind { unarchived } => snap.records.len() - unarchived,
            Sync::Stop(_) => return Ok((sync, None)),
        } as u64;
        if held == 0 || held <= snap.checkpoint {
            return Ok((sync, None));
        }
        let hash = snap.records[held as usize - 1].hash.clone();
        let r = self.archive_locked(kp, at, &snap, held, &hash)?;
        Ok((sync, Some(r)))
    }

    /// Records that the remote holds the chain up to `seq` (with `hash`): first the `archived`
    /// record, then the checkpoint file (§3.5, §3.7).
    pub fn archive(&self, kp: &Keypair, at: &str, seq: u64, hash: &str) -> Result<Record> {
        let _lock = self.lock()?;
        let snap = self.load_locked()?;
        self.archive_locked(kp, at, &snap, seq, hash)
    }

    fn archive_locked(
        &self,
        kp: &Keypair,
        at: &str,
        snap: &Snapshot,
        seq: u64,
        hash: &str,
    ) -> Result<Record> {
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
            snap,
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

/// An offline view of the log for the user (6c `admin milestone status`, Codex n6 §3): nothing
/// is written, locked, created, recovered or fetched. Never a basis for release.
#[derive(Debug, Clone, Serialize)]
pub struct ReadOnly {
    /// Every complete line, each verified with the fixed key (a bad one is an error).
    pub records: Vec<Record>,
    /// The file ends in a line without its newline (a write in progress or cut off): only the
    /// verified prefix above is shown; the view is incomplete.
    pub truncated_tail: bool,
    /// The checkpoint the log itself proves (last `archived` record), when it agrees with the
    /// checkpoint file; else 0.
    pub checkpoint: u64,
    /// Why the checkpoint file and the log do not agree (they are read at different moments
    /// without the lock, so this may also be a concurrent write): nothing is repaired.
    pub checkpoint_mismatch: Option<String>,
}

/// Reads `dir` without writing anything. `key` must be the fixed public key the user pins: it is
/// never taken from the log or created.
pub fn read_only(dir: &Path, repo: &str, key: &VerifyingKey) -> Result<ReadOnly> {
    if !dir.is_dir() {
        bail!("{} does not exist: no milestone state here", dir.display());
    }
    let text = match fs::read_to_string(dir.join("log.jsonl")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).context("reading the milestone log"),
    };
    let (complete, truncated_tail) = match text.rfind('\n') {
        _ if text.is_empty() => ("", false),
        Some(i) => (&text[..=i], i + 1 < text.len()),
        None => ("", true),
    };
    let records = parse_log(complete)?;
    verify_chain(&records, key, repo)?;
    let from_log = last_archived(&records);
    let file: Option<std::result::Result<Checkpoint, String>> =
        match fs::read(dir.join("checkpoint.json")) {
            Ok(b) => Some(serde_json::from_slice(&b).map_err(|e| e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("reading the checkpoint"),
        };
    let (checkpoint, checkpoint_mismatch) = match (from_log, file) {
        (Err(why), _) => (0, Some(why)),
        (Ok(_), Some(Err(e))) => (0, Some(format!("checkpoint file unreadable: {e}"))),
        (Ok(None), None) => (0, None),
        (Ok(Some(l)), Some(Ok(f))) if l == f => (l.seq, None),
        (Ok(l), f) => (
            0,
            Some(format!(
                "the log's last archived position ({}) and the checkpoint file ({}) differ",
                l.map_or("none".into(), |l| format!("{} {}", l.seq, l.hash)),
                f.and_then(|f| f.ok())
                    .map_or("missing".into(), |f| format!("{} {}", f.seq, f.hash)),
            )),
        ),
    };
    Ok(ReadOnly {
        records,
        truncated_tail,
        checkpoint,
        checkpoint_mismatch,
    })
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

    fn draft(i: u64) -> Draft {
        Draft {
            kind: "note".into(),
            phase: "p".into(),
            body: json!({"i": i}),
        }
    }

    #[test]
    fn a_transaction_checks_under_the_lock_and_writes_the_whole_group() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        note(&l, 1);
        let (seen, written) = l
            .transact(&kp(), "t", |snap| {
                Ok(Tx {
                    drafts: vec![draft(2), draft(3)],
                    out: snap.records.len(),
                })
            })
            .unwrap();
        assert_eq!(seen, 1, "the closure saw the log as it is under the lock");
        assert_eq!(written.iter().map(|r| r.seq).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(written[1].prev, written[0].hash, "sealed one on the other");
        assert_eq!(l.load().unwrap().records.len(), 3);
        // A refusal writes nothing.
        let before = fs::read(d.path().join("log.jsonl")).unwrap();
        let e = l
            .transact::<()>(&kp(), "t", |_| Err("already pending".into()))
            .unwrap_err();
        assert!(matches!(e, TxError::Refused(ref w) if w == "already pending"));
        assert_eq!(fs::read(d.path().join("log.jsonl")).unwrap(), before);
    }

    #[test]
    fn a_crash_inside_a_transaction_leaves_a_prefix_of_the_group() {
        for k in 0..3 {
            let d = tempfile::tempdir().unwrap();
            let l = ledger(d.path());
            note(&l, 1);
            l.crash_after(k);
            let e = l
                .transact(&kp(), "t", |_| {
                    Ok(Tx {
                        drafts: vec![draft(2), draft(3), draft(4)],
                        out: (),
                    })
                })
                .unwrap_err();
            assert!(e.to_string().contains("injected crash"), "{e}");
            l.crash_after(usize::MAX);
            let s = ledger(d.path()).load().unwrap();
            assert_eq!(
                s.records.len(),
                1 + k,
                "a verified prefix of the group, k={k}"
            );
            assert_eq!(s.stop, None);
        }
    }

    #[test]
    fn a_stopped_log_refuses_a_transaction() {
        let d = tempfile::tempdir().unwrap();
        let l = ledger(d.path());
        note(&l, 1);
        fs::write(d.path().join("checkpoint.json"), r#"{"seq":9,"hash":"x"}"#).unwrap();
        let e = l
            .transact(&kp(), "t", |_| {
                Ok(Tx {
                    drafts: vec![draft(2)],
                    out: (),
                })
            })
            .unwrap_err();
        assert!(matches!(e, TxError::Stopped(_)), "{e}");
    }

    /// A second ledger (the remote's chain) built from the same key.
    fn remote_with(n: u64) -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let r = ledger(d.path());
        for i in 1..=n {
            note(&r, i);
        }
        let text = fs::read_to_string(d.path().join("log.jsonl")).unwrap();
        (d, text)
    }

    #[test]
    fn importing_a_longer_remote_copies_its_records_byte_for_byte() {
        let (_r, text) = remote_with(3);
        let d = tempfile::tempdir().unwrap();
        // The local log holds the remote's first record.
        fs::write(
            d.path().join("log.jsonl"),
            text.lines().next().unwrap().to_string() + "\n",
        )
        .unwrap();
        let l = ledger(d.path());
        let sync = l.import(&Remote::Log(text.clone())).unwrap();
        assert!(matches!(sync, Sync::RemoteAhead { ref extra } if extra.len() == 2));
        assert_eq!(
            fs::read_to_string(d.path().join("log.jsonl")).unwrap(),
            text
        );
        // Nothing more to import now.
        assert_eq!(l.import(&Remote::Log(text)).unwrap(), Sync::Equal);
    }

    #[test]
    fn import_classifies_again_under_the_lock_and_never_writes_otherwise() {
        let (_r, text) = remote_with(2);
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("log.jsonl"),
            text.lines().next().unwrap().to_string() + "\n",
        )
        .unwrap();
        let l = ledger(d.path());
        // The local log grew its own record 2 after an earlier read: now they diverge.
        note(&l, 99);
        let before = fs::read(d.path().join("log.jsonl")).unwrap();
        assert!(matches!(
            l.import(&Remote::Log(text.clone())).unwrap(),
            Sync::Stop(_)
        ));
        assert_eq!(fs::read(d.path().join("log.jsonl")).unwrap(), before);
        // A tampered remote record: refused, nothing written.
        let d2 = tempfile::tempdir().unwrap();
        fs::write(
            d2.path().join("log.jsonl"),
            text.lines().next().unwrap().to_string() + "\n",
        )
        .unwrap();
        let l2 = ledger(d2.path());
        let bad = text.replace("\"i\":2", "\"i\":3");
        assert!(matches!(
            l2.import(&Remote::Log(bad)).unwrap(),
            Sync::Stop(_)
        ));
        assert_eq!(l2.load().unwrap().records.len(), 1);
        // Unreachable: Stop, nothing written.
        assert!(matches!(
            l2.import(&Remote::Unreachable("down".into())).unwrap(),
            Sync::Stop(_)
        ));
    }

    #[test]
    fn the_offline_view_writes_nothing_and_reports_what_it_could_not_check() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("none");
        assert!(read_only(&missing, "repo", &kp().verifying_key()).is_err());
        assert!(!missing.exists(), "nothing created");
        let l = ledger(d.path());
        let a = note(&l, 1);
        note(&l, 2);
        l.archive(&kp(), "t", 1, &a.hash).unwrap();
        let ro = read_only(d.path(), "repo", &kp().verifying_key()).unwrap();
        assert_eq!(
            (ro.records.len(), ro.truncated_tail, ro.checkpoint),
            (3, false, 1)
        );
        assert!(ro.checkpoint_mismatch.is_none());
        // A half-written last line: the verified prefix, marked incomplete.
        let log = d.path().join("log.jsonl");
        let mut text = fs::read_to_string(&log).unwrap();
        text.push_str("{\"seq\":4,");
        fs::write(&log, &text).unwrap();
        let ro = read_only(d.path(), "repo", &kp().verifying_key()).unwrap();
        assert_eq!((ro.records.len(), ro.truncated_tail), (3, true));
        // The checkpoint file missing (the crash gap): reported, not rebuilt.
        fs::remove_file(d.path().join("checkpoint.json")).unwrap();
        let ro = read_only(d.path(), "repo", &kp().verifying_key()).unwrap();
        assert_eq!(ro.checkpoint, 0);
        assert!(ro.checkpoint_mismatch.is_some());
        assert!(!d.path().join("checkpoint.json").exists(), "not rebuilt");
        // Another key, or a bad complete line, is an error, never skipped.
        assert!(
            read_only(
                d.path(),
                "repo",
                &Keypair::from_seed([1; 32]).verifying_key()
            )
            .is_err()
        );
        let lines: Vec<&str> = text.lines().collect();
        let tampered = format!("{}\n{}\n", lines[0], lines[1].replace("\"i\":2", "\"i\":3"));
        fs::write(&log, tampered).unwrap();
        assert!(read_only(d.path(), "repo", &kp().verifying_key()).is_err());
    }
}
