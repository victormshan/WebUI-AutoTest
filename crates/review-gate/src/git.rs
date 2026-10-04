//! Thin wrapper around the `git` binary (same approach as the Node gate it replaces).

use std::path::Path;
use std::process::Command;

use crate::GateError;

pub fn git(repo: &Path, args: &[&str]) -> Result<String, GateError> {
    // The gate runs as its own system user and reads repositories owned by the implementer:
    // without this git refuses them as "dubious ownership". Writes happen only client-side.
    let out = Command::new("git")
        .args(["-c", "safe.directory=*"])
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| GateError::Git(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        return Err(GateError::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolves a revision to a full object id.
pub fn rev_parse(repo: &Path, rev: &str) -> Result<String, GateError> {
    git(repo, &["rev-parse", "--verify", "--quiet", rev])
}

/// Parent commit ids of `commit` (empty for a root commit, two or more for a merge).
pub fn parents(repo: &Path, commit: &str) -> Result<Vec<String>, GateError> {
    let line = git(repo, &["rev-list", "--parents", "-n", "1", commit])?;
    Ok(line
        .split_whitespace()
        .skip(1)
        .map(str::to_string)
        .collect())
}

/// Stages every change in the working tree and returns (tree of the index, HEAD).
pub fn stage_all(repo: &Path) -> Result<(String, String), GateError> {
    git(repo, &["add", "-A"])?;
    Ok((git(repo, &["write-tree"])?, rev_parse(repo, "HEAD")?))
}
