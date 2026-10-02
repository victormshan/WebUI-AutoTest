//! Saved login state: cookies (CDP format) and localStorage per origin.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageState {
    /// Cookies as returned by CDP `Storage.getCookies`.
    pub cookies: Vec<Value>,
    #[serde(default)]
    pub origins: Vec<OriginStorage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OriginStorage {
    pub origin: String,
    /// `[key, value]` pairs.
    #[serde(rename = "localStorage")]
    pub local_storage: Vec<(String, String)>,
}

impl StorageState {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading storage state {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Writes the state readable by the owner only: it holds live session secrets.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

/// CDP `Cookie` (from getCookies) -> `CookieParam` (for setCookies).
pub(crate) fn to_cookie_param(c: &Value) -> Value {
    const KEEP: &[&str] = &[
        "name",
        "value",
        "domain",
        "path",
        "secure",
        "httpOnly",
        "sameSite",
        "priority",
        "sourceScheme",
        "sourcePort",
    ];
    let mut out = Map::new();
    if let Some(m) = c.as_object() {
        for k in KEEP {
            if let Some(v) = m.get(*k) {
                out.insert((*k).to_string(), v.clone());
            }
        }
        // Session cookies report expires = -1; omitting it keeps them session cookies.
        let session = m.get("session").and_then(Value::as_bool).unwrap_or(false);
        if let Some(exp) = m.get("expires").and_then(Value::as_f64)
            && !session
            && exp > 0.0
        {
            out.insert("expires".into(), exp.into());
        }
    }
    Value::Object(out)
}

/// Parses the `evaluate_script` reply `{origin, items}` (JSON somewhere in the text).
pub(crate) fn parse_origin_dump(text: &str) -> Option<OriginStorage> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let v: Value = serde_json::from_str(&text[start..=end]).ok()?;
    let origin = v["origin"].as_str()?.to_string();
    if !origin.starts_with("http") {
        return None;
    }
    let local_storage = serde_json::from_value(v["items"].clone()).ok()?;
    Some(OriginStorage {
        origin,
        local_storage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cookie_param_drops_readonly_fields() {
        let c = json!({"name": "session", "value": "x", "domain": "127.0.0.1", "path": "/",
            "expires": -1, "size": 8, "httpOnly": true, "secure": false, "session": true,
            "sameSite": "Lax", "priority": "Medium", "sourceScheme": "NonSecure", "sourcePort": 8765});
        let p = to_cookie_param(&c);
        assert_eq!(p["httpOnly"], true);
        assert!(
            p.get("size").is_none() && p.get("session").is_none() && p.get("expires").is_none()
        );
        let persistent = json!({"name": "a", "value": "b", "expires": 1.9e9, "session": false});
        assert_eq!(to_cookie_param(&persistent)["expires"], 1.9e9);
    }

    #[test]
    fn parses_dump_and_skips_opaque_origins() {
        let t = "Script ran on page and returned:\n```json\n{\"origin\":\"http://x:1\",\"items\":[[\"k\",\"v\"]]}\n```";
        let o = parse_origin_dump(t).unwrap();
        assert_eq!(o.origin, "http://x:1");
        assert_eq!(o.local_storage, vec![("k".to_string(), "v".to_string())]);
        assert!(parse_origin_dump(r#"{"origin":"null","items":[]}"#).is_none());
    }
}
