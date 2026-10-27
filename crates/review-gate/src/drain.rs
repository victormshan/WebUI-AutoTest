//! Maintenance drain (M2 step 8; Codex review of b7a3a91, n8 R1 and n10 §1–2).
//!
//! An operator stops admission by creating the drain file (`serve --drain-file`). From the
//! moment the latch is set, every external write operation is refused with 503 before it has
//! any effect, while operations admitted earlier run to the end and persist their results.
//!
//! One mutex linearizes the two sides: an operation is admitted (`enter`) only if the latch is
//! not set, and is then counted until its guard is dropped, which happens only after its last
//! write. So a snapshot taken under the same mutex that shows `draining` and no active
//! operation means that every operation admitted before the drain has finished persisting.
//! Work an operation hands to a background thread or task (an M1 review job, an M2 job) takes
//! a carried guard first, while the admitting guard is still held, so the count never drops to
//! zero in between.
//!
//! What did not end well — a result that could not be recorded, a job not sent, a panic — is
//! kept as a failure, and a failure blocks a safe stop as much as an active operation does.
//!
//! The latch cannot be undone in a running process. A restart reads the file again before
//! listening, so it stays drained until an authorized operator has removed the file.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Serialize;

/// How often the watcher looks at the drain file.
pub const POLL: Duration = Duration::from_secs(1);
/// Failures kept in full (the count keeps going).
const KEEP_FAILURES: usize = 32;

pub struct Drain {
    file: Option<PathBuf>,
    state: Mutex<State>,
    /// Runs right after an operation is admitted, outside the mutex (tests).
    #[cfg(test)]
    after_enter: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct State {
    draining: Option<String>,
    active: u64,
    failures: Vec<String>,
    failure_count: u64,
}

/// One consistent view (taken under the drain mutex).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    /// A drain file is configured.
    pub configured: bool,
    pub draining: bool,
    pub reason: Option<String>,
    /// Admitted operations (and the work they handed on) not finished yet.
    pub active_ops: u64,
    pub failure_count: u64,
    pub failures: Vec<String>,
}

impl Snapshot {
    /// Safe to stop: drained, nothing running, nothing that went wrong.
    pub fn idle(&self) -> bool {
        self.draining && self.active_ops == 0 && self.failure_count == 0
    }
}

/// Holds one admitted operation (or work carried on from one) in the count.
pub struct OpGuard {
    drain: Arc<Drain>,
    what: String,
}

