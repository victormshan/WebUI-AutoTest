//! Accessibility tree → snapshot text in the same syntax (C1) and with the same node
//! selection as chrome-devtools-mcp, so the agent's snapshot functions work unchanged.
//!
//! This is a port of what chrome-devtools-mcp 1.10.1 does (`DEFAULT_PACKAGE`), in two parts:
//! - which nodes are kept and with which properties: Puppeteer's `Accessibility.snapshot`
//!   with `interestingOnly` (`AXNode.isInteresting`, `isLeafNode`, `serialize`);
//! - how a node is printed: chrome-devtools-mcp's `SnapshotFormatter`.
//!
//! The functions here are pure; fetching the trees (main frame and iframes) is the
//! backend's job.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

/// One node of `Accessibility.getFullAXTree`, with the flags Puppeteer derives from it.
#[derive(Debug)]
pub struct AxNode {
    payload: Value,
    children: Vec<usize>,
    /// Snapshot of the document inside this iframe, if one could be taken.
    pub iframe: Option<SerNode>,
    role: String,
    name: String,
    description: Option<String>,
    ignored: bool,
    focusable: bool,
    hidden: bool,
    busy: bool,
    live: Option<String>,
    modal: bool,
    has_errormessage: bool,
    has_details: bool,
    roledescription: bool,
    editable: bool,
    richly_editable: bool,
}

/// A flat AX tree; index 0 is the root (the first node CDP returns).
#[derive(Debug)]
pub struct AxTree {
    pub nodes: Vec<AxNode>,
}

/// A kept node after filtering, with Puppeteer's serialized properties.
#[derive(Debug, Clone, PartialEq)]
pub struct SerNode {
    pub role: String,
    /// Serialized properties (`name`, `value`, `disabled`, `checked`, `level`, …).
    pub props: BTreeMap<String, Value>,
    pub backend_node_id: Option<i64>,
    pub children: Vec<SerNode>,
}

/// JavaScript truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

impl AxTree {
    /// Builds the tree from `getFullAXTree`'s `nodes` (Puppeteer `AXNode.createTree`).
    pub fn new(nodes: &[Value]) -> Option<Self> {
        if nodes.is_empty() {
            return None;
        }
        let index: HashMap<String, usize> = nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| Some((id_str(&n["nodeId"])?, i)))
            .collect();
        let nodes = nodes
            .iter()
            .map(|payload| {
                let children = payload["childIds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| index.get(&id_str(c)?).copied())
                    .collect();
                AxNode::new(payload.clone(), children)
            })
            .collect();
        Some(Self { nodes })
    }

    /// Iframe nodes that have a DOM node, as (tree index, backendDOMNodeId).
    pub fn iframes(&self) -> Vec<(usize, i64)> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.role == "Iframe")
            .filter_map(|(i, n)| Some((i, n.payload["backendDOMNodeId"].as_i64()?)))
            .collect()
    }

    /// The interesting-only snapshot (Puppeteer `snapshot({interestingOnly: true})`).
    pub fn snapshot(&self) -> Option<SerNode> {
        let mut memo = vec![None; self.nodes.len()];
        let mut keep = vec![false; self.nodes.len()];
        self.collect(0, false, &mut keep, &mut memo);
        self.serialize_tree(0, &keep).into_iter().next()
    }

    fn has_focusable_child(&self, i: usize, memo: &mut Vec<Option<bool>>) -> bool {
        if let Some(v) = memo[i] {
            return v;
        }
        let mut v = false;
        for &c in &self.nodes[i].children {
            if self.nodes[c].focusable || self.has_focusable_child(c, memo) {
                v = true;
                break;
            }
        }
        memo[i] = Some(v);
        v
    }

    fn is_leaf(&self, i: usize, memo: &mut Vec<Option<bool>>) -> bool {
        let n = &self.nodes[i];
        if n.children.is_empty() {
            return true;
        }
        if n.is_plain_text_field() || n.is_text_only() {
            return true;
        }
        if matches!(
            n.role.as_str(),
            "doc-cover"
                | "graphics-symbol"
                | "img"
                | "image"
                | "Meter"
                | "scrollbar"
                | "slider"
                | "separator"
                | "progressbar"
        ) {
            return true;
        }
        if self.has_focusable_child(i, memo) {
            return false;
        }
        n.role == "heading" && !n.name.is_empty()
    }

    fn is_interesting(&self, i: usize, inside_control: bool, memo: &mut Vec<Option<bool>>) -> bool {
        let n = &self.nodes[i];
        if n.role == "Ignored" || n.hidden || n.ignored {
            return false;
        }
        if n.is_landmark() {
            return true;
        }
        if n.focusable
            || n.richly_editable
            || n.busy
            || n.live
                .as_deref()
                .is_some_and(|l| !l.is_empty() && l != "off")
            || n.modal
            || n.has_errormessage
            || n.has_details
            || n.roledescription
        {
            return true;
        }
        if n.is_control() {
            return true;
        }
        if inside_control {
            return false;
        }
        self.is_leaf(i, memo)
            && (!n.name.is_empty() || n.description.as_deref().is_some_and(|d| !d.is_empty()))
    }

    fn collect(
        &self,
        i: usize,
        inside_control: bool,
        keep: &mut Vec<bool>,
        memo: &mut Vec<Option<bool>>,
    ) {
        if self.is_interesting(i, inside_control, memo) || self.nodes[i].iframe.is_some() {
            keep[i] = true;
        }
        if self.is_leaf(i, memo) {
            return;
        }
        let inside = inside_control || self.nodes[i].is_control();
        for c in self.nodes[i].children.clone() {
            self.collect(c, inside, keep, memo);
        }
    }

    fn serialize_tree(&self, i: usize, keep: &[bool]) -> Vec<SerNode> {
        let mut children = Vec::new();
        for &c in &self.nodes[i].children {
            children.extend(self.serialize_tree(c, keep));
        }
        if !keep[i] {
            return children;
        }
        let mut s = self.nodes[i].serialize();
        s.children = children;
        if let Some(inner) = &self.nodes[i].iframe {
            s.children.push(inner.clone());
        }
        vec![s]
    }
}

