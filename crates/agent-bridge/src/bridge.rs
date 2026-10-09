//! The task state machine. Every rule the two agents agreed on is checked here, before a message
//! is stored: who may send which kind, in which state, within which limits. A message the
//! machine refuses is never stored; a stored message always replays to the same state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::Utc;
use serde::Deserialize;

use crate::model::{Judgement, Kind, Message, Outcome, State, Task, TaskMeta};
use crate::store::Store;
use crate::{BridgeError, MAX_MESSAGES, MAX_QUESTION_ROUNDS, PROTOCOL, valid_id};

/// What a sender submits; the service fills in task, n, from and the time.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Draft {
    pub kind: Option<Kind>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub meta: Option<TaskMeta>,
    #[serde(default)]
    pub questions: Vec<crate::model::Question>,
    #[serde(default)]
    pub results: Vec<crate::model::ResultItem>,
    #[serde(default)]
    pub needs_user: Vec<crate::model::NeedsUser>,
    #[serde(default)]
    pub outcome: Option<Outcome>,
    #[serde(default)]
    pub judgement: Option<Judgement>,
    #[serde(default)]
    pub reply_to: Option<u32>,
    #[serde(default)]
    pub supersedes: Option<u32>,
    #[serde(default)]
    pub client_msg_id: Option<String>,
    #[serde(default)]
    pub session_epoch: Option<String>,
    #[serde(default)]
    pub wake: Option<crate::model::WakeLevel>,
    #[serde(default)]
    pub phase: Option<crate::model::Phase>,
    #[serde(default)]
    pub protocol: Option<String>,
}

struct Entry {
    task: Task,
    messages: Vec<Message>,
}

pub struct Bridge {
    store: Store,
    agents: BTreeSet<String>,
    tasks: BTreeMap<String, Entry>,
    /// Lines skipped while loading (corrupt, oversized, or no longer replayable).
    pub bad_lines: usize,
}

impl Bridge {
    /// Opens the store and replays every task. `agents` are the participants (e.g. claude, dsh).
    pub fn open(root: impl Into<PathBuf>, agents: &[&str]) -> Result<Self, BridgeError> {
        let store = Store::open(root)?;
        let agents: BTreeSet<String> = agents.iter().map(|a| a.to_string()).collect();
        for a in &agents {
            if !valid_id(a) {
                return Err(BridgeError::InvalidId(a.clone()));
            }
        }
        let mut tasks = BTreeMap::new();
        let mut bad_lines = 0;
        for id in store.task_ids()? {
            let loaded = store.load(&id)?;
            bad_lines += loaded.bad_lines;
            let mut iter = loaded.messages.into_iter();
            let Some(first) = iter.next() else { continue };
            let Ok(mut task) = open_task(&first, &agents) else {
                bad_lines += 1;
                continue;
            };
            if first.imported {
                // An archived file-protocol task: kept as a record, not replayed by the rules.
                let rest: Vec<Message> = iter.collect();
                task = archived(task, &rest);
                let mut messages = vec![first];
                messages.extend(rest);
                tasks.insert(id, Entry { task, messages });
                continue;
            }
            let mut messages = vec![first];
            for m in iter {
                let mut next = task.clone();
                if m.n == next.last_n + 1 && apply(&mut next, &messages, &m).is_ok() {
                    task = next;
                    messages.push(m);
                } else {
                    bad_lines += 1;
                }
            }
            tasks.insert(id, Entry { task, messages });
        }
        Ok(Self {
            store,
            agents,
            tasks,
            bad_lines,
        })
    }

    /// Whether the store holds message `n` of `task` from `from` (mirror self-check).
    pub fn knows(&self, task: &str, n: u32, from: &str) -> bool {
        self.tasks
            .get(task)
            .is_some_and(|e| e.messages.iter().any(|m| m.n == n && m.from == from))
    }

    /// Stores a finished file-protocol task as a closed archive (see `import.rs`). Idempotent:
    /// importing the same messages again is a no-op; different content under an existing id is
    /// refused.
    pub fn import_task(&mut self, msgs: Vec<Message>) -> Result<bool, BridgeError> {
        let first = msgs
            .first()
            .ok_or_else(|| BridgeError::Invalid("nothing to import".into()))?;
        let id = first.task.clone();
        if let Some(e) = self.tasks.get(&id) {
            let same = e.messages.len() == msgs.len()
                && e.messages
                    .iter()
                    .zip(&msgs)
                    .all(|(a, b)| a.n == b.n && a.from == b.from && a.body == b.body);
            return if same {
                Ok(false)
            } else {
                Err(BridgeError::Exists(id))
            };
        }
        if !msgs.iter().all(|m| m.imported && m.task == id) {
            return Err(BridgeError::Invalid(
                "only imported messages of one task".into(),
            ));
        }
        let task = open_task(first, &self.agents)?;
        let task = archived(task, &msgs[1..]);
        for (i, m) in msgs.iter().enumerate() {
            self.store.append(m, i == 0)?;
        }
        self.tasks.insert(
            id,
            Entry {
                task,
                messages: msgs,
            },
        );
        Ok(true)
    }

    /// The store's directory (other state, such as read cursors, lives next to the tasks).
    pub fn root(&self) -> &std::path::Path {
        self.store.root()
    }

