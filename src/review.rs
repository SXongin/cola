//! Repository review diff (spec #803, ticket #805): the read-only git data
//! producer behind `/diff`.
//!
//! The module answers one question — "what does this working tree change
//! relative to the fork point with a base branch?" — and returns both the
//! generic [`DiffDocument`] (#798) and the metadata the command needs to name
//! the artifact and phrase an empty result (ticket #804). Nothing here writes
//! to the repository: no fetch, no `git add`, no index mutation.
//!
//! Two caps are deliberately separate. This producer bounds the **raw patch**
//! bytes (2 MiB per file, 8 MiB in total) as a read/memory guarantee and says
//! so through [`DiffDocument::truncated`]; [`render_diff_html`] owns the final
//! **HTML** 8 MiB bound and appends its own note. A raw 8 MiB patch never
//! renders to under 8 MiB of HTML, so the two guarantees cannot be one number.
//!
//! [`render_diff_html`]: crate::backend::render_diff_html

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::backend::{DiffDocument, DiffFile};

/// The base refs tried, in order, when the caller names none.
const DEFAULT_BASES: [&str; 2] = ["origin/main", "main"];

/// Git's empty tree object id: the comparison base of an unborn HEAD, where no
/// commit exists to fork from.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The most raw patch bytes one file may contribute (2 MiB). A larger file is
/// represented by one note line instead of its content.
const MAX_FILE_PATCH_BYTES: usize = 2 * 1024 * 1024;

/// The most raw patch bytes the whole document may carry (8 MiB). Files are
/// admitted in order; the rest are omitted and reported through
/// [`DiffDocument::truncated`].
const MAX_TOTAL_PATCH_BYTES: usize = 8 * 1024 * 1024;

/// One git invocation's wall-clock bound.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// stdout bound for commands whose output is metadata, never a patch.
const SMALL_STDOUT_BYTES: usize = 64 * 1024;

/// stdout bound for `--numstat -z` and `ls-files -z`: large repositories fit,
/// a runaway listing does not.
const LISTING_STDOUT_BYTES: usize = 4 * 1024 * 1024;

/// Every call's stderr bound: enough for git's diagnostic, never unbounded.
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// stdout bound for the patch stream: the total budget plus one whole per-file
/// budget, so every file that can be admitted is read completely before the
/// stream is cut.
const PATCH_STDOUT_BYTES: usize = MAX_TOTAL_PATCH_BYTES + MAX_FILE_PATCH_BYTES;

/// A file bigger than [`MAX_FILE_PATCH_BYTES`] is summarised by this one line.
const OVERSIZE_NOTE: &str = "文件超过 2 MB，未包含改动内容";

/// A non-regular, non-symlink untracked entry (FIFO, socket, device) is never
/// opened; it is summarised by this one line.
const IRREGULAR_NOTE: &str = "不是常规文件，未包含内容";

/// The distinct, readable failures of a repository review. Every one of them is
/// a real error: the producer never returns an empty document pretending the
/// working tree had no changes.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReviewError {
    /// `dir` is not inside a git work tree.
    #[error("不是 git 仓库：{0}")]
    NotARepository(String),

    /// Neither the explicit base nor any default ref exists.
    #[error("找不到基准分支：{0}")]
    BaseNotFound(String),

    /// The base and HEAD share no history (or git refused the computation).
    #[error("无法计算基准 {base} 与 HEAD 的 merge-base：{detail}")]
    MergeBaseFailed { base: String, detail: String },

    /// The `git` binary is not on PATH.
    #[error("未找到 git 可执行文件，请确认 git 已安装并在 PATH 中")]
    GitNotFound,

    /// The OS refused access to the repository or one of its files.
    #[error("git 无权限访问：{0}")]
    PermissionDenied(String),

    /// A git invocation other than the two above failed.
    #[error("git 执行失败：{0}")]
    GitFailed(String),

    /// A git invocation exceeded [`GIT_TIMEOUT`].
    #[error("git 执行超时（{} 秒）", GIT_TIMEOUT.as_secs())]
    Timeout,

    /// Reading an untracked file failed.
    #[error("读取文件失败：{0}")]
    Io(String),
}

