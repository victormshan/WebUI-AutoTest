//! Read-only observers (M2 design §3.8, option A): an identity that can list every task's
//! metadata and nothing else. The review gate uses one to find the bridge tasks of a milestone
//! itself instead of trusting the implementer's list.
//!
//! - An observer is not a bridge participant: it cannot send or receive tasks and has no inbox,
//!   cursor, presence or push. Its token may be used on exactly two routes, both `GET`
//!   (`service::observer_gate`); everything else, `/v1/health` included, is refused with 403
//!   before any handler runs. Anonymous `/v1/health` stays public: the role does not hide what
//!   is public anyway.
//! - Responses are built field by field from the types below (a whitelist), never by removing
//!   fields from a `Task` or `Message`: no body, questions, results, needs_user or meta.
//! - Every response carries `revision`, a digest over the observed metadata of **all** tasks
//!   (see [`revision`]), so a client can tell a consistent read across pages from a mixed one
//!   ([`collect`]).
//! - `complete` only says the history the service loaded had no unreadable lines. It cannot
//!   notice a task file removed from disk, and it proves nothing about review content, user
//!   decisions or signatures: those are checked from their own sources.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bridge::Bridge;
use crate::model::{Kind, Message, State, Task};

/// Tasks per list page: default and largest.
pub const LIST_LIMIT_DEFAULT: usize = 100;
pub const LIST_LIMIT_MAX: usize = 200;
/// Message metadata per detail page: default and largest.
pub const DETAIL_LIMIT_DEFAULT: usize = 200;
pub const DETAIL_LIMIT_MAX: usize = 500;
/// Prefix of the digest input; a change of the format below changes it.
pub const REVISION_FORMAT: &str = "agent-bridge-observe v1\n";
/// The exact keys of each object the observe routes answer with. [`collect`] refuses an object
/// with a key missing or added: a missing `next` must not read as a last page.
pub const LIST_KEYS: &[&str] = &["revision", "complete", "tasks", "next"];
pub const DETAIL_KEYS: &[&str] = &["revision", "complete", "task", "message_metadata", "next_n"];
pub const TASK_KEYS: &[&str] = &[
    "id",
    "parent",
    "from",
    "to",
    "state",
    "created_at",
    "updated_at",
    "message_count",
    "last_n",
    "participants",
];
pub const MESSAGE_KEYS: &[&str] = &["n", "from", "kind", "at"];
/// Rounds [`collect`] retries a read that changed under it before giving up.
pub const COLLECT_ROUNDS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedTask {
    pub id: String,
    pub parent: Option<String>,
    pub from: String,
    pub to: String,
    pub state: State,
    pub created_at: DateTime<Utc>,
    /// Time of the task's last message: the bridge sets it on every message it stores, and
    /// every state change is a message (bridge.rs, `apply` and `archived`).
    pub updated_at: DateTime<Utc>,
    pub message_count: u32,
    pub last_n: u32,
    /// Sender and receiver of the task and the sender of every message, sorted, once each.
    pub participants: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedMessage {
    pub n: u32,
    pub from: String,
    pub kind: Kind,
    pub at: DateTime<Utc>,
}

pub fn observe_task(t: &Task, messages: &[Message]) -> ObservedTask {
    let mut participants = BTreeSet::from([t.from.clone(), t.to.clone()]);
    participants.extend(messages.iter().map(|m| m.from.clone()));
    ObservedTask {
        id: t.id.clone(),
        parent: t.parent.clone(),
        from: t.from.clone(),
        to: t.to.clone(),
        state: t.state,
        created_at: t.created_at,
        updated_at: t.updated_at,
        message_count: t.messages,
        last_n: t.last_n,
        participants,
    }
}

pub fn observe_message(m: &Message) -> ObservedMessage {
    ObservedMessage {
        n: m.n,
        from: m.from.clone(),
        kind: m.kind,
        at: m.at,
    }
}

/// Every task with its messages, ordered by task id (byte order), messages by n.
pub fn observe_all(b: &Bridge) -> Vec<(ObservedTask, Vec<ObservedMessage>)> {
    let mut tasks = b.list(None);
    tasks.sort_by(|x, y| x.id.cmp(&y.id));
    tasks
        .into_iter()
        .map(|t| {
            let ms = b.messages(&t.id).unwrap_or(&[]);
            let mut om: Vec<ObservedMessage> = ms.iter().map(observe_message).collect();
            om.sort_by_key(|m| m.n);
            (observe_task(t, ms), om)
        })
        .collect()
}

/// Lowercase hex SHA-256 of [`REVISION_FORMAT`] followed by the compact serde_json encoding of
/// `all` as an array of `[task, [message…]]` pairs, in the order [`observe_all`] gives (task
/// fields in declaration order, participants sorted). The input holds only observed metadata:
/// not the revision itself, no paging fields, no time of the request. Equal content gives the
/// same revision, across restarts too; any change to a task or message that an observer can see
/// changes it, wherever the task falls in the paging.
pub fn revision(all: &[(ObservedTask, Vec<ObservedMessage>)]) -> String {
    let mut h = Sha256::new();
    h.update(REVISION_FORMAT.as_bytes());
    h.update(serde_json::to_vec(all).expect("serializable"));
    crate::service::hex(&h.finalize())
}

/// Observers from `path`: a JSON object name → sha256 of the token (as agents.json).
/// `explicit` is true for a path given on the command line: then a missing file is an error.
/// For the default path only "not found" means "no observers"; an empty, unreadable or malformed
/// file is an error. Errors name the file and the entry, never a value from it.
pub fn load_observers(path: &Path, explicit: bool) -> anyhow::Result<Vec<(String, [u8; 32])>> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => return Ok(vec![]),
        Err(e) => anyhow::bail!("reading {}: {}", path.display(), e.kind()),
    };
    parse_observers(&raw).map_err(|why| anyhow::anyhow!("{}: {why}", path.display()))
}