    pub fn agents(&self) -> impl Iterator<Item = &str> {
        self.agents.iter().map(String::as_str)
    }

    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.get(id).map(|e| &e.task)
    }

    pub fn messages(&self, id: &str) -> Result<&[Message], BridgeError> {
        self.tasks
            .get(id)
            .map(|e| e.messages.as_slice())
            .ok_or_else(|| BridgeError::NoSuchTask(id.into()))
    }

    /// Tasks `agent` takes part in (all tasks if `None`), most recently updated first.
    pub fn list(&self, agent: Option<&str>) -> Vec<&Task> {
        let mut v: Vec<&Task> = self
            .tasks
            .values()
            .map(|e| &e.task)
            .filter(|t| agent.is_none_or(|a| t.from == a || t.to == a))
            .collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
        v
    }

    fn check_protocol(d: &Draft) -> Result<(), BridgeError> {
        match d.protocol.as_deref() {
            Some(p) if crate::PROTOCOLS.contains(&p) => Ok(()),
            other => Err(BridgeError::Invalid(format!(
                "protocol {other:?} not supported (this service speaks {:?})",
                crate::PROTOCOLS
            ))),
        }
    }

    fn check_agent(&self, a: &str) -> Result<(), BridgeError> {
        if self.agents.contains(a) {
            Ok(())
        } else {
            Err(BridgeError::UnknownAgent(a.into()))
        }
    }

    /// Opens a new task from `from`; the draft's `meta` names the receiver and title.
    pub fn create_task(&mut self, id: &str, from: &str, d: Draft) -> Result<Message, BridgeError> {
        if !valid_id(id) {
            return Err(BridgeError::InvalidId(id.into()));
        }
        Self::check_protocol(&d)?;
        self.check_agent(from)?;
        if self.tasks.contains_key(id) {
            // A retry of the same creation is idempotent; anything else is a clash.
            let e = &self.tasks[id];
            let first = &e.messages[0];
            if d.client_msg_id.is_some()
                && first.client_msg_id == d.client_msg_id
                && first.from == from
            {
                return Ok(first.clone());
            }
            return Err(BridgeError::Exists(id.into()));
        }
        if d.kind.is_some_and(|k| k != Kind::Task) {
            return Err(BridgeError::Invalid(
                "a new task starts with kind task".into(),
            ));
        }
        if let Some(parent) = d.meta.as_ref().and_then(|m| m.parent.as_deref()) {
            let Some(p) = self.tasks.get(parent) else {
                return Err(BridgeError::Invalid(format!(
                    "parent task {parent:?} does not exist"
                )));
            };
            if p.task.from != from && p.task.to != from {
                return Err(BridgeError::Forbidden(format!(
                    "{from} is not a party to parent task {parent}"
                )));
            }
        }
        let msg = build(id, 1, from, Kind::Task, d);
        let task = open_task(&msg, &self.agents)?;
        self.store.append(&msg, true)?;
        self.tasks.insert(
            id.into(),
            Entry {
                task,
                messages: vec![msg.clone()],
            },
        );
        Ok(msg)
    }

    /// Appends a message from `from` to task `id`, if the state machine allows it.
    pub fn post(&mut self, id: &str, from: &str, d: Draft) -> Result<Message, BridgeError> {
        Self::check_protocol(&d)?;
        self.check_agent(from)?;
        let e = self
            .tasks
            .get(id)
            .ok_or_else(|| BridgeError::NoSuchTask(id.into()))?;
        if from != e.task.from && from != e.task.to {
            return Err(BridgeError::Forbidden(format!(
                "{from} is not a party to task {id}"
            )));
        }
        if let Some(key) = &d.client_msg_id
            && let Some(m) = e
                .messages
                .iter()
                .find(|m| m.from == from && m.client_msg_id.as_ref() == Some(key))
        {
            return Ok(m.clone());
        }
        let kind = d
            .kind
            .ok_or_else(|| BridgeError::Invalid("kind is required".into()))?;
        if kind == Kind::Resume {
            return Err(BridgeError::Forbidden(
                "resume is written by the service from a recorded user decision".into(),
            ));
        }
        // A note is informational (quiet) by default — except the requester's note on work the
        // receiver has taken on: that is a review, a rework or a decision the receiver must act
        // on. Seen live: a rework sent as a plain note was never pushed and DSH sat idle.
        // The level is stored on the message, so what was decided stays visible.
        let mut d = d;
        if d.wake.is_none()
            && kind == Kind::Note
            && from == e.task.from
            && matches!(e.task.state, State::Acked | State::Working)
        {
            d.wake = Some(crate::model::WakeLevel::Normal);
        }
        // A default taken is written too: "defaulted to quiet" must not read like a message from
        // before the field existed (asked by DSH when agreeing to the change above).
        d.wake.get_or_insert(kind.default_wake());
        self.append_checked(id, from, kind, d)
    }

    /// Resumes a task paused for the user, carrying the user's verbatim decision as relayed by
    /// `by`. Called by the service after it has recorded the decision.
    pub fn resume(&mut self, id: &str, by: &str, verbatim: &str) -> Result<Message, BridgeError> {
        self.check_agent(by)?;
        let e = self
            .tasks
            .get(id)
            .ok_or_else(|| BridgeError::NoSuchTask(id.into()))?;
        if by != e.task.from && by != e.task.to {
            return Err(BridgeError::Forbidden(format!(
                "{by} is not a party to task {id}"
            )));
        }
        let d = Draft {
            body: format!("用户决定（由 {by} 转达，原话）：{verbatim}"),
            protocol: Some(PROTOCOL.into()),
            ..Default::default()
        };
        self.append_checked(id, by, Kind::Resume, d)
    }

    fn append_checked(
        &mut self,
        id: &str,
        from: &str,
        kind: Kind,
        d: Draft,
    ) -> Result<Message, BridgeError> {
        let e = self
            .tasks
            .get(id)
            .ok_or_else(|| BridgeError::NoSuchTask(id.into()))?;
        let msg = build(id, e.task.last_n + 1, from, kind, d);
        check_new_rules(&msg)?;
        let mut next = e.task.clone();
        apply(&mut next, &e.messages, &msg)?;
        self.store.append(&msg, false)?;
        let e = self.tasks.get_mut(id).expect("checked above");
        e.task = next;
        e.messages.push(msg.clone());
        Ok(msg)
    }
}

