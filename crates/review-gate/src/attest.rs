//! Signed approval attestations.
//!
//! When a version is approved the gate signs a canonical statement (task, version, commit, tree,
//! parent, review, reviewer) with an ed25519 key that lives only in its private state directory.
//! The implementer attaches the text to the commit as a git note (`refs/notes/review-gate`) but
//! cannot forge one. CI verifies every commit with the public key pinned in the workflow file,
//! which the implementer has no permission to change — tamper-evident, not tamper-proof.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use crate::git;

pub const NOTES_REF: &str = "refs/notes/review-gate";
const HEADER: &str = "review-gate-attestation: v1";

pub struct Keypair {
    signing: SigningKey,
}

impl Keypair {
    /// Loads `<state>/keys/gate.ed25519` (32-byte seed, 0600), creating it on first use.
    pub fn load_or_create(state: &Path) -> Result<Self> {
        let dir = state.join("keys");
        let path = dir.join("gate.ed25519");
        if path.exists() {
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let seed: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .context("gate key must be 32 bytes")?;
            return Ok(Self {
                signing: SigningKey::from_bytes(&seed),
            });
        }
        fs::create_dir_all(&dir)?;
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
        write_private(&path, &seed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            signing: SigningKey::from_bytes(&seed),
        })
    }

    pub fn public_b64(&self) -> String {
        B64.encode(self.signing.verifying_key().to_bytes())
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Ok(fs::write(path, bytes)?)
    }
}

pub fn parse_public_key(b64: &str) -> Result<VerifyingKey> {
    let bytes = B64.decode(b64.trim()).context("public key is not base64")?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .context("public key must be 32 bytes")?;
    Ok(VerifyingKey::from_bytes(&arr)?)
}

/// What the gate vouches for.
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub task: String,
    pub version: String,
    pub commit: String,
    pub tree: String,
    pub parent: String,
    pub review: String,
    pub reviewer: String,
    pub verdict: String,
    pub at: String,
}

const FIELDS: [&str; 9] = [
    "task", "version", "commit", "tree", "parent", "review", "reviewer", "verdict", "at",
];

impl Statement {
    fn values(&self) -> [&str; 9] {
        [
            &self.task,
            &self.version,
            &self.commit,
            &self.tree,
            &self.parent,
            &self.review,
            &self.reviewer,
            &self.verdict,
            &self.at,
        ]
    }

    /// The exact bytes that are signed.
    pub fn canonical(&self) -> String {
        let mut s = format!("{HEADER}\n");
        for (k, v) in FIELDS.iter().zip(self.values()) {
            s.push_str(&format!("{k}: {}\n", v.replace('\n', " ")));
        }
        s
    }
}

/// Full attestation text: canonical statement + signature + signing key.
pub fn sign(kp: &Keypair, st: &Statement) -> String {
    let body = st.canonical();
    let sig = kp.signing.sign(body.as_bytes());
    format!(
        "{body}signature: {}\nkey: {}\n",
        B64.encode(sig.to_bytes()),
        kp.public_b64()
    )
}

/// Checks the signature with `key` (the pinned public key — the `key:` line is informational).
pub fn verify(text: &str, key: &VerifyingKey) -> Result<Statement> {
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        bail!("not a review-gate attestation");
    }
    let mut map = std::collections::HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(": ") {
            map.insert(k.to_string(), v.to_string());
        }
    }
    let get = |k: &str| {
        map.get(k)
            .cloned()
            .with_context(|| format!("attestation lacks `{k}`"))
    };
    let st = Statement {
        task: get("task")?,
        version: get("version")?,
        commit: get("commit")?,
        tree: get("tree")?,
        parent: get("parent")?,
        review: get("review")?,
        reviewer: get("reviewer")?,
        verdict: get("verdict")?,
        at: get("at")?,
    };
    let sig_bytes = B64
        .decode(get("signature")?)
        .context("signature is not base64")?;
    let sig = Signature::from_slice(&sig_bytes).context("malformed signature")?;
    key.verify_strict(st.canonical().as_bytes(), &sig)
        .context("signature does not verify with the pinned key")?;
    if st.verdict != "approved" {
        bail!("attestation verdict is {}", st.verdict);
    }
    Ok(st)
}