/// See [`load_observers`]. The error never contains a name or value from `raw`.
pub fn parse_observers(raw: &[u8]) -> Result<Vec<(String, [u8; 32])>, String> {
    let mut de = serde_json::Deserializer::from_slice(raw);
    let entries = Entries::deserialize(&mut de)
        .and_then(|e| de.end().map(|()| e))
        .map_err(|e| match e.classify() {
            // The message of a data error can quote the value; the position is enough.
            serde_json::error::Category::Data => format!(
                "not an object of name → hash (line {}, column {})",
                e.line(),
                e.column()
            ),
            c => format!(
                "invalid JSON ({c:?}, line {}, column {})",
                e.line(),
                e.column()
            ),
        })?;
    if entries.0.is_empty() {
        return Err("names no observer (delete the file for none)".into());
    }
    let mut out: Vec<(String, [u8; 32])> = Vec::new();
    for (i, (name, h)) in entries.0.into_iter().enumerate() {
        let entry = i + 1;
        if out.iter().any(|(x, _)| *x == name) {
            return Err(format!("entry {entry}: duplicate key"));
        }
        if !crate::valid_id(&name) {
            return Err(format!("entry {entry}: name is not a valid id"));
        }
        let h = crate::service::parse_hex32(&h)
            .ok_or_else(|| format!("entry {entry}: expected 64 hex digits (a sha256)"))?;
        if out.iter().any(|(_, x)| *x == h) {
            return Err(format!(
                "entry {entry}: same token hash as an earlier entry"
            ));
        }
        out.push((name, h));
    }
    Ok(out)
}

/// Agents and observers are told apart by token alone: no name and no token hash may be in both
/// sets, nor twice in one.
pub fn check_distinct(
    agents: &[(String, [u8; 32])],
    observers: &[(String, [u8; 32])],
) -> anyhow::Result<()> {
    for (i, (a, ha)) in agents.iter().enumerate() {
        anyhow::ensure!(
            !agents[..i].iter().any(|(_, h)| h == ha),
            "agent {a}: same token hash as another agent"
        );
    }
    for (o, ho) in observers {
        anyhow::ensure!(
            !agents.iter().any(|(a, _)| a == o),
            "observer {o}: also an agent name"
        );
        anyhow::ensure!(
            !agents.iter().any(|(_, h)| h == ho),
            "observer {o}: same token hash as an agent"
        );
    }
    for (i, (o, _)) in observers.iter().enumerate() {
        anyhow::ensure!(
            !observers[..i].iter().any(|(x, _)| x == o),
            "observer {o}: named twice"
        );
    }
    Ok(())
}

/// A JSON object read in file order with every entry kept, so a repeated key is seen and
/// refused (a plain map would keep the last value without a word).
struct Entries(Vec<(String, String)>);

impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Entries;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object of name → hash")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> Result<Entries, A::Error> {
                let mut out: Vec<(String, String)> = Vec::new();
                while let Some(k) = m.next_key::<String>()? {
                    let v: String = m.next_value()?;
                    out.push((k, v));
                }
                Ok(Entries(out))
            }
        }
        d.deserialize_map(V)
    }
}

/// What [`collect`] read: every task with all its message metadata, at one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collected {
    pub revision: String,
    pub tasks: Vec<(ObservedTask, Vec<ObservedMessage>)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CollectError {
    /// The service says its loaded history is incomplete (503): the sources are unknown.
    #[error("bridge history incomplete")]
    Incomplete,
    /// Still changing after [`COLLECT_ROUNDS`] reads.
    #[error("bridge kept changing during {COLLECT_ROUNDS} reads")]
    Unstable,
    /// The service answered something this client does not accept: fail closed.
    #[error("bridge answer refused: {0}")]
    Protocol(String),
    #[error("bridge unreachable: {0}")]
    Transport(String),
}

enum Attempt {
    Changed,
    Failed(CollectError),
}

impl From<CollectError> for Attempt {
    fn from(e: CollectError) -> Self {
        Attempt::Failed(e)
    }
}

fn protocol(why: impl Into<String>) -> CollectError {
    CollectError::Protocol(why.into())
}

/// Reference client: reads every task and all its message metadata through `get` (a path and
/// query such as `/v1/observe/tasks?limit=200`, answered with status and JSON body) and accepts
/// the result only if every page answered at the same revision, the first list page still does
/// at the end, cursors moved strictly forward, nothing repeated, and each task's message
/// metadata matches its count and last n. A revision change starts over, at most
/// [`COLLECT_ROUNDS`] times; anything else unexpected is an error, never a partial result.
pub async fn collect<F, Fut>(mut get: F) -> Result<Collected, CollectError>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<(u16, Value), String>>,
{
    for _ in 0..COLLECT_ROUNDS {
        match attempt(&mut get).await {
            Ok(c) => return Ok(c),
            Err(Attempt::Changed) => continue,
            Err(Attempt::Failed(e)) => return Err(e),
        }
    }
    Err(CollectError::Unstable)
}

async fn fetch<F, Fut>(get: &mut F, path: String) -> Result<Value, CollectError>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<(u16, Value), String>>,
{
    let (status, v) = get(path).await.map_err(CollectError::Transport)?;
    match status {
        200 => {}
        503 => return Err(CollectError::Incomplete),
        s => return Err(protocol(format!("status {s}"))),
    }
    if v.get("complete") != Some(&Value::Bool(true)) {
        return Err(protocol("complete is not true"));
    }
    Ok(v)
}

fn same_revision(v: &Value, rev: &mut Option<String>) -> Result<bool, CollectError> {
    let r = v
        .get("revision")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol("no revision"))?;
    match rev {
        None => {
            *rev = Some(r.to_string());
            Ok(true)
        }
        Some(x) => Ok(x == r),
    }
}

/// `v` is an object with exactly `keys` (a null value counts as present; a missing key does not).
fn exact_keys(v: &Value, keys: &[&str], what: &str) -> Result<(), CollectError> {
    let o = v
        .as_object()
        .ok_or_else(|| protocol(format!("{what}: not an object")))?;
    if o.len() != keys.len() || keys.iter().any(|k| !o.contains_key(*k)) {
        let got: Vec<&String> = o.keys().collect();
        return Err(protocol(format!("{what}: keys {got:?}, expected {keys:?}")));
    }
    Ok(())
}

/// A list or detail answer: the envelope and every task and message object in it have exactly
/// their keys.
fn check_shape(v: &Value, detail: bool) -> Result<(), CollectError> {
    let tasks: Vec<&Value> = if detail {
        exact_keys(v, DETAIL_KEYS, "detail")?;
        let ms = v["message_metadata"]
            .as_array()
            .ok_or_else(|| protocol("message_metadata: not an array"))?;
        for m in ms {
            exact_keys(m, MESSAGE_KEYS, "message")?;
        }
        vec![&v["task"]]
    } else {
        exact_keys(v, LIST_KEYS, "list")?;
        v["tasks"]
            .as_array()
            .ok_or_else(|| protocol("tasks: not an array"))?
            .iter()
            .collect()
    };
    for t in tasks {
        exact_keys(t, TASK_KEYS, "task")?;
    }
    Ok(())
}

