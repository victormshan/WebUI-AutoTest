//! The state machine: version gate, reviewer-strength gate, tree binding, circuit breaker.
//!
//! The implementer never reports a verdict. It asks the gate to `record` a review record by id;
//! records are created only by the gate (from an external model's reply, or from a weaker
//! channel explicitly submitted), live in the gate's private store, and are single-use.

use std::path::Path;

use chrono::Utc;

use crate::model::{Action, Channel, HistoryEntry, Outcome, ReviewRecord, Status, Task, Verdict};
use crate::store::{Store, valid_id};
use crate::{GateError, git};

pub const REJECT_STREAK_LIMIT: u32 = 3;
pub const MAX_ITERATIONS: u32 = 10;

pub struct NewTask {
    pub id: String,
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

/// Result of recording a review against a task.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub action: Action,
    pub task: Task,
}

pub struct Gate {
    store: Store,
}

impl Gate {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn init(&self, t: NewTask) -> Result<Task, GateError> {
        if !valid_id(&t.id) {
            return Err(GateError::InvalidId(t.id));
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
            expr_id: None,
            goal: t.goal,
            final_acceptance: t.final_acceptance,
            repo: repo.to_string_lossy().into_owned(),
            iterations: t.iterations,
            current_iteration: 1,
            reject_streak: 0,
            min_reviewer: t.min_reviewer,
            review_provider: t.review_provider,
            implementer_family: "claude".into(),
            status: Status::Running,
            stop_reason: None,
            created_at: now,
            updated_at: now,
            history: Vec::new(),
        };
        self.store.save_task(&task)?;
        Ok(task)
    }

    pub fn link(&self, id: &str, expr_id: &str) -> Result<Task, GateError> {
        let mut task = self.store.load_task(id)?;
        task.expr_id = Some(expr_id.to_string());
        task.updated_at = Utc::now();
        self.store.save_task(&task)?;
        Ok(task)
    }

    pub fn pause(&self, id: &str, reason: &str) -> Result<Task, GateError> {
        let mut task = self.store.load_task(id)?;
        task.status = Status::Paused;
        task.stop_reason = Some(reason.to_string());
        task.updated_at = Utc::now();
        self.store.save_task(&task)?;
        Ok(task)
    }

    /// Stores a new review record for the task's current version. Only the gate calls this:
    /// from an external model's reply, or from an explicitly submitted weaker channel.
    pub fn add_review(&self, task: &Task, input: ReviewInput) -> Result<ReviewRecord, GateError> {
        if task.status != Status::Running {
            return Err(GateError::NotRunning(task.id.clone(), task.status));
        }
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
            family: input.family,
            prompt_sha: input.prompt_sha,
            text: input.text,
            at: Utc::now(),
        };
        self.store.save_review(&r)?;
        Ok(r)
    }

    /// Applies a review record. `commit`/`tag` are required when the review approved.
    pub fn record(
        &self,
        id: &str,
        review_id: &str,
        commit: Option<&str>,
        tag: Option<&str>,
    ) -> Result<Recorded, GateError> {
        let mut task = self.store.load_task(id)?;
        if task.status != Status::Running {
            return Err(GateError::NotRunning(task.id.clone(), task.status));
        }
        let r = self.store.load_review(id, review_id)?;
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
                task.history.push(entry(
                    Outcome::Approved,
                    Some(commit_id),
                    Some(tag.to_string()),
                ));
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
                Ok(Recorded { action, task })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
        e.gate.add_review(&task, input).unwrap()
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
            .record("t", &r.id, Some(&bad), Some("bad"))
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
            .record("t", &r.id, Some(&good), Some("good"))
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
            e.gate.record("t", &r.id, None, None),
            Err(GateError::Invalid(_))
        ));
        let c = commit_tag(&e, "v1");
        sh(&e.repo, &["tag", "elsewhere", "HEAD~1"]);
        let err = e
            .gate
            .record("t", &r.id, Some(&c), Some("elsewhere"))
            .unwrap_err();
        assert!(err.to_string().contains("没有指向"), "{err}");
        let out = e.gate.record("t", &r.id, Some(&c), Some("v1")).unwrap();
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
        let r = e.gate.add_review(&task, input).unwrap();
        let merge = commit_tag(&e, "m1");
        let err = e
            .gate
            .record("t", &r.id, Some(&merge), Some("m1"))
            .unwrap_err();
        assert!(err.to_string().contains("有 2 个父提交"), "{err}");
    }

    #[test]
    fn records_are_single_use_and_bound_to_the_version() {
        let e = env();
        init(&e, "t", 3, Channel::WebGemini);
        let r = review(&e, "t", "v1\n", Verdict::Rejected, Channel::WebGemini);
        assert_eq!(
            e.gate.record("t", &r.id, None, None).unwrap().action,
            Action::RetrySameRound
        );
        let again = e.gate.record("t", &r.id, None, None).unwrap_err();
        assert!(again.to_string().contains("already used"));
        assert!(matches!(
            e.gate.record("t", "v9-1", None, None),
            Err(GateError::NoSuchReview(_))
        ));
        assert!(matches!(
            e.gate.record("t", "../../x", None, None),
            Err(GateError::InvalidId(_))
        ));
    }

    #[test]
    fn weak_channel_pauses_without_counting_as_rejection() {
        let e = env();
        init(&e, "t", 2, Channel::WebGemini);
        let r = review(&e, "t", "v1\n", Verdict::Approved, Channel::ClaudeSubagent);
        let out = e.gate.record("t", &r.id, Some("HEAD"), Some("x")).unwrap();
        assert_eq!(out.action, Action::Pause);
        assert_eq!(out.task.reject_streak, 0);
        assert!(out.task.stop_reason.unwrap().contains("强度不足"));
        assert_eq!(out.task.history[0].outcome, Outcome::InsufficientReviewer);
        // paused tasks accept nothing more
        let r2 = e.gate.store().load_review("t", &r.id).unwrap();
        assert!(matches!(
            e.gate.record("t", &r2.id, None, None),
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
                .record("t", &r.id, Some(&c), Some("v1"))
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
            e.gate.record("t", &r.id, None, None).unwrap();
        }
        let r = review(&e, "t", "v1\n", Verdict::Approved, Channel::WebGemini);
        let c = commit_tag(&e, "v1");
        assert_eq!(
            e.gate
                .record("t", &r.id, Some(&c), Some("v1"))
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
            last = Some(e.gate.record("t", &r.id, None, None).unwrap());
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
}
