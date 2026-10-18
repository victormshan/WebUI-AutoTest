//! The audit input (plans/M2-design.md §4.1): assembled whole from fixed git objects, never
//! trimmed, refused when its final size is over the runner's limit.
//!
//! - **Repository material** (the task book of the baseline and of head, the evidence files,
//!   the diff base..head, each commit's attestation note) is read only by object id, and only
//!   in a throwaway bare repository ([`Isolated`]) that borrows the implementer's objects
//!   read-only through `objects/info/alternates`. That repository has no configuration, no
//!   hooks and no `info/`; git runs with an empty environment, `HOME`/`XDG_CONFIG_HOME` on empty
//!   directories, no system or global config, `core.attributesFile=/dev/null`, and attributes
//!   only from head's committed `.gitattributes` (`--attr-source`). So nothing in the
//!   implementer's work tree, `.git/config`, `.git/info/attributes` or user configuration
//!   changes the bytes. It writes only under its temporary directory, removed afterwards.
//! - **Trusted material** (open conditions, the participant mapping, the bridge revision, the
//!   vendor table, the auditor) comes from the caller and is written as given.
//! - Nothing is converted lossily: a path that is not UTF-8 (shown in hex), a symlink, a gitlink,
//!   a file that is not UTF-8 text or holds NUL, and the patch of a binary change are listed as
//!   **not provided**, with size and sha256 where there are bytes to hash.
//! - Merge commits are assembled like any other; the current commit-review attestation check
//!   does not support them, so such a phase cannot be released (see sources.rs).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use super::answer::FORMAT;
use super::record::sha256_hex;

/// The runner's limit on one input (bytes, after the final serialisation).
pub const MAX_INPUT: usize = 4 * 1024 * 1024;
pub const TEMPLATE: &str = "milestone-input v1";

/// git with nothing of the caller's environment or configuration.
fn clean_git(home: &Path) -> Command {
    let mut c = Command::new("git");
    c.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .args(["-c", "core.attributesFile=/dev/null"]);
    c
}

fn run(mut c: Command, what: &str) -> Result<Vec<u8>> {
    let out = c.output().with_context(|| format!("running git {what}"))?;
    if !out.status.success() {
        bail!(
            "git {what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn text(b: Vec<u8>, what: &str) -> Result<String> {
    String::from_utf8(b).with_context(|| format!("git {what}: output is not UTF-8"))
}

/// What step 0 finds in the implementer's repository: where its objects really are, their
/// format, and the object ids the assembly will use. Nothing else is read there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub objects: PathBuf,
    pub format: String,
    pub ids: Vec<String>,
}

/// Resolves each of `revs` (`<rev>^{commit}`) in `repo` (work tree, linked work tree or bare)
/// and finds its object directory with git itself, never by assuming `.git/objects`.
pub fn locate(repo: &Path, revs: &[&str]) -> Result<Located> {
    let empty = tempfile::tempdir().context("temporary home")?;
    let git = |args: &[&str], what: &str| -> Result<String> {
        let mut c = clean_git(empty.path());
        c.args(["-c", "safe.directory=*"])
            .arg("-C")
            .arg(repo)
            .args(args);
        Ok(text(run(c, what)?, what)?.trim().to_string())
    };
    let objects = PathBuf::from(git(
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
        "rev-parse --git-path objects",
    )?);
    ensure!(
        objects.is_dir(),
        "object directory {} is not a directory",
        objects.display()
    );
    let format = git(
        &["rev-parse", "--show-object-format"],
        "rev-parse --show-object-format",
    )?;
    ensure!(
        format == "sha1" || format == "sha256",
        "object format {format} is not supported"
    );
    let mut ids = Vec::new();
    for r in revs {
        ids.push(git(
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{r}^{{commit}}"),
            ],
            "rev-parse",
        )?);
    }
    Ok(Located {
        objects,
        format,
        ids,
    })
}

/// A throwaway bare repository on the implementer's objects (see the module documentation).
pub struct Isolated {
    dir: tempfile::TempDir,
    attr_source: Option<String>,
}

impl Isolated {
    pub fn new(objects: &Path, format: &str) -> Result<Self> {
        let dir = tempfile::tempdir().context("temporary repository")?;
        std::fs::create_dir(dir.path().join("home"))?;
        let mut c = clean_git(&dir.path().join("home"));
        c.args(["init", "-q", "--bare", "--template="])
            .arg(format!("--object-format={format}"))
            .arg(dir.path().join("repo.git"));
        run(c, "init")?;
        let info = dir.path().join("repo.git/objects/info");
        std::fs::create_dir_all(&info)?;
        let mut alt = objects.as_os_str().as_bytes().to_vec();
        alt.push(b'\n');
        std::fs::write(info.join("alternates"), alt)?;
        Ok(Self {
            dir,
            attr_source: None,
        })
    }

    /// Attributes come only from this commit's tree from now on.
    pub fn attributes_from(&mut self, commit: &str) {
        self.attr_source = Some(commit.to_string());
    }

    fn cmd(&self) -> Command {
        let mut c = clean_git(&self.dir.path().join("home"));
        c.env("GIT_DIR", self.dir.path().join("repo.git"));
        if let Some(a) = &self.attr_source {
            c.arg(format!("--attr-source={a}"));
        }
        c
    }

    pub fn git(&self, args: &[&OsStr], what: &str) -> Result<Vec<u8>> {
        let mut c = self.cmd();
        c.args(args);
        run(c, what)
    }

    fn git_s(&self, args: &[&str], what: &str) -> Result<Vec<u8>> {
        let a: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        self.git(&a, what)
    }

