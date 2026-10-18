//! The auditor's answer (plans/M2-design.md §4.2). Only three sections are read; everything
//! else stays prose, kept verbatim in the job record. Anything that could be read two ways is
//! `Invalid`, and an invalid answer carries nothing: no condition, no resolution.
//!
//! Grammar (one line at a time; only a trailing `\r` is removed):
//! - Headers start in column 0 and are exactly `VERDICT: <value>`, `CONDITIONS:`, `RESOLVED:`.
//!   `<value>` is `APPROVE`, `APPROVE WITH CONDITIONS`, `REWORK` or `BLOCK`, as written.
//! - There is exactly one VERDICT header. Any other line containing `VERDICT:` after removing
//!   leading `>` and whitespace (a quote, a code block, prose that mentions it) makes the answer
//!   invalid: deliberately conservative.
//! - CONDITIONS and RESOLVED appear at most once each, after VERDICT, in that order; either may
//!   be left out.
//! - A section's lines are the consecutive lines right after its header that start with `- `;
//!   it ends at the first line that does not (a blank line included) or at the next header.
//!   A header followed by no such line is an empty section.
//! - In CONDITIONS every line is `- [new][blocking] <text>` or `- [new][non-blocking] <text>`,
//!   text not blank. In RESOLVED every line is `- <condition id>: <basis>`, the id among the
//!   open conditions, basis not blank, each id once. Anything else in a section is invalid.
//! - A line starting `- [new][` (after removing leading `>` and whitespace) outside CONDITIONS is
//!   invalid: a condition that only looks like one must not be lost or half-read.
//! - APPROVE with a new blocking condition is invalid.

use std::collections::BTreeSet;

use serde::Serialize;

pub const VERDICTS: [&str; 4] = ["APPROVE", "APPROVE WITH CONDITIONS", "REWORK", "BLOCK"];

