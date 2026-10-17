//! Milestone audit records (plans/M2-design.md §3.3): an append-only chain, each record hashed
//! over its canonical JSON and signed by the gate.

use anyhow::{Context, Result, bail};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::attest::{Keypair, key_id, verify_bytes};

/// Record format version.
pub const FORMAT: u32 = 1;
/// `prev` of the first record.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Domain prefix of every record signature, so a record signature can never be mistaken for a
/// commit attestation or anything else the gate signs.
const SIG_DOMAIN: &str = "review-gate-milestone-record v1\n";

/// One record. `hash` covers every other field except `sig`; `sig` signs the hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub format: u32,
    pub seq: u64,
    pub prev: String,
    pub at: String,
    pub repo: String,
    pub key_id: String,
    pub kind: String,
    pub phase: String,
    pub body: Value,
    pub hash: String,
    pub sig: String,
}

/// What a caller supplies for a new record; the chain fills in the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub kind: String,
    pub phase: String,
    pub body: Value,
}

/// Canonical JSON (RFC 8785 for the values records use): object keys sorted by UTF-16 code
/// units, no insignificant whitespace, strings escaped as JSON requires, integers only.
pub fn canonical(v: &Value) -> Result<String> {
    let mut out = String::new();
    write_canonical(v, &mut out)?;
    Ok(out)
}

