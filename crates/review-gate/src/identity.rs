//! Service-side identities and reviewer families.
//!
//! The gate reads a read-only service configuration (written by the administrator, never by the
//! implementer) that maps credential **hashes** — never the tokens themselves — to a stable
//! principal and a canonical model family. Creating a task freezes both, so later operations can
//! be authorized against that principal and external reviews can be required to come from a
//! different family.
//!
//! `client.token` is still supported, but only as an explicit legacy compatibility entry: when no
//! configuration file is given it authenticates as principal `legacy-claude`, family `claude`.
//! That file remains a plaintext credential file; "only hashes are stored" describes the new
//! administrator mapping, not the legacy token. A configuration file that exists but does not
//! load is fatal: there is deliberately no fallback to the legacy token.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::store::valid_id;

pub const LEGACY_PRINCIPAL: &str = "legacy-claude";
pub const LEGACY_FAMILY: &str = "claude";
pub const CONFIG_VERSION: u32 = 1;
/// The default administrator-managed configuration path. It is only consulted when it exists.
pub const DEFAULT_CONFIG: &str = "/etc/review-gate/identity.json";

/// Canonical model families. A family names the vendor of the model, not the tool or protocol.
pub const CANONICAL_FAMILIES: &[&str] = &["claude", "openai", "deepseek", "gemini"];

/// Maps a configured family name (or a known alias) to its canonical form.
/// `openai-compatible` is deliberately **not** an alias of `openai`: a protocol is not a vendor.
pub fn canonical_family(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Some("claude"),
        "openai" => Some("openai"),
        "deepseek" => Some("deepseek"),
        "gemini" | "google" | "google-gemini" => Some("gemini"),
        _ => None,
    }
}

/// The authenticated caller of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub principal: String,
    pub credential: String,
    pub family: String,
    pub admin: bool,
    /// SHA-256 of the configuration content that authenticated this credential (not a secret).
    /// `None` for the legacy plaintext token.
    pub config_digest: Option<String>,
    /// The local administrator session (`admin manual`, run as the gate's user). Only
    /// [`Identity::admin`] sets it; nothing that arrives over HTTP can.
    pub local_admin: bool,
}

impl Identity {
    pub fn new(principal: impl Into<String>, family: impl Into<String>) -> Self {
        let family = family.into();
        Self {
            principal: principal.into(),
            credential: "test".into(),
            family: canonical_family(&family).unwrap_or("unknown").to_string(),
            admin: false,
            config_digest: None,
            local_admin: false,
        }
    }

    /// The legacy `client.token`: principal `legacy-claude`, family `claude`, not an administrator.
    pub fn legacy() -> Self {
        Self {
            principal: LEGACY_PRINCIPAL.into(),
            credential: "legacy-client-token".into(),
            family: LEGACY_FAMILY.into(),
            admin: false,
            config_digest: None,
            local_admin: false,
        }
    }

