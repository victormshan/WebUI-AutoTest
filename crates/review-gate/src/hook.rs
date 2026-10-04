//! Claude Code PreToolUse hook: `review-gate hook` (configured on the Bash tool).
//!
//! In a repository with an auto-iterate task, a `git commit` is let through only when the gate
//! holds an unused approved review for exactly the staged tree on top of HEAD, and a `git push`
//! only when every unpushed commit carries a valid attestation note. Anything else in other
//! repositories passes. Exit 2 blocks the tool call and shows the reason to Claude.
//!
//! This is a guard rail against mistakes, not the security boundary: the boundary is the gate's
//! private store (record checks) and the attestation check in CI.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use crate::client::Client;
use crate::model::Task;
use crate::{attest, git};

#[derive(Debug, PartialEq)]
pub enum GitOp {
    Commit { dir: Option<String>, all: bool },
    Push { dir: Option<String> },
}

/// Finds `git commit` / `git push` invocations in a shell command (best effort; quotes are
/// respected, `-C <dir>` is honoured).
pub fn git_ops(command: &str) -> Vec<GitOp> {
    let mut ops = Vec::new();
    for seg in split_commands(command) {
        let toks = seg;
        let Some(start) = toks.iter().position(|t| t == "git" || t.ends_with("/git")) else {
            continue;
        };
        let mut i = start + 1;
        let mut dir = None;
        while i < toks.len() && toks[i].starts_with('-') {
            match toks[i].as_str() {
                "-C" => {
                    dir = toks.get(i + 1).cloned();
                    i += 2;
                }
                "-c" | "--git-dir" | "--work-tree" | "--namespace" => i += 2,
                _ => i += 1,
            }
        }
        match toks.get(i).map(String::as_str) {
            Some("commit") => {
                let all = toks[i + 1..]
                    .iter()
                    .any(|t| t == "--all" || short_flags_include_all(t));
                ops.push(GitOp::Commit { dir, all });
            }
            Some("push") => ops.push(GitOp::Push { dir }),
            _ => {}
        }
    }
    ops
}

/// `-a`, `-am`, `-va` … but not `-ma` (that is `-m a`): stop at the first flag taking a value.
fn short_flags_include_all(t: &str) -> bool {
    let Some(flags) = t.strip_prefix('-') else {
        return false;
    };
    if flags.starts_with('-') {
        return false;
    }
    for c in flags.chars() {
        match c {
            'a' => return true,
            'm' | 'F' | 'C' | 'c' | 't' | 'S' | 'u' => return false,
            _ => {}
        }
    }
    false
}

/// Splits on `&&`, `||`, `;`, `|`, newlines, outside quotes, into token lists.
fn split_commands(s: &str) -> Vec<Vec<String>> {
    let mut out = vec![Vec::new()];
    let mut tok = String::new();
    let mut quote: Option<char> = None;
    let mut has_tok = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let flush = |tok: &mut String, has: &mut bool, out: &mut Vec<Vec<String>>| {
        if *has {
            out.last_mut().unwrap().push(std::mem::take(tok));
            *has = false;
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => tok.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    has_tok = true;
                }
                ' ' | '\t' => flush(&mut tok, &mut has_tok, &mut out),
                ';' | '|' | '&' | '\n' | '(' | ')' => {
                    flush(&mut tok, &mut has_tok, &mut out);
                    out.push(Vec::new());
                }
                '\\' if i + 1 < chars.len() => {
                    tok.push(chars[i + 1]);
                    has_tok = true;
                    i += 1;
                }
                _ => {
                    tok.push(c);
                    has_tok = true;
                }
            },
        }
        i += 1;
    }
    flush(&mut tok, &mut has_tok, &mut out);
    out.into_iter().filter(|v| !v.is_empty()).collect()
}

