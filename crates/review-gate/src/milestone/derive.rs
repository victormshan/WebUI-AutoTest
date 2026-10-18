//! Release state of a phase, derived from one verified snapshot of the record chain
//! (plans/M2-design.md §3.4). Pure: no I/O, no clock.
//!
//! State (current request, closures, revocations, pauses, open conditions, the count) comes
//! from **all** records, archived or not, so a new request, a revocation or a rebaseline takes
//! effect at once. Release additionally needs every record that produces it to be archived.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::record::Record;

/// Pause threshold: valid REWORK/BLOCK verdicts in a row (user decision of 2026-10-14).
pub const THRESHOLD: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    Pending,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condition {
    pub id: String,
    pub text: String,
    pub blocking: bool,
    /// Where it came from: "registration" or the seq of the audit that raised it.
    pub origin: String,
    /// None while open; else "resolved_by_audit" or "waived_by_user" with the record seq.
    pub closed: Option<(String, u64)>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CurrentRequest {
    pub request_id: String,
    pub seq: u64,
    pub head: String,
    /// The baseline (seq of its registration or `baseline_changed`) the request is bound to.
    pub baseline_seq: Option<u64>,
    /// The independence check recorded with the request (§3.8), and why it failed.
    pub independent: bool,
    pub independence_note: String,
    /// Whether the request's implementation sources were all found (§3.8), and if not, why.
    pub sources_known: bool,
    pub sources_note: String,
    pub state: RequestState,
    pub verdict: Option<String>,
    pub audit_seq: Option<u64>,
    pub closed_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PhaseState {
    pub phase: String,
    pub registered: bool,
    pub superseded_by: Option<String>,
    pub baseline_seq: Option<u64>,
    pub current: Option<CurrentRequest>,
    pub conditions: Vec<Condition>,
    pub count: u32,
    pub paused: bool,
    pub released: bool,
    /// Every reason release is withheld (empty when released).
    pub reasons: Vec<String>,
}

/// Fields each record kind must carry (string or integer as noted). A record without them is
/// malformed: it is never read as an empty value, and the phase stops releasing.
fn required(kind: &str) -> &'static [(&'static str, bool)] {
    // (field, is_integer)
    match kind {
        "phase_superseded" => &[("by", false)],
        "baseline_changed" => &[("task_book_sha256", false)],
        "request" => &[
            ("request_id", false),
            ("head", false),
            ("baseline_seq", true),
        ],
        "job" => &[("request_id", false)],
        "audit" => &[("request_id", false)],
        "request_closed" => &[("request_id", false), ("state", false)],
        "condition_resolved" => &[("condition", false), ("audit_seq", true)],
        "condition_waived" => &[("condition", false), ("reason", false)],
        "source_dropped" | "task_linked" | "task_ignored" | "revoke" => &[("reason", false)],
        _ => &[],
    }
}

