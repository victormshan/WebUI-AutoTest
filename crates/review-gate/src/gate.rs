//! The state machine: version gate, reviewer-strength gate, tree binding, circuit breaker.
//!
//! The implementer never reports a verdict. It asks the gate to `record` a review record by id;
//! records are created only by the gate (from an external model's reply, or from a weaker
//! channel explicitly submitted), live in the gate's private store, and are single-use.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::attest::{self, Keypair, Statement};
use crate::identity::{Identity, canonical_family};
use crate::model::{
    Action, Channel, HistoryEntry, Outcome, ReviewRecord, Status, Task, Verdict, review_eligibility,
};
use crate::relay::RelayTrace;
use crate::store::{Store, valid_id};
use crate::{GateError, git};

pub const REJECT_STREAK_LIMIT: u32 = 3;
pub const MAX_ITERATIONS: u32 = 10;

pub struct NewTask {
    pub id: String,
    /// The authenticated principal that creates the task; frozen with the task.
    pub principal: String,
    /// The creator's canonical model family; frozen as the implementer family.
    pub implementer_family: String,
    /// Non-secret accountability: the credential id that created the task and the digest of the
    /// identity configuration in force.
    pub credential: String,
    pub config_digest: Option<String>,
    /// Optional self-report from the client: only checked for consistency, never trusted.
    pub claimed_family: Option<String>,
    pub goal: String,
    pub final_acceptance: String,
    pub repo: String,
    pub iterations: u32,
    pub min_reviewer: Channel,
    pub review_provider: String,
}

/// A review to store: the staged tree it covers and what the reviewer said.
pub struct ReviewInput {
    pub tree: String,
    pub base: String,
    pub verdict: Verdict,
    pub channel: Channel,
    pub provider: String,
    pub model: Option<String>,
    pub family: String,
    pub prompt_sha: String,
    pub text: String,
}

/// Whether a commit of `tree` on top of `base` is covered by the gate (the hook asks this).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CommitCheck {
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    pub reason: String,
}

/// Result of recording a review against a task.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub action: Action,
    pub task: Task,
}

pub struct Gate {
    store: Store,
    signer: Option<Keypair>,
    relay: Option<RelayTrace>,
}

