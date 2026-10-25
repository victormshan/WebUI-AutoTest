//! Comparing the local milestone log with the remote one, and publishing it
//! (plans/M2-design.md §3.5, §3.6).
//!
//! The remote copy is one git note on `refs/notes/review-gate-milestones`, attached to a fixed
//! anchor blob whose id anyone can compute. The note holds the whole log, byte for byte (stored
//! as a blob and attached with `notes add -C`, so git never rewrites its whitespace). The gate
//! reads the remote anonymously through its own bare cache; the implementer publishes with an
//! ordinary, never forced, push.

use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
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

/// Which git runs: the implementer's own (publishing needs its credentials), or the gate's
/// anonymous one for its private cache.
#[derive(Clone, Copy)]
enum Git<'a> {
    Plain,
    Anon(&'a Anon),
}

/// The gate's read of the remote, made anonymous by construction (Codex review of b7a3a91, n8
/// R2 and n10 §4): git runs with a cleared environment, so nothing the service inherits
/// (EnvironmentFile, PassEnvironment, the manager's environment: credentials, proxies,
/// GIT_CONFIG_COUNT/PARAMETERS, askpass programs) reaches it; system and global config are
/// off; HOME and XDG_CONFIG_HOME are an empty directory of the gate's own (no .netrc, no git
/// config); credential helpers and askpass are blanked on the command line; only the expected
/// protocol is allowed and redirects are not followed. What is left is the cache's own local
/// config, which is checked against an exact list of keys and values before every use.
pub(crate) struct Anon {
    git: PathBuf,
    home: PathBuf,
    /// A local path or file:// remote (tests and the fault-injection build only).
    allow_local: bool,
}

/// The git the service runs (absolute: the cleared environment has no inherited PATH).
const GIT: &str = "/usr/bin/git";
/// The cache's local config may hold exactly these keys, with these values.
const LOCAL_CONFIG: &[(&str, &[&str])] = &[
    ("core.repositoryformatversion", &["0"]),
    ("core.filemode", &["true", "false"]),
    ("core.bare", &["true"]),
];

impl Anon {
    /// The production setting for `cache`: its empty home sits next to it.
    pub(crate) fn for_cache(cache: &Path) -> Self {
        Self {
            git: PathBuf::from(GIT),
            home: cache.with_file_name("anon-home"),
            allow_local: cfg!(any(test, feature = "fault-injection")),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_git(mut self, git: PathBuf) -> Self {
        self.git = git;
        self
    }

    /// The empty home: created 0700 if missing; must be a directory of ours, not writable by
    /// others, with nothing in it.
    fn ensure_home(&self) -> Result<()> {
        if !self.home.exists() {
            std::fs::create_dir_all(&self.home)?;
            std::fs::set_permissions(&self.home, std::fs::Permissions::from_mode(0o700))?;
        }
        let m = std::fs::symlink_metadata(&self.home)?;
        if !m.is_dir() {
            bail!("the anonymous home is not a directory");
        }
        owned_and_private(&m, "the anonymous home")?;
        if std::fs::read_dir(&self.home)?.next().is_some() {
            bail!("the anonymous home is not empty");
        }
        Ok(())
    }
}

/// Owned by this process's user and not writable by group or others.
fn owned_and_private(m: &std::fs::Metadata, what: &str) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if m.uid() != euid {
        bail!("{what} is not owned by the gate's user");
    }
    if m.mode() & 0o022 != 0 {
        bail!("{what} is writable by group or others");
    }
    Ok(())
}

/// The remote the gate reads: an https URL without user info, query or fragment (a local path
/// or file:// URL only where `allow_local`). Returns the protocol to allow.
fn remote_protocol(url: &str, allow_local: bool) -> Result<&'static str> {
    if let Some(rest) = url.strip_prefix("https://") {
        let authority = rest.split('/').next().unwrap_or("");
        let bad = authority.is_empty()
            || authority.contains('@')
            || url.contains(['?', '#'])
            || url.chars().any(|c| c.is_whitespace() || c.is_control());
        if bad {
            bail!("the remote must be an https URL without credentials");
        }
        return Ok("https");
    }
    if allow_local {
        return Ok("file");
    }
    bail!("the remote must be an https URL without credentials")
}