    /// The object type of `id`, checking that it exists.
    pub fn kind(&self, id: &str) -> Result<String> {
        Ok(text(
            self.git_s(&["cat-file", "-t", id], "cat-file -t")?,
            "cat-file -t",
        )?
        .trim()
        .to_string())
    }

    pub fn expect_commit(&self, id: &str) -> Result<()> {
        let k = self.kind(id).with_context(|| format!("object {id}"))?;
        ensure!(k == "commit", "object {id} is a {k}, not a commit");
        Ok(())
    }

    /// The tree entry for `path` in `commit`: (mode, type, id), or None if absent.
    pub fn entry(&self, commit: &str, path: &[u8]) -> Result<Option<(String, String, String)>> {
        let out = self.git(
            &[
                OsStr::new("ls-tree"),
                OsStr::new("-z"),
                OsStr::new(commit),
                OsStr::new("--"),
                OsStr::from_bytes(path),
            ],
            "ls-tree",
        )?;
        let Some(rec) = out.split(|b| *b == 0).find(|r| !r.is_empty()) else {
            return Ok(None);
        };
        let tab = rec
            .iter()
            .position(|b| *b == b'\t')
            .context("ls-tree record")?;
        if &rec[tab + 1..] != path {
            return Ok(None);
        }
        let meta = std::str::from_utf8(&rec[..tab]).context("ls-tree record")?;
        let mut it = meta.split(' ');
        let (m, t, i) = (it.next(), it.next(), it.next());
        match (m, t, i) {
            (Some(m), Some(t), Some(i)) => Ok(Some((m.into(), t.into(), i.into()))),
            _ => bail!("ls-tree record {meta:?}"),
        }
    }

    pub fn blob(&self, id: &str) -> Result<Vec<u8>> {
        self.git_s(&["cat-file", "blob", id], "cat-file blob")
    }
}

/// Fixed object ids and paths of one request (resolved by [`locate`] first).
pub struct Material<'a> {
    pub phase: &'a str,
    pub request_id: &'a str,
    pub base: &'a str,
    pub head: &'a str,
    /// Commit holding the baseline task book (registration or latest `baseline_changed`).
    pub baseline_commit: &'a str,
    pub task_book: &'a str,
    pub task_book_sha256: &'a str,
    pub evidence_dir: &'a str,
    /// Fixed commit of refs/notes/review-gate, if the ref exists.
    pub notes_commit: Option<&'a str>,
}

