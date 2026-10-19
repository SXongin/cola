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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    /// A hermetic local repository: no remote is contacted, no global config
    /// (signing, line endings) is allowed to change the output.
    struct Repo {
        dir: TempDir,
    }

    impl Repo {
        fn init() -> Self {
            let repo = Repo {
                dir: tempfile::tempdir().unwrap(),
            };
            repo.git(&["init", "-q", "-b", "main"]);
            repo.git(&["config", "user.email", "review@example.com"]);
            repo.git(&["config", "user.name", "review"]);
            repo.git(&["config", "commit.gpgsign", "false"]);
            repo.git(&["config", "core.autocrlf", "false"]);
            repo
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn path_str(&self) -> String {
            self.dir.path().to_string_lossy().into_owned()
        }

        fn git(&self, args: &[&str]) -> String {
            let out = StdCommand::new("git")
                .arg("-C")
                .arg(self.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim_end().to_string()
        }

        fn write(&self, name: &str, content: impl AsRef<[u8]>) {
            let path = self.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }

        fn commit(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
        }
    }

    fn find<'a>(review: &'a ReviewDiff, path: &str) -> &'a DiffFile {
        review
            .document
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| {
                panic!(
                    "missing {path:?}; present: {:?}",
                    review
                        .document
                        .files
                        .iter()
                        .map(|file| &file.path)
                        .collect::<Vec<_>>()
                )
            })
    }

    #[tokio::test]
    async fn non_git_directory_is_a_distinct_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = review_diff(dir.path().to_str().unwrap(), None).await.unwrap_err();
        assert!(
            matches!(err, ReviewError::NotARepository(_)),
            "expected NotARepository, got {err:?}"
        );
        assert!(err.to_string().contains("不是 git 仓库"));
    }

    #[tokio::test]
    async fn missing_base_is_a_distinct_error() {
        let repo = Repo::init();
        // A branch that is neither origin/main nor main.
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/work"]);
        repo.write("a.txt", "a\n");
        repo.commit("base");

        let err = review_diff(&repo.path_str(), None).await.unwrap_err();
        assert!(
            matches!(err, ReviewError::BaseNotFound(_)),
            "expected BaseNotFound, got {err:?}"
        );
        assert!(err.to_string().contains("origin/main"));

        let err = review_diff(&repo.path_str(), Some("nope")).await.unwrap_err();
        assert!(matches!(err, ReviewError::BaseNotFound(_)));
        assert!(err.to_string().contains("nope"));
    }

    #[tokio::test]
    async fn clean_tree_at_the_base_is_an_empty_document() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("base");

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(review.base, "main");
        assert_eq!(review.branch, "main");
        assert_eq!(review.short_sha.len(), 7);
        assert_eq!(review.merge_base, repo.git(&["rev-parse", "HEAD"]));
        assert!(review.document.files.is_empty());
        assert!(review.document.truncated.is_none());
    }

    /// Acceptance: committed + unstaged + untracked changes all appear; the
    /// tracked counts are `git diff --numstat`'s, and git does not count
    /// untracked files (they are separate additions). The index is untouched.
    #[tokio::test]
    async fn mixed_committed_unstaged_and_untracked_changes() {
        let repo = Repo::init();
        repo.write("committed.txt", "a\nb\nc\n");
        repo.write("tracked.txt", "one\n");
        repo.write("gone.txt", "bye\n");
        repo.commit("base");

        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("committed.txt", "a\nB\nc\nd\n");
        repo.commit("committed change");
        repo.git(&["rm", "-q", "gone.txt"]);
        repo.write("tracked.txt", "one\nunstaged\n");
        repo.write("dir/untracked.txt", "hello\nworld\n");

        let index_before = repo.git(&["ls-files", "-s", "-z"]);
        let review = review_diff(&repo.path_str(), None).await.unwrap();
        let index_after = repo.git(&["ls-files", "-s", "-z"]);
        assert_eq!(
            index_before, index_after,
            "review_diff must never touch the index"
        );

        assert_eq!(review.base, "main");
        assert_eq!(review.branch, "feature");
        assert_eq!(review.document.files.len(), 4);

        let committed = find(&review, "committed.txt");
        assert_eq!(committed.status, "modified");
        assert_eq!((committed.additions, committed.deletions), (2, 1));
        assert!(committed.patch.contains("+B"));

        let gone = find(&review, "gone.txt");
        assert_eq!(gone.status, "deleted");
        assert_eq!((gone.additions, gone.deletions), (0, 1));
        assert!(gone.patch.contains("-bye"));

        let tracked = find(&review, "tracked.txt");
        assert_eq!(tracked.status, "modified");
        assert_eq!((tracked.additions, tracked.deletions), (1, 0));

        let untracked = find(&review, "dir/untracked.txt");
        assert_eq!(untracked.status, "added");
        assert_eq!((untracked.additions, untracked.deletions), (2, 0));
        assert!(untracked.patch.contains("@@ -0,0 +1,2 @@"));
        assert!(untracked.patch.contains("+hello"));

        let numstat = repo.git(&["diff", "--numstat", "main"]);
        assert!(numstat.contains("2\t1\tcommitted.txt"));
        assert!(numstat.contains("0\t1\tgone.txt"));
        assert!(numstat.contains("1\t0\ttracked.txt"));
        assert!(
            !numstat.contains("untracked.txt"),
            "git never counts untracked files in --numstat"
        );
    }

    #[tokio::test]
    async fn detached_head_still_compares_against_the_merge_base() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("a.txt", "a\ncommitted\n");
        repo.commit("feature change");
        repo.git(&["checkout", "-q", "--detach"]);
        repo.write("a.txt", "a\ncommitted\nunstaged\n");

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(review.branch, "detached");
        assert_eq!(review.short_sha.len(), 7);
        assert_eq!(review.base, "main");
        let file = find(&review, "a.txt");
        assert_eq!((file.additions, file.deletions), (2, 0));
    }

    /// Acceptance: a brand-new repository (no HEAD at all) treats every tracked
    /// file as an addition against the empty tree.
    #[tokio::test]
    async fn unborn_head_uses_the_empty_tree() {
        let repo = Repo::init();
        repo.write("a.txt", "hi\n");
        repo.write("d/b.txt", "there\n");
        repo.git(&["add", "-A"]);

        let review = review_diff(&repo.path_str(), None).await.unwrap();
        assert_eq!(review.branch, "unborn");
        assert_eq!(review.short_sha, "unborn");
        assert_eq!(review.base, "unborn");
        assert_eq!(review.merge_base, EMPTY_TREE);
        assert!(review.document.subtitle.contains("HEAD unborn"));
        assert_eq!(review.document.files.len(), 2);

        let a = find(&review, "a.txt");
        assert_eq!(a.status, "added");
        assert_eq!((a.additions, a.deletions), (1, 0));
        assert!(a.patch.contains("+hi"));
        assert_eq!(find(&review, "d/b.txt").status, "added");
    }

    /// An orphan checkout keeps an unborn HEAD while a real base ref exists, so
    /// the empty-tree fallback and normal base resolution coexist.
    #[tokio::test]
    async fn unborn_head_with_a_base_ref_still_uses_the_empty_tree() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("base");
        let base_sha = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&["update-ref", "refs/remotes/origin/main", &base_sha]);
        repo.git(&["checkout", "-q", "--orphan", "new"]);
        repo.write("b.txt", "b\n");
        repo.git(&["add", "-A"]);

        let review = review_diff(&repo.path_str(), None).await.unwrap();
        assert_eq!(review.base, "origin/main");
        assert_eq!(review.branch, "unborn");
        assert_eq!(review.merge_base, EMPTY_TREE);
        assert_eq!(review.document.files.len(), 2);
        assert_eq!(find(&review, "a.txt").status, "added");
        assert_eq!(find(&review, "b.txt").status, "added");
    }

    /// Acceptance: a rename is one `renamed` file at its new path, not a
    /// delete + add, and its patch keeps the rename headers.
    #[tokio::test]
    async fn rename_is_one_renamed_file() {
        let repo = Repo::init();
        repo.write("old.txt", "one\ntwo\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.git(&["mv", "old.txt", "new.txt"]);

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(review.document.files.len(), 1);
        let file = &review.document.files[0];
        assert_eq!(file.path, "new.txt");
        assert_eq!(file.status, "renamed");
        assert_eq!((file.additions, file.deletions), (0, 0));
        assert!(file.patch.contains("rename from old.txt"));
        assert!(file.patch.contains("rename to new.txt"));
    }

    /// Acceptance: a binary file keeps its status but no payload — one
    /// `Binary files … differ` line.
    #[tokio::test]
    async fn binary_file_has_no_payload() {
        let repo = Repo::init();
        repo.write("bin.dat", b"BIN\0SECRETBYTES\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("bin.dat", b"BIN\0CHANGED\n");

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        let file = &review.document.files[0];
        assert_eq!(file.path, "bin.dat");
        assert_eq!(file.status, "modified");
        assert_eq!(file.patch.lines().count(), 1);
        assert!(file.patch.starts_with("Binary files "));
        assert!(!file.patch.contains("SECRETBYTES"));
        assert!(!file.patch.contains('\0'));
    }

    /// Acceptance: a submodule is one `子模块 <path> <old> → <new>` line.
    #[tokio::test]
    async fn submodule_is_one_note_line() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("one");
        let first = repo.git(&["rev-parse", "HEAD"]);
        repo.write("b.txt", "b\n");
        repo.commit("two");
        repo.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{first},sub"),
        ]);
        std::fs::create_dir(repo.path().join("sub")).unwrap();
        repo.commit("add sub");
        let main_sub = repo.git(&["rev-parse", "HEAD"]);

        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.git(&["update-index", "--cacheinfo", &format!("160000,{main_sub},sub")]);

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(review.document.files.len(), 1);
        let file = &review.document.files[0];
        assert_eq!(file.path, "sub");
        assert_eq!(file.status, "modified");
        assert_eq!(file.patch, format!("子模块 sub {first} → {main_sub}"));
        assert!(!file.patch.contains("Subproject commit"));
        assert!(!file.patch.contains('\0'));
    }

    /// Acceptance: the raw patch is capped at 8 MB, files are kept in order and
    /// the rest are omitted with `truncated` set.
    #[tokio::test]
    async fn total_cap_omits_files_in_order_and_sets_truncated() {
        let repo = Repo::init();
        let big = "x".repeat(1_500_000);
        for index in 0..6 {
            repo.write(&format!("big{index}.txt"), format!("{big}\n"));
        }
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        for index in 0..6 {
            repo.write(&format!("big{index}.txt"), format!("changed-{index}\n"));
        }

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        let truncated = review
            .document
            .truncated
            .as_deref()
            .expect("the total cap must set truncated");
        assert!(truncated.contains("8 MB"));
        assert!(
            review.document.files.len() < 6,
            "files past the cap must be omitted"
        );
        let total: usize = review.document.files.iter().map(|file| file.patch.len()).sum();
        assert!(total <= MAX_TOTAL_PATCH_BYTES);
        for (index, file) in review.document.files.iter().enumerate() {
            assert_eq!(file.path, format!("big{index}.txt"), "order is preserved");
        }
    }

    /// A single file over the per-file cap keeps its numstat counts but becomes
    /// one note line — never 2 MB of content.
    #[tokio::test]
    async fn single_file_over_the_per_file_cap_is_a_note() {
        let repo = Repo::init();
        repo.write("huge.txt", format!("{}\n", "x".repeat(2_500_000)));
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("huge.txt", "small\n");

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        let file = find(&review, "huge.txt");
        assert_eq!(file.patch, OVERSIZE_NOTE);
        assert!(file.patch.len() < MAX_FILE_PATCH_BYTES);
        assert_eq!((file.additions, file.deletions), (1, 1));
        assert_eq!(file.status, "modified");
    }

    /// Untracked text becomes a new-file patch; a binary or an oversized
    /// untracked file is one note line with no payload.
    #[tokio::test]
    async fn untracked_text_binary_and_oversize_shapes() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("base");
        repo.write("untracked.txt", "hello\nworld\n");
        repo.write("nolf.txt", "no newline");
        repo.write("binary.dat", b"BIN\0SECRET\n");
        repo.write("huge.txt", format!("{}\n", "y".repeat(2_500_000)));

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(review.document.files.len(), 4);

        let text = find(&review, "untracked.txt");
        assert_eq!(text.status, "added");
        assert_eq!((text.additions, text.deletions), (2, 0));
        assert!(text.patch.contains("@@ -0,0 +1,2 @@"));
        assert!(text.patch.contains("+hello"));

        let nolf = find(&review, "nolf.txt");
        assert_eq!((nolf.additions, nolf.deletions), (1, 0));
        assert!(nolf.patch.contains("+no newline"));
        assert!(nolf.patch.contains("\\ No newline at end of file"));

        let binary = find(&review, "binary.dat");
        assert_eq!(binary.status, "added");
        assert_eq!(binary.patch, "Binary files /dev/null and b/binary.dat differ");
        assert!(!binary.patch.contains("SECRET"));

        let huge = find(&review, "huge.txt");
        assert_eq!(huge.patch, OVERSIZE_NOTE);
    }

    /// base resolution prefers `origin/main` over `main`, and an explicit base
    /// over both. A remote URL pointing at a closed port proves no fetch is
    /// attempted: any network call would fail (or hang) instead of succeeding.
    #[tokio::test]
    async fn base_chain_prefers_origin_main_and_never_fetches() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("base");
        let base_commit = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&["remote", "add", "origin", "http://127.0.0.1:1/nope.git"]);
        repo.git(&["update-ref", "refs/remotes/origin/main", &base_commit]);

        repo.write("b.txt", "b\n");
        repo.commit("second");

        let review = review_diff(&repo.path_str(), None).await.unwrap();
        assert_eq!(review.base, "origin/main");
        assert_eq!(review.merge_base, base_commit);
        assert_eq!(review.document.files.len(), 1);
        assert_eq!(review.document.files[0].path, "b.txt");
        assert_eq!(review.document.files[0].status, "added");

        let explicit = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(explicit.base, "main");
        assert!(explicit.document.files.is_empty());
    }

    /// An unrelated history has no merge base: that is its own readable error,
    /// not an empty diff.
    #[tokio::test]
    async fn merge_base_failure_is_a_distinct_error() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("one");
        repo.git(&["checkout", "-q", "--orphan", "other"]);
        repo.write("b.txt", "b\n");
        repo.commit("unrelated root");

        let err = review_diff(&repo.path_str(), None).await.unwrap_err();
        assert!(
            matches!(err, ReviewError::MergeBaseFailed { .. }),
            "expected MergeBaseFailed, got {err:?}"
        );
        assert!(err.to_string().contains("merge-base"));
    }

    /// Acceptance: every patch shape this producer emits renders through #798
    /// without panicking, with no binary payload and the right path/status.
    #[tokio::test]
    async fn every_patch_shape_renders_through_diff_html() {
        let repo = Repo::init();
        repo.write("old.txt", "one\ntwo\n");
        repo.write("bin.dat", b"BIN\0SECRETBYTES\n");
        repo.commit("base");
        let first = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{first},sub"),
        ]);
        std::fs::create_dir(repo.path().join("sub")).unwrap();
        repo.commit("add sub");
        let second = repo.git(&["rev-parse", "HEAD"]);

        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.git(&["mv", "old.txt", "new.txt"]);
        repo.write("bin.dat", b"BIN\0CHANGED\n");
        repo.write("untracked.txt", "hello\n");
        repo.git(&["update-index", "--cacheinfo", &format!("160000,{second},sub")]);

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(review.document.files.len(), 4);
        assert_eq!(find(&review, "new.txt").status, "renamed");
        assert!(find(&review, "new.txt").patch.contains("rename from old.txt"));
        assert_eq!(find(&review, "bin.dat").status, "modified");
        assert!(find(&review, "bin.dat").patch.starts_with("Binary files "));
        assert_eq!(find(&review, "sub").status, "modified");
        assert!(find(&review, "sub").patch.starts_with("子模块 sub "));
        assert_eq!(find(&review, "untracked.txt").status, "added");

        let html = crate::backend::render_diff_html(&review.document);
        assert_eq!(html.matches("<details").count(), 4);
        for path in ["new.txt", "bin.dat", "sub", "untracked.txt"] {
            assert!(html.contains(path), "rendered HTML is missing {path}");
        }
        assert!(html.contains("rename from old.txt"));
        assert!(html.contains("Binary files "));
        assert!(html.contains("子模块 sub "));
        assert!(html.contains("hello"));
        assert!(
            !html.contains("SECRETBYTES"),
            "binary payload must never reach the artifact"
        );
        assert!(!html.contains('\0'));
    }

    /// The patch stream itself is capped: past it the child is killed rather
    /// than buffered, and the document still reports the omission instead of
    /// hanging on a blocked pipe.
    #[tokio::test]
    async fn a_patch_stream_past_the_read_cap_is_cut() {
        let repo = Repo::init();
        let big = "x".repeat(1_500_000);
        for index in 0..8 {
            repo.write(&format!("big{index}.txt"), format!("{big}\n"));
        }
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        for index in 0..8 {
            repo.write(&format!("big{index}.txt"), format!("changed-{index}\n"));
        }

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert!(review.document.truncated.is_some());
        assert!(review.document.files.len() <= 5);
        let total: usize = review.document.files.iter().map(|file| file.patch.len()).sum();
        assert!(total <= MAX_TOTAL_PATCH_BYTES);
    }

    /// Added/deleted gitlinks have no `160000` suffix on their `index` line;
    /// the mode header still identifies them, so all three shapes collapse.
    #[test]
    fn submodule_sections_cover_add_modify_delete() {
        let modify = "diff --git a/sub b/sub\nindex aaa..bbb 160000\n--- a/sub\n+++ b/sub\n\
                      @@ -1 +1 @@\n-Subproject commit aaa\n+Subproject commit bbb\n";
        assert_eq!(
            submodule_pair(modify),
            Some(("aaa".to_string(), "bbb".to_string()))
        );

        let added = "diff --git a/sub b/sub\nnew file mode 160000\nindex 0000000..bbb\n\
                     --- /dev/null\n+++ b/sub\n@@ -0,0 +1 @@\n+Subproject commit bbb\n";
        assert_eq!(
            submodule_pair(added),
            Some(("0000000".to_string(), "bbb".to_string()))
        );

        let deleted = "diff --git a/sub b/sub\ndeleted file mode 160000\nindex aaa..0000000\n\
                       --- a/sub\n+++ /dev/null\n@@ -1 +0,0 @@\n-Subproject commit aaa\n";
        assert_eq!(
            submodule_pair(deleted),
            Some(("aaa".to_string(), "0000000".to_string()))
        );

        let ordinary = "diff --git a/a.txt b/a.txt\nindex aaa..bbb 100644\n--- a/a.txt\n+++ b/a.txt\n\
                        @@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(submodule_pair(ordinary), None);
    }

    #[test]
    fn numstat_z_aligns_the_rename_shape() {
        let raw = "1\t2\ta.txt\x00-\t-\tb.bin\x000\t0\t\x00old.txt\x00new.txt\x00";
        let entries = parse_numstat(raw);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            NumStatEntry {
                additions: 1,
                deletions: 2,
                path: "a.txt".to_string(),
                renamed: false,
                binary: false,
            }
        );
        assert!(entries[1].binary);
        assert_eq!(entries[1].path, "b.bin");
        assert!(entries[2].renamed);
        assert_eq!(entries[2].path, "new.txt");
    }

    #[test]
    fn sections_split_only_at_column_zero_headers() {
        let patch = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n\
                     +diff --git a/fake b/fake\n diff --git a/fake2 b/fake2\n\
                     diff --git a/b b/b\n@@ -1 +1 @@\n+x\n";
        let sections = split_sections(patch);
        assert_eq!(sections.len(), 2);
        assert!(sections[0].contains("+diff --git a/fake"));
        assert!(sections[1].starts_with("diff --git a/b b/b"));
    }

    /// git missing / permission / other I/O each map to their own readable
    /// error — the runner never collapses them into "no changes".
    #[test]
    fn io_failures_keep_their_identity() {
        assert!(matches!(
            map_spawn_error(std::io::Error::from(std::io::ErrorKind::NotFound)),
            ReviewError::GitNotFound
        ));
        assert!(matches!(
            map_spawn_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            ReviewError::PermissionDenied(_)
        ));
        // A file that vanished while reading is I/O, not a missing git binary.
        assert!(matches!(
            map_io_error(std::io::Error::from(std::io::ErrorKind::NotFound)),
            ReviewError::Io(_)
        ));
        assert!(matches!(
            map_io_error(std::io::Error::other("boom")),
            ReviewError::Io(_)
        ));
    }
}