/// The repository review's data: the generic document (#798) plus the metadata
/// #804 needs. The consumer reads these fields, never
/// [`DiffDocument::subtitle`].
#[derive(Debug, Clone)]
pub(crate) struct ReviewDiff {
    /// The renderable document: files, counts and the raw-patch truncation note.
    pub document: DiffDocument,
    /// The resolved base ref: the explicit `--base`, else `origin/main`, else
    /// `main` (or `unborn` for a repository with no HEAD and no base ref).
    pub base: String,
    /// The commit `git merge-base <base> HEAD` resolved to — the empty tree for
    /// an unborn HEAD.
    pub merge_base: String,
    /// The current branch, `detached`, or `unborn` when HEAD does not exist.
    pub branch: String,
    /// The abbreviated HEAD commit, or `unborn`.
    pub short_sha: String,
}

/// Produce the review diff of `dir` against `base` (else `origin/main`, else
/// `main`). Read-only and bounded: every git call has a timeout and capped
/// stdout/stderr, and untracked files are read only inside the repository root.
///
/// Covers committed + uncommitted tracked changes through
/// `git diff <merge-base>` and lists untracked files separately, synthesising
/// each into a new-file patch. Renames keep their `rename from`/`rename to`
/// headers with the new path; binaries and submodules are one note line.
pub(crate) async fn review_diff(dir: &str, base: Option<&str>) -> Result<ReviewDiff, ReviewError> {
    let root = repo_root(dir).await?;
    let head = read_head(&root).await?;
    let (base_ref, base_sha) = resolve_base(&root, base, head.sha.is_some()).await?;
    let merge_base = match &head.sha {
        Some(_) => merge_base(&root, &base_ref, base_sha.as_deref()).await?,
        None => EMPTY_TREE.to_string(),
    };

    let patch_args = [
        "-c",
        "core.quotepath=false",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-M",
        "-U3",
        merge_base.as_str(),
    ];
    let patch_out = run_git(&root, &patch_args, PATCH_STDOUT_BYTES).await?;
    // A cut stream is killed by this module, so its non-zero status is the
    // expected outcome of the cap — not a git failure.
    if !patch_out.success && !patch_out.stdout_truncated {
        require_success(&patch_out, "diff")?;
    }

    // `-U3` (like `-p`) requests patch output, so it must NOT ride along with
    // `--numstat`: git would emit the numstat rows AND the 8 MB patch. The
    // counts need only the same rename detection and deterministic config.
    let numstat_args = [
        "-c",
        "core.quotepath=false",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-M",
        "--numstat",
        "-z",
        merge_base.as_str(),
    ];
    let numstat_out = run_git(&root, &numstat_args, LISTING_STDOUT_BYTES).await?;
    require_success(&numstat_out, "diff --numstat")?;
    if numstat_out.stdout_truncated {
        return Err(ReviewError::GitFailed(
            "git diff --numstat 输出超出上限".to_string(),
        ));
    }

    let patch_text = String::from_utf8_lossy(&patch_out.stdout).into_owned();
    let sections = split_sections(&patch_text);
    // A cut patch stream ends inside the last section: its bytes are a partial
    // hunk and must never reach the document. The file is counted as omitted.
    let complete_sections = if patch_out.stdout_truncated {
        sections.len().saturating_sub(1)
    } else {
        sections.len()
    };
    let entries = parse_numstat(&String::from_utf8_lossy(&numstat_out.stdout));
    let untracked = list_untracked(&root).await?;

    let (files, truncated) =
        assemble_files(&root, &entries, &sections, complete_sections, &untracked).await?;

    let base_display = match &base_sha {
        Some(sha) => format!("{base_ref} @ {}", short_sha(sha)),
        None => format!("{base_ref} · HEAD unborn"),
    };
    let document = DiffDocument {
        title: "仓库改动".to_string(),
        subtitle: format!("{base_display} · merge-base {}", short_sha(&merge_base)),
        files,
        truncated,
    };

    Ok(ReviewDiff {
        document,
        base: base_ref,
        merge_base,
        branch: head.branch,
        short_sha: head.short_sha,
    })
}