fn write_canonical(v: &Value, out: &mut String) -> Result<()> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                bail!("records hold integers only, not {n}");
            }
            out.push_str(&n.to_string());
        }
        Value::String(s) => out.push_str(&serde_json::to_string(s)?),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(x, out)?;
            }
            out.push(']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k)?);
                out.push(':');
                write_canonical(&m[k.as_str()], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Record {
    /// The hash a record must carry: sha256 of the canonical JSON of all fields but hash, sig.
    pub fn compute_hash(&self) -> Result<String> {
        let mut v = serde_json::to_value(self)?;
        let obj = v.as_object_mut().expect("a record is an object");
        obj.remove("hash");
        obj.remove("sig");
        Ok(sha256_hex(canonical(&v)?.as_bytes()))
    }

    fn signed_message(hash: &str) -> Vec<u8> {
        format!("{SIG_DOMAIN}{hash}").into_bytes()
    }

    /// One line of the log.
    pub fn to_line(&self) -> Result<String> {
        canonical(&serde_json::to_value(self)?)
    }
}

/// Builds and signs the record that follows `prev` (None for the first one).
pub fn seal(kp: &Keypair, prev: Option<&Record>, repo: &str, at: &str, d: Draft) -> Result<Record> {
    let mut r = Record {
        format: FORMAT,
        seq: prev.map_or(1, |p| p.seq + 1),
        prev: prev.map_or_else(|| GENESIS.to_string(), |p| p.hash.clone()),
        at: at.to_string(),
        repo: repo.to_string(),
        key_id: key_id(&kp.verifying_key()),
        kind: d.kind,
        phase: d.phase,
        body: d.body,
        hash: String::new(),
        sig: String::new(),
    };
    r.hash = r.compute_hash()?;
    r.sig = kp.sign_bytes(&Record::signed_message(&r.hash));
    Ok(r)
}

/// Verifies a whole chain: format, sequence, links, hashes, repository, key id and every
/// signature. Returns the first problem, naming the record.
pub fn verify_chain(records: &[Record], key: &VerifyingKey, repo: &str) -> Result<()> {
    let kid = key_id(key);
    let mut prev = GENESIS.to_string();
    for (i, r) in records.iter().enumerate() {
        let at = || format!("record {}", i + 1);
        if r.format != FORMAT {
            bail!("{}: format {} is not {FORMAT}", at(), r.format);
        }
        if r.seq != i as u64 + 1 {
            bail!("{}: seq {} out of order", at(), r.seq);
        }
        if r.prev != prev {
            bail!("{}: does not link to the record before it", at());
        }
        if r.repo != repo {
            bail!("{}: repository {:?} is not {repo:?}", at(), r.repo);
        }
        if r.key_id != kid {
            bail!("{}: signed with key {} not {kid}", at(), r.key_id);
        }
        let h = r.compute_hash().with_context(at)?;
        if h != r.hash {
            bail!("{}: hash does not match its content", at());
        }
        verify_bytes(key, &Record::signed_message(&r.hash), &r.sig).with_context(at)?;
        prev = r.hash.clone();
    }
    Ok(())
}

/// Parses a log (one canonical record per line). Every line must be a complete record: a
/// truncated last line is an error, never silently dropped (§3.7).
pub fn parse_log(text: &str) -> Result<Vec<Record>> {
    let mut out = Vec::new();
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if !line.ends_with('\n') {
            bail!(
                "line {}: incomplete (no newline): the log was cut off",
                i + 1
            );
        }
        let line = line.trim_end_matches('\n');
        let r: Record =
            serde_json::from_str(line).with_context(|| format!("line {}: not a record", i + 1))?;
        if r.to_line()? != line {
            bail!("line {}: not in canonical form", i + 1);
        }
        out.push(r);
    }
    Ok(out)
}

/// The log text for records (each line canonical, newline-terminated).
pub fn render_log(records: &[Record]) -> Result<String> {
    let mut s = String::new();
    for r in records {
        s.push_str(&r.to_line()?);
        s.push('\n');
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kp(n: u8) -> Keypair {
        Keypair::from_seed([n; 32])
    }

    fn draft(kind: &str, body: Value) -> Draft {
        Draft {
            kind: kind.into(),
            phase: "m2".into(),
            body,
        }
    }

    fn chain(k: &Keypair, n: usize) -> Vec<Record> {
        let mut v: Vec<Record> = Vec::new();
        for i in 0..n {
            let r = seal(
                k,
                v.last(),
                "repo",
                "2026-10-15T00:00:00Z",
                draft("note", json!({"i": i})),
            )
            .unwrap();
            v.push(r);
        }
        v
    }

    #[test]
    fn canonical_json_sorts_keys_and_has_no_whitespace() {
        let v = json!({"b": 1, "a": [true, null, "x\ny"], "é": {"z": 0, "Z": -3}});
        assert_eq!(
            canonical(&v).unwrap(),
            r#"{"a":[true,null,"x\ny"],"b":1,"é":{"Z":-3,"z":0}}"#
        );
        assert!(canonical(&json!({"f": 1.5})).is_err());
    }

    #[test]
    fn a_sealed_chain_verifies_and_round_trips_through_the_log() {
        let k = kp(1);
        let c = chain(&k, 3);
        assert_eq!((c[0].seq, c[0].prev.as_str()), (1, GENESIS));
        assert_eq!(c[2].prev, c[1].hash);
        verify_chain(&c, &k.verifying_key(), "repo").unwrap();
        let text = render_log(&c).unwrap();
        assert_eq!(parse_log(&text).unwrap(), c);
    }

    #[test]
    fn tampering_is_caught_record_by_record() {
        let k = kp(1);
        let key = k.verifying_key();
        let c = chain(&k, 3);
        let err = |c: &[Record]| verify_chain(c, &key, "repo").unwrap_err().to_string();

        let mut body = c.clone();
        body[1].body = json!({"i": 99});
        assert!(err(&body).contains("record 2: hash"));

        let mut rehashed = body.clone();
        rehashed[1].hash = rehashed[1].compute_hash().unwrap();
        assert!(
            err(&rehashed).contains("record 2"),
            "a new hash needs a new signature"
        );

        let mut dropped = c.clone();
        dropped.remove(1);
        assert!(err(&dropped).contains("record 2"));

        let mut other_key = c.clone();
        other_key[2] = seal(&kp(2), Some(&c[1]), "repo", "t", draft("note", json!({}))).unwrap();
        assert!(err(&other_key).contains("record 3: signed with key"));

        // A correctly numbered, validly signed record that links to the wrong predecessor.
        let mut fake = c[1].clone();
        fake.hash = "ab".repeat(32);
        let mut relinked = c.clone();
        relinked[2] = seal(&k, Some(&fake), "repo", "t", draft("note", json!({}))).unwrap();
        assert_eq!(relinked[2].seq, 3);
        assert!(err(&relinked).contains("record 3: does not link"));

        assert!(verify_chain(&c, &key, "other-repo").is_err());
        assert!(verify_chain(&c, &kp(2).verifying_key(), "repo").is_err());
    }

    #[test]
    fn a_cut_off_or_rewritten_log_line_is_an_error() {
        let k = kp(1);
        let text = render_log(&chain(&k, 2)).unwrap();
        let cut = &text[..text.len() - 5];
        assert!(
            parse_log(cut)
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
        let spaced = text.replacen("\"seq\":1", "\"seq\": 1", 1);
        assert!(
            parse_log(&spaced)
                .unwrap_err()
                .to_string()
                .contains("canonical")
        );
        assert!(parse_log("").unwrap().is_empty());
    }
}
