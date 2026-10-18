//! Implementation sources and independence (plans/M2-design.md §3.8): who took part in a phase,
//! which vendor each of them belongs to, and every reason the auditor is not independent of
//! them or the sources are not known. Pure: the caller supplies the verified records, each
//! commit's attestation result and one bridge read (`agent_bridge::observe::collect`).
//!
//! - **Sources unknown** (no release): the bridge could not be read; the window is inverted; an
//!   implementer task in the window belongs to no registered phase and is not ignored for this
//!   one; a parent chain is broken or circular; a link names a task that does not exist; a link
//!   or ignore record is malformed.
//! - **Not independent** (no release): a commit lacks a valid commit-review attestation; a
//!   participant or the auditor maps to no vendor, to two vendors, or is malformed; the auditor
//!   shares a vendor with a participant.
//!
//! "Valid attestation" means [`crate::attest::verify`] passed for that commit: the review-gate
//! signature, not a git GPG/SSH signature. Merge commits are assembled into the audit input,
//! but the current attestation check does not support them (it requires exactly one parent),
//! so a phase containing one cannot be released.
//!
//! Identity names are self-declared (model A): an agent of one vendor named `claude-x` maps to
//! anthropic. The vendor table is the user's to maintain.

use std::collections::{BTreeMap, BTreeSet};

use super::record::{Record, canonical, sha256_hex};
use agent_bridge::observe::{Collected, ObservedMessage, ObservedTask};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where a name was seen; a rule names one of these or `Any`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Any,
    Bridge,
    GitAuthor,
    Trailer,
    Auditor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Match {
    Exact,
    Prefix,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub source: Source,
    #[serde(rename = "match")]
    pub matcher: Match,
    pub value: String,
    pub vendor: String,
}

/// `state/milestones/vendors.json`: changed only by the user (a `vendors_changed` record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vendors {
    pub version: u32,
    pub rules: Vec<Rule>,
}

impl Vendors {
    /// The first table (§3.8): `claude*` → anthropic; codex, auditor → openai; dsh → deepseek;
    /// the user (git author victormshan) → user. No default for any other identity.
    pub fn first() -> Self {
        let any = |m, v: &str, vendor: &str| Rule {
            source: Source::Any,
            matcher: m,
            value: v.into(),
            vendor: vendor.into(),
        };
        Vendors {
            version: 1,
            rules: vec![
                any(Match::Prefix, "claude", "anthropic"),
                any(Match::Exact, "codex", "openai"),
                any(Match::Exact, "auditor", "openai"),
                any(Match::Exact, "dsh", "deepseek"),
                Rule {
                    source: Source::GitAuthor,
                    matcher: Match::Exact,
                    value: "victormshan".into(),
                    vendor: "user".into(),
                },
            ],
        }
    }

    /// sha256 of the table's canonical JSON (recorded with each request).
    pub fn content_sha256(&self) -> Result<String> {
        let v = serde_json::to_value(self).context("serializing the vendor table")?;
        let c = canonical(&v)?;
        Ok(sha256_hex(c.as_bytes()))
    }

    /// Every rule that matches is collected (never first-match): one vendor maps, none or
    /// several do not. Names compare with ASCII case folded; nothing else is normalised.
    pub fn map(&self, source: Source, name: &str) -> Mapping {
        if malformed(name) {
            return Mapping::Malformed;
        }
        let n = name.to_ascii_lowercase();
        let vendors: BTreeSet<String> = self
            .rules
            .iter()
            .filter(|r| r.source == Source::Any || r.source == source)
            .filter(|r| {
                let v = r.value.to_ascii_lowercase();
                match r.matcher {
                    Match::Exact => n == v,
                    Match::Prefix => n.starts_with(&v),
                }
            })
            .map(|r| r.vendor.clone())
            .collect();
        match vendors.len() {
            0 => Mapping::Unmapped,
            1 => Mapping::Vendor(vendors.into_iter().next().expect("one")),
            _ => Mapping::Conflict(vendors.into_iter().collect()),
        }
    }
}