/// Build the document's files in source order under the two raw-patch caps.
/// `complete_sections` is how many patch sections were read whole; entries past
/// it (and everything after the first file that does not fit the total budget)
/// are omitted and counted.
async fn assemble_files(
    root: &str,
    entries: &[NumStatEntry],
    sections: &[&str],
    complete_sections: usize,
    untracked: &[String],
) -> Result<(Vec<DiffFile>, Option<String>), ReviewError> {
    let mut files = Vec::new();
    let mut used = 0usize;
    let mut omitted = 0usize;
    let mut stopped = false;
    let mut stopped_by_cap = false;
    let mut stopped_by_stream = complete_sections < sections.len();

    for (index, entry) in entries.iter().enumerate() {
        if stopped || index >= complete_sections {
            stopped = true;
            stopped_by_stream = stopped_by_stream || index >= complete_sections;
            omitted += 1;
            continue;
        }
        let file = tracked_file(entry, sections[index]);
        if used.saturating_add(file.patch.len()) > MAX_TOTAL_PATCH_BYTES {
            stopped = true;
            stopped_by_cap = true;
            omitted += 1;
            continue;
        }
        used += file.patch.len();
        files.push(file);
    }

    // Patch sections without a numstat row would be dropped without a trace;
    // report that as an incomplete read instead of a clean document.
    if !stopped && sections.len() > entries.len() {
        stopped = true;
        stopped_by_stream = true;
        omitted += sections.len() - entries.len();
    }

    let root_path = Path::new(root);
    for path in untracked {
        if stopped {
            omitted += 1;
            continue;
        }
        let file = untracked_file(root_path, path).await?;
        if used.saturating_add(file.patch.len()) > MAX_TOTAL_PATCH_BYTES {
            stopped = true;
            stopped_by_cap = true;
            omitted += 1;
            continue;
        }
        used += file.patch.len();
        files.push(file);
    }

    let truncated = (omitted > 0).then(|| {
        let reason = if stopped_by_cap || stopped_by_stream {
            "原始 patch 超过 8 MB 上限"
        } else {
            "git 输出不完整"
        };
        format!("已省略 {omitted} 个文件（{reason}）")
    });
    Ok((files, truncated))
}

/// One entry of `git diff --numstat -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NumStatEntry {
    additions: usize,
    deletions: usize,
    /// The destination path; for a rename this is the new path.
    path: String,
    renamed: bool,
    binary: bool,
}

/// Parse `git diff --numstat -z`. The NUL-separated rename shape is
/// `add\tdelete\t\0old\0new\0` — the path field between the counts and the
/// first NUL is EMPTY, so a parser that assumes a non-empty path there
/// misaligns the whole stream. A binary file reports `-` for both counts.
fn parse_numstat(raw: &str) -> Vec<NumStatEntry> {
    let mut entries = Vec::new();
    let mut rest = raw;
    while !rest.is_empty() {
        let Some(end) = rest.find('\0') else {
            break;
        };
        let head = &rest[..end];
        rest = &rest[end + 1..];

        let mut fields = head.splitn(3, '\t');
        let additions = fields.next().unwrap_or_default();
        let deletions = fields.next().unwrap_or_default();
        let path = fields.next().unwrap_or_default();
        let binary = additions == "-" || deletions == "-";
        let additions = additions.parse().unwrap_or(0);
        let deletions = deletions.parse().unwrap_or(0);

        if path.is_empty() {
            // Rename: two NUL-terminated paths follow the empty path field.
            let Some(old_end) = rest.find('\0') else {
                break;
            };
            // The old path is consumed for alignment only; the raw patch
            // section already carries `rename from`.
            rest = &rest[old_end + 1..];
            let Some(new_end) = rest.find('\0') else {
                break;
            };
            let new = &rest[..new_end];
            rest = &rest[new_end + 1..];
            entries.push(NumStatEntry {
                additions,
                deletions,
                path: new.to_string(),
                renamed: true,
                binary,
            });
        } else {
            entries.push(NumStatEntry {
                additions,
                deletions,
                path: path.to_string(),
                renamed: false,
                binary,
            });
        }
    }
    entries
}