    /// A local administrator session (the `admin manual` command, never HTTP).
    pub fn admin() -> Self {
        Self {
            principal: "admin".into(),
            credential: "local-admin".into(),
            family: "human".into(),
            admin: true,
            config_digest: None,
            local_admin: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Credential {
    principal: String,
    credential: String,
    family: &'static str,
    hash: [u8; 32],
    enabled: bool,
    admin: bool,
}

/// Credentials the service accepts. Hashing happens once at load; tokens are never stored.
#[derive(Debug, Clone)]
pub enum IdentityConfig {
    /// Explicit legacy compatibility: the single plaintext `client.token`.
    Legacy { token: String },
    /// Administrator-managed mapping of credential hashes to principals and families.
    Hashed {
        credentials: Vec<Credential>,
        /// SHA-256 of the configuration text, recorded on tasks for accountability.
        digest: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    version: u32,
    principals: Vec<PrincipalEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrincipalEntry {
    id: String,
    family: String,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    #[serde(default)]
    admin: bool,
    credentials: Vec<CredentialEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialEntry {
    id: String,
    sha256: String,
}

fn enabled_by_default() -> bool {
    true
}

impl IdentityConfig {
    /// The explicit legacy entry: one plaintext token, principal `legacy-claude`, family `claude`.
    pub fn legacy(token: impl Into<String>) -> Self {
        Self::Legacy {
            token: token.into(),
        }
    }

    /// Loads and validates an administrator configuration. Any error is fatal to the caller;
    /// nothing here falls back to the legacy token.
    pub fn load(path: &Path) -> Result<Self> {
        let text = read_admin_file(path)
            .with_context(|| format!("reading identity configuration {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("identity configuration {}", path.display()))
    }

    /// Strict parse: unknown fields, families, versions, disabled/duplicate mappings are errors.
    pub fn parse(text: &str) -> Result<Self> {
        let file: ConfigFile = serde_json::from_str(text).context("not valid JSON")?;
        if file.version != CONFIG_VERSION {
            bail!(
                "unsupported identity configuration version {} (expected {})",
                file.version,
                CONFIG_VERSION
            );
        }
        if file.principals.is_empty() {
            bail!("identity configuration lists no principals");
        }
        let mut principal_ids = HashSet::new();
        let mut credential_ids = HashSet::new();
        let mut hashes = HashSet::new();
        let mut credentials = Vec::new();
        for p in file.principals {
            if !valid_id(&p.id) {
                bail!("invalid principal id {:?}", p.id);
            }
            if !principal_ids.insert(p.id.clone()) {
                bail!("duplicate principal id {:?}", p.id);
            }
            let family = canonical_family(&p.family).ok_or_else(|| {
                anyhow::anyhow!(
                    "principal {:?} has unknown family {:?} (known: {})",
                    p.id,
                    p.family,
                    CANONICAL_FAMILIES.join(", ")
                )
            })?;
            if p.credentials.is_empty() {
                bail!("principal {:?} has no credentials", p.id);
            }
            for c in p.credentials {
                if !valid_id(&c.id) {
                    bail!("invalid credential id {:?}", c.id);
                }
                if !credential_ids.insert(c.id.clone()) {
                    bail!("duplicate credential id {:?}", c.id);
                }
                let hash = parse_hash(&c.sha256)
                    .with_context(|| format!("credential {:?} of {:?}", c.id, p.id))?;
                if !hashes.insert(hash) {
                    bail!(
                        "credential {:?} of {:?} maps a token that another credential already maps",
                        c.id,
                        p.id
                    );
                }
                credentials.push(Credential {
                    principal: p.id.clone(),
                    credential: c.id,
                    family,
                    hash,
                    enabled: p.enabled,
                    admin: p.admin,
                });
            }
        }
        Ok(Self::Hashed {
            credentials,
            digest: sha256_hex(text.as_bytes()),
        })
    }

    /// Whether `principal` exists and is enabled here (the legacy entry is `legacy-claude`).
    pub fn has_enabled_principal(&self, principal: &str) -> bool {
        match self {
            Self::Legacy { .. } => principal == LEGACY_PRINCIPAL,
            Self::Hashed { credentials, .. } => credentials
                .iter()
                .any(|c| c.enabled && c.principal == principal),
        }
    }

    /// Resolves a bearer token to an identity. Disabled credentials and principals never
    /// authenticate; there is no fallback to another entry.
    pub fn authenticate(&self, token: &str) -> Option<Identity> {
        if token.is_empty() {
            return None;
        }
        match self {
            Self::Legacy { token: expected } => {
                constant_time_eq(expected.as_bytes(), token.as_bytes()).then(|| Identity {
                    principal: LEGACY_PRINCIPAL.into(),
                    credential: "legacy-client-token".into(),
                    family: LEGACY_FAMILY.into(),
                    admin: false,
                    config_digest: None,
                    local_admin: false,
                })
            }
            Self::Hashed {
                credentials,
                digest,
            } => {
                let given: [u8; 32] = Sha256::digest(token.as_bytes()).into();
                let mut found: Option<&Credential> = None;
                for c in credentials {
                    if c.enabled && constant_time_eq(&c.hash, &given) {
                        found = Some(c);
                    }
                }
                found.map(|c| Identity {
                    principal: c.principal.clone(),
                    credential: c.credential.clone(),
                    family: c.family.to_string(),
                    admin: c.admin,
                    config_digest: Some(digest.clone()),
                    local_admin: false,
                })
            }
        }
    }
}

/// Reads an administrator-owned configuration file (Codex review of the identity binding, n18
/// F3): opened without following a symlink, checked on the same open file (a regular file owned
/// by root or by the service's own user, no write bit for group or others, nothing for others),
/// read from that same file, and its directory checked the same way so another caller cannot
/// replace it. Errors name the path and the reason, never the content.
pub fn read_admin_file(path: &Path) -> Result<String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    let trusted = |uid: u32| uid == 0 || uid == euid;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let d = fs::metadata(dir).with_context(|| format!("directory of {}", path.display()))?;
    if !trusted(d.uid()) || d.mode() & 0o022 != 0 {
        bail!(
            "{} is in a directory another user can change (owner uid {}, mode {:o})",
            path.display(),
            d.uid(),
            d.mode() & 0o7777
        );
    }
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("opening {} (symlinks are not followed)", path.display()))?;
    let m = f.metadata()?;
    if !m.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if !trusted(m.uid()) {
        bail!(
            "{} is owned by uid {}, not by root or the service's user",
            path.display(),
            m.uid()
        );
    }
    if m.mode() & 0o027 != 0 {
        bail!(
            "{} has mode {:o}: group or others may write it, or others may read it (use 0600 or 0640)",
            path.display(),
            m.mode() & 0o777
        );
    }
    let mut text = String::new();
    f.read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(text)
}

/// The JSON object of an agents file, as written: every key with its value, duplicates kept
/// (a map type would keep only the last one).
struct Pairs(Vec<(String, String)>);

impl<'de> serde::Deserialize<'de> for Pairs {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Pairs;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object of token -> family strings")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Pairs, A::Error> {
                let mut v = Vec::new();
                while let Some(kv) = m.next_entry::<String, String>()? {
                    v.push(kv);
                }
                Ok(Pairs(v))
            }
        }
        d.deserialize_map(V)
    }
}

/// `--agents-file` (the identity binding deployed on 2026-10-22, token -> family) is kept as a
/// command-line form, but it no longer creates identities: the identity configuration is the
/// only authority (Codex n20 ①). Every entry must be a token that the identity configuration
/// already maps, to an enabled credential of the same family; anything else stops the start.
/// Duplicate tokens (after JSON unescaping) and the legacy client token are refused (n18 F2).
/// Returns the number of entries. Errors name entries by position, never by token.
pub fn check_agents_file(
    path: &Path,
    ids: &IdentityConfig,
    legacy_token: Option<&str>,
) -> Result<usize> {
    let IdentityConfig::Hashed { credentials, .. } = ids else {
        bail!(
            "--agents-file needs --identity-config: the agents file names no principals, so it cannot be the identity authority"
        );
    };
    let text = read_admin_file(path)?;
    let Pairs(pairs) = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "agents file {}: not a JSON object of strings (line {}, column {})",
            path.display(),
            e.line(),
            e.column()
        )
    })?;
    if pairs.is_empty() {
        bail!("agents file {} is empty", path.display());
    }
    let mut seen = HashSet::new();
    for (i, (token, family)) in pairs.iter().enumerate() {
        let n = i + 1;
        if token.len() < 32 || !token.chars().all(|c| c.is_ascii_graphic()) {
            bail!("agents file entry {n}: the token is shorter than 32 printable characters");
        }
        if !seen.insert(token.as_str()) {
            bail!("agents file entry {n}: the same token appears more than once");
        }
        if legacy_token.is_some_and(|l| constant_time_eq(l.as_bytes(), token.as_bytes())) {
            bail!("agents file entry {n}: the token is the legacy client token");
        }
        let Some(fam) = canonical_family(family) else {
            bail!(
                "agents file entry {n}: unknown family (known: {})",
                CANONICAL_FAMILIES.join(", ")
            );
        };
        let given: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let Some(c) = credentials
            .iter()
            .find(|c| constant_time_eq(&c.hash, &given))
        else {
            bail!("agents file entry {n}: the token is not in the identity configuration");
        };
        if !c.enabled {
            bail!(
                "agents file entry {n}: the token belongs to a disabled principal ({})",
                c.principal
            );
        }
        if c.family != fam {
            bail!(
                "agents file entry {n}: family {fam} differs from principal {}'s family {}",
                c.principal,
                c.family
            );
        }
    }
    Ok(pairs.len())
}