/// Given by the caller and written as is.
#[derive(Debug, Clone, Serialize)]
pub struct Trusted {
    pub auditor: String,
    /// (id, blocking, text) of every open condition.
    pub open_conditions: Vec<(String, bool, String)>,
    /// The participant mapping (sources.rs `Outcome`, as JSON).
    pub participants: serde_json::Value,
    pub bridge_revision: String,
    pub vendors_version: u32,
    pub vendors_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub name: String,
    pub size: usize,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotProvided {
    pub what: String,
    pub why: String,
    /// Size and sha256 of the bytes left out, where there are bytes.
    pub size: Option<usize>,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub template: String,
    pub items: Vec<Item>,
    pub not_provided: Vec<NotProvided>,
    pub size: usize,
    pub sha256: String,
}

#[derive(Debug)]
pub struct Assembled {
    pub bytes: Vec<u8>,
    pub manifest: Manifest,
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("audit input is {size} bytes, over the limit of {MAX_INPUT}: refused, not trimmed")]
    TooLarge { size: usize },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// How a path is named in the input: as is when UTF-8 without control characters, else hex.
fn path_name(p: &[u8]) -> (String, bool) {
    match std::str::from_utf8(p) {
        Ok(s) if !s.chars().any(char::is_control) => (s.to_string(), true),
        _ => {
            let hex: String = p.iter().map(|b| format!("{b:02x}")).collect();
            (format!("hex:{hex}"), false)
        }
    }
}

/// UTF-8 text without NUL, or None.
fn as_text(b: &[u8]) -> Option<&str> {
    if b.contains(&0) {
        return None;
    }
    std::str::from_utf8(b).ok()
}

struct Out {
    buf: String,
    items: Vec<Item>,
    not_provided: Vec<NotProvided>,
}

impl Out {
    fn line(&mut self, s: &str) {
        self.buf.push_str(s);
        self.buf.push('\n');
    }
    /// A block with an unambiguous start and end, recorded in the manifest.
    fn block(&mut self, name: &str, body: &str) {
        let sha = sha256_hex(body.as_bytes());
        self.line(&format!(
            "<<<BEGIN {name} bytes={} sha256={sha}>>>",
            body.len()
        ));
        self.buf.push_str(body);
        if !body.ends_with('\n') && !body.is_empty() {
            self.buf.push('\n');
        }
        self.line(&format!("<<<END {name}>>>"));
        self.items.push(Item {
            name: name.into(),
            size: body.len(),
            sha256: sha,
        });
    }
    fn missing(&mut self, what: String, why: &str, bytes: Option<&[u8]>) {
        self.not_provided.push(NotProvided {
            what,
            why: why.into(),
            size: bytes.map(<[u8]>::len),
            sha256: bytes.map(sha256_hex),
        });
    }
}

const DIFF_OPTS: &[&str] = &[
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--no-renames",
    "--diff-algorithm=myers",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--no-relative",
    "--full-index",
    "-U3",
];

/// Assembles the input for one request. `g` must be an [`Isolated`] repository on the
/// implementer's objects; every id in `m` was resolved by [`locate`].
pub fn assemble(g: &mut Isolated, m: &Material, t: &Trusted) -> Result<Assembled, InputError> {
    let a = build(g, m, t)?;
    if a.bytes.len() > MAX_INPUT {
        return Err(InputError::TooLarge {
            size: a.bytes.len(),
        });
    }
    Ok(a)
}

fn build(g: &mut Isolated, m: &Material, t: &Trusted) -> Result<Assembled> {
    for id in [m.base, m.head, m.baseline_commit]
        .into_iter()
        .chain(m.notes_commit)
    {
        g.expect_commit(id)?;
    }
    g.attributes_from(m.head);
    let mut o = Out {
        buf: String::new(),
        items: Vec::new(),
        not_provided: Vec::new(),
    };

    // The baseline task book: the acceptance basis. It must be there and match its hash.
    let book = m.task_book.as_bytes();
    let base_entry = g.entry(m.baseline_commit, book)?.with_context(|| {
        format!(
            "baseline task book {} missing in {}",
            m.task_book, m.baseline_commit
        )
    })?;
    if base_entry.1 != "blob" || base_entry.0 == "120000" {
        return Err(anyhow::anyhow!(
            "baseline task book {} is not a regular file",
            m.task_book
        ));
    }
    let base_book = g.blob(&base_entry.2)?;
    let got = sha256_hex(&base_book);
    if got != m.task_book_sha256 {
        return Err(anyhow::anyhow!(
            "baseline task book {} has sha256 {got}, registered {}",
            m.task_book,
            m.task_book_sha256
        ));
    }
    let base_text = as_text(&base_book)
        .with_context(|| format!("baseline task book {} is not UTF-8 text", m.task_book))?
        .to_string();

    o.line(&format!("# {TEMPLATE}"));
    o.line("");
    o.line("## 指令");
    o.line(&format!(
        "- 验收依据是下面的**基准任务书**（{}，sha256 {}，来自提交 {}）。",
        m.task_book, m.task_book_sha256, m.baseline_commit
    ));
    o.line("- \"修订材料\"未经用户认可，只作参考，不能作为验收依据。");
    o.line("- 列在\"未提供\"中的内容没有附上，不能视为已审查。");
    o.line("- 合并提交照常列出；当前的提交审核签名不支持合并提交，含合并提交的阶段不能放行。");
    o.line(
        "- 答复必须严格按以下格式（标题从行首开始；正文的其他地方不要出现这几个标题或条件行）：",
    );
    o.block("format", FORMAT);
    o.line("");
    o.line("## 阶段");
    o.line(&format!("- phase: {}", m.phase));
    o.line(&format!("- request: {}", m.request_id));
    o.line(&format!("- base: {}", m.base));
    o.line(&format!("- head: {}", m.head));
    o.line(&format!("- auditor: {}", t.auditor));
    o.line("");
    o.line("## 基准任务书（验收依据）");
    o.block(&format!("baseline:{}", m.task_book), &base_text);
    o.line("");
    o.line("## 修订材料（未经用户认可，仅作参考）");
    match g.entry(m.head, book)? {
        None => o.line("head 中任务书已删除。"),
        Some((mode, kind, id)) if kind == "blob" && mode != "120000" => {
            let b = g.blob(&id)?;
            if b == base_book {
                o.line("head 中的任务书与基准相同。");
            } else {
                match as_text(&b) {
                    Some(s) => o.block(&format!("head:{}", m.task_book), s),
                    None => {
                        o.line("head 中的任务书不是 UTF-8 文本，未提供。");
                        o.missing(format!("head:{}", m.task_book), "not UTF-8 text", Some(&b));
                    }
                }
            }
        }
        Some((mode, kind, _)) => {
            o.line(&format!(
                "head 中的任务书不是普通文件（{mode} {kind}），未提供。"
            ));
            o.missing(format!("head:{}", m.task_book), "not a regular file", None);
        }
    }
    o.line("");
    o.line("## 未解除条件");
    if t.open_conditions.is_empty() {
        o.line("（无）");
    }
    for (id, blocking, text) in &t.open_conditions {
        let b = if *blocking {
            "blocking"
        } else {
            "non-blocking"
        };
        o.line(&format!("- {id} [{b}]: {}", text.replace('\n', " ")));
    }
    o.line("");
    o.line("## 参与者与独立性（gate 判定）");
    o.block(
        "participants",
        &serde_json::to_string_pretty(&t.participants).context("participants")?,
    );
    o.line(&format!("- bridge revision: {}", t.bridge_revision));
    o.line(&format!(
        "- vendor table: version {}, sha256 {}",
        t.vendors_version, t.vendors_sha256
    ));
    o.line("");

    // Commits and their attestation notes, oldest first.
    o.line("## 提交与提交审核签名");
    let range = format!("{}..{}", m.base, m.head);
    let list = text(
        g.git_s(&["rev-list", "--reverse", "--parents", &range], "rev-list")?,
        "rev-list",
    )?;
    for l in list.lines().filter(|l| !l.is_empty()) {
        let mut ids = l.split(' ');
        let c = ids.next().unwrap_or_default();
        let parents = ids.count();
        o.line(&format!(
            "### {c}{}",
            if parents > 1 {
                "（合并提交）"
            } else {
                ""
            }
        ));
        let note = match m.notes_commit {
            Some(n) => note_in(g, n, c)?,
            None => None,
        };
        match note {
            Some(b) => match as_text(&b) {
                Some(s) => o.block(&format!("attestation:{c}"), s),
                None => o.missing(format!("attestation:{c}"), "not UTF-8 text", Some(&b)),
            },
            None => o.line("提交审核签名：缺失"),
        }
    }
    o.line("");

    // Evidence: committed files under the directory, sorted by raw path bytes.
    o.line(&format!("## 证据文件（{}）", m.evidence_dir));
    let dir = m.evidence_dir.trim_end_matches('/');
    // A relative directory inside the repository, named plainly: never the whole tree by
    // accident, never outside it.
    // An empty name or a leading slash shows up as an empty component.
    let bad = |c: &str| c.is_empty() || c == "." || c == "..";
    if dir.split('/').any(bad) {
        bail!(
            "evidence directory {:?} is not a plain relative path",
            m.evidence_dir
        );
    }
    let listing = g.git(
        &[
            OsStr::new("ls-tree"),
            OsStr::new("-r"),
            OsStr::new("-z"),
            OsStr::new("--full-tree"),
            OsStr::new(m.head),
            OsStr::new("--"),
            OsStr::new(dir),
        ],
        "ls-tree -r",
    )?;
    let mut entries: Vec<(&[u8], String, String, String)> = Vec::new();
    for rec in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = rec
            .iter()
            .position(|b| *b == b'\t')
            .context("ls-tree record")?;
        let meta = std::str::from_utf8(&rec[..tab]).context("ls-tree record")?;
        let f: Vec<&str> = meta.split(' ').collect();
        ensure!(f.len() == 3, "ls-tree record {meta:?}");
        entries.push((&rec[tab + 1..], f[0].into(), f[1].into(), f[2].into()));
    }
    entries.sort_by(|a, b| a.0.cmp(b.0));
    if entries.is_empty() {
        o.line("（目录不存在或为空）");
    }
    for (path, mode, kind, id) in entries {
        let (name, utf8) = path_name(path);
        let label = format!("evidence:{name}");
        if mode == "120000" {
            o.missing(label, "symlink (not followed)", None);
        } else if kind == "commit" {
            o.missing(label, "gitlink (submodule)", None);
        } else if kind != "blob" {
            o.missing(label, &format!("{kind} entry"), None);
        } else {
            let b = g.blob(&id)?;
            match (utf8, as_text(&b)) {
                (true, Some(s)) => o.block(&label, s),
                (false, _) => o.missing(
                    label,
                    "path is not UTF-8 or holds control characters",
                    Some(&b),
                ),
                (true, None) => o.missing(label, "binary (not UTF-8 text or holds NUL)", Some(&b)),
            }
        }
    }
    o.line("");

