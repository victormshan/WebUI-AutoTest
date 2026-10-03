//! Cross-review by an external AI (a model from another vendor) during exploration.
//!
//! Mirrors the dsh-web-relay three-party protocol: Claude (implementer) proposes tasks and
//! writes flows; the external reviewer can veto proposed tasks, add missing ones, drop
//! assertions it considers unstable or meaningless, and add assertions it considers missing.
//! Reviewer output is never trusted blindly: added assertions must hold on the recorded
//! final page, at least one assertion always remains, and a changed flow is re-verified by
//! LLM-free replay (callers revert on failure).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::flow::Assertion;
use crate::snapshot;

/// Reviewer prompts are kept small: web-gemini relays through a browser text box.
pub const REVIEW_SNAPSHOT_CHARS: usize = 12_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub name: String,
    pub goal: String,
    #[serde(default = "positive")]
    pub kind: String,
    /// "claude" (implementer) or "external" (added by the reviewer).
    #[serde(default = "claude")]
    pub source: String,
}

fn positive() -> String {
    "positive".into()
}
fn claude() -> String {
    "claude".into()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Dropped {
    pub item: String,
    pub reason: String,
}

/// What the reviewer did to one state's task proposals.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProposalReview {
    pub state: String,
    pub claude: Vec<String>,
    pub dropped: Vec<Dropped>,
    pub added: Vec<String>,
    /// Tasks not run because the task budget ran out (after reserving room for additions).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_for_budget: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Cuts the reviewed task list to `budget` without letting the reviewer's additions
/// (appended last) be silently cut first: one slot is reserved for the first addition
/// when the budget allows two or more tasks. Returns (kept, skipped names).
pub fn fit_to_budget(tasks: Vec<Proposal>, budget: usize) -> (Vec<Proposal>, Vec<String>) {
    if tasks.len() <= budget {
        return (tasks, Vec::new());
    }
    let (external, implementer): (Vec<_>, Vec<_>) =
        tasks.into_iter().partition(|t| t.source == "external");
    let reserve = usize::from(budget >= 2 && !external.is_empty());
    let mut kept: Vec<Proposal> = Vec::new();
    let mut skipped = Vec::new();
    for (i, t) in implementer.into_iter().enumerate() {
        if i < budget - reserve {
            kept.push(t)
        } else {
            skipped.push(t.name)
        }
    }
    for t in external {
        if kept.len() < budget {
            kept.push(t)
        } else {
            skipped.push(t.name)
        }
    }
    (kept, skipped)
}

/// What the reviewer did to one flow's assertions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlowReview {
    pub dropped: Vec<Dropped>,
    pub added: Vec<String>,
    /// Suggested assertions that did not hold on the recorded page (not applied).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected_additions: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// Changes were rolled back because the reviewed flow failed re-verification.
    #[serde(default)]
    pub reverted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn proposal_review_prompt(
    raw_snapshot: &str,
    context: &str,
    deny: &[String],
    proposals: &[Proposal],
    known: &[String],
    max_additions: usize,
) -> (String, String) {
    let system = format!(
        r#"You are the EXTERNAL REVIEWER in a three-party QA protocol, independent of the implementer (another vendor's AI) that proposed the tasks below for an automated web test suite.
Review each PROPOSED TASK and drop it only for a concrete reason: unsafe/destructive or irreversible (deleting data, payments, messaging real people, changing credentials), needs information not in CONTEXT, duplicates a KNOWN task, cannot start from or be verified on the CURRENT PAGE, or touches elements containing these off-limits keywords: {deny}.
Then add up to {max_additions} IMPORTANT tasks the implementer missed (main business paths or one negative/validation case per form), following the same safety rules. Goals must be concrete, include the visible expected outcome, and be written in the page's language.
Reply with ONLY JSON:
{{"reviews": [{{"name": "<task name>", "verdict": "keep"|"drop", "reason": "<short>"}}], "additions": [{{"name": "<short_snake_case_ascii>", "goal": "<instruction>", "kind": "positive"|"negative"}}]}}"#,
        deny = if deny.is_empty() {
            "(none)".to_string()
        } else {
            deny.join(", ")
        },
    );
    let list = proposals
        .iter()
        .map(|p| format!("- {} [{}]: {}", p.name, p.kind, p.goal))
        .collect::<Vec<_>>()
        .join("\n");
    let known = if known.is_empty() {
        "(none)".to_string()
    } else {
        known
            .iter()
            .map(|g| format!("- {g}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let user = format!(
        "CONTEXT:\n{}\n\nKNOWN TASKS:\n{known}\n\nPROPOSED TASKS:\n{list}\n\nCURRENT PAGE:\n{}\nYour JSON reply:",
        if context.is_empty() {
            "(none)"
        } else {
            context
        },
        snapshot::compact(raw_snapshot, REVIEW_SNAPSHOT_CHARS)
    );
    (system, user)
}

/// Applies the reviewer's verdicts: returns (tasks to run, dropped, added names).
/// Unknown names in reviews are ignored; drops without a reason are ignored.
pub fn apply_proposal_review(
    proposals: Vec<Proposal>,
    reply: &Value,
    max_additions: usize,
) -> (Vec<Proposal>, Vec<Dropped>, Vec<String>) {
    let mut dropped = Vec::new();
    let mut keep = Vec::new();
    let reviews = reply["reviews"].as_array().cloned().unwrap_or_default();
    for p in proposals {
        let verdict = reviews
            .iter()
            .find(|r| r["name"].as_str() == Some(p.name.as_str()));
        let reason = verdict
            .and_then(|r| r["reason"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if verdict.and_then(|r| r["verdict"].as_str()) == Some("drop") && !reason.is_empty() {
            dropped.push(Dropped {
                item: p.name.clone(),
                reason,
            });
        } else {
            keep.push(p);
        }
    }
    let mut added = Vec::new();
    for a in reply["additions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(max_additions)
    {
        let Ok(mut p) = serde_json::from_value::<Proposal>(a) else {
            continue;
        };
        if p.goal.trim().is_empty() || keep.iter().any(|k| k.name == p.name || k.goal == p.goal) {
            continue;
        }
        p.source = "external".into();
        added.push(p.name.clone());
        keep.push(p);
    }
    (keep, dropped, added)
}

pub fn flow_review_prompt(
    goal: &str,
    raw_final_snapshot: &str,
    assertions: &[Assertion],
) -> (String, String) {
    let system = r#"You are the EXTERNAL REVIEWER in a three-party QA protocol, independent of the implementer (another vendor's AI) that wrote the regression ASSERTIONS below for a recorded web test.
Check each assertion: drop it only if it is unstable across runs (exact wording of generated/AI/personalized content, randomized greetings, timestamps, order ids, counters) or does not prove the GOAL. Then add up to 3 assertions that are missing to prove the GOAL, copied exactly from the FINAL PAGE.
Assertion forms: {"element": {"role": "<role>", "name": "<exact name>"}} | {"text": "<substring of some element's text>"} | {"absent_text": "<text that must not appear>"}
Reply with ONLY JSON:
{"reviews": [{"index": <number>, "verdict": "keep"|"drop", "reason": "<short>"}], "additions": [<assertion>], "summary": "<one sentence>"}"#
        .to_string();
    let list = assertions
        .iter()
        .enumerate()
        .map(|(i, a)| format!("{i}. {a}"))
        .collect::<Vec<_>>()
        .join("\n");
    let user = format!(
        "GOAL:\n{goal}\n\nASSERTIONS:\n{list}\n\nFINAL PAGE:\n{}\nYour JSON reply:",
        snapshot::compact(raw_final_snapshot, REVIEW_SNAPSHOT_CHARS)
    );
    (system, user)
}

/// Applies the reviewer's assertion verdicts against the recorded final page.
pub fn apply_flow_review(
    assertions: &[Assertion],
    reply: &Value,
    raw_final_snapshot: &str,
) -> (Vec<Assertion>, FlowReview) {
    let mut review = FlowReview {
        summary: reply["summary"].as_str().unwrap_or("").to_string(),
        ..Default::default()
    };
    let drops: Vec<(usize, String)> = reply["reviews"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["verdict"].as_str() == Some("drop"))
        .filter_map(|r| {
            Some((
                r["index"].as_u64()? as usize,
                r["reason"].as_str().unwrap_or("").to_string(),
            ))
        })
        .filter(|(i, reason)| *i < assertions.len() && !reason.trim().is_empty())
        .collect();
    let mut out: Vec<Assertion> = assertions
        .iter()
        .enumerate()
        .filter(|(i, _)| !drops.iter().any(|(d, _)| d == i))
        .map(|(_, a)| a.clone())
        .collect();
    for a in reply["additions"].as_array().into_iter().flatten().take(3) {
        let Ok(a) = serde_json::from_value::<Assertion>(a.clone()) else {
            continue;
        };
        if out.contains(&a) {
            continue;
        }
        if a.check(raw_final_snapshot) {
            review.added.push(a.to_string());
            out.push(a);
        } else {
            review.rejected_additions.push(a.to_string());
        }
    }
    if out.is_empty() {
        review.summary = format!(
            "{} (reviewer would remove every assertion; kept the originals)",
            review.summary
        );
        return (assertions.to_vec(), review);
    }
    review.dropped = drops
        .into_iter()
        .map(|(i, reason)| Dropped {
            item: assertions[i].to_string(),
            reason,
        })
        .collect();
    (out, review)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(name: &str) -> Proposal {
        Proposal {
            name: name.into(),
            goal: format!("goal {name}"),
            kind: "positive".into(),
            source: "claude".into(),
        }
    }

    #[test]
    fn proposals_drop_needs_reason_and_additions_are_tagged_external() {
        let reply = json!({
            "reviews": [
                {"name": "login", "verdict": "keep"},
                {"name": "delete_account", "verdict": "drop", "reason": "irreversible"},
                {"name": "search", "verdict": "drop", "reason": ""},
                {"name": "ghost", "verdict": "drop", "reason": "unknown task"}
            ],
            "additions": [
                {"name": "wrong_password", "goal": "用错误密码登录，确认提示错误", "kind": "negative"},
                {"name": "dup", "goal": "goal login"},
                {"name": "x1", "goal": "a"}, {"name": "x2", "goal": "b"}
            ]
        });
        let (keep, dropped, added) = apply_proposal_review(
            vec![p("login"), p("delete_account"), p("search")],
            &reply,
            2,
        );
        assert_eq!(
            dropped,
            vec![Dropped {
                item: "delete_account".into(),
                reason: "irreversible".into()
            }]
        );
        assert_eq!(
            keep.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(),
            ["login", "search", "wrong_password"]
        );
        assert_eq!(
            added,
            ["wrong_password"],
            "duplicate goal skipped, cap of 2 counts attempts"
        );
        assert_eq!(keep[2].source, "external");
        assert_eq!(keep[2].kind, "negative");
    }

    #[test]
    fn budget_reserves_a_slot_for_reviewer_additions() {
        let ext = |n: &str| Proposal {
            source: "external".into(),
            ..p(n)
        };
        let (kept, skipped) = fit_to_budget(vec![p("a"), p("b"), p("c"), ext("x"), ext("y")], 3);
        assert_eq!(
            kept.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(),
            ["a", "b", "x"]
        );
        assert_eq!(skipped, ["c", "y"]);
        // budget of 1: the implementer's first task wins; within budget nothing changes
        assert_eq!(fit_to_budget(vec![p("a"), ext("x")], 1).0[0].name, "a");
        assert_eq!(fit_to_budget(vec![p("a"), ext("x")], 5).1.len(), 0);
    }

    #[test]
    fn unusable_reply_keeps_everything() {
        let (keep, dropped, added) = apply_proposal_review(vec![p("a")], &json!({"oops": 1}), 3);
        assert_eq!((keep.len(), dropped.len(), added.len()), (1, 0, 0));
    }

    const SNAP: &str = "uid=1_0 RootWebArea \"Shop\"\n  uid=1_1 heading \"订单已提交\" level=\"2\"\n  uid=1_2 StaticText \"共 2 件，合计 ¥528\"\n";

    #[test]
    fn flow_review_drops_with_reason_and_validates_additions_on_page() {
        let a = vec![
            Assertion::Text("Where should we start?".into()),
            Assertion::Element {
                role: "heading".into(),
                name: "订单已提交".into(),
            },
        ];
        let reply = json!({
            "reviews": [{"index": 0, "verdict": "drop", "reason": "randomized greeting"}, {"index": 1, "verdict": "keep"}],
            "additions": [{"text": "合计 ¥528"}, {"text": "不存在的文字"}, {"element": {"role": "heading", "name": "订单已提交"}}],
            "summary": "ok"
        });
        let (out, r) = apply_flow_review(&a, &reply, SNAP);
        assert_eq!(out, vec![a[1].clone(), Assertion::Text("合计 ¥528".into())]);
        assert_eq!(r.dropped.len(), 1);
        assert_eq!(r.added, vec!["text \"合计 ¥528\""]);
        assert_eq!(r.rejected_additions, vec!["text \"不存在的文字\""]);
    }

    #[test]
    fn flow_review_never_removes_every_assertion() {
        let a = vec![Assertion::Text("x".into())];
        let reply = json!({"reviews": [{"index": 0, "verdict": "drop", "reason": "weak"}]});
        let (out, r) = apply_flow_review(&a, &reply, SNAP);
        assert_eq!(out, a);
        assert!(r.dropped.is_empty());
        assert!(r.summary.contains("kept the originals"));
    }

    #[test]
    fn prompts_carry_the_material_to_review() {
        let (sys, user) = proposal_review_prompt(
            SNAP,
            "alice/secret",
            &["删除".into()],
            &[p("login")],
            &[],
            2,
        );
        assert!(
            sys.contains("EXTERNAL REVIEWER") && sys.contains("删除") && sys.contains("up to 2")
        );
        assert!(
            user.contains("- login [positive]")
                && user.contains("订单已提交")
                && user.contains("alice/secret")
        );
        let (_, user) = flow_review_prompt("下单", SNAP, &[Assertion::Text("合计".into())]);
        assert!(user.contains("0. text \"合计\"") && user.contains("GOAL:\n下单"));
    }
}