fn id_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl AxNode {
    fn new(payload: Value, children: Vec<usize>) -> Self {
        let role = payload["role"]["value"]
            .as_str()
            .unwrap_or("Unknown")
            .to_string();
        let name = match &payload["name"]["value"] {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        let description = payload["description"]["value"].as_str().map(String::from);
        let ignored = truthy(&payload["ignored"]);
        let mut n = Self {
            payload,
            children,
            iframe: None,
            role,
            name,
            description,
            ignored,
            focusable: false,
            hidden: false,
            busy: false,
            live: None,
            modal: false,
            has_errormessage: false,
            has_details: false,
            roledescription: false,
            editable: false,
            richly_editable: false,
        };
        for p in n.payload["properties"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let v = &p["value"]["value"];
            match p["name"].as_str().unwrap_or_default() {
                "editable" => {
                    n.richly_editable = v.as_str() == Some("richtext");
                    n.editable = true;
                }
                "focusable" => n.focusable = truthy(v),
                "hidden" => n.hidden = truthy(v),
                "busy" => n.busy = truthy(v),
                "live" => n.live = v.as_str().map(String::from),
                "modal" => n.modal = truthy(v),
                "roledescription" => n.roledescription = truthy(v),
                "errormessage" => n.has_errormessage = true,
                "details" => n.has_details = true,
                _ => {}
            }
        }
        n
    }

    fn is_plain_text_field(&self) -> bool {
        if self.richly_editable {
            return false;
        }
        self.editable || self.role == "textbox" || self.role == "searchbox"
    }

    fn is_text_only(&self) -> bool {
        matches!(
            self.role.as_str(),
            "LineBreak" | "text" | "InlineTextBox" | "StaticText"
        )
    }

    fn is_control(&self) -> bool {
        matches!(
            self.role.as_str(),
            "button"
                | "checkbox"
                | "ColorWell"
                | "combobox"
                | "DisclosureTriangle"
                | "listbox"
                | "menu"
                | "menubar"
                | "menuitem"
                | "menuitemcheckbox"
                | "menuitemradio"
                | "radio"
                | "scrollbar"
                | "searchbox"
                | "slider"
                | "spinbutton"
                | "switch"
                | "tab"
                | "textbox"
                | "tree"
                | "treeitem"
        )
    }

    fn is_landmark(&self) -> bool {
        matches!(
            self.role.as_str(),
            "banner"
                | "complementary"
                | "contentinfo"
                | "form"
                | "main"
                | "navigation"
                | "region"
                | "search"
        )
    }

    /// Puppeteer `AXNode.serialize`.
    fn serialize(&self) -> SerNode {
        let mut props: HashMap<String, Value> = HashMap::new();
        for p in self.payload["properties"].as_array().into_iter().flatten() {
            if let Some(name) = p["name"].as_str() {
                props.insert(name.to_lowercase(), p["value"]["value"].clone());
            }
        }
        for key in ["name", "value", "description"] {
            if self.payload.get(key).is_some_and(|v| !v.is_null()) {
                props.insert(key.into(), self.payload[key]["value"].clone());
            }
        }
        let mut out = BTreeMap::new();
        for k in [
            "name",
            "value",
            "description",
            "keyshortcuts",
            "roledescription",
            "valuetext",
            "url",
        ] {
            if let Some(v) = props.get(k) {
                out.insert(k.to_string(), v.clone());
            }
        }
        for k in [
            "disabled",
            "expanded",
            "focused",
            "modal",
            "multiline",
            "multiselectable",
            "readonly",
            "required",
            "selected",
            "busy",
            "atomic",
        ] {
            if k == "focused" && self.role == "RootWebArea" {
                continue;
            }
            if let Some(v) = props.get(k) {
                out.insert(k.to_string(), Value::Bool(truthy(v)));
            }
        }
        for k in ["checked", "pressed"] {
            if let Some(v) = props.get(k) {
                let t = match v.as_str() {
                    Some("mixed") => Value::String("mixed".into()),
                    Some("true") => Value::Bool(true),
                    _ => Value::Bool(false),
                };
                out.insert(k.to_string(), t);
            }
        }
        for k in ["level", "valuemax", "valuemin"] {
            if let Some(v) = props.get(k) {
                out.insert(k.to_string(), v.clone());
            }
        }
        for k in [
            "autocomplete",
            "haspopup",
            "invalid",
            "orientation",
            "live",
            "relevant",
            "errormessage",
            "details",
        ] {
            if let Some(v) = props.get(k)
                && truthy(v)
                && v.as_str() != Some("false")
            {
                out.insert(k.to_string(), v.clone());
            }
        }
        SerNode {
            role: self.role.clone(),
            props: out,
            backend_node_id: self.payload["backendDOMNodeId"].as_i64(),
            children: Vec::new(),
        }
    }
}

/// chrome-devtools-mcp's boolean attribute → capability word printed before it.
fn capability(attr: &str) -> Option<&'static str> {
    match attr {
        "disabled" => Some("disableable"),
        "expanded" => Some("expandable"),
        "focused" => Some("focusable"),
        "selected" => Some("selectable"),
        _ => None,
    }
}