/// The value of `k`, which must be present (`check_shape` has made sure of that).
fn field<T: serde::de::DeserializeOwned>(v: &Value, k: &str) -> Result<T, CollectError> {
    let x = v.get(k).ok_or_else(|| protocol(format!("{k}: missing")))?;
    serde_json::from_value(x.clone()).map_err(|e| protocol(format!("{k}: {e}")))
}

async fn attempt<F, Fut>(get: &mut F) -> Result<Collected, Attempt>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<(u16, Value), String>>,
{
    let mut rev: Option<String> = None;
    let mut tasks: Vec<ObservedTask> = Vec::new();
    let mut after: Option<String> = None;
    let first_page = format!("/v1/observe/tasks?limit={LIST_LIMIT_MAX}");
    loop {
        let path = match &after {
            None => first_page.clone(),
            Some(a) => format!("{first_page}&after={a}"),
        };
        let v = fetch(get, path).await?;
        check_shape(&v, false)?;
        if !same_revision(&v, &mut rev)? {
            return Err(Attempt::Changed);
        }
        let page: Vec<ObservedTask> = field(&v, "tasks")?;
        for t in &page {
            // Strictly increasing ids: no repeats, nothing at or before the cursor.
            let floor = tasks.last().map(|x| &x.id).or(after.as_ref());
            if floor.is_some_and(|f| t.id <= *f) {
                return Err(protocol(format!("task {} out of order or repeated", t.id)).into());
            }
            tasks.push(t.clone());
        }
        let next: Option<String> = field(&v, "next")?;
        match next {
            None => break,
            Some(n) => {
                if page.last().map(|t| &t.id) != Some(&n) {
                    return Err(protocol("next is not the last task of the page").into());
                }
                after = Some(n);
            }
        }
    }
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        let mut msgs: Vec<ObservedMessage> = Vec::new();
        let mut after_n: Option<u32> = None;
        loop {
            let mut path = format!("/v1/observe/tasks/{}?limit={DETAIL_LIMIT_MAX}", t.id);
            if let Some(n) = after_n {
                path.push_str(&format!("&after_n={n}"));
            }
            let v = fetch(get, path).await?;
            check_shape(&v, true)?;
            if !same_revision(&v, &mut rev)? {
                return Err(Attempt::Changed);
            }
            let dt: ObservedTask = field(&v, "task")?;
            if dt != t {
                return Err(protocol(format!(
                    "task {} differs from the list at one revision",
                    t.id
                ))
                .into());
            }
            let page: Vec<ObservedMessage> = field(&v, "message_metadata")?;
            for m in &page {
                let floor = msgs.last().map(|x| x.n).or(after_n);
                if floor.is_some_and(|f| m.n <= f) {
                    return Err(protocol(format!(
                        "task {}: message {} out of order or repeated",
                        t.id, m.n
                    ))
                    .into());
                }
                msgs.push(m.clone());
            }
            let next: Option<u32> = field(&v, "next_n")?;
            match next {
                None => break,
                Some(n) => {
                    if page.last().map(|m| m.n) != Some(n) {
                        return Err(protocol("next_n is not the last message of the page").into());
                    }
                    after_n = Some(n);
                }
            }
        }
        if msgs.len() != t.message_count as usize {
            return Err(protocol(format!(
                "task {}: {} messages listed, message_count {}",
                t.id,
                msgs.len(),
                t.message_count
            ))
            .into());
        }
        if msgs.last().map(|m| m.n) != Some(t.last_n) {
            return Err(protocol(format!("task {}: last message is not last_n", t.id)).into());
        }
        out.push((t, msgs));
    }
    // The first page again: anything changed anywhere since the start changes the revision.
    let v = fetch(get, first_page).await?;
    check_shape(&v, false)?;
    if !same_revision(&v, &mut rev)? {
        return Err(Attempt::Changed);
    }
    Ok(Collected {
        revision: rev.expect("set by the first page"),
        tasks: out,
    })
}

