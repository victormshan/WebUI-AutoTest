//! File-backed store. The directory is created with mode 0700: when the gate runs as its own
//! system user, nobody else (including the implementer) can read or forge its state.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

use crate::GateError;
use crate::model::{ReviewRecord, Task};

/// Task ids become file names: letters, digits, `.`, `_`, `-`; no leading dot; max 64.
pub fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && id.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        create_private_dir(&root)?;
        create_private_dir(&root.join("tasks"))?;
        create_private_dir(&root.join("reviews"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn task_path(&self, id: &str) -> Result<PathBuf, GateError> {
        if !valid_id(id) {
            return Err(GateError::InvalidId(id.to_string()));
        }
        Ok(self.root.join("tasks").join(format!("{id}.json")))
    }

    fn reviews_dir(&self, task: &str) -> Result<PathBuf, GateError> {
        if !valid_id(task) {
            return Err(GateError::InvalidId(task.to_string()));
        }
        Ok(self.root.join("reviews").join(task))
    }

    pub fn task_exists(&self, id: &str) -> Result<bool, GateError> {
        Ok(self.task_path(id)?.exists())
    }

    pub fn load_task(&self, id: &str) -> Result<Task, GateError> {
        let p = self.task_path(id)?;
        if !p.exists() {
            return Err(GateError::NoSuchTask(id.to_string()));
        }
        read_json(&p).map_err(GateError::Io)
    }

    pub fn save_task(&self, task: &Task) -> Result<(), GateError> {
        write_json(&self.task_path(&task.id)?, task).map_err(GateError::Io)
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.root.join("tasks"))? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "json") {
                out.push(read_json(&p)?);
            }
        }
        out.sort_by_key(|t: &Task| std::cmp::Reverse(t.updated_at));
        Ok(out)
    }

    /// Number of review records already stored for `iteration` (to number the next attempt).
    pub fn review_count(&self, task: &str, iteration: u32) -> Result<u32, GateError> {
        let dir = self.reviews_dir(task)?;
        if !dir.exists() {
            return Ok(0);
        }
        let prefix = format!("v{iteration}-");
        let n = fs::read_dir(&dir)
            .map_err(|e| GateError::Io(e.into()))?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .count();
        Ok(n as u32)
    }

    pub fn save_review(&self, r: &ReviewRecord) -> Result<(), GateError> {
        let dir = self.reviews_dir(&r.task)?;
        create_private_dir(&dir).map_err(GateError::Io)?;
        let p = dir.join(format!("{}.json", r.id));
        if p.exists() {
            return Err(GateError::Conflict(format!(
                "review {} already exists",
                r.id
            )));
        }
        write_json(&p, r).map_err(GateError::Io)
    }

    pub fn load_review(&self, task: &str, review_id: &str) -> Result<ReviewRecord, GateError> {
        if !valid_id(review_id) {
            return Err(GateError::InvalidId(review_id.to_string()));
        }
        let p = self.reviews_dir(task)?.join(format!("{review_id}.json"));
        if !p.exists() {
            return Err(GateError::NoSuchReview(review_id.to_string()));
        }
        read_json(&p).map_err(GateError::Io)
    }

    /// Reviewer answers keyed by prompt hash, so an interrupted multi-chunk review can resume.
    pub fn cache_get<T: DeserializeOwned>(
        &self,
        task: &str,
        key: &str,
    ) -> Result<Option<T>, GateError> {
        if !valid_id(key) {
            return Err(GateError::InvalidId(key.to_string()));
        }
        let p = self
            .root
            .join("cache")
            .join(task)
            .join(format!("{key}.json"));
        if !valid_id(task) {
            return Err(GateError::InvalidId(task.to_string()));
        }
        if !p.exists() {
            return Ok(None);
        }
        read_json(&p).map(Some).map_err(GateError::Io)
    }

    pub fn cache_put<T: Serialize>(&self, task: &str, key: &str, v: &T) -> Result<(), GateError> {
        if !valid_id(task) || !valid_id(key) {
            return Err(GateError::InvalidId(format!("{task}/{key}")));
        }
        let dir = self.root.join("cache").join(task);
        create_private_dir(&dir).map_err(GateError::Io)?;
        write_json(&dir.join(format!("{key}.json")), v).map_err(GateError::Io)
    }
}

fn create_private_dir(p: &Path) -> Result<()> {
    fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(p: &Path) -> Result<T> {
    let text = fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))
}

/// Atomic write (temp file + rename), owner-only permissions.
fn write_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    let tmp = p.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(v)? + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, p).with_context(|| format!("writing {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        for ok in ["a", "task-1", "v1-2", "A.b_c"] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["", ".hidden", "../x", "a/b", "a b", &"x".repeat(65)] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("state")).unwrap();
        for sub in ["", "tasks", "reviews"] {
            let mode = fs::metadata(s.root().join(sub))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{sub}");
        }
    }
}