/// Split a raw `git diff` stream into per-file sections at column-0
/// `diff --git ` lines. A hunk line always carries its `+`/`-`/space prefix, so
/// file content can never be mistaken for a section header.
fn split_sections(patch: &str) -> Vec<&str> {
    let mut starts = Vec::new();
    let mut offset = 0usize;
    for line in patch.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(patch.len());
            &patch[*start..end]
        })
        .collect()
}

/// Build one tracked file's [`DiffFile`] from its numstat entry and raw patch
/// section. Submodules and binaries collapse to one line; everything else keeps
/// the section verbatim (renames therefore keep their `rename from`/`rename to`
/// headers), subject to the per-file cap.
fn tracked_file(entry: &NumStatEntry, section: &str) -> DiffFile {
    let mut status = section_status(section);
    if entry.renamed {
        status = "renamed";
    }
    let patch = if let Some((old, new)) = submodule_pair(section) {
        format!("子模块 {} {old} → {new}", entry.path)
    } else if entry.binary {
        binary_note(section, &entry.path)
    } else if section.len() > MAX_FILE_PATCH_BYTES {
        OVERSIZE_NOTE.to_string()
    } else {
        section.to_string()
    };
    DiffFile {
        path: entry.path.clone(),
        patch,
        additions: entry.additions,
        deletions: entry.deletions,
        status: status.to_string(),
    }
}

/// The status a raw diff section states: `added` / `deleted` / `renamed` are
/// explicit mode/rename headers; everything else is a modification.
fn section_status(section: &str) -> &'static str {
    for line in section.lines() {
        if line.starts_with("new file mode ") {
            return "added";
        }
        if line.starts_with("deleted file mode ") {
            return "deleted";
        }
        if line.starts_with("rename from ") || line.starts_with("rename to ") {
            return "renamed";
        }
    }
    "modified"
}