/// The cache's local config, read without includes, NUL-separated (no line splitting), must be
/// exactly what `git init --bare` writes. Nothing about a refused entry is echoed: a key can
/// itself carry a URL with credentials.
fn check_local_config(a: &Anon, cache: &Path) -> Result<()> {
    let cfg = cache.join("config");
    owned_and_private(&std::fs::symlink_metadata(cache)?, "the cache")?;
    let m = std::fs::symlink_metadata(&cfg)?;
    if !m.is_file() {
        bail!("the cache config is not a regular file");
    }
    owned_and_private(&m, "the cache config")?;
    let o = run(
        Git::Anon(a),
        cache,
        &["config", "--local", "--no-includes", "--list", "-z"],
        None,
    )?;
    if !o.ok {
        bail!("reading the cache config failed");
    }
    let mut refused = 0;
    for entry in o.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let text = std::str::from_utf8(entry).unwrap_or("");
        let (key, value) = text.split_once('\n').unwrap_or((text, ""));
        let ok = LOCAL_CONFIG
            .iter()
            .any(|(k, vals)| key == *k && vals.contains(&value));
        if !ok {
            refused += 1;
        }
    }
    if refused > 0 {
        bail!("the cache config has {refused} entries outside the allowed list: not used");
    }
    Ok(())
}