fn malformed(r: &Record) -> Option<String> {
    for (k, int) in required(&r.kind) {
        let ok = match r.body.get(*k) {
            Some(Value::String(v)) => !*int && !v.is_empty(),
            Some(v) => *int && v.as_u64().is_some(),
            None => false,
        };
        if !ok {
            return Some(format!("record {} ({}) lacks {k}", r.seq, r.kind));
        }
    }
    if r.kind == "audit"
        && r.body.get("valid") == Some(&Value::Bool(true))
        && s(&r.body, "verdict").is_empty()
    {
        return Some(format!(
            "record {} (audit) is valid but has no verdict",
            r.seq
        ));
    }
    // Conditions are never read with defaults: a condition whose blocking flag is missing would
    // otherwise silently count as non-blocking (DSH review note N2).
    let conditions_of = |need_blocking: bool| -> Option<String> {
        let list = match r.body.get("conditions") {
            None => return None,
            Some(Value::Array(a)) => a,
            Some(_) => {
                return Some(format!(
                    "record {} ({}): conditions is not a list",
                    r.seq, r.kind
                ));
            }
        };
        for (i, c) in list.iter().enumerate() {
            let id_ok = c
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty());
            let text_ok = c.get("text").and_then(Value::as_str).is_some();
            let blocking_ok = !need_blocking || c.get("blocking").is_some_and(Value::is_boolean);
            if !(id_ok && text_ok && blocking_ok) {
                return Some(format!(
                    "record {} ({}): condition {} lacks id, text{}",
                    r.seq,
                    r.kind,
                    i + 1,
                    if need_blocking {
                        " or a boolean blocking flag"
                    } else {
                        ""
                    }
                ));
            }
        }
        None
    };
    // Every request says whether its implementation sources are complete (§3.8): a missing
    // flag is never read as "known" (DSH review note N4).
    if r.kind == "request"
        && !r
            .body
            .pointer("/sources/known")
            .is_some_and(Value::is_boolean)
    {
        return Some(format!(
            "record {} (request) lacks a boolean sources.known",
            r.seq
        ));
    }
    match r.kind.as_str() {
        "audit" if r.body.get("valid") == Some(&Value::Bool(true)) => conditions_of(true),
        "phase_registered" => conditions_of(false),
        _ => None,
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn u(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(Value::as_u64)
}

/// Derives the state of `phase` from a verified chain. `checkpoint` is the last archived seq
/// (0 when nothing is archived yet).
pub fn derive(records: &[Record], phase: &str, checkpoint: u64) -> PhaseState {
    derive_inner(records, phase, checkpoint, 0)
}

fn derive_inner(records: &[Record], phase: &str, checkpoint: u64, depth: usize) -> PhaseState {
    let mut st = PhaseState {
        phase: phase.to_string(),
        registered: false,
        superseded_by: None,
        baseline_seq: None,
        current: None,
        conditions: Vec::new(),
        count: 0,
        paused: false,
        released: false,
        reasons: Vec::new(),
    };
    // Records that must be archived for a release to count (§3.4 ② item 7).
    let mut needed: BTreeSet<u64> = BTreeSet::new();
    let mut registration_seq = None;
    let mut revoked_after_request = false;
    // Source-handling records (task links etc.) seen before the current request.
    let mut source_records: Vec<u64> = Vec::new();
    // Per request id: its request seq, and whether it was current when each later record came.
    let mut requests: BTreeMap<String, u64> = BTreeMap::new();

    let mut bad: Vec<String> = Vec::new();
    for r in records.iter().filter(|r| r.phase == phase) {
        let b = &r.body;
        if let Some(why) = malformed(r) {
            bad.push(why);
            continue;
        }
        match r.kind.as_str() {
            "phase_registered" => {
                if registration_seq.is_some() {
                    bad.push(format!("record {}: phase {phase} registered twice", r.seq));
                    continue;
                }
                st.registered = true;
                registration_seq = Some(r.seq);
                st.baseline_seq = Some(r.seq);
                // A superseding phase inherits the open conditions and the count (§3.1) as they
                // stood when it was registered: only the records before its registration count,
                // so nothing the old phase gets later (a late result, a waiver, a resume) changes
                // what was inherited. The old phase must have been registered before, which also
                // rules out superseding oneself and cycles.
                if let Some(old) = b.get("supersedes").and_then(Value::as_str) {
                    let before = &records[..records.partition_point(|x| x.seq < r.seq)];
                    let known = before
                        .iter()
                        .any(|x| x.kind == "phase_registered" && x.phase == old);
                    if !known || depth >= 16 {
                        bad.push(format!(
                            "record {}: supersedes {old:?}, which was not registered before",
                            r.seq
                        ));
                        continue;
                    }
                    let prev = derive_inner(before, old, checkpoint, depth + 1);
                    st.count = prev.count;
                    for mut c in prev.conditions.into_iter().filter(|c| c.closed.is_none()) {
                        // Keep the lineage: where the condition came from, through which phase.
                        c.origin = format!("{old}:{}", c.origin);
                        st.conditions.push(c);
                    }
                }
                for c in b
                    .get("conditions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    st.conditions.push(Condition {
                        id: s(c, "id").into(),
                        text: s(c, "text").into(),
                        blocking: true,
                        origin: "registration".into(),
                        closed: None,
                    });
                }
            }
            "phase_superseded" => {
                let by = s(b, "by");
                match &st.superseded_by {
                    Some(x) if x != by => bad.push(format!(
                        "record {}: phase {phase} superseded by both {x} and {by}",
                        r.seq
                    )),
                    _ => st.superseded_by = Some(by.into()),
                }
            }
            "baseline_changed" => {
                st.baseline_seq = Some(r.seq);
                // The current release ends; an unfinished request on the old baseline ends.
                if let Some(c) = st.current.as_mut()
                    && c.state == RequestState::Pending
                {
                    c.state = RequestState::Cancelled;
                    c.closed_seq = Some(r.seq);
                }
            }
            "request" => {
                let id = s(b, "request_id").to_string();
                // Audits and closures find their request by id alone, so a reused id would let a
                // late result of the earlier request act on the new one (DSH review note N3).
                if let Some(first) = requests.insert(id.clone(), r.seq) {
                    bad.push(format!(
                        "record {} (request) reuses request id {id} of record {first}",
                        r.seq
                    ));
                    continue;
                }
                revoked_after_request = false;
                let independent = b.pointer("/independence/ok") == Some(&Value::Bool(true));
                st.current = Some(CurrentRequest {
                    request_id: id,
                    seq: r.seq,
                    head: s(b, "head").into(),
                    baseline_seq: u(b, "baseline_seq"),
                    independent,
                    independence_note: b
                        .pointer("/independence/why")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    sources_known: b.pointer("/sources/known") == Some(&Value::Bool(true)),
                    sources_note: b
                        .pointer("/sources/why")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    state: RequestState::Pending,
                    verdict: None,
                    audit_seq: None,
                    closed_seq: None,
                });
                // A request only counts if bound to the baseline in force.
                if u(b, "baseline_seq") != st.baseline_seq
                    && let Some(c) = st.current.as_mut()
                {
                    c.state = RequestState::Failed;
                    c.closed_seq = Some(r.seq);
                }
            }
            "audit" => {
                // Only the current, still pending request's valid audit has any effect.
                let Some(c) = st.current.as_mut() else {
                    continue;
                };
                if s(b, "request_id") != c.request_id || c.state != RequestState::Pending {
                    continue;
                }
                if b.get("valid") != Some(&Value::Bool(true)) {
                    continue;
                }
                // One valid audit per request: a second one is kept in the log but has no effect
                // (DSH review note N5).
                if c.audit_seq.is_some() {
                    continue;
                }
                let verdict = s(b, "verdict").to_string();
                c.verdict = Some(verdict.clone());
                c.audit_seq = Some(r.seq);
                if verdict == "REWORK" || verdict == "BLOCK" {
                    st.count += 1;
                }
                for n in b
                    .get("conditions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    st.conditions.push(Condition {
                        id: s(n, "id").into(),
                        text: s(n, "text").into(),
                        blocking: n.get("blocking") == Some(&Value::Bool(true)),
                        origin: r.seq.to_string(),
                        closed: None,
                    });
                }
            }
            "request_closed" => {
                let Some(c) = st.current.as_mut() else {
                    continue;
                };
                // A terminal state is written once and never rewritten.
                if s(b, "request_id") != c.request_id || c.state != RequestState::Pending {
                    continue;
                }
                c.state = match s(b, "state") {
                    "completed" if c.audit_seq.is_some() => RequestState::Completed,
                    "completed" => RequestState::Failed,
                    "timed_out" => RequestState::TimedOut,
                    "cancelled" => RequestState::Cancelled,
                    _ => RequestState::Failed,
                };
                c.closed_seq = Some(r.seq);
            }
            "condition_resolved" => {
                // Only from the current request's valid audit, while that request is current.
                let audit_seq = u(b, "audit_seq");
                let ok = st
                    .current
                    .as_ref()
                    .is_some_and(|c| c.audit_seq.is_some() && c.audit_seq == audit_seq);
                if ok {
                    close(
                        &mut st.conditions,
                        s(b, "condition"),
                        "resolved_by_audit",
                        r.seq,
                    );
                }
            }
            "condition_waived" => {
                close(
                    &mut st.conditions,
                    s(b, "condition"),
                    "waived_by_user",
                    r.seq,
                );
            }
            "source_dropped" | "task_linked" | "task_ignored" => source_records.push(r.seq),
            "revoke" => revoked_after_request = true,
            "resume" => {
                st.count = 0;
                st.paused = false;
            }
            _ => {}
        }
        // The run of rejections ends with a release (§3.4 ④), not with an approval alone: an
        // approval that still leaves a blocking condition open does not reset the count (DSH
        // review note N1). Archival is left out here, so the count does not depend on how far
        // the log has been published; it only decides whether the release is visible.
        if eligible_apart_from_archival(&st, revoked_after_request) {
            st.count = 0;
        }
        if st.count >= THRESHOLD {
            st.paused = true;
        }
    }

    // A phase is superseded as soon as either record says so: the new phase's registration
    // (written first) or the old phase's `phase_superseded`. Both must name the same phase.
    for r in records.iter().filter(|r| {
        r.phase != phase
            && r.kind == "phase_registered"
            && r.body.get("supersedes").and_then(Value::as_str) == Some(phase)
    }) {
        match &st.superseded_by {
            Some(x) if *x != r.phase => bad.push(format!(
                "phase {phase} superseded by both {x} and {} (record {})",
                r.phase, r.seq
            )),
            _ => st.superseded_by = Some(r.phase.clone()),
        }
    }

    // ---- release ----
    let mut why = Vec::new();
    for b in &bad {
        why.push(format!("malformed record, phase stops releasing: {b}"));
    }
    if !st.registered {
        why.push("phase not registered (or not confirmed by the user)".to_string());
    }
    if let Some(by) = &st.superseded_by {
        why.push(format!("superseded by phase {by}"));
    }
    if st.paused {
        why.push(format!(
            "paused after {} REWORK/BLOCK verdicts; only the user can resume",
            st.count
        ));
    }
    match &st.current {
        None => why.push("no audit request".into()),
        Some(c) => {
            if c.state != RequestState::Completed {
                why.push(format!("current request {} is {:?}", c.request_id, c.state));
            }
            if c.baseline_seq != st.baseline_seq {
                why.push("the baseline changed after this request; a new request is needed".into());
            }
            if !c.independent {
                why.push(format!(
                    "independence check failed: {}",
                    c.independence_note
                ));
            }
            if !c.sources_known {
                why.push(format!(
                    "implementation sources unknown: {}",
                    c.sources_note
                ));
            }
            match c.verdict.as_deref() {
                Some("APPROVE") | Some("APPROVE WITH CONDITIONS") => {}
                Some(v) => why.push(format!("verdict {v}")),
                None => why.push("no valid audit of the current request".into()),
            }
            needed.extend(
                [Some(c.seq), c.audit_seq, c.closed_seq]
                    .into_iter()
                    .flatten(),
            );
            needed.extend(
                records
                    .iter()
                    .filter(|r| {
                        r.phase == phase
                            && r.kind == "job"
                            && s(&r.body, "request_id") == c.request_id
                    })
                    .map(|r| r.seq),
            );
            needed.extend(source_records.iter().filter(|q| **q < c.seq));
        }
    }
    if revoked_after_request {
        why.push("revoked".into());
    }
    let open: Vec<&Condition> = st
        .conditions
        .iter()
        .filter(|c| c.blocking && c.closed.is_none())
        .collect();
    if !open.is_empty() {
        let ids: Vec<&str> = open.iter().map(|c| c.id.as_str()).collect();
        why.push(format!("open blocking conditions: {}", ids.join(", ")));
    }
    needed.extend(registration_seq);
    needed.extend(st.baseline_seq);
    needed.extend(
        st.conditions
            .iter()
            .filter(|c| c.blocking)
            .filter_map(|c| c.closed.as_ref().map(|x| x.1)),
    );
    let unarchived: Vec<u64> = needed.into_iter().filter(|q| *q > checkpoint).collect();
    if !unarchived.is_empty() {
        why.push(format!(
            "not archived yet: records {}",
            unarchived
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    st.released = why.is_empty();
    st.reasons = why;
    st
}

/// Whether the current request would release the phase if every record were archived.
fn eligible_apart_from_archival(st: &PhaseState, revoked: bool) -> bool {
    let Some(c) = &st.current else { return false };
    st.registered
        && st.superseded_by.is_none()
        && !revoked
        && c.state == RequestState::Completed
        && matches!(
            c.verdict.as_deref(),
            Some("APPROVE") | Some("APPROVE WITH CONDITIONS")
        )
        && c.baseline_seq == st.baseline_seq
        && c.independent
        && c.sources_known
        && !st
            .conditions
            .iter()
            .any(|x| x.blocking && x.closed.is_none())
}

fn close(conditions: &mut [Condition], id: &str, how: &str, seq: u64) {
    if let Some(c) = conditions
        .iter_mut()
        .find(|c| c.id == id && c.closed.is_none())
    {
        c.closed = Some((how.to_string(), seq));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attest::Keypair;
    use crate::milestone::record::{Draft, seal};
    use serde_json::json;

    /// A chain under construction.
    struct Log {
        kp: Keypair,
        recs: Vec<Record>,
    }

    impl Log {
        fn new() -> Self {
            Self {
                kp: Keypair::from_seed([7; 32]),
                recs: Vec::new(),
            }
        }

        fn add(&mut self, phase: &str, kind: &str, body: Value) -> u64 {
            let d = Draft {
                kind: kind.into(),
                phase: phase.into(),
                body,
            };
            let r = seal(&self.kp, self.recs.last(), "repo", "t", d).unwrap();
            let seq = r.seq;
            self.recs.push(r);
            seq
        }

        fn last(&self) -> u64 {
            self.recs.len() as u64
        }

        fn register(&mut self, phase: &str, conds: &[&str]) -> u64 {
            let conditions: Vec<Value> = conds
                .iter()
                .map(|c| json!({"id": c, "text": format!("acceptance {c}")}))
                .collect();
            self.add(phase, "phase_registered", json!({"conditions": conditions}))
        }

        fn request(&mut self, phase: &str, id: &str, baseline: u64) -> u64 {
            self.add(
                phase,
                "request",
                json!({"request_id": id, "head": "h", "baseline_seq": baseline,
                       "independence": {"ok": true}, "sources": {"known": true}}),
            )
        }

        /// job + valid audit + resolutions + closure for request `id`.
        fn audit(&mut self, phase: &str, id: &str, verdict: &str, resolve: &[&str]) -> u64 {
            self.add(phase, "job", json!({"request_id": id}));
            let a = self.add(
                phase,
                "audit",
                json!({"request_id": id, "verdict": verdict, "valid": true,
                       "conditions": [], "resolved": resolve}),
            );
            for c in resolve {
                self.add(
                    phase,
                    "condition_resolved",
                    json!({"condition": c, "audit_seq": a}),
                );
            }
            a
        }

        fn close(&mut self, phase: &str, id: &str, state: &str) -> u64 {
            self.add(
                phase,
                "request_closed",
                json!({"request_id": id, "state": state}),
            )
        }

        fn state(&self, phase: &str, checkpoint: u64) -> PhaseState {
            derive(&self.recs, phase, checkpoint)
        }
    }

    /// Registered with acceptance condition c1, audited APPROVE resolving c1, closed.
    fn approved() -> (Log, u64) {
        let mut l = Log::new();
        let reg = l.register("p", &["c1"]);
        l.request("p", "r1", reg);
        l.audit("p", "r1", "APPROVE", &["c1"]);
        l.close("p", "r1", "completed");
        (l, reg)
    }

    #[test]
    fn an_approved_archived_request_releases() {
        let (l, _) = approved();
        let st = l.state("p", l.last());
        assert!(st.released, "{:?}", st.reasons);
        assert_eq!(
            st.conditions[0].closed.as_ref().unwrap().0,
            "resolved_by_audit"
        );
    }

    #[test]
    fn an_approval_that_is_not_archived_does_not_release() {
        let (l, _) = approved();
        let st = l.state("p", 2);
        assert!(!st.released);
        assert!(st.reasons.iter().any(|r| r.starts_with("not archived yet")));
    }

    #[test]
    fn an_unarchived_new_request_ends_the_old_approval_at_once() {
        let (mut l, reg) = approved();
        let archived = l.last();
        l.request("p", "r2", reg);
        let st = l.state("p", archived);
        assert!(!st.released);
        assert_eq!(st.current.unwrap().request_id, "r2");
    }

    #[test]
    fn an_unarchived_revoke_ends_the_approval_at_once() {
        let (mut l, _) = approved();
        let archived = l.last();
        l.add("p", "revoke", json!({"reason": "found a problem"}));
        let st = l.state("p", archived);
        assert!(!st.released);
        assert!(st.reasons.contains(&"revoked".to_string()));
    }

    #[test]
    fn approve_cannot_skip_an_open_blocking_condition() {
        let mut l = Log::new();
        let reg = l.register("p", &["c1"]);
        l.request("p", "r1", reg);
        l.audit("p", "r1", "APPROVE", &[]); // c1 not mentioned: stays open
        l.close("p", "r1", "completed");
        let st = l.state("p", l.last());
        assert!(!st.released);
        assert!(
            st.reasons
                .iter()
                .any(|r| r.contains("open blocking conditions: c1"))
        );
    }

    #[test]
    fn a_waiver_releases_only_once_archived() {
        let mut l = Log::new();
        let reg = l.register("p", &["c1"]);
        l.request("p", "r1", reg);
        l.audit("p", "r1", "APPROVE", &[]);
        l.close("p", "r1", "completed");
        let before = l.last();
        l.add(
            "p",
            "condition_waived",
            json!({"condition": "c1", "reason": "user"}),
        );
        assert!(!l.state("p", before).released, "unarchived waiver");
        let st = l.state("p", l.last());
        assert!(st.released, "{:?}", st.reasons);
        assert_eq!(
            st.conditions[0].closed.as_ref().unwrap().0,
            "waived_by_user"
        );
    }

    #[test]
    fn a_late_result_neither_resolves_nor_counts() {
        let mut l = Log::new();
        let reg = l.register("p", &["c1"]);
        l.request("p", "r1", reg);
        l.request("p", "r2", reg); // r1 replaced before its result came
        let late = l.add(
            "p",
            "audit",
            json!({"request_id": "r1", "verdict": "REWORK", "valid": true, "conditions": []}),
        );
        l.add(
            "p",
            "condition_resolved",
            json!({"condition": "c1", "audit_seq": late}),
        );
        let st = l.state("p", l.last());
        assert_eq!(st.count, 0);
        assert!(st.conditions[0].closed.is_none());
        assert_eq!(st.current.unwrap().state, RequestState::Pending);
    }

    #[test]
    fn a_terminal_state_is_never_rewritten() {
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.request("p", "r1", reg);
        l.audit("p", "r1", "APPROVE", &[]); // a valid approval arrives in time ...
        l.close("p", "r1", "timed_out"); // ... but the request is closed as timed out first
        l.close("p", "r1", "completed"); // a later "completed" must not rewrite that
        let st = l.state("p", l.last());
        assert_eq!(st.current.unwrap().state, RequestState::TimedOut);
        assert!(!st.released);
    }

    #[test]
    fn three_rejections_pause_until_the_user_resumes() {
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        for i in 0..3 {
            let id = format!("r{i}");
            l.request("p", &id, reg);
            l.audit("p", &id, "REWORK", &[]);
            l.close("p", &id, "completed");
        }
        let st = l.state("p", l.last());
        assert!(st.paused && st.count == 3);
        l.add("p", "resume", json!({}));
        let st = l.state("p", l.last());
        assert!(!st.paused && st.count == 0);
    }

    #[test]
    fn invalid_audits_and_run_failures_do_not_count() {
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.request("p", "r1", reg);
        l.add(
            "p",
            "audit",
            json!({"request_id": "r1", "verdict": "REWORK", "valid": false}),
        );
        l.close("p", "r1", "failed");
        l.request("p", "r2", reg);
        l.close("p", "r2", "timed_out");
        assert_eq!(l.state("p", l.last()).count, 0);
    }

    #[test]
    fn a_superseding_phase_keeps_open_conditions_and_the_count() {
        let mut l = Log::new();
        let reg = l.register("old", &["c1"]);
        l.request("old", "r1", reg);
        l.audit("old", "r1", "REWORK", &[]);
        l.close("old", "r1", "completed");
        l.add("old", "phase_superseded", json!({"by": "new"}));
        let reg2 = l.add(
            "new",
            "phase_registered",
            json!({"supersedes": "old", "conditions": []}),
        );
        let st = l.state("new", l.last());
        assert_eq!(st.count, 1);
        assert_eq!(st.conditions.len(), 1, "c1 carried over");
        // Approving the new phase without resolving c1 does not release.
        l.request("new", "r2", reg2);
        l.audit("new", "r2", "APPROVE", &[]);
        l.close("new", "r2", "completed");
        let st = l.state("new", l.last());
        assert!(!st.released);
        assert!(st.reasons.iter().any(|r| r.contains("c1")));
        // The old phase itself never releases again.
        assert!(!l.state("old", l.last()).released);
    }

    #[test]
    fn a_rebaseline_ends_approvals_and_pending_requests_but_keeps_conditions_and_count() {
        // After an approval.
        let (mut l, _) = approved();
        let archived = l.last();
        let b2 = l.add("p", "baseline_changed", json!({"task_book_sha256": "x"}));
        let st = l.state("p", archived);
        assert!(!st.released, "even before the change is archived");
        assert!(st.reasons.iter().any(|r| r.contains("baseline changed")));
        // A request on the new baseline is needed.
        l.request("p", "r2", b2);
        l.audit("p", "r2", "APPROVE", &[]);
        l.close("p", "r2", "completed");
        assert!(l.state("p", l.last()).released);

        // While a job runs: the request ends, its late result has no effect.
        let mut l = Log::new();
        let reg = l.register("p", &["c1"]);
        l.request("p", "r1", reg);
        l.add("p", "baseline_changed", json!({"task_book_sha256": "x"}));
        l.audit("p", "r1", "REWORK", &["c1"]);
        let st = l.state("p", l.last());
        assert_eq!(st.current.unwrap().state, RequestState::Cancelled);
        assert_eq!(st.count, 0);
        assert!(st.conditions[0].closed.is_none());
    }

    #[test]
    fn a_request_on_an_old_baseline_or_without_independence_never_releases() {
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.add("p", "baseline_changed", json!({"task_book_sha256": "x"}));
        l.request("p", "r1", reg); // bound to the old baseline
        l.audit("p", "r1", "APPROVE", &[]);
        l.close("p", "r1", "completed");
        assert!(!l.state("p", l.last()).released);

        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.add(
            "p",
            "request",
            json!({"request_id": "r1", "head": "h", "baseline_seq": reg,
                   "independence": {"ok": false, "why": "auditor and implementer share a vendor"},
                   "sources": {"known": true}}),
        );
        l.audit("p", "r1", "APPROVE", &[]);
        l.close("p", "r1", "completed");
        let st = l.state("p", l.last());
        assert!(!st.released);
        assert!(st.reasons.iter().any(|r| r.contains("share a vendor")));
    }

    #[test]
    fn unarchived_source_handling_cannot_release() {
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.add(
            "p",
            "task_linked",
            json!({"task": "p-impl", "reason": "user"}),
        );
        let before = l.last();
        l.request("p", "r1", reg);
        l.audit("p", "r1", "APPROVE", &[]);
        l.close("p", "r1", "completed");
        assert!(l.state("p", l.last()).released);
        let st = l.state("p", before - 1);
        assert!(!st.released);
        assert!(st.reasons.iter().any(|r| r.contains(&before.to_string())));
    }

    #[test]
    fn a_record_missing_a_field_is_never_read_as_empty_and_stops_release() {
        let (mut l, _) = approved();
        l.add("p", "phase_superseded", json!({})); // no "by"
        let st = l.state("p", l.last());
        assert!(!st.released);
        assert_eq!(st.superseded_by, None, "not read as an empty phase name");
        assert!(st.reasons.iter().any(|r| r.contains("lacks by")));

        let (mut l, _) = approved();
        l.add("p", "revoke", json!({})); // no reason
        assert!(
            l.state("p", l.last())
                .reasons
                .iter()
                .any(|r| r.contains("malformed"))
        );

        let (mut l, _) = approved();
        l.add(
            "p",
            "condition_resolved",
            json!({"condition": "c9", "audit_seq": "3"}),
        );
        assert!(
            !l.state("p", l.last()).released,
            "audit_seq must be an integer"
        );
    }

    #[test]
    fn a_condition_without_its_blocking_flag_is_malformed_not_non_blocking() {
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.request("p", "r1", reg);
        l.add("p", "job", json!({"request_id": "r1"}));
        l.add(
            "p",
            "audit",
            json!({"request_id": "r1", "verdict": "APPROVE WITH CONDITIONS", "valid": true,
                   "conditions": [{"id": "a1", "text": "fix the parser"}]}),
        );
        l.close("p", "r1", "completed");
        let st = l.state("p", l.last());
        assert!(!st.released, "{:?}", st.reasons);
        assert!(
            st.reasons
                .iter()
                .any(|r| r.contains("boolean blocking flag")),
            "{:?}",
            st.reasons
        );
        assert!(
            st.conditions.is_empty(),
            "the condition is not taken as non-blocking either"
        );

        // A string "true" is not a boolean either.
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.request("p", "r1", reg);
        l.add(
            "p",
            "audit",
            json!({"request_id": "r1", "verdict": "APPROVE", "valid": true,
                   "conditions": [{"id": "a1", "text": "x", "blocking": "true"}]}),
        );
        assert!(
            l.state("p", l.last())
                .reasons
                .iter()
                .any(|r| r.contains("malformed"))
        );

        // A registration condition needs an id.
        let mut l = Log::new();
        l.add(
            "p",
            "phase_registered",
            json!({"conditions": [{"text": "no id"}]}),
        );
        let st = l.state("p", l.last());
        assert!(!st.registered && st.reasons.iter().any(|r| r.contains("lacks id")));

        // An invalid audit's conditions are not read at all, so they are not checked.
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.request("p", "r1", reg);
        l.add(
            "p",
            "audit",
            json!({"request_id": "r1", "verdict": "", "valid": false, "conditions": [{}]}),
        );
        assert!(
            !l.state("p", l.last())
                .reasons
                .iter()
                .any(|r| r.contains("malformed"))
        );
    }

    #[test]
    fn the_count_resets_on_release_not_on_an_approval_that_leaves_a_blocking_condition_open() {
        // REWORK, REWORK, APPROVE WITH CONDITIONS whose blocking condition stays open, REWORK:
        // three counted rejections in a row, so the phase pauses (DSH review note N1).
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        for (i, v) in ["REWORK", "REWORK"].iter().enumerate() {
            let id = format!("r{i}");
            l.request("p", &id, reg);
            l.audit("p", &id, v, &[]);
            l.close("p", &id, "completed");
        }
        l.request("p", "r2", reg);
        l.add("p", "job", json!({"request_id": "r2"}));
        l.add(
            "p",
            "audit",
            json!({"request_id": "r2", "verdict": "APPROVE WITH CONDITIONS", "valid": true,
                   "conditions": [{"id": "a1", "text": "still open", "blocking": true}]}),
        );
        l.close("p", "r2", "completed");
        assert_eq!(
            l.state("p", l.last()).count,
            2,
            "no reset: a1 is still open"
        );
        l.request("p", "r3", reg);
        l.audit("p", "r3", "REWORK", &[]);
        l.close("p", "r3", "completed");
        let st = l.state("p", l.last());
        assert_eq!(st.count, 3);
        assert!(st.paused);

        // When the approval does release (nothing left open), the run ends there.
        let mut l = Log::new();
        let reg = l.register("p", &["c1"]);
        for i in 0..2 {
            let id = format!("r{i}");
            l.request("p", &id, reg);
            l.audit("p", &id, "REWORK", &[]);
            l.close("p", &id, "completed");
        }
        l.request("p", "r2", reg);
        l.audit("p", "r2", "APPROVE", &["c1"]);
        l.close("p", "r2", "completed");
        assert_eq!(
            l.state("p", 0).count,
            0,
            "reset even before the log is archived"
        );
        l.request("p", "r3", reg);
        l.audit("p", "r3", "REWORK", &[]);
        assert_eq!(l.state("p", l.last()).count, 1);
    }

    #[test]
    fn an_approval_of_a_revoked_request_does_not_end_the_run() {
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        for i in 0..2 {
            let id = format!("r{i}");
            l.request("p", &id, reg);
            l.audit("p", &id, "REWORK", &[]);
            l.close("p", &id, "completed");
        }
        l.request("p", "r2", reg);
        l.add(
            "p",
            "revoke",
            json!({"reason": "user revoked while r2 ran"}),
        );
        l.audit("p", "r2", "APPROVE", &[]);
        l.close("p", "r2", "completed");
        let st = l.state("p", l.last());
        assert!(!st.released);
        assert_eq!(
            st.count, 2,
            "a revoked request does not release, so the run goes on"
        );
    }

    #[test]
    fn a_waiver_that_completes_a_release_also_ends_the_run() {
        let mut l = Log::new();
        let reg = l.register("p", &["c1"]);
        for i in 0..2 {
            let id = format!("r{i}");
            l.request("p", &id, reg);
            l.audit("p", &id, "REWORK", &[]);
            l.close("p", &id, "completed");
        }
        l.request("p", "r2", reg);
        l.audit("p", "r2", "APPROVE", &[]);
        l.close("p", "r2", "completed");
        assert_eq!(l.state("p", l.last()).count, 2, "c1 still open");
        l.add(
            "p",
            "condition_waived",
            json!({"condition": "c1", "reason": "user"}),
        );
        assert_eq!(l.state("p", l.last()).count, 0);
    }

    #[test]
    fn a_valid_audit_condition_with_a_boolean_blocking_flag_is_accepted() {
        // Positive case for the N2 validation (DSH's suggestion): true and false both parse.
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.request("p", "r1", reg);
        l.add("p", "job", json!({"request_id": "r1"}));
        l.add(
            "p",
            "audit",
            json!({"request_id": "r1", "verdict": "APPROVE WITH CONDITIONS", "valid": true,
                   "conditions": [{"id": "a1", "text": "must fix", "blocking": true},
                                  {"id": "a2", "text": "nice to have", "blocking": false}]}),
        );
        l.close("p", "r1", "completed");
        let st = l.state("p", l.last());
        assert!(
            !st.reasons.iter().any(|r| r.contains("malformed")),
            "{:?}",
            st.reasons
        );
        let got: Vec<(&str, bool)> = st
            .conditions
            .iter()
            .map(|c| (c.id.as_str(), c.blocking))
            .collect();
        assert_eq!(got, vec![("a1", true), ("a2", false)]);
        assert!(
            st.reasons
                .iter()
                .any(|r| r.contains("open blocking conditions: a1"))
        );
    }

    #[test]
    fn a_reused_request_id_is_malformed_and_cannot_pick_up_an_old_result() {
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.request("p", "r1", reg);
        l.request("p", "r2", reg);
        l.request("p", "r1", reg); // the id of the first request, again
        l.audit("p", "r1", "APPROVE", &[]);
        l.close("p", "r1", "completed");
        let st = l.state("p", l.last());
        assert!(!st.released);
        assert!(
            st.reasons
                .iter()
                .any(|r| r.contains("reuses request id r1")),
            "{:?}",
            st.reasons
        );
        assert_eq!(
            st.current.unwrap().request_id,
            "r2",
            "the reused id never became current"
        );
    }

    #[test]
    fn only_the_first_valid_audit_of_a_request_counts() {
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.request("p", "r1", reg);
        l.add("p", "job", json!({"request_id": "r1"}));
        for v in ["REWORK", "APPROVE"] {
            l.add(
                "p",
                "audit",
                json!({"request_id": "r1", "verdict": v, "valid": true,
                       "conditions": [{"id": format!("{v}-c"), "text": "x", "blocking": true}]}),
            );
        }
        let st = l.state("p", l.last());
        assert_eq!(st.count, 1, "the second audit adds nothing");
        assert_eq!(st.current.unwrap().verdict.as_deref(), Some("REWORK"));
        let ids: Vec<&str> = st.conditions.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["REWORK-c"]);
    }

    #[test]
    fn unknown_implementation_sources_stop_release_and_a_missing_flag_is_malformed() {
        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.add(
            "p",
            "request",
            json!({"request_id": "r1", "head": "h", "baseline_seq": reg,
                   "independence": {"ok": true},
                   "sources": {"known": false, "why": "unmatched bridge task p-x"}}),
        );
        l.audit("p", "r1", "APPROVE", &[]);
        l.close("p", "r1", "completed");
        let st = l.state("p", l.last());
        assert!(!st.released);
        assert!(
            st.reasons
                .iter()
                .any(|r| r.contains("sources unknown: unmatched bridge task p-x"))
        );

        let mut l = Log::new();
        let reg = l.register("p", &[]);
        l.add(
            "p",
            "request",
            json!({"request_id": "r1", "head": "h", "baseline_seq": reg,
                   "independence": {"ok": true}}),
        );
        let st = l.state("p", l.last());
        assert!(
            st.reasons
                .iter()
                .any(|r| r.contains("lacks a boolean sources.known"))
        );
        assert!(
            st.current.is_none(),
            "the malformed request never became current"
        );
    }

    #[test]
    fn inherited_conditions_keep_their_lineage() {
        let mut l = Log::new();
        l.register("old", &["c1"]);
        l.add("old", "phase_superseded", json!({"by": "new"}));
        l.add(
            "new",
            "phase_registered",
            json!({"supersedes": "old", "conditions": []}),
        );
        let st = l.state("new", l.last());
        assert_eq!(st.conditions[0].origin, "old:registration");
    }

    #[test]
    fn an_unregistered_phase_never_releases() {
        let mut l = Log::new();
        l.request("p", "r1", 0);
        l.audit("p", "r1", "APPROVE", &[]);
        l.close("p", "r1", "completed");
        let st = l.state("p", l.last());
        assert!(!st.released && !st.registered);
    }

    fn supersede(l: &mut Log, new: &str, old: &str, conds: &[&str]) -> u64 {
        let conditions: Vec<Value> = conds
            .iter()
            .map(|c| json!({"id": c, "text": format!("acceptance {c}")}))
            .collect();
        l.add(
            new,
            "phase_registered",
            json!({"supersedes": old, "conditions": conditions}),
        )
    }

    fn open_ids(st: &PhaseState) -> Vec<String> {
        st.conditions
            .iter()
            .filter(|c| c.closed.is_none())
            .map(|c| c.id.clone())
            .collect()
    }

    #[test]
    fn inheritance_is_fixed_at_registration() {
        // old: c1 open, one REWORK; new registered; then old gets a late approval resolving
        // c1, a waiver and a resume. None of it reaches new.
        let mut l = Log::new();
        let reg = l.register("old", &["c1", "c2"]);
        l.request("old", "r1", reg);
        l.audit("old", "r1", "REWORK", &[]);
        l.close("old", "r1", "completed");
        l.request("old", "r2", reg);
        supersede(&mut l, "new", "old", &[]);
        let at_registration = l.state("new", l.last());
        assert_eq!(at_registration.count, 1);
        assert_eq!(open_ids(&at_registration), ["c1", "c2"]);
        l.audit("old", "r2", "APPROVE", &["c1"]);
        l.close("old", "r2", "completed");
        l.add(
            "old",
            "condition_waived",
            json!({"condition": "c2", "reason": "late"}),
        );
        l.add("old", "resume", json!({}));
        let later = l.state("new", l.last());
        assert_eq!(
            later.count, 1,
            "the old phase's resume does not reset new's count"
        );
        assert_eq!(
            open_ids(&later),
            ["c1", "c2"],
            "late resolutions are not inherited"
        );
        // The old phase is superseded by new's registration alone, before any
        // phase_superseded record.
        let old = l.state("old", l.last());
        assert_eq!(old.superseded_by.as_deref(), Some("new"));
        assert!(!old.released);
    }

    #[test]
    fn inheritance_cutoff_holds_for_the_first_record_and_several_levels() {
        // The phase registered as record 1 has nothing before it to inherit from.
        let mut l = Log::new();
        let first = l.register("a", &["ca"]);
        assert_eq!(first, 1);
        // a → b → c: each inherits what was open just before its own registration.
        supersede(&mut l, "b", "a", &["cb"]);
        l.add(
            "a",
            "condition_waived",
            json!({"condition": "ca", "reason": "after b"}),
        );
        supersede(&mut l, "c", "b", &["cc"]);
        l.add(
            "b",
            "condition_waived",
            json!({"condition": "cb", "reason": "after c"}),
        );
        let c = l.state("c", l.last());
        assert_eq!(open_ids(&c), ["ca", "cb", "cc"]);
        assert!(
            c.reasons.iter().all(|r| !r.contains("malformed")),
            "{:?}",
            c.reasons
        );
    }

    #[test]
    fn superseding_must_name_an_earlier_phase_once() {
        // Superseding a phase not registered before (itself, a later one, an unknown one).
        for (old, label) in [
            ("self", "itself"),
            ("later", "a later phase"),
            ("ghost", "unknown"),
        ] {
            let mut l = Log::new();
            supersede(&mut l, "self", old, &[]);
            l.register("later", &[]);
            let st = l.state("self", l.last());
            assert!(
                st.reasons
                    .iter()
                    .any(|r| r.contains("was not registered before")),
                "{label}: {:?}",
                st.reasons
            );
        }
        // Registered twice.
        let mut l = Log::new();
        l.register("p", &[]);
        l.register("p", &[]);
        assert!(
            l.state("p", l.last())
                .reasons
                .iter()
                .any(|r| r.contains("registered twice"))
        );
        // Superseded by two phases.
        let mut l = Log::new();
        l.register("old", &[]);
        supersede(&mut l, "n1", "old", &[]);
        supersede(&mut l, "n2", "old", &[]);
        assert!(
            l.state("old", l.last())
                .reasons
                .iter()
                .any(|r| r.contains("superseded by both"))
        );
        // The two records disagree.
        let mut l = Log::new();
        l.register("old", &[]);
        supersede(&mut l, "n1", "old", &[]);
        l.add("old", "phase_superseded", json!({"by": "n2"}));
        assert!(
            l.state("old", l.last())
                .reasons
                .iter()
                .any(|r| r.contains("superseded by both"))
        );
        // Two phase_superseded records alone, naming different phases.
        let mut l = Log::new();
        l.register("old", &[]);
        l.add("old", "phase_superseded", json!({"by": "n1"}));
        l.add("old", "phase_superseded", json!({"by": "n2"}));
        assert!(
            l.state("old", l.last())
                .reasons
                .iter()
                .any(|r| r.contains("superseded by both"))
        );
    }
}