    // The diff, file by file, so one file that cannot be shown leaves the others whole.
    o.line(&format!("## diff {range}"));
    let names = g.git(
        &[
            OsStr::new("diff"),
            OsStr::new("--name-only"),
            OsStr::new("-z"),
            OsStr::new("--no-renames"),
            OsStr::new("--no-relative"),
            OsStr::new(m.base),
            OsStr::new(m.head),
        ],
        "diff --name-only",
    )?;
    let mut paths: Vec<&[u8]> = names.split(|b| *b == 0).filter(|r| !r.is_empty()).collect();
    paths.sort();
    for p in paths {
        let (name, _) = path_name(p);
        let mut args: Vec<&OsStr> = vec![OsStr::new("diff")];
        args.extend(DIFF_OPTS.iter().map(OsStr::new));
        args.extend([
            OsStr::new(m.base),
            OsStr::new(m.head),
            OsStr::new("--"),
            OsStr::from_bytes(p),
        ]);
        let d = g.git(&args, "diff")?;
        let numstat = g.git(
            &[
                OsStr::new("diff"),
                OsStr::new("--numstat"),
                OsStr::new("-z"),
                OsStr::new("--no-renames"),
                OsStr::new("--no-textconv"),
                OsStr::new("--no-ext-diff"),
                OsStr::new(m.base),
                OsStr::new(m.head),
                OsStr::new("--"),
                OsStr::from_bytes(p),
            ],
            "diff --numstat",
        )?;
        let binary = numstat.starts_with(b"-\t-\t");
        match as_text(&d) {
            Some(s) => {
                o.block(&format!("diff:{name}"), s);
                if binary {
                    o.missing(
                        format!("diff:{name}"),
                        "binary change: only the summary line, no patch",
                        None,
                    );
                }
            }
            None => o.missing(format!("diff:{name}"), "diff is not UTF-8 text", Some(&d)),
        }
    }
    o.line("");
    o.line("## 未提供");
    if o.not_provided.is_empty() {
        o.line("（无）");
    }
    let np: Vec<String> = o
        .not_provided
        .iter()
        .map(|n| {
            let extra = match (&n.size, &n.sha256) {
                (Some(s), Some(h)) => format!("（{s} 字节，sha256 {h}）"),
                _ => String::new(),
            };
            format!("- {}：{}{extra}", n.what, n.why)
        })
        .collect();
    for l in np {
        o.line(&l);
    }

    let bytes = o.buf.into_bytes();
    let manifest = Manifest {
        template: TEMPLATE.into(),
        items: o.items,
        not_provided: o.not_provided,
        size: bytes.len(),
        sha256: sha256_hex(&bytes),
    };
    Ok(Assembled { bytes, manifest })
}