fn run(g: Git<'_>, dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Out> {
    let mut cmd = match g {
        Git::Plain => {
            let mut c = Command::new("git");
            c.args(["-c", "safe.directory=*"])
                .env("GIT_TERMINAL_PROMPT", "0");
            c
        }
        Git::Anon(a) => {
            a.ensure_home()?;
            let home = a.home.to_str().context("anonymous home path")?;
            let dir_s = dir.to_str().context("cache path")?;
            let mut c = Command::new(&a.git);
            c.env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", home)
                .env("XDG_CONFIG_HOME", home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "/bin/false")
                .env("SSH_ASKPASS", "/bin/false")
                .env("GIT_SSH_COMMAND", "/bin/false")
                .env("LC_ALL", "C")
                .args(["-c", &format!("safe.directory={dir_s}")])
                .args(["-c", "credential.helper="])
                .args(["-c", "core.askPass="])
                .args(["-c", "http.followRedirects=false"])
                .args(["-c", "protocol.allow=never"])
                .args(["-c", "protocol.https.allow=always"]);
            if a.allow_local {
                c.args(["-c", "protocol.file.allow=always"]);
            }
            c
        }
    };
    let mut child = cmd
        .arg("-C")
        .arg(dir)
        .args(args)
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

fn must(g: Git<'_>, dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<String> {
    let o = run(g, dir, args, stdin)?;
    if !o.ok {
        bail!("git {} failed: {}", args.join(" "), o.stderr);
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The anchor blob's id, writing the blob into `repo`.
pub fn anchor(repo: &Path) -> Result<String> {
    anchor_with(Git::Plain, repo)
}

fn anchor_with(g: Git<'_>, repo: &Path) -> Result<String> {
    must(
        g,
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(ANCHOR.as_bytes()),
    )
}

/// The log held in `notes_ref` of `repo`, byte for byte (None: no note there).
pub fn read_log(repo: &Path, notes_ref: &str) -> Result<Option<String>> {
    read_log_with(Git::Plain, repo, notes_ref)
}

fn read_log_with(g: Git<'_>, repo: &Path, notes_ref: &str) -> Result<Option<String>> {
    let a = anchor_with(g, repo)?;
    let list = run(g, repo, &["notes", "--ref", notes_ref, "list", &a], None)?;
    if !list.ok {
        return Ok(None);
    }
    let blob = String::from_utf8_lossy(&list.stdout).trim().to_string();
    let bytes = run(g, repo, &["cat-file", "blob", &blob], None)?;
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
    fetch_remote_with(&Anon::for_cache(cache), cache, url)
}

pub(crate) fn fetch_remote_with(a: &Anon, cache: &Path, url: &str) -> Remote {
    let go = || -> Result<Remote> {
        remote_protocol(url, a.allow_local)?;
        if !cache.join("HEAD").exists() {
            std::fs::create_dir_all(cache)?;
            std::fs::set_permissions(cache, std::fs::Permissions::from_mode(0o700))?;
            must(Git::Anon(a), cache, &["init", "--bare", "-q"], None)?;
        }
        check_local_config(a, cache)?;
        let probe = run(Git::Anon(a), cache, &["ls-remote", url, NOTES_REF], None)?;
        if !probe.ok {
            return Ok(Remote::Unreachable(probe.stderr));
        }
        if String::from_utf8_lossy(&probe.stdout).trim().is_empty() {
            return Ok(Remote::Missing);
        }
        let spec = format!("+{NOTES_REF}:{NOTES_REF}");
        let f = run(
            Git::Anon(a),
            cache,
            &["fetch", "-q", "--no-tags", url, &spec],
            None,
        )?;
        if !f.ok {
            return Ok(Remote::Unreachable(f.stderr));
        }
        // The fetch writes FETCH_HEAD and the ref, never the config: checked again all the same
        // before the log is read from the cache.
        check_local_config(a, cache)?;
        Ok(match read_log_with(Git::Anon(a), cache, NOTES_REF)? {
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
    let probe = run(Git::Plain, repo, &["ls-remote", remote, NOTES_REF], None)?;
    if !probe.ok {
        bail!("remote unreachable: {}", probe.stderr);
    }
    if !String::from_utf8_lossy(&probe.stdout).trim().is_empty() {
        // Non-forced fetch: fails when the local notes ref has diverged from the remote one.
        let spec = format!("{NOTES_REF}:{NOTES_REF}");
        must(
            Git::Plain,
            repo,
            &["fetch", "-q", "--no-tags", remote, &spec],
            None,
        )?;
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
        Git::Plain,
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(log.as_bytes()),
    )?;
    must(
        Git::Plain,
        repo,
        &["notes", "--ref", NOTES_REF, "add", "-f", "-C", &blob, &a],
        None,
    )?;
    must(Git::Plain, repo, &["push", "-q", remote, NOTES_REF], None)?;
    Ok((last.seq, last.hash.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attest::Keypair;
    use crate::milestone::record::{Draft, render_log, seal};
    use serde_json::Value;
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
        must(Git::Plain, dir, &["init", "--bare", "-q"], None).unwrap();
        dir.to_str().unwrap().to_string()
    }

    fn work(dir: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        must(Git::Plain, dir, &["init", "-q"], None).unwrap();
        for (k, v) in [("user.name", "t"), ("user.email", "t@t")] {
            must(Git::Plain, dir, &["config", k, v], None).unwrap();
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

    // ---- the gate's anonymous git (Codex review of b7a3a91, n8 R2 / n10 §4) ----

    /// A bare cache made by the gate's own fetch (the remote has no notes yet: Missing).
    fn fresh_cache(root: &Path) -> (PathBuf, PathBuf) {
        let remote = root.join("remote.git");
        must(
            Git::Plain,
            root,
            &["init", "--bare", "-q", remote.to_str().unwrap()],
            None,
        )
        .unwrap();
        let state = root.join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cache = state.join("remote.git");
        assert_eq!(
            fetch_remote(&cache, remote.to_str().unwrap()),
            Remote::Missing
        );
        (cache, remote)
    }

    fn unreachable(r: Remote) -> String {
        match r {
            Remote::Unreachable(why) => why,
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[test]
    fn the_gate_git_runs_with_exactly_the_fixed_environment() {
        let dir = tempfile::tempdir().unwrap();
        let (cache, remote) = fresh_cache(dir.path());
        // A stand-in git that records its environment and arguments, then runs the real git.
        let log = dir.path().join("calls");
        std::fs::create_dir(&log).unwrap();
        let fake = dir.path().join("fake-git");
        std::fs::write(
            &fake,
            format!(
                "#!/usr/bin/python3 -I\nimport json, os, sys\n\
                 p = os.path.join({log:?}, str(len(os.listdir({log:?}))))\n\
                 json.dump({{'env': dict(os.environ), 'argv': sys.argv[1:]}}, open(p, 'w'))\n\
                 os.execv('/usr/bin/git', ['git'] + sys.argv[1:])\n",
                log = log.to_str().unwrap()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let a = Anon::for_cache(&cache).with_git(fake);
        // A script written just now can fail to exec with ETXTBSY while another test thread
        // forks (the child briefly holds the write descriptor). Only that error is retried.
        let mut got = fetch_remote_with(&a, &cache, remote.to_str().unwrap());
        for _ in 0..5 {
            match &got {
                Remote::Unreachable(why) if why.contains("Text file busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    got = fetch_remote_with(&a, &cache, remote.to_str().unwrap());
                }
                _ => break,
            }
        }
        assert_eq!(got, Remote::Missing);
        let calls: Vec<Value> = std::fs::read_dir(&log)
            .unwrap()
            .map(|e| serde_json::from_slice(&std::fs::read(e.unwrap().path()).unwrap()).unwrap())
            .collect();
        assert!(
            calls.len() >= 2,
            "config check and ls-remote ran: {calls:?}"
        );
        let home = cache.with_file_name("anon-home");
        let home = home.to_str().unwrap();
        let want = json!({
            "PATH": "/usr/bin:/bin", "HOME": home, "XDG_CONFIG_HOME": home,
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_TERMINAL_PROMPT": "0", "GIT_ASKPASS": "/bin/false", "SSH_ASKPASS": "/bin/false",
            "GIT_SSH_COMMAND": "/bin/false", "LC_ALL": "C",
        });
        for c in &calls {
            // Exactly this set: the test process's own HOME, USER, PATH, CARGO_* … are gone.
            assert_eq!(c["env"], want, "{c}");
            let argv: Vec<&str> = c["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let cache_s = cache.to_str().unwrap();
            for flag in [
                format!("safe.directory={cache_s}"),
                "credential.helper=".to_string(),
                "core.askPass=".to_string(),
                "http.followRedirects=false".to_string(),
                "protocol.allow=never".to_string(),
                "protocol.https.allow=always".to_string(),
            ] {
                assert!(
                    argv.windows(2).any(|w| w[0] == "-c" && w[1] == flag),
                    "{flag} in {argv:?}"
                );
            }
            assert!(
                !argv.iter().any(|a| a.contains("safe.directory=*")),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn a_cache_config_outside_the_exact_list_is_refused_without_echoing_it() {
        let secret = "c2VjcmV0LXZhbHVl";
        let cases: &[(&str, String)] = &[
            (
                "http.https://github.com/.extraheader",
                format!("AUTHORIZATION: basic {secret}"),
            ),
            ("http.extraheader", format!("AUTHORIZATION: basic {secret}")),
            ("credential.helper", format!("store --file=/tmp/{secret}")),
            ("credential.https://github.com.username", secret.to_string()),
            ("include.path", format!("/tmp/{secret}")),
            ("includeIf.gitdir:/.path", format!("/tmp/{secret}")),
            ("core.sshCommand", format!("/bin/{secret}")),
            ("core.gitProxy", format!("/bin/{secret}")),
            ("core.hooksPath", format!("/tmp/{secret}")),
            ("core.fsmonitor", format!("/bin/{secret}")),
            (
                "url.https://evil.example/.insteadOf",
                "https://github.com/".to_string(),
            ),
            ("http.proxy", format!("http://{secret}@proxy:3128")),
            (
                "remote.origin.url",
                format!("https://{secret}@github.com/x"),
            ),
            ("core.bare", "false".to_string()),
            ("core.repositoryformatversion", "1".to_string()),
        ];
        for (key, value) in cases {
            let dir = tempfile::tempdir().unwrap();
            let (cache, remote) = fresh_cache(dir.path());
            must(
                Git::Plain,
                dir.path(),
                &[
                    "config",
                    "--file",
                    cache.join("config").to_str().unwrap(),
                    key,
                    value,
                ],
                None,
            )
            .unwrap();
            let why = unreachable(fetch_remote(&cache, remote.to_str().unwrap()));
            assert!(why.contains("outside the allowed list"), "{key}: {why}");
            assert!(
                !why.contains(secret) && !why.contains("evil") && !why.contains(key),
                "{key}: {why}"
            );
        }
    }

    #[test]
    fn a_config_entry_with_an_embedded_newline_is_one_refused_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (cache, remote) = fresh_cache(dir.path());
        // A value with a newline must not read as a second, allowed "core.bare\ntrue" line.
        must(
            Git::Plain,
            dir.path(),
            &[
                "config",
                "--file",
                cache.join("config").to_str().unwrap(),
                "x.y",
                "1\ncore.bare=true",
            ],
            None,
        )
        .unwrap();
        let why = unreachable(fetch_remote(&cache, remote.to_str().unwrap()));
        assert!(why.contains("1 entries outside"), "{why}");
    }

    #[test]
    fn an_anonymous_home_that_is_not_empty_or_a_writable_config_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (cache, remote) = fresh_cache(dir.path());
        let home = cache.with_file_name("anon-home");
        std::fs::write(
            home.join(".netrc"),
            "machine github.com login x password y\n",
        )
        .unwrap();
        let why = unreachable(fetch_remote(&cache, remote.to_str().unwrap()));
        assert!(why.contains("not empty"), "{why}");
        std::fs::remove_file(home.join(".netrc")).unwrap();
        assert_eq!(
            fetch_remote(&cache, remote.to_str().unwrap()),
            Remote::Missing
        );
        std::fs::set_permissions(cache.join("config"), std::fs::Permissions::from_mode(0o664))
            .unwrap();
        let why = unreachable(fetch_remote(&cache, remote.to_str().unwrap()));
        assert!(why.contains("writable by group or others"), "{why}");
    }

    #[test]
    fn only_an_https_remote_without_credentials_is_accepted_in_production() {
        for url in [
            "https://github.com/victormshan/WebUI-AutoTest.git",
            "https://github.com/x",
        ] {
            assert_eq!(remote_protocol(url, false).unwrap(), "https", "{url}");
        }
        for url in [
            "https://user:tok3n@github.com/x.git",
            "https://tok3n@github.com/x.git",
            "https:///x.git",
            "https://github.com/x.git?tok3n=1",
            "https://github.com/x.git#tok3n",
            "https://github.com/x y",
            "http://github.com/x.git",
            "ssh://git@github.com/x.git",
            "git@github.com:x.git",
            "file:///srv/x.git",
            "/srv/x.git",
        ] {
            let e = remote_protocol(url, false).unwrap_err().to_string();
            assert!(!e.contains("tok3n") && !e.contains("github"), "{url}: {e}");
        }
        // Local remotes only where allowed (tests, the fault-injection build).
        assert_eq!(remote_protocol("/srv/x.git", true).unwrap(), "file");
        // And the refusal reaches fetch_remote as Unreachable before any git runs.
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("state/remote.git");
        let a = Anon {
            git: PathBuf::from("/nonexistent/git"),
            home: dir.path().join("state/anon-home"),
            allow_local: false,
        };
        let why = unreachable(fetch_remote_with(
            &a,
            &cache,
            "https://u:tok3n@github.com/x",
        ));
        assert!(!why.contains("tok3n"), "{why}");
        assert!(!cache.exists(), "nothing was created");
    }
}