/// Is the drain file there? Only NotFound means "no": any other error is an error.
fn probe(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

impl Drain {
    pub fn new(file: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            file,
            state: Mutex::new(State::default()),
            #[cfg(test)]
            after_enter: Mutex::new(None),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Before listening or admitting anything: a drain file that is already there latches at
    /// once; one that cannot be checked stops the start.
    pub fn check_at_start(&self) -> Result<()> {
        let Some(f) = &self.file else {
            return Ok(());
        };
        match probe(f) {
            Ok(true) => self.latch("drain file present at start".into()),
            Ok(false) => {}
            Err(e) => bail!("cannot check the drain file {}: {}", f.display(), e.kind()),
        }
        Ok(())
    }

    /// Looks at the drain file every `POLL` on an OS thread of its own. An error other than
    /// NotFound latches too (admission stops rather than guessing).
    pub fn watch(self: &Arc<Self>) {
        let Some(f) = self.file.clone() else {
            return;
        };
        let me = self.clone();
        std::thread::Builder::new()
            .name("drain-watch".into())
            .spawn(move || {
                loop {
                    match probe(&f) {
                        Ok(true) => {
                            me.latch("drain file present".into());
                            return;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            me.latch(format!("drain file unreadable: {}", e.kind()));
                            return;
                        }
                    }
                    std::thread::sleep(POLL);
                }
            })
            .expect("spawning the drain watcher");
    }

    /// Sets the latch (the first reason is kept).
    pub fn latch(&self, why: String) {
        let mut s = self.lock();
        if s.draining.is_none() {
            eprintln!("review-gate: draining: {why}");
            s.draining = Some(why);
        }
    }

    /// Admits one external operation, or refuses it once draining.
    pub fn enter(self: &Arc<Self>, what: &str) -> std::result::Result<OpGuard, String> {
        let mut s = self.lock();
        if let Some(why) = &s.draining {
            return Err(why.clone());
        }
        s.active += 1;
        drop(s);
        #[cfg(test)]
        {
            let h = self.after_enter.lock().unwrap().clone();
            if let Some(h) = h {
                h();
            }
        }
        Ok(OpGuard {
            drain: self.clone(),
            what: what.to_string(),
        })
    }

    /// A read that may write (recovery, import, archive) is counted like an admitted operation
    /// while not draining; once draining it gets `None` and must take a path that writes
    /// nothing (m2c/A214-1). Decided under the same mutex as `enter` and `snapshot`, so a
    /// snapshot that is idle stays idle: nothing that could write starts after it.
    pub fn read(self: &Arc<Self>, what: &str) -> Option<OpGuard> {
        let mut s = self.lock();
        if s.draining.is_some() {
            return None;
        }
        s.active += 1;
        Some(OpGuard {
            drain: self.clone(),
            what: what.to_string(),
        })
    }

    /// Counts an operation that is never refused (a read that may still write recovery
    /// records): it keeps a safe stop waiting while it runs.
    pub fn track(self: &Arc<Self>, what: &str) -> OpGuard {
        self.lock().active += 1;
        OpGuard {
            drain: self.clone(),
            what: what.to_string(),
        }
    }

    /// Records something that did not end well; it blocks a safe stop.
    pub fn fail(&self, why: String) {
        eprintln!("review-gate: drain failure: {why}");
        let mut s = self.lock();
        s.failure_count += 1;
        if s.failures.len() < KEEP_FAILURES {
            s.failures.push(why);
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let s = self.lock();
        Snapshot {
            configured: self.file.is_some(),
            draining: s.draining.is_some(),
            reason: s.draining.clone(),
            active_ops: s.active,
            failure_count: s.failure_count,
            failures: s.failures.clone(),
        }
    }
}

impl OpGuard {
    /// Counts work this operation hands on (a background job). Never refused: the operation
    /// was admitted, and the new guard exists before this one can be dropped.
    pub fn carry(&self, what: &str) -> OpGuard {
        self.drain.lock().active += 1;
        OpGuard {
            drain: self.drain.clone(),
            what: what.to_string(),
        }
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.drain.fail(format!("{} panicked", self.what));
        }
        let mut s = self.drain.lock();
        s.active = s.active.saturating_sub(1);
    }
}

#[cfg(test)]
impl Drain {
    pub(crate) fn set_after_enter(&self, h: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.after_enter.lock().unwrap() = h;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_after_the_latch_and_counts_until_drop() {
        let d = Drain::new(None);
        let g = d.enter("a").unwrap();
        let c = g.carry("a job");
        assert_eq!(d.snapshot().active_ops, 2);
        d.latch("test".into());
        assert_eq!(d.enter("b").err().as_deref(), Some("test"));
        drop(g);
        let s = d.snapshot();
        assert!(s.draining && s.active_ops == 1 && !s.idle());
        drop(c);
        assert!(d.snapshot().idle());
    }

    #[test]
    fn a_read_is_counted_before_the_drain_and_gets_the_unwritten_path_after_it() {
        // m2c/A214-1: a read admitted before the latch keeps a safe stop waiting; once the
        // latch is set no read is counted any more, so an idle snapshot stays idle.
        let d = Drain::new(None);
        let r = d.read("status").expect("not draining: counted");
        d.latch("t".into());
        assert!(
            !d.snapshot().idle(),
            "the read admitted before the latch is still running"
        );
        assert!(
            d.read("status").is_none(),
            "after the latch: the unwritten path"
        );
        drop(r);
        let s = d.snapshot();
        assert!(s.idle(), "{s:?}");
        assert!(d.read("status").is_none());
        assert!(d.snapshot().idle(), "still idle after another read");
    }

    #[test]
    fn a_failure_blocks_idle() {
        let d = Drain::new(None);
        d.latch("x".into());
        d.fail("m2 job q1: result not recorded".into());
        let s = d.snapshot();
        assert_eq!((s.active_ops, s.failure_count), (0, 1));
        assert!(!s.idle());
    }

    #[test]
    fn a_panic_while_holding_a_guard_is_a_failure() {
        let d = Drain::new(None);
        let g = d.enter("op").unwrap();
        let r = std::thread::spawn(move || {
            let _g = g;
            panic!("boom");
        })
        .join();
        assert!(r.is_err());
        let s = d.snapshot();
        assert_eq!(s.active_ops, 0);
        assert_eq!(s.failures, vec!["op panicked".to_string()]);
    }

    #[test]
    fn the_file_latches_at_start_and_errors_stop_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("drain");
        let d = Drain::new(Some(f.clone()));
        d.check_at_start().unwrap();
        assert!(!d.snapshot().draining);
        std::fs::write(&f, "").unwrap();
        let d = Drain::new(Some(f.clone()));
        d.check_at_start().unwrap();
        assert_eq!(
            d.snapshot().reason.as_deref(),
            Some("drain file present at start")
        );
        // A path through a regular file: NotDir, not NotFound.
        let d = Drain::new(Some(f.join("below")));
        let e = d.check_at_start().unwrap_err().to_string();
        assert!(e.contains("cannot check the drain file"), "{e}");
    }

    #[test]
    fn the_watcher_latches_when_the_file_appears() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("drain");
        let d = Drain::new(Some(f.clone()));
        d.check_at_start().unwrap();
        d.watch();
        assert!(d.enter("before").is_ok());
        std::fs::write(&f, "").unwrap();
        let t0 = std::time::Instant::now();
        while !d.snapshot().draining {
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "watcher never latched"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(d.snapshot().reason.as_deref(), Some("drain file present"));
        assert!(d.enter("after").is_err());
        // Removing the file does not undo the latch.
        std::fs::remove_file(&f).unwrap();
        std::thread::sleep(POLL * 2);
        assert!(d.enter("after removal").is_err());
    }
}
