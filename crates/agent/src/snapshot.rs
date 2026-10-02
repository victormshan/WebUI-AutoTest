//! Helpers for chrome-devtools-mcp accessibility snapshots.
//!
//! A snapshot line looks like:
//! `  uid=2_6 button "加入购物车" disableable disabled`

use serde::{Deserialize, Serialize};

/// Stable, uid-free description of an element: what a later replay can
/// search for in a fresh snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locator {
    pub role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// 0-based index among elements with the same role+name (e.g. the 2nd "加入购物车").
    #[serde(default, skip_serializing_if = "is_zero")]
    pub nth: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl std::fmt::Display for Locator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {:?}", self.role, self.name)?;
        if self.nth > 0 {
            write!(f, " #{}", self.nth + 1)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Node {
    depth: usize,
    uid: String,
    role: String,
    name: String,
    /// Everything after the name (attributes).
    rest: String,
}

/// Escapes newlines inside quoted strings so every node is exactly one line.
fn normalize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let (mut in_str, mut esc) = (false, false);
    for c in raw.chars() {
        if in_str {
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                '\n' => {
                    out.push_str("\\n");
                    continue;
                }
                _ => {}
            }
        } else if c == '"' {
            in_str = true;
        }
        out.push(c);
    }
    out
}

fn parse_line(line: &str) -> Option<Node> {
    let trimmed = line.trim_start();
    let depth = (line.len() - trimmed.len()) / 2;
    let rest = trimmed.strip_prefix("uid=")?;
    let (uid, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    let (role, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    let (name, rest) = match rest.strip_prefix('"') {
        Some(after) => {
            // Find the closing quote, honouring escapes.
            let mut esc = false;
            let mut end = None;
            for (i, c) in after.char_indices() {
                match c {
                    _ if esc => esc = false,
                    '\\' => esc = true,
                    '"' => {
                        end = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            let end = end.unwrap_or(after.len());
            (
                &after[..end],
                after.get(end + 1..).unwrap_or("").trim_start(),
            )
        }
        None => ("", rest),
    };
    Some(Node {
        depth,
        uid: uid.to_string(),
        role: role.to_string(),
        name: name.to_string(),
        rest: rest.to_string(),
    })
}

fn parse(raw: &str) -> Vec<Node> {
    normalize(raw).lines().filter_map(parse_line).collect()
}

/// Parses and normalizes: drops line breaks and merges runs of sibling
/// `StaticText` nodes (some sites emit one node per character).
fn merged(raw: &str) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::new();
    for n in parse(raw) {
        if n.role == "LineBreak" {
            continue;
        }
        if n.role == "StaticText"
            && let Some(prev) = out.last_mut()
            && prev.role == "StaticText"
            && prev.depth == n.depth
        {
            prev.name.push_str(&n.name);
            continue;
        }
        out.push(n);
    }
    out
}

/// Shrinks a snapshot for the LLM (see [`merged`]), capped at `max_chars`.
pub fn compact(raw: &str, max_chars: usize) -> String {
    let out = merged(raw);
    let mut s = String::new();
    for n in &out {
        if n.role == "StaticText" && n.name.trim().is_empty() {
            continue;
        }
        let line = format!(
            "{}uid={} {}{}{}\n",
            "  ".repeat(n.depth),
            n.uid,
            n.role,
            if n.name.is_empty() {
                String::new()
            } else {
                format!(" {:?}", n.name)
            },
            if n.rest.is_empty() {
                String::new()
            } else {
                format!(" {}", n.rest)
            },
        );
        if s.len() + line.len() > max_chars {
            s.push_str("... (snapshot truncated)\n");
            break;
        }
        s.push_str(&line);
    }
    s
}

/// Builds a uid-free locator for `uid` from the snapshot it came from.
pub fn locator_for(raw: &str, uid: &str) -> Option<Locator> {
    let nodes = parse(raw);
    let target = nodes.iter().find(|n| n.uid == uid)?;
    let nth = nodes
        .iter()
        .take_while(|n| n.uid != uid)
        .filter(|n| n.role == target.role && n.name == target.name)
        .count();
    Some(Locator {
        role: target.role.clone(),
        name: target.name.clone(),
        nth,
    })
}

/// Resolves a locator back to a uid in a (newer) snapshot.
pub fn resolve(raw: &str, loc: &Locator) -> Option<String> {
    parse(raw)
        .into_iter()
        .filter(|n| n.role == loc.role && n.name == loc.name)
        .nth(loc.nth)
        .map(|n| n.uid)
}

/// The raw snapshot line for `uid` (used for safety checks and logging).
pub fn describe(raw: &str, uid: &str) -> Option<String> {
    parse(raw).into_iter().find(|n| n.uid == uid).map(|n| {
        format!("{} {:?} {}", n.role, n.name, n.rest)
            .trim_end()
            .to_string()
    })
}

/// True if an element with exactly this role and name exists.
pub fn has_element(raw: &str, role: &str, name: &str) -> bool {
    merged(raw).iter().any(|n| n.role == role && n.name == name)
}

/// True if any element's name contains `text`.
pub fn contains_text(raw: &str, text: &str) -> bool {
    merged(raw).iter().any(|n| n.name.contains(text))
}

/// `(role, name)` of headings and alerts with text: stable "landmarks" of a page state.
pub fn landmarks(raw: &str) -> Vec<(String, String)> {
    merged(raw)
        .into_iter()
        .filter(|n| matches!(n.role.as_str(), "heading" | "alert") && !n.name.trim().is_empty())
        .map(|n| (n.role, n.name))
        .collect()
}

/// Roles that make up a page's "shape" for state identity.
const STRUCTURAL_ROLES: &[&str] = &[
    "heading",
    "button",
    "link",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "menuitem",
    "tab",
    "switch",
    "slider",
    "spinbutton",
];

/// Identity of a page state: URL path plus the set of headings and controls.
/// Dynamic text (counts, totals, messages) is ignored so that e.g. a cart with
/// one or two items is the same state.
pub fn fingerprint(raw: &str) -> String {
    use std::hash::{Hash, Hasher};
    let nodes = merged(raw);
    let url = nodes
        .iter()
        .find(|n| n.role == "RootWebArea")
        .and_then(|n| n.rest.split("url=\"").nth(1))
        .and_then(|u| u.split('"').next())
        .unwrap_or("")
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_string();
    let mut parts: Vec<String> = nodes
        .iter()
        .filter(|n| STRUCTURAL_ROLES.contains(&n.role.as_str()))
        .map(|n| format!("{}:{}", n.role, n.name))
        .collect();
    parts.sort();
    parts.dedup();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    parts.hash(&mut h);
    format!("{:016x}", h.finish())[..8].to_string()
}

/// Short human label for a page state: its headings, most specific last.
pub fn label(raw: &str) -> String {
    let heads: Vec<String> = merged(raw)
        .into_iter()
        .filter(|n| n.role == "heading" && !n.name.trim().is_empty())
        .map(|n| n.name)
        .collect();
    let mut s = heads.join(" · ");
    if s.chars().count() > 60 {
        s = s.chars().take(57).collect::<String>() + "...";
    }
    if s.is_empty() { "(untitled)".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAP: &str = r#"## Latest page snapshot
uid=1_0 RootWebArea "Shop" url="http://x/"
  uid=1_3 form
    uid=1_4 StaticText "用户名 "
    uid=1_5 textbox "用户名"
    uid=1_6 LineBreak "
"
  uid=2_6 button "加入购物车"
  uid=2_8 button "加入购物车"
  uid=2_9 StaticText "a"
  uid=2_10 StaticText "b"
  uid=2_16 button "去结算" disableable disabled
"#;

    #[test]
    fn compacts_text_and_linebreaks() {
        let c = compact(SNAP, 10_000);
        assert!(!c.contains("LineBreak"));
        assert!(c.contains(r#"uid=2_9 StaticText "ab""#));
        assert!(!c.contains("2_10"));
        assert!(c.contains(r#"uid=2_16 button "去结算" disableable disabled"#));
    }

    #[test]
    fn locator_roundtrip_with_duplicates() {
        let loc = locator_for(SNAP, "2_8").unwrap();
        assert_eq!(
            loc,
            Locator {
                role: "button".into(),
                name: "加入购物车".into(),
                nth: 1
            }
        );
        let newer = SNAP.replace("2_6", "3_1").replace("2_8", "3_2");
        assert_eq!(resolve(&newer, &loc).as_deref(), Some("3_2"));
    }

    #[test]
    fn queries() {
        assert!(has_element(SNAP, "button", "去结算"));
        assert!(!has_element(SNAP, "button", "去"));
        assert!(contains_text(SNAP, "ab"));
        assert!(contains_text(SNAP, "去结"));
        assert!(!contains_text(SNAP, "zzz"));
    }

    #[test]
    fn fingerprint_ignores_dynamic_text() {
        let a = SNAP.replace(r#"StaticText "a""#, r#"StaticText "共 1 件""#);
        let b = SNAP.replace(r#"StaticText "a""#, r#"StaticText "共 2 件""#);
        assert_eq!(fingerprint(&a), fingerprint(&b));
        let c = SNAP.replace(r#"button "去结算""#, r#"button "提交订单""#);
        assert_ne!(fingerprint(SNAP), fingerprint(&c));
        let d = SNAP.replace(r#"url="http://x/""#, r#"url="http://x/?q=1""#);
        assert_eq!(fingerprint(SNAP), fingerprint(&d));
    }

    #[test]
    fn truncates() {
        assert!(compact(SNAP, 60).ends_with("(snapshot truncated)\n"));
    }
}
