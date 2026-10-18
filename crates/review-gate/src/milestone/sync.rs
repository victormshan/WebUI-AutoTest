//! Comparing the local milestone log with the remote one, and publishing it
//! (plans/M2-design.md §3.5, §3.6).
//!
//! The remote copy is one git note on `refs/notes/review-gate-milestones`, attached to a fixed
//! anchor blob whose id anyone can compute. The note holds the whole log, byte for byte (stored
//! as a blob and attached with `notes add -C`, so git never rewrites its whitespace). The gate
//! reads the remote anonymously through its own bare cache; the implementer publishes with an
//! ordinary, never forced, push.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use ed25519_dalek::VerifyingKey;

use super::ledger::Checkpoint;
use super::record::{Record, parse_log, verify_chain};

pub const NOTES_REF: &str = "refs/notes/review-gate-milestones";
/// Content of the anchor blob the note hangs on.
pub const ANCHOR: &str = "review-gate-milestones anchor v1\n";

/// How the remote log relates to the local one.
#[derive(Debug, Clone, PartialEq)]
pub enum Sync {
    /// Same records.
    Equal,
    /// The remote is a prefix of the local log: the local tail is not archived yet.
    RemoteBehind { unarchived: usize },
    /// The local log is a prefix of the remote one: these verified records are to be appended
    /// locally before anything is released.
    RemoteAhead { extra: Vec<Record> },
    /// Release stops for every phase: divergence, a remote that fails verification or is out of
    /// reach, or a remote that does not hold the checkpoint.
    Stop(String),
}

/// What could be read from the remote.
#[derive(Debug, Clone, PartialEq)]
pub enum Remote {
    /// The remote answered and has no milestone log yet.
    Missing,
    /// The log text, byte for byte.
    Log(String),
    /// Out of reach (network, authentication, a missing repository): never "empty".
    Unreachable(String),
}

/// Classifies the remote against the local log (§3.6). `local` is the verified local chain.
pub fn classify(
    local: &[Record],
    remote: &Remote,
    checkpoint: Option<&Checkpoint>,
    key: &VerifyingKey,
    repo: &str,
) -> Sync {
    let records = match remote {
        Remote::Unreachable(why) => return Sync::Stop(format!("remote unreachable: {why}")),
        Remote::Missing => Vec::new(),
        Remote::Log(text) => match parse_log(text) {
            Ok(r) => r,
            Err(e) => return Sync::Stop(format!("remote log unreadable: {e}")),
        },
    };
    if let Err(e) = verify_chain(&records, key, repo) {
        return Sync::Stop(format!("remote log fails verification: {e}"));
    }
    // The remote must hold the checkpoint; it may grow beyond it (§3.6).
    if let Some(cp) = checkpoint
        && cp.seq > 0
    {
        match records.get(cp.seq as usize - 1) {
            None => {
                return Sync::Stop(format!(
                    "remote has {} records, fewer than the checkpoint {}: it was truncated",
                    records.len(),
                    cp.seq
                ));
            }
            Some(r) if r.hash != cp.hash => {
                return Sync::Stop(format!(
                    "remote record {} differs from the checkpoint: the remote was rewritten",
                    cp.seq
                ));
            }
            Some(_) => {}
        }
    }
    let common = local
        .iter()
        .zip(&records)
        .take_while(|(a, b)| a == b)
        .count();
    if common < local.len() && common < records.len() {
        return Sync::Stop(format!("local and remote diverge after record {common}"));
    }
    match records.len().cmp(&local.len()) {
        std::cmp::Ordering::Equal => Sync::Equal,
        std::cmp::Ordering::Less => Sync::RemoteBehind {
            unarchived: local.len() - records.len(),
        },
        std::cmp::Ordering::Greater => Sync::RemoteAhead {
            extra: records[local.len()..].to_vec(),
        },
    }
}