/// The note blob for `commit` in the notes commit `notes`, as raw bytes (any fan-out).
fn note_in(g: &Isolated, notes: &str, commit: &str) -> Result<Option<Vec<u8>>> {
    let names = g.git_s(
        &["ls-tree", "-r", "-z", "--name-only", notes],
        "ls-tree notes",
    )?;
    for n in names.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let flat: Vec<u8> = n.iter().copied().filter(|b| *b != b'/').collect();
        if flat == commit.as_bytes() {
            let mut spec = notes.as_bytes().to_vec();
            spec.push(b':');
            spec.extend_from_slice(n);
            return Ok(Some(g.git(
                &[
                    OsStr::new("cat-file"),
                    OsStr::new("blob"),
                    OsStr::from_bytes(&spec),
                ],
                "cat-file note",
            )?));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// git for setting up fixtures (never the code under test).
    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", "2026-10-18T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-10-18T00:00:00Z")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    }

    fn write(dir: &Path, path: &str, body: &[u8]) {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn commit(dir: &Path, msg: &str) -> String {
        sh(dir, &["add", "-A"]);
        sh(dir, &["commit", "-qm", msg]);
        sh(dir, &["rev-parse", "HEAD"])
    }

    struct Fixture {
        dir: tempfile::TempDir,
        base: String,
        head: String,
    }

    impl Fixture {
        fn root(&self) -> &Path {
            self.dir.path()
        }
    }

    const BOOK: &[u8] = "# Task book\nAccept when X.\n多字节\n".as_bytes();

    /// base: the task book; head: evidence and a changed x.txt (two commits).
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        sh(r, &["init", "-q", "-b", "main"]);
        sh(r, &["config", "user.name", "victormshan"]);
        sh(r, &["config", "user.email", "v@x"]);
        write(r, "plans/book.md", BOOK);
        write(r, "x.txt", b"one\n");
        let base = commit(r, "base");
        write(r, "ev/a.txt", b"evidence a\n");
        write(r, "x.txt", b"one\ntwo\n");
        commit(r, "first");
        write(r, "ev/b.txt", "第二份证据\n".as_bytes());
        let head = commit(r, "second");
        Fixture { dir, base, head }
    }

    fn trusted() -> Trusted {
        Trusted {
            auditor: "auditor".into(),
            open_conditions: vec![("C1".into(), true, "fix it".into())],
            participants: serde_json::json!({"claude": "anthropic"}),
            bridge_revision: "rev".into(),
            vendors_version: 1,
            vendors_sha256: "v".repeat(64),
        }
    }

    fn try_assemble(
        f: &Fixture,
        book_sha: &str,
        notes: Option<&str>,
    ) -> Result<Assembled, InputError> {
        let l = locate(f.root(), &[&f.base, &f.head]).unwrap();
        let mut g = Isolated::new(&l.objects, &l.format).unwrap();
        assemble(
            &mut g,
            &Material {
                phase: "p",
                request_id: "p-1",
                base: &l.ids[0],
                head: &l.ids[1],
                baseline_commit: &l.ids[0],
                task_book: "plans/book.md",
                task_book_sha256: book_sha,
                evidence_dir: "ev",
                notes_commit: notes,
            },
            &trusted(),
        )
    }

    fn assemble_ok(f: &Fixture) -> Assembled {
        try_assemble(f, &sha256_hex(BOOK), None).unwrap()
    }

    fn s(a: &Assembled) -> String {
        String::from_utf8(a.bytes.clone()).unwrap()
    }

    #[test]
    fn assembles_every_section_deterministically() {
        let f = fixture();
        let a = assemble_ok(&f);
        let t = s(&a);
        for want in [
            "# milestone-input v1",
            "验收依据是下面的**基准任务书**",
            "<<<BEGIN format",
            "<<<BEGIN baseline:plans/book.md",
            "head 中的任务书与基准相同。",
            "- C1 [blocking]: fix it",
            "<<<BEGIN participants",
            "bridge revision: rev",
            "提交审核签名：缺失",
            "<<<BEGIN evidence:ev/a.txt",
            "<<<BEGIN evidence:ev/b.txt",
            "第二份证据",
            "<<<BEGIN diff:x.txt",
            "+two",
        ] {
            assert!(t.contains(want), "missing {want:?}");
        }
        assert!(t.find("ev/a.txt").unwrap() < t.find("ev/b.txt").unwrap());
        assert_eq!(a.manifest.size, a.bytes.len());
        assert_eq!(a.manifest.sha256, sha256_hex(&a.bytes));
        assert!(
            a.manifest
                .items
                .iter()
                .any(|i| i.name == "evidence:ev/a.txt" && i.size == 11)
        );
        assert_eq!(assemble_ok(&f).bytes, a.bytes, "same input, same bytes");
    }

    #[test]
    fn the_work_tree_does_not_count() {
        let f = fixture();
        let clean = assemble_ok(&f).bytes;
        write(f.root(), "x.txt", b"uncommitted\n");
        write(f.root(), "ev/c.txt", b"untracked\n");
        write(f.root(), "plans/book.md", b"edited\n");
        sh(f.root(), &["add", "ev/c.txt"]);
        assert_eq!(assemble_ok(&f).bytes, clean);
    }

    /// The five places that could change how git shows x.txt, each set against it.
    fn contaminate(f: &Fixture, which: usize, outside: &Path) {
        let r = f.root();
        match which {
            0 => write(r, ".gitattributes", b"*.txt binary\n"),
            1 => write(r, ".git/info/attributes", b"*.txt binary\n"),
            2 => {
                write(outside, "attrs", b"*.txt -diff\n");
                sh(
                    r,
                    &[
                        "config",
                        "core.attributesFile",
                        outside.join("attrs").to_str().unwrap(),
                    ],
                );
            }
            3 => write(outside, "xdg/git/attributes", b"*.txt binary\n"),
            4 => {
                for (k, v) in [
                    ("diff.external", "/bin/false"),
                    ("diff.noprefix", "true"),
                    ("diff.mnemonicPrefix", "true"),
                    ("color.ui", "always"),
                    ("diff.renames", "copies"),
                    ("diff.algorithm", "patience"),
                    ("diff.context", "10"),
                    ("diff.relative", "true"),
                    ("core.quotePath", "false"),
                    ("diff.txt.textconv", "tr a-z A-Z"),
                ] {
                    sh(r, &["config", k, v]);
                }
                write(r, ".git/info/attributes", b"*.txt diff=txt\n");
            }
            _ => unreachable!(),
        }
    }

    /// Runs the assembly in a child process with `HOME`/`XDG_CONFIG_HOME` pointing at `outside`,
    /// so the caller's own environment is in play; returns the sha256 of the bytes.
    fn assemble_in_child(f: &Fixture, outside: &Path) -> String {
        let exe = std::env::current_exe().unwrap();
        let out = Command::new(exe)
            .args([
                "--exact",
                "milestone::input::tests::child_assembly",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("INPUT_TEST_REPO", f.root())
            .env("INPUT_TEST_BASE", &f.base)
            .env("INPUT_TEST_HEAD", &f.head)
            .env("HOME", outside)
            .env("XDG_CONFIG_HOME", outside.join("xdg"))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let at = text
            .find("INPUT-SHA=")
            .unwrap_or_else(|| panic!("no hash in {text}"));
        text[at + "INPUT-SHA=".len()..][..64].to_string()
    }

    #[test]
    #[ignore = "run by assemble_in_child only"]
    fn child_assembly() {
        let f = Fixture {
            dir: tempfile::tempdir().unwrap(),
            base: std::env::var("INPUT_TEST_BASE").unwrap(),
            head: std::env::var("INPUT_TEST_HEAD").unwrap(),
        };
        let repo = PathBuf::from(std::env::var("INPUT_TEST_REPO").unwrap());
        let l = locate(&repo, &[&f.base, &f.head]).unwrap();
        let mut g = Isolated::new(&l.objects, &l.format).unwrap();
        let a = assemble(
            &mut g,
            &Material {
                phase: "p",
                request_id: "p-1",
                base: &l.ids[0],
                head: &l.ids[1],
                baseline_commit: &l.ids[0],
                task_book: "plans/book.md",
                task_book_sha256: &sha256_hex(BOOK),
                evidence_dir: "ev",
                notes_commit: None,
            },
            &trusted(),
        )
        .unwrap();
        println!("INPUT-SHA={}", sha256_hex(&a.bytes));
    }

    #[test]
    fn no_local_or_user_configuration_changes_the_bytes() {
        let f = fixture();
        let clean_outside = tempfile::tempdir().unwrap();
        let clean = assemble_in_child(&f, clean_outside.path());
        assert_eq!(clean, sha256_hex(&assemble_ok(&f).bytes));
        for which in 0..5 {
            let f = fixture();
            let outside = tempfile::tempdir().unwrap();
            contaminate(&f, which, outside.path());
            assert_eq!(
                assemble_in_child(&f, outside.path()),
                clean,
                "contamination {which}"
            );
        }
        let f = fixture();
        let outside = tempfile::tempdir().unwrap();
        for which in 0..5 {
            contaminate(&f, which, outside.path());
        }
        assert_eq!(assemble_in_child(&f, outside.path()), clean, "all at once");
        // Not vacuous: each contamination does change what plain git shows.
        let plain = |f: &Fixture, outside: &Path| {
            let o = Command::new("git")
                .arg("-C")
                .arg(f.root())
                .args(["diff", &f.base, &f.head, "--", "x.txt"])
                .env("HOME", outside)
                .env("XDG_CONFIG_HOME", outside.join("xdg"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            o.stdout
        };
        let f = fixture();
        let o = tempfile::tempdir().unwrap();
        let before = plain(&f, o.path());
        for which in 0..5 {
            let f = fixture();
            let outside = tempfile::tempdir().unwrap();
            contaminate(&f, which, outside.path());
            assert_ne!(
                plain(&f, outside.path()),
                before,
                "contamination {which} shows in plain git"
            );
        }
    }

    #[test]
    fn committed_attributes_of_head_do_count() {
        let f = fixture();
        let clean = assemble_ok(&f);
        write(f.root(), ".gitattributes", b"*.txt binary\n");
        write(f.root(), "x.txt", b"one\ntwo\nthree\n");
        let head = commit(f.root(), "attrs");
        let f = Fixture { head, ..f };
        let a = assemble_ok(&f);
        assert_ne!(a.bytes, clean.bytes);
        let t = s(&a);
        assert!(t.contains("Binary files"), "the binary summary stays");
        assert!(
            a.manifest
                .not_provided
                .iter()
                .any(|n| n.what == "diff:x.txt" && n.why.contains("binary change"))
        );
    }

    #[test]
    fn task_book_rules() {
        let f = fixture();
        // Wrong registered hash, or no task book at the baseline: refused.
        let e = try_assemble(&f, &"0".repeat(64), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("has sha256"), "{e}");
        // Head revises it: shown apart, as material not approved by the user.
        write(f.root(), "plans/book.md", b"# Task book\nAccept when Y.\n");
        let head = commit(f.root(), "revise");
        let g = Fixture { head, ..f };
        let t = s(&assemble_ok(&g));
        assert!(t.contains("<<<BEGIN head:plans/book.md"));
        assert!(t.contains("Accept when Y."));
        assert!(
            t.contains("Accept when X."),
            "the baseline is still the basis"
        );
        // Head deletes it.
        sh(g.root(), &["rm", "-q", "plans/book.md"]);
        let head = commit(g.root(), "delete");
        let h = Fixture { head, ..g };
        assert!(s(&assemble_ok(&h)).contains("head 中任务书已删除。"));
    }

    #[test]
    fn the_evidence_directory_must_be_a_plain_relative_path() {
        let f = fixture();
        let l = locate(f.root(), &[&f.base, &f.head]).unwrap();
        for dir in ["", "/", "/ev", "./ev", "ev/../plans", "ev//x", ".."] {
            let mut g = Isolated::new(&l.objects, &l.format).unwrap();
            let e = assemble(
                &mut g,
                &Material {
                    phase: "p",
                    request_id: "p-1",
                    base: &l.ids[0],
                    head: &l.ids[1],
                    baseline_commit: &l.ids[0],
                    task_book: "plans/book.md",
                    task_book_sha256: &sha256_hex(BOOK),
                    evidence_dir: dir,
                    notes_commit: None,
                },
                &trusted(),
            )
            .unwrap_err()
            .to_string();
            assert!(e.contains("not a plain relative path"), "{dir:?}: {e}");
        }
        // A trailing slash is fine; nested directories work.
        write(f.root(), "ev/deep/er/c.txt", b"deep\n");
        let head = commit(f.root(), "deep");
        let g = Fixture { head, ..f };
        assert!(s(&assemble_ok(&g)).contains("<<<BEGIN evidence:ev/deep/er/c.txt"));
    }

    #[test]
    fn a_missing_baseline_task_book_is_refused() {
        let f = fixture();
        let l = locate(f.root(), &[&f.base, &f.head]).unwrap();
        let mut g = Isolated::new(&l.objects, &l.format).unwrap();
        let e = assemble(
            &mut g,
            &Material {
                phase: "p",
                request_id: "p-1",
                base: &l.ids[0],
                head: &l.ids[1],
                baseline_commit: &l.ids[0],
                task_book: "plans/none.md",
                task_book_sha256: &sha256_hex(BOOK),
                evidence_dir: "ev",
                notes_commit: None,
            },
            &trusted(),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("missing"), "{e}");
    }

    #[test]
    fn evidence_that_cannot_be_shown_is_listed_not_converted() {
        let f = fixture();
        let r = f.root();
        write(r, "ev/bin.dat", b"a\0b");
        write(r, "ev/latin1.txt", b"caf\xe9\n");
        let odd = std::ffi::OsStr::from_bytes(b"ev/\xff\xfe.txt");
        std::fs::write(r.join(odd), b"odd name\n").unwrap();
        std::os::unix::fs::symlink("a.txt", r.join("ev/link")).unwrap();
        write(r, "ev/tab\there.txt", b"tab in the name\n");
        let other = sh(r, &["rev-parse", "HEAD"]);
        // `commit` runs `add -A`, which would drop the gitlink (no directory for it): add the
        // files first, then the gitlink, then commit what is staged.
        sh(r, &["add", "-A"]);
        sh(
            r,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{other},ev/sub"),
            ],
        );
        sh(r, &["commit", "-qm", "odd evidence"]);
        let head = sh(r, &["rev-parse", "HEAD"]);
        let f = Fixture { head, ..f };
        let a = assemble_ok(&f);
        let np = |what: &str| {
            a.manifest
                .not_provided
                .iter()
                .find(|n| n.what == what)
                .cloned()
        };
        assert!(np("evidence:ev/bin.dat").unwrap().why.contains("binary"));
        assert_eq!(
            np("evidence:ev/bin.dat").unwrap().sha256,
            Some(sha256_hex(b"a\0b"))
        );
        assert!(np("evidence:ev/latin1.txt").unwrap().why.contains("binary"));
        assert!(
            np("evidence:hex:65762fff fe2e747874".replace(' ', "").as_str())
                .unwrap()
                .why
                .contains("not UTF-8")
        );
        assert!(np("evidence:ev/link").unwrap().why.contains("symlink"));
        // A control character in a path is shown in hex too, not as is.
        let tab: String = b"ev/tab\there.txt"
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(
            np(&format!("evidence:hex:{tab}"))
                .unwrap()
                .why
                .contains("not UTF-8")
        );
        assert!(np("evidence:ev/sub").unwrap().why.contains("gitlink"));
        let t = s(&a);
        assert!(!t.contains("caf"), "no lossy text");
        assert!(t.contains("## 未提供"));
    }

    #[test]
    fn merge_commits_and_notes_are_listed() {
        let f = fixture();
        let r = f.root();
        sh(r, &["checkout", "-q", "-b", "side", &f.base]);
        write(r, "side.txt", b"side\n");
        commit(r, "side");
        sh(r, &["checkout", "-q", "main"]);
        sh(r, &["merge", "-q", "--no-edit", "side"]);
        let head = sh(r, &["rev-parse", "HEAD"]);
        sh(
            r,
            &[
                "notes",
                "--ref=review-gate",
                "add",
                "-m",
                "note for head",
                &head,
            ],
        );
        let notes = sh(r, &["rev-parse", "refs/notes/review-gate"]);
        let f = Fixture {
            head: head.clone(),
            ..f
        };
        let a = try_assemble(&f, &sha256_hex(BOOK), Some(&notes)).unwrap();
        let t = s(&a);
        assert!(t.contains(&format!("### {head}（合并提交）")));
        assert!(t.contains(&format!("<<<BEGIN attestation:{head}")));
        assert!(t.contains("note for head"));
        assert!(
            t.contains("提交审核签名：缺失"),
            "the other commits have none"
        );
    }

    /// The input for base..HEAD of `r` (head moves as the test commits).
    fn at_head(r: &Path, base: &str) -> Result<Assembled, InputError> {
        let l = locate(r, &[base, "HEAD"]).unwrap();
        let mut g = Isolated::new(&l.objects, &l.format).unwrap();
        assemble(
            &mut g,
            &Material {
                phase: "p",
                request_id: "p-1",
                base: &l.ids[0],
                head: &l.ids[1],
                baseline_commit: &l.ids[0],
                task_book: "plans/book.md",
                task_book_sha256: &sha256_hex(BOOK),
                evidence_dir: "ev",
                notes_commit: None,
            },
            &trusted(),
        )
    }

    fn size_of(r: &Path, base: &str) -> usize {
        match at_head(r, base) {
            Ok(a) => a.bytes.len(),
            Err(InputError::TooLarge { size }) => size,
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn the_limit_is_on_final_bytes() {
        let f = fixture();
        let r = f.root();
        // Two evidence files: ev/odd.txt is committed in the base (shown once, as evidence),
        // ev/big.txt only at head (shown twice, as evidence and in the diff) and multi-byte
        // ("é" is two bytes). Adjusting both lands the input on exactly MAX_INPUT.
        // Both start non-empty: an empty block gets no closing newline, a non-empty one without a
        // final newline gets one, so a file growing from empty would jump by one byte.
        let (mut odd, mut two, mut ascii) = (10i64, 1i64, 0i64);
        let mut base = String::new();
        for _ in 0..12 {
            let _ = std::fs::remove_file(r.join("ev/big.txt"));
            write(r, "ev/odd.txt", "o".repeat(odd as usize).as_bytes());
            base = commit(r, "base for the limit");
            let mut b = "é".repeat(two as usize).into_bytes();
            b.extend(std::iter::repeat_n(b'a', ascii as usize));
            write(r, "ev/big.txt", &b);
            commit(r, "big");
            let gap = MAX_INPUT as i64 - size_of(r, &base) as i64;
            if gap == 0 {
                break;
            }
            let mut gap = gap;
            if gap % 2 != 0 {
                odd += gap.signum();
                gap -= gap.signum();
            }
            // Each "é" adds 4 bytes in all, each ASCII byte 2.
            let total = (two * 4 + ascii * 2 + gap).max(0);
            two = total / 4;
            ascii = (total % 4) / 2;
            odd = odd.max(1);
            two = two.max(1);
        }
        assert_eq!(size_of(r, &base), MAX_INPUT, "did not converge");
        let a = at_head(r, &base).expect("exactly at the limit is accepted");
        assert_eq!(a.bytes.len(), MAX_INPUT);
        assert!(String::from_utf8(a.bytes).unwrap().contains("é"));
        // One byte more: refused, not trimmed.
        let mut fh = std::fs::OpenOptions::new()
            .append(true)
            .open(r.join("ev/big.txt"))
            .unwrap();
        fh.write_all(b"a").unwrap();
        drop(fh);
        commit(r, "one more");
        match at_head(r, &base) {
            Err(InputError::TooLarge { size }) => assert!(size > MAX_INPUT),
            Ok(a) => panic!("accepted {} bytes", a.bytes.len()),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn objects_are_found_in_worktrees_and_bare_repos_and_ids_are_typed() {
        let f = fixture();
        let wt = f.root().join("wt");
        sh(
            f.root(),
            &["worktree", "add", "-q", wt.to_str().unwrap(), &f.head],
        );
        let a = locate(&wt, &["HEAD"]).unwrap();
        assert_eq!(a.ids, vec![f.head.clone()]);
        assert!(a.objects.ends_with("objects") && a.objects.is_dir());
        let bare = tempfile::tempdir().unwrap();
        sh(
            bare.path(),
            &["clone", "-q", "--bare", f.root().to_str().unwrap(), "b.git"],
        );
        let b = locate(&bare.path().join("b.git"), &["HEAD"]).unwrap();
        assert_eq!(b.ids, vec![f.head.clone()]);
        // A tree id passed as a commit is refused.
        let g = Isolated::new(&a.objects, &a.format).unwrap();
        let tree = sh(f.root(), &["rev-parse", "HEAD^{tree}"]);
        assert!(
            g.expect_commit(&tree)
                .unwrap_err()
                .to_string()
                .contains("not a commit")
        );
        assert!(g.expect_commit(&"1".repeat(40)).is_err());
        // assemble itself checks every id it is given.
        let mut g = Isolated::new(&a.objects, &a.format).unwrap();
        let e = assemble(
            &mut g,
            &Material {
                phase: "p",
                request_id: "p-1",
                base: &f.base,
                head: &tree,
                baseline_commit: &f.base,
                task_book: "plans/book.md",
                task_book_sha256: &sha256_hex(BOOK),
                evidence_dir: "ev",
                notes_commit: None,
            },
            &trusted(),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("not a commit"), "{e}");
    }

    #[test]
    fn sha256_repositories_work_and_formats_must_match() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        sh(r, &["init", "-q", "--object-format=sha256", "-b", "main"]);
        sh(r, &["config", "user.name", "u"]);
        sh(r, &["config", "user.email", "u@x"]);
        write(r, "a", b"a\n");
        let c = commit(r, "a");
        let l = locate(r, &["HEAD"]).unwrap();
        assert_eq!((l.format.as_str(), l.ids[0].len()), ("sha256", 64));
        assert_eq!(c.len(), 64);
        let g = Isolated::new(&l.objects, &l.format).unwrap();
        g.expect_commit(&l.ids[0]).unwrap();
        // The same objects through a sha1 repository are not readable.
        let wrong = Isolated::new(&l.objects, "sha1").unwrap();
        assert!(wrong.expect_commit(&l.ids[0]).is_err());
    }
}