fn build(task: &str, n: u32, from: &str, kind: Kind, d: Draft) -> Message {
    Message {
        task: task.into(),
        n,
        from: from.into(),
        kind,
        body: d.body,
        meta: d.meta,
        questions: d.questions,
        results: d.results,
        needs_user: d.needs_user,
        outcome: d.outcome,
        judgement: d.judgement,
        reply_to: d.reply_to,
        supersedes: d.supersedes,
        client_msg_id: d.client_msg_id,
        session_epoch: d.session_epoch,
        wake: d.wake,
        phase: d.phase,
        protocol: d.protocol.unwrap_or_else(|| PROTOCOL.into()),
        at: Utc::now(),
        imported: false,
    }
}

/// The derived view of an imported (closed) task.
fn archived(mut t: Task, rest: &[Message]) -> Task {
    t.state = State::Closed;
    t.messages = 1 + rest.len() as u32;
    t.question_rounds = rest.iter().filter(|m| m.kind == Kind::Question).count() as u32;
    if let Some(last) = rest.last() {
        t.last_n = last.n;
        t.updated_at = last.at;
    }
    t
}

/// Validates message 1 and derives the task it opens.
fn open_task(m: &Message, agents: &BTreeSet<String>) -> Result<Task, BridgeError> {
    let meta = m
        .meta
        .as_ref()
        .ok_or_else(|| BridgeError::Invalid("a task needs meta {to, title}".into()))?;
    if m.n != 1 || m.kind != Kind::Task {
        return Err(BridgeError::Invalid("message 1 must be the task".into()));
    }
    if !agents.contains(&m.from) {
        return Err(BridgeError::UnknownAgent(m.from.clone()));
    }
    if !agents.contains(&meta.to) {
        return Err(BridgeError::UnknownAgent(meta.to.clone()));
    }
    if meta.to == m.from {
        return Err(BridgeError::Invalid("a task goes to another agent".into()));
    }
    if meta.title.trim().is_empty() {
        return Err(BridgeError::Invalid("title is required".into()));
    }
    Ok(Task {
        id: m.task.clone(),
        from: m.from.clone(),
        to: meta.to.clone(),
        title: meta.title.clone(),
        priority: meta.priority,
        deadline: meta.deadline,
        expr_id: meta.expr_id.clone(),
        parent: meta.parent.clone(),
        state: State::Open,
        paused_reason: None,
        question_rounds: 0,
        limit_base_rounds: 0,
        limit_base_messages: 0,
        messages: 1,
        last_n: 1,
        created_at: m.at,
        updated_at: m.at,
        superseded: vec![],
    })
}

