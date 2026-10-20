//! The state machine: version gate, reviewer-strength gate, tree binding, circuit breaker.
//!
//! The implementer never reports a verdict. It asks the gate to `record` a review record by id;
//! records are created only by the gate (from an external model's reply, or from a weaker
//! channel explicitly submitted), live in the gate's private store, and are single-use.

use std::path::Path;

use chrono::Utc;

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

    /// Whether `who` may read or change this task. The administrator may access every task;
    /// anybody else is confined to the principal frozen at creation.
    pub fn can_access(&self, task: &Task, who: &Identity) -> bool {
        who.admin || task.owner_principal() == who.principal
    }

    pub fn ensure_owner(&self, task: &Task, who: &Identity) -> Result<(), GateError> {
        if self.can_access(task, who) {
            Ok(())
        } else {
            Err(GateError::Forbidden(format!(
                "task {} belongs to principal {}, not {}",
                task.id,
                task.owner_principal(),
                who.principal
            )))
        }
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
        let Some(task) = running.into_iter().find(|t| self.can_access(t, who)) else {
            // The running task belongs to another principal. Do not hide it and let the
            // commit through, and do not leak its id or goal to the caller.
            return Ok(CommitCheck {
                allowed: false,
                task: None,
                review: None,
                reason: "a running task for this repository belongs to another principal; only that principal may commit under it".into(),
            });
        };
        let count = self.store.review_count(&task.id, task.current_iteration)?;
        for attempt in (1..=count).rev() {
            let r = self
                .store
                .load_review(&task.id, &format!("v{}-{attempt}", task.current_iteration))?;
            if task.history.iter().any(|h| h.review == r.id) {
                continue;
            }
            let eligible = review_eligibility(
                &task.implementer_family,
                r.channel,
                &r.family,
                true, // a manual record was created by the administrator
            )
            .is_ok();
            if r.verdict == Verdict::Approved
                && r.tree == tree
                && r.base == base
                && r.channel.strength() >= task.min_reviewer.strength()
                && eligible
            {
                return Ok(CommitCheck {
                    allowed: true,
                    reason: format!("covered by approved review {}", r.id),
                    review: Some(r.id),
                    task: Some(task.id),
                });
            }
        }
        Ok(CommitCheck {
            allowed: false,
            reason: format!(
                "task {} 第 {} 版没有覆盖当前暂存内容（tree {tree} on {base}）的通过审核：先审核，通过后原样提交",
                task.id, task.current_iteration
            ),
            task: Some(task.id),
            review: None,
        })
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
    fn tasks_without_a_principal_belong_only_to_legacy_claude() {
        let e = env();
        let mut task = init_as(&e, "t", "legacy-claude", "claude", None);
        task.principal.clear();
        e.gate.store().save_task(&task).unwrap();
        let loaded = e.gate.store().load_task("t").unwrap();
        assert_eq!(loaded.owner_principal(), "legacy-claude");
        assert!(e.gate.ensure_owner(&loaded, &legacy()).is_ok());
        assert!(matches!(
            e.gate.ensure_owner(&loaded, &who("opencode", "deepseek")),
            Err(GateError::Forbidden(_))
        ));
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