struct Out {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

fn run(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Out> {
    let mut child = Command::new("git")
        .args(["-c", "safe.directory=*"])
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git")?;
    if let Some(input) = stdin {
        child.stdin.take().expect("piped stdin").write_all(input)?;
    }
    let o = child.wait_with_output()?;
    Ok(Out {
        ok: o.status.success(),
        stdout: o.stdout,
        stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
    })
}

fn must(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<String> {
    let o = run(dir, args, stdin)?;
    if !o.ok {
        bail!("git {} failed: {}", args.join(" "), o.stderr);
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The anchor blob's id, writing the blob into `repo`.
pub fn anchor(repo: &Path) -> Result<String> {
    must(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(ANCHOR.as_bytes()),
    )
}

/// The log held in `notes_ref` of `repo`, byte for byte (None: no note there).
pub fn read_log(repo: &Path, notes_ref: &str) -> Result<Option<String>> {
    let a = anchor(repo)?;
    let list = run(repo, &["notes", "--ref", notes_ref, "list", &a], None)?;
    if !list.ok {
        return Ok(None);
    }
    let blob = String::from_utf8_lossy(&list.stdout).trim().to_string();
    let bytes = run(repo, &["cat-file", "blob", &blob], None)?;
    if !bytes.ok {
        bail!("reading note blob {blob}: {}", bytes.stderr);
    }
    Ok(Some(
        String::from_utf8(bytes.stdout).context("note is not UTF-8")?,
    ))
}

/// The gate's read of the remote: anonymous fetch of the notes ref into its own bare `cache`
/// (created on first use) from the fixed `url`, never the implementer's `origin`.
pub fn fetch_remote(cache: &Path, url: &str) -> Remote {
    let go = || -> Result<Remote> {
        if !cache.join("HEAD").exists() {
            std::fs::create_dir_all(cache)?;
            must(cache, &["init", "--bare", "-q"], None)?;
        }
        let probe = run(cache, &["ls-remote", url, NOTES_REF], None)?;
        if !probe.ok {
            return Ok(Remote::Unreachable(probe.stderr));
        }
        if String::from_utf8_lossy(&probe.stdout).trim().is_empty() {
            return Ok(Remote::Missing);
        }
        let spec = format!("+{NOTES_REF}:{NOTES_REF}");
        let f = run(cache, &["fetch", "-q", "--no-tags", url, &spec], None)?;
        if !f.ok {
            return Ok(Remote::Unreachable(f.stderr));
        }
        Ok(match read_log(cache, NOTES_REF)? {
            Some(t) => Remote::Log(t),
            None => Remote::Unreachable("notes ref has no note on the anchor".into()),
        })
    };
    go().unwrap_or_else(|e| Remote::Unreachable(e.to_string()))
}

/// The implementer's publish (§3.5): bring the remote notes ref in (fast-forward only), refuse
/// unless the remote log is a prefix of `log`, attach `log` and push without force. Returns the
/// position published.
pub fn publish(
    repo: &Path,
    remote: &str,
    log: &str,
    key: &VerifyingKey,
    repo_id: &str,
) -> Result<(u64, String)> {
    let records = parse_log(log)?;
    verify_chain(&records, key, repo_id)?;
    let last = records.last().context("nothing to publish")?;
    let probe = run(repo, &["ls-remote", remote, NOTES_REF], None)?;
    if !probe.ok {
        bail!("remote unreachable: {}", probe.stderr);
    }
    if !String::from_utf8_lossy(&probe.stdout).trim().is_empty() {
        // Non-forced fetch: fails when the local notes ref has diverged from the remote one.
        let spec = format!("{NOTES_REF}:{NOTES_REF}");
        must(repo, &["fetch", "-q", "--no-tags", remote, &spec], None)?;
        let theirs = read_log(repo, NOTES_REF)?.unwrap_or_default();
        let theirs = parse_log(&theirs)?;
        let prefix = theirs.len() <= records.len() && records[..theirs.len()] == theirs[..];
        if !prefix {
            bail!(
                "the remote log ({} records) is not a prefix of this log ({} records): refusing to overwrite it",
                theirs.len(),
                records.len()
            );
        }
    }
    let a = anchor(repo)?;
    let blob = must(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(log.as_bytes()),
    )?;
    must(
        repo,
        &["notes", "--ref", NOTES_REF, "add", "-f", "-C", &blob, &a],
        None,
    )?;
    must(repo, &["push", "-q", remote, NOTES_REF], None)?;
    Ok((last.seq, last.hash.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attest::Keypair;
    use crate::milestone::record::{Draft, render_log, seal};
    use serde_json::json;

    fn kp() -> Keypair {
        Keypair::from_seed([5; 32])
    }

    fn chain(n: usize) -> Vec<Record> {
        let k = kp();
        let mut v: Vec<Record> = Vec::new();
        for i in 0..n {
            let d = Draft {
                kind: "note".into(),
                phase: "p".into(),
                body: json!({"i": i}),
            };
            v.push(seal(&k, v.last(), "repo", "t", d).unwrap());
        }
        v
    }

    fn cls(local: &[Record], remote: Remote, cp: Option<&Checkpoint>) -> Sync {
        classify(local, &remote, cp, &kp().verifying_key(), "repo")
    }

    fn log(r: &[Record]) -> Remote {
        Remote::Log(render_log(r).unwrap())
    }

    fn cp(r: &Record) -> Checkpoint {
        Checkpoint {
            seq: r.seq,
            hash: r.hash.clone(),
        }
    }

    #[test]
    fn equal_behind_and_ahead() {
        let c = chain(4);
        assert_eq!(cls(&c, log(&c), None), Sync::Equal);
        assert_eq!(
            cls(&c, log(&c[..2]), None),
            Sync::RemoteBehind { unarchived: 2 }
        );
        assert_eq!(
            cls(&c, Remote::Missing, None),
            Sync::RemoteBehind { unarchived: 4 }
        );
        assert_eq!(
            cls(&c[..1], log(&c), Some(&cp(&c[0]))),
            Sync::RemoteAhead {
                extra: c[1..].to_vec()
            }
        );
    }

    #[test]
    fn a_remote_growing_past_the_checkpoint_is_normal() {
        let c = chain(5);
        assert_eq!(
            cls(&c[..3], log(&c), Some(&cp(&c[2]))),
            Sync::RemoteAhead {
                extra: c[3..].to_vec()
            }
        );
    }

    #[test]
    fn stops_on_truncation_rewrite_divergence_tampering_and_no_reach() {
        let c = chain(4);
        let is_stop = |s: Sync, needle: &str| match s {
            Sync::Stop(w) => assert!(w.contains(needle), "{w}"),
            other => panic!("expected stop ({needle}), got {other:?}"),
        };
        // Truncated below the checkpoint, including to nothing.
        is_stop(cls(&c, log(&c[..2]), Some(&cp(&c[3]))), "truncated");
        is_stop(cls(&c, Remote::Missing, Some(&cp(&c[1]))), "truncated");
        // Rewritten at the checkpoint (another chain of the same length).
        let k = kp();
        let mut other = c[..1].to_vec();
        for i in 0..3 {
            let d = Draft {
                kind: "note".into(),
                phase: "p".into(),
                body: json!({"other": i}),
            };
            other.push(seal(&k, other.last(), "repo", "t", d).unwrap());
        }
        is_stop(cls(&c, log(&other), Some(&cp(&c[2]))), "rewritten");
        // Diverged with no checkpoint to tell.
        is_stop(cls(&c, log(&other), None), "diverge");
        // Tampered, cut off, or out of reach.
        let tampered = render_log(&c).unwrap().replace("\"i\":1", "\"i\":9");
        is_stop(cls(&c, Remote::Log(tampered), None), "verification");
        let cut = render_log(&c).unwrap();
        is_stop(
            cls(&c, Remote::Log(cut[..cut.len() - 3].into()), None),
            "unreadable",
        );
        is_stop(
            cls(&c, Remote::Unreachable("dns".into()), None),
            "unreachable",
        );
    }

    fn bare(dir: &Path) -> String {
        std::fs::create_dir_all(dir).unwrap();
        must(dir, &["init", "--bare", "-q"], None).unwrap();
        dir.to_str().unwrap().to_string()
    }

    fn work(dir: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        must(dir, &["init", "-q"], None).unwrap();
        for (k, v) in [("user.name", "t"), ("user.email", "t@t")] {
            must(dir, &["config", k, v], None).unwrap();
        }
        dir.to_path_buf()
    }

    #[test]
    fn publish_and_fetch_round_trip_byte_for_byte() {
        let d = tempfile::tempdir().unwrap();
        let url = bare(&d.path().join("remote.git"));
        let w = work(&d.path().join("work"));
        let key = kp().verifying_key();
        let cache = d.path().join("cache.git");
        assert_eq!(fetch_remote(&cache, &url), Remote::Missing);

        let c = chain(3);
        let text = render_log(&c[..2]).unwrap();
        assert_eq!(
            publish(&w, &url, &text, &key, "repo").unwrap(),
            (2, c[1].hash.clone())
        );
        assert_eq!(fetch_remote(&cache, &url), Remote::Log(text.clone()));

        // A longer log extends it (an ordinary push).
        let longer = render_log(&c).unwrap();
        publish(&w, &url, &longer, &key, "repo").unwrap();
        assert_eq!(fetch_remote(&cache, &url), Remote::Log(longer));
    }

    #[test]
    fn publish_refuses_to_overwrite_a_newer_or_different_remote() {
        let d = tempfile::tempdir().unwrap();
        let url = bare(&d.path().join("remote.git"));
        let key = kp().verifying_key();
        let c = chain(3);
        let w1 = work(&d.path().join("w1"));
        publish(&w1, &url, &render_log(&c).unwrap(), &key, "repo").unwrap();
        // Another clone that only has an older export.
        let w2 = work(&d.path().join("w2"));
        let err = publish(&w2, &url, &render_log(&c[..2]).unwrap(), &key, "repo")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a prefix"), "{err}");
        let cache = d.path().join("cache.git");
        assert_eq!(
            fetch_remote(&cache, &url),
            Remote::Log(render_log(&c).unwrap())
        );
    }

    #[test]
    fn an_unreachable_remote_is_never_read_as_empty() {
        let d = tempfile::tempdir().unwrap();
        let cache = d.path().join("cache.git");
        let gone = d.path().join("no-such-repo.git");
        assert!(matches!(
            fetch_remote(&cache, gone.to_str().unwrap()),
            Remote::Unreachable(_)
        ));
    }
}