/// JavaScript `String(number)` for a JSON number as chrome-devtools-mcp sees it: parsed
/// into a JavaScript Number (an f64, so integers beyond 2^53 round), then printed with
/// ECMAScript Number::toString.
pub fn js_number(n: &serde_json::Number) -> String {
    // serde_json keeps integers exact; JavaScript does not. `as_f64` rounds the way
    // JSON.parse does (to the nearest f64).
    match n.as_f64() {
        Some(f) => js_f64(f),
        None => n.to_string(),
    }
}

/// ECMAScript Number::toString for a finite f64: the shortest round-trip digits, written
/// plainly for exponents -7 < e < 21 and in exponent form (`1e+21`, `1.5e-7`) otherwise.
fn js_f64(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f == 0.0 {
        return "0".into(); // also -0
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    let sign = if f < 0.0 { "-" } else { "" };
    // Rust's `{:e}` gives the shortest digits that round-trip: "1.5e-7", "1e21".
    let sci = format!("{:e}", f.abs());
    let (mantissa, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i64;
    let n = exp.parse::<i64>().expect("integer exponent") + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let e = if e >= 0 {
            format!("+{e}")
        } else {
            e.to_string()
        };
        if k == 1 {
            format!("{digits}e{e}")
        } else {
            format!("{}.{}e{e}", &digits[..1], &digits[1..])
        }
    };
    format!("{sign}{body}")
}

/// One printed line's attributes (chrome-devtools-mcp `SnapshotFormatter.#getAttributes`).
fn attributes(uid: &str, node: &SerNode) -> Vec<String> {
    let mut a = vec![format!("uid={uid}")];
    a.push(if node.role == "none" {
        "ignored".into()
    } else {
        node.role.clone()
    });
    if let Some(name) = node.props.get("name")
        && truthy(name)
    {
        let name = match name {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        a.push(format!("\"{name}\""));
    }
    for (attr, val) in &node.props {
        if attr == "name" {
            continue;
        }
        if let Value::Bool(_) = val
            && let Some(cap) = capability(attr)
        {
            a.push(cap.to_string());
        }
        match val {
            Value::Bool(true) => a.push(attr.clone()),
            Value::String(s) => a.push(format!("{attr}=\"{s}\"")),
            Value::Number(n) => a.push(format!("{attr}=\"{}\"", js_number(n))),
            _ => {}
        }
    }
    a
}

/// Renders the snapshot with uids `<gen>_<n>` in pre-order, as chrome-devtools-mcp
/// numbers a fresh snapshot. Returns the text and each uid's backendDOMNodeId.
pub fn render(root: &SerNode, generation: u64) -> (String, Vec<(String, Option<i64>)>) {
    let mut text = String::new();
    let mut uids = Vec::new();
    fn walk(
        n: &SerNode,
        depth: usize,
        generation: u64,
        text: &mut String,
        uids: &mut Vec<(String, Option<i64>)>,
    ) {
        let uid = format!("{generation}_{}", uids.len());
        // chrome-devtools-mcp gives options their text as value.
        let mut node = n.clone();
        if node.role == "option"
            && let Some(name) = node.props.get("name").filter(|v| truthy(v)).cloned()
        {
            let s = match name {
                Value::String(s) => s,
                other => other.to_string(),
            };
            node.props.insert("value".into(), Value::String(s));
        }
        text.push_str(&" ".repeat(depth * 2));
        text.push_str(&attributes(&uid, &node).join(" "));
        text.push('\n');
        uids.push((uid, n.backend_node_id));
        for c in &n.children {
            walk(c, depth + 1, generation, text, uids);
        }
    }
    walk(root, 0, generation, &mut text, &mut uids);
    (text, uids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn prop(name: &str, value: Value) -> Value {
        json!({ "name": name, "value": { "type": "x", "value": value } })
    }

    fn node(
        id: u32,
        role: &str,
        name: &str,
        children: &[u32],
        props: Vec<Value>,
        backend: Option<i64>,
    ) -> Value {
        let mut n = json!({
            "nodeId": id.to_string(),
            "ignored": false,
            "role": { "type": "role", "value": role },
            "childIds": children.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            "properties": props,
        });
        if !name.is_empty() {
            n["name"] = json!({ "type": "computedString", "value": name });
        }
        if let Some(b) = backend {
            n["backendDOMNodeId"] = json!(b);
        }
        n
    }

    fn text_of(nodes: Vec<Value>) -> String {
        let tree = AxTree::new(&nodes).unwrap();
        render(&tree.snapshot().unwrap(), 1).0
    }

    #[test]
    fn prints_like_chrome_devtools_mcp() {
        let nodes = vec![
            node(
                1,
                "RootWebArea",
                "Shop",
                &[2, 3, 4, 5, 6, 9],
                vec![
                    prop("focusable", json!(true)),
                    prop("url", json!("http://x/")),
                ],
                Some(1),
            ),
            node(
                2,
                "button",
                "Buy",
                &[],
                vec![
                    prop("focusable", json!(true)),
                    prop("disabled", json!(true)),
                ],
                Some(2),
            ),
            node(
                3,
                "checkbox",
                "Agree",
                &[],
                vec![
                    prop("focusable", json!(true)),
                    prop("checked", json!("mixed")),
                ],
                Some(3),
            ),
            node(
                4,
                "heading",
                "Title",
                &[],
                vec![prop("level", json!(2))],
                Some(4),
            ),
            node(
                5,
                "button",
                "Open",
                &[],
                vec![
                    prop("focusable", json!(true)),
                    prop("expanded", json!(false)),
                ],
                Some(5),
            ),
            node(
                6,
                "combobox",
                "Size",
                &[7, 8],
                vec![
                    prop("focusable", json!(true)),
                    prop("expanded", json!(false)),
                ],
                Some(6),
            ),
            node(
                7,
                "option",
                "Small",
                &[],
                vec![
                    prop("focusable", json!(true)),
                    prop("selected", json!(false)),
                ],
                Some(7),
            ),
            node(
                8,
                "option",
                "Large",
                &[],
                vec![
                    prop("focusable", json!(true)),
                    prop("selected", json!(true)),
                ],
                Some(8),
            ),
            node(9, "generic", "", &[10], vec![], Some(9)),
            node(10, "StaticText", "Hello", &[], vec![], Some(10)),
        ];
        assert_eq!(
            text_of(nodes),
            "uid=1_0 RootWebArea \"Shop\" url=\"http://x/\"\n\
             \x20 uid=1_1 button \"Buy\" disableable disabled\n\
             \x20 uid=1_2 checkbox \"Agree\" checked=\"mixed\"\n\
             \x20 uid=1_3 heading \"Title\" level=\"2\"\n\
             \x20 uid=1_4 button \"Open\" expandable\n\
             \x20 uid=1_5 combobox \"Size\" expandable\n\
             \x20   uid=1_6 option \"Small\" selectable value=\"Small\"\n\
             \x20   uid=1_7 option \"Large\" selectable selected value=\"Large\"\n\
             \x20 uid=1_8 StaticText \"Hello\"\n"
        );
    }

    #[test]
    fn hidden_and_ignored_nodes_are_dropped_but_their_children_kept() {
        let mut ignored = node(3, "generic", "", &[4], vec![], Some(3));
        ignored["ignored"] = json!(true);
        let nodes = vec![
            node(
                1,
                "RootWebArea",
                "R",
                &[2, 3],
                vec![prop("focusable", json!(true))],
                Some(1),
            ),
            node(
                2,
                "button",
                "Gone",
                &[],
                vec![prop("hidden", json!(true))],
                Some(2),
            ),
            ignored,
            node(
                4,
                "button",
                "Kept",
                &[],
                vec![prop("focusable", json!(true))],
                Some(4),
            ),
        ];
        let t = text_of(nodes);
        assert!(!t.contains("Gone"), "{t}");
        assert!(t.contains("uid=1_1 button \"Kept\""), "{t}");
    }

    #[test]
    fn text_inside_a_control_is_not_repeated() {
        let nodes = vec![
            node(
                1,
                "RootWebArea",
                "R",
                &[2],
                vec![prop("focusable", json!(true))],
                Some(1),
            ),
            node(
                2,
                "button",
                "Inner text",
                &[3],
                vec![prop("focusable", json!(true))],
                Some(2),
            ),
            node(3, "StaticText", "Inner text", &[], vec![], Some(3)),
        ];
        assert_eq!(
            text_of(nodes),
            "uid=1_0 RootWebArea \"R\"\n  uid=1_1 button \"Inner text\"\n"
        );
    }

    #[test]
    fn uids_follow_pre_order_and_carry_the_dom_node() {
        let nodes = vec![
            node(
                1,
                "RootWebArea",
                "R",
                &[2, 3],
                vec![prop("focusable", json!(true))],
                Some(10),
            ),
            node(
                2,
                "button",
                "A",
                &[],
                vec![prop("focusable", json!(true))],
                Some(20),
            ),
            node(
                3,
                "button",
                "B",
                &[],
                vec![prop("focusable", json!(true))],
                None,
            ),
        ];
        let tree = AxTree::new(&nodes).unwrap();
        let (_, uids) = render(&tree.snapshot().unwrap(), 7);
        assert_eq!(
            uids,
            vec![
                ("7_0".into(), Some(10)),
                ("7_1".into(), Some(20)),
                ("7_2".into(), None)
            ]
        );
    }

    #[test]
    fn numbers_print_as_javascript_does() {
        for (f, js) in [
            (-0.0, "0"),
            (0.0, "0"),
            (4.0, "4"),
            (0.5, "0.5"),
            (-2.5, "-2.5"),
            (123.456, "123.456"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e-6, "0.000001"),
            (1.5e-6, "0.0000015"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (2.5e21, "2.5e+21"),
            (-1e21, "-1e+21"),
            (1e300, "1e+300"),
            (123456789012345680000.0, "123456789012345680000"),
        ] {
            assert_eq!(js_f64(f), js, "{f:e}");
        }
        // Numbers as CDP sends them in JSON.
        let n = |v: serde_json::Value| js_number(v.as_number().unwrap());
        assert_eq!(n(json!(4)), "4");
        assert_eq!(n(json!(-3)), "-3");
        // Beyond 2^53 a JavaScript Number rounds: String(Number(9007199254740993)).
        assert_eq!(n(json!(9007199254740993u64)), "9007199254740992");
        assert_eq!(n(json!(-9007199254740993i64)), "-9007199254740992");
        assert_eq!(n(json!(u64::MAX)), "18446744073709552000");
        assert_eq!(n(json!(0.0000001)), "1e-7");
        assert_eq!(n(json!(1e21)), "1e+21");
    }

    #[test]
    fn an_empty_tree_has_no_snapshot() {
        assert!(AxTree::new(&[]).is_none());
    }
}