/// A submodule's old and new gitlink commits, or None. Detection covers a
/// modified gitlink (`index <old>..<new> 160000`), an added one
/// (`new file mode 160000` + a `Subproject commit` line) and a deleted one.
fn submodule_pair(section: &str) -> Option<(String, String)> {
    let mut old_commit = None;
    let mut new_commit = None;
    let mut mode = false;
    let mut index = None;
    for line in section.lines() {
        if line.starts_with("new file mode 160000") || line.starts_with("deleted file mode 160000") {
            mode = true;
        } else if let Some(rest) = line.strip_prefix("-Subproject commit ") {
            mode = true;
            old_commit = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("+Subproject commit ") {
            mode = true;
            new_commit = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("index ") {
            let mut parts = rest.split_whitespace();
            let range = parts.next();
            if parts.next() == Some("160000") {
                mode = true;
            }
            // An added/deleted gitlink's `index` line carries no mode suffix
            // (`index 0000000..abc`), so the mode header above is what proves
            // this is a submodule; keep the range either way.
            if mode && let Some((old, new)) = range.and_then(|range| range.split_once("..")) {
                index = Some((old.to_string(), new.to_string()));
            }
        }
    }
    if !mode {
        return None;
    }
    let (index_old, index_new) = index.unwrap_or_else(|| (String::new(), String::new()));
    let old = old_commit.unwrap_or(index_old);
    let new = new_commit.unwrap_or(index_new);
    (!old.is_empty() && !new.is_empty()).then_some((old, new))
}

/// The one-line binary marker, taken from the raw section when git wrote one.
fn binary_note(section: &str, path: &str) -> String {
    section
        .lines()
        .find(|line| line.starts_with("Binary files "))
        .map(str::to_string)
        .unwrap_or_else(|| format!("Binary files a/{path} and b/{path} differ"))
}

/// Build an untracked file's new-file patch. Symlinks are summarised by their
/// target (never followed), non-regular entries are never opened, and a file
/// over the per-file cap keeps one note line.
async fn untracked_file(root: &Path, path: &str) -> Result<DiffFile, ReviewError> {
    let full = root.join(path);
    let meta = tokio::fs::symlink_metadata(&full).await.map_err(map_io_error)?;
    if meta.file_type().is_symlink() {
        let target = tokio::fs::read_link(&full).await.map_err(map_io_error)?;
        let text = target.to_string_lossy();
        let (patch, additions) = new_file_patch(path, &text);
        return Ok(DiffFile {
            path: path.to_string(),
            patch,
            additions,
            deletions: 0,
            status: "added".to_string(),
        });
    }
    if !meta.file_type().is_file() {
        return Ok(note_file(path, IRREGULAR_NOTE));
    }
    if meta.len() > MAX_FILE_PATCH_BYTES as u64 {
        return Ok(note_file(path, OVERSIZE_NOTE));
    }

    let bytes = read_file_capped(&full).await?;
    if bytes.len() > MAX_FILE_PATCH_BYTES {
        return Ok(note_file(path, OVERSIZE_NOTE));
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(binary_added(path));
    };
    if text.as_bytes().iter().take(8000).any(|byte| *byte == 0) {
        return Ok(binary_added(path));
    }
    let (patch, additions) = new_file_patch(path, &text);
    Ok(DiffFile {
        path: path.to_string(),
        patch,
        additions,
        deletions: 0,
        status: "added".to_string(),
    })
}

fn note_file(path: &str, note: &str) -> DiffFile {
    DiffFile {
        path: path.to_string(),
        patch: note.to_string(),
        additions: 0,
        deletions: 0,
        status: "added".to_string(),
    }
}

fn binary_added(path: &str) -> DiffFile {
    note_file(path, &format!("Binary files /dev/null and b/{path} differ"))
}

/// A new file's unified patch: the headers the renderer drops, one hunk and one
/// `+` line per content line. Returns the patch and its added-line count.
fn new_file_patch(path: &str, text: &str) -> (String, usize) {
    let mut patch = format!("Index: {path}\n--- /dev/null\n+++ b/{path}\n");
    if text.is_empty() {
        return (patch, 0);
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    let ends_with_newline = text.ends_with('\n');
    if ends_with_newline {
        lines.pop();
    }
    let count = lines.len();
    patch.push_str(&format!("@@ -0,0 +1,{count} @@\n"));
    for line in &lines {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    if !ends_with_newline {
        patch.push_str("\\ No newline at end of file\n");
    }
    (patch, count)
}

async fn read_file_capped(path: &Path) -> Result<Vec<u8>, ReviewError> {
    let file = tokio::fs::File::open(path).await.map_err(map_io_error)?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_PATCH_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(map_io_error)?;
    Ok(bytes)
}

/// The untracked files git would show, sorted as git lists them.
async fn list_untracked(root: &str) -> Result<Vec<String>, ReviewError> {
    let out = run_git(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        LISTING_STDOUT_BYTES,
    )
    .await?;
    require_success(&out, "ls-files")?;
    if out.stdout_truncated {
        return Err(ReviewError::GitFailed("未跟踪文件列表超出上限".to_string()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

struct HeadState {
    branch: String,
    short_sha: String,
    sha: Option<String>,
}

async fn read_head(dir: &str) -> Result<HeadState, ReviewError> {
    let head = run_git(
        dir,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        SMALL_STDOUT_BYTES,
    )
    .await?;
    let sha = head
        .success
        .then(|| first_line(&head.stdout))
        .filter(|sha| !sha.is_empty());
    let Some(sha) = sha else {
        return Ok(HeadState {
            branch: "unborn".to_string(),
            short_sha: "unborn".to_string(),
            sha: None,
        });
    };
    let symbolic = run_git(
        dir,
        &["symbolic-ref", "--short", "-q", "HEAD"],
        SMALL_STDOUT_BYTES,
    )
    .await?;
    let branch = if symbolic.success {
        let name = first_line(&symbolic.stdout);
        if name.is_empty() {
            "detached".to_string()
        } else {
            name
        }
    } else {
        "detached".to_string()
    };
    Ok(HeadState {
        branch,
        short_sha: short_sha(&sha),
        sha: Some(sha),
    })
}

/// Resolve the base ref: explicit `--base`, else `origin/main`, else `main`.
/// A repository with no HEAD at all has no refs to find either; its comparison
/// base is the empty tree and the label is `unborn` (an explicit `--base` still
/// has to resolve, so a typo is never silently ignored).
async fn resolve_base(
    dir: &str,
    explicit: Option<&str>,
    has_head: bool,
) -> Result<(String, Option<String>), ReviewError> {
    if let Some(base) = explicit {
        return match resolve_commit(dir, base).await? {
            Some(sha) => Ok((base.to_string(), Some(sha))),
            None => Err(ReviewError::BaseNotFound(base.to_string())),
        };
    }
    for base in DEFAULT_BASES {
        if let Some(sha) = resolve_commit(dir, base).await? {
            return Ok((base.to_string(), Some(sha)));
        }
    }
    if !has_head {
        return Ok(("unborn".to_string(), None));
    }
    Err(ReviewError::BaseNotFound(format!(
        "{} 或 {}",
        DEFAULT_BASES[0], DEFAULT_BASES[1]
    )))
}

async fn resolve_commit(dir: &str, rev: &str) -> Result<Option<String>, ReviewError> {
    let spec = format!("{rev}^{{commit}}");
    let out = run_git(
        dir,
        &["rev-parse", "--verify", "--quiet", &spec],
        SMALL_STDOUT_BYTES,
    )
    .await?;
    if out.stdout_truncated {
        return Err(ReviewError::GitFailed("git rev-parse 输出超出上限".to_string()));
    }
    if !out.success {
        return Ok(None);
    }
    let sha = first_line(&out.stdout);
    Ok((!sha.is_empty()).then_some(sha))
}

async fn merge_base(dir: &str, base: &str, base_sha: Option<&str>) -> Result<String, ReviewError> {
    let out = run_git(
        dir,
        &["merge-base", base_sha.unwrap_or(base), "HEAD"],
        SMALL_STDOUT_BYTES,
    )
    .await?;
    if !out.success {
        return Err(ReviewError::MergeBaseFailed {
            base: base.to_string(),
            detail: out.stderr,
        });
    }
    let merge_base = first_line(&out.stdout);
    if merge_base.is_empty() {
        return Err(ReviewError::MergeBaseFailed {
            base: base.to_string(),
            detail: "git merge-base 没有输出".to_string(),
        });
    }
    Ok(merge_base)
}

/// The repository root, so untracked paths are opened against the top-level
/// even when the caller's directory is a subdirectory. Also the first check
/// that `dir` is a work tree, which is why a non-git directory has its own
/// error.
async fn repo_root(dir: &str) -> Result<String, ReviewError> {
    let out = run_git(dir, &["rev-parse", "--show-toplevel"], SMALL_STDOUT_BYTES).await?;
    if out.success {
        let root = first_line(&out.stdout);
        if !root.is_empty() {
            return Ok(root);
        }
    }
    Err(classify_failure(dir, &out))
}

fn classify_failure(dir: &str, out: &GitOutput) -> ReviewError {
    let lower = out.stderr.to_ascii_lowercase();
    if lower.contains("not a git repository") || out.stderr.is_empty() {
        ReviewError::NotARepository(dir.to_string())
    } else if lower.contains("permission denied") {
        ReviewError::PermissionDenied(out.stderr.clone())
    } else {
        ReviewError::GitFailed(out.stderr.clone())
    }
}

fn require_success(out: &GitOutput, what: &str) -> Result<(), ReviewError> {
    if out.success {
        return Ok(());
    }
    if out.stderr.to_ascii_lowercase().contains("permission denied") {
        return Err(ReviewError::PermissionDenied(out.stderr.clone()));
    }
    let detail = if out.stderr.is_empty() {
        format!("`git {what}` 退出码非零")
    } else {
        out.stderr.clone()
    };
    Err(ReviewError::GitFailed(detail))
}

fn first_line(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// One bounded git invocation's captured result.
struct GitOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: String,
    /// stdout hit [`run_git`]'s cap; the bytes are a prefix, not the output.
    stdout_truncated: bool,
}

/// Run `git -C dir <args>` with a timeout and capped stdout/stderr. stdout is
/// read to `stdout_cap`; when it is cut, the child is killed so a writer blocked
/// on a full pipe cannot hold the call. stderr is always drained (only the
/// first [`MAX_STDERR_BYTES`] are kept), so neither pipe can deadlock the other.
async fn run_git(dir: &str, args: &[&str], stdout_cap: usize) -> Result<GitOutput, ReviewError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().map_err(map_spawn_error)?;
    match tokio::time::timeout(GIT_TIMEOUT, finish_child(child, stdout_cap)).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(map_io_error(error)),
        Err(_) => Err(ReviewError::Timeout),
    }
}

async fn finish_child(mut child: Child, stdout_cap: usize) -> std::io::Result<GitOutput> {
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_buf = vec![0u8; 64 * 1024];
    let mut stderr_buf = vec![0u8; 8 * 1024];
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stdout_truncated = false;

    while !stdout_done || !stderr_done {
        tokio::select! {
            read = stdout.read(&mut stdout_buf), if !stdout_done => {
                let read = read?;
                if read == 0 {
                    stdout_done = true;
                } else {
                    let room = stdout_cap.saturating_sub(stdout_bytes.len());
                    if read > room {
                        stdout_bytes.extend_from_slice(&stdout_buf[..room]);
                        stdout_truncated = true;
                        stdout_done = true;
                    } else {
                        stdout_bytes.extend_from_slice(&stdout_buf[..read]);
                    }
                }
            }
            read = stderr.read(&mut stderr_buf), if !stderr_done => {
                let read = read?;
                if read == 0 {
                    stderr_done = true;
                } else {
                    let room = MAX_STDERR_BYTES.saturating_sub(stderr_bytes.len());
                    stderr_bytes.extend_from_slice(&stderr_buf[..read.min(room)]);
                }
            }
        }
        if stdout_truncated {
            // The writer may be blocked on a full stdout pipe; the stream is
            // already over budget, so stop it and let stderr reach EOF.
            let _ = child.kill().await;
        }
    }

    let status = child.wait().await?;
    Ok(GitOutput {
        success: status.success(),
        stdout: stdout_bytes,
        stderr: String::from_utf8_lossy(&stderr_bytes).trim().to_string(),
        stdout_truncated,
    })
}

/// A spawn failure: `NotFound` here means the `git` binary itself is missing.
fn map_spawn_error(error: std::io::Error) -> ReviewError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ReviewError::GitNotFound,
        std::io::ErrorKind::PermissionDenied => ReviewError::PermissionDenied(error.to_string()),
        _ => ReviewError::Io(error.to_string()),
    }
}

/// A pipe or filesystem failure while a command runs (or an untracked file is
/// read): `NotFound` is a vanished file, not a missing git binary.
fn map_io_error(error: std::io::Error) -> ReviewError {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => ReviewError::PermissionDenied(error.to_string()),
        _ => ReviewError::Io(error.to_string()),
    }
}