/// What to tell Claude Code: `None` lets the command run, `Some(reason)` blocks it (exit 2).
pub async fn decide(input: &Value, client: Option<&Client>) -> Option<String> {
    if input["tool_name"] != "Bash" {
        return None;
    }
    let command = input["tool_input"]["command"].as_str()?;
    let ops = git_ops(command);
    if ops.is_empty() {
        return None;
    }
    let cwd = PathBuf::from(input["cwd"].as_str().unwrap_or("."));
    let Some(client) = client else {
        return Some(
            "review-gate: no client token configured; cannot check this git command".into(),
        );
    };
    for op in ops {
        let (dir, is_commit, all) = match &op {
            GitOp::Commit { dir, all } => (dir, true, *all),
            GitOp::Push { dir } => (dir, false, false),
        };
        let dir = dir.as_ref().map_or(cwd.clone(), |d| cwd.join(d));
        let Ok(top) = git::git(&dir, &["rev-parse", "--show-toplevel"]) else {
            continue; // not a repository: git itself will complain
        };
        let repo = std::fs::canonicalize(top.trim()).unwrap_or_else(|_| PathBuf::from(top.trim()));
        let verdict = if is_commit {
            check_commit(client, &repo, all).await
        } else {
            check_push(client, &repo).await
        };
        match verdict {
            Ok(None) => {}
            Ok(Some(reason)) => return Some(reason),
            Err(e) => return Some(format!("review-gate: cannot check `{command}`: {e:#}")),
        }
    }
    None
}

async fn tasks_for(client: &Client, repo: &Path) -> Result<Vec<Task>> {
    let tasks: Vec<Task> = client.get("/tasks").await?;
    Ok(tasks
        .into_iter()
        .filter(|t| Path::new(&t.repo) == repo)
        .collect())
}

async fn check_commit(client: &Client, repo: &Path, all: bool) -> Result<Option<String>> {
    let running = tasks_for(client, repo)
        .await?
        .into_iter()
        .any(|t| t.status == crate::model::Status::Running);
    if !running {
        return Ok(None);
    }
    if all {
        return Ok(Some(
            "review-gate: in an auto-iterate repository commit the reviewed index as is (`git commit -m …` after the review), not with -a/--all".into(),
        ));
    }
    let tree = git::git(repo, &["write-tree"])?.trim().to_string();
    let base = git::rev_parse(repo, "HEAD")?;
    let v: Value = client
        .post(
            "/check",
            json!({ "repo": repo, "tree": tree, "base": base }),
        )
        .await?;
    Ok(if v["allowed"] == true {
        None
    } else {
        Some(format!(
            "review-gate: commit blocked — {}",
            v["reason"]
                .as_str()
                .unwrap_or("no approved review for the staged tree")
        ))
    })
}

