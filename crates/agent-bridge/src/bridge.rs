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
    pub needs_user: Vec<String>,
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
            Some(PROTOCOL) => Ok(()),
            other => Err(BridgeError::Invalid(format!(
                "protocol {other:?} not supported (this service speaks {PROTOCOL:?})"
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
        let msg = build(id, e.task.last_n + 1, from, kind, d);
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
        protocol: d.protocol.unwrap_or_else(|| PROTOCOL.into()),
        at: Utc::now(),
    }
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
        state: State::Open,
        paused_reason: None,
        question_rounds: 0,
        messages: 1,
        last_n: 1,
        created_at: m.at,
        updated_at: m.at,
        superseded: vec![],
    })
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
    // Roles: the receiver works the task, the requester steers and closes it.
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
        (PausedForUser, Kind::Close) => Closed,
        (PausedForUser, _) => {
            return Err(deny(
                "paused for the user: only the user can move it on (requester may close or cancel)",
            ));
        }
        (Open, Kind::Ack) => Acked,
        (Open, _) => return Err(deny("acknowledge the task first")),
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
        if t.question_rounds > MAX_QUESTION_ROUNDS {
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
    if t.messages >= MAX_MESSAGES && !t.state.terminal() && t.state != PausedForUser {
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
}