/// Paging of the list: tasks with id strictly after `after`, at most `limit`; `next` is the
/// last id returned when more follow, else `None`.
pub fn list_page<'a>(
    all: &'a [(ObservedTask, Vec<ObservedMessage>)],
    after: Option<&str>,
    limit: usize,
) -> (Vec<&'a ObservedTask>, Option<String>) {
    let rest: Vec<&ObservedTask> = all
        .iter()
        .map(|(t, _)| t)
        .filter(|t| after.is_none_or(|a| t.id.as_str() > a))
        .collect();
    let page: Vec<&ObservedTask> = rest.iter().take(limit).copied().collect();
    let next = (rest.len() > page.len()).then(|| page.last().expect("limit ≥ 1").id.clone());
    (page, next)
}

/// Paging of one task's messages: n strictly after `after_n`, at most `limit`.
pub fn message_page(
    msgs: &[ObservedMessage],
    after_n: Option<u32>,
    limit: usize,
) -> (Vec<&ObservedMessage>, Option<u32>) {
    let rest: Vec<&ObservedMessage> = msgs
        .iter()
        .filter(|m| after_n.is_none_or(|a| m.n > a))
        .collect();
    let page: Vec<&ObservedMessage> = rest.iter().take(limit).copied().collect();
    let next = (rest.len() > page.len()).then(|| page.last().expect("limit ≥ 1").n);
    (page, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    #[test]
    fn observers_parse_strictly() {
        let ok = parse_observers(format!(r#"{{"review-gate":"{}"}}"#, h('a')).as_bytes()).unwrap();
        assert_eq!(ok, vec![("review-gate".to_string(), [0xaa; 32])]);
        let cases: Vec<(String, &str)> = vec![
            (String::new(), "invalid JSON"),
            ("{}".into(), "names no observer"),
            ("[]".into(), "not an object"),
            (
                format!(r#"{{"a":"{}","a":"{}"}}"#, h('a'), h('b')),
                "duplicate key",
            ),
            (
                format!(r#"{{"a":"{}","b":"{}"}}"#, h('a'), h('a')),
                "same token hash",
            ),
            (format!(r#"{{"a b":"{}"}}"#, h('a')), "not a valid id"),
            (format!(r#"{{"a":"{}"}}"#, &h('a')[..63]), "64 hex"),
            (format!(r#"{{"a":"{}"}}"#, h('g')), "64 hex"),
            // 64 bytes but not ASCII: must be refused, not sliced mid-character.
            (format!(r#"{{"a":"{}"}}"#, "é".repeat(32)), "64 hex"),
            (format!(r#"{{"a":"{}a"}}"#, "€".repeat(21)), "64 hex"),
            (r#"{"a":5}"#.into(), "not an object"),
            (format!(r#"{{"a":"{}"}} x"#, h('a')), "invalid JSON"),
        ];
        for (raw, want) in cases {
            let e = parse_observers(raw.as_bytes()).unwrap_err();
            assert!(e.contains(want), "{raw:?}: {e}");
        }
    }

    #[test]
    fn parse_errors_never_quote_the_file() {
        let secretish = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85";
        for raw in [
            format!(r#"{{"x":"{secretish}"}}"#),
            format!(r#"{{"x":["{secretish}"]}}"#),
            format!(r#"{{"x":"{secretish}","x":"{secretish}"}}"#),
            format!(r#"{{"{secretish}!":"{secretish}5"}}"#),
        ] {
            let e = parse_observers(raw.as_bytes()).unwrap_err();
            assert!(!e.contains(&secretish[..16]), "{e}");
        }
    }

    #[test]
    fn roles_never_share_a_name_or_a_hash() {
        let a = vec![
            ("claude".to_string(), [1; 32]),
            ("dsh".to_string(), [2; 32]),
        ];
        assert!(check_distinct(&a, &[("gate".into(), [3; 32])]).is_ok());
        assert!(check_distinct(&a, &[]).is_ok());
        for (o, why) in [
            (vec![("dsh".to_string(), [3; 32])], "agent name"),
            (vec![("gate".to_string(), [2; 32])], "as an agent"),
            (
                vec![("gate".to_string(), [3; 32]), ("gate".to_string(), [4; 32])],
                "named twice",
            ),
        ] {
            let e = check_distinct(&a, &o).unwrap_err().to_string();
            assert!(e.contains(why), "{e}");
        }
        let twins = vec![
            ("claude".to_string(), [1; 32]),
            ("dsh".to_string(), [1; 32]),
        ];
        assert!(check_distinct(&twins, &[]).is_err());
    }

    #[test]
    fn missing_default_file_means_none_but_an_explicit_one_must_exist() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("observers.json");
        assert!(load_observers(&p, false).unwrap().is_empty());
        assert!(load_observers(&p, true).is_err());
        std::fs::write(&p, "").unwrap();
        assert!(load_observers(&p, false).is_err());
    }

    #[test]
    fn unreadable_file_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("observers.json");
        std::fs::write(&p, format!(r#"{{"g":"{}"}}"#, h('a'))).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&p).is_ok() {
            // Root reads it anyway: this case is not covered here, and the test says so.
            eprintln!("SKIPPED unreadable_file_is_an_error: this user can read a 0000 file");
            return;
        }
        let e = load_observers(&p, false).unwrap_err().to_string();
        assert!(e.contains("permission denied"), "{e}");
    }

    fn task(id: &str, n: u32) -> (ObservedTask, Vec<ObservedMessage>) {
        let at = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let msgs: Vec<ObservedMessage> = (1..=n)
            .map(|n| ObservedMessage {
                n,
                from: "claude".into(),
                kind: Kind::Note,
                at,
            })
            .collect();
        (
            ObservedTask {
                id: id.into(),
                parent: None,
                from: "claude".into(),
                to: "dsh".into(),
                state: State::Open,
                created_at: at,
                updated_at: at,
                message_count: n,
                last_n: n,
                participants: BTreeSet::from(["claude".into(), "dsh".into()]),
            },
            msgs,
        )
    }

    #[test]
    fn participants_include_every_sender() {
        // The bridge lets only the two parties post today; the set does not rely on that.
        let at = "2026-10-18T00:00:00Z";
        let m: Message = serde_json::from_value(serde_json::json!({
            "task": "t", "n": 2, "from": "codex", "kind": "note", "at": at, "protocol": "2"
        }))
        .unwrap();
        let d = tempfile::tempdir().unwrap();
        let mut b = Bridge::open(d.path(), &["claude", "dsh"]).unwrap();
        b.create_task(
            "t",
            "claude",
            serde_json::from_value(serde_json::json!({
                "protocol": "2", "body": "b", "meta": { "to": "dsh", "title": "x" }
            }))
            .unwrap(),
        )
        .unwrap();
        let t = b.task("t").unwrap().clone();
        let o = observe_task(&t, &[m]);
        assert_eq!(
            o.participants.into_iter().collect::<Vec<_>>(),
            ["claude", "codex", "dsh"]
        );
    }

    #[test]
    fn paging_is_exclusive_and_ends_with_none() {
        let all: Vec<_> = ["a", "b", "c"].iter().map(|i| task(i, 1)).collect();
        let (p, n) = list_page(&all, None, 2);
        assert_eq!((p.len(), n.as_deref()), (2, Some("b")));
        let (p, n) = list_page(&all, Some("b"), 2);
        assert_eq!((p[0].id.as_str(), p.len(), n), ("c", 1, None));
        let (p, n) = list_page(&all, None, 3);
        assert_eq!((p.len(), n), (3, None));
        let (p, n) = list_page(&all, Some("c"), 3);
        assert_eq!((p.len(), n), (0, None));
        let (_, msgs) = task("x", 5);
        let (p, n) = message_page(&msgs, Some(2), 2);
        assert_eq!((p[0].n, p.len(), n), (3, 2, Some(4)));
        let (p, n) = message_page(&msgs, Some(4), 2);
        assert_eq!((p[0].n, p.len(), n), (5, 1, None));
    }

    #[test]
    fn revision_covers_every_task_and_nothing_else() {
        let all: Vec<_> = ["a", "m", "z"].iter().map(|i| task(i, 2)).collect();
        let r = revision(&all);
        assert_eq!(r.len(), 64);
        assert_eq!(r, revision(&all.clone()), "same content, same revision");
        let mut later = all.clone();
        later[2].0.state = State::Working;
        assert_ne!(r, revision(&later), "a change on the last page");
        let mut more = all.clone();
        more.insert(0, task("0", 1));
        assert_ne!(r, revision(&more), "a task before the cursor");
        let mut msg = all.clone();
        msg[1].1[0].kind = Kind::Answer;
        assert_ne!(r, revision(&msg), "a message's kind");
    }
}
