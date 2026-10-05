//! Per-agent read cursors: for every task, the highest message number the agent has confirmed
//! reading. Unconfirmed messages are delivered again (at-least-once); receivers de-duplicate by
//! `(task, n)`. Reading never marks anything read — only an explicit ack does (design §5 / b8).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::valid_id;

pub struct Cursors {
    dir: PathBuf,
    /// agent → task → highest acknowledged n
    map: BTreeMap<String, BTreeMap<String, u32>>,
}

impl Cursors {
    pub fn open(root: &Path, agents: impl Iterator<Item = String>) -> Result<Self> {
        let dir = root.join("cursors");
        fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        let mut map = BTreeMap::new();
        for a in agents {
            let p = dir.join(format!("{a}.json"));
            let m: BTreeMap<String, u32> = match fs::read(&p) {
                Ok(b) => serde_json::from_slice(&b)
                    .with_context(|| format!("parsing {}", p.display()))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                Err(e) => return Err(e).with_context(|| format!("reading {}", p.display())),
            };
            map.insert(a, m);
        }
        Ok(Self { dir, map })
    }

    pub fn get(&self, agent: &str, task: &str) -> u32 {
        self.map
            .get(agent)
            .and_then(|m| m.get(task))
            .copied()
            .unwrap_or(0)
    }

    /// Raises the cursor (never lowers it) and persists durably before returning.
    pub fn ack(&mut self, agent: &str, task: &str, n: u32) -> Result<bool> {
        anyhow::ensure!(valid_id(agent) && valid_id(task), "invalid id");
        let m = self.map.entry(agent.into()).or_default();
        let cur = m.get(task).copied().unwrap_or(0);
        if n <= cur {
            return Ok(false);
        }
        m.insert(task.into(), n);
        let bytes = serde_json::to_vec_pretty(m)?;
        write_atomic(&self.dir.join(format!("{agent}.json")), &bytes)?;
        Ok(true)
    }
}

/// Temp file in the same directory, fsync, rename, fsync the directory.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .context("bad file name")?
    ));
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_only_move_forward_and_persist() {
        let d = tempfile::tempdir().unwrap();
        let agents = || ["claude".to_string(), "dsh".to_string()].into_iter();
        let mut c = Cursors::open(d.path(), agents()).unwrap();
        assert_eq!(c.get("dsh", "t"), 0);
        assert!(c.ack("dsh", "t", 3).unwrap());
        assert!(!c.ack("dsh", "t", 2).unwrap(), "never lowered");
        assert!(c.ack("claude", "t", 1).unwrap());
        drop(c);
        let c = Cursors::open(d.path(), agents()).unwrap();
        assert_eq!((c.get("dsh", "t"), c.get("claude", "t")), (3, 1));
        assert!(!d.path().join("cursors/.dsh.json.tmp").exists());
    }
}