async fn check_push(client: &Client, repo: &Path) -> Result<Option<String>> {
    if tasks_for(client, repo).await?.is_empty() {
        return Ok(None);
    }
    let pk: Value = client.get("/pubkey").await?;
    let key = attest::parse_public_key(pk["key"].as_str().unwrap_or_default())?;
    let unpushed = git::git(
        repo,
        &["rev-list", "--no-merges", "HEAD", "--not", "--remotes"],
    )?;
    let bad: Vec<String> = unpushed
        .lines()
        .filter(|c| !c.is_empty())
        .filter_map(|c| {
            attest::verify_commit(repo, c, &key)
                .err()
                .map(|e| format!("{e:#}"))
        })
        .collect();
    Ok((!bad.is_empty()).then(|| {
        format!(
            "review-gate: push blocked — {} unpushed commit(s) without a valid attestation:\n{}",
            bad.len(),
            bad.join("\n")
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reviewer::tests::mock_bridge;
    use crate::service::tests::start;
    use std::fs;

    #[test]
    fn finds_git_commit_and_push_in_shell_commands() {
        use GitOp::*;
        assert_eq!(git_ops("ls -la"), vec![]);
        assert_eq!(git_ops("git status && git log"), vec![]);
        assert_eq!(
            git_ops("git add -A && git commit -m 'git push later'"),
            vec![Commit {
                dir: None,
                all: false
            }]
        );
        assert_eq!(
            git_ops("git -C ../x -c a=b commit -qam fix"),
            vec![Commit {
                dir: Some("../x".into()),
                all: true
            }]
        );
        assert_eq!(
            git_ops("git commit --all -m x; /usr/bin/git push origin main"),
            vec![
                Commit {
                    dir: None,
                    all: true
                },
                Push { dir: None }
            ]
        );
        assert_eq!(
            git_ops("git commit -ma"),
            vec![Commit {
                dir: None,
                all: false
            }]
        );
        assert_eq!(git_ops("echo \"git commit\" | cat"), vec![]);
        assert_eq!(git_ops("(cd repo && git push)"), vec![Push { dir: None }]);
    }

    fn bash(cmd: &str, cwd: &Path) -> Value {
        json!({ "tool_name": "Bash", "tool_input": { "command": cmd }, "cwd": cwd })
    }

    #[tokio::test]
    async fn blocks_unreviewed_commits_and_unattested_pushes() {
        let (bridge, _) = mock_bridge(vec![Ok("VERDICT: APPROVED\n\n无")]).await;
        let e = start(&bridge).await;
        let c = &e.client;
        assert_eq!(decide(&json!({ "tool_name": "Read" }), Some(c)).await, None);
        assert_eq!(
            decide(&bash("ls", &e.repo), None).await,
            None,
            "no git op: no token needed"
        );
        assert!(
            decide(&bash("git commit -m x", &e.repo), None)
                .await
                .is_some(),
            "fail closed without token"
        );
        let elsewhere = e._dir.path().join("other");
        fs::create_dir(&elsewhere).unwrap();
        git::git(&elsewhere, &["init", "-q"]).unwrap();
        assert_eq!(
            decide(&bash("git commit -m x", &elsewhere), Some(c)).await,
            None,
            "repo without task"
        );

        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        let (tree, base) = git::stage_all(&e.repo).unwrap();
        let blocked = decide(&bash("git commit -m v1", &e.repo), Some(c)).await;
        assert!(
            blocked
                .as_deref()
                .unwrap_or_default()
                .contains("commit blocked"),
            "{blocked:?}"
        );
        let job = c
            .review(
                "t",
                &tree,
                &base,
                "",
                None,
                std::time::Duration::from_millis(20),
            )
            .await
            .unwrap();
        let rec = job.record.unwrap();
        assert_eq!(
            decide(&bash("git commit -m v1", &e.repo), Some(c)).await,
            None,
            "approved tree"
        );
        assert!(
            decide(&bash("git commit -am v1", &e.repo), Some(c))
                .await
                .is_some(),
            "-a is refused"
        );
        fs::write(e.repo.join("a.txt"), "v1 + more\n").unwrap();
        git::git(&e.repo, &["add", "-A"]).unwrap();
        assert!(
            decide(&bash("git commit -m v1", &e.repo), Some(c))
                .await
                .is_some(),
            "changed after review"
        );
        fs::write(e.repo.join("a.txt"), "v1\n").unwrap();
        git::git(&e.repo, &["add", "-A"]).unwrap();

        git::git(&e.repo, &["commit", "-qm", "v1"]).unwrap();
        git::git(&e.repo, &["tag", "v1"]).unwrap();
        // Unattested local commits (no remote yet: all of them are unpushed) block a push.
        let blocked = decide(&bash("git push", &e.repo), Some(c)).await;
        assert!(
            blocked
                .as_deref()
                .unwrap_or_default()
                .contains("push blocked"),
            "{blocked:?}"
        );
        c.record("t", &rec.id, Some("HEAD"), Some("v1"))
            .await
            .unwrap();
        // The base commit predates the gate: pretend it was pushed.
        git::git(&e.repo, &["update-ref", "refs/remotes/origin/main", &base]).unwrap();
        assert_eq!(
            decide(
                &bash("git push origin HEAD refs/notes/review-gate", &e.repo),
                Some(c)
            )
            .await,
            None
        );
    }
}
