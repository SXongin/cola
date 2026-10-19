//! Repository review diff (spec #803, ticket #805): the read-only git data
//! producer behind `/diff`.
//!
//! The module answers one question — "what does this working tree change
//! relative to the fork point with a base branch?" — and returns both the
//! generic [`DiffDocument`] (#798) and the metadata the command needs to name
//! the artifact and phrase an empty result (ticket #804). Nothing here stages
//! or changes the repository: no fetch, no `git add`/`git add -N`, no staged
//! entry and no tracked content is modified. `git diff` may refresh the index's
//! stat cache, which is not a content change.
//!
//! Two caps are deliberately separate. This producer bounds the **raw patch**
//! bytes (2 MiB for an untracked file's synthesized patch, 8 MiB in total) as a
//! read/memory guarantee and says so through [`DiffDocument::truncated`];
//! [`render_diff_html`] owns the final **HTML** 8 MiB bound and appends its own
//! note. A raw 8 MiB patch never renders to under 8 MiB of HTML, so the two
//! guarantees cannot be one number.
//!
//! [`render_diff_html`]: crate::backend::render_diff_html

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::backend::{DiffDocument, DiffFile};

/// The base refs tried, in order, when the caller names none.
const DEFAULT_BASES: [&str; 2] = ["origin/main", "main"];

/// The deterministic diff configuration shared by the patch and `--numstat`
/// calls: user config must not rewrite the output, and renames are detected.
/// `--ignore-submodules=none` overrides a user's `diff.ignoreSubmodules=all`,
/// which would otherwise omit changed submodules from both calls. The explicit
/// `a/`/`b/` prefixes override `diff.mnemonicPrefix` and `diff.noprefix`, so
/// section paths stay in the form [`section_path`] strips and still pair with
/// `--numstat`. The patch call adds `-U3`; the numstat call must not (it would
/// also request patch output).
const DIFF_FLAGS: [&str; 10] = [
    "-c",
    "core.quotepath=false",
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--ignore-submodules=none",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "-M",
];

/// Git's empty tree object id: the comparison base of an unborn HEAD, where no
/// commit exists to fork from.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The most bytes one untracked file's synthesized patch may contribute
/// (2 MiB); a larger untracked file is one note line instead of its content.
/// Tracked patches are bounded only by the total raw cap below, as the ticket
/// specifies.
const MAX_FILE_PATCH_BYTES: usize = 2 * 1024 * 1024;

/// The most raw patch bytes the whole document may carry (8 MiB). Files are
/// admitted in order; the rest are omitted and reported through
/// [`DiffDocument::truncated`].
const MAX_TOTAL_PATCH_BYTES: usize = 8 * 1024 * 1024;

/// One git invocation's wall-clock bound.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// One untracked file's inspection+read bound. A stalled network filesystem
/// must not keep the review pending; the entry degrades to
/// [`UNREADABLE_NOTE`].
const UNTRACKED_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The whole untracked phase's bound. The per-entry bound alone does not bound
/// the total: a large listing on a slow mount would otherwise wait an entry's
/// timeout at a time. Once this expires the remaining entries are omitted as a
/// group instead of being read one by one.
const UNTRACKED_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

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

/// An untracked entry that vanished or could not be read between `git ls-files`
/// and the read (a build removed it, an editor replaced it, its path is not
/// UTF-8) is summarised by this one line; it must not sink the whole review.
const UNREADABLE_NOTE: &str = "无法读取该文件，未包含内容";

/// The distinct, readable failures of a repository review. Every one of them is
/// a real error: the producer never returns an empty document pretending the
/// working tree had no changes. A per-entry untracked file's stat/read failure
/// is deliberately not one of them: it becomes a note line for that path so one
/// bad entry cannot lose the review.
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

    /// The OS refused access on a required git call (a repository or ref read,
    /// or a git invocation). A per-entry untracked file refusal is a note
    /// instead, never this error.
    #[error("git 无权限访问：{0}")]
    PermissionDenied(String),

    /// A git invocation other than the two above failed.
    #[error("git 执行失败：{0}")]
    GitFailed(String),

    /// A git invocation exceeded [`GIT_TIMEOUT`].
    #[error("git 执行超时（{} 秒）", GIT_TIMEOUT.as_secs())]
    Timeout,

    /// A pipe or other I/O failure outside the per-entry untracked path (those
    /// degrade to a note line instead of escaping).
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
    // Anchor the untracked phase to the root as it is NOW, before the listing:
    // a later rename/replacement of the root pathname must not redirect any
    // untracked read. Tracked git calls keep using the root string.
    let scope = capture_untracked_scope(&root).await?;
    let head = read_head(&root).await?;
    let (base_ref, base_sha) = resolve_base(&root, base, head.sha.is_some()).await?;
    let merge_base = match &head.sha {
        Some(_) => merge_base(&root, &base_ref, base_sha.as_deref()).await?,
        None => EMPTY_TREE.to_string(),
    };

    let mut patch_args = DIFF_FLAGS.to_vec();
    patch_args.push("-U3");
    patch_args.push(merge_base.as_str());
    let patch_out = run_git(&root, &patch_args, PATCH_STDOUT_BYTES).await?;
    // A cut stream is killed by this module, so its non-zero status is the
    // expected outcome of the cap — not a git failure.
    if !patch_out.success && !patch_out.stdout_truncated {
        require_success(&patch_out, "diff")?;
    }

    // `-U3` (like `-p`) requests patch output, so it must NOT ride along with
    // `--numstat`: git would emit the numstat rows AND the whole patch.
    let mut numstat_args = DIFF_FLAGS.to_vec();
    numstat_args.push("--numstat");
    numstat_args.push("-z");
    numstat_args.push(merge_base.as_str());
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
        assemble_files(&scope, &entries, &sections, complete_sections, &untracked).await?;

    let base_display = match &base_sha {
        Some(sha) => format!("{base_ref} @ {}", short_sha(sha)),
        None => format!("{base_ref} · HEAD unborn"),
    };
    let mut subtitle = format!("{base_display} · merge-base {}", short_sha(&merge_base));
    if !untracked.is_empty() {
        // git's `--stat` never counts untracked files, so the header numbers
        // (which do include them) need this marker. It keys off the listing:
        // the entries are in the review scope even if one is later omitted by
        // the raw cap or becomes a note.
        subtitle.push_str(" · 含未跟踪文件");
    }
    let document = DiffDocument {
        title: "仓库改动".to_string(),
        subtitle,
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
/// Patch sections are paired with numstat entries by **path**, never by index:
/// the two invocations can see different trees if a file is renamed between
/// them, and one file's patch labelled with another's counts is worse than an
/// honest omission. `complete_sections` is how many sections were read whole;
/// a cut tail is inserted as an unusable match so its entry, if any, is counted
/// once as omitted.
async fn assemble_files(
    scope: &Arc<UntrackedScope>,
    entries: &[NumStatEntry],
    sections: &[&str],
    complete_sections: usize,
    untracked: &[String],
) -> Result<(Vec<DiffFile>, Option<String>), ReviewError> {
    assemble_files_with_timeout(
        scope,
        entries,
        sections,
        complete_sections,
        untracked,
        UNTRACKED_TOTAL_TIMEOUT,
    )
    .await
}

/// [`assemble_files`] with an injectable total untracked budget, so the
/// deadline is testable without waiting 30 s.
async fn assemble_files_with_timeout(
    scope: &Arc<UntrackedScope>,
    entries: &[NumStatEntry],
    sections: &[&str],
    complete_sections: usize,
    untracked: &[String],
    untracked_budget: Duration,
) -> Result<(Vec<DiffFile>, Option<String>), ReviewError> {
    let mut files = Vec::new();
    let mut used = 0usize;
    let mut omitted = 0usize;
    let mut stopped = false;
    let mut stopped_by_cap = false;
    let mut stopped_by_time = false;
    // A cut tail is itself evidence of an incomplete read, even when the total
    // cap stops the entry loop before that tail's entry is reached.
    let mut stopped_by_stream = complete_sections < sections.len();

    // `Some(section)` is usable; `None` marks a section cut by the stream cap.
    let mut by_path: HashMap<String, Option<&str>> = HashMap::new();
    for (index, section) in sections.iter().enumerate() {
        let whole = index < complete_sections;
        match section_path(section) {
            Some(path) => {
                let value = whole.then_some(*section);
                if by_path.insert(path, value).is_some() {
                    // The same path twice cannot be paired unambiguously.
                    stopped_by_stream = true;
                    omitted += 1;
                }
            }
            None => {
                // A section whose path cannot be read is never guessed at.
                stopped_by_stream = true;
                omitted += 1;
            }
        }
    }

    for entry in entries {
        let section = by_path.remove(entry.path.as_str());
        if stopped {
            omitted += 1;
            continue;
        }
        let Some(Some(section)) = section else {
            // No usable section for this entry: an incomplete read, never a
            // different file's patch.
            stopped_by_stream = true;
            omitted += 1;
            continue;
        };
        let file = tracked_file(entry, section);
        if used.saturating_add(file.patch.len()) > MAX_TOTAL_PATCH_BYTES {
            stopped = true;
            stopped_by_cap = true;
            omitted += 1;
            continue;
        }
        used += file.patch.len();
        files.push(file);
    }

    // Sections no numstat entry claimed are likewise incomplete.
    if !by_path.is_empty() {
        stopped_by_stream = true;
        omitted += by_path.len();
    }

    // ONE total deadline for the whole untracked phase: per-entry timeouts
    // alone would let a large listing on a slow mount wait indefinitely. Once
    // it expires, the remaining entries are omitted as a group, never noted
    // one by one.
    let untracked_deadline = tokio::time::Instant::now() + untracked_budget;
    for (index, path) in untracked.iter().enumerate() {
        if stopped {
            omitted += 1;
            continue;
        }
        let remaining = untracked_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            stopped_by_time = true;
            omitted += untracked.len() - index;
            break;
        }
        let budget = remaining.min(UNTRACKED_READ_TIMEOUT);
        let file = match untracked_file(scope, path, budget).await {
            // One entry that vanished or cannot be read is a note for that
            // path; it must never abort the whole review.
            Ok(file) => file,
            Err(_) => note_file(path, UNREADABLE_NOTE),
        };
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
        format!(
            "已省略 {omitted} 个文件（{}）",
            omission_reason(stopped_by_cap, stopped_by_stream, stopped_by_time)
        )
    });
    Ok((files, truncated))
}