/// Empty, blank, or holding a control character: refused as it is, never cleaned up first.
fn malformed(name: &str) -> bool {
    name.trim().is_empty() || name.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case", tag = "result", content = "detail")]
pub enum Mapping {
    Vendor(String),
    Unmapped,
    Conflict(Vec<String>),
    Malformed,
}

/// The commit-review attestation check of one commit, done by the caller against a fixed notes
/// commit ([`crate::attest::verify_commit_at`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attestation {
    Verified,
    Missing,
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitCheck {
    pub id: String,
    pub author: String,
    /// Raw values of the `Co-Authored-By` trailers as git parses them.
    pub trailers: Vec<String>,
    pub attestation: Attestation,
}

/// The display name of a `Co-Authored-By` value `Name <address>`; `None` if it is not that.
pub fn trailer_name(value: &str) -> Option<&str> {
    let v = value.trim();
    let (name, rest) = v.split_once('<')?;
    let addr = rest.strip_suffix('>')?;
    let name = name.trim();
    (!name.is_empty() && !addr.contains('<') && !addr.contains('>')).then_some(name)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Participant {
    pub name: String,
    pub source: Source,
    /// The commit or bridge task it was seen in.
    pub seen_in: String,
    pub mapping: Mapping,
}

pub struct Inputs<'a> {
    pub phase: &'a str,
    /// The verified chain (all records, every phase).
    pub records: &'a [Record],
    pub commits: &'a [CommitCheck],
    /// Committer time of `head`: the end of the window.
    pub head_time: DateTime<Utc>,
    /// One bridge read, or why it failed.
    pub bridge: std::result::Result<&'a Collected, String>,
    /// Bridge identities of the implementer (configuration, e.g. `["claude"]`).
    pub implementers: &'a BTreeSet<String>,
    pub auditor: &'a str,
    pub vendors: &'a Vendors,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub participants: Vec<Participant>,
    pub auditor: Mapping,
    /// Bridge tasks of this phase (with its predecessors').
    pub tasks: BTreeSet<String>,
    pub unmatched: Vec<String>,
    pub sources_known: bool,
    pub sources_reasons: Vec<String>,
    pub independent: bool,
    pub independence_reasons: Vec<String>,
}

/// Phases registered on the chain and which ones each supersedes.
struct Phases {
    registered: BTreeMap<String, String>, // phase → `at` of its registration
    predecessors: BTreeMap<String, BTreeSet<String>>,
}

fn phases(records: &[Record]) -> Phases {
    let mut registered = BTreeMap::new();
    let mut predecessors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in records {
        match r.kind.as_str() {
            "phase_registered" => {
                registered.entry(r.phase.clone()).or_insert(r.at.clone());
                if let Some(old) = r.body.get("supersedes").and_then(Value::as_str) {
                    predecessors
                        .entry(r.phase.clone())
                        .or_default()
                        .insert(old.into());
                }
            }
            "phase_superseded" => {
                if let Some(new) = r.body.get("by").and_then(Value::as_str) {
                    predecessors
                        .entry(new.into())
                        .or_default()
                        .insert(r.phase.clone());
                }
            }
            _ => {}
        }
    }
    Phases {
        registered,
        predecessors,
    }
}

impl Phases {
    /// `p` and every phase it supersedes, transitively (terminates on cycles).
    fn lineage(&self, p: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut todo = vec![p.to_string()];
        while let Some(x) = todo.pop() {
            if out.insert(x.clone())
                && let Some(ps) = self.predecessors.get(&x)
            {
                todo.extend(ps.iter().cloned());
            }
        }
        out
    }
}

/// A task's chain of ancestors (itself first), or why it is broken.
fn ancestors<'a>(
    id: &'a str,
    tasks: &'a BTreeMap<&str, &ObservedTask>,
) -> std::result::Result<Vec<&'a str>, String> {
    let mut chain = vec![id];
    let mut seen = BTreeSet::from([id]);
    let mut cur = id;
    while let Some(p) = tasks.get(cur).and_then(|t| t.parent.as_deref()) {
        if !tasks.contains_key(p) {
            return Err(format!(
                "bridge task {cur}: parent {p} does not exist (broken parent chain)"
            ));
        }
        if !seen.insert(p) {
            return Err(format!("bridge task {id}: parent chain is circular"));
        }
        chain.push(p);
        cur = p;
    }
    Ok(chain)
}