/// Verifies that `rev` carries a valid attestation note whose commit/tree/parent match the commit.
pub fn verify_commit(repo: &Path, rev: &str, key: &VerifyingKey) -> Result<Statement> {
    let commit = git::rev_parse(repo, &format!("{rev}^{{commit}}"))?;
    let note = git::git(
        repo,
        &["notes", &format!("--ref={NOTES_REF}"), "show", &commit],
    )
    .map_err(|_| anyhow::anyhow!("{commit}: no review-gate attestation note"))?;
    let st = verify(&note, key).with_context(|| format!("{commit}: invalid attestation"))?;
    let tree = git::rev_parse(repo, &format!("{commit}^{{tree}}"))?;
    let parents = git::parents(repo, &commit)?;
    if st.commit != commit || st.tree != tree || parents.len() != 1 || st.parent != parents[0] {
        bail!(
            "{commit}: attestation is for commit {} / tree {} / parent {}, not this commit",
            st.commit,
            st.tree,
            st.parent
        );
    }
    Ok(st)
}

/// Verifies every commit in `range` (`from..to`, as for git rev-list). Merge commits are
/// rejected unless `allow_merges`. Returns (verified, failures).
pub fn verify_range(
    repo: &Path,
    range: &str,
    key: &VerifyingKey,
    allow_merges: bool,
) -> Result<(Vec<Statement>, Vec<String>)> {
    let list = git::git(repo, &["rev-list", "--reverse", range])?;
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for c in list.lines().filter(|l| !l.is_empty()) {
        if allow_merges && git::parents(repo, c)?.len() > 1 {
            continue;
        }
        match verify_commit(repo, c, key) {
            Ok(st) => ok.push(st),
            Err(e) => bad.push(format!("{e:#}")),
        }
    }
    Ok((ok, bad))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmt() -> Statement {
        Statement {
            task: "t".into(),
            version: "1/2".into(),
            commit: "c".repeat(40),
            tree: "e".repeat(40),
            parent: "a".repeat(40),
            review: "v1-1".into(),
            reviewer: "web-gemini/web-gemini".into(),
            verdict: "approved".into(),
            at: "2026-10-04T00:00:00Z".into(),
        }
    }

    #[test]
    fn key_is_created_once_with_private_permissions_and_reloaded() {
        let d = tempfile::tempdir().unwrap();
        let a = Keypair::load_or_create(d.path()).unwrap();
        let b = Keypair::load_or_create(d.path()).unwrap();
        assert_eq!(a.public_b64(), b.public_b64());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(m(&d.path().join("keys/gate.ed25519")), 0o600);
            assert_eq!(m(&d.path().join("keys")), 0o700);
        }
    }

    #[test]
    fn sign_verify_and_tamper_detection() {
        let d = tempfile::tempdir().unwrap();
        let kp = Keypair::load_or_create(d.path()).unwrap();
        let key = parse_public_key(&kp.public_b64()).unwrap();
        let text = sign(&kp, &stmt());
        assert_eq!(verify(&text, &key).unwrap(), stmt());
        let forged = text.replace(&"c".repeat(40), &"d".repeat(40));
        assert!(
            verify(&forged, &key).is_err(),
            "changed commit must not verify"
        );
        let other = Keypair::load_or_create(&d.path().join("other")).unwrap();
        assert!(
            verify(&text, &parse_public_key(&other.public_b64()).unwrap()).is_err(),
            "pinned key decides, not the key: line"
        );
        assert!(verify("hello", &key).is_err());
    }
}