impl Gate {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            signer: None,
            relay: None,
        }
    }

    /// Sign an attestation for every approved version.
    pub fn with_signer(mut self, signer: Keypair) -> Self {
        self.signer = Some(signer);
        self
    }

    /// Write the gate's own entries into claude-step-relay's trace for linked tasks.
    pub fn with_relay(mut self, relay: RelayTrace) -> Self {
        self.relay = Some(relay);
        self
    }

    pub fn public_key(&self) -> Option<String> {
        self.signer.as_ref().map(Keypair::public_b64)
    }

    /// Best effort: the trace is a record for humans, never a reason to fail a gate decision.
    fn trace(&self, task: &Task, text: &str) {
        if let (Some(relay), Some(expr)) = (&self.relay, &task.expr_id)
            && let Err(e) = relay.append(expr, text)
        {
            eprintln!("review-gate: relay trace for {expr}: {e:#}");
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn init(&self, t: NewTask) -> Result<Task, GateError> {
        if !valid_id(&t.id) {
            return Err(GateError::InvalidId(t.id));
        }
        if !valid_id(&t.principal) {
            return Err(GateError::Invalid(format!(
                "invalid principal {:?}",
                t.principal
            )));
        }
        let implementer_family = canonical_family(&t.implementer_family).ok_or_else(|| {
            GateError::Invalid(format!(
                "unknown implementer family {:?}; the service identity must map to a known model family",
                t.implementer_family
            ))
        })?;
        if let Some(claim) = t.claimed_family.as_deref() {
            let claim = canonical_family(claim).ok_or_else(|| {
                GateError::Mismatch(format!("unknown claimed implementer family {claim:?}"))
            })?;
            if claim != implementer_family {
                return Err(GateError::Mismatch(format!(
                    "claimed implementer family {claim} does not match the authenticated family {implementer_family}"
                )));
            }
        }
        if !(1..=MAX_ITERATIONS).contains(&t.iterations) {
            return Err(GateError::Invalid(format!(
                "iterations must be 1-{MAX_ITERATIONS}"
            )));
        }
        if t.goal.trim().is_empty() || t.final_acceptance.trim().is_empty() {
            return Err(GateError::Invalid(
                "goal and acceptance are required".into(),
            ));
        }
        if self.store.task_exists(&t.id)? {
            return Err(GateError::Conflict(format!("task {} already exists", t.id)));
        }
        let repo = std::fs::canonicalize(&t.repo)
            .map_err(|e| GateError::Invalid(format!("repo {}: {e}", t.repo)))?;
        git::rev_parse(&repo, "HEAD")?;
        let now = Utc::now();
        let task = Task {
            id: t.id,
            principal: t.principal,
            credential: t.credential,
            config_digest: t.config_digest,
            owner_assigned: None,
            expr_id: None,
            goal: t.goal,
            final_acceptance: t.final_acceptance,
            repo: repo.to_string_lossy().into_owned(),
            iterations: t.iterations,
            current_iteration: 1,
            reject_streak: 0,
            min_reviewer: t.min_reviewer,
            review_provider: t.review_provider,
            implementer_family: implementer_family.to_string(),
            status: Status::Running,
            stop_reason: None,
            created_at: now,
            updated_at: now,
            history: Vec::new(),
        };
        self.store.save_task(&task)?;
        Ok(task)
    }

    /// Whether `who` may read this task: its owner, or an administrator.
    pub fn can_read(&self, task: &Task, who: &Identity) -> bool {
        who.admin || task.owner_principal() == Some(who.principal.as_str())
    }

    /// Whether `who` may change this task, or have a commit checked against it (Codex review
    /// of the integration: reading and writing are separate). Only its owner, and the local
    /// administrator command for a task with a known owner. A task with no known owner can be
    /// changed by nobody — not an administrator over HTTP, not the local command — until
    /// `admin task assign` gives it one; an administrator over HTTP has no write exemption.
    pub fn can_write(&self, task: &Task, who: &Identity) -> bool {
        match task.owner_principal() {
            None => false,
            Some(owner) => owner == who.principal || who.local_admin,
        }
    }

    pub fn ensure_readable(&self, task: &Task, who: &Identity) -> Result<(), GateError> {
        if self.can_read(task, who) {
            Ok(())
        } else {
            Err(GateError::Forbidden(format!(
                "task {} belongs to another principal",
                task.id
            )))
        }
    }

    /// The write check of every route that changes a task or starts work on it.
    pub fn ensure_owner(&self, task: &Task, who: &Identity) -> Result<(), GateError> {
        if self.can_write(task, who) {
            Ok(())
        } else {
            Err(GateError::Forbidden(match task.owner_principal() {
                Some(owner) => format!(
                    "task {} belongs to principal {owner}, not {}",
                    task.id, who.principal
                ),
                None => format!(
                    "task {} has no known owner (created before principals); nobody can change it until `admin task assign` gives it one",
                    task.id
                ),
            }))
        }
    }

    /// `admin task assign`: gives a task with no known owner its first owner (n20). Refused when
    /// the task already has an owner (there is no reassignment), when it changed since the
    /// operator looked (`expect_updated_at`), or without a reason. Without `yes` nothing is
    /// written and the task as it would be saved is returned. The owner and its audit are one
    /// atomic write of the task file.
    ///
    /// The write needs `lock`, the exclusive state lock (the service stopped, so nothing else
    /// changes the task between the check and the save, and two assignments cannot interleave).
    pub fn assign_owner(
        &self,
        id: &str,
        principal: &str,
        reason: &str,
        expect_updated_at: DateTime<Utc>,
        lock: Option<&crate::store::ServiceLock>,
    ) -> Result<Task, GateError> {
        let yes = lock.is_some();
        if !crate::store::valid_id(principal) {
            return Err(GateError::Invalid(format!(
                "invalid principal {principal:?}"
            )));
        }
        if reason.trim().is_empty() {
            return Err(GateError::Invalid("a reason is required".into()));
        }
        let mut task = self.store.load_task(id)?;
        if let Some(owner) = task.owner_principal() {
            return Err(GateError::Conflict(format!(
                "task {id} already belongs to {owner}; an owner is assigned once and never changed"
            )));
        }
        if task.updated_at != expect_updated_at {
            return Err(GateError::Conflict(format!(
                "task {id} changed since it was listed (updated_at {}, expected {})",
                task.updated_at.to_rfc3339(),
                expect_updated_at.to_rfc3339()
            )));
        }
        let now = Utc::now();
        task.principal = principal.to_string();
        task.owner_assigned = Some(crate::model::OwnerAssignment {
            principal: principal.to_string(),
            at: now,
            by: "admin".into(),
            reason: reason.trim().to_string(),
        });
        task.updated_at = now;
        if yes {
            self.store.save_task(&task)?;
        }
        Ok(task)
    }

    pub fn link(&self, who: &Identity, id: &str, expr_id: &str) -> Result<Task, GateError> {
        let mut task = self.store.load_task(id)?;
        self.ensure_owner(&task, who)?;
        task.expr_id = Some(expr_id.to_string());
        task.updated_at = Utc::now();
        self.store.save_task(&task)?;
        Ok(task)
    }

    pub fn pause(&self, who: &Identity, id: &str, reason: &str) -> Result<Task, GateError> {
        let mut task = self.store.load_task(id)?;
        self.ensure_owner(&task, who)?;
        task.status = Status::Paused;
        task.stop_reason = Some(reason.to_string());
        task.updated_at = Utc::now();
        self.store.save_task(&task)?;
        Ok(task)
    }

    /// Stores a new review record for the task's current version. Only the gate calls this:
    /// from an external model's reply, or from an explicitly submitted weaker channel.
    pub fn add_review(
        &self,
        who: &Identity,
        task: &Task,
        input: ReviewInput,
    ) -> Result<ReviewRecord, GateError> {
        self.ensure_owner(task, who)?;
        if task.status != Status::Running {
            return Err(GateError::NotRunning(task.id.clone(), task.status));
        }
        // The same rule at the storage boundary: a record can never claim a family the
        // implementer-neutral policy forbids, whatever produced it.
        let family = review_eligibility(
            &task.implementer_family,
            input.channel,
            &input.family,
            who.admin,
        )
        .map_err(|e| GateError::Mismatch(format!("review is not eligible: {e}")))?;
        let attempt = self.store.review_count(&task.id, task.current_iteration)? + 1;
        let r = ReviewRecord {
            id: format!("v{}-{attempt}", task.current_iteration),
            task: task.id.clone(),
            iteration: task.current_iteration,
            attempt,
            tree: input.tree,
            base: input.base,
            verdict: input.verdict,
            channel: input.channel,
            provider: input.provider,
            model: input.model,
            family,
            prompt_sha: input.prompt_sha,
            text: input.text,
            at: Utc::now(),
        };
        self.store.save_review(&r)?;
        self.trace(
            task,
            &format!(
                "第 {}/{} 版审核记录 {}（{} · {}{}）\n\n{}",
                task.current_iteration,
                task.iterations,
                r.id,
                r.channel,
                r.provider,
                r.model
                    .as_deref()
                    .map(|m| format!(" · {m}"))
                    .unwrap_or_default(),
                r.text.trim()
            ),
        );
        Ok(r)
    }

    /// A commit in `repo` is allowed when no running task governs the repository, or when the
    /// running task holds an unused approved review (strong enough) for exactly `tree` on `base`.
    pub fn check_commit(
        &self,
        who: &Identity,
        repo: &str,
        tree: &str,
        base: &str,
    ) -> Result<CommitCheck, GateError> {
        let repo = std::fs::canonicalize(repo)
            .map_err(|e| GateError::Invalid(format!("repo {repo}: {e}")))?
            .to_string_lossy()
            .into_owned();
        let running: Vec<Task> = self
            .store
            .list_tasks()?
            .into_iter()
            .filter(|t| t.repo == repo && t.status == Status::Running)
            .collect();
        if running.is_empty() {
            return Ok(CommitCheck {
                allowed: true,
                task: None,
                review: None,
                reason: "no running auto-iterate task for this repository".into(),
            });
        }
        // Any running task of this repository that is not the caller's (another principal's, or
        // one with no known owner) refuses the commit, even when the caller has a running task
        // there too: an approval is never borrowed across principals, and a conflict is not
        // resolved by looking only at the caller's own task. Nothing about the other task is
        // revealed.
        if running.iter().any(|t| !self.can_write(t, who)) {
            return Ok(CommitCheck {
                allowed: false,
                task: None,
                review: None,
                reason: "a running task for this repository belongs to another principal or has no known owner; only its owner may commit under it".into(),
            });
        }
        // Every running task here is the caller's. A caller may run several on one repository
        // (init does not forbid it): an unused, eligible approval of this tree under any of
        // them covers the commit (gate review of the integration, v1-1). Reported with the task
        // it belongs to; `record` binds the commit to that review explicitly.
        for task in &running {
            if let Some(r) = self.approved_review(task, tree, base)? {
                return Ok(CommitCheck {
                    allowed: true,
                    reason: format!("covered by approved review {} of task {}", r, task.id),
                    review: Some(r),
                    task: Some(task.id.clone()),
                });
            }
        }
        let names: Vec<String> = running
            .iter()
            .map(|t| format!("{} 第 {} 版", t.id, t.current_iteration))
            .collect();
        Ok(CommitCheck {
            allowed: false,
            reason: format!(
                "{} 没有覆盖当前暂存内容（tree {tree} on {base}）的通过审核：先审核，通过后原样提交",
                names.join("、")
            ),
            task: (running.len() == 1).then(|| running[0].id.clone()),
            review: None,
        })
    }

    /// The newest unused approval of `tree` on `base` in the task's current version that is
    /// strong enough and eligible, if any.
    fn approved_review(
        &self,
        task: &Task,
        tree: &str,
        base: &str,
    ) -> Result<Option<String>, GateError> {
        let count = self.store.review_count(&task.id, task.current_iteration)?;
        for attempt in (1..=count).rev() {
            let r = self
                .store
                .load_review(&task.id, &format!("v{}-{attempt}", task.current_iteration))?;
            if task.history.iter().any(|h| h.review == r.id) {
                continue;
            }
            // `caller_is_admin` only matters for the manual channel, and a manual record can
            // only have been written by the local administrator command (HTTP refuses it), so
            // a stored manual record is the administrator's. Other channels ignore the flag.
            let eligible =
                review_eligibility(&task.implementer_family, r.channel, &r.family, true).is_ok();
            if r.verdict == Verdict::Approved
                && r.tree == tree
                && r.base == base
                && r.channel.strength() >= task.min_reviewer.strength()
                && eligible
            {
                return Ok(Some(r.id));
            }
        }
        Ok(None)
    }

    /// Applies a review record. `commit`/`tag` are required when the review approved.
    pub fn record(
        &self,
        who: &Identity,
        id: &str,
        review_id: &str,
        commit: Option<&str>,
        tag: Option<&str>,
    ) -> Result<Recorded, GateError> {
        let mut task = self.store.load_task(id)?;
        self.ensure_owner(&task, who)?;
        if task.status != Status::Running {
            return Err(GateError::NotRunning(task.id.clone(), task.status));
        }
        let r = self.store.load_review(id, review_id)?;
        // Legacy records are not grandfathered: an external record whose family is unknown or
        // the implementer's own cannot finalize a version. Signed history still verifies.
        review_eligibility(&task.implementer_family, r.channel, &r.family, true).map_err(|e| {
            GateError::Mismatch(format!(
                "review {} cannot be finalized under implementer neutrality: {e}",
                r.id
            ))
        })?;
        if r.iteration != task.current_iteration {
            return Err(GateError::Mismatch(format!(
                "review {} is for version {}, task is at version {}",
                r.id, r.iteration, task.current_iteration
            )));
        }
        if task.history.iter().any(|h| h.review == r.id) {
            return Err(GateError::Mismatch(format!(
                "review {} was already used",
                r.id
            )));
        }
        let now = Utc::now();
        let entry = |outcome, commit: Option<String>, tag: Option<String>| HistoryEntry {
            iteration: task.current_iteration,
            outcome,
            review: r.id.clone(),
            channel: r.channel,
            commit,
            tag,
            attestation: None,
            at: now,
        };

        // Strength gate: a weak channel pauses for a human, it is not a rejection.
        if r.channel.strength() < task.min_reviewer.strength() {
            task.history
                .push(entry(Outcome::InsufficientReviewer, None, None));
            task.status = Status::Paused;
            task.stop_reason = Some(format!(
                "审核通道强度不足：{} < 门槛 {}——需要外部 AI 或人工审核",
                r.channel, task.min_reviewer
            ));
            task.updated_at = now;
            self.store.save_task(&task)?;
            self.trace(&task, task.stop_reason.as_deref().unwrap_or_default());
            return Ok(Recorded {
                action: Action::Pause,
                task,
            });
        }

        match r.verdict {
            Verdict::Approved => {
                let (commit, tag) = match (commit, tag) {
                    (Some(c), Some(t)) => (c, t),
                    _ => {
                        return Err(GateError::Invalid(
                            "approved needs --commit and --tag".into(),
                        ));
                    }
                };
                let repo = Path::new(&task.repo);
                let commit_id = git::rev_parse(repo, &format!("{commit}^{{commit}}"))?;
                let tree = git::rev_parse(repo, &format!("{commit_id}^{{tree}}"))?;
                if tree != r.tree {
                    return Err(GateError::Mismatch(format!(
                        "提交内容与被审内容不一致：commit tree {tree} ≠ 审核 tree {}",
                        r.tree
                    )));
                }
                // A version is exactly one commit on top of the reviewed base: no merges,
                // no root commits (`commit^` alone would accept a merge whose first parent is base).
                let parents = git::parents(repo, &commit_id)?;
                if parents.len() != 1 {
                    return Err(GateError::Mismatch(format!(
                        "提交 {commit_id} 有 {} 个父提交，每一版必须恰好是 base 之上的一个普通提交",
                        parents.len()
                    )));
                }
                if parents[0] != r.base {
                    return Err(GateError::Mismatch(format!(
                        "提交的父提交 {} ≠ 审核时的 base {}",
                        parents[0], r.base
                    )));
                }
                let tagged = git::rev_parse(repo, &format!("{tag}^{{commit}}"))?;
                if tagged != commit_id {
                    return Err(GateError::Mismatch(format!(
                        "tag {tag} 没有指向 {commit_id}"
                    )));
                }
                let mut e = entry(
                    Outcome::Approved,
                    Some(commit_id.clone()),
                    Some(tag.to_string()),
                );
                e.attestation = self.signer.as_ref().map(|kp| {
                    attest::sign(
                        kp,
                        &Statement {
                            task: task.id.clone(),
                            version: format!("{}/{}", task.current_iteration, task.iterations),
                            commit: commit_id.clone(),
                            tree: tree.clone(),
                            parent: parents[0].clone(),
                            review: r.id.clone(),
                            reviewer: format!("{}/{}", r.channel.as_str(), r.provider),
                            verdict: "approved".into(),
                            at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        },
                    )
                });
                let note = format!(
                    "第 {}/{} 版通过：commit {commit_id} · tag {tag} · 审核 {} {}{}",
                    task.current_iteration,
                    task.iterations,
                    r.channel,
                    r.id,
                    if e.attestation.is_some() {
                        " · 已签名证明"
                    } else {
                        ""
                    }
                );
                task.history.push(e);
                task.reject_streak = 0;
                task.current_iteration += 1;
                let action = if task.current_iteration > task.iterations {
                    task.status = Status::Done;
                    Action::Finalize
                } else {
                    Action::StartRound
                };
                task.updated_at = now;
                self.store.save_task(&task)?;
                self.trace(&task, &note);
                Ok(Recorded { action, task })
            }
            Verdict::Rejected => {
                task.history.push(entry(Outcome::Rejected, None, None));
                task.reject_streak += 1;
                let action = if task.reject_streak >= REJECT_STREAK_LIMIT {
                    task.status = Status::Paused;
                    task.stop_reason = Some(format!(
                        "第 {} 版连续 {} 次审核打回，自动熔断",
                        task.current_iteration, task.reject_streak
                    ));
                    Action::Pause
                } else {
                    Action::RetrySameRound
                };
                task.updated_at = now;
                self.store.save_task(&task)?;
                self.trace(
                    &task,
                    &format!(
                        "第 {} 版打回（{}，连续第 {} 次）{}",
                        task.current_iteration,
                        r.id,
                        task.reject_streak,
                        task.stop_reason
                            .as_deref()
                            .map(|s| format!("——{s}"))
                            .unwrap_or_default()
                    ),
                );
                Ok(Recorded { action, task })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use std::fs;

    fn legacy() -> Identity {
        Identity::legacy()
    }
    use std::path::PathBuf;

    struct Env {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        gate: Gate,
    }

    fn sh(repo: &Path, args: &[&str]) -> String {
        git::git(repo, args).unwrap()
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        sh(&repo, &["init", "-q"]);
        sh(&repo, &["config", "user.email", "t@t"]);
        sh(&repo, &["config", "user.name", "t"]);
        fs::write(repo.join("a.txt"), "v0\n").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "base"]);
        let gate = Gate::new(Store::open(dir.path().join("state")).unwrap());
        Env {
            _dir: dir,
            repo,
            gate,
        }
    }

    fn init(e: &Env, id: &str, n: u32, min: Channel) -> Task {
        e.gate
            .init(NewTask {
                id: id.into(),
                principal: crate::identity::LEGACY_PRINCIPAL.into(),
                implementer_family: crate::identity::LEGACY_FAMILY.into(),
                credential: "test".into(),
                config_digest: None,
                claimed_family: None,
                goal: "g".into(),
                final_acceptance: "a".into(),
                repo: e.repo.to_string_lossy().into(),
                iterations: n,
                min_reviewer: min,
                review_provider: "auto".into(),
            })
            .unwrap()
    }

    /// Writes content, stages it and stores a review of the staged tree.
    fn review(e: &Env, id: &str, content: &str, v: Verdict, ch: Channel) -> ReviewRecord {
        fs::write(e.repo.join("a.txt"), content).unwrap();
        let task = e.gate.store().load_task(id).unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let input = ReviewInput {
            tree,
            base,
            verdict: v,
            channel: ch,
            provider: "test".into(),
            model: None,
            family: "gemini".into(),
            prompt_sha: String::new(),
            text: "ok".into(),
        };
        e.gate.add_review(&legacy(), &task, input).unwrap()
    }

    fn commit_tag(e: &Env, tag: &str) -> String {
        sh(&e.repo, &["commit", "-qm", tag]);
        sh(&e.repo, &["tag", tag]);
        sh(&e.repo, &["rev-parse", "HEAD"])
    }

    #[test]
    fn approval_requires_the_reviewed_tree() {
        let e = env();
        init(&e, "t", 1, Channel::WebGemini);
        let r = review(&e, "t", "v1\n", Verdict::Approved, Channel::WebGemini);
        // change content after the review, then commit: rejected
        fs::write(e.repo.join("a.txt"), "v1-sneaky\n").unwrap();
        sh(&e.repo, &["add", "-A"]);
        let bad = commit_tag(&e, "bad");
        let err = e
            .gate
            .record(&legacy(), "t", &r.id, Some(&bad), Some("bad"))
            .unwrap_err();
        assert!(
            err.to_string().contains("提交内容与被审内容不一致"),
            "{err}"
        );
        // back to the reviewed content: accepted and finalized
        sh(&e.repo, &["reset", "-q", "--hard", "HEAD~1"]);
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        sh(&e.repo, &["add", "-A"]);
        let good = commit_tag(&e, "good");
        let out = e
            .gate
            .record(&legacy(), "t", &r.id, Some(&good), Some("good"))
            .unwrap();
        assert_eq!(out.action, Action::Finalize);
        assert_eq!(out.task.status, Status::Done);
        assert_eq!(out.task.history[0].commit.as_deref(), Some(good.as_str()));
    }

    #[test]
    fn approval_checks_parent_and_tag_and_needs_both() {
        let e = env();
        init(&e, "t", 2, Channel::WebGemini);
        let r = review(&e, "t", "v1\n", Verdict::Approved, Channel::WebGemini);
        assert!(matches!(
            e.gate.record(&legacy(), "t", &r.id, None, None),
            Err(GateError::Invalid(_))
        ));
        let c = commit_tag(&e, "v1");
        sh(&e.repo, &["tag", "elsewhere", "HEAD~1"]);
        let err = e
            .gate
            .record(&legacy(), "t", &r.id, Some(&c), Some("elsewhere"))
            .unwrap_err();
        assert!(err.to_string().contains("没有指向"), "{err}");
        let out = e
            .gate
            .record(&legacy(), "t", &r.id, Some(&c), Some("v1"))
            .unwrap();
        assert_eq!(
            (out.action, out.task.current_iteration),
            (Action::StartRound, 2)
        );
    }

    #[test]
    fn merge_commits_are_not_versions() {
        let e = env();
        init(&e, "t", 1, Channel::WebGemini);
        let base = sh(&e.repo, &["rev-parse", "HEAD"]);
        sh(&e.repo, &["checkout", "-q", "-b", "side"]);
        fs::write(e.repo.join("side.txt"), "s\n").unwrap();
        sh(&e.repo, &["add", "-A"]);
        sh(&e.repo, &["commit", "-qm", "side"]);
        sh(&e.repo, &["checkout", "-q", "-"]);
        assert_eq!(sh(&e.repo, &["rev-parse", "HEAD"]), base);
        // review the merged content, then commit it as a merge whose first parent is base
        sh(&e.repo, &["merge", "-q", "--no-ff", "--no-commit", "side"]);
        let task = e.gate.store().load_task("t").unwrap();
        let (tree, b) = git::stage_all(&e.repo).unwrap();
        let input = ReviewInput {
            tree,
            base: b,
            verdict: Verdict::Approved,
            channel: Channel::WebGemini,
            provider: "test".into(),
            model: None,
            family: "gemini".into(),
            prompt_sha: String::new(),
            text: "ok".into(),
        };
        let r = e.gate.add_review(&legacy(), &task, input).unwrap();
        let merge = commit_tag(&e, "m1");
        let err = e
            .gate
            .record(&legacy(), "t", &r.id, Some(&merge), Some("m1"))
            .unwrap_err();
        assert!(err.to_string().contains("有 2 个父提交"), "{err}");
    }

    #[test]
    fn records_are_single_use_and_bound_to_the_version() {
        let e = env();
        init(&e, "t", 3, Channel::WebGemini);
        let r = review(&e, "t", "v1\n", Verdict::Rejected, Channel::WebGemini);
        assert_eq!(
            e.gate
                .record(&legacy(), "t", &r.id, None, None)
                .unwrap()
                .action,
            Action::RetrySameRound
        );
        let again = e
            .gate
            .record(&legacy(), "t", &r.id, None, None)
            .unwrap_err();
        assert!(again.to_string().contains("already used"));
        assert!(matches!(
            e.gate.record(&legacy(), "t", "v9-1", None, None),
            Err(GateError::NoSuchReview(_))
        ));
        assert!(matches!(
            e.gate.record(&legacy(), "t", "../../x", None, None),
            Err(GateError::InvalidId(_))
        ));
    }

    #[test]
    fn weak_channel_pauses_without_counting_as_rejection() {
        let e = env();
        init(&e, "t", 2, Channel::WebGemini);
        let r = review(&e, "t", "v1\n", Verdict::Approved, Channel::ClaudeSubagent);
        let out = e
            .gate
            .record(&legacy(), "t", &r.id, Some("HEAD"), Some("x"))
            .unwrap();
        assert_eq!(out.action, Action::Pause);
        assert_eq!(out.task.reject_streak, 0);
        assert!(out.task.stop_reason.unwrap().contains("强度不足"));
        assert_eq!(out.task.history[0].outcome, Outcome::InsufficientReviewer);
        // paused tasks accept nothing more
        let r2 = e.gate.store().load_review("t", &r.id).unwrap();
        assert!(matches!(
            e.gate.record(&legacy(), "t", &r2.id, None, None),
            Err(GateError::NotRunning(..))
        ));
    }

    #[test]
    fn explicitly_lowered_threshold_accepts_a_subagent_review() {
        let e = env();
        init(&e, "t", 1, Channel::ClaudeSubagent);
        let r = review(&e, "t", "v1\n", Verdict::Approved, Channel::ClaudeSubagent);
        let c = commit_tag(&e, "v1");
        assert_eq!(
            e.gate
                .record(&legacy(), "t", &r.id, Some(&c), Some("v1"))
                .unwrap()
                .action,
            Action::Finalize
        );
    }

    #[test]
    fn three_rejections_trip_the_breaker_and_approval_resets_the_streak() {
        let e = env();
        init(&e, "t", 2, Channel::WebGemini);
        for i in 0..2 {
            let r = review(
                &e,
                "t",
                &format!("r{i}\n"),
                Verdict::Rejected,
                Channel::WebGemini,
            );
            e.gate.record(&legacy(), "t", &r.id, None, None).unwrap();
        }
        let r = review(&e, "t", "v1\n", Verdict::Approved, Channel::WebGemini);
        let c = commit_tag(&e, "v1");
        assert_eq!(
            e.gate
                .record(&legacy(), "t", &r.id, Some(&c), Some("v1"))
                .unwrap()
                .task
                .reject_streak,
            0
        );
        let mut last = None;
        for i in 0..3 {
            let r = review(
                &e,
                "t",
                &format!("x{i}\n"),
                Verdict::Rejected,
                Channel::WebGemini,
            );
            last = Some(e.gate.record(&legacy(), "t", &r.id, None, None).unwrap());
        }
        let last = last.unwrap();
        assert_eq!(last.action, Action::Pause);
        assert!(last.task.stop_reason.unwrap().contains("第 2 版连续 3 次"));
    }

    #[test]
    fn review_ids_number_attempts_per_version() {
        let e = env();
        init(&e, "t", 1, Channel::WebGemini);
        assert_eq!(
            review(&e, "t", "a\n", Verdict::Rejected, Channel::WebGemini).id,
            "v1-1"
        );
        assert_eq!(
            review(&e, "t", "b\n", Verdict::Rejected, Channel::WebGemini).id,
            "v1-2"
        );
    }

    #[test]
    fn init_validates_input() {
        let e = env();
        let base = |id: &str, n: u32| NewTask {
            id: id.into(),
            principal: crate::identity::LEGACY_PRINCIPAL.into(),
            implementer_family: crate::identity::LEGACY_FAMILY.into(),
            credential: "test".into(),
            config_digest: None,
            claimed_family: None,
            goal: "g".into(),
            final_acceptance: "a".into(),
            repo: e.repo.to_string_lossy().into(),
            iterations: n,
            min_reviewer: Channel::WebGemini,
            review_provider: "auto".into(),
        };
        assert!(matches!(
            e.gate.init(base("../evil", 1)),
            Err(GateError::InvalidId(_))
        ));
        assert!(matches!(
            e.gate.init(base("t", 0)),
            Err(GateError::Invalid(_))
        ));
        assert!(matches!(
            e.gate.init(base("t", 11)),
            Err(GateError::Invalid(_))
        ));
        e.gate.init(base("t", 1)).unwrap();
        assert!(matches!(
            e.gate.init(base("t", 1)),
            Err(GateError::Conflict(_))
        ));
        let mut not_repo = base("u", 1);
        not_repo.repo = e._dir.path().to_string_lossy().into();
        assert!(
            e.gate.init(not_repo).is_err(),
            "must be a git repository with a HEAD"
        );
    }

    #[test]
    fn channel_names_and_strengths() {
        assert_eq!(Channel::parse("web-gemini"), Some(Channel::WebGemini));
        assert_eq!(Channel::parse("nope"), None);
        assert!(Channel::Manual.strength() > Channel::ExternalApi.strength());
        assert!(Channel::WebGemini.strength() > Channel::ClaudeSubagent.strength());
        assert_eq!(Channel::ExternalApi.to_string(), "external-api(4)");
    }

    #[test]
    fn check_commit_needs_an_unused_approved_review_of_the_staged_tree() {
        let e = env();
        let repo = e.repo.to_string_lossy().to_string();
        let head = sh(&e.repo, &["rev-parse", "HEAD"]).trim().to_string();
        let free = e.gate.check_commit(&legacy(), &repo, "x", &head).unwrap();
        assert!(
            free.allowed && free.task.is_none(),
            "no task: anything goes"
        );

        init(&e, "t", 2, Channel::WebGemini);
        let rej = review(&e, "t", "v1\n", Verdict::Rejected, Channel::WebGemini);
        let c = e
            .gate
            .check_commit(&legacy(), &repo, &rej.tree, &rej.base)
            .unwrap();
        assert!(!c.allowed && c.task.as_deref() == Some("t"));
        let weak = review(&e, "t", "v1\n", Verdict::Approved, Channel::SelfReview);
        assert!(
            !e.gate
                .check_commit(&legacy(), &repo, &weak.tree, &weak.base)
                .unwrap()
                .allowed,
            "too weak"
        );
        let ok = review(&e, "t", "v1\n", Verdict::Approved, Channel::WebGemini);
        let c = e
            .gate
            .check_commit(&legacy(), &repo, &ok.tree, &ok.base)
            .unwrap();
        assert!(
            c.allowed && c.review.as_deref() == Some(ok.id.as_str()),
            "{c:?}"
        );
        assert!(
            !e.gate
                .check_commit(&legacy(), &repo, &rej.base, &rej.base)
                .unwrap()
                .allowed,
            "other tree"
        );

        sh(&e.repo, &["commit", "-qm", "v1"]);
        sh(&e.repo, &["tag", "v1"]);
        e.gate
            .record(&legacy(), "t", &ok.id, Some("HEAD"), Some("v1"))
            .unwrap();
        assert!(
            !e.gate
                .check_commit(&legacy(), &repo, &ok.tree, &ok.base)
                .unwrap()
                .allowed,
            "used review"
        );
    }
    fn who(principal: &str, family: &str) -> Identity {
        Identity::new(principal, family)
    }

    fn init_as(e: &Env, id: &str, principal: &str, family: &str, claimed: Option<&str>) -> Task {
        e.gate
            .init(NewTask {
                id: id.into(),
                principal: principal.into(),
                implementer_family: family.into(),
                credential: "test".into(),
                config_digest: None,
                claimed_family: claimed.map(str::to_string),
                goal: "g".into(),
                final_acceptance: "a".into(),
                repo: e.repo.to_string_lossy().into(),
                iterations: 1,
                min_reviewer: Channel::WebGemini,
                review_provider: "auto".into(),
            })
            .unwrap()
    }

    #[test]
    fn init_freezes_the_authenticated_principal_and_family() {
        let e = env();
        let task = init_as(&e, "t", "opencode", "deepseek", Some("DeepSeek"));
        assert_eq!(task.principal, "opencode");
        assert_eq!(task.implementer_family, "deepseek");

        // A self-report is only a consistency check, never the source of truth.
        let err = e
            .gate
            .init(NewTask {
                id: "u".into(),
                principal: "opencode".into(),
                implementer_family: "deepseek".into(),
                credential: "test".into(),
                config_digest: None,
                claimed_family: Some("openai".into()),
                goal: "g".into(),
                final_acceptance: "a".into(),
                repo: e.repo.to_string_lossy().into(),
                iterations: 1,
                min_reviewer: Channel::WebGemini,
                review_provider: "auto".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");

        let err = e
            .gate
            .init(NewTask {
                id: "v".into(),
                principal: "opencode".into(),
                implementer_family: "openai-compatible".into(),
                credential: "test".into(),
                config_digest: None,
                claimed_family: None,
                goal: "g".into(),
                final_acceptance: "a".into(),
                repo: e.repo.to_string_lossy().into(),
                iterations: 1,
                min_reviewer: Channel::WebGemini,
                review_provider: "auto".into(),
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown implementer family"),
            "{err}"
        );
    }

    #[test]
    fn another_principal_cannot_read_write_review_or_record_the_task() {
        let e = env();
        init_as(&e, "t", "opencode", "deepseek", None);
        let owner = who("opencode", "deepseek");
        let other = who("claude", "claude");
        let task = e.gate.store().load_task("t").unwrap();

        assert!(e.gate.ensure_owner(&task, &owner).is_ok());
        assert!(matches!(
            e.gate.ensure_owner(&task, &other),
            Err(GateError::Forbidden(_))
        ));
        assert!(matches!(
            e.gate.link(&other, "t", "x"),
            Err(GateError::Forbidden(_))
        ));
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let input = ReviewInput {
            tree,
            base,
            verdict: Verdict::Approved,
            channel: Channel::WebGemini,
            provider: "web-gemini".into(),
            model: None,
            family: "gemini".into(),
            prompt_sha: String::new(),
            text: "VERDICT: APPROVED".into(),
        };
        assert!(matches!(
            e.gate.add_review(&other, &task, input),
            Err(GateError::Forbidden(_))
        ));
        assert!(matches!(
            e.gate.record(&other, "t", "v1-1", None, None),
            Err(GateError::Forbidden(_))
        ));

        // The hook must not hide another principal's running task and let the commit through.
        let repo = e.repo.to_string_lossy().to_string();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let unseen = e.gate.check_commit(&other, &repo, &tree, &base).unwrap();
        assert!(!unseen.allowed, "{unseen:?}");
        assert!(
            unseen.task.is_none(),
            "a non-owner must not see the task's content"
        );

        // The owner passes with an eligible approval …
        let owner_ok = e
            .gate
            .add_review(
                &owner,
                &task,
                ReviewInput {
                    tree: tree.clone(),
                    base: base.clone(),
                    verdict: Verdict::Approved,
                    channel: Channel::WebGemini,
                    provider: "web-gemini".into(),
                    model: None,
                    family: "gemini".into(),
                    prompt_sha: String::new(),
                    text: "VERDICT: APPROVED".into(),
                },
            )
            .unwrap();
        let seen_by_owner = e.gate.check_commit(&owner, &repo, &tree, &base).unwrap();
        assert!(
            seen_by_owner.allowed && seen_by_owner.review.as_deref() == Some(owner_ok.id.as_str()),
            "{seen_by_owner:?}"
        );
        // … but another principal cannot borrow the owner's approval.
        let borrowed = e.gate.check_commit(&other, &repo, &tree, &base).unwrap();
        assert!(!borrowed.allowed, "{borrowed:?}");
        assert!(borrowed.task.is_none() && borrowed.review.is_none());
    }

    #[test]
    fn a_task_without_a_principal_has_no_owner_and_is_never_given_one() {
        let e = env();
        let repo = e.repo.to_string_lossy().to_string();
        let mut task = init_as(&e, "t", "legacy-claude", "claude", None);
        task.principal.clear();
        e.gate.store().save_task(&task).unwrap();
        let loaded = e.gate.store().load_task("t").unwrap();
        assert_eq!(loaded.owner_principal(), None);
        // Neither the legacy credential nor anyone else can write it; only the administrator
        // can read it.
        for caller in [
            legacy(),
            who("claude", "claude"),
            who("opencode", "deepseek"),
        ] {
            let err = e.gate.ensure_owner(&loaded, &caller).unwrap_err();
            assert!(err.to_string().contains("no known owner"), "{err}");
            assert!(e.gate.link(&caller, "t", "x").is_err());
        }
        // The administrator may read it, but not change it (Codex review of the integration).
        assert!(e.gate.can_read(&loaded, &Identity::admin()));
        assert!(e.gate.ensure_owner(&loaded, &Identity::admin()).is_err());
        // And it blocks commits on its repository for everyone (no borrowing, no hiding).
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let c = e.gate.check_commit(&legacy(), &repo, &tree, &base).unwrap();
        assert!(!c.allowed && c.task.is_none(), "{c:?}");
        assert!(c.reason.contains("no known owner"), "{c:?}");
    }

    #[test]
    fn an_unowned_task_gets_its_first_owner_only_by_an_explicit_assignment() {
        let e = env();
        let mut task = init_as(&e, "t", "legacy-claude", "claude", None);
        task.principal.clear();
        e.gate.store().save_task(&task).unwrap();
        let listed = e.gate.store().load_task("t").unwrap();
        let at = listed.updated_at;
        // Writes need the exclusive state lock (the service stopped).
        let lock = e.gate.store().try_service_lock().unwrap().unwrap();
        // Refusals: no reason, a stale listing, an invalid principal.
        assert!(
            e.gate
                .assign_owner("t", "opencode", "  ", at, Some(&lock))
                .is_err()
        );
        let stale = at - chrono::Duration::seconds(1);
        let err = e
            .gate
            .assign_owner("t", "opencode", "user decided", stale, Some(&lock))
            .unwrap_err();
        assert!(
            err.to_string().contains("changed since it was listed"),
            "{err}"
        );
        assert!(
            e.gate
                .assign_owner("t", "bad id", "r", at, Some(&lock))
                .is_err()
        );
        // A dry run shows the result and writes nothing.
        let planned = e
            .gate
            .assign_owner("t", "opencode", "user decided", at, None)
            .unwrap();
        assert_eq!(planned.principal, "opencode");
        assert_eq!(
            e.gate.store().load_task("t").unwrap(),
            listed,
            "dry run wrote nothing"
        );
        // The write: owner and audit in the same task file.
        let done = e
            .gate
            .assign_owner("t", "opencode", "user decided", at, Some(&lock))
            .unwrap();
        let on_file = e.gate.store().load_task("t").unwrap();
        assert_eq!(on_file, done);
        assert_eq!(on_file.owner_principal(), Some("opencode"));
        let audit = on_file.owner_assigned.clone().unwrap();
        assert_eq!(
            (
                audit.principal.as_str(),
                audit.by.as_str(),
                audit.reason.as_str()
            ),
            ("opencode", "admin", "user decided")
        );
        // Once only: no reassignment, even with the current revision.
        let err = e
            .gate
            .assign_owner("t", "claude", "again", on_file.updated_at, Some(&lock))
            .unwrap_err();
        assert!(
            err.to_string().contains("already belongs to opencode"),
            "{err}"
        );
        // From now on only the new owner writes it.
        assert!(e.gate.link(&who("opencode", "deepseek"), "t", "x").is_ok());
        assert!(e.gate.link(&legacy(), "t", "y").is_err());
        // A task that always had an owner cannot be assigned either.
        let owned = init_as(&e, "u", "claude", "claude", None);
        assert!(
            e.gate
                .assign_owner("u", "opencode", "r", owned.updated_at, Some(&lock))
                .is_err()
        );
    }

    #[test]
    fn nobody_writes_a_task_with_no_known_owner_not_even_an_administrator() {
        let e = env();
        let repo = e.repo.to_string_lossy().to_string();
        let mut task = init_as(&e, "t", "claude", "claude", None);
        task.principal.clear();
        e.gate.store().save_task(&task).unwrap();
        let unowned = e.gate.store().load_task("t").unwrap();
        // An administrator principal from the identity configuration (over HTTP) and the local
        // administrator command.
        let mut http_admin = who("ops", "claude");
        http_admin.admin = true;
        for caller in [http_admin.clone(), Identity::admin()] {
            assert!(
                e.gate.can_read(&unowned, &caller),
                "an administrator reads it"
            );
            let err = e.gate.ensure_owner(&unowned, &caller).unwrap_err();
            assert!(err.to_string().contains("no known owner"), "{err}");
            assert!(e.gate.link(&caller, "t", "x").is_err());
            assert!(e.gate.record(&caller, "t", "v1-1", None, None).is_err());
        }
        // No review can be added, so no approval exists to borrow; and /check refuses.
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let input = || ReviewInput {
            tree: tree.clone(),
            base: base.clone(),
            verdict: Verdict::Approved,
            channel: Channel::WebGemini,
            provider: "web-gemini".into(),
            model: None,
            family: "gemini".into(),
            prompt_sha: String::new(),
            text: "VERDICT: APPROVED".into(),
        };
        assert!(e.gate.add_review(&http_admin, &unowned, input()).is_err());
        assert!(
            e.gate
                .add_review(&Identity::admin(), &unowned, input())
                .is_err()
        );
        assert_eq!(
            e.gate.store().review_count("t", 1).unwrap(),
            0,
            "nothing was stored"
        );
        for caller in [http_admin.clone(), Identity::admin()] {
            let c = e.gate.check_commit(&caller, &repo, &tree, &base).unwrap();
            assert!(!c.allowed, "{c:?}");
        }
        assert_eq!(
            e.gate.store().load_task("t").unwrap(),
            unowned,
            "the task is unchanged"
        );
    }

    #[test]
    fn an_old_approval_on_a_task_with_no_known_owner_is_never_borrowed() {
        // A task the old binary created and whose version it reviewed (the review is on file),
        // then read by this version with no principal: nobody may commit under it, an
        // administrator included.
        let e = env();
        let repo = e.repo.to_string_lossy().to_string();
        let owner = who("claude", "claude");
        let task = init_as(&e, "t", "claude", "claude", None);
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        e.gate
            .add_review(
                &owner,
                &task,
                ReviewInput {
                    tree: tree.clone(),
                    base: base.clone(),
                    verdict: Verdict::Approved,
                    channel: Channel::WebGemini,
                    provider: "web-gemini".into(),
                    model: None,
                    family: "gemini".into(),
                    prompt_sha: String::new(),
                    text: "VERDICT: APPROVED".into(),
                },
            )
            .unwrap();
        let mut t = e.gate.store().load_task("t").unwrap();
        t.principal.clear();
        e.gate.store().save_task(&t).unwrap();
        let mut http_admin = who("ops", "claude");
        http_admin.admin = true;
        for caller in [http_admin, Identity::admin(), owner, legacy()] {
            let c = e.gate.check_commit(&caller, &repo, &tree, &base).unwrap();
            assert!(!c.allowed && c.review.is_none(), "{c:?}");
        }
    }

    #[test]
    fn an_administrator_over_http_has_no_write_exemption_on_another_principals_task() {
        let e = env();
        let owned = init_as(&e, "t", "claude", "claude", None);
        let mut http_admin = who("ops", "claude");
        http_admin.admin = true;
        assert!(
            e.gate.ensure_readable(&owned, &http_admin).is_ok(),
            "it may read"
        );
        assert!(e.gate.link(&http_admin, "t", "x").is_err(), "but not write");
        // The local administrator command may still act on a task with a known owner (the
        // manual channel is recorded that way).
        assert!(e.gate.link(&Identity::admin(), "t", "y").is_ok());
        assert!(!http_admin.local_admin && Identity::admin().local_admin);
    }

    #[test]
    fn the_service_lock_has_one_holder() {
        let e = env();
        let held = e.gate.store().try_service_lock().unwrap();
        assert!(held.is_some());
        assert!(
            e.gate.store().try_service_lock().unwrap().is_none(),
            "a second holder is refused"
        );
        drop(held);
        // A test thread forking at that moment keeps the open file description (and its lock)
        // until its child execs, so the release is observed within a short bound.
        let mut again = None;
        for _ in 0..50 {
            again = e.gate.store().try_service_lock().unwrap();
            if again.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(again.is_some(), "free again once released");
    }

    #[test]
    fn an_approval_under_any_of_the_callers_running_tasks_covers_the_commit() {
        let e = env();
        let repo = e.repo.to_string_lossy().to_string();
        let owner = who("claude", "claude");
        let first = init_as(&e, "first", "claude", "claude", None);
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let r = e
            .gate
            .add_review(
                &owner,
                &first,
                ReviewInput {
                    tree: tree.clone(),
                    base: base.clone(),
                    verdict: Verdict::Approved,
                    channel: Channel::WebGemini,
                    provider: "web-gemini".into(),
                    model: None,
                    family: "gemini".into(),
                    prompt_sha: String::new(),
                    text: "VERDICT: APPROVED".into(),
                },
            )
            .unwrap();
        // A second running task of the same principal, updated later (listed first), with no
        // review: the approval under the first one still covers the commit.
        std::thread::sleep(std::time::Duration::from_millis(5));
        init_as(&e, "second", "claude", "claude", None);
        let c = e.gate.check_commit(&owner, &repo, &tree, &base).unwrap();
        assert!(c.allowed, "{c:?}");
        assert_eq!(
            (c.task.as_deref(), c.review.as_deref()),
            (Some("first"), Some(r.id.as_str()))
        );
        // Another tree: refused, naming both tasks, without picking one.
        let other = e.gate.check_commit(&owner, &repo, "0000", &base).unwrap();
        assert!(!other.allowed && other.task.is_none(), "{other:?}");
        assert!(
            other.reason.contains("first") && other.reason.contains("second"),
            "{other:?}"
        );
    }

    #[test]
    fn a_commit_is_refused_when_another_principal_also_runs_a_task_on_the_repo() {
        let e = env();
        let repo = e.repo.to_string_lossy().to_string();
        let mine = init_as(&e, "mine", "claude", "claude", None);
        let owner = who("claude", "claude");
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        e.gate
            .add_review(
                &owner,
                &mine,
                ReviewInput {
                    tree: tree.clone(),
                    base: base.clone(),
                    verdict: Verdict::Approved,
                    channel: Channel::WebGemini,
                    provider: "web-gemini".into(),
                    model: None,
                    family: "gemini".into(),
                    prompt_sha: String::new(),
                    text: "VERDICT: APPROVED".into(),
                },
            )
            .unwrap();
        let alone = e.gate.check_commit(&owner, &repo, &tree, &base).unwrap();
        assert!(alone.allowed, "{alone:?}");
        // Another principal starts a task on the same repository: the caller's own approval no
        // longer lets the commit through.
        init_as(&e, "theirs", "opencode", "deepseek", None);
        let both = e.gate.check_commit(&owner, &repo, &tree, &base).unwrap();
        assert!(!both.allowed && both.task.is_none(), "{both:?}");
        assert!(both.reason.contains("another principal"), "{both:?}");
    }

    #[test]
    fn external_reviews_must_come_from_another_known_family() {
        let e = env();
        init_as(&e, "t", "opencode", "deepseek", None);
        let owner = who("opencode", "deepseek");
        let task = e.gate.store().load_task("t").unwrap();
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let input = |ch, family: &str| ReviewInput {
            tree: tree.clone(),
            base: base.clone(),
            verdict: Verdict::Approved,
            channel: ch,
            provider: "p".into(),
            model: None,
            family: family.into(),
            prompt_sha: String::new(),
            text: "VERDICT: APPROVED".into(),
        };

        // Each external channel refuses the implementer's own vendor.
        for ch in [Channel::ExternalApi, Channel::WebGemini] {
            let err = e
                .gate
                .add_review(&owner, &task, input(ch, "deepseek"))
                .unwrap_err();
            assert!(
                err.to_string().contains("implementer's family"),
                "{ch}: {err}"
            );
            let err = e
                .gate
                .add_review(&owner, &task, input(ch, "openai-compatible"))
                .unwrap_err();
            assert!(
                err.to_string().contains("unknown reviewer family"),
                "{ch}: {err}"
            );
        }

        // A different, known vendor is accepted and recorded canonically.
        let mut gemini = input(Channel::WebGemini, "Google");
        gemini.family = "Google".into();
        let r = e.gate.add_review(&owner, &task, gemini).unwrap();
        assert_eq!(r.family, "gemini");
        assert_eq!(r.channel, Channel::WebGemini);

        // Weak channels stay in the implementer's family, with their existing strengths.
        let r = e
            .gate
            .add_review(&owner, &task, input(Channel::ClaudeSubagent, "gemini"))
            .unwrap();
        assert_eq!(r.family, "deepseek");
        assert_eq!(r.channel.strength(), 2);
        assert_eq!(Channel::SelfReview.strength(), 1);
    }

    #[test]
    fn check_commit_refuses_same_family_and_unknown_family_external_records() {
        let e = env();
        init_as(&e, "t", "opencode", "deepseek", None);
        let owner = who("opencode", "deepseek");
        let repo = e.repo.to_string_lossy().to_string();
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        for (attempt, family) in [(1u32, "deepseek"), (2, "openai-compatible")] {
            let r = ReviewRecord {
                id: format!("v1-{attempt}"),
                task: "t".into(),
                iteration: 1,
                attempt,
                tree: tree.clone(),
                base: base.clone(),
                verdict: Verdict::Approved,
                channel: Channel::ExternalApi,
                provider: "p".into(),
                model: None,
                family: family.into(),
                prompt_sha: String::new(),
                text: "VERDICT: APPROVED".into(),
                at: Utc::now(),
            };
            e.gate.store().save_review(&r).unwrap();
            let c = e.gate.check_commit(&owner, &repo, &tree, &base).unwrap();
            assert!(!c.allowed, "{family} must not authorize a commit: {c:?}");
        }
    }

    #[test]
    fn manual_is_admin_only_and_legacy_external_records_cannot_finalize() {
        let e = env();
        init_as(&e, "t", "legacy-claude", "claude", None);
        let task = e.gate.store().load_task("t").unwrap();
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let manual = |family: &str| ReviewInput {
            tree: tree.clone(),
            base: base.clone(),
            verdict: Verdict::Approved,
            channel: Channel::Manual,
            provider: "manual".into(),
            model: None,
            family: family.into(),
            prompt_sha: String::new(),
            text: "VERDICT: APPROVED".into(),
        };
        assert!(matches!(
            e.gate.add_review(&legacy(), &task, manual("human")),
            Err(GateError::Mismatch(_))
        ));
        assert!(
            e.gate
                .add_review(&Identity::admin(), &task, manual("human"))
                .is_ok()
        );

        // Two records written by an older gate: same vendor, and an unknown protocol family.
        for (id, family) in [("v1-9", "claude"), ("v1-10", "openai-compatible")] {
            let r = ReviewRecord {
                id: id.into(),
                task: "t".into(),
                iteration: 1,
                attempt: 9,
                tree: tree.clone(),
                base: base.clone(),
                verdict: Verdict::Approved,
                channel: Channel::ExternalApi,
                provider: "openai".into(),
                model: None,
                family: family.into(),
                prompt_sha: String::new(),
                text: "VERDICT: APPROVED".into(),
                at: Utc::now(),
            };
            e.gate.store().save_review(&r).unwrap();
            let err = e.gate.record(&legacy(), "t", id, None, None).unwrap_err();
            assert!(err.to_string().contains("cannot be finalized"), "{err}");
        }
        // Signed history is untouched by this rule: attestation verification has no channel.
    }
}