/// The bytes a client sends for a token file's content: [`crate::client::Client::new`] trims it.
/// Every hash the converter writes is taken over exactly this.
pub fn normalize_token(file_text: &str) -> &str {
    file_text.trim()
}

/// One principal for [`build_config`]: its id, family and the file holding its token.
#[derive(Debug, Clone)]
pub struct CredentialSpec {
    pub principal: String,
    pub family: String,
    pub token_file: std::path::PathBuf,
}

/// Reads a token file the converter is given: not a symlink, a regular file, nothing for group
/// or others. The content never appears in an error.
fn read_token_file(path: &Path) -> Result<String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| {
            format!(
                "opening token file {} (symlinks are not followed)",
                path.display()
            )
        })?;
    let m = f.metadata()?;
    if !m.is_file() || m.mode() & 0o077 != 0 {
        bail!(
            "token file {} must be a regular file with mode 0600 or stricter",
            path.display()
        );
    }
    let mut t = String::new();
    f.read_to_string(&mut t)?;
    let tok = normalize_token(&t).to_string();
    if tok.len() < 32 || !tok.chars().all(|c| c.is_ascii_graphic()) {
        bail!(
            "token file {}: the token is shorter than 32 printable characters",
            path.display()
        );
    }
    Ok(tok)
}