fn parse_at(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

pub fn judge(i: &Inputs) -> Outcome {
    let mut sources_reasons = Vec::new();
    let mut independence_reasons = Vec::new();
    let mut participants = Vec::new();
    let ph = phases(i.records);
    let lineage = ph.lineage(i.phase);

    // Commits: authors, trailers, attestations.
    for c in i.commits {
        participants.push(Participant {
            name: c.author.clone(),
            source: Source::GitAuthor,
            seen_in: c.id.clone(),
            mapping: i.vendors.map(Source::GitAuthor, &c.author),
        });
        for t in &c.trailers {
            let mapping = match trailer_name(t) {
                Some(n) => i.vendors.map(Source::Trailer, n),
                None => Mapping::Malformed,
            };
            participants.push(Participant {
                name: trailer_name(t).unwrap_or(t).to_string(),
                source: Source::Trailer,
                seen_in: c.id.clone(),
                mapping,
            });
        }
        match &c.attestation {
            Attestation::Verified => {}
            Attestation::Missing => independence_reasons
                .push(format!("commit {} has no commit-review attestation", c.id)),
            Attestation::Invalid(why) => independence_reasons.push(format!(
                "commit {} has no valid commit-review attestation: {why}",
                c.id
            )),
        }
    }

    let mut phase_tasks = BTreeSet::new();
    let mut unmatched = Vec::new();
    match &i.bridge {
        Err(why) => sources_reasons.push(format!("bridge could not be read: {why}")),
        Ok(collected) => {
            let tasks: BTreeMap<&str, &ObservedTask> = collected
                .tasks
                .iter()
                .map(|(t, _)| (t.id.as_str(), t))
                .collect();
            let messages: BTreeMap<&str, &Vec<ObservedMessage>> = collected
                .tasks
                .iter()
                .map(|(t, m)| (t.id.as_str(), m))
                .collect();
            // Links and ignores, per phase.
            let mut linked: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            let mut ignored: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for r in i.records {
                let into = match r.kind.as_str() {
                    "task_linked" => &mut linked,
                    "task_ignored" => &mut ignored,
                    _ => continue,
                };
                match r.body.get("task").and_then(Value::as_str) {
                    Some(t) if !t.is_empty() => {
                        if r.kind == "task_linked" && !tasks.contains_key(t) {
                            sources_reasons.push(format!(
                                "record {}: linked bridge task {t} does not exist",
                                r.seq
                            ));
                        }
                        into.entry(r.phase.clone()).or_default().insert(t.into());
                    }
                    _ => {
                        sources_reasons.push(format!("record {} ({}) names no task", r.seq, r.kind))
                    }
                }
            }
            // Chains, once; a broken one is a reason only where it matters (below).
            let chains: BTreeMap<&str, std::result::Result<Vec<&str>, String>> = tasks
                .keys()
                .map(|id| (*id, ancestors(id, &tasks)))
                .collect();
            // Each registered phase's own tasks: id prefix or a link, then their descendants;
            // for a link also its ancestors.
            let own = |x: &str| -> BTreeSet<String> {
                let prefix = format!("{x}-");
                let links = linked.get(x);
                let mut seeds: BTreeSet<&str> = tasks
                    .keys()
                    .copied()
                    .filter(|id| id.starts_with(&prefix))
                    .collect();
                let mut out: BTreeSet<String> = BTreeSet::new();
                // Matched by prefix: a member whatever its own parent chain looks like (a broken
                // chain is reported below, and its participants still count).
                out.extend(seeds.iter().map(|s| s.to_string()));
                for l in links.into_iter().flatten() {
                    if let Some(Ok(chain)) = chains.get(l.as_str()) {
                        out.extend(chain.iter().map(|s| s.to_string()));
                    }
                    if tasks.contains_key(l.as_str()) {
                        seeds.insert(l.as_str());
                    }
                }
                for (id, chain) in &chains {
                    if let Ok(chain) = chain
                        && chain.iter().any(|a| seeds.contains(a))
                    {
                        out.insert(id.to_string());
                    }
                }
                out
            };
            let mut member: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for x in ph.registered.keys() {
                let mut m = BTreeSet::new();
                for y in ph.lineage(x) {
                    m.extend(own(&y));
                }
                member.insert(x.clone(), m);
            }
            let matched: BTreeSet<&String> = member.values().flatten().collect();
            phase_tasks = member.get(i.phase).cloned().unwrap_or_default();
            for id in &phase_tasks {
                if let Some(Err(why)) = chains.get(id.as_str()) {
                    sources_reasons.push(why.clone());
                }
                if let Some(t) = tasks.get(id.as_str()) {
                    for p in &t.participants {
                        participants.push(Participant {
                            name: p.clone(),
                            source: Source::Bridge,
                            seen_in: id.clone(),
                            mapping: i.vendors.map(Source::Bridge, p),
                        });
                    }
                }
            }
            // The window and the tasks in it that belong to no phase.
            let start = ph.registered.get(i.phase).and_then(|a| parse_at(a));
            match start {
                None => sources_reasons.push(format!(
                    "phase {} has no readable registration time",
                    i.phase
                )),
                Some(s) if s > i.head_time => sources_reasons.push(format!(
                    "window inverted: head committed at {} before the phase was registered at {s}",
                    i.head_time
                )),
                Some(s) => {
                    let e = i.head_time;
                    let inside = |t: DateTime<Utc>| s <= t && t <= e;
                    let ignored_here: BTreeSet<&String> = lineage
                        .iter()
                        .filter_map(|p| ignored.get(p))
                        .flatten()
                        .collect();
                    for (id, t) in &tasks {
                        let implementer =
                            i.implementers.contains(&t.from) || i.implementers.contains(&t.to);
                        let in_window = inside(t.created_at)
                            || messages
                                .get(id)
                                .is_some_and(|ms| ms.iter().any(|m| inside(m.at)));
                        let id = id.to_string();
                        if implementer
                            && in_window
                            && !matched.contains(&id)
                            && !ignored_here.contains(&id)
                        {
                            if let Some(Err(why)) = chains.get(id.as_str()) {
                                sources_reasons.push(why.clone());
                            }
                            unmatched.push(id);
                        }
                    }
                }
            }
            for u in &unmatched {
                sources_reasons.push(format!(
                    "bridge task {u} of the implementer in the window belongs to no phase"
                ));
            }
        }
    }

    // Vendors and independence.
    let auditor = i.vendors.map(Source::Auditor, i.auditor);
    match &auditor {
        Mapping::Vendor(_) => {}
        m => independence_reasons.push(format!("auditor {}: {}", i.auditor, describe(m))),
    }
    let mut seen = BTreeSet::new();
    for p in &participants {
        if !seen.insert((p.source, p.name.clone(), p.mapping.clone())) {
            continue;
        }
        match (&p.mapping, &auditor) {
            (Mapping::Vendor(v), Mapping::Vendor(a)) if v == a => {
                independence_reasons.push(format!(
                    "{} ({:?}, {}) shares vendor {v} with the auditor",
                    p.name, p.source, p.seen_in
                ))
            }
            (Mapping::Vendor(_), _) => {}
            (m, _) => independence_reasons.push(format!(
                "{} ({:?}, {}): {}",
                p.name,
                p.source,
                p.seen_in,
                describe(m)
            )),
        }
    }
    sources_reasons.dedup();
    Outcome {
        participants,
        auditor,
        tasks: phase_tasks,
        unmatched,
        sources_known: sources_reasons.is_empty(),
        sources_reasons,
        independent: independence_reasons.is_empty(),
        independence_reasons,
    }
}

/// One read of the bridge as the observer `token` (agent-bridge observe.rs): the same client
/// and protocol checks the bridge ships, over HTTP. Any failure is an error, never a partial
/// result.
pub async fn read_bridge(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> std::result::Result<Collected, agent_bridge::observe::CollectError> {
    agent_bridge::observe::collect(|path| {
        let req = http.get(format!("{base_url}{path}")).bearer_auth(token);
        async move {
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            let v: Value = resp.json().await.map_err(|e| e.to_string())?;
            Ok((status, v))
        }
    })
    .await
}

fn describe(m: &Mapping) -> String {
    match m {
        Mapping::Vendor(v) => format!("vendor {v}"),
        Mapping::Unmapped => "maps to no vendor".into(),
        Mapping::Conflict(vs) => format!("maps to several vendors {vs:?}"),
        Mapping::Malformed => "malformed identity".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridge::model::{Kind, State};
    use serde_json::json;

    fn t0() -> DateTime<Utc> {
        parse_at("2026-10-18T00:00:00Z").unwrap()
    }

    fn at(h: i64) -> DateTime<Utc> {
        t0() + chrono::Duration::hours(h)
    }

    fn rec(seq: u64, phase: &str, kind: &str, at: &str, body: Value) -> Record {
        Record {
            format: 1,
            seq,
            prev: String::new(),
            at: at.into(),
            repo: "r".into(),
            key_id: "k".into(),
            kind: kind.into(),
            phase: phase.into(),
            body,
            hash: String::new(),
            sig: String::new(),
        }
    }

    /// A task created at hour `h`, with messages at the given hours.
    fn task(
        id: &str,
        parent: Option<&str>,
        from: &str,
        to: &str,
        h: i64,
        msgs: &[i64],
    ) -> (ObservedTask, Vec<ObservedMessage>) {
        let ms: Vec<ObservedMessage> = std::iter::once(h)
            .chain(msgs.iter().copied())
            .enumerate()
            .map(|(i, mh)| ObservedMessage {
                n: i as u32 + 1,
                from: if i == 0 { from.into() } else { to.into() },
                kind: if i == 0 { Kind::Task } else { Kind::Note },
                at: at(mh),
            })
            .collect();
        (
            ObservedTask {
                id: id.into(),
                parent: parent.map(String::from),
                from: from.into(),
                to: to.into(),
                state: State::Open,
                created_at: at(h),
                updated_at: ms.last().unwrap().at,
                message_count: ms.len() as u32,
                last_n: ms.len() as u32,
                participants: BTreeSet::from([from.to_string(), to.to_string()]),
            },
            ms,
        )
    }

    fn collected(tasks: Vec<(ObservedTask, Vec<ObservedMessage>)>) -> Collected {
        let mut tasks = tasks;
        tasks.sort_by(|a, b| a.0.id.cmp(&b.0.id));
        Collected {
            revision: "rev".into(),
            tasks,
        }
    }

    fn commit(author: &str, trailers: &[&str]) -> CommitCheck {
        CommitCheck {
            id: "c1".into(),
            author: author.into(),
            trailers: trailers.iter().map(|s| s.to_string()).collect(),
            attestation: Attestation::Verified,
        }
    }

    /// P registered at hour 0 (superseding O); Q registered too.
    fn base_records() -> Vec<Record> {
        vec![
            rec(
                1,
                "o",
                "phase_registered",
                "2026-10-17T00:00:00Z",
                json!({"conditions": []}),
            ),
            rec(
                2,
                "o",
                "phase_superseded",
                "2026-10-17T12:00:00Z",
                json!({"by": "p"}),
            ),
            rec(
                3,
                "p",
                "phase_registered",
                "2026-10-18T00:00:00Z",
                json!({"supersedes": "o", "conditions": []}),
            ),
            rec(
                4,
                "q",
                "phase_registered",
                "2026-10-18T00:00:00Z",
                json!({"conditions": []}),
            ),
        ]
    }

    fn base_tasks() -> Vec<(ObservedTask, Vec<ObservedMessage>)> {
        vec![
            task("p-a", None, "claude", "dsh", 1, &[2]),
            task("x1", Some("p-a"), "claude", "dsh", 2, &[]),
            task("o-c", None, "claude", "dsh", -20, &[]),
            task("q-b", None, "claude", "codex", 3, &[]),
            task("z", None, "dsh", "claude", 4, &[]),
            task("bystander", None, "dsh", "codex", 4, &[]),
            task("before", None, "claude", "dsh", -5, &[]),
        ]
    }

    struct Case {
        records: Vec<Record>,
        tasks: Vec<(ObservedTask, Vec<ObservedMessage>)>,
        commits: Vec<CommitCheck>,
        head: DateTime<Utc>,
        auditor: &'static str,
        bridge_err: Option<String>,
        vendors: Vendors,
    }

    impl Case {
        fn new() -> Self {
            Case {
                records: base_records(),
                tasks: base_tasks(),
                commits: vec![commit(
                    "victormshan",
                    &["Claude Opus 5.5 <noreply@anthropic.com>"],
                )],
                head: at(10),
                auditor: "auditor",
                bridge_err: None,
                vendors: Vendors::first(),
            }
        }
        fn run(&self) -> Outcome {
            let c = collected(self.tasks.clone());
            let implementers = BTreeSet::from(["claude".to_string()]);
            judge(&Inputs {
                phase: "p",
                records: &self.records,
                commits: &self.commits,
                head_time: self.head,
                bridge: match &self.bridge_err {
                    Some(e) => Err(e.clone()),
                    None => Ok(&c),
                },
                implementers: &implementers,
                auditor: self.auditor,
                vendors: &self.vendors,
            })
        }
        fn add(&mut self, phase: &str, kind: &str, body: Value) {
            let seq = self.records.len() as u64 + 1;
            self.records
                .push(rec(seq, phase, kind, "2026-10-18T05:00:00Z", body));
        }
    }

    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn first_table_maps_as_the_design_says() {
        let v = Vendors::first();
        let vendor = |s: &str| Mapping::Vendor(s.into());
        for (src, name, want) in [
            (Source::Bridge, "claude", vendor("anthropic")),
            (Source::Trailer, "Claude Opus 5.5", vendor("anthropic")),
            (Source::Trailer, "CLAUDE x", vendor("anthropic")),
            (Source::GitAuthor, "claude-bot", vendor("anthropic")),
            (Source::Auditor, "codex", vendor("openai")),
            (Source::Trailer, "Codex", vendor("openai")),
            (Source::Auditor, "auditor", vendor("openai")),
            (Source::Bridge, "auditor", vendor("openai")),
            (Source::Auditor, "dsh", vendor("deepseek")),
            (Source::GitAuthor, "DSH", vendor("deepseek")),
            (Source::GitAuthor, "victormshan", vendor("user")),
            (Source::Bridge, "victormshan", Mapping::Unmapped),
            (Source::Bridge, "codex-collab", Mapping::Unmapped),
            (Source::Bridge, "opencode", Mapping::Unmapped),
            (Source::Auditor, "gpt", Mapping::Unmapped),
            (Source::Bridge, "", Mapping::Malformed),
            (Source::Bridge, "  ", Mapping::Malformed),
            (Source::Trailer, "a\nb", Mapping::Malformed),
            (Source::Trailer, "a\u{7}", Mapping::Malformed),
        ] {
            assert_eq!(v.map(src, name), want, "{src:?} {name:?}");
        }
        assert_eq!(v.content_sha256().unwrap().len(), 64);
        assert_ne!(
            v.content_sha256().unwrap(),
            Vendors {
                version: 2,
                ..Vendors::first()
            }
            .content_sha256()
            .unwrap()
        );
    }

    #[test]
    fn overlapping_rules_conflict_instead_of_first_match() {
        let mut v = Vendors::first();
        v.rules.push(Rule {
            source: Source::Trailer,
            matcher: Match::Prefix,
            value: "Claude".into(),
            vendor: "openai".into(),
        });
        assert_eq!(
            v.map(Source::Trailer, "Claude x"),
            Mapping::Conflict(vec!["anthropic".into(), "openai".into()])
        );
        // The same vendor twice is one vendor.
        v.rules.push(Rule {
            source: Source::Bridge,
            matcher: Match::Exact,
            value: "claude".into(),
            vendor: "anthropic".into(),
        });
        assert_eq!(
            v.map(Source::Bridge, "claude"),
            Mapping::Vendor("anthropic".into())
        );
    }

    #[test]
    fn trailer_names() {
        assert_eq!(
            trailer_name("Claude Opus 5.5 <noreply@anthropic.com>"),
            Some("Claude Opus 5.5")
        );
        assert_eq!(trailer_name("  X  <a@b> "), Some("X"));
        for bad in ["X", "<a@b>", "X <a@b", "X a@b>", "X <a<b>", " <a@b>"] {
            assert_eq!(trailer_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_clean_phase_is_known_and_independent() {
        let mut c = Case::new();
        c.tasks.retain(|(t, _)| t.id != "z");
        let o = c.run();
        assert!(o.sources_known, "{:?}", o.sources_reasons);
        assert!(o.independent, "{:?}", o.independence_reasons);
        // Own task, its descendant, the predecessor's task; not Q's, not the bystander.
        assert_eq!(o.tasks, set(&["o-c", "p-a", "x1"]));
        assert_eq!(o.auditor, Mapping::Vendor("openai".into()));
    }

    #[test]
    fn tasks_of_no_phase_are_unmatched_tasks_of_another_phase_are_not() {
        let o = Case::new().run();
        assert_eq!(o.unmatched, vec!["z".to_string()]);
        assert!(!o.sources_known);
        assert!(!o.tasks.contains("q-b"), "another phase's task is not ours");
        assert!(
            !o.unmatched.contains(&"bystander".to_string()),
            "no implementer"
        );
        assert!(
            !o.unmatched.contains(&"before".to_string()),
            "outside the window"
        );
    }

    #[test]
    fn ignores_count_for_their_phase_and_its_successors_only() {
        let mut c = Case::new();
        c.add("q", "task_ignored", json!({"task": "z", "reason": "r"}));
        assert_eq!(
            c.run().unmatched,
            vec!["z".to_string()],
            "Q's ignore is not P's"
        );
        let mut c = Case::new();
        c.add("p", "task_ignored", json!({"task": "z", "reason": "r"}));
        assert!(c.run().sources_known);
        let mut c = Case::new();
        c.add("o", "task_ignored", json!({"task": "z", "reason": "r"}));
        assert!(
            c.run().sources_known,
            "an ignore of the superseded phase carries over"
        );
    }

    #[test]
    fn a_link_to_another_phase_matches_without_joining_this_one() {
        let mut c = Case::new();
        c.add("q", "task_linked", json!({"task": "z", "reason": "r"}));
        let o = c.run();
        assert!(o.sources_known, "{:?}", o.sources_reasons);
        assert!(!o.tasks.contains("z"));
    }

    #[test]
    fn a_link_brings_its_ancestors_and_descendants() {
        let mut c = Case::new();
        c.tasks.push(task("root", None, "dsh", "codex", -30, &[]));
        c.tasks
            .push(task("mid", Some("root"), "dsh", "codex", -29, &[]));
        c.tasks
            .push(task("kid", Some("mid"), "dsh", "codex", 5, &[]));
        c.tasks
            .push(task("grandkid", Some("kid"), "dsh", "codex", 6, &[]));
        c.add("p", "task_linked", json!({"task": "mid", "reason": "r"}));
        let o = c.run();
        for t in ["root", "mid", "kid", "grandkid"] {
            assert!(o.tasks.contains(t), "{t}");
        }
        // Participants come along: dsh and codex of those tasks.
        assert!(
            o.participants
                .iter()
                .any(|p| p.seen_in == "grandkid" && p.name == "codex")
        );
    }

    #[test]
    fn broken_and_circular_chains_are_unknown_sources() {
        let has = |o: &Outcome, w: &str| o.sources_reasons.iter().any(|r| r.contains(w));
        // An implementer task outside any phase with a broken chain.
        let mut c = Case::new();
        c.tasks.retain(|(t, _)| t.id != "z");
        c.tasks
            .push(task("orphan", Some("gone"), "claude", "dsh", 3, &[]));
        assert!(has(&c.run(), "gone does not exist"));
        // Phase members that are not the implementer's, so only the phase's own check sees them.
        let mut c = Case::new();
        c.tasks.retain(|(t, _)| t.id != "z");
        c.tasks
            .push(task("p-orphan", Some("gone"), "dsh", "codex", 3, &[]));
        let o = c.run();
        assert!(has(&o, "gone does not exist"), "{:?}", o.sources_reasons);
        assert!(
            o.tasks.contains("p-orphan"),
            "a prefix match stays a member"
        );
        let mut c = Case::new();
        c.tasks.retain(|(t, _)| t.id != "z");
        c.tasks
            .push(task("p-loop1", Some("loop2"), "dsh", "codex", 3, &[]));
        c.tasks
            .push(task("loop2", Some("p-loop1"), "dsh", "codex", 3, &[]));
        let o = c.run();
        assert!(has(&o, "circular"), "{:?}", o.sources_reasons);
        assert!(
            o.participants
                .iter()
                .any(|p| p.seen_in == "p-loop1" && p.name == "codex"),
            "its participants still count"
        );
    }

    #[test]
    fn bad_links_and_ignores_are_unknown_sources() {
        let mut c = Case::new();
        c.add("p", "task_linked", json!({"task": "nope", "reason": "r"}));
        c.add("p", "task_ignored", json!({"reason": "r"}));
        let o = c.run();
        assert!(
            o.sources_reasons
                .iter()
                .any(|r| r.contains("nope does not exist"))
        );
        assert!(
            o.sources_reasons
                .iter()
                .any(|r| r.contains("names no task"))
        );
    }

    #[test]
    fn the_window_is_closed_and_never_inverted() {
        let mut c = Case::new();
        c.tasks = vec![
            task("on-start", None, "claude", "dsh", 0, &[]),
            task("msg-on-end", None, "dsh", "claude", -3, &[10]),
            task("after", None, "claude", "dsh", 11, &[]),
            task("just-before", None, "claude", "dsh", -1, &[]),
        ];
        let o = c.run();
        assert_eq!(
            o.unmatched,
            vec!["msg-on-end".to_string(), "on-start".to_string()]
        );
        let mut c = Case::new();
        c.head = at(-1);
        let o = c.run();
        assert!(
            o.sources_reasons
                .iter()
                .any(|r| r.contains("window inverted"))
        );
        let mut c = Case::new();
        c.records[2].at = "yesterday".into();
        assert!(
            c.run()
                .sources_reasons
                .iter()
                .any(|r| r.contains("no readable registration time"))
        );
    }

    #[test]
    fn an_unreadable_bridge_is_unknown_sources() {
        let mut c = Case::new();
        c.bridge_err = Some("bridge history incomplete".into());
        let o = c.run();
        assert!(!o.sources_known);
        assert!(o.sources_reasons[0].contains("incomplete"));
    }

    #[test]
    fn independence_is_checked_on_every_participant_and_the_auditor() {
        // An openai auditor shares a vendor with codex in p-codex.
        let mut c = Case::new();
        c.tasks
            .push(task("p-codex", None, "claude", "codex", 2, &[]));
        c.auditor = "codex";
        let o = c.run();
        assert!(!o.independent);
        assert!(
            o.independence_reasons
                .iter()
                .any(|r| r.contains("codex") && r.contains("openai"))
        );
        // A deepseek auditor clashes with dsh in x1.
        let mut c = Case::new();
        c.auditor = "dsh";
        assert!(
            c.run()
                .independence_reasons
                .iter()
                .any(|r| r.contains("deepseek"))
        );
        // Unknown and conflicting auditors.
        let mut c = Case::new();
        c.auditor = "gpt";
        assert!(
            c.run()
                .independence_reasons
                .iter()
                .any(|r| r.contains("auditor gpt: maps to no vendor"))
        );
        let mut c = Case::new();
        c.vendors.rules.push(Rule {
            source: Source::Auditor,
            matcher: Match::Exact,
            value: "auditor".into(),
            vendor: "x".into(),
        });
        assert!(
            c.run()
                .independence_reasons
                .iter()
                .any(|r| r.contains("several vendors"))
        );
    }

    #[test]
    fn independence_needs_attestations_and_mapped_participants() {
        let mut c = Case::new();
        c.tasks.retain(|(t, _)| t.id != "z" && t.id != "p-a");
        c.auditor = "auditor";
        // No openai participant: the openai auditor is independent of the rest.
        assert!(c.run().independent, "{:?}", c.run().independence_reasons);
        let mut d = Case::new();
        d.tasks.retain(|(t, _)| t.id != "z" && t.id != "p-a");
        d.commits[0].attestation = Attestation::Missing;
        assert!(
            d.run()
                .independence_reasons
                .iter()
                .any(|r| r.contains("no commit-review attestation"))
        );
        d.commits[0].attestation = Attestation::Invalid("merge commit".into());
        assert!(
            d.run()
                .independence_reasons
                .iter()
                .any(|r| r.contains("merge commit"))
        );
        let mut e = Case::new();
        e.tasks.retain(|(t, _)| t.id != "z" && t.id != "p-a");
        e.tasks
            .push(task("p-oc", None, "claude", "opencode", 2, &[]));
        assert!(
            e.run()
                .independence_reasons
                .iter()
                .any(|r| r.contains("opencode") && r.contains("no vendor"))
        );
        let mut f = Case::new();
        f.tasks.retain(|(t, _)| t.id != "z" && t.id != "p-a");
        f.commits[0].trailers.push("nobody".into());
        assert!(
            f.run()
                .independence_reasons
                .iter()
                .any(|r| r.contains("malformed"))
        );
    }

    #[tokio::test]
    async fn reads_a_real_bridge_through_the_observer_routes() {
        use agent_bridge::bridge::Bridge;
        use agent_bridge::service::{AppState, serve, sha256};
        let dir = tempfile::tempdir().unwrap();
        let bridge = Bridge::open(dir.path().join("state"), &["claude", "dsh"]).unwrap();
        let state = AppState::new(
            bridge,
            vec![
                ("claude".into(), sha256("ct")),
                ("dsh".into(), sha256("dt")),
            ],
        )
        .unwrap()
        .with_observers(vec![("review-gate".into(), sha256("ot"))])
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(std::sync::Arc::new(state), listener));
        let http = reqwest::Client::new();
        for id in ["p-a", "p-b"] {
            let r = http
                .post(format!("{url}/v1/tasks"))
                .bearer_auth("ct")
                .json(
                    &json!({ "id": id, "kind": "task", "protocol": "2", "body": "b",
                               "meta": { "to": "dsh", "title": "t" } }),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
        }
        let c = read_bridge(&http, &url, "ot").await.unwrap();
        assert_eq!(c.tasks.len(), 2);
        assert_eq!(c.tasks[0].0.id, "p-a");
        assert_eq!(c.tasks[0].0.participants, set(&["claude", "dsh"]));
        assert_eq!(c.tasks[0].1.len(), 1);
        // An agent token, a wrong token, a dead address: errors, never a partial result.
        assert!(read_bridge(&http, &url, "ct").await.is_err());
        assert!(read_bridge(&http, &url, "nope").await.is_err());
        assert!(matches!(
            read_bridge(&http, "http://127.0.0.1:9", "ot").await,
            Err(agent_bridge::observe::CollectError::Transport(_))
        ));
    }
}
