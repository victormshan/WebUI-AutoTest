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
        let text = fs::read_to_string(path)
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
                })
            }
        }
    }
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
}