/// The output format as the auditor is asked to write it (also shown in the audit input). The
/// tests check it parses with this grammar.
pub const FORMAT: &str = "VERDICT: APPROVE | APPROVE WITH CONDITIONS | REWORK | BLOCK
CONDITIONS:
- [new][blocking] <text>
- [new][non-blocking] <text>
RESOLVED:
- <condition id>: <basis>
";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewCondition {
    pub blocking: bool,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolution {
    pub condition: String,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Parsed {
    Valid {
        verdict: String,
        conditions: Vec<NewCondition>,
        resolved: Vec<Resolution>,
    },
    Invalid {
        reason: String,
    },
}

fn invalid(reason: impl Into<String>) -> Parsed {
    Parsed::Invalid {
        reason: reason.into(),
    }
}

/// A line with leading `>` and whitespace removed: how a quote or an indent would show it.
fn unquoted(l: &str) -> &str {
    l.trim_start_matches(|c: char| c == '>' || c.is_whitespace())
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    None,
    Conditions,
    Resolved,
}

/// `open`: ids of the conditions open when the request was made.
pub fn parse(text: &str, open: &BTreeSet<String>) -> Parsed {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut verdict: Option<String> = None;
    let mut seen_conditions = false;
    let mut seen_resolved = false;
    let mut section = Section::None;
    let mut conditions = Vec::new();
    let mut resolved: Vec<Resolution> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let no = i + 1;
        if let Some(v) = l.strip_prefix("VERDICT: ") {
            if verdict.is_some() {
                return invalid(format!("line {no}: a second VERDICT"));
            }
            if !VERDICTS.contains(&v) {
                return invalid(format!(
                    "line {no}: VERDICT {v:?} is not one of {VERDICTS:?}"
                ));
            }
            verdict = Some(v.to_string());
            section = Section::None;
            continue;
        }
        if unquoted(l).contains("VERDICT:") {
            return invalid(format!("line {no}: VERDICT: outside the VERDICT header"));
        }
        if *l == "CONDITIONS:" {
            if verdict.is_none() || seen_conditions || seen_resolved {
                return invalid(format!("line {no}: CONDITIONS: out of order or repeated"));
            }
            seen_conditions = true;
            section = Section::Conditions;
            continue;
        }
        if *l == "RESOLVED:" {
            if verdict.is_none() || seen_resolved {
                return invalid(format!("line {no}: RESOLVED: out of order or repeated"));
            }
            seen_resolved = true;
            section = Section::Resolved;
            continue;
        }
        let item = l.strip_prefix("- ");
        if item.is_none() {
            section = Section::None;
        }
        match (section, item) {
            (Section::Conditions, Some(rest)) => {
                let (blocking, text) = if let Some(t) = rest.strip_prefix("[new][blocking] ") {
                    (true, t)
                } else if let Some(t) = rest.strip_prefix("[new][non-blocking] ") {
                    (false, t)
                } else {
                    return invalid(format!("line {no}: not a condition line"));
                };
                if text.trim().is_empty() {
                    return invalid(format!("line {no}: condition without text"));
                }
                conditions.push(NewCondition {
                    blocking,
                    text: text.trim().to_string(),
                });
            }
            (Section::Resolved, Some(rest)) => {
                let Some((id, basis)) = rest.split_once(": ") else {
                    return invalid(format!("line {no}: not a resolution line"));
                };
                if !open.contains(id) {
                    return invalid(format!("line {no}: {id:?} is not an open condition"));
                }
                if basis.trim().is_empty() {
                    return invalid(format!("line {no}: resolution without a basis"));
                }
                if resolved.iter().any(|r| r.condition == id) {
                    return invalid(format!("line {no}: {id} resolved twice"));
                }
                resolved.push(Resolution {
                    condition: id.to_string(),
                    basis: basis.trim().to_string(),
                });
            }
            _ => {
                if unquoted(l).starts_with("- [new][") {
                    return invalid(format!("line {no}: a condition outside CONDITIONS:"));
                }
            }
        }
    }
    let Some(verdict) = verdict else {
        return invalid("no VERDICT");
    };
    if verdict == "APPROVE" && conditions.iter().any(|c| c.blocking) {
        return invalid("APPROVE with a new blocking condition");
    }
    Parsed::Valid {
        verdict,
        conditions,
        resolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> BTreeSet<String> {
        BTreeSet::from(["C1".to_string(), "C2".to_string()])
    }

    fn reason(text: &str) -> String {
        match parse(text, &open()) {
            Parsed::Invalid { reason } => reason,
            v => panic!("valid: {v:?}\n{text}"),
        }
    }

    fn valid(text: &str) -> (String, Vec<NewCondition>, Vec<Resolution>) {
        match parse(text, &open()) {
            Parsed::Valid {
                verdict,
                conditions,
                resolved,
            } => (verdict, conditions, resolved),
            Parsed::Invalid { reason } => panic!("invalid: {reason}\n{text}"),
        }
    }

    #[test]
    fn a_full_answer() {
        let (v, c, r) = valid(
            "Some prose first.\n\nVERDICT: APPROVE WITH CONDITIONS\nCONDITIONS:\n- [new][blocking] fix A\n- [new][non-blocking]  tidy B \nRESOLVED:\n- C1: done in abc\n\nMore prose, - dashes inside are fine.\n- a later list item is prose\n",
        );
        assert_eq!(v, "APPROVE WITH CONDITIONS");
        assert_eq!(
            c,
            vec![
                NewCondition {
                    blocking: true,
                    text: "fix A".into()
                },
                NewCondition {
                    blocking: false,
                    text: "tidy B".into()
                }
            ]
        );
        assert_eq!(
            r,
            vec![Resolution {
                condition: "C1".into(),
                basis: "done in abc".into()
            }]
        );
    }

    #[test]
    fn sections_are_optional_and_may_be_empty() {
        assert_eq!(valid("VERDICT: REWORK\n").0, "REWORK");
        let (_, c, r) = valid("VERDICT: BLOCK\r\nCONDITIONS:\r\n\r\nRESOLVED:\r\n");
        assert!(c.is_empty() && r.is_empty());
        let (_, _, r) = valid("VERDICT: APPROVE\nRESOLVED:\n- C2: ok\n");
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn verdict_rules() {
        for (text, want) in [
            ("no header here\n", "no VERDICT"),
            ("VERDICT: APPROVE\nVERDICT: REWORK\n", "a second VERDICT"),
            ("VERDICT: approve\n", "is not one of"),
            ("VERDICT: APPROVE \n", "is not one of"),
            ("VERDICT:APPROVE\n", "outside the VERDICT header"),
            (" VERDICT: APPROVE\n", "outside the VERDICT header"),
            (
                "> VERDICT: APPROVE\nVERDICT: REWORK\n",
                "outside the VERDICT header",
            ),
            (
                "VERDICT: REWORK\n> VERDICT: APPROVE\n",
                "outside the VERDICT header",
            ),
            ("> VERDICT: APPROVE\n", "outside the VERDICT header"),
            (
                "VERDICT: REWORK\n```\nVERDICT: APPROVE\n```\n",
                "a second VERDICT",
            ),
            (
                "VERDICT: REWORK\n    VERDICT: APPROVE\n",
                "outside the VERDICT header",
            ),
            (
                "VERDICT: REWORK\nthe VERDICT: line says it\n",
                "outside the VERDICT header",
            ),
            (
                "VERDICT: APPROVE\nCONDITIONS:\n- [new][blocking] x\n",
                "APPROVE with a new blocking",
            ),
        ] {
            assert!(reason(text).contains(want), "{text:?}: {}", reason(text));
        }
        assert!(
            valid("VERDICT: APPROVE\nCONDITIONS:\n- [new][non-blocking] x\n")
                .1
                .len()
                == 1
        );
    }

    #[test]
    fn section_rules() {
        for (text, want) in [
            ("CONDITIONS:\nVERDICT: REWORK\n", "out of order"),
            ("VERDICT: REWORK\nRESOLVED:\nCONDITIONS:\n", "out of order"),
            ("VERDICT: REWORK\nCONDITIONS:\nCONDITIONS:\n", "repeated"),
            (
                "VERDICT: REWORK\nRESOLVED:\n- C1: a\nRESOLVED:\n",
                "repeated",
            ),
            (
                "VERDICT: REWORK\nCONDITIONS:\n- [new] x\n",
                "not a condition line",
            ),
            (
                "VERDICT: REWORK\nCONDITIONS:\n- [NEW][blocking] x\n",
                "not a condition line",
            ),
            (
                "VERDICT: REWORK\nCONDITIONS:\n- fix it\n",
                "not a condition line",
            ),
            (
                "VERDICT: REWORK\nCONDITIONS:\n- [new][blocking]  \n",
                "without text",
            ),
            (
                "VERDICT: REWORK\nRESOLVED:\n- C1 done\n",
                "not a resolution line",
            ),
            (
                "VERDICT: REWORK\nRESOLVED:\n- C9: x\n",
                "not an open condition",
            ),
            ("VERDICT: REWORK\nRESOLVED:\n- C1:  \n", "without a basis"),
            (
                "VERDICT: REWORK\nRESOLVED:\n- C1: a\n- C1: b\n",
                "resolved twice",
            ),
        ] {
            assert!(reason(text).contains(want), "{text:?}: {}", reason(text));
        }
    }

    #[test]
    fn conditions_outside_their_section_are_refused() {
        for text in [
            "- [new][blocking] before the verdict\nVERDICT: REWORK\n",
            "VERDICT: REWORK\nCONDITIONS:\n- [new][blocking] a\n\n- [new][blocking] after a blank line\n",
            "VERDICT: REWORK\n> - [new][blocking] quoted\n",
            "VERDICT: REWORK\nRESOLVED:\n- [new][blocking] in resolved\n",
            "VERDICT: REWORK\n```\n- [new][blocking] in a code block\n```\n",
        ] {
            let r = reason(text);
            assert!(
                r.contains("condition") || r.contains("resolution"),
                "{text:?}: {r}"
            );
        }
    }

    #[test]
    fn invalid_carries_no_partial_result() {
        // A resolution before the failing line must not survive.
        let p = parse("VERDICT: REWORK\nRESOLVED:\n- C1: done\n- C9: x\n", &open());
        assert!(matches!(p, Parsed::Invalid { .. }));
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["reason", "result"]
        );
    }

    #[test]
    fn the_format_shown_to_the_auditor_follows_this_grammar() {
        // Every line of FORMAT, with the placeholders filled, is read as intended.
        let filled = FORMAT
            .replace(
                "VERDICT: APPROVE | APPROVE WITH CONDITIONS | REWORK | BLOCK",
                "VERDICT: APPROVE WITH CONDITIONS",
            )
            .replace("<text>", "t")
            .replace("<condition id>: <basis>", "C1: b");
        let (v, c, r) = valid(&filled);
        assert_eq!(
            (v.as_str(), c.len(), r.len()),
            ("APPROVE WITH CONDITIONS", 2, 1)
        );
        for v in VERDICTS {
            assert!(FORMAT.contains(v));
        }
    }
}
