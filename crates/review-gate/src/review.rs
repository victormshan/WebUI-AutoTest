//! One review of the task's current version: stage, build the prompt from a fixed template,
//! ask an external model (chunked for long diffs), parse VERDICT, store the record.
//!
//! The implementer neither writes the prompt nor reports the verdict.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::gate::{Gate, ReviewInput};
use crate::model::{Outcome, ReviewRecord, Status, Task, Verdict};
use crate::reviewer::{self, AskError, Reply, ReviewerConfig};
use crate::{GateError, git};

/// web-gemini relays through a browser text box and times out on long prompts
/// (35KB failed every time, 10KB intermittently): review in chunks of whole files.
pub const CHUNK_CHARS: usize = 6000;
/// Whole-version prompts beyond this are truncated (and say so).
pub const MAX_DIFF_CHARS: usize = 60_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub files: Vec<String>,
    pub diff: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error(transparent)]
    Gate(#[from] GateError),
    #[error(transparent)]
    Ask(#[from] AskError),
    #[error("reviewer reply has no VERDICT line (chunk {0})")]
    NoVerdict(usize),
}

pub fn parse_verdict(text: &str) -> Option<Verdict> {
    let upper = text.to_uppercase();
    let i = upper.find("VERDICT")?;
    let rest = upper[i + "VERDICT".len()..].trim_start_matches([' ', ':', '：', '*']);
    if rest.starts_with("APPROVED") {
        Some(Verdict::Approved)
    } else if rest.starts_with("REJECTED") {
        Some(Verdict::Rejected)
    } else {
        None
    }
}

/// Packs per-file diffs into chunks of at most `limit` chars; oversized files are split by lines.
pub fn chunk_diff(per_file: &[(String, String)], limit: usize) -> Vec<Chunk> {
    let mut pieces: Vec<Chunk> = Vec::new();
    for (file, diff) in per_file {
        if diff.len() <= limit {
            pieces.push(Chunk {
                files: vec![file.clone()],
                diff: diff.clone(),
            });
            continue;
        }
        let mut parts: Vec<String> = Vec::new();
        let mut cur = String::new();
        for line in diff.split_inclusive('\n') {
            if !cur.is_empty() && cur.len() + line.len() > limit {
                parts.push(std::mem::take(&mut cur));
            }
            cur.push_str(line);
        }
        if !cur.is_empty() {
            parts.push(cur);
        }
        let n = parts.len();
        for (i, d) in parts.into_iter().enumerate() {
            pieces.push(Chunk {
                files: vec![format!("{file}（第 {}/{n} 部分）", i + 1)],
                diff: d,
            });
        }
    }
    let mut chunks: Vec<Chunk> = Vec::new();
    for p in pieces {
        match chunks.last_mut() {
            Some(last) if last.diff.len() + p.diff.len() <= limit => {
                last.files.extend(p.files);
                last.diff.push_str(&p.diff);
            }
            _ => chunks.push(p),
        }
    }
    chunks
}

pub struct Staged {
    pub tree: String,
    pub base: String,
    pub stat: String,
    pub per_file: Vec<(String, String)>,
}

pub fn stage(repo: &Path) -> Result<Staged, GateError> {
    let (tree, base) = git::stage_all(repo)?;
    let stat = git::git(repo, &["diff", "--cached", "--stat"])?;
    if stat.is_empty() {
        return Err(GateError::Invalid(
            "nothing staged: this version has no changes to review".into(),
        ));
    }
    let names = git::git(repo, &["diff", "--cached", "--name-only"])?;
    let mut per_file = Vec::new();
    for f in names.lines().filter(|l| !l.is_empty()) {
        per_file.push((
            f.to_string(),
            git::git(repo, &["diff", "--cached", "--", f])? + "\n",
        ));
    }
    Ok(Staged {
        tree,
        base,
        stat,
        per_file,
    })
}

pub fn build_prompt(
    task: &Task,
    previous: &[String],
    evidence: &str,
    stat: &str,
    body: &str,
) -> String {
    let mut p = vec![
        "你是三方协作协议中的【外部审核者】，独立于实施方（另一家厂商的 AI）。你只负责找问题：不重写代码、不打分、不给与问题无关的建议。".to_string(),
        String::new(),
        format!("【任务目标】{}", task.goal),
        format!("【最终验收标准】{}", task.final_acceptance),
        format!(
            "【当前版本】第 {} / {} 版。只审本版改动是否满足其中对应本版的部分，且没有引入回归。",
            task.current_iteration, task.iterations
        ),
    ];
    if !previous.is_empty() {
        p.push(format!(
            "【本版此前的打回意见——确认是否已解决】\n{}",
            previous.join("\n\n")
        ));
    }
    if !evidence.trim().is_empty() {
        p.push(format!(
            "【实施方提供的验证证据（未经你独立验证，请审慎采信）】\n{}",
            evidence.chars().take(6000).collect::<String>()
        ));
    }
    p.push(format!("【改动统计（本版全部文件）】\n{stat}"));
    p.push(body.to_string());
    p.push(String::new());
    p.push("【输出格式（严格）】".into());
    p.push(
        "第一行只能是 `VERDICT: APPROVED` 或 `VERDICT: REJECTED`（存在任一阻断问题即 REJECTED）。"
            .into(),
    );
    p.push("之后逐条列出问题（最多 8 条，每条不超过 2 行），每条标注【阻断】或【非阻断】，写明文件/位置与具体失败场景。没有问题就写\"无\"。".into());
    p.join("\n")
}

/// Rejection texts already recorded for the current version (fed back to the reviewer).
fn previous_rejections(gate: &Gate, task: &Task) -> Vec<String> {
    task.history
        .iter()
        .filter(|h| h.iteration == task.current_iteration && h.outcome == Outcome::Rejected)
        .enumerate()
        .filter_map(|(i, h)| {
            let r = gate.store().load_review(&task.id, &h.review).ok()?;
            Some(format!(
                "第 {} 次打回（{}）：\n{}",
                i + 1,
                r.channel,
                r.text.chars().take(1500).collect::<String>()
            ))
        })
        .collect()
}

/// Reviews the staged changes of the task's current version and stores the record.
/// A chunk whose exact prompt was already answered (an interrupted earlier run) reuses that answer.
pub async fn run(
    gate: &Gate,
    task_id: &str,
    evidence: &str,
    provider: Option<&str>,
    cfg: &ReviewerConfig,
) -> Result<ReviewRecord, ReviewError> {
    let task = gate.store().load_task(task_id)?;
    if task.status != Status::Running {
        return Err(GateError::NotRunning(task.id.clone(), task.status).into());
    }
    let staged = stage(Path::new(&task.repo))?;
    let previous = previous_rejections(gate, &task);
    let full: String = staged.per_file.iter().map(|(_, d)| d.as_str()).collect();
    let bodies: Vec<String> = if full.len() <= CHUNK_CHARS {
        vec![format!("【完整 diff】\n{full}")]
    } else {
        let chunks = chunk_diff(&staged.per_file, CHUNK_CHARS);
        let n = chunks.len();
        chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    "【本段 diff：第 {}/{n} 段，只含 {}】\n只就本段判定 VERDICT；需要其他段才能确认的跨文件问题，标为【非阻断】并说明。\n{}",
                    i + 1,
                    c.files.join("、"),
                    c.diff
                )
            })
            .collect()
    };
    let n = bodies.len();
    let provider = provider.unwrap_or(&task.review_provider).to_string();
    let mut verdict = Verdict::Approved;
    let mut answers = Vec::new();
    let mut hasher = Sha256::new();
    let mut last: Option<Reply> = None;
    for (i, body) in bodies.iter().enumerate() {
        let prompt = build_prompt(&task, &previous, evidence, &staged.stat, body);
        let key = hex(&Sha256::digest(prompt.as_bytes()));
        hasher.update(prompt.as_bytes());
        let header = if n > 1 {
            format!("### 第 {}/{n} 段\n", i + 1)
        } else {
            String::new()
        };
        if let Some(cached) = gate.store().cache_get::<Reply>(&task.id, &key)? {
            if parse_verdict(&cached.answer) == Some(Verdict::Rejected) {
                verdict = Verdict::Rejected;
            }
            answers.push(format!(
                "{header}（复用先前对同一提示的审核）\n{}",
                cached.answer
            ));
            last = Some(cached);
            continue;
        }
        if n > 1 {
            eprintln!("[review-gate] reviewing chunk {}/{n}", i + 1);
        }
        let mut reply = reviewer::ask(cfg, &prompt, &provider).await?;
        if parse_verdict(&reply.answer).is_none() {
            let again = format!("{prompt}\n\n（上一次回复缺少首行 VERDICT。请严格按格式重答。）");
            reply = reviewer::ask(cfg, &again, &reply.provider).await?;
        }
        let v = parse_verdict(&reply.answer).ok_or(ReviewError::NoVerdict(i + 1))?;
        if v == Verdict::Rejected {
            verdict = Verdict::Rejected;
        }
        gate.store().cache_put(&task.id, &key, &reply)?;
        answers.push(format!("{header}{}", reply.answer));
        last = Some(reply);
    }
    let reply = last.expect("at least one chunk");
    let summary = if n > 1 {
        format!(
            "VERDICT: {}（分 {n} 段审核，任一段打回即整版打回）\n\n",
            if verdict == Verdict::Approved {
                "APPROVED"
            } else {
                "REJECTED"
            }
        )
    } else {
        String::new()
    };
    let record = gate.add_review(
        &task,
        ReviewInput {
            tree: staged.tree,
            base: staged.base,
            verdict,
            channel: reply.channel,
            provider: reply.provider,
            model: reply.model,
            family: reply.family,
            prompt_sha: hex(&hasher.finalize()),
            text: summary + &answers.join("\n\n"),
        },
    )?;
    Ok(record)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::NewTask;
    use crate::model::Channel;
    use crate::reviewer::tests::{cfg, mock_bridge};
    use crate::store::Store;
    use std::fs;

    #[test]
    fn verdicts() {
        assert_eq!(
            parse_verdict("VERDICT: APPROVED\n无"),
            Some(Verdict::Approved)
        );
        assert_eq!(
            parse_verdict("**Verdict：rejected**"),
            Some(Verdict::Rejected)
        );
        assert_eq!(parse_verdict("looks fine"), None);
        assert_eq!(parse_verdict("VERDICT: maybe"), None);
    }

    #[test]
    fn chunks_pack_small_files_and_split_big_ones() {
        let big: String = (0..50).map(|i| format!("+line {i}\n")).collect();
        let files = vec![
            ("x".to_string(), "aa\n".to_string()),
            ("y".into(), "bb\n".into()),
            ("z".into(), big),
        ];
        let chunks = chunk_diff(&files, 120);
        assert_eq!(chunks[0].files, vec!["x", "y"]);
        assert!(chunks.len() > 2 && chunks.iter().all(|c| c.diff.len() <= 120));
        assert!(chunks[1].files[0].starts_with("z（第 1/"));
    }

    fn setup(dir: &Path) -> (Gate, std::path::PathBuf) {
        let repo = dir.join("repo");
        fs::create_dir(&repo).unwrap();
        for a in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
        ] {
            git::git(&repo, a).unwrap();
        }
        fs::write(repo.join("a.txt"), "v0\n").unwrap();
        git::git(&repo, &["add", "-A"]).unwrap();
        git::git(&repo, &["commit", "-qm", "base"]).unwrap();
        let gate = Gate::new(Store::open(dir.join("state")).unwrap());
        gate.init(NewTask {
            id: "t".into(),
            goal: "目标".into(),
            final_acceptance: "验收".into(),
            repo: repo.to_string_lossy().into(),
            iterations: 1,
            min_reviewer: Channel::WebGemini,
            review_provider: "auto".into(),
        })
        .unwrap();
        (gate, repo)
    }

    #[tokio::test]
    async fn small_diff_is_one_prompt_from_the_template_bound_to_the_tree() {
        let d = tempfile::tempdir().unwrap();
        let (gate, repo) = setup(d.path());
        fs::write(repo.join("a.txt"), "v1-change\n").unwrap();
        let (url, prompts) = mock_bridge(vec![Ok("VERDICT: APPROVED\n\n无")]).await;
        let r = run(&gate, "t", "tests: ok", None, &cfg(&url))
            .await
            .unwrap();
        assert_eq!(
            (r.verdict, r.channel),
            (Verdict::Approved, Channel::WebGemini)
        );
        assert_eq!(r.tree, git::git(&repo, &["write-tree"]).unwrap());
        let p = prompts.lock().unwrap();
        assert_eq!(p.len(), 1);
        assert!(
            p[0].contains("外部审核者")
                && p[0].contains("目标")
                && p[0].contains("v1-change")
                && p[0].contains("tests: ok")
        );
    }

    #[tokio::test]
    async fn large_diff_is_chunked_and_any_rejection_rejects() {
        let d = tempfile::tempdir().unwrap();
        let (gate, repo) = setup(d.path());
        fs::write(repo.join("a.txt"), "A".repeat(CHUNK_CHARS - 500) + "\n").unwrap();
        fs::write(repo.join("b.txt"), "B".repeat(CHUNK_CHARS - 500) + "\n").unwrap();
        let (url, prompts) = mock_bridge(vec![
            Ok("VERDICT: APPROVED"),
            Ok("VERDICT: REJECTED\n【阻断】b"),
        ])
        .await;
        let r = run(&gate, "t", "", None, &cfg(&url)).await.unwrap();
        assert_eq!(r.verdict, Verdict::Rejected);
        let p = prompts.lock().unwrap();
        assert_eq!(p.len(), 2);
        assert!(p[0].contains("第 1/2 段，只含 a.txt") && !p[0].contains("BBBB"));
        assert!(r.text.starts_with("VERDICT: REJECTED（分 2 段审核"));
    }

    #[tokio::test]
    async fn rerun_reuses_answered_chunks() {
        let d = tempfile::tempdir().unwrap();
        let (gate, repo) = setup(d.path());
        fs::write(repo.join("a.txt"), "A".repeat(CHUNK_CHARS - 500) + "\n").unwrap();
        fs::write(repo.join("b.txt"), "B".repeat(CHUNK_CHARS - 500) + "\n").unwrap();
        let (url, prompts) = mock_bridge(vec![
            Ok("VERDICT: APPROVED"),
            Err("x"),
            Err("x"),
            Err("x"),
            Err("x"),
            Ok("VERDICT: APPROVED"),
        ])
        .await;
        assert!(run(&gate, "t", "", None, &cfg(&url)).await.is_err());
        assert_eq!(prompts.lock().unwrap().len(), 5);
        let r = run(&gate, "t", "", None, &cfg(&url)).await.unwrap();
        assert_eq!(prompts.lock().unwrap().len(), 6, "chunk 1 reused");
        assert!(r.text.contains("复用先前对同一提示的审核"));
        assert_eq!(r.verdict, Verdict::Approved);
    }
}