/// The migration converter (Codex n20, configuration migration): builds an identity
/// configuration (hashes only) from token files, with the legacy client token as the explicit
/// `legacy-claude` entry, and — when given — checks it against the deployed agents file: the
/// same set of tokens, each with the same family. Duplicate principals or tokens (after the
/// client's normalization), unknown families and any mismatch are errors; no token is ever
/// printed. The result is parsed back with [`IdentityConfig::parse`] before it is returned.
pub fn build_config(
    creds: &[CredentialSpec],
    legacy: Option<&Path>,
    agents: Option<&Path>,
) -> Result<String> {
    let mut principals = Vec::new();
    let mut ids = HashSet::new();
    let mut tokens: Vec<(String, &'static str, String)> = Vec::new(); // token, family, principal
    let mut push = |id: &str, family: &str, token: String| -> Result<()> {
        if !valid_id(id) || !ids.insert(id.to_string()) {
            bail!("principal {id:?} is invalid or given twice");
        }
        let fam = canonical_family(family)
            .ok_or_else(|| anyhow::anyhow!("principal {id:?}: unknown family {family:?}"))?;
        if tokens
            .iter()
            .any(|(t, _, _)| constant_time_eq(t.as_bytes(), token.as_bytes()))
        {
            bail!("principal {id:?} has the same token as another principal");
        }
        principals.push(serde_json::json!({
            "id": id, "family": fam,
            "credentials": [{ "id": format!("{id}-1"), "sha256": token_hash(&token) }],
        }));
        tokens.push((token, fam, id.to_string()));
        Ok(())
    };
    for c in creds {
        let t = read_token_file(&c.token_file)?;
        push(&c.principal, &c.family, t)?;
    }
    if let Some(l) = legacy {
        let t = read_token_file(l)?;
        push(LEGACY_PRINCIPAL, LEGACY_FAMILY, t)?;
    }
    if let Some(a) = agents {
        let text = read_admin_file(a)?;
        let Pairs(pairs) = serde_json::from_str(&text).map_err(|_| {
            anyhow::anyhow!("agents file {}: not a JSON object of strings", a.display())
        })?;
        let mut matched = HashSet::new();
        for (i, (token, family)) in pairs.iter().enumerate() {
            let n = i + 1;
            let Some((_, fam, p)) = tokens
                .iter()
                .find(|(t, _, _)| constant_time_eq(t.as_bytes(), token.as_bytes()))
            else {
                bail!("agents file entry {n}: its token is none of the given token files");
            };
            if p == LEGACY_PRINCIPAL {
                bail!("agents file entry {n}: its token is the legacy client token");
            }
            if canonical_family(family) != Some(*fam) {
                bail!("agents file entry {n}: family differs from principal {p}'s family {fam}");
            }
            if !matched.insert(p.clone()) {
                bail!("agents file entry {n}: the same token appears more than once");
            }
        }
        for (_, _, p) in &tokens {
            if p != LEGACY_PRINCIPAL && !matched.contains(p) {
                bail!("principal {p}'s token is not in the agents file");
            }
        }
    }
    let out = serde_json::to_string_pretty(&serde_json::json!({
        "version": CONFIG_VERSION, "principals": principals,
    }))? + "\n";
    IdentityConfig::parse(&out).context("the generated configuration does not load")?;
    Ok(out)
}

fn parse_hash(text: &str) -> Result<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("sha256 must be 64 hexadecimal characters");
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

/// Hex SHA-256 of a token, the form an administrator puts in the identity configuration.
pub fn token_hash(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(principals: &str) -> String {
        format!(r#"{{"version": 1, "principals": {principals}}}"#)
    }

    fn principal(id: &str, family: &str, enabled: bool, token: &str) -> String {
        format!(
            r#"{{"id": "{id}", "family": "{family}", "enabled": {enabled},
                "credentials": [{{"id": "{id}-1", "sha256": "{}"}}]}}"#,
            token_hash(token)
        )
    }

    #[test]
    fn families_are_canonical_and_protocols_are_not_vendors() {
        assert_eq!(canonical_family("Anthropic"), Some("claude"));
        assert_eq!(canonical_family("google"), Some("gemini"));
        assert_eq!(canonical_family("deepseek"), Some("deepseek"));
        assert_eq!(canonical_family("openai-compatible"), None);
        assert_eq!(canonical_family("unknown-vendor"), None);
    }

    #[test]
    fn the_three_principals_authenticate_to_their_own_family() {
        let text = config(&format!(
            "[{}, {}, {}]",
            principal("claude", "claude", true, "claude-token"),
            principal("opencode", "deepseek", true, "opencode-token"),
            principal("codex", "openai", true, "codex-token"),
        ));
        let c = IdentityConfig::parse(&text).unwrap();
        let id = c.authenticate("opencode-token").unwrap();
        assert_eq!(
            (id.principal.as_str(), id.family.as_str()),
            ("opencode", "deepseek")
        );
        // Accountability data: the credential id and a hash of the configuration content.
        assert_eq!(id.credential, "opencode-1");
        assert!(
            id.config_digest
                .as_deref()
                .is_some_and(|d| d.len() == 64 && d.chars().all(|c| c.is_ascii_hexdigit()))
        );
        assert_eq!(c.authenticate("codex-token").unwrap().family, "openai");
        assert_eq!(c.authenticate("claude-token").unwrap().family, "claude");
        assert!(c.authenticate("wrong-token").is_none());
        assert!(c.authenticate("").is_none());
    }

    #[test]
    fn the_legacy_entry_maps_to_legacy_claude_and_never_falls_back() {
        let legacy = IdentityConfig::legacy("old-token");
        let id = legacy.authenticate("old-token").unwrap();
        assert_eq!(
            (id.principal.as_str(), id.family.as_str()),
            (LEGACY_PRINCIPAL, LEGACY_FAMILY)
        );
        assert_eq!(id.credential, "legacy-client-token");
        assert!(
            id.config_digest.is_none(),
            "the legacy token file is not an admin configuration"
        );
        // A new configuration does not know the legacy token: no fallback.
        let text = config(&format!(
            "[{}]",
            principal("opencode", "deepseek", true, "new-token")
        ));
        let c = IdentityConfig::parse(&text).unwrap();
        assert!(c.authenticate("old-token").is_none());
    }

    #[test]
    fn revocation_and_rotation_keep_the_principal() {
        let first = config(&format!(
            "[{}]",
            principal("opencode", "deepseek", true, "token-one")
        ));
        let c = IdentityConfig::parse(&first).unwrap();
        assert_eq!(c.authenticate("token-one").unwrap().principal, "opencode");
        assert!(c.authenticate("token-two").is_none());

        let rotated = config(&format!(
            "[{}]",
            principal("opencode", "deepseek", true, "token-two")
        ));
        let c = IdentityConfig::parse(&rotated).unwrap();
        assert!(
            c.authenticate("token-one").is_none(),
            "old credential revoked"
        );
        assert_eq!(
            c.authenticate("token-two").unwrap().principal,
            "opencode",
            "rotation does not change the principal"
        );

        let disabled = config(&format!(
            "[{}]",
            principal("opencode", "deepseek", false, "token-two")
        ));
        let c = IdentityConfig::parse(&disabled).unwrap();
        assert!(c.authenticate("token-two").is_none(), "disabled mapping");
    }

    #[test]
    fn invalid_configurations_are_fail_closed() {
        // Unknown family: a protocol name is not a vendor.
        let unknown = config(&format!(
            "[{}]",
            principal("x", "openai-compatible", true, "t")
        ));
        assert!(IdentityConfig::parse(&unknown).is_err());

        // Duplicate credential hash (two principals mapping the same token).
        let dup_hash = config(&format!(
            "[{}, {}]",
            principal("a", "claude", true, "same"),
            principal("b", "openai", true, "same"),
        ));
        assert!(IdentityConfig::parse(&dup_hash).is_err());

        // Duplicate principal id.
        let dup_principal = config(&format!(
            "[{}, {}]",
            principal("a", "claude", true, "one"),
            principal("a", "openai", true, "two"),
        ));
        assert!(IdentityConfig::parse(&dup_principal).is_err());

        // Unknown field (typo) is not silently ignored.
        let typo = config(&format!(
            r#"[{{"id": "a", "family": "claude", "credential": [],
                 "credentials": [{{"id": "a-1", "sha256": "{}"}}]}}]"#,
            token_hash("x")
        ));
        assert!(IdentityConfig::parse(&typo).is_err());

        let version = config(&format!("[{}]", principal("a", "claude", true, "x")))
            .replace("\"version\": 1", "\"version\": 2");
        assert!(IdentityConfig::parse(&version).is_err());

        let bad_hash = config(
            r#"[{"id": "a", "family": "claude",
                 "credentials": [{"id": "a-1", "sha256": "not-a-hash"}]}]"#,
        );
        assert!(IdentityConfig::parse(&bad_hash).is_err());
    }

    #[test]
    fn a_missing_file_is_an_error_the_caller_may_treat_as_legacy() {
        let d = tempfile::tempdir().unwrap();
        assert!(IdentityConfig::load(&d.path().join("absent.json")).is_err());
    }

    // ---- the agents file and administrator files (Codex n18 F2/F3, n20) ----

    use std::os::unix::fs::PermissionsExt;

    const T_CLAUDE: &str = "claude-token-0123456789abcdef0123456789abcdef";
    const T_OC: &str = "opencode-token-0123456789abcdef0123456789abcdef";
    const T_DSH: &str = "dsh-token-0123456789abcdef0123456789abcdef0123";
    const T_LEGACY: &str = "legacy-token-0123456789abcdef0123456789abcdef";

    fn ids() -> IdentityConfig {
        IdentityConfig::parse(&config(&format!(
            "[{}, {}, {}]",
            principal("claude", "claude", true, T_CLAUDE),
            principal("opencode", "deepseek", true, T_OC),
            principal("dsh", "deepseek", false, T_DSH),
        )))
        .unwrap()
    }

    fn private_dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o750)).unwrap();
        d
    }

    fn file(d: &tempfile::TempDir, body: &str, mode: u32) -> std::path::PathBuf {
        let p = d.path().join("agents.json");
        let _ = fs::remove_file(&p);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    fn agents_err(body: &str) -> String {
        let d = private_dir();
        let e = check_agents_file(&file(&d, body, 0o640), &ids(), Some(T_LEGACY))
            .unwrap_err()
            .to_string();
        for secret in [T_CLAUDE, T_OC, T_DSH, T_LEGACY] {
            assert!(!e.contains(secret), "a token was echoed: {e}");
        }
        e
    }

    #[test]
    fn an_admin_file_must_be_private_regular_and_not_a_symlink() {
        let d = private_dir();
        for ok in [0o600, 0o640, 0o400] {
            assert!(read_admin_file(&file(&d, "x", ok)).is_ok(), "{ok:o}");
        }
        for bad in [0o660, 0o620, 0o644, 0o604, 0o602] {
            let e = read_admin_file(&file(&d, "secret-content", bad))
                .unwrap_err()
                .to_string();
            assert!(e.contains("mode"), "{bad:o}: {e}");
            assert!(!e.contains("secret-content"), "{e}");
        }
        let real = file(&d, "x", 0o600);
        let link = d.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let e = read_admin_file(&link).unwrap_err().to_string();
        assert!(e.contains("symlinks are not followed"), "{e}");
        let sub = d.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();
        let e = read_admin_file(&sub).unwrap_err().to_string();
        assert!(e.contains("not a regular file"), "{e}");
        // A directory others can change: refused before the file is opened.
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o770)).unwrap();
        let e = read_admin_file(&real).unwrap_err().to_string();
        assert!(e.contains("directory another user can change"), "{e}");
        // Ownership by another uid needs root to set up; it is checked at deployment (RUNBOOK).
    }

    #[test]
    fn an_agents_file_only_checks_tokens_the_identity_configuration_maps() {
        let d = private_dir();
        let good = format!(r#"{{"{T_CLAUDE}": "claude", "{T_OC}": "DeepSeek"}}"#);
        assert_eq!(
            check_agents_file(&file(&d, &good, 0o640), &ids(), Some(T_LEGACY)).unwrap(),
            2
        );
        // Without an identity configuration it is refused: it names no principals.
        let e = check_agents_file(
            &file(&d, &good, 0o640),
            &IdentityConfig::legacy(T_LEGACY),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("needs --identity-config"), "{e}");
    }

    #[test]
    fn an_agents_file_with_a_bad_entry_stops_the_start_without_echoing_tokens() {
        let cases = [
            (
                format!(r#"{{"{T_OC}": "deepseek", "{T_OC}": "deepseek"}}"#),
                "more than once",
            ),
            // The same token once escaped: still a duplicate after unescaping.
            (
                format!(
                    r#"{{"{T_OC}": "deepseek", "{}": "deepseek"}}"#,
                    T_OC.replacen('o', "\\u006f", 1)
                ),
                "more than once",
            ),
            (
                format!(r#"{{"{T_LEGACY}": "claude"}}"#),
                "legacy client token",
            ),
            (
                r#"{"not-in-config-0123456789abcdef0123456789": "claude"}"#.to_string(),
                "not in the identity configuration",
            ),
            (
                format!(r#"{{"{T_DSH}": "deepseek"}}"#),
                "disabled principal",
            ),
            (
                format!(r#"{{"{T_OC}": "claude"}}"#),
                "differs from principal",
            ),
            (format!(r#"{{"{T_OC}": "opencode"}}"#), "unknown family"),
            (r#"{"short": "claude"}"#.to_string(), "shorter than 32"),
            ("{}".to_string(), "is empty"),
            (
                format!(r#"{{"{T_OC}": 1}}"#),
                "not a JSON object of strings",
            ),
        ];
        for (body, want) in cases {
            let e = agents_err(&body);
            assert!(e.contains(want), "{want}: {e}");
        }
    }

    // ---- the migration converter (n20) ----

    fn token_file(d: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let p = d.path().join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        p
    }

    fn spec(p: &str, f: &str, file: &std::path::Path) -> CredentialSpec {
        CredentialSpec {
            principal: p.into(),
            family: f.into(),
            token_file: file.into(),
        }
    }

    #[test]
    fn the_converter_hashes_what_the_client_sends_and_matches_the_agents_file() {
        let d = private_dir();
        // Token files with the trailing newline an editor or `echo` leaves.
        let c = token_file(&d, "claude.token", &format!("{T_CLAUDE}\n"));
        let o = token_file(&d, "opencode.token", &format!("  {T_OC}\n"));
        let l = token_file(&d, "client.token", &format!("{T_LEGACY}\n"));
        let agents = file(
            &d,
            &format!(r#"{{"{T_CLAUDE}": "claude", "{T_OC}": "deepseek"}}"#),
            0o640,
        );
        let text = build_config(
            &[
                spec("claude", "claude", &c),
                spec("opencode", "DeepSeek", &o),
            ],
            Some(&l),
            Some(&agents),
        )
        .unwrap();
        for secret in [T_CLAUDE, T_OC, T_LEGACY] {
            assert!(!text.contains(secret), "a token in the output");
        }
        let ids = IdentityConfig::parse(&text).unwrap();
        // What a client built from each file sends authenticates as the right principal.
        for (file, principal, family) in [
            (&c, "claude", "claude"),
            (&o, "opencode", "deepseek"),
            (&l, LEGACY_PRINCIPAL, "claude"),
        ] {
            let sent = normalize_token(&fs::read_to_string(file).unwrap()).to_string();
            let who = ids.authenticate(&sent).unwrap();
            assert_eq!(
                (who.principal.as_str(), who.family.as_str()),
                (principal, family)
            );
        }
        // The deployed agents file then passes the start-up check against it.
        assert_eq!(check_agents_file(&agents, &ids, Some(T_LEGACY)).unwrap(), 2);
    }

    #[test]
    fn the_converter_refuses_duplicates_mismatches_and_unknown_families() {
        let d = private_dir();
        let c = token_file(&d, "claude.token", T_CLAUDE);
        let o = token_file(&d, "opencode.token", T_OC);
        let same = token_file(&d, "same.token", &format!("{T_OC}\n"));
        let l = token_file(&d, "client.token", T_LEGACY);
        let err = |creds: &[CredentialSpec], legacy: Option<&Path>, agents: Option<&str>| {
            let a = agents.map(|b| file(&d, b, 0o640));
            let e = build_config(creds, legacy, a.as_deref())
                .unwrap_err()
                .to_string();
            for secret in [T_CLAUDE, T_OC, T_LEGACY] {
                assert!(!e.contains(secret), "{e}");
            }
            e
        };
        assert!(
            err(
                &[spec("a", "claude", &c), spec("a", "claude", &o)],
                None,
                None
            )
            .contains("given twice")
        );
        // The same token after the client's trimming, under two principals.
        assert!(
            err(
                &[
                    spec("opencode", "deepseek", &o),
                    spec("dsh", "deepseek", &same)
                ],
                None,
                None
            )
            .contains("same token")
        );
        assert!(err(&[spec("x", "opencode", &o)], None, None).contains("unknown family"));
        let e = err(
            &[spec("claude", "claude", &c)],
            None,
            Some(&format!(r#"{{"{T_OC}": "deepseek"}}"#)),
        );
        assert!(e.contains("none of the given token files"), "{e}");
        let e = err(
            &[spec("opencode", "deepseek", &o)],
            None,
            Some(&format!(r#"{{"{T_OC}": "claude"}}"#)),
        );
        assert!(e.contains("family differs"), "{e}");
        let e = err(
            &[
                spec("claude", "claude", &c),
                spec("opencode", "deepseek", &o),
            ],
            None,
            Some(&format!(r#"{{"{T_CLAUDE}": "claude"}}"#)),
        );
        assert!(e.contains("not in the agents file"), "{e}");
        let e = err(
            &[spec("claude", "claude", &c)],
            Some(&l),
            Some(&format!(
                r#"{{"{T_CLAUDE}": "claude", "{T_LEGACY}": "claude"}}"#
            )),
        );
        assert!(e.contains("legacy client token"), "{e}");
        // A token file others can read is refused.
        fs::set_permissions(&o, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(err(&[spec("opencode", "deepseek", &o)], None, None).contains("0600"));
    }
}