/// The readable cause of an omitted-file note. The byte cap is cola's bound;
/// an incomplete stream is git's output ending early (or a section without a
/// numstat row); the untracked total deadline is the phase's time bound. They
/// are distinct causes, so the note must name the one that actually stopped the
/// loop — the time bound wins when it fired.
fn omission_reason(stopped_by_cap: bool, stopped_by_stream: bool, stopped_by_time: bool) -> String {
    if stopped_by_time {
        return format!(
            "未跟踪文件的读取超过 {} 秒总时限",
            UNTRACKED_TOTAL_TIMEOUT.as_secs()
        );
    }
    match (stopped_by_cap, stopped_by_stream) {
        (true, true) => "git 输出不完整或超出 8 MB 上限".to_string(),
        (true, false) => "原始 patch 超过 8 MB 上限".to_string(),
        (false, true) => "git 输出不完整".to_string(),
        // `assemble_files` only increments the omission counter under one of
        // the causes above; keep a truthful fallback instead of an empty reason.
        (false, false) => "超出上限".to_string(),
    }
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

/// The path a raw patch section is about, from its headers only (hunk content
/// is never consulted): `rename to` first, then `+++ b/<path>` (or the old side
/// of a deletion), and finally a `diff --git` header when both sides spell the
/// same path. A C-quoted field is unquoted; `core.quotepath=false` still quotes
/// control characters.
fn section_path(section: &str) -> Option<String> {
    let mut rename_to = None;
    let mut plus = None;
    let mut minus = None;
    let mut diff_git = None;
    for line in section.lines() {
        // File headers precede the first hunk. A removed line whose text begins
        // `-- ` renders as `--- …` inside a hunk and must not be read as one.
        if line.starts_with("@@") {
            break;
        }
        if let Some(rest) = line.strip_prefix("rename to ") {
            rename_to = Some(unquote_path(rest));
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            plus = Some(unquote_path(rest));
        } else if let Some(rest) = line.strip_prefix("--- ") {
            minus = Some(unquote_path(rest));
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            diff_git = Some(rest.to_string());
        }
    }

    if let Some(path) = rename_to {
        return Some(path);
    }
    if let Some(path) = plus
        && path != "/dev/null"
    {
        return Some(strip_prefix_component(&path));
    }
    if let Some(path) = minus
        && path != "/dev/null"
    {
        // A deletion (`+++ /dev/null`) is keyed by its old path.
        return Some(strip_prefix_component(&path));
    }
    diff_git_path(&diff_git?)
}

/// Strip git's `a/` or `b/` side prefix. `diff.noprefix` leaves the path as-is.
fn strip_prefix_component(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

/// Undo git's C quoting for a path field. With `core.quotepath=false` only
/// control characters, quotes and backslashes are quoted, but octal escapes
/// still encode arbitrary bytes.
fn unquote_path(raw: &str) -> String {
    let Some(inner) = raw.strip_prefix('"').and_then(|raw| raw.strip_suffix('"')) else {
        return raw.to_string();
    };
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'\\' || index + 1 >= bytes.len() {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        let escaped = bytes[index + 1];
        let mut step = 2usize;
        match escaped {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'v' => out.push(0x0b),
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            b'0'..=b'7' => {
                let mut value = u16::from(escaped - b'0');
                let mut consumed = 1usize;
                while consumed < 3
                    && index + 1 + consumed < bytes.len()
                    && let digit @ b'0'..=b'7' = bytes[index + 1 + consumed]
                {
                    value = value * 8 + u16::from(digit - b'0');
                    consumed += 1;
                }
                out.push(value as u8);
                step = 1 + consumed;
            }
            other => out.push(other),
        }
        index += step;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The path from a `diff --git a/<p> b/<p>` header, accepted only when both
/// sides spell the same path. A rename spells two different paths and carries
/// a `rename to` header instead, so it never reaches this fallback.
fn diff_git_path(rest: &str) -> Option<String> {
    if rest.starts_with('"') {
        let (first, remainder) = take_quoted(rest)?;
        let (second, _) = take_quoted(remainder.trim_start())?;
        let first = strip_prefix_component(&unquote_path(first));
        let second = strip_prefix_component(&unquote_path(second));
        return (first == second).then_some(first);
    }
    // Unquoted paths may contain spaces, so try every split and accept only one
    // where both sides name the same path.
    for (index, _) in rest.match_indices(' ') {
        let left = strip_prefix_component(&rest[..index]);
        let right = strip_prefix_component(rest[index + 1..].trim_start());
        if !left.is_empty() && left == right {
            return Some(left);
        }
    }
    None
}

/// Take one C-quoted field (quotes included) and the remainder after it.
fn take_quoted(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut index = 1usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some((&text[..=index], &text[index + 1..])),
            _ => index += 1,
        }
    }
    None
}

/// Build one tracked file's [`DiffFile`] from its numstat entry and raw patch
/// section. Submodules and binaries collapse to one line; everything else keeps
/// the section verbatim (renames therefore keep their `rename from`/`rename to`
/// headers). A tracked patch is never replaced by a note: it is admitted whole
/// while it fits the total raw cap, and the total cap's omission is what
/// `truncated` reports.
fn tracked_file(entry: &NumStatEntry, section: &str) -> DiffFile {
    let mut status = section_status(section);
    if entry.renamed {
        status = "renamed";
    }
    let patch = if let Some((old, new)) = submodule_pair(section) {
        format!("子模块 {} {old} → {new}", entry.path)
    } else if entry.binary {
        binary_note(section, &entry.path)
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

/// A submodule's old and new gitlink commits, or None. A gitlink **mode** is
/// the only proof: `new file mode 160000`, `deleted file mode 160000` or an
/// `index <old>..<new> 160000` suffix. The `Subproject commit` lines only
/// supply the commits — a tracked text file may legitimately have those as its
/// content, and treating them as proof would discard its real hunk.
fn submodule_pair(section: &str) -> Option<(String, String)> {
    let mut old_commit = None;
    let mut new_commit = None;
    let mut gitlink = false;
    let mut index = None;
    for line in section.lines() {
        if line.starts_with("new file mode 160000") || line.starts_with("deleted file mode 160000") {
            gitlink = true;
        } else if let Some(rest) = line.strip_prefix("-Subproject commit ") {
            old_commit = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("+Subproject commit ") {
            new_commit = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("index ") {
            let mut parts = rest.split_whitespace();
            let range = parts.next();
            if parts.next() == Some("160000") {
                gitlink = true;
            }
            // An added/deleted gitlink's `index` line carries no mode suffix
            // (`index 0000000..abc`), so the mode header above is what proves
            // this is a submodule; keep the range either way.
            if let Some((old, new)) = range.and_then(|range| range.split_once("..")) {
                index = Some((old.to_string(), new.to_string()));
            }
        }
    }
    if !gitlink {
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

/// The untracked phase's anchor, captured once before the listing. The anchor
/// is a HANDLE opened first, never a path re-resolved later: on Unix the held
/// directory handle pins the root directory itself, so renaming the root
/// pathname (or replacing it with a symlink or another ordinary directory)
/// cannot redirect a walk. Windows stores the captured directory identity and
/// re-checks it around every open. Other targets keep the canonical path only.
struct UntrackedScope {
    /// The repository root as it was when the scope was captured. Used only
    /// for the metadata pre-check; content walks use the anchored handle.
    canonical_root: PathBuf,
    /// The open directory handle every Unix walk starts from.
    #[cfg(unix)]
    dir: std::fs::File,
    /// The root pathname as git reported it; the Windows identity re-check
    /// opens it to prove it still names the captured directory.
    #[cfg(windows)]
    root: PathBuf,
    /// The directory identity captured before canonicalization, so a
    /// replacement at the same pathname is never adopted.
    #[cfg(windows)]
    root_handle: same_file::Handle,
}

impl UntrackedScope {
    /// Capture the scope. Runs on the blocking pool (it can stall on a network
    /// filesystem). The handle is opened FIRST, from the path git gave us, with
    /// no canonicalize→open gap: a directory swapped in at the same pathname
    /// after capture cannot be adopted as the anchor. The canonical path is
    /// derived afterwards only for the metadata pre-check and the Windows
    /// prefix check.
    fn capture(root: &str) -> Result<Self, ReviewError> {
        #[cfg(unix)]
        let dir = {
            use rustix::fs::{CWD, Mode, OFlags};
            let fd = rustix::fs::openat(
                CWD,
                root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| map_io_error(error.into()))?;
            std::fs::File::from(fd)
        };
        #[cfg(windows)]
        let root_handle = same_file::Handle::from_path(root).map_err(map_io_error)?;

        let canonical_root = std::fs::canonicalize(root).map_err(map_io_error)?;
        Ok(Self {
            canonical_root,
            #[cfg(unix)]
            dir,
            #[cfg(windows)]
            root: PathBuf::from(root),
            #[cfg(windows)]
            root_handle,
        })
    }
}

/// Capture [`UntrackedScope`] under the per-entry bound. A failure or timeout
/// is a `ReviewError`, never an unanchored read.
async fn capture_untracked_scope(root: &str) -> Result<Arc<UntrackedScope>, ReviewError> {
    let root = root.to_string();
    let task = tokio::task::spawn_blocking(move || UntrackedScope::capture(&root));
    match tokio::time::timeout(UNTRACKED_READ_TIMEOUT, task).await {
        Ok(Ok(Ok(scope))) => Ok(Arc::new(scope)),
        Ok(Ok(Err(error))) => Err(error),
        // The blocking task is detached; without an anchor the phase must not
        // proceed.
        Ok(Err(_)) | Err(_) => Err(ReviewError::Io("无法在有界时间内确定仓库根目录".to_string())),
    }
}

/// What the blocking untracked inspection found: a symlink's target (which
/// becomes a new-file patch), an opened regular file to read, or a one-line
/// note for an entry that cannot be content.
enum Inspected {
    Symlink(String),
    File(std::fs::File),
    Note(&'static str),
}

/// Inspect one untracked entry's filesystem state. Everything that can stall on
/// a network filesystem (metadata, a symlink read, the open and its handle
/// checks) lives here, so the caller runs it on the blocking pool under one
/// deadline. A symlink is read with `read_link` and never followed; a regular
/// file is opened with [`open_beneath`].
fn inspect_untracked(scope: &UntrackedScope, path: &str) -> Result<Inspected, ReviewError> {
    let full = scope.canonical_root.join(path);
    let meta = std::fs::symlink_metadata(&full).map_err(map_io_error)?;
    if meta.file_type().is_symlink() {
        // Read the link through the same component walk as a file open: a
        // parent swapped for a symlink must not redirect the read to an
        // outside target.
        let Some(target) = read_link_beneath(scope, path)? else {
            return Ok(Inspected::Note(IRREGULAR_NOTE));
        };
        let target = target.to_string_lossy();
        if target.len() > MAX_FILE_PATCH_BYTES {
            return Ok(Inspected::Note(OVERSIZE_NOTE));
        }
        return Ok(Inspected::Symlink(target.into_owned()));
    }
    if !meta.file_type().is_file() {
        return Ok(Inspected::Note(IRREGULAR_NOTE));
    }
    if meta.len() > MAX_FILE_PATCH_BYTES as u64 {
        return Ok(Inspected::Note(OVERSIZE_NOTE));
    }

    // Every component is re-opened beneath the held root handle: a parent
    // directory swapped for a symlink (or a final symlink/FIFO) is refused,
    // not followed.
    match open_beneath(scope, path)? {
        Some(file) => Ok(Inspected::File(file)),
        None => Ok(Inspected::Note(IRREGULAR_NOTE)),
    }
}

/// Build an untracked file's new-file patch. `budget` covers the whole entry:
/// the inspection (metadata, symlink target, open and handle checks) runs in
/// one blocking task under it, and the returned regular file is read under
/// whatever remains. The caller passes `min(remaining total, per-entry cap)`.
async fn untracked_file(
    scope: &Arc<UntrackedScope>,
    path: &str,
    budget: Duration,
) -> Result<DiffFile, ReviewError> {
    let entry_deadline = tokio::time::Instant::now() + budget;
    match inspect_within(scope, path, budget).await? {
        Inspected::Symlink(target) => {
            // The link's content is its target path; the target has no
            // trailing newline, which is why the synthesized line is fed one.
            let (patch, additions) = new_file_patch(path, &format!("{target}\n"));
            Ok(added_file(path.to_string(), patch, additions))
        }
        Inspected::File(file) => {
            let remaining = entry_deadline.saturating_duration_since(tokio::time::Instant::now());
            let bytes = match read_opened_within(file, remaining).await {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return Ok(note_file(path, UNREADABLE_NOTE)),
                Err(error) => return Err(error),
            };
            if bytes.len() > MAX_FILE_PATCH_BYTES {
                return Ok(note_file(path, OVERSIZE_NOTE));
            }
            // Git's binary test over the whole (bounded) read window: a NUL
            // anywhere means binary. Scanning only the first 8 KB would let
            // later NUL-containing content reach the patch as text.
            if bytes.contains(&0) {
                return Ok(binary_added(path));
            }
            let Ok(text) = String::from_utf8(bytes) else {
                return Ok(binary_added(path));
            };
            let (patch, additions) = new_file_patch(path, &text);
            Ok(added_file(path.to_string(), patch, additions))
        }
        Inspected::Note(note) => Ok(note_file(path, note)),
    }
}

/// Split an untracked path into components that are safe to open beneath the
/// root: no absolute path and no empty, `.` or `..` component.
fn beneath_components(relative: &str) -> Option<Vec<&str>> {
    if Path::new(relative).is_absolute() {
        return None;
    }
    let components: Vec<&str> = relative.split('/').collect();
    (!components.is_empty()
        && !components
            .iter()
            .any(|component| component.is_empty() || *component == "." || *component == ".."))
    .then_some(components)
}

/// Read a symlink's target without following any path component: the parent
/// directories are walked with `openat(O_NOFOLLOW)` and the final component is
/// resolved with `readlinkat`. A parent swapped for a symlink, a missing
/// component, or a final entry that is no longer a symlink is `None` (the
/// caller emits the one-line note).
#[cfg(unix)]
fn read_link_beneath(scope: &UntrackedScope, relative: &str) -> Result<Option<PathBuf>, ReviewError> {
    use rustix::fs::{Mode, OFlags};
    use std::os::fd::AsFd;
    use std::os::unix::ffi::OsStringExt;

    let Some(components) = beneath_components(relative) else {
        return Ok(None);
    };
    let Some((name, parents)) = components.split_last() else {
        return Ok(None);
    };

    let mut dir: Option<rustix::fd::OwnedFd> = None;
    for component in parents {
        let parent = match &dir {
            Some(dir) => dir.as_fd(),
            None => scope.dir.as_fd(),
        };
        let opened = match rustix::fs::openat(
            parent,
            *component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(opened) => opened,
            Err(error) => {
                return match error {
                    rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR | rustix::io::Errno::NOENT => {
                        Ok(None)
                    }
                    _ => Err(map_io_error(error.into())),
                };
            }
        };
        dir = Some(opened);
    }

    let parent = match &dir {
        Some(dir) => dir.as_fd(),
        None => scope.dir.as_fd(),
    };
    match rustix::fs::readlinkat(parent, *name, Vec::new()) {
        Ok(target) => Ok(Some(PathBuf::from(std::ffi::OsString::from_vec(
            target.into_bytes(),
        )))),
        Err(error) => match error {
            rustix::io::Errno::LOOP
            | rustix::io::Errno::NOTDIR
            | rustix::io::Errno::NOENT
            | rustix::io::Errno::INVAL => Ok(None),
            _ => Err(map_io_error(error.into())),
        },
    }
}

/// A component-verified link read needs `readlinkat`; without it, an untracked
/// symlink becomes the one-line note rather than a parent-following read.
#[cfg(not(unix))]
fn read_link_beneath(_scope: &UntrackedScope, _relative: &str) -> Result<Option<PathBuf>, ReviewError> {
    Ok(None)
}

/// Open an untracked file for reading without leaving the repository root.
/// On Unix every component is opened relative to the previous directory with
/// `O_NOFOLLOW`, so neither a parent swapped for a symlink nor a final
/// symlink/FIFO can redirect (or block) the read. The opened handle's metadata
/// is re-checked for regular-file type and the 2 MB cap. `None` means "not a
/// regular, bounded file": the caller emits the one-line note instead.
#[cfg(unix)]
fn open_beneath(scope: &UntrackedScope, relative: &str) -> Result<Option<std::fs::File>, ReviewError> {
    use rustix::fs::{Mode, OFlags};
    use std::os::fd::AsFd;

    let Some(components) = beneath_components(relative) else {
        return Ok(None);
    };

    let mut dir: Option<rustix::fd::OwnedFd> = None;
    let last = components.len() - 1;
    for (index, component) in components.iter().enumerate() {
        let flags = if index == last {
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        };
        let parent = match &dir {
            Some(dir) => dir.as_fd(),
            None => scope.dir.as_fd(),
        };
        let opened = match rustix::fs::openat(parent, *component, flags, Mode::empty()) {
            Ok(opened) => opened,
            Err(error) => {
                return match error {
                    rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR | rustix::io::Errno::NOENT => {
                        Ok(None)
                    }
                    _ => Err(map_io_error(error.into())),
                };
            }
        };
        if index == last {
            let file = std::fs::File::from(opened);
            let meta = file.metadata().map_err(map_io_error)?;
            if !meta.is_file() || meta.len() > MAX_FILE_PATCH_BYTES as u64 {
                return Ok(None);
            }
            return Ok(Some(file));
        }
        dir = Some(opened);
    }
    Ok(None)
}

/// Windows has no `openat`, so the opened handle is instead proved to be the
/// canonical repository file: the resolved path must stay under the canonical
/// root, and the handle's volume/file identity must match the resolved path's.
/// That closes both the parent-directory swap (after `canonicalize`) and the
/// final-file swap (after `open`).
#[cfg(windows)]
fn open_beneath(scope: &UntrackedScope, relative: &str) -> Result<Option<std::fs::File>, ReviewError> {
    if beneath_components(relative).is_none() {
        return Ok(None);
    }
    // 1. The root pathname must still name the directory captured with the
    //    scope. An ordinary-directory or symlink replacement fails here.
    if !root_is_captured(scope)? {
        return Ok(None);
    }
    // The resolved path must stay under the CAPTURED canonical root; a root
    // replaced after capture resolves outside.
    let full = scope.canonical_root.join(relative);
    let file = std::fs::File::open(&full).map_err(map_io_error)?;
    if !opened_stays_beneath(&scope.canonical_root, &full, &file).map_err(map_io_error)? {
        return Ok(None);
    }
    // 2. Re-check the root after the open: a replacement between the first
    //    check and the open would otherwise be adopted as the anchor.
    if !root_is_captured(scope)? {
        return Ok(None);
    }
    let meta = file.metadata().map_err(map_io_error)?;
    if !meta.is_file() || meta.len() > MAX_FILE_PATCH_BYTES as u64 {
        return Ok(None);
    }
    Ok(Some(file))
}

/// Whether the root pathname still names the directory whose identity was
/// captured with the scope. Opened before and after the file open, so neither
/// an ordinary-directory nor a symlink replacement can be adopted.
#[cfg(windows)]
fn root_is_captured(scope: &UntrackedScope) -> Result<bool, ReviewError> {
    let current = same_file::Handle::from_path(&scope.root).map_err(map_io_error)?;
    Ok(current == scope.root_handle)
}

/// Whether `file` is the file `full` resolves to, and that resolution is under
/// `canonical_root`. `same-file` compares the handle's volume serial and file
/// index (the `MetadataExt` accessors for those are still unstable); a handle
/// that resolves outside the root, or names a different file than the opened
/// handle, is refused.
#[cfg(windows)]
fn opened_stays_beneath(canonical_root: &Path, full: &Path, file: &std::fs::File) -> std::io::Result<bool> {
    let resolved = std::fs::canonicalize(full)?;
    if !resolved.starts_with(canonical_root) {
        return Ok(false);
    }
    let opened = same_file::Handle::from_file(file.try_clone()?)?;
    let named = same_file::Handle::from_path(&resolved)?;
    Ok(opened == named)
}

/// No verified beneath-root open exists on this target (no `openat` and no
/// file identity): the entry becomes the one-line note rather than risking a
/// read outside the repository.
#[cfg(all(not(unix), not(windows)))]
fn open_beneath(_scope: &UntrackedScope, relative: &str) -> Result<Option<std::fs::File>, ReviewError> {
    let _ = beneath_components(relative);
    Ok(None)
}

/// Run the whole synchronous untracked inspection on the blocking pool under
/// one deadline. A timed-out or panicked blocking task is DETACHED, not
/// awaited: the review must not wait on a stalled filesystem, and the entry
/// becomes the unreadable note.
async fn inspect_within(
    scope: &Arc<UntrackedScope>,
    path: &str,
    timeout: Duration,
) -> Result<Inspected, ReviewError> {
    let scope = Arc::clone(scope);
    let path = path.to_string();
    deadline(timeout, move || inspect_untracked(&scope, &path)).await
}

async fn deadline<F>(timeout: Duration, inspect: F) -> Result<Inspected, ReviewError>
where
    F: FnOnce() -> Result<Inspected, ReviewError> + Send + 'static,
{
    // A zero budget can never complete an inspection; answering without
    // spawning keeps that result deterministic.
    if timeout.is_zero() {
        return Ok(Inspected::Note(UNREADABLE_NOTE));
    }
    let task = tokio::task::spawn_blocking(inspect);
    match tokio::time::timeout(timeout, task).await {
        Ok(Ok(Ok(inspected))) => Ok(inspected),
        Ok(Ok(Err(error))) => Err(error),
        // A JoinError (the inspector panicked) or an expired deadline: one bad
        // entry is a note, never a failed review.
        Ok(Err(_)) | Err(_) => Ok(Inspected::Note(UNREADABLE_NOTE)),
    }
}

/// Read one opened untracked file, bounded in bytes and in time. `None` is the
/// timeout: the caller emits the unreadable note instead of keeping the review
/// pending on a stalled filesystem.
async fn read_opened_within(file: std::fs::File, timeout: Duration) -> Result<Option<Vec<u8>>, ReviewError> {
    within(timeout, read_opened(file)).await
}

/// Apply the read budget to any byte-read future. Kept generic so the timeout
/// seam is testable without a stalled filesystem.
async fn within<F>(timeout: Duration, future: F) -> Result<Option<Vec<u8>>, ReviewError>
where
    F: std::future::Future<Output = Result<Vec<u8>, ReviewError>>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(Ok(bytes)) => Ok(Some(bytes)),
        Ok(Err(error)) => Err(error),
        Err(_) => Ok(None),
    }
}

async fn read_opened(file: std::fs::File) -> Result<Vec<u8>, ReviewError> {
    let mut bytes = Vec::new();
    tokio::fs::File::from_std(file)
        .take(MAX_FILE_PATCH_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(map_io_error)?;
    Ok(bytes)
}

/// One untracked entry's `added` file. Every synthesized entry has this shape:
/// untracked synthesis only ever adds and never deletes.
fn added_file(path: String, patch: String, additions: usize) -> DiffFile {
    DiffFile {
        path,
        patch,
        additions,
        deletions: 0,
        status: "added".to_string(),
    }
}

fn note_file(path: &str, note: &str) -> DiffFile {
    added_file(path.to_string(), note.to_string(), 0)
}

fn binary_added(path: &str) -> DiffFile {
    note_file(
        path,
        &format!(
            "Binary files /dev/null and {} differ",
            quote_path(&format!("b/{path}"))
        ),
    )
}

/// Quote a synthesized-path field the way `git diff` does when it contains a
/// control character, `"` or `\`; an ordinary path is returned unchanged. The
/// document's [`DiffFile::path`] stays raw — only the patch headers are quoted,
/// so a filename can never fabricate a hunk line.
fn quote_path(path: &str) -> Cow<'_, str> {
    let needs_quoting = path.chars().any(quoted_char);
    if !needs_quoting {
        return Cow::Borrowed(path);
    }
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for ch in path.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\x07' => quoted.push_str("\\a"),
            '\x08' => quoted.push_str("\\b"),
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
            '\x0b' => quoted.push_str("\\v"),
            '\x0c' => quoted.push_str("\\f"),
            '\r' => quoted.push_str("\\r"),
            ch if quoted_char(ch) => quoted.push_str(&format!("\\{:03o}", ch as u32)),
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    Cow::Owned(quoted)
}

/// Whether git's C-quoting escapes this character: ASCII controls (including
/// DEL), `"` and `\` (with `core.quotepath=false`, non-ASCII stays raw).
fn quoted_char(ch: char) -> bool {
    ch == '"' || ch == '\\' || (ch as u32) < 0x20 || ch == '\x7f'
}

/// A new file's unified patch: the headers the renderer drops, one hunk and one
/// `+` line per content line. Returns the patch and its added-line count.
fn new_file_patch(path: &str, text: &str) -> (String, usize) {
    let index = quote_path(path);
    let prefixed = format!("b/{path}");
    let destination = quote_path(&prefixed);
    let mut patch = format!("Index: {index}\n--- /dev/null\n+++ {destination}\n");
    if text.is_empty() {
        return (patch, 0);
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    let ends_with_newline = text.ends_with('\n');
    if ends_with_newline {
        lines.pop();
    }
    let count = lines.len();
    // Unified diff omits a count of exactly 1, as `git diff` does.
    if count == 1 {
        patch.push_str("@@ -0,0 +1 @@\n");
    } else {
        patch.push_str(&format!("@@ -0,0 +1,{count} @@\n"));
    }
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
    let head = run_git(dir, &["rev-parse", "--verify", "HEAD"], SMALL_STDOUT_BYTES).await?;
    if !head.success {
        // Only the documented unknown-revision result is an unborn HEAD. A
        // permission or repository failure must stay an error instead of
        // turning into a misleading "all files added" diff.
        if is_unknown_revision(&head) {
            return Ok(HeadState {
                branch: "unborn".to_string(),
                short_sha: "unborn".to_string(),
                sha: None,
            });
        }
        return Err(status_error(&head, "rev-parse HEAD"));
    }
    let sha = first_line(&head.stdout);
    if sha.is_empty() {
        return Err(ReviewError::GitFailed("git rev-parse HEAD 没有输出".to_string()));
    }
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
    let out = run_git(dir, &["rev-parse", "--verify", &spec], SMALL_STDOUT_BYTES).await?;
    if out.stdout_truncated {
        return Err(ReviewError::GitFailed("git rev-parse 输出超出上限".to_string()));
    }
    if !out.success {
        // Only a genuinely unknown ref is "not found". A permission, I/O or
        // object failure must not silently fall through to another base.
        if is_unknown_revision(&out) {
            return Ok(None);
        }
        return Err(status_error(&out, "rev-parse"));
    }
    let sha = first_line(&out.stdout);
    Ok((!sha.is_empty()).then_some(sha))
}

/// Whether `git rev-parse --verify` failed because the revision does not exist
/// — the only failure that means "unknown ref" or "unborn HEAD" — rather than
/// because git could not read the repository or a ref. `--verify` spells an
/// unknown revision as `fatal: Needed a single revision`, and a plain unknown
/// name as an unknown revision.
fn is_unknown_revision(out: &GitOutput) -> bool {
    if out.success || !out.stdout.is_empty() {
        return false;
    }
    let lower = out.stderr.to_ascii_lowercase();
    lower.contains("needed a single revision") || lower.contains("unknown revision")
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
    Err(status_error(out, what))
}

/// Map a failed git command to its distinct readable error. A refusal the OS
/// made keeps [`ReviewError::PermissionDenied`]; everything else is a git
/// failure carrying git's own diagnostic.
fn status_error(out: &GitOutput, what: &str) -> ReviewError {
    if out.stderr.to_ascii_lowercase().contains("permission denied") {
        return ReviewError::PermissionDenied(out.stderr.clone());
    }
    let detail = if out.stderr.is_empty() {
        format!("`git {what}` 退出码非零")
    } else {
        out.stderr.clone()
    };
    ReviewError::GitFailed(detail)
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
            // The stream is already over budget: stop the writer and stop
            // reading. Waiting for stderr EOF here can hang on Windows when a
            // grandchild inherited the write end after the direct child was
            // killed; the bytes collected so far are enough for the omission
            // note, and normal exits still drain stderr above.
            let _ = child.kill().await;
            break;
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

    async fn scope_for(dir: &Path) -> Arc<UntrackedScope> {
        capture_untracked_scope(dir.to_str().unwrap())
            .await
            .expect("capture the untracked scope")
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
    /// untracked files (they are separate additions). No staged entry changes.
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
            "review_diff must never change the staged entries"
        );

        assert_eq!(review.base, "main");
        assert_eq!(review.branch, "feature");
        assert_eq!(review.document.files.len(), 4);
        assert!(
            review.document.subtitle.ends_with("· 含未跟踪文件"),
            "untracked files need the marker: {}",
            review.document.subtitle
        );

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

    /// The document's header sums every file, including untracked additions,
    /// while `git diff --stat` counts only tracked files — so the totals are
    /// pinned here and the subtitle carries the marker that explains them.
    #[tokio::test]
    async fn totals_are_tracked_numstat_plus_untracked_additions() {
        let repo = Repo::init();
        repo.write("edited.txt", "a\nb\nc\n");
        repo.write("deleted.txt", "gone\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("edited.txt", "a\nB\nc\nd\n");
        repo.git(&["rm", "-q", "deleted.txt"]);
        repo.write("fresh.txt", "one\ntwo\nthree\n");

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();

        // `git diff --numstat` is the authority for tracked files.
        let numstat = repo.git(&["diff", "--numstat", "main"]);
        let mut tracked_additions = 0usize;
        let mut tracked_deletions = 0usize;
        for line in numstat.lines() {
            let mut fields = line.split('\t');
            let additions: usize = fields.next().unwrap().parse().unwrap();
            let deletions: usize = fields.next().unwrap().parse().unwrap();
            let path = fields.next().unwrap();
            let file = find(&review, path);
            assert_eq!(
                (file.additions, file.deletions),
                (additions, deletions),
                "{path} must match git diff --numstat"
            );
            tracked_additions += additions;
            tracked_deletions += deletions;
        }

        // The untracked file is a separate addition: git's `--stat` does not
        // count it, the document does.
        let untracked = find(&review, "fresh.txt");
        assert_eq!((untracked.additions, untracked.deletions), (3, 0));
        assert!(
            !repo.git(&["diff", "--stat", "main"]).contains("fresh.txt"),
            "git diff --stat never counts untracked files"
        );

        let document_additions: usize = review.document.files.iter().map(|file| file.additions).sum();
        let document_deletions: usize = review.document.files.iter().map(|file| file.deletions).sum();
        assert_eq!(document_additions, tracked_additions + 3);
        assert_eq!(document_deletions, tracked_deletions);
        assert!(
            review.document.subtitle.contains("· 含未跟踪文件"),
            "the marker explains the --stat difference: {}",
            review.document.subtitle
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
    /// delete + add. Path-keyed pairing keeps the new path's counts with the
    /// new path's patch, and the patch keeps the rename headers.
    #[tokio::test]
    async fn rename_is_one_renamed_file() {
        let repo = Repo::init();
        let original: String = (0..10).map(|line| format!("line{line}\n")).collect();
        repo.write("old.txt", &original);
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.git(&["mv", "old.txt", "new.txt"]);
        repo.write("new.txt", original.replace("line5\n", "CHANGED\n"));
        repo.git(&["add", "new.txt"]);

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(review.document.files.len(), 1);
        let file = &review.document.files[0];
        assert_eq!(file.path, "new.txt");
        assert_eq!(file.status, "renamed");
        assert_eq!((file.additions, file.deletions), (1, 1));
        assert!(file.patch.contains("rename from old.txt"));
        assert!(file.patch.contains("rename to new.txt"));
        assert!(file.patch.contains("+CHANGED"));
    }

    /// User prefix config must not break path pairing: the command line pins
    /// `a/`/`b/`, overriding both `diff.mnemonicPrefix` and `diff.noprefix`.
    #[tokio::test]
    async fn explicit_prefixes_ignore_mnemonic_and_noprefix_config() {
        let repo = Repo::init();
        repo.git(&["config", "diff.mnemonicPrefix", "true"]);
        repo.git(&["config", "diff.noprefix", "true"]);

        repo.write("tracked.txt", "one\n");
        let original: String = (0..10).map(|line| format!("line{line}\n")).collect();
        repo.write("old.txt", &original);
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("tracked.txt", "one\ntwo\n");
        repo.git(&["add", "tracked.txt"]);
        repo.git(&["mv", "old.txt", "new.txt"]);
        repo.write("new.txt", original.replace("line5\n", "CHANGED\n"));
        repo.git(&["add", "new.txt"]);

        let assert_paired = |review: &ReviewDiff| {
            assert!(
                review.document.truncated.is_none(),
                "sections must pair with their numstat rows: {:?}",
                review.document.truncated
            );
            assert_eq!(review.document.files.len(), 2);

            let tracked = find(review, "tracked.txt");
            assert_eq!(tracked.status, "modified");
            assert_eq!((tracked.additions, tracked.deletions), (1, 0));
            assert!(tracked.patch.contains("--- a/tracked.txt"));
            assert!(tracked.patch.contains("+++ b/tracked.txt"));

            let renamed = find(review, "new.txt");
            assert_eq!(renamed.status, "renamed");
            assert_eq!((renamed.additions, renamed.deletions), (1, 1));
            assert!(renamed.patch.contains("rename from old.txt"));
            assert!(renamed.patch.contains("rename to new.txt"));
        };

        // Both keys set (`noprefix` normally wins), and then mnemonic prefixes
        // alone, whose `w/` spelling would otherwise never pair.
        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_paired(&review);

        repo.git(&["config", "diff.noprefix", "false"]);
        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_paired(&review);
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

    /// A repo whose `main` commit has a gitlink `sub` pointing at `first`, with
    /// `feature` checked out and its gitlink moved to `main_sub`.
    fn repo_with_changed_submodule() -> (Repo, String, String) {
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
        (repo, first, main_sub)
    }

    /// Acceptance: a submodule is one `子模块 <path> <old> → <new>` line.
    #[tokio::test]
    async fn submodule_is_one_note_line() {
        let (repo, first, main_sub) = repo_with_changed_submodule();

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(review.document.files.len(), 1);
        let file = &review.document.files[0];
        assert_eq!(file.path, "sub");
        assert_eq!(file.status, "modified");
        assert_eq!(file.patch, format!("子模块 sub {first} → {main_sub}"));
        assert!(!file.patch.contains("Subproject commit"));
        assert!(!file.patch.contains('\0'));
    }

    /// A user's `diff.ignoreSubmodules=all` must not hide a changed submodule:
    /// the deterministic call passes `--ignore-submodules=none`.
    #[tokio::test]
    async fn submodule_is_not_hidden_by_ignore_submodules_config() {
        let (repo, first, main_sub) = repo_with_changed_submodule();
        repo.git(&["config", "diff.ignoreSubmodules", "all"]);

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(review.document.files.len(), 1);
        let file = &review.document.files[0];
        assert_eq!(file.path, "sub");
        assert_eq!(file.status, "modified");
        assert_eq!(file.patch, format!("子模块 sub {first} → {main_sub}"));
    }

    /// A tracked text file whose changed lines happen to read
    /// `Subproject commit …` is an ordinary modification: only a gitlink mode
    /// header makes a section a submodule, so the real hunk is kept and no
    /// 子模块 note replaces it.
    #[tokio::test]
    async fn a_text_file_with_subproject_lines_is_not_a_submodule() {
        let repo = Repo::init();
        repo.write("log.txt", "Subproject commit aaa\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("log.txt", "Subproject commit bbb\n");

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        let file = find(&review, "log.txt");
        assert_eq!(file.status, "modified");
        assert_eq!((file.additions, file.deletions), (1, 1));
        assert!(
            !file.patch.contains("子模块"),
            "a text lookalike is not a gitlink: {}",
            file.patch
        );
        assert!(file.patch.contains("@@ -1 +1 @@"), "{}", file.patch);
        assert!(file.patch.contains("-Subproject commit aaa\n"), "{}", file.patch);
        assert!(file.patch.contains("+Subproject commit bbb\n"), "{}", file.patch);
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
            !truncated.contains("git 输出不完整"),
            "the stream was read whole: {truncated}"
        );
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

    /// The 2 MB per-file cap is for untracked synthesis only: a larger TRACKED
    /// patch is admitted whole while it fits the total raw cap (the ticket
    /// bounds tracked patches by the 8 MB total, not a per-file substitution).
    #[tokio::test]
    async fn a_tracked_patch_over_two_mb_is_included_whole() {
        let repo = Repo::init();
        repo.write("big.txt", format!("{}\n", "x".repeat(2_500_000)));
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("big.txt", "small\n");

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        let file = find(&review, "big.txt");
        assert!(review.document.truncated.is_none());
        assert!(
            file.patch.len() > MAX_FILE_PATCH_BYTES,
            "tracked patches are not per-file capped"
        );
        assert!(file.patch.starts_with("diff --git "));
        assert!(file.patch.contains(&"x".repeat(1024)));
        assert_eq!((file.additions, file.deletions), (1, 1));
        assert_eq!(file.status, "modified");
    }

    /// When the total raw cap drops a tracked file, the document says so:
    /// `truncated` names the omission and no included patch is a note
    /// substitute. The note talks about the RAW cap, never the HTML cap.
    #[tokio::test]
    async fn total_cap_omitting_a_tracked_file_sets_truncated() {
        let repo = Repo::init();
        for index in 0..3 {
            repo.write(&format!("big{index}.txt"), format!("{}\n", "x".repeat(2_500_000)));
        }
        repo.write("big3.txt", format!("{}\n", "x".repeat(1_500_000)));
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        for index in 0..4 {
            repo.write(&format!("big{index}.txt"), format!("small-{index}\n"));
        }

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        let truncated = review
            .document
            .truncated
            .as_deref()
            .expect("the total cap must set truncated");
        assert!(truncated.contains("8 MB"));
        assert!(
            !truncated.contains("git 输出不完整"),
            "the stream was read whole: {truncated}"
        );
        assert!(truncated.contains("原始 patch"), "the note names the raw cap");
        assert_eq!(review.document.files.len(), 3);
        assert!(
            review.document.files.iter().all(|file| file.path != "big3.txt"),
            "the file past the total cap is omitted"
        );
        for file in &review.document.files {
            assert!(file.patch.starts_with("diff --git "));
            assert!(file.patch.len() > MAX_FILE_PATCH_BYTES);
        }
    }

    /// A NUL anywhere in the (bounded) read window marks an untracked file
    /// binary — including one after the first 8 KB, which the old check missed.
    #[tokio::test]
    async fn untracked_late_nul_is_binary_and_has_no_payload() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("base");
        let mut content = vec![b'a'; 10_000];
        content.push(0);
        content.extend_from_slice(b"SECRET-AFTER-THE-NUL");
        repo.write("late-nul.txt", &content);

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        let file = find(&review, "late-nul.txt");
        assert_eq!(file.status, "added");
        assert_eq!(file.patch, "Binary files /dev/null and b/late-nul.txt differ");
        assert!(!file.patch.contains("SECRET-AFTER-THE-NUL"));
        assert!(!file.patch.contains('\0'));
    }

    /// A filename with a newline cannot fabricate a hunk: the synthesized
    /// headers are C-quoted, while the document keeps the raw path.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_untracked_name_cannot_fabricate_a_hunk() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("base");
        let name = "evil\n@@ -0,0 +1,1 @@\n+fake";
        repo.write(name, "real\n");

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        let file = find(&review, name);
        assert_eq!(file.path, name, "the document keeps the raw path");
        assert!(
            file.patch.contains("Index: \"evil\\n@@ -0,0 +1,1 @@\\n+fake\""),
            "{}",
            file.patch
        );
        assert!(
            file.patch.contains("+++ \"b/evil\\n@@ -0,0 +1,1 @@\\n+fake\""),
            "{}",
            file.patch
        );

        let html = crate::backend::render_diff_html(&review.document);
        assert_eq!(
            html.matches("<span class=\"line add\">").count(),
            1,
            "only the real content line is an addition"
        );
        assert!(html.contains("<span class=\"code\">+real"));
        assert!(!html.contains("<span class=\"code\">+fake"));
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

    /// One untracked entry that vanished between `git ls-files` and the read is
    /// a note for that path, never an error for the whole review.
    #[tokio::test]
    async fn a_vanished_untracked_entry_is_a_note_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_for(dir.path()).await;
        let (files, truncated) = assemble_files(&scope, &[], &[], 0, &["vanished.txt".to_string()])
            .await
            .expect("a per-entry read failure must not fail the review");

        assert!(truncated.is_none());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "vanished.txt");
        assert_eq!(files[0].status, "added");
        assert_eq!(files[0].patch, UNREADABLE_NOTE);
    }

    /// A non-UTF-8 path survives `ls-files -z`'s lossy decode but cannot be
    /// reopened by its lossy spelling: the review still finishes with a note
    /// instead of failing the command. APFS refuses such a filename outright
    /// (EILSEQ), so the fixture can only exist on Linux.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_non_utf8_untracked_name_is_a_note_not_an_error() {
        use std::os::unix::ffi::OsStrExt;

        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("base");
        let name = std::ffi::OsStr::from_bytes(b"bad\xffname.txt");
        std::fs::write(repo.path().join(name), "content\n").unwrap();

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        assert_eq!(review.document.files.len(), 1);
        assert_eq!(review.document.files[0].patch, UNREADABLE_NOTE);
    }

    /// An untracked symlink is a new-file patch whose single content line is
    /// the link target (read with `read_link`, never followed) — the outside
    /// file's contents must never reach the patch.
    #[cfg(unix)]
    #[tokio::test]
    async fn untracked_symlink_is_a_new_file_patch_and_never_followed() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("base");
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "OUTSIDE-SECRET\n").unwrap();
        std::os::unix::fs::symlink(&secret, repo.path().join("link.txt")).unwrap();

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        let link = find(&review, "link.txt");
        assert_eq!(link.status, "added");
        assert_eq!((link.additions, link.deletions), (1, 0));
        assert_eq!(
            link.patch,
            format!(
                "Index: link.txt\n--- /dev/null\n+++ b/link.txt\n@@ -0,0 +1 @@\n+{}\n",
                secret.to_string_lossy()
            )
        );
        assert!(!link.patch.contains("OUTSIDE-SECRET"));
    }

    /// A link listed while its parent was real, with the parent then swapped
    /// for a symlink to an outside directory, must not leak the outside
    /// link's target: the component walk refuses the parent.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_behind_a_swapped_parent_is_a_note_not_the_target() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join("dir")).unwrap();
        std::os::unix::fs::symlink("/outside/secret-target", outside.path().join("dir/link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("dir"), root.path().join("dir")).unwrap();

        let scope = scope_for(root.path()).await;
        let (files, truncated) = assemble_files(&scope, &[], &[], 0, &["dir/link".to_string()])
            .await
            .unwrap();
        assert!(truncated.is_none());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].patch, IRREGULAR_NOTE);
        assert!(!files[0].patch.contains("/outside/secret-target"));
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

    /// A HEAD that names a missing object is a git error, never the empty-tree
    /// path (which would report the whole worktree as additions).
    #[tokio::test]
    async fn head_pointing_at_a_missing_object_is_an_error() {
        let repo = Repo::init();
        repo.write("a.txt", "a\n");
        repo.commit("base");
        // A detached HEAD naming an object that does not exist: `rev-parse`
        // prints the raw id, so the merge-base step is where it must fail.
        std::fs::write(
            repo.path().join(".git/HEAD"),
            "0000000000000000000000000000000000000000\n",
        )
        .unwrap();

        let err = review_diff(&repo.path_str(), Some("main")).await.unwrap_err();
        assert!(
            matches!(
                err,
                ReviewError::MergeBaseFailed { .. } | ReviewError::GitFailed(_)
            ),
            "expected a git/merge-base error, got {err:?}"
        );
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
        let truncated = review.document.truncated.as_deref().unwrap();
        assert!(
            truncated.contains("git 输出不完整"),
            "a cut stream names the stream, not only the cap: {truncated}"
        );
    }

    /// The producer's raw cap and the renderer's HTML cap are independent
    /// guarantees. This document trips both: the raw patch drops a tracked file
    /// and the escaped artifact cannot hold the admitted ones, so each note
    /// appears once, names its own cap, and the HTML stays under its bound.
    #[tokio::test]
    async fn both_the_raw_and_html_caps_report_their_own_omission() {
        let repo = Repo::init();
        let old = "x".repeat(1_500_000);
        // `<` escapes to `&lt;`, so the HTML is larger than the raw patch and
        // the renderer's cap (not the producer's) decides what fits.
        let new = "<".repeat(1_500_000);
        for index in 0..3 {
            repo.write(&format!("big{index}.txt"), format!("{old}\n"));
        }
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        for index in 0..3 {
            repo.write(&format!("big{index}.txt"), format!("{new}\n"));
        }

        let review = review_diff(&repo.path_str(), Some("main")).await.unwrap();
        let producer_note = review
            .document
            .truncated
            .as_deref()
            .expect("the raw cap must omit a file")
            .to_string();
        assert!(
            producer_note.contains("原始 patch") && producer_note.contains("8 MB"),
            "the producer note names the raw cap: {producer_note}"
        );

        let html = crate::backend::render_diff_html(&review.document);
        assert!(
            html.contains(&producer_note),
            "the producer note reaches the artifact: {producer_note}"
        );
        assert!(
            html.contains("超出预览上限"),
            "the renderer's own omission note is present"
        );
        assert_eq!(
            html.matches("已省略").count(),
            2,
            "one note per cap, neither contradicting the other"
        );
        assert!(
            html.len() < crate::backend::MAX_DIFF_HTML_BYTES,
            "the artifact stays under the HTML cap: {}",
            html.len()
        );
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

        // The content alone never proves a gitlink: a text file whose changed
        // lines read as submodule commits keeps its ordinary 100644 mode.
        let lookalike = "diff --git a/log.txt b/log.txt\nindex aaa..bbb 100644\n\
                         --- a/log.txt\n+++ b/log.txt\n@@ -1 +1 @@\n\
                         -Subproject commit aaa\n+Subproject commit bbb\n";
        assert_eq!(submodule_pair(lookalike), None);
    }

    /// The omission note names the cause that actually stopped the read: the
    /// byte cap, an incomplete git stream, or both — never the cap alone when
    /// git's output was cut.
    #[test]
    fn omission_reason_names_the_real_cause() {
        assert_eq!(omission_reason(true, false, false), "原始 patch 超过 8 MB 上限");
        assert_eq!(omission_reason(false, true, false), "git 输出不完整");
        let both = omission_reason(true, true, false);
        assert!(both.contains("git 输出不完整"), "{both}");
        assert!(both.contains("8 MB"), "{both}");

        // The time bound wins when it is what stopped the loop, even if the
        // byte cap or the stream also contributed.
        for cap in [false, true] {
            for stream in [false, true] {
                let timed = omission_reason(cap, stream, true);
                assert!(timed.contains("30 秒总时限"), "{timed}");
            }
        }
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

    /// Every patch shape yields the path its numstat row uses: the new path for
    /// a rename, the old path for a deletion, and never hunk content.
    #[test]
    fn section_path_reads_each_shape() {
        let modified =
            "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(section_path(modified).as_deref(), Some("src/a.rs"));

        let added = "diff --git a/new.txt b/new.txt\nnew file mode 100644\n\
                     --- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hi\n";
        assert_eq!(section_path(added).as_deref(), Some("new.txt"));

        let deleted = "diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n\
                       --- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n";
        assert_eq!(section_path(deleted).as_deref(), Some("gone.txt"));

        let renamed = "diff --git a/old.txt b/new.txt\nsimilarity index 100%\n\
                       rename from old.txt\nrename to new.txt\n";
        assert_eq!(section_path(renamed).as_deref(), Some("new.txt"));

        // A removed line whose text begins `-- ` renders as `--- …` INSIDE the
        // hunk; it must never be read as the file header.
        let lookalike = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n\
                         --- old comment\n+++ new comment\n";
        assert_eq!(section_path(lookalike).as_deref(), Some("x"));

        // C-quoted control characters are unquoted before the side prefix goes.
        let quoted = "diff --git \"a/tab\\tname\" \"b/tab\\tname\"\n\
                      --- \"a/tab\\tname\"\n+++ \"b/tab\\tname\"\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(section_path(quoted).as_deref(), Some("tab\tname"));

        // The `diff --git` fallback only accepts a path both sides agree on.
        assert_eq!(
            section_path("diff --git a/same b/same\n").as_deref(),
            Some("same")
        );
        assert_eq!(section_path("diff --git a/old b/new\n"), None);

        assert_eq!(unquote_path("\"a\\101b\""), "aAb");
        assert_eq!(unquote_path("\"a\\\\b\\\"c\""), "a\\b\"c");
    }

    /// Pairing is by path: sections in a different order from the numstat rows
    /// still get their own patch and counts.
    #[tokio::test]
    async fn sections_pair_with_entries_by_path_not_index() {
        let entries = vec![
            NumStatEntry {
                additions: 1,
                deletions: 0,
                path: "a.txt".to_string(),
                renamed: false,
                binary: false,
            },
            NumStatEntry {
                additions: 2,
                deletions: 0,
                path: "b.txt".to_string(),
                renamed: false,
                binary: false,
            },
        ];
        let a = "diff --git a/a.txt b/a.txt\nnew file mode 100644\n\
                 --- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+A\n";
        let b = "diff --git a/b.txt b/b.txt\nnew file mode 100644\n\
                 --- /dev/null\n+++ b/b.txt\n@@ -0,0 +1,2 @@\n+B1\n+B2\n";
        let sections = vec![b, a];

        // A real, existing root: the scope anchors a directory handle. The
        // synthetic sections need no files under it.
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_for(dir.path()).await;
        let (files, truncated) = assemble_files(&scope, &entries, &sections, 2, &[]).await.unwrap();
        assert!(truncated.is_none());
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].additions, 1);
        assert!(files[0].patch.contains("diff --git a/a.txt b/a.txt"));
        assert_eq!(files[1].path, "b.txt");
        assert_eq!(files[1].additions, 2);
        assert!(files[1].patch.contains("diff --git a/b.txt b/b.txt"));
    }

    /// An entry with no section, or a section with no entry, is an incomplete
    /// read: it is omitted with the stream note, never paired with a different
    /// file's patch.
    #[tokio::test]
    async fn a_missing_or_extra_section_is_reported_incomplete() {
        let entry = |path: &str| NumStatEntry {
            additions: 1,
            deletions: 0,
            path: path.to_string(),
            renamed: false,
            binary: false,
        };
        let a = "diff --git a/a.txt b/a.txt\n--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+A\n";
        let b = "diff --git a/b.txt b/b.txt\n--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+B\n";

        // A real, existing root: the scope anchors a directory handle. The
        // synthetic sections need no files under it.
        let dir = tempfile::tempdir().unwrap();
        let scope = scope_for(dir.path()).await;
        let (files, truncated) = assemble_files(&scope, &[entry("a.txt"), entry("b.txt")], &[a], 1, &[])
            .await
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.txt");
        assert!(truncated.unwrap().contains("git 输出不完整"));

        let (files, truncated) = assemble_files(&scope, &[entry("a.txt")], &[a, b], 2, &[])
            .await
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.txt");
        assert!(truncated.unwrap().contains("git 输出不完整"));
    }

    /// The untracked phase has ONE total deadline: with no budget left no
    /// entry is read, and the remaining entries are omitted as a group with a
    /// time note, not one note each.
    #[tokio::test]
    async fn the_untracked_phase_has_one_total_deadline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readable.txt"), "content\n").unwrap();
        let paths: Vec<String> = (0..5)
            .map(|index| format!("entry{index}.txt"))
            .chain(std::iter::once("readable.txt".to_string()))
            .collect();

        let scope = scope_for(dir.path()).await;
        let (files, truncated) =
            assemble_files_with_timeout(&scope, &[], &[], 0, &paths, std::time::Duration::ZERO)
                .await
                .unwrap();

        assert!(
            files.is_empty(),
            "no untracked entry may be read past the total deadline"
        );
        let note = truncated.expect("the total deadline must be reported");
        assert!(note.contains("6 个文件"), "{note}");
        assert!(note.contains("30 秒总时限"), "{note}");
    }

    /// The untracked read is time-bounded: an exhausted budget is the
    /// unreadable note, not a pending review. A slow future against a zero
    /// budget is deterministic, and fails if the timeout wrapper is removed
    /// (the future then completes and returns its bytes).
    #[tokio::test]
    async fn an_untracked_read_timeout_is_an_unreadable_note() {
        let slow = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(Vec::new())
        };
        assert!(
            within(std::time::Duration::ZERO, slow).await.unwrap().is_none(),
            "an exhausted read budget must surface as a timeout"
        );
    }

    /// The inspection (metadata, symlink read, open, handle checks) is bounded
    /// too: an exhausted budget is the unreadable note, not a pending review.
    #[tokio::test]
    async fn an_untracked_inspection_timeout_is_an_unreadable_note() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "content").unwrap();
        let scope = scope_for(dir.path()).await;

        match inspect_within(&scope, "file.txt", std::time::Duration::ZERO)
            .await
            .unwrap()
        {
            Inspected::Note(note) => assert_eq!(note, UNREADABLE_NOTE),
            _ => panic!("a zero inspection budget must be the unreadable note"),
        }
    }

    /// A slow inspection against a short deadline is the unreadable note. This
    /// fails if the timeout wrapper is removed: the closure would then finish
    /// and return its own note.
    #[tokio::test]
    async fn a_slow_untracked_inspection_hits_the_deadline() {
        let inspected = deadline(std::time::Duration::from_millis(1), || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok(Inspected::Note(IRREGULAR_NOTE))
        })
        .await
        .unwrap();

        match inspected {
            Inspected::Note(note) => assert_eq!(note, UNREADABLE_NOTE),
            _ => panic!("a stalled inspection must be the unreadable note"),
        }
    }

    /// Only a genuinely unknown revision takes the not-found/unborn path; a
    /// permission or repository failure keeps its distinct error.
    #[test]
    fn unknown_revisions_are_separated_from_git_failures() {
        let failed = |stderr: &str| GitOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.to_string(),
            stdout_truncated: false,
        };

        assert!(is_unknown_revision(&failed("fatal: Needed a single revision")));
        assert!(is_unknown_revision(&failed(
            "fatal: ambiguous argument 'nope': unknown revision or path not in the working tree."
        )));
        assert!(!is_unknown_revision(&failed(
            "fatal: could not open '.git/refs/heads/main': Permission denied"
        )));
        assert!(!is_unknown_revision(&GitOutput {
            success: true,
            stdout: b"abc".to_vec(),
            stderr: String::new(),
            stdout_truncated: false,
        }));

        assert!(matches!(
            status_error(
                &failed("fatal: could not open '.git/refs/heads/main': Permission denied"),
                "rev-parse"
            ),
            ReviewError::PermissionDenied(_)
        ));
        assert!(matches!(
            status_error(&failed("fatal: Needed a single revision"), "rev-parse"),
            ReviewError::GitFailed(_)
        ));
    }

    /// The untracked open refuses a final symlink (O_NOFOLLOW) and a FIFO
    /// (opened non-blocking, then rejected by the opened handle's type) without
    /// reading either. The race this guards: a regular path replaced after the
    /// `symlink_metadata` check.
    #[cfg(unix)]
    #[test]
    fn open_beneath_rejects_symlinks_and_fifos() {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let scope = UntrackedScope::capture(dir.path().to_str().unwrap()).unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "target").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_beneath(&scope, "link.txt").unwrap().is_none());

        let fifo = dir.path().join("pipe");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a valid NUL-terminated path inside an existing dir.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(
            open_beneath(&scope, "pipe").unwrap().is_none(),
            "a FIFO must be a note, not a blocking read"
        );

        let regular = dir.path().join("regular.txt");
        std::fs::write(&regular, "ok").unwrap();
        assert!(open_beneath(&scope, "regular.txt").unwrap().is_some());
    }

    /// A parent directory swapped for a symlink must not let the read escape:
    /// the entry becomes a note and the outside bytes never appear.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_parent_directory_is_a_note_not_an_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join("dir")).unwrap();
        std::fs::write(outside.path().join("dir/secret.txt"), "OUTSIDE-SECRET\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("dir"), root.path().join("dir")).unwrap();

        let scope = scope_for(root.path()).await;
        assert!(open_beneath(&scope, "dir/secret.txt").unwrap().is_none());
        // And the review's per-entry path turns it into the note.
        let (files, truncated) = assemble_files(&scope, &[], &[], 0, &["dir/secret.txt".to_string()])
            .await
            .unwrap();
        assert!(truncated.is_none());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].patch, IRREGULAR_NOTE);
        assert!(!files[0].patch.contains("OUTSIDE-SECRET"));
    }

    /// The untracked phase is anchored to the root captured before the listing:
    /// renaming the root and replacing its pathname with a symlink to an
    /// outside directory must not expose the outside file.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_untracked_scope_stays_anchored_after_the_root_is_replaced() {
        use std::io::Read;

        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("secret.txt"), "ORIGINAL\n").unwrap();
        let scope = scope_for(&repo).await;

        // Rename the repository, then put a symlink at its old pathname
        // pointing at an outside directory with a same-named secret.
        let moved = parent.path().join("moved");
        std::fs::rename(&repo, &moved).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "OUTSIDE-SECRET\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), &repo).unwrap();

        // The held directory fd still reaches the original content (or the
        // open is refused) — never the outside file.
        if let Some(mut file) = open_beneath(&scope, "secret.txt").unwrap() {
            let mut content = String::new();
            file.read_to_string(&mut content).unwrap();
            assert_eq!(content, "ORIGINAL\n");
        }

        // The whole per-entry path never shows the outside bytes.
        let file = untracked_file(&scope, "secret.txt", UNTRACKED_READ_TIMEOUT)
            .await
            .unwrap();
        assert!(!file.patch.contains("OUTSIDE-SECRET"), "{}", file.patch);
        assert!(
            file.patch.contains("ORIGINAL") || file.patch == IRREGULAR_NOTE || file.patch == UNREADABLE_NOTE,
            "{}",
            file.patch
        );
    }

    /// Replacing the captured root with an ORDINARY directory (not a symlink)
    /// must not adopt it either: the held fd keeps reading the original tree,
    /// or the open is refused — never the replacement's bytes.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_untracked_scope_survives_an_ordinary_root_replacement() {
        use std::io::Read;

        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("secret.txt"), "ORIGINAL\n").unwrap();
        let scope = scope_for(&repo).await;

        // Move the captured directory away, then put a DIFFERENT ordinary
        // directory at the same pathname with a same-named secret.
        let moved = parent.path().join("moved");
        std::fs::rename(&repo, &moved).unwrap();
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("secret.txt"), "REPLACEMENT-SECRET\n").unwrap();

        // The held directory fd still reaches the original content (or the
        // open is refused) — never the replacement file.
        if let Some(mut file) = open_beneath(&scope, "secret.txt").unwrap() {
            let mut content = String::new();
            file.read_to_string(&mut content).unwrap();
            assert_eq!(content, "ORIGINAL\n");
        }

        // The whole per-entry path never shows the replacement bytes.
        let file = untracked_file(&scope, "secret.txt", UNTRACKED_READ_TIMEOUT)
            .await
            .unwrap();
        assert!(!file.patch.contains("REPLACEMENT-SECRET"), "{}", file.patch);
        assert!(
            file.patch.contains("ORIGINAL") || file.patch == IRREGULAR_NOTE || file.patch == UNREADABLE_NOTE,
            "{}",
            file.patch
        );
    }

    /// The Windows identity check accepts the very file the resolved path
    /// names; real handles only, no fabricated metadata.
    #[cfg(windows)]
    #[test]
    fn opened_stays_beneath_accepts_the_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "content").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
        assert!(
            opened_stays_beneath(&canonical_root, &path, &file).unwrap(),
            "the opened handle is the resolved repository file"
        );
    }

    /// The captured canonical root, not a fresh canonicalization, decides
    /// whether the opened path is beneath: a root replaced with a symlink after
    /// capture resolves outside and is refused.
    #[cfg(windows)]
    #[test]
    fn the_windows_scope_refuses_a_replaced_root() {
        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("secret.txt"), "ORIGINAL").unwrap();
        let scope = UntrackedScope::capture(repo.to_str().unwrap()).unwrap();

        let moved = parent.path().join("moved");
        std::fs::rename(&repo, &moved).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "OUTSIDE-SECRET").unwrap();
        std::os::windows::fs::symlink_dir(outside.path(), &repo).unwrap();

        assert!(
            open_beneath(&scope, "secret.txt").unwrap().is_none(),
            "a replaced root must resolve outside the captured canonical root"
        );
    }

    /// An ordinary-directory replacement at the captured pathname is refused
    /// by the stored root identity, not merely by a canonical prefix.
    #[cfg(windows)]
    #[test]
    fn the_windows_scope_refuses_an_ordinary_directory_replacement() {
        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("secret.txt"), "ORIGINAL").unwrap();
        let scope = UntrackedScope::capture(repo.to_str().unwrap()).unwrap();

        let moved = parent.path().join("moved");
        std::fs::rename(&repo, &moved).unwrap();
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("secret.txt"), "OUTSIDE-SECRET").unwrap();

        assert!(
            open_beneath(&scope, "secret.txt").unwrap().is_none(),
            "a replaced ordinary root must fail the captured root identity"
        );
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