/// Rules for messages written from now on, which older logs did not follow (P7 came with
/// protocol 2). They are checked when a message is posted, never when the log is replayed —
/// history is not rewritten by later rules.
fn check_new_rules(m: &Message) -> Result<(), BridgeError> {
    if m.kind == Kind::Result {
        let unfinished: Vec<&crate::model::ResultItem> =
            m.results.iter().filter(|r| !r.finished()).collect();
        match m.outcome {
            Some(Outcome::Done) if !unfinished.is_empty() => {
                return Err(BridgeError::Invalid(format!(
                    "outcome done with unfinished items ({}): use partial with follow_up, or blocked",
                    unfinished
                        .iter()
                        .map(|r| r.item.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            Some(Outcome::Partial) if unfinished.is_empty() => {
                return Err(BridgeError::Invalid(
                    "outcome partial needs at least one unfinished item".into(),
                ));
            }
            Some(Outcome::Partial)
                if unfinished
                    .iter()
                    .any(|r| r.follow_up.as_deref().is_none_or(|f| f.trim().is_empty())) =>
            {
                return Err(BridgeError::Invalid(
                    "every unfinished item of a partial result needs follow_up (a task id or a needs_user text)".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Applies message `m` (from a party to the task) to `t`, or explains why it is not allowed.
fn apply(t: &mut Task, earlier: &[Message], m: &Message) -> Result<(), BridgeError> {
    let deny = |why: &str| BridgeError::Transition {
        state: t.state,
        kind: m.kind,
        why: why.into(),
    };
    if m.kind == Kind::Task {
        return Err(BridgeError::Invalid(
            "a task is opened once; use answer/progress/… afterwards".into(),
        ));
    }
    // Roles: the receiver works the task, the requester steers and closes it. Either party may
    // relay the user's decision that resumes a paused task, and either may add a note.
    if m.kind == Kind::Note {
        if m.reply_to.is_none() {
            return Err(BridgeError::Invalid(
                "a note replies to an earlier message (reply_to)".into(),
            ));
        }
        if !m.questions.is_empty()
            || !m.results.is_empty()
            || m.outcome.is_some()
            || m.judgement.is_some()
        {
            return Err(BridgeError::Invalid(
                "a note only adds information; questions, results and verdicts have their own kinds".into(),
            ));
        }
    } else if m.kind != Kind::Resume {
        let (role, must) = if m.kind.from_receiver() {
            ("receiver", &t.to)
        } else {
            ("requester", &t.from)
        };
        if &m.from != must {
            return Err(BridgeError::Forbidden(format!(
                "{} may only be sent by the {role} ({must})",
                m.kind
            )));
        }
    } else if m.body.trim().is_empty() {
        return Err(BridgeError::Invalid(
            "resume carries the user's verbatim decision".into(),
        ));
    }
    if m.meta.is_some() {
        return Err(BridgeError::Invalid(
            "meta belongs to the task message only".into(),
        ));
    }
    if let Some(r) = m.reply_to
        && (r == 0 || r >= m.n)
    {
        return Err(BridgeError::Invalid(format!(
            "reply_to {r} is not an earlier message"
        )));
    }
    if let Some(s) = m.supersedes {
        match earlier.iter().find(|e| e.n == s) {
            Some(e) if e.from == m.from && e.kind != Kind::Task && e.kind == m.kind => {}
            _ => {
                return Err(BridgeError::Invalid(format!(
                    "supersedes {s}: must be an earlier {} of the same sender",
                    m.kind
                )));
            }
        }
    }
    if m.phase.is_some() && m.kind != Kind::Progress {
        return Err(BridgeError::Invalid(
            "phase is announced with a progress message".into(),
        ));
    }
    match m.kind {
        Kind::Question if m.questions.is_empty() => {
            return Err(BridgeError::Invalid("a question needs questions[]".into()));
        }
        Kind::Result if m.outcome.is_none() => {
            return Err(BridgeError::Invalid(
                "a result needs outcome done|blocked|rejected".into(),
            ));
        }
        Kind::Verdict if m.judgement.is_none() => {
            return Err(BridgeError::Invalid(
                "a verdict needs judgement pass|rework".into(),
            ));
        }
        _ => {}
    }

    use State::*;
    let s = t.state;
    if s.terminal() {
        return Err(deny("task is finished"));
    }
    let next = match (s, m.kind) {
        (_, Kind::Cancel) => Cancelled,
        // A note never changes where the task stands, wherever it stands.
        (_, Kind::Note) => s,
        (Acked | Working | AwaitingAnswer, Kind::Progress)
            if m.phase == Some(crate::model::Phase::PendingRestart) =>
        {
            AwaitingRestart
        }
        (AwaitingRestart, Kind::Progress) => match m.phase {
            Some(crate::model::Phase::Restarted) => Working,
            _ => AwaitingRestart,
        },
        (_, Kind::Progress) if m.phase == Some(crate::model::Phase::Restarted) => {
            return Err(deny("restarted only follows pending-restart"));
        }
        (AwaitingRestart, Kind::Result) => AwaitingVerdict,
        (PausedForUser, Kind::Close) => Closed,
        (PausedForUser, Kind::Resume) => Working,
        (_, Kind::Resume) => return Err(deny("only a task paused for the user can be resumed")),
        (PausedForUser, _) => {
            return Err(deny(
                "paused for the user: only the user can move it on (requester may close or cancel)",
            ));
        }
        (Open, Kind::Ack) => Acked,
        (Open, _) => {
            return Err(deny(
                "acknowledge the task first: post a message with kind \"ack\" (POST /v1/tasks/{id}/messages); POST /v1/inbox/ack only moves your read cursor",
            ));
        }
        (Acked | Working, Kind::Progress) => Working,
        (AwaitingAnswer, Kind::Progress) => AwaitingAnswer,
        (Acked | Working, Kind::Question) => AwaitingAnswer,
        (AwaitingAnswer, Kind::Answer) => Working,
        (Acked | Working, Kind::Result) => AwaitingVerdict,
        (AwaitingVerdict, Kind::Verdict) => match m.judgement {
            Some(Judgement::Pass) => Verified,
            _ => Working,
        },
        (Verified | AwaitingVerdict, Kind::Close) => Closed,
        _ => return Err(deny("not a valid next step")),
    };
    t.state = next;
    if m.kind == Kind::Question {
        t.question_rounds += 1;
        if t.question_rounds - t.limit_base_rounds > MAX_QUESTION_ROUNDS {
            t.state = PausedForUser;
            t.paused_reason = Some(format!("超过 {MAX_QUESTION_ROUNDS} 轮问答，交给用户"));
        }
    }
    if let Some(s) = m.supersedes {
        t.superseded.push(s);
    }
    t.messages += 1;
    t.last_n = m.n;
    t.updated_at = m.at;
    if m.kind == Kind::Resume {
        // The user moved it on: limits count afresh from here.
        t.limit_base_rounds = t.question_rounds;
        t.limit_base_messages = t.messages;
        t.paused_reason = None;
    }
    if t.messages - t.limit_base_messages >= MAX_MESSAGES
        && !t.state.terminal()
        && t.state != PausedForUser
    {
        t.state = PausedForUser;
        t.paused_reason = Some(format!("消息数达到 {MAX_MESSAGES} 条上限，交给用户"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Question, ResultItem};

    const AGENTS: &[&str] = &["claude", "dsh"];

    fn d(kind: Kind) -> Draft {
        Draft {
            kind: Some(kind),
            protocol: Some(PROTOCOL.into()),
            ..Default::default()
        }
    }
    fn task_to(to: &str) -> Draft {
        Draft {
            meta: Some(TaskMeta {
                to: to.into(),
                title: "do it".into(),
                priority: Default::default(),
                deadline: None,
                expr_id: None,
                parent: None,
            }),
            body: "please".into(),
            ..d(Kind::Task)
        }
    }
    fn question() -> Draft {
        Draft {
            questions: vec![Question {
                id: "q1".into(),
                text: "which?".into(),
                blocking: true,
            }],
            ..d(Kind::Question)
        }
    }
    fn result(o: Outcome) -> Draft {
        Draft {
            outcome: Some(o),
            results: vec![ResultItem {
                item: "x".into(),
                status: "done".into(),
                evidence: "cargo test: ok".into(),
                follow_up: None,
            }],
            ..d(Kind::Result)
        }
    }
    fn verdict(j: Judgement) -> Draft {
        Draft {
            judgement: Some(j),
            ..d(Kind::Verdict)
        }
    }
    fn open() -> (tempfile::TempDir, Bridge) {
        let dir = tempfile::tempdir().unwrap();
        let b = Bridge::open(dir.path().join("state"), AGENTS).unwrap();
        (dir, b)
    }

    #[test]
    fn full_round_trip_is_enforced_and_survives_a_restart() {
        let (dir, mut b) = open();
        b.create_task("t1", "claude", task_to("dsh")).unwrap();
        b.post("t1", "dsh", d(Kind::Ack)).unwrap();
        b.post("t1", "dsh", question()).unwrap();
        assert_eq!(b.task("t1").unwrap().state, State::AwaitingAnswer);
        b.post("t1", "dsh", d(Kind::Progress)).unwrap();
        b.post("t1", "claude", d(Kind::Answer)).unwrap();
        b.post("t1", "dsh", result(Outcome::Done)).unwrap();
        b.post("t1", "claude", verdict(Judgement::Rework)).unwrap();
        assert_eq!(b.task("t1").unwrap().state, State::Working);
        b.post("t1", "dsh", result(Outcome::Done)).unwrap();
        b.post("t1", "claude", verdict(Judgement::Pass)).unwrap();
        let last = b.post("t1", "claude", d(Kind::Close)).unwrap();
        assert_eq!(last.n, 10);
        let t = b.task("t1").unwrap().clone();
        assert_eq!(
            (t.state, t.question_rounds, t.messages),
            (State::Closed, 1, 10)
        );
        assert!(
            b.post("t1", "claude", d(Kind::Cancel)).is_err(),
            "finished tasks accept nothing"
        );

        drop(b);
        let again = Bridge::open(dir.path().join("state"), AGENTS).unwrap();
        assert_eq!(
            again.task("t1"),
            Some(&t),
            "replay reproduces the same task"
        );
        assert_eq!(again.messages("t1").unwrap().len(), 10);
        assert_eq!(again.bad_lines, 0);
    }

    #[test]
    fn roles_are_enforced_both_ways() {
        let (_d, mut b) = open();
        b.create_task("x", "dsh", task_to("claude")).unwrap(); // DSH can hand Claude a task too
        assert!(
            matches!(
                b.post("x", "dsh", d(Kind::Ack)),
                Err(BridgeError::Forbidden(_))
            ),
            "requester cannot ack"
        );
        b.post("x", "claude", d(Kind::Ack)).unwrap();
        assert!(matches!(
            b.post("x", "claude", verdict(Judgement::Pass)),
            Err(BridgeError::Forbidden(_))
        ));
        assert!(matches!(
            b.post("x", "claude", d(Kind::Close)),
            Err(BridgeError::Forbidden(_))
        ));
        assert!(matches!(
            b.post("x", "mallory", d(Kind::Progress)),
            Err(BridgeError::UnknownAgent(_))
        ));
        assert!(
            b.create_task("y", "claude", task_to("claude")).is_err(),
            "no tasks to oneself"
        );
        assert!(b.create_task("y", "claude", task_to("mallory")).is_err());
        let dir3 = tempfile::tempdir().unwrap();
        let mut three = Bridge::open(dir3.path(), &["claude", "dsh", "cc"]).unwrap();
        three.create_task("z", "claude", task_to("dsh")).unwrap();
        assert!(
            matches!(
                three.post("z", "cc", d(Kind::Progress)),
                Err(BridgeError::Forbidden(_))
            ),
            "outsiders cannot post"
        );
    }

    #[test]
    fn transitions_and_required_fields() {
        let (_d, mut b) = open();
        b.create_task("t", "claude", task_to("dsh")).unwrap();
        assert!(
            matches!(
                b.post("t", "dsh", d(Kind::Progress)),
                Err(BridgeError::Transition { .. })
            ),
            "ack first"
        );
        b.post("t", "dsh", d(Kind::Ack)).unwrap();
        assert!(
            matches!(
                b.post("t", "claude", d(Kind::Answer)),
                Err(BridgeError::Transition { .. })
            ),
            "no open question"
        );
        assert!(
            matches!(
                b.post("t", "dsh", d(Kind::Question)),
                Err(BridgeError::Invalid(_))
            ),
            "needs questions[]"
        );
        assert!(
            matches!(
                b.post("t", "dsh", d(Kind::Result)),
                Err(BridgeError::Invalid(_))
            ),
            "needs outcome"
        );
        assert!(
            matches!(
                b.post("t", "claude", d(Kind::Close)),
                Err(BridgeError::Transition { .. })
            ),
            "nothing to close yet"
        );
        b.post("t", "dsh", result(Outcome::Blocked)).unwrap();
        assert!(
            matches!(
                b.post("t", "claude", d(Kind::Verdict)),
                Err(BridgeError::Invalid(_))
            ),
            "needs judgement"
        );
        b.post("t", "claude", d(Kind::Close)).unwrap(); // accepting a blocked result without a verdict
        assert_eq!(b.task("t").unwrap().state, State::Closed);
        let bad = Draft {
            protocol: Some("0".into()),
            ..d(Kind::Ack)
        };
        b.create_task("u", "claude", task_to("dsh")).unwrap();
        assert!(
            matches!(b.post("u", "dsh", bad), Err(BridgeError::Invalid(_))),
            "unknown protocol"
        );
        assert!(
            matches!(
                b.post(
                    "u",
                    "dsh",
                    Draft {
                        kind: Some(Kind::Ack),
                        ..Default::default()
                    }
                ),
                Err(BridgeError::Invalid(_))
            ),
            "protocol required"
        );
        assert!(matches!(
            b.create_task("u", "claude", task_to("dsh")),
            Err(BridgeError::Exists(_))
        ));
        assert!(matches!(
            b.create_task("../u", "claude", task_to("dsh")),
            Err(BridgeError::InvalidId(_))
        ));
        assert!(b.messages("nope").is_err());
    }

    #[test]
    fn limits_hand_the_task_to_the_user() {
        let (_d, mut b) = open();
        b.create_task("q", "claude", task_to("dsh")).unwrap();
        b.post("q", "dsh", d(Kind::Ack)).unwrap();
        for _ in 0..3 {
            b.post("q", "dsh", question()).unwrap();
            b.post("q", "claude", d(Kind::Answer)).unwrap();
        }
        b.post("q", "dsh", question()).unwrap();
        let t = b.task("q").unwrap();
        assert_eq!(t.state, State::PausedForUser);
        assert!(t.paused_reason.as_deref().unwrap().contains("3 轮"));
        assert!(
            b.post("q", "claude", d(Kind::Answer)).is_err(),
            "only the user moves it on"
        );
        b.post("q", "claude", d(Kind::Cancel)).unwrap();

        b.create_task("m", "claude", task_to("dsh")).unwrap();
        b.post("m", "dsh", d(Kind::Ack)).unwrap();
        for _ in 0..(MAX_MESSAGES - 2) {
            b.post("m", "dsh", d(Kind::Progress)).unwrap();
        }
        let t = b.task("m").unwrap();
        assert_eq!((t.messages, t.state), (MAX_MESSAGES, State::PausedForUser));
        assert!(b.post("m", "dsh", d(Kind::Progress)).is_err());
        b.post("m", "claude", d(Kind::Close)).unwrap();
        assert_eq!(b.task("m").unwrap().state, State::Closed);
    }

    #[test]
    fn retries_are_idempotent_and_supersedes_is_checked() {
        let (_d, mut b) = open();
        let key = || Some("k-1".to_string());
        let a = b
            .create_task(
                "t",
                "claude",
                Draft {
                    client_msg_id: key(),
                    ..task_to("dsh")
                },
            )
            .unwrap();
        let again = b
            .create_task(
                "t",
                "claude",
                Draft {
                    client_msg_id: key(),
                    ..task_to("dsh")
                },
            )
            .unwrap();
        assert_eq!(
            a, again,
            "creating again with the same key returns the stored task message"
        );
        let ack = b
            .post(
                "t",
                "dsh",
                Draft {
                    client_msg_id: Some("a".into()),
                    ..d(Kind::Ack)
                },
            )
            .unwrap();
        let dup = b
            .post(
                "t",
                "dsh",
                Draft {
                    client_msg_id: Some("a".into()),
                    ..d(Kind::Ack)
                },
            )
            .unwrap();
        assert_eq!((ack.n, dup.n, b.task("t").unwrap().messages), (2, 2, 2));
        let p = b.post("t", "dsh", d(Kind::Progress)).unwrap();
        assert!(
            b.post(
                "t",
                "dsh",
                Draft {
                    supersedes: Some(p.n),
                    ..d(Kind::Ack)
                }
            )
            .is_err(),
            "must replace the same kind"
        );
        assert!(
            b.post(
                "t",
                "dsh",
                Draft {
                    supersedes: Some(1),
                    ..d(Kind::Progress)
                }
            )
            .is_err(),
            "not someone else's / the task"
        );
        b.post(
            "t",
            "dsh",
            Draft {
                supersedes: Some(p.n),
                body: "corrected".into(),
                ..d(Kind::Progress)
            },
        )
        .unwrap();
        assert_eq!(b.task("t").unwrap().superseded, vec![p.n]);
        assert!(
            b.post(
                "t",
                "dsh",
                Draft {
                    reply_to: Some(99),
                    ..d(Kind::Progress)
                }
            )
            .is_err()
        );
    }

    #[test]
    fn corrupt_lines_do_not_lose_the_task() {
        let (dir, mut b) = open();
        b.create_task("t", "claude", task_to("dsh")).unwrap();
        b.post("t", "dsh", d(Kind::Ack)).unwrap();
        drop(b);
        use std::io::Write;
        let path = dir.path().join("state/tasks/t.jsonl");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "garbage").unwrap();
        drop(f);
        let mut b = Bridge::open(dir.path().join("state"), AGENTS).unwrap();
        assert_eq!(b.bad_lines, 1);
        assert_eq!(b.task("t").unwrap().state, State::Acked);
        let p = b.post("t", "dsh", d(Kind::Progress)).unwrap();
        assert_eq!(p.n, 3, "numbering continues after the valid messages");
        assert_eq!(b.list(Some("dsh")).len(), 1);
        assert_eq!(b.list(Some("cc")).len(), 0);
    }

    fn draft_json(v: serde_json::Value) -> Draft {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn notes_add_information_anywhere_without_moving_the_task() {
        let (_d, mut b) = open();
        b.create_task("t", "claude", task_to("dsh")).unwrap();
        b.post("t", "dsh", d(Kind::Ack)).unwrap();
        // Either party, any state, never a state change.
        let n = b
            .post(
                "t",
                "claude",
                Draft {
                    reply_to: Some(2),
                    body: "补充：注册表回退即可".into(),
                    ..d(Kind::Note)
                },
            )
            .unwrap();
        assert_eq!((n.n, b.task("t").unwrap().state), (3, State::Acked));
        b.post("t", "dsh", question()).unwrap();
        b.post(
            "t",
            "dsh",
            Draft {
                reply_to: Some(4),
                ..d(Kind::Note)
            },
        )
        .unwrap();
        assert_eq!(
            b.task("t").unwrap().state,
            State::AwaitingAnswer,
            "a note is not an answer"
        );
        assert_eq!(
            b.task("t").unwrap().question_rounds,
            1,
            "notes do not count as rounds"
        );
        // A note must point at what it adds to, and carries no question/result/verdict.
        assert!(matches!(
            b.post("t", "claude", d(Kind::Note)),
            Err(BridgeError::Invalid(_))
        ));
        assert!(
            b.post(
                "t",
                "claude",
                Draft {
                    reply_to: Some(9),
                    ..d(Kind::Note)
                }
            )
            .is_err()
        );
        let with_q = Draft {
            reply_to: Some(2),
            questions: question().questions,
            ..d(Kind::Note)
        };
        assert!(matches!(
            b.post("t", "claude", with_q),
            Err(BridgeError::Invalid(_))
        ));
        // Default wake: the requester's note on acked/working work wakes the receiver (it is
        // something to act on); the receiver's own note stays quiet unless it asks otherwise.
        assert_eq!(n.effective_wake(), crate::model::WakeLevel::Normal);
        let own = &b.messages("t").unwrap()[4];
        assert_eq!((own.from.as_str(), own.kind), ("dsh", Kind::Note));
        assert_eq!(own.effective_wake(), crate::model::WakeLevel::Quiet);
        assert_eq!(
            own.wake,
            Some(crate::model::WakeLevel::Quiet),
            "a default taken is stored, not left absent like a message from before the field"
        );
        let explicit = b
            .post(
                "t",
                "claude",
                Draft {
                    reply_to: Some(2),
                    wake: Some(crate::model::WakeLevel::Quiet),
                    ..d(Kind::Note)
                },
            )
            .unwrap();
        assert_eq!(
            explicit.effective_wake(),
            crate::model::WakeLevel::Quiet,
            "an explicit choice is kept"
        );
        let loud = b
            .post(
                "t",
                "claude",
                Draft {
                    reply_to: Some(2),
                    wake: Some(crate::model::WakeLevel::Urgent),
                    ..d(Kind::Note)
                },
            )
            .unwrap();
        assert_eq!(loud.effective_wake(), crate::model::WakeLevel::Urgent);
        assert_eq!(
            b.messages("t").unwrap()[0].effective_wake(),
            crate::model::WakeLevel::Normal,
            "a task wakes by default"
        );
    }

    #[test]
    fn a_restart_is_a_lifecycle_step_not_a_stall() {
        use crate::model::Phase;
        let (_d, mut b) = open();
        b.create_task("t", "claude", task_to("dsh")).unwrap();
        b.post("t", "dsh", d(Kind::Ack)).unwrap();
        assert!(
            b.post(
                "t",
                "dsh",
                Draft {
                    phase: Some(Phase::Restarted),
                    ..d(Kind::Progress)
                }
            )
            .is_err(),
            "restarted needs a pending restart"
        );
        assert!(
            matches!(
                b.post(
                    "t",
                    "dsh",
                    Draft {
                        phase: Some(Phase::PendingRestart),
                        ..d(Kind::Ack)
                    }
                ),
                Err(BridgeError::Invalid(_))
            ),
            "phase only on progress"
        );
        b.post(
            "t",
            "dsh",
            Draft {
                phase: Some(Phase::PendingRestart),
                ..d(Kind::Progress)
            },
        )
        .unwrap();
        assert_eq!(b.task("t").unwrap().state, State::AwaitingRestart);
        b.post("t", "dsh", d(Kind::Progress)).unwrap();
        assert_eq!(
            b.task("t").unwrap().state,
            State::AwaitingRestart,
            "plain progress keeps waiting"
        );
        b.post(
            "t",
            "dsh",
            Draft {
                phase: Some(Phase::Restarted),
                body: "bootId muwlusnj".into(),
                ..d(Kind::Progress)
            },
        )
        .unwrap();
        assert_eq!(b.task("t").unwrap().state, State::Working);
        // A result may also come straight after the restart.
        b.post(
            "t",
            "dsh",
            Draft {
                phase: Some(Phase::PendingRestart),
                ..d(Kind::Progress)
            },
        )
        .unwrap();
        b.post("t", "dsh", result(Outcome::Done)).unwrap();
        assert_eq!(b.task("t").unwrap().state, State::AwaitingVerdict);
    }

    #[test]
    fn done_means_done_and_partial_names_its_follow_ups() {
        let (_d, mut b) = open();
        b.create_task("t", "claude", task_to("dsh")).unwrap();
        b.post("t", "dsh", d(Kind::Ack)).unwrap();
        let item = |name: &str, status: &str, follow: Option<&str>| ResultItem {
            item: name.into(),
            status: status.into(),
            evidence: "e".into(),
            follow_up: follow.map(str::to_string),
        };
        let res = |o: Outcome, items: Vec<ResultItem>| Draft {
            outcome: Some(o),
            results: items,
            ..d(Kind::Result)
        };
        let e = b
            .post(
                "t",
                "dsh",
                res(
                    Outcome::Done,
                    vec![item("a", "done", None), item("b", "failed", None)],
                ),
            )
            .unwrap_err();
        assert!(e.to_string().contains("unfinished items (b)"), "{e}");
        assert!(
            b.post(
                "t",
                "dsh",
                res(Outcome::Partial, vec![item("a", "done", None)])
            )
            .is_err(),
            "partial needs an unfinished item"
        );
        assert!(
            b.post(
                "t",
                "dsh",
                res(
                    Outcome::Partial,
                    vec![item("a", "done", None), item("b", "pending", None)]
                )
            )
            .is_err(),
            "follow_up required"
        );
        b.post(
            "t",
            "dsh",
            res(
                Outcome::Partial,
                vec![
                    item("a", "done", None),
                    item("b", "pending", Some("t-followup")),
                ],
            ),
        )
        .unwrap();
        assert_eq!(b.task("t").unwrap().state, State::AwaitingVerdict);
        // blocked/rejected stay free-form.
        b.post("t", "claude", verdict(Judgement::Rework)).unwrap();
        b.post(
            "t",
            "dsh",
            res(Outcome::Blocked, vec![item("b", "failed", None)]),
        )
        .unwrap();
    }

    #[test]
    fn parents_must_exist_and_belong_to_the_sender() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = Bridge::open(dir.path(), &["claude", "dsh", "cc"]).unwrap();
        b.create_task("p", "claude", task_to("dsh")).unwrap();
        let child = |parent: &str| Draft {
            meta: Some(TaskMeta {
                to: "dsh".into(),
                title: "child".into(),
                priority: Default::default(),
                deadline: None,
                expr_id: None,
                parent: Some(parent.into()),
            }),
            ..d(Kind::Task)
        };
        assert!(matches!(
            b.create_task("c0", "claude", child("missing")),
            Err(BridgeError::Invalid(_))
        ));
        assert!(matches!(
            b.create_task(
                "c1",
                "cc",
                Draft {
                    meta: Some(TaskMeta {
                        to: "claude".into(),
                        ..child("p").meta.unwrap()
                    }),
                    ..child("p")
                }
            ),
            Err(BridgeError::Forbidden(_))
        ));
        b.create_task(
            "c2",
            "dsh",
            Draft {
                meta: Some(TaskMeta {
                    to: "claude".into(),
                    ..child("p").meta.unwrap()
                }),
                ..child("p")
            },
        )
        .unwrap();
        assert_eq!(b.task("c2").unwrap().parent.as_deref(), Some("p"));
    }

    #[test]
    fn protocol_1_clients_keep_working_and_old_needs_user_strings_still_read() {
        let (_d, mut b) = open();
        let v1 = draft_json(
            serde_json::json!({ "kind": "task", "protocol": "1", "meta": { "to": "dsh", "title": "old client" }, "needs_user": ["旧格式"] }),
        );
        let m = b.create_task("t", "claude", v1).unwrap();
        assert_eq!(
            (m.needs_user[0].text.as_str(), m.needs_user[0].relay),
            ("旧格式", None)
        );
        let v2 = draft_json(
            serde_json::json!({ "kind": "ack", "protocol": "2", "needs_user": [{ "text": "要不要重启", "relay": "dsh" }] }),
        );
        let m = b.post("t", "dsh", v2).unwrap();
        assert_eq!(m.needs_user[0].relay, Some(crate::model::Relay::Dsh));
        assert!(
            b.post(
                "t",
                "dsh",
                draft_json(serde_json::json!({ "kind": "progress", "protocol": "0" }))
            )
            .is_err()
        );
    }

    #[test]
    fn logs_written_under_older_rules_still_replay() {
        // A result written before P7 existed: outcome done although an item failed.
        let (dir, mut b) = open();
        b.create_task("t", "claude", task_to("dsh")).unwrap();
        b.post("t", "dsh", d(Kind::Ack)).unwrap();
        drop(b);
        let old = Message {
            task: "t".into(),
            n: 3,
            from: "dsh".into(),
            kind: Kind::Result,
            body: "old".into(),
            meta: None,
            questions: vec![],
            needs_user: vec![],
            judgement: None,
            reply_to: None,
            supersedes: None,
            client_msg_id: None,
            session_epoch: None,
            wake: None,
            phase: None,
            imported: false,
            protocol: "1".into(),
            at: chrono::Utc::now(),
            outcome: Some(Outcome::Done),
            results: vec![ResultItem {
                item: "x".into(),
                status: "failed".into(),
                evidence: "e".into(),
                follow_up: None,
            }],
        };
        Store::open(dir.path().join("state"))
            .unwrap()
            .append(&old, false)
            .unwrap();
        let mut b = Bridge::open(dir.path().join("state"), AGENTS).unwrap();
        assert_eq!(b.bad_lines, 0, "history is not rewritten by later rules");
        assert_eq!(b.task("t").unwrap().state, State::AwaitingVerdict);
        b.post("t", "claude", verdict(Judgement::Pass)).unwrap();
        // …while the same message posted today is refused.
        b.create_task("u", "claude", task_to("dsh")).unwrap();
        b.post("u", "dsh", d(Kind::Ack)).unwrap();
        let today = Draft {
            outcome: Some(Outcome::Done),
            results: old.results.clone(),
            ..d(Kind::Result)
        };
        assert!(b.post("u", "dsh", today).is_err());
    }
}
