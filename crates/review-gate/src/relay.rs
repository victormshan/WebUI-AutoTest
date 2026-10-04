//! The gate's own entries in claude-step-relay's trace.
//!
//! Written to `<relay>/traces/<exprId>.gate.md`, a file owned by the gate's system user (the
//! implementer can read it but not edit it). claude-step-relay merges it into the task's trace and
//! refuses the reserved roles from anyone else, so "external review" entries cannot be forged.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;

use crate::store::valid_id;

/// Roles only the gate may write. claude-step-relay rejects them in `step_relay_append_trace`.
pub const RESERVED_ROLE_PREFIXES: &[&str] = &["外部审核", "review-gate"];
pub const ROLE: &str = "外部审核 · review-gate";

#[derive(Debug, Clone)]
pub struct RelayTrace {
    dir: PathBuf,
}

impl RelayTrace {
    /// `dir` is claude-step-relay's data directory (the MCP server's `STEP_RELAY_DIR`).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self, expr_id: &str) -> Option<PathBuf> {
        valid_id(expr_id).then(|| self.dir.join("traces").join(format!("{expr_id}.gate.md")))
    }

    /// Appends one entry in claude-step-relay's trace format.
    pub fn append(&self, expr_id: &str, text: &str) -> Result<()> {
        let path = self
            .path(expr_id)
            .with_context(|| format!("invalid exprId {expr_id:?}"))?;
        fs::create_dir_all(path.parent().expect("has parent"))?;
        let new = !path.exists();
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        if new {
            write!(f, "# review-gate 记录\n\nexprId: {expr_id}\n\n---\n\n")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
            }
        }
        write!(
            f,
            "## [{}] [{ROLE}]\n\n{}\n\n",
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            text.trim_end()
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_entries_in_relay_format_to_the_gate_file() {
        let d = tempfile::tempdir().unwrap();
        let r = RelayTrace::new(d.path());
        r.append("2026-10-04_01-00-00", "VERDICT: APPROVED")
            .unwrap();
        r.append("2026-10-04_01-00-00", "第二条").unwrap();
        let text = fs::read_to_string(d.path().join("traces/2026-10-04_01-00-00.gate.md")).unwrap();
        assert!(text.starts_with("# review-gate 记录"));
        assert_eq!(text.matches("[外部审核 · review-gate]").count(), 2);
        assert!(text.contains("VERDICT: APPROVED") && text.contains("第二条"));
        assert!(r.append("../evil", "x").is_err());
        assert!(RESERVED_ROLE_PREFIXES.iter().any(|p| ROLE.starts_with(p)));
    }
}
